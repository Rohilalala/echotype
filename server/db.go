package main

import (
	"database/sql"
	"strings"

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

-- One row per proxied request: counts only, never text or audio. A row is written as a
-- reservation (upper bounds) before the upstream call and settled to real figures after.
CREATE TABLE IF NOT EXISTS usage (
	account_id INTEGER NOT NULL REFERENCES accounts(id),
	at         INTEGER NOT NULL,
	words      INTEGER NOT NULL,
	audio_ms   INTEGER NOT NULL,
	tokens     INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS usage_account_at ON usage(account_id, at);

CREATE TABLE IF NOT EXISTS stripe_events (
	id TEXT PRIMARY KEY,
	at INTEGER NOT NULL
);

-- Opt-in anonymous app telemetry (POST /v1/telemetry): a random install id, never an account.
CREATE TABLE IF NOT EXISTS telemetry_days (
	install_id  TEXT NOT NULL,
	date        TEXT NOT NULL,
	version     TEXT NOT NULL,
	os          TEXT NOT NULL,
	arch        TEXT NOT NULL,
	data        TEXT NOT NULL,
	received_at INTEGER NOT NULL,
	PRIMARY KEY (install_id, date)
);
CREATE TABLE IF NOT EXISTS telemetry_crashes (
	install_id  TEXT NOT NULL,
	version     TEXT NOT NULL,
	os          TEXT NOT NULL,
	arch        TEXT NOT NULL,
	message     TEXT NOT NULL,
	backtrace   TEXT NOT NULL,
	received_at INTEGER NOT NULL
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
	// Columns added after the first release. On a database that already has them the
	// ALTER fails with "duplicate column", which is the no-op case.
	if _, err := db.Exec(`ALTER TABLE usage ADD COLUMN tokens INTEGER NOT NULL DEFAULT 0`); err != nil && !strings.Contains(err.Error(), "duplicate column") {
		db.Close()
		return nil, err
	}
	return db, nil
}
