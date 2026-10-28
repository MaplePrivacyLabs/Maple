//! Desktop-only manual ordering over the unchanged summary paging protocol.
//! Only complete small inventories are reused. Larger scopes are scanned with
//! a bounded candidate heap; neither saved ranks nor local cursors leave here.
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::sync::{Arc, Mutex, Weak};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use maple_agent::store::{
    NavigationSearchQuery, NavigationTaskPage, NavigationTaskQuery, TaskSummary,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::backend::{AgentBackend, PinnedTasksRequest};

pub(super) const CANCELLED: &str = "Sidebar manual ordering was cancelled";
const BAD_CURSOR: &str = "The sidebar order changed; refresh this list";
const CURSOR_PREFIX: &str = "sidebar-manual-v1:";
const MAX_CURSOR_BYTES: usize = 4096;
const MAX_WIRE_CURSOR_BYTES: usize = 16 * 1024;
const MAX_PAGE: usize = 200;
const MAX_INVENTORY_ROWS: usize = 2048;
const MAX_INVENTORY_BYTES: usize = 8 * 1024 * 1024;
const MAX_CACHE_BYTES: usize = 16 * 1024 * 1024;
const MAX_CACHE_SCOPES: usize = 8;

/// The paged catalog and loaded sidebar rows use the same manual order bands.
pub(super) fn manual_order_key(rank: Option<usize>, created: i64, cutoff: i64) -> (u8, i64) {
    if let Some(rank) = rank {
        (1, rank as i64)
    } else if created > cutoff {
        (0, created.saturating_neg())
    } else {
        (2, created.saturating_neg())
    }
}

#[derive(Clone, Serialize)]
pub(super) struct ManualOrder {
    pub(super) cutoff: i64,
    pub(super) tasks: Vec<String>,
}

impl ManualOrder {
    /// Used after independently read active/settled prefixes are combined.
    /// Callers run their summary preparation on the backend executor.
    pub(super) fn sort(&self, tasks: &mut [TaskSummary]) {
        let ranks = Ranks::new(self);
        tasks.sort_by_cached_key(|task| ranks.position(task));
    }

    fn validate(&self) -> Result<(), String> {
        if self.cutoff < 0 || self.tasks.len() > 8192 {
            return Err("Invalid sidebar manual order".into());
        }
        let mut seen = std::collections::HashSet::new();
        for task in &self.tasks {
            if task.is_empty()
                || task.len() > 256
                || task.chars().any(char::is_control)
                || !seen.insert(task)
            {
                return Err("Invalid sidebar manual order".into());
            }
        }
        Ok(())
    }
}

#[derive(Clone, Serialize)]
#[serde(tag = "kind", content = "query")]
pub(super) enum TaskQuery {
    Tasks(NavigationTaskQuery),
    Search(NavigationSearchQuery),
    Pinned(PinnedTasksRequest),
}

impl TaskQuery {
    fn cursor(&self) -> Option<&str> {
        match self {
            Self::Tasks(query) => query.cursor.as_deref(),
            Self::Search(query) => query.cursor.as_deref(),
            Self::Pinned(query) => query.cursor.as_deref(),
        }
    }

    fn limit(&self) -> usize {
        usize::from(match self {
            Self::Tasks(query) => query.limit,
            Self::Search(query) => query.limit,
            Self::Pinned(query) => query.limit,
        })
    }

    fn wire_query(&self, cursor: Option<String>) -> Self {
        let mut query = self.clone();
        match &mut query {
            Self::Tasks(query) => {
                query.cursor = cursor;
                query.limit = 200;
            }
            Self::Search(query) => {
                query.cursor = cursor;
                // Protocol seven bounds search pages to one hundred rows.
                query.limit = 100;
            }
            Self::Pinned(query) => {
                query.cursor = cursor;
                query.limit = 200;
            }
        }
        query
    }

    async fn fetch(
        &self,
        backend: &AgentBackend,
        user: &str,
        cursor: Option<String>,
    ) -> Result<NavigationTaskPage, String> {
        match self.wire_query(cursor) {
            Self::Tasks(query) => backend.list_tasks(user, query).await,
            Self::Search(query) => backend.search_tasks(user, query).await,
            Self::Pinned(query) => backend.pinned_tasks(user, query).await,
        }
    }
}

#[derive(Default, Clone)]
pub(super) struct Catalog {
    shared: Arc<Shared>,
}

struct Shared {
    state: Mutex<State>,
    scan_slots: Arc<Semaphore>,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            state: Mutex::new(State::default()),
            scan_slots: Arc::new(Semaphore::new(2)),
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        self.state
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .pending
            .cancel();
    }
}

