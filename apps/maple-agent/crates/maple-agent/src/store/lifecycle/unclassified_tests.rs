use super::*;

fn healthy(path: &Path) -> Store {
    let store = Store::unavailable(StorePaths::new(path), StoreState::Maintenance).unwrap();
    store.initialize_recovered().unwrap();
    store
        .upsert_account("current-new-store-private-fixture", 1)
        .unwrap();
    store
}

fn unqualified(path: &Path) -> Store {
    let store = healthy(path);
    store
        .writer
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .execute("DROP TABLE store_meta", [])
        .unwrap();
    store.close().unwrap();
    fs::write(store.paths.generation(), b"{corrupted public generation").unwrap();
    drop(store);
    let store = Store::unavailable(StorePaths::new(path), StoreState::Maintenance).unwrap();
    assert!(store.initialize_recovered().is_err());
    let status = store.lock_status().unwrap();
    assert_eq!(
        status.state,
        StoreState::Damaged {
            reason: DamageReason::Generation
        }
    );
    assert!(status.store_id.is_none());
    store
}

fn fixture_journal(paths: &StorePaths, after: ResetAfter) -> Journal {
    Journal {
        format: FORMAT.into(),
        operation_id: encode_id(&[1; 16]),
        new_store_id: encode_id(&[3; 16]),
        after,
        previous_mode: StoreMode::Password,
        phase: Phase::RetireOriginal,
        source: DirectoryManifest::capture(&paths.db()).unwrap(),
        attachments: attachments::root_identity(paths).unwrap(),
    }
}

#[test]
fn confirmed_start_fresh_discards_only_the_manifest_qualified_current_source() {
    let directory = tempfile::tempdir().unwrap();
    let outside = directory.path().join("unrelated-previous-data");
    fs::write(&outside, b"leave all previous data untouched").unwrap();
    let store = unqualified(directory.path());
    store.start_fresh(START_FRESH_CONFIRMATION).unwrap();
    assert_eq!(store.state(), StoreState::Open);
    assert!(
        store
            .account_id("current-new-store-private-fixture")
            .unwrap()
            .is_none()
    );
    assert!(store.lock_status().unwrap().store_id.is_some());
    assert!(!store.paths.root().join("db.old").exists());
    assert!(!intent_path(&store.paths).exists());
    assert_eq!(
        fs::read(outside).unwrap(),
        b"leave all previous data untouched"
    );
}

#[test]
fn an_uncertain_reset_intent_never_admits_new_writes_and_startup_finishes_erase() {
    fn fail(_: &Path) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::Other))
    }
    let directory = tempfile::tempdir().unwrap();
    let store = unqualified(directory.path());
    let before = fs::read(store.paths.database()).unwrap();
    {
        let _gate = store.lifecycle.lock().unwrap();
        assert_eq!(
            store
                .begin_unclassified_with_sync_locked(ResetAfter::Erase, fail)
                .unwrap_err(),
            LifecycleError::RecoveryRequired {
                mode: StoreMode::Plain
            }
        );
    }
    assert_eq!(store.state(), StoreState::Maintenance);
    assert!(store.upsert_account("must-not-be-acknowledged", 2).is_err());
    assert_eq!(fs::read(store.paths.database()).unwrap(), before);
    assert!(intent_path(&store.paths).exists());
    drop(store);
    let store =
        Store::unavailable(StorePaths::new(directory.path()), StoreState::Maintenance).unwrap();
    store.initialize_recovered().unwrap();
    assert_eq!(store.state(), StoreState::Open);
    assert!(
        store
            .account_id("must-not-be-acknowledged")
            .unwrap()
            .is_none()
    );
    assert!(!intent_path(&store.paths).exists());
    assert!(!store.paths.root().join("db.old").exists());
}

#[test]
fn retirement_recovers_before_and_after_rename_without_reclassifying_the_source() {
    for after_rename in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let store = unqualified(directory.path());
        let paths = store.paths.clone();
        let journal = fixture_journal(&paths, ResetAfter::StartFresh);
        write_json(&intent_path(&paths), &journal).unwrap();
        if after_rename {
            fs::rename(paths.db(), journal.retired_source(&paths)).unwrap();
            private_directory(&paths.db()).unwrap();
        }
        let recovered = recover(&paths).unwrap().unwrap();
        assert_eq!(recovered.phase, Phase::AwaitChoice);
        assert!(!journal.retired_source(&paths).exists());
        assert!(
            status(&paths, StoreState::NeedsSetup)
                .unwrap()
                .unwrap()
                .store_id
                .is_none()
        );
        assert_eq!(recover(&paths).unwrap().unwrap(), recovered);
    }
}

