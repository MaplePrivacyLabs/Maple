use std::collections::HashSet;

use serde_json::json;

use super::*;
use crate::store::StorePaths;

macro_rules! project_root {
    ($root:literal) => {
        if cfg!(windows) {
            concat!("C:", $root)
        } else {
            $root
        }
    };
}

fn fixture() -> (tempfile::TempDir, Store, AccountId, AccountId) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(StorePaths::new(directory.path())).unwrap();
    let alice = store.upsert_account("alice@example.test", 1).unwrap();
    let bob = store.upsert_account("bob@example.test", 1).unwrap();
    (directory, store, alice, bob)
}

fn new_task(number: u64, root: &str, now_ms: i64) -> NewTask {
    NewTask {
        uuid: format!("00000000-0000-7000-8000-{number:012x}"),
        project_root: root.to_owned(),
        kind: TaskKind::Desktop,
        title: format!("Task {number}"),
        provisional: false,
        model: Some("test-model".to_owned()),
        created_ms: now_ms,
    }
}

fn listed_task(
    store: &Store,
    account: AccountId,
    number: u64,
    root: &str,
    now_ms: i64,
) -> TaskRecord {
    let task = store
        .create_task(account, &new_task(number, root, now_ms))
        .unwrap();
    assert!(
        store
            .refresh_task_facts(
                account,
                &task.uuid,
                &TaskFacts {
                    message_count: 1,
                    updated_ms: Some(now_ms),
                    ..TaskFacts::default()
                },
            )
            .unwrap()
    );
    store.task(account, &task.uuid).unwrap().unwrap()
}

fn query(section: TaskSection, limit: u16) -> TaskQuery {
    TaskQuery {
        section,
        project: None,
        pins: PinFilter::Exclude,
        cursor: None,
        limit,
    }
}

#[test]
fn foreign_account_cannot_resolve_or_mutate_task_metadata() {
    let (_directory, store, alice, bob) = fixture();
    let task = listed_task(&store, alice, 1, project_root!("/projects/alice"), 10);
    let foreign_patch = TaskPatch {
        title: Some("foreign title".to_owned()),
        project_root: Some(project_root!("/projects/injected").to_owned()),
        state: Some(TaskState::Archived),
        settings: Some(json!({"computerUse": true})),
        ..TaskPatch::default()
    };

    assert_eq!(store.task(bob, &task.uuid).unwrap(), None);
    assert!(
        store
            .get_tasks(bob, std::slice::from_ref(&task.uuid))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.update_task(bob, &task.uuid, &foreign_patch).unwrap(),
        None
    );
    assert!(!store.set_task_pinned(bob, &task.uuid, true, 20).unwrap());
    assert!(!store.mark_task_deleted(bob, &task.uuid, 20).unwrap());
    assert!(
        !store
            .refresh_task_facts(bob, &task.uuid, &TaskFacts::default())
            .unwrap()
    );
    assert_eq!(
        store
            .project(bob, project_root!("/projects/injected"))
            .unwrap(),
        None
    );

    let mut foreign_project = query(TaskSection::Recent, 10);
    foreign_project.project = Some(task.project);
    assert!(
        store
            .list_tasks(bob, &foreign_project)
            .unwrap()
            .tasks
            .is_empty()
    );
    assert_eq!(store.task(alice, &task.uuid).unwrap(), Some(task.clone()));

    let updated = store
        .update_task(
            alice,
            &task.uuid,
            &TaskPatch {
                title: Some("My renamed task".to_owned()),
                title_user_set: Some(true),
                project_root: Some(project_root!("/projects/moved").to_owned()),
                state: Some(TaskState::Settled),
                web_enabled: Some(false),
                settings: Some(json!({"mcpServers": {"example": false}})),
                ..TaskPatch::default()
            },
        )
        .unwrap()
        .unwrap();
    assert_eq!(updated.title, "My renamed task");
    assert!(updated.title_user_set);
    assert_eq!(updated.state, TaskState::Settled);
    assert!(!updated.web_enabled);
    assert_eq!(updated.settings, json!({"mcpServers": {"example": false}}));
    assert_ne!(updated.project, task.project);
    assert_eq!(updated.created_ms, task.created_ms);
    assert_eq!(updated.facts, task.facts);
}

