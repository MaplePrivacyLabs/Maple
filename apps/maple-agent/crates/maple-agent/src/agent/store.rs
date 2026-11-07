//! Runtime compatibility over the host-owned device-local store.
//!
//! Account authorization, source sessions, derived rows and deletion tombstones
//! belong to the typed store. This adapter never discovers previous indexes,
//! JSONL files or attachment directories.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine;
use pi_ai::Message;
use pi_coding_agent::SessionMessage;
use pi_coding_agent::session::{EntryKind, SessionManager};
use serde_json::{Value, json};

use super::types::{AgentPermissionMode, AgentSessionSummary, AgentTaskState, AgentTimelineItem};
use crate::store::{
    AccountId, NewTask, SourceReceipts, Store, StoreError, TaskPatch, TaskRecord,
    WeakSourceReceipts,
};

const PROVISIONAL: &str = "surfaceProvisional";
pub(crate) const PERMISSION_MODE: &str = "permissionMode";
const PAGE_SIZE: u16 = 200;

/// Which surface created a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TaskKind {
    Desktop,
    Acp,
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
        let mut row = Self {
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
        };
        // ACP callers run their tasks with no way to answer a prompt yet.
        if kind == TaskKind::Desktop {
            row.set_permission_mode(AgentPermissionMode::default());
        }
        row
    }

    /// What the task may do without asking.
    pub(crate) fn permission_mode(&self) -> AgentPermissionMode {
        AgentPermissionMode::from_saved(self.settings.get(PERMISSION_MODE).map(|mode| {
            // Anything but text is as unknown as an unknown mode.
            mode.as_str().unwrap_or_default()
        }))
    }

    pub(crate) fn set_permission_mode(&mut self, mode: AgentPermissionMode) {
        if !self.settings.is_object() {
            self.settings = Value::Object(Default::default());
        }
        self.settings[PERMISSION_MODE] = json!(mode.as_str());
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
            permission_mode: self.permission_mode(),
            state: self.state,
            acp: self.kind == TaskKind::Acp,
        }
    }
}

/// Compatibility facts computed by older runtime call sites. Source appends
/// already commit the authoritative caches; refresh never writes these values.
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

/// An account-bound view of the host's one store.
pub(crate) struct TaskStore {
    store: Arc<Store>,
    account: AccountId,
    user_id: String,
    store_generation: Option<String>,
    revoked: AtomicBool,
    sources: Mutex<HashMap<String, WeakSourceReceipts>>,
}

impl TaskStore {
    pub(crate) fn new(store: Arc<Store>, account: AccountId, user_id: &str) -> Self {
        let store_generation = store.persistence_status().store_id;
        Self {
            store,
            account,
            user_id: user_id.to_owned(),
            store_generation,
            revoked: AtomicBool::new(false),
            sources: Mutex::new(HashMap::new()),
        }
    }

    /// A numeric account id can be reused after collection, and the host keeps
    /// the same Store handle through erase/setup. Neither reuse may authorize
    /// an old adapter. Destructive operations still require the host's drain;
    /// this check does not replace that lifecycle synchronization.
    pub(crate) fn ensure_current(&self) -> Result<(), StoreError> {
        if !self.is_current()? {
            return Err(StoreError::Conflict);
        }
        Ok(())
    }

    pub(crate) fn is_current(&self) -> Result<bool, StoreError> {
        if self.revoked.load(Ordering::Acquire)
            || self.store_generation.is_none()
            || self.store.persistence_status().store_id != self.store_generation
        {
            return Ok(false);
        }
        Ok(self.store.account_id(&self.user_id)? == Some(self.account))
    }

