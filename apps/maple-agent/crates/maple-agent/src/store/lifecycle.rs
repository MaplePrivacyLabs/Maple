//! Crash recovery and password operations, confined to store-v1/.
//!
//! The host must hold ownership and drain work before replacing a store. Every
//! retained source must be saved unless an exact destructive confirmation covers
//! its loss. Key-only password rewraps preserve database/runtime admission. This
//! module drains native readers before directory renames and rejects ordinary
//! access until metadata retirement is durable.

use super::space::free_bytes;

use std::{
    fmt,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use rand::{RngCore, rngs::OsRng};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use super::{
    DamageReason, Store, StoreError, StorePaths, StoreResult, StoreState, connection, engine,
    keys::{
        self, GeneratedPassphrase, KdfParams, KeyError, KeyFile, KeyFilePair, LockedKey, Password,
        WrappingKey,
    },
    paths::{private_directory, private_new_file, sync_directory, verify_private_path},
    schema,
};

pub const ERASE_CONFIRMATION: &str = "ERASE";
pub const START_FRESH_CONFIRMATION: &str = "START FRESH";
const GENERATION_FORMAT: &str = "maple-store-generation/1";
const SETUP_FORMAT: &str = "maple-store-setup/2";

pub type LifecycleResult<T> = Result<T, LifecycleError>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LifecycleError {
    Store(StoreError),
    Key(KeyError),
    Cancelled,
    Confirmation,
    NotEnoughSpace,
    VerificationIncomplete,
    Generation,
    RecoveryRequired { mode: StoreMode },
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => fmt::Display::fmt(error, f),
            Self::Key(error) => fmt::Display::fmt(error, f),
            Self::Cancelled => f.write_str("The local store operation was cancelled."),
            Self::Confirmation => f.write_str("Type the confirmation exactly to continue."),
            Self::NotEnoughSpace => {
                f.write_str("Maple needs room for a second copy of its local store.")
            }
            Self::VerificationIncomplete => f.write_str("Maple couldn't finish checking the replacement store. Retry the operation."),
            Self::Generation => f.write_str("Maple found conflicting local store generations."),
            Self::RecoveryRequired { .. } => f.write_str("Maple couldn't finish the store change. Keep the passwords used for this change and restart Maple to finish recovery."),
        }
    }
}

impl std::error::Error for LifecycleError {}
impl From<StoreError> for LifecycleError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}
impl From<KeyError> for LifecycleError {
    fn from(value: KeyError) -> Self {
        Self::Key(value)
    }
}
impl From<io::Error> for LifecycleError {
    fn from(value: io::Error) -> Self {
        Self::Store(value.into())
    }
}
impl From<rusqlite::Error> for LifecycleError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Store(value.into())
    }
}

