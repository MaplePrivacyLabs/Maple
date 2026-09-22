use crate::{
    models::{
        org_memberships::OrgRole,
        project_settings::{AppleOAuthSettings, OAuthProviderSettings},
    },
    web::platform::validation::{
        validate_alphanumeric_only, validate_alphanumeric_with_symbols, validate_secret_size,
    },
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use validator::Validate;

pub const PROJECT_RESEND_API_KEY: &str = "RESEND_API_KEY";
pub const PROJECT_GOOGLE_OAUTH_SECRET: &str = "GOOGLE_OAUTH_SECRET";
pub const PROJECT_GITHUB_OAUTH_SECRET: &str = "GITHUB_OAUTH_SECRET";
pub const PROJECT_APPLE_OAUTH_SECRET: &str = "APPLE_OAUTH_SECRET";
pub const THIRD_PARTY_JWT_SECRET: &str = "THIRD_PARTY_JWT_SECRET";

// Request Types
#[derive(Deserialize, Clone, Validate)]
pub struct CreateOrgRequest {
    #[validate(length(min = 1, max = 50))]
    #[validate(custom(function = "validate_alphanumeric_with_symbols"))]
    pub name: String,
}

#[derive(Deserialize, Clone, Validate)]
pub struct CreateProjectRequest {
    #[validate(length(min = 1, max = 50))]
    #[validate(custom(function = "validate_alphanumeric_with_symbols"))]
    pub name: String,
    #[validate(length(max = 255))]
    pub description: Option<String>,
}

#[derive(Deserialize, Clone, Validate)]
pub struct UpdateProjectRequest {
    #[validate(length(min = 1, max = 50))]
    #[validate(custom(function = "validate_alphanumeric_with_symbols"))]
    pub name: Option<String>,
    #[validate(length(max = 255))]
    pub description: Option<String>,
    #[validate(custom(function = "validate_project_status"))]
    pub status: Option<String>,
}

#[derive(Deserialize, Clone, Validate)]
pub struct CreateInviteRequest {
    #[validate(email(message = "Invalid email format"))]
    #[validate(length(max = 255, message = "Email must not exceed 255 characters"))]
    pub email: String,
    #[serde(default = "default_invite_role")]
    pub role: OrgRole,
}

#[derive(Deserialize, Clone, Validate)]
pub struct UpdateMembershipRequest {
    pub role: OrgRole,
}

#[derive(Deserialize, Clone, Validate)]
pub struct CreateSecretRequest {
    #[validate(length(min = 1, max = 50))]
    #[validate(custom(function = "validate_alphanumeric_only"))]
    pub key_name: String,
    #[validate(custom(function = "validate_secret_size"))]
    pub secret: String, // Base64 encoded secret value
}

#[derive(Deserialize, Clone, Validate)]
pub struct UpdateEmailSettingsRequest {
    #[validate(length(min = 1, max = 255))]
    #[validate(custom(function = "validate_email_provider"))]
    pub provider: String,
    #[validate(email)]
    pub send_from: String,
    #[validate(length(
        min = 1,
        max = 255,
        message = "URL must not be empty and must not exceed 255 characters"
    ))]
    #[validate(url(message = "Invalid URL format"))]
    pub email_verification_url: String,
}

#[derive(Deserialize, Clone, Validate)]
pub struct UpdateOAuthSettingsRequest {
    pub google_oauth_enabled: bool,
    pub github_oauth_enabled: bool,
    pub apple_oauth_enabled: bool,
    #[validate(custom(function = "validate_oauth_provider_settings"))]
    pub google_oauth_settings: Option<OAuthProviderSettings>,
    #[validate(custom(function = "validate_oauth_provider_settings"))]
    pub github_oauth_settings: Option<OAuthProviderSettings>,
    #[validate(custom(function = "validate_apple_oauth_settings"))]
    pub apple_oauth_settings: Option<AppleOAuthSettings>,
}

// Response Types
#[derive(Serialize)]
pub struct OrgResponse {
    pub id: Uuid,
    pub name: String,
}

#[derive(Serialize)]
pub struct ProjectResponse {
    pub id: Uuid,
    pub client_id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub status: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Serialize)]
pub struct MembershipResponse {
    pub user_id: Uuid,
    pub role: String,
    pub name: Option<String>,
}

#[derive(Serialize)]
pub struct InviteResponse {
    pub code: Uuid,
    pub email: String,
    pub role: String,
    pub used: bool,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Serialize)]
