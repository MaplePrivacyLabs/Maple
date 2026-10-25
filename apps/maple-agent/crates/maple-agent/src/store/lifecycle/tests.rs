use super::*;

fn opened(directory: &Path) -> Store {
    let store = Store::unavailable(StorePaths::new(directory), StoreState::Maintenance).unwrap();
    store.initialize_recovered().unwrap();
    assert_eq!(store.state(), StoreState::Open);
    store
}

fn fixture_generation(id: u8, operation: Option<u8>) -> Generation {
    Generation::new([id; 16], operation.map(|id| encode_id(&[id; 16])))
}

fn generation_directory(path: &Path, generation: &Generation) {
    private_directory(path).unwrap();
    write_json(&path.join("generation.json"), generation).unwrap();
    let mut file = private_new_file(&path.join("maple.db")).unwrap();
    file.write_all(b"public test fixture only").unwrap();
    keys::sync_key_file(&file).unwrap();
    sync_directory(path).unwrap();
}

#[test]
fn synchronizing_staged_files_preserves_bytes_and_releases_handles() {
    let directory = tempfile::tempdir().unwrap();
    let paths = StorePaths::new(directory.path()).prepare().unwrap();
    let staged = paths.root().join("db.tmp");
    generation_directory(&staged, &fixture_generation(7, None));
    let mut keys = private_new_file(&staged.join("keys.json")).unwrap();
    keys.write_all(b"public staged key-file fixture").unwrap();
    drop(keys);
    let files = ["maple.db", "generation.json", "keys.json"];
    let original = files.map(|name| fs::read(staged.join(name)).unwrap());

    synchronize_database_directory(&staged).unwrap();
    synchronize_database_directory(&staged).unwrap();
    for (name, bytes) in files.into_iter().zip(&original) {
        assert_eq!(&fs::read(staged.join(name)).unwrap(), bytes);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // An exclusive reopen fails if synchronization leaked a handle.
            let _exclusive = OpenOptions::new()
                .read(true)
                .write(true)
                .share_mode(0)
                .open(staged.join(name))
                .unwrap();
        }
    }

    let installed = paths.root().join("db.new");
    durable_rename(&staged, &installed, paths.root()).unwrap();
    for (name, bytes) in files.into_iter().zip(&original) {
        assert_eq!(&fs::read(installed.join(name)).unwrap(), bytes);
    }
    fs::remove_dir_all(installed).unwrap();
}

#[test]
fn ordinary_swap_recovery_handles_every_committed_rename_state() {
    for stage in 0..4 {
        let directory = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(directory.path()).prepare().unwrap();
        let generation = fixture_generation(1, None);
        generation_directory(&paths.db(), &generation);
        match stage {
            0 => generation_directory(&paths.root().join("db.tmp"), &generation),
            1 => generation_directory(&paths.root().join("db.new"), &generation),
            2 => {
                fs::rename(paths.db(), paths.root().join("db.old")).unwrap();
                generation_directory(&paths.root().join("db.new"), &generation);
            }
            3 => generation_directory(&paths.root().join("db.old"), &generation),
            _ => unreachable!(),
        }
        // prepare() can create this placeholder between the two swap renames.
        private_directory(&paths.db()).unwrap();
        assert!(recover_filesystem(&paths).unwrap().is_none());
        assert_eq!(read_generation(&paths.db()).unwrap(), Some(generation));
        for artifact in ["db.tmp", "db.new", "db.old"] {
            assert!(!paths.root().join(artifact).exists());
        }
        assert!(recover_filesystem(&paths).unwrap().is_none());
    }
}

#[test]
fn completed_setup_authenticates_password_retries_without_changing_the_store() {
    let directory = tempfile::tempdir().unwrap();
    let store = opened(directory.path());
    store
        .upsert_account("existing-history-must-survive", 1)
        .unwrap();
    let generation = store.store_id().unwrap();
    let current = Password::normalize("Z7jv!9qP#3nMx@8Ks^2dH").unwrap();
    let different = Password::normalize("H5gx&2qT%8mVz!4Kd@7sJ").unwrap();
    assert!(matches!(
        store.setup_store(Some(&current), &[], KdfParams::floor()),
        Err(LifecycleError::Store(StoreError::Conflict))
    ));
    assert_eq!(store.lock_status().unwrap().mode, StoreMode::Plain);
    store.setup_store(None, &[], KdfParams::floor()).unwrap();
    assert_eq!(store.store_id().unwrap(), generation);
    store
        .set_store_password(&current, &[], KdfParams::floor())
        .unwrap();
    let salt = database_salt(&store.paths.database()).unwrap();
    let primary = keys::read_key_file(&store.paths.keys(), &salt).unwrap();
    let key = primary.unlock(&current).unwrap();
    let stale_backup = KeyFile::create(&different, &[], &key, salt, KdfParams::floor()).unwrap();
    let backup_path = store.paths.db().join("keys.backup.json");
    fs::write(&backup_path, stale_backup.to_json().unwrap()).unwrap();
    let keys = fs::read(store.paths.keys()).unwrap();
    let backup = fs::read(&backup_path).unwrap();
    let metadata = fs::read(store.paths.generation()).unwrap();
    let notice = store.mode_change_notice().unwrap();
    let status = store.lock_status().unwrap();
    assert!(matches!(
        store.setup_store(None, &[], KdfParams::floor()),
        Err(LifecycleError::Store(StoreError::Conflict))
    ));
    assert!(matches!(
        store.setup_store(Some(&different), &[], KdfParams::floor()),
        Err(LifecycleError::Key(KeyError::WrongPassword))
    ));
    assert_eq!(
        store
            .setup_store(Some(&current), &[], KdfParams::floor())
            .unwrap(),
        status
    );
    assert_eq!(store.lock_status().unwrap().mode, StoreMode::Password);
    assert_eq!(fs::read(store.paths.keys()).unwrap(), keys);
    assert_eq!(fs::read(backup_path).unwrap(), backup);
    assert_eq!(fs::read(store.paths.generation()).unwrap(), metadata);
    assert_eq!(database_salt(&store.paths.database()).unwrap(), salt);
    assert_eq!(store.mode_change_notice().unwrap(), notice);
    assert_eq!(store.store_id().unwrap(), generation);
    assert!(
        store
            .account_id("existing-history-must-survive")
            .unwrap()
            .is_some()
    );
}