#[derive(Default)]
struct State {
    generation: u64,
    pending: CancellationToken,
    cache: VecDeque<Inventory>,
    cache_bytes: usize,
}

struct Inventory {
    scope: [u8; 32],
    backend: Weak<AgentBackend>,
    tasks: Arc<Vec<TaskSummary>>,
    bytes: usize,
}

impl Catalog {
    pub(super) fn reader(&self) -> Reader {
        let state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        Reader {
            shared: Arc::downgrade(&self.shared),
            generation: state.generation,
            pending: state.pending.clone(),
        }
    }

    /// Live activity invalidates reuse without starving scans already reading.
    /// Their result can finish, but cannot refill this generation's cache.
    pub(super) fn invalidate(&self) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.generation = state.generation.wrapping_add(1);
        state.cache.clear();
        state.cache_bytes = 0;
    }

    /// Explicit scope/order refreshes also fence the caller's request generation.
    pub(super) fn cancel_pending(&self) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.pending.cancel();
        state.pending = CancellationToken::new();
    }
}

#[derive(Clone)]
pub(super) struct Reader {
    shared: Weak<Shared>,
    generation: u64,
    pending: CancellationToken,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct Position {
    band: u8,
    value: i64,
    id: String,
}

struct Ranks<'a> {
    cutoff: i64,
    tasks: HashMap<&'a str, usize>,
}

impl<'a> Ranks<'a> {
    fn new(order: &'a ManualOrder) -> Self {
        Self {
            cutoff: order.cutoff,
            tasks: order
                .tasks
                .iter()
                .enumerate()
                .map(|(rank, id)| (id.as_str(), rank))
                .collect(),
        }
    }

    fn position(&self, task: &TaskSummary) -> Position {
        let (band, value) = manual_order_key(
            self.tasks.get(task.uuid.as_str()).copied(),
            task.created_ms,
            self.cutoff,
        );
        Position {
            band,
            value,
            id: task.uuid.clone(),
        }
    }
}

struct Candidate {
    position: Position,
    task: TaskSummary,
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.position == other.position
    }
}
impl Eq for Candidate {}
impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.position.cmp(&other.position)
    }
}

/// The heap contains only the next page and one lookahead row. Its root is the
/// worst retained row, so no number of discarded history pages grows memory.
struct Candidates {
    rows: BinaryHeap<Candidate>,
    maximum: usize,
}

impl Candidates {
    fn new(limit: usize) -> Self {
        Self {
            rows: BinaryHeap::new(),
            maximum: limit + 1,
        }
    }

    fn offer(&mut self, candidate: Candidate) {
        if let Some(previous) = self
            .rows
            .iter()
            .find(|row| row.task.uuid == candidate.task.uuid)
        {
            if previous.task.updated_ms > candidate.task.updated_ms {
                return;
            }
            self.rows.retain(|row| row.task.uuid != candidate.task.uuid);
        }
        if self.rows.len() < self.maximum {
            self.rows.push(candidate);
        } else if self
            .rows
            .peek()
            .is_some_and(|worst| candidate.position < worst.position)
        {
            self.rows.pop();
            self.rows.push(candidate);
        }
    }

