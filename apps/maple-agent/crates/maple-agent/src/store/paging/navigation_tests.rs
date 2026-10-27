use super::*;
use crate::store::{NewTask, StorePaths, TaskFacts, TaskKind, TaskPatch};

fn root(name: &str) -> String {
    if cfg!(windows) {
        format!("C:/projects/{name}")
    } else {
        format!("/projects/{name}")
    }
}

fn fixture() -> (tempfile::TempDir, Store, AccountId, AccountId) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(StorePaths::new(directory.path())).unwrap();
    let alice = store
        .upsert_account("navigation-alice@example.test", 1)
        .unwrap();
    let bob = store
        .upsert_account("navigation-bob@example.test", 1)
        .unwrap();
    (directory, store, alice, bob)
}

fn task(store: &Store, account: AccountId, number: u64, project_root: &str, title: &str) -> String {
    let uuid = format!("10000000-0000-7000-8000-{number:012x}");
    store
        .create_task(
            account,
            &NewTask {
                uuid: uuid.clone(),
                project_root: project_root.to_owned(),
                kind: TaskKind::Desktop,
                title: title.to_owned(),
                provisional: false,
                model: None,
                created_ms: number as i64,
            },
        )
        .unwrap();
    store
        .refresh_task_facts(
            account,
            &uuid,
            &TaskFacts {
                message_count: 1,
                ..TaskFacts::default()
            },
        )
        .unwrap();
    uuid
}

fn query(section: TaskSection, limit: u16) -> NavigationTaskQuery {
    NavigationTaskQuery {
        manual: None,
        section,
        project: None,
        pins: PinFilter::Ignore,
        cursor: None,
        limit,
    }
}

#[test]
fn pins_page_beyond_two_hundred_and_bind_account_and_filter() {
    let (_dir, store, alice, bob) = fixture();
    for number in 1..=205 {
        let id = task(&store, alice, number, &root("pins"), "Pinned");
        store.set_task_pinned(alice, &id, true, 1).unwrap();
    }
    let mut request = query(TaskSection::Pinned, 200);
    let first = store
        .list_task_summaries(alice, &request, PageTransport::Local)
        .unwrap();
    assert_eq!(first.tasks.len(), 200);
    request.cursor = first.next_cursor;
    let second = store
        .list_task_summaries(alice, &request, PageTransport::Local)
        .unwrap();
    assert_eq!(second.tasks.len(), 5);
    assert!(second.next_cursor.is_none());
    let all: HashSet<_> = first
        .tasks
        .iter()
        .chain(second.tasks.iter())
        .map(|row| row.uuid.as_str())
        .collect();
    assert_eq!(all.len(), 205);
    assert!(matches!(
        store.list_task_summaries(bob, &request, PageTransport::Local),
        Err(StoreError::InvalidInput)
    ));
    request.pins = PinFilter::Exclude;
    assert!(matches!(
        store.list_task_summaries(alice, &request, PageTransport::Local),
        Err(StoreError::InvalidInput)
    ));
}

#[test]
fn remote_navigation_and_get_tasks_keep_a_byte_continuation() {
    let (_dir, store, alice, bob) = fixture();
    // JSON escaping makes a root much larger than its in-database UTF-8 size.
    let large_root = root(&"\u{1}".repeat(15_000));
    let mut ids = Vec::new();
    for number in 1..=10 {
        ids.push(task(
            &store,
            alice,
            number,
            &large_root,
            &"\u{2}".repeat(4096),
        ));
    }
    let mut request = query(TaskSection::State(TaskState::Active), 200);
    let first = store
        .list_task_summaries(alice, &request, PageTransport::Remote)
        .unwrap();
    assert!(!first.tasks.is_empty() && first.tasks.len() < ids.len());
    assert!(serialized_len(&first).unwrap() <= PageTransport::Remote.caps().max_reply_bytes);
    request.cursor = first.next_cursor;
    let next = store
        .list_task_summaries(alice, &request, PageTransport::Remote)
        .unwrap();
    assert!(
        next.tasks
            .iter()
            .all(|row| !first.tasks.iter().any(|previous| previous.uuid == row.uuid))
    );
    let foreign = task(&store, bob, 999, &root("foreign"), "foreign");
    ids.insert(0, foreign);
    let mut pending = ids;
    let mut loaded = Vec::new();
    while !pending.is_empty() {
        let reply = store
            .get_task_summaries(alice, &pending, PageTransport::Remote)
            .unwrap();
        assert!(serialized_len(&reply).unwrap() <= PageTransport::Remote.caps().max_reply_bytes);
        assert!(reply.remaining_ids.len() < pending.len());
        loaded.extend(reply.tasks.into_iter().map(|row| row.uuid));
        pending = reply.remaining_ids;
    }
    assert_eq!(loaded.len(), 10);
    assert_eq!(loaded.iter().collect::<HashSet<_>>().len(), 10);
}

