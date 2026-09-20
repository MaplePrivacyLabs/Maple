use crate::db::DBConnection;
use crate::models::oauth::NewOAuthProvider;
use crate::Error;
use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use oauth2::{
    basic::BasicClient as OAuthBasicClient, AuthUrl, ClientId, ClientSecret, CsrfToken,
    EndpointNotSet, EndpointSet, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, TokenUrl,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{debug, error, info};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::transport_v2::crypto::SessionId;

// OAuth redirects normally complete within seconds. Ten minutes leaves room for a user to
// authenticate without retaining abandoned CSRF state for the encrypted session lifetime.
const OAUTH_STATE_TTL: Duration = Duration::from_secs(10 * 60);
// This limit applies independently to each configured provider.
const OAUTH_STATE_CAPACITY: usize = 4_096;

pub type BasicClient =
    OAuthBasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthState {
    pub csrf_token: String,
    pub client_id: Uuid,
    // Snapshot the selected callback, including when initiation used the default.
    // Complete-state equality protects this value before callback code uses it.
    pub redirect_url: String,
}

#[derive(Debug, Clone)]
struct OAuthStateStore {
    inner: Arc<Mutex<OAuthStateStoreInner>>,
    ttl: Duration,
    capacity: usize,
}

#[derive(Debug, Default)]
struct OAuthStateStoreInner {
    entries: HashMap<String, StoredOAuthState>,
}

#[derive(Debug)]
struct StoredOAuthState {
    state: OAuthState,
    binding: OAuthStateBinding,
    expires_at: Instant,
}

/// Server-only binding for an OAuth authorization round trip.
///
/// V1 preserves its existing state semantics. V2 binds the one-time callback
/// state to the attested transport session that initiated the redirect, so
/// copied provider callback parameters cannot authenticate another session.
#[derive(Debug)]
enum OAuthStateBinding {
    LegacyV1,
    TransportV2 {
        session_id: SessionId,
        code_binding: OAuthCodeBinding,
    },
}

/// Provider-verifiable binding between one V2 authorization response and the
/// enclave session that initiated it.
///
/// GitHub and Google bind the authorization code to a one-time PKCE verifier.
/// Apple does not advertise PKCE, so its signed ID token carries a nonce
/// generated independently and stored alongside the state and V2 session.
pub(crate) enum OAuthCodeBinding {
    Pkce(PkceCodeVerifier),
    AppleNonce(Zeroizing<String>),
}

fn new_pkce_binding() -> (PkceCodeChallenge, OAuthCodeBinding) {
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    (challenge, OAuthCodeBinding::Pkce(verifier))
}

fn new_apple_nonce() -> (String, OAuthCodeBinding) {
    let raw_nonce = Zeroizing::new(URL_SAFE_NO_PAD.encode(crate::encrypt::generate_random::<32>()));
    let public_nonce = hex::encode(Sha256::digest(raw_nonce.as_bytes()));
    (public_nonce, OAuthCodeBinding::AppleNonce(raw_nonce))
}

impl fmt::Debug for OAuthCodeBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pkce(_) => formatter.write_str("Pkce([redacted])"),
            Self::AppleNonce(_) => formatter.write_str("AppleNonce([redacted])"),
        }
    }
}

#[derive(Debug)]
pub(crate) struct BoundOAuthAuthorization {
    pub(crate) auth_url: String,
    pub(crate) csrf_token: CsrfToken,
    pub(crate) code_binding: OAuthCodeBinding,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("OAuth state capacity reached")]
pub struct OAuthStateCapacityError;

impl OAuthStateStore {
    fn new() -> Self {
        Self::with_limits(OAUTH_STATE_TTL, OAUTH_STATE_CAPACITY)
    }

    fn with_limits(ttl: Duration, capacity: usize) -> Self {
        assert!(capacity > 0, "OAuth state capacity must be positive");
        Self {
            inner: Arc::new(Mutex::new(OAuthStateStoreInner::default())),
            ttl,
            capacity,
        }
    }

    async fn store(
        &self,
        csrf_token: &str,
        state: OAuthState,
    ) -> Result<(), OAuthStateCapacityError> {
        self.store_with_binding(
            csrf_token,
            state,
            OAuthStateBinding::LegacyV1,
            Instant::now(),
        )
        .await
    }

    async fn store_for_session(
        &self,
        csrf_token: &str,
        state: OAuthState,
        session_id: SessionId,
        code_binding: OAuthCodeBinding,
    ) -> Result<(), OAuthStateCapacityError> {
        self.store_with_binding(
            csrf_token,
            state,
            OAuthStateBinding::TransportV2 {
                session_id,
                code_binding,
            },
            Instant::now(),
        )
        .await
    }

    #[cfg(test)]
    async fn store_at(
        &self,
        csrf_token: &str,
        state: OAuthState,
        now: Instant,
    ) -> Result<(), OAuthStateCapacityError> {
        self.store_with_binding(csrf_token, state, OAuthStateBinding::LegacyV1, now)
            .await
    }

