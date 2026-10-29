use base64::Engine;
use pi_ai::{
    AssistantContent, AssistantMessage, Content, ImageContent, StopReason, ThinkingLevel,
    ToolResultMessage, Usage, UserMessage,
};
use pi_coding_agent::compaction::estimate_projected_context_tokens;
use pi_coding_agent::messages::{BashExecutionMessage, CustomMessage};
use serde_json::json;

use super::*;
use crate::store::StorePaths;
use crate::store::StoreState;
use crate::store::tasks::{NewTask, TaskKind};

const UUID: &str = "00000000-0000-7000-8000-000000000001";

fn fixture() -> (tempfile::TempDir, Arc<Store>, AccountId) {
    fixture_with_key(false)
}

fn image_test_store(paths: StorePaths, keyed: bool) -> Arc<Store> {
    Arc::new(if keyed {
        Store::open_keyed(
            paths,
            crate::store::keys::LockedKey::from_bytes([0x51; 32]).unwrap(),
        )
        .unwrap()
    } else {
        Store::open(paths).unwrap()
    })
}

fn fixture_with_key(keyed: bool) -> (tempfile::TempDir, Arc<Store>, AccountId) {
    let directory = tempfile::tempdir().unwrap();
    let store = image_test_store(StorePaths::new(directory.path()), keyed);
    let account = store.upsert_account("alice@example.test", 1).unwrap();
    store
        .create_task(
            account,
            &NewTask {
                uuid: UUID.into(),
                project_root: if cfg!(windows) {
                    "C:/projects/test"
                } else {
                    "/projects/test"
                }
                .into(),
                kind: TaskKind::Desktop,
                title: "Session fixture".into(),
                provisional: false,
                model: Some("seed-model".into()),
                created_ms: 1,
            },
        )
        .unwrap();
    (directory, store, account)
}

fn user(text: &str) -> SessionMessage {
    SessionMessage::Llm(Message::User(UserMessage {
        content: vec![Content::text(text)],
        timestamp: 10,
    }))
}

