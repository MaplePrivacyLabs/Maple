//! One on-demand process owns a profile's backend. Frontends attach over
//! OS-private IPC; closing a frontend never tears down another client's runtime.

pub(crate) mod hardening;
pub(crate) mod ipc;
#[cfg(test)]
mod lifecycle_tests;
mod passwords;
pub mod protocol;
pub(crate) mod receipts;
#[cfg(test)]
mod recovery_tests;
mod repair;
#[cfg(test)]
mod send_receipt_tests;
pub(crate) mod subscription;
#[cfg(test)]
mod subscription_tests;

use crate::backend::{self, LocalBackend};
use maple_agent::agent::AgentServiceEvent;
use maple_agent::store::{Store, StoreState};
use protocol::{ConnectionMode, Hello, IncomingMessage, Message, Request, SensitiveValue, Welcome};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Semaphore, broadcast, mpsc, oneshot};

const START_TIMEOUT: Duration = Duration::from_secs(15);
const IDLE_GRACE: Duration = Duration::from_secs(2);
const RECONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const DISCONNECT_GRACE: Duration = Duration::from_secs(30);
const MAX_CLIENTS: usize = 32;
const MAX_CALLS: usize = 16;
type Pending = HashMap<u64, oneshot::Sender<Result<SensitiveValue, String>>>;

fn decode_response<R: DeserializeOwned>(method: &str, value: SensitiveValue) -> Result<R, String> {
    let result = if matches!(
        method,
        "generate_store_passphrase" | "request_account_deletion" | "create_api_key" | "oauth_start"
    ) {
        // These small responses contain secrets. Borrowing creates the typed
        // value while the original JSON remains guarded through success or a
        // schema error. History and attachment pages retain their move path.
        R::deserialize(&*value)
    } else {
        serde_json::from_value(value.into_value())
    };
    result.map_err(|_| "Incompatible Maple host response; restart Maple".into())
}

fn deliver_response(state: &ClientState, id: u64, result: Result<SensitiveValue, String>) {
    if let Some(sender) = state
        .pending
        .lock()
        .unwrap()
        .as_mut()
        .and_then(|pending| pending.remove(&id))
    {
        // A cancelled receiver returns ownership here, so its reply is wiped
        // just as a response with no remaining pending caller is wiped.
        let _ = sender.send(result);
    }
}

fn password_creation_allowed(
    debug: bool,
    method: &str,
    params: &serde_json::Value,
    setup_acknowledgement: bool,
) -> bool {
    if debug {
        return true;
    }
    match method {
        "set_store_password" | "change_store_password" => false,
        "setup_store" if !setup_acknowledgement => !params
            .get("password")
            .is_some_and(|password| !password.is_null()),
        _ => true,
    }
}

struct ClientState {
    pending: Mutex<Option<Pending>>,
    user_id: RwLock<Option<String>>,
    store: RwLock<StoreState>,
    generation: AtomicU64,
}

impl ClientState {
    fn disconnect(&self) {
        // Closing these senders distinguishes transport loss from a host's
        // application error, even if reconnection wins the response race.
        let mut pending = self.pending.lock().unwrap();
        pending.take();
        self.generation.fetch_add(1, Ordering::Relaxed);
    }
}

struct PendingCall<'a> {
    state: &'a ClientState,
    id: u64,
}
impl Drop for PendingCall<'_> {
    fn drop(&mut self) {
        if let Some(pending) = self.state.pending.lock().unwrap().as_mut() {
            pending.remove(&self.id);
        }
    }
}

pub struct RemoteClient {
    endpoint: ipc::Endpoint,
    requests: mpsc::Sender<protocol::SensitiveRequest>,
    state: Arc<ClientState>,
    next_id: AtomicU64,
    #[cfg_attr(not(feature = "desktop"), allow(dead_code))]
    events: tokio::sync::Mutex<Option<mpsc::Receiver<AgentServiceEvent>>>,
    permits: Semaphore,
}

impl RemoteClient {
    pub fn store_state(&self) -> StoreState {
        self.state.store.read().unwrap().clone()
    }

    pub async fn connect(api_url: &str) -> Result<Self, String> {
        let endpoint = ipc::Endpoint::new(api_url)?;
        let (connection, welcome) = connect(&endpoint, ConnectionMode::Client, true).await?;
        Ok(Self::attach(endpoint, connection, welcome))
    }

    pub async fn connect_remote(api_url: &str, peer: String) -> Result<Self, String> {
        let mut endpoint = ipc::Endpoint::new(api_url)?;
        endpoint.remote_host = Some(peer);
        let (connection, welcome) = connect(&endpoint, ConnectionMode::Client, true).await?;
        Ok(Self::attach(endpoint, connection, welcome))
    }

    #[cfg(all(test, feature = "desktop"))]
    pub(crate) fn attach_for_test(
        endpoint: ipc::Endpoint,
        connection: ipc::Connection,
        welcome: Welcome,
    ) -> Self {
        Self::attach(endpoint, connection, welcome)
    }

    fn attach(endpoint: ipc::Endpoint, connection: ipc::Connection, welcome: Welcome) -> Self {
        let state = Arc::new(ClientState {
            pending: Mutex::new(Some(HashMap::new())),
            user_id: RwLock::new(welcome.user_id),
            store: RwLock::new(welcome.store),
            generation: AtomicU64::new(0),
        });
        let (requests, request_rx) = mpsc::channel::<protocol::SensitiveRequest>(MAX_CALLS);
        let (event_tx, event_rx) = mpsc::channel(256);
        tokio::spawn(maintain_connection(
            endpoint.clone(),
            connection,
            state.clone(),
            request_rx,
            event_tx,
        ));
        Self {
            endpoint,
            requests,
            state,
            next_id: AtomicU64::new(1),
            events: tokio::sync::Mutex::new(Some(event_rx)),
            permits: Semaphore::new(MAX_CALLS),
        }
    }

    pub async fn detach(&self) {
        // Normal desktop Quit is explicit, unlike a socket failure. Bounded
        // so an unresponsive host cannot prevent the window from closing.
        let _ = tokio::time::timeout(Duration::from_secs(2), self.call::<()>("detach", ())).await;
    }

    pub fn user_id(&self) -> Option<String> {
        self.state.user_id.read().unwrap().clone()
    }

    #[cfg_attr(not(feature = "desktop"), allow(dead_code))]
    pub async fn take_events(&self) -> Option<mpsc::Receiver<AgentServiceEvent>> {
        self.events.lock().await.take()
    }

    pub async fn call<R: DeserializeOwned>(
        &self,
        method: &str,
        params: impl Serialize,
    ) -> Result<R, String> {
        let send_id = (method == "send_message").then(|| uuid::Uuid::new_v4().to_string());
        self.call_identified(method, params, send_id).await
    }

    pub async fn call_identified<R: DeserializeOwned>(
        &self,
        method: &str,
        params: impl Serialize,
        send_id: Option<String>,
    ) -> Result<R, String> {
        let mut params = serde_json::to_value(params)
            .map_err(|_| "Could not encode host request".to_string())?;
        for _ in 0..3 {
            let (result, generation) = self.call_value(method, params, send_id.clone()).await?;
            if method == "store_status"
                && result
                    .get("connection_rebind_required")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
            {
                // This query is safe to repeat; mutations are never replayed.
                // Do not return Open to a caller until its replacement socket
                // is ready for the ordinary command it is about to issue.
                tokio::time::timeout(RECONNECT_TIMEOUT, async {
                    loop {
                        if self.state.generation.load(Ordering::Acquire) != generation
                            && self.state.pending.lock().unwrap().is_some()
                        {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .map_err(|_| {
                    "The Maple host could not reconnect to the new local data".to_string()
                })?;
                params = serde_json::Value::Null;
                continue;
            }
            return decode_response(method, result);
        }
        Err("Maple local data changed repeatedly. Reconnect and retry.".into())
    }

    async fn call_value(
        &self,
        method: &str,
        params: serde_json::Value,
        send_id: Option<String>,
    ) -> Result<(SensitiveValue, u64), String> {
        let generation = self.state.generation.load(Ordering::Relaxed);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = protocol::SensitiveRequest(Request {
            id,
            method: method.into(),
            params,
            send_id: send_id.clone(),
        });
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|_| "Host disconnected".to_string())?;
        let (sender, reply) = oneshot::channel();
        {
            let mut pending = self.state.pending.lock().unwrap();
            if generation != self.state.generation.load(Ordering::Relaxed) {
                return Err("The Maple host disconnected before this operation was sent.".into());
            }
            pending
                .as_mut()
                .ok_or("The Maple host disconnected")?
                .insert(id, sender);
        }
        let _pending = PendingCall {
            state: &self.state,
            id,
        };
        let sent = self.requests.send(request).await;
        if sent.is_err() {
            self.state.disconnect();
        }
        let result = match (sent, reply.await) {
            (Ok(()), Ok(Err(error)))
                if error.starts_with("Send outcome unknown") && send_id.is_some() =>
            {
                SensitiveValue::new(
                    recover_send(&self.endpoint, send_id.as_deref().unwrap()).await?,
                )
            }
            (Ok(()), Ok(result)) => result?,
            (_, _) => match send_id {
                Some(id) => SensitiveValue::new(recover_send(&self.endpoint, &id).await?),
                None => {
                    return Err("The Maple host disconnected. Reconnect before retrying.".into());
                }
            },
        };
        Ok((result, generation))
    }
}

/// Own the socket independently of the UI. Reconnection never starts a host or
/// replays a request: pending callers fail (sends query their admission receipt).
async fn maintain_connection(
    endpoint: ipc::Endpoint,
    mut connection: ipc::Connection,
    state: Arc<ClientState>,
    mut requests: mpsc::Receiver<protocol::SensitiveRequest>,
    events: mpsc::Sender<AgentServiceEvent>,
) {
    loop {
        let graceful = client_connection(connection, &state, &mut requests, &events).await;
        state.disconnect();
        if graceful || requests.is_closed() || events.is_closed() {
            return;
        }
        let recovery = async {
            events
                .send(AgentServiceEvent::HostReconnecting)
                .await
                .map_err(|_| ())?;
            let mut delay = Duration::from_millis(250);
            loop {
                if requests.is_closed() || events.is_closed() {
                    return Err(());
                }
                // No startup arbitration or spawn here: explicit host stop and
                // crashes must never be undone by a frontend reconnect loop.
                let connected = connect(&endpoint, ConnectionMode::Client, false).await;
                if endpoint.remote_host.is_some()
                    && connected
                        .as_ref()
                        .err()
                        .is_some_and(|error| error.starts_with("Remote access ended:"))
                {
                    let reason = connected.err().unwrap();
                    let _ = events
                        .send(AgentServiceEvent::RemoteAccessEnded(reason))
                        .await;
                    return Err(());
                }
                if let Ok((next, welcome)) = connected {
                    *state.store.write().unwrap() = welcome.store.clone();
                    // Publish the ready replacement before any Open/account
                    // event can trigger fresh status or snapshot calls.
                    *state.pending.lock().unwrap() = Some(HashMap::new());
                    events
                        .send(AgentServiceEvent::StoreStateChanged {
                            state: welcome.store.clone(),
                        })
                        .await
                        .map_err(|_| ())?;
                    if welcome.store == StoreState::Open {
                        *state.user_id.write().unwrap() = welcome.user_id.clone();
                        events
                            .send(AgentServiceEvent::AccountChanged(welcome.user_id))
                            .await
                            .map_err(|_| ())?;
                    }
                    events
                        .send(AgentServiceEvent::HostReconnected)
                        .await
                        .map_err(|_| ())?;
                    return Ok::<_, ()>(next);
                }
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(5));
            }
        };
        let recovered = if endpoint.remote_host.is_some() {
            recovery.await
        } else {
            tokio::time::timeout(RECONNECT_TIMEOUT, recovery)
                .await
                .unwrap_or(Err(()))
        };
        match recovered {
            Ok(next) => connection = next,
            _ => {
                state.disconnect();
                let _ = events.send(AgentServiceEvent::HostDisconnected).await;
                return;
            }
        }
    }
}

/// Reads are never cancelled midway through a frame. All queues are bounded;
/// an event overflow coalesces into a resync while replies keep flowing.
async fn client_connection(
    connection: ipc::Connection,
    state: &ClientState,
    requests: &mut mpsc::Receiver<protocol::SensitiveRequest>,
    events: &mpsc::Sender<AgentServiceEvent>,
) -> bool {
    let (mut read, mut write) = tokio::io::split(connection);
    let (messages, mut incoming) = mpsc::channel(256);
    let reader = tokio::spawn(async move {
        while let Ok(message) = protocol::read::<IncomingMessage>(&mut read).await {
            if messages.send(message).await.is_err() {
                break;
            }
        }
    });
    let _reader = AbortOnDrop(reader.abort_handle());
    let mut resync_pending = false;
    loop {
        tokio::select! {
            permit = events.reserve(), if resync_pending => {
                match permit { Ok(permit) => { permit.send(AgentServiceEvent::HostReconnected); resync_pending=false; }, Err(_) => return true }
            }
            request = requests.recv() => {
                let Some(request) = request else {
                    let _ = tokio::time::timeout(Duration::from_secs(2), protocol::write(&mut write, &Request {
                        id: 0, send_id: None, method: "detach".into(), params: serde_json::Value::Null,
                    })).await;
                    return true;
                };
                // A request queued before a disconnect lost its caller. It
                // must not become a mutation on the replacement connection.
                if !state.pending.lock().unwrap().as_ref().is_some_and(|pending| pending.contains_key(&request.id)) {
                    continue;
                }
                if !matches!(tokio::time::timeout(START_TIMEOUT, protocol::write(&mut write, &request)).await, Ok(Ok(()))) {
                    return false;
                }
                if request.method == "detach" {
                    // No reconnection after an intentional detach, even if
                    // the acknowledgement is lost or the host is unresponsive.
                    let _ = tokio::time::timeout(Duration::from_secs(2), async {
                    while let Some(message) = incoming.recv().await {
                        if let IncomingMessage::Response { id, result } = message {
                            deliver_response(state, id, result);
                            if id == request.id { break; }
                        }
                    }
                    }).await;
                    return true;
                }
            }
            message = incoming.recv() => match message {
                Some(IncomingMessage::Resync) => { resync_pending = true; },
                Some(IncomingMessage::Closed { reason }) => {
                    let _ = events.send(AgentServiceEvent::RemoteAccessEnded(reason)).await;
                    return true;
                }
                Some(IncomingMessage::Response { id, result }) => {
                    deliver_response(state, id, result);
                }
                Some(IncomingMessage::Event(event)) => {
                    match &*event {
                        AgentServiceEvent::AccountChanged(user_id) => *state.user_id.write().unwrap() = user_id.clone(),
                        AgentServiceEvent::StoreStateChanged { state: store } => *state.store.write().unwrap() = store.clone(),
                        _ => {}
                    }
                    if !resync_pending && events.try_send(*event).is_err() { resync_pending = true; }
                }
                None => return false,
            },
        }
    }
}

#[cfg(test)]
async fn handshake(
    stream: ipc::Connection,
    profile: &str,
    mode: ConnectionMode,
) -> Result<(ipc::Connection, Welcome), String> {
    handshake_scoped(stream, profile, mode, None).await
}

async fn handshake_scoped(
    mut stream: ipc::Connection,
    profile: &str,
    mode: ConnectionMode,
    remote_host: Option<String>,
) -> Result<(ipc::Connection, Welcome), String> {
    protocol::write(
        &mut stream,
        &Hello {
            version: protocol::VERSION,
            profile: profile.into(),
            build: protocol::BUILD.into(),
            mode,
            remote_host,
        },
    )
    .await?;
    let welcome: Result<Welcome, String> = protocol::read(&mut stream).await?;
    let welcome = welcome?;
    if mode != ConnectionMode::Stop
        && (welcome.version != protocol::VERSION || welcome.build != protocol::BUILD)
    {
        return Err(
            "A different Maple host version is running. Quit that host before opening this build."
                .into(),
        );
    }
    Ok((stream, welcome))
}

async fn connect(
    endpoint: &ipc::Endpoint,
    mode: ConnectionMode,
    start: bool,
) -> Result<(ipc::Connection, Welcome), String> {
    if let Ok(stream) = endpoint.connect().await {
        return tokio::time::timeout(
            START_TIMEOUT,
            handshake_scoped(
                stream,
                &endpoint.profile,
                mode,
                endpoint.remote_host.clone(),
            ),
        )
        .await
        .map_err(|_| "Maple host did not answer".to_string())?;
    }
    if !start {
        return Err("No Maple host is running for this profile".into());
    }
    let startup = endpoint.lock("startup.lock")?;
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    loop {
        match startup.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await
            }
            Err(error) => return Err(format!("Cannot acquire Maple host startup lock: {error}")),
        }
    }
    if let Ok(stream) = endpoint.connect().await {
        return tokio::time::timeout_at(
            deadline,
            handshake_scoped(
                stream,
                &endpoint.profile,
                mode,
                endpoint.remote_host.clone(),
            ),
        )
        .await
        .map_err(|_| "Maple host did not answer".to_string())?;
    }
    spawn_host()?;
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    loop {
        if let Ok(stream) = endpoint.connect().await {
            let result = tokio::time::timeout_at(
                deadline,
                handshake_scoped(
                    stream,
                    &endpoint.profile,
                    mode,
                    endpoint.remote_host.clone(),
                ),
            )
            .await
            .map_err(|_| "Maple host did not answer".to_string())?;
            drop(startup);
            return result;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("Maple host did not start. See the profile's maple-agent.log.".into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn spawn_host() -> Result<(), String> {
    let mut command = crate::self_launch::command()?;
    command
        .args(["host", "serve"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Host startup must not choose the first editor's project as everyone's
    // default working directory. Each task carries its own explicit cwd.
    if let Some(home) = dirs::home_dir() {
        command.current_dir(home);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Only async-signal-safe setsid between fork and exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x00000008 | 0x00000200 | 0x01000000); // detached, new group, break away from client job
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("Cannot start Maple host: {e}"))?;
    // Reap on Unix while the parent remains alive; the host is not kill-on-drop.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Restore host availability after an update failed before committing its swap.
/// This explicit recovery never replays work; ordinary reconnect loops still
/// cannot undo a deliberate host stop. Call only after the update fence drops.
#[cfg(feature = "desktop")]
pub async fn recover_after_failed_update(api_url: &str) -> Result<(), String> {
    let endpoint = ipc::Endpoint::new(api_url)?;
    let (connection, _) = connect(&endpoint, ConnectionMode::Client, true).await?;
    drop(connection);
    Ok(())
}

pub fn run(api_url: String) -> Result<(), String> {
    let endpoint = ipc::Endpoint::new(&api_url)?;
    let _previous_owner = endpoint.previous_owner_guard()?;
    let ownership = endpoint.ownership_lock()?;
    let runtime = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    runtime.block_on(async {
        // The previous owner removes its listener before draining runs and
        // children. A replacement waits for that drain instead of exiting on
        // the first locked attempt. Never open shared state before ownership.
        wait_for_ownership(&ownership, START_TIMEOUT).await?;
        let store = Arc::new(
            Store::unavailable(backend::store_paths(), StoreState::Maintenance)
                .map_err(|error| error.to_string())?,
        );
        let backend = Arc::new(LocalBackend::new_with_store(api_url, String::new(), store)?);
        serve(endpoint, backend).await
    })
}

async fn wait_for_ownership(file: &std::fs::File, timeout: Duration) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err("The previous Maple host is still running or shutting down. Try again after it exits.".into());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => return Err(format!("Cannot acquire Maple host ownership: {error}")),
        }
    }
}

/// Bound once before a local handshake. A frontend cannot carry admitted work
/// from an erased store into its replacement, even while waiting for maintenance.
#[derive(Clone, PartialEq, Eq)]
struct ConnectionGeneration(Option<String>, Option<String>);
impl ConnectionGeneration {
    fn capture(backend: &LocalBackend) -> Result<Self, String> {
        backend
            .store()
            .map(|store| {
                store
                    .lock_status()
                    .map(|s| Self(s.store_id, s.setup_operation_id))
                    .map_err(|_| "Local store identity is unavailable".to_owned())
            })
            .unwrap_or_else(|| Ok(Self(None, None)))
    }
    fn is_current(&self, backend: &LocalBackend) -> bool {
        Self::capture(backend).is_ok_and(|current| current == *self)
    }
    fn check(&self, backend: &LocalBackend, method: &str) -> Result<(), String> {
        if matches!(
            method,
            "store_status"
                | "host_status"
                | "store_progress"
                | "cancel_store_operation"
                | "repair_status"
                | "repair_progress"
                | "cancel_store_repair"
                | "detach"
                | "cancel_oauth"
                | "generate_store_passphrase"
                | "assess_store_password"
        ) || self.is_current(backend)
        {
            return Ok(());
        }
        Err("Maple local data changed. Reconnect before retrying this operation.".into())
    }
}

struct OwnerContext {
    admission: tokio::sync::RwLock<()>,
    operation: tokio::sync::Mutex<()>,
    draining: tokio::sync::watch::Sender<u64>,
    busy: AtomicBool,
    maintenance: AtomicBool,
    completion_notice: Mutex<Option<CompletionNotice>>,
    active_store_operation: Mutex<Option<ActiveStoreOperation>>,
    retained_failure: AtomicBool,
    shutdown_requested: AtomicBool,
    password_mode: AtomicBool,
    acp_clients: std::sync::atomic::AtomicUsize,
    passwords: passwords::PasswordHost,
    repairs: repair::RepairHost,
}

#[derive(Clone)]
struct ActiveStoreOperation {
    operation: String,
    ui_operation_id: Option<String>,
    baseline_run_id: u64,
}

struct ActiveStoreOperationGuard<'a>(&'a Mutex<Option<ActiveStoreOperation>>);
impl Drop for ActiveStoreOperationGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut operation) = self.0.lock() {
            operation.take();
        }
    }
}

