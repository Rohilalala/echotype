package main

import (
	"bytes"
	"crypto/hmac"
	"crypto/sha256"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"mime/multipart"
	"net/http"
	"net/http/httptest"
	"os"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

func TestMain(m *testing.M) {
	slog.SetDefault(slog.New(slog.NewTextHandler(io.Discard, nil)))
	os.Exit(m.Run())
}

// Monday 2026-10-12 10:00 UTC
var t0 = time.Date(2026, 10, 12, 10, 0, 0, 0, time.UTC)

type harness struct {
	s   *server
	h   http.Handler
	now time.Time
	// The fake Supabase: access token -> the user GET /auth/v1/user answers with.
	users     map[string]map[string]any
	supaDown  bool
	supaCalls int
}

func newHarness(t *testing.T, upstream http.HandlerFunc) *harness {
	t.Helper()
	db, err := openDB(":memory:")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { db.Close() })
	cfg := config{PublicURL: "https://cloud.test", STTModel: "whisper-large-v3-turbo", TranslateModel: "whisper-large-v3", LLMModel: "openai/gpt-oss-20b",
		STTKey: "server-stt-key", LLMKey: "server-llm-key", StripeWebhookSecret: "whsec_test"}
	if upstream != nil {
		up := httptest.NewServer(upstream)
		t.Cleanup(up.Close)
		cfg.STTURL, cfg.LLMURL = up.URL, up.URL
	}
	x := &harness{now: t0, users: map[string]map[string]any{}}
	supa := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		x.supaCalls++
		u, ok := x.users[strings.TrimPrefix(r.Header.Get("Authorization"), "Bearer ")]
		switch {
		case x.supaDown:
			w.WriteHeader(500)
		case r.Method != "GET" || r.URL.Path != "/auth/v1/user" || r.Header.Get("apikey") != "anon-key" || !ok:
			w.WriteHeader(401)
		default:
			json.NewEncoder(w).Encode(u)
		}
	}))
	t.Cleanup(supa.Close)
	cfg.SupabaseURL, cfg.SupabaseAnonKey = supa.URL, "anon-key"
	x.s = newServer(cfg, db)
	x.s.now = func() time.Time { return x.now }
	x.h = x.s.routes()
	return x
}

func (x *harness) do(method, path string, body io.Reader, hdr ...string) *httptest.ResponseRecorder {
	r := httptest.NewRequest(method, path, body)
	for i := 0; i+1 < len(hdr); i += 2 {
		r.Header.Set(hdr[i], hdr[i+1])
	}
	w := httptest.NewRecorder()
	x.h.ServeHTTP(w, r)
	return w
}

func (x *harness) json(method, path, body string, hdr ...string) *httptest.ResponseRecorder {
	return x.do(method, path, strings.NewReader(body), append([]string{"Content-Type", "application/json"}, hdr...)...)
}

// signIn signs in through the fake Supabase as a user with this confirmed email and returns the API key.
func (x *harness) signIn(t *testing.T, email string) string {
	t.Helper()
	token := fmt.Sprintf("supa-token-%d", len(x.users))
	x.users[token] = map[string]any{"id": "u-" + email, "email": email, "email_confirmed_at": "2026-10-12T09:00:00Z"}
	w := x.json("POST", "/v1/auth/supabase", `{"access_token":"`+token+`"}`)
	if w.Code != 200 {
		t.Fatalf("sign in: %d %s", w.Code, w.Body)
	}
	key, _ := decode(t, w)["api_key"].(string)
	return key
}

func bearer(key string) []string { return []string{"Authorization", "Bearer " + key} }

func decode(t *testing.T, w *httptest.ResponseRecorder) map[string]any {
	t.Helper()
	var m map[string]any
	if err := json.Unmarshal(w.Body.Bytes(), &m); err != nil {
		t.Fatalf("not JSON (%d): %s", w.Code, w.Body)
	}
	return m
}

func errType(t *testing.T, w *httptest.ResponseRecorder) string {
	t.Helper()
	e, _ := decode(t, w)["error"].(map[string]any)
	s, _ := e["type"].(string)
	return s
}

// ---------- auth ----------

func TestAuthFlowAndKeyHashing(t *testing.T) {
	x := newHarness(t, nil)
	key := x.signIn(t, " Ada@Example.com ")
	if !strings.HasPrefix(key, "et_live_") || len(key) != 8+64 {
		t.Fatalf("key shape: %q", key)
	}
	var n int
	x.s.db.QueryRow(`SELECT COUNT(*) FROM api_keys WHERE hash = ?`, hashSecret(key)).Scan(&n)
	if n != 1 {
		t.Fatal("key hash not stored")
	}
	x.s.db.QueryRow(`SELECT COUNT(*) FROM api_keys WHERE hash = ?`, key).Scan(&n)
	if n != 0 {
		t.Fatal("raw key stored")
	}

	me := decode(t, x.do("GET", "/v1/me", nil, bearer(key)...))
	if me["email"] != "ada@example.com" || me["plan"] != "free" || me["words_limit"] != float64(2000) {
		t.Fatalf("me: %v", me)
	}
	// Second sign-in: same account, new key, both keys work.
	key2 := x.signIn(t, "ada@example.com")
	if key2 == key {
		t.Fatal("key reused")
	}
	x.s.db.QueryRow(`SELECT COUNT(*) FROM accounts`).Scan(&n)
	if n != 1 {
		t.Fatalf("accounts = %d", n)
	}
	for _, k := range []string{key, key2} {
		if w := x.do("GET", "/v1/me", nil, bearer(k)...); w.Code != 200 {
			t.Fatal("key rejected", w.Code)
		}
	}
	for _, h := range []string{"", "Bearer et_live_" + strings.Repeat("0", 64), "Bearer nope", "Basic " + key} {
		if w := x.do("GET", "/v1/me", nil, "Authorization", h); w.Code != 401 || errType(t, w) != "invalid_api_key" {
			t.Fatalf("auth %q: %d", h, w.Code)
		}
	}
}