#[test]
fn completed_password_setup_requires_the_primary_dek_and_installed_generation() {
    let directory = tempfile::tempdir().unwrap();
    let store = opened(directory.path());
    let current = Password::normalize("Z7jv!9qP#3nMx@8Ks^2dH").unwrap();
    store
        .set_store_password(&current, &[], KdfParams::floor())
        .unwrap();
    store.upsert_account("retry-must-keep-history", 1).unwrap();
    let actual = read_generation(&store.paths.db()).unwrap().unwrap();
    let foreign = Generation::new([91; 16], actual.setup_operation_id.clone());
    write_json(&store.paths.generation(), &foreign).unwrap();
    let foreign_bytes = fs::read(store.paths.generation()).unwrap();
    let keys = fs::read(store.paths.keys()).unwrap();
    assert!(matches!(
        store.setup_store(Some(&current), &[], KdfParams::floor()),
        Err(LifecycleError::Generation)
    ));
    assert_eq!(fs::read(store.paths.generation()).unwrap(), foreign_bytes);
    assert_eq!(fs::read(store.paths.keys()).unwrap(), keys);
    write_json(&store.paths.generation(), &actual).unwrap();

    let salt = database_salt(&store.paths.database()).unwrap();
    let wrong_key = LockedKey::random().unwrap();
    let wrong_slot = KeyFile::create(&current, &[], &wrong_key, salt, KdfParams::floor()).unwrap();
    fs::write(store.paths.keys(), wrong_slot.to_json().unwrap()).unwrap();
    let wrong_slot_bytes = fs::read(store.paths.keys()).unwrap();
    let backup = fs::read(store.paths.db().join("keys.backup.json")).unwrap();
    let metadata = fs::read(store.paths.generation()).unwrap();
    assert!(
        store
            .setup_store(Some(&current), &[], KdfParams::floor())
            .is_err()
    );
    assert_eq!(store.state(), StoreState::Open);
    assert_eq!(fs::read(store.paths.keys()).unwrap(), wrong_slot_bytes);
    assert_eq!(
        fs::read(store.paths.db().join("keys.backup.json")).unwrap(),
        backup
    );
    assert_eq!(fs::read(store.paths.generation()).unwrap(), metadata);
    assert!(
        store
            .account_id("retry-must-keep-history")
            .unwrap()
            .is_some()
    );
    for artifact in ["db.tmp", "db.new", "db.old"] {
        assert!(!store.paths.root().join(artifact).exists());
    }
}

#[test]
fn staged_checker_accepts_only_the_current_export_or_reserved_operation_generation() {
    let directory = tempfile::tempdir().unwrap();
    let store = opened(directory.path());
    let current = read_generation(&store.paths.db()).unwrap().unwrap();
    let temporary = store.paths.root().join("db.tmp");
    generation_directory(&temporary, &current);
    assert_eq!(
        validate_staged_checker(&store.paths, &current.store_id, None).unwrap(),
        temporary.join("maple.db")
    );
    assert!(validate_staged_checker(&store.paths, &encode_id(&[9; 16]), None).is_err());
    let journal = SetupJournal {
        format: SETUP_FORMAT.into(),
        operation_id: encode_id(&[5; 16]),
        after: ResetAfter::StartFresh,
        previous_mode: StoreMode::Plain,
        old_store_id: current.store_id.clone(),
        new_store_id: encode_id(&[7; 16]),
        phase: SetupPhase::BuildReplacement,
    };
    let replacement = journal.replacement();
    write_json(&temporary.join("generation.json"), &replacement).unwrap();
    assert!(
        validate_staged_checker(
            &store.paths,
            &replacement.store_id,
            replacement.setup_operation_id.as_deref()
        )
        .is_err()
    );
    write_json(&store.paths.setup(), &journal).unwrap();
    assert!(
        validate_staged_checker(
            &store.paths,
            &replacement.store_id,
            replacement.setup_operation_id.as_deref()
        )
        .is_ok()
    );
    assert!(
        validate_staged_checker(
            &store.paths,
            &replacement.store_id,
            Some(&encode_id(&[6; 16]))
        )
        .is_err()
    );
    assert!(validate_staged_checker(&store.paths, "../unrelated", None).is_err());
}

#[test]
fn refused_reset_preflight_never_publishes_intent_or_retires_source() {
    for encrypted in [false, true] {
        for after in [ResetAfter::Erase, ResetAfter::StartFresh] {
            let directory = tempfile::tempdir().unwrap();
            let store = opened(directory.path());
            let password = Password::normalize("Z7jv!9qP#3nMx@8Ks^2dH").unwrap();
            if encrypted {
                store
                    .set_store_password(&password, &[], KdfParams::floor())
                    .unwrap();
            }
            store
                .upsert_account("preflight-source-must-survive", 1)
                .unwrap();
            let generation = read_generation(&store.paths.db()).unwrap().unwrap();
            let foreign = store.paths.db().join(".DS_Store");
            fs::write(&foreign, b"foreign file must remain untouched").unwrap();
            // Finish any in-flight WAL checkpoint before comparing physical
            // database bytes. Keep every SQL handle and the source state open:
            // refusal must still preserve the live source and cold recovery.
            let worker = store.checkpoint_worker.lock().unwrap().take().unwrap();
            worker
                .sender
                .send(super::super::CheckpointMessage::Stop)
                .unwrap();
            worker.thread.join().unwrap();
            let before = fs::read(store.paths.database()).unwrap();
            if after == ResetAfter::StartFresh {
                set_state(
                    &store,
                    StoreState::Damaged {
                        reason: DamageReason::Database,
                    },
                )
                .unwrap();
            }
            let previous_state = store.state();
            let result = match after {
                ResetAfter::Erase => store.erase_local_data(ERASE_CONFIRMATION),
                ResetAfter::StartFresh => store.start_fresh(START_FRESH_CONFIRMATION),
            };
            assert!(result.is_err());
            assert_eq!(store.state(), previous_state);
            assert!(!store.paths.setup().exists());
            assert_eq!(
                read_generation(&store.paths.db()).unwrap(),
                Some(generation)
            );
            assert_eq!(fs::read(store.paths.database()).unwrap(), before);
            assert_eq!(
                fs::read(&foreign).unwrap(),
                b"foreign file must remain untouched"
            );
            for artifact in ["db.old", "db.new", "db.tmp"] {
                assert!(!store.paths.root().join(artifact).exists());
            }
            drop(store);

            // The refused reset must not be resumed destructively on restart.
            let recovered =
                Store::unavailable(StorePaths::new(directory.path()), StoreState::Maintenance)
                    .unwrap();
            recovered.initialize_recovered().unwrap();
            if encrypted {
                recovered.unlock_store(&password, None).unwrap();
            }
            assert_eq!(recovered.state(), StoreState::Open);
            assert!(
                recovered
                    .account_id("preflight-source-must-survive")
                    .unwrap()
                    .is_some()
            );
            assert_eq!(
                fs::read(&foreign).unwrap(),
                b"foreign file must remain untouched"
            );
        }
    }
}

#[test]
fn competing_retired_source_is_refused_before_intent() {
    let directory = tempfile::tempdir().unwrap();
    let store = opened(directory.path());
    let original = read_generation(&store.paths.db()).unwrap().unwrap();
    let archive = store.paths.root().join("db.old");
    generation_directory(&archive, &original);
    let archive_bytes = fs::read(archive.join("maple.db")).unwrap();
    let source_bytes = fs::read(store.paths.database()).unwrap();
    set_state(
        &store,
        StoreState::Damaged {
            reason: DamageReason::Database,
        },
    )
    .unwrap();
    assert!(store.start_fresh(START_FRESH_CONFIRMATION).is_err());
    assert!(!store.paths.setup().exists());
    assert_eq!(fs::read(store.paths.database()).unwrap(), source_bytes);
    assert_eq!(fs::read(archive.join("maple.db")).unwrap(), archive_bytes);
    assert!(matches!(store.state(), StoreState::Damaged { .. }));
}