#[test]
fn navigation_omits_large_settings_and_context_and_uses_selected_projects() {
    let (_dir, store, alice, _bob) = fixture();
    let wanted = root("wanted");
    let id = task(&store, alice, 1, &wanted, "visible");
    store
        .set_project_display_name(alice, &wanted, Some("Custom project"), 1)
        .unwrap();
    let private = "secret-context-and-settings".repeat(8000);
    store
        .update_task(
            alice,
            &id,
            &TaskPatch {
                settings: Some(json!({"secret":private})),
                ..TaskPatch::default()
            },
        )
        .unwrap();
    store
        .refresh_task_facts(
            alice,
            &id,
            &TaskFacts {
                message_count: 1,
                context_base: Some(json!({"secret":private})),
                ..TaskFacts::default()
            },
        )
        .unwrap();
    for index in 0..4 {
        store
            .remember_project(alice, &root(&format!("recent{index}")), index, 10)
            .unwrap();
    }
    let page = store
        .list_task_summaries(alice, &query(TaskSection::Recent, 50), PageTransport::Local)
        .unwrap();
    assert_eq!(page.tasks[0].project_root, wanted);
    assert_eq!(
        page.tasks[0].project_display_name.as_deref(),
        Some("Custom project")
    );
    let encoded = serde_json::to_string(&page).unwrap();
    assert!(!encoded.contains("secret-context"));
    assert!(!encoded.contains("settings"));
    assert!(!encoded.contains("context_base"));
}

#[test]
fn search_is_unicode_folded_and_cursor_binds_search_terms() {
    let (_dir, store, alice, _bob) = fixture();
    let project = root("unicode");
    task(&store, alice, 1, &project, "ÄPFEL one");
    task(&store, alice, 2, &project, "äpfel two");
    task(&store, alice, 3, &root("other"), "unrelated");
    let mut request = NavigationSearchQuery {
        manual: None,
        project: None,
        text: "Äpfel".to_owned(),
        project_roots: Vec::new(),
        cursor: None,
        limit: 1,
    };
    let first = store
        .search_task_summaries(alice, &request, PageTransport::Local)
        .unwrap();
    assert_eq!(first.tasks.len(), 1);
    request.cursor = first.next_cursor;
    let second = store
        .search_task_summaries(alice, &request, PageTransport::Local)
        .unwrap();
    assert_eq!(second.tasks.len(), 1);
    assert_ne!(first.tasks[0].uuid, second.tasks[0].uuid);
    let scoped = NavigationSearchQuery {
        manual: None,
        text: "äpfel".to_owned(),
        project: Some(root("other")),
        project_roots: vec![project.clone()],
        cursor: None,
        limit: 50,
    };
    assert!(
        store
            .search_task_summaries(alice, &scoped, PageTransport::Local)
            .unwrap()
            .tasks
            .is_empty()
    );
    request.project = Some(project);
    assert!(matches!(
        store.search_task_summaries(alice, &request, PageTransport::Local),
        Err(StoreError::InvalidInput)
    ));
    request.project = None;
    request.text = "other".to_owned();
    assert!(matches!(
        store.search_task_summaries(alice, &request, PageTransport::Local),
        Err(StoreError::InvalidInput)
    ));
}

