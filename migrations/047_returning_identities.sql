-- 047: returning_devices now holds every identity whose free start in a tenant is spent, not only devices: the
-- anonymous Keychain id, the Midgar ID, and the Apple, Google or GitHub account an account signed in with. A row is
-- written when such an account is deleted, or when an anonymous account is linked to Sign in with Apple and carries
-- its free start away from the device id. Only tenants that give a free start keep rows. The rows are pseudonymous
-- data kept for fraud prevention: the cron forgets a row twelve months after last_deleted_at, which this index serves,
-- and the privacy policies of the tenants say so. Apply it before deploying the code that uses it. Safe to repeat.
CREATE INDEX IF NOT EXISTS returning_devices_by_last_deleted ON returning_devices (last_deleted_at);