func TestSupabaseSignInNeedsATokenSupabaseVouchesFor(t *testing.T) {
	x := newHarness(t, nil)
	x.users["unconfirmed"] = map[string]any{"email": "a@b.co", "email_confirmed_at": nil}
	x.users["no-email"] = map[string]any{"email": "", "email_confirmed_at": "2026-10-12T09:00:00Z"} // a phone-only user
	x.users["bad-email"] = map[string]any{"email": "Ada <a@b.co>", "email_confirmed_at": "2026-10-12T09:00:00Z"}
	for _, tok := range []string{"unknown", "unconfirmed", "no-email", "bad-email"} {
		w := x.json("POST", "/v1/auth/supabase", `{"access_token":"`+tok+`"}`)
		if w.Code != 401 || errType(t, w) != "invalid_token" {
			t.Fatalf("%s: %d %s", tok, w.Code, w.Body)
		}
	}
	calls := x.supaCalls
	if w := x.json("POST", "/v1/auth/supabase", `{"access_token":"  "}`); w.Code != 401 || x.supaCalls != calls {
		t.Fatalf("blank token: %d, asked Supabase: %v", w.Code, x.supaCalls != calls)
	}
	var n int
	x.s.db.QueryRow(`SELECT COUNT(*) FROM accounts`).Scan(&n)
	if n != 0 {
		t.Fatalf("accounts = %d", n)
	}
}

func TestSupabaseDownIsNotASignInFailure(t *testing.T) {
	x := newHarness(t, nil)
	x.users["tok"] = map[string]any{"email": "a@b.co", "email_confirmed_at": "2026-10-12T09:00:00Z"}
	x.supaDown = true
	if w := x.json("POST", "/v1/auth/supabase", `{"access_token":"tok"}`); w.Code != 502 || errType(t, w) != "auth_unavailable" {
		t.Fatalf("Supabase down: %d %s", w.Code, w.Body)
	}
	x.supaDown = false
	if w := x.json("POST", "/v1/auth/supabase", `{"access_token":"tok"}`); w.Code != 200 {
		t.Fatalf("Supabase back: %d", w.Code)
	}
}

func TestSupabaseSignInRateLimitPerIP(t *testing.T) {
	x := newHarness(t, nil)
	for i := 0; i < 30; i++ {
		if w := x.json("POST", "/v1/auth/supabase", `{"access_token":"wrong"}`); w.Code != 401 {
			t.Fatalf("attempt %d: %d", i, w.Code)
		}
	}
	if w := x.json("POST", "/v1/auth/supabase", `{"access_token":"wrong"}`); w.Code != 429 {
		t.Fatalf("31st attempt from one IP: %d", w.Code)
	}
	x.now = x.now.Add(10*time.Minute + time.Second)
	if w := x.json("POST", "/v1/auth/supabase", `{"access_token":"wrong"}`); w.Code != 401 {
		t.Fatalf("after window: %d", w.Code)
	}
}

// ---------- quota ----------

func TestWeekStart(t *testing.T) {
	cases := map[string]string{
		"2026-10-12T00:00:00Z": "2026-10-12T00:00:00Z", // Monday midnight
		"2026-10-18T23:59:59Z": "2026-10-12T00:00:00Z", // Sunday night
		"2026-10-19T00:00:00Z": "2026-10-19T00:00:00Z",
		"2026-11-01T12:00:00Z": "2026-10-26T00:00:00Z", // across a month
		"2027-01-01T08:00:00Z": "2026-12-28T00:00:00Z", // across a year
	}
	for in, want := range cases {
		ts, _ := time.Parse(time.RFC3339, in)
		if got := weekStart(ts).Format(time.RFC3339); got != want {
			t.Errorf("weekStart(%s) = %s, want %s", in, got, want)
		}
	}
	// A non-UTC zone still resolves to the UTC week.
	ist := time.FixedZone("IST", 5*3600+1800)
	if got := weekStart(time.Date(2026, 10, 19, 3, 0, 0, 0, ist)); !got.Equal(time.Date(2026, 10, 12, 0, 0, 0, 0, time.UTC)) {
		t.Errorf("IST Monday 03:00 is Sunday UTC: got %s", got)
	}
}

func accountID(t *testing.T, x *harness, email string) int64 {
	var id int64
	if err := x.s.db.QueryRow(`SELECT id FROM accounts WHERE email = ?`, email).Scan(&id); err != nil {
		t.Fatal(err)
	}
	return id
}

func TestFreeQuota(t *testing.T) {
	x := newHarness(t, fakeUpstream(t, "one two three", 2))
	key := x.signIn(t, "a@b.co")
	id := accountID(t, x, "a@b.co")
	// Last week's usage does not count.
	x.s.db.Exec(`INSERT INTO usage (account_id, at, words, audio_ms) VALUES (?, ?, 5000, 1000)`, id, t0.Add(-11*time.Hour).Unix())
	x.s.db.Exec(`INSERT INTO usage (account_id, at, words, audio_ms) VALUES (?, ?, 1999, 1000)`, id, t0.Add(-time.Hour).Unix())
	me := decode(t, x.do("GET", "/v1/me", nil, bearer(key)...))
	if me["words_used"] != float64(1999) || me["resets_at"] != "2026-10-19T00:00:00Z" {
		t.Fatalf("me: %v", me)
	}
	if w := transcribeReq(x, key, wavFile(1), nil); w.Code != 200 {
		t.Fatalf("under quota: %d %s", w.Code, w.Body)
	}
	w := transcribeReq(x, key, wavFile(1), nil)
	if w.Code != 402 || errType(t, w) != "quota_exceeded" || !strings.Contains(w.Body.String(), "Monday 19 October 00:00 UTC") {
		t.Fatalf("at quota: %d %s", w.Code, w.Body)
	}
	if w := x.json("POST", "/v1/chat/completions", `{"messages":[{"role":"user","content":"x"}]}`, bearer(key)...); w.Code != 402 {
		t.Fatalf("chat over quota: %d", w.Code)
	}
	// Monday 00:00 UTC resets it.
	x.now = time.Date(2026, 10, 19, 0, 0, 0, 0, time.UTC)
	if w := transcribeReq(x, key, wavFile(1), nil); w.Code != 200 {
		t.Fatalf("after reset: %d", w.Code)
	}
}