#[test]
fn project_preferences_and_account_settings_are_isolated_by_account() {
    let (_directory, store, alice, bob) = fixture();
    let root = project_root!("/projects/shared-directory");
    let alice_task = listed_task(&store, alice, 1, root, 10);
    let bob_task = listed_task(&store, bob, 2, root, 11);
    assert_ne!(alice_task.project, bob_task.project);

    store.remember_project(alice, root, 2, 12).unwrap();
    store
        .set_project_display_name(alice, root, Some("Alice's project"), 12)
        .unwrap();
    store
        .set_project_trust(alice, root, Some(true), 12)
        .unwrap();
    store
        .set_project_mcp_defaults(alice, root, &json!({"server": false}), 12)
        .unwrap();
    store
        .set_account_setting(
            alice,
            "agent.defaults",
            &json!({"model": "alice-model"}),
            12,
        )
        .unwrap();
    store
        .set_account_setting(bob, "agent.defaults", &json!({"model": "bob-model"}), 12)
        .unwrap();
    store
        .set_app_setting("ui", &json!({"theme": "dark"}), 12)
        .unwrap();

    let alice_project = store.project(alice, root).unwrap().unwrap();
    let bob_project = store.project(bob, root).unwrap().unwrap();
    assert_eq!(
        alice_project.display_name.as_deref(),
        Some("Alice's project")
    );
    assert_eq!(alice_project.trusted, Some(true));
    assert_eq!(alice_project.mcp_defaults, json!({"server": false}));
    assert_eq!(bob_project.display_name, None);
    assert_eq!(bob_project.trusted, None);
    assert_eq!(bob_project.mcp_defaults, json!({}));
    assert_eq!(
        store.account_setting(alice, "agent.defaults").unwrap(),
        Some(json!({"model": "alice-model"}))
    );
    assert_eq!(
        store.account_setting(bob, "agent.defaults").unwrap(),
        Some(json!({"model": "bob-model"}))
    );
    assert_eq!(
        store.app_setting("ui").unwrap(),
        Some(json!({"theme": "dark"}))
    );

    assert!(store.remove_project(alice, root, 20).unwrap());
    assert!(store.recent_projects(alice, 10).unwrap().is_empty());
    assert_eq!(store.recent_projects(bob, 10).unwrap(), vec![bob_project]);
    assert!(store.task(alice, &alice_task.uuid).unwrap().is_some());
    assert_eq!(
        store.project(alice, root).unwrap().unwrap().trusted,
        Some(true)
    );
    store.remember_project(alice, root, 0, 30).unwrap();
    assert_eq!(store.recent_projects(alice, 10).unwrap().len(), 1);
}

