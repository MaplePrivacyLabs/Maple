//! Browser sign-in for an HTTP MCP server, and the saved sign-in its
//! connections use. It runs on the computer that holds the server's
//! definition: the browser returns to a loopback listener here, tokens stay in
//! this host's store, and no Maple service takes part.
//!
//! A server is reached over HTTPS, through rmcp's client that refuses private
//! addresses, or over plain HTTP, held to that exact listener.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::stream::BoxStream;
use http::{HeaderName, HeaderValue};
use rmcp::model::ClientJsonRpcMessage;
use rmcp::service::ClientServiceExt;
use rmcp::transport::auth::{
    AuthClient, AuthError, AuthorizationManager, AuthorizationMetadata, AuthorizationRequest,
    AuthorizationSession, CredentialRefreshGuard, CredentialStore, InMemoryCredentialStore,
    OAuthClientConfig, OAuthHttpClient, OAuthHttpClientFuture, OAuthHttpRequest, StoredCredentials,
};
use rmcp::transport::streamable_http_client::{
    SseError, StreamableHttpClient, StreamableHttpClientTransport,
    StreamableHttpClientTransportConfig, StreamableHttpError, StreamableHttpPostResponse,
};
use serde::{Deserialize, Serialize};
use sse_stream::Sse;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;
use tracing::instrument::WithSubscriber;

use super::super::AgentRuntimeHandle;
use super::super::config::account_store;
use crate::sign_in_page::sign_in_page_response;
use crate::store::records::{
    McpDefinition, McpOAuthCredentials, McpServerEdit, McpTarget, SecretString,
};
use crate::store::{AccountId, Store};

/// How long a person has to sign in, two-factor prompts included.
const AUTH_TIMEOUT: Duration = Duration::from_secs(300);
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_HTTP_BODY: usize = 1024 * 1024;
const MAX_CREDENTIAL_BYTES: usize = 64 * 1024;
const CALLBACK_BYTES: usize = 8 * 1024;
const AUTH_FAILURE: &str = "Sign-in did not complete. Try again.";
const STORAGE_FAILURE: &str =
    "The saved sign-in changed or its local store is unavailable. Sign in again.";
const SIGNED_OUT: &str = "This server's sign-in ended. Sign in again in Integrations.";
const NOT_APPROVED: &str = "Access was not approved.";
const NEEDS_REGISTERED_APP: &str =
    "This server only accepts apps registered with it, so Maple cannot sign in to it yet.";
const CLIENT_REFUSED: &str = "This server did not accept Maple as an app.";
/// How long a plain HTTP server gets to answer. These are usually close by, and
/// someone waits on this when an app's usual address is tried for them.
const PLAIN_ANSWER_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const SIGN_IN_CANCELLED: &str = "Sign-in was cancelled.";
/// How Maple names itself when it registers with a server.
const CLIENT_NAME: &str = "Maple";

/// Why a sign-in stopped. The reasons that change what happens next stay typed
/// on the way to the UI, which also words them in the person's language.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpSignInError {
    /// No address was given.
    NoAddress,
    /// The address is not one a sign-in can use.
    InvalidAddress,
    /// Nothing answered at the address, so a different one might work.
    Unreachable,
    /// Something answered, but not an MCP server, so a different one might work.
    NoServerHere,
    /// Anything else, in words for the person.
    Other(String),
}

impl std::fmt::Display for McpSignInError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NoAddress => "Enter an address first.",
            Self::InvalidAddress => "Sign-in needs an http:// or https:// address.",
            Self::Unreachable => "Maple could not reach this address. Check it and try again.",
            Self::NoServerHere => "Maple reached this address, but found no MCP server there.",
            Self::Other(message) => message,
        })
    }
}

impl std::error::Error for McpSignInError {}

impl From<String> for McpSignInError {
    fn from(message: String) -> Self {
        Self::Other(message)
    }
}

impl From<&str> for McpSignInError {
    fn from(message: &str) -> Self {
        Self::Other(message.into())
    }
}

/// The browser launcher is injected by the local host, never by MCP output.
pub type McpBrowser = Arc<dyn Fn(String) -> Result<(), String> + Send + Sync>;

/// A server to sign in to.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpSignIn {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub url: String,
    /// Empty asks for what the server advertises.
    #[serde(default)]
    pub scopes: Vec<String>,
}

/// An address a sign-in may use, HTTP or HTTPS. A token sent over plain HTTP
/// can be read on the way, so [`PlainOAuthHttp`] at least holds the sign-in to
/// the address the person gave.
pub(crate) fn sign_in_endpoint(value: &str) -> Result<reqwest::Url, McpSignInError> {
    let url = reqwest::Url::parse(value.trim()).map_err(|_| McpSignInError::InvalidAddress)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(McpSignInError::InvalidAddress);
    }
    Ok(url)
}

fn is_plain_http(endpoint: &reqwest::Url) -> bool {
    endpoint.scheme() == "http"
}

fn auth_error(_: impl std::fmt::Debug) -> String {
    // OAuth/server errors can contain codes, request bodies and callback URLs.
    AUTH_FAILURE.into()
}

fn store_auth_error(_: impl std::fmt::Debug) -> AuthError {
    AuthError::CredentialStoreError(STORAGE_FAILURE.into())
}

/// Every OAuth operation of a server reached over plain HTTP is constrained to
/// its exact listener. In particular metadata cannot redirect credential-bearing
/// registration/token operations to a public or different local endpoint.
#[derive(Clone)]
struct PlainOAuthHttp {
    endpoint: reqwest::Url,
    client: reqwest::Client,
}

