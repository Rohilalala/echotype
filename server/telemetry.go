package main

import (
	"encoding/json"
	"net/http"
	"regexp"
	"time"
)

// Opt-in anonymous usage counts and crash reports from the app (telemetry.rs there). No auth:
// the install id is random and tied to no account. Unknown fields are rejected, so a client
// can only ever store the fields below.

type telemetryDay struct {
	Date             string         `json:"date"`
	Dictations       int            `json:"dictations"`
	Words            int            `json:"words"`
	Failures         map[string]int `json:"failures"`
	SpeechMsMedian   *int           `json:"speech_ms_median"`
	CleanupMsMedian  *int           `json:"cleanup_ms_median"`
	SpeechProvider   string         `json:"speech_provider"`
	SpeechModel      string         `json:"speech_model"`
	CleanupProvider  string         `json:"cleanup_provider"`
	CleanupModel     string         `json:"cleanup_model"`
	MeetingsStarted  int            `json:"meetings_started"`
	MeetingsFinished int            `json:"meetings_finished"`
}

type telemetryCrash struct {
	Version   string `json:"version"`
	OS        string `json:"os"`
	Arch      string `json:"arch"`
	Message   string `json:"message"`
	Backtrace string `json:"backtrace"`
}

type telemetryBatch struct {
	InstallID string           `json:"install_id"`
	Version   string           `json:"version"`
	OS        string           `json:"os"`
	Arch      string           `json:"arch"`
	Days      []telemetryDay   `json:"days"`
	Crashes   []telemetryCrash `json:"crashes"`
}

var installID = regexp.MustCompile(`^[0-9a-f]{32}$`)

const telemetryLimit = 64 << 10

func (s *server) telemetry(w http.ResponseWriter, r *http.Request) {
	if !s.limits.allow("telemetry-ip:"+s.clientIP(r), 30, time.Hour, s.now()) {
		apiError(w, http.StatusTooManyRequests, "rate_limit_exceeded", "Too many requests. Try again later.")
		return
	}
	var b telemetryBatch
	if !readJSON(w, r, telemetryLimit, &b) {
		return
	}
	short := func(v string) bool { return v != "" && len(v) <= 32 }
	ok := installID.MatchString(b.InstallID) && short(b.Version) && short(b.OS) && short(b.Arch) && len(b.Days) <= 31 && len(b.Crashes) <= 10
	for _, d := range b.Days {
		if _, err := time.Parse(time.DateOnly, d.Date); err != nil {
			ok = false
		}
	}
	if !ok {
		apiError(w, http.StatusBadRequest, "invalid_request_error", "Not a valid telemetry batch.")
		return
	}
	tx, err := s.db.BeginTx(r.Context(), nil)
	if err != nil {
		internalError(w, "telemetry begin", err)
		return
	}
	defer tx.Rollback()
	at := s.now().Unix()
	for _, d := range b.Days {
		data, _ := json.Marshal(d)
		// A resend after a lost response replaces the day instead of counting it twice.
		if _, err := tx.ExecContext(r.Context(), `INSERT OR REPLACE INTO telemetry_days (install_id, date, version, os, arch, data, received_at) VALUES (?, ?, ?, ?, ?, ?, ?)`,
			b.InstallID, d.Date, b.Version, b.OS, b.Arch, string(data), at); err != nil {
			internalError(w, "telemetry day", err)
			return
		}
	}
	for _, c := range b.Crashes {
		if _, err := tx.ExecContext(r.Context(), `INSERT INTO telemetry_crashes (install_id, version, os, arch, message, backtrace, received_at) VALUES (?, ?, ?, ?, ?, ?, ?)`,
			b.InstallID, c.Version, c.OS, c.Arch, c.Message, c.Backtrace, at); err != nil {
			internalError(w, "telemetry crash", err)
			return
		}
	}
	if err := tx.Commit(); err != nil {
		internalError(w, "telemetry commit", err)
		return
	}
	w.WriteHeader(http.StatusNoContent)
}
