//! Backend boundary for the gpui frontend.
//!
//! This is the only module that imports `maple_agent`. It owns a private
//! Tokio runtime and exposes an async facade plus an event stream. The UI
//! never touches the agent runtime directly, so this seam can later be moved
//! behind a process or socket boundary without touching UI code.

// This module is the desktop frontend's boundary. A headless build (no
// `desktop` feature) uses only a few entry points, so the rest is unused
// there by design.
#![cfg_attr(not(feature = "desktop"), allow(dead_code))]

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::process::Command;
use std::sync::Arc;

use maple_agent::agent::{
    AgentCreateSessionRequest, AgentDesktopQueueSnapshot, AgentEventSink, AgentIntegration,
    AgentIntegrationPermissionKind, AgentIntegrationPermissions, AgentProjectRootRegistration,
    AgentProjectTrustStatus, AgentPythonStatus, AgentQueueControlRequest,
    AgentRenameSessionRequest, AgentRuntimeStatus, AgentSendMessageRequest, AgentServiceEvent,
    AgentSessionDetail, AgentSessionSummary, AgentSetIntegrationEnabledRequest,
    AgentSetupIntegrationRequest, AgentSlashCommand, AgentStartRequest, AgentSubagent,
    MapleAgentHostResources, MapleAgentService, RecentProjectRoot,
};
use maple_agent::maple_api::{
    MapleApiAuthEventSink, MapleApiAuthRequest, MapleApiAuthSnapshot, MapleApiAuthState,
};
use maple_agent::open_secret_config::configured_pcr0_environment;
use maple_sdk::OpenSecretClient;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct PendingQuestion {
    pub session_id: String,
    pub request_id: String,
    /// One or more related questions answered together in one card.
    pub questions: Vec<maple_agent::agent::AgentQuestion>,
}

#[derive(Debug, Clone)]
pub struct PendingPermission {
    pub session_id: String,
    pub run_id: String,
    pub request_id: String,
    pub tool_name: String,
    pub prompt: Option<String>,
    /// Prepared tool arguments, including literal multiline Python source,
    /// formatted once when the request arrives instead of on every frame.
    pub arguments: Arc<str>,
}

// Re-exported for the settings screens; a headless build has no reader.
#[cfg_attr(not(feature = "desktop"), allow(unused_imports))]
pub use maple_agent::maple_api::{
    MapleAccount, MapleAccountError, MapleApiKey, MapleApiKeyCreated, MapleLoginMethod,
};

/// The signed-in account identity.
#[derive(Debug, Clone)]
pub struct AuthSession {
    pub user_id: String,
}

/// Result of the background credential validation started by
/// [`AgentBackend::restore_in_background`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreOutcome {
    /// The server accepted the saved credentials; the session is installed.
    Valid(String),
    /// The server rejected the saved credentials; they were cleared.
    Rejected,
    /// Offline, timeout, or a server fault. The credentials may still be
    /// good, so they are kept for the next launch.
    Unavailable,
}

/// Everything the chat screen can show before any network call: the saved
/// project root, the task list, the recent roots, and the newest task's
/// transcript. Read in one backend call so it all lands before a runtime
/// start takes the lifecycle lock for its network round trips.
pub struct LocalBootstrap {
    pub project_root: Option<String>,
    pub sessions: Vec<AgentSessionSummary>,
    pub recent_roots: Vec<String>,
    pub latest: Option<AgentSessionDetail>,
}

pub struct AgentBackend {
    runtime: Runtime,
    service: MapleAgentService,
    auth: MapleApiAuthState,
    api_url: String,
    persisted_auth: Arc<PersistedAuthStore>,
    client_id: Uuid,
    event_rx: tokio::sync::Mutex<Option<mpsc::UnboundedReceiver<AgentServiceEvent>>>,
    billing: crate::billing::BillingClient,
    /// Cached billing JWT per user id. Replaced after a 401.
    billing_tokens: tokio::sync::Mutex<HashMap<String, String>>,
    /// Open handle to the usage ledger DB; the context ring polls it every
    /// second during a run, so it is not reopened per query. Shared with
    /// the blocking task that runs each query.
    usage_db: Arc<std::sync::Mutex<Option<(PathBuf, rusqlite::Connection)>>>,
    /// Open handle to the app-owned tool summary store; keyed by account
    /// scope path so a user switch reopens it.
    summary_db: std::sync::Mutex<Option<(PathBuf, rusqlite::Connection)>>,
    /// True while a background credential restore runs. Calls that need a
    /// validated session wait on it (see `session_for`); local reads do not.
    restore_pending: (
        tokio::sync::watch::Sender<bool>,
        tokio::sync::watch::Receiver<bool>,
    ),
}

fn configured_client_id() -> Uuid {
    client_id_from(crate::env::env_string("MAPLE_CLIENT_ID").as_deref())
}

/// The client id to send: `MAPLE_CLIENT_ID` when it is a UUID, else the
/// production id. A malformed override is logged instead of silently
/// pointing a test build at production.
fn client_id_from(configured: Option<&str>) -> Uuid {
    let default = || DEFAULT_CLIENT_ID.parse().expect("valid uuid");
    let Some(value) = configured else {
        return default();
    };
    match value.parse() {
        Ok(id) => id,
        Err(error) => {
            log::warn!(
                "MAPLE_CLIENT_ID {value:?} is not a UUID ({error}); using the default client id"
            );
            default()
        }
    }
}

struct ChannelEventSink(mpsc::UnboundedSender<AgentServiceEvent>);

impl AgentEventSink for ChannelEventSink {
    fn emit(&self, event: &AgentServiceEvent) {
        // The channel is unbounded, so sends only fail after the UI dropped
        // the receiver (window closed). The runtime tolerates missing
        // notifications for that case; surface it once for diagnosis.
        if self.0.send(event.clone()).is_err() {
            log::debug!("agent event receiver is gone; dropping further events");
        }
    }
}

const APP_DIR_NAME: &str = "maple-gpui";

/// Maple's public OpenSecret project id. The backend rejects unknown
/// client ids, so this must match the registered project.
const DEFAULT_CLIENT_ID: &str = "ba5a14b5-d915-47b1-b7b1-afda52bc5fc6";

/// OAuth providers supported by the OpenSecret backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthProvider {
    Github,
    Google,
    Apple,
}

impl OAuthProvider {
    pub fn label(self) -> &'static str {
        match self {
            Self::Github => "GitHub",
            Self::Google => "Google",
            Self::Apple => "Apple",
        }
    }
}

impl AgentBackend {
    /// The account scope (sha of the user id) used for on-disk layout.
    pub fn account_scope(&self, user_id: &str) -> Option<String> {
        maple_agent::maple_api::account_scope(user_id).ok()
    }
}

/// App configuration root (XDG-style), also used by the settings store.
pub fn app_config_root() -> PathBuf {
    config_root()
}

/// Path to the goose sessions database for one account scope. The agent
/// runtime owns and writes this file; the app only reads it.
pub fn account_session_db(account_scope: &str) -> PathBuf {
    local_data_root()
        .join("agent")
        .join("accounts")
        .join(account_scope)
        .join("goose")
        .join("data")
        .join("sessions")
        .join("sessions.db")
}

/// Open the goose sessions database for reading. Returns `None` when the
/// file does not exist yet (read-only open never creates it). The busy
/// timeout covers the short locks goose takes for WAL checkpoints.
pub fn open_session_db_read_only(path: &std::path::Path) -> Option<rusqlite::Connection> {
    use rusqlite::OpenFlags;
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_URI;
    let conn = match rusqlite::Connection::open_with_flags(path, flags) {
        Ok(conn) => conn,
        Err(error) => {
            if path.exists() {
                log::warn!("Cannot open session db {}: {error}", path.display());
            }
            return None;
        }
    };
    if let Err(error) = conn.busy_timeout(std::time::Duration::from_secs(5)) {
        log::warn!("Cannot set busy timeout on {}: {error}", path.display());
    }
    Some(conn)
}

/// Path to the app-owned store of model-written tool call summaries for
/// one account scope. Lives next to the agent data so it is removed with
/// the account.
fn account_summary_db(account_scope: &str) -> PathBuf {
    local_data_root()
        .join("agent")
        .join("accounts")
        .join(account_scope)
        .join("tool_summaries.db")
}

/// Open (and create) the tool summary store.
fn open_summary_db(path: &std::path::Path) -> Result<rusqlite::Connection, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("Cannot create {}: {error}", parent.display()))?;
    }
    let conn = rusqlite::Connection::open(path)
        .map_err(|error| format!("Cannot open {}: {error}", path.display()))?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; \
         CREATE TABLE IF NOT EXISTS tool_summaries ( \
             session_id TEXT NOT NULL, \
             item_id TEXT NOT NULL, \
             summary TEXT NOT NULL, \
             PRIMARY KEY (session_id, item_id) \
         );",
    )
    .map_err(|error| format!("Cannot init {}: {error}", path.display()))?;
    Ok(conn)
}

