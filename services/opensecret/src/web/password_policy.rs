//! Account password policy enforced before any password write effect.
//!
//! The Maple clients enforce a minimum password length at their own
//! boundaries (Research web/desktop signup and reset forms, Agent
//! change-password), and the platform routes already enforced 8–64 via the
//! `validator` crate. The backend previously accepted any string, including
//! the empty string, at every account-password boundary; this module is the
//! shared server-side rule that closes that gap for user accounts with the
//! same 8–64 window.

/// Minimum accepted password length, matching the Maple clients' rule of at
/// least eight characters.
pub(crate) const MIN_PASSWORD_CHARS: usize = 8;

/// Maximum accepted password length, matching the platform routes' existing
/// rule of at most sixty-four characters.
pub(crate) const MAX_PASSWORD_CHARS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum PasswordPolicyError {
    #[error("Password must be at least {MIN_PASSWORD_CHARS} characters")]
    TooShort,
    #[error("Password must not exceed {MAX_PASSWORD_CHARS} characters")]
    TooLong,
}

impl From<PasswordPolicyError> for crate::ApiError {
    fn from(error: PasswordPolicyError) -> Self {
        match error {
            PasswordPolicyError::TooShort => Self::PasswordTooShort,
            PasswordPolicyError::TooLong => Self::PasswordTooLong,
        }
    }
}

/// Validate a new password at an account-password write boundary
/// (registration, password reset, password change). Login is intentionally
/// not validated: a wrong-length login attempt is simply an authentication
/// failure and must not reveal policy state.
pub(crate) fn validate_password(password: &str) -> Result<(), PasswordPolicyError> {
    // Count Unicode scalar values, matching the Agent client's
    // `chars().count()` rule rather than byte length.
    let chars = password.chars().count();
    if chars < MIN_PASSWORD_CHARS {
        return Err(PasswordPolicyError::TooShort);
    }
    if chars > MAX_PASSWORD_CHARS {
        return Err(PasswordPolicyError::TooLong);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_passwords_at_or_above_the_minimum() {
        assert_eq!(validate_password("12345678"), Ok(()));
        assert_eq!(validate_password("long enough password"), Ok(()));
    }

    #[test]
    fn rejects_empty_and_short_passwords() {
        assert_eq!(validate_password(""), Err(PasswordPolicyError::TooShort));
        assert_eq!(
            validate_password("1234567"),
            Err(PasswordPolicyError::TooShort)
        );
    }

    #[test]
    fn accepts_passwords_at_or_below_the_maximum() {
        assert_eq!(validate_password(&"x".repeat(64)), Ok(()));
    }

    #[test]
    fn rejects_passwords_above_the_maximum() {
        assert_eq!(
            validate_password(&"x".repeat(65)),
            Err(PasswordPolicyError::TooLong)
        );
    }

    #[test]
    fn counts_unicode_characters_not_bytes() {
        // Eight two-byte characters: 16 bytes but 8 characters.
        assert_eq!(validate_password("áááááááá"), Ok(()));
        // Seven characters that are longer in bytes must still be rejected.
        assert_eq!(
            validate_password("ááááááá"),
            Err(PasswordPolicyError::TooShort)
        );
    }
}
