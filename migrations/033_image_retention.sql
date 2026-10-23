-- 033: image retention. The 30-minute cron deletes stored images (R2 object, report rows, stored_images row)
-- once stored_images.expires_at has passed, except for a tenant with keep_images = 1, whose images stay until
-- the user deletes them or the account. PixiePocket ('pixie') keeps them: its gallery lists a user's own images
-- from stored_images and its published privacy policy says they stay until deleted.
ALTER TABLE apps ADD COLUMN keep_images INTEGER NOT NULL DEFAULT 0;
UPDATE apps SET keep_images = 1 WHERE app_id = 'pixie';
CREATE INDEX IF NOT EXISTS idx_stored_images_expires_at ON stored_images(expires_at);