    async fn store_with_binding(
        &self,
        csrf_token: &str,
        state: OAuthState,
        binding: OAuthStateBinding,
        now: Instant,
    ) -> Result<(), OAuthStateCapacityError> {
        let mut inner = self.inner.lock().await;
        inner
            .entries
            .retain(|_, stored_state| stored_state.expires_at > now);

        if !inner.entries.contains_key(csrf_token) && inner.entries.len() >= self.capacity {
            return Err(OAuthStateCapacityError);
        }

        inner.entries.insert(
            csrf_token.to_string(),
            StoredOAuthState {
                state,
                binding,
                expires_at: now
                    .checked_add(self.ttl)
                    .expect("OAuth state TTL must fit in Instant"),
            },
        );
        Ok(())
    }

    async fn consume(&self, state: &OAuthState) -> bool {
        self.take_with_binding(state, None, Instant::now())
            .await
            .is_some()
    }

    async fn consume_for_session(
        &self,
        state: &OAuthState,
        session_id: SessionId,
    ) -> Option<OAuthCodeBinding> {
        let stored_state = self
            .take_with_binding(state, Some(session_id), Instant::now())
            .await?;
        match stored_state.binding {
            OAuthStateBinding::TransportV2 { code_binding, .. } => Some(code_binding),
            OAuthStateBinding::LegacyV1 => None,
        }
    }

    #[cfg(test)]
    async fn consume_at(&self, state: &OAuthState, now: Instant) -> bool {
        self.take_with_binding(state, None, now).await.is_some()
    }

    async fn take_with_binding(
        &self,
        state: &OAuthState,
        session_id: Option<SessionId>,
        now: Instant,
    ) -> Option<StoredOAuthState> {
        let mut inner = self.inner.lock().await;
        inner
            .entries
            .retain(|_, stored_state| stored_state.expires_at > now);

        let matches = inner.entries.get(&state.csrf_token).is_some_and(|stored| {
            stored.state == *state
                && match (&stored.binding, session_id) {
                    (OAuthStateBinding::LegacyV1, None) => true,
                    (
                        OAuthStateBinding::TransportV2 {
                            session_id: stored_session,
                            ..
                        },
                        Some(expected_session),
                    ) => *stored_session == expected_session,
                    _ => false,
                }
        });
        matches
            .then(|| inner.entries.remove(&state.csrf_token))
            .flatten()
    }

    #[cfg(test)]
    async fn len_at(&self, now: Instant) -> usize {
        let mut inner = self.inner.lock().await;
        inner
            .entries
            .retain(|_, stored_state| stored_state.expires_at > now);
        inner.entries.len()
    }
}

#[derive(Debug, Clone)]
pub struct GithubProvider {
    pub auth_url: String,
    pub token_url: String,
    pub user_info_url: String,
    state_store: OAuthStateStore,
}

impl GithubProvider {
    pub async fn new(db: Arc<dyn DBConnection + Send + Sync>) -> Result<Self, Error> {
        let auth_url = "https://github.com/login/oauth/authorize".to_string();
        let token_url = "https://github.com/login/oauth/access_token".to_string();
        let user_info_url = "https://api.github.com/user".to_string();

        let provider = Self {
            auth_url,
            token_url,
            user_info_url,
            state_store: OAuthStateStore::new(),
        };

        // Ensure the provider exists in the database
        provider.ensure_provider_exists(db).await?;

        info!("GitHub OAuth provider initialized successfully");
        Ok(provider)
    }

    pub async fn build_client(
        &self,
        client_id: String,
        client_secret: String,
        redirect_url: String,
    ) -> Result<BasicClient, Error> {
        let auth_url = AuthUrl::new(self.auth_url.clone())
            .map_err(|e| Error::OAuthError(format!("Invalid auth URL: {}", e)))?;
        let token_url = TokenUrl::new(self.token_url.clone())
            .map_err(|e| Error::OAuthError(format!("Invalid token URL: {}", e)))?;

        Ok(OAuthBasicClient::new(ClientId::new(client_id))
            .set_client_secret(ClientSecret::new(client_secret))
            .set_auth_uri(auth_url)
            .set_token_uri(token_url)
            .set_redirect_uri(
                RedirectUrl::new(redirect_url)
                    .map_err(|e| Error::OAuthError(format!("Invalid redirect URL: {}", e)))?,
            ))
    }

    pub async fn generate_authorize_url(&self, client: &BasicClient) -> (String, CsrfToken) {
        let (auth_url, csrf_token) = client
            .authorize_url(CsrfToken::new_random)
            .add_scope(Scope::new("user:email".to_string()))
            .url();

        (auth_url.to_string(), csrf_token)
    }