#[derive(Clone, Serialize)]
struct CompletionNotice {
    operation_id: String,
    message: String,
    is_error: bool,
    /// Routine successes belong to their initiating surface. Only failures or
    /// incomplete recovery/metadata work need attention after that surface closes.
    requires_attention: bool,
    /// A live initiating Settings form may present this success inline. Other
    /// clients retain the latched fallback if that form or its reply was lost.
    inline_password_success: bool,
    ui_operation_id: Option<String>,
    mode_notice_id: Option<String>,
}

fn password_settings_operation(method: &str) -> bool {
    matches!(
        method,
        "set_store_password" | "change_store_password" | "remove_store_password"
    )
}

fn password_ui_operation_id(
    method: &str,
    params: &serde_json::Value,
) -> Result<Option<String>, String> {
    if !password_settings_operation(method) && method != "setup_store" {
        return Ok(None);
    }
    match params.get("ui_operation_id") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(id)) if id.len() == 36 => uuid::Uuid::parse_str(id)
            .map(|id| Some(id.to_string()))
            .map_err(|_| "Invalid password operation identifier".into()),
        _ => Err("Invalid password operation identifier".into()),
    }
}

fn pending_store_metadata(value: &serde_json::Value) -> bool {
    [
        "backup_pending",
        "synchronization_pending",
        "notice_pending",
        "reconciliation_pending",
    ]
    .iter()
    .any(|key| value.get(key).and_then(serde_json::Value::as_bool) == Some(true))
}

fn attach_mode_notice_receipt(
    value: &mut serde_json::Value,
    notice: maple_agent::store::StoreResult<Option<maple_agent::store::lifecycle::StoreModeNotice>>,
) {
    match notice {
        Ok(Some(notice)) => value["mode_change_notice"] = serde_json::json!(notice),
        // The password operation already committed. Failure to obtain its
        // receipt must warn, never claim that the operation rolled back.
        _ => value["notice_pending"] = serde_json::json!(true),
    }
}

fn completion_message(
    method: &str,
    result: &Result<serde_json::Value, String>,
) -> Option<CompletionNotice> {
    let message = match result {
        Err(error) => error.clone(),
        Ok(value) => {
            let pending = pending_store_metadata(value);
            let base = match method {
                "unlock"
                    if value
                        .get("used_previous_password")
                        .and_then(serde_json::Value::as_bool)
                        == Some(true) =>
                {
                    "That was your previous unlock password. Your current password is still active."
                }
                "unlock" if !pending => return None,
                "unlock" => "Maple is unlocked.",
                "set_store_password" => "Local data is now encrypted on this computer.",
                "change_store_password" => "Your unlock password changed on this computer.",
                "remove_store_password" => "Your unlock password was removed on this computer.",
                "erase_local_data" => "Local data was erased. Choose how to set up Maple.",
                "start_fresh" => "The current local data was deleted. Choose how to set up Maple.",
                "setup_store" => "Maple local data is ready.",
                "salvage_store" => {
                    "Readable local data was saved in a verified replacement. The original damaged store was deleted."
                }
                _ => return None,
            };
            if pending {
                format!(
                    "{base} The change is active, but backup or metadata synchronization is still pending. Keep Maple open and retry before relying on a backup."
                )
            } else {
                base.to_owned()
            }
        }
    };
    Some(CompletionNotice {
        operation_id: uuid::Uuid::new_v4().to_string(),
        message,
        is_error: result.is_err(),
        requires_attention: match result {
            Err(_) => true,
            Ok(value) => {
                pending_store_metadata(value)
                    || (method == "unlock"
                        && value
                            .get("used_previous_password")
                            .and_then(serde_json::Value::as_bool)
                            == Some(true))
            }
        },
        inline_password_success: password_settings_operation(method)
            && result
                .as_ref()
                .is_ok_and(|value| !pending_store_metadata(value)),
        ui_operation_id: None,
        mode_notice_id: None,
    })
}

struct BusyOperation<'a>(&'a AtomicBool);
impl Drop for BusyOperation<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
#[cfg(feature = "acp")]
struct AcpClient<'a>(&'a std::sync::atomic::AtomicUsize);
#[cfg(feature = "acp")]
impl Drop for AcpClient<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

const LOCKED_MESSAGE: &str =
    "Maple is locked. Open Maple or run `maple-agent unlock`, then try again.";
const SETUP_MESSAGE: &str = "Maple needs local store setup. Open Maple to continue.";

fn gate(state: &StoreState, method: &str) -> Result<(), String> {
    if *state == StoreState::Open {
        return Ok(());
    }
    let control = matches!(
        method,
        "store_status" | "host_status" | "detach" | "cancel_oauth"
    ) || (method == "erase_local_data"
        && !matches!(state, StoreState::Refused { .. }));
    let allowed = control
        || match state {
            StoreState::Locked => method == "unlock",
            StoreState::NeedsSetup => matches!(
                method,
                "setup_store" | "generate_store_passphrase" | "assess_store_password"
            ),
            StoreState::Maintenance => {
                matches!(
                    method,
                    "store_progress"
                        | "cancel_store_operation"
                        | "repair_status"
                        | "repair_progress"
                        | "cancel_store_repair"
                )
            }
            StoreState::Damaged { reason } => {
                method == "start_fresh"
                    || repair::handles(method)
                    || (*reason == maple_agent::store::DamageReason::Generation
                        && method == "unlock")
            }
            StoreState::Refused { .. } => false,
            StoreState::Open => true,
        };
    if allowed {
        return Ok(());
    }
    Err(match state {
        StoreState::Locked => LOCKED_MESSAGE.to_owned(),
        StoreState::NeedsSetup => SETUP_MESSAGE.to_owned(),
        StoreState::Maintenance => {
            "Maple is finishing a local store operation. Try again when it finishes.".into()
        }
        StoreState::Damaged { .. } => {
            "Maple's local store needs attention. Open Maple to continue.".into()
        }
        StoreState::Refused { reason } => reason.message().to_owned(),
        StoreState::Open => unreachable!(),
    })
}

fn operation_progress_value(
    active: Option<ActiveStoreOperation>,
    core: maple_agent::store::lifecycle::StoreProgress,
    repair: Option<serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let Some(active) = active else {
        let mut value =
            serde_json::to_value(maple_agent::store::lifecycle::StoreProgress::default())
                .map_err(|_| "Could not encode store progress")?;
        value["busy"] = serde_json::json!(false);
        return Ok(value);
    };
    let progress = if core.run_id != active.baseline_run_id {
        core
    } else {
        // Authentication, draining, KDF work and key-only rewraps have no
        // new export counters. A prior terminal snapshot is unrelated.
        maple_agent::store::lifecycle::StoreProgress {
            phase: maple_agent::store::lifecycle::OperationPhase::Preparing,
            ..Default::default()
        }
    };
    let mut value = match repair {
        Some(value) => value,
        None => serde_json::to_value(progress).map_err(|_| "Could not encode store progress")?,
    };
    value["busy"] = serde_json::json!(true);
    value["operation"] = serde_json::json!(active.operation);
    value["ui_operation_id"] = serde_json::json!(active.ui_operation_id);
    Ok(value)
}

impl OwnerContext {
    fn operation_progress(
        &self,
        store: &maple_agent::store::Store,
    ) -> Result<serde_json::Value, String> {
        // Keep metadata and worker snapshots in one interval. Otherwise a poll
        // could label operation B's new counters with operation A's caller ID.
        let active = self
            .active_store_operation
            .lock()
            .map_err(|_| "Store progress is unavailable")?;
        let value = operation_progress_value(
            active.clone(),
            store.store_progress(),
            self.repairs.progress(),
        );
        drop(active);
        value
    }

    fn new(store: Option<&Arc<Store>>) -> Self {
        let password_mode = store
            .and_then(|store| store.lock_status().ok())
            .is_some_and(|status| {
                status.mode == maple_agent::store::lifecycle::StoreMode::Password
            });
        Self {
            admission: tokio::sync::RwLock::new(()),
            operation: tokio::sync::Mutex::new(()),
            draining: tokio::sync::watch::channel(0).0,
            busy: AtomicBool::new(false),
            maintenance: AtomicBool::new(false),
            completion_notice: Mutex::new(None),
            active_store_operation: Mutex::new(None),
            retained_failure: AtomicBool::new(false),
            shutdown_requested: AtomicBool::new(false),
            password_mode: AtomicBool::new(password_mode),
            acp_clients: std::sync::atomic::AtomicUsize::new(0),
            passwords: passwords::PasswordHost::default(),
            repairs: repair::RepairHost::default(),
        }
    }

    fn state(&self, backend: &LocalBackend) -> StoreState {
        if self.maintenance.load(Ordering::Acquire) {
            StoreState::Maintenance
        } else {
            backend
                .store()
                .map_or(StoreState::Open, |store| store.state())
        }
    }

    fn needs_rebind(&self, backend: &LocalBackend, generation: &ConnectionGeneration) -> bool {
        // Rename phases can temporarily remove generation metadata. The
        // operation's admission writer prevents ordinary access until the
        // committed identity is installed; keep progress/status usable meanwhile.
        !self.maintenance.load(Ordering::Acquire) && !generation.is_current(backend)
    }

    fn keeps_host(&self, backend: &LocalBackend) -> bool {
        let state = self.state(backend);
        self.retained_failure.load(Ordering::Acquire)
            || self.busy.load(Ordering::Acquire)
            || state == StoreState::NeedsSetup
            || (self.password_mode.load(Ordering::Acquire)
                && matches!(state, StoreState::Open | StoreState::Maintenance))
    }

    fn drain_acp(&self) {
        self.draining
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }

    fn status(&self, backend: &LocalBackend) -> Result<serde_json::Value, String> {
        let store = backend.store().ok_or("Local storage is unavailable")?;
        let status = store.lock_status().map_err(|error| error.to_string())?;
        self.password_mode.store(
            status.mode == maple_agent::store::lifecycle::StoreMode::Password,
            Ordering::Release,
        );
        let mut value =
            serde_json::to_value(status).map_err(|_| "Could not encode store status")?;
        value["state"] = serde_json::to_value(self.state(backend))
            .map_err(|_| "Could not encode store state")?;
        // Release exposure remains gated on native G3 evidence. Local debug
        // builds permit exact-app validation without shipping this capability.
        value["password_settings_available"] = serde_json::json!(cfg!(debug_assertions));
        let persistence = serde_json::to_value(store.persistence_status())
            .map_err(|_| "Could not encode persistence status")?;
        value["persistence_failure"] = persistence
            .get("failure")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        value["persistence_status"] = persistence;
        value["completion_notice"] = serde_json::to_value(
            self.completion_notice
                .lock()
                .map_err(|_| "Store notice is unavailable")?
                .clone(),
        )
        .map_err(|_| "Could not encode store notice")?;
        value["mode_change_notice"] = if self.state(backend) == StoreState::Open {
            serde_json::to_value(
                store
                    .mode_change_notice()
                    .map_err(|error| error.to_string())?,
            )
            .map_err(|_| "Could not encode mode-change notice")?
        } else {
            serde_json::Value::Null
        };
        Ok(value)
    }

    async fn host_status(
        &self,
        backend: &LocalBackend,
        remote: &crate::remote::manager::Manager,
    ) -> Result<serde_json::Value, String> {
        let (running_tasks, background_tasks) = backend.store_activity().await?;
        let (remote_controllers, _) = remote.host_activity().await;
        serde_json::to_value(protocol::HostStatus {
            acp_clients: self.acp_clients.load(Ordering::Acquire),
            running_tasks,
            remote_controllers,
            background_tasks,
        })
        .map_err(|_| "Could not encode host status".into())
    }

