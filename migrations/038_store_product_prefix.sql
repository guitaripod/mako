-- 038: RevenueCat is retired (2026-11-06), so a tenant's App Store product prefix is no longer RevenueCat's. Adds
-- apps.store_product_prefix with the value of rc_product_prefix. Apply BEFORE deploying the code that reads
-- store_product_prefix and writes payment_provider 'app_store'; the code running now keeps reading rc_product_prefix.
-- Migration 039 drops the old column after that deploy. Safe to repeat except for the ALTER.
ALTER TABLE apps ADD COLUMN store_product_prefix TEXT;
UPDATE apps SET store_product_prefix = rc_product_prefix WHERE store_product_prefix IS NULL;
