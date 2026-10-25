//! Maple's agent runtime on the Pi crates.
//!
//! The runtime turns Maple's task model (an account, its projects and tasks,
//! one run per task, a desktop queue) into Pi agent sessions, and Pi's events
//! back into the timeline and run events Maple's surfaces render.
//!
//! A host builds one [`MapleAgentService`] and asks it for an
//! [`AgentRuntimeHandle`] per signed-in account. The handle starts the
//! account's runtime, and every task operation goes through it. Tasks are
//! listed, read, renamed and deleted whether or not the runtime runs; runs
//! need it.

mod attachments;
mod bounded_process;
mod catalog;
mod config;
mod cua;
mod external_agents;
mod integrations;
mod login_path;
mod maintenance;
mod mcp;
mod persistence;
mod projects;
pub(crate) mod provider;
mod questions;
mod resources;
mod runs;
mod runtime;
mod side_models;
mod store;
mod surface;
mod tasks;
mod thinking;
pub(crate) mod timeline;
mod tools;
mod types;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, RwLock};

use crate::maple_api::{MapleApiSession, account_scope};
pub use attachments::{AgentAttachment, AgentImageUpload};
use config::*;
pub use config::{
    MAPLE_WORKSPACE_DIRECTORY_NAME, MAPLE_WORKSPACE_DISPLAY_NAME, default_maple_workspace_path,
    ensure_default_maple_workspace, include_default_maple_workspace, is_default_maple_workspace,
    maple_workspace_directory, startup_project_root,
};
pub use integrations::begin_integration_setup;
pub(crate) use projects::owner_project_display_name;
pub use provider::CatalogEntry;
pub use questions::QuestionResponder;
pub use runs::AgentRunHandle;
use runtime::AgentRuntime;
use store::TaskStore;
pub use surface::{
    AGENT_SURFACE_INACTIVE_ERROR, AgentSurfaceAccess, AgentSurfaceLease, AgentSurfaceSession,
};
pub use tasks::NOTHING_TO_COMPACT_ERROR;
pub use types::*;

/// The model a new account starts on.
const DEFAULT_AGENT_MODEL: &str = "glm-5-3";
/// Earlier defaults; an account still saved with one moves to the current one.
const LEGACY_AGENT_DEFAULT_MODEL: &str = "auto:powerful";
const PREVIOUS_RECOMMENDED_AGENT_MODEL: &str = "glm-5-2";
const DEFAULT_MCP_TIMEOUT_SECONDS: u64 = 300;

const AGENT_SERVICE_OPEN: u8 = 0;
const AGENT_SERVICE_DRAINING: u8 = 1;
const AGENT_SERVICE_DRAINING_ERROR: &str =
    "Maple Work services are draining and cannot accept new work";
const RUNTIME_NOT_RUNNING_ERROR: &str = "Agent runtime is not running";
const RUNTIME_SAVE_PENDING_ERROR: &str =
    "Agent history is waiting to be saved. Retry saving before starting more work.";

/// File resources are separate from app state. The store owns settings and
/// history; these paths contain only generated skills and user Markdown.
#[derive(Clone)]
pub struct AgentPathLayout {
    generated_resource_root: PathBuf,
    instruction_config_root: PathBuf,
    store: Option<Arc<crate::store::Store>>,
    home: Option<PathBuf>,
}

impl AgentPathLayout {
    pub fn from_resource_root(resource_root: PathBuf) -> Self {
        Self {
            generated_resource_root: resource_root.clone(),
            instruction_config_root: resource_root,
            store: None,
            home: config::home_dir(),
        }
    }

    /// Existing synthetic fixtures may name two roots. Runtime callers use
    /// the explicit resource-only constructor and an injected SQL store.
    #[cfg(test)]
    pub(crate) fn from_app_roots(app_config_root: PathBuf, _app_local_data_root: PathBuf) -> Self {
        Self::from_resource_root(app_config_root.join("agent"))
    }

    /// Keep user-authored Markdown, prompts and skills at their existing path,
    /// independently of the fresh app-state roots. This never creates that path.
    pub fn with_instruction_config_root(mut self, app_config_root: PathBuf) -> Self {
        self.instruction_config_root = app_config_root.join("agent");
        self
    }

    pub fn instruction_config_root(&self) -> &Path {
        &self.instruction_config_root
    }

    /// Bind app-owned settings to the same host store as task history.
    pub fn with_store(mut self, store: Arc<crate::store::Store>) -> Self {
        self.generated_resource_root = store.paths().resources();
        self.store = Some(store);
        self
    }

    pub(crate) fn store(&self) -> Result<&Arc<crate::store::Store>, String> {
        self.store
            .as_ref()
            .ok_or_else(|| "The host has not attached its local store".to_string())
    }

    pub(crate) fn home(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    /// The same layout with another home folder, so a test reads none of
    /// the real one's.
    #[cfg(test)]
    pub(crate) fn with_home(mut self, home: Option<PathBuf>) -> Self {
        self.home = home;
        self
    }
}

/// Receives every event the runtime publishes to its host.
pub trait AgentEventSink: Send + Sync + 'static {
    fn emit(&self, event: &AgentServiceEvent);
}

