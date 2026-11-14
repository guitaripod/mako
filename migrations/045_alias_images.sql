-- 045: an alias may take pictures (P6.62, Midgar D39). Adds chat_aliases.max_images: how many pictures a call of
-- the alias may carry in "images" beside its input, each a data: URL or bare base64 of a JPEG, PNG, WebP or GIF.
-- 0, the default, is none, so every alias that exists keeps refusing them. The pictures join the caller's turn for
-- the model to look at and are never stored. Apply it before deploying the code that uses it: the alias loader
-- names the new column. The tenant pixie, whose photo.ideas.v1 is the first such alias and is free in its Pixie
-- Week, gets its own share of the Anthropic ceiling, 0.50 dollars a day, so that allowance can never use up what
-- the other apps share. Safe to repeat except for the ALTER.
ALTER TABLE chat_aliases ADD COLUMN max_images INTEGER NOT NULL DEFAULT 0;

UPDATE apps SET anthropic_daily_micro_usd = 500000 WHERE app_id = 'pixie' AND anthropic_daily_micro_usd IS NULL;
