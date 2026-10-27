//! Native filesystem transaction tests. Fixtures contain ordinary data files;
//! these tests do not waive Authenticode, verify installation admission, or run
//! an executable. Production admission and signature checks remain separate.
#![cfg(windows)]

use super::*;
use std::cell::Cell;

struct TransactionFixture {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    staged: PathBuf,
    receipt: PathBuf,
    previous: Current,
    next: Current,
    previous_nodes: BTreeMap<String, Node>,
    next_nodes: BTreeMap<String, Node>,
}

impl TransactionFixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("installation");
        windows_fs::private_directory(&root).unwrap();
        windows_fs::private_directory(&root.join("versions")).unwrap();
        let previous = Current {
            format: POINTER_FORMAT.into(),
            generation: "0.0.9-701".into(),
        };
        let next = Current {
            format: POINTER_FORMAT.into(),
            generation: "0.0.10-702".into(),
        };
        let previous_path = previous.path(&root);
        fixture_tree(&previous_path, "0.0.9", "701");
        let previous_nodes = scan_generation(&previous_path).unwrap();
        write_private_json(&root.join(POINTER), &previous).unwrap();

        let stage = root.join("transaction-stage");
        windows_fs::private_directory(&stage).unwrap();
        let staged = stage.join(ROOT);
        fixture_tree(&staged, "0.0.10", "702");
        let next_nodes = scan_generation(&staged).unwrap();
        let receipt = stage.join(RECEIPT_NAME);
        durable_file(&receipt, b"durable transaction receipt fixture\n");
        Self {
            _temporary: temporary,
            root,
            staged,
            receipt,
            previous,
            next,
            previous_nodes,
            next_nodes,
        }
    }

    fn current(&self) -> Current {
        Current::parse(&read_bounded(&self.root.join(POINTER), 1024).unwrap()).unwrap()
    }

    fn assert_previous_untouched(&self) {
        assert_eq!(self.current(), self.previous);
        assert_eq!(
            scan_generation(&self.previous.path(&self.root)).unwrap(),
            self.previous_nodes
        );
    }

    fn assert_unpublished_retry_state(&self) {
        self.assert_previous_untouched();
        assert!(!self.next.path(&self.root).exists());
        assert_eq!(scan_generation(&self.staged).unwrap(), self.next_nodes);
        assert!(!self.receipt.exists());
    }

    fn assert_published(&self) {
        assert_eq!(self.current(), self.next);
        assert_eq!(
            scan_generation(&self.next.path(&self.root)).unwrap(),
            self.next_nodes
        );
        assert_eq!(
            scan_generation(&self.previous.path(&self.root)).unwrap(),
            self.previous_nodes
        );
        assert!(!self.staged.exists());
        assert_eq!(
            read_bounded(&self.receipt, 1024).unwrap(),
            b"durable transaction receipt fixture\n"
        );
    }
}

fn durable_file(path: &Path, bytes: &[u8]) {
    let mut file = windows_fs::open_private(path, true).unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
}

fn fixture_tree(path: &Path, version: &str, build: &str) {
    for directory in DIRS {
        windows_fs::private_directory(&path.join(directory)).unwrap();
    }
    assert_eq!(FILES.len(), 9);
    let bundle = Bundle {
        format: "maple-agent-windows-bundle-v1".into(),
        product: "maple-agent".into(),
        profile: "prod".into(),
        version: version.into(),
        build: build.into(),
        source_sha: "a".repeat(40),
        bundle_id: profile_identity("prod").unwrap().into(),
        entrypoint: "maple-agent.exe".into(),
    };
    for name in FILES {
        let bytes = if *name == DESCRIPTOR {
            serde_json::to_vec(&bundle).unwrap()
        } else {
            format!("ordinary transaction fixture {version}/{build}: {name}\n").into_bytes()
        };
        durable_file(&path.join(name), &bytes);
    }
}

