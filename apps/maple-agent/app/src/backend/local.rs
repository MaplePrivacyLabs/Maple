//! Account, persistence, and runtime operations owned by the local host.

// This module is the desktop frontend's boundary. A headless build (no
// `desktop` feature) uses only a few entry points, so the rest is unused
// there by design.
#![cfg_attr(not(feature = "desktop"), allow(dead_code))]

use std::collections::HashMap;
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::process::Command;
use std::sync::Arc;

use maple_agent::agent::{
    AgentCreateSessionRequest, AgentDesktopQueueSnapshot, AgentEventSink, AgentIntegration,
    AgentIntegrationPermissionKind, AgentIntegrationPermissions, AgentProjectTrustStatus,
    AgentQueueControlRequest, AgentRenameSessionRequest, AgentRuntimeStatus,
    AgentSendMessageRequest, AgentServiceEvent, AgentSessionDetail, AgentSessionSummary,
    AgentSetIntegrationEnabledRequest, AgentSetupIntegrationRequest, AgentSlashCommand,
    AgentStartRequest, AgentSubagent, AgentTaskState, MapleAgentHostResources, MapleAgentService,
    RecentProjectRoot,
};
use maple_agent::maple_api::{
    MapleApiAuthEventSink, MapleApiAuthRequest, MapleApiAuthSnapshot, MapleApiAuthState,
};
use maple_agent::open_secret_config::configured_pcr0_environment;
use maple_sdk::OpenSecretClient;
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::hosted_oauth::{ATTEMPT_TIMEOUT, HandoffListener};

#[path = "deletion.rs"]
mod deletion;
pub use deletion::AccountDeletionStatus;

#[path = "project_folders.rs"]
mod project_folders;
#[allow(unused_imports)]
pub use project_folders::{ProjectFolderEntry, ProjectFolderPage, ProjectFolderRequest};

#[derive(Debug, Clone)]
pub struct PendingQuestion {
    pub session_id: String,
    pub request_id: String,
    /// One or more related questions answered together in one card.
    pub questions: Vec<maple_agent::agent::AgentQuestion>,
}

// Re-exported for the settings screens; a headless build has no reader.
#[cfg_attr(not(feature = "desktop"), allow(unused_imports))]
pub use maple_agent::maple_api::{
    MapleAccount, MapleAccountError, MapleApiKey, MapleApiKeyCreated, MapleLoginMethod,
};

/// The signed-in account identity.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AuthSession {
    pub user_id: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectedProject {
    pub project_root: String,
    pub project: RecentProjectRoot,
}