func TestProFairUse(t *testing.T) {
	x := newHarness(t, fakeUpstream(t, "hi", 1))
	key := x.signIn(t, "a@b.co")
	id := accountID(t, x, "a@b.co")
	x.s.db.Exec(`UPDATE accounts SET plan = 'pro' WHERE id = ?`, id)
	x.s.db.Exec(`INSERT INTO usage (account_id, at, words, audio_ms) VALUES (?, ?, 900000, ?)`, id, t0.Add(-24*time.Hour).Unix(), int64(40*3600*1000-1000)) // exactly 1 s left
	x.s.db.Exec(`INSERT INTO usage (account_id, at, words, audio_ms) VALUES (?, ?, 1, 99999999)`, id, time.Date(2026, 9, 30, 0, 0, 0, 0, time.UTC).Unix())  // last month
	me := decode(t, x.do("GET", "/v1/me", nil, bearer(key)...))
	if me["plan"] != "pro" || me["words_limit"] != float64(0) || me["resets_at"] != "2026-11-01T00:00:00Z" {
		t.Fatalf("me: %v", me)
	}
	if w := transcribeReq(x, key, wavFile(1), nil); w.Code != 200 {
		t.Fatalf("pro under ceiling with 900k words: %d", w.Code)
	}
	if w := transcribeReq(x, key, wavFile(1), nil); w.Code != 402 || !strings.Contains(w.Body.String(), "40 hours of audio") {
		t.Fatalf("pro over ceiling: %d %s", w.Code, w.Body)
	}
}

// ---------- transcription proxy ----------

// wavFile builds a 16 kHz mono 16-bit PCM WAV of the given seconds, as the app sends.
func wavFile(secs int) []byte { return wavOf(1, 16000, 16, secs) }

// wavOf builds a mono WAV with the given format tag, sample rate and bit depth.
func wavOf(tag uint16, rate uint32, bits uint16, secs int) []byte {
	data := make([]byte, int(rate)*int(bits/8)*secs)
	var b bytes.Buffer
	b.WriteString("RIFF")
	binary.Write(&b, binary.LittleEndian, uint32(36+len(data)))
	b.WriteString("WAVEfmt ")
	for _, v := range []any{uint32(16), tag, uint16(1), rate, rate * uint32(bits/8), bits / 8, bits} {
		binary.Write(&b, binary.LittleEndian, v)
	}
	b.WriteString("data")
	binary.Write(&b, binary.LittleEndian, uint32(len(data)))
	b.Write(data)
	return b.Bytes()
}

func TestWavSeconds(t *testing.T) {
	if s, ok := wavSeconds(wavFile(3)); !ok || s != 3 {
		t.Fatalf("got %v %v", s, ok)
	}
	// A header claiming more data than is present is clamped to the bytes sent.
	if s, _ := wavSeconds(wavFile(3)[:44+32000]); s != 1 {
		t.Fatalf("truncated: %v", s)
	}
	// The declared byte rate is ignored: a huge one cannot shrink the duration.
	f := wavFile(2)
	binary.LittleEndian.PutUint32(f[28:], 1<<30)
	if s, _ := wavSeconds(f); s != 2 {
		t.Fatalf("doctored byte rate: %v", s)
	}
	if _, ok := wavSeconds([]byte("ID3\x03 not a wav")); ok {
		t.Fatal("mp3 parsed as wav")
	}
	if _, ok := wavSeconds(wavOf(0x55, 16000, 16, 1)); ok {
		t.Fatal("MP3-in-WAV (format 0x55) accepted")
	}
	if s, ok := wavSeconds(wavOf(3, 8000, 32, 2)); !ok || s != 2 {
		t.Fatalf("float WAV: %v %v", s, ok)
	}
}

type upstreamSeen struct {
	path, auth, model, format, filename, language string
	body                                          map[string]any
}

var lastSeen upstreamSeen

// fakeUpstream answers /audio/transcriptions with text and duration, and /chat/completions
// with a canned reply, recording what it received.
func fakeUpstream(t *testing.T, text string, duration float64) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		lastSeen = upstreamSeen{path: r.URL.Path, auth: r.Header.Get("Authorization")}
		switch r.URL.Path {
		case "/audio/transcriptions", "/audio/translations":
			if err := r.ParseMultipartForm(32 << 20); err != nil {
				t.Errorf("upstream multipart: %v", err)
			}
			lastSeen.model, lastSeen.format, lastSeen.language = r.FormValue("model"), r.FormValue("response_format"), r.FormValue("language")
			if _, fh, err := r.FormFile("file"); err == nil {
				lastSeen.filename = fh.Filename
			}
			json.NewEncoder(w).Encode(map[string]any{"text": text, "duration": duration})
		case "/chat/completions":
			json.NewDecoder(r.Body).Decode(&lastSeen.body)
			io.WriteString(w, `{"choices":[{"message":{"role":"assistant","content":"Clean."}}]}`)
		default:
			http.NotFound(w, r)
		}
	}
}

func transcribeReq(x *harness, key string, audio []byte, fields map[string]string) *httptest.ResponseRecorder {
	return audioReq(x, "/v1/audio/transcriptions", key, audio, fields)
}

func audioReq(x *harness, path, key string, audio []byte, fields map[string]string) *httptest.ResponseRecorder {
	var b bytes.Buffer
	mw := multipart.NewWriter(&b)
	fw, _ := mw.CreateFormFile("file", "clip.wav")
	fw.Write(audio)
	for k, v := range fields {
		mw.WriteField(k, v)
	}
	mw.Close()
	return x.do("POST", path, &b, append(bearer(key), "Content-Type", mw.FormDataContentType())...)
}

