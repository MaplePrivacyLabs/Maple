use crate::provider_registry::ProviderId;
use crate::proxy_config::ProxyConfig;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use reqwest::{Method, Request as ReqwestRequest};
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

mod diagnostics;
pub use diagnostics::UpstreamDiagnostic;

type StandardHttpClient = reqwest::Client;
type TinfoilRefreshTask = JoinHandle<Result<SecureClientSnapshot, ProviderRequestError>>;

const TINFOIL_INITIAL_RETRY_DELAY: Duration = Duration::from_secs(1);
const TINFOIL_MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
const TINFOIL_DISCOVERY_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(30);
const TINFOIL_UNINITIALIZED_BASE_URL: &str = "https://in-process.tinfoil.invalid";

#[derive(Debug, thiserror::Error)]
pub enum ProviderClientError {
    #[error("failed to build provider HTTP client: {0}")]
    Client(#[from] reqwest::Error),

    #[error("failed to install the required Ring crypto provider")]
    CryptoProvider,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ProviderRequestError {
    #[error(
        "Tinfoil is temporarily unavailable while router discovery and attestation retry in the background"
    )]
    TinfoilUnavailable,

    #[error("provider response start timed out after {0:?}")]
    Timeout(Duration),

    #[error("failed to build provider request: {0}")]
    Build(String),

    #[error("failed to connect to provider: {0}")]
    Connect(String),

    #[error("provider request failed: {0}")]
    Send(String),

    #[error("provider returned HTTP {}", .0.status)]
    Upstream(UpstreamProviderError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamProviderError {
    pub status: u16,
    pub retry_after: Option<Duration>,
    pub upstream_request_id: Option<String>,
    pub diagnostic: Option<UpstreamDiagnostic>,
}

pub(crate) struct ProviderSendTrace {
    pub(crate) prior_failures: Vec<ProviderRequestError>,
    pub(crate) result: Result<ProviderResponse, ProviderRequestError>,
}

impl ProviderSendTrace {
    fn terminal(result: Result<ProviderResponse, ProviderRequestError>) -> Self {
        Self {
            prior_failures: Vec::new(),
            result,
        }
    }
}

pub enum ProviderResponse {
    Tinfoil(reqwest::Response),
    Standard(reqwest::Response),
}

#[derive(Clone)]
pub struct ProviderRequest<'a> {
    method: Method,
    path: &'a str,
    source_headers: Option<&'a HeaderMap>,
    content_type: Option<&'a str>,
    body: Option<Bytes>,
    response_start_timeout: Duration,
}

impl<'a> ProviderRequest<'a> {
    pub fn new(method: Method, path: &'a str, response_start_timeout: Duration) -> Self {
        Self {
            method,
            path,
            source_headers: None,
            content_type: None,
            body: None,
            response_start_timeout,
        }
    }

    pub fn source_headers(mut self, source_headers: &'a HeaderMap) -> Self {
        self.source_headers = Some(source_headers);
        self
    }

    pub fn content_type(mut self, content_type: &'a str) -> Self {
        self.content_type = Some(content_type);
        self
    }

    pub fn body(mut self, body: impl Into<Bytes>) -> Self {
        self.body = Some(body.into());
        self
    }
}

impl ProviderResponse {
    pub fn is_success(&self) -> bool {
        match self {
            Self::Tinfoil(response) => response.status().is_success(),
            Self::Standard(response) => response.status().is_success(),
        }
    }

    pub fn status_code(&self) -> u16 {
        match self {
            Self::Tinfoil(response) => response.status().as_u16(),
            Self::Standard(response) => response.status().as_u16(),
        }
    }

    pub fn content_type(&self) -> Option<&str> {
        match self {
            Self::Tinfoil(response) => response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Self::Standard(response) => response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
        }
    }

    pub fn header_str(&self, name: &HeaderName) -> Option<&str> {
        match self {
            Self::Tinfoil(response) => response.headers().get(name),
            Self::Standard(response) => response.headers().get(name),
        }
        .and_then(|value| value.to_str().ok())
    }

    pub async fn bytes(self) -> Result<Bytes, String> {
        match self {
            Self::Tinfoil(response) => response.bytes().await.map_err(|error| error.to_string()),
            Self::Standard(response) => response.bytes().await.map_err(|error| error.to_string()),
        }
    }

    /// Best-effort metadata only: body reads are bounded independently of the
    /// inference timeout and arbitrary provider text never leaves the reader.
    pub async fn error_diagnostic(self) -> UpstreamDiagnostic {
        diagnostics::read_error_diagnostic(self.bytes_stream()).await
    }

    pub fn bytes_stream(
        self,
    ) -> Pin<Box<dyn Stream<Item = Result<Bytes, String>> + Send + 'static>> {
        match self {
            Self::Tinfoil(response) => Box::pin(
                response
                    .bytes_stream()
                    .map(|result| result.map_err(|e| e.to_string())),
            ),
            Self::Standard(response) => Box::pin(
                response
                    .bytes_stream()
                    .map(|result| result.map_err(|e| e.to_string())),
            ),
        }
    }
}

fn map_reqwest_send_error(error: reqwest::Error) -> ProviderRequestError {
    if error.is_connect() {
        ProviderRequestError::Connect(error.to_string())
    } else {
        ProviderRequestError::Send(error.to_string())
    }
}

#[derive(Clone)]
struct SecureClientSnapshot {
    client: Arc<tinfoil::Client>,
    refresh_attempt: u64,
}

struct SecureClientSlot {
    client: Option<Arc<tinfoil::Client>>,
    refresh_attempt: u64,
}

struct SecureTinfoilTransport {
    api_key: String,
    client: RwLock<SecureClientSlot>,
    refresh_gate: Mutex<()>,
    initialization_started: AtomicBool,
}

struct InitializationClaim {
    transport: Arc<SecureTinfoilTransport>,
    active: bool,
}

impl InitializationClaim {
    fn new(transport: Arc<SecureTinfoilTransport>) -> Self {
        Self {
            transport,
            active: true,
        }
    }

    fn release(&mut self) {
        if self.active {
            self.transport
                .initialization_started
                .store(false, Ordering::Release);
            self.active = false;
        }
    }
}

impl Drop for InitializationClaim {
    fn drop(&mut self) {
        self.release();
    }
}

#[derive(Clone)]
enum TinfoilAttempt {
    Secure(SecureClientSnapshot),
    #[cfg(test)]
    Plain {
        client: reqwest::Client,
        base_url: String,
        api_key: String,
    },
}

#[derive(Clone)]
enum TinfoilTransport {
    Secure(Arc<SecureTinfoilTransport>),
    #[cfg(test)]
    Plain {
        client: reqwest::Client,
        base_url: String,
        api_key: String,
    },
}

#[derive(Debug, Eq, PartialEq)]
enum RefreshDecision {
    Refresh,
    UseCurrent,
    PreviousAttemptFailed,
}

fn refresh_decision(
    failed_attempt: u64,
    current_attempt: u64,
    same_client: bool,
) -> RefreshDecision {
    if !same_client {
        RefreshDecision::UseCurrent
    } else if failed_attempt == current_attempt {
        RefreshDecision::Refresh
    } else {
        RefreshDecision::PreviousAttemptFailed
    }
}

