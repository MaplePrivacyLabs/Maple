//! Bounded, summary-only navigation queries. No task settings, context base,
//! source entry, or timeline payload is loaded by these APIs.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rusqlite::{OptionalExtension, Row, Transaction, params, params_from_iter};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Cursor, ENVELOPE_RESERVE, PageTransport, cursor_scope, decode_cursor, encode_cursor};
use crate::store::{AccountId, PinFilter, Store, StoreError, StoreResult, TaskSection, TaskState};

const MAX_ROWS: u16 = 200;
const MAX_TEXT_BYTES: usize = 16 * 1024;
const MAX_MANUAL_TASKS: usize = 8192;
// SQLite bounds even anomalously long model/title values before Rust allocates.
const TASK_COLUMNS: &str = "id,uuid,project_id,substr(title,1,4096),state,kind,pinned_ms,web_enabled,\
    substr(model,1,1024),message_count,created_ms,updated_ms,\
    (length(title)>4096 OR coalesce(length(model)>1024,0))";
const PROJECT_COLUMNS: &str = "id,root,substr(display_name,1,4096),position,last_used_ms,\
    coalesce(length(display_name)>4096,0),name_revision";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskSummary {
    pub uuid: String,
    pub title: String,
    pub project_root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_display_name: Option<String>,
    pub state: String,
    pub kind: String,
    pub pinned_ms: Option<i64>,
    pub web_enabled: bool,
    pub model: Option<String>,
    pub message_count: u64,
    pub created_ms: i64,
    pub updated_ms: i64,
    /// Only display metadata is shortened; ids and canonical roots never are.
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProjectSummary {
    pub root: String,
    pub display_name: Option<String>,
    #[serde(default)]
    pub name_revision: u64,
    #[serde(default)]
    pub is_default_workspace: bool,
    pub position: Option<i64>,
    pub last_used_ms: i64,
    pub truncated: bool,
}

/// A bounded owner-UUID order. Tasks created after the snapshot lead the saved
/// order; unranked older history follows it by creation time, never activity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NavigationManualOrder {
    pub created_before_ms: i64,
    pub tasks: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NavigationTaskQuery {
    pub section: TaskSection,
    pub project: Option<String>,
    #[serde(default = "default_pin_filter")]
    pub pins: PinFilter,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual: Option<NavigationManualOrder>,
    pub cursor: Option<String>,
    #[serde(default = "default_task_limit")]
    pub limit: u16,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NavigationTaskPage {
    pub tasks: Vec<TaskSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NavigationSearchQuery {
    pub text: String,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub project_roots: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual: Option<NavigationManualOrder>,
    pub cursor: Option<String>,
    #[serde(default = "default_search_limit")]
    pub limit: u16,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NavigationGetTasksReply {
    pub tasks: Vec<TaskSummary>,
    /// Includes every input id not yet processed when the byte cap is reached.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remaining_ids: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NavigationProjectPage {
    pub projects: Vec<ProjectSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NavigationGroupQuery {
    #[serde(default = "default_group_limit")]
    pub limit_groups: u16,
    #[serde(default = "default_group_limit")]
    pub per_project: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual: Option<NavigationManualOrder>,
    pub cursor: Option<String>,
}

fn default_pin_filter() -> PinFilter {
    PinFilter::Exclude
}
fn default_task_limit() -> u16 {
    50
}
fn default_search_limit() -> u16 {
    100
}
fn default_group_limit() -> u16 {
    20
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectTaskCursors {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settled: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NavigationProjectGroup {
    pub project: ProjectSummary,
    pub tasks: Vec<TaskSummary>,
    pub more: ProjectTaskCursors,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NavigationGroupPage {
    pub groups: Vec<NavigationProjectGroup>,
    pub archived_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug)]
struct SummaryRow {
    key: i64,
    project: i64,
    summary: TaskSummary,
}

#[derive(Clone, Debug)]
struct ProjectRow {
    key: i64,
    summary: ProjectSummary,
}

struct PreparedManualOrder {
    cutoff: i64,
    ranks: HashMap<String, i64>,
    encoded_tasks: String,
    fingerprint: Value,
}

impl PreparedManualOrder {
    fn new(order: &NavigationManualOrder) -> StoreResult<Self> {
        if order.created_before_ms < 0 || order.tasks.len() > MAX_MANUAL_TASKS {
            return Err(StoreError::InvalidInput);
        }
        let mut ranks = HashMap::with_capacity(order.tasks.len());
        let mut tasks = Vec::with_capacity(order.tasks.len());
        for (index, task) in order.tasks.iter().enumerate() {
            validate_uuid(task)?;
            let task = task.to_ascii_lowercase();
            if ranks.insert(task.clone(), index as i64).is_some() {
                return Err(StoreError::InvalidInput);
            }
            tasks.push(task);
        }
        Ok(Self {
            cutoff: order.created_before_ms,
            ranks,
            encoded_tasks: serde_json::to_string(&tasks).map_err(|_| StoreError::InvalidInput)?,
            fingerprint: cursor_scope(order)?,
        })
    }

    fn position(&self, row: &SummaryRow) -> TaskPosition {
        let rank = self
            .ranks
            .get(&row.summary.uuid.to_ascii_lowercase())
            .copied();
        TaskPosition::Manual {
            band: if rank.is_some() {
                1
            } else if row.summary.created_ms > self.cutoff {
                0
            } else {
                2
            },
            rank: rank.unwrap_or(0),
            created: if rank.is_some() {
                0
            } else {
                row.summary.created_ms
            },
            uuid: row.summary.uuid.clone(),
        }
    }

    fn compare(&self, left: &SummaryRow, right: &SummaryRow) -> std::cmp::Ordering {
        let TaskPosition::Manual {
            band: left_band,
            rank: left_rank,
            created: left_created,
            uuid: left_uuid,
        } = self.position(left)
        else {
            unreachable!()
        };
        let TaskPosition::Manual {
            band: right_band,
            rank: right_rank,
            created: right_created,
            uuid: right_uuid,
        } = self.position(right)
        else {
            unreachable!()
        };
        left_band
            .cmp(&right_band)
            .then_with(|| left_rank.cmp(&right_rank))
            .then_with(|| right_created.cmp(&left_created))
            .then_with(|| left_uuid.cmp(&right_uuid))
    }
}

#[derive(Clone)]
enum TaskPosition {
    Updated {
        updated: i64,
        key: i64,
    },
    Manual {
        band: i64,
        rank: i64,
        created: i64,
        uuid: String,
    },
}

impl Store {
    pub fn list_task_summaries(
        &self,
        account: AccountId,
        query: &NavigationTaskQuery,
        transport: PageTransport,
    ) -> StoreResult<NavigationTaskPage> {
        validate_limit(query.limit, MAX_ROWS)?;
        if let Some(root) = &query.project {
            validate_root(root)?;
        }
        let manual = query
            .manual
            .as_ref()
            .map(PreparedManualOrder::new)
            .transpose()?;
        let scope = cursor_scope(&task_scope(
            query.section,
            query.project.as_deref(),
            query.pins,
            manual.as_ref(),
        ))?;
        let position = cursor_position(query.cursor.as_deref(), "tasks", account, &scope)?;
        let position = read_task_position(position.as_ref(), manual.is_some())?;
        self.read(|tx| {
            let project = resolve_project(tx, account, query.project.as_deref())?;
            if query.project.is_some() && project.is_none() {
                return Ok(NavigationTaskPage::default());
            }
            let mut rows = task_rows(
                tx,
                account,
                query.section,
                project,
                query.pins,
                manual.as_ref(),
                position,
                usize::from(query.limit) + 1,
            )?;
            populate_roots(tx, account, &mut rows)?;
            finish_task_page(
                account,
                &scope,
                "tasks",
                rows,
                query.limit,
                transport,
                manual.as_ref(),
            )
        })
    }

    pub fn pinned_task_summaries(
        &self,
        account: AccountId,
        project: Option<&str>,
        cursor: Option<&str>,
        limit: u16,
        transport: PageTransport,
    ) -> StoreResult<NavigationTaskPage> {
        self.list_task_summaries(
            account,
            &NavigationTaskQuery {
                section: TaskSection::Pinned,
                project: project.map(str::to_owned),
                pins: PinFilter::Ignore,
                manual: None,
                cursor: cursor.map(str::to_owned),
                limit,
            },
            transport,
        )
    }

    /// Process ids in input order. Missing ids are consumed, duplicate ids are
    /// consumed once per occurrence, and foreign-account ids never resolve.
    pub fn get_task_summaries(
        &self,
        account: AccountId,
        ids: &[String],
        transport: PageTransport,
    ) -> StoreResult<NavigationGetTasksReply> {
        if ids.len() > usize::from(MAX_ROWS) {
            return Err(StoreError::InvalidInput);
        }
        for id in ids {
            validate_uuid(id)?;
        }
        let encoded = serde_json::to_string(ids).map_err(|_| StoreError::InvalidInput)?;
        self.read(|tx| {
            let sql = format!(
                "SELECT {TASK_COLUMNS} FROM tasks WHERE account_id=?1
                AND uuid IN (SELECT value FROM json_each(?2)) AND deleted_ms IS NULL"
            );
            let mut statement = tx.prepare_cached(&sql)?;
            let mut rows = statement
                .query_map(params![account.0, encoded], read_summary)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            populate_roots(tx, account, &mut rows)?;
            let mut by_id: HashMap<_, _> = rows
                .into_iter()
                .map(|row| (row.summary.uuid.clone(), row.summary))
                .collect();
            let mut reply = NavigationGetTasksReply::default();
            for (index, id) in ids.iter().enumerate() {
                // Already emitted duplicate ids count as processed as well.
                let Some(summary) = by_id.remove(id) else {
                    continue;
                };
                reply.tasks.push(summary.clone());
                reply.remaining_ids = ids[index + 1..].to_vec();
                if serialized_len(&reply)? > reply_limit(transport) {
                    reply.tasks.pop();
                    reply.remaining_ids = ids[index..].to_vec();
                    if reply.tasks.is_empty() {
                        return Err(StoreError::InvalidData);
                    }
                    return Ok(reply);
                }
            }
            reply.remaining_ids.clear();
            Ok(reply)
        })
    }

    pub fn search_task_summaries(
        &self,
        account: AccountId,
        query: &NavigationSearchQuery,
        transport: PageTransport,
    ) -> StoreResult<NavigationTaskPage> {
        validate_limit(query.limit, 100)?;
        validate_text(&query.text)?;
        if let Some(root) = &query.project {
            validate_root(root)?;
        }
        for root in &query.project_roots {
            validate_root(root)?;
        }
        let mut roots = query.project_roots.clone();
        roots.sort();
        roots.dedup();
        if serde_json::to_vec(&roots)
            .map_err(|_| StoreError::InvalidInput)?
            .len()
            > 256 * 1024
        {
            return Err(StoreError::InvalidInput);
        }
        let fold = query.text.to_lowercase();
        let manual = query
            .manual
            .as_ref()
            .map(PreparedManualOrder::new)
            .transpose()?;
        let mut scope = json!({"text":fold,"project_roots":roots,"project":query.project});
        if let Some(manual) = &manual {
            scope["manual"] = manual.fingerprint.clone();
        }
        let scope = cursor_scope(&scope)?;
        let position = cursor_position(query.cursor.as_deref(), "search", account, &scope)?;
        let position = read_task_position(position.as_ref(), manual.is_some())?;
        let roots = serde_json::to_string(&roots).map_err(|_| StoreError::InvalidInput)?;
        self.read(|tx| {
            let project = resolve_project(tx, account, query.project.as_deref())?;
            if query.project.is_some() && project.is_none() {
                return Ok(NavigationTaskPage::default());
            }
            let (mut sql, mut values) = task_select("tasks_recent", account, manual.as_ref());
            sql.push_str(
                " AND provisional=0 AND deleted_ms IS NULL AND message_count>0
                AND (instr(title_fold,?)>0 OR project_id IN
                    (SELECT id FROM projects WHERE account_id=? AND
                        (root IN (SELECT value FROM json_each(?)) OR
                            instr(maple_project_label_fold(root,display_name),?)>0)))",
            );
            values.extend([
                fold.clone().into(),
                account.0.into(),
                roots.clone().into(),
                fold.clone().into(),
            ]);
            if let Some(project) = project {
                sql.push_str(" AND project_id=?");
                values.push(project.into());
            }
            append_task_order(
                &mut sql,
                &mut values,
                position,
                manual.is_some(),
                usize::from(query.limit) + 1,
            )?;
            let mut statement = tx.prepare_cached(&sql)?;
            let mut rows = statement
                .query_map(params_from_iter(values), read_summary)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            populate_roots(tx, account, &mut rows)?;
            finish_task_page(
                account,
                &scope,
                "search",
                rows,
                query.limit,
                transport,
                manual.as_ref(),
            )
        })
    }

    /// Switcher roots are independently paged, rather than sliced from a task
    /// page or loaded with project MCP definitions and trust settings.
    pub fn navigation_projects(
        &self,
        account: AccountId,
        cursor: Option<&str>,
        limit: u16,
        transport: PageTransport,
    ) -> StoreResult<NavigationProjectPage> {
        self.navigation_projects_with_budget(
            account,
            cursor,
            limit,
            transport,
            reply_limit(transport),
        )
    }

    /// Fit a project first page inside an enclosing reply. An empty first page
    /// carries a start cursor when the enclosing reply has room for metadata
    /// but its remaining budget cannot hold the first canonical root.
    pub fn navigation_projects_with_budget(
        &self,
        account: AccountId,
        cursor: Option<&str>,
        limit: u16,
        transport: PageTransport,
        bytes: usize,
    ) -> StoreResult<NavigationProjectPage> {
        validate_limit(limit, MAX_ROWS)?;
        if bytes < ENVELOPE_RESERVE || bytes > transport.caps().max_reply_bytes {
            return Err(StoreError::InvalidInput);
        }
        let bytes = bytes.min(reply_limit(transport));
        let scope = cursor_scope(&json!({"view":"switcher"}))?;
        let cursor_position = cursor_position(cursor, "projects", account, &scope)?;
        let position = read_project_position(cursor_position.as_ref())?;
        self.read(|tx| {
            let rows = project_rows(
                tx,
                account,
                position.as_ref(),
                usize::from(limit) + 1,
                false,
            )?;
            let mut page = NavigationProjectPage::default();
            let count = rows.len();
            for (index, row) in rows.iter().take(usize::from(limit)).enumerate() {
                page.projects.push(row.summary.clone());
                page.next_cursor = (index + 1 < count)
                    .then(|| make_cursor("projects", account, &scope, project_position(row)))
                    .transpose()?;
                if serialized_len(&page)? > bytes {
                    page.projects.pop();
                    if page.projects.is_empty() {
                        if bytes == reply_limit(transport) {
                            return Err(StoreError::InvalidData);
                        }
                        page.next_cursor = Some(make_cursor(
                            "projects",
                            account,
                            &scope,
                            cursor_position.clone().unwrap_or(Value::Null),
                        )?);
                        if serialized_len(&page)? > bytes {
                            return Err(StoreError::InvalidInput);
                        }
                        return Ok(page);
                    }
                    page.next_cursor = Some(make_cursor(
                        "projects",
                        account,
                        &scope,
                        project_position(&rows[index - 1]),
                    )?);
                    return Ok(page);
                }
            }
            Ok(page)
        })
    }

    /// Projects are paged before their task queries. Even once the flattened
    /// 200-task cap is reached, emitted groups retain start cursors for both
    /// sections so no later project's history disappears.
    pub fn list_project_group_summaries(
        &self,
        account: AccountId,
        query: &NavigationGroupQuery,
        transport: PageTransport,
    ) -> StoreResult<NavigationGroupPage> {
        validate_limit(query.limit_groups, 50)?;
        validate_limit(query.per_project, 50)?;
        let manual = query
            .manual
            .as_ref()
            .map(PreparedManualOrder::new)
            .transpose()?;
        let mut scope = json!({"view":"groups","per_project":query.per_project});
        if let Some(manual) = &manual {
            scope["manual"] = manual.fingerprint.clone();
        }
        let scope = cursor_scope(&scope)?;
        let position = cursor_position(query.cursor.as_deref(), "groups", account, &scope)?;
        let position = read_project_position(position.as_ref())?;
        self.read(|tx| {
            let projects = project_rows(
                tx,
                account,
                position.as_ref(),
                usize::from(query.limit_groups) + 1,
                true,
            )?;
            let archived_count: i64 = tx.query_row(
                "SELECT count(*) FROM tasks WHERE account_id=?1
                AND state='archived' AND provisional=0 AND message_count>0 AND deleted_ms IS NULL",
                [account.0],
                |row| row.get(0),
            )?;
            let mut page = NavigationGroupPage {
                archived_count: u64::try_from(archived_count)
                    .map_err(|_| StoreError::InvalidData)?,
                ..NavigationGroupPage::default()
            };
            let mut flat_count = 0;
            for (project_index, project) in projects
                .iter()
                .take(usize::from(query.limit_groups))
                .enumerate()
            {
                let active_scope = cursor_scope(&task_scope(
                    TaskSection::State(TaskState::Active),
                    Some(&project.summary.root),
                    PinFilter::Ignore,
                    manual.as_ref(),
                ))?;
                let settled_scope = cursor_scope(&task_scope(
                    TaskSection::State(TaskState::Settled),
                    Some(&project.summary.root),
                    PinFilter::Ignore,
                    manual.as_ref(),
                ))?;
                let budget = usize::from(query.per_project).min(usize::from(MAX_ROWS) - flat_count);
                // Read at most the remaining task budget plus one lookahead.
                let mut active = task_rows(
                    tx,
                    account,
                    TaskSection::State(TaskState::Active),
                    Some(project.key),
                    PinFilter::Ignore,
                    manual.as_ref(),
                    None,
                    budget + 1,
                )?;
                let mut settled = task_rows(
                    tx,
                    account,
                    TaskSection::State(TaskState::Settled),
                    Some(project.key),
                    PinFilter::Ignore,
                    manual.as_ref(),
                    None,
                    budget + 1,
                )?;
                for row in active.iter_mut().chain(settled.iter_mut()) {
                    row.summary.project_root = project.summary.root.clone();
                    row.summary.project_display_name = project.summary.display_name.clone();
                    row.summary.truncated |= project.summary.truncated;
                }
                let active_len = active.len();
                let settled_len = settled.len();
                let mut group = NavigationProjectGroup {
                    project: project.summary.clone(),
                    tasks: Vec::new(),
                    more: ProjectTaskCursors {
                        active: (!active.is_empty())
                            .then(|| make_cursor("tasks", account, &active_scope, Value::Null))
                            .transpose()?,
                        settled: (!settled.is_empty())
                            .then(|| make_cursor("tasks", account, &settled_scope, Value::Null))
                            .transpose()?,
                    },
                };
                page.next_cursor = (project_index + 1 < projects.len())
                    .then(|| make_cursor("groups", account, &scope, project_position(project)))
                    .transpose()?;
                // Budget the empty group (including its section continuations)
                // before advancing the group cursor past it.
                page.groups.push(group.clone());
                if serialized_len(&page)? > reply_limit(transport) {
                    page.groups.pop();
                    if page.groups.is_empty() {
                        return Err(StoreError::InvalidData);
                    }
                    page.next_cursor = Some(make_cursor(
                        "groups",
                        account,
                        &scope,
                        project_position(&projects[project_index - 1]),
                    )?);
                    return Ok(page);
                }
                let mut active_taken = 0;
                let mut settled_taken = 0;
                let mut tasks: Vec<_> = active
                    .into_iter()
                    .map(|r| (true, r))
                    .chain(settled.into_iter().map(|r| (false, r)))
                    .collect();
                if let Some(manual) = &manual {
                    // Merge the independently paged sections by the same
                    // immutable order before applying the shared row budget.
                    tasks.sort_by(|(_, left), (_, right)| manual.compare(left, right));
                }
                for (is_active, row) in tasks {
                    if group.tasks.len() == budget {
                        break;
                    }
                    let old_more = group.more.clone();
                    group.tasks.push(row.summary.clone());
                    if is_active {
                        active_taken += 1;
                        group.more.active = (active_taken < active_len)
                            .then(|| {
                                make_cursor(
                                    "tasks",
                                    account,
                                    &active_scope,
                                    task_position(&row, manual.as_ref()),
                                )
                            })
                            .transpose()?;
                    } else {
                        settled_taken += 1;
                        group.more.settled = (settled_taken < settled_len)
                            .then(|| {
                                make_cursor(
                                    "tasks",
                                    account,
                                    &settled_scope,
                                    task_position(&row, manual.as_ref()),
                                )
                            })
                            .transpose()?;
                    }
                    *page.groups.last_mut().ok_or(StoreError::InvalidData)? = group.clone();
                    if serialized_len(&page)? > reply_limit(transport) {
                        group.tasks.pop();
                        group.more = old_more;
                        *page.groups.last_mut().ok_or(StoreError::InvalidData)? = group.clone();
                        break;
                    }
                }
                flat_count += group.tasks.len();
            }
            Ok(page)
        })
    }
}

fn task_rows(
    tx: &Transaction<'_>,
    account: AccountId,
    section: TaskSection,
    project: Option<i64>,
    pins: PinFilter,
    manual: Option<&PreparedManualOrder>,
    position: Option<TaskPosition>,
    limit: usize,
) -> StoreResult<Vec<SummaryRow>> {
    let index = match (section, project) {
        (TaskSection::Pinned, _) => "tasks_pinned",
        (TaskSection::Recent, None) => "tasks_recent",
        (TaskSection::Recent, Some(_)) => "tasks_project",
        (TaskSection::State(_), Some(_)) => "tasks_project_section",
        (TaskSection::State(_), None) => "tasks_section",
    };
    let (mut sql, mut values) = task_select(index, account, manual);
    sql.push_str(" AND provisional=0 AND deleted_ms IS NULL");
    match section {
        TaskSection::State(state) => {
            sql.push_str(" AND state=? AND message_count>0");
            values.push(state.as_str().to_owned().into());
            if pins == PinFilter::Exclude && state != TaskState::Archived {
                sql.push_str(" AND pinned_ms IS NULL");
            }
        }
        TaskSection::Pinned => {
            sql.push_str(" AND pinned_ms IS NOT NULL AND message_count>0 AND state<>'archived'")
        }
        TaskSection::Recent => {}
    }
    if let Some(project) = project {
        sql.push_str(" AND project_id=?");
        values.push(project.into());
    }
    append_task_order(&mut sql, &mut values, position, manual.is_some(), limit)?;
    let mut statement = tx.prepare_cached(&sql)?;
    Ok(statement
        .query_map(params_from_iter(values), read_summary)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn task_select(
    index: &str,
    account: AccountId,
    manual: Option<&PreparedManualOrder>,
) -> (String, Vec<rusqlite::types::Value>) {
    if let Some(manual) = manual {
        // Materialize at most 8192 UUID ranks once; SQLite can index the
        // materialized rank relation rather than scanning JSON per task.
        return (
            format!(
                "WITH manual_ranks AS MATERIALIZED (
                    SELECT value AS rank_uuid,CAST(key AS INTEGER) AS rank_index FROM json_each(?)
                ), manual_tasks AS (
                    SELECT {TASK_COLUMNS},
                        CASE WHEN rank_index IS NOT NULL THEN 1 WHEN created_ms>? THEN 0 ELSE 2 END AS manual_band,
                        coalesce(rank_index,0) AS manual_rank,
                        CASE WHEN rank_index IS NULL THEN created_ms ELSE 0 END AS manual_created
                    FROM tasks INDEXED BY {index}
                    LEFT JOIN manual_ranks ON manual_ranks.rank_uuid=lower(tasks.uuid)
                    WHERE account_id=?"
            ),
            vec![manual.encoded_tasks.clone().into(), manual.cutoff.into(), account.0.into()],
        );
    }
    (
        format!("SELECT {TASK_COLUMNS} FROM tasks INDEXED BY {index} WHERE account_id=?"),
        vec![account.0.into()],
    )
}

fn append_task_order(
    sql: &mut String,
    values: &mut Vec<rusqlite::types::Value>,
    position: Option<TaskPosition>,
    manual: bool,
    limit: usize,
) -> StoreResult<()> {
    if manual {
        sql.push_str(") SELECT * FROM manual_tasks WHERE 1=1");
        if let Some(position) = position {
            let TaskPosition::Manual {
                band,
                rank,
                created,
                uuid,
            } = position
            else {
                return Err(StoreError::InvalidInput);
            };
            sql.push_str(
                " AND ((manual_band,manual_rank)>(?,?) OR
                    ((manual_band,manual_rank)=(?,?) AND
                        (manual_created<? OR (manual_created=? AND uuid>?))))",
            );
            values.extend([
                band.into(),
                rank.into(),
                band.into(),
                rank.into(),
                created.into(),
                created.into(),
                uuid.into(),
            ]);
        }
        sql.push_str(" ORDER BY manual_band,manual_rank,manual_created DESC,uuid ASC LIMIT ?");
    } else {
        if let Some(position) = position {
            let TaskPosition::Updated { updated, key } = position else {
                return Err(StoreError::InvalidInput);
            };
            sql.push_str(" AND (updated_ms,id)<(?,?)");
            values.extend([updated.into(), key.into()]);
        }
        sql.push_str(" ORDER BY updated_ms DESC,id DESC LIMIT ?");
    }
    values.push(
        i64::try_from(limit)
            .map_err(|_| StoreError::InvalidInput)?
            .into(),
    );
    Ok(())
}

fn finish_task_page(
    account: AccountId,
    scope: &Value,
    kind: &str,
    rows: Vec<SummaryRow>,
    limit: u16,
    transport: PageTransport,
    manual: Option<&PreparedManualOrder>,
) -> StoreResult<NavigationTaskPage> {
    let mut page = NavigationTaskPage::default();
    let count = rows.len();
    for (index, row) in rows.iter().take(usize::from(limit)).enumerate() {
        page.tasks.push(row.summary.clone());
        page.next_cursor = (index + 1 < count)
            .then(|| make_cursor(kind, account, scope, task_position(row, manual)))
            .transpose()?;
        if serialized_len(&page)? > reply_limit(transport) {
            page.tasks.pop();
            if page.tasks.is_empty() {
                return Err(StoreError::InvalidData);
            }
            page.next_cursor = Some(make_cursor(
                kind,
                account,
                scope,
                task_position(&rows[index - 1], manual),
            )?);
            return Ok(page);
        }
    }
    Ok(page)
}

fn project_rows(
    tx: &Transaction<'_>,
    account: AccountId,
    position: Option<&ProjectPosition>,
    limit: usize,
    groups: bool,
) -> StoreResult<Vec<ProjectRow>> {
    let mut base = format!(
        "SELECT {PROJECT_COLUMNS} FROM projects p WHERE p.account_id=? AND p.removed_ms IS NULL AND "
    );
    if !groups {
        base.push_str("(p.position IS NOT NULL OR ");
    }
    base.push_str(
        "EXISTS (SELECT 1 FROM tasks t WHERE t.account_id=p.account_id AND t.project_id=p.id
        AND t.provisional=0 AND t.message_count>0 AND t.deleted_ms IS NULL",
    );
    if groups {
        base.push_str(" AND t.state IN ('active','settled')");
    }
    base.push(')');
    if !groups {
        base.push(')');
    }
    let Some(position) = position else {
        return project_range(
            tx,
            account,
            &base,
            "",
            &[],
            "p.position IS NULL,coalesce(p.position,0),p.root,p.id",
            limit,
        );
    };
    if position.unpositioned {
        return project_range(
            tx,
            account,
            &base,
            " AND (p.position IS NULL)=1 AND coalesce(p.position,0)=0 AND (p.root,p.id)>(?,?)",
            &[position.root.clone().into(), position.key.into()],
            "p.root,p.id",
            limit,
        );
    }
    // SQLite does not seek an expression-index row-value range as a unit.
    // Split the durable order tuple into at most three index ranges, omitting
    // fixed order prefixes so each query also avoids a temporary sort.
    let mut rows = project_range(
        tx,
        account,
        &base,
        " AND (p.position IS NULL)=0 AND coalesce(p.position,0)=? AND (p.root,p.id)>(?,?)",
        &[
            position.position.into(),
            position.root.clone().into(),
            position.key.into(),
        ],
        "p.root,p.id",
        limit,
    )?;
    if rows.len() < limit {
        rows.extend(project_range(
            tx,
            account,
            &base,
            " AND (p.position IS NULL)=0 AND coalesce(p.position,0)>?",
            &[position.position.into()],
            "coalesce(p.position,0),p.root,p.id",
            limit - rows.len(),
        )?);
    }
    if rows.len() < limit {
        rows.extend(project_range(
            tx,
            account,
            &base,
            " AND (p.position IS NULL)=1 AND coalesce(p.position,0)=0",
            &[],
            "p.root,p.id",
            limit - rows.len(),
        )?);
    }
    Ok(rows)
}

fn project_range(
    tx: &Transaction<'_>,
    account: AccountId,
    base: &str,
    condition: &str,
    parameters: &[rusqlite::types::Value],
    order: &str,
    limit: usize,
) -> StoreResult<Vec<ProjectRow>> {
    let sql = format!("{base}{condition} ORDER BY {order} LIMIT ?");
    let mut values = vec![rusqlite::types::Value::Integer(account.0)];
    values.extend_from_slice(parameters);
    values.push(
        i64::try_from(limit)
            .map_err(|_| StoreError::InvalidInput)?
            .into(),
    );
    let mut statement = tx.prepare_cached(&sql)?;
    let mut rows = statement
        .query_map(params_from_iter(values), |row| {
            let name_revision: i64 = row.get(6)?;
            let name_revision = u64::try_from(name_revision)
                .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(6, name_revision))?;
            Ok(ProjectRow {
                key: row.get(0)?,
                summary: ProjectSummary {
                    root: row.get(1)?,
                    display_name: row.get(2)?,
                    position: row.get(3)?,
                    last_used_ms: row.get(4)?,
                    truncated: row.get(5)?,
                    name_revision,
                    is_default_workspace: false,
                },
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let workspace = crate::agent::default_maple_workspace_path();
    for row in &mut rows {
        validate_stored_root(&row.summary.root)?;
        row.summary.is_default_workspace = workspace
            .as_deref()
            .is_some_and(|path| path == Path::new(&row.summary.root));
        row.summary.display_name = Some(crate::agent::owner_project_display_name(
            &row.summary.root,
            row.summary.display_name.as_deref(),
            row.summary.is_default_workspace,
        ));
    }
    Ok(rows)
}

fn populate_roots(
    tx: &Transaction<'_>,
    account: AccountId,
    rows: &mut [SummaryRow],
) -> StoreResult<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let ids: HashSet<_> = rows.iter().map(|row| row.project).collect();
    let ids = serde_json::to_string(&ids).map_err(|_| StoreError::InvalidData)?;
    let mut statement = tx.prepare_cached(
        "SELECT id,root,substr(display_name,1,4096),coalesce(length(display_name)>4096,0) FROM projects WHERE account_id=?1
        AND id IN (SELECT value FROM json_each(?2))",
    )?;
    let roots = statement
        .query_map(params![account.0, ids], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                (
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, bool>(3)?,
                ),
            ))
        })?
        .collect::<rusqlite::Result<HashMap<_, _>>>()?;
    let workspace = crate::agent::default_maple_workspace_path();
    for row in rows {
        let (root, name, truncated) = roots.get(&row.project).ok_or(StoreError::InvalidData)?;
        validate_stored_root(root)?;
        row.summary.project_root = root.clone();
        let is_default = workspace
            .as_deref()
            .is_some_and(|path| path == Path::new(root));
        row.summary.project_display_name = Some(crate::agent::owner_project_display_name(
            root,
            name.as_deref(),
            is_default,
        ));
        row.summary.truncated |= *truncated;
    }
    Ok(())
}

/// Used by open-task reads on the same reader transaction as their history.
pub(super) fn task_summary_tx(
    tx: &Transaction<'_>,
    account: AccountId,
    uuid: &str,
) -> StoreResult<Option<TaskSummary>> {
    validate_uuid(uuid)?;
    let sql = format!(
        "SELECT {TASK_COLUMNS} FROM tasks WHERE account_id=?1 AND uuid=?2 AND deleted_ms IS NULL"
    );
    let row = tx
        .query_row(&sql, params![account.0, uuid], read_summary)
        .optional()?;
    let Some(row) = row else { return Ok(None) };
    let mut rows = [row];
    populate_roots(tx, account, &mut rows)?;
    Ok(Some(rows[0].summary.clone()))
}

/// Fit display metadata into an open-task envelope. Identity fields are never
/// shortened; a byte limit smaller than those fields is an invalid request.
pub(super) fn fit_task_summary(summary: &mut TaskSummary, max_bytes: usize) -> StoreResult<()> {
    while serialized_len(summary)? > max_bytes {
        summary.truncated = true;
        if summary.title.is_empty()
            && summary.model.as_ref().is_none_or(String::is_empty)
            && summary
                .project_display_name
                .as_ref()
                .is_none_or(String::is_empty)
        {
            return Err(StoreError::InvalidInput);
        }
        let model_len = summary.model.as_ref().map_or(0, String::len);
        let name_len = summary.project_display_name.as_ref().map_or(0, String::len);
        if name_len > model_len && name_len > summary.title.len() {
            if let Some(name) = &mut summary.project_display_name {
                truncate_half(name);
            }
        } else if model_len > summary.title.len() {
            if let Some(model) = &mut summary.model {
                truncate_half(model);
            }
        } else {
            truncate_half(&mut summary.title);
        }
    }
    Ok(())
}

fn truncate_half(value: &mut String) {
    let mut end = value.len() / 2;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
}

fn read_summary(row: &Row<'_>) -> rusqlite::Result<SummaryRow> {
    let message_count: i64 = row.get(9)?;
    let message_count = u64::try_from(message_count)
        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(9, message_count))?;
    Ok(SummaryRow {
        key: row.get(0)?,
        project: row.get(2)?,
        summary: TaskSummary {
            uuid: row.get(1)?,
            title: row.get(3)?,
            project_root: String::new(),
            project_display_name: None,
            state: row.get(4)?,
            kind: row.get(5)?,
            pinned_ms: row.get(6)?,
            web_enabled: row.get(7)?,
            model: row.get(8)?,
            message_count,
            created_ms: row.get(10)?,
            updated_ms: row.get(11)?,
            truncated: row.get(12)?,
        },
    })
}

fn task_scope(
    section: TaskSection,
    project: Option<&str>,
    pins: PinFilter,
    manual: Option<&PreparedManualOrder>,
) -> Value {
    let section = match section {
        TaskSection::State(state) => state.as_str(),
        TaskSection::Pinned => "pinned",
        TaskSection::Recent => "recent",
    };
    let mut scope = json!({"section":section,"project":project,"pins":if pins==PinFilter::Exclude {"exclude"} else {"ignore"}});
    if let Some(manual) = manual {
        scope["manual"] = manual.fingerprint.clone();
    }
    scope
}

fn task_position(row: &SummaryRow, manual: Option<&PreparedManualOrder>) -> Value {
    if let Some(manual) = manual {
        let TaskPosition::Manual {
            band,
            rank,
            created,
            uuid,
        } = manual.position(row)
        else {
            unreachable!()
        };
        return json!({"band":band,"rank":rank,"created_ms":created,"uuid":uuid});
    }
    json!({"updated_ms":row.summary.updated_ms,"id":row.key})
}

fn read_task_position(value: Option<&Value>, manual: bool) -> StoreResult<Option<TaskPosition>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            if manual {
                let band = value
                    .get("band")
                    .and_then(Value::as_i64)
                    .filter(|band| (0..=2).contains(band))
                    .ok_or(StoreError::InvalidInput)?;
                let rank = value
                    .get("rank")
                    .and_then(Value::as_i64)
                    .filter(|rank| (0..MAX_MANUAL_TASKS as i64).contains(rank))
                    .ok_or(StoreError::InvalidInput)?;
                let created = value
                    .get("created_ms")
                    .and_then(Value::as_i64)
                    .ok_or(StoreError::InvalidInput)?;
                if (band != 1 && rank != 0) || (band == 1 && created != 0) {
                    return Err(StoreError::InvalidInput);
                }
                let uuid = value
                    .get("uuid")
                    .and_then(Value::as_str)
                    .ok_or(StoreError::InvalidInput)?;
                validate_uuid(uuid)?;
                Ok(Some(TaskPosition::Manual {
                    band,
                    rank,
                    created,
                    uuid: uuid.to_owned(),
                }))
            } else {
                let key = value
                    .get("id")
                    .and_then(Value::as_i64)
                    .filter(|n| *n > 0)
                    .ok_or(StoreError::InvalidInput)?;
                let updated = value
                    .get("updated_ms")
                    .and_then(Value::as_i64)
                    .ok_or(StoreError::InvalidInput)?;
                Ok(Some(TaskPosition::Updated { updated, key }))
            }
        }
    }
}

#[derive(Clone, Debug)]
struct ProjectPosition {
    unpositioned: bool,
    position: i64,
    root: String,
    key: i64,
}

fn project_position(row: &ProjectRow) -> Value {
    json!({"unpositioned":row.summary.position.is_none(),"position":row.summary.position.unwrap_or(0),
        // A raw root can expand sixfold through JSON escaping. Base64 keeps
        // the durable tuple bounded even for unusual valid path characters.
        "root_b64":URL_SAFE_NO_PAD.encode(row.summary.root.as_bytes()),"id":row.key})
}

fn read_project_position(value: Option<&Value>) -> StoreResult<Option<ProjectPosition>> {
    let Some(value) = value else { return Ok(None) };
    if value.is_null() {
        return Ok(None);
    }
    let unpositioned = value
        .get("unpositioned")
        .and_then(Value::as_bool)
        .ok_or(StoreError::InvalidInput)?;
    let position = value
        .get("position")
        .and_then(Value::as_i64)
        .ok_or(StoreError::InvalidInput)?;
    let root = value
        .get("root_b64")
        .and_then(Value::as_str)
        .ok_or(StoreError::InvalidInput)?;
    let root = URL_SAFE_NO_PAD
        .decode(root)
        .map_err(|_| StoreError::InvalidInput)?;
    let root = String::from_utf8(root).map_err(|_| StoreError::InvalidInput)?;
    validate_root(&root)?;
    let key = value
        .get("id")
        .and_then(Value::as_i64)
        .filter(|n| *n > 0)
        .ok_or(StoreError::InvalidInput)?;
    if unpositioned && position != 0 {
        return Err(StoreError::InvalidInput);
    }
    Ok(Some(ProjectPosition {
        unpositioned,
        position,
        root,
        key,
    }))
}

fn cursor_position(
    token: Option<&str>,
    kind: &str,
    account: AccountId,
    scope: &Value,
) -> StoreResult<Option<Value>> {
    let Some(token) = token else { return Ok(None) };
    let cursor = decode_cursor(token)?;
    if cursor.v != 1 || cursor.kind != kind || cursor.account != account.0 || cursor.scope != *scope
    {
        return Err(StoreError::InvalidInput);
    }
    Ok(Some(cursor.position))
}

fn make_cursor(
    kind: &str,
    account: AccountId,
    scope: &Value,
    position: Value,
) -> StoreResult<String> {
    encode_cursor(&Cursor {
        v: 1,
        kind: kind.to_owned(),
        account: account.0,
        scope: scope.clone(),
        position,
    })
}

fn resolve_project(
    tx: &Transaction<'_>,
    account: AccountId,
    root: Option<&str>,
) -> StoreResult<Option<i64>> {
    root.map(|root| {
        tx.query_row(
            "SELECT id FROM projects WHERE account_id=?1 AND root=?2",
            params![account.0, root],
            |row| row.get(0),
        )
        .optional()
        .map_err(StoreError::from)
    })
    .transpose()
    .map(Option::flatten)
}

fn serialized_len(value: &impl Serialize) -> StoreResult<usize> {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .map_err(|_| StoreError::InvalidData)
}

fn reply_limit(transport: PageTransport) -> usize {
    transport
        .caps()
        .default_open_bytes
        .saturating_sub(ENVELOPE_RESERVE)
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
            .any(|part| matches!(part, Component::CurDir | Component::ParentDir))
    {
        Err(StoreError::InvalidInput)
    } else {
        Ok(())
    }
}
fn validate_stored_root(root: &str) -> StoreResult<()> {
    validate_root(root).map_err(|_| StoreError::InvalidData)
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
        Err(StoreError::InvalidInput)
    } else {
        Ok(())
    }
}

#[cfg(test)]
#[path = "navigation_tests.rs"]
mod tests;