#[test]
fn actual_sqlite_full_preserves_pi_source_until_retry_saves_the_same_entries() {
    let (_directory, store, account) = fixture();
    let loaded = store
        .load_session(account, UUID, SessionLoad::Context)
        .unwrap();
    let receipts = loaded.source_receipts();
    let mut manager = loaded.into_manager("/projects/test");
    let saved_id = manager.append_message(user("saved prefix"));
    assert_eq!(receipts.drain()[0].entry_id, saved_id);
    assert!(!manager.has_unpersisted());
    let before = store.task(account, UUID).unwrap().unwrap();
    let saved = store
        .load_session(account, UUID, SessionLoad::Full)
        .unwrap()
        .entries;
    let mut changes = store.subscribe_changes();
    let pages = {
        let writer = store.writer.lock().unwrap();
        let connection = writer.as_ref().unwrap();
        let pages = connection
            .pragma_query_value::<i64, _>(None, "page_count", |row| row.get(0))
            .unwrap();
        connection
            .pragma_update(None, "max_page_count", pages)
            .unwrap();
        pages
    };
    // A deterministic, poorly compressible fixture must allocate new pages.
    let mut sequence = 0x39e7_a206_42bd_150fu64;
    let text: String = (0..1024 * 1024)
        .map(|_| {
            sequence ^= sequence << 13;
            sequence ^= sequence >> 7;
            sequence ^= sequence << 17;
            char::from(33 + (sequence % 90) as u8)
        })
        .collect();
    let failed_id = manager.append_message(user(&text));
    // A source receipt means observed by Pi, not durably committed.
    let observed = receipts.drain();
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].entry_id, failed_id);
    let error = manager
        .take_persist_error()
        .expect("SQLite must hit its real page limit");
    assert_eq!(error.kind(), io::ErrorKind::StorageFull);
    assert!(manager.has_unpersisted());
    assert_eq!(manager.entries().len(), saved.len() + 1);
    assert_eq!(store.state(), StoreState::Open);
    assert_eq!(store.task(account, UUID).unwrap().unwrap(), before);
    assert_eq!(
        store
            .load_session(account, UUID, SessionLoad::Full)
            .unwrap()
            .entries,
        saved
    );
    assert!(matches!(
        changes.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    assert!(manager.flush().is_err());
    assert!(
        receipts.drain().is_empty(),
        "retry must not repeat source receipts"
    );
    assert!(manager.has_unpersisted());
    let failure = store.persistence_failure().unwrap();
    assert_eq!(
        failure.reason,
        super::super::PersistenceFailureReason::DiskFull
    );
    {
        let writer = store.writer.lock().unwrap();
        writer
            .as_ref()
            .unwrap()
            .pragma_update(None, "max_page_count", pages + 8192)
            .unwrap();
    }
    let retained = manager.entries().to_vec();
    manager.flush().unwrap();
    assert!(!manager.has_unpersisted());
    assert_eq!(manager.entries(), retained);
    assert_eq!(
        store
            .load_session(account, UUID, SessionLoad::Full)
            .unwrap()
            .entries,
        retained
    );
    let committed = changes.try_recv().unwrap();
    assert!(!committed.rows.is_empty());
    assert!(
        committed
            .rows
            .iter()
            .all(|row| row.update_entry_id == failed_id)
    );
    assert!(receipts.drain().is_empty());
    // Only the explicit all-context retry barrier clears the visible notice.
    assert_eq!(store.persistence_failure().as_ref(), Some(&failure));
    assert!(store.clear_persistence_failure(failure.revision));
}

#[test]
fn receipt_queue_seeds_cold_history_and_observes_only_new_source_entries() {
    let (_directory, store, account) = fixture();
    let loaded = store
        .load_session(account, UUID, SessionLoad::Context)
        .unwrap();
    let receipts = loaded.source_receipts();
    let mut manager = loaded.into_manager("/projects/test");
    manager.append_model_change("fixture", "test-model");
    let provisional = manager.append_custom_message(
        "provisional",
        vec![Content::text("before conversation")],
        false,
        None,
    );
    // This is Pi's lazy store contract: no SQL attempt or source receipt yet.
    assert!(receipts.drain().is_empty());
    assert_eq!(store.task(account, UUID).unwrap().unwrap().last_seq, 0);
    let first = manager.append_message(user("same text"));
    let observed = receipts.drain();
    assert_eq!(
        observed
            .iter()
            .map(|entry| entry.entry_id.as_str())
            .collect::<Vec<_>>(),
        [provisional.as_str(), first.as_str()]
    );
    let custom = manager.append_custom_message(
        "visible",
        vec![Content::text("after conversation")],
        true,
        Some(json!({"data": 1})),
    );
    let observed = receipts.drain();
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].entry_id, custom);
    assert_eq!(
        observed[0].signature,
        SourceReceipt::of(manager.entry(&custom).unwrap())
            .unwrap()
            .signature
    );
    drop(manager);
    let loaded = store
        .load_session(account, UUID, SessionLoad::Context)
        .unwrap();
    let cold = loaded.source_receipts();
    let mut manager = loaded.into_manager("/projects/test");
    assert!(cold.drain().is_empty(), "loading is not a live append");
    let second = manager.append_message(user("same text"));
    assert_ne!(first, second);
    let observed = cold.drain();
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].entry_id, second);
    manager.flush().unwrap();
    assert!(cold.drain().is_empty());
}

fn assistant(text: &str, tokens: u64) -> SessionMessage {
    SessionMessage::Llm(Message::Assistant(AssistantMessage {
        content: vec![AssistantContent::text(text)],
        api: "fixture".into(),
        provider: "fixture".into(),
        model: "test-model".into(),
        response_id: Some("response-id".into()),
        thinking_level: Some(ThinkingLevel::High),
        usage: Usage {
            input: tokens,
            total_tokens: tokens,
            ..Usage::default()
        },
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: 20,
    }))
}

fn header() -> SessionHeader {
    SessionHeader {
        version: 1,
        id: UUID.into(),
        timestamp: 1,
        cwd: "/projects/test".into(),
        parent_session: None,
    }
}