/// Result of the background credential validation started by
/// the frontend at startup.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
#[derive(serde::Serialize, serde::Deserialize)]
pub struct LocalBootstrap {
    pub project_root: Option<String>,
    pub sessions: Vec<AgentSessionSummary>,
    pub recent_roots: Vec<RecentProjectRoot>,
    pub latest: Option<AgentSessionDetail>,
    pub questions: Vec<AgentServiceEvent>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct RecoverySnapshot {
    pub bootstrap: LocalBootstrap,
    pub status: AgentRuntimeStatus,
    pub selected: Option<AgentSessionDetail>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct DictionaryPage {
    pub words: Vec<String>,
    pub next: Option<String>,
}

pub struct LocalBackend {
    store: Option<Arc<maple_agent::store::Store>>,
    pub(crate) remote_updates: Arc<crate::remote_updates::Broker>,
    row_reader: Option<Arc<maple_agent::store::RowReader>>,
    #[cfg(test)]
    pub(super) ui_fixture: bool,
    pub(crate) account_gate: Arc<tokio::sync::RwLock<()>>,
    restore_lock: tokio::sync::Mutex<()>,
    ui_settings_gate: tokio::sync::Mutex<()>,
    service: MapleAgentService,
    auth: MapleApiAuthState,
    api_url: String,
    persisted_auth: Arc<PersistedAuthStore>,
    pending_oauth: Arc<PendingOAuthStore>,
    client_id: Uuid,
    event_rx: tokio::sync::Mutex<Option<mpsc::UnboundedReceiver<AgentServiceEvent>>>,
    billing: crate::billing::BillingClient,
    /// Cached billing JWT per user id. Replaced after a 401.
    billing_tokens: tokio::sync::Mutex<HashMap<String, String>>,
}

pub(crate) fn configured_client_id() -> Uuid {
    client_id_from(Some(&crate::profile::runtime_value(
        "MAPLE_CLIENT_ID",
        crate::profile::CLIENT_ID,
    )))
}

/// The client id to send: local `MAPLE_CLIENT_ID` when it is a UUID, else the
/// compiled public project id. A malformed local override is logged instead of silently
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

pub(crate) const APP_DIR_NAME: &str = crate::profile::DATA_NAMESPACE;
/// Maple's public OpenSecret project id. The backend rejects unknown
/// client ids, so this must match the registered project.
const DEFAULT_CLIENT_ID: &str = crate::profile::CLIENT_ID;

/// OAuth providers supported by the OpenSecret backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    handoff: Option<HostedHandoff>,
}

struct HostedHandoff {
    listener: HandoffListener,
    prepared: maple_sdk::PreparedNativeOAuthHandoff,
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
            handoff: None,
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
                    && flow.handoff.is_none()
            })
            .ok_or_else(|| {
                "This callback does not match the pending sign in. Start again.".to_string()
            })?;
        flow.completing = true;
        Ok(Arc::clone(&flow.attempt))
    }

    fn set_handoff(
        self: &Arc<Self>,
        attempt: &Arc<OAuthAttempt>,
        handoff: HostedHandoff,
    ) -> Result<(), String> {
        let mut pending = self.0.lock().expect("pending OAuth lock");
        let flow = pending
            .as_mut()
            .filter(|flow| Arc::ptr_eq(&flow.attempt, attempt))
            .ok_or_else(|| OAUTH_CANCELLED_MESSAGE.to_string())?;
        flow.handoff = Some(handoff);
        drop(pending);
        self.expire_handoff_after(attempt, ATTEMPT_TIMEOUT);
        Ok(())
    }

    fn expire_handoff_after(
        self: &Arc<Self>,
        attempt: &Arc<OAuthAttempt>,
        timeout: std::time::Duration,
    ) {
        // Expire even if the UI is closed or never begins waiting. Weak
        // references cannot keep a backend or completed attempt alive.
        let store = Arc::downgrade(self);
        let weak_attempt = Arc::downgrade(attempt);
        let mut cancelled = attempt.cancelled.subscribe();
        tokio::spawn(async move {
            tokio::select! {
                _ = cancelled.changed() => {},
                _ = tokio::time::sleep(timeout) => {
                    if let (Some(store), Some(attempt)) = (store.upgrade(), weak_attempt.upgrade()) {
                        store.clear(&attempt);
                    }
                }
            }
        });
    }

    fn take_handoff(
        &self,
        provider: OAuthProvider,
    ) -> Result<(Arc<OAuthAttempt>, HostedHandoff), String> {
        let mut pending = self.0.lock().expect("pending OAuth lock");
        let flow = pending
            .as_mut()
            .filter(|flow| flow.attempt.provider == provider && !flow.completing)
            .ok_or_else(|| OAUTH_CANCELLED_MESSAGE.to_string())?;
        let handoff = flow
            .handoff
            .take()
            .ok_or_else(|| OAUTH_CANCELLED_MESSAGE.to_string())?;
        flow.completing = true;
        Ok((Arc::clone(&flow.attempt), handoff))
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

impl LocalBackend {
    #[cfg(test)]
    pub(crate) fn isolated_for_test(root: &std::path::Path) -> Self {
        let api_url = "http://127.0.0.1:1".to_string();
        let store = Arc::new(
            maple_agent::store::Store::open(maple_agent::store::StorePaths::new(root.join("data")))
                .unwrap(),
        );
        let mut backend = Self::new_with_store(api_url.clone(), String::new(), store).unwrap();
        let (events, receiver) = mpsc::unbounded_channel();
        backend.service = MapleAgentService::new(
            MapleAgentHostResources::new(
                maple_agent::agent::AgentPathLayout::from_resource_root(
                    backend
                        .store
                        .as_ref()
                        .expect("test store")
                        .paths()
                        .resources(),
                ),
                Arc::new(ChannelEventSink(events)),
                String::new(),
            )
            .with_store(backend.store.as_ref().expect("test store").clone()),
        );
        backend.service.manage_question_availability();
        backend.event_rx = tokio::sync::Mutex::new(Some(receiver));
        backend.persisted_auth = Arc::new(PersistedAuthStore::new(backend.store.clone(), api_url));
        backend
    }

    /// The account scope (sha of the user id) used for on-disk layout.
    pub fn account_scope(&self, user_id: &str) -> Option<String> {
        maple_agent::maple_api::account_scope(user_id).ok()
    }
}

/// Only generated Markdown resources and skills live outside SQL.
pub fn generated_resource_root() -> PathBuf {
    store_paths().resources()
}

/// Root for configuration that may roam between machines. Mirrors Tauri's
/// `app_config_dir`: `~/.config` on Linux, `~/Library/Application Support`
/// on macOS, `%APPDATA%` on Windows. `XDG_CONFIG_HOME` overrides it on
/// every platform so tests and portable installs can redirect it.
pub fn instruction_config_root() -> PathBuf {
    let base = env_dir("XDG_CONFIG_HOME")
        .or_else(dirs::config_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(APP_DIR_NAME)
}

/// The selected profile's parent namespace. Runtime app state opens only
/// the distinct store-v1 child; this resolver performs no filesystem I/O.
pub fn profile_data_root() -> PathBuf {
    let base = env_dir("XDG_DATA_HOME")
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(APP_DIR_NAME)
}

/// New implementation paths. Previous app directories are never inspected.
pub fn store_paths() -> maple_agent::store::StorePaths {
    maple_agent::store::StorePaths::new(profile_data_root())
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

/// Root the desktop app opens when the account has no saved root and the
/// Documents/Maple workspace cannot be created. The GUI must not depend on
/// the directory it was launched from: that is the job of the `maple acp`
/// command, not a windowed app started from a launcher.
fn fallback_project_root() -> Option<String> {
    home_dir().map(|path| path.to_string_lossy().to_string())
}

/// Root for a GUI start: the saved folder when it still exists and was not
/// removed, otherwise Documents/Maple unless the user removed it. Home is
/// only the last resort. Never the process working directory. The
/// workspace may not exist yet: `gui_start_root` creates it.
pub(super) fn gui_project_root(config: &maple_agent::agent::AgentConfig) -> Option<String> {
    let workspace = maple_agent::agent::default_maple_workspace_path();
    maple_agent::agent::startup_project_root(config, workspace.as_deref())
        .or_else(fallback_project_root)
}

/// `gui_project_root` for a runtime start, which needs the folder on disk.
/// Documents/Maple is created here, the first time a start uses it. If
/// that fails (Documents access denied, for example), the start uses home.
fn gui_start_root(config: &maple_agent::agent::AgentConfig) -> Option<String> {
    let root = gui_project_root(config)?;
    if !maple_agent::agent::is_default_maple_workspace(std::path::Path::new(&root)) {
        return Some(root);
    }
    match maple_agent::agent::ensure_default_maple_workspace() {
        Ok(path) => Some(path.to_string_lossy().to_string()),
        Err(error) => {
            log::warn!("{error}");
            fallback_project_root()
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
struct PersistedAuthRecord {
    owner: maple_agent::store::records::CredentialOwner,
    user_id: String,
    api_url: String,
    access_token: String,
    refresh_token: Option<String>,
}

impl std::fmt::Debug for PersistedAuthRecord {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PersistedAuthRecord")
            .field("owner", &self.owner)
            .field("tokens", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl Drop for PersistedAuthRecord {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.access_token.zeroize();
        self.refresh_token.zeroize();
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PersistedAuthOwner {
    user_id: String,
    durable: maple_agent::store::records::CredentialOwner,
}

impl PersistedAuthOwner {
    fn matches(&self, snapshot: &MapleApiAuthSnapshot) -> bool {
        self.user_id == snapshot.user_id && self.durable.session_id == snapshot.session_id
    }
}

#[derive(Default)]
struct PersistedAuthState {
    owner: Option<PersistedAuthOwner>,
    // A validated SDK session may remain usable after a failed persistence.
    // This identity never grants a callback durable credential ownership.
    live: Option<(String, String)>,
    pending_clear: Option<maple_agent::store::records::CredentialOwner>,
    // An explicit sign-in may observe an occupied slot belonging to another
    // origin. A failed write retains that exact replacement authority; Retry
    // must never reread and adopt a newer slot owner.
    pending_claim: Option<PendingCredentialClaim>,
}

struct PendingCredentialClaim {
    user_id: String,
    session_id: String,
    expected: Option<maple_agent::store::records::CredentialOwner>,
}

impl PendingCredentialClaim {
    fn matches(&self, snapshot: &MapleApiAuthSnapshot) -> bool {
        self.user_id == snapshot.user_id && self.session_id == snapshot.session_id
    }
}

/// Orders SDK callbacks against durable account/session/revision ownership.
/// Construction never reads credentials; reads require the store to be open.
struct PersistedAuthStore {
    store: Option<Arc<maple_agent::store::Store>>,
    api_url: String,
    state: std::sync::Mutex<PersistedAuthState>,
}

impl PersistedAuthStore {
    fn new(store: Option<Arc<maple_agent::store::Store>>, api_url: String) -> Self {
        Self {
            store,
            api_url,
            state: std::sync::Mutex::new(PersistedAuthState::default()),
        }
    }

    fn read_record(&self) -> Option<PersistedAuthRecord> {
        let store = self.store.as_ref()?;
        if store.state() != maple_agent::store::StoreState::Open {
            return None;
        }
        match store.credentials(&self.api_url) {
            Ok(record) => record.map(|record| PersistedAuthRecord {
                owner: record.owner,
                user_id: record.user_id,
                api_url: record.api_url,
                access_token: record.tokens.access_token.expose().to_owned(),
                refresh_token: record
                    .tokens
                    .refresh_token
                    .as_ref()
                    .map(|token| token.expose().to_owned()),
            }),
            Err(error) => {
                log::warn!("Cannot read saved credentials: {error}");
                None
            }
        }
    }

    fn load(&self) -> Option<PersistedAuthRecord> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.owner = None;
        state.live = None;
        state.pending_clear = None;
        state.pending_claim = None;
        let record = self.read_record()?;
        state.live = Some((record.user_id.clone(), record.owner.session_id.clone()));
        state.owner = Some(PersistedAuthOwner {
            user_id: record.user_id.clone(),
            durable: record.owner.clone(),
        });
        Some(record)
    }

    fn saved_user_id(&self) -> Option<String> {
        if self.store.as_ref()?.state() != maple_agent::store::StoreState::Open {
            return None;
        }
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .live
            .as_ref()
            .map(|(user, _)| user.clone())
    }

    fn forget_owner(&self) -> Option<String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.owner = None;
        state.pending_clear = None;
        state.pending_claim = None;
        state.live.take().map(|(user, _)| user)
    }

    fn live_identity(&self) -> Option<(String, String)> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .live
            .clone()
    }

    fn retry_snapshot(&self, snapshot: &MapleApiAuthSnapshot) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.live.as_ref().is_some_and(|(user, session)| {
            user == &snapshot.user_id && session == &snapshot.session_id
        }) || state.owner.as_ref().is_some_and(|owner| {
            owner.matches(snapshot) && snapshot.revision < owner.durable.revision
        }) {
            return Err("Authentication changed during saving. Retry saving.".into());
        }
        let store = self.store.as_ref().ok_or("Local storage is unavailable")?;
        // Retry retains either the last durable owner or the exact slot an
        // explicit validated sign-in observed before its failed write. It must
        // never adopt a newer independently replaced row.
        let account = store
            .upsert_account(&snapshot.user_id, Self::now_ms())
            .map_err(|error| error.to_string())?;
        let expected = state
            .pending_claim
            .as_ref()
            .filter(|claim| claim.matches(snapshot))
            .map(|claim| claim.expected.as_ref())
            .unwrap_or_else(|| state.owner.as_ref().map(|owner| &owner.durable));
        let durable = store
            .replace_credentials(
                expected,
                account,
                &self.api_url,
                &snapshot.session_id,
                snapshot.revision,
                &Self::tokens(snapshot),
                Self::now_ms(),
            )
            .map_err(|_| "Current sign-in credentials could not be saved. Keep Maple open and retry saving.")?;
        state.owner = Some(PersistedAuthOwner {
            user_id: snapshot.user_id.clone(),
            durable,
        });
        state.pending_clear = None;
        state.pending_claim = None;
        Ok(())
    }

    // A cold restored identity has no SDK session until its first API call.
    // Verify that it still names the actual durable owner; do not manufacture
    // or refresh an SDK session just to clear a storage warning.
    fn retry_without_sdk(&self) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(expected) = state.pending_clear.as_ref() {
            if !self.clear_owner(expected) {
                return Err("Sign-out could not remove saved credentials. Retry saving.".into());
            }
            state.pending_clear = None;
        }
        let Some((user, session)) = state.live.as_ref() else {
            return Ok(());
        };
        let owner = state
            .owner
            .as_ref()
            .filter(|owner| owner.user_id == *user && owner.durable.session_id == *session)
            .ok_or(
                "Current sign-in credentials are not saved. Keep Maple open and retry saving.",
            )?;
        let store = self.store.as_ref().ok_or("Local storage is unavailable")?;
        let current = store
            .credentials(&self.api_url)
            .map_err(|error| error.to_string())?;
        if current
            .as_ref()
            .is_some_and(|current| current.user_id == *user && current.owner == owner.durable)
        {
            Ok(())
        } else {
            Err("Saved sign-in ownership changed. Retry saving.".into())
        }
    }

    fn tokens(snapshot: &MapleApiAuthSnapshot) -> maple_agent::store::records::CredentialTokens {
        use maple_agent::store::records::{CredentialTokens, SecretString};
        CredentialTokens {
            access_token: SecretString::new(snapshot.access_token.clone()),
            refresh_token: snapshot.refresh_token.clone().map(SecretString::new),
        }
    }

    fn now_ms() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    fn claim_and_persist(&self, snapshot: &MapleApiAuthSnapshot) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.owner.as_ref().is_some_and(|owner| {
            owner.matches(snapshot) && snapshot.revision < owner.durable.revision
        }) {
            return false;
        }
        state.live = Some((snapshot.user_id.clone(), snapshot.session_id.clone()));
        state.pending_claim = None;
        let Some(store) = self.store.as_ref() else {
            return false;
        };
        let current = match store.credential_owner_for_sign_in() {
            Ok(current) => current,
            Err(error) => {
                log::error!("Cannot read credential ownership: {error}");
                return false;
            }
        };
        self.save_claim(&mut state, snapshot, current)
    }

    fn persist_restored(
        &self,
        snapshot: &MapleApiAuthSnapshot,
        restored: &PersistedAuthRecord,
    ) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if snapshot.user_id != restored.user_id
            || restored.api_url != self.api_url
            || !state.live.as_ref().is_some_and(|(user, session)| {
                user == &restored.user_id && session == &restored.owner.session_id
            })
            || !state.owner.as_ref().is_some_and(|owner| {
                owner.user_id == restored.user_id && owner.durable == restored.owner
            })
        {
            return false;
        }
        state.live = Some((snapshot.user_id.clone(), snapshot.session_id.clone()));
        // Validation of saved credentials grants no authority over a row that
        // replaced them while the network request was outstanding.
        self.save_claim(&mut state, snapshot, Some(restored.owner.clone()))
    }

    fn save_claim(
        &self,
        state: &mut PersistedAuthState,
        snapshot: &MapleApiAuthSnapshot,
        current: Option<maple_agent::store::records::CredentialOwner>,
    ) -> bool {
        let Some(store) = self.store.as_ref() else {
            return false;
        };
        state.pending_claim = Some(PendingCredentialClaim {
            user_id: snapshot.user_id.clone(),
            session_id: snapshot.session_id.clone(),
            expected: current.clone(),
        });
        let result = store
            .upsert_account(&snapshot.user_id, Self::now_ms())
            .and_then(|account| {
                if current.as_ref().is_some_and(|owner| {
                    owner.account == account
                        && owner.session_id == snapshot.session_id
                        && snapshot.revision < owner.revision
                }) {
                    return Err(maple_agent::store::StoreError::Conflict);
                }
                store.replace_credentials(
                    current.as_ref(),
                    account,
                    &self.api_url,
                    &snapshot.session_id,
                    snapshot.revision,
                    &Self::tokens(snapshot),
                    Self::now_ms(),
                )
            });
        match result {
            Ok(durable) => {
                // Publication follows the successful durable transaction.
                state.owner = Some(PersistedAuthOwner {
                    user_id: snapshot.user_id.clone(),
                    durable,
                });
                state.pending_clear = None;
                state.pending_claim = None;
                true
            }
            Err(error) => {
                log::error!(
                    "Cannot save credentials: {error}. Sign in will be needed at the next start."
                );
                false
            }
        }
    }

    fn persist_rotation(&self, snapshot: &MapleApiAuthSnapshot) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.live.as_ref().is_some_and(|(user, session)| {
            user == &snapshot.user_id && session == &snapshot.session_id
        }) {
            return;
        }
        let Some(owner) = state.owner.as_mut() else {
            return;
        };
        if !owner.matches(snapshot) || snapshot.revision < owner.durable.revision {
            return;
        }
        let Some(store) = self.store.as_ref() else {
            return;
        };
        // SDK revisions can jump after failed persistence. CAS the last durable
        // owner while storing the actual SDK revision, rather than incrementing.
        match store.replace_credentials(
            Some(&owner.durable),
            owner.durable.account,
            &self.api_url,
            &snapshot.session_id,
            snapshot.revision,
            &Self::tokens(snapshot),
            Self::now_ms(),
        ) {
            Ok(durable) => owner.durable = durable,
            Err(error) => log::error!("Cannot save rotated credentials: {error}"),
        }
    }

    fn clear_if_owned(&self, snapshot: &MapleApiAuthSnapshot) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.live.as_ref().is_some_and(|(user, session)| {
            user == &snapshot.user_id && session == &snapshot.session_id
        }) {
            state.live = None;
            state.pending_claim = None;
        }
        if !state
            .owner
            .as_ref()
            .is_some_and(|owner| owner.matches(snapshot))
        {
            return true;
        }
        let owner = state.owner.take().expect("matched credential owner");
        // Stop late callbacks even when durable deletion fails, and let the
        // caller report that sign-out could not erase the saved credentials.
        let cleared = self.clear_owner(&owner.durable);
        if !cleared {
            state.pending_clear = Some(owner.durable);
        }
        cleared
    }

    fn clear_record_if_unchanged(&self, expected: &PersistedAuthRecord) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .owner
            .as_ref()
            .is_some_and(|owner| owner.durable == expected.owner)
        {
            state.owner = None;
            state.live = None;
            state.pending_claim = None;
        }
        let cleared = self.clear_owner(&expected.owner);
        if !cleared {
            state.pending_clear = Some(expected.owner.clone());
        }
        cleared
    }

    fn clear_owner(&self, owner: &maple_agent::store::records::CredentialOwner) -> bool {
        let Some(store) = self.store.as_ref() else {
            return false;
        };
        match store.clear_credentials(owner) {
            Ok(_) => true, // A CAS miss preserves the replacement session.
            Err(error) => {
                log::error!("Cannot remove saved credentials: {error}");
                false
            }
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

impl LocalBackend {
    #[cfg(test)]
    pub fn new(api_url: String, harness_instructions: String) -> Result<Self, String> {
        // App tests never open any developer-owned store, including store-v1.
        let root = std::env::temp_dir().join(format!("maple-app-store-test-{}", Uuid::new_v4()));
        let store = Arc::new(
            maple_agent::store::Store::open(maple_agent::store::StorePaths::new(root))
                .map_err(|error| error.to_string())?,
        );
        Self::build(api_url, harness_instructions, Some(store))
    }

    pub fn new_with_store(
        api_url: String,
        harness_instructions: String,
        store: Arc<maple_agent::store::Store>,
    ) -> Result<Self, String> {
        Self::build(api_url, harness_instructions, Some(store))
    }

    pub(crate) fn store(&self) -> Option<&Arc<maple_agent::store::Store>> {
        self.store.as_ref()
    }

    /// Called only after the host has opened or unlocked the store.
    pub(crate) fn reload_open_store(&self) -> Result<(), String> {
        let store = self.store.as_ref().ok_or("Local storage is unavailable")?;
        if store.state() != maple_agent::store::StoreState::Open {
            return Err("Maple is locked.".into());
        }
        // Spill cleanup is admitted only after this generation is compatible
        // and authenticated. Locked/refused startup leaves owned spill files
        // untouched until a later successful open reaches this boundary.
        maple_agent::store::spill::SpillRoot::new(store.paths())
            .and_then(|spills| spills.cleanup())
            .map_err(|error| error.to_string())?;
        let _ = self.persisted_auth.load();
        let user = self.persisted_auth.saved_user_id();
        let account = user
            .as_deref()
            .map(|user| store.account_id(user))
            .transpose()
            .map_err(|error| error.to_string())?
            .flatten();
        let settings = crate::settings::load_host_settings(store, account)?;
        self.set_harness_instructions(settings.effective_harness_instructions());
        Ok(())
    }

    pub(crate) async fn retry_store_persistence(&self) -> Result<(), String> {
        self.reconcile_credentials().await?;
        // Only explicit Retry dismisses the exact storage-failure latch after
        // credentials and every retained source history are durably saved.
        self.service.flush_persistence().await
    }

    async fn reconcile_credentials(&self) -> Result<(), String> {
        struct Snapshot(MapleApiAuthSnapshot);
        impl Drop for Snapshot {
            fn drop(&mut self) {
                use zeroize::Zeroize;
                self.0.access_token.zeroize();
                self.0.refresh_token.zeroize();
            }
        }
        let mut credentials_ready = false;
        for _ in 0..3 {
            let Some((user, session)) = self.persisted_auth.live_identity() else {
                self.persisted_auth.retry_without_sdk()?;
                credentials_ready = true;
                break;
            };
            let snapshot = match self.auth.auth_snapshot_for(&user).await {
                Ok(snapshot) => Snapshot(snapshot),
                Err(_) => {
                    self.persisted_auth.retry_without_sdk()?;
                    credentials_ready = true;
                    break;
                }
            };
            if snapshot.0.session_id != session {
                return Err("Authentication changed during saving. Retry saving.".into());
            }
            self.persisted_auth.retry_snapshot(&snapshot.0)?;
            let current = Snapshot(self.auth.auth_snapshot_for(&user).await?);
            if current.0.session_id == snapshot.0.session_id
                && current.0.revision == snapshot.0.revision
            {
                credentials_ready = true;
                break;
            }
        }
        if !credentials_ready {
            return Err(
                "Sign-in credentials changed during saving. Keep Maple open and retry saving."
                    .into(),
            );
        }
        Ok(())
    }

    pub(crate) async fn forget_store_session(&self) -> Result<(), String> {
        self.service.finish_store_reset().await?;
        self.service.invalidate_store_session();
        if let Some(user) = self.persisted_auth.forget_owner() {
            self.auth.clear_auth(&user).await?;
        }
        self.billing_tokens.lock().await.clear();
        self.clear_paging_cache()
    }

    pub(crate) fn begin_store_drain(&self) {
        self.service.begin_draining();
    }
    pub(crate) fn resume_store_admission(&self) {
        self.service.resume_admission();
    }
    pub(crate) async fn store_activity(&self) -> Result<(usize, usize), String> {
        self.service.store_activity().await
    }

    pub(super) fn service(&self) -> &MapleAgentService {
        &self.service
    }

    pub(super) fn row_reader(&self) -> Result<Arc<maple_agent::store::RowReader>, String> {
        self.row_reader
            .clone()
            .ok_or_else(|| "Local store is unavailable".into())
    }

    /// The host closes request admission and drains readers before this call.
    pub(crate) fn clear_paging_cache(&self) -> Result<(), String> {
        if let Some(reader) = &self.row_reader {
            reader.clear().map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    fn build(
        api_url: String,
        harness_instructions: String,
        store: Option<Arc<maple_agent::store::Store>>,
    ) -> Result<Self, String> {
        // Enforce the credential-bearing URL policy before any client is
        // built, including the login-time SDK client.
        let api_url = maple_agent::maple_api::validate_api_url(&api_url)?;
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let resource_root = store
            .as_ref()
            .map_or_else(generated_resource_root, |store| store.paths().resources());
        let instruction_root = if cfg!(test) {
            resource_root.clone()
        } else {
            instruction_config_root()
        };
        let paths = maple_agent::agent::AgentPathLayout::from_resource_root(resource_root)
            .with_instruction_config_root(instruction_root);
        let mut resources = MapleAgentHostResources::new(
            paths,
            Arc::new(ChannelEventSink(event_tx)),
            harness_instructions,
        );
        if let Some(store) = &store {
            resources = resources.with_store(store.clone());
        }
        let service = MapleAgentService::new(resources);
        service.manage_question_availability();
        let runtime = Handle::try_current()
            .map_err(|error| format!("Host runtime is unavailable: {error}"))?;
        let persisted_auth = Arc::new(PersistedAuthStore::new(store.clone(), api_url.clone()));
        if store
            .as_ref()
            .is_some_and(|store| store.state() == maple_agent::store::StoreState::Open)
        {
            let _ = persisted_auth.load();
        }
        // reqwest clients must be built inside a Tokio runtime context; one
        // built outside never completes a request.
        let billing = {
            let _guard = runtime.enter();
            crate::billing::BillingClient::new(crate::billing::configured_billing_api_url())?
        };
        let row_reader = store
            .as_ref()
            .map(|store| Arc::new(maple_agent::store::RowReader::new(store.clone())));
        Ok(Self {
            remote_updates: Arc::new(crate::remote_updates::Broker::new(store.clone())),
            store,
            row_reader,
            #[cfg(test)]
            ui_fixture: false,
            restore_lock: tokio::sync::Mutex::new(()),
            ui_settings_gate: tokio::sync::Mutex::new(()),
            account_gate: Arc::new(tokio::sync::RwLock::new(())),
            service,
            auth: MapleApiAuthState::new(),
            api_url,
            persisted_auth,
            pending_oauth: Arc::new(PendingOAuthStore::default()),
            client_id: configured_client_id(),
            event_rx: tokio::sync::Mutex::new(Some(event_rx)),
            billing,
            billing_tokens: tokio::sync::Mutex::new(HashMap::new()),
        })
    }

    fn open_store(&self) -> Result<Arc<maple_agent::store::Store>, String> {
        let store = self.store.as_ref().ok_or("Local store is unavailable")?;
        if store.state() != maple_agent::store::StoreState::Open {
            return Err("Local store is not open".into());
        }
        Ok(store.clone())
    }

    fn presentation_account(
        &self,
        user_id: &str,
    ) -> Result<
        (
            Arc<maple_agent::store::Store>,
            maple_agent::store::AccountId,
        ),
        String,
    > {
        self.authorize_account(user_id)?;
        let store = self.open_store()?;
        let account = store
            .account_id(user_id)
            .map_err(|e| e.to_string())?
            .ok_or("Local account is not ready")?;
        Ok((store, account))
    }

    pub async fn get_ui_settings(&self) -> Result<crate::settings::AppSettings, String> {
        let store = self.open_store()?;
        tokio::task::spawn_blocking(move || crate::settings::load_host_settings(&store, None))
            .await
            .map_err(|_| "Could not read UI settings".to_string())?
    }

    pub async fn set_ui_settings(&self, patch: serde_json::Value) -> Result<(), String> {
        let _gate = self.ui_settings_gate.lock().await;
        let store = self.open_store()?;
        let harness_changed = patch.get("harness_instructions").is_some();
        let settings = tokio::task::spawn_blocking(move || {
            crate::settings::apply_host_patch(&store, None, &patch)?;
            crate::settings::load_host_settings(&store, None)
        })
        .await
        .map_err(|_| "Could not save UI settings".to_string())??;
        if harness_changed {
            self.service
                .set_harness_instructions(settings.effective_harness_instructions());
        }
        Ok(())
    }

    pub async fn flush_ui_settings(&self) -> Result<(), String> {
        let _gate = self.ui_settings_gate.lock().await;
        self.open_store().map(|_| ())
    }

    pub async fn dictionary(
        &self,
        after: Option<String>,
        limit: u16,
    ) -> Result<DictionaryPage, String> {
        let store = self.open_store()?;
        tokio::task::spawn_blocking(move || {
            let page = store
                .dictionary_words(after.as_deref(), limit)
                .map_err(|e| e.to_string())?;
            Ok(DictionaryPage {
                words: page.words,
                next: page.next,
            })
        })
        .await
        .map_err(|_| "Could not read dictionary words".to_string())?
    }

    pub async fn add_word(&self, word: String) -> Result<bool, String> {
        let store = self.open_store()?;
        tokio::task::spawn_blocking(move || {
            store.add_dictionary_word(&word).map_err(|e| e.to_string())
        })
        .await
        .map_err(|_| "Could not add dictionary word".to_string())?
    }

    pub async fn get_remote_sidebars(
        &self,
        user_id: &str,
    ) -> Result<HashMap<String, crate::settings::RemoteSidebarSettings>, String> {
        let (store, account) = self.presentation_account(user_id)?;
        tokio::task::spawn_blocking(move || {
            let value = store
                .account_setting(account, "remote.sidebars")
                .map_err(|e| e.to_string())?
                .unwrap_or_else(|| serde_json::json!({}));
            serde_json::from_value(value)
                .map_err(|_| "Saved remote sidebar settings are invalid".to_string())
        })
        .await
        .map_err(|_| "Could not read remote sidebar settings".to_string())?
    }

    pub async fn set_remote_sidebar(
        &self,
        user_id: &str,
        computer: String,
        patch: serde_json::Value,
    ) -> Result<(), String> {
        if computer.is_empty() || computer.len() > 128 || computer.chars().any(char::is_control) {
            return Err("Invalid remote computer".into());
        }
        let _gate = self.ui_settings_gate.lock().await;
        let (store, account) = self.presentation_account(user_id)?;
        tokio::task::spawn_blocking(move || {
            let mut all: HashMap<String, crate::settings::RemoteSidebarSettings> =
                serde_json::from_value(
                    store
                        .account_setting(account, "remote.sidebars")
                        .map_err(|e| e.to_string())?
                        .unwrap_or_else(|| serde_json::json!({})),
                )
                .map_err(|_| "Saved remote sidebar settings are invalid".to_string())?;
            let current = all.entry(computer).or_default();
            crate::settings::patch_remote_sidebar(current, &patch)?;
            store
                .set_account_setting(
                    account,
                    "remote.sidebars",
                    &serde_json::to_value(all)
                        .map_err(|_| "Could not encode remote sidebar settings")?,
                    crate::settings::timestamp_ms(),
                )
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(|_| "Could not save remote sidebar settings".to_string())?
    }

    pub async fn set_task_pinned(
        &self,
        user_id: &str,
        task: &str,
        pinned: bool,
    ) -> Result<(), String> {
        let (store, account) = self.presentation_account(user_id)?;
        let task = task.to_owned();
        tokio::task::spawn_blocking(move || {
            store
                .set_task_pinned(account, &task, pinned, crate::settings::timestamp_ms())
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(|_| "Could not save task pin".to_string())?
    }

    /// Replace the opening system prompt text for tasks this app hosts.
    /// Applies to agents built after the call, so to a task's next fresh
    /// agent, not to one already loaded.
    pub fn set_harness_instructions(&self, harness_instructions: String) {
        self.service.set_harness_instructions(harness_instructions);
    }

    pub(crate) fn question_responder(&self) -> maple_agent::agent::QuestionResponder {
        self.service.question_responder()
    }

    pub(crate) fn authorize_account(&self, user_id: &str) -> Result<(), String> {
        #[cfg(test)]
        if self.ui_fixture {
            return Ok(());
        }
        if self.saved_user_id().as_deref() == Some(user_id) {
            Ok(())
        } else {
            Err("The host account changed. Reopen this screen before continuing.".into())
        }
    }

    fn require_signed_out(&self) -> Result<(), String> {
        if self.pending_deletion_status()?.is_some() {
            return Err(deletion::PENDING_DELETION_MESSAGE.to_string());
        }
        if self.saved_user_id().is_some() {
            Err("Maple is already signed in. Sign out before changing accounts.".into())
        } else {
            if let Some(store) = &self.store {
                // A pending deletion reserves the one physical slot even if
                // this launch selected a different API origin. Do not install
                // a new live session whose credential write must be rejected.
                store.credential_owner_for_sign_in().map_err(|error| {
                    if error == maple_agent::store::StoreError::Conflict {
                        deletion::PENDING_DELETION_MESSAGE.to_owned()
                    } else {
                        "Could not read saved sign-in ownership. Try again.".to_owned()
                    }
                })?;
            }
            Ok(())
        }
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
        let _account = self.restore_lock.lock().await;
        self.require_signed_out()?;
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

    fn persist_auth(&self, snapshot: &MapleApiAuthSnapshot) -> bool {
        self.persisted_auth.claim_and_persist(snapshot)
    }

    /// The sink the runtime calls when the SDK rotates the token pair
    /// during an API call. It commits the new pair in the store so the
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
    /// A read from the open store, so the UI may show account data at once
    /// while the host validates the credentials.
    pub fn saved_user_id(&self) -> Option<String> {
        self.persisted_auth.saved_user_id()
    }

    /// Validate the saved credentials with the server and install the
    /// session on success. Definitive rejections compare-clear the saved record.
    pub(super) async fn validate_persisted_auth(&self) -> RestoreOutcome {
        let _restore = self.restore_lock.lock().await;
        if let Some(user_id) = self.saved_user_id()
            && self.auth.session_for(&user_id).await.is_ok()
        {
            return RestoreOutcome::Valid(user_id);
        }
        self.cancel_oauth();
        let Some(persisted) = self.load_persisted_auth() else {
            return RestoreOutcome::Rejected;
        };
        let request_record = persisted.clone();
        let request = MapleApiAuthRequest {
            user_id: request_record.user_id.clone(),
            api_url: self.api_url.clone(),
            access_token: request_record.access_token.clone(),
            refresh_token: request_record.refresh_token.clone(),
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
                self.persisted_auth.persist_restored(&snapshot, &persisted);
                RestoreOutcome::Valid(snapshot.user_id)
            }
            Err(error) if maple_agent::maple_api::is_auth_rejection(&error) => {
                log::debug!("persisted auth rejected: {error:?}");
                let _ = self.service.sign_out(&persisted.user_id).await;
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
        self.authorize_account(user_id)?;
        if self.auth.session_for(user_id).await.is_err() {
            let _ = self.validate_persisted_auth().await;
        }
        self.auth.session_for(user_id).await
    }

    pub(crate) fn remote_api_url(&self) -> &str {
        &self.api_url
    }

    pub(crate) async fn remote_session(
        &self,
        user_id: &str,
    ) -> Result<Arc<maple_agent::maple_api::MapleApiSession>, String> {
        self.authorize_account(user_id)?;
        self.session_for(user_id).await
    }

    /// Sign out completely: invalidate the live session first, then remove
    /// only the persisted credentials that this sign-out observed.
    pub async fn logout_and_clear(&self, user_id: &str) -> Result<(), String> {
        self.clear_session(user_id, true).await
    }

    /// Drop the live session and the persisted record. `revoke` also sends
    /// `POST /logout`; a deleted account has no session left to report.
    async fn clear_session(&self, user_id: &str, revoke: bool) -> Result<(), String> {
        let _account = self.restore_lock.lock().await;
        let pending = self.pending_deletion_status()?;
        if let Some(pending) = pending.as_ref() {
            if pending.user_id != user_id {
                return Err(
                    "The host account changed. Reopen this screen before continuing.".into(),
                );
            }
            // A sign-out cannot resolve an uncertain server deletion. Keep the
            // exact pending credentials for privileged reconciliation, while
            // dropping every normal in-memory session and runtime handle.
            self.service.sign_out(user_id).await?;
            self.cancel_oauth();
            let _ = self.persisted_auth.forget_owner();
            return self.auth.clear_auth(user_id).await;
        }
        self.authorize_account(user_id)?;
        self.service.sign_out(user_id).await?;
        self.cancel_oauth();
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
                if !self.persisted_auth.clear_if_owned(snapshot) {
                    return Err(
                        "Sign-out could not remove saved credentials. Try again.".to_string()
                    );
                }
            } else if let Some(persisted) = persisted_without_session.as_ref() {
                // Preserve the old offline/logout behavior without letting a
                // concurrent sign-in's replacement record be removed.
                if !self.persisted_auth.clear_record_if_unchanged(persisted) {
                    return Err(
                        "Sign-out could not remove saved credentials. Try again.".to_string()
                    );
                }
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
        self.publish_session_with_persistence(user_id, access_token, refresh_token)
            .await
            .map(|(session, _)| session)
    }

    async fn publish_session_with_persistence(
        &self,
        user_id: String,
        access_token: String,
        refresh_token: Option<String>,
    ) -> Result<(AuthSession, bool), String> {
        let _account = self.restore_lock.lock().await;
        self.require_signed_out()?;
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
                let persisted = self.persist_auth(&snapshot);
                (
                    AuthSession {
                        user_id: snapshot.user_id.clone(),
                    },
                    persisted,
                )
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
        if let Some(origin) = crate::profile::AUTH_ORIGIN {
            let auth_url = attempt
                .while_active(async {
                    let prepared = client
                        .prepare_native_oauth_handoff()
                        .await
                        .map_err(|_| "Could not prepare browser sign-in. Try again.".to_string())?;
                    let listener = HandoffListener::bind()?;
                    let provider_name = match provider {
                        OAuthProvider::Github => "github",
                        OAuthProvider::Google => "google",
                        OAuthProvider::Apple => "apple",
                    };
                    let url = listener.start_url(
                        origin,
                        provider_name,
                        prepared.session_id(),
                        prepared.request_id(),
                    );
                    self.pending_oauth
                        .set_handoff(&attempt, HostedHandoff { listener, prepared })?;
                    Ok(url)
                })
                .await?;
            if webbrowser::open(&auth_url).is_err() {
                return Err(
                    "Could not open your browser. Check your default browser and try again."
                        .to_string(),
                );
            }
            guard.retain = true;
            return Ok(auth_url);
        }
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

    /// Exercise the real cancellation primitive without hosted credentials or
    /// network work. Callers provide the same account/admission guards as IPC.
    #[cfg(test)]
    pub(crate) fn pending_oauth_waiter_for_test(
        &self,
        release_publication: tokio::sync::oneshot::Receiver<()>,
        published: Arc<std::sync::atomic::AtomicBool>,
    ) -> impl Future<Output = Result<(), String>> + '_ {
        let attempt = self.pending_oauth.begin(
            OAuthProvider::Google,
            OpenSecretClient::new("http://127.0.0.1:1").unwrap(),
        );
        async move {
            let _guard = OAuthAttemptGuard {
                store: &self.pending_oauth,
                attempt: &attempt,
                retain: false,
            };
            attempt
                .while_active(async {
                    release_publication
                        .await
                        .map_err(|_| "The OAuth fixture was dropped".to_string())?;
                    published.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                })
                .await
        }
    }

    /// Wait on the existing listener and consume exactly the SDK handle that
    /// created the advertised target. Cancellation closes the listener/socket;
    /// malformed callbacks leave it available until the attempt deadline.
    pub async fn oauth_wait_for_handoff(
        &self,
        provider: OAuthProvider,
    ) -> Result<AuthSession, String> {
        let (attempt, handoff) = self.pending_oauth.take_handoff(provider)?;
        let _guard = OAuthAttemptGuard {
            store: &self.pending_oauth,
            attempt: &attempt,
            retain: false,
        };
        attempt
            .while_active(async {
                let HostedHandoff { listener, prepared } = handoff;
                let (grant, browser) = listener.receive(&prepared).await?;
                let outcome = async {
                    let response = attempt
                        .client
                        .redeem_native_oauth_handoff(prepared, grant)
                        .await
                        .map_err(|_| {
                            "Browser sign-in failed or expired. Start again.".to_string()
                        })?;
                    self.publish_session_with_persistence(
                        response.id.to_string(),
                        response.access_token,
                        Some(response.refresh_token),
                    )
                    .await
                }
                .await;
                drop(listener);
                browser.finish(
                    outcome
                        .as_ref()
                        .map(|(_, persisted)| *persisted)
                        .map_err(|_| ()),
                );
                // No await after native publication: a late Back or failed browser
                // response must not hide an already committed account from the UI.
                outcome.map(|(session, _)| session)
            })
            .await
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

    /// Prepaid API credits left on the account. The billing error is
    /// logged, not shown: it can carry the server's whole response body.
    pub async fn api_credit_balance(&self, user_id: &str) -> Result<i64, String> {
        self.billing_call(user_id, |billing, token| async move {
            billing.api_credit_balance(&token).await
        })
        .await
        .map_err(|error| {
            log::warn!("Could not load the credit balance: {error}");
            "Could not load the latest balance. Refresh to try again.".to_string()
        })
    }

    /// Start a card or Bitcoin checkout for `credits` prepaid API credits in
    /// the system browser. The billing service credits the account from the
    /// provider's webhook; the browser then lands on the web app, so no
    /// desktop callback is involved. Guests have no email and pay with
    /// Bitcoin only, as on the web.
    pub async fn purchase_api_credits(
        &self,
        user_id: &str,
        method: crate::billing::PaymentMethod,
        credits: u64,
    ) -> Result<(), String> {
        use crate::billing::PaymentMethod;
        if !credits.is_multiple_of(crate::billing::CREDITS_PER_DOLLAR) {
            return Err("Buy credits in whole dollars".to_string());
        }
        crate::billing::credits_for_dollars(credits / crate::billing::CREDITS_PER_DOLLAR)?;
        let account = self.account(user_id).await?;
        if method == PaymentMethod::Stripe && account.login_method == MapleLoginMethod::Guest {
            return Err("Anonymous accounts pay with Bitcoin".to_string());
        }
        let request = Arc::new(crate::billing::CreditPurchaseRequest {
            credits,
            email: account.email.unwrap_or_default(),
            success_url: crate::billing::CREDITS_SUCCESS_URL.to_string(),
            cancel_url: (method == PaymentMethod::Stripe)
                .then(|| crate::billing::CREDITS_CANCEL_URL.to_string()),
        });
        let url = self
            .billing_call(user_id, |billing, token| {
                let request = Arc::clone(&request);
                async move { billing.credit_checkout_url(&token, method, &request).await }
            })
            .await
            .map_err(|error| billing_failure(error, "Could not start the credit checkout"))?;
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
        // saved root or Documents/Maple instead.
        let request = match request {
            Some(AgentStartRequest {
                project_root: None,
                model,
            }) => {
                let config = handle.load_config().await?;
                Some(AgentStartRequest {
                    project_root: gui_start_root(&config),
                    model,
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

    /// Only the owner's exactly confirmed reset path may call this. Saving
    /// credentials cannot be a prerequisite for discarding that same store.
    pub(crate) async fn prepare_store_reset(&self) -> Result<(), String> {
        self.service.prepare_store_reset().await
    }

    pub(crate) async fn shutdown_host(&self) -> Result<(), String> {
        // Stop must not drop the SDK's only current token pair or resurrect
        // credentials whose earlier logout deletion failed. This path is also
        // used during password maintenance, under the owner's exclusive gate.
        // It deliberately does not consult shutdown_requested or clear the
        // failure notice; inability to save returns before runtime/key release.
        if self
            .store
            .as_ref()
            .is_some_and(|store| store.state() == maple_agent::store::StoreState::Open)
        {
            self.reconcile_credentials().await?;
        }
        self.service.begin_draining();
        self.service.shutdown().await
    }

    #[cfg(feature = "acp")]
    pub async fn get_acp_config(
        &self,
        user_id: &str,
    ) -> Result<maple_agent::acp::AgentAcpConfig, String> {
        self.authorize_account(user_id)?;
        maple_agent::acp::load_acp_config(self.open_store()?.as_ref(), user_id)
    }

    #[cfg(feature = "acp")]
    pub async fn set_acp_config(
        &self,
        user_id: &str,
        config: maple_agent::acp::AgentAcpConfig,
    ) -> Result<maple_agent::acp::AgentAcpConfig, String> {
        self.authorize_account(user_id)?;
        maple_agent::acp::save_acp_config(
            self.open_store()?.as_ref(),
            user_id,
            config,
            chrono::Utc::now().timestamp_millis(),
        )
    }

    #[cfg(feature = "acp")]
    pub(crate) async fn serve_acp<R, W>(&self, read: R, write: W) -> Result<(), String>
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        // Joining the existing account is not an account switch. Keep local
        // desktop operations available while saved credentials are validated.
        let account = self.account_gate.read().await;
        let user_id = match self.validate_persisted_auth().await {
            RestoreOutcome::Valid(user_id) => user_id,
            _ => {
                return Err(
                    "No saved Maple sign-in. Open the desktop app and sign in first.".into(),
                );
            }
        };
        let handle = self.service.handle_for_user(&user_id).await?;
        let session = self.session_for(&user_id).await?;
        let starting = handle.clone();
        let start = maple_agent::acp::shared_runtime_start(async move {
            starting.start(session, None).await.map(|_| ())
        });
        let config = maple_agent::acp::load_acp_config(self.open_store()?.as_ref(), &user_id)?;
        drop(account);
        maple_agent::acp::serve_connection(handle, config, start, read, write).await
    }

    /// Rename one project on the execution computer. The owner publishes a
    /// bounded metadata delta to every desktop; it never returns the catalog.
    pub async fn rename_project_root(
        &self,
        user_id: &str,
        path: String,
        name: String,
    ) -> Result<RecentProjectRoot, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .rename_project_root(path, name)
            .await
    }

    /// Register and select the default root for new tasks, retaining each
    /// existing task's working directory and its independent trust decision.
    pub async fn select_project_root(
        &self,
        user_id: &str,
        path: String,
    ) -> Result<SelectedProject, String> {
        let handle = self.service.handle_for_user(user_id).await?;
        // The listed Maple workspace is created the first time it is chosen.
        if maple_agent::agent::is_default_maple_workspace(std::path::Path::new(path.trim())) {
            maple_agent::agent::ensure_default_maple_workspace()?;
        }
        let project_root = handle.save_recent_project_root(path).await?;
        let project = handle.project_root_metadata(project_root.clone()).await?;
        Ok(SelectedProject {
            project_root,
            project,
        })
    }

    pub async fn archive_project_root(
        &self,
        user_id: &str,
        path: String,
        fallback: Option<String>,
    ) -> Result<(), String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .archive_project_root(path, fallback)
            .await
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

    /// Move a task between active, settled, and archived. The runtime
    /// refuses to settle or archive a task while it runs.
    pub async fn set_session_state(
        &self,
        user_id: &str,
        session_id: &str,
        state: AgentTaskState,
    ) -> Result<AgentSessionSummary, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .set_session_state(session_id.to_string(), state)
            .await
    }

    /// Delete a task and everything stored for it. The runtime refuses
    /// while the task runs or an external surface holds it.
    pub async fn delete_session(&self, user_id: &str, session_id: &str) -> Result<(), String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .delete_session(session_id.to_string())
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

    pub async fn model_catalog(
        &self,
        user_id: &str,
    ) -> Result<maple_sdk::ModelCatalogResponse, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .model_catalog()
            .await
    }

    /// What the catalog says about a model: its vision and context window.
    /// None when the catalog cannot be read.
    pub async fn model_catalog_entry(
        &self,
        user_id: &str,
        model: &str,
    ) -> Result<Option<maple_agent::agent::CatalogEntry>, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .model_catalog_entry(model)
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
        kind: maple_agent::agent::AgentSessionIntegrationKind,
        enabled: bool,
    ) -> Result<Vec<maple_agent::agent::AgentSessionMcpServer>, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .set_session_mcp_server_enabled(maple_agent::agent::AgentSetSessionMcpServerRequest {
                session_id: session_id.to_string(),
                name: name.to_string(),
                kind,
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

    pub async fn list_mcp_definitions(
        &self,
        user_id: &str,
    ) -> Result<Vec<maple_agent::store::records::McpDefinition>, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .list_mcp_definitions()
            .await
    }

    /// The local host owns the entire OAuth attempt and its credentials. Only
    /// public MCP metadata returns to the desktop after the approved setup is
    /// installed by the account-bound runtime handle.
    pub async fn connect_vana_local(
        &self,
        user_id: &str,
        endpoint: String,
    ) -> Result<Vec<maple_agent::store::records::McpDefinition>, String> {
        self.authorize_account(user_id)?;
        let handle = self.service.handle_for_user(user_id).await?;
        let browser_endpoint = endpoint.clone();
        handle
            .connect_vana_local(
                endpoint,
                Arc::new(move |url| open_vana_authorization(&browser_endpoint, &url)),
            )
            .await
    }

    pub async fn list_project_mcp_servers(
        &self,
        user_id: &str,
        project_root: &str,
    ) -> Result<Vec<maple_agent::agent::AgentMcpServer>, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .list_project_mcp_servers(project_root.to_string())
            .await
    }

    pub async fn set_project_mcp_server_enabled(
        &self,
        user_id: &str,
        project_root: &str,
        name: &str,
        enabled: bool,
    ) -> Result<(), String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .set_project_mcp_server_enabled(project_root.to_string(), name.to_string(), enabled)
            .await
    }

    pub async fn list_integrations(&self, user_id: &str) -> Result<Vec<AgentIntegration>, String> {
        #[cfg(test)]
        if self.ui_fixture {
            // UI state tests still use real account-local MCP persistence, but
            // must not probe the runner's installed CLIs or OS permissions.
            // In particular, Claude's version and auth probes can each take
            // three seconds. Host and runtime tests use the real catalog.
            return Ok([
                ("codex", "Codex"),
                ("claude", "Claude Code"),
                ("cua-driver", "Computer use"),
            ]
            .into_iter()
            .map(|(id, name)| AgentIntegration {
                id: id.into(),
                name: name.into(),
                description: String::new(),
                availability: maple_agent::agent::AgentIntegrationAvailability::NotDetected,
                backend: None,
                version: None,
                permissions: None,
                setup_available: false,
                enabled_for_new_tasks: false,
                detail: None,
            })
            .collect());
        }
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

    pub async fn save_mcp_servers_remote(
        &self,
        user_id: &str,
        servers: Vec<maple_agent::agent::AgentMcpServer>,
    ) -> Result<Vec<maple_agent::agent::AgentMcpServer>, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .save_mcp_servers_remote(servers)
            .await
    }

    pub async fn save_mcp_definition_edits(
        &self,
        user_id: &str,
        edits: Vec<maple_agent::store::records::McpServerEdit>,
    ) -> Result<Vec<maple_agent::store::records::McpDefinition>, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .save_mcp_definition_edits(edits)
            .await
    }

    pub async fn save_mcp_definition_edits_remote(
        &self,
        user_id: &str,
        edits: Vec<maple_agent::store::records::McpServerEdit>,
    ) -> Result<Vec<maple_agent::store::records::McpDefinition>, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .save_mcp_definition_edits_remote(edits)
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

    /// Compact a session's history now; reload the session afterwards.
    pub async fn compact_session(&self, user_id: &str, session_id: &str) -> Result<(), String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .compact_session(session_id.to_string())
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

    /// Interrupt an external agent (Codex) from its row. The agent keeps
    /// its thread so the task can continue it later.
    pub async fn cancel_external_agent(
        &self,
        user_id: &str,
        session_id: &str,
        agent_id: &str,
    ) -> Result<(), String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .cancel_external_agent(session_id, agent_id)
            .await
    }

    /// Slash commands (installed skills) for a working directory. Filesystem
    /// scan, so it runs on a blocking thread.
    pub async fn list_slash_commands(
        &self,
        user_id: &str,
        working_dir: Option<String>,
    ) -> Result<Vec<AgentSlashCommand>, String> {
        let service = self.service.clone();
        let user_id = user_id.to_string();
        tokio::task::spawn_blocking(move || {
            service.list_slash_commands(Some(&user_id), working_dir.as_deref())
        })
        .await
        .map_err(|error| format!("Slash command scan failed: {error}"))
    }

    /// Expand `/command args` into the skill or prompt template's prompt;
    /// `None` when the command matches neither.
    pub async fn resolve_slash_command(
        &self,
        user_id: &str,
        working_dir: Option<String>,
        command: String,
        args: String,
    ) -> Result<Option<String>, String> {
        let service = self.service.clone();
        let user_id = user_id.to_string();
        tokio::task::spawn_blocking(move || {
            service.resolve_slash_command(Some(&user_id), working_dir.as_deref(), &command, &args)
        })
        .await
        .map_err(|error| format!("Slash command resolve failed: {error}"))?
    }

    /// One-line summary of a completed tool call from the summary model.
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

    /// One-line summary of a tool call from its name and input, asked as
    /// the call starts.
    pub async fn summarize_tool_call_start(
        &self,
        user_id: &str,
        session_id: &str,
        tool_name: String,
        input: Option<serde_json::Value>,
    ) -> Result<Option<String>, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .summarize_tool_call_start(session_id, &tool_name, input.as_ref())
            .await
    }

    /// One-line summary of a finished thinking block from the summary model.
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
    /// A session's context usage: (context tokens, context limit). The
    /// tokens are the runtime's count of the task's context; the limit comes
    /// from the model catalog for the selected model; MAPLE_CONTEXT_LIMIT is
    /// a manual override; 200k is the fallback when the catalog lacks the
    /// model.
    pub async fn context_usage(
        &self,
        user_id: &str,
        session_id: &str,
        model: Option<&str>,
    ) -> Result<Option<(i64, i64)>, String> {
        if self.account_scope(user_id).is_none() {
            return Ok(None);
        }
        let handle = self.service.handle_for_user(user_id).await?;
        let limit: i64 = match std::env::var("MAPLE_CONTEXT_LIMIT")
            .ok()
            .and_then(|value| value.parse().ok())
        {
            Some(limit) if limit > 0 => limit,
            _ => match model {
                Some(model) => handle
                    .context_limit_for_model(model)
                    .await?
                    .unwrap_or(200_000),
                None => 200_000,
            },
        };
        let tokens = handle.session_context_tokens(session_id).await?;
        Ok(tokens.map(|tokens| (i64::try_from(tokens).unwrap_or(i64::MAX), limit)))
    }

    /// Deliver the user's answer to an ask_user question. Returns false
    /// when no question was pending.
    pub async fn answer_question(
        &self,
        user_id: &str,
        request_id: &str,
        answer: String,
    ) -> Result<bool, String> {
        let request_id = request_id.to_string();
        self.service
            .handle_for_user(user_id)
            .await?
            .answer_question_via_handle(&request_id, answer)
            .await
    }

    pub async fn thinking_state(
        &self,
        user_id: &str,
        session_id: Option<&str>,
        model: &str,
    ) -> Result<maple_agent::agent::AgentThinkingState, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .thinking_state(session_id, model)
            .await
    }

    pub async fn set_thinking_level(
        &self,
        user_id: &str,
        session_id: Option<&str>,
        model: &str,
        level: maple_agent::agent::ThinkingLevel,
    ) -> Result<maple_agent::agent::AgentThinkingState, String> {
        self.service
            .handle_for_user(user_id)
            .await?
            .set_thinking_level(session_id, model, level)
            .await
    }

    /// Persist the UI's model choice as the account's default model.
    pub async fn save_default_model(&self, user_id: &str, model: String) -> Result<(), String> {
        let handle = self.service.handle_for_user(user_id).await?;
        let mut config = handle.load_config().await?;
        config.default_model = model;
        handle.save_config(config).await
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

/// OAuth metadata is untrusted even when it comes from a user-selected local
/// endpoint. The browser may only open its exact loopback origin; never launch
/// arbitrary protocols or disclose the authorization query in an error/log.
fn validate_vana_authorization(endpoint: &str, address: &str) -> Result<(), String> {
    const INVALID: &str =
        "Vana returned an unexpected sign-in address. Copy its Local MCP endpoint again.";
    if endpoint.len() > 4096 || address.len() > 16 * 1024 {
        return Err(INVALID.into());
    }
    let endpoint = reqwest::Url::parse(endpoint.trim()).map_err(|_| INVALID.to_string())?;
    let address = reqwest::Url::parse(address).map_err(|_| INVALID.to_string())?;
    if endpoint.scheme() != "http"
        || endpoint.host_str() != Some("127.0.0.1")
        || !endpoint.port().is_some_and(|port| port != 0)
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.path() != "/mcp"
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
        || address.origin() != endpoint.origin()
        || !address.username().is_empty()
        || address.password().is_some()
        || address.fragment().is_some()
    {
        return Err(INVALID.into());
    }
    Ok(())
}

fn open_vana_authorization(endpoint: &str, address: &str) -> Result<(), String> {
    validate_vana_authorization(endpoint, address)?;
    // This runs in the local host's IPC worker, never the GPUI thread. A
    // webbrowser error may contain the full URL; keep it out of logs and IPC.
    webbrowser::open(address).map_err(|_| {
        "Could not open your browser for Vana sign-in. Check your default browser and try again."
            .to_string()
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
#[path = "project_names_tests.rs"]
mod project_names_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vana_local_browser_only_opens_the_requested_loopback_origin() {
        let endpoint = "http://127.0.0.1:47321/mcp";
        assert!(
            validate_vana_authorization(
                endpoint,
                "http://127.0.0.1:47321/mcp/oauth/authorize?client_id=fixture&state=synthetic-state",
            )
            .is_ok()
        );
        for address in [
            "https://example.com/authorize?state=synthetic-state",
            "http://127.0.0.1:47322/mcp/oauth/authorize",
            "http://localhost:47321/mcp/oauth/authorize",
            "http://user:password@127.0.0.1:47321/mcp/oauth/authorize",
            "http://127.0.0.1:47321/mcp/oauth/authorize#unexpected",
            "vana://mcp-authorization?fixture=synthetic",
        ] {
            assert!(validate_vana_authorization(endpoint, address).is_err());
        }
        for endpoint in [
            "https://example.com/mcp",
            "http://127.0.0.1:0/mcp",
            "http://127.0.0.1:47321/mcp?unexpected=true",
            "http://127.0.0.1:47321/mcp#unexpected",
            "http://127.0.0.1:47321/another-resource",
        ] {
            assert!(
                validate_vana_authorization(endpoint, "http://127.0.0.1:47321/authorize").is_err()
            );
        }
    }

    #[tokio::test]
    async fn remote_mcp_host_boundary_accepts_write_only_values_and_keeps_account_fences() {
        let root = tempfile::tempdir().unwrap();
        let backend = LocalBackend::isolated_for_test(root.path());
        assert!(backend.persisted_auth.claim_and_persist(&auth_snapshot(
            "fixture-user",
            "fixture-session",
            1,
            "fixture-token"
        )));
        let request = |method: &str, params| crate::host::protocol::Request {
            id: 1,
            method: method.into(),
            params,
            send_id: None,
        };
        let edit = serde_json::json!({
            "key":"example","name":"Example","description":"Synthetic","enabled":true,"timeoutSeconds":30,
            "target":{"type":"streamable_http","url":"https://mcp.example.invalid/"},
            "environment":[{"name":"TOKEN","update":{"operation":"set","value":"synthetic-private-value"}}],
            "headers":[{"name":"Authorization","update":{"operation":"set","value":"synthetic-private-header"}}]
        });
        let saved = super::super::dispatch_remote_inner(
            &backend,
            request(
                "save_mcp_definition_edits",
                serde_json::json!(["fixture-user", [edit.clone()]]),
            ),
        )
        .await
        .unwrap();
        assert!(!saved.to_string().contains("synthetic-private"));
        for kind in ["environment", "headers"] {
            assert_eq!(
                saved.pointer(&format!("/0/{kind}/0/hasValue")),
                Some(&serde_json::Value::Bool(true))
            );
        }
        assert!(
            super::super::dispatch_remote_inner(
                &backend,
                request(
                    "save_mcp_definition_edits",
                    serde_json::json!(["other-user", [edit.clone()]])
                )
            )
            .await
            .is_err()
        );
        let mut keep = edit;
        keep["description"] = "Remote metadata edit".into();
        for kind in ["environment", "headers"] {
            keep[kind][0]["update"] = serde_json::json!({"operation":"keep"});
        }
        let updated = super::super::dispatch_remote_inner(
            &backend,
            request(
                "save_mcp_definition_edits",
                serde_json::json!(["fixture-user", [keep.clone()]]),
            ),
        )
        .await
        .unwrap();
        assert!(!updated.to_string().contains("synthetic-private"));
        let store = backend.store().unwrap();
        let account = store.account_id("fixture-user").unwrap().unwrap();
        let definition = store.mcp_definitions(account).unwrap().remove(0);
        let launch = store
            .mcp_launch_secrets(account, "example", &definition.target)
            .unwrap();
        assert_eq!(
            launch.environment["TOKEN"].expose(),
            "synthetic-private-value"
        );
        assert_eq!(
            launch.headers["authorization"].expose(),
            "synthetic-private-header"
        );
        drop(launch);

        let mut replace = keep;
        replace["environment"][0]["update"] =
            serde_json::json!({"operation":"set","value":"synthetic-replacement-value"});
        replace["headers"][0]["update"] =
            serde_json::json!({"operation":"set","value":"synthetic-replacement-header"});
        let replaced = super::super::dispatch_remote_inner(
            &backend,
            request(
                "save_mcp_definition_edits",
                serde_json::json!(["fixture-user", [replace.clone()]]),
            ),
        )
        .await
        .unwrap();
        assert!(!replaced.to_string().contains("synthetic-replacement"));
        let launch = store
            .mcp_launch_secrets(account, "example", &definition.target)
            .unwrap();
        assert_eq!(
            launch.environment["TOKEN"].expose(),
            "synthetic-replacement-value"
        );
        assert_eq!(
            launch.headers["authorization"].expose(),
            "synthetic-replacement-header"
        );
        drop(launch);

        for kind in ["environment", "headers"] {
            replace[kind][0]["update"] = serde_json::json!({"operation":"clear"});
        }
        let cleared = super::super::dispatch_remote_inner(
            &backend,
            request(
                "save_mcp_definition_edits",
                serde_json::json!(["fixture-user", [replace]]),
            ),
        )
        .await
        .unwrap();
        for kind in ["environment", "headers"] {
            assert_eq!(
                cleared.pointer(&format!("/0/{kind}/0/hasValue")),
                Some(&serde_json::Value::Bool(false))
            );
        }
        let listed = super::super::dispatch_remote_inner(
            &backend,
            request("list_mcp_definitions", serde_json::json!(["fixture-user"])),
        )
        .await
        .unwrap();
        assert_eq!(listed, cleared);
        assert!(
            store
                .mcp_launch_secrets(account, "example", &definition.target)
                .unwrap()
                .environment
                .is_empty()
        );
    }

    #[tokio::test]
    async fn project_mcp_operations_use_the_host_account_and_shared_settings_lock() {
        let root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let project = project.path().to_string_lossy().into_owned();
        let backend = LocalBackend::isolated_for_test(root.path());
        backend.persisted_auth.claim_and_persist(&auth_snapshot(
            "fixture-user",
            "fixture-session",
            1,
            "fixture-token",
        ));
        let request = |method: &str, params| crate::host::protocol::Request {
            id: 1,
            method: method.into(),
            params,
            send_id: None,
        };
        let server = |name: &str| maple_agent::agent::AgentMcpServer {
            name: name.into(),
            description: String::new(),
            enabled: true,
            timeout_seconds: 30,
            transport: maple_agent::agent::AgentMcpTransport::StreamableHttp {
                url: "http://127.0.0.1:1/mcp".into(),
                headers: Vec::new(),
                environment: Vec::new(),
            },
        };
        super::super::dispatch(
            &backend,
            request(
                "save_mcp_servers",
                serde_json::json!(["fixture-user", [server("Files"), server("Docs")]]),
            ),
        )
        .await
        .unwrap();

        // Local settings do not wait on credential validation. Two clients'
        // independent switches must not overwrite each other's sidecar edits.
        let _validation = backend.restore_lock.lock().await;
        let (files, docs) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(
                super::super::dispatch(
                    &backend,
                    request(
                        "set_project_mcp_server_enabled",
                        serde_json::json!(["fixture-user", project, "Files", false])
                    )
                ),
                super::super::dispatch(
                    &backend,
                    request(
                        "set_project_mcp_server_enabled",
                        serde_json::json!(["fixture-user", project, "Docs", false])
                    )
                )
            )
        })
        .await
        .expect("settings must not wait for credential validation");
        files.unwrap();
        docs.unwrap();
        let rows = super::super::dispatch(
            &backend,
            request(
                "list_project_mcp_servers",
                serde_json::json!(["fixture-user", project]),
            ),
        )
        .await
        .unwrap();
        let rows: Vec<maple_agent::agent::AgentMcpServer> = serde_json::from_value(rows).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| !row.enabled));

        for (method, params) in [
            (
                "list_project_mcp_servers",
                serde_json::json!(["different-user", project]),
            ),
            (
                "set_project_mcp_server_enabled",
                serde_json::json!(["different-user", project, "Files", true]),
            ),
        ] {
            assert!(
                super::super::dispatch(&backend, request(method, params))
                    .await
                    .unwrap_err()
                    .contains("account changed")
            );
        }
        assert!(
            backend
                .list_project_mcp_servers("different-user", &project)
                .await
                .unwrap()
                .is_empty(),
            "rejected writes must not create another account's defaults"
        );
        // Reopening host-owned storage retains both overrides without any
        // frontend-owned persistence or live authentication request.
        let reopened = LocalBackend::isolated_for_test(root.path());
        assert_eq!(
            reopened
                .list_project_mcp_servers("fixture-user", &project)
                .await
                .unwrap(),
            rows
        );
    }

    #[tokio::test]
    async fn checking_remote_status_does_not_create_identity_or_require_network() {
        let root = tempfile::tempdir().unwrap();
        let backend = Arc::new(LocalBackend::isolated_for_test(root.path()));
        backend.persisted_auth.claim_and_persist(&auth_snapshot(
            "fixture-user",
            "fixture-session",
            1,
            "fixture-token",
        ));
        let (events, _) = tokio::sync::broadcast::channel(8);
        let receipts = Arc::new(crate::host::receipts::Receipts::new(
            backend.store().unwrap().clone(),
        ));
        let remote = crate::remote::manager::Manager::new(backend.clone(), events, receipts);
        remote.restore().await;
        let status = remote
            .dispatch(crate::host::protocol::Request {
                id: 1,
                method: "remote_status".into(),
                params: serde_json::json!([]),
                send_id: None,
            })
            .await
            .unwrap();
        assert_eq!(status["enabled"], false);
        assert!(status["endpoint"].is_null());
        assert!(!root.path().join("remote").exists());
        assert!(!remote.stays_running());
        // Naming an offline computer is local even when authentication cannot
        // finish. It must not bind an endpoint or enable incoming access.
        let _validation = backend.restore_lock.lock().await;
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            remote.dispatch(crate::host::protocol::Request {
                id: 2,
                method: "remote_set_name".into(),
                params: serde_json::json!(["Offline Mac"]),
                send_id: None,
            }),
        )
        .await
        .unwrap()
        .unwrap();
        let status = remote
            .dispatch(crate::host::protocol::Request {
                id: 3,
                method: "remote_status".into(),
                params: serde_json::json!([]),
                send_id: None,
            })
            .await
            .unwrap();
        assert_eq!(status["name"], "Offline Mac");
        assert_eq!(status["ready"], false);
        assert_eq!(status["enabled"], false);
        assert!(!remote.stays_running());
    }

    #[tokio::test]
    async fn disabling_remote_does_not_wait_for_initializing_credentials() {
        use std::{future::Future, task::Poll};
        let root = tempfile::tempdir().unwrap();
        let backend = Arc::new(LocalBackend::isolated_for_test(root.path()));
        backend.persisted_auth.claim_and_persist(&auth_snapshot(
            "fixture-user",
            "fixture-session",
            1,
            "fixture-token",
        ));
        let (events, _) = tokio::sync::broadcast::channel(8);
        let receipts = Arc::new(crate::host::receipts::Receipts::new(
            backend.store().unwrap().clone(),
        ));
        let remote = crate::remote::manager::Manager::new(backend.clone(), events, receipts);
        let request = |method: &str, params| crate::host::protocol::Request {
            id: 1,
            method: method.into(),
            params,
            send_id: None,
        };
        remote
            .dispatch(request("remote_enable", serde_json::json!([true])))
            .await
            .unwrap();
        let validation = backend.restore_lock.lock().await;
        let active = remote.active();
        tokio::pin!(active);
        std::future::poll_fn(|cx| {
            assert!(active.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            remote.dispatch(request("remote_enable", serde_json::json!([false]))),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!remote.stays_running());
        let status = remote
            .dispatch(request("remote_status", serde_json::json!([])))
            .await
            .unwrap();
        assert_eq!(status["enabled"], false);
        assert_eq!(status["ready"], false);
        drop(validation);
        // Invalid fixture credentials cannot create a node after the cancellation.
        assert!(active.await.is_err());
        remote.shutdown().await;
    }

    #[tokio::test]
    async fn credential_validation_and_acp_attachment_do_not_block_local_operations() {
        use std::{future::Future, task::Poll};

        let root = tempfile::tempdir().unwrap();
        let backend = LocalBackend::isolated_for_test(root.path());
        backend.persisted_auth.claim_and_persist(&auth_snapshot(
            "fixture-user",
            "fixture-session",
            1,
            "fixture-token",
        ));
        let request = |method: &str, params| crate::host::protocol::Request {
            id: 1,
            method: method.into(),
            params,
            send_id: None,
        };

        // Hold the credential-validation boundary rather than relying on a
        // network timeout. Both entry points have acquired their account guard
        // by the time this poll returns Pending.
        let _validation = backend.restore_lock.lock().await;
        let restore =
            super::super::dispatch(&backend, request("restore_auth", serde_json::Value::Null));
        tokio::pin!(restore);
        std::future::poll_fn(|cx| {
            assert!(restore.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        #[cfg(feature = "acp")]
        let mut attach = Box::pin(backend.serve_acp(tokio::io::empty(), tokio::io::sink()));
        #[cfg(feature = "acp")]
        std::future::poll_fn(|cx| {
            assert!(attach.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;

        for method in ["navigation_bootstrap", "stop_runtime"] {
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                super::super::dispatch(
                    &backend,
                    request(method, serde_json::json!(["fixture-user"])),
                ),
            )
            .await
            .expect("local work must not wait for credential validation")
            .unwrap();
        }
        assert!(
            super::super::dispatch(
                &backend,
                request(
                    "navigation_bootstrap",
                    serde_json::json!(["different-user"])
                ),
            )
            .await
            .unwrap_err()
            .contains("account changed")
        );

        // Real account changes must still wait; releasing the credential
        // serialization alone cannot let a switch overtake admitted work.
        let switch =
            super::super::dispatch(&backend, request("login", serde_json::json!(["", ""])));
        tokio::pin!(switch);
        std::future::poll_fn(|cx| {
            assert!(switch.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
    }

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
    async fn hosted_deadline_cancels_its_attempt_but_not_a_replacement() {
        let store = Arc::new(PendingOAuthStore::default());
        let expired = store.begin(OAuthProvider::Google, oauth_test_client());
        store.expire_handoff_after(&expired, std::time::Duration::from_millis(10));
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            expired.while_active(std::future::pending::<Result<(), String>>()),
        )
        .await
        .unwrap();
        assert_eq!(result, Err(OAUTH_CANCELLED_MESSAGE.to_string()));
        assert!(store.0.lock().unwrap().is_none());

        let old = store.begin(OAuthProvider::Google, oauth_test_client());
        store.expire_handoff_after(&old, std::time::Duration::from_millis(10));
        let replacement = store.begin(OAuthProvider::Apple, oauth_test_client());
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!*replacement.cancelled.borrow());
        assert!(Arc::ptr_eq(
            &store.0.lock().unwrap().as_ref().unwrap().attempt,
            &replacement
        ));
        // Waiting on the hosted path cannot claim or consume a legacy attempt.
        assert!(store.take_handoff(OAuthProvider::Apple).is_err());
        store
            .set_state(&replacement, "fixture-state".to_string())
            .unwrap();
        assert!(
            store
                .complete(OAuthProvider::Apple, "fixture-state")
                .is_ok()
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

    fn persisted_auth_fixture() -> (
        tempfile::TempDir,
        Arc<maple_agent::store::Store>,
        PersistedAuthStore,
    ) {
        let root = tempfile::tempdir().unwrap();
        let database = Arc::new(
            maple_agent::store::Store::open(maple_agent::store::StorePaths::new(root.path()))
                .unwrap(),
        );
        let credentials =
            PersistedAuthStore::new(Some(database.clone()), "https://api.example".to_owned());
        (root, database, credentials)
    }

    #[test]
    fn persisted_auth_compare_clear_preserves_a_new_session_and_rejects_late_rotation() {
        let (_root, database, store) = persisted_auth_fixture();
        let old = auth_snapshot("same-user", "session-old", 2, "old-token");
        let new = auth_snapshot("same-user", "session-new", 1, "new-token");
        assert!(store.claim_and_persist(&old));
        let mut newer_old = old.clone();
        newer_old.revision = 3;
        newer_old.access_token = "newer-old-token".to_owned();
        store.persist_rotation(&newer_old);
        assert!(!store.claim_and_persist(&old));
        assert_eq!(store.read_record().unwrap().access_token, "newer-old-token");
        assert!(store.claim_and_persist(&new));
        assert!(store.clear_if_owned(&old));
        assert_eq!(store.read_record().unwrap().access_token, "new-token");
        store.persist_rotation(&newer_old);
        assert_eq!(store.read_record().unwrap().access_token, "new-token");
        assert!(store.clear_if_owned(&new));
        let mut late_new = new;
        late_new.revision = 2;
        store.persist_rotation(&late_new);
        assert!(
            store.read_record().is_none(),
            "post-logout callback must not restore credentials"
        );
        database.close().unwrap();
    }

    #[test]
    fn explicit_sign_in_can_persist_after_an_api_origin_change() {
        let (_root, database, original) = persisted_auth_fixture();
        let old = auth_snapshot("old-user", "old-session", 3, "old-token");
        let new = auth_snapshot("new-user", "new-session", 1, "new-token");
        assert!(original.claim_and_persist(&old));
        let current = PersistedAuthStore::new(
            Some(Arc::clone(&database)),
            "https://new.example".to_owned(),
        );
        assert!(current.load().is_none());
        assert!(current.claim_and_persist(&new));
        current.retry_without_sdk().unwrap();
        original.persist_rotation(&auth_snapshot("old-user", "old-session", 4, "late-token"));
        assert!(original.clear_if_owned(&old));
        let restored = PersistedAuthStore::new(
            Some(Arc::clone(&database)),
            "https://new.example".to_owned(),
        )
        .load()
        .unwrap();
        assert_eq!(restored.user_id, "new-user");
        assert_eq!(restored.access_token, "new-token");
        assert!(
            database
                .credentials("https://api.example")
                .unwrap()
                .is_none()
        );
        database.close().unwrap();
    }

    #[test]
    fn failed_origin_claim_retries_only_the_slot_it_observed() {
        let (_root, database, original) = persisted_auth_fixture();
        let old = auth_snapshot("old-user", "old-session", 1, "old-token");
        let new = auth_snapshot("new-user", "new-session", 2, "new-token");
        assert!(original.claim_and_persist(&old));
        let target = database.upsert_account("new-user", 1).unwrap();
        let generation = Uuid::new_v4().to_string();
        database
            .begin_account_deletion(target, &generation, 2)
            .unwrap();
        let current = PersistedAuthStore::new(
            Some(Arc::clone(&database)),
            "https://new.example".to_owned(),
        );
        // The slot is observed successfully, but target-account admission
        // rejects the actual write. Retry retains the old origin's exact CAS.
        assert!(!current.claim_and_persist(&new));
        assert!(current.retry_without_sdk().is_err());
        database
            .abort_account_deletion(target, &generation)
            .unwrap();
        current.retry_snapshot(&new).unwrap();
        assert_eq!(current.read_record().unwrap().access_token, "new-token");
        assert!(original.retry_snapshot(&old).is_err());
        database.close().unwrap();
    }

    #[test]
    fn failed_origin_claim_cannot_retry_over_an_independent_replacement() {
        let (_root, database, original) = persisted_auth_fixture();
        let old = auth_snapshot("old-user", "old-session", 1, "old-token");
        let new = auth_snapshot("new-user", "new-session", 2, "new-token");
        let independent = auth_snapshot("third-user", "third-session", 1, "third-token");
        assert!(original.claim_and_persist(&old));
        let target = database.upsert_account("new-user", 1).unwrap();
        let generation = Uuid::new_v4().to_string();
        database
            .begin_account_deletion(target, &generation, 2)
            .unwrap();
        let current = PersistedAuthStore::new(
            Some(Arc::clone(&database)),
            "https://new.example".to_owned(),
        );
        assert!(!current.claim_and_persist(&new));
        database
            .abort_account_deletion(target, &generation)
            .unwrap();
        assert!(original.claim_and_persist(&independent));
        assert!(current.retry_snapshot(&new).is_err());
        current.persist_rotation(&new);
        assert_eq!(original.read_record().unwrap().access_token, "third-token");
        assert!(current.clear_if_owned(&new));
        assert_eq!(original.read_record().unwrap().access_token, "third-token");
        database.close().unwrap();
    }

    #[tokio::test]
    async fn confirmed_deletion_allows_a_saved_sign_in_and_shutdown_before_collection() {
        let root = tempfile::tempdir().unwrap();
        let backend = LocalBackend::isolated_for_test(root.path());
        backend.service.begin_draining();
        let database = backend.store.as_ref().unwrap();
        let old = auth_snapshot("old-user", "old-session", 1, "old-token");
        let new = auth_snapshot("new-user", "new-session", 2, "new-token");
        assert!(backend.persisted_auth.claim_and_persist(&old));
        let account = database.account_id("old-user").unwrap().unwrap();
        database
            .create_task(
                account,
                &maple_agent::store::NewTask {
                    uuid: Uuid::new_v4().to_string(),
                    project_root: root.path().to_string_lossy().into_owned(),
                    kind: maple_agent::store::TaskKind::Desktop,
                    title: "retained history".into(),
                    provisional: false,
                    model: None,
                    created_ms: 1,
                },
            )
            .unwrap();
        let generation = Uuid::new_v4().to_string();
        database
            .begin_account_deletion(account, &generation, 2)
            .unwrap();
        database
            .confirm_account_deletion(account, &generation)
            .unwrap();
        backend.persisted_auth.forget_owner();
        assert!(matches!(
            database.collect_account_step(account),
            Err(maple_agent::store::StoreError::Busy)
        ));
        backend.require_signed_out().unwrap();
        assert!(backend.persisted_auth.claim_and_persist(&new));
        backend.shutdown_host().await.unwrap();
        let restored = PersistedAuthStore::new(Some(Arc::clone(database)), backend.api_url.clone())
            .load()
            .unwrap();
        assert_eq!(restored.user_id, "new-user");
        assert_eq!(restored.access_token, "new-token");
        database.close().unwrap();
    }

    #[tokio::test]
    async fn pending_deletion_at_another_origin_prevents_installing_a_new_live_session() {
        let root = tempfile::tempdir().unwrap();
        let backend = LocalBackend::isolated_for_test(root.path());
        let database = backend.store.as_ref().unwrap();
        let old = auth_snapshot("old-user", "old-session", 1, "old-token");
        let foreign =
            PersistedAuthStore::new(Some(Arc::clone(database)), "https://old.example".into());
        assert!(foreign.claim_and_persist(&old));
        let account = database.account_id("old-user").unwrap().unwrap();
        let generation = Uuid::new_v4().to_string();
        database
            .begin_account_deletion(account, &generation, 2)
            .unwrap();
        assert!(backend.pending_deletion_status().unwrap().is_none());
        assert_eq!(
            backend.require_signed_out().unwrap_err(),
            deletion::PENDING_DELETION_MESSAGE
        );
        assert!(backend.persisted_auth.live_identity().is_none());
        assert_eq!(
            database
                .pending_deletion_credentials(account, &generation, "https://old.example")
                .unwrap()
                .unwrap()
                .owner
                .session_id,
            "old-session"
        );
        database.close().unwrap();
    }

    #[test]
    fn restored_auth_cannot_adopt_an_independent_new_owner() {
        let (_root, database, original) = persisted_auth_fixture();
        let old = auth_snapshot("old-user", "old-session", 1, "old-token");
        assert!(original.claim_and_persist(&old));
        let loaded = original.load().unwrap();
        let replacement =
            PersistedAuthStore::new(Some(Arc::clone(&database)), "https://api.example".into());
        assert!(replacement.claim_and_persist(&auth_snapshot(
            "new-user",
            "new-session",
            1,
            "new-token"
        )));
        assert!(!original.persist_restored(
            &auth_snapshot("old-user", "restored-session", 1, "restored-token"),
            &loaded
        ));
        assert!(
            original
                .retry_snapshot(&auth_snapshot(
                    "old-user",
                    "restored-session",
                    1,
                    "restored-token"
                ))
                .is_err()
        );
        assert_eq!(replacement.read_record().unwrap().access_token, "new-token");
        database.close().unwrap();
    }

    #[test]
    fn restored_auth_persists_a_new_sdk_session_using_only_its_loaded_owner() {
        let (_root, database, store) = persisted_auth_fixture();
        let old = auth_snapshot("fixture-user", "old-session", 3, "old-token");
        assert!(store.claim_and_persist(&old));
        let loaded = store.load().unwrap();
        let restored = auth_snapshot("fixture-user", "restored-session", 1, "restored-token");
        assert!(store.persist_restored(&restored, &loaded));
        let saved = store.read_record().unwrap();
        assert_eq!(saved.owner.session_id, "restored-session");
        assert_eq!(saved.owner.revision, 1);
        assert_eq!(saved.access_token, "restored-token");
        database.close().unwrap();
    }

    #[test]
    fn persistence_reports_write_failure_and_successful_restore() {
        let root = tempfile::tempdir().unwrap();
        let database = Arc::new(
            maple_agent::store::Store::unavailable(
                maple_agent::store::StorePaths::new(root.path()),
                maple_agent::store::StoreState::NeedsSetup,
            )
            .unwrap(),
        );
        let store =
            PersistedAuthStore::new(Some(database.clone()), "https://api.example".to_owned());
        let snapshot = auth_snapshot("fixture-user", "fixture-session", 1, "fixture-token");
        assert!(!store.claim_and_persist(&snapshot));
        assert!(store.load().is_none());
        assert!(
            store.state.lock().unwrap().owner.is_none(),
            "failed write cannot claim durable ownership"
        );
        database.initialize_plain().unwrap();
        let mut jump = snapshot.clone();
        jump.revision = 4;
        store.persist_rotation(&jump);
        assert!(
            store.load().is_none(),
            "a callback cannot claim a failed sign-in write"
        );
        assert!(store.claim_and_persist(&snapshot));
        store.persist_rotation(&jump);
        let restored =
            PersistedAuthStore::new(Some(database.clone()), "https://api.example".to_owned())
                .load()
                .unwrap();
        assert_eq!(restored.user_id, snapshot.user_id);
        assert_eq!(
            restored.owner.revision, 4,
            "persist the SDK revision, including jumps"
        );
        assert_eq!(restored.access_token, snapshot.access_token);
        assert!(!format!("{restored:?}").contains("fixture-token"));
        database.close().unwrap();
    }

    #[test]
    fn explicit_retry_saves_the_live_replacement_after_a_failed_write() {
        let (_root, database, store) = persisted_auth_fixture();
        let old = auth_snapshot("old-user", "old-session", 1, "old-token");
        let new = auth_snapshot("new-user", "new-session", 4, "new-token");
        assert!(store.claim_and_persist(&old));
        database.close().unwrap();
        assert!(!store.claim_and_persist(&new));
        database.initialize_plain().unwrap();
        assert!(
            store.retry_without_sdk().is_err(),
            "a durable old owner cannot stand in for the unsaved live session"
        );
        store.retry_snapshot(&new).unwrap();
        assert!(store.retry_snapshot(&old).is_err());
        let saved = store.read_record().unwrap();
        assert_eq!(saved.user_id, "new-user");
        assert_eq!(saved.owner.revision, 4);
        assert_eq!(saved.access_token, "new-token");
        database.close().unwrap();
    }

    #[test]
    fn exact_snapshot_retry_saves_a_failed_rotation_with_a_revision_jump() {
        let (_root, database, store) = persisted_auth_fixture();
        let original = auth_snapshot("fixture-user", "fixture-session", 1, "old-token");
        let rotated = auth_snapshot("fixture-user", "fixture-session", 7, "rotated-token");
        assert!(store.claim_and_persist(&original));
        database.close().unwrap();
        store.persist_rotation(&rotated);
        assert_eq!(
            store
                .state
                .lock()
                .unwrap()
                .owner
                .as_ref()
                .unwrap()
                .durable
                .revision,
            1,
            "a failed write must retain the last durable CAS owner"
        );
        database.initialize_plain().unwrap();
        assert_eq!(store.read_record().unwrap().access_token, "old-token");
        store.retry_snapshot(&rotated).unwrap();
        let saved = store.read_record().unwrap();
        assert_eq!(saved.owner.revision, 7);
        assert_eq!(saved.access_token, "rotated-token");
        assert!(store.retry_snapshot(&original).is_err());
        database.close().unwrap();
    }

    #[tokio::test]
    async fn shutdown_refuses_an_unsaved_live_replacement_until_exact_retry_succeeds() {
        let root = tempfile::tempdir().unwrap();
        let backend = LocalBackend::isolated_for_test(root.path());
        let database = backend.store.as_ref().unwrap();
        let old = auth_snapshot("old-user", "old-session", 1, "old-token");
        let new = auth_snapshot("new-user", "new-session", 4, "new-token");
        assert!(backend.persisted_auth.claim_and_persist(&old));
        database.close().unwrap();
        assert!(!backend.persisted_auth.claim_and_persist(&new));
        database.initialize_plain().unwrap();
        backend.begin_store_drain();
        assert!(backend.shutdown_host().await.is_err());
        assert_eq!(database.state(), maple_agent::store::StoreState::Open);
        assert_eq!(
            backend.persisted_auth.saved_user_id().as_deref(),
            Some("new-user")
        );
        assert_eq!(
            backend.persisted_auth.read_record().unwrap().user_id,
            "old-user"
        );
        backend.resume_store_admission();
        backend.persisted_auth.retry_snapshot(&new).unwrap();
        backend.shutdown_host().await.unwrap();
        let saved = backend.persisted_auth.read_record().unwrap();
        assert_eq!(saved.user_id, "new-user");
        assert_eq!(saved.owner.revision, 4);
        database.close().unwrap();
    }

    #[tokio::test]
    async fn shutdown_completes_a_retained_exact_logout_clear() {
        let root = tempfile::tempdir().unwrap();
        let backend = LocalBackend::isolated_for_test(root.path());
        let database = backend.store.as_ref().unwrap();
        let snapshot = auth_snapshot("fixture-user", "fixture-session", 1, "fixture-token");
        assert!(backend.persisted_auth.claim_and_persist(&snapshot));
        database.close().unwrap();
        assert!(!backend.persisted_auth.clear_if_owned(&snapshot));
        database.initialize_plain().unwrap();
        backend.shutdown_host().await.unwrap();
        assert!(backend.persisted_auth.read_record().is_none());
        assert!(
            backend
                .persisted_auth
                .state
                .lock()
                .unwrap()
                .pending_clear
                .is_none()
        );
        database.close().unwrap();
    }

    #[test]
    fn explicit_retry_cannot_adopt_an_independent_replacement_owner() {
        let (_root, database, store) = persisted_auth_fixture();
        let old = auth_snapshot("old-user", "old-session", 1, "old-token");
        let new = auth_snapshot("new-user", "new-session", 1, "new-token");
        assert!(store.claim_and_persist(&old));
        let saved = database
            .credentials("https://api.example")
            .unwrap()
            .unwrap();
        let account = database.upsert_account(&new.user_id, 1).unwrap();
        database
            .replace_credentials(
                Some(&saved.owner),
                account,
                "https://api.example",
                &new.session_id,
                new.revision,
                &PersistedAuthStore::tokens(&new),
                1,
            )
            .unwrap();
        assert!(store.retry_snapshot(&old).is_err());
        assert_eq!(store.read_record().unwrap().user_id, "new-user");
        database.close().unwrap();
    }

    #[test]
    fn explicit_retry_finishes_only_the_failed_logout_owner() {
        let (_root, database, store) = persisted_auth_fixture();
        let snapshot = auth_snapshot("fixture-user", "fixture-session", 1, "fixture-token");
        assert!(store.claim_and_persist(&snapshot));
        database.close().unwrap();
        assert!(!store.clear_if_owned(&snapshot));
        database.initialize_plain().unwrap();
        store.retry_without_sdk().unwrap();
        assert!(store.read_record().is_none());
        database.close().unwrap();
    }

    #[test]
    fn rejected_restore_clears_only_the_record_that_was_loaded() {
        let (_root, database, store) = persisted_auth_fixture();
        let old = auth_snapshot("old-user", "session-old", 1, "old-token");
        let new = auth_snapshot("new-user", "session-new", 1, "new-token");
        assert!(store.claim_and_persist(&old));
        let loaded = store.load().unwrap();
        assert!(store.claim_and_persist(&new));
        assert!(store.clear_record_if_unchanged(&loaded));
        assert_eq!(store.read_record().unwrap().user_id, "new-user");
        // A logout snapshot may precede the final SDK rotation. Delete using
        // the owner's latest durable revision under the adapter mutex.
        let mut rotated = new.clone();
        rotated.revision = 3;
        store.persist_rotation(&rotated);
        assert!(store.clear_if_owned(&new));
        assert!(store.read_record().is_none());
        database.close().unwrap();
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
    #[test]
    fn versioned_store_paths_do_not_adopt_previous_data() {
        let fixture = tempfile::tempdir().unwrap();
        let previous = fixture.path().join("profile");
        std::fs::create_dir_all(&previous).unwrap();
        std::fs::write(previous.join("auth.json"), b"previous sentinel").unwrap();
        let paths = maple_agent::store::StorePaths::new(&previous);
        assert_eq!(paths.root(), previous.join("store-v1"));
        assert_eq!(paths.resources(), previous.join("store-v1/resources"));
        assert_eq!(paths.logs(), previous.join("store-v1/logs"));
        assert!(!paths.root().exists());
        assert_eq!(
            std::fs::read(previous.join("auth.json")).unwrap(),
            b"previous sentinel"
        );
    }
}
