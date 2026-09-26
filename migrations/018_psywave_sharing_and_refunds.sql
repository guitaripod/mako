-- 018: Psywave public playlist pages and one-time refunds.
-- shared_playlists backs /p/<id>: a playlist someone chose to share, stored as
-- the sanitized JSON the page renders. credit_refunds is the claim table that
-- makes a charge refundable exactly once (its primary key is the charge's
-- ledger reference), and its timestamps enforce the daily refund allowance.

CREATE TABLE IF NOT EXISTS shared_playlists (
    id TEXT PRIMARY KEY,
    app_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    payload TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_shared_playlists_owner
    ON shared_playlists(app_id, user_id, created_at);

CREATE TABLE IF NOT EXISTS credit_refunds (
    reference_id TEXT PRIMARY KEY,
    app_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    amount INTEGER NOT NULL,
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_credit_refunds_owner
    ON credit_refunds(app_id, user_id, created_at);
