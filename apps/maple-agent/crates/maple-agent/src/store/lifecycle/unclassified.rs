//! Confirmed recovery when no source generation ID can be verified.
//!
//! The original is qualified by private filesystem identities without reading
//! its contents. Temporary retirement is deleted after explicit reset; only the
//! new canonical database receives a verified generation.

use super::{unclassified_identity::DirectoryManifest, *};

const FORMAT: &str = "maple-store-unclassified-reset/3";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    RetireOriginal,
    AwaitChoice,
    InstallNew,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Journal {
    format: String,
    operation_id: String,
    new_store_id: String,
    after: ResetAfter,
    previous_mode: StoreMode,
    phase: Phase,
    source: DirectoryManifest,
    attachments: Option<unclassified_identity::FileIdentity>,
}

impl Journal {
    fn validate(&self) -> LifecycleResult<()> {
        if self.format != FORMAT {
            return Err(StoreError::UnsupportedOperationFormat.into());
        }
        if [&self.operation_id, &self.new_store_id]
            .iter()
            .any(|id| decode_id(id).is_none())
        {
            return Err(LifecycleError::Generation);
        }
        self.source.validate()?;
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
    pub(super) fn mode(&self) -> StoreMode {
        self.previous_mode
    }
}

fn intent_path(paths: &StorePaths) -> PathBuf {
    paths.root().join("unclassified-reset.json")
}
pub(super) fn pending(paths: &StorePaths) -> LifecycleResult<Option<Journal>> {
    let journal: Option<Journal> = read_operation_json_optional(&intent_path(paths), FORMAT)?;
    if let Some(journal) = &journal {
        journal.validate()?;
    }
    Ok(journal)
}

pub(super) fn status(paths: &StorePaths, state: StoreState) -> LifecycleResult<Option<LockStatus>> {
    let Some(journal) = pending(paths)? else {
        return Ok(None);
    };
    let replacement = journal.replacement();
    let installed = matches!(read_generation(&paths.db()),Ok(Some(actual)) if actual==replacement);
    Ok(Some(LockStatus {
        state,
        mode: journal.previous_mode,
        store_id: installed.then_some(replacement.store_id),
        setup_operation_id: Some(journal.operation_id),
    }))
}

pub(super) fn validate_staged(paths: &StorePaths, expected: &Generation) -> LifecycleResult<bool> {
    let Some(journal) = pending(paths)? else {
        return Ok(false);
    };
    if journal.replacement() != *expected {
        return Err(LifecycleError::Generation);
    }
    preflight(paths, &journal)?;
    Ok(true)
}

pub(super) fn recover(paths: &StorePaths) -> LifecycleResult<Option<Journal>> {
    let Some(mut journal) = pending(paths)? else {
        return Ok(None);
    };
    preflight(paths, &journal)?;
    let replacement = journal.replacement();
    if matches!(read_generation(&paths.db()),Ok(Some(actual)) if actual==replacement) {
        finish_installed(paths, &journal)?;
        return Ok(None);
    }
    retire_original(paths, &mut journal)?;
    let temporary = paths.root().join("db.tmp");
    let new = paths.root().join("db.new");
    if journal.phase == Phase::InstallNew {
        if !path_present(&new)? {
            durable_rename(&temporary, &new, paths.root())?;
        }
        install_committed(paths, &replacement, None)?;
        finish(paths, &journal)?;
        return Ok(None);
    }
    if path_present(&temporary)? {
        discard_temporary(&temporary, &replacement)?;
    }
    Ok(Some(journal))
}

fn preflight(paths: &StorePaths, journal: &Journal) -> LifecycleResult<()> {
    journal.validate()?;
    verify_private_path(paths.root(), true)?;
    attachments::verify_root(paths, &journal.attachments)?;
    if path_present(&paths.setup())? {
        return Err(LifecycleError::Generation);
    }
    let replacement = journal.replacement();
    let installed = matches!(read_generation(&paths.db()),Ok(Some(actual)) if actual==replacement);
    // Installed generation metadata wins over possible filesystem-ID reuse
    // after the old directory was deleted. A recreated empty db/ is likewise a
    // placeholder, never a surviving original source.
    let original = !installed
        && path_present(&paths.db())?
        && !directory_empty(&paths.db())?
        && journal.source.matches_directory(&paths.db())?;
    let archive = journal.retired_source(paths);
    let archived = path_present(&archive)?;
    if installed {
        verify_generation_contents(&paths.db())?;
    } else if original {
        if archived {
            return Err(LifecycleError::Generation);
        }
        journal.source.verify(&paths.db(), false)?;
    } else if path_present(&paths.db())? && !directory_empty(&paths.db())? {
        return Err(LifecycleError::Generation);
    }
    if archived {
        journal.source.verify(&archive, true)?;
    }
    let temporary = paths.root().join("db.tmp");
    if path_present(&temporary)? {
        if installed {
            return Err(LifecycleError::Generation);
        }
        match read_generation(&temporary)? {
            Some(actual) if actual == replacement => verify_generation_contents(&temporary)?,
            None => {
                metadata_only_temporary_files(&temporary)?;
            }
            _ => return Err(LifecycleError::Generation),
        }
    }
    let new = paths.root().join("db.new");
    if path_present(&new)? {
        if installed
            || read_generation(&new)?.as_ref() != Some(&replacement)
            || journal.phase != Phase::InstallNew
        {
            return Err(LifecycleError::Generation);
        }
        complete_stage(&new, &replacement)?;
    }
    if journal.phase == Phase::InstallNew && !installed && !path_present(&new)? {
        complete_stage(&temporary, &replacement)?;
    }
    Ok(())
}

fn complete_stage(path: &Path, expected: &Generation) -> LifecycleResult<()> {
    if read_generation(path)?.as_ref() != Some(expected) {
        return Err(LifecycleError::Generation);
    }
    verify_generation_contents(path)?;
    verify_private_path(&path.join("maple.db"), false)?;
    if fs::symlink_metadata(path.join("maple.db"))?.len() < 16 {
        return Err(LifecycleError::Generation);
    }
    Ok(())
}

fn retire_original(paths: &StorePaths, journal: &mut Journal) -> LifecycleResult<()> {
    let archive = journal.retired_source(paths);
    let installed =
        matches!(read_generation(&paths.db()),Ok(Some(actual)) if actual==journal.replacement());
    if !installed {
        if journal.source.matches_directory(&paths.db())? {
            durable_rename(&paths.db(), &archive, paths.root())?;
        } else if path_present(&paths.db())? {
            if !directory_empty(&paths.db())? {
                return Err(LifecycleError::Generation);
            }
            fs::remove_dir(paths.db())?;
            sync_directory(paths.root())?;
        }
    }
    // Both confirmed reset variants permanently discard their exact current
    // source. The manifest permits partial deletion after a crash, never a
    // replacement file or a directory belonging to another operation.
    attachments::remove_root(paths, &journal.attachments)?;
    journal.source.delete_remaining(&archive)?;
    if journal.phase == Phase::RetireOriginal {
        journal.phase = Phase::AwaitChoice;
        write_json(&intent_path(paths), journal)?;
    }
    Ok(())
}

fn finish(paths: &StorePaths, journal: &Journal) -> LifecycleResult<()> {
    if read_generation(&paths.db())?.as_ref() != Some(&journal.replacement()) {
        return Err(LifecycleError::Generation);
    }
    attachments::remove_root(paths, &journal.attachments)?;
    journal
        .source
        .delete_remaining(&journal.retired_source(paths))?;
    verify_private_path(&intent_path(paths), false)?;
    fs::remove_file(intent_path(paths))?;
    sync_directory(paths.root())?;
    Ok(())
}

fn finish_installed(paths: &StorePaths, journal: &Journal) -> LifecycleResult<()> {
    // The installed pair wins on retry. Cleanup can touch only this journal's
    // exact temporary source, not a later active generation or historical copy.
    attachments::remove_root(paths, &journal.attachments)?;
    journal
        .source
        .delete_remaining(&journal.retired_source(paths))?;
    verify_private_path(&intent_path(paths), false)?;
    fs::remove_file(intent_path(paths))?;
    sync_directory(paths.root())?;
    Ok(())
}

impl Store {
    pub(super) fn begin_unclassified_reset_locked(&self, after: ResetAfter) -> LifecycleResult<()> {
        self.begin_unclassified_with_sync_locked(after, sync_directory)
    }