fn entries(kinds: Vec<EntryKind>) -> Vec<SessionEntry> {
    kinds
        .into_iter()
        .enumerate()
        .map(|(i, kind)| SessionEntry {
            id: format!("{i:08x}"),
            parent_id: i.checked_sub(1).map(|i| format!("{i:08x}")),
            timestamp: i as i64 + 2,
            kind,
        })
        .collect()
}

fn cache_parity(store: &Store, account: AccountId, manager: &SessionManager) {
    let task = store.task(account, UUID).unwrap().unwrap();
    let path = manager.branch();
    let projection = manager.projection();
    let tokens = estimate_projected_context_tokens(&projection, &path);
    let oracle = ContextCache::of(&projection, &path);
    assert_eq!(task.facts.context_estimate, oracle.estimate());
    assert_eq!(task.facts.context_usage, oracle.usage());
    assert_eq!(oracle.tokens(), tokens);
    assert_eq!(task.facts.model, projection.model.map(|(_, model)| model));
    assert_eq!(
        task.facts.thinking_level,
        Some(thinking_name(projection.thinking_level))
    );
}

#[test]
fn all_entry_kinds_and_message_variants_round_trip_exactly() {
    let (_directory, store, account) = fixture();
    let stored = store
        .store_image_uploads(
            account,
            UUID,
            &[crate::store::ImageUpload {
                name: "fixture.png".into(),
                data_url: format!(
                    "data:image/png;base64,{}",
                    base64::engine::general_purpose::STANDARD.encode(b"\x89PNG\r\n\x1a\nfixture")
                ),
            }],
        )
        .unwrap();
    let image = ImageContent {
        data: stored[0].source.clone(),
        mime_type: "image/png".into(),
    };
    let mut first_user = UserMessage {
        content: vec![
            Content::text("emoji 🦊 and café"),
            Content::Image(image.clone()),
        ],
        timestamp: 7,
    };
    first_user
        .content
        .push(Content::text("compress me 🦊".repeat(1800)));
    let source = entries(vec![
        EntryKind::ModelChange {
            provider: "fixture".into(),
            model_id: "test-model".into(),
        },
        EntryKind::ThinkingLevelChange {
            thinking_level: ThinkingLevel::High,
        },
        EntryKind::Message {
            message: SessionMessage::Llm(Message::System(SystemMessage {
                content: "instructions".into(),
                timestamp: 3,
                ..Default::default()
            })),
        },
        EntryKind::Message {
            message: SessionMessage::Llm(Message::User(first_user)),
        },
        EntryKind::Message {
            message: assistant("answer", 120),
        },
        EntryKind::Message {
            message: SessionMessage::Llm(Message::ToolResult(ToolResultMessage {
                tool_call_id: "call-id".into(),
                tool_name: "read_image".into(),
                content: vec![Content::text("image result"), Content::Image(image.clone())],
                details: Some(json!({"opaque":{"data":"maple-blob:999"}})),
                usage: Some(Usage {
                    input: 10,
                    ..Default::default()
                }),
                is_error: false,
                timestamp: 9,
            })),
        },
        EntryKind::Message {
            message: SessionMessage::BashExecution(BashExecutionMessage {
                command: "pwd".into(),
                output: "/projects/test".into(),
                exit_code: Some(0),
                cancelled: false,
                truncated: false,
                full_output_path: None,
                timestamp: 9,
                exclude_from_context: false,
            }),
        },
        EntryKind::Message {
            message: SessionMessage::Custom(CustomMessage {
                custom_type: "extension-message".into(),
                content: vec![Content::text("custom")],
                display: true,
                details: Some(json!([null,1,true,{"a":"b"}])),
                timestamp: 10,
            }),
        },
        EntryKind::Custom {
            custom_type: "opaque".into(),
            data: Some(
                json!({"data":"maple-blob:999","number":123456789012345_u64,"nested":[null,false]}),
            ),
        },
        EntryKind::CustomMessage {
            custom_type: "extension".into(),
            content: vec![
                Content::text("visible custom"),
                Content::Image(image.clone()),
            ],
            display: true,
            details: Some(json!({"unknown":"preserved"})),
        },
        EntryKind::ContextEdit {
            target_id: "00000003".into(),
            replacement: Some(vec![Content::text("edited"), Content::Image(image)]),
        },
        EntryKind::Label {
            target_id: "00000003".into(),
            label: Some("start".into()),
        },
        EntryKind::SessionInfo {
            name: Some("Unicode session 🦊".into()),
        },
        EntryKind::BranchSummary {
            from_id: "branch-origin".into(),
            summary: "branch summary".into(),
            details: Some(json!({"keep":true})),
            usage: Some(Usage::default()),
            from_extension: true,
        },
        EntryKind::Compaction {
            summary: "compacted summary".into(),
            first_kept_entry_id: "00000003".into(),
            tokens_before: 12345,
            details: Some(json!({"version":2})),
            usage: Some(Usage::default()),
            from_extension: false,
            system_message: Some(SystemMessage {
                content: "instructions".into(),
                timestamp: 3,
                ..Default::default()
            }),
        },
    ]);
    let mut adapter = store
        .load_session(account, UUID, SessionLoad::Context)
        .unwrap()
        .store;
    assert!(adapter.file().is_none());
    adapter.write_all(&header(), &source).unwrap();
    let loaded = store
        .load_session(account, UUID, SessionLoad::Full)
        .unwrap();
    assert_eq!(loaded.header, Some(header()));
    assert_eq!(
        serde_json::to_vec(&loaded.entries).unwrap(),
        serde_json::to_vec(&source).unwrap()
    );
    assert!(adapter.contains_entry("00000000").unwrap());
    let (compressed, files): (i64, i64) = store
        .read(|tx| {
            Ok((
                tx.query_row("SELECT count(*) FROM entries WHERE codec=1", [], |r| {
                    r.get(0)
                })?,
                tx.query_row(
                    "SELECT count(*) FROM file_attachments WHERE state='ready' AND bound=1",
                    [],
                    |r| r.get(0),
                )?,
            ))
        })
        .unwrap();
    assert!(compressed > 0);
    assert_eq!(files, 1);
}

