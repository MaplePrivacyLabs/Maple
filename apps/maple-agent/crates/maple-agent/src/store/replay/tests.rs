use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use base64::Engine;
use pi_ai::{
    AssistantContent, AssistantMessage, Content, Message, StopReason, ToolCall, ToolResultMessage,
    Usage, UserMessage,
};
use pi_coding_agent::SessionMessage;
use pi_coding_agent::session::SessionManager;
use serde_json::json;

use super::*;
use crate::agent::{AgentPersistedRowIdentity, AgentTimelineItem};
use crate::store::session::SessionLoad;
use crate::store::{
    Durability, HistoryPageRequest, NewTask, PageTransport, RowRangeRequest, RowReader, StorePaths,
    TaskKind,
};

const TASK: &str = "00000000-0000-7000-8000-000000000001";
const CAPTURE_BUDGET: Duration = Duration::from_secs(30);

type ReplayRow = (AgentTimelineItem, AgentPersistedRowIdentity);

fn fixture(keyed: bool) -> (tempfile::TempDir, Arc<Store>, AccountId, SessionManager) {
    let directory = tempfile::tempdir().unwrap();
    let paths = StorePaths::new(directory.path());
    let store = Arc::new(if keyed {
        Store::open_keyed(
            paths,
            crate::store::keys::LockedKey::from_bytes([0x74; 32]).unwrap(),
        )
        .unwrap()
    } else {
        Store::open(paths).unwrap()
    });
    let account = store.upsert_account("replay@example.test", 1).unwrap();
    store
        .create_task(
            account,
            &NewTask {
                uuid: TASK.into(),
                project_root: if cfg!(windows) {
                    "C:/replay"
                } else {
                    "/replay"
                }
                .into(),
                kind: TaskKind::Desktop,
                title: "Replay snapshot".into(),
                provisional: false,
                model: None,
                created_ms: 1,
            },
        )
        .unwrap();
    let manager = store
        .load_session(account, TASK, SessionLoad::Context)
        .unwrap()
        .into_manager("/replay");
    (directory, store, account, manager)
}

fn user(text: &str, timestamp: i64) -> SessionMessage {
    SessionMessage::Llm(Message::User(UserMessage {
        content: vec![Content::text(text)],
        timestamp,
    }))
}

fn call(input: &str) -> SessionMessage {
    SessionMessage::Llm(Message::Assistant(AssistantMessage {
        content: vec![AssistantContent::ToolCall(ToolCall {
            id: "replay-call".into(),
            name: "read".into(),
            arguments: json!({"path":"README.md", "payload":input})
                .as_object()
                .unwrap()
                .clone(),
            partial_json: None,
        })],
        api: "fixture".into(),
        provider: "fixture".into(),
        model: "fixture".into(),
        response_id: None,
        thinking_level: None,
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: 20,
    }))
}

fn result(text: &str, timestamp: i64) -> SessionMessage {
    SessionMessage::Llm(Message::ToolResult(ToolResultMessage {
        tool_call_id: "replay-call".into(),
        tool_name: "read".into(),
        content: vec![Content::text(text)],
        details: None,
        usage: None,
        is_error: false,
        timestamp,
    }))
}

fn capture(store: &Arc<Store>, account: AccountId) -> Arc<ReplaySnapshot> {
    let stamp = store.task_history_stamp(account, TASK).unwrap();
    let snapshot = store
        .capture_replay(
            account,
            TASK,
            stamp,
            Arc::new(AtomicBool::new(false)),
            CAPTURE_BUDGET,
        )
        .unwrap();
    assert_eq!(snapshot.stamp(), stamp);
    snapshot
}

fn normalized(mut row: ReplayRow) -> ReplayRow {
    row.0.persisted = None;
    row
}

fn drain(snapshot: &ReplaySnapshot) -> Vec<ReplayRow> {
    let mut rows = Vec::new();
    while let Some(row) = snapshot.next_row().unwrap() {
        rows.push(normalized(row));
    }
    rows
}

