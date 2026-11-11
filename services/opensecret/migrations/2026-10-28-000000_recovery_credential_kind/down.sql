DROP INDEX IF EXISTS idx_user_seed_wrappings_one_recovery;

ALTER TABLE user_seed_wrappings
    DROP CONSTRAINT IF EXISTS user_seed_wrappings_credential_kind_check;

-- Re-adding the narrower CHECK validates existing rows, so any enrolled
-- recovery wrap would abort the rollback. Drop them first: a re-deploy of the
-- code cannot open them anyway, and re-enrolling issues a fresh code.
DELETE FROM user_seed_wrappings
    WHERE credential_kind = 'recovery';

ALTER TABLE user_seed_wrappings
    ADD CONSTRAINT user_seed_wrappings_credential_kind_check
    CHECK (credential_kind IN ('password', 'oauth'));
