-- 046: devices that deleted an account in a tenant. Deleting an account erases every row of the person, which also
-- erased the proof that the device had already had its free start (the welcome bonus and the edit runway), so a
-- device could delete and sign up again for a new one as often as it liked. On 2026-11-15 one PixiePocket device did
-- that every six minutes, each round spending Gemini on five to eight free edits. A row here is a one-way hash of the
-- tenant, provider and device id (an anonymous Keychain id or a Midgar ID) with no user_id: pseudonymous data kept
-- for fraud prevention, which migration 047 limits to twelve months and the privacy policies disclose. A new account
-- from a remembered device starts without the free start. Apply it before deploying the code that uses it. Safe to
-- repeat.
CREATE TABLE IF NOT EXISTS returning_devices (
    app_id TEXT NOT NULL,
    device_hash TEXT NOT NULL,
    first_deleted_at TEXT NOT NULL,
    last_deleted_at TEXT NOT NULL,
    deletions INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (app_id, device_hash)
);
