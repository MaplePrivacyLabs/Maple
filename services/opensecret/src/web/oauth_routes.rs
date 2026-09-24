use crate::apple_signin::{generate_apple_client_secret, validate_apple_native_token};
#[cfg(test)]
use crate::models::oauth::NewUserOAuthConnection;
use crate::models::oauth::UserOAuthConnection;
use crate::oauth::{BasicClient, OAuthCodeBinding, OAuthState};
use crate::web::encryption_middleware::{
    decrypt_request, encrypt_response, Decrypted, TransportSession,
};
use crate::web::login_routes::handle_new_user_registration;
use crate::web::platform::common::{
    PROJECT_APPLE_OAUTH_SECRET, PROJECT_GITHUB_OAUTH_SECRET, PROJECT_GOOGLE_OAUTH_SECRET,
};
use crate::GithubProvider;
use crate::GoogleProvider;
use crate::{decrypt_with_key, private_key::generate_twelve_word_seed};
use crate::{encrypt, DBError};
use crate::{
    jwt::{AuthContext, NewToken, TokenType},
    models::{
        project_settings::OAuthProviderSettings,
        users::{NewUser, User},
    },
    ApiError, AppState,
};
use axum::{
    extract::{Extension, State},
    response::Response,
    routing::post,
    Router,
};
use base64::Engine as _;
use oauth2::TokenResponse;
use oauth2::{
    basic::BasicTokenType, AuthorizationCode, EmptyExtraTokenFields, StandardTokenResponse,
};
use reqwest::header::AUTHORIZATION;
use secp256k1::SecretKey;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, error};
use uuid::Uuid;

pub fn router(app_state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/auth/github",
            post(|state, ext1, ext2| initiate_oauth(state, ext1, ext2, "github")).layer(
                axum::middleware::from_fn_with_state(
                    app_state.clone(),
                    decrypt_request::<OAuthAuthRequest>,
                ),
            ),
        )
        .route(
            "/auth/github/callback",
            post(|state, ext1, ext2| oauth_callback(state, ext1, ext2, "github")).layer(
                axum::middleware::from_fn_with_state(
                    app_state.clone(),
                    decrypt_request::<OAuthCallbackRequest>,
                ),
            ),
        )
        .route(
            "/auth/google",
            post(|state, ext1, ext2| initiate_oauth(state, ext1, ext2, "google")).layer(
                axum::middleware::from_fn_with_state(
                    app_state.clone(),
                    decrypt_request::<OAuthAuthRequest>,
                ),
            ),
        )
        .route(
            "/auth/google/callback",
            post(|state, ext1, ext2| oauth_callback(state, ext1, ext2, "google")).layer(
                axum::middleware::from_fn_with_state(
                    app_state.clone(),
                    decrypt_request::<OAuthCallbackRequest>,
                ),
            ),
        )
        .route(
            "/auth/apple",
            post(|state, ext1, ext2| initiate_oauth(state, ext1, ext2, "apple")).layer(
                axum::middleware::from_fn_with_state(
                    app_state.clone(),
                    decrypt_request::<OAuthAuthRequest>,
                ),
            ),
        )
        .route(
            "/auth/apple/callback",
            post(|state, ext1, ext2| oauth_callback(state, ext1, ext2, "apple")).layer(
                axum::middleware::from_fn_with_state(
                    app_state.clone(),
                    decrypt_request::<OAuthCallbackRequest>,
                ),
            ),
        )
        .route(
            "/auth/apple/native",
            post(handle_apple_native_signin).layer(axum::middleware::from_fn_with_state(
                app_state.clone(),
                decrypt_request::<AppleNativeSignInRequest>,
            )),
        )
        .with_state(app_state)
}

#[derive(Serialize)]
pub struct OAuthOAuthCallbackResponse {
    auth_url: String,
    state: String,
}

#[derive(Deserialize, Clone)]
pub struct OAuthAuthRequest {
    pub client_id: Uuid,
    #[serde(default)]
    pub redirect_url: Option<String>,
}

#[derive(Deserialize, Clone)]
pub struct OAuthCallbackRequest {
    pub code: String,
    pub state: String,
}

#[derive(Serialize)]
pub struct OAuthCallbackResponse {
    id: Uuid,
    email: String,
    access_token: String,
    refresh_token: String,
}

struct AuthenticatedOAuthUser {
    user: User,
    auth_context: AuthContext,
}

#[derive(Deserialize, Clone)]
pub struct AppleNativeSignInRequest {
    pub identity_token: String,          // JWT from Apple
    pub user_identifier: Option<String>, // Apple's unique user ID (sub) - Optional since we can extract from token
    pub email: Option<String>,           // Email (only provided on first sign-in)
    pub given_name: Option<String>,      // First name (only provided on first sign-in)
    pub family_name: Option<String>,     // Last name (only provided on first sign-in)
    pub client_id: Uuid,                 // Your app's client ID for the project
    pub nonce: Option<String>,           // Optional nonce for preventing replay attacks
}

#[derive(Deserialize, Clone, Debug)]
struct GithubUser {
    id: i64,
    login: String,
    name: Option<String>,
    email: Option<String>,
}

#[derive(Deserialize)]
struct GithubEmail {
    email: String,
    primary: bool,
    verified: bool,
}

#[derive(Deserialize, Clone, Debug)]
struct GoogleUser {
    sub: String,
    email: String,
    email_verified: bool,
    name: Option<String>,
}

#[derive(Deserialize, Clone, Debug)]
struct AppleUser {
    sub: String,
    email: Option<String>,
    email_verified: Option<bool>,
    name: Option<String>,
}

#[derive(Deserialize, Debug)]
struct AppleTokenResponse {
    access_token: String,
    #[allow(dead_code)]
    token_type: String,
    expires_in: u64,
    refresh_token: Option<String>,
    id_token: String,
}

impl AppleTokenResponse {
    // Convert to OAuth2 StandardTokenResponse
    fn to_standard_token(&self) -> StandardTokenResponse<EmptyExtraTokenFields, BasicTokenType> {
        // Create the base token
        let mut token = StandardTokenResponse::new(
            oauth2::AccessToken::new(self.access_token.clone()),
            BasicTokenType::Bearer,
            EmptyExtraTokenFields {},
        );

        // Add refresh token if available
        if let Some(ref rt) = self.refresh_token {
            token.set_refresh_token(Some(oauth2::RefreshToken::new(rt.clone())));
        }

        // Add expiration
        let expires = std::time::Duration::from_secs(self.expires_in);
        token.set_expires_in(Some(&expires));

        token
    }
}

enum OAuthRedirectSelection<'a> {
    Requested(Option<&'a str>),
    // Only construct after the callback state matches and consumes server-stored state.
    ValidatedState(&'a str),
}

fn select_oauth_redirect_url<'a>(
    settings: &'a OAuthProviderSettings,
    selection: OAuthRedirectSelection<'a>,
) -> Result<&'a str, ApiError> {
    match selection {
        OAuthRedirectSelection::Requested(None) => Ok(&settings.redirect_url),
        OAuthRedirectSelection::Requested(Some(requested)) => {
            if requested == settings.redirect_url
                || settings
                    .additional_redirect_urls
                    .as_ref()
                    .is_some_and(|urls| urls.iter().any(|url| url == requested))
            {
                Ok(requested)
            } else {
                Err(ApiError::BadRequest)
            }
        }
        // Configuration may change while the user authenticates. A validated
        // callback must exchange its code with the same URL used at initiation.
        OAuthRedirectSelection::ValidatedState(redirect_url) => Ok(redirect_url),
    }
}

