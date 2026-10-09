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
mod integrations;
mod login_path;
mod mcp;
mod placeholders;
pub(crate) mod provider;
mod questions;
mod runs;
mod runtime;
mod store;
mod tasks;
mod timeline;
mod tool_context;
mod tools;
mod types;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, RwLock};

use crate::maple_api::{MapleApiSession, account_scope};
pub use attachments::AgentImageUpload;
use config::*;
pub use config::{
    MAPLE_WORKSPACE_DIRECTORY_NAME, MAPLE_WORKSPACE_DISPLAY_NAME, account_local_data_dir,
    account_tool_summaries_db_path, default_maple_workspace_path, ensure_default_maple_workspace,
    include_default_maple_workspace, is_default_maple_workspace, maple_workspace_directory,
    startup_project_root,
};
pub use integrations::begin_integration_setup;
pub use runs::AgentRunHandle;
use runtime::AgentRuntime;
use store::TaskStore;
pub use tool_context::{AgentToolContextSpec, default_tool_context_spec};
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
    "Maple Agent services are draining and cannot accept new work";
const RUNTIME_NOT_RUNNING_ERROR: &str = "Agent runtime is not running";

/// Where the runtime keeps its files: the account configuration below the
/// app's config root, and device-local task data below its local data root.
#[derive(Clone)]
pub struct AgentPathLayout {
    config_root: PathBuf,
    local_data_root: PathBuf,
}

impl AgentPathLayout {
    pub fn from_app_roots(app_config_root: PathBuf, app_local_data_root: PathBuf) -> Self {
        Self {
            config_root: app_config_root.join("agent"),
            local_data_root: app_local_data_root.join("agent"),
        }
    }
}

/// Receives every event the runtime publishes to its host.
pub trait AgentEventSink: Send + Sync + 'static {
    fn emit(&self, event: &AgentServiceEvent);
}

#[derive(Clone)]
pub(crate) struct AgentEventDispatcher {
    sink: Arc<dyn AgentEventSink>,
}

impl AgentEventDispatcher {
    pub(crate) fn new(sink: Arc<dyn AgentEventSink>) -> Self {
        Self { sink }
    }
}

pub(crate) fn emit_agent_event(events: &AgentEventDispatcher, event: AgentServiceEvent) {
    events.sink.emit(&event);
}

/// What the host gives the runtime: where to keep files, where to send
/// events, the environment of task tools, and the opening of the system
/// prompt.
#[derive(Clone)]
pub struct MapleAgentHostResources {
    paths: AgentPathLayout,
    events: AgentEventDispatcher,
    default_tool_context: AgentToolContextSpec,
    /// Opening system prompt text from the host: who the agent is and how
    /// it behaves. The host may change it at any time; sessions built
    /// afterwards use the new text.
    harness_instructions: Arc<RwLock<String>>,
}

impl MapleAgentHostResources {
    pub fn new(
        paths: AgentPathLayout,
        event_sink: Arc<dyn AgentEventSink>,
        default_tool_context: AgentToolContextSpec,
        harness_instructions: String,
    ) -> Self {
        Self {
            paths,
            events: AgentEventDispatcher::new(event_sink),
            default_tool_context,
            harness_instructions: Arc::new(RwLock::new(harness_instructions)),
        }
    }