/// Use the ordinary paged/ranged consumer API as the baseline, including full
/// source reconstruction for reduced rows. Snapshot tests never decode scratch.
fn canonical_rows(store: &Arc<Store>, account: AccountId) -> Vec<ReplayRow> {
    let reader = RowReader::new(store.clone());
    let mut rows = Vec::new();
    let mut cursor = None;
    loop {
        let page = store
            .history_forward_page(
                account,
                &HistoryPageRequest {
                    task: TASK.into(),
                    cursor,
                    limit_rows: Some(50),
                    limit_bytes: Some(512 * 1024),
                },
                PageTransport::Local,
            )
            .unwrap();
        for row in page.rows {
            let identity = AgentPersistedRowIdentity {
                task_id: TASK.into(),
                rows_epoch: row.rows_epoch,
                row_seq: row.row_seq,
                update_seq: row.update_seq,
            };
            let item = if row.item.truncated.is_some() {
                let mut bytes = Vec::new();
                loop {
                    let range = reader
                        .read_row(
                            account,
                            &RowRangeRequest {
                                task: TASK.into(),
                                rows_epoch: row.rows_epoch,
                                row_seq: row.row_seq,
                                update_seq: Some(row.update_seq),
                                offset: bytes.len() as u64,
                                len: 128 * 1024,
                            },
                            PageTransport::Local,
                        )
                        .unwrap();
                    bytes.extend(
                        base64::engine::general_purpose::STANDARD
                            .decode(range.data)
                            .unwrap(),
                    );
                    if bytes.len() as u64 == range.total {
                        break;
                    }
                }
                serde_json::from_slice(&bytes).unwrap()
            } else {
                row.item
            };
            rows.push(normalized((item, identity)));
        }
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    rows
}

fn files_below(directory: &Path) -> BTreeSet<PathBuf> {
    let mut files = BTreeSet::new();
    for entry in std::fs::read_dir(directory).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            files.extend(files_below(&entry.path()));
        } else {
            files.insert(entry.path());
        }
    }
    files
}

#[test]
fn replay_keeps_canonical_identity_and_running_tool_while_source_progresses() {
    for keyed in [false, true] {
        let (_directory, store, account, mut manager) = fixture(keyed);
        manager.append_message(user("captured user", 10));
        manager.append_message(call("small input"));
        let expected = canonical_rows(&store, account);
        assert_eq!(expected.len(), 2);
        let snapshot = capture(&store, account);
        let first = normalized(snapshot.next_row().unwrap().unwrap());
        assert_eq!(first, expected[0]);

        manager.append_message(result("result committed during replay", 30));
        manager.append_message(user("new message after capture", 40));
        assert!(!manager.has_unpersisted());
        let current = canonical_rows(&store, account);
        assert_eq!(current.len(), 3);
        let old_tool = &expected[1];
        let new_tool = &current[1];
        assert_eq!(old_tool.0.status.as_deref(), Some("running"));
        assert_eq!(new_tool.0.status.as_deref(), Some("completed"));
        assert_eq!(old_tool.1.row_seq, new_tool.1.row_seq);
        assert!(new_tool.1.update_seq > old_tool.1.update_seq);
        assert_eq!(drain(&snapshot), expected[1..]);
        snapshot.close().unwrap();
    }
}

#[test]
fn replay_reconstructs_original_large_fields_after_same_tool_row_changes() {
    for keyed in [false, true] {
        let (_directory, store, account, mut manager) = fixture(keyed);
        // All exceed StoredItem's field cutoff; both user text and distinct
        // source origins for tool input/output must survive later row merges.
        let large_user = "captured user 🦊\n".repeat(80_000);
        let large_input = "original argument \"🦊\"\n".repeat(70_000);
        let old_output = "captured output café\n".repeat(75_000);
        let new_output = "replacement output after capture\n".repeat(50_000);
        manager.append_message(user(&large_user, 10));
        manager.append_message(call(&large_input));
        manager.append_message(result(&old_output, 30));
        let expected = canonical_rows(&store, account);
        let snapshot = capture(&store, account);
        manager.append_message(result(&new_output, 40));
        assert!(!manager.has_unpersisted());

        let frozen = drain(&snapshot);
        assert_eq!(frozen, expected);
        assert_eq!(frozen[0].0.text.as_deref(), Some(large_user.as_str()));
        let tool = &frozen[1].0;
        assert_eq!(tool.input.as_ref().unwrap()["payload"], large_input);
        assert_eq!(tool.output.as_ref().unwrap()["text"], old_output);
        assert_eq!(
            tool.output.as_ref().unwrap()["content"][0]["text"],
            old_output
        );
        let current = canonical_rows(&store, account);
        assert_eq!(current[1].0.output.as_ref().unwrap()["text"], new_output);
        assert_eq!(current[1].1.row_seq, frozen[1].1.row_seq);
        assert!(current[1].1.update_seq > frozen[1].1.update_seq);
        snapshot.close().unwrap();
    }
}

