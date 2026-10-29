//! The one replacement path for explicit, potentially lossy recovery.
//!
//! The caller owns preview/confirmation and copies only readable logical rows
//! into the canonical destination. This module owns keys, identity, verification,
//! quarantine, durability, temporary retirement, and crash recovery.

use rusqlite::config::DbConfig;

use super::*;

impl Store {
    /// Persist the generation's known-damaged state before acknowledging a check.
    /// Call after admission is frozen, without already holding the lifecycle gate.
    pub(in crate::store) fn quarantine_generation(&self) -> StoreResult<()> {
        let result = (|| -> LifecycleResult<()> {
            let _gate = self.lifecycle.lock().map_err(|_| StoreError::Busy)?;
            if !matches!(self.state(), StoreState::Damaged { .. }) {
                return Err(StoreError::Conflict.into());
            }
            let worker = self
                .checkpoint_worker
                .lock()
                .map_err(|_| StoreError::Busy)?
                .take();
            if let Some(worker) = worker {
                let _ = worker.sender.send(super::super::CheckpointMessage::Stop);
                worker.thread.join().map_err(|_| StoreError::Busy)?;
            }
            self.quarantine_journal_locked()?;
            Ok(())
        })();
        result.map_err(startup_error)
    }

    /// Authenticate the authoritative slot without opening or migrating damaged
    /// database pages. A previous-password backup cannot authorize replacement.
    pub fn authenticate_repair_password(&self, password: &Password) -> LifecycleResult<()> {
        let _gate = self.lifecycle.lock().map_err(|_| StoreError::Busy)?;
        if !matches!(self.state(), StoreState::Damaged { .. }) {
            return Err(StoreError::Conflict.into());
        }
        if disk_mode(&self.paths)? != StoreMode::Password {
            return Err(StoreError::InvalidInput.into());
        }
        let salt = database_salt(&self.paths.database())?;
        let pair = KeyFilePair::load(&self.paths.db(), &salt)?;
        let (key, wrapping) = pair.primary().authenticate(password)?;
        let metadata_damaged = matches!(
            self.state(),
            StoreState::Damaged {
                reason: DamageReason::Generation
            }
        ) || !matches!(read_generation(&self.paths.db()), Ok(Some(_)));
        if metadata_damaged && self.repair_generation_from_readonly(Some(&key)).is_err() {
            // Password authentication is useful even when damaged database pages
            // cannot qualify a generation. Retain only authenticated material;
            // explicit reset will use the separate unclassified intent.
            *self.dek.lock().map_err(|_| StoreError::Busy)? = Some(key);
            *self
                .lifecycle_control
                .repair_wrapping
                .lock()
                .map_err(|_| StoreError::Busy)? = Some(wrapping);
            set_state(
                self,
                StoreState::Damaged {
                    reason: DamageReason::Generation,
                },
            )?;
            return Ok(());
        }
        self.quarantine_journal_locked()?;
        *self.dek.lock().map_err(|_| StoreError::Busy)? = Some(key);
        *self
            .lifecycle_control
            .repair_wrapping
            .lock()
            .map_err(|_| StoreError::Busy)? = Some(wrapping);
        Ok(())
    }

    /// Key access is scoped to the trusted read-only checker request. The key
    /// remains in locked memory and must never become CLI arguments or text.
    pub(in crate::store) fn with_authenticated_store_key<T>(
        &self,
        callback: impl FnOnce(Option<&LockedKey>) -> StoreResult<T>,
    ) -> LifecycleResult<T> {
        let _gate = self.lifecycle.lock().map_err(|_| StoreError::Busy)?;
        if !matches!(self.state(), StoreState::Open | StoreState::Damaged { .. }) {
            return Err(match self.state() {
                StoreState::Locked => StoreError::Locked,
                StoreState::NeedsSetup => StoreError::NeedsSetup,
                StoreState::Refused { reason } => StoreError::Refused { reason },
                _ => StoreError::Busy,
            }
            .into());
        }
        let mode = disk_mode(&self.paths)?;
        let key = self.dek.lock().map_err(|_| StoreError::Busy)?;
        if mode == StoreMode::Plain {
            return callback(None).map_err(Into::into);
        }
        if let Some(key) = key.as_ref() {
            return callback(Some(key)).map_err(Into::into);
        }
        let wrapping = self
            .lifecycle_control
            .repair_wrapping
            .lock()
            .map_err(|_| StoreError::Busy)?;
        let wrapping = wrapping.as_ref().ok_or(StoreError::Locked)?;
        if wrapping.database_salt() != &database_salt(&self.paths.database())? {
            return Err(KeyError::MismatchedDatabase.into());
        }
        let key = wrapping.data_key()?;
        callback(Some(&key)).map_err(Into::into)
    }