fn plain_http_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(HTTP_TIMEOUT)
}

/// MCP owns request deadlines (including progress extensions) above the HTTP
/// client. An exchange-wide timeout here would also terminate healthy SSE
/// streams and long-running tools after the OAuth exchange deadline.
fn mcp_client(endpoint: &reqwest::Url) -> Result<reqwest::Client, String> {
    let builder = if is_plain_http(endpoint) {
        plain_http_builder()
    } else {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(HTTP_TIMEOUT)
    };
    builder
        .user_agent(concat!("maple/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(auth_error)
}

/// The sign-in manager for an endpoint, with the client its network rules need.
async fn oauth_manager(endpoint: &reqwest::Url) -> Result<AuthorizationManager, String> {
    if is_plain_http(endpoint) {
        let http = PlainOAuthHttp::new(endpoint.clone())?;
        let mut manager =
            AuthorizationManager::new_with_oauth_http_client(endpoint.as_str(), Arc::new(http))
                .await
                .map_err(auth_error)?;
        // A server that gives paths names no issuer. [`validate_metadata`]
        // still refuses one that gives neither paths nor an issuer.
        manager.set_allow_missing_issuer(true);
        Ok(manager)
    } else {
        AuthorizationManager::new(endpoint.as_str())
            .await
            .map_err(auth_error)
    }
}

async fn capped_body(response: &mut reqwest::Response) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(auth_error)? {
        if bytes.len().saturating_add(chunk.len()) > MAX_HTTP_BODY {
            return Err("OAuth response is too large".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

impl PlainOAuthHttp {
    fn new(endpoint: reqwest::Url) -> Result<Self, String> {
        let client = plain_http_builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(auth_error)?;
        Ok(Self { endpoint, client })
    }

    fn check_url(&self, url: &reqwest::Url) -> Result<(), AuthError> {
        if url.origin() != self.endpoint.origin()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(AuthError::MetadataError(
                "A plain HTTP sign-in must use its exact listener.".into(),
            ));
        }
        Ok(())
    }
}

impl OAuthHttpClient for PlainOAuthHttp {
    fn execute(&self, operation: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
        Box::pin(async move {
            let (parts, body) = operation.request.into_parts();
            let url = reqwest::Url::parse(&parts.uri.to_string())?;
            self.check_url(&url)?;
            let mut response = self
                .client
                .request(parts.method, url)
                .headers(parts.headers)
                .body(body)
                .send()
                .await?;
            let mut builder = http::Response::builder().status(response.status());
            *builder.headers_mut().ok_or("Invalid OAuth response")? = response.headers().clone();
            let bytes = capped_body(&mut response).await?;
            Ok(builder.body(bytes)?)
        })
    }
}

/// Fill in sign-in addresses a server gave as paths, and say whether it did.
/// Home Assistant does this when it is reached by a name it was not told, which
/// includes the `.local` name most people use. A path can only mean this server,
/// which then stands as its own issuer.
fn fill_in_paths(endpoint: &reqwest::Url, metadata: &mut AuthorizationMetadata) -> bool {
    let is_path = |value: &str| value.starts_with('/') && !value.starts_with("//");
    if metadata.issuer.is_some()
        || !is_path(&metadata.authorization_endpoint)
        || !is_path(&metadata.token_endpoint)
    {
        return false;
    }
    let (Ok(authorization), Ok(token)) = (
        endpoint.join(&metadata.authorization_endpoint),
        endpoint.join(&metadata.token_endpoint),
    ) else {
        return false;
    };
    metadata.authorization_endpoint = authorization.to_string();
    metadata.token_endpoint = token.to_string();
    metadata.issuer = Some(endpoint.origin().ascii_serialization());
    true
}

/// Whether a server may be known by no more than a client's address. Such a
/// server has no registration, and says so with this flag.
fn accepts_client_address(metadata: &AuthorizationMetadata) -> bool {
    metadata
        .additional_fields
        .get("client_id_metadata_document_supported")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// A server's sign-in metadata. rmcp refuses a private address named in
/// metadata whichever client it is given, so a plain HTTP server that
/// names itself by its address is read directly, from that address.
/// [`PlainOAuthHttp`] holds everything after this to the same listener.
async fn discover_metadata(
    manager: &AuthorizationManager,
    endpoint: &reqwest::Url,
) -> Result<AuthorizationMetadata, String> {
    let discovered = manager
        .resolve_metadata()
        .with_subscriber(tracing::subscriber::NoSubscriber::default())
        .await;
    match discovered {
        Ok(resolution) if resolution.source.is_discovered() => Ok(resolution.metadata),
        _ if is_plain_http(endpoint) => {
            let mut url = endpoint.clone();
            url.set_path("/.well-known/oauth-authorization-server");
            let mut response = plain_http_builder()
                .timeout(HTTP_TIMEOUT)
                .build()
                .map_err(auth_error)?
                .get(url)
                .send()
                .await
                .map_err(auth_error)?;
            if !response.status().is_success() {
                return Err(AUTH_FAILURE.into());
            }
            serde_json::from_slice(&capped_body(&mut response).await?).map_err(auth_error)
        }
        _ => Err(AUTH_FAILURE.into()),
    }
}

/// Whether an MCP server answers here, before asking the person to sign in.
/// Sign-in metadata alone does not show that: Home Assistant publishes it
/// whether or not its MCP integration is added.
async fn check_server_answers(endpoint: &reqwest::Url) -> Result<(), McpSignInError> {
    let response = mcp_client(endpoint)?
        .post(endpoint.clone())
        .header(http::header::CONTENT_TYPE, "application/json")
        .body("{}")
        .timeout(if is_plain_http(endpoint) {
            PLAIN_ANSWER_TIMEOUT
        } else {
            HTTP_TIMEOUT
        })
        .send()
        .await
        .map_err(|_| McpSignInError::Unreachable)?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(McpSignInError::NoServerHere);
    }
    Ok(())
}

/// Whether sign-in metadata, which the server chooses, may be followed: an
/// address on HTTPS, or on a plain HTTP server's own listener. This also keeps the
/// browser from being sent to any other kind of address.
fn check_sign_in_url(endpoint: &reqwest::Url, url: &reqwest::Url) -> Result<(), String> {
    let allowed = if is_plain_http(endpoint) {
        url.origin() == endpoint.origin()
    } else {
        url.scheme() == "https"
    };
    if !allowed || !url.username().is_empty() || url.password().is_some() {
        return Err(AUTH_FAILURE.into());
    }
    Ok(())
}

fn validate_metadata(
    endpoint: &reqwest::Url,
    metadata: &AuthorizationMetadata,
) -> Result<(), String> {
    for value in [
        Some(metadata.authorization_endpoint.as_str()),
        Some(metadata.token_endpoint.as_str()),
        metadata.registration_endpoint.as_deref(),
        metadata.issuer.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        let url = reqwest::Url::parse(value).map_err(auth_error)?;
        check_sign_in_url(endpoint, &url)?;
    }
    if metadata.issuer.is_none()
        || !metadata
            .code_challenge_methods_supported
            .as_ref()
            .is_some_and(|methods| methods.iter().any(|m| m == "S256"))
    {
        return Err("This server did not advertise the sign-in support Maple needs.".into());
    }
    Ok(())
}

/// Where an issuer publishes its own metadata: RFC 8414's place, then OpenID
/// Connect's two, which are the same place for an issuer with no path.
fn issuer_metadata_urls(issuer: &reqwest::Url) -> Vec<reqwest::Url> {
    let path = issuer.path().trim_end_matches('/');
    let mut urls: Vec<_> = [
        format!("/.well-known/oauth-authorization-server{path}"),
        format!("/.well-known/openid-configuration{path}"),
        format!("{path}/.well-known/openid-configuration"),
    ]
    .into_iter()
    .map(|path| {
        let mut url = issuer.clone();
        url.set_path(&path);
        url.set_query(None);
        url
    })
    .collect();
    urls.dedup();
    urls
}

/// Whether the issuer itself agrees with what a server said about it. rmcp only
/// checks an issuer when it found the metadata at that issuer's own address,
/// and a server can name a document elsewhere that merely claims to be one.
/// This read carries no credentials, and its answer is only ever compared.
async fn confirm_issuer(metadata: &AuthorizationMetadata) -> Result<(), String> {
    let issuer = metadata.issuer.as_deref().ok_or(AUTH_FAILURE)?;
    let issuer = reqwest::Url::parse(issuer).map_err(auth_error)?;
    let client = mcp_client(&issuer)?;
    for url in issuer_metadata_urls(&issuer) {
        let Ok(mut response) = client.get(url).timeout(HTTP_TIMEOUT).send().await else {
            continue;
        };
        if !response.status().is_success() {
            continue;
        }
        let Ok(body) = capped_body(&mut response).await else {
            continue;
        };
        let Ok(own) = serde_json::from_slice::<AuthorizationMetadata>(&body) else {
            continue;
        };
        if own.authorization_endpoint == metadata.authorization_endpoint
            && own.token_endpoint == metadata.token_endpoint
            && own.registration_endpoint == metadata.registration_endpoint
        {
            return Ok(());
        }
        break;
    }
    Err(AUTH_FAILURE.into())
}

/// What is saved for a sign-in: rmcp's credentials, and what they leave out
/// that a refresh needs. A server may issue a secret even to a public client,
/// and the refresh after a restart then has to present it. The metadata stays
/// as it was at sign-in, because a server can change what it advertises and a
/// refresh token must only go where the person signed in.
#[derive(Serialize, Deserialize)]
struct SavedSignIn {
    credentials: StoredCredentials,
    metadata: AuthorizationMetadata,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_secret: Option<String>,
    redirect_uri: String,
}

fn saved_secret(saved: &SavedSignIn) -> Result<SecretString, AuthError> {
    let value = serde_json::to_string(saved).map_err(store_auth_error)?;
    if value.len() > MAX_CREDENTIAL_BYTES {
        return Err(store_auth_error("oversized credentials"));
    }
    Ok(SecretString::new(value))
}

fn decode_saved(record: &McpOAuthCredentials) -> Result<SavedSignIn, AuthError> {
    if record.payload.expose().len() > MAX_CREDENTIAL_BYTES {
        return Err(store_auth_error("oversized credentials"));
    }
    serde_json::from_str(record.payload.expose()).map_err(store_auth_error)
}

/// The sign-ins in progress, so a person can stop one.
static SIGN_INS: OnceLock<Mutex<HashMap<String, CancellationToken>>> = OnceLock::new();

fn sign_ins() -> std::sync::MutexGuard<'static, HashMap<String, CancellationToken>> {
    SIGN_INS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// One sign-in in progress, forgotten when it ends, however it ends.
pub(super) struct RunningSignIn(String);

impl RunningSignIn {
    pub(super) fn start(key: String) -> (Self, CancellationToken) {
        let cancel = CancellationToken::new();
        sign_ins().insert(key.clone(), cancel.clone());
        (Self(key), cancel)
    }
}

impl Drop for RunningSignIn {
    fn drop(&mut self) {
        sign_ins().remove(&self.0);
    }
}

type RefreshLocks = HashMap<String, Weak<tokio::sync::Mutex<()>>>;
static REFRESH_LOCKS: OnceLock<Mutex<RefreshLocks>> = OnceLock::new();

fn refresh_lock(key: String) -> Arc<tokio::sync::Mutex<()>> {
    let mut locks = REFRESH_LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    locks.retain(|_, lock| lock.strong_count() != 0);
    if let Some(existing) = locks.get(&key).and_then(Weak::upgrade) {
        return existing;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    lock
}

/// This context never crosses GUI IPC. A task receives only its own host store
/// and account, and the credential adapter additionally captures an auth-row
/// generation so a late refresh cannot resurrect a disconnected connection.
#[derive(Clone)]
pub(crate) struct OAuthAccountContext {
    store: Arc<Store>,
    account: AccountId,
    owner: Arc<super::super::store::TaskStore>,
}

impl OAuthAccountContext {
    pub(crate) fn new(owner: Arc<super::super::store::TaskStore>) -> Self {
        Self {
            store: owner.store().clone(),
            account: owner.account(),
            owner,
        }
    }

    fn ensure_current(&self) -> Result<(), AuthError> {
        self.owner.ensure_current().map_err(store_auth_error)
    }

    fn record_for(
        &self,
        key: &str,
        target: &McpTarget,
    ) -> Result<Option<McpOAuthCredentials>, String> {
        self.ensure_current()
            .map_err(|_| STORAGE_FAILURE.to_string())?;
        // ACP may supply transient HTTP bindings that were never saved in this
        // host's catalog. They retain their ordinary static-header path.
        let definitions = self
            .store
            .mcp_definitions(self.account)
            .map_err(|_| STORAGE_FAILURE.to_string())?;
        let Some(definition) = definitions
            .into_iter()
            .find(|definition| definition.key == key)
        else {
            return Ok(None);
        };
        if &definition.target != target {
            if self
                .store
                .mcp_oauth_managed(self.account, key, &definition.target)
                .map_err(|_| STORAGE_FAILURE.to_string())?
            {
                return Err(STORAGE_FAILURE.into());
            }
            return Ok(None);
        }
        let record = self
            .store
            .mcp_oauth_credentials(self.account, key, target)
            .map_err(|_| STORAGE_FAILURE.to_string())?;
        if record.is_none()
            && self
                .store
                .mcp_oauth_managed(self.account, key, target)
                .map_err(|_| STORAGE_FAILURE.to_string())?
        {
            return Err(SIGNED_OUT.into());
        }
        Ok(record)
    }

    pub(crate) fn active_generation(
        &self,
        key: &str,
        endpoint: &str,
    ) -> Result<Option<String>, String> {
        let target = McpTarget::StreamableHttp {
            url: endpoint.into(),
        };
        let record = self.record_for(key, &target)?;
        if let Some(record) = record {
            return Ok(Some(record.generation));
        }
        Ok(None)
    }
}

#[derive(Clone)]
struct HostCredentialStore {
    context: OAuthAccountContext,
    key: String,
    target: McpTarget,
    generation: String,
    current: Arc<tokio::sync::Mutex<McpOAuthCredentials>>,
    refresh: Arc<tokio::sync::Mutex<()>>,
}

impl HostCredentialStore {
    fn new(
        context: OAuthAccountContext,
        key: &str,
        target: McpTarget,
        record: McpOAuthCredentials,
    ) -> Self {
        let lock_key = format!(
            "{:p}:{:?}:{key}:{:?}:{}",
            Arc::as_ptr(&context.store),
            context.account,
            target,
            record.generation
        );
        Self {
            context,
            key: key.into(),
            target,
            generation: record.generation.clone(),
            current: Arc::new(tokio::sync::Mutex::new(record)),
            refresh: refresh_lock(lock_key),
        }
    }
}

#[async_trait]
impl CredentialStore for HostCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        self.context.ensure_current()?;
        let record = self
            .context
            .store
            .mcp_oauth_credentials(self.context.account, &self.key, &self.target)
            .map_err(store_auth_error)?;
        let Some(record) = record else {
            return Err(store_auth_error("authorization removed"));
        };
        if record.generation != self.generation {
            return Err(store_auth_error("authorization replaced"));
        }
        let saved = decode_saved(&record)?;
        let mut current = self.current.lock().await;
        self.context.ensure_current()?;
        *current = record;
        Ok(Some(saved.credentials))
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        self.context.ensure_current()?;
        let mut current = self.current.lock().await;
        self.context.ensure_current()?;
        let saved = decode_saved(&current)?;
        let payload = saved_secret(&SavedSignIn {
            credentials,
            ..saved
        })?;
        let next = self
            .context
            .store
            .rotate_mcp_oauth_credentials(
                self.context.account,
                &self.key,
                &self.target,
                &current,
                payload,
            )
            .map_err(store_auth_error)?;
        *current = next;
        Ok(())
    }

    async fn clear(&self) -> Result<(), AuthError> {
        self.context.ensure_current()?;
        let current = self.current.lock().await;
        self.context.ensure_current()?;
        self.context
            .store
            .clear_mcp_oauth_credentials(self.context.account, &self.key, &self.target, &current)
            .map_err(store_auth_error)?;
        Ok(())
    }

    async fn acquire_refresh_guard(&self) -> Result<Option<CredentialRefreshGuard>, AuthError> {
        Ok(Some(CredentialRefreshGuard::new(
            self.refresh.clone().lock_owned().await,
        )))
    }
}

/// rmcp's OAuth module logs token exchange details at DEBUG. Scope every call
/// into that module to a no-op subscriber, including later runtime refreshes.
/// Normal Maple diagnostics remain available; codes/tokens never become logs.
#[derive(Clone)]
pub(crate) struct QuietAuthClient {
    inner: AuthClient<reqwest::Client>,
    endpoint: reqwest::Url,
    generation: Option<String>,
    credentials: Option<HostCredentialStore>,
    token_operation: Arc<tokio::sync::Mutex<()>>,
}

impl QuietAuthClient {
    pub(crate) fn generation(&self) -> Option<String> {
        self.generation.clone()
    }

    async fn clear_rejected(&self) -> Result<(), AuthError> {
        if let Some(credentials) = &self.credentials {
            credentials.clear().await?;
        }
        Ok(())
    }

    async fn clear_rejected_token(&self, rejected: &str) -> Result<(), AuthError> {
        let _operation = self.token_operation.lock().await;
        let Some(credentials) = &self.credentials else {
            return Ok(());
        };
        credentials.context.ensure_current()?;
        let current = credentials
            .context
            .store
            .mcp_oauth_credentials(
                credentials.context.account,
                &credentials.key,
                &credentials.target,
            )
            .map_err(store_auth_error)?;
        let Some(current) = current else {
            return Ok(());
        };
        if current.generation != credentials.generation {
            return Ok(());
        }
        let payload: serde_json::Value =
            serde_json::from_str(current.payload.expose()).map_err(store_auth_error)?;
        let actual = payload
            .get("credentials")
            .and_then(|credentials| credentials.get("token_response"))
            .and_then(|response| response.get("access_token"))
            .and_then(serde_json::Value::as_str);
        if actual == Some(rejected) {
            // A delayed verdict about an older token must never clear one
            // another task has since refreshed. The final CAS also fences a
            // concurrent explicit reconnect or store mutation.
            credentials
                .context
                .store
                .clear_mcp_oauth_credentials(
                    credentials.context.account,
                    &credentials.key,
                    &credentials.target,
                    &current,
                )
                .map_err(store_auth_error)?;
        }
        Ok(())
    }

    /// Separate managers belonging to loaded tasks share this lock. Check
    /// current persisted expiry/token *inside* it, so a waiter uses a token
    /// another task has already refreshed instead of rotating it again.
    async fn access_token(&self, rejected: Option<&str>) -> Result<String, AuthError> {
        let _operation = self.token_operation.lock().await;
        let manager = self.inner.auth_manager.lock().await;
        let result = async {
            let current = manager.get_access_token().await?;
            if rejected == Some(current.as_str()) {
                manager.refresh_token().await?;
                manager.get_access_token().await
            } else {
                Ok(current)
            }
        }
        .with_subscriber(tracing::subscriber::NoSubscriber::default())
        .await;
        match result {
            Err(AuthError::AuthorizationRequired | AuthError::TokenRefreshRejected(_)) => {
                self.clear_rejected().await?;
                Err(AuthError::AuthorizationRequired)
            }
            other => other,
        }
    }

    async fn authorized<T, F, Fut>(&self, call: F) -> Result<T, StreamableHttpError<reqwest::Error>>
    where
        F: Fn(Option<String>) -> Fut,
        Fut: std::future::Future<Output = Result<T, StreamableHttpError<reqwest::Error>>>,
    {
        let sent = self.access_token(None).await.map_err(sanitized_auth)?;
        match call(Some(sent.clone())).await {
            Err(StreamableHttpError::AuthRequired(_)) => {
                let refreshed = self
                    .access_token(Some(&sent))
                    .await
                    .map_err(sanitized_auth)?;
                match call(Some(refreshed.clone())).await {
                    Err(StreamableHttpError::AuthRequired(_)) => {
                        self.clear_rejected_token(&refreshed)
                            .await
                            .map_err(sanitized_auth)?;
                        Err(sanitized_auth(AuthError::AuthorizationRequired))
                    }
                    result => result.map_err(sanitized_transport),
                }
            }
            result => result.map_err(sanitized_transport),
        }
    }

    fn check_uri(&self, uri: &str) -> Result<(), StreamableHttpError<reqwest::Error>> {
        if uri != self.endpoint.as_str() {
            return Err(StreamableHttpError::UnexpectedServerResponse(
                "The server's address changed".into(),
            ));
        }
        Ok(())
    }
}

fn sanitized_auth(error: AuthError) -> StreamableHttpError<reqwest::Error> {
    match error {
        AuthError::AuthorizationRequired | AuthError::TokenRefreshRejected(_) => {
            StreamableHttpError::Auth(AuthError::AuthorizationRequired)
        }
        _ => StreamableHttpError::Auth(AuthError::AuthorizationFailed(AUTH_FAILURE.into())),
    }
}

fn sanitized_transport(
    error: StreamableHttpError<reqwest::Error>,
) -> StreamableHttpError<reqwest::Error> {
    match error {
        StreamableHttpError::Auth(error) => sanitized_auth(error),
        StreamableHttpError::AuthRequired(_) => sanitized_auth(AuthError::AuthorizationRequired),
        StreamableHttpError::InsufficientScope(_) => StreamableHttpError::Auth(
            AuthError::InvalidScope("The server did not approve the requested scope.".into()),
        ),
        StreamableHttpError::UnexpectedServerResponse(_) => {
            StreamableHttpError::UnexpectedServerResponse(
                "The server returned an unexpected response.".into(),
            )
        }
        StreamableHttpError::Client(error) => StreamableHttpError::Client(error.without_url()),
        _ => StreamableHttpError::UnexpectedServerResponse("The MCP request failed.".into()),
    }
}

impl StreamableHttpClient for QuietAuthClient {
    type Error = reqwest::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        self.post_message_with_max_sse_event_size(
            uri,
            message,
            session_id,
            auth_header,
            custom_headers,
            4 * 1024 * 1024,
        )
        .await
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max: usize,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        self.check_uri(&uri)?;
        if auth_header.is_some() {
            return Err(sanitized_auth(AuthError::AuthorizationRequired));
        }
        self.authorized(|token| {
            self.inner.http_client.post_message_with_max_sse_event_size(
                uri.clone(),
                message.clone(),
                session_id.clone(),
                token,
                custom_headers.clone(),
                max,
            )
        })
        .await
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        self.check_uri(&uri)?;
        if auth_header.is_some() {
            return Err(sanitized_auth(AuthError::AuthorizationRequired));
        }
        self.authorized(|token| {
            self.inner.http_client.delete_session(
                uri.clone(),
                session_id.clone(),
                token,
                custom_headers.clone(),
            )
        })
        .await
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        self.get_stream_with_max_sse_event_size(
            uri,
            session_id,
            last_event_id,
            auth_header,
            custom_headers,
            4 * 1024 * 1024,
        )
        .await
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max: usize,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        self.check_uri(&uri)?;
        if auth_header.is_some() {
            return Err(sanitized_auth(AuthError::AuthorizationRequired));
        }
        self.authorized(|token| {
            self.inner.http_client.get_stream_with_max_sse_event_size(
                uri.clone(),
                session_id.clone(),
                last_event_id.clone(),
                token,
                custom_headers.clone(),
                max,
            )
        })
        .await
    }
}

pub(crate) async fn runtime_client(
    context: &OAuthAccountContext,
    key: &str,
    value: &str,
) -> Result<Option<QuietAuthClient>, String> {
    let target = McpTarget::StreamableHttp { url: value.into() };
    let Some(record) = context.record_for(key, &target)? else {
        return Ok(None);
    };
    let saved = decode_saved(&record).map_err(auth_error)?;
    let endpoint = sign_in_endpoint(value).map_err(|error| error.to_string())?;
    let mut manager = oauth_manager(&endpoint).await?;
    manager.set_metadata(saved.metadata);
    let generation = Some(record.generation.clone());
    let token_operation = refresh_lock(format!(
        "token-operation:{:p}:{:?}:{key}:{}",
        Arc::as_ptr(&context.store),
        context.account,
        record.generation
    ));
    let credentials = HostCredentialStore::new(context.clone(), key, target, record);
    manager.set_credential_store(credentials.clone());
    if !manager
        .initialize_from_store()
        .with_subscriber(tracing::subscriber::NoSubscriber::default())
        .await
        .map_err(auth_error)?
    {
        return Err(AUTH_FAILURE.into());
    }
    if let Some(secret) = saved.client_secret {
        manager
            .configure_client(
                OAuthClientConfig::new(saved.credentials.client_id, saved.redirect_uri)
                    .with_client_secret(secret),
            )
            .map_err(auth_error)?;
    }
    Ok(Some(QuietAuthClient {
        inner: AuthClient::new(mcp_client(&endpoint)?, manager),
        endpoint,
        generation,
        credentials: Some(credentials),
        token_operation,
    }))
}

struct Callback {
    code: String,
    state: String,
    issuer: Option<String>,
}

fn parse_callback(
    request: &str,
    callback: &reqwest::Url,
    expected_state: &str,
) -> Result<Callback, String> {
    let mut lines = request.split("\r\n");
    let parts: Vec<_> = lines
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .collect();
    if parts.len() != 3 || parts[0] != "GET" || parts[2] != "HTTP/1.1" {
        return Err(AUTH_FAILURE.into());
    }
    let mut hosts = lines
        .filter_map(|line| line.split_once(':'))
        .filter(|(key, _)| key.eq_ignore_ascii_case("host"));
    let Some((_, host)) = hosts.next() else {
        return Err(AUTH_FAILURE.into());
    };
    if hosts.next().is_some()
        || host.trim() != format!("127.0.0.1:{}", callback.port().unwrap_or_default())
    {
        return Err(AUTH_FAILURE.into());
    }
    let url = callback.join(parts[1]).map_err(auth_error)?;
    if url.origin() != callback.origin()
        || url.path() != callback.path()
        || url.fragment().is_some()
    {
        return Err(AUTH_FAILURE.into());
    }
    let mut values = HashMap::new();
    for (key, value) in url.query_pairs() {
        if values
            .insert(key.into_owned(), value.into_owned())
            .is_some()
        {
            return Err(AUTH_FAILURE.into());
        }
    }
    let state = values
        .remove("state")
        .ok_or_else(|| AUTH_FAILURE.to_string())?;
    if state != expected_state {
        return Err(AUTH_FAILURE.into());
    }
    if values.remove("error").is_some() {
        return Err(NOT_APPROVED.into());
    }
    let code = values
        .remove("code")
        .filter(|code| !code.is_empty() && code.len() <= 4096)
        .ok_or_else(|| AUTH_FAILURE.to_string())?;
    Ok(Callback {
        code,
        state,
        issuer: values.remove("iss"),
    })
}

async fn receive_callback(
    listener: TcpListener,
    callback: &reqwest::Url,
    state: &str,
) -> Result<Callback, String> {
    loop {
        let (mut stream, peer) = listener.accept().await.map_err(auth_error)?;
        if !peer.ip().is_loopback() {
            continue;
        }
        let parsed = tokio::time::timeout(
            Duration::from_secs(5),
            read_callback(&mut stream, callback, state),
        )
        .await;
        // The code is not redeemed yet, so this cannot promise a connection.
        let reply = match &parsed {
            Ok(Ok(_)) => sign_in_page_response(
                "200 OK",
                "Return to Maple Work",
                "Maple Work is finishing the connection. You can close this tab.",
            ),
            _ => sign_in_page_response(
                "400 Bad Request",
                "Sign-in could not finish",
                "Return to Maple Work and try again. You can close this tab.",
            ),
        };
        let _ =
            tokio::time::timeout(Duration::from_secs(1), stream.write_all(reply.as_bytes())).await;
        match parsed {
            Ok(Ok(callback)) => return Ok(callback),
            Ok(Err(error)) if error == NOT_APPROVED => return Err(error),
            _ => {}
        }
    }
}

async fn read_callback(
    stream: &mut TcpStream,
    callback: &reqwest::Url,
    state: &str,
) -> Result<Callback, String> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 1024];
    while bytes.len() < CALLBACK_BYTES {
        let count = stream.read(&mut chunk).await.map_err(auth_error)?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.len() > CALLBACK_BYTES {
            return Err(AUTH_FAILURE.into());
        }
        if bytes.windows(4).any(|w| w == b"\r\n\r\n") {
            let request = std::str::from_utf8(&bytes).map_err(auth_error)?;
            return parse_callback(request, callback, state);
        }
    }
    Err(AUTH_FAILURE.into())
}

