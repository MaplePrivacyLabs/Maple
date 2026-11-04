//! This computer's optional HTTP API. The local host owns every listener,
//! connection and request; selecting a remote computer never moves this service.

use axum::{
    Router,
    body::Body,
    extract::Request,
    http::{Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    service::TowerToHyperService,
};
use serde::{Deserialize, Serialize};
use std::{
    io,
    net::{Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{Mutex as AsyncMutex, OwnedSemaphorePermit, Semaphore, watch},
    task::{JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;
use tower_http::cors::{AllowHeaders, Any, CorsLayer};

const MAX_CONNECTIONS: usize = 32;
const MAX_REQUESTS: usize = 8;
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// Keep this host's proxy traffic attributed to Work, independently of the API key.
pub(crate) fn work_proxy_config(
    host: String,
    port: u16,
    backend_url: String,
    pcr0: maple_sdk::Pcr0Environment,
) -> maple_proxy::Config {
    maple_proxy::Config::new(host, port, backend_url)
        .with_client_app(maple_proxy::ClientApp::Work)
        .with_pcr0_environment(pcr0)
        .with_debug(false)
        .with_cors(false)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalApiConfig {
    pub enabled: bool,
    pub port: u16,
}

impl Default for LocalApiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: crate::env::env_string("MAPLE_PORT")
                .and_then(|value| value.parse().ok())
                .filter(|port| *port != 0)
                .unwrap_or(8080),
        }
    }
}

impl LocalApiConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.port == 0 {
            return Err("Choose a local API port between 1 and 65535.".into());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalApiState {
    Stopped,
    Starting,
    Running,
    Stopping,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalApiStatus {
    pub config: LocalApiConfig,
    pub state: LocalApiState,
    pub active_requests: usize,
    pub error: Option<String>,
    pub owner: Option<String>,
    pub revision: u64,
}

impl Default for LocalApiStatus {
    fn default() -> Self {
        Self {
            config: LocalApiConfig::default(),
            state: LocalApiState::Stopped,
            active_requests: 0,
            error: None,
            owner: None,
            revision: 0,
        }
    }
}

/// The CLI retains its opt-in browser mode. The host-owned service always calls
/// this with `false`, and never configures a server-held API key.
pub fn apply_browser_policy(app: Router, cors: bool) -> Router {
    if cors {
        app.layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
                .allow_headers(AllowHeaders::mirror_request()),
        )
    } else {
        app.layer(middleware::from_fn(
            |request: Request, next: Next| async move {
                if request.headers().contains_key(header::ORIGIN)
                    || request.headers().contains_key("sec-fetch-site")
                {
                    return StatusCode::FORBIDDEN.into_response();
                }
                next.run(request).await
            },
        ))
    }
}

#[derive(Clone)]
pub struct LocalApiManager {
    inner: Arc<ManagerInner>,
}

struct ManagerInner {
    api_url: String,
    pcr0: maple_proxy::Pcr0Environment,
    state: Mutex<ManagerState>,
    running: AsyncMutex<Option<Running>>,
    updates: watch::Sender<LocalApiStatus>,
}

struct ManagerState {
    generation: u64,
    cancel: CancellationToken,
    status: LocalApiStatus,
}

struct Running {
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl Drop for ManagerInner {
    fn drop(&mut self) {
        self.state.get_mut().unwrap().cancel.cancel();
        if let Some(running) = self.running.get_mut().take() {
            running.cancel.cancel();
            // The accept task owns a JoinSet: aborting it also aborts every
            // connection, including peers that have not sent request headers.
            running.task.abort();
        }
    }
}

impl ManagerInner {
    fn change(&self, change: impl FnOnce(&mut ManagerState)) -> LocalApiStatus {
        let mut state = self.state.lock().unwrap();
        change(&mut state);
        state.status.revision = state.status.revision.saturating_add(1);
        let status = state.status.clone();
        self.updates.send_replace(status.clone());
        status
    }

    fn current(&self, generation: u64) -> bool {
        let state = self.state.lock().unwrap();
        state.generation == generation && !state.cancel.is_cancelled()
    }

    fn finish(&self, generation: u64, phase: LocalApiState, error: Option<String>) {
        let mut state = self.state.lock().unwrap();
        if state.generation != generation {
            return;
        }
        state.status.state = phase;
        state.status.error = error;
        if phase == LocalApiState::Stopped {
            state.status.owner = None;
        }
        state.status.revision = state.status.revision.saturating_add(1);
        self.updates.send_replace(state.status.clone());
    }
}

impl LocalApiManager {
    pub fn new(api_url: String) -> Result<Self, String> {
        let api_url = maple_agent::maple_api::validate_api_url(&api_url)?;
        let pcr0 = maple_agent::open_secret_config::configured_pcr0_environment()?;
        let status = LocalApiStatus::default();
        let (updates, _) = watch::channel(status.clone());
        Ok(Self {
            inner: Arc::new(ManagerInner {
                api_url,
                pcr0,
                state: Mutex::new(ManagerState {
                    generation: 0,
                    cancel: CancellationToken::new(),
                    status,
                }),
                running: AsyncMutex::new(None),
                updates,
            }),
        })
    }

    pub fn status(&self) -> LocalApiStatus {
        self.inner.state.lock().unwrap().status.clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<LocalApiStatus> {
        self.inner.updates.subscribe()
    }

    pub fn generation(&self) -> u64 {
        self.inner.state.lock().unwrap().generation
    }

    #[cfg(test)]
    pub async fn listener_task_for_test(&self) -> Option<tokio::task::AbortHandle> {
        self.inner
            .running
            .lock()
            .await
            .as_ref()
            .map(|running| running.task.abort_handle())
    }

    pub fn stays_running(&self) -> bool {
        matches!(
            self.status().state,
            LocalApiState::Starting | LocalApiState::Running | LocalApiState::Stopping
        )
    }

    #[cfg(test)]
    pub async fn configure(
        &self,
        config: LocalApiConfig,
        owner: String,
    ) -> Result<LocalApiStatus, String> {
        self.configure_if_current(config, owner, self.generation())
            .await
    }

    /// Fence async account/store validation performed by the caller. Stop,
    /// logout or another configuration invalidates the captured generation.
    pub async fn configure_if_current(
        &self,
        config: LocalApiConfig,
        owner: String,
        expected: u64,
    ) -> Result<LocalApiStatus, String> {
        config.validate()?;
        if config.enabled {
            self.start_router(config, owner, Some(expected), None).await
        } else {
            Ok(self.stop_with_config(Some(config), Some(expected)).await)
        }
    }

    #[cfg(test)]
    pub async fn start(
        &self,
        config: LocalApiConfig,
        owner: String,
    ) -> Result<LocalApiStatus, String> {
        self.start_router(config, owner, None, None).await
    }

    async fn start_router(
        &self,
        config: LocalApiConfig,
        owner: String,
        expected: Option<u64>,
        router: Option<Router>,
    ) -> Result<LocalApiStatus, String> {
        config.validate()?;
        if !config.enabled {
            return Ok(self.stop_with_config(Some(config), expected).await);
        }
        if owner.is_empty() {
            return Err("Sign in on this computer before enabling its local API.".into());
        }
        // Invalidate immediately, before waiting for the lifecycle lock. A
        // delayed bind/start from the previous owner cannot publish afterward.
        let Some((generation, cancel)) = self.begin(Some(config.clone()), Some(owner), expected)
        else {
            return Ok(self.status());
        };
        let Some(mut running) = self.running_if_current(generation).await else {
            return Ok(self.status());
        };
        stop_running(&mut running).await;
        if !self.inner.current(generation) {
            return Ok(self.status());
        }
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, config.port));
        let listener = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(self.status()),
            listener = TcpListener::bind(addr) => listener,
        };
        let listener = match listener {
            Ok(listener) => listener,
            Err(error) => {
                let message = if error.kind() == io::ErrorKind::AddrInUse {
                    "That local API port is already in use. Choose another port."
                } else {
                    "The local API could not bind its loopback port. Check the port and try again."
                };
                self.inner
                    .finish(generation, LocalApiState::Failed, Some(message.into()));
                return Err(message.into());
            }
        };
        let router = router.unwrap_or_else(|| {
            maple_proxy::create_app(work_proxy_config(
                "127.0.0.1".into(),
                config.port,
                self.inner.api_url.clone(),
                self.inner.pcr0,
            ))
        });
        let requests = Arc::new(Semaphore::new(MAX_REQUESTS));
        let request_state = RequestState {
            manager: Arc::downgrade(&self.inner),
            cancel: cancel.clone(),
            requests,
        };
        let router = apply_browser_policy(
            router.layer(middleware::from_fn_with_state(request_state, admit_request)),
            false,
        );
        let mut state = self.inner.state.lock().unwrap();
        if state.generation != generation || cancel.is_cancelled() {
            return Ok(state.status.clone());
        }
        let task = tokio::spawn(serve(
            listener,
            router,
            cancel.clone(),
            Arc::downgrade(&self.inner),
            generation,
        ));
        *running = Some(Running { cancel, task });
        state.status.state = LocalApiState::Running;
        state.status.revision = state.status.revision.saturating_add(1);
        self.inner.updates.send_replace(state.status.clone());
        Ok(state.status.clone())
    }

    /// Suspend the listener while retaining the user's desired configuration.
    pub async fn stop(&self) -> LocalApiStatus {
        self.stop_with_config(None, None).await
    }

    async fn running_if_current(
        &self,
        generation: u64,
    ) -> Option<tokio::sync::MutexGuard<'_, Option<Running>>> {
        let running = self.inner.running.lock().await;
        // A newer operation may have acquired this mutex before us after
        // publishing its intent. Never tear down that newer listener.
        self.inner.current(generation).then_some(running)
    }

    fn begin(
        &self,
        config: Option<LocalApiConfig>,
        owner: Option<String>,
        expected: Option<u64>,
    ) -> Option<(u64, CancellationToken)> {
        let mut state = self.inner.state.lock().unwrap();
        if expected.is_some_and(|expected| expected != state.generation) {
            return None;
        }
        // Restoring the same account from another attached client must not
        // restart its already-running listener or interrupt inference streams.
        // Compare under the same lock as intent publication so a concurrent
        // Stop/configuration cannot be overwritten by this redundant restore.
        if expected.is_some()
            && state.status.state == LocalApiState::Running
            && !state.cancel.is_cancelled()
            && owner.is_some()
            && owner == state.status.owner
            && config.as_ref() == Some(&state.status.config)
        {
            return None;
        }
        state.cancel.cancel();
        state.generation = state.generation.wrapping_add(1);
        state.cancel = CancellationToken::new();
        if let Some(config) = config {
            state.status.config = config;
        }
        state.status.state = if owner.is_some() {
            LocalApiState::Starting
        } else {
            LocalApiState::Stopping
        };
        state.status.owner = owner;
        state.status.error = None;
        state.status.revision = state.status.revision.saturating_add(1);
        self.inner.updates.send_replace(state.status.clone());
        Some((state.generation, state.cancel.clone()))
    }

    async fn stop_with_config(
        &self,
        config: Option<LocalApiConfig>,
        expected: Option<u64>,
    ) -> LocalApiStatus {
        let Some((generation, _)) = self.begin(config, None, expected) else {
            return self.status();
        };
        let Some(mut running) = self.running_if_current(generation).await else {
            return self.status();
        };
        stop_running(&mut running).await;
        self.inner.finish(generation, LocalApiState::Stopped, None);
        self.status()
    }
}

async fn stop_running(running: &mut Option<Running>) {
    if let Some(mut running) = running.take() {
        running.cancel.cancel();
        if tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut running.task)
            .await
            .is_err()
        {
            running.task.abort();
            // Poll the aborted parent once more so its JoinSet is dropped.
            let _ = running.task.await;
        }
    }
}

