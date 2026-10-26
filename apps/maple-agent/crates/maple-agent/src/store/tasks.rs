//! Account-authorized task metadata and navigation queries for the new store.
//!
//! This module never opens legacy indexes or session files. Source entries and
//! their cached facts are written together by the session store; refreshing a
//! derived cache here is only for an explicit rebuild.

use std::collections::HashSet;
use std::path::{Component, Path};

use rusqlite::{OptionalExtension, Row, Transaction, params, params_from_iter};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{AccountId, Durability, Store, StoreChange, StoreError, StoreResult};

const MAX_PAGE: u16 = 200;
const MAX_TEXT_BYTES: usize = 16 * 1024;
const MAX_SETTINGS_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TaskKey(pub(super) i64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ProjectKey(pub(super) i64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskKind {
    Desktop,
    Acp,
}

impl TaskKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Acp => "acp",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Active,
    Settled,
    Archived,
}

impl TaskState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Settled => "settled",
            Self::Archived => "archived",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TaskFacts {
    pub model: Option<String>,
    pub thinking_level: Option<String>,
    pub message_count: u64,
    pub updated_ms: Option<i64>,
    pub context_usage: Option<u64>,
    pub context_estimate: u64,
    pub context_start_seq: Option<i64>,
    pub context_base: Option<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TaskRecord {
    pub key: TaskKey,
    pub uuid: String,
    pub project: ProjectKey,
    pub kind: TaskKind,
    pub state: TaskState,
    pub provisional: bool,
    pub title: String,
    pub title_user_set: bool,
    pub pinned_ms: Option<i64>,
    pub web_enabled: bool,
    pub settings: Value,
    pub created_ms: i64,
    pub updated_ms: i64,
    pub facts: TaskFacts,
    pub last_seq: i64,
    pub rows_epoch: i64,
    pub timeline_format: i64,
    pub last_row_seq: i64,
}

#[derive(Clone, Debug)]
pub struct NewTask {
    pub uuid: String,
    pub project_root: String,
    pub kind: TaskKind,
    pub title: String,
    pub provisional: bool,
    pub model: Option<String>,
    pub created_ms: i64,
}

/// Only user metadata may be changed through this path. Account, identity,
/// creation time and source caches are absent except the pre-conversation model.
#[derive(Clone, Debug, Default)]
pub struct TaskPatch {
    pub title: Option<String>,
    pub title_user_set: Option<bool>,
    pub project_root: Option<String>,
    pub state: Option<TaskState>,
    pub provisional: Option<bool>,
    pub web_enabled: Option<bool>,
    pub settings: Option<Value>,
    /// A selection before the first conversation. Source appends own it afterward.
    pub model: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "SerializedTaskSection", into = "SerializedTaskSection")]
pub enum TaskSection {
    State(TaskState),
    Pinned,
    /// ACP's recent sessions include empty committed tasks but not provisional
    /// sessions created only because an editor opened a folder.
    Recent,
}

/// Keep the request shape a plain section string while preserving the typed
/// state wrapper used by SQL query construction internally.
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SerializedTaskSection {
    Active,
    Settled,
    Archived,
    Pinned,
    Recent,
}

impl From<SerializedTaskSection> for TaskSection {
    fn from(value: SerializedTaskSection) -> Self {
        match value {
            SerializedTaskSection::Active => Self::State(TaskState::Active),
            SerializedTaskSection::Settled => Self::State(TaskState::Settled),
            SerializedTaskSection::Archived => Self::State(TaskState::Archived),
            SerializedTaskSection::Pinned => Self::Pinned,
            SerializedTaskSection::Recent => Self::Recent,
        }
    }
}

impl From<TaskSection> for SerializedTaskSection {
    fn from(value: TaskSection) -> Self {
        match value {
            TaskSection::State(TaskState::Active) => Self::Active,
            TaskSection::State(TaskState::Settled) => Self::Settled,
            TaskSection::State(TaskState::Archived) => Self::Archived,
            TaskSection::Pinned => Self::Pinned,
            TaskSection::Recent => Self::Recent,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PinFilter {
    Exclude,
    Ignore,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskPosition {
    pub updated_ms: i64,
    pub key: TaskKey,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveProjectCandidates {
    pub task_ids: Vec<String>,
    /// Last scanned metadata position, including an empty eligible batch.
    pub next: Option<TaskPosition>,
}

#[derive(Clone, Debug)]
pub struct TaskQuery {
    pub section: TaskSection,
    pub project: Option<ProjectKey>,
    pub pins: PinFilter,
    pub cursor: Option<TaskPosition>,
    pub limit: u16,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TaskPage {
    pub tasks: Vec<TaskRecord>,
    pub next: Option<TaskPosition>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProjectRecord {
    pub key: ProjectKey,
    pub root: String,
    pub display_name: Option<String>,
    pub name_revision: u64,
    pub position: Option<i64>,
    pub last_used_ms: i64,
    pub removed_ms: Option<i64>,
    pub trusted: Option<bool>,
    pub mcp_defaults: Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectPosition {
    pub unpositioned: bool,
    pub position: i64,
    pub root: String,
    pub key: ProjectKey,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProjectPage {
    pub projects: Vec<ProjectRecord>,
    pub next: Option<ProjectPosition>,
    pub archived_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DictionaryPage {
    pub words: Vec<String>,
    pub next: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccountDeletion {
    Pending { generation: String, started_ms: i64 },
    Confirmed { generation: String, started_ms: i64 },
}

const TASK_COLUMNS: &str = "id, uuid, project_id, kind, state, provisional, title, title_user_set, \
    pinned_ms, web_enabled, settings, created_ms, updated_ms, model, thinking_level, \
    message_count, context_usage, context_estimate, context_start_seq, context_base, \
    last_seq, rows_epoch, timeline_format, last_row_seq";
const PROJECT_COLUMNS: &str = "id, root, display_name, position, last_used_ms, removed_ms, trusted, mcp_defaults, name_revision";

impl Store {
    pub fn create_task(&self, account: AccountId, task: &NewTask) -> StoreResult<TaskRecord> {
        self.create_task_with_patch(account, task, &TaskPatch::default())
    }

    /// Record the initial metadata with the task, so a failed settings write
    /// never leaves an incompletely configured task visible to another client.
    pub fn create_task_with_patch(
        &self,
        account: AccountId,
        task: &NewTask,
        patch: &TaskPatch,
    ) -> StoreResult<TaskRecord> {
        validate_uuid(&task.uuid)?;
        validate_root(&task.project_root)?;
        validate_text(&task.title)?;
        if let Some(model) = &task.model {
            validate_text(model)?;
        }
        validate_patch(patch)?;
        let settings = patch.settings.as_ref().map(encode_settings).transpose()?;
        self.write_with_change(Durability::Durable, |tx| {
            writable_account(tx, account)?;
            let root = patch.project_root.as_deref().unwrap_or(&task.project_root);
            let project = ensure_project(tx, account, root, task.created_ms)?;
            let title = patch.title.as_deref().unwrap_or(&task.title);
            tx.execute(
                "INSERT INTO tasks (uuid, account_id, project_id, kind, state, provisional, title,
                    title_fold, created_ms, updated_ms, model, title_user_set, web_enabled, settings)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, ?10, ?11, ?12, ?13)",
                params![task.uuid, account.0, project.0, task.kind.as_str(),
                    patch.state.unwrap_or(TaskState::Active).as_str(),
                    patch.provisional.unwrap_or(task.provisional), title, title.to_lowercase(),
                    task.created_ms, patch.model.as_ref().or(task.model.as_ref()),
                    patch.title_user_set.unwrap_or(false), patch.web_enabled.unwrap_or(true),
                    settings.unwrap_or_else(|| "{}".to_owned())],
            )?;
            let record = task_by_uuid(tx, account, &task.uuid)?.ok_or(StoreError::NotFound)?;
            Ok((record, Some(task_change(account, &task.uuid))))
        })
    }

    pub fn task(&self, account: AccountId, uuid: &str) -> StoreResult<Option<TaskRecord>> {
        validate_uuid(uuid)?;
        self.read(|tx| task_by_uuid(tx, account, uuid))
    }

    pub fn update_task(
        &self,
        account: AccountId,
        uuid: &str,
        patch: &TaskPatch,
    ) -> StoreResult<Option<TaskRecord>> {
        self.update_task_checked(account, uuid, None, patch)
    }

    /// Preserve predicates evaluated by a read/modify/write caller. Source
    /// cache changes alone do not conflict with unrelated metadata edits.
    pub fn update_task_if(
        &self,
        account: AccountId,
        uuid: &str,
        expected: &TaskRecord,
        patch: &TaskPatch,
    ) -> StoreResult<Option<TaskRecord>> {
        self.update_task_checked(account, uuid, Some(expected), patch)
    }

    fn update_task_checked(
        &self,
        account: AccountId,
        uuid: &str,
        expected: Option<&TaskRecord>,
        patch: &TaskPatch,
    ) -> StoreResult<Option<TaskRecord>> {
        validate_uuid(uuid)?;
        validate_patch(patch)?;
        let settings = patch.settings.as_ref().map(encode_settings).transpose()?;
        self.write_with_change(Durability::Durable, |tx| {
            writable_account(tx, account)?;
            let Some(row) = task_by_uuid(tx, account, uuid)? else {
                return Ok((None, None));
            };
            if expected.is_some_and(|expected| !same_task_metadata(expected, &row)) {
                return Err(StoreError::Conflict);
            }
            if patch
                .model
                .as_ref()
                .is_some_and(|model| row.facts.model.as_ref() != Some(model))
                && row.facts.message_count != 0
            {
                return Err(StoreError::Conflict);
            }
            let project = match &patch.project_root {
                Some(root) => ensure_project(tx, account, root, row.updated_ms)?,
                None => row.project,
            };
            let title = patch.title.as_deref().unwrap_or(&row.title);
            tx.execute(
                "UPDATE tasks SET title = ?3, title_fold = ?4, title_user_set = ?5,
                    project_id = ?6, state = ?7, provisional = ?8, web_enabled = ?9,
                    settings = ?10, model = ?11
                 WHERE account_id = ?1 AND id = ?2 AND deleted_ms IS NULL",
                params![
                    account.0,
                    row.key.0,
                    title,
                    title.to_lowercase(),
                    patch.title_user_set.unwrap_or(row.title_user_set),
                    project.0,
                    patch.state.unwrap_or(row.state).as_str(),
                    patch.provisional.unwrap_or(row.provisional),
                    patch.web_enabled.unwrap_or(row.web_enabled),
                    settings.unwrap_or_else(|| row.settings.to_string()),
                    patch.model.as_ref().or(row.facts.model.as_ref())
                ],
            )?;
            Ok((
                task_by_uuid(tx, account, uuid)?,
                Some(task_change(account, uuid)),
            ))
        })
    }

    /// Compatibility and maintenance callers need empty/provisional tasks too.
    /// This remains a bounded account-authorized keyset query.
    pub fn all_tasks_page(
        &self,
        account: AccountId,
        project: Option<ProjectKey>,
        cursor: Option<TaskPosition>,
        limit: u16,
    ) -> StoreResult<TaskPage> {
        validate_limit(limit, MAX_PAGE)?;
        self.read(|tx| {
            let mut sql = format!(
                "SELECT {TASK_COLUMNS} FROM tasks INDEXED BY tasks_account
                WHERE account_id=? AND deleted_ms IS NULL"
            );
            let mut values = vec![rusqlite::types::Value::Integer(account.0)];
            if let Some(project) = project {
                sql.push_str(" AND project_id=?");
                values.push(rusqlite::types::Value::Integer(project.0));
            }
            if let Some(cursor) = cursor {
                sql.push_str(" AND id<?");
                values.push(rusqlite::types::Value::Integer(cursor.key.0));
            }
            sql.push_str(" ORDER BY id DESC LIMIT ?");
            values.push(rusqlite::types::Value::Integer(i64::from(limit) + 1));
            let mut statement = tx.prepare_cached(&sql)?;
            let mut tasks = statement
                .query_map(params_from_iter(values), read_task)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let more = tasks.len() > usize::from(limit);
            tasks.truncate(usize::from(limit));
            let next = more
                .then(|| {
                    tasks.last().map(|task| TaskPosition {
                        updated_ms: task.updated_ms,
                        key: task.key,
                    })
                })
                .flatten();
            Ok(TaskPage { tasks, next })
        })
    }

    pub fn set_task_pinned(
        &self,
        account: AccountId,
        uuid: &str,
        pinned: bool,
        now_ms: i64,
    ) -> StoreResult<bool> {
        validate_uuid(uuid)?;
        self.write_with_change(Durability::Durable, |tx| {
            writable_account(tx, account)?;
            let changed = tx.execute(
                "UPDATE tasks SET pinned_ms = CASE WHEN ?3 THEN coalesce(pinned_ms, ?4) ELSE NULL END
                 WHERE account_id = ?1 AND uuid = ?2 AND deleted_ms IS NULL",
                params![account.0, uuid, pinned, now_ms],
            )? != 0;
            Ok((changed, changed.then(|| task_change(account,uuid))))
        })
    }

    /// Rebuild only. Ordinary appends must update these fields in the same
    /// durable transaction as the source entry rather than calling this method.
    pub fn refresh_task_facts(
        &self,
        account: AccountId,
        uuid: &str,
        facts: &TaskFacts,
    ) -> StoreResult<bool> {
        validate_uuid(uuid)?;
        let message_count = db_integer(facts.message_count)?;
        let context_usage = facts.context_usage.map(db_integer).transpose()?;
        let estimate = db_integer(facts.context_estimate)?;
        let base = facts
            .context_base
            .as_ref()
            .map(encode_settings)
            .transpose()?;
        self.write_with_change(Durability::Normal, |tx| {
            writable_account(tx, account)?;
            let changed = tx.execute(
                "UPDATE tasks SET model = ?3, thinking_level = ?4, message_count = ?5,
                    context_usage = ?6, context_estimate = ?7, context_start_seq = ?8,
                    context_base = ?9, updated_ms = max(updated_ms, coalesce(?10, updated_ms))
                 WHERE account_id = ?1 AND uuid = ?2 AND deleted_ms IS NULL",
                params![
                    account.0,
                    uuid,
                    facts.model,
                    facts.thinking_level,
                    message_count,
                    context_usage,
                    estimate,
                    facts.context_start_seq,
                    base,
                    facts.updated_ms
                ],
            )? != 0;
            Ok((changed, changed.then(|| task_change(account, uuid))))
        })
    }

    pub fn mark_task_deleted(
        &self,
        account: AccountId,
        uuid: &str,
        now_ms: i64,
    ) -> StoreResult<bool> {
        validate_uuid(uuid)?;
        let result = self.write_with_change(Durability::Durable, |tx| {
            writable_account(tx, account)?;
            let changed = tx.execute(
                "UPDATE tasks SET deleted_ms = ?3
                 WHERE account_id = ?1 AND uuid = ?2 AND deleted_ms IS NULL",
                params![account.0, uuid, now_ms],
            )? != 0;
            Ok((changed, changed.then(|| task_change(account, uuid))))
        });
        // Drain outside the writer: readers may enter source quarantine.
        if let Err(error) = self.cancel_task_replays(account, uuid) {
            log::warn!("Deleted task replay cleanup failed: {error}");
        }
        result
    }

    /// A closing surface may discard only a draft that is still untouched in
    /// this same transaction. A concurrent first prompt or rename wins safely.
    pub fn mark_untouched_provisional_deleted(
        &self,
        account: AccountId,
        uuid: &str,
        now_ms: i64,
    ) -> StoreResult<bool> {
        validate_uuid(uuid)?;
        let result = self.write_with_change(Durability::Durable, |tx| {
            writable_account(tx, account)?;
            let changed = tx.execute(
                "UPDATE tasks SET deleted_ms=?3 WHERE account_id=?1 AND uuid=?2
                 AND deleted_ms IS NULL AND provisional=1 AND title_user_set=0
                 AND message_count=0 AND last_seq=0
                 AND NOT EXISTS (SELECT 1 FROM entries e WHERE e.task_id=tasks.id)
                 AND NOT EXISTS (SELECT 1 FROM file_attachments a WHERE a.task_id=tasks.id)",
                params![account.0, uuid, now_ms],
            )? != 0;
            Ok((changed, changed.then(|| task_change(account, uuid))))
        });
        // Drain outside the writer: readers may enter source quarantine.
        if let Err(error) = self.cancel_task_replays(account, uuid) {
            log::warn!("Deleted task replay cleanup failed: {error}");
        }
        result
    }

    pub fn list_tasks(&self, account: AccountId, query: &TaskQuery) -> StoreResult<TaskPage> {
        validate_limit(query.limit, MAX_PAGE)?;
        self.read(|tx| list_tasks_tx(tx, account, query))
    }

    /// Scan at most one metadata page with the existing project recency index.
    /// Archived/deleted/provisional rows still consume the scan budget, so a
    /// project with a large archived tail cannot force an unbounded filter scan.
    pub fn archive_project_candidates(
        &self,
        account: AccountId,
        root: &str,
        after: Option<&TaskPosition>,
        limit: u16,
    ) -> StoreResult<ArchiveProjectCandidates> {
        validate_root(root)?;
        validate_limit(limit, MAX_PAGE)?;
        if after.is_some_and(|cursor| cursor.key.0 < 1 || cursor.updated_ms < 0) {
            return Err(StoreError::InvalidInput);
        }
        self.read(|tx| {
            let project: Option<i64> = tx
                .query_row(
                    "SELECT id FROM projects WHERE account_id=?1 AND root=?2",
                    params![account.0, root],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(project) = project else {
                return Ok(ArchiveProjectCandidates {
                    task_ids: Vec::new(),
                    next: None,
                });
            };
            let mut sql = String::from(
                "SELECT id,uuid,updated_ms,provisional,state,deleted_ms IS NULL
                FROM tasks INDEXED BY tasks_project WHERE project_id=?1 AND account_id=?2",
            );
            let mut values = vec![
                rusqlite::types::Value::Integer(project),
                rusqlite::types::Value::Integer(account.0),
            ];
            if let Some(cursor) = after {
                sql.push_str(" AND (updated_ms,id)<(?3,?4)");
                values.push(rusqlite::types::Value::Integer(cursor.updated_ms));
                values.push(rusqlite::types::Value::Integer(cursor.key.0));
            }
            sql.push_str(" ORDER BY updated_ms DESC,id DESC LIMIT ?");
            values.push(rusqlite::types::Value::Integer(i64::from(limit)));
            let mut statement = tx.prepare_cached(&sql)?;
            let mut rows = statement.query(params_from_iter(values))?;
            let mut scanned = 0;
            let mut last = None;
            let mut task_ids = Vec::new();
            while let Some(row) = rows.next()? {
                scanned += 1;
                last = Some(TaskPosition {
                    key: TaskKey(row.get(0)?),
                    updated_ms: row.get(2)?,
                });
                if !row.get::<_, bool>(3)?
                    && row.get::<_, String>(4)? != "archived"
                    && row.get::<_, bool>(5)?
                {
                    task_ids.push(row.get(1)?);
                }
            }
            Ok(ArchiveProjectCandidates {
                task_ids,
                next: (scanned == usize::from(limit)).then_some(last).flatten(),
            })
        })
    }

    /// Controllers may issue as many bounded batches as necessary; there is no
    /// total pin count limit. Deleted and foreign-account ids never resolve.
    pub fn get_tasks(&self, account: AccountId, uuids: &[String]) -> StoreResult<Vec<TaskRecord>> {
        if uuids.len() > usize::from(MAX_PAGE) {
            return Err(StoreError::InvalidInput);
        }
        for uuid in uuids {
            validate_uuid(uuid)?;
        }
        let encoded = serde_json::to_string(uuids).map_err(|_| StoreError::InvalidInput)?;
        self.read(|tx| {
            let sql = format!(
                "SELECT {TASK_COLUMNS} FROM tasks WHERE account_id = ?1
                AND uuid IN (SELECT value FROM json_each(?2)) AND deleted_ms IS NULL
                ORDER BY updated_ms DESC, id DESC"
            );
            let mut statement = tx.prepare_cached(&sql)?;
            Ok(statement
                .query_map(params![account.0, encoded], read_task)?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn search_tasks(
        &self,
        account: AccountId,
        text: &str,
        matching_roots: &[String],
        limit: u16,
    ) -> StoreResult<Vec<TaskRecord>> {
        validate_text(text)?;
        validate_limit(limit, 100)?;
        if matching_roots.len() > 200 {
            return Err(StoreError::InvalidInput);
        }
        for root in matching_roots {
            validate_root(root)?;
        }
        let roots = serde_json::to_string(matching_roots).map_err(|_| StoreError::InvalidInput)?;
        self.read(|tx| {
            let sql = format!("SELECT {TASK_COLUMNS} FROM tasks INDEXED BY tasks_recent
                WHERE account_id = ?1 AND provisional = 0 AND deleted_ms IS NULL AND message_count > 0
                AND (instr(title_fold, ?2) > 0 OR project_id IN
                    (SELECT id FROM projects WHERE account_id = ?1 AND root IN (SELECT value FROM json_each(?3))))
                ORDER BY updated_ms DESC, id DESC LIMIT ?4");
            let mut statement = tx.prepare_cached(&sql)?;
            Ok(statement.query_map(params![account.0, text.to_lowercase(), roots, limit], read_task)?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn project(&self, account: AccountId, root: &str) -> StoreResult<Option<ProjectRecord>> {
        validate_root(root)?;
        self.read(|tx| {
            let sql = format!(
                "SELECT {PROJECT_COLUMNS} FROM projects WHERE account_id = ?1 AND root = ?2"
            );
            Ok(tx
                .query_row(&sql, params![account.0, root], read_project)
                .optional()?)
        })
    }

    /// Complete settings projection, including removed and task-only projects.
    /// Navigation callers use the paged project APIs instead.
    pub fn account_projects(&self, account: AccountId) -> StoreResult<Vec<ProjectRecord>> {
        self.read(|tx| {
            writable_account(tx, account)?;
            let sql = format!(
                "SELECT {PROJECT_COLUMNS} FROM projects WHERE account_id=?1
                ORDER BY position IS NULL,position,root,id"
            );
            let mut statement = tx.prepare_cached(&sql)?;
            Ok(statement
                .query_map([account.0], read_project)?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Replace explicit recent-project ordering without changing trust, names,
    /// MCP choices or removal tombstones. A staged restore remains hidden until
    /// its caller explicitly commits the updated removal set.
    pub fn replace_project_order(
        &self,
        account: AccountId,
        roots: &[(String, i64)],
    ) -> StoreResult<()> {
        let mut seen = HashSet::new();
        for (root, used_ms) in roots {
            validate_root(root)?;
            if *used_ms < 0 || !seen.insert(root.as_str()) {
                return Err(StoreError::InvalidInput);
            }
        }
        self.write_with_change(Durability::Durable, |tx| {
            writable_account(tx, account)?;
            tx.execute(
                "UPDATE projects SET position=NULL WHERE account_id=?1",
                [account.0],
            )?;
            for (position, (root, used_ms)) in roots.iter().enumerate() {
                let position = i64::try_from(position).map_err(|_| StoreError::InvalidInput)?;
                let project = ensure_project(tx, account, root, *used_ms)?;
                tx.execute(
                    "UPDATE projects SET position=?3,last_used_ms=?4 WHERE account_id=?1 AND id=?2",
                    params![account.0, project.0, position, used_ms],
                )?;
            }
            Ok((
                (),
                Some(StoreChange {
                    account: Some(account),
                    listed_changed: true,
                    ..StoreChange::default()
                }),
            ))
        })
    }

    /// Save the exact account-local removal set. Newly removed roots lose their
    /// recent position; an existing tombstone keeps its original timestamp.
    pub fn set_removed_project_roots(
        &self,
        account: AccountId,
        roots: &[String],
        now_ms: i64,
    ) -> StoreResult<()> {
        if now_ms < 0 {
            return Err(StoreError::InvalidInput);
        }
        let mut seen = HashSet::new();
        for root in roots {
            validate_root(root)?;
            if !seen.insert(root.as_str()) {
                return Err(StoreError::InvalidInput);
            }
        }
        let encoded = serde_json::to_string(roots).map_err(|_| StoreError::InvalidInput)?;
        self.write_with_change(Durability::Durable, |tx| {
            writable_account(tx, account)?;
            tx.execute(
                "UPDATE projects SET removed_ms=NULL WHERE account_id=?1 AND removed_ms IS NOT NULL
                AND root NOT IN (SELECT value FROM json_each(?2))",
                params![account.0, encoded],
            )?;
            for root in roots {
                let project = ensure_project(tx, account, root, now_ms)?;
                tx.execute(
                    "UPDATE projects SET position=CASE WHEN removed_ms IS NULL THEN NULL ELSE position END,
                    removed_ms=coalesce(removed_ms,?3) WHERE account_id=?1 AND id=?2",
                    params![account.0, project.0, now_ms],
                )?;
            }
            Ok(((), Some(StoreChange {
                account: Some(account), listed_changed: true, ..StoreChange::default()
            })))
        })
    }

    /// Save defaults and the supplied explicit trust changes atomically. Omitted
    /// projects retain their trust; bounded callers never submit an account map.
    pub fn save_agent_defaults(
        &self,
        account: AccountId,
        defaults: &Value,
        trust: &[(String, bool)],
        now_ms: i64,
    ) -> StoreResult<()> {
        let object = defaults.as_object().ok_or(StoreError::InvalidInput)?;
        if now_ms < 0
            || object.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "defaultProjectRoot" | "defaultModel" | "defaultThinkingLevel"
                )
            })
        {
            return Err(StoreError::InvalidInput);
        }
        validate_text(
            object
                .get("defaultModel")
                .and_then(Value::as_str)
                .ok_or(StoreError::InvalidInput)?,
        )?;
        if let Some(root) = object
            .get("defaultProjectRoot")
            .filter(|value| !value.is_null())
        {
            validate_root(root.as_str().ok_or(StoreError::InvalidInput)?)?;
        }
        if let Some(level) = object
            .get("defaultThinkingLevel")
            .filter(|value| !value.is_null())
        {
            validate_text(level.as_str().ok_or(StoreError::InvalidInput)?)?;
        }
        let encoded = encode_settings(defaults)?;
        let mut seen = HashSet::new();
        for (root, _) in trust {
            validate_root(root)?;
            if !seen.insert(root.as_str()) {
                return Err(StoreError::InvalidInput);
            }
        }
        self.write(Durability::Durable, |tx| {
            writable_account(tx, account)?;
            for (root, trusted) in trust {
                let project = ensure_project(tx, account, root, now_ms)?;
                tx.execute("UPDATE projects SET trusted=?3 WHERE account_id=?1 AND id=?2",
                    params![account.0, project.0, trusted])?;
            }
            tx.execute(
                "INSERT INTO account_settings(account_id,key,value,updated_ms) VALUES (?1,'agent.defaults',?2,?3)
                ON CONFLICT(account_id,key) DO UPDATE SET value=excluded.value,updated_ms=excluded.updated_ms",
                params![account.0, encoded, now_ms],
            )?;
            Ok(())
        })
    }

    pub fn project_by_key(
        &self,
        account: AccountId,
        key: ProjectKey,
    ) -> StoreResult<Option<ProjectRecord>> {
        self.read(|tx| {
            let sql =
                format!("SELECT {PROJECT_COLUMNS} FROM projects WHERE account_id=?1 AND id=?2");
            Ok(tx
                .query_row(&sql, params![account.0, key.0], read_project)
                .optional()?)
        })
    }

    /// Populate the navigation root map for exactly the projects in a task
    /// page, without truncating it to the recent-project page.
    pub fn get_projects(
        &self,
        account: AccountId,
        keys: &[ProjectKey],
    ) -> StoreResult<Vec<ProjectRecord>> {
        if keys.len() > usize::from(MAX_PAGE) {
            return Err(StoreError::InvalidInput);
        }
        let keys = serde_json::to_string(&keys.iter().map(|key| key.0).collect::<Vec<_>>())
            .map_err(|_| StoreError::InvalidInput)?;
        self.read(|tx| {
            let sql = format!(
                "SELECT {PROJECT_COLUMNS} FROM projects WHERE account_id=?1
                AND id IN (SELECT value FROM json_each(?2)) ORDER BY id"
            );
            let mut statement = tx.prepare_cached(&sql)?;
            Ok(statement
                .query_map(params![account.0, keys], read_project)?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// A provisional ACP task uses ensure_project without giving it a position.
    /// Only an explicit open/recent operation makes a root a recent project.
    pub fn register_explicit_project(
        &self,
        account: AccountId,
        root: &str,
        now_ms: i64,
    ) -> StoreResult<ProjectKey> {
        validate_root(root)?;
        if now_ms < 0 {
            return Err(StoreError::InvalidInput);
        }
        self.write_with_change(Durability::Durable, |tx| {
            writable_account(tx, account)?;
            let existing: Option<(i64, Option<i64>, Option<i64>)> = tx
                .query_row(
                    "SELECT id,position,removed_ms FROM projects WHERE account_id=?1 AND root=?2",
                    params![account.0, root],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            if let Some((id, Some(_), None)) = existing {
                return Ok((ProjectKey(id), None));
            }
            // Prepending uses one indexed seek and one point update, regardless
            // of the number of roots. Selecting an existing root keeps its order.
            let first: Option<i64> = tx
                .query_row(
                    "SELECT position FROM projects INDEXED BY projects_navigation
                    WHERE account_id=?1 AND removed_ms IS NULL AND (position IS NULL)=0
                    ORDER BY coalesce(position,0),root,id LIMIT 1",
                    [account.0],
                    |row| row.get(0),
                )
                .optional()?;
            let position = match first {
                Some(first) => first.checked_sub(1).ok_or(StoreError::InvalidInput)?,
                None => 0,
            };
            let project = ensure_project(tx, account, root, now_ms)?;
            tx.execute(
                "UPDATE projects SET position=?3,last_used_ms=?4,removed_ms=NULL
                WHERE account_id=?1 AND id=?2",
                params![account.0, project.0, position, now_ms],
            )?;
            Ok((
                project,
                Some(StoreChange {
                    account: Some(account),
                    listed_changed: true,
                    ..StoreChange::default()
                }),
            ))
        })
    }

    /// Assign an explicit rank for fixture initialization and the offered
    /// default workspace. User selection uses register_explicit_project.
    pub fn remember_project(
        &self,
        account: AccountId,
        root: &str,
        position: i64,
        now_ms: i64,
    ) -> StoreResult<ProjectKey> {
        validate_root(root)?;
        if position < 0 {
            return Err(StoreError::InvalidInput);
        }
        self.write(Durability::Durable, |tx| {
            writable_account(tx, account)?;
            let project = ensure_project(tx, account, root, now_ms)?;
            tx.execute(
                "UPDATE projects SET position = ?3, last_used_ms = ?4, removed_ms = NULL
                WHERE account_id = ?1 AND id = ?2",
                params![account.0, project.0, position, now_ms],
            )?;
            Ok(project)
        })
    }

    /// Commit a label and its monotonically increasing account-local revision
    /// together. Resetting a label retains a revision so stale page responses
    /// cannot reintroduce an older custom name.
    pub fn set_project_display_name(
        &self,
        account: AccountId,
        root: &str,
        name: Option<&str>,
        now_ms: i64,
    ) -> StoreResult<bool> {
        validate_root(root)?;
        if now_ms < 0 {
            return Err(StoreError::InvalidInput);
        }
        if let Some(name) = name {
            validate_text(name)?;
        }
        let name = name.filter(|name| !name.is_empty());
        self.write(Durability::Durable, |tx| {
            writable_account(tx, account)?;
            let project = ensure_project(tx, account, root, now_ms)?;
            let current: Option<String> = tx.query_row(
                "SELECT display_name FROM projects WHERE account_id=?1 AND id=?2",
                params![account.0, project.0], |row| row.get(0))?;
            if current.as_deref() == name { return Ok(false); }
            let previous: Option<String> = tx.query_row(
                "SELECT value FROM account_settings WHERE account_id=?1 AND key='project.names.revision'",
                [account.0], |row| row.get(0)).optional()?;
            let previous = match previous {
                Some(value) => value.parse::<i64>().ok().filter(|value| *value >= 0).ok_or(StoreError::InvalidData)?,
                None => 0,
            };
            let revision = previous.checked_add(1).filter(|value| *value > 0).ok_or(StoreError::InvalidData)?;
            tx.execute(
                "INSERT INTO account_settings(account_id,key,value,updated_ms) VALUES (?1,'project.names.revision',?2,?3)
                ON CONFLICT(account_id,key) DO UPDATE SET value=excluded.value,updated_ms=excluded.updated_ms",
                params![account.0, revision.to_string(), now_ms])?;
            tx.execute(
                "UPDATE projects SET display_name=?3,name_revision=?4 WHERE account_id=?1 AND id=?2",
                params![account.0, project.0, name, revision])?;
            Ok(true)
        })
    }

    /// Point-query membership uses the same visibility rule as navigation.
    /// Trust-only or otherwise unlisted records do not authorize a rename.
    pub fn listed_project(
        &self,
        account: AccountId,
        root: &str,
    ) -> StoreResult<Option<ProjectRecord>> {
        validate_root(root)?;
        self.read(|tx| {
            let sql = format!("SELECT {PROJECT_COLUMNS} FROM projects p WHERE account_id=?1 AND root=?2
                AND removed_ms IS NULL AND (position IS NOT NULL OR EXISTS
                    (SELECT 1 FROM tasks t WHERE t.account_id=?1 AND t.project_id=p.id AND t.provisional=0
                        AND t.message_count>0 AND t.deleted_ms IS NULL))");
            Ok(tx.query_row(&sql, params![account.0,root], read_project).optional()?)
        })
    }

    pub fn set_project_trust(
        &self,
        account: AccountId,
        root: &str,
        trusted: Option<bool>,
        now_ms: i64,
    ) -> StoreResult<()> {
        validate_root(root)?;
        self.write(Durability::Durable, |tx| {
            writable_account(tx, account)?;
            let project = ensure_project(tx, account, root, now_ms)?;
            tx.execute(
                "UPDATE projects SET trusted = ?3 WHERE account_id = ?1 AND id = ?2",
                params![account.0, project.0, trusted],
            )?;
            Ok(())
        })
    }

    pub fn set_project_mcp_defaults(
        &self,
        account: AccountId,
        root: &str,
        defaults: &Value,
        now_ms: i64,
    ) -> StoreResult<()> {
        validate_root(root)?;
        if !defaults.is_object()
            || defaults
                .as_object()
                .is_some_and(|map| map.values().any(|v| !v.is_boolean()))
        {
            return Err(StoreError::InvalidInput);
        }
        let encoded = encode_settings(defaults)?;
        self.write(Durability::Durable, |tx| {
            writable_account(tx, account)?;
            let project = ensure_project(tx, account, root, now_ms)?;
            tx.execute(
                "UPDATE projects SET mcp_defaults = ?3 WHERE account_id = ?1 AND id = ?2",
                params![account.0, project.0, encoded],
            )?;
            Ok(())
        })
    }

    /// Removing a root hides the switcher entry, retaining tasks and trust.
    pub fn remove_project(&self, account: AccountId, root: &str, now_ms: i64) -> StoreResult<bool> {
        validate_root(root)?;
        self.write(Durability::Durable, |tx| {
            writable_account(tx, account)?;
            Ok(tx.execute(
                "UPDATE projects SET position = NULL, removed_ms = ?3
                WHERE account_id = ?1 AND root = ?2",
                params![account.0, root, now_ms],
            )? != 0)
        })
    }

    /// Keyset-page project groups before reading any tasks, so projects cannot
    /// vanish because their tasks fall beyond a global task page.
    pub fn project_groups(
        &self,
        account: AccountId,
        cursor: Option<&ProjectPosition>,
        limit: u16,
    ) -> StoreResult<ProjectPage> {
        validate_limit(limit, 50)?;
        self.read(|tx| {
            let mut sql = format!("SELECT {PROJECT_COLUMNS} FROM projects AS p
                WHERE p.account_id = ?1 AND p.removed_ms IS NULL AND EXISTS
                (SELECT 1 FROM tasks t WHERE t.account_id = ?1 AND t.project_id = p.id
                    AND t.state IN ('active','settled') AND t.provisional = 0
                    AND t.message_count > 0 AND t.deleted_ms IS NULL)");
            let mut values = vec![rusqlite::types::Value::Integer(account.0)];
            if let Some(cursor) = cursor {
                sql.push_str(" AND (p.position IS NULL, coalesce(p.position,0), p.root, p.id) > (?2,?3,?4,?5)");
                values.extend([rusqlite::types::Value::Integer(i64::from(cursor.unpositioned)),
                    rusqlite::types::Value::Integer(cursor.position), rusqlite::types::Value::Text(cursor.root.clone()),
                    rusqlite::types::Value::Integer(cursor.key.0)]);
            }
            sql.push_str(" ORDER BY p.position IS NULL, p.position, p.root, p.id LIMIT ?");
            values.push(rusqlite::types::Value::Integer(i64::from(limit) + 1));
            let mut statement = tx.prepare_cached(&sql)?;
            let mut projects = statement.query_map(params_from_iter(values), read_project)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let more = projects.len() > usize::from(limit);
            projects.truncate(usize::from(limit));
            let next = more.then(|| projects.last().map(project_position)).flatten();
            let archived_count: i64 = tx.query_row("SELECT count(*) FROM tasks WHERE account_id = ?1
                AND state = 'archived' AND provisional = 0 AND message_count > 0 AND deleted_ms IS NULL",
                [account.0], |row| row.get(0))?;
            Ok(ProjectPage { projects, next, archived_count: archived_count as u64 })
        })
    }

    pub fn recent_projects(
        &self,
        account: AccountId,
        limit: u16,
    ) -> StoreResult<Vec<ProjectRecord>> {
        validate_limit(limit, MAX_PAGE)?;
        self.read(|tx| {
            let sql = format!("SELECT {PROJECT_COLUMNS} FROM projects AS p WHERE account_id = ?1
                AND removed_ms IS NULL AND (position IS NOT NULL OR EXISTS
                    (SELECT 1 FROM tasks t WHERE t.account_id = ?1 AND t.project_id = p.id AND t.provisional = 0
                        AND t.message_count > 0 AND t.deleted_ms IS NULL))
                ORDER BY position IS NULL, position, root, id LIMIT ?2");
            let mut statement = tx.prepare_cached(&sql)?;
            Ok(statement.query_map(params![account.0, limit], read_project)?.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn app_setting(&self, key: &str) -> StoreResult<Option<Value>> {
        validate_setting_key(key)?;
        self.read(|tx| read_setting(tx, "SELECT value FROM app_settings WHERE key = ?1", [key]))
    }

    pub fn set_app_setting(&self, key: &str, value: &Value, now_ms: i64) -> StoreResult<()> {
        validate_setting_key(key)?;
        let value = encode_settings(value)?;
        self.write(Durability::Durable, |tx| {
            tx.execute(
                "INSERT INTO app_settings (key,value,updated_ms) VALUES (?1,?2,?3)
                ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_ms=excluded.updated_ms",
                params![key, value, now_ms],
            )?;
            Ok(())
        })
    }

    pub fn account_setting(&self, account: AccountId, key: &str) -> StoreResult<Option<Value>> {
        validate_setting_key(key)?;
        self.read(|tx| {
            read_setting(
                tx,
                "SELECT value FROM account_settings WHERE account_id = ?1 AND key = ?2",
                params![account.0, key],
            )
        })
    }

    pub fn set_account_setting(
        &self,
        account: AccountId,
        key: &str,
        value: &Value,
        now_ms: i64,
    ) -> StoreResult<()> {
        validate_setting_key(key)?;
        let value = encode_settings(value)?;
        self.write(Durability::Durable, |tx| {
            writable_account(tx, account)?;
            tx.execute("INSERT INTO account_settings (account_id,key,value,updated_ms) VALUES (?1,?2,?3,?4)
                ON CONFLICT(account_id,key) DO UPDATE SET value=excluded.value,updated_ms=excluded.updated_ms",
                params![account.0, key, value, now_ms])?;
            Ok(())
        })
    }

    pub fn dictionary_words(&self, after: Option<&str>, limit: u16) -> StoreResult<DictionaryPage> {
        validate_limit(limit, MAX_PAGE)?;
        if let Some(after) = after {
            validate_text(after)?;
        }
        self.read(|tx| {
            let mut words = if let Some(after) = after {
                let mut statement = tx.prepare_cached(
                    "SELECT word FROM dictionary_words WHERE word>?1 ORDER BY word LIMIT ?2",
                )?;
                statement
                    .query_map(params![after, i64::from(limit) + 1], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<String>>>()?
            } else {
                let mut statement =
                    tx.prepare_cached("SELECT word FROM dictionary_words ORDER BY word LIMIT ?1")?;
                statement
                    .query_map([i64::from(limit) + 1], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<String>>>()?
            };
            let more = words.len() > usize::from(limit);
            words.truncate(usize::from(limit));
            let next = more.then(|| words.last().cloned()).flatten();
            Ok(DictionaryPage { words, next })
        })
    }

    pub fn add_dictionary_word(&self, word: &str) -> StoreResult<bool> {
        let word = word.trim().trim_matches('\'');
        validate_text(word)?;
        if word.is_empty() || word.contains(['\n', '\r']) {
            return Err(StoreError::InvalidInput);
        }
        self.write(Durability::Durable, |tx| {
            Ok(tx.execute(
                "INSERT INTO dictionary_words(word) VALUES (?1) ON CONFLICT(word) DO NOTHING",
                [word],
            )? != 0)
        })
    }

    pub fn remove_dictionary_word(&self, word: &str) -> StoreResult<bool> {
        validate_text(word)?;
        self.write(Durability::Durable, |tx| {
            Ok(tx.execute("DELETE FROM dictionary_words WHERE word=?1", [word])? != 0)
        })
    }

    /// Records an attempt only. No task tombstones are changed until a proven
    /// server success confirms the same generation.
    pub fn begin_account_deletion(
        &self,
        account: AccountId,
        generation: &str,
        now_ms: i64,
    ) -> StoreResult<()> {
        validate_generation(generation)?;
        let result = self.write(Durability::Durable, |tx| {
            match deletion_tx(tx, account)? {
                Some(AccountDeletion::Pending { generation: current, .. }) if current == generation => return Ok(()),
                Some(_) => return Err(StoreError::Conflict),
                None => {}
            }
            if tx.execute("UPDATE accounts SET deleting_ms=?3,deletion_phase='pending_server',deletion_generation=?2
                WHERE id=?1 AND deletion_phase IS NULL AND deleting_ms IS NULL",
                params![account.0, generation, now_ms])? == 0 { return Err(StoreError::NotFound); }
            Ok(())
        });
        if let Err(error) = self.cancel_account_replays(account) {
            log::warn!("Deleting account replay cleanup failed: {error}");
        }
        result
    }

    pub fn confirm_account_deletion(
        &self,
        account: AccountId,
        generation: &str,
    ) -> StoreResult<()> {
        validate_generation(generation)?;
        let result = self.write(Durability::Durable, |tx| {
            match deletion_tx(tx, account)? {
                Some(AccountDeletion::Confirmed { generation: current, .. }) if current == generation => {},
                Some(AccountDeletion::Pending { generation: current, .. }) if current == generation => {},
                _ => return Err(StoreError::Conflict),
            }
            tx.execute("UPDATE accounts SET deletion_phase='confirmed' WHERE id=?1 AND deletion_generation=?2
                AND deletion_phase='pending_server'", params![account.0, generation])?;
            tx.execute("UPDATE tasks SET deleted_ms=(SELECT deleting_ms FROM accounts WHERE id=?1)
                WHERE account_id=?1 AND deleted_ms IS NULL", [account.0])?;
            // A confirmed deletion no longer needs authentication. Release
            // only this account's slot in the same commit, independently of
            // bounded history collection. A later account's sign-in survives
            // an idempotent acknowledgement of this generation.
            tx.execute("DELETE FROM credentials WHERE slot=1 AND account_id=?1", [account.0])?;
            Ok(())
        });
        if let Err(error) = self.cancel_account_replays(account) {
            log::warn!("Deleting account replay cleanup failed: {error}");
        }
        result
    }

    pub fn abort_account_deletion(&self, account: AccountId, generation: &str) -> StoreResult<()> {
        validate_generation(generation)?;
        self.write(Durability::Durable, |tx| {
            if tx.execute(
                "UPDATE accounts SET deleting_ms=NULL,deletion_phase=NULL,deletion_generation=NULL
                WHERE id=?1 AND deletion_phase='pending_server' AND deletion_generation=?2",
                params![account.0, generation],
            )? == 0
            {
                return Err(StoreError::Conflict);
            }
            Ok(())
        })
    }

    pub fn account_deletion(&self, account: AccountId) -> StoreResult<Option<AccountDeletion>> {
        self.read(|tx| deletion_tx(tx, account))
    }

    /// The collector must use this list, never accounts.deleting_ms alone.
    pub fn confirmed_account_deletions(&self, limit: u16) -> StoreResult<Vec<AccountId>> {
        validate_limit(limit, MAX_PAGE)?;
        self.read(|tx| {
            let mut statement = tx.prepare_cached(
                "SELECT id FROM accounts WHERE deletion_phase='confirmed'
                ORDER BY deleting_ms,id LIMIT ?1",
            )?;
            Ok(statement
                .query_map([limit], |row| row.get(0).map(AccountId))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Remove only untouched provisional tasks. Such tasks contain no source
    /// entries or attachment uploads; deleting them does not orphan staged files.
    pub fn sweep_provisional_tasks(
        &self,
        account: AccountId,
        before_ms: i64,
        limit: u16,
    ) -> StoreResult<usize> {
        validate_limit(limit, MAX_PAGE)?;
        self.write(Durability::Normal, |tx| {
            writable_account(tx, account)?;
            let deleted = tx.execute("DELETE FROM tasks WHERE id IN (SELECT t.id FROM tasks t
                WHERE t.account_id=?1 AND t.provisional=1 AND t.title_user_set=0 AND t.last_seq=0 AND t.message_count=0
                AND t.updated_ms<?2 AND t.deleted_ms IS NULL
                AND NOT EXISTS (SELECT 1 FROM entries e WHERE e.task_id=t.id)
                AND NOT EXISTS (SELECT 1 FROM file_attachments a WHERE a.task_id=t.id)
                ORDER BY t.updated_ms,t.id LIMIT ?3)", params![account.0, before_ms, limit])?;
            tx.execute("DELETE FROM projects WHERE id IN (SELECT p.id FROM projects p WHERE p.account_id=?1
                AND p.position IS NULL AND p.display_name IS NULL AND p.trusted IS NULL
                AND p.removed_ms IS NULL AND p.mcp_defaults='{}'
                AND NOT EXISTS (SELECT 1 FROM tasks t WHERE t.project_id=p.id) LIMIT ?2)",
                params![account.0, limit])?;
            Ok(deleted)
        })
    }
}

pub(super) fn writable_account(tx: &Transaction<'_>, account: AccountId) -> StoreResult<()> {
    let state: Option<(Option<String>, Option<i64>)> = tx
        .query_row(
            "SELECT deletion_phase,deleting_ms FROM accounts WHERE id=?1",
            [account.0],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    match state {
        Some((None, None)) => Ok(()),
        Some(_) => Err(StoreError::Conflict),
        None => Err(StoreError::NotFound),
    }
}

fn task_change(account: AccountId, uuid: &str) -> StoreChange {
    StoreChange {
        account: Some(account),
        task: Some(uuid.to_owned()),
        summary_changed: true,
        listed_changed: true,
        ..StoreChange::default()
    }
}

pub(super) fn task_key_tx(
    tx: &Transaction<'_>,
    account: AccountId,
    uuid: &str,
) -> StoreResult<TaskKey> {
    tx.query_row(
        "SELECT id FROM tasks WHERE account_id=?1 AND uuid=?2 AND deleted_ms IS NULL",
        params![account.0, uuid],
        |row| row.get(0).map(TaskKey),
    )
    .optional()?
    .ok_or(StoreError::NotFound)
}

fn task_by_uuid(
    tx: &Transaction<'_>,
    account: AccountId,
    uuid: &str,
) -> StoreResult<Option<TaskRecord>> {
    let sql = format!(
        "SELECT {TASK_COLUMNS} FROM tasks WHERE account_id=?1 AND uuid=?2 AND deleted_ms IS NULL"
    );
    Ok(tx
        .query_row(&sql, params![account.0, uuid], read_task)
        .optional()?)
}

fn list_tasks_tx(
    tx: &Transaction<'_>,
    account: AccountId,
    query: &TaskQuery,
) -> StoreResult<TaskPage> {
    let index = match (query.section, query.project) {
        (TaskSection::Pinned, _) => "tasks_pinned",
        (TaskSection::Recent, None) => "tasks_recent",
        (TaskSection::Recent, Some(_)) => "tasks_project",
        (TaskSection::State(_), Some(_)) => "tasks_project_section",
        (TaskSection::State(_), None) => "tasks_section",
    };
    let mut sql = format!(
        "SELECT {TASK_COLUMNS} FROM tasks INDEXED BY {index}
        WHERE account_id=? AND provisional=0 AND deleted_ms IS NULL"
    );
    let mut values = vec![rusqlite::types::Value::Integer(account.0)];
    match query.section {
        TaskSection::State(state) => {
            sql.push_str(" AND state=? AND message_count>0");
            values.push(rusqlite::types::Value::Text(state.as_str().to_owned()));
            if query.pins == PinFilter::Exclude && state != TaskState::Archived {
                sql.push_str(" AND pinned_ms IS NULL");
            }
        }
        TaskSection::Pinned => {
            sql.push_str(" AND pinned_ms IS NOT NULL AND message_count>0 AND state<>'archived'")
        }
        TaskSection::Recent => {}
    }
    if let Some(project) = query.project {
        sql.push_str(" AND project_id=?");
        values.push(rusqlite::types::Value::Integer(project.0));
    }
    if let Some(cursor) = query.cursor {
        sql.push_str(" AND (updated_ms,id)<(?,?)");
        values.extend([
            rusqlite::types::Value::Integer(cursor.updated_ms),
            rusqlite::types::Value::Integer(cursor.key.0),
        ]);
    }
    sql.push_str(" ORDER BY updated_ms DESC,id DESC LIMIT ?");
    values.push(rusqlite::types::Value::Integer(i64::from(query.limit) + 1));
    let mut statement = tx.prepare_cached(&sql)?;
    let mut tasks = statement
        .query_map(params_from_iter(values), read_task)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let more = tasks.len() > usize::from(query.limit);
    tasks.truncate(usize::from(query.limit));
    let next = more
        .then(|| {
            tasks.last().map(|task| TaskPosition {
                updated_ms: task.updated_ms,
                key: task.key,
            })
        })
        .flatten();
    Ok(TaskPage { tasks, next })
}

fn ensure_project(
    tx: &Transaction<'_>,
    account: AccountId,
    root: &str,
    now_ms: i64,
) -> StoreResult<ProjectKey> {
    tx.execute(
        "INSERT INTO projects (account_id,root,last_used_ms) VALUES (?1,?2,?3)
        ON CONFLICT(account_id,root) DO NOTHING",
        params![account.0, root, now_ms],
    )?;
    Ok(tx.query_row(
        "SELECT id FROM projects WHERE account_id=?1 AND root=?2",
        params![account.0, root],
        |row| row.get(0).map(ProjectKey),
    )?)
}

fn read_task(row: &Row<'_>) -> rusqlite::Result<TaskRecord> {
    let kind = match row.get::<_, String>(3)?.as_str() {
        "desktop" => TaskKind::Desktop,
        "acp" => TaskKind::Acp,
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    let state = match row.get::<_, String>(4)?.as_str() {
        "active" => TaskState::Active,
        "settled" => TaskState::Settled,
        "archived" => TaskState::Archived,
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    let updated_ms = row.get(12)?;
    Ok(TaskRecord {
        key: TaskKey(row.get(0)?),
        uuid: row.get(1)?,
        project: ProjectKey(row.get(2)?),
        kind,
        state,
        provisional: row.get(5)?,
        title: row.get(6)?,
        title_user_set: row.get(7)?,
        pinned_ms: row.get(8)?,
        web_enabled: row.get(9)?,
        settings: read_json(row, 10)?,
        created_ms: row.get(11)?,
        updated_ms,
        facts: TaskFacts {
            model: row.get(13)?,
            thinking_level: row.get(14)?,
            message_count: read_nonnegative(row, 15)?,
            updated_ms: Some(updated_ms),
            context_usage: row
                .get::<_, Option<i64>>(16)?
                .map(|n| {
                    u64::try_from(n).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(16, n))
                })
                .transpose()?,
            context_estimate: read_nonnegative(row, 17)?,
            context_start_seq: row.get(18)?,
            context_base: row
                .get::<_, Option<String>>(19)?
                .map(|json| parse_json(19, &json))
                .transpose()?,
        },
        last_seq: row.get(20)?,
        rows_epoch: row.get(21)?,
        timeline_format: row.get(22)?,
        last_row_seq: row.get(23)?,
    })
}

fn read_project(row: &Row<'_>) -> rusqlite::Result<ProjectRecord> {
    Ok(ProjectRecord {
        key: ProjectKey(row.get(0)?),
        root: row.get(1)?,
        display_name: row.get(2)?,
        position: row.get(3)?,
        last_used_ms: row.get(4)?,
        removed_ms: row.get(5)?,
        trusted: row.get(6)?,
        mcp_defaults: read_json(row, 7)?,
        name_revision: read_nonnegative(row, 8)?,
    })
}

fn project_position(project: &ProjectRecord) -> ProjectPosition {
    ProjectPosition {
        unpositioned: project.position.is_none(),
        position: project.position.unwrap_or(0),
        root: project.root.clone(),
        key: project.key,
    }
}

fn read_setting(
    tx: &Transaction<'_>,
    sql: &str,
    parameters: impl rusqlite::Params,
) -> StoreResult<Option<Value>> {
    Ok(tx
        .query_row(sql, parameters, |row| read_json(row, 0))
        .optional()?)
}

fn read_json(row: &Row<'_>, index: usize) -> rusqlite::Result<Value> {
    parse_json(index, &row.get::<_, String>(index)?)
}

fn parse_json(index: usize, value: &str) -> rusqlite::Result<Value> {
    serde_json::from_str(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn read_nonnegative(row: &Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
}

fn deletion_tx(tx: &Transaction<'_>, account: AccountId) -> StoreResult<Option<AccountDeletion>> {
    let result: Option<(Option<String>, Option<String>, Option<i64>)> = tx
        .query_row(
            "SELECT deletion_phase,deletion_generation,deleting_ms FROM accounts WHERE id=?1",
            [account.0],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    match result {
        Some((None, None, None)) => Ok(None),
        Some((Some(phase), Some(generation), Some(started_ms))) if phase == "pending_server" => {
            Ok(Some(AccountDeletion::Pending {
                generation,
                started_ms,
            }))
        }
        Some((Some(phase), Some(generation), Some(started_ms))) if phase == "confirmed" => {
            Ok(Some(AccountDeletion::Confirmed {
                generation,
                started_ms,
            }))
        }
        Some(_) => Err(StoreError::InvalidData),
        None => Err(StoreError::NotFound),
    }
}

fn validate_patch(patch: &TaskPatch) -> StoreResult<()> {
    if let Some(title) = &patch.title {
        validate_text(title)?;
    }
    if let Some(root) = &patch.project_root {
        validate_root(root)?;
    }
    if let Some(model) = &patch.model {
        validate_text(model)?;
    }
    Ok(())
}

fn same_task_metadata(left: &TaskRecord, right: &TaskRecord) -> bool {
    left.key == right.key
        && left.uuid == right.uuid
        && left.project == right.project
        && left.kind == right.kind
        && left.state == right.state
        && left.provisional == right.provisional
        && left.title == right.title
        && left.title_user_set == right.title_user_set
        && left.pinned_ms == right.pinned_ms
        && left.web_enabled == right.web_enabled
        && left.settings == right.settings
        && left.created_ms == right.created_ms
}

fn db_integer(value: u64) -> StoreResult<i64> {
    i64::try_from(value).map_err(|_| StoreError::InvalidInput)
}

fn validate_limit(limit: u16, max: u16) -> StoreResult<()> {
    if limit == 0 || limit > max {
        Err(StoreError::InvalidInput)
    } else {
        Ok(())
    }
}

fn validate_text(text: &str) -> StoreResult<()> {
    if text.len() > MAX_TEXT_BYTES || text.contains('\0') {
        Err(StoreError::InvalidInput)
    } else {
        Ok(())
    }
}

fn validate_root(root: &str) -> StoreResult<()> {
    validate_text(root)?;
    if !Path::new(root).is_absolute()
        || Path::new(root)
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        return Err(StoreError::InvalidInput);
    }
    Ok(())
}

fn validate_uuid(uuid: &str) -> StoreResult<()> {
    if uuid.len() != 36
        || !uuid.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
    {
        return Err(StoreError::InvalidInput);
    }
    Ok(())
}

fn validate_setting_key(key: &str) -> StoreResult<()> {
    if key.is_empty()
        || key.len() > 256
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(StoreError::InvalidInput);
    }
    Ok(())
}

fn validate_generation(generation: &str) -> StoreResult<()> {
    if generation.is_empty()
        || generation.len() > 128
        || !generation
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(StoreError::InvalidInput);
    }
    Ok(())
}

fn encode_settings(value: &Value) -> StoreResult<String> {
    let encoded = serde_json::to_string(value).map_err(|_| StoreError::InvalidInput)?;
    if encoded.len() > MAX_SETTINGS_BYTES {
        return Err(StoreError::InvalidInput);
    }
    Ok(encoded)
}

#[cfg(test)]
#[path = "tasks_tests.rs"]
mod tests;
