-- 043: an alias may let an Anthropic model think before it answers (P6.54). Adds chat_aliases.reasoning: NULL, the
-- default, keeps the forced tool call that returns the reply's structure at once; 'low', 'medium' or 'high' asks for
-- the same structure as a structured output at that effort instead, which leaves the model free to think first
-- (a forced tool call cannot). It costs more output tokens and a few seconds, so it is for aliases whose answer is a
-- judgement, like choosing what to watch. Other providers ignore it. Apply it before deploying the code that uses it:
-- the alias loader names the new column. Not safe to repeat (ALTER).
ALTER TABLE chat_aliases ADD COLUMN reasoning TEXT;
