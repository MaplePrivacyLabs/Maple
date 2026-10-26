use std::fs;

use base64::{Engine as _, engine::general_purpose::STANDARD};

use super::*;
use crate::store::{
    AccountId, DamageReason, Durability, ImageUpload, NewTask, StorePaths, TaskKind, TaskRecord,
};

const PRIVATE_MARKER: &str = "SALVAGE-PRIVATE-CONTENT-MARKER";

fn fixture() -> (tempfile::TempDir, Store, AccountId) {
    let directory = tempfile::tempdir().unwrap();
    let store =
        Store::unavailable(StorePaths::new(directory.path()), StoreState::Maintenance).unwrap();
    store.initialize_recovered().unwrap();
    let account = store.upsert_account("salvage-fixture", 1).unwrap();
    (directory, store, account)
}

fn task(store: &Store, account: AccountId, number: u64) -> TaskRecord {
    let task = store
        .create_task(
            account,
            &NewTask {
                uuid: format!("00000000-0000-7000-8000-{number:012x}"),
                project_root: if cfg!(windows) {
                    "C:/salvage-fixture"
                } else {
                    "/salvage-fixture"
                }
                .into(),
                kind: TaskKind::Desktop,
                title: format!("Task {number}"),
                provisional: false,
                model: None,
                created_ms: 1,
            },
        )
        .unwrap();
    let header = SessionHeader {
        version: 1,
        id: task.uuid.clone(),
        timestamp: 1,
        cwd: if cfg!(windows) {
            "C:/salvage-fixture"
        } else {
            "/salvage-fixture"
        }
        .into(),
        parent_session: None,
    };
    let entry = SessionEntry {
        id: "00000001".into(),
        parent_id: None,
        timestamp: 2,
        kind: EntryKind::SessionInfo {
            name: Some(PRIVATE_MARKER.into()),
        },
    };
    let payload = serde_json::to_vec(&entry).unwrap();
    store.write(Durability::Durable,|transaction| {
        transaction.execute("UPDATE tasks SET pi_header=?1,last_seq=1,timeline_format=1,rows_epoch=7,last_row_seq=1 WHERE id=?2",params![serde_json::to_string(&header).unwrap(),task.key.0])?;
        transaction.execute("INSERT INTO entries(task_id,seq,entry_id,kind,created_ms,flags,codec,bytes,payload) VALUES (?1,1,?2,'session_info',2,0,0,?3,?4)",params![task.key.0,entry.id,payload.len() as i64,payload])?;
        transaction.execute("INSERT INTO timeline_rows(task_id,epoch,row_seq,row_key,entry_seq,item_index,update_seq,codec,bytes,item) VALUES (?1,7,1,'derived',1,0,1,0,2,x'7b7d')",[task.key.0])?;
        Ok(())
    }).unwrap();
    task
}

fn damage(store: &Store) {
    *store.state.write().unwrap() = StoreState::Damaged {
        reason: DamageReason::Database,
    };
    store.quarantine_generation().unwrap();
}

fn upload(store: &Store, account: AccountId, task: &TaskRecord) -> String {
    store
        .store_image_uploads(
            account,
            &task.uuid,
            &[ImageUpload {
                name: "image.png".into(),
                data_url: format!(
                    "data:image/png;base64,{}",
                    STANDARD.encode(b"\x89PNG\r\n\x1a\nSALVAGE_IMAGE_BYTES")
                ),
            }],
        )
        .unwrap()[0]
        .id
        .clone()
}

