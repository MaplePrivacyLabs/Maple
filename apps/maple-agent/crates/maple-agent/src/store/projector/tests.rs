use super::*;
use pi_ai::{
    AssistantContent, AssistantMessage, Content, StopReason, ToolCall, ToolResultMessage, Usage,
    UserMessage,
};
use pi_coding_agent::{
    session::{SESSION_VERSION, SessionHeader, SessionManager},
    store::MemoryStore,
};
use rusqlite::Connection;
use serde_json::json;

fn database() -> Connection {
    let mut connection = Connection::open_in_memory().unwrap();
    connection
        .pragma_update(None, "application_id", super::super::schema::APPLICATION_ID)
        .unwrap();
    connection
        .pragma_update(None, "foreign_keys", true)
        .unwrap();
    super::super::schema::migrate(&mut connection, 1).unwrap();
    connection.execute("INSERT INTO accounts(id,user_key,user_id,created_ms,last_seen_ms) VALUES(1,'a','a',1,1)",[]).unwrap();
    connection
        .execute(
            "INSERT INTO projects(id,account_id,root,last_used_ms) VALUES(1,1,'/project',1)",
            [],
        )
        .unwrap();
    connection.execute("INSERT INTO tasks(id,account_id,project_id,uuid,kind,state,title,title_fold,created_ms,updated_ms) VALUES(1,1,1,'task','desktop','active','Test','test',1,1)",[]).unwrap();
    connection
}

fn user(timestamp: i64, text: &str) -> EntryKind {
    EntryKind::Message {
        message: SessionMessage::Llm(Message::User(UserMessage {
            timestamp,
            content: vec![Content::text(text)],
        })),
    }
}

fn assistant(timestamp: i64, call: Option<&str>) -> EntryKind {
    let mut content = vec![
        AssistantContent::thinking("think"),
        AssistantContent::text("reply"),
    ];
    if let Some(id) = call {
        content.push(AssistantContent::ToolCall(ToolCall {
            id: id.into(),
            name: "read".into(),
            arguments: json!({"path":"README.md"}).as_object().unwrap().clone(),
            arguments_error: None,
            partial_json: None,
        }));
    }
    EntryKind::Message {
        message: SessionMessage::Llm(Message::Assistant(AssistantMessage {
            content,
            api: "test".into(),
            provider: "test".into(),
            model: "test".into(),
            response_id: None,
            thinking_level: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp,
        })),
    }
}

fn result(timestamp: i64, call: &str, text: &str) -> EntryKind {
    EntryKind::Message {
        message: SessionMessage::Llm(Message::ToolResult(ToolResultMessage {
            tool_call_id: call.into(),
            tool_name: "read".into(),
            content: vec![Content::text(text)],
            details: None,
            usage: None,
            is_error: false,
            timestamp,
        })),
    }
}

fn add(
    connection: &mut Connection,
    entries: &mut Vec<SessionEntry>,
    kind: EntryKind,
) -> ProjectionChange {
    let seq = entries.len() as i64 + 1;
    let entry = SessionEntry {
        id: format!("{seq:08x}"),
        parent_id: entries.last().map(|entry| entry.id.clone()),
        timestamp: seq,
        kind,
    };
    let change = append_entry(connection, seq, &entry).unwrap();
    entries.push(entry);
    let tx = connection.transaction().unwrap();
    let epoch: i64 = tx
        .query_row("SELECT rows_epoch FROM tasks WHERE id=1", [], |r| r.get(0))
        .unwrap();
    let rows = read_all(&tx, epoch);
    let oracle = oracle(entries);
    assert_eq!(rows, oracle, "after entry {seq}");
    // The change returned above can be reconstructed independently; its caller
    // publishes only after this transaction's successful COMMIT.
    change
}