#[test]
fn a_published_reset_intent_sync_failure_blocks_new_acknowledged_writes() {
    fn fail_sync(_: &Path) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::Other))
    }
    let directory = tempfile::tempdir().unwrap();
    let store = opened(directory.path());
    store.upsert_account("old-confirmed-erase", 1).unwrap();
    {
        let _gate = store.lifecycle.lock().unwrap();
        assert_eq!(
            store
                .begin_reset_with_intent_sync_locked(ResetAfter::Erase, fail_sync)
                .unwrap_err(),
            LifecycleError::RecoveryRequired {
                mode: StoreMode::Plain
            }
        );
    }
    assert_eq!(store.state(), StoreState::Maintenance);
    assert!(store.upsert_account("must-not-be-acknowledged", 2).is_err());
    drop(store);
    let recovered = opened(directory.path());
    assert!(
        recovered
            .account_id("old-confirmed-erase")
            .unwrap()
            .is_none()
    );
    assert!(
        recovered
            .account_id("must-not-be-acknowledged")
            .unwrap()
            .is_none()
    );
}

#[test]
fn recovery_resumes_the_initial_metadata_write_and_final_cleanup_windows() {
    let directory = tempfile::tempdir().unwrap();
    let paths = StorePaths::new(directory.path()).prepare().unwrap();
    let generation = fixture_generation(1, None);
    generation_directory(&paths.db(), &generation);
    let temporary = paths.root().join("db.tmp");
    private_directory(&temporary).unwrap();
    let incomplete = temporary.join(format!(".control-{}.tmp", encode_id(&[9; 16])));
    let mut file = private_new_file(&incomplete).unwrap();
    file.write_all(b"incomplete public generation metadata")
        .unwrap();
    drop(file);
    assert!(recover_filesystem(&paths).unwrap().is_none());
    assert!(!temporary.exists());
    private_directory(&paths.root().join("db.old")).unwrap();
    assert!(recover_filesystem(&paths).unwrap().is_none());
    assert!(!paths.root().join("db.old").exists());
    private_directory(&temporary).unwrap();
    drop(private_new_file(&temporary.join("maple.db")).unwrap());
    // Missing identity beside database bytes is never treated as metadata-only.
    assert_eq!(
        recover_filesystem(&paths).unwrap_err(),
        LifecycleError::Generation
    );
    assert!(paths.database().exists());
    assert!(temporary.join("maple.db").exists());
}

#[test]
fn conflicting_generations_and_symlinks_stop_before_deleting_source() {
    let directory = tempfile::tempdir().unwrap();
    let paths = StorePaths::new(directory.path()).prepare().unwrap();
    let original = fixture_generation(1, None);
    generation_directory(&paths.db(), &original);
    let conflicting = fixture_generation(2, None);
    generation_directory(&paths.root().join("db.new"), &conflicting);
    let before = fs::read(paths.database()).unwrap();
    assert_eq!(
        recover_filesystem(&paths).unwrap_err(),
        LifecycleError::Generation
    );
    assert_eq!(fs::read(paths.database()).unwrap(), before);
    assert!(paths.root().join("db.new").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let private_fixture = directory.path().join("unrelated");
        fs::write(&private_fixture, b"untouched").unwrap();
        symlink(&private_fixture, paths.db().join("redirect")).unwrap();
        assert!(remove_generation_directory(&paths.db(), Some(&original)).is_err());
        assert_eq!(fs::read(&private_fixture).unwrap(), b"untouched");
        assert_eq!(fs::read(paths.database()).unwrap(), before);
    }
}

#[test]
fn every_stale_setup_phase_preserves_an_already_installed_new_generation() {
    for phase in [
        SetupPhase::BuildReplacement,
        SetupPhase::RetireOld,
        SetupPhase::AwaitChoice,
        SetupPhase::InstallNew,
        SetupPhase::Installed,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let store = opened(directory.path());
        let old = store.store_id().unwrap();
        store.erase_local_data(ERASE_CONFIRMATION).unwrap();
        let new = store.store_id().unwrap();
        assert_ne!(new, old);
        store.upsert_account("new-history-must-survive", 1).unwrap();
        let installed = read_generation(&store.paths.db()).unwrap().unwrap();
        let journal = SetupJournal {
            format: SETUP_FORMAT.into(),
            operation_id: installed.setup_operation_id.clone().unwrap(),
            after: ResetAfter::Erase,
            previous_mode: StoreMode::Plain,
            old_store_id: encode_id(&old),
            new_store_id: encode_id(&new),
            phase,
        };
        store.close().unwrap();
        write_json(&store.paths.setup(), &journal).unwrap();
        let recovered = Store::unavailable(store.paths.clone(), StoreState::Maintenance).unwrap();
        recovered.initialize_recovered().unwrap();
        assert_eq!(recovered.store_id().unwrap(), new);
        assert!(
            recovered
                .account_id("new-history-must-survive")
                .unwrap()
                .is_some()
        );
        assert!(!recovered.paths.setup().exists());
        // A duplicated setup request cannot discard newly written history.
        recovered
            .setup_store(None, &[], KdfParams::floor())
            .unwrap();
        assert!(
            recovered
                .account_id("new-history-must-survive")
                .unwrap()
                .is_some()
        );
    }
}

#[test]
fn pending_password_erase_waits_for_an_explicit_choice() {
    let directory = tempfile::tempdir().unwrap();
    let paths = StorePaths::new(directory.path()).prepare().unwrap();
    let old = fixture_generation(1, None);
    generation_directory(&paths.db(), &old);
    let journal = SetupJournal {
        format: SETUP_FORMAT.into(),
        operation_id: encode_id(&[3; 16]),
        after: ResetAfter::Erase,
        previous_mode: StoreMode::Password,
        old_store_id: old.store_id.clone(),
        new_store_id: encode_id(&[2; 16]),
        phase: SetupPhase::RetireOld,
    };
    write_json(&paths.setup(), &journal).unwrap();
    let store = Store::unavailable(paths.clone(), StoreState::Maintenance).unwrap();
    store.initialize_recovered().unwrap();
    assert_eq!(store.state(), StoreState::NeedsSetup);
    assert!(!paths.database().exists());
    assert!(paths.setup().exists());
    store.setup_store(None, &[], KdfParams::floor()).unwrap();
    assert_eq!(store.state(), StoreState::Open);
    assert_eq!(store.lock_status().unwrap().mode, StoreMode::Plain);
    assert_eq!(store.store_id().unwrap(), [2; 16]);
    assert!(!paths.setup().exists());
}

