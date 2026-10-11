package main

import (
	"database/sql"

	_ "modernc.org/sqlite"
)

const schema = `
CREATE TABLE IF NOT EXISTS accounts (
	id                  INTEGER PRIMARY KEY,
	email               TEXT NOT NULL UNIQUE,
	plan                TEXT NOT NULL DEFAULT 'free' CHECK (plan IN ('free', 'pro')),
	stripe_customer     TEXT,
	stripe_subscription TEXT,
	created_at          INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS accounts_customer ON accounts(stripe_customer);
CREATE INDEX IF NOT EXISTS accounts_subscription ON accounts(stripe_subscription);

-- Only SHA-256 hashes of keys and codes are stored.
CREATE TABLE IF NOT EXISTS api_keys (
	hash       BLOB PRIMARY KEY,
	account_id INTEGER NOT NULL REFERENCES accounts(id),
	created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS login_codes (
	email      TEXT PRIMARY KEY,
	hash       BLOB NOT NULL,
	expires_at INTEGER NOT NULL,
	attempts   INTEGER NOT NULL DEFAULT 0
);

-- Failed sign-in code checks, counted per email across codes (brute-force cap).
CREATE TABLE IF NOT EXISTS auth_failures (
	email TEXT NOT NULL,
	at    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS auth_failures_email_at ON auth_failures(email, at);

-- One row per transcription: counts only, never text or audio.
CREATE TABLE IF NOT EXISTS usage (
	account_id INTEGER NOT NULL REFERENCES accounts(id),
	at         INTEGER NOT NULL,
	words      INTEGER NOT NULL,
	audio_ms   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS usage_account_at ON usage(account_id, at);

CREATE TABLE IF NOT EXISTS stripe_events (
	id TEXT PRIMARY KEY,
	at INTEGER NOT NULL
);
`

// openDB opens (and migrates) the SQLite database. path ":memory:" is used by tests.
func openDB(path string) (*sql.DB, error) {
	dsn := path + "?_pragma=busy_timeout(5000)&_pragma=foreign_keys(1)&_pragma=journal_mode(WAL)"
	db, err := sql.Open("sqlite", dsn)
	if err != nil {
		return nil, err
	}
	// ponytail: one connection serialises every query, plenty for a metering proxy;
	// move to a read pool + single writer if DB time ever shows up in latency.
	db.SetMaxOpenConns(1)
	if _, err := db.Exec(schema); err != nil {
		db.Close()
		return nil, err
	}
	return db, nil
}