func TestTranscribeProxy(t *testing.T) {
	x := newHarness(t, fakeUpstream(t, " Hello there, general  Kenobi. ", 0.5))
	key := x.signIn(t, "a@b.co")
	w := transcribeReq(x, key, wavFile(2), map[string]string{"model": "whisper-1", "language": "en", "response_format": "json"})
	if w.Code != 200 {
		t.Fatalf("%d %s", w.Code, w.Body)
	}
	if got := decode(t, w); got["text"] != " Hello there, general  Kenobi. " || len(got) != 1 {
		t.Fatalf("body: %v", got)
	}
	if lastSeen.auth != "Bearer server-stt-key" {
		t.Fatalf("upstream got auth %q, want the server key", lastSeen.auth)
	}
	if lastSeen.model != "whisper-large-v3-turbo" || lastSeen.format != "verbose_json" || lastSeen.language != "en" || lastSeen.filename != "audio.wav" {
		t.Fatalf("upstream saw %+v", lastSeen)
	}
	var words, ms int64
	x.s.db.QueryRow(`SELECT words, audio_ms FROM usage`).Scan(&words, &ms)
	if words != 4 || ms != 2000 { // WAV header (2 s) beats the upstream's smaller 0.5 s
		t.Fatalf("metered %d words, %d ms", words, ms)
	}

	// response_format=text returns plain text; an upstream duration above the header's wins.
	y := newHarness(t, fakeUpstream(t, "plain words", 7.25))
	key = y.signIn(t, "a@b.co")
	w = transcribeReq(y, key, wavFile(1), map[string]string{"response_format": "text"})
	if w.Code != 200 || w.Body.String() != "plain words" || !strings.HasPrefix(w.Header().Get("Content-Type"), "text/plain") {
		t.Fatalf("text format: %d %q", w.Code, w.Body)
	}
	y.s.db.QueryRow(`SELECT audio_ms FROM usage`).Scan(&ms)
	if ms != 7250 {
		t.Fatalf("audio_ms = %d", ms)
	}
	if me := decode(t, y.do("GET", "/v1/me", nil, bearer(key)...)); me["words_used"] != float64(2) || me["audio_seconds_used"] != float64(7) {
		t.Fatalf("me: %v", me)
	}
}

func TestTranslateProxy(t *testing.T) {
	x := newHarness(t, fakeUpstream(t, "Where are you going tomorrow", 2.5))
	key := x.signIn(t, "a@b.co")
	w := audioReq(x, "/v1/audio/translations", key, wavFile(2), map[string]string{"model": "whisper-1", "language": "hi"})
	if w.Code != 200 || decode(t, w)["text"] != "Where are you going tomorrow" {
		t.Fatalf("%d %s", w.Code, w.Body)
	}
	if lastSeen.path != "/audio/translations" || lastSeen.model != "whisper-large-v3" || lastSeen.auth != "Bearer server-stt-key" || lastSeen.language != "" {
		t.Fatalf("upstream saw %+v", lastSeen)
	}
	var words, ms int64
	x.s.db.QueryRow(`SELECT words, audio_ms FROM usage`).Scan(&words, &ms)
	if words != 5 || ms != 2500 {
		t.Fatalf("metered %d words, %d ms", words, ms)
	}
	// Same quota gate as transcriptions.
	x.s.db.Exec(`INSERT INTO usage (account_id, at, words, audio_ms) VALUES (?, ?, 2000, 0)`, accountID(t, x, "a@b.co"), t0.Unix())
	if w := audioReq(x, "/v1/audio/translations", key, wavFile(1), nil); w.Code != 402 {
		t.Fatalf("over quota: %d", w.Code)
	}
}

func TestTranscribeRejects(t *testing.T) {
	x := newHarness(t, fakeUpstream(t, "x", 1))
	key := x.signIn(t, "a@b.co")
	if w := transcribeReq(x, "et_live_"+strings.Repeat("a", 64), wavFile(1), nil); w.Code != 401 {
		t.Fatalf("bad key: %d", w.Code)
	}
	if w := transcribeReq(x, key, make([]byte, 25<<20+1), nil); w.Code != 413 || errType(t, w) != "request_too_large" {
		t.Fatalf("oversize: %d %s", w.Code, w.Body)
	}
	if w := transcribeReq(x, key, nil, nil); w.Code != 400 {
		t.Fatalf("empty file: %d", w.Code)
	}
	if w := transcribeReq(x, key, wavFile(1), map[string]string{"response_format": "srt"}); w.Code != 400 {
		t.Fatalf("srt: %d", w.Code)
	}
	if w := transcribeReq(x, key, wavFile(1), map[string]string{"language": "en; rm -rf"}); w.Code != 400 {
		t.Fatalf("bad language: %d", w.Code)
	}
	if w := x.do("POST", "/v1/audio/transcriptions", strings.NewReader("{}"), append(bearer(key), "Content-Type", "application/json")...); w.Code != 400 {
		t.Fatalf("not multipart: %d", w.Code)
	}
	var n int
	x.s.db.QueryRow(`SELECT COUNT(*) FROM usage`).Scan(&n)
	if n != 0 {
		t.Fatalf("rejected requests metered: %d", n)
	}
}

func TestUpstreamErrorsDoNotLookLikeClientAuth(t *testing.T) {
	x := newHarness(t, func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(401)
		io.WriteString(w, `{"error":{"message":"Invalid API Key"}}`)
	})
	key := x.signIn(t, "a@b.co")
	if w := transcribeReq(x, key, wavFile(1), nil); w.Code != 502 {
		t.Fatalf("upstream 401 became %d", w.Code)
	}
	var n int
	x.s.db.QueryRow(`SELECT COUNT(*) FROM usage`).Scan(&n)
	if n != 0 {
		t.Fatal("reservation kept for a request the upstream did not serve")
	}
}

// countingUpstream counts transcription calls and, when gate is non-nil, holds each one
// until gate is closed.
func countingUpstream(calls *atomic.Int32, gate chan struct{}, duration float64) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		calls.Add(1)
		if gate != nil {
			<-gate
		}
		json.NewEncoder(w).Encode(map[string]any{"text": "a b", "duration": duration})
	}
}

func TestOnlyWAVAndBoundCheckedBeforeUpstream(t *testing.T) {
	var calls atomic.Int32
	x := newHarness(t, countingUpstream(&calls, nil, 1))
	key := x.signIn(t, "a@b.co")
	id := accountID(t, x, "a@b.co")
	wantWAV := "Echotype Cloud accepts WAV audio."
	for name, file := range map[string][]byte{
		"compressed bytes": make([]byte, 6_000_000),
		"mp3":              []byte("ID3\x03 fake mp3"),
		"MP3-in-WAV":       wavOf(0x55, 16000, 16, 1),
	} {
		for _, path := range []string{"/v1/audio/transcriptions", "/v1/audio/translations"} {
			w := audioReq(x, path, key, file, nil)
			if w.Code != 400 || errType(t, w) != "invalid_request_error" || !strings.Contains(w.Body.String(), wantWAV) {
				t.Fatalf("%s to %s: %d %s", name, path, w.Code, w.Body)
			}
		}
	}
	// 11 minutes of WAV (8 kHz, 8-bit): over Free's 10-minute request cap.
	if w := transcribeReq(x, key, wavOf(1, 8000, 8, 11*60), nil); w.Code != 400 {
		t.Fatalf("over the per-request cap: %d", w.Code)
	}
	// 6 minutes of WAV with 5 left this week: 402.
	x.s.db.Exec(`INSERT INTO usage (account_id, at, words, audio_ms) VALUES (?, ?, 0, ?)`, id, t0.Unix(), (freeAudioSecondsPerWeek-300)*1000)
	if w := transcribeReq(x, key, wavOf(1, 8000, 8, 6*60), nil); w.Code != 402 || errType(t, w) != "quota_exceeded" {
		t.Fatalf("over the remaining allowance: %d %s", w.Code, w.Body)
	}
	if calls.Load() != 0 {
		t.Fatalf("upstream called %d times for refused requests", calls.Load())
	}
	if w := transcribeReq(x, key, wavFile(3), nil); w.Code != 200 || calls.Load() != 1 {
		t.Fatalf("short WAV: %d", w.Code)
	}
}