fn next_retry_delay(current: Duration) -> Duration {
    current.saturating_mul(2).min(TINFOIL_MAX_RETRY_DELAY)
}

async fn run_tinfoil_discovery_attempt_with_timeout<F, T>(
    attempt: F,
    attempt_timeout: Duration,
) -> Result<T, tokio::time::error::Elapsed>
where
    F: Future<Output = T>,
{
    tokio::time::timeout(attempt_timeout, attempt).await
}

fn spawn_owned_recovery<F, T>(recovery: F) -> JoinHandle<T>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    tokio::spawn(recovery)
}

fn spawn_tinfoil_refresh_after_connect_failure(
    transport: Arc<SecureTinfoilTransport>,
    failed: SecureClientSnapshot,
) -> TinfoilRefreshTask {
    // The owned recovery boundary intentionally accepts no ProviderRequest or
    // body. If the caller's response timeout drops its JoinHandle, Tokio
    // detaches this task and recovery still updates the shared transport state.
    spawn_owned_recovery(async move { transport.refresh_after_connect_failure(&failed).await })
}

impl SecureTinfoilTransport {
    fn read_client(&self) -> RwLockReadGuard<'_, SecureClientSlot> {
        self.client
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write_client(&self) -> RwLockWriteGuard<'_, SecureClientSlot> {
        self.client
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn snapshot(&self) -> Option<SecureClientSnapshot> {
        let slot = self.read_client();
        slot.client.as_ref().map(|client| SecureClientSnapshot {
            client: Arc::clone(client),
            refresh_attempt: slot.refresh_attempt,
        })
    }

    fn claim_background_initialization(&self) -> bool {
        self.initialization_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn start_background_initialization(self: &Arc<Self>) {
        if self.snapshot().is_some() {
            return;
        }
        if !self.claim_background_initialization() {
            return;
        }

        let transport = Arc::clone(self);
        // Construct the claim before spawning so cancellation before the task's
        // first poll still drops it and makes initialization claimable again.
        let mut claim = InitializationClaim::new(Arc::clone(&transport));
        tokio::spawn(async move {
            transport.initialize_until_ready(&mut claim).await;
        });
    }

    async fn initialize_until_ready(&self, claim: &mut InitializationClaim) {
        let mut retry_delay = TINFOIL_INITIAL_RETRY_DELAY;

        loop {
            // The gate is shared with certificate-rotation refreshes. This keeps
            // discovery and attestation single-flight even if initialization and
            // a request race as the first client becomes available.
            let refresh_guard = self.refresh_gate.lock().await;
            if self.snapshot().is_some() {
                claim.release();
                return;
            }

            let discovery = run_tinfoil_discovery_attempt_with_timeout(
                tinfoil::Client::new_default_with_api_key(self.api_key.clone()),
                TINFOIL_DISCOVERY_ATTEMPT_TIMEOUT,
            )
            .await;
            match discovery {
                Ok(Ok(client)) => {
                    let mut slot = self.write_client();
                    slot.client = Some(Arc::new(client));
                    slot.refresh_attempt = slot.refresh_attempt.wrapping_add(1);
                    // Release the claim while the slot is still write-locked.
                    // A request can only observe the new client after another
                    // initializer is allowed to claim recovery ownership.
                    claim.release();
                    tracing::info!("Tinfoil router discovery and attestation succeeded");
                    return;
                }
                Ok(Err(error)) => {
                    tracing::warn!(
                        retry_delay_seconds = retry_delay.as_secs(),
                        error = %error,
                        "Tinfoil router discovery or attestation failed; retrying in background"
                    );
                }
                Err(_) => {
                    tracing::warn!(
                        attempt_timeout_seconds = TINFOIL_DISCOVERY_ATTEMPT_TIMEOUT.as_secs(),
                        retry_delay_seconds = retry_delay.as_secs(),
                        "Tinfoil router discovery or attestation timed out; retrying in background"
                    );
                }
            }
            drop(refresh_guard);

            tokio::time::sleep(retry_delay).await;
            retry_delay = next_retry_delay(retry_delay);
        }
    }

    fn try_refresh_gate(&self) -> Result<tokio::sync::MutexGuard<'_, ()>, ProviderRequestError> {
        self.refresh_gate
            .try_lock()
            .map_err(|_| ProviderRequestError::TinfoilUnavailable)
    }

    async fn refresh_after_connect_failure(
        self: &Arc<Self>,
        failed: &SecureClientSnapshot,
    ) -> Result<SecureClientSnapshot, ProviderRequestError> {
        // Never retain a request (and potentially a large request body) in a
        // mutex wait queue. One failure-wave owner attempts refresh; concurrent
        // callers receive an immediate, retryable 503.
        let refresh_guard = self.try_refresh_gate()?;
        let decision = {
            let current = self.read_client();
            refresh_decision(
                failed.refresh_attempt,
                current.refresh_attempt,
                current
                    .client
                    .as_ref()
                    .is_some_and(|client| Arc::ptr_eq(&failed.client, client)),
            )
        };

        match decision {
            RefreshDecision::UseCurrent => {
                return self
                    .snapshot()
                    .ok_or(ProviderRequestError::TinfoilUnavailable);
            }
            RefreshDecision::PreviousAttemptFailed => {
                return Err(ProviderRequestError::TinfoilUnavailable);
            }
            RefreshDecision::Refresh => {}
        }

        // Router discovery is preferable to re-verifying only the stale host:
        // a rotation may publish a replacement router as well as a new pin.
        let refreshed = run_tinfoil_discovery_attempt_with_timeout(
            tinfoil::Client::new_default_with_api_key(self.api_key.clone()),
            TINFOIL_DISCOVERY_ATTEMPT_TIMEOUT,
        )
        .await;
        let mut slot = self.write_client();
        slot.refresh_attempt = slot.refresh_attempt.wrapping_add(1);
        match refreshed {
            Ok(Ok(client)) => {
                slot.client = Some(Arc::new(client));
                Ok(SecureClientSnapshot {
                    client: Arc::clone(slot.client.as_ref().expect("client was just installed")),
                    refresh_attempt: slot.refresh_attempt,
                })
            }
            Ok(Err(error)) => {
                tracing::warn!(
                    error = %error,
                    "Tinfoil router refresh failed; transitioning unavailable"
                );
                slot.client = None;
                drop(slot);
                drop(refresh_guard);
                self.start_background_initialization();
                Err(ProviderRequestError::TinfoilUnavailable)
            }
            Err(_) => {
                tracing::warn!(
                    attempt_timeout_seconds = TINFOIL_DISCOVERY_ATTEMPT_TIMEOUT.as_secs(),
                    "Tinfoil router refresh timed out; transitioning unavailable"
                );
                slot.client = None;
                drop(slot);
                drop(refresh_guard);
                self.start_background_initialization();
                Err(ProviderRequestError::TinfoilUnavailable)
            }
        }
    }
}

impl TinfoilTransport {
    fn snapshot(&self) -> Result<TinfoilAttempt, ProviderRequestError> {
        match self {
            Self::Secure(transport) => transport
                .snapshot()
                .map(TinfoilAttempt::Secure)
                .ok_or(ProviderRequestError::TinfoilUnavailable),
            #[cfg(test)]
            Self::Plain {
                client,
                base_url,
                api_key,
            } => Ok(TinfoilAttempt::Plain {
                client: client.clone(),
                base_url: base_url.clone(),
                api_key: api_key.clone(),
            }),
        }
    }