#[test]
fn group_flatten_cap_retains_empty_group_section_cursors() {
    let (_dir, store, alice, _bob) = fixture();
    for project in 0..5 {
        for item in 1..=50 {
            task(
                &store,
                alice,
                project * 50 + item,
                &root(&format!("group-{project}")),
                "visible",
            );
        }
    }
    let page = store
        .list_project_group_summaries(
            alice,
            &NavigationGroupQuery {
                manual: None,
                limit_groups: 5,
                per_project: 50,
                cursor: None,
            },
            PageTransport::Local,
        )
        .unwrap();
    assert_eq!(page.groups.len(), 5);
    assert_eq!(
        page.groups
            .iter()
            .map(|group| group.tasks.len())
            .sum::<usize>(),
        200
    );
    let last = &page.groups[4];
    assert!(last.tasks.is_empty());
    assert!(last.more.active.is_some());
    let tasks = store
        .list_task_summaries(
            alice,
            &NavigationTaskQuery {
                manual: None,
                section: TaskSection::State(TaskState::Active),
                project: Some(last.project.root.clone()),
                pins: PinFilter::Ignore,
                cursor: last.more.active.clone(),
                limit: 50,
            },
            PageTransport::Local,
        )
        .unwrap();
    assert_eq!(tasks.tasks.len(), 50);
}

#[test]
fn mixed_group_sections_keep_independent_continuations() {
    let (_dir, store, alice, _bob) = fixture();
    let project = root("mixed");
    task(&store, alice, 1, &project, "active one");
    task(&store, alice, 2, &project, "active two");
    let settled = task(&store, alice, 3, &project, "settled");
    store
        .update_task(
            alice,
            &settled,
            &TaskPatch {
                state: Some(TaskState::Settled),
                ..TaskPatch::default()
            },
        )
        .unwrap();
    let groups = store
        .list_project_group_summaries(
            alice,
            &NavigationGroupQuery {
                manual: None,
                limit_groups: 1,
                per_project: 1,
                cursor: None,
            },
            PageTransport::Remote,
        )
        .unwrap();
    let group = &groups.groups[0];
    assert_eq!(group.tasks.len(), 1);
    let active = store
        .list_task_summaries(
            alice,
            &NavigationTaskQuery {
                manual: None,
                section: TaskSection::State(TaskState::Active),
                project: Some(project.clone()),
                pins: PinFilter::Ignore,
                cursor: group.more.active.clone(),
                limit: 50,
            },
            PageTransport::Remote,
        )
        .unwrap();
    let settled = store
        .list_task_summaries(
            alice,
            &NavigationTaskQuery {
                manual: None,
                section: TaskSection::State(TaskState::Settled),
                project: Some(project),
                pins: PinFilter::Ignore,
                cursor: group.more.settled.clone(),
                limit: 50,
            },
            PageTransport::Remote,
        )
        .unwrap();
    assert_eq!(active.tasks.len(), 1);
    assert_eq!(settled.tasks.len(), 1);
    assert_ne!(active.tasks[0].uuid, group.tasks[0].uuid);
}

#[test]
fn remote_group_byte_cap_keeps_all_groups_and_unloaded_tasks_reachable() {
    let (_dir, store, alice, _bob) = fixture();
    let mut expected = HashSet::new();
    for project in 0..4 {
        // Owner-resolved default names repeat the basename in both the group
        // and each task summary. This size leaves room for a later empty group
        // but not its first task, deliberately exercising section continuations.
        let project_root = root(&format!("{}-{project}", "\u{1}".repeat(6_000)));
        for item in 1..=3 {
            expected.insert(task(
                &store,
                alice,
                project * 3 + item,
                &project_root,
                &"\u{2}".repeat(4096),
            ));
        }
    }
    let mut request = NavigationGroupQuery {
        manual: None,
        limit_groups: 4,
        per_project: 50,
        cursor: None,
    };
    let mut projects = HashSet::new();
    let mut actual = HashSet::new();
    let mut saw_empty = false;
    loop {
        let page = store
            .list_project_group_summaries(alice, &request, PageTransport::Remote)
            .unwrap();
        assert!(serialized_len(&page).unwrap() <= reply_limit(PageTransport::Remote));
        for group in page.groups {
            assert!(projects.insert(group.project.root.clone()));
            if group.tasks.is_empty() {
                saw_empty = true;
                assert!(
                    group.more.active.is_some(),
                    "an empty group retains its task continuation"
                );
            }
            actual.extend(group.tasks.into_iter().map(|row| row.uuid));
            let mut cursor = group.more.active;
            while cursor.is_some() {
                let tasks = store
                    .list_task_summaries(
                        alice,
                        &NavigationTaskQuery {
                            manual: None,
                            section: TaskSection::State(TaskState::Active),
                            project: Some(group.project.root.clone()),
                            pins: PinFilter::Ignore,
                            cursor,
                            limit: 50,
                        },
                        PageTransport::Remote,
                    )
                    .unwrap();
                assert!(serialized_len(&tasks).unwrap() <= reply_limit(PageTransport::Remote));
                actual.extend(tasks.tasks.into_iter().map(|row| row.uuid));
                cursor = tasks.next_cursor;
            }
        }
        request.cursor = page.next_cursor;
        if request.cursor.is_none() {
            break;
        }
    }
    assert!(saw_empty);
    assert_eq!(projects.len(), 4);
    assert_eq!(actual, expected);
}

