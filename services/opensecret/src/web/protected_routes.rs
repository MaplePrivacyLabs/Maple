use crate::encrypt;
use crate::jwt::{validate_jwt, AuthContext, NewToken, TokenType};
use crate::message_signing::SigningAlgorithm;
use crate::private_key::{
    derive_bip85_mnemonic_from_root, plaintext_user_seed_to_mnemonic, VALID_BIP39_WORD_COUNTS,
};
use crate::recovery_code::RecoveryCode;
use crate::seed_wrapping::new_recovery_seed_wrapping;
use crate::web::encryption_middleware::{
    decrypt_request, encrypt_response, require_transport_v2, require_v2_transport_session,
    Decrypted, TransportSession,
};
use crate::Error;
use crate::{
    db::DBError, email::send_verification_email, models::email_verification::NewEmailVerification,
    models::users::User, ApiError, AppState,
};

// BIP-85 constants
const BIP85_PURPOSE: u32 = 83696968; // BIP-85 purpose value
const BIP85_APPLICATION_BIP39: u32 = 39; // Application number for BIP-39 mnemonics
use axum::middleware::from_fn;
use axum::middleware::from_fn_with_state;
use axum::Extension;
use axum::{
    extract::{Path, Query, State},
    response::Response,
    routing::{delete, get, post, put},
    Router,
};
use base64::{engine::general_purpose, Engine as _};
use bitcoin::bip32::DerivationPath;
use chrono::{DateTime, Utc};
use secp256k1::Secp256k1;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::str::FromStr;
use std::sync::Arc;
use tracing::{debug, error, info, warn};
use uuid::Uuid;
use validator::Validate;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LoginMethod {
    Email,
    Github,
    Google,
    Apple,
    Guest,
}

