//! OAuth for a Vana server on this computer. No owner key, public relay, or
//! developer grant service participates in this client flow.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::stream::BoxStream;
use http::{HeaderName, HeaderValue};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ClientJsonRpcMessage};
use rmcp::transport::auth::{
    AuthClient, AuthError, AuthorizationManager, AuthorizationMetadata, AuthorizationRequest,
    AuthorizationSession, CredentialRefreshGuard, CredentialStore, InMemoryCredentialStore,
    OAuthHttpClient, OAuthHttpClientFuture, OAuthHttpRequest, StoredCredentials,
};
use rmcp::transport::streamable_http_client::{
    SseError, StreamableHttpClient, StreamableHttpClientTransport,
    StreamableHttpClientTransportConfig, StreamableHttpError, StreamableHttpPostResponse,
};
use sse_stream::Sse;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::instrument::WithSubscriber;

use super::super::AgentRuntimeHandle;
use super::super::config::account_store;
use crate::store::records::{
    McpDefinition, McpOAuthCredentials, McpServerEdit, McpTarget, SecretString,
};
use crate::store::{AccountId, Store};

const AUTH_TIMEOUT: Duration = Duration::from_secs(180);
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_HTTP_BODY: usize = 1024 * 1024;
const MAX_CREDENTIAL_BYTES: usize = 64 * 1024;
const CALLBACK_BYTES: usize = 8 * 1024;
const AUTH_FAILURE: &str = "Vana sign-in did not complete. Check Vana Desktop and try again.";
const STORAGE_FAILURE: &str =
    "Vana credentials changed or their local store is unavailable. Connect Vana again.";

/// The browser launcher is injected by the local host, never by MCP output.
pub type LocalMcpBrowser = Arc<dyn Fn(String) -> Result<(), String> + Send + Sync>;

pub(crate) fn local_endpoint(value: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(value.trim()).map_err(|_| endpoint_error())?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || url.port().is_none_or(|port| port == 0)
        || url.path() != "/mcp"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(endpoint_error());
    }
    Ok(url)
}

fn endpoint_error() -> String {
    "Copy the Local MCP endpoint from Vana Desktop Settings → Server. It must be http://127.0.0.1:<port>/mcp.".into()
}

fn auth_error(_: impl std::fmt::Debug) -> String {
    // OAuth/server errors can contain codes, request bodies and callback URLs.
    AUTH_FAILURE.into()
}

fn store_auth_error(_: impl std::fmt::Debug) -> AuthError {
    AuthError::CredentialStoreError(STORAGE_FAILURE.into())
}

/// Every OAuth operation is constrained to the exact local listener. In
/// particular metadata cannot redirect credential-bearing registration/token
/// operations to a public or different loopback endpoint.
#[derive(Clone)]
struct LocalOAuthHttp {
    endpoint: reqwest::Url,
    client: reqwest::Client,
}

fn local_http_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(HTTP_TIMEOUT)
}

/// MCP owns request deadlines (including progress extensions) above the HTTP
/// client. An exchange-wide timeout here would also terminate healthy SSE
/// streams and long-running tools after the OAuth exchange deadline.
fn local_mcp_client() -> Result<reqwest::Client, String> {
    local_http_builder().build().map_err(auth_error)
}

impl LocalOAuthHttp {
    fn new(endpoint: reqwest::Url) -> Result<Self, String> {
        let client = local_http_builder()
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
                "Vana OAuth must use its exact local listener.".into(),
            ));
        }
        Ok(())
    }
}

impl OAuthHttpClient for LocalOAuthHttp {
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
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                if bytes.len().saturating_add(chunk.len()) > MAX_HTTP_BODY {
                    return Err("Vana OAuth response is too large".into());
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(builder.body(bytes)?)
        })
    }
}

fn validate_metadata(
    http: &LocalOAuthHttp,
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
        http.check_url(&url).map_err(auth_error)?;
    }
    if metadata.issuer.is_none()
        || metadata.registration_endpoint.is_none()
        || !metadata
            .code_challenge_methods_supported
            .as_ref()
            .is_some_and(|methods| methods.iter().any(|m| m == "S256"))
    {
        return Err(
            "Vana's local endpoint did not advertise the required OAuth and PKCE support.".into(),
        );
    }
    Ok(())
}

