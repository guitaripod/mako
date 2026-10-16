-- 026: per-device client API keys. A login for an existing user inserts a new row here and returns that key, so signing in on
-- a second device never invalidates the first. Only the SHA-256 of a key is stored. Auth tries this table first, then the
-- legacy single key in users.api_key_hash (migration 025), then the legacy plaintext column. Account deletion removes every
-- row for the user. Safe to repeat.
CREATE TABLE IF NOT EXISTS api_keys (
    key_hash TEXT PRIMARY KEY,
    user_id TEXT NOT NULL,
    app_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    last_used_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_api_keys_app_user ON api_keys(app_id, user_id);