/// Root for configuration that may roam between machines. Mirrors Tauri's
/// `app_config_dir`: `~/.config` on Linux, `~/Library/Application Support`
/// on macOS, `%APPDATA%` on Windows. `XDG_CONFIG_HOME` overrides it on
/// every platform so tests and portable installs can redirect it.
fn config_root() -> PathBuf {
    let base = env_dir("XDG_CONFIG_HOME")
        .or_else(dirs::config_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(APP_DIR_NAME)
}

/// Root for device-local data: session history, attachments, logs, and
/// credentials. Mirrors Tauri's `app_local_data_dir`: `~/.local/share` on
/// Linux, `~/Library/Application Support` on macOS, `%LOCALAPPDATA%` on
/// Windows. `XDG_DATA_HOME` overrides it on every platform.
pub fn local_data_root() -> PathBuf {
    let base = env_dir("XDG_DATA_HOME")
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(APP_DIR_NAME)
}

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// Root the desktop app opens when the account has no saved root. The GUI
/// must not depend on the directory it was launched from: that is the job of
/// the `maple acp` command, not a windowed app started from a launcher.
fn fallback_project_root() -> Option<String> {
    home_dir().map(|path| path.to_string_lossy().to_string())
}

/// Root for a GUI start: the saved default when it still is a folder, else
/// the home directory. Never the process working directory.
fn gui_project_root(config: &maple_agent::agent::AgentConfig) -> Option<String> {
    config
        .default_project_root
        .as_deref()
        .filter(|path| !path.trim().is_empty() && std::path::Path::new(path).is_dir())
        .map(str::to_owned)
        .or_else(fallback_project_root)
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
struct PersistedAuthRecord {
    user_id: String,
    api_url: String,
    access_token: String,
    refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revision: Option<u64>,
}

impl PersistedAuthRecord {
    fn from_snapshot(api_url: &str, snapshot: &MapleApiAuthSnapshot) -> Self {
        Self {
            user_id: snapshot.user_id.clone(),
            api_url: api_url.to_string(),
            access_token: snapshot.access_token.clone(),
            refresh_token: snapshot.refresh_token.clone(),
            session_id: Some(snapshot.session_id.clone()),
            revision: Some(snapshot.revision),
        }
    }

    fn owns_snapshot(&self, snapshot: &MapleApiAuthSnapshot) -> bool {
        self.user_id == snapshot.user_id
            && self.session_id.as_deref() == Some(snapshot.session_id.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PersistedAuthOwner {
    user_id: String,
    session_id: String,
    revision: u64,
}

impl PersistedAuthOwner {
    fn from_snapshot(snapshot: &MapleApiAuthSnapshot) -> Self {
        Self {
            user_id: snapshot.user_id.clone(),
            session_id: snapshot.session_id.clone(),
            revision: snapshot.revision,
        }
    }

    fn matches(&self, snapshot: &MapleApiAuthSnapshot) -> bool {
        self.user_id == snapshot.user_id && self.session_id == snapshot.session_id
    }
}

#[derive(Default)]
struct PersistedAuthState {
    owner: Option<PersistedAuthOwner>,
}

/// Serializes persisted credential ownership with token-rotation callbacks.
/// A queued callback may update only the exact in-memory auth session that
/// currently owns the file; sign-out compare-clears that same opaque session.
struct PersistedAuthStore {
    path: PathBuf,
    api_url: String,
    state: std::sync::Mutex<PersistedAuthState>,
}

impl PersistedAuthStore {
    fn new(path: PathBuf, api_url: String) -> Self {
        Self {
            path,
            api_url,
            state: std::sync::Mutex::new(PersistedAuthState::default()),
        }
    }

    fn read_record(&self) -> Option<PersistedAuthRecord> {
        let bytes = std::fs::read(&self.path).ok()?;
        let record: PersistedAuthRecord = serde_json::from_slice(&bytes).ok()?;
        (record.api_url == self.api_url).then_some(record)
    }

    fn load(&self) -> Option<PersistedAuthRecord> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let record = self.read_record()?;
        state.owner = record
            .session_id
            .as_ref()
            .map(|session_id| PersistedAuthOwner {
                user_id: record.user_id.clone(),
                session_id: session_id.clone(),
                revision: record.revision.unwrap_or(0),
            });
        Some(record)
    }

    fn claim_and_persist(&self, snapshot: &MapleApiAuthSnapshot) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .owner
            .as_ref()
            .is_some_and(|owner| owner.matches(snapshot) && snapshot.revision < owner.revision)
        {
            log::debug!(
                "ignoring stale persisted auth claim (revision {})",
                snapshot.revision
            );
            return;
        }
        state.owner = Some(PersistedAuthOwner::from_snapshot(snapshot));
        self.write_locked(&PersistedAuthRecord::from_snapshot(&self.api_url, snapshot));
    }

    fn persist_rotation(&self, snapshot: &MapleApiAuthSnapshot) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(owner) = state.owner.as_mut() else {
            log::debug!(
                "ignoring rotated credentials for an unowned persisted auth session (revision {})",
                snapshot.revision
            );
            return;
        };
        if !owner.matches(snapshot) || snapshot.revision < owner.revision {
            log::debug!(
                "ignoring stale rotated credentials (revision {})",
                snapshot.revision
            );
            return;
        }
        owner.revision = snapshot.revision;
        self.write_locked(&PersistedAuthRecord::from_snapshot(&self.api_url, snapshot));
    }

    fn clear_if_owned(&self, snapshot: &MapleApiAuthSnapshot) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .owner
            .as_ref()
            .is_some_and(|owner| owner.matches(snapshot))
        {
            state.owner = None;
        }
        if self
            .read_record()
            .is_some_and(|record| record.owns_snapshot(snapshot))
        {
            self.remove_locked();
        }
    }

    fn clear_record_if_unchanged(&self, expected: &PersistedAuthRecord) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.read_record().as_ref() != Some(expected) {
            return;
        }
        if state
            .owner
            .as_ref()
            .is_some_and(|owner| expected.session_id.as_deref() == Some(owner.session_id.as_str()))
        {
            state.owner = None;
        }
        self.remove_locked();
    }

    fn write_locked(&self, record: &PersistedAuthRecord) {
        if let Err(error) = maple_agent::private_file::write_private_json(&self.path, record) {
            log::error!(
                "Cannot save credentials to {}: {error}. Sign in is needed again at the next start.",
                self.path.display()
            );
        }
    }

    fn remove_locked(&self) {
        if let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            log::error!(
                "Cannot remove credentials at {}: {error}",
                self.path.display()
            );
        }
    }
}

struct PersistAuthSink {
    store: Arc<PersistedAuthStore>,
}

impl MapleApiAuthEventSink for PersistAuthSink {
    fn auth_changed(&self, snapshot: &MapleApiAuthSnapshot) {
        // Tokens never reach the log; only the revision does.
        log::debug!(
            "persisting rotated credentials (revision {})",
            snapshot.revision
        );
        // Keep this small write synchronous with the session publication. A
        // detached writer could run after sign-out and resurrect credentials;
        // the store mutex also orders it against compare-and-clear.
        self.store.persist_rotation(snapshot);
    }
}

impl AgentBackend {
    pub fn new(api_url: String, harness_instructions: String) -> Result<Self, String> {
        // Enforce the credential-bearing URL policy before any client is
        // built, including the login-time SDK client.
        let api_url = maple_agent::maple_api::validate_api_url(&api_url)?;
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let paths =
            maple_agent::agent::AgentPathLayout::from_app_roots(config_root(), local_data_root());
        // Keeps ACP bridge credentials out of desktop tool environments.
        let default_tool_context = maple_agent::agent::default_tool_context_spec()?;
        let service = MapleAgentService::new(MapleAgentHostResources::new(
            paths,
            Arc::new(ChannelEventSink(event_tx)),
            default_tool_context,
            harness_instructions,
        ));
        let runtime =
            Runtime::new().map_err(|error| format!("failed to start runtime: {error}"))?;
        let persisted_auth = Arc::new(PersistedAuthStore::new(Self::auth_file(), api_url.clone()));
        // reqwest clients must be built inside a Tokio runtime context; one
        // built outside never completes a request.
        let billing = {
            let _guard = runtime.enter();
            crate::billing::BillingClient::new(crate::billing::configured_billing_api_url())?
        };
        Ok(Self {
            runtime,
            service,
            auth: MapleApiAuthState::new(),
            api_url,
            persisted_auth,
            client_id: configured_client_id(),
            event_rx: tokio::sync::Mutex::new(Some(event_rx)),
            billing,
            billing_tokens: tokio::sync::Mutex::new(HashMap::new()),
            usage_db: Arc::new(std::sync::Mutex::new(None)),
            summary_db: std::sync::Mutex::new(None),
            restore_pending: tokio::sync::watch::channel(false),
        })
    }

