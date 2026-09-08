use crate::email::{
    send_account_deletion_confirmation_email, send_account_deletion_email,
    send_password_reset_confirmation_email, send_password_reset_email,
    send_platform_password_reset_confirmation_email, send_platform_password_reset_email,
};
use crate::encrypt::encrypt_key_deterministic;
use crate::encrypt::generate_random;
use crate::encrypt::{
    decrypt_with_key, decrypt_with_kms, encrypt_with_key, validate_aes_256_key_length, CustomRng,
    GenKeyResult,
};
use crate::jwt::validate_platform_jwt;
use crate::login_routes::RegisterCredentials;
use crate::model_config::{ModelAliasTargets, ModelPlan, PaidModelAliasOverrides};
use crate::models::account_deletion::{AccountDeletionError, NewAccountDeletionRequest};
use crate::models::email_verification::{EmailVerificationError, NewEmailVerification};
use crate::models::oauth::{NewUserOAuthConnection, OAuthError};
use crate::models::password_reset::NewPasswordResetRequest;
use crate::models::platform_password_reset::NewPlatformPasswordResetRequest;
use crate::models::platform_users::PlatformUser;
use crate::sqs::SqsEventPublisher;
use crate::web::openai_auth::{validate_openai_auth, validate_optional_openai_auth};
use crate::web::platform_login_routes;
use crate::web::{
    conversation_projects_routes, conversations_routes, health_routes, instructions_routes,
    login_routes, native_handoff_routes, oauth_routes, openai_models_routes, openai_routes,
    protected_routes, recovery_router, responses_routes, web_routes,
};
use crate::{attestation_routes::SessionState, web::platform_routes};
use bounded_ttl_cache::BoundedTtlCache;
use lease_aware_cache::{CacheLease, InsertError as SessionInsertError, LeaseAwareTtlCache};