    pub(crate) async fn generate_bound_authorize_url(
        &self,
        client: &BasicClient,
    ) -> BoundOAuthAuthorization {
        let (challenge, binding) = new_pkce_binding();
        let (auth_url, csrf_token) = client
            .authorize_url(CsrfToken::new_random)
            .add_scope(Scope::new("user:email".to_string()))
            .set_pkce_challenge(challenge)
            .url();

        BoundOAuthAuthorization {
            auth_url: auth_url.to_string(),
            csrf_token,
            code_binding: binding,
        }
    }

    async fn ensure_provider_exists(
        &self,
        db: Arc<dyn DBConnection + Send + Sync>,
    ) -> Result<(), Error> {
        debug!("Checking if GitHub OAuth provider exists in the database");
        let existing_provider = db.get_oauth_provider_by_name("github")?;

        if existing_provider.is_none() {
            info!("GitHub OAuth provider not found in database, creating new entry");
            let new_provider = NewOAuthProvider {
                name: "github".to_string(),
                auth_url: self.auth_url.clone(),
                token_url: self.token_url.clone(),
                user_info_url: self.user_info_url.clone(),
            };

            match db.create_oauth_provider(new_provider) {
                Ok(_) => info!("GitHub OAuth provider successfully added to database"),
                Err(e) => {
                    error!(
                        "Failed to create GitHub OAuth provider in database: {:?}",
                        e
                    );
                    return Err(e.into());
                }
            }
        } else {
            debug!("GitHub OAuth provider already exists in database");
        }

        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct GoogleProvider {
    pub auth_url: String,
    pub token_url: String,
    pub user_info_url: String,
    state_store: OAuthStateStore,
}

impl GoogleProvider {
    pub async fn new(db: Arc<dyn DBConnection + Send + Sync>) -> Result<Self, Error> {
        let auth_url = "https://accounts.google.com/o/oauth2/v2/auth".to_string();
        let token_url = "https://oauth2.googleapis.com/token".to_string();
        let user_info_url = "https://www.googleapis.com/oauth2/v3/userinfo".to_string();

        let provider = Self {
            auth_url,
            token_url,
            user_info_url,
            state_store: OAuthStateStore::new(),
        };

        // Ensure the provider exists in the database
        provider.ensure_provider_exists(db).await?;

        info!("Google OAuth provider initialized successfully");
        Ok(provider)
    }

    pub async fn build_client(
        &self,
        client_id: String,
        client_secret: String,
        redirect_url: String,
    ) -> Result<BasicClient, Error> {
        let auth_url = AuthUrl::new(self.auth_url.clone())
            .map_err(|e| Error::OAuthError(format!("Invalid auth URL: {}", e)))?;
        let token_url = TokenUrl::new(self.token_url.clone())
            .map_err(|e| Error::OAuthError(format!("Invalid token URL: {}", e)))?;

        Ok(OAuthBasicClient::new(ClientId::new(client_id))
            .set_client_secret(ClientSecret::new(client_secret))
            .set_auth_uri(auth_url)
            .set_token_uri(token_url)
            .set_redirect_uri(
                RedirectUrl::new(redirect_url)
                    .map_err(|e| Error::OAuthError(format!("Invalid redirect URL: {}", e)))?,
            ))
    }

    pub async fn generate_authorize_url(&self, client: &BasicClient) -> (String, CsrfToken) {
        let (auth_url, csrf_token) = client
            .authorize_url(CsrfToken::new_random)
            .add_scope(Scope::new("email".to_string()))
            .add_scope(Scope::new("profile".to_string()))
            .url();

        (auth_url.to_string(), csrf_token)
    }

    pub(crate) async fn generate_bound_authorize_url(
        &self,
        client: &BasicClient,
    ) -> BoundOAuthAuthorization {
        let (challenge, binding) = new_pkce_binding();
        let (auth_url, csrf_token) = client
            .authorize_url(CsrfToken::new_random)
            .add_scope(Scope::new("email".to_string()))
            .add_scope(Scope::new("profile".to_string()))
            .set_pkce_challenge(challenge)
            .url();

        BoundOAuthAuthorization {
            auth_url: auth_url.to_string(),
            csrf_token,
            code_binding: binding,
        }
    }

    async fn ensure_provider_exists(
        &self,
        db: Arc<dyn DBConnection + Send + Sync>,
    ) -> Result<(), Error> {
        debug!("Checking if Google OAuth provider exists in the database");
        let existing_provider = db.get_oauth_provider_by_name("google")?;

        if existing_provider.is_none() {
            info!("Google OAuth provider not found in database, creating new entry");
            let new_provider = NewOAuthProvider {
                name: "google".to_string(),
                auth_url: self.auth_url.clone(),
                token_url: self.token_url.clone(),
                user_info_url: self.user_info_url.clone(),
            };

            match db.create_oauth_provider(new_provider) {
                Ok(_) => info!("Google OAuth provider successfully added to database"),
                Err(e) => {
                    error!(
                        "Failed to create Google OAuth provider in database: {:?}",
                        e
                    );
                    return Err(e.into());
                }
            }
        } else {
            debug!("Google OAuth provider already exists in database");
        }

        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct AppleProvider {
    pub auth_url: String,
    pub token_url: String,
    pub jwks_url: String,
    state_store: OAuthStateStore,
    // No need for user_info_url as Apple doesn't have a separate endpoint - all info is in the ID token
}

impl AppleProvider {
    pub async fn new(db: Arc<dyn DBConnection + Send + Sync>) -> Result<Self, Error> {
        let auth_url = "https://appleid.apple.com/auth/authorize".to_string();
        let token_url = "https://appleid.apple.com/auth/token".to_string();
        let jwks_url = "https://appleid.apple.com/auth/keys".to_string();

        let provider = Self {
            auth_url,
            token_url,
            jwks_url,
            state_store: OAuthStateStore::new(),
        };

        // Ensure the provider exists in the database
        provider.ensure_provider_exists(db).await?;

        info!("Apple OAuth provider initialized successfully");
        Ok(provider)
    }

    pub async fn build_client(
        &self,
        client_id: String,
        client_secret: String,
        redirect_url: String,
    ) -> Result<BasicClient, Error> {
        let auth_url = AuthUrl::new(self.auth_url.clone())
            .map_err(|e| Error::OAuthError(format!("Invalid auth URL: {}", e)))?;
        let token_url = TokenUrl::new(self.token_url.clone())
            .map_err(|e| Error::OAuthError(format!("Invalid token URL: {}", e)))?;

        Ok(OAuthBasicClient::new(ClientId::new(client_id))
            .set_client_secret(ClientSecret::new(client_secret))
            .set_auth_uri(auth_url)
            .set_token_uri(token_url)
            .set_redirect_uri(
                RedirectUrl::new(redirect_url)
                    .map_err(|e| Error::OAuthError(format!("Invalid redirect URL: {}", e)))?,
            ))
    }

    pub async fn generate_authorize_url(&self, client: &BasicClient) -> (String, CsrfToken) {
        // For Apple Sign In, we'll primarily use the JS SDK on the frontend
        // This URL is fallback for non-JS environments
        let (auth_url, csrf_token) = client
            .authorize_url(CsrfToken::new_random)
            .add_scope(Scope::new("name".to_string()))
            .add_scope(Scope::new("email".to_string()))
            .add_extra_param("response_type", "code id_token") // Apple best practice
            .add_extra_param("response_mode", "form_post") // Required by Apple
            .url();

        (auth_url.to_string(), csrf_token)
    }

    pub(crate) async fn generate_bound_authorize_url(
        &self,
        client: &BasicClient,
    ) -> BoundOAuthAuthorization {
        let (nonce, binding) = new_apple_nonce();
        let (auth_url, csrf_token) = client
            .authorize_url(CsrfToken::new_random)
            .add_scope(Scope::new("name".to_string()))
            .add_scope(Scope::new("email".to_string()))
            .add_extra_param("response_type", "code id_token")
            .add_extra_param("response_mode", "form_post")
            .add_extra_param("nonce", nonce)
            .url();

        BoundOAuthAuthorization {
            auth_url: auth_url.to_string(),
            csrf_token,
            code_binding: binding,
        }
    }

    async fn ensure_provider_exists(
        &self,
        db: Arc<dyn DBConnection + Send + Sync>,
    ) -> Result<(), Error> {
        debug!("Checking if Apple OAuth provider exists in the database");
        let existing_provider = db.get_oauth_provider_by_name("apple")?;

        if existing_provider.is_none() {
            info!("Apple OAuth provider not found in database, creating new entry");
            // Set a dummy user_info_url as Apple doesn't have one, everything comes from the JWT
            let new_provider = NewOAuthProvider {
                name: "apple".to_string(),
                auth_url: self.auth_url.clone(),
                token_url: self.token_url.clone(),
                user_info_url: self.jwks_url.clone(), // Use JWKS URL in place of user_info_url
            };

            match db.create_oauth_provider(new_provider) {
                Ok(_) => info!("Apple OAuth provider successfully added to database"),
                Err(e) => {
                    error!("Failed to create Apple OAuth provider in database: {:?}", e);
                    return Err(e.into());
                }
            }
        } else {
            debug!("Apple OAuth provider already exists in database");
        }

        Ok(())
    }
}

#[async_trait]
pub trait OAuthProvider: Send + Sync + 'static {
    fn as_github(&self) -> Option<&GithubProvider> {
        None
    }

    fn as_google(&self) -> Option<&GoogleProvider> {
        None
    }

    fn as_apple(&self) -> Option<&AppleProvider> {
        None
    }

    async fn generate_authorize_url(&self, client: &BasicClient) -> (String, CsrfToken);
    async fn store_state(
        &self,
        csrf_token: &str,
        state: OAuthState,
    ) -> Result<(), OAuthStateCapacityError>;
    async fn store_state_for_session(
        &self,
        csrf_token: &str,
        state: OAuthState,
        session_id: SessionId,
        code_binding: OAuthCodeBinding,
    ) -> Result<(), OAuthStateCapacityError>;
    async fn consume_state(&self, state: &OAuthState) -> bool;
    async fn consume_state_for_session(
        &self,
        state: &OAuthState,
        session_id: SessionId,
    ) -> Option<OAuthCodeBinding>;
    async fn build_client(
        &self,
        client_id: String,
        client_secret: String,
        redirect_url: String,
    ) -> Result<BasicClient, Error>;
}

pub struct OAuthManager {
    providers: HashMap<String, Box<dyn OAuthProvider>>,
}

impl OAuthManager {
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
        }
    }

    pub fn add_provider(&mut self, name: String, provider: Box<dyn OAuthProvider>) {
        self.providers.insert(name, provider);
    }

    pub fn get_provider(&self, name: &str) -> Option<&dyn OAuthProvider> {
        self.providers.get(name).map(|p| p.as_ref())
    }
}

#[async_trait]
impl OAuthProvider for GithubProvider {
    fn as_github(&self) -> Option<&GithubProvider> {
        Some(self)
    }

