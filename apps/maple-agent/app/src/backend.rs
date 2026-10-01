//! Account-level backend for the gpui frontend.
//!
//! This owns the private Tokio runtime, the OpenSecret sign-in, billing,
//! audio, and the in-process agent service. Everything a client drives on
//! a host (tasks, projects, runs, integrations) goes through
//! [`maple_agent::host::HostBackend`] instead; [`AgentBackend::local_host`]
//! hands out the in-process implementation. A remote host implements the
//! same trait over the wire, so the UI never branches on where a host runs.

// This module is the desktop frontend's boundary. A headless build (no
// `desktop` feature) uses only a few entry points, so the rest is unused
// there by design.
#![cfg_attr(not(feature = "desktop"), allow(dead_code))]

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::Command;
use std::sync::Arc;

use maple_agent::agent::{
    AgentIntegrationPermissionKind, AgentIntegrationPermissions, AgentSetupIntegrationRequest,
    AgentStartRequest, MapleAgentHostResources, MapleAgentService,
};
use maple_agent::host::{HostEvent, HostEventHub, LocalHostAuth, LocalHostBackend};
use maple_agent::maple_api::{
    MapleApiAuthEventSink, MapleApiAuthRequest, MapleApiAuthSnapshot, MapleApiAuthState,
    MapleApiSession,
};
use maple_agent::open_secret_config::configured_pcr0_environment;
use maple_sdk::OpenSecretClient;
use tokio::runtime::Runtime;
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
    /// Pretty-printed tool arguments, formatted once when the request
    /// arrives instead of on every frame.
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

pub struct AgentBackend {
    runtime: Runtime,
    service: MapleAgentService,
    auth: MapleApiAuthState,
    api_url: String,
    persisted_auth: Arc<PersistedAuthStore>,
    pending_oauth: PendingOAuthStore,
    client_id: Uuid,
    /// The local host's event stream. The runtime emits into it; every
    /// subscriber receives every event.
    events: Arc<HostEventHub>,
    /// One in-process host per signed-in account, created on first use.
    local_hosts: std::sync::Mutex<HashMap<String, Arc<LocalHostBackend>>>,
    billing: crate::billing::BillingClient,
    /// Cached billing JWT per user id. Replaced after a 401.
    billing_tokens: tokio::sync::Mutex<HashMap<String, String>>,
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

pub(crate) const APP_DIR_NAME: &str = "maple-agent";
/// Directory name used before the package rename. An existing directory is
/// adopted in place on first start; see [`adopt_legacy_app_dirs`].
const LEGACY_APP_DIR_NAME: &str = "maple-gpui";

/// Rename state written under the previous directory name into the current
/// one, once per distinct root, before anything opens files. Only a missing
/// current directory is filled: an already-current or partially migrated
/// layout is never touched. Returns one line per root for the caller to log
/// once logging is up.
pub fn adopt_legacy_app_dirs() -> Vec<String> {
    let mut notes = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    for root in [config_root(), local_data_root()] {
        if seen.contains(&root) {
            continue;
        }
        if let Some(note) = adopt_legacy_app_dir(&root) {
            notes.push(note);
        }
        seen.push(root);
    }
    notes
}

fn adopt_legacy_app_dir(root: &Path) -> Option<String> {
    let legacy = root.parent()?.join(LEGACY_APP_DIR_NAME);
    if root.exists() || !legacy.is_dir() {
        return None;
    }
    Some(match std::fs::rename(&legacy, root) {
        Ok(()) => format!("adopted {} as {}", legacy.display(), root.display()),
        Err(error) => format!(
            "could not adopt {} as {}: {error}",
            legacy.display(),
            root.display()
        ),
    })
}

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

const OAUTH_CANCELLED_MESSAGE: &str = "Sign in was cancelled. Start again.";

/// V2 binds the provider callback to the SDK session that started it. Keep
/// that exact client until the one permitted completion, and revoke work
/// already in flight when the user cancels or starts another sign-in.
struct OAuthAttempt {
    provider: OAuthProvider,
    client: Arc<OpenSecretClient>,
    cancelled: tokio::sync::watch::Sender<bool>,
}

impl OAuthAttempt {
    async fn while_active<T>(
        &self,
        operation: impl Future<Output = Result<T, String>>,
    ) -> Result<T, String> {
        let mut cancelled = self.cancelled.subscribe();
        if *cancelled.borrow() {
            return Err(OAUTH_CANCELLED_MESSAGE.to_string());
        }
        tokio::select! {
            biased;
            _ = cancelled.changed() => Err(OAUTH_CANCELLED_MESSAGE.to_string()),
            result = operation => result,
        }
    }
}

struct PendingOAuth {
    attempt: Arc<OAuthAttempt>,
    state: Option<String>,
    completing: bool,
}

#[derive(Default)]
struct PendingOAuthStore(std::sync::Mutex<Option<PendingOAuth>>);

impl PendingOAuthStore {
    fn begin(&self, provider: OAuthProvider, client: OpenSecretClient) -> Arc<OAuthAttempt> {
        let attempt = Arc::new(OAuthAttempt {
            provider,
            client: Arc::new(client),
            cancelled: tokio::sync::watch::channel(false).0,
        });
        let mut pending = self.0.lock().expect("pending OAuth lock");
        if let Some(previous) = pending.replace(PendingOAuth {
            attempt: Arc::clone(&attempt),
            state: None,
            completing: false,
        }) {
            previous.attempt.cancelled.send_replace(true);
        }
        attempt
    }

