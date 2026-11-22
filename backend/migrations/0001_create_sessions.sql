-- Sessions table (signaling + PIN gate). Uses IF NOT EXISTS so this applies
-- cleanly to the production DB, which was created from the old schema.sql
-- before migrations were tracked. Future schema changes go in new numbered
-- files: `npx wrangler d1 migrations create screenshare-signals <name>`.
CREATE TABLE IF NOT EXISTS sessions (
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
