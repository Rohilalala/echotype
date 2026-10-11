// Command server is Echotype Cloud: the optional hosted speech-to-text and cleanup
// tier for the Echotype desktop app. It is a metering proxy in front of an
// OpenAI-compatible upstream (Groq by default), with email-code sign-in, API keys,
// per-plan quotas and Stripe subscriptions. It stores no audio and no transcripts;
// only word and audio-second counts per request.
//
// Configuration (environment variables):
//
//	PORT                   listen port (default 8080)
//	DATABASE_PATH          SQLite file (default echotype-cloud.db)
//	PUBLIC_URL             public origin, used for Stripe return URLs (default http://localhost:PORT)
//	TRUST_PROXY            "1" when behind one reverse proxy: the client IP is taken from the
//	                       right-most X-Forwarded-For entry (default off: the TCP peer address)
//	STT_UPSTREAM_URL       OpenAI-compatible speech-to-text base URL (default https://api.groq.com/openai/v1)
//	STT_UPSTREAM_KEY       bearer key for the STT upstream (required)
//	STT_MODEL              model sent upstream; the client's model is ignored (default whisper-large-v3-turbo)
//	LLM_UPSTREAM_URL       OpenAI-compatible chat base URL (default STT_UPSTREAM_URL)
//	LLM_UPSTREAM_KEY       bearer key for the chat upstream (default STT_UPSTREAM_KEY)
//	LLM_MODEL              model sent upstream for cleanup (default openai/gpt-oss-20b)
//	STRIPE_SECRET_KEY      Stripe secret key; billing routes answer 503 when empty
//	STRIPE_WEBHOOK_SECRET  signing secret of the /webhooks/stripe endpoint (whsec_...)
//	STRIPE_PRICE_MONTHLY   Stripe price id for Pro monthly ($6/month)
//	STRIPE_PRICE_YEARLY    Stripe price id for Pro yearly ($60/year)
//	SMTP_HOST              mail server for sign-in codes; when empty the server runs in dev mode
//	                       and logs each code to stdout instead of emailing it
//	SMTP_PORT              default 587; STARTTLS is required
//	SMTP_USER, SMTP_PASS   SMTP credentials (PLAIN auth over TLS)
//	SMTP_FROM              From address, e.g. "Echotype <login@example.com>"
package main

import (
	"context"
	"errors"
	"log/slog"
	"net/http"
	"os"
	"os/signal"
	"strings"
	"syscall"
	"time"
)

type config struct {
	Port, DatabasePath, PublicURL                             string
	TrustProxy                                                bool
	STTURL, STTKey, STTModel, LLMURL, LLMKey, LLMModel        string
	StripeKey, StripeWebhookSecret, PriceMonthly, PriceYearly string
	SMTPHost, SMTPPort, SMTPUser, SMTPPass, SMTPFrom          string
}

func env(k, def string) string {
	if v := strings.TrimSpace(os.Getenv(k)); v != "" {
		return v
	}
	return def
}

func loadConfig() config {
	c := config{
		Port:                env("PORT", "8080"),
		DatabasePath:        env("DATABASE_PATH", "echotype-cloud.db"),
		TrustProxy:          os.Getenv("TRUST_PROXY") == "1",
		STTURL:              strings.TrimRight(env("STT_UPSTREAM_URL", "https://api.groq.com/openai/v1"), "/"),
		STTKey:              env("STT_UPSTREAM_KEY", ""),
		STTModel:            env("STT_MODEL", "whisper-large-v3-turbo"),
		LLMModel:            env("LLM_MODEL", "openai/gpt-oss-20b"),
		StripeKey:           env("STRIPE_SECRET_KEY", ""),
		StripeWebhookSecret: env("STRIPE_WEBHOOK_SECRET", ""),
		PriceMonthly:        env("STRIPE_PRICE_MONTHLY", ""),
		PriceYearly:         env("STRIPE_PRICE_YEARLY", ""),
		SMTPHost:            env("SMTP_HOST", ""),
		SMTPPort:            env("SMTP_PORT", "587"),
		SMTPUser:            env("SMTP_USER", ""),
		SMTPPass:            env("SMTP_PASS", ""),
		SMTPFrom:            env("SMTP_FROM", ""),
	}
	c.PublicURL = strings.TrimRight(env("PUBLIC_URL", "http://localhost:"+c.Port), "/")
	c.LLMURL = strings.TrimRight(env("LLM_UPSTREAM_URL", c.STTURL), "/")
	c.LLMKey = env("LLM_UPSTREAM_KEY", c.STTKey)
	return c
}

func main() {
	slog.SetDefault(slog.New(slog.NewJSONHandler(os.Stdout, nil)))
	cfg := loadConfig()
	if cfg.STTKey == "" {
		slog.Warn("STT_UPSTREAM_KEY is empty; upstream calls will be unauthenticated")
	}
	if cfg.SMTPHost == "" {
		slog.Warn("DEV MODE: SMTP_HOST is empty, sign-in codes are logged to stdout instead of emailed")
	}
	if cfg.StripeKey == "" || cfg.StripeWebhookSecret == "" {
		slog.Warn("Stripe is not fully configured; billing routes answer 503")
	}
	db, err := openDB(cfg.DatabasePath)
	if err != nil {
		slog.Error("open database", "err", err)
		os.Exit(1)
	}
	defer db.Close()

	s := newServer(cfg, db)
	srv := &http.Server{
		Addr:              ":" + cfg.Port,
		Handler:           s.routes(),
		ReadHeaderTimeout: 10 * time.Second,
		ReadTimeout:       2 * time.Minute, // a 25 MB upload on a slow link
		WriteTimeout:      3 * time.Minute, // upstream transcription included
		IdleTimeout:       2 * time.Minute,
	}
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	go func() {
		slog.Info("listening", "addr", srv.Addr, "public_url", cfg.PublicURL)
		if err := srv.ListenAndServe(); err != nil && !errors.Is(err, http.ErrServerClosed) {
			slog.Error("listen", "err", err)
			stop()
		}
	}()
	<-ctx.Done()
	slog.Info("shutting down")
	shutdownCtx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	if err := srv.Shutdown(shutdownCtx); err != nil {
		slog.Error("shutdown", "err", err)
	}
}
