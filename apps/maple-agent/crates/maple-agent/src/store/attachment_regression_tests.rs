use super::*;

const ALICE_TASK: &str = "00000000-0000-7000-8000-000000000001";
const BOB_TASK: &str = "00000000-0000-7000-8000-000000000002";

fn regression_task(index: usize) -> String {
    format!("00000000-0000-7000-8000-{index:012x}")
}

fn reserve_upload(store: &Store, account: AccountId, task: &str, operation: &str, size: u64) {
    let mut upload = metadata(b"declared upload fixture");
    upload.size = size;
    match store
        .begin_attachment_upload(account, task, operation, &upload)
        .unwrap()
    {
        AttachmentUploadStart::Writing(writer) => drop(writer),
        AttachmentUploadStart::Complete(_) => panic!("new reservation was already complete"),
    }
}

fn stored_attachment_count(store: &Store, id: &str) -> i64 {
    store
        .read(|tx| {
            Ok(tx.query_row(
                "SELECT count(*) FROM file_attachments WHERE public_id=?1",
                [id],
                |row| row.get(0),
            )?)
        })
        .unwrap()
}

fn stored_task_count(store: &Store, uuid: &str) -> i64 {
    store
        .read(|tx| {
            Ok(
                tx.query_row("SELECT count(*) FROM tasks WHERE uuid=?1", [uuid], |row| {
                    row.get(0)
                })?,
            )
        })
        .unwrap()
}

fn set_legacy_name(store: &Store, id: &str) {
    store
        .write(Durability::Durable, |tx| {
            // This was a possible persisted result of the old normalizer. It must
            // not be accepted by content readers, but deletion still owns its tree.
            tx.execute(
                "UPDATE file_attachments SET name=' notes.txt' WHERE public_id=?1",
                [id],
            )?;
            Ok(())
        })
        .unwrap();
}

#[test]
fn normalized_attachment_names_are_bounded_fixed_points() {
    let mut names = vec![
        "* notes.txt".to_owned(),
        "? FAQ.md".to_owned(),
        "| draft.md".to_owned(),
        "../* \u{3000}notes.txt. \u{3000}".to_owned(),
        "\u{3000}\t<>:*?\u{3000}".to_owned(),
        ".part".to_owned(),
        format!("{}\u{3000}suffix", "あ".repeat(59)),
        format!("CON.{}", "x".repeat(176)),
    ];
    for prefix in ["", "* ", "? \u{3000}", "CON.", "LPT9.", ".part."] {
        for length in [0, 1, 168, 169, 179, 180, 181, 240] {
            for suffix in ["", ".", " ", "\u{3000}", "\u{3000}. "] {
                names.push(format!("{prefix}{}{suffix}", "a".repeat(length)));
                names.push(format!("{prefix}{}{suffix}", "あ".repeat(length)));
            }
        }
    }
    for name in names {
        let normalized = normalized_name(&name);
        assert!(!normalized.is_empty(), "empty normalization of {name:?}");
        assert!(
            normalized.len() <= 180,
            "oversized normalization of {name:?}"
        );
        assert_eq!(
            normalized_name(&normalized),
            normalized,
            "not a fixed point: {name:?}"
        );
    }
    assert_eq!(normalized_name("* notes.txt"), "notes.txt");
    assert_eq!(normalized_name("? FAQ.md"), "FAQ.md");
}