pub struct DetailedInviteResponse {
    pub code: Uuid,
    pub email: String,
    pub role: String,
    pub used: bool,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub organization_name: String,
}

#[derive(Serialize)]
pub struct SecretResponse {
    pub key_name: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct PlatformUserResponse {
    pub id: Uuid,
    pub email: String,
    pub name: Option<String>,
    pub email_verified: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Serialize)]
pub struct MeResponse {
    pub user: PlatformUserResponse,
    pub organizations: Vec<OrgResponse>,
}

// Validation Functions
pub fn validate_project_status(status: &str) -> Result<(), validator::ValidationError> {
    match status {
        "active" | "inactive" | "suspended" => Ok(()),
        _ => Err(validator::ValidationError::new("project_status")),
    }
}

pub fn default_invite_role() -> OrgRole {
    OrgRole::Admin
}

pub fn validate_email_provider(provider: &str) -> Result<(), validator::ValidationError> {
    if provider != "resend" {
        let mut error = validator::ValidationError::new("invalid_email_provider");
        error.message = Some("Only 'resend' is supported as an email provider".into());
        return Err(error);
    }
    Ok(())
}

const MAX_ADDITIONAL_REDIRECT_URLS: usize = 16;

fn validate_oauth_redirect_url(redirect_url: &str) -> Result<(), validator::ValidationError> {
    if redirect_url.is_empty() || redirect_url.len() > 255 {
        let mut error = validator::ValidationError::new("oauth_redirect_url");
        error.message = Some(format!("Redirect URL must not be empty and must not exceed 255 characters (current length: {})", redirect_url.len()).into());
        return Err(error);
    }
    // Preserve the existing generic URL contract, including loopback development.
    if let Err(parse_err) = url::Url::parse(redirect_url) {
        let mut error = validator::ValidationError::new("oauth_redirect_url_invalid");
        error.message = Some(format!("Invalid redirect URL: {}", parse_err).into());
        return Err(error);
    }
    Ok(())
}

fn validate_additional_redirect_urls(
    redirect_urls: Option<&[String]>,
) -> Result<(), validator::ValidationError> {
    let Some(redirect_urls) = redirect_urls else {
        return Ok(());
    };
    if redirect_urls.len() > MAX_ADDITIONAL_REDIRECT_URLS {
        let mut error = validator::ValidationError::new("oauth_additional_redirect_urls");
        error.message = Some("At most 16 additional redirect URLs are allowed".into());
        return Err(error);
    }
    for redirect_url in redirect_urls {
        validate_oauth_redirect_url(redirect_url)?;
    }
    Ok(())
}

pub fn validate_oauth_provider_settings(
    settings: &OAuthProviderSettings,
) -> Result<(), validator::ValidationError> {
    // Validate client_id
    if settings.client_id.is_empty() || settings.client_id.len() > 255 {
        let mut error = validator::ValidationError::new("oauth_client_id");
        error.message = Some(format!("Client ID must not be empty and must not exceed 255 characters (current length: {})", settings.client_id.len()).into());
        return Err(error);
    }
    validate_oauth_redirect_url(&settings.redirect_url)?;
    validate_additional_redirect_urls(settings.additional_redirect_urls.as_deref())?;

    Ok(())
}

