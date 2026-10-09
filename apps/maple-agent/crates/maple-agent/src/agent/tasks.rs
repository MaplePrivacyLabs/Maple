//! Tasks: create, list, read, rename, settle, archive and delete.
//!
//! A task is a row in the account's index and, once it has run, a Pi
//! session file. Everything here works whether or not the account's runtime
//! runs, except creating a task, which starts in the runtime's project.

use std::path::Path;
use std::sync::Arc;

use icu_properties::{CodePointSetData, props::DefaultIgnorableCodePoint};
use pi_coding_agent::store::JsonlStore;

use super::config::{
    account_attachment_store, ensure_session_project_root_is_visible, load_agent_config_inner,
    normalize_project_root, path_string,
};
use super::mcp::{normalize_mcp_servers, servers_for_new_task, set_chosen_servers};
use super::store::{TaskKind, TaskRow};
use super::timeline::{MAX_AGENT_SESSION_TITLE_CHARS, session_timeline};
use super::{
    AgentCreateSessionRequest, AgentRenameSessionRequest, AgentRuntimeHandle, AgentServiceEvent,
    AgentSessionDetail, AgentSessionSummary, AgentSetSessionWebRequest, AgentTaskState,
    AgentTimelineItem, emit_agent_event,
};
use crate::maple_api::MapleApiSession;

/// The title of a task that has no prompt yet.
pub(super) const DEFAULT_AGENT_SESSION_TITLE: &str = "New task";
/// The title an ACP connection gives the tasks it creates.
pub(super) const ACP_SESSION_FALLBACK_TITLE: &str = "Maple ACP";

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
            system_prompt: None,
        });
        let config = {
            let _settings = self.lock_settings().await;
            load_agent_config_inner(self.paths(), &self.user_id)
                .map_err(|error| error.to_string())?
        };
        let mcp_servers = servers_for_new_task(
            &normalize_mcp_servers(config.mcp_servers.clone())?,
            request.mcp_server_names.as_deref(),
        )?;
        let root = match request.project_root.as_deref() {
            Some(path) if !path.trim().is_empty() => normalize_project_root(Path::new(path))?,
            _ => runtime.project_root(),
        };
        ensure_session_project_root_is_visible(&root, &config.removed_project_roots)?;
        let title = request
            .title
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_AGENT_SESSION_TITLE.to_string());
        let model = request.model.unwrap_or_else(|| runtime.model.clone());
        let mut row = TaskRow::new(
            pi_coding_agent::session::SessionManager::new_id(),
            title,
            path_string(&root),
            TaskKind::Desktop,
            Some(model),
            pi_ai::now_ms(),
        );
        set_chosen_servers(&mut row, mcp_servers);
        runtime.store.insert(&row)?;
        let summary = row.summary();
        emit_agent_event(
            &self.service.host.events,
            AgentServiceEvent::SessionCreated(summary.clone()),
        );
        Ok(AgentSessionDetail {
            session: summary,
            timeline: Vec::new(),
            mcp_errors: Vec::new(),
            queue: runtime.runs.queue_snapshot(&row.id),
        })
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
            .map(TaskRow::summary)
            .collect())
    }

    /// A task's summary, transcript and queue.
    pub async fn load_session(&self, session_id: String) -> Result<AgentSessionDetail, String> {
        let runtime = self.current_runtime().await?;
        let store = self.store()?;
        let row = store
            .get(&session_id)?
            .ok_or_else(|| format!("Failed to load Agent task: {session_id} was not found"))?;
        let loaded = match &runtime {
            Some(runtime) => runtime.loaded_session(&session_id).await,
            None => None,
        };
        let mut timeline = match loaded {
            Some(session) => session.with_session(session_timeline),
            None => {
                let path = store.session_path(&session_id);
                tokio::task::spawn_blocking(move || stored_timeline(&path))
                    .await
                    .map_err(|error| format!("Failed to load Agent task: {error}"))??
            }
        };
        let queue = match &runtime {
            Some(runtime) => {
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
        let tokens = match loaded {
            Some(session) => session.context_usage().map_or(0, |usage| usage.tokens),
            None => {
                let store = self.store()?;
                if store.get(session_id)?.is_none() {
                    return Err(format!("Failed to find Agent task {session_id}"));
                }
                let path = store.session_path(session_id);
                tokio::task::spawn_blocking(move || stored_context_tokens(&path))
                    .await
                    .map_err(|error| format!("Failed to read the Agent task: {error}"))??
            }
        };
        Ok(Some(tokens).filter(|tokens| *tokens > 0))
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
        if state != AgentTaskState::Active
            && let Some(runtime) = self.current_runtime().await?
            && runtime.runs.is_running(&session_id)
        {
            return Err(format!(
                "Stop the running agent before {} this task",
                state.gerund()
            ));
        }
        self.verify_generation().await?;
        let store = self.store()?;
        let current = store
            .get(&session_id)?
            .ok_or_else(|| format!("Failed to load Agent task: {session_id} was not found"))?;
        if current.state == state {
            return Ok(current.summary());
        }
        let row = store
            .update(&session_id, |row| row.state = state)?
            .ok_or_else(|| format!("Failed to load Agent task: {session_id} was not found"))?;
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
            runtime.unload_task(&session_id).await;
            runtime.runs.clear_queue(&session_id);
            runtime.failures.clear(&session_id);
        }
        let attachments = account_attachment_store(self.paths(), &self.user_id)?;
        let deleted = self
            .store()?
            .delete(&session_id, || attachments.delete_session(&session_id))?;
        if !deleted {
            return Err(format!("Failed to find Agent task {session_id}"));
        }
        Ok(())
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
        let store = account_attachment_store(self.paths(), &self.user_id)?;
        tokio::task::spawn_blocking(move || store.read(&session_id, &attachment_id))
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
                let model = runtime.pi_model(&model_id, None, false);
                runtime.task_session(&row, model).await?
            }
        };
        session
            .compact(None)
            .await
            .map_err(|error| format!("Compaction failed: {error}"))?;
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

/// The context size of a task's session file, as Pi estimates it; zero when
/// it has none yet.
fn stored_context_tokens(path: &Path) -> Result<u64, String> {
    match JsonlStore::load(path) {
        Ok(Some((header, entries))) => {
            let manager = pi_coding_agent::session::SessionManager::open(
                header,
                entries,
                Box::new(pi_coding_agent::store::MemoryStore),
            );
            Ok(pi_coding_agent::compaction::estimate_context_tokens(
                &manager.projection().messages,
            ))
        }
        Ok(None) => Ok(0),
        Err(error) => Err(format!("Failed to read the Agent task's history: {error}")),
    }
}

/// The transcript of a task's session file; empty when it has none yet.
fn stored_timeline(path: &Path) -> Result<Vec<AgentTimelineItem>, String> {
    match JsonlStore::load(path) {
        Ok(Some((header, entries))) => {
            let manager = pi_coding_agent::session::SessionManager::open(
                header,
                entries,
                Box::new(pi_coding_agent::store::MemoryStore),
            );
            Ok(session_timeline(&manager))
        }
        Ok(None) => Ok(Vec::new()),
        Err(error) => Err(format!("Failed to read the Agent task's history: {error}")),
    }
}

#[cfg(test)]
mod tests;