#[test]
fn partial_erase_resumes_only_for_surviving_manifest_files() {
    let directory = tempfile::tempdir().unwrap();
    let store = unqualified(directory.path());
    let paths = &store.paths;
    let journal = fixture_journal(paths, ResetAfter::Erase);
    write_json(&intent_path(paths), &journal).unwrap();
    fs::rename(paths.db(), journal.retired_source(paths)).unwrap();
    fs::remove_file(journal.retired_source(paths).join("maple.db")).unwrap();
    let recovered = recover(paths).unwrap().unwrap();
    assert_eq!(recovered.phase, Phase::AwaitChoice);
    assert!(!journal.retired_source(paths).exists());
}

#[test]
fn foreign_staging_is_rejected_before_any_original_byte_is_retired() {
    let directory = tempfile::tempdir().unwrap();
    let store = unqualified(directory.path());
    let paths = &store.paths;
    let journal = fixture_journal(paths, ResetAfter::Erase);
    write_json(&intent_path(paths), &journal).unwrap();
    let new = paths.root().join("db.new");
    private_directory(&new).unwrap();
    write_json(
        &new.join("generation.json"),
        &Generation::new([9; 16], None),
    )
    .unwrap();
    drop(private_new_file(&new.join("maple.db")).unwrap());
    let before = fs::read(paths.database()).unwrap();
    assert!(recover(paths).is_err());
    assert_eq!(fs::read(paths.database()).unwrap(), before);
    assert!(!journal.retired_source(paths).exists());
}

#[test]
fn a_stale_unclassified_intent_preserves_later_writes_to_the_installed_generation() {
    fn fail(_: &Path) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::Other))
    }
    let directory = tempfile::tempdir().unwrap();
    let store = unqualified(directory.path());
    {
        let _gate = store.lifecycle.lock().unwrap();
        assert!(
            store
                .begin_unclassified_with_sync_locked(ResetAfter::StartFresh, fail)
                .is_err()
        );
    }
    let journal = pending(&store.paths).unwrap().unwrap();
    drop(store);
    let store =
        Store::unavailable(StorePaths::new(directory.path()), StoreState::Maintenance).unwrap();
    store.initialize_recovered().unwrap();
    store.upsert_account("new-history-must-survive", 2).unwrap();
    write_json(&intent_path(&store.paths), &journal).unwrap();
    drop(store);
    let store =
        Store::unavailable(StorePaths::new(directory.path()), StoreState::Maintenance).unwrap();
    store.initialize_recovered().unwrap();
    assert!(
        store
            .account_id("new-history-must-survive")
            .unwrap()
            .is_some()
    );
    assert_eq!(
        store.lock_status().unwrap().store_id.as_deref(),
        Some(journal.new_store_id.as_str())
    );
    assert!(!intent_path(&store.paths).exists());
}

#[test]
fn corrupt_generation_unlock_repairs_only_after_the_authoritative_password_authenticates() {
    let directory = tempfile::tempdir().unwrap();
    let store = healthy(directory.path());
    let old = Password::normalize("Z7jv!9qP#3nMx@8Ks^2dH").unwrap();
    let current = Password::normalize("H5gx&2qT%8mVz!4Kd@7sJ").unwrap();
    store
        .set_store_password(&old, &[], KdfParams::floor())
        .unwrap();
    let salt = database_salt(&store.paths.database()).unwrap();
    let previous = KeyFilePair::load(&store.paths.db(), &salt)
        .unwrap()
        .primary()
        .to_json()
        .unwrap();
    let id = store.store_id().unwrap();
    store
        .change_store_password(&old, &current, &[], KdfParams::floor(), false)
        .unwrap();
    store.close().unwrap();
    fs::write(store.paths.generation(), b"{corrupted metadata").unwrap();
    fs::write(store.paths.db().join("keys.backup.json"), previous).unwrap();
    let paths = store.paths.clone();
    drop(store);
    let store = Store::unavailable(paths.clone(), StoreState::Maintenance).unwrap();
    assert!(store.initialize_recovered().is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(paths.db(), fs::Permissions::from_mode(0o500)).unwrap();
    }
    store.validate_unlock_slot().unwrap();
    assert!(matches!(
        store.unlock_store(&old, None),
        Err(LifecycleError::Key(KeyError::WrongPassword))
    ));
    assert_eq!(
        fs::read(paths.generation()).unwrap(),
        b"{corrupted metadata"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(paths.db(), fs::Permissions::from_mode(0o700)).unwrap();
    }
    store.unlock_store(&current, None).unwrap();
    assert_eq!(store.state(), StoreState::Open);
    assert_eq!(store.store_id().unwrap(), id);
    assert!(
        store
            .account_id("current-new-store-private-fixture")
            .unwrap()
            .is_some()
    );
}

