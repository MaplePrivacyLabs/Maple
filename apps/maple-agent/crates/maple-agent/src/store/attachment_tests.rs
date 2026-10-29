use super::*;
use crate::store::StorePaths;

#[path = "attachment_regression_tests.rs"]
mod regression_tests;

fn fixture() -> (tempfile::TempDir, Arc<Store>, AccountId, AccountId) {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(StorePaths::new(directory.path())).unwrap());
    let alice = store.upsert_account("alice", 1).unwrap();
    let bob = store.upsert_account("bob", 1).unwrap();
    create_task(&store, alice, "00000000-0000-7000-8000-000000000001");
    create_task(&store, bob, "00000000-0000-7000-8000-000000000002");
    (directory, store, alice, bob)
}

fn create_task(store: &Store, account: AccountId, uuid: &str) -> i64 {
    store.write(Durability::Durable, |transaction| {
        transaction.execute(
            "INSERT INTO projects(account_id,root,last_used_ms) VALUES(?1,?2,1) ON CONFLICT DO NOTHING",
            params![account.0, format!("/fixture/{}",account.0)],
        )?;
        let project: i64 = transaction.query_row("SELECT id FROM projects WHERE account_id=?1",[account.0],|row| row.get(0))?;
        transaction.execute(
            "INSERT INTO tasks(uuid,account_id,project_id,kind,state,title,title_fold,created_ms,updated_ms) \
             VALUES(?1,?2,?3,'desktop','active','fixture','fixture',1,1)",
            params![uuid,account.0,project],
        )?;
        Ok(transaction.last_insert_rowid())
    }).unwrap()
}

fn png(size: usize) -> Vec<u8> {
    let mut bytes = vec![17; size.max(8)];
    bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
    bytes
}

fn upload(bytes: &[u8]) -> ImageUpload {
    ImageUpload {
        name: "../photo.png".to_string(),
        data_url: format!("data:image/png;base64,{}", STANDARD.encode(bytes)),
    }
}

fn metadata(bytes: &[u8]) -> AttachmentUploadMetadata {
    AttachmentUploadMetadata {
        name: "../report.zip".into(),
        mime_type: "application/zip".into(),
        size: bytes.len() as u64,
        sha256: hex(&Sha256::digest(bytes)),
    }
}
fn write_upload(
    store: &Store,
    account: AccountId,
    task: &str,
    op: &str,
    bytes: &[u8],
) -> StoredAttachment {
    match store
        .begin_attachment_upload(account, task, op, &metadata(bytes))
        .unwrap()
    {
        AttachmentUploadStart::Complete(value) => value,
        AttachmentUploadStart::Writing(mut writer) => {
            for chunk in bytes.chunks(MAX_ATTACHMENT_RANGE_BYTES) {
                writer.write_chunk(chunk).unwrap();
            }
            writer.finish(store).unwrap()
        }
    }
}
fn expire(store: &Store, id: &str) {
    store
        .write(Durability::Durable, |tx| {
            tx.execute(
                "UPDATE file_attachments SET updated_ms=0 WHERE public_id=?1",
                [id],
            )?;
            Ok(())
        })
        .unwrap();
}

#[test]
fn cleanup_metadata_rejects_negative_sizes_and_accepts_zero_and_file_cap() {
    let (_directory, store, _, _) = fixture();
    for size in [-1, 0, MAX_ATTACHMENT_BYTES as i64] {
        let result = store.read(|tx| {
            Ok(tx
                .query_row(
                    "SELECT '0123456789abcdef0123456789abcdef','cleanup-size',?1",
                    [size],
                    attachment_cleanup_row,
                )?
                .size)
        });
        if size < 0 {
            assert_eq!(result, Err(StoreError::InvalidData));
        } else {
            assert_eq!(result, Ok(size as u64));
        }
    }
}