impl AgentRuntimeHandle {
    /// Sign in to an HTTP MCP server in the browser, confirm the server takes
    /// the result, and save it, on for new tasks. Signing in to a server
    /// already signed in replaces its sign-in and leaves it on or off as it
    /// was. Only public configuration and presence DTOs return to the desktop.
    pub async fn sign_in_mcp_server(
        &self,
        request: McpSignIn,
        open_browser: McpBrowser,
    ) -> Result<Vec<McpDefinition>, McpSignInError> {
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let endpoint = sign_in_endpoint(&request.url)?;
        let mut edit = McpServerEdit {
            key: super::name_to_key(request.name.trim()),
            name: request.name.trim().to_string(),
            description: request.description.trim().to_string(),
            enabled: true,
            timeout_seconds: super::super::types::default_mcp_timeout_seconds(),
            target: McpTarget::StreamableHttp {
                url: endpoint.to_string(),
            },
            environment: Vec::new(),
            headers: Vec::new(),
        };
        let proposed = super::normalize_mcp_servers(vec![super::edit_to_validation_server(&edit)])?;
        super::super::integrations::validate_new_mcp_integration_collisions(&[], &proposed)?;
        let (store, account) =
            account_store(self.paths(), &self.user_id).map_err(|_| STORAGE_FAILURE.to_string())?;
        let previous = store
            .mcp_definitions(account)
            .map_err(|_| STORAGE_FAILURE.to_string())?
            .into_iter()
            .find(|definition| definition.key == edit.key);
        edit.enabled = previous.as_ref().is_none_or(|previous| previous.enabled);
        if let Some(previous) = &previous
            && !store
                .mcp_oauth_managed(account, &edit.key, &previous.target)
                .map_err(|_| STORAGE_FAILURE.to_string())?
        {
            return Err(format!(
                "A server named {} already exists. Remove or rename it before signing in.",
                previous.name
            )
            .into());
        }
        let lock = refresh_lock(format!("setup:{:p}:{account:?}:{}", store, edit.key));
        let _guard = lock
            .try_lock()
            .map_err(|_| format!("Sign-in to {} is already in progress.", edit.name))?;
        let (_running, cancel) = RunningSignIn::start(self.sign_in_key(&edit.key));
        let attempt = tokio::time::timeout(AUTH_TIMEOUT, async {
            check_server_answers(&endpoint).await?;
            let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .await
                .map_err(auth_error)?;
            let port = listener.local_addr().map_err(auth_error)?.port();
            let callback = reqwest::Url::parse(&format!(
                "http://127.0.0.1:{port}/oauth/callback/{}",
                uuid::Uuid::new_v4()
            ))
            .map_err(auth_error)?;
            let mut manager = oauth_manager(&endpoint).await?;
            let memory = InMemoryCredentialStore::new();
            manager.set_credential_store(memory.clone());
            let mut metadata = discover_metadata(&manager, &endpoint).await?;
            let filled_in = fill_in_paths(&endpoint, &mut metadata);
            validate_metadata(&endpoint, &metadata)?;
            let registers = metadata.registration_endpoint.is_some();
            if !registers && !accepts_client_address(&metadata) {
                return Err(NEEDS_REGISTERED_APP.into());
            }
            // Paths came from this server and stay on it, so there is no other
            // issuer to ask.
            if !filled_in {
                confirm_issuer(&metadata).await?;
            }
            manager.set_metadata(metadata.clone());
            let scopes = if request.scopes.is_empty() {
                manager.select_scopes(None, &[])
            } else {
                request.scopes.clone()
            };
            let scope_names: Vec<&str> = scopes.iter().map(String::as_str).collect();
            let (client_id, client_secret) = if registers {
                // Registered here rather than by the session, which keeps what
                // the server issues to itself: a refresh after a restart needs it.
                let client = manager
                    .register_client(CLIENT_NAME, callback.as_str(), &scope_names)
                    .with_subscriber(tracing::subscriber::NoSubscriber::default())
                    .await
                    .map_err(|_| CLIENT_REFUSED.to_string())?;
                (client.client_id, client.client_secret)
            } else {
                // The callback's own origin names Maple, so the server can check
                // the way back without fetching anything from this computer.
                (format!("http://127.0.0.1:{port}/"), None)
            };
            let mut authorization = AuthorizationRequest::new(callback.as_str())
                .with_preregistered_client(client_id)
                .with_scopes(scopes);
            if let Some(secret) = &client_secret {
                authorization = authorization.with_client_secret(secret.clone());
            }
            let session = AuthorizationSession::new(manager, authorization)
                .with_subscriber(tracing::subscriber::NoSubscriber::default())
                .await
                .map_err(|(_, error)| auth_error(error))?;
            let authorization = reqwest::Url::parse(&session.auth_url).map_err(auth_error)?;
            check_sign_in_url(&endpoint, &authorization)?;
            let expected_state = authorization
                .query_pairs()
                .find(|(key, _)| key == "state")
                .map(|(_, value)| value.into_owned())
                .ok_or_else(|| AUTH_FAILURE.to_string())?;
            self.verify_generation().await?;
            self.ensure_accepting_new_work()?;
            open_browser(authorization.to_string())?;
            let answer = receive_callback(listener, &callback, &expected_state).await?;
            self.verify_generation().await?;
            session
                .handle_callback_with_issuer(&answer.code, &answer.state, answer.issuer.as_deref())
                .with_subscriber(tracing::subscriber::NoSubscriber::default())
                .await
                .map_err(auth_error)?;
            let quiet = QuietAuthClient {
                inner: AuthClient::new(mcp_client(&endpoint)?, session.auth_manager),
                endpoint: endpoint.clone(),
                generation: None,
                credentials: None,
                token_operation: Arc::new(tokio::sync::Mutex::new(())),
            };
            // Confirm the actual authenticated MCP server before publishing a
            // definition. Listing its tools proves the server took the token:
            // on the stateless revision opening alone may not need it.
            let transport = StreamableHttpClientTransport::with_client(
                quiet,
                StreamableHttpClientTransportConfig::with_uri(endpoint.as_str()),
            );
            let mut service = tokio::time::timeout(
                HTTP_TIMEOUT,
                ().serve_with_lifecycle(transport, super::connection::http_lifecycle()),
            )
            .await
            .map_err(auth_error)?
            .map_err(auth_error)?;
            let tools = service
                .peer()
                .peer_info()
                .is_some_and(|info| info.capabilities.tools.is_some());
            let confirmed = if tools {
                tokio::time::timeout(HTTP_TIMEOUT, service.peer().list_all_tools())
                    .await
                    .map_err(auth_error)
                    .and_then(|listed| listed.map(|_| ()).map_err(auth_error))
            } else {
                Ok(())
            };
            let _ = service.close_with_timeout(Duration::from_millis(500)).await;
            confirmed?;
            let credentials = memory
                .load()
                .await
                .map_err(auth_error)?
                .ok_or_else(|| AUTH_FAILURE.to_string())?;
            let payload = saved_secret(&SavedSignIn {
                credentials,
                metadata,
                client_secret,
                redirect_uri: callback.to_string(),
            })
            .map_err(auth_error)?;
            let _settings = self.lock_settings().await;
            self.verify_generation().await?;
            self.ensure_accepting_new_work()?;
            store
                .install_mcp_oauth(account, previous.as_ref(), &edit, payload)
                .map_err(|_| STORAGE_FAILURE.to_string())?;
            store
                .mcp_definitions(account)
                .map_err(|_| McpSignInError::from(STORAGE_FAILURE))
        });
        tokio::select! {
            result = attempt => result.map_err(|_| "Sign-in timed out. Try again.".to_string())?,
            _ = cancel.cancelled() => Err(SIGN_IN_CANCELLED.into()),
        }
    }

    /// Stop a sign-in in progress, which closes its callback listener.
    pub fn cancel_mcp_sign_in(&self, name: &str) {
        let key = self.sign_in_key(&super::name_to_key(name.trim()));
        if let Some(cancel) = sign_ins().get(&key) {
            cancel.cancel();
        }
    }

    pub(super) fn sign_in_key(&self, key: &str) -> String {
        let root = self.paths().generated_resource_root.display();
        format!("{root}:{}:{key}", self.user_id)
    }
}

#[cfg(test)]
mod tests;