#[test]
fn third_party_images_survive_failed_append_retry_and_cold_plain_or_keyed_reopen() {
    use base64::engine::general_purpose::STANDARD;

    for keyed in [false, true] {
        let (directory, store, account) = fixture_with_key(keyed);
        let loaded = store
            .load_session(account, UUID, SessionLoad::Context)
            .unwrap();
        let receipts = loaded.source_receipts();
        let mut manager = loaded.into_manager("/projects/test");
        manager.append_message(user("saved before a third-party tool result"));
        receipts.drain();
        let canonical = STANDARD.encode(b"\x89PNG\r\n\x1a\n\xfb\xff");
        let mut content = vec![
            Content::image("not base64!", "image/png"),
            Content::image("", "image/png"),
            Content::image(format!("data:image/png;base64,{canonical}"), "image/png"),
            Content::image(format!("{canonical}\n"), "image/png"),
            Content::image(canonical.trim_end_matches('='), "image/png"),
            Content::image(canonical.replace('+', "-").replace('/', "_"), "image/png"),
            Content::image(canonical.clone(), ""),
            Content::image("maple-blob:1", "image/png"),
            Content::image("maple-inline-image:bWFwbGUtYmxvYjox", "image/png"),
            Content::image("opaque image 🦊", "image/png"),
        ];
        content.extend([
            Content::image(canonical.clone(), "image/png"),
            Content::image(canonical.clone(), "image/png"),
        ]);
        // Keep the exact formerly poisoning entry in Pi's retry slice while an
        // unrelated transaction failure forces write_all instead of append.
        store.write(Durability::Durable, |tx| {
            tx.execute_batch("CREATE TEMP TRIGGER fail_image_entry BEFORE UPDATE OF last_seq ON tasks WHEN NEW.last_seq<>OLD.last_seq BEGIN SELECT RAISE(ABORT,'fixture'); END;")?;
            Ok(())
        }).unwrap();
        let image_id = manager.append_message(SessionMessage::Llm(Message::ToolResult(
            ToolResultMessage {
                tool_call_id: "third-party-image".into(),
                tool_name: "mcp__fixture__screenshot".into(),
                content,
                details: Some(json!({"data":"maple-inline-image:opaque-details"})),
                usage: None,
                is_error: false,
                timestamp: 11,
            },
        )));
        assert!(manager.take_persist_error().is_some());
        assert!(manager.has_unpersisted());
        assert_eq!(receipts.drain()[0].entry_id, image_id);
        manager.append_message(user("work following the image"));
        assert!(manager.take_persist_error().is_some());
        receipts.drain();
        let retained = manager.entries().to_vec();
        store
            .write(Durability::Durable, |tx| {
                tx.execute_batch("DROP TRIGGER fail_image_entry")?;
                Ok(())
            })
            .unwrap();
        manager.flush().unwrap();
        assert!(!manager.has_unpersisted());
        assert!(
            receipts.drain().is_empty(),
            "retry must not repeat identities"
        );
        assert_eq!(manager.entries(), retained);
        cache_parity(&store, account, &manager);
        let persisted = store
            .load_session(account, UUID, SessionLoad::Full)
            .unwrap()
            .entries;
        let original = retained.iter().find(|entry| entry.id == image_id).unwrap();
        let saved = persisted.iter().find(|entry| entry.id == image_id).unwrap();
        assert_eq!(
            (&saved.id, &saved.parent_id, saved.timestamp),
            (&original.id, &original.parent_id, original.timestamp)
        );
        let EntryKind::Message {
            message: SessionMessage::Llm(Message::ToolResult(result)),
        } = &saved.kind
        else {
            panic!("tool result must retain its entry kind");
        };
        assert!(!result.is_error);
        assert_eq!(result.tool_call_id, "third-party-image");
        assert_eq!(result.tool_name, "mcp__fixture__screenshot");
        assert_eq!(
            result.details,
            Some(json!({"data":"maple-inline-image:opaque-details"}))
        );
        assert_eq!(result.content.len(), 12);
        for block in &result.content {
            let Content::Image(image) = block else {
                panic!("typed image marker must remain typed");
            };
            assert!(
                image.data.starts_with("maple-attachment://")
                    || image.data == "maple-attachment-unavailable:retired"
            );
            assert!(
                !image.data.contains(&canonical),
                "source rows must not retain image bytes"
            );
        }
        for index in [0, 1, 6, 7, 8, 9] {
            let Content::Image(image) = &result.content[index] else {
                unreachable!()
            };
            assert_eq!(image.data, "maple-attachment-unavailable:retired");
        }
        for index in [2, 3, 10, 11] {
            let Content::Image(image) = &result.content[index] else {
                unreachable!()
            };
            assert!(
                image.data.starts_with("maple-attachment://"),
                "valid wrapped/native media must be retained"
            );
        }
        let mut model_messages = vec![Message::ToolResult(result.clone())];
        store
            .materialize_message_images(account, UUID, &mut model_messages)
            .unwrap();
        let Message::ToolResult(materialized) = &model_messages[0] else {
            unreachable!()
        };
        for index in [2, 3, 10, 11] {
            let Content::Image(image) = &materialized.content[index] else {
                panic!("valid image lost");
            };
            assert_eq!(image.data, canonical);
        }
        for index in [0, 1, 6, 7, 8, 9] {
            assert!(
                matches!(&materialized.content[index], Content::Text(text) if text.text.contains("no longer available"))
            );
        }
        let (files, bound): (i64, i64) = store
            .read(|tx| {
                Ok((
                    tx.query_row(
                        "SELECT count(*) FROM file_attachments WHERE state='ready'",
                        [],
                        |row| row.get(0),
                    )?,
                    tx.query_row(
                        "SELECT count(*) FROM file_attachments WHERE state='ready' AND bound=1",
                        [],
                        |row| row.get(0),
                    )?,
                ))
            })
            .unwrap();
        assert!(files >= 4);
        assert_eq!(
            files, bound,
            "retry must not leave duplicate unbound copies"
        );
        let page_request = crate::store::paging::OpenTaskPageRequest {
            task: UUID.into(),
            limit_rows: None,
            limit_bytes: None,
        };
        let items = |store: &Store| {
            store
                .open_task_page(
                    account,
                    &page_request,
                    crate::store::paging::PageTransport::Local,
                )
                .unwrap()
                .rows
                .into_iter()
                .map(|row| row.item)
                .collect::<Vec<_>>()
        };
        let before_rebuild = items(&store);
        let ticket = store.begin_timeline_rebuild(account, UUID, false).unwrap();
        let prepared = store.prepare_timeline_rebuild(&ticket).unwrap();
        let chunk = store.prepare_timeline_chunk(&prepared).unwrap();
        assert!(matches!(
            store.commit_timeline_chunk(&chunk, false).unwrap(),
            crate::store::projector::RebuildProgress::Replaced { .. }
        ));
        assert_eq!(
            items(&store),
            before_rebuild,
            "metadata rebuild preserves canonical image lengths"
        );
        drop(manager);
        store.close_for_shutdown().unwrap();
        drop(store);
        let reopened = image_test_store(StorePaths::new(directory.path()), keyed);
        let cold = reopened
            .load_session(account, UUID, SessionLoad::Full)
            .unwrap();
        assert_eq!(cold.entries, persisted);
        let mut manager = cold.into_manager("/projects/test");
        manager.append_message(user("continues after reopening"));
        assert!(manager.take_persist_error().is_none());
        assert!(!manager.has_unpersisted());
        assert_eq!(
            reopened
                .load_session(account, UUID, SessionLoad::Full)
                .unwrap()
                .entries,
            manager.entries()
        );
        drop(manager);
        reopened.close_for_shutdown().unwrap();
    }
}

