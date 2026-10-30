use pi_ai::{Content, Message, UserMessage};
use pi_coding_agent::{
    SessionMessage,
    session::{EntryKind, SessionEntry},
};

use super::*;
use crate::store::{Durability, NewTask, StorePaths, TaskKind, codec};

const TASK: &str = "00000000-0000-7000-8000-000000000001";
const OTHER: &str = "00000000-0000-7000-8000-000000000002";

fn fixture() -> (tempfile::TempDir, Arc<Store>, AccountId, AccountId) {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(StorePaths::new(directory.path())).unwrap());
    let alice = store.upsert_account("alice", 1).unwrap();
    let bob = store.upsert_account("bob", 1).unwrap();
    for (account, uuid) in [(alice, TASK), (bob, OTHER)] {
        store
            .create_task(
                account,
                &NewTask {
                    uuid: uuid.into(),
                    project_root: if cfg!(windows) { "C:/test" } else { "/test" }.into(),
                    kind: TaskKind::Desktop,
                    title: "History".into(),
                    provisional: false,
                    model: None,
                    created_ms: 1,
                },
            )
            .unwrap();
    }
    (directory, store, alice, bob)
}

#[test]
fn navigation_wire_sections_are_plain_strings_with_bounded_defaults() {
    let query: NavigationTaskQuery = serde_json::from_value(json!({"section":"active"})).unwrap();
    assert_eq!(
        query.section,
        crate::store::TaskSection::State(crate::store::TaskState::Active)
    );
    assert_eq!(query.pins, crate::store::PinFilter::Exclude);
    assert_eq!(query.limit, 50);
    assert_eq!(serde_json::to_value(&query).unwrap()["section"], "active");
    let query: NavigationSearchQuery = serde_json::from_value(json!({"text":"project"})).unwrap();
    assert_eq!(query.limit, 100);
    assert!(query.project_roots.is_empty());
    assert!(serde_json::from_value::<NavigationTaskQuery>(json!({"section":"unknown"})).is_err());
}

fn users(store: &Store, account: AccountId, task: &str, texts: &[String]) {
    store
        .write(Durability::Durable, |tx| {
            let key = super::super::tasks::task_key_tx(tx, account, task)?;
            for (index, text) in texts.iter().enumerate() {
                let seq = index as i64 + 1;
                let entry = SessionEntry {
                    id: format!("{seq:08x}"),
                    parent_id: (seq > 1).then(|| format!("{:08x}", seq - 1)),
                    timestamp: seq,
                    kind: EntryKind::Message {
                        message: SessionMessage::Llm(Message::User(UserMessage {
                            timestamp: 7,
                            content: vec![Content::text(text)],
                        })),
                    },
                };
                let raw = serde_json::to_vec(&entry).unwrap();
                let (entry_codec, payload) = codec::encode(&raw)?;
                tx.execute(
                    "INSERT INTO entries(task_id,seq,entry_id,kind,created_ms,codec,bytes,payload)
                VALUES (?1,?2,?3,'message',?2,?4,?5,?6)",
                    params![
                        key.0,
                        seq,
                        entry.id,
                        entry_codec,
                        payload.len() as i64,
                        payload
                    ],
                )?;
                projector::append(tx, key, seq, &entry)?;
                tx.execute(
                    "UPDATE tasks SET last_seq=?2,message_count=?2,updated_ms=?2 WHERE id=?1",
                    params![key.0, seq],
                )?;
            }
            Ok(())
        })
        .unwrap();
}

fn open(task: &str, rows: u16, bytes: usize) -> OpenTaskPageRequest {
    OpenTaskPageRequest {
        task: task.into(),
        limit_rows: Some(rows),
        limit_bytes: Some(bytes),
    }
}

fn history(task: &str, cursor: Option<String>, rows: u16, bytes: usize) -> HistoryPageRequest {
    HistoryPageRequest {
        task: task.into(),
        cursor,
        limit_rows: Some(rows),
        limit_bytes: Some(bytes),
    }
}