#[test]
fn explicit_project_selection_prepends_new_roots_and_preserves_existing_metadata() {
    let (_directory, store, alice, bob) = fixture();
    let first = project_root!("/projects/a");
    let second = project_root!("/projects/z");
    let third = project_root!("/projects/m");
    for (index, root) in [first, second, third].into_iter().enumerate() {
        store
            .register_explicit_project(alice, root, index as i64 + 1)
            .unwrap();
    }
    store
        .set_project_display_name(alice, first, Some("Custom name"), 4)
        .unwrap();
    store
        .set_project_trust(alice, first, Some(false), 4)
        .unwrap();
    store
        .set_project_mcp_defaults(alice, first, &json!({"tools": false}), 4)
        .unwrap();
    let before = store.project(alice, first).unwrap().unwrap();
    store.register_explicit_project(alice, first, 50).unwrap();
    assert_eq!(store.project(alice, first).unwrap().unwrap(), before);
    assert_eq!(
        store
            .recent_projects(alice, 10)
            .unwrap()
            .into_iter()
            .map(|row| row.root)
            .collect::<Vec<_>>(),
        [third, second, first]
    );

    store.remove_project(alice, first, 60).unwrap();
    store.register_explicit_project(alice, first, 70).unwrap();
    let restored = store.project(alice, first).unwrap().unwrap();
    assert_eq!(restored.position, Some(-3));
    assert_eq!(restored.last_used_ms, 70);
    assert_eq!(restored.removed_ms, None);
    assert_eq!(restored.display_name, before.display_name);
    assert_eq!(restored.trusted, before.trusted);
    assert_eq!(restored.mcp_defaults, before.mcp_defaults);
    assert_eq!(store.recent_projects(alice, 10).unwrap()[0].root, first);
    assert!(store.project(bob, first).unwrap().is_none());

    let details = store.read(|tx| {
        Ok(tx.prepare("EXPLAIN QUERY PLAN SELECT position FROM projects INDEXED BY projects_navigation
            WHERE account_id=?1 AND removed_ms IS NULL AND (position IS NULL)=0
            ORDER BY coalesce(position,0),root,id LIMIT 1")?
            .query_map([alice.0], |row| row.get::<_, String>(3))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }).unwrap();
    assert!(
        details
            .iter()
            .any(|line| line.contains("projects_navigation")),
        "{details:?}"
    );
    assert!(
        !details.iter().any(|line| line.contains("TEMP B-TREE")),
        "{details:?}"
    );
}

#[test]
fn concurrent_project_prepend_allocates_distinct_ranks_and_pages_signed_positions() {
    let (_directory, store, alice, _bob) = fixture();
    let roots = [project_root!("/projects/a"), project_root!("/projects/z")];
    let store_ref = &store;
    std::thread::scope(|scope| {
        let writes = roots
            .map(|root| scope.spawn(move || store_ref.register_explicit_project(alice, root, 1)));
        for write in writes {
            write.join().unwrap().unwrap();
        }
    });
    let mut cursor = None;
    let mut actual = Vec::new();
    let mut ranks = Vec::new();
    loop {
        let page = store
            .navigation_projects(
                alice,
                cursor.as_deref(),
                1,
                crate::store::PageTransport::Local,
            )
            .unwrap();
        for project in page.projects {
            let row = store.project(alice, &project.root).unwrap().unwrap();
            ranks.push(row.position.unwrap());
            actual.push(project.root);
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    actual.sort();
    assert_eq!(actual, roots);
    assert_eq!(ranks, [-1, 0]);
}

#[test]
fn more_than_two_hundred_pins_page_without_losing_timestamp_ties() {
    let (_directory, store, alice, bob) = fixture();
    let mut expected = Vec::new();
    for number in 1..=205 {
        let task = listed_task(&store, alice, number, project_root!("/projects/busy"), 100);
        store.set_task_pinned(alice, &task.uuid, true, 101).unwrap();
        expected.push(task.uuid);
    }
    // A project with an older task must survive even when it is absent from
    // the first global task page. Project groups have their own keyset.
    let older = listed_task(&store, alice, 206, project_root!("/projects/older"), 1);
    let foreign = listed_task(&store, bob, 207, project_root!("/projects/foreign"), 200);
    store
        .set_task_pinned(bob, &foreign.uuid, true, 201)
        .unwrap();

    let mut request = query(TaskSection::Pinned, 200);
    let first = store.list_tasks(alice, &request).unwrap();
    assert_eq!(first.tasks.len(), 200);
    assert!(first.next.is_some());
    request.cursor = first.next;
    let second = store.list_tasks(alice, &request).unwrap();
    assert_eq!(second.tasks.len(), 5);
    assert_eq!(second.next, None);
    let actual: Vec<_> = first
        .tasks
        .iter()
        .chain(&second.tasks)
        .map(|task| task.uuid.clone())
        .collect();
    expected.reverse();
    assert_eq!(actual, expected);
    assert_eq!(actual.iter().collect::<HashSet<_>>().len(), 205);

    let mut resolved = HashSet::new();
    for batch in actual.chunks(200) {
        for task in store.get_tasks(alice, batch).unwrap() {
            assert!(resolved.insert(task.uuid));
        }
    }
    assert_eq!(resolved, actual.into_iter().collect());
    assert_eq!(
        store.get_tasks(alice, &vec![older.uuid.clone(); 201]),
        Err(StoreError::InvalidInput)
    );

    let recent = store
        .list_tasks(alice, &query(TaskSection::Recent, 200))
        .unwrap();
    assert!(!recent.tasks.iter().any(|task| task.uuid == older.uuid));
    let first_group = store.project_groups(alice, None, 1).unwrap();
    let second_group = store
        .project_groups(alice, first_group.next.as_ref(), 1)
        .unwrap();
    assert_eq!(
        first_group.projects[0].root,
        project_root!("/projects/busy")
    );
    assert_eq!(
        second_group.projects[0].root,
        project_root!("/projects/older")
    );
    assert_eq!(second_group.next, None);
    let mut group_tasks = query(TaskSection::State(TaskState::Active), 10);
    group_tasks.project = Some(second_group.projects[0].key);
    assert_eq!(
        store.list_tasks(alice, &group_tasks).unwrap().tasks,
        vec![older]
    );
}

#[test]
fn archived_pins_leave_pinned_section_but_remain_in_archive() {
    let (_directory, store, alice, _bob) = fixture();
    let active = listed_task(&store, alice, 1, project_root!("/projects/example"), 10);
    let archived = listed_task(&store, alice, 2, project_root!("/projects/example"), 20);
    for task in [&active, &archived] {
        store.set_task_pinned(alice, &task.uuid, true, 30).unwrap();
    }
    // Repinning preserves its original pin time.
    store
        .set_task_pinned(alice, &active.uuid, true, 40)
        .unwrap();
    assert_eq!(
        store.task(alice, &active.uuid).unwrap().unwrap().pinned_ms,
        Some(30)
    );
    store
        .update_task(
            alice,
            &archived.uuid,
            &TaskPatch {
                state: Some(TaskState::Archived),
                ..TaskPatch::default()
            },
        )
        .unwrap();

    let pinned = store
        .list_tasks(alice, &query(TaskSection::Pinned, 10))
        .unwrap();
    assert_eq!(
        pinned
            .tasks
            .iter()
            .map(|task| task.uuid.as_str())
            .collect::<Vec<_>>(),
        vec![active.uuid.as_str()]
    );
    let archive = store
        .list_tasks(alice, &query(TaskSection::State(TaskState::Archived), 10))
        .unwrap();
    assert_eq!(archive.tasks.len(), 1);
    assert_eq!(archive.tasks[0].uuid, archived.uuid);
    assert_eq!(archive.tasks[0].pinned_ms, Some(30));
    assert_eq!(
        store
            .project_groups(alice, None, 10)
            .unwrap()
            .archived_count,
        1
    );
    assert!(
        store
            .list_tasks(alice, &query(TaskSection::State(TaskState::Active), 10))
            .unwrap()
            .tasks
            .is_empty()
    );
    store
        .set_task_pinned(alice, &active.uuid, false, 50)
        .unwrap();
    assert_eq!(
        store
            .list_tasks(alice, &query(TaskSection::State(TaskState::Active), 10))
            .unwrap()
            .tasks
            .len(),
        1
    );
}

#[test]
fn recent_pages_include_empty_committed_tasks_but_hide_provisional_and_deleted() {
    let (_directory, store, alice, _bob) = fixture();
    let empty = store
        .create_task(alice, &new_task(1, project_root!("/projects/example"), 10))
        .unwrap();
    let mut provisional = new_task(2, project_root!("/projects/provisional"), 20);
    provisional.kind = TaskKind::Acp;
    provisional.provisional = true;
    store.create_task(alice, &provisional).unwrap();
    let deleted = listed_task(&store, alice, 3, project_root!("/projects/example"), 30);
    store.mark_task_deleted(alice, &deleted.uuid, 40).unwrap();

    assert_eq!(
        store
            .list_tasks(alice, &query(TaskSection::Recent, 10))
            .unwrap()
            .tasks,
        vec![empty]
    );
    assert!(
        store
            .list_tasks(alice, &query(TaskSection::State(TaskState::Active), 10))
            .unwrap()
            .tasks
            .is_empty()
    );
    assert!(store.recent_projects(alice, 10).unwrap().is_empty());
    assert!(
        store
            .project_groups(alice, None, 10)
            .unwrap()
            .projects
            .is_empty()
    );
}

#[test]
fn pending_account_deletion_survives_reopen_without_authorizing_collection() {
    let (directory, store, alice, bob) = fixture();
    let live = listed_task(&store, alice, 1, project_root!("/projects/alice"), 10);
    let foreign = listed_task(&store, bob, 2, project_root!("/projects/bob"), 10);
    store
        .begin_account_deletion(alice, "request-1", 20)
        .unwrap();
    // Retrying the same attempt must not change its durable start time.
    store
        .begin_account_deletion(alice, "request-1", 99)
        .unwrap();
    drop(store);
    let store = Store::open(StorePaths::new(directory.path())).unwrap();

    assert_eq!(
        store.account_deletion(alice).unwrap(),
        Some(AccountDeletion::Pending {
            generation: "request-1".to_owned(),
            started_ms: 20,
        })
    );
    assert!(store.confirmed_account_deletions(10).unwrap().is_empty());
    assert_eq!(store.task(alice, &live.uuid).unwrap(), Some(live));
    assert_eq!(store.task(bob, &foreign.uuid).unwrap(), Some(foreign));
    assert_eq!(
        store.create_task(alice, &new_task(3, project_root!("/projects/alice"), 30)),
        Err(StoreError::Conflict)
    );
    assert_eq!(
        store.set_account_setting(alice, "agent.defaults", &json!({}), 30),
        Err(StoreError::Conflict)
    );
    assert_eq!(
        store.sweep_provisional_tasks(alice, 30, 10),
        Err(StoreError::Conflict)
    );
}

#[test]
fn abort_preserves_independent_tombstones_and_rejects_stale_generations() {
    let (_directory, store, alice, _bob) = fixture();
    let live = listed_task(&store, alice, 1, project_root!("/projects/alice"), 10);
    let already_deleted = listed_task(&store, alice, 2, project_root!("/projects/alice"), 11);
    store
        .mark_task_deleted(alice, &already_deleted.uuid, 12)
        .unwrap();
    store
        .begin_account_deletion(alice, "request-1", 20)
        .unwrap();

    assert_eq!(
        store.begin_account_deletion(alice, "request-2", 21),
        Err(StoreError::Conflict)
    );
    assert_eq!(
        store.confirm_account_deletion(alice, "request-2"),
        Err(StoreError::Conflict)
    );
    assert_eq!(
        store.abort_account_deletion(alice, "request-2"),
        Err(StoreError::Conflict)
    );
    store.abort_account_deletion(alice, "request-1").unwrap();
    assert_eq!(store.account_deletion(alice).unwrap(), None);
    assert!(store.task(alice, &live.uuid).unwrap().is_some());
    assert_eq!(store.task(alice, &already_deleted.uuid).unwrap(), None);

    store
        .begin_account_deletion(alice, "request-2", 30)
        .unwrap();
    assert_eq!(
        store.confirm_account_deletion(alice, "request-1"),
        Err(StoreError::Conflict)
    );
    assert_eq!(
        store.abort_account_deletion(alice, "request-1"),
        Err(StoreError::Conflict)
    );
    assert_eq!(
        store.account_deletion(alice).unwrap(),
        Some(AccountDeletion::Pending {
            generation: "request-2".to_owned(),
            started_ms: 30,
        })
    );
}

#[test]
fn confirmation_tombstones_only_its_account_and_cannot_be_aborted() {
    let (_directory, store, alice, bob) = fixture();
    let live = listed_task(&store, alice, 1, project_root!("/projects/shared"), 10);
    let already_deleted = listed_task(&store, alice, 2, project_root!("/projects/shared"), 11);
    let foreign = listed_task(&store, bob, 3, project_root!("/projects/shared"), 12);
    store
        .mark_task_deleted(alice, &already_deleted.uuid, 13)
        .unwrap();
    store
        .begin_account_deletion(alice, "request-1", 20)
        .unwrap();
    store.confirm_account_deletion(alice, "request-1").unwrap();
    store.confirm_account_deletion(alice, "request-1").unwrap();

    assert_eq!(store.confirmed_account_deletions(10).unwrap(), vec![alice]);
    assert_eq!(
        store.account_deletion(alice).unwrap(),
        Some(AccountDeletion::Confirmed {
            generation: "request-1".to_owned(),
            started_ms: 20,
        })
    );
    assert_eq!(store.task(alice, &live.uuid).unwrap(), None);
    assert_eq!(store.task(alice, &already_deleted.uuid).unwrap(), None);
    assert_eq!(store.task(bob, &foreign.uuid).unwrap(), Some(foreign));
    assert_eq!(
        store.abort_account_deletion(alice, "request-1"),
        Err(StoreError::Conflict)
    );
    assert_eq!(
        store.begin_account_deletion(alice, "request-2", 30),
        Err(StoreError::Conflict)
    );
    let deleted_at: i64 = store
        .read(|tx| {
            Ok(tx.query_row(
                "SELECT deleted_ms FROM tasks WHERE id=?1",
                [already_deleted.key.0],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(deleted_at, 13);
}

#[test]
fn provisional_sweep_preserves_committed_listed_referenced_and_recent_tasks() {
    let (_directory, store, alice, bob) = fixture();
    let mut tasks = Vec::new();
    for number in 1..=7 {
        let mut input = new_task(
            number,
            &format!("{}/task-{number}", project_root!("/projects")),
            if number == 6 { 100 } else { 1 },
        );
        input.provisional = number != 2;
        tasks.push(store.create_task(alice, &input).unwrap());
    }
    store
        .refresh_task_facts(
            alice,
            &tasks[2].uuid,
            &TaskFacts {
                message_count: 1,
                ..TaskFacts::default()
            },
        )
        .unwrap();
    store
        .update_task(
            alice,
            &tasks[6].uuid,
            &TaskPatch {
                title: Some("A renamed draft survives".into()),
                title_user_set: Some(true),
                ..TaskPatch::default()
            },
        )
        .unwrap();
    // Model the source and attachment references the session/attachment APIs write;
    // even a stale zero cache must not make those tasks safe to sweep.
    store
        .write(Durability::Durable, |tx| {
            tx.execute(
                "INSERT INTO entries(task_id,seq,entry_id,kind,created_ms,bytes,payload)
            VALUES (?1,1,'00000001','message',1,2,x'7b7d')",
                [tasks[3].key.0],
            )?;
            tx.execute(
                "INSERT INTO file_attachments(public_id,account_id,task_id,operation_id,name,mime,size,sha256,state,bound,created_ms,updated_ms)
                VALUES ('00000000000000000000000000000001',?1,?2,'pending-test','file.txt','text/plain',0,?3,'uploading',0,1,1)",
                params![alice.0, tasks[4].key.0, "0".repeat(64)],
            )?;
            Ok(())
        })
        .unwrap();
    let mut foreign_input = new_task(8, project_root!("/projects/foreign"), 1);
    foreign_input.provisional = true;
    let foreign = store.create_task(bob, &foreign_input).unwrap();
    store
        .remember_project(alice, project_root!("/projects/remembered"), 0, 1)
        .unwrap();

    assert_eq!(store.sweep_provisional_tasks(alice, 50, 1).unwrap(), 1);
    assert_eq!(store.task(alice, &tasks[0].uuid).unwrap(), None);
    assert_eq!(
        store
            .project(alice, project_root!("/projects/task-1"))
            .unwrap(),
        None
    );
    for task in &tasks[1..] {
        assert!(
            store.task(alice, &task.uuid).unwrap().is_some(),
            "{} must survive",
            task.uuid
        );
    }
    assert!(store.task(bob, &foreign.uuid).unwrap().is_some());
    assert!(
        store
            .project(alice, project_root!("/projects/remembered"))
            .unwrap()
            .is_some()
    );
    assert_eq!(store.sweep_provisional_tasks(alice, 50, 200).unwrap(), 0);
}

#[test]
fn closing_a_surface_rechecks_draft_ownership_and_touch_state_in_the_hide_transaction() {
    let (_directory, store, alice, bob) = fixture();
    let mut draft = new_task(91, project_root!("/projects/draft"), 1);
    draft.provisional = true;
    let renamed = store.create_task(alice, &draft).unwrap();
    // A surface inspected this as unused, then another client renamed it.
    let before = store.task(alice, &renamed.uuid).unwrap().unwrap();
    assert!(before.provisional && !before.title_user_set && before.facts.message_count == 0);
    store
        .update_task(
            alice,
            &renamed.uuid,
            &TaskPatch {
                title: Some("Keep this task".into()),
                title_user_set: Some(true),
                ..TaskPatch::default()
            },
        )
        .unwrap();
    assert!(
        !store
            .mark_untouched_provisional_deleted(alice, &renamed.uuid, 2)
            .unwrap()
    );
    assert!(store.task(alice, &renamed.uuid).unwrap().is_some());

    draft.uuid = new_task(92, project_root!("/projects/draft"), 1).uuid;
    let prompted = store.create_task(alice, &draft).unwrap();
    store
        .refresh_task_facts(
            alice,
            &prompted.uuid,
            &TaskFacts {
                message_count: 1,
                ..TaskFacts::default()
            },
        )
        .unwrap();
    assert!(
        !store
            .mark_untouched_provisional_deleted(alice, &prompted.uuid, 2)
            .unwrap()
    );
    assert!(store.task(alice, &prompted.uuid).unwrap().is_some());

    draft.uuid = new_task(93, project_root!("/projects/draft"), 1).uuid;
    let untouched = store.create_task(alice, &draft).unwrap();
    assert!(
        !store
            .mark_untouched_provisional_deleted(bob, &untouched.uuid, 2)
            .unwrap()
    );
    assert!(
        store
            .mark_untouched_provisional_deleted(alice, &untouched.uuid, 2)
            .unwrap()
    );
    assert!(store.task(alice, &untouched.uuid).unwrap().is_none());
}

#[test]
fn archive_candidate_pages_bound_scanned_metadata_and_continue_past_ineligible_rows() {
    let (_directory, store, alice, bob) = fixture();
    let root = project_root!("/projects/archive");
    let mut created = Vec::new();
    for number in 101..=105 {
        created.push(
            store
                .create_task(alice, &new_task(number, root, (106 - number) as i64))
                .unwrap(),
        );
    }
    store
        .update_task(
            alice,
            &created[0].uuid,
            &TaskPatch {
                state: Some(TaskState::Archived),
                ..TaskPatch::default()
            },
        )
        .unwrap();
    store
        .update_task(
            alice,
            &created[1].uuid,
            &TaskPatch {
                provisional: Some(true),
                ..TaskPatch::default()
            },
        )
        .unwrap();
    store
        .mark_task_deleted(alice, &created[2].uuid, 10)
        .unwrap();
    store
        .update_task(
            alice,
            &created[4].uuid,
            &TaskPatch {
                state: Some(TaskState::Settled),
                ..TaskPatch::default()
            },
        )
        .unwrap();
    let foreign = store.create_task(bob, &new_task(106, root, 100)).unwrap();
    let elsewhere = store
        .create_task(
            alice,
            &new_task(107, project_root!("/projects/elsewhere"), 100),
        )
        .unwrap();

    let first = store
        .archive_project_candidates(alice, root, None, 1)
        .unwrap();
    assert!(first.task_ids.is_empty());
    assert!(
        first.next.is_some(),
        "an archived metadata row cannot terminate the scan"
    );
    let mut cursor = first.next;
    let mut ids = Vec::new();
    while let Some(position) = cursor {
        let page = store
            .archive_project_candidates(alice, root, Some(&position), 1)
            .unwrap();
        ids.extend(page.task_ids);
        cursor = page.next;
    }
    assert_eq!(ids, vec![created[3].uuid.clone(), created[4].uuid.clone()]);
    assert!(!ids.contains(&foreign.uuid));
    assert!(!ids.contains(&elsewhere.uuid));
    assert_eq!(
        store
            .archive_project_candidates(bob, root, None, 200)
            .unwrap()
            .task_ids,
        vec![foreign.uuid]
    );
    let plan = store
        .read(|tx| {
            let mut statement = tx.prepare(
                "EXPLAIN QUERY PLAN SELECT id,uuid,updated_ms,provisional,state,deleted_ms IS NULL
            FROM tasks INDEXED BY tasks_project WHERE project_id=?1 AND account_id=?2
            AND (updated_ms,id)<(?3,?4) ORDER BY updated_ms DESC,id DESC LIMIT 200",
            )?;
            Ok(statement
                .query_map(params![created[0].project.0, alice.0, 100, 999], |row| {
                    row.get::<_, String>(3)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
                .join(" "))
        })
        .unwrap();
    assert!(plan.contains("USING INDEX tasks_project"), "{plan}");
    assert!(!plan.contains("TEMP B-TREE"), "{plan}");
}

#[test]
fn search_uses_unicode_lowercase_and_authorized_project_matches() {
    let (_directory, store, alice, bob) = fixture();
    let accented = listed_task(&store, alice, 1, project_root!("/projects/unicode"), 10);
    store
        .update_task(
            alice,
            &accented.uuid,
            &TaskPatch {
                title: Some("CAFÉ: Δοκιμή".to_owned()),
                ..TaskPatch::default()
            },
        )
        .unwrap();
    let by_root = listed_task(&store, alice, 2, project_root!("/projects/matched"), 20);
    let foreign = listed_task(&store, bob, 3, project_root!("/projects/matched"), 30);
    let empty = store
        .create_task(alice, &new_task(4, project_root!("/projects/matched"), 40))
        .unwrap();
    let mut provisional = new_task(5, project_root!("/projects/matched"), 50);
    provisional.provisional = true;
    let provisional = store.create_task(alice, &provisional).unwrap();
    store
        .refresh_task_facts(
            alice,
            &provisional.uuid,
            &TaskFacts {
                message_count: 1,
                ..TaskFacts::default()
            },
        )
        .unwrap();

    let found = store.search_tasks(alice, "café", &[], 100).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].uuid, accented.uuid);
    assert_eq!(
        store.search_tasks(alice, "ΔΟΚΙΜΉ", &[], 100).unwrap()[0].uuid,
        accented.uuid
    );
    let found = store
        .search_tasks(
            alice,
            "no title matches",
            &[project_root!("/projects/matched").to_owned()],
            100,
        )
        .unwrap();
    assert_eq!(
        found
            .iter()
            .map(|task| task.uuid.as_str())
            .collect::<Vec<_>>(),
        vec![by_root.uuid.as_str()]
    );
    assert!(!found.iter().any(|task| task.uuid == foreign.uuid
        || task.uuid == empty.uuid
        || task.uuid == provisional.uuid));
    store.mark_task_deleted(alice, &accented.uuid, 60).unwrap();
    assert!(
        store
            .search_tasks(alice, "café", &[], 100)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn dictionary_normalizes_input_and_pages_every_word_once() {
    let (_directory, store, _alice, _bob) = fixture();
    assert!(store.add_dictionary_word("  'café'  ").unwrap());
    assert!(!store.add_dictionary_word("café").unwrap());
    for word in ["delta", "alpha", "beta"] {
        assert!(store.add_dictionary_word(word).unwrap());
    }
    let first = store.dictionary_words(None, 2).unwrap();
    assert_eq!(first.words, ["alpha", "beta"]);
    assert_eq!(first.next.as_deref(), Some("beta"));
    let second = store.dictionary_words(first.next.as_deref(), 2).unwrap();
    assert_eq!(second.words, ["café", "delta"]);
    assert_eq!(second.next, None);
    assert!(store.remove_dictionary_word("café").unwrap());
    assert!(!store.remove_dictionary_word("café").unwrap());
    assert_eq!(
        store.dictionary_words(None, 10).unwrap().words,
        ["alpha", "beta", "delta"]
    );
    assert_eq!(
        store.add_dictionary_word("  ''  "),
        Err(StoreError::InvalidInput)
    );
    assert_eq!(
        store.add_dictionary_word("two\nlines"),
        Err(StoreError::InvalidInput)
    );
    assert_eq!(
        store.dictionary_words(None, 0),
        Err(StoreError::InvalidInput)
    );
}