fn startup_error(error: LifecycleError) -> StoreError {
    match error {
        LifecycleError::Store(error) => error,
        LifecycleError::Generation => StoreError::Damaged {
            reason: DamageReason::Generation,
        },
        LifecycleError::Key(_) => StoreError::Locked,
        _ => StoreError::InvalidData,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreMode {
    Plain,
    Password,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LockStatus {
    pub state: StoreState,
    pub mode: StoreMode,
    pub store_id: Option<String>,
    pub setup_operation_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationPhase {
    #[default]
    Idle,
    Preparing,
    Exporting,
    Verifying,
    Committing,
    Complete,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct StoreProgress {
    /// Identifies a newly started export; terminal snapshots are never reused.
    pub run_id: u64,
    pub phase: OperationPhase,
    pub completed_bytes: u64,
    pub total_bytes: u64,
    pub remaining_ms: Option<u64>,
    pub cancellable: bool,
}

#[derive(Default)]
pub(super) struct LifecycleControl {
    cancelled: Arc<AtomicBool>,
    active: AtomicBool,
    progress: Arc<Mutex<StoreProgress>>,
    repair_wrapping: Mutex<Option<WrappingKey>>,
}

impl LifecycleControl {
    pub(super) fn clear_repair_material(&self) -> StoreResult<()> {
        self.repair_wrapping
            .lock()
            .map_err(|_| StoreError::Busy)?
            .take();
        Ok(())
    }

    fn begin(&self) {
        self.cancelled.store(false, Ordering::Release);
        if let Ok(mut progress) = self.progress.lock() {
            *progress = StoreProgress {
                run_id: progress.run_id.wrapping_add(1),
                phase: OperationPhase::Preparing,
                cancellable: true,
                ..StoreProgress::default()
            };
        }
        self.active.store(true, Ordering::Release);
    }

    fn update(
        &self,
        phase: OperationPhase,
        done: u64,
        total: u64,
        remaining: Option<u64>,
        cancellable: bool,
    ) {
        if let Ok(mut progress) = self.progress.lock() {
            *progress = StoreProgress {
                run_id: progress.run_id,
                phase,
                completed_bytes: done,
                total_bytes: total,
                remaining_ms: remaining,
                cancellable,
            };
        }
    }

    fn finish(&self) {
        self.active.store(false, Ordering::Release);
        if let Ok(mut progress) = self.progress.lock() {
            progress.cancellable = false;
        }
    }

    fn check_cancelled(&self) -> LifecycleResult<()> {
        if self.cancelled.load(Ordering::Acquire) {
            Err(LifecycleError::Cancelled)
        } else {
            Ok(())
        }
    }
}

struct ActiveOperation<'a>(&'a LifecycleControl);
impl Drop for ActiveOperation<'_> {
    fn drop(&mut self) {
        self.0.finish();
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Generation {
    format: String,
    store_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    setup_operation_id: Option<String>,
}

impl Generation {
    fn new(store_id: [u8; 16], setup_operation_id: Option<String>) -> Self {
        Self {
            format: GENERATION_FORMAT.into(),
            store_id: encode_id(&store_id),
            setup_operation_id,
        }
    }

    fn validate(&self) -> LifecycleResult<()> {
        if self.format != GENERATION_FORMAT
            || decode_id(&self.store_id).is_none()
            || self
                .setup_operation_id
                .as_ref()
                .is_some_and(|id| decode_id(id).is_none())
        {
            return Err(LifecycleError::Generation);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ResetAfter {
    Erase,
    StartFresh,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SetupPhase {
    /// A known-damaged original is retained while an uncommitted salvage is
    /// built. This marker also preserves quarantine after an aborted build.
    BuildReplacement,
    RetireOld,
    AwaitChoice,
    InstallNew,
    Installed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SetupJournal {
    format: String,
    operation_id: String,
    after: ResetAfter,
    previous_mode: StoreMode,
    old_store_id: String,
    new_store_id: String,
    phase: SetupPhase,
}

impl SetupJournal {
    fn validate(&self) -> LifecycleResult<()> {
        if self.format != SETUP_FORMAT {
            return Err(StoreError::UnsupportedOperationFormat.into());
        }
        if decode_id(&self.operation_id).is_none()
            || decode_id(&self.old_store_id).is_none()
            || decode_id(&self.new_store_id).is_none()
            || self.old_store_id == self.new_store_id
        {
            return Err(LifecycleError::Generation);
        }
        Ok(())
    }

    fn replacement(&self) -> Generation {
        Generation {
            format: GENERATION_FORMAT.into(),
            store_id: self.new_store_id.clone(),
            setup_operation_id: Some(self.operation_id.clone()),
        }
    }

    fn retired_source(&self, paths: &StorePaths) -> PathBuf {
        paths.root().join("db.old")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UnlockNotice {
    pub used_previous_password: bool,
    pub reconciliation_pending: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PasswordChangeResult {
    pub mode: StoreMode,
    pub backup_pending: bool,
    pub synchronization_pending: bool,
    pub notice_pending: bool,
}

/// Suggested is a host-owned opaque generator result. Never deserialize this
/// distinction from a client boolean; edited text must use Typed.
#[derive(Clone, Copy, Debug)]
pub enum PasswordChoice<'a> {
    Typed(&'a Password),
    Suggested(&'a GeneratedPassphrase),
}

impl<'a> PasswordChoice<'a> {
    fn password(self) -> &'a Password {
        match self {
            Self::Typed(password) => password,
            Self::Suggested(generated) => generated.password(),
        }
    }

    fn assess(self, context: &[&str]) -> keys::PasswordAssessment {
        match self {
            Self::Typed(password) => password.assess(context),
            Self::Suggested(generated) => generated.assessment(),
        }
    }

    fn create_slot(
        self,
        context: &[&str],
        key: &LockedKey,
        salt: [u8; 16],
        params: KdfParams,
    ) -> Result<KeyFile, KeyError> {
        match self {
            Self::Typed(password) => KeyFile::create(password, context, key, salt, params),
            Self::Suggested(generated) => KeyFile::create_generated(generated, key, salt, params),
        }
    }
}

impl Store {
    /// Call after IPC is bound. The same handle remains installed in clients.
    /// A locked or setup-needed store is a successful initialization outcome.
    pub fn initialize_recovered(&self) -> StoreResult<()> {
        let result = (|| -> LifecycleResult<()> {
            let _gate = self.lifecycle.lock().map_err(|_| StoreError::Busy)?;
            if self.writer.lock().map_err(|_| StoreError::Busy)?.is_some() {
                return Err(StoreError::Conflict.into());
            }
            set_state(self, StoreState::Maintenance)?;
            if unclassified::pending(&self.paths)?.is_some()
                && let Some(journal) = unclassified::recover(&self.paths)?
            {
                set_state(self, StoreState::NeedsSetup)?;
                if journal.mode() == StoreMode::Plain {
                    self.complete_unclassified_locked(
                        &journal,
                        None,
                        &[],
                        KdfParams::floor(),
                        None,
                    )?;
                }
                return Ok(());
            }
            let pending = recover_filesystem(&self.paths)?;
            if let Some(journal) = pending {
                if journal.phase == SetupPhase::BuildReplacement {
                    set_state(
                        self,
                        match journal.previous_mode {
                            StoreMode::Plain => StoreState::Damaged {
                                reason: DamageReason::Database,
                            },
                            StoreMode::Password => StoreState::Locked,
                        },
                    )?;
                    return Ok(());
                }
                set_state(self, StoreState::NeedsSetup)?;
                if journal.previous_mode == StoreMode::Plain {
                    self.complete_setup_locked(&journal, None, &[], KdfParams::floor(), None)?;
                }
                return Ok(());
            }
            if disk_mode(&self.paths)? == StoreMode::Password {
                // Reconcile public slot files even while locked, so a torn
                // password change cannot leave an old backup slot indefinitely.
                // Metadata damage remains an unlock/key-file error, never an
                // invitation to create a plain database or run SQLite repair.
                if let Ok(salt) = database_salt(&self.paths.database()) {
                    let _ = KeyFilePair::load(&self.paths.db(), &salt);
                }
                set_state(self, StoreState::Locked)?;
                return Ok(());
            }
            self.install_connections(&engine::guard_plain)?;
            self.verify_or_create_generation()?;
            set_state(self, StoreState::Open)?;
            self.resume_replays();
            Ok(())
        })();
        if let Err(error) = result {
            if matches!(error, LifecycleError::Store(StoreError::Conflict)) {
                return Err(StoreError::Conflict);
            }
            let error = startup_error(error);
            let _ = self.close_connections();
            self.record_initialization_error(&error);
            return Err(error);
        }
        Ok(())
    }

    pub fn lock_status(&self) -> LifecycleResult<LockStatus> {
        let state = self.state();
        if matches!(
            state,
            StoreState::Refused { .. }
                | StoreState::Damaged {
                    reason: DamageReason::Generation
                }
        ) {
            // Known refusal or generation damage is authoritative. Do not
            // reinterpret it or read unsafe paths just to populate metadata.
            return Ok(LockStatus {
                state,
                // Conservative admission metadata, not a claim of encryption.
                mode: StoreMode::Password,
                store_id: None,
                setup_operation_id: None,
            });
        }
        // Status must not turn a transient metadata read failure into damage.
        // Initialization and verified corruption own state classification.
        self.lock_status_strict()
    }

    fn lock_status_strict(&self) -> LifecycleResult<LockStatus> {
        if let Some(status) = unclassified::status(&self.paths, self.state())? {
            return Ok(status);
        }
        let setup: Option<SetupJournal> =
            read_operation_json_optional(&self.paths.setup(), SETUP_FORMAT)?;
        if let Some(journal) = &setup {
            journal.validate()?;
        }
        let mode = match &setup {
            Some(journal) => journal.previous_mode,
            None => disk_mode(&self.paths)?,
        };
        let generation = read_generation(&self.paths.db())?;
        if let (Some(journal), Some(generation)) = (&setup, &generation)
            && generation.store_id != journal.old_store_id
            && generation != &journal.replacement()
        {
            return Err(LifecycleError::Generation);
        }
        Ok(LockStatus {
            state: self.state(),
            mode,
            store_id: generation.map(|generation| generation.store_id),
            setup_operation_id: setup.map(|journal| journal.operation_id),
        })
    }

    pub fn store_progress(&self) -> StoreProgress {
        self.lifecycle_control
            .progress
            .lock()
            .map(|value| value.clone())
            .unwrap_or_default()
    }

    pub fn cancel_store_operation(&self) -> bool {
        let Ok(progress) = self.lifecycle_control.progress.lock() else {
            return false;
        };
        // The same mutex protects the transition to non-cancellable commit.
        // An accepted cancel is visible before the final pre-rename check.
        if self.lifecycle_control.active.load(Ordering::Acquire) && progress.cancellable {
            self.lifecycle_control
                .cancelled
                .store(true, Ordering::Release);
            true
        } else {
            false
        }
    }

    /// Validate public slot metadata and bounds before the host calibrates a
    /// KDF. This performs no password derivation and opens no SQLite handle.
    pub fn validate_unlock_slot(&self) -> LifecycleResult<()> {
        let _gate = self.lifecycle.lock().map_err(|_| StoreError::Busy)?;
        if !matches!(
            self.state(),
            StoreState::Locked
                | StoreState::Damaged {
                    reason: DamageReason::Generation
                }
        ) {
            return Err(StoreError::Conflict.into());
        }
        let salt = database_salt(&self.paths.database())?;
        KeyFilePair::load(&self.paths.db(), &salt)?;
        Ok(())
    }

    pub fn unlock_store(
        &self,
        password: &Password,
        upgrade: Option<KdfParams>,
    ) -> LifecycleResult<UnlockNotice> {
        let _gate = self.lifecycle.lock().map_err(|_| StoreError::Busy)?;
        let damaged_generation = matches!(
            self.state(),
            StoreState::Damaged {
                reason: DamageReason::Generation
            }
        );
        if self.state() != StoreState::Locked && !damaged_generation {
            return Err(StoreError::Conflict.into());
        }
        let salt = database_salt(&self.paths.database())?;
        let mut pair = KeyFilePair::load(&self.paths.db(), &salt)?;
        let authenticated = if damaged_generation {
            let (key, wrapping) = pair.primary().authenticate(password)?;
            keys::AuthenticatedUnlock {
                key,
                wrapping,
                used_previous_password: false,
                reconciliation_pending: pair.reconciliation_pending(),
            }
        } else {
            pair.authenticate(password)?
        };
        let notice = UnlockNotice {
            used_previous_password: authenticated.used_previous_password,
            reconciliation_pending: authenticated.reconciliation_pending,
        };
        if damaged_generation
            && let Err(error) = self.repair_generation_from_readonly(Some(&authenticated.key))
        {
            *self.dek.lock().map_err(|_| StoreError::Busy)? = Some(authenticated.key);
            *self
                .lifecycle_control
                .repair_wrapping
                .lock()
                .map_err(|_| StoreError::Busy)? = Some(authenticated.wrapping);
            return Err(error);
        }
        let pending: Option<SetupJournal> =
            read_operation_json_optional(&self.paths.setup(), SETUP_FORMAT)?;
        if let Some(journal) = pending {
            journal.validate()?;
            if journal.phase == SetupPhase::BuildReplacement {
                preflight_setup(&self.paths, &journal)?;
                if notice.used_previous_password {
                    return Err(KeyError::WrongPassword.into());
                }
                *self.dek.lock().map_err(|_| StoreError::Busy)? = Some(authenticated.key);
                *self
                    .lifecycle_control
                    .repair_wrapping
                    .lock()
                    .map_err(|_| StoreError::Busy)? = Some(authenticated.wrapping);
                set_state(
                    self,
                    StoreState::Damaged {
                        reason: DamageReason::Database,
                    },
                )?;
                return Err(StoreError::Damaged {
                    reason: DamageReason::Database,
                }
                .into());
            }
        }
        set_state(self, StoreState::Maintenance)?;
        let opened = self
            .install_connections(&|connection| {
                engine::key_connection(connection, &authenticated.key)
            })
            .map_err(LifecycleError::from)
            .and_then(|()| self.verify_or_create_generation());
        if let Err(error) = opened {
            let _ = self.close_connections();
            // A previous-password fallback must not silently become the active
            // password of a replacement store after a completed change.
            if !notice.used_previous_password {
                *self
                    .lifecycle_control
                    .repair_wrapping
                    .lock()
                    .map_err(|_| StoreError::Busy)? = Some(authenticated.wrapping);
            }
            *self.dek.lock().map_err(|_| StoreError::Busy)? = Some(authenticated.key);
            let mapped = startup_error(error.clone());
            self.record_initialization_error(&mapped);
            return Err(error);
        }
        let mut backup_pending = notice.reconciliation_pending;
        if !notice.used_previous_password
            && let Some(target) = upgrade
            && pair.primary().params().should_raise_to(target)
        {
            match pair
                .primary()
                .raise(password, target)
                .and_then(|raised| KeyFilePair::write(&self.paths.db(), &raised))
            {
                Ok(outcome) => backup_pending |= outcome.backup_pending,
                Err(_) => backup_pending = true,
            }
        }
        *self.dek.lock().map_err(|_| StoreError::Busy)? = Some(authenticated.key);
        self.lifecycle_control
            .repair_wrapping
            .lock()
            .map_err(|_| StoreError::Busy)?
            .take();
        set_state(self, StoreState::Open)?;
        self.resume_replays();
        Ok(UnlockNotice {
            reconciliation_pending: backup_pending,
            ..notice
        })
    }

    pub fn set_store_password(
        &self,
        password: &Password,
        context: &[&str],
        params: KdfParams,
    ) -> LifecycleResult<PasswordChangeResult> {
        self.set_store_password_choice(PasswordChoice::Typed(password), context, params)
    }

    pub fn set_store_password_choice(
        &self,
        password: PasswordChoice<'_>,
        context: &[&str],
        params: KdfParams,
    ) -> LifecycleResult<PasswordChangeResult> {
        let _gate = self.lifecycle.lock().map_err(|_| StoreError::Busy)?;
        self.ensure_open()?;
        if disk_mode(&self.paths)? != StoreMode::Plain {
            return Err(StoreError::Conflict.into());
        }
        if !password.assess(context).allowed() {
            return Err(KeyError::WeakPassword.into());
        }
        params.validate_for_write()?;
        let new_key = LockedKey::random()?;
        self.export_swap_locked(
            Some(new_key),
            Some((password, context, params)),
            "password_set",
        )?;
        Ok(PasswordChangeResult {
            mode: StoreMode::Password,
            backup_pending: false,
            synchronization_pending: false,
            notice_pending: false,
        })
    }

    pub fn change_store_password(
        &self,
        current: &Password,
        new: &Password,
        context: &[&str],
        params: KdfParams,
        reencrypt: bool,
    ) -> LifecycleResult<PasswordChangeResult> {
        self.change_store_password_choice(
            current,
            PasswordChoice::Typed(new),
            context,
            params,
            reencrypt,
        )
    }

    pub fn change_store_password_choice(
        &self,
        current: &Password,
        new: PasswordChoice<'_>,
        context: &[&str],
        params: KdfParams,
        reencrypt: bool,
    ) -> LifecycleResult<PasswordChangeResult> {
        let _gate = self.lifecycle.lock().map_err(|_| StoreError::Busy)?;
        self.ensure_open()?;
        if disk_mode(&self.paths)? != StoreMode::Password {
            return Err(StoreError::Conflict.into());
        }
        if !new.assess(context).allowed() {
            return Err(KeyError::WeakPassword.into());
        }
        params.validate_for_write()?;
        let salt = database_salt(&self.paths.database())?;
        let pair = KeyFilePair::load(&self.paths.db(), &salt)?;
        // Explicit password operations require the authoritative current slot;
        // the previous-password unlock escape hatch does not authorize them.
        let authenticated = pair.primary().unlock(current)?;
        if reencrypt {
            self.export_swap_locked(
                Some(LockedKey::random()?),
                Some((new, context, params)),
                "password_rotated",
            )?;
            Ok(PasswordChangeResult {
                mode: StoreMode::Password,
                backup_pending: false,
                synchronization_pending: false,
                notice_pending: false,
            })
        } else {
            let replacement = new.create_slot(context, &authenticated, salt, params)?;
            let outcome = KeyFilePair::write(&self.paths.db(), &replacement)?;
            // Do not reconcile the old in-memory pair after the commit: it is
            // authoritative only until replacement keys.json is installed.
            let notice_pending = self.write_notice_locked("password_changed").is_err();
            Ok(PasswordChangeResult {
                mode: StoreMode::Password,
                backup_pending: outcome.backup_pending,
                synchronization_pending: outcome.synchronization_pending,
                notice_pending,
            })
        }
    }

    pub fn remove_store_password(
        &self,
        current: &Password,
    ) -> LifecycleResult<PasswordChangeResult> {
        let _gate = self.lifecycle.lock().map_err(|_| StoreError::Busy)?;
        self.ensure_open()?;
        let pair = KeyFilePair::load(&self.paths.db(), &database_salt(&self.paths.database())?)?;
        let _authenticated = pair.primary().unlock(current)?;
        self.export_swap_locked(None, None, "password_removed")?;
        Ok(PasswordChangeResult {
            mode: StoreMode::Plain,
            backup_pending: false,
            synchronization_pending: false,
            notice_pending: false,
        })
    }

    /// No password is required; only the explicitly named new generation can
    /// be retired. Previous application directories are never inspected.
    pub fn erase_local_data(&self, confirmation: &str) -> LifecycleResult<LockStatus> {
        if confirmation != ERASE_CONFIRMATION {
            return Err(LifecycleError::Confirmation);
        }
        let _gate = self.lifecycle.lock().map_err(|_| StoreError::Busy)?;
        self.begin_reset_locked(ResetAfter::Erase)?;
        self.lock_status()
    }

    pub fn start_fresh(&self, confirmation: &str) -> LifecycleResult<LockStatus> {
        if confirmation != START_FRESH_CONFIRMATION {
            return Err(LifecycleError::Confirmation);
        }
        let _gate = self.lifecycle.lock().map_err(|_| StoreError::Busy)?;
        self.ensure_not_refused()?;
        if !matches!(self.state(), StoreState::Damaged { .. }) {
            return Err(StoreError::Conflict.into());
        }
        self.begin_reset_locked(ResetAfter::StartFresh)?;
        self.lock_status()
    }

    /// None is an explicit "continue without a password" choice, not fallback.
    pub fn setup_store(
        &self,
        password: Option<&Password>,
        context: &[&str],
        params: KdfParams,
    ) -> LifecycleResult<LockStatus> {
        self.setup_store_choice(password.map(PasswordChoice::Typed), context, params)
    }

    pub fn setup_store_choice(
        &self,
        password: Option<PasswordChoice<'_>>,
        context: &[&str],
        params: KdfParams,
    ) -> LifecycleResult<LockStatus> {
        let _gate = self.lifecycle.lock().map_err(|_| StoreError::Busy)?;
        self.ensure_not_refused()?;
        if unclassified::pending(&self.paths)?.is_some() {
            if let Some(journal) = unclassified::recover(&self.paths)? {
                if let Some(password) = password {
                    if !password.assess(context).allowed() {
                        return Err(KeyError::WeakPassword.into());
                    }
                    params.validate_for_write()?;
                }
                self.complete_unclassified_locked(&journal, password, context, params, None)?;
            } else {
                if self.state() != StoreState::Open {
                    self.reopen_after_unclassified()?;
                }
                return self.acknowledge_completed_setup_locked(password);
            }
            return self.lock_status();
        }
        let pending = recover_filesystem(&self.paths)?;
        let Some(journal) = pending else {
            // A repeated setup after durable retirement is idempotent and may
            // not discard any work already written to the installed generation.
            return self.acknowledge_completed_setup_locked(password);
        };
        if journal.phase == SetupPhase::BuildReplacement {
            return Err(StoreError::Damaged {
                reason: DamageReason::Database,
            }
            .into());
        }
        if let Some(password) = password {
            if !password.assess(context).allowed() {
                return Err(KeyError::WeakPassword.into());
            }
            params.validate_for_write()?;
        }
        self.complete_setup_locked(&journal, password, context, params, None)?;
        self.lock_status()
    }

    /// A retry of an already completed choice can only acknowledge the
    /// currently Open store. It never begins or resumes a setup operation.
    pub fn acknowledge_completed_setup(
        &self,
        password: Option<&Password>,
    ) -> LifecycleResult<LockStatus> {
        let _gate = self.lifecycle.lock().map_err(|_| StoreError::Busy)?;
        self.ensure_open()?;
        self.acknowledge_completed_setup_locked(password.map(PasswordChoice::Typed))
    }

    fn acknowledge_completed_setup_locked(
        &self,
        password: Option<PasswordChoice<'_>>,
    ) -> LifecycleResult<LockStatus> {
        match (disk_mode(&self.paths)?, password) {
            (StoreMode::Plain, None) => {
                if self.state() != StoreState::Open {
                    self.reopen_locked(None)?;
                }
            }
            (StoreMode::Password, Some(password)) => {
                self.ensure_open()?;
                let expected =
                    read_generation(&self.paths.db())?.ok_or(LifecycleError::Generation)?;
                {
                    let guard = self.writer.lock().map_err(|_| StoreError::Busy)?;
                    let writer = guard.as_ref().ok_or(StoreError::Busy)?;
                    if connection_generation(writer)? != expected {
                        return Err(LifecycleError::Generation);
                    }
                }
                // A retry proves the chosen password against the current
                // primary only. Loading a pair would reconcile files even on
                // rejection, and an older backup must not authorize this ack.
                let salt = database_salt(&self.paths.database())?;
                let primary = keys::read_key_file(&self.paths.keys(), &salt)?;
                let key = primary.unlock(password.password())?;
                // Opening this exact source read-only proves the unwrapped DEK
                // belongs to the generation already admitted by the live store.
                let source = salvage::open_readonly_source(&self.paths.database(), Some(&key))?;
                if connection_generation(&source)? != expected {
                    return Err(LifecycleError::Generation);
                }
                source
                    .close()
                    .map_err(|(_, error)| StoreError::from(error))?;
            }
            _ => return Err(StoreError::Conflict.into()),
        }
        self.lock_status()
    }

    pub(super) fn ensure_generation_metadata(&self) -> StoreResult<()> {
        self.verify_or_create_generation().map_err(startup_error)
    }

    fn verify_or_create_generation(&self) -> LifecycleResult<()> {
        let guard = self.writer.lock().map_err(|_| StoreError::Busy)?;
        let connection = guard.as_ref().ok_or(StoreError::Busy)?;
        let expected = connection_generation(connection)?;
        if let Some(actual) = read_generation(&self.paths.db())? {
            if actual != expected {
                return Err(LifecycleError::Generation);
            }
        } else {
            write_json(&self.paths.generation(), &expected)?;
        }
        Ok(())
    }

    fn write_notice_locked(&self, kind: &str) -> LifecycleResult<()> {
        let value = notice::new_value(kind)?;
        let guard = self.writer.lock().map_err(|_| StoreError::Busy)?;
        let connection = guard.as_ref().ok_or(StoreError::Busy)?;
        connection.execute("INSERT INTO app_settings(key,value,updated_ms) VALUES ('store.mode_notice',?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_ms=excluded.updated_ms", params![value,super::now_ms()])?;
        Ok(())
    }

    fn export_swap_locked(
        &self,
        new_key: Option<LockedKey>,
        password: Option<(PasswordChoice<'_>, &[&str], KdfParams)>,
        notice: &str,
    ) -> LifecycleResult<()> {
        self.export_swap_with_commit_sync_locked(new_key, password, notice, sync_directory)
    }

    fn export_swap_with_commit_sync_locked(
        &self,
        new_key: Option<LockedKey>,
        password: Option<(PasswordChoice<'_>, &[&str], KdfParams)>,
        notice: &str,
        commit_sync: fn(&Path) -> io::Result<()>,
    ) -> LifecycleResult<()> {
        if let Err(error) = self.verify_or_create_generation() {
            let _ = self.close_connections();
            self.record_initialization_error(&startup_error(error.clone()));
            return Err(error);
        }
        let generation = read_generation(&self.paths.db())?.ok_or(LifecycleError::Generation)?;
        verify_generation_contents(&self.paths.db())?;
        let total = source_size(&self.paths)?;
        if free_bytes(self.paths.root())? < total.saturating_add(16 * 1024 * 1024) {
            return Err(LifecycleError::NotEnoughSpace);
        }
        self.lifecycle_control.begin();
        let _operation = ActiveOperation(&self.lifecycle_control);
        set_state(self, StoreState::Maintenance)?;
        // A held external WAL reader is an operational refusal. Keep the
        // original native handles/key and restore Open until it can be retried.
        // Only successful native close transfers the locked DEK into this scope.
        self.close_for_export()?;
        let old_key = self.dek.lock().map_err(|_| StoreError::Busy)?.take();
        let temporary = self.paths.root().join("db.tmp");
        let mut committed = false;
        let result = (|| -> LifecycleResult<()> {
            self.lifecycle_control.check_cancelled()?;
            if temporary.try_exists()?
                || self.paths.root().join("db.new").try_exists()?
                || self.paths.root().join("db.old").try_exists()?
            {
                return Err(LifecycleError::Generation);
            }
            private_directory(&temporary)?;
            write_json(&temporary.join("generation.json"), &generation)?;
            let destination = temporary.join("maple.db");
            let file = private_new_file(&destination)?;
            keys::sync_key_file(&file)?;
            drop(file);
            let source = open_direct(&self.paths.database(), old_key.as_ref())?;
            if !connection::checkpoint(&source, true)? {
                return Err(StoreError::Busy.into());
            }
            self.lifecycle_control
                .update(OperationPhase::Exporting, 0, total, None, true);
            install_progress_handler(&source, destination.clone(), total, &self.lifecycle_control)?;
            let path = destination.to_str().ok_or(StoreError::InvalidInput)?;
            let attached = if let Some(key) = &new_key {
                // The native helper binds the locked raw key STATIC and
                // finalizes before that mapping is wiped. Ordinary rusqlite
                // text bindings would leave a transient native heap copy.
                engine::attach_keyed(&source, path, key)?;
                true
            } else {
                source.execute("ATTACH DATABASE ?1 AS dst KEY ?2", params![path, ""])?;
                true
            };
            let exported = (|| -> LifecycleResult<()> {
                source.execute_batch("PRAGMA dst.auto_vacuum=INCREMENTAL; PRAGMA dst.synchronous=FULL; PRAGMA fullfsync=ON;")?;
                source.query_row("SELECT sqlcipher_export('dst')", [], |_| Ok(()))?;
                source.pragma_update(Some("dst"), "user_version", schema::VERSION)?;
                source.pragma_update(Some("dst"), "application_id", schema::APPLICATION_ID)?;
                let value = notice::new_value(notice)?;
                source.execute("INSERT INTO dst.app_settings(key,value,updated_ms) VALUES ('store.mode_notice',?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_ms=excluded.updated_ms", params![value,super::now_ms()])?;
                for table in ["tasks", "entries", "file_attachments"] {
                    let count: i64 = source.query_row(
                        &format!("SELECT count(*) FROM main.{table}"),
                        [],
                        |r| r.get(0),
                    )?;
                    let copied: i64 = source.query_row(
                        &format!("SELECT count(*) FROM dst.{table}"),
                        [],
                        |r| r.get(0),
                    )?;
                    if count != copied {
                        return Err(StoreError::InvalidData.into());
                    }
                }
                Ok(())
            })();
            source
                .progress_handler(0, None::<fn() -> bool>)
                .map_err(StoreError::from)?;
            if attached {
                let _ = source.execute_batch("DETACH DATABASE dst");
            }
            drop(source);
            if self.lifecycle_control.cancelled.load(Ordering::Acquire) {
                return Err(LifecycleError::Cancelled);
            }
            exported?;
            self.lifecycle_control
                .update(OperationPhase::Verifying, total, total, None, true);
            verify_destination(
                &self.paths,
                &destination,
                new_key.as_ref(),
                Some(&self.lifecycle_control),
            )?;
            if let Some((password, context, params)) = password {
                let key = new_key.as_ref().ok_or(StoreError::InvalidInput)?;
                let slot =
                    password.create_slot(context, key, database_salt(&destination)?, params)?;
                if KeyFilePair::write(&temporary, &slot)?.backup_pending {
                    return Err(io::Error::from(io::ErrorKind::Other).into());
                }
            }
            synchronize_database_directory(&temporary)?;
            self.lifecycle_control.check_cancelled()?;
            self.lifecycle_control
                .update(OperationPhase::Committing, total, total, None, false);
            self.lifecycle_control.check_cancelled()?;
            let committed_path = self.paths.root().join("db.new");
            verify_private_path(&temporary, true)?;
            if path_present(&committed_path)? {
                return Err(LifecycleError::Generation);
            }
            keys::replace_key_file(&temporary, &committed_path)?;
            // Visibility is already committed after rename. A failed directory
            // sync must never reopen the old store and admit newer writes.
            committed = true;
            commit_sync(self.paths.root())?;
            install_committed(&self.paths, &generation, Some(&generation.store_id))?;
            Ok(())
        })();
        if result.is_ok() {
            self.reopen_locked(new_key)?;
            self.lifecycle_control
                .update(OperationPhase::Complete, total, total, Some(0), false);
        } else if !committed {
            if temporary.try_exists()? {
                discard_temporary(&temporary, &generation)?;
            }
            self.reopen_locked(old_key)?;
        } else {
            set_state(self, StoreState::Maintenance)?;
            return Err(LifecycleError::RecoveryRequired {
                mode: if new_key.is_some() {
                    StoreMode::Password
                } else {
                    StoreMode::Plain
                },
            });
        }
        result
    }

    fn reopen_locked(&self, key: Option<LockedKey>) -> LifecycleResult<()> {
        let result = match &key {
            Some(key) => {
                self.install_connections(&|connection| engine::key_connection(connection, key))
            }
            None => self.install_connections(&engine::guard_plain),
        };
        if let Err(error) = result {
            let _ = self.close_connections();
            self.record_initialization_error(&error);
            return Err(error.into());
        }
        if let Err(error) = self.verify_or_create_generation() {
            let _ = self.close_connections();
            self.record_initialization_error(&startup_error(error.clone()));
            return Err(error);
        }
        *self.dek.lock().map_err(|_| StoreError::Busy)? = key;
        set_state(self, StoreState::Open)?;
        self.resume_replays();
        Ok(())
    }

    /// Exercise a visible reset journal whose final parent-directory sync fails.
    /// This test-only entrypoint retains the real confirmation and lifecycle gate.
    #[cfg(test)]
    pub(crate) fn erase_with_failed_intent_sync_for_test(
        &self,
        confirmation: &str,
    ) -> LifecycleResult<LockStatus> {
        if confirmation != ERASE_CONFIRMATION {
            return Err(LifecycleError::Confirmation);
        }
        let _gate = self.lifecycle.lock().map_err(|_| StoreError::Busy)?;
        self.begin_reset_with_intent_sync_locked(ResetAfter::Erase, |_| {
            Err(io::Error::from(io::ErrorKind::Other))
        })?;
        self.lock_status()
    }

    fn begin_reset_locked(&self, after: ResetAfter) -> LifecycleResult<()> {
        self.begin_reset_with_intent_sync_locked(after, sync_directory)
    }

    fn begin_reset_with_intent_sync_locked(
        &self,
        after: ResetAfter,
        intent_sync: fn(&Path) -> io::Result<()>,
    ) -> LifecycleResult<()> {
        self.ensure_not_refused()?;
        if unclassified::pending(&self.paths)?.is_some() {
            return self.begin_unclassified_reset_locked(after);
        }
        let pending =
            match read_operation_json_optional::<SetupJournal>(&self.paths.setup(), SETUP_FORMAT) {
                Ok(pending) => pending,
                Err(LifecycleError::Generation) => {
                    return self.supersede_malformed_setup_locked(after, intent_sync);
                }
                Err(error) => return Err(error),
            };
        if let Some(journal) = &pending {
            journal.validate()?;
        }
        if pending.is_none()
            && (matches!(
                self.state(),
                StoreState::Damaged {
                    reason: DamageReason::Generation
                }
            ) || !matches!(read_generation(&self.paths.db()), Ok(Some(_))))
        {
            let repaired = {
                let key = self.dek.lock().map_err(|_| StoreError::Busy)?;
                self.repair_generation_from_readonly(key.as_ref())
            };
            if repaired.is_err() {
                return self.begin_unclassified_reset_locked(after);
            }
        }
        if self.state() == StoreState::Open {
            self.verify_or_create_generation()?;
        }
        let mut journal = if pending.is_some() {
            freeze_attachment_writers(self, true)?;
            self.close_connections_without_checkpoint()?;
            match recover_filesystem(&self.paths)? {
                Some(journal) => journal,
                None => {
                    // This request retries an operation whose replacement was
                    // already installed. Retire its marker and preserve every
                    // later row; do not reserve/build another replacement.
                    match disk_mode(&self.paths)? {
                        StoreMode::Plain => self.reopen_locked(None)?,
                        StoreMode::Password => set_state(self, StoreState::Locked)?,
                    }
                    return Ok(());
                }
            }
        } else {
            let generation =
                read_generation(&self.paths.db())?.ok_or(LifecycleError::Generation)?;
            let journal = SetupJournal {
                format: SETUP_FORMAT.into(),
                operation_id: random_id()?,
                after,
                previous_mode: malformed_setup::conservative_reset_mode(&self.paths)?,
                old_store_id: generation.store_id,
                new_store_id: random_id()?,
                phase: SetupPhase::RetireOld,
            };
            // A deterministic refusal must not become a durable reset intent.
            // Recovery cannot resolve foreign files or a competing archive,
            // so publishing first would wedge an otherwise usable source.
            // Recheck after closing handles below before retiring any bytes.
            preflight_setup(&self.paths, &journal)?;
            // Publishing the reset intent can succeed before parent syncing
            // fails. Freeze admission first so no newer acknowledged writes can
            // be lost when startup finishes that confirmed reset later.
            freeze_attachment_writers(self, true)?;
            // An in-flight checkpoint can publish its own quarantine journal.
            // Join it before publishing reset intent so it cannot overwrite the
            // operation identity used to retire the confirmed source below.
            self.close_connections_without_checkpoint()?;
            preflight_setup(&self.paths, &journal)?;
            if write_json_with_sync(&self.paths.setup(), &journal, intent_sync).is_err() {
                return Err(LifecycleError::RecoveryRequired {
                    mode: journal.previous_mode,
                });
            }
            journal
        };
        if journal.phase == SetupPhase::BuildReplacement {
            // Explicit reset supersedes a quarantined/aborted salvage. Publish
            // that reset intent before retiring the original damaged directory.
            journal.after = after;
            journal.phase = SetupPhase::RetireOld;
            write_json(&self.paths.setup(), &journal)?;
        }
        set_state(self, StoreState::Maintenance)?;
        if !self.cancel_attachment_writers()? {
            return Err(StoreError::Busy.into());
        }
        self.close_connections_without_checkpoint()?;
        if journal.after == ResetAfter::Erase {
            self.lifecycle_control
                .repair_wrapping
                .lock()
                .map_err(|_| StoreError::Busy)?
                .take();
        }
        preflight_setup(&self.paths, &journal)?;
        retire_old(&self.paths, &mut journal)?;
        set_state(self, StoreState::NeedsSetup)?;
        if journal.previous_mode == StoreMode::Plain {
            self.complete_setup_locked(&journal, None, &[], KdfParams::floor(), None)?;
        } else if journal.after == ResetAfter::StartFresh {
            let wrapping = self
                .lifecycle_control
                .repair_wrapping
                .lock()
                .map_err(|_| StoreError::Busy)?
                .take();
            if let Some(wrapping) = wrapping {
                self.complete_setup_locked(
                    &journal,
                    None,
                    &[],
                    KdfParams::floor(),
                    Some(wrapping),
                )?;
            }
        }
        Ok(())
    }

    fn complete_setup_locked(
        &self,
        journal: &SetupJournal,
        password: Option<PasswordChoice<'_>>,
        context: &[&str],
        params: KdfParams,
        wrapping: Option<WrappingKey>,
    ) -> LifecycleResult<()> {
        let mut journal = journal.clone();
        journal.validate()?;
        let generation = journal.replacement();
        let key =
            self.build_empty_replacement_locked(&generation, password, context, params, wrapping)?;
        let temporary = self.paths.root().join("db.tmp");
        journal.phase = SetupPhase::InstallNew;
        write_json(&self.paths.setup(), &journal)?;
        durable_rename(
            &temporary,
            &self.paths.root().join("db.new"),
            self.paths.root(),
        )?;
        install_committed(&self.paths, &generation, Some(&journal.old_store_id))?;
        finish_setup(&self.paths, &mut journal)?;
        self.reopen_locked(key)?;
        Ok(())
    }

    fn build_empty_replacement_locked(
        &self,
        generation: &Generation,
        password: Option<PasswordChoice<'_>>,
        context: &[&str],
        params: KdfParams,
        wrapping: Option<WrappingKey>,
    ) -> LifecycleResult<Option<LockedKey>> {
        let temporary = self.paths.root().join("db.tmp");
        if temporary.try_exists()? {
            discard_temporary(&temporary, generation)?;
        }
        private_directory(&temporary)?;
        write_json(&temporary.join("generation.json"), generation)?;
        let key = if password.is_some() || wrapping.is_some() {
            Some(LockedKey::random()?)
        } else {
            None
        };
        let path = temporary.join("maple.db");
        drop(private_new_file(&path)?);
        let mut database = open_direct(&path, key.as_ref())?;
        database.pragma_update(None, "page_size", 4096)?;
        database.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        database.pragma_update(None, "application_id", schema::APPLICATION_ID)?;
        schema::migrate(&mut database, super::now_ms())?;
        let id = decode_id(&generation.store_id).ok_or(LifecycleError::Generation)?;
        let operation = generation
            .setup_operation_id
            .as_deref()
            .and_then(decode_id)
            .ok_or(LifecycleError::Generation)?;
        let transaction = database.transaction()?;
        transaction.execute(
            "UPDATE store_meta SET value=?1 WHERE key='store_id'",
            [id.as_slice()],
        )?;
        transaction.execute(
            "INSERT INTO store_meta(key,value) VALUES ('setup_operation_id',?1)",
            [operation.as_slice()],
        )?;
        transaction.commit()?;
        database
            .close()
            .map_err(|(_, error)| StoreError::from(error))?;
        verify_destination(&self.paths, &path, key.as_ref(), None)?;
        if let Some(key) = &key {
            let salt = database_salt(&path)?;
            let slot = match wrapping {
                Some(wrapping) => wrapping.wrap(key, salt)?,
                None => password
                    .ok_or(StoreError::InvalidInput)?
                    .create_slot(context, key, salt, params)?,
            };
            if KeyFilePair::write(&temporary, &slot)?.backup_pending {
                return Err(io::Error::from(io::ErrorKind::Other).into());
            }
        }
        synchronize_database_directory(&temporary)?;
        Ok(key)
    }
}

fn connection_generation(connection: &Connection) -> LifecycleResult<Generation> {
    let id: Vec<u8> = connection.query_row(
        "SELECT value FROM store_meta WHERE key='store_id'",
        [],
        |row| row.get(0),
    )?;
    let id: [u8; 16] = id.try_into().map_err(|_| LifecycleError::Generation)?;
    let operation: Option<Vec<u8>> = connection
        .query_row(
            "SELECT value FROM store_meta WHERE key='setup_operation_id'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let operation = operation
        .map(|bytes| <[u8; 16]>::try_from(bytes).map(|id| encode_id(&id)))
        .transpose()
        .map_err(|_| LifecycleError::Generation)?;
    Ok(Generation::new(id, operation))
}

fn set_state(store: &Store, state: StoreState) -> StoreResult<()> {
    *store.state.write().map_err(|_| StoreError::Busy)? = state;
    Ok(())
}

/// Freeze new upload admission before cancelling existing writers. A refusal
/// before durable replacement intent leaves the original store usable while
/// those already-cancelled handles drain; it must not strand the UI in upkeep.
fn freeze_attachment_writers(store: &Store, restore_on_busy: bool) -> StoreResult<()> {
    let previous = {
        let mut state = store.state.write().map_err(|_| StoreError::Busy)?;
        let previous = state.clone();
        *state = StoreState::Maintenance;
        previous
    };
    if store.cancel_attachment_writers()? {
        return Ok(());
    }
    if restore_on_busy {
        let mut state = store.state.write().map_err(|_| StoreError::Busy)?;
        if *state == StoreState::Maintenance {
            // A checkpoint failure can race the request without taking the
            // lifecycle gate. Never revive Open after damage was observed.
            *state = if previous == StoreState::Open && store.runtime_damage.load(Ordering::Acquire)
            {
                StoreState::Damaged {
                    reason: DamageReason::Database,
                }
            } else {
                previous
            };
        }
    }
    Err(StoreError::Busy)
}

fn disk_mode(paths: &StorePaths) -> LifecycleResult<StoreMode> {
    if path_present(&paths.keys())? || path_present(&paths.db().join("keys.backup.json"))? {
        return Ok(StoreMode::Password);
    }
    match fs::symlink_metadata(paths.database()) {
        Ok(metadata) => {
            verify_private_path(&paths.database(), false)?;
            if metadata.len() < 16 {
                return Ok(
                    if metadata.len() == 0 && read_generation(&paths.db())?.is_none() {
                        StoreMode::Plain
                    } else {
                        StoreMode::Password
                    },
                );
            }
            let salt = database_salt(&paths.database())?;
            Ok(if &salt == b"SQLite format 3\0" {
                StoreMode::Plain
            } else {
                StoreMode::Password
            })
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if read_generation(&paths.db())?.is_some() {
                Err(LifecycleError::Generation)
            } else {
                Ok(StoreMode::Plain)
            }
        }
        Err(error) => Err(error.into()),
    }
}

fn path_present(path: &Path) -> LifecycleResult<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn database_salt(path: &Path) -> LifecycleResult<[u8; 16]> {
    verify_private_path(path, false)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut header = [0_u8; 16];
    options.open(path)?.read_exact(&mut header)?;
    Ok(header)
}

fn open_direct(path: &Path, key: Option<&LockedKey>) -> StoreResult<Connection> {
    verify_private_path(path, false)?;
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    match key {
        Some(key) => engine::key_connection(&connection, key)?,
        None => engine::guard_plain(&connection)?,
    }
    connection.pragma_update(None, "synchronous", "FULL")?;
    connection.pragma_update(None, "fullfsync", true)?;
    connection.pragma_update(None, "checkpoint_fullfsync", true)?;
    connection.pragma_update(None, "temp_store", "MEMORY")?;
    connection.pragma_update(None, "secure_delete", true)?;
    Ok(connection)
}

fn verify_destination(
    paths: &StorePaths,
    path: &Path,
    key: Option<&LockedKey>,
    control: Option<&LifecycleControl>,
) -> LifecycleResult<()> {
    let generation = read_generation(path.parent().ok_or(StoreError::UnsafePath)?)?
        .ok_or(LifecycleError::Generation)?;
    // SQLCipher's native HMAC scan can run during PREPARE, outside SQLite VM
    // progress callbacks. A qualified read-only child gives cancellation an
    // enforceable kill/reap boundary. The independent one-hour ceiling permits
    // large replacements; weekly checks have their own shorter budget.
    let should_cancel = || control.is_some_and(|control| control.cancelled.load(Ordering::Acquire));
    let report = super::integrity::check_staged(
        paths,
        key,
        &generation.store_id,
        generation.setup_operation_id.as_deref(),
        std::time::Duration::from_secs(60 * 60),
        should_cancel,
    )?;
    if should_cancel() {
        return Err(LifecycleError::Cancelled);
    }
    if report.outcome == super::integrity::IntegrityOutcome::Damaged {
        return Err(StoreError::Damaged {
            reason: DamageReason::Database,
        }
        .into());
    }
    if !report.complete || report.outcome != super::integrity::IntegrityOutcome::Clean {
        return Err(LifecycleError::VerificationIncomplete);
    }
    let connection = open_direct(path, key)?;
    let mode: String = connection.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(StoreError::InvalidData.into());
    }
    connection
        .close()
        .map_err(|(_, error)| StoreError::from(error))?;
    // Finish the journal-mode transition before checkpointing. SQLite retains
    // a lock on this freshly converted handle even after the PRAGMA statement
    // finishes; a new handle observes the completed WAL mode without that lock.
    let connection = open_direct(path, key)?;
    if !connection::checkpoint(&connection, true)? {
        return Err(StoreError::Busy.into());
    }
    connection
        .close()
        .map_err(|(_, error)| StoreError::from(error))?;
    Ok(())
}

fn install_progress_handler(
    connection: &Connection,
    destination: PathBuf,
    total: u64,
    control: &LifecycleControl,
) -> StoreResult<()> {
    let cancel = Arc::clone(&control.cancelled);
    let progress = Arc::clone(&control.progress);
    let start = Instant::now();
    let mut last = start;
    connection
        .progress_handler(
            2000,
            Some(move || {
                if cancel.load(Ordering::Acquire) {
                    return true;
                }
                if last.elapsed().as_millis() >= 100 {
                    last = Instant::now();
                    let done = fs::metadata(&destination)
                        .map_or(0, |metadata| metadata.len())
                        .min(total);
                    let elapsed = start.elapsed();
                    let remaining = if done > 0 && elapsed.as_secs() >= 1 {
                        Some(
                            ((total.saturating_sub(done) as u128 * elapsed.as_millis())
                                / u128::from(done))
                            .min(u128::from(u64::MAX)) as u64,
                        )
                    } else {
                        None
                    };
                    if let Ok(mut value) = progress.lock() {
                        value.completed_bytes = done;
                        value.remaining_ms = remaining;
                    }
                }
                false
            }),
        )
        .map_err(StoreError::from)
}

fn recover_filesystem(paths: &StorePaths) -> LifecycleResult<Option<SetupJournal>> {
    verify_private_path(paths.root(), true)?;
    if let Some(mut journal) =
        read_operation_json_optional::<SetupJournal>(&paths.setup(), SETUP_FORMAT)?
    {
        journal.validate()?;
        preflight_setup(paths, &journal)?;
        let replacement = journal.replacement();
        if read_generation(&paths.db())?.as_ref() == Some(&replacement) {
            finish_setup(paths, &mut journal)?;
            return Ok(None);
        }
        if journal.phase == SetupPhase::BuildReplacement {
            let temporary = paths.root().join("db.tmp");
            if path_present(&temporary)? {
                discard_temporary(&temporary, &replacement)?;
            }
            attachments::remove_generation(paths, &journal.new_store_id)?;
            // Keep this generation-qualified marker: a failed salvage does not
            // restore trust in the original database on the following restart.
            return Ok(Some(journal));
        }
        if journal.phase == SetupPhase::Installed {
            return Err(LifecycleError::Generation);
        }
        retire_old(paths, &mut journal)?;
        let new = paths.root().join("db.new");
        let temporary = paths.root().join("db.tmp");
        if new.try_exists()? {
            if read_generation(&new)?.as_ref() != Some(&replacement) {
                return Err(LifecycleError::Generation);
            }
            install_committed(paths, &replacement, Some(&journal.old_store_id))?;
            finish_setup(paths, &mut journal)?;
            return Ok(None);
        }
        if journal.phase == SetupPhase::InstallNew {
            if read_generation(&temporary)?.as_ref() != Some(&replacement) {
                return Err(LifecycleError::Generation);
            }
            durable_rename(&temporary, &new, paths.root())?;
            install_committed(paths, &replacement, Some(&journal.old_store_id))?;
            finish_setup(paths, &mut journal)?;
            return Ok(None);
        }
        if temporary.try_exists()? {
            discard_temporary(&temporary, &replacement)?;
        }
        return Ok(Some(journal));
    }
    let current = read_generation(&paths.db())?;
    let temporary = paths.root().join("db.tmp");
    if temporary.try_exists()? {
        let expected = current.as_ref().ok_or(LifecycleError::Generation)?;
        discard_temporary(&temporary, expected)?;
    }
    let new = paths.root().join("db.new");
    if new.try_exists()? {
        let generation = read_generation(&new)?.ok_or(LifecycleError::Generation)?;
        install_committed(paths, &generation, Some(&generation.store_id))?;
    } else {
        let old = paths.root().join("db.old");
        if old.try_exists()? {
            let generation = read_generation(&paths.db())?.ok_or(LifecycleError::Generation)?;
            remove_generation_directory(&old, Some(&generation))?;
            sync_directory(paths.root())?;
        }
    }
    Ok(None)
}

fn preflight_setup(paths: &StorePaths, journal: &SetupJournal) -> LifecycleResult<()> {
    journal.validate()?;
    attachments::verify_generation(paths, &journal.old_store_id)?;
    let replacement = journal.replacement();
    let current = read_generation(&paths.db())?;
    if journal.phase == SetupPhase::BuildReplacement
        && current.as_ref() != Some(&replacement)
        && (current
            .as_ref()
            .map(|generation| generation.store_id.as_str())
            != Some(journal.old_store_id.as_str())
            || path_present(&journal.retired_source(paths))?
            || path_present(&paths.root().join("db.new"))?)
    {
        return Err(LifecycleError::Generation);
    }
    if let Some(current) = &current {
        if current.store_id != journal.old_store_id && current != &replacement {
            return Err(LifecycleError::Generation);
        }
        verify_generation_contents(&paths.db())?;
    } else if path_present(&paths.db())? && !directory_empty(&paths.db())? {
        return Err(LifecycleError::Generation);
    }
    let archive = journal.retired_source(paths);
    if path_present(&archive)? {
        if directory_empty(&archive)? {
            // An empty archive is the last resumable step of deletion.
        } else {
            let archived = read_generation(&archive)?.ok_or(LifecycleError::Generation)?;
            if archived.store_id != journal.old_store_id {
                return Err(LifecycleError::Generation);
            }
            verify_generation_contents(&archive)?;
        }
        if current
            .as_ref()
            .is_some_and(|value| value.store_id == journal.old_store_id)
        {
            return Err(LifecycleError::Generation);
        }
    }
    let new = paths.root().join("db.new");
    if path_present(&new)? {
        if read_generation(&new)?.as_ref() != Some(&replacement)
            || current.as_ref() == Some(&replacement)
        {
            return Err(LifecycleError::Generation);
        }
        verify_generation_contents(&new)?;
        verify_private_path(&new.join("maple.db"), false)?;
        if fs::metadata(new.join("maple.db"))?.len() < 16 {
            return Err(LifecycleError::Generation);
        }
    }
    let temporary = paths.root().join("db.tmp");
    if path_present(&temporary)? {
        if current.as_ref() == Some(&replacement) {
            // The operation's temporary directory was renamed during install.
            // A new temporary beside its installed replacement is not ours.
            return Err(LifecycleError::Generation);
        }
        match read_generation(&temporary)? {
            Some(generation) => {
                if generation != replacement {
                    return Err(LifecycleError::Generation);
                }
                verify_generation_contents(&temporary)?;
            }
            None => {
                metadata_only_temporary_files(&temporary)?;
            }
        }
    }
    if journal.phase == SetupPhase::InstallNew
        && current.as_ref() != Some(&replacement)
        && !path_present(&new)?
    {
        if read_generation(&temporary)?.as_ref() != Some(&replacement) {
            return Err(LifecycleError::Generation);
        }
        verify_private_path(&temporary.join("maple.db"), false)?;
        if fs::metadata(temporary.join("maple.db"))?.len() < 16 {
            return Err(LifecycleError::Generation);
        }
    }
    Ok(())
}

fn retire_old(paths: &StorePaths, journal: &mut SetupJournal) -> LifecycleResult<()> {
    journal.validate()?;
    let current = read_generation(&paths.db())?;
    if current.as_ref() == Some(&journal.replacement()) {
        return Ok(());
    }
    let retired = journal.retired_source(paths);
    if let Some(generation) = current {
        if generation.store_id != journal.old_store_id || path_present(&retired)? {
            return Err(LifecycleError::Generation);
        }
        durable_rename(&paths.db(), &retired, paths.root())?;
    } else if path_present(&paths.db())? {
        // prepare() may recreate an empty placeholder between the two renames.
        if !directory_empty(&paths.db())? {
            return Err(LifecycleError::Generation);
        }
        fs::remove_dir(paths.db())?;
        sync_directory(paths.root())?;
    }
    if journal.phase != SetupPhase::InstallNew {
        // A confirmed reset discards its current source before asking for new
        // setup choices. A committed salvage keeps its temporary source until
        // the verified replacement has been installed.
        attachments::remove_generation(paths, &journal.old_store_id)?;
        remove_retired_source(paths, &journal.old_store_id)?;
    }
    if journal.phase == SetupPhase::RetireOld {
        journal.phase = SetupPhase::AwaitChoice;
        write_json(&paths.setup(), journal)?;
    }
    Ok(())
}

fn remove_retired_source(paths: &StorePaths, old_id: &str) -> LifecycleResult<()> {
    let retired = paths.root().join("db.old");
    if !path_present(&retired)? {
        return Ok(());
    }
    let previous = read_generation(&retired)?;
    if let Some(previous) = &previous {
        if previous.store_id != old_id {
            return Err(LifecycleError::Generation);
        }
    } else if !directory_empty(&retired)? {
        return Err(LifecycleError::Generation);
    }
    remove_generation_directory(&retired, previous.as_ref())?;
    sync_directory(paths.root())?;
    Ok(())
}

fn install_committed(
    paths: &StorePaths,
    generation: &Generation,
    old_id: Option<&str>,
) -> LifecycleResult<()> {
    let new = paths.root().join("db.new");
    let old = paths.root().join("db.old");
    if read_generation(&new)?.as_ref() != Some(generation) {
        return Err(LifecycleError::Generation);
    }
    if paths.db().try_exists()? {
        if directory_empty(&paths.db())? {
            fs::remove_dir(paths.db())?;
        } else {
            let current = read_generation(&paths.db())?.ok_or(LifecycleError::Generation)?;
            if Some(current.store_id.as_str()) != old_id || old.try_exists()? {
                return Err(LifecycleError::Generation);
            }
            durable_rename(&paths.db(), &old, paths.root())?;
        }
    }
    durable_rename(&new, &paths.db(), paths.root())?;
    if old.try_exists()? {
        remove_retired_source(paths, old_id.ok_or(LifecycleError::Generation)?)?;
    }
    Ok(())
}

fn finish_setup(paths: &StorePaths, journal: &mut SetupJournal) -> LifecycleResult<()> {
    if read_generation(&paths.db())?.as_ref() != Some(&journal.replacement()) {
        return Err(LifecycleError::Generation);
    }
    journal.validate()?;
    // A crash may have installed the replacement before source cleanup. Its
    // journal stays authoritative until the exact old source is gone.
    attachments::remove_generation(paths, &journal.old_store_id)?;
    remove_retired_source(paths, &journal.old_store_id)?;
    journal.phase = SetupPhase::Installed;
    write_json(&paths.setup(), journal)?;
    verify_private_path(&paths.setup(), false)?;
    fs::remove_file(paths.setup())?;
    sync_directory(paths.root())?;
    Ok(())
}

fn read_generation(directory: &Path) -> LifecycleResult<Option<Generation>> {
    match fs::symlink_metadata(directory) {
        Ok(_) => verify_private_path(directory, true)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let generation: Option<Generation> = read_json_optional(&directory.join("generation.json"))?;
    if let Some(value) = &generation {
        value.validate()?;
    }
    Ok(generation)
}

/// The read-only checker may inspect only this exact owned staging directory,
/// bound either to the current export generation or to the active setup/salvage
/// operation's reserved replacement. It accepts no arbitrary database path.
pub(in crate::store) fn validate_staged_checker(
    paths: &StorePaths,
    store_id: &str,
    setup_operation_id: Option<&str>,
) -> StoreResult<PathBuf> {
    (|| -> LifecycleResult<PathBuf> {
        verify_private_path(paths.root(), true)?;
        let expected = Generation {
            format: GENERATION_FORMAT.into(),
            store_id: store_id.to_owned(),
            setup_operation_id: setup_operation_id.map(str::to_owned),
        };
        expected.validate()?;
        let temporary = paths.root().join("db.tmp");
        if read_generation(&temporary)?.as_ref() != Some(&expected) {
            return Err(LifecycleError::Generation);
        }
        verify_generation_contents(&temporary)?;
        verify_private_path(&temporary.join("maple.db"), false)?;
        if fs::symlink_metadata(temporary.join("maple.db"))?.len() < 16 {
            return Err(LifecycleError::Generation);
        }
        if unclassified::validate_staged(paths, &expected)? {
            return Ok(temporary.join("maple.db"));
        }
        match read_operation_json_optional::<SetupJournal>(&paths.setup(), SETUP_FORMAT)? {
            Some(journal) => {
                journal.validate()?;
                if journal.replacement() != expected || journal.phase == SetupPhase::Installed {
                    return Err(LifecycleError::Generation);
                }
                preflight_setup(paths, &journal)?;
            }
            None => {
                if read_generation(&paths.db())?.as_ref() != Some(&expected)
                    || path_present(&paths.root().join("db.new"))?
                    || path_present(&paths.root().join("db.old"))?
                {
                    return Err(LifecycleError::Generation);
                }
                verify_generation_contents(&paths.db())?;
            }
        }
        Ok(temporary.join("maple.db"))
    })()
    .map_err(startup_error)
}

fn remove_generation_directory(
    directory: &Path,
    expected: Option<&Generation>,
) -> LifecycleResult<()> {
    verify_private_path(directory, true)?;
    let actual = read_generation(directory)?;
    if actual.is_none() && directory_empty(directory)? {
        fs::remove_dir(directory)?;
        sync_directory(directory.parent().ok_or(StoreError::UnsafePath)?)?;
        return Ok(());
    }
    if actual.is_none() || actual.as_ref() != expected {
        return Err(LifecycleError::Generation);
    }
    // Refuse symlinks, foreign files, nested directories and hard links before
    // deleting anything. A compromised path cannot redirect recursive cleanup.
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_str().ok_or(LifecycleError::Generation)?;
        if !unclassified_identity::allowed_name(name) {
            return Err(LifecycleError::Generation);
        }
        verify_private_path(&entry.path(), false)?;
    }
    let files = fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<io::Result<Vec<_>>>()?;
    // Retain identity until every data file has been removed. A crash either
    // leaves a recognizable generation or an empty directory, both resumable.
    for file in files {
        if file
            .file_name()
            .is_some_and(|name| name != "generation.json")
        {
            fs::remove_file(file)?;
        }
    }
    sync_directory(directory)?;
    fs::remove_file(directory.join("generation.json"))?;
    sync_directory(directory)?;
    fs::remove_dir(directory)?;
    sync_directory(directory.parent().ok_or(StoreError::UnsafePath)?)?;
    Ok(())
}

fn discard_temporary(directory: &Path, expected: &Generation) -> LifecycleResult<()> {
    if read_generation(directory)?.is_some() {
        return remove_generation_directory(directory, Some(expected));
    }
    let files = metadata_only_temporary_files(directory)?;
    for file in files {
        fs::remove_file(file)?;
    }
    fs::remove_dir(directory)?;
    sync_directory(directory.parent().ok_or(StoreError::UnsafePath)?)?;
    Ok(())
}

fn metadata_only_temporary_files(directory: &Path) -> LifecycleResult<Vec<PathBuf>> {
    verify_private_path(directory, true)?;
    // The generation is written before creating any database bytes. A crash
    // during that first atomic metadata write can leave only its private temp.
    // A missing identity beside any database/content file remains an error.
    let mut files = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_str().ok_or(LifecycleError::Generation)?;
        let id = name
            .strip_prefix(".control-")
            .and_then(|name| name.strip_suffix(".tmp"));
        if id.and_then(decode_id).is_none() {
            return Err(LifecycleError::Generation);
        }
        verify_private_path(&entry.path(), false)?;
        files.push(entry.path());
    }
    Ok(files)
}

fn verify_generation_contents(directory: &Path) -> LifecycleResult<()> {
    verify_private_path(directory, true)?;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_str().ok_or(LifecycleError::Generation)?;
        if !unclassified_identity::allowed_name(name) {
            return Err(LifecycleError::Generation);
        }
        verify_private_path(&entry.path(), false)?;
    }
    Ok(())
}

fn directory_empty(directory: &Path) -> LifecycleResult<bool> {
    verify_private_path(directory, true)?;
    Ok(fs::read_dir(directory)?.next().is_none())
}

fn read_json_optional<T: DeserializeOwned>(path: &Path) -> LifecycleResult<Option<T>> {
    match fs::symlink_metadata(path) {
        Ok(_) => verify_private_path(path, false)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut bytes = Vec::new();
    options
        .open(path)?
        .take(16 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 16 * 1024 {
        return Err(LifecycleError::Generation);
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| LifecycleError::Generation)
}

/// Decode an operation's format before its changing payload. Valid older or
/// unknown journals are refusal states, never malformed-reset authorization.
fn read_operation_json_optional<T: DeserializeOwned>(
    path: &Path,
    expected_format: &str,
) -> LifecycleResult<Option<T>> {
    let Some(value) = read_json_optional::<serde_json::Value>(path)? else {
        return Ok(None);
    };
    if value.get("format").and_then(serde_json::Value::as_str) != Some(expected_format) {
        return Err(StoreError::UnsupportedOperationFormat.into());
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(|_| LifecycleError::Generation)
}

fn write_json(path: &Path, value: &impl Serialize) -> LifecycleResult<()> {
    write_json_with_sync(path, value, sync_directory)
}

fn write_json_with_sync(
    path: &Path,
    value: &impl Serialize,
    parent_sync: fn(&Path) -> io::Result<()>,
) -> LifecycleResult<()> {
    let parent = path.parent().ok_or(StoreError::UnsafePath)?;
    verify_private_path(parent, true)?;
    match fs::symlink_metadata(path) {
        Ok(_) => verify_private_path(path, false)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let temporary = parent.join(format!(".control-{}.tmp", random_id()?));
    let mut file = private_new_file(&temporary)?;
    let result = (|| {
        let bytes = serde_json::to_vec(value).map_err(|_| StoreError::InvalidData)?;
        file.write_all(&bytes)?;
        keys::sync_key_file(&file)?;
        drop(file);
        keys::replace_key_file(&temporary, path)?;
        parent_sync(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn durable_rename(source: &Path, destination: &Path, parent: &Path) -> LifecycleResult<()> {
    verify_private_path(source, true)?;
    if destination.try_exists()? {
        return Err(LifecycleError::Generation);
    }
    keys::replace_key_file(source, destination)?;
    sync_directory(parent)?;
    Ok(())
}

fn synchronize_database_directory(directory: &Path) -> LifecycleResult<()> {
    verify_private_path(directory, true)?;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        verify_private_path(&entry.path(), false)?;
        let mut options = OpenOptions::new();
        options.read(true);
        // Windows FlushFileBuffers requires a handle with write access.
        // Open the existing staged file without creating or truncating it.
        #[cfg(windows)]
        options.write(true);
        let file = options.open(entry.path())?;
        keys::sync_key_file(&file)?;
    }
    sync_directory(directory)?;
    Ok(())
}

fn source_size(paths: &StorePaths) -> LifecycleResult<u64> {
    verify_private_path(&paths.database(), false)?;
    let mut size = fs::symlink_metadata(paths.database())?.len();
    let wal = paths.db().join("maple.db-wal");
    match fs::symlink_metadata(&wal) {
        Ok(metadata) => {
            verify_private_path(&wal, false)?;
            size = size.saturating_add(metadata.len());
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(size)
}

fn random_id() -> LifecycleResult<String> {
    let mut id = [0_u8; 16];
    OsRng
        .try_fill_bytes(&mut id)
        .map_err(|_| KeyError::Random)?;
    Ok(encode_id(&id))
}

fn encode_id(id: &[u8; 16]) -> String {
    id.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode_id(value: &str) -> Option<[u8; 16]> {
    if value.len() != 32
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    let mut id = [0_u8; 16];
    for (index, byte) in id.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(id)
}

mod attachments;
#[cfg(test)]
mod generated_tests;
mod generation_repair;
mod malformed_setup;
mod notice;
mod salvage;
mod unclassified;
mod unclassified_identity;
pub use notice::StoreModeNotice;
#[cfg(test)]
mod export_retry_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod progress_run_tests {
    use super::*;

    #[test]
    fn a_new_export_run_resets_every_previous_terminal_counter() {
        let control = LifecycleControl::default();
        control.begin();
        let first = control.progress.lock().unwrap().run_id;
        control.update(
            OperationPhase::Complete,
            8 * 1024 * 1024,
            8 * 1024 * 1024,
            Some(0),
            false,
        );
        control.finish();
        control.begin();
        let progress = control.progress.lock().unwrap().clone();
        assert_ne!(progress.run_id, first);
        assert_eq!(progress.phase, OperationPhase::Preparing);
        assert_eq!(progress.completed_bytes, 0);
        assert_eq!(progress.total_bytes, 0);
        assert_eq!(progress.remaining_ms, None);
        assert!(progress.cancellable);
    }
}
