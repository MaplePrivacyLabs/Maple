//! Tasks: create, list, read, rename, settle, archive and delete.
//!
//! A task is an account-authorized row in the host-owned store and, once it
//! has run, an append-only Pi source tree with materialized history rows. Everything here works whether or not the account's runtime
//! runs, except creating a task, which starts in the runtime's project.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use icu_properties::{CodePointSetData, props::DefaultIgnorableCodePoint};

use super::config::{
    ensure_session_project_root_is_visible, load_project_config_inner, normalize_project_root,
    path_string,
};
use super::mcp::{
    normalize_mcp_servers, servers_for_new_task, servers_with_project_defaults, set_chosen_servers,
};
use super::runtime::AgentRuntime;
use super::store::{TaskKind, TaskRow};
use super::timeline::{MAX_AGENT_SESSION_TITLE_CHARS, merge_into};
use super::{
    AgentCreateSessionRequest, AgentRenameSessionRequest, AgentRuntimeHandle, AgentServiceEvent,
    AgentSessionDetail, AgentSessionSummary, AgentSetSessionWebRequest, AgentTaskState,
    emit_agent_event,
};
use crate::maple_api::MapleApiSession;

/// The title of a task that has no prompt yet.
pub(super) const DEFAULT_AGENT_SESSION_TITLE: &str = "New task";
/// The title an ACP connection gives the tasks it creates.
pub(super) const ACP_SESSION_FALLBACK_TITLE: &str = "Maple ACP";
/// Why `/compact` did nothing: the history is all recent enough to keep.
pub const NOTHING_TO_COMPACT_ERROR: &str = "There is nothing to compact yet";
const HISTORY_PREPARATION_BUDGET: Duration = Duration::from_secs(5);
const HISTORY_PREPARING: &str =
    "Maple is preparing this task's saved history. Try opening it again shortly.";

/// A title from a task's first prompt: its text on one line, shortened.
pub(super) fn session_title_from_prompt(prompt: &str) -> String {
    let collapsed = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= MAX_AGENT_SESSION_TITLE_CHARS {
        return collapsed;
    }
    let mut title = collapsed
        .chars()
        .take(MAX_AGENT_SESSION_TITLE_CHARS - 1)
        .collect::<String>();
    title.truncate(title.trim_end().len());
    title.push('…');
    title
}

/// Whether a task still takes its title from its first prompt.
pub(super) fn names_from_prompt(row: &TaskRow) -> bool {
    row.message_count == 0
        && !row.title_user_set
        && (row.title == DEFAULT_AGENT_SESSION_TITLE || row.title == ACP_SESSION_FALLBACK_TITLE)
}

pub(super) fn normalize_user_provided_session_title(title: &str) -> Result<String, String> {
    let title = title.trim();
    if title.is_empty() {
        return Err("Agent task title cannot be empty".to_string());
    }
    if title
        .chars()
        .any(|character| character.is_control() || matches!(character, '\u{2028}' | '\u{2029}'))
    {
        return Err(
            "Agent task title must be a single line without control characters".to_string(),
        );
    }
    let default_ignorable = CodePointSetData::new::<DefaultIgnorableCodePoint>();
    if !title.chars().any(|character| {
        !character.is_whitespace()
            && !character.is_control()
            && !default_ignorable.contains(character)
    }) {
        return Err("Agent task title must contain visible characters".to_string());
    }
    if title.chars().count() > MAX_AGENT_SESSION_TITLE_CHARS {
        return Err(format!(
            "Agent task title must be {MAX_AGENT_SESSION_TITLE_CHARS} characters or fewer"
        ));
    }
    Ok(title.to_string())
}

fn session_id_argument(session_id: &str) -> Result<String, String> {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return Err("Agent task ID cannot be empty".to_string());
    }
    Ok(session_id.to_string())
}

impl AgentRuntimeHandle {
    #[cfg(feature = "acp")]
    pub(crate) fn user_id(&self) -> &str {
        &self.user_id
    }

