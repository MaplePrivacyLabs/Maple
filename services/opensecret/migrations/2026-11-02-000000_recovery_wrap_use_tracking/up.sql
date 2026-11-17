-- Use tracking for the single recovery slot. The code is a long-lived
-- credential that survives a preserving reset by design, so the owner needs a
-- way to notice that it was used. Rotation replaces the row, which resets both
-- columns to the defaults below; a successful preserving reset bumps them.
ALTER TABLE user_seed_wrappings
    ADD COLUMN recovery_last_used_at TIMESTAMPTZ,
    ADD COLUMN recovery_use_count INTEGER NOT NULL DEFAULT 0;
