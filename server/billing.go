package main

import (
	"crypto/hmac"
	"crypto/sha256"
	"database/sql"
	"encoding/hex"
	"encoding/json"
	"errors"
	"io"
	"log/slog"
	"net/http"
	"net/url"
	"strconv"
	"strings"
	"time"
)

// stripePost calls the Stripe REST API with form encoding and returns the session URL.
func (s *server) stripePost(r *http.Request, path string, form url.Values) (string, error) {
	req, _ := http.NewRequestWithContext(r.Context(), http.MethodPost, s.stripeAPI+path, strings.NewReader(form.Encode()))
	req.Header.Set("Authorization", "Bearer "+s.cfg.StripeKey)
	req.Header.Set("Content-Type", "application/x-www-form-urlencoded")
	resp, err := s.stripe.Do(req)
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()
	var out struct {
		URL   string `json:"url"`
		Error struct {
			Message string `json:"message"`
		} `json:"error"`
	}
	if err := json.NewDecoder(io.LimitReader(resp.Body, 1<<20)).Decode(&out); err != nil {
		return "", err
	}
	if resp.StatusCode != http.StatusOK || out.URL == "" {
		return "", errors.New("stripe " + path + ": " + strconv.Itoa(resp.StatusCode) + " " + out.Error.Message)
	}
	return out.URL, nil
}

func (s *server) billingReady(w http.ResponseWriter) bool {
	if s.cfg.StripeKey == "" {
		apiError(w, http.StatusServiceUnavailable, "billing_unavailable", "Billing is not set up on this server.")
		return false
	}
	return true
}

// POST /v1/billing/checkout {plan: "pro_monthly"|"pro_yearly"} -> {url}
func (s *server) checkout(w http.ResponseWriter, r *http.Request, a account) {
	var body struct {
		Plan string `json:"plan"`
	}
	if !readJSON(w, r, 1<<10, &body) || !s.billingReady(w) {
		return
	}
	price := map[string]string{"pro_monthly": s.cfg.PriceMonthly, "pro_yearly": s.cfg.PriceYearly}[body.Plan]
	if price == "" {
		apiError(w, http.StatusBadRequest, "invalid_request_error", `plan must be "pro_monthly" or "pro_yearly".`)
		return
	}
	if a.Plan == "pro" {
		apiError(w, http.StatusConflict, "already_subscribed", "You already have Pro. Manage it from the billing portal.")
		return
	}
	var customer sql.NullString
	if err := s.db.QueryRowContext(r.Context(), `SELECT stripe_customer FROM accounts WHERE id = ?`, a.ID).Scan(&customer); err != nil {
		internalError(w, "checkout customer", err)
		return
	}
	id := strconv.FormatInt(a.ID, 10)
	form := url.Values{
		"mode":                    {"subscription"},
		"line_items[0][price]":    {price},
		"line_items[0][quantity]": {"1"},
		"client_reference_id":     {id},
		"subscription_data[metadata][account_id]": {id},
		"success_url": {s.cfg.PublicURL + "/billing/done"},
		"cancel_url":  {s.cfg.PublicURL + "/"},
	}
	if customer.String != "" {
		form.Set("customer", customer.String)
	} else {
		form.Set("customer_email", a.Email)
	}
	u, err := s.stripePost(r, "/v1/checkout/sessions", form)
	if err != nil {
		slog.Error("stripe checkout", "account", a.ID, "err", err)
		apiError(w, http.StatusBadGateway, "billing_error", "Could not start checkout. Try again.")
		return
	}
	writeJSON(w, http.StatusOK, map[string]string{"url": u})
}

// POST /v1/billing/portal -> {url}
func (s *server) portal(w http.ResponseWriter, r *http.Request, a account) {
	if !s.billingReady(w) {
		return
	}
	var customer sql.NullString
	if err := s.db.QueryRowContext(r.Context(), `SELECT stripe_customer FROM accounts WHERE id = ?`, a.ID).Scan(&customer); err != nil {
		internalError(w, "portal customer", err)
		return
	}
	if customer.String == "" {
		apiError(w, http.StatusConflict, "no_subscription", "There is no subscription on this account yet.")
		return
	}
	u, err := s.stripePost(r, "/v1/billing_portal/sessions", url.Values{"customer": {customer.String}, "return_url": {s.cfg.PublicURL + "/"}})
	if err != nil {
		slog.Error("stripe portal", "account", a.ID, "err", err)
		apiError(w, http.StatusBadGateway, "billing_error", "Could not open the billing portal. Try again.")
		return
	}
	writeJSON(w, http.StatusOK, map[string]string{"url": u})
}

const webhookTolerance = 5 * time.Minute