    fn start_background_initialization(&self) {
        match self {
            Self::Secure(transport) => transport.start_background_initialization(),
            #[cfg(test)]
            Self::Plain { .. } => {}
        }
    }

    #[cfg(not(test))]
    fn secure(&self) -> &Arc<SecureTinfoilTransport> {
        let Self::Secure(transport) = self;
        transport
    }

    #[cfg(test)]
    fn secure(&self) -> &Arc<SecureTinfoilTransport> {
        match self {
            Self::Secure(transport) => transport,
            Self::Plain { .. } => unreachable!("secure attempt must use secure transport"),
        }
    }
}

/// Shared outbound provider clients.
///
/// Tinfoil discovery and attestation retry in the background so an external
/// Tinfoil control-plane outage cannot block backend startup or non-Tinfoil
/// routes. Once attested, TLS pinning and upstream connection pooling are shared
/// by every hot-path request. The ordinary client is retained for
/// Continuum/OpenAI-compatible providers.
#[derive(Clone)]
pub struct ProviderClient {
    tinfoil: TinfoilTransport,
    standard: StandardHttpClient,
}

impl ProviderClient {
    pub async fn new(tinfoil_api_key: String) -> Result<Self, ProviderClientError> {
        crate::http_client::install_ring_crypto_provider()
            .map_err(|_| ProviderClientError::CryptoProvider)?;

        let secure_transport = Arc::new(SecureTinfoilTransport {
            api_key: tinfoil_api_key,
            client: RwLock::new(SecureClientSlot {
                client: None,
                refresh_attempt: 0,
            }),
            refresh_gate: Mutex::new(()),
            initialization_started: AtomicBool::new(false),
        });
        secure_transport.start_background_initialization();

        Ok(Self {
            tinfoil: TinfoilTransport::Secure(secure_transport),
            standard: standard_http_client()?,
        })
    }

    #[cfg(test)]
    fn uninitialized_for_test() -> Result<Self, ProviderClientError> {
        crate::http_client::install_ring_crypto_provider()
            .map_err(|_| ProviderClientError::CryptoProvider)?;

        Ok(Self {
            tinfoil: TinfoilTransport::Secure(Arc::new(SecureTinfoilTransport {
                api_key: "unused-test-key".to_string(),
                client: RwLock::new(SecureClientSlot {
                    client: None,
                    refresh_attempt: 0,
                }),
                refresh_gate: Mutex::new(()),
                // Keep this deterministic: tests exercise the unready behavior
                // without creating a real discovery request.
                initialization_started: AtomicBool::new(true),
            })),
            standard: standard_http_client()?,
        })
    }

    #[cfg(test)]
    pub fn for_test(base_url: String) -> Result<Self, ProviderClientError> {
        crate::http_client::install_ring_crypto_provider()
            .map_err(|_| ProviderClientError::CryptoProvider)?;
        let client = test_http_client()?;

        Ok(Self {
            tinfoil: TinfoilTransport::Plain {
                client,
                base_url,
                api_key: "test-tinfoil-key".to_string(),
            },
            standard: standard_http_client()?,
        })
    }

    pub fn tinfoil_base_url(&self) -> String {
        match &self.tinfoil {
            TinfoilTransport::Secure(transport) => transport
                .snapshot()
                .map(|snapshot| format!("https://{}", snapshot.client.enclave()))
                .unwrap_or_else(|| TINFOIL_UNINITIALIZED_BASE_URL.to_string()),
            #[cfg(test)]
            TinfoilTransport::Plain { base_url, .. } => base_url.clone(),
        }
    }

    pub async fn send(
        &self,
        provider: &ProxyConfig,
        request: ProviderRequest<'_>,
    ) -> Result<ProviderResponse, ProviderRequestError> {
        self.send_traced(provider, request).await.result
    }

    pub(crate) async fn send_traced(
        &self,
        provider: &ProxyConfig,
        request: ProviderRequest<'_>,
    ) -> ProviderSendTrace {
        if provider.provider_name != ProviderId::Tinfoil.as_str() {
            return ProviderSendTrace::terminal(self.send_standard(provider, request).await);
        }

        let response_start_timeout = request.response_start_timeout;
        // Construction already starts this loop. Calling it here also makes the
        // request path self-healing if construction behavior changes later;
        // the atomic claim prevents discovery herds.
        self.tinfoil.start_background_initialization();
        let attempt = match self.tinfoil.snapshot() {
            Ok(attempt) => attempt,
            Err(error) => return ProviderSendTrace::terminal(Err(error)),
        };
        let first_request =
            match self.build_tinfoil_request_for_attempt(provider, request.clone(), &attempt) {
                Ok(request) => request,
                Err(error) => return ProviderSendTrace::terminal(Err(error)),
            };

        let prior_failures = Arc::new(std::sync::Mutex::new(Vec::new()));
        let prior_failures_for_send = Arc::clone(&prior_failures);

        let send = async {
            match attempt {
                TinfoilAttempt::Secure(failed_snapshot) => {
                    let http_client = failed_snapshot
                        .client
                        .http_client()
                        .map_err(|error| ProviderRequestError::Send(error.to_string()))?;
                    match http_client.execute(first_request).await {
                        Ok(response) => Ok(response),
                        // reqwest's typed connect class is limited to connector setup
                        // (DNS/TCP/TLS). In particular, the SDK's rustls pin mismatch
                        // arrives here before any HTTP request bytes can be written.
                        // Request/body/timeout failures are deliberately not replayed by this
                        // certificate-rotation layer.
                        Err(error) if error.is_connect() => {
                            prior_failures_for_send
                                .lock()
                                .expect("provider attempt trace mutex poisoned")
                                .push(ProviderRequestError::Connect(error.to_string()));
                            let refreshed = spawn_tinfoil_refresh_after_connect_failure(
                                Arc::clone(self.tinfoil.secure()),
                                failed_snapshot.clone(),
                            )
                            .await
                            // A task panic/cancellation is an availability
                            // failure; do not expose JoinError internals.
                            .map_err(|_| ProviderRequestError::TinfoilUnavailable)??;
                            let retry_attempt = TinfoilAttempt::Secure(refreshed.clone());
                            let retry_request = self.build_tinfoil_request_for_attempt(
                                provider,
                                request,
                                &retry_attempt,
                            )?;
                            refreshed
                                .client
                                .http_client()
                                .map_err(|error| ProviderRequestError::Send(error.to_string()))?
                                .execute(retry_request)
                                .await
                                .map_err(|error| {
                                    if error.is_connect() {
                                        ProviderRequestError::Connect(error.to_string())
                                    } else {
                                        ProviderRequestError::Send(error.to_string())
                                    }
                                })
                        }
                        Err(error) => Err(if error.is_connect() {
                            ProviderRequestError::Connect(error.to_string())
                        } else {
                            ProviderRequestError::Send(error.to_string())
                        }),
                    }
                }
                #[cfg(test)]
                TinfoilAttempt::Plain { client, .. } => client
                    .execute(first_request)
                    .await
                    .map_err(map_reqwest_send_error),
            }
        };

        let result = match tokio::time::timeout(response_start_timeout, send).await {
            Ok(result) => result.map(ProviderResponse::Tinfoil),
            Err(_) => Err(ProviderRequestError::Timeout(response_start_timeout)),
        };
        let prior_failures = prior_failures
            .lock()
            .expect("provider attempt trace mutex poisoned")
            .clone();

        ProviderSendTrace {
            prior_failures,
            result,
        }
    }