    fn begin_unclassified_with_sync_locked(
        &self,
        after: ResetAfter,
        intent_sync: fn(&Path) -> io::Result<()>,
    ) -> LifecycleResult<()> {
        freeze_attachment_writers(self, true)?;
        self.close_connections_without_checkpoint()?;
        if let Some(mut journal) = pending(&self.paths)? {
            if matches!(read_generation(&self.paths.db()),Ok(Some(actual)) if actual==journal.replacement())
            {
                recover(&self.paths)?;
                return self.reopen_after_unclassified();
            }
            if after == ResetAfter::Erase && journal.after != ResetAfter::Erase {
                preflight(&self.paths, &journal)?;
                journal.after = ResetAfter::Erase;
                write_json(&intent_path(&self.paths), &journal)?;
            }
        } else {
            for path in [
                self.paths.setup(),
                self.paths.root().join("db.tmp"),
                self.paths.root().join("db.new"),
                self.paths.root().join("db.old"),
            ] {
                if path_present(&path)? {
                    return Err(LifecycleError::Generation);
                }
            }
            let mode = if !path_present(&self.paths.keys())?
                && !path_present(&self.paths.db().join("keys.backup.json"))?
                && matches!(database_salt(&self.paths.database()),Ok(header) if &header==b"SQLite format 3\0")
            {
                StoreMode::Plain
            } else {
                StoreMode::Password
            };
            let journal = Journal {
                format: FORMAT.into(),
                operation_id: random_id()?,
                new_store_id: random_id()?,
                after,
                previous_mode: mode,
                phase: Phase::RetireOriginal,
                source: DirectoryManifest::capture(&self.paths.db())?,
                attachments: attachments::root_identity(&self.paths)?,
            };
            preflight(&self.paths, &journal)?;
            if write_json_with_sync(&intent_path(&self.paths), &journal, intent_sync).is_err() {
                return Err(LifecycleError::RecoveryRequired { mode });
            }
        }
        let Some(journal) = recover(&self.paths)? else {
            return self.reopen_after_unclassified();
        };
        set_state(self, StoreState::NeedsSetup)?;
        if after == ResetAfter::Erase {
            self.lifecycle_control.clear_repair_material()?;
        }
        let wrapping = if after == ResetAfter::StartFresh {
            self.lifecycle_control
                .repair_wrapping
                .lock()
                .map_err(|_| StoreError::Busy)?
                .take()
        } else {
            None
        };
        if journal.previous_mode == StoreMode::Plain || wrapping.is_some() {
            self.complete_unclassified_locked(&journal, None, &[], KdfParams::floor(), wrapping)?;
        }
        Ok(())
    }