    /// Replace the opening system prompt text for tasks this app hosts.
    /// Applies to agents built after the call, so to a task's next fresh
    /// agent, not to one already loaded.
    pub fn set_harness_instructions(&self, harness_instructions: String) {
        self.service.set_harness_instructions(harness_instructions);
    }

    pub fn api_url(&self) -> &str {
        &self.api_url
    }

    /// The backend runtime, for command-line modes that block on one call.
    pub fn runtime_handle(&self) -> tokio::runtime::Handle {
        self.runtime.handle().clone()
    }

    /// Run a backend future on the backend runtime. The returned handle is a
    /// plain future, so the UI executor can await it without owning Tokio.
    pub fn spawn<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.runtime.spawn(future)
    }

    /// Take the backend event stream. Only the first caller receives it.
    pub async fn take_events(&self) -> Option<mpsc::UnboundedReceiver<AgentServiceEvent>> {
        self.event_rx.lock().await.take()
    }

    fn normalize_email(email: &str) -> Result<String, String> {
        let email = email.trim().to_ascii_lowercase();
        if email.is_empty() {
            return Err("Enter an email address".to_string());
        }
        Ok(email)
    }

    /// Sign in with email and password through the OpenSecret SDK, then hand
    /// the validated credentials to the agent runtime.
    pub async fn login(&self, email: String, password: String) -> Result<AuthSession, String> {
        let email = Self::normalize_email(&email)?;
        if password.is_empty() {
            return Err("Enter a password".to_string());
        }
        let environment = configured_pcr0_environment()?;
        let client = OpenSecretClient::new_with_pcr0_environment(self.api_url.clone(), environment)
            .map_err(|_| "Maple API authentication failed".to_string())?;
        let response = client
            .login(email.clone(), password, self.client_id)
            .await
            .map_err(|_| "Sign in failed. Check your email and password.".to_string())?;
        let user_id = response.id.to_string();
        let snapshot = self
            .auth
            .set_auth(
                self.auth_sink(),
                MapleApiAuthRequest {
                    user_id: user_id.clone(),
                    api_url: self.api_url.clone(),
                    access_token: response.access_token,
                    refresh_token: Some(response.refresh_token),
                },
            )
            .await
            .map_err(|message| {
                // Keep validation detail out of the UI; it can echo the
                // configured URL back to the user.
                log::debug!("set_auth failed during sign in: {message}");
                "Sign in failed. Try again.".to_string()
            })?;
        self.persist_auth(&snapshot);
        Ok(AuthSession { user_id })
    }

    /// Credentials are device-local, like the web app's `localStorage`.
    /// They must not sit in a roaming profile (`%APPDATA%`), so they live in
    /// the local data root rather than next to `settings.json`.
    fn auth_file() -> std::path::PathBuf {
        local_data_root().join("auth.json")
    }

    fn persist_auth(&self, snapshot: &MapleApiAuthSnapshot) {
        self.persisted_auth.claim_and_persist(snapshot);
    }

    /// The sink the runtime calls when the SDK rotates the token pair
    /// during an API call. It writes the new pair to `auth.json` so the
    /// next launch does not restore stale tokens.
    fn auth_sink(&self) -> Arc<dyn MapleApiAuthEventSink> {
        Arc::new(PersistAuthSink {
            store: Arc::clone(&self.persisted_auth),
        })
    }

    fn load_persisted_auth(&self) -> Option<PersistedAuthRecord> {
        self.persisted_auth.load()
    }

    /// The account id saved by a previous sign-in, without validating it.
    /// A local file read, so the UI may show the account's data at once
    /// while [`Self::restore_in_background`] validates the credentials.
    pub fn saved_user_id(&self) -> Option<String> {
        self.load_persisted_auth().map(|record| record.user_id)
    }

    /// Restore a persisted session before the UI starts. Validates the
    /// credentials against the backend; returns the account id on success.
    pub fn restore_now(&self) -> Option<String> {
        match self.runtime.block_on(self.validate_persisted_auth()) {
            RestoreOutcome::Valid(user_id) => Some(user_id),
            RestoreOutcome::Rejected | RestoreOutcome::Unavailable => None,
        }
    }

    /// Validate the persisted credentials on the backend runtime while the
    /// UI already shows the account's local data. Calls that need the
    /// session wait for this to finish (see `session_for`).
    pub fn restore_in_background(self: &Arc<Self>) -> tokio::task::JoinHandle<RestoreOutcome> {
        let _ = self.restore_pending.0.send(true);
        let this = self.clone();
        self.runtime.spawn(async move {
            let outcome = this.validate_persisted_auth().await;
            let _ = this.restore_pending.0.send(false);
            outcome
        })
    }

    /// Validate the saved credentials with the server and install the
    /// session on success. Definitive rejections clear the saved file.
    async fn validate_persisted_auth(&self) -> RestoreOutcome {
        let Some(persisted) = self.load_persisted_auth() else {
            return RestoreOutcome::Rejected;
        };
        let request_record = persisted.clone();
        let request = MapleApiAuthRequest {
            user_id: request_record.user_id,
            api_url: self.api_url.clone(),
            access_token: request_record.access_token,
            refresh_token: request_record.refresh_token,
        };
        let result = match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.auth.set_auth(self.auth_sink(), request),
        )
        .await
        {
            Ok(Ok(snapshot)) => Ok(snapshot),
            Ok(Err(error)) => Err(error),
            Err(_) => Err("timeout".to_string()),
        };
        match result {
            Ok(snapshot) => {
                self.persist_auth(&snapshot);
                RestoreOutcome::Valid(snapshot.user_id)
            }
            Err(error) if maple_agent::maple_api::is_auth_rejection(&error) => {
                log::debug!("persisted auth rejected: {error:?}");
                self.persisted_auth.clear_record_if_unchanged(&persisted);
                RestoreOutcome::Rejected
            }
            Err(error) => {
                // Offline, timeout, or a server fault: the credentials may
                // still be good, so keep them for the next launch.
                log::warn!("persisted auth could not be validated: {error}");
                RestoreOutcome::Unavailable
            }
        }
    }

    /// The validated session for `user_id`, waiting first for a background
    /// credential restore that is still in flight. Local reads never call
    /// this; only backend requests that spend the credentials do.
    async fn session_for(
        &self,
        user_id: &str,
    ) -> Result<Arc<maple_agent::maple_api::MapleApiSession>, String> {
        self.wait_for_restore().await;
        self.auth.session_for(user_id).await
    }

    async fn wait_for_restore(&self) {
        let mut pending = self.restore_pending.1.clone();
        while *pending.borrow() {
            if pending.changed().await.is_err() {
                break;
            }
        }
    }

    /// Sign out completely: invalidate the live session first, then remove
    /// only the persisted credentials that this sign-out observed.
    pub async fn logout_and_clear(&self, user_id: &str) -> Result<(), String> {
        self.clear_session(user_id, true).await
    }

    /// Drop the live session and the persisted record. `revoke` also sends
    /// `POST /logout`; a deleted account has no session left to report.
    async fn clear_session(&self, user_id: &str, revoke: bool) -> Result<(), String> {
        self.wait_for_restore().await;
        let auth_snapshot = self.auth.auth_snapshot_for(user_id).await.ok();
        let persisted_without_session = auth_snapshot
            .is_none()
            .then(|| self.load_persisted_auth())
            .flatten();

        // Report the sign-out to the server first, best effort: an offline
        // sign-out must still complete locally, and the session is
        // invalidated below whatever the server said. (The backend's
        // logout route does not revoke the refresh token yet.)
        if revoke
            && auth_snapshot.is_some()
            && let Ok(session) = self.auth.session_for(user_id).await
        {
            match tokio::time::timeout(std::time::Duration::from_secs(5), session.logout()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => log::debug!("server logout failed: {error}"),
                Err(_) => log::debug!("server logout timed out"),
            }
        }

        let result = self.auth.clear_auth(user_id).await;
        if result.is_ok() {
            if let Some(snapshot) = auth_snapshot.as_ref() {
                self.persisted_auth.clear_if_owned(snapshot);
            } else if let Some(persisted) = persisted_without_session.as_ref() {
                // Preserve the old offline/logout behavior without letting a
                // concurrent sign-in's replacement record be removed.
                self.persisted_auth.clear_record_if_unchanged(persisted);
            }
        }
        result
    }

    fn oauth_client(&self) -> Result<OpenSecretClient, String> {
        let environment = configured_pcr0_environment()?;
        OpenSecretClient::new_with_pcr0_environment(self.api_url.clone(), environment)
            .map_err(|_| "Maple API authentication failed".to_string())
    }

    async fn publish_session(
        &self,
        user_id: String,
        access_token: String,
        refresh_token: Option<String>,
    ) -> Result<AuthSession, String> {
        self.auth
            .set_auth(
                self.auth_sink(),
                MapleApiAuthRequest {
                    user_id,
                    api_url: self.api_url.clone(),
                    access_token,
                    refresh_token,
                },
            )
            .await
            .map_err(|message| {
                // Keep validation detail out of the UI; it can echo the
                // configured URL back to the user.
                log::debug!("set_auth failed during sign in: {message}");
                "Sign in failed. Try again.".to_string()
            })
            .map(|snapshot| {
                self.persist_auth(&snapshot);
                AuthSession {
                    user_id: snapshot.user_id.clone(),
                }
            })
    }
    /// Begin an OAuth flow: returns the authorization URL to open in a
    /// browser (also opens it via the system browser).
    pub async fn oauth_start(&self, provider: OAuthProvider) -> Result<String, String> {
        let client = self.oauth_client()?;
        let client_id = self.client_id;
        let (auth_url, state) = match provider {
            OAuthProvider::Github => {
                let response = client
                    .initiate_github_auth(client_id, None)
                    .await
                    .map_err(|_| "Could not start GitHub sign in".to_string())?;
                (response.auth_url, response.state)
            }
            OAuthProvider::Google => {
                let response = client
                    .initiate_google_auth(client_id, None)
                    .await
                    .map_err(|_| "Could not start Google sign in".to_string())?;
                (response.auth_url, response.state)
            }
            OAuthProvider::Apple => {
                let response = client
                    .initiate_apple_auth(client_id, None)
                    .await
                    .map_err(|_| "Could not start Apple sign in".to_string())?;
                (response.auth_url, response.state)
            }
        };
        // The state lives in the redirected URL the user pastes back; the
        // backend re-validates it during the callback exchange.
        let _ = state;
        if webbrowser::open(&auth_url).is_err() {
            // No system browser available: the UI still shows the URL.
            log::debug!("failed to open system browser for OAuth");
        }
        Ok(auth_url)
    }

    /// Complete an OAuth flow from the redirected URL (pasted by the user or
    /// captured from a loopback redirect).
    pub async fn oauth_complete(
        &self,
        provider: OAuthProvider,
        redirected_url: String,
    ) -> Result<AuthSession, String> {
        let Some((code, state)) = parse_oauth_callback(&redirected_url) else {
            return Err(
                "Paste the full URL you were redirected to (it contains code and state)"
                    .to_string(),
            );
        };
        let client = self.oauth_client()?;
        let response = match provider {
            OAuthProvider::Github => client
                .handle_github_callback(code, state, String::new())
                .await
                .map_err(|_| "GitHub sign in failed".to_string())?,
            OAuthProvider::Google => client
                .handle_google_callback(code, state, String::new())
                .await
                .map_err(|_| "Google sign in failed".to_string())?,
            OAuthProvider::Apple => client
                .handle_apple_callback(code, state, String::new())
                .await
                .map_err(|_| "Apple sign in failed".to_string())?,
        };
        self.publish_session(
            response.id.to_string(),
            response.access_token,
            Some(response.refresh_token),
        )
        .await
    }

    /// The signed-in account's profile from the backend.
    pub async fn account(&self, user_id: &str) -> Result<MapleAccount, String> {
        let session = self.session_for(user_id).await?;
        session
            .account()
            .await
            .map_err(|error| account_error_message(error, "Could not load the account"))
    }

    /// Email a fresh verification code to the account's address.
    pub async fn request_verification_email(&self, user_id: &str) -> Result<(), String> {
        let session = self.session_for(user_id).await?;
        session
            .request_verification_email()
            .await
            .map_err(|error| account_error_message(error, "Could not send the verification email"))
    }

    /// Change the account password. The rotated token pair is persisted
    /// through the auth sink before this returns.
    pub async fn change_password(
        &self,
        user_id: &str,
        current_password: String,
        new_password: String,
    ) -> Result<(), String> {
        if current_password.is_empty() {
            return Err("Enter your current password".to_string());
        }
        validate_new_password(&new_password)?;
        let session = self.session_for(user_id).await?;
        session
            .change_password(current_password, new_password)
            .await
            .map_err(|error| match error {
                // The route answers 401 for a wrong current password; a
                // dead session would have failed the session lookup first.
                MapleAccountError::Unauthorized => "The current password is incorrect".to_string(),
                other => account_error_message(other, "Could not change the password"),
            })
    }

    /// Start deleting the account: the server emails a confirmation code.
    /// Returns the client-held secret the confirmation step must present.
    pub async fn request_account_deletion(&self, user_id: &str) -> Result<String, String> {
        let session = self.session_for(user_id).await?;
        let (plaintext, hashed) = maple_agent::maple_api::new_confirmation_secret();
        session
            .request_account_deletion(hashed)
            .await
            .map_err(|error| account_error_message(error, "Could not start account deletion"))?;
        Ok(plaintext)
    }

    /// Delete the account for good. The agent runtime stops first, then
    /// the server deletes the account, then the local credentials go. A
    /// server failure leaves the session usable.
    pub async fn confirm_account_deletion(
        &self,
        user_id: &str,
        confirmation_code: String,
        plaintext_secret: String,
    ) -> Result<(), String> {
        let code = confirmation_code.trim().to_string();
        if code.is_empty() {
            return Err("Enter the confirmation code from the email".to_string());
        }
        let session = self.session_for(user_id).await?;
        self.stop_runtime(user_id).await?;
        session
            .confirm_account_deletion(code, plaintext_secret)
            .await
            .map_err(|error| match error {
                MapleAccountError::Status(400) => {
                    "That confirmation code is wrong or has expired".to_string()
                }
                other => account_error_message(other, "Could not delete the account"),
            })?;
        if let Err(error) = self.clear_session(user_id, false).await {
            log::warn!("local sign-out after account deletion failed: {error}");
        }
        Ok(())
    }

    /// The account's API keys, newest first.
    pub async fn list_api_keys(&self, user_id: &str) -> Result<Vec<MapleApiKey>, String> {
        let session = self.session_for(user_id).await?;
        let mut keys = session
            .list_api_keys()
            .await
            .map_err(|error| api_key_error_message(error, "Could not load the API keys"))?;
        keys.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(keys)
    }

    /// Create an API key named `name`. The key value in the result is the
    /// only copy; the server never returns it again.
    pub async fn create_api_key(
        &self,
        user_id: &str,
        name: String,
    ) -> Result<MapleApiKeyCreated, String> {
        let name = validate_api_key_name(&name)?;
        let session = self.session_for(user_id).await?;
        session
            .create_api_key(name)
            .await
            .map_err(|error| api_key_error_message(error, "Could not create the API key"))
    }

    pub async fn delete_api_key(&self, user_id: &str, name: &str) -> Result<(), String> {
        let session = self.session_for(user_id).await?;
        session
            .delete_api_key(name)
            .await
            .map_err(|error| api_key_error_message(error, "Could not delete the API key"))
    }

    /// Plan usage for the sidebar card from the Maple billing API. Returns
    /// `None` when the subscription has no token meter.
    pub async fn plan_usage(
        &self,
        user_id: &str,
    ) -> Result<Option<crate::billing::PlanUsage>, String> {
        let status = self.billing_status(user_id).await?;
        let plan = crate::billing::PlanUsage::from_status(&status, chrono::Local::now());
        log::debug!("plan usage: {plan:?}");
        Ok(plan)
    }

    /// The full subscription status for the Billing section.
    pub async fn billing_status(
        &self,
        user_id: &str,
    ) -> Result<crate::billing::BillingStatus, String> {
        self.billing_call(user_id, |billing, token| async move {
            billing.subscription_status(&token).await
        })
        .await
    }

    /// The plans on sale. Public on the billing API, no token needed.
    pub async fn billing_products(&self) -> Result<Vec<crate::billing::Product>, String> {
        tokio::time::timeout(std::time::Duration::from_secs(20), self.billing.products())
            .await
            .unwrap_or_else(|_| {
                Err(crate::billing::BillingError::Other(
                    "billing request timed out".to_string(),
                ))
            })
            .map_err(|error| error.to_string())
    }

    /// Open the Stripe customer portal in the system browser.
    pub async fn open_billing_portal(&self, user_id: &str) -> Result<(), String> {
        let url = self
            .billing_call(user_id, |billing, token| async move {
                billing
                    .portal_url(&token, crate::billing::PORTAL_RETURN_URL)
                    .await
            })
            .await
            .map_err(|error| billing_failure(error, "Could not open the subscription portal"))?;
        open_in_browser(&url, "the subscription portal")
    }

    /// Start a Stripe checkout for `product_id` in the system browser. The
    /// account email is sent when there is one; guests check out without.
    pub async fn start_checkout(&self, user_id: &str, product_id: String) -> Result<(), String> {
        let email = self.account(user_id).await?.email.unwrap_or_default();
        let request = Arc::new(crate::billing::CheckoutRequest {
            email,
            product_id,
            success_url: crate::billing::CHECKOUT_SUCCESS_URL.to_string(),
            cancel_url: crate::billing::CHECKOUT_CANCEL_URL.to_string(),
            quantity: None,
        });
        let url = self
            .billing_call(user_id, |billing, token| {
                let request = Arc::clone(&request);
                async move { billing.checkout_url(&token, &request).await }
            })
            .await
            .map_err(|error| billing_failure(error, "Could not start checkout"))?;
        open_in_browser(&url, "checkout")
    }

    /// Run one billing request with the cached token, minting a token
    /// first when there is none and once more after a 401.
    async fn billing_call<T, F, Fut>(&self, user_id: &str, request: F) -> Result<T, String>
    where
        F: Fn(crate::billing::BillingClient, String) -> Fut,
        Fut: Future<Output = Result<T, crate::billing::BillingError>>,
    {
        use crate::billing::BillingError;
        let session = self.session_for(user_id).await?;
        let cached = self.billing_tokens.lock().await.get(user_id).cloned();
        let mut token = match cached {
            Some(token) => token,
            None => self.mint_billing_token(&session, user_id).await?,
        };
        let mut result = self
            .timed_billing(request(self.billing.clone(), token))
            .await;
        if matches!(result, Err(BillingError::Unauthorized)) {
            // The cached token expired or was revoked: mint one and retry once.
            token = self.mint_billing_token(&session, user_id).await?;
            result = self
                .timed_billing(request(self.billing.clone(), token))
                .await;
        }
        match result {
            Ok(value) => Ok(value),
            Err(BillingError::Unauthorized) => {
                self.billing_tokens.lock().await.remove(user_id);
                Err(BillingError::Unauthorized.to_string())
            }
            Err(BillingError::Other(error)) => Err(error),
        }
    }

    async fn timed_billing<T>(
        &self,
        request: impl Future<Output = Result<T, crate::billing::BillingError>>,
    ) -> Result<T, crate::billing::BillingError> {
        tokio::time::timeout(std::time::Duration::from_secs(20), request)
            .await
            .unwrap_or_else(|_| {
                Err(crate::billing::BillingError::Other(
                    "billing request timed out".to_string(),
                ))
            })
    }

    async fn mint_billing_token(
        &self,
        session: &Arc<maple_agent::maple_api::MapleApiSession>,
        user_id: &str,
    ) -> Result<String, String> {
        let token = session
            .third_party_token(self.billing.base_url().to_string())
            .await?;
        self.billing_tokens
            .lock()
            .await
            .insert(user_id.to_string(), token.clone());
        Ok(token)
    }

    pub async fn start_runtime(
        &self,
        user_id: &str,
        request: Option<AgentStartRequest>,
    ) -> Result<AgentRuntimeStatus, String> {
        let handle = self.service.handle_for_user(user_id).await?;
        let session = self.session_for(user_id).await?;
        // The agent falls back to the process working directory when no root
        // is given. That is right for `maple acp`, not for the GUI: pick the
        // saved root or the home directory instead.
        let request = match request {
            Some(AgentStartRequest {
                project_root: None,
                model,
                mode,
            }) => {
                let config = handle.load_config().await?;
                Some(AgentStartRequest {
                    project_root: gui_project_root(&config),
                    model,
                    mode,
                })
            }
            other => other,
        };
        // A wedged enclave connection must surface as an error, not an
        // eternal spinner.
        tokio::time::timeout(
            std::time::Duration::from_secs(60),
            handle.start(session, request),
        )
        .await
        .map_err(|_| "Runtime start timed out. Check your connection and retry.".to_string())?
    }

    pub async fn stop_runtime(&self, user_id: &str) -> Result<AgentRuntimeStatus, String> {
        self.service.handle_for_user(user_id).await?.stop().await
    }

    /// Serve ACP on stdin/stdout for `user_id` until the peer closes stdin.
    /// Starts the runtime first, rooted at the process working directory,
    /// and stops it when the connection ends.
    #[cfg(feature = "acp")]
    pub fn run_acp_stdio(&self, user_id: &str) -> Result<(), String> {
        self.runtime.block_on(async {
            let handle = self.service.handle_for_user(user_id).await?;
            let session = self.session_for(user_id).await?;
            // Start the runtime concurrently instead of before the handshake:
            // `initialize` answers immediately and the first `session/new`
            // awaits this shared start.
            let starting = handle.clone();
            let runtime_start = maple_agent::acp::shared_runtime_start(async move {
                starting.start(session, None).await.map(|_| ())
            });
            let config = maple_agent::acp::load_acp_config(&local_data_root(), user_id)?;
            let result = maple_agent::acp::serve_stdio(handle.clone(), config, runtime_start).await;
            if let Err(error) = handle.stop().await {
                log::warn!("failed to stop the agent runtime after ACP: {error}");
            }
            result
        })
    }

    /// Roots recently used by this account, most recent first.
    pub async fn recent_project_roots(
        &self,
        user_id: &str,
    ) -> Result<Vec<RecentProjectRoot>, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .list_recent_project_roots()
            .await
    }

    /// Register and select the default root for new tasks.
    ///
    /// The account runtime is deliberately not restarted: existing tasks own
    /// their persisted working directories and may keep running under other
    /// roots while the UI moves between projects.
    ///
    /// The root is trusted when no decision is saved yet, as the runtime
    /// does for the root it starts under: this app treats choosing a
    /// directory as the choice. A saved "do not trust" answer stays.
    pub async fn select_project_root(
        &self,
        user_id: &str,
        path: String,
    ) -> Result<AgentProjectRootRegistration, String> {
        let handle = self.service.handle_for_user(user_id).await?;
        let registration = handle.save_recent_project_root(path).await?;
        let root = registration.project_root.clone();
        match handle.get_project_trust(root.clone()).await {
            Ok(status) if status.available && status.decision.is_none() => {
                if let Err(error) = handle.set_project_trust(root, true).await {
                    log::warn!("Cannot trust selected project root: {error}");
                }
            }
            Ok(_) => {}
            Err(error) => log::warn!("Cannot read project trust: {error}"),
        }
        Ok(registration)
    }

    pub async fn list_sessions(
        &self,
        user_id: &str,
        project_root: Option<String>,
    ) -> Result<Vec<AgentSessionSummary>, String> {
        let mut sessions = self
            .service
            .handle_for_user(user_id)
            .await?
            .list_sessions(project_root)
            .await?;
        // Tasks that an ACP client created belong to that client's UI, not
        // to the desktop task list.
        sessions.retain(|session| !session.acp);
        Ok(sessions)
    }

    /// Read everything the chat screen can show without the network: the
    /// saved project root, the task list, the recent roots, and the newest
    /// task's transcript. Call it before `start_runtime`: the runtime start
    /// holds the lifecycle lock across its network round trips, and these
    /// reads would queue behind it.
    pub async fn local_bootstrap(&self, user_id: &str) -> Result<LocalBootstrap, String> {
        let handle = self.service.handle_for_user(user_id).await?;
        let config = handle.load_config().await?;
        let project_root = gui_project_root(&config);
        let mut sessions = handle.list_sessions(None).await?;
        // Tasks that an ACP client created belong to that client's UI, not
        // to the desktop task list.
        sessions.retain(|session| !session.acp);
        let recent_roots = handle
            .list_recent_project_roots()
            .await?
            .into_iter()
            .map(|root| root.path)
            .collect();
        // Same choice refresh_sessions makes: the newest unarchived task
        // under the root that the runtime will start in.
        let latest_id = sessions
            .iter()
            .find(|session| {
                !session.archived && Some(&session.project_root) == project_root.as_ref()
            })
            .map(|session| session.id.clone());
        let latest = match latest_id {
            Some(id) => handle.load_session(id).await.ok(),
            None => None,
        };
        Ok(LocalBootstrap {
            project_root,
            sessions,
            recent_roots,
            latest,
        })
    }

    pub async fn create_session(
        &self,
        user_id: &str,
        request: Option<AgentCreateSessionRequest>,
    ) -> Result<AgentSessionDetail, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .create_session(request)
            .await
    }

    pub async fn load_session(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<AgentSessionDetail, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .load_session(session_id.to_string())
            .await
    }

    pub async fn set_session_archived(
        &self,
        user_id: &str,
        session_id: &str,
        archived: bool,
    ) -> Result<AgentSessionSummary, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .set_session_archived(session_id.to_string(), archived)
            .await
    }

    /// Drop a root from the recent list. `fallback` becomes the runtime
    /// root when the removed one was current.
    pub async fn remove_project_root(
        &self,
        user_id: &str,
        path: String,
        fallback: Option<String>,
    ) -> Result<(), String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .remove_project_root(path, fallback)
            .await
            .map(|_| ())
    }

    pub async fn rename_session(
        &self,
        user_id: &str,
        session_id: &str,
        title: String,
    ) -> Result<AgentSessionSummary, String> {
        let handle = self.service.handle_for_user(user_id).await?;
        let session = self.session_for(user_id).await?;
        handle
            .rename_session(
                session,
                AgentRenameSessionRequest {
                    session_id: session_id.to_string(),
                    title,
                },
            )
            .await
    }

    /// Whether the project at `path` has skills or other guidance that
    /// need a trust decision, and what the saved decision is.
    pub async fn project_trust(
        &self,
        user_id: &str,
        path: String,
    ) -> Result<AgentProjectTrustStatus, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .get_project_trust(path)
            .await
    }

    pub async fn set_project_trust(
        &self,
        user_id: &str,
        path: String,
        trusted: bool,
    ) -> Result<AgentProjectTrustStatus, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .set_project_trust(path, trusted)
            .await
    }

    /// Drop a message that waits behind the active run.
    pub async fn cancel_queued_message(
        &self,
        user_id: &str,
        session_id: &str,
        queue_id: &str,
    ) -> Result<AgentDesktopQueueSnapshot, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .cancel_queued_message(AgentQueueControlRequest {
                session_id: session_id.to_string(),
                queue_id: queue_id.to_string(),
            })
            .await
    }

    /// Hold a queued message while the user edits it: it is not promoted
    /// into the run until the edit ends.
    pub async fn begin_queued_message_edit(
        &self,
        user_id: &str,
        session_id: &str,
        queue_id: &str,
    ) -> Result<(), String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .begin_queued_message_edit(AgentQueueControlRequest {
                session_id: session_id.to_string(),
                queue_id: queue_id.to_string(),
            })
            .await
    }

    /// Release a queued message held by [`Self::begin_queued_message_edit`]
    /// without changing it.
    pub async fn end_queued_message_edit(
        &self,
        user_id: &str,
        session_id: &str,
        queue_id: &str,
    ) -> Result<(), String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .end_queued_message_edit(AgentQueueControlRequest {
                session_id: session_id.to_string(),
                queue_id: queue_id.to_string(),
            })
            .await
    }

    pub async fn send_message(
        &self,
        user_id: &str,
        request: AgentSendMessageRequest,
    ) -> Result<String, String> {
        let run_id = self
            .service
            .handle_for_user(user_id)
            .await?
            .send_message(request)
            .await?
            .run_id;
        Ok(run_id)
    }

    /// Bytes of an image the user attached to a message in `session_id`.
    pub async fn read_image_attachment(
        &self,
        user_id: &str,
        session_id: &str,
        attachment_id: &str,
    ) -> Result<Vec<u8>, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .read_image_attachment(session_id.to_string(), attachment_id.to_string())
            .await
    }

    pub async fn cancel_run(&self, user_id: &str, run_id: &str) -> Result<(), String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .cancel_desktop_run(run_id.to_string())
            .await
    }

    pub async fn available_model_ids(&self, user_id: &str) -> Result<Vec<String>, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .available_model_ids()
            .await
    }

    /// Catalog vision flag for a model; None when unknown.
    pub async fn model_supports_vision(
        &self,
        user_id: &str,
        model: &str,
    ) -> Result<Option<bool>, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .model_supports_vision(model)
            .await
    }

    /// Which voice endpoints the account offers.
    pub async fn audio_capabilities(
        &self,
        user_id: &str,
    ) -> Result<maple_agent::agent::AudioCapabilities, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .audio_capabilities()
            .await
    }

    /// WAV audio for `text` in the given voice.
    pub async fn synthesize_speech(
        &self,
        user_id: &str,
        text: String,
        voice: String,
        speed: f32,
    ) -> Result<Vec<u8>, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .synthesize_speech(&text, &voice, speed)
            .await
    }

    /// Transcript text for a WAV recording.
    pub async fn transcribe_audio(&self, user_id: &str, wav: Vec<u8>) -> Result<String, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .transcribe_audio(wav)
            .await
    }

    /// MCP servers configured for the account, with the session's enabled
    /// state for each.
    pub async fn list_session_mcp_servers(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<Vec<maple_agent::agent::AgentSessionMcpServer>, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .list_session_mcp_servers(session_id.to_string())
            .await
    }

    pub async fn set_session_mcp_server_enabled(
        &self,
        user_id: &str,
        session_id: &str,
        name: &str,
        enabled: bool,
    ) -> Result<Vec<maple_agent::agent::AgentSessionMcpServer>, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .set_session_mcp_server_enabled(maple_agent::agent::AgentSetSessionMcpServerRequest {
                session_id: session_id.to_string(),
                name: name.to_string(),
                enabled,
            })
            .await
    }

    pub async fn list_mcp_servers(
        &self,
        user_id: &str,
    ) -> Result<Vec<maple_agent::agent::AgentMcpServer>, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .list_mcp_servers()
            .await
    }

    pub async fn list_integrations(&self, user_id: &str) -> Result<Vec<AgentIntegration>, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .list_integrations()
            .await
    }

    pub async fn set_integration_enabled(
        &self,
        user_id: &str,
        id: &str,
        enabled: bool,
    ) -> Result<Vec<AgentIntegration>, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .set_integration_enabled(AgentSetIntegrationEnabledRequest {
                id: id.to_string(),
                enabled,
            })
            .await
    }

    /// Start a curated integration's host-owned permission flow from the UI
    /// thread that received the user's setup action.
    pub fn begin_integration_setup(
        &self,
        id: &str,
    ) -> Result<maple_agent::agent::AgentIntegrationPermissions, String> {
        maple_agent::agent::begin_integration_setup(&AgentSetupIntegrationRequest {
            id: id.to_string(),
        })
    }

    /// Persist a curated integration after its host-owned permission flow.
    pub async fn setup_integration(
        &self,
        user_id: &str,
        id: &str,
        permissions: AgentIntegrationPermissions,
    ) -> Result<Vec<AgentIntegration>, String> {
        open_integration_setup_settings(&permissions).await?;
        self.service
            .handle_for_user(user_id)
            .await?
            .setup_integration(AgentSetupIntegrationRequest { id: id.to_string() })
            .await
    }

    pub async fn save_mcp_servers(
        &self,
        user_id: &str,
        servers: Vec<maple_agent::agent::AgentMcpServer>,
    ) -> Result<Vec<maple_agent::agent::AgentMcpServer>, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .save_mcp_servers(servers)
            .await
    }

    /// Turn the web tools on or off for a session (next turn onward).
    pub async fn set_session_web_enabled(
        &self,
        user_id: &str,
        session_id: &str,
        enabled: bool,
    ) -> Result<AgentSessionSummary, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .set_session_web_enabled(maple_agent::agent::AgentSetSessionWebRequest {
                session_id: session_id.to_string(),
                enabled,
            })
            .await
    }

    /// Set the permission policy for a session: "smart_approve" asks for
    /// each gated tool, "auto" approves everything (bypass).
    pub async fn set_permission_mode(
        &self,
        user_id: &str,
        session_id: &str,
        mode: &str,
    ) -> Result<(), String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .set_permission_mode(maple_agent::agent::AgentPermissionModeRequest {
                session_id: session_id.to_string(),
                mode: mode.to_string(),
            })
            .await
    }

    /// Compact a session's history now; reload the session afterwards.
    pub async fn compact_session(&self, user_id: &str, session_id: &str) -> Result<(), String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .compact_session(session_id.to_string())
            .await
    }

    /// Read the current task's reset availability without starting Python.
    pub async fn python_status(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<AgentPythonStatus, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .python_status(session_id.to_string())
            .await
    }

    /// Reset the currently retained task state; success means cleanup completed.
    pub async fn reset_python(&self, user_id: &str, session_id: &str) -> Result<(), String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .reset_python(session_id.to_string())
            .await
    }

    /// The subagents still working for a task. A task whose run ended can
    /// still have a background subagent; this rebuilds the card for it.
    pub async fn session_subagents(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<Vec<AgentSubagent>, String> {
        Ok(self
            .service
            .handle_for_user(user_id)
            .await?
            .session_subagents(session_id)
            .await)
    }

    /// Slash commands (installed skills) for a working directory. Filesystem
    /// scan, so it runs on a blocking thread.
    pub async fn list_slash_commands(
        &self,
        working_dir: Option<String>,
    ) -> Result<Vec<AgentSlashCommand>, String> {
        let service = self.service.clone();
        tokio::task::spawn_blocking(move || service.list_slash_commands(working_dir.as_deref()))
            .await
            .map_err(|error| format!("Slash command scan failed: {error}"))
    }

    /// Expand `/command args` into the skill prompt; `None` when the command
    /// matches no skill.
    pub async fn resolve_slash_command(
        &self,
        working_dir: Option<String>,
        command: String,
        args: String,
    ) -> Result<Option<String>, String> {
        let service = self.service.clone();
        tokio::task::spawn_blocking(move || {
            service.resolve_slash_command(working_dir.as_deref(), &command, &args)
        })
        .await
        .map_err(|error| format!("Slash command resolve failed: {error}"))?
    }

    /// One-line summary of a completed tool call from the cheap title model.
    pub async fn summarize_tool_call(
        &self,
        user_id: &str,
        session_id: &str,
        tool_name: String,
        input: Option<serde_json::Value>,
        output_text: String,
    ) -> Result<Option<String>, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .summarize_tool_call(session_id, &tool_name, input.as_ref(), &output_text)
            .await
    }

    /// One-line summary of a finished thinking block from the cheap title
    /// model.
    pub async fn summarize_thinking(
        &self,
        user_id: &str,
        session_id: &str,
        thinking_text: String,
    ) -> Result<Option<String>, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .summarize_thinking(session_id, &thinking_text)
            .await
    }
    /// Stream the answer to a `/btw` side question; see
    /// `AgentRuntimeHandle::ask_side_question`.
    pub async fn ask_side_question(
        &self,
        user_id: &str,
        session_id: &str,
        request_id: String,
        prior: Vec<maple_agent::agent::SideQuestionTurn>,
        question: String,
    ) -> Result<(), String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .ask_side_question(session_id, request_id, prior, question)
            .await
    }
    /// Run `f` against the summary store of `user_id`. Blocking: call from
    /// `spawn_blocking`.
    fn with_summary_db<T>(
        &self,
        user_id: &str,
        f: impl FnOnce(&rusqlite::Connection) -> Result<T, String>,
    ) -> Result<T, String> {
        let scope = self
            .account_scope(user_id)
            .ok_or_else(|| "No account scope".to_string())?;
        let db = account_summary_db(&scope);
        let mut guard = self.summary_db.lock().unwrap_or_else(|e| e.into_inner());
        if guard.as_ref().map(|(path, _)| path != &db).unwrap_or(true) {
            *guard = Some((db.clone(), open_summary_db(&db)?));
        }
        f(&guard.as_ref().expect("summary db opened above").1)
    }

    /// Stored summaries for one session, keyed by timeline item id.
    /// Blocking.
    pub fn load_tool_summaries_blocking(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<HashMap<String, String>, String> {
        self.with_summary_db(user_id, |conn| {
            let mut stmt = conn
                .prepare("SELECT item_id, summary FROM tool_summaries WHERE session_id = ?1")
                .map_err(|error| error.to_string())?;
            let rows = stmt
                .query_map([session_id], |row| Ok((row.get(0)?, row.get(1)?)))
                .map_err(|error| error.to_string())?;
            rows.collect::<Result<HashMap<_, _>, _>>()
                .map_err(|error| error.to_string())
        })
    }

    /// Persist one summary. Blocking.
    pub fn store_tool_summary_blocking(
        &self,
        user_id: &str,
        session_id: &str,
        item_id: &str,
        summary: &str,
    ) -> Result<(), String> {
        self.with_summary_db(user_id, |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO tool_summaries (session_id, item_id, summary) \
                 VALUES (?1, ?2, ?3)",
                [session_id, item_id, summary],
            )
            .map(|_| ())
            .map_err(|error| error.to_string())
        })
    }

    /// Latest context usage for a session from the goose usage ledger:
    /// (context tokens, context limit). The limit comes from the model
    /// catalog for the selected model; MAPLE_CONTEXT_LIMIT is a manual
    /// override; 200k is the fallback when the catalog lacks the model.
    pub async fn context_usage(
        &self,
        user_id: &str,
        session_id: &str,
        model: Option<&str>,
    ) -> Result<Option<(i64, i64)>, String> {
        let Some(scope) = self.account_scope(user_id) else {
            return Ok(None);
        };
        let limit: i64 = match std::env::var("MAPLE_CONTEXT_LIMIT")
            .ok()
            .and_then(|value| value.parse().ok())
        {
            Some(limit) if limit > 0 => limit,
            _ => match model {
                Some(model) => self
                    .service
                    .handle_for_user(user_id)
                    .await?
                    .context_limit_for_model(model)
                    .await?
                    .unwrap_or(200_000),
                None => 200_000,
            },
        };
        // SQLite is synchronous; keep it off the async workers.
        let db = crate::backend::account_session_db(&scope);
        let usage_db = self.usage_db.clone();
        let session_id = session_id.to_string();
        let tokens = tokio::task::spawn_blocking(move || {
            let mut guard = usage_db.lock().unwrap_or_else(|e| e.into_inner());
            if guard.as_ref().map(|(path, _)| path != &db).unwrap_or(true) {
                let conn = crate::backend::open_session_db_read_only(&db)?;
                *guard = Some((db, conn));
            }
            let conn = &guard.as_ref().expect("usage db opened above").1;
            conn.query_row(
                "SELECT COALESCE(input_tokens,0) + COALESCE(cache_read_tokens,0) \
                 + COALESCE(cache_write_tokens,0) FROM usage_ledger \
                 WHERE session_id = ?1 AND is_compaction = 0 \
                 ORDER BY id DESC LIMIT 1",
                [session_id],
                |row| row.get::<_, i64>(0),
            )
            .ok()
        })
        .await
        .map_err(|error| format!("Context usage query failed: {error}"))?;
        Ok(tokens.map(|tokens| (tokens, limit)))
    }

    /// Deliver the user's answer to an ask_user question. Returns false
    /// when no question was pending.
    pub async fn answer_question(
        &self,
        user_id: &str,
        request_id: &str,
        answer: String,
    ) -> Result<bool, String> {
        let _service = self.service.clone();
        let request_id = request_id.to_string();
        self.service
            .handle_for_user(user_id)
            .await?
            .answer_question_via_handle(&request_id, answer)
            .await
    }

    pub async fn permission_respond(
        &self,
        user_id: &str,
        session_id: &str,
        request_id: &str,
        allow: bool,
    ) -> Result<(), String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .permission_respond(maple_agent::agent::AgentPermissionResponse {
                session_id: session_id.to_string(),
                request_id: request_id.to_string(),
                decision: if allow {
                    "allow_once".to_string()
                } else {
                    "deny_once".to_string()
                },
            })
            .await
    }

    /// Standard start request for this app: the saved project root (see
    /// `start_runtime`) with the configured model and the SmartApprove policy.
    pub fn default_start_request(&self) -> AgentStartRequest {
        AgentStartRequest {
            project_root: None,
            model: std::env::var("MAPLE_MODEL").ok(),
            mode: None,
        }
    }

    /// Persist the UI's model choice as the account's default model.
    pub async fn save_default_model(&self, user_id: &str, model: String) -> Result<(), String> {
        let handle = self.service.handle_for_user(user_id).await?;
        let mut config = handle.load_config().await?;
        config.default_model = model;
        handle.save_config(config).await
    }

    /// Model the UI should select initially: MAPLE_MODEL when set.
    pub fn configured_model(&self) -> Option<String> {
        std::env::var("MAPLE_MODEL").ok()
    }

    /// The account's saved default model, if any.
    pub async fn saved_model(&self, user_id: &str) -> Option<String> {
        let handle = self.service.handle_for_user(user_id).await.ok()?;
        let config = handle.load_config().await.ok()?;
        Some(config.default_model).filter(|model| !model.is_empty())
    }
}