    #[cfg(test)]
    fn build_tinfoil_request(
        &self,
        provider: &ProxyConfig,
        request: ProviderRequest<'_>,
    ) -> Result<ReqwestRequest, ProviderRequestError> {
        self.build_tinfoil_request_for_attempt(
            provider,
            request,
            &self
                .tinfoil
                .snapshot()
                .expect("test transport is initialized"),
        )
    }

    fn build_tinfoil_request_for_attempt(
        &self,
        provider: &ProxyConfig,
        request: ProviderRequest<'_>,
        attempt: &TinfoilAttempt,
    ) -> Result<ReqwestRequest, ProviderRequestError> {
        debug_assert_eq!(provider.provider_name, ProviderId::Tinfoil.as_str());
        let ProviderRequest {
            method,
            path,
            source_headers,
            content_type,
            body,
            ..
        } = request;
        let (base_url, api_key) = match attempt {
            TinfoilAttempt::Secure(snapshot) => (
                format!("https://{}", snapshot.client.enclave()),
                snapshot.client.secure_client().api_key(),
            ),
            #[cfg(test)]
            TinfoilAttempt::Plain {
                base_url, api_key, ..
            } => (base_url.clone(), api_key.as_str()),
        };

        let url = format!(
            "{}/{}",
            base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        );
        let url = reqwest::Url::parse(&url)
            .map_err(|error| ProviderRequestError::Build(error.to_string()))?;
        let mut request = ReqwestRequest::new(method, url);

        if let Some(headers) = source_headers {
            *request.headers_mut() = forwarded_headers(headers);
        }
        if let Some(content_type) = content_type {
            let content_type = HeaderValue::from_str(content_type)
                .map_err(|error| ProviderRequestError::Build(error.to_string()))?;
            request
                .headers_mut()
                .insert(header::CONTENT_TYPE, content_type);
        }
        if !api_key.is_empty() {
            let mut authorization = HeaderValue::from_str(&format!("Bearer {api_key}"))
                .map_err(|error| ProviderRequestError::Build(error.to_string()))?;
            authorization.set_sensitive(true);
            request
                .headers_mut()
                .insert(header::AUTHORIZATION, authorization);
        }
        if let Some(body) = body {
            *request.body_mut() = Some(body.into());
        }

        Ok(request)
    }

    async fn send_standard(
        &self,
        provider: &ProxyConfig,
        request: ProviderRequest<'_>,
    ) -> Result<ProviderResponse, ProviderRequestError> {
        let ProviderRequest {
            method,
            path,
            source_headers,
            content_type,
            body,
            response_start_timeout,
        } = request;
        let url = format!(
            "{}/{}",
            provider.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        );
        let url = reqwest::Url::parse(&url)
            .map_err(|error| ProviderRequestError::Build(error.to_string()))?;
        let mut headers = source_headers.map(forwarded_headers).unwrap_or_default();

        if let Some(content_type) = content_type {
            let content_type = HeaderValue::from_str(content_type)
                .map_err(|error| ProviderRequestError::Build(error.to_string()))?;
            headers.insert(header::CONTENT_TYPE, content_type);
        }
        if let Some(api_key) = provider.api_key.as_deref().filter(|key| !key.is_empty()) {
            let mut authorization = HeaderValue::from_str(&format!("Bearer {api_key}"))
                .map_err(|error| ProviderRequestError::Build(error.to_string()))?;
            authorization.set_sensitive(true);
            headers.insert(header::AUTHORIZATION, authorization);
        }
        let mut request = self.standard.request(method, url).headers(headers);
        if let Some(body) = body {
            request = request.body(body);
        }

        let response = tokio::time::timeout(response_start_timeout, request.send())
            .await
            .map_err(|_| ProviderRequestError::Timeout(response_start_timeout))?
            .map_err(map_reqwest_send_error)?;
        Ok(ProviderResponse::Standard(response))
    }
}

#[cfg(test)]
fn test_http_client() -> Result<reqwest::Client, reqwest::Error> {
    crate::http_client::client_builder()
        // Plain transport exists only for loopback contract tests. Supplying an
        // empty trust store keeps client construction independent of host CA
        // files while ensuring an accidental HTTPS test request trusts no CA.
        .tls_certs_only(std::iter::empty::<reqwest::Certificate>())
        .pool_idle_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(10)
        .build()
}

fn standard_http_client() -> Result<StandardHttpClient, reqwest::Error> {
    crate::http_client::client_builder()
        // Provider responses are untrusted. In particular, following a 307 or
        // 308 could replay a prompt or audio body to an arbitrary Location.
        .redirect(reqwest::redirect::Policy::none())
        // Preserve the previous direct-connection behavior. The enclave's
        // plaintext loopback provider traffic must not be rerouted by inherited
        // HTTP_PROXY/HTTPS_PROXY/ALL_PROXY environment variables.
        .no_proxy()
        .pool_idle_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(10)
        .build()
}

fn skipped_forward_headers(source: &HeaderMap) -> HashSet<HeaderName> {
    let mut skipped = HashSet::from([
        header::AUTHORIZATION,
        header::CONNECTION,
        header::CONTENT_LENGTH,
        header::CONTENT_TYPE,
        header::HOST,
        header::PROXY_AUTHENTICATE,
        header::PROXY_AUTHORIZATION,
        header::TE,
        header::TRAILER,
        header::TRANSFER_ENCODING,
        header::UPGRADE,
    ]);
    skipped.insert(HeaderName::from_static("keep-alive"));
    skipped.insert(HeaderName::from_static("proxy-connection"));

    for value in source.get_all(header::CONNECTION) {
        for token in value.as_bytes().split(|byte| *byte == b',') {
            let token = trim_ascii_whitespace(token);
            if !token.is_empty() {
                if let Ok(name) = HeaderName::from_bytes(token) {
                    skipped.insert(name);
                }
            }
        }
    }

    skipped
}

fn forwarded_headers(source: &HeaderMap) -> HeaderMap {
    let skipped = skipped_forward_headers(source);

    let mut forwarded = HeaderMap::new();
    for (name, value) in source {
        if !skipped.contains(name) {
            forwarded.append(name.clone(), value.clone());
        }
    }
    forwarded
}

