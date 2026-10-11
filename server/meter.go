package main

import (
	"context"
	"database/sql"
	"encoding/binary"
	"fmt"
	"log/slog"
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
	// Longest single recording accepted, by plan.
	freeMaxRequestSeconds = 10 * 60
	proMaxRequestSeconds  = 60 * 60
	// 6 kbit/s, below any speech codec a client normally sends, so bytes divided by this is
	// an upper bound on a compressed file's duration before the upstream has seen it.
	// ponytail: a crafted file (e.g. FLAC of silence) can still be longer than this bound; it
	// is metered at its real length afterwards, so the overshoot is one request. Probe the
	// duration with a demuxer before forwarding if that ever shows up in the bill.
	boundBytesPerSecond = 750
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
	MaxRequestMs          int64
	Since, Resets         time.Time
}

type querier interface {
	QueryRowContext(context.Context, string, ...any) *sql.Row
}

// quotaFor sums the account's usage in its current period: the ISO week for Free, the
// calendar month (UTC) for Pro. Pending reservations count as used.
func (s *server) quotaFor(ctx context.Context, db querier, a account) (quota, error) {
	now := s.now()
	q := quota{Since: weekStart(now), WordsLimit: freeWordsPerWeek, AudioLimitMs: freeAudioSecondsPerWeek * 1000, MaxRequestMs: freeMaxRequestSeconds * 1000}
	q.Resets = q.Since.AddDate(0, 0, 7)
	if a.Plan == "pro" {
		q = quota{Since: monthStart(now), AudioLimitMs: proAudioSecondsPerMonth * 1000, MaxRequestMs: proMaxRequestSeconds * 1000}
		q.Resets = q.Since.AddDate(0, 1, 0)
	}
	err := db.QueryRowContext(ctx, `SELECT COALESCE(SUM(words), 0), COALESCE(SUM(audio_ms), 0) FROM usage WHERE account_id = ? AND at >= ?`,
		a.ID, q.Since.Unix()).Scan(&q.WordsUsed, &q.AudioMs)
	return q, err
}

// overQuota answers 402 and returns true when the account has nothing left. It is the cheap
// check made before reading a request body; reserve makes the binding one.
func (s *server) overQuota(w http.ResponseWriter, r *http.Request, a account) bool {
	q, err := s.quotaFor(r.Context(), s.db, a)
	if err != nil {
		internalError(w, "quota", err)
		return true
	}
	return refuse(w, q, 0)
}

// refuse answers the client and returns true when a request needing up to audioMs of audio
// does not fit the account's remaining allowance or the plan's per-request cap.
func refuse(w http.ResponseWriter, q quota, audioMs int64) bool {
	resets := q.Resets.Format("Monday 2 January 15:04 UTC")
	left := q.AudioLimitMs - q.AudioMs
	switch {
	case q.WordsLimit > 0 && q.WordsUsed >= q.WordsLimit:
		apiError(w, http.StatusPaymentRequired, "quota_exceeded",
			fmt.Sprintf("You have used this week's %d free words. The allowance resets %s, or upgrade to Pro for unlimited words.", q.WordsLimit, resets))
	case left <= 0:
		apiError(w, http.StatusPaymentRequired, "quota_exceeded",
			fmt.Sprintf("You have used this period's %d hours of audio. It resets %s.", q.AudioLimitMs/3_600_000, resets))
	case audioMs > left:
		apiError(w, http.StatusPaymentRequired, "quota_exceeded",
			fmt.Sprintf("This recording may be up to %d minutes long, more than the %d minutes of audio left this period. It resets %s.",
				(audioMs+59_999)/60_000, left/60_000, resets))
	case audioMs > q.MaxRequestMs:
		apiError(w, http.StatusBadRequest, "invalid_request_error",
			fmt.Sprintf("Recordings are limited to %d minutes per request on this plan.", q.MaxRequestMs/60_000))
	default:
		return false
	}
	return true
}

// reserve checks the account's allowance against this request's upper bound and, if it fits,
// records that bound as a usage row in the same transaction. Requests in flight therefore
// see each other's reservations and cannot jointly overshoot. The caller must settle or
// release the returned row. It answers the client itself when it returns false.
func (s *server) reserve(w http.ResponseWriter, r *http.Request, a account, audioMs int64) (int64, bool) {
	tx, err := s.db.BeginTx(r.Context(), nil)
	if err != nil {
		internalError(w, "reserve", err)
		return 0, false
	}
	defer tx.Rollback()
	q, err := s.quotaFor(r.Context(), tx, a)
	if err != nil {
		internalError(w, "reserve", err)
		return 0, false
	}
	if refuse(w, q, audioMs) {
		return 0, false
	}
	var id int64
	err = tx.QueryRowContext(r.Context(), `INSERT INTO usage (account_id, at, words, audio_ms) VALUES (?, ?, 0, ?) RETURNING rowid`,
		a.ID, s.now().Unix(), audioMs).Scan(&id)
	if err == nil {
		err = tx.Commit()
	}
	if err != nil {
		internalError(w, "reserve", err)
		return 0, false
	}
	return id, true
}

// settle replaces a reservation with what the request really used. It runs even if the
// client has gone away: the upstream has already billed us.
func (s *server) settle(id, words, audioMs int64) {
	if _, err := s.db.Exec(`UPDATE usage SET words = ?, audio_ms = ? WHERE rowid = ?`, words, audioMs, id); err != nil {
		slog.Error("settle usage", "row", id, "words", words, "audio_ms", audioMs, "err", err)
	}
}

// release drops the reservation of a request the upstream did not serve.
func (s *server) release(id int64) {
	if _, err := s.db.Exec(`DELETE FROM usage WHERE rowid = ?`, id); err != nil {
		slog.Error("release usage", "row", id, "err", err)
	}
}

// GET /v1/me
func (s *server) me(w http.ResponseWriter, r *http.Request, a account) {
	q, err := s.quotaFor(r.Context(), s.db, a)
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

// boundSeconds is the most audio a file can hold before the upstream has seen it: exact for
// WAV (the desktop app always sends 16 kHz mono WAV), else bytes at 6 kbit/s.
func boundSeconds(audio []byte) float64 {
	if secs, ok := wavSeconds(audio); ok {
		return secs
	}
	return float64(len(audio)) / boundBytesPerSecond
}
