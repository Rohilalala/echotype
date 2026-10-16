package main

import (
	"crypto/sha256"
	"database/sql"
	"encoding/json"
	"errors"
	"log/slog"
	"net"
	"net/http"
	"strconv"
	"strings"
	"sync"
	"time"
)

type server struct {
	cfg       config
	db        *sql.DB
	now       func() time.Time
	upstream  *http.Client
	stripe    *http.Client
	stripeAPI string // https://api.stripe.com, swapped in tests
	sendCode  func(email, code string) error
	limits    limiter

	mu       sync.Mutex
	inflight map[int64]int
}

func newServer(cfg config, db *sql.DB) *server {
	s := &server{
		cfg:       cfg,
		db:        db,
		now:       time.Now,
		upstream:  &http.Client{Timeout: 150 * time.Second},
		stripe:    &http.Client{Timeout: 30 * time.Second},
		stripeAPI: "https://api.stripe.com",
		limits:    limiter{hits: map[string]*window{}},
		inflight:  map[int64]int{},
	}
	s.sendCode = s.mailCode
	return s
}

func (s *server) routes() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("GET /healthz", func(w http.ResponseWriter, _ *http.Request) { w.Write([]byte("ok")) })
	mux.HandleFunc("GET /{$}", page("index.html"))
	mux.HandleFunc("GET /terms", page("terms.html"))
	mux.HandleFunc("GET /privacy", page("privacy.html"))
	mux.HandleFunc("GET /style.css", page("style.css"))
	mux.HandleFunc("GET /billing/done", page("done.html"))

	mux.HandleFunc("POST /v1/auth/start", s.authStart)
	mux.HandleFunc("POST /v1/auth/verify", s.authVerify)
	mux.HandleFunc("GET /v1/me", s.authed(s.me))
	mux.HandleFunc("GET /v1/models", s.authed(s.models))
	mux.HandleFunc("POST /v1/audio/transcriptions", s.authed(s.audio("transcriptions")))
	mux.HandleFunc("POST /v1/audio/translations", s.authed(s.audio("translations")))
	mux.HandleFunc("POST /v1/chat/completions", s.authed(s.chat))
	mux.HandleFunc("POST /v1/summary", s.authed(s.summary))
	mux.HandleFunc("POST /v1/billing/checkout", s.authed(s.checkout))
	mux.HandleFunc("POST /v1/billing/portal", s.authed(s.portal))
	mux.HandleFunc("POST /webhooks/stripe", s.stripeWebhook)
	return logRequests(mux)
}

// ---------- responses ----------

func writeJSON(w http.ResponseWriter, status int, v any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	json.NewEncoder(w).Encode(v)
}

// apiError writes the OpenAI error shape: {"error":{"message","type","code"}}.
func apiError(w http.ResponseWriter, status int, typ, msg string) {
	writeJSON(w, status, map[string]any{"error": map[string]any{"message": msg, "type": typ, "code": typ}})
}

func internalError(w http.ResponseWriter, where string, err error) {
	slog.Error(where, "err", err)
	apiError(w, http.StatusInternalServerError, "server_error", "Something went wrong on our side. Try again.")
}

// readJSON decodes a small JSON body into v; it answers the client itself and returns false on failure.
func readJSON(w http.ResponseWriter, r *http.Request, limit int64, v any) bool {
	dec := json.NewDecoder(http.MaxBytesReader(w, r.Body, limit))
	dec.DisallowUnknownFields()
	if err := dec.Decode(v); err != nil {
		if tooLarge(err) {
			apiError(w, http.StatusRequestEntityTooLarge, "request_too_large", "Request body is too large.")
		} else {
			apiError(w, http.StatusBadRequest, "invalid_request_error", "Request body must be valid JSON.")
		}
		return false
	}
	return true
}

func tooLarge(err error) bool {
	var mbe *http.MaxBytesError
	return errors.As(err, &mbe)
}

// ---------- request context ----------

func (s *server) clientIP(r *http.Request) string {
	if s.cfg.TrustProxy {
		if xff := r.Header.Get("X-Forwarded-For"); xff != "" {
			parts := strings.Split(xff, ",")
			return strings.TrimSpace(parts[len(parts)-1])
		}
	}
	host, _, err := net.SplitHostPort(r.RemoteAddr)
	if err != nil {
		return r.RemoteAddr
	}
	return host
}

