ALTER TABLE user_seed_wrappings
    DROP COLUMN IF EXISTS recovery_use_count,
    DROP COLUMN IF EXISTS recovery_last_used_at;