    async fn generate_authorize_url(&self, client: &BasicClient) -> (String, CsrfToken) {
        self.generate_authorize_url(client).await
    }

    async fn store_state(
        &self,
        csrf_token: &str,
        state: OAuthState,
    ) -> Result<(), OAuthStateCapacityError> {
        self.state_store.store(csrf_token, state).await
    }

    async fn store_state_for_session(
        &self,
        csrf_token: &str,
        state: OAuthState,
        session_id: SessionId,
        code_binding: OAuthCodeBinding,
    ) -> Result<(), OAuthStateCapacityError> {
        self.state_store
            .store_for_session(csrf_token, state, session_id, code_binding)
            .await
    }

    async fn consume_state(&self, state: &OAuthState) -> bool {
        self.state_store.consume(state).await
    }

    async fn consume_state_for_session(
        &self,
        state: &OAuthState,
        session_id: SessionId,
    ) -> Option<OAuthCodeBinding> {
        self.state_store
            .consume_for_session(state, session_id)
            .await
    }

    async fn build_client(
        &self,
        client_id: String,
        client_secret: String,
        redirect_url: String,
    ) -> Result<BasicClient, Error> {
        self.build_client(client_id, client_secret, redirect_url)
            .await
    }
}

#[async_trait]
impl OAuthProvider for GoogleProvider {
    fn as_google(&self) -> Option<&GoogleProvider> {
        Some(self)
    }

