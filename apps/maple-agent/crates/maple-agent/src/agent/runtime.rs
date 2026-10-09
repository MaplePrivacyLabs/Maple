//! One account's running runtime: its model registry, its tasks' loaded
//! sessions and active runs, and the one factory every Pi session of the
//! account comes from.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pi_agent_core::QueueMode;
use pi_ai::{InputModality, Model, ThinkingLevel};
use pi_coding_agent::resources::Resources;
use pi_coding_agent::settings::{RetrySettings, Settings};
use pi_coding_agent::{AgentSession, AgentSessionOptions, ModelRegistry};
use tokio_util::sync::CancellationToken;

use super::config::{account_attachment_store, path_string};
use super::mcp::{
    MAX_IDLE_TASKS_WITH_SERVERS, STARTUP_WAIT, TaskMcp, read_saved_servers, task_servers,
};
use super::provider::{CatalogEntry, maple_model, maple_model_registry};
use super::questions::QuestionBroker;
use super::runs::{Failures, Runs};
use super::store::{TaskRow, TaskStore};
use super::tool_context::SharedAgentToolContext;
use super::{
    AgentRuntimeStatus, AgentServiceEvent, MapleAgentHostResources, emit_agent_event, login_path,
    tools,
};
use crate::maple_api::MapleApiSession;

/// The product name Pi's default system prompt names.
const APP_NAME: &str = "Maple";

