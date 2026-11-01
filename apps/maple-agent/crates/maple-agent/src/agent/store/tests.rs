use super::*;
use crate::store::{StorePaths, StoreState};
use pi_ai::{Content, UserMessage};

fn store() -> (tempfile::TempDir, TaskStore) {
    let directory = tempfile::tempdir().unwrap();
    let source = Arc::new(Store::open(StorePaths::new(directory.path())).unwrap());
    let account = source.upsert_account("alice@example.test", 1).unwrap();
    (
        directory,
        TaskStore::new(source, account, "alice@example.test"),
    )
}

fn id(number: u64) -> String {
    format!("00000000-0000-7000-8000-{number:012x}")
}

fn root(name: &str) -> String {
    if cfg!(windows) {
        format!("C:/{name}")
    } else {
        format!("/{name}")
    }
}

fn row(number: u64, project: &str, now_ms: i64) -> TaskRow {
    TaskRow::new(
        id(number),
        "New task".into(),
        root(project),
        TaskKind::Desktop,
        Some("glm-5-3".into()),
        now_ms,
    )
}

fn user(text: &str) -> SessionMessage {
    SessionMessage::Llm(Message::User(UserMessage {
        content: vec![Content::text(text)],
        timestamp: 10,
    }))
}

#[test]
fn typed_rows_round_trip_and_whole_lists_keep_empty_and_provisional_tasks() {
    let (_directory, store) = store();
    store.insert(&row(1, "one", 10)).unwrap();
    store.insert(&row(2, "two", 30)).unwrap();
    let mut provisional = row(3, "one", 20);
    provisional.kind = TaskKind::Acp;
    provisional.settings[PROVISIONAL] = Value::Bool(true);
    store.insert(&provisional).unwrap();

    assert_eq!(
        store
            .list(None)
            .unwrap()
            .into_iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        [id(2), id(3), id(1)]
    );
    assert_eq!(
        store
            .list(Some(&root("one")))
            .unwrap()
            .into_iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        [id(3), id(1)]
    );
    assert_eq!(store.get(&id(3)).unwrap(), Some(provisional));
    assert!(store.list(Some(&root("missing"))).unwrap().is_empty());

    let updated = store
        .update(&id(1), |row| {
            row.title = "Fix the build".into();
            row.title_user_set = true;
            row.state = AgentTaskState::Archived;
            row.web_enabled = false;
            row.settings = serde_json::json!({"mcp": ["github"]});
        })
        .unwrap()
        .unwrap();
    assert_eq!(store.get(&id(1)).unwrap().unwrap(), updated);
    assert_eq!(updated.summary().title, "Fix the build");
    assert_eq!(updated.summary().state, AgentTaskState::Archived);
    assert!(!updated.summary().web_enabled);
    assert!(store.update(&id(999), |_| {}).unwrap().is_none());
    let task = store
        .store()
        .task(store.account(), &id(3))
        .unwrap()
        .unwrap();
    assert!(task.provisional);
    assert!(task.settings.get(PROVISIONAL).is_none());
    store
        .update(&id(3), |row| {
            row.settings.as_object_mut().unwrap().remove(PROVISIONAL);
        })
        .unwrap();
    assert!(
        !store
            .store()
            .task(store.account(), &id(3))
            .unwrap()
            .unwrap()
            .provisional
    );
}

#[test]
fn whole_list_walks_multiple_typed_pages() {
    let (_directory, store) = store();
    for number in 1..=205 {
        store
            .insert(&row(
                number,
                if number % 2 == 0 { "even" } else { "odd" },
                number as i64,
            ))
            .unwrap();
    }
    let rows = store.list(None).unwrap();
    assert_eq!(rows.len(), 205);
    assert_eq!(rows.first().unwrap().id, id(205));
    assert_eq!(rows.last().unwrap().id, id(1));
}

#[test]
fn session_appends_own_the_caches_and_refresh_cannot_replace_them() {
    let (_directory, store) = store();
    store.insert(&row(1, "one", 1)).unwrap();
    let mut session = store.open_session(&id(1), &root("one")).unwrap();
    assert_eq!(session.id(), id(1));
    session.append_model_change("maple", "glm-5-3");
    session.append_message(user("hello"));
    assert!(!session.has_unpersisted());
    let committed = store.get(&id(1)).unwrap().unwrap();
    assert_eq!(committed.message_count, 1);
    assert_eq!(committed.model.as_deref(), Some("glm-5-3"));
    let refreshed = store
        .refresh_caches(
            &id(1),
            &SessionFacts {
                model: Some("stale-model".into()),
                message_count: 999,
                updated_ms: Some(i64::MAX),
            },
        )
        .unwrap()
        .unwrap();
    assert_eq!(refreshed, committed);
    assert!(store.context_tokens(&id(1)).unwrap().is_some());
    let reopened = store.open_session(&id(1), &root("elsewhere")).unwrap();
    assert_eq!(reopened.cwd(), root("one"));
    assert_eq!(SessionFacts::of(&reopened).message_count, 1);
    assert_eq!(store.timeline(&id(1)).unwrap().len(), 1);
}