#[test]
fn native_transaction_commits_pointer_and_preserves_old_generation() {
    let fixture = TransactionFixture::new();
    // Deny writes and deletion of the old exe throughout publication, as an
    // active Windows image does. This is a file-share restriction, not proof of
    // executing an app; no binary fixture is launched by this test.
    let old_executable =
        windows_fs::open_readonly(&fixture.previous.path(&fixture.root).join("maple-agent.exe"))
            .unwrap();
    let reached_publish = Cell::new(false);
    commit_generation(
        &fixture.root,
        &fixture.staged,
        &fixture.next,
        &fixture.receipt,
        || {
            reached_publish.set(true);
            fixture.assert_previous_untouched();
            assert!(!fixture.staged.exists());
            assert!(fixture.receipt.exists());
            assert_eq!(
                scan_generation(&fixture.next.path(&fixture.root)).unwrap(),
                fixture.next_nodes
            );
            Ok(())
        },
    )
    .unwrap();
    assert!(reached_publish.get());
    fixture.assert_published();
    assert!(old_executable.metadata().unwrap().is_file());
}

#[test]
fn native_transaction_interruption_before_pointer_keeps_previous() {
    let fixture = TransactionFixture::new();
    let reached_publish = Cell::new(false);
    let result = commit_generation(
        &fixture.root,
        &fixture.staged,
        &fixture.next,
        &fixture.receipt,
        || {
            reached_publish.set(true);
            fixture.assert_previous_untouched();
            assert!(!fixture.staged.exists());
            assert!(fixture.next.path(&fixture.root).is_dir());
            assert!(fixture.receipt.exists());
            Err(InstallError::Invalid("injected before pointer publication"))
        },
    );
    assert!(matches!(
        result,
        Err(InstallError::Invalid("injected before pointer publication"))
    ));
    assert!(reached_publish.get());
    fixture.assert_unpublished_retry_state();
}

#[test]
fn native_transaction_pointer_lock_failure_is_retryable() {
    let fixture = TransactionFixture::new();
    // FILE_SHARE_READ excludes deletion and therefore makes atomic replacement
    // fail on native Windows. Releasing this real handle must permit a retry.
    let locked_pointer = windows_fs::open_readonly(&fixture.root.join(POINTER)).unwrap();
    let reached_publish = Cell::new(false);
    let result = commit_generation(
        &fixture.root,
        &fixture.staged,
        &fixture.next,
        &fixture.receipt,
        || {
            reached_publish.set(true);
            Ok(())
        },
    );
    assert!(result.is_err());
    assert!(reached_publish.get());
    fixture.assert_unpublished_retry_state();
    drop(locked_pointer);

    // The production replacement path writes a fresh durable receipt before
    // every commit attempt; reproduce that contract for the same staged bytes.
    durable_file(&fixture.receipt, b"durable transaction receipt fixture\n");
    commit_generation(
        &fixture.root,
        &fixture.staged,
        &fixture.next,
        &fixture.receipt,
        || Ok(()),
    )
    .unwrap();
    fixture.assert_published();
}

#[test]
fn native_install_lock_rejects_concurrent_writers() {
    let fixture = TransactionFixture::new();
    let path = fixture.root.join(".install.lock");
    let first = windows_fs::open_private(&path, false).unwrap();
    let second = windows_fs::open_private(&path, false).unwrap();
    fs2::FileExt::try_lock_exclusive(&first).unwrap();
    assert!(fs2::FileExt::try_lock_exclusive(&second).is_err());
    fs2::FileExt::unlock(&first).unwrap();
    fs2::FileExt::try_lock_exclusive(&second).unwrap();
    fs2::FileExt::unlock(&second).unwrap();
}

fn write_manual_intent(root: &Path, bundle: &Bundle, executable: &Path) -> Vec<u8> {
    let intent = ManualIntent {
        format: "maple-agent-windows-installer-intent-v1".into(),
        profile: bundle.profile.clone(),
        generation: bundle.generation(),
        version: bundle.version.clone(),
        source_sha: bundle.source_sha.clone(),
        executable_sha256: exact_file_digest(executable).unwrap(),
        // Zero identifies a stale owner for the recovery fixtures below. The
        // live-owner test replaces these with actual process identity values.
        owner_pid: 0,
        owner_created: 0,
    };
    write_private_json(&root.join(MANUAL_INTENT), &intent).unwrap();
    read_bounded(&root.join(MANUAL_INTENT), DESCRIPTOR_LIMIT).unwrap()
}

