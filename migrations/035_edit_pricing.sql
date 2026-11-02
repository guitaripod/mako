-- 035: per-edit pricing (roadmap P6.33). A tenant with apps.credits_per_edit sells edits: image_tiers names what each
-- tier costs in edits and which models serve it, tried in order; a premium subscriber's edits come from a weekly
-- allowance of premium_weekly_edits, and a person's first runway_days days (from their first successful edit) carry
-- runway_daily_edits free standard edits a day. edit_allowances counts both; runways holds when each person's week
-- started. Every other tenant keeps the old per-model credit prices because its credits_per_edit stays NULL.
--
-- PixiePocket ('pixie') moves onto it: 21 credits stay one edit, so every balance, the wallet and old clients keep
-- their meaning; Standard is one edit on Nano Banana 2.1 with GPT Image 2 behind it, Max is five on GPT Image 2.5
-- Sunburst with Nano Banana Pro behind it; Plus is 60 edits a week; the Pixie Week is 5 a day for 7 days. The three
-- packs it still sells grant 10, 40 and 120 edits; business and enterprise keep their grants for old clients.
-- The ALTERs run once per database.
ALTER TABLE apps ADD COLUMN credits_per_edit INTEGER;
ALTER TABLE apps ADD COLUMN premium_weekly_edits INTEGER NOT NULL DEFAULT 0;
ALTER TABLE apps ADD COLUMN runway_days INTEGER NOT NULL DEFAULT 0;
ALTER TABLE apps ADD COLUMN runway_daily_edits INTEGER NOT NULL DEFAULT 0;

CREATE TABLE IF NOT EXISTS image_tiers (
    app_id TEXT NOT NULL,
    tier TEXT NOT NULL,
    edits INTEGER NOT NULL,
    chain TEXT NOT NULL,
    PRIMARY KEY (app_id, tier)
);

CREATE TABLE IF NOT EXISTS edit_allowances (
    app_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    bucket TEXT NOT NULL,
    period TEXT NOT NULL,
    used INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (app_id, user_id, bucket, period)
);

CREATE TABLE IF NOT EXISTS runways (
    app_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    started_at TEXT NOT NULL,
    PRIMARY KEY (app_id, user_id)
);

INSERT OR REPLACE INTO image_tiers (app_id, tier, edits, chain) VALUES
    ('pixie', 'standard', 1, '["gemini-nano-banana-2.1", "gpt-image-2:medium"]'),
    ('pixie', 'max', 5, '["gpt-image-2.5-sunburst:high", "gemini-3-pro-image"]');

UPDATE apps SET credits_per_edit = 21, premium_weekly_edits = 60, runway_days = 7, runway_daily_edits = 5,
    premium_entitlement = 'plus'
WHERE app_id = 'pixie';

UPDATE credit_packs SET credits = 210, bonus_credits = 0, description = '10 photo edits' WHERE app_id = 'pixie' AND pack_id = 'starter';
UPDATE credit_packs SET credits = 840, bonus_credits = 0, description = '40 photo edits' WHERE app_id = 'pixie' AND pack_id = 'basic';
UPDATE credit_packs SET credits = 2520, bonus_credits = 0, description = '120 photo edits' WHERE app_id = 'pixie' AND pack_id = 'popular';
