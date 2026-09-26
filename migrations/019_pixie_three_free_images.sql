-- 019: PixiePocket's welcome grant covers three Nano Banana 2 images (3 x 21 = 63, +2)
-- instead of one. Every Aug-Sep 2026 user who generated stopped after exactly one image:
-- 25 - 21 left 4 credits, so the store opened before a second try and the review prompt
-- (asked at image two) never fired. Applied to prod 2026-09-26; idempotent.
UPDATE apps SET new_user_free_credits = 65 WHERE app_id = 'pixie';
