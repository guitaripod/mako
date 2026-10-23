-- 033: image retention, opt-in per tenant. The 30-minute cron deletes stored images (R2 object, report rows,
-- stored_images row) once stored_images.expires_at has passed, but only for a tenant with purge_expired_images = 1.
-- No tenant is opted in by this migration: apps may rely on old image URLs staying valid, so each tenant is
-- switched on by the operator (UPDATE apps SET purge_expired_images = 1 WHERE app_id = ...) once its app is known
-- not to need them. PixiePocket ('pixie') must stay off: its gallery lists a user's own images from stored_images
-- and its published privacy policy says they stay until the user deletes them.
ALTER TABLE apps ADD COLUMN purge_expired_images INTEGER NOT NULL DEFAULT 0;
CREATE INDEX IF NOT EXISTS idx_stored_images_expires_at ON stored_images(expires_at);