#[test]
fn replay_prefix_survives_failed_append_and_write_all_retry() {
    for keyed in [false, true] {
        let (_directory, store, account, mut manager) = fixture(keyed);
        manager.append_message(user("durable captured prefix", 10));
        let expected = canonical_rows(&store, account);
        let snapshot = capture(&store, account);
        store
            .write(Durability::Normal, |tx| {
                tx.execute_batch(
                    "CREATE TEMP TRIGGER replay_fail_append BEFORE INSERT ON entries
                     BEGIN SELECT RAISE(ABORT,'replay fixture'); END;",
                )?;
                Ok(())
            })
            .unwrap();
        manager.append_message(user("retained unpersisted tail", 20));
        assert!(manager.has_unpersisted());
        assert!(manager.take_persist_error().is_some());
        store
            .write(Durability::Normal, |tx| {
                tx.execute_batch("DROP TRIGGER replay_fail_append")?;
                Ok(())
            })
            .unwrap();
        manager.flush().unwrap();
        assert!(!manager.has_unpersisted());
        assert_eq!(canonical_rows(&store, account).len(), 2);
        assert_eq!(drain(&snapshot), expected);
        snapshot.close().unwrap();
    }
}

#[test]
fn captured_rows_do_not_pin_obsolete_epoch_rows_or_consult_the_new_epoch() {
    for keyed in [false, true] {
        let (_directory, store, account, mut manager) = fixture(keyed);
        manager.append_message(user("canonical epoch before replacement", 10));
        let expected = canonical_rows(&store, account);
        let snapshot = capture(&store, account);
        store
            .write(Durability::Normal, |tx| {
                // Mimic completed canonical replacement/collection without
                // mutating immutable source entries required for expansion.
                tx.execute(
                    "DELETE FROM timeline_rows WHERE task_id=(SELECT id FROM tasks
                     WHERE account_id=?1 AND uuid=?2)",
                    rusqlite::params![account.0, TASK],
                )?;
                tx.execute(
                    "UPDATE tasks SET rows_epoch=rows_epoch+1,last_row_seq=0
                     WHERE account_id=?1 AND uuid=?2",
                    rusqlite::params![account.0, TASK],
                )?;
                Ok(())
            })
            .unwrap();
        assert_ne!(
            store.task_history_stamp(account, TASK).unwrap(),
            snapshot.stamp()
        );
        assert!(canonical_rows(&store, account).is_empty());
        assert_eq!(drain(&snapshot), expected);
        snapshot.close().unwrap();
    }
}

#[test]
fn failed_capture_leaves_no_scratch_for_stale_marker_cancel_or_deadline() {
    for keyed in [false, true] {
        let (_directory, store, account, mut manager) = fixture(keyed);
        manager.append_message(user("before capture", 10));
        let stale = store.task_history_stamp(account, TASK).unwrap();
        manager.append_message(user("commit invalidates preflight marker", 20));
        let current = store.task_history_stamp(account, TASK).unwrap();
        let before = files_below(store.paths().root());
        for (stamp, cancelled, budget) in [
            (stale, false, CAPTURE_BUDGET),
            (current, true, CAPTURE_BUDGET),
            (current, false, Duration::ZERO),
        ] {
            assert!(
                store
                    .capture_replay(
                        account,
                        TASK,
                        stamp,
                        Arc::new(AtomicBool::new(cancelled)),
                        budget,
                    )
                    .is_err()
            );
            assert_eq!(files_below(store.paths().root()), before);
        }
        let bob = store.upsert_account("other@example.test", 2).unwrap();
        assert!(
            store
                .capture_replay(
                    bob,
                    TASK,
                    current,
                    Arc::new(AtomicBool::new(false)),
                    CAPTURE_BUDGET,
                )
                .is_err()
        );
        assert_eq!(files_below(store.paths().root()), before);
    }
}

