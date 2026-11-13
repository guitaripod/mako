-- 042: Crucible as a tenant, a daily allowance per alias and a tenant's own share of the Anthropic ceiling (P6.54,
-- Midgar D40). Adds chat_aliases.daily_limit: how many answered calls of an alias one person gets per UTC day, whoever
-- pays for them (0, the default, is no limit); the count is kept in daily_usage and a call over it answers 429
-- daily_limit_reached before the provider is called. Adds apps.anthropic_daily_micro_usd: the most a tenant's own
-- Anthropic calls may cost in one UTC day, in millionths of a dollar (NULL, the default, leaves only the ceiling all
-- tenants share); a tenant's spend is counted in provider_spend under the provider name 'anthropic:<app_id>'. Seeds the
-- tenant crucible (Crucible for Plex): chat only, no welcome credits, Pro is the ledger entitlement 'pro' and is
-- remembered for a day while the ledger cannot answer, nothing of a prompt is stored, and its Anthropic calls stop at
-- 0.75 dollars a day so a free allowance there can never use up what the other apps share. Its aliases are inserted by
-- the operator from the Crucible repository (tools/ai/gen_alias_sql.py), like every alias. Apply it before deploying
-- the code that uses it: the alias loader names the new column. Safe to repeat except for the ALTERs.
ALTER TABLE chat_aliases ADD COLUMN daily_limit INTEGER NOT NULL DEFAULT 0;
ALTER TABLE apps ADD COLUMN anthropic_daily_micro_usd INTEGER;

CREATE TABLE IF NOT EXISTS daily_usage (
    app_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    alias TEXT NOT NULL,
    day TEXT NOT NULL,
    used INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (app_id, user_id, alias, day)
);

INSERT OR IGNORE INTO apps
  (app_id, name, enabled, apple_team_id, apple_app_bundle_id, enabled_capabilities, new_user_free_credits,
   premium_entitlement, default_chat_model, store_prompts, store_images, strict_premium, anthropic_daily_micro_usd)
VALUES
  ('crucible', 'Crucible for Plex', 1, 'P4DQK6SRKR', 'com.guitaripod.crucible', 'chat.completion', 0,
   'pro', 'claude-haiku-5-5', 0, 0, 1, 750000);
