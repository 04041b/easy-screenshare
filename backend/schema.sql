-- v2: add PIN gate columns (pin_salt, pin_hash, pin_fails). Early-stage
-- project: dropping and recreating is fine. If you've previously run
-- db:migrate:local, re-run it to pick up the new columns.
DROP TABLE IF EXISTS sessions;
CREATE TABLE sessions (
  id            TEXT PRIMARY KEY,
  sender_token  TEXT NOT NULL,
  pin_salt      TEXT NOT NULL,
  pin_hash      TEXT NOT NULL,
  pin_fails     INTEGER NOT NULL DEFAULT 0,
  sender_offer  TEXT,
  viewer_answer TEXT,
  fallback      INTEGER NOT NULL DEFAULT 0,
  created_at    INTEGER NOT NULL,
  expires_at    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_sessions_expires ON sessions(expires_at);