fn apple_token_exchange_params<'a>(
    oauth_client: &'a BasicClient,
    client_id: &'a str,
    client_secret: &'a str,
    code: &'a str,
) -> Result<[(&'static str, &'a str); 5], ApiError> {
    let redirect_uri = oauth_client
        .redirect_uri()
        .ok_or(ApiError::InternalServerError)?
        .as_str();
    Ok([
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("code", code),
        ("grant_type", "authorization_code"),
        ("redirect_uri", redirect_uri),
    ])
}

async fn get_project_oauth_client(
    app_state: &AppState,
    project_id: i32,
    provider_name: &str,
    redirect_selection: OAuthRedirectSelection<'_>,
) -> Result<BasicClient, ApiError> {
    // Get project OAuth settings
    let oauth_settings = app_state
        .db
        .get_project_oauth_settings(project_id)?
        .ok_or_else(|| {
            error!("OAuth settings not found for project");
            ApiError::BadRequest
        })?;

    // Get provider settings based on provider name
    let (enabled, provider_settings, secret_key) = match provider_name {
        "github" => {
            let enabled = oauth_settings.github_oauth_enabled;
            let settings = oauth_settings.github_oauth_settings;
            let secret = app_state
                .db
                .get_org_project_secret_by_key_name_and_project(
                    PROJECT_GITHUB_OAUTH_SECRET,
                    project_id,
                )?;
            (enabled, settings, secret)
        }
        "google" => {
            let enabled = oauth_settings.google_oauth_enabled;
            let settings = oauth_settings.google_oauth_settings;
            let secret = app_state
                .db
                .get_org_project_secret_by_key_name_and_project(
                    PROJECT_GOOGLE_OAUTH_SECRET,
                    project_id,
                )?;
            (enabled, settings, secret)
        }
        "apple" => {
            let enabled = oauth_settings.apple_oauth_enabled;
            let mut apple_settings = oauth_settings.apple_oauth_settings.clone();

            // For web-based Apple OAuth flow, append .services to the client ID
            // This is because Apple requires Services ID for web flow, not App ID
            if let Some(ref mut settings) = apple_settings {
                if !settings.client_id.ends_with(".services") {
                    settings.client_id = format!("{}.services", settings.client_id);
                    debug!(
                        "Modified Apple client ID for OAuth client: {}",
                        settings.client_id
                    );
                }
            }

            let secret = app_state
                .db
                .get_org_project_secret_by_key_name_and_project(
                    PROJECT_APPLE_OAUTH_SECRET,
                    project_id,
                )?;

            // Convert AppleOAuthSettings to OAuthProviderSettings for the tuple
            let standard_settings = apple_settings.as_ref().map(|apple| OAuthProviderSettings {
                client_id: apple.client_id.clone(),
                redirect_url: apple.redirect_url.clone(),
                additional_redirect_urls: apple.additional_redirect_urls.clone(),
            });

            (enabled, standard_settings, secret)
        }
        _ => {
            error!("Unsupported OAuth provider: {}", provider_name);
            return Err(ApiError::BadRequest);
        }
    };

    // Verify OAuth is enabled for this provider
    if !enabled {
        error!("{} OAuth is not enabled for this project", provider_name);
        return Err(ApiError::BadRequest);
    }

    // Get provider settings
    let provider_settings = provider_settings.ok_or_else(|| {
        error!("{} OAuth settings not configured", provider_name);
        ApiError::BadRequest
    })?;

    // Reject unlisted requested URLs before decrypting credentials, generating
    // provider authorization parameters, or allocating one-time state.
    let redirect_url = select_oauth_redirect_url(&provider_settings, redirect_selection)?;

    // Get and decrypt client secret
    let secret = secret_key.ok_or_else(|| {
        error!("{} OAuth secret not found", provider_name);
        ApiError::BadRequest
    })?;

    let secret_key = SecretKey::from_slice(&app_state.enclave_key).map_err(|_| {
        error!("Failed to create secret key from enclave key");
        ApiError::InternalServerError
    })?;

    let client_secret = String::from_utf8(
        decrypt_with_key(&secret_key, &secret.secret_enc).map_err(|_| {
            error!("Failed to decrypt OAuth client secret");
            ApiError::InternalServerError
        })?,
    )
    .map_err(|_| {
        error!("Failed to parse decrypted OAuth client secret as UTF-8");
        ApiError::InternalServerError
    })?;

    // Get the OAuth provider
    let oauth_provider = app_state
        .oauth_manager
        .get_provider(provider_name)
        .ok_or_else(|| {
            error!("{} provider not initialized", provider_name);
            ApiError::InternalServerError
        })?;

    // For Apple, generate a JWT client secret instead of using the raw key
    if provider_name == "apple" {
        debug!("Generating Apple JWT client secret");

        // Get the Apple settings
        let apple_settings = oauth_settings
            .apple_oauth_settings
            .as_ref()
            .ok_or_else(|| {
                error!("Apple OAuth settings not configured");
                ApiError::BadRequest
            })?;

        // The client_secret is the private key in base64 - decode it first
        let private_key_pem = base64::engine::general_purpose::STANDARD
            .decode(&client_secret)
            .map_err(|_e| {
                error!("Failed to base64 decode the Apple private key");
                ApiError::InternalServerError
            })?;

        // Convert to UTF-8 string as it should be a PEM-encoded private key
        let private_key_pem_str = String::from_utf8(private_key_pem).map_err(|_e| {
            error!("Failed to convert Apple private key to string");
            ApiError::InternalServerError
        })?;

        // Make sure client_id for JWT includes .services suffix
        let client_id_with_services = if !apple_settings.client_id.ends_with(".services") {
            format!("{}.services", apple_settings.client_id)
        } else {
            apple_settings.client_id.clone()
        };

        // Check that Team ID and Key ID are configured
        let team_id = apple_settings.team_id.as_ref().ok_or_else(|| {
            error!("Apple OAuth Team ID is not configured");
            ApiError::BadRequest
        })?;

        let key_id = apple_settings.key_id.as_ref().ok_or_else(|| {
            error!("Apple OAuth Key ID is not configured");
            ApiError::BadRequest
        })?;

        // Generate the JWT client secret
        let client_secret_jwt = generate_apple_client_secret(
            team_id,
            key_id,
            &private_key_pem_str,
            &client_id_with_services,
        )
        .map_err(|_e| {
            error!("Failed to generate Apple client secret JWT");
            ApiError::InternalServerError
        })?;

        debug!("Building Apple OAuth client");

        // Use the same client ID for the OAuth client as in the JWT's sub claim
        let client_id_for_client = client_id_with_services.clone();

        // Build and return the client with the JWT as client_secret
        // Important: Use the same client ID with .services suffix for building the client
        oauth_provider
            .build_client(
                client_id_for_client, // Use the same client ID as in the JWT
                client_secret_jwt,
                redirect_url.to_string(),
            )
            .await
            .map_err(|_| {
                error!("Failed to build Apple OAuth client");
                ApiError::InternalServerError
            })
    } else {
        // For other providers, use the client_secret as-is
        oauth_provider
            .build_client(
                provider_settings.client_id.clone(),
                client_secret,
                redirect_url.to_string(),
            )
            .await
            .map_err(|_| ApiError::InternalServerError)
    }
}