#[test]
fn whole_timeline_keeps_repeated_keys_across_pages_and_expands_long_unicode_rows() {
    let (_directory, store) = store();
    store.insert(&row(1, "one", 1)).unwrap();
    let mut session = store.open_session(&id(1), &root("one")).unwrap();
    let text = "🦊 café ".repeat(250_000);
    session.append_message(user(&text));
    for _ in 0..200 {
        session.append_message(user("same timestamp"));
    }
    assert!(session.take_persist_error().is_none());
    let rows = store.timeline(&id(1)).unwrap();
    assert_eq!(rows.len(), 201);
    assert_eq!(rows[0].text.as_deref(), Some(text.as_str()));
    assert!(rows.iter().all(|row| row.truncated.is_none()));
    assert!(rows.iter().all(|row| row.id == rows[0].id));
}

#[test]
fn foreign_account_cannot_read_open_change_or_delete_a_task() {
    let (_directory, alice) = store();
    alice.insert(&row(1, "one", 1)).unwrap();
    let account = alice.store().upsert_account("bob@example.test", 1).unwrap();
    let bob = TaskStore::new(Arc::clone(alice.store()), account, "bob@example.test");
    assert!(bob.get(&id(1)).unwrap().is_none());
    assert!(bob.list(None).unwrap().is_empty());
    assert!(bob.open_session(&id(1), &root("one")).is_err());
    assert!(bob.timeline(&id(1)).is_err());
    assert!(bob.context_tokens(&id(1)).is_err());
    assert!(
        bob.update(&id(1), |row| row.title = "foreign".into())
            .unwrap()
            .is_none()
    );
    assert!(!bob.delete(&id(1)).unwrap());
    assert_eq!(alice.get(&id(1)).unwrap().unwrap().title, "New task");
}

#[test]
fn metadata_compare_and_swap_preserves_a_concurrent_user_rename() {
    let (_directory, store) = store();
    store.insert(&row(1, "one", 1)).unwrap();
    let result = store.update(&id(1), |row| {
        store
            .store()
            .update_task(
                store.account(),
                &id(1),
                &TaskPatch {
                    title: Some("User's title".into()),
                    title_user_set: Some(true),
                    ..TaskPatch::default()
                },
            )
            .unwrap();
        row.title = "Generated title".into();
    });
    assert!(result.is_err());
    let saved = store.get(&id(1)).unwrap().unwrap();
    assert_eq!(saved.title, "User's title");
    assert!(saved.title_user_set);
}

/// A started task takes a selection for its next run; the session's next
/// append then records the model the session has.
#[test]
fn a_started_task_takes_a_model_selection_until_its_session_records_one() {
    let (_directory, store) = store();
    store.insert(&row(1, "one", 1)).unwrap();
    store
        .update(&id(1), |row| row.model = Some("initial-selection".into()))
        .unwrap();
    let mut session = store.open_session(&id(1), &root("one")).unwrap();
    session.append_model_change("maple", "initial-selection");
    session.append_message(user("first message"));

    let selected = store
        .update(&id(1), |row| row.model = Some("next-selection".into()))
        .unwrap()
        .unwrap();
    assert_eq!(selected.model.as_deref(), Some("next-selection"));
    assert_eq!(selected.message_count, 1);

    session.append_model_change("maple", "next-selection");
    session.append_message(user("second message"));
    let saved = store.get(&id(1)).unwrap().unwrap();
    assert_eq!(saved.model.as_deref(), Some("next-selection"));
    assert_eq!(saved.message_count, 2);
}

#[test]
fn failed_initial_metadata_does_not_leave_an_unconfigured_task() {
    let (_directory, store) = store();
    let mut task = row(1, "one", 1);
    task.settings = serde_json::json!({"oversized": "x".repeat(256 * 1024)});
    assert!(store.insert(&task).is_err());
    assert!(store.get(&task.id).unwrap().is_none());
}

#[test]
fn delete_hides_immediately_and_leaves_bounded_collection_to_maintenance() {
    let (directory, store) = store();
    let legacy = directory.path().join("sessions");
    std::fs::create_dir(&legacy).unwrap();
    let previous_session = legacy.join(format!("{}.jsonl", id(1)));
    let previous_index = legacy.join("tasks.sqlite");
    std::fs::write(&previous_session, b"previous session sentinel").unwrap();
    std::fs::write(&previous_index, b"previous index sentinel").unwrap();
    store.insert(&row(1, "one", 1)).unwrap();
    let mut session = store.open_session(&id(1), &root("one")).unwrap();
    session.append_message(user("hello"));
    assert!(store.delete(&id(1)).unwrap());
    assert!(store.get(&id(1)).unwrap().is_none());
    assert!(
        store
            .store
            .deleted_tasks(10)
            .unwrap()
            .iter()
            .any(|task| task.uuid == id(1))
    );
    assert!(!store.delete(&id(1)).unwrap());
    assert_eq!(
        std::fs::read(previous_session).unwrap(),
        b"previous session sentinel"
    );
    assert_eq!(
        std::fs::read(previous_index).unwrap(),
        b"previous index sentinel"
    );
}

#[test]
fn construction_accepts_a_locked_host_handle_without_opening_another_store() {
    let directory = tempfile::tempdir().unwrap();
    let (_other_directory, open) = store();
    let source = Arc::new(
        Store::unavailable(StorePaths::new(directory.path()), StoreState::Locked).unwrap(),
    );
    let adapter = TaskStore::new(Arc::clone(&source), open.account(), "alice@example.test");
    assert!(Arc::ptr_eq(adapter.store(), &source));
    assert!(adapter.get(&id(1)).is_err());
    assert!(!source.paths().database().exists());
}
