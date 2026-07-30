-- 015: Apple Ads (AdServices) install attribution + payer join.
--
-- Two tables, both in the same D1 database as the credits ledger, both scoped by
-- app_id like every other tenant table:
--
--   install_attributions  one row per identity: the campaign/ad group/keyword
--                         Apple returned for that install (or attributed=0 for
--                         an organic install). The AdServices token itself is
--                         NEVER stored — only its SHA-256 fingerprint, which is
--                         what makes a resubmitted token a no-op instead of a
--                         second install.
--   purchase_attributions one row per paid purchase, carrying the campaign and
--                         keyword copied from the buyer's install row. This is
--                         the keyword -> payer join the stopping rules need.
--
-- No IDFA, no ATT, no device identifier, no IP: AdServices attribution describes
-- the ad, not the person.

CREATE TABLE IF NOT EXISTS install_attributions (
    app_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    token_hash TEXT NOT NULL,
    attributed INTEGER NOT NULL DEFAULT 0,
    org_id INTEGER,
    campaign_id INTEGER,
    ad_group_id INTEGER,
    keyword_id INTEGER,
    ad_id INTEGER,
    country_or_region TEXT,
    click_date TEXT,
    conversion_type TEXT,
    created_at TIMESTAMP NOT NULL,
    PRIMARY KEY (app_id, user_id)
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_install_attributions_token
    ON install_attributions(app_id, token_hash);
CREATE INDEX IF NOT EXISTS idx_install_attributions_keyword
    ON install_attributions(app_id, attributed, campaign_id, keyword_id);
CREATE INDEX IF NOT EXISTS idx_install_attributions_created
    ON install_attributions(app_id, created_at);

-- product_id is the store SKU for a subscription and the credit-pack id for a
-- pack, i.e. whatever identifies what was bought on the path that recorded it.
-- net_usd_cents is filled only when RevenueCat reported takehome_percentage;
-- otherwise it stays NULL and the report nets it out with apps.store_net_share
-- rather than assuming a commission.
CREATE TABLE IF NOT EXISTS purchase_attributions (
    purchase_id TEXT PRIMARY KEY,
    app_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    product_id TEXT,
    campaign_id INTEGER,
    ad_group_id INTEGER,
    keyword_id INTEGER,
    country_or_region TEXT,
    amount_usd_cents INTEGER NOT NULL DEFAULT 0,
    net_usd_cents INTEGER,
    created_at TIMESTAMP NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_purchase_attributions_app_user
    ON purchase_attributions(app_id, user_id);
CREATE INDEX IF NOT EXISTS idx_purchase_attributions_keyword
    ON purchase_attributions(app_id, campaign_id, keyword_id);

-- The tenant's share of a store sale after Apple's commission (0.85 under the
-- Small Business Program, 0.70 otherwise). Deliberately left NULL: the keyword
-- report will not invent a commission. Set it per app when the program is known,
-- e.g. UPDATE apps SET store_net_share = 0.85 WHERE app_id IN ('dreameater','payday');
-- Until it is set, the report returns net_per_payer_usd_cents = null for any
-- keyword whose purchases carry no RevenueCat takehome_percentage, and exposes
-- gross revenue_per_payer_usd_cents so the kill rule stays computable.
ALTER TABLE apps ADD COLUMN store_net_share REAL;
