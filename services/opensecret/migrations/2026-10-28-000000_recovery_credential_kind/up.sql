ALTER TABLE user_seed_wrappings
    DROP CONSTRAINT IF EXISTS user_seed_wrappings_credential_kind_check;

ALTER TABLE user_seed_wrappings
    ADD CONSTRAINT user_seed_wrappings_credential_kind_check
    CHECK (credential_kind IN ('password', 'oauth', 'recovery'));

-- One recovery slot per user across every wrapping version. A version bump
-- replaces the slot (as rotation does); it never adds a second row.
CREATE UNIQUE INDEX idx_user_seed_wrappings_one_recovery
    ON user_seed_wrappings(user_id)
    WHERE credential_kind = 'recovery';