func TestConcurrentRequestsCannotJointlyOvershoot(t *testing.T) {
	var calls atomic.Int32
	gate := make(chan struct{})
	x := newHarness(t, countingUpstream(&calls, gate, 300))
	key := x.signIn(t, "a@b.co")
	id := accountID(t, x, "a@b.co")
	// 10 minutes of audio left this week.
	x.s.db.Exec(`INSERT INTO usage (account_id, at, words, audio_ms) VALUES (?, ?, 0, ?)`, id, t0.Unix(), (freeAudioSecondsPerWeek-600)*1000)

	// Three 5-minute WAVs, all in flight at once.
	five := wavOf(1, 8000, 8, 300)
	codes := make(chan int, 3)
	for i := 0; i < 3; i++ {
		go func() { codes <- transcribeReq(x, key, five, nil).Code }()
	}
	refused := <-codes // only the one that does not fit can finish while the upstream is held
	close(gate)
	got := map[int]int{refused: 1}
	got[<-codes]++
	got[<-codes]++
	if got[200] != 2 || got[402] != 1 || calls.Load() != 2 {
		t.Fatalf("codes %v, upstream calls %d", got, calls.Load())
	}
	var ms int64
	x.s.db.QueryRow(`SELECT SUM(audio_ms) FROM usage WHERE account_id = ?`, id).Scan(&ms)
	if ms != freeAudioSecondsPerWeek*1000 {
		t.Fatalf("audio used %d ms, allowance %d", ms, freeAudioSecondsPerWeek*1000)
	}
}

func TestPerKeyLimits(t *testing.T) {
	x := newHarness(t, nil)
	var releases []func()
	for i := 0; i < maxInflight; i++ {
		r, ok := x.s.acquire(1)
		if !ok {
			t.Fatal("slot refused early")
		}
		releases = append(releases, r)
	}
	if _, ok := x.s.acquire(1); ok {
		t.Fatal("4th in-flight request allowed")
	}
	if _, ok := x.s.acquire(2); !ok {
		t.Fatal("another account blocked")
	}
	releases[0]()
	if _, ok := x.s.acquire(1); !ok {
		t.Fatal("slot not released")
	}

	y := newHarness(t, fakeUpstream(t, "x", 1))
	key := y.signIn(t, "a@b.co")
	var last int
	for i := 0; i < 61; i++ {
		last = y.json("POST", "/v1/chat/completions", `{"messages":[{"role":"user","content":"x"}]}`, bearer(key)...).Code
	}
	if last != 429 {
		t.Fatalf("61st request in a minute: %d", last)
	}
}

// ---------- chat proxy ----------

func TestChatProxyCaps(t *testing.T) {
	x := newHarness(t, fakeUpstream(t, "", 0))
	key := x.signIn(t, "a@b.co")
	w := x.json("POST", "/v1/chat/completions", `{"model":"gpt-5","stream":true,"tools":[{"type":"function"}],"n":4,
		"max_tokens":999999,"temperature":0,"messages":[{"role":"user","content":"um so hi"}]}`, bearer(key)...)
	if w.Code != 200 || !strings.Contains(w.Body.String(), "Clean.") {
		t.Fatalf("%d %s", w.Code, w.Body)
	}
	b := lastSeen.body
	if lastSeen.auth != "Bearer server-llm-key" || b["model"] != "openai/gpt-oss-20b" || b["stream"] != false ||
		b["max_tokens"] != float64(2048) || b["tools"] != nil || b["n"] != nil || b["temperature"] != float64(0) {
		t.Fatalf("upstream saw %v (auth %q)", b, lastSeen.auth)
	}
	x.json("POST", "/v1/chat/completions", `{"max_completion_tokens":300,"messages":[{"role":"user","content":"x"}]}`, bearer(key)...)
	if lastSeen.body["max_tokens"] != float64(300) || lastSeen.body["max_completion_tokens"] != nil {
		t.Fatalf("smaller cap not kept: %v", lastSeen.body)
	}
	big := `{"messages":[{"role":"user","content":"` + strings.Repeat("a", 16<<10) + `"}]}`
	if w := x.json("POST", "/v1/chat/completions", big, bearer(key)...); w.Code != 413 {
		t.Fatalf("16 KB+ body: %d", w.Code)
	}
	if w := x.json("POST", "/v1/chat/completions", `{"messages":[]}`, bearer(key)...); w.Code != 400 {
		t.Fatalf("empty messages: %d", w.Code)
	}
	if w := x.json("POST", "/v1/chat/completions", `{"messages":[{"role":"user","content":"x"}]}`); w.Code != 401 {
		t.Fatalf("no key: %d", w.Code)
	}
	models := decode(t, x.do("GET", "/v1/models", nil, bearer(key)...))
	if d, _ := models["data"].([]any); len(d) != 3 {
		t.Fatalf("models: %v", models)
	}
}