#[test]
fn generic_upload_is_a_stable_file_with_only_metadata_in_sql() {
    let (_directory, store, alice, bob) = fixture();
    let bytes = b"a ZIP fixture with its own bytes";
    let first = write_upload(
        &store,
        alice,
        "00000000-0000-7000-8000-000000000001",
        "operation-1",
        bytes,
    );
    let second = write_upload(
        &store,
        bob,
        "00000000-0000-7000-8000-000000000002",
        "operation-2",
        bytes,
    );
    assert_eq!(first.name, "report.zip");
    assert_ne!(first.id, second.id);
    assert_ne!(first.execution_path, second.execution_path);
    assert_eq!(fs::read(&first.execution_path).unwrap(), bytes);
    assert_eq!(
        store
            .read_attachment(
                alice,
                "00000000-0000-7000-8000-000000000001",
                &first.id,
                2,
                5
            )
            .unwrap()
            .data,
        &bytes[2..7]
    );
    assert!(
        store
            .read_attachment(bob, "00000000-0000-7000-8000-000000000002", &first.id, 0, 1)
            .is_err()
    );
    assert!(
        store
            .resolve_attachments(
                alice,
                "00000000-0000-7000-8000-000000000002",
                std::slice::from_ref(&first.id)
            )
            .is_err()
    );
    store
        .read(|tx| {
            for legacy in ["blobs", "blob_chunks", "task_blobs", "attachments"] {
                assert!(!tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name=?1)",
                    [legacy],
                    |row| row.get::<_, bool>(0)
                )?);
            }
            let names: Vec<String> = tx
                .prepare("PRAGMA table_info(file_attachments)")?
                .query_map([], |row| row.get(1))?
                .collect::<Result<_, _>>()?;
            assert!(!names.iter().any(|name| name == "data" || name == "payload"));
            Ok(())
        })
        .unwrap();
}

#[test]
fn operation_replay_is_idempotent_and_bound_to_task_and_metadata() {
    let (_directory, store, alice, _) = fixture();
    create_task(&store, alice, "00000000-0000-7000-8000-000000000003");
    let first = write_upload(
        &store,
        alice,
        "00000000-0000-7000-8000-000000000001",
        "operation",
        b"hello",
    );
    assert_eq!(
        first,
        write_upload(
            &store,
            alice,
            "00000000-0000-7000-8000-000000000001",
            "operation",
            b"hello"
        )
    );
    assert!(matches!(
        store.begin_attachment_upload(
            alice,
            "00000000-0000-7000-8000-000000000001",
            "operation",
            &metadata(b"other")
        ),
        Err(StoreError::Conflict)
    ));
    assert!(matches!(
        store.begin_attachment_upload(
            alice,
            "00000000-0000-7000-8000-000000000003",
            "operation",
            &metadata(b"hello")
        ),
        Err(StoreError::Conflict)
    ));
    assert!(
        matches!(store.attachment_upload_status(alice,"00000000-0000-7000-8000-000000000001","operation").unwrap(),AttachmentUploadStatus::Complete(value) if value==first)
    );
}

#[test]
fn partial_upload_can_restart_but_never_publish_wrong_size_or_digest() {
    let (_directory, store, alice, _) = fixture();
    let meta = metadata(b"expected");
    let AttachmentUploadStart::Writing(mut writer) = store
        .begin_attachment_upload(
            alice,
            "00000000-0000-7000-8000-000000000001",
            "partial",
            &meta,
        )
        .unwrap()
    else {
        panic!()
    };
    writer.write_chunk(b"ex").unwrap();
    assert!(matches!(
        store.begin_attachment_upload(
            alice,
            "00000000-0000-7000-8000-000000000001",
            "partial",
            &meta
        ),
        Err(StoreError::Busy)
    ));
    drop(writer);
    let AttachmentUploadStart::Writing(mut writer) = store
        .begin_attachment_upload(
            alice,
            "00000000-0000-7000-8000-000000000001",
            "partial",
            &meta,
        )
        .unwrap()
    else {
        panic!()
    };
    writer.write_chunk(b"xxxxxxxx").unwrap();
    assert!(matches!(
        writer.finish(&store),
        Err(StoreError::InvalidInput)
    ));
    assert_eq!(
        store
            .attachment_upload_status(alice, "00000000-0000-7000-8000-000000000001", "partial")
            .unwrap(),
        AttachmentUploadStatus::Uploading
    );
    let result = write_upload(
        &store,
        alice,
        "00000000-0000-7000-8000-000000000001",
        "partial",
        b"expected",
    );
    assert_eq!(fs::read(result.execution_path).unwrap(), b"expected");
}

