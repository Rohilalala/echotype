package main

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/mail"
	"strings"
	"time"
)

const keyPrefix = "et_live_"

// normalizeEmail lowercases and validates a bare address ("a@b.co", no display name).
func normalizeEmail(raw string) (string, bool) {
	e := strings.ToLower(strings.TrimSpace(raw))
	if len(e) > 254 || strings.ContainsAny(e, "\r\n<>\" ") {
		return "", false
	}
	addr, err := mail.ParseAddress(e)
	if err != nil || addr.Address != e || addr.Name != "" {
		return "", false
	}
	at := strings.LastIndexByte(e, '@')
	if at < 1 || !strings.Contains(e[at+1:], ".") || strings.HasSuffix(e, ".") {
		return "", false
	}
	return e, true
}

// POST /v1/auth/supabase {access_token} -> {api_key, email, plan}. The app signs in with
// Supabase (Google, Apple or an emailed code); Supabase vouches for the email here. The
// account is created on first sign-in and gets a fresh key on every one.
func (s *server) authSupabase(w http.ResponseWriter, r *http.Request) {
	var body struct {
		AccessToken string `json:"access_token"`
	}
	if !readJSON(w, r, 16<<10, &body) {
		return
	}
	if !s.limits.allow("auth-ip:"+s.clientIP(r), 30, 10*time.Minute, s.now()) {
		apiError(w, http.StatusTooManyRequests, "rate_limit_exceeded", "Too many attempts. Try again later.")
		return
	}
	email, err := s.supabaseEmail(r.Context(), body.AccessToken)
	if errors.Is(err, errBadToken) {
		apiError(w, http.StatusUnauthorized, "invalid_token", "That sign-in did not go through. Try again.")
		return
	}
	if err != nil {
		apiError(w, http.StatusBadGateway, "auth_unavailable", "Sign-in is unavailable right now. Try again in a minute.")
		return
	}
	key, plan, err := s.issueKey(r.Context(), email)
	if err != nil {
		internalError(w, "issue key", err)
		return
	}
	writeJSON(w, http.StatusOK, map[string]string{"api_key": key, "email": email, "plan": plan})
}

var errBadToken = errors.New("bad token")

// supabaseEmail asks Supabase whose access token this is (GET /auth/v1/user) and returns the
// user's confirmed email. A token Supabase rejects, or a user without a confirmed email, is errBadToken.
func (s *server) supabaseEmail(ctx context.Context, token string) (string, error) {
	if strings.TrimSpace(token) == "" {
		return "", errBadToken
	}
	req, err := http.NewRequestWithContext(ctx, "GET", s.cfg.SupabaseURL+"/auth/v1/user", nil)
	if err != nil {
		return "", err
	}
	req.Header.Set("apikey", s.cfg.SupabaseAnonKey)
	req.Header.Set("Authorization", "Bearer "+token)
	resp, err := s.supabase.Do(req)
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()
	switch {
	case resp.StatusCode == http.StatusUnauthorized || resp.StatusCode == http.StatusForbidden:
		return "", errBadToken
	case resp.StatusCode != http.StatusOK:
		return "", fmt.Errorf("supabase user: %d", resp.StatusCode)
	}
	var u struct {
		Email            string  `json:"email"`
		EmailConfirmedAt *string `json:"email_confirmed_at"`
	}
	if err := json.NewDecoder(io.LimitReader(resp.Body, 1<<20)).Decode(&u); err != nil {
		return "", err
	}
	email, ok := normalizeEmail(u.Email)
	if !ok || u.EmailConfirmedAt == nil || *u.EmailConfirmedAt == "" {
		return "", errBadToken
	}
	return email, nil
}

// issueKey creates the account on first sign-in and returns a fresh API key for it.
func (s *server) issueKey(ctx context.Context, email string) (key, plan string, err error) {
	tx, err := s.db.BeginTx(ctx, nil)
	if err != nil {
		return "", "", err
	}
	defer tx.Rollback()
	now := s.now()
	var id int64
	err = tx.QueryRowContext(ctx, `INSERT INTO accounts (email, created_at) VALUES (?, ?)
		ON CONFLICT(email) DO UPDATE SET email = excluded.email RETURNING id, plan`,
		email, now.Unix()).Scan(&id, &plan)
	if err != nil {
		return "", "", err
	}
	raw := make([]byte, 32)
	rand.Read(raw)
	key = keyPrefix + hex.EncodeToString(raw)
	if _, err = tx.ExecContext(ctx, `INSERT INTO api_keys (hash, account_id, created_at) VALUES (?, ?, ?)`, hashSecret(key), id, now.Unix()); err != nil {
		return "", "", err
	}
	return key, plan, tx.Commit()
}
