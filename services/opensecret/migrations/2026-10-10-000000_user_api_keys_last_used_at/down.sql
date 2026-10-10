-- This file should undo anything in `up.sql`
ALTER TABLE user_api_keys DROP COLUMN last_used_at;