#[test]
fn published_file_without_ready_ack_is_reconciled_after_reopen() {
    let (directory, store, alice, _) = fixture();
    let attachment = write_upload(
        &store,
        alice,
        "00000000-0000-7000-8000-000000000001",
        "lost-ack",
        b"published",
    );
    store
        .write(Durability::Durable, |tx| {
            tx.execute(
                "UPDATE file_attachments SET state='uploading' WHERE public_id=?1",
                [&attachment.id],
            )?;
            Ok(())
        })
        .unwrap();
    drop(store);
    let store = Store::open(StorePaths::new(directory.path())).unwrap();
    assert_eq!(
        attachment,
        write_upload(
            &store,
            alice,
            "00000000-0000-7000-8000-000000000001",
            "lost-ack",
            b"published"
        )
    );
}

#[test]
fn cancellation_before_begin_and_during_stream_are_durable_fences() {
    let (directory, store, alice, _) = fixture();
    store
        .cancel_attachment_upload(
            alice,
            "00000000-0000-7000-8000-000000000001",
            "before-begin",
        )
        .unwrap();
    assert_eq!(
        store
            .attachment_upload_status(
                alice,
                "00000000-0000-7000-8000-000000000001",
                "before-begin"
            )
            .unwrap(),
        AttachmentUploadStatus::Cancelled
    );
    assert!(matches!(
        store.begin_attachment_upload(
            alice,
            "00000000-0000-7000-8000-000000000001",
            "before-begin",
            &metadata(b"late")
        ),
        Err(StoreError::Conflict)
    ));
    let AttachmentUploadStart::Writing(mut writer) = store
        .begin_attachment_upload(
            alice,
            "00000000-0000-7000-8000-000000000001",
            "during",
            &metadata(b"hello"),
        )
        .unwrap()
    else {
        panic!()
    };
    store
        .cancel_attachment_upload(alice, "00000000-0000-7000-8000-000000000001", "during")
        .unwrap();
    assert!(matches!(
        writer.write_chunk(b"hello"),
        Err(StoreError::Conflict)
    ));
    drop(writer);
    drop(store);
    let store = Store::open(StorePaths::new(directory.path())).unwrap();
    assert_eq!(
        store
            .attachment_upload_status(alice, "00000000-0000-7000-8000-000000000001", "during")
            .unwrap(),
        AttachmentUploadStatus::Cancelled
    );
    assert!(matches!(
        store.begin_attachment_upload(
            alice,
            "00000000-0000-7000-8000-000000000001",
            "before-begin",
            &metadata(b"late")
        ),
        Err(StoreError::Conflict)
    ));
}

#[test]
fn pinning_is_all_or_nothing_and_unresolved_references_survive_staging_gc() {
    let (_directory, store, alice, _) = fixture();
    let attachment = write_upload(
        &store,
        alice,
        "00000000-0000-7000-8000-000000000001",
        "pin",
        b"durable",
    );
    assert!(
        store
            .pin_attachments(
                alice,
                "00000000-0000-7000-8000-000000000001",
                &[attachment.id.clone(), "f".repeat(32)]
            )
            .is_err()
    );
    assert!(
        !store
            .read(|tx| Ok(tx.query_row(
                "SELECT bound FROM file_attachments WHERE public_id=?1",
                [&attachment.id],
                |row| row.get::<_, bool>(0)
            )?))
            .unwrap()
    );
    store
        .pin_attachments(
            alice,
            "00000000-0000-7000-8000-000000000001",
            std::slice::from_ref(&attachment.id),
        )
        .unwrap();
    expire(&store, &attachment.id);
    assert!(store.collect_unbound_attachments_step().unwrap().done);
    assert!(Path::new(&attachment.execution_path).exists());
    assert!(matches!(
        store.cancel_attachment_upload(alice, "00000000-0000-7000-8000-000000000001", "pin"),
        Err(StoreError::Conflict)
    ));
}

