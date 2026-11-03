use crate::provider_registry::{MapleInstanceId, ProviderId};
use crate::proxy_config::ProxyConfig;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use reqwest::{Method, Request as ReqwestRequest};
use std::collections::{BTreeMap, HashSet};
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

#[derive(Debug, PartialEq, Eq)]
struct AttestationFailureDiagnostic {
    category: &'static str,
    reason: &'static str,
}

fn attestation_failure_diagnostic(error: &tinfoil::Error) -> AttestationFailureDiagnostic {
    // SDK errors can contain arbitrary response text, URLs, and credentials.
    // Log only finite labels derived from typed variants, never their payloads.
    let category = if error.is_configuration() {
        "configuration"
    } else if error.is_fetch() {
        "fetch"
    } else if error.is_attestation() {
        "attestation"
    } else if error.is_api() {
        "api"
    } else {
        "other"
    };
    let reason = match error {
        tinfoil::Error::Configuration(_) => "configuration",
        tinfoil::Error::Api(_) => "api",
        tinfoil::Error::AttestationFetch(_) => "attestation_fetch",
        tinfoil::Error::AttestationVerification(_) => "attestation_verification",
        tinfoil::Error::CertificateMismatch => "certificate_mismatch",
        tinfoil::Error::SigstoreVerification(_) => "sigstore_verification",
        tinfoil::Error::GitHub(_) => "github_fetch",
        tinfoil::Error::MeasurementMismatch { .. } => "measurement_mismatch",
        tinfoil::Error::Tls(_) => "tls_verification",
        tinfoil::Error::Ehbp(_) => "ehbp",
        tinfoil::Error::EhbpKeyMismatch(_) => "ehbp_key_mismatch",
        tinfoil::Error::Network(_) => "network",
        tinfoil::Error::UnsupportedFormat(_) => "unsupported_format",
        tinfoil::Error::Http(error) if error.is_timeout() => "http_timeout",
        tinfoil::Error::Http(error) if error.is_connect() => "http_connect",
        tinfoil::Error::Http(error) if error.is_status() => "http_status",
        tinfoil::Error::Http(_) => "http",
        tinfoil::Error::Json(_) => "json",
        tinfoil::Error::Base64(_) => "base64",
        tinfoil::Error::Io(_) => "io",
        _ => "unknown",
    };
    AttestationFailureDiagnostic { category, reason }
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderClientError {
    #[error("failed to build provider HTTP client: {0}")]
    Client(#[from] reqwest::Error),

    #[error("failed to install the required Ring crypto provider")]
    CryptoProvider,

    #[error("invalid compiled Maple inference attestation policy")]
    MaplePolicy,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ProviderRequestError {
    #[error(
        "Tinfoil is temporarily unavailable while router discovery and attestation retry in the background"
    )]
    TinfoilUnavailable,

    #[error("Maple inference is temporarily unavailable while direct-node attestation retries in the background")]
    MapleUnavailable,

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

// Current verification status controls recovery ownership, never whether an
// already-executed request is safe to replay. An expiry concurrent with a reset
// does not prove the upstream failed to accept the POST.
fn classify_post_execute_failure(
    is_connect: bool,
    verified_state_current: bool,
    message: String,
) -> (ProviderRequestError, bool) {
    let error = if is_connect {
        ProviderRequestError::Connect(message)
    } else {
        ProviderRequestError::Send(message)
    };
    (error, !verified_state_current)
}

fn current_slot_client<T>(
    candidate: Option<&Arc<T>>,
    is_current: impl FnOnce(&T) -> bool,
) -> Option<Arc<T>> {
    let client = candidate?;
    is_current(client).then(|| Arc::clone(client))
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

#[derive(Clone)]
enum AttestationTarget {
    SharedRouter,
    MapleDirect {
        instance: MapleInstanceId,
        host: &'static str,
        policy: tinfoil::DirectPolicy,
    },
}

impl AttestationTarget {
    fn unavailable(&self) -> ProviderRequestError {
        match self {
            Self::SharedRouter => ProviderRequestError::TinfoilUnavailable,
            Self::MapleDirect { .. } => ProviderRequestError::MapleUnavailable,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::SharedRouter => "tinfoil",
            Self::MapleDirect { instance, .. } => ProviderId::Maple(*instance).as_str(),
        }
    }

    fn client_is_current(&self, client: &tinfoil::Client) -> bool {
        match self {
            Self::SharedRouter => true,
            Self::MapleDirect { .. } => client.secure_client().is_verified(),
        }
    }

    async fn create_client(&self, api_key: String) -> Result<tinfoil::Client, tinfoil::Error> {
        match self {
            Self::SharedRouter => tinfoil::Client::new_default_with_api_key(api_key).await,
            Self::MapleDirect { host, policy, .. } => {
                tinfoil::Client::new_direct(*host, api_key, policy.clone()).await
            }
        }
    }
}

#[cfg(test)]
type TestClientFactory = Arc<
    dyn Fn(
            AttestationTarget,
            String,
        ) -> Pin<Box<dyn Future<Output = Result<tinfoil::Client, tinfoil::Error>> + Send>>
        + Send
        + Sync,
>;

struct SecureTinfoilTransport {
    api_key: String,
    target: AttestationTarget,
    #[cfg(test)]
    client_factory: Option<TestClientFactory>,
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
    fn new(api_key: String, target: AttestationTarget) -> Self {
        Self {
            api_key,
            target,
            client: RwLock::new(SecureClientSlot {
                client: None,
                refresh_attempt: 0,
            }),
            refresh_gate: Mutex::new(()),
            initialization_started: AtomicBool::new(false),
            #[cfg(test)]
            client_factory: None,
        }
    }

    async fn create_client(&self) -> Result<tinfoil::Client, tinfoil::Error> {
        #[cfg(test)]
        if let Some(factory) = &self.client_factory {
            return factory(self.target.clone(), self.api_key.clone()).await;
        }
        self.target.create_client(self.api_key.clone()).await
    }

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
        current_slot_client(slot.client.as_ref(), |client| {
            self.target.client_is_current(client)
        })
        .map(|client| SecureClientSnapshot {
            client,
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
                self.create_client(),
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
                    tracing::info!(
                        provider = self.target.label(),
                        "Provider attestation succeeded"
                    );
                    return;
                }
                Ok(Err(error)) => {
                    let diagnostic = attestation_failure_diagnostic(&error);
                    tracing::warn!(
                        retry_delay_seconds = retry_delay.as_secs(),
                        provider = self.target.label(),
                        error_category = diagnostic.category,
                        error_reason = diagnostic.reason,
                        error_is_attestation = error.is_attestation(),
                        error_is_fetch = error.is_fetch(),
                        "Provider attestation failed; retrying in background"
                    );
                }
                Err(_) => {
                    tracing::warn!(
                        attempt_timeout_seconds = TINFOIL_DISCOVERY_ATTEMPT_TIMEOUT.as_secs(),
                        retry_delay_seconds = retry_delay.as_secs(),
                        provider = self.target.label(),
                        "Provider attestation timed out; retrying in background"
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
            .map_err(|_| self.target.unavailable())
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
                return self.snapshot().ok_or_else(|| self.target.unavailable());
            }
            RefreshDecision::PreviousAttemptFailed => {
                return Err(self.target.unavailable());
            }
            RefreshDecision::Refresh => {}
        }

        // Shared Tinfoil may discover a replacement router. Maple always
        // reattests the same explicit host and immutable current/staged policy;
        // certificate rotation never changes endpoint or trust authority.
        let refreshed = run_tinfoil_discovery_attempt_with_timeout(
            self.create_client(),
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
                let diagnostic = attestation_failure_diagnostic(&error);
                tracing::warn!(
                    provider = self.target.label(),
                    error_category = diagnostic.category,
                    error_reason = diagnostic.reason,
                    error_is_attestation = error.is_attestation(),
                    error_is_fetch = error.is_fetch(),
                    "Provider attestation refresh failed; transitioning unavailable"
                );
                slot.client = None;
                drop(slot);
                drop(refresh_guard);
                self.start_background_initialization();
                Err(self.target.unavailable())
            }
            Err(_) => {
                tracing::warn!(
                    attempt_timeout_seconds = TINFOIL_DISCOVERY_ATTEMPT_TIMEOUT.as_secs(),
                    provider = self.target.label(),
                    "Provider attestation refresh timed out; transitioning unavailable"
                );
                slot.client = None;
                drop(slot);
                drop(refresh_guard);
                self.start_background_initialization();
                Err(self.target.unavailable())
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
                .ok_or_else(|| transport.target.unavailable()),
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

/// Route eligibility follows the active verified direct transport, including
/// its hard freshness deadline. A stale/missing client starts body-free recovery
/// but remains ineligible until that recovery succeeds.
#[derive(Clone)]
pub(crate) struct MapleReadiness {
    transports: BTreeMap<MapleInstanceId, TinfoilTransport>,
}

impl MapleReadiness {
    pub(crate) fn is_ready(&self, instance: MapleInstanceId) -> bool {
        let Some(transport) = self.transports.get(&instance) else {
            return false;
        };
        transport.start_background_initialization();
        transport.snapshot().is_ok()
    }

    #[cfg(test)]
    pub(crate) fn any_ready(&self) -> bool {
        // Check every configured guest so one ready replica does not suppress
        // background recovery of the other.
        let mut ready = false;
        for instance in self.transports.keys() {
            ready |= self.is_ready(*instance);
        }
        ready
    }

    #[cfg(test)]
    pub(crate) fn for_test(ready: bool) -> Self {
        Self::for_test_instances(if ready { &MapleInstanceId::ALL } else { &[] })
    }

    #[cfg(test)]
    pub(crate) fn for_test_instances(ready: &[MapleInstanceId]) -> Self {
        Self {
            transports: ready
                .iter()
                .map(|instance| {
                    (
                        *instance,
                        TinfoilTransport::Plain {
                            client: test_http_client().expect("test client"),
                            base_url: "http://127.0.0.1:9".to_owned(),
                            api_key: String::new(),
                        },
                    )
                })
                .collect(),
        }
    }
}

impl std::fmt::Debug for MapleReadiness {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MapleReadiness")
            .field(
                "configured_instances",
                &self.transports.keys().collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
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
    maple: BTreeMap<MapleInstanceId, TinfoilTransport>,
    standard: StandardHttpClient,
}

impl ProviderClient {
    pub async fn new(tinfoil_api_key: String) -> Result<Self, ProviderClientError> {
        crate::http_client::install_ring_crypto_provider()
            .map_err(|_| ProviderClientError::CryptoProvider)?;

        let secure_transport = Arc::new(SecureTinfoilTransport::new(
            tinfoil_api_key,
            AttestationTarget::SharedRouter,
        ));
        secure_transport.start_background_initialization();

        Ok(Self {
            tinfoil: TinfoilTransport::Secure(secure_transport),
            maple: BTreeMap::new(),
            standard: standard_http_client()?,
        })
    }

    /// Enable Maple's dedicated direct-attested transport with its own key.
    /// Missing/blank configuration leaves Maple unavailable; it never borrows
    /// the shared Tinfoil credential or uses an ordinary HTTP transport.
    pub fn with_maple_api_key(
        mut self,
        api_key: Option<String>,
    ) -> Result<Self, ProviderClientError> {
        self.maple.clear();
        if let Some(api_key) = api_key.filter(|key| !key.trim().is_empty()) {
            for instance in MapleInstanceId::ALL {
                let endpoint = crate::maple_inference::endpoint(instance);
                let Some(policy) = crate::maple_inference::direct_policy(instance)
                    .map_err(|_| ProviderClientError::MaplePolicy)?
                else {
                    continue;
                };
                let transport = Arc::new(SecureTinfoilTransport::new(
                    api_key.clone(),
                    AttestationTarget::MapleDirect {
                        instance: endpoint.instance,
                        host: endpoint.host,
                        policy,
                    },
                ));
                transport.start_background_initialization();
                self.maple
                    .insert(instance, TinfoilTransport::Secure(transport));
            }
        }
        Ok(self)
    }

    #[cfg(test)]
    fn uninitialized_for_test() -> Result<Self, ProviderClientError> {
        crate::http_client::install_ring_crypto_provider()
            .map_err(|_| ProviderClientError::CryptoProvider)?;

        Ok(Self {
            tinfoil: TinfoilTransport::Secure(Arc::new(SecureTinfoilTransport {
                api_key: "unused-test-key".to_string(),
                target: AttestationTarget::SharedRouter,
                client_factory: None,
                client: RwLock::new(SecureClientSlot {
                    client: None,
                    refresh_attempt: 0,
                }),
                refresh_gate: Mutex::new(()),
                // Keep this deterministic: tests exercise the unready behavior
                // without creating a real discovery request.
                initialization_started: AtomicBool::new(true),
            })),
            maple: BTreeMap::new(),
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
            maple: BTreeMap::new(),
            standard: standard_http_client()?,
        })
    }

    pub(crate) fn maple_readiness(&self) -> MapleReadiness {
        MapleReadiness {
            transports: self.maple.clone(),
        }
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
        let transport = match ProviderId::from_name(&provider.provider_name) {
            Some(ProviderId::Tinfoil) => &self.tinfoil,
            Some(ProviderId::Maple(instance)) => {
                let Some(transport) = self.maple.get(&instance) else {
                    return ProviderSendTrace::terminal(Err(
                        ProviderRequestError::MapleUnavailable,
                    ));
                };
                transport
            }
            Some(ProviderId::Continuum) => {
                return ProviderSendTrace::terminal(self.send_standard(provider, request).await);
            }
            None if provider.provider_name == "openai" => {
                return ProviderSendTrace::terminal(self.send_standard(provider, request).await);
            }
            None => {
                // In particular, misspelled/retired Maple identities must never
                // fall back to unverified HTTP or use the proxy's supplied key.
                return ProviderSendTrace::terminal(Err(ProviderRequestError::Build(
                    "unknown provider routing identity".to_owned(),
                )));
            }
        };

        let response_start_timeout = request.response_start_timeout;
        // Construction already starts this loop. Calling it here also makes the
        // request path self-healing if construction behavior changes later;
        // the atomic claim prevents discovery herds.
        transport.start_background_initialization();
        let attempt = match transport.snapshot() {
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
                    let http_client = failed_snapshot.client.http_client().map_err(|error| {
                        if !transport
                            .secure()
                            .target
                            .client_is_current(&failed_snapshot.client)
                        {
                            transport.start_background_initialization();
                            transport.secure().target.unavailable()
                        } else {
                            ProviderRequestError::Send(error.to_string())
                        }
                    })?;
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
                                Arc::clone(transport.secure()),
                                failed_snapshot.clone(),
                            )
                            .await
                            // A task panic/cancellation is an availability
                            // failure; do not expose JoinError internals.
                            .map_err(|_| transport.secure().target.unavailable())??;
                            let retry_attempt = TinfoilAttempt::Secure(refreshed.clone());
                            let retry_request = self.build_tinfoil_request_for_attempt(
                                provider,
                                request,
                                &retry_attempt,
                            )?;
                            refreshed
                                .client
                                .http_client()
                                .map_err(|error| {
                                    if !transport
                                        .secure()
                                        .target
                                        .client_is_current(&refreshed.client)
                                    {
                                        transport.start_background_initialization();
                                        transport.secure().target.unavailable()
                                    } else {
                                        ProviderRequestError::Send(error.to_string())
                                    }
                                })?
                                .execute(retry_request)
                                .await
                                .map_err(|error| {
                                    let (error, recover) = classify_post_execute_failure(
                                        error.is_connect(),
                                        transport
                                            .secure()
                                            .target
                                            .client_is_current(&refreshed.client),
                                        error.to_string(),
                                    );
                                    if recover {
                                        transport.start_background_initialization();
                                    }
                                    error
                                })
                        }
                        Err(error) => {
                            let (error, recover) = classify_post_execute_failure(
                                error.is_connect(),
                                transport
                                    .secure()
                                    .target
                                    .client_is_current(&failed_snapshot.client),
                                error.to_string(),
                            );
                            if recover {
                                transport.start_background_initialization();
                            }
                            Err(error)
                        }
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
        debug_assert!(
            provider.provider_name == ProviderId::Tinfoil.as_str()
                || ProviderId::from_name(&provider.provider_name).is_some_and(ProviderId::is_maple)
        );
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
        if ProviderId::from_name(&provider.provider_name).is_some_and(ProviderId::is_maple) {
            // vLLM accepts this header as an override of body priority. The
            // dedicated node's scheduling policy is owned by the server.
            request.headers_mut().remove("x-vllm-priority");
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
    // Client attribution is for our usage telemetry, never an upstream header.
    skipped.insert(HeaderName::from_static(
        crate::client_app::CLIENT_APP_HEADER,
    ));

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

    fn maple_proxy() -> ProxyConfig {
        maple_proxy_for(MapleInstanceId::I01)
    }

    fn maple_proxy_for(instance: MapleInstanceId) -> ProxyConfig {
        ProxyConfig {
            // Deliberately misleading: attested transports must select their
            // verified endpoint and own key, never these proxy fields.
            base_url: "http://127.0.0.1:9/should-not-be-used".to_owned(),
            api_key: Some("wrong-shared-key".to_owned()),
            provider_name: ProviderId::Maple(instance).as_str().to_owned(),
        }
    }

    fn direct_target() -> AttestationTarget {
        direct_target_for(MapleInstanceId::I01)
    }

    fn direct_target_for(instance: MapleInstanceId) -> AttestationTarget {
        AttestationTarget::MapleDirect {
            instance,
            host: crate::maple_inference::endpoint(instance).host,
            policy: crate::maple_inference::direct_policy(instance)
                .unwrap()
                .unwrap(),
        }
    }

    #[test]
    fn attestation_failure_diagnostics_preserve_typed_causes_without_payloads() {
        let sensitive = "Bearer diagnostic-secret https://private.invalid/?token=diagnostic-secret\nforged_log=true";
        let cases = [
            (
                tinfoil::Error::Configuration(sensitive.into()),
                "configuration",
                "configuration",
            ),
            (
                tinfoil::Error::AttestationFetch(sensitive.into()),
                "fetch",
                "attestation_fetch",
            ),
            (
                tinfoil::Error::AttestationVerification(sensitive.into()),
                "attestation",
                "attestation_verification",
            ),
            (
                tinfoil::Error::CertificateMismatch,
                "attestation",
                "certificate_mismatch",
            ),
            (
                tinfoil::Error::SigstoreVerification(sensitive.into()),
                "attestation",
                "sigstore_verification",
            ),
            (
                tinfoil::Error::GitHub(sensitive.into()),
                "fetch",
                "github_fetch",
            ),
            (
                tinfoil::Error::MeasurementMismatch {
                    expected: sensitive.into(),
                    actual: sensitive.into(),
                },
                "attestation",
                "measurement_mismatch",
            ),
            (
                tinfoil::Error::Tls(sensitive.into()),
                "attestation",
                "tls_verification",
            ),
            (
                tinfoil::Error::Ehbp(sensitive.into()),
                "attestation",
                "ehbp",
            ),
            (
                tinfoil::Error::EhbpKeyMismatch(sensitive.into()),
                "attestation",
                "ehbp_key_mismatch",
            ),
            (
                tinfoil::Error::Network(sensitive.into()),
                "fetch",
                "network",
            ),
            (
                tinfoil::Error::UnsupportedFormat(sensitive.into()),
                "attestation",
                "unsupported_format",
            ),
            (
                tinfoil::Error::Io(std::io::Error::other(sensitive)),
                "fetch",
                "io",
            ),
            (
                tinfoil::Error::Json(serde_json::from_value::<bool>(json!(sensitive)).unwrap_err()),
                "attestation",
                "json",
            ),
            (
                tinfoil::Error::Http(
                    test_http_client()
                        .unwrap()
                        .get("https://private.invalid/?token=diagnostic-secret")
                        .header("invalid\nheader", sensitive)
                        .build()
                        .unwrap_err(),
                ),
                "fetch",
                "http",
            ),
        ];

        for (error, category, reason) in cases {
            let diagnostic = attestation_failure_diagnostic(&error);
            assert_eq!(
                diagnostic,
                AttestationFailureDiagnostic { category, reason }
            );
            let rendered = format!("{diagnostic:?}");
            for forbidden in [
                "Bearer",
                "diagnostic-secret",
                "private.invalid",
                "forged_log",
            ] {
                assert!(!rendered.contains(forbidden));
            }
        }
    }

    #[test]
    fn expiry_during_ambiguous_execute_never_makes_the_request_replay_safe() {
        for verified_state_current in [true, false] {
            let (error, recover) = classify_post_execute_failure(
                false,
                verified_state_current,
                "synthetic connection reset after POST".to_owned(),
            );
            assert!(matches!(error, ProviderRequestError::Send(_)));
            assert_eq!(recover, !verified_state_current);
        }
        let (error, _) =
            classify_post_execute_failure(true, false, "synthetic TLS setup failure".to_owned());
        assert!(matches!(error, ProviderRequestError::Connect(_)));
    }

    #[test]
    fn populated_noncurrent_slot_is_unavailable_and_can_claim_recovery() {
        // The fork separately exercises actual verified-client expiration.
        // This checks the backend slot selection used by snapshot(): presence
        // of an Arc alone must not make expired state ready or block recovery.
        let current = Arc::new(AtomicBool::new(true));
        let candidate = Some(Arc::clone(&current));
        assert!(
            current_slot_client(candidate.as_ref(), |state| state.load(Ordering::Acquire))
                .is_some()
        );
        current.store(false, Ordering::Release);
        assert!(
            candidate.is_some(),
            "the old client remains present in the slot"
        );
        assert!(
            current_slot_client(candidate.as_ref(), |state| state.load(Ordering::Acquire))
                .is_none()
        );
        let transport = uninitialized_secure_transport();
        assert!(transport.claim_background_initialization());
        assert!(!transport.claim_background_initialization());
        let mut claim = InitializationClaim::new(Arc::clone(&transport));
        claim.release();
        assert!(
            transport.claim_background_initialization(),
            "recovery remains claimable after a completed attempt"
        );
    }

    #[tokio::test]
    async fn missing_maple_key_never_uses_shared_or_standard_transport() {
        let client = ProviderClient::for_test("http://127.0.0.1:9".to_owned())
            .unwrap()
            .with_maple_api_key(None)
            .unwrap();
        assert!(!client.maple_readiness().any_ready());
        let result = client
            .send(
                &maple_proxy(),
                ProviderRequest::new(
                    Method::POST,
                    "/v1/chat/completions",
                    Duration::from_millis(50),
                )
                .body(Bytes::from_static(b"synthetic-test-body")),
            )
            .await;
        assert!(matches!(
            result,
            Err(ProviderRequestError::MapleUnavailable)
        ));
        let client = client.with_maple_api_key(Some("   ".to_owned())).unwrap();
        assert!(!client.maple_readiness().any_ready());
    }

    #[tokio::test]
    async fn direct_target_factories_keep_each_immutable_host_policy_and_shared_maple_key() {
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        for instance in MapleInstanceId::ALL {
            let observed = Arc::clone(&captured);
            let mut transport = SecureTinfoilTransport::new(
                "dedicated-test-key".to_owned(),
                direct_target_for(instance),
            );
            transport.client_factory = Some(Arc::new(move |target, key| {
                let AttestationTarget::MapleDirect {
                    instance,
                    host,
                    policy,
                } = target
                else {
                    panic!("dedicated verification must never discover a shared router");
                };
                observed
                    .lock()
                    .unwrap()
                    .push((instance, host, format!("{policy:?}"), key));
                Box::pin(async {
                    Err(tinfoil::Error::Configuration(
                        "injected verifier failure".to_owned(),
                    ))
                })
            }));
            // The same immutable target is used for initialization and refresh.
            assert!(transport.create_client().await.is_err());
            assert!(transport.create_client().await.is_err());
        }
        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 4);
        for (index, instance) in MapleInstanceId::ALL.into_iter().enumerate() {
            let first = &captured[index * 2];
            assert_eq!(first, &captured[index * 2 + 1]);
            assert_eq!(first.0, instance);
            assert_eq!(first.1, crate::maple_inference::endpoint(instance).host);
            assert_eq!(
                first.2,
                format!(
                    "{:?}",
                    crate::maple_inference::direct_policy(instance)
                        .unwrap()
                        .unwrap()
                )
            );
            assert_eq!(first.3, "dedicated-test-key");
        }
        assert_ne!(captured[0].1, captured[2].1);
    }

    #[tokio::test]
    async fn maple_readiness_and_recovery_are_independent_between_guests() {
        let pending = Arc::new(SecureTinfoilTransport {
            api_key: "dedicated-test-key".to_owned(),
            target: direct_target_for(MapleInstanceId::I01),
            client_factory: None,
            client: RwLock::new(SecureClientSlot {
                client: None,
                refresh_attempt: 7,
            }),
            refresh_gate: Mutex::new(()),
            initialization_started: AtomicBool::new(true),
        });
        let ready_transport = TinfoilTransport::Plain {
            client: test_http_client().unwrap(),
            base_url: "http://127.0.0.1:9".to_owned(),
            api_key: "dedicated-test-key".to_owned(),
        };
        let mut client = ProviderClient::for_test("http://127.0.0.1:9".to_owned()).unwrap();
        client.maple = BTreeMap::from([
            (
                MapleInstanceId::I01,
                TinfoilTransport::Secure(Arc::clone(&pending)),
            ),
            (MapleInstanceId::I02, ready_transport),
        ]);
        let readiness = client.maple_readiness();
        assert!(!readiness.is_ready(MapleInstanceId::I01));
        assert!(readiness.is_ready(MapleInstanceId::I02));
        assert!(readiness.any_ready());
        assert!(!readiness.clone().is_ready(MapleInstanceId::I01));
        let recovery_gate = pending.refresh_gate.lock().await;
        assert!(
            readiness.is_ready(MapleInstanceId::I02),
            "i01 recovery cannot lock out i02"
        );
        let result = client
            .send(
                &maple_proxy_for(MapleInstanceId::I01),
                ProviderRequest::new(
                    Method::POST,
                    "/v1/chat/completions",
                    Duration::from_millis(50),
                ),
            )
            .await;
        assert!(matches!(
            result,
            Err(ProviderRequestError::MapleUnavailable)
        ));
        assert!(readiness.is_ready(MapleInstanceId::I02));
        drop(recovery_gate);
    }

    #[tokio::test]
    async fn unknown_or_unconfigured_maple_route_never_uses_other_transports() {
        let (capture_tx, mut capture_rx) = mpsc::channel::<()>(1);
        let app = Router::new().fallback(any(move || {
            let capture_tx = capture_tx.clone();
            async move {
                capture_tx.send(()).await.unwrap();
                StatusCode::OK
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        for ready in MapleInstanceId::ALL {
            let mut client = ProviderClient::for_test(format!("http://{address}")).unwrap();
            client.maple.insert(
                ready,
                TinfoilTransport::Plain {
                    client: test_http_client().unwrap(),
                    base_url: format!("http://{address}"),
                    api_key: "dedicated-test-key".to_owned(),
                },
            );
            let absent = if ready == MapleInstanceId::I01 {
                MapleInstanceId::I02
            } else {
                MapleInstanceId::I01
            };
            let mut provider = maple_proxy_for(absent);
            provider.base_url = format!("http://{address}");
            let request = || {
                ProviderRequest::new(
                    Method::POST,
                    "/v1/chat/completions",
                    Duration::from_millis(50),
                )
            };
            assert!(matches!(
                client.send(&provider, request()).await,
                Err(ProviderRequestError::MapleUnavailable)
            ));
            for invalid in [
                "maple",
                "maple-i03",
                "maple-i01.attacker",
                "MAPLE-I01",
                "unknown",
            ] {
                provider.provider_name = invalid.to_owned();
                assert!(matches!(
                    client.send(&provider, request()).await,
                    Err(ProviderRequestError::Build(_))
                ));
            }
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(25), capture_rx.recv())
                .await
                .is_err()
        );
        server.abort();
    }

    #[tokio::test]
    async fn maple_readiness_recovers_single_flight_without_waiting_for_attestation() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let mut transport =
            SecureTinfoilTransport::new("dedicated-test-key".to_owned(), direct_target());
        transport.client_factory = Some(Arc::new(move |target, _| {
            assert!(matches!(target, AttestationTarget::MapleDirect { .. }));
            observed.fetch_add(1, Ordering::SeqCst);
            Box::pin(std::future::pending())
        }));
        let transport = Arc::new(transport);
        let readiness = MapleReadiness {
            transports: BTreeMap::from([(
                MapleInstanceId::I01,
                TinfoilTransport::Secure(Arc::clone(&transport)),
            )]),
        };
        for _ in 0..20 {
            assert!(!readiness.clone().is_ready(MapleInstanceId::I01));
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            while calls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            !readiness.is_ready(MapleInstanceId::I01),
            "pending attestation must not make a route eligible"
        );
        assert!(matches!(
            transport.try_refresh_gate(),
            Err(ProviderRequestError::MapleUnavailable)
        ));
    }

    #[tokio::test]
    async fn maple_dispatch_keeps_guest_selection_own_key_cache_salt_body_and_stream_bytes() {
        let expected_stream = "data: {\"choices\":[]}\n\ndata: [DONE]\n\n";
        let mut client = ProviderClient::for_test("http://127.0.0.1:9".to_owned()).unwrap();
        let mut servers = Vec::new();
        let mut captures = BTreeMap::new();
        for instance in MapleInstanceId::ALL {
            let (capture_tx, capture_rx) = mpsc::channel(1);
            let app = Router::new().route(
                "/v1/chat/completions",
                any(move |headers: HeaderMap, body: Bytes| {
                    let capture_tx = capture_tx.clone();
                    async move {
                        capture_tx.send((headers, body)).await.unwrap();
                        (
                            StatusCode::OK,
                            [(header::CONTENT_TYPE, "text/event-stream")],
                            expected_stream,
                        )
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            servers.push(tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap()
            }));
            client.maple.insert(
                instance,
                TinfoilTransport::Plain {
                    client: test_http_client().unwrap(),
                    base_url: format!("http://{address}"),
                    api_key: "dedicated-test-key".to_owned(),
                },
            );
            captures.insert(instance, (address, capture_rx));
        }
        for instance in [MapleInstanceId::I02, MapleInstanceId::I01] {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::AUTHORIZATION,
                HeaderValue::from_static("Bearer caller-secret"),
            );
            headers.insert(header::HOST, HeaderValue::from_static("wrong-host.invalid"));
            headers.insert(
                header::CONNECTION,
                HeaderValue::from_static("x-private-hop"),
            );
            headers.insert("x-private-hop", HeaderValue::from_static("must-be-removed"));
            headers.insert("x-vllm-priority", HeaderValue::from_static("-100"));
            let body = Bytes::from_static(br#"{"model":"deepseek-v4-1-flash","cache_salt":"backend-owned-synthetic-user","stream":true}"#);
            let response = client
                .send(
                    &maple_proxy_for(instance),
                    ProviderRequest::new(
                        Method::POST,
                        "/v1/chat/completions",
                        Duration::from_secs(2),
                    )
                    .source_headers(&headers)
                    .content_type("application/json")
                    .body(body.clone()),
                )
                .await
                .unwrap();
            assert!(matches!(&response, ProviderResponse::Tinfoil(_)));
            assert_eq!(response.content_type(), Some("text/event-stream"));
            let (address, capture_rx) = captures.get_mut(&instance).unwrap();
            let (headers, captured_body) =
                tokio::time::timeout(Duration::from_secs(1), capture_rx.recv())
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(headers[header::AUTHORIZATION], "Bearer dedicated-test-key");
            assert_eq!(headers[header::HOST], address.to_string());
            assert!(!headers.contains_key("x-private-hop"));
            assert!(!headers.contains_key("x-vllm-priority"));
            assert_eq!(captured_body, body);
            let mut stream = response.bytes_stream();
            let mut received = Vec::new();
            while let Some(chunk) = stream.next().await {
                received.extend_from_slice(&chunk.unwrap());
            }
            assert_eq!(received, expected_stream.as_bytes());
        }
        for (_, capture_rx) in captures.values_mut() {
            assert!(
                capture_rx.try_recv().is_err(),
                "no duplicated or cross-guest dispatch"
            );
        }
        for server in servers {
            server.abort();
        }
    }

    fn uninitialized_secure_transport() -> Arc<SecureTinfoilTransport> {
        Arc::new(SecureTinfoilTransport {
            api_key: "unused-test-key".to_string(),
            target: AttestationTarget::SharedRouter,
            client_factory: None,
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
        headers.append(
            crate::client_app::CLIENT_APP_HEADER,
            HeaderValue::from_static("work"),
        );

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
        assert!(!request
            .headers()
            .contains_key(crate::client_app::CLIENT_APP_HEADER));
        assert_eq!(
            request.body().and_then(|body| body.as_bytes()),
            Some(body.as_slice())
        );
    }

    #[test]
    fn client_app_attribution_is_never_forwarded_to_a_provider() {
        let mut source = HeaderMap::new();
        source.append(
            crate::client_app::CLIENT_APP_HEADER,
            HeaderValue::from_static("chat"),
        );
        source.append(
            crate::client_app::CLIENT_APP_HEADER,
            HeaderValue::from_static("work"),
        );
        source.insert("x-safe", HeaderValue::from_static("preserved"));
        let forwarded = forwarded_headers(&source);
        assert!(!forwarded.contains_key(crate::client_app::CLIENT_APP_HEADER));
        assert_eq!(forwarded["x-safe"], "preserved");
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
    fn maple_strips_priority_headers_without_changing_shared_tinfoil_headers() {
        let client = ProviderClient::for_test("https://router.example.test".to_owned()).unwrap();
        let attempt = client.tinfoil.snapshot().unwrap();
        let mut headers = HeaderMap::new();
        headers.append(
            HeaderName::from_bytes(b"X-VlLm-PrIoRiTy").unwrap(),
            HeaderValue::from_static("-100"),
        );
        headers.append("x-vllm-priority", HeaderValue::from_static("-200"));
        headers.insert("x-request-id", HeaderValue::from_static("request-123"));

        for (provider, expect_priority) in [
            (maple_proxy_for(MapleInstanceId::I01), false),
            (maple_proxy_for(MapleInstanceId::I02), false),
            (tinfoil_proxy(), true),
        ] {
            let request = client
                .build_tinfoil_request_for_attempt(
                    &provider,
                    ProviderRequest::new(
                        Method::POST,
                        "/v1/chat/completions",
                        Duration::from_secs(5),
                    )
                    .source_headers(&headers),
                    &attempt,
                )
                .unwrap();
            assert_eq!(
                request.headers().contains_key("x-vllm-priority"),
                expect_priority
            );
            if expect_priority {
                assert_eq!(
                    request.headers().get_all("x-vllm-priority").iter().count(),
                    2
                );
            }
            assert_eq!(request.headers()["x-request-id"], "request-123");
            assert_eq!(
                request.headers()[header::AUTHORIZATION],
                "Bearer test-tinfoil-key"
            );
        }
        assert_eq!(headers.get_all("x-vllm-priority").iter().count(), 2);
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
