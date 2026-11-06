-- 040: Google Play pack purchases (P6.44). Adds apps.play_package_name, the Android package whose purchases
-- POST /v1/credits/purchase/validate verifies with the Google Play Developer API, and names PixiePocket's. A tenant
-- without one answers platform "android" with 503 not_configured. Adds credit_purchases.store_token, the Play purchase
-- token a credited purchase keeps until mako has acknowledged it at Google (the cron acknowledges what the validate
-- call could not). Adds play_voided_sweeps, where the cron's sweep of Google's voided purchases (refunds, chargebacks,
-- cancellations) records how far each package was read. Apply it before deploying the code that uses it; the code
-- reads apps through SELECT * and only logs a failed token write, so the other order credits correctly too. Back up the
-- database first; safe to repeat except for the ALTERs.
ALTER TABLE apps ADD COLUMN play_package_name TEXT;
UPDATE apps SET play_package_name = 'com.guitaripod.pixie' WHERE app_id = 'pixie';
ALTER TABLE credit_purchases ADD COLUMN store_token TEXT;
CREATE TABLE IF NOT EXISTS play_voided_sweeps (
    package_name TEXT PRIMARY KEY,
    swept_until_ms INTEGER NOT NULL,
    updated_at TEXT NOT NULL
);