const MACOS_ACCESSIBILITY_SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";
const MACOS_SCREEN_RECORDING_SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";

/// The settings pane that grants the permission the integration still needs.
///
/// Which permission that is comes from `AgentIntegrationPermissions`, so the
/// pane Maple opens and the notice telling the user what to do there cannot
/// disagree about the order.
fn next_integration_setup_settings_url(
    permissions: &AgentIntegrationPermissions,
) -> Option<&'static str> {
    match permissions.first_missing()? {
        AgentIntegrationPermissionKind::Accessibility => Some(MACOS_ACCESSIBILITY_SETTINGS_URL),
        AgentIntegrationPermissionKind::ScreenRecording => {
            Some(MACOS_SCREEN_RECORDING_SETTINGS_URL)
        }
        // The user installs a compositor helper themselves; there is no
        // settings pane that grants it.
        AgentIntegrationPermissionKind::DesktopHelper => None,
    }
}

async fn open_integration_setup_settings(
    permissions: &AgentIntegrationPermissions,
) -> Result<(), String> {
    let Some(url) = next_integration_setup_settings_url(permissions) else {
        return Ok(());
    };
    #[cfg(target_os = "macos")]
    {
        let status =
            tokio::task::spawn_blocking(move || Command::new("/usr/bin/open").arg(url).status())
                .await
                .map_err(|error| format!("Failed to start macOS System Settings: {error}"))?
                .map_err(|error| format!("Failed to open macOS System Settings: {error}"))?;
        if !status.success() {
            return Err(format!(
                "Failed to open macOS System Settings (exit status {:?})",
                status.code()
            ));
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = url;
    Ok(())
}

/// Extract `code` and `state` query parameters from an OAuth redirect URL.
/// Values are form-decoded: Google codes carry `%2F`, and a browser may
/// encode a space in `state` as `+`.
fn parse_oauth_callback(url: &str) -> Option<(String, String)> {
    let query = url.split_once('?')?.1;
    let mut code = None;
    let mut state = None;
    for pair in query.split(['&', '#']) {
        if let Some((key, value)) = pair.split_once('=') {
            match key {
                "code" => code = Some(decode_query_value(value)),
                "state" => state = Some(decode_query_value(value)),
                _ => {}
            }
        }
    }
    Some((code?, state?))
}

/// `application/x-www-form-urlencoded` decoding of one query value.
fn decode_query_value(value: &str) -> String {
    let spaced = value.replace('+', " ");
    percent_encoding::percent_decode_str(&spaced)
        .decode_utf8_lossy()
        .into_owned()
}

/// A user-facing message for a failed billing action; the server detail
/// goes to the log, not the screen.
fn billing_failure(error: String, fallback: &str) -> String {
    log::warn!("{fallback}: {error}");
    format!("{fallback}. Try again, or use the pricing page on the web.")
}

/// Hand a billing URL to the system browser. Only `https` URLs are opened:
/// the billing API is trusted, but a bad deploy must not launch anything
/// else through the browser handler.
pub fn open_in_browser(url: &str, what: &str) -> Result<(), String> {
    if !url.starts_with("https://") {
        log::warn!("refusing to open a non-https {what} URL");
        return Err(format!("Could not open {what}: unexpected link"));
    }
    webbrowser::open(url).map_err(|error| {
        log::warn!("failed to open {what} in the browser: {error}");
        format!("Could not open {what} in your browser")
    })
}

/// The server's API key name rule: 1 to 50 characters after trimming.
pub const MAX_API_KEY_NAME_LENGTH: usize = 50;

pub fn validate_api_key_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Give the key a name".to_string());
    }
    if name.chars().count() > MAX_API_KEY_NAME_LENGTH {
        return Err(format!(
            "Use at most {MAX_API_KEY_NAME_LENGTH} characters for the key name"
        ));
    }
    Ok(name.to_string())
}

