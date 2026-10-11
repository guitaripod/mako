-- 021: server-side prompt aliases and the weekly free allowance. A request that carries an `alias`
-- resolves its model, system prompt, response schema and price from chat_aliases, so a prompt fix
-- ships without an app release. No alias rows are seeded here; the operator inserts them per tenant.
--
--   chat_aliases   one row per (tenant, alias). weekly_free is the number of free calls per UTC week
--                  (0 = none); premium_bypass = 1 lets premium users call it for free.
--   weekly_usage   calls already taken from the weekly bucket; week_start is the Monday of the UTC
--                  week as YYYY-MM-DD, so a new week starts a new row and the old one simply ages out.
--   chat_requests  one row per settled alias request, keyed like the ledger (chat:<idempotency_key>),
--                  so a repeated key or a retry is recognised even when the call was free and left no
--                  ledger row. replays counts the free repeats of one key (capped in code), and the
--                  unique retry_of index lets one request be retried for free once.
CREATE TABLE IF NOT EXISTS chat_aliases (
    app_id TEXT NOT NULL,
    alias TEXT NOT NULL,
    model TEXT NOT NULL,
    system_prompt TEXT NOT NULL,
    response_schema TEXT,
    max_tokens INTEGER,
    temperature REAL,
    flat_credits INTEGER NOT NULL DEFAULT 1,
    weekly_free INTEGER NOT NULL DEFAULT 0,
    premium_bypass INTEGER NOT NULL DEFAULT 0,
    version INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (app_id, alias)
);

CREATE TABLE IF NOT EXISTS weekly_usage (
    app_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    alias TEXT NOT NULL,
    week_start TEXT NOT NULL,
    used INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (app_id, user_id, alias, week_start)
);

CREATE TABLE IF NOT EXISTS chat_requests (
    app_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    reference TEXT NOT NULL,
    alias TEXT NOT NULL,
    kind TEXT NOT NULL,
    retry_of TEXT,
    replays INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    PRIMARY KEY (app_id, user_id, reference)
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_chat_requests_retry
    ON chat_requests(app_id, user_id, retry_of) WHERE retry_of IS NOT NULL;
