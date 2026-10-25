//! Host-owned device-local persistence. All app data lives in store-v1/.
//!
//! Only typed APIs leave this module. Raw connections and transactions are
//! restricted to the store subtree, and errors never retain SQL or row values.

mod attachment_cleanup;
mod attachment_migration;
mod attachments;
#[cfg(feature = "store-bench")]
pub mod bench;
mod codec;
mod connection;
#[cfg(test)]
mod disk_full_tests;
pub mod engine;
mod error;
mod gc;
mod idle_maintenance;
pub mod integrity;
mod integrity_retry;
pub mod keys;
pub mod lifecycle;
mod live_identity;
mod paging;
mod paths;
mod persistence;
mod projector;
pub mod records;
#[cfg(any(feature = "acp", test))]
mod replay;
#[cfg(not(any(feature = "acp", test)))]
#[path = "replay/disabled.rs"]
mod replay;
#[cfg(test)]
mod runtime_quarantine_tests;
mod schema;
mod session;
mod space;
pub mod spill;
mod tasks;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

pub use attachments::{
    ATTACHMENT_SOURCE_PREFIX, AttachmentCollectionProgress, AttachmentRange,
    AttachmentUploadMetadata, AttachmentUploadStart, AttachmentUploadStatus,
    AttachmentUploadWriter, ImageUpload, MAX_ATTACHMENT_BYTES, MAX_ATTACHMENT_RANGE_BYTES,
    MAX_ATTACHMENTS, MAX_UPLOAD_IMAGE_BYTES, StoredAttachment, UNAVAILABLE_IMAGE_SOURCE,
};
pub use error::{DamageReason, RefusalReason, StoreError, StoreResult};
pub use gc::{CollectionProgress, CollectionStage, DeletedTask};
pub use paging::{
    AttachmentRangeReply, HistoryAnchorRequest, HistoryPage, HistoryPageRequest,
    NavigationGetTasksReply, NavigationGroupPage, NavigationGroupQuery, NavigationProjectGroup,
    NavigationProjectPage, NavigationSearchQuery, NavigationTaskPage, NavigationTaskQuery,
    OpenTaskPage, OpenTaskPageRequest, PageTransport, ProjectSummary, ProjectTaskCursors, RowRange,
    RowRangeRequest, RowReader, TaskFactsReply, TaskHistoryStamp, TaskSummary, TimelineRow,
    TransportCaps,
};
pub use paths::{FilesystemKind, StorePaths};
pub use persistence::{PersistenceFailure, PersistenceFailureReason, PersistenceStatus};
pub(crate) use projector::{PreparedRebuild, RebuildProgress};
#[cfg(feature = "acp")]
pub(crate) use replay::ReplaySnapshot;
pub(crate) use session::{
    SessionLoad, SourceReceipt, SourceReceipts, WeakSourceReceipts, message_signature,
};
pub use tasks::{
    AccountDeletion, ArchiveProjectCandidates, DictionaryPage, NewTask, PinFilter, ProjectKey,
    ProjectPage, ProjectPosition, ProjectRecord, TaskFacts, TaskKey, TaskKind, TaskPage, TaskPatch,
    TaskPosition, TaskQuery, TaskRecord, TaskSection, TaskState,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AccountId(pub(super) i64);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum StoreState {
    Open,
    Locked,
    NeedsSetup,
    Maintenance,
    Damaged { reason: DamageReason },
    Refused { reason: RefusalReason },
}

/// Durable is the default for saved authoritative app state. Normal is reserved
/// for explicitly reconstructible caches whose loss cannot lose user data.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Durability {
    #[default]
    Durable,
    Normal,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoredRowIdentity {
    pub epoch: i64,
    pub row_seq: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoredRowChange {
    pub epoch: i64,
    pub row_key: String,
    pub row_seq: i64,
    pub entry_seq: i64,
    pub item_index: i64,
    pub origin_entry_id: String,
    pub update_entry_id: String,
    pub update_seq: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoreChange {
    pub account: Option<AccountId>,
    pub task: Option<String>,
    pub rows: Vec<StoredRowChange>,
    pub hidden_rows: Vec<StoredRowIdentity>,
    pub summary_changed: bool,
    pub listed_changed: bool,
    pub flipped_epoch: Option<i64>,
}

enum CheckpointMessage {
    Committed,
    Stop,
}

struct CheckpointWorker {
    sender: mpsc::Sender<CheckpointMessage>,
    thread: JoinHandle<()>,
}

pub struct Store {
    paths: StorePaths,
    state: Arc<RwLock<StoreState>>,
    writer: Mutex<Option<Connection>>,
    readers: [Mutex<Option<Connection>>; 2],
    bulk: Mutex<Option<Connection>>,
    checkpointer: Arc<Mutex<Option<Connection>>>,
    dek: Mutex<Option<keys::LockedKey>>,
    checkpoint_worker: Mutex<Option<CheckpointWorker>>,
    runtime_damage: Arc<AtomicBool>,
    lifecycle: Mutex<()>,
    lifecycle_control: lifecycle::LifecycleControl,
    next_reader: AtomicUsize,
    foreground_waiters: AtomicUsize,
    pub(super) attachment_locks: Arc<attachments::AttachmentLocks>,
    changes: tokio::sync::broadcast::Sender<StoreChange>,
    persistence: persistence::PersistenceState,
    replays: replay::ReplayRegistry,
}

impl Store {
    /// Open a plain store. Password and setup markers are never treated as an
    /// invitation to create an empty database; lifecycle APIs handle them.
    pub fn open(paths: StorePaths) -> StoreResult<Self> {
        let store = Self::empty(paths.prepare()?, StoreState::Maintenance);
        store.initialize_plain()?;
        Ok(store)
    }

    /// A handle can exist before a password or setup operation admits any SQL.
    pub fn unavailable(paths: StorePaths, state: StoreState) -> StoreResult<Self> {
        if state == StoreState::Open {
            return Err(StoreError::InvalidInput);
        }
        Ok(Self::empty(paths.prepare()?, state))
    }

    fn empty(paths: StorePaths, state: StoreState) -> Self {
        Self {
            paths,
            state: Arc::new(RwLock::new(state)),
            writer: Mutex::new(None),
            readers: std::array::from_fn(|_| Mutex::new(None)),
            bulk: Mutex::new(None),
            checkpointer: Arc::new(Mutex::new(None)),
            dek: Mutex::new(None),
            checkpoint_worker: Mutex::new(None),
            runtime_damage: Arc::new(AtomicBool::new(false)),
            lifecycle: Mutex::new(()),
            lifecycle_control: lifecycle::LifecycleControl::default(),
            next_reader: AtomicUsize::new(0),
            foreground_waiters: AtomicUsize::new(0),
            attachment_locks: Arc::new(attachments::AttachmentLocks::default()),
            changes: tokio::sync::broadcast::channel(256).0,
            persistence: persistence::PersistenceState::default(),
            replays: replay::ReplayRegistry::default(),
        }
    }

    /// Open on the same handle already held by IPC clients. This runs after the
    /// host has bound its socket, so migrations cannot delay its handshake.
    pub fn initialize_plain(&self) -> StoreResult<()> {
        if self.state() == StoreState::Open {
            return Err(StoreError::Conflict);
        }
        let result = (|| {
            if self.paths.setup().try_exists()? {
                return Err(StoreError::NeedsSetup);
            }
            if self.paths.keys().try_exists()?
                || self.paths.db().join("keys.backup.json").try_exists()?
            {
                return Err(StoreError::Locked);
            }
            self.initialize_with(&engine::guard_plain)
        })();
        if let Err(error) = &result {
            self.record_initialization_error(error);
        }
        result
    }

    pub(super) fn initialize_with(
        &self,
        initialize: &connection::Initializer<'_>,
    ) -> StoreResult<()> {
        let _lifecycle = self.lifecycle.lock().map_err(|_| StoreError::Busy)?;
        if self.writer.lock().map_err(|_| StoreError::Busy)?.is_some() {
            return Err(StoreError::Conflict);
        }
        *self.state.write().map_err(|_| StoreError::Busy)? = StoreState::Maintenance;
        let result = self
            .install_connections(initialize)
            .and_then(|()| self.ensure_generation_metadata());
        match &result {
            Ok(()) => {
                *self.state.write().map_err(|_| StoreError::Busy)? = StoreState::Open;
                self.resume_replays();
            }
            Err(error) => {
                let _ = self.close_connections();
                self.record_initialization_error(error);
            }
        }
        result
    }

    /// Key ownership is installed before Open is visible and before close can
    /// acquire the lifecycle mutex. A failed open closes connections before the
    /// dedicated key mapping is dropped.
    pub(super) fn initialize_key_material(
        &self,
        key: keys::LockedKey,
        initialize: impl Fn(&Connection, &keys::LockedKey) -> StoreResult<()>,
    ) -> StoreResult<()> {
        let _lifecycle = self.lifecycle.lock().map_err(|_| StoreError::Busy)?;
        if self.writer.lock().map_err(|_| StoreError::Busy)?.is_some() {
            return Err(StoreError::Conflict);
        }
        *self.state.write().map_err(|_| StoreError::Busy)? = StoreState::Maintenance;
        let result = self
            .install_connections(&|connection| initialize(connection, &key))
            .and_then(|()| self.ensure_generation_metadata());
        match &result {
            Ok(()) => {
                *self.dek.lock().map_err(|_| StoreError::Busy)? = Some(key);
                *self.state.write().map_err(|_| StoreError::Busy)? = StoreState::Open;
                self.resume_replays();
            }
            Err(error) => {
                let _ = self.close_connections();
                self.record_initialization_error(error);
            }
        }
        result
    }

    fn install_connections(&self, initialize: &connection::Initializer<'_>) -> StoreResult<()> {
        if matches!(
            self.paths.filesystem_kind()?,
            FilesystemKind::Network | FilesystemKind::Virtual
        ) {
            return Err(StoreError::UnsupportedFilesystem);
        }
        self.paths.precreate_database()?;
        let mut writer = connection::open_initializing_writer(&self.paths, initialize)?;
        schema::migrate(&mut writer, now_ms())?;
        writer.execute_batch("PRAGMA optimize = 0x10002")?;
        schema::mark_interrupted_receipts(&mut writer)?;
        let generation: Vec<u8> = writer.query_row(
            "SELECT value FROM store_meta WHERE key='store_id'",
            [],
            |row| row.get(0),
        )?;
        let generation: [u8; 16] = generation.try_into().map_err(|_| StoreError::InvalidData)?;
        let first = connection::open(
            &self.paths,
            connection::Role::Interactive,
            false,
            initialize,
        )?;
        let second = connection::open(
            &self.paths,
            connection::Role::Interactive,
            false,
            initialize,
        )?;
        let bulk = connection::open(&self.paths, connection::Role::Bulk, false, initialize)?;
        let checkpointer = connection::open(
            &self.paths,
            connection::Role::Checkpointer,
            false,
            initialize,
        )?;
        self.start_checkpointer()?;
        *self.writer.lock().map_err(|_| StoreError::Busy)? = Some(writer);
        *self.readers[0].lock().map_err(|_| StoreError::Busy)? = Some(first);
        *self.readers[1].lock().map_err(|_| StoreError::Busy)? = Some(second);
        *self.bulk.lock().map_err(|_| StoreError::Busy)? = Some(bulk);
        *self.checkpointer.lock().map_err(|_| StoreError::Busy)? = Some(checkpointer);
        self.persistence.set_generation(generation);
        self.runtime_damage.store(false, Ordering::Release);
        Ok(())
    }

    fn record_initialization_error(&self, error: &StoreError) {
        if matches!(error, StoreError::Conflict) {
            return;
        }
        if let Ok(mut state) = self.state.write() {
            *state = match error {
                StoreError::Locked => StoreState::Locked,
                StoreError::NeedsSetup => StoreState::NeedsSetup,
                StoreError::Busy | StoreError::DiskFull => StoreState::Maintenance,
                StoreError::Damaged { reason } => StoreState::Damaged { reason: *reason },
                StoreError::Refused { reason } => StoreState::Refused { reason: *reason },
                StoreError::NewerVersion { .. } => StoreState::Refused {
                    reason: RefusalReason::NewerVersion,
                },
                StoreError::SchemaEpoch { .. } | StoreError::SchemaMismatch => {
                    StoreState::Refused {
                        reason: RefusalReason::IncompatibleSchema,
                    }
                }
                StoreError::UnsupportedFilesystem => StoreState::Refused {
                    reason: RefusalReason::UnsupportedFilesystem,
                },
                StoreError::UnsafePath => StoreState::Refused {
                    reason: RefusalReason::UnsafePath,
                },
                StoreError::UnsupportedOperationFormat => StoreState::Refused {
                    reason: RefusalReason::UnsupportedOperationFormat,
                },
                _ => StoreState::Refused {
                    reason: RefusalReason::Unavailable,
                },
            };
        }
        if matches!(error, StoreError::DiskFull) {
            self.record_persistence_failure(error);
        }
    }

    pub fn paths(&self) -> &StorePaths {
        &self.paths
    }

    pub fn state(&self) -> StoreState {
        self.state
            .read()
            .map(|state| state.clone())
            .unwrap_or(StoreState::Refused {
                reason: RefusalReason::Unavailable,
            })
    }

    pub fn subscribe_changes(&self) -> tokio::sync::broadcast::Receiver<StoreChange> {
        self.changes.subscribe()
    }

    pub fn upsert_account(&self, user_id: &str, seen_ms: i64) -> StoreResult<AccountId> {
        let key = normalize_user_key(user_id)?;
        self.write(Durability::Durable, |tx| {
            let account: Option<(i64, Option<String>, Option<i64>)> = tx.query_row(
                "SELECT id,deletion_phase,deleting_ms FROM accounts WHERE user_key=?1",
                [&key], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
            ).optional()?;
            if let Some((id, phase, deleting)) = account {
                if phase.is_some() || deleting.is_some() {
                    return Err(StoreError::Conflict);
                }
                tx.execute("UPDATE accounts SET user_id=?2,last_seen_ms=max(last_seen_ms,?3) WHERE id=?1",
                    params![id,user_id,seen_ms])?;
                Ok(AccountId(id))
            } else {
                let id = tx.query_row(
                    "INSERT INTO accounts(user_key,user_id,created_ms,last_seen_ms) VALUES (?1,?2,?3,?3) RETURNING id",
                    params![key,user_id,seen_ms], |row| row.get(0),
                )?;
                Ok(AccountId(id))
            }
        })
    }

    pub fn account_id(&self, user_id: &str) -> StoreResult<Option<AccountId>> {
        let key = normalize_user_key(user_id)?;
        self.read(|tx| {
            Ok(tx
                .query_row("SELECT id FROM accounts WHERE user_key=?1", [key], |row| {
                    row.get(0)
                })
                .optional()?
                .map(AccountId))
        })
    }

    /// Resolve the owner for account-scoped maintenance events at publication.
    pub(crate) fn account_user_id(&self, account: AccountId) -> StoreResult<Option<String>> {
        self.read(|tx| {
            Ok(tx
                .query_row(
                    "SELECT user_id FROM accounts WHERE id=?1",
                    [account.0],
                    |row| row.get(0),
                )
                .optional()?)
        })
    }

    /// Bounded primary-key discovery for host-owned maintenance. A pending
    /// account is discoverable but never grants collection or repair authority.
    pub(crate) fn accounts_for_maintenance(
        &self,
        after: Option<AccountId>,
        limit: u16,
    ) -> StoreResult<Vec<AccountId>> {
        if limit == 0 || limit > 200 || after.is_some_and(|account| account.0 < 1) {
            return Err(StoreError::InvalidInput);
        }
        self.read(|tx| {
            let mut statement =
                tx.prepare_cached("SELECT id FROM accounts WHERE id>?1 ORDER BY id LIMIT ?2")?;
            Ok(statement
                .query_map(
                    params![after.map_or(0, |account| account.0), limit],
                    |row| row.get(0).map(AccountId),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn store_id(&self) -> StoreResult<[u8; 16]> {
        self.read(|tx| {
            let id: Vec<u8> = tx.query_row(
                "SELECT value FROM store_meta WHERE key='store_id'",
                [],
                |row| row.get(0),
            )?;
            id.try_into().map_err(|_| StoreError::InvalidData)
        })
    }

    pub(super) fn ensure_not_refused(&self) -> StoreResult<()> {
        match self.state() {
            StoreState::Refused { reason } => Err(StoreError::Refused { reason }),
            _ => Ok(()),
        }
    }

    fn ensure_open(&self) -> StoreResult<()> {
        if self.runtime_damage.load(Ordering::Acquire) {
            return Err(StoreError::Damaged {
                reason: DamageReason::Database,
            });
        }
        match self.state() {
            StoreState::Open => Ok(()),
            StoreState::Locked => Err(StoreError::Locked),
            StoreState::NeedsSetup => Err(StoreError::NeedsSetup),
            StoreState::Maintenance => Err(StoreError::Busy),
            StoreState::Damaged { reason } => Err(StoreError::Damaged { reason }),
            StoreState::Refused { reason } => Err(StoreError::Refused { reason }),
        }
    }

    pub(super) fn write<T>(
        &self,
        durability: Durability,
        operation: impl FnOnce(&Transaction<'_>) -> StoreResult<T>,
    ) -> StoreResult<T> {
        self.write_with_change(durability, |tx| Ok((operation(tx)?, None)))
    }

    pub(super) fn write_with_change<T>(
        &self,
        durability: Durability,
        operation: impl FnOnce(&Transaction<'_>) -> StoreResult<(T, Option<StoreChange>)>,
    ) -> StoreResult<T> {
        self.ensure_open()?;
        let waiting = WaitingWrite::new(&self.foreground_waiters);
        let mut guard = self.writer.lock().map_err(|_| StoreError::Busy)?;
        drop(waiting);
        self.ensure_open()?;
        let writer = guard.as_mut().ok_or(StoreError::Busy)?;
        let result = self.write_locked(writer, durability, operation);
        self.freeze_damage(&result);
        drop(guard);
        self.observe_result(result)
    }

    /// Background collectors/rebuilds yield to every queued foreground write.
    /// A Busy result means reschedule this bounded chunk, never delete eagerly.
    pub(super) fn write_idle<T>(
        &self,
        operation: impl FnOnce(&Transaction<'_>) -> StoreResult<T>,
    ) -> StoreResult<T> {
        self.write_idle_with_change(|tx| Ok((operation(tx)?, None)))
    }

    /// The idle admission rules also apply to derived writes which publish a
    /// post-commit change. No event can escape a rolled-back transaction.
    pub(super) fn write_idle_with_change<T>(
        &self,
        operation: impl FnOnce(&Transaction<'_>) -> StoreResult<(T, Option<StoreChange>)>,
    ) -> StoreResult<T> {
        self.ensure_open()?;
        if self.foreground_waiters.load(Ordering::Acquire) != 0 {
            return Err(StoreError::Busy);
        }
        let mut guard = self.writer.try_lock().map_err(|_| StoreError::Busy)?;
        self.ensure_open()?;
        if self.foreground_waiters.load(Ordering::Acquire) != 0 {
            return Err(StoreError::Busy);
        }
        let writer = guard.as_mut().ok_or(StoreError::Busy)?;
        let result = self.write_locked(writer, Durability::Normal, operation);
        self.freeze_damage(&result);
        drop(guard);
        self.observe_result(result)
    }

    fn write_locked<T>(
        &self,
        writer: &mut Connection,
        durability: Durability,
        operation: impl FnOnce(&Transaction<'_>) -> StoreResult<(T, Option<StoreChange>)>,
    ) -> StoreResult<T> {
        if durability == Durability::Normal {
            writer.pragma_update(None, "synchronous", "NORMAL")?;
        }
        let result = (|| {
            let tx = writer.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let (result, change) = operation(&tx)?;
            tx.commit()?;
            Ok((result, change))
        })();
        // Preserve a native corruption result even if restoring the connection's
        // durability setting subsequently fails with a different category.
        self.freeze_damage(&result);
        if let Err(
            error @ (StoreError::DiskFull
            | StoreError::Database { .. }
            | StoreError::Io { .. }
            | StoreError::Damaged { .. }),
        ) = &result
        {
            self.record_persistence_failure(error);
        }
        // Restore even after a failed closure or failed COMMIT. Never return
        // the writer to authoritative callers with weaker sync settings.
        if durability == Durability::Normal
            && writer.pragma_update(None, "synchronous", "FULL").is_err()
        {
            *self.state.write().map_err(|_| StoreError::Busy)? = StoreState::Damaged {
                reason: DamageReason::Synchronization,
            };
            return Err(StoreError::Damaged {
                reason: DamageReason::Synchronization,
            });
        }
        let (value, change) = result?;
        if let Some(change) = change {
            let _ = self.changes.send(change);
        }
        if let Some(worker) = self
            .checkpoint_worker
            .lock()
            .map_err(|_| StoreError::Busy)?
            .as_ref()
        {
            let _ = worker.sender.send(CheckpointMessage::Committed);
        }
        Ok(value)
    }

    pub(super) fn read<T>(
        &self,
        operation: impl FnOnce(&Transaction<'_>) -> StoreResult<T>,
    ) -> StoreResult<T> {
        let index = self.next_reader.fetch_add(1, Ordering::Relaxed) % self.readers.len();
        self.read_on(&self.readers[index], operation)
    }

    pub(super) fn read_bulk<T>(
        &self,
        operation: impl FnOnce(&Transaction<'_>) -> StoreResult<T>,
    ) -> StoreResult<T> {
        self.read_on(&self.bulk, operation)
    }

    fn read_on<T>(
        &self,
        connection: &Mutex<Option<Connection>>,
        operation: impl FnOnce(&Transaction<'_>) -> StoreResult<T>,
    ) -> StoreResult<T> {
        self.ensure_open()?;
        let mut guard = connection.lock().map_err(|_| StoreError::Busy)?;
        self.ensure_open()?;
        let result = (|| {
            let connection = guard.as_mut().ok_or(StoreError::Busy)?;
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
            let value = operation(&transaction)?;
            transaction.commit()?;
            Ok(value)
        })();
        self.freeze_damage(&result);
        drop(guard);
        self.observe_result(result)
    }

    fn freeze_damage<T>(&self, result: &StoreResult<T>) {
        if let Err(StoreError::Damaged { reason }) = result {
            if *reason == DamageReason::Database {
                self.runtime_damage.store(true, Ordering::Release);
            }
            if let Ok(mut state) = self.state.write() {
                *state = StoreState::Damaged { reason: *reason };
            }
        }
    }

    fn observe_result<T>(&self, result: StoreResult<T>) -> StoreResult<T> {
        if result.is_err() && self.runtime_damage.load(Ordering::Acquire) {
            // No SQL connection guard survives here. Quarantine joins the
            // worker and writes the existing recovery journal before the
            // damaged operation returns, without checkpointing the source.
            if let Err(error) = self.quarantine_generation() {
                log::warn!("Local store damage quarantine could not be synchronized: {error}");
            }
        }
        result
    }

    fn start_checkpointer(&self) -> StoreResult<()> {
        self.start_checkpointer_with(connection::checkpoint)
    }

    fn start_checkpointer_with(
        &self,
        checkpoint: impl Fn(&Connection, bool) -> StoreResult<bool> + Send + 'static,
    ) -> StoreResult<()> {
        let mut worker = self
            .checkpoint_worker
            .lock()
            .map_err(|_| StoreError::Busy)?;
        if worker.is_some() {
            return Err(StoreError::Conflict);
        }
        let (sender, receiver) = mpsc::channel();
        let connection = Arc::clone(&self.checkpointer);
        let state = Arc::clone(&self.state);
        let paths = self.paths.clone();
        let damage = Arc::clone(&self.runtime_damage);
        let thread = std::thread::Builder::new()
            .name("maple-store-checkpoint".into())
            .spawn(move || {
                checkpoint_loop(receiver, connection, state, paths, damage, checkpoint);
            })?;
        *worker = Some(CheckpointWorker { sender, thread });
        Ok(())
    }

    /// Stop background work, drain transactions, truncate, and close the writer
    /// last. The host must hold its ownership lock throughout this call.
    pub fn close(&self) -> StoreResult<()> {
        let _lifecycle = self.lifecycle.lock().map_err(|_| StoreError::Busy)?;
        let damaged = matches!(self.state(), StoreState::Damaged { .. });
        *self.state.write().map_err(|_| StoreError::Busy)? = StoreState::Maintenance;
        let result = if damaged {
            self.close_connections_without_checkpoint()
        } else {
            self.close_connections()
        };
        self.lifecycle_control.clear_repair_material()?;
        result
    }

    /// A completed Quit must not destroy the host's retry authority when an
    /// outside reader prevents the final WAL checkpoint. Shutdown retains
    /// every native handle and the locked DEK on
    /// checkpoint failure. Only a fully restored Open store admits work again.
    pub fn close_for_shutdown(&self) -> StoreResult<()> {
        let _lifecycle = self.lifecycle.lock().map_err(|_| StoreError::Busy)?;
        let previous = self.state();
        *self.state.write().map_err(|_| StoreError::Busy)? = StoreState::Maintenance;
        let result = self.close_connections_inner(
            !matches!(previous, StoreState::Damaged { .. }),
            previous == StoreState::Open,
            false,
        );
        if result.is_ok() {
            self.lifecycle_control.clear_repair_material()?;
        }
        result
    }

    fn close_connections(&self) -> StoreResult<()> {
        self.close_connections_inner(true, false, false)
    }

    /// The lifecycle caller already froze admission and holds its gate. An
    /// operational checkpoint refusal retains all native handles and the DEK;
    /// success closes the native handles but leaves the locked mapping for the
    /// caller to transfer directly into its export/rollback authority.
    fn close_for_export(&self) -> StoreResult<()> {
        self.close_connections_inner(true, true, true)
    }

    /// Drain damaged handles without modifying the only remaining source copy.
    /// Native handles have NO_CKPT_ON_CLOSE configured during initialization.
    pub(super) fn close_connections_without_checkpoint(&self) -> StoreResult<()> {
        self.close_connections_inner(false, false, false)
    }

    fn close_connections_inner(
        &self,
        checkpoint_before_close: bool,
        retain_on_checkpoint_error: bool,
        retain_key_after_close: bool,
    ) -> StoreResult<()> {
        // Cancels registered-but-not-started workers as well as completed
        // private spools. No replay key survives the main store's key drain.
        self.replays.seal()?;
        if let Err(error) = self.cancel_all_replays() {
            // Private keys/connections have already been closed. A leftover
            // ciphertext file must not prevent draining authoritative keys.
            log::warn!("Private history replay file cleanup failed: {error}");
        }
        let worker = self
            .checkpoint_worker
            .lock()
            .map_err(|_| StoreError::Busy)?
            .take();
        if let Some(worker) = worker {
            let _ = worker.sender.send(CheckpointMessage::Stop);
            worker.thread.join().map_err(|_| StoreError::Busy)?;
        }
        // Acquiring every reader first drains outstanding snapshots, ensuring
        // the final TRUNCATE can actually reset the WAL.
        let mut first = self.readers[0].lock().map_err(|_| StoreError::Busy)?;
        let mut second = self.readers[1].lock().map_err(|_| StoreError::Busy)?;
        let mut bulk = self.bulk.lock().map_err(|_| StoreError::Busy)?;
        let mut checkpointer = self.checkpointer.lock().map_err(|_| StoreError::Busy)?;
        let mut writer = self.writer.lock().map_err(|_| StoreError::Busy)?;
        let runtime_damage = self.runtime_damage.load(Ordering::Acquire);
        if runtime_damage && let Err(error) = Self::persist_checkpoint_quarantine(&self.paths) {
            log::warn!("Local store damage quarantine could not be synchronized: {error}");
        }
        let checkpoint = if checkpoint_before_close && runtime_damage && writer.is_some() {
            // A worker may discover corruption while shutdown is joining it.
            // Never follow that report with another write into the same source.
            Err(StoreError::Damaged {
                reason: DamageReason::Database,
            })
        } else if checkpoint_before_close {
            writer
                .as_ref()
                .map(|connection| connection::checkpoint(connection, true))
                .transpose()
        } else {
            Ok(None)
        };
        let checkpoint = checkpoint.and_then(|completed| match completed {
            Some(false) => Err(StoreError::Busy),
            _ => Ok(()),
        });
        if let Err(StoreError::Damaged {
            reason: DamageReason::Database,
        }) = &checkpoint
        {
            self.runtime_damage.store(true, Ordering::Release);
            if let Ok(mut state) = self.state.write() {
                *state = StoreState::Damaged {
                    reason: DamageReason::Database,
                };
            }
            // The worker is already joined, and the lifecycle caller prevents
            // replacement while this filesystem-only marker is committed.
            if let Err(error) = Self::persist_checkpoint_quarantine(&self.paths) {
                log::warn!("Local store damage quarantine could not be synchronized: {error}");
            }
        }
        if retain_on_checkpoint_error && checkpoint.is_err() {
            // No connection or key has been taken. Release their guards before
            // restoring the single background worker so its first checkpoint
            // cannot contend with this shutdown attempt.
            let complete_handles = first.is_some()
                && second.is_some()
                && bulk.is_some()
                && checkpointer.is_some()
                && writer.is_some();
            drop(writer);
            drop(checkpointer);
            drop(bulk);
            drop(second);
            drop(first);
            if let Err(StoreError::Damaged { reason }) = &checkpoint {
                *self.state.write().map_err(|_| StoreError::Busy)? =
                    StoreState::Damaged { reason: *reason };
            } else if complete_handles && self.state() == StoreState::Maintenance {
                self.start_checkpointer()?;
                *self.state.write().map_err(|_| StoreError::Busy)? = StoreState::Open;
                self.resume_replays();
            }
            return checkpoint;
        }
        first.take();
        second.take();
        bulk.take();
        checkpointer.take();
        writer.take();
        // SQLCipher has now closed every connection that retained a key copy.
        // Export transfers the original locked mapping only after this success;
        // ordinary close wipes and unlocks it here.
        if !retain_key_after_close {
            self.dek.lock().map_err(|_| StoreError::Busy)?.take();
        }
        checkpoint
    }
}

struct WaitingWrite<'a>(&'a AtomicUsize);

impl<'a> WaitingWrite<'a> {
    fn new(counter: &'a AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self(counter)
    }
}

impl Drop for WaitingWrite<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        if let Err(error) = self.close() {
            log::warn!("Local store close did not finish cleanly: {error}");
        }
    }
}

fn normalize_user_key(user_id: &str) -> StoreResult<String> {
    let trimmed = user_id.trim();
    if trimmed.is_empty() || trimmed.chars().any(char::is_control) {
        return Err(StoreError::InvalidInput);
    }
    Ok(trimmed.to_ascii_lowercase())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

fn checkpoint_loop(
    receiver: mpsc::Receiver<CheckpointMessage>,
    connection: Arc<Mutex<Option<Connection>>>,
    state: Arc<RwLock<StoreState>>,
    paths: StorePaths,
    damage: Arc<AtomicBool>,
    checkpoint: impl Fn(&Connection, bool) -> StoreResult<bool>,
) {
    const INTERVAL: Duration = Duration::from_millis(250);
    const IDLE: Duration = Duration::from_secs(30);
    let mut last_write = Instant::now();
    let mut last_passive = Instant::now();
    let mut dirty = false;
    let mut needs_truncate = true;
    loop {
        let wait = if dirty {
            INTERVAL.saturating_sub(last_passive.elapsed())
        } else if needs_truncate {
            IDLE.saturating_sub(last_write.elapsed())
        } else {
            IDLE
        };
        match receiver.recv_timeout(wait) {
            Ok(CheckpointMessage::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(CheckpointMessage::Committed) => {
                last_write = Instant::now();
                dirty = true;
                needs_truncate = true;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        let current = state.read().map(|state| state.clone());
        if !matches!(current, Ok(StoreState::Open)) {
            if matches!(current, Ok(StoreState::Damaged { .. }) | Err(_)) {
                break;
            }
            last_write = Instant::now();
            last_passive = Instant::now();
            continue;
        }
        let truncate = needs_truncate && last_write.elapsed() >= IDLE;
        if truncate || (dirty && last_passive.elapsed() >= INTERVAL) {
            let result = connection
                .lock()
                .map_err(|_| StoreError::Busy)
                .and_then(|connection| {
                    connection
                        .as_ref()
                        .map(|connection| checkpoint(connection, truncate))
                        .transpose()
                        .map(|result| result.unwrap_or(true))
                });
            match result {
                Ok(true) => {
                    dirty = false;
                    if truncate {
                        needs_truncate = false;
                    }
                }
                Ok(false) | Err(StoreError::Busy) => {}
                Err(StoreError::Damaged { reason }) => {
                    damage.store(true, Ordering::Release);
                    if let Ok(mut state) = state.write() {
                        *state = StoreState::Damaged { reason };
                    }
                    // This thread owns no connection guard now. Its owner must
                    // join it before replacing any generation files, so the
                    // existing journal can be persisted without taking lifecycle
                    // or attempting to join this same thread.
                    if let Err(error) = Store::persist_checkpoint_quarantine(&paths) {
                        log::warn!(
                            "Local store damage quarantine could not be synchronized: {error}"
                        );
                    }
                    break;
                }
                Err(error) => log::warn!("Local store checkpoint failed: {error}"),
            }
            last_passive = Instant::now();
            if truncate && needs_truncate {
                // A blocked reader must not turn idle retry into a busy loop.
                last_write = Instant::now();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(StorePaths::new(temp.path())).unwrap();
        (temp, store)
    }

    #[test]
    fn bundled_sqlite_is_past_the_wal_reset_fix() {
        assert!(rusqlite::version_number() >= 3_051_003);
    }

    #[test]
    fn account_normalization_is_shared_without_reactivating_deletion() {
        let (_temp, store) = store();
        let account = store.upsert_account("  User-ABC  ", 1).unwrap();
        assert_eq!(store.upsert_account("user-abc", 2).unwrap(), account);
        store
            .write(Durability::Durable, |tx| {
                tx.execute(
                    "UPDATE accounts SET deletion_phase='pending_server',deleting_ms=3 WHERE id=?1",
                    [account.0],
                )?;
                Ok(())
            })
            .unwrap();
        assert_eq!(
            store.upsert_account("USER-ABC", 4),
            Err(StoreError::Conflict)
        );
        assert_eq!(store.account_id("user-abc").unwrap(), Some(account));
    }

    #[test]
    fn all_roles_apply_private_connection_settings_and_readers_cannot_write() {
        let (_temp, store) = store();
        for role in [&store.readers[0], &store.readers[1], &store.bulk] {
            let guard = role.lock().unwrap();
            let connection = guard.as_ref().unwrap();
            for (name, value) in [
                ("query_only", 1),
                ("foreign_keys", 1),
                ("trusted_schema", 0),
                ("temp_store", 2),
                ("cache_size", -32_768),
                ("checkpoint_fullfsync", 1),
                ("wal_autocheckpoint", 0),
            ] {
                assert_eq!(
                    connection
                        .pragma_query_value::<i64, _>(None, name, |r| r.get(0))
                        .unwrap(),
                    value,
                    "{name}"
                );
            }
            assert!(
                connection
                    .execute("INSERT INTO dictionary_words(word) VALUES ('x')", [])
                    .is_err()
            );
            assert!(
                connection
                    .execute_batch("PRAGMA optimize = 0x10002")
                    .is_err()
            );
        }
        let guard = store.writer.lock().unwrap();
        let connection = guard.as_ref().unwrap();
        assert_eq!(
            connection
                .pragma_query_value::<i64, _>(None, "synchronous", |r| r.get(0))
                .unwrap(),
            2
        );
        assert_eq!(
            connection
                .pragma_query_value::<i64, _>(None, "fullfsync", |r| r.get(0))
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .pragma_query_value::<i64, _>(None, "secure_delete", |r| r.get(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn failed_transactions_roll_back_and_restore_durable_settings() {
        let (_temp, store) = store();
        let result: StoreResult<()> = store.write(Durability::Normal, |tx| {
            tx.execute(
                "INSERT INTO dictionary_words(word) VALUES ('never committed')",
                [],
            )?;
            Err(StoreError::InvalidInput)
        });
        assert_eq!(result, Err(StoreError::InvalidInput));
        assert_eq!(
            store
                .read(|tx| Ok(tx.query_row::<i64, _, _>(
                    "SELECT count(*) FROM dictionary_words",
                    [],
                    |r| r.get(0)
                )?))
                .unwrap(),
            0
        );
        let guard = store.writer.lock().unwrap();
        assert_eq!(
            guard
                .as_ref()
                .unwrap()
                .pragma_query_value::<i64, _>(None, "synchronous", |r| r.get(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn changes_only_follow_successful_commits_in_commit_order() {
        let (_temp, store) = store();
        let mut receiver = store.subscribe_changes();
        let failed: StoreResult<()> =
            store.write_with_change(Durability::Durable, |_| Err(StoreError::InvalidInput));
        assert!(failed.is_err());
        assert!(receiver.try_recv().is_err());
        for task in ["one", "two"] {
            store
                .write_with_change(Durability::Durable, |_| {
                    Ok((
                        (),
                        Some(StoreChange {
                            task: Some(task.into()),
                            ..Default::default()
                        }),
                    ))
                })
                .unwrap();
        }
        assert_eq!(receiver.try_recv().unwrap().task.as_deref(), Some("one"));
        assert_eq!(receiver.try_recv().unwrap().task.as_deref(), Some("two"));
    }

    #[test]
    fn close_truncates_wal_and_refuses_new_work() {
        let (_temp, store) = store();
        store.upsert_account("user", 1).unwrap();
        store.close().unwrap();
        let wal = store.paths.db().join("maple.db-wal");
        assert!(!wal.exists() || std::fs::metadata(wal).unwrap().len() == 0);
        assert_eq!(store.upsert_account("next", 2), Err(StoreError::Busy));
        store.close().unwrap();
    }

    #[test]
    fn shutdown_checkpoint_busy_retains_handles_key_and_one_background_worker() {
        for keyed in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let paths = StorePaths::new(temp.path());
            let key_bytes = [0x93; 32];
            let store = if keyed {
                Store::open_keyed(
                    paths.clone(),
                    keys::LockedKey::from_bytes(key_bytes).unwrap(),
                )
                .unwrap()
            } else {
                Store::open(paths.clone()).unwrap()
            };
            store.upsert_account("before-snapshot", 1).unwrap();
            let reader = connection::open(
                store.paths(),
                connection::Role::Interactive,
                false,
                &|conn| {
                    if keyed {
                        engine::key_connection(
                            conn,
                            &keys::LockedKey::from_bytes(key_bytes).unwrap(),
                        )
                    } else {
                        engine::guard_plain(conn)
                    }
                },
            )
            .unwrap();
            reader.execute_batch("BEGIN").unwrap();
            let _: i64 = reader
                .query_row("SELECT count(*) FROM accounts", [], |row| row.get(0))
                .unwrap();
            let saved = store.upsert_account("after-snapshot", 2).unwrap();
            store
                .writer
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .busy_timeout(Duration::from_millis(20))
                .unwrap();
            for _ in 0..2 {
                assert_eq!(store.close_for_shutdown(), Err(StoreError::Busy));
                assert_eq!(store.state(), StoreState::Open);
                assert_eq!(store.account_id("after-snapshot").unwrap(), Some(saved));
                assert_eq!(store.dek.lock().unwrap().is_some(), keyed);
                assert!(store.checkpoint_worker.lock().unwrap().is_some());
                assert_eq!(store.start_checkpointer(), Err(StoreError::Conflict));
            }
            store.upsert_account("retained-host-write", 3).unwrap();
            reader.execute_batch("ROLLBACK").unwrap();
            drop(reader);
            store.close_for_shutdown().unwrap();
            assert!(store.checkpoint_worker.lock().unwrap().is_none());
            assert!(store.dek.lock().unwrap().is_none());
            assert_eq!(store.account_id("after-snapshot"), Err(StoreError::Busy));
            let reopened = if keyed {
                Store::open_keyed(paths, keys::LockedKey::from_bytes(key_bytes).unwrap()).unwrap()
            } else {
                Store::open(paths).unwrap()
            };
            assert_eq!(reopened.account_id("after-snapshot").unwrap(), Some(saved));
            assert!(
                reopened
                    .account_id("retained-host-write")
                    .unwrap()
                    .is_some()
            );
            reopened.close_for_shutdown().unwrap();
        }
    }

    #[test]
    fn incompatible_and_operational_open_failures_never_authorize_damaged_reset() {
        let temp = tempfile::tempdir().unwrap();
        let store =
            Store::unavailable(StorePaths::new(temp.path()), StoreState::Maintenance).unwrap();
        for (error, reason) in [
            (
                StoreError::NewerVersion {
                    found: 3,
                    supported: 2,
                },
                RefusalReason::NewerVersion,
            ),
            (
                StoreError::SchemaEpoch {
                    found: 1,
                    supported: 2,
                },
                RefusalReason::IncompatibleSchema,
            ),
            (
                StoreError::SchemaMismatch,
                RefusalReason::IncompatibleSchema,
            ),
            (
                StoreError::UnsupportedFilesystem,
                RefusalReason::UnsupportedFilesystem,
            ),
            (StoreError::UnsafePath, RefusalReason::UnsafePath),
            (
                StoreError::UnsupportedOperationFormat,
                RefusalReason::UnsupportedOperationFormat,
            ),
            (
                StoreError::Io {
                    kind: std::io::ErrorKind::PermissionDenied,
                },
                RefusalReason::Unavailable,
            ),
            (
                StoreError::Database {
                    code: 10,
                    extended_code: 10,
                },
                RefusalReason::Unavailable,
            ),
            (StoreError::InvalidData, RefusalReason::Unavailable),
        ] {
            store.record_initialization_error(&error);
            assert_eq!(store.state(), StoreState::Refused { reason });
            assert_eq!(
                store.account_id("refused"),
                Err(StoreError::Refused { reason })
            );
            assert_eq!(
                store.erase_local_data(lifecycle::ERASE_CONFIRMATION),
                Err(lifecycle::LifecycleError::Store(StoreError::Refused {
                    reason
                })),
            );
            assert_eq!(
                store.setup_store(None, &[], keys::KdfParams::floor()),
                Err(lifecycle::LifecycleError::Store(StoreError::Refused {
                    reason
                })),
            );
            let status = store.lock_status().unwrap();
            assert_eq!(status.state, StoreState::Refused { reason });
            assert!(status.store_id.is_none());
            assert!(status.setup_operation_id.is_none());
            assert!(!store.paths.database().exists());
            assert!(!store.paths.setup().exists());
        }
        let reason = DamageReason::Database;
        store.record_initialization_error(&StoreError::Damaged { reason });
        assert_eq!(store.state(), StoreState::Damaged { reason });
    }

    #[test]
    fn unavailable_handles_never_open_or_create_a_database() {
        let temp = tempfile::tempdir().unwrap();
        for (state, error) in [
            (StoreState::Locked, StoreError::Locked),
            (StoreState::NeedsSetup, StoreError::NeedsSetup),
            (StoreState::Maintenance, StoreError::Busy),
        ] {
            let store = Store::unavailable(StorePaths::new(temp.path()), state).unwrap();
            assert_eq!(store.account_id("user"), Err(error));
            assert!(!store.paths.database().exists());
        }
    }

    #[test]
    fn the_same_handle_opens_after_the_handshake_and_can_reopen_after_close() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(
            Store::unavailable(StorePaths::new(temp.path()), StoreState::Maintenance).unwrap(),
        );
        let client_handle = Arc::clone(&store);
        let mut subscriber = client_handle.subscribe_changes();
        store.initialize_plain().unwrap();
        assert_eq!(client_handle.state(), StoreState::Open);
        let account = client_handle.upsert_account("user", 1).unwrap();
        store
            .write_with_change(Durability::Durable, |_| {
                Ok((
                    (),
                    Some(StoreChange {
                        account: Some(account),
                        task: Some("task".into()),
                        ..Default::default()
                    }),
                ))
            })
            .unwrap();
        assert_eq!(subscriber.try_recv().unwrap().task.as_deref(), Some("task"));
        assert_eq!(store.initialize_plain(), Err(StoreError::Conflict));
        assert_eq!(store.state(), StoreState::Open);
        store.close().unwrap();
        store.initialize_plain().unwrap();
        assert_eq!(client_handle.account_id("user").unwrap(), Some(account));
    }

    #[test]
    fn idle_writes_yield_and_failed_chunks_do_not_publish_or_commit() {
        let (_temp, store) = store();
        store.foreground_waiters.store(1, Ordering::Release);
        assert_eq!(store.write_idle(|_| Ok(())), Err(StoreError::Busy));
        store.foreground_waiters.store(0, Ordering::Release);
        let result: StoreResult<()> = store.write_idle(|tx| {
            tx.execute("INSERT INTO dictionary_words(word) VALUES ('rollback')", [])?;
            Err(StoreError::InvalidInput)
        });
        assert_eq!(result, Err(StoreError::InvalidInput));
        assert_eq!(
            store
                .read(|tx| Ok(tx.query_row::<i64, _, _>(
                    "SELECT count(*) FROM dictionary_words",
                    [],
                    |r| r.get(0)
                )?))
                .unwrap(),
            0
        );
    }

    #[test]
    fn detecting_corruption_closes_the_write_admission_gate() {
        let (_temp, store) = store();
        let damaged = StoreError::Damaged {
            reason: DamageReason::Database,
        };
        let result: StoreResult<()> = store.read(|_| Err(damaged.clone()));
        assert_eq!(result, Err(damaged.clone()));
        assert_eq!(store.upsert_account("user", 1), Err(damaged));
    }
}