    fn finish(
        self,
        scope: [u8; 32],
        after: Option<Position>,
        limit: usize,
    ) -> Result<NavigationTaskPage, String> {
        let mut rows = self.rows.into_sorted_vec();
        let more = rows.len() > limit;
        rows.truncate(limit);
        let last = rows.last().map(|row| row.position.clone()).or(after);
        Ok(NavigationTaskPage {
            tasks: rows.into_iter().map(|row| row.task).collect(),
            next_cursor: more.then(|| encode_cursor(scope, last)).transpose()?,
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    scope: [u8; 32],
    after: Option<Position>,
}

fn encode_cursor(scope: [u8; 32], after: Option<Position>) -> Result<String, String> {
    let bytes = serde_json::to_vec(&Cursor { scope, after }).map_err(|_| BAD_CURSOR.to_string())?;
    Ok(format!("{CURSOR_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes)))
}

fn decode_cursor(cursor: Option<&str>, scope: [u8; 32]) -> Result<Option<Position>, String> {
    let Some(cursor) = cursor else {
        return Ok(None);
    };
    if cursor.len() > MAX_CURSOR_BYTES {
        return Err(BAD_CURSOR.into());
    }
    let encoded = cursor.strip_prefix(CURSOR_PREFIX).ok_or(BAD_CURSOR)?;
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| BAD_CURSOR.to_string())?;
    let cursor: Cursor = serde_json::from_slice(&bytes).map_err(|_| BAD_CURSOR.to_string())?;
    if cursor.scope != scope
        || cursor
            .after
            .as_ref()
            .is_some_and(|after| after.band > 2 || after.id.is_empty() || after.id.len() > 256)
    {
        return Err(BAD_CURSOR.into());
    }
    Ok(cursor.after)
}

fn scopes(
    backend: &Arc<AgentBackend>,
    user: &str,
    query: &TaskQuery,
    order: &ManualOrder,
) -> Result<([u8; 32], [u8; 32]), String> {
    let mut hash = Sha256::new();
    hash.update(b"maple.sidebar.catalog.v1");
    hash.update((Arc::as_ptr(backend) as usize).to_be_bytes());
    hash.update((user.len() as u64).to_be_bytes());
    hash.update(user.as_bytes());
    let host = backend.remote_host().unwrap_or("");
    hash.update((host.len() as u64).to_be_bytes());
    hash.update(host.as_bytes());
    hash.update(serde_json::to_vec(&query.wire_query(None)).map_err(|_| BAD_CURSOR.to_string())?);
    let inventory: [u8; 32] = hash.finalize().into();
    let mut hash = Sha256::new();
    hash.update(inventory);
    hash.update(serde_json::to_vec(order).map_err(|_| BAD_CURSOR.to_string())?);
    Ok((inventory, hash.finalize().into()))
}

fn charged_bytes(task: &TaskSummary) -> Result<usize, String> {
    // Charge owned strings, struct/index overhead and JSON escaping conservatively.
    Ok(serde_json::to_vec(task)
        .map_err(|_| "Could not prepare sidebar summaries".to_string())?
        .len()
        .saturating_add(std::mem::size_of::<TaskSummary>())
        .saturating_add(task.uuid.len())
        .saturating_add(64))
}

impl Reader {
    fn owner(&self) -> Result<Arc<Shared>, String> {
        if self.pending.is_cancelled() {
            return Err(CANCELLED.into());
        }
        self.shared.upgrade().ok_or_else(|| CANCELLED.into())
    }

    fn authorize(&self, backend: &AgentBackend, user: &str) -> Result<(), String> {
        self.owner()?;
        // The IPC client's current account is maintained by its host event
        // stream. This also fences cache-only replies without a network call.
        if backend.saved_user_id().as_deref() != Some(user) {
            return Err("The sidebar account is no longer active".into());
        }
        Ok(())
    }

    fn cached(
        &self,
        backend: &Arc<AgentBackend>,
        scope: [u8; 32],
    ) -> Result<Option<Arc<Vec<TaskSummary>>>, String> {
        let shared = self.owner()?;
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let found = state.cache.iter().position(|entry| {
            entry.scope == scope
                && entry
                    .backend
                    .upgrade()
                    .is_some_and(|owner| Arc::ptr_eq(&owner, backend))
        });
        let Some(index) = found else {
            return Ok(None);
        };
        let inventory = state.cache.remove(index).expect("located cache entry");
        let tasks = inventory.tasks.clone();
        state.cache.push_back(inventory);
        Ok(Some(tasks))
    }

    fn install(
        &self,
        backend: &Arc<AgentBackend>,
        scope: [u8; 32],
        tasks: Vec<TaskSummary>,
        bytes: usize,
    ) {
        let Ok(shared) = self.owner() else {
            return;
        };
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.generation != self.generation {
            return;
        }
        if let Some(index) = state.cache.iter().position(|entry| entry.scope == scope)
            && let Some(previous) = state.cache.remove(index)
        {
            state.cache_bytes = state.cache_bytes.saturating_sub(previous.bytes);
        }
        while state.cache.len() >= MAX_CACHE_SCOPES
            || state.cache_bytes.saturating_add(bytes) > MAX_CACHE_BYTES
        {
            let Some(previous) = state.cache.pop_front() else {
                break;
            };
            state.cache_bytes = state.cache_bytes.saturating_sub(previous.bytes);
        }
        state.cache_bytes = state.cache_bytes.saturating_add(bytes);
        state.cache.push_back(Inventory {
            scope,
            backend: Arc::downgrade(backend),
            tasks: Arc::new(tasks),
            bytes,
        });
    }

    /// Derive an exact continuation for a prefix of this same returned page.
    /// Group projection can merge two state prefixes without a second scan.
    pub(super) fn prefix_cursor(
        &self,
        backend: &Arc<AgentBackend>,
        user: &str,
        query: &TaskQuery,
        order: &ManualOrder,
        page: &NavigationTaskPage,
        keep: usize,
    ) -> Result<Option<String>, String> {
        self.authorize(backend, user)?;
        order.validate()?;
        if keep > page.tasks.len() {
            return Err(BAD_CURSOR.into());
        }
        if keep == page.tasks.len() && page.next_cursor.is_none() {
            return Ok(None);
        }
        let (_, scope) = scopes(backend, user, query, order)?;
        let after = if let Some(task) = keep.checked_sub(1).and_then(|index| page.tasks.get(index))
        {
            Some(Ranks::new(order).position(task))
        } else {
            decode_cursor(query.cursor(), scope)?
        };
        encode_cursor(scope, after).map(Some)
    }

    async fn scan_slot(&self) -> Result<tokio::sync::OwnedSemaphorePermit, String> {
        let slots = self.owner()?.scan_slots.clone();
        tokio::select! {
            biased;
            _ = self.pending.cancelled() => Err(CANCELLED.into()),
            permit = slots.acquire_owned() => permit.map_err(|_| CANCELLED.to_string()),
        }
    }

    pub(super) async fn page(
        self,
        backend: Arc<AgentBackend>,
        user: String,
        query: TaskQuery,
        order: ManualOrder,
    ) -> Result<NavigationTaskPage, String> {
        self.authorize(&backend, &user)?;
        order.validate()?;
        let limit = query.limit();
        if limit > MAX_PAGE {
            return Err("Sidebar page is too large".into());
        }
        let (inventory_scope, scope) = scopes(&backend, &user, &query, &order)?;
        let after = decode_cursor(query.cursor(), scope)?;
        let ranks = Ranks::new(&order);
        let mut candidates = Candidates::new(limit);
        if let Some(tasks) = self.cached(&backend, inventory_scope)? {
            for task in tasks.iter() {
                let position = ranks.position(task);
                if after.as_ref().is_none_or(|after| position > *after) {
                    candidates.offer(Candidate {
                        position,
                        task: task.clone(),
                    });
                }
            }
            self.authorize(&backend, &user)?;
            return candidates.finish(scope, after, limit);
        }
        let _permit = self.scan_slot().await?;
        // A preceding scan may have completed while this reader waited.
        if let Some(tasks) = self.cached(&backend, inventory_scope)? {
            for task in tasks.iter() {
                let position = ranks.position(task);
                if after.as_ref().is_none_or(|after| position > *after) {
                    candidates.offer(Candidate {
                        position,
                        task: task.clone(),
                    });
                }
            }
            self.authorize(&backend, &user)?;
            return candidates.finish(scope, after, limit);
        }
        let mut inventory = Some(HashMap::<String, (TaskSummary, usize)>::new());
        let mut bytes = 0_usize;
        let mut wire_cursor = None;
        // Brent's cycle guard retains one cursor, rather than unbounded history.
        let mut checkpoint = None;
        let mut distance = 0_usize;
        let mut power = 1_usize;
        loop {
            self.authorize(&backend, &user)?;
            let page = tokio::select! {
                biased;
                _ = self.pending.cancelled() => return Err(CANCELLED.into()),
                page = query.fetch(&backend, &user, wire_cursor.clone()) => page?,
            };
            self.authorize(&backend, &user)?;
            if page.tasks.is_empty() && page.next_cursor.is_some() {
                return Err("Sidebar summary paging did not advance".into());
            }
            for task in page.tasks {
                if let Some(rows) = &mut inventory {
                    let charge = charged_bytes(&task)?;
                    if let Some((previous, previous_bytes)) = rows.get(&task.uuid) {
                        if previous.updated_ms <= task.updated_ms {
                            bytes = bytes.saturating_sub(*previous_bytes).saturating_add(charge);
                            rows.insert(task.uuid.clone(), (task.clone(), charge));
                        }
                    } else {
                        bytes = bytes.saturating_add(charge);
                        rows.insert(task.uuid.clone(), (task.clone(), charge));
                    }
                    if rows.len() > MAX_INVENTORY_ROWS || bytes > MAX_INVENTORY_BYTES {
                        inventory = None;
                    }
                }
                let position = ranks.position(&task);
                if after.as_ref().is_none_or(|after| position > *after) {
                    candidates.offer(Candidate { position, task });
                }
            }
            let Some(next) = page.next_cursor else {
                break;
            };
            if next.len() > MAX_WIRE_CURSOR_BYTES
                || wire_cursor.as_ref() == Some(&next)
                || checkpoint.as_ref() == Some(&next)
            {
                return Err("Sidebar summary paging did not advance".into());
            }
            distance = distance.saturating_add(1);
            if distance == power {
                checkpoint = Some(next.clone());
                power = power.saturating_mul(2);
                distance = 0;
            }
            wire_cursor = Some(next);
            tokio::task::yield_now().await;
        }
        self.authorize(&backend, &user)?;
        if let Some(inventory) = inventory {
            self.install(
                &backend,
                inventory_scope,
                inventory.into_values().map(|(task, _)| task).collect(),
                bytes,
            );
        }
        candidates.finish(scope, after, limit)
    }
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;

    #[tokio::test]
    async fn cancelling_a_reload_releases_queued_scans_before_a_slot_opens() {
        let catalog = Catalog::default();
        let first = catalog.reader().scan_slot().await.unwrap();
        let second = catalog.reader().scan_slot().await.unwrap();
        let queued_reader = catalog.reader();
        let mut queued = Box::pin(queued_reader.scan_slot());
        let pending = std::future::poll_fn(|cx| {
            std::task::Poll::Ready(std::future::Future::poll(queued.as_mut(), cx).is_pending())
        })
        .await;
        assert!(pending, "both scan slots are occupied");
        catalog.cancel_pending();
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), queued)
            .await
            .expect("cancelled queued scan must not wait for an old slot");
        assert!(matches!(result, Err(message) if message == CANCELLED));
        drop((first, second));
        assert!(catalog.reader().scan_slot().await.is_ok());
    }

    #[test]
    fn replacing_a_reload_cancels_active_and_queued_readers() {
        let catalog = Catalog::default();
        let active = catalog.reader();
        let queued = catalog.reader();
        catalog.cancel_pending();
        catalog.invalidate();
        assert!(matches!(active.owner(), Err(message) if message == CANCELLED));
        assert!(matches!(queued.owner(), Err(message) if message == CANCELLED));
        assert!(catalog.reader().owner().is_ok());
    }
}
