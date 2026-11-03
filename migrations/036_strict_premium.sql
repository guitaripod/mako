-- 036: a tenant whose premium is sold through the purchase ledger counts a RevenueCat entitlement as premium only when
-- it is the tenant's premium entitlement. Without this, a non-strict tenant reads any active RevenueCat entitlement as
-- premium, and PixiePocket's old pack-era entitlements read as Plus (60 edits a week). strict_purchases is not used
-- for this because it also changes how RevenueCat webhooks credit packs. Safe to repeat except for the ALTER.
ALTER TABLE apps ADD COLUMN strict_premium INTEGER NOT NULL DEFAULT 0;
UPDATE apps SET strict_premium = 1 WHERE app_id = 'pixie';