    pub(crate) fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
        self.sources
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
    }

    pub(crate) fn store(&self) -> &Arc<Store> {
        &self.store
    }

    pub(crate) fn account(&self) -> AccountId {
        self.account
    }

    pub(crate) fn insert(&self, row: &TaskRow) -> Result<(), String> {
        self.ensure_current().map_err(|error| error.to_string())?;
        if row.message_count != 0 || row.updated_ms != row.created_ms {
            return Err(StoreError::InvalidInput.to_string());
        }
        let task = NewTask {
            uuid: row.id.clone(),
            project_root: row.project_root.clone(),
            kind: store_kind(row.kind),
            title: row.title.clone(),
            provisional: provisional(&row.settings),
            model: row.model.clone(),
            created_ms: row.created_ms,
        };
        let patch = TaskPatch {
            title_user_set: Some(row.title_user_set),
            state: Some(store_state(row.state)),
            web_enabled: Some(row.web_enabled),
            settings: Some(stored_settings(&row.settings)),
            ..TaskPatch::default()
        };
        self.store
            .create_task_with_patch(self.account, &task, &patch)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub(crate) fn get(&self, id: &str) -> Result<Option<TaskRow>, String> {
        self.ensure_current().map_err(|error| error.to_string())?;
        let Some(record) = self
            .store
            .task(self.account, id)
            .map_err(|error| error.to_string())?
        else {
            return Ok(None);
        };
        self.row(record).map(Some)
    }

    /// The legacy whole-list callers consume bounded typed pages until their
    /// own navigation contracts move to the store's paged APIs.
    pub(crate) fn list(&self, project_root: Option<&str>) -> Result<Vec<TaskRow>, String> {
        self.ensure_current().map_err(|error| error.to_string())?;
        let project = match project_root {
            Some(root) => match self
                .store
                .project(self.account, root)
                .map_err(|error| error.to_string())?
            {
                Some(project) => Some(project.key),
                None => return Ok(Vec::new()),
            },
            None => None,
        };
        let mut cursor = None;
        let mut rows = Vec::new();
        loop {
            let page = self
                .store
                .all_tasks_page(self.account, project, cursor, PAGE_SIZE)
                .map_err(|error| error.to_string())?;
            let keys: Vec<_> = page.tasks.iter().map(|task| task.project).collect();
            let projects: HashMap<_, _> = self
                .store
                .get_projects(self.account, &keys)
                .map_err(|error| error.to_string())?
                .into_iter()
                .map(|project| (project.key, project.root))
                .collect();
            for task in page.tasks {
                let root = projects
                    .get(&task.project)
                    .ok_or_else(|| StoreError::InvalidData.to_string())?;
                rows.push(task_row(task, root.clone()));
            }
            let Some(next) = page.next else { break };
            cursor = Some(next);
        }
        rows.sort_by(|left, right| {
            right
                .updated_ms
                .cmp(&left.updated_ms)
                .then_with(|| right.created_ms.cmp(&left.created_ms))
                .then_with(|| right.id.cmp(&left.id))
        });
        Ok(rows)
    }

    /// Cache fields are source-owned. Metadata changes use a compare-and-swap
    /// so a generated title cannot undo a concurrent user rename.
    pub(crate) fn update(
        &self,
        id: &str,
        change: impl FnOnce(&mut TaskRow),
    ) -> Result<Option<TaskRow>, String> {
        self.ensure_current().map_err(|error| error.to_string())?;
        let Some(record) = self
            .store
            .task(self.account, id)
            .map_err(|error| error.to_string())?
        else {
            return Ok(None);
        };
        let before = self.row(record.clone())?;
        let mut after = before.clone();
        change(&mut after);
        if before.id != after.id
            || before.kind != after.kind
            || before.created_ms != after.created_ms
            || before.updated_ms != after.updated_ms
            || before.message_count != after.message_count
            || (before.model != after.model && after.model.is_none())
        {
            return Err(StoreError::InvalidInput.to_string());
        }
        let patch = TaskPatch {
            title: (before.title != after.title).then_some(after.title),
            title_user_set: (before.title_user_set != after.title_user_set)
                .then_some(after.title_user_set),
            project_root: (before.project_root != after.project_root).then_some(after.project_root),
            state: (before.state != after.state).then_some(store_state(after.state)),
            provisional: (provisional(&before.settings) != provisional(&after.settings))
                .then_some(provisional(&after.settings)),
            web_enabled: (before.web_enabled != after.web_enabled).then_some(after.web_enabled),
            settings: (stored_settings(&before.settings) != stored_settings(&after.settings))
                .then(|| stored_settings(&after.settings)),
            model: if before.model != after.model {
                after.model
            } else {
                None
            },
        };
        self.store
            .update_task_if(self.account, id, &record, &patch)
            .map_err(|error| error.to_string())?
            .map(|record| self.row(record))
            .transpose()
    }

    /// Source appends update caches atomically. Facts computed from an older
    /// in-memory session must never replace those committed values.
    pub(crate) fn refresh_caches(
        &self,
        id: &str,
        _facts: &SessionFacts,
    ) -> Result<Option<TaskRow>, String> {
        self.get(id)
    }

    pub(crate) fn open_session(&self, id: &str, cwd: &str) -> Result<SessionManager, String> {
        self.ensure_current().map_err(|error| error.to_string())?;
        let loaded = self
            .store
            .load_session(self.account, id, crate::store::SessionLoad::Context)
            .map_err(|error| error.to_string())?;
        let mut sources = self
            .sources
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // Unloaded sessions own no queue; opening tasks cannot grow this registry forever.
        sources.retain(|_, source| source.upgrade().is_some());
        sources.insert(id.to_owned(), loaded.source_receipts().downgrade());
        drop(sources);
        Ok(loaded.into_manager(cwd))
    }

    pub(crate) fn source_receipts(&self, id: &str) -> Option<SourceReceipts> {
        self.ensure_current().ok()?;
        self.sources
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(id)
            .and_then(WeakSourceReceipts::upgrade)
    }

    pub(crate) fn context_tokens(&self, id: &str) -> Result<Option<u64>, String> {
        self.ensure_current().map_err(|error| error.to_string())?;
        self.store
            .task_facts(self.account, id)
            .map(|facts| (facts.context_tokens > 0).then_some(facts.context_tokens))
            .map_err(|error| error.to_string())
    }

    /// Existing whole-object IPC callers are adapted through bounded pages and
    /// range reads. An epoch/append change retries the snapshot instead of
    /// concatenating different versions of a long mutable tool row.
    pub(crate) fn timeline(&self, id: &str) -> Result<Vec<AgentTimelineItem>, String> {
        self.ensure_current().map_err(|error| error.to_string())?;
        for _ in 0..3 {
            match self.full_timeline(id) {
                Ok(rows) => return Ok(rows),
                Err(StoreError::Conflict | StoreError::EpochChanged) => continue,
                Err(error) => return Err(error.to_string()),
            }
        }
        Err(StoreError::Busy.to_string())
    }

    fn full_timeline(&self, id: &str) -> Result<Vec<AgentTimelineItem>, StoreError> {
        use crate::store::{HistoryPageRequest, PageTransport, RowRangeRequest, RowReader};
        let before = self
            .store
            .task(self.account, id)?
            .ok_or(StoreError::NotFound)?;
        let reader = RowReader::new(Arc::clone(&self.store));
        let mut cursor = None;
        let mut items = Vec::new();
        loop {
            let page = self.store.history_forward_page(
                self.account,
                &HistoryPageRequest {
                    task: id.to_owned(),
                    cursor,
                    limit_rows: Some(PAGE_SIZE),
                    limit_bytes: Some(4 * 1024 * 1024),
                },
                PageTransport::Local,
            )?;
            if page.rows_epoch != before.rows_epoch {
                return Err(StoreError::EpochChanged);
            }
            for row in page.rows {
                if row.item.truncated.is_none() {
                    items.push(row.item);
                    continue;
                }
                let mut encoded = Vec::new();
                let mut total = None;
                loop {
                    let range = reader.read_row(
                        self.account,
                        &RowRangeRequest {
                            task: id.to_owned(),
                            rows_epoch: row.rows_epoch,
                            row_seq: row.row_seq,
                            update_seq: Some(row.update_seq),
                            offset: encoded.len() as u64,
                            len: 1024 * 1024,
                        },
                        PageTransport::Local,
                    )?;
                    if total.is_some_and(|total| total != range.total) {
                        return Err(StoreError::Conflict);
                    }
                    total = Some(range.total);
                    let data = base64::engine::general_purpose::STANDARD
                        .decode(range.data)
                        .map_err(|_| StoreError::InvalidData)?;
                    if data.is_empty() && encoded.len() as u64 != range.total {
                        return Err(StoreError::InvalidData);
                    }
                    encoded.extend_from_slice(&data);
                    if encoded.len() as u64 == range.total {
                        break;
                    }
                    if encoded.len() as u64 > range.total {
                        return Err(StoreError::InvalidData);
                    }
                }
                items.push(serde_json::from_slice(&encoded).map_err(|_| StoreError::InvalidData)?);
            }
            let Some(next) = page.next else { break };
            cursor = Some(next);
        }
        let after = self
            .store
            .task(self.account, id)?
            .ok_or(StoreError::NotFound)?;
        if before.rows_epoch != after.rows_epoch {
            return Err(StoreError::EpochChanged);
        }
        if before.last_seq != after.last_seq {
            return Err(StoreError::Conflict);
        }
        Ok(items)
    }

    /// Once the tombstone commits, interrupted physical collection is resumable
    /// by the host's idle collector. No loose state directory is consulted.
    pub(crate) fn delete(&self, id: &str) -> Result<bool, String> {
        self.ensure_current().map_err(|error| error.to_string())?;
        let marked = self
            .store
            .mark_task_deleted(self.account, id, pi_ai::now_ms())
            .map_err(|error| error.to_string())?;
        if !marked {
            return Ok(false);
        }
        self.collect_hidden_task(id)?;
        Ok(true)
    }

    pub(crate) fn discard_provisional(&self, id: &str) -> Result<bool, String> {
        self.ensure_current().map_err(|error| error.to_string())?;
        let marked = self
            .store
            .mark_untouched_provisional_deleted(self.account, id, pi_ai::now_ms())
            .map_err(|error| error.to_string())?;
        if !marked {
            return Ok(false);
        }
        self.collect_hidden_task(id)?;
        Ok(true)
    }

    fn collect_hidden_task(&self, id: &str) -> Result<(), String> {
        if let Err(error) = crate::store::spill::SpillRoot::new(self.store.paths())
            .and_then(|spills| spills.remove_task(id))
        {
            log::warn!("Deleted Agent task spill cleanup failed: {error}");
            // Preserve the queued UUID until idle maintenance can retry the
            // exact new spill directory. The durable hide still succeeded.
            return Ok(());
        }
        // A visible delete acknowledges the durable tombstone. Perform at
        // most one bounded cleanup transaction; the idle collector resumes
        // any remaining history without delaying this acknowledgement.
        match self.store.collect_task_step(self.account, id) {
            Ok(_) | Err(StoreError::Busy | StoreError::NotFound) => {}
            Err(error) => return Err(error.to_string()),
        }
        Ok(())
    }

    fn row(&self, record: TaskRecord) -> Result<TaskRow, String> {
        let project = self
            .store
            .project_by_key(self.account, record.project)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| StoreError::InvalidData.to_string())?;
        Ok(task_row(record, project.root))
    }
}