#[test]
fn normalized_attachment_names_support_upload_status_retry_and_cancel() {
    let (_directory, store, alice, _) = fixture();
    for (index, name) in [
        "* notes.txt".to_owned(),
        "? FAQ.md".to_owned(),
        "| draft.md".to_owned(),
        format!("{}\u{3000}suffix", "あ".repeat(59)),
        format!("CON.{}", "x".repeat(176)),
    ]
    .into_iter()
    .enumerate()
    {
        let operation = format!("normalized-{index}");
        let bytes = b"normalization lifecycle fixture";
        let mut upload = metadata(bytes);
        upload.name = name;
        let AttachmentUploadStart::Writing(mut writer) = store
            .begin_attachment_upload(alice, ALICE_TASK, &operation, &upload)
            .unwrap()
        else {
            panic!("new upload was already complete");
        };
        writer.write_chunk(bytes).unwrap();
        let attachment = writer.finish(&store).unwrap();
        assert_eq!(attachment.name, normalized_name(&upload.name));
        assert_eq!(fs::read(&attachment.execution_path).unwrap(), bytes);
        assert_eq!(
            store
                .attachment_upload_status(alice, ALICE_TASK, &operation)
                .unwrap(),
            AttachmentUploadStatus::Complete(attachment.clone()),
        );
        let AttachmentUploadStart::Complete(retried) = store
            .begin_attachment_upload(alice, ALICE_TASK, &operation, &upload)
            .unwrap()
        else {
            panic!("completed retry started another upload");
        };
        assert_eq!(retried, attachment);
        store
            .cancel_attachment_upload(alice, ALICE_TASK, &operation)
            .unwrap();
        assert_eq!(
            store
                .attachment_upload_status(alice, ALICE_TASK, &operation)
                .unwrap(),
            AttachmentUploadStatus::Cancelled,
        );
        assert!(
            !Path::new(&attachment.execution_path)
                .parent()
                .unwrap()
                .exists()
        );
    }
}

