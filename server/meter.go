package main

import (
	"context"
	"encoding/binary"
	"fmt"
	"net/http"
	"time"
)

const (
	freeWordsPerWeek = 2000
	// Free accounts also get an audio ceiling so silence or music (which yields no words)
	// cannot run up the upstream bill. 2 hours is far above what 2,000 words of speech take.
	freeAudioSecondsPerWeek = 2 * 3600
	// Pro is unlimited words with a fair-use ceiling on audio.
	proAudioSecondsPerMonth = 40 * 3600
)

// weekStart is Monday 00:00 UTC of t's ISO week.
func weekStart(t time.Time) time.Time {
	t = t.UTC()
	d := (int(t.Weekday()) + 6) % 7 // Monday = 0
	return time.Date(t.Year(), t.Month(), t.Day()-d, 0, 0, 0, 0, time.UTC)
}

func monthStart(t time.Time) time.Time {
	t = t.UTC()
	return time.Date(t.Year(), t.Month(), 1, 0, 0, 0, 0, time.UTC)
}

type quota struct {
	WordsUsed, WordsLimit int64 // limit 0 = unlimited
	AudioMs, AudioLimitMs int64
	Since, Resets         time.Time
}

// quotaFor sums the account's usage in its current period: the ISO week for Free, the
// calendar month (UTC) for Pro.
func (s *server) quotaFor(ctx context.Context, a account) (quota, error) {
	now := s.now()
	q := quota{Since: weekStart(now), WordsLimit: freeWordsPerWeek, AudioLimitMs: freeAudioSecondsPerWeek * 1000}
	q.Resets = q.Since.AddDate(0, 0, 7)
	if a.Plan == "pro" {
		q = quota{Since: monthStart(now), AudioLimitMs: proAudioSecondsPerMonth * 1000}
		q.Resets = q.Since.AddDate(0, 1, 0)
	}
	err := s.db.QueryRowContext(ctx, `SELECT COALESCE(SUM(words), 0), COALESCE(SUM(audio_ms), 0) FROM usage WHERE account_id = ? AND at >= ?`,
		a.ID, q.Since.Unix()).Scan(&q.WordsUsed, &q.AudioMs)
	return q, err
}

// overQuota answers 402 and returns true when the account may not use the proxy right now.
func (s *server) overQuota(w http.ResponseWriter, r *http.Request, a account) bool {
	q, err := s.quotaFor(r.Context(), a)
	if err != nil {
		internalError(w, "quota", err)
		return true
	}
	resets := q.Resets.Format("Monday 2 January 15:04 UTC")
	switch {
	case q.WordsLimit > 0 && q.WordsUsed >= q.WordsLimit:
		apiError(w, http.StatusPaymentRequired, "quota_exceeded",
			fmt.Sprintf("You have used this week's %d free words. The allowance resets %s, or upgrade to Pro for unlimited words.", q.WordsLimit, resets))
	case q.AudioMs >= q.AudioLimitMs:
		apiError(w, http.StatusPaymentRequired, "quota_exceeded",
			fmt.Sprintf("You have reached the fair-use limit of %d audio hours for this period. It resets %s.", q.AudioLimitMs/3_600_000, resets))
	default:
		return false
	}
	return true
}

// GET /v1/me
func (s *server) me(w http.ResponseWriter, r *http.Request, a account) {
	q, err := s.quotaFor(r.Context(), a)
	if err != nil {
		internalError(w, "me", err)
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{
		"email":               a.Email,
		"plan":                a.Plan,
		"words_used":          q.WordsUsed,
		"words_limit":         q.WordsLimit,
		"resets_at":           q.Resets.Format(time.RFC3339),
		"audio_seconds_used":  q.AudioMs / 1000,
		"audio_seconds_limit": q.AudioLimitMs / 1000,
	})
}

// wavSeconds reads the duration of a PCM WAV file from its header. The data size is clamped
// to the bytes actually present and the byte rate is derived from the format fields (not the
// declared byte rate), so a doctored header cannot shrink the metered duration.
func wavSeconds(b []byte) (float64, bool) {
	if len(b) < 12 || string(b[0:4]) != "RIFF" || string(b[8:12]) != "WAVE" {
		return 0, false
	}
	var byteRate float64
	for p := 12; p+8 <= len(b); {
		id, size := string(b[p:p+4]), int(binary.LittleEndian.Uint32(b[p+4:p+8]))
		body := p + 8
		switch id {
		case "fmt ":
			if size < 16 || body+16 > len(b) {
				return 0, false
			}
			ch := float64(binary.LittleEndian.Uint16(b[body+2:]))
			rate := float64(binary.LittleEndian.Uint32(b[body+4:]))
			bits := float64(binary.LittleEndian.Uint16(b[body+14:]))
			if ch < 1 || ch > 8 || rate < 8000 || rate > 192000 || (bits != 8 && bits != 16 && bits != 24 && bits != 32) {
				return 0, false
			}
			byteRate = rate * ch * bits / 8
		case "data":
			if byteRate == 0 {
				return 0, false
			}
			return float64(min(size, len(b)-body)) / byteRate, true
		}
		p = body + size + size%2
	}
	return 0, false
}

// estimateSeconds is the fallback when neither a WAV header nor the upstream reports a
// duration. It assumes 16 kbit/s, lower than any speech codec a client is likely to send,
// so it over-counts rather than under-counts (an over-count only matters at the fair-use cap).
func estimateSeconds(n int) float64 { return float64(n) / 2000 }
