//! Private ACP history captures. Only compact canonical rows are spooled; the
//! immutable authoritative entries remain in their original store. A capture
//! holds one read snapshot during local copying, never during client I/O.
//!
//! Register before starting a blocking worker, then fence runtime generation.
//! Store close seals registration and drains every slot before closing SQL/keys.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags, Transaction, config::DbConfig, params};
use zeroize::Zeroizing;

use crate::agent::{AgentPersistedRowIdentity, AgentTimelineItem};

use super::keys::LockedKey;
use super::paths::{private_directory, private_new_file, verify_private_path};
use super::{
    AccountId, Store, StoreError, StoreResult, TaskHistoryStamp, TaskKey, engine, projector, tasks,
};

// Bound simultaneous private caches/keys, not the length of a conversation.
const MAX_REPLAYS: usize = 4;
const MAX_FULL_ROW_BYTES: usize = 64 * 1024 * 1024;
const SPACE_CHECK_BYTES: u64 = 1024 * 1024;
const SCRATCH_PREFIX: &str = "capture-";
const SCRATCH_SUFFIX: &str = ".sqlite";

// Temporary native-flake diagnosis. This owns no snapshot, hook, key or path;
// it must not keep the worker's hook sender alive after a capture failure.
#[cfg(test)]
std::thread_local! {
    static CAPTURE_STAGE: std::cell::Cell<&'static str> = const { std::cell::Cell::new("worker-entry") };
}

#[cfg(test)]
fn capture_stage(stage: &'static str) {
    CAPTURE_STAGE.set(stage);
}

#[cfg(test)]
fn capture_last_stage() -> &'static str {
    CAPTURE_STAGE.get()
}

#[derive(Default)]
pub(super) struct ReplayRegistry {
    inner: Mutex<Registry>,
}

#[derive(Default)]
struct Registry {
    accepting: bool,
    slots: Vec<Slot>,
}

struct Slot {
    snapshot: Weak<ReplaySnapshot>,
    cancellation: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
}

impl ReplayRegistry {
    pub(super) fn resume(&self, paths: &super::StorePaths) -> StoreResult<()> {
        // Lifecycle owns reopening; no capture may register until cleanup ends.
        cleanup_stale(paths.root())?;
        self.inner.lock().map_err(|_| StoreError::Busy)?.accepting = true;
        Ok(())
    }

    fn register(&self, snapshot: &Arc<ReplaySnapshot>) -> StoreResult<()> {
        let mut registry = self.inner.lock().map_err(|_| StoreError::Busy)?;
        // No temporary Snapshot Arc may be destroyed under this mutex: its
        // destructor closes SQL/files. Slot tokens make pruning flag-only.
        registry.slots.retain(|slot| {
            !slot.closed.load(Ordering::Acquire) && slot.snapshot.strong_count() > 0
        });
        if !registry.accepting || registry.slots.len() >= MAX_REPLAYS {
            return Err(StoreError::Busy);
        }
        registry.slots.push(Slot {
            snapshot: Arc::downgrade(snapshot),
            cancellation: Arc::clone(&snapshot.cancellation),
            closed: Arc::clone(&snapshot.closed),
        });
        Ok(())
    }

    fn revoke_all(&self) {
        let registry = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for slot in &registry.slots {
            slot.cancellation.store(true, Ordering::Release);
        }
    }

    fn matching(
        &self,
        seal: bool,
        predicate: impl Fn(&ReplaySnapshot) -> bool,
    ) -> StoreResult<Vec<Arc<ReplaySnapshot>>> {
        let mut keep_alive = Vec::new();
        let mut registry = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if seal {
            registry.accepting = false;
        }
        let mut slots = Vec::new();
        registry.slots.retain(|slot| {
            let Some(snapshot) = slot.snapshot.upgrade() else {
                return false;
            };
            let keep = !snapshot.closed.load(Ordering::Acquire);
            if keep && predicate(&snapshot) {
                snapshot.cancellation.store(true, Ordering::Release);
                slots.push(Arc::clone(&snapshot));
            }
            keep_alive.push(snapshot);
            keep
        });
        drop(registry);
        drop(keep_alive);
        Ok(slots)
    }

