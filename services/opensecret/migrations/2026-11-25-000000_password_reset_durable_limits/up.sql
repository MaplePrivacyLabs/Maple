-- Durable, database-backed limits for the password-reset email codes
-- (issue #1486). One failed-attempt counter per reset request row makes the
-- guess budget cross-process: charging is a guarded UPDATE scoped to the
-- request's own secret, so nobody can spend a budget belonging to a request
-- they did not start. The email cap is the per-account count of unconsumed
-- requests created inside the rolling hour; the creation transaction refuses
-- to add a row once the cap is reached, and every created request row means
-- exactly one sent email.
ALTER TABLE password_reset_requests
    ADD COLUMN failed_attempts INTEGER NOT NULL DEFAULT 0;

ALTER TABLE platform_password_reset_requests
    ADD COLUMN failed_attempts INTEGER NOT NULL DEFAULT 0;

-- The rolling-hour email cap counts a account's recent request rows.
CREATE INDEX idx_password_reset_requests_user_id_created_at
    ON password_reset_requests(user_id, created_at);

CREATE INDEX idx_platform_password_reset_requests_user_id_created_at
    ON platform_password_reset_requests(platform_user_id, created_at);