    async fn generate_authorize_url(&self, client: &BasicClient) -> (String, CsrfToken) {
        self.generate_authorize_url(client).await
    }

    async fn store_state(
        &self,
        csrf_token: &str,
        state: OAuthState,
    ) -> Result<(), OAuthStateCapacityError> {
        self.state_store.store(csrf_token, state).await
    }

    async fn store_state_for_session(
        &self,
        csrf_token: &str,
        state: OAuthState,
        session_id: SessionId,
        code_binding: OAuthCodeBinding,
    ) -> Result<(), OAuthStateCapacityError> {
        self.state_store
            .store_for_session(csrf_token, state, session_id, code_binding)
            .await
    }

    async fn consume_state(&self, state: &OAuthState) -> bool {
        self.state_store.consume(state).await
    }

    async fn consume_state_for_session(
        &self,
        state: &OAuthState,
        session_id: SessionId,
    ) -> Option<OAuthCodeBinding> {
        self.state_store
            .consume_for_session(state, session_id)
            .await
    }

    async fn build_client(
        &self,
        client_id: String,
        client_secret: String,
        redirect_url: String,
    ) -> Result<BasicClient, Error> {
        self.build_client(client_id, client_secret, redirect_url)
            .await
    }
}

#[async_trait]
impl OAuthProvider for AppleProvider {
    fn as_apple(&self) -> Option<&AppleProvider> {
        Some(self)
    }

    async fn generate_authorize_url(&self, client: &BasicClient) -> (String, CsrfToken) {
        self.generate_authorize_url(client).await
    }

    async fn store_state(
        &self,
        csrf_token: &str,
        state: OAuthState,
    ) -> Result<(), OAuthStateCapacityError> {
        self.state_store.store(csrf_token, state).await
    }

    async fn store_state_for_session(
        &self,
        csrf_token: &str,
        state: OAuthState,
        session_id: SessionId,
        code_binding: OAuthCodeBinding,
    ) -> Result<(), OAuthStateCapacityError> {
        self.state_store
            .store_for_session(csrf_token, state, session_id, code_binding)
            .await
    }

    async fn consume_state(&self, state: &OAuthState) -> bool {
        self.state_store.consume(state).await
    }

    async fn consume_state_for_session(
        &self,
        state: &OAuthState,
        session_id: SessionId,
    ) -> Option<OAuthCodeBinding> {
        self.state_store
            .consume_for_session(state, session_id)
            .await
    }