fn task_row(record: TaskRecord, project_root: String) -> TaskRow {
    let mut settings = record.settings;
    if record.provisional {
        if !settings.is_object() {
            settings = Value::Object(Default::default());
        }
        settings[PROVISIONAL] = Value::Bool(true);
    }
    TaskRow {
        id: record.uuid,
        title: record.title,
        title_user_set: record.title_user_set,
        project_root,
        kind: match record.kind {
            crate::store::TaskKind::Desktop => TaskKind::Desktop,
            crate::store::TaskKind::Acp => TaskKind::Acp,
        },
        state: match record.state {
            crate::store::TaskState::Active => AgentTaskState::Active,
            crate::store::TaskState::Settled => AgentTaskState::Settled,
            crate::store::TaskState::Archived => AgentTaskState::Archived,
        },
        web_enabled: record.web_enabled,
        model: record.facts.model,
        message_count: usize::try_from(record.facts.message_count).unwrap_or(usize::MAX),
        created_ms: record.created_ms,
        updated_ms: record.updated_ms,
        settings,
    }
}

fn store_kind(kind: TaskKind) -> crate::store::TaskKind {
    match kind {
        TaskKind::Desktop => crate::store::TaskKind::Desktop,
        TaskKind::Acp => crate::store::TaskKind::Acp,
    }
}

fn store_state(state: AgentTaskState) -> crate::store::TaskState {
    match state {
        AgentTaskState::Active => crate::store::TaskState::Active,
        AgentTaskState::Settled => crate::store::TaskState::Settled,
        AgentTaskState::Archived => crate::store::TaskState::Archived,
    }
}

fn provisional(settings: &Value) -> bool {
    settings
        .get(PROVISIONAL)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn stored_settings(settings: &Value) -> Value {
    let mut settings = settings.clone();
    if let Some(settings) = settings.as_object_mut() {
        settings.remove(PROVISIONAL);
    }
    settings
}

#[cfg(test)]
mod tests;
