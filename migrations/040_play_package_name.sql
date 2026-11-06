-- 040: Google Play pack purchases (P6.44). Adds apps.play_package_name, the Android package whose purchases
-- POST /v1/credits/purchase/validate verifies with the Google Play Developer API, and names PixiePocket's. A tenant
-- without one answers platform "android" with 503 not_configured. The code reads the column through SELECT *, so it
-- may deploy before or after this migration. Back up the database first; safe to repeat except for the ALTER.
ALTER TABLE apps ADD COLUMN play_package_name TEXT;
UPDATE apps SET play_package_name = 'com.guitaripod.pixie' WHERE app_id = 'pixie';
