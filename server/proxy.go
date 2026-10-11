package main

import (
	"bytes"
	"encoding/json"
	"errors"
	"io"
	"log/slog"
	"mime"
	"mime/multipart"
	"net/http"
	"path/filepath"
	"strings"
	"unicode"
)

const (
	maxAudioBytes  = 25 << 20
	maxChatBytes   = 64 << 10
	maxChatTokens  = 2048
	maxPromptBytes = 4 << 10
)

// GET /v1/models
func (s *server) models(w http.ResponseWriter, _ *http.Request, _ account) {
	m := func(id string) map[string]string {
		return map[string]string{"id": id, "object": "model", "owned_by": "echotype"}
	}
	writeJSON(w, http.StatusOK, map[string]any{"object": "list", "data": []any{m(s.cfg.STTModel), m(s.cfg.LLMModel)}})
}

// POST /v1/audio/transcriptions (OpenAI-compatible multipart). The server's own model and
// upstream key are used; the client's model field is ignored.
func (s *server) transcribe(w http.ResponseWriter, r *http.Request, a account) {
	release, ok := s.limitKey(w, a)
	if !ok {
		return
	}
	defer release()
	// Cheap refusal before reading 25 MB; the binding check is reserve() below.
	if s.overQuota(w, r, a) {
		return
	}

	// ponytail: the upload is buffered in memory (25 MB x 3 in flight per key); stream it
	// through an io.Pipe if memory per instance becomes the constraint.
	b := boundary(r)
	if b == "" {
		apiError(w, http.StatusBadRequest, "invalid_request_error", "Send the audio as multipart/form-data with a file field.")
		return
	}
	mr := multipart.NewReader(http.MaxBytesReader(w, r.Body, maxAudioBytes+64<<10), b)
	var audio []byte
	var filename, language, prompt, format string
	for {
		part, err := mr.NextPart()
		if err == io.EOF {
			break
		}
		if err != nil {
			multipartError(w, err)
			return
		}
		switch part.FormName() {
		case "file":
			filename = part.FileName()
			audio, err = io.ReadAll(io.LimitReader(part, maxAudioBytes+1))
			if err == nil && len(audio) > maxAudioBytes {
				err = &http.MaxBytesError{Limit: maxAudioBytes}
			}
		case "language":
			language, err = readField(part, 16)
		case "prompt":
			prompt, err = readField(part, maxPromptBytes)
		case "response_format":
			format, err = readField(part, 16)
			// model, temperature, ...: the server decides; NextPart skips them.
		}
		if err != nil {
			multipartError(w, err)
			return
		}
	}
	if len(audio) == 0 {
		apiError(w, http.StatusBadRequest, "invalid_request_error", "The file field is missing or empty.")
		return
	}
	if format != "" && format != "json" && format != "text" {
		apiError(w, http.StatusBadRequest, "invalid_request_error", "response_format must be json or text.")
		return
	}
	if language != "" && !validLanguage(language) {
		apiError(w, http.StatusBadRequest, "invalid_request_error", "language must be an ISO-639-1 code such as en.")
		return
	}

	// Rebuild the request for the upstream. verbose_json carries the audio duration.
	var buf bytes.Buffer
	mw := multipart.NewWriter(&buf)
	fw, _ := mw.CreateFormFile("file", safeFilename(filename))
	fw.Write(audio)
	mw.WriteField("model", s.cfg.STTModel)
	mw.WriteField("response_format", "verbose_json")
	if language != "" {
		mw.WriteField("language", language)
	}
	if prompt != "" {
		mw.WriteField("prompt", prompt)
	}
	mw.Close()

	// Reserve the most audio this file can hold before any upstream cost is incurred.
	bound := boundSeconds(audio)
	row, ok := s.reserve(w, r, a, int64(bound*1000))
	if !ok {
		return
	}
	settled := false
	defer func() {
		if !settled {
			s.release(row)
		}
	}()

	req, _ := http.NewRequestWithContext(r.Context(), http.MethodPost, s.cfg.STTURL+"/audio/transcriptions", &buf)
	req.Header.Set("Content-Type", mw.FormDataContentType())
	req.Header.Set("Authorization", "Bearer "+s.cfg.STTKey)
	body, ok := s.callUpstream(w, req, "stt")
	if !ok {
		return
	}
	var out struct {
		Text     string   `json:"text"`
		Duration *float64 `json:"duration"`
	}
	if err := json.Unmarshal(body, &out); err != nil {
		slog.Error("stt upstream returned non-JSON", "bytes", len(body))
		apiError(w, http.StatusBadGateway, "upstream_error", "The speech-to-text service returned an unexpected response.")
		return
	}

	// Real audio seconds: the WAV header (raised to the upstream's figure if that is larger),
	// else the upstream's duration, else the reserved bound.
	secs, isWav := wavSeconds(audio)
	switch {
	case out.Duration != nil && (!isWav || *out.Duration > secs):
		secs = *out.Duration
	case !isWav:
		secs = bound
	}
	s.settle(row, int64(len(strings.Fields(out.Text))), int64(secs*1000))
	settled = true
	if format == "text" {
		w.Header().Set("Content-Type", "text/plain; charset=utf-8")
		io.WriteString(w, out.Text)
		return
	}
	writeJSON(w, http.StatusOK, map[string]string{"text": out.Text})
}

// chatFields are the OpenAI chat parameters forwarded upstream; everything else (model,
// tools, stream, n, ...) is dropped.
var chatFields = map[string]bool{
	"messages": true, "temperature": true, "top_p": true, "stop": true, "seed": true,
	"max_tokens": true, "max_completion_tokens": true, "response_format": true, "reasoning_effort": true,
}

