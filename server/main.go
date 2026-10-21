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
//	TRANSLATE_MODEL        model for /v1/audio/translations (default whisper-large-v3; Groq's turbo
//	                       model cannot translate)
//	LLM_UPSTREAM_URL       OpenAI-compatible chat base URL (default STT_UPSTREAM_URL)
//	LLM_UPSTREAM_KEY       bearer key for the chat upstream (default STT_UPSTREAM_KEY)
//	LLM_MODEL              model sent upstream for cleanup (default openai/gpt-oss-20b)
//	STRIPE_SECRET_KEY      Stripe secret key; billing routes answer 503 when empty
//	STRIPE_WEBHOOK_SECRET  signing secret of the /webhooks/stripe endpoint (whsec_...)
//	STRIPE_PRICE_MONTHLY   Stripe price id for Pro monthly ($6/month)
//	STRIPE_PRICE_YEARLY    Stripe price id for Pro yearly ($60/year)
//	SUPABASE_URL           https://<project>.supabase.co; it signs people in (required)
//	SUPABASE_ANON_KEY      the project's anon / publishable key (public, the app ships it too)
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
	STTURL, STTKey, STTModel, TranslateModel                  string
	LLMURL, LLMKey, LLMModel                                  string
	StripeKey, StripeWebhookSecret, PriceMonthly, PriceYearly string
	SupabaseURL, SupabaseAnonKey                              string
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
		TranslateModel:      env("TRANSLATE_MODEL", "whisper-large-v3"),
		LLMModel:            env("LLM_MODEL", "openai/gpt-oss-20b"),
		StripeKey:           env("STRIPE_SECRET_KEY", ""),
		StripeWebhookSecret: env("STRIPE_WEBHOOK_SECRET", ""),
		PriceMonthly:        env("STRIPE_PRICE_MONTHLY", ""),
		PriceYearly:         env("STRIPE_PRICE_YEARLY", ""),
		SupabaseURL:         strings.TrimRight(env("SUPABASE_URL", ""), "/"),
		SupabaseAnonKey:     env("SUPABASE_ANON_KEY", ""),
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
	if cfg.SupabaseURL == "" || cfg.SupabaseAnonKey == "" {
		slog.Error("SUPABASE_URL and SUPABASE_ANON_KEY are required: nobody can sign in without them")
		os.Exit(1)
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
