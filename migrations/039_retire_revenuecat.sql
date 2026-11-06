-- 039: finishes retiring RevenueCat. Apply AFTER the deploy that reads both payment_provider labels and before the
-- deploy that reads only 'app_store': relabels every App Store pack purchase and drops the two RevenueCat columns of
-- apps (rc_product_prefix, which 038 copied into store_product_prefix, and rc_project_id). Back up the database first.
UPDATE credit_purchases SET payment_provider = 'app_store' WHERE payment_provider = 'revenuecat';
ALTER TABLE apps DROP COLUMN rc_product_prefix;
ALTER TABLE apps DROP COLUMN rc_project_id;