func TestChatTokensMeteredAndCapped(t *testing.T) {
	withUsage := true
	x := newHarness(t, func(w http.ResponseWriter, r *http.Request) {
		io.Copy(io.Discard, r.Body)
		if withUsage {
			io.WriteString(w, `{"choices":[{"message":{"content":"ok"}}],"usage":{"prompt_tokens":120,"completion_tokens":30,"total_tokens":150}}`)
		} else {
			io.WriteString(w, `{"choices":[{"message":{"content":"ok"}}]}`)
		}
	})
	key := x.signIn(t, "a@b.co")
	id := accountID(t, x, "a@b.co")
	msg := `{"messages":[{"role":"user","content":"um so hi"}]}`
	if w := x.json("POST", "/v1/chat/completions", msg, bearer(key)...); w.Code != 200 {
		t.Fatalf("chat: %d", w.Code)
	}
	me := decode(t, x.do("GET", "/v1/me", nil, bearer(key)...))
	if me["tokens_used"] != float64(150) || me["tokens_limit"] != float64(100_000) {
		t.Fatalf("metered from upstream usage: %v", me)
	}
	// Without usage in the reply: bytes/3 of the forwarded body + the max_tokens cap.
	withUsage = false
	x.json("POST", "/v1/chat/completions", msg, bearer(key)...)
	var last int64
	x.s.db.QueryRow(`SELECT tokens FROM usage WHERE account_id = ? ORDER BY rowid DESC LIMIT 1`, id).Scan(&last)
	if last < 2048 || last > 2048+100 {
		t.Fatalf("estimated tokens = %d", last)
	}
	// Budget spent: 402, and the upstream is not called.
	x.s.db.Exec(`INSERT INTO usage (account_id, at, words, audio_ms, tokens) VALUES (?, ?, 0, 0, 100000)`, id, t0.Unix())
	w := x.json("POST", "/v1/chat/completions", msg, bearer(key)...)
	if w.Code != 402 || !strings.Contains(w.Body.String(), "cleanup tokens") {
		t.Fatalf("over token budget: %d %s", w.Code, w.Body)
	}
	// Transcription is still allowed: the token budget only gates cleanup.
	if w := transcribeReq(x, key, wavFile(1), nil); w.Code != 200 {
		t.Fatalf("transcribe with tokens spent: %d %s", w.Code, w.Body)
	}
	// Caps: 16 KB body, 8 messages.
	big := `{"messages":[{"role":"user","content":"` + strings.Repeat("a", 16<<10) + `"}]}`
	if w := x.json("POST", "/v1/chat/completions", big, bearer(key)...); w.Code != 413 {
		t.Fatalf("16 KB+ body: %d", w.Code)
	}
	nine := `{"messages":[` + strings.TrimSuffix(strings.Repeat(`{"role":"user","content":"x"},`, 9), ",") + `]}`
	if w := x.json("POST", "/v1/chat/completions", nine, bearer(key)...); w.Code != 400 {
		t.Fatalf("9 messages: %d", w.Code)
	}
	// Pro: 4M a month.
	x.s.db.Exec(`UPDATE accounts SET plan = 'pro' WHERE id = ?`, id)
	if w := x.json("POST", "/v1/chat/completions", msg, bearer(key)...); w.Code != 200 {
		t.Fatalf("pro chat: %d %s", w.Code, w.Body)
	}
	if me := decode(t, x.do("GET", "/v1/me", nil, bearer(key)...)); me["tokens_limit"] != float64(4_000_000) {
		t.Fatalf("pro me: %v", me)
	}
}

func TestSummaryIsProOnlyWithTheServersPrompt(t *testing.T) {
	x := newHarness(t, func(w http.ResponseWriter, r *http.Request) {
		json.NewDecoder(r.Body).Decode(&lastSeen.body)
		io.WriteString(w, `{"choices":[{"message":{"content":"  ## Summary\nShipped it.  "}}],"usage":{"prompt_tokens":900,"completion_tokens":100}}`)
	})
	key := x.signIn(t, "a@b.co")
	id := accountID(t, x, "a@b.co")
	body := `{"transcript":"You: we ship friday\nSpeaker 1: agreed"}`
	if w := x.json("POST", "/v1/summary", body, bearer(key)...); w.Code != 402 || errType(t, w) != "pro_required" {
		t.Fatalf("free: %d %s", w.Code, w.Body)
	}
	if w := x.json("POST", "/v1/summary", body); w.Code != 401 {
		t.Fatalf("no key: %d", w.Code)
	}
	x.s.db.Exec(`UPDATE accounts SET plan = 'pro' WHERE id = ?`, id)
	w := x.json("POST", "/v1/summary", body, bearer(key)...)
	if w.Code != 200 || decode(t, w)["summary"] != "## Summary\nShipped it." {
		t.Fatalf("pro: %d %s", w.Code, w.Body)
	}
	msgs, _ := lastSeen.body["messages"].([]any)
	sys, _ := msgs[0].(map[string]any)
	if len(msgs) != 2 || !strings.HasPrefix(sys["content"].(string), "You write meeting notes") || lastSeen.body["max_tokens"] != float64(4096) {
		t.Fatalf("upstream got %v", lastSeen.body)
	}
	if me := decode(t, x.do("GET", "/v1/me", nil, bearer(key)...)); me["tokens_used"] != float64(1000) {
		t.Fatalf("metered: %v", me)
	}
	big := `{"transcript":"` + strings.Repeat("a", 64<<10+1) + `"}`
	if w := x.json("POST", "/v1/summary", big, bearer(key)...); w.Code != 413 {
		t.Fatalf("64 KB+: %d", w.Code)
	}
	if w := x.json("POST", "/v1/summary", `{"transcript":"  "}`, bearer(key)...); w.Code != 400 {
		t.Fatalf("empty: %d", w.Code)
	}
}

func TestAskIsProOnlyAndBounded(t *testing.T) {
	x := newHarness(t, func(w http.ResponseWriter, r *http.Request) {
		json.NewDecoder(r.Body).Decode(&lastSeen.body)
		io.WriteString(w, `{"choices":[{"message":{"content":"Priya will send the deck."}}]}`)
	})
	key := x.signIn(t, "a@b.co")
	body := `{"transcript":"Priya: I'll send the deck","question":"Who sends the deck?"}`
	if w := x.json("POST", "/v1/ask", body, bearer(key)...); w.Code != 402 || errType(t, w) != "pro_required" {
		t.Fatalf("free: %d %s", w.Code, w.Body)
	}
	x.s.db.Exec(`UPDATE accounts SET plan = 'pro' WHERE id = ?`, accountID(t, x, "a@b.co"))
	w := x.json("POST", "/v1/ask", body, bearer(key)...)
	if w.Code != 200 || decode(t, w)["answer"] != "Priya will send the deck." {
		t.Fatalf("pro: %d %s", w.Code, w.Body)
	}
	msgs, _ := lastSeen.body["messages"].([]any)
	sys, _ := msgs[0].(map[string]any)
	user, _ := msgs[1].(map[string]any)
	if !strings.HasPrefix(sys["content"].(string), "You answer questions about a meeting") || !strings.HasSuffix(user["content"].(string), "Question: Who sends the deck?") {
		t.Fatalf("upstream got %v", lastSeen.body)
	}
	long := `{"transcript":"x","question":"` + strings.Repeat("a", 501) + `"}`
	if w := x.json("POST", "/v1/ask", long, bearer(key)...); w.Code != 400 {
		t.Fatalf("long question: %d", w.Code)
	}
}