#[test]
fn committed_setup_recovers_both_before_and_after_the_install_rename() {
    for phase in [SetupPhase::AwaitChoice, SetupPhase::InstallNew] {
        for temporary in [false, true] {
            if temporary && phase == SetupPhase::AwaitChoice {
                continue;
            }
            let directory = tempfile::tempdir().unwrap();
            let paths = StorePaths::new(directory.path()).prepare().unwrap();
            let journal = SetupJournal {
                format: SETUP_FORMAT.into(),
                operation_id: encode_id(&[3; 16]),
                after: ResetAfter::Erase,
                previous_mode: StoreMode::Password,
                old_store_id: encode_id(&[1; 16]),
                new_store_id: encode_id(&[2; 16]),
                phase,
            };
            write_json(&paths.setup(), &journal).unwrap();
            let staging = paths
                .root()
                .join(if temporary { "db.tmp" } else { "db.new" });
            generation_directory(&staging, &journal.replacement());
            assert!(recover_filesystem(&paths).unwrap().is_none());
            assert_eq!(
                read_generation(&paths.db()).unwrap(),
                Some(journal.replacement())
            );
            assert!(!paths.setup().exists());
            assert!(recover_filesystem(&paths).unwrap().is_none());
        }
    }
}

#[test]
fn confirmed_reset_discards_only_current_source_and_survives_cold_reopen() {
    for encrypted in [false, true] {
        for after in [ResetAfter::Erase, ResetAfter::StartFresh] {
            let directory = tempfile::tempdir().unwrap();
            let store = opened(directory.path());
            let password = Password::normalize("Z7jv!9qP#3nMx@8Ks^2dH").unwrap();
            if encrypted {
                store
                    .set_store_password(&password, &[], KdfParams::floor())
                    .unwrap();
            }
            let old = store.store_id().unwrap();
            let attachment_root = store.paths.root().join("attachments");
            private_directory(&attachment_root).unwrap();
            let old_attachments = attachment_root.join(encode_id(&old));
            private_directory(&old_attachments).unwrap();
            let mut original = private_new_file(&old_attachments.join("current-file")).unwrap();
            original.write_all(b"current session attachment").unwrap();
            drop(original);
            let historical_attachments = attachment_root.join(encode_id(&[89; 16]));
            private_directory(&historical_attachments).unwrap();
            let mut previous =
                private_new_file(&historical_attachments.join("historical-file")).unwrap();
            previous.write_all(b"keep unrelated generation").unwrap();
            drop(previous);
            store
                .upsert_account("current-history-to-discard", 1)
                .unwrap();
            let untouched = store
                .paths
                .root()
                .join(format!("db.damaged-{}", encode_id(&[88; 16])));
            private_directory(&untouched).unwrap();
            fs::write(
                untouched.join("sentinel"),
                b"preexisting copies are not swept",
            )
            .unwrap();
            set_state(
                &store,
                StoreState::Damaged {
                    reason: DamageReason::Database,
                },
            )
            .unwrap();
            match after {
                ResetAfter::Erase => store.erase_local_data(ERASE_CONFIRMATION).unwrap(),
                ResetAfter::StartFresh => store.start_fresh(START_FRESH_CONFIRMATION).unwrap(),
            };
            if encrypted {
                assert_eq!(store.state(), StoreState::NeedsSetup);
                store
                    .setup_store(Some(&password), &[], KdfParams::floor())
                    .unwrap();
            }
            let replacement = store.store_id().unwrap();
            assert_ne!(replacement, old);
            assert!(!old_attachments.exists());
            assert_eq!(
                fs::read(historical_attachments.join("historical-file")).unwrap(),
                b"keep unrelated generation"
            );
            assert!(
                store
                    .account_id("current-history-to-discard")
                    .unwrap()
                    .is_none()
            );
            for name in ["db.old", "db.tmp", "db.new"] {
                assert!(!store.paths.root().join(name).exists());
            }
            assert!(
                !store
                    .paths
                    .root()
                    .join(format!("db.damaged-{}", encode_id(&old)))
                    .exists()
            );
            assert!(!store.paths.setup().exists());
            assert_eq!(
                fs::read(untouched.join("sentinel")).unwrap(),
                b"preexisting copies are not swept"
            );
            store.close().unwrap();
            drop(store);
            let reopened =
                Store::unavailable(StorePaths::new(directory.path()), StoreState::Maintenance)
                    .unwrap();
            reopened.initialize_recovered().unwrap();
            if encrypted {
                reopened.unlock_store(&password, None).unwrap();
            }
            assert_eq!(reopened.store_id().unwrap(), replacement);
            assert!(
                reopened
                    .account_id("current-history-to-discard")
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                fs::read(untouched.join("sentinel")).unwrap(),
                b"preexisting copies are not swept"
            );
        }
    }
}