    async fn dispatch(
        &self,
        backend: &LocalBackend,
        remote: &Arc<crate::remote::manager::Manager>,
        events: &broadcast::Sender<Arc<AgentServiceEvent>>,
        request: protocol::SensitiveRequest,
        receipts: Arc<receipts::Receipts>,
        generation: &ConnectionGeneration,
    ) -> Result<serde_json::Value, String> {
        let method_owned = request.method.clone();
        let method = method_owned.as_str();
        if let Some(error) = repair::retired_operation_error(method) {
            return Err(error.to_owned());
        }
        gate(&self.state(backend), method)?; // Must precede remote_* routing.
        if method == "store_status" {
            let mut status = self.status(backend)?;
            status["connection_rebind_required"] =
                serde_json::json!(self.needs_rebind(backend, generation));
            return Ok(status);
        }
        if method == "host_status" {
            return self.host_status(backend, remote).await;
        }
        let store = backend
            .store()
            .ok_or("Local storage is unavailable")?
            .clone();
        if method == "store_progress" {
            return self.operation_progress(&store);
        }
        if method == "cancel_store_operation" {
            let repair_cancelled = self.repairs.cancel();
            return Ok(serde_json::json!(
                store.cancel_store_operation() || repair_cancelled
            ));
        }
        if matches!(
            method,
            "repair_status" | "repair_progress" | "cancel_store_repair"
        ) {
            return self.repairs.dispatch(store, method, &request.params).await;
        }
        if method == "retry_store_persistence" {
            let _operation = self.operation.lock().await;
            self.busy.store(true, Ordering::Release);
            let _busy = BusyOperation(&self.busy);
            let _admission = self.admission.write().await;
            generation.check(backend, method)?;
            gate(&self.state(backend), method)?;
            let _account = backend.account_gate.write().await;
            backend.retry_store_persistence().await?;
            self.retained_failure.store(false, Ordering::Release);
            return Ok(serde_json::Value::Null);
        }
        let password_operation = passwords::changes_store(method);
        let repair_operation = repair::changes_store(method);
        let store_operation = password_operation || repair_operation;
        // Caller correlation is presentation metadata, never a credential or
        // authority to acknowledge anything. Missing/invalid IDs use fallback.
        let ui_operation_id = password_ui_operation_id(method, &request.params)?;
        if store_operation || method == "confirm_account_deletion" {
            let setup_acknowledgement = method == "setup_store"
                && passwords::is_setup_acknowledgement(&store, &request.params)?;
            if !cfg!(debug_assertions) && repair::requires_native_validation(method) {
                return Err(
                    "Local recovery changes are unavailable until native validation is complete."
                        .into(),
                );
            }
            if !password_creation_allowed(
                cfg!(debug_assertions),
                method,
                &request.params,
                setup_acknowledgement,
            ) {
                return Err("Password settings are unavailable in this release until native validation is complete.".into());
            }
            let _operation = self.operation.lock().await;
            generation.check(backend, method)?;
            gate(&self.state(backend), method)?;
            passwords::validate_before_drain(store.clone(), method, &request.params).await?;
            let destructive = matches!(method, "erase_local_data" | "start_fresh");
            let drain_operation = repair_operation
                || method == "confirm_account_deletion"
                || (!setup_acknowledgement && passwords::requires_drain(method, &request.params)?);
            self.busy.store(true, Ordering::Release);
            let _busy = BusyOperation(&self.busy);
            let _maintenance = (store_operation && drain_operation).then(|| {
                self.maintenance.store(true, Ordering::Release);
                BusyOperation(&self.maintenance)
            });
            let _progress = if store_operation {
                *self
                    .active_store_operation
                    .lock()
                    .map_err(|_| "Store progress is unavailable")? = Some(ActiveStoreOperation {
                    operation: method.to_owned(),
                    ui_operation_id: ui_operation_id.clone(),
                    baseline_run_id: store.store_progress().run_id,
                });
                Some(ActiveStoreOperationGuard(&self.active_store_operation))
            } else {
                None
            };
            if store_operation {
                *self
                    .completion_notice
                    .lock()
                    .map_err(|_| "Store notice is unavailable")? = None;
            }
            let (_exclusive_admission, _shared_admission) = if drain_operation {
                backend.begin_store_drain();
                self.drain_acp();
                remote.shutdown().await;
                (Some(self.exclusive_admission(backend).await), None)
            } else {
                (None, Some(self.admission.read().await))
            };
            if let Err(error) = generation.check(backend, method) {
                if store.state() == StoreState::Open {
                    backend.resume_store_admission();
                }
                return Err(error);
            }
            if store_operation && drain_operation {
                let _ = events.send(Arc::new(AgentServiceEvent::StoreStateChanged {
                    state: StoreState::Maintenance,
                }));
            }
            let mut result = async {
                if method == "confirm_account_deletion" {
                    // backend::dispatch takes its own account writer after ACP
                    // and remote have stopped, avoiding a lifetime-reader deadlock.
                    return dispatch_with_receipt(backend, request.into_request(), receipts).await;
                }
                let (_exclusive_account, _shared_account) = if drain_operation {
                    (Some(backend.account_gate.write().await), None)
                } else {
                    (None, Some(backend.account_gate.read().await))
                };
                generation.check(backend, method)?;
                gate(&store.state(), method)?;
                if drain_operation && !destructive {
                    let (running, background) = backend.store_activity().await?;
                    if running != 0 || background != 0 {
                        return Err("Wait for running tasks to finish.".into());
                    }
                }
                if destructive {
                    // Exact confirmation was validated under operation ordering.
                    // Stop/reap tools, retaining unsaved sessions until the reset
                    // commits. Saving the data being erased cannot veto consent.
                    backend.prepare_store_reset().await?;
                    let paths = store.paths().clone();
                    tokio::task::spawn_blocking(move || {
                        maple_agent::store::spill::SpillRoot::new(&paths)?.cleanup()
                    })
                    .await
                    .map_err(|_| "Local reset cleanup did not finish")?
                    .map_err(|error| error.to_string())?;
                } else if drain_operation {
                    // Ordinary changes and Quit still require every source write.
                    backend.shutdown_host().await?;
                }
                if drain_operation {
                    backend.clear_paging_cache()?;
                }
                if self.shutdown_requested.load(Ordering::Acquire) {
                    return Err(
                        "Maple is stopping. Retry the operation after reopening Maple.".into(),
                    );
                }
                let context = backend
                    .saved_user_id()
                    .into_iter()
                    .chain(Some(
                        hostname::get()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned(),
                    ))
                    .collect();
                let result = if destructive {
                    let reset = passwords::reset_store(store.clone(), method, &request.params).await?;
                    if reset.is_ok()
                        || matches!(
                            &reset,
                            Err(maple_agent::store::lifecycle::LifecycleError::RecoveryRequired { .. })
                        )
                    {
                        // Exact destructive confirmation also covers an intent
                        // whose durability is uncertain. Keep Maintenance and
                        // return its restart error, but release the old source
                        // so ordinary Quit can finish and cold recovery can run.
                        backend.forget_store_session().await?;
                    }
                    reset
                        .map_err(|error| error.to_string())
                        .and_then(|status| serde_json::to_value(status)
                            .map_err(|_| "Could not encode local store status".into()))
                } else if setup_acknowledgement {
                    // A release may acknowledge an already-installed password
                    // store. The dedicated core method rechecks Open and cannot
                    // become setup if this pre-admission observation went stale.
                    passwords::acknowledge_setup(store.clone(), &request.params).await
                } else if repair_operation {
                    self.repairs
                        .dispatch(store.clone(), method, &request.params)
                        .await
                } else {
                    self.passwords
                        .dispatch(store.clone(), method, &request.params, context)
                        .await
                };
                if store.state() == StoreState::Open
                    && matches!(method, "unlock" | "setup_store" | "salvage_store")
                {
                    backend.reload_open_store()?;
                }
                result
            }
            .await;
            if drain_operation && store.state() == StoreState::Open {
                backend.resume_store_admission();
            }
            self.busy.store(false, Ordering::Release);
            self.maintenance.store(false, Ordering::Release);
            if store_operation {
                // Capture this operation's exact durable notice while its
                // mutex is still held. A later UI acknowledgement cannot clear
                // another client's subsequent change or a replacement store.
                if password_settings_operation(method)
                    && let Ok(value) = &mut result
                    && !pending_store_metadata(value)
                {
                    attach_mode_notice_receipt(value, store.mode_change_notice());
                }
                let mut completion = completion_message(method, &result);
                if let Some(notice) = &mut completion {
                    notice.ui_operation_id = ui_operation_id;
                    if notice.inline_password_success {
                        notice.mode_notice_id = result
                            .as_ref()
                            .ok()
                            .and_then(|value| value.get("mode_change_notice"))
                            .and_then(|value| value.get("notice_id"))
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned);
                    }
                }
                if let (Ok(value), Some(notice)) = (&mut result, &completion) {
                    value["completion_operation_id"] = serde_json::json!(notice.operation_id);
                }
                *self
                    .completion_notice
                    .lock()
                    .map_err(|_| "Store notice is unavailable")? = completion;
            }
            let _ = self.status(backend);
            if store_operation && (drain_operation || store.state() != StoreState::Open) {
                // A key-only rewrap normally leaves Open unchanged. Still
                // report unexpected damage discovered during that cheap lane.
                let _ = events.send(Arc::new(AgentServiceEvent::StoreStateChanged {
                    state: store.state(),
                }));
            }
            // Restore only when this accepted operation actually drained it.
            // Refused unlock and key-only rewrap never touch either transport.
            if drain_operation {
                remote.shutdown().await;
                if backend.saved_user_id().is_some() && store.state() == StoreState::Open {
                    remote.restore().await;
                }
            }
            return result;
        }
        let _admission = self.admission.read().await;
        generation.check(backend, method)?;
        gate(&self.state(backend), method)?;
        if method == "acknowledge_mode_change_notice" {
            let (store_id, notice_id): (&str, &str) =
                serde::Deserialize::deserialize(&request.params)
                    .map_err(|_| "Invalid mode-change notice acknowledgement")?;
            if store_id.is_empty()
                || store_id.len() > 128
                || notice_id.is_empty()
                || notice_id.len() > 128
            {
                return Err("Invalid mode-change notice acknowledgement".into());
            }
            let (store_id, notice_id) = (store_id.to_owned(), notice_id.to_owned());
            let acknowledged = tokio::task::spawn_blocking(move || {
                store.acknowledge_mode_change_notice(&store_id, &notice_id)
            })
            .await
            .map_err(|_| "Could not acknowledge mode-change notice")?
            .map_err(|error| error.to_string())?;
            return Ok(serde_json::json!(acknowledged));
        }
        if passwords::handles(method) {
            let context = backend
                .saved_user_id()
                .into_iter()
                .chain(Some(
                    hostname::get()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                ))
                .collect();
            return self
                .passwords
                .dispatch(store, method, &request.params, context)
                .await;
        }
        if repair::handles(method) {
            return self.repairs.dispatch(store, method, &request.params).await;
        }
        if matches!(
            method,
            "remote_update_attach"
                | "remote_update_publish"
                | "remote_update_take"
                | "remote_update_complete"
        ) {
            let account = request
                .params
                .get("expected_account")
                .and_then(serde_json::Value::as_str)
                .ok_or("Invalid remote updater account")?;
            backend.authorize_account(account)?;
            return match method {
                "remote_update_attach" => {
                    let request: crate::remote_updates::AttachRequest =
                        serde_json::from_value(request.params.clone())
                            .map_err(|_| "Invalid desktop updater attachment")?;
                    serde_json::to_value(
                        backend
                            .remote_updates
                            .attach(request.expected_account)
                            .await?,
                    )
                    .map_err(|_| "Could not encode desktop updater response".to_owned())
                }
                "remote_update_publish" => {
                    backend
                        .remote_updates
                        .publish(
                            serde_json::from_value(request.params.clone())
                                .map_err(|_| "Invalid desktop updater publication")?,
                        )
                        .await?;
                    Ok(serde_json::Value::Null)
                }
                "remote_update_take" => serde_json::to_value(
                    backend
                        .remote_updates
                        .take(
                            serde_json::from_value(request.params.clone())
                                .map_err(|_| "Invalid desktop updater poll")?,
                        )
                        .await?,
                )
                .map_err(|_| "Could not encode desktop updater response".to_owned()),
                _ => {
                    backend
                        .remote_updates
                        .complete(
                            serde_json::from_value(request.params.clone())
                                .map_err(|_| "Invalid desktop updater completion")?,
                        )
                        .await?;
                    Ok(serde_json::Value::Null)
                }
            };
        }
        if method.starts_with("remote_") {
            remote.dispatch(request.into_request()).await
        } else {
            dispatch_with_receipt(backend, request.into_request(), receipts).await
        }
    }
    async fn exclusive_admission<'a>(
        &'a self,
        backend: &LocalBackend,
    ) -> tokio::sync::RwLockWriteGuard<'a, ()> {
        let admission = self.admission.write();
        tokio::pin!(admission);
        loop {
            // An already-admitted oauth_start may register its attempt after
            // the first cancellation. Keep revoking until admission is ours.
            // The OAuth publication guard remains responsible for late grants.
            backend.cancel_oauth();
            tokio::select! {
                guard = &mut admission => return guard,
                _ = tokio::time::sleep(Duration::from_millis(50)) => {},
            }
        }
    }

    async fn shutdown(
        &self,
        backend: &LocalBackend,
        remote: &Arc<crate::remote::manager::Manager>,
        events: &broadcast::Sender<Arc<AgentServiceEvent>>,
    ) -> Result<(), String> {
        self.shutdown_with_update(backend, remote, events, None)
            .await
    }

    async fn shutdown_with_update(
        &self,
        backend: &LocalBackend,
        remote: &Arc<crate::remote::manager::Manager>,
        events: &broadcast::Sender<Arc<AgentServiceEvent>>,
        mut update: Option<StopRequest>,
    ) -> Result<(), String> {
        let is_remote_update = update.as_ref().is_some_and(|u| u.operation.is_some());
        let mut stopping = if is_remote_update {
            None
        } else {
            self.shutdown_requested.store(true, Ordering::Release);
            Some(BusyOperation(&self.shutdown_requested))
        };
        let _operation = if is_remote_update {
            // A stale remote request has no authority to interrupt local work.
            // Refuse Busy without cancelling OAuth, recovery or store workers.
            self.operation.try_lock().map_err(|_| {
                let error =
                    "This computer is busy with a local operation. Try again after it finishes."
                        .to_owned();
                if let Some(reply) = update.as_mut().and_then(|u| u.accepted.take()) {
                    let _ = reply.send(Err(error.clone()));
                }
                error
            })?
        } else {
            // Administrative local Stop intentionally cancels local work while
            // waiting for its exclusive operation/admission guards.
            backend.cancel_oauth();
            let operation = self.operation.lock();
            tokio::pin!(operation);
            loop {
                // An accepted operation may still be draining before it registers
                // its worker; keep cancellation active across that whole interval.
                backend.cancel_oauth();
                self.repairs.cancel();
                if let Some(store) = backend.store() {
                    store.cancel_store_operation();
                }
                tokio::select! {
                    guard = &mut operation => break guard,
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {},
                }
            }
        };
        // Remote installation is ordered with password changes by operation,
        // and with account/revocation changes by exclusive local admission.
        // No remote RPC/account reader is held by this stop control connection.
        let remote_admission = if is_remote_update {
            let admission = tokio::time::timeout(Duration::from_secs(1), self.admission.write()).await.map_err(|_| {
                let error = "This computer is busy with a local operation. Try again after it finishes.".to_owned();
                if let Some(reply) = update.as_mut().and_then(|u| u.accepted.take()) {
                    let _ = reply.send(Err(error.clone()));
                }
                error
            })?;
            let operation = update
                .as_ref()
                .and_then(|u| u.operation)
                .expect("remote operation");
            let result = backend
                .remote_updates
                .authorize_stop(operation, backend.saved_user_id())
                .await;
            if let Some(reply) = update.as_mut().and_then(|u| u.accepted.take()) {
                let _ = reply.send(result.clone());
            }
            result?;
            if let Some(acknowledged) = update.as_mut().and_then(|u| u.acknowledged.take()) {
                tokio::time::timeout(Duration::from_secs(5), acknowledged)
                    .await
                    .map_err(|_| "Remote update stop acknowledgement timed out".to_owned())?
                    .map_err(|_| "Remote update stop acknowledgement was lost".to_owned())?;
            }
            Some(admission)
        } else {
            None
        };
        if is_remote_update {
            // Authorization and its local control acknowledgement both
            // succeeded. Only now may this request begin host shutdown.
            if let Some(request) = &update {
                request.started.store(true, Ordering::Release);
            }
            self.shutdown_requested.store(true, Ordering::Release);
            stopping = Some(BusyOperation(&self.shutdown_requested));
            backend.cancel_oauth();
        }
        let _stopping = stopping;
        self.busy.store(true, Ordering::Release);
        let _busy = BusyOperation(&self.busy);
        let remote_owner = backend.saved_user_id();
        let restore_remote = remote.stays_running();
        backend.begin_store_drain();
        self.drain_acp();
        remote.shutdown().await;
        let _admission = match remote_admission {
            Some(admission) => admission,
            None => self.exclusive_admission(backend).await,
        };
        let _account = backend.account_gate.write().await;
        // An already-admitted account operation may have restored its remote
        // node while Stop waited for admission. Close that late node as well;
        // no account operation can publish another under these exclusive gates.
        remote.shutdown().await;
        let result = async {
            backend.shutdown_host().await?;
            backend.clear_paging_cache()?;
            if let Some(store) = backend.store() {
                // Cleanup can fail while native connections and the retained
                // key are still usable. Close is the final irreversible step.
                if store.state() == StoreState::Open {
                    maple_agent::store::spill::SpillRoot::new(store.paths())
                        .and_then(|root| root.cleanup())
                        .map_err(|error| error.to_string())?;
                }
                store
                    .close_for_shutdown()
                    .map_err(|error| error.to_string())?;
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            self.retained_failure.store(true, Ordering::Release);
            let state = backend
                .store()
                .map_or(StoreState::Open, |store| store.state());
            if state == StoreState::Open {
                backend.resume_store_admission();
            }
            // Remote startup can take the account reader. Release its writer
            // while retaining admission/operation ownership, so a login or
            // generation replacement cannot race this restoration decision.
            drop(_account);
            if state == StoreState::Open
                && restore_remote
                && remote_owner.is_some()
                && backend.saved_user_id() == remote_owner
            {
                remote.restore().await;
            }
            let _ = events.send(Arc::new(AgentServiceEvent::StoreStateChanged { state }));
            return Err(error);
        }
        Ok(())
    }
}

struct StartupClient {
    connection: ipc::Connection,
    mode: ConnectionMode,
    generation: ConnectionGeneration,
}

struct StopRequest {
    operation: Option<[u8; 16]>,
    accepted: Option<tokio::sync::oneshot::Sender<Result<(), String>>>,
    acknowledged: Option<tokio::sync::oneshot::Receiver<()>>,
    // Shared only with the host's serve loop, never supplied by a client.
    // Expected refusals before this point are not failed host shutdowns.
    started: Arc<AtomicBool>,
}
impl StopRequest {
    fn local() -> Self {
        Self {
            operation: None,
            accepted: None,
            acknowledged: None,
            started: Arc::new(AtomicBool::new(false)),
        }
    }
}