#[test]
fn metadata_only_is_lazy_and_failed_persistence_retries_without_loss() {
    let (_directory, store, account) = fixture();
    let mut manager = store
        .load_session(account, UUID, SessionLoad::Context)
        .unwrap()
        .into_manager("/projects/test");
    manager.append_model_change("fixture", "test-model");
    manager.append_thinking_level_change(ThinkingLevel::High);
    assert_eq!(store.task(account, UUID).unwrap().unwrap().last_seq, 0);
    manager.append_message(user("first"));
    assert!(!manager.has_unpersisted());
    let before = store.task(account, UUID).unwrap().unwrap();
    let mut changes = store.subscribe_changes();
    // Fail after both the source entry and projected rows have been written.
    store.write(Durability::Durable,|tx| {tx.execute_batch("CREATE TEMP TRIGGER fail_session BEFORE UPDATE OF last_seq ON tasks WHEN NEW.last_seq<>OLD.last_seq BEGIN SELECT RAISE(ABORT,'fixture'); END;")?;Ok(())}).unwrap();
    manager.append_message(user("must retry the same real entry"));
    assert!(manager.has_unpersisted());
    assert!(manager.take_persist_error().is_some());
    assert_eq!(store.task(account, UUID).unwrap().unwrap(), before);
    assert_eq!(
        store
            .load_session(account, UUID, SessionLoad::Full)
            .unwrap()
            .entries
            .len(),
        3
    );
    assert!(changes.try_recv().is_err());
    store
        .write(Durability::Durable, |tx| {
            tx.execute_batch("DROP TRIGGER fail_session")?;
            Ok(())
        })
        .unwrap();
    manager.flush().unwrap();
    assert!(!manager.has_unpersisted());
    let loaded = store
        .load_session(account, UUID, SessionLoad::Full)
        .unwrap();
    assert_eq!(loaded.entries, manager.entries());
    assert_eq!(loaded.entries.len(), 4);
    assert_eq!(
        store
            .task(account, UUID)
            .unwrap()
            .unwrap()
            .facts
            .message_count,
        2
    );
    cache_parity(&store, account, &manager);
}

