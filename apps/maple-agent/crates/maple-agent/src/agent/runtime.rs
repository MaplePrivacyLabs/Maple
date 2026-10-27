//! One account's running runtime: its model registry, its tasks' loaded
//! sessions and active runs, and the one factory every Pi session of the
//! account comes from.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use pi_agent_core::QueueMode;
use pi_ai::Model;
use pi_coding_agent::resources::Resources;
use pi_coding_agent::settings::{RetrySettings, Settings};
use pi_coding_agent::{AgentSession, AgentSessionOptions, ModelRegistry};
use tokio_util::sync::CancellationToken;

use super::config::path_string;
use super::cua::{self, TaskCua};
use super::external_agents::{
    ExternalAgentHost, ExternalAgentRegistry, ExternalAgentToolsFor, TaskProviders,
    external_agent_tools, sync_external_agent_tools, task_providers,
};
use super::integrations::{CUA_NAME, external_agents_on};
use super::mcp::{
    MAX_IDLE_TASKS_WITH_SERVERS, STARTUP_WAIT, TaskMcp, read_saved_servers, task_servers,
};
use super::provider::{CatalogEntry, catalog_entry, maple_model, maple_model_registry};
use super::questions::QuestionBroker;
use super::runs::{Failures, Runs};
use super::store::{TaskKind, TaskRow, TaskStore};
use super::surface::{AgentSurfaceAccess, SurfaceTask, with_surface_servers};
use super::{
    AgentRuntimeStatus, AgentServiceEvent, MapleAgentHostResources, emit_agent_event, login_path,
    tools,
};
use crate::maple_api::MapleApiSession;

/// The product name Pi's default system prompt names.
const APP_NAME: &str = "Maple";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Retries of a failed request: today's budget of about five minutes.
const RETRY: RetrySettings = RetrySettings {
    enabled: true,
    max_retries: 10,
    base_delay_ms: 1_000,
    max_agent_delay_ms: 30_000,
    // The provider sends nothing again itself; the session's retries do.
    provider: pi_coding_agent::settings::ProviderRetrySettings {
        timeout_ms: None,
        max_retries: None,
        max_retry_delay_ms: 60_000,
    },
};

/// The developer's override of every model's context window.
const CONTEXT_LIMIT_OVERRIDE_ENV: &str = "MAPLE_CONTEXT_LIMIT";

pub(super) struct RuntimeParts {
    pub(super) account_scope: String,
    pub(super) user_id: String,
    pub(super) api: Arc<MapleApiSession>,
    pub(super) store: Arc<TaskStore>,
    pub(super) host: MapleAgentHostResources,
    /// The service's broker, which the interface answers through.
    pub(super) questions: QuestionBroker,
    pub(super) project_root: PathBuf,
    pub(super) model: String,
}

/// How long a stopping runtime gives its external agents to end.
const EXTERNAL_AGENTS_SHUTDOWN: Duration = Duration::from_secs(10);

/// A task's loaded session, the MCP servers and built-in CUA that live with
/// it, and the external agents its tools may use.
struct LoadedTask {
    session: AgentSession,
    mcp: Arc<TaskMcp>,
    cua: Arc<TaskCua>,
    agents: TaskProviders,
    last_used: Instant,
    unloading: Arc<AtomicBool>,
}