#[test]
fn uncertain_encryption_never_silently_creates_a_plain_replacement() {
    for payload in [b"".as_slice(), b"unreadable encryption state".as_slice()] {
        let directory = tempfile::tempdir().unwrap();
        let store = unqualified(directory.path());
        let mut file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(store.paths.database())
            .unwrap();
        file.write_all(payload).unwrap();
        drop(file);
        store.start_fresh(START_FRESH_CONFIRMATION).unwrap();
        assert_eq!(store.state(), StoreState::NeedsSetup);
        let status = store.lock_status().unwrap();
        assert_eq!(status.mode, StoreMode::Password);
        assert!(status.store_id.is_none());
        assert!(status.setup_operation_id.is_some());
        store.setup_store(None, &[], KdfParams::floor()).unwrap();
        assert_eq!(store.state(), StoreState::Open);
        assert_eq!(store.lock_status().unwrap().mode, StoreMode::Plain);
    }
}

#[test]
fn a_stale_erase_intent_never_deletes_retained_copies_created_after_installation() {
    fn fail(_: &Path) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::Other))
    }
    let directory = tempfile::tempdir().unwrap();
    let store = unqualified(directory.path());
    {
        let _gate = store.lifecycle.lock().unwrap();
        assert!(
            store
                .begin_unclassified_with_sync_locked(ResetAfter::Erase, fail)
                .is_err()
        );
    }
    let old = pending(&store.paths).unwrap().unwrap();
    drop(store);
    let store =
        Store::unavailable(StorePaths::new(directory.path()), StoreState::Maintenance).unwrap();
    store.initialize_recovered().unwrap();
    let future = store
        .paths
        .root()
        .join(format!("db.damaged-{}", encode_id(&[8; 16])));
    private_directory(&future).unwrap();
    write_json(
        &future.join("generation.json"),
        &Generation::new([8; 16], None),
    )
    .unwrap();
    let mut file = private_new_file(&future.join("maple.db")).unwrap();
    file.write_all(b"later retained copy must survive").unwrap();
    drop(file);
    write_json(&intent_path(&store.paths), &old).unwrap();
    drop(store);
    let store =
        Store::unavailable(StorePaths::new(directory.path()), StoreState::Maintenance).unwrap();
    store.initialize_recovered().unwrap();
    assert_eq!(store.state(), StoreState::Open);
    assert_eq!(
        fs::read(future.join("maple.db")).unwrap(),
        b"later retained copy must survive"
    );
}

#[test]
fn a_stale_start_fresh_intent_does_not_require_an_already_deleted_copy() {
    fn fail(_: &Path) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::Other))
    }
    let directory = tempfile::tempdir().unwrap();
    let store = unqualified(directory.path());
    {
        let _gate = store.lifecycle.lock().unwrap();
        assert!(
            store
                .begin_unclassified_with_sync_locked(ResetAfter::StartFresh, fail)
                .is_err()
        );
    }
    let old = pending(&store.paths).unwrap().unwrap();
    drop(store);
    let store =
        Store::unavailable(StorePaths::new(directory.path()), StoreState::Maintenance).unwrap();
    store.initialize_recovered().unwrap();
    store.upsert_account("later-history", 2).unwrap();
    write_json(&intent_path(&store.paths), &old).unwrap();
    drop(store);
    let store =
        Store::unavailable(StorePaths::new(directory.path()), StoreState::Maintenance).unwrap();
    store.initialize_recovered().unwrap();
    assert!(store.account_id("later-history").unwrap().is_some());
    assert!(!intent_path(&store.paths).exists());
}

#[test]
fn old_unclassified_operation_format_is_refused_before_decoding_its_old_payload() {
    for phase in [Phase::RetireOriginal, Phase::AwaitChoice, Phase::InstallNew] {
        let directory = tempfile::tempdir().unwrap();
        let store = unqualified(directory.path());
        let paths = &store.paths;
        let mut journal = fixture_journal(paths, ResetAfter::StartFresh);
        journal.phase = phase;
        let mut value = serde_json::to_value(journal).unwrap();
        value["format"] = serde_json::json!("maple-store-unclassified-reset/1");
        value["copy_id"] = serde_json::json!(encode_id(&[42; 16]));
        write_json(&intent_path(paths), &value).unwrap();
        let source = fs::read(paths.database()).unwrap();
        let intent = fs::read(intent_path(paths)).unwrap();
        assert_eq!(
            recover(paths).unwrap_err(),
            LifecycleError::Store(StoreError::UnsupportedOperationFormat)
        );
        assert_eq!(fs::read(paths.database()).unwrap(), source);
        assert_eq!(fs::read(intent_path(paths)).unwrap(), intent);
        assert!(!paths.root().join("db.old").exists());
    }
}