#[test]
fn invalid_source_drops_whole_task_and_keeps_readable_history_files_and_settings() {
    let (_directory, store, account) = fixture();
    let good = task(&store, account, 1);
    let bad = task(&store, account, 2);
    let attachment = upload(&store, account, &good);
    upload(&store, account, &bad);
    store.write(Durability::Durable,|transaction| {
        transaction.execute("UPDATE entries SET payload=x'7b7d',bytes=2 WHERE task_id=?1",[bad.key.0])?;
        transaction.execute("INSERT INTO app_settings(key,value,updated_ms) VALUES ('fixture',?1,1)",[format!("\"{PRIVATE_MARKER}\"")])?;
        transaction.execute("INSERT INTO credentials(slot,account_id,api_url,session_id,revision,access_token,refresh_token,updated_ms) VALUES (1,?1,'https://example.invalid','fixture',1,?2,?2,1)",params![account.0,PRIVATE_MARKER.as_bytes()])?;
        Ok(())
    }).unwrap();
    let generation = store.store_id().unwrap();
    damage(&store);
    let preview = store
        .preview_salvage(Duration::from_secs(5), &IntegrityCancellation::default())
        .unwrap();
    assert_eq!(preview.report().retained_tasks, 1);
    assert_eq!(preview.report().dropped_tasks, 1);
    assert_eq!(preview.report().tasks_to_rebuild, 1);
    assert!(
        preview
            .report()
            .dropped_rows
            .iter()
            .any(|row| row.table == SalvageTable::Tasks
                && row.key == SalvageRowKey::Integer { id: bad.key.0 })
    );
    assert!(
        preview
            .report()
            .dropped_rows
            .iter()
            .any(|row| row.table == SalvageTable::Entries)
    );
    assert!(
        !serde_json::to_string(preview.report())
            .unwrap()
            .contains(PRIVATE_MARKER)
    );
    let report = store
        .save_readable_data(&preview, SALVAGE_CONFIRMATION)
        .unwrap();
    let mut expected = preview.report().clone();
    expected.replacement_generation = Some(hex(&store.store_id().unwrap()));
    assert_eq!(report, expected);
    assert_eq!(store.state(), StoreState::Open);
    assert_ne!(store.store_id().unwrap(), generation);
    assert!(store.task(account, &good.uuid).unwrap().is_some());
    assert!(store.task(account, &bad.uuid).unwrap().is_none());
    assert_eq!(
        store
            .read_attachment(account, &good.uuid, &attachment, 0, 1024)
            .unwrap()
            .data,
        b"\x89PNG\r\n\x1a\nSALVAGE_IMAGE_BYTES"
    );
    store
        .read(|transaction| {
            assert_eq!(
                transaction.query_row::<Vec<u8>, _, _>(
                    "SELECT refresh_token FROM credentials",
                    [],
                    |row| row.get(0)
                )?,
                PRIVATE_MARKER.as_bytes()
            );
            assert_eq!(
                transaction.query_row::<String, _, _>(
                    "SELECT value FROM app_settings WHERE key='fixture'",
                    [],
                    |row| row.get(0)
                )?,
                format!("\"{PRIVATE_MARKER}\"")
            );
            assert_eq!(
                transaction
                    .query_row::<i64, _, _>("SELECT count(*) FROM entries", [], |row| row.get(0))?,
                1
            );
            assert_eq!(
                transaction.query_row::<i64, _, _>(
                    "SELECT count(*) FROM timeline_rows",
                    [],
                    |row| row.get(0)
                )?,
                0
            );
            assert_eq!(
                transaction.query_row::<i64, _, _>(
                    "SELECT timeline_format FROM tasks",
                    [],
                    |row| row.get(0)
                )?,
                0
            );
            Ok(())
        })
        .unwrap();
    assert!(
        !store
            .paths()
            .root()
            .join(format!("db.damaged-{}", hex(&generation)))
            .exists()
    );
    for staging in ["db.old", "db.new", "db.tmp"] {
        assert!(!store.paths().root().join(staging).exists());
    }
}

#[test]
fn missing_attachment_file_does_not_discard_readable_session_history() {
    let (_directory, store, account) = fixture();
    let first = task(&store, account, 1);
    let attachment = upload(&store, account, &first);
    let path = store
        .paths()
        .root()
        .join("attachments")
        .join(hex(&store.store_id().unwrap()))
        .join(account.0.to_string())
        .join(&first.uuid)
        .join(&attachment)
        .join("image.png");
    fs::remove_file(path).unwrap();
    damage(&store);
    let preview = store
        .preview_salvage(Duration::from_secs(5), &IntegrityCancellation::default())
        .unwrap();
    assert_eq!(preview.report().retained_tasks, 1);
    assert_eq!(preview.report().dropped_tasks, 0);
    assert!(preview.report().dropped_rows.is_empty());
    store
        .save_readable_data(&preview, SALVAGE_CONFIRMATION)
        .unwrap();
    assert!(store.task(account, &first.uuid).unwrap().is_some());
    assert!(
        store
            .read_attachment(account, &first.uuid, &attachment, 0, 1024)
            .is_err()
    );
}