/// Retries of a failed request: today's budget of about five minutes.
const RETRY: RetrySettings = RetrySettings {
    enabled: true,
    max_retries: 10,
    base_delay_ms: 1_000,
    max_delay_ms: 30_000,
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

/// A task's loaded session, and the MCP servers that live with it.
struct LoadedTask {
    session: AgentSession,
    mcp: Arc<TaskMcp>,
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
    pub(super) runs: Runs,
    pub(super) failures: Failures,
}

impl AgentRuntime {
    pub(super) fn new(parts: RuntimeParts) -> Self {
        let models = maple_model_registry(parts.api.clone(), []);
        // Ask the login shell for its PATH now, so the first task does not
        // wait for it.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(login_path::login_search_path());
        }
        Self {
            account_scope: parts.account_scope,
            user_id: parts.user_id,
            api: parts.api,
            store: parts.store,
            host: parts.host,
            questions: parts.questions,
            models,
            project_root: Mutex::new(parts.project_root),
            model: parts.model,
            lifetime: CancellationToken::new(),
            sessions: tokio::sync::Mutex::new(HashMap::new()),
            runs: Runs::default(),
            failures: Failures::default(),
        }
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

    pub(super) fn running_task_ids(&self) -> HashSet<String> {
        self.runs.running_task_ids()
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

    /// Forget the loaded sessions of a project's idle tasks, so they are
    /// built again with what changed.
    pub(super) async fn unload_project_tasks(&self, project_root: &str) {
        let running = self.running_task_ids();
        let unloaded: Vec<LoadedTask> = {
            let mut sessions = self.sessions.lock().await;
            let ids: Vec<String> = sessions
                .iter()
                .filter(|(id, task)| {
                    !running.contains(*id)
                        && task
                            .session
                            .with_session(|manager| manager.cwd() == project_root)
                })
                .map(|(id, _)| id.clone())
                .collect();
            ids.iter().filter_map(|id| sessions.remove(id)).collect()
        };
        futures_util::future::join_all(unloaded.iter().map(|task| task.session.shutdown())).await;
    }

    /// Forget a task's loaded session, which stops its MCP servers.
    pub(super) async fn unload_task(&self, session_id: &str) {
        let task = self.sessions.lock().await.remove(session_id);
        if let Some(task) = task {
            task.session.shutdown().await;
        }
    }

    /// A task's loaded session, if it is loaded.
    pub(super) async fn loaded_session(&self, session_id: &str) -> Option<AgentSession> {
        self.sessions
            .lock()
            .await
            .get(session_id)
            .map(|task| task.session.clone())
    }

    /// A loaded task's MCP servers.
    pub(super) async fn loaded_mcp(&self, session_id: &str) -> Option<Arc<TaskMcp>> {
        self.sessions
            .lock()
            .await
            .get(session_id)
            .map(|task| Arc::clone(&task.mcp))
    }

    /// Run the task's MCP servers as it has them now and the account saves
    /// them, and wait a little for those connecting, unless the run stops.
    /// Returns the notice of servers that failed or are still connecting.
    pub(super) async fn start_task_mcp(
        &self,
        session_id: &str,
        stopped: &CancellationToken,
    ) -> Option<String> {
        let mcp = self.loaded_mcp(session_id).await?;
        let row = self.store.get(session_id).ok().flatten()?;
        let (paths, user_id) = (self.host.paths.clone(), self.user_id.clone());
        let saved = tokio::task::spawn_blocking(move || read_saved_servers(&paths, &user_id))
            .await
            .map_err(|error| error.to_string())
            .and_then(|saved| saved);
        match saved {
            Ok(saved) => mcp.sync(task_servers(&saved, &row)),
            Err(error) => log::warn!("Failed to read the account's MCP servers: {error}"),
        }
        self.stop_idle_task_servers(session_id).await;
        tokio::select! {
            _ = mcp.wait(STARTUP_WAIT) => {}
            _ = stopped.cancelled() => return None,
        }
        mcp.take_notice()
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
        self.task_session(&row, self.pi_model(&model_id, None, false))
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

    /// The Pi model of a Maple model id. The host resolves the context
    /// window and vision from the catalog; the developer override wins.
    pub(super) fn pi_model(
        &self,
        model_id: &str,
        context_window: Option<u64>,
        vision: bool,
    ) -> Model {
        let entry = CatalogEntry {
            context_window,
            vision: Some(vision),
        };
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
        let mut sessions = self.sessions.lock().await;
        // Shutdown closes the loaded sessions; none is built after it.
        if self.lifetime.is_cancelled() {
            return Err(super::RUNTIME_NOT_RUNNING_ERROR.to_string());
        }
        if let Some(LoadedTask { session, .. }) = sessions.get(&row.id) {
            // A new context window or vision flag for the same model, or
            // another model for a task that has not started.
            if session.model().as_ref() != Some(&model) {
                self.models.register_models([model.clone()]);
                session.set_model(model).await;
            }
            // The web switch may have changed since the last run.
            tools::sync_web_tools(session, row.web_enabled);
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
        let describer = (!model.input.contains(&InputModality::Image)).then(|| self.models.clone());
        let mut options =
            AgentSessionOptions::new(&row.project_root, APP_NAME, manager, self.models.clone());
        options.model = Some(model);
        options.settings = self.settings();
        options.resources = resources.unwrap_or_default();
        let attachments = Arc::new(account_attachment_store(&self.host.paths, &self.user_id)?);
        let tools = tools::task_tools(tools::TaskToolsFor {
            session_id: row.id.clone(),
            kind: row.kind,
            tool_context: SharedAgentToolContext::new(self.host.default_tool_context.clone()),
            login_path: search_path.clone(),
            questions: self.questions.clone(),
            web: self.api.clone(),
            web_enabled: row.web_enabled,
            read_image: tools::ReadImageFor {
                session_id: row.id.clone(),
                cwd: PathBuf::from(&row.project_root),
                attachments,
                describer,
            },
        });
        options.tool_options = tools.options;
        options.builtin_tools = Some(tools.builtin);
        options.tools = tools.maple;
        let mcp = TaskMcp::new(Path::new(&row.project_root), search_path);
        options.extensions.push(mcp.extension());
        let session = AgentSession::new(options)
            .await
            .map_err(|error| format!("Failed to start the Agent task: {error}"))?;
        sessions.insert(
            row.id.clone(),
            LoadedTask {
                session: session.clone(),
                mcp,
            },
        );
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
    /// delivered together, no thinking control yet, and the host's opening
    /// instructions after Pi's own.
    fn settings(&self) -> Settings {
        let harness_instructions = self.host.harness_instructions();
        Settings {
            retry: RETRY,
            steering_mode: QueueMode::All,
            follow_up_mode: QueueMode::All,
            default_thinking_level: Some(ThinkingLevel::Off),
            append_system_prompt: Some(harness_instructions.trim().to_string())
                .filter(|text| !text.is_empty()),
            ..Settings::default()
        }
    }

    /// Serve Maple's models with `stream` instead of the account's
    /// transport, for tests with a scripted provider.
    #[cfg(test)]
    pub(super) fn use_stream_fn(&self, stream: Arc<dyn pi_ai::StreamFn>) {
        self.models.register_api(super::provider::MAPLE_API, stream);
    }

    /// Stop every run, end detached work and close the loaded sessions.
    pub(super) async fn shutdown(&self) {
        self.lifetime.cancel();
        self.runs.stop_all().await;
        let tasks: Vec<LoadedTask> = self
            .sessions
            .lock()
            .await
            .drain()
            .map(|(_, task)| task)
            .collect();
        futures_util::future::join_all(tasks.iter().map(|task| task.session.shutdown())).await;
    }
}

fn context_limit_override() -> Option<u64> {
    std::env::var(CONTEXT_LIMIT_OVERRIDE_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|limit| *limit > 0)
}