fn credentials_secret(credentials: &StoredCredentials) -> Result<SecretString, AuthError> {
    let value = serde_json::to_string(credentials).map_err(store_auth_error)?;
    if value.len() > MAX_CREDENTIAL_BYTES {
        return Err(store_auth_error("oversized credentials"));
    }
    Ok(SecretString::new(value))
}

fn decode_credentials(record: &McpOAuthCredentials) -> Result<StoredCredentials, AuthError> {
    if record.payload.expose().len() > MAX_CREDENTIAL_BYTES {
        return Err(store_auth_error("oversized credentials"));
    }
    serde_json::from_str(record.payload.expose()).map_err(store_auth_error)
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
            return Err("Vana access was disconnected. Connect Vana again in Integrations.".into());
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
        let credentials = decode_credentials(&record)?;
        let mut current = self.current.lock().await;
        self.context.ensure_current()?;
        *current = record;
        Ok(Some(credentials))
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        self.context.ensure_current()?;
        let mut current = self.current.lock().await;
        self.context.ensure_current()?;
        let next = self
            .context
            .store
            .rotate_mcp_oauth_credentials(
                self.context.account,
                &self.key,
                &self.target,
                &current,
                credentials_secret(&credentials)?,
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
            .get("token_response")
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
                "Vana target changed".into(),
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
            AuthError::InvalidScope("Vana did not approve the requested scope.".into()),
        ),
        StreamableHttpError::UnexpectedServerResponse(_) => {
            StreamableHttpError::UnexpectedServerResponse(
                "Vana returned an unexpected response.".into(),
            )
        }
        StreamableHttpError::Client(error) => StreamableHttpError::Client(error.without_url()),
        _ => {
            StreamableHttpError::UnexpectedServerResponse("Vana's local MCP request failed.".into())
        }
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
    let endpoint = local_endpoint(value)?;
    let http = LocalOAuthHttp::new(endpoint.clone())?;
    let mut manager =
        AuthorizationManager::new_with_oauth_http_client(endpoint.as_str(), Arc::new(http.clone()))
            .await
            .map_err(auth_error)?;
    let resolution = manager
        .resolve_metadata()
        .with_subscriber(tracing::subscriber::NoSubscriber::default())
        .await
        .map_err(auth_error)?;
    if !resolution.source.is_discovered() {
        return Err(AUTH_FAILURE.into());
    }
    validate_metadata(&http, &resolution.metadata)?;
    manager.set_metadata(resolution.metadata);
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
    Ok(Some(QuietAuthClient {
        inner: AuthClient::new(local_mcp_client()?, manager),
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
        return Err("Vana access was not approved.".into());
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
        let (status, text) = match &parsed {
            Ok(Ok(_)) => (
                "200 OK",
                "Sign-in response received. You can return to Maple.",
            ),
            _ => (
                "400 Bad Request",
                "This sign-in response was not accepted. Return to Vana or Maple.",
            ),
        };
        let reply = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{text}",
            text.len()
        );
        let _ =
            tokio::time::timeout(Duration::from_secs(1), stream.write_all(reply.as_bytes())).await;
        match parsed {
            Ok(Ok(callback)) => return Ok(callback),
            Ok(Err(error)) if error == "Vana access was not approved." => return Err(error),
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
    /// Authorize the copied local endpoint through Vana's own consent UI.
    /// Only public configuration/presence DTOs return to the desktop.
    pub async fn connect_vana_local(
        &self,
        endpoint: String,
        open_browser: LocalMcpBrowser,
    ) -> Result<Vec<McpDefinition>, String> {
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let endpoint = local_endpoint(&endpoint)?;
        let (store, account) =
            account_store(self.paths(), &self.user_id).map_err(|_| STORAGE_FAILURE.to_string())?;
        let previous = store
            .mcp_definitions(account)
            .map_err(|_| STORAGE_FAILURE.to_string())?
            .into_iter()
            .find(|d| d.key == "vana");
        if let Some(previous) = &previous
            && !store
                .mcp_oauth_managed(account, "vana", &previous.target)
                .map_err(|_| STORAGE_FAILURE.to_string())?
        {
            return Err("A custom server named vana already exists. Remove or rename it before connecting Vana.".into());
        }
        let lock = refresh_lock(format!("setup:{:p}:{account:?}", store));
        let _guard = lock
            .try_lock()
            .map_err(|_| "A Vana connection is already in progress.".to_string())?;
        tokio::time::timeout(AUTH_TIMEOUT, async {
            let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .await
                .map_err(auth_error)?;
            let port = listener.local_addr().map_err(auth_error)?.port();
            let callback = reqwest::Url::parse(&format!(
                "http://127.0.0.1:{port}/oauth/callback/{}",
                uuid::Uuid::new_v4()
            ))
            .map_err(auth_error)?;
            let http = LocalOAuthHttp::new(endpoint.clone())?;
            let mut manager = AuthorizationManager::new_with_oauth_http_client(
                endpoint.as_str(),
                Arc::new(http.clone()),
            )
            .await
            .map_err(auth_error)?;
            let memory = InMemoryCredentialStore::new();
            manager.set_credential_store(memory.clone());
            let resolution = manager
                .resolve_metadata()
                .with_subscriber(tracing::subscriber::NoSubscriber::default())
                .await
                .map_err(auth_error)?;
            if !resolution.source.is_discovered() {
                return Err(AUTH_FAILURE.into());
            }
            validate_metadata(&http, &resolution.metadata)?;
            manager.set_metadata(resolution.metadata);
            let request = AuthorizationRequest::new(callback.as_str())
                .with_client_name("Maple Agent")
                .with_scopes(["vana:read"]);
            let session = AuthorizationSession::new(manager, request)
                .with_subscriber(tracing::subscriber::NoSubscriber::default())
                .await
                .map_err(|(_, error)| auth_error(error))?;
            let authorization = reqwest::Url::parse(&session.auth_url).map_err(auth_error)?;
            http.check_url(&authorization).map_err(auth_error)?;
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
            let client = QuietAuthClient {
                inner: AuthClient::new(local_mcp_client()?, session.auth_manager),
                endpoint: endpoint.clone(),
                generation: None,
                credentials: None,
                token_operation: Arc::new(tokio::sync::Mutex::new(())),
            };
            // Verify the actual authenticated MCP server before publishing a definition.
            let transport = StreamableHttpClientTransport::with_client(
                client,
                StreamableHttpClientTransportConfig::with_uri(endpoint.as_str()),
            );
            let mut service = tokio::time::timeout(HTTP_TIMEOUT, ().serve(transport))
                .await
                .map_err(auth_error)?
                .map_err(auth_error)?;
            let probe = tokio::time::timeout(
                HTTP_TIMEOUT,
                service
                    .peer()
                    // Vana's MCP SDK validates this tool's empty object schema;
                    // omitted arguments are not equivalent to an empty object.
                    .call_tool(
                        CallToolRequestParams::new("list_granted_scopes")
                            .with_arguments(serde_json::Map::new()),
                    ),
            )
            .await;
            let _ = service.close_with_timeout(Duration::from_millis(500)).await;
            let result = probe.map_err(auth_error)?.map_err(auth_error)?;
            if result.is_error == Some(true) {
                return Err("Vana did not confirm the approved scopes.".into());
            }
            let credentials = memory
                .load()
                .await
                .map_err(auth_error)?
                .ok_or_else(|| AUTH_FAILURE.to_string())?;
            let _settings = self.lock_settings().await;
            self.verify_generation().await?;
            self.ensure_accepting_new_work()?;
            let edit = McpServerEdit {
                key: "vana".into(),
                name: "vana".into(),
                description: "Personal context from Vana on this computer".into(),
                enabled: false,
                timeout_seconds: 30,
                target: McpTarget::StreamableHttp {
                    url: endpoint.to_string(),
                },
                environment: Vec::new(),
                headers: Vec::new(),
            };
            store
                .install_mcp_oauth(
                    account,
                    previous.as_ref(),
                    &edit,
                    credentials_secret(&credentials).map_err(auth_error)?,
                )
                .map_err(|_| STORAGE_FAILURE.to_string())?;
            store
                .mcp_definitions(account)
                .map_err(|_| STORAGE_FAILURE.to_string())
        })
        .await
        .map_err(|_| "Vana sign-in timed out. Open Vana Desktop and connect again.".to_string())?
    }
}

#[cfg(test)]
mod tests;