#[test]
fn set_rewrap_reencrypt_and_remove_preserve_rows_and_require_current_password() {
    use crate::store::{ImageUpload, NewTask, SessionLoad, TaskKind};
    use base64::Engine;
    use pi_ai::{Content, Message, UserMessage};
    use pi_coding_agent::SessionMessage;
    let directory = tempfile::Builder::new()
        .prefix("Maple O'Brien ")
        .tempdir()
        .unwrap();
    let store = Arc::new(opened(directory.path()));
    let generation = store.store_id().unwrap();
    let marker = "encrypted-history-reference-marker";
    let account = store.upsert_account(marker, 1).unwrap();
    let uuid = "00000000-0000-7000-8000-000000000031";
    let project = if cfg!(windows) {
        "C:/fixture/project"
    } else {
        "/fixture/project"
    };
    store
        .create_task(
            account,
            &NewTask {
                uuid: uuid.into(),
                project_root: project.into(),
                kind: TaskKind::Desktop,
                title: marker.into(),
                provisional: false,
                model: None,
                created_ms: 1,
            },
        )
        .unwrap();
    let mut session = store
        .load_session(account, uuid, SessionLoad::Context)
        .unwrap()
        .into_manager(project);
    session.append_message(SessionMessage::Llm(Message::User(UserMessage {
        content: vec![Content::text(marker)],
        timestamp: 2,
    })));
    session.flush().unwrap();
    let blob = b"\x89PNG\r\n\x1a\nprivate-image-reference-bytes";
    let attachment = store
        .store_image_uploads(
            account,
            uuid,
            &[ImageUpload {
                name: "fixture.png".into(),
                data_url: format!(
                    "data:image/png;base64,{}",
                    base64::engine::general_purpose::STANDARD.encode(blob)
                ),
            }],
        )
        .unwrap()
        .remove(0);
    let old = Password::normalize("Z7jv!9qP#3nMx@8Ks^2dH").unwrap();
    let new = Password::normalize("H5gx&2qT%8mVz!4Kd@7sJ").unwrap();
    let wrong = Password::normalize("an incorrect password entirely").unwrap();
    // Valid length, but an obvious repeated password pattern under zxcvbn.
    let weak = Password::normalize("passwordpassword").unwrap();
    assert!(matches!(
        store.set_store_password(&weak, &[], KdfParams::floor()),
        Err(LifecycleError::Key(KeyError::WeakPassword))
    ));
    store
        .set_store_password(&old, &[], KdfParams::floor())
        .unwrap();
    assert_eq!(store.lock_status().unwrap().mode, StoreMode::Password);
    assert_eq!(store.store_id().unwrap(), generation);
    assert!(store.account_id(marker).unwrap().is_some());
    assert_eq!(
        store
            .task(account, uuid)
            .unwrap()
            .unwrap()
            .facts
            .message_count,
        1
    );
    assert_eq!(
        store
            .read_attachment(account, uuid, &attachment.id, 0, blob.len())
            .unwrap()
            .data,
        blob
    );
    let first_salt = database_salt(&store.paths.database()).unwrap();
    let first_pair = KeyFilePair::load(&store.paths.db(), &first_salt).unwrap();
    let first_key = first_pair.primary().unlock(&old).unwrap();
    assert!(matches!(
        store.remove_store_password(&wrong),
        Err(LifecycleError::Key(KeyError::WrongPassword))
    ));
    assert!(matches!(
        store.change_store_password(&wrong, &new, &[], KdfParams::floor(), false),
        Err(LifecycleError::Key(KeyError::WrongPassword))
    ));
    store
        .change_store_password(&old, &new, &[], KdfParams::floor(), false)
        .unwrap();
    assert_eq!(database_salt(&store.paths.database()).unwrap(), first_salt);
    let changed = KeyFilePair::load(&store.paths.db(), &first_salt).unwrap();
    assert_eq!(
        changed.primary().unlock(&new).unwrap().as_bytes(),
        first_key.as_bytes()
    );
    assert!(matches!(
        changed.primary().unlock(&old),
        Err(KeyError::WrongPassword)
    ));
    store
        .change_store_password(&new, &old, &[], KdfParams::floor(), true)
        .unwrap();
    let rotated_salt = database_salt(&store.paths.database()).unwrap();
    assert_ne!(rotated_salt, first_salt);
    let rotated = KeyFilePair::load(&store.paths.db(), &rotated_salt).unwrap();
    assert_ne!(
        rotated.primary().unlock(&old).unwrap().as_bytes(),
        first_key.as_bytes()
    );
    assert!(store.account_id(marker).unwrap().is_some());
    assert_eq!(
        store
            .load_session(account, uuid, SessionLoad::Full)
            .unwrap()
            .entries
            .len(),
        1
    );
    assert_eq!(
        store
            .read_attachment(account, uuid, &attachment.id, 0, blob.len())
            .unwrap()
            .data,
        blob
    );
    assert!(
        !fs::read(store.paths.database())
            .unwrap()
            .windows(marker.len())
            .any(|value| value == marker.as_bytes())
    );
    store.remove_store_password(&old).unwrap();
    assert_eq!(store.lock_status().unwrap().mode, StoreMode::Plain);
    assert_eq!(
        &database_salt(&store.paths.database()).unwrap(),
        b"SQLite format 3\0"
    );
    assert!(!store.paths.keys().exists());
    assert!(store.account_id(marker).unwrap().is_some());
    assert_eq!(
        store
            .load_session(account, uuid, SessionLoad::Full)
            .unwrap()
            .entries
            .len(),
        1
    );
    assert_eq!(
        store
            .read_attachment(account, uuid, &attachment.id, 0, blob.len())
            .unwrap()
            .data,
        blob
    );
    assert_eq!(store.store_id().unwrap(), generation);
}

#[test]
fn restart_of_password_store_is_locked_and_unlock_preserves_the_same_handle() {
    let directory = tempfile::tempdir().unwrap();
    let store = opened(directory.path());
    let password = Password::normalize("Z7jv!9qP#3nMx@8Ks^2dH").unwrap();
    store.upsert_account("persisted-account", 1).unwrap();
    store
        .set_store_password(&password, &[], KdfParams::floor())
        .unwrap();
    let paths = store.paths.clone();
    store.close().unwrap();
    drop(store);
    fs::remove_file(paths.keys()).unwrap();
    let restarted = Store::unavailable(paths, StoreState::Maintenance).unwrap();
    restarted.initialize_recovered().unwrap();
    assert_eq!(restarted.state(), StoreState::Locked);
    assert!(restarted.paths.keys().exists());
    assert_eq!(
        restarted.account_id("persisted-account"),
        Err(StoreError::Locked)
    );
    let wrong = Password::normalize("a different password entirely").unwrap();
    assert!(matches!(
        restarted.unlock_store(&wrong, None),
        Err(LifecycleError::Key(KeyError::WrongPassword))
    ));
    assert_eq!(restarted.state(), StoreState::Locked);
    restarted.unlock_store(&password, None).unwrap();
    assert_eq!(restarted.state(), StoreState::Open);
    assert!(restarted.account_id("persisted-account").unwrap().is_some());
}

#[test]
fn lifecycle_errors_do_not_retain_paths_sql_or_passwords() {
    let errors = [
        LifecycleError::Generation,
        LifecycleError::Confirmation,
        LifecycleError::Cancelled,
        LifecycleError::NotEnoughSpace,
        LifecycleError::Key(KeyError::WrongPassword),
    ];
    for error in errors {
        let rendered = format!("{error:?}: {error}");
        assert!(!rendered.contains("ATTACH"));
        assert!(!rendered.contains("password-reference-marker"));
        assert!(std::error::Error::source(&error).is_none());
    }
}

#[test]
fn a_post_commit_directory_sync_failure_never_reopens_the_old_writer() {
    fn fail_sync(_: &Path) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::Other))
    }
    let directory = tempfile::tempdir().unwrap();
    let store = opened(directory.path());
    store.upsert_account("saved-before-export", 1).unwrap();
    let outcome =
        store.export_swap_with_commit_sync_locked(None, None, "password_removed", fail_sync);
    assert_eq!(
        outcome,
        Err(LifecycleError::RecoveryRequired {
            mode: StoreMode::Plain
        })
    );
    assert_eq!(store.state(), StoreState::Maintenance);
    assert_eq!(
        store.upsert_account("must-not-be-acknowledged", 2),
        Err(StoreError::Busy)
    );
    assert!(store.paths.root().join("db.new").exists());
    store.initialize_recovered().unwrap();
    assert_eq!(store.state(), StoreState::Open);
    assert!(store.account_id("saved-before-export").unwrap().is_some());
    assert!(
        store
            .account_id("must-not-be-acknowledged")
            .unwrap()
            .is_none()
    );
}