    fn set_state(&self, attempt: &Arc<OAuthAttempt>, state: String) -> Result<(), String> {
        let mut pending = self.0.lock().expect("pending OAuth lock");
        let flow = pending
            .as_mut()
            .filter(|flow| Arc::ptr_eq(&flow.attempt, attempt))
            .ok_or_else(|| OAUTH_CANCELLED_MESSAGE.to_string())?;
        flow.state = Some(state);
        Ok(())
    }

    fn complete(&self, provider: OAuthProvider, state: &str) -> Result<Arc<OAuthAttempt>, String> {
        let mut pending = self.0.lock().expect("pending OAuth lock");
        let flow = pending
            .as_mut()
            .filter(|flow| {
                flow.attempt.provider == provider
                    && flow.state.as_deref() == Some(state)
                    && !flow.completing
            })
            .ok_or_else(|| {
                "This callback does not match the pending sign in. Start again.".to_string()
            })?;
        flow.completing = true;
        Ok(Arc::clone(&flow.attempt))
    }

    fn clear(&self, attempt: &Arc<OAuthAttempt>) {
        let mut pending = self.0.lock().expect("pending OAuth lock");
        if pending
            .as_ref()
            .is_some_and(|flow| Arc::ptr_eq(&flow.attempt, attempt))
            && let Some(flow) = pending.take()
        {
            flow.attempt.cancelled.send_replace(true);
        }
    }

    fn cancel(&self) {
        if let Some(flow) = self.0.lock().expect("pending OAuth lock").take() {
            flow.attempt.cancelled.send_replace(true);
        }
    }
}

/// A dropped initiation/completion must not leave a usable pending flow.
/// Clearing is scoped to this attempt so late cleanup preserves a replacement.
struct OAuthAttemptGuard<'a> {
    store: &'a PendingOAuthStore,
    attempt: &'a Arc<OAuthAttempt>,
    retain: bool,
}

impl Drop for OAuthAttemptGuard<'_> {
    fn drop(&mut self) {
        if !self.retain {
            self.store.clear(self.attempt);
        }
    }
}

