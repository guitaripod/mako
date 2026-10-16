-- 025: client API keys are stored as a SHA-256 hash (api_key_hash), never as plaintext.
-- Auth looks the presented key up by its hash and falls back to the legacy plaintext column once per row, writing the
-- hash on that request. New and rotated keys write the hash and a non-secret placeholder ('hashed:' + user id) into
-- the NOT NULL UNIQUE api_key column. The scheduled handler backfills the remaining legacy rows in batches of 500,
-- inside Cloudflare, replacing each plaintext key with its placeholder. The ALTER runs once per database; the index
-- is safe to repeat. Apply with `wrangler d1 execute --file`, then deploy.
ALTER TABLE users ADD COLUMN api_key_hash TEXT;

CREATE UNIQUE INDEX IF NOT EXISTS idx_users_api_key_hash ON users(api_key_hash);
