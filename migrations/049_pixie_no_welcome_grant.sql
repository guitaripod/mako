-- 049: PixiePocket's free start is the Pixie Week alone (5 standard edits a day for 7 days from the first edit, once
-- per device since 046 to 048). The 65-credit welcome grant of 019 was raised because every new person stopped after
-- one image; the week answers that, and the grant only paid silently for a sixth edit in a day or lingered as three
-- edits after the week had told the person it was over. Accounts that already hold welcome credits keep them.
-- Safe to repeat.
UPDATE apps SET new_user_free_credits = 0 WHERE app_id = 'pixie';