// verifyStripeSignature checks a Stripe-Signature header ("t=...,v1=...,v1=...") against
// HMAC-SHA256(secret, "t.payload"), rejecting timestamps more than 5 minutes off.
func verifyStripeSignature(payload []byte, header, secret string, now time.Time) error {
	var ts int64 = -1
	var sigs [][]byte
	for _, kv := range strings.Split(header, ",") {
		k, v, _ := strings.Cut(strings.TrimSpace(kv), "=")
		switch k {
		case "t":
			n, err := strconv.ParseInt(v, 10, 64)
			if err != nil {
				return errors.New("bad timestamp")
			}
			ts = n
		case "v1":
			if b, err := hex.DecodeString(v); err == nil {
				sigs = append(sigs, b)
			}
		}
	}
	if ts < 0 || len(sigs) == 0 {
		return errors.New("missing timestamp or v1 signature")
	}
	if d := now.Sub(time.Unix(ts, 0)); d > webhookTolerance || d < -webhookTolerance {
		return errors.New("timestamp outside tolerance")
	}
	mac := hmac.New(sha256.New, []byte(secret))
	mac.Write([]byte(strconv.FormatInt(ts, 10) + "."))
	mac.Write(payload)
	want := mac.Sum(nil)
	for _, sig := range sigs {
		if hmac.Equal(sig, want) {
			return nil
		}
	}
	return errors.New("no matching signature")
}

// POST /webhooks/stripe. Each event id is processed once, in the same transaction as its
// effect, so a failure rolls both back and Stripe's retry gets another go.
func (s *server) stripeWebhook(w http.ResponseWriter, r *http.Request) {
	if s.cfg.StripeWebhookSecret == "" {
		apiError(w, http.StatusServiceUnavailable, "billing_unavailable", "Billing is not set up on this server.")
		return
	}
	payload, err := io.ReadAll(http.MaxBytesReader(w, r.Body, 1<<20))
	if err != nil {
		apiError(w, http.StatusRequestEntityTooLarge, "request_too_large", "Payload too large.")
		return
	}
	if err := verifyStripeSignature(payload, r.Header.Get("Stripe-Signature"), s.cfg.StripeWebhookSecret, s.now()); err != nil {
		slog.Warn("stripe webhook rejected", "err", err)
		apiError(w, http.StatusBadRequest, "invalid_signature", "Invalid signature.")
		return
	}
	var ev stripeEvent
	if err := json.Unmarshal(payload, &ev); err != nil || ev.ID == "" {
		apiError(w, http.StatusBadRequest, "invalid_request_error", "Malformed event.")
		return
	}
	if err := s.applyStripeEvent(r, ev); err != nil {
		internalError(w, "stripe webhook", err)
		return
	}
	w.WriteHeader(http.StatusOK)
}

type stripeEvent struct {
	ID   string `json:"id"`
	Type string `json:"type"`
	Data struct {
		Object struct {
			ID                string `json:"id"`
			ClientReferenceID string `json:"client_reference_id"`
			Customer          string `json:"customer"`
			Subscription      string `json:"subscription"`
			Mode              string `json:"mode"`
			Status            string `json:"status"`
		} `json:"object"`
	} `json:"data"`
}

func (s *server) applyStripeEvent(r *http.Request, ev stripeEvent) error {
	o := ev.Data.Object
	tx, err := s.db.BeginTx(r.Context(), nil)
	if err != nil {
		return err
	}
	defer tx.Rollback()
	res, err := tx.Exec(`INSERT INTO stripe_events (id, at) VALUES (?, ?) ON CONFLICT(id) DO NOTHING`, ev.ID, s.now().Unix())
	if err != nil {
		return err
	}
	if n, _ := res.RowsAffected(); n == 0 {
		return nil // already processed
	}
	switch ev.Type {
	case "checkout.session.completed":
		id, err := strconv.ParseInt(o.ClientReferenceID, 10, 64)
		if o.Mode != "subscription" || err != nil || o.Customer == "" || o.Subscription == "" {
			slog.Warn("stripe checkout event ignored", "event", ev.ID)
			break
		}
		if _, err := tx.Exec(`UPDATE accounts SET plan = 'pro', stripe_customer = ?, stripe_subscription = ? WHERE id = ?`, o.Customer, o.Subscription, id); err != nil {
			return err
		}
	case "customer.subscription.updated", "customer.subscription.deleted":
		plan := "free"
		if ev.Type == "customer.subscription.updated" && (o.Status == "active" || o.Status == "trialing") {
			plan = "pro"
		}
		// Only the subscription linked at checkout moves the plan, so a late event for an
		// older subscription cannot downgrade a current one.
		if _, err := tx.Exec(`UPDATE accounts SET plan = ? WHERE stripe_subscription = ?`, plan, o.ID); err != nil {
			return err
		}
	}
	slog.Info("stripe event", "event", ev.ID, "type", ev.Type)
	return tx.Commit()
}