async fn serve(
    listener: TcpListener,
    router: Router,
    cancel: CancellationToken,
    manager: Weak<ManagerInner>,
    generation: u64,
) {
    let connection_slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            _ = connections.join_next(), if !connections.is_empty() => {},
            incoming = listener.accept() => {
                let (stream, _) = match incoming {
                    Ok(incoming) => incoming,
                    Err(_) => {
                        if let Some(manager) = manager.upgrade() {
                            manager.finish(generation, LocalApiState::Failed, Some("The local API listener stopped unexpectedly. Turn it off and on to retry.".into()));
                        }
                        break;
                    }
                };
                let Ok(slot) = connection_slots.clone().try_acquire_owned() else {
                    drop(stream);
                    continue;
                };
                let cancel = cancel.clone();
                let service = TowerToHyperService::new(router.clone());
                connections.spawn(async move {
                    let _slot = slot;
                    let mut http = hyper::server::conn::http1::Builder::new();
                    http.timer(TokioTimer::new()).header_read_timeout(HEADER_TIMEOUT).max_headers(64);
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {},
                        _ = http.serve_connection(TokioIo::new(stream), service) => {},
                    }
                });
            }
        }
    }
    // Cancel both request setup and response polling; abort also closes sockets
    // whose peers are stalled while reading, writing or sending HTTP headers.
    cancel.cancel();
    drop(listener);
    connections.abort_all();
    while connections.join_next().await.is_some() {}
}