#[test]
fn an_installed_unclassified_marker_never_deletes_later_same_generation_staging() {
    fn fail(_: &Path) -> io::Result<()> {
        Err(io::ErrorKind::Other.into())
    }
    let directory = tempfile::tempdir().unwrap();
    let store = unqualified(directory.path());
    {
        let _gate = store.lifecycle.lock().unwrap();
        assert!(
            store
                .begin_unclassified_with_sync_locked(ResetAfter::StartFresh, fail)
                .is_err()
        );
    }
    let old = pending(&store.paths).unwrap().unwrap();
    drop(store);
    let store =
        Store::unavailable(StorePaths::new(directory.path()), StoreState::Maintenance).unwrap();
    store.initialize_recovered().unwrap();
    let paths = store.paths.clone();
    store.close().unwrap();
    drop(store);
    let future = paths.root().join("db.tmp");
    private_directory(&future).unwrap();
    write_json(&future.join("generation.json"), &old.replacement()).unwrap();
    let mut file = private_new_file(&future.join("maple.db")).unwrap();
    file.write_all(b"a later uncommitted export must survive")
        .unwrap();
    drop(file);
    write_json(&intent_path(&paths), &old).unwrap();
    assert_eq!(recover(&paths).unwrap_err(), LifecycleError::Generation);
    assert_eq!(
        fs::read(future.join("maple.db")).unwrap(),
        b"a later uncommitted export must survive"
    );
    assert!(intent_path(&paths).exists());
}

#[test]
fn installed_generation_wins_over_reused_source_directory_identity() {
    fn fail(_: &Path) -> io::Result<()> {
        Err(io::ErrorKind::Other.into())
    }
    let directory = tempfile::tempdir().unwrap();
    let store = unqualified(directory.path());
    {
        let _gate = store.lifecycle.lock().unwrap();
        assert!(
            store
                .begin_unclassified_with_sync_locked(ResetAfter::Erase, fail)
                .is_err()
        );
    }
    let old = pending(&store.paths).unwrap().unwrap();
    drop(store);
    let store =
        Store::unavailable(StorePaths::new(directory.path()), StoreState::Maintenance).unwrap();
    store.initialize_recovered().unwrap();
    let installed = store.store_id().unwrap();
    store
        .upsert_account("later-acknowledged-history", 2)
        .unwrap();
    let installed_directory =
        super::super::unclassified_identity::capture_path(&store.paths.db(), true)
            .unwrap()
            .0;
    let mut replay = serde_json::to_value(old).unwrap();
    // Model lawful inode/file-ID reuse deterministically. Only the directory
    // identity aliases the installed replacement; original file identities and
    // lengths remain the captured old source, never the replacement's files.
    replay["source"]["directory"] = serde_json::to_value(installed_directory).unwrap();
    write_json(&intent_path(&store.paths), &replay).unwrap();
    drop(store);

    let store =
        Store::unavailable(StorePaths::new(directory.path()), StoreState::Maintenance).unwrap();
    store.initialize_recovered().unwrap();
    assert_eq!(store.state(), StoreState::Open);
    assert_eq!(store.store_id().unwrap(), installed);
    assert!(
        store
            .account_id("later-acknowledged-history")
            .unwrap()
            .is_some()
    );
    assert!(!intent_path(&store.paths).exists());
    assert!(!store.paths.root().join("db.old").exists());
}

#[test]
fn unclassified_reset_removes_only_its_captured_attachment_root() {
    let directory = tempfile::tempdir().unwrap();
    let store = unqualified(directory.path());
    let root = store.paths.root().join("attachments");
    private_directory(&root).unwrap();
    let nested = root.join("captured");
    private_directory(&nested).unwrap();
    let mut file = private_new_file(&nested.join("file.txt")).unwrap();
    file.write_all(b"owned upload").unwrap();
    drop(file);
    store.start_fresh(START_FRESH_CONFIRMATION).unwrap();
    assert_eq!(store.state(), StoreState::Open);
    assert!(!root.exists());
}

#[test]
fn unclassified_reset_refuses_a_replaced_attachment_root() {
    let directory = tempfile::tempdir().unwrap();
    let store = unqualified(directory.path());
    let root = store.paths.root().join("attachments");
    private_directory(&root).unwrap();
    let journal = fixture_journal(&store.paths, ResetAfter::Erase);
    write_json(&intent_path(&store.paths), &journal).unwrap();
    let original = directory.path().join("preserved-original-attachments");
    fs::rename(&root, &original).unwrap();
    private_directory(&root).unwrap();
    let mut file = private_new_file(&root.join("other-file")).unwrap();
    file.write_all(b"do not delete replacement").unwrap();
    drop(file);
    assert!(matches!(
        recover(&store.paths),
        Err(LifecycleError::Store(StoreError::UnsafePath))
    ));
    assert_eq!(
        fs::read(root.join("other-file")).unwrap(),
        b"do not delete replacement"
    );
    assert!(original.exists());
    assert!(store.paths.database().exists());
}