type account struct {
	ID    int64
	Email string
	Plan  string
}

func hashSecret(secret string) []byte {
	h := sha256.Sum256([]byte(secret))
	return h[:]
}

// authed resolves "Authorization: Bearer et_live_..." to an account. Keys are looked up by
// their SHA-256 hash, so the stored value is useless to anyone who reads the database and
// lookup timing reveals nothing about the key itself.
func (s *server) authed(h func(http.ResponseWriter, *http.Request, account)) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		key, ok := strings.CutPrefix(r.Header.Get("Authorization"), "Bearer ")
		key = strings.TrimSpace(key)
		if !ok || !strings.HasPrefix(key, keyPrefix) || len(key) != len(keyPrefix)+64 {
			apiError(w, http.StatusUnauthorized, "invalid_api_key", "Missing or invalid API key.")
			return
		}
		var a account
		err := s.db.QueryRowContext(r.Context(),
			`SELECT a.id, a.email, a.plan FROM api_keys k JOIN accounts a ON a.id = k.account_id WHERE k.hash = ?`,
			hashSecret(key)).Scan(&a.ID, &a.Email, &a.Plan)
		if errors.Is(err, sql.ErrNoRows) {
			apiError(w, http.StatusUnauthorized, "invalid_api_key", "Missing or invalid API key.")
			return
		}
		if err != nil {
			internalError(w, "key lookup", err)
			return
		}
		h(w, r, a)
	}
}

// ---------- rate and concurrency limits ----------

type window struct {
	n     int
	reset time.Time
}

// limiter is a fixed-window counter per key.
// ponytail: in-process memory, so limits are per instance and reset on restart;
// move to the database or Redis when running more than one instance.
type limiter struct {
	mu   sync.Mutex
	hits map[string]*window
}

func (l *limiter) allow(key string, max int, per time.Duration, now time.Time) bool {
	l.mu.Lock()
	defer l.mu.Unlock()
	if len(l.hits) > 100_000 {
		for k, w := range l.hits {
			if now.After(w.reset) {
				delete(l.hits, k)
			}
		}
	}
	w := l.hits[key]
	if w == nil || now.After(w.reset) {
		w = &window{reset: now.Add(per)}
		l.hits[key] = w
	}
	w.n++
	return w.n <= max
}

const maxInflight = 3

// acquire reserves one of the account's in-flight slots; call the returned func when done.
func (s *server) acquire(id int64) (func(), bool) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.inflight[id] >= maxInflight {
		return nil, false
	}
	s.inflight[id]++
	return func() {
		s.mu.Lock()
		defer s.mu.Unlock()
		if s.inflight[id]--; s.inflight[id] <= 0 {
			delete(s.inflight, id)
		}
	}, true
}

// limitKey applies the per-key request rate and concurrency caps shared by the proxy routes.
func (s *server) limitKey(w http.ResponseWriter, a account) (func(), bool) {
	if !s.limits.allow("key:"+strconv.FormatInt(a.ID, 10), 60, time.Minute, s.now()) {
		apiError(w, http.StatusTooManyRequests, "rate_limit_exceeded", "Too many requests. Wait a minute and try again.")
		return nil, false
	}
	release, ok := s.acquire(a.ID)
	if !ok {
		apiError(w, http.StatusTooManyRequests, "rate_limit_exceeded", "Too many requests in flight. Wait for one to finish.")
		return nil, false
	}
	return release, true
}

// ---------- logging ----------

type statusWriter struct {
	http.ResponseWriter
	status int
}

func (w *statusWriter) WriteHeader(code int) {
	w.status = code
	w.ResponseWriter.WriteHeader(code)
}

// logRequests logs method, path, status and duration. Never headers or bodies:
// those carry keys, codes, audio and transcripts.
func logRequests(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		start := time.Now()
		sw := &statusWriter{ResponseWriter: w, status: http.StatusOK}
		next.ServeHTTP(sw, r)
		slog.Info("request", "method", r.Method, "path", r.URL.Path, "status", sw.status, "ms", time.Since(start).Milliseconds())
	})
}