#[derive(Clone)]
struct RequestState {
    manager: Weak<ManagerInner>,
    cancel: CancellationToken,
    requests: Arc<Semaphore>,
}

struct RequestGuard {
    manager: Weak<ManagerInner>,
    _permit: OwnedSemaphorePermit,
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        if let Some(manager) = self.manager.upgrade() {
            manager.change(|state| {
                state.status.active_requests = state.status.active_requests.saturating_sub(1);
            });
        }
    }
}

async fn admit_request(
    axum::extract::State(state): axum::extract::State<RequestState>,
    request: Request,
    next: Next,
) -> Response {
    if state.cancel.is_cancelled() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if request.uri().path().starts_with("/v1/")
        && !request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .is_some_and(|key| !key.trim().is_empty())
    {
        return (
            StatusCode::UNAUTHORIZED,
            "Send a Maple API key as Authorization: Bearer <key>.",
        )
            .into_response();
    }
    // Admission precedes the shared router's Bytes extractor, so rejected
    // uploads cannot occupy its 50 MiB body buffer while waiting for a slot.
    let Ok(permit) = state.requests.clone().try_acquire_owned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "The local API is busy. Try again after an active request finishes.",
        )
            .into_response();
    };
    if let Some(manager) = state.manager.upgrade() {
        manager.change(|state| state.status.active_requests += 1);
    }
    let guard = RequestGuard {
        manager: state.manager,
        _permit: permit,
    };
    let (parts, body) = request.into_parts();
    let upload_cancel = state.cancel.clone();
    let upload_deadline = tokio::time::Instant::now() + UPLOAD_TIMEOUT;
    let upload = futures_util::stream::unfold(
        (body.into_data_stream(), upload_cancel),
        move |(mut body, cancel)| async move {
            let chunk = tokio::select! {
                biased;
                _ = cancel.cancelled() => return None,
                result = tokio::time::timeout_at(upload_deadline, body.next()) => result,
            };
            match chunk {
                Ok(Some(chunk)) => Some((chunk.map_err(io::Error::other), (body, cancel))),
                Ok(None) => None,
                Err(_) => Some((
                    Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "Local API upload timed out",
                    )),
                    (body, cancel),
                )),
            }
        },
    );
    let request = Request::from_parts(parts, Body::from_stream(upload));
    let response = tokio::select! {
        biased;
        _ = state.cancel.cancelled() => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        response = next.run(request) => response,
    };
    let (parts, body) = response.into_parts();
    // Keep the permit and counter through streaming completion or disconnect,
    // rather than releasing them as soon as response headers are available.
    let response = futures_util::stream::unfold(
        (body.into_data_stream(), guard, state.cancel),
        |(mut body, guard, cancel)| async move {
            let chunk = tokio::select! {
                biased;
                _ = cancel.cancelled() => return None,
                chunk = body.next() => chunk,
            };
            chunk.map(|chunk| (chunk, (body, guard, cancel)))
        },
    );
    Response::from_parts(parts, Body::from_stream(response))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Bytes,
        routing::{get, post},
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
        sync::Notify,
    };

    #[test]
    fn local_api_and_proxy_cli_keep_work_identity_with_an_api_key() {
        let config = work_proxy_config(
            "127.0.0.1".into(),
            8080,
            "http://127.0.0.1:3000".into(),
            maple_sdk::Pcr0Environment::Development,
        )
        .with_api_key("fixture-key".into());
        assert_eq!(config.client_app, Some(maple_proxy::ClientApp::Work));
        assert!(config.default_api_key.is_some());
        assert!(!config.enable_cors);
    }

    fn config() -> LocalApiConfig {
        let socket = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        LocalApiConfig {
            enabled: true,
            port: socket.local_addr().unwrap().port(),
        }
    }

    fn manager() -> LocalApiManager {
        LocalApiManager::new("http://127.0.0.1:3000".into()).unwrap()
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap()
    }

    fn endpoint(config: &LocalApiConfig, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", config.port)
    }

    async fn listener_task(manager: &LocalApiManager) -> tokio::task::AbortHandle {
        let task = manager
            .listener_task_for_test()
            .await
            .expect("the running manager must own its accept task");
        assert!(!task.is_finished());
        task
    }

    async fn start(
        manager: &LocalApiManager,
        config: LocalApiConfig,
        router: Router,
    ) -> LocalApiStatus {
        manager
            .start_router(config, "local-api-test-owner".into(), None, Some(router))
            .await
            .unwrap()
    }

    async fn wait_for(manager: &LocalApiManager, predicate: impl Fn(&LocalApiStatus) -> bool) {
        let mut updates = manager.subscribe();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if predicate(&updates.borrow().clone()) {
                    break;
                }
                updates.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
    }

    async fn assert_closed(mut stream: TcpStream) {
        let mut data = [0; 1024];
        tokio::time::timeout(Duration::from_secs(1), async move {
            loop {
                match stream.read(&mut data).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        })
        .await
        .unwrap();
    }

    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn binds_loopback_reports_busy_port_and_retains_requested_configuration() {
        let manager = manager();
        let socket = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let config = LocalApiConfig {
            enabled: true,
            port: socket.local_addr().unwrap().port(),
        };
        let error = manager
            .start(config.clone(), "owner".into())
            .await
            .unwrap_err();
        assert!(error.contains("already in use"));
        let status = manager.status();
        assert_eq!(status.config, config);
        assert_eq!(status.state, LocalApiState::Failed);
        assert_eq!(status.owner.as_deref(), Some("owner"));
        assert_eq!(status.active_requests, 0);
        drop(socket);
        let running = start(
            &manager,
            config.clone(),
            Router::new().route("/health", get(|| async { "ok" })),
        )
        .await;
        assert_eq!(running.state, LocalApiState::Running);
        let task = listener_task(&manager).await;
        assert!(manager.stays_running());
        assert!(running.revision > status.revision);
        assert_eq!(
            client()
                .get(endpoint(&config, "/health"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let stopped = manager.stop().await;
        assert_eq!(stopped.config, config);
        assert_eq!(stopped.owner, None);
        assert_eq!(stopped.state, LocalApiState::Stopped);
        assert!(!manager.stays_running());
        assert!(task.is_finished());
    }

    #[tokio::test]
    async fn browser_and_missing_bearer_are_rejected_before_request_body_is_read() {
        let manager = manager();
        let config = config();
        let called = Arc::new(AtomicBool::new(false));
        let router = Router::new().route(
            "/v1/chat/completions",
            post({
                let called = called.clone();
                move |_: Bytes| {
                    let called = called.clone();
                    async move {
                        called.store(true, Ordering::SeqCst);
                        "ok"
                    }
                }
            }),
        );
        start(&manager, config.clone(), router).await;
        for browser in [
            Some("Origin: https://untrusted.example\r\n"),
            Some("Sec-Fetch-Site: cross-site\r\n"),
            None,
        ] {
            let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, config.port))
                .await
                .unwrap();
            let bearer = if browser.is_some() {
                "Authorization: Bearer fixture-key\r\n"
            } else {
                ""
            };
            stream.write_all(format!("POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{bearer}{}Content-Length: 50000000\r\n\r\n", config.port, browser.unwrap_or("")).as_bytes()).await.unwrap();
            let mut response = [0; 1024];
            let read = tokio::time::timeout(Duration::from_secs(1), stream.read(&mut response))
                .await
                .unwrap()
                .unwrap();
            let response = String::from_utf8_lossy(&response[..read]);
            assert!(
                response.contains(if browser.is_some() { "403" } else { "401" }),
                "{response}"
            );
        }
        assert!(!called.load(Ordering::SeqCst));
        assert_eq!(manager.status().active_requests, 0);
        manager.stop().await;
    }

    #[tokio::test]
    async fn admission_limit_precedes_buffering_and_stop_cancels_request_setup() {
        let manager = manager();
        let config = config();
        let entered = Arc::new(Notify::new());
        let dropped = Arc::new(AtomicBool::new(false));
        let router = Router::new().route(
            "/v1/chat/completions",
            post({
                let entered = entered.clone();
                let dropped = dropped.clone();
                move || {
                    let entered = entered.clone();
                    let dropped = dropped.clone();
                    async move {
                        let _guard = DropFlag(dropped);
                        entered.notify_waiters();
                        std::future::pending::<&'static str>().await
                    }
                }
            }),
        );
        start(&manager, config.clone(), router).await;
        let mut callers = JoinSet::new();
        for _ in 0..MAX_REQUESTS {
            let url = endpoint(&config, "/v1/chat/completions");
            callers
                .spawn(async move { client().post(url).bearer_auth("fixture-key").send().await });
        }
        wait_for(&manager, |status| status.active_requests == MAX_REQUESTS).await;
        let mut excess = TcpStream::connect((Ipv4Addr::LOCALHOST, config.port))
            .await
            .unwrap();
        excess.write_all(format!("POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer fixture-key\r\nContent-Length: 50000000\r\n\r\n", config.port).as_bytes()).await.unwrap();
        let mut response = [0; 1024];
        let read = tokio::time::timeout(Duration::from_secs(1), excess.read(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(String::from_utf8_lossy(&response[..read]).contains("503"));
        let task = listener_task(&manager).await;
        let stopped = tokio::time::timeout(Duration::from_secs(3), manager.stop())
            .await
            .unwrap();
        assert_eq!(stopped.active_requests, 0);
        assert!(dropped.load(Ordering::SeqCst));
        while let Some(result) = callers.join_next().await {
            assert!(result.unwrap().is_err());
        }
        assert!(task.is_finished());
    }

    #[tokio::test]
    async fn streaming_permit_lives_until_disconnect_and_stop_cancels_slow_readers() {
        let manager = manager();
        let config = config();
        let dropped = Arc::new(AtomicBool::new(false));
        let router = Router::new().route(
            "/v1/chat/completions",
            post({
                let dropped = dropped.clone();
                move || {
                    let guard = DropFlag(dropped.clone());
                    async move {
                        Body::from_stream(futures_util::stream::unfold(
                            (0_u64, guard),
                            |(count, guard)| async move {
                                tokio::task::yield_now().await;
                                Some((
                                    Ok::<_, io::Error>(Bytes::from(vec![b'x'; 64 * 1024])),
                                    (count + 1, guard),
                                ))
                            },
                        ))
                    }
                }
            }),
        );
        start(&manager, config.clone(), router).await;
        let response = client()
            .post(endpoint(&config, "/v1/chat/completions"))
            .bearer_auth("fixture-key")
            .send()
            .await
            .unwrap();
        wait_for(&manager, |status| status.active_requests == 1).await;
        assert!(!dropped.load(Ordering::SeqCst));
        let task = listener_task(&manager).await;
        // Do not consume the stream. Connection cancellation must interrupt a
        // peer whose receive window fills, not wait for another body poll.
        let stopped = tokio::time::timeout(Duration::from_secs(3), manager.stop())
            .await
            .unwrap();
        assert_eq!(stopped.active_requests, 0);
        assert!(dropped.load(Ordering::SeqCst));
        drop(response);
        assert!(task.is_finished());
    }

    #[tokio::test]
    async fn ordinary_stream_disconnect_releases_upstream_and_admission_without_stop() {
        let manager = manager();
        let config = config();
        let dropped = Arc::new(AtomicBool::new(false));
        let router = Router::new().route(
            "/v1/chat/completions",
            post({
                let dropped = dropped.clone();
                move || {
                    let guard = DropFlag(dropped.clone());
                    async move {
                        Body::from_stream(futures_util::stream::unfold(
                            (true, guard),
                            |(first, guard)| async move {
                                if first {
                                    Some((
                                        Ok::<_, io::Error>(Bytes::from_static(b"first")),
                                        (false, guard),
                                    ))
                                } else {
                                    std::future::pending().await
                                }
                            },
                        ))
                    }
                }
            }),
        );
        start(&manager, config.clone(), router).await;
        let mut response = client()
            .post(endpoint(&config, "/v1/chat/completions"))
            .bearer_auth("fixture-key")
            .send()
            .await
            .unwrap();
        assert_eq!(response.chunk().await.unwrap().unwrap(), "first");
        assert_eq!(manager.status().active_requests, 1);
        drop(response);
        wait_for(&manager, |status| status.active_requests == 0).await;
        assert!(dropped.load(Ordering::SeqCst));
        assert_eq!(manager.status().state, LocalApiState::Running);
        manager.stop().await;
    }

    #[tokio::test]
    async fn identical_account_restore_preserves_live_sse_and_its_admission_permit() {
        let manager = manager();
        let config = config();
        let resume = Arc::new(Notify::new());
        let dropped = Arc::new(AtomicBool::new(false));
        let router = Router::new().route(
            "/v1/chat/completions",
            post({
                let resume = resume.clone();
                let dropped = dropped.clone();
                move || {
                    let guard = DropFlag(dropped.clone());
                    let resume = resume.clone();
                    async move {
                        let body = Body::from_stream(futures_util::stream::unfold(
                            (0, guard, resume),
                            |(index, guard, resume)| async move {
                                let chunk = match index {
                                    0 => Bytes::from_static(b"data: first\n\n"),
                                    1 => {
                                        resume.notified().await;
                                        Bytes::from_static(b"data: second\n\n")
                                    }
                                    _ => std::future::pending().await,
                                };
                                Some((Ok::<_, io::Error>(chunk), (index + 1, guard, resume)))
                            },
                        ));
                        ([(header::CONTENT_TYPE, "text/event-stream")], body)
                    }
                }
            }),
        );
        start(&manager, config.clone(), router).await;
        let mut response = client()
            .post(endpoint(&config, "/v1/chat/completions"))
            .bearer_auth("fixture-key")
            .send()
            .await
            .unwrap();
        assert_eq!(response.chunk().await.unwrap().unwrap(), "data: first\n\n");
        let before = manager.status();
        let generation = manager.generation();
        assert_eq!(before.active_requests, 1);
        let restored = manager
            .configure_if_current(config, "local-api-test-owner".into(), generation)
            .await
            .unwrap();
        assert_eq!(restored, before);
        assert_eq!(manager.generation(), generation);
        assert!(!dropped.load(Ordering::SeqCst));
        resume.notify_one();
        assert_eq!(response.chunk().await.unwrap().unwrap(), "data: second\n\n");
        assert_eq!(manager.status().active_requests, 1);
        let stopped = manager.stop().await;
        assert_eq!(stopped.active_requests, 0);
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn stop_closes_clients_stalled_before_headers_and_during_upload() {
        let manager = manager();
        let config = config();
        start(
            &manager,
            config.clone(),
            Router::new().route("/v1/chat/completions", post(|_: Bytes| async { "ok" })),
        )
        .await;
        let mut headers = TcpStream::connect((Ipv4Addr::LOCALHOST, config.port))
            .await
            .unwrap();
        headers
            .write_all(b"POST /v1/chat/completions HTTP/1.1\r\nHost: ")
            .await
            .unwrap();
        let mut upload = TcpStream::connect((Ipv4Addr::LOCALHOST, config.port))
            .await
            .unwrap();
        upload.write_all(format!("POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer fixture-key\r\nContent-Length: 1000\r\n\r\nx", config.port).as_bytes()).await.unwrap();
        wait_for(&manager, |status| status.active_requests == 1).await;
        assert_eq!(manager.stop().await.active_requests, 0);
        assert_closed(headers).await;
        assert_closed(upload).await;
    }

    #[tokio::test]
    async fn stop_invalidates_queued_start_and_a_previously_validated_configuration() {
        let manager = manager();
        let config = config();
        let lock = manager.inner.running.lock().await;
        let starting = {
            let manager = manager.clone();
            let config = config.clone();
            tokio::spawn(async move { manager.start(config, "old-owner".into()).await })
        };
        wait_for(&manager, |status| status.state == LocalApiState::Starting).await;
        let old_generation = manager.generation();
        let stopping = {
            let manager = manager.clone();
            tokio::spawn(async move { manager.stop().await })
        };
        wait_for(&manager, |status| status.state == LocalApiState::Stopping).await;
        drop(lock);
        starting.await.unwrap().unwrap();
        let stopped = stopping.await.unwrap();
        assert_eq!(stopped.state, LocalApiState::Stopped);
        assert_eq!(stopped.owner, None);
        let delayed = manager
            .configure_if_current(config.clone(), "old-owner".into(), old_generation)
            .await
            .unwrap();
        assert_eq!(delayed.state, LocalApiState::Stopped);
        assert_eq!(delayed.owner, None);
        assert!(manager.listener_task_for_test().await.is_none());
        let disabled = LocalApiConfig {
            enabled: false,
            ..config
        };
        assert_eq!(
            manager
                .configure(disabled.clone(), String::new())
                .await
                .unwrap()
                .config,
            disabled
        );
    }

    #[tokio::test]
    async fn dropping_last_manager_closes_owned_listener_and_connections() {
        let manager = manager();
        let config = config();
        start(&manager, config.clone(), Router::new()).await;
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, config.port))
            .await
            .unwrap();
        stream.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();
        let task = listener_task(&manager).await;
        drop(manager);
        tokio::time::timeout(Duration::from_secs(3), async {
            while !task.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dropping the manager must finish its owned accept task");
        assert_closed(stream).await;
    }

    #[tokio::test]
    async fn stale_published_intent_cannot_acquire_a_newer_runtime_for_teardown() {
        let manager = manager();
        let config = config();
        for owner in [Some("stale-start".to_owned()), None] {
            // Pause immediately after an old start/stop publishes intent, then
            // let a newer start publish and acquire the runtime mutex first.
            let (stale_generation, _) = manager.begin(None, owner, None).unwrap();
            start(
                &manager,
                config.clone(),
                Router::new().route("/health", get(|| async { "ok" })),
            )
            .await;
            assert!(manager.running_if_current(stale_generation).await.is_none());
            assert_eq!(
                client()
                    .get(endpoint(&config, "/health"))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::OK
            );
            assert_eq!(manager.status().state, LocalApiState::Running);
        }
        manager.stop().await;
    }

    #[tokio::test]
    async fn shared_cli_browser_policy_retains_opt_in_preflight_and_bearer_headers() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cancel = CancellationToken::new();
        let router = apply_browser_policy(
            Router::new().route("/v1/models", get(|| async { "ok" })),
            true,
        );
        let task = tokio::spawn(serve(listener, router, cancel.clone(), Weak::new(), 0));
        let url = format!("http://127.0.0.1:{port}/v1/models");
        let preflight = client()
            .request(Method::OPTIONS, &url)
            .header(header::ORIGIN, "https://caller.example")
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
            .header(
                header::ACCESS_CONTROL_REQUEST_HEADERS,
                "authorization,x-client-test",
            )
            .send()
            .await
            .unwrap();
        assert!(preflight.status().is_success());
        assert_eq!(
            preflight
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .unwrap(),
            "*"
        );
        let allowed = preflight
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_HEADERS)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(allowed.contains("authorization"));
        assert!(allowed.contains("x-client-test"));
        let response = client()
            .get(&url)
            .header(header::ORIGIN, "https://caller.example")
            .bearer_auth("fixture-key")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        cancel.cancel();
        task.await.unwrap();
    }
}
