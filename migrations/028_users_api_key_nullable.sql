-- 028: users.api_key becomes nullable and non-unique, and its two indexes go. This is the bridge for removing the column
-- (P3.07 step 2): the deployed code inserts api_key (NOT NULL UNIQUE) and the next code release omits it, so neither order
-- works against a one-shot DROP COLUMN. After this rebuild both the old and the new code run. Migration 029 drops the column
-- once the new code has soaked. Apply this BEFORE deploying the code release.
--
-- Same procedure and PRAGMA handling as 005: `users` is referenced by foreign keys from stored_images, usage_records,
-- device_auth_flows, credit_transactions, credit_purchases (and others), so the rebuild runs with `PRAGMA foreign_keys=OFF`,
-- not `defer_foreign_keys`. All user ids are preserved, so foreign_key_check returns no rows. Only idx_users_provider,
-- idx_users_app_provider and idx_users_api_key_hash are recreated; idx_users_api_key and idx_users_app_api_key are not.
-- Not safe to repeat: run it once per database, after exporting a backup.
PRAGMA foreign_keys=OFF;

CREATE TABLE users_new (
    id TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    email TEXT,
    name TEXT,
    api_key TEXT,
    is_admin BOOLEAN NOT NULL DEFAULT FALSE,
    preferred_model TEXT NOT NULL DEFAULT 'gemini-2.5-flash',
    gemini_api_key TEXT,
    openai_api_key TEXT,
    created_at TIMESTAMP NOT NULL,
    updated_at TIMESTAMP NOT NULL,
    app_id TEXT NOT NULL DEFAULT 'pixie',
    api_key_hash TEXT,
    premium_checked_at TEXT,
    premium_cached INTEGER
);

INSERT INTO users_new (id, provider, provider_id, email, name, api_key, is_admin, preferred_model, gemini_api_key, openai_api_key, created_at, updated_at, app_id, api_key_hash, premium_checked_at, premium_cached)
    SELECT id, provider, provider_id, email, name, api_key, is_admin, preferred_model, gemini_api_key, openai_api_key, created_at, updated_at, app_id, api_key_hash, premium_checked_at, premium_cached FROM users;

DROP TABLE users;
ALTER TABLE users_new RENAME TO users;

CREATE INDEX idx_users_provider ON users(provider, provider_id);
CREATE UNIQUE INDEX idx_users_app_provider ON users(app_id, provider, provider_id);
CREATE UNIQUE INDEX idx_users_api_key_hash ON users(api_key_hash);

PRAGMA foreign_key_check;
PRAGMA foreign_keys=ON;
