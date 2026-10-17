-- 030: the wallet (P3.07). wallet_links is the saga log that moves a user's D1 balance into the Durable Object
-- wallet of their Midgar ID; a user links to exactly one Midgar ID, ever. residue_moves counts finished sweeps of
-- credits that landed in the frozen D1 row after the link, residue_pending holds the amount of a sweep in progress.
-- wallet_summary is an advisory mirror of the wallet balances for reporting; no decision reads it. apps.registry_slug
-- maps mako's app ids to the registry slug in the Midgar access token. Nothing writes these tables yet. Apply this
-- before deploying code that lists wallet_links in USER_DATA_TABLES.
CREATE TABLE IF NOT EXISTS wallet_links (
    app_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    midgar_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('moving', 'linked')),
    moved_credits INTEGER,
    residue_moves INTEGER NOT NULL DEFAULT 0,
    residue_pending INTEGER,
    welcome_part INTEGER,
    linked_at TEXT NOT NULL,
    PRIMARY KEY (app_id, user_id)
);

CREATE INDEX IF NOT EXISTS idx_wallet_links_midgar ON wallet_links(midgar_id, app_id, linked_at);

CREATE TABLE IF NOT EXISTS wallet_summary (
    app_id TEXT NOT NULL,
    midgar_id TEXT NOT NULL,
    balance INTEGER NOT NULL,
    purchased INTEGER NOT NULL,
    spent INTEGER NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (app_id, midgar_id)
);

ALTER TABLE apps ADD COLUMN registry_slug TEXT;

UPDATE apps SET registry_slug = CASE app_id
    WHEN 'dreameater' THEN 'dream-eater'
    WHEN 'payday' THEN 'pay-day'
    WHEN 'livingdex' THEN 'living-dex'
    WHEN 'doublekick' THEN 'double-kick'
    WHEN 'pixie' THEN 'pixiepocket'
    ELSE app_id
END;
