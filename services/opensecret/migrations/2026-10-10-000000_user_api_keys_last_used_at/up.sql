-- Track when each user API key last authenticated a request.
-- Nullable: keys never used remain NULL so the UI can show "never used".
ALTER TABLE user_api_keys ADD COLUMN last_used_at TIMESTAMP WITH TIME ZONE;