#[test]
fn native_pending_manual_intent_preserves_current_generation() {
    let fixture = TransactionFixture::new();
    let previous_bundle = descriptor(&fixture.previous.path(&fixture.root), "prod").unwrap();
    let next_bundle = descriptor(&fixture.staged, "prod").unwrap();
    let intent_bytes = write_manual_intent(
        &fixture.root,
        &next_bundle,
        &fixture.staged.join("maple-agent.exe"),
    );

    // Exercise only recovery's filesystem decision after the production caller
    // has verified its actual current signature and tree. Ordinary fixture
    // bytes provide no signed-launch or full installer admission evidence.
    recover_completed_manual_intent(&fixture.root, &previous_bundle).unwrap();
    fixture.assert_previous_untouched();
    assert_eq!(
        read_bounded(&fixture.root.join(MANUAL_INTENT), DESCRIPTOR_LIMIT).unwrap(),
        intent_bytes
    );
    assert!(!fixture.next.path(&fixture.root).exists());
    assert_eq!(
        scan_generation(&fixture.staged).unwrap(),
        fixture.next_nodes
    );
    assert!(fixture.receipt.exists());
}

#[test]
fn native_completed_manual_intent_recovers_after_pointer_commit() {
    let fixture = TransactionFixture::new();
    let next_bundle = descriptor(&fixture.staged, "prod").unwrap();
    let intent_bytes = write_manual_intent(
        &fixture.root,
        &next_bundle,
        &fixture.staged.join("maple-agent.exe"),
    );
    commit_generation(
        &fixture.root,
        &fixture.staged,
        &fixture.next,
        &fixture.receipt,
        || Ok(()),
    )
    .unwrap();
    fixture.assert_published();
    assert!(fixture.root.join(MANUAL_INTENT).is_file());

    // Model interruption after pointer publication and before intent deletion.
    // Signature/tree admission remains the real launcher's prerequisite; this
    // calls only its recovery helper and checks exact identity/digest matching.
    let current_bundle = descriptor(&fixture.next.path(&fixture.root), "prod").unwrap();
    let mut changed: ManualIntent = serde_json::from_slice(&intent_bytes).unwrap();
    changed.executable_sha256 = "0".repeat(64);
    write_private_json(&fixture.root.join(MANUAL_INTENT), &changed).unwrap();
    let changed_bytes = read_bounded(&fixture.root.join(MANUAL_INTENT), DESCRIPTOR_LIMIT).unwrap();
    recover_completed_manual_intent(&fixture.root, &current_bundle).unwrap();
    assert_eq!(
        read_bounded(&fixture.root.join(MANUAL_INTENT), DESCRIPTOR_LIMIT).unwrap(),
        changed_bytes
    );
    fixture.assert_published();

    // Matching version/source alone cannot clear another executable's intent.
    durable_file(&fixture.root.join(MANUAL_INTENT), &intent_bytes);
    recover_completed_manual_intent(&fixture.root, &current_bundle).unwrap();
    assert!(!fixture.root.join(MANUAL_INTENT).exists());
    fixture.assert_published();

    // Repeated startup recovery is a harmless no-op once collection completed.
    recover_completed_manual_intent(&fixture.root, &current_bundle).unwrap();
    fixture.assert_published();
}

fn current_process_owner() -> (u32, u64) {
    let pid = unsafe { windows_sys::Win32::System::Threading::GetCurrentProcessId() };
    let created = process_created(pid)
        .unwrap()
        .expect("the native test process must still be live");
    (pid, created)
}

#[test]
fn native_live_matching_manual_intent_is_never_cleared() {
    let fixture = TransactionFixture::new();
    let current = descriptor(&fixture.previous.path(&fixture.root), "prod").unwrap();
    let bytes = write_manual_intent(
        &fixture.root,
        &current,
        &fixture.previous.path(&fixture.root).join("maple-agent.exe"),
    );
    let mut intent: ManualIntent = serde_json::from_slice(&bytes).unwrap();
    let owner = current_process_owner();
    intent.owner_pid = owner.0;
    intent.owner_created = owner.1;
    write_private_json(&fixture.root.join(MANUAL_INTENT), &intent).unwrap();
    let before = read_bounded(&fixture.root.join(MANUAL_INTENT), DESCRIPTOR_LIMIT).unwrap();

    // Even an exact current generation cannot clear an intent owned by a live
    // installer. This exercises process identity and recovery, not admission.
    recover_completed_manual_intent(&fixture.root, &current).unwrap();
    assert_eq!(
        read_bounded(&fixture.root.join(MANUAL_INTENT), DESCRIPTOR_LIMIT).unwrap(),
        before
    );
    fixture.assert_previous_untouched();
    assert_eq!(
        scan_generation(&fixture.staged).unwrap(),
        fixture.next_nodes
    );
}