/// API key errors by status, matching the web app's wording.
fn api_key_error_message(error: MapleAccountError, fallback: &str) -> String {
    match error {
        MapleAccountError::Unauthorized => "API keys need a Pro, Max, or Team plan".to_string(),
        MapleAccountError::Status(409) => "A key with that name already exists".to_string(),
        MapleAccountError::Status(400) => "That key name is not allowed".to_string(),
        MapleAccountError::Status(404) => "That key no longer exists".to_string(),
        MapleAccountError::Status(429) => "You have reached the API key limit".to_string(),
        other => account_error_message(other, fallback),
    }
}

/// Minimum password length, the same rule as Maple's web forms.
pub const MIN_PASSWORD_LENGTH: usize = 8;

/// The web app's password rule: at least eight characters.
pub fn validate_new_password(password: &str) -> Result<(), String> {
    if password.chars().count() < MIN_PASSWORD_LENGTH {
        return Err(format!(
            "Use at least {MIN_PASSWORD_LENGTH} characters for the new password"
        ));
    }
    Ok(())
}

/// A user-facing message for an account call that failed. Backend detail
/// stays in the log; the status alone picks the wording.
fn account_error_message(error: MapleAccountError, fallback: &str) -> String {
    match error {
        MapleAccountError::Unauthorized => {
            maple_agent::maple_api::AUTH_REJECTED_MESSAGE.to_string()
        }
        MapleAccountError::Status(status) => {
            log::debug!("account request failed with status {status}");
            format!("{fallback}. Try again.")
        }
        MapleAccountError::Other(message) => {
            log::debug!("account request failed: {message}");
            format!("{fallback}. Check your connection and try again.")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth_snapshot(
        user_id: &str,
        session_id: &str,
        revision: u64,
        access_token: &str,
    ) -> MapleApiAuthSnapshot {
        MapleApiAuthSnapshot {
            user_id: user_id.to_string(),
            access_token: access_token.to_string(),
            refresh_token: Some(format!("refresh-{access_token}")),
            native_instance_id: "native-test".to_string(),
            session_id: session_id.to_string(),
            revision,
        }
    }

    #[test]
    fn persisted_auth_compare_clear_preserves_a_new_session_and_rejects_late_rotation() {
        let root = std::env::temp_dir().join(format!(
            "maple-persisted-auth-race-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("auth.json");
        let store = PersistedAuthStore::new(path.clone(), "https://api.example".to_string());
        let old = auth_snapshot("same-user", "session-old", 2, "old-token");
        let new = auth_snapshot("same-user", "session-new", 1, "new-token");

        store.claim_and_persist(&old);
        let mut newer_old = old.clone();
        newer_old.revision = 3;
        newer_old.access_token = "newer-old-token".to_string();
        store.persist_rotation(&newer_old);
        store.claim_and_persist(&old);
        assert_eq!(
            store.read_record().unwrap().access_token,
            "newer-old-token",
            "a late same-session claim must not roll back a newer rotation"
        );

        store.claim_and_persist(&new);
        store.clear_if_owned(&old);
        assert_eq!(
            store.read_record().unwrap().access_token,
            "new-token",
            "a delayed old logout must not erase a new same-account session"
        );

        let mut late_old = old.clone();
        late_old.revision = 3;
        late_old.access_token = "late-old-token".to_string();
        store.persist_rotation(&late_old);
        assert_eq!(store.read_record().unwrap().access_token, "new-token");

        store.clear_if_owned(&new);
        assert!(!path.exists());
        let mut late_new = new;
        late_new.revision = 2;
        late_new.access_token = "late-new-token".to_string();
        store.persist_rotation(&late_new);
        assert!(
            !path.exists(),
            "a rotation published after sign-out must not recreate auth.json"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejected_restore_clears_only_the_record_that_was_loaded() {
        let root = std::env::temp_dir().join(format!(
            "maple-persisted-auth-restore-race-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("auth.json");
        let store = PersistedAuthStore::new(path.clone(), "https://api.example".to_string());
        let old = auth_snapshot("old-user", "session-old", 1, "old-token");
        let new = auth_snapshot("new-user", "session-new", 1, "new-token");

        store.claim_and_persist(&old);
        let loaded = store.load().unwrap();
        store.claim_and_persist(&new);
        store.clear_record_if_unchanged(&loaded);
        assert_eq!(store.read_record().unwrap().user_id, "new-user");

        store.clear_if_owned(&new);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn oauth_callback_values_are_form_decoded() {
        let url = "http://localhost/cb?state=ab%20cd+ef&code=4%2F0AX4XfWh%3Dz&x=1#frag";
        assert_eq!(
            parse_oauth_callback(url),
            Some(("4/0AX4XfWh=z".to_string(), "ab cd ef".to_string()))
        );
        assert_eq!(
            parse_oauth_callback("http://localhost/cb?code=plain&state=s"),
            Some(("plain".to_string(), "s".to_string()))
        );
        assert_eq!(parse_oauth_callback("http://localhost/cb?code=only"), None);
        assert_eq!(parse_oauth_callback("http://localhost/cb"), None);
    }

    #[test]
    fn malformed_client_id_falls_back_to_default() {
        let default: Uuid = DEFAULT_CLIENT_ID.parse().unwrap();
        assert_eq!(client_id_from(None), default);
        assert_eq!(client_id_from(Some("not-a-uuid")), default);
        let custom = "123e4567-e89b-12d3-a456-426614174000";
        assert_eq!(
            client_id_from(Some(custom)),
            custom.parse::<Uuid>().unwrap()
        );
    }

    fn macos_permissions(
        accessibility: bool,
        screen_recording: bool,
    ) -> AgentIntegrationPermissions {
        AgentIntegrationPermissions::default()
            .with(AgentIntegrationPermissionKind::Accessibility, accessibility)
            .with(
                AgentIntegrationPermissionKind::ScreenRecording,
                screen_recording,
            )
    }

    #[test]
    fn integration_setup_opens_one_missing_permission_at_a_time() {
        assert_eq!(
            next_integration_setup_settings_url(&macos_permissions(false, false)),
            Some(MACOS_ACCESSIBILITY_SETTINGS_URL)
        );
        assert_eq!(
            next_integration_setup_settings_url(&macos_permissions(true, false)),
            Some(MACOS_SCREEN_RECORDING_SETTINGS_URL)
        );
        assert_eq!(
            next_integration_setup_settings_url(&macos_permissions(true, true)),
            None
        );
    }
}
