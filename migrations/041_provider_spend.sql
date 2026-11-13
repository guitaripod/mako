-- 041: a daily ceiling in dollars on Anthropic spend (P3.21, Midgar D39). Adds provider_spend, one row per UTC day
-- and model provider holding what that day's answered calls cost in millionths of a dollar (from each call's token
-- usage at the model's listed prices) and how many calls that was. mako refuses Anthropic-backed chat with 429
-- rate_limit_exceeded once the day's Anthropic total reaches ANTHROPIC_DAILY_SPEND_LIMIT_USD (2.00 when unset); the
-- count starts over with the next day's row. Apply it before deploying the code that uses it: without the table the
-- code logs that the spend could not be read or counted and lets every call through. Safe to repeat.
CREATE TABLE IF NOT EXISTS provider_spend (
    day TEXT NOT NULL,
    provider TEXT NOT NULL,
    micro_usd INTEGER NOT NULL DEFAULT 0,
    calls INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (day, provider)
);