    pub(super) fn seal(&self) -> StoreResult<()> {
        // Flag pending/running captures before waiting for any individual slot.
        self.matching(true, |_| true).map(|_| ())
    }
}

fn drain(slots: Vec<Arc<ReplaySnapshot>>) -> StoreResult<()> {
    // Never keep the registry mutex while waiting for a worker or filesystem.
    let mut first = None;
    for snapshot in slots {
        if let Err(error) = snapshot.close() {
            first.get_or_insert(error);
        }
    }
    first.map_or(Ok(()), Err)
}

/// A registered, not-yet-started blocking capture. Dropping it revokes the slot.
/// Register it before the caller's final runtime-generation check and spawn.
pub(crate) struct ReplayCapture {
    snapshot: Arc<ReplaySnapshot>,
    started: Instant,
    budget: Duration,
}

impl ReplayCapture {
    pub(crate) fn run(self) -> StoreResult<Arc<ReplaySnapshot>> {
        let snapshot = Arc::clone(&self.snapshot);
        #[cfg(test)]
        capture_stage("capture-admission");
        // A queued worker may start after close/reopen; do not open even a
        // source read transaction when its registered slot was revoked.
        if let Some(error) = stop_error(&snapshot.cancellation, self.started, self.budget) {
            snapshot.close()?;
            return Err(error);
        }
        #[cfg(test)]
        capture_stage("store-upgrade");
        let store = snapshot.store.upgrade().ok_or(StoreError::Busy)?;
        // Nested result keeps scratch I/O/cipher failures out of source damage
        // observation. Only operations on the real source return the outer Err.
        #[cfg(test)]
        capture_stage("source-read-bulk");
        let result = store.read_bulk(|tx| {
            #[cfg(test)]
            capture_stage("capture-state-lock");
            let mut state = snapshot.state.lock().map_err(|_| StoreError::Busy)?;
            if let Some(error) = stop_error(&snapshot.cancellation, self.started, self.budget) {
                return Ok(Err(error));
            }
            let result = capture(tx, &store, &snapshot, &mut state, self.started, self.budget);
            match result {
                Ok(scope) => {
                    if snapshot.scope.set(scope).is_err() {
                        state.take();
                        return Ok(Err(StoreError::Conflict));
                    }
                    Ok(Ok(()))
                }
                Err(error) => {
                    // Release the private connection/key before Store::read_bulk
                    // observes actual source damage and enters quarantine.
                    state.take();
                    match error {
                        CaptureError::Source(error) => {
                            // Our progress-hook interrupt is a replay deadline
                            // or cancellation, not an operational store fault.
                            if matches!(
                                error,
                                StoreError::Database {
                                    code: rusqlite::ffi::SQLITE_INTERRUPT,
                                    ..
                                }
                            ) && let Some(stopped) =
                                stop_error(&snapshot.cancellation, self.started, self.budget)
                            {
                                Ok(Err(stopped))
                            } else {
                                Err(error)
                            }
                        }
                        CaptureError::Scratch(error) => Ok(Err(error)),
                    }
                }
            }
        });
        match result {
            Ok(Ok(())) => {
                if let Some(error) = stop_error(&snapshot.cancellation, self.started, self.budget) {
                    snapshot.close()?;
                    return Err(error);
                }
                // Prevent this consumed registration's Drop from revoking the
                // successfully published snapshot. No worker is detached.
                self.snapshot.published.store(true, Ordering::Release);
                Ok(snapshot)
            }
            Ok(Err(error)) | Err(error) => {
                let _ = snapshot.close();
                Err(error)
            }
        }
    }
}

impl Drop for ReplayCapture {
    fn drop(&mut self) {
        if !self.snapshot.published.load(Ordering::Acquire) {
            self.snapshot.cancellation.store(true, Ordering::Release);
        }
    }
}