async fn serve(endpoint: ipc::Endpoint, backend: Arc<LocalBackend>) -> Result<(), String> {
    // Accept and answer the handshake before migrations, settings or receipt I/O.
    let mut listener = endpoint.bind()?;
    let store = backend
        .store()
        .ok_or("Host store handle is missing")?
        .clone();
    let opening = store.clone();
    let startup_ready = Arc::new(AtomicBool::new(false));
    let initialize = tokio::task::spawn_blocking(move || {
        // Validate recovery formats and the generation before any spill sweep.
        // reload_open_store performs cleanup only after a compatible Open.
        let result = opening.initialize_recovered();
        if let Err(error) = result {
            log::warn!("Local store could not open: {error}");
        }
        initial_integrity_check(&opening);
        let receipts = Arc::new(receipts::Receipts::new(opening.clone()));
        let harness = if opening.state() == StoreState::Open {
            crate::settings::load_host_settings(&opening, None)?.effective_harness_instructions()
        } else {
            String::new()
        };
        Ok::<_, String>((receipts, harness))
    });
    tokio::pin!(initialize);
    let mut clients = tokio::task::JoinSet::new();
    let (stop_tx, mut stop_rx) = mpsc::channel(1);
    loop {
        tokio::select! {
            biased;
            ready = &mut initialize => {
                let (receipts, harness) = ready.map_err(|error| error.to_string())??;
                backend.set_harness_instructions(harness);
                if store.state() == StoreState::Open { backend.reload_open_store()?; }
                // Restored credentials already join an existing account. Account
                // creation and last-seen updates belong to explicit sign-in/save,
                // not a metadata write that can prevent host startup.
                startup_ready.store(true, Ordering::Release);
                return serve_runtime(endpoint, backend, receipts, listener, clients).await;
            }
            _ = stop_rx.recv() => {
                // Accepted Stop completes even if its requester disconnects.
                let _ = (&mut initialize).await;
                clients.abort_all();
                while clients.join_next().await.is_some() {}
                store.close().map_err(|error| error.to_string())?;
                return Ok(());
            }
            incoming = listener.accept(), if clients.len() < MAX_CLIENTS => {
                let mut connection = incoming?;
                let endpoint = endpoint.clone(); let stop = stop_tx.clone(); let backend = backend.clone(); let store = store.clone(); let ready = startup_ready.clone();
                clients.spawn(async move {
                    // ACP needs actual credentials; wait only within its handshake budget.
                    // Client handshakes can immediately show Maintenance.
                    let hello = tokio::time::timeout(START_TIMEOUT, protocol::read::<Hello>(&mut connection))
                        .await.map_err(|_| "Host handshake timed out".to_string())??;
                    if hello.mode == ConnectionMode::Acp {
                        let _ = tokio::time::timeout(START_TIMEOUT - Duration::from_secs(1), async {
                            while !ready.load(Ordering::Acquire) {
                                tokio::time::sleep(Duration::from_millis(10)).await;
                            }
                        }).await;
                    }
                    let state = if ready.load(Ordering::Acquire) { store.state() } else { StoreState::Maintenance };
                    let user = (state == StoreState::Open).then(|| backend.saved_user_id()).flatten();
                    let generation = if ready.load(Ordering::Acquire) {
                        ConnectionGeneration::capture(&backend)?
                    } else { ConnectionGeneration(None, None) };
                    let (mode, _) = accept_hello(&mut connection, hello, &endpoint.profile, user,
                        state, None, Some(&stop)).await?;
                    Ok::<_, String>(StartupClient { connection, mode, generation })
                });
            }
        }
    }
}

fn initial_integrity_check(store: &Store) {
    use maple_agent::store::integrity::{IntegrityCancellation, IntegrityKind, IntegrityOutcome};
    if store.state() != StoreState::Open {
        return;
    }
    match store.check_integrity(
        IntegrityKind::Quick,
        Duration::from_millis(100),
        &IntegrityCancellation::default(),
    ) {
        Ok(report) if report.outcome == IntegrityOutcome::Damaged => {
            log::warn!("Local store quick check found damage; ordinary access is disabled.")
        }
        Ok(_) => {} // A budget-limited partial is not a clean full receipt.
        Err(error) => log::warn!("Local store quick check did not complete: {error}"),
    }
}

fn store_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

#[cfg(test)]
async fn serve_with_receipts(
    endpoint: ipc::Endpoint,
    backend: Arc<LocalBackend>,
    receipts: Arc<receipts::Receipts>,
) -> Result<(), String> {
    let listener = endpoint.bind()?;
    serve_runtime(
        endpoint,
        backend,
        receipts,
        listener,
        tokio::task::JoinSet::new(),
    )
    .await
}