    /// ACP captures a private immutable replay before writing any history.
    /// Raw SQLite access and every scratch lifetime remain owned by store/.
    #[cfg(feature = "acp")]
    pub(crate) fn history_replay_source(
        &self,
    ) -> Result<(crate::store::AccountId, Arc<crate::store::Store>), String> {
        self.verify_generation_now()?;
        let tasks = self.store()?;
        Ok((tasks.account(), Arc::clone(tasks.store())))
    }

    #[cfg(test)]
    pub(crate) fn test_persistence(
        &self,
    ) -> Result<(Arc<crate::store::Store>, crate::store::AccountId), String> {
        let tasks = self.store()?;
        Ok((Arc::clone(tasks.store()), tasks.account()))
    }

    /// Summary-only navigation. Readers never instantiate a Pi session or load
    /// source entries, task settings, or the cached context base.
    pub async fn list_tasks_page(
        &self,
        query: crate::store::NavigationTaskQuery,
        transport: crate::store::PageTransport,
    ) -> Result<crate::store::NavigationTaskPage, String> {
        self.verify_generation().await?;
        let tasks = self.store()?;
        tokio::task::spawn_blocking(move || {
            tasks
                .store()
                .list_task_summaries(tasks.account(), &query, transport)
                .map_err(|error| error.to_string())
        })
        .await
        .map_err(|_| "Task navigation worker failed".to_string())?
    }

    pub async fn task_summary(&self, task: String) -> Result<crate::store::TaskSummary, String> {
        self.verify_generation().await?;
        let tasks = self.store()?;
        tokio::task::spawn_blocking(move || {
            let mut reply = tasks
                .store()
                .get_task_summaries(tasks.account(), &[task], crate::store::PageTransport::Local)
                .map_err(|error| error.to_string())?;
            reply
                .tasks
                .pop()
                .ok_or_else(|| "The requested Agent task was not found".to_string())
        })
        .await
        .map_err(|_| "Task summary worker failed".to_string())?
    }

    pub async fn task_facts(&self, task: String) -> Result<crate::store::TaskFactsReply, String> {
        self.verify_generation().await?;
        let tasks = self.store()?;
        tokio::task::spawn_blocking(move || {
            tasks
                .store()
                .task_facts(tasks.account(), &task)
                .map_err(|error| error.to_string())
        })
        .await
        .map_err(|_| "Task facts worker failed".to_string())?
    }

    pub async fn open_task_page(
        &self,
        request: crate::store::OpenTaskPageRequest,
        transport: crate::store::PageTransport,
    ) -> Result<crate::store::OpenTaskPage, String> {
        self.ensure_task_history_ready(&request.task).await?;
        let tasks = self.store()?;
        tokio::task::spawn_blocking(move || {
            tasks
                .store()
                .open_task_page(tasks.account(), &request, transport)
                .map_err(|error| error.to_string())
        })
        .await
        .map_err(|_| "Task page worker failed".to_string())?
    }

    /// Migration deliberately removes obsolete derived history. Opening a task
    /// gives its existing bounded projector immediate work instead of showing a
    /// false empty conversation while it waits in the background queue. ACP uses
    /// this same boundary before capturing its immutable history replay.
    pub async fn ensure_task_history_ready(&self, task: &str) -> Result<(), String> {
        self.verify_generation().await?;
        let handle = self.clone();
        let task = task.to_owned();
        let deadline = Instant::now() + HISTORY_PREPARATION_BUDGET;
        tokio::task::spawn_blocking(move || handle.prepare_history_for_read(&task, deadline))
            .await
            .map_err(|_| "Task history preparation worker failed".to_string())?
    }