#[test]
fn project_cursors_preserve_bounded_durable_order_tuples_for_large_roots() {
    let (_dir, store, alice, _bob) = fixture();
    let first_root = root(&"a".repeat(15_000));
    task(&store, alice, 1, &first_root, "one");
    task(&store, alice, 2, &root("z"), "two");
    let first = store
        .navigation_projects(alice, None, 1, PageTransport::Remote)
        .unwrap();
    let cursor = first.next_cursor.as_deref().unwrap();
    assert!(cursor.len() < 44 * 1024);
    let second = store
        .navigation_projects(alice, Some(cursor), 1, PageTransport::Remote)
        .unwrap();
    assert_eq!(second.projects.len(), 1);
    assert_ne!(first.projects[0].root, second.projects[0].root);
    assert!(second.next_cursor.is_none());
}

#[test]
fn project_cursor_survives_collection_of_the_previously_emitted_project() {
    let (_dir, store, alice, _bob) = fixture();
    let old_root = root("a");
    let old_id = task(&store, alice, 1, &old_root, "old");
    task(&store, alice, 2, &root("z"), "later");
    let first = store
        .navigation_projects(alice, None, 1, PageTransport::Local)
        .unwrap();
    store
        .write(crate::store::Durability::Durable, |tx| {
            tx.execute(
                "DELETE FROM tasks WHERE account_id=?1 AND uuid=?2",
                params![alice.0, old_id],
            )?;
            tx.execute(
                "DELETE FROM projects WHERE account_id=?1 AND root=?2",
                params![alice.0, old_root],
            )?;
            Ok(())
        })
        .unwrap();
    let second = store
        .navigation_projects(alice, first.next_cursor.as_deref(), 1, PageTransport::Local)
        .unwrap();
    assert_eq!(second.projects[0].root, root("z"));
}