#[test]
fn repeated_timestamp_keys_remain_distinct_across_backward_and_forward_pages() {
    let (_directory, store, alice, _bob) = fixture();
    users(
        &store,
        alice,
        TASK,
        &(1..=9).map(|i| format!("message {i}")).collect::<Vec<_>>(),
    );
    let tail = store
        .open_task_page(alice, &open(TASK, 3, 64 * 1024), PageTransport::Local)
        .unwrap();
    assert_eq!(
        tail.rows.iter().map(|r| r.row_seq).collect::<Vec<_>>(),
        [7, 8, 9]
    );
    assert!(tail.rows.iter().all(|row| row.item.id == "u7"));
    let mut ids = tail.rows.iter().map(|r| r.row_seq).collect::<Vec<_>>();
    let mut cursor = tail.before;
    while cursor.is_some() {
        let page = store
            .history_before_page(
                alice,
                &history(TASK, cursor, 3, 64 * 1024),
                PageTransport::Local,
            )
            .unwrap();
        ids.extend(page.rows.iter().map(|r| r.row_seq));
        cursor = page.next;
    }
    ids.sort_unstable();
    assert_eq!(ids, (1..=9).collect::<Vec<_>>());
    let mut ids = Vec::new();
    let mut cursor = None;
    loop {
        let page = store
            .history_forward_page(
                alice,
                &history(TASK, cursor, 2, 64 * 1024),
                PageTransport::Local,
            )
            .unwrap();
        ids.extend(page.rows.iter().map(|r| r.row_seq));
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(ids, (1..=9).collect::<Vec<_>>());
}

#[test]
fn cursors_reject_account_task_direction_and_epoch_mismatches() {
    let (_directory, store, alice, bob) = fixture();
    users(&store, alice, TASK, &vec!["message".into(); 3]);
    users(&store, bob, OTHER, &vec!["other".into(); 3]);
    let cursor = store
        .open_task_page(alice, &open(TASK, 1, 64 * 1024), PageTransport::Local)
        .unwrap()
        .before
        .unwrap();
    assert_eq!(
        store.history_before_page(
            bob,
            &history(OTHER, Some(cursor.clone()), 1, 64 * 1024),
            PageTransport::Local
        ),
        Err(StoreError::InvalidInput)
    );
    assert_eq!(
        store.history_forward_page(
            alice,
            &history(TASK, Some(cursor.clone()), 1, 64 * 1024),
            PageTransport::Local
        ),
        Err(StoreError::InvalidInput)
    );
    assert_eq!(
        store.history_before_page(
            alice,
            &history(OTHER, Some(cursor.clone()), 1, 64 * 1024),
            PageTransport::Local
        ),
        Err(StoreError::InvalidInput)
    );
    store
        .write(Durability::Normal, |tx| {
            tx.execute(
                "UPDATE tasks SET rows_epoch=1 WHERE account_id=?1",
                [alice.0],
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        store.history_before_page(
            alice,
            &history(TASK, Some(cursor), 1, 64 * 1024),
            PageTransport::Local
        ),
        Err(StoreError::EpochChanged)
    );
    for bad in ["!", "e30", &"A".repeat(MAX_CURSOR_BYTES * 2)] {
        assert_eq!(
            store.history_before_page(
                alice,
                &history(TASK, Some(bad.into()), 1, 64 * 1024),
                PageTransport::Local
            ),
            Err(StoreError::InvalidInput)
        );
    }
}

#[test]
fn oversized_rows_are_inline_bounded_and_range_reconstruction_is_exact_and_versioned() {
    let (_directory, store, alice, bob) = fixture();
    let text = "quoted \" 🦊\n".repeat(180_000);
    users(&store, alice, TASK, std::slice::from_ref(&text));
    let page = store
        .open_task_page(alice, &open(TASK, 20, MIB / 2), PageTransport::Remote)
        .unwrap();
    assert_eq!(page.rows.len(), 1);
    assert!(serialized_len(&page).unwrap() <= MIB / 2);
    let row = &page.rows[0];
    assert!(row.item.truncated.is_some());
    assert!(serialized_len(&row.item).unwrap() <= MIB / 4);
    let reader = RowReader::new(store.clone());
    let mut output = Vec::new();
    loop {
        let range = reader
            .read_row(
                alice,
                &RowRangeRequest {
                    task: TASK.into(),
                    rows_epoch: row.rows_epoch,
                    row_seq: row.row_seq,
                    update_seq: Some(row.update_seq),
                    offset: output.len() as u64,
                    len: 128 * 1024,
                },
                PageTransport::Remote,
            )
            .unwrap();
        assert_eq!(range.update_seq, row.update_seq);
        assert!(range.data.len() <= (128 * 1024usize).div_ceil(3) * 4);
        output.extend(STANDARD.decode(range.data).unwrap());
        if output.len() as u64 == range.total {
            break;
        }
    }
    let full: AgentTimelineItem = serde_json::from_slice(&output).unwrap();
    assert_eq!(full.text.as_deref(), Some(text.as_str()));
    assert_eq!(full.truncated, None);
    assert_eq!(row.item.truncated, Some(output.len()));
    assert_eq!(reader.cached_bytes().unwrap(), output.len());
    assert_eq!(
        reader.read_row(
            bob,
            &RowRangeRequest {
                task: TASK.into(),
                rows_epoch: 0,
                row_seq: 1,
                update_seq: Some(1),
                offset: 0,
                len: 1
            },
            PageTransport::Local
        ),
        Err(StoreError::NotFound)
    );
    store
        .write(Durability::Normal, |tx| {
            tx.execute(
                "UPDATE timeline_rows SET update_seq=2 WHERE task_id=?1",
                [store_task(tx, alice, TASK)?.0],
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        reader.read_row(
            alice,
            &RowRangeRequest {
                task: TASK.into(),
                rows_epoch: 0,
                row_seq: 1,
                update_seq: Some(1),
                offset: 1,
                len: 1
            },
            PageTransport::Local
        ),
        Err(StoreError::Conflict)
    );
    reader.clear().unwrap();
    assert_eq!(reader.cached_bytes().unwrap(), 0);
}

fn store_task(tx: &Transaction<'_>, account: AccountId, uuid: &str) -> StoreResult<TaskKey> {
    super::super::tasks::task_key_tx(tx, account, uuid)
}

#[test]
fn hidden_rows_do_not_reappear_and_summaries_follow_creation_origin() {
    let (_directory, store, alice, _bob) = fixture();
    users(
        &store,
        alice,
        TASK,
        &["first".into(), "second".into(), "third".into()],
    );
    store.write(Durability::Normal,|tx| {
        let task=store_task(tx,alice,TASK)?;
        tx.execute("UPDATE timeline_rows SET hidden=1 WHERE task_id=?1 AND row_seq=2",[task.0])?;
        tx.execute("INSERT INTO tool_summaries(task_id,entry_seq,item_index,bytes,summary) VALUES (?1,1,0,13,'first summary')",[task.0])?;
        Ok(())
    }).unwrap();
    let page = store
        .history_forward_page(
            alice,
            &history(TASK, None, 10, 64 * 1024),
            PageTransport::Local,
        )
        .unwrap();
    assert_eq!(
        page.rows.iter().map(|r| r.row_seq).collect::<Vec<_>>(),
        [1, 3]
    );
    assert_eq!(page.rows[0].summary.as_deref(), Some("first summary"));
    assert_eq!(page.rows[1].summary, None);
    assert_eq!(page.rows[0].item.id, page.rows[1].item.id);
    store
        .write(Durability::Normal, |tx| {
            tx.execute(
                "UPDATE tool_summaries SET summary=?1,bytes=?2 WHERE task_id=?3",
                params![
                    "\u{1}".repeat(100_000),
                    100_000,
                    store_task(tx, alice, TASK)?.0
                ],
            )?;
            Ok(())
        })
        .unwrap();
    let page = store
        .history_forward_page(
            alice,
            &history(TASK, None, 10, 64 * 1024),
            PageTransport::Remote,
        )
        .unwrap();
    assert_eq!(page.rows[0].summary_truncated, Some(100_000));
    assert!(serialized_len(&page).unwrap() <= 64 * 1024);
}

#[test]
fn hard_serialized_caps_apply_to_multiple_large_rows_and_structural_json() {
    let (_directory, store, alice, _bob) = fixture();
    users(&store, alice, TASK, &vec!["x".repeat(100_000); 20]);
    let page = store
        .history_forward_page(
            alice,
            &history(TASK, None, 500, 64 * 1024),
            PageTransport::Remote,
        )
        .unwrap();
    assert!(!page.rows.is_empty());
    assert!(page.next.is_some());
    assert!(serialized_len(&page).unwrap() <= 64 * 1024);
    let mut item:AgentTimelineItem=serde_json::from_value(json!({"revision":0,"id":"structural","itemType":"tool",
        "input":(0..100_000).map(|_|json!({})).collect::<Vec<_>>(),"createdMs":1,"merge":"replace"})).unwrap();
    let original = serialized_len(&item).unwrap();
    let fitted_bytes = fit_item(&mut item, original, 8192, original).unwrap();
    assert!(serialized_len(&item).unwrap() <= 8192);
    assert_eq!(fitted_bytes, serde_json::to_vec(&item).unwrap().len());
    assert_eq!(item.truncated, Some(original));
    let mut item: AgentTimelineItem =
        serde_json::from_value(json!({"revision":0,"id":"custom".repeat(100_000),
        "itemType":"notice","text":"a custom notice","createdMs":1,"merge":"replace"}))
        .unwrap();
    let original = serialized_len(&item).unwrap();
    let fitted_bytes = fit_item(&mut item, original, 2048, original).unwrap();
    assert!(serialized_len(&item).unwrap() <= 2048);
    assert_eq!(fitted_bytes, serde_json::to_vec(&item).unwrap().len());
    assert_eq!(item.truncated, Some(original));
}

#[test]
fn fitted_row_sizes_match_exact_wire_bytes_after_fast_and_truncated_paths() {
    for (payload, summary, cap) in [
        (json!({"text":"quoted \" 🦊\n".repeat(20)}), None, 64 * 1024),
        (
            json!({"text":"quoted \" 🦊\n".repeat(20_000)}),
            Some("\u{1}".repeat(4096)),
            64 * 1024,
        ),
        (
            json!({"input":(0..1000).map(|_|json!({})).collect::<Vec<_>>()}),
            None,
            2048,
        ),
    ] {
        let mut value = json!({"revision":0,"id":"occurrence","itemType":"message",
            "createdMs":7,"merge":"replace","persisted":{
                "taskId":TASK,"rowsEpoch":3,"rowSeq":17,"updateSeq":19}});
        for (key, field) in payload.as_object().unwrap() {
            value[key.as_str()] = field.clone();
        }
        let item: AgentTimelineItem = serde_json::from_value(value).unwrap();
        let original = serde_json::to_vec(&item).unwrap().len();
        let identity = item.persisted.clone();
        let mut row = TimelineRow {
            rows_epoch: 3,
            row_seq: 17,
            update_seq: 19,
            item,
            summary,
            summary_truncated: None,
        };
        let size = fit_row(&mut row, cap, original).unwrap();
        assert_eq!(
            size.item_bytes,
            serde_json::to_vec(&row.item).unwrap().len()
        );
        assert_eq!(size.row_bytes, serde_json::to_vec(&row).unwrap().len());
        assert!(size.row_bytes <= cap);
        assert_eq!(row.item.persisted, identity);
        assert_eq!((row.rows_epoch, row.row_seq, row.update_seq), (3, 17, 19));
        if original > cap {
            assert_eq!(row.item.truncated, Some(original));
        } else {
            assert_eq!(row.item.truncated, None);
        }
    }
}

#[test]
fn reused_wire_sizes_keep_escaped_rows_bounded_and_reachable_in_both_directions() {
    let (_directory, store, alice, _bob) = fixture();
    let texts = (0..8)
        .map(|index| format!("{index}{}", "\u{1}".repeat(2000)))
        .collect::<Vec<_>>();
    users(&store, alice, TASK, &texts);
    let bytes = 16 * 1024;
    let tail = store
        .open_task_page(alice, &open(TASK, 20, bytes), PageTransport::Remote)
        .unwrap();
    assert!(serde_json::to_vec(&tail).unwrap().len() <= bytes);
    let mut backward = tail.rows.iter().map(|row| row.row_seq).collect::<Vec<_>>();
    let mut before = tail.before;
    for _ in 0..texts.len() {
        let Some(cursor) = before.take() else { break };
        let page = store
            .history_before_page(
                alice,
                &history(TASK, Some(cursor), 20, bytes),
                PageTransport::Remote,
            )
            .unwrap();
        assert!(serde_json::to_vec(&page).unwrap().len() <= bytes);
        assert!(!page.rows.is_empty());
        backward.extend(page.rows.iter().map(|row| row.row_seq));
        before = page.next;
    }
    assert!(before.is_none());
    backward.sort_unstable();
    assert_eq!(backward, (1..=8).collect::<Vec<_>>());

    let mut forward = Vec::new();
    let mut after = None;
    for _ in 0..texts.len() {
        let page = store
            .history_forward_page(
                alice,
                &history(TASK, after, 20, bytes),
                PageTransport::Remote,
            )
            .unwrap();
        assert!(serde_json::to_vec(&page).unwrap().len() <= bytes);
        assert!(!page.rows.is_empty());
        forward.extend(page.rows.iter().map(|row| row.row_seq));
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    assert!(after.is_none());
    assert_eq!(forward, (1..=8).collect::<Vec<_>>());
}

#[test]
fn row_ranges_enforce_bounds_and_reauthorize_even_when_cached() {
    let (_directory, store, alice, _bob) = fixture();
    users(&store, alice, TASK, &["cached".into()]);
    let reader = RowReader::new(store.clone());
    let request = RowRangeRequest {
        task: TASK.into(),
        rows_epoch: 0,
        row_seq: 1,
        update_seq: Some(1),
        offset: 0,
        len: 32,
    };
    reader
        .read_row(alice, &request, PageTransport::Local)
        .unwrap();
    let cached_bytes = reader.cached_bytes().unwrap();
    assert!(cached_bytes > 0);
    let mut bad = request.clone();
    bad.offset = cached_bytes as u64 + 1;
    assert_eq!(
        reader.read_row(alice, &bad, PageTransport::Local),
        Err(StoreError::InvalidInput)
    );
    assert_eq!(reader.cached_bytes().unwrap(), cached_bytes);
    reader
        .read_row(alice, &request, PageTransport::Local)
        .unwrap();
    assert_eq!(reader.cached_bytes().unwrap(), cached_bytes);
    bad = request.clone();
    bad.len = MIB + 1;
    assert_eq!(
        reader.read_row(alice, &bad, PageTransport::Local),
        Err(StoreError::InvalidInput)
    );
    bad = request.clone();
    bad.offset = 1;
    bad.update_seq = None;
    assert_eq!(
        reader.read_row(alice, &bad, PageTransport::Local),
        Err(StoreError::InvalidInput)
    );
    store.mark_task_deleted(alice, TASK, 2).unwrap();
    assert_eq!(
        reader.read_row(alice, &request, PageTransport::Local),
        Err(StoreError::NotFound)
    );
}

#[test]
fn cache_generation_changes_cannot_reuse_an_old_store_row() {
    let (_directory, store, alice, _bob) = fixture();
    users(&store, alice, TASK, &["old store".into()]);
    let reader = RowReader::new(store.clone());
    let request = RowRangeRequest {
        task: TASK.into(),
        rows_epoch: 0,
        row_seq: 1,
        update_seq: Some(1),
        offset: 0,
        len: MIB,
    };
    let old = reader
        .read_row(alice, &request, PageTransport::Local)
        .unwrap();
    let (_new_directory, new_store, new_account, _new_bob) = fixture();
    users(&new_store, new_account, TASK, &["new store".into()]);
    let (item_codec, payload): (i64, Vec<u8>) = new_store
        .read(|tx| {
            let task = store_task(tx, new_account, TASK)?;
            Ok(tx.query_row(
                "SELECT codec,item FROM timeline_rows WHERE task_id=?1 AND row_seq=1",
                [task.0],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?)
        })
        .unwrap();
    // Model an erase/new-generation boundary with deliberately reused test ids.
    store
        .write(Durability::Durable, |tx| {
            let task = store_task(tx, alice, TASK)?;
            tx.execute(
                "UPDATE timeline_rows SET codec=?2,bytes=?3,item=?4 WHERE task_id=?1",
                params![task.0, item_codec, payload.len() as i64, payload],
            )?;
            tx.execute(
                "UPDATE store_meta SET value=zeroblob(16) WHERE key='store_id'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    let new = reader
        .read_row(alice, &request, PageTransport::Local)
        .unwrap();
    assert_ne!(old.data, new.data);
    let item: AgentTimelineItem =
        serde_json::from_slice(&STANDARD.decode(new.data).unwrap()).unwrap();
    assert_eq!(item.text.as_deref(), Some("new store"));
}

#[test]
fn open_task_allocates_actual_canonical_root_bytes_inside_the_reply_cap() {
    let (_directory, store, alice, _bob) = fixture();
    users(&store, alice, TASK, &["message".into()]);
    let root = format!(
        "{}{}",
        if cfg!(windows) { "C:/" } else { "/" },
        "\u{1}".repeat(15_000)
    );
    store
        .update_task(
            alice,
            TASK,
            &crate::store::TaskPatch {
                project_root: Some(root.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    let page = store
        .open_task_page(alice, &open(TASK, 10, MIB / 2), PageTransport::Remote)
        .unwrap();
    assert_eq!(page.summary.project_root, root);
    assert_eq!(page.rows.len(), 1);
    assert!(serialized_len(&page).unwrap() <= MIB / 2);
}

#[test]
fn latest_plan_is_returned_even_when_it_precedes_the_tail_page() {
    let (_directory, store, alice, _bob) = fixture();
    let plan = EntryKind::Message {
        message: SessionMessage::Llm(Message::Assistant(pi_ai::AssistantMessage {
            content: vec![pi_ai::AssistantContent::ToolCall(pi_ai::ToolCall {
                id: "plan".into(),
                name: "todo_write".into(),
                arguments: json!({"todos":[{"id":"one","content":"persist","status":"pending"}]})
                    .as_object()
                    .unwrap()
                    .clone(),
                arguments_error: None,
                partial_json: None,
            })],
            api: "test".into(),
            provider: "test".into(),
            model: "test".into(),
            response_id: None,
            thinking_level: None,
            usage: Default::default(),
            stop_reason: pi_ai::StopReason::Stop,
            error_message: None,
            timestamp: 7,
        })),
    };
    let mut kinds = vec![plan];
    kinds.extend((0..6).map(|_| EntryKind::Message {
        message: SessionMessage::Llm(Message::User(UserMessage {
            timestamp: 7,
            content: vec![Content::text("later")],
        })),
    }));
    store
        .write(Durability::Durable, |tx| {
            let task = store_task(tx, alice, TASK)?;
            for (index, kind) in kinds.into_iter().enumerate() {
                let seq = index as i64 + 1;
                let entry = SessionEntry {
                    id: format!("{seq:08x}"),
                    parent_id: (seq > 1).then(|| format!("{:08x}", seq - 1)),
                    timestamp: seq,
                    kind,
                };
                let (entry_codec, payload) = codec::encode(&serde_json::to_vec(&entry).unwrap())?;
                tx.execute(
                    "INSERT INTO entries(task_id,seq,entry_id,kind,created_ms,codec,bytes,payload)
                VALUES (?1,?2,?3,'message',?2,?4,?5,?6)",
                    params![
                        task.0,
                        seq,
                        entry.id,
                        entry_codec,
                        payload.len() as i64,
                        payload
                    ],
                )?;
                projector::append(tx, task, seq, &entry)?;
                tx.execute(
                    "UPDATE tasks SET last_seq=?2,message_count=?2,updated_ms=?2 WHERE id=?1",
                    params![task.0, seq],
                )?;
            }
            Ok(())
        })
        .unwrap();
    let page = store
        .open_task_page(alice, &open(TASK, 2, 64 * 1024), PageTransport::Local)
        .unwrap();
    assert_eq!(
        page.rows.iter().map(|row| row.row_seq).collect::<Vec<_>>(),
        [6, 7]
    );
    let plan = page.plan.unwrap();
    assert_eq!(plan.row_seq, 1);
    assert!(plan.item.input.unwrap().get("todos").unwrap().is_array());
}

#[test]
fn history_and_project_navigation_queries_have_no_depth_dependent_sort() {
    let (_directory, store, alice, _bob) = fixture();
    users(&store, alice, TASK, &vec!["deep".into(); 20]);
    store.read(|tx| {
        let queries=[
            "EXPLAIN QUERY PLAN SELECT r.row_seq,r.codec,r.item FROM timeline_rows r LEFT JOIN tool_summaries s ON s.task_id=r.task_id AND s.entry_seq=r.entry_seq AND s.item_index=r.item_index WHERE r.task_id=1 AND r.epoch=0 AND r.hidden=0 AND r.row_seq<10 ORDER BY r.row_seq DESC LIMIT 10",
            "EXPLAIN QUERY PLAN SELECT id,root FROM projects WHERE account_id=1 AND removed_ms IS NULL ORDER BY position IS NULL,coalesce(position,0),root,id LIMIT 10",
        ];
        for sql in queries {
            let mut statement=tx.prepare(sql)?;
            let plans=statement.query_map([],|row|row.get::<_,String>(3))?.collect::<rusqlite::Result<Vec<_>>>()?;
            assert!(!plans.iter().any(|plan|plan.contains("TEMP B-TREE")),"{plans:?}");
            assert!(!plans.iter().any(|plan|plan.starts_with("SCAN timeline_rows")),"{plans:?}");
        }
        Ok(())
    }).unwrap();
}