#[test]
fn legacy_invalid_names_do_not_prevent_cancel_expiry_or_task_collection() {
    let (_directory, store, alice, bob) = fixture();
    let cancelled = write_upload(
        &store,
        alice,
        ALICE_TASK,
        "legacy-cancel",
        b"cancel fixture",
    );
    let expired = write_upload(
        &store,
        alice,
        ALICE_TASK,
        "legacy-expire",
        b"expired fixture",
    );
    let other = write_upload(&store, bob, BOB_TASK, "unrelated", b"other account fixture");
    set_legacy_name(&store, &cancelled.id);
    set_legacy_name(&store, &expired.id);
    assert!(
        store
            .attachment_upload_status(alice, ALICE_TASK, "legacy-cancel")
            .is_err()
    );

    store
        .cancel_attachment_upload(alice, ALICE_TASK, "legacy-cancel")
        .unwrap();
    assert!(
        !Path::new(&cancelled.execution_path)
            .parent()
            .unwrap()
            .exists()
    );
    let cancelled_state: (String, i64) = store
        .read(|tx| {
            Ok(tx.query_row(
                "SELECT state,size FROM file_attachments WHERE public_id=?1",
                [&cancelled.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?)
        })
        .unwrap();
    assert_eq!(cancelled_state, ("cancelled".to_owned(), 0));

    expire(&store, &expired.id);
    store.collect_unbound_attachments_step().unwrap();
    assert_eq!(stored_attachment_count(&store, &expired.id), 0);
    assert!(
        !Path::new(&expired.execution_path)
            .parent()
            .unwrap()
            .exists()
    );

    store.mark_task_deleted(alice, ALICE_TASK, 2).unwrap();
    let mut done = false;
    for _ in 0..8 {
        if store.collect_task_step(alice, ALICE_TASK).unwrap().done {
            done = true;
            break;
        }
    }
    assert!(done, "invalid cancellation metadata stranded the task");
    assert_eq!(stored_task_count(&store, ALICE_TASK), 0);
    assert_eq!(stored_attachment_count(&store, &cancelled.id), 0);
    assert_eq!(
        fs::read(&other.execution_path).unwrap(),
        b"other account fixture"
    );
}

#[cfg(unix)]
#[test]
fn widened_owned_roots_do_not_strand_session_or_confirmed_account_deletion() {
    use std::os::unix::fs::PermissionsExt;

    for mode in [0o755, 0o711] {
        for delete_account in [false, true] {
            let (_directory, store, alice, bob) = fixture();
            let owned = write_upload(&store, alice, ALICE_TASK, "widened-root", b"owned archive");
            store
                .pin_attachments(alice, ALICE_TASK, std::slice::from_ref(&owned.id))
                .unwrap();
            let other = write_upload(
                &store,
                bob,
                BOB_TASK,
                "other-account",
                b"keep other account",
            );
            let root = Path::new(&owned.execution_path).parent().unwrap();
            fs::set_permissions(root, fs::Permissions::from_mode(mode)).unwrap();
            if delete_account {
                store
                    .begin_account_deletion(alice, "widened-root-deletion", 2)
                    .unwrap();
                store
                    .confirm_account_deletion(alice, "widened-root-deletion")
                    .unwrap();
            } else {
                store.mark_task_deleted(alice, ALICE_TASK, 2).unwrap();
            }
            let mut task_done = false;
            for _ in 0..8 {
                if store.collect_task_step(alice, ALICE_TASK).unwrap().done {
                    task_done = true;
                    break;
                }
            }
            assert!(task_done, "mode {mode:o} stranded session cleanup");
            assert!(!root.exists());
            assert_eq!(stored_task_count(&store, ALICE_TASK), 0);
            if delete_account {
                let mut account_done = false;
                for _ in 0..16 {
                    if store.collect_account_step(alice).unwrap().done {
                        account_done = true;
                        break;
                    }
                }
                assert!(account_done, "mode {mode:o} stranded account cleanup");
            }
            assert_eq!(
                fs::read(&other.execution_path).unwrap(),
                b"keep other account"
            );
        }
    }
}

#[test]
fn pending_file_count_is_per_account_even_during_pending_account_deletion() {
    let (_directory, store, alice, bob) = fixture();
    for index in 0..MAX_PENDING_FILES {
        let task = regression_task(100 + index / MAX_ATTACHMENTS);
        if index % MAX_ATTACHMENTS == 0 {
            create_task(&store, alice, &task);
        }
        reserve_upload(&store, alice, &task, &format!("pending-count-{index}"), 1);
    }
    let overflow = regression_task(200);
    create_task(&store, alice, &overflow);
    assert!(matches!(
        store.begin_attachment_upload(alice, &overflow, "count-overflow", &metadata(b"x")),
        Err(StoreError::Busy),
    ));
    reserve_upload(&store, bob, BOB_TASK, "bob-first", 1);

    store
        .begin_account_deletion(alice, "pending-count-deletion", 3)
        .unwrap();
    store
        .write(Durability::Durable, |tx| {
            tx.execute(
                "UPDATE file_attachments SET updated_ms=0 WHERE account_id=?1",
                [alice.0],
            )?;
            Ok(())
        })
        .unwrap();
    assert!(store.collect_unbound_attachments_step().unwrap().done);
    let retained: i64 = store
        .read(|tx| {
            Ok(tx.query_row(
                "SELECT count(*) FROM file_attachments WHERE account_id=?1",
                [alice.0],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(retained, MAX_PENDING_FILES as i64);
    reserve_upload(&store, bob, BOB_TASK, "bob-while-alice-pending", 1);
}

#[test]
fn pending_declared_byte_limit_is_per_account() {
    let (_directory, store, alice, bob) = fixture();
    assert_eq!(MAX_PENDING_BYTES / MAX_ATTACHMENT_BYTES as u64, 10);
    for index in 0..10 {
        reserve_upload(
            &store,
            alice,
            ALICE_TASK,
            &format!("pending-bytes-{index}"),
            MAX_ATTACHMENT_BYTES as u64,
        );
    }
    // Use another task to distinguish the account byte limit from the pending
    // per-task count. Reservations write no declared payload bytes to disk.
    let overflow = regression_task(201);
    create_task(&store, alice, &overflow);
    assert!(matches!(
        store.begin_attachment_upload(alice, &overflow, "bytes-overflow", &metadata(b"x")),
        Err(StoreError::Busy),
    ));
    reserve_upload(&store, bob, BOB_TASK, "bob-with-full-other-account", 1);
}

#[cfg(unix)]
struct UnsafeTaskDirectory {
    path: PathBuf,
    saved: PathBuf,
}

#[cfg(unix)]
impl UnsafeTaskDirectory {
    fn new(attachment: &StoredAttachment, outside: &Path) -> Self {
        let path = Path::new(&attachment.execution_path)
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_owned();
        let saved = path.with_file_name(format!(
            "retained-{}",
            path.file_name().unwrap().to_string_lossy()
        ));
        fs::rename(&path, &saved).unwrap();
        std::os::unix::fs::symlink(outside, &path).unwrap();
        Self { path, saved }
    }
}

#[cfg(unix)]
impl Drop for UnsafeTaskDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::rename(&self.saved, &self.path);
    }
}

#[cfg(unix)]
#[test]
fn unbound_collection_rotates_past_multiple_unsafe_trees() {
    let (_directory, store, alice, bob) = fixture();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("keep"), b"unrelated directory fixture").unwrap();
    let mut blocked = Vec::new();
    let mut guards = Vec::new();
    for index in 0..2 {
        let task = regression_task(300 + index);
        create_task(&store, alice, &task);
        let attachment = write_upload(
            &store,
            alice,
            &task,
            &format!("unsafe-unbound-{index}"),
            b"retained fixture",
        );
        store
            .write(Durability::Durable, |tx| {
                tx.execute(
                    "UPDATE file_attachments SET updated_ms=?2 WHERE public_id=?1",
                    params![attachment.id, index as i64],
                )?;
                Ok(())
            })
            .unwrap();
        guards.push(UnsafeTaskDirectory::new(&attachment, outside.path()));
        blocked.push(attachment);
    }
    let healthy = write_upload(&store, bob, BOB_TASK, "healthy-expired", b"healthy fixture");
    store
        .write(Durability::Durable, |tx| {
            tx.execute(
                "UPDATE file_attachments SET updated_ms=2 WHERE public_id=?1",
                [&healthy.id],
            )?;
            Ok(())
        })
        .unwrap();
    let mut failures = 0;
    for _ in 0..6 {
        match store.collect_unbound_attachments_step() {
            Err(StoreError::UnsafePath) => failures += 1,
            Ok(_) => {}
            Err(error) => panic!("unexpected collection error: {error:?}"),
        }
        if stored_attachment_count(&store, &healthy.id) == 0 {
            break;
        }
    }
    assert_eq!(failures, 2);
    assert_eq!(stored_attachment_count(&store, &healthy.id), 0);
    assert!(
        !Path::new(&healthy.execution_path)
            .parent()
            .unwrap()
            .exists()
    );
    for attachment in &blocked {
        assert_eq!(stored_attachment_count(&store, &attachment.id), 1);
    }
    assert_eq!(
        fs::read(outside.path().join("keep")).unwrap(),
        b"unrelated directory fixture"
    );
    drop(guards);
    for _ in 0..4 {
        store.collect_unbound_attachments_step().unwrap();
    }
    for attachment in blocked {
        assert_eq!(stored_attachment_count(&store, &attachment.id), 0);
        assert!(
            !Path::new(&attachment.execution_path)
                .parent()
                .unwrap()
                .exists()
        );
    }
}

#[cfg(unix)]
#[test]
fn deleted_task_collection_rotates_past_four_unsafe_trees() {
    let (_directory, store, alice, bob) = fixture();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("keep"), b"unrelated directory fixture").unwrap();
    let mut blocked = Vec::new();
    let mut guards = Vec::new();
    for index in 0..4 {
        let task = regression_task(400 + index);
        create_task(&store, alice, &task);
        let attachment = write_upload(
            &store,
            alice,
            &task,
            &format!("unsafe-deleted-{index}"),
            b"retained fixture",
        );
        guards.push(UnsafeTaskDirectory::new(&attachment, outside.path()));
        store.mark_task_deleted(alice, &task, 1).unwrap();
        blocked.push(task);
    }
    let healthy = write_upload(&store, bob, BOB_TASK, "healthy-deleted", b"healthy fixture");
    store.mark_task_deleted(bob, BOB_TASK, 2).unwrap();
    let mut failures = 0;
    for _ in 0..6 {
        for task in store.deleted_tasks_for_collection(4).unwrap() {
            match store.collect_task_step(task.account, &task.uuid) {
                Err(StoreError::UnsafePath) => failures += 1,
                Ok(_) => {}
                Err(error) => panic!("unexpected task collection error: {error:?}"),
            }
        }
        if stored_task_count(&store, BOB_TASK) == 0 {
            break;
        }
    }
    assert!(failures >= 4);
    assert_eq!(stored_task_count(&store, BOB_TASK), 0);
    assert!(
        !Path::new(&healthy.execution_path)
            .parent()
            .unwrap()
            .exists()
    );
    for task in &blocked {
        assert_eq!(stored_task_count(&store, task), 1);
    }
    assert_eq!(
        fs::read(outside.path().join("keep")).unwrap(),
        b"unrelated directory fixture"
    );
    drop(guards);
    for _ in 0..4 {
        for task in store.deleted_tasks_for_collection(4).unwrap() {
            store.collect_task_step(task.account, &task.uuid).unwrap();
        }
    }
    for task in blocked {
        assert_eq!(stored_task_count(&store, &task), 0);
    }
}