struct Scope {
    store_id: [u8; 16],
    task_key: TaskKey,
    user_key: String,
    user_id: String,
}

/// No raw SQL, paths, keys or captured content cross the store API.
pub(crate) struct ReplaySnapshot {
    store: Weak<Store>,
    account: AccountId,
    task: String,
    stamp: TaskHistoryStamp,
    scope: OnceLock<Scope>,
    cancellation: Arc<AtomicBool>,
    published: AtomicBool,
    closed: Arc<AtomicBool>,
    state: Mutex<Option<Spool>>,
    #[cfg(test)]
    capture_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl ReplaySnapshot {
    pub(crate) fn stamp(&self) -> TaskHistoryStamp {
        self.stamp
    }

    pub(crate) fn next_row(
        &self,
    ) -> StoreResult<Option<(AgentTimelineItem, AgentPersistedRowIdentity)>> {
        let result = self.next_row_inner();
        if result.is_err() || matches!(&result, Ok(None)) {
            let close = self.close();
            if result.is_ok() {
                close?;
            }
        }
        result
    }

    fn next_row_inner(
        &self,
    ) -> StoreResult<Option<(AgentTimelineItem, AgentPersistedRowIdentity)>> {
        if self.cancellation.load(Ordering::Acquire) {
            return Err(StoreError::Busy);
        }
        let scope = self.scope.get().ok_or(StoreError::Busy)?;
        // Never retain the spool mutex while entering Store::read. Source
        // damage observation may acquire lifecycle and drain all replay slots.
        let frozen = {
            let mut state = self.state.lock().map_err(|_| StoreError::Busy)?;
            if self.cancellation.load(Ordering::Acquire) {
                return Err(StoreError::Busy);
            }
            let spool = state.as_mut().ok_or(StoreError::Busy)?;
            spool.next_row()?
        };
        let Some(frozen) = frozen else {
            return Ok(None);
        };
        let store = self.store.upgrade().ok_or(StoreError::Busy)?;
        let item = store.read(|tx| {
            authorize(tx, self.account, &self.task, scope)?;
            if self.cancellation.load(Ordering::Acquire) {
                return Err(StoreError::Busy);
            }
            let size = projector::stored_row_size(frozen.codec, &frozen.payload)?;
            if size > MAX_FULL_ROW_BYTES {
                return Err(StoreError::InvalidInput);
            }
            let item =
                projector::decode_row(tx, scope.task_key, frozen.codec, &frozen.payload, true)?;
            if self.cancellation.load(Ordering::Acquire) {
                return Err(StoreError::Busy);
            }
            Ok(item)
        })?;
        if self.cancellation.load(Ordering::Acquire) {
            return Err(StoreError::Busy);
        }
        let identity = AgentPersistedRowIdentity {
            task_id: self.task.clone(),
            rows_epoch: self.stamp.rows_epoch,
            row_seq: frozen.row_seq,
            update_seq: frozen.update_seq,
        };
        Ok(Some((item, identity)))
    }

    /// Cancel, wait for an active local capture, close its private connection,
    /// wipe its key, then remove its file. Safe with retained caller Arcs.
    pub(crate) fn close(&self) -> StoreResult<()> {
        self.cancellation.store(true, Ordering::Release);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = match state.take() {
            Some(spool) => spool.close(),
            None => Ok(()),
        };
        self.closed.store(true, Ordering::Release);
        result
    }

    #[cfg(test)]
    fn scratch_path(&self) -> Option<PathBuf> {
        self.state
            .lock()
            .unwrap()
            .as_ref()
            .map(|spool| spool.path.clone())
    }
}

impl Drop for ReplaySnapshot {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

impl Store {
    pub(super) fn resume_replays(&self) {
        // A scratch cleanup problem disables only captures. Opening usable
        // authoritative app data must not depend on private scratch cleanup.
        if let Err(error) = self.replays.resume(&self.paths) {
            log::warn!("Private history replay admission unavailable: {error}");
        }
    }