#[derive(Clone)]
pub(crate) struct AgentEventDispatcher {
    sink: Arc<dyn AgentEventSink>,
    subscribers: tokio::sync::broadcast::Sender<AgentServiceEvent>,
}

impl AgentEventDispatcher {
    pub(crate) fn new(sink: Arc<dyn AgentEventSink>) -> Self {
        Self {
            sink,
            subscribers: tokio::sync::broadcast::channel(256).0,
        }
    }
}

pub(crate) fn emit_agent_event(events: &AgentEventDispatcher, event: AgentServiceEvent) {
    let _ = events.subscribers.send(event.clone());
    events.sink.emit(&event);
}

/// What the host gives the runtime: where to keep files, where to send
/// events, and the opening of the system prompt.
#[derive(Clone)]
pub struct MapleAgentHostResources {
    paths: AgentPathLayout,
    store: Option<Arc<crate::store::Store>>,
    events: AgentEventDispatcher,
    /// A readonly integrity job yields immediately when task work is admitted.
    /// Runtime clones share this host-local cancellation slot.
    integrity_check:
        Arc<std::sync::Mutex<Option<Arc<crate::store::integrity::IntegrityCancellation>>>>,
    integrity_idle: Arc<tokio::sync::Notify>,
    /// Opening system prompt text from the host: who the agent is and how
    /// it behaves. The host may change it at any time; sessions built
    /// afterwards use the new text.
    harness_instructions: Arc<RwLock<String>>,
}

impl MapleAgentHostResources {
    pub fn new(
        paths: AgentPathLayout,
        event_sink: Arc<dyn AgentEventSink>,
        harness_instructions: String,
    ) -> Self {
        Self {
            paths,
            store: None,
            events: AgentEventDispatcher::new(event_sink),
            integrity_check: Arc::new(std::sync::Mutex::new(None)),
            integrity_idle: Arc::new(tokio::sync::Notify::new()),
            harness_instructions: Arc::new(RwLock::new(harness_instructions)),
        }
    }

    /// Inject the one store opened and owned by the host. Runtime construction
    /// never opens a second connection set or falls back to previous files.
    pub fn with_store(mut self, store: Arc<crate::store::Store>) -> Self {
        self.paths = self.paths.with_store(Arc::clone(&store));
        self.store = Some(store);
        self
    }

    fn harness_instructions(&self) -> String {
        self.harness_instructions
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(super) fn cancel_integrity_check(&self) {
        if let Some(check) = self
            .integrity_check
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            check.cancel();
        }
    }

    async fn wait_integrity_idle(&self) {
        loop {
            let idle = self.integrity_idle.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();
            if self
                .integrity_check
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_none()
            {
                return;
            }
            idle.await;
        }
    }
}

/// The runtime service of one host process. At most one account's runtime
/// runs at a time.
#[derive(Clone)]
pub struct MapleAgentService {
    host: MapleAgentHostResources,
    state: Arc<ServiceState>,
}

struct ServiceState {
    /// The running runtime, if any.
    runtime: tokio::sync::Mutex<Option<Arc<AgentRuntime>>>,
    /// A stopped runtime retains its sessions until their final writes succeed.
    /// It is never returned to callers as a runnable runtime.
    shutdown_pending: tokio::sync::Mutex<Option<PendingRuntimeShutdown>>,
    /// Orders start, stop and restart.
    lifecycle: tokio::sync::Mutex<()>,
    /// Held while the account's settings are read and saved, so a save cannot
    /// overwrite a change made since its read. Taken after `lifecycle`.
    settings: tokio::sync::Mutex<()>,
    /// Each account's task index, bound to its current store and row owner.
    stores: std::sync::Mutex<HashMap<String, Arc<TaskStore>>>,
    /// Bumped when an account's data is cleared, which revokes the handles
    /// issued before.
    generations: std::sync::Mutex<HashMap<String, u64>>,
    admission: AtomicU8,
    /// The question broker this service installed as the process global.
    questions: questions::QuestionBroker,
}

#[derive(Clone)]
struct PendingRuntimeShutdown {
    runtime: Arc<AgentRuntime>,
    completed: tokio::sync::watch::Receiver<Option<Result<(), String>>>,
}

impl PendingRuntimeShutdown {
    fn new(runtime: Arc<AgentRuntime>) -> Self {
        let (finished, completed) = tokio::sync::watch::channel(None);
        let draining = runtime.clone();
        // A disconnected caller must not abort the tool/session drain or lose
        // retained source history. Repeated retries await this same drain;
        // they never emit another Pi session_shutdown for the old sessions.
        tokio::spawn(async move {
            let _ = finished.send_replace(Some(draining.shutdown().await));
        });
        Self { runtime, completed }
    }

