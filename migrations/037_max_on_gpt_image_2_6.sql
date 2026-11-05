-- 037: PixiePocket's Max tier moves from GPT Image 2.5 Sunburst to GPT Image 2.6 Sunburst (released 2026-11-04 at the
-- same token rates, faster, with steadier detail across edits), still at high quality with Nano Banana Pro behind it.
-- Deploy the code that knows gpt-image-2.6-sunburst first; the old code sends any unknown id as gpt-image-2. Safe to
-- repeat.
UPDATE image_tiers SET chain = '["gpt-image-2.6-sunburst:high", "gemini-3-pro-image"]' WHERE app_id = 'pixie' AND tier = 'max';