#[test]
fn native_uninstall_owner_fences_until_nsis_finishes() {
    let fixture = TransactionFixture::new();
    let marker = fixture.root.join(".uninstaller-intent.json");
    let owner = current_process_owner();
    let mut intent = UninstallIntent {
        format: "maple-agent-windows-uninstall-intent-v1".into(),
        profile: "prod".into(),
        owner_pid: owner.0,
        owner_created: owner.1,
        generations: vec![fixture.previous.generation.clone()],
    };
    write_private_json(&marker, &intent).unwrap();
    let before = read_bounded(&marker, DESCRIPTOR_LIMIT).unwrap();
    assert!(live_uninstall_owner(&fixture.root, "prod").unwrap());
    assert!(live_uninstall_owner(&fixture.root, "dev").is_err());
    assert_eq!(read_bounded(&marker, DESCRIPTOR_LIMIT).unwrap(), before);
    fixture.assert_previous_untouched();

    // PID reuse cannot adopt an old marker: creation time must match too.
    intent.owner_created ^= 1;
    write_private_json(&marker, &intent).unwrap();
    assert!(!live_uninstall_owner(&fixture.root, "prod").unwrap());
    assert!(marker.is_file());

    intent.owner_pid = 0;
    intent.owner_created = 0;
    write_private_json(&marker, &intent).unwrap();
    let stale = read_bounded(&marker, DESCRIPTOR_LIMIT).unwrap();
    assert!(!live_uninstall_owner(&fixture.root, "prod").unwrap());
    assert_eq!(read_bounded(&marker, DESCRIPTOR_LIMIT).unwrap(), stale);
    fixture.assert_previous_untouched();

    // Ownership inspection never removes NSIS's durable marker on its behalf.
    // Model NSIS completing its own outer transaction explicitly.
    fs::remove_file(&marker).unwrap();
    assert!(!live_uninstall_owner(&fixture.root, "prod").unwrap());
    fixture.assert_previous_untouched();
}

#[test]
fn native_partial_generation_removal_preserves_unknown_files() {
    let fixture = TransactionFixture::new();
    fs::remove_file(fixture.staged.join("LICENSE.txt")).unwrap();
    fs::remove_file(fixture.staged.join("notices/THIRD_PARTY_NOTICES.html")).unwrap();
    let guard = windows_fs::open_executable_mutation_guard(&fixture.staged.join("maple-agent.exe"))
        .unwrap();
    remove_generation_files(&fixture.staged, Some(guard)).unwrap();
    assert!(!fixture.staged.exists());
    fixture.assert_previous_untouched();

    let unknown = fixture.root.join("versions").join("0.0.11-703");
    fixture_tree(&unknown, "0.0.11", "703");
    durable_file(&unknown.join("my-project.txt"), b"user-owned data\n");
    let before: Vec<_> = FILES
        .iter()
        .copied()
        .chain(std::iter::once("my-project.txt"))
        .map(|name| (name, fs::read(unknown.join(name)).unwrap()))
        .collect();
    let guard =
        windows_fs::open_executable_mutation_guard(&unknown.join("maple-agent.exe")).unwrap();
    // Removal itself must prevalidate every entry, even if its caller forgot.
    assert!(remove_generation_files(&unknown, Some(guard)).is_err());
    for (name, bytes) in before {
        assert_eq!(fs::read(unknown.join(name)).unwrap(), bytes);
    }
    fixture.assert_previous_untouched();
}