pub async fn initiate_oauth(
    State(app_state): State<Arc<AppState>>,
    Decrypted(auth_request): Decrypted<OAuthAuthRequest>,
    Extension(session_id): Extension<TransportSession>,
    provider_name: &str,
) -> Result<Response, ApiError> {
    // Get project
    let project = app_state
        .db
        .get_org_project_by_client_id(auth_request.client_id)
        .map_err(|_| ApiError::BadRequest)?;

    // Get OAuth client for this project
    let oauth_client = get_project_oauth_client(
        &app_state,
        project.id,
        provider_name,
        OAuthRedirectSelection::Requested(auth_request.redirect_url.as_deref()),
    )
    .await?;

    // Get the OAuth provider
    let oauth_provider = app_state
        .oauth_manager
        .get_provider(provider_name)
        .ok_or_else(|| {
            error!("{} provider not initialized", provider_name);
            ApiError::InternalServerError
        })?;

    // V2 adds a provider-verified authorization-code binding. V1 deliberately
    // retains its existing authorization URL and state behavior.
    let v2_session_id = session_id.v2_session_id();
    let (initial_url, csrf_token, code_binding) = if v2_session_id.is_some() {
        let authorization = match provider_name {
            "github" => {
                oauth_provider
                    .as_github()
                    .ok_or(ApiError::InternalServerError)?
                    .generate_bound_authorize_url(&oauth_client)
                    .await
            }
            "google" => {
                oauth_provider
                    .as_google()
                    .ok_or(ApiError::InternalServerError)?
                    .generate_bound_authorize_url(&oauth_client)
                    .await
            }
            "apple" => {
                oauth_provider
                    .as_apple()
                    .ok_or(ApiError::InternalServerError)?
                    .generate_bound_authorize_url(&oauth_client)
                    .await
            }
            _ => return Err(ApiError::InternalServerError),
        };
        (
            authorization.auth_url,
            authorization.csrf_token,
            Some(authorization.code_binding),
        )
    } else {
        let (auth_url, csrf_token) = oauth_provider.generate_authorize_url(&oauth_client).await;
        (auth_url, csrf_token, None)
    };

    // Snapshot the actual callback even when the caller omitted redirect_url.
    // Callbacks must not pick up a subsequently changed project default.
    let state = OAuthState {
        csrf_token: csrf_token.secret().clone(),
        client_id: project.client_id,
        redirect_url: oauth_client
            .redirect_uri()
            .ok_or(ApiError::InternalServerError)?
            .as_str()
            .to_string(),
    };

    // Store the complete state in the provider
    match (v2_session_id, code_binding) {
        (Some(session_id), Some(code_binding)) => {
            oauth_provider
                .store_state_for_session(
                    csrf_token.secret(),
                    state.clone(),
                    session_id,
                    code_binding,
                )
                .await
        }
        (None, None) => {
            oauth_provider
                .store_state(csrf_token.secret(), state.clone())
                .await
        }
        _ => return Err(ApiError::InternalServerError),
    }
    .map_err(|_| ApiError::TooManyRequests)?;

    let state_json = serde_json::to_string(&state).map_err(|_| ApiError::InternalServerError)?;
    let state_base64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(state_json);

    // Replace the CSRF token in the URL with our encoded state
    let auth_url = initial_url.replace(csrf_token.secret(), &state_base64);

    let response = OAuthOAuthCallbackResponse {
        auth_url,
        state: state_base64,
    };
    encrypt_response(&app_state, &session_id, &response).await
}

