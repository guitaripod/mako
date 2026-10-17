-- 029: drop users.api_key. DO NOT APPLY until migration 028 AND the code release that no longer reads or writes the column
-- have been in production long enough to soak (P3.07 step 2). Irreversible: export the production database to
-- ~/.config/midgar/backups/ (mode 600) first. SQLite allows DROP COLUMN once the column is not indexed, unique or part of a
-- key, which 028 guarantees. If D1 refuses it, repeat 028's rebuild without the column instead.
-- Not safe to repeat: the column is gone after the first run.
ALTER TABLE users DROP COLUMN api_key;
