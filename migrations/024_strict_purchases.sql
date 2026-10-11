-- 024: stricter purchase handling, switched on per tenant. strict_purchases = 1 makes the RevenueCat webhook
-- credit NON_RENEWING_PURCHASE events (deduplicated with the client validate call on the same transaction id),
-- reverse CANCELLATION refunds of credit packs down to a zero balance exactly once, skip SANDBOX events when
-- the worker runs in production, and makes the premium check require the tenant's own entitlement and fall back
-- to the last known answer for 24 hours when RevenueCat cannot be reached. Every tenant defaults to 0 and keeps
-- today's behaviour; only dreameater is switched on. The ALTERs run once per database; the rest is safe to repeat.
ALTER TABLE apps ADD COLUMN strict_purchases INTEGER NOT NULL DEFAULT 0;
ALTER TABLE users ADD COLUMN premium_checked_at TEXT;
ALTER TABLE users ADD COLUMN premium_cached INTEGER;

CREATE TABLE IF NOT EXISTS purchase_refunds (
    app_id TEXT NOT NULL,
    transaction_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    credits INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (app_id, transaction_id)
);

UPDATE apps SET strict_purchases = 1 WHERE app_id = 'dreameater';
