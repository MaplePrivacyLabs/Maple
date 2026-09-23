/// Outcome of one Apple Passwords request.
///
/// `Selected` carries the credential the system returned. It is not logged.
#[derive(PartialEq, Eq)]
pub enum ApplePasswordDecision {
    Selected {
        username: String,
        password: String,
    },
    Cancelled,
    Unavailable,
    Failed {
        code: i64,
    },
    // Only the macOS sheet constructs this. Linux builds still compile the enum.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Rejected {
        message: &'static str,
    },
}

/// Map an `ASAuthorizationError` code onto a user-visible outcome.
///
/// 1001 is cancel. 1003–1005 are the "nothing to fill" results. Every other
/// code stays a generic failure so the numeric value can be logged without
/// the credential.
pub fn classify_apple_authorization_code(code: i64) -> ApplePasswordDecision {
    match code {
        1001 => ApplePasswordDecision::Cancelled,
        1003 | 1004 | 1005 => ApplePasswordDecision::Unavailable,
        other => ApplePasswordDecision::Failed { code: other },
    }
}

/// Accept a username and password only when both are present and bounded.
///
/// The domain is fixed by the app entitlement. This rejects empty and
/// oversized values before they cross IPC. It does not decide whether the
/// username is an email; the email form does that.
pub fn accept_apple_password(username: &str, password: &str) -> ApplePasswordDecision {
    if username.chars().any(char::is_control) {
        return ApplePasswordDecision::Unavailable;
    }
    let username = username.trim();
    if username.is_empty() || password.is_empty() || username.len() > 512 || password.len() > 1024 {
        return ApplePasswordDecision::Unavailable;
    }
    ApplePasswordDecision::Selected {
        username: username.to_string(),
        password: password.to_string(),
    }
}

pub fn apple_password_failure_message(decision: &ApplePasswordDecision) -> Option<&'static str> {
    match decision {
        ApplePasswordDecision::Unavailable => Some("No Apple Passwords entry for trymaple.ai."),
        ApplePasswordDecision::Failed { .. } => {
            Some("Apple Passwords could not be used. Try again.")
        }
        ApplePasswordDecision::Rejected { message } => Some(*message),
        ApplePasswordDecision::Selected { .. } | ApplePasswordDecision::Cancelled => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        accept_apple_password, apple_password_failure_message, classify_apple_authorization_code,
        ApplePasswordDecision,
    };

    #[test]
    fn classifies_cancel_and_missing_credentials() {
        assert!(matches!(
            classify_apple_authorization_code(1001),
            ApplePasswordDecision::Cancelled
        ));
        for code in [1003, 1004, 1005] {
            assert!(
                matches!(
                    classify_apple_authorization_code(code),
                    ApplePasswordDecision::Unavailable
                ),
                "{code}"
            );
        }
        assert!(matches!(
            classify_apple_authorization_code(1000),
            ApplePasswordDecision::Failed { code: 1000 }
        ));
    }

    #[test]
    fn accepts_a_bounded_credential_and_rejects_empty_or_huge_values() {
        match accept_apple_password("  ada@example.com  ", "fixture-password") {
            ApplePasswordDecision::Selected { username, password } => {
                assert_eq!(username, "ada@example.com");
                assert_eq!(password, "fixture-password");
            }
            ApplePasswordDecision::Cancelled
            | ApplePasswordDecision::Unavailable
            | ApplePasswordDecision::Failed { .. }
            | ApplePasswordDecision::Rejected { .. } => panic!("expected the credential"),
        }
        assert!(matches!(
            accept_apple_password("   ", "fixture-password"),
            ApplePasswordDecision::Unavailable
        ));
        assert!(matches!(
            accept_apple_password("ada@example.com", ""),
            ApplePasswordDecision::Unavailable
        ));
        assert!(matches!(
            accept_apple_password("ada@example.com\n", "fixture-password"),
            ApplePasswordDecision::Unavailable
        ));
        assert!(matches!(
            accept_apple_password(&"a".repeat(513), "fixture-password"),
            ApplePasswordDecision::Unavailable
        ));
    }

    #[test]
    fn failure_messages_do_not_include_the_credential() {
        let selected = accept_apple_password("ada@example.com", "fixture-password");
        assert!(apple_password_failure_message(&selected).is_none());
        let unavailable = apple_password_failure_message(&ApplePasswordDecision::Unavailable)
            .expect("missing-entry message");
        assert!(!unavailable.contains("fixture-password"));
        assert!(unavailable.contains("trymaple.ai"));
    }
}