// ---------- Stripe ----------

func sign(payload []byte, secret string, ts int64) string {
	mac := hmac.New(sha256.New, []byte(secret))
	fmt.Fprintf(mac, "%d.", ts)
	mac.Write(payload)
	return hex.EncodeToString(mac.Sum(nil))
}

func TestVerifyStripeSignature(t *testing.T) {
	p := []byte(`{"id":"evt_1"}`)
	ts := t0.Unix()
	good := sign(p, "whsec_test", ts)
	cases := []struct {
		name, header string
		payload      []byte
		ok           bool
	}{
		{"valid", fmt.Sprintf("t=%d,v1=%s", ts, good), p, true},
		{"valid with v0 and spaces", fmt.Sprintf("t=%d, v0=abc, v1=%s", ts, good), p, true},
		{"second of two v1", fmt.Sprintf("t=%d,v1=%s,v1=%s", ts, sign(p, "old_secret", ts), good), p, true},
		{"tampered payload", fmt.Sprintf("t=%d,v1=%s", ts, good), []byte(`{"id":"evt_2"}`), false},
		{"wrong secret", fmt.Sprintf("t=%d,v1=%s", ts, sign(p, "other", ts)), p, false},
		{"timestamp swapped", fmt.Sprintf("t=%d,v1=%s", ts+1, good), p, false},
		{"expired", fmt.Sprintf("t=%d,v1=%s", ts-301, sign(p, "whsec_test", ts-301)), p, false},
		{"from the future", fmt.Sprintf("t=%d,v1=%s", ts+301, sign(p, "whsec_test", ts+301)), p, false},
		{"no v1", fmt.Sprintf("t=%d,v0=%s", ts, good), p, false},
		{"no timestamp", "v1=" + good, p, false},
		{"empty", "", p, false},
	}
	for _, c := range cases {
		if err := verifyStripeSignature(c.payload, c.header, "whsec_test", t0); (err == nil) != c.ok {
			t.Errorf("%s: err = %v", c.name, err)
		}
	}
}

func (x *harness) webhook(t *testing.T, event string) int {
	t.Helper()
	p := []byte(event)
	return x.do("POST", "/webhooks/stripe", bytes.NewReader(p), "Stripe-Signature", fmt.Sprintf("t=%d,v1=%s", x.now.Unix(), sign(p, "whsec_test", x.now.Unix()))).Code
}

func plan(t *testing.T, x *harness, key string) string {
	return decode(t, x.do("GET", "/v1/me", nil, bearer(key)...))["plan"].(string)
}

func TestStripeWebhookPlanTransitions(t *testing.T) {
	x := newHarness(t, nil)
	key := x.signIn(t, "a@b.co")
	id := accountID(t, x, "a@b.co")
	completed := fmt.Sprintf(`{"id":"evt_1","type":"checkout.session.completed","data":{"object":{"id":"cs_1","mode":"subscription",
		"client_reference_id":"%d","customer":"cus_1","subscription":"sub_1"}}}`, id)

	if c := x.do("POST", "/webhooks/stripe", strings.NewReader(completed), "Stripe-Signature", "t=1,v1=00").Code; c != 400 {
		t.Fatalf("bad signature: %d", c)
	}
	if plan(t, x, key) != "free" {
		t.Fatal("unsigned event changed the plan")
	}
	if c := x.webhook(t, completed); c != 200 || plan(t, x, key) != "pro" {
		t.Fatalf("checkout completed: %d %s", c, plan(t, x, key))
	}
	sub := func(evt, typ, subID, status string) string {
		return fmt.Sprintf(`{"id":%q,"type":%q,"data":{"object":{"id":%q,"customer":"cus_1","status":%q}}}`, evt, typ, subID, status)
	}
	steps := []struct {
		event, want string
	}{
		{sub("evt_2", "customer.subscription.updated", "sub_1", "past_due"), "free"},
		{sub("evt_3", "customer.subscription.updated", "sub_1", "active"), "pro"},
		{sub("evt_4", "customer.subscription.deleted", "sub_other", "canceled"), "pro"}, // not this account's subscription
		{sub("evt_5", "customer.subscription.updated", "sub_1", "trialing"), "pro"},
		{sub("evt_6", "customer.subscription.deleted", "sub_1", "canceled"), "free"},
		{completed, "free"}, // replay of evt_1: already processed, so no re-upgrade
		{`{"id":"evt_7","type":"invoice.paid","data":{"object":{}}}`, "free"},
	}
	for i, s := range steps {
		if c := x.webhook(t, s.event); c != 200 {
			t.Fatalf("step %d: status %d", i, c)
		}
		if got := plan(t, x, key); got != s.want {
			t.Fatalf("step %d: plan %s, want %s", i, got, s.want)
		}
	}
	var n int
	x.s.db.QueryRow(`SELECT COUNT(*) FROM stripe_events`).Scan(&n)
	if n != 7 {
		t.Fatalf("processed events = %d", n)
	}
	// Checkout for a different account id cannot touch this one; a non-subscription session is ignored.
	x.webhook(t, `{"id":"evt_8","type":"checkout.session.completed","data":{"object":{"mode":"payment","client_reference_id":"`+fmt.Sprint(id)+`","customer":"cus_1","subscription":"sub_9"}}}`)
	if plan(t, x, key) != "free" {
		t.Fatal("payment-mode session upgraded the plan")
	}
}