    /// The host has confirmed the explicit salvage preview and drained work.
    /// The builder receives a read-only source with its committed WAL, and a
    /// fresh canonical schema. It must use transactions/savepoints to omit
    /// unreadable logical units and may not copy source DDL or reserved metadata.
    pub(in crate::store) fn install_salvaged_replacement<T>(
        &self,
        builder: impl FnOnce(&Connection, &mut Connection) -> StoreResult<T>,
    ) -> LifecycleResult<T> {
        self.install_salvaged_with_commit_sync(builder, sync_directory)
    }

    fn install_salvaged_with_commit_sync<T>(
        &self,
        builder: impl FnOnce(&Connection, &mut Connection) -> StoreResult<T>,
        intent_sync: fn(&Path) -> io::Result<()>,
    ) -> LifecycleResult<T> {
        let _gate = self.lifecycle.lock().map_err(|_| StoreError::Busy)?;
        if !matches!(self.state(), StoreState::Damaged { .. }) {
            return Err(StoreError::Conflict.into());
        }
        let journal = self.quarantine_journal_locked()?;
        preflight_setup(&self.paths, &journal)?;
        let generation = journal.replacement();
        let mode = journal.previous_mode;
        let source_salt = database_salt(&self.paths.database())?;
        let total = source_size(&self.paths)?;
        if free_bytes(self.paths.root())? < total.saturating_add(16 * 1024 * 1024) {
            return Err(LifecycleError::NotEnoughSpace);
        }
        // Password-mode salvage always uses the original authoritative KEK.
        // Retaining only a DEK is enough for checking, but never for silently
        // replacing an encrypted store with plaintext or another password.
        let wrapping = self
            .lifecycle_control
            .repair_wrapping
            .lock()
            .map_err(|_| StoreError::Busy)?;
        let source_key = match mode {
            StoreMode::Plain => None,
            StoreMode::Password => {
                let wrapping = wrapping.as_ref().ok_or(StoreError::Locked)?;
                if wrapping.database_salt() != &source_salt {
                    return Err(KeyError::MismatchedDatabase.into());
                }
                Some(wrapping.data_key()?)
            }
        };
        let replacement_key = match mode {
            StoreMode::Plain => None,
            StoreMode::Password => Some(LockedKey::random()?),
        };
        self.lifecycle_control.begin();
        let _operation = ActiveOperation(&self.lifecycle_control);
        freeze_attachment_writers(self, true)?;
        // Do not checkpoint or truncate the damaged source, including on close.
        if let Err(error) = self.close_connections_without_checkpoint() {
            set_state(
                self,
                StoreState::Damaged {
                    reason: DamageReason::Database,
                },
            )?;
            return Err(error.into());
        }
        let temporary = self.paths.root().join("db.tmp");
        let mut commit_intent = false;
        let mut journal = journal;
        let result = (|| -> LifecycleResult<T> {
            self.lifecycle_control.check_cancelled()?;
            if path_present(&temporary)? {
                discard_temporary(&temporary, &generation)?;
            }
            private_directory(&temporary)?;
            write_json(&temporary.join("generation.json"), &generation)?;
            let destination_path = temporary.join("maple.db");
            drop(private_new_file(&destination_path)?);
            let mut destination = open_direct(&destination_path, replacement_key.as_ref())?;
            destination.pragma_update(None, "page_size", 4096)?;
            destination.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
            destination.pragma_update(None, "foreign_keys", true)?;
            destination.pragma_update(None, "application_id", schema::APPLICATION_ID)?;
            destination.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)?;
            schema::migrate(&mut destination, super::super::now_ms())?;
            set_replacement_identity(&destination, &journal)?;
            // Revalidate the public identity and key binding immediately before
            // the read-only source is opened. Do not use immutable=1: committed
            // source pages may still reside in its WAL.
            let current = read_generation(&self.paths.db())?.ok_or(LifecycleError::Generation)?;
            if current.store_id != journal.old_store_id
                || database_salt(&self.paths.database())? != source_salt
            {
                return Err(LifecycleError::Generation);
            }
            let source = open_readonly_source(&self.paths.database(), source_key.as_ref())?;
            self.lifecycle_control
                .update(OperationPhase::Exporting, 0, total, None, true);
            install_progress_handler(
                &source,
                destination_path.clone(),
                total,
                &self.lifecycle_control,
            )?;
            install_progress_handler(
                &destination,
                destination_path.clone(),
                total,
                &self.lifecycle_control,
            )?;
            let built = builder(&source, &mut destination);
            source
                .progress_handler(0, None::<fn() -> bool>)
                .map_err(StoreError::from)?;
            destination
                .progress_handler(0, None::<fn() -> bool>)
                .map_err(StoreError::from)?;
            source
                .close()
                .map_err(|(_, error)| StoreError::from(error))?;
            self.lifecycle_control.check_cancelled()?;
            let value = built?;
            if !destination.is_autocommit() {
                return Err(StoreError::InvalidData.into());
            }
            validate_replacement(&destination, &journal)?;
            attachments::copy_retained(
                &self.paths,
                &destination,
                &journal.old_store_id,
                &journal.new_store_id,
                &self.lifecycle_control,
            )?;
            destination
                .close()
                .map_err(|(_, error)| StoreError::from(error))?;
            self.lifecycle_control
                .update(OperationPhase::Verifying, total, total, None, true);
            verify_destination(
                &self.paths,
                &destination_path,
                replacement_key.as_ref(),
                Some(&self.lifecycle_control),
            )?;
            if let Some(key) = replacement_key.as_ref() {
                let wrapping = wrapping.as_ref().ok_or(StoreError::Locked)?;
                let slot = wrapping.wrap(key, database_salt(&destination_path)?)?;
                let outcome = KeyFilePair::write(&temporary, &slot)?;
                if outcome.backup_pending || outcome.synchronization_pending {
                    return Err(io::Error::from(io::ErrorKind::Other).into());
                }
            }
            synchronize_database_directory(&temporary)?;
            self.lifecycle_control.check_cancelled()?;
            self.lifecycle_control
                .update(OperationPhase::Committing, total, total, None, false);
            self.lifecycle_control.check_cancelled()?;
            journal.phase = SetupPhase::InstallNew;
            // Conservative one-way boundary: even a failed intent write may
            // have renamed its new phase before directory synchronization failed.
            // Never discard staging or reopen the original from this point.
            commit_intent = true;
            write_json_with_sync(&self.paths.setup(), &journal, intent_sync)?;
            preflight_setup(&self.paths, &journal)?;
            retire_old(&self.paths, &mut journal)?;
            durable_rename(
                &temporary,
                &self.paths.root().join("db.new"),
                self.paths.root(),
            )?;
            install_committed(&self.paths, &generation, Some(&journal.old_store_id))?;
            finish_setup(&self.paths, &mut journal)?;
            Ok(value)
        })();
        drop(wrapping);
        match result {
            Ok(value) => {
                self.lifecycle_control
                    .repair_wrapping
                    .lock()
                    .map_err(|_| StoreError::Busy)?
                    .take();
                self.reopen_locked(replacement_key)?;
                self.lifecycle_control.update(
                    OperationPhase::Complete,
                    total,
                    total,
                    Some(0),
                    false,
                );
                Ok(value)
            }
            Err(_) if commit_intent => {
                set_state(self, StoreState::Maintenance)?;
                Err(LifecycleError::RecoveryRequired { mode })
            }
            Err(error) => {
                // Keep the BuildReplacement marker as a durable quarantine.
                // Matching uncommitted staging may be removed on this retry or
                // startup; cleanup errors preserve every source byte and marker.
                if path_present(&temporary)? {
                    discard_temporary(&temporary, &generation)?;
                }
                attachments::remove_generation(&self.paths, &journal.new_store_id)?;
                *self.dek.lock().map_err(|_| StoreError::Busy)? = source_key;
                set_state(
                    self,
                    StoreState::Damaged {
                        reason: DamageReason::Database,
                    },
                )?;
                Err(error)
            }
        }
    }

    fn quarantine_journal_locked(&self) -> LifecycleResult<SetupJournal> {
        quarantine_journal(&self.paths)
    }
}