#[test]
fn retrying_erase_with_an_installed_replacement_and_stale_marker_preserves_new_writes() {
    let directory = tempfile::tempdir().unwrap();
    let store = opened(directory.path());
    let original = store.store_id().unwrap();
    store.erase_local_data(ERASE_CONFIRMATION).unwrap();
    let generation = read_generation(&store.paths.db()).unwrap().unwrap();
    store
        .upsert_account("new-row-after-first-erase", 1)
        .unwrap();
    let stale = SetupJournal {
        format: SETUP_FORMAT.into(),
        operation_id: generation.setup_operation_id.clone().unwrap(),
        after: ResetAfter::Erase,
        previous_mode: StoreMode::Plain,
        old_store_id: encode_id(&original),
        new_store_id: generation.store_id.clone(),
        phase: SetupPhase::RetireOld,
    };
    write_json(&store.paths.setup(), &stale).unwrap();
    store.erase_local_data(ERASE_CONFIRMATION).unwrap();
    assert_eq!(store.state(), StoreState::Open);
    assert!(
        store
            .account_id("new-row-after-first-erase")
            .unwrap()
            .is_some()
    );
    assert!(!store.paths.root().join("db.new").exists());
    assert!(!store.paths.setup().exists());
}

#[test]
fn conflicting_setup_artifacts_are_all_checked_before_retiring_original_data() {
    for artifact in ["db.new", "db.tmp", "db.old"] {
        let directory = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(directory.path()).prepare().unwrap();
        let old = fixture_generation(1, None);
        generation_directory(&paths.db(), &old);
        let journal = SetupJournal {
            format: SETUP_FORMAT.into(),
            operation_id: encode_id(&[3; 16]),
            after: ResetAfter::Erase,
            previous_mode: StoreMode::Password,
            old_store_id: old.store_id.clone(),
            new_store_id: encode_id(&[2; 16]),
            phase: SetupPhase::RetireOld,
        };
        write_json(&paths.setup(), &journal).unwrap();
        let conflicting = paths.root().join(artifact);
        generation_directory(&conflicting, &fixture_generation(9, None));
        let original = fs::read(paths.database()).unwrap();
        assert_eq!(
            recover_filesystem(&paths).unwrap_err(),
            LifecycleError::Generation
        );
        assert_eq!(fs::read(paths.database()).unwrap(), original);
        assert_eq!(read_generation(&paths.db()).unwrap(), Some(old));
        assert!(conflicting.join("maple.db").exists());
        assert_eq!(paths.root().join("db.old").exists(), artifact == "db.old");
    }
}

#[test]
fn an_install_marker_with_missing_or_incomplete_staging_preserves_original_data() {
    for stage in [0, 1, 2] {
        let directory = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(directory.path()).prepare().unwrap();
        let original = fixture_generation(1, None);
        generation_directory(&paths.db(), &original);
        let journal = SetupJournal {
            format: SETUP_FORMAT.into(),
            operation_id: encode_id(&[3; 16]),
            after: ResetAfter::Erase,
            previous_mode: StoreMode::Password,
            old_store_id: original.store_id.clone(),
            new_store_id: encode_id(&[2; 16]),
            phase: SetupPhase::InstallNew,
        };
        write_json(&paths.setup(), &journal).unwrap();
        if stage > 0 {
            let temporary = paths.root().join("db.tmp");
            private_directory(&temporary).unwrap();
            write_json(&temporary.join("generation.json"), &journal.replacement()).unwrap();
            if stage == 2 {
                drop(private_new_file(&temporary.join("maple.db")).unwrap());
            }
        }
        let saved = fs::read(paths.database()).unwrap();
        assert!(recover_filesystem(&paths).is_err());
        assert_eq!(fs::read(paths.database()).unwrap(), saved);
        assert_eq!(read_generation(&paths.db()).unwrap(), Some(original));
        assert!(!paths.root().join("db.old").exists());
    }
}

#[test]
fn failure_to_save_a_notice_does_not_report_a_committed_password_change_as_failed() {
    let directory = tempfile::tempdir().unwrap();
    let store = opened(directory.path());
    let old = Password::normalize("Z7jv!9qP#3nMx@8Ks^2dH").unwrap();
    let new = Password::normalize("H5gx&2qT%8mVz!4Kd@7sJ").unwrap();
    store
        .set_store_password(&old, &[], KdfParams::floor())
        .unwrap();
    store.writer.lock().unwrap().as_ref().unwrap().execute_batch(
        "CREATE TEMP TRIGGER fail_mode_notice BEFORE INSERT ON app_settings WHEN NEW.key='store.mode_notice' BEGIN SELECT RAISE(ABORT,'fixture'); END;"
    ).unwrap();
    let changed = store
        .change_store_password(&old, &new, &[], KdfParams::floor(), false)
        .unwrap();
    assert!(changed.notice_pending);
    assert_eq!(store.state(), StoreState::Open);
    let pair = KeyFilePair::load(
        &store.paths.db(),
        &database_salt(&store.paths.database()).unwrap(),
    )
    .unwrap();
    assert!(pair.primary().unlock(&new).is_ok());
    assert!(matches!(
        pair.primary().unlock(&old),
        Err(KeyError::WrongPassword)
    ));
}