#[test]
fn native_retained_guard_removes_owned_generation_without_partial_pointer_change() {
    let fixture = TransactionFixture::new();
    commit_generation(
        &fixture.root,
        &fixture.staged,
        &fixture.next,
        &fixture.receipt,
        || Ok(()),
    )
    .unwrap();
    fixture.assert_published();
    let pointer_before = read_bounded(&fixture.root.join(POINTER), 1024).unwrap();
    let current_pointer_guard = windows_fs::open_readonly(&fixture.root.join(POINTER)).unwrap();
    let previous = fixture.previous.path(&fixture.root);
    let guard =
        windows_fs::open_executable_mutation_guard(&previous.join("maple-agent.exe")).unwrap();
    remove_generation_files(&previous, Some(guard)).unwrap();
    assert!(!previous.exists());
    assert_eq!(
        read_bounded(&fixture.root.join(POINTER), 1024).unwrap(),
        pointer_before
    );
    assert_eq!(fixture.current(), fixture.next);
    assert_eq!(
        scan_generation(&fixture.next.path(&fixture.root)).unwrap(),
        fixture.next_nodes
    );
    assert!(fixture.receipt.is_file());
    assert!(current_pointer_guard.metadata().unwrap().is_file());
}

#[test]
fn native_dead_uninstall_journal_allows_only_owned_partial_repair() {
    let fixture = TransactionFixture::new();
    let previous = fixture.previous.path(&fixture.root);
    let pointer_before = read_bounded(&fixture.root.join(POINTER), 1024).unwrap();
    let guard =
        windows_fs::open_executable_mutation_guard(&previous.join("maple-agent.exe")).unwrap();
    remove_generation_files(&previous, Some(guard)).unwrap();
    let intent = UninstallIntent {
        format: "maple-agent-windows-uninstall-intent-v1".into(),
        profile: "prod".into(),
        owner_pid: 0,
        owner_created: 0,
        generations: vec![fixture.previous.generation.clone()],
    };
    write_private_json(&fixture.root.join(UNINSTALL_INTENT), &intent).unwrap();
    let journal_before =
        read_bounded(&fixture.root.join(UNINSTALL_INTENT), DESCRIPTOR_LIMIT).unwrap();
    assert!(!live_uninstall_owner(&fixture.root, "prod").unwrap());
    assert_eq!(fixture.current(), fixture.previous);
    assert!(!previous.exists());
    validate_uninstall_repair(&fixture.root, "prod", &intent).unwrap();

    // A dead journal explains an incomplete owned tree without pretending these
    // ordinary files constitute a signed PE or full manual preflight proof.
    windows_fs::private_directory(&previous).unwrap();
    durable_file(&previous.join("LICENSE.txt"), b"remaining owned license\n");
    validate_uninstall_repair(&fixture.root, "prod", &intent).unwrap();

    let unknown = previous.join("user-project.txt");
    durable_file(&unknown, b"unowned data must survive repair refusal\n");
    assert!(validate_uninstall_repair(&fixture.root, "prod", &intent).is_err());
    assert_eq!(
        fs::read(&unknown).unwrap(),
        b"unowned data must survive repair refusal\n"
    );
    assert_eq!(
        fs::read(previous.join("LICENSE.txt")).unwrap(),
        b"remaining owned license\n"
    );
    // Remove only this test's injected file, then prove the refusal below comes
    // from an unowned generation rather than the earlier unknown-file control.
    fs::remove_file(&unknown).unwrap();
    validate_uninstall_repair(&fixture.root, "prod", &intent).unwrap();
    let extra = fixture.root.join("versions").join("0.0.11-703");
    fixture_tree(&extra, "0.0.11", "703");
    let extra_before = scan_generation(&extra).unwrap();
    assert!(validate_uninstall_repair(&fixture.root, "prod", &intent).is_err());
    assert_eq!(scan_generation(&extra).unwrap(), extra_before);
    assert_eq!(
        fs::read(previous.join("LICENSE.txt")).unwrap(),
        b"remaining owned license\n"
    );
    assert_eq!(
        read_bounded(&fixture.root.join(POINTER), 1024).unwrap(),
        pointer_before
    );
    assert_eq!(
        read_bounded(&fixture.root.join(UNINSTALL_INTENT), DESCRIPTOR_LIMIT).unwrap(),
        journal_before
    );
}
