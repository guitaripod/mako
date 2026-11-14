-- 044: Master of Inventory as a tenant (P6.55, Midgar D39). The app asks one thing of a model: which lines of a
-- receipt are things that were bought. It reaches mako with its Midgar ID through the gateway, so the tenant's app_id
-- is the registry slug the gateway already sends as X-App-ID. Chat only, no welcome credits and nothing to buy: the
-- feature is part of Pro, the ledger entitlement 'pro', remembered for a day while the ledger cannot answer. Nothing
-- of a prompt is stored, and its Anthropic calls stop at 0.50 dollars a day so it can never use up what the other
-- apps share. Its alias is inserted by the operator from apps/master-of-inventory/tools/ai with
-- services/intelligence/scripts/gen_alias_sql.py, like every alias. Data only, no deploy. Safe to repeat.
INSERT OR IGNORE INTO apps
  (app_id, name, enabled, apple_team_id, apple_app_bundle_id, enabled_capabilities, new_user_free_credits,
   premium_entitlement, default_chat_model, store_prompts, store_images, strict_premium, anthropic_daily_micro_usd)
VALUES
  ('master-of-inventory', 'Master of Inventory', 1, 'P4DQK6SRKR', 'com.marcusziade.inventory', 'chat.completion', 0,
   'pro', 'claude-haiku-5-5', 0, 0, 1, 500000);
