use crate::aws_credentials::AwsCredentialManager;
use crate::encrypt::generate_random_bytes_from_enclave;
use crate::Error;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Request, State},
    http::header,
    middleware::Next,
    response::IntoResponse,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, Duration, Utc};
use jwt_compact::{alg::Es256k, prelude::*, AlgorithmExt};
use secp256k1::{All, PublicKey, Secp256k1, SecretKey};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use uuid::Uuid;

use crate::db::DBError;
use crate::{ApiError, AppState};
use jsonwebtoken::{
    encode as jwt_encode, Algorithm as JwtAlgorithm, EncodingKey, Header as JwtHeader,
};
use url::Url;

use crate::models::{platform_users::PlatformUser, users::User};
use crate::{
    transport_v2::{
        crypto::SessionId,
        envelope::{Credential, CredentialKind, RequestId},
    },
    web::encryption_middleware::TransportSession,
};

pub const USER_ACCESS: &str = "access";
pub const USER_REFRESH: &str = "refresh";

pub const PLATFORM_ACCESS: &str = "platform_access";
pub const PLATFORM_REFRESH: &str = "platform_refresh";
// Every internal Transport V2 audience intentionally exceeds the existing
// 50-byte third-party audience limit. Reserving these values therefore does
// not newly reject any audience that a V1 caller could issue before Transport
// V2 existed.
pub const TRANSPORT_V2_USER_ACCESS: &str = "urn:opensecret:internal:transport-v2:user:access-token";
pub const TRANSPORT_V2_USER_REFRESH: &str =
    "urn:opensecret:internal:transport-v2:user:refresh-token";
pub const TRANSPORT_V2_PLATFORM_ACCESS: &str =
    "urn:opensecret:internal:transport-v2:platform:access-token";
pub const TRANSPORT_V2_PLATFORM_REFRESH: &str =
    "urn:opensecret:internal:transport-v2:platform:refresh-token";
pub const TRANSPORT_V2_NATIVE_HANDOFF: &str =
    "urn:opensecret:internal:transport-v2:user:native-handoff";
const TRANSPORT_V2_ISSUER: &str = "urn:opensecret:transport-v2";
const TRANSPORT_V2_VERSION: u8 = 2;
const TRANSPORT_V2_NATIVE_HANDOFF_TTL: Duration = Duration::minutes(5);
const TRANSPORT_V2_NATIVE_HANDOFF_CLOCK_LEEWAY: Duration = Duration::seconds(30);
pub(crate) const TRANSPORT_V2_NATIVE_HANDOFF_MAX_BYTES: usize = 4 * 1024;

pub const USER_TOKEN_FORMAT_V2: u8 = 2;

#[derive(Debug, Clone)]
pub enum TokenType {
    Access,
    Refresh,
    TransportV2Access,
    TransportV2Refresh,
    ThirdParty { aud: Option<String>, azp: String },
}

impl TokenType {
    pub(crate) const fn access_for_transport(is_transport_v2: bool) -> Self {
        if is_transport_v2 {
            Self::TransportV2Access
        } else {
            Self::Access
        }
    }

