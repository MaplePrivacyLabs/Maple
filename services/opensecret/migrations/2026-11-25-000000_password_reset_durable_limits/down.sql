DROP INDEX idx_password_reset_requests_user_id_created_at;
DROP INDEX idx_platform_password_reset_requests_user_id_created_at;

ALTER TABLE platform_password_reset_requests
    DROP COLUMN failed_attempts;

ALTER TABLE password_reset_requests
    DROP COLUMN failed_attempts;