/// The checkpoint thread cannot acquire lifecycle: its owner can already hold
/// that gate while joining it. Generation replacement always joins that worker
/// before touching files, making this existing journal safe to publish here.
impl Store {
    pub(in crate::store) fn persist_checkpoint_quarantine(paths: &StorePaths) -> StoreResult<()> {
        quarantine_journal(paths).map(|_| ()).map_err(startup_error)
    }
}

fn quarantine_journal(paths: &StorePaths) -> LifecycleResult<SetupJournal> {
    let current = read_generation(&paths.db())?.ok_or(LifecycleError::Generation)?;
    if let Some(journal) =
        read_operation_json_optional::<SetupJournal>(&paths.setup(), SETUP_FORMAT)?
    {
        journal.validate()?;
        if journal.phase != SetupPhase::BuildReplacement || journal.old_store_id != current.store_id
        {
            return Err(LifecycleError::Generation);
        }
        preflight_setup(paths, &journal)?;
        return Ok(journal);
    }
    let journal = SetupJournal {
        format: SETUP_FORMAT.into(),
        operation_id: random_id()?,
        after: ResetAfter::StartFresh,
        previous_mode: disk_mode(paths)?,
        old_store_id: current.store_id,
        new_store_id: random_id()?,
        phase: SetupPhase::BuildReplacement,
    };
    preflight_setup(paths, &journal)?;
    write_json(&paths.setup(), &journal)?;
    Ok(journal)
}