#[test]
fn deleting_one_session_removes_only_its_exact_files_and_survives_missing_bytes() {
    let (_directory, store, alice, bob) = fixture();
    let first = write_upload(
        &store,
        alice,
        "00000000-0000-7000-8000-000000000001",
        "delete",
        b"same",
    );
    let other = write_upload(
        &store,
        bob,
        "00000000-0000-7000-8000-000000000002",
        "keep",
        b"same",
    );
    store
        .mark_task_deleted(alice, "00000000-0000-7000-8000-000000000001", 5)
        .unwrap();
    fs::remove_file(&first.execution_path).unwrap();
    for _ in 0..20 {
        if store
            .collect_task_step(alice, "00000000-0000-7000-8000-000000000001")
            .unwrap()
            .done
        {
            break;
        }
    }
    assert!(
        store
            .task(alice, "00000000-0000-7000-8000-000000000001")
            .unwrap()
            .is_none()
    );
    assert!(!Path::new(&first.execution_path).parent().unwrap().exists());
    assert_eq!(fs::read(&other.execution_path).unwrap(), b"same");
}

#[test]
fn missing_effective_image_becomes_unavailable_but_bad_source_stays_an_error() {
    let (_directory, store, alice, _) = fixture();
    let attachment = store
        .store_image_uploads(
            alice,
            "00000000-0000-7000-8000-000000000001",
            &[upload(&png(100))],
        )
        .unwrap()
        .remove(0);
    let mut messages = vec![Message::User(pi_ai::UserMessage {
        content: vec![
            Content::text("keep this text"),
            Content::image(&attachment.source, "image/png"),
        ],
        timestamp: 1,
    })];
    fs::remove_file(&attachment.execution_path).unwrap();
    store
        .materialize_message_images(alice, "00000000-0000-7000-8000-000000000001", &mut messages)
        .unwrap();
    let Message::User(message) = &messages[0] else {
        panic!()
    };
    assert_eq!(
        message.content,
        vec![
            Content::text("keep this text"),
            Content::text("Image no longer available.")
        ]
    );
    let mut invalid = vec![Message::User(pi_ai::UserMessage {
        content: vec![Content::image(
            format!("{ATTACHMENT_SOURCE_PREFIX}{}", "f".repeat(32)),
            "image/png",
        )],
        timestamp: 1,
    })];
    assert!(
        store
            .materialize_message_images(alice, "00000000-0000-7000-8000-000000000001", &mut invalid)
            .is_err()
    );
}

#[test]
fn historical_file_changes_are_unavailable_but_outgoing_images_remain_strict() {
    for change in 0..3 {
        let (_directory, store, alice, _) = fixture();
        let task = "00000000-0000-7000-8000-000000000001";
        let bytes = png(100);
        let attachment = store
            .store_image_uploads(alice, task, &[upload(&bytes)])
            .unwrap()
            .remove(0);
        let original = vec![Message::User(pi_ai::UserMessage {
            content: vec![
                Content::text("keep the caption"),
                Content::image(&attachment.source, "image/png"),
            ],
            timestamp: 1,
        })];
        match change {
            0 => {
                let mut changed = bytes.clone();
                changed[99] ^= 1;
                fs::write(&attachment.execution_path, changed).unwrap();
            }
            1 => fs::write(&attachment.execution_path, b"resized").unwrap(),
            2 => fs::remove_file(&attachment.execution_path).unwrap(),
            _ => unreachable!(),
        }
        let mut outgoing = original.clone();
        assert!(
            store
                .materialize_outgoing_images(alice, task, &mut outgoing)
                .is_err()
        );
        let Message::User(outgoing) = &outgoing[0] else {
            panic!()
        };
        assert_eq!(
            outgoing.content[1],
            Content::image(&attachment.source, "image/png")
        );
        let mut historical = original;
        store
            .materialize_message_images(alice, task, &mut historical)
            .unwrap();
        let Message::User(historical) = &historical[0] else {
            panic!()
        };
        assert_eq!(
            historical.content,
            vec![
                Content::text("keep the caption"),
                Content::text("Image no longer available.")
            ]
        );
    }
}

