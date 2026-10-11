package main

import (
	"crypto/rand"
	"crypto/subtle"
	"crypto/tls"
	"database/sql"
	"encoding/hex"
	"errors"
	"fmt"
	"log/slog"
	"math/big"
	"net"
	"net/http"
	"net/mail"
	"net/smtp"
	"strings"
	"time"
)

const (
	keyPrefix    = "et_live_"
	codeTTL      = 10 * time.Minute
	maxCodeTries = 5
)

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

func codeHash(email, code string) []byte { return hashSecret(email + "\x00" + code) }

// POST /v1/auth/start {email} -> 204. The answer is the same whether or not the email has
// an account, so the endpoint cannot be used to discover who signed up.
func (s *server) authStart(w http.ResponseWriter, r *http.Request) {
	var body struct {
		Email string `json:"email"`
	}
	if !readJSON(w, r, 1<<10, &body) {
		return
	}
	email, ok := normalizeEmail(body.Email)
	if !ok {
		apiError(w, http.StatusBadRequest, "invalid_request_error", "Enter a valid email address.")
		return
	}
	now := s.now()
	if !s.limits.allow("start-ip:"+s.clientIP(r), 20, time.Hour, now) || !s.limits.allow("start-email:"+email, 5, time.Hour, now) {
		apiError(w, http.StatusTooManyRequests, "rate_limit_exceeded", "Too many sign-in codes requested. Try again later.")
		return
	}
	n, err := rand.Int(rand.Reader, big.NewInt(1_000_000))
	if err != nil {
		internalError(w, "code rand", err)
		return
	}
	code := fmt.Sprintf("%06d", n.Int64())
	_, err = s.db.ExecContext(r.Context(),
		`INSERT INTO login_codes (email, hash, expires_at, attempts) VALUES (?, ?, ?, 0)
		 ON CONFLICT(email) DO UPDATE SET hash = excluded.hash, expires_at = excluded.expires_at, attempts = 0`,
		email, codeHash(email, code), now.Add(codeTTL).Unix())
	if err != nil {
		internalError(w, "store code", err)
		return
	}
	if err := s.sendCode(email, code); err != nil {
		slog.Error("send code", "err", err)
		apiError(w, http.StatusBadGateway, "email_failed", "We could not send the email. Try again in a minute.")
		return
	}
	w.WriteHeader(http.StatusNoContent)
}

// POST /v1/auth/verify {email, code} -> {api_key, email, plan}. Creates the account on first
// success and issues a fresh key on every sign-in.
func (s *server) authVerify(w http.ResponseWriter, r *http.Request) {
	var body struct {
		Email string `json:"email"`
		Code  string `json:"code"`
	}
	if !readJSON(w, r, 1<<10, &body) {
		return
	}
	if !s.limits.allow("verify-ip:"+s.clientIP(r), 30, 10*time.Minute, s.now()) {
		apiError(w, http.StatusTooManyRequests, "rate_limit_exceeded", "Too many attempts. Try again later.")
		return
	}
	email, ok := normalizeEmail(body.Email)
	code := strings.TrimSpace(body.Code)
	if !ok || len(code) != 6 {
		apiError(w, http.StatusUnauthorized, "invalid_code", "That code is wrong or has expired.")
		return
	}
	key, plan, err := s.redeemCode(r, email, code)
	if errors.Is(err, errBadCode) {
		apiError(w, http.StatusUnauthorized, "invalid_code", "That code is wrong or has expired.")
		return
	}
	if err != nil {
		internalError(w, "verify", err)
		return
	}
	writeJSON(w, http.StatusOK, map[string]string{"api_key": key, "email": email, "plan": plan})
}

var errBadCode = errors.New("bad code")

func (s *server) redeemCode(r *http.Request, email, code string) (key, plan string, err error) {
	tx, err := s.db.BeginTx(r.Context(), nil)
	if err != nil {
		return "", "", err
	}
	defer tx.Rollback()
	now := s.now()
	// Count the attempt before comparing, so a crash or a race cannot buy extra guesses.
	var stored []byte
	err = tx.QueryRow(`UPDATE login_codes SET attempts = attempts + 1
		WHERE email = ? AND attempts < ? AND expires_at > ? RETURNING hash`,
		email, maxCodeTries, now.Unix()).Scan(&stored)
	if errors.Is(err, sql.ErrNoRows) {
		return "", "", errBadCode
	}
	if err != nil {
		return "", "", err
	}
	if subtle.ConstantTimeCompare(stored, codeHash(email, code)) != 1 {
		return "", "", errors.Join(errBadCode, tx.Commit())
	}
	if _, err = tx.Exec(`DELETE FROM login_codes WHERE email = ?`, email); err != nil {
		return "", "", err
	}
	var id int64
	err = tx.QueryRow(`INSERT INTO accounts (email, created_at) VALUES (?, ?)
		ON CONFLICT(email) DO UPDATE SET email = excluded.email RETURNING id, plan`,
		email, now.Unix()).Scan(&id, &plan)
	if err != nil {
		return "", "", err
	}
	raw := make([]byte, 32)
	rand.Read(raw)
	key = keyPrefix + hex.EncodeToString(raw)
	if _, err = tx.Exec(`INSERT INTO api_keys (hash, account_id, created_at) VALUES (?, ?, ?)`, hashSecret(key), id, now.Unix()); err != nil {
		return "", "", err
	}
	return key, plan, tx.Commit()
}

// mailCode sends the sign-in code. Without SMTP_HOST it logs the code instead (dev mode).
func (s *server) mailCode(email, code string) error {
	c := s.cfg
	if c.SMTPHost == "" {
		slog.Warn("DEV MODE sign-in code (SMTP_HOST unset; never run like this in production)", "email", email, "code", code)
		return nil
	}
	msg := "From: " + c.SMTPFrom + "\r\n" +
		"To: " + email + "\r\n" +
		"Subject: Your Echotype sign-in code: " + code + "\r\n" +
		"Date: " + time.Now().UTC().Format(time.RFC1123Z) + "\r\n" +
		"MIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n" +
		"Your Echotype Cloud sign-in code is " + code + ".\r\n\r\n" +
		"It expires in 10 minutes. If you did not ask for it, ignore this email.\r\n"

	conn, err := net.DialTimeout("tcp", net.JoinHostPort(c.SMTPHost, c.SMTPPort), 15*time.Second)
	if err != nil {
		return err
	}
	conn.SetDeadline(time.Now().Add(30 * time.Second))
	cl, err := smtp.NewClient(conn, c.SMTPHost)
	if err != nil {
		conn.Close()
		return err
	}
	defer cl.Close()
	// STARTTLS is mandatory: codes are credentials.
	if err := cl.StartTLS(&tls.Config{ServerName: c.SMTPHost, MinVersion: tls.VersionTLS12}); err != nil {
		return fmt.Errorf("starttls: %w", err)
	}
	if c.SMTPUser != "" {
		if err := cl.Auth(smtp.PlainAuth("", c.SMTPUser, c.SMTPPass, c.SMTPHost)); err != nil {
			return fmt.Errorf("auth: %w", err)
		}
	}
	from, err := mail.ParseAddress(c.SMTPFrom)
	if err != nil {
		return fmt.Errorf("SMTP_FROM: %w", err)
	}
	if err := cl.Mail(from.Address); err != nil {
		return err
	}
	if err := cl.Rcpt(email); err != nil {
		return err
	}
	wc, err := cl.Data()
	if err != nil {
		return err
	}
	if _, err := wc.Write([]byte(msg)); err != nil {
		return err
	}
	if err := wc.Close(); err != nil {
		return err
	}
	return cl.Quit()
}