#[test]
fn encrypted_ephemeral_scratch_is_removed_on_explicit_close_and_drop() {
    for keyed in [false, true] {
        let (_directory, store, account, mut manager) = fixture(keyed);
        let canary = "REPLAY_SCRATCH_PLAINTEXT_CANARY_7e163a9b";
        manager.append_message(user(canary, 10));
        let before = files_below(store.paths().root());
        for explicit_close in [false, true] {
            let snapshot = capture(&store, account);
            let path = snapshot.scratch_path().expect("captured scratch path");
            assert!(path.is_file());
            let during = files_below(store.paths().root());
            let scratch: Vec<_> = during.difference(&before).cloned().collect();
            assert!(
                !scratch.is_empty(),
                "capture must use owned on-disk scratch"
            );
            assert_eq!(
                scratch,
                vec![path.clone()],
                "capture must create no journal or WAL sidecars"
            );
            for path in &scratch {
                let bytes = std::fs::read(path).unwrap();
                assert!(
                    !bytes
                        .windows(canary.len())
                        .any(|window| window == canary.as_bytes())
                );
                assert!(!bytes.starts_with(b"SQLite format 3\0"));
            }
            assert_eq!(drain(&snapshot)[0].0.text.as_deref(), Some(canary));
            if explicit_close {
                snapshot.close().unwrap();
                snapshot.close().unwrap();
            }
            drop(snapshot);
            assert_eq!(files_below(store.paths().root()), before);
        }
    }
}

#[test]
fn task_or_pending_account_deletion_refuses_remaining_reconstruction() {
    for keyed in [false, true] {
        for account_deletion in [false, true] {
            let (_directory, store, account, mut manager) = fixture(keyed);
            manager.append_message(user("must not replay after authorization changes", 10));
            let before = files_below(store.paths().root());
            let snapshot = capture(&store, account);
            if account_deletion {
                store
                    .begin_account_deletion(account, "replay-delete", 20)
                    .unwrap();
            } else {
                store.mark_task_deleted(account, TASK, 20).unwrap();
                // Replay cannot obstruct collection of explicitly deleted data.
                while !store.collect_task_step(account, TASK).unwrap().done {}
            }
            assert!(snapshot.next_row().is_err());
            snapshot.close().unwrap();
            assert_eq!(files_below(store.paths().root()), before);
        }
    }
}

#[test]
fn cancellation_and_store_close_revoke_scratch_before_reuse() {
    for keyed in [false, true] {
        for close_store in [false, true] {
            let (_directory, store, account, mut manager) = fixture(keyed);
            manager.append_message(user("generation-private replay", 10));
            let cancellation = Arc::new(AtomicBool::new(false));
            let before = files_below(store.paths().root());
            let snapshot = store
                .capture_replay(
                    account,
                    TASK,
                    store.task_history_stamp(account, TASK).unwrap(),
                    cancellation.clone(),
                    CAPTURE_BUDGET,
                )
                .unwrap();
            if close_store {
                drop(manager);
                store.close().unwrap();
                assert!(files_below(store.paths().root()).is_subset(&before));
            } else {
                cancellation.store(true, Ordering::Release);
            }
            assert!(snapshot.next_row().is_err());
            snapshot.close().unwrap();
            let after = files_below(store.paths().root());
            // Closing a store can remove WAL/SHM; no capture-owned file remains.
            assert!(after.is_subset(&before));
        }
    }
}