    async fn wait(&self) -> Result<Result<(), String>, String> {
        let mut completed = self.completed.clone();
        let result = completed
            .wait_for(Option::is_some)
            .await
            .map_err(|_| {
                "Agent shutdown did not complete. Keep Maple open before retrying.".to_string()
            })?
            .clone();
        result.ok_or_else(|| "Agent shutdown has not completed".to_string())
    }
}

/// One account's view of the service. Hosts create one per operation, or
/// keep one for a long-lived connection; clearing the account's data
/// revokes it.
#[derive(Clone)]
pub struct AgentRuntimeHandle {
    service: MapleAgentService,
    user_id: Arc<str>,
    account_scope: Arc<str>,
    generation: u64,
    store_generation: Option<String>,
}

impl MapleAgentService {
    pub fn new(host: MapleAgentHostResources) -> Self {
        let questions = questions::init_global(host.events.clone());
        let service = Self {
            host,
            state: Arc::new(ServiceState {
                runtime: tokio::sync::Mutex::new(None),
                shutdown_pending: tokio::sync::Mutex::new(None),
                lifecycle: tokio::sync::Mutex::new(()),
                settings: tokio::sync::Mutex::new(()),
                stores: std::sync::Mutex::new(HashMap::new()),
                generations: std::sync::Mutex::new(HashMap::new()),
                admission: AtomicU8::new(AGENT_SERVICE_OPEN),
                questions,
            }),
        };
        maintenance::start(&service);
        persistence::start(&service);
        service
    }

    pub fn manage_question_availability(&self) {
        self.state.questions.manage_availability();
    }
    pub fn question_responder(&self) -> QuestionResponder {
        self.state.questions.responder()
    }
    pub async fn pending_questions(&self) -> Vec<AgentServiceEvent> {
        self.state.questions.snapshot().await
    }

    /// Bind operations to one Maple account and its current data generation.
    pub async fn handle_for_user(&self, user_id: &str) -> Result<AgentRuntimeHandle, String> {
        let account_scope = account_scope(user_id)?;
        let generation = self.generation(&account_scope);
        Ok(AgentRuntimeHandle {
            service: self.clone(),
            user_id: Arc::from(user_id),
            account_scope: Arc::from(account_scope),
            generation,
            store_generation: self.store_generation(),
        })
    }

    fn generation(&self, account_scope: &str) -> u64 {
        *self
            .state
            .generations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(account_scope.to_string())
            .or_default()
    }

    fn store_generation(&self) -> Option<String> {
        self.host
            .store
            .as_ref()
            .and_then(|store| store.persistence_status().store_id)
    }

    /// Revoke account views after a successful destructive store reset. The
    /// host must finish draining work before resetting; failed or cancelled
    /// operations keep their current handles and cached source identities.
    pub fn invalidate_store_session(&self) {
        let mut generations = self
            .state
            .generations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for generation in generations.values_mut() {
            *generation += 1;
        }
        drop(generations);
        if let Some(store) = &self.host.store {
            store.revoke_all_replays();
        }
        let mut stores = self
            .state
            .stores
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for store in stores.values() {
            store.revoke();
        }
        stores.clear();
    }

    /// Deliver the user's answer to a pending question.
    pub async fn answer_question(&self, request_id: &str, answer: String) -> bool {
        // The question tool registers its question in the process-global
        // broker. This service installed its own broker as that global, so
        // answer through it first, then through a newer one that replaced
        // it, or the run would wait forever.
        if self
            .state
            .questions
            .answer(request_id, answer.clone())
            .await
        {
            return true;
        }
        match questions::global() {
            Some(broker) if !broker.same_as(&self.state.questions) => {
                broker.answer(request_id, answer).await
            }
            _ => false,
        }
    }

    /// The slash commands of `user_id`'s skills and prompt templates in
    /// `working_dir`. Reads the disk.
    pub fn list_slash_commands(
        &self,
        user_id: Option<&str>,
        working_dir: Option<&str>,
    ) -> Vec<AgentSlashCommand> {
        let Some(user_id) = user_id else {
            return Vec::new();
        };
        resources::slash_commands(&self.host.paths, user_id, working_dir.map(Path::new))
    }

    /// Expand `/command args` into the prompt that runs the command. `None`
    /// when no skill or prompt template has that name. Reads the disk.
    pub fn resolve_slash_command(
        &self,
        user_id: Option<&str>,
        working_dir: Option<&str>,
        command: &str,
        args: &str,
    ) -> Result<Option<String>, String> {
        let Some(user_id) = user_id else {
            return Ok(None);
        };
        resources::resolve_slash_command(
            &self.host.paths,
            user_id,
            working_dir.map(Path::new),
            command,
            args,
        )
    }

    /// Stop admitting new work before the host tears down. Work in progress
    /// and cleanup keep going.
    pub fn begin_draining(&self) {
        self.host.cancel_integrity_check();
        self.state
            .admission
            .store(AGENT_SERVICE_DRAINING, Ordering::Release);
    }