fn append_entry(
    connection: &mut Connection,
    seq: i64,
    entry: &SessionEntry,
) -> StoreResult<ProjectionChange> {
    let projection = prepare_entry(entry, seq)?;
    let raw = serde_json::to_vec(entry).unwrap();
    let (entry_codec, payload) = codec::encode(&raw)?;
    let tx = connection.transaction()?;
    tx.execute("INSERT INTO entries(task_id,seq,entry_id,kind,created_ms,codec,bytes,payload) VALUES(1,?1,?2,'message',?3,?4,?5,?6)",params![seq,entry.id,entry.timestamp,entry_codec,payload.len() as i64,payload])?;
    let changed = append_prepared(&tx, TaskKey(1), seq, entry, &projection)?;
    tx.execute("UPDATE tasks SET last_seq=?1 WHERE id=1", [seq])?;
    tx.commit()?;
    Ok(changed)
}

fn oracle(entries: &[SessionEntry]) -> Vec<AgentTimelineItem> {
    let header = SessionHeader {
        version: SESSION_VERSION,
        id: "test".into(),
        timestamp: 0,
        cwd: "/project".into(),
        parent_session: None,
    };
    let session = SessionManager::open(header, entries.to_vec(), Box::new(MemoryStore));
    timeline::session_timeline(&session)
}

fn read_all(tx: &Transaction<'_>, epoch: i64) -> Vec<AgentTimelineItem> {
    let mut statement=tx.prepare("SELECT codec,item FROM timeline_rows WHERE task_id=1 AND epoch=?1 AND hidden=0 ORDER BY row_seq").unwrap();
    let raw = statement
        .query_map([epoch], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    raw.into_iter()
        .map(|(item_codec, payload)| {
            decode_row(tx, TaskKey(1), item_codec, &payload, true).unwrap()
        })
        .collect()
}

#[test]
fn repeated_keys_have_distinct_occurrences_and_merge_into_first_matching_key() {
    let mut connection = database();
    let mut entries = Vec::new();
    add(&mut connection, &mut entries, user(7, "first"));
    add(&mut connection, &mut entries, user(7, "second"));
    add(&mut connection, &mut entries, assistant(8, Some("same")));
    add(&mut connection, &mut entries, assistant(8, Some("same")));
    add(&mut connection, &mut entries, result(9, "same", "done"));
    let tx = connection.transaction().unwrap();
    let rows = read_all(&tx, 0);
    assert_eq!(rows.iter().filter(|row| row.id == "u7").count(), 2);
    let tools: Vec<_> = rows.iter().filter(|row| row.id == "a8-same").collect();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].status.as_deref(), Some("completed"));
    assert_eq!(tools[1].status.as_deref(), Some("running"));
    let identities:Vec<(i64,i64,i64)>=tx.prepare("SELECT row_seq,entry_seq,item_index FROM timeline_rows WHERE task_id=1 AND epoch=0 AND row_key='a8-same' ORDER BY row_seq").unwrap().query_map([],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap().collect::<rusqlite::Result<_>>().unwrap();
    assert_ne!(identities[0], identities[1]);
}