#[test]
fn pending_registration_is_revoked_before_it_can_start_after_store_close() {
    for keyed in [false, true] {
        let (_directory, store, account, mut manager) = fixture(keyed);
        manager.append_message(user("registered before blocking spawn", 10));
        let before = files_below(store.paths().root());
        let registration = store
            .prepare_replay_capture(
                account,
                TASK,
                store.task_history_stamp(account, TASK).unwrap(),
                Arc::new(AtomicBool::new(false)),
                CAPTURE_BUDGET,
            )
            .unwrap();
        assert!(registration.snapshot.scratch_path().is_none());
        drop(manager);
        store.close().unwrap();
        assert!(registration.snapshot.closed.load(Ordering::Acquire));
        assert!(registration.run().is_err());
        assert!(files_below(store.paths().root()).is_subset(&before));
        if keyed {
            store
                .initialize_keyed(crate::store::keys::LockedKey::from_bytes([0x74; 32]).unwrap())
                .unwrap();
        } else {
            store.initialize_plain().unwrap();
        }
        let fresh = capture(&store, account);
        assert_eq!(drain(&fresh).len(), 1);
    }
}

#[test]
fn reused_account_identity_task_uuid_or_store_metadata_cannot_authorize_captured_rows() {
    for keyed in [false, true] {
        for replacement in 0..4 {
            let (_directory, store, account, mut manager) = fixture(keyed);
            manager.append_message(user("captured under original authority", 10));
            let before = files_below(store.paths().root());
            let snapshot = capture(&store, account);
            store
                .write(Durability::Normal, |tx| {
                    match replacement {
                        0 => {
                            tx.execute(
                                "UPDATE accounts SET user_id='different-user@example.test' WHERE id=?1",
                                [account.0],
                            )?;
                        }
                        1 => {
                            tx.execute(
                                "UPDATE accounts SET user_key='different-key@example.test' WHERE id=?1",
                                [account.0],
                            )?;
                        }
                        2 => {
                            // The public UUID is reused while the old source
                            // remains at its previous internal task key.
                            tx.execute(
                                "UPDATE tasks SET uuid='00000000-0000-7000-8000-000000000002'
                                 WHERE account_id=?1 AND uuid=?2",
                                rusqlite::params![account.0, TASK],
                            )?;
                            tx.execute(
                                "INSERT INTO tasks(account_id,project_id,uuid,kind,state,title,title_fold,
                                 created_ms,updated_ms)
                                 SELECT account_id,project_id,?2,'desktop','active','Reused UUID',
                                 'reused uuid',1,1 FROM tasks WHERE account_id=?1",
                                rusqlite::params![account.0, TASK],
                            )?;
                        }
                        3 => {
                            tx.execute(
                                "UPDATE store_meta SET value=?1 WHERE key='store_id'",
                                [vec![0xa5u8; 16]],
                            )?;
                        }
                        _ => unreachable!(),
                    }
                    Ok(())
                })
                .unwrap();
            assert_eq!(snapshot.next_row(), Err(StoreError::Conflict));
            assert!(snapshot.scratch_path().is_none());
            assert_eq!(files_below(store.paths().root()), before);
        }
    }
}