    /// Reopen request admission after a completed or cancelled store operation.
    pub fn resume_admission(&self) {
        self.reopen_after_failed_shutdown();
    }

    /// Observe active work without creating or starting a runtime.
    pub async fn store_activity(&self) -> Result<(usize, usize), String> {
        let runtime = self.state.runtime.lock().await.clone();
        Ok(match runtime {
            Some(runtime) => runtime.store_activity().await,
            None => (0, 0),
        })
    }

    /// Retry retained history while preserving live tasks and their contexts.
    /// A previously stopped runtime is replaced only after its retained source
    /// writes succeed. No cancelled run or prompt is replayed.
    /// Only this explicit retry may dismiss the save-failure notice.
    pub async fn flush_persistence(&self) -> Result<(), String> {
        let _lifecycle = self.state.lifecycle.lock().await;
        let store = self
            .host
            .store
            .as_ref()
            .ok_or_else(|| "The host has not attached its local store".to_string())?;
        let failure = store.persistence_failure();
        let runtime = self.state.runtime.lock().await.clone();
        if let Some(runtime) = runtime {
            runtime.flush_loaded_sessions().await?;
        }
        let pending = self.state.shutdown_pending.lock().await.clone();
        let replacement = if let Some(pending) = &pending {
            // The inner error is the first flush failure; the completed drain
            // has already stopped tools and notified extensions exactly once.
            // An outer error means the drain task did not complete safely.
            let _first_flush = pending.wait().await?;
            pending
                .runtime
                .store
                .ensure_current()
                .map_err(|error| error.to_string())?;
            pending.runtime.finish_shutdown().await?;
            Some(AgentRuntime::new(runtime::RuntimeParts {
                account_scope: pending.runtime.account_scope.clone(),
                user_id: pending.runtime.user_id.clone(),
                api: pending.runtime.api.clone(),
                store: self.task_store(&pending.runtime.user_id)?,
                host: self.host.clone(),
                questions: self.state.questions.clone(),
                project_root: pending.runtime.project_root(),
                model: pending.runtime.model.clone(),
            }))
        } else {
            None
        };
        // Acquire both publication slots before clearing the failure latch.
        // Cancellation at either await leaves the old non-runnable owner and
        // notice intact; no await separates the successful publication below.
        let mut pending_slot = self.state.shutdown_pending.lock().await;
        let mut runtime_slot = self.state.runtime.lock().await;
        match failure {
            Some(failure) if !store.clear_persistence_failure(failure.revision) => {
                return Err(
                    "Another save failed during the retry. Keep Maple open and retry saving."
                        .into(),
                );
            }
            None if store.persistence_failure().is_some() => {
                return Err(
                    "A save failed during the retry. Keep Maple open and retry saving.".into(),
                );
            }
            _ => {}
        }
        let status = replacement.map(|replacement| {
            let status = replacement.desktop_status();
            *runtime_slot = Some(replacement);
            pending_slot.take();
            status
        });
        drop(runtime_slot);
        drop(pending_slot);
        if let Some(status) = status {
            emit_agent_event(&self.host.events, AgentServiceEvent::RuntimeStatus(status));
        }
        Ok(())
    }

    /// Called with the service lifecycle lock. Publish retained ownership before
    /// the first await that can leave a drain running without its caller.
    async fn begin_runtime_shutdown_locked(&self) -> Option<(PendingRuntimeShutdown, bool)> {
        let mut pending_slot = self.state.shutdown_pending.lock().await;
        if let Some(pending) = pending_slot.as_ref() {
            return Some((pending.clone(), true));
        }
        let runtime = self.state.runtime.lock().await.take()?;
        let pending = PendingRuntimeShutdown::new(runtime);
        *pending_slot = Some(pending.clone());
        drop(pending_slot);
        emit_agent_event(
            &self.host.events,
            AgentServiceEvent::RuntimeStatus(stopped_status()),
        );
        Some((pending, false))
    }

    /// Called with the service lifecycle lock. The pending slot owns every
    /// stopped session even if this request is cancelled while awaiting drain.
    async fn shutdown_runtime_locked(&self) -> Result<(), String> {
        let Some((pending, retry)) = self.begin_runtime_shutdown_locked().await else {
            return Ok(());
        };
        let first_flush = pending.wait().await?;
        if !retry {
            first_flush?;
        }
        pending.runtime.finish_shutdown().await?;
        self.state.shutdown_pending.lock().await.take();
        Ok(())
    }

    /// Drain the profile's runtime during host shutdown, regardless of which
    /// frontend or account originally started it.
    pub async fn shutdown(&self) -> Result<(), String> {
        self.begin_draining();
        // A readonly checker also owns a DEK. Completed shutdown includes its
        // cancellation and reap before the host may release ownership.
        self.host.wait_integrity_idle().await;
        let _lifecycle = self.state.lifecycle.lock().await;
        self.shutdown_runtime_locked().await
    }

