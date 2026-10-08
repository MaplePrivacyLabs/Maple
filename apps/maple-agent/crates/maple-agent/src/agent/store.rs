//! Where tasks live: one JSONL session file per task, and the account's task
//! index beside them.
//!
//! The session file belongs to Pi: it holds the transcript, its tree and leaf,
//! and the model and thinking selections. The index is Maple's: the title and
//! whether the user set it, the project root, the kind of task, its state on
//! the active, settled and archived ladder, its web switch and its other
//! per-task settings. The index also caches the model, the message count and
//! the last update, so the task list opens without reading every file; those
//! caches are refreshed from the file and never fed back into Pi.
//!
//! The index is SQLite in WAL mode, so the desktop app and `maple-agent acp`
//! processes of one account can share it. Deleting a task marks its row,
//! removes the file and the task's attachments, then drops the row; a startup
//! finishes any deletion that was interrupted.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use pi_ai::Message;
use pi_coding_agent::SessionMessage;
use pi_coding_agent::session::{EntryKind, SessionManager};
use pi_coding_agent::store::JsonlStore;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use super::types::{AgentSessionSummary, AgentTaskState};

const SCHEMA_VERSION: i64 = 1;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Which surface created a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TaskKind {
    Desktop,
    Acp,
}

impl TaskKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Acp => "acp",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "acp" => Self::Acp,
            _ => Self::Desktop,
        }
    }
}

fn state_str(state: AgentTaskState) -> &'static str {
    match state {
        AgentTaskState::Active => "active",
        AgentTaskState::Settled => "settled",
        AgentTaskState::Archived => "archived",
    }
}

fn parse_state(value: &str) -> AgentTaskState {
    match value {
        "settled" => AgentTaskState::Settled,
        "archived" => AgentTaskState::Archived,
        _ => AgentTaskState::Active,
    }
}

/// One task's index row.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TaskRow {
    pub(crate) id: String,
    pub(crate) title: String,
    /// The user named the task; generated titles never replace it.
    pub(crate) title_user_set: bool,
    pub(crate) project_root: String,
    pub(crate) kind: TaskKind,
    pub(crate) state: AgentTaskState,
    pub(crate) web_enabled: bool,
    /// Cache: the model of the latest selection or response.
    pub(crate) model: Option<String>,
    /// Cache: user and assistant messages on the current branch.
    pub(crate) message_count: usize,
    pub(crate) created_ms: i64,
    /// Cache: the latest entry's time.
    pub(crate) updated_ms: i64,
    /// Maple's other per-task settings (MCP servers, integrations), as JSON.
    pub(crate) settings: Value,
}

impl TaskRow {
    pub(crate) fn new(
        id: String,
        title: String,
        project_root: String,
        kind: TaskKind,
        model: Option<String>,
        now_ms: i64,
    ) -> Self {
        Self {
            id,
            title,
            title_user_set: false,
            project_root,
            kind,
            state: AgentTaskState::Active,
            web_enabled: true,
            model,
            message_count: 0,
            created_ms: now_ms,
            updated_ms: now_ms,
            settings: Value::Object(Default::default()),
        }
    }

    pub(crate) fn summary(&self) -> AgentSessionSummary {
        AgentSessionSummary {
            id: self.id.clone(),
            title: self.title.clone(),
            project_root: self.project_root.clone(),
            created_ms: self.created_ms,
            updated_ms: self.updated_ms,
            message_count: self.message_count,
            model: self.model.clone(),
            web_enabled: self.web_enabled,
            state: self.state,
            acp: self.kind == TaskKind::Acp,
        }
    }
}

/// What a session file says about the caches in its index row.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SessionFacts {
    pub(crate) model: Option<String>,
    pub(crate) message_count: usize,
    pub(crate) updated_ms: Option<i64>,
}

impl SessionFacts {
    pub(crate) fn of(session: &SessionManager) -> Self {
        let branch = session.branch();
        let message_count = branch
            .iter()
            .filter(|entry| {
                matches!(
                    &entry.kind,
                    EntryKind::Message {
                        message: SessionMessage::Llm(Message::User(_) | Message::Assistant(_)),
                    }
                )
            })
            .count();
        Self {
            model: session.projection().model.map(|(_, id)| id),
            message_count,
            updated_ms: branch.last().map(|entry| entry.timestamp),
        }
    }
}

/// The account's task index and session folder.
pub(crate) struct TaskStore {
    sessions_dir: PathBuf,
    connection: Mutex<Connection>,
}