/// Clear the marker even if an RPC is cancelled during extension shutdown.
struct UnloadingTask(Arc<AtomicBool>);
impl Drop for UnloadingTask {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub(super) struct AgentRuntime {
    pub(super) account_scope: String,
    pub(super) user_id: String,
    pub(super) api: Arc<MapleApiSession>,
    pub(super) store: Arc<TaskStore>,
    pub(super) host: MapleAgentHostResources,
    questions: QuestionBroker,
    models: ModelRegistry,
    /// The root new tasks start in.
    project_root: Mutex<PathBuf>,
    /// The model new tasks start on.
    pub(super) model: String,
    /// Cancelled when the runtime stops; detached work ends with it.
    pub(super) lifetime: CancellationToken,
    /// The tasks whose sessions are loaded.
    sessions: tokio::sync::Mutex<HashMap<String, LoadedTask>>,
    /// The tasks a calling surface holds, by task id. Changed, and read for
    /// a session's build, under the sessions' lock, so a session is built
    /// with what the task has.
    surfaces: Mutex<HashMap<String, HashMap<u64, SurfaceTask>>>,
    pub(super) runs: Runs,
    pub(super) failures: Failures,
    /// The external agents of the account's tasks.
    pub(super) external_agents: Arc<ExternalAgentRegistry>,
}

impl AgentRuntime {
    pub(super) fn new(parts: RuntimeParts) -> Arc<Self> {
        let models = maple_model_registry(parts.api.clone(), []);
        // Ask the login shell for its PATH now, so the first task does not
        // wait for it.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(login_path::login_search_path());
        }
        let lifetime = CancellationToken::new();
        Arc::new_cyclic(|runtime| Self {
            external_agents: Arc::new(ExternalAgentRegistry::new(ExternalAgentHost {
                events: parts.host.events.clone(),
                questions: parts.questions.clone(),
                runtime: runtime.clone(),
                lifetime: lifetime.clone(),
            })),
            account_scope: parts.account_scope,
            user_id: parts.user_id,
            api: parts.api,
            store: parts.store,
            host: parts.host,
            questions: parts.questions,
            models,
            project_root: Mutex::new(parts.project_root),
            model: parts.model,
            lifetime,
            sessions: tokio::sync::Mutex::new(HashMap::new()),
            surfaces: Mutex::new(HashMap::new()),
            runs: Runs::default(),
            failures: Failures::default(),
        })
    }

    pub(super) fn ensure_account(&self, account_scope: &str) -> Result<(), String> {
        if self.account_scope == account_scope {
            Ok(())
        } else {
            Err("Agent runtime belongs to a different signed-in account".to_string())
        }
    }