/// App configuration root (XDG-style), also used by the settings store.
pub fn app_config_root() -> PathBuf {
    config_root()
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

/// The agent runtime's directory layout under this app's roots. Other
/// modules that keep per-account files beside the runtime's take the
/// account directory from here instead of rebuilding the layout.
pub fn agent_paths() -> maple_agent::agent::AgentPathLayout {
    maple_agent::agent::AgentPathLayout::from_app_roots(config_root(), local_data_root())
}

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
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

/// The local host's view of this backend's sign-in. Weak so the backend,
/// which owns the hosts, does not own itself through them.
struct BackendHostAuth(std::sync::Weak<AgentBackend>);

#[async_trait::async_trait]
impl LocalHostAuth for BackendHostAuth {
    async fn api_session(&self, user_id: &str) -> Result<Arc<MapleApiSession>, String> {
        let backend = self
            .0
            .upgrade()
            .ok_or_else(|| "The app is shutting down".to_string())?;
        backend.session_for(user_id).await
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
    pub fn new(api_url: String) -> Result<Self, String> {
        // Enforce the credential-bearing URL policy before any client is
        // built, including the login-time SDK client.
        let api_url = maple_agent::maple_api::validate_api_url(&api_url)?;
        let events = Arc::new(HostEventHub::default());
        let paths = agent_paths();
        // Keeps ACP bridge credentials out of desktop tool environments.
        let default_tool_context = maple_agent::agent::default_tool_context_spec()?;
        // The harness instructions are per account and reach the runtime
        // through the local host once an account is bound.
        let service = MapleAgentService::new(MapleAgentHostResources::new(
            paths,
            Arc::clone(&events) as Arc<dyn maple_agent::agent::AgentEventSink>,
            default_tool_context,
            maple_agent::host::DEFAULT_HARNESS_INSTRUCTIONS.to_string(),
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
            pending_oauth: PendingOAuthStore::default(),
            client_id: configured_client_id(),
            events,
            local_hosts: std::sync::Mutex::new(HashMap::new()),
            billing,
            billing_tokens: tokio::sync::Mutex::new(HashMap::new()),
            restore_pending: tokio::sync::watch::channel(false),
        })
    }

    /// A fresh subscription to the local host's events. The desktop event
    /// pump takes one for the whole process.
    pub fn subscribe_events(&self) -> tokio::sync::mpsc::UnboundedReceiver<HostEvent> {
        self.events.subscribe()
    }

    /// The in-process host for `user_id`, created once per account. It
    /// shares this backend's runtime and event stream.
    pub fn local_host(self: &Arc<Self>, user_id: &str) -> Arc<LocalHostBackend> {
        let mut hosts = self
            .local_hosts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(host) = hosts.get(user_id) {
            return Arc::clone(host);
        }
        let host = LocalHostBackend::new(
            self.service.clone(),
            user_id.to_string(),
            Arc::new(BackendHostAuth(Arc::downgrade(self))),
            Arc::clone(&self.events),
        );
        hosts.insert(user_id.to_string(), Arc::clone(&host));
        host
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
        self.cancel_oauth();
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
        match self.restore_outcome_now() {
            RestoreOutcome::Valid(user_id) => Some(user_id),
            RestoreOutcome::Rejected | RestoreOutcome::Unavailable => None,
        }
    }

    /// [`Self::restore_now`] with the full outcome, for a command that
    /// treats an unreachable server differently from a rejected sign-in.
    /// `Rejected` also covers a missing sign-in; check
    /// [`Self::saved_user_id`] first to tell them apart.
    pub fn restore_outcome_now(&self) -> RestoreOutcome {
        self.runtime.block_on(self.validate_persisted_auth())
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
        self.cancel_oauth();
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
    async fn session_for(&self, user_id: &str) -> Result<Arc<MapleApiSession>, String> {
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
        self.cancel_oauth();
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
        let attempt = self.pending_oauth.begin(provider, self.oauth_client()?);
        let mut guard = OAuthAttemptGuard {
            store: &self.pending_oauth,
            attempt: &attempt,
            retain: false,
        };
        let client = &attempt.client;
        let client_id = self.client_id;
        let (auth_url, state) = attempt
            .while_active(async {
                Ok(match provider {
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
                })
            })
            .await?;
        self.pending_oauth.set_state(&attempt, state)?;
        if webbrowser::open(&auth_url).is_err() {
            // No system browser available: the UI still shows the URL.
            log::debug!("failed to open system browser for OAuth");
        }
        guard.retain = true;
        Ok(auth_url)
    }

    pub fn cancel_oauth(&self) {
        self.pending_oauth.cancel();
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
        let attempt = self.pending_oauth.complete(provider, &state)?;
        let _guard = OAuthAttemptGuard {
            store: &self.pending_oauth,
            attempt: &attempt,
            retain: false,
        };
        attempt
            .while_active(async {
                let client = &attempt.client;
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
            })
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
        self.service.handle_for_user(user_id).await?.stop().await?;
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
        session: &Arc<MapleApiSession>,
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

    /// Serve ACP on stdin/stdout for `user_id` until the peer closes stdin.
    /// Starts the runtime first, rooted at the process working directory,
    /// and stops it when the connection ends.
    #[cfg(feature = "acp")]
    pub fn run_acp_stdio(self: &Arc<Self>, user_id: &str) -> Result<(), String> {
        let host = self.local_host(user_id);
        self.runtime.block_on(async {
            host.apply_saved_harness().await?;
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

    /// Open the settings pane that grants the permission a curated
    /// integration still needs, after [`Self::begin_integration_setup`]
    /// ran. A local capability: it opens a pane on this machine's screen,
    /// so it applies to the local host only. Persist the integration with
    /// `HostBackend::setup_integration` afterwards.
    pub async fn open_integration_setup_settings(
        &self,
        permissions: &AgentIntegrationPermissions,
    ) -> Result<(), String> {
        open_integration_setup_settings(permissions).await
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

    /// Standard start request for this app: the saved project root (see
    /// `start_runtime`) with the configured model and the SmartApprove policy.
    pub fn default_start_request(&self) -> AgentStartRequest {
        AgentStartRequest {
            project_root: None,
            model: std::env::var("MAPLE_MODEL").ok(),
            mode: None,
        }
    }

    /// Model the UI should select initially: MAPLE_MODEL when set.
    pub fn configured_model(&self) -> Option<String> {
        std::env::var("MAPLE_MODEL").ok()
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

    fn oauth_test_client() -> OpenSecretClient {
        // Construct the SDK without making any network request.
        OpenSecretClient::new("http://127.0.0.1:1").unwrap()
    }

    #[tokio::test]
    async fn oauth_completion_keeps_the_originating_client_and_consumes_only_its_callback() {
        let store = PendingOAuthStore::default();
        let started = store.begin(OAuthProvider::Github, oauth_test_client());
        store
            .set_state(&started, "fixture-state".to_string())
            .unwrap();

        assert!(
            store
                .complete(OAuthProvider::Google, "fixture-state")
                .is_err()
        );
        assert!(
            store
                .complete(OAuthProvider::Github, "other-state")
                .is_err()
        );
        let completed = store
            .complete(OAuthProvider::Github, "fixture-state")
            .unwrap();
        assert!(Arc::ptr_eq(&started.client, &completed.client));
        assert!(
            store
                .complete(OAuthProvider::Github, "fixture-state")
                .is_err()
        );
        store.clear(&completed);
        assert!(
            store
                .complete(OAuthProvider::Github, "fixture-state")
                .is_err()
        );
    }

    #[tokio::test]
    async fn oauth_replacement_rejects_late_start_and_preserves_new_attempt_during_old_cleanup() {
        let store = PendingOAuthStore::default();
        let old = store.begin(OAuthProvider::Github, oauth_test_client());
        let old_guard = OAuthAttemptGuard {
            store: &store,
            attempt: &old,
            retain: false,
        };
        let current = store.begin(OAuthProvider::Google, oauth_test_client());
        assert!(store.set_state(&old, "old-state".to_string()).is_err());
        drop(old_guard);
        store
            .set_state(&current, "current-state".to_string())
            .unwrap();
        let completed = store
            .complete(OAuthProvider::Google, "current-state")
            .unwrap();
        assert!(Arc::ptr_eq(&completed.client, &current.client));

        assert!(
            old.while_active::<()>(async { panic!("cancelled work must not run") })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn oauth_cancellation_discards_an_inflight_completion_before_publication() {
        let store = Arc::new(PendingOAuthStore::default());
        let attempt = store.begin(OAuthProvider::Apple, oauth_test_client());
        store
            .set_state(&attempt, "fixture-state".to_string())
            .unwrap();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let published = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let task_store = Arc::clone(&store);
        let task_published = Arc::clone(&published);
        let task = tokio::spawn(async move {
            let attempt = task_store
                .complete(OAuthProvider::Apple, "fixture-state")
                .unwrap();
            let _guard = OAuthAttemptGuard {
                store: &task_store,
                attempt: &attempt,
                retain: false,
            };
            attempt
                .while_active(async {
                    entered_tx.send(()).unwrap();
                    release_rx.await.unwrap();
                    task_published.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                })
                .await
        });
        entered_rx.await.unwrap();
        store.cancel();
        let _ = release_tx.send(());
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result, Err(OAUTH_CANCELLED_MESSAGE.to_string()));
        assert!(!published.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            store
                .complete(OAuthProvider::Apple, "fixture-state")
                .is_err()
        );
    }

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

#[cfg(test)]
mod app_dir_tests {
    use super::{APP_DIR_NAME, LEGACY_APP_DIR_NAME, adopt_legacy_app_dir};

    #[test]
    fn legacy_app_dir_is_adopted_only_when_the_current_one_is_absent() {
        let base = std::env::temp_dir().join(format!(
            "maple-agent-adopt-{}-{}",
            std::process::id(),
            std::time::SystemTime::UNIX_EPOCH
                .elapsed()
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        let legacy = base.join(LEGACY_APP_DIR_NAME);
        let root = base.join(APP_DIR_NAME);
        std::fs::create_dir_all(legacy.join("logs")).unwrap();
        std::fs::write(legacy.join("auth.json"), "saved").unwrap();

        let note = adopt_legacy_app_dir(&root).expect("first start adopts the legacy dir");
        assert!(note.starts_with("adopted "), "{note}");
        assert_eq!(
            std::fs::read_to_string(root.join("auth.json")).unwrap(),
            "saved"
        );
        assert!(root.join("logs").is_dir());
        assert!(!legacy.exists());

        // Nothing to do once the current directory exists.
        assert_eq!(adopt_legacy_app_dir(&root), None);

        // A legacy directory that reappears next to a current one is left alone.
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("auth.json"), "stale").unwrap();
        assert_eq!(adopt_legacy_app_dir(&root), None);
        assert_eq!(
            std::fs::read_to_string(root.join("auth.json")).unwrap(),
            "saved"
        );
        assert!(legacy.join("auth.json").is_file());

        // No legacy directory: nothing happens and nothing is created.
        let fresh = base.join("fresh").join(APP_DIR_NAME);
        std::fs::create_dir_all(fresh.parent().unwrap()).unwrap();
        assert_eq!(adopt_legacy_app_dir(&fresh), None);
        assert!(!fresh.exists());

        std::fs::remove_dir_all(&base).unwrap();
    }
}
