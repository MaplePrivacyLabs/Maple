//! One account-authorized paging facade for local IPC and remote control.
//! Transport limits are selected by the receiving host, never by a request.

use std::collections::HashMap;
use std::sync::Arc;

use maple_agent::agent::{
    AgentDesktopQueueSnapshot, AgentMcpConnectionError, AgentPersistedRowIdentity,
    AgentQueuedMessage, AgentRuntimeStatus, AgentServiceEvent, AgentSessionSummary, AgentTaskState,
    AgentTimelineItem,
};
use maple_agent::store::{AccountId, PageTransport, Store};
pub use maple_agent::store::{
    AttachmentRangeReply, HistoryAnchorRequest, HistoryPage, HistoryPageRequest,
    NavigationGetTasksReply, NavigationGroupPage, NavigationGroupQuery, NavigationProjectPage,
    NavigationSearchQuery, NavigationTaskPage, NavigationTaskQuery, OpenTaskPageRequest,
    ProjectSummary, RowRange, RowRangeRequest, TaskSummary, TimelineRow,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use super::{AgentBackend, LocalBackend};
use crate::host::protocol::Request;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PinnedTasksRequest {
    pub project: Option<String>,
    pub cursor: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: u16,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GetTasksRequest {
    pub tasks: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectPageRequest {
    pub cursor: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: u16,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReadAttachmentRequest {
    pub task: String,
    pub attachment: String,
    pub offset: u64,
    pub len: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoreToolSummaryRequest {
    pub task: String,
    pub rows_epoch: i64,
    pub row_seq: i64,
    pub summary: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QueuedMessageRequest {
    pub task: String,
    pub queue_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NavigationBootstrap {
    pub runtime_status: AgentRuntimeStatus,
    pub project_root: Option<String>,
    pub recent_projects: Vec<ProjectSummary>,
    pub projects_next: Option<String>,
    /// Offered workspace metadata is a bounded anchor, independent of the first
    /// recent-project page. An explicit removal tombstone omits it.
    #[serde(default)]
    pub default_project: Option<ProjectSummary>,
    pub pending_questions: Vec<AgentServiceEvent>,
    #[serde(default)]
    pub persistence_status: maple_agent::store::PersistenceStatus,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PagedTaskDetail {
    pub session: AgentSessionSummary,
    pub timeline: Vec<AgentTimelineItem>,
    pub timeline_revision: u64,
    pub before: Option<String>,
    pub rows_epoch: i64,
    pub plan: Option<AgentTimelineItem>,
    pub summaries: HashMap<String, String>,
    pub queue: AgentDesktopQueueSnapshot,
    /// Preview-only queue text must be fetched by id before editing.
    pub queue_previews: Vec<String>,
    pub mcp_errors: Vec<AgentMcpConnectionError>,
    pub live_has_more: bool,
}

fn default_limit() -> u16 {
    50
}

fn reply_budget(transport: PageTransport) -> usize {
    transport
        .caps()
        .max_reply_bytes
        .saturating_sub(if transport == PageTransport::Remote {
            2048
        } else {
            0
        })
}

pub fn summary_to_session(summary: TaskSummary) -> AgentSessionSummary {
    AgentSessionSummary {
        id: summary.uuid,
        title: summary.title,
        project_root: summary.project_root,
        created_ms: summary.created_ms,
        updated_ms: summary.updated_ms,
        message_count: usize::try_from(summary.message_count).unwrap_or(usize::MAX),
        model: summary.model,
        web_enabled: summary.web_enabled,
        state: match summary.state.as_str() {
            "settled" => AgentTaskState::Settled,
            "archived" => AgentTaskState::Archived,
            _ => AgentTaskState::Active,
        },
        acp: summary.kind == "acp",
    }
}

/// Every persisted occurrence gets a UI key independent of the legacy merge
/// key. The runtime carries a live occurrence across its commit transition.
pub fn timeline_view_key(item: &AgentTimelineItem) -> Option<String> {
    if let Some(row) = &item.persisted {
        Some(format!(
            "stored:{}:{}:{}",
            row.task_id, row.rows_epoch, row.row_seq
        ))
    } else {
        item.live
            .as_ref()
            .map(|live| format!("live:{}:{}", live.run_id, live.occurrence))
    }
}

pub fn persisted_item(task: &str, row: TimelineRow) -> AgentTimelineItem {
    let mut item = row.item;
    item.persisted = Some(AgentPersistedRowIdentity {
        task_id: task.into(),
        rows_epoch: row.rows_epoch,
        row_seq: row.row_seq,
        update_seq: row.update_seq,
    });
    item
}

impl AgentBackend {
    /// Call from the UI's navigation callback, before moving work into a future.
    /// Allocating while polling an asynchronous open would let an older,
    /// delayed request acquire a newer subscription than the current selection.
    pub fn next_subscription_intent(&self) -> u64 {
        self.subscription_sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1
    }

    pub async fn navigation_bootstrap(&self, user: &str) -> Result<NavigationBootstrap, String> {
        self.call("navigation_bootstrap", (user,)).await
    }
    pub async fn list_tasks(
        &self,
        user: &str,
        request: NavigationTaskQuery,
    ) -> Result<NavigationTaskPage, String> {
        self.call("list_tasks", (user, request)).await
    }
    pub async fn pinned_tasks(
        &self,
        user: &str,
        request: PinnedTasksRequest,
    ) -> Result<NavigationTaskPage, String> {
        self.call("pinned_tasks", (user, request)).await
    }
    pub async fn get_tasks(
        &self,
        user: &str,
        request: GetTasksRequest,
    ) -> Result<NavigationGetTasksReply, String> {
        self.call("get_tasks", (user, request)).await
    }
    pub async fn list_projects(
        &self,
        user: &str,
        request: ProjectPageRequest,
    ) -> Result<NavigationProjectPage, String> {
        self.call("list_projects", (user, request)).await
    }
    pub async fn list_project_groups(
        &self,
        user: &str,
        request: NavigationGroupQuery,
    ) -> Result<NavigationGroupPage, String> {
        self.call("list_project_groups", (user, request)).await
    }
    pub async fn search_tasks(
        &self,
        user: &str,
        request: NavigationSearchQuery,
    ) -> Result<NavigationTaskPage, String> {
        self.call("search_tasks", (user, request)).await
    }
    pub async fn open_task(
        &self,
        user: &str,
        request: OpenTaskPageRequest,
        intent: u64,
    ) -> Result<PagedTaskDetail, String> {
        self.subscribe_task(user, &request.task, intent).await?;
        self.call("open_task", (user, request)).await
    }

    /// Select transcript deltas before sending to a newly created task. The
    /// caller reserves the intent synchronously with its navigation action.
    pub async fn subscribe_task(&self, user: &str, task: &str, intent: u64) -> Result<(), String> {
        #[cfg(test)]
        let subscribe = matches!(&self.client, super::BackendClient::Host(_));
        #[cfg(not(test))]
        let subscribe = true;
        if subscribe {
            self.call::<()>(
                "subscribe",
                (
                    user,
                    crate::host::subscription::SubscribeRequest {
                        intent,
                        tasks: vec![task.to_owned()],
                    },
                ),
            )
            .await?;
        }
        Ok(())
    }
    pub async fn history_before_anchor(
        &self,
        user: &str,
        request: HistoryAnchorRequest,
    ) -> Result<HistoryPage, String> {
        self.call("history_before", (user, request)).await
    }
    pub async fn history_forward_after(
        &self,
        user: &str,
        request: HistoryAnchorRequest,
    ) -> Result<HistoryPage, String> {
        self.call("history_forward", (user, request)).await
    }
    pub async fn read_row(&self, user: &str, request: RowRangeRequest) -> Result<RowRange, String> {
        self.call("read_row", (user, request)).await
    }
    pub async fn read_attachment(
        &self,
        user: &str,
        request: ReadAttachmentRequest,
    ) -> Result<AttachmentRangeReply, String> {
        self.call("read_attachment", (user, request)).await
    }
    pub async fn save_tool_summary(
        &self,
        user: &str,
        request: StoreToolSummaryRequest,
    ) -> Result<(), String> {
        self.call("store_tool_summary", (user, request)).await
    }
    pub async fn queued_message(
        &self,
        user: &str,
        request: QueuedMessageRequest,
    ) -> Result<AgentQueuedMessage, String> {
        self.call("queued_message", (user, request)).await
    }
}

pub(super) fn is_paging(method: &str) -> bool {
    matches!(
        method,
        "navigation_bootstrap"
            | "list_tasks"
            | "pinned_tasks"
            | "get_tasks"
            | "list_projects"
            | "list_project_groups"
            | "search_tasks"
            | "open_task"
            | "history_before"
            | "history_forward"
            | "task_facts"
            | "read_row"
            | "read_attachment"
            | "store_tool_summary"
            | "queued_message"
    )
}

async fn context(backend: &LocalBackend, user: &str) -> Result<(Arc<Store>, AccountId), String> {
    backend.authorize_account(user)?;
    let store = backend
        .store()
        .cloned()
        .ok_or("Local store is unavailable")?;
    let account_store = store.clone();
    let user = user.to_owned();
    let account = blocking(move || match account_store.account_id(&user)? {
        Some(account) => Ok(account),
        None => account_store.upsert_account(&user, chrono::Utc::now().timestamp_millis()),
    })
    .await?;
    Ok((store, account))
}

async fn blocking<T: Send + 'static>(
    operation: impl FnOnce() -> maple_agent::store::StoreResult<T> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| "Local store worker did not complete".to_owned())?
        .map_err(|error| error.to_string())
}

fn ensure_workspace_project(
    store: &Store,
    account: AccountId,
) -> maple_agent::store::StoreResult<()> {
    if let Some(path) = maple_agent::agent::default_maple_workspace_path() {
        let root = path.to_string_lossy();
        let project = store.project(account, &root)?;
        if project
            .as_ref()
            .is_none_or(|project| project.removed_ms.is_none() && project.position.is_none())
        {
            let now = chrono::Utc::now().timestamp_millis();
            // Switcher pages include explicit positions or task-bearing roots.
            // The offered empty workspace is an explicit choice at the end.
            store.remember_project(account, &root, i64::MAX, now)?;
        }
    }
    Ok(())
}

fn parse<T: DeserializeOwned>(params: Value) -> Result<(String, T), String> {
    serde_json::from_value(params).map_err(|_| "Invalid host paging parameters".to_owned())
}

fn encode(value: impl Serialize) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|_| "Could not encode host page".to_owned())
}

pub(super) async fn dispatch(
    backend: &LocalBackend,
    request: Request,
    transport: PageTransport,
) -> Result<Value, String> {
    let method = request.method.as_str();
    if method == "navigation_bootstrap" {
        let (user,): (String,) =
            serde_json::from_value(request.params).map_err(|_| "Invalid host paging parameters")?;
        let (store, account) = context(backend, &user).await?;
        let workspace_store = store.clone();
        blocking(move || ensure_workspace_project(&workspace_store, account)).await?;
        let handle = backend.service().handle_for_user(&user).await?;
        let runtime_status = handle.status().await?;
        let project_root = super::local::gui_project_root(&handle.load_config().await?);
        let default_project = match maple_agent::agent::default_maple_workspace_path() {
            Some(path) => {
                let path = path.to_string_lossy().into_owned();
                let point_store = store.clone();
                let point_path = path.clone();
                let record = blocking(move || point_store.project(account, &point_path)).await?;
                if record
                    .as_ref()
                    .is_some_and(|record| record.removed_ms.is_some())
                {
                    None
                } else {
                    let metadata = handle.project_root_metadata(path.clone()).await?;
                    Some(ProjectSummary {
                        root: path,
                        display_name: Some(metadata.name),
                        name_revision: metadata.name_revision,
                        is_default_workspace: metadata.is_default_workspace,
                        position: record.as_ref().and_then(|record| record.position),
                        last_used_ms: i64::try_from(metadata.last_used_ms)
                            .map_err(|_| "Project metadata has an invalid timestamp")?,
                        truncated: false,
                    })
                }
            }
            None => None,
        };
        let pending_questions = handle.pending_questions().await;
        return encode(
            navigation_bootstrap_page(
                store,
                account,
                runtime_status,
                project_root,
                default_project,
                pending_questions,
                transport,
            )
            .await?,
        );
    }
    match method {
        "list_tasks" => {
            let (user, query) = parse::<NavigationTaskQuery>(request.params)?;
            let (store, account) = context(backend, &user).await?;
            encode(blocking(move || store.list_task_summaries(account, &query, transport)).await?)
        }
        "pinned_tasks" => {
            let (user, query) = parse::<PinnedTasksRequest>(request.params)?;
            let (store, account) = context(backend, &user).await?;
            encode(
                blocking(move || {
                    store.pinned_task_summaries(
                        account,
                        query.project.as_deref(),
                        query.cursor.as_deref(),
                        query.limit,
                        transport,
                    )
                })
                .await?,
            )
        }
        "get_tasks" => {
            let (user, query) = parse::<GetTasksRequest>(request.params)?;
            let (store, account) = context(backend, &user).await?;
            encode(
                blocking(move || store.get_task_summaries(account, &query.tasks, transport))
                    .await?,
            )
        }
        "list_projects" => {
            let (user, query) = parse::<ProjectPageRequest>(request.params)?;
            let (store, account) = context(backend, &user).await?;
            encode(
                blocking(move || {
                    ensure_workspace_project(&store, account)?;
                    store.navigation_projects(
                        account,
                        query.cursor.as_deref(),
                        query.limit,
                        transport,
                    )
                })
                .await?,
            )
        }
        "list_project_groups" => {
            let (user, query) = parse::<NavigationGroupQuery>(request.params)?;
            let (store, account) = context(backend, &user).await?;
            encode(
                blocking(move || store.list_project_group_summaries(account, &query, transport))
                    .await?,
            )
        }
        "search_tasks" => {
            let (user, query) = parse::<NavigationSearchQuery>(request.params)?;
            let (store, account) = context(backend, &user).await?;
            encode(blocking(move || store.search_task_summaries(account, &query, transport)).await?)
        }
        "open_task" => {
            let (user, query) = parse::<OpenTaskPageRequest>(request.params)?;
            encode(open_task(backend, &user, query, transport).await?)
        }
        "history_before" | "history_forward" => {
            let before = method == "history_before";
            if request
                .params
                .get(1)
                .is_some_and(|query| query.get("rowSeq").is_some())
            {
                let (user, query) = parse::<HistoryAnchorRequest>(request.params)?;
                let (store, account) = context(backend, &user).await?;
                return encode(
                    blocking(move || {
                        if before {
                            store.history_before_from(account, &query, transport)
                        } else {
                            store.history_forward_from(account, &query, transport)
                        }
                    })
                    .await?,
                );
            }
            let (user, query) = parse::<HistoryPageRequest>(request.params)?;
            let (store, account) = context(backend, &user).await?;
            encode(
                blocking(move || {
                    if before {
                        store.history_before_page(account, &query, transport)
                    } else {
                        store.history_forward_page(account, &query, transport)
                    }
                })
                .await?,
            )
        }
        "task_facts" => {
            let (user, task) = parse::<String>(request.params)?;
            let (store, account) = context(backend, &user).await?;
            encode(blocking(move || store.task_facts(account, &task)).await?)
        }
        "read_row" => {
            let (user, query) = parse::<RowRangeRequest>(request.params)?;
            let (_, account) = context(backend, &user).await?;
            let reader = backend.row_reader()?;
            encode(blocking(move || reader.read_row(account, &query, transport)).await?)
        }
        "read_attachment" => {
            let (user, query) = parse::<ReadAttachmentRequest>(request.params)?;
            let (_, account) = context(backend, &user).await?;
            let reader = backend.row_reader()?;
            encode(
                blocking(move || {
                    reader.read_attachment(
                        account,
                        &query.task,
                        &query.attachment,
                        query.offset,
                        query.len,
                        transport,
                    )
                })
                .await?,
            )
        }
        "store_tool_summary" => {
            let (user, query) = parse::<StoreToolSummaryRequest>(request.params)?;
            let (store, account) = context(backend, &user).await?;
            blocking(move || {
                store.store_tool_summary(
                    account,
                    &query.task,
                    query.rows_epoch,
                    query.row_seq,
                    &query.summary,
                )
            })
            .await?;
            Ok(Value::Null)
        }
        "queued_message" => {
            let (user, query) = parse::<QueuedMessageRequest>(request.params)?;
            context(backend, &user).await?;
            let handle = backend.service().handle_for_user(&user).await?;
            let page = handle.live_task_page(&query.task, 1, 4096).await?;
            let message = page
                .queue
                .items
                .into_iter()
                .find(|message| message.queue_id == query.queue_id)
                .ok_or("The queued message is no longer available")?;
            ensure_reply(&message, reply_budget(transport))?;
            encode(message)
        }
        _ => Err("Unknown Maple paging operation".into()),
    }
}

async fn navigation_bootstrap_page(
    store: Arc<Store>,
    account: AccountId,
    runtime_status: AgentRuntimeStatus,
    root: Option<String>,
    default_project: Option<ProjectSummary>,
    pending_questions: Vec<AgentServiceEvent>,
    transport: PageTransport,
) -> Result<NavigationBootstrap, String> {
    let mut response = NavigationBootstrap {
        runtime_status,
        project_root: root.clone(),
        recent_projects: Vec::new(),
        projects_next: None,
        default_project,
        pending_questions,
        persistence_status: store.persistence_status(),
    };
    let cap = reply_budget(transport);
    // Keep space for the continuation and enclosing field names. Project roots
    // are never shortened; a start cursor can defer even the first root.
    let mut budget = cap
        .checked_sub(serialized_bytes(&response)?.saturating_add(2048))
        .filter(|remaining| *remaining >= 2048)
        .ok_or("Navigation metadata exceeds its serialized page budget")?;
    for _ in 0..8 {
        let page_store = store.clone();
        let projects = blocking(move || {
            page_store.navigation_projects_with_budget(account, None, 50, transport, budget)
        })
        .await?;
        response.project_root = root.clone().or_else(|| {
            projects
                .projects
                .first()
                .map(|project| project.root.clone())
        });
        response.recent_projects = projects.projects;
        response.projects_next = projects.next_cursor;
        let size = serialized_bytes(&response)?;
        if size <= cap {
            return Ok(response);
        }
        let reduced = budget.saturating_sub(size - cap + 256).max(2048);
        if reduced >= budget {
            break;
        }
        budget = reduced;
    }
    Err("Navigation metadata exceeds its serialized page budget".into())
}

async fn open_task(
    backend: &LocalBackend,
    user: &str,
    request: OpenTaskPageRequest,
    transport: PageTransport,
) -> Result<PagedTaskDetail, String> {
    let (store, account) = context(backend, user).await?;
    let handle = backend.service().handle_for_user(user).await?;
    let requested = request
        .limit_bytes
        .unwrap_or(transport.caps().default_open_bytes);
    if !(16 * 1024..=transport.caps().max_reply_bytes).contains(&requested) {
        return Err("Invalid open-task page budget".into());
    }
    let cap = requested.min(reply_budget(transport));
    let row_limit = request
        .limit_rows
        .unwrap_or(if transport == PageTransport::Local {
            100
        } else {
            50
        });
    if row_limit == 0 || row_limit > 500 {
        return Err("Invalid open-task row limit".into());
    }
    handle.ensure_task_history_ready(&request.task).await?;
    // Start with the whole budget so immutable metadata (notably canonical
    // roots) need not fit an arbitrary fraction. If the merged wire response
    // exceeds the budget, reduce the next persisted page by the measured excess.
    let mut persisted_budget = cap;
    for _ in 0..8 {
        let before = handle.live_task_marker(&request.task).await?;
        let live = handle
            .live_task_page(
                &request.task,
                usize::from(row_limit.saturating_sub(1).max(1)),
                cap / 4,
            )
            .await?;
        let check_store = store.clone();
        let task = request.task.clone();
        let stamp = blocking(move || check_store.task_history_stamp(account, &task)).await?;
        let persisted_limit = row_limit.saturating_sub(live.items.len() as u16).max(1);
        let mut page_request = request.clone();
        page_request.limit_rows = Some(persisted_limit);
        page_request.limit_bytes = Some(persisted_budget);
        let page_store = store.clone();
        let page =
            blocking(move || page_store.open_task_page(account, &page_request, transport)).await?;
        let after = handle.live_task_marker(&request.task).await?;
        let check_store = store.clone();
        let task = request.task.clone();
        let final_stamp = blocking(move || check_store.task_history_stamp(account, &task)).await?;
        if before != after
            || before != live.marker
            || stamp != final_stamp
            || (stamp.rows_epoch, stamp.last_seq) != live.stored_marker
            || page.rows_epoch != final_stamp.rows_epoch
        {
            continue;
        }
        let oldest = page.rows.first().map(|row| row.row_seq);
        let mut summaries = HashMap::new();
        let mut timeline = Vec::with_capacity(page.rows.len() + live.items.len());
        for row in page.rows {
            if let Some(summary) = &row.summary {
                summaries.insert(
                    format!("stored:{}:{}:{}", request.task, row.rows_epoch, row.row_seq),
                    summary.clone(),
                );
            }
            timeline.push(persisted_item(&request.task, row));
        }
        let mut live_has_more = live.has_more;
        for item in live.items {
            if let Some(identity) = &item.persisted {
                if identity.rows_epoch != page.rows_epoch
                    || oldest.is_some_and(|oldest| identity.row_seq < oldest)
                {
                    continue;
                }
                if let Some(index) = timeline.iter().position(|row| {
                    row.persisted.as_ref().is_some_and(|row| {
                        row.rows_epoch == identity.rows_epoch && row.row_seq == identity.row_seq
                    })
                }) {
                    if timeline[index]
                        .persisted
                        .as_ref()
                        .is_some_and(|stored| stored.update_seq > identity.update_seq)
                        || (timeline[index]
                            .persisted
                            .as_ref()
                            .is_some_and(|stored| stored.update_seq == identity.update_seq)
                            && timeline[index].revision > item.revision)
                    {
                        continue;
                    }
                    timeline[index] = item;
                    continue;
                }
            }
            if timeline.len() >= usize::from(row_limit) {
                live_has_more = true;
                continue;
            }
            timeline.push(item);
        }
        let mut queue = live.queue;
        let queue_previews = fit_queue(&mut queue, cap / 8)?;
        let response = PagedTaskDetail {
            session: summary_to_session(page.summary),
            timeline,
            timeline_revision: live.timeline_revision,
            before: page.before,
            rows_epoch: page.rows_epoch,
            plan: page.plan.map(|row| persisted_item(&request.task, row)),
            summaries,
            queue,
            queue_previews,
            mcp_errors: Vec::new(),
            live_has_more,
        };
        let response_bytes = serialized_bytes(&response)?;
        if response_bytes > cap {
            let reduced = persisted_budget.saturating_sub(response_bytes - cap + 256);
            if reduced < 4096 || reduced >= persisted_budget {
                return Err("Host reply exceeds its serialized page budget".into());
            }
            persisted_budget = reduced;
            continue;
        }
        return Ok(response);
    }
    Err("Task history changed while it was opening; try opening it again".into())
}

fn fit_queue(queue: &mut AgentDesktopQueueSnapshot, cap: usize) -> Result<Vec<String>, String> {
    let mut previews = Vec::new();
    while serialized_bytes(queue)? > cap {
        let mut changed = false;
        for item in &mut queue.items {
            if !item.text.is_empty() {
                if !previews.contains(&item.queue_id) {
                    previews.push(item.queue_id.clone());
                }
                let mut end = item.text.len() / 2;
                while !item.text.is_char_boundary(end) {
                    end -= 1;
                }
                item.text.truncate(end);
                changed = true;
            }
        }
        if !changed {
            return Err("Queued-message metadata exceeds the page budget".into());
        }
    }
    Ok(previews)
}

fn serialized_bytes(value: &impl Serialize) -> Result<usize, String> {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .map_err(|_| "Could not encode bounded host reply".into())
}

fn ensure_reply(value: &impl Serialize, cap: usize) -> Result<(), String> {
    if serialized_bytes(value)? > cap {
        Err("Host reply exceeds its serialized page budget".into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_workspace_is_paged_and_an_explicit_removal_is_preserved() {
        let Some(workspace) = maple_agent::agent::default_maple_workspace_path() else {
            return;
        };
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(maple_agent::store::StorePaths::new(directory.path())).unwrap();
        let account = store.upsert_account("workspace@example.test", 1).unwrap();
        ensure_workspace_project(&store, account).unwrap();
        let page = store
            .navigation_projects(account, None, 50, PageTransport::Local)
            .unwrap();
        assert_eq!(page.projects.len(), 1);
        assert_eq!(
            page.projects[0].root.as_str(),
            workspace.to_string_lossy().as_ref()
        );
        store
            .remove_project(account, &page.projects[0].root, 2)
            .unwrap();
        ensure_workspace_project(&store, account).unwrap();
        assert!(
            store
                .navigation_projects(account, None, 50, PageTransport::Local)
                .unwrap()
                .projects
                .is_empty()
        );
    }

    #[test]
    fn remote_payload_budget_leaves_room_for_the_largest_response_id() {
        let payload = serde_json::json!("x".repeat(reply_budget(PageTransport::Remote) - 2));
        assert_eq!(
            serialized_bytes(&payload).unwrap(),
            reply_budget(PageTransport::Remote)
        );
        let response = crate::host::protocol::Message::Response {
            id: u64::MAX,
            result: Ok(payload),
        };
        assert!(
            serialized_bytes(&response).unwrap() <= PageTransport::Remote.caps().max_reply_bytes
        );
    }

    #[tokio::test]
    async fn bootstrap_budgets_projects_after_questions_and_preserves_continuation() {
        let directory = tempfile::tempdir().unwrap();
        let store =
            Arc::new(Store::open(maple_agent::store::StorePaths::new(directory.path())).unwrap());
        let account = store.upsert_account("bootstrap@example.test", 1).unwrap();
        let mut roots = Vec::new();
        for index in 0..50 {
            let prefix = if cfg!(windows) {
                "C:/bootstrap"
            } else {
                "/bootstrap"
            };
            let root = format!("{prefix}/{index:03}{}", "\u{1}".repeat(15_000));
            store.remember_project(account, &root, index, 1).unwrap();
            roots.push(root);
        }
        let questions = (0..32)
            .map(|index| AgentServiceEvent::Question {
                session_id: format!("10000000-0000-7000-8000-{index:012x}"),
                request_id: format!("question-{index}"),
                questions: vec![maple_agent::agent::AgentQuestion {
                    multi_select: false,
                    id: "one".into(),
                    header: "Question".into(),
                    question: "x".repeat(15 * 1024),
                    options: Vec::new(),
                }],
            })
            .collect::<Vec<_>>();
        let response = navigation_bootstrap_page(
            store.clone(),
            account,
            AgentRuntimeStatus {
                running: true,
                project_root: Some(roots[0].clone()),
                model: None,
                active_runs: HashMap::new(),
            },
            Some(roots[0].clone()),
            None,
            questions,
            PageTransport::Local,
        )
        .await
        .unwrap();
        assert_eq!(response.pending_questions.len(), 32);
        assert!(!response.recent_projects.is_empty());
        assert!(response.recent_projects.len() < roots.len());
        assert!(
            serialized_bytes(&response).unwrap() <= PageTransport::Local.caps().max_reply_bytes
        );
        let mut observed = response
            .recent_projects
            .iter()
            .map(|project| project.root.clone())
            .collect::<Vec<_>>();
        let mut cursor = response.projects_next;
        while let Some(next) = cursor {
            let page = store
                .navigation_projects(account, Some(&next), 50, PageTransport::Local)
                .unwrap();
            assert!(
                !page.projects.is_empty(),
                "a continuation must make progress"
            );
            assert!(
                serialized_bytes(&page).unwrap() <= PageTransport::Local.caps().max_reply_bytes
            );
            assert_eq!(page.projects[0].root, roots[observed.len()]);
            observed.extend(page.projects.into_iter().map(|project| project.root));
            assert!(observed.len() <= roots.len());
            cursor = page.next_cursor;
        }
        assert_eq!(
            observed, roots,
            "every project appears exactly once in order"
        );
    }
}
