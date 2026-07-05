-- Seed the mako (openai-image-proxy D1) `helia` tenant. Idempotent.
--
-- Helia is a sun-safety / UV-exposure companion. The only mako-metered AI
-- capability is `chat.completion`, which powers the "Ask the Science" evidence
-- explainer (grounded, cited answers about UV, SPF, vitamin D, skin types).
-- Everything else Helia does (sun sessions, burn events, leaderboard, friends,
-- push, content) lives in the sibling `solar-worker` against its OWN D1 and is
-- NOT charged through mako.
--
-- Apply from ~/Dev/rust/pixie (DATA-ONLY onboarding, no Worker deploy):
--   CLOUDFLARE_API_TOKEN=$(cat ~/.cloudflare-api-token) \
--     npx wrangler d1 execute openai-image-proxy --remote \
--       --file migrations/012_seed_helia.sql
--
-- This is the pixie-side mirror of the canonical Helia copy at
-- ~/Dev/ios/Helia/backend/migrations/0001_seed_helia.sql. Keep the two in sync;
-- both are INSERT OR REPLACE so applying either (or both) is safe.
--
-- SKU mapping: rc_product_prefix + pack_id = the App Store consumable id
--   (e.g. com.guitaripod.helia.credits. + starter = com.guitaripod.helia.credits.starter).

INSERT OR REPLACE INTO apps
  (app_id, name, enabled, rc_project_id, rc_product_prefix, apple_team_id, apple_app_bundle_id,
   enabled_capabilities, new_user_free_credits, premium_entitlement, default_chat_model)
VALUES
  ('helia', 'Helia', 1, NULL, 'com.guitaripod.helia.credits.', 'P4DQK6SRKR',
   'com.guitaripod.helia', 'chat.completion', 15, 'pro', 'gpt-5-mini');

-- "Ask the Science" evidence explainer = one chat.completion call. Flat 3 credits
-- per question; `pro` entitlement holders skip the charge (verified server-side
-- against RevenueCat). rc_project_id is filled in once the RevenueCat project +
-- IAP products exist.
INSERT OR REPLACE INTO capability_costs (app_id, capability, flat_credits, credit_multiplier) VALUES
  ('helia', 'chat.completion', 3, NULL);

-- Credit packs. 1 question = 3 credits, so `starter` ~= 10 questions. Free users
-- get 15 credits (= 5 questions) on first launch to try the explainer before
-- buying. Heavy users / data nerds get the bigger tiers; `pro` subscribers bypass
-- credits entirely.
INSERT OR REPLACE INTO credit_packs
  (app_id, pack_id, name, credits, bonus_credits, price_usd_cents, description, sort_order)
VALUES
  ('helia', 'starter', 'Starter',  30,  0,  299, '~10 Ask-the-Science questions',          0),
  ('helia', 'regular', 'Regular', 110,  0,  999, 'A season of curiosity (~36 questions)',   1),
  ('helia', 'propack', 'Pro Pack', 300, 0, 2499, 'For the data-driven (~100 questions)',    2),
  ('helia', 'science', 'Science', 650,  0, 4999, 'Ask everything (~216 questions)',         3);
