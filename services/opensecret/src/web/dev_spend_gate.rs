//! Hosted Dev is reachable directly, outside the Access-protected web app, and
//! anyone can register there. Routes that spend provider or relay money are
//! limited to internal accounts in Dev; every other mode passes through.
use crate::{db::DBError, models::users::User, ApiError, AppMode, AppState};
use axum::{
    extract::{Request, State},
    middleware::Next,
    response::{IntoResponse, Response},
    Extension,
};
use std::sync::Arc;
use tracing::error;

const INTERNAL_EMAIL_DOMAINS: &[&str] = &["maple.ai", "opensecret.cloud"];

/// Per-user os-flags override for internal testers without an internal email,
/// such as guest accounts or Apple private-relay sign-ins.
const DEV_SPEND_FLAG_KEY: &str = "dev_spend";

/// Runs after JWT or API-key authentication has attached the `User`.
pub(crate) async fn require_internal(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    request: Request,
    next: Next,
) -> Response {
    if state.app_mode != AppMode::Dev {
        return next.run(request).await;
    }
    match is_internal_account(&state, &user).await {
        Ok(true) => next.run(request).await,
        Ok(false) => ApiError::InternalAccountRequired.into_response(),
        Err(error) => error.into_response(),
    }
}

async fn is_internal_account(state: &AppState, user: &User) -> Result<bool, ApiError> {
    if user.get_email().is_some_and(is_internal_email) {
        // A password signup can claim any address, so require verification.
        match state.db.get_email_verification_by_user_id(user.uuid) {
            Ok(verification) if verification.is_verified => return Ok(true),
            Ok(_) | Err(DBError::EmailVerificationNotFound) => {}
            Err(e) => {
                error!("Error checking email verification: {:?}", e);
                return Err(ApiError::InternalServerError);
            }
        }
    }
    Ok(state
        .is_feature_enabled(user.uuid, DEV_SPEND_FLAG_KEY)
        .await)
}

fn is_internal_email(email: &str) -> bool {
    email.rsplit_once('@').is_some_and(|(local, domain)| {
        !local.is_empty()
            && INTERNAL_EMAIL_DOMAINS
                .iter()
                .any(|internal| domain.eq_ignore_ascii_case(internal))
    })
}

#[cfg(test)]
mod tests {
    use super::is_internal_email;

    #[test]
    fn internal_email_requires_an_exact_internal_domain() {
        assert!(is_internal_email("anthony@maple.ai"));
        assert!(is_internal_email("Anthony@Maple.AI"));
        assert!(is_internal_email("dev@opensecret.cloud"));

        assert!(!is_internal_email("@maple.ai"));
        assert!(!is_internal_email("maple.ai"));
        assert!(!is_internal_email("someone@gmail.com"));
        assert!(!is_internal_email("someone@evilmaple.ai"));
        assert!(!is_internal_email("someone@maple.ai.evil.com"));
        assert!(!is_internal_email("someone@sub.maple.ai"));
        assert!(!is_internal_email("someone@maple.ai@gmail.com"));
    }
}