    pub(crate) fn prepare_replay_capture(
        self: &Arc<Self>,
        account: AccountId,
        task: &str,
        expected: TaskHistoryStamp,
        cancellation: Arc<AtomicBool>,
        budget: Duration,
    ) -> StoreResult<ReplayCapture> {
        self.ensure_open()?;
        if uuid::Uuid::parse_str(task).is_err()
            || expected.rows_epoch < 0
            || expected.last_seq < 0
            || budget.is_zero()
            || cancellation.load(Ordering::Acquire)
        {
            return Err(StoreError::InvalidInput);
        }
        let snapshot = Arc::new(ReplaySnapshot {
            store: Arc::downgrade(self),
            account,
            task: task.to_owned(),
            stamp: expected,
            scope: OnceLock::new(),
            cancellation,
            published: AtomicBool::new(false),
            closed: Arc::new(AtomicBool::new(false)),
            state: Mutex::new(None),
            #[cfg(test)]
            capture_hook: Mutex::new(None),
        });
        self.replays.register(&snapshot)?;
        Ok(ReplayCapture {
            snapshot,
            started: Instant::now(),
            budget,
        })
    }

    #[cfg(test)]
    pub(crate) fn capture_replay(
        self: &Arc<Self>,
        account: AccountId,
        task: &str,
        expected: TaskHistoryStamp,
        cancellation: Arc<AtomicBool>,
        budget: Duration,
    ) -> StoreResult<Arc<ReplaySnapshot>> {
        self.prepare_replay_capture(account, task, expected, cancellation, budget)?
            .run()
    }

    /// Fast revocation for a synchronous runtime-generation transition. The
    /// owning async/blocking drain must subsequently close the slots' keys.
    pub(crate) fn revoke_all_replays(&self) {
        self.replays.revoke_all();
    }

    pub(crate) fn cancel_all_replays(&self) -> StoreResult<()> {
        drain(self.replays.matching(false, |_| true)?)
    }

    pub(crate) fn cancel_account_replays(&self, account: AccountId) -> StoreResult<()> {
        drain(
            self.replays
                .matching(false, |snapshot| snapshot.account == account)?,
        )
    }