#[test]
fn retries_simple_hides_and_merged_tool_omissions_equal_full_projection() {
    let mut connection = database();
    let mut entries = Vec::new();
    add(&mut connection, &mut entries, user(7, "prompt"));
    let failed = assistant(8, None);
    add(&mut connection, &mut entries, failed);
    let omitted = entries[1].id.clone();
    add(
        &mut connection,
        &mut entries,
        EntryKind::ContextEdit {
            target_id: omitted,
            replacement: None,
        },
    );
    add(&mut connection, &mut entries, assistant(9, Some("c")));
    add(&mut connection, &mut entries, result(10, "c", "output"));
    let target_id = entries[3].id.clone();
    add(
        &mut connection,
        &mut entries,
        EntryKind::ContextEdit {
            target_id,
            replacement: None,
        },
    );
    let epoch: i64 = connection
        .query_row("SELECT rows_epoch FROM tasks WHERE id=1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(epoch, 1, "an omission after a merge flips a fresh epoch");
    // The orphan result survives exactly as in the canonical projection.
    let tx = connection.transaction().unwrap();
    assert_eq!(read_all(&tx, epoch).last().unwrap().id, "tool-c");
}

#[test]
fn generated_append_sequences_match_full_projection_after_every_write() {
    for seed in 0..12u64 {
        let mut connection = database();
        let mut entries: Vec<SessionEntry> = Vec::new();
        let mut random = seed + 1;
        for index in 0..70 {
            random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
            let timestamp = (random % 9) as i64;
            let kind = match random % 8 {
                0 => assistant(timestamp, Some("repeat")),
                1 => result(timestamp, "repeat", "output"),
                2 if !entries.is_empty() => EntryKind::ContextEdit {
                    target_id: entries[(random as usize) % entries.len()].id.clone(),
                    replacement: None,
                },
                3 => EntryKind::Custom {
                    custom_type: timeline::MAPLE_NOTICE_ENTRY.into(),
                    data: Some(timeline::notice_entry_data(
                        &format!("notice-{index}"),
                        "Stopped by user",
                    )),
                },
                4 => EntryKind::BranchSummary {
                    from_id: "before".into(),
                    summary: "branch summary".into(),
                    details: None,
                    usage: None,
                    from_extension: false,
                },
                5 => EntryKind::Compaction {
                    summary: "summary".into(),
                    first_kept_entry_id: entries
                        .first()
                        .map(|entry| entry.id.clone())
                        .unwrap_or_default(),
                    tokens_before: 100,
                    details: None,
                    usage: None,
                    from_extension: false,
                    system_message: None,
                },
                _ => user(timestamp, "message"),
            };
            add(&mut connection, &mut entries, kind);
        }
    }
}

#[test]
fn branch_move_rebuilds_without_rewriting_source_entries() {
    let mut connection = database();
    let mut entries = Vec::new();
    add(&mut connection, &mut entries, user(1, "root"));
    add(&mut connection, &mut entries, user(2, "old branch"));
    let entry = SessionEntry {
        id: "00000003".into(),
        parent_id: Some(entries[0].id.clone()),
        timestamp: 3,
        kind: user(3, "new branch"),
    };
    let change = append_entry(&mut connection, 3, &entry).unwrap();
    entries.push(entry);
    assert_eq!(change.history_replaced, Some(1));
    let tx = connection.transaction().unwrap();
    assert_eq!(read_all(&tx, 1), oracle(&entries));
    assert_eq!(
        tx.query_row::<i64, _, _>("SELECT count(*) FROM entries WHERE task_id=1", [], |row| {
            row.get(0)
        })
        .unwrap(),
        3
    );
}

#[test]
fn large_field_reconstruction_retains_call_input_after_result_merge() {
    let mut connection = database();
    let mut entries = Vec::new();
    let mut call = assistant(1, Some("big"));
    if let EntryKind::Message {
        message: SessionMessage::Llm(Message::Assistant(reply)),
    } = &mut call
        && let AssistantContent::ToolCall(call) = reply.content.last_mut().unwrap()
    {
        call.arguments
            .insert("content".into(), json!("large".repeat(400_000)));
    }
    add(&mut connection, &mut entries, call);
    add(&mut connection, &mut entries, result(2, "big", "result"));
    let tx = connection.transaction().unwrap();
    let (item_codec, payload): (i64, Vec<u8>) = tx
        .query_row(
            "SELECT codec,item FROM timeline_rows WHERE task_id=1 AND call_id='big'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let bounded = decode_row(&tx, TaskKey(1), item_codec, &payload, false).unwrap();
    assert_eq!(
        bounded.input.as_ref().unwrap()["content"]
            .as_str()
            .unwrap()
            .len(),
        CHUNK_BYTES
    );
    let full = decode_row(&tx, TaskKey(1), item_codec, &payload, true).unwrap();
    assert_eq!(
        full.input.as_ref().unwrap()["content"]
            .as_str()
            .unwrap()
            .len(),
        2_000_000
    );
    assert_eq!(
        full.output.as_ref().unwrap()["content"][0]["text"],
        "result"
    );
}

#[test]
fn plan_predicate_matches_todo_tool_shape() {
    let EntryKind::Message {
        message: SessionMessage::Llm(Message::Assistant(reply)),
    } = assistant(1, Some("c"))
    else {
        unreachable!()
    };
    let mut item = timeline::reply_items(&reply).pop().unwrap();
    assert!(!is_plan_row(&item));
    item.input = Some(json!({"todos":[]}));
    assert!(is_plan_row(&item));
    item.item_type = "message".into();
    assert!(!is_plan_row(&item));
}

#[test]
fn external_turn_end_merges_and_keeps_creation_origin() {
    let mut connection = database();
    let mut entries = Vec::new();
    add(
        &mut connection,
        &mut entries,
        assistant(8, Some("external")),
    );
    let change = add(
        &mut connection,
        &mut entries,
        EntryKind::Custom {
            custom_type: "maple.external-agent".into(),
            data: Some(json!({
                "rowId":"a8-external","resultText":"Status: completed",
                "mapleExternalAgent":{"provider":"codex","agentId":"thread","status":"completed","text":"finished"}
            })),
        },
    );
    assert_eq!(change.rows.len(), 1);
    assert_eq!(
        change.rows[0].origin,
        RowOrigin {
            entry_seq: 1,
            item_index: 2
        }
    );
    assert_eq!(change.rows[0].update_seq, 2);
    assert_eq!(
        change.rows[0].item.output.as_ref().unwrap()["text"],
        "Status: completed"
    );
    let target_id = entries[1].id.clone();
    add(
        &mut connection,
        &mut entries,
        EntryKind::ContextEdit {
            target_id,
            replacement: None,
        },
    );
    let epoch: i64 = connection
        .query_row("SELECT rows_epoch FROM tasks WHERE id=1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(epoch, 1);
    let origin:(i64,i64)=connection.query_row("SELECT entry_seq,item_index FROM timeline_rows WHERE task_id=1 AND epoch=1 AND call_id='external'",[],|row|Ok((row.get(0)?,row.get(1)?))).unwrap();
    assert_eq!(origin, (1, 2));
}

#[test]
fn timeline_format_pins_the_canonical_fixture() {
    use sha2::{Digest, Sha256};
    let entries = vec![SessionEntry {
        id: "00000001".into(),
        parent_id: None,
        timestamp: 1,
        kind: user(7, "hello"),
    }];
    let serialized = serde_json::to_vec(&oracle(&entries)).unwrap();
    let digest = format!("{:x}", Sha256::digest(serialized));
    // Golden output is intentionally paired with its derivation format. A
    // builder change requires reviewing/replacing the golden and format bump.
    assert_eq!(
        (TIMELINE_FORMAT, digest.as_str()),
        (
            2,
            "bd6b8487d68c8376bcda6a6d7323a632c5bd942368dbe742a9de26440af852c2"
        )
    );
}

fn prepared(connection: &mut Connection, entries: &[SessionEntry]) -> PreparedRebuild {
    let tx = connection.transaction().unwrap();
    let epoch = tx
        .query_row::<Option<i64>, _, _>("SELECT rebuild_epoch FROM tasks WHERE id=1", [], |row| {
            row.get(0)
        })
        .unwrap()
        .unwrap();
    let snapshot = PreparedRebuild {
        ticket: RebuildTicket {
            task: TaskKey(1),
            account: AccountId(1),
            uuid: "task".into(),
            epoch,
        },
        last_seq: entries.len() as i64,
        outline: Outline::Compact(
            resolve_outline(
                entries
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| CompactMeta::from_entry(index as i64 + 1, entry).unwrap())
                    .collect(),
            )
            .unwrap(),
        ),
    };
    tx.commit().unwrap();
    snapshot
}

fn rebuild_chunk(
    tx: &Transaction<'_>,
    prepared: &PreparedRebuild,
) -> StoreResult<(RebuildProgress, Option<super::super::StoreChange>)> {
    let cursor = rebuild_cursor(tx, &prepared.ticket)?;
    let Outline::Compact(outline) = &prepared.outline else {
        return Err(StoreError::InvalidData);
    };
    let start = outline.partition_point(|entry| entry.seq <= cursor.0);
    let end = (start + CHUNK_ENTRIES).min(outline.len());
    let chunk = prepare_chunk(
        tx,
        prepared,
        cursor,
        &outline[start..end],
        end == outline.len(),
    )?;
    commit_chunk(tx, &chunk)
}

#[test]
fn chunked_rebuild_resumes_inside_a_large_entry_and_publishes_once() {
    let mut connection = database();
    let mut entries = Vec::new();
    let mut reply = assistant(8, None);
    if let EntryKind::Message {
        message: SessionMessage::Llm(Message::Assistant(reply)),
    } = &mut reply
    {
        for index in 0..2500 {
            reply.content.push(AssistantContent::ToolCall(ToolCall {
                id: format!("call-{index}"),
                name: "read".into(),
                arguments: json!({"path":"file"}).as_object().unwrap().clone(),
                arguments_error: None,
                partial_json: None,
            }));
        }
    }
    add(&mut connection, &mut entries, reply);
    // Omitting a non-row source cannot take the simple-hide path, and this
    // task exceeds an inline rebuild's 1000-row limit.
    let target_id = entries[0].id.clone();
    add(
        &mut connection,
        &mut entries,
        EntryKind::Custom {
            custom_type: "meta".into(),
            data: None,
        },
    );
    let edit = SessionEntry {
        id: "00000003".into(),
        parent_id: Some(entries[1].id.clone()),
        timestamp: 3,
        kind: EntryKind::ContextEdit {
            target_id: entries[1].id.clone(),
            replacement: None,
        },
    };
    let change = append_entry(&mut connection, 3, &edit).unwrap();
    assert!(change.rebuild_pending);
    entries.push(edit);
    let snapshot = prepared(&mut connection, &entries);
    let mut flips = 0;
    let mut chunks = 0;
    loop {
        let tx = connection.transaction().unwrap();
        let before: i64 = tx
            .query_row(
                "SELECT count(*) FROM timeline_rows WHERE task_id=1 AND epoch=?1",
                [snapshot.ticket.epoch],
                |row| row.get(0),
            )
            .unwrap();
        let (progress, event) = rebuild_chunk(&tx, &snapshot).unwrap();
        let after: i64 = tx
            .query_row(
                "SELECT count(*) FROM timeline_rows WHERE task_id=1 AND epoch=?1",
                [snapshot.ticket.epoch],
                |row| row.get(0),
            )
            .unwrap();
        assert!(after - before <= CHUNK_ROWS as i64);
        if event.is_some() {
            flips += 1;
        }
        tx.commit().unwrap();
        chunks += 1;
        match progress {
            RebuildProgress::Replaced { .. } => break,
            RebuildProgress::Pending { bytes, .. } => assert!(bytes <= CHUNK_BYTES),
            RebuildProgress::NeedsSnapshot => panic!("unchanged snapshot"),
        }
    }
    assert!(chunks >= 3);
    assert_eq!(flips, 1);
    let tx = connection.transaction().unwrap();
    assert_eq!(read_all(&tx, snapshot.ticket.epoch), oracle(&entries));
    assert!(entries.iter().any(|entry| entry.id == target_id));
}

#[test]
fn shadow_rebuild_with_interleaved_appends_equals_full_projection() {
    let mut connection = database();
    let mut entries = Vec::new();
    for index in 0..1005 {
        // Direct append avoids quadratic oracle testing during fixture setup.
        let entry = SessionEntry {
            id: format!("{:08x}", index + 1),
            parent_id: entries.last().map(|entry: &SessionEntry| entry.id.clone()),
            timestamp: index + 1,
            kind: user(index + 1, "message"),
        };
        append_entry(&mut connection, index + 1, &entry).unwrap();
        entries.push(entry);
    }
    let tx = connection.transaction().unwrap();
    schedule_rebuild(&tx, TaskKey(1), 0).unwrap();
    tx.commit().unwrap();
    let snapshot = prepared(&mut connection, &entries);
    let tx = connection.transaction().unwrap();
    assert!(matches!(
        rebuild_chunk(&tx, &snapshot).unwrap().0,
        RebuildProgress::Pending { .. }
    ));
    tx.commit().unwrap();
    let extra = SessionEntry {
        id: "00009999".into(),
        parent_id: entries.last().map(|entry| entry.id.clone()),
        timestamp: 2000,
        kind: user(2000, "arrived during replay"),
    };
    append_entry(&mut connection, 1006, &extra).unwrap();
    entries.push(extra);
    let tx = connection.transaction().unwrap();
    assert_eq!(
        rebuild_chunk(&tx, &snapshot).unwrap().0,
        RebuildProgress::NeedsSnapshot
    );
    tx.commit().unwrap();
    let updated = prepared(&mut connection, &entries);
    let tx = connection.transaction().unwrap();
    let (progress, event) = rebuild_chunk(&tx, &updated).unwrap();
    assert_eq!(
        progress,
        RebuildProgress::Replaced {
            epoch: updated.ticket.epoch
        }
    );
    assert_eq!(event.unwrap().flipped_epoch, Some(updated.ticket.epoch));
    assert_eq!(read_all(&tx, updated.ticket.epoch), oracle(&entries));
    tx.commit().unwrap();
}

fn file_store() -> (tempfile::TempDir, Store, AccountId, TaskKey, String) {
    use super::super::tasks::{NewTask, TaskKind};
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(super::super::StorePaths::new(directory.path())).unwrap();
    let account = store.upsert_account("projector@example.test", 1).unwrap();
    let uuid = "00000000-0000-7000-8000-000000000001".to_owned();
    let task = store
        .create_task(
            account,
            &NewTask {
                uuid: uuid.clone(),
                project_root: if cfg!(windows) {
                    "C:/project".into()
                } else {
                    "/project".into()
                },
                kind: TaskKind::Desktop,
                title: "Test".into(),
                provisional: false,
                model: None,
                created_ms: 1,
            },
        )
        .unwrap();
    (directory, store, account, task.key, uuid)
}

fn save_entry(store: &Store, task: TaskKey, seq: i64, entry: &SessionEntry) {
    let prepared = prepare_entry(entry, seq).unwrap();
    let raw = serde_json::to_vec(entry).unwrap();
    let (entry_codec, payload) = codec::encode(&raw).unwrap();
    store.write(super::super::Durability::Durable,|tx| {
        tx.execute("INSERT INTO entries(task_id,seq,entry_id,kind,created_ms,codec,bytes,payload) VALUES(?1,?2,?3,'message',?4,?5,?6,?7)",params![task.0,seq,entry.id,entry.timestamp,entry_codec,payload.len() as i64,payload])?;
        append_prepared(tx,task,seq,entry,&prepared)?;
        tx.execute("UPDATE tasks SET last_seq=?2 WHERE id=?1",params![task.0,seq])?;
        Ok(())
    }).unwrap();
}

#[test]
fn noncanonical_id_rescan_fallback_matches_compact_outline() {
    let (_directory, store, account, task, uuid) = file_store();
    let entries = vec![
        SessionEntry {
            id: "first".into(),
            parent_id: None,
            timestamp: 1,
            kind: user(1, "hidden"),
        },
        SessionEntry {
            id: "second".into(),
            parent_id: Some("first".into()),
            timestamp: 2,
            kind: user(2, "retained"),
        },
        SessionEntry {
            id: "third".into(),
            parent_id: Some("second".into()),
            timestamp: 3,
            kind: EntryKind::ContextEdit {
                target_id: "first".into(),
                replacement: None,
            },
        },
    ];
    for (index, entry) in entries.iter().enumerate() {
        save_entry(&store, task, index as i64 + 1, entry);
    }
    let ticket = store.begin_timeline_rebuild(account, &uuid, false).unwrap();
    let prepared = store.prepare_timeline_rebuild(&ticket).unwrap();
    assert!(matches!(prepared.outline, Outline::Rescan));
    let chunk = store.prepare_timeline_chunk(&prepared).unwrap();
    assert!(chunk.complete);
    assert!(chunk.writes.len() <= CHUNK_ROWS);
    let progress = store.commit_timeline_chunk(&chunk, false).unwrap();
    assert_eq!(
        progress,
        RebuildProgress::Replaced {
            epoch: ticket.epoch
        }
    );
    store
        .read(|tx| {
            assert_eq!(read_all(tx, ticket.epoch), oracle(&entries));
            Ok(())
        })
        .unwrap();
}

#[test]
fn prepared_chunk_rejects_new_context_edit_and_account_deletion() {
    let (_directory, store, account, task, uuid) = file_store();
    let first = SessionEntry {
        id: "00000001".into(),
        parent_id: None,
        timestamp: 1,
        kind: user(1, "prompt"),
    };
    save_entry(&store, task, 1, &first);
    let ticket = store.begin_timeline_rebuild(account, &uuid, false).unwrap();
    let prepared = store.prepare_timeline_rebuild(&ticket).unwrap();
    let chunk = store.prepare_timeline_chunk(&prepared).unwrap();
    let edit = SessionEntry {
        id: "00000002".into(),
        parent_id: Some(first.id.clone()),
        timestamp: 2,
        kind: EntryKind::ContextEdit {
            target_id: first.id.clone(),
            replacement: None,
        },
    };
    save_entry(&store, task, 2, &edit);
    assert_eq!(
        store.commit_timeline_chunk(&chunk, false),
        Ok(RebuildProgress::NeedsSnapshot)
    );
    let new_ticket = store.begin_timeline_rebuild(account, &uuid, false).unwrap();
    let new_prepared = store.prepare_timeline_rebuild(&new_ticket).unwrap();
    let new_chunk = store.prepare_timeline_chunk(&new_prepared).unwrap();
    store.write(super::super::Durability::Durable,|tx| {tx.execute("UPDATE accounts SET deleting_ms=3,deletion_phase='pending_server',deletion_generation='generation' WHERE id=?1",[account.0])?;Ok(())}).unwrap();
    assert_eq!(
        store.commit_timeline_chunk(&new_chunk, false),
        Err(StoreError::Conflict)
    );
}

#[test]
fn compressed_sources_are_reduced_to_metadata_one_at_a_time() {
    let mut connection = database();
    let mut previous = None;
    for seq in 1..=4 {
        let entry = SessionEntry {
            id: format!("{seq:08x}"),
            parent_id: previous,
            timestamp: seq,
            kind: user(seq, &"large".repeat(400_000)),
        };
        previous = Some(entry.id.clone());
        append_entry(&mut connection, seq, &entry).unwrap();
    }
    let tx = connection.transaction().unwrap();
    let metadata = source_metadata_batch(&tx, TaskKey(1), 0, 4).unwrap();
    assert_eq!(metadata.len(), 4);
    assert!(metadata.iter().all(|(_, metadata)| metadata.is_some()));
    assert!(
        std::mem::size_of_val(metadata.as_slice()) < 1024,
        "the retained batch contains no source payload strings"
    );
}