#[test]
fn first_write_failure_rolls_back_header_entries_rows_and_all_caches() {
    let (_directory, store, account) = fixture();
    let mut adapter = store
        .load_session(account, UUID, SessionLoad::Context)
        .unwrap()
        .store;
    let source = entries(vec![
        EntryKind::Message {
            message: user("first"),
        },
        EntryKind::Message {
            message: user("second"),
        },
        EntryKind::Message {
            message: assistant("third", 90),
        },
    ]);
    store.write(Durability::Durable,|tx| {tx.execute_batch("CREATE TEMP TRIGGER fail_session BEFORE UPDATE OF last_seq ON tasks WHEN NEW.last_seq=2 BEGIN SELECT RAISE(ABORT,'fixture'); END;")?;Ok(())}).unwrap();
    let mut changes = store.subscribe_changes();
    assert!(adapter.write_all(&header(), &source).is_err());
    let saved = store
        .load_session(account, UUID, SessionLoad::Full)
        .unwrap();
    assert!(saved.header.is_none());
    assert!(saved.entries.is_empty());
    assert_eq!(store.task(account, UUID).unwrap().unwrap().last_row_seq, 0);
    assert!(changes.try_recv().is_err());
    store
        .write(Durability::Durable, |tx| {
            tx.execute_batch("DROP TRIGGER fail_session")?;
            Ok(())
        })
        .unwrap();
    adapter.write_all(&header(), &source).unwrap();
    assert_eq!(
        store
            .load_session(account, UUID, SessionLoad::Full)
            .unwrap()
            .entries,
        source
    );
}