pub fn validate_apple_oauth_settings(
    settings: &AppleOAuthSettings,
) -> Result<(), validator::ValidationError> {
    // Validate client_id
    if settings.client_id.is_empty() || settings.client_id.len() > 255 {
        let mut error = validator::ValidationError::new("oauth_client_id");
        error.message = Some(format!("Client ID must not be empty and must not exceed 255 characters (current length: {})", settings.client_id.len()).into());
        return Err(error);
    }
    validate_oauth_redirect_url(&settings.redirect_url)?;
    validate_additional_redirect_urls(settings.additional_redirect_urls.as_deref())?;
    if let Some(client_ids) = &settings.additional_native_client_ids {
        if client_ids.len() > 16
            || client_ids.iter().any(|client_id| {
                client_id.is_empty()
                    || client_id.len() > 255
                    || client_id.split('.').any(str::is_empty)
                    || !client_id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
            })
        {
            return Err(validator::ValidationError::new("apple_native_client_ids"));
        }
    }

    // Validate team_id if provided
    if let Some(ref team_id) = settings.team_id {
        if team_id.len() != 10 {
            let mut error = validator::ValidationError::new("oauth_team_id_length");
            error.message = Some(
                format!(
                    "Apple Team ID should be exactly 10 characters long (current length: {})",
                    team_id.len()
                )
                .into(),
            );
            return Err(error);
        }
    }

    // Validate key_id if provided
    if let Some(ref key_id) = settings.key_id {
        if key_id.len() != 10 {
            let mut error = validator::ValidationError::new("oauth_key_id_length");
            error.message = Some(
                format!(
                    "Apple Key ID should be exactly 10 characters long (current length: {})",
                    key_id.len()
                )
                .into(),
            );
            return Err(error);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn oauth_request(provider: &str, additions: Option<Value>) -> Value {
        let mut settings = json!({
            "client_id": "customer-client",
            "redirect_url": "https://customer.example/callback"
        });
        if let Some(additions) = additions {
            settings["additional_redirect_urls"] = additions;
        }
        let mut request = json!({
            "google_oauth_enabled": false,
            "github_oauth_enabled": false,
            "apple_oauth_enabled": false
        });
        request[format!("{provider}_oauth_enabled")] = json!(true);
        request[format!("{provider}_oauth_settings")] = settings;
        request
    }

    #[test]
    fn oauth_redirect_lists_accept_legacy_null_empty_and_generic_urls() {
        for provider in ["google", "github", "apple"] {
            for additions in [
                None,
                Some(Value::Null),
                Some(json!([])),
                Some(json!([
                    "http://127.0.0.1:5173/callback",
                    "https://dev.secretgpt.ai/callback",
                    "https://preview.opensecret.cloud/callback",
                    "https://customer.example/callback"
                ])),
            ] {
                let request: UpdateOAuthSettingsRequest =
                    serde_json::from_value(oauth_request(provider, additions)).unwrap();
                assert!(request.validate().is_ok(), "provider: {provider}");
            }
        }
    }

    #[test]
    fn oauth_redirect_lists_enforce_count_and_per_url_bounds() {
        for provider in ["google", "github", "apple"] {
            let at_limit = vec!["https://customer.example/callback"; MAX_ADDITIONAL_REDIRECT_URLS];
            let request: UpdateOAuthSettingsRequest =
                serde_json::from_value(oauth_request(provider, Some(json!(at_limit)))).unwrap();
            assert!(request.validate().is_ok());

            for additions in [
                json!(vec![
                    "https://customer.example/callback";
                    MAX_ADDITIONAL_REDIRECT_URLS + 1
                ]),
                json!([""]),
                json!(["/relative/callback"]),
                json!([format!("https://customer.example/{}", "x".repeat(256))]),
            ] {
                let request: UpdateOAuthSettingsRequest =
                    serde_json::from_value(oauth_request(provider, Some(additions))).unwrap();
                assert!(request.validate().is_err(), "provider: {provider}");
            }
        }
    }

    #[test]
    fn oauth_redirect_lists_reject_wrong_json_types() {
        for provider in ["google", "github", "apple"] {
            for additions in [json!("https://customer.example/callback"), json!([42])] {
                assert!(
                    serde_json::from_value::<UpdateOAuthSettingsRequest>(oauth_request(
                        provider,
                        Some(additions)
                    ))
                    .is_err()
                );
            }
        }
    }

    #[test]
    fn apple_native_audience_settings_validate_exact_bounded_identifiers() {
        for (value, valid) in [
            (Value::Null, true),
            (json!([]), true),
            (json!(["com.example.dev", "com.example.App-Beta"]), true),
            (json!(vec!["com.example.dev"; 16]), true),
            (json!(vec!["com.example.dev"; 17]), false),
            (json!([""]), false),
            (json!(["com.example.*"]), false),
            (json!(["com.example.dev "]), false),
            (json!(["com..dev"]), false),
            (json!(["https://com.example.dev"]), false),
            (json!(["com.éxample.dev"]), false),
            (json!(["x".repeat(256)]), false),
        ] {
            let mut request_json = oauth_request("apple", None);
            request_json["apple_oauth_settings"]["additional_native_client_ids"] = value;
            let request: UpdateOAuthSettingsRequest = serde_json::from_value(request_json).unwrap();
            assert_eq!(request.validate().is_ok(), valid);
        }
        for value in [json!("com.example.dev"), json!([42])] {
            let mut request_json = oauth_request("apple", None);
            request_json["apple_oauth_settings"]["additional_native_client_ids"] = value;
            assert!(serde_json::from_value::<UpdateOAuthSettingsRequest>(request_json).is_err());
        }
    }
}
