-- 048: a tenant may mark its free start with one of Apple DeviceCheck's two bits per device (src/device_bits.rs).
-- Apple keeps the bits for the whole developer team, through app deletion, reinstall and a new Keychain id, and every
-- app of the team shares the same two, so each bit belongs to one tenant: bit 0 is PixiePocket's,
-- whose free start costs real Gemini edits; bit 1 is unclaimed. A DeviceCheck registration in a claiming tenant reads
-- the bits, opens a new account without the free start when the claimed bit was set in the last twelve months, and
-- sets it when it grants one. NULL claims nothing. The column is read with a fallback, so the order of this migration
-- and the deploy does not matter; apply it first anyway. Safe to repeat except for the ALTER.
ALTER TABLE apps ADD COLUMN devicecheck_free_start_bit INTEGER;

UPDATE apps SET devicecheck_free_start_bit = 0 WHERE app_id = 'pixie';