    pub(crate) fn cancel_task_replays(&self, account: AccountId, task: &str) -> StoreResult<()> {
        drain(self.replays.matching(false, |snapshot| {
            snapshot.account == account && snapshot.task == task
        })?)
    }
}

enum CaptureError {
    Source(StoreError),
    Scratch(StoreError),
}

fn capture(
    tx: &Transaction<'_>,
    store: &Store,
    snapshot: &ReplaySnapshot,
    state: &mut Option<Spool>,
    started: Instant,
    budget: Duration,
) -> Result<Scope, CaptureError> {
    #[cfg(test)]
    capture_stage("source-scope");
    let scope = source_scope(tx, snapshot.account, &snapshot.task, snapshot.stamp)
        .map_err(CaptureError::Source)?;
    let cancellation = Arc::clone(&snapshot.cancellation);
    #[cfg(test)]
    capture_stage("source-progress-handler");
    tx.progress_handler(1000, Some(move || stopped(&cancellation, started, budget)))
        .map_err(|error| CaptureError::Source(error.into()))?;
    let _reset = ProgressReset(tx);
    *state = Some(Spool::create(store.paths.root()).map_err(CaptureError::Scratch)?);
    let spool = state
        .as_mut()
        .ok_or(CaptureError::Scratch(StoreError::Busy))?;
    let scratch = spool
        .connection
        .as_mut()
        .ok_or(CaptureError::Scratch(StoreError::Busy))?;
    #[cfg(test)]
    capture_stage("scratch-transaction");
    let scratch_tx = scratch
        .transaction()
        .map_err(|error| CaptureError::Scratch(error.into()))?;
    #[cfg(test)]
    capture_stage("source-row-query");
    let mut rows = tx.prepare_cached(
        "SELECT row_seq,update_seq,entry_seq,item_index,codec,length(item),item FROM timeline_rows
         WHERE task_id=?1 AND epoch=?2 AND hidden=0 ORDER BY row_seq",
    ).map_err(|error| CaptureError::Source(error.into()))?;
    let mut rows = rows
        .query(params![scope.task_key.0, snapshot.stamp.rows_epoch])
        .map_err(|error| CaptureError::Source(error.into()))?;
    #[cfg(test)]
    capture_stage("scratch-space-probe");
    let mut space = SpaceBudget::new(store.paths.root()).map_err(CaptureError::Scratch)?;
    #[cfg(test)]
    capture_stage("source-next-row");
    while let Some(row) = rows
        .next()
        .map_err(|error| CaptureError::Source(error.into()))?
    {
        #[cfg(test)]
        capture_stage("capture-cancellation-or-deadline");
        if let Some(error) = stop_error(&snapshot.cancellation, started, budget) {
            return Err(CaptureError::Scratch(error));
        }
        #[cfg(test)]
        capture_stage("source-row-size");
        let stored: i64 = row
            .get(5)
            .map_err(|error| CaptureError::Source(error.into()))?;
        let stored =
            usize::try_from(stored).map_err(|_| CaptureError::Source(StoreError::InvalidData))?;
        // A full ACP row is bounded independently of conversation length. A
        // malformed stored row must not allocate the whole available disk/RAM.
        if stored > super::codec::MAX_DECODED_BYTES {
            return Err(CaptureError::Source(StoreError::InvalidData));
        }
        #[cfg(test)]
        capture_stage("scratch-space-admission");
        space
            .before_write(stored as u64)
            .map_err(CaptureError::Scratch)?;
        #[cfg(test)]
        capture_stage("source-row-payload");
        let payload = Zeroizing::new(
            row.get::<_, Vec<u8>>(6)
                .map_err(|error| CaptureError::Source(error.into()))?,
        );
        if payload.len() != stored {
            return Err(CaptureError::Source(StoreError::InvalidData));
        }
        #[cfg(test)]
        capture_stage("scratch-first-row-insert");
        scratch_tx.execute(
            "INSERT INTO frozen(row_seq,update_seq,entry_seq,item_index,codec,item) VALUES(?1,?2,?3,?4,?5,?6)",
            params![row.get::<_, i64>(0).map_err(|error| CaptureError::Source(error.into()))?,
                row.get::<_, i64>(1).map_err(|error| CaptureError::Source(error.into()))?,
                row.get::<_, i64>(2).map_err(|error| CaptureError::Source(error.into()))?,
                row.get::<_, i64>(3).map_err(|error| CaptureError::Source(error.into()))?,
                row.get::<_, i64>(4).map_err(|error| CaptureError::Source(error.into()))?,
                payload.as_slice()],
        ).map_err(|error| CaptureError::Scratch(error.into()))?;
        #[cfg(test)]
        {
            let hook = snapshot.capture_hook.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
            }
        }
    }
    if let Some(error) = stop_error(&snapshot.cancellation, started, budget) {
        return Err(CaptureError::Scratch(error));
    }
    scratch_tx
        .commit()
        .map_err(|error| CaptureError::Scratch(error.into()))?;
    Ok(scope)
}

fn source_scope(
    tx: &Transaction<'_>,
    account: AccountId,
    task: &str,
    expected: TaskHistoryStamp,
) -> StoreResult<Scope> {
    #[cfg(test)]
    capture_stage("source-account-authority");
    tasks::writable_account(tx, account)?;
    #[cfg(test)]
    capture_stage("source-task-identity");
    let task_key = tasks::task_key_tx(tx, account, task)?;
    #[cfg(test)]
    capture_stage("source-history-stamp");
    let stamp: TaskHistoryStamp = tx.query_row(
        "SELECT rows_epoch,last_seq FROM tasks WHERE id=?1",
        [task_key.0],
        |row| {
            Ok(TaskHistoryStamp {
                rows_epoch: row.get(0)?,
                last_seq: row.get(1)?,
            })
        },
    )?;
    if stamp != expected {
        return Err(StoreError::Conflict);
    }
    #[cfg(test)]
    capture_stage("source-user-identity");
    let (user_key, user_id) = tx.query_row(
        "SELECT user_key,user_id FROM accounts WHERE id=?1",
        [account.0],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    #[cfg(test)]
    capture_stage("source-store-identity");
    Ok(Scope {
        store_id: store_id(tx)?,
        task_key,
        user_key,
        user_id,
    })
}

fn authorize(
    tx: &Transaction<'_>,
    account: AccountId,
    task: &str,
    scope: &Scope,
) -> StoreResult<()> {
    tasks::writable_account(tx, account)?;
    let (user_key, user_id): (String, String) = tx.query_row(
        "SELECT user_key,user_id FROM accounts WHERE id=?1",
        [account.0],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if user_key != scope.user_key
        || user_id != scope.user_id
        || tasks::task_key_tx(tx, account, task)? != scope.task_key
        || store_id(tx)? != scope.store_id
    {
        return Err(StoreError::Conflict);
    }
    Ok(())
}

fn store_id(tx: &Transaction<'_>) -> StoreResult<[u8; 16]> {
    let bytes: Vec<u8> = tx.query_row(
        "SELECT value FROM store_meta WHERE key='store_id'",
        [],
        |row| row.get(0),
    )?;
    bytes.try_into().map_err(|_| StoreError::InvalidData)
}

fn stop_error(cancellation: &AtomicBool, started: Instant, budget: Duration) -> Option<StoreError> {
    if cancellation.load(Ordering::Acquire) {
        Some(StoreError::Busy)
    } else if started.elapsed() >= budget {
        Some(StoreError::Io {
            kind: std::io::ErrorKind::TimedOut,
        })
    } else {
        None
    }
}

fn stopped(cancellation: &AtomicBool, started: Instant, budget: Duration) -> bool {
    stop_error(cancellation, started, budget).is_some()
}

struct ProgressReset<'a>(&'a Connection);
impl Drop for ProgressReset<'_> {
    fn drop(&mut self) {
        let _ = self.0.progress_handler(0, None::<fn() -> bool>);
    }
}

struct FrozenRow {
    row_seq: i64,
    update_seq: i64,
    codec: i64,
    payload: Zeroizing<Vec<u8>>,
}

struct Spool {
    connection: Option<Connection>,
    key: Option<LockedKey>,
    path: PathBuf,
    after: i64,
}

impl Spool {
    fn create(root: &Path) -> StoreResult<Self> {
        let directory = root.join("replays");
        #[cfg(test)]
        capture_stage("scratch-private-root");
        verify_private_path(root, true)?;
        #[cfg(test)]
        capture_stage("scratch-private-directory");
        private_directory(&directory)?;
        let path = directory.join(format!(
            "{SCRATCH_PREFIX}{}{SCRATCH_SUFFIX}",
            uuid::Uuid::new_v4()
        ));
        #[cfg(test)]
        capture_stage("scratch-private-file");
        drop(private_new_file(&path)?);
        let mut spool = Self {
            connection: None,
            key: None,
            path,
            after: 0,
        };
        #[cfg(test)]
        capture_stage("scratch-locked-key");
        spool.key = Some(LockedKey::random().map_err(|_error| {
            #[cfg(test)]
            eprintln!("replay scratch key setup failed: {_error:?}");
            StoreError::Busy
        })?);
        #[cfg(test)]
        capture_stage("scratch-sqlite-open");
        let connection = Connection::open_with_flags(
            &spool.path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        #[cfg(test)]
        capture_stage("scratch-sqlcipher-key");
        engine::key_connection(&connection, spool.key.as_ref().ok_or(StoreError::Busy)?)?;

        #[cfg(test)]
        capture_stage("scratch-sqlite-config");
        connection.pragma_update(None, "trusted_schema", false)?;
        connection.pragma_update(None, "temp_store", "MEMORY")?;
        connection.pragma_update(None, "cache_size", -4096)?;
        // Private, disposable, and never recovered: no journal/WAL/keyfile is
        // needed. The key must still be installed before ANY schema/data SQL.
        let mode: String = connection.query_row("PRAGMA journal_mode=OFF", [], |row| row.get(0))?;
        if !mode.eq_ignore_ascii_case("off") {
            return Err(StoreError::InvalidData);
        }
        connection.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
        let mode: String = connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        if !mode.eq_ignore_ascii_case("off") {
            return Err(StoreError::InvalidData);
        }
        connection.pragma_update(None, "synchronous", "OFF")?;
        #[cfg(test)]
        capture_stage("scratch-schema");
        connection.execute_batch("CREATE TABLE frozen(row_seq INTEGER PRIMARY KEY,update_seq INTEGER NOT NULL,
            entry_seq INTEGER NOT NULL,item_index INTEGER NOT NULL,codec INTEGER NOT NULL,item BLOB NOT NULL) STRICT")?;
        #[cfg(test)]
        capture_stage("scratch-file-verification");
        verify_private_path(&spool.path, false)?;
        spool.connection = Some(connection);
        Ok(spool)
    }

    fn next_row(&mut self) -> StoreResult<Option<FrozenRow>> {
        use rusqlite::OptionalExtension;
        let connection = self.connection.as_ref().ok_or(StoreError::Busy)?;
        let row = connection.query_row(
            "SELECT row_seq,update_seq,codec,item FROM frozen WHERE row_seq>?1 ORDER BY row_seq LIMIT 1",
            [self.after], |row| Ok(FrozenRow { row_seq: row.get(0)?, update_seq: row.get(1)?,
                codec: row.get(2)?, payload: Zeroizing::new(row.get(3)?) }),
        ).optional()?;
        if let Some(row) = &row {
            self.after = row.row_seq;
        }
        Ok(row)
    }

    fn close(mut self) -> StoreResult<()> {
        self.connection.take();
        self.key.take();
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

impl Drop for Spool {
    fn drop(&mut self) {
        self.connection.take();
        self.key.take();
        let _ = fs::remove_file(&self.path);
    }
}

struct SpaceBudget<'a> {
    root: &'a Path,
    reserve: u64,
    until_check: u64,
}

impl<'a> SpaceBudget<'a> {
    fn new(root: &'a Path) -> StoreResult<Self> {
        let free = super::space::free_bytes(root)?;
        Ok(Self {
            root,
            reserve: (free / 10).max(16 * 1024 * 1024),
            until_check: 0,
        })
    }

    fn before_write(&mut self, bytes: u64) -> StoreResult<()> {
        if bytes >= self.until_check {
            let required = self
                .reserve
                .saturating_add(bytes.saturating_mul(2))
                .saturating_add(SPACE_CHECK_BYTES);
            if super::space::free_bytes(self.root)? < required {
                return Err(StoreError::DiskFull);
            }
            self.until_check = SPACE_CHECK_BYTES;
        }
        self.until_check = self.until_check.saturating_sub(bytes);
        Ok(())
    }
}

fn cleanup_stale(root: &Path) -> StoreResult<()> {
    let directory = root.join("replays");
    match fs::symlink_metadata(&directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
        Ok(_) => verify_private_path(&directory, true)?,
    }
    for entry in fs::read_dir(&directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_str().ok_or(StoreError::UnsafePath)?;
        let id = name
            .strip_prefix(SCRATCH_PREFIX)
            .and_then(|name| name.strip_suffix(SCRATCH_SUFFIX))
            .ok_or(StoreError::UnsafePath)?;
        if uuid::Uuid::parse_str(id).is_err() {
            return Err(StoreError::UnsafePath);
        }
        verify_private_path(&entry.path(), false)?;
        fs::remove_file(entry.path())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