func TestCheckoutAndPortal(t *testing.T) {
	var form map[string][]string
	var auth string
	stripe := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		r.ParseForm()
		form, auth = r.PostForm, r.Header.Get("Authorization")
		io.WriteString(w, `{"id":"cs_test","url":"https://checkout.stripe.test/c/1"}`)
	}))
	defer stripe.Close()
	x := newHarness(t, nil)
	x.s.cfg.StripeKey, x.s.cfg.PriceMonthly, x.s.cfg.PriceYearly = "sk_test_x", "price_m", "price_y"
	x.s.stripeAPI = stripe.URL
	key := x.signIn(t, "a@b.co")
	id := accountID(t, x, "a@b.co")

	if w := x.json("POST", "/v1/billing/checkout", `{"plan":"pro_lifetime"}`, bearer(key)...); w.Code != 400 {
		t.Fatalf("unknown plan: %d", w.Code)
	}
	if w := x.json("POST", "/v1/billing/portal", ``, bearer(key)...); w.Code != 409 {
		t.Fatalf("portal without customer: %d", w.Code)
	}
	w := x.json("POST", "/v1/billing/checkout", `{"plan":"pro_yearly"}`, bearer(key)...)
	if w.Code != 200 || decode(t, w)["url"] != "https://checkout.stripe.test/c/1" {
		t.Fatalf("checkout: %d %s", w.Code, w.Body)
	}
	if auth != "Bearer sk_test_x" || form["mode"][0] != "subscription" || form["line_items[0][price]"][0] != "price_y" ||
		form["client_reference_id"][0] != fmt.Sprint(id) || form["customer_email"][0] != "a@b.co" ||
		form["success_url"][0] != "https://cloud.test/billing/done" {
		t.Fatalf("stripe saw %v", form)
	}
	x.s.db.Exec(`UPDATE accounts SET stripe_customer = 'cus_1', plan = 'pro' WHERE id = ?`, id)
	if w := x.json("POST", "/v1/billing/checkout", `{"plan":"pro_monthly"}`, bearer(key)...); w.Code != 409 {
		t.Fatalf("checkout while pro: %d", w.Code)
	}
	if w := x.json("POST", "/v1/billing/portal", ``, bearer(key)...); w.Code != 200 || form["customer"][0] != "cus_1" {
		t.Fatalf("portal: %d %v", w.Code, form)
	}
}

// ---------- pages ----------

func TestPages(t *testing.T) {
	x := newHarness(t, nil)
	for path, want := range map[string]string{
		"/":             "$6 a month, or $60 a year",
		"/terms":        "Terms of Service",
		"/privacy":      "We do not store audio or transcripts",
		"/billing/done": "Payment received",
		"/style.css":    "--accent",
		"/healthz":      "ok",
	} {
		w := x.do("GET", path, nil)
		if w.Code != 200 || !strings.Contains(w.Body.String(), want) {
			t.Errorf("%s: %d, missing %q", path, w.Code, want)
		}
	}
	if w := x.do("GET", "/nope", nil); w.Code != 404 {
		t.Errorf("/nope: %d", w.Code)
	}
	if !strings.Contains(x.do("GET", "/", nil).Body.String(), "2,000 words a week") {
		t.Error("free tier missing from landing page")
	}
}

// ---------- telemetry ----------

const batch = `{"install_id":"0123456789abcdef0123456789abcdef","version":"0.1.0","os":"macos","arch":"aarch64",
	"days":[{"date":"2026-10-11","dictations":3,"words":42,"failures":{"no_speech":1},"speech_ms_median":600,"cleanup_ms_median":null,
		"speech_provider":"local","speech_model":"tiny.en","cleanup_provider":"off","cleanup_model":"","meetings_started":1,"meetings_finished":1}],
	"crashes":[{"version":"0.1.0","os":"macos","arch":"aarch64","message":"boom at src/lib.rs:1","backtrace":"0: echotype::run"}]}`

func TestTelemetryStoresABatch(t *testing.T) {
	x := newHarness(t, nil)
	for range 2 { // a resend after a lost response replaces the day
		if w := x.json("POST", "/v1/telemetry", batch); w.Code != 204 {
			t.Fatalf("%d %s", w.Code, w.Body)
		}
	}
	var days, crashes int
	var data string
	x.s.db.QueryRow(`SELECT COUNT(*), MAX(data) FROM telemetry_days WHERE install_id = '0123456789abcdef0123456789abcdef' AND date = '2026-10-11'`).Scan(&days, &data)
	x.s.db.QueryRow(`SELECT COUNT(*) FROM telemetry_crashes`).Scan(&crashes)
	if days != 1 || crashes != 2 || !strings.Contains(data, `"words":42`) {
		t.Fatalf("days %d crashes %d data %s", days, crashes, data)
	}
}

func TestTelemetryRejectsBadBatches(t *testing.T) {
	x := newHarness(t, nil)
	for name, body := range map[string]string{
		"not json":      `{"install_id":`,
		"unknown field": strings.Replace(batch, `"words":42`, `"words":42,"transcript":"hello"`, 1),
		"bad id":        strings.Replace(batch, "0123456789abcdef0123456789abcdef", "someone@example.com", 1),
		"bad date":      strings.Replace(batch, "2026-10-11", "yesterday", 1),
	} {
		if w := x.json("POST", "/v1/telemetry", body); w.Code != 400 {
			t.Errorf("%s: %d %s", name, w.Code, w.Body)
		}
	}
	big := strings.Replace(batch, "echotype::run", strings.Repeat("x", telemetryLimit), 1)
	if w := x.json("POST", "/v1/telemetry", big); w.Code != 413 {
		t.Errorf("oversized: %d", w.Code)
	}
	var n int
	x.s.db.QueryRow(`SELECT (SELECT COUNT(*) FROM telemetry_days) + (SELECT COUNT(*) FROM telemetry_crashes)`).Scan(&n)
	if n != 0 {
		t.Fatalf("%d rows stored from bad batches", n)
	}
}

func TestTelemetryRateLimitPerIP(t *testing.T) {
	x := newHarness(t, nil)
	for i := range 30 {
		if w := x.json("POST", "/v1/telemetry", batch); w.Code != 204 {
			t.Fatalf("request %d: %d", i, w.Code)
		}
	}
	if w := x.json("POST", "/v1/telemetry", batch); w.Code != 429 {
		t.Fatalf("31st: %d", w.Code)
	}
	x.now = x.now.Add(time.Hour + time.Second)
	if w := x.json("POST", "/v1/telemetry", batch); w.Code != 204 {
		t.Fatalf("after an hour: %d", w.Code)
	}
}
