-- 017: paywall hit events — one row every time a metered capability answers
-- 402 because the wallet's balance is below the capability's rate. Feeds the
-- activation funnel: today no Psybeam wallet has ever exhausted its free
-- minutes, so this table is what would tell us if/when that starts happening.

CREATE TABLE IF NOT EXISTS paywall_events (
    id TEXT PRIMARY KEY,
    app_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    capability TEXT NOT NULL,
    balance INTEGER NOT NULL,
    created_at TIMESTAMP NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_paywall_events_app_created
    ON paywall_events(app_id, created_at);