#[test]
fn stale_review_and_missing_confirmation_preserve_the_original_generation() {
    let (_directory, store, account) = fixture();
    task(&store, account, 1);
    let generation = store.store_id().unwrap();
    damage(&store);
    let preview = store
        .preview_salvage(Duration::from_secs(5), &IntegrityCancellation::default())
        .unwrap();
    assert_eq!(
        store.save_readable_data(&preview, ""),
        Err(SalvageError::Lifecycle(LifecycleError::Confirmation))
    );
    // Simulate an out-of-process edit after review. The real source remains
    // quarantined; the test never bypasses Store's ordinary write gate.
    let source = Connection::open(store.paths().database()).unwrap();
    source
        .execute("UPDATE tasks SET title='changed-after-preview'", [])
        .unwrap();
    drop(source);
    assert_eq!(
        store.save_readable_data(&preview, SALVAGE_CONFIRMATION),
        Err(SalvageError::StalePreview)
    );
    assert!(matches!(store.state(), StoreState::Damaged { .. }));
    assert!(
        !store
            .paths()
            .root()
            .join(format!("db.damaged-{}", hex(&generation)))
            .exists()
    );
    let source = super::super::open_readonly(store.paths(), None).unwrap();
    let actual: Vec<u8> = source
        .query_row(
            "SELECT value FROM store_meta WHERE key='store_id'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(actual, generation);
}

#[test]
fn cancelled_preview_does_not_create_a_replacement_or_archive() {
    let (_directory, store, account) = fixture();
    task(&store, account, 1);
    damage(&store);
    let cancel = IntegrityCancellation::default();
    cancel.cancel();
    assert!(matches!(
        store.preview_salvage(Duration::from_secs(5), &cancel),
        Err(SalvageError::Cancelled)
    ));
    assert!(!store.paths().root().join("db.tmp").exists());
    assert!(!store.paths().root().join("db.new").exists());
}

#[test]
fn valid_compaction_self_boundary_and_root_branch_summary_are_retained() {
    let (_directory, store, account) = fixture();
    let task = task(&store, account, 1);
    let first = SessionEntry {
        id: "00000001".into(),
        parent_id: None,
        timestamp: 2,
        kind: EntryKind::BranchSummary {
            from_id: "root".into(),
            summary: "empty branch".into(),
            details: None,
            usage: None,
            from_extension: false,
        },
    };
    let second = SessionEntry {
        id: "00000002".into(),
        parent_id: Some(first.id.clone()),
        timestamp: 3,
        kind: EntryKind::Compaction {
            summary: "keep nothing earlier".into(),
            first_kept_entry_id: "00000002".into(),
            tokens_before: 0,
            details: None,
            usage: None,
            from_extension: false,
            system_message: None,
        },
    };
    store.write(Durability::Durable,|transaction| {
        let first=serde_json::to_vec(&first).unwrap();
        let second=serde_json::to_vec(&second).unwrap();
        transaction.execute("UPDATE entries SET payload=?1,bytes=?2,kind='branch_summary' WHERE task_id=?3",params![first,first.len() as i64,task.key.0])?;
        transaction.execute("INSERT INTO entries(task_id,seq,entry_id,kind,created_ms,flags,codec,bytes,payload) VALUES (?1,2,'00000002','compaction',3,0,0,?2,?3)",params![task.key.0,second.len() as i64,second])?;
        transaction.execute("UPDATE tasks SET last_seq=2 WHERE id=?1",[task.key.0])?;
        Ok(())
    }).unwrap();
    damage(&store);
    let preview = store
        .preview_salvage(Duration::from_secs(5), &IntegrityCancellation::default())
        .unwrap();
    assert_eq!(preview.report().retained_tasks, 1);
    assert!(preview.report().dropped_rows.is_empty());
    store
        .save_readable_data(&preview, SALVAGE_CONFIRMATION)
        .unwrap();
    assert_eq!(
        store.task(account, &task.uuid).unwrap().unwrap().last_seq,
        2
    );
}

#[test]
fn unenumerable_table_page_is_refused_without_replacing_or_scanning_previous_data() {
    let (directory, store, account) = fixture();
    task(&store, account, 1);
    let previous = directory.path().join("previous-application-state");
    fs::write(&previous, PRIVATE_MARKER).unwrap();
    let root: i64 = store
        .read(|transaction| {
            Ok(transaction.query_row(
                "SELECT rootpage FROM sqlite_schema WHERE name='entries'",
                [],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    damage(&store);
    store.close().unwrap();
    let paths = store.paths().clone();
    let database = paths.database();
    drop(store);
    // Checkpoint was deliberately forbidden after quarantine. Flush the WAL
    // into this disposable fixture first using a separate owned connection.
    let source = Connection::open(&database).unwrap();
    source
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    drop(source);
    let mut bytes = fs::read(&database).unwrap();
    assert!(matches!(bytes[(root as usize - 1) * 4096], 0x05 | 0x0d));
    bytes[(root as usize - 1) * 4096] = 0xff;
    fs::write(&database, &bytes).unwrap();
    let store = Store::unavailable(paths, StoreState::Maintenance).unwrap();
    store.initialize_recovered().unwrap();
    assert!(matches!(store.state(), StoreState::Damaged { .. }));
    let result = store.preview_salvage(Duration::from_secs(5), &IntegrityCancellation::default());
    assert!(
        matches!(&result, Err(SalvageError::UnenumerableSource)),
        "sanitized recovery outcome: {:?}",
        result.as_ref().map(|_| ())
    );
    assert_eq!(fs::read(&database).unwrap(), bytes);
    assert_eq!(fs::read_to_string(previous).unwrap(), PRIVATE_MARKER);
    assert!(!store.paths().root().join("db.tmp").exists());
}

// These checks exercise the scanner and candidate copy directly. Lifecycle
// installation, retention and cold-crash coverage belong to its own tests.
fn candidate(source: &Connection) -> (Scan, Connection) {
    let scanned = scan(source).unwrap();
    let mut destination = Connection::open_in_memory().unwrap();
    destination
        .pragma_update(None, "application_id", schema::APPLICATION_ID)
        .unwrap();
    destination
        .pragma_update(None, "foreign_keys", true)
        .unwrap();
    schema::migrate(&mut destination, 1).unwrap();
    copy_rows(source, &mut destination, &scanned).unwrap();
    assert_eq!(
        destination
            .query_row::<String, _, _>("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap(),
        "ok"
    );
    let mut check = destination.prepare("PRAGMA foreign_key_check").unwrap();
    assert!(check.query([]).unwrap().next().unwrap().is_none());
    drop(check);
    (scanned, destination)
}

fn corrupt_fixture_root(store: &Store, object: &str) -> Vec<u8> {
    let root: i64 = store
        .read(|connection| {
            Ok(connection.query_row(
                "SELECT rootpage FROM sqlite_schema WHERE name=?1",
                [object],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    store.close().unwrap();
    let source = Connection::open(store.paths().database()).unwrap();
    source
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    drop(source);
    let mut bytes = fs::read(store.paths().database()).unwrap();
    assert!(root > 1);
    let offset = (root as usize - 1) * 4096;
    assert!(matches!(bytes[offset], 0x02 | 0x05 | 0x0a | 0x0d));
    bytes[offset] = 0xff;
    fs::write(store.paths().database(), &bytes).unwrap();
    bytes
}

#[test]
fn authoritative_walk_and_image_lookup_open_only_the_primary_tree_plain_and_keyed() {
    use crate::store::keys::{KdfParams, Password};

    for encrypted in [false, true] {
        let (_directory, store, account) = fixture();
        let task = task(&store, account, 1);
        upload(&store, account, &task);
        if encrypted {
            let password = Password::normalize("Z7jv!9qP#3nMx@8Ks^2dH").unwrap();
            store
                .set_store_password(&password, &[], KdfParams::floor())
                .unwrap();
        }
        store
            .with_authenticated_store_key(|key| {
                let source = super::super::open_readonly(store.paths(), key)?;
                for table in TABLES {
                    let root: i64 = source.query_row(
                        "SELECT rootpage FROM sqlite_schema WHERE name=?1",
                        [table.name()],
                        |row| row.get(0),
                    )?;
                    let mut explain = source
                        .prepare(&format!("EXPLAIN SELECT * FROM {}", table.primary_source()))?;
                    let roots = explain
                        .query_map([], |row| {
                            Ok((row.get::<_, String>(1)?, row.get::<_, i64>(3)?))
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?
                        .into_iter()
                        .filter_map(|(opcode, root)| (opcode == "OpenRead").then_some(root))
                        .collect::<Vec<_>>();
                    assert_eq!(roots, [root], "{} authoritative root", table.name());
                }
                let root: i64 = source.query_row(
                    "SELECT rootpage FROM sqlite_schema WHERE name='file_attachments'",
                    [],
                    |row| row.get(0),
                )?;
                let mut explain = source.prepare(&format!("EXPLAIN {TASK_HAS_ATTACHMENT_SQL}"))?;
                let roots = explain
                    .query_map(
                        params![task.key.0, "00000000000000000000000000000000"],
                        |row| Ok((row.get::<_, String>(1)?, row.get::<_, i64>(3)?)),
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?
                    .into_iter()
                    .filter_map(|(opcode, root)| (opcode == "OpenRead").then_some(root))
                    .collect::<Vec<_>>();
                assert_eq!(roots, [root]);
                Ok(())
            })
            .unwrap();
    }
}

#[test]
fn corrupt_attachment_index_does_not_lose_primary_metadata_or_image_markers() {
    let (_directory, store, account) = fixture();
    let task = task(&store, account, 1);
    upload(&store, account, &task);
    store
        .write(Durability::Durable, |transaction| {
            let attachment: String = transaction.query_row(
                "SELECT public_id FROM file_attachments WHERE task_id=?1",
                [task.key.0],
                |row| row.get(0),
            )?;
            let entry = SessionEntry {
                id: "00000001".into(),
                parent_id: None,
                timestamp: 2,
                kind: EntryKind::Message {
                    message: SessionMessage::Llm(Message::User(pi_ai::UserMessage {
                        content: vec![Content::image(
                            format!("maple-attachment://{attachment}"),
                            "image/png",
                        )],
                        timestamp: 2,
                    })),
                },
            };
            let payload = serde_json::to_vec(&entry).unwrap();
            transaction.execute(
                "UPDATE entries SET kind='message',flags=1,bytes=?1,payload=?2 WHERE task_id=?3",
                params![payload.len() as i64, payload, task.key.0],
            )?;
            Ok(())
        })
        .unwrap();
    let bytes = corrupt_fixture_root(&store, "file_attachments_task");
    let source = super::super::open_readonly(store.paths(), None).unwrap();
    assert!(
        source
            .query_row::<i64, _, _>(
                "SELECT task_id FROM file_attachments INDEXED BY file_attachments_task LIMIT 1",
                [],
                |row| row.get(0),
            )
            .is_err()
    );
    let (scanned, destination) = candidate(&source);
    assert_eq!(scanned.preview.report.retained_tasks, 1);
    assert_eq!(scanned.preview.report.tasks_to_rebuild, 1);
    assert!(scanned.preview.report.dropped_rows.is_empty());
    for table in ["file_attachments", "entries"] {
        assert_eq!(
            destination
                .query_row::<i64, _, _>(&format!("SELECT count(*) FROM {table}"), [], |row| row
                    .get(0))
                .unwrap(),
            1
        );
    }
    assert_eq!(fs::read(store.paths().database()).unwrap(), bytes);
}

#[test]
fn unreadable_derived_timeline_is_not_part_of_the_authoritative_preview() {
    let (_directory, store, account) = fixture();
    task(&store, account, 1);
    let before = store
        .with_authenticated_store_key(|key| {
            let source = super::super::open_readonly(store.paths(), key)?;
            scan(&source)
        })
        .unwrap();
    let bytes = corrupt_fixture_root(&store, "timeline_rows");
    let source = super::super::open_readonly(store.paths(), None).unwrap();
    assert!(
        source
            .query_row::<i64, _, _>(
                "SELECT id FROM timeline_rows NOT INDEXED LIMIT 1",
                [],
                |row| row.get(0)
            )
            .is_err()
    );
    let (after, destination) = candidate(&source);
    assert_eq!(after.preview.fingerprint, before.preview.fingerprint);
    assert_eq!(after.preview.report, before.preview.report);
    assert_eq!(after.preview.report.tasks_to_rebuild, 1);
    assert_eq!(
        destination
            .query_row::<i64, _, _>("SELECT count(*) FROM timeline_rows", [], |row| row.get(0))
            .unwrap(),
        0
    );
    assert_eq!(fs::read(store.paths().database()).unwrap(), bytes);
}

#[test]
fn tombstoned_partial_collection_is_preserved_without_exhausting_the_loss_review() {
    use crate::store::keys::{KdfParams, Password};

    for encrypted in [false, true] {
        let (_directory, store, account) = fixture();
        let live = task(&store, account, 1);
        let deleted = task(&store, account, 2);
        upload(&store, account, &deleted);
        store.write(Durability::Durable, |transaction| {
            transaction.execute("UPDATE tasks SET deleted_ms=3,last_seq=20000 WHERE id=?1", [deleted.key.0])?;
            // A normal partial collection has removed the sequence prefix. More
            // remaining rows than MAX_LOSS_ROWS must not become new loss rows.
            transaction.execute("DELETE FROM entries WHERE task_id=?1", [deleted.key.0])?;
            transaction.execute(
                "WITH RECURSIVE sequence(seq) AS (SELECT 1000 UNION ALL SELECT seq+1 FROM sequence WHERE seq<11000)
                 INSERT INTO entries(task_id,seq,entry_id,kind,created_ms,codec,bytes,payload)
                 SELECT ?1,seq,printf('%08x',seq),'session_info',2,0,2,x'7b7d' FROM sequence",
                [deleted.key.0],
            )?;
            // An unfinished upload remains owned by this tombstone. Account deletion is only pending:
            // salvage must preserve its phase, credentials and remaining source.
            transaction.execute("UPDATE file_attachments SET state='uploading'", [])?;
            transaction.execute("UPDATE accounts SET deleting_ms=3,deletion_phase='pending_server',deletion_generation='pending-fixture' WHERE id=?1", [account.0])?;
            transaction.execute("INSERT INTO credentials(slot,account_id,api_url,session_id,revision,access_token,refresh_token,updated_ms) VALUES (1,?1,'https://example.invalid','fixture',1,?2,?2,1)",params![account.0,PRIVATE_MARKER.as_bytes()])?;
            transaction.execute("INSERT INTO store_meta(key,value) VALUES ('integrity_incomplete_attempts',1),('integrity_retry_after_ms',5000)", [])?;
            Ok(())
        }).unwrap();
        if encrypted {
            let password = Password::normalize("Z7jv!9qP#3nMx@8Ks^2dH").unwrap();
            store
                .set_store_password(&password, &[], KdfParams::floor())
                .unwrap();
        }
        store
            .with_authenticated_store_key(|key| {
                let source = super::super::open_readonly(store.paths(), key)?;
                let (scanned, destination) = candidate(&source);
                assert_eq!(scanned.preview.report.retained_tasks, 1);
                assert_eq!(scanned.preview.report.dropped_tasks, 0);
                assert_eq!(scanned.preview.report.tasks_to_rebuild, 1);
                assert!(scanned.preview.report.dropped_rows.is_empty());
                assert_eq!(
                    destination.query_row::<i64, _, _>(
                        "SELECT count(*) FROM entries WHERE task_id=?1",
                        [deleted.key.0],
                        |row| row.get(0)
                    )?,
                    10001
                );
                assert_eq!(
                    destination.query_row::<i64, _, _>(
                        "SELECT deleted_ms FROM tasks WHERE id=?1",
                        [deleted.key.0],
                        |row| row.get(0)
                    )?,
                    3
                );
                assert_eq!(
                    destination.query_row::<i64, _, _>(
                        "SELECT last_seq FROM tasks WHERE id=?1",
                        [deleted.key.0],
                        |row| row.get(0)
                    )?,
                    20000
                );
                assert_eq!(
                    destination.query_row::<i64, _, _>(
                        "SELECT count(*) FROM entries WHERE task_id=?1",
                        [live.key.0],
                        |row| row.get(0)
                    )?,
                    1
                );
                assert_eq!(
                    destination.query_row::<String, _, _>(
                        "SELECT deletion_phase FROM accounts WHERE id=?1",
                        [account.0],
                        |row| row.get(0)
                    )?,
                    "pending_server"
                );
                assert_eq!(
                    destination.query_row::<String, _, _>(
                        "SELECT deletion_generation FROM accounts WHERE id=?1",
                        [account.0],
                        |row| row.get(0)
                    )?,
                    "pending-fixture"
                );
                assert_eq!(
                    destination.query_row::<Vec<u8>, _, _>(
                        "SELECT refresh_token FROM credentials WHERE account_id=?1",
                        [account.0],
                        |row| row.get(0)
                    )?,
                    PRIVATE_MARKER.as_bytes()
                );
                assert_eq!(
                    destination.query_row::<String, _, _>(
                        "SELECT state FROM file_attachments",
                        [],
                        |row| row.get(0)
                    )?,
                    "uploading"
                );
                Ok(())
            })
            .unwrap();
    }
}

#[test]
fn attachment_account_mismatch_cannot_be_salvaged_into_a_different_owner() {
    let (_directory, store, account) = fixture();
    let other = store.upsert_account("other-file-owner", 1).unwrap();
    let session = task(&store, account, 1);
    upload(&store, account, &session);
    store
        .write(Durability::Durable, |tx| {
            tx.execute("UPDATE file_attachments SET account_id=?1", [other.0])?;
            Ok(())
        })
        .unwrap();
    let source = super::super::open_readonly(store.paths(), None).unwrap();
    assert!(matches!(scan(&source), Err(StoreError::InvalidData)));
}

#[test]
fn tombstone_does_not_turn_broken_authoritative_ownership_into_completed_deletion() {
    for broken_project in [false, true] {
        let (_directory, store, account) = fixture();
        let deleted = task(&store, account, 1);
        upload(&store, account, &deleted);
        store.mark_task_deleted(account, &deleted.uuid, 3).unwrap();
        let source = Connection::open(store.paths().database()).unwrap();
        source.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
        if broken_project {
            source
                .execute(
                    "UPDATE tasks SET project_id=999 WHERE id=?1",
                    [deleted.key.0],
                )
                .unwrap();
        } else {
            source
                .execute(
                    "UPDATE file_attachments SET account_id=999 WHERE task_id=?1",
                    [deleted.key.0],
                )
                .unwrap();
        }
        drop(source);
        let source = super::super::open_readonly(store.paths(), None).unwrap();
        assert!(matches!(scan(&source), Err(StoreError::InvalidData)));
        assert!(!store.paths().root().join("db.tmp").exists());
    }
}

#[test]
fn keyed_salvage_copies_retained_files_to_the_replacement_generation() {
    use crate::store::keys::{KdfParams, Password};
    let (_directory, store, account) = fixture();
    let session = task(&store, account, 1);
    let attachment = upload(&store, account, &session);
    let old = hex(&store.store_id().unwrap());
    let password = Password::normalize("Z7jv!9qP#3nMx@8Ks^2dH").unwrap();
    store
        .set_store_password(&password, &[], KdfParams::floor())
        .unwrap();
    assert_eq!(
        store
            .read_attachment(account, &session.uuid, &attachment, 0, 1024)
            .unwrap()
            .data,
        b"\x89PNG\r\n\x1a\nSALVAGE_IMAGE_BYTES"
    );
    damage(&store);
    store.authenticate_repair_password(&password).unwrap();
    let preview = store
        .preview_salvage(Duration::from_secs(5), &IntegrityCancellation::default())
        .unwrap();
    store
        .save_readable_data(&preview, SALVAGE_CONFIRMATION)
        .unwrap();
    assert_ne!(hex(&store.store_id().unwrap()), old);
    assert_eq!(
        store
            .read_attachment(account, &session.uuid, &attachment, 0, 1024)
            .unwrap()
            .data,
        b"\x89PNG\r\n\x1a\nSALVAGE_IMAGE_BYTES"
    );
    assert!(!store.paths().root().join("attachments").join(old).exists());
}
