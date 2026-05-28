CREATE TABLE IF NOT EXISTS sessions (
  id            TEXT PRIMARY KEY,
  sender_token  TEXT NOT NULL,
  sender_offer  TEXT,
  viewer_answer TEXT,
  fallback      INTEGER NOT NULL DEFAULT 0,
  created_at    INTEGER NOT NULL,
  expires_at    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_sessions_expires ON sessions(expires_at);