#[test]
fn project_keysets_seek_ties_later_positions_and_unpositioned_roots() {
    let (_dir, store, alice, _bob) = fixture();
    for (index, name) in ["a", "b", "c", "d", "q", "z"].into_iter().enumerate() {
        let project_root = root(name);
        task(&store, alice, index as u64 + 1, &project_root, "visible");
        if index < 4 {
            store
                .remember_project(alice, &project_root, if index < 2 { 0 } else { 3 }, 1)
                .unwrap();
        }
    }
    let mut cursor = None;
    let mut actual = Vec::new();
    loop {
        let page = store
            .navigation_projects(alice, cursor.as_deref(), 1, PageTransport::Local)
            .unwrap();
        actual.extend(page.projects.into_iter().map(|project| project.root));
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(actual, ["a", "b", "c", "d", "q", "z"].map(root));
}

#[test]
fn summary_fit_preserves_exact_identity_and_marks_shortened_metadata() {
    let mut summary = TaskSummary {
        project_display_name: None,
        uuid: "10000000-0000-7000-8000-000000000001".to_owned(),
        title: "é".repeat(4096),
        project_root: root("exact"),
        state: "active".to_owned(),
        kind: "desktop".to_owned(),
        pinned_ms: None,
        web_enabled: true,
        model: Some("test".repeat(1024)),
        message_count: 1,
        created_ms: 1,
        updated_ms: 1,
        truncated: false,
    };
    let exact_root = summary.project_root.clone();
    fit_task_summary(&mut summary, 1024).unwrap();
    assert!(summary.truncated);
    assert!(serialized_len(&summary).unwrap() <= 1024);
    assert_eq!(summary.project_root, exact_root);
    assert!(matches!(
        fit_task_summary(&mut summary, 1),
        Err(StoreError::InvalidInput)
    ));
}

#[test]
fn enclosing_project_budget_keeps_a_start_cursor_when_first_root_cannot_fit() {
    let (_dir, store, alice, bob) = fixture();
    let large_root = root(&"\u{1}".repeat(2000));
    task(&store, alice, 1, &large_root, "visible");
    let first = store
        .navigation_projects_with_budget(alice, None, 50, PageTransport::Local, 2048)
        .unwrap();
    assert!(first.projects.is_empty());
    assert!(serialized_len(&first).unwrap() <= 2048);
    let next = store
        .navigation_projects(
            alice,
            first.next_cursor.as_deref(),
            50,
            PageTransport::Local,
        )
        .unwrap();
    assert_eq!(next.projects[0].root, large_root);
    assert!(next.next_cursor.is_none());
    assert!(matches!(
        store.navigation_projects(bob, first.next_cursor.as_deref(), 50, PageTransport::Local),
        Err(StoreError::InvalidInput)
    ));
}

#[test]
fn enclosing_project_budget_resumes_after_its_last_emitted_root() {
    let (_dir, store, alice, _bob) = fixture();
    let first_root = root("a");
    let large_root = root(&format!("z{}", "\u{1}".repeat(2000)));
    task(&store, alice, 1, &first_root, "first");
    task(&store, alice, 2, &large_root, "later");
    let first = store
        .navigation_projects_with_budget(alice, None, 50, PageTransport::Local, 2048)
        .unwrap();
    assert_eq!(
        first
            .projects
            .iter()
            .map(|project| &project.root)
            .collect::<Vec<_>>(),
        vec![&first_root]
    );
    let next = store
        .navigation_projects(
            alice,
            first.next_cursor.as_deref(),
            50,
            PageTransport::Local,
        )
        .unwrap();
    assert_eq!(
        next.projects
            .iter()
            .map(|project| &project.root)
            .collect::<Vec<_>>(),
        vec![&large_root]
    );
}

#[test]
fn name_revisions_survive_reset_and_are_in_both_bounded_project_projections() {
    let (_dir, store, alice, bob) = fixture();
    let project = root("names");
    task(&store, alice, 1, &project, "Task");
    store.register_explicit_project(alice, &project, 1).unwrap();
    assert!(
        store
            .set_project_display_name(alice, &project, Some("Custom name"), 2)
            .unwrap()
    );
    let first = store
        .navigation_projects(alice, None, 1, PageTransport::Local)
        .unwrap();
    let revision = first.projects[0].name_revision;
    assert!(revision > 0);
    assert_eq!(
        first.projects[0].display_name.as_deref(),
        Some("Custom name")
    );
    assert!(
        !store
            .set_project_display_name(alice, &project, Some("Custom name"), 3)
            .unwrap()
    );
    assert_eq!(
        store
            .project(alice, &project)
            .unwrap()
            .unwrap()
            .name_revision,
        revision
    );
    assert!(
        store
            .set_project_display_name(alice, &project, None, 4)
            .unwrap()
    );
    let reset = store
        .navigation_projects(alice, None, 1, PageTransport::Local)
        .unwrap();
    assert!(reset.projects[0].name_revision > revision);
    assert_eq!(reset.projects[0].display_name.as_deref(), Some("names"));
    let groups = store
        .list_project_group_summaries(
            alice,
            &NavigationGroupQuery {
                manual: None,
                limit_groups: 1,
                per_project: 1,
                cursor: None,
            },
            PageTransport::Local,
        )
        .unwrap();
    assert_eq!(groups.groups[0].project, reset.projects[0]);
    assert!(
        store
            .navigation_projects(bob, None, 1, PageTransport::Local)
            .unwrap()
            .projects
            .is_empty()
    );
}

#[test]
fn search_finds_unloaded_owner_project_names_with_unicode_and_bounded_cursors() {
    let (_dir, store, alice, bob) = fixture();
    for number in 1..=65 {
        let project = root(&format!("ordered-{number:03}"));
        store.remember_project(alice, &project, number, 1).unwrap();
        task(&store, alice, number as u64, &project, "Unrelated title");
    }
    let target = root("ordered-065");
    store
        .set_project_display_name(alice, &target, Some("ÄPFEL research"), 2)
        .unwrap();
    let first_projects = store
        .navigation_projects(alice, None, 50, PageTransport::Local)
        .unwrap();
    assert_eq!(first_projects.projects.len(), 50);
    assert!(
        first_projects
            .projects
            .iter()
            .all(|project| project.root != target)
    );
    let second_id = task(&store, alice, 100, &target, "Another unrelated title");
    task(&store, bob, 200, &target, "Unrelated other account");
    let mut request = NavigationSearchQuery {
        manual: None,
        text: "äpfel".into(),
        project: None,
        project_roots: Vec::new(),
        cursor: None,
        limit: 1,
    };
    let first = store
        .search_task_summaries(alice, &request, PageTransport::Local)
        .unwrap();
    assert_eq!(first.tasks.len(), 1);
    assert_eq!(first.tasks[0].uuid, second_id);
    assert_eq!(
        first.tasks[0].project_display_name.as_deref(),
        Some("ÄPFEL research")
    );
    assert!(first.next_cursor.is_some());
    request.cursor = first.next_cursor;
    let second = store
        .search_task_summaries(alice, &request, PageTransport::Local)
        .unwrap();
    assert_eq!(second.tasks.len(), 1);
    assert_ne!(second.tasks[0].uuid, first.tasks[0].uuid);
    assert_eq!(second.tasks[0].project_root, target);
    assert!(second.next_cursor.is_none());
    request.cursor = None;
    assert!(
        store
            .search_task_summaries(bob, &request, PageTransport::Local)
            .unwrap()
            .tasks
            .is_empty()
    );
    request.project = Some(root("ordered-001"));
    assert!(
        store
            .search_task_summaries(alice, &request, PageTransport::Local)
            .unwrap()
            .tasks
            .is_empty()
    );
}

#[test]
fn search_resolves_owner_default_basename_and_workspace_label_without_client_hints() {
    let (_dir, store, alice, bob) = fixture();
    let project = root("ÅngströmFolder");
    let id = task(&store, alice, 1, &project, "Unrelated title");
    let mut request = NavigationSearchQuery {
        manual: None,
        text: "ångströmfolder".into(),
        project: None,
        project_roots: Vec::new(),
        cursor: None,
        limit: 10,
    };
    let found = store
        .search_task_summaries(alice, &request, PageTransport::Local)
        .unwrap();
    assert_eq!(found.tasks.len(), 1);
    assert_eq!(found.tasks[0].uuid, id);
    assert_eq!(
        found.tasks[0].project_display_name.as_deref(),
        Some("ÅngströmFolder")
    );
    assert!(
        store
            .search_task_summaries(bob, &request, PageTransport::Local)
            .unwrap()
            .tasks
            .is_empty()
    );
    // The well-known workspace is data only: this fixture never opens or creates
    // the actual directory on the developer's execution computer.
    if let Some(workspace) = crate::agent::default_maple_workspace_path() {
        let workspace_id = task(
            &store,
            alice,
            2,
            &workspace.to_string_lossy(),
            "Different title",
        );
        request.text = "MAPLE WORKSPACE".into();
        let found = store
            .search_task_summaries(alice, &request, PageTransport::Local)
            .unwrap();
        assert_eq!(found.tasks.len(), 1);
        assert_eq!(found.tasks[0].uuid, workspace_id);
        assert_eq!(
            found.tasks[0].project_display_name.as_deref(),
            Some("Maple Workspace")
        );
    }
}