    pub(super) fn prepare_history_for_read(
        &self,
        task: &str,
        deadline: Instant,
    ) -> Result<(), String> {
        use crate::store::{RebuildProgress, StoreError};

        let tasks = self.store()?;
        let store = tasks.store();
        let account = tasks.account();
        let state = &self.service.state;
        let preparing = |error: StoreError| match error {
            StoreError::Busy => HISTORY_PREPARING.to_string(),
            other => other.to_string(),
        };
        'readiness: loop {
            self.verify_generation_now()?;
            match store.history_needs_rebuild(account, task) {
                Ok(false) => return Ok(()),
                Ok(true) => {}
                Err(StoreError::Busy) => {
                    self.retry_history_preparation(deadline)?;
                    continue;
                }
                Err(error) => return Err(preparing(error)),
            }
            if Instant::now() >= deadline {
                return Err(HISTORY_PREPARING.to_string());
            }
            let ticket = loop {
                let result = super::maintenance::while_task_idle(state, account, task, || {
                    self.verify_generation_now()
                        .map_err(|_| StoreError::NotFound)?;
                    // Completion can race the initial read or a Busy retry.
                    // Under admission, a finished task needs no new epoch.
                    if !store.history_needs_rebuild(account, task)? {
                        return Ok(None);
                    }
                    store.begin_timeline_rebuild(account, task, false).map(Some)
                });
                match result {
                    Ok(Some(ticket)) => break ticket,
                    Ok(None) => return Ok(()),
                    Err(StoreError::Busy) => self.retry_history_preparation(deadline)?,
                    Err(error) => return Err(preparing(error)),
                }
            };
            let prepared = loop {
                match store.prepare_timeline_rebuild_before(&ticket, Some(deadline)) {
                    Ok(prepared) => break prepared,
                    Err(StoreError::Busy) => self.retry_history_preparation(deadline)?,
                    // Another explicit open or the background worker completed
                    // this epoch. Recheck instead of reporting a failed load.
                    Err(StoreError::Conflict) => continue 'readiness,
                    Err(error) => return Err(preparing(error)),
                }
            };
            loop {
                self.verify_generation_now()?;
                let chunk = match store.prepare_timeline_chunk_before(&prepared, Some(deadline)) {
                    Ok(chunk) => chunk,
                    Err(StoreError::Busy) => {
                        self.retry_history_preparation(deadline)?;
                        continue;
                    }
                    Err(StoreError::Conflict) => continue 'readiness,
                    Err(error) => return Err(preparing(error)),
                };
                loop {
                    let progress =
                        super::maintenance::while_task_idle(state, account, task, || {
                            self.verify_generation_now()
                                .map_err(|_| StoreError::NotFound)?;
                            store.commit_timeline_chunk(&chunk, false)
                        });
                    match progress {
                        Ok(RebuildProgress::Pending { .. }) => break,
                        Ok(RebuildProgress::NeedsSnapshot) | Err(StoreError::Conflict) => {
                            continue 'readiness;
                        }
                        Ok(RebuildProgress::Replaced { epoch }) => {
                            emit_agent_event(
                                &self.service.host.events,
                                AgentServiceEvent::HistoryReplaced {
                                    user_id: self.user_id.to_string(),
                                    session_id: task.to_owned(),
                                    rows_epoch: epoch,
                                },
                            );
                            return Ok(());
                        }
                        // Keep the encoded chunk while admission or the writer
                        // is busy. Commit still checks its source and cursor.
                        Err(StoreError::Busy) => self.retry_history_preparation(deadline)?,
                        Err(error) => return Err(preparing(error)),
                    }
                }
            }
        }
    }

    fn retry_history_preparation(&self, deadline: Instant) -> Result<(), String> {
        self.verify_generation_now()?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(HISTORY_PREPARING.to_string());
        }
        // This runs only in the blocking worker, after all admission, writer
        // and read guards are released. Brief contention consumes the existing
        // budget rather than surfacing as an immediate failed task open.
        std::thread::sleep(remaining.min(Duration::from_millis(10)));
        if Instant::now() >= deadline {
            return Err(HISTORY_PREPARING.to_string());
        }
        Ok(())
    }

    pub async fn history_forward_page(
        &self,
        request: crate::store::HistoryPageRequest,
        transport: crate::store::PageTransport,
    ) -> Result<crate::store::HistoryPage, String> {
        self.verify_generation().await?;
        let tasks = self.store()?;
        tokio::task::spawn_blocking(move || {
            tasks
                .store()
                .history_forward_page(tasks.account(), &request, transport)
                .map_err(|error| error.to_string())
        })
        .await
        .map_err(|_| "Task history worker failed".to_string())?
    }

    pub async fn history_before_page(
        &self,
        request: crate::store::HistoryPageRequest,
        transport: crate::store::PageTransport,
    ) -> Result<crate::store::HistoryPage, String> {
        self.verify_generation().await?;
        let tasks = self.store()?;
        tokio::task::spawn_blocking(move || {
            tasks
                .store()
                .history_before_page(tasks.account(), &request, transport)
                .map_err(|error| error.to_string())
        })
        .await
        .map_err(|_| "Task history worker failed".to_string())?
    }

    /// A per-replay bounded row cache. Keep it only while the caller is
    /// authorized; dropping it wipes reconstructed payload allocations.
    pub async fn history_row_reader(
        &self,
    ) -> Result<(crate::store::AccountId, crate::store::RowReader), String> {
        self.verify_generation().await?;
        let tasks = self.store()?;
        Ok((
            tasks.account(),
            crate::store::RowReader::new(Arc::clone(tasks.store())),
        ))
    }

    /// Create a task in the requested project, or the runtime's.
    pub async fn create_session(
        &self,
        request: Option<AgentCreateSessionRequest>,
    ) -> Result<AgentSessionDetail, String> {
        self.ensure_accepting_new_work()?;
        let runtime = self.runtime().await?;
        let request = request.unwrap_or(AgentCreateSessionRequest {
            project_root: None,
            title: None,
            model: None,
            context_limit: None,
            mcp_server_names: None,
        });
        let row = self
            .new_task_row(&runtime, request, TaskKind::Desktop)
            .await?;
        runtime.store.insert(&row)?;
        let summary = row.summary();
        emit_agent_event(
            &self.service.host.events,
            AgentServiceEvent::SessionCreated(summary.clone()),
        );
        Ok(AgentSessionDetail {
            timeline_revision: 0,
            session: summary,
            timeline: Vec::new(),
            mcp_errors: Vec::new(),
            queue: runtime.runs.queue_snapshot(&row.id),
        })
    }

    /// The row of a new task of `kind`, not recorded yet: in the requested
    /// project or the runtime's, on the requested model or the runtime's,
    /// with the servers it asks for or those on for new tasks.
    pub(super) async fn new_task_row(
        &self,
        runtime: &AgentRuntime,
        request: AgentCreateSessionRequest,
        kind: TaskKind,
    ) -> Result<TaskRow, String> {
        let root = match request.project_root.as_deref() {
            Some(path) if !path.trim().is_empty() => normalize_project_root(Path::new(path))?,
            _ => runtime.project_root(),
        };
        let (config, mcp_servers) = {
            let _settings = self.lock_settings().await;
            self.verify_generation().await?;
            let config = load_project_config_inner(self.paths(), &self.user_id, &root)
                .map_err(|error| error.to_string())?;
            let mut servers = normalize_mcp_servers(config.mcp_servers.clone())?;
            if request.mcp_server_names.is_none() {
                servers =
                    servers_with_project_defaults(self.paths(), &self.user_id, &root, servers)?;
            }
            let chosen = servers_for_new_task(&servers, request.mcp_server_names.as_deref())?;
            (config, chosen)
        };
        ensure_session_project_root_is_visible(&root, &config.removed_project_roots)?;
        let fallback_title = match kind {
            TaskKind::Desktop => DEFAULT_AGENT_SESSION_TITLE,
            TaskKind::Acp => ACP_SESSION_FALLBACK_TITLE,
        };
        let title = request
            .title
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| fallback_title.to_string());
        let model = request.model.unwrap_or_else(|| runtime.model.clone());
        let mut row = TaskRow::new(
            pi_coding_agent::session::SessionManager::new_id(),
            title,
            path_string(&root),
            kind,
            Some(model),
            pi_ai::now_ms(),
        );
        set_chosen_servers(&mut row, mcp_servers);
        // Built-in CUA set up on this device is on for any new task when
        // the composer asks for it, or by the default for new tasks.
        if let Some(cua_on) = super::cua::choice_for_new_task(
            super::integrations::cua_default(self.paths(), &self.user_id),
            request.mcp_server_names.as_deref(),
        ) {
            super::cua::set_task_choice(&mut row, cua_on);
        }
        Ok(row)
    }

    /// The tasks, newest first, optionally only one project's.
    pub async fn list_sessions(
        &self,
        project_root: Option<String>,
    ) -> Result<Vec<AgentSessionSummary>, String> {
        self.verify_generation().await?;
        let root = project_root
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(|path| normalize_project_root(Path::new(path)).map(|root| path_string(&root)))
            .transpose()?;
        Ok(self
            .store()?
            .list(root.as_deref())?
            .iter()
            .filter(|row| super::surface::is_listed(row))
            .map(TaskRow::summary)
            .collect())
    }

    /// Select the model of a task's next run, at the same boundary that
    /// admits runs. A started task switches when that run starts.
    pub async fn set_task_model(
        &self,
        session_id: &str,
        model: &str,
    ) -> Result<AgentSessionSummary, String> {
        self.verify_generation().await?;
        let runtime = self.runtime().await?;
        let summary = runtime.runs.while_idle(session_id, || {
            runtime
                .store
                .update(session_id, |row| row.model = Some(model.into()))?
                .map(|row| row.summary())
                .ok_or_else(|| "Task was not found".into())
        })?;
        emit_agent_event(
            &runtime.host.events,
            AgentServiceEvent::SessionUpdated {
                session_id: session_id.into(),
                run_id: None,
                session: summary.clone(),
            },
        );
        Ok(summary)
    }

    /// A task's summary, transcript and queue.
    pub async fn load_session(&self, session_id: String) -> Result<AgentSessionDetail, String> {
        self.ensure_task_history_ready(&session_id).await?;
        let runtime = self.current_runtime().await?;
        let store = self.store()?;
        let row = store
            .get(&session_id)?
            .ok_or_else(|| format!("Failed to load Agent task: {session_id} was not found"))?;
        let (mut timeline, timeline_revision) = loop {
            let before = runtime
                .as_ref()
                .and_then(|runtime| runtime.runs.snapshot_marker(&session_id));
            // History reads use the committed projection regardless of whether
            // Pi is loaded. Viewing a task never builds its model context.
            let (history_store, history_id) = (Arc::clone(&store), session_id.clone());
            let mut timeline =
                tokio::task::spawn_blocking(move || history_store.timeline(&history_id))
                    .await
                    .map_err(|error| format!("Failed to load Agent task: {error}"))??;

            let (after, revision, live, has_more) = runtime
                .as_ref()
                .map(|runtime| {
                    runtime
                        .runs
                        .live_snapshot_page(&session_id, 500, 4 * 1024 * 1024)
                })
                .unwrap_or_default();
            if has_more {
                return Err(
                    "The active task exceeds the live replay limit; reload after its turn finishes"
                        .into(),
                );
            }
            if before != after {
                continue;
            }
            for item in live {
                merge_into(&mut timeline, item);
            }
            break (timeline, revision);
        };
        let queue = match &runtime {
            Some(runtime) => {
                // An external agent's turn in progress is on its row only
                // once it ends; until then the row is as the agent left it.
                for row in runtime.external_agents.live_rows(&session_id).await {
                    merge_into(&mut timeline, row);
                }
                if let Some(failure) = runtime.failures.get(&session_id) {
                    timeline.push(failure);
                }
                runtime.runs.queue_snapshot(&session_id)
            }
            None => super::AgentDesktopQueueSnapshot {
                revision: 0,
                items: Vec::new(),
            },
        };
        Ok(AgentSessionDetail {
            timeline_revision,
            session: row.summary(),
            timeline,
            mcp_errors: Vec::new(),
            queue,
        })
    }

    /// How many tokens of its model's context a task fills, as Pi counts
    /// them: the last reply's reported usage, plus an estimate of what came
    /// after it. `None` for a task with nothing in it yet.
    pub async fn session_context_tokens(&self, session_id: &str) -> Result<Option<u64>, String> {
        let runtime = self.current_runtime().await?;
        let loaded = match &runtime {
            Some(runtime) => runtime.loaded_session(session_id).await,
            None => None,
        };
        if let Some(session) = loaded {
            return Ok(session.context_usage().and_then(|usage| usage.tokens));
        }
        let store = self.store()?;
        if store.get(session_id)?.is_none() {
            return Err(format!("Failed to find Agent task {session_id}"));
        }
        let id = session_id.to_string();
        tokio::task::spawn_blocking(move || store.context_tokens(&id))
            .await
            .map_err(|error| format!("Failed to read the Agent task: {error}"))?
    }

    /// The task's current title, or `None` when it does not exist.
    pub async fn session_display_title(&self, session_id: &str) -> Result<Option<String>, String> {
        self.verify_generation().await?;
        Ok(self.store()?.get(session_id)?.map(|row| row.title))
    }

    /// Give a task the user's title. Generated titles never replace it.
    pub async fn rename_session(
        &self,
        maple_api_session: Arc<MapleApiSession>,
        request: AgentRenameSessionRequest,
    ) -> Result<AgentSessionSummary, String> {
        if maple_api_session.account_scope() != self.account_scope.as_ref() {
            return Err(
                "Maple API authentication belongs to a different signed-in account".to_string(),
            );
        }
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let session_id = request.session_id.trim().to_string();
        if session_id.is_empty() {
            return Err("Agent task rename requires a task ID".to_string());
        }
        let title = normalize_user_provided_session_title(&request.title)?;
        // A rename is the signed-in user's: not once they signed out.
        let _signed_in = maple_api_session.active_lease().await?;
        let row = self
            .store()?
            .update(&session_id, |row| {
                row.title = title;
                row.title_user_set = true;
            })?
            .ok_or_else(|| format!("Failed to rename Agent task: {session_id} was not found"))?;
        let summary = row.summary();
        emit_agent_event(
            &self.service.host.events,
            AgentServiceEvent::SessionUpdated {
                session_id,
                run_id: None,
                session: summary.clone(),
            },
        );
        Ok(summary)
    }

    /// Move a task between active, settled and archived. A running task can
    /// only be made active.
    pub async fn set_session_state(
        &self,
        session_id: String,
        state: AgentTaskState,
    ) -> Result<AgentSessionSummary, String> {
        self.ensure_accepting_new_work()?;
        let session_id = session_id_argument(&session_id)?;
        let runtime = self.current_runtime().await?;
        self.verify_generation().await?;
        let store = self.store()?;
        let task = session_id.clone();
        let archived = runtime
            .clone()
            .filter(|_| state == AgentTaskState::Archived);
        let summary = tokio::task::spawn_blocking(move || {
            let action = || {
                store
                    .update(&task, |row| row.state = state)?
                    .map(|row| row.summary())
                    .ok_or_else(|| format!("Failed to load Agent task: {task} was not found"))
            };
            if state != AgentTaskState::Active
                && let Some(runtime) = runtime
            {
                runtime
                    .maintenance_while_idle(&task, action)
                    .map_err(|error| {
                        if error == "The task is running; its model cannot change" {
                            "Stop the running agent before changing this task's state".to_owned()
                        } else {
                            error
                        }
                    })
            } else {
                action()
            }
        })
        .await
        .map_err(|_| "Task state worker did not complete".to_owned())??;
        if let Some(runtime) = archived {
            runtime
                .discard_python(&session_id, super::tools::ARCHIVED_REASON)
                .await;
        }
        emit_agent_event(
            &self.service.host.events,
            AgentServiceEvent::SessionUpdated {
                session_id,
                run_id: None,
                session: summary.clone(),
            },
        );
        Ok(summary)
    }

    /// Explicitly archive every task before hiding its project. Each scan and
    /// write batch is bounded; normal and autonomous run admission share the
    /// final check for every batch. A retry resumes already completed progress.
    pub async fn archive_project_root(
        &self,
        path: String,
        fallback: Option<String>,
    ) -> Result<(), String> {
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let path = path.trim().to_owned();
        if !super::config::structurally_valid_project_root(&path) {
            return Err("Project path must be an absolute folder path".into());
        }
        let _lifecycle = self.service.state.lifecycle.lock().await;
        let runtime = self.current_runtime().await?;
        if let Some(runtime) = &runtime {
            runtime.maintenance_while_all_idle(|| Ok(()))?;
        }
        let tasks = self.store()?;
        let mut total = 0usize;
        for _ in 0..3 {
            let mut cursor = None;
            let mut changed = 0usize;
            loop {
                self.verify_generation().await?;
                self.ensure_accepting_new_work()?;
                let (tasks, path, after, guard, events) = (
                    tasks.clone(),
                    path.clone(),
                    cursor,
                    runtime.clone(),
                    self.service.host.events.clone(),
                );
                let result = tokio::task::spawn_blocking(move || {
                    let batch = tasks
                        .store()
                        .archive_project_candidates(tasks.account(), &path, after.as_ref(), 200)
                        .map_err(|error| error.to_string())?;
                    let action = || {
                        let mut count = 0;
                        for task in batch.task_ids {
                            if let Some(row) =
                                tasks.update(&task, |row| row.state = AgentTaskState::Archived)?
                            {
                                emit_agent_event(
                                    &events,
                                    AgentServiceEvent::SessionUpdated {
                                        session_id: task,
                                        run_id: None,
                                        session: row.summary(),
                                    },
                                );
                                count += 1;
                            }
                        }
                        Ok::<_, String>((batch.next, count))
                    };
                    if let Some(runtime) = guard {
                        runtime.maintenance_while_all_idle(action)
                    } else {
                        action()
                    }
                })
                .await
                .map_err(|_| "Project archive worker did not complete".to_owned())?;
                let (next,count)=result.map_err(|error|format!("Project archive stopped after {total} tasks: {error}. Retry after agents are idle."))?;
                total += count;
                changed += count;
                cursor = next;
                if cursor.is_none() {
                    break;
                }
                tokio::task::yield_now().await;
            }
            if changed == 0 {
                self.remove_project_root(path, fallback).await?;
                return Ok(());
            }
        }
        Err(format!(
            "Project changed while archiving. {total} tasks were archived; retry to finish."
        ))
    }

    /// Turn the web tools on or off for one task, from its next turn.
    pub async fn set_session_web_enabled(
        &self,
        request: AgentSetSessionWebRequest,
    ) -> Result<AgentSessionSummary, String> {
        self.verify_generation().await?;
        self.ensure_accepting_new_work()?;
        let session_id = session_id_argument(&request.session_id)?;
        let row = self
            .store()?
            .update(&session_id, |row| row.web_enabled = request.enabled)?
            .ok_or_else(|| format!("Failed to load Agent task: {session_id} was not found"))?;
        Ok(row.summary())
    }

    /// Delete a task, its history and its attachments. Refused while it runs.
    pub async fn delete_session(&self, session_id: String) -> Result<(), String> {
        self.ensure_accepting_new_work()?;
        let session_id = session_id_argument(&session_id)?;
        let runtime = self.current_runtime().await?;
        if let Some(runtime) = &runtime {
            if runtime.runs.is_running(&session_id) {
                return Err("Stop the running agent before deleting this task".to_string());
            }
            runtime.unload_task(&session_id).await?;
            // Its external agents end with it, before its history goes.
            runtime.external_agents.shutdown_session(&session_id).await;
            runtime.runs.clear_queue(&session_id);
            runtime.failures.clear(&session_id);
        }
        let store = self.store()?;
        let delete_id = session_id.clone();
        let service = self.service.clone();
        let scope = Arc::clone(&self.account_scope);
        let generation = self.generation;
        let deleted = tokio::task::spawn_blocking(move || {
            // Re-read the current runtime under the service lifecycle gate.
            // A run can register while async unload/extension shutdown waits;
            // only the same admission lock may grant the final durable hide.
            super::maintenance::while_task_idle(&service.state, store.account(), &delete_id, || {
                Ok(if service.generation(&scope) != generation {
                    Err("Agent Mode data changed while this operation was waiting".to_string())
                } else {
                    store.delete(&delete_id)
                })
            }).map_err(|error| match error {
                crate::store::StoreError::Busy => "The task became active while deletion was waiting. Stop its work and try again.".to_string(),
                other => other.to_string(),
            })?
        })
            .await
            .map_err(|error| format!("Failed to delete the Agent task: {error}"))??;
        if !deleted {
            return Err(format!("Failed to find Agent task {session_id}"));
        }
        Ok(())
    }

    /// ACP's protocol-native images enter the same file store as desktop
    /// imports and remote streams. Sending subsequently uses committed IDs.
    pub async fn import_image_attachments(
        &self,
        session_id: String,
        images: Vec<super::AgentImageUpload>,
    ) -> Result<Vec<super::AgentAttachment>, String> {
        self.verify_generation().await?;
        let tasks = self.store()?;
        if tasks.get(&session_id)?.is_none() {
            return Err(format!("Failed to find Agent task {session_id}"));
        }
        let attachments = super::attachments::AgentAttachmentStore::new(
            Arc::clone(tasks.store()),
            tasks.account(),
        );
        tokio::task::spawn_blocking(move || attachments.store_uploads(&session_id, &images))
            .await
            .map_err(|error| format!("Attachment import failed: {error}"))?
    }

    /// Raw bytes of a stored image attachment, for display.
    pub async fn read_image_attachment(
        &self,
        session_id: String,
        attachment_id: String,
    ) -> Result<Vec<u8>, String> {
        self.verify_generation().await?;
        if self.store()?.get(&session_id)?.is_none() {
            return Err(format!("Failed to find Agent task {session_id}"));
        }
        let tasks = self.store()?;
        let attachments = super::attachments::AgentAttachmentStore::new(
            Arc::clone(tasks.store()),
            tasks.account(),
        );
        tokio::task::spawn_blocking(move || attachments.read(&session_id, &attachment_id))
            .await
            .map_err(|error| format!("Agent image attachment task failed: {error}"))?
    }

    /// Compact a task's history now (`/compact`). The caller reloads the
    /// task afterwards.
    pub async fn compact_session(&self, session_id: String) -> Result<(), String> {
        self.ensure_accepting_new_work()?;
        let runtime = self.runtime().await?;
        if runtime.runs.is_running(&session_id) {
            return Err("This Agent task is already running".to_string());
        }
        let row = runtime
            .store
            .get(&session_id)?
            .ok_or_else(|| format!("Failed to find Agent task {session_id}"))?;
        let session = match runtime.loaded_session(&session_id).await {
            Some(session) => session,
            None => {
                let model_id = row.model.clone().unwrap_or_else(|| runtime.model.clone());
                let model = runtime.pi_model(&model_id, None, None).await;
                runtime.task_session(&row, model).await?
            }
        };
        match session.compact(None).await {
            Ok(_) => {}
            Err(pi_coding_agent::AgentSessionError::NothingToCompact) => {
                return Err(NOTHING_TO_COMPACT_ERROR.to_string());
            }
            Err(error) => return Err(format!("Compaction failed: {error}")),
        }
        let facts = session.with_session(super::store::SessionFacts::of);
        if let Some(row) = runtime.store.refresh_caches(&session_id, &facts)? {
            emit_agent_event(
                &self.service.host.events,
                AgentServiceEvent::SessionUpdated {
                    session_id,
                    run_id: None,
                    session: row.summary(),
                },
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