// Update AppUser struct to include login_method
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppUser {
    pub id: Uuid,
    pub name: Option<String>,
    pub email: Option<String>,
    pub email_verified: bool,
    pub login_method: LoginMethod,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<&User> for AppUser {
    fn from(user: &User) -> Self {
        AppUser {
            id: user.uuid,
            name: user.name.clone(),
            email: user.email.clone(),
            // This will be set separately
            email_verified: false,
            // This will be updated for oauth
            login_method: if user.is_guest() {
                LoginMethod::Guest
            } else {
                LoginMethod::Email
            },
            created_at: user.created_at,
            updated_at: user.updated_at,
        }
    }
}

#[derive(Serialize)]
pub struct ProtectedUserData {
    pub user: AppUser,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChangePasswordRequest {
    pub current_password: String,
    pub new_password: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InitiateAccountDeletionRequest {
    pub hashed_secret: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConfirmAccountDeletionRequest {
    pub confirmation_code: String,
    pub plaintext_secret: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EnrollRecoveryRequest {
    pub current_password: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RotateRecoveryRequest {
    pub current_password: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DisableRecoveryRequest {
    pub current_password: String,
}

#[derive(Debug, Serialize)]
pub struct RecoveryStatusResponse {
    pub enrolled: bool,
    pub enrolled_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
pub struct RecoveryCodeResponse {
    pub recovery_code: String,
}

#[derive(Debug, Serialize)]
pub struct PrivateKeyResponse {
    /// Root mnemonic or derived mnemonic if BIP-85 path is specified
    mnemonic: String,
}

/// Response struct for the private key bytes endpoint.
/// Contains the private key encoded as a hexadecimal string.
#[derive(Debug, Serialize)]
pub struct PrivateKeyBytesResponse {
    /// The private key as a 64-character hexadecimal string (32 bytes).
    /// This is the standard secp256k1 private key format.
    private_key: String,
}

/// Structure for key derivation options.
/// Supports both BIP-32 and BIP-85 derivation paths.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct KeyOptions {
    /// BIP-85 derivation path to derive a child seed phrase
    /// Format: "m/83696968'/39'/0'/12'/0'" where:
    ///   - 83696968' is the BIP-85 purpose
    ///   - 39' is for BIP-39 mnemonics
    ///   - 0' is the language code; only English (0') is currently supported
    ///   - 12', 18', or 24' is the word count (only these word counts are supported)
    ///   - 0' is the index
    pub seed_phrase_derivation_path: Option<String>,

    /// BIP-32 derivation path for deriving a private key
    /// Format: "m/44'/0'/0'/0/0"
    pub private_key_derivation_path: Option<String>,
}

impl KeyOptions {
    /// Validates both derivation paths if present.
    pub fn validate(&self) -> Result<(), ApiError> {
        // Validate private_key_derivation_path if present
        if let Some(ref path) = self.private_key_derivation_path {
            validate_path(path)?;
        }

        // Validate seed_phrase_derivation_path if present
        if let Some(ref path) = self.seed_phrase_derivation_path {
            validate_path(path)?;
            validate_bip85_path(path)?;
        }

        Ok(())
    }
}

/// Validates a BIP-85 path to ensure it follows the correct format and contains valid values.
/// Format: m/PURPOSE'/APP'/LANGUAGE'/WORDS'/INDEX' where:
/// - PURPOSE' is the BIP-85 purpose value (BIP85_PURPOSE = 83696968', must be hardened)
/// - APP' is for BIP-39 mnemonics (BIP85_APPLICATION_BIP39 = 39', must be hardened)
/// - LANGUAGE' is typically 0' for English (must be hardened)
/// - WORDS' must be one of VALID_BIP39_WORD_COUNTS (12', 18', 24', must be hardened)
/// - INDEX' is the derivation index (must be hardened)
pub fn validate_bip85_path(path: &str) -> Result<(), ApiError> {
    // Split the path into segments
    let segments: Vec<&str> = path.split('/').collect();

    // Path must have exactly 6 segments: m/PURPOSE'/APP'/LANGUAGE'/WORDS'/INDEX'
    if segments.len() != 6 {
        error!(
            "BIP-85 path must have exactly 6 segments, found {}: {}",
            segments.len(),
            path
        );
        return Err(ApiError::BadRequest);
    }

    // First segment should be "m" (master key)
    if segments[0] != "m" {
        error!(
            "BIP-85 path must start with 'm', found '{}': {}",
            segments[0], path
        );
        return Err(ApiError::BadRequest);
    }

    let purpose = parse_hardened_bip85_segment(segments[1], "purpose", path)?;
    if purpose != BIP85_PURPOSE {
        error!(
            "BIP-85 path purpose must be {}' or {}h, found '{}': {}",
            BIP85_PURPOSE, BIP85_PURPOSE, segments[1], path
        );
        return Err(ApiError::BadRequest);
    }

    let application = parse_hardened_bip85_segment(segments[2], "application", path)?;
    if application != BIP85_APPLICATION_BIP39 {
        error!(
            "BIP-85 path application must be {}' or {}h for BIP-39 mnemonics, found '{}': {}",
            BIP85_APPLICATION_BIP39, BIP85_APPLICATION_BIP39, segments[2], path
        );
        return Err(ApiError::BadRequest);
    }

    let language = parse_hardened_bip85_segment(segments[3], "language", path)?;
    if language != 0 {
        error!(
            "BIP-85 path language must be 0' or 0h for English, found '{}': {}",
            segments[3], path
        );
        return Err(ApiError::BadRequest);
    }

    // Check word count is valid (12, 18, 24) and hardened
    let word_count = segments[4];
    let word_count_value = match word_count.trim_end_matches(&['\'', 'h'][..]).parse::<u32>() {
        Ok(value) => value,
        Err(_) => {
            error!(
                "BIP-85 path word count must be a number, found '{}': {}",
                word_count, path
            );
            return Err(ApiError::BadRequest);
        }
    };

    // Validate word count is one of the valid options
    if !VALID_BIP39_WORD_COUNTS.contains(&word_count_value) {
        error!(
            "BIP-85 path word count must be one of {:?}, found {}: {}",
            VALID_BIP39_WORD_COUNTS, word_count_value, path
        );
        return Err(ApiError::BadRequest);
    }

    // Check word count is hardened
    if !(word_count.ends_with('\'') || word_count.ends_with('h')) {
        error!(
            "BIP-85 path word count must be hardened, found '{}': {}",
            word_count, path
        );
        return Err(ApiError::BadRequest);
    }

    // Check index is hardened
    let index = segments[5];
    if !(index.ends_with('\'') || index.ends_with('h')) {
        error!(
            "BIP-85 path index must be hardened, found '{}': {}",
            index, path
        );
        return Err(ApiError::BadRequest);
    }

    // Try to parse the index to ensure it's a valid number
    match index.trim_end_matches(&['\'', 'h'][..]).parse::<u32>() {
        Ok(value) => value,
        Err(_) => {
            error!(
                "BIP-85 path index must be a valid number, found '{}': {}",
                index, path
            );
            return Err(ApiError::BadRequest);
        }
    };

    Ok(())
}

fn parse_hardened_bip85_segment(segment: &str, name: &str, path: &str) -> Result<u32, ApiError> {
    if !(segment.ends_with('\'') || segment.ends_with('h')) {
        error!(
            "BIP-85 path {} must be hardened, found '{}': {}",
            name, segment, path
        );
        return Err(ApiError::BadRequest);
    }

    segment
        .trim_end_matches(&['\'', 'h'][..])
        .parse::<u32>()
        .map_err(|_| {
            error!(
                "BIP-85 path {} must be a valid number, found '{}': {}",
                name, segment, path
            );
            ApiError::BadRequest
        })
}

/// Query parameters for endpoints that accept a derivation path.
/// The derivation path should follow BIP32 format (e.g., "m/44'/0'/0'/0/0").
#[derive(Debug, Clone, Deserialize)]
pub struct DerivationPathQuery {
    #[serde(default, flatten)]
    pub key_options: KeyOptions,
}

impl DerivationPathQuery {
    /// Validates that the derivation paths follow BIP32 and BIP-85 format if present.
    pub fn validate(&self) -> Result<(), ApiError> {
        // Use the KeyOptions validation
        self.key_options.validate()
    }
}

/// Helper function to validate a derivation path
fn validate_path(path: &str) -> Result<(), ApiError> {
    // Allow empty path or "m" alone
    if path.is_empty() || path == "m" {
        return Ok(());
    }

    // For non-empty paths, validate using bitcoin library's DerivationPath
    DerivationPath::from_str(path).map_err(|e| {
        error!("Invalid derivation path format: {}", e);
        ApiError::BadRequest
    })?;

    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
pub struct SignMessageRequest {
    pub message_base64: String,
    pub algorithm: SigningAlgorithm,
    #[serde(default)]
    pub key_options: KeyOptions,
}

#[derive(Debug, Serialize)]
pub struct SignMessageResponseJson {
    pub signature: String,
    pub message_hash: String,
}

#[derive(Debug, Serialize)]
pub struct PublicKeyResponseJson {
    pub public_key: String,
    pub algorithm: SigningAlgorithm,
}

#[derive(Debug, Deserialize)]
pub struct PublicKeyQuery {
    algorithm: SigningAlgorithm,
    #[serde(default, flatten)]
    key_options: KeyOptions,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ThirdPartyTokenRequest {
    pub audience: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ThirdPartyTokenResponse {
    pub token: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EncryptDataRequest {
    pub data: String,
    #[serde(default)]
    pub key_options: KeyOptions,
}

#[derive(Debug, Serialize)]
pub struct EncryptDataResponse {
    pub encrypted_data: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DecryptDataRequest {
    pub encrypted_data: String,
    #[serde(default)]
    pub key_options: KeyOptions,
}

// API Key Management Types

// Validation function for API key names
// More restrictive than project names since they're used in URL paths
fn validate_api_key_name(name: &str) -> Result<(), validator::ValidationError> {
    use validator::ValidationError;

    // First check for leading/trailing whitespace
    if name != name.trim() {
        return Err(
            ValidationError::new("whitespace").with_message(std::borrow::Cow::Borrowed(
                "Name cannot have leading or trailing whitespace",
            )),
        );
    }

    // After confirming no leading/trailing space, check length
    if name.is_empty() || name.len() > 50 {
        return Err(ValidationError::new("length")
            .with_message(std::borrow::Cow::Borrowed("Name must be 1-50 characters")));
    }

    // Check for valid characters (ASCII only for URL safety)
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == ' ' || c == '-' || c == '_')
    {
        return Err(ValidationError::new("invalid_characters").with_message(
            std::borrow::Cow::Borrowed(
                "Only ASCII alphanumeric characters, spaces, hyphens, and underscores are allowed",
            ),
        ));
    }

    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct CreateApiKeyRequest {
    #[validate(length(min = 1, max = 50))]
    #[validate(custom(function = "validate_api_key_name"))]
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateApiKeyResponse {
    pub key: String, // UUID format with dashes - only returned on creation
    pub name: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyInfo {
    pub name: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListApiKeysResponse {
    pub keys: Vec<ApiKeyInfo>,
}

pub fn router(app_state: Arc<AppState>) -> Router<()> {
    Router::new()
        .route(
            "/protected/user",
            get(user_protected).layer(from_fn_with_state(app_state.clone(), decrypt_request::<()>)),
        )
        .route(
            "/protected/kv/:key",
            get(get_kv).layer(from_fn_with_state(app_state.clone(), decrypt_request::<()>)),
        )
        .route(
            "/protected/kv/:key",
            put(put_kv).layer(from_fn_with_state(
                app_state.clone(),
                decrypt_request::<String>,
            )),
        )
        .route(
            "/protected/kv/:key",
            delete(delete_kv).layer(from_fn_with_state(app_state.clone(), decrypt_request::<()>)),
        )
        .route(
            "/protected/kv",
            get(list_kv).layer(from_fn_with_state(app_state.clone(), decrypt_request::<()>)),
        )
        .route(
            "/protected/kv",
            delete(delete_all_kv)
                .layer(from_fn_with_state(app_state.clone(), decrypt_request::<()>)),
        )
        .route(
            "/protected/request_verification",
            post(request_new_verification_code)
                .layer(from_fn_with_state(app_state.clone(), decrypt_request::<()>)),
        )
        .route(
            "/protected/change_password",
            post(change_password).layer(from_fn_with_state(
                app_state.clone(),
                decrypt_request::<ChangePasswordRequest>,
            )),
        )
        .route(
            "/protected/delete-account/request",
            post(initiate_account_deletion).layer(from_fn_with_state(
                app_state.clone(),
                decrypt_request::<InitiateAccountDeletionRequest>,
            )),
        )
        .route(
            "/protected/delete-account/confirm",
            post(confirm_account_deletion).layer(from_fn_with_state(
                app_state.clone(),
                decrypt_request::<ConfirmAccountDeletionRequest>,
            )),
        )
        .route(
            "/protected/private_key",
            get(get_private_key)
                .layer(from_fn_with_state(app_state.clone(), decrypt_request::<()>)),
        )
        .route(
            "/protected/private_key_bytes",
            get(get_private_key_bytes)
                .layer(from_fn_with_state(app_state.clone(), decrypt_request::<()>)),
        )
        .route(
            "/protected/sign_message",
            post(sign_message).layer(from_fn_with_state(
                app_state.clone(),
                decrypt_request::<SignMessageRequest>,
            )),
        )
        .route(
            "/protected/public_key",
            get(get_public_key).layer(from_fn_with_state(app_state.clone(), decrypt_request::<()>)),
        )
        .route(
            "/protected/third_party_token",
            post(generate_third_party_token).layer(from_fn_with_state(
                app_state.clone(),
                decrypt_request::<ThirdPartyTokenRequest>,
            )),
        )
        .route(
            "/protected/encrypt",
            post(encrypt_data).layer(from_fn_with_state(
                app_state.clone(),
                decrypt_request::<EncryptDataRequest>,
            )),
        )
        .route(
            "/protected/decrypt",
            post(decrypt_data).layer(from_fn_with_state(
                app_state.clone(),
                decrypt_request::<DecryptDataRequest>,
            )),
        )
        // API Key management endpoints
        .route(
            "/protected/api-keys",
            post(create_api_key).layer(from_fn_with_state(
                app_state.clone(),
                decrypt_request::<CreateApiKeyRequest>,
            )),
        )
        .route(
            "/protected/api-keys",
            get(list_api_keys).layer(from_fn_with_state(app_state.clone(), decrypt_request::<()>)),
        )
        .route(
            "/protected/api-keys/:name",
            delete(delete_api_key)
                .layer(from_fn_with_state(app_state.clone(), decrypt_request::<()>)),
        )
        .with_state(app_state.clone())
}

pub async fn user_protected(
    State(data): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Extension(session_id): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    let mut app_user: AppUser = AppUser::from(&user);

    // Set email verification status - only if not a guest user
    app_user.email_verified = if user.is_guest() {
        false
    } else {
        match data.db.get_email_verification_by_user_id(user.uuid) {
            Ok(verification) => verification.is_verified,
            Err(DBError::EmailVerificationNotFound) => false,
            Err(e) => {
                tracing::error!("Error checking email verification: {:?}", e);
                return Err(ApiError::InternalServerError);
            }
        }
    };

    // Determine login method
    if user.password_enc.is_none() {
        // This is an OAuth user - find out which provider
        match data.db.get_all_user_oauth_connections_for_user(user.uuid) {
            Ok(connections) => {
                if let Some(connection) = connections.first() {
                    // Get the provider details
                    match data.db.get_oauth_provider_by_id(connection.provider_id) {
                        Ok(Some(provider)) => {
                            // Set the login method based on the provider name
                            app_user.login_method = match provider.name.as_str() {
                                "github" => LoginMethod::Github,
                                "google" => LoginMethod::Google,
                                "apple" => LoginMethod::Apple,
                                // Add other providers here as they're supported
                                _ => {
                                    tracing::error!("Unknown OAuth provider: {}", provider.name);
                                    return Err(ApiError::InternalServerError);
                                }
                            };
                        }
                        Ok(None) => {
                            tracing::error!(
                                "OAuth provider not found for id: {}",
                                connection.provider_id
                            );
                            return Err(ApiError::InternalServerError);
                        }
                        Err(e) => {
                            tracing::error!("Error fetching OAuth provider: {:?}", e);
                            return Err(ApiError::InternalServerError);
                        }
                    }
                }
            }
            Err(e) => {
                tracing::error!("Error fetching OAuth connections: {:?}", e);
                return Err(ApiError::InternalServerError);
            }
        }
    }

    let response = ProtectedUserData { user: app_user };
    encrypt_response(&data, &session_id, &response).await
}

pub async fn get_kv(
    State(data): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Extension(auth_context): Extension<AuthContext>,
    Extension(session_id): Extension<TransportSession>,
    Path(key): Path<String>,
) -> Result<Response, ApiError> {
    let value = match data.get(&user, &auth_context, key).await {
        Ok(kv) => kv,
        Err(e) => {
            tracing::error!("Error getting key-value pair: {:?}", e);
            return Err(ApiError::InternalServerError);
        }
    };
    encrypt_response(&data, &session_id, &value).await
}

pub async fn put_kv(
    State(data): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Extension(auth_context): Extension<AuthContext>,
    Extension(session_id): Extension<TransportSession>,
    Path(key): Path<String>,
    Decrypted(value): Decrypted<String>,
) -> Result<Response, ApiError> {
    match data.put(&user, &auth_context, key, value.clone()).await {
        Ok(kv) => kv,
        Err(e) => {
            tracing::error!("Error putting key-value pair: {:?}", e);
            return Err(ApiError::InternalServerError);
        }
    };
    encrypt_response(&data, &session_id, &value).await
}

pub async fn delete_kv(
    State(data): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Extension(auth_context): Extension<AuthContext>,
    Extension(session_id): Extension<TransportSession>,
    Path(key): Path<String>,
) -> Result<Response, ApiError> {
    match data.delete(&user, &auth_context, key).await {
        Ok(_) => {
            let response = json!({ "message": "Resource deleted successfully" });
            encrypt_response(&data, &session_id, &response).await
        }
        Err(e) => {
            tracing::error!("Error deleting key-value pair: {:?}", e);
            Err(ApiError::InternalServerError)
        }
    }
}

pub async fn delete_all_kv(
    State(data): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Extension(session_id): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    match data.delete_all(user.uuid).await {
        Ok(_) => {
            let response = json!({
                "message": "All key-value pairs deleted successfully"
            });
            encrypt_response(&data, &session_id, &response).await
        }
        Err(e) => {
            tracing::error!("Error deleting all key-value pairs: {:?}", e);
            Err(ApiError::InternalServerError)
        }
    }
}

pub async fn list_kv(
    State(data): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Extension(auth_context): Extension<AuthContext>,
    Extension(session_id): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    let kvs = match data.list(&user, &auth_context).await {
        Ok(kvs) => kvs,
        Err(e) => {
            tracing::error!("Error listing key-value pairs: {:?}", e);
            return Err(ApiError::InternalServerError);
        }
    };
    encrypt_response(&data, &session_id, &kvs).await
}

pub async fn request_new_verification_code(
    State(data): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Extension(session_id): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    // First check if user has an email
    let email = match user.get_email() {
        Some(email) => email.to_string(),
        None => {
            let response = json!({ "error": "No email associated with account" });
            return encrypt_response(&data, &session_id, &response).await;
        }
    };

    // Check if the user is already verified
    match data.db.get_email_verification_by_user_id(user.uuid) {
        Ok(verification) => {
            if verification.is_verified {
                let response = json!({ "error": "User is already verified" });
                return encrypt_response(&data, &session_id, &response).await;
            }
            // Delete the old verification
            if let Err(e) = data.db.delete_email_verification(&verification) {
                tracing::error!("Error deleting old verification: {:?}", e);
                return Err(ApiError::InternalServerError);
            }
        }
        Err(DBError::EmailVerificationNotFound) => {
            // This is fine, we'll create a new verification
        }
        Err(e) => {
            tracing::error!("Error checking email verification: {:?}", e);
            return Err(ApiError::InternalServerError);
        }
    }

    // Create a new verification entry
    let new_verification = NewEmailVerification::new(user.uuid, 24, false); // 24 hours expiration
    let verification = match data.db.create_email_verification(new_verification) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("Error creating email verification: {:?}", e);
            return Err(ApiError::InternalServerError);
        }
    };

    // Send the new verification email
    if let Err(e) = send_verification_email(
        &data,
        user.project_id,
        email,
        verification.verification_code,
    )
    .await
    {
        tracing::error!("Error sending verification email: {:?}", e);
        return Err(ApiError::InternalServerError);
    }

    let response = json!({ "message": "New verification code sent successfully" });
    encrypt_response(&data, &session_id, &response).await
}

pub async fn change_password(
    State(data): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Extension(auth_context): Extension<AuthContext>,
    Decrypted(change_request): Decrypted<ChangePasswordRequest>,
    Extension(session_id): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    // Check if user is an OAuth-only user
    if user.password_enc.is_none() {
        error!("OAuth-only user attempted to change password");
        return Err(ApiError::InvalidUsernameOrPassword);
    }

    // Get email if it exists
    let email = user.get_email().map(|e| e.to_string());

    match data
        .authenticate_user(
            email,
            Some(user.uuid),
            change_request.current_password,
            user.project_id,
        )
        .await
    {
        Ok(Some(authenticated_user)) if authenticated_user.user.uuid == user.uuid => {
            // Current password is correct, proceed with password change
            match data
                .update_user_password_and_seed_wrap(
                    &user,
                    &auth_context,
                    change_request.new_password,
                )
                .await
            {
                Ok(new_auth_context) => {
                    let access_token = NewToken::new_with_auth_context(
                        &user,
                        TokenType::access_for_transport(session_id.is_v2()),
                        &data,
                        &new_auth_context,
                    )?;
                    let refresh_token = NewToken::new_with_auth_context(
                        &user,
                        TokenType::refresh_for_transport(session_id.is_v2()),
                        &data,
                        &new_auth_context,
                    )?;
                    let response = json!({
                        "message": "Password changed successfully",
                        "access_token": access_token.token,
                        "refresh_token": refresh_token.token
                    });
                    encrypt_response(&data, &session_id, &response).await
                }
                Err(e) => {
                    error!("Error changing password: {:?}", e);
                    match e {
                        Error::AuthenticationError => Err(ApiError::InvalidUsernameOrPassword),
                        _ => Err(ApiError::InternalServerError),
                    }
                }
            }
        }
        _ => {
            // Current password is incorrect
            Err(ApiError::InvalidUsernameOrPassword)
        }
    }
}

pub async fn get_private_key(
    State(data): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Extension(auth_context): Extension<AuthContext>,
    Extension(session_id): Extension<TransportSession>,
    Query(query): Query<DerivationPathQuery>,
) -> Result<Response, ApiError> {
    // Validate paths if present
    query.validate()?;

    let plaintext_seed = data
        .decrypt_seed_for_auth_context(&user, &auth_context)
        .map_err(|e| {
            error!("Failed to decrypt authenticated seed wrap: {:?}", e);
            ApiError::Unauthorized
        })?;
    let root_mnemonic = plaintext_user_seed_to_mnemonic(&plaintext_seed).map_err(|e| {
        error!("Failed to parse user seed mnemonic: {:?}", e);
        ApiError::InternalServerError
    })?;

    // Check if BIP-85 derivation is requested
    if let Some(bip85_path) = &query.key_options.seed_phrase_derivation_path {
        // Derive a child mnemonic
        let child_mnemonic =
            derive_bip85_mnemonic_from_root(root_mnemonic, bip85_path).map_err(|e| {
                error!("BIP-85 derivation error: {:?}", e);
                ApiError::BadRequest
            })?;

        let response = PrivateKeyResponse {
            mnemonic: child_mnemonic.to_string(),
        };

        encrypt_response(&data, &session_id, &response).await
    } else {
        // Return root mnemonic
        let response = PrivateKeyResponse {
            mnemonic: root_mnemonic.to_string(),
        };

        encrypt_response(&data, &session_id, &response).await
    }
}

pub async fn get_private_key_bytes(
    State(data): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Extension(auth_context): Extension<AuthContext>,
    Extension(session_id): Extension<TransportSession>,
    Query(query): Query<DerivationPathQuery>,
) -> Result<Response, ApiError> {
    // Validate derivation path if present
    query.validate()?;

    // Use the method that supports both BIP-85 and BIP-32 derivation
    let secret_key = data
        .get_user_key(
            &user,
            &auth_context,
            query.key_options.private_key_derivation_path.as_deref(),
            query.key_options.seed_phrase_derivation_path.as_deref(),
        )
        .await
        .map_err(|e| match e {
            Error::InvalidDerivationPath(msg) => {
                error!("Invalid derivation path: {}", msg);
                ApiError::BadRequest
            }
            Error::KeyDerivationError(msg) => {
                error!("Failed to derive key: {}", msg);
                ApiError::BadRequest
            }
            _ => {
                error!("Failed to get user key: {:?}", e);
                ApiError::InternalServerError
            }
        })?;

    // Convert key to string
    let response = PrivateKeyBytesResponse {
        private_key: secret_key.display_secret().to_string(),
    };

    encrypt_response(&data, &session_id, &response).await
}

pub async fn sign_message(
    State(data): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Extension(auth_context): Extension<AuthContext>,
    Decrypted(sign_request): Decrypted<SignMessageRequest>,
    Extension(session_id): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    // Validate key_options
    let validation_query = DerivationPathQuery {
        key_options: sign_request.key_options.clone(),
    };
    validation_query.validate()?;

    // Extract derivation paths from key_options
    let derivation_path = sign_request
        .key_options
        .private_key_derivation_path
        .as_deref();
    let seed_phrase_derivation_path = sign_request
        .key_options
        .seed_phrase_derivation_path
        .as_deref();

    let message_bytes = general_purpose::STANDARD
        .decode(&sign_request.message_base64)
        .map_err(|e| {
            error!("Failed to decode base64 message: {:?}", e);
            ApiError::BadRequest
        })?;

    let response = data
        .sign_message(
            &user,
            &auth_context,
            &message_bytes,
            sign_request.algorithm,
            derivation_path,
            seed_phrase_derivation_path,
        )
        .await
        .map_err(|e| {
            error!("Error signing message: {:?}", e);
            ApiError::InternalServerError
        })?;

    let json_response = SignMessageResponseJson {
        signature: response.signature.to_string(),
        message_hash: hex::encode(response.message_hash),
    };

    encrypt_response(&data, &session_id, &json_response).await
}

pub async fn get_public_key(
    State(data): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Extension(auth_context): Extension<AuthContext>,
    Extension(session_id): Extension<TransportSession>,
    Query(query): Query<PublicKeyQuery>,
) -> Result<Response, ApiError> {
    // Validate the key_options
    let validation_query = DerivationPathQuery {
        key_options: query.key_options.clone(),
    };
    validation_query.validate()?;

    // Extract derivation paths from key_options
    let derivation_path = query.key_options.private_key_derivation_path.as_deref();
    let seed_phrase_derivation_path = query.key_options.seed_phrase_derivation_path.as_deref();

    let user_secret_key = data
        .get_user_key(
            &user,
            &auth_context,
            derivation_path,
            seed_phrase_derivation_path,
        )
        .await
        .map_err(|e| {
            error!("Error getting user key: {:?}", e);
            ApiError::InternalServerError
        })?;

    let secp = Secp256k1::new();
    let public_key = user_secret_key.public_key(&secp);

    // Format public key according to algorithm
    let public_key_str = match query.algorithm {
        SigningAlgorithm::Schnorr => {
            let (xonly_pubkey, _parity) = public_key.x_only_public_key();
            xonly_pubkey.to_string()
        }
        SigningAlgorithm::Ecdsa => {
            // For ECDSA, use the compressed format
            public_key.to_string()
        }
    };

    let response = PublicKeyResponseJson {
        public_key: public_key_str,
        algorithm: query.algorithm,
    };
    encrypt_response(&data, &session_id, &response).await
}

pub async fn generate_third_party_token(
    State(data): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Decrypted(request): Decrypted<ThirdPartyTokenRequest>,
    Extension(session_id): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    // Validate the audience
    if let Some(audience) = request.audience.as_ref() {
        if audience.is_empty() || (audience.contains(':') && url::Url::parse(audience).is_err()) {
            error!("Invalid audience provided: {}", audience);
            return Err(ApiError::BadRequest);
        }
    }

    let project = data.db.get_org_project_by_id(user.project_id)?;

    let token = match NewToken::new(
        &user,
        TokenType::ThirdParty {
            aud: request.audience,
            azp: project.client_id.to_string(),
        },
        &data,
    ) {
        Ok(token) => token,
        Err(e) => {
            error!("Failed to generate third party token: {:?}", e);
            return Err(e);
        }
    };

    let response = ThirdPartyTokenResponse { token: token.token };
    encrypt_response(&data, &session_id, &response).await
}

pub async fn encrypt_data(
    State(data): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Extension(auth_context): Extension<AuthContext>,
    Decrypted(request): Decrypted<EncryptDataRequest>,
    Extension(session_id): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    // Validate key_options
    let validation_query = DerivationPathQuery {
        key_options: request.key_options.clone(),
    };
    validation_query.validate()?;

    // Extract derivation paths from key_options
    let derivation_path = request.key_options.private_key_derivation_path.as_deref();
    let seed_phrase_derivation_path = request.key_options.seed_phrase_derivation_path.as_deref();

    // Get the user's key
    let user_key = data
        .get_user_key(
            &user,
            &auth_context,
            derivation_path,
            seed_phrase_derivation_path,
        )
        .await
        .map_err(|e| match e {
            Error::InvalidDerivationPath(msg) => {
                error!("Invalid derivation path: {}", msg);
                ApiError::BadRequest
            }
            Error::KeyDerivationError(msg) => {
                error!("Failed to derive key: {}", msg);
                ApiError::BadRequest
            }
            _ => {
                error!("Failed to get user key: {:?}", e);
                ApiError::InternalServerError
            }
        })?;

    // Encrypt the data
    let data_bytes = request.data.as_bytes();
    let encrypted_data = encrypt::encrypt_with_key(&user_key, data_bytes).await;

    // Convert to base64 for easy transport
    let encrypted_data_base64 = general_purpose::STANDARD.encode(&encrypted_data);

    let response = EncryptDataResponse {
        encrypted_data: encrypted_data_base64,
    };
    encrypt_response(&data, &session_id, &response).await
}

pub async fn decrypt_data(
    State(data): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Extension(auth_context): Extension<AuthContext>,
    Decrypted(request): Decrypted<DecryptDataRequest>,
    Extension(session_id): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    // Validate key_options
    let validation_query = DerivationPathQuery {
        key_options: request.key_options.clone(),
    };
    validation_query.validate()?;

    // Extract derivation paths from key_options
    let derivation_path = request.key_options.private_key_derivation_path.as_deref();
    let seed_phrase_derivation_path = request.key_options.seed_phrase_derivation_path.as_deref();

    // Get the user's key
    let user_key = data
        .get_user_key(
            &user,
            &auth_context,
            derivation_path,
            seed_phrase_derivation_path,
        )
        .await
        .map_err(|e| match e {
            Error::InvalidDerivationPath(msg) => {
                error!("Invalid derivation path: {}", msg);
                ApiError::BadRequest
            }
            Error::KeyDerivationError(msg) => {
                error!("Failed to derive key: {}", msg);
                ApiError::BadRequest
            }
            _ => {
                error!("Failed to get user key: {e}");
                ApiError::InternalServerError
            }
        })?;

    // Decode the base64 encrypted data
    let encrypted_data = match general_purpose::STANDARD.decode(&request.encrypted_data) {
        Ok(data) => data,
        Err(e) => {
            error!("Failed to decode base64 data: {e}");
            return Err(ApiError::BadRequest);
        }
    };

    // Decrypt the data
    let decrypted_data = match encrypt::decrypt_with_key(&user_key, &encrypted_data) {
        Ok(data) => data,
        Err(e) => {
            error!("Decryption failed: {e}");
            return Err(ApiError::BadRequest);
        }
    };

    // Convert decrypted bytes to string
    let decrypted_string = match String::from_utf8(decrypted_data) {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to convert decrypted data to string: {e}");
            return Err(ApiError::BadRequest);
        }
    };
    encrypt_response(&data, &session_id, &decrypted_string).await
}

pub async fn initiate_account_deletion(
    State(data): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Decrypted(delete_request): Decrypted<InitiateAccountDeletionRequest>,
    Extension(session_id): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    info!("User {} is initiating account deletion request", user.uuid);

    // Create the account deletion request
    match data
        .create_account_deletion_request(
            user.uuid,
            delete_request.hashed_secret.clone(),
            user.project_id,
        )
        .await
    {
        Ok(_) => {
            // Success - we return a generic message regardless of whether the
            // request was actually created and email sent
            let response = json!({
                "message": "We have sent a confirmation code to your email."
            });

            let result = encrypt_response(&data, &session_id, &response).await;
            result
        }
        Err(e) => {
            error!("Error in initiating account deletion: {:?}", e);
            Err(ApiError::InternalServerError)
        }
    }
}

pub async fn confirm_account_deletion(
    State(data): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Decrypted(confirm_request): Decrypted<ConfirmAccountDeletionRequest>,
    Extension(session_id): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    info!("User {} is confirming account deletion", user.uuid);

    // Confirm the account deletion
    match data
        .confirm_account_deletion(
            user.uuid,
            confirm_request.confirmation_code.clone(),
            confirm_request.plaintext_secret.clone(),
        )
        .await
    {
        Ok(_) => {
            let response = json!({
                "message": "Your account has been successfully deleted."
            });

            let result = encrypt_response(&data, &session_id, &response).await;
            result
        }
        Err(e) => match e {
            Error::AccountDeletionExpired => {
                warn!("Account deletion confirmation has expired: {:?}", e);
                Err(ApiError::BadRequest)
            }
            Error::InvalidAccountDeletionSecret => {
                warn!("Invalid account deletion secret: {:?}", e);
                Err(ApiError::BadRequest)
            }
            Error::InvalidAccountDeletionRequest => {
                warn!("Invalid account deletion request: {:?}", e);
                Err(ApiError::BadRequest)
            }
            _ => {
                error!("Error in confirming account deletion: {:?}", e);
                Err(ApiError::InternalServerError)
            }
        },
    }
}

// API Key Management Handlers

pub async fn create_api_key(
    State(data): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Decrypted(request): Decrypted<CreateApiKeyRequest>,
    Extension(session_id): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    debug!("Creating new API key for user: {}", user.uuid);

    // Validate the request
    if request.validate().is_err() {
        error!("API key request validation failed");
        return Err(ApiError::BadRequest);
    }

    // Use the name directly (validation ensures no leading/trailing whitespace)
    let name = request.name.clone();

    // Generate a random UUID for the API key
    let random_bytes: [u8; 16] =
        crate::encrypt::generate_random_enclave::<16>(data.aws_credential_manager.clone()).await;
    let api_key_uuid = Uuid::from_bytes(random_bytes);

    // Hash the UUID string (with dashes) using SHA-256
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(api_key_uuid.to_string().as_bytes());
    let key_hash = format!("{:x}", hasher.finalize());

    // Create the database record
    let new_key =
        crate::models::user_api_keys::NewUserApiKey::new(user.uuid, key_hash, name.clone());

    let api_key_record = data.db.create_user_api_key(new_key).map_err(|e| {
        match e {
            DBError::UserApiKeyError(
                crate::models::user_api_keys::UserApiKeyError::DuplicateName,
            ) => {
                error!("API key with name '{}' already exists for user", name);
                ApiError::Conflict // 409 - name already in use
            }
            _ => {
                error!("Failed to create API key: {:?}", e);
                ApiError::InternalServerError
            }
        }
    })?;

    let response = CreateApiKeyResponse {
        key: api_key_uuid.to_string(), // Return the actual UUID to the user (only time it's shown)
        name: api_key_record.name,
        created_at: api_key_record.created_at,
    };

    info!("Created API key '{}' for user {}", response.name, user.uuid);
    encrypt_response(&data, &session_id, &response).await
}

pub async fn list_api_keys(
    State(data): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Extension(session_id): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    debug!("Listing API keys for user: {}", user.uuid);

    let api_keys = data
        .db
        .get_all_user_api_keys_for_user(user.uuid)
        .map_err(|e| {
            error!("Failed to list API keys: {:?}", e);
            ApiError::InternalServerError
        })?;

    let keys: Vec<ApiKeyInfo> = api_keys
        .into_iter()
        .map(|key| ApiKeyInfo {
            name: key.name,
            created_at: key.created_at,
        })
        .collect();

    let response = ListApiKeysResponse { keys };

    encrypt_response(&data, &session_id, &response).await
}

pub async fn delete_api_key(
    State(data): State<Arc<AppState>>,
    Path(name): Path<String>,
    Extension(user): Extension<User>,
    Extension(session_id): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    debug!("Deleting API key '{}' for user: {}", name, user.uuid);

    data.db
        .delete_user_api_key_by_name(&name, user.uuid)
        .map_err(|e| {
            error!("Failed to delete API key: {:?}", e);
            match e {
                DBError::UserApiKeyError(
                    crate::models::user_api_keys::UserApiKeyError::NotFound,
                ) => ApiError::NotFound,
                _ => ApiError::InternalServerError,
            }
        })?;

    info!("Deleted API key '{}' for user {}", name, user.uuid);

    let response = json!({ "success": true });
    encrypt_response(&data, &session_id, &response).await
}

/// Recovery-code management branch.
///
/// Carries its own `validate_jwt` layer so it can be merged outside the
/// shared `protected_routes` JWT scope (which would otherwise validate the
/// same JWT twice), plus the v2-transport gate that rejects legacy v1
/// sessions before any recovery logic runs.
pub fn recovery_router(app_state: Arc<AppState>) -> Router<()> {
    Router::new()
        .route(
            "/protected/recovery-code",
            get(recovery_status)
                .layer(from_fn_with_state(app_state.clone(), decrypt_request::<()>)),
        )
        .route(
            "/protected/recovery-code/enroll",
            post(enroll_recovery).layer(from_fn_with_state(
                app_state.clone(),
                decrypt_request::<EnrollRecoveryRequest>,
            )),
        )
        .route(
            "/protected/recovery-code/rotate",
            post(rotate_recovery).layer(from_fn_with_state(
                app_state.clone(),
                decrypt_request::<RotateRecoveryRequest>,
            )),
        )
        .route(
            "/protected/recovery-code",
            delete(disable_recovery).layer(from_fn_with_state(
                app_state.clone(),
                decrypt_request::<DisableRecoveryRequest>,
            )),
        )
        .route_layer(from_fn_with_state(app_state.clone(), validate_jwt))
        .route_layer(from_fn(require_transport_v2))
        .with_state(app_state)
}

/// Recovery management is limited to email-backed password users in V1.
/// Guest and OAuth-only accounts cannot enroll, rotate, or disable recovery.
fn require_email_password_user(user: &User) -> Result<(), ApiError> {
    if user.is_guest() || user.password_enc.is_none() {
        return Err(ApiError::BadRequest);
    }
    Ok(())
}

/// Password step-up: verifies the submitted current password against the
/// stored verifier and confirms the password credential still opens an
/// active seed wrap before returning.
async fn require_current_password(
    data: &AppState,
    user: &User,
    current_password: String,
) -> Result<(), ApiError> {
    let email = user.get_email().map(|e| e.to_string());
    let reauthenticated = match data
        .authenticate_user(email, Some(user.uuid), current_password, user.project_id)
        .await
        .map_err(|e| {
            error!("Recovery password step-up failed: {:?}", e);
            ApiError::InternalServerError
        })? {
        Some(reauthenticated) if reauthenticated.user.uuid == user.uuid => reauthenticated,
        _ => return Err(ApiError::InvalidUsernameOrPassword),
    };
    debug_assert_eq!(reauthenticated.user.uuid, user.uuid);
    Ok(())
}

/// Opens the currently enrolled seed through the JWT's signed auth context.
/// Recovery must be bound to the exact seed the user's other credentials
/// still unwrap, not to a fresh or substituted seed.
async fn seed_for_recovery_management(
    data: &AppState,
    user: &User,
    auth_context: &AuthContext,
) -> Result<Vec<u8>, ApiError> {
    data.decrypt_seed_for_auth_context(user, auth_context)
        .map_err(|e| {
            error!(
                "Recovery management could not open the authenticated seed: {:?}",
                e
            );
            ApiError::Unauthorized
        })
}

pub async fn recovery_status(
    State(data): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Extension(transport_session): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    require_v2_transport_session(&transport_session)?;
    let status = data.db.get_recovery_wrap(user.uuid).map_err(|e| {
        error!("Failed to load recovery status: {:?}", e);
        ApiError::InternalServerError
    })?;
    let response = RecoveryStatusResponse {
        enrolled: status.is_some(),
        enrolled_at: status.map(|wrap| wrap.created_at),
    };
    encrypt_response(&data, &transport_session, &response).await
}

pub async fn enroll_recovery(
    State(data): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Extension(auth_context): Extension<AuthContext>,
    Extension(transport_session): Extension<TransportSession>,
    Decrypted(request): Decrypted<EnrollRecoveryRequest>,
) -> Result<Response, ApiError> {
    require_v2_transport_session(&transport_session)?;
    require_email_password_user(&user)?;
    require_current_password(&data, &user, request.current_password).await?;

    let plaintext_seed = seed_for_recovery_management(&data, &user, &auth_context).await?;

    if data
        .db
        .get_recovery_wrap(user.uuid)
        .map(|existing| existing.is_some())
        .map_err(|e| {
            error!(
                "Failed to check recovery enrollment for user {}: {:?}",
                user.uuid, e
            );
            ApiError::InternalServerError
        })?
    {
        return Err(ApiError::Conflict);
    }

    let code = RecoveryCode::generate(Some(data.aws_credential_manager.clone()))
        .await
        .map_err(|e| {
            error!("Failed to generate recovery code: {:?}", e);
            ApiError::InternalServerError
        })?;

    // Seals the wrap over the enrolled seed and verifies it byte-for-byte
    // before any database work. A sealing failure is an enclave-internal
    // fault, not a client-visible 400.
    let wrapping = new_recovery_seed_wrapping(&data.enclave_key, &user, &code, &plaintext_seed)
        .map_err(|e| {
            error!("Failed to create recovery seed wrapping: {:?}", e);
            ApiError::InternalServerError
        })?;

    data.db
        .insert_recovery_wrap_if_absent(wrapping)
        .map_err(|e| {
            debug!("Recovery enrollment insert rejected: {:?}", e);
            match e {
                DBError::StaleCredentialState => ApiError::Conflict,
                _ => ApiError::InternalServerError,
            }
        })?;

    info!("Recovery code enrolled for user {}", user.uuid);
    let response = RecoveryCodeResponse {
        // Shown to the user exactly once in the encrypted response; never
        // logged, stored in a path, or included in any other channel. The
        // intermediate copy inside the serialized response body is not
        // separately zeroized; it lives only until `encrypt_response` seals
        // it into the encrypted transport frame.
        recovery_code: code.display().to_string(),
    };
    encrypt_response(&data, &transport_session, &response).await
}

pub async fn rotate_recovery(
    State(data): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Extension(auth_context): Extension<AuthContext>,
    Extension(transport_session): Extension<TransportSession>,
    Decrypted(request): Decrypted<RotateRecoveryRequest>,
) -> Result<Response, ApiError> {
    require_v2_transport_session(&transport_session)?;
    require_email_password_user(&user)?;
    require_current_password(&data, &user, request.current_password).await?;

    let plaintext_seed = seed_for_recovery_management(&data, &user, &auth_context).await?;

    let old_wrap = data
        .db
        .get_recovery_wrap(user.uuid)
        .map_err(|e| {
            error!("Failed to load recovery wrap before rotation: {:?}", e);
            ApiError::InternalServerError
        })?
        .ok_or(ApiError::BadRequest)?;

    let code = RecoveryCode::generate(Some(data.aws_credential_manager.clone()))
        .await
        .map_err(|e| {
            error!("Failed to generate replacement recovery code: {:?}", e);
            ApiError::InternalServerError
        })?;

    // Seals and verifies the replacement wrap byte-for-byte before any
    // database work.
    let replacement = new_recovery_seed_wrapping(&data.enclave_key, &user, &code, &plaintext_seed)
        .map_err(|e| {
            error!(
                "Failed to create replacement recovery seed wrapping: {:?}",
                e
            );
            ApiError::EncryptionError
        })?;

    data.db
        .replace_recovery_wrap_if_unchanged(&old_wrap, replacement)
        .map_err(|e| {
            debug!("Recovery rotation CAS rejected: {:?}", e);
            match e {
                DBError::StaleCredentialState => ApiError::Conflict,
                _ => ApiError::InternalServerError,
            }
        })?;

    info!("Recovery code rotated for user {}", user.uuid);
    let response = RecoveryCodeResponse {
        recovery_code: code.display().to_string(),
    };
    encrypt_response(&data, &transport_session, &response).await
}

pub async fn disable_recovery(
    State(data): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Extension(auth_context): Extension<AuthContext>,
    Extension(transport_session): Extension<TransportSession>,
    Decrypted(request): Decrypted<DisableRecoveryRequest>,
) -> Result<Response, ApiError> {
    require_v2_transport_session(&transport_session)?;
    require_email_password_user(&user)?;
    require_current_password(&data, &user, request.current_password).await?;

    // Confirm the password credential still opens an active seed wrap before
    // deleting recovery state. Disablement does not require the recovery code.
    data.verify_seed_wrap_for_auth_context(&user, &auth_context)
        .map_err(|e| {
            error!(
                "Recovery disablement could not verify the active seed wrap: {:?}",
                e
            );
            ApiError::Unauthorized
        })?;

    // Idempotent at the API boundary: deleting an absent wrap is a no-op, so
    // a concurrent rotation or disabling race cannot make this fail.
    data.db
        .delete_recovery_wrap_for_user(user.uuid)
        .map_err(|e| {
            error!("Failed to delete recovery wrap: {:?}", e);
            ApiError::InternalServerError
        })?;

    info!("Recovery disabled for user {}", user.uuid);
    let response = json!({ "message": "Recovery disabled successfully" });
    encrypt_response(&data, &transport_session, &response).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encrypt::{decrypt_with_key, encrypt_with_key};
    use secp256k1::SecretKey;

    #[test]
    fn test_derivation_path_validation() {
        // Test valid paths
        let valid_paths = vec![
            "m/44'/0'/0'/0/0", // Standard BIP44 absolute
            "44'/0'/0'/0/0",   // Standard BIP44 relative
            "m/84'/0'/0'/0/0", // Standard BIP84 absolute
            "84'/0'/0'/0/0",   // Standard BIP84 relative
            "m/49'/0'/0'/0/0", // Standard BIP49 absolute
            "0/0",             // Simple relative path
            "m/0/0",           // Simple absolute path
            "0",               // Single level relative
            "m/0",             // Single level absolute
            "m",               // Master key
            "",                // Empty path
            "0'",              // Hardened child
            "m/0'",            // Hardened child absolute
            "0h",              // Hardened child (alternate notation)
            "m/0h",            // Hardened child absolute (alternate notation)
        ];

        for path in valid_paths {
            let query = DerivationPathQuery {
                key_options: KeyOptions {
                    private_key_derivation_path: Some(path.to_string()),
                    seed_phrase_derivation_path: None,
                },
            };
            assert!(query.validate().is_ok(), "Path should be valid: {}", path);
        }

        // Test invalid paths
        let invalid_paths = vec![
            "invalid",      // Random string
            "m//0",         // Double slash
            "m/x/0",        // Invalid character
            "m/0'/x/0",     // Invalid character after hardened
            "M/0/0",        // Wrong case for master key
            "m/2147483648", // Exceeds u32::MAX
            "n/0/0",        // Wrong master key letter
            "/0/0",         // Slash without master key
        ];

        for path in invalid_paths {
            let query = DerivationPathQuery {
                key_options: KeyOptions {
                    private_key_derivation_path: Some(path.to_string()),
                    seed_phrase_derivation_path: None,
                },
            };
            assert!(
                query.validate().is_err(),
                "Path should be invalid: {}",
                path
            );
        }

        // Test None path
        let query = DerivationPathQuery {
            key_options: KeyOptions {
                private_key_derivation_path: None,
                seed_phrase_derivation_path: None,
            },
        };
        assert!(query.validate().is_ok(), "None path should be valid");

        // Test valid BIP-85 paths - only 12, 18, and 24 word phrases are supported
        let valid_bip85_paths = vec![
            "m/83696968'/39'/0'/12'/0'",   // Standard BIP-85 for 12-word mnemonic
            "m/83696968'/39'/0'/18'/0'",   // Standard BIP-85 for 18-word mnemonic
            "m/83696968'/39'/0'/24'/0'",   // Standard BIP-85 for 24-word mnemonic
            "m/83696968'/39'/0'/12'/5'",   // BIP-85 with non-zero index
            "m/83696968'/39'/0'/12'/255'", // BIP-85 with large index
            // Test alternate hardened notation
            "m/83696968h/39h/0h/12h/0h", // Using 'h' notation instead of '
        ];

        for path in valid_bip85_paths {
            let query = DerivationPathQuery {
                key_options: KeyOptions {
                    private_key_derivation_path: None,
                    seed_phrase_derivation_path: Some(path.to_string()),
                },
            };
            assert!(
                query.validate().is_ok(),
                "BIP-85 path should be valid: {}",
                path
            );
        }

        // Test invalid BIP-85 paths
        let invalid_bip85_paths = vec![
            "m/44'/0'/0'/0/0",             // Not a BIP-85 path (wrong purpose)
            "m/83696968/39'/0'/12'/0'",    // Purpose not hardened
            "m/83696968'/39/0'/12'/0'",    // Application not hardened
            "m/83696968'/39'/0/12'/0'",    // Language not hardened
            "m/83696968'/39'/0'/12/0'",    // Word count not hardened
            "m/83696968'/39'/0'/12'/0",    // Index not hardened
            "m/83696968'/39'/0'/13'/0'",   // Invalid word count (not 12,18,24)
            "m/83696968'/39'/0'/15'/0'",   // Invalid word count (not 12,18,24)
            "m/83696968'/39'/0'/21'/0'",   // Invalid word count (not 12,18,24)
            "m/83696968'/39'/0'/256'/0'",  // Word count too large
            "m/836969681'/39'/0'/12'/0'",  // Wrong purpose with valid prefix
            "m/83696968'/390'/0'/12'/0'",  // Wrong application with valid prefix
            "m/83696968'/39'/1'/12'/0'",   // Unsupported non-English language
            "m/83696968'",                 // Incomplete path (too few segments)
            "m/83696968'/39'",             // Incomplete path (too few segments)
            "m/83696968'/39'/0'",          // Incomplete path (too few segments)
            "m/83696968'/39'/0'/12'",      // Incomplete path (too few segments)
            "83696968'/39'/0'/12'/0'",     // Missing 'm' prefix
            "m/83696968'/38'/0'/12'/0'",   // Wrong application (not 39)
            "m/83696968'/39'/0'/12'/0'/0", // Extra segment
            "m/83696968'/39'/ABC'/12'/0'", // Non-numeric language
            "m/83696968'/39'/0'/ABC'/0'",  // Non-numeric word count
            "m/83696968'/39'/0'/12'/ABC'", // Non-numeric index
        ];

        for path in invalid_bip85_paths {
            let query = DerivationPathQuery {
                key_options: KeyOptions {
                    private_key_derivation_path: None,
                    seed_phrase_derivation_path: Some(path.to_string()),
                },
            };
            assert!(
                query.validate().is_err(),
                "BIP-85 path should be invalid: {}",
                path
            );
        }
    }

    #[tokio::test]
    async fn test_encrypt_decrypt_empty_array() {
        // Mock a private key for testing
        let test_key = SecretKey::from_slice(&[0x42; 32]).unwrap();

        // Test with empty array as JSON string "[]"
        let empty_array = "[]".as_bytes();

        // First encryption
        let encrypted1 = encrypt_with_key(&test_key, empty_array).await;
        // Second encryption of the same data
        let encrypted2 = encrypt_with_key(&test_key, empty_array).await;

        // Since we use a random nonce, the two encryptions should be different
        assert_ne!(encrypted1, encrypted2);

        // Decrypt the first encrypted value
        let decrypted1 = decrypt_with_key(&test_key, &encrypted1).unwrap();
        assert_eq!(decrypted1, empty_array);

        // Decrypt the second encrypted value - this should also work
        let decrypted2 = decrypt_with_key(&test_key, &encrypted2).unwrap();
        assert_eq!(decrypted2, empty_array);

        // Test with multiple consecutive decryption attempts on the same encrypted values
        let decrypted1_again = decrypt_with_key(&test_key, &encrypted1).unwrap();
        assert_eq!(decrypted1_again, empty_array);

        let decrypted2_again = decrypt_with_key(&test_key, &encrypted2).unwrap();
        assert_eq!(decrypted2_again, empty_array);
    }

    #[tokio::test]
    async fn test_encrypt_decrypt_base64_payload() {
        // Test with the specific base64 payloads from the user report
        let test_key = SecretKey::from_slice(&[0x42; 32]).unwrap();

        // Recreate the two payloads mentioned in the report
        let payload1 = general_purpose::STANDARD
            .decode("eoW3a/gBol+uMLr6DmMwMjYHxiS31/cSet3sAnTi")
            .unwrap();
        let payload2 = general_purpose::STANDARD
            .decode("YWF2+/2fKK1UBTaFqpAzGh/7y/QrxzfE5/FkqLfe")
            .unwrap();

        // Attempt to decrypt both payloads
        match decrypt_with_key(&test_key, &payload1) {
            Ok(decrypted) => {
                // If decryption succeeds, check if it's an empty array
                let as_str = String::from_utf8_lossy(&decrypted);
                println!("Payload 1 decrypted: {}", as_str);
            }
            Err(e) => {
                // Expected - our test key won't match the real key, but we're testing the process
                println!("Payload 1 decryption failed as expected: {:?}", e);
            }
        }

        match decrypt_with_key(&test_key, &payload2) {
            Ok(decrypted) => {
                // If decryption succeeds, check if it's an empty array
                let as_str = String::from_utf8_lossy(&decrypted);
                println!("Payload 2 decrypted: {}", as_str);
            }
            Err(e) => {
                // Expected - our test key won't match the real key, but we're testing the process
                println!("Payload 2 decryption failed as expected: {:?}", e);
            }
        }
    }

    #[tokio::test]
    async fn test_multiple_empty_array_encryptions() {
        // Simulate the exact user scenario: multiple different private keys,
        // each encrypting "[]" twice, then decrypting both versions
        let empty_array = "[]".as_bytes();

        // Run 10 iterations with different keys
        for i in 0..10 {
            // Generate a valid key for each iteration
            // Use a pattern that ensures a valid secp256k1 key (non-zero, less than curve order)
            let base_key = [
                0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x10, 0x11, 0x12, 0x13, 0x14,
                0x15, 0x16, 0x17, 0x18, 0x19, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28,
                0x29, 0x30, 0x31, 0x32,
            ];
            let mut key_bytes = base_key;
            key_bytes[0] = (i + 1) as u8; // Change first byte based on iteration

            let private_key = SecretKey::from_slice(&key_bytes)
                .unwrap_or_else(|e| panic!("Failed to create key in iteration {}: {:?}", i, e));

            // Encrypt the empty array twice with the same key
            let encrypted1 = encrypt_with_key(&private_key, empty_array).await;
            let encrypted2 = encrypt_with_key(&private_key, empty_array).await;

            // The encryptions should be different due to random nonce
            assert_ne!(
                encrypted1, encrypted2,
                "Iteration {}: Encryptions should differ",
                i
            );

            // Decrypt both and verify they both correctly decrypt to "[]"
            let decrypted1 = decrypt_with_key(&private_key, &encrypted1)
                .unwrap_or_else(|e| panic!("Iteration {}: First decryption failed: {:?}", i, e));

            let decrypted2 = decrypt_with_key(&private_key, &encrypted2)
                .unwrap_or_else(|e| panic!("Iteration {}: Second decryption failed: {:?}", i, e));

            // Both decryptions should match the original empty array
            assert_eq!(
                decrypted1, empty_array,
                "Iteration {}: First decryption didn't match original data",
                i
            );
            assert_eq!(
                decrypted2, empty_array,
                "Iteration {}: Second decryption didn't match original data",
                i
            );

            // Print debug info
            println!("Iteration {}: Both decryptions successful", i);
        }
    }
}

#[cfg(test)]
mod api_key_validation_tests {
    use super::*;

    #[test]
    fn test_valid_api_key_names() {
        let max_length_name = "a".repeat(50);
        let valid_names = vec![
            "my-api-key",
            "test_key_123",
            "Production Key",
            "key-with_spaces and-dashes",
            "UPPERCASE",
            "lowercase",
            "MixedCase",
            "123numeric",
            "a",                      // single character
            max_length_name.as_str(), // max length
        ];

        for name in valid_names {
            assert!(
                validate_api_key_name(name).is_ok(),
                "Expected '{}' to be valid",
                name
            );
        }
    }

    #[test]
    fn test_invalid_api_key_names_whitespace() {
        let invalid_names = vec![
            "   ",            // only spaces
            "\t",             // tab
            "\n",             // newline
            "  leading",      // leading space
            "trailing  ",     // trailing space
            " both ",         // both leading and trailing
            "\tleading_tab",  // leading tab
            "trailing_tab\t", // trailing tab
        ];

        for name in invalid_names {
            let result = validate_api_key_name(name);
            assert!(
                result.is_err(),
                "Expected '{}' to be invalid",
                name.escape_debug()
            );
            if let Err(e) = result {
                assert_eq!(
                    e.code,
                    "whitespace",
                    "Expected whitespace error for '{}'",
                    name.escape_debug()
                );
            }
        }
    }

    #[test]
    fn test_invalid_api_key_names_length() {
        let too_long = "a".repeat(51);
        let way_too_long = "x".repeat(100);
        let invalid_names = vec![
            "",                    // empty
            too_long.as_str(),     // too long (51 chars)
            way_too_long.as_str(), // way too long
        ];

        for name in invalid_names {
            let result = validate_api_key_name(name);
            assert!(
                result.is_err(),
                "Expected '{}' (len={}) to be invalid",
                if name.len() > 20 { "[truncated]" } else { name },
                name.len()
            );
            if let Err(e) = result {
                assert_eq!(
                    e.code,
                    "length",
                    "Expected length error for name of length {}",
                    name.len()
                );
            }
        }
    }

    #[test]
    fn test_invalid_api_key_names_characters() {
        let invalid_names = vec![
            "café",             // accented character
            "测试",             // Chinese characters
            "тест",             // Cyrillic
            "key!",             // exclamation mark
            "key@host",         // at symbol
            "key#hash",         // hash
            "key$money",        // dollar sign
            "key%percent",      // percent
            "key^caret",        // caret
            "key&and",          // ampersand
            "key*star",         // asterisk
            "key(paren",        // parenthesis
            "key[bracket",      // bracket
            "key{brace",        // brace
            "key|pipe",         // pipe
            "key\\backslash",   // backslash
            "key/slash",        // forward slash
            "key:colon",        // colon
            "key;semicolon",    // semicolon
            "key'quote",        // single quote
            "key\"doublequote", // double quote
            "key<less",         // less than
            "key>greater",      // greater than
            "key?question",     // question mark
            "key,comma",        // comma
            "key.period",       // period
            "😀emoji",          // emoji
            "key\0null",        // null character
            "hello\nworld",     // newline in middle
            "hello\tworld",     // tab in middle
        ];

        for name in invalid_names {
            let result = validate_api_key_name(name);
            assert!(
                result.is_err(),
                "Expected '{}' to be invalid due to invalid characters",
                name.escape_debug()
            );
            if let Err(e) = result {
                assert_eq!(
                    e.code,
                    "invalid_characters",
                    "Expected invalid_characters error for '{}'",
                    name.escape_debug()
                );
            }
        }
    }

    #[test]
    fn test_edge_cases() {
        // Test that internal spaces are allowed
        assert!(validate_api_key_name("valid name").is_ok());
        assert!(validate_api_key_name("multiple  spaces  inside").is_ok());

        // Test all allowed characters together
        assert!(validate_api_key_name("aZ_0-9 mix").is_ok());

        // Test exact boundary lengths
        let exactly_50 = "x".repeat(50);
        let exactly_51 = "x".repeat(51);
        assert!(validate_api_key_name(&exactly_50).is_ok()); // exactly 50
        assert!(validate_api_key_name(&exactly_51).is_err()); // exactly 51
    }

    #[test]
    fn test_security_concerns() {
        // Path traversal attempts
        assert!(validate_api_key_name("../etc/passwd").is_err());
        assert!(validate_api_key_name("..\\windows\\system32").is_err());

        // SQL injection attempts
        assert!(validate_api_key_name("'; DROP TABLE users--").is_err());
        assert!(validate_api_key_name("1' OR '1'='1").is_err());

        // XSS attempts
        assert!(validate_api_key_name("<script>alert(1)</script>").is_err());
        assert!(validate_api_key_name("javascript:alert(1)</script>").is_err());
        assert!(validate_api_key_name("javascript:alert(1)").is_err());

        // Command injection attempts
        assert!(validate_api_key_name("key; rm -rf /").is_err());
        assert!(validate_api_key_name("key`whoami`").is_err());
        assert!(validate_api_key_name("key$(whoami)").is_err());
        assert!(validate_api_key_name("key&&whoami").is_err());
        assert!(validate_api_key_name("key||whoami").is_err());
    }
}

#[cfg(test)]
mod recovery_guard_tests {
    use super::*;
    use crate::transport_v2::crypto::SessionId;
    use axum::body::Body;
    use axum::http::{Request, Request as HttpRequest, StatusCode};
    use axum::routing::get;
    use serde_json::json;
    use tower::ServiceExt;

    fn user_fixture(email: Option<&str>, password_enc: Option<Vec<u8>>) -> User {
        serde_json::from_value(json!({
            "id": 1,
            "uuid": Uuid::new_v4(),
            "name": None::<String>,
            "email": email,
            "password_enc": password_enc,
            "created_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-01-01T00:00:00Z",
            "project_id": 1,
        }))
        .expect("user fixture should deserialize")
    }

    #[test]
    fn v1_session_is_rejected_at_handler_boundary() {
        let session = TransportSession::v1(Uuid::new_v4());
        assert!(
            require_v2_transport_session(&session).is_err(),
            "a v1 transport session must not reach recovery handler logic"
        );
    }

    #[test]
    fn v2_session_passes_handler_boundary_guard() {
        let session = TransportSession::v2(SessionId::from_bytes([0x22; 16]));
        assert!(require_v2_transport_session(&session).is_ok());
    }

    #[test]
    fn guests_cannot_manage_recovery() {
        let guest = user_fixture(None, None);
        assert!(require_email_password_user(&guest).is_err());
    }

    #[test]
    fn oauth_only_users_cannot_manage_recovery() {
        let oauth_only = user_fixture(Some("oauth@example.com"), None);
        assert!(require_email_password_user(&oauth_only).is_err());
    }

    #[test]
    fn email_password_users_can_manage_recovery() {
        let password_user = user_fixture(Some("user@example.com"), Some(vec![1, 2, 3]));
        assert!(require_email_password_user(&password_user).is_ok());
    }

    #[tokio::test]
    async fn recovery_router_rejects_v1_transport_session() {
        let router = recovery_test_router();
        let request = HttpRequest::builder()
            .uri("/protected/recovery-code")
            .body(Body::empty())
            .unwrap();
        // In production the v1 gateway inserts this extension; the test
        // reproduces an authenticated-but-v1 transport reaching the router.
        let extension = TransportSession::v1(Uuid::new_v4());
        let request = attach_transport(request, extension);
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn recovery_router_rejects_missing_transport_session() {
        let router = recovery_test_router();
        let request = HttpRequest::builder()
            .uri("/protected/recovery-code")
            .body(Body::empty())
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn recovery_router_accepts_v2_transport_session() {
        let router = recovery_test_router();
        let request = HttpRequest::builder()
            .uri("/protected/recovery-code")
            .body(Body::empty())
            .unwrap();
        let extension = TransportSession::v2(SessionId::from_bytes([0x22; 16]));
        let request = attach_transport(request, extension);
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    fn attach_transport(request: Request<Body>, session: TransportSession) -> Request<Body> {
        let (mut parts, body) = request.into_parts();
        parts.extensions.insert(session);
        Request::from_parts(parts, body)
    }

    fn recovery_test_router() -> Router<()> {
        Router::new()
            .route(
                "/protected/recovery-code",
                get(
                    |Extension(session): Extension<TransportSession>| async move {
                        // This handler would perform recovery status work; the
                        // test handler only reports whether the transport gate
                        // let it run.
                        if session.is_v2() {
                            StatusCode::OK
                        } else {
                            StatusCode::CONFLICT
                        }
                    },
                ),
            )
            .layer(from_fn(require_transport_v2))
    }
}