    /// Stop tools for an explicitly confirmed destructive reset, retaining all
    /// source history until the host reports that the reset itself succeeded.
    /// Unlike ordinary shutdown, a failed save cannot veto the confirmed reset.
    pub async fn prepare_store_reset(&self) -> Result<(), String> {
        self.begin_draining();
        self.host.wait_integrity_idle().await;
        let _lifecycle = self.state.lifecycle.lock().await;
        if let Some((pending, _)) = self.begin_runtime_shutdown_locked().await {
            // Only persistence can fail inside a completed runtime drain. An
            // outer error means tools/extensions did not finish safely.
            let _save_result = pending.wait().await?;
        }
        Ok(())
    }

    /// Discard already-drained history after a confirmed reset succeeds or
    /// returns typed RecoveryRequired after fencing the old generation. The
    /// host must retain pending history for every other failed/cancelled reset.
    pub async fn finish_store_reset(&self) -> Result<(), String> {
        let _lifecycle = self.state.lifecycle.lock().await;
        if self.state.runtime.lock().await.is_some() {
            return Err("Stop Agent work before finishing the local store reset".into());
        }
        let pending = self.state.shutdown_pending.lock().await.clone();
        if let Some(pending) = pending {
            let _save_result = pending.wait().await?;
            pending.runtime.discard_drained_sessions().await;
            self.state.shutdown_pending.lock().await.take();
        }
        Ok(())
    }

    /// Stop and invalidate account handles before changing profile identity.
    pub async fn sign_out(&self, user_id: &str) -> Result<(), String> {
        self.drain_account(user_id).await?;
        emit_agent_event(&self.host.events, AgentServiceEvent::AccountChanged(None));
        Ok(())
    }

    /// Drain for a pending account operation without publishing sign-out before
    /// the authoritative server outcome is known.
    pub async fn drain_account(&self, user_id: &str) -> Result<(), String> {
        self.host.cancel_integrity_check();
        self.host.wait_integrity_idle().await;
        let scope = account_scope(user_id)?;
        let _lifecycle = self.state.lifecycle.lock().await;
        self.advance_generation(&scope);
        // A replay source has already resolved this cached TaskStore. Do not
        // make account drain depend on a new SQL read of an unavailable store.
        let account = self
            .state
            .stores
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&scope)
            .map(|store| store.account());
        if let Some((store, account)) = self.host.store.clone().zip(account) {
            tokio::task::spawn_blocking(move || store.cancel_account_replays(account))
                .await
                .map_err(|_| "Agent history replay drain failed".to_string())?
                .map_err(|error| error.to_string())?;
        }
        self.shutdown_runtime_locked().await?;
        if let Some(store) = self
            .state
            .stores
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&scope)
        {
            store.revoke();
        }
        Ok(())
    }

    /// Admit work again after a shutdown that did not happen.
    pub fn reopen_after_failed_shutdown(&self) {
        self.state
            .admission
            .store(AGENT_SERVICE_OPEN, Ordering::Release);
    }

    pub fn ensure_accepting_new_work(&self) -> Result<(), String> {
        if self.state.admission.load(Ordering::Acquire) != AGENT_SERVICE_OPEN {
            return Err(AGENT_SERVICE_DRAINING_ERROR.to_string());
        }
        // A stopped runtime can still own unsaved source entries. Mutations
        // such as task deletion must not remove their destination before Retry
        // saves them. Status and committed-history reads remain available.
        match self.state.shutdown_pending.try_lock() {
            Ok(pending) if pending.is_none() => Ok(()),
            _ => Err(RUNTIME_SAVE_PENDING_ERROR.to_string()),
        }
    }

    /// Replace the opening of the system prompt. Sessions built after this
    /// call use it; a session already loaded keeps its prompt.
    pub fn set_harness_instructions(&self, harness_instructions: String) {
        *self
            .host
            .harness_instructions
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = harness_instructions;
    }

    /// The task index of an account, opened on first use.
    fn task_store(&self, user_id: &str) -> Result<Arc<TaskStore>, String> {
        let scope = account_scope(user_id)?;
        let mut stores = self
            .state
            .stores
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(store) = stores.get(&scope)
            && store.is_current().map_err(|error| error.to_string())?
        {
            return Ok(Arc::clone(store));
        }
        if let Some(stale) = stores.remove(&scope) {
            stale.revoke();
        }
        let device_store = self
            .host
            .store
            .as_ref()
            .ok_or_else(|| "The host has not attached its local store".to_string())?;
        let account = device_store
            .upsert_account(user_id, pi_ai::now_ms())
            .map_err(|error| error.to_string())?;
        let store = Arc::new(TaskStore::new(Arc::clone(device_store), account, user_id));
        stores.insert(scope, Arc::clone(&store));
        Ok(store)
    }

    #[cfg(test)]
    fn question_broker(&self) -> questions::QuestionBroker {
        self.state.questions.clone()
    }

    fn advance_generation(&self, account_scope: &str) {
        let mut generations = self
            .state
            .generations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *generations.entry(account_scope.to_string()).or_default() += 1;
    }
}