#[cfg(unix)]
#[test]
fn unsafe_historical_images_are_not_read_or_allowed_in_new_sends() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    for change in 0..3 {
        let (directory, store, alice, _) = fixture();
        let task = "00000000-0000-7000-8000-000000000001";
        let attachment = store
            .store_image_uploads(alice, task, &[upload(&png(100))])
            .unwrap()
            .remove(0);
        let outside = directory.path().join("outside-image");
        match change {
            0 => fs::set_permissions(
                &attachment.execution_path,
                fs::Permissions::from_mode(0o644),
            )
            .unwrap(),
            1 => {
                fs::remove_file(&attachment.execution_path).unwrap();
                fs::write(&outside, b"unrelated image bytes").unwrap();
                symlink(&outside, &attachment.execution_path).unwrap();
            }
            2 => fs::hard_link(&attachment.execution_path, &outside).unwrap(),
            _ => unreachable!(),
        }
        let original = vec![Message::User(pi_ai::UserMessage {
            content: vec![Content::image(&attachment.source, "image/png")],
            timestamp: 1,
        })];
        assert!(
            store
                .materialize_outgoing_images(alice, task, &mut original.clone())
                .is_err()
        );
        let mut historical = original;
        store
            .materialize_message_images(alice, task, &mut historical)
            .unwrap();
        let Message::User(message) = &historical[0] else {
            panic!()
        };
        assert_eq!(
            message.content,
            vec![Content::text("Image no longer available.")]
        );
        if change == 1 {
            assert_eq!(fs::read(&outside).unwrap(), b"unrelated image bytes");
        }
    }
}

#[test]
fn historical_metadata_corruption_is_not_hidden_as_unavailable_media() {
    let (_directory, store, alice, _) = fixture();
    let task = "00000000-0000-7000-8000-000000000001";
    let attachment = store
        .store_image_uploads(alice, task, &[upload(&png(100))])
        .unwrap()
        .remove(0);
    store
        .write(Durability::Durable, |tx| {
            tx.execute(
                "UPDATE file_attachments SET name=' not-normalized.png' WHERE public_id=?1",
                [&attachment.id],
            )?;
            Ok(())
        })
        .unwrap();
    let mut messages = vec![Message::User(pi_ai::UserMessage {
        content: vec![Content::image(&attachment.source, "image/png")],
        timestamp: 1,
    })];
    assert_eq!(
        store.materialize_message_images(alice, task, &mut messages),
        Err(StoreError::InvalidData)
    );
}

