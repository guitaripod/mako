-- 020: per-tenant privacy flags. store_prompts = 0 keeps every prompt, scene and input text out of
-- usage_records, stored_images and the logs; store_images = 0 returns generated images to the client
-- as base64 and never writes them to R2 or stored_images. Both default to 1 so every existing tenant
-- behaves exactly as before; only dreameater is switched off. The ALTERs run once per database, the
-- UPDATE is safe to repeat.
ALTER TABLE apps ADD COLUMN store_prompts INTEGER NOT NULL DEFAULT 1;
ALTER TABLE apps ADD COLUMN store_images INTEGER NOT NULL DEFAULT 1;

UPDATE apps SET store_prompts = 0, store_images = 0 WHERE app_id = 'dreameater';