    pub(super) fn complete_unclassified_locked(
        &self,
        journal: &Journal,
        password: Option<PasswordChoice<'_>>,
        context: &[&str],
        params: KdfParams,
        wrapping: Option<WrappingKey>,
    ) -> LifecycleResult<()> {
        let mut journal = journal.clone();
        preflight(&self.paths, &journal)?;
        if journal.phase != Phase::AwaitChoice {
            return Err(LifecycleError::Generation);
        }
        let generation = journal.replacement();
        let key =
            self.build_empty_replacement_locked(&generation, password, context, params, wrapping)?;
        journal.phase = Phase::InstallNew;
        write_json(&intent_path(&self.paths), &journal)?;
        durable_rename(
            &self.paths.root().join("db.tmp"),
            &self.paths.root().join("db.new"),
            self.paths.root(),
        )?;
        install_committed(&self.paths, &generation, None)?;
        finish(&self.paths, &journal)?;
        self.reopen_locked(key)
    }

    pub(super) fn reopen_after_unclassified(&self) -> LifecycleResult<()> {
        match disk_mode(&self.paths)? {
            StoreMode::Plain => self.reopen_locked(None),
            StoreMode::Password => set_state(self, StoreState::Locked).map_err(Into::into),
        }
    }
}

#[cfg(test)]
#[path = "unclassified_tests.rs"]
mod tests;