impl TaskStore {
    /// Open the index in `sessions_dir`, creating it when missing, and finish
    /// deletions an earlier process left half done.
    pub(crate) fn open(sessions_dir: &Path) -> Result<Self, String> {
        let path = sessions_dir.join(super::config::AGENT_TASK_INDEX_NAME);
        let connection = Connection::open(&path)
            .map_err(|error| format!("Failed to open the task index: {error}"))?;
        crate::private_file::set_owner_only_file(&path)
            .map_err(|error| format!("Failed to secure the task index: {error}"))?;
        connection
            .busy_timeout(BUSY_TIMEOUT)
            .and_then(|()| connection.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(())))
            .and_then(|()| {
                connection.execute_batch(
                    "CREATE TABLE IF NOT EXISTS tasks (
                        id TEXT PRIMARY KEY,
                        title TEXT NOT NULL,
                        title_user_set INTEGER NOT NULL DEFAULT 0,
                        project_root TEXT NOT NULL,
                        kind TEXT NOT NULL,
                        state TEXT NOT NULL,
                        web_enabled INTEGER NOT NULL DEFAULT 1,
                        model TEXT,
                        message_count INTEGER NOT NULL DEFAULT 0,
                        created_ms INTEGER NOT NULL,
                        updated_ms INTEGER NOT NULL,
                        settings TEXT NOT NULL DEFAULT '{}',
                        deleting INTEGER NOT NULL DEFAULT 0
                    );",
                )
            })
            .map_err(|error| format!("Failed to prepare the task index: {error}"))?;
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|error| format!("Failed to read the task index version: {error}"))?;
        if version > SCHEMA_VERSION {
            return Err("The task index was written by a newer Maple".to_string());
        }
        if version < SCHEMA_VERSION {
            connection
                .execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))
                .map_err(|error| format!("Failed to update the task index: {error}"))?;
        }
        let store = Self {
            sessions_dir: sessions_dir.to_path_buf(),
            connection: Mutex::new(connection),
        };
        store.finish_deletions();
        Ok(store)
    }

    fn connection(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The session file of a task, whether or not it exists yet.
    pub(crate) fn session_path(&self, id: &str) -> PathBuf {
        self.sessions_dir.join(format!("{id}.jsonl"))
    }

    pub(crate) fn insert(&self, row: &TaskRow) -> Result<(), String> {
        self.connection()
            .execute(
                "INSERT INTO tasks (id, title, title_user_set, project_root, kind, state,
                    web_enabled, model, message_count, created_ms, updated_ms, settings)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    row.id,
                    row.title,
                    row.title_user_set,
                    row.project_root,
                    row.kind.as_str(),
                    state_str(row.state),
                    row.web_enabled,
                    row.model,
                    row.message_count as i64,
                    row.created_ms,
                    row.updated_ms,
                    row.settings.to_string(),
                ],
            )
            .map_err(|error| format!("Failed to record the Agent task: {error}"))?;
        Ok(())
    }

    /// A task's row; `None` for an unknown task or one being deleted.
    pub(crate) fn get(&self, id: &str) -> Result<Option<TaskRow>, String> {
        self.connection()
            .query_row(
                "SELECT id, title, title_user_set, project_root, kind, state, web_enabled,
                    model, message_count, created_ms, updated_ms, settings
                 FROM tasks WHERE id = ?1 AND deleting = 0",
                [id],
                read_row,
            )
            .optional()
            .map_err(|error| format!("Failed to read the Agent task: {error}"))
    }

    /// The tasks, newest first, optionally only those of one project root.
    pub(crate) fn list(&self, project_root: Option<&str>) -> Result<Vec<TaskRow>, String> {
        let connection = self.connection();
        let mut statement = connection
            .prepare(
                "SELECT id, title, title_user_set, project_root, kind, state, web_enabled,
                    model, message_count, created_ms, updated_ms, settings
                 FROM tasks WHERE deleting = 0 AND (?1 IS NULL OR project_root = ?1)
                 ORDER BY updated_ms DESC, created_ms DESC",
            )
            .map_err(|error| format!("Failed to list Agent tasks: {error}"))?;
        let rows = statement
            .query_map([project_root], read_row)
            .map_err(|error| format!("Failed to list Agent tasks: {error}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("Failed to list Agent tasks: {error}"))?;
        Ok(rows)
    }

    /// Change a task's row; `None` when the task is unknown.
    pub(crate) fn update(
        &self,
        id: &str,
        change: impl FnOnce(&mut TaskRow),
    ) -> Result<Option<TaskRow>, String> {
        let mut connection = self.connection();
        let transaction = connection
            .transaction()
            .map_err(|error| format!("Failed to update the Agent task: {error}"))?;
        let Some(mut row) = transaction
            .query_row(
                "SELECT id, title, title_user_set, project_root, kind, state, web_enabled,
                    model, message_count, created_ms, updated_ms, settings
                 FROM tasks WHERE id = ?1 AND deleting = 0",
                [id],
                read_row,
            )
            .optional()
            .map_err(|error| format!("Failed to update the Agent task: {error}"))?
        else {
            return Ok(None);
        };
        change(&mut row);
        transaction
            .execute(
                "UPDATE tasks SET title = ?2, title_user_set = ?3, project_root = ?4, kind = ?5,
                    state = ?6, web_enabled = ?7, model = ?8, message_count = ?9,
                    updated_ms = ?10, settings = ?11
                 WHERE id = ?1",
                params![
                    row.id,
                    row.title,
                    row.title_user_set,
                    row.project_root,
                    row.kind.as_str(),
                    state_str(row.state),
                    row.web_enabled,
                    row.model,
                    row.message_count as i64,
                    row.updated_ms,
                    row.settings.to_string(),
                ],
            )
            .map_err(|error| format!("Failed to update the Agent task: {error}"))?;
        transaction
            .commit()
            .map_err(|error| format!("Failed to update the Agent task: {error}"))?;
        Ok(Some(row))
    }

    /// Bring a row's caches in line with its session file.
    pub(crate) fn refresh_caches(
        &self,
        id: &str,
        facts: &SessionFacts,
    ) -> Result<Option<TaskRow>, String> {
        self.update(id, |row| {
            if facts.model.is_some() {
                row.model = facts.model.clone();
            }
            row.message_count = facts.message_count;
            if let Some(updated_ms) = facts.updated_ms {
                row.updated_ms = row.updated_ms.max(updated_ms);
            }
        })
    }

    /// Open a task's session, or start it under the task's id when its file
    /// was never written.
    pub(crate) fn open_session(&self, id: &str, cwd: &str) -> Result<SessionManager, String> {
        let path = self.session_path(id);
        match JsonlStore::load(&path) {
            Ok(Some((header, entries))) => {
                if header.id != id {
                    return Err("The Agent task's session file belongs to another task".into());
                }
                Ok(SessionManager::open(
                    header,
                    entries,
                    Box::new(JsonlStore::new(path)),
                ))
            }
            Ok(None) => Ok(SessionManager::create_with_id(
                id,
                cwd,
                Box::new(JsonlStore::new(path)),
            )),
            Err(error) => Err(format!("Failed to read the Agent task's history: {error}")),
        }
    }

    /// Delete a task: mark its row, remove its session file and then the row.
    /// `remove_extra` removes the task's other files (its attachments) while
    /// the row is marked; a failure there is reported but does not keep the
    /// task.
    pub(crate) fn delete(
        &self,
        id: &str,
        remove_extra: impl FnOnce() -> Result<(), String>,
    ) -> Result<bool, String> {
        let marked = self
            .connection()
            .execute(
                "UPDATE tasks SET deleting = 1 WHERE id = ?1 AND deleting = 0",
                [id],
            )
            .map_err(|error| format!("Failed to delete the Agent task: {error}"))?;
        if marked == 0 {
            return Ok(false);
        }
        self.finish_deletion(id, remove_extra)?;
        Ok(true)
    }

    fn finish_deletion(
        &self,
        id: &str,
        remove_extra: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        for path in [
            self.session_path(id),
            self.session_path(id).with_extension("jsonl.tmp"),
        ] {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "Failed to delete the Agent task's history: {error}"
                    ));
                }
            }
        }
        if let Err(error) = remove_extra() {
            log::warn!("Deleted Agent task {id}, but failed to clear its files: {error}");
        }
        self.connection()
            .execute("DELETE FROM tasks WHERE id = ?1", [id])
            .map_err(|error| format!("Failed to delete the Agent task: {error}"))?;
        Ok(())
    }

    /// Finish deletions a stopped process left marked.
    fn finish_deletions(&self) {
        let marked: Vec<String> = {
            let connection = self.connection();
            let Ok(mut statement) = connection.prepare("SELECT id FROM tasks WHERE deleting = 1")
            else {
                return;
            };
            statement
                .query_map([], |row| row.get(0))
                .map(|rows| rows.flatten().collect())
                .unwrap_or_default()
        };
        for id in marked {
            if let Err(error) = self.finish_deletion(&id, || Ok(())) {
                log::warn!("Failed to finish deleting Agent task {id}: {error}");
            }
        }
    }
}

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRow> {
    let kind: String = row.get(4)?;
    let state: String = row.get(5)?;
    let message_count: i64 = row.get(8)?;
    let settings: String = row.get(11)?;
    Ok(TaskRow {
        id: row.get(0)?,
        title: row.get(1)?,
        title_user_set: row.get(2)?,
        project_root: row.get(3)?,
        kind: TaskKind::parse(&kind),
        state: parse_state(&state),
        web_enabled: row.get(6)?,
        model: row.get(7)?,
        message_count: usize::try_from(message_count).unwrap_or(0),
        created_ms: row.get(9)?,
        updated_ms: row.get(10)?,
        settings: serde_json::from_str(&settings)
            .unwrap_or_else(|_| Value::Object(Default::default())),
    })
}

#[cfg(test)]
mod tests;