// POST /v1/chat/completions: transcript cleanup. Not metered, but only for accounts that may
// still transcribe.
func (s *server) chat(w http.ResponseWriter, r *http.Request, a account) {
	release, ok := s.limitKey(w, a)
	if !ok {
		return
	}
	defer release()
	if s.overQuota(w, r, a) {
		return
	}
	raw, err := io.ReadAll(http.MaxBytesReader(w, r.Body, maxChatBytes))
	if err != nil {
		if tooLarge(err) {
			apiError(w, http.StatusRequestEntityTooLarge, "request_too_large", "Chat requests are limited to 64 KB.")
		} else {
			apiError(w, http.StatusBadRequest, "invalid_request_error", "Could not read the request body.")
		}
		return
	}
	var in map[string]json.RawMessage
	if err := json.Unmarshal(raw, &in); err != nil {
		apiError(w, http.StatusBadRequest, "invalid_request_error", "Request body must be a JSON object.")
		return
	}
	var msgs []json.RawMessage
	if json.Unmarshal(in["messages"], &msgs) != nil || len(msgs) == 0 {
		apiError(w, http.StatusBadRequest, "invalid_request_error", "messages must be a non-empty array.")
		return
	}
	out := map[string]any{}
	for k, v := range in {
		if chatFields[k] {
			out[k] = v
		}
	}
	limit := maxChatTokens
	for _, k := range []string{"max_tokens", "max_completion_tokens"} {
		var n int
		if v, ok := in[k]; ok && json.Unmarshal(v, &n) == nil && n > 0 && n < limit {
			limit = n
		}
		delete(out, k)
	}
	out["max_tokens"] = limit
	out["model"] = s.cfg.LLMModel
	out["stream"] = false
	payload, _ := json.Marshal(out)

	req, _ := http.NewRequestWithContext(r.Context(), http.MethodPost, s.cfg.LLMURL+"/chat/completions", bytes.NewReader(payload))
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Authorization", "Bearer "+s.cfg.LLMKey)
	body, ok := s.callUpstream(w, req, "llm")
	if !ok {
		return
	}
	w.Header().Set("Content-Type", "application/json")
	w.Write(body)
}

// callUpstream sends req and returns the 2xx body. Otherwise it answers the client: the
// upstream's own 400 message passes through (it describes the client's request), 429 stays
// 429, and anything else becomes a 502 so an upstream auth problem never looks like the
// client's key being wrong.
func (s *server) callUpstream(w http.ResponseWriter, req *http.Request, name string) ([]byte, bool) {
	resp, err := s.upstream.Do(req)
	if err != nil {
		slog.Error("upstream request", "upstream", name, "err", err)
		apiError(w, http.StatusBadGateway, "upstream_error", "The upstream service did not respond. Try again.")
		return nil, false
	}
	defer resp.Body.Close()
	body, err := io.ReadAll(io.LimitReader(resp.Body, 4<<20))
	if err != nil {
		apiError(w, http.StatusBadGateway, "upstream_error", "The upstream service did not respond. Try again.")
		return nil, false
	}
	switch {
	case resp.StatusCode >= 200 && resp.StatusCode < 300:
		return body, true
	case resp.StatusCode == http.StatusBadRequest:
		var e struct {
			Error struct {
				Message string `json:"message"`
			} `json:"error"`
		}
		json.Unmarshal(body, &e)
		msg := e.Error.Message
		if msg == "" {
			msg = "The upstream service rejected the request."
		}
		apiError(w, http.StatusBadRequest, "invalid_request_error", msg)
	case resp.StatusCode == http.StatusTooManyRequests:
		apiError(w, http.StatusTooManyRequests, "rate_limit_exceeded", "The service is busy. Try again in a few seconds.")
	default:
		slog.Error("upstream status", "upstream", name, "status", resp.StatusCode)
		apiError(w, http.StatusBadGateway, "upstream_error", "The upstream service failed. Try again.")
	}
	return nil, false
}

func boundary(r *http.Request) string {
	mt, params, err := mime.ParseMediaType(r.Header.Get("Content-Type"))
	if err != nil || mt != "multipart/form-data" {
		return ""
	}
	return params["boundary"]
}

func readField(p *multipart.Part, max int) (string, error) {
	b, err := io.ReadAll(io.LimitReader(p, int64(max)+1))
	if err == nil && len(b) > max {
		err = errFieldTooLong
	}
	return strings.TrimSpace(string(b)), err
}

var errFieldTooLong = errors.New("field too long")

func multipartError(w http.ResponseWriter, err error) {
	switch {
	case tooLarge(err):
		apiError(w, http.StatusRequestEntityTooLarge, "request_too_large", "Audio files are limited to 25 MB.")
	case errors.Is(err, errFieldTooLong):
		apiError(w, http.StatusBadRequest, "invalid_request_error", "A form field is too long.")
	default:
		apiError(w, http.StatusBadRequest, "invalid_request_error", "Malformed multipart body.")
	}
}

func validLanguage(s string) bool {
	for _, r := range s {
		if r > unicode.MaxASCII || !(unicode.IsLetter(r) || r == '-') {
			return false
		}
	}
	return len(s) >= 2
}

// safeFilename keeps the extension (the upstream sniffs the format from it) and nothing else
// of the client's name.
func safeFilename(name string) string {
	ext := strings.ToLower(filepath.Ext(name))
	if len(ext) < 2 || len(ext) > 6 || strings.IndexFunc(ext[1:], func(r rune) bool { return !unicode.IsLetter(r) && !unicode.IsDigit(r) }) >= 0 {
		ext = ".wav"
	}
	return "audio" + ext
}