#[test]
fn confirmed_reset_intent_wins_over_inflight_quarantine_and_recovers_after_sync_failure() {
    use std::sync::mpsc;
    use std::time::Duration;

    fn fail_intent_sync(_: &Path) -> io::Result<()> {
        Err(io::Error::other(
            "injected reset intent synchronization failure",
        ))
    }

    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(opened(directory.path()));
    store.upsert_account("confirmed-reset-source", 1).unwrap();
    let old_generation = read_generation(&store.paths.db()).unwrap().unwrap();
    let worker = store.checkpoint_worker.lock().unwrap().take().unwrap();
    worker
        .sender
        .send(super::super::CheckpointMessage::Stop)
        .unwrap();
    worker.thread.join().unwrap();
    let (entered, at_checkpoint) = mpsc::sync_channel(1);
    let (release_checkpoint, released) = mpsc::sync_channel(1);
    store
        .start_checkpointer_with(move |connection, truncate| {
            entered.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(10)).unwrap();
            connection::checkpoint(connection, truncate)?;
            Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
                None,
            )
            .into())
        })
        .unwrap();
    store.upsert_account("before-reset-commit", 2).unwrap();
    at_checkpoint.recv_timeout(Duration::from_secs(5)).unwrap();
    let resetting_store = Arc::clone(&store);
    let resetting = std::thread::spawn(move || {
        let _gate = resetting_store.lifecycle.lock().unwrap();
        resetting_store.begin_reset_with_intent_sync_locked(ResetAfter::Erase, fail_intent_sync)
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if store.checkpoint_worker.lock().unwrap().is_none() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "reset did not reach the old worker join"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let intent_published_before_join = store.paths.setup().exists();
    release_checkpoint.send(()).unwrap();
    assert_eq!(
        resetting.join().unwrap(),
        Err(LifecycleError::RecoveryRequired {
            mode: StoreMode::Plain
        })
    );
    assert!(!intent_published_before_join);
    let winning: SetupJournal = read_json_optional(&store.paths.setup()).unwrap().unwrap();
    assert_eq!(winning.phase, SetupPhase::RetireOld);
    assert_eq!(winning.after, ResetAfter::Erase);
    assert_eq!(winning.old_store_id, old_generation.store_id);
    assert!(store.upsert_account("must-not-be-acknowledged", 3).is_err());
    let paths = store.paths.clone();
    let committed_intent = fs::read(paths.setup()).unwrap();
    drop(store); // A process restart sees only the already-published reset intent.
    assert_eq!(fs::read(paths.setup()).unwrap(), committed_intent);
    let reopened = Store::unavailable(paths, StoreState::Maintenance).unwrap();
    reopened.initialize_recovered().unwrap();
    assert_eq!(reopened.state(), StoreState::Open);
    let replacement = read_generation(&reopened.paths.db()).unwrap().unwrap();
    assert_eq!(replacement.store_id, winning.new_store_id);
    assert_eq!(
        replacement.setup_operation_id.as_deref(),
        Some(winning.operation_id.as_str())
    );
    for user_id in [
        "confirmed-reset-source",
        "before-reset-commit",
        "must-not-be-acknowledged",
    ] {
        assert!(reopened.account_id(user_id).unwrap().is_none());
    }
    assert!(!reopened.paths.setup().exists());
}

#[test]
fn reset_retirement_recovers_each_partial_delete_without_retaining_an_archive() {
    for after in [ResetAfter::Erase, ResetAfter::StartFresh] {
        for stage in 0..5 {
            let directory = tempfile::tempdir().unwrap();
            let paths = StorePaths::new(directory.path()).prepare().unwrap();
            let old = fixture_generation(11, None);
            generation_directory(&paths.db(), &old);
            let journal = SetupJournal {
                format: SETUP_FORMAT.into(),
                operation_id: encode_id(&[12; 16]),
                after,
                previous_mode: StoreMode::Password,
                old_store_id: old.store_id,
                new_store_id: encode_id(&[13; 16]),
                phase: SetupPhase::RetireOld,
            };
            write_json(&paths.setup(), &journal).unwrap();
            let retired = paths.root().join("db.old");
            if stage >= 1 {
                fs::rename(paths.db(), &retired).unwrap();
            }
            if stage >= 2 {
                fs::remove_file(retired.join("maple.db")).unwrap();
            }
            if stage >= 3 {
                fs::remove_file(retired.join("generation.json")).unwrap();
            }
            if stage >= 4 {
                fs::remove_dir(&retired).unwrap();
            }
            let recovered = recover_filesystem(&paths).unwrap().unwrap();
            assert_eq!(recovered.phase, SetupPhase::AwaitChoice);
            assert!(!retired.exists());
            assert_eq!(
                recover_filesystem(&paths).unwrap().unwrap().phase,
                SetupPhase::AwaitChoice
            );
        }
    }
}

#[test]
fn installed_replacement_finishes_only_its_exact_partial_source_cleanup() {
    for stage in 0..4 {
        let directory = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(directory.path()).prepare().unwrap();
        let old = fixture_generation(21, None);
        let journal = SetupJournal {
            format: SETUP_FORMAT.into(),
            operation_id: encode_id(&[22; 16]),
            after: ResetAfter::StartFresh,
            previous_mode: StoreMode::Plain,
            old_store_id: old.store_id.clone(),
            new_store_id: encode_id(&[23; 16]),
            phase: SetupPhase::InstallNew,
        };
        generation_directory(&paths.db(), &journal.replacement());
        let retired = paths.root().join("db.old");
        generation_directory(&retired, &old);
        if stage >= 1 {
            fs::remove_file(retired.join("maple.db")).unwrap();
        }
        if stage >= 2 {
            fs::remove_file(retired.join("generation.json")).unwrap();
        }
        if stage >= 3 {
            fs::remove_dir(&retired).unwrap();
        }
        write_json(&paths.setup(), &journal).unwrap();
        let installed = fs::read(paths.database()).unwrap();
        assert!(recover_filesystem(&paths).unwrap().is_none());
        assert_eq!(fs::read(paths.database()).unwrap(), installed);
        assert_eq!(
            read_generation(&paths.db()).unwrap(),
            Some(journal.replacement())
        );
        assert!(!retired.exists());
        assert!(!paths.setup().exists());
        assert!(recover_filesystem(&paths).unwrap().is_none());
    }
}

#[test]
fn old_setup_formats_are_refused_before_deriving_paths_or_mutating_files() {
    for phase in [
        SetupPhase::BuildReplacement,
        SetupPhase::RetireOld,
        SetupPhase::AwaitChoice,
        SetupPhase::InstallNew,
        SetupPhase::Installed,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(directory.path()).prepare().unwrap();
        let old = fixture_generation(31, None);
        generation_directory(&paths.db(), &old);
        let journal = SetupJournal {
            format: "maple-store-setup/1".into(),
            operation_id: encode_id(&[32; 16]),
            after: ResetAfter::StartFresh,
            previous_mode: StoreMode::Plain,
            old_store_id: old.store_id.clone(),
            new_store_id: encode_id(&[33; 16]),
            phase,
        };
        let historical = paths.root().join(format!("db.damaged-{}", old.store_id));
        private_directory(&historical).unwrap();
        fs::write(historical.join("sentinel"), b"never inspect an old archive").unwrap();
        write_json(&paths.setup(), &journal).unwrap();
        let source = fs::read(paths.database()).unwrap();
        let control = fs::read(paths.setup()).unwrap();
        assert_eq!(
            recover_filesystem(&paths).unwrap_err(),
            LifecycleError::Store(StoreError::UnsupportedOperationFormat)
        );
        assert_eq!(fs::read(paths.database()).unwrap(), source);
        assert_eq!(fs::read(paths.setup()).unwrap(), control);
        assert_eq!(
            fs::read(historical.join("sentinel")).unwrap(),
            b"never inspect an old archive"
        );
        assert!(!paths.root().join("db.old").exists());
    }
}

#[test]
fn refused_reset_and_setup_return_typed_error_without_inspecting_operation_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let store = opened(directory.path());
    let reason = crate::store::RefusalReason::UnsupportedFilesystem;
    let before = fs::read(store.paths.database()).unwrap();
    let mut file = private_new_file(&store.paths.setup()).unwrap();
    file.write_all(b"{deliberately malformed operation canary")
        .unwrap();
    drop(file);
    set_state(&store, StoreState::Refused { reason }).unwrap();
    let expected = LifecycleError::Store(StoreError::Refused { reason });
    assert_eq!(
        store.erase_local_data(ERASE_CONFIRMATION).unwrap_err(),
        expected
    );
    assert_eq!(
        store.start_fresh(START_FRESH_CONFIRMATION).unwrap_err(),
        expected
    );
    assert_eq!(
        store
            .setup_store(None, &[], KdfParams::floor())
            .unwrap_err(),
        expected
    );
    let mut called = false;
    assert_eq!(
        store
            .with_authenticated_store_key(|_| {
                called = true;
                Ok(())
            })
            .unwrap_err(),
        expected
    );
    assert!(!called);
    assert_eq!(store.state(), StoreState::Refused { reason });
    assert_eq!(fs::read(store.paths.database()).unwrap(), before);
    assert_eq!(
        fs::read(store.paths.setup()).unwrap(),
        b"{deliberately malformed operation canary"
    );
}