#[test]
fn retries_verify_payloads_ids_and_tail_without_replacing_prefixes() {
    let (_directory, store, account) = fixture();
    let mut adapter = store
        .load_session(account, UUID, SessionLoad::Context)
        .unwrap()
        .store;
    let source = entries(vec![
        EntryKind::Message {
            message: user("one"),
        },
        EntryKind::Message {
            message: assistant("two", 100),
        },
    ]);
    adapter.write_all(&header(), &source).unwrap();
    adapter.write_all(&header(), &source).unwrap();
    let before = store.task(account, UUID).unwrap().unwrap();
    let mut corrupt = source.clone();
    corrupt[0].kind = EntryKind::Message {
        message: user("changed prefix"),
    };
    assert!(adapter.write_all(&header(), &corrupt).is_err());
    assert!(adapter.write_all(&header(), &source[..1]).is_err());
    assert!(adapter.append(&source[1]).is_err());
    let mut duplicate = source.clone();
    duplicate.push(source[1].clone());
    assert!(adapter.write_all(&header(), &duplicate).is_err());
    assert_eq!(store.task(account, UUID).unwrap().unwrap(), before);
    assert_eq!(
        store
            .load_session(account, UUID, SessionLoad::Full)
            .unwrap()
            .entries,
        source
    );
}