pub async fn oauth_callback(
    State(app_state): State<Arc<AppState>>,
    Decrypted(callback_request): Decrypted<OAuthCallbackRequest>,
    Extension(session_id): Extension<TransportSession>,
    provider_name: &str,
) -> Result<Response, ApiError> {
    debug!(
        "Received code (redacted, len={})",
        callback_request.code.len()
    );
    debug!(
        "Received OAuth state (redacted, len={})",
        callback_request.state.len()
    );

    // Decode and parse the state
    let state_json = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(&callback_request.state)
        .map_err(|_| {
            error!("Could not decode OAuth state");
            ApiError::BadRequest
        })?;
    debug!("Parsed state from request");
    let state: OAuthState = serde_json::from_slice(&state_json).map_err(|_| {
        error!("Could not parse OAuth state");
        ApiError::BadRequest
    })?;
    debug!("Converted state to OAuthState");

    // Get the OAuth provider
    let oauth_provider = app_state
        .oauth_manager
        .get_provider(provider_name)
        .ok_or_else(|| {
            error!("{} provider not initialized", provider_name);
            ApiError::InternalServerError
        })?;

    // Validate and atomically consume the complete state so a successful match is one-time.
    // A mismatched state is left in the store for the legitimate callback.
    let is_transport_v2 = session_id.is_v2();
    let code_binding = match session_id.v2_session_id() {
        Some(session_id) => oauth_provider
            .consume_state_for_session(&state, session_id)
            .await
            .map(Some),
        None => oauth_provider.consume_state(&state).await.then_some(None),
    };

    let Some(code_binding) = code_binding else {
        error!("Invalid state in {} callback", provider_name);
        return Err(ApiError::BadRequest);
    };

    let (pkce_verifier, apple_nonce) = match (is_transport_v2, provider_name, code_binding) {
        (false, _, None) => (None, None),
        (true, "github" | "google", Some(OAuthCodeBinding::Pkce(verifier))) => {
            (Some(verifier), None)
        }
        (true, "apple", Some(OAuthCodeBinding::AppleNonce(nonce))) => (None, Some(nonce)),
        _ => return Err(ApiError::InternalServerError),
    };

    // Get project (we can trust the client_id now since we validated it against our stored state)
    debug!("Getting project from client_id: {:?}", state.client_id);
    let project = app_state
        .db
        .get_org_project_by_client_id(state.client_id)
        .map_err(|e| {
            error!(
                "Could not get project by client {:?} id: {:?}",
                state.client_id, e
            );
            ApiError::BadRequest
        })?;

    // Get OAuth client for this project
    let oauth_client = get_project_oauth_client(
        &app_state,
        project.id,
        provider_name,
        OAuthRedirectSelection::ValidatedState(&state.redirect_url),
    )
    .await?;

    // Exchange the code for an access token
    debug!(
        "Exchanging authorization code for access token with provider: {}",
        provider_name
    );

    // For Apple, add extra debugging for token exchange
    // Return both the token and the ID token for Apple
    let (token, apple_id_token): (
        StandardTokenResponse<EmptyExtraTokenFields, BasicTokenType>,
        Option<String>,
    ) = if provider_name == "apple" {
        debug!("Setting up Apple token exchange");

        // Perform the token exchange
        let token_url = "https://appleid.apple.com/auth/token";

        // Let's try a custom approach matching your successful Postman request
        let client = crate::http_client::client();

        // Get the parameters from the OAuth client
        let client_id = oauth_client.client_id().as_str();

        // We need to regenerate the JWT here
        // First get the project OAuth settings again
        let oauth_settings = app_state
            .db
            .get_project_oauth_settings(project.id)?
            .ok_or_else(|| {
                error!("OAuth settings not found for project");
                ApiError::BadRequest
            })?;

        // Then get the Apple-specific settings
        let apple_settings = oauth_settings
            .apple_oauth_settings
            .as_ref()
            .ok_or_else(|| {
                error!("Apple OAuth settings not configured");
                ApiError::BadRequest
            })?;

        // Decrypt the private key again
        let secret = app_state
            .db
            .get_org_project_secret_by_key_name_and_project(PROJECT_APPLE_OAUTH_SECRET, project.id)?
            .ok_or_else(|| {
                error!("Apple OAuth secret not found");
                ApiError::BadRequest
            })?;

        let secret_key = SecretKey::from_slice(&app_state.enclave_key).map_err(|_| {
            error!("Failed to create secret key from enclave key");
            ApiError::InternalServerError
        })?;

        let client_secret_raw = String::from_utf8(
            decrypt_with_key(&secret_key, &secret.secret_enc).map_err(|_| {
                error!("Failed to decrypt OAuth client secret");
                ApiError::InternalServerError
            })?,
        )
        .map_err(|_| {
            error!("Failed to parse decrypted OAuth client secret as UTF-8");
            ApiError::InternalServerError
        })?;

        // The client_secret is the private key in base64 - decode it first
        let private_key_pem = base64::engine::general_purpose::STANDARD
            .decode(&client_secret_raw)
            .map_err(|_e| {
                error!("Failed to base64 decode the Apple private key");
                ApiError::InternalServerError
            })?;

        // Convert to UTF-8 string as it should be a PEM-encoded private key
        let private_key_pem_str = String::from_utf8(private_key_pem).map_err(|_e| {
            error!("Failed to convert Apple private key to string");
            ApiError::InternalServerError
        })?;

        // Make sure client_id for JWT includes .services suffix
        let client_id_with_services = if !apple_settings.client_id.ends_with(".services") {
            format!("{}.services", apple_settings.client_id)
        } else {
            apple_settings.client_id.clone()
        };

        // Check that Team ID and Key ID are configured for token exchange
        let team_id = apple_settings.team_id.as_ref().ok_or_else(|| {
            error!("Apple OAuth Team ID is not configured for token exchange");
            ApiError::BadRequest
        })?;

        let key_id = apple_settings.key_id.as_ref().ok_or_else(|| {
            error!("Apple OAuth Key ID is not configured for token exchange");
            ApiError::BadRequest
        })?;

        // Generate the JWT client secret
        let client_secret = generate_apple_client_secret(
            team_id,
            key_id,
            &private_key_pem_str,
            &client_id_with_services,
        )
        .map_err(|_e| {
            error!("Failed to generate Apple client secret JWT");
            ApiError::InternalServerError
        })?;

        // Make sure the client_id parameter matches what's in the JWT's sub claim
        // Apple requires these to be exactly the same
        // - client_id parameter must match JWT's sub claim exactly
        let client_id_param = if apple_settings.client_id.ends_with(".services") {
            // Settings already have .services suffix
            client_id
        } else {
            // Use the same value with .services suffix that we used in the JWT
            client_id_with_services.as_str()
        };

        // Build the form data for the token request
        let params = apple_token_exchange_params(
            &oauth_client,
            client_id_param,
            &client_secret,
            &callback_request.code,
        )?;

        // Create the request
        let request = client
            .post(token_url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .form(&params);

        // Send the request
        let response = request.send().await.map_err(|e| {
            error!("HTTP request failed: {:?}", e);
            ApiError::InternalServerError
        })?;

        // Check the response
        let status = response.status();

        // Check if successful
        if !status.is_success() {
            error!(
                "Apple OAuth token exchange returned non-success status: {}",
                status
            );

            return Err(ApiError::InternalServerError);
        }

        // Parse the successful response
        let token_json = response.text().await.map_err(|e| {
            error!("Failed to read response body: {:?}", e);
            ApiError::InternalServerError
        })?;

        // Parse the JSON using our custom AppleTokenResponse struct
        let apple_token: AppleTokenResponse = serde_json::from_str(&token_json).map_err(|e| {
            error!("Failed to parse Apple token response: {:?}", e);
            ApiError::InternalServerError
        })?;

        debug!("Successfully parsed Apple token response");

        // Convert AppleTokenResponse to StandardTokenResponse
        let token = apple_token.to_standard_token();

        // Extract id_token for return
        let id_token = apple_token.id_token;

        // Return both the token and the ID token
        (token, Some(id_token))
    } else {
        // For other providers, use the standard OAuth client
        let http_client = oauth2::reqwest::ClientBuilder::new()
            // OAuth token endpoints must not follow redirects: doing so could
            // send client credentials to an attacker-controlled destination.
            .redirect(oauth2::reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| {
                error!("Failed to build OAuth HTTP client: {:?}", e);
                ApiError::InternalServerError
            })?;
        let exchange =
            oauth_client.exchange_code(AuthorizationCode::new(callback_request.code.clone()));
        let exchange = if let Some(verifier) = pkce_verifier {
            exchange.set_pkce_verifier(verifier)
        } else {
            exchange
        };
        let token = exchange.request_async(&http_client).await.map_err(|e| {
            error!("Failed to exchange code for access token: {:?}", e);
            ApiError::InternalServerError
        })?;

        // No ID token for non-Apple providers
        (token, None)
    };

    debug!("Successfully obtained {} token", provider_name);

    let access_token = token.access_token().secret().to_string();

    // Fetch user information and find or create the user
    let authenticated_user = match provider_name {
        "github" => {
            debug!("Access token obtained, fetching GitHub user");
            // Get GitHub provider for GitHub-specific operations
            let github_provider = oauth_provider.as_github().ok_or_else(|| {
                error!("Failed to get GithubProvider");
                ApiError::InternalServerError
            })?;
            let github_user = match fetch_github_user(&access_token, github_provider).await {
                Ok(user) => {
                    debug!("Successfully fetched GitHub user");
                    user
                }
                Err(e) => {
                    error!("Failed to fetch GitHub user: {:?}", e);
                    return Err(e);
                }
            };

            find_or_create_user_from_oauth(
                &app_state,
                github_user.email.clone(),
                github_user.id.to_string(),
                "github",
                access_token,
                github_user.name.clone().or(Some(github_user.login.clone())),
                project.id,
            )
            .await?
        }
        "google" => {
            debug!("Access token obtained, fetching Google user");
            // Get Google provider for Google-specific operations
            let google_provider = oauth_provider.as_google().ok_or_else(|| {
                error!("Failed to get GoogleProvider");
                ApiError::InternalServerError
            })?;
            let google_user = match fetch_google_user(&access_token, google_provider).await {
                Ok(user) => {
                    debug!("Successfully fetched Google user");
                    user
                }
                Err(e) => {
                    error!("Failed to fetch Google user: {:?}", e);
                    return Err(e);
                }
            };

            find_or_create_user_from_oauth(
                &app_state,
                Some(google_user.email.clone()),
                google_user.sub.clone(),
                "google",
                access_token,
                google_user.name.clone(),
                project.id,
            )
            .await?
        }
        "apple" => {
            debug!("Access token obtained, processing Apple ID token");

            // Verify we have a valid Apple provider
            let _apple_provider = oauth_provider.as_apple().ok_or_else(|| {
                error!("Failed to get AppleProvider");
                ApiError::InternalServerError
            })?;

            // Get project OAuth settings first
            let oauth_settings = app_state
                .db
                .get_project_oauth_settings(project.id)?
                .ok_or_else(|| {
                    error!("OAuth settings not found for project");
                    ApiError::BadRequest
                })?;

            // Get Apple client ID from project OAuth settings
            let mut client_id = oauth_settings
                .apple_oauth_settings
                .ok_or_else(|| {
                    error!("Apple OAuth settings not configured");
                    ApiError::BadRequest
                })?
                .client_id;

            // For web-based OAuth flow, append .services to the client ID
            // This is because Apple requires Services ID for web flow, not App ID
            if !client_id.ends_with(".services") {
                client_id = format!("{}.services", client_id);
                debug!("Modified Apple client ID for web flow: {}", client_id);
            }

            // For Apple, we need to extract the ID token from the token exchange response
            debug!("Processing Apple OAuth token response to get ID token");

            // Get the ID token that was returned directly from the token exchange
            // Use unwrap_or_default instead of a match since we know it exists
            let id_token = apple_id_token.ok_or_else(|| {
                error!("Missing Apple ID token that should have been returned from token exchange");
                ApiError::InternalServerError
            })?;

            // Verify the token just like we do for native sign-in
            let apple_user = match fetch_apple_user(
                &app_state,
                &id_token,
                &client_id,
                apple_nonce.as_ref().map(|nonce| nonce.as_str()),
            )
            .await
            {
                Ok(user) => {
                    debug!("Successfully verified Apple ID token");
                    user
                }
                Err(e) => {
                    error!("Failed to verify Apple ID token: {:?}", e);
                    return Err(e);
                }
            };

            let sub = apple_user.sub.clone();
            // For Apple web flow, use the refresh token if available, otherwise empty string
            let token_to_store = token
                .refresh_token()
                .map(|rt| rt.secret().to_string())
                .unwrap_or_else(|| "".to_string());

            find_or_create_user_from_oauth(
                &app_state,
                apple_user.email.clone(),
                sub,
                "apple",
                token_to_store,
                apple_user.name.clone(),
                project.id,
            )
            .await?
        }
        _ => {
            error!("Unsupported provider: {}", provider_name);
            return Err(ApiError::InternalServerError);
        }
    };

    let auth_response =
        oauth_callback_response(&app_state, &authenticated_user, session_id.is_v2())?;
    encrypt_response(&app_state, &session_id, &auth_response).await
}

async fn fetch_github_user(
    access_token: &str,
    github_provider: &GithubProvider,
) -> Result<GithubUser, ApiError> {
    let client = crate::http_client::client();
    let user_url = &github_provider.user_info_url;

    debug!("Sending request to GitHub API: {}", user_url);
    let response = client
        .get(user_url)
        .header("Authorization", format!("token {}", access_token))
        .header("User-Agent", "OpenSecret")
        .send()
        .await
        .map_err(|e| {
            error!("Failed to send request to GitHub API: {:?}", e);
            ApiError::InternalServerError
        })?;

    let status = response.status();
    debug!("GitHub API response status: {}", status);

    if !status.is_success() {
        error!(
            "GitHub API returned non-success status: {} {}",
            status,
            status.canonical_reason().unwrap_or("")
        );
        return Err(ApiError::InternalServerError);
    }

    let user_body = response.text().await.map_err(|e| {
        error!("Failed to read GitHub user response body: {:?}", e);
        ApiError::InternalServerError
    })?;

    let mut github_user: GithubUser = serde_json::from_str(&user_body).map_err(|e| {
        error!(
            "Failed to parse GitHub user JSON: {} (response_bytes={})",
            e,
            user_body.len()
        );
        ApiError::InternalServerError
    })?;

    // If the email is not public, fetch the email separately
    if github_user.email.is_none() {
        let emails_url = "https://api.github.com/user/emails";
        debug!("Fetching GitHub user emails: {}", emails_url);
        let emails_response = client
            .get(emails_url)
            .header("Authorization", format!("token {}", access_token))
            .header("User-Agent", "OpenSecret")
            .send()
            .await
            .map_err(|e| {
                error!("Failed to send request for GitHub user emails: {:?}", e);
                ApiError::InternalServerError
            })?;

        let emails_status = emails_response.status();

        if !emails_status.is_success() {
            error!(
                "GitHub API returned non-success status for emails: {} {}",
                emails_status,
                emails_status.canonical_reason().unwrap_or("")
            );
            return Err(ApiError::InternalServerError);
        }

        let emails_body = emails_response.text().await.map_err(|e| {
            error!("Failed to read GitHub emails response body: {:?}", e);
            ApiError::InternalServerError
        })?;

        let emails: Vec<GithubEmail> = serde_json::from_str(&emails_body).map_err(|e| {
            error!(
                "Failed to parse GitHub emails JSON: {} (response_bytes={})",
                e,
                emails_body.len()
            );
            ApiError::InternalServerError
        })?;

        github_user.email = emails
            .into_iter()
            .find(|e| e.primary && e.verified)
            .map(|e| e.email);
    }

    // If we still don't have an email, return an error
    if github_user.email.is_none() {
        error!("No valid email found for GitHub user");
        return Err(ApiError::NoEmailFound);
    }

    Ok(github_user)
}

async fn fetch_google_user(
    access_token: &str,
    google_provider: &GoogleProvider,
) -> Result<GoogleUser, ApiError> {
    let client = crate::http_client::client();
    let user_url = &google_provider.user_info_url;

    debug!("Sending request to Google API: {}", user_url);
    let response = client
        .get(user_url)
        .header(AUTHORIZATION, format!("Bearer {}", access_token))
        .send()
        .await
        .map_err(|e| {
            error!("Failed to send request to Google API: {:?}", e);
            ApiError::InternalServerError
        })?;

    let status = response.status();
    if !status.is_success() {
        error!("Google API returned non-success status: {}", status);
        return Err(ApiError::InternalServerError);
    }

    let google_user: GoogleUser = response.json().await.map_err(|e| {
        error!("Failed to parse Google user JSON: {:?}", e);
        ApiError::InternalServerError
    })?;

    // Ensure email is present and verified
    if google_user.email.is_empty() || !google_user.email_verified {
        error!("Google user email is not present or not verified");
        return Err(ApiError::BadRequest);
    }

    Ok(google_user)
}

async fn fetch_apple_user(
    app_state: &AppState,
    id_token: &str,
    client_id: &str,
    expected_nonce: Option<&str>,
) -> Result<AppleUser, ApiError> {
    debug!("Parsing and validating Apple ID token");

    // V2 requires the provider-signed ID token to carry the nonce generated
    // for this exact state and session. V1 continues without nonce validation.
    let claims = validate_apple_native_token(
        &app_state.apple_jwt_verifier,
        id_token,
        client_id,
        expected_nonce,
    )
    .await?;

    // Create an AppleUser from the claims
    let apple_user = AppleUser {
        sub: claims.sub,
        email: claims.email,
        email_verified: claims.email_verified,
        name: None, // Apple doesn't include name in JWT, name comes from frontend
    };

    // Check if we have an email and it's verified
    if let Some(email) = &apple_user.email {
        if !email.is_empty() && apple_user.email_verified.unwrap_or(false) {
            return Ok(apple_user);
        }
    }

    // If we reach here, we either don't have an email or it's not verified
    error!("Apple user email is not present or not verified");
    Err(ApiError::NoEmailFound)
}

fn authenticated_oauth_user(
    app_state: &AppState,
    user: User,
    provider_name: &str,
    provider_user_id: &str,
) -> Result<AuthenticatedOAuthUser, ApiError> {
    let auth_context = app_state
        .oauth_auth_context_for_user(&user, provider_name, provider_user_id)
        .map_err(|e| {
            error!("Failed to compute OAuth auth context: {:?}", e);
            ApiError::InternalServerError
        })?;

    app_state
        .verify_seed_wrap_for_auth_context(&user, &auth_context)
        .map_err(|e| {
            error!("OAuth seed wrap verification failed: {:?}", e);
            ApiError::Unauthorized
        })?;

    Ok(AuthenticatedOAuthUser { user, auth_context })
}

fn oauth_callback_response(
    app_state: &AppState,
    authenticated_user: &AuthenticatedOAuthUser,
    is_transport_v2: bool,
) -> Result<OAuthCallbackResponse, ApiError> {
    let access_token = NewToken::new_with_auth_context(
        &authenticated_user.user,
        TokenType::access_for_transport(is_transport_v2),
        app_state,
        &authenticated_user.auth_context,
    )
    .map_err(|e| {
        error!("Failed to generate access token: {:?}", e);
        ApiError::InternalServerError
    })?;
    let refresh_token = NewToken::new_with_auth_context(
        &authenticated_user.user,
        TokenType::refresh_for_transport(is_transport_v2),
        app_state,
        &authenticated_user.auth_context,
    )
    .map_err(|e| {
        error!("Failed to generate refresh token: {:?}", e);
        ApiError::InternalServerError
    })?;

    Ok(OAuthCallbackResponse {
        id: authenticated_user.user.get_id(),
        email: authenticated_user
            .user
            .get_email()
            .expect("OAuth user must have email")
            .to_string(),
        access_token: access_token.token,
        refresh_token: refresh_token.token,
    })
}

async fn find_or_create_user_from_oauth(
    app_state: &AppState,
    email: Option<String>,
    provider_user_id: String,
    provider_name: &str,
    access_token: String,
    user_name: Option<String>,
    project_id: i32,
) -> Result<AuthenticatedOAuthUser, ApiError> {
    let provider = app_state
        .db
        .get_oauth_provider_by_name(provider_name)
        .map_err(|e| {
            error!("Failed to get {} OAuth provider: {:?}", provider_name, e);
            ApiError::InternalServerError
        })?
        .ok_or_else(|| {
            error!("{} OAuth provider not found", provider_name);
            ApiError::InternalServerError
        })?;

    if let Some(existing_connection) = app_state
        .db
        .get_project_user_oauth_connection_by_provider_subject(
            provider.id,
            &provider_user_id,
            project_id,
        )
        .map_err(|e| {
            error!(
                "Failed to get OAuth connection for verified provider subject: {:?}",
                e
            );
            ApiError::InternalServerError
        })?
    {
        let user = app_state.db.get_user_by_uuid(existing_connection.user_id)?;
        if user.project_id != project_id {
            error!("OAuth connection user belongs to a different project");
            return Err(ApiError::Unauthorized);
        }

        update_provider_connection(app_state, &existing_connection, &access_token).await?;
        return authenticated_oauth_user(app_state, user, provider_name, &provider_user_id);
    }

    let email = email
        .filter(|email| !email.trim().is_empty())
        .ok_or_else(|| {
            error!("No valid email found for new OAuth user");
            ApiError::NoEmailFound
        })?;

    match app_state.db.get_user_by_email(email.clone(), project_id) {
        Ok(existing_user) => {
            let existing_connection = app_state
                .db
                .get_user_oauth_connection_by_user_and_provider(existing_user.uuid, provider.id)
                .map_err(|e| {
                    error!("Failed to get existing OAuth connection: {:?}", e);
                    ApiError::InternalServerError
                })?;

            if existing_connection.is_some() {
                error!(
                    "Existing user has a {} connection, but not for the verified provider subject",
                    provider_name
                );
                Err(ApiError::UserExistsNotLinked)
            } else {
                error!("User exists but hasn't linked {} before", provider_name);
                Err(ApiError::UserExistsNotLinked)
            }
        }
        Err(DBError::UserNotFound) => {
            // Generate private key for new user
            let user_seed_words =
                generate_twelve_word_seed(app_state.aws_credential_manager.clone())
                    .await
                    .map_err(|_e| ApiError::InternalServerError)?
                    .to_string();

            // Create new user
            let new_user = NewUser::new(Some(email), None, project_id)
                .with_name(user_name.unwrap_or_default());

            let encrypted_access_token = if !access_token.is_empty() {
                encrypt_access_token(app_state, &access_token).await?
            } else {
                Vec::new()
            };

            let user = app_state
                .create_user_with_oauth_seed_wrap(
                    new_user,
                    provider.id,
                    provider_name,
                    &provider_user_id,
                    encrypted_access_token,
                    user_seed_words.as_bytes(),
                )
                .map_err(|e| {
                    error!("Failed to create new OAuth user and seed wrap: {:?}", e);
                    ApiError::InternalServerError
                })?;

            // Handle new user registration
            handle_new_user_registration(app_state, &user, false).await?;

            authenticated_oauth_user(app_state, user, provider_name, &provider_user_id)
        }
        Err(e) => {
            error!("Database error when fetching user: {:?}", e);
            Err(ApiError::InternalServerError)
        }
    }
}

/// Handler for Apple native sign-in (iOS Sign in with Apple)
pub async fn handle_apple_native_signin(
    State(app_state): State<Arc<AppState>>,
    Decrypted(request): Decrypted<AppleNativeSignInRequest>,
    Extension(session_id): Extension<TransportSession>,
) -> Result<Response, ApiError> {
    debug!("Handling Apple native sign-in");

    // Get project
    let project = app_state
        .db
        .get_org_project_by_client_id(request.client_id)
        .map_err(|_| ApiError::BadRequest)?;

    // Get project OAuth settings
    let oauth_settings = app_state
        .db
        .get_project_oauth_settings(project.id)?
        .ok_or_else(|| {
            error!("OAuth settings not found for project");
            ApiError::BadRequest
        })?;

    // Verify Apple OAuth is enabled
    if !oauth_settings.apple_oauth_enabled {
        error!("Apple OAuth is not enabled for this project");
        return Err(ApiError::BadRequest);
    }

    // Get Apple client ID from project settings
    let apple_oauth_settings = oauth_settings.apple_oauth_settings.ok_or_else(|| {
        error!("Apple OAuth settings not configured");
        ApiError::BadRequest
    })?;

    // Verify the Apple JWT token using the shared verifier with nonce if provided
    debug!("Verifying Apple identity token");
    let claims = app_state
        .apple_jwt_verifier
        .verify_token_for_audiences(
            &request.identity_token,
            &apple_oauth_settings.native_client_ids(),
            request.nonce.as_deref(),
        )
        .await?;

    // If user_identifier is provided, verify it matches the sub from the token
    if let Some(user_id) = &request.user_identifier {
        if *user_id != claims.sub {
            error!("User identifier mismatch in Apple token");
            return Err(ApiError::BadRequest);
        }
    }

    // Use the sub claim from the token as the verified user identifier
    let verified_user_id = claims.sub.clone();

    // Get the Apple provider from the database
    let apple_provider = app_state
        .db
        .get_oauth_provider_by_name("apple")
        .map_err(|e| {
            error!("Failed to get Apple OAuth provider: {:?}", e);
            ApiError::InternalServerError
        })?
        .ok_or_else(|| {
            error!("Apple OAuth provider not found");
            ApiError::InternalServerError
        })?;

    // For Apple native flow, we don't need to store any tokens
    // The iOS device handles authentication and token management
    let access_token = "".to_string(); // Empty string instead of storing the ID token

    if let Some(connection) = app_state
        .db
        .get_project_user_oauth_connection_by_provider_subject(
            apple_provider.id,
            &verified_user_id,
            project.id,
        )
        .map_err(|e| {
            error!(
                "Failed to get Apple OAuth connection for verified subject: {:?}",
                e
            );
            ApiError::InternalServerError
        })?
    {
        debug!("Found existing Apple OAuth connection");

        let user = app_state.db.get_user_by_uuid(connection.user_id)?;
        if user.project_id != project.id {
            error!("Apple OAuth connection user belongs to a different project");
            return Err(ApiError::Unauthorized);
        }

        update_provider_connection(&app_state, &connection, &access_token).await?;

        let authenticated_user =
            authenticated_oauth_user(&app_state, user, "apple", &verified_user_id)?;
        let auth_response =
            oauth_callback_response(&app_state, &authenticated_user, session_id.is_v2())?;

        debug!("Apple sign-in successful for existing user");
        return encrypt_response(&app_state, &session_id, &auth_response).await;
    }

    // If we get here, user doesn't exist - need to create new user
    debug!("No existing Apple OAuth connection; creating new user");

    // For new users, we absolutely need an email
    // Determine the email to use - prioritize the one from the token if available
    // Make sure we actually have an email - don't allow empty strings
    let email = claims
        .email
        .or(request.email.clone())
        .filter(|e| !e.is_empty())
        .ok_or_else(|| {
            error!("No valid email found in Apple token or request for new user");
            ApiError::NoEmailFound
        })?;

    // Construct a name from given_name and family_name if provided
    // and ensure it's None if empty or just whitespace
    let user_name = match (request.given_name.clone(), request.family_name.clone()) {
        (Some(given), Some(family)) => {
            let combined = format!("{} {}", given.trim(), family.trim())
                .trim()
                .to_string();
            if combined.is_empty() {
                None
            } else {
                Some(combined)
            }
        }
        (Some(given), None) => {
            let trimmed = given.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        }
        (None, Some(family)) => {
            let trimmed = family.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        }
        (None, None) => None,
    };

    // Create the new user
    let authenticated_user = find_or_create_user_from_oauth(
        &app_state,
        Some(email),
        verified_user_id,
        "apple",
        access_token,
        user_name,
        project.id,
    )
    .await?;

    let auth_response =
        oauth_callback_response(&app_state, &authenticated_user, session_id.is_v2())?;

    debug!("Apple native sign-in successful for new user");
    encrypt_response(&app_state, &session_id, &auth_response).await
}

async fn update_provider_connection(
    app_state: &AppState,
    connection: &UserOAuthConnection,
    token: &str,
) -> Result<(), ApiError> {
    let mut connection = connection.clone();

    // Only encrypt and update token if it's not empty
    if !token.is_empty() {
        let encrypted_token = encrypt_access_token(app_state, token).await?;
        connection.access_token_enc = encrypted_token;
    }

    app_state
        .db
        .update_user_oauth_connection(&connection)
        .map_err(|e| {
            error!("Failed to update OAuth connection: {:?}", e);
            ApiError::InternalServerError
        })?;

    Ok(())
}

#[cfg(test)]
async fn create_provider_connection(
    app_state: &AppState,
    user: &User,
    provider_id: i32,
    provider_user_id: String,
    access_token: &str,
) -> Result<(), ApiError> {
    // Only encrypt/store the token if it's not empty
    let encrypted_token = if !access_token.is_empty() {
        Some(encrypt_access_token(app_state, access_token).await?)
    } else {
        None
    };

    let new_connection = NewUserOAuthConnection {
        user_id: user.uuid,
        provider_id,
        provider_user_id,
        access_token_enc: encrypted_token.unwrap_or_default(),
        refresh_token_enc: None,
        expires_at: None,
    };

    app_state
        .db
        .create_user_oauth_connection(new_connection)
        .map_err(|e| {
            error!("Failed to create new OAuth connection: {:?}", e);
            ApiError::InternalServerError
        })?;

    Ok(())
}

async fn encrypt_access_token(
    app_state: &AppState,
    access_token: &str,
) -> Result<Vec<u8>, ApiError> {
    let secret_key = SecretKey::from_slice(&app_state.enclave_key).map_err(|e| {
        error!("Failed to create SecretKey from enclave key: {:?}", e);
        ApiError::InternalServerError
    })?;
    Ok(encrypt::encrypt_with_key(&secret_key, access_token.as_bytes()).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        db::setup_db,
        models::{org_projects::OrgProject, schema::org_projects},
        seed_wrapping::CredentialKind,
        AppMode, AppStateBuilder,
    };
    use diesel::{ExpressionMethods, QueryDsl, RunQueryDsl};
    use tokio::sync::RwLock;

    fn redirect_settings() -> OAuthProviderSettings {
        OAuthProviderSettings {
            client_id: "provider-client".to_string(),
            redirect_url: "https://app.example/auth/callback".to_string(),
            additional_redirect_urls: Some(vec![
                "https://auth.example/auth/callback".to_string(),
                "http://127.0.0.1:3000/auth/callback".to_string(),
            ]),
        }
    }

    #[test]
    fn oauth_initiate_request_keeps_old_payloads_compatible() {
        let client_id = Uuid::from_u128(1);
        for payload in [
            serde_json::json!({ "client_id": client_id }),
            serde_json::json!({ "client_id": client_id, "redirect_url": null }),
        ] {
            let request: OAuthAuthRequest = serde_json::from_value(payload).unwrap();
            assert_eq!(request.client_id, client_id);
            assert!(request.redirect_url.is_none());
        }

        let request: OAuthAuthRequest = serde_json::from_value(serde_json::json!({
            "client_id": client_id,
            "redirect_url": "https://auth.example/auth/callback",
        }))
        .unwrap();
        assert_eq!(
            request.redirect_url.as_deref(),
            Some("https://auth.example/auth/callback")
        );
        assert!(
            serde_json::from_value::<OAuthAuthRequest>(serde_json::json!({
                "client_id": client_id,
                "redirect_url": ["https://auth.example/auth/callback"],
            }))
            .is_err()
        );
    }

    #[test]
    fn oauth_redirect_selection_uses_default_or_an_exact_allowlist_match() {
        let settings = redirect_settings();
        assert_eq!(
            select_oauth_redirect_url(&settings, OAuthRedirectSelection::Requested(None)).unwrap(),
            settings.redirect_url
        );
        for requested in [
            settings.redirect_url.as_str(),
            "https://auth.example/auth/callback",
            "http://127.0.0.1:3000/auth/callback",
        ] {
            assert_eq!(
                select_oauth_redirect_url(
                    &settings,
                    OAuthRedirectSelection::Requested(Some(requested)),
                )
                .unwrap(),
                requested
            );
        }
        for unlisted in [
            "https://other.example/auth/callback",
            "https://auth.example/auth/other",
            "https://auth.example/auth/callback/",
            "https://auth.example/auth/callback?extra=1",
            "https://auth.example/auth/callback#fragment",
            "https://auth.example:443/auth/callback",
            "https://AUTH.example/auth/callback",
            "https://auth.example/auth/%63allback",
            "https://auth.example.evil.example/auth/callback",
            "",
        ] {
            assert!(matches!(
                select_oauth_redirect_url(
                    &settings,
                    OAuthRedirectSelection::Requested(Some(unlisted)),
                ),
                Err(ApiError::BadRequest)
            ));
        }
    }

    #[test]
    fn validated_callback_keeps_selected_url_after_configuration_changes() {
        let mut settings = redirect_settings();
        for request in [None, Some("https://auth.example/auth/callback")] {
            let selected =
                select_oauth_redirect_url(&settings, OAuthRedirectSelection::Requested(request))
                    .unwrap()
                    .to_string();
            settings.redirect_url = "https://new-app.example/auth/callback".to_string();
            settings.additional_redirect_urls = None;
            assert_eq!(
                select_oauth_redirect_url(
                    &settings,
                    OAuthRedirectSelection::ValidatedState(&selected),
                )
                .unwrap(),
                selected
            );
            assert!(matches!(
                select_oauth_redirect_url(
                    &settings,
                    OAuthRedirectSelection::Requested(Some(&selected)),
                ),
                Err(ApiError::BadRequest)
            ));
            settings = redirect_settings();
        }
    }

    #[test]
    fn apple_token_exchange_uses_the_callback_clients_recorded_redirect() {
        let selected = "https://auth.example/auth/apple/callback";
        let client = oauth2::basic::BasicClient::new(oauth2::ClientId::new(
            "apple-client.services".to_string(),
        ))
        .set_auth_uri(oauth2::AuthUrl::new("https://apple.example/authorize".to_string()).unwrap())
        .set_token_uri(oauth2::TokenUrl::new("https://apple.example/token".to_string()).unwrap())
        .set_redirect_uri(oauth2::RedirectUrl::new(selected.to_string()).unwrap());
        let params = apple_token_exchange_params(
            &client,
            "apple-client.services",
            "test-secret",
            "test-code",
        )
        .unwrap();
        assert_eq!(
            params,
            [
                ("client_id", "apple-client.services"),
                ("client_secret", "test-secret"),
                ("code", "test-code"),
                ("grant_type", "authorization_code"),
                ("redirect_uri", selected),
            ]
        );
    }

    #[tokio::test]
    #[ignore = "requires AEAD_TAMPER_TEST_DATABASE_URL pointing at disposable migrated local Postgres"]
    async fn db_oauth_same_email_different_subject_is_not_linked() {
        let Some(database_url) = std::env::var("AEAD_TAMPER_TEST_DATABASE_URL").ok() else {
            eprintln!("skipping: AEAD_TAMPER_TEST_DATABASE_URL is not set");
            return;
        };

        let app_state = build_local_test_app_state(database_url).await;
        let project = first_active_project(&app_state);
        let marker = Uuid::new_v4();
        let email = format!("aead-oauth-subject-mismatch-{marker}@example.com");
        let existing_subject = format!("existing-google-sub-{marker}");
        let attacker_subject = format!("attacker-google-sub-{marker}");

        let user = create_oauth_wrapped_user(
            &app_state,
            project.id,
            email.clone(),
            "google",
            existing_subject,
        )
        .await;

        let result = find_or_create_user_from_oauth(
            &app_state,
            Some(email),
            attacker_subject.clone(),
            "google",
            "attacker-access-token".to_string(),
            Some("Attacker Name".to_string()),
            project.id,
        )
        .await;

        assert!(
            matches!(result, Err(ApiError::UserExistsNotLinked)),
            "same email with a different verified provider subject must not be linked"
        );

        let google_provider = app_state
            .db
            .get_oauth_provider_by_name("google")
            .expect("test OAuth provider lookup should succeed")
            .expect("test OAuth provider should exist after AppState build");
        let attacker_connection = app_state
            .db
            .get_project_user_oauth_connection_by_provider_subject(
                google_provider.id,
                &attacker_subject,
                project.id,
            )
            .expect("attacker subject lookup should not error");
        assert!(
            attacker_connection.is_none(),
            "failed same-email OAuth attempt must not create attacker subject connection"
        );

        let _ = app_state.db.delete_user(&user);
    }

    #[tokio::test]
    #[ignore = "requires AEAD_TAMPER_TEST_DATABASE_URL pointing at disposable migrated local Postgres"]
    async fn db_oauth_new_user_creates_initial_seed_wrap_and_login_works() {
        let Some(database_url) = std::env::var("AEAD_TAMPER_TEST_DATABASE_URL").ok() else {
            eprintln!("skipping: AEAD_TAMPER_TEST_DATABASE_URL is not set");
            return;
        };

        let app_state = build_local_test_app_state(database_url).await;
        let project = first_active_project(&app_state);
        let marker = Uuid::new_v4();
        let email = format!("aead-oauth-registration-{marker}@example.com");
        let provider_subject = format!("google-sub-{marker}");

        let authenticated_user = find_or_create_user_from_oauth(
            &app_state,
            Some(email.clone()),
            provider_subject.clone(),
            "google",
            "oauth-registration-access-token".to_string(),
            Some("OAuth Registration Test".to_string()),
            project.id,
        )
        .await
        .expect("new OAuth registration should create user and auth context");

        assert_eq!(authenticated_user.user.get_email(), Some(email.as_str()));

        let google_provider = app_state
            .db
            .get_oauth_provider_by_name("google")
            .expect("test OAuth provider lookup should succeed")
            .expect("test OAuth provider should exist after AppState build");
        let connection = app_state
            .db
            .get_project_user_oauth_connection_by_provider_subject(
                google_provider.id,
                &provider_subject,
                project.id,
            )
            .expect("new OAuth provider subject should load")
            .expect("new OAuth provider subject should be connected");
        assert_eq!(connection.user_id, authenticated_user.user.uuid);

        let verification = app_state
            .db
            .get_email_verification_by_user_id(authenticated_user.user.uuid)
            .expect("new OAuth user should have a verified email row");
        assert!(
            verification.is_verified,
            "OAuth-created users should have verified email rows"
        );

        let oauth_wraps = app_state
            .db
            .get_user_seed_wrappings_for_user_and_kind(
                authenticated_user.user.uuid,
                CredentialKind::OAuth.as_str(),
            )
            .expect("new OAuth user's seed wraps should load");
        assert_eq!(
            oauth_wraps.len(),
            1,
            "OAuth registration should commit exactly one initial OAuth seed wrap"
        );

        app_state
            .verify_seed_wrap_for_auth_context(
                &authenticated_user.user,
                &authenticated_user.auth_context,
            )
            .expect("new OAuth auth context should unwrap the committed seed wrap");

        let _ = app_state.db.delete_user(&authenticated_user.user);
    }

    async fn build_local_test_app_state(database_url: String) -> AppState {
        let db = setup_db(database_url);
        AppStateBuilder::default()
            .app_mode(AppMode::Local)
            .db(db)
            .enclave_key([42u8; 32].to_vec())
            .aws_credential_manager(Arc::new(RwLock::new(None)))
            .openai_api_base("http://localhost:9".to_string())
            .tinfoil_api_base("http://localhost:9".to_string())
            .jwt_secret([24u8; 32].to_vec())
            .build()
            .await
            .expect("local test app state should build")
    }

    fn first_active_project(app_state: &AppState) -> OrgProject {
        let conn = &mut app_state
            .db
            .get_pool()
            .get()
            .expect("test database connection should be available");

        org_projects::table
            .filter(org_projects::status.eq("active"))
            .order(org_projects::id.asc())
            .first::<OrgProject>(conn)
            .expect("test database should contain at least one active project")
    }

    async fn create_oauth_wrapped_user(
        app_state: &AppState,
        project_id: i32,
        email: String,
        provider_name: &str,
        provider_user_id: String,
    ) -> User {
        let user_seed_words = generate_twelve_word_seed(app_state.aws_credential_manager.clone())
            .await
            .expect("test seed should generate")
            .to_string();

        let user = app_state
            .db
            .create_user(NewUser::new(Some(email), None, project_id))
            .expect("test OAuth user should insert");

        let provider = app_state
            .db
            .get_oauth_provider_by_name(provider_name)
            .expect("test OAuth provider lookup should succeed")
            .expect("test OAuth provider should exist after AppState build");

        create_provider_connection(app_state, &user, provider.id, provider_user_id.clone(), "")
            .await
            .expect("test OAuth connection should insert");

        app_state
            .create_oauth_seed_wrap_for_user(
                &user,
                provider_name,
                &provider_user_id,
                user_seed_words.as_bytes(),
            )
            .expect("test OAuth seed wrap should insert");

        user
    }
}
