-- 027: index for the scheduled prune of api_keys that were not used for 180 days. The prune orders by the same
-- expression, COALESCE(last_used_at, created_at), so it reads the oldest rows without scanning the table. Safe to repeat.
CREATE INDEX IF NOT EXISTS idx_api_keys_last_used ON api_keys(COALESCE(last_used_at, created_at));