async fn serve_runtime(
    endpoint: ipc::Endpoint,
    backend: Arc<LocalBackend>,
    receipts: Arc<receipts::Receipts>,
    mut listener: ipc::Listener,
    mut startup_clients: tokio::task::JoinSet<Result<StartupClient, String>>,
) -> Result<(), String> {
    let (events, _) = broadcast::channel::<Arc<AgentServiceEvent>>(256);
    let (stop_tx, mut stop_rx) = mpsc::channel::<StopRequest>(1);
    let mut source = backend
        .take_events()
        .await
        .ok_or("Host event source already taken")?;
    let remote =
        crate::remote::manager::Manager::new(backend.clone(), events.clone(), receipts.clone());
    if backend
        .store()
        .is_none_or(|store| store.state() == StoreState::Open)
    {
        let resume = remote.clone();
        tokio::spawn(async move {
            resume.restore().await;
        });
    }
    let owner = Arc::new(OwnerContext::new(backend.store()));
    let sink = events.clone();
    let account_remote = remote.clone();
    let pump = tokio::spawn(async move {
        while let Some(event) = source.recv().await {
            if matches!(event, AgentServiceEvent::AccountChanged(_)) {
                account_remote.shutdown().await;
            }
            let _ = sink.send(Arc::new(event));
        }
    });
    let mut clients = tokio::task::JoinSet::new();
    let mut idle = tokio::time::Instant::now() + START_TIMEOUT;
    let mut recovery_until = tokio::time::Instant::now();
    let mut shutdown = Box::pin(shutdown_signal());
    let mut endpoint_check = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            Some(request) = stop_rx.recv() => {
                let remote_update = request.operation.is_some();
                let started = request.started.clone();
                match owner.shutdown_with_update(&backend, &remote, &events, Some(request)).await {
                    Ok(()) => break,
                    Err(error) => {
                        if remote_update && !started.load(Ordering::Acquire) {
                            log::debug!("Remote update did not start host shutdown: {error}");
                        } else {
                            log::error!("Maple could not finish shutdown; owner retained: {error}");
                        }
                        idle=tokio::time::Instant::now()+IDLE_GRACE;
                    }
                }
            },
            _ = &mut shutdown => {
                match owner.shutdown(&backend, &remote, &events).await {
                    Ok(()) => break,
                    Err(error) => { log::error!("Maple could not finish shutdown; owner retained: {error}"); shutdown=Box::pin(shutdown_signal()); }
                }
            },
            _ = endpoint_check.tick() => {
                if !listener.endpoint_present() {
                    endpoint.repair_directory()?;
                    let replacement = endpoint.bind()?;
                    listener = replacement;
                }
            },
            pending = startup_clients.join_next(), if !startup_clients.is_empty() => {
                if let Some(Ok(Ok(client))) = pending {
                    if client.mode == ConnectionMode::Stop { let _ = stop_tx.send(StopRequest::local()).await; }
                    let backend = backend.clone(); let endpoint = endpoint.clone(); let events = events.clone(); let stop = stop_tx.clone(); let receipts = receipts.clone(); let remote = remote.clone(); let owner=owner.clone();
                    clients.spawn(async move {
                        serve_client_inner(client.connection, endpoint, backend, events, stop, receipts, remote, owner, Some((client.mode, client.generation))).await
                    });
                }
            },
            _ = tokio::time::sleep_until(idle), if clients.is_empty() => {
                if remote.stays_running() || owner.keeps_host(&backend) { idle = tokio::time::Instant::now() + Duration::from_secs(1); }
                else {
                    match owner.shutdown(&backend, &remote, &events).await {
                        Ok(()) => break,
                        Err(error) => { log::error!("Maple retained its owner after a failed idle flush: {error}"); idle=tokio::time::Instant::now()+IDLE_GRACE; }
                    }
                }
            },
            result = clients.join_next(), if !clients.is_empty() => {
                if matches!(result, Some(Ok(Ok(false))) | Some(Err(_))) {
                    recovery_until = tokio::time::Instant::now() + DISCONNECT_GRACE;
                }
                if clients.is_empty() {
                    idle = (tokio::time::Instant::now() + IDLE_GRACE).max(recovery_until);
                }
            }
            incoming = listener.accept() => {
                let connection = match incoming { Ok(connection) => connection, Err(error) => { log::warn!("Host rejected a connection: {error}"); continue; } };
                if clients.len() >= MAX_CLIENTS { drop(connection); continue; }
                let backend = backend.clone(); let endpoint = endpoint.clone(); let events = events.clone(); let stop = stop_tx.clone(); let receipts = receipts.clone();
                let remote = remote.clone(); let owner=owner.clone();
                clients.spawn(async move {
                    serve_client(connection, endpoint, backend, events, stop, receipts, remote, owner).await
                });
            }
        }
    }
    // The successful shutdown above has already durably flushed, closed every
    // native connection and wiped the DEK while retaining the owner lock.
    drop(listener);
    clients.abort_all();
    while clients.join_next().await.is_some() {}
    pump.abort();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn serve_client(
    connection: ipc::Connection,
    endpoint: ipc::Endpoint,
    backend: Arc<LocalBackend>,
    events: broadcast::Sender<Arc<AgentServiceEvent>>,
    stop: mpsc::Sender<StopRequest>,
    receipts: Arc<receipts::Receipts>,
    remote: Arc<crate::remote::manager::Manager>,
    owner: Arc<OwnerContext>,
) -> Result<bool, String> {
    serve_client_inner(
        connection, endpoint, backend, events, stop, receipts, remote, owner, None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn serve_client_inner(
    mut connection: ipc::Connection,
    endpoint: ipc::Endpoint,
    backend: Arc<LocalBackend>,
    events: broadcast::Sender<Arc<AgentServiceEvent>>,
    stop: mpsc::Sender<StopRequest>,
    receipts: Arc<receipts::Receipts>,
    remote: Arc<crate::remote::manager::Manager>,
    owner: Arc<OwnerContext>,
    accepted: Option<(ConnectionMode, ConnectionGeneration)>,
) -> Result<bool, String> {
    let mut subscription = events.subscribe();
    let mut drain = owner.draining.subscribe();
    let handshake_admission = if owner.maintenance.load(Ordering::Acquire) {
        None
    } else {
        tokio::select! {
            guard = owner.admission.read() => Some(guard),
            _ = drain.changed() => None,
        }
    };
    let generation = match &accepted {
        Some((_, generation)) => generation.clone(),
        None if handshake_admission.is_some() => ConnectionGeneration::capture(&backend)?,
        None => ConnectionGeneration(None, None),
    };
    let state = if owner.needs_rebind(&backend, &generation) {
        StoreState::Maintenance
    } else {
        owner.state(&backend)
    };
    let (mode, link) = if let Some((mode, _)) = accepted {
        if mode == ConnectionMode::Client {
            write_message(
                &mut connection,
                &Message::Event(Box::new(AgentServiceEvent::AccountChanged(
                    (state == StoreState::Open)
                        .then(|| backend.saved_user_id())
                        .flatten(),
                ))),
            )
            .await?;
            write_message(
                &mut connection,
                &Message::Event(Box::new(AgentServiceEvent::StoreStateChanged {
                    state: state.clone(),
                })),
            )
            .await?;
        }
        (mode, None)
    } else {
        let user = (state == StoreState::Open)
            .then(|| backend.saved_user_id())
            .flatten();
        accept_scoped_handshake(
            &mut connection,
            &endpoint.profile,
            user,
            state,
            Some(&remote),
            Some(&stop),
        )
        .await?
    };
    drop(handshake_admission);
    if let Some(link) = link {
        return crate::remote::control::bridge(connection, link).await;
    }
    match mode {
        ConnectionMode::RemoteUpdateStop { operation } => {
            let (accepted, result) = tokio::sync::oneshot::channel();
            let (acknowledged, confirmation) = tokio::sync::oneshot::channel();
            stop.send(StopRequest {
                operation: Some(operation),
                accepted: Some(accepted),
                acknowledged: Some(confirmation),
                started: Arc::new(AtomicBool::new(false)),
            })
            .await
            .map_err(|_| "Maple host is already stopping")?;
            let result = result
                .await
                .map_err(|_| "Remote update authorization stopped".to_owned())?;
            protocol::write(&mut connection, &result).await?;
            result?;
            let _ = acknowledged.send(());
            // Shutdown closes this connection after the full host drain.
            use tokio::io::AsyncReadExt;
            let mut byte = [0_u8];
            while connection
                .read(&mut byte)
                .await
                .map_err(|_| "Remote update stop connection closed")?
                != 0
            {}
            return Ok(true);
        }
        ConnectionMode::Stop => {
            // Keep the accepted control connection until completed stop; also
            // release its capacity if a failed-stop caller leaves.
            use tokio::io::AsyncReadExt;
            let mut byte = [0_u8];
            while connection
                .read(&mut byte)
                .await
                .map_err(|_| "Stop connection closed")?
                != 0
            {}
            return Ok(true);
        }
        ConnectionMode::Acp => {
            #[cfg(feature = "acp")]
            {
                let mut drain = owner.draining.subscribe();
                let admitted = owner.admission.read().await;
                generation.check(&backend, "acp")?;
                gate(&owner.state(&backend), "acp")?;
                owner.acp_clients.fetch_add(1, Ordering::AcqRel);
                let _client = AcpClient(&owner.acp_clients);
                let (read, write) = tokio::io::split(connection);
                drop(admitted);
                return tokio::select! {
                    result=backend.serve_acp(read, write)=>result.map(|_|true),
                    _=drain.changed()=>Ok(true),
                };
            }
            #[cfg(not(feature = "acp"))]
            return Err("This host was built without ACP".into());
        }
        ConnectionMode::Client => {}
    }
    let _questions = backend.question_responder();
    let (mut input, mut output) = tokio::io::split(connection);
    let (requests, mut request_rx) = mpsc::channel(1);
    // Framed reads cannot be cancelled between the header and body.
    let reader = tokio::spawn(async move {
        while let Ok(request) = protocol::read::<Request>(&mut input).await {
            if requests
                .send(protocol::SensitiveRequest(request))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let _reader = AbortOnDrop(reader.abort_handle());
    let (responses, mut response_rx) =
        mpsc::channel::<(protocol::SensitiveMessage, bool)>(MAX_CALLS);
    let mut task_subscription = subscription::Subscription::default();
    let mut calls = tokio::task::JoinSet::new();
    let result: Result<bool, String> = async {
    loop {
        tokio::select! {
            request = request_rx.recv(), if calls.len() < MAX_CALLS => {
                let Some(request) = request else { return Ok(false); };
                if request.method == "detach" {
                    write_message(&mut output, &Message::Response { id: request.id, result: Ok(serde_json::Value::Null) }).await?;
                    return Ok(true);
                }
                if request.method == "subscribe" {
                    let _admission = owner.admission.read().await;
                    let result = (|| {
                        generation.check(&backend, "subscribe")?;
                        gate(&owner.state(&backend), "subscribe")?;
                        let (user, filter): (String, subscription::SubscribeRequest) = serde::Deserialize::deserialize(&request.params)
                            .map_err(|_| "Invalid task subscription".to_owned())?;
                        backend.authorize_account(&user)?;
                        task_subscription.set(filter)?;
                        Ok(serde_json::Value::Null)
                    })();
                    write_message(&mut output, &Message::Response { id:request.id,result }).await?;
                    if !generation.is_current(&backend) { return Ok(false); }
                    continue;
                }
                let backend = backend.clone(); let responses = responses.clone(); let events = events.clone(); let receipts = receipts.clone();
                let remote = remote.clone(); let owner=owner.clone(); let generation=generation.clone();
                calls.spawn(async move {
                    let id = request.id;
                    let before = backend.saved_user_id();
                    let result = owner.dispatch(&backend, &remote, &events, request, receipts, &generation).await;
                    let after = backend.saved_user_id();
                    if before != after {
                        remote.shutdown().await;
                        if after.is_some() && owner.state(&backend) == StoreState::Open {
                            let resume = remote.clone();
                            tokio::spawn(async move { resume.restore().await; });
                        }
                        if before != after { let _ = events.send(Arc::new(AgentServiceEvent::AccountChanged(after))); }
                    }
                    let rebind = owner.needs_rebind(&backend, &generation);
                    let _ = responses.send((protocol::SensitiveMessage(Message::Response { id, result }), rebind)).await;
                });
            }
            _ = calls.join_next(), if !calls.is_empty() => {},
            response = response_rx.recv() => {
                if let Some((response, rebind)) = response {
                    write_message(&mut output, &response.0).await?;
                    if rebind { return Ok(false); }
                }
            }
            event = subscription.recv() => {
                // A lagging frontend reloads persisted state; it cannot stall
                // the host or silently miss an arbitrary portion of the stream.
                match event {
                    Ok(event) => {
                        let stale = owner.needs_rebind(&backend, &generation);
                        if stale && matches!(&*event, AgentServiceEvent::StoreStateChanged { .. }) {
                            write_message(&mut output, &Message::Event(Box::new(AgentServiceEvent::StoreStateChanged { state:StoreState::Maintenance }))).await?;
                            continue;
                        }
                        let readable = (!stale && owner.state(&backend) == StoreState::Open)
                            || matches!(&*event, AgentServiceEvent::StoreStateChanged { .. } | AgentServiceEvent::AccountChanged(None) | AgentServiceEvent::PersistenceFailureChanged { .. });
                        if readable && task_subscription.allows(&event) {
                            write_event(&mut output, &event).await?;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => write_message(&mut output, &Message::Resync).await?,
                    Err(broadcast::error::RecvError::Closed) => return Ok(false),
                }
            }
        }
    }
    }.await;
    // Accepted mutations finish even if their response cannot be delivered.
    drop(response_rx);
    while calls.join_next().await.is_some() {}
    if let Err(error) = &result {
        log::debug!("Host connection ended: {error}");
    }
    Ok(result.unwrap_or(false))
}

#[cfg(test)]
async fn accept_handshake(
    connection: &mut ipc::Connection,
    profile: &str,
    user_id: Option<String>,
) -> Result<ConnectionMode, String> {
    accept_scoped_handshake(connection, profile, user_id, StoreState::Open, None, None)
        .await
        .map(|(mode, _)| mode)
}

async fn accept_scoped_handshake(
    connection: &mut ipc::Connection,
    profile: &str,
    user_id: Option<String>,
    store: StoreState,
    remote: Option<&Arc<crate::remote::manager::Manager>>,
    stop: Option<&mpsc::Sender<StopRequest>>,
) -> Result<(ConnectionMode, Option<crate::remote::control::Link>), String> {
    let hello = tokio::time::timeout(START_TIMEOUT, protocol::read::<Hello>(connection))
        .await
        .map_err(|_| "Host handshake timed out".to_string())??;
    accept_hello(connection, hello, profile, user_id, store, remote, stop).await
}

#[allow(clippy::too_many_arguments)]
async fn accept_hello(
    connection: &mut ipc::Connection,
    hello: Hello,
    profile: &str,
    user_id: Option<String>,
    store: StoreState,
    remote: Option<&Arc<crate::remote::manager::Manager>>,
    stop: Option<&mpsc::Sender<StopRequest>>,
) -> Result<(ConnectionMode, Option<crate::remote::control::Link>), String> {
    // Stop is an explicit, minimal control operation on the OS-private storage
    // endpoint. It must remain usable when the new build cannot join the old
    // runtime. It grants no access to account data or ordinary host commands.
    let error = if hello.mode != ConnectionMode::Stop
        && (hello.version != protocol::VERSION
            || hello.profile != profile
            || hello.build != protocol::BUILD)
    {
        Some(
            "Incompatible Maple host or profile. Close the existing host or run `maple-agent host stop` for this profile, then reopen Maple.",
        )
    } else if matches!(hello.mode, ConnectionMode::RemoteUpdateStop { .. })
        && (store != StoreState::Open || remote.is_none())
    {
        Some(
            "Remote updating is unavailable while this host is starting or local data is not ready",
        )
    } else if hello.remote_host.is_some() && store != StoreState::Open {
        Some(match store {
            StoreState::Locked => LOCKED_MESSAGE,
            StoreState::NeedsSetup => SETUP_MESSAGE,
            _ => "Maple local data is not ready. Open Maple and try again.",
        })
    } else if hello.remote_host.is_some()
        && (hello.mode != ConnectionMode::Client || remote.is_none())
    {
        Some("Remote selection is available only to a desktop client")
    } else if hello.mode == ConnectionMode::Acp && store != StoreState::Open {
        Some(match store {
            StoreState::Locked => LOCKED_MESSAGE,
            StoreState::NeedsSetup => SETUP_MESSAGE,
            _ => "Maple local data is not ready. Open Maple and try again.",
        })
    } else if hello.mode == ConnectionMode::Acp && user_id.is_none() {
        Some(
            "Maple ACP requires sign-in. Sign in through Maple Desktop or `maple-agent login` first.",
        )
    } else {
        None
    };
    if let Some(error) = error {
        protocol::write(connection, &Err::<Welcome, _>(error)).await?;
        return Err(error.into());
    }
    let link = match hello.remote_host {
        Some(peer) => match remote
            .expect("validated remote manager")
            .connect(&peer)
            .await
        {
            Ok(link) => Some(link),
            Err(error) => {
                protocol::write(connection, &Err::<Welcome, _>(&error)).await?;
                return Err(error);
            }
        },
        None => None,
    };
    // Always commit an authorized Stop, including when the caller leaves before
    // its acknowledgement. Attempt the small reply first so shutdown cannot
    // abort this task before an attached caller receives it.
    let welcome = protocol::write(
        connection,
        &Ok::<_, String>(Welcome {
            version: protocol::VERSION,
            pid: std::process::id(),
            build: protocol::BUILD.into(),
            user_id,
            store,
        }),
    )
    .await;
    if hello.mode == ConnectionMode::Stop
        && let Some(stop) = stop
    {
        stop.send(StopRequest::local())
            .await
            .map_err(|_| "Maple host is already stopping")?;
    }
    welcome?;
    Ok((hello.mode, link))
}

#[cfg(feature = "desktop")]
pub async fn request_stop(api_url: &str) -> Result<(), String> {
    let endpoint = ipc::Endpoint::new(api_url)?;
    request_stop_endpoint(&endpoint).await
}

/// Read only the existing control authority. This never connects, sends Stop
/// or starts a replacement host; uncertainty must preserve the frontend.
#[cfg(feature = "desktop")]
pub(crate) async fn host_has_stopped(api_url: &str) -> Result<bool, String> {
    let endpoint = ipc::Endpoint::new(api_url)?;
    owner_is_free(&endpoint).await
}

#[cfg(any(feature = "desktop", test))]
async fn owner_is_free(endpoint: &ipc::Endpoint) -> Result<bool, String> {
    let endpoint = endpoint.clone();
    tokio::task::spawn_blocking(move || {
        let ownership = endpoint.existing_ownership_lock()?;
        ownership_lock_is_free(&ownership)
    })
    .await
    .map_err(|_| "Could not inspect Maple host ownership".to_owned())?
}

fn ownership_lock_is_free(ownership: &std::fs::File) -> Result<bool, String> {
    match ownership.try_lock() {
        Ok(()) => Ok(true),
        Err(std::fs::TryLockError::WouldBlock) => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

async fn request_stop_endpoint(endpoint: &ipc::Endpoint) -> Result<(), String> {
    match connect(endpoint, ConnectionMode::Stop, false).await {
        Ok((mut connection, _)) => {
            // Welcome acknowledges acceptance, not lock completion. Keep the
            // control connection until the host drain closes it, then prove
            // the authoritative lock has been released.
            tokio::time::timeout(Duration::from_secs(45), async {
                use tokio::io::AsyncReadExt;
                let mut byte = [0];
                while connection
                    .read(&mut byte)
                    .await
                    .map_err(|e| e.to_string())?
                    != 0
                {}
                loop {
                    let ownership = endpoint.ownership_lock()?;
                    if ownership_lock_is_free(&ownership)? {
                        return Ok::<_, String>(());
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            })
            .await
            .map_err(|_| {
                "Maple host is still stopping and remains unlocked. Keep Maple open and try again."
                    .to_string()
            })?
        }
        Err(error) => {
            // A missing/inaccessible endpoint alone does not prove the host is
            // gone. The same ownership lock protects startup and the full drain.
            // Preserve idempotent CLI Stop on a never-started profile. The
            // separate frontend-after-save-failure proof above deliberately
            // requires an existing validated inode and cannot use this path.
            let endpoint = endpoint.clone();
            let absent = tokio::task::spawn_blocking(move || {
                let ownership = endpoint.ownership_lock()?;
                ownership_lock_is_free(&ownership)
            })
            .await
            .map_err(|_| "Could not inspect Maple host ownership".to_owned())??;
            if absent { Ok(()) } else { Err(error) }
        }
    }
}

/// The remote path requires explicit same-build host admission. Connection or
/// authorization failure never falls back to unconditional administrative Stop.
#[cfg(feature = "desktop")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteUpdateStopError {
    pub message: String,
    /// True only when the request was not sent, or the same-build host
    /// explicitly refused it before shutdown. Transport failures are uncertain.
    pub host_unchanged: bool,
}

#[cfg(feature = "desktop")]
impl RemoteUpdateStopError {
    fn unchanged(message: String) -> Self {
        Self {
            message,
            host_unchanged: true,
        }
    }

    fn uncertain(message: String) -> Self {
        Self {
            message,
            host_unchanged: false,
        }
    }
}

#[cfg(feature = "desktop")]
async fn request_remote_stop_endpoint(
    endpoint: &ipc::Endpoint,
    operation: [u8; 16],
) -> Result<(), RemoteUpdateStopError> {
    let (mut connection, _) = connect(
        endpoint,
        ConnectionMode::RemoteUpdateStop { operation },
        false,
    )
    .await
    .map_err(RemoteUpdateStopError::uncertain)?;
    finish_remote_stop_connection(
        &mut connection,
        endpoint,
        Duration::from_secs(45),
        Duration::from_secs(45),
    )
    .await
}

/// Kept separate from connection establishment so each protocol stage can be
/// tested with real framed IPC bytes. An explicit refusal is the only reply
/// proving the still-running host is unchanged; a missing/malformed reply does
/// not prove whether the request took effect.
#[cfg(feature = "desktop")]
async fn finish_remote_stop_connection(
    connection: &mut (impl tokio::io::AsyncRead + Unpin),
    endpoint: &ipc::Endpoint,
    authorization_timeout: Duration,
    drain_timeout: Duration,
) -> Result<(), RemoteUpdateStopError> {
    let accepted: Result<(), String> =
        tokio::time::timeout(authorization_timeout, protocol::read(&mut *connection))
            .await
            .map_err(|_| {
                RemoteUpdateStopError::uncertain("Remote update authorization timed out".to_owned())
            })?
            .map_err(RemoteUpdateStopError::uncertain)?;
    accepted.map_err(RemoteUpdateStopError::unchanged)?;
    tokio::time::timeout(drain_timeout, async {
        use tokio::io::AsyncReadExt;
        let mut byte = [0_u8];
        while connection
            .read(&mut byte)
            .await
            .map_err(|e| e.to_string())?
            != 0
        {}
        loop {
            let ownership = endpoint.ownership_lock()?;
            if ownership_lock_is_free(&ownership)? {
                return Ok::<_, String>(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .map_err(|_| {
        RemoteUpdateStopError::uncertain(
            "Maple did not finish stopping. Keep it open and check this computer.".to_owned(),
        )
    })?
    .map_err(RemoteUpdateStopError::uncertain)
}

pub fn stop(api_url: &str) -> Result<(), String> {
    let runtime = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    runtime.block_on(async {
        let endpoint = ipc::Endpoint::new(api_url)?;
        request_stop_endpoint(&endpoint).await
    })
}

/// Holds this profile's startup admission and host ownership through replacement
/// and the old desktop's entire restart handoff, including helper-spawn failure.
/// The new process acquires these normally after the old process exits.
#[cfg(feature = "desktop")]
pub struct HostUpdateFence {
    // Release ownership before admission if a pre-commit attempt fails.
    _ownership: std::fs::File,
    _startup: std::fs::File,
}

/// Stop the current host and reserve this profile against queued/direct starts.
///
/// Call from a blocking task, never the UI or an async executor thread. The caller
/// also owns the application-path installation lock and revalidates staging in
/// `replace`. Retain the returned guards after commit until process exit; drop
/// them before recovering from a pre-commit failure. Normal host startup owns
/// `startup.lock` while waiting for the child's handshake; the child must not try
/// to acquire that same lock a second time.
#[cfg(feature = "desktop")]
pub fn acquire_update_fence(api_url: &str) -> Result<HostUpdateFence, String> {
    let endpoint = ipc::Endpoint::new(api_url)?;
    acquire_update_fence_endpoint(&endpoint, Duration::from_secs(45), || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| "Could not prepare update shutdown".to_string())?;
        runtime.block_on(request_stop_endpoint(&endpoint))
    })
}

#[cfg(feature = "desktop")]
pub(crate) fn acquire_remote_update_fence(
    api_url: &str,
    operation: [u8; 16],
) -> Result<HostUpdateFence, RemoteUpdateStopError> {
    let endpoint = ipc::Endpoint::new(api_url).map_err(RemoteUpdateStopError::unchanged)?;
    acquire_remote_update_fence_endpoint(&endpoint, Duration::from_secs(45), || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| {
                RemoteUpdateStopError::unchanged(
                    "Could not prepare remote update shutdown".to_owned(),
                )
            })?;
        runtime.block_on(request_remote_stop_endpoint(&endpoint, operation))
    })
}

#[cfg(feature = "desktop")]
fn acquire_remote_update_fence_endpoint(
    endpoint: &ipc::Endpoint,
    timeout: Duration,
    stop: impl FnOnce() -> Result<(), RemoteUpdateStopError>,
) -> Result<HostUpdateFence, RemoteUpdateStopError> {
    acquire_update_fence_staged(
        endpoint,
        timeout,
        stop,
        RemoteUpdateStopError::unchanged,
        RemoteUpdateStopError::uncertain,
    )
}

#[cfg(feature = "desktop")]
fn acquire_update_fence_endpoint(
    endpoint: &ipc::Endpoint,
    timeout: Duration,
    stop: impl FnOnce() -> Result<(), String>,
) -> Result<HostUpdateFence, String> {
    acquire_update_fence_staged(
        endpoint,
        timeout,
        stop,
        std::convert::identity,
        std::convert::identity,
    )
}

#[cfg(feature = "desktop")]
fn acquire_update_fence_staged<E>(
    endpoint: &ipc::Endpoint,
    timeout: Duration,
    stop: impl FnOnce() -> Result<(), E>,
    before_stop: impl Fn(String) -> E,
    after_stop: impl Fn(String) -> E,
) -> Result<HostUpdateFence, E> {
    let startup = endpoint.lock("startup.lock").map_err(&before_stop)?;
    wait_update_lock(&startup, timeout, "Another Maple host is starting").map_err(before_stop)?;
    stop()?;
    // Stop acknowledgement and even socket EOF can precede the host's final
    // drop of ownership. Acquiring ownership proves the drain has completed
    // and prevents direct `host serve` attempts during the file replacement.
    let ownership = endpoint.ownership_lock().map_err(&after_stop)?;
    wait_update_lock(&ownership, timeout, "Maple host is still shutting down")
        .map_err(after_stop)?;
    Ok(HostUpdateFence {
        _ownership: ownership,
        _startup: startup,
    })
}

#[cfg(all(test, feature = "desktop"))]
fn with_update_fence_endpoint<T>(
    endpoint: &ipc::Endpoint,
    timeout: Duration,
    stop: impl FnOnce() -> Result<(), String>,
    replace: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let _fence = acquire_update_fence_endpoint(endpoint, timeout, stop)?;
    replace()
}

#[cfg(feature = "desktop")]
fn wait_update_lock(file: &std::fs::File, timeout: Duration, busy: &str) -> Result<(), String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(std::fs::TryLockError::WouldBlock) => return Err(busy.to_string()),
            Err(_) => return Err("Could not acquire Maple update ownership".into()),
        }
    }
}

#[cfg(all(test, feature = "desktop"))]
mod update_install_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[tokio::test]
    async fn remote_stop_explicit_refusal_proves_the_host_was_not_stopped() {
        let root = tempfile::tempdir().unwrap();
        let endpoint = ipc::Endpoint::for_test(root.path().to_path_buf());
        let owner = endpoint.ownership_lock().unwrap();
        owner.lock().unwrap();
        let (mut client, mut server) = tokio::io::duplex(4096);
        let (finished, waiting) = oneshot::channel();
        let refusal = "This computer is busy with a local operation. Try again after it finishes.";
        let response = async {
            protocol::write(&mut server, &Err::<(), String>(refusal.into()))
                .await
                .unwrap();
            // The client must return the explicit refusal without requiring
            // EOF from a host that correctly remains alive.
            let _ = waiting.await;
        };
        let request = async {
            let error = finish_remote_stop_connection(
                &mut client,
                &endpoint,
                Duration::from_secs(5),
                Duration::from_millis(50),
            )
            .await
            .unwrap_err();
            assert_eq!(error.message, refusal);
            assert!(error.host_unchanged);
            assert!(matches!(
                endpoint.ownership_lock().unwrap().try_lock(),
                Err(std::fs::TryLockError::WouldBlock)
            ));
            let _ = finished.send(());
        };
        tokio::join!(response, request);
    }

    #[tokio::test]
    async fn remote_stop_missing_invalid_or_timed_out_authorization_is_uncertain() {
        let root = tempfile::tempdir().unwrap();
        let endpoint = ipc::Endpoint::for_test(root.path().to_path_buf());
        let (mut client, server) = tokio::io::duplex(4096);
        drop(server);
        let error = finish_remote_stop_connection(
            &mut client,
            &endpoint,
            Duration::from_secs(5),
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();
        assert!(
            !error.host_unchanged,
            "EOF before a reply is not an explicit refusal"
        );

        let (mut client, mut server) = tokio::io::duplex(4096);
        protocol::write(&mut server, &serde_json::json!({"unexpected": true}))
            .await
            .unwrap();
        let error = finish_remote_stop_connection(
            &mut client,
            &endpoint,
            Duration::from_secs(5),
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();
        assert!(
            !error.host_unchanged,
            "malformed replies cannot authorize safe retry"
        );

        let (mut client, _server) = tokio::io::duplex(4096);
        let error = finish_remote_stop_connection(
            &mut client,
            &endpoint,
            Duration::from_millis(20),
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();
        assert!(
            !error.host_unchanged,
            "an unanswered request may have taken effect"
        );
    }

    #[tokio::test]
    async fn accepted_remote_stop_without_completed_drain_stays_uncertain() {
        let root = tempfile::tempdir().unwrap();
        let endpoint = ipc::Endpoint::for_test(root.path().to_path_buf());
        let owner = endpoint.ownership_lock().unwrap();
        owner.lock().unwrap();
        for close_stream in [false, true] {
            let (mut client, mut server) = tokio::io::duplex(4096);
            protocol::write(&mut server, &Ok::<(), String>(()))
                .await
                .unwrap();
            let _server = if close_stream {
                drop(server);
                None
            } else {
                Some(server)
            };
            let error = finish_remote_stop_connection(
                &mut client,
                &endpoint,
                Duration::from_secs(5),
                Duration::from_millis(50),
            )
            .await
            .unwrap_err();
            assert!(!error.host_unchanged);
            assert!(matches!(
                endpoint.ownership_lock().unwrap().try_lock(),
                Err(std::fs::TryLockError::WouldBlock)
            ));
        }
    }

    #[test]
    fn remote_fence_preserves_stop_stage_and_releases_startup_without_fallback() {
        let root = tempfile::tempdir().unwrap();
        let endpoint = ipc::Endpoint::for_test(root.path().to_path_buf());
        for host_unchanged in [true, false] {
            let calls = std::sync::atomic::AtomicUsize::new(0);
            let result =
                acquire_remote_update_fence_endpoint(&endpoint, Duration::from_millis(30), || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Err(RemoteUpdateStopError {
                        message: "fixture stop result".into(),
                        host_unchanged,
                    })
                });
            let error = match result {
                Ok(_) => panic!("failed stop unexpectedly acquired a fence"),
                Err(error) => error,
            };
            assert_eq!(error.host_unchanged, host_unchanged);
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "remote stop must never retry through ordinary Stop"
            );
            endpoint.lock("startup.lock").unwrap().try_lock().unwrap();
        }
        let owner = endpoint.ownership_lock().unwrap();
        owner.lock().unwrap();
        let result =
            acquire_remote_update_fence_endpoint(&endpoint, Duration::from_millis(30), || Ok(()));
        let error = match result {
            Ok(_) => panic!("retained owner unexpectedly acquired a fence"),
            Err(error) => error,
        };
        assert!(
            !error.host_unchanged,
            "ownership failure after accepted stop is uncertain"
        );
        endpoint.lock("startup.lock").unwrap().try_lock().unwrap();
    }

    #[test]
    fn remote_fence_startup_failure_never_sends_a_stop() {
        let root = tempfile::tempdir().unwrap();
        let endpoint = ipc::Endpoint::for_test(root.path().to_path_buf());
        let starting = endpoint.lock("startup.lock").unwrap();
        starting.lock().unwrap();
        let sent = AtomicBool::new(false);
        let result =
            acquire_remote_update_fence_endpoint(&endpoint, Duration::from_millis(20), || {
                sent.store(true, Ordering::SeqCst);
                Ok(())
            });
        let error = match result {
            Ok(_) => panic!("busy startup unexpectedly acquired a fence"),
            Err(error) => error,
        };
        assert!(error.host_unchanged);
        assert!(!sent.load(Ordering::SeqCst));
        drop(starting);
        endpoint.lock("startup.lock").unwrap().try_lock().unwrap();
    }

    #[tokio::test]
    async fn durable_store_shutdown_failure_prevents_update_and_preserves_owner() {
        let root = tempfile::tempdir().unwrap();
        let backend = Arc::new(LocalBackend::isolated_for_test(root.path()));
        let store = backend.store().unwrap().clone();
        store
            .set_app_setting("update-sentinel", &serde_json::json!(true), 1)
            .unwrap();
        let endpoint = ipc::Endpoint::for_test(store.paths().root().to_path_buf());
        let ownership = endpoint.ownership_lock().unwrap();
        ownership.lock().unwrap();
        let ownership = Arc::new(std::sync::Mutex::new(Some(ownership)));
        let owner = Arc::new(OwnerContext::new(Some(&store)));
        let receipts = Arc::new(receipts::Receipts::new(store.clone()));
        let (events, _) = broadcast::channel(16);
        let remote = Arc::new(crate::remote::manager::Manager::new(
            backend.clone(),
            events.clone(),
            receipts,
        ));
        // Exercise a real late durable-close failure through the existing SQL
        // fixture boundary; no raw SQLite handle or production test hook.
        let spills = maple_agent::store::spill::SpillRoot::new(store.paths()).unwrap();
        spills
            .task_dir("00000000-0000-0000-0000-000000000001")
            .unwrap();
        std::fs::remove_dir_all(spills.path()).unwrap();
        std::fs::write(spills.path(), b"invalid spill fixture").unwrap();
        let replaced = Arc::new(AtomicBool::new(false));
        let attempt = || {
            let (endpoint, backend, owner, remote, events, ownership, replaced) = (
                endpoint.clone(),
                backend.clone(),
                owner.clone(),
                remote.clone(),
                events.clone(),
                ownership.clone(),
                replaced.clone(),
            );
            let runtime = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || {
                with_update_fence_endpoint(
                    &endpoint,
                    Duration::from_secs(2),
                    || {
                        runtime.block_on(owner.shutdown(&backend, &remote, &events))?;
                        drop(ownership.lock().unwrap().take());
                        Ok(())
                    },
                    || {
                        assert!(matches!(
                            endpoint.ownership_lock().unwrap().try_lock(),
                            Err(std::fs::TryLockError::WouldBlock)
                        ));
                        // The previous inode is no longer the authoritative fence.
                        endpoint.lock("host.lock").unwrap().try_lock().unwrap();
                        replaced.store(true, Ordering::SeqCst);
                        Ok(())
                    },
                )
            })
        };
        assert!(attempt().await.unwrap().is_err());
        assert!(!replaced.load(Ordering::SeqCst));
        assert!(matches!(
            endpoint.ownership_lock().unwrap().try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        assert_eq!(store.state(), StoreState::Open);
        assert_eq!(
            store.app_setting("update-sentinel").unwrap(),
            Some(serde_json::json!(true))
        );
        endpoint.lock("startup.lock").unwrap().try_lock().unwrap();

        std::fs::remove_file(spills.path()).unwrap();
        attempt().await.unwrap().unwrap();
        assert!(replaced.load(Ordering::SeqCst));
        store.initialize_plain().unwrap();
        assert_eq!(
            store.app_setting("update-sentinel").unwrap(),
            Some(serde_json::json!(true))
        );
        store.close_for_shutdown().unwrap();
    }

    #[test]
    fn committed_restart_fence_survives_the_last_controller_handle() {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = ipc::Endpoint::for_test(directory.path().to_path_buf());
        let fence =
            acquire_update_fence_endpoint(&endpoint, Duration::from_secs(2), || Ok(())).unwrap();
        let controller = crate::update::Controller::new(true);
        controller.set_test_installed_with_fence(directory.path().join("Maple Agent.app"), fence);
        let backend = Arc::new(
            crate::backend::AgentBackend::for_test("http://127.0.0.1:9".into(), String::new())
                .unwrap(),
        );
        backend.retire_after_update();
        assert!(tokio::runtime::Handle::try_current().is_err());
        gpui::block_on(controller.cleanup_on_exit(backend)).unwrap();
        let last_handle = controller.clone();
        drop(controller);
        drop(last_handle);
        // The committed handoff intentionally leaves these two descriptors
        // to OS process-exit cleanup, even when GPUI tears down every handle.
        for lock in [
            endpoint.lock("startup.lock").unwrap(),
            endpoint.ownership_lock().unwrap(),
        ] {
            assert!(matches!(
                lock.try_lock(),
                Err(std::fs::TryLockError::WouldBlock)
            ));
        }
    }

    #[test]
    fn retained_restart_fence_blocks_queued_and_direct_starts_until_exit() {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = ipc::Endpoint::for_test(directory.path().to_path_buf());
        let fence =
            acquire_update_fence_endpoint(&endpoint, Duration::from_secs(2), || Ok(())).unwrap();
        let direct = endpoint.ownership_lock().unwrap();
        let queued = endpoint.lock("startup.lock").unwrap();
        let (started, startup_waiting) = std::sync::mpsc::channel();
        let (admitted, admission) = std::sync::mpsc::channel();
        let client = std::thread::spawn(move || {
            started.send(()).unwrap();
            wait_update_lock(
                &queued,
                Duration::from_secs(2),
                "restart fence did not release",
            )
            .unwrap();
            admitted.send(()).unwrap();
        });
        startup_waiting
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert!(admission.recv_timeout(Duration::from_millis(40)).is_err());
        assert!(matches!(
            direct.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        // Simulate the old desktop exiting after a failed relaunch handoff.
        drop(fence);
        admission.recv_timeout(Duration::from_secs(1)).unwrap();
        direct.try_lock().unwrap();
        client.join().unwrap();
    }

    #[test]
    fn update_waits_for_drain_and_holds_startup_and_ownership_during_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = ipc::Endpoint::for_test(directory.path().to_path_buf());
        let old_host = endpoint.ownership_lock().unwrap();
        old_host.lock().unwrap();
        let queued_start = endpoint.lock("startup.lock").unwrap();
        let direct_start = endpoint.ownership_lock().unwrap();
        let result = with_update_fence_endpoint(
            &endpoint,
            Duration::from_secs(2),
            || {
                assert!(matches!(
                    queued_start.try_lock(),
                    Err(std::fs::TryLockError::WouldBlock)
                ));
                drop(old_host);
                Ok(())
            },
            || {
                assert!(matches!(
                    queued_start.try_lock(),
                    Err(std::fs::TryLockError::WouldBlock)
                ));
                assert!(matches!(
                    direct_start.try_lock(),
                    Err(std::fs::TryLockError::WouldBlock)
                ));
                Ok(42)
            },
        )
        .unwrap();
        assert_eq!(result, 42);
        queued_start.try_lock().unwrap();
        direct_start.try_lock().unwrap();
    }

    #[test]
    fn shutdown_failure_never_replaces_and_releases_startup_admission() {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = ipc::Endpoint::for_test(directory.path().to_path_buf());
        let replaced = AtomicBool::new(false);
        let result = with_update_fence_endpoint(
            &endpoint,
            Duration::from_millis(30),
            || Err("stop failed".into()),
            || {
                replaced.store(true, Ordering::SeqCst);
                Ok(())
            },
        );
        assert_eq!(result.unwrap_err(), "stop failed");
        assert!(!replaced.load(Ordering::SeqCst));
        endpoint.lock("startup.lock").unwrap().try_lock().unwrap();
    }

    #[test]
    fn stop_acknowledgement_does_not_bypass_a_host_that_keeps_ownership() {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = ipc::Endpoint::for_test(directory.path().to_path_buf());
        let old_host = endpoint.ownership_lock().unwrap();
        old_host.lock().unwrap();
        let replaced = AtomicBool::new(false);
        let result = with_update_fence_endpoint(
            &endpoint,
            Duration::from_millis(30),
            || Ok(()),
            || {
                replaced.store(true, Ordering::SeqCst);
                Ok(())
            },
        );
        assert!(result.unwrap_err().contains("shutting down"));
        assert!(!replaced.load(Ordering::SeqCst));
        endpoint.lock("startup.lock").unwrap().try_lock().unwrap();
    }

    #[test]
    fn replacement_failure_releases_both_locks_for_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = ipc::Endpoint::for_test(directory.path().to_path_buf());
        let result: Result<(), String> = with_update_fence_endpoint(
            &endpoint,
            Duration::from_millis(30),
            || Ok(()),
            || Err("replacement failed".into()),
        );
        assert_eq!(result.unwrap_err(), "replacement failed");
        endpoint.lock("startup.lock").unwrap().try_lock().unwrap();
        endpoint.ownership_lock().unwrap().try_lock().unwrap();
    }
}

#[cfg(feature = "acp")]
pub fn acp(api_url: &str) -> Result<(), String> {
    let runtime = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    let result = runtime.block_on(async {
        let endpoint = ipc::Endpoint::new(api_url)?;
        let (stream, _) = connect(&endpoint, ConnectionMode::Acp, true).await?;
        let (mut read, mut write) = tokio::io::split(stream);
        let mut input = tokio::io::stdin(); let mut output = tokio::io::stdout();
        tokio::select! {
            result = tokio::io::copy(&mut input, &mut write) => result.map(|_| ()).map_err(|e| e.to_string()),
            result = tokio::io::copy(&mut read, &mut output) => result.map(|_| ()).map_err(|e| e.to_string()),
        }
    });
    // Tokio's stdin uses an uncancellable blocking read. If the host closes
    // first (for example on sign-out), runtime Drop would wait for the editor
    // to write to stdin before this adapter could exit.
    runtime.shutdown_background();
    result
}

struct AbortOnDrop(tokio::task::AbortHandle);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn write_message(
    output: &mut (impl tokio::io::AsyncWrite + Unpin),
    message: &Message,
) -> Result<(), String> {
    tokio::time::timeout(START_TIMEOUT, protocol::write(output, message))
        .await
        .map_err(|_| "Host client stopped receiving updates".to_string())?
}

#[derive(Serialize)]
enum EventRef<'a> {
    Event(&'a AgentServiceEvent),
}

async fn write_event(
    output: &mut (impl tokio::io::AsyncWrite + Unpin),
    event: &AgentServiceEvent,
) -> Result<(), String> {
    match crate::framing::encode(&EventRef::Event(event), 4 * 1024 * 1024) {
        Ok(bytes) => tokio::time::timeout(START_TIMEOUT, protocol::write_encoded(output, &bytes))
            .await
            .map_err(|_| "Host client stopped receiving updates".to_owned())?,
        Err(crate::framing::EncodeError::TooLarge) => write_message(output, &Message::Resync).await,
        Err(crate::framing::EncodeError::Invalid) => Err("Could not encode host event".into()),
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

pub(crate) async fn dispatch_remote_with_receipt(
    backend: &LocalBackend,
    request: Request,
    receipts: Arc<receipts::Receipts>,
    session: &maple_agent::maple_api::MapleApiSession,
    account: &str,
) -> Result<serde_json::Value, String> {
    let _guard = backend.account_gate.read().await;
    if !session.remote_session_active().await || backend.saved_user_id().as_deref() != Some(account)
    {
        return Err("The remote account changed".into());
    }
    if request.method == "send_outcome" {
        let id = request.params.as_str().ok_or("Invalid send identity")?;
        receipts.authorize(id, account)?;
    } else if request
        .params
        .pointer("/0")
        .and_then(serde_json::Value::as_str)
        != Some(account)
    {
        return Err("Remote request account mismatch".into());
    }
    dispatch_receipt(backend, request, receipts, true).await
}

async fn dispatch_with_receipt(
    backend: &LocalBackend,
    request: Request,
    receipts: Arc<receipts::Receipts>,
) -> Result<serde_json::Value, String> {
    dispatch_receipt(backend, request, receipts, false).await
}

async fn dispatch_receipt(
    backend: &LocalBackend,
    request: Request,
    receipts: Arc<receipts::Receipts>,
    remote_guarded: bool,
) -> Result<serde_json::Value, String> {
    if request.method == "send_outcome" {
        let account = backend
            .saved_user_id()
            .ok_or("Sign in to inspect a send outcome")?;
        let id: String =
            serde_json::from_value(request.params).map_err(|_| "Invalid send identity")?;
        return tokio::task::spawn_blocking(move || {
            receipts
                .outcome(&account, &id)
                .and_then(|result| serde_json::to_value(result).map_err(|e| e.to_string()))
        })
        .await
        .map_err(|e| e.to_string())?;
    }
    if request.method != "send_message" {
        return if remote_guarded {
            backend::dispatch_remote_inner(backend, request).await
        } else {
            backend::dispatch(backend, request).await
        };
    }
    let id = request
        .send_id
        .clone()
        .ok_or("A send requires a stable identity")?;
    let record_id = id.clone();
    let account = request
        .params
        .pointer("/0")
        .and_then(serde_json::Value::as_str)
        .ok_or("Invalid send account")?
        .to_owned();
    let params = request.params.clone();
    let store = receipts.clone();
    match tokio::task::spawn_blocking(move || store.reserve(&record_id, &params))
        .await
        .map_err(|e| e.to_string())??
    {
        Some(receipts::Outcome::Finished(result)) => return result,
        Some(_) => {
            return Err(format!(
                "Send outcome unknown ({id}). Reload this task before sending again."
            ));
        }
        None => {}
    }
    let result = if remote_guarded {
        backend::dispatch_remote_inner(backend, request).await
    } else {
        backend::dispatch(backend, request).await
    };
    finish_admitted_send(receipts, account, id, result).await
}

/// Dispatch has already run after a durable reservation. A missing completion
/// cannot prove that its effects did not happen, even when dispatch returned an
/// error. Keep the original identity uncertain and recover by receipt only.
async fn finish_admitted_send(
    receipts: Arc<receipts::Receipts>,
    account: String,
    id: String,
    result: Result<serde_json::Value, String>,
) -> Result<serde_json::Value, String> {
    let saved = result.clone();
    let original_id = id.clone();
    let completion =
        tokio::task::spawn_blocking(move || receipts.finish(&account, &id, &saved)).await;
    if !matches!(completion, Ok(Ok(()))) {
        return Err(format!(
            "Send outcome unknown ({original_id}). Reload this task before sending again; it may already be running."
        ));
    }
    result
}

async fn recover_send(endpoint: &ipc::Endpoint, id: &str) -> Result<serde_json::Value, String> {
    // Query only. Starting another host or replaying inference is never recovery.
    let recovery = async {
        let (mut connection, _) = connect(endpoint, ConnectionMode::Client, false).await?;
        for _ in 0..20 {
            protocol::write(
                &mut connection,
                &Request {
                    id: 1,
                    send_id: None,
                    method: "send_outcome".into(),
                    params: serde_json::json!(id),
                },
            )
            .await?;
            loop {
                match protocol::read::<Message>(&mut connection).await? {
                    Message::Response { result, .. } => {
                        let outcome: receipts::Outcome =
                            serde_json::from_value(result?).map_err(|_| "Invalid send receipt")?;
                        match outcome {
                            receipts::Outcome::Finished(result) => return result,
                            receipts::Outcome::Missing => return Err(
                                "The host did not admit this message. It is safe to send again."
                                    .into(),
                            ),
                            receipts::Outcome::Pending | receipts::Outcome::Interrupted => break,
                        }
                    }
                    Message::Event(_) | Message::Resync => {}
                    Message::Closed { reason } => return Err(reason),
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Err(format!(
            "Send outcome unknown ({id}). Reload this task before sending again."
        ))
    };
    // Remote receipt lookup includes enrollment/dial/handshake. Keep local
    // recovery quick without squeezing the entire remote setup into five seconds.
    let budget = if endpoint.remote_host.is_some() {
        Duration::from_secs(40)
    } else {
        Duration::from_secs(5)
    };
    match tokio::time::timeout(budget, recovery).await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(error)) if error.starts_with("The host did not admit") => Err(error),
        _ => Err(format!(
            "Send outcome unknown ({id}). Reload this task before sending again; it may already be running."
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn failed_save_owner_proof_requires_an_existing_free_control_lock() {
        let root = tempfile::tempdir().unwrap();
        let paths = maple_agent::store::StorePaths::new(root.path())
            .prepare()
            .unwrap();
        let endpoint = ipc::Endpoint::for_test(paths.root().to_path_buf());
        assert!(owner_is_free(&endpoint).await.is_err());
        assert!(!paths.owner_lock().exists());
        let owner = endpoint.ownership_lock().unwrap();
        owner.try_lock().unwrap();
        assert!(!owner_is_free(&endpoint).await.unwrap());
        // No listener exists: an unavailable endpoint is insufficient while
        // the authoritative host still holds this exact lock.
        drop(owner);
        assert!(owner_is_free(&endpoint).await.unwrap());
        assert!(paths.owner_lock().exists());
    }

    #[tokio::test]
    async fn explicit_stop_remains_idempotent_for_a_never_started_profile() {
        let root = tempfile::tempdir().unwrap();
        let paths = maple_agent::store::StorePaths::new(root.path())
            .prepare()
            .unwrap();
        let endpoint = ipc::Endpoint::for_test(paths.root().to_path_buf());
        assert!(!paths.owner_lock().exists());
        request_stop_endpoint(&endpoint).await.unwrap();
        assert!(owner_is_free(&endpoint).await.unwrap());
    }

    #[test]
    fn missing_cancelled_and_queued_reply_receivers_wipe_owned_secrets() {
        protocol::reset_test_secret_wipes();
        let state = ClientState {
            pending: Mutex::new(Some(HashMap::new())),
            user_id: RwLock::new(None),
            store: RwLock::new(StoreState::Open),
            generation: AtomicU64::new(0),
        };
        let reply = || {
            Ok(SensitiveValue::new(serde_json::json!({
                "passphrase":"maple-wipe-test-secret-client-reply"
            })))
        };
        deliver_response(&state, 1, reply());
        assert_eq!(protocol::test_secret_wipes(), 1);

        let (sender, receiver) = oneshot::channel();
        state
            .pending
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .insert(2, sender);
        drop(receiver);
        deliver_response(&state, 2, reply());
        assert_eq!(protocol::test_secret_wipes(), 2);

        let (sender, receiver) = oneshot::channel();
        state
            .pending
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .insert(3, sender);
        deliver_response(&state, 3, reply());
        assert_eq!(protocol::test_secret_wipes(), 2);
        drop(receiver);
        assert_eq!(protocol::test_secret_wipes(), 3);
    }

    #[test]
    fn secret_reply_decode_wipes_original_json_on_success_and_schema_error() {
        #[derive(serde::Deserialize)]
        struct Suggested {
            passphrase: maple_agent::store::records::SecretString,
            required: bool,
        }
        protocol::reset_test_secret_wipes();
        let invalid = SensitiveValue::new(serde_json::json!({
            "passphrase":"maple-wipe-test-secret-schema"
        }));
        assert!(decode_response::<Suggested>("generate_store_passphrase", invalid).is_err());
        assert_eq!(protocol::test_secret_wipes(), 1);

        let valid = SensitiveValue::new(serde_json::json!({
            "passphrase":"maple-wipe-test-secret-typed", "required":true
        }));
        let decoded: Suggested = decode_response("generate_store_passphrase", valid).unwrap();
        assert_eq!(protocol::test_secret_wipes(), 2);
        assert_eq!(decoded.passphrase.expose(), "maple-wipe-test-secret-typed");
        assert!(decoded.required);

        for method in ["request_account_deletion", "create_api_key", "oauth_start"] {
            let value = SensitiveValue::new(serde_json::Value::String(
                "maple-wipe-test-secret-small-response".into(),
            ));
            assert!(decode_response::<bool>(method, value).is_err());
        }
        assert_eq!(protocol::test_secret_wipes(), 5);
    }

    #[test]
    fn ordinary_history_decoding_moves_large_strings_without_cloning() {
        #[derive(serde::Deserialize)]
        struct Row {
            text: String,
        }
        #[derive(serde::Deserialize)]
        struct Page {
            rows: Vec<Row>,
        }
        let text = "ordinary history row ".repeat(100_000);
        let allocation = text.as_ptr();
        let row = serde_json::Value::Object(serde_json::Map::from_iter([(
            "text".into(),
            serde_json::Value::String(text),
        )]));
        let value = SensitiveValue::new(serde_json::Value::Object(serde_json::Map::from_iter([(
            "rows".into(),
            serde_json::Value::Array(vec![row]),
        )])));
        let page: Page = decode_response("history_before", value).unwrap();
        assert_eq!(page.rows[0].text.as_ptr(), allocation);
        assert!(page.rows[0].text.len() > 1_000_000);
    }

    #[test]
    fn api_key_reply_missing_a_later_field_keeps_partial_keys_guarded() {
        protocol::reset_test_secret_wipes();
        let invalid = SensitiveValue::new(serde_json::json!({
            "name":"fixture", "key":"maple-wipe-test-secret-api-key"
        }));
        assert!(
            decode_response::<maple_agent::maple_api::MapleApiKeyCreated>(
                "create_api_key",
                invalid
            )
            .is_err()
        );
        assert_eq!(protocol::test_secret_wipes(), 1);
        // The DTO's deserializer retains a Zeroizing key until this complete
        // shape succeeds; only then does its existing public String escape.
        let valid = SensitiveValue::new(serde_json::json!({
            "name":"fixture", "key":"maple-wipe-test-secret-api-key", "created_at":"2026-10-21T00:00:00Z"
        }));
        let mut decoded: maple_agent::maple_api::MapleApiKeyCreated =
            decode_response("create_api_key", valid).unwrap();
        assert_eq!(protocol::test_secret_wipes(), 2);
        assert_eq!(decoded.key, "maple-wipe-test-secret-api-key");
        zeroize::Zeroize::zeroize(&mut decoded.key);
    }

    #[tokio::test]
    async fn retired_in_place_repair_refuses_before_runtime_drain_or_source_changes() {
        fn snapshot(
            path: &std::path::Path,
        ) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
            fn visit(
                root: &std::path::Path,
                path: &std::path::Path,
                files: &mut std::collections::BTreeMap<std::path::PathBuf, Vec<u8>>,
            ) {
                for entry in std::fs::read_dir(path).unwrap() {
                    let entry = entry.unwrap();
                    let path = entry.path();
                    if entry.file_type().unwrap().is_dir() {
                        visit(root, &path, files);
                    } else {
                        assert!(entry.file_type().unwrap().is_file());
                        files.insert(
                            path.strip_prefix(root).unwrap().to_owned(),
                            std::fs::read(path).unwrap(),
                        );
                    }
                }
            }
            let mut files = std::collections::BTreeMap::new();
            visit(path, path, &mut files);
            files
        }
        let directory = tempfile::tempdir().unwrap();
        let backend = Arc::new(LocalBackend::isolated_for_test(directory.path()));
        let store = backend.store().unwrap().clone();
        store
            .set_app_setting("repair-refusal-sentinel", &serde_json::json!(true), 1)
            .unwrap();
        let receipts = Arc::new(receipts::Receipts::new(store.clone()));
        let (events, mut observed) = broadcast::channel(16);
        let remote =
            crate::remote::manager::Manager::new(backend.clone(), events.clone(), receipts.clone());
        let owner = OwnerContext::new(Some(&store));
        let generation = ConnectionGeneration::capture(&backend).unwrap();
        let drain = owner.draining.subscribe();
        let before = snapshot(store.paths().root());
        // A stale repair request must return without joining the operation or
        // admission queues, including while another operation owns them.
        let _operation = owner.operation.lock().await;
        let _admission = owner.admission.read().await;
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            owner.dispatch(
                &backend,
                &remote,
                &events,
                protocol::SensitiveRequest(Request {
                    id: 1,
                    method: "repair_store".into(),
                    send_id: None,
                    params: serde_json::json!({"rebuild_timeline":true}),
                }),
                receipts,
                &generation,
            ),
        )
        .await
        .expect("retired repair cannot wait for a runtime drain");
        assert_eq!(
            result.unwrap_err(),
            repair::retired_operation_error("repair_store").unwrap()
        );
        assert_eq!(snapshot(store.paths().root()), before);
        assert_eq!(store.state(), StoreState::Open);
        assert!(!owner.busy.load(Ordering::Acquire));
        assert!(!owner.maintenance.load(Ordering::Acquire));
        assert!(!drain.has_changed().unwrap());
        assert!(matches!(
            observed.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        assert!(owner.completion_notice.lock().unwrap().is_none());
        assert_eq!(
            store.app_setting("repair-refusal-sentinel").unwrap(),
            Some(serde_json::json!(true))
        );
        store
            .set_app_setting("after-retired-repair", &serde_json::json!(true), 2)
            .unwrap();
    }

    #[tokio::test]
    async fn failed_late_shutdown_cleanup_retains_open_store_and_can_retry() {
        let root = tempfile::tempdir().unwrap();
        let backend = Arc::new(LocalBackend::isolated_for_test(root.path()));
        let store = backend.store().unwrap().clone();
        let receipts = Arc::new(receipts::Receipts::new(store.clone()));
        let (events, mut observed) = broadcast::channel(16);
        let remote =
            crate::remote::manager::Manager::new(backend.clone(), events.clone(), receipts);
        let owner = OwnerContext::new(Some(&store));
        let spills = maple_agent::store::spill::SpillRoot::new(store.paths()).unwrap();
        spills
            .task_dir("00000000-0000-0000-0000-000000000001")
            .unwrap();
        std::fs::remove_dir_all(spills.path()).unwrap();
        std::fs::write(spills.path(), b"unsafe spill type fixture").unwrap();

        assert!(owner.shutdown(&backend, &remote, &events).await.is_err());
        assert!(owner.retained_failure.load(Ordering::Acquire));
        assert!(owner.keeps_host(&backend));
        assert!(!owner.shutdown_requested.load(Ordering::Acquire));
        assert_eq!(store.state(), StoreState::Open);
        assert!(matches!(
            observed.try_recv().unwrap().as_ref(),
            AgentServiceEvent::StoreStateChanged {
                state: StoreState::Open
            }
        ));
        store
            .set_app_setting("after-refused-quit", &serde_json::json!(true), 1)
            .unwrap();

        std::fs::remove_file(spills.path()).unwrap();
        owner.shutdown(&backend, &remote, &events).await.unwrap();
        assert_eq!(store.state(), StoreState::Maintenance);
        store.initialize_plain().unwrap();
        assert_eq!(
            store.app_setting("after-refused-quit").unwrap(),
            Some(serde_json::json!(true))
        );
        store.close_for_shutdown().unwrap();
    }

    fn seed_remote_for_shutdown(backend: &LocalBackend, user: &str) -> String {
        use maple_agent::store::records::{CredentialTokens, SecretString};
        let store = backend.store().unwrap();
        let account = store.upsert_account(user, 1).unwrap();
        store
            .replace_credentials(
                None,
                account,
                backend.remote_api_url(),
                "shutdown-remote-session",
                1,
                &CredentialTokens {
                    access_token: SecretString::new("synthetic-token".into()),
                    refresh_token: None,
                },
                1,
            )
            .unwrap();
        backend.reload_open_store().unwrap();
        crate::remote::enable_saved_remote_for_test(backend, user).unwrap()
    }

    #[tokio::test]
    async fn failed_stop_restores_enabled_remote_and_real_retry_retains_ownership() {
        const USER: &str = "shutdown-remote@example.test";
        let root = tempfile::tempdir().unwrap();
        let backend = Arc::new(LocalBackend::isolated_for_test(root.path()));
        let store = backend.store().unwrap().clone();
        let endpoint = ipc::Endpoint::for_test(store.paths().root().to_path_buf());
        let ownership = endpoint.ownership_lock().unwrap();
        ownership.lock().unwrap();
        let contender = endpoint.ownership_lock().unwrap();
        let remote_identity = seed_remote_for_shutdown(&backend, USER);
        let receipts = Arc::new(receipts::Receipts::new(store.clone()));
        let (events, _) = broadcast::channel(16);
        let remote =
            crate::remote::manager::Manager::new(backend.clone(), events.clone(), receipts.clone());
        // The isolated backend has no reachable service. This checks the real
        // restoration/admission path, not successful network registration.
        remote.restore().await;
        assert!(remote.stays_running());
        let owner = OwnerContext::new(Some(&store));
        let generation = ConnectionGeneration::capture(&backend).unwrap();
        let spills = maple_agent::store::spill::SpillRoot::new(store.paths()).unwrap();
        spills
            .task_dir("00000000-0000-0000-0000-000000000002")
            .unwrap();
        std::fs::remove_dir_all(spills.path()).unwrap();
        std::fs::write(spills.path(), b"unsafe spill fixture").unwrap();

        assert!(owner.shutdown(&backend, &remote, &events).await.is_err());
        assert_eq!(backend.saved_user_id().as_deref(), Some(USER));
        assert_eq!(store.state(), StoreState::Open);
        assert!(remote.stays_running(), "failed Stop restores remote retry");
        assert!(matches!(
            contender.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        let status = remote
            .dispatch(Request {
                id: 2,
                method: "remote_status".into(),
                send_id: None,
                params: serde_json::json!([]),
            })
            .await
            .unwrap();
        assert_eq!(status["endpoint"].as_str(), Some(remote_identity.as_str()));

        // Exercise the actual owner dispatcher rather than duplicating its
        // admission/account locking sequence in a test helper.
        owner
            .dispatch(
                &backend,
                &remote,
                &events,
                protocol::SensitiveRequest(Request {
                    id: 1,
                    method: "retry_store_persistence".into(),
                    send_id: None,
                    params: serde_json::json!([]),
                }),
                receipts,
                &generation,
            )
            .await
            .unwrap();
        assert!(!owner.retained_failure.load(Ordering::Acquire));
        assert!(remote.stays_running());
        assert!(matches!(
            contender.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));

        std::fs::remove_file(spills.path()).unwrap();
        owner.shutdown(&backend, &remote, &events).await.unwrap();
        assert!(!remote.stays_running());
        drop(ownership);
        contender.try_lock().unwrap();
    }

    #[tokio::test]
    async fn stop_closes_remote_restored_by_a_previously_admitted_operation() {
        let root = tempfile::tempdir().unwrap();
        let backend = Arc::new(LocalBackend::isolated_for_test(root.path()));
        let store = backend.store().unwrap().clone();
        seed_remote_for_shutdown(&backend, "late-restore@example.test");
        let receipts = Arc::new(receipts::Receipts::new(store.clone()));
        let (events, _) = broadcast::channel(16);
        let remote =
            crate::remote::manager::Manager::new(backend.clone(), events.clone(), receipts);
        remote.restore().await;
        assert!(remote.stays_running());
        let owner = Arc::new(OwnerContext::new(Some(&store)));

        // Hold the same admission reader as an in-flight account operation.
        // Stop first drains remote, then must wait behind this operation.
        let admitted = owner.admission.read().await;
        let stopping = {
            let owner = owner.clone();
            let backend = backend.clone();
            let remote = remote.clone();
            let events = events.clone();
            tokio::spawn(async move { owner.shutdown(&backend, &remote, &events).await })
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            while remote.stays_running() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!stopping.is_finished());

        // An already-admitted operation's completion can start remote again
        // before dropping its reader. Exercise that exact ordering.
        remote.restore().await;
        assert!(remote.stays_running());
        drop(admitted);
        tokio::time::timeout(Duration::from_secs(10), stopping)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!remote.stays_running());
        assert_eq!(store.state(), StoreState::Maintenance);
    }

    #[tokio::test]
    async fn status_waits_for_a_ready_rebound_connection() {
        let root = tempfile::tempdir().unwrap();
        let state = Arc::new(ClientState {
            pending: Mutex::new(Some(HashMap::new())),
            user_id: RwLock::new(None),
            store: RwLock::new(StoreState::Maintenance),
            generation: AtomicU64::new(0),
        });
        let (requests, mut received) = mpsc::channel(MAX_CALLS);
        let client = Arc::new(RemoteClient {
            endpoint: ipc::Endpoint::for_test(root.path().to_path_buf()),
            requests,
            state: state.clone(),
            next_id: AtomicU64::new(1),
            events: tokio::sync::Mutex::new(None),
            permits: Semaphore::new(MAX_CALLS),
        });
        let mut status = tokio::spawn({
            let client = client.clone();
            async move { client.call::<serde_json::Value>("store_status", ()).await }
        });
        let first = received.recv().await.unwrap();
        state
            .pending
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .remove(&first.id)
            .unwrap()
            .send(Ok(SensitiveValue::new(
                serde_json::json!({"state":"Open", "connection_rebind_required":true}),
            )))
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut status)
                .await
                .is_err()
        );
        state.disconnect();
        // A disconnected map is insufficient: the replacement must be ready.
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut status)
                .await
                .is_err()
        );
        *state.pending.lock().unwrap() = Some(HashMap::new());
        let rebound = tokio::time::timeout(Duration::from_secs(1), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rebound.method, "store_status");
        state
            .pending
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .remove(&rebound.id)
            .unwrap()
            .send(Ok(SensitiveValue::new(
                serde_json::json!({"state":"Open", "connection_rebind_required":false}),
            )))
            .unwrap();
        assert_eq!(status.await.unwrap().unwrap()["state"], "Open");
    }

    #[tokio::test]
    async fn queued_write_cannot_cross_a_replaced_store_identity() {
        let old_root = tempfile::tempdir().unwrap();
        let old_store =
            Arc::new(Store::open(maple_agent::store::StorePaths::new(old_root.path())).unwrap());
        let old_backend = LocalBackend::new_with_store(
            "https://fixture.invalid".into(),
            String::new(),
            old_store.clone(),
        )
        .unwrap();
        let generation = ConnectionGeneration::capture(&old_backend).unwrap();
        assert!(generation.0.is_some());
        old_store.close().unwrap();

        // The admission test uses another genuinely initialized SQLite store.
        // Export/setup and its native checker are covered by core lifecycle
        // tests; an app libtest executable is not the production checker CLI.
        let new_root = tempfile::tempdir().unwrap();
        let store =
            Arc::new(Store::open(maple_agent::store::StorePaths::new(new_root.path())).unwrap());
        let backend = Arc::new(
            LocalBackend::new_with_store(
                "https://fixture.invalid".into(),
                String::new(),
                store.clone(),
            )
            .unwrap(),
        );
        assert!(generation.check(&backend, "setup_store").is_err());
        assert!(generation.check(&backend, "unlock").is_err());
        let owner = Arc::new(OwnerContext::new(Some(&store)));
        let exclusive = owner.admission.write().await;
        let (started_tx, started_rx) = oneshot::channel();
        let pending = tokio::spawn({
            let owner = owner.clone();
            let backend = backend.clone();
            async move {
                let _ = started_tx.send(());
                let _admitted = owner.admission.read().await;
                generation.check(&backend, "set_ui_settings")?;
                backend
                    .store()
                    .unwrap()
                    .set_app_setting("stale-fixture", &serde_json::json!(true), 1)
                    .map_err(|e| e.to_string())
            }
        });
        started_rx.await.unwrap();
        drop(exclusive);
        assert!(pending.await.unwrap().unwrap_err().contains("Reconnect"));
        assert_eq!(store.app_setting("stale-fixture").unwrap(), None);
        let rebound = ConnectionGeneration::capture(&backend).unwrap();
        assert!(rebound.check(&backend, "set_ui_settings").is_ok());
        store.close().unwrap();
    }

    #[tokio::test]
    async fn incompatible_clients_cannot_join_but_can_explicitly_stop() {
        for mode in [
            ConnectionMode::Client,
            ConnectionMode::Acp,
            ConnectionMode::Stop,
        ] {
            for mismatch in ["profile", "build", "version"] {
                let (mut client, server) = tokio::io::duplex(4096);
                let mut server: ipc::Connection = Box::new(server);
                let mut hello = Hello {
                    version: protocol::VERSION,
                    profile: "same-storage-profile".into(),
                    build: protocol::BUILD.into(),
                    mode,
                    remote_host: None,
                };
                match mismatch {
                    "profile" => hello.profile = "different-backend".into(),
                    "build" => hello.build = "old-build".into(),
                    _ => hello.version += 1,
                }
                let (reply, accepted) = tokio::join!(
                    async {
                        protocol::write(&mut client, &hello).await.unwrap();
                        protocol::read::<Result<Welcome, String>>(&mut client)
                            .await
                            .unwrap()
                    },
                    accept_handshake(
                        &mut server,
                        "same-storage-profile",
                        Some("test-user".into())
                    )
                );
                assert_eq!(reply.is_ok(), mode == ConnectionMode::Stop, "{mismatch}");
                assert_eq!(
                    accepted.ok(),
                    (mode == ConnectionMode::Stop).then_some(mode)
                );
            }
        }
    }

    #[tokio::test]
    async fn stop_client_accepts_an_older_hosts_welcome() {
        for mode in [ConnectionMode::Client, ConnectionMode::Stop] {
            let (client, mut server) = tokio::io::duplex(4096);
            let (result, ()) = tokio::join!(handshake(Box::new(client), "profile", mode), async {
                let _: Hello = protocol::read(&mut server).await.unwrap();
                protocol::write(
                    &mut server,
                    &Ok::<_, String>(Welcome {
                        version: protocol::VERSION + 1,
                        pid: 1,
                        build: "old-build".into(),
                        user_id: None,
                        store: StoreState::Open,
                    }),
                )
                .await
                .unwrap();
            });
            assert_eq!(result.is_ok(), mode == ConnectionMode::Stop);
        }
    }

    #[tokio::test]
    async fn signed_out_acp_gets_an_error_while_desktop_can_open_login() {
        for mode in [ConnectionMode::Acp, ConnectionMode::Client] {
            let (client, server) = tokio::io::duplex(4096);
            let mut server: ipc::Connection = Box::new(server);
            let (result, _) = tokio::join!(
                handshake(Box::new(client), "profile", mode),
                accept_handshake(&mut server, "profile", None)
            );
            match (mode, result) {
                (ConnectionMode::Acp, Err(error)) => assert!(error.contains("requires sign-in")),
                (ConnectionMode::Client, Ok((_, welcome))) => assert!(welcome.user_id.is_none()),
                _ => panic!("unexpected signed-out handshake result"),
            }
        }
    }

    #[tokio::test]
    async fn remote_update_stop_cannot_use_the_legacy_unconditional_stop_handshake() {
        let (client, server) = tokio::io::duplex(4096);
        let mut server: ipc::Connection = Box::new(server);
        let (result, _) = tokio::join!(
            handshake(
                Box::new(client),
                "profile",
                ConnectionMode::RemoteUpdateStop {
                    operation: *uuid::Uuid::new_v4().as_bytes()
                }
            ),
            accept_handshake(&mut server, "profile", Some("fixture-account".into()))
        );
        // A startup/legacy host has no broker authority and must refuse this
        // typed mode rather than interpreting it as administrative Stop.
        assert!(
            result
                .err()
                .unwrap()
                .contains("Remote updating is unavailable")
        );
    }

    #[tokio::test]
    async fn refused_remote_stop_keeps_the_store_open_without_waiting_for_account_readers() {
        let directory = tempfile::tempdir().unwrap();
        let backend = Arc::new(LocalBackend::isolated_for_test(directory.path()));
        let store = backend.store().unwrap().clone();
        let receipts = Arc::new(receipts::Receipts::new(store.clone()));
        let (events, _) = broadcast::channel(8);
        let remote =
            crate::remote::manager::Manager::new(backend.clone(), events.clone(), receipts);
        let owner = OwnerContext::new(Some(&store));
        let account_reader = backend.account_gate.read().await;
        let (accepted, acceptance) = oneshot::channel();
        let started = Arc::new(AtomicBool::new(false));
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            owner.shutdown_with_update(
                &backend,
                &remote,
                &events,
                Some(StopRequest {
                    operation: Some(*uuid::Uuid::new_v4().as_bytes()),
                    accepted: Some(accepted),
                    acknowledged: None,
                    started: started.clone(),
                }),
            ),
        )
        .await
        .unwrap();
        assert!(result.is_err());
        assert!(acceptance.await.unwrap().is_err());
        assert_eq!(store.state(), StoreState::Open);
        assert!(!owner.shutdown_requested.load(Ordering::Acquire));
        assert!(!owner.busy.load(Ordering::Acquire));
        assert!(!started.load(Ordering::Acquire));
        drop(account_reader);
    }

    #[tokio::test]
    async fn busy_remote_stop_does_not_cancel_an_active_local_operation() {
        // Exercise both exclusive guards. Ordinary local Stop deliberately
        // cancels this same real OAuth primitive; an unadmitted remote update
        // must leave its pending publication and the store untouched.
        for operation_busy in [true, false] {
            let directory = tempfile::tempdir().unwrap();
            let backend = Arc::new(LocalBackend::isolated_for_test(directory.path()));
            let store = backend.store().unwrap().clone();
            let receipts = Arc::new(receipts::Receipts::new(store.clone()));
            let (events, _) = broadcast::channel(8);
            let remote =
                crate::remote::manager::Manager::new(backend.clone(), events.clone(), receipts);
            let owner = OwnerContext::new(Some(&store));
            let _operation = if operation_busy {
                Some(owner.operation.lock().await)
            } else {
                None
            };
            let _admission = if operation_busy {
                None
            } else {
                Some(owner.admission.read().await)
            };
            let published = Arc::new(AtomicBool::new(false));
            let (release, waiting) = oneshot::channel();
            let local_operation = backend.pending_oauth_waiter_for_test(waiting, published.clone());
            let (accepted, acceptance) = oneshot::channel();
            let started = Arc::new(AtomicBool::new(false));
            let result = tokio::time::timeout(
                Duration::from_secs(5),
                owner.shutdown_with_update(
                    &backend,
                    &remote,
                    &events,
                    Some(StopRequest {
                        operation: Some(*uuid::Uuid::new_v4().as_bytes()),
                        accepted: Some(accepted),
                        acknowledged: None,
                        started: started.clone(),
                    }),
                ),
            )
            .await
            .unwrap();
            assert!(result.unwrap_err().contains("busy with a local operation"));
            assert!(acceptance.await.unwrap().unwrap_err().contains("busy"));
            assert!(!owner.shutdown_requested.load(Ordering::Acquire));
            assert!(!owner.busy.load(Ordering::Acquire));
            assert!(!started.load(Ordering::Acquire));
            assert_eq!(store.state(), StoreState::Open);
            release.send(()).unwrap();
            assert!(
                tokio::time::timeout(Duration::from_secs(5), local_operation)
                    .await
                    .unwrap()
                    .is_ok()
            );
            assert!(
                published.load(Ordering::Acquire),
                "remote refusal cancelled unrelated local work"
            );
        }
    }

    #[tokio::test]
    async fn replacement_waits_for_shutdown_to_release_ownership() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("host.lock");
        let old = std::fs::File::create(&path).unwrap();
        old.lock().unwrap();
        let replacement = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let waiting = wait_for_ownership(&replacement, Duration::from_secs(5));
        tokio::pin!(waiting);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut waiting)
                .await
                .is_err()
        );
        drop(old);
        tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .unwrap()
            .unwrap();
        let contender = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(matches!(
            contender.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        assert!(
            wait_for_ownership(&contender, Duration::from_millis(50))
                .await
                .is_err()
        );
    }
    #[test]
    fn locked_gate_rejects_data_auth_remote_and_subscription_methods() {
        for method in [
            "login",
            "get_ui_settings",
            "set_ui_settings",
            "subscribe",
            "remote_status",
            "remote_enable",
            "send_message",
            "open_task_page",
            "browse_project_folders",
            "create_project_folder",
            "generate_store_passphrase",
            "setup_store",
            "retry_store_persistence",
            "repair_store",
            "salvage_store",
            "authenticate_repair_password",
            "acknowledge_mode_change_notice",
        ] {
            assert_eq!(
                gate(&StoreState::Locked, method),
                Err(LOCKED_MESSAGE.to_owned()),
                "{method}"
            );
        }
        for method in [
            "store_status",
            "host_status",
            "unlock",
            "erase_local_data",
            "detach",
            "cancel_oauth",
        ] {
            assert!(gate(&StoreState::Locked, method).is_ok(), "{method}");
        }
        assert!(gate(&StoreState::NeedsSetup, "setup_store").is_ok());
        assert!(gate(&StoreState::NeedsSetup, "login").is_err());
        let damaged_generation = StoreState::Damaged {
            reason: maple_agent::store::DamageReason::Generation,
        };
        assert!(gate(&damaged_generation, "unlock").is_ok());
        assert!(gate(&damaged_generation, "login").is_err());
        assert!(gate(&damaged_generation, "remote_status").is_err());
        assert!(
            gate(
                &StoreState::Damaged {
                    reason: maple_agent::store::DamageReason::Database,
                },
                "unlock"
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn locked_acp_and_remote_selection_fail_before_auth_or_network() {
        for (mode, peer) in [
            (ConnectionMode::Acp, None),
            (ConnectionMode::Client, Some("unused-peer".to_owned())),
        ] {
            let (mut client, server) = tokio::io::duplex(4096);
            let mut server: ipc::Connection = Box::new(server);
            let hello = Hello {
                version: protocol::VERSION,
                profile: "fixture".into(),
                build: protocol::BUILD.into(),
                mode,
                remote_host: peer,
            };
            let (response, accepted) = tokio::join!(
                async {
                    protocol::read::<Result<Welcome, String>>(&mut client)
                        .await
                        .unwrap()
                },
                accept_hello(
                    &mut server,
                    hello,
                    "fixture",
                    None,
                    StoreState::Locked,
                    None,
                    None
                )
            );
            assert_eq!(response.err().as_deref(), Some(LOCKED_MESSAGE));
            assert_eq!(accepted.err().as_deref(), Some(LOCKED_MESSAGE));
        }
    }

    #[tokio::test]
    async fn oversized_borrowed_events_request_resync_without_sending_the_payload() {
        let event = AgentServiceEvent::HistoryReplaced {
            user_id: "fixture@example.test".into(),
            session_id: "x".repeat(4 * 1024 * 1024 + 1),
            rows_epoch: 1,
        };
        let (mut sender, mut receiver) = tokio::io::duplex(512);
        let (written, message) = tokio::join!(
            write_event(&mut sender, &event),
            protocol::read::<Message>(&mut receiver)
        );
        written.unwrap();
        assert!(matches!(message.unwrap(), Message::Resync));
    }

    #[test]
    fn committed_password_warnings_are_latched_as_nonsecret_static_notices() {
        let result = Ok(
            serde_json::json!({"mode":"password","backup_pending":true,"synchronization_pending":false}),
        );
        let notice = completion_message("change_store_password", &result).unwrap();
        assert!(!notice.is_error);
        assert!(!notice.inline_password_success);
        assert!(notice.message.contains("change is active"));
        assert!(uuid::Uuid::parse_str(&notice.operation_id).is_ok());
        assert!(completion_message("unlock", &Ok(serde_json::json!({}))).is_none());
    }

    #[test]
    fn routine_store_completions_do_not_require_attention() {
        for method in [
            "set_store_password",
            "change_store_password",
            "remove_store_password",
            "erase_local_data",
            "start_fresh",
            "setup_store",
            "salvage_store",
        ] {
            let notice = completion_message(method, &Ok(serde_json::json!({}))).unwrap();
            assert!(!notice.is_error, "{method}");
            assert!(!notice.requires_attention, "{method}");
        }
        assert!(completion_message("unlock", &Ok(serde_json::json!({}))).is_none());
    }

    #[test]
    fn store_operation_failures_require_attention() {
        for method in [
            "unlock",
            "set_store_password",
            "change_store_password",
            "remove_store_password",
            "erase_local_data",
            "start_fresh",
            "setup_store",
            "salvage_store",
        ] {
            let notice = completion_message(method, &Err("Try again".into())).unwrap();
            assert!(notice.is_error, "{method}");
            assert!(notice.requires_attention, "{method}");
            assert_eq!(notice.message, "Try again", "{method}");
        }
    }

    #[test]
    fn each_pending_store_metadata_flag_requires_attention_after_success() {
        for method in [
            "unlock",
            "set_store_password",
            "change_store_password",
            "remove_store_password",
            "erase_local_data",
            "start_fresh",
            "setup_store",
            "salvage_store",
        ] {
            for pending in [
                "backup_pending",
                "synchronization_pending",
                "notice_pending",
                "reconciliation_pending",
            ] {
                let mut result = serde_json::json!({});
                result[pending] = true.into();
                let notice = completion_message(method, &Ok(result)).unwrap();
                assert!(!notice.is_error, "{method}: {pending}");
                assert!(notice.requires_attention, "{method}: {pending}");
                assert!(
                    notice.message.contains("change is active"),
                    "{method}: {pending}"
                );
                assert!(!notice.inline_password_success, "{method}: {pending}");
            }
        }
    }

    #[test]
    fn previous_password_unlock_requires_attention_even_without_pending_metadata() {
        for pending in [false, true] {
            let notice = completion_message(
                "unlock",
                &Ok(serde_json::json!({
                    "used_previous_password": true,
                    "backup_pending": pending,
                })),
            )
            .unwrap();
            assert!(!notice.is_error);
            assert!(notice.requires_attention);
            assert!(
                notice
                    .message
                    .starts_with("That was your previous unlock password.")
            );
            assert!(!notice.inline_password_success);
        }
        assert!(
            completion_message(
                "unlock",
                &Ok(serde_json::json!({"used_previous_password": false})),
            )
            .is_none()
        );
    }

    #[test]
    fn only_clean_password_changes_allow_inline_success_routing() {
        for method in [
            "set_store_password",
            "change_store_password",
            "remove_store_password",
        ] {
            assert!(
                completion_message(method, &Ok(serde_json::json!({})))
                    .unwrap()
                    .inline_password_success
            );
            assert!(
                !completion_message(method, &Err("Try again".into()))
                    .unwrap()
                    .inline_password_success
            );
            for pending in [
                "backup_pending",
                "synchronization_pending",
                "notice_pending",
                "reconciliation_pending",
            ] {
                let mut result = serde_json::json!({});
                result[pending] = true.into();
                assert!(
                    !completion_message(method, &Ok(result))
                        .unwrap()
                        .inline_password_success
                );
            }
        }
        assert!(
            !completion_message(
                "unlock",
                &Ok(serde_json::json!({"used_previous_password":true}))
            )
            .unwrap()
            .inline_password_success
        );
    }

    #[test]
    fn mode_receipt_failure_warns_without_reversing_committed_result() {
        let mut value = serde_json::json!({"mode":"password"});
        attach_mode_notice_receipt(&mut value, Err(maple_agent::store::StoreError::Busy));
        assert_eq!(value["mode"], "password");
        assert_eq!(value["notice_pending"], true);
        assert!(
            !completion_message("set_store_password", &Ok(value))
                .unwrap()
                .inline_password_success
        );
    }

    #[test]
    fn presentation_operation_identifiers_are_optional_bounded_and_canonical() {
        let id = uuid::Uuid::new_v4().to_string();
        assert_eq!(
            password_ui_operation_id(
                "set_store_password",
                &serde_json::json!({"ui_operation_id":id.to_uppercase()})
            )
            .unwrap(),
            Some(id)
        );
        assert!(
            password_ui_operation_id("set_store_password", &serde_json::json!({}))
                .unwrap()
                .is_none()
        );
        for id in [
            serde_json::json!("x".repeat(4096)),
            serde_json::json!(7),
            serde_json::json!("not-a-uuid"),
        ] {
            assert!(
                password_ui_operation_id(
                    "change_store_password",
                    &serde_json::json!({"ui_operation_id":id})
                )
                .is_err()
            );
        }
    }
}

#[cfg(test)]
mod operation_progress_tests {
    use super::*;
    use maple_agent::store::lifecycle::{OperationPhase, StoreProgress};

    fn active(run: u64) -> ActiveStoreOperation {
        ActiveStoreOperation {
            operation: "change_store_password".into(),
            ui_operation_id: Some("current-form-attempt".into()),
            baseline_run_id: run,
        }
    }

    #[test]
    fn previous_export_counters_do_not_appear_during_authentication_or_key_only_rewrap() {
        let cached = StoreProgress {
            run_id: 7,
            phase: OperationPhase::Complete,
            completed_bytes: 8 << 20,
            total_bytes: 8 << 20,
            remaining_ms: Some(0),
            cancellable: false,
        };
        let value = operation_progress_value(Some(active(7)), cached.clone(), None).unwrap();
        assert_eq!(value["busy"], true);
        assert_eq!(value["operation"], "change_store_password");
        assert_eq!(value["ui_operation_id"], "current-form-attempt");
        assert_eq!(value["phase"], "preparing");
        assert_eq!(value["completed_bytes"], 0);
        assert_eq!(value["total_bytes"], 0);
        assert!(value["remaining_ms"].is_null());
        let idle = operation_progress_value(None, cached, None).unwrap();
        assert_eq!(idle["busy"], false);
        assert_eq!(idle["phase"], "idle");
        assert_eq!(idle["total_bytes"], 0);
    }

    #[test]
    fn only_a_new_export_run_contributes_counters_to_the_current_operation() {
        let core = StoreProgress {
            run_id: 8,
            phase: OperationPhase::Exporting,
            completed_bytes: 1024,
            total_bytes: 4096,
            remaining_ms: Some(1250),
            cancellable: true,
        };
        let value = operation_progress_value(Some(active(7)), core, None).unwrap();
        assert_eq!(value["phase"], "exporting");
        assert_eq!(value["completed_bytes"], 1024);
        assert_eq!(value["remaining_ms"], 1250);
        assert_eq!(value["operation"], "change_store_password");
    }

    #[test]
    fn operation_progress_authority_is_cleared_on_early_return() {
        let metadata = Mutex::new(Some(active(7)));
        let refusal = || -> Result<(), ()> {
            let _guard = ActiveStoreOperationGuard(&metadata);
            Err(())
        };
        assert!(refusal().is_err());
        assert!(metadata.lock().unwrap().is_none());
    }
}