#[test]
fn installed_reset_marker_never_discards_a_later_export_stage_with_the_same_generation() {
    let directory = tempfile::tempdir().unwrap();
    let paths = StorePaths::new(directory.path()).prepare().unwrap();
    let old = fixture_generation(51, None);
    let journal = SetupJournal {
        format: SETUP_FORMAT.into(),
        operation_id: encode_id(&[52; 16]),
        after: ResetAfter::StartFresh,
        previous_mode: StoreMode::Plain,
        old_store_id: old.store_id.clone(),
        new_store_id: encode_id(&[53; 16]),
        phase: SetupPhase::InstallNew,
    };
    generation_directory(&paths.db(), &journal.replacement());
    let retired = paths.root().join("db.old");
    generation_directory(&retired, &old);
    let future = paths.root().join("db.tmp");
    generation_directory(&future, &journal.replacement());
    let source = fs::read(retired.join("maple.db")).unwrap();
    let future_bytes = fs::read(future.join("maple.db")).unwrap();
    write_json(&paths.setup(), &journal).unwrap();
    assert_eq!(
        recover_filesystem(&paths).unwrap_err(),
        LifecycleError::Generation
    );
    assert_eq!(fs::read(future.join("maple.db")).unwrap(), future_bytes);
    assert_eq!(fs::read(retired.join("maple.db")).unwrap(), source);
    assert!(paths.setup().exists());
}

#[test]
fn private_foreign_tempish_names_are_refused_before_reset_intent_or_deletion() {
    for name in [
        ".keys-personal-copy",
        ".control-personal-copy.tmp",
        ".keys-AAAAAAAAAAAAAAAAAAAAAB.tmp",
    ] {
        for after in [ResetAfter::Erase, ResetAfter::StartFresh] {
            let directory = tempfile::tempdir().unwrap();
            let store = opened(directory.path());
            let foreign = store.paths.db().join(name);
            let mut file = private_new_file(&foreign).unwrap();
            file.write_all(b"private foreign file must survive")
                .unwrap();
            drop(file);
            let source = fs::read(store.paths.database()).unwrap();
            let generation = read_generation(&store.paths.db()).unwrap().unwrap();
            if after == ResetAfter::StartFresh {
                set_state(
                    &store,
                    StoreState::Damaged {
                        reason: DamageReason::Database,
                    },
                )
                .unwrap();
            }
            let previous = store.state();
            let result = match after {
                ResetAfter::Erase => store.erase_local_data(ERASE_CONFIRMATION),
                ResetAfter::StartFresh => store.start_fresh(START_FRESH_CONFIRMATION),
            };
            assert_eq!(result.unwrap_err(), LifecycleError::Generation);
            assert_eq!(store.state(), previous);
            assert!(!store.paths.setup().exists());
            assert!(!store.paths.root().join("db.old").exists());
            assert_eq!(fs::read(store.paths.database()).unwrap(), source);
            assert_eq!(
                read_generation(&store.paths.db()).unwrap(),
                Some(generation)
            );
            assert_eq!(
                fs::read(&foreign).unwrap(),
                b"private foreign file must survive"
            );
        }
    }
}

#[test]
fn generation_cleanup_rechecks_strict_temp_names_before_unlinking_any_file() {
    let directory = tempfile::tempdir().unwrap();
    let paths = StorePaths::new(directory.path()).prepare().unwrap();
    let retired = paths.root().join("db.old");
    let generation = fixture_generation(61, None);
    generation_directory(&retired, &generation);
    let foreign = retired.join(".keys-personal-copy");
    let mut file = private_new_file(&foreign).unwrap();
    file.write_all(b"not an owned temporary key file").unwrap();
    drop(file);
    let source = fs::read(retired.join("maple.db")).unwrap();
    assert_eq!(
        remove_generation_directory(&retired, Some(&generation)).unwrap_err(),
        LifecycleError::Generation
    );
    assert_eq!(fs::read(retired.join("maple.db")).unwrap(), source);
    assert_eq!(read_generation(&retired).unwrap(), Some(generation));
    assert_eq!(
        fs::read(foreign).unwrap(),
        b"not an owned temporary key file"
    );
}

#[test]
fn status_read_failure_does_not_reclassify_an_open_generation() {
    let directory = tempfile::tempdir().unwrap();
    let store = opened(directory.path());
    let generation = fs::read(store.paths.generation()).unwrap();
    fs::write(
        store.paths.generation(),
        b"{invalid public metadata fixture",
    )
    .unwrap();
    assert_eq!(store.lock_status().unwrap_err(), LifecycleError::Generation);
    assert_eq!(store.state(), StoreState::Open);
    fs::write(store.paths.generation(), generation).unwrap();
    assert_eq!(store.lock_status().unwrap().state, StoreState::Open);
    store
        .upsert_account("status-retry-remains-usable", 1)
        .unwrap();
}

#[test]
fn reset_busy_on_an_upload_keeps_the_store_usable_until_retry() {
    use crate::store::{AttachmentUploadMetadata, AttachmentUploadStart, NewTask, TaskKind};
    use sha2::{Digest, Sha256};
    let directory = tempfile::tempdir().unwrap();
    let store = opened(directory.path());
    let account = store.upsert_account("reset-upload", 1).unwrap();
    let task = store
        .create_task(
            account,
            &NewTask {
                uuid: "00000000-0000-7000-8000-000000000001".into(),
                project_root: if cfg!(windows) {
                    "C:/reset-upload"
                } else {
                    "/reset-upload"
                }
                .into(),
                kind: TaskKind::Desktop,
                title: "Upload".into(),
                provisional: true,
                model: None,
                created_ms: 1,
            },
        )
        .unwrap();
    let AttachmentUploadStart::Writing(mut upload) = store
        .begin_attachment_upload(
            account,
            &task.uuid,
            "reset-active-upload",
            &AttachmentUploadMetadata {
                name: "file.txt".into(),
                mime_type: "text/plain".into(),
                size: 3,
                sha256: format!("{:x}", Sha256::digest(b"abc")),
            },
        )
        .unwrap()
    else {
        panic!("fresh upload unexpectedly complete");
    };
    upload.write_chunk(b"a").unwrap();
    assert_eq!(
        store.erase_local_data(ERASE_CONFIRMATION),
        Err(LifecycleError::Store(StoreError::Busy))
    );
    assert_eq!(store.state(), StoreState::Open);
    assert_eq!(store.account_id("reset-upload").unwrap(), Some(account));
    assert!(!store.paths.setup().exists());
    assert_eq!(upload.write_chunk(b"bc"), Err(StoreError::Conflict));
    drop(upload);
    store.erase_local_data(ERASE_CONFIRMATION).unwrap();
    assert_eq!(store.state(), StoreState::Open);
    assert_eq!(store.account_id("reset-upload").unwrap(), None);
}
