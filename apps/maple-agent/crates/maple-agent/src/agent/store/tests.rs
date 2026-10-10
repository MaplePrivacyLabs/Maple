use super::*;
use pi_ai::{Content, UserMessage};

fn store() -> (tempfile::TempDir, TaskStore) {
    let directory = tempfile::tempdir().unwrap();
    let store = TaskStore::open(directory.path()).unwrap();
    (directory, store)
}

fn row(id: &str, root: &str, updated_ms: i64) -> TaskRow {
    let mut row = TaskRow::new(
        id.to_string(),
        "New task".to_string(),
        root.to_string(),
        TaskKind::Desktop,
        Some("glm-5-3".to_string()),
        updated_ms,
    );
    row.updated_ms = updated_ms;
    row
}

fn user(text: &str) -> SessionMessage {
    SessionMessage::Llm(Message::User(UserMessage {
        content: vec![Content::text(text)],
        timestamp: 1,
    }))
}

#[test]
fn rows_round_trip_and_list_newest_first_by_root() {
    let (_directory, store) = store();
    store.insert(&row("a", "/one", 10)).unwrap();
    store.insert(&row("b", "/two", 30)).unwrap();
    store.insert(&row("c", "/one", 20)).unwrap();

    let all: Vec<String> = store
        .list(None)
        .unwrap()
        .into_iter()
        .map(|row| row.id)
        .collect();
    assert_eq!(all, ["b", "c", "a"]);
    let one: Vec<String> = store
        .list(Some("/one"))
        .unwrap()
        .into_iter()
        .map(|row| row.id)
        .collect();
    assert_eq!(one, ["c", "a"]);

    let updated = store
        .update("a", |row| {
            row.title = "Fix the build".into();
            row.title_user_set = true;
            row.state = AgentTaskState::Archived;
            row.web_enabled = false;
            row.settings = serde_json::json!({"mcp": ["github"]});
        })
        .unwrap()
        .unwrap();
    assert_eq!(store.get("a").unwrap().unwrap(), updated);
    let summary = updated.summary();
    assert_eq!(summary.title, "Fix the build");
    assert_eq!(summary.state, AgentTaskState::Archived);
    assert!(!summary.web_enabled && !summary.acp);
    assert!(store.update("missing", |_| {}).unwrap().is_none());
}

#[test]
fn the_index_outlives_its_store_and_refuses_a_newer_schema() {
    let directory = tempfile::tempdir().unwrap();
    TaskStore::open(directory.path())
        .unwrap()
        .insert(&row("a", "/one", 1))
        .unwrap();
    let reopened = TaskStore::open(directory.path()).unwrap();
    assert!(reopened.get("a").unwrap().is_some());
    drop(reopened);

    let connection = Connection::open(
        directory
            .path()
            .join(super::super::config::AGENT_TASK_INDEX_NAME),
    )
    .unwrap();
    connection
        .execute_batch("PRAGMA user_version = 99")
        .unwrap();
    drop(connection);
    assert!(TaskStore::open(directory.path()).is_err());
}

#[test]
fn two_processes_share_the_index() {
    let directory = tempfile::tempdir().unwrap();
    let desktop = TaskStore::open(directory.path()).unwrap();
    let acp = TaskStore::open(directory.path()).unwrap();
    let mut task = row("acp-task", "/one", 5);
    task.kind = TaskKind::Acp;
    acp.insert(&task).unwrap();
    let seen = desktop.get("acp-task").unwrap().unwrap();
    assert!(seen.summary().acp);
}

#[test]
fn a_task_without_a_file_opens_under_its_own_id_and_writes_on_its_first_message() {
    let (_directory, store) = store();
    let mut session = store.open_session("task-1", "/one").unwrap();
    assert_eq!(session.id(), "task-1");
    let path = store.session_path("task-1");
    assert!(!path.exists());

    session.append_model_change("maple", "glm-5-3");
    assert!(!path.exists(), "nothing is written before a conversation");
    session.append_message(user("hello"));
    assert!(path.exists());

    let reopened = store.open_session("task-1", "/elsewhere").unwrap();
    assert_eq!(reopened.id(), "task-1");
    assert_eq!(reopened.cwd(), "/one");
    let facts = SessionFacts::of(&reopened);
    assert_eq!(facts.message_count, 1);
    assert_eq!(facts.model.as_deref(), Some("glm-5-3"));
    assert!(facts.updated_ms.is_some());
}

#[test]
fn a_file_of_another_task_is_refused() {
    let (_directory, store) = store();
    let mut session = store.open_session("task-1", "/one").unwrap();
    session.append_message(user("hello"));
    fs::copy(store.session_path("task-1"), store.session_path("task-2")).unwrap();
    assert!(store.open_session("task-2", "/one").is_err());
}

#[test]
fn caches_follow_the_session_file() {
    let (_directory, store) = store();
    store.insert(&row("a", "/one", 1)).unwrap();
    let facts = SessionFacts {
        model: Some("kimi-k3".into()),
        message_count: 4,
        updated_ms: Some(50),
    };
    let row = store.refresh_caches("a", &facts).unwrap().unwrap();
    assert_eq!(row.model.as_deref(), Some("kimi-k3"));
    assert_eq!(row.message_count, 4);
    assert_eq!(row.updated_ms, 50);
    // An older file time never moves the task back in the list.
    let row = store
        .refresh_caches(
            "a",
            &SessionFacts {
                updated_ms: Some(10),
                ..facts
            },
        )
        .unwrap()
        .unwrap();
    assert_eq!(row.updated_ms, 50);
}

#[test]
fn deleting_removes_the_file_the_extra_files_and_the_row() {
    let (_directory, store) = store();
    store.insert(&row("a", "/one", 1)).unwrap();
    let mut session = store.open_session("a", "/one").unwrap();
    session.append_message(user("hello"));
    let mut cleared = false;
    assert!(
        store
            .delete("a", || {
                cleared = true;
                Ok(())
            })
            .unwrap()
    );
    assert!(cleared);
    assert!(!store.session_path("a").exists());
    assert!(store.get("a").unwrap().is_none());
    assert!(!store.delete("a", || Ok(())).unwrap());
}

#[test]
fn a_deletion_cut_short_is_finished_at_the_next_start() {
    let directory = tempfile::tempdir().unwrap();
    let store = TaskStore::open(directory.path()).unwrap();
    store.insert(&row("a", "/one", 1)).unwrap();
    let mut session = store.open_session("a", "/one").unwrap();
    session.append_message(user("hello"));
    // The process stopped after marking the row.
    store
        .connection()
        .execute("UPDATE tasks SET deleting = 1 WHERE id = 'a'", [])
        .unwrap();
    assert!(
        store.get("a").unwrap().is_none(),
        "a marked task is not listed"
    );
    drop(store);

    let store = TaskStore::open(directory.path()).unwrap();
    assert!(!store.session_path("a").exists());
    let rows: i64 = store
        .connection()
        .query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 0);
}

#[cfg(unix)]
#[test]
fn the_index_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let (directory, _store) = store();
    let mode = fs::metadata(
        directory
            .path()
            .join(super::super::config::AGENT_TASK_INDEX_NAME),
    )
    .unwrap()
    .permissions()
    .mode();
    assert_eq!(mode & 0o777, 0o600);
}
