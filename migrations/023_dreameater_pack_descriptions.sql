-- 023: Dream Eater 2.0 credits buy art, extra readings, lenses and follow-ups, not "dreams". Pack ids,
-- names, credits and prices are unchanged; only the description text is. Safe to repeat.
UPDATE credit_packs SET description = '5 credits' WHERE app_id = 'dreameater' AND pack_id = 'small';
UPDATE credit_packs SET description = '15 credits' WHERE app_id = 'dreameater' AND pack_id = 'large';
UPDATE credit_packs SET description = '50 credits' WHERE app_id = 'dreameater' AND pack_id = 'mega';
UPDATE credit_packs SET description = '100 credits' WHERE app_id = 'dreameater' AND pack_id = 'giga';
