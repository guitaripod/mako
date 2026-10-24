-- 034: Dream Eater's App Store prices for three packs rose after 007 seeded them. price_usd_cents is what mako records
-- as a purchase's amount and sorts packs by, so it follows App Store Connect: large $7.99, mega $24.99, giga $39.99.
-- Small stays $2.99. Safe to repeat.
UPDATE credit_packs SET price_usd_cents = 799 WHERE app_id = 'dreameater' AND pack_id = 'large';
UPDATE credit_packs SET price_usd_cents = 2499 WHERE app_id = 'dreameater' AND pack_id = 'mega';
UPDATE credit_packs SET price_usd_cents = 3999 WHERE app_id = 'dreameater' AND pack_id = 'giga';