use crate::jwt::{AuthContext, AuthMethod};
use crate::models::user_seed_wrappings::{NewUserSeedWrapping, UserSeedWrappingError};
use crate::seed_wrapping::{
    compute_oauth_auth_binding, compute_password_auth_binding, decrypt_seed_v1, encrypt_seed_v1,
    normalize_email_login_identifier, normalize_guest_login_identifier,
    oauth_credential_lookup_hash, password_credential_lookup_hash, password_reset_code_mac,
    AuthBinding, CredentialKind, PasswordLoginIdentifierKind, SEED_WRAP_VERSION_V1,
};
use crate::{
    aws_credentials::AwsCredentialError,
    models::enclave_secrets::NewEnclaveSecret,
    private_key::{generate_twelve_word_seed, plaintext_user_seed_to_key},
};
use crate::{
    billing::{BillingClient, ChatBillingAccess},
    web::platform::common::PROJECT_RESEND_API_KEY,
};
use crate::{
    db::{setup_db, DBConnection, DBError},
    models::users::{NewUser, User, UserError},
};
use crate::{encrypt::create_new_encryption_key, jwt::validate_jwt};
use aws_credentials::{AwsCredentialManager, AwsCredentials};
use axum::{
    http::{header, HeaderName, HeaderValue, StatusCode},
    middleware::{from_fn, from_fn_with_state, Next},
    response::{IntoResponse, Response},
    Json, Router,
};
use base64::engine::general_purpose;
use base64::Engine as _;
use chacha20poly1305::aead::Aead;
use chacha20poly1305::KeyInit;
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use diesel::Connection;
use kv::{KVPair, StoreError, StoreResult};
use password_auth::{generate_hash, verify_password, VerifyError};
use rand_core::{CryptoRng, RngCore};
use secp256k1::SecretKey;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::env;
use std::fmt;
use std::io::Write;
use std::num::NonZeroUsize;
use std::str::FromStr;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use subtle::ConstantTimeEq;
use tokio::spawn;
use tokio::sync::RwLock;
use tokio::task::{self};
use tower_http::cors::{Any, CorsLayer};
use tracing::{debug, error, info, trace, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
use url::Url;
use uuid::Uuid;
use vsock::{VsockAddr, VsockStream};
use web::attestation_routes;
use x25519_dalek::{EphemeralSecret, PublicKey};

mod apple_signin;
mod aws_credentials;
mod billing;
mod bounded_ttl_cache;
#[cfg(test)]
mod crypto_property_tests;
mod db;
mod email;
mod encrypt;
mod http_client;
mod inference;
mod inference_planning;
mod jwt;
mod kagi;
mod kv;
mod lease_aware_cache;
mod message_signing;
mod migrations;
mod model_config;
mod models;
mod oauth;
mod os_flags;
mod private_key;
#[allow(dead_code)] // Used by the dormant Transport V2 core in this stack layer.
mod provider_cache;
mod provider_client;
mod provider_registry;
mod provider_routing;
mod proxy_config;
mod recovery_code;
mod secret_cache_maintenance;
#[cfg(test)]
mod security_invariants;
mod seed_wrapping;
mod sqs;
mod tokens;
#[allow(dead_code)] // Includes client-side vector helpers shared with the SDK implementation.
mod transport_v2;
mod web;

#[cfg(test)]
mod aead_db_tamper_tests;
#[cfg(test)]
mod recovery_db_tests;
#[cfg(test)]
mod recovery_route_tests;

use apple_signin::AppleJwtVerifier;
use inference_planning::ProviderPreference;
use oauth::{AppleProvider, GithubProvider, GoogleProvider, OAuthManager};
use provider_client::{ProviderClient, ProviderRequestError};
use provider_routing::{InferenceRoutingMode, ProviderRouter};
use proxy_config::ProxyRouter;

const ENCLAVE_KEY_NAME: &str = "enclave_key";
const OPENAI_API_KEY_NAME: &str = "openai_api_key";
const JWT_SECRET_KEY_NAME: &str = "jwt_secret";

// General secret key names
const GITHUB_CLIENT_ID_NAME: &str = "github_client_id";
const GITHUB_CLIENT_SECRET_NAME: &str = "github_client_secret";
const GOOGLE_CLIENT_ID_NAME: &str = "google_client_id";
const GOOGLE_CLIENT_SECRET_NAME: &str = "google_client_secret";
const RESEND_API_KEY_NAME: &str = "resend_api_key";

const BILLING_API_KEY_NAME: &str = "billing_api_key";
const BILLING_SERVER_URL_NAME: &str = "billing_server_url";
const KAGI_API_KEY_NAME: &str = "kagi_api_key";
const OS_FLAGS_API_KEY_NAME: &str = "os_flags_api_key";
const OS_FLAGS_BASE_URL_NAME: &str = "os_flags_base_url";
const MODEL_ALIAS_FLAGS_TIMEOUT_SECS: u64 = 5;
const PROVIDER_ROUTING_FLAGS_TIMEOUT_SECS: u64 = 5;
const ROUTING_FLAGS_FAILURE_BACKOFF_SECS: u64 = 30;
const BILLING_ACCESS_TIMEOUT_SECS: u64 = 5;
const MAX_ATTESTATION_NONCE_BYTES: usize = 512;
const MAX_PENDING_ATTESTATIONS: usize = 65_536;
const PENDING_ATTESTATION_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_ENCRYPTION_SESSIONS: usize = 2_097_152;
const ENCRYPTION_SESSION_IDLE_TTL: Duration = Duration::from_secs(65 * 60);

pub(crate) const ERROR_CONTRACT_HEADER: &str = "x-opensecret-error-contract";
pub(crate) const ERROR_CODE_HEADER: &str = "x-opensecret-error-code";
pub(crate) const CLIENT_REPLAY_HEADER: &str = "x-opensecret-client-replay";
pub(crate) const ERROR_CONTRACT_VERSION: &str = "1";
const SESSION_NOT_FOUND_ERROR_CODE: &str = "session_not_found";
const ACCESS_TOKEN_EXPIRED_ERROR_CODE: &str = "access_token_expired";
const IMAGE_DESCRIPTION_UNAVAILABLE_ERROR_CODE: &str = "image_description_unavailable";
pub(crate) const INFERENCE_CAPACITY_ERROR_CODE: &str = "inference_capacity";
const CLIENT_REPLAY_SAFE: &str = "safe";

type PendingAttestationKey = [u8; 32];
pub(crate) type SessionLease = CacheLease<SessionState>;

fn validate_attestation_nonce(nonce: &str) -> Result<(), ApiError> {
    if nonce.len() > MAX_ATTESTATION_NONCE_BYTES {
        return Err(ApiError::BadRequest);
    }
    Ok(())
}

fn attestation_nonce_key(nonce: &str) -> PendingAttestationKey {
    Sha256::digest(nonce.as_bytes()).into()
}

#[cfg(test)]
mod pending_attestation_key_tests {
    use super::*;

    #[test]
    fn nonce_limit_is_measured_in_bytes() {
        assert!(validate_attestation_nonce(&"a".repeat(512)).is_ok());
        assert!(matches!(
            validate_attestation_nonce(&"a".repeat(513)),
            Err(ApiError::BadRequest)
        ));

        assert!(validate_attestation_nonce(&"é".repeat(256)).is_ok());
        assert!(matches!(
            validate_attestation_nonce(&"é".repeat(257)),
            Err(ApiError::BadRequest)
        ));
    }

    #[test]
    fn cache_key_is_a_fixed_sha256_digest() {
        let first_nonce = Uuid::new_v4().to_string();
        let different_nonce = Uuid::new_v4().to_string();
        let first = attestation_nonce_key(&first_nonce);

        assert_eq!(first, attestation_nonce_key(&first_nonce));
        assert_ne!(first, attestation_nonce_key(&different_nonce));
        assert_eq!(std::mem::size_of_val(&first), 32);

        let expected: PendingAttestationKey = Sha256::digest(first_nonce.as_bytes()).into();
        assert_eq!(first, expected);
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EnclaveRequest {
    pub request_type: String,
    pub key_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ParentResponse {
    pub response_type: String,
    pub response_value: serde_json::Value,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    TaskJoin(#[from] task::JoinError),

    #[error(transparent)]
    StdIo(#[from] std::io::Error),

    #[error(transparent)]
    TryInit(#[from] tracing_subscriber::util::TryInitError),

    #[error("failed to install the required Ring crypto provider")]
    CryptoProvider,

    #[error("Database error: {0}")]
    DatabaseError(#[from] DBError),

    #[error("Private key not found")]
    PrivateKeyNotFound,

    #[error("Private key could not be generated")]
    PrivateKeyGenerationFailure,

    #[error("User not found")]
    UserNotFound,

    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),

    #[error("Encryption error: {0}")]
    EncryptionError(String),

    #[error("Authentication error")]
    AuthenticationError,

    #[error("Failed to parse secret")]
    SecretParsingError,

    #[error("AWS Credential error: {0}")]
    AwsCredentialError(#[from] aws_credentials::AwsCredentialError),

    #[error("User is already verified")]
    UserAlreadyVerified,

    #[error("Builder error: {0}")]
    BuilderError(String),

    #[error("Password reset request expired")]
    PasswordResetExpired,

    #[error("Invalid password reset secret")]
    InvalidPasswordResetSecret,

    #[error("Invalid password reset request")]
    InvalidPasswordResetRequest,

    #[error("Account deletion request expired")]
    AccountDeletionExpired,

    #[error("Invalid account deletion secret")]
    InvalidAccountDeletionSecret,

    #[error("Invalid account deletion request")]
    InvalidAccountDeletionRequest,

    #[error("Password verification error: {0}")]
    PasswordVerificationError(#[from] VerifyError),

    #[error("Password is required for registration")]
    PasswordRequired,

    #[error("OAuth error: {0}")]
    OAuthError(String),

    #[error("User with this email already exists")]
    UserAlreadyExists,

    #[error("Signing error: {0}")]
    SigningError(String),

    #[error("Invalid derivation path: {0}")]
    InvalidDerivationPath(String),

    #[error("Key derivation failed: {0}")]
    KeyDerivationError(String),
}

#[derive(Debug, thiserror::Error)]
enum CreateUserSeedWrapTransactionError {
    #[error("Diesel error: {0}")]
    Diesel(#[from] diesel::result::Error),
    #[error("User error: {0}")]
    User(#[from] UserError),
    #[error("User seed wrapping error: {0}")]
    UserSeedWrapping(#[from] UserSeedWrappingError),
    #[error("OAuth error: {0}")]
    OAuth(#[from] OAuthError),
    #[error("Email verification error: {0}")]
    EmailVerification(#[from] EmailVerificationError),
    #[error("Application error: {0}")]
    App(#[from] Error),
}

impl From<CreateUserSeedWrapTransactionError> for Error {
    fn from(error: CreateUserSeedWrapTransactionError) -> Self {
        match error {
            CreateUserSeedWrapTransactionError::Diesel(e) => {
                Error::DatabaseError(DBError::QueryError(e))
            }
            CreateUserSeedWrapTransactionError::User(e) => {
                Error::DatabaseError(DBError::UserError(e))
            }
            CreateUserSeedWrapTransactionError::UserSeedWrapping(e) => {
                Error::DatabaseError(DBError::UserSeedWrappingError(e))
            }
            CreateUserSeedWrapTransactionError::OAuth(e) => {
                Error::DatabaseError(DBError::OAuthError(e))
            }
            CreateUserSeedWrapTransactionError::EmailVerification(e) => {
                Error::DatabaseError(DBError::EmailVerificationError(e))
            }
            CreateUserSeedWrapTransactionError::App(e) => e,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("Invalid email, password, or login method")]
    InvalidUsernameOrPassword,

    #[error("Invalid JWT")]
    InvalidJwt,

    #[error("Invalid JWT")]
    AccessTokenExpired,

    #[error("Internal server error")]
    InternalServerError,

    #[error("Upstream provider temporarily unavailable")]
    ServiceUnavailable,

    #[error("Upstream provider temporarily unavailable")]
    ImageDescriptionUnavailable,
    #[error("Inference capacity is temporarily unavailable")]
    InferenceCapacity {
        status: StatusCode,
        retry_after: Option<Duration>,
        client_replay_safe: bool,
    },

    #[error("Bad Request")]
    BadRequest,

    #[error("Bad Request")]
    SessionNotFound,

    #[error("Conflict")]
    Conflict,

    #[error("Encryption error")]
    EncryptionError,

    #[error("Unauthorized")]
    Unauthorized,

    #[error("Invalid invite code")]
    InvalidInviteCode,

    #[error("Token refresh failed")]
    RefreshFailed,

    #[error("User is already verified")]
    UserAlreadyVerified,

    #[error("No valid email found for the Oauth account")]
    NoEmailFound,

    #[error("User exists but Oauth not linked")]
    UserExistsNotLinked,

    #[error("User not found")]
    UserNotFound,

    #[error("Email already registered")]
    EmailAlreadyExists,

    #[error("Usage limit reached")]
    UsageLimitReached,

    #[error("Free tier token limit exceeded")]
    FreeTokenLimitExceeded,

    #[error("Model not available on current plan")]
    ModelNotAvailableOnPlan,

    #[error("Message exceeds context limit")]
    MessageExceedsContextLimit,

    #[error("Resource not found")]
    NotFound,

    #[error("Unprocessable entity")]
    UnprocessableEntity,

    #[error("Payload too large")]
    PayloadTooLarge,

    #[error("Too many requests")]
    TooManyRequests,
}

impl ApiError {
    pub(crate) fn with_client_replay_safe(mut self) -> Self {
        if let Self::InferenceCapacity {
            client_replay_safe, ..
        } = &mut self
        {
            *client_replay_safe = true;
        }
        self
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let status = match &self {
            ApiError::InvalidUsernameOrPassword => StatusCode::UNAUTHORIZED,
            ApiError::InvalidJwt => StatusCode::UNAUTHORIZED,
            ApiError::AccessTokenExpired => StatusCode::UNAUTHORIZED,
            ApiError::Unauthorized => StatusCode::UNAUTHORIZED,
            ApiError::InternalServerError => StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::ServiceUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            ApiError::ImageDescriptionUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            ApiError::InferenceCapacity { status, .. } => *status,
            ApiError::BadRequest => StatusCode::BAD_REQUEST,
            ApiError::SessionNotFound => StatusCode::BAD_REQUEST,
            ApiError::Conflict => StatusCode::CONFLICT,
            ApiError::InvalidInviteCode => StatusCode::UNAUTHORIZED,
            ApiError::RefreshFailed => StatusCode::UNAUTHORIZED,
            ApiError::UserAlreadyVerified => StatusCode::BAD_REQUEST,
            ApiError::EncryptionError => StatusCode::BAD_REQUEST,
            ApiError::NoEmailFound => StatusCode::BAD_REQUEST,
            ApiError::UserExistsNotLinked => StatusCode::CONFLICT,
            ApiError::UserNotFound => StatusCode::NOT_FOUND,
            ApiError::EmailAlreadyExists => StatusCode::CONFLICT,
            ApiError::UsageLimitReached => StatusCode::FORBIDDEN,
            ApiError::FreeTokenLimitExceeded => StatusCode::FORBIDDEN,
            ApiError::ModelNotAvailableOnPlan => StatusCode::FORBIDDEN,
            ApiError::MessageExceedsContextLimit => StatusCode::PAYLOAD_TOO_LARGE,
            ApiError::NotFound => StatusCode::NOT_FOUND,
            ApiError::UnprocessableEntity => StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            ApiError::TooManyRequests => StatusCode::TOO_MANY_REQUESTS,
        };
        let error_code = match &self {
            ApiError::SessionNotFound => Some(SESSION_NOT_FOUND_ERROR_CODE),
            ApiError::AccessTokenExpired => Some(ACCESS_TOKEN_EXPIRED_ERROR_CODE),
            ApiError::ImageDescriptionUnavailable => Some(IMAGE_DESCRIPTION_UNAVAILABLE_ERROR_CODE),
            ApiError::InferenceCapacity { .. } => Some(INFERENCE_CAPACITY_ERROR_CODE),
            _ => None,
        };
        let mut response = (
            status,
            Json(ErrorResponse {
                status: status.as_u16(),
                message: self.to_string(),
            }),
        )
            .into_response();
        response.headers_mut().insert(
            ERROR_CONTRACT_HEADER,
            HeaderValue::from_static(ERROR_CONTRACT_VERSION),
        );
        if let Some(error_code) = error_code {
            response
                .headers_mut()
                .insert(ERROR_CODE_HEADER, HeaderValue::from_static(error_code));
        }
        if let ApiError::InferenceCapacity {
            retry_after,
            client_replay_safe,
            ..
        } = self
        {
            if client_replay_safe {
                response.headers_mut().insert(
                    CLIENT_REPLAY_HEADER,
                    HeaderValue::from_static(CLIENT_REPLAY_SAFE),
                );
            }
            if let Some(retry_after) = retry_after {
                if let Ok(value) = HeaderValue::from_str(&retry_after.as_secs().to_string()) {
                    response.headers_mut().insert(header::RETRY_AFTER, value);
                }
            }
        }
        response
    }
}

async fn add_error_contract_header(request: axum::extract::Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        ERROR_CONTRACT_HEADER,
        HeaderValue::from_static(ERROR_CONTRACT_VERSION),
    );
    response
}

impl From<ProviderRequestError> for ApiError {
    fn from(error: ProviderRequestError) -> Self {
        match error {
            ProviderRequestError::TinfoilUnavailable => Self::ServiceUnavailable,
            ProviderRequestError::Timeout(_)
            | ProviderRequestError::Build(_)
            | ProviderRequestError::Connect(_)
            | ProviderRequestError::Send(_)
            | ProviderRequestError::Upstream(_) => Self::InternalServerError,
        }
    }
}

impl From<DBError> for ApiError {
    fn from(err: DBError) -> Self {
        error!("Database error: {:?}", err);
        match err {
            DBError::PlatformUserNotFound => ApiError::UserNotFound,
            DBError::PlatformUserError(_) => ApiError::InternalServerError,
            DBError::OrgMembershipNotFound => ApiError::NotFound,
            DBError::OrgMembershipError(_) => ApiError::InternalServerError,
            _ => ApiError::InternalServerError,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    status: u16,
    message: String,
}

#[cfg(test)]
mod api_error_contract_tests {
    use super::*;

    async fn assert_error_response(
        error: ApiError,
        expected_status: StatusCode,
        expected_body: &[u8],
        expected_code: Option<&str>,
    ) {
        let response = error.into_response();

        assert_eq!(response.status(), expected_status);
        assert_eq!(
            response.headers().get(ERROR_CONTRACT_HEADER).unwrap(),
            ERROR_CONTRACT_VERSION
        );
        assert_eq!(
            response
                .headers()
                .get(ERROR_CODE_HEADER)
                .map(|value| value.to_str().unwrap()),
            expected_code
        );

        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), expected_body);
    }

    #[tokio::test]
    async fn session_not_found_adds_code_without_changing_legacy_response() {
        assert_error_response(
            ApiError::SessionNotFound,
            StatusCode::BAD_REQUEST,
            br#"{"status":400,"message":"Bad Request"}"#,
            Some(SESSION_NOT_FOUND_ERROR_CODE),
        )
        .await;
    }

    #[tokio::test]
    async fn access_token_expired_adds_code_without_changing_legacy_response() {
        assert_error_response(
            ApiError::AccessTokenExpired,
            StatusCode::UNAUTHORIZED,
            br#"{"status":401,"message":"Invalid JWT"}"#,
            Some(ACCESS_TOKEN_EXPIRED_ERROR_CODE),
        )
        .await;
    }

    #[tokio::test]
    async fn image_description_unavailable_adds_code_without_changing_legacy_response() {
        assert_error_response(
            ApiError::ImageDescriptionUnavailable,
            StatusCode::SERVICE_UNAVAILABLE,
            br#"{"status":503,"message":"Upstream provider temporarily unavailable"}"#,
            Some(IMAGE_DESCRIPTION_UNAVAILABLE_ERROR_CODE),
        )
        .await;
    }

    #[tokio::test]
    async fn generic_service_unavailable_remains_uncoded() {
        assert_error_response(
            ApiError::ServiceUnavailable,
            StatusCode::SERVICE_UNAVAILABLE,
            br#"{"status":503,"message":"Upstream provider temporarily unavailable"}"#,
            None,
        )
        .await;
    }

    #[tokio::test]
    async fn generic_bad_request_and_invalid_jwt_are_not_recoverable() {
        assert_error_response(
            ApiError::BadRequest,
            StatusCode::BAD_REQUEST,
            br#"{"status":400,"message":"Bad Request"}"#,
            None,
        )
        .await;
        assert_error_response(
            ApiError::InvalidJwt,
            StatusCode::UNAUTHORIZED,
            br#"{"status":401,"message":"Invalid JWT"}"#,
            None,
        )
        .await;
    }

    #[tokio::test]
    async fn inference_capacity_contract_is_sanitized_and_replay_is_explicit() {
        let response = ApiError::InferenceCapacity {
            status: StatusCode::TOO_MANY_REQUESTS,
            retry_after: Some(Duration::from_secs(9)),
            client_replay_safe: true,
        }
        .into_response();

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[ERROR_CONTRACT_HEADER], "1");
        assert_eq!(
            response.headers()[ERROR_CODE_HEADER],
            INFERENCE_CAPACITY_ERROR_CODE
        );
        assert_eq!(response.headers()[CLIENT_REPLAY_HEADER], "safe");
        assert_eq!(response.headers()[header::RETRY_AFTER], "9");
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(
            body.as_ref(),
            br#"{"status":429,"message":"Inference capacity is temporarily unavailable"}"#
        );

        let response = ApiError::InferenceCapacity {
            status: StatusCode::SERVICE_UNAVAILABLE,
            retry_after: None,
            client_replay_safe: false,
        }
        .into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(response.headers().get(CLIENT_REPLAY_HEADER).is_none());
        assert!(response.headers().get(header::RETRY_AFTER).is_none());
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenClaims {
    // Subject (whom token refers to)
    pub sub: String, // This will now be the UUID
    // Expiration time (as UTC timestamp)
    pub exp: i64,
    // Issued at (as UTC timestamp)
    pub iat: i64,
    // Audience
    pub aud: String,
}

#[derive(Debug, Clone)]
pub struct Config {
    jwt_keys: jwt::JwtKeys,
    access_token_maxage: i64,
    refresh_token_maxage: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AppMode {
    Local,
    Dev,
    Preview,
    Prod,
    Custom(String),
}

impl AppMode {
    fn frontend_url(&self) -> &str {
        match self {
            AppMode::Local => "http://127.0.0.1:5173",
            AppMode::Dev => "https://dev.secretgpt.ai",
            AppMode::Preview => "https://preview.opensecret.cloud",
            AppMode::Prod => "https://trymaple.ai",
            AppMode::Custom(_) => "https://preview.opensecret.cloud",
        }
    }
}

impl fmt::Display for AppMode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            AppMode::Local => write!(f, "local"),
            AppMode::Dev => write!(f, "dev"),
            AppMode::Preview => write!(f, "preview"),
            AppMode::Prod => write!(f, "prod"),
            AppMode::Custom(_) => write!(f, "custom"),
        }
    }
}

impl FromStr for AppMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "local" => Ok(AppMode::Local),
            "dev" => Ok(AppMode::Dev),
            "preview" => Ok(AppMode::Preview),
            "prod" => Ok(AppMode::Prod),
            "custom" => {
                // For custom mode, get the ENV_NAME
                match std::env::var("ENV_NAME") {
                    Ok(env_name) => Ok(AppMode::Custom(env_name)),
                    Err(_) => Err("ENV_NAME must be set when using custom mode".to_string()),
                }
            }
            _ => Err(format!("Invalid app mode: {}", s)),
        }
    }
}

fn normalize_optional_env(value: Result<String, env::VarError>) -> Option<String> {
    value
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn optional_env(name: &str) -> Option<String> {
    normalize_optional_env(env::var(name))
}

/// Default `RUST_LOG` when unset. The binary crate is `opensecret`; the old
/// `sg_backend` default matched no events, so local `cargo run` was silent.
const DEFAULT_RUST_LOG_FILTER: &str =
    "opensecret=debug,axum_login=debug,tower_sessions=debug,sqlx=warn,tower_http=debug";

fn init_tracing(app_mode: &AppMode) -> Result<(), Error> {
    let filter = EnvFilter::new(
        std::env::var("RUST_LOG").unwrap_or_else(|_| DEFAULT_RUST_LOG_FILTER.to_string()),
    );
    let fmt = tracing_subscriber::fmt::layer().with_ansi(false);
    if matches!(app_mode, AppMode::Local) {
        // File-redirected stdout is block-buffered; local debug needs lines
        // to appear in workspace / `cargo run` logs immediately.
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt.with_writer(|| std::io::LineWriter::new(std::io::stdout())))
            .try_init()?;
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt)
            .try_init()?;
    }
    Ok(())
}

#[cfg(test)]
mod optional_env_tests {
    use super::*;

    #[test]
    fn absent_or_blank_values_are_not_configured() {
        assert_eq!(normalize_optional_env(Err(env::VarError::NotPresent)), None);
        assert_eq!(normalize_optional_env(Ok(String::new())), None);
        assert_eq!(normalize_optional_env(Ok(" \t\n ".to_string())), None);
    }

    #[test]
    fn configured_values_are_trimmed() {
        assert_eq!(
            normalize_optional_env(Ok("  configured value  ".to_string())),
            Some("configured value".to_string())
        );
    }
}

#[cfg(test)]
mod default_log_filter_tests {
    use super::*;

    #[test]
    fn default_filter_targets_the_opensecret_crate() {
        assert!(
            DEFAULT_RUST_LOG_FILTER
                .split(',')
                .any(|directive| directive == "opensecret=debug"),
            "local default must enable opensecret=debug: {DEFAULT_RUST_LOG_FILTER}"
        );
        assert!(
            !DEFAULT_RUST_LOG_FILTER.contains("sg_backend"),
            "stale sg_backend default hides every local application log: {DEFAULT_RUST_LOG_FILTER}"
        );
    }
}

#[derive(Clone)]
pub struct AppState {
    app_mode: AppMode,
    db: Arc<dyn DBConnection + Send + Sync>,
    config: Config,
    aws_credential_manager: Arc<tokio::sync::RwLock<Option<AwsCredentialManager>>>,
    enclave_key: Vec<u8>,
    provider_client: Arc<ProviderClient>,
    proxy_router: Arc<ProxyRouter>,
    provider_router: Arc<ProviderRouter>,
    resend_api_key: Option<String>,
    ephemeral_keys: Arc<RwLock<BoundedTtlCache<PendingAttestationKey, EphemeralSecret>>>,
    session_states: Arc<tokio::sync::Mutex<LeaseAwareTtlCache<Uuid, SessionState>>>,
    oauth_manager: Arc<OAuthManager>,
    sqs_publisher: Option<Arc<SqsEventPublisher>>,
    billing_client: Option<BillingClient>,
    os_flags_client: Option<os_flags::OsFlagsClient>,
    router_v2_flag_failure_backoff: RoutingFlagsFailureBackoff,
    responses_message_reservations: ResponseMessageReservations,
    apple_jwt_verifier: Arc<AppleJwtVerifier>,
    response_executions: web::responses::ResponseExecutionRegistry,
    kagi_client: Option<Arc<crate::kagi::KagiClient>>,
}

#[derive(Clone, Default)]
pub(crate) struct ResponseMessageReservations {
    active: Arc<StdMutex<HashSet<Uuid>>>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ResponseMessageReservationError {
    AlreadyReserved,
    Unavailable,
}

pub(crate) struct ResponseMessageReservation {
    message_uuid: Uuid,
    active: Arc<StdMutex<HashSet<Uuid>>>,
}

impl ResponseMessageReservations {
    pub(crate) fn try_reserve(
        &self,
        message_uuid: Uuid,
    ) -> Result<ResponseMessageReservation, ResponseMessageReservationError> {
        let mut active = self
            .active
            .lock()
            .map_err(|_| ResponseMessageReservationError::Unavailable)?;
        if !active.insert(message_uuid) {
            return Err(ResponseMessageReservationError::AlreadyReserved);
        }
        drop(active);

        Ok(ResponseMessageReservation {
            message_uuid,
            active: self.active.clone(),
        })
    }
}

impl Drop for ResponseMessageReservation {
    fn drop(&mut self) {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active.remove(&self.message_uuid);
    }
}

#[cfg(test)]
mod response_message_reservations_tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;

    #[test]
    fn one_enclave_allows_only_one_active_request_per_message_uuid() {
        let reservations = ResponseMessageReservations::default();
        let message_uuid = Uuid::new_v4();
        let first = reservations
            .try_reserve(message_uuid)
            .expect("first request reserves the message UUID");

        assert!(matches!(
            reservations.try_reserve(message_uuid),
            Err(ResponseMessageReservationError::AlreadyReserved)
        ));

        drop(first);
        assert!(reservations.try_reserve(message_uuid).is_ok());
    }

    #[test]
    fn concurrent_duplicate_cannot_begin_while_the_owner_is_active() {
        let reservations = ResponseMessageReservations::default();
        let message_uuid = Uuid::new_v4();
        let owner_reservations = reservations.clone();
        let (owner_ready_tx, owner_ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let owner = thread::spawn(move || {
            let _reservation = owner_reservations
                .try_reserve(message_uuid)
                .expect("owner reserves the message UUID");
            owner_ready_tx.send(()).expect("signal owner readiness");
            release_rx.recv().expect("wait for release");
        });

        owner_ready_rx.recv().expect("owner becomes ready");
        assert!(matches!(
            reservations.try_reserve(message_uuid),
            Err(ResponseMessageReservationError::AlreadyReserved)
        ));

        release_tx.send(()).expect("release owner");
        owner.join().expect("owner thread exits");
        assert!(reservations.try_reserve(message_uuid).is_ok());
    }
}

#[derive(Clone, Debug)]
struct RoutingFlagsFailureBackoff {
    unavailable_until: Arc<RwLock<Option<tokio::time::Instant>>>,
    duration: Duration,
}

impl Default for RoutingFlagsFailureBackoff {
    fn default() -> Self {
        Self {
            unavailable_until: Arc::new(RwLock::new(None)),
            duration: Duration::from_secs(ROUTING_FLAGS_FAILURE_BACKOFF_SECS),
        }
    }
}

impl RoutingFlagsFailureBackoff {
    async fn is_active(&self) -> bool {
        self.unavailable_until
            .read()
            .await
            .is_some_and(|deadline| deadline > tokio::time::Instant::now())
    }

    async fn record_failure(&self) {
        *self.unavailable_until.write().await = Some(tokio::time::Instant::now() + self.duration);
    }

    async fn record_success(&self) {
        *self.unavailable_until.write().await = None;
    }
}

#[cfg(test)]
mod routing_flags_failure_backoff_tests {
    use super::*;

    #[tokio::test]
    async fn failure_backoff_opens_closes_and_expires() {
        let backoff = RoutingFlagsFailureBackoff {
            unavailable_until: Arc::new(RwLock::new(None)),
            duration: Duration::from_millis(1),
        };

        assert!(!backoff.is_active().await);
        backoff.record_failure().await;
        assert!(backoff.is_active().await);

        backoff.record_success().await;
        assert!(!backoff.is_active().await);

        backoff.record_failure().await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(!backoff.is_active().await);
    }
}

#[derive(Debug, Clone)]
struct AuthenticatedUser {
    user: User,
    auth_context: AuthContext,
}

#[derive(Default)]
pub struct AppStateBuilder {
    app_mode: Option<AppMode>,
    db: Option<Arc<dyn DBConnection + Send + Sync>>,
    enclave_key: Option<Vec<u8>>,
    aws_credential_manager: Option<Arc<tokio::sync::RwLock<Option<AwsCredentialManager>>>>,
    openai_api_key: Option<String>,
    openai_api_base: Option<String>,
    tinfoil_api_key: Option<String>,
    #[cfg(test)]
    tinfoil_api_base: Option<String>,
    jwt_secret: Option<Vec<u8>>,
    resend_api_key: Option<String>,
    github_client_secret: Option<String>,
    github_client_id: Option<String>,
    google_client_secret: Option<String>,
    google_client_id: Option<String>,
    apple_client_secret: Option<String>,
    apple_client_id: Option<String>,
    sqs_queue_maple_events_url: Option<String>,
    sqs_publisher: Option<Arc<SqsEventPublisher>>,
    billing_api_key: Option<String>,
    billing_server_url: Option<String>,
    os_flags_base_url: Option<String>,
    os_flags_api_key: Option<String>,
    kagi_api_key: Option<String>,
}

impl AppStateBuilder {
    pub fn app_mode(mut self, app_mode: AppMode) -> Self {
        self.app_mode = Some(app_mode);
        self
    }

    pub fn db(mut self, db: Arc<dyn DBConnection + Send + Sync>) -> Self {
        self.db = Some(db);
        self
    }

    pub fn enclave_key(mut self, enclave_key: Vec<u8>) -> Self {
        self.enclave_key = Some(enclave_key);
        self
    }

    pub fn aws_credential_manager(
        mut self,
        aws_credential_manager: Arc<tokio::sync::RwLock<Option<AwsCredentialManager>>>,
    ) -> Self {
        self.aws_credential_manager = Some(aws_credential_manager);
        self
    }

    pub fn openai_api_key(mut self, openai_api_key: Option<String>) -> Self {
        self.openai_api_key = openai_api_key;
        self
    }

    pub fn openai_api_base(mut self, openai_api_base: String) -> Self {
        self.openai_api_base = Some(openai_api_base);
        self
    }

    pub fn tinfoil_api_key(mut self, tinfoil_api_key: String) -> Self {
        self.tinfoil_api_key = Some(tinfoil_api_key);
        self
    }

    #[cfg(test)]
    pub fn tinfoil_api_base(mut self, tinfoil_api_base: String) -> Self {
        self.tinfoil_api_base = Some(tinfoil_api_base);
        self
    }

    pub fn jwt_secret(mut self, jwt_secret: Vec<u8>) -> Self {
        self.jwt_secret = Some(jwt_secret);
        self
    }

    pub fn resend_api_key(mut self, resend_api_key: Option<String>) -> Self {
        self.resend_api_key = resend_api_key;
        self
    }

    pub fn github_client_secret(mut self, github_client_secret: Option<String>) -> Self {
        self.github_client_secret = github_client_secret;
        self
    }

    pub fn github_client_id(mut self, github_client_id: Option<String>) -> Self {
        self.github_client_id = github_client_id;
        self
    }

    pub fn google_client_secret(mut self, google_client_secret: Option<String>) -> Self {
        self.google_client_secret = google_client_secret;
        self
    }

    pub fn google_client_id(mut self, google_client_id: Option<String>) -> Self {
        self.google_client_id = google_client_id;
        self
    }

    pub fn apple_client_secret(mut self, apple_client_secret: Option<String>) -> Self {
        self.apple_client_secret = apple_client_secret;
        self
    }

    pub fn apple_client_id(mut self, apple_client_id: Option<String>) -> Self {
        self.apple_client_id = apple_client_id;
        self
    }

    pub fn sqs_queue_maple_events_url(
        mut self,
        sqs_queue_maple_events_url: Option<String>,
    ) -> Self {
        self.sqs_queue_maple_events_url = sqs_queue_maple_events_url;
        self
    }

    pub fn sqs_publisher(mut self, sqs_publisher: Option<Arc<SqsEventPublisher>>) -> Self {
        self.sqs_publisher = sqs_publisher;
        self
    }

    pub fn billing_api_key(mut self, billing_api_key: Option<String>) -> Self {
        self.billing_api_key = billing_api_key;
        self
    }

    pub fn billing_server_url(mut self, billing_server_url: Option<String>) -> Self {
        self.billing_server_url = billing_server_url;
        self
    }

    pub fn os_flags_base_url(mut self, os_flags_base_url: Option<String>) -> Self {
        self.os_flags_base_url = os_flags_base_url;
        self
    }

    pub fn os_flags_api_key(mut self, os_flags_api_key: Option<String>) -> Self {
        self.os_flags_api_key = os_flags_api_key;
        self
    }

    pub fn kagi_api_key(mut self, kagi_api_key: Option<String>) -> Self {
        self.kagi_api_key = kagi_api_key;
        self
    }

    pub async fn build(self) -> Result<AppState, Error> {
        let app_mode = self
            .app_mode
            .ok_or(Error::BuilderError("app_mode is required".to_string()))?;
        let db = self
            .db
            .ok_or(Error::BuilderError("db is required".to_string()))?;
        let enclave_key = self
            .enclave_key
            .ok_or(Error::BuilderError("enclave_key is required".to_string()))?;
        let aws_credential_manager = self.aws_credential_manager.ok_or(Error::BuilderError(
            "aws_credential_manager is required".to_string(),
        ))?;
        let openai_api_base = self.openai_api_base.ok_or(Error::BuilderError(
            "openai_api_base is required".to_string(),
        ))?;
        let jwt_secret = self
            .jwt_secret
            .ok_or(Error::BuilderError("jwt_secret is required".to_string()))?;

        #[cfg(test)]
        let provider_client = if let Some(tinfoil_api_base) = self.tinfoil_api_base {
            ProviderClient::for_test(tinfoil_api_base)
        } else {
            let tinfoil_api_key = self.tinfoil_api_key.ok_or(Error::BuilderError(
                "tinfoil_api_key is required".to_string(),
            ))?;
            ProviderClient::new(tinfoil_api_key).await
        }
        .map_err(|error| {
            Error::BuilderError(format!("Failed to initialize provider client: {error}"))
        })?;

        #[cfg(not(test))]
        let provider_client = {
            let tinfoil_api_key = self.tinfoil_api_key.ok_or(Error::BuilderError(
                "tinfoil_api_key is required".to_string(),
            ))?;
            ProviderClient::new(tinfoil_api_key)
                .await
                .map_err(|error| {
                    Error::BuilderError(format!("Failed to initialize provider client: {error}"))
                })?
        };
        let provider_client = Arc::new(provider_client);

        let config = Config {
            jwt_keys: jwt::JwtKeys::new(jwt_secret)?,
            access_token_maxage: 60,  // 60 minutes
            refresh_token_maxage: 30, // 30 days
        };

        // Log the public key in hex format
        tracing::info!(
            "JWT ES256K public key (hex): {}",
            hex::encode(config.jwt_keys.public_key().serialize())
        );

        let mut oauth_manager = OAuthManager::new();

        // Initialize GitHub provider
        let github_provider = GithubProvider::new(db.clone()).await.map_err(|e| {
            error!("Failed to initialize GitHub OAuth provider: {:?}", e);
            Error::BuilderError("Failed to initialize GitHub OAuth provider".to_string())
        })?;
        oauth_manager.add_provider("github".to_string(), Box::new(github_provider));

        // Initialize Google provider
        let google_provider = GoogleProvider::new(db.clone()).await.map_err(|e| {
            error!("Failed to initialize Google OAuth provider: {:?}", e);
            Error::BuilderError("Failed to initialize Google OAuth provider".to_string())
        })?;
        oauth_manager.add_provider("google".to_string(), Box::new(google_provider));

        // Initialize Apple provider
        let apple_provider = AppleProvider::new(db.clone()).await.map_err(|e| {
            error!("Failed to initialize Apple OAuth provider: {:?}", e);
            Error::BuilderError("Failed to initialize Apple OAuth provider".to_string())
        })?;
        oauth_manager.add_provider("apple".to_string(), Box::new(apple_provider));

        let oauth_manager = Arc::new(oauth_manager);

        // Initialize SQS publisher if URL is provided
        let sqs_publisher = if let Some(ref queue_url) = self.sqs_queue_maple_events_url {
            // Use the same region as AWS credentials if available
            let region = if let Some(creds) = aws_credential_manager.read().await.as_ref() {
                creds.get_credentials().await.map(|c| c.region)
            } else {
                None
            };

            Some(Arc::new(
                SqsEventPublisher::new(queue_url.clone(), region, aws_credential_manager.clone())
                    .await,
            ))
        } else {
            None
        };

        let billing_client = if let (Some(api_key), Some(base_url)) =
            (self.billing_api_key, self.billing_server_url)
        {
            tracing::debug!("Billing client is configured.");
            Some(BillingClient::new(api_key, base_url))
        } else {
            tracing::debug!("Billing client not configured");
            None
        };

        let os_flags_client = if let Some(base_url) = self.os_flags_base_url {
            tracing::debug!("os-flags client is configured.");
            Some(
                os_flags::OsFlagsClient::new(base_url, self.os_flags_api_key).map_err(|e| {
                    Error::BuilderError(format!("Failed to initialize os-flags client: {e}"))
                })?,
            )
        } else {
            tracing::debug!("os-flags client not configured");
            None
        };

        // Initialize the AppleJwtVerifier
        let apple_jwt_verifier = Arc::new(AppleJwtVerifier::new());

        // Tinfoil routing is identified here, but the in-process SDK resolves
        // and attests its enclave independently in the background. Its control
        // plane therefore cannot gate AppState construction or non-Tinfoil routes.
        let proxy_router = Arc::new(ProxyRouter::new(
            openai_api_base.clone(),
            self.openai_api_key.clone(),
            provider_client.tinfoil_base_url(),
        ));
        let provider_router = Arc::new(ProviderRouter::default());

        let kagi_client = if let Some(ref api_key) = self.kagi_api_key {
            tracing::info!("Initializing Kagi client");
            match crate::kagi::KagiClient::new(api_key.clone()) {
                Ok(client) => {
                    tracing::debug!("Kagi client initialized successfully");
                    Some(Arc::new(client))
                }
                Err(e) => {
                    tracing::error!(
                        "Failed to initialize Kagi client: {:?}. Web search will be unavailable.",
                        e
                    );
                    None
                }
            }
        } else {
            tracing::debug!("Kagi API key not configured, web search will be unavailable");
            None
        };

        Ok(AppState {
            app_mode,
            db,
            config,
            aws_credential_manager,
            enclave_key,
            provider_client,
            proxy_router,
            provider_router,
            resend_api_key: self.resend_api_key,
            ephemeral_keys: Arc::new(RwLock::new(BoundedTtlCache::new(
                NonZeroUsize::new(MAX_PENDING_ATTESTATIONS)
                    .expect("pending attestation capacity must be non-zero"),
                PENDING_ATTESTATION_TTL,
            ))),
            session_states: Arc::new(tokio::sync::Mutex::new(LeaseAwareTtlCache::new(
                NonZeroUsize::new(MAX_ENCRYPTION_SESSIONS)
                    .expect("encryption session capacity must be non-zero"),
                ENCRYPTION_SESSION_IDLE_TTL,
            ))),
            oauth_manager,
            sqs_publisher,
            billing_client,
            os_flags_client,
            router_v2_flag_failure_backoff: RoutingFlagsFailureBackoff::default(),
            responses_message_reservations: ResponseMessageReservations::default(),
            apple_jwt_verifier,
            response_executions: web::responses::ResponseExecutionRegistry::default(),
            kagi_client,
        })
    }
}

impl AppState {
    pub fn os_flags(&self) -> Option<&os_flags::OsFlagsClient> {
        self.os_flags_client.as_ref()
    }

    pub async fn is_feature_enabled(&self, user_uuid: Uuid, flag_key: &str) -> bool {
        let Some(client) = &self.os_flags_client else {
            return false;
        };

        match client.is_enabled(user_uuid, flag_key).await {
            Ok(enabled) => enabled,
            Err(e) => {
                warn!(
                    "os-flags check failed (user_uuid={}, flag_key={}): {}",
                    user_uuid, flag_key, e
                );
                false
            }
        }
    }

    /// Resolve automatic aliases under the request's router policy. Router v2
    /// uses fixed aliases without consulting legacy selector flags. Legacy paid
    /// overrides default off on missing configuration, failure, or timeout.
    pub(crate) async fn model_alias_targets(
        &self,
        user_uuid: Uuid,
        model_plan: ModelPlan,
        routing_mode: InferenceRoutingMode,
    ) -> ModelAliasTargets {
        if routing_mode == InferenceRoutingMode::V2 {
            return ModelAliasTargets::for_router_v2(model_plan);
        }
        if !model_plan.is_paid() {
            return ModelAliasTargets::for_plan(model_plan);
        }

        let Some(client) = &self.os_flags_client else {
            trace!(
                "os-flags client not configured; using default paid model aliases (user_uuid={})",
                user_uuid
            );
            return ModelAliasTargets::for_plan(model_plan);
        };

        let overrides = match tokio::time::timeout(
            Duration::from_secs(MODEL_ALIAS_FLAGS_TIMEOUT_SECS),
            client.get_user_flags(user_uuid, Some(os_flags::PAID_MODEL_ALIAS_FLAG_KEYS)),
        )
        .await
        {
            Ok(Ok(response)) => PaidModelAliasOverrides::from_flag_values(&response.flags),
            Ok(Err(e)) => {
                warn!(
                    "os-flags paid model alias check failed (user_uuid={}): {}; using default aliases",
                    user_uuid, e
                );
                PaidModelAliasOverrides::default()
            }
            Err(_) => {
                warn!(
                    "os-flags paid model alias check timed out after {}s (user_uuid={}); using default aliases",
                    MODEL_ALIAS_FLAGS_TIMEOUT_SECS, user_uuid
                );
                PaidModelAliasOverrides::default()
            }
        };

        ModelAliasTargets::for_plan_with_overrides(model_plan, overrides)
    }

    /// Resolve the inference router exactly once for an authenticated external
    /// request. Router v2 is opt-in: absent configuration, a missing or false
    /// flag, service failure, and timeout all retain the legacy router.
    pub(crate) async fn inference_routing_mode(&self, user_uuid: Uuid) -> InferenceRoutingMode {
        let flag_key = os_flags::INFERENCE_ROUTER_V2_FLAG_KEY;
        let Some(client) = &self.os_flags_client else {
            trace!(
                user_uuid = %user_uuid,
                flag_key,
                "os-flags client not configured; retaining legacy inference router"
            );
            return InferenceRoutingMode::Legacy;
        };
        if self.router_v2_flag_failure_backoff.is_active().await {
            let cached_value = client.get_cached_bool_flag(user_uuid, flag_key).await;
            let mode = cached_value
                .flatten()
                .map(|value| InferenceRoutingMode::from_router_v2_flag(Some(value)))
                .unwrap_or(InferenceRoutingMode::Legacy);
            trace!(
                user_uuid = %user_uuid,
                flag_key,
                cache_hit = cached_value.is_some(),
                routing_mode = ?mode,
                "os-flags routing checks are in failure backoff; using cached inference-router decision"
            );
            return mode;
        }

        let mode = match tokio::time::timeout(
            Duration::from_secs(PROVIDER_ROUTING_FLAGS_TIMEOUT_SECS),
            client.get_bool_flag(user_uuid, flag_key),
        )
        .await
        {
            Ok(Ok(value)) => {
                self.router_v2_flag_failure_backoff.record_success().await;
                InferenceRoutingMode::from_router_v2_flag(value)
            }
            Ok(Err(error)) => {
                self.router_v2_flag_failure_backoff.record_failure().await;
                warn!(
                    user_uuid = %user_uuid,
                    flag_key,
                    %error,
                    "os-flags inference-router check failed; retaining legacy router"
                );
                InferenceRoutingMode::Legacy
            }
            Err(_) => {
                self.router_v2_flag_failure_backoff.record_failure().await;
                warn!(
                    user_uuid = %user_uuid,
                    flag_key,
                    timeout_seconds = PROVIDER_ROUTING_FLAGS_TIMEOUT_SECS,
                    "os-flags inference-router check timed out; retaining legacy router"
                );
                InferenceRoutingMode::Legacy
            }
        };

        debug!(
            user_uuid = %user_uuid,
            flag_key,
            routing_mode = ?mode,
            "Resolved request-scoped inference router"
        );
        mode
    }

    /// Resolve an explicit provider preference only for models configured for
    /// flag-controlled routing. Missing configuration, flags, failures, and
    /// timeouts retain the model's default provider.
    pub(crate) async fn provider_routing_preference(
        &self,
        user_uuid: Uuid,
        requested_model: &str,
    ) -> Option<ProviderPreference> {
        let provider_flag = self
            .provider_router
            .provider_routing_flag_for_completion_model(requested_model)?;
        let flag_key = provider_flag.key();

        let Some(client) = &self.os_flags_client else {
            trace!(
                "os-flags client not configured; using default provider routing for model {}",
                requested_model
            );
            return None;
        };
        match tokio::time::timeout(
            Duration::from_secs(PROVIDER_ROUTING_FLAGS_TIMEOUT_SECS),
            client.get_bool_flag(user_uuid, flag_key),
        )
        .await
        {
            Ok(Ok(Some(enabled))) => Some(provider_flag.preference_for(enabled)),
            Ok(Ok(None)) => {
                debug!(
                    "os-flags provider routing flag missing (user_uuid={}, requested_model={}, flag_key={}); using default provider routing",
                    user_uuid, requested_model, flag_key
                );
                None
            }
            Ok(Err(e)) => {
                warn!(
                    "os-flags provider routing check failed (user_uuid={}, requested_model={}, flag_key={}): {}; using default provider routing",
                    user_uuid, requested_model, flag_key, e
                );
                None
            }
            Err(_) => {
                warn!(
                    "os-flags provider routing check timed out after {}s (user_uuid={}, requested_model={}, flag_key={}); using default provider routing",
                    PROVIDER_ROUTING_FLAGS_TIMEOUT_SECS, user_uuid, requested_model, flag_key
                );
                None
            }
        }
    }

    /// Load one billing snapshot for quota, guest, and model-plan decisions.
    /// Missing configuration, failures, and timeouts retain the existing
    /// fail-open quota behavior while granting free-model access only.
    pub(crate) async fn chat_billing_access(
        &self,
        user_uuid: Uuid,
        is_api: bool,
    ) -> Option<ChatBillingAccess> {
        let Some(client) = &self.billing_client else {
            trace!(
                "billing client not configured; using free model access (user_uuid={})",
                user_uuid
            );
            return None;
        };

        match tokio::time::timeout(
            Duration::from_secs(BILLING_ACCESS_TIMEOUT_SECS),
            client.chat_access(user_uuid, is_api),
        )
        .await
        {
            Ok(Ok(access)) => Some(access),
            Ok(Err(e)) => {
                warn!(
                    "billing access check failed (user_uuid={}): {}; using free model access",
                    user_uuid, e
                );
                None
            }
            Err(_) => {
                warn!(
                    "billing access check timed out after {}s (user_uuid={}); using free model access",
                    BILLING_ACCESS_TIMEOUT_SECS, user_uuid
                );
                None
            }
        }
    }

    fn password_login_identifier_for_user(user: &User) -> (PasswordLoginIdentifierKind, String) {
        match user.get_email() {
            Some(email) => (
                PasswordLoginIdentifierKind::Email,
                normalize_email_login_identifier(email),
            ),
            None => (
                PasswordLoginIdentifierKind::GuestUuid,
                normalize_guest_login_identifier(user.uuid),
            ),
        }
    }

    fn password_auth_binding_for_user(
        &self,
        user: &User,
        decrypted_password_verifier: &str,
    ) -> Result<AuthBinding, Error> {
        let (login_identifier_kind, normalized_login_identifier) =
            Self::password_login_identifier_for_user(user);

        compute_password_auth_binding(
            &self.enclave_key,
            user.project_id,
            user.uuid,
            login_identifier_kind,
            &normalized_login_identifier,
            decrypted_password_verifier,
        )
        .map_err(|e| Error::EncryptionError(e.to_string()))
    }

    fn password_auth_context_for_user(
        &self,
        user: &User,
        decrypted_password_verifier: &str,
    ) -> Result<AuthContext, Error> {
        let auth_binding =
            self.password_auth_binding_for_user(user, decrypted_password_verifier)?;
        Ok(AuthContext::new(
            AuthMethod::Password,
            user.project_id,
            *auth_binding.as_bytes(),
        ))
    }

    fn oauth_auth_binding_for_user(
        &self,
        user: &User,
        provider_name: &str,
        provider_user_id: &str,
    ) -> Result<AuthBinding, Error> {
        compute_oauth_auth_binding(
            &self.enclave_key,
            user.project_id,
            user.uuid,
            provider_name,
            provider_user_id,
        )
        .map_err(|e| Error::EncryptionError(e.to_string()))
    }

    fn oauth_auth_context_for_user(
        &self,
        user: &User,
        provider_name: &str,
        provider_user_id: &str,
    ) -> Result<AuthContext, Error> {
        let auth_binding =
            self.oauth_auth_binding_for_user(user, provider_name, provider_user_id)?;
        Ok(AuthContext::new(
            AuthMethod::OAuth,
            user.project_id,
            *auth_binding.as_bytes(),
        ))
    }

    fn decrypt_seed_for_auth_context(
        &self,
        user: &User,
        auth_context: &AuthContext,
    ) -> Result<Vec<u8>, Error> {
        if user.project_id != auth_context.project_id {
            return Err(Error::AuthenticationError);
        }

        let credential_kind = match auth_context.method {
            AuthMethod::Password => CredentialKind::Password,
            AuthMethod::OAuth => CredentialKind::OAuth,
        };
        let auth_binding = AuthBinding::from_bytes(auth_context.auth_binding);
        let candidates = self
            .db
            .get_user_seed_wrappings_for_user_and_kind(user.uuid, credential_kind.as_str())?;

        for candidate in candidates {
            if candidate.wrapping_version != SEED_WRAP_VERSION_V1 {
                continue;
            }

            match decrypt_seed_v1(
                &self.enclave_key,
                &candidate.seed_enc,
                user.uuid,
                user.project_id,
                credential_kind,
                &auth_binding,
            ) {
                Ok(seed) => return Ok(seed),
                Err(_) => continue,
            }
        }

        Err(Error::AuthenticationError)
    }

    fn verify_seed_wrap_for_auth_context(
        &self,
        user: &User,
        auth_context: &AuthContext,
    ) -> Result<(), Error> {
        self.decrypt_seed_for_auth_context(user, auth_context)
            .map(|_| ())
    }

    fn new_password_seed_wrapping_for_user(
        &self,
        user: &User,
        decrypted_password_verifier: &str,
        plaintext_seed: &[u8],
    ) -> Result<NewUserSeedWrapping, Error> {
        let auth_binding =
            self.password_auth_binding_for_user(user, decrypted_password_verifier)?;

        let credential_lookup_hash =
            password_credential_lookup_hash(&self.enclave_key, user.project_id, user.uuid)
                .map_err(|e| Error::EncryptionError(e.to_string()))?;

        let seed_enc = encrypt_seed_v1(
            &self.enclave_key,
            plaintext_seed,
            user.uuid,
            user.project_id,
            CredentialKind::Password,
            &auth_binding,
        )
        .map_err(|e| Error::EncryptionError(e.to_string()))?;

        let new_wrapping = NewUserSeedWrapping::new(
            user.uuid,
            CredentialKind::Password.as_str(),
            credential_lookup_hash.as_bytes().to_vec(),
            SEED_WRAP_VERSION_V1,
            seed_enc,
        );

        self.verify_new_password_seed_wrapping_for_user(
            user,
            decrypted_password_verifier,
            plaintext_seed,
            &new_wrapping,
        )?;

        Ok(new_wrapping)
    }

    fn verify_new_password_seed_wrapping_for_user(
        &self,
        user: &User,
        decrypted_password_verifier: &str,
        plaintext_seed: &[u8],
        new_wrapping: &NewUserSeedWrapping,
    ) -> Result<(), Error> {
        let auth_binding =
            self.password_auth_binding_for_user(user, decrypted_password_verifier)?;
        let decrypted_seed = decrypt_seed_v1(
            &self.enclave_key,
            &new_wrapping.seed_enc,
            user.uuid,
            user.project_id,
            CredentialKind::Password,
            &auth_binding,
        )
        .map_err(|e| Error::EncryptionError(e.to_string()))?;

        if decrypted_seed != plaintext_seed {
            return Err(Error::EncryptionError(
                "New password seed wrap verification failed".to_string(),
            ));
        }

        Ok(())
    }

    #[cfg(test)]
    fn create_password_seed_wrap_for_user(
        &self,
        user: &User,
        decrypted_password_verifier: &str,
        plaintext_seed: &[u8],
    ) -> Result<(), Error> {
        let new_wrapping = self.new_password_seed_wrapping_for_user(
            user,
            decrypted_password_verifier,
            plaintext_seed,
        )?;

        self.db.upsert_user_seed_wrapping(new_wrapping)?;
        Ok(())
    }

    fn create_user_with_password_seed_wrap(
        &self,
        new_user: NewUser,
        decrypted_password_verifier: &str,
        plaintext_seed: &[u8],
    ) -> Result<User, Error> {
        let conn = &mut self
            .db
            .get_pool()
            .get()
            .map_err(|_| Error::DatabaseError(DBError::ConnectionError))?;

        conn.transaction::<_, CreateUserSeedWrapTransactionError, _>(|conn| {
            let user = new_user.insert(conn)?;
            let new_wrapping = self.new_password_seed_wrapping_for_user(
                &user,
                decrypted_password_verifier,
                plaintext_seed,
            )?;
            new_wrapping.upsert_by_credential(conn)?;
            Ok(user)
        })
        .map_err(Error::from)
    }

    fn new_oauth_seed_wrapping_for_user(
        &self,
        user: &User,
        provider_name: &str,
        provider_user_id: &str,
        plaintext_seed: &[u8],
    ) -> Result<NewUserSeedWrapping, Error> {
        let auth_binding =
            self.oauth_auth_binding_for_user(user, provider_name, provider_user_id)?;
        let credential_lookup_hash = oauth_credential_lookup_hash(
            &self.enclave_key,
            user.project_id,
            user.uuid,
            provider_name,
            provider_user_id,
        )
        .map_err(|e| Error::EncryptionError(e.to_string()))?;

        let seed_enc = encrypt_seed_v1(
            &self.enclave_key,
            plaintext_seed,
            user.uuid,
            user.project_id,
            CredentialKind::OAuth,
            &auth_binding,
        )
        .map_err(|e| Error::EncryptionError(e.to_string()))?;

        let new_wrapping = NewUserSeedWrapping::new(
            user.uuid,
            CredentialKind::OAuth.as_str(),
            credential_lookup_hash.as_bytes().to_vec(),
            SEED_WRAP_VERSION_V1,
            seed_enc,
        );

        self.verify_new_oauth_seed_wrapping_for_user(
            user,
            provider_name,
            provider_user_id,
            plaintext_seed,
            &new_wrapping,
        )?;

        Ok(new_wrapping)
    }

    #[cfg(test)]
    fn create_oauth_seed_wrap_for_user(
        &self,
        user: &User,
        provider_name: &str,
        provider_user_id: &str,
        plaintext_seed: &[u8],
    ) -> Result<(), Error> {
        let new_wrapping = self.new_oauth_seed_wrapping_for_user(
            user,
            provider_name,
            provider_user_id,
            plaintext_seed,
        )?;

        self.db.upsert_user_seed_wrapping(new_wrapping)?;
        Ok(())
    }

    pub fn create_user_with_oauth_seed_wrap(
        &self,
        new_user: NewUser,
        provider_id: i32,
        provider_name: &str,
        provider_user_id: &str,
        access_token_enc: Vec<u8>,
        plaintext_seed: &[u8],
    ) -> Result<User, Error> {
        let conn = &mut self
            .db
            .get_pool()
            .get()
            .map_err(|_| Error::DatabaseError(DBError::ConnectionError))?;

        conn.transaction::<_, CreateUserSeedWrapTransactionError, _>(|conn| {
            let user = new_user.insert(conn)?;

            let new_connection = NewUserOAuthConnection {
                user_id: user.uuid,
                provider_id,
                provider_user_id: provider_user_id.to_string(),
                access_token_enc,
                refresh_token_enc: None,
                expires_at: None,
            };
            new_connection.insert(conn)?;

            let new_wrapping = self.new_oauth_seed_wrapping_for_user(
                &user,
                provider_name,
                provider_user_id,
                plaintext_seed,
            )?;
            new_wrapping.upsert_by_credential(conn)?;

            let new_verification = NewEmailVerification::new(user.uuid, 24, true);
            new_verification.insert(conn)?;

            Ok(user)
        })
        .map_err(Error::from)
    }

    fn verify_new_oauth_seed_wrapping_for_user(
        &self,
        user: &User,
        provider_name: &str,
        provider_user_id: &str,
        plaintext_seed: &[u8],
        new_wrapping: &NewUserSeedWrapping,
    ) -> Result<(), Error> {
        let auth_binding =
            self.oauth_auth_binding_for_user(user, provider_name, provider_user_id)?;
        let decrypted_seed = decrypt_seed_v1(
            &self.enclave_key,
            &new_wrapping.seed_enc,
            user.uuid,
            user.project_id,
            CredentialKind::OAuth,
            &auth_binding,
        )
        .map_err(|e| Error::EncryptionError(e.to_string()))?;

        if decrypted_seed != plaintext_seed {
            return Err(Error::EncryptionError(
                "New OAuth seed wrap verification failed".to_string(),
            ));
        }

        Ok(())
    }

    async fn register_user(&self, creds: RegisterCredentials) -> Result<User, Error> {
        // Get project by client_id
        let project = self
            .db
            .get_org_project_by_client_id(creds.client_id)
            .map_err(|e| {
                error!(
                    "Database error during client_id ({:?}) lookup: {:?}",
                    creds.client_id, e
                );
                DBError::OrgProjectNotFound
            })?;

        // First check if user exists - only if email is provided
        if let Some(email) = &creds.email {
            match self.db.get_user_by_email(email.clone(), project.id) {
                Ok(_) => {
                    // User already exists in this project
                    return Err(Error::UserAlreadyExists);
                }
                Err(DBError::UserNotFound) => {
                    // This is what we want - user doesn't exist
                }
                Err(e) => {
                    // Some other database error
                    return Err(Error::DatabaseError(e));
                }
            }
        }

        let password = creds.password;

        // hash then encrypt with enclave key
        let password_hash = generate_hash(password);

        let secret_key = SecretKey::from_slice(&self.enclave_key.clone())
            .map_err(|e| Error::EncryptionError(e.to_string()))?;

        let encrypted_pw = encrypt_with_key(&secret_key, password_hash.as_bytes()).await;

        tracing::debug!("Registering new user");

        // Generate private key for new user
        let user_seed_words = generate_twelve_word_seed(self.aws_credential_manager.clone())
            .await?
            .to_string();

        let new_user =
            NewUser::new(creds.email, Some(encrypted_pw), project.id).with_name_option(creds.name);

        let user = self.create_user_with_password_seed_wrap(
            new_user,
            &password_hash,
            user_seed_words.as_bytes(),
        )?;

        tracing::info!("Registered new user: {}", user.uuid);

        Ok(user)
    }

    async fn authenticate_user(
        &self,
        user_email: Option<String>,
        user_id: Option<Uuid>,
        user_password: String,
        user_project_id: i32,
    ) -> Result<Option<AuthenticatedUser>, Error> {
        // Ensure at least one identifier is provided
        if user_email.is_none() && user_id.is_none() {
            return Err(Error::AuthenticationError);
        }

        // Try email first if provided, fall back to UUID
        let user = if let Some(email) = user_email {
            self.db.get_user_by_email(email, user_project_id)?
        } else {
            // We can safely unwrap id here because we checked above that at least one exists
            let user = self.db.get_user_by_uuid(user_id.unwrap())?;
            // Verify user belongs to the specified project
            if user.project_id != user_project_id {
                return Err(Error::AuthenticationError);
            }
            user
        };

        // Check if the user is an OAuth-only user
        if user.password_enc.is_none() {
            error!("OAuth-only user attempted password login");
            return Ok(None);
        }

        // Verify the current password
        let secret_key = SecretKey::from_slice(&self.enclave_key.clone())
            .map_err(|e| Error::EncryptionError(e.to_string()))?;

        let decrypted_password_bytes =
            decrypt_with_key(&secret_key, user.password_enc.as_ref().unwrap())
                .map_err(|e| Error::EncryptionError(e.to_string()))?;

        let decrypted_password_hash = String::from_utf8(decrypted_password_bytes)
            .map_err(|e| Error::EncryptionError(format!("Failed to decode UTF-8: {}", e)))?;
        let verifier_for_binding = decrypted_password_hash.clone();

        // Verifying the password is blocking and potentially slow, so we'll do so via
        // `spawn_blocking`.
        let res =
            task::spawn_blocking(move || verify_password(user_password, &decrypted_password_hash))
                .await?;

        match res {
            Ok(_) => {
                let auth_context =
                    self.password_auth_context_for_user(&user, &verifier_for_binding)?;
                self.verify_seed_wrap_for_auth_context(&user, &auth_context)?;
                Ok(Some(AuthenticatedUser { user, auth_context }))
            }
            Err(_) => Ok(None),
        }
    }

    async fn get_user(&self, user_uuid: Uuid) -> Result<User, Error> {
        let user = self
            .db
            .get_user_by_uuid(user_uuid)
            .map_err(|_| Error::UserNotFound)?;
        Ok(user)
    }

    /// Returns the user's private key, optionally derived using the provided derivation paths.
    ///
    /// # Arguments
    /// * `user` - The authenticated user
    /// * `auth_context` - The signed auth context from the validated user JWT
    /// * `derivation_path` - Optional BIP-32 derivation path
    /// * `seed_phrase_derivation_path` - Optional BIP-85 derivation path to derive a child seed
    ///
    /// # Returns
    /// * `Result<SecretKey, Error>` - The user's private key, potentially derived through
    ///   BIP-85 and/or BIP-32 derivation paths
    async fn get_user_key(
        &self,
        user: &User,
        auth_context: &AuthContext,
        derivation_path: Option<&str>,
        seed_phrase_derivation_path: Option<&str>,
    ) -> Result<SecretKey, Error> {
        let plaintext_seed = self.decrypt_seed_for_auth_context(user, auth_context)?;

        let user_secret_key = plaintext_user_seed_to_key(
            &plaintext_seed,
            derivation_path,
            seed_phrase_derivation_path,
        )?;

        Ok(user_secret_key)
    }

    /// Sign a message with the user's private key, using the specified algorithm
    async fn sign_message(
        &self,
        user: &User,
        auth_context: &AuthContext,
        message_bytes: &[u8],
        algorithm: message_signing::SigningAlgorithm,
        derivation_path: Option<&str>,
        seed_phrase_derivation_path: Option<&str>,
    ) -> Result<message_signing::SignMessageResponse, Error> {
        let user_secret_key = self
            .get_user_key(
                user,
                auth_context,
                derivation_path,
                seed_phrase_derivation_path,
            )
            .await?;

        let result = message_signing::sign_message(&user_secret_key, message_bytes, algorithm);

        if result.is_err() {
            error!("Failed to sign message for user: {}", user.uuid);
        }

        result
    }

    async fn get(
        &self,
        user: &User,
        auth_context: &AuthContext,
        key: String,
    ) -> StoreResult<Option<String>> {
        let user_key = self
            .get_user_key(user, auth_context, None, None)
            .await
            .map_err(|_| StoreError::Unauthorized)?;
        kv::get(self.db.get_pool(), user.uuid, &key, &user_key)
    }

    async fn put(
        &self,
        user: &User,
        auth_context: &AuthContext,
        key: String,
        value: String,
    ) -> StoreResult<()> {
        let user_key = self
            .get_user_key(user, auth_context, None, None)
            .await
            .map_err(|_| StoreError::Unauthorized)?;
        kv::put(
            self.db.get_pool(),
            user.uuid,
            key,
            value,
            &user_key,
            self.aws_credential_manager.clone(),
        )
        .await
    }

    async fn delete(
        &self,
        user: &User,
        auth_context: &AuthContext,
        key: String,
    ) -> StoreResult<()> {
        let user_key = self
            .get_user_key(user, auth_context, None, None)
            .await
            .map_err(|_| StoreError::Unauthorized)?;
        kv::delete(self.db.get_pool(), user.uuid, &key, &user_key)
    }

    async fn delete_all(&self, user_id: Uuid) -> StoreResult<()> {
        kv::delete_all(self.db.get_pool(), user_id)
    }

    async fn list(&self, user: &User, auth_context: &AuthContext) -> StoreResult<Vec<KVPair>> {
        let user_key = self
            .get_user_key(user, auth_context, None, None)
            .await
            .map_err(|_| StoreError::Unauthorized)?;
        kv::list(self.db.get_pool(), user.uuid, &user_key)
    }

    pub async fn get_aws_credentials(&self) -> Option<AwsCredentials> {
        if let Some(manager) = self.aws_credential_manager.read().await.as_ref() {
            manager.get_credentials().await
        } else {
            None
        }
    }

    pub async fn get_enclave_key(&self) -> Vec<u8> {
        self.enclave_key.clone()
    }

    pub async fn create_ephemeral_key(&self, nonce: &str) -> Result<PublicKey, ApiError> {
        validate_attestation_nonce(nonce)?;
        let custom_rng = CustomRng::new();

        // Use a wrapper that implements RngCore and CryptoRng
        let mut rng_wrapper = AsyncRngWrapper::new(custom_rng);

        // Create the EphemeralSecret using the RngCore implementation
        let ephemeral_secret = EphemeralSecret::random_from_rng(&mut rng_wrapper);
        let public_key = PublicKey::from(&ephemeral_secret);

        self.ephemeral_keys
            .write()
            .await
            .insert_evicting(attestation_nonce_key(nonce), ephemeral_secret);

        Ok(public_key)
    }

    pub async fn get_and_remove_ephemeral_secret(
        &self,
        nonce: &str,
    ) -> Result<Option<EphemeralSecret>, ApiError> {
        validate_attestation_nonce(nonce)?;
        Ok(self
            .ephemeral_keys
            .write()
            .await
            .take_live(&attestation_nonce_key(nonce)))
    }

    pub(crate) async fn store_session_state(
        &self,
        session_id: Uuid,
        session_state: SessionState,
    ) -> Result<(), ApiError> {
        match self
            .session_states
            .lock()
            .await
            .insert_evicting(session_id, session_state)
        {
            Ok(_) => Ok(()),
            Err(SessionInsertError::DuplicateKey) => Err(ApiError::InternalServerError),
            Err(SessionInsertError::AllEntriesLeased) => Err(ApiError::TooManyRequests),
        }
    }

    pub(crate) async fn acquire_request_session(
        &self,
        session_id: &Uuid,
    ) -> Result<SessionLease, ApiError> {
        self.session_states
            .lock()
            .await
            .acquire(session_id)
            .ok_or(ApiError::SessionNotFound)
    }

    pub(crate) async fn touch_session(&self, session_id: &Uuid) -> Result<(), ApiError> {
        if self.session_states.lock().await.touch(session_id) {
            Ok(())
        } else {
            Err(ApiError::BadRequest)
        }
    }

    pub(crate) async fn decrypt_session_data(
        &self,
        session_id: &Uuid,
        session_lease: &SessionLease,
        encrypted_data: &str,
    ) -> Result<Vec<u8>, ApiError> {
        tracing::trace!("decrypting session data for session_id: {}", session_id);

        let decoded_data = general_purpose::STANDARD
            .decode(encrypted_data)
            .map_err(|e| {
                tracing::error!("Failed to decode base64 data: {:?}", e);
                ApiError::BadRequest
            })?;

        tracing::trace!("decoded session data length: {}", decoded_data.len());

        if decoded_data.len() < 12 {
            tracing::error!("Decoded data is too short");
            return Err(ApiError::BadRequest);
        }

        let (nonce, ciphertext) = decoded_data.split_at(12);
        let nonce_array: [u8; 12] = nonce.try_into().map_err(|e| {
            tracing::error!("Failed to convert nonce: {:?}", e);
            ApiError::BadRequest
        })?;

        tracing::trace!("nonce: {:?}", nonce_array);
        tracing::trace!("ciphertext length: {}", ciphertext.len());

        let decrypted = session_lease
            .value()
            .decrypt(ciphertext, &nonce_array)
            .map_err(|e| {
                tracing::error!("Decryption failed: {:?}", e);
                e
            })?;

        // Invalid ciphertext must not extend an attacker's retained session.
        self.touch_session(session_id).await?;
        Ok(decrypted)
    }

    pub async fn encrypt_session_data(
        &self,
        session_id: &Uuid,
        data: &[u8],
    ) -> Result<Vec<u8>, ApiError> {
        let session_lease = self
            .session_states
            .lock()
            .await
            .acquire(session_id)
            .ok_or(ApiError::BadRequest)?;

        let session_key = session_lease.value().get_session_key();
        let key = Key::from_slice(session_key.as_ref());

        let nonce_bytes: [u8; 12] = crate::encrypt::generate_random();
        let nonce = Nonce::from_slice(&nonce_bytes);

        let cipher = ChaCha20Poly1305::new(key);

        let mut encrypted_data = nonce_bytes.to_vec();
        encrypted_data.extend_from_slice(
            &cipher
                .encrypt(nonce, data)
                .map_err(|_| ApiError::InternalServerError)?,
        );

        if !self.session_states.lock().await.touch(session_id) {
            return Err(ApiError::BadRequest);
        }
        Ok(encrypted_data)
    }

    async fn create_password_reset_request(
        &self,
        email: String,
        hashed_secret: String,
        project_id: i32,
    ) -> Result<String, Error> {
        let alphanumeric_code = self.generate_alphanumeric_code();

        // Check if the user exists
        match self.db.get_user_by_email(email.clone(), project_id) {
            Ok(user) => {
                // Only password-backed users can use the password reset flow. OAuth-only
                // users recover by signing in with their linked OAuth provider.
                if user.get_email().is_some() && user.password_enc.is_some() {
                    // User exists, proceed with the actual reset request
                    let reset_code_mac = password_reset_code_mac(
                        &self.enclave_key,
                        project_id,
                        user.uuid,
                        &alphanumeric_code,
                    )
                    .map_err(|e| Error::EncryptionError(e.to_string()))?;

                    let new_request = NewPasswordResetRequest::new(
                        user.uuid,
                        hashed_secret,
                        reset_code_mac.to_vec(),
                        24, // 24 hours expiration
                    );

                    self.db.create_password_reset_request(new_request)?;

                    // Send the actual email in the background
                    let app_state = self.clone();
                    let user_email = email.clone();
                    let code = alphanumeric_code.clone();
                    tokio::spawn(async move {
                        if let Err(e) =
                            send_password_reset_email(&app_state, project_id, user_email, code)
                                .await
                        {
                            error!("Failed to send password reset email: {:?}", e);
                        }
                    });
                }
            }
            Err(DBError::UserNotFound) => {
                // User doesn't exist, but we don't want to reveal this information
                // So we'll just log it and return as if everything was successful
                debug!("Password reset requested for nonexistent user");
            }
            Err(e) => {
                // For other errors, we should still log them but not expose them to the user
                error!("Error during password reset request: {:?}", e);
            }
        }

        // Always return the generated code, even if we didn't actually create a request
        Ok(alphanumeric_code)
    }

    async fn confirm_password_reset(
        &self,
        email: String,
        alphanumeric_code: String,
        plaintext_secret: String,
        new_password: String,
        project_id: i32,
    ) -> Result<(), Error> {
        let user = self.db.get_user_by_email(email.clone(), project_id)?;

        // Verify user has an email
        if user.get_email().is_none() || user.password_enc.is_none() {
            return Err(Error::UserNotFound);
        }

        // MAC the provided alphanumeric code for lookup, bound to this user/project.
        let reset_code_mac =
            password_reset_code_mac(&self.enclave_key, project_id, user.uuid, &alphanumeric_code)
                .map_err(|e| Error::EncryptionError(e.to_string()))?;

        let reset_request = self
            .db
            .get_password_reset_request_by_user_id_and_code(user.uuid, reset_code_mac.to_vec())?;

        if let Some(reset_request) = reset_request {
            if reset_request.is_expired() {
                warn!("Password reset expired for user: {}", user.uuid);
                return Err(Error::PasswordResetExpired);
            }

            // Hash the plaintext secret again for comparison
            let hashed_plaintext = generate_reset_hash(plaintext_secret.clone());

            // Compare the hashed values using constant-time comparison
            if hashed_plaintext
                .as_bytes()
                .ct_eq(reset_request.hashed_secret.as_bytes())
                .into()
            {
                let user_seed_words =
                    generate_twelve_word_seed(self.aws_credential_manager.clone())
                        .await?
                        .to_string();
                let (password_hash, encrypted_password) =
                    self.encrypt_user_password_verifier(new_password).await?;
                let new_wrapping = self.new_password_seed_wrapping_for_user(
                    &user,
                    &password_hash,
                    user_seed_words.as_bytes(),
                )?;
                self.verify_new_password_seed_wrapping_for_user(
                    &user,
                    &password_hash,
                    user_seed_words.as_bytes(),
                    &new_wrapping,
                )?;

                self.db.complete_destructive_password_reset(
                    &user,
                    &reset_request,
                    encrypted_password,
                    new_wrapping,
                )?;

                // Send confirmation email in the background
                let app_state = self.clone();
                let user_email = user.email.clone();
                tokio::spawn(async move {
                    if let Err(e) = send_password_reset_confirmation_email(
                        &app_state,
                        project_id,
                        user_email.expect("We checked email had to exist above"),
                    )
                    .await
                    {
                        error!("Failed to send password reset confirmation email: {:?}", e);
                    }
                });

                Ok(())
            } else {
                warn!(
                    "Password verification failed for user {}. Hashes do not match.",
                    user.uuid
                );
                Err(Error::InvalidPasswordResetSecret)
            }
        } else {
            Err(Error::InvalidPasswordResetRequest)
        }
    }

    fn generate_alphanumeric_code(&self) -> String {
        const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        const CODE_LEN: usize = 8;

        let random_bytes: [u8; CODE_LEN] = generate_random();

        random_bytes
            .iter()
            .map(|&b| CHARSET[b as usize % CHARSET.len()] as char)
            .collect()
    }

    pub async fn create_platform_password_reset_request(
        &self,
        email: String,
        hashed_secret: String,
    ) -> Result<String, Error> {
        let alphanumeric_code = self.generate_alphanumeric_code();

        // Check if the platform user exists
        match self.db.get_platform_user_by_email(&email) {
            Ok(Some(platform_user)) => {
                // Only proceed with email if user has a password
                if platform_user.password_enc.is_some() {
                    // User exists, proceed with the actual reset request
                    let secret_key = SecretKey::from_slice(&self.enclave_key)
                        .map_err(|e| Error::EncryptionError(e.to_string()))?;
                    let encrypted_code =
                        encrypt_key_deterministic(&secret_key, alphanumeric_code.as_bytes());

                    let new_request = match NewPlatformPasswordResetRequest::new(
                        platform_user.uuid,
                        hashed_secret,
                        encrypted_code,
                        24, // 24 hours expiration
                    ) {
                        Ok(req) => req,
                        Err(e) => {
                            return Err(Error::EncryptionError(format!(
                                "Invalid expiration hours: {}",
                                e
                            )))
                        }
                    };

                    self.db
                        .create_platform_password_reset_request(new_request)?;

                    // Send the actual email in the background
                    let app_state = self.clone();
                    let user_email = email.clone();
                    let code = alphanumeric_code.clone();
                    spawn(async move {
                        if let Err(e) = send_platform_password_reset_email(
                            &app_state,
                            app_state.resend_api_key.clone(),
                            user_email,
                            code,
                        )
                        .await
                        {
                            error!("Failed to send platform password reset email: {:?}", e);
                        }
                    });
                }
            }
            Ok(None) => {
                // User doesn't exist, but we don't want to reveal this information
                // So we'll just log it and return as if everything was successful
                debug!("Password reset requested for nonexistent platform user");
            }
            Err(e) => {
                // For other errors, we should still log them but not expose them to the user
                error!("Error during platform password reset request: {:?}", e);
            }
        }

        // Always return the generated code, even if we didn't actually create a request
        Ok(alphanumeric_code)
    }

    pub async fn confirm_platform_password_reset(
        &self,
        email: String,
        alphanumeric_code: String,
        plaintext_secret: String,
        new_password: String,
    ) -> Result<(), Error> {
        trace!("Confirm platform password reset for {email}");

        let platform_user = match self.db.get_platform_user_by_email(&email) {
            Ok(Some(user)) => user,
            Ok(None) => return Err(Error::UserNotFound),
            Err(_) => return Err(Error::UserNotFound),
        };

        // Deterministically encrypt the provided alphanumeric code for lookup
        let secret_key = SecretKey::from_slice(&self.enclave_key)
            .map_err(|e| Error::EncryptionError(e.to_string()))?;
        let encrypted_code = encrypt_key_deterministic(&secret_key, alphanumeric_code.as_bytes());

        let reset_request = self
            .db
            .get_platform_password_reset_request_by_user_id_and_code(
                platform_user.uuid,
                encrypted_code,
            )?;

        if let Some(reset_request) = reset_request {
            if reset_request.is_expired() {
                warn!(
                    "Platform password reset expired for user: {}",
                    platform_user.uuid
                );
                return Err(Error::PasswordResetExpired);
            }

            // Hash the plaintext secret again for comparison
            let hashed_plaintext = generate_reset_hash(plaintext_secret.clone());

            // Compare the hashed values using constant-time comparison
            if hashed_plaintext
                .as_bytes()
                .ct_eq(reset_request.hashed_secret.as_bytes())
                .into()
            {
                // Password verification succeeded, continue with reset
                // Hash the new password
                let password_hash = password_auth::generate_hash(new_password.clone());

                // Encrypt the hashed password
                let secret_key = SecretKey::from_slice(&self.enclave_key)
                    .map_err(|e| Error::EncryptionError(e.to_string()))?;
                let encrypted_password =
                    encrypt_with_key(&secret_key, password_hash.as_bytes()).await;

                // Update the platform user's password
                self.db
                    .update_platform_user_password(&platform_user, encrypted_password)?;
                self.db
                    .mark_platform_password_reset_as_complete(&reset_request)?;

                // Send confirmation email in the background
                let user_email = platform_user.email.clone();
                let app_state = self.clone();
                spawn(async move {
                    if let Err(e) = send_platform_password_reset_confirmation_email(
                        &app_state,
                        app_state.resend_api_key.clone(),
                        user_email,
                    )
                    .await
                    {
                        error!(
                            "Failed to send platform password reset confirmation email: {:?}",
                            e
                        );
                    }
                });

                Ok(())
            } else {
                warn!(
                    "Password verification failed for platform user {}. Hashes do not match.",
                    platform_user.uuid
                );
                Err(Error::InvalidPasswordResetSecret)
            }
        } else {
            Err(Error::InvalidPasswordResetRequest)
        }
    }

    async fn update_user_password_and_seed_wrap(
        &self,
        user: &User,
        auth_context: &AuthContext,
        new_password: String,
    ) -> Result<AuthContext, Error> {
        let expected_password_enc = user
            .password_enc
            .as_deref()
            .ok_or(Error::AuthenticationError)?;
        let plaintext_seed = self.decrypt_seed_for_auth_context(user, auth_context)?;
        let (password_hash, encrypted_password) =
            self.encrypt_user_password_verifier(new_password).await?;
        let new_wrapping =
            self.new_password_seed_wrapping_for_user(user, &password_hash, &plaintext_seed)?;
        let new_auth_context = self.password_auth_context_for_user(user, &password_hash)?;

        self.db
            .update_user_password_and_seed_wrap(
                user,
                expected_password_enc,
                encrypted_password,
                new_wrapping,
            )
            .map_err(|err| match err {
                DBError::StaleCredentialState => Error::AuthenticationError,
                err => Error::from(err),
            })?;
        self.verify_seed_wrap_for_auth_context(user, &new_auth_context)?;

        Ok(new_auth_context)
    }

    async fn encrypt_user_password_verifier(
        &self,
        new_password: String,
    ) -> Result<(String, Vec<u8>), Error> {
        let password_hash = password_auth::generate_hash(new_password);
        let secret_key = SecretKey::from_slice(&self.enclave_key)
            .map_err(|e| Error::EncryptionError(e.to_string()))?;
        let encrypted_password = encrypt_with_key(&secret_key, password_hash.as_bytes()).await;
        Ok((password_hash, encrypted_password))
    }

    pub async fn update_platform_user_password(
        &self,
        user: &PlatformUser,
        new_password: String,
    ) -> Result<(), Error> {
        // Hash the new password
        let password_hash = password_auth::generate_hash(new_password);

        // Encrypt the hashed password
        let secret_key = SecretKey::from_slice(&self.enclave_key)
            .map_err(|e| Error::EncryptionError(e.to_string()))?;
        let encrypted_password = encrypt_with_key(&secret_key, password_hash.as_bytes()).await;

        // Update the platform user's password
        self.db
            .update_platform_user_password(user, encrypted_password)
            .map_err(Error::from)
    }

    pub fn frontend_url(&self) -> String {
        self.app_mode.frontend_url().to_string()
    }

    pub fn oauth_callback_url(&self, provider: &str) -> Result<String, url::ParseError> {
        let base_url = Url::parse(self.frontend_url().as_str())?;
        Ok(base_url
            .join(&format!("/auth/{}/callback", provider))?
            .to_string())
    }

    pub async fn create_account_deletion_request(
        &self,
        user_id: Uuid,
        hashed_secret: String,
        project_id: i32,
    ) -> Result<(), Error> {
        // Generate a random UUID for the confirmation code
        let confirmation_code = Uuid::new_v4();

        // Check if the user exists
        match self.db.get_user_by_uuid(user_id) {
            Ok(user) => {
                // Only proceed if user belongs to the project
                if user.project_id != project_id {
                    return Err(Error::UserNotFound);
                }

                // User exists, proceed with the actual deletion request
                let secret_key = SecretKey::from_slice(&self.enclave_key)
                    .map_err(|e| Error::EncryptionError(e.to_string()))?;
                let encrypted_code =
                    encrypt_key_deterministic(&secret_key, confirmation_code.as_bytes());

                let new_request = NewAccountDeletionRequest::new(
                    user.uuid,
                    project_id,
                    hashed_secret,
                    encrypted_code,
                    24, // 24 hours expiration
                );

                self.db.create_account_deletion_request(new_request)?;

                // Send the account deletion email in the background
                let app_state = self.clone();
                let user_email = user.email.clone();
                let code = confirmation_code.to_string();
                tokio::spawn(async move {
                    if let Some(email) = user_email {
                        if let Err(e) =
                            send_account_deletion_email(&app_state, project_id, email, code).await
                        {
                            error!("Failed to send account deletion email: {:?}", e);
                        }
                    }
                });
            }
            Err(DBError::UserNotFound) => {
                // User doesn't exist, but we don't want to reveal this information
                // So we'll just log it and return as if everything was successful
                debug!(
                    "Account deletion requested for non-existent user: {}",
                    user_id
                );
            }
            Err(e) => {
                // For other errors, we should still log them but not expose them to the user
                error!("Error during account deletion request: {:?}", e);
            }
        }

        // Return Ok regardless of whether we actually created a request
        // The confirmation code is only delivered via email
        Ok(())
    }

    pub async fn confirm_account_deletion(
        &self,
        user_id: Uuid,
        confirmation_code: String,
        plaintext_secret: String,
    ) -> Result<(), Error> {
        let user = self.db.get_user_by_uuid(user_id)?;

        // Parse the confirmation code UUID from string
        let confirmation_code_uuid = match Uuid::parse_str(&confirmation_code) {
            Ok(uuid) => uuid,
            Err(_) => return Err(Error::InvalidAccountDeletionRequest),
        };

        // Deterministically encrypt the provided confirmation code for lookup
        let secret_key = SecretKey::from_slice(&self.enclave_key)
            .map_err(|e| Error::EncryptionError(e.to_string()))?;
        let encrypted_code =
            encrypt_key_deterministic(&secret_key, confirmation_code_uuid.as_bytes());

        let deletion_request = self
            .db
            .get_account_deletion_request_by_user_id_and_code(user.uuid, encrypted_code)?;

        if let Some(deletion_request) = deletion_request {
            // Since we're filtering by expiration_time at the database level,
            // this check serves as an additional validation and converts
            // the model-specific error to an application-level error
            if let Err(AccountDeletionError::RequestExpired) = deletion_request.is_expired() {
                warn!("Account deletion request expired for user: {}", user.uuid);
                return Err(Error::AccountDeletionExpired);
            }

            // Hash the plaintext secret again for comparison
            let hashed_plaintext = generate_reset_hash(plaintext_secret.clone());

            // Compare the hashed values using constant-time comparison
            if hashed_plaintext
                .as_bytes()
                .ct_eq(deletion_request.hashed_secret.as_bytes())
                .into()
            {
                // Secret verification succeeded, proceed with account deletion
                // Use the transaction-based method for atomicity
                self.db.mark_and_delete_user(&user, &deletion_request)?;

                // Send confirmation email in the background if the user has an email
                if let Some(email) = user.email.clone() {
                    let app_state = self.clone();
                    let project_id = user.project_id;
                    tokio::spawn(async move {
                        if let Err(e) =
                            send_account_deletion_confirmation_email(&app_state, project_id, email)
                                .await
                        {
                            error!(
                                "Failed to send account deletion confirmation email: {:?}",
                                e
                            );
                        }
                    });
                }

                Ok(())
            } else {
                warn!(
                    "Secret verification failed for user {}. Hashes do not match.",
                    user.uuid
                );
                Err(Error::InvalidAccountDeletionSecret)
            }
        } else {
            Err(Error::InvalidAccountDeletionRequest)
        }
    }

    async fn authenticate_platform_user(
        &self,
        email: &str,
        password: String,
    ) -> Result<Option<PlatformUser>, Error> {
        // Get the platform user
        let platform_user = match self.db.get_platform_user_by_email(email)? {
            Some(user) => user,
            None => {
                warn!("Could not find platform user by provided login identifier");
                return Ok(None);
            }
        };

        // Check if this is an OAuth-only user (no password)
        if platform_user.password_enc.is_none() {
            error!("OAuth-only platform user attempted password login");
            return Ok(None);
        }

        let secret_key = SecretKey::from_slice(&self.enclave_key.clone())
            .map_err(|e| Error::EncryptionError(e.to_string()))?;

        let decrypted_password_bytes =
            decrypt_with_key(&secret_key, platform_user.password_enc.as_ref().unwrap())
                .map_err(|e| Error::EncryptionError(e.to_string()))?;

        let decrypted_password_hash = String::from_utf8(decrypted_password_bytes)
            .map_err(|e| Error::EncryptionError(format!("Failed to decode UTF-8: {}", e)))?;

        // Verifying the password is blocking and potentially slow, so we'll do so via
        // `spawn_blocking`.
        let res = task::spawn_blocking(move || verify_password(password, &decrypted_password_hash))
            .await?;

        match res {
            Ok(_) => Ok(Some(platform_user)),
            Err(_) => Ok(None),
        }
    }

    pub async fn get_project_secret(
        &self,
        project_id: i32,
        key_name: &str,
    ) -> Result<Option<String>, Error> {
        // Get the encrypted secret
        let secret = match self
            .db
            .get_org_project_secret_by_key_name_and_project(key_name, project_id)?
        {
            Some(s) => s,
            None => return Ok(None),
        };

        // Decrypt the secret using the enclave key
        let secret_key = SecretKey::from_slice(&self.enclave_key)
            .map_err(|e| Error::EncryptionError(e.to_string()))?;

        let decrypted_bytes = decrypt_with_key(&secret_key, &secret.secret_enc)
            .map_err(|e| Error::EncryptionError(e.to_string()))?;

        // Always return base64 encoded bytes
        Ok(Some(general_purpose::STANDARD.encode(&decrypted_bytes)))
    }

    pub async fn get_project_resend_api_key(
        &self,
        project_id: i32,
    ) -> Result<Option<String>, Error> {
        // Get the project's Resend API key
        self.get_project_secret(project_id, PROJECT_RESEND_API_KEY)
            .await
    }
}

async fn get_secret(key_name: &str) -> Result<String, Error> {
    let cid = crate::aws_credentials::PARENT_CID;
    let port = crate::aws_credentials::CREDENTIAL_REQUESTER_PORT;

    let sock_addr = VsockAddr::new(cid, port);
    let mut stream = VsockStream::connect(&sock_addr)?;
    crate::aws_credentials::set_vsock_timeouts(&stream, Duration::from_secs(30))?;

    let request = EnclaveRequest {
        request_type: "SecretsManager".to_string(),
        key_name: Some(key_name.to_string()),
    };
    let request_json = serde_json::to_string(&request)?;
    stream.write_all(request_json.as_bytes())?;

    let response = crate::aws_credentials::read_vsock_response_string_limited(
        &mut stream,
        crate::aws_credentials::MAX_VSOCK_RESPONSE_BYTES,
    )?;

    let parent_response: ParentResponse = serde_json::from_str(&response)?;
    if parent_response.response_type == "secret" {
        let secret_json: Value =
            serde_json::from_str(parent_response.response_value.as_str().unwrap())?;

        // Assuming the secret is always a JSON object with a single key-value pair
        if let Some((_, value)) = secret_json.as_object().and_then(|obj| obj.iter().next()) {
            Ok(value.as_str().unwrap_or_default().to_string())
        } else {
            Err(Error::SecretParsingError)
        }
    } else {
        Err(Error::AuthenticationError)
    }
}

async fn get_or_create_enclave_key(
    app_mode: &AppMode,
    aws_credential_manager: Arc<tokio::sync::RwLock<Option<AwsCredentialManager>>>,
    db: Arc<dyn DBConnection + Send + Sync>,
) -> Result<GenKeyResult, Error> {
    let creds = aws_credential_manager
        .read()
        .await
        .clone()
        .expect("non-local mode should have creds")
        .get_credentials()
        .await
        .expect("non-local mode should have creds");

    let aws_kms_key_id = get_kms_key_id(app_mode);

    // Check if the key has been initialized before
    let existing_key = db.get_enclave_secret_by_key(ENCLAVE_KEY_NAME)?;

    resolve_enclave_key_material(
        existing_key.as_ref().map(|secret| secret.value.as_slice()),
        |encrypted_key| {
            let base64_encrypted_key = general_purpose::STANDARD.encode(encrypted_key);
            decrypt_with_kms(
                &creds.region,
                &creds.access_key_id,
                &creds.secret_access_key,
                &creds.token,
                &base64_encrypted_key,
            )
            .map_err(|e| Error::EncryptionError(e.to_string()))
        },
        || {
            create_new_encryption_key(
                &creds.region,
                &creds.access_key_id,
                &creds.secret_access_key,
                &creds.token,
                &aws_kms_key_id,
            )
            .map_err(|e| Error::EncryptionError(e.to_string()))
        },
        |encrypted_key| {
            let new_secret =
                NewEnclaveSecret::new(ENCLAVE_KEY_NAME.to_string(), encrypted_key.to_vec());
            db.create_enclave_secret(new_secret)?;
            Ok(())
        },
    )
}

fn resolve_enclave_key_material<Decrypt, Generate, Persist>(
    existing_encrypted_key: Option<&[u8]>,
    decrypt_existing: Decrypt,
    generate_new: Generate,
    persist_new: Persist,
) -> Result<GenKeyResult, Error>
where
    Decrypt: FnOnce(&[u8]) -> Result<Vec<u8>, Error>,
    Generate: FnOnce() -> Result<GenKeyResult, Error>,
    Persist: FnOnce(&[u8]) -> Result<(), Error>,
{
    if let Some(encrypted_key) = existing_encrypted_key {
        let key = decrypt_existing(encrypted_key)?;
        validate_aes_256_key_length(&key).map_err(|e| Error::EncryptionError(e.to_string()))?;

        return Ok(GenKeyResult {
            key,
            encrypted_key: encrypted_key.to_vec(),
        });
    }

    let key_result = generate_new()?;
    validate_aes_256_key_length(&key_result.key)
        .map_err(|e| Error::EncryptionError(e.to_string()))?;
    persist_new(&key_result.encrypted_key)?;
    Ok(key_result)
}

#[cfg(test)]
mod enclave_key_compatibility_tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn existing_enclave_key_is_decrypted_without_generation_or_write() {
        let encrypted_key = vec![3u8; 48];
        let decrypt_calls = Cell::new(0);
        let generate_calls = Cell::new(0);
        let persist_calls = Cell::new(0);

        let result = resolve_enclave_key_material(
            Some(&encrypted_key),
            |stored| {
                decrypt_calls.set(decrypt_calls.get() + 1);
                assert_eq!(stored, encrypted_key);
                Ok(vec![7u8; 32])
            },
            || {
                generate_calls.set(generate_calls.get() + 1);
                Err(Error::EncryptionError(
                    "generation must not run for an existing key".to_string(),
                ))
            },
            |_| {
                persist_calls.set(persist_calls.get() + 1);
                Err(Error::EncryptionError(
                    "persistence must not run for an existing key".to_string(),
                ))
            },
        )
        .unwrap();

        assert_eq!(result.key, vec![7u8; 32]);
        assert_eq!(result.encrypted_key, encrypted_key);
        assert_eq!(decrypt_calls.get(), 1);
        assert_eq!(generate_calls.get(), 0);
        assert_eq!(persist_calls.get(), 0);
    }

    #[test]
    fn existing_enclave_key_decrypt_failure_fails_before_generation_or_write() {
        let encrypted_key = vec![3u8; 48];
        let generate_calls = Cell::new(0);
        let persist_calls = Cell::new(0);

        let result = resolve_enclave_key_material(
            Some(&encrypted_key),
            |_| {
                Err(Error::EncryptionError(
                    "synthetic decrypt failure".to_string(),
                ))
            },
            || {
                generate_calls.set(generate_calls.get() + 1);
                Err(Error::EncryptionError(
                    "generation must not follow a decrypt failure".to_string(),
                ))
            },
            |_| {
                persist_calls.set(persist_calls.get() + 1);
                Ok(())
            },
        );

        assert!(result.is_err());
        assert_eq!(generate_calls.get(), 0);
        assert_eq!(persist_calls.get(), 0);
    }

    #[test]
    fn invalid_existing_enclave_key_length_fails_before_write() {
        let encrypted_key = vec![3u8; 48];
        let persist_calls = Cell::new(0);

        let result = resolve_enclave_key_material(
            Some(&encrypted_key),
            |_| Ok(vec![7u8; 31]),
            || {
                Err(Error::EncryptionError(
                    "generation must not run for an existing key".to_string(),
                ))
            },
            |_| {
                persist_calls.set(persist_calls.get() + 1);
                Ok(())
            },
        );

        assert!(result.is_err());
        assert_eq!(persist_calls.get(), 0);
    }

    #[test]
    fn new_enclave_key_is_written_only_after_successful_generation() {
        let persist_calls = Cell::new(0);
        let expected_encrypted_key = vec![3u8; 48];

        let result = resolve_enclave_key_material(
            None,
            |_| {
                Err(Error::EncryptionError(
                    "decrypt must not run without an existing key".to_string(),
                ))
            },
            || {
                Ok(GenKeyResult {
                    key: vec![7u8; 32],
                    encrypted_key: expected_encrypted_key.clone(),
                })
            },
            |encrypted_key| {
                persist_calls.set(persist_calls.get() + 1);
                assert_eq!(encrypted_key, expected_encrypted_key);
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(result.key, vec![7u8; 32]);
        assert_eq!(result.encrypted_key, expected_encrypted_key);
        assert_eq!(persist_calls.get(), 1);
    }

    #[test]
    fn failed_new_enclave_key_generation_does_not_write() {
        let persist_calls = Cell::new(0);

        let result = resolve_enclave_key_material(
            None,
            |_| {
                Err(Error::EncryptionError(
                    "decrypt must not run without an existing key".to_string(),
                ))
            },
            || {
                Err(Error::EncryptionError(
                    "synthetic generation failure".to_string(),
                ))
            },
            |_| {
                persist_calls.set(persist_calls.get() + 1);
                Ok(())
            },
        );

        assert!(result.is_err());
        assert_eq!(persist_calls.get(), 0);
    }
}

async fn retrieve_openai_api_key(
    aws_credential_manager: Arc<tokio::sync::RwLock<Option<AwsCredentialManager>>>,
    db: Arc<dyn DBConnection + Send + Sync>,
) -> Result<String, Error> {
    let creds = aws_credential_manager
        .read()
        .await
        .clone()
        .expect("non-local mode should have creds")
        .get_credentials()
        .await
        .expect("non-local mode should have creds");

    // check if the key already exists in the db
    let existing_key = db.get_enclave_secret_by_key(OPENAI_API_KEY_NAME)?;

    if let Some(ref encrypted_key) = existing_key {
        // Convert the stored bytes back to base64
        let base64_encrypted_key = general_purpose::STANDARD.encode(&encrypted_key.value);

        debug!("trying to decrypt base64 encrypted api key");

        // Decrypt the existing key
        let decrypted_bytes = decrypt_with_kms(
            &creds.region,
            &creds.access_key_id,
            &creds.secret_access_key,
            &creds.token,
            &base64_encrypted_key,
        )
        .map_err(|e| Error::EncryptionError(e.to_string()))?;

        // Convert the decrypted bytes to a UTF-8 string
        String::from_utf8(decrypted_bytes)
            .map_err(|e| Error::EncryptionError(format!("Failed to decode UTF-8: {}", e)))
    } else {
        Err(Error::EncryptionError(
            "OpenAI API key not found in the database".to_string(),
        ))
    }
}

async fn get_or_create_jwt_secret(
    app_mode: &AppMode,
    aws_credential_manager: Arc<tokio::sync::RwLock<Option<AwsCredentialManager>>>,
    db: Arc<dyn DBConnection + Send + Sync>,
    enclave_key: &[u8],
) -> Result<Vec<u8>, Error> {
    match app_mode {
        AppMode::Local => {
            // For local mode, use environment variable
            Ok(std::env::var("JWT_SECRET")
                .expect("JWT_SECRET must be set in local mode")
                .into_bytes())
        }
        _ => {
            // Check if JWT secret exists in enclave_secrets
            if let Some(encrypted_jwt_secret) = db.get_enclave_secret_by_key(JWT_SECRET_KEY_NAME)? {
                // Decrypt existing JWT secret
                let secret_key = SecretKey::from_slice(enclave_key)
                    .map_err(|e| Error::EncryptionError(e.to_string()))?;
                decrypt_with_key(&secret_key, &encrypted_jwt_secret.value)
                    .map_err(|e| Error::EncryptionError(e.to_string()))
            } else {
                // Generate new JWT secret
                let jwt_secret = jwt::generate_jwt_secret(aws_credential_manager.clone()).await?;

                // Encrypt and store the new JWT secret
                let secret_key = SecretKey::from_slice(enclave_key)
                    .map_err(|e| Error::EncryptionError(e.to_string()))?;
                let encrypted_jwt_secret = encrypt_with_key(&secret_key, &jwt_secret).await;

                let new_secret =
                    NewEnclaveSecret::new(JWT_SECRET_KEY_NAME.to_string(), encrypted_jwt_secret);
                db.create_enclave_secret(new_secret)?;

                Ok(jwt_secret)
            }
        }
    }
}

fn get_kms_key_id(app_mode: &AppMode) -> String {
    match app_mode {
        AppMode::Prod => "alias/open-secret-prod-enclave".to_string(),
        AppMode::Preview => "alias/open-secret-preview1-enclave".to_string(),
        AppMode::Dev => "alias/open-secret-dev-enclave".to_string(),
        AppMode::Custom(env_name) => format!("alias/open-secret-{}-enclave", env_name),
        AppMode::Local => unreachable!("shouldn't use kms in local mode"),
    }
}

fn is_default_openai_domain(domain: &str) -> bool {
    domain.contains("openai.com")
}

fn default_os_flags_base_url(app_mode: &AppMode) -> String {
    match app_mode {
        AppMode::Prod => "https://flags.opensecret.cloud".to_string(),
        _ => "https://flags-dev.opensecret.cloud".to_string(),
    }
}

async fn retrieve_resend_api_key(
    aws_credential_manager: Arc<tokio::sync::RwLock<Option<AwsCredentialManager>>>,
    db: Arc<dyn DBConnection + Send + Sync>,
) -> Result<Option<String>, Error> {
    let creds = aws_credential_manager
        .read()
        .await
        .clone()
        .expect("non-local mode should have creds")
        .get_credentials()
        .await
        .expect("non-local mode should have creds");

    // check if the key already exists in the db
    let existing_key = db.get_enclave_secret_by_key(RESEND_API_KEY_NAME)?;

    if let Some(ref encrypted_key) = existing_key {
        // Convert the stored bytes back to base64
        let base64_encrypted_key = general_purpose::STANDARD.encode(&encrypted_key.value);

        debug!("trying to decrypt base64 encrypted Resend API key");

        // Decrypt the existing key
        let decrypted_bytes = decrypt_with_kms(
            &creds.region,
            &creds.access_key_id,
            &creds.secret_access_key,
            &creds.token,
            &base64_encrypted_key,
        )
        .map_err(|e| Error::EncryptionError(e.to_string()))?;

        // Convert the decrypted bytes to a UTF-8 string
        String::from_utf8(decrypted_bytes)
            .map_err(|e| Error::EncryptionError(format!("Failed to decode UTF-8: {}", e)))
            .map(Some)
    } else {
        tracing::info!("Resend API key not found in the database");
        Ok(None)
    }
}

async fn retrieve_github_client_id(
    aws_credential_manager: Arc<tokio::sync::RwLock<Option<AwsCredentialManager>>>,
    db: Arc<dyn DBConnection + Send + Sync>,
) -> Result<Option<String>, Error> {
    let creds = aws_credential_manager
        .read()
        .await
        .clone()
        .expect("non-local mode should have creds")
        .get_credentials()
        .await
        .expect("non-local mode should have creds");

    // check if the key already exists in the db
    let existing_key = db.get_enclave_secret_by_key(GITHUB_CLIENT_ID_NAME)?;

    if let Some(ref encrypted_key) = existing_key {
        // Convert the stored bytes back to base64
        let base64_encrypted_key = general_purpose::STANDARD.encode(&encrypted_key.value);

        debug!("trying to decrypt base64 encrypted GitHub client ID");

        // Decrypt the existing key
        let decrypted_bytes = decrypt_with_kms(
            &creds.region,
            &creds.access_key_id,
            &creds.secret_access_key,
            &creds.token,
            &base64_encrypted_key,
        )
        .map_err(|e| Error::EncryptionError(e.to_string()))?;

        // Convert the decrypted bytes to a UTF-8 string
        String::from_utf8(decrypted_bytes)
            .map_err(|e| Error::EncryptionError(format!("Failed to decode UTF-8: {}", e)))
            .map(Some)
    } else {
        tracing::info!("GitHub client ID not found in the database");
        Ok(None)
    }
}

async fn retrieve_github_client_secret(
    aws_credential_manager: Arc<tokio::sync::RwLock<Option<AwsCredentialManager>>>,
    db: Arc<dyn DBConnection + Send + Sync>,
) -> Result<Option<String>, Error> {
    let creds = aws_credential_manager
        .read()
        .await
        .clone()
        .expect("non-local mode should have creds")
        .get_credentials()
        .await
        .expect("non-local mode should have creds");

    // check if the key already exists in the db
    let existing_key = db.get_enclave_secret_by_key(GITHUB_CLIENT_SECRET_NAME)?;

    if let Some(ref encrypted_key) = existing_key {
        // Convert the stored bytes back to base64
        let base64_encrypted_key = general_purpose::STANDARD.encode(&encrypted_key.value);

        debug!("trying to decrypt base64 encrypted GitHub client secret");

        // Decrypt the existing key
        let decrypted_bytes = decrypt_with_kms(
            &creds.region,
            &creds.access_key_id,
            &creds.secret_access_key,
            &creds.token,
            &base64_encrypted_key,
        )
        .map_err(|e| Error::EncryptionError(e.to_string()))?;

        // Convert the decrypted bytes to a UTF-8 string
        String::from_utf8(decrypted_bytes)
            .map_err(|e| Error::EncryptionError(format!("Failed to decode UTF-8: {}", e)))
            .map(Some)
    } else {
        tracing::info!("GitHub client secret not found in the database");
        Ok(None)
    }
}

async fn retrieve_google_client_secret(
    aws_credential_manager: Arc<tokio::sync::RwLock<Option<AwsCredentialManager>>>,
    db: Arc<dyn DBConnection + Send + Sync>,
) -> Result<Option<String>, Error> {
    let creds = aws_credential_manager
        .read()
        .await
        .clone()
        .expect("non-local mode should have creds")
        .get_credentials()
        .await
        .expect("non-local mode should have creds");

    // check if the key already exists in the db
    let existing_key = db.get_enclave_secret_by_key(GOOGLE_CLIENT_SECRET_NAME)?;

    if let Some(ref encrypted_key) = existing_key {
        // Convert the stored bytes back to base64
        let base64_encrypted_key = general_purpose::STANDARD.encode(&encrypted_key.value);

        debug!("trying to decrypt base64 encrypted Google client secret");

        // Decrypt the existing key
        let decrypted_bytes = decrypt_with_kms(
            &creds.region,
            &creds.access_key_id,
            &creds.secret_access_key,
            &creds.token,
            &base64_encrypted_key,
        )
        .map_err(|e| Error::EncryptionError(e.to_string()))?;

        // Convert the decrypted bytes to a UTF-8 string
        String::from_utf8(decrypted_bytes)
            .map_err(|e| Error::EncryptionError(format!("Failed to decode UTF-8: {}", e)))
            .map(Some)
    } else {
        tracing::info!("Google client secret not found in the database");
        Ok(None)
    }
}

async fn retrieve_google_client_id(
    aws_credential_manager: Arc<tokio::sync::RwLock<Option<AwsCredentialManager>>>,
    db: Arc<dyn DBConnection + Send + Sync>,
) -> Result<Option<String>, Error> {
    let creds = aws_credential_manager
        .read()
        .await
        .clone()
        .expect("non-local mode should have creds")
        .get_credentials()
        .await
        .expect("non-local mode should have creds");

    // check if the key already exists in the db
    let existing_key = db.get_enclave_secret_by_key(GOOGLE_CLIENT_ID_NAME)?;

    if let Some(ref encrypted_key) = existing_key {
        // Convert the stored bytes back to base64
        let base64_encrypted_key = general_purpose::STANDARD.encode(&encrypted_key.value);

        debug!("trying to decrypt base64 encrypted Google client ID");

        // Decrypt the existing key
        let decrypted_bytes = decrypt_with_kms(
            &creds.region,
            &creds.access_key_id,
            &creds.secret_access_key,
            &creds.token,
            &base64_encrypted_key,
        )
        .map_err(|e| Error::EncryptionError(e.to_string()))?;

        // Convert the decrypted bytes to a UTF-8 string
        String::from_utf8(decrypted_bytes)
            .map_err(|e| Error::EncryptionError(format!("Failed to decode UTF-8: {}", e)))
            .map(Some)
    } else {
        tracing::info!("Google client ID not found in the database");
        Ok(None)
    }
}

struct AsyncRngWrapper {
    inner: CustomRng,
}

impl AsyncRngWrapper {
    fn new(inner: CustomRng) -> Self {
        AsyncRngWrapper { inner }
    }
}

impl RngCore for AsyncRngWrapper {
    fn next_u32(&mut self) -> u32 {
        futures::executor::block_on(self.inner.next_u32())
    }

    fn next_u64(&mut self) -> u64 {
        futures::executor::block_on(self.inner.next_u64())
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        futures::executor::block_on(self.inner.fill_bytes(dest))
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        futures::executor::block_on(self.inner.fill_bytes(dest));
        Ok(())
    }
}

// Implement CryptoRng for AsyncRngWrapper
impl CryptoRng for AsyncRngWrapper {}

pub fn generate_reset_hash(password: String) -> String {
    let mut hasher = Sha256::new();
    hasher.update(password.as_bytes());
    format!("{:x}", hasher.finalize())
}

async fn retrieve_billing_api_key(
    aws_credential_manager: Arc<tokio::sync::RwLock<Option<AwsCredentialManager>>>,
    db: Arc<dyn DBConnection + Send + Sync>,
) -> Result<Option<String>, Error> {
    let creds = aws_credential_manager
        .read()
        .await
        .clone()
        .expect("non-local mode should have creds")
        .get_credentials()
        .await
        .expect("non-local mode should have creds");

    // check if the key already exists in the db
    let existing_key = db.get_enclave_secret_by_key(BILLING_API_KEY_NAME)?;

    if let Some(ref encrypted_key) = existing_key {
        // Convert the stored bytes back to base64
        let base64_encrypted_key = general_purpose::STANDARD.encode(&encrypted_key.value);

        debug!("trying to decrypt base64 encrypted billing API key");

        // Decrypt the existing key
        let decrypted_bytes = decrypt_with_kms(
            &creds.region,
            &creds.access_key_id,
            &creds.secret_access_key,
            &creds.token,
            &base64_encrypted_key,
        )
        .map_err(|e| Error::EncryptionError(e.to_string()))?;

        // Convert the decrypted bytes to a UTF-8 string
        String::from_utf8(decrypted_bytes)
            .map_err(|e| Error::EncryptionError(format!("Failed to decode UTF-8: {}", e)))
            .map(Some)
    } else {
        tracing::info!("Billing API key not found in the database");
        Ok(None)
    }
}

async fn retrieve_billing_server_url(
    aws_credential_manager: Arc<tokio::sync::RwLock<Option<AwsCredentialManager>>>,
    db: Arc<dyn DBConnection + Send + Sync>,
) -> Result<Option<String>, Error> {
    let creds = aws_credential_manager
        .read()
        .await
        .clone()
        .expect("non-local mode should have creds")
        .get_credentials()
        .await
        .expect("non-local mode should have creds");

    // check if the url already exists in the db
    let existing_url = db.get_enclave_secret_by_key(BILLING_SERVER_URL_NAME)?;

    if let Some(ref encrypted_url) = existing_url {
        // Convert the stored bytes back to base64
        let base64_encrypted_url = general_purpose::STANDARD.encode(&encrypted_url.value);

        debug!("trying to decrypt base64 encrypted billing server URL");

        // Decrypt the existing url
        let decrypted_bytes = decrypt_with_kms(
            &creds.region,
            &creds.access_key_id,
            &creds.secret_access_key,
            &creds.token,
            &base64_encrypted_url,
        )
        .map_err(|e| Error::EncryptionError(e.to_string()))?;

        // Convert the decrypted bytes to a UTF-8 string
        String::from_utf8(decrypted_bytes)
            .map_err(|e| Error::EncryptionError(format!("Failed to decode UTF-8: {}", e)))
            .map(Some)
    } else {
        tracing::info!("Billing server URL not found in the database");
        Ok(None)
    }
}

async fn retrieve_os_flags_api_key(
    aws_credential_manager: Arc<tokio::sync::RwLock<Option<AwsCredentialManager>>>,
    db: Arc<dyn DBConnection + Send + Sync>,
) -> Result<Option<String>, Error> {
    let creds = aws_credential_manager
        .read()
        .await
        .clone()
        .expect("non-local mode should have creds")
        .get_credentials()
        .await
        .expect("non-local mode should have creds");

    let existing_key = db.get_enclave_secret_by_key(OS_FLAGS_API_KEY_NAME)?;

    if let Some(ref encrypted_key) = existing_key {
        let base64_encrypted_key = general_purpose::STANDARD.encode(&encrypted_key.value);

        debug!("trying to decrypt base64 encrypted os-flags API key");

        let decrypted_bytes = decrypt_with_kms(
            &creds.region,
            &creds.access_key_id,
            &creds.secret_access_key,
            &creds.token,
            &base64_encrypted_key,
        )
        .map_err(|e| Error::EncryptionError(e.to_string()))?;

        String::from_utf8(decrypted_bytes)
            .map_err(|e| Error::EncryptionError(format!("Failed to decode UTF-8: {}", e)))
            .map(Some)
    } else {
        tracing::info!("os-flags API key not found in the database");
        Ok(None)
    }
}

async fn retrieve_os_flags_base_url(
    aws_credential_manager: Arc<tokio::sync::RwLock<Option<AwsCredentialManager>>>,
    db: Arc<dyn DBConnection + Send + Sync>,
) -> Result<Option<String>, Error> {
    let creds = aws_credential_manager
        .read()
        .await
        .clone()
        .expect("non-local mode should have creds")
        .get_credentials()
        .await
        .expect("non-local mode should have creds");

    let existing_url = db.get_enclave_secret_by_key(OS_FLAGS_BASE_URL_NAME)?;

    if let Some(ref encrypted_url) = existing_url {
        let base64_encrypted_url = general_purpose::STANDARD.encode(&encrypted_url.value);

        debug!("trying to decrypt base64 encrypted os-flags base URL");

        let decrypted_bytes = decrypt_with_kms(
            &creds.region,
            &creds.access_key_id,
            &creds.secret_access_key,
            &creds.token,
            &base64_encrypted_url,
        )
        .map_err(|e| Error::EncryptionError(e.to_string()))?;

        String::from_utf8(decrypted_bytes)
            .map_err(|e| Error::EncryptionError(format!("Failed to decode UTF-8: {}", e)))
            .map(Some)
    } else {
        tracing::info!("os-flags base URL not found in the database");
        Ok(None)
    }
}

async fn retrieve_kagi_api_key(
    aws_credential_manager: Arc<tokio::sync::RwLock<Option<AwsCredentialManager>>>,
    db: Arc<dyn DBConnection + Send + Sync>,
) -> Result<Option<String>, Error> {
    let creds = aws_credential_manager
        .read()
        .await
        .clone()
        .expect("non-local mode should have creds")
        .get_credentials()
        .await
        .expect("non-local mode should have creds");

    let existing_key = db.get_enclave_secret_by_key(KAGI_API_KEY_NAME)?;

    if let Some(ref encrypted_key) = existing_key {
        let base64_encrypted_key = general_purpose::STANDARD.encode(&encrypted_key.value);

        debug!("trying to decrypt base64 encrypted Kagi API key");

        let decrypted_bytes = decrypt_with_kms(
            &creds.region,
            &creds.access_key_id,
            &creds.secret_access_key,
            &creds.token,
            &base64_encrypted_key,
        )
        .map_err(|e| Error::EncryptionError(e.to_string()))?;

        String::from_utf8(decrypted_bytes)
            .map_err(|e| Error::EncryptionError(format!("Failed to decode UTF-8: {}", e)))
            .map(Some)
    } else {
        tracing::info!("Kagi API key not found in the database");
        Ok(None)
    }
}

fn application_routes(app_state: Arc<AppState>) -> Router<()> {
    protected_routes(app_state.clone())
        .route_layer(from_fn_with_state(app_state.clone(), validate_jwt))
        .merge(recovery_router(app_state.clone()))
        .merge(login_routes(app_state.clone()))
        .merge(
            openai_routes(app_state.clone())
                .route_layer(from_fn_with_state(app_state.clone(), validate_openai_auth)),
        )
        .merge(
            openai_models_routes(app_state.clone()).route_layer(from_fn_with_state(
                app_state.clone(),
                validate_optional_openai_auth,
            )),
        )
        .merge(
            responses_routes(app_state.clone())
                .route_layer(from_fn_with_state(app_state.clone(), validate_jwt)),
        )
        .merge(
            web_routes(app_state.clone())
                .route_layer(from_fn_with_state(app_state.clone(), validate_jwt)),
        )
        .merge(
            conversations_routes(app_state.clone())
                .route_layer(from_fn_with_state(app_state.clone(), validate_jwt)),
        )
        .merge(
            conversation_projects_routes(app_state.clone())
                .route_layer(from_fn_with_state(app_state.clone(), validate_jwt)),
        )
        .merge(
            instructions_routes(app_state.clone())
                .route_layer(from_fn_with_state(app_state.clone(), validate_jwt)),
        )
        .merge(oauth_routes(app_state.clone()))
        .merge(platform_login_routes(app_state.clone()))
        .merge(
            platform_routes(app_state.clone())
                .route_layer(from_fn_with_state(app_state.clone(), validate_platform_jwt)),
        )
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    dotenv::dotenv().ok();
    http_client::install_ring_crypto_provider().map_err(|_| Error::CryptoProvider)?;

    let app_mode = std::env::var("APP_MODE")
        .unwrap_or_else(|_| "local".to_string())
        .parse::<AppMode>()
        .expect("Invalid APP_MODE");

    init_tracing(&app_mode)?;
    info!(app_mode = %app_mode, "OpenSecret logging initialized");
    debug!("Starting application");

    let aws_credential_manager = if app_mode != AppMode::Local {
        Arc::new(RwLock::new(Some(AwsCredentialManager::new())))
    } else {
        Arc::new(RwLock::new(None))
    };

    if app_mode != AppMode::Local {
        // Wait for initial credentials with a timeout
        let timeout = Duration::from_secs(60); // 1 minute timeout
        match tokio::time::timeout(
            timeout,
            aws_credential_manager
                .read()
                .await
                .as_ref()
                .expect("non-local mode should have creds")
                .wait_for_credentials(),
        )
        .await
        {
            Ok(_) => tracing::info!("Initial AWS credentials fetched successfully"),
            Err(_) => {
                tracing::error!("Timed out waiting for initial AWS credentials");
                return Err(Error::AwsCredentialError(AwsCredentialError::Timeout));
            }
        }

        let manager = aws_credential_manager
            .read()
            .await
            .as_ref()
            .expect("non-local mode should have creds")
            .clone();

        // Spawn a task to refresh AWS credentials
        let refresh_manager = manager.clone();
        tokio::spawn(async move {
            let refresh_interval = Duration::from_secs(5 * 60 * 60); // 5 hours
            let retry_interval = Duration::from_secs(5); // 5 seconds

            loop {
                tracing::info!("Refreshing AWS credentials");

                let fetch_res = refresh_manager.fetch_credentials().await;

                match fetch_res {
                    Ok(_creds) => {
                        tracing::info!("AWS credentials refreshed successfully");
                        tokio::time::sleep(refresh_interval).await;
                    }
                    Err(e) => {
                        tracing::error!("Failed to refresh AWS credentials: {:?}", e);
                        tracing::info!("Retrying in 5 seconds...");
                        tokio::time::sleep(retry_interval).await;
                    }
                }
            }
        });
    }

    let pg_url = if app_mode != AppMode::Local {
        // Fetch database URL from Secrets Manager using the new AwsCredentialManager
        let secret_name = match app_mode {
            AppMode::Prod => "opensecret_prod_database_url",
            AppMode::Preview => "opensecret_preview1_database_url",
            AppMode::Dev => "opensecret_dev_database_url",
            AppMode::Custom(ref env_name) => {
                let name = format!("opensecret_{}_database_url", env_name);
                Box::leak(name.into_boxed_str())
            }
            AppMode::Local => unreachable!("just checked"),
        };
        match get_secret(secret_name).await {
            Ok(encrypted_url) => {
                let creds = aws_credential_manager
                    .read()
                    .await
                    .clone()
                    .expect("non-local mode should have creds")
                    .get_credentials()
                    .await
                    .expect("should have just waited for credentials");

                tracing::info!("Retrieved and decrypting database URL from Secrets Manager");
                let url_vec = decrypt_with_kms(
                    &creds.region,
                    &creds.access_key_id,
                    &creds.secret_access_key,
                    &creds.token,
                    &encrypted_url,
                )
                .map_err(|e| {
                    tracing::error!("Failed to decrypt database URL: {:?}", e);
                    Error::EncryptionError(e.to_string())
                })?;

                String::from_utf8(url_vec).expect("should parse url")
            }
            Err(e) => {
                tracing::error!(
                    "Failed to retrieve database URL from Secrets Manager: {:?}",
                    e
                );
                return Err(e);
            }
        }
    } else {
        std::env::var("DATABASE_URL").expect("DATABASE_URL must be set")
    };

    let db = setup_db(pg_url);

    // enclave secret retrieval
    let enclave_key = if app_mode != AppMode::Local {
        let enclave_key =
            get_or_create_enclave_key(&app_mode, aws_credential_manager.clone(), db.clone())
                .await?;
        enclave_key.key
    } else {
        let enclave_key =
            std::env::var("ENCLAVE_SECRET_MOCK").expect("needs ENCLAVE_SECRET_MOCK in local mode");
        let enclave_key: [u8; 32] = hex::decode(enclave_key)
            .unwrap()
            .try_into()
            .expect("ENCLAVE_SECRET_MOCK must be 32 bytes");
        enclave_key.to_vec()
    };

    let openai_api_base =
        env::var("OPENAI_API_BASE").unwrap_or_else(|_| "https://api.openai.com".to_string());

    let openai_api_key = if is_default_openai_domain(&openai_api_base) {
        if app_mode != AppMode::Local {
            Some(
                retrieve_openai_api_key(aws_credential_manager.clone(), db.clone())
                    .await
                    .expect("OpenAI API key should be retrieved correctly"),
            )
        } else {
            Some(
                std::env::var("OPENAI_API_KEY")
                    .expect("OPENAI_API_KEY must be set for OpenAI domain"),
            )
        }
    } else {
        None // No API key needed if not using OpenAI's domain
    };

    // Tinfoil is a hard runtime requirement for OpenSecret. In local mode the
    // managed workspace symlinks the shared key into this gitignored path.
    let tinfoil_api_key = match env::var("TINFOIL_API_KEY") {
        Ok(key) if !key.trim().is_empty() => key.trim().to_string(),
        _ if app_mode == AppMode::Local => std::fs::read_to_string(
            ".local/secrets/tinfoil_api_key",
        )
        .map(|key| key.trim().to_string())
        .map_err(|error| {
            Error::BuilderError(format!(
                "TINFOIL_API_KEY is required (or write .local/secrets/tinfoil_api_key in local mode): {error}"
            ))
        })?,
        _ => {
            return Err(Error::BuilderError(
                "TINFOIL_API_KEY is required".to_string(),
            ))
        }
    };
    if tinfoil_api_key.is_empty() {
        return Err(Error::BuilderError(
            "TINFOIL_API_KEY must not be empty".to_string(),
        ));
    }

    let jwt_secret = get_or_create_jwt_secret(
        &app_mode,
        aws_credential_manager.clone(),
        db.clone(),
        &enclave_key,
    )
    .await?;

    let resend_api_key = if app_mode != AppMode::Local {
        retrieve_resend_api_key(aws_credential_manager.clone(), db.clone()).await?
    } else {
        optional_env("RESEND_API_KEY")
    };

    let github_client_secret = if app_mode != AppMode::Local {
        retrieve_github_client_secret(aws_credential_manager.clone(), db.clone()).await?
    } else {
        optional_env("GITHUB_CLIENT_SECRET")
    };

    let github_client_id = if app_mode != AppMode::Local {
        retrieve_github_client_id(aws_credential_manager.clone(), db.clone()).await?
    } else {
        optional_env("GITHUB_CLIENT_ID")
    };

    let google_client_secret = if app_mode != AppMode::Local {
        retrieve_google_client_secret(aws_credential_manager.clone(), db.clone()).await?
    } else {
        optional_env("GOOGLE_CLIENT_SECRET")
    };

    let google_client_id = if app_mode != AppMode::Local {
        retrieve_google_client_id(aws_credential_manager.clone(), db.clone()).await?
    } else {
        optional_env("GOOGLE_CLIENT_ID")
    };

    let sqs_queue_maple_events_url = if app_mode != AppMode::Local {
        // Get from database if in enclave mode
        if let Some(ref encrypted_url) =
            db.get_enclave_secret_by_key("sqs_queue_maple_events_url")?
        {
            let creds = aws_credential_manager
                .read()
                .await
                .clone()
                .expect("non-local mode should have creds")
                .get_credentials()
                .await
                .expect("should have just waited for credentials");

            // Decrypt the URL
            let url_vec = decrypt_with_kms(
                &creds.region,
                &creds.access_key_id,
                &creds.secret_access_key,
                &creds.token,
                &general_purpose::STANDARD.encode(&encrypted_url.value),
            )
            .map_err(|e| {
                tracing::error!("Failed to decrypt SQS queue URL: {:?}", e);
                Error::EncryptionError(e.to_string())
            })?;

            Some(String::from_utf8(url_vec).expect("should parse url"))
        } else {
            // URL not found in database - this is optional so we'll return None
            None
        }
    } else {
        // In local mode, get from environment variable
        std::env::var("SQS_QUEUE_MAPLE_EVENTS_URL").ok()
    };

    let billing_api_key = if app_mode != AppMode::Local {
        // Get from database if in enclave mode
        retrieve_billing_api_key(aws_credential_manager.clone(), db.clone()).await?
    } else {
        optional_env("BILLING_API_KEY")
    };

    let billing_server_url = if app_mode != AppMode::Local {
        // Get from database if in enclave mode
        retrieve_billing_server_url(aws_credential_manager.clone(), db.clone()).await?
    } else {
        optional_env("BILLING_SERVER_URL")
    };

    let kagi_api_key = if app_mode != AppMode::Local {
        retrieve_kagi_api_key(aws_credential_manager.clone(), db.clone()).await?
    } else {
        optional_env("KAGI_API_KEY")
    };

    let os_flags_api_key = if app_mode != AppMode::Local {
        retrieve_os_flags_api_key(aws_credential_manager.clone(), db.clone()).await?
    } else {
        optional_env("OS_FLAGS_API_KEY")
    };

    let os_flags_base_url = if app_mode != AppMode::Local {
        match retrieve_os_flags_base_url(aws_credential_manager.clone(), db.clone()).await? {
            Some(url) => Some(url),
            None => {
                let default_url = default_os_flags_base_url(&app_mode);
                tracing::info!(
                    "os-flags base URL not configured; defaulting to {}",
                    default_url
                );
                Some(default_url)
            }
        }
    } else {
        optional_env("OS_FLAGS_BASE_URL")
    };

    let app_state = AppStateBuilder::default()
        .app_mode(app_mode.clone())
        .db(db)
        .enclave_key(enclave_key)
        .aws_credential_manager(aws_credential_manager)
        .openai_api_key(openai_api_key)
        .openai_api_base(openai_api_base)
        .tinfoil_api_key(tinfoil_api_key)
        .jwt_secret(jwt_secret)
        .resend_api_key(resend_api_key)
        .github_client_secret(github_client_secret.clone())
        .github_client_id(github_client_id.clone())
        .google_client_secret(google_client_secret.clone())
        .google_client_id(google_client_id.clone())
        .sqs_queue_maple_events_url(sqs_queue_maple_events_url)
        .billing_api_key(billing_api_key)
        .billing_server_url(billing_server_url)
        .os_flags_base_url(os_flags_base_url)
        .os_flags_api_key(os_flags_api_key)
        .kagi_api_key(kagi_api_key)
        .build()
        .await?;
    tracing::info!("App state created, app_mode: {:?}", app_mode);

    let app_state = Arc::new(app_state);

    // Run migrations before starting the server
    migrations::run_migrations(
        &app_state,
        github_client_secret.clone(),
        google_client_secret.clone(),
        github_client_id.clone(),
        google_client_id.clone(),
    )
    .await?;

    let cors = CorsLayer::new()
        // allow all method types
        .allow_methods(Any)
        // allow all headers
        .allow_headers(Any)
        // expose the machine-readable recovery contract to browser SDKs
        .expose_headers([
            HeaderName::from_static(ERROR_CONTRACT_HEADER),
            HeaderName::from_static(ERROR_CODE_HEADER),
            HeaderName::from_static(CLIENT_REPLAY_HEADER),
            header::RETRY_AFTER,
        ])
        // allow requests from any origin
        .allow_origin(Any);

    let application = application_routes(app_state.clone());
    let v2_application = application
        .clone()
        .merge(health_routes())
        .merge(native_handoff_routes(app_state.clone()))
        .layer(from_fn(add_error_contract_header));
    let transport_v2_gateway =
        transport_v2::gateway::TransportV2Gateway::new(app_state.clone(), v2_application);

    let app = application
        .merge(health_routes())
        .merge(attestation_routes::router(app_state.clone()))
        .merge(transport_v2_gateway.router())
        .layer(cors)
        .layer(from_fn(add_error_contract_header));

    let bind_addr =
        std::env::var("OPENSECRET_BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".to_string());
    let listener = tokio::net::TcpListener::bind(&bind_addr).await.unwrap();

    tracing::info!("Listening on http://{}", bind_addr);

    tokio::select! {
        result = axum::serve(listener, app.into_make_service()) => Ok(result?),
        _ = secret_cache_maintenance::run(app_state) => {
            unreachable!("secret cache maintenance runs until the server stops")
        }
        _ = transport_v2_gateway.run_maintenance() => {
            unreachable!("transport-v2 session maintenance runs until the server stops")
        }
    }
}