    pub(crate) const fn refresh_for_transport(is_transport_v2: bool) -> Self {
        if is_transport_v2 {
            Self::TransportV2Refresh
        } else {
            Self::Refresh
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewToken {
    pub token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct NativeHandoffClaims {
    sub: String,
    iss: String,
    aud: String,
    version: u8,
    kind: String,
    #[serde(rename = "tf")]
    token_format: u8,
    #[serde(rename = "am")]
    auth_method: String,
    #[serde(rename = "pid")]
    project_id: i32,
    #[serde(rename = "ab")]
    auth_binding: String,
    #[serde(rename = "sid")]
    target_session_id: String,
    #[serde(rename = "rid")]
    request_id: String,
}

pub(crate) struct IssuedNativeHandoffGrant {
    pub(crate) grant: String,
    pub(crate) expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct JwtKeys {
    signing_key: SecretKey, // For ES256K
    secp: Secp256k1<All>,
}

impl JwtKeys {
    pub fn new(secret_bytes: Vec<u8>) -> Result<Self, Error> {
        // check for size before slicing
        if secret_bytes.len() < 32 {
            return Err(Error::EncryptionError(
                "Insufficient key length: must be at least 32 bytes".to_string(),
            ));
        }

        let secp = Secp256k1::new(); // Creates All context
        let signing_key = SecretKey::from_slice(&secret_bytes[..32])
            .map_err(|e| Error::EncryptionError(e.to_string()))?;

        Ok(Self { signing_key, secp })
    }

    pub fn public_key(&self) -> PublicKey {
        PublicKey::from_secret_key(&self.secp, &self.signing_key)
    }
}

#[derive(Debug, PartialEq, Serialize, Deserialize, Clone)]
pub struct CustomClaims {
    pub sub: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aud: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub azp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(rename = "tf", skip_serializing_if = "Option::is_none")]
    pub token_format: Option<u8>,
    #[serde(rename = "am", skip_serializing_if = "Option::is_none")]
    pub auth_method: Option<String>,
    #[serde(rename = "pid", skip_serializing_if = "Option::is_none")]
    pub project_id: Option<i32>,
    #[serde(rename = "ab", skip_serializing_if = "Option::is_none")]
    pub auth_binding: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    Password,
    OAuth,
}

impl AuthMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthMethod::Password => "password",
            AuthMethod::OAuth => "oauth",
        }
    }

    fn from_str(value: &str) -> Result<Self, ApiError> {
        match value {
            "password" => Ok(AuthMethod::Password),
            "oauth" => Ok(AuthMethod::OAuth),
            _ => Err(ApiError::InvalidJwt),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthContext {
    pub token_format: u8,
    pub method: AuthMethod,
    pub project_id: i32,
    pub auth_binding: [u8; 32],
}

impl AuthContext {
    pub fn new(method: AuthMethod, project_id: i32, auth_binding: [u8; 32]) -> Self {
        Self {
            token_format: USER_TOKEN_FORMAT_V2,
            method,
            project_id,
            auth_binding,
        }
    }

    pub fn apply_to_claims(&self, claims: &mut CustomClaims) {
        claims.token_format = Some(self.token_format);
        claims.auth_method = Some(self.method.as_str().to_string());
        claims.project_id = Some(self.project_id);
        claims.auth_binding = Some(URL_SAFE_NO_PAD.encode(self.auth_binding));
    }

    pub fn from_claims(claims: &CustomClaims) -> Result<Self, ApiError> {
        if claims.token_format != Some(USER_TOKEN_FORMAT_V2) {
            tracing::error!("Missing or invalid user token format");
            return Err(ApiError::InvalidJwt);
        }

        let method = claims
            .auth_method
            .as_deref()
            .ok_or(ApiError::InvalidJwt)
            .and_then(AuthMethod::from_str)?;

        let project_id = claims.project_id.ok_or(ApiError::InvalidJwt)?;

        let auth_binding_claim = claims.auth_binding.as_ref().ok_or(ApiError::InvalidJwt)?;
        let auth_binding_bytes = URL_SAFE_NO_PAD
            .decode(auth_binding_claim)
            .map_err(|_| ApiError::InvalidJwt)?;
        let auth_binding: [u8; 32] = auth_binding_bytes
            .try_into()
            .map_err(|_| ApiError::InvalidJwt)?;

        Ok(Self {
            token_format: USER_TOKEN_FORMAT_V2,
            method,
            project_id,
            auth_binding,
        })
    }
}

impl TokenType {
    pub fn validate_third_party_audience(aud: &str) -> Result<(), ApiError> {
        // Validate third party audience can't use our internal audience types
        const RESERVED_AUDIENCES: [&str; 9] = [
            USER_ACCESS,
            USER_REFRESH,
            PLATFORM_ACCESS,
            PLATFORM_REFRESH,
            TRANSPORT_V2_USER_ACCESS,
            TRANSPORT_V2_USER_REFRESH,
            TRANSPORT_V2_PLATFORM_ACCESS,
            TRANSPORT_V2_PLATFORM_REFRESH,
            TRANSPORT_V2_NATIVE_HANDOFF,
        ];

        // 1. Check for reserved audiences
        if RESERVED_AUDIENCES.contains(&aud) {
            tracing::error!(
                "Third-party tokens cannot use internal audience types: {}",
                aud
            );
            return Err(ApiError::BadRequest);
        }

        // 2. Check length limit (max 50 characters)
        const MAX_AUDIENCE_LENGTH: usize = 50;
        if aud.len() > MAX_AUDIENCE_LENGTH {
            tracing::error!(
                "Audience value exceeds maximum length of {}: {} (length: {})",
                MAX_AUDIENCE_LENGTH,
                aud,
                aud.len()
            );
            return Err(ApiError::BadRequest);
        }

        // 3. Check for null bytes which can cause issues in some systems
        if aud.contains('\0') {
            tracing::error!("Audience contains null bytes which is not allowed");
            return Err(ApiError::BadRequest);
        }

        // 4. Check for character set restrictions - only allow alphanumeric, dots, dashes, colons, and slashes
        // This helps prevent injection attacks while still allowing typical URL characters
        if !aud.chars().all(|c| {
            c.is_alphanumeric()
                || c == '.'
                || c == '-'
                || c == ':'
                || c == '/'
                || c == '_'
                || c == '~'
                || c == '?'
                || c == '&'
                || c == '='
                || c == '+'
                || c == '%'
                || c == '#'
        }) {
            tracing::error!("Audience contains disallowed characters: {}", aud);
            return Err(ApiError::BadRequest);
        }

        // 5. Reject empty audience values
        if aud.is_empty() {
            tracing::error!("Audience value cannot be empty");
            return Err(ApiError::BadRequest);
        }

        // 6. Parse as URI to ensure it's valid if it contains ':'
        if aud.contains(':') {
            Url::parse(aud).map_err(|e| {
                tracing::error!("Invalid audience URI format: {}, error: {:?}", aud, e);
                ApiError::BadRequest
            })?;
        }

        Ok(())
    }
}

impl NewToken {
    /// Attempts to generate a token for third-party authentication using a project-specific JWT key.
    /// Falls back to the default JWT key if no project-specific key exists.
    fn get_third_party_token(
        azp: &str,
        app_state: &AppState,
        header: &Header,
        claims: &Claims<CustomClaims>,
    ) -> Result<String, ApiError> {
        use crate::web::platform::common::THIRD_PARTY_JWT_SECRET;

        // Parse the "azp" value which should be the project client_id
        let project_client_id = Uuid::parse_str(azp).map_err(|e| {
            tracing::error!(
                "Invalid project client_id format in azp: {}, error: {:?}",
                azp,
                e
            );
            ApiError::BadRequest
        })?;

        // Look up the project by client_id (not UUID)
        let project = app_state
            .db
            .get_org_project_by_client_id(project_client_id)
            .map_err(|e| {
                tracing::error!(
                    "Error looking up project with client_id {}: {:?}",
                    project_client_id,
                    e
                );
                match e {
                    DBError::OrgProjectNotFound => ApiError::BadRequest,
                    _ => ApiError::InternalServerError,
                }
            })?;

        // Look up a custom JWT secret for this project
        match app_state
            .db
            .get_org_project_secret_by_key_name_and_project(THIRD_PARTY_JWT_SECRET, project.id)
        {
            Ok(Some(secret)) => {
                // Decrypt the custom JWT secret using the enclave key
                let secret_key =
                    secp256k1::SecretKey::from_slice(&app_state.enclave_key).map_err(|e| {
                        tracing::error!("Failed to create secret key from enclave key: {:?}", e);
                        ApiError::InternalServerError
                    })?;

                let decrypted_key =
                    crate::encrypt::decrypt_with_key(&secret_key, &secret.secret_enc).map_err(
                        |e| {
                            tracing::error!(
                                "Failed to decrypt custom JWT secret for project {}: {:?}",
                                project_client_id,
                                e
                            );
                            ApiError::InternalServerError
                        },
                    )?;

                // For custom secrets, use HS256 algorithm (HMAC with shared secret)
                // This is what third-party services like Supabase expect
                tracing::debug!(
                    "Using custom JWT secret with HS256 for project {}",
                    project_client_id
                );

                // Create HS256 header
                let jwt_header = JwtHeader::new(JwtAlgorithm::HS256);

                // Create encoding key from the decrypted secret
                let encoding_key = EncodingKey::from_secret(&decrypted_key);

                // Encode the token using HS256
                jwt_encode(&jwt_header, claims, &encoding_key).map_err(|e| {
                    tracing::error!("Error creating HS256 token with custom secret: {:?}", e);
                    ApiError::InternalServerError
                })
            }
            Ok(None) => {
                // No custom secret found, use the default key
                tracing::debug!(
                    "No custom JWT secret found for project {}, using default",
                    project_client_id
                );
                let es256k = Es256k::<Sha256>::new(app_state.config.jwt_keys.secp.clone());

                es256k
                    .token(header, claims, &app_state.config.jwt_keys.signing_key)
                    .map_err(|e| {
                        tracing::error!("Error creating token: {:?}", e);
                        ApiError::InternalServerError
                    })
            }
            Err(e) => {
                // Database error looking up the secret
                tracing::error!(
                    "Database error looking up custom JWT secret for project {}: {:?}",
                    project_client_id,
                    e
                );
                Err(ApiError::InternalServerError)
            }
        }
    }

    pub fn new(user: &User, token_type: TokenType, app_state: &AppState) -> Result<Self, ApiError> {
        let (aud, azp, role, duration) = match &token_type {
            TokenType::ThirdParty { aud, azp } => {
                // Validate the audience URL against allowed domains
                if aud.is_some() {
                    TokenType::validate_third_party_audience(aud.as_ref().expect("just checked"))?;
                }

                (
                    aud.clone(),
                    Some(azp.clone()),
                    Some("authenticated".to_string()),
                    Duration::hours(1),
                )
            }
            TokenType::Access
            | TokenType::Refresh
            | TokenType::TransportV2Access
            | TokenType::TransportV2Refresh => {
                tracing::error!("User access/refresh tokens require AuthContext");
                return Err(ApiError::BadRequest);
            }
        };

        let custom_claims = CustomClaims {
            sub: user.get_id().to_string(),
            aud,
            azp,
            role,
            token_format: None,
            auth_method: None,
            project_id: None,
            auth_binding: None,
        };

        tracing::debug!("Creating new token with claims: {:?}", custom_claims);

        // Account for clock drift by setting issued_at 1 minute in the past
        let now = Utc::now();
        let iat = now - Duration::minutes(1);
        let exp = iat + duration;

        let mut claims = Claims::new(custom_claims);
        claims.issued_at = Some(iat);
        claims.expiration = Some(exp);
        claims.not_before = Some(iat);

        // Create header with typ field
        let header = Header::empty().with_token_type("JWT");

        // Check if we need to use a custom JWT secret for third-party tokens
        let token_string = if let TokenType::ThirdParty { azp, .. } = &token_type {
            // Try to get the third-party token using project-specific key or fall back to default key
            Self::get_third_party_token(azp, app_state, &header, &claims)?
        } else {
            // For normal user tokens, use the default key
            let es256k = Es256k::<Sha256>::new(app_state.config.jwt_keys.secp.clone());

            es256k
                .token(&header, &claims, &app_state.config.jwt_keys.signing_key)
                .map_err(|e| {
                    tracing::error!("Error creating token: {:?}", e);
                    ApiError::InternalServerError
                })?
        };

        tracing::debug!("Successfully created token");

        Ok(Self {
            token: token_string,
        })
    }

    pub fn new_with_auth_context(
        user: &User,
        token_type: TokenType,
        app_state: &AppState,
        auth_context: &AuthContext,
    ) -> Result<Self, ApiError> {
        if user.project_id != auth_context.project_id {
            tracing::error!("User token auth context project does not match user project");
            return Err(ApiError::BadRequest);
        }

        let (aud, duration) = match &token_type {
            TokenType::Access => (
                Some(USER_ACCESS.to_string()),
                Duration::minutes(app_state.config.access_token_maxage),
            ),
            TokenType::Refresh => (
                Some(USER_REFRESH.to_string()),
                Duration::days(app_state.config.refresh_token_maxage),
            ),
            TokenType::TransportV2Access => (
                Some(TRANSPORT_V2_USER_ACCESS.to_string()),
                Duration::minutes(app_state.config.access_token_maxage),
            ),
            TokenType::TransportV2Refresh => (
                Some(TRANSPORT_V2_USER_REFRESH.to_string()),
                Duration::days(app_state.config.refresh_token_maxage),
            ),
            TokenType::ThirdParty { .. } => {
                return Err(ApiError::BadRequest);
            }
        };

        let mut custom_claims = CustomClaims {
            sub: user.get_id().to_string(),
            aud,
            azp: None,
            role: None,
            token_format: None,
            auth_method: None,
            project_id: None,
            auth_binding: None,
        };
        auth_context.apply_to_claims(&mut custom_claims);

        tracing::debug!(
            "Creating new v2 user token for user {} with audience {:?}",
            user.get_id(),
            custom_claims.aud
        );

        let now = Utc::now();
        let iat = now - Duration::minutes(1);
        let exp = iat + duration;

        let mut claims = Claims::new(custom_claims);
        claims.issued_at = Some(iat);
        claims.expiration = Some(exp);
        claims.not_before = Some(iat);

        let header = Header::empty().with_token_type("JWT");
        let es256k = Es256k::<Sha256>::new(app_state.config.jwt_keys.secp.clone());

        let token_string = es256k
            .token(&header, &claims, &app_state.config.jwt_keys.signing_key)
            .map_err(|e| {
                tracing::error!("Error creating v2 user token: {:?}", e);
                ApiError::InternalServerError
            })?;

        Ok(Self {
            token: token_string,
        })
    }

    pub fn new_for_platform_user(
        user: &PlatformUser,
        token_type: TokenType,
        app_state: &AppState,
    ) -> Result<Self, ApiError> {
        let (aud, azp, duration) = match token_type {
            TokenType::Access => (
                PLATFORM_ACCESS.to_string(),
                None,
                Duration::minutes(app_state.config.access_token_maxage),
            ),
            TokenType::Refresh => (
                PLATFORM_REFRESH.to_string(),
                None,
                Duration::days(app_state.config.refresh_token_maxage),
            ),
            TokenType::TransportV2Access => (
                TRANSPORT_V2_PLATFORM_ACCESS.to_string(),
                None,
                Duration::minutes(app_state.config.access_token_maxage),
            ),
            TokenType::TransportV2Refresh => (
                TRANSPORT_V2_PLATFORM_REFRESH.to_string(),
                None,
                Duration::days(app_state.config.refresh_token_maxage),
            ),
            TokenType::ThirdParty { .. } => {
                // Platform users cannot create third-party tokens
                return Err(ApiError::BadRequest);
            }
        };

        let custom_claims = CustomClaims {
            sub: user.uuid.to_string(),
            aud: Some(aud),
            azp,
            role: None,
            token_format: None,
            auth_method: None,
            project_id: None,
            auth_binding: None,
        };

        tracing::debug!(
            "Creating new platform token with claims: {:?}",
            custom_claims
        );

        // Account for clock drift by setting issued_at 1 minute in the past
        let now = Utc::now();
        let iat = now - Duration::minutes(1);
        let exp = iat + duration;

        let mut claims = Claims::new(custom_claims);
        claims.issued_at = Some(iat);
        claims.expiration = Some(exp);
        claims.not_before = Some(iat);

        let header = Header::empty().with_token_type("JWT");
        let es256k = Es256k::<Sha256>::new(app_state.config.jwt_keys.secp.clone());

        let token_string = es256k
            .token(&header, &claims, &app_state.config.jwt_keys.signing_key)
            .map_err(|e| {
                tracing::error!("Error creating token: {:?}", e);
                ApiError::InternalServerError
            })?;

        tracing::debug!("Successfully created platform token");

        Ok(Self {
            token: token_string,
        })
    }
}

pub(crate) fn issue_native_handoff_grant(
    user: &User,
    auth_context: &AuthContext,
    target_session_id: SessionId,
    request_id: RequestId,
    app_state: &AppState,
) -> Result<IssuedNativeHandoffGrant, ApiError> {
    if user.project_id != auth_context.project_id {
        return Err(ApiError::BadRequest);
    }
    issue_native_handoff_grant_with_keys(
        user.get_id(),
        auth_context,
        target_session_id,
        request_id,
        &app_state.config.jwt_keys,
        Utc::now(),
    )
}

fn issue_native_handoff_grant_with_keys(
    user_id: Uuid,
    auth_context: &AuthContext,
    target_session_id: SessionId,
    request_id: RequestId,
    jwt_keys: &JwtKeys,
    issued_at: DateTime<Utc>,
) -> Result<IssuedNativeHandoffGrant, ApiError> {
    let expiration = issued_at + TRANSPORT_V2_NATIVE_HANDOFF_TTL;
    let custom_claims = NativeHandoffClaims {
        sub: user_id.to_string(),
        iss: TRANSPORT_V2_ISSUER.to_owned(),
        aud: TRANSPORT_V2_NATIVE_HANDOFF.to_owned(),
        version: TRANSPORT_V2_VERSION,
        kind: "native_handoff".to_owned(),
        token_format: auth_context.token_format,
        auth_method: auth_context.method.as_str().to_owned(),
        project_id: auth_context.project_id,
        auth_binding: URL_SAFE_NO_PAD.encode(auth_context.auth_binding),
        target_session_id: target_session_id.to_string(),
        request_id: request_id.to_string(),
    };
    let mut claims = Claims::new(custom_claims);
    claims.issued_at = Some(issued_at);
    claims.not_before = Some(issued_at);
    claims.expiration = Some(expiration);

    let grant = Es256k::<Sha256>::new(jwt_keys.secp.clone())
        .token(
            &Header::empty().with_token_type("JWT"),
            &claims,
            &jwt_keys.signing_key,
        )
        .map_err(|error| {
            tracing::error!(?error, "failed to create transport-v2 native handoff grant");
            ApiError::InternalServerError
        })?;
    if !is_canonical_compact_jwt(&grant) {
        tracing::error!("issued transport-v2 native handoff grant exceeded its wire contract");
        return Err(ApiError::InternalServerError);
    }

    Ok(IssuedNativeHandoffGrant {
        grant,
        expires_at: expiration,
    })
}

pub(crate) fn validate_native_handoff_grant(
    grant: &str,
    expected_session_id: SessionId,
    expected_request_id: RequestId,
    app_state: &AppState,
) -> Result<(Uuid, AuthContext), ApiError> {
    validate_native_handoff_grant_with_keys(
        grant,
        expected_session_id,
        expected_request_id,
        &app_state.config.jwt_keys,
        Utc::now(),
    )
}

fn validate_native_handoff_grant_with_keys(
    grant: &str,
    expected_session_id: SessionId,
    expected_request_id: RequestId,
    jwt_keys: &JwtKeys,
    now: DateTime<Utc>,
) -> Result<(Uuid, AuthContext), ApiError> {
    if !is_canonical_compact_jwt(grant) {
        return Err(ApiError::InvalidJwt);
    }
    let untrusted = UntrustedToken::new(grant).map_err(|_| ApiError::InvalidJwt)?;
    let public_key = jwt_keys.public_key();
    let token: Token<NativeHandoffClaims> = Es256k::<Sha256>::new(jwt_keys.secp.clone())
        .validator(&public_key)
        .validate(&untrusted)
        .map_err(|_| ApiError::InvalidJwt)?;
    let claims = token.claims();
    if claims.custom.iss != TRANSPORT_V2_ISSUER
        || claims.custom.aud != TRANSPORT_V2_NATIVE_HANDOFF
        || claims.custom.version != TRANSPORT_V2_VERSION
        || claims.custom.kind != "native_handoff"
        || claims.custom.target_session_id != expected_session_id.to_string()
        || claims.custom.request_id != expected_request_id.to_string()
    {
        return Err(ApiError::InvalidJwt);
    }
    validate_native_handoff_times(claims, now)?;

    let user_id = Uuid::parse_str(&claims.custom.sub).map_err(|_| ApiError::InvalidJwt)?;
    if user_id.is_nil() || user_id.to_string() != claims.custom.sub {
        return Err(ApiError::InvalidJwt);
    }
    let auth_context = AuthContext::from_claims(&CustomClaims {
        sub: claims.custom.sub.clone(),
        aud: Some(claims.custom.aud.clone()),
        azp: None,
        role: None,
        token_format: Some(claims.custom.token_format),
        auth_method: Some(claims.custom.auth_method.clone()),
        project_id: Some(claims.custom.project_id),
        auth_binding: Some(claims.custom.auth_binding.clone()),
    })?;
    Ok((user_id, auth_context))
}

fn validate_native_handoff_times<T>(
    claims: &Claims<T>,
    now: DateTime<Utc>,
) -> Result<(), ApiError> {
    let time_options = TimeOptions::new(TRANSPORT_V2_NATIVE_HANDOFF_CLOCK_LEEWAY, move || now);
    claims
        .validate_expiration(&time_options)
        .and_then(|claims| claims.validate_maturity(&time_options))
        .map_err(|_| ApiError::InvalidJwt)?;

    let issued_at = claims.issued_at.ok_or(ApiError::InvalidJwt)?;
    let not_before = claims.not_before.ok_or(ApiError::InvalidJwt)?;
    let expiration = claims.expiration.ok_or(ApiError::InvalidJwt)?;
    let latest_issuance = now + TRANSPORT_V2_NATIVE_HANDOFF_CLOCK_LEEWAY;
    if issued_at != not_before
        || issued_at > latest_issuance
        || expiration <= issued_at
        || expiration - issued_at > TRANSPORT_V2_NATIVE_HANDOFF_TTL
    {
        return Err(ApiError::InvalidJwt);
    }
    Ok(())
}

fn is_canonical_compact_jwt(value: &str) -> bool {
    if value.is_empty() || value.len() > TRANSPORT_V2_NATIVE_HANDOFF_MAX_BYTES {
        return false;
    }
    let mut segments = value.split('.');
    let canonical_segment = |segment: &str| {
        if segment.is_empty()
            || !segment
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return false;
        }
        URL_SAFE_NO_PAD
            .decode(segment)
            .is_ok_and(|decoded| URL_SAFE_NO_PAD.encode(decoded) == segment)
    };
    matches!(
        (segments.next(), segments.next(), segments.next(), segments.next()),
        (Some(header), Some(payload), Some(signature), None)
            if canonical_segment(header)
                && canonical_segment(payload)
                && canonical_segment(signature)
    )
}

pub async fn generate_jwt_secret(
    aws_credential_manager: Arc<tokio::sync::RwLock<Option<AwsCredentialManager>>>,
) -> Result<Vec<u8>, Error> {
    tracing::info!("Generating new JWT secret");
    if let Some(cred_manager) = aws_credential_manager.read().await.as_ref().cloned() {
        let aws_creds = cred_manager
            .get_credentials()
            .await
            .expect("should have creds");

        generate_random_bytes_from_enclave(
            &aws_creds.region,
            &aws_creds.access_key_id,
            &aws_creds.secret_access_key,
            &aws_creds.token,
            32,
        )
        .await
        .map_err(|e| Error::EncryptionError(e.to_string()))
    } else {
        Ok(crate::encrypt::generate_random::<32>().to_vec())
    }
}

pub async fn validate_jwt(
    State(data): State<Arc<AppState>>,
    mut req: Request<Body>,
    next: Next,
) -> impl IntoResponse {
    let is_transport_v2 = req
        .extensions()
        .get::<TransportSession>()
        .is_some_and(TransportSession::is_v2);
    let token = if is_transport_v2 {
        match req.extensions().get::<Credential>() {
            Some(credential) if credential.kind() == CredentialKind::Bearer => credential.value(),
            _ => return ApiError::InvalidJwt.into_response(),
        }
    } else {
        match req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|auth_header| auth_header.to_str().ok())
            .and_then(|auth_value| auth_value.strip_prefix("Bearer "))
        {
            Some(token) => token,
            None => return ApiError::InvalidJwt.into_response(),
        }
    };

    let expected_audience = if is_transport_v2 {
        TRANSPORT_V2_USER_ACCESS
    } else {
        USER_ACCESS
    };
    let (claims, access_token_expired) =
        match validate_access_token_for_auth(token, &data, expected_audience) {
            Ok(validation) => validation,
            Err(_) => return ApiError::InvalidJwt.into_response(),
        };

    let auth_context = match AuthContext::from_claims(&claims) {
        Ok(auth_context) => auth_context,
        Err(_) => return ApiError::InvalidJwt.into_response(),
    };

    let user_uuid: Uuid = match Uuid::parse_str(&claims.sub) {
        Ok(uuid) => uuid,
        Err(e) => {
            tracing::error!("Error parsing user uuid: {:?}", e);
            return ApiError::InvalidJwt.into_response();
        }
    };

    let user = match data.get_user(user_uuid).await {
        Ok(user) => user,
        Err(e) => {
            tracing::error!("Error getting user: {:?}", e);
            return ApiError::InternalServerError.into_response();
        }
    };

    if user.project_id != auth_context.project_id {
        tracing::error!("JWT auth context project does not match user project");
        return ApiError::InvalidJwt.into_response();
    }

    if let Err(e) = data.verify_seed_wrap_for_auth_context(&user, &auth_context) {
        tracing::error!(
            "JWT auth context no longer unwraps an active seed wrap: {:?}",
            e
        );
        return ApiError::InvalidJwt.into_response();
    }

    if access_token_expired {
        return ApiError::AccessTokenExpired.into_response();
    }

    req.extensions_mut().insert(auth_context);
    req.extensions_mut().insert(user);
    next.run(req).await
}

pub async fn validate_platform_jwt(
    State(data): State<Arc<AppState>>,
    mut req: Request<Body>,
    next: Next,
) -> impl IntoResponse {
    let is_transport_v2 = req
        .extensions()
        .get::<TransportSession>()
        .is_some_and(TransportSession::is_v2);
    let token = if is_transport_v2 {
        match req.extensions().get::<Credential>() {
            Some(credential) if credential.kind() == CredentialKind::Bearer => credential.value(),
            _ => return ApiError::InvalidJwt.into_response(),
        }
    } else {
        match req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|auth_header| auth_header.to_str().ok())
            .and_then(|auth_value| auth_value.strip_prefix("Bearer "))
        {
            Some(token) => token,
            None => return ApiError::InvalidJwt.into_response(),
        }
    };

    let expected_audience = if is_transport_v2 {
        TRANSPORT_V2_PLATFORM_ACCESS
    } else {
        PLATFORM_ACCESS
    };
    let (claims, access_token_expired) =
        match validate_access_token_for_auth(token, &data, expected_audience) {
            Ok(validation) => validation,
            Err(_) => return ApiError::InvalidJwt.into_response(),
        };

    let platform_user_id: Uuid = match Uuid::parse_str(&claims.sub) {
        Ok(uuid) => uuid,
        Err(e) => {
            tracing::error!("Error parsing platform user uuid: {:?}", e);
            return ApiError::InvalidJwt.into_response();
        }
    };

    let platform_user = match data.db.get_platform_user_by_uuid(platform_user_id) {
        Ok(user) => user,
        Err(e) => {
            tracing::error!("Error getting platform user: {:?}", e);
            return ApiError::Unauthorized.into_response();
        }
    };

    if access_token_expired {
        return ApiError::AccessTokenExpired.into_response();
    }

    req.extensions_mut().insert(platform_user);
    next.run(req).await
}

pub(crate) fn validate_token(
    original_token: &str,
    data: &AppState,
    expected_audience: &str,
) -> Result<CustomClaims, ApiError> {
    validate_token_with_keys(original_token, &data.config.jwt_keys, expected_audience)
}

pub(crate) fn validate_access_token_for_auth(
    original_token: &str,
    data: &AppState,
    expected_audience: &str,
) -> Result<(CustomClaims, bool), ApiError> {
    if !is_access_token_audience(expected_audience) {
        return Err(ApiError::InvalidJwt);
    }
    validate_token_with_keys_for_auth(original_token, &data.config.jwt_keys, expected_audience)
}

fn validate_token_with_keys(
    original_token: &str,
    jwt_keys: &JwtKeys,
    expected_audience: &str,
) -> Result<CustomClaims, ApiError> {
    let (claims, access_token_expired) =
        validate_token_with_keys_for_auth(original_token, jwt_keys, expected_audience)?;
    if access_token_expired {
        Err(ApiError::AccessTokenExpired)
    } else {
        Ok(claims)
    }
}

fn is_access_token_audience(audience: &str) -> bool {
    matches!(
        audience,
        USER_ACCESS | PLATFORM_ACCESS | TRANSPORT_V2_USER_ACCESS | TRANSPORT_V2_PLATFORM_ACCESS
    )
}

fn validate_token_with_keys_for_auth(
    original_token: &str,
    jwt_keys: &JwtKeys,
    expected_audience: &str,
) -> Result<(CustomClaims, bool), ApiError> {
    // Try ES256K first
    let es256k = Es256k::<Sha256>::new(jwt_keys.secp.clone());
    let public_key = jwt_keys.public_key();

    // First parse the token with the correct type
    let parsed_token = match UntrustedToken::new(original_token) {
        Ok(token) => token,
        Err(e) => {
            tracing::error!("Failed to parse token: {:?}", e);
            return Err(ApiError::InvalidJwt);
        }
    };

    // Deserialize claims first
    let (token, access_token_expired): (Token<CustomClaims>, bool) =
        match es256k.validator(&public_key).validate(&parsed_token) {
            Ok(token) => {
                // Validate the audience before classifying expiration. This ensures
                // an expired token for another audience is never a refresh signal.
                let claims: &Claims<CustomClaims> = token.claims();
                if let Some(audience) = &claims.custom.aud {
                    if audience != expected_audience {
                        tracing::error!(
                            "Invalid audience: got {}, expected {}",
                            audience,
                            expected_audience
                        );
                        return Err(ApiError::InvalidJwt);
                    }
                } else {
                    tracing::error!("Missing audience in token, expected {}", expected_audience);
                    return Err(ApiError::InvalidJwt);
                }

                // Only validate expiration, not maturity. Access-token expiry is
                // carried to the authentication middleware so identity and auth
                // binding checks still run before a refresh signal is emitted.
                let time_options = TimeOptions::default();
                let access_token_expired = match token.claims().validate_expiration(&time_options) {
                    Ok(_) => false,
                    Err(jwt_compact::ValidationError::Expired)
                        if is_access_token_audience(expected_audience) =>
                    {
                        true
                    }
                    Err(e) => {
                        tracing::error!("Token expiration validation failed: {:?}", e);
                        return Err(ApiError::InvalidJwt);
                    }
                };

                (token, access_token_expired)
            }
            Err(e) => {
                tracing::debug!("ES256K validation failed: {:?}", e);
                return Err(ApiError::InvalidJwt);
            }
        };

    Ok((token.claims().custom.clone(), access_token_expired))
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{decode as jwt_decode, DecodingKey, Validation};

    fn test_keys(byte: u8) -> JwtKeys {
        JwtKeys::new(vec![byte; 32]).unwrap()
    }

    fn signed_test_token(
        keys: &JwtKeys,
        audience: &str,
        expiration: Option<chrono::DateTime<Utc>>,
    ) -> String {
        signed_test_token_with_auth_binding(
            keys,
            audience,
            expiration,
            Some(URL_SAFE_NO_PAD.encode([7u8; 32])),
        )
    }

    fn signed_test_token_with_auth_binding(
        keys: &JwtKeys,
        audience: &str,
        expiration: Option<chrono::DateTime<Utc>>,
        auth_binding: Option<String>,
    ) -> String {
        let now = Utc::now();
        let mut claims = Claims::new(CustomClaims {
            sub: Uuid::nil().to_string(),
            aud: Some(audience.to_string()),
            azp: None,
            role: None,
            token_format: Some(USER_TOKEN_FORMAT_V2),
            auth_method: Some(AuthMethod::Password.as_str().to_string()),
            project_id: Some(1),
            auth_binding,
        });
        claims.issued_at = Some(now - Duration::minutes(2));
        claims.not_before = Some(now - Duration::minutes(2));
        claims.expiration = expiration;

        Es256k::<Sha256>::new(keys.secp.clone())
            .token(
                &Header::empty().with_token_type("JWT"),
                &claims,
                &keys.signing_key,
            )
            .unwrap()
    }

    #[test]
    fn expired_user_access_token_is_recoverable_only_after_signature_and_audience_validation() {
        let trusted_keys = test_keys(1);
        let other_keys = test_keys(2);
        let expired = Utc::now() - Duration::minutes(1);

        let expired_access = signed_test_token(&trusted_keys, USER_ACCESS, Some(expired));
        assert!(matches!(
            validate_token_with_keys(&expired_access, &trusted_keys, USER_ACCESS),
            Err(ApiError::AccessTokenExpired)
        ));

        let wrong_audience = signed_test_token(&trusted_keys, USER_REFRESH, Some(expired));
        assert!(matches!(
            validate_token_with_keys(&wrong_audience, &trusted_keys, USER_ACCESS),
            Err(ApiError::InvalidJwt)
        ));

        let wrong_signature = signed_test_token(&other_keys, USER_ACCESS, Some(expired));
        assert!(matches!(
            validate_token_with_keys(&wrong_signature, &trusted_keys, USER_ACCESS),
            Err(ApiError::InvalidJwt)
        ));
    }

    #[test]
    fn expired_access_tokens_are_recoverable_but_refresh_tokens_are_not() {
        let keys = test_keys(3);
        let expired = Utc::now() - Duration::minutes(1);

        let refresh = signed_test_token(&keys, USER_REFRESH, Some(expired));
        assert!(matches!(
            validate_token_with_keys(&refresh, &keys, USER_REFRESH),
            Err(ApiError::InvalidJwt)
        ));

        let platform_access = signed_test_token(&keys, PLATFORM_ACCESS, Some(expired));
        assert!(matches!(
            validate_token_with_keys(&platform_access, &keys, PLATFORM_ACCESS),
            Err(ApiError::AccessTokenExpired)
        ));
    }

    #[test]
    fn token_audiences_are_exactly_separated_across_transport_and_principal_kinds() {
        let keys = test_keys(6);
        let audiences = [
            USER_ACCESS,
            USER_REFRESH,
            PLATFORM_ACCESS,
            PLATFORM_REFRESH,
            TRANSPORT_V2_USER_ACCESS,
            TRANSPORT_V2_USER_REFRESH,
            TRANSPORT_V2_PLATFORM_ACCESS,
            TRANSPORT_V2_PLATFORM_REFRESH,
        ];

        for issued_audience in audiences {
            let token = signed_test_token(
                &keys,
                issued_audience,
                Some(Utc::now() + Duration::minutes(5)),
            );
            for expected_audience in audiences {
                let result = validate_token_with_keys(&token, &keys, expected_audience);
                assert_eq!(
                    result.is_ok(),
                    issued_audience == expected_audience,
                    "token for {issued_audience} was validated as {expected_audience}"
                );
            }
        }

        for transport_v2_audience in [
            TRANSPORT_V2_USER_ACCESS,
            TRANSPORT_V2_USER_REFRESH,
            TRANSPORT_V2_PLATFORM_ACCESS,
            TRANSPORT_V2_PLATFORM_REFRESH,
        ] {
            assert!(
                transport_v2_audience.len() > 50,
                "internal V2 audiences must remain outside V1's existing third-party audience space"
            );
        }
    }

    #[test]
    fn native_handoff_grant_is_exactly_session_request_and_key_bound() {
        let trusted_keys = test_keys(7);
        let other_keys = test_keys(8);
        let user_id = Uuid::from_u128(7);
        let auth_context = AuthContext::new(AuthMethod::Password, 42, [0x33; 32]);
        let session_id = SessionId::from_bytes([0x44; 16]);
        let request_id = RequestId::from_bytes([0x55; 16]);
        let now = Utc::now();
        let issued = issue_native_handoff_grant_with_keys(
            user_id,
            &auth_context,
            session_id,
            request_id,
            &trusted_keys,
            now,
        )
        .unwrap();

        assert_eq!(
            validate_native_handoff_grant_with_keys(
                &issued.grant,
                session_id,
                request_id,
                &trusted_keys,
                now,
            )
            .unwrap(),
            (user_id, auth_context)
        );
        assert!(validate_native_handoff_grant_with_keys(
            &issued.grant,
            SessionId::from_bytes([0x45; 16]),
            request_id,
            &trusted_keys,
            now,
        )
        .is_err());
        assert!(validate_native_handoff_grant_with_keys(
            &issued.grant,
            session_id,
            RequestId::from_bytes([0x56; 16]),
            &trusted_keys,
            now,
        )
        .is_err());
        assert!(validate_native_handoff_grant_with_keys(
            &issued.grant,
            session_id,
            request_id,
            &other_keys,
            now,
        )
        .is_err());
        assert!(TokenType::validate_third_party_audience(TRANSPORT_V2_NATIVE_HANDOFF).is_err());
        assert!(
            TRANSPORT_V2_NATIVE_HANDOFF.len() > 50,
            "the handoff audience must remain outside V1's existing third-party audience space"
        );
    }

    #[test]
    fn native_handoff_grant_rejects_expired_and_noncanonical_wire_values() {
        let keys = test_keys(9);
        let auth_context = AuthContext::new(AuthMethod::OAuth, 3, [0x66; 32]);
        let session_id = SessionId::from_bytes([0x77; 16]);
        let request_id = RequestId::from_bytes([0x88; 16]);
        let now = Utc::now();
        let expired = issue_native_handoff_grant_with_keys(
            Uuid::from_u128(9),
            &auth_context,
            session_id,
            request_id,
            &keys,
            now - Duration::minutes(10),
        )
        .unwrap();
        assert!(validate_native_handoff_grant_with_keys(
            &expired.grant,
            session_id,
            request_id,
            &keys,
            now,
        )
        .is_err());

        let mut padded = expired.grant;
        padded.push('=');
        assert!(!is_canonical_compact_jwt(&padded));
        assert!(!is_canonical_compact_jwt(
            &"x".repeat(TRANSPORT_V2_NATIVE_HANDOFF_MAX_BYTES + 1)
        ));
    }

    #[test]
    fn expired_access_claims_are_available_for_auth_binding_validation() {
        let keys = test_keys(5);
        let malformed_binding = URL_SAFE_NO_PAD.encode([7u8; 31]);
        let token = signed_test_token_with_auth_binding(
            &keys,
            USER_ACCESS,
            Some(Utc::now() - Duration::minutes(1)),
            Some(malformed_binding),
        );

        let (claims, access_token_expired) =
            validate_token_with_keys_for_auth(&token, &keys, USER_ACCESS).unwrap();
        assert!(access_token_expired);
        assert!(matches!(
            AuthContext::from_claims(&claims),
            Err(ApiError::InvalidJwt)
        ));
    }

    #[test]
    fn valid_access_and_malformed_tokens_do_not_report_expiration() {
        let keys = test_keys(4);
        let valid = signed_test_token(&keys, USER_ACCESS, Some(Utc::now() + Duration::minutes(5)));
        let missing_expiration = signed_test_token(&keys, USER_ACCESS, None);

        assert!(validate_token_with_keys(&valid, &keys, USER_ACCESS).is_ok());
        assert!(matches!(
            validate_token_with_keys(&missing_expiration, &keys, USER_ACCESS),
            Err(ApiError::InvalidJwt)
        ));
        assert!(matches!(
            validate_token_with_keys("not-a-jwt", &keys, USER_ACCESS),
            Err(ApiError::InvalidJwt)
        ));
    }

    #[test]
    fn test_jsonwebtoken_hs256_round_trip() {
        let now = Utc::now();
        let claims = {
            let mut claims = Claims::new(CustomClaims {
                sub: "user-id".to_string(),
                aud: Some("https://example.com".to_string()),
                azp: Some(Uuid::nil().to_string()),
                role: Some("authenticated".to_string()),
                token_format: None,
                auth_method: None,
                project_id: None,
                auth_binding: None,
            });
            claims.issued_at = Some(now);
            claims.not_before = Some(now);
            claims.expiration = Some(now + Duration::minutes(5));
            claims
        };

        let token = jwt_encode(
            &JwtHeader::new(JwtAlgorithm::HS256),
            &claims,
            &EncodingKey::from_secret(b"super-secret"),
        )
        .expect("token should encode");

        let mut validation = Validation::new(JwtAlgorithm::HS256);
        validation.set_audience(&["https://example.com"]);

        let decoded = jwt_decode::<Claims<CustomClaims>>(
            &token,
            &DecodingKey::from_secret(b"super-secret"),
            &validation,
        )
        .expect("token should decode");

        assert_eq!(decoded.claims.custom, claims.custom);
    }

    #[test]
    fn auth_context_round_trips_through_custom_claims() {
        let auth_context = AuthContext::new(AuthMethod::Password, 7, [3u8; 32]);
        let mut claims = CustomClaims {
            sub: Uuid::nil().to_string(),
            aud: Some(USER_ACCESS.to_string()),
            azp: None,
            role: None,
            token_format: None,
            auth_method: None,
            project_id: None,
            auth_binding: None,
        };

        auth_context.apply_to_claims(&mut claims);
        let parsed = AuthContext::from_claims(&claims).unwrap();

        assert_eq!(auth_context, parsed);
        let expected_binding = URL_SAFE_NO_PAD.encode([3u8; 32]);
        assert_eq!(
            claims.auth_binding.as_deref(),
            Some(expected_binding.as_str())
        );
    }

    #[test]
    fn auth_context_rejects_legacy_user_access_and_refresh_claim_shapes() {
        for audience in [USER_ACCESS, USER_REFRESH] {
            let claims = CustomClaims {
                sub: Uuid::nil().to_string(),
                aud: Some(audience.to_string()),
                azp: None,
                role: None,
                token_format: None,
                auth_method: None,
                project_id: None,
                auth_binding: None,
            };

            assert!(
                AuthContext::from_claims(&claims).is_err(),
                "legacy {audience} claims without auth context must be rejected"
            );
        }
    }

    #[test]
    fn auth_context_rejects_bad_method() {
        let mut claims = CustomClaims {
            sub: Uuid::nil().to_string(),
            aud: Some(USER_ACCESS.to_string()),
            azp: None,
            role: None,
            token_format: None,
            auth_method: None,
            project_id: None,
            auth_binding: None,
        };
        AuthContext::new(AuthMethod::OAuth, 7, [4u8; 32]).apply_to_claims(&mut claims);
        claims.auth_method = Some("api_key".to_string());

        assert!(AuthContext::from_claims(&claims).is_err());
    }

    #[test]
    fn auth_context_rejects_wrong_binding_length() {
        let mut claims = CustomClaims {
            sub: Uuid::nil().to_string(),
            aud: Some(USER_ACCESS.to_string()),
            azp: None,
            role: None,
            token_format: None,
            auth_method: None,
            project_id: None,
            auth_binding: None,
        };
        AuthContext::new(AuthMethod::Password, 7, [5u8; 32]).apply_to_claims(&mut claims);
        claims.auth_binding = Some(URL_SAFE_NO_PAD.encode([5u8; 31]));

        assert!(AuthContext::from_claims(&claims).is_err());
    }
}