impl AgentRuntimeHandle {
    /// Subscribe before reading a task snapshot; projections reconcile updates
    /// by their existing timeline item identities.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<AgentServiceEvent> {
        self.service.host.events.subscribers.subscribe()
    }

    pub fn question_responder(&self) -> QuestionResponder {
        self.service.question_responder()
    }
    pub async fn pending_questions(&self) -> Vec<AgentServiceEvent> {
        if self.verify_generation().await.is_err() {
            return Vec::new();
        }
        let Ok(tasks) = self.store() else {
            return Vec::new();
        };
        let pending = self.service.pending_questions().await;
        let questions = tokio::task::spawn_blocking(move || {
            if !matches!(tasks.store().account_deletion(tasks.account()), Ok(None)) {
                return Vec::new();
            }
            pending
                .into_iter()
                .filter(|event| {
                    matches!(event,AgentServiceEvent::Question {session_id,..}
                    if tasks.store().task_history_stamp(tasks.account(),session_id).is_ok())
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
        if self.verify_generation().await.is_err() {
            return Vec::new();
        }
        questions
    }

    pub async fn verify_generation(&self) -> Result<(), String> {
        self.verify_generation_now()
    }

    fn verify_generation_now(&self) -> Result<(), String> {
        if self.service.generation(&self.account_scope) == self.generation
            && self.service.store_generation() == self.store_generation
        {
            Ok(())
        } else {
            Err("Agent Mode data changed while this operation was waiting".to_string())
        }
    }

    pub fn ensure_accepting_new_work(&self) -> Result<(), String> {
        self.service.ensure_accepting_new_work()
    }

    fn paths(&self) -> &AgentPathLayout {
        &self.service.host.paths
    }

    /// Hold while reading or saving the account's settings. Loading can save
    /// a migrated copy, so reads take it too.
    async fn lock_settings(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.service.state.settings.lock().await
    }

    /// The account's running runtime, if it is this account's.
    async fn current_runtime(&self) -> Result<Option<Arc<AgentRuntime>>, String> {
        self.verify_generation().await?;
        let runtime = self.service.state.runtime.lock().await;
        match runtime.as_ref() {
            Some(current) => {
                current.ensure_account(&self.account_scope)?;
                Ok(Some(Arc::clone(current)))
            }
            None => Ok(None),
        }
    }

    /// The account's running runtime, or an error when it is not running.
    async fn runtime(&self) -> Result<Arc<AgentRuntime>, String> {
        if self.service.state.shutdown_pending.lock().await.is_some() {
            return Err(RUNTIME_SAVE_PENDING_ERROR.to_string());
        }
        self.current_runtime()
            .await?
            .ok_or_else(|| RUNTIME_NOT_RUNNING_ERROR.to_string())
    }

    /// The account's task index, whether or not its runtime runs.
    fn store(&self) -> Result<Arc<TaskStore>, String> {
        self.verify_generation_now()?;
        self.service.task_store(&self.user_id)
    }

    pub async fn status(&self) -> Result<AgentRuntimeStatus, String> {
        Ok(match self.current_runtime().await? {
            Some(runtime) => runtime.desktop_status(),
            None => stopped_status(),
        })
    }

    /// Start the account's runtime, or report the one already running.
    pub async fn start(
        &self,
        maple_api_session: Arc<MapleApiSession>,
        request: Option<AgentStartRequest>,
    ) -> Result<AgentRuntimeStatus, String> {
        let _lifecycle = self.service.state.lifecycle.lock().await;
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        self.start_locked(maple_api_session, request).await
    }

    async fn start_locked(
        &self,
        maple_api_session: Arc<MapleApiSession>,
        request: Option<AgentStartRequest>,
    ) -> Result<AgentRuntimeStatus, String> {
        if self.service.state.shutdown_pending.lock().await.is_some() {
            return Err(RUNTIME_SAVE_PENDING_ERROR.to_string());
        }
        if let Some(current) = self.service.state.runtime.lock().await.as_ref() {
            current.ensure_account(&self.account_scope)?;
            return Ok(current.desktop_status());
        }
        if maple_api_session.account_scope() != self.account_scope.as_ref() {
            return Err(
                "Maple API authentication belongs to a different signed-in account".to_string(),
            );
        }
        let _settings = self.lock_settings().await;
        let mut config = load_agent_config_inner(self.paths(), &self.user_id)
            .map_err(|error| format!("Failed to load Agent config: {error}"))?;
        let request = request.unwrap_or(AgentStartRequest {
            project_root: None,
            model: None,
        });
        let project_root = resolve_project_root(request.project_root.as_deref(), &config)
            .map_err(|error| format!("Failed to resolve Agent Mode project root: {error}"))?;
        let model = request
            .model
            .unwrap_or_else(|| config.default_model.clone());
        let runtime = AgentRuntime::new(runtime::RuntimeParts {
            account_scope: self.account_scope.to_string(),
            user_id: self.user_id.to_string(),
            api: maple_api_session,
            store: self.store()?,
            host: self.service.host.clone(),
            questions: self.service.state.questions.clone(),
            project_root: project_root.clone(),
            model: model.clone(),
        });
        let status = runtime.desktop_status();
        // A crash can strand a task an ACP client created and never
        // prompted; remove those before the task list is read.
        surface::sweep_stale_provisional(&runtime.store, self.paths(), &self.user_id);
        // The delegation skills follow the external agents in Settings.
        if let Err(error) = integrations::sync_external_agent_skills(self.paths(), &self.user_id) {
            log::warn!("Failed to update the external agent skills: {error}");
        }
        *self.service.state.runtime.lock().await = Some(runtime);

        // Starting is project use, not a folder the user added, so the
        // recent list is left as it is.
        config.default_project_root = Some(path_string(&project_root));
        config.default_model = model;
        if let Err(error) = save_agent_config_inner(self.paths(), &self.user_id, &config) {
            log::warn!("Failed to save Agent config after runtime start: {error}");
        }
        emit_agent_event(
            &self.service.host.events,
            AgentServiceEvent::RuntimeStatus(status.clone()),
        );
        Ok(status)
    }

    /// Stop the account's runtime: its runs end, and the call returns once
    /// they have.
    pub async fn stop(&self) -> Result<AgentRuntimeStatus, String> {
        let _lifecycle = self.service.state.lifecycle.lock().await;
        self.verify_generation().await?;
        self.stop_locked().await?;
        Ok(stopped_status())
    }

    async fn stop_locked(&self) -> Result<(), String> {
        if let Some(current) = self.service.state.runtime.lock().await.as_ref() {
            current.ensure_account(&self.account_scope)?;
        }
        if let Some(pending) = self.service.state.shutdown_pending.lock().await.as_ref() {
            pending.runtime.ensure_account(&self.account_scope)?;
        }
        self.service.shutdown_runtime_locked().await
    }

    pub async fn restart(
        &self,
        maple_api_session: Arc<MapleApiSession>,
        request: Option<AgentStartRequest>,
    ) -> Result<AgentRuntimeStatus, String> {
        let _lifecycle = self.service.state.lifecycle.lock().await;
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        self.stop_locked().await?;
        self.start_locked(maple_api_session, request).await
    }

    pub async fn load_config(&self) -> Result<AgentConfig, String> {
        self.verify_generation().await?;
        let _settings = self.lock_settings().await;
        load_agent_config_inner(self.paths(), &self.user_id).map_err(|error| error.to_string())
    }

    pub async fn save_config(&self, config: AgentConfig) -> Result<(), String> {
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let _settings = self.lock_settings().await;
        // MCP servers have their own command. Keep the saved ones so a late
        // project or model save cannot overwrite newer servers.
        let mut next =
            load_agent_config_inner(self.paths(), &self.user_id).map_err(|e| e.to_string())?;
        next.default_project_root = config.default_project_root;
        next.default_model = config.default_model;
        save_agent_config_inner(self.paths(), &self.user_id, &next).map_err(|e| e.to_string())
    }

    pub async fn save_recent_project_root(&self, path: String) -> Result<String, String> {
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let project_root = normalize_project_root(Path::new(&path))?;
        let _settings = self.lock_settings().await;
        let canonical_path = path_string(&project_root);
        let (paths, user, root, tasks) = (
            self.paths().clone(),
            self.user_id.clone(),
            canonical_path.clone(),
            self.store()?,
        );
        tokio::task::spawn_blocking(move || {
            save_default_project_root_inner(&paths, &user, Some(root.clone()))
                .map_err(|error| error.to_string())?;
            tasks
                .store()
                .register_explicit_project(tasks.account(), &root, pi_ai::now_ms())
                .map_err(|error| error.to_string())?;
            Ok::<_, String>(())
        })
        .await
        .map_err(|_| "Project settings worker did not complete".to_owned())??;
        self.verify_generation().await?;
        Ok(canonical_path)
    }

    pub async fn remove_project_root(
        &self,
        path: String,
        fallback_path: Option<String>,
    ) -> Result<AgentConfig, String> {
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let path = path.trim().to_string();
        if !structurally_valid_project_root(&path) {
            return Err("Project path must be an absolute folder path".to_string());
        }
        let fallback_path = fallback_path
            .map(|fallback| fallback.trim().to_string())
            .filter(|fallback| !fallback.is_empty());
        if let Some(fallback) = fallback_path.as_deref()
            && (fallback == path || !structurally_valid_project_root(fallback))
        {
            return Err("Project fallback must be a different absolute folder path".to_string());
        }
        let runtime = self.current_runtime().await?;
        let _settings = self.lock_settings().await;
        let mut config = load_agent_config_inner(self.paths(), &self.user_id)
            .map_err(|error| error.to_string())?;
        let (paths, user, tasks, removed, fallback, guard) = (
            self.paths().clone(),
            self.user_id.clone(),
            self.store()?,
            path.clone(),
            fallback_path.clone(),
            runtime.clone(),
        );
        tokio::task::spawn_blocking(move || {
            let action = || {
                if let Some(fallback) = &fallback
                    && tasks
                        .store()
                        .project(tasks.account(), fallback)
                        .map_err(|error| error.to_string())?
                        .is_some_and(|project| project.removed_ms.is_some())
                {
                    return Err("Project fallback is already removed".to_owned());
                }
                tasks
                    .store()
                    .remove_project(tasks.account(), &removed, pi_ai::now_ms())
                    .map_err(|error| error.to_string())?;
                if config.default_project_root.as_deref() == Some(removed.as_str()) {
                    save_default_project_root_inner(&paths, &user, fallback.clone())
                        .map_err(|error| error.to_string())?;
                    config.default_project_root = fallback;
                }
                if !config.removed_project_roots.contains(&removed) {
                    config.removed_project_roots.push(removed);
                }
                Ok(config)
            };
            if let Some(runtime) = guard {
                runtime.maintenance_while_all_idle(action)
            } else {
                action()
            }
        })
        .await
        .map_err(|_| "Project settings worker did not complete".to_owned())??;
        if let Some(runtime) = runtime {
            runtime.project_root_removed(&path, fallback_path.as_deref());
        }
        let config = load_agent_config_inner(self.paths(), &self.user_id)
            .map_err(|error| error.to_string())?;
        emit_agent_event(
            &self.service.host.events,
            AgentServiceEvent::ProjectRemoved {
                user_id: self.user_id.to_string(),
                project_root: path,
            },
        );
        Ok(config)
    }

    pub async fn get_project_trust(&self, path: String) -> Result<AgentProjectTrustStatus, String> {
        self.verify_generation().await?;
        let requested = Path::new(path.trim());
        if !requested.is_dir() {
            return Ok(AgentProjectTrustStatus {
                path: path_string(requested),
                decision: None,
                available: false,
                protected_features: Vec::new(),
            });
        }
        let project_root = normalize_project_root(requested)?;
        let config = {
            let _settings = self.lock_settings().await;
            load_project_config_inner(self.paths(), &self.user_id, &project_root)
                .map_err(|e| e.to_string())?
        };
        Ok(self.project_trust_status(&config, &project_root))
    }

    /// The trust status of `project_root`, with what trusting it would add
    /// to its tasks.
    fn project_trust_status(
        &self,
        config: &AgentConfig,
        project_root: &Path,
    ) -> AgentProjectTrustStatus {
        let features = resources::protected_features(self.paths(), &self.user_id, project_root);
        project_trust_status(config, project_root, features)
    }

    pub async fn set_project_trust(
        &self,
        path: String,
        trusted: bool,
    ) -> Result<AgentProjectTrustStatus, String> {
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let project_root = normalize_project_root(Path::new(&path))?;
        let root = path_string(&project_root);
        let runtime = self.current_runtime().await?;
        if let Some(runtime) = &runtime {
            let running = runtime.running_task_ids();
            let in_project = self
                .store()?
                .list(Some(&root))?
                .into_iter()
                .any(|row| running.contains(&row.id));
            if in_project {
                return Err(
                    "Stop running agents in this project before changing project trust".to_string(),
                );
            }
        }
        let config = {
            let _settings = self.lock_settings().await;
            let mut config =
                load_agent_config_inner(self.paths(), &self.user_id).map_err(|e| e.to_string())?;
            apply_project_trust(&mut config, &project_root, trusted);
            save_agent_config_inner(self.paths(), &self.user_id, &config)
                .map_err(|e| e.to_string())?;
            config
        };
        // The project's tasks read the decision when their sessions are
        // built again.
        if let Some(runtime) = runtime {
            runtime.unload_project_tasks(&root).await?;
        }
        Ok(self.project_trust_status(&config, &project_root))
    }

    /// Deliver the user's answer to a question from this account's runtime.
    /// False when nothing was pending.
    pub async fn answer_question_via_handle(
        &self,
        request_id: &str,
        answer: String,
    ) -> Result<bool, String> {
        self.verify_generation().await?;
        Ok(self.service.answer_question(request_id, answer).await)
    }

    /// Slash commands available in `working_dir`.
    pub fn slash_commands(&self, working_dir: Option<&str>) -> Vec<AgentSlashCommand> {
        self.service
            .list_slash_commands(Some(&self.user_id), working_dir)
    }

    /// Expand `/command args` into the prompt that runs the command.
    pub fn expand_slash_command(
        &self,
        working_dir: Option<&str>,
        command: &str,
        args: &str,
    ) -> Result<Option<String>, String> {
        self.service
            .resolve_slash_command(Some(&self.user_id), working_dir, command, args)
    }
}

/// The runtime on a scripted provider, which other modules' tests use too.
#[cfg(test)]
pub(crate) mod tests;

#[cfg(test)]
mod cache_tests;

#[cfg(test)]
mod shutdown_tests;