    pub(super) fn project_root(&self) -> PathBuf {
        self.project_root
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(super) fn desktop_status(&self) -> AgentRuntimeStatus {
        AgentRuntimeStatus {
            running: true,
            project_root: Some(path_string(&self.project_root())),
            model: Some(self.model.clone()),
            active_runs: self.runs.desktop_runs(),
        }
    }

    pub(super) async fn store_activity(&self) -> (usize, usize) {
        let running = self.runs.running_task_ids().len();
        // A task with active external agents cannot be unloaded. Snapshot its
        // ids without holding the sessions mutex across registry awaits.
        let tasks = self
            .sessions
            .lock()
            .await
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let mut background = 0;
        for task in tasks {
            background += self.external_agents.snapshot(&task).await.len();
        }
        (running, background)
    }

    pub(super) fn running_task_ids(&self) -> HashSet<String> {
        self.runs.running_task_ids()
    }

    /// Serialize the final maintenance check/write with normal run admission and
    /// autonomous external-agent launches. No async snapshots grant permission.
    pub(super) fn maintenance_while_idle<T>(
        &self,
        session_id: &str,
        action: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        self.runs.while_idle(session_id, || {
            self.external_agents.while_session_idle(session_id, action)
        })
    }

    pub(super) fn maintenance_while_all_idle<T>(
        &self,
        action: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        self.runs
            .while_all_idle(|| self.external_agents.while_all_idle(action))
    }

    /// Follow the removal of the project new tasks start in.
    pub(super) fn project_root_removed(&self, removed: &str, fallback: Option<&str>) {
        let mut project_root = self
            .project_root
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        super::config::update_runtime_project_root_after_removal(
            &mut project_root,
            removed,
            fallback,
        );
    }

    /// Forget idle project sessions only after pending source writes commit.
    pub(super) async fn unload_project_tasks(&self, project_root: &str) -> Result<(), String> {
        let running = self.running_task_ids();
        let ids = self
            .sessions
            .lock()
            .await
            .iter()
            .filter(|(id, task)| {
                !running.contains(*id)
                    && task
                        .session
                        .with_session(|manager| manager.cwd() == project_root)
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in ids {
            self.unload_task(&id).await?;
        }
        Ok(())
    }

    /// Retain the session throughout shutdown/flush. A failed persistence retry
    /// never drops the only in-memory copy, and a concurrent loader cannot revive
    /// a session whose extensions are being stopped.
    pub(super) async fn unload_task(&self, session_id: &str) -> Result<(), String> {
        let (session, _unloading) = {
            let mut sessions = self.sessions.lock().await;
            if self.runs.is_running(session_id) {
                return Err("Stop the running agent before unloading this task".into());
            }
            let Some(task) = sessions.get_mut(session_id) else {
                return Ok(());
            };
            if task.unloading.swap(true, Ordering::AcqRel) {
                return Err("Agent task is being unloaded".into());
            }
            (
                task.session.clone(),
                UnloadingTask(Arc::clone(&task.unloading)),
            )
        };
        session.shutdown().await;
        flush_session(&session).await?;
        self.sessions.lock().await.remove(session_id);
        Ok(())
    }

    /// Keep at most eight idle contexts. Active runs, detached external work and
    /// failed persistence retries retain their context until safe to unload.
    async fn evict_idle_tasks(&self, current: &str) {
        let mut candidates = self
            .sessions
            .lock()
            .await
            .iter()
            .filter(|(id, task)| id.as_str() != current && !task.unloading.load(Ordering::Acquire))
            .map(|(id, task)| (task.last_used, id.clone()))
            .collect::<Vec<_>>();
        candidates.sort_by_key(|(used, _)| *used);
        for (_, id) in candidates {
            if self.sessions.lock().await.len() <= 8 {
                break;
            }
            if self.runs.is_running(&id) || !self.external_agents.snapshot(&id).await.is_empty() {
                continue;
            }
            if let Err(error) = self.unload_task(&id).await {
                log::warn!("Retained an Agent context whose unload failed: {error}");
            }
        }
    }

    /// A loaded session, if it is available and not being evicted.
    pub(super) async fn loaded_session(&self, session_id: &str) -> Option<AgentSession> {
        let mut sessions = self.sessions.lock().await;
        let task = sessions.get_mut(session_id)?;
        if task.unloading.load(Ordering::Acquire) {
            return None;
        }
        task.last_used = Instant::now();
        Some(task.session.clone())
    }

    /// Read only while run/external admission is held. A failed source append
    /// can leave the SQL draft caches empty while its only conversation copy
    /// is still in this loaded Pi session.
    pub(super) fn has_unpersisted_session(&self, session_id: &str) -> Result<bool, String> {
        let sessions = self
            .sessions
            .try_lock()
            .map_err(|_| "Agent contexts are busy".to_string())?;
        let Some(task) = sessions.get(session_id) else {
            return Ok(false);
        };
        if task.unloading.load(Ordering::Acquire) {
            return Err("Agent task is being unloaded".into());
        }
        Ok(task.session.has_unpersisted())
    }

    /// A loaded task's MCP servers.
    pub(super) async fn loaded_mcp(&self, session_id: &str) -> Option<Arc<TaskMcp>> {
        self.sessions
            .lock()
            .await
            .get(session_id)
            .map(|task| Arc::clone(&task.mcp))
    }

    /// A loaded task's built-in CUA.
    pub(super) async fn loaded_cua(&self, session_id: &str) -> Option<Arc<TaskCua>> {
        self.sessions
            .lock()
            .await
            .get(session_id)
            .map(|task| Arc::clone(&task.cua))
    }

    /// Run the task's MCP servers as it has them now and the account saves
    /// them, and bind its built-in CUA anew for a model with or without
    /// `vision`, waiting a little for servers connecting, unless the run
    /// stops. Returns the notice of what failed or is still connecting.
    pub(super) async fn start_task_integrations(
        &self,
        session_id: &str,
        vision: bool,
        stopped: &CancellationToken,
    ) -> Option<String> {
        let mcp = self.loaded_mcp(session_id).await?;
        let row = self.store.get(session_id).ok().flatten()?;
        let (paths, user_id) = (self.host.paths.clone(), self.user_id.clone());
        let saved = tokio::task::spawn_blocking(move || read_saved_servers(&paths, &user_id))
            .await
            .map_err(|error| error.to_string())
            .and_then(|saved| saved);
        // Synchronize under the attachment lock. Detach revokes under this
        // same lock before disabling its servers, so a stale snapshot cannot
        // resurrect client credentials after disconnection.
        {
            let surfaces = lock(&self.surfaces);
            let surface_servers = surfaces
                .get(session_id)
                .into_iter()
                .flat_map(|tasks| tasks.values())
                .filter(|task| !task.revoked.is_cancelled())
                .flat_map(|task| task.mcp_servers.clone())
                .collect();
            match saved {
                Ok(saved) => {
                    match with_surface_servers(task_servers(&saved, &row), surface_servers) {
                        Ok(servers) => mcp.sync(servers),
                        Err(error) => {
                            mcp.sync(Vec::new());
                            return Some(error);
                        }
                    }
                }
                Err(error) => {
                    log::warn!("Failed to read the account's MCP servers: {error}");
                    mcp.sync(surface_servers);
                }
            }
        }
        self.stop_idle_task_servers(session_id).await;
        let started =
            futures_util::future::join(mcp.wait(STARTUP_WAIT), self.start_task_cua(&row, vision));
        let cua_failure = tokio::select! {
            (_, failure) = started => failure,
            _ = stopped.cancelled() => return None,
        };
        mcp.take_notice(cua_failure.map(|error| (CUA_NAME.to_string(), error)))
    }

    /// Bind a desktop task that has built-in CUA on, which renews its
    /// authorization, or take CUA from one that has it off. Returns why the
    /// binding failed, if it did.
    async fn start_task_cua(&self, row: &TaskRow, vision: bool) -> Option<String> {
        let task_cua = self.loaded_cua(&row.id).await?;
        if cua::task_choice(row) == Some(true) {
            task_cua.bind(vision).await.err()
        } else {
            if task_cua.is_bound() {
                task_cua.unbind().await;
            }
            None
        }
    }

    pub(super) fn surface_holds(&self, access: &AgentSurfaceAccess) -> bool {
        lock(&self.surfaces)
            .get(access.session_id())
            .and_then(|tasks| tasks.get(&access.installation()))
            .is_some_and(|task| !task.revoked.is_cancelled())
    }

    pub(super) async fn install_surface(
        &self,
        session_id: &str,
        task: SurfaceTask,
    ) -> Result<(), String> {
        if self.lifetime.is_cancelled() {
            return Err(super::RUNTIME_NOT_RUNNING_ERROR.into());
        }
        // Host and client servers must never silently replace each other.
        let saved = super::mcp::load_saved_mcp_servers(&self.host.paths, &self.user_id)?;
        let mut surfaces = lock(&self.surfaces);
        let attached = surfaces.entry(session_id.to_string()).or_default();
        for server in &task.mcp_servers {
            let key = super::mcp::name_to_key(&server.name);
            if saved
                .iter()
                .any(|other| super::mcp::name_to_key(&other.name) == key)
                || attached
                    .values()
                    .flat_map(|other| &other.mcp_servers)
                    .any(|other| super::mcp::name_to_key(&other.name) == key)
            {
                return Err(format!(
                    "MCP server {} is already configured on this task",
                    server.name
                ));
            }
        }
        attached.insert(task.installation, task);
        Ok(())
    }

    /// Detach only this interface. Shared runs and the host-owned task survive.
    /// Client MCP bindings disappear from the next server synchronization.
    pub(super) async fn release_surface(&self, session_id: &str, installation: u64) {
        let names: Vec<String> = {
            let surfaces = lock(&self.surfaces);
            match surfaces
                .get(session_id)
                .and_then(|tasks| tasks.get(&installation))
            {
                Some(task) => {
                    task.revoked.cancel();
                    task.mcp_servers
                        .iter()
                        .map(|server| super::mcp::name_to_key(&server.name))
                        .collect()
                }
                None => Vec::new(),
            }
        };
        if let Some(mcp) = self.loaded_mcp(session_id).await {
            for name in names {
                mcp.disable(&name).await;
            }
        }
        let mut surfaces = lock(&self.surfaces);
        if let Some(tasks) = surfaces.get_mut(session_id) {
            tasks.remove(&installation);
            if tasks.is_empty() {
                surfaces.remove(session_id);
            }
        }
    }

    /// Stop the MCP servers of the tasks idle longest, beyond the few
    /// most recently used. They start again when the task runs. They are
    /// taken under the sessions' lock, which a run takes before it starts
    /// its servers, so a task that starts running keeps its own.
    async fn stop_idle_task_servers(&self, current: &str) {
        let sessions = self.sessions.lock().await;
        let running = self.running_task_ids();
        let mut idle: Vec<(std::time::Instant, &Arc<TaskMcp>)> = sessions
            .iter()
            .filter(|(id, _)| id.as_str() != current && !running.contains(*id))
            .filter_map(|(_, task)| Some((task.mcp.last_used()?, &task.mcp)))
            .collect();
        idle.sort_by_key(|(used, _)| std::cmp::Reverse(*used));
        let stopped: Vec<_> = idle
            .into_iter()
            .skip(MAX_IDLE_TASKS_WITH_SERVERS)
            .flat_map(|(_, mcp)| mcp.take_all())
            .collect();
        drop(sessions);
        for server in stopped {
            tokio::spawn(async move { server.shutdown().await });
        }
    }

    /// The models the runtime serves, side models included.
    pub(super) fn models(&self) -> &ModelRegistry {
        &self.models
    }

    /// Every request containing task history, including side questions, uses
    /// the same task-scoped attachment boundary.
    pub(super) fn attachment_models(&self, session_id: &str) -> ModelRegistry {
        super::provider::attachment_model_registry(
            &self.models,
            super::attachments::AgentAttachmentStore::new(
                Arc::clone(self.store.store()),
                self.store.account(),
            ),
            session_id.to_string(),
        )
    }

    /// A task's session for a side question: the loaded one as it is, or
    /// the task loaded on the model it last ran on.
    pub(super) async fn side_question_session(
        &self,
        session_id: &str,
    ) -> Result<AgentSession, String> {
        if let Some(session) = self.loaded_session(session_id).await {
            return Ok(session);
        }
        let row = self
            .store
            .get(session_id)?
            .ok_or_else(|| format!("Failed to find Agent task {session_id}"))?;
        let model_id = row.model.clone().unwrap_or_else(|| self.model.clone());
        self.task_session(&row, self.pi_model(&model_id, None, None).await)
            .await
    }

    /// Give a task named from its first prompt a generated title, unless
    /// its title changes first. Runs beside the task until the runtime
    /// stops.
    pub(super) fn generate_title(&self, session_id: &str, first_prompt: &str, fallback: String) {
        let models = self.models.clone();
        let store = Arc::clone(&self.store);
        let events = self.host.events.clone();
        let cancel = self.lifetime.child_token();
        let (session_id, first_prompt) = (session_id.to_string(), first_prompt.to_string());
        tokio::spawn(async move {
            let title = match super::side_models::generate_title(
                &models,
                &session_id,
                &first_prompt,
                cancel.clone(),
            )
            .await
            {
                Ok(Some(title)) if !cancel.is_cancelled() => title,
                Ok(_) => return,
                Err(error) => {
                    log::debug!("{error}");
                    return;
                }
            };
            let mut renamed = false;
            let updated = store.update(&session_id, |row| {
                if !row.title_user_set && row.title == fallback {
                    row.title = title;
                    renamed = true;
                }
            });
            match updated {
                Ok(Some(row)) if renamed => emit_agent_event(
                    &events,
                    AgentServiceEvent::SessionUpdated {
                        session_id: session_id.clone(),
                        run_id: None,
                        session: row.summary(),
                    },
                ),
                Ok(_) => {}
                Err(error) => log::warn!("Failed to save the generated Agent task title: {error}"),
            }
        });
    }

    /// Resolve the full catalog metadata for a task. Hosts may supply a
    /// newer window or vision flag; the developer's context override wins.
    pub(super) async fn pi_model(
        &self,
        model_id: &str,
        context_window: Option<u64>,
        vision: Option<bool>,
    ) -> Model {
        let mut entry = match self.api.model_catalog().await {
            Ok(catalog) => catalog_entry(&catalog, model_id).unwrap_or_default(),
            Err(error) => {
                log::warn!("Failed to fetch the task's model catalog: {error}");
                // A transient catalog failure must not clamp a loaded task's
                // saved thinking level to Off.
                if let Some(mut model) = self.models.find(super::provider::MAPLE_PROVIDER, model_id)
                {
                    model.context_window = context_limit_override()
                        .or(context_window)
                        .unwrap_or(model.context_window);
                    if let Some(vision) = vision {
                        model
                            .input
                            .retain(|input| *input != pi_ai::InputModality::Image);
                        if vision {
                            model.input.push(pi_ai::InputModality::Image);
                        }
                    }
                    return model;
                }
                CatalogEntry::default()
            }
        };
        entry.context_window = context_window.or(entry.context_window);
        entry.vision = vision.or(entry.vision);
        maple_model(model_id, Some(&entry), context_limit_override())
    }

    /// The task's Pi session, built on first use, on `model`. Its skills
    /// and instruction files are read again each time, so a change shows
    /// from the next run.
    pub(super) async fn task_session(
        &self,
        row: &TaskRow,
        model: Model,
    ) -> Result<AgentSession, String> {
        // Read before the sessions are locked: the first read asks a shell,
        // and the resources come from disk.
        let search_path = login_path::login_search_path().await;
        let resources = self.resources(&row.project_root).await;
        let agents_on = external_agents_on(&self.host.paths, &self.user_id);
        let mut sessions = self.sessions.lock().await;
        // Shutdown closes the loaded sessions; none is built after it.
        if self.lifetime.is_cancelled() {
            return Err(super::RUNTIME_NOT_RUNNING_ERROR.to_string());
        }
        // Every interface drives the same host-owned tools.
        let providers = task_providers(&agents_on, row);
        if let Some(LoadedTask {
            session,
            agents,
            last_used,
            unloading,
            ..
        }) = sessions.get_mut(&row.id)
        {
            if unloading.load(Ordering::Acquire) {
                return Err("Agent task is being unloaded".into());
            }
            *last_used = Instant::now();
            // A new context window or vision flag for the same model, or
            // another model for a task that has not started.
            if session.model().as_ref() != Some(&model) {
                self.models.register_models([model.clone()]);
                session.set_model(model).await;
            }
            // The web switch and the external agents may have changed since
            // the last run.
            tools::sync_web_tools(session, row.web_enabled);
            sync_external_agent_tools(session, agents, providers);
            if let Some(resources) = resources {
                session.set_resources(resources);
            }
            return Ok(session.clone());
        }
        // Registered first, so a resumed session finds the model it names.
        self.models.register_models([model.clone()]);
        let store = Arc::clone(&self.store);
        let (id, cwd) = (row.id.clone(), row.project_root.clone());
        let manager = tokio::task::spawn_blocking(move || store.open_session(&id, &cwd))
            .await
            .map_err(|error| format!("Failed to read the Agent task: {error}"))??;
        // A model that cannot see images gets them described.
        let describe = !model.supports_images();
        let attachments = Arc::new(super::attachments::AgentAttachmentStore::new(
            Arc::clone(self.store.store()),
            self.store.account(),
        ));
        let task_models = self.attachment_models(&row.id);
        let mut options =
            AgentSessionOptions::new(&row.project_root, APP_NAME, manager, task_models);
        options.model = Some(model);
        options.settings = self.settings();
        options.resources = resources.unwrap_or_default();

        let output_dir = crate::store::spill::SpillRoot::new(self.store.store().paths())
            .and_then(|spills| spills.output_dir(&row.id))
            .map_err(|error| error.to_string())?;
        let mut tools = tools::task_tools(tools::TaskToolsFor {
            session_id: row.id.clone(),
            kind: TaskKind::Desktop,
            login_path: search_path.clone(),
            questions: self.questions.clone(),
            web: self.api.clone(),
            web_enabled: row.web_enabled,
            read_image: tools::ReadImageFor {
                session_id: row.id.clone(),
                cwd: PathBuf::from(&row.project_root),
                attachments,
                models: self.models.clone(),
                describe,
            },
        });
        tools.options.output_dir = Some(output_dir.clone());
        options.tool_options = tools.options;
        options.builtin_tools = Some(tools.builtin);
        options.tools = tools.maple;
        options.extensions.push(tools.extension);
        let agents = TaskProviders::default();
        options
            .tools
            .extend(external_agent_tools(ExternalAgentToolsFor {
                registry: Arc::clone(&self.external_agents),
                session_id: row.id.clone(),
                working_dir: PathBuf::from(&row.project_root),
                login_path: search_path.clone(),
                providers: agents.clone(),
            }));
        let mcp = TaskMcp::new_with_output_dir(
            Path::new(&row.project_root),
            search_path,
            Some(output_dir),
        );
        mcp.set_oauth_context(super::mcp::oauth::OAuthAccountContext::new(
            self.store.clone(),
        ));
        options.extensions.push(mcp.extension());
        let task_cua = TaskCua::new(&self.account_scope, &row.id, self.models.clone());
        options.extensions.push(task_cua.extension());
        let session = AgentSession::new(options)
            .await
            .map_err(|error| format!("Failed to start the Agent task: {error}"))?;
        sync_external_agent_tools(&session, &agents, providers);
        sessions.insert(
            row.id.clone(),
            LoadedTask {
                session: session.clone(),
                mcp,
                cua: task_cua,
                agents,
                last_used: Instant::now(),
                unloading: Arc::new(AtomicBool::new(false)),
            },
        );
        drop(sessions);
        self.evict_idle_tasks(&row.id).await;
        Ok(session)
    }

    /// The skills, prompt templates and instruction files of a task in
    /// `cwd`, or `None` when they cannot be read.
    async fn resources(&self, cwd: &str) -> Option<Resources> {
        let (layout, user_id, cwd) = (
            self.host.paths.clone(),
            self.user_id.clone(),
            PathBuf::from(cwd),
        );
        let loaded =
            tokio::task::spawn_blocking(move || super::resources::load(&layout, &user_id, &cwd))
                .await
                .map_err(|error| error.to_string())
                .and_then(|loaded| loaded);
        match loaded {
            Ok(resources) => Some(resources),
            Err(error) => {
                log::warn!("Failed to read the task's skills and instructions: {error}");
                None
            }
        }
    }

    /// The settings of every session: Maple's retry budget, queued messages
    /// delivered together, the saved thinking default, and the host's opening
    /// instructions after Pi's own.
    fn settings(&self) -> Settings {
        let appended = self.host.harness_instructions().trim().to_string();
        Settings {
            retry: RETRY,
            // The transport times out a response that does not start or
            // stalls, with Maple's messages, so Pi's own timeout is off.
            http_idle_timeout_ms: Some(0),
            steering_mode: QueueMode::All,
            follow_up_mode: QueueMode::All,
            default_thinking_level: super::config::load_agent_config_inner(
                &self.host.paths,
                &self.user_id,
            )
            .map(|config| config.default_thinking_level)
            .unwrap_or_else(|error| {
                log::warn!("Failed to read the thinking default: {error}");
                None
            }),
            append_system_prompt: Some(appended).filter(|text| !text.is_empty()),
            ..Settings::default()
        }
    }

    /// Serve Maple's models with `stream` instead of the account's
    /// transport, for tests with a scripted provider.
    #[cfg(test)]
    pub(super) fn use_stream_fn(&self, stream: Arc<dyn pi_ai::StreamFn>) {
        self.models.register_api(super::provider::MAPLE_API, stream);
    }

    #[cfg(test)]
    pub(super) async fn hold_loaded_sessions_for_test(&self) -> impl Send + '_ {
        self.sessions.lock().await
    }

    /// Retry pending source writes without changing the lifetime or ownership
    /// of any loaded context. A failure in one task does not skip the others.
    pub(super) async fn flush_loaded_sessions(&self) -> Result<(), String> {
        let sessions = self
            .sessions
            .lock()
            .await
            .values()
            .map(|task| task.session.clone())
            .collect::<Vec<_>>();
        let mut first_error = None;
        for session in sessions {
            if let Err(error) = flush_session(&session).await {
                first_error.get_or_insert(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Stop every run, end detached work and external agents, and close the
    /// loaded sessions.
    pub(super) async fn shutdown(&self) -> Result<(), String> {
        self.lifetime.cancel();
        self.runs.stop_all().await;
        self.external_agents
            .shutdown_all(EXTERNAL_AGENTS_SHUTDOWN)
            .await;
        let sessions = self
            .sessions
            .lock()
            .await
            .values()
            .map(|task| task.session.clone())
            .collect::<Vec<_>>();
        // Keep every LoadedTask in the map until every session has shut down and
        // its persistence retry succeeds. The host must retain this runtime on
        // failure and keep its ownership lock/key until a successful retry.
        futures_util::future::join_all(sessions.iter().map(AgentSession::shutdown)).await;
        self.finish_shutdown().await
    }

    /// Finish saving an already-drained runtime without notifying extensions or
    /// stopping tools again. The service keeps this runtime non-runnable until
    /// every retained session is durable.
    pub(super) async fn finish_shutdown(&self) -> Result<(), String> {
        self.flush_loaded_sessions().await?;
        self.sessions.lock().await.clear();
        Ok(())
    }

    /// The service has awaited this runtime's drain, and an explicitly
    /// confirmed reset has committed or fenced the old generation for recovery.
    /// No second Pi shutdown fires.
    pub(super) async fn discard_drained_sessions(&self) {
        self.sessions.lock().await.clear();
    }
}

fn context_limit_override() -> Option<u64> {
    std::env::var(CONTEXT_LIMIT_OVERRIDE_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|limit| *limit > 0)
}

/// SQLite runs off the async executor; the session mutex serializes the retry
/// against any final external append.
async fn flush_session(session: &AgentSession) -> Result<(), String> {
    let session = session.clone();
    tokio::task::spawn_blocking(move || {
        session
            .flush()
            .map_err(|error| format!("Failed to save Agent history: {error}"))?;
        if session.has_unpersisted() {
            return Err("Agent history is still waiting to be saved".into());
        }
        Ok(())
    })
    .await
    .map_err(|error| format!("Failed to flush Agent history: {error}"))?
}