    async fn build_client(
        &self,
        client_id: String,
        client_secret: String,
        redirect_url: String,
    ) -> Result<BasicClient, Error> {
        self.build_client(client_id, client_secret, redirect_url)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::Barrier;

    fn state(csrf_token: &str, client_id: u128) -> OAuthState {
        OAuthState {
            csrf_token: csrf_token.to_string(),
            client_id: Uuid::from_u128(client_id),
            redirect_url: "https://app.example/auth/callback".to_string(),
        }
    }

    fn assert_pkce_authorization(authorization: &BoundOAuthAuthorization) {
        let parameters: HashMap<_, _> = url::Url::parse(&authorization.auth_url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect();
        assert_eq!(
            parameters.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        let OAuthCodeBinding::Pkce(verifier) = &authorization.code_binding else {
            panic!("V2 authorization must retain a PKCE verifier");
        };
        assert_eq!(
            parameters.get("code_challenge").map(String::as_str),
            Some(PkceCodeChallenge::from_code_verifier_sha256(verifier).as_str())
        );
    }

    #[tokio::test]
    async fn oauth_state_expires_at_ttl() {
        let ttl = Duration::from_secs(10);
        let store = OAuthStateStore::with_limits(ttl, 2);
        let now = Instant::now();
        let oauth_state = state("expired", 1);

        store
            .store_at(&oauth_state.csrf_token, oauth_state.clone(), now)
            .await
            .unwrap();

        assert!(!store.consume_at(&oauth_state, now + ttl).await);
        assert_eq!(store.len_at(now + ttl).await, 0);
    }

    #[tokio::test]
    async fn oauth_state_store_rejects_new_entry_at_capacity() {
        let store = OAuthStateStore::with_limits(Duration::from_secs(60), 2);
        let now = Instant::now();
        let first = state("first", 1);
        let second = state("second", 2);
        let third = state("third", 3);

        store
            .store_at(&first.csrf_token, first.clone(), now)
            .await
            .unwrap();
        store
            .store_at(&second.csrf_token, second.clone(), now)
            .await
            .unwrap();

        assert_eq!(
            store.store_at(&third.csrf_token, third.clone(), now).await,
            Err(OAuthStateCapacityError)
        );

        assert_eq!(store.len_at(now).await, 2);
        assert!(store.consume_at(&first, now).await);
        assert!(store.consume_at(&second, now).await);
        assert!(!store.consume_at(&third, now).await);
    }

    #[tokio::test]
    async fn oauth_state_store_replaces_existing_entry_at_capacity() {
        let store = OAuthStateStore::with_limits(Duration::from_secs(60), 1);
        let now = Instant::now();
        let original = state("shared", 1);
        let replacement = state("shared", 2);

        store
            .store_at(&original.csrf_token, original.clone(), now)
            .await
            .unwrap();
        store
            .store_at(&replacement.csrf_token, replacement.clone(), now)
            .await
            .unwrap();

        assert_eq!(store.len_at(now).await, 1);
        assert!(!store.consume_at(&original, now).await);
        assert!(store.consume_at(&replacement, now).await);
    }

    #[tokio::test]
    async fn oauth_state_store_prunes_expired_entries_before_capacity_check() {
        let ttl = Duration::from_secs(10);
        let store = OAuthStateStore::with_limits(ttl, 2);
        let now = Instant::now();
        let expired = state("expired", 1);
        let live = state("live", 2);
        let newest = state("newest", 3);

        store
            .store_at(&expired.csrf_token, expired.clone(), now)
            .await
            .unwrap();
        store
            .store_at(&live.csrf_token, live.clone(), now + Duration::from_secs(5))
            .await
            .unwrap();
        store
            .store_at(&newest.csrf_token, newest.clone(), now + ttl)
            .await
            .unwrap();

        assert_eq!(store.len_at(now + ttl).await, 2);
        assert!(!store.consume_at(&expired, now + ttl).await);
        assert!(store.consume_at(&live, now + ttl).await);
        assert!(store.consume_at(&newest, now + ttl).await);
    }

    #[tokio::test]
    async fn mismatched_oauth_callback_does_not_consume_valid_state() {
        let store = OAuthStateStore::with_limits(Duration::from_secs(60), 2);
        let now = Instant::now();
        let valid = state("csrf-token", 1);
        let mismatched = state("csrf-token", 2);

        store
            .store_at(&valid.csrf_token, valid.clone(), now)
            .await
            .unwrap();

        assert!(!store.consume_at(&mismatched, now).await);
        assert!(store.consume_at(&valid, now).await);
    }

    #[test]
    fn oauth_state_round_trip_requires_the_selected_redirect() {
        let original = state("state-round-trip", 1);
        let encoded = serde_json::to_value(&original).unwrap();
        assert_eq!(
            serde_json::from_value::<OAuthState>(encoded.clone()).unwrap(),
            original
        );
        let mut missing_redirect = encoded;
        missing_redirect
            .as_object_mut()
            .unwrap()
            .remove("redirect_url");
        assert!(serde_json::from_value::<OAuthState>(missing_redirect).is_err());
    }

    #[tokio::test]
    async fn changed_redirect_is_rejected_without_consuming_v1_or_v2_state() {
        for session_id in [None, Some(SessionId::from_bytes([42; 16]))] {
            let store = OAuthStateStore::with_limits(Duration::from_secs(60), 2);
            let now = Instant::now();
            let valid = state("redirect-bound-state", 1);
            let binding = match session_id {
                Some(session_id) => OAuthStateBinding::TransportV2 {
                    session_id,
                    code_binding: new_pkce_binding().1,
                },
                None => OAuthStateBinding::LegacyV1,
            };
            store
                .store_with_binding(&valid.csrf_token, valid.clone(), binding, now)
                .await
                .unwrap();

            for changed_redirect in ["https://auth.example/auth/callback", ""] {
                let mut changed = valid.clone();
                changed.redirect_url = changed_redirect.to_string();
                assert!(store
                    .take_with_binding(&changed, session_id, now)
                    .await
                    .is_none());
            }
            let consumed = store
                .take_with_binding(&valid, session_id, now)
                .await
                .unwrap();
            assert_eq!(consumed.state.redirect_url, valid.redirect_url);
            assert!(store
                .take_with_binding(&valid, session_id, now)
                .await
                .is_none());
        }
    }

    #[tokio::test]
    async fn oauth_state_protocol_and_v2_session_binding_are_non_consuming_on_mismatch() {
        let store = OAuthStateStore::with_limits(Duration::from_secs(60), 2);
        let now = Instant::now();
        let v2_state = state("v2-state", 1);
        let initiating_session = SessionId::from_bytes([10; 16]);
        let other_session = SessionId::from_bytes([11; 16]);

        store
            .store_with_binding(
                &v2_state.csrf_token,
                v2_state.clone(),
                OAuthStateBinding::TransportV2 {
                    session_id: initiating_session,
                    code_binding: OAuthCodeBinding::AppleNonce(Zeroizing::new(
                        "raw-nonce".to_string(),
                    )),
                },
                now,
            )
            .await
            .unwrap();

        assert!(!store.consume_at(&v2_state, now).await);
        assert!(store
            .take_with_binding(&v2_state, Some(other_session), now)
            .await
            .is_none());
        let consumed = store
            .take_with_binding(&v2_state, Some(initiating_session), now)
            .await
            .unwrap();
        let OAuthStateBinding::TransportV2 {
            code_binding: OAuthCodeBinding::AppleNonce(raw_nonce),
            ..
        } = consumed.binding
        else {
            panic!("expected an Apple nonce binding");
        };
        assert_eq!(raw_nonce.as_str(), "raw-nonce");

        let v1_state = state("v1-state", 2);
        store
            .store_at(&v1_state.csrf_token, v1_state.clone(), now)
            .await
            .unwrap();
        assert!(store
            .take_with_binding(&v1_state, Some(initiating_session), now)
            .await
            .is_none());
        assert!(store.consume_at(&v1_state, now).await);
    }

    #[tokio::test]
    async fn v2_authorization_urls_bind_codes_without_changing_v1_urls() {
        let github = GithubProvider {
            auth_url: "https://github.example/authorize".to_string(),
            token_url: "https://github.example/token".to_string(),
            user_info_url: "https://github.example/user".to_string(),
            state_store: OAuthStateStore::new(),
        };
        let github_client = github
            .build_client(
                "github-client".to_string(),
                "github-secret".to_string(),
                "https://maple.example/callback".to_string(),
            )
            .await
            .unwrap();

        let legacy_url = github.generate_authorize_url(&github_client).await.0;
        let legacy_parameters: HashMap<_, _> = url::Url::parse(&legacy_url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect();
        assert!(!legacy_parameters.contains_key("code_challenge"));
        assert!(!legacy_parameters.contains_key("code_challenge_method"));
        assert_eq!(
            legacy_parameters.get("redirect_uri").map(String::as_str),
            Some("https://maple.example/callback")
        );

        let bound = github.generate_bound_authorize_url(&github_client).await;
        assert_pkce_authorization(&bound);
        assert!(
            url::Url::parse(&bound.auth_url)
                .unwrap()
                .query_pairs()
                .any(|(key, value)| key == "redirect_uri"
                    && value == "https://maple.example/callback")
        );

        let google = GoogleProvider {
            auth_url: "https://google.example/authorize".to_string(),
            token_url: "https://google.example/token".to_string(),
            user_info_url: "https://google.example/user".to_string(),
            state_store: OAuthStateStore::new(),
        };
        let google_client = google
            .build_client(
                "google-client".to_string(),
                "google-secret".to_string(),
                "https://maple.example/callback".to_string(),
            )
            .await
            .unwrap();
        let bound = google.generate_bound_authorize_url(&google_client).await;
        assert_pkce_authorization(&bound);
        for auth_url in [
            google.generate_authorize_url(&google_client).await.0,
            bound.auth_url,
        ] {
            assert!(url::Url::parse(&auth_url)
                .unwrap()
                .query_pairs()
                .any(|(key, value)| key == "redirect_uri"
                    && value == "https://maple.example/callback"));
        }

        let apple = AppleProvider {
            auth_url: "https://apple.example/authorize".to_string(),
            token_url: "https://apple.example/token".to_string(),
            jwks_url: "https://apple.example/keys".to_string(),
            state_store: OAuthStateStore::new(),
        };
        let apple_client = apple
            .build_client(
                "apple-client".to_string(),
                "apple-secret".to_string(),
                "https://maple.example/callback".to_string(),
            )
            .await
            .unwrap();
        let legacy_url = apple.generate_authorize_url(&apple_client).await.0;
        let legacy_parameters: HashMap<_, _> = url::Url::parse(&legacy_url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect();
        assert!(!legacy_parameters.contains_key("nonce"));
        assert_eq!(
            legacy_parameters.get("redirect_uri").map(String::as_str),
            Some("https://maple.example/callback")
        );

        let bound = apple.generate_bound_authorize_url(&apple_client).await;
        let parameters: HashMap<_, _> = url::Url::parse(&bound.auth_url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect();
        let OAuthCodeBinding::AppleNonce(raw_nonce) = &bound.code_binding else {
            panic!("Apple V2 authorization must retain the raw nonce");
        };
        let expected_nonce = hex::encode(Sha256::digest(raw_nonce.as_bytes()));
        assert_eq!(
            parameters.get("nonce").map(String::as_str),
            Some(expected_nonce.as_str())
        );
        assert_eq!(
            parameters.get("redirect_uri").map(String::as_str),
            Some("https://maple.example/callback")
        );
    }

    #[tokio::test]
    async fn oauth_token_exchange_sends_the_recorded_redirect_uri() {
        let recorded_redirect = "https://auth.example/auth/google/callback";
        let client = OAuthBasicClient::new(ClientId::new("test-client".to_string()))
            .set_client_secret(ClientSecret::new("test-secret".to_string()))
            .set_auth_uri(AuthUrl::new("https://provider.example/authorize".to_string()).unwrap())
            .set_token_uri(TokenUrl::new("https://provider.example/token".to_string()).unwrap())
            .set_redirect_uri(RedirectUrl::new(recorded_redirect.to_string()).unwrap());
        let token = client
            .exchange_code(oauth2::AuthorizationCode::new("test-code".to_string()))
            .request_async(&|request: oauth2::HttpRequest| async move {
                let parameters: HashMap<_, _> = url::form_urlencoded::parse(request.body())
                    .into_owned()
                    .collect();
                assert_eq!(
                    parameters.get("redirect_uri").map(String::as_str),
                    Some(recorded_redirect)
                );
                Ok::<_, std::convert::Infallible>(
                    oauth2::http::Response::builder()
                        .status(200)
                        .header("Content-Type", "application/json")
                        .body(br#"{"access_token":"test-token","token_type":"bearer"}"#.to_vec())
                        .unwrap(),
                )
            })
            .await
            .unwrap();
        assert_eq!(
            oauth2::TokenResponse::access_token(&token).secret(),
            "test-token"
        );
    }

    #[tokio::test]
    async fn valid_oauth_state_can_only_be_consumed_once() {
        let store = OAuthStateStore::with_limits(Duration::from_secs(60), 2);
        let now = Instant::now();
        let oauth_state = state("one-time", 1);

        store
            .store_at(&oauth_state.csrf_token, oauth_state.clone(), now)
            .await
            .unwrap();

        assert!(store.consume_at(&oauth_state, now).await);
        assert!(!store.consume_at(&oauth_state, now).await);
    }

    #[tokio::test]
    async fn concurrent_oauth_callbacks_have_one_winner() {
        let store = OAuthStateStore::with_limits(Duration::from_secs(60), 2);
        let now = Instant::now();
        let oauth_state = state("raced", 1);
        store
            .store_at(&oauth_state.csrf_token, oauth_state.clone(), now)
            .await
            .unwrap();

        let barrier = Arc::new(Barrier::new(3));
        let first = {
            let store = store.clone();
            let barrier = barrier.clone();
            let oauth_state = oauth_state.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                store.consume_at(&oauth_state, now).await
            })
        };
        let second = {
            let store = store.clone();
            let barrier = barrier.clone();
            let oauth_state = oauth_state.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                store.consume_at(&oauth_state, now).await
            })
        };

        barrier.wait().await;
        let consumed = [first.await.unwrap(), second.await.unwrap()]
            .into_iter()
            .filter(|was_consumed| *was_consumed)
            .count();

        assert_eq!(consumed, 1);
    }
}