pub(super) fn open_readonly_source(
    path: &Path,
    key: Option<&LockedKey>,
) -> StoreResult<Connection> {
    verify_private_path(path, false)?;
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    match key {
        Some(key) => engine::key_connection(&connection, key)?,
        None => engine::guard_plain(&connection)?,
    }
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)?;
    connection.pragma_update(None, "query_only", true)?;
    connection.pragma_update(None, "trusted_schema", false)?;
    connection.pragma_update(None, "temp_store", "MEMORY")?;
    Ok(connection)
}

fn set_replacement_identity(
    connection: &Connection,
    journal: &SetupJournal,
) -> LifecycleResult<()> {
    let id = decode_id(&journal.new_store_id).ok_or(LifecycleError::Generation)?;
    let operation = decode_id(&journal.operation_id).ok_or(LifecycleError::Generation)?;
    connection.execute(
        "UPDATE store_meta SET value=?1 WHERE key='store_id'",
        [id.as_slice()],
    )?;
    connection.execute(
        "INSERT INTO store_meta(key,value) VALUES ('setup_operation_id',?1)",
        [operation.as_slice()],
    )?;
    Ok(())
}

fn validate_replacement(connection: &Connection, journal: &SetupJournal) -> LifecycleResult<()> {
    schema::validate(connection)?;
    let identity: Vec<u8> = connection.query_row(
        "SELECT value FROM store_meta WHERE key='store_id'",
        [],
        |row| row.get(0),
    )?;
    let operation: Vec<u8> = connection.query_row(
        "SELECT value FROM store_meta WHERE key='setup_operation_id'",
        [],
        |row| row.get(0),
    )?;
    if identity != decode_id(&journal.new_store_id).ok_or(LifecycleError::Generation)?
        || operation != decode_id(&journal.operation_id).ok_or(LifecycleError::Generation)?
        || connection.pragma_query_value::<i64, _>(None, "application_id", |row| row.get(0))?
            != schema::APPLICATION_ID
        || connection.pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))?
            != schema::VERSION
        || connection.query_row::<i64, _, _>(
            "SELECT value FROM store_meta WHERE key='schema_epoch'",
            [],
            |row| row.get(0),
        )? != schema::SCHEMA_EPOCH
    {
        return Err(LifecycleError::Generation);
    }
    let mut statement = connection.prepare("PRAGMA foreign_key_check")?;
    if statement.query([])?.next()?.is_some() {
        return Err(StoreError::InvalidData.into());
    }
    Ok(())
}

#[cfg(test)]
#[path = "salvage_tests.rs"]
mod tests;