#[cfg(unix)]
#[test]
fn confirmed_account_deletion_reclaims_readonly_owned_trees_and_keeps_other_accounts() {
    use std::os::unix::fs::PermissionsExt;
    let (_directory, store, alice, bob) = fixture();
    let task = "00000000-0000-7000-8000-000000000001";
    let owned = write_upload(&store, alice, task, "readonly-account", b"owned archive");
    store
        .pin_attachments(alice, task, std::slice::from_ref(&owned.id))
        .unwrap();
    let other = write_upload(
        &store,
        bob,
        "00000000-0000-7000-8000-000000000002",
        "other-account",
        b"keep other account",
    );
    let root = Path::new(&owned.execution_path).parent().unwrap();
    let extracted = root.join("extracted");
    fs::create_dir(&extracted).unwrap();
    fs::write(extracted.join("plaintext"), b"owned extraction").unwrap();
    fs::set_permissions(&extracted, fs::Permissions::from_mode(0o555)).unwrap();
    store
        .begin_account_deletion(alice, "confirmed-readonly", 2)
        .unwrap();
    store
        .confirm_account_deletion(alice, "confirmed-readonly")
        .unwrap();
    let mut done = false;
    for _ in 0..32 {
        for deleted in store.deleted_tasks_for_collection(4).unwrap() {
            store
                .collect_task_step(deleted.account, &deleted.uuid)
                .unwrap();
        }
        match store.collect_account_step(alice) {
            Ok(progress) if progress.done => {
                done = true;
                break;
            }
            Ok(_) | Err(StoreError::Busy) => {}
            Err(error) => panic!("account cleanup failed: {error:?}"),
        }
    }
    assert!(done);
    assert!(!root.exists());
    let retained: i64 = store
        .read(|tx| {
            Ok(tx.query_row(
                "SELECT count(*) FROM accounts WHERE id=?1",
                [alice.0],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(retained, 0);
    assert_eq!(
        fs::read(&other.execution_path).unwrap(),
        b"keep other account"
    );
}

#[test]
fn file_caps_and_image_caps_are_separate_and_chunk_writes_are_bounded() {
    let (_directory, store, alice, _) = fixture();
    let bytes = vec![42; MAX_UPLOAD_IMAGE_BYTES + 1];
    let mut large_image_metadata = metadata(&bytes);
    large_image_metadata.mime_type = "image/png".into();
    assert!(matches!(
        store.begin_attachment_upload(
            alice,
            "00000000-0000-7000-8000-000000000001",
            "image-through-file-api",
            &large_image_metadata
        ),
        Err(StoreError::InvalidInput)
    ));
    let generic = write_upload(
        &store,
        alice,
        "00000000-0000-7000-8000-000000000001",
        "large-file",
        &bytes,
    );
    assert_eq!(generic.size, bytes.len() as u64);
    assert!(matches!(
        store.store_image_uploads(
            alice,
            "00000000-0000-7000-8000-000000000001",
            &[upload(&png(MAX_UPLOAD_IMAGE_BYTES + 1))]
        ),
        Err(StoreError::InvalidInput)
    ));
    let mut excessive = metadata(b"");
    excessive.size = MAX_ATTACHMENT_BYTES as u64 + 1;
    assert!(matches!(
        store.begin_attachment_upload(
            alice,
            "00000000-0000-7000-8000-000000000001",
            "too-large",
            &excessive
        ),
        Err(StoreError::InvalidInput)
    ));
    let AttachmentUploadStart::Writing(mut writer) = store
        .begin_attachment_upload(
            alice,
            "00000000-0000-7000-8000-000000000001",
            "chunk",
            &metadata(&bytes),
        )
        .unwrap()
    else {
        panic!()
    };
    assert!(matches!(
        writer.write_chunk(&bytes),
        Err(StoreError::InvalidInput)
    ));
}

#[test]
fn cleanup_never_follows_a_substituted_attachment_symlink() {
    #[cfg(unix)]
    {
        let (directory, store, alice, _) = fixture();
        let attachment = write_upload(
            &store,
            alice,
            "00000000-0000-7000-8000-000000000001",
            "symlink",
            b"private",
        );
        let outside = directory.path().join("do-not-delete");
        fs::write(&outside, b"outside").unwrap();
        fs::remove_file(&attachment.execution_path).unwrap();
        std::os::unix::fs::symlink(&outside, &attachment.execution_path).unwrap();
        store
            .mark_task_deleted(alice, "00000000-0000-7000-8000-000000000001", 5)
            .unwrap();
        let progress = store
            .collect_task_attachments_step(alice, "00000000-0000-7000-8000-000000000001")
            .unwrap();
        assert_eq!(progress.links, 1);
        assert_eq!(fs::read(outside).unwrap(), b"outside");
    }
}

#[test]
fn provisional_session_is_kept_while_upload_has_no_message_yet() {
    let (_directory, store, alice, _) = fixture();
    store
        .write(Durability::Durable, |tx| {
            tx.execute(
                "UPDATE tasks SET provisional=1 WHERE uuid='00000000-0000-7000-8000-000000000001'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    let _attachment = write_upload(
        &store,
        alice,
        "00000000-0000-7000-8000-000000000001",
        "draft",
        b"still needed",
    );
    assert!(
        !store
            .mark_untouched_provisional_deleted(alice, "00000000-0000-7000-8000-000000000001", 10)
            .unwrap()
    );
}

#[test]
fn repeated_cancel_does_not_consume_active_upload_quota_and_cleans_bytes() {
    let (_directory, store, alice, _) = fixture();
    let task = "00000000-0000-7000-8000-000000000001";
    for index in 0..80 {
        let op = format!("cancel-{index}");
        let file = write_upload(&store, alice, task, &op, b"cancelled bytes");
        store.cancel_attachment_upload(alice, task, &op).unwrap();
        assert!(!Path::new(&file.execution_path).exists());
        assert!(store.collect_unbound_attachments_step().unwrap().done);
        assert_eq!(
            store.attachment_upload_status(alice, task, &op).unwrap(),
            AttachmentUploadStatus::Cancelled
        );
    }
    let file = write_upload(&store, alice, task, "still-usable", b"oops");
    assert_eq!(file.size, 4);
}

#[test]
fn cancelled_active_writers_reserve_bytes_until_files_are_collected() {
    let (_directory, store, alice, _) = fixture();
    let task = "00000000-0000-7000-8000-000000000001";
    let mut metadata = metadata(b"partial");
    metadata.size = MAX_ATTACHMENT_BYTES as u64;
    let mut writers = Vec::new();
    for index in 0..MAX_ATTACHMENTS {
        let operation = format!("held-cancel-{index}");
        let AttachmentUploadStart::Writing(mut writer) = store
            .begin_attachment_upload(alice, task, &operation, &metadata)
            .unwrap()
        else {
            panic!("new writer");
        };
        writer.write_chunk(b"partial").unwrap();
        store
            .cancel_attachment_upload(alice, task, &operation)
            .unwrap();
        assert!(writer.is_cancelled());
        writers.push(writer);
    }
    assert_eq!(metadata.size * writers.len() as u64, MAX_PENDING_BYTES);
    assert!(matches!(
        store.begin_attachment_upload(alice, task, "budget-still-reserved", &metadata),
        Err(StoreError::Busy)
    ));
    assert_eq!(store.collect_unbound_attachments_step().unwrap().files, 0);
    drop(writers);
    let collected = store.collect_unbound_attachments_step().unwrap();
    assert_eq!(collected.bytes, MAX_ATTACHMENT_BYTES);
    assert_eq!(collected.links, 1);
    assert!(matches!(
        store.begin_attachment_upload(alice, task, "budget-released", &metadata),
        Ok(AttachmentUploadStart::Writing(_))
    ));
}

#[test]
fn source_batch_over_composer_count_has_independent_file_storage() {
    let (_directory, store, alice, _) = fixture();
    let task = "00000000-0000-7000-8000-000000000001";
    for index in 0..MAX_ATTACHMENTS {
        write_upload(&store, alice, task, &format!("pending-{index}"), b"pending");
    }
    let entries = (0..15)
        .map(|index| SessionEntry {
            id: format!("{index:08x}"),
            parent_id: None,
            timestamp: 1,
            kind: EntryKind::Message {
                message: SessionMessage::Llm(Message::User(pi_ai::UserMessage {
                    content: vec![Content::image(STANDARD.encode(png(20)), "image/png")],
                    timestamp: 1,
                })),
            },
        })
        .collect::<Vec<_>>();
    let prepared = store.prepare_entries_images(alice, task, &entries).unwrap();
    assert_eq!(prepared.entries.len(), 15);
    for entry in prepared.entries {
        let EntryKind::Message {
            message: SessionMessage::Llm(Message::User(message)),
        } = entry.entry.kind
        else {
            panic!()
        };
        let Content::Image(image) = &message.content[0] else {
            panic!()
        };
        assert!(image.data.starts_with(ATTACHMENT_SOURCE_PREFIX));
    }
}

#[test]
fn source_image_operation_identity_is_scoped_to_each_session() {
    let (_directory, store, alice, _) = fixture();
    let first = "00000000-0000-7000-8000-000000000001";
    let second = "00000000-0000-7000-8000-000000000003";
    create_task(&store, alice, second);
    let entry = SessionEntry {
        id: "entry-with-a-long-external-identity".repeat(4),
        parent_id: None,
        timestamp: 1,
        kind: EntryKind::Message {
            message: SessionMessage::Llm(Message::User(pi_ai::UserMessage {
                content: vec![Content::image(STANDARD.encode(png(20)), "image/png")],
                timestamp: 1,
            })),
        },
    };
    let a = store.prepare_entry_images(alice, first, &entry).unwrap();
    let b = store.prepare_entry_images(alice, second, &entry).unwrap();
    assert_ne!(a.attachments, b.attachments);
    let apath = store
        .attachment_metadata(alice, first, &a.attachments[0])
        .unwrap()
        .execution_path;
    let bpath = store
        .attachment_metadata(alice, second, &b.attachments[0])
        .unwrap()
        .execution_path;
    assert_ne!(apath, bpath);
    assert_eq!(fs::read(apath).unwrap(), fs::read(bpath).unwrap());
}

#[test]
fn stored_source_rejects_inline_images_while_incoming_media_is_externalized() {
    let entry = SessionEntry {
        id: "01020304".into(),
        parent_id: None,
        timestamp: 1,
        kind: EntryKind::Message {
            message: SessionMessage::Llm(Message::User(pi_ai::UserMessage {
                content: vec![Content::image(STANDARD.encode(png(20)), "image/png")],
                timestamp: 1,
            })),
        },
    };
    let encoded = super::super::codec::encode(&serde_json::to_vec(&entry).unwrap()).unwrap();
    assert!(matches!(
        super::super::session::decode_entry(encoded.0, &encoded.1),
        Err(StoreError::InvalidData)
    ));
}

#[test]
fn partial_filename_is_reserved_case_insensitively() {
    let (_directory, store, alice, _) = fixture();
    let task = "00000000-0000-7000-8000-000000000001";
    let mut metadata = metadata(b"data");
    metadata.name = ".PART".into();
    let AttachmentUploadStart::Writing(mut writer) = store
        .begin_attachment_upload(alice, task, "reserved-name", &metadata)
        .unwrap()
    else {
        panic!()
    };
    writer.write_chunk(b"data").unwrap();
    let file = writer.finish(&store).unwrap();
    assert_eq!(file.name, "attachment-.PART");
    assert_eq!(fs::read(file.execution_path).unwrap(), b"data");
}

#[test]
fn image_mime_mismatch_is_refused_but_arbitrary_file_bytes_are_allowed() {
    let (_directory, store, alice, _) = fixture();
    let task = "00000000-0000-7000-8000-000000000001";
    let bytes = b"PK\x03\x04a zip, not a png";
    let mut meta = metadata(bytes);
    meta.mime_type = "image/png".into();
    let AttachmentUploadStart::Writing(mut writer) = store
        .begin_attachment_upload(alice, task, "false-image", &meta)
        .unwrap()
    else {
        panic!()
    };
    writer.write_chunk(bytes).unwrap();
    assert!(matches!(
        writer.finish(&store),
        Err(StoreError::InvalidInput)
    ));
    let file = write_upload(&store, alice, task, "ordinary-zip", bytes);
    assert_eq!(fs::read(file.execution_path).unwrap(), bytes);
}

#[test]
fn session_collection_reclaims_adjacent_outputs_and_nested_folders_in_bounded_steps() {
    let (directory, store, alice, _) = fixture();
    let task = "00000000-0000-7000-8000-000000000001";
    let attachment = write_upload(&store, alice, task, "expanded-archive", b"archive");
    let root = Path::new(&attachment.execution_path).parent().unwrap();
    fs::write(root.join(".DS_Store"), b"finder metadata").unwrap();
    let nested = root.join("unpacked").join("nested");
    fs::create_dir_all(&nested).unwrap();
    for index in 0..300 {
        fs::write(
            nested.join(format!("result-{index}")),
            b"model-created output",
        )
        .unwrap();
    }
    let outside = directory.path().join("outside-folder");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("keep"), b"unrelated bytes").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, root.join("external-link")).unwrap();
    store.mark_task_deleted(alice, task, 2).unwrap();
    let mut steps = 0;
    loop {
        let progress = store.collect_task_attachments_step(alice, task).unwrap();
        assert!(progress.files <= 128);
        steps += 1;
        if progress.done {
            break;
        }
        assert!(steps < 100, "bounded attachment cleanup did not finish");
    }
    assert!(steps > 1);
    assert!(!root.exists());
    assert_eq!(fs::read(outside.join("keep")).unwrap(), b"unrelated bytes");
}
