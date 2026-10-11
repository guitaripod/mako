-- 022: Art v2. A request to POST /v1/images/generations that carries a `style` is composed server-side from
-- this table plus the client's short scene (and mood), so the app never sends raw dream text to the image
-- model. Tenants without rows here never take that path.
--
--   image_styles  per-tenant style fragments; `suffix` is the tenant's common closing sentence.
--   apps.art_credits  credits one art image costs; NULL leaves the tenant's image billing exactly as it
--                 was. Premium users are not charged, and nothing is charged until an image was made.
--
-- The ALTER runs once per database; the CREATE and the seed rows are safe to repeat.
ALTER TABLE apps ADD COLUMN art_credits INTEGER;

CREATE TABLE IF NOT EXISTS image_styles (
    app_id TEXT NOT NULL,
    style_id TEXT NOT NULL,
    fragment TEXT NOT NULL,
    suffix TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (app_id, style_id)
);

UPDATE apps SET art_credits = 1 WHERE app_id = 'dreameater';

INSERT OR REPLACE INTO image_styles (app_id, style_id, fragment, suffix) VALUES
    ('dreameater', 'dreamscape', 'soft painterly digital art, luminous atmosphere, deep violet and gold palette, surreal but coherent composition', 'No text, no letters, no logos, no watermarks, no recognizable real people. Avoid graphic violence, gore and nudity.'),
    ('dreameater', 'ink', 'sumi-e inspired ink wash, restrained palette, generous negative space', 'No text, no letters, no logos, no watermarks, no recognizable real people. Avoid graphic violence, gore and nudity.'),
    ('dreameater', 'gilded', 'ornate illustrated card, fine linework, symbolic composition, decorative border', 'No text, no letters, no logos, no watermarks, no recognizable real people. Avoid graphic violence, gore and nudity.'),
    ('dreameater', 'cinema', 'cinematic film still, shallow depth of field, moody lighting', 'No text, no letters, no logos, no watermarks, no recognizable real people. Avoid graphic violence, gore and nudity.'),
    ('dreameater', 'storybook', 'gouache children''s-book illustration, warm textures', 'No text, no letters, no logos, no watermarks, no recognizable real people. Avoid graphic violence, gore and nudity.'),
    ('dreameater', 'collage', 'surreal paper collage, vintage engraving cut-outs, muted colors', 'No text, no letters, no logos, no watermarks, no recognizable real people. Avoid graphic violence, gore and nudity.');
