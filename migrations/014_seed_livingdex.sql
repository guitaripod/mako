-- Seed the mako (openai-image-proxy D1) `livingdex` tenant. Idempotent.
--
-- Living Dex is a camera-first "real-life Pokédex": identify and collect every
-- life form on Earth. Species identification runs on-device (Core ML) for free;
-- mako meters only the Pro-tier CLOUD AI, all of which is a `chat.completion`
-- call to Claude:
--   * vision corroboration / high-accuracy cloud ID  -> claude-sonnet-4-6 + image
--   * "ask the creature" grounded Q&A                 -> claude-haiku-4-5
-- Rich Pokédex-entry narration is precomputed server-side via the Claude Batch
-- API in the Living Dex domain Worker and cached per species+locale, so it is
-- NOT charged through this per-user chat path.
--
-- Charging is TOKEN-BASED (no flat capability_costs row): the two call types
-- differ ~5x in cost (Sonnet+image vs a short Haiku turn), so a single flat
-- price would over/undercharge. mako reserves MIN_BALANCE_GUARD up front, then
-- deducts real cost from model_prices() x CREDIT_MULTIPLIER. `pro` entitlement
-- holders bypass credits entirely (verified server-side against RevenueCat).
--
-- Apply from ~/Dev/rust/pixie (DATA-ONLY onboarding, no Worker deploy):
--   CLOUDFLARE_API_TOKEN=$(cat ~/.cloudflare-api-token) \
--     npx wrangler d1 execute openai-image-proxy --remote \
--       --file migrations/014_seed_livingdex.sql
--
-- rc_project_id is NULL until the RevenueCat project + IAP products exist; fill
-- it in then. Also set the tenant secret REVENUECAT_IOS_API_KEY_LIVINGDEX so the
-- `pro` premium bypass can verify entitlements.
--
-- SKU mapping: rc_product_prefix + pack_id = the App Store consumable id
--   (e.g. com.guitaripod.livingdex.credits. + starter = ...credits.starter).

INSERT OR REPLACE INTO apps
  (app_id, name, enabled, rc_project_id, rc_product_prefix, apple_team_id, apple_app_bundle_id,
   enabled_capabilities, new_user_free_credits, premium_entitlement, default_chat_model)
VALUES
  ('livingdex', 'Living Dex', 1, NULL, 'com.guitaripod.livingdex.credits.', 'P4DQK6SRKR',
   'com.guitaripod.livingdex', 'chat.completion', 30, 'pro', 'claude-haiku-4-5');

-- No capability_costs row: charging is token-based (see header). Free users get
-- 30 credits on first launch (~10 cloud IDs or ~30 Q&A turns) to taste the Pro
-- magic before subscribing; `pro` subscribers bypass credits entirely.

-- Credit packs for non-subscribers / à-la-carte. A Sonnet cloud ID ~3 credits,
-- a Haiku Q&A ~1 credit, so `starter` ~= 30 cloud IDs. Subscription (`pro`) is
-- the primary monetization; packs are the whale / non-sub fallback.
INSERT OR REPLACE INTO credit_packs
  (app_id, pack_id, name, credits, bonus_credits, price_usd_cents, description, sort_order)
VALUES
  ('livingdex', 'starter', 'Starter',  100,  0,  299, '~30 cloud identifications',            0),
  ('livingdex', 'regular', 'Regular',  400,  0,  999, 'A season of discovery (~130 IDs)',      1),
  ('livingdex', 'propack', 'Explorer', 1200, 0, 2499, 'For the relentless collector (~400)',   2),
  ('livingdex', 'science', 'Naturalist', 2600, 0, 4999, 'Identify everything (~860 IDs)',       3);