#[test]
fn mid_capture_cancellation_drains_the_worker_before_releasing_scratch() {
    // Temporary Windows diagnosis: retain the ordinary suite's concurrent load
    // while exercising every keyed/close combination repeatedly.
    let iterations = if cfg!(windows) { 20 } else { 1 };
    for iteration in 1..=iterations {
        for keyed in [false, true] {
            for close_store in [false, true] {
                let (_directory, store, account, mut manager) = fixture(keyed);
                manager.append_message(user("first copied row", 10));
                manager.append_message(user("remaining uncopied row", 20));
                let before = files_below(store.paths().root());
                let cancellation = Arc::new(AtomicBool::new(false));
                let registration = store
                    .prepare_replay_capture(
                        account,
                        TASK,
                        store.task_history_stamp(account, TASK).unwrap(),
                        cancellation.clone(),
                        CAPTURE_BUDGET,
                    )
                    .unwrap();
                let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
                let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
                *registration.snapshot.capture_hook.lock().unwrap() = Some(Box::new(move || {
                    entered_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(30)).unwrap();
                }));
                let worker = std::thread::spawn(move || {
                    let result = registration.run();
                    (result, capture_last_stage())
                });
                if let Err(entered) = entered_rx.recv_timeout(Duration::from_secs(10)) {
                    // A failed capture drops the hook sender. Preserve the actual
                    // worker result instead of reporting only Disconnected, and
                    // release a late hook before joining after a timeout.
                    let _ = release_tx.send(());
                    match worker.join() {
                        Ok((Err(error), stage)) => panic!(
                            "capture did not reach its first-row hook: {entered:?}; \
                         iteration={iteration}, keyed={keyed}, close_store={close_store}; \
                         stage={stage}; worker failed: {error:?}"
                        ),
                        Ok((Ok(snapshot), stage)) => {
                            let closed = snapshot.close();
                            panic!(
                                "capture did not reach its first-row hook: {entered:?}; \
                             iteration={iteration}, keyed={keyed}, close_store={close_store}; \
                             stage={stage}; worker completed without the hook; close={closed:?}"
                            );
                        }
                        Err(panic) => {
                            eprintln!(
                                "capture worker panicked before its first-row hook: {entered:?}; \
                             iteration={iteration}, keyed={keyed}, close_store={close_store}"
                            );
                            std::panic::resume_unwind(panic);
                        }
                    }
                }
                assert!(
                    files_below(store.paths().root())
                        .difference(&before)
                        .next()
                        .is_some()
                );

                let draining_store = store.clone();
                let (drained_tx, drained_rx) = std::sync::mpsc::sync_channel(1);
                let draining = std::thread::spawn(move || {
                    let result = if close_store {
                        draining_store.close()
                    } else {
                        draining_store.cancel_all_replays()
                    };
                    drained_tx.send(result).unwrap();
                });
                let started = std::time::Instant::now();
                while !cancellation.load(Ordering::Acquire)
                    && started.elapsed() < Duration::from_secs(5)
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
                let cancellation_observed = cancellation.load(Ordering::Acquire);
                let early_drain = drained_rx.try_recv();
                // Release before assertions so a failing check never leaves a
                // blocking capture or drain detached behind the test harness.
                release_tx.send(()).unwrap();
                let (captured, _) = worker.join().unwrap();
                let drained = drained_rx.recv_timeout(Duration::from_secs(10));
                draining.join().unwrap();
                assert!(cancellation_observed);
                assert!(matches!(
                    early_drain,
                    Err(std::sync::mpsc::TryRecvError::Empty)
                ));
                assert!(captured.is_err());
                drained.unwrap().unwrap();
                assert!(files_below(store.paths().root()).is_subset(&before));
            }
        }
    }
}

#[test]
fn production_startup_unlock_and_password_exports_restore_replay_admission() {
    use crate::store::StoreState;
    use crate::store::keys::{KdfParams, Password};

    let (_directory, store, account, mut manager) = fixture(false);
    manager.append_message(user("history survives host lifecycle transitions", 10));
    let expected = canonical_rows(&store, account);
    let stamp = store.task_history_stamp(account, TASK).unwrap();
    let paths = store.paths().clone();
    drop(manager);
    store.close().unwrap();
    drop(store);

    // Production host startup uses unavailable + initialize_recovered, rather
    // than the convenience Store::open constructor used by most fixtures.
    let store = Arc::new(Store::unavailable(paths.clone(), StoreState::Maintenance).unwrap());
    store.initialize_recovered().unwrap();
    assert_eq!(store.state(), StoreState::Open);
    assert_eq!(drain(&capture(&store, account)), expected);

    let password = Password::normalize("Z7jv!9qP#3nMx@8Ks^2dH").unwrap();
    store
        .set_store_password(&password, &[], KdfParams::floor())
        .unwrap();
    assert_eq!(store.state(), StoreState::Open);
    assert_eq!(drain(&capture(&store, account)), expected);
    store.close().unwrap();
    drop(store);

    let store = Arc::new(Store::unavailable(paths, StoreState::Maintenance).unwrap());
    store.initialize_recovered().unwrap();
    assert_eq!(store.state(), StoreState::Locked);
    assert!(matches!(
        store.prepare_replay_capture(
            account,
            TASK,
            stamp,
            Arc::new(AtomicBool::new(false)),
            CAPTURE_BUDGET,
        ),
        Err(StoreError::Locked)
    ));
    store.unlock_store(&password, None).unwrap();
    assert_eq!(store.state(), StoreState::Open);
    assert_eq!(drain(&capture(&store, account)), expected);

    store.remove_store_password(&password).unwrap();
    assert_eq!(store.state(), StoreState::Open);
    assert_eq!(drain(&capture(&store, account)), expected);
}