    fn harness_instructions(&self) -> String {
        self.harness_instructions
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
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
    /// Orders start, stop and restart.
    lifecycle: tokio::sync::Mutex<()>,
    /// Held while the account's settings are read and saved, so a save cannot
    /// overwrite a change made since its read. Taken after `lifecycle`.
    settings: tokio::sync::Mutex<()>,
    /// Each account's task index, opened once.
    stores: std::sync::Mutex<HashMap<String, Arc<TaskStore>>>,
    /// Bumped when an account's data is cleared, which revokes the handles
    /// issued before.
    generations: std::sync::Mutex<HashMap<String, u64>>,
    admission: AtomicU8,
    /// The question broker this service installed as the process global.
    questions: questions::QuestionBroker,
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
}

impl MapleAgentService {
    pub fn new(host: MapleAgentHostResources) -> Self {
        let questions = questions::init_global(host.events.clone());
        Self {
            host,
            state: Arc::new(ServiceState {
                runtime: tokio::sync::Mutex::new(None),
                lifecycle: tokio::sync::Mutex::new(()),
                settings: tokio::sync::Mutex::new(()),
                stores: std::sync::Mutex::new(HashMap::new()),
                generations: std::sync::Mutex::new(HashMap::new()),
                admission: AtomicU8::new(AGENT_SERVICE_OPEN),
                questions,
            }),
        }
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
        })
    }

    fn generation(&self, account_scope: &str) -> u64 {
        *self
            .state
            .generations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(account_scope)
            .unwrap_or(&0)
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

    /// Slash commands available in `working_dir`.
    pub fn list_slash_commands(
        &self,
        user_id: Option<&str>,
        working_dir: Option<&str>,
    ) -> Vec<AgentSlashCommand> {
        placeholders::slash_commands(&self.host.paths, user_id, working_dir)
    }

    /// Expand `/command args` into the prompt that runs the command. `None`
    /// when no command has that name.
    pub fn resolve_slash_command(
        &self,
        working_dir: Option<&str>,
        command: &str,
        args: &str,
    ) -> Result<Option<String>, String> {
        placeholders::resolve_slash_command(working_dir, command, args)
    }

    /// Stop admitting new work before the host tears down. Work in progress
    /// and cleanup keep going.
    pub fn begin_draining(&self) {
        self.state
            .admission
            .store(AGENT_SERVICE_DRAINING, Ordering::Release);
    }

    /// Admit work again after a shutdown that did not happen.
    pub fn reopen_after_failed_shutdown(&self) {
        self.state
            .admission
            .store(AGENT_SERVICE_OPEN, Ordering::Release);
    }

    pub fn ensure_accepting_new_work(&self) -> Result<(), String> {
        if self.state.admission.load(Ordering::Acquire) == AGENT_SERVICE_OPEN {
            Ok(())
        } else {
            Err(AGENT_SERVICE_DRAINING_ERROR.to_string())
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
        if let Some(store) = stores.get(&scope) {
            return Ok(Arc::clone(store));
        }
        let sessions_dir = account_sessions_dir(&self.host.paths, user_id)?;
        let store = Arc::new(TaskStore::open(&sessions_dir)?);
        stores.insert(scope, Arc::clone(&store));
        Ok(store)
    }

    #[cfg(test)]
    fn question_broker(&self) -> questions::QuestionBroker {
        self.state.questions.clone()
    }

    #[cfg(test)]
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
    pub async fn verify_generation(&self) -> Result<(), String> {
        if self.service.generation(&self.account_scope) == self.generation {
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
        self.current_runtime()
            .await?
            .ok_or_else(|| RUNTIME_NOT_RUNNING_ERROR.to_string())
    }

    /// The account's task index, whether or not its runtime runs.
    fn store(&self) -> Result<Arc<TaskStore>, String> {
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
        let runtime = Arc::new(AgentRuntime::new(runtime::RuntimeParts {
            account_scope: self.account_scope.to_string(),
            user_id: self.user_id.to_string(),
            api: maple_api_session,
            store: self.store()?,
            host: self.service.host.clone(),
            project_root: project_root.clone(),
            model: model.clone(),
        }));
        let status = runtime.desktop_status();
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
        let runtime = {
            let mut slot = self.service.state.runtime.lock().await;
            match slot.as_ref() {
                Some(current) => {
                    current.ensure_account(&self.account_scope)?;
                    slot.take()
                }
                None => None,
            }
        };
        if let Some(runtime) = runtime {
            runtime.shutdown().await;
        }
        Ok(())
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

    pub async fn list_recent_project_roots(&self) -> Result<Vec<RecentProjectRoot>, String> {
        self.verify_generation().await?;
        load_recent_project_roots_inner(self.paths(), &self.user_id).map_err(|e| e.to_string())
    }

    pub async fn save_recent_project_root(
        &self,
        path: String,
    ) -> Result<AgentProjectRootRegistration, String> {
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let project_root = normalize_project_root(Path::new(&path))?;
        let _settings = self.lock_settings().await;
        let mut config = load_agent_config_inner(self.paths(), &self.user_id)
            .map_err(|error| error.to_string())?;
        let canonical_path = path_string(&project_root);
        let restoring = config
            .removed_project_roots
            .iter()
            .any(|removed| removed == &canonical_path);
        let roots = if restoring {
            restore_explicit_project_root_inner(self.paths(), &self.user_id, &project_root)
        } else {
            register_explicit_project_root_inner(self.paths(), &self.user_id, &project_root)
        }
        .map_err(|error| error.to_string())?;

        config
            .removed_project_roots
            .retain(|removed| removed != &canonical_path);
        config.default_project_root = Some(canonical_path.clone());
        save_agent_config_inner(self.paths(), &self.user_id, &config)
            .map_err(|error| error.to_string())?;
        // Clear the device-local removal last: if anything above failed,
        // the project stays hidden.
        if restoring {
            save_removed_project_roots_inner(
                self.paths(),
                &self.user_id,
                &config.removed_project_roots,
            )
            .map_err(|error| error.to_string())?;
        }
        Ok(AgentProjectRootRegistration {
            project_root: canonical_path,
            roots,
            config,
        })
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
        if let Some(runtime) = &runtime {
            let session_roots = self
                .store()?
                .list(None)?
                .into_iter()
                .map(|row| (row.id, row.project_root))
                .collect::<HashMap<_, _>>();
            if project_has_active_session_run(&session_roots, &runtime.running_task_ids(), &path) {
                return Err("Stop the running agent before removing this project".to_string());
            }
        }
        let _settings = self.lock_settings().await;
        let mut config = load_agent_config_inner(self.paths(), &self.user_id)
            .map_err(|error| error.to_string())?;
        apply_project_root_removal(&mut config, &path, fallback_path.as_deref())?;
        // The removal is recorded on this device only; saving the fallback
        // into the account config would change other devices too.
        save_removed_project_roots_inner(
            self.paths(),
            &self.user_id,
            &config.removed_project_roots,
        )
        .map_err(|error| error.to_string())?;
        if let Some(runtime) = runtime {
            runtime.project_root_removed(&path, fallback_path.as_deref());
        }
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
        let _settings = self.lock_settings().await;
        let config =
            load_agent_config_inner(self.paths(), &self.user_id).map_err(|e| e.to_string())?;
        Ok(project_trust_status(&config, &project_root, true))
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
            runtime.unload_project_tasks(&root).await;
        }
        Ok(project_trust_status(&config, &project_root, true))
    }

    pub async fn save_project_root_order(
        &self,
        paths: Vec<String>,
    ) -> Result<Vec<RecentProjectRoot>, String> {
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let _settings = self.lock_settings().await;
        save_project_root_order_inner(self.paths(), &self.user_id, paths).map_err(|e| e.to_string())
    }

    /// Deliver the user's answer to a question from this account's runtime.
    /// False when nothing was pending.
    pub async fn answer_question_via_handle(
        &self,
        request_id: &str,
        answer: String,
    ) -> Result<bool, String> {
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
            .resolve_slash_command(working_dir, command, args)
    }
}

#[cfg(test)]
mod tests;