#[test]
fn bounded_context_restores_pre_boundary_facts_and_partial_retry_preserves_history() {
    let (_directory, store, account) = fixture();
    let mut manager = store
        .load_session(account, UUID, SessionLoad::Context)
        .unwrap()
        .into_manager("/projects/test");
    manager.append_model_change("fixture", "test-model");
    manager.append_thinking_level_change(ThinkingLevel::High);
    manager.append_session_info(Some("original name".into()));
    manager.append_message(SessionMessage::Llm(Message::System(SystemMessage {
        content: "base instructions".into(),
        timestamp: 1,
        ..Default::default()
    })));
    for i in 0..150 {
        manager.append_message(user(&format!("old history {i}")));
    }
    let kept = manager.append_message(user("kept context"));
    manager.append_message(assistant("old measurement", 400));
    manager.append_compaction("summary".into(), Some(kept.clone()), 800, None, None, false);
    manager.append_message(user("new turn"));
    cache_parity(&store, account, &manager);
    let total = manager.entries().len();
    let loaded = store
        .load_session(account, UUID, SessionLoad::Context)
        .unwrap();
    assert_eq!(loaded.entries.len(), 4);
    assert_eq!(
        loaded.base.model,
        Some(("fixture".into(), "test-model".into()))
    );
    assert_eq!(loaded.base.thinking_level, ThinkingLevel::High);
    assert_eq!(loaded.base.session_name.as_deref(), Some("original name"));
    let mut partial = loaded.into_manager("/projects/test");
    assert_eq!(partial.projection(), manager.projection());
    store.write(Durability::Durable,|tx| {tx.execute_batch("CREATE TEMP TRIGGER fail_session BEFORE INSERT ON entries BEGIN SELECT RAISE(ABORT,'fixture'); END;")?;Ok(())}).unwrap();
    partial.append_message(assistant("new measured reply", 125));
    assert!(partial.has_unpersisted());
    store
        .write(Durability::Durable, |tx| {
            tx.execute_batch("DROP TRIGGER fail_session")?;
            Ok(())
        })
        .unwrap();
    partial.flush().unwrap();
    cache_parity(&store, account, &partial);
    assert_eq!(
        store
            .task(account, UUID)
            .unwrap()
            .unwrap()
            .facts
            .message_count,
        manager
            .entries()
            .iter()
            .filter(|entry| conversation(entry))
            .count() as u64
            + 1
    );
    let full = store
        .load_session(account, UUID, SessionLoad::Full)
        .unwrap();
    assert_eq!(full.entries.len(), total + 1);
    assert_eq!(&full.entries[..total], manager.entries());
    assert_eq!(full.entries.last(), partial.entries().last());
    let full = full.into_manager("/projects/test");
    assert_eq!(partial.projection(), full.projection());
}

#[test]
fn context_edits_and_system_changes_match_current_pi_helper_after_every_write() {
    let (_directory, store, account) = fixture();
    let mut manager = store
        .load_session(account, UUID, SessionLoad::Context)
        .unwrap()
        .into_manager("/projects/test");
    manager.append_model_change("fixture", "test-model");
    manager.append_message(SessionMessage::Llm(Message::System(SystemMessage {
        content: "base".into(),
        timestamp: 1,
        ..Default::default()
    })));
    let original = manager.append_message(user("secret original"));
    manager.append_message(assistant("measured reply", 400));
    cache_parity(&store, account, &manager);
    manager.append_message(SessionMessage::Llm(Message::System(SystemMessage {
        content: "new rules".into(),
        timestamp: 4,
        ..Default::default()
    })));
    cache_parity(&store, account, &manager);
    manager
        .append_context_edit(&original, Some(vec![Content::text("short")]))
        .unwrap();
    cache_parity(&store, account, &manager);
    manager.append_context_edit(&original, None).unwrap();
    cache_parity(&store, account, &manager);
    let kept = manager.append_message(user("kept"));
    manager.append_compaction("compact".into(), Some(kept.clone()), 100, None, None, false);
    cache_parity(&store, account, &manager);
    manager.append_message(assistant("post-compaction measurement", 80));
    manager
        .append_context_edit(
            &kept,
            Some(vec![Content::text("replaced after measurement")]),
        )
        .unwrap();
    cache_parity(&store, account, &manager);
}

#[test]
fn account_isolation_and_deleted_tasks_reject_session_access_and_writes() {
    let (_directory, store, alice) = fixture();
    let bob = store.upsert_account("bob@example.test", 2).unwrap();
    assert!(matches!(
        store.load_session(bob, UUID, SessionLoad::Context),
        Err(StoreError::NotFound)
    ));
    let mut adapter = store
        .load_session(alice, UUID, SessionLoad::Context)
        .unwrap()
        .store;
    adapter
        .write_all(
            &header(),
            &entries(vec![EntryKind::Message {
                message: user("private"),
            }]),
        )
        .unwrap();
    store.mark_task_deleted(alice, UUID, 3).unwrap();
    assert!(adapter.contains_entry("00000000").is_err());
    assert!(
        adapter
            .append(&SessionEntry {
                id: "00000001".into(),
                parent_id: Some("00000000".into()),
                timestamp: 9,
                kind: EntryKind::Message {
                    message: user("too late")
                }
            })
            .is_err()
    );
    assert!(matches!(
        store.load_session(alice, UUID, SessionLoad::Full),
        Err(StoreError::NotFound)
    ));
}