fn trim_ascii_whitespace(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(|byte| byte.is_ascii_whitespace()) {
        value = &value[1..];
    }
    while value.last().is_some_and(|byte| byte.is_ascii_whitespace()) {
        value = &value[..value.len() - 1];
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderValue, StatusCode, Uri};
    use axum::{routing::any, Router};
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::sync::mpsc;

    fn tinfoil_proxy() -> ProxyConfig {
        ProxyConfig {
            base_url: "http://ignored.invalid".to_string(),
            api_key: None,
            provider_name: ProviderId::Tinfoil.as_str().to_string(),
        }
    }

    fn uninitialized_secure_transport() -> Arc<SecureTinfoilTransport> {
        Arc::new(SecureTinfoilTransport {
            api_key: "unused-test-key".to_string(),
            client: RwLock::new(SecureClientSlot {
                client: None,
                refresh_attempt: 0,
            }),
            refresh_gate: Mutex::new(()),
            initialization_started: AtomicBool::new(false),
        })
    }

    fn response_header<'a>(response: &'a ProviderResponse, name: &str) -> Option<&'a str> {
        match response {
            ProviderResponse::Tinfoil(response) => response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok()),
            ProviderResponse::Standard(response) => response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok()),
        }
    }

    fn evidence_dir() -> Option<PathBuf> {
        std::env::var_os("TINFOIL_PARITY_EVIDENCE_DIR").map(PathBuf::from)
    }

    fn write_evidence_file(name: &str, contents: impl AsRef<[u8]>) {
        let Some(dir) = evidence_dir() else {
            return;
        };
        std::fs::create_dir_all(&dir).expect("create parity evidence directory");
        std::fs::write(dir.join(name), contents).expect("write parity evidence file");
    }

    fn write_response_headers(name: &str, response: &ProviderResponse) {
        if evidence_dir().is_none() {
            return;
        }

        let mut lines = Vec::new();
        match response {
            ProviderResponse::Tinfoil(response) => {
                for (header_name, value) in response.headers() {
                    lines.push(format!(
                        "{}: {}",
                        header_name.as_str(),
                        value.to_str().unwrap_or("<non-UTF-8>")
                    ));
                }
            }
            ProviderResponse::Standard(response) => {
                for (header_name, value) in response.headers() {
                    lines.push(format!(
                        "{}: {}",
                        header_name.as_str(),
                        value.to_str().unwrap_or("<non-UTF-8>")
                    ));
                }
            }
        }
        lines.sort_unstable();
        let mut output = format!("HTTP {}\n", response.status_code());
        output.push_str(&lines.join("\n"));
        output.push('\n');
        write_evidence_file(name, output);
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    fn git_output(args: &[&str]) -> Option<String> {
        let output = Command::new("git").args(args).output().ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    #[test]
    fn provider_response_exposes_the_upstream_content_type() {
        let response = axum::http::Response::builder()
            .header("content-type", "audio/flac")
            .body(reqwest::Body::default())
            .unwrap()
            .into();

        assert_eq!(
            ProviderResponse::Standard(response).content_type(),
            Some("audio/flac")
        );
    }

    #[tokio::test]
    async fn both_provider_response_variants_preserve_only_safe_error_diagnostics() {
        for attested in [false, true] {
            let response = axum::http::Response::builder()
                .status(400)
                .body(reqwest::Body::from(
                    r#"{"error":{"code":"context_length_exceeded","type":"invalid_request_error","message":"private prompt and secret tool argument"}}"#,
                ))
                .unwrap()
                .into();
            let response = if attested {
                ProviderResponse::Tinfoil(response)
            } else {
                ProviderResponse::Standard(response)
            };
            let diagnostic = response.error_diagnostic().await;
            assert_eq!(diagnostic.code, Some("context_length_exceeded"));
            assert_eq!(diagnostic.error_type, Some("invalid_request_error"));
            let rendered = format!("{diagnostic:?}");
            assert!(!rendered.contains("private prompt"));
            assert!(!rendered.contains("secret tool argument"));
        }
    }

    #[test]
    fn tinfoil_request_matches_the_removed_go_proxy_upstream_contract() {
        let client = ProviderClient::for_test("https://router.example.test".to_string()).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer caller-secret"),
        );
        headers.insert(
            header::CONNECTION,
            HeaderValue::from_static("keep-alive, x-hop"),
        );
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("999"));
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        headers.insert("x-hop", HeaderValue::from_static("remove-me"));
        headers.insert("x-request-id", HeaderValue::from_static("request-123"));

        let body = br#"{"model":"glm-5-3","messages":[],"stream":true}"#.to_vec();
        let request = client
            .build_tinfoil_request(
                &tinfoil_proxy(),
                ProviderRequest::new(
                    Method::POST,
                    "/v1/chat/completions?trace=1",
                    Duration::from_secs(5),
                )
                .source_headers(&headers)
                .content_type("application/json")
                .body(body.clone()),
            )
            .unwrap();

        assert_eq!(request.method(), Method::POST);
        assert_eq!(
            request.url().as_str(),
            "https://router.example.test/v1/chat/completions?trace=1"
        );
        assert_eq!(
            request.headers().get(header::AUTHORIZATION).unwrap(),
            "Bearer test-tinfoil-key"
        );
        assert_eq!(
            request.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        assert_eq!(
            request.headers().get("x-request-id").unwrap(),
            "request-123"
        );
        assert!(!request.headers().contains_key(header::CONNECTION));
        assert!(!request.headers().contains_key("x-hop"));
        assert_eq!(
            request.body().and_then(|body| body.as_bytes()),
            Some(body.as_slice())
        );
    }

    #[test]
    fn forwarded_headers_strip_hop_by_hop_fields() {
        let mut source = HeaderMap::new();
        source.insert("keep-alive", HeaderValue::from_static("timeout=5"));
        source.insert("proxy-connection", HeaderValue::from_static("keep-alive"));
        source.insert(
            header::PROXY_AUTHORIZATION,
            HeaderValue::from_static("Basic caller-proxy-secret"),
        );
        source.insert(header::TE, HeaderValue::from_static("trailers"));
        source.insert(header::TRAILER, HeaderValue::from_static("x-checksum"));
        source.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        source.insert("x-safe", HeaderValue::from_static("yes"));

        let forwarded = forwarded_headers(&source);

        assert_eq!(forwarded.get("x-safe").unwrap(), "yes");
        assert!(!forwarded.contains_key("keep-alive"));
        assert!(!forwarded.contains_key("proxy-connection"));
        assert!(!forwarded.contains_key(header::PROXY_AUTHORIZATION));
        assert!(!forwarded.contains_key(header::TE));
        assert!(!forwarded.contains_key(header::TRAILER));
        assert!(!forwarded.contains_key(header::UPGRADE));
    }

    #[test]
    fn refresh_decision_single_flights_success_and_failure() {
        assert_eq!(
            refresh_decision(4, 4, true),
            RefreshDecision::Refresh,
            "the first failure for a client generation performs discovery"
        );
        assert_eq!(
            refresh_decision(4, 5, false),
            RefreshDecision::UseCurrent,
            "waiters reuse the client installed by the first refresh"
        );
        assert_eq!(
            refresh_decision(4, 5, true),
            RefreshDecision::PreviousAttemptFailed,
            "waiters do not repeat a failed discovery for the same failure wave"
        );
    }

    #[test]
    fn background_initialization_is_single_flight_and_backoff_is_bounded() {
        let transport = uninitialized_secure_transport();

        assert!(transport.claim_background_initialization());
        let mut successful_claim = InitializationClaim::new(Arc::clone(&transport));
        assert!(
            !transport.claim_background_initialization(),
            "only one task may own discovery and attestation retries"
        );
        successful_claim.release();
        assert!(
            transport.claim_background_initialization(),
            "a successful initializer releases ownership"
        );
        let canceled_claim = InitializationClaim::new(Arc::clone(&transport));
        drop(canceled_claim);
        assert!(
            transport.claim_background_initialization(),
            "dropping a canceled initializer releases ownership"
        );
        assert_eq!(
            next_retry_delay(TINFOIL_INITIAL_RETRY_DELAY),
            Duration::from_secs(2)
        );
        assert_eq!(
            next_retry_delay(TINFOIL_MAX_RETRY_DELAY),
            TINFOIL_MAX_RETRY_DELAY
        );
    }

    #[tokio::test]
    async fn discovery_attempt_timeout_bounds_a_pending_future() {
        assert_eq!(TINFOIL_DISCOVERY_ATTEMPT_TIMEOUT, Duration::from_secs(30));
        let result = run_tinfoil_discovery_attempt_with_timeout(
            std::future::pending::<()>(),
            Duration::from_millis(5),
        )
        .await;

        assert!(result.is_err(), "a pending discovery attempt must time out");
    }

    #[tokio::test]
    async fn caller_timeout_does_not_cancel_owned_recovery() {
        let started = Arc::new(tokio::sync::Notify::new());
        let completed = Arc::new(AtomicBool::new(false));
        let task_started = Arc::clone(&started);
        let task_completed = Arc::clone(&completed);
        let recovery = spawn_owned_recovery(async move {
            task_started.notify_one();
            tokio::time::sleep(Duration::from_millis(25)).await;
            task_completed.store(true, Ordering::Release);
        });

        started.notified().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(1), recovery)
                .await
                .is_err(),
            "the caller-side waiter should time out first"
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            while !completed.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached recovery must continue after its JoinHandle is dropped");
    }

    #[test]
    fn refresh_task_capture_boundary_excludes_provider_requests() {
        let _spawn: fn(Arc<SecureTinfoilTransport>, SecureClientSnapshot) -> TinfoilRefreshTask =
            spawn_tinfoil_refresh_after_connect_failure;
    }

    #[test]
    fn refresh_gate_is_non_waiting_and_returns_typed_unavailable() {
        let transport = uninitialized_secure_transport();
        let owner = transport.refresh_gate.try_lock().unwrap();

        assert!(matches!(
            transport.try_refresh_gate(),
            Err(ProviderRequestError::TinfoilUnavailable)
        ));

        drop(owner);
        assert!(transport.try_refresh_gate().is_ok());
    }

    #[tokio::test]
    async fn unready_tinfoil_is_explicitly_unavailable_without_blocking() {
        let client = ProviderClient::uninitialized_for_test().unwrap();

        let result = tokio::time::timeout(
            Duration::from_millis(100),
            client.send(
                &tinfoil_proxy(),
                ProviderRequest::new(Method::GET, "/v1/models", Duration::from_secs(5)),
            ),
        )
        .await
        .expect("an unready Tinfoil route must fail immediately");

        assert!(matches!(
            result,
            Err(ProviderRequestError::TinfoilUnavailable)
        ));
        assert_eq!(client.tinfoil_base_url(), TINFOIL_UNINITIALIZED_BASE_URL);
    }

    #[test]
    fn only_tinfoil_unready_maps_to_service_unavailable() {
        use axum::response::IntoResponse;

        let unavailable =
            crate::ApiError::from(ProviderRequestError::TinfoilUnavailable).into_response();
        let ordinary_failure = crate::ApiError::from(ProviderRequestError::Send(
            "ordinary provider failure".to_string(),
        ))
        .into_response();

        assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(ordinary_failure.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn unready_tinfoil_does_not_block_standard_provider_routes() {
        let app = Router::new().route(
            "/v1/models",
            any(|| async { (StatusCode::OK, "standard-provider-ok") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = ProviderClient::uninitialized_for_test().unwrap();
        let provider = ProxyConfig {
            base_url: format!("http://{address}"),
            api_key: None,
            provider_name: ProviderId::Continuum.as_str().to_string(),
        };

        let response = client
            .send(
                &provider,
                ProviderRequest::new(Method::GET, "/v1/models", Duration::from_secs(5)),
            )
            .await
            .unwrap();
        server.abort();

        assert!(response.is_success());
        assert_eq!(response.bytes().await.unwrap(), "standard-provider-ok");
    }

    #[tokio::test]
    async fn standard_provider_redirect_does_not_replay_request() {
        let (target_tx, mut target_rx) = mpsc::channel(1);
        let target_app = Router::new().route(
            "/redirect-target",
            any(move |body: Bytes| {
                let target_tx = target_tx.clone();
                async move {
                    target_tx.send(body).await.unwrap();
                    StatusCode::OK
                }
            }),
        );
        let target_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_address = target_listener.local_addr().unwrap();
        let target_server =
            tokio::spawn(async move { axum::serve(target_listener, target_app).await.unwrap() });

        let redirect_location = format!("http://{target_address}/redirect-target");
        let redirect_app = Router::new().route(
            "/v1/chat/completions",
            any(move || {
                let redirect_location = redirect_location.clone();
                async move {
                    (
                        StatusCode::TEMPORARY_REDIRECT,
                        [(header::LOCATION, redirect_location)],
                    )
                }
            }),
        );
        let redirect_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let redirect_address = redirect_listener.local_addr().unwrap();
        let redirect_server =
            tokio::spawn(
                async move { axum::serve(redirect_listener, redirect_app).await.unwrap() },
            );

        let client = ProviderClient::uninitialized_for_test().unwrap();
        let provider = ProxyConfig {
            base_url: format!("http://{redirect_address}"),
            api_key: None,
            provider_name: ProviderId::Continuum.as_str().to_string(),
        };
        let body = Bytes::from_static(br#"{"model":"glm-5-3"}"#);

        let response = client
            .send(
                &provider,
                ProviderRequest::new(Method::POST, "/v1/chat/completions", Duration::from_secs(5))
                    .content_type("application/json")
                    .body(body),
            )
            .await
            .unwrap();

        assert_eq!(
            response.status_code(),
            StatusCode::TEMPORARY_REDIRECT.as_u16()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), target_rx.recv())
                .await
                .is_err(),
            "the redirect target must never receive the provider request body"
        );

        redirect_server.abort();
        target_server.abort();
    }

    #[tokio::test]
    async fn standard_request_strips_hop_headers_and_preserves_opaque_values() {
        let (capture_tx, mut capture_rx) = mpsc::channel(1);
        let app = Router::new().route(
            "/v1/chat/completions",
            any(move |headers: HeaderMap, body: Bytes| {
                let capture_tx = capture_tx.clone();
                async move {
                    capture_tx.send((headers, body)).await.unwrap();
                    (StatusCode::OK, "{}")
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = ProviderClient::uninitialized_for_test().unwrap();
        let provider = ProxyConfig {
            base_url: format!("http://{address}"),
            api_key: Some("provider-key".to_string()),
            provider_name: ProviderId::Continuum.as_str().to_string(),
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer caller-secret"),
        );
        headers.insert(
            header::CONNECTION,
            HeaderValue::from_static("keep-alive, x-hop"),
        );
        headers.insert("keep-alive", HeaderValue::from_static("timeout=5"));
        headers.insert("proxy-connection", HeaderValue::from_static("keep-alive"));
        headers.insert("x-hop", HeaderValue::from_static("remove-me"));
        headers.insert(
            header::PROXY_AUTHORIZATION,
            HeaderValue::from_static("Basic caller-proxy-secret"),
        );
        headers.insert(
            header::TRANSFER_ENCODING,
            HeaderValue::from_static("chunked"),
        );
        headers.insert("x-safe", HeaderValue::from_static("yes"));
        headers.insert(
            "x-opaque",
            HeaderValue::from_bytes(b"\x80raw").expect("opaque header value"),
        );
        let body = Bytes::from_static(br#"{"model":"glm-5-3"}"#);

        let response = client
            .send(
                &provider,
                ProviderRequest::new(Method::POST, "/v1/chat/completions", Duration::from_secs(5))
                    .source_headers(&headers)
                    .content_type("application/json")
                    .body(body.clone()),
            )
            .await
            .unwrap();
        assert!(response.is_success());

        let (captured_headers, captured_body) = capture_rx.recv().await.unwrap();
        server.abort();

        assert_eq!(
            captured_headers.get(header::AUTHORIZATION).unwrap(),
            "Bearer provider-key"
        );
        assert_eq!(captured_headers.get("x-safe").unwrap(), "yes");
        assert_eq!(
            captured_headers.get("x-opaque").unwrap().as_bytes(),
            b"\x80raw"
        );
        assert!(!captured_headers.contains_key(header::CONNECTION));
        assert!(!captured_headers.contains_key("keep-alive"));
        assert!(!captured_headers.contains_key("proxy-connection"));
        assert!(!captured_headers.contains_key("x-hop"));
        assert!(!captured_headers.contains_key(header::PROXY_AUTHORIZATION));
        assert!(!captured_headers.contains_key(header::TRANSFER_ENCODING));
        assert_eq!(captured_body, body);
    }

    #[test]
    fn retry_rebuild_preserves_request_contract_for_a_new_router() {
        let client = ProviderClient::for_test("http://unused.invalid".to_string()).unwrap();
        let test_client = match &client.tinfoil {
            TinfoilTransport::Plain { client, .. } => client.clone(),
            TinfoilTransport::Secure(_) => unreachable!("for_test uses the plain transport"),
        };
        let old_router = TinfoilAttempt::Plain {
            client: test_client.clone(),
            base_url: "https://old-router.example".to_string(),
            api_key: "same-key".to_string(),
        };
        let new_router = TinfoilAttempt::Plain {
            client: test_client,
            base_url: "https://new-router.example".to_string(),
            api_key: "same-key".to_string(),
        };
        let mut headers = HeaderMap::new();
        headers.insert("x-request-id", HeaderValue::from_static("request-123"));
        let body = br#"{"model":"glm-5-3","messages":[],"stream":true}"#.to_vec();
        let template = ProviderRequest::new(
            Method::POST,
            "/v1/chat/completions?trace=1",
            Duration::from_secs(5),
        )
        .source_headers(&headers)
        .content_type("application/json")
        .body(body.clone());

        let first = client
            .build_tinfoil_request_for_attempt(&tinfoil_proxy(), template.clone(), &old_router)
            .unwrap();
        let retry = client
            .build_tinfoil_request_for_attempt(&tinfoil_proxy(), template, &new_router)
            .unwrap();

        assert_eq!(first.method(), retry.method());
        assert_eq!(first.url().path(), retry.url().path());
        assert_eq!(first.url().query(), retry.url().query());
        assert_eq!(first.headers(), retry.headers());
        assert_eq!(
            first.body().and_then(|value| value.as_bytes()),
            retry.body().and_then(|value| value.as_bytes())
        );
        assert_eq!(
            first.body().and_then(|value| value.as_bytes()),
            Some(body.as_slice())
        );
        assert_eq!(first.url().host_str(), Some("old-router.example"));
        assert_eq!(retry.url().host_str(), Some("new-router.example"));
    }

    #[tokio::test]
    async fn tinfoil_request_matches_contract_at_the_network_boundary() {
        let (capture_tx, mut capture_rx) = mpsc::channel(1);
        let app = Router::new().route(
            "/v1/chat/completions",
            any({
                let capture_tx = capture_tx.clone();
                move |method: axum::http::Method, uri: Uri, headers: HeaderMap, body: Bytes| {
                    let capture_tx = capture_tx.clone();
                    async move {
                        capture_tx.send((method, uri, headers, body)).await.unwrap();
                        (StatusCode::OK, [(header::CONTENT_TYPE, "audio/flac")], "{}")
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = ProviderClient::for_test(format!("http://{address}")).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer caller-secret"),
        );
        headers.insert(
            header::CONNECTION,
            HeaderValue::from_static("keep-alive, x-hop"),
        );
        headers.insert("x-hop", HeaderValue::from_static("remove-me"));
        headers.insert("x-request-id", HeaderValue::from_static("request-123"));
        let body = br#"{"model":"glm-5-3","messages":[],"stream":true}"#.to_vec();

        let response = client
            .send(
                &tinfoil_proxy(),
                ProviderRequest::new(
                    Method::POST,
                    "/v1/chat/completions?trace=1",
                    Duration::from_secs(5),
                )
                .source_headers(&headers)
                .content_type("application/json")
                .body(body.clone()),
            )
            .await
            .unwrap();
        assert!(response.is_success());
        assert_eq!(response.content_type(), Some("audio/flac"));

        let (method, uri, captured_headers, captured_body) = capture_rx.recv().await.unwrap();
        server.abort();

        assert_eq!(method, Method::POST);
        assert_eq!(
            uri.path_and_query().unwrap(),
            "/v1/chat/completions?trace=1"
        );
        assert_eq!(
            captured_headers.get(header::AUTHORIZATION).unwrap(),
            "Bearer test-tinfoil-key"
        );
        assert_eq!(
            captured_headers.get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        assert_eq!(captured_headers.get("x-request-id").unwrap(), "request-123");
        assert!(!captured_headers.contains_key(header::CONNECTION));
        assert!(!captured_headers.contains_key("x-hop"));
        assert_eq!(captured_body.as_ref(), body.as_slice());
    }

    #[tokio::test]
    #[ignore = "requires live Tinfoil credentials and network access"]
    async fn live_tinfoil_models_and_completions_match_the_legacy_api_contract() {
        let api_key = std::env::var("TINFOIL_API_KEY").unwrap_or_else(|_| {
            std::fs::read_to_string(".local/secrets/tinfoil_api_key")
                .expect("TINFOIL_API_KEY")
                .trim()
                .to_string()
        });
        let credential_label = std::env::var("TINFOIL_PARITY_CREDENTIAL_LABEL")
            .unwrap_or_else(|_| "shared local Tinfoil test credential".to_string());
        let client = ProviderClient::new(api_key).await.unwrap();
        let provider = ProxyConfig {
            // Secure Tinfoil requests ignore this routing placeholder and use
            // the enclave selected and attested by the SDK.
            base_url: client.tinfoil_base_url(),
            api_key: None,
            provider_name: ProviderId::Tinfoil.as_str().to_string(),
        };

        let initialization_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let models = loop {
            match client
                .send(
                    &provider,
                    ProviderRequest::new(Method::GET, "/v1/models", Duration::from_secs(30)),
                )
                .await
            {
                Ok(response) => break response,
                Err(ProviderRequestError::TinfoilUnavailable)
                    if tokio::time::Instant::now() < initialization_deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(error) => panic!("Tinfoil initialization or models request failed: {error}"),
            }
        };
        let target_url = client.tinfoil_base_url();
        assert_ne!(target_url, TINFOIL_UNINITIALIZED_BASE_URL);
        assert!(models.is_success());
        assert_eq!(models.status_code(), 200);
        let models_content_type = response_header(&models, "content-type")
            .expect("models response content type")
            .to_string();
        assert!(models_content_type.starts_with("application/json"));
        write_response_headers("models.headers", &models);
        let models_body = models.bytes().await.unwrap();
        write_evidence_file("models.json", &models_body);
        let models: serde_json::Value = serde_json::from_slice(&models_body).unwrap();
        let mut model_ids: Vec<&str> = models["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|model| model["id"].as_str())
            .collect();
        model_ids.sort_unstable();
        assert_eq!(models["object"], "list");
        assert_eq!(
            model_ids,
            [
                "doc-upload",
                "gemma4-31b",
                "gpt-oss-120b",
                "gpt-oss-safeguard-120b",
                "kimi-k2-6",
                "llama3-3-70b",
                "nomic-embed-text",
                "qwen3-tts",
                "voxtral-mini-4b-realtime",
                "voxtral-small-24b",
                "voxtral-tts",
                "websearch",
                "whisper-large-v3-turbo",
            ]
        );

        let completion_body = br#"{"model":"gpt-oss-120b","messages":[{"role":"user","content":"Reply with exactly: parity-ok"}],"temperature":0,"max_tokens":32,"stream":false}"#.to_vec();
        let completion = client
            .send(
                &provider,
                ProviderRequest::new(
                    Method::POST,
                    "/v1/chat/completions",
                    Duration::from_secs(120),
                )
                .content_type("application/json")
                .body(completion_body.clone()),
            )
            .await
            .unwrap();
        assert!(completion.is_success());
        assert_eq!(completion.status_code(), 200);
        let completion_content_type = response_header(&completion, "content-type")
            .expect("completion response content type")
            .to_string();
        assert!(completion_content_type.starts_with("application/json"));
        write_response_headers("completion.headers", &completion);
        let completion_response_body = completion.bytes().await.unwrap();
        write_evidence_file("completion.json", &completion_response_body);
        let completion: serde_json::Value =
            serde_json::from_slice(&completion_response_body).unwrap();
        assert_eq!(completion["object"], "chat.completion");
        assert_eq!(completion["model"], "gpt-oss-120b");
        assert_eq!(completion["choices"][0]["message"]["content"], "parity-ok");
        assert!(completion["usage"]["prompt_tokens"].is_number());
        assert!(completion["usage"]["completion_tokens"].is_number());

        let stream_body = br#"{"model":"gpt-oss-120b","messages":[{"role":"user","content":"Reply with exactly: parity-stream-ok"}],"temperature":0,"max_tokens":32,"stream":true,"stream_options":{"include_usage":true}}"#.to_vec();
        let stream = client
            .send(
                &provider,
                ProviderRequest::new(
                    Method::POST,
                    "/v1/chat/completions",
                    Duration::from_secs(120),
                )
                .content_type("application/json")
                .body(stream_body.clone()),
            )
            .await
            .unwrap();
        assert!(stream.is_success());
        assert_eq!(stream.status_code(), 200);
        let stream_content_type = response_header(&stream, "content-type")
            .expect("stream response content type")
            .to_string();
        assert!(stream_content_type.starts_with("text/event-stream"));
        write_response_headers("completion-stream.headers", &stream);
        let stream_response_body = stream.bytes().await.unwrap();
        write_evidence_file("completion-stream.sse", &stream_response_body);
        let stream = String::from_utf8(stream_response_body.to_vec()).unwrap();
        let stream = stream.replace("\r\n", "\n");
        let data_frames: Vec<&str> = stream
            .split("\n\n")
            .filter_map(|frame| frame.lines().find_map(|line| line.strip_prefix("data: ")))
            .collect();
        assert_eq!(data_frames.last(), Some(&"[DONE]"));
        assert_eq!(
            data_frames.iter().filter(|data| **data == "[DONE]").count(),
            1
        );

        let mut json_frames = 0;
        let mut saw_usage = false;
        for data in &data_frames[..data_frames.len() - 1] {
            let frame: serde_json::Value = serde_json::from_str(data).unwrap();
            assert_eq!(frame["object"], "chat.completion.chunk");
            assert_eq!(frame["model"], "gpt-oss-120b");
            saw_usage |= frame.get("usage").is_some_and(|usage| !usage.is_null());
            json_frames += 1;
        }
        assert!(json_frames > 1);
        assert!(saw_usage);

        let final_json_frame: serde_json::Value =
            serde_json::from_str(data_frames[data_frames.len() - 2]).unwrap();
        let final_usage = &final_json_frame["usage"];
        assert!(final_usage["prompt_tokens"].is_number());
        assert!(final_usage["completion_tokens"].is_number());
        assert!(final_usage["total_tokens"].is_number());

        let captured_at_unix_seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after Unix epoch")
            .as_secs();
        let revision = git_output(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
        let working_tree_status = git_output(&["status", "--short"])
            .map(|status| if status.is_empty() { "clean" } else { "dirty" })
            .unwrap_or("unknown");
        let manifest = json!({
            "captured_at_unix_seconds": captured_at_unix_seconds,
            "implementation": "tinfoil-rs v0.2.0 (in-process)",
            "revision": revision,
            "working_tree": working_tree_status,
            "credential_label": credential_label,
            "target_url": target_url,
            "requests": {
                "models": {
                    "method": "GET",
                    "path": "/v1/models",
                    "body_sha256": sha256_hex(b"")
                },
                "completion": {
                    "method": "POST",
                    "path": "/v1/chat/completions",
                    "content_type": "application/json",
                    "body_utf8": String::from_utf8_lossy(&completion_body),
                    "body_sha256": sha256_hex(&completion_body)
                },
                "stream": {
                    "method": "POST",
                    "path": "/v1/chat/completions",
                    "content_type": "application/json",
                    "body_utf8": String::from_utf8_lossy(&stream_body),
                    "body_sha256": sha256_hex(&stream_body)
                }
            },
            "results": {
                "models": {
                    "status": 200,
                    "content_type": models_content_type,
                    "object": models["object"],
                    "model_ids": model_ids
                },
                "completion": {
                    "status": 200,
                    "content_type": completion_content_type,
                    "object": completion["object"],
                    "model": completion["model"],
                    "content": completion["choices"][0]["message"]["content"],
                    "usage": completion["usage"]
                },
                "stream": {
                    "status": 200,
                    "content_type": stream_content_type,
                    "json_frames": json_frames,
                    "done_frames": 1,
                    "done_is_terminal": true,
                    "final_usage": final_usage
                }
            }
        });
        write_evidence_file(
            "manifest.json",
            serde_json::to_vec_pretty(&manifest).unwrap(),
        );
    }
}
