//! Tests for the chat screen.

mod state_tests {
    use std::sync::Arc;

    use crate::ui::chat::cache::{INLINE_PARSE_LIMIT, MAX_DIFF_LINES, ORDINAL_SPACING};
    use crate::ui::chat::composer::SideThreadTurn;
    use crate::ui::chat::computers::ProjectTarget;
    use crate::ui::chat::images::{MAX_DRAFT_IMAGES, encode_data_url};
    use crate::ui::chat::sidebar::{RenameTarget, SidebarEvent, TaskMove};
    use crate::ui::chat::transcript::{diff_lines_for, tool_label_title};
    use crate::ui::chat::*;
    use gpui::TestAppContext;
    use maple_agent::agent::{
        AgentMcpServer, AgentMcpTransport, AgentSessionIntegrationKind, AgentTaskState,
        RecentProjectRoot,
    };

    fn summary(id: &str, title: &str) -> AgentSessionSummary {
        summary_at(id, title, "/tmp/proj")
    }

    fn summary_at(id: &str, title: &str, project_root: &str) -> AgentSessionSummary {
        AgentSessionSummary {
            web_enabled: true,
            state: AgentTaskState::Active,
            acp: false,
            id: id.to_string(),
            title: title.to_string(),
            project_root: project_root.to_string(),
            created_ms: 0,
            updated_ms: 0,
            message_count: 1,
            model: None,
        }
    }

    fn recent_root(path: &str) -> RecentProjectRoot {
        named_root(path, &root_display_name(path), 0)
    }

    fn named_root(path: &str, name: &str, name_revision: u64) -> RecentProjectRoot {
        RecentProjectRoot {
            path: path.to_string(),
            name: name.to_string(),
            last_used_ms: 0,
            is_default_workspace: false,
            name_revision,
        }
    }

    /// A project root that `select_project_root` accepts on every platform. A bare
    /// `/name` has a root but no drive, so `Path::is_absolute` rejects it on
    /// Windows and the switch never starts.
    fn absolute_fixture_root(name: &str) -> String {
        if cfg!(windows) {
            format!("C:\\{name}")
        } else {
            format!("/{name}")
        }
    }

    #[gpui::test]
    fn computer_catalog_buffers_summary_changes_without_loading_transcripts(
        cx: &mut TestAppContext,
    ) {
        use maple_agent::agent::{AgentRunEvent, AgentRuntimeStatus};
        let chat = screen(cx);
        chat.update(cx, |this, cx| {
            this.navigation_only = true;
            this.visible = false;
            this.selected_session = None;
            this.host_recovery.events = Some(Vec::new());
            for _ in 0..1000 {
                this.handle_service_event(
                    AgentServiceEvent::TimelineItem {
                        session_id: "remote-task".into(),
                        run_id: Some("run".into()),
                        item: item("reply", "message", Some("streaming text")),
                    },
                    cx,
                );
            }
            this.handle_service_event(
                AgentServiceEvent::SessionCreated(summary("remote-task", "New remote title")),
                cx,
            );
            this.handle_service_event(
                AgentServiceEvent::Run {
                    session_id: "remote-task".into(),
                    run_id: "run".into(),
                    event: AgentRunEvent::Started,
                },
                cx,
            );
            assert!(!this.host_recovery.overflow);
            assert_eq!(this.host_recovery.events.as_ref().unwrap().len(), 2);
            let events = this.host_recovery.events.take().unwrap();
            this.apply_recovery_snapshot(
                crate::backend::RecoverySnapshot {
                    bootstrap: crate::backend::LocalBootstrap {
                        project_root: Some("/other/machine".into()),
                        recent_roots: vec![recent_root("/other/machine")],
                        latest: None,
                        sessions: Vec::new(),
                        questions: Vec::new(),
                    },
                    status: AgentRuntimeStatus {
                        running: true,
                        project_root: None,
                        model: None,
                        active_runs: HashMap::new(),
                    },
                    selected: None,
                },
                this.selection_generation,
                cx,
            );
            this.handle_service_events(events, cx);
            this.finish_navigation_bootstrap(cx);
            assert!(this.navigation_only);
            assert!(this.navigation_ready);
            assert!(this.selected_session.is_none());
            assert!(this.timeline.is_empty());
            assert_eq!(this.sessions[0].title, "New remote title");
            assert_eq!(
                this.active_runs.get("remote-task").map(String::as_str),
                Some("run")
            );
            assert_eq!(this.project_root.as_deref(), Some("/other/machine"));
            this.handle_service_event(AgentServiceEvent::HostReconnecting, cx);
            let cached = this.computer_snapshot("Office".into(), cx);
            assert_eq!(cached.sessions.len(), 1);
            assert!(cached.error.is_some());
        });
    }

    #[gpui::test]
    fn completion_in_a_background_computer_stays_unread_until_opened(cx: &mut TestAppContext) {
        use maple_agent::agent::{AgentRunEvent, AgentRunTerminal};
        cx.executor().allow_parking();
        let chat = screen(cx);
        chat.update(cx, |this, cx| {
            this.sessions = vec![summary("s1", "Remote work")];
            this.selected_session = Some("s1".into());
            this.set_visible(false, cx);
            this.handle_run_event(
                "s1",
                "run",
                AgentRunEvent::Finished(AgentRunTerminal::Completed),
                cx,
            );
            assert!(this.completed_unread_sessions.contains("s1"));
            // Merely receiving another summary or snapshot does not read it.
            this.sync_sidebar(cx);
            assert!(
                this.computer_snapshot("Office".into(), cx)
                    .unread
                    .contains("s1")
            );
            this.set_visible(true, cx);
            assert!(!this.completed_unread_sessions.contains("s1"));
        });
    }

    #[gpui::test]
    fn computer_projects_keep_identical_paths_and_task_ids_separate(cx: &mut TestAppContext) {
        use super::super::computers::{Computer, Destination, Navigate, Project};
        use super::super::sidebar::SidebarEntry;
        let chat = screen(cx);
        chat.update(cx, |this, cx| {
            this.sessions = vec![summary("same-id", "Local task")];
            this.recent_roots = vec![recent_root("/tmp/proj")];
            this.sync_sidebar(cx);
            let local = this.computer_snapshot("This computer".into(), cx);
            let remote = Computer {
                key: Some("paired-device".into()),
                label: "Office Mac".into(),
                backend: None,
                host_epoch: 0,
                project_names: HashMap::new(),
                projects: vec![Project {
                    path: "/tmp/proj".into(),
                    label: "proj".into(),
                }],
                sessions: vec![summary("same-id", "Remote task")],
                running: HashSet::from(["same-id".into()]),
                unread: HashSet::new(),
                pinned: Vec::new(),
                loading: false,
                error: None,
            };
            this.set_computers(Arc::new(vec![local, remote]), cx);
            this.sidebar.update(cx, |sidebar, _| {
                assert_eq!(
                    sidebar
                        .entries()
                        .iter()
                        .filter(|e| matches!(e, SidebarEntry::ComputerProject(_)))
                        .count(),
                    2
                );
                assert_eq!(
                    sidebar
                        .entries()
                        .iter()
                        .filter(|e| matches!(e, SidebarEntry::Task(_)))
                        .count(),
                    1
                );
                let entry = *sidebar
                    .entries()
                    .iter()
                    .find(|e| matches!(e, SidebarEntry::OtherTask(_)))
                    .unwrap();
                assert_eq!(
                    sidebar.computer_target(entry),
                    Some(navigation::SidebarTarget::Computer(Navigate {
                        computer: Some("paired-device".into()),
                        destination: Destination::Task("same-id".into()),
                    }))
                );
            });
            this.selected_session = Some("same-id".into());
            this.sync_sidebar_selection(cx);
            assert_eq!(
                this.task_step_target(1, cx).map(|(_, request)| request),
                Some(Navigate {
                    computer: Some("paired-device".into()),
                    destination: Destination::Task("same-id".into()),
                })
            );
            // The projection never merges foreign records into the local runtime list.
            assert_eq!(this.sessions.len(), 1);
            assert_eq!(this.sessions[0].title, "Local task");
            let local = this.computer_snapshot("This computer".into(), cx);
            this.set_computers(Arc::new(vec![local]), cx);
            assert!(!this.sidebar.read(cx).entries().iter().any(|e| matches!(
                e,
                SidebarEntry::OtherTask(_) | SidebarEntry::ComputerProject(_)
            )));
        });
    }

    #[gpui::test]
    fn computer_project_search_and_live_summary_refresh(cx: &mut TestAppContext) {
        use super::super::computers::{Computer, Project};
        use super::super::sidebar::SidebarEntry;
        let chat = screen(cx);
        chat.update(cx, |this, cx| {
            this.sessions = vec![summary("local", "Local title")];
            this.sync_sidebar(cx);
            let local = this.computer_snapshot("This computer".into(), cx);
            let mut remote = Computer::pending("peer".into(), "Office Mac".into(), None);
            remote.projects = vec![Project {
                path: "/remote".into(),
                label: "Remote folder".into(),
            }];
            remote.sessions = vec![summary_at("remote", "Before title", "/remote")];
            this.set_computers(Arc::new(vec![local.clone(), remote.clone()]), cx);
            this.set_sidebar_filter("after title", cx);
            assert!(
                !this
                    .sidebar
                    .read(cx)
                    .entries()
                    .iter()
                    .any(|e| matches!(e, SidebarEntry::OtherTask(_)))
            );
            remote.sessions[0].title = "After title".into();
            this.set_computers(Arc::new(vec![local, remote]), cx);
            assert!(
                this.sidebar
                    .read(cx)
                    .entries()
                    .iter()
                    .any(|e| matches!(e, SidebarEntry::OtherTask(_)))
            );
            this.set_sidebar_filter("office mac", cx);
            assert!(
                this.sidebar
                    .read(cx)
                    .entries()
                    .iter()
                    .any(|e| matches!(e, SidebarEntry::OtherTask(_)))
            );
            assert!(
                !this
                    .sidebar
                    .read(cx)
                    .entries()
                    .iter()
                    .any(|e| matches!(e, SidebarEntry::Task(_)))
            );
        });
    }

    #[gpui::test]
    fn project_name_events_refresh_visible_and_background_catalogs(cx: &mut TestAppContext) {
        let chat = screen(cx);
        chat.update(cx, |this, cx| {
            this.project_root = Some("/tmp/proj".into());
            this.sessions = vec![summary("s1", "Unchanged task")];
            this.replace_timeline(vec![user_item("message", "Unchanged transcript")]);
            for (background, before, after, revision) in [
                (false, "Original label", "Visible owner label", 1),
                (true, "Visible owner label", "Background owner label", 2),
            ] {
                this.navigation_only = background;
                this.visible = !background;
                this.apply_recent_roots(vec![named_root("/tmp/proj", before, revision - 1)], cx);
                let selected = this.selected_session.clone();
                let generation = this.selection_generation;
                assert!(this.apply_service_event(
                    AgentServiceEvent::ProjectsChanged {
                        user_id: "user".into(),
                        roots: vec![named_root("/tmp/proj", after, revision)],
                    },
                    cx,
                ));
                assert_eq!(this.project_label(cx), after);
                assert_eq!(this.project_label.as_ref(), after);
                assert_eq!(this.sidebar.read(cx).rows()[0].project_name.as_ref(), after);
                let catalog = this.computer_snapshot("Owner computer".into(), cx);
                assert_eq!(catalog.projects[0].path, "/tmp/proj");
                assert_eq!(catalog.projects[0].label.as_ref(), after);
                this.set_sidebar_filter(before, cx);
                assert!(
                    this.sidebar_active(cx).is_empty(),
                    "old label no longer matches"
                );
                this.set_sidebar_filter(&after.to_uppercase(), cx);
                assert_eq!(this.sidebar_active(cx), vec![0], "new label is searchable");
                this.set_sidebar_filter("", cx);
                assert_eq!(this.selected_session, selected);
                assert_eq!(this.selection_generation, generation);
                assert_eq!(this.timeline[0].id, "message");
                assert_eq!(this.sessions[0].title, "Unchanged task");
                assert!(!this.apply_service_event(
                    AgentServiceEvent::ProjectsChanged {
                        user_id: "another-account".into(),
                        roots: vec![named_root("/tmp/proj", "Wrong account", 999)],
                    },
                    cx,
                ));
                assert_eq!(this.project_label(cx), after);
                assert_eq!(this.recent_roots[0].name_revision, revision);
            }
        });
    }

    #[gpui::test]
    fn project_name_delta_preserves_other_loaded_projects(cx: &mut TestAppContext) {
        let chat = screen(cx);
        chat.update(cx, |this, cx| {
            this.apply_recent_roots(
                vec![
                    named_root("/tmp/proj", "Original name", 1),
                    named_root("/another/project", "Other project", 3),
                ],
                cx,
            );
            let selection = this.selected_session.clone();
            let generation = this.selection_generation;
            assert!(this.apply_service_event(
                AgentServiceEvent::ProjectsChanged {
                    user_id: "user".into(),
                    roots: vec![named_root("/tmp/proj", "Owner rename", 2)],
                },
                cx,
            ));
            assert_eq!(this.recent_roots.len(), 2);
            assert_eq!(this.recent_roots[0].name, "Owner rename");
            assert_eq!(this.recent_roots[1].name, "Other project");
            assert_eq!(
                this.sidebar.read(cx).navigation_roots(),
                vec!["/tmp/proj", "/another/project"]
            );
            assert_eq!(this.selected_session, selection);
            assert_eq!(this.selection_generation, generation);
        });
    }

    #[gpui::test]
    fn removed_workspace_anchor_stays_hidden_until_an_owner_restore(cx: &mut TestAppContext) {
        let chat = screen(cx);
        chat.update(cx, |this, cx| {
            let mut workspace = named_root("/workspace", "Maple Workspace", 7);
            workspace.is_default_workspace = true;
            let other = named_root("/another/project", "Other project", 2);
            this.apply_recent_roots(vec![workspace.clone(), other.clone()], cx);
            // A first page can omit an older, still-listed workspace. That
            // ordinary absence must retain its separately loaded anchor.
            let page = || maple_agent::store::NavigationProjectPage {
                projects: vec![maple_agent::store::ProjectSummary {
                    root: other.path.clone(),
                    display_name: Some(other.name.clone()),
                    name_revision: other.name_revision,
                    is_default_workspace: false,
                    last_used_ms: 0,
                    position: None,
                    truncated: false,
                }],
                next_cursor: Some("more-projects".into()),
            };
            this.sidebar.update(cx, |sidebar, cx| {
                sidebar.apply_project_page_for_test(page(), cx);
                assert!(sidebar.navigation_roots().contains(&workspace.path));
            });
            assert!(!this.apply_service_event(
                AgentServiceEvent::ProjectRemoved {
                    user_id: "different-account".into(),
                    project_root: workspace.path.clone(),
                },
                cx,
            ));
            assert!(this.apply_service_event(
                AgentServiceEvent::ProjectRemoved {
                    user_id: "user".into(),
                    project_root: workspace.path.clone(),
                },
                cx,
            ));
            // A pushed removal (including one from another controller),
            // later page refreshes, and stale metadata all keep it hidden.
            this.sidebar.update(cx, |sidebar, cx| {
                assert!(sidebar.project_is_removed(&workspace.path));
                sidebar.apply_project_page_for_test(page(), cx);
                sidebar.merge_project_metadata(vec![workspace.clone()], cx);
                assert!(!sidebar.navigation_roots().contains(&workspace.path));
            });
            assert!(
                !this
                    .recent_roots
                    .iter()
                    .any(|root| root.path == workspace.path)
            );

            // Only an owner-issued selection/restore or an authoritative
            // project page lists the path again.
            this.restore_selected_project(workspace.clone(), cx);
            this.sidebar.update(cx, |sidebar, cx| {
                assert!(!sidebar.project_is_removed(&workspace.path));
                sidebar.apply_project_page_for_test(page(), cx);
                assert!(sidebar.navigation_roots().contains(&workspace.path));
            });
            assert!(
                this.recent_roots
                    .iter()
                    .any(|root| root.path == workspace.path)
            );

            // If removal happened while disconnected, no push is available.
            // Recovery's separate default-project lookup reports None, so its
            // bootstrap roots omit the anchor even if it is still cached.
            this.selected_session = None;
            this.apply_recovery_snapshot(
                crate::backend::RecoverySnapshot {
                    bootstrap: crate::backend::LocalBootstrap {
                        project_root: Some(other.path.clone()),
                        recent_roots: vec![other.clone()],
                        sessions: Vec::new(),
                        latest: None,
                        questions: Vec::new(),
                    },
                    status: maple_agent::agent::AgentRuntimeStatus {
                        running: false,
                        project_root: Some(other.path.clone()),
                        model: None,
                        active_runs: HashMap::new(),
                    },
                    selected: None,
                },
                this.selection_generation,
                cx,
            );
            this.sidebar.update(cx, |sidebar, cx| {
                sidebar.apply_project_page_for_test(page(), cx);
                assert!(sidebar.project_is_removed(&workspace.path));
                assert!(!sidebar.navigation_roots().contains(&workspace.path));
            });
        });
    }

    #[gpui::test]
    fn project_name_revisions_survive_stale_events_snapshots_and_explicit_reset(
        cx: &mut TestAppContext,
    ) {
        use maple_agent::agent::AgentRuntimeStatus;
        let chat = screen(cx);
        chat.update(cx, |this, cx| {
            this.navigation_only = true;
            this.visible = false;
            this.selected_session = None;
            this.project_root = Some("/tmp/proj".into());
            this.sessions = vec![summary("s1", "Existing task")];
            this.apply_recent_roots(vec![named_root("/tmp/proj", "Latest owner name", 8)], cx);
            for old_name in ["Older alias", "proj"] {
                assert!(!this.apply_service_event(
                    AgentServiceEvent::ProjectsChanged {
                        user_id: "user".into(),
                        roots: vec![named_root("/tmp/proj", old_name, 7)],
                    },
                    cx,
                ));
                assert_eq!(this.project_label(cx), "Latest owner name");
            }
            this.apply_recovery_snapshot(
                crate::backend::RecoverySnapshot {
                    bootstrap: crate::backend::LocalBootstrap {
                        project_root: Some("/tmp/proj".into()),
                        recent_roots: vec![named_root("/tmp/proj", "Stale snapshot", 6)],
                        latest: None,
                        sessions: vec![summary("s1", "Existing task")],
                        questions: Vec::new(),
                    },
                    status: AgentRuntimeStatus {
                        running: true,
                        project_root: Some("/tmp/proj".into()),
                        model: None,
                        active_runs: HashMap::new(),
                    },
                    selected: None,
                },
                this.selection_generation,
                cx,
            );
            assert_eq!(this.project_label(cx), "Latest owner name");
            assert_eq!(this.recent_roots[0].name_revision, 8);
            // Resetting to the folder name is a newer owner action, not an
            // absent label that an old rename or pre-reset snapshot can restore.
            assert!(this.apply_service_event(
                AgentServiceEvent::ProjectsChanged {
                    user_id: "user".into(),
                    roots: vec![named_root("/tmp/proj", "proj", 9)],
                },
                cx,
            ));
            this.apply_recent_roots(vec![named_root("/tmp/proj", "Latest owner name", 8)], cx);
            assert_eq!(this.project_label(cx), "proj");
            assert_eq!(
                this.sidebar.read(cx).rows()[0].project_name.as_ref(),
                "proj"
            );
            assert_eq!(this.recent_roots[0].name_revision, 9);
            assert!(this.apply_service_event(
                AgentServiceEvent::ProjectsChanged {
                    user_id: "user".into(),
                    roots: vec![named_root("/tmp/proj", "Newer rename", 10)],
                },
                cx,
            ));
            assert_eq!(this.project_label(cx), "Newer rename");
            // A list requested before this project was registered/restored may
            // omit it altogether. That is not permission to forget its newer
            // name revision while an existing task/header still references it.
            this.apply_recent_roots(vec![recent_root("/another/project")], cx);
            assert!(
                this.recent_roots
                    .iter()
                    .all(|root| root.path != "/tmp/proj")
            );
            assert_eq!(this.project_label(cx), "Newer rename");
            assert_eq!(this.project_label.as_ref(), "Newer rename");
            assert_eq!(
                this.sidebar.read(cx).rows()[0].project_name.as_ref(),
                "Newer rename"
            );
            // An even older snapshot can then reintroduce the path. Preserve
            // the owner-issued high-water revision independently of membership.
            this.apply_recovery_snapshot(
                crate::backend::RecoverySnapshot {
                    bootstrap: crate::backend::LocalBootstrap {
                        project_root: Some("/tmp/proj".into()),
                        recent_roots: vec![named_root("/tmp/proj", "Before restoration", 8)],
                        latest: None,
                        sessions: vec![summary("s1", "Existing task")],
                        questions: Vec::new(),
                    },
                    status: AgentRuntimeStatus {
                        running: true,
                        project_root: Some("/tmp/proj".into()),
                        model: None,
                        active_runs: HashMap::new(),
                    },
                    selected: None,
                },
                this.selection_generation,
                cx,
            );
            assert_eq!(this.project_label(cx), "Newer rename");
            assert_eq!(this.project_label.as_ref(), "Newer rename");
            assert_eq!(
                this.sidebar.read(cx).rows()[0].project_name.as_ref(),
                "Newer rename"
            );
            assert_eq!(this.recent_roots[0].name, "Newer rename");
            assert_eq!(this.recent_roots[0].name_revision, 10);
        });
    }

    #[gpui::test]
    fn unloaded_project_owner_label_survives_task_search_projection(cx: &mut TestAppContext) {
        let chat = screen(cx);
        chat.update(cx, |this, cx| {
            let root = "/tmp/ordinary-folder";
            let task = maple_agent::store::TaskSummary {
                uuid: "label-match".into(),
                title: "Discuss next step".into(),
                project_root: root.into(),
                project_display_name: Some("Nebula Planning".into()),
                state: "active".into(),
                kind: "desktop".into(),
                pinned_ms: None,
                web_enabled: false,
                model: None,
                message_count: 1,
                created_ms: 0,
                updated_ms: 1,
                truncated: false,
            };
            assert!(!task.title.to_lowercase().contains("nebula"));
            assert!(!root.contains("nebula"));
            assert_eq!(this.sidebar.read(cx).root_name(root), "ordinary-folder");
            for project in [None, Some(root.to_owned())] {
                this.sidebar.update(cx, |sidebar, cx| {
                    sidebar.apply_search_page_for_test("nebula", project, vec![task.clone()], cx);
                    assert_eq!(sidebar.active_rows().len(), 1);
                    let row = &sidebar.rows()[sidebar.active_rows()[0]];
                    assert_eq!(row.title.as_ref(), "Discuss next step");
                    assert_eq!(row.project_name.as_ref(), "Nebula Planning");
                    assert!(
                        sidebar
                            .entries()
                            .iter()
                            .any(|entry| matches!(entry, sidebar::SidebarEntry::Task(_)))
                    );
                });
            }
            // A newer pushed owner name still supersedes unversioned labels
            // carried by task pages, including a delayed old search reply.
            assert!(this.apply_service_event(
                AgentServiceEvent::ProjectsChanged {
                    user_id: "user".into(),
                    roots: vec![named_root(root, "Renamed owner project", 2)],
                },
                cx
            ));
            this.sidebar.update(cx, |sidebar, cx| {
                sidebar.apply_search_page_for_test("nebula", None, vec![task], cx);
                assert!(sidebar.active_rows().is_empty());
                assert_eq!(sidebar.root_name(root), "Renamed owner project");
            });
        });
    }

    #[gpui::test]
    fn recovery_reauthorizes_an_unloaded_project_label_before_search(cx: &mut TestAppContext) {
        let chat = screen(cx);
        chat.update(cx, |this, cx| {
            let root = "/tmp/older-project";
            let owner_page = (0..50)
                .map(|index| named_root(&format!("/tmp/recent-{index:02}"), "Recent project", 1))
                .collect::<Vec<_>>();
            let mut before_disconnect = owner_page.clone();
            before_disconnect.push(named_root(root, "Old cached label", 4));
            this.apply_recent_roots(before_disconnect, cx);
            assert_eq!(this.sidebar.read(cx).root_name(root), "Old cached label");
            assert!(!owner_page.iter().any(|project| project.path == root));

            // The successful reconnect snapshot covers fifty other projects.
            // Its callback resets unobserved labels before replaying pushes.
            this.prepare_project_cache_for_recovery(&owner_page, cx);
            this.selected_session = None;
            this.apply_recovery_snapshot(
                crate::backend::RecoverySnapshot {
                    bootstrap: crate::backend::LocalBootstrap {
                        project_root: None,
                        recent_roots: owner_page,
                        sessions: Vec::new(),
                        latest: None,
                        questions: Vec::new(),
                    },
                    status: maple_agent::agent::AgentRuntimeStatus {
                        running: false,
                        project_root: None,
                        model: None,
                        active_runs: HashMap::new(),
                    },
                    selected: None,
                },
                this.selection_generation,
                cx,
            );
            let task = maple_agent::store::TaskSummary {
                uuid: "offline-label-match".into(),
                title: "Unrelated title".into(),
                project_root: root.into(),
                project_display_name: Some("ÄPFEL research".into()),
                state: "active".into(),
                kind: "desktop".into(),
                pinned_ms: None,
                web_enabled: false,
                model: None,
                message_count: 1,
                created_ms: 0,
                updated_ms: 1,
                truncated: false,
            };
            this.sidebar.update(cx, |sidebar, cx| {
                sidebar.apply_search_page_for_test("äpfel", None, vec![task.clone()], cx);
                assert_eq!(sidebar.active_rows().len(), 1);
                let row = &sidebar.rows()[sidebar.active_rows()[0]];
                assert_eq!(row.project_name.as_ref(), "ÄPFEL research");
                assert!(
                    sidebar
                        .entries()
                        .iter()
                        .any(|entry| matches!(entry, sidebar::SidebarEntry::Task(_)))
                );
            });

            // A newer push buffered during the owner read is applied after
            // that reset; an older task-page reply still cannot undo it.
            assert!(this.apply_service_event(
                AgentServiceEvent::ProjectsChanged {
                    user_id: "user".into(),
                    roots: vec![named_root(root, "Newest owner label", 6)],
                },
                cx
            ));
            this.sidebar.update(cx, |sidebar, cx| {
                sidebar.apply_search_page_for_test("äpfel", None, vec![task], cx);
                assert!(sidebar.active_rows().is_empty());
                assert_eq!(sidebar.root_name(root), "Newest owner label");
            });
        });
    }

    #[gpui::test]
    fn renamed_computer_project_keeps_picker_identity_and_updates_cross_computer_search(
        cx: &mut TestAppContext,
    ) {
        use super::super::computers::{Computer, Navigate};
        use super::super::sidebar::SidebarEntry;
        use std::cell::RefCell;
        use std::rc::Rc;
        let (chat, cx) = chat_window(cx, |this, cx| {
            this.project_root = Some("/tmp/proj".into());
            this.sessions = vec![summary("s1", "Local task")];
            this.apply_recent_roots(vec![named_root("/tmp/proj", "Local name", 1)], cx);
            let local = this.computer_snapshot("This computer".into(), cx);
            let mut remote = Computer::pending("peer".into(), "Office Mac".into(), None);
            remote.loading = false;
            remote.projects = vec![computers::Project {
                path: "/tmp/proj".into(),
                label: "Old remote label".into(),
            }];
            remote.sessions = vec![summary("s1", "Remote task")];
            this.set_computers(Arc::new(vec![local, remote]), cx);
        });
        let requests = Rc::new(RefCell::new(Vec::new()));
        let seen = requests.clone();
        let _subscription = cx.update(|_, app| {
            app.subscribe(&chat, move |_, request: &Navigate, _| {
                seen.borrow_mut().push(request.clone());
            })
        });
        chat.update(cx, |this, cx| {
            let mut catalog = this.computers.as_ref().clone();
            catalog[1].projects[0].label = "Renamed remote project".into();
            this.set_computers(Arc::new(catalog), cx);
            assert_eq!(
                this.project_label(cx),
                "Local name",
                "same path on another computer is separate"
            );
            assert_eq!(
                this.computers[1].projects[0].label.as_ref(),
                "Renamed remote project"
            );
            this.set_sidebar_filter("old remote label", cx);
            assert!(
                !this
                    .sidebar
                    .read(cx)
                    .entries()
                    .iter()
                    .any(|entry| matches!(entry, SidebarEntry::OtherTask(_)))
            );
            this.set_sidebar_filter("renamed remote project", cx);
            assert!(
                this.sidebar
                    .read(cx)
                    .entries()
                    .iter()
                    .any(|entry| matches!(entry, SidebarEntry::OtherTask(_)))
            );
            assert!(
                !this
                    .sidebar
                    .read(cx)
                    .entries()
                    .iter()
                    .any(|entry| matches!(entry, SidebarEntry::Task(_)))
            );
            this.set_sidebar_filter("", cx);
        });
        press(cx, "root-picker");
        press(cx, "computer-1-project-0");
        assert_eq!(
            requests.borrow().last(),
            Some(&Navigate {
                computer: Some("peer".into()),
                destination: Destination::Project("/tmp/proj".into()),
            })
        );
    }

    #[gpui::test]
    fn navigation_before_bootstrap_keeps_only_the_latest_request(cx: &mut TestAppContext) {
        use super::super::computers::Destination;
        let chat = screen(cx);
        chat.update(cx, |this, cx| {
            this.navigate(Destination::Project("/obsolete".into()), cx);
            this.navigate(Destination::Overview, cx);
            assert_eq!(
                this.pending_navigation,
                Some((this.selection_generation, Destination::Overview))
            );
            this.finish_navigation_bootstrap(cx);
            assert!(this.pending_navigation.is_none());
            assert!(!this.root_selecting);
            assert_eq!(this.sidebar.read(cx).project_filter(), None);
        });
    }

    #[gpui::test]
    fn new_draft_supersedes_a_navigation_waiting_for_the_catalog(cx: &mut TestAppContext) {
        let chat = screen(cx);
        chat.update(cx, |this, cx| {
            this.navigate(Destination::Task("earlier-click".into()), cx);
            this.new_session(cx);
            this.finish_navigation_bootstrap(cx);
            assert!(this.pending_navigation.is_none());
            assert!(this.selected_session.is_none());
            assert!(this.loading_session.is_none());
            assert!(this.draft);
        });
    }

    #[gpui::test]
    fn persisted_rows_with_repeated_legacy_keys_keep_distinct_view_identities(
        cx: &mut TestAppContext,
    ) {
        let chat = screen(cx);
        chat.update(cx, |this, _cx| {
            for row_seq in 1..=2 {
                let mut row = item("u7", "message", Some("same timestamp"));
                row.persisted = Some(maple_agent::agent::AgentPersistedRowIdentity {
                    task_id: "s1".into(),
                    rows_epoch: 0,
                    row_seq,
                    update_seq: row_seq,
                });
                this.apply_timeline_item("s1", row);
            }
            assert_eq!(this.timeline.len(), 2);
            assert_ne!(this.timeline[0].id, this.timeline[1].id);
            let mut live = item("u7", "message", Some("streamed"));
            live.live = Some(maple_agent::agent::AgentLiveRowIdentity {
                run_id: "run".into(),
                occurrence: 1,
            });
            this.apply_timeline_item("s1", live.clone());
            live.persisted = Some(maple_agent::agent::AgentPersistedRowIdentity {
                task_id: "s1".into(),
                rows_epoch: 0,
                row_seq: 3,
                update_seq: 3,
            });
            this.apply_timeline_item("s1", live);
            assert_eq!(this.timeline.len(), 3);
            assert_eq!(this.timeline[2].persisted.as_ref().unwrap().row_seq, 3);
        });
    }

    #[gpui::test]
    fn transcript_window_eviction_and_prepend_restore_the_visible_occurrence(
        cx: &mut TestAppContext,
    ) {
        let chat = screen(cx);
        chat.update(cx, |this, _cx| {
            let rows = (1..=2100)
                .map(|row_seq| {
                    let mut row = item("u7", "message", Some("history"));
                    row.persisted = Some(maple_agent::agent::AgentPersistedRowIdentity {
                        task_id: "s1".into(),
                        rows_epoch: 0,
                        row_seq,
                        update_seq: row_seq,
                    });
                    row
                })
                .collect();
            this.replace_timeline(rows);
            assert_eq!(this.timeline.len(), 2000);
            assert!(this.pages.before);
            this.list_state.scroll_to(gpui::ListOffset {
                item_ix: 10,
                offset_in_item: px(23.),
            });
            let anchor = this.scroll_anchor();
            let id = anchor.as_ref().unwrap().0.clone();
            let mut older = item("older", "message", Some("earlier"));
            older.id = "stored:s1:0:100".into();
            this.timeline.insert(0, older);
            this.bound_transcript_window(false);
            this.rebuild_window(anchor);
            let offset = this.list_state.logical_scroll_top();
            assert_eq!(this.timeline[offset.item_ix].id, id);
            assert_eq!(offset.offset_in_item, px(23.));
            assert!(this.pages.after);
            assert_eq!(this.timeline.len(), 2000);
        });
    }

    #[gpui::test]
    fn stale_commit_does_not_rekey_a_newer_live_occurrence(cx: &mut TestAppContext) {
        let chat = screen(cx);
        chat.update(cx, |this, _cx| {
            let mut live = item("legacy", "message", Some("newer live output"));
            live.revision = 20;
            live.live = Some(maple_agent::agent::AgentLiveRowIdentity {
                run_id: "run".into(),
                occurrence: 1,
            });
            this.apply_timeline_item("s1", live.clone());
            let key = this.timeline[0].id.clone();
            let mut old = live;
            old.revision = 19;
            old.persisted = Some(maple_agent::agent::AgentPersistedRowIdentity {
                task_id: "s1".into(),
                rows_epoch: 0,
                row_seq: 1,
                update_seq: 1,
            });
            assert!(this.apply_timeline_item("s1", old).is_none());
            assert_eq!(this.timeline[0].id, key);
            assert!(this.timeline[0].persisted.is_none());
            assert_eq!(this.timeline_index.get(&key).map(|entry| entry.0), Some(0));
        });
    }

    #[gpui::test]
    fn missed_live_occurrence_keeps_reload_available_at_the_committed_tail(
        cx: &mut TestAppContext,
    ) {
        let chat = screen(cx);
        chat.update(cx, |this, cx| {
            this.replace_timeline(vec![item("history", "message", Some("older history"))]);
            this.pages.after = true;
            let mut live = item("waiting-tool", "tool", Some("waiting for an answer"));
            live.revision = 100;
            live.live = Some(maple_agent::agent::AgentLiveRowIdentity {
                run_id: "run".into(),
                occurrence: 42,
            });
            this.apply_incoming_item("s1", live, cx);
            assert_eq!(this.timeline.len(), 1);
            assert!(this.pages.live_has_more);
            // This is the final forward-page transition. It cannot prove a
            // paused, uncommitted occurrence was recovered from durable rows.
            this.pages.after = false;
            assert!(this.pages.live_has_more);
            let reload = this.reload_generation;
            this.reload_paged_latest(cx);
            assert!(this.reload_generation > reload);
            assert!(
                this.pages.live_has_more,
                "only a successful bounded open clears the notice"
            );
        });
    }

    fn item(id: &str, item_type: &str, text: Option<&str>) -> AgentTimelineItem {
        AgentTimelineItem {
            persisted: None,
            live: None,
            truncated: None,
            revision: 0,
            id: id.to_string(),
            item_type: item_type.to_string(),
            role: None,
            title: None,
            text: text.map(str::to_string),
            status: None,
            input: None,
            output: None,
            created_ms: 0,
            merge: "replace".to_string(),
        }
    }

    fn user_item(id: &str, text: &str) -> AgentTimelineItem {
        AgentTimelineItem {
            role: Some("user".to_string()),
            ..item(id, "message", Some(text))
        }
    }

    /// Serializes constructions that read the shared settings snapshot while
    /// another fixture installs its own host-backed settings cache.
    use crate::settings::SETTINGS_IO_LOCK as SETTINGS_LOCK;

    fn screen(cx: &mut TestAppContext) -> Entity<ChatScreen> {
        let _guard = SETTINGS_LOCK.lock();
        let backend = std::sync::Arc::new(
            crate::backend::AgentBackend::for_test("http://127.0.0.1:9".to_string(), String::new())
                .expect("backend"),
        );
        // This gpui's test scheduler flags activity on other threads unless
        // parking is allowed; the backend runtime and image encoder run on tokio.
        cx.executor().allow_parking();
        let screen = cx.new(|cx| {
            let mut screen = ChatScreen::new_inner(backend, "user".to_string(), cx);
            screen.selected_session = Some("s1".to_string());
            screen
        });
        // Sections must not depend on presentation preferences left in the
        // shared settings cache by an earlier fixture.
        screen.update(cx, |this, cx| {
            this.sidebar
                .update(cx, |sidebar, _| sidebar.reset_persisted_for_test());
        });
        screen
    }

    /// A chat screen in an active window, with the shipped key bindings.
    /// `setup` runs on the screen before the first frame.
    fn chat_window(
        cx: &mut TestAppContext,
        setup: impl FnOnce(&mut ChatScreen, &mut Context<ChatScreen>),
    ) -> (Entity<ChatScreen>, &mut gpui::VisualTestContext) {
        let backend = {
            let _guard = SETTINGS_LOCK.lock();
            std::sync::Arc::new(
                crate::backend::AgentBackend::for_test(
                    "http://127.0.0.1:9".to_string(),
                    String::new(),
                )
                .expect("backend"),
            )
        };
        chat_window_in(cx, backend, setup)
    }

    /// `chat_window` on a backend the test built, for tests that read
    /// something back through it.
    fn chat_window_in(
        cx: &mut TestAppContext,
        backend: Arc<crate::backend::AgentBackend>,
        setup: impl FnOnce(&mut ChatScreen, &mut Context<ChatScreen>),
    ) -> (Entity<ChatScreen>, &mut gpui::VisualTestContext) {
        struct ChatHost {
            chat: Entity<ChatScreen>,
        }
        impl Render for ChatHost {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut Context<Self>,
            ) -> impl IntoElement {
                // A column, so the screen fills the window and the sidebar
                // list has room for its rows.
                div()
                    .w(px(1200.))
                    .h(px(800.))
                    .flex()
                    .flex_col()
                    .child(self.chat.clone())
            }
        }
        cx.executor().allow_parking();
        let chat = cx.new(|cx| {
            let _guard = SETTINGS_LOCK.lock();
            crate::desktop::register_key_bindings(cx);
            let mut chat = ChatScreen::new_without_start(backend, "user".to_string(), cx);
            chat.selected_session = Some("s1".to_string());
            chat.booting = false;
            chat.trust_prompts = false;
            chat.sidebar
                .update(cx, |sidebar, _| sidebar.reset_persisted_for_test());
            setup(&mut chat, cx);
            chat
        });
        let (_host, cx) = cx.add_window_view(|_window, _cx| ChatHost { chat: chat.clone() });
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        // Focus events carry no path while the window is inactive.
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        (chat, cx)
    }

    fn todo_item(id: &str, todos: serde_json::Value) -> AgentTimelineItem {
        AgentTimelineItem {
            status: Some("completed".to_string()),
            input: Some(serde_json::json!({ "todos": todos })),
            ..item(id, "tool", None)
        }
    }

    /// The subagent card shows one row per live `delegate` call, follows
    /// the tool each subagent runs, and empties when they end.
    #[gpui::test]
    fn test_subagent_card_tracks_live_delegates(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        use maple_agent::agent::AgentRunEvent;

        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            assert!(this.render_subagents_card(cx).is_none());
            this.handle_run_event(
                "s1",
                "r1",
                AgentRunEvent::SubagentStarted {
                    id: "d1".to_string(),
                    task: "Review the parser".to_string(),
                    background: false,
                    external: None,
                },
                cx,
            );
            assert_eq!(this.subagents.len(), 1);
            assert!(this.render_subagents_card(cx).is_some());

            // A subagent of a task that is not on screen stays off it.
            this.handle_run_event(
                "s2",
                "r2",
                AgentRunEvent::SubagentStarted {
                    id: "d2".to_string(),
                    task: "Other task".to_string(),
                    background: true,
                    external: None,
                },
                cx,
            );
            assert_eq!(this.subagents.len(), 1);

            this.handle_run_event(
                "s1",
                "r1",
                AgentRunEvent::SubagentActivity {
                    id: "d1".to_string(),
                    tool: "Terminal: cargo test".to_string(),
                },
                cx,
            );
            assert_eq!(
                this.subagents[0]
                    .activity
                    .as_ref()
                    .map(SharedString::as_ref),
                Some("Terminal: cargo test")
            );
            // The same tool again changes nothing, so nothing repaints.
            assert!(!this.handle_run_event(
                "s1",
                "r1",
                AgentRunEvent::SubagentActivity {
                    id: "d1".to_string(),
                    tool: "Terminal: cargo test".to_string(),
                },
                cx,
            ));

            this.handle_run_event(
                "s1",
                "r1",
                AgentRunEvent::SubagentFinished {
                    id: "d1".to_string(),
                },
                cx,
            );
            assert!(this.subagents.is_empty());
            assert!(this.render_subagents_card(cx).is_none());
        });
    }

    /// A turn that ends takes the subagents it was waiting for with it,
    /// but a background subagent keeps working and keeps its row.
    #[gpui::test]
    fn test_run_end_keeps_background_subagents(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        use maple_agent::agent::{AgentRunEvent, AgentRunTerminal, AgentSubagent};

        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.active_runs.insert("s1".to_string(), "r1".to_string());
            for (id, background) in [("waited-for", false), ("in-background", true)] {
                this.handle_run_event(
                    "s1",
                    "r1",
                    AgentRunEvent::SubagentStarted {
                        id: id.to_string(),
                        task: id.to_string(),
                        background,
                        external: None,
                    },
                    cx,
                );
            }
            assert_eq!(this.subagents.len(), 2);

            this.handle_run_event(
                "s1",
                "r1",
                AgentRunEvent::Finished(AgentRunTerminal::Completed),
                cx,
            );
            assert_eq!(this.subagents.len(), 1);
            assert_eq!(this.subagents[0].id, "in-background");
            assert!(this.render_subagents_card(cx).is_some());

            // Opening the task later rebuilds the card from the runtime.
            this.set_subagents(
                vec![AgentSubagent {
                    id: "in-background".to_string(),
                    task: "Build the release".to_string(),
                    background: true,
                    elapsed_ms: 90_000,
                    activity: Some("Terminal: cargo build".to_string()),
                    external: None,
                }],
                cx,
            );
            assert_eq!(this.subagents.len(), 1);
            assert_eq!(
                this.subagents[0]
                    .activity
                    .as_ref()
                    .map(SharedString::as_ref),
                Some("Terminal: cargo build")
            );
            // An elapsed time already on screen is kept, so it cannot
            // jump backwards when the snapshot lands.
            assert!(this.subagents[0].started.elapsed() < std::time::Duration::from_secs(60));

            // An empty snapshot for a task with no subagent clears the card.
            this.set_subagents(Vec::new(), cx);
            assert!(this.subagents.is_empty());
            assert!(this.render_subagents_card(cx).is_none());
        });
    }

    /// A snapshot describes the moment it was asked for. A row that an
    /// event added while the snapshot was in flight must survive it;
    /// nothing re-creates a wiped row until the task is reopened.
    #[gpui::test]
    fn test_stale_subagent_snapshot_cannot_wipe_a_live_row(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        use maple_agent::agent::AgentRunEvent;

        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            let requested_at = this.subagent_epoch;
            // The delegate fires while the snapshot is still in flight.
            this.handle_run_event(
                "s1",
                "r1",
                AgentRunEvent::SubagentStarted {
                    id: "d1".to_string(),
                    task: "Review the parser".to_string(),
                    background: false,
                    external: None,
                },
                cx,
            );
            // The snapshot predates the row, so it must not remove it.
            this.apply_subagent_snapshot(requested_at, Vec::new(), cx);
            assert_eq!(this.subagents.len(), 1);

            // A snapshot from the current epoch is authoritative.
            let requested_at = this.subagent_epoch;
            this.apply_subagent_snapshot(requested_at, Vec::new(), cx);
            assert!(this.subagents.is_empty());
        });
    }

    /// Elapsed times read as minutes and seconds, and add hours only
    /// when a subagent has worked that long.
    #[test]
    fn test_subagent_elapsed_reads_as_a_clock() {
        use crate::ui::chat::transcript::format_subagent_elapsed;
        use std::time::Duration;

        assert_eq!(format_subagent_elapsed(Duration::from_secs(7)), "0:07");
        assert_eq!(format_subagent_elapsed(Duration::from_secs(605)), "10:05");
        assert_eq!(
            format_subagent_elapsed(Duration::from_secs(3661)),
            "1:01:01"
        );
    }

    /// The pinned plan tracks the newest todo_write list: it follows
    /// incoming items in order, and a loaded history uses its last list.
    #[gpui::test]
    fn test_plan_follows_latest_todo_list(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            assert!(this.plan.is_empty());
            assert!(this.render_plan_card(cx).is_none());
            this.apply_incoming_item(
                "s1",
                todo_item(
                    "t1",
                    serde_json::json!([{ "content": "one", "status": "in_progress" }]),
                ),
                cx,
            );
            assert_eq!(this.plan.len(), 1);
            assert_eq!(this.plan[0].status, PlanStatus::InProgress);
            // A plain tool call leaves the plan alone.
            this.apply_incoming_item("s1", item("x", "tool", None), cx);
            assert_eq!(this.plan.len(), 1);
            this.apply_incoming_item(
                "s1",
                todo_item(
                    "t2",
                    serde_json::json!([
                        { "content": "one", "status": "completed" },
                        { "content": "two", "status": "pending" }
                    ]),
                ),
                cx,
            );
            assert_eq!(this.plan.len(), 2);
            assert_eq!(this.plan[0].status, PlanStatus::Completed);
            assert!(this.render_plan_card(cx).is_some());

            this.replace_timeline(vec![
                todo_item("a", serde_json::json!([{ "content": "old" }])),
                item("b", "message", Some("hi")),
                todo_item(
                    "c",
                    serde_json::json!([{ "content": "new" }, { "content": "newer" }]),
                ),
            ]);
            assert_eq!(this.plan.len(), 2);
            assert_eq!(this.plan[0].content.as_ref(), "new");
            this.replace_timeline(Vec::new());
            assert!(this.plan.is_empty());
        });
    }

    /// A persisted settings file must shape a freshly built chat screen:
    /// tool cards collapsed and web off for new tasks without visiting
    /// the settings screen first.
    #[gpui::test]
    fn test_constructor_reads_persisted_defaults(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let _guard = SETTINGS_LOCK.lock();
        let previous = crate::settings::load_settings();
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(
            maple_agent::store::Store::open(maple_agent::store::StorePaths::new(directory.path()))
                .unwrap(),
        );
        store
            .set_app_setting(
                "ui",
                &serde_json::json!({"tool_details":false,"default_web_enabled":false}),
                1,
            )
            .unwrap();
        crate::settings::install_test_store(store);
        // This gpui's test scheduler flags activity on other threads unless
        // parking is allowed; the backend runtime and image encoder run on tokio.
        cx.executor().allow_parking();
        let screen = cx.new(|cx| {
            ChatScreen::new_inner(
                std::sync::Arc::new(
                    crate::backend::AgentBackend::for_test(
                        "http://127.0.0.1:9".to_string(),
                        String::new(),
                    )
                    .expect("backend"),
                ),
                "user".to_string(),
                cx,
            )
        });
        crate::settings::install_test_settings(previous);
        screen.update(cx, |this, _cx| {
            assert!(!this.tool_details);
            assert!(!this.default_web_enabled);
        });
    }

    #[gpui::test]
    fn test_archived_tasks_leave_the_sections(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.sessions = vec![summary("s1", "Live"), summary("s2", "Old")];
            this.sessions[1].state = AgentTaskState::Archived;
            this.sync_sidebar(cx);
            assert_eq!(this.sidebar_active(cx), vec![0]);
            assert_eq!(this.archived_indices(cx), vec![1]);

            this.sessions[0].state = AgentTaskState::Archived;
            this.sync_sidebar(cx);
            assert!(this.sidebar_active(cx).is_empty());
            assert_eq!(this.archived_indices(cx), vec![0, 1]);
        });
    }

    #[gpui::test]
    fn test_timeline_appends_streamed_text(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, _cx| {
            this.apply_timeline_item("s1", item("m1", "message", Some("Hel")));
            this.apply_timeline_item(
                "s1",
                AgentTimelineItem {
                    merge: "append".to_string(),
                    ..item("m1", "message", Some("lo"))
                },
            );
            assert_eq!(this.timeline.len(), 1);
            assert_eq!(this.timeline[0].text.as_deref(), Some("Hello"));
        });
    }

    #[gpui::test]
    fn test_timeline_field_merge_keeps_prior_payloads(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, _cx| {
            let mut tool = item("t1", "tool", None);
            tool.title = Some("edit".to_string());
            tool.input = Some(serde_json::json!({"edits": []}));
            this.apply_timeline_item("s1", tool);
            // Completion row carries only a status; fields must survive.
            this.apply_timeline_item(
                "s1",
                AgentTimelineItem {
                    status: Some("completed".to_string()),
                    ..item("t1", "tool", None)
                },
            );
            assert_eq!(this.timeline.len(), 1);
            assert_eq!(this.timeline[0].title.as_deref(), Some("edit"));
            assert!(this.timeline[0].input.is_some());
            assert_eq!(this.timeline[0].status.as_deref(), Some("completed"));
        });
    }

    #[gpui::test]
    fn test_events_from_other_sessions_are_ignored(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        let event = AgentServiceEvent::TimelineItem {
            session_id: "other".to_string(),
            run_id: None,
            item: item("m9", "message", Some("alien")),
        };
        screen.update(cx, |this, cx| {
            this.handle_service_event(event, cx);
            assert!(this.timeline.is_empty());
            // The revision still moves so a snapshot load of that session
            // in flight retries instead of applying a stale snapshot.
            assert_eq!(this.timeline_revisions.get("other"), Some(&1));
            assert!(!this.timeline_revisions.contains_key("s1"));
            this.handle_run_event(
                "other",
                "run-1",
                maple_agent::agent::AgentRunEvent::TimelineItem(item(
                    "m10",
                    "message",
                    Some("more"),
                )),
                cx,
            );
            assert_eq!(this.timeline_revisions.get("other"), Some(&2));
            assert!(this.timeline.is_empty());
        });
    }

    #[gpui::test]
    fn test_session_upsert_never_duplicates(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.upsert_session(summary("s1", "A"), cx);
            this.upsert_session(summary("s1", "A2"), cx);
            this.upsert_session(summary("s2", "B"), cx);
            assert_eq!(this.sessions.len(), 2);
            // New sessions prepend; updates happen in place.
            assert_eq!(this.sessions[0].title, "B");
            assert_eq!(this.sessions[1].title, "A2");
        });
    }

    fn one_question(id: &str, text: &str) -> AgentServiceEvent {
        AgentServiceEvent::Question {
            session_id: "s1".to_string(),
            request_id: format!("req-{id}"),
            questions: vec![maple_agent::agent::AgentQuestion {
                multi_select: false,
                id: id.to_string(),
                header: "Question".to_string(),
                question: text.to_string(),
                options: Vec::new(),
            }],
        }
    }

    #[gpui::test]
    fn recovery_snapshot_replaces_old_host_revisions_and_reconciles_live_events(
        cx: &mut TestAppContext,
    ) {
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            let mut old = item("reply", "message", Some("old partial"));
            old.revision = 900;
            this.replace_timeline(vec![old]);
            this.snapshot_revision = 900;
            this.active_runs.insert("s1".into(), "old-run".into());
            this.handle_service_event(one_question("old", "old question"), cx);
            this.handle_service_event(AgentServiceEvent::HostReconnecting, cx);
            assert!(this.host_recovery.unavailable);
            // Model a subscribed replacement host and updates arriving while
            // its snapshot is in flight. The new host starts revisions at 1.
            this.host_recovery.events = Some(Vec::new());
            let mut latest = item("reply", "message", Some("new complete reply"));
            latest.revision = 3;
            this.handle_service_event(
                AgentServiceEvent::TimelineItem {
                    session_id: "s1".into(),
                    run_id: None,
                    item: latest.clone(),
                },
                cx,
            );
            this.handle_service_event(
                AgentServiceEvent::QuestionResolved {
                    request_id: "req-current".into(),
                },
                cx,
            );
            let events = this.host_recovery.events.take().unwrap();
            let mut snapshot_row = latest.clone();
            snapshot_row.revision = 2;
            snapshot_row.text = Some("new partial".into());
            let selected = summary("s1", "Recovered title");
            this.apply_recovery_snapshot(
                crate::backend::RecoverySnapshot {
                    bootstrap: crate::backend::LocalBootstrap {
                        project_root: None,
                        recent_roots: Vec::new(),
                        latest: None,
                        sessions: vec![selected.clone()],
                        questions: vec![one_question("current", "new question")],
                    },
                    status: maple_agent::agent::AgentRuntimeStatus {
                        running: true,
                        project_root: None,
                        model: None,
                        active_runs: HashMap::new(),
                    },
                    selected: Some(maple_agent::agent::AgentSessionDetail {
                        timeline_revision: 2,
                        session: selected,
                        timeline: vec![snapshot_row],
                        mcp_errors: Vec::new(),
                        queue: maple_agent::agent::AgentDesktopQueueSnapshot {
                            revision: 1,
                            items: vec![maple_agent::agent::AgentQueuedMessage {
                                queue_id: "queued".into(),
                                message_id: "queued-message".into(),
                                session_id: "s1".into(),
                                text: "pending message".into(),
                                attachments: Vec::new(),
                                created_ms: 0,
                            }],
                        },
                    }),
                },
                this.selection_generation,
                cx,
            );
            this.handle_service_events(events, cx);
            assert_eq!(this.timeline.len(), 1);
            assert_eq!(this.timeline[0].text.as_deref(), Some("new complete reply"));
            assert_eq!(this.timeline[0].revision, 3);
            assert!(this.active_runs.is_empty());
            assert!(
                this.pending_questions.is_empty(),
                "resolved questions must not reappear"
            );
            assert_eq!(this.queue[0].text, "pending message");
            assert_eq!(this.sessions[0].title, "Recovered title");
            this.handle_service_event(
                AgentServiceEvent::TimelineItem {
                    session_id: "s1".into(),
                    run_id: None,
                    item: latest,
                },
                cx,
            );
            assert_eq!(
                this.timeline.len(),
                1,
                "snapshot/event overlap must not duplicate rows"
            );
        });
    }

    #[gpui::test]
    fn store_maintenance_preserves_the_draft_and_cancels_an_unsent_catalog_wait(
        cx: &mut TestAppContext,
    ) {
        let (chat, cx) = chat_window(cx, |this, _| {
            this.booting = false;
        });
        chat.update(cx, |this, cx| {
            this.fill_empty_composer("keep this unsent draft", cx);
            this.session_setup_pending = true;
            this.first_send_waiting_for_mcp = true;
            this.pending_first_send = Some(FirstSend::Message {
                text: "keep this unsent draft".into(),
                steer: false,
            });
            this.begin_store_maintenance(cx);
            let epoch = this.host_recovery.epoch;
            this.begin_store_maintenance(cx);
            assert_eq!(
                this.host_recovery.epoch, epoch,
                "duplicate barriers coalesce"
            );
            assert!(this.host_recovery.unavailable);
            assert!(this.booting);
            assert!(!this.first_send_waiting_for_mcp);
            assert!(!this.session_setup_pending);
            assert!(this.pending_first_send.is_none());
            this.handle_service_event(AgentServiceEvent::HostReconnected, cx);
            assert!(
                this.host_recovery.events.is_none(),
                "wait for verified Open"
            );
            this.send_inner(cx);
            assert!(
                this.pending_first_send.is_none(),
                "maintenance cannot submit a draft"
            );
            assert_eq!(
                this.composer_text(cx).as_deref(),
                Some("keep this unsent draft")
            );
        });
    }

    #[gpui::test]
    fn reconnecting_keeps_composer_text_and_bounds_snapshot_event_buffer(cx: &mut TestAppContext) {
        let (screen, cx) = chat_window(cx, |this, _| {
            this.booting = false;
        });
        screen.update(cx, |this, cx| {
            this.fill_empty_composer("do not automatically send this", cx);
            this.handle_service_event(AgentServiceEvent::HostReconnecting, cx);
            this.send_inner(cx);
            assert_eq!(
                this.composer_text(cx).as_deref(),
                Some("do not automatically send this")
            );
            assert!(!this.session_setup_pending);
            this.host_recovery.events = Some(Vec::new());
            for _ in 0..300 {
                this.handle_service_event(
                    AgentServiceEvent::QuestionResolved {
                        request_id: "fixture".into(),
                    },
                    cx,
                );
            }
            assert_eq!(this.host_recovery.events.as_ref().unwrap().len(), 256);
            assert!(this.host_recovery.overflow);
        });
    }

    #[gpui::test]
    fn test_question_event_sets_pending_card(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            assert!(this.pending_questions.is_empty());
            this.handle_service_event(one_question("color", "Favorite color?"), cx);
            let question = this.pending_questions.first().expect("question set");
            assert_eq!(question.questions.len(), 1);
            assert_eq!(question.questions[0].id, "color");
            assert!(this.pending_question_input.is_some());
        });
    }

    #[gpui::test]
    fn test_question_options_select_and_compose(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        let event = AgentServiceEvent::Question {
            session_id: "s1".to_string(),
            request_id: "q2".to_string(),
            questions: vec![maple_agent::agent::AgentQuestion {
                multi_select: false,
                id: "pick".to_string(),
                header: "Pick".to_string(),
                question: "Pick one".to_string(),
                options: vec![
                    maple_agent::agent::AgentQuestionOption {
                        label: "A".to_string(),
                        description: "First".to_string(),
                    },
                    maple_agent::agent::AgentQuestionOption {
                        label: "B".to_string(),
                        description: "Second".to_string(),
                    },
                ],
            }],
        };
        screen.update(cx, |this, cx| {
            this.handle_service_event(event, cx);
            // Repicking replaces the selection (single-select).
            this.select_question_option(0, 1, cx);
            this.select_question_option(0, 0, cx);
            let answer = this.composed_question_answer(cx);
            let parsed: serde_json::Value = serde_json::from_str(&answer).unwrap();
            assert_eq!(parsed["answers"]["pick"]["answers"][0], "A");
        });
    }

    #[gpui::test]
    fn test_multi_select_preserves_choices_notes_and_question_steps(cx: &mut TestAppContext) {
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            let question = |id: &str, multi_select| maple_agent::agent::AgentQuestion {
                id: id.into(),
                header: "Choose".into(),
                question: "Which options?".into(),
                multi_select,
                options: ["A", "B", "C"]
                    .into_iter()
                    .map(|label| maple_agent::agent::AgentQuestionOption {
                        label: label.into(),
                        description: String::new(),
                    })
                    .collect(),
            };
            this.handle_service_event(
                AgentServiceEvent::Question {
                    session_id: "s1".into(),
                    request_id: "multi".into(),
                    questions: vec![question("many", true), question("one", false)],
                },
                cx,
            );
            this.toggle_question_option(0, 1, cx);
            this.toggle_question_option(0, 0, cx);
            assert_eq!(this.question_selected[&0].picked, BTreeSet::from([0, 1]));
            // Number shortcuts toggle without prematurely sending the answer.
            this.pick_and_submit_question_option(0, cx);
            assert_eq!(this.question_step, 0);
            assert!(this.question_step_answers.is_empty());
            assert_eq!(this.question_selected[&0].picked, BTreeSet::from([1]));
            // Moving the Vim cursor must not check the options it passes.
            this.focus_question_option(0, 2, cx);
            this.focus_question_option(0, 0, cx);
            assert_eq!(this.question_selected[&0].picked, BTreeSet::from([1]));
            this.pick_and_submit_question_option(0, cx);
            this.pending_question_input
                .clone()
                .unwrap()
                .update(cx, |input, cx| input.set_text("  on Linux  ", cx));
            this.submit_question(cx);
            assert_eq!(this.question_step, 1);
            assert!(this.question_selected.is_empty());
            assert_eq!(
                this.question_step_answers[&0],
                ["A", "B", "Additional note: on Linux"]
            );
            this.toggle_question_option(1, 0, cx);
            this.toggle_question_option(1, 2, cx);
            let answer: serde_json::Value =
                serde_json::from_str(&this.composed_question_answer(cx)).unwrap();
            assert_eq!(
                answer["answers"]["many"]["answers"],
                serde_json::json!(["A", "B", "Additional note: on Linux"])
            );
            assert_eq!(
                answer["answers"]["one"]["answers"],
                serde_json::json!(["C"])
            );
        });
    }

    /// Run this ignored fixture alone on a private Linux desktop. It mounts
    /// the real chat screen without credentials or a live inference request.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "interactive native question-card fixture; requires a private display"]
    fn native_question_card_fixture() {
        gpui_platform::application()
            .with_assets(crate::assets::Assets)
            .run(|cx| {
                cx.text_system()
                    .add_fonts(
                        crate::assets::FONTS
                            .iter()
                            .map(|font| std::borrow::Cow::Borrowed(*font))
                            .collect(),
                    )
                    .unwrap();
                let _shortcuts =
                    crate::shortcuts::ShortcutRuntime::bootstrap(&Default::default(), cx);
                theme::apply_preference(
                    theme::Preference::parse(&crate::settings::load_settings().theme),
                    cx,
                );
                let backend = Arc::new(
                    AgentBackend::for_test("http://127.0.0.1:9".into(), String::new()).unwrap(),
                );
                cx.open_window(
                    gpui::WindowOptions {
                        window_bounds: Some(gpui::WindowBounds::Windowed(gpui::Bounds::centered(
                            None,
                            gpui::size(px(1100.), px(800.)),
                            cx,
                        ))),
                        titlebar: Some(gpui::TitlebarOptions {
                            title: Some("Question card fixture".into()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    |window, cx| {
                        theme::resolve(window.appearance());
                        cx.new(|cx| {
                            let mut chat =
                                ChatScreen::new_mounted(backend, "fixture-user".into(), cx);
                            chat.booting = false;
                            chat.selected_session = Some("s1".into());
                            chat.replace_timeline(vec![user_item(
                                "prompt",
                                "Choose which checks to run.",
                            )]);
                            chat.handle_service_event(
                                AgentServiceEvent::Question {
                                    session_id: "s1".into(),
                                    request_id: "fixture".into(),
                                    questions: [true, false]
                                        .into_iter()
                                        .enumerate()
                                        .map(|(index, multi_select)| {
                                            maple_agent::agent::AgentQuestion {
                                                id: format!("q{index}"),
                                                header: "Validation".into(),
                                                multi_select,
                                                question: if multi_select {
                                                    "Which checks should run?"
                                                } else {
                                                    "Which check should run first?"
                                                }
                                                .into(),
                                                options: [
                                                    "Unit tests",
                                                    "Integration tests",
                                                    "Native UI checks",
                                                ]
                                                .into_iter()
                                                .map(|label| {
                                                    maple_agent::agent::AgentQuestionOption {
                                                        label: label.into(),
                                                        description: String::new(),
                                                    }
                                                })
                                                .collect(),
                                            }
                                        })
                                        .collect(),
                                },
                                cx,
                            );
                            chat
                        })
                    },
                )
                .unwrap();
                cx.on_window_closed(|cx, _| {
                    if cx.windows().is_empty() {
                        cx.quit();
                    }
                })
                .detach();
                cx.activate(true);
            });
    }

    #[gpui::test]
    fn test_question_typed_text_rides_along_with_picked_option(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        let event = AgentServiceEvent::Question {
            session_id: "s1".to_string(),
            request_id: "q3".to_string(),
            questions: vec![maple_agent::agent::AgentQuestion {
                multi_select: false,
                id: "pick".to_string(),
                header: "Pick".to_string(),
                question: "Pick one".to_string(),
                options: vec![maple_agent::agent::AgentQuestionOption {
                    label: "A".to_string(),
                    description: "First".to_string(),
                }],
            }],
        };
        screen.update(cx, |this, cx| {
            this.handle_service_event(event, cx);
            this.select_question_option(0, 0, cx);
            let input = this.pending_question_input.clone().expect("input exists");
            input.update(cx, |input, cx| input.set_text("  but only on Linux  ", cx));
            let answer = this.composed_question_answer(cx);
            let parsed: serde_json::Value = serde_json::from_str(&answer).unwrap();
            let answers = parsed["answers"]["pick"]["answers"].as_array().unwrap();
            assert_eq!(answers.len(), 2);
            assert_eq!(answers[0], "A");
            assert_eq!(answers[1], "Additional note: but only on Linux");
        });
    }

    #[gpui::test]
    fn test_question_option_click_toggles_off(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        let event = AgentServiceEvent::Question {
            session_id: "s1".to_string(),
            request_id: "q4".to_string(),
            questions: vec![maple_agent::agent::AgentQuestion {
                multi_select: false,
                id: "pick".to_string(),
                header: "Pick".to_string(),
                question: "Pick one".to_string(),
                options: vec![maple_agent::agent::AgentQuestionOption {
                    label: "A".to_string(),
                    description: "First".to_string(),
                }],
            }],
        };
        screen.update(cx, |this, cx| {
            this.handle_service_event(event, cx);
            // Clicking the picked option again clears it; the typed text
            // then stands alone.
            this.toggle_question_option(0, 0, cx);
            assert_eq!(this.question_selected[&0].picked, BTreeSet::from([0]));
            this.toggle_question_option(0, 0, cx);
            assert!(this.question_selected.is_empty());
            let input = this.pending_question_input.clone().expect("input exists");
            input.update(cx, |input, cx| input.set_text("something else", cx));
            let answer = this.composed_question_answer(cx);
            let parsed: serde_json::Value = serde_json::from_str(&answer).unwrap();
            assert_eq!(parsed["answers"]["pick"]["answers"][0], "something else");
        });
    }

    #[gpui::test]
    fn test_sidebar_filter_hides_non_matching_tasks_and_projects(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            let mut a = summary("s1", "Fix login bug");
            a.project_root = "/work/alpha".to_string();
            let mut b = summary("s2", "Write docs");
            b.project_root = "/work/beta".to_string();
            let mut c = summary("s3", "Old login task");
            c.project_root = "/work/beta".to_string();
            c.state = AgentTaskState::Archived;
            this.sessions = vec![a, b, c];
            this.sync_sidebar(cx);
            assert_eq!(this.sidebar_active(cx).len(), 2);
            assert_eq!(this.archived_indices(cx), vec![2]);

            this.set_sidebar_filter("login", cx);
            // Only the matching task stays visible.
            assert_eq!(this.sidebar_active(cx), vec![0]);
            // Archived rows are searched too.
            assert_eq!(this.archived_indices(cx), vec![2]);

            // A project name matches all of its tasks.
            this.set_sidebar_filter("beta", cx);
            assert_eq!(this.sidebar_active(cx), vec![1]);

            this.set_sidebar_filter("", cx);
            assert_eq!(this.sidebar_active(cx).len(), 2);
        });
    }

    #[gpui::test]
    fn test_prompt_history_recall(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, _| {
            this.remember_prompt("first");
            this.remember_prompt("second");
            // Up from an empty composer walks back; Down returns to the draft.
            assert_eq!(this.recall_prompt("up", ""), Some("second".into()));
            assert_eq!(this.recall_prompt("up", "second"), Some("first".into()));
            assert_eq!(this.recall_prompt("up", "first"), None);
            assert_eq!(this.recall_prompt("down", "first"), Some("second".into()));
            assert_eq!(this.recall_prompt("down", "second"), Some(String::new()));
            // Typed text that is not a recalled prompt keeps the caret keys.
            assert_eq!(this.recall_prompt("up", "typing"), None);
            // Re-sending an old prompt moves it to the end.
            this.remember_prompt("first");
            assert_eq!(this.recall_prompt("up", ""), Some("first".into()));
        });
    }

    #[gpui::test]
    fn test_skip_question_clears_card(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        let event = one_question("skip", "Skip me");
        screen.update(cx, |this, cx| {
            this.handle_service_event(event, cx);
            assert!(!this.pending_questions.is_empty());
            this.skip_question(cx);
            assert!(this.pending_questions.is_empty());
            assert!(this.question_selected.is_empty());
        });
    }

    #[gpui::test]
    fn test_first_agent_item_clears_waiting_state(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.awaiting_first_token = true;
            this.handle_run_event(
                "s1",
                "run-1",
                maple_agent::agent::AgentRunEvent::TimelineItem(user_item("u1", "hi")),
                cx,
            );
            // The user's own echo does not stop the waiting indicator.
            assert!(this.awaiting_first_token);
            this.handle_run_event(
                "s1",
                "run-1",
                maple_agent::agent::AgentRunEvent::TimelineItem(item("t1", "thinking", None)),
                cx,
            );
            assert!(!this.awaiting_first_token);
        });
    }

    #[gpui::test]
    fn test_send_closes_chip_menus(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.selected_session = Some("s1".to_string());
            this.booting = false;
            this.popup.open(ChatPopup::Model, cx);
            this.send_text("hello".to_string(), cx);
            assert_eq!(this.popup.open_key(), None);
        });
    }

    #[gpui::test]
    fn test_multi_question_batch_steps_through(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        let event = AgentServiceEvent::Question {
            session_id: "s1".to_string(),
            request_id: "batch".to_string(),
            questions: vec![
                maple_agent::agent::AgentQuestion {
                    multi_select: false,
                    id: "first".to_string(),
                    header: "One".to_string(),
                    question: "First?".to_string(),
                    options: vec![maple_agent::agent::AgentQuestionOption {
                        label: "Yes".to_string(),
                        description: String::new(),
                    }],
                },
                maple_agent::agent::AgentQuestion {
                    multi_select: false,
                    id: "second".to_string(),
                    header: "Two".to_string(),
                    question: "Second?".to_string(),
                    options: vec![maple_agent::agent::AgentQuestionOption {
                        label: "No".to_string(),
                        description: String::new(),
                    }],
                },
            ],
        };
        screen.update(cx, |this, cx| {
            this.handle_service_event(event, cx);
            assert_eq!(this.question_step, 0);
            // Answer step one: the batch stays on the card, advanced.
            this.select_question_option(0, 0, cx);
            this.submit_question(cx);
            assert_eq!(this.pending_questions.len(), 1);
            assert_eq!(this.question_step, 1);
            // Answer step two: the queue pops and both answers are recorded.
            this.select_question_option(1, 0, cx);
            this.submit_question(cx);
            assert!(this.pending_questions.is_empty());
            assert_eq!(this.question_step, 0);
            assert!(this.question_step_answers.is_empty());
        });
    }

    #[gpui::test]
    fn test_parallel_questions_queue_and_advance(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        let question = |id: &str| AgentServiceEvent::Question {
            session_id: "s1".to_string(),
            request_id: id.to_string(),
            questions: vec![maple_agent::agent::AgentQuestion {
                multi_select: false,
                id: id.to_string(),
                header: "Question".to_string(),
                question: format!("Question {id}"),
                options: Vec::new(),
            }],
        };
        screen.update(cx, |this, cx| {
            this.handle_service_event(question("q1"), cx);
            this.handle_service_event(question("q2"), cx);
            this.handle_service_event(question("q3"), cx);
            // Duplicate delivery must not double-queue.
            this.handle_service_event(question("q2"), cx);
            assert_eq!(this.pending_questions.len(), 3);
            assert_eq!(this.pending_questions[0].request_id, "q1");
            // Answering pops the head and leaves the rest queued.
            this.answer_question("first".to_string(), cx);
            assert_eq!(this.pending_questions.len(), 2);
            assert_eq!(this.pending_questions[0].request_id, "q2");
            // Skipping pops the head too.
            this.skip_question(cx);
            assert_eq!(this.pending_questions.len(), 1);
            assert_eq!(this.pending_questions[0].request_id, "q3");
            // Every surfaced question must have an answer input; a card
            // without one is unanswerable.
            assert!(this.pending_question_input.is_some());
            this.answer_question("third".to_string(), cx);
            assert!(this.pending_questions.is_empty());
            assert!(this.pending_question_input.is_none());
        });
    }

    #[gpui::test]
    fn test_application_vim_enter_focuses_the_question_answer(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        struct ChatHost {
            chat: Entity<ChatScreen>,
        }
        impl Render for ChatHost {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut Context<Self>,
            ) -> impl IntoElement {
                div().w(px(1200.)).h(px(800.)).child(self.chat.clone())
            }
        }

        let chat = cx.new(|cx| {
            let _guard = SETTINGS_LOCK.lock();
            let backend = std::sync::Arc::new(
                crate::backend::AgentBackend::for_test(
                    "http://127.0.0.1:9".to_string(),
                    String::new(),
                )
                .expect("backend"),
            );
            crate::desktop::register_key_bindings(cx);
            let mut chat = ChatScreen::new_without_start(backend, "user".to_string(), cx);
            chat.selected_session = Some("s1".to_string());
            chat.booting = false;
            chat.application_vim_enabled = false;
            chat.set_application_vim_enabled(true, cx);
            chat.replace_timeline(vec![user_item("u1", "question")]);
            chat.handle_service_event(
                AgentServiceEvent::Question {
                    session_id: "s1".to_string(),
                    request_id: "free-form".to_string(),
                    questions: vec![maple_agent::agent::AgentQuestion {
                        multi_select: false,
                        id: "answer".to_string(),
                        header: "Question".to_string(),
                        question: "What should Maple do?".to_string(),
                        options: Vec::new(),
                    }],
                },
                cx,
            );
            chat
        });
        let (_host, cx) = cx.add_window_view(|_window, _cx| ChatHost { chat: chat.clone() });
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));

        let application_focus =
            cx.update(|_window, app| chat.read(app).application_focus.clone().unwrap());
        assert_eq!(
            cx.update(|window, app| window.focused(app)),
            Some(application_focus),
            "a fresh question starts on the application-navigation proxy"
        );

        cx.simulate_keystrokes("enter");
        let answer_focus = cx.update(|_window, app| {
            chat.read(app)
                .pending_question_input
                .clone()
                .expect("question input")
                .read(app)
                .focus_handle(app)
        });
        assert_eq!(
            cx.update(|window, app| window.focused(app)),
            Some(answer_focus),
            "Enter must provide a keyboard route into the free-form answer"
        );
        assert_eq!(
            cx.update(|_window, app| chat.read(app).pending_questions.len()),
            1,
            "focusing the answer must not submit an empty response"
        );
    }

    #[gpui::test]
    fn test_send_blocked_while_question_pending(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.booting = false;
            this.pending_questions = vec![crate::backend::PendingQuestion {
                session_id: "s1".to_string(),
                request_id: "q9".to_string(),
                questions: vec![maple_agent::agent::AgentQuestion {
                    multi_select: false,
                    id: "paused".to_string(),
                    header: "Question".to_string(),
                    question: "Paused?".to_string(),
                    options: Vec::new(),
                }],
            }];
            this.send_text("a stray reply".to_string(), cx);
            assert_eq!(
                this.notice.as_ref().map(SharedString::as_ref),
                Some("Answer the question above first")
            );
            assert!(this.question_focus_pending);
        });
    }

    #[test]
    fn test_slash_entries_filter_and_cap() {
        let skills = vec![AgentSlashCommand {
            name: "deploy".to_string(),
            description: "Deploy".to_string(),
            input_hint: None,
        }];
        let entries = slash_entries_for("de", &skills);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "deploy");
        assert!(slash_entries_for("zzz", &skills).is_empty());
        assert_eq!(slash_entries_for("", &skills).len(), 8);
    }

    #[gpui::test]
    fn test_side_question_streams_into_panel(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.btw = Some(SideQuestionPanel {
                request_id: "btw-2".to_string(),
                turns: vec![SideThreadTurn {
                    question: "why?".into(),
                    answer: String::new(),
                }],
                revision: 0,
                pending: true,
                error: None,
            });
            let event =
                |request_id: &str, event: SideQuestionEvent| AgentServiceEvent::SideQuestion {
                    session_id: "s1".to_string(),
                    request_id: request_id.to_string(),
                    event,
                };
            // A stream for a closed question changes nothing.
            assert!(
                !this.apply_service_event(
                    event("btw-1", SideQuestionEvent::Chunk("old".into())),
                    cx
                )
            );
            assert!(this.apply_service_event(
                event("btw-2", SideQuestionEvent::Chunk("Because ".into())),
                cx
            ));
            assert!(
                this.apply_service_event(
                    event("btw-2", SideQuestionEvent::Chunk("so.".into())),
                    cx
                )
            );
            assert!(this.apply_service_event(event("btw-2", SideQuestionEvent::Finished), cx));
            let btw = this.btw.as_ref().expect("panel stays open");
            assert_eq!(btw.turns[0].answer, "Because so.");
            assert_eq!(btw.revision, 2);
            assert!(!btw.pending);
            assert!(this.render_btw_card(cx).is_some());
            // A follow-up keeps the finished turn and adds the new one.
            this.booting = false;
            this.ask_side_question("s1", "and then?", cx);
            let btw = this.btw.as_ref().expect("thread continues");
            assert_eq!(btw.turns.len(), 2);
            assert_eq!(btw.turns[0].answer, "Because so.");
            assert_eq!(btw.turns[1].question, "and then?");
            assert!(btw.pending);
            assert_eq!(btw.request_id, format!("btw-{}", this.btw_sequence));
            // While the thread is open, a plain message joins it instead of
            // going to the task; a command still runs as a command.
            let live_id = this.btw.as_ref().unwrap().request_id.clone();
            assert!(this.apply_service_event(event(&live_id, SideQuestionEvent::Finished), cx));
            this.btw.as_mut().unwrap().turns[1].answer = "Then that.".into();
            this.send_text("plain follow-up".to_string(), cx);
            let btw = this.btw.as_ref().expect("thread continues");
            assert_eq!(btw.turns.len(), 3);
            assert_eq!(btw.turns[2].question, "plain follow-up");
            assert!(this.try_command(Some("s1"), "/web", cx));
            assert_eq!(this.btw.as_ref().unwrap().turns.len(), 3);
            this.close_side_thread(cx);
            assert!(this.btw.is_none());
            // Esc closes the panel.
            this.btw = None;
            assert!(this.render_btw_card(cx).is_none());
            // Landing on another task by any route ends the thread.
            this.ask_side_question("s1", "again?", cx);
            assert!(this.btw.is_some());
            this.set_active_session(summary("s1", "Same"), Vec::new(), HashMap::new(), cx);
            assert!(this.btw.is_some(), "same task keeps the thread");
            this.set_active_session(summary("s2", "Other"), Vec::new(), HashMap::new(), cx);
            assert!(this.btw.is_none(), "another task ends the thread");
        });
    }

    #[gpui::test]
    fn test_builtin_commands_execute(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.booting = false;
            this.web_enabled = true;
            assert!(this.try_command(Some("s1"), "/web", cx));
            assert!(!this.web_enabled);
            assert!(this.try_command(Some("s1"), "/model", cx));
            assert!(this.popup.is_open(&ChatPopup::Model));
            // Unknown commands fall through to a normal send.
            assert!(!this.try_command(Some("s1"), "/definitely-not-a-command", cx));
            // Paths that merely start with a slash are not commands.
            assert!(!this.try_command(Some("s1"), "/etc/hosts is a path", cx));
        });
    }

    #[gpui::test]
    fn test_skill_command_resolves_via_backend(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.slash_commands = vec![AgentSlashCommand {
                name: "deploy".to_string(),
                description: "Deploy the app".to_string(),
                input_hint: None,
            }];
            assert!(this.try_command(Some("s1"), "/deploy staging", cx));
            assert_eq!(
                this.notice.as_ref().map(SharedString::as_ref),
                Some("Loading skill…")
            );
        });
    }

    fn long_tool(id: &str, status: &str) -> AgentTimelineItem {
        let mut tool = item(id, "tool", None);
        tool.title = Some("shell".to_string());
        tool.input = Some(serde_json::json!({"command": "ls"}));
        tool.status = Some(status.to_string());
        tool.output = Some(serde_json::json!({"stdout": "x".repeat(600)}));
        tool
    }

    #[gpui::test]
    fn test_tool_summary_gating(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.summaries_enabled = true;
            // Running tools are not summarized.
            let index = this
                .apply_timeline_item("s1", long_tool("tool-1", "running"))
                .unwrap();
            this.maybe_summarize_tool(index, cx);
            assert!(this.summary_requests.is_empty());
            // Completed tools with long output are queued once.
            let index = this
                .apply_timeline_item("s1", long_tool("tool-1", "completed"))
                .unwrap();
            this.maybe_summarize_tool(index, cx);
            assert!(this.summary_requests.contains("tool-1"));
            assert_eq!(this.pending_summaries, 1);
            this.maybe_summarize_tool(index, cx);
            assert_eq!(this.pending_summaries, 1);
            // Short outputs queue too; only empty output is skipped.
            let mut short = item("tool-2", "tool", None);
            short.status = Some("completed".to_string());
            short.input = Some(serde_json::json!({"q": 1}));
            short.output = Some(serde_json::json!({"stdout": "ok"}));
            let index = this.apply_timeline_item("s1", short).unwrap();
            this.maybe_summarize_tool(index, cx);
            assert!(this.summary_requests.contains("tool-2"));
            let mut empty = item("tool-3", "tool", None);
            empty.status = Some("completed".to_string());
            empty.input = Some(serde_json::json!({"q": 1}));
            empty.output = Some(serde_json::json!({"stdout": "  "}));
            let index = this.apply_timeline_item("s1", empty).unwrap();
            this.maybe_summarize_tool(index, cx);
            assert!(!this.summary_requests.contains("tool-3"));
        });
    }

    #[gpui::test]
    fn test_thinking_summary_waits_for_a_following_item(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.summaries_enabled = true;
            this.active_runs
                .insert("s1".to_string(), "run-1".to_string());
            let long_thought = "x".repeat(600);
            let index = this
                .apply_timeline_item("s1", item("th-1", "thinking", Some(&long_thought)))
                .unwrap();
            // The newest item of an active run is still streaming.
            this.maybe_summarize_thinking(index, cx);
            assert!(this.summary_requests.is_empty());
            // A later item finalizes the block and requests its summary.
            this.apply_incoming_item("s1", item("m-1", "message", Some("done")), cx);
            assert!(this.summary_requests.contains("th-1"));
            // A short thought is not worth a model call.
            this.apply_incoming_item("s1", item("th-2", "thinking", Some("brief")), cx);
            this.apply_incoming_item("s1", item("m-2", "message", Some("done")), cx);
            assert!(!this.summary_requests.contains("th-2"));
        });
    }

    #[gpui::test]
    fn test_tool_summaries_queue_past_the_slot_cap(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.summaries_enabled = true;
            for n in 0..5 {
                let index = this
                    .apply_timeline_item("s1", long_tool(&format!("t{n}"), "completed"))
                    .unwrap();
                this.maybe_summarize_tool(index, cx);
            }
            // Three ride at once; the rest wait instead of being dropped.
            assert_eq!(this.pending_summaries, 3);
            assert_eq!(this.summary_queue.len(), 2);
            assert_eq!(this.summary_requests.len(), 5);
            // A session switch restarts the slots and drops the queue.
            let before = this.summary_generation;
            this.set_active_session(summary("s2", "B"), Vec::new(), HashMap::new(), cx);
            assert_eq!(this.summary_generation, before + 1);
            assert_eq!(this.pending_summaries, 0);
            assert!(this.summary_queue.is_empty());
            assert!(this.summary_requests.is_empty());
        });
    }

    #[gpui::test]
    fn test_finished_run_drops_its_questions(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.active_runs
                .insert("s1".to_string(), "run-1".to_string());
            this.handle_service_event(one_question("q", "Still there?"), cx);
            this.select_question_option(0, 0, cx);
            assert!(this.current_question().is_some());
            this.handle_run_event(
                "s1",
                "run-1",
                maple_agent::agent::AgentRunEvent::Finished(
                    maple_agent::agent::AgentRunTerminal::Cancelled,
                ),
                cx,
            );
            assert!(this.pending_questions.is_empty());
            assert!(this.pending_question_input.is_none());
            assert!(this.question_selected.is_empty());
            assert_eq!(this.question_step, 0);
            // The composer is unblocked again.
            this.booting = false;
            this.send_text("next".to_string(), cx);
            assert_ne!(
                this.notice.as_ref().map(SharedString::as_ref),
                Some("Answer the question above first")
            );
        });
    }

    #[gpui::test]
    fn test_questions_are_scoped_to_their_session(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        let other = AgentServiceEvent::Question {
            session_id: "s2".to_string(),
            request_id: "req-other".to_string(),
            questions: vec![maple_agent::agent::AgentQuestion {
                multi_select: false,
                id: "other".to_string(),
                header: "Question".to_string(),
                question: "From another task".to_string(),
                options: Vec::new(),
            }],
        };
        screen.update(cx, |this, cx| {
            this.active_runs
                .insert("s2".to_string(), "run-2".to_string());
            this.handle_service_event(other, cx);
            // Queued for later, but not shown and not blocking this task.
            assert_eq!(this.pending_questions.len(), 1);
            assert!(this.current_question().is_none());
            assert!(this.pending_question_input.is_none());
            this.booting = false;
            this.send_text("still typing here".to_string(), cx);
            assert_ne!(
                this.notice.as_ref().map(SharedString::as_ref),
                Some("Answer the question above first")
            );
            // Escape must not cancel the other session's run.
            this.skip_question(cx);
            assert_eq!(this.pending_questions.len(), 1);
            // A second question for the shown session keeps the pick made
            // on the first card.
            let mut event = one_question("a", "First?");
            if let AgentServiceEvent::Question { questions, .. } = &mut event {
                questions[0]
                    .options
                    .push(maple_agent::agent::AgentQuestionOption {
                        label: "A".into(),
                        description: String::new(),
                    });
            }
            this.handle_service_event(event, cx);
            this.select_question_option(0, 0, cx);
            this.handle_service_event(one_question("b", "Second?"), cx);
            assert_eq!(this.question_selected[&0].picked, BTreeSet::from([0]));
            // Switching to the other task shows its card.
            this.set_active_session(summary("s2", "B"), Vec::new(), HashMap::new(), cx);
            assert_eq!(
                this.current_question().map(|q| q.request_id.as_str()),
                Some("req-other")
            );
            assert!(this.pending_question_input.is_some());
            assert!(this.question_selected.is_empty());
            // Switching back still shows the first task's card.
            this.set_active_session(summary("s1", "A"), Vec::new(), HashMap::new(), cx);
            assert_eq!(
                this.current_question().map(|q| q.request_id.as_str()),
                Some("req-a")
            );
        });
    }

    #[gpui::test]
    fn test_project_selection_rejects_reentry_and_relative_paths(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.root_selecting = true;
            this.select_project_root(absolute_fixture_root("other"), cx);
            assert!(this.root_selecting);
            this.root_selecting = false;
            this.select_project_root("relative".to_string(), cx);
            assert!(!this.root_selecting);
            assert_eq!(
                this.notice.as_ref().map(SharedString::as_ref),
                Some("Enter an absolute directory path")
            );
        });
    }

    /// A project selection moves the navigation generation only when it
    /// lands, so a registration that fails leaves loads in flight alive,
    /// while a task clicked meanwhile supersedes the selection callback.
    #[gpui::test]
    fn test_project_selection_keeps_loads_alive_until_it_lands(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            let old_selection = this.selection_generation;
            let old_reload = this.reload_generation;
            this.select_project_root(absolute_fixture_root("other"), cx);

            assert!(this.root_selecting);
            assert_eq!(this.selection_generation, old_selection);
            assert_eq!(this.reload_generation, old_reload);

            this.select_session("newer-task", cx);
            assert!(this.selection_generation > old_selection);
        });
    }

    /// A session list requested before a click must not auto-select over
    /// the click, whose load leaves the selection empty until it lands.
    #[gpui::test]
    fn test_session_list_does_not_auto_select_over_a_newer_click(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.project_root = Some("/work/alpha".to_string());
            this.selected_session = None;
            let requested = this.selection_generation;
            this.select_session("clicked", cx);

            this.apply_session_list(vec![summary_at("s1", "A", "/work/alpha")], requested, cx);

            assert_eq!(this.selected_session, None);
            assert!(!this.session_setup_pending);
            assert_eq!(this.sessions.len(), 1);
        });
    }

    /// The header names what the pane shows: an empty pane is a new task
    /// even while the list still marks a row.
    #[gpui::test]
    fn test_header_says_new_task_while_the_pane_is_empty(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, _cx| {
            this.sessions = vec![summary_at("s1", "Hello", "/work/alpha")];
            this.selected_session = Some("s1".to_string());
            this.replace_timeline(Vec::new());
            assert_eq!(this.selected_title.as_ref(), "New Task");

            this.replace_timeline(vec![user_item("u1", "hi")]);
            assert_eq!(this.selected_title.as_ref(), "Hello");

            this.replace_timeline(Vec::new());
            assert_eq!(this.selected_title.as_ref(), "New Task");
        });
    }

    /// While a project selection is landing, neither "New Task" nor a
    /// first send may run ahead of it.
    #[gpui::test]
    fn test_new_task_waits_for_a_project_selection(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.project_root = Some("/work/alpha".to_string());
            this.root_selecting = true;
            let generation = this.selection_generation;
            this.new_session(cx);
            assert!(!this.session_setup_pending);
            assert_eq!(this.selection_generation, generation);
            assert_eq!(this.selected_session.as_deref(), Some("s1"));
            assert!(this.notice.is_some());

            this.booting = false;
            this.selected_session = None;
            this.send_text("hello".to_string(), cx);
            assert!(!this.session_setup_pending);
            assert!(this.pending_first_send.is_none());
        });
    }

    /// "New Task" clears the selection and shows the empty screen for the
    /// visible project; no task is created until something is sent.
    #[gpui::test]
    fn test_new_task_clears_the_selection_and_creates_nothing(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.project_root = Some("/work/alpha".to_string());
            this.sessions = vec![summary_at("s1", "Hello", "/work/alpha")];
            this.selected_session = Some("s1".to_string());
            this.replace_timeline(vec![user_item("u1", "hi")]);
            this.loading_session = Some("s1".to_string());
            this.default_web_enabled = false;
            this.web_enabled = true;
            let generation = this.selection_generation;

            this.new_session(cx);

            assert_eq!(this.selected_session, None);
            assert!(!this.session_setup_pending);
            assert!(this.pending_first_send.is_none());
            assert!(this.timeline.is_empty());
            assert_eq!(this.loading_session, None);
            assert_eq!(this.selected_title.as_ref(), "New Task");
            assert_eq!(this.project_root.as_deref(), Some("/work/alpha"));
            assert!(
                this.selection_generation > generation,
                "a draft is navigation"
            );
            assert!(!this.web_enabled, "the draft takes the web default");
            assert_eq!(this.sessions.len(), 1, "no row was added");
        });
    }

    /// Opening a task with a persisted model points the selector at that
    /// model without changing the account default. New Task restores it.
    #[gpui::test]
    fn test_existing_task_adopts_its_locked_model(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.project_root = Some("/tmp/proj".to_string());
            this.models = vec!["glm-5-3".into(), "kimi-k3".into()];
            this.default_model = Some("glm-5-3".into());
            this.selected_model = Some("glm-5-3".into());

            this.model_labels.insert("kimi-k3".into(), "Kimi K3".into());
            assert_eq!(this.model_label("kimi-k3"), "Kimi K3");
            assert_eq!(this.model_label("unknown-model"), "unknown-model");

            let mut glm = summary("glm-task", "GLM");
            glm.model = Some("glm-5-3".into());
            glm.message_count = 2;
            let mut kimi = summary("kimi-task", "Kimi");
            kimi.model = Some("kimi-k3".into());
            kimi.message_count = 3;

            this.set_active_session(kimi, Vec::new(), HashMap::new(), cx);
            assert_eq!(this.selected_model.as_deref(), Some("kimi-k3"));
            assert!(this.model_locked);
            assert_eq!(this.default_model.as_deref(), Some("glm-5-3"));
            assert_eq!(this.model_menu_models(), vec!["kimi-k3".to_string()]);

            this.pick_model("glm-5-3".into(), cx);
            assert_eq!(this.selected_model.as_deref(), Some("kimi-k3"));
            assert_eq!(this.default_model.as_deref(), Some("glm-5-3"));
            assert!(
                this.notice
                    .as_ref()
                    .is_some_and(|notice| notice.as_ref().contains("locked to model kimi-k3")),
                "picking another model on a locked task must not change the selector"
            );

            this.set_active_session(glm, Vec::new(), HashMap::new(), cx);
            assert_eq!(this.selected_model.as_deref(), Some("glm-5-3"));
            assert!(this.model_locked);

            this.new_session(cx);
            assert_eq!(this.selected_model.as_deref(), Some("glm-5-3"));
            assert!(!this.model_locked);
            assert_eq!(
                this.model_menu_models(),
                vec!["glm-5-3".to_string(), "kimi-k3".to_string()]
            );

            this.pick_model("kimi-k3".into(), cx);
            assert_eq!(this.selected_model.as_deref(), Some("kimi-k3"));
            assert_eq!(this.default_model.as_deref(), Some("kimi-k3"));
            assert!(!this.model_locked);
        });
    }

    /// A task opened before its first message locks as soon as the runtime
    /// reports that message, without being opened again.
    #[gpui::test]
    fn test_first_message_locks_the_task_on_screen(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        use maple_agent::agent::AgentRunEvent;
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.project_root = Some("/tmp/proj".to_string());
            this.models = vec!["glm-5-3".into(), "kimi-k3".into()];
            this.default_model = Some("glm-5-3".into());
            this.selected_model = Some("glm-5-3".into());

            let mut created = summary("new-task", "New");
            created.model = Some("glm-5-3".into());
            created.message_count = 0;
            this.upsert_session(created.clone(), cx);
            this.set_active_session(created.clone(), Vec::new(), HashMap::new(), cx);
            assert!(!this.model_locked);

            let mut sent = created;
            sent.message_count = 1;
            this.handle_run_event("new-task", "r1", AgentRunEvent::SessionUpdated(sent), cx);
            assert!(this.model_locked);
            assert_eq!(this.model_menu_models(), vec!["glm-5-3".to_string()]);

            this.pick_model("kimi-k3".into(), cx);
            assert_eq!(this.selected_model.as_deref(), Some("glm-5-3"));
            assert_eq!(this.default_model.as_deref(), Some("glm-5-3"));
            assert!(
                this.notice
                    .as_ref()
                    .is_some_and(|notice| notice.as_ref().contains("locked to model glm-5-3"))
            );
        });
    }

    /// Deleting the last task of a project leaves an empty screen that uses
    /// the default model, not the deleted task's lock.
    #[gpui::test]
    fn test_leaving_a_locked_task_restores_the_default_model(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.project_root = Some("/tmp/proj".to_string());
            this.models = vec!["glm-5-3".into(), "kimi-k3".into()];
            this.default_model = Some("glm-5-3".into());
            this.selected_model = Some("glm-5-3".into());

            let mut kimi = summary("kimi-task", "Kimi");
            kimi.model = Some("kimi-k3".into());
            kimi.message_count = 3;
            this.sessions = vec![kimi.clone()];
            this.set_active_session(kimi, Vec::new(), HashMap::new(), cx);
            assert!(this.model_locked);

            this.remove_session("kimi-task", cx);
            assert!(this.selected_session.is_none());
            assert!(!this.model_locked);
            assert_eq!(this.selected_model.as_deref(), Some("glm-5-3"));
            assert_eq!(this.model_menu_models().len(), 2);
            let request = this.new_session_request().expect("request");
            assert_eq!(request.model.as_deref(), Some("glm-5-3"));
        });
    }

    /// The first send on the empty screen creates the task, carrying the
    /// draft's mode and model, and sends once the task lands.
    #[gpui::test]
    fn test_first_send_creates_the_task_then_sends(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.booting = false;
            this.selected_session = None;
            this.project_root = Some("/work/alpha".to_string());
            this.selected_model = Some("model-x".to_string());
            this.default_model = Some("model-x".to_string());
            let request = this.new_session_request().expect("draft request");
            assert_eq!(request.model.as_deref(), Some("model-x"));
            assert_eq!(request.mcp_server_names, None);
            let generation = this.selection_generation;

            this.send_text("hello".to_string(), cx);

            assert!(this.session_setup_pending);
            assert_eq!(
                this.pending_first_send,
                Some(FirstSend::Message {
                    text: "hello".to_string(),
                    steer: false,
                })
            );
            assert_eq!(
                this.selected_session, None,
                "nothing is selected until the task lands"
            );

            // The create lands: the task is selected and the message goes
            // out to it.
            this.finish_new_session(
                summary_at("created", "New Task", "/work/alpha"),
                generation,
                cx,
            );
            assert!(!this.session_setup_pending);
            assert!(this.pending_first_send.is_none());
            assert_eq!(this.selected_session.as_deref(), Some("created"));
            assert!(this.awaiting_first_token, "the send was dispatched");
        });
    }

    /// A newly created task waits for its subscription, and a later
    /// navigation wins even while that acknowledgement is in flight.
    #[gpui::test]
    fn new_task_waits_for_subscription_and_yields_to_newer_navigation(cx: &mut TestAppContext) {
        let root = absolute_fixture_root("work");
        let (chat, cx) = chat_window(cx, |this, cx| {
            this.project_root = Some(root.clone());
            this.selected_session = None;
            this.new_session(cx);
        });
        chat.update(cx, |this, cx| {
            this.session_setup_pending = true;
            this.pending_first_send = Some(FirstSend::Message {
                text: "first message".into(),
                steer: false,
            });
            let generation = this.selection_generation;
            let intent = this.backend.next_subscription_intent();
            this.subscribe_new_session(
                summary_at("created", "New Task", &root),
                Vec::new(),
                generation,
                intent,
                cx,
            );
            assert!(this.session_setup_pending);
            assert!(this.pending_first_send.is_some());
            assert!(this.selected_session.is_none());
            assert!(
                !this.awaiting_first_token,
                "no send before subscription acknowledgement"
            );

            // Navigation during the subscription round trip supersedes the
            // first send just as navigation during task creation does.
            this.begin_navigation();
            this.withdraw_first_send(cx);
            this.set_active_session(
                summary_at("newer", "Newer selection", &root),
                Vec::new(),
                HashMap::new(),
                cx,
            );
        });
        wait_until(&chat, cx, "old subscription callback settled", |this| {
            !this.session_setup_pending
        });
        chat.update(cx, |this, _| {
            assert_eq!(this.selected_session.as_deref(), Some("newer"));
            assert!(this.pending_first_send.is_none());
            assert!(!this.awaiting_first_token, "the abandoned task never sent");
        });
    }

    /// Enter while the first send is still creating its task does not
    /// start a second create; the new text waits in the composer.
    #[gpui::test]
    fn test_send_during_a_pending_create_does_not_start_another(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.booting = false;
            this.selected_session = None;
            this.project_root = Some("/work/alpha".to_string());
            this.send_text("first".to_string(), cx);
            assert!(this.session_setup_pending);

            this.send_text("second".to_string(), cx);

            assert_eq!(
                this.pending_first_send,
                Some(FirstSend::Message {
                    text: "first".to_string(),
                    steer: false,
                })
            );
            assert!(this.notice.is_some());
            // "New Task" cannot supersede the create either.
            let generation = this.selection_generation;
            this.new_session(cx);
            assert_eq!(this.selection_generation, generation);
        });
    }

    /// The created task is compared with the composer before anything is
    /// sent: web access the create could not carry, a server switched on
    /// after Enter, and an external agent, which no request switches on.
    /// With a difference the send waits for the apply; with none it goes
    /// out at once. Before, the first turn ran under the web access the
    /// task happened to be created with.
    #[gpui::test]
    fn test_first_send_applies_the_composer_before_sending(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.booting = false;
            this.selected_session = None;
            this.project_root = Some("/work/alpha".to_string());
            this.draft = true;
            this.default_web_enabled = false;
            this.web_enabled = false;
            this.set_draft_mcp_defaults(vec![
                draft_mcp_row(mcp_server("docs", true)),
                draft_mcp_row(mcp_server("wiki", false)),
                AgentSessionMcpServer {
                    name: "codex".to_string(),
                    kind: AgentSessionIntegrationKind::ExternalAgent,
                    display_name: "Codex".to_string(),
                    description: String::new(),
                    transport: "external_agent".to_string(),
                    enabled: false,
                    available: true,
                },
            ]);
            let request = this.new_session_request().expect("draft request");
            assert_eq!(request.mcp_server_names, Some(vec!["docs".to_string()]));
            this.send_text("hello".to_string(), cx);
            assert!(this.session_setup_pending);
            let generation = this.selection_generation;

            // While the create is in flight: wiki on, Codex on.
            this.toggle_session_mcp(
                "wiki".to_string(),
                AgentSessionIntegrationKind::Mcp,
                true,
                cx,
            );
            this.toggle_session_mcp(
                "codex".to_string(),
                AgentSessionIntegrationKind::ExternalAgent,
                true,
                cx,
            );

            // The create lands with what it was asked for.
            let mut created = summary_at("created", "New Task", "/work/alpha");
            created.web_enabled = true;
            let task_mcp = this.created_task_mcp(request.mcp_server_names.as_deref());
            assert_eq!(
                task_mcp,
                vec![
                    DraftMcpChange {
                        name: "docs".to_string(),
                        kind: AgentSessionIntegrationKind::Mcp,
                        enabled: true,
                    },
                    DraftMcpChange {
                        name: "wiki".to_string(),
                        kind: AgentSessionIntegrationKind::Mcp,
                        enabled: false,
                    },
                    DraftMcpChange {
                        name: "codex".to_string(),
                        kind: AgentSessionIntegrationKind::ExternalAgent,
                        enabled: false,
                    },
                ]
            );
            assert_eq!(
                this.new_session_changes(&created, &task_mcp),
                NewSessionChanges {
                    web: Some(false),
                    mcp: vec![
                        DraftMcpChange {
                            name: "wiki".to_string(),
                            kind: AgentSessionIntegrationKind::Mcp,
                            enabled: true,
                        },
                        DraftMcpChange {
                            name: "codex".to_string(),
                            kind: AgentSessionIntegrationKind::ExternalAgent,
                            enabled: true,
                        },
                    ],
                }
            );
            // A request that named no servers means the task took the
            // defaults: docs on, wiki off.
            assert_eq!(
                this.created_task_mcp(None)[..2],
                task_mcp[..2],
                "no names: the defaults"
            );

            this.settle_new_session(created.clone(), task_mcp.clone(), generation, cx);
            assert!(this.session_setup_pending, "the apply is in flight");
            assert!(this.pending_first_send.is_some(), "nothing sent yet");
            assert_eq!(this.selected_session, None);
            assert!(!this.awaiting_first_token);

            // Once the task matches the composer the send goes out.
            created.web_enabled = false;
            let mut task_mcp = task_mcp;
            task_mcp[1].enabled = true;
            task_mcp[2].enabled = true;
            assert!(this.new_session_changes(&created, &task_mcp).is_empty());
            this.settle_new_session(created, task_mcp, generation, cx);
            assert!(!this.session_setup_pending);
            assert!(this.pending_first_send.is_none());
            assert_eq!(this.selected_session.as_deref(), Some("created"));
            assert!(!this.web_enabled, "the chip is not clobbered");
            assert!(this.awaiting_first_token, "the send was dispatched");
        });
    }

    /// A composer setting that fails to apply keeps the created task,
    /// opens it, shows the error, and sends nothing: the text stays for
    /// the user to fix the setting and send again. Before, the send ran
    /// anyway with only a warning.
    #[gpui::test]
    fn test_failed_restriction_keeps_the_task_and_sends_nothing(cx: &mut TestAppContext) {
        let (chat, cx) = chat_window(cx, |this, cx| {
            this.project_root = Some(absolute_fixture_root("work"));
            this.selected_session = None;
            this.new_session(cx);
        });
        chat.update(cx, |this, cx| {
            this.session_setup_pending = true;
            this.pending_first_send = Some(FirstSend::Message {
                text: "hello".to_string(),
                steer: false,
            });
            let generation = this.selection_generation;

            this.fail_new_session_setup(
                summary_at("created", "New Task", &absolute_fixture_root("work")),
                "Could not change web access: boom".to_string(),
                generation,
                cx,
            );

            assert!(!this.session_setup_pending);
            assert!(this.pending_first_send.is_none());
            assert_eq!(this.selected_session.as_deref(), Some("created"));
            assert!(!this.draft);
            assert!(!this.awaiting_first_token, "nothing was sent");
            assert_eq!(
                this.notice.as_ref().map(SharedString::as_ref),
                Some("Could not change web access: boom")
            );
            assert_eq!(this.composer.clone().unwrap().read(cx).text(), "hello");
            assert!(
                this.sessions.iter().any(|session| session.id == "created"),
                "the task stays in the list"
            );
        });
    }

    /// The text typed into the draft stays in the composer until the send
    /// goes out. While the create is in flight the composer shows it as
    /// waiting, the Send button and Enter keep the text; a failed create
    /// leaves it there and in the history; a create that lands sends what
    /// the composer shows; one that lands after the user opened another
    /// task sends nothing, gives the text back, and discards the task.
    /// Before, the Send button cleared the text ahead of the pending
    /// check, and a stale or failed create dropped it.
    #[gpui::test]
    fn test_the_composer_keeps_the_text_until_the_first_send_goes_out(cx: &mut TestAppContext) {
        let root = absolute_fixture_root("work");
        let (chat, cx) = chat_window(cx, |this, cx| {
            this.project_root = Some(root.clone());
            this.selected_session = None;
            this.new_session(cx);
        });
        let composer = chat.update(cx, |this, _| this.composer.clone().unwrap());
        let pending = |text: &str| {
            Some(FirstSend::Message {
                text: text.to_string(),
                steer: false,
            })
        };

        // A create in flight for "hello".
        chat.update(cx, |this, cx| {
            composer.update(cx, |input, cx| input.set_text("hello", cx));
            this.session_setup_pending = true;
            this.pending_first_send = pending("hello");
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("composer-status").is_some(),
            "the composer shows the create in progress"
        );
        press(cx, "send-message");
        assert_eq!(
            composer.update(cx, |input, _| input.text()),
            "hello",
            "the Send button keeps the text while the create is pending"
        );
        chat.update(cx, |this, cx| {
            this.send_text("hello".to_string(), cx);
            assert!(this.session_setup_pending);
            assert_eq!(this.pending_first_send, pending("hello"));
            assert_eq!(
                this.composer_text(cx).as_deref(),
                Some("hello"),
                "Enter keeps it too"
            );

            // The create fails: the text stays, and the history has it.
            this.fail_new_session("no runtime".to_string(), cx);
            assert_eq!(this.composer_text(cx).as_deref(), Some("hello"));
            assert!(this.prompt_history.contains(&"hello".to_string()));
            assert!(this.draft);
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("composer-status").is_none(),
            "the composer is live again"
        );

        // The user edits while the next create is in flight; when it
        // lands, what the composer shows goes out and the composer clears.
        chat.update(cx, |this, cx| {
            this.session_setup_pending = true;
            this.pending_first_send = pending("hello");
            composer.update(cx, |input, cx| input.set_text("hello, edited", cx));
            let generation = this.selection_generation;
            this.finish_new_session(summary_at("created", "New Task", &root), generation, cx);
            assert_eq!(this.selected_session.as_deref(), Some("created"));
            assert!(this.awaiting_first_token, "the send was dispatched");
            assert_eq!(
                this.prompt_history.last().map(String::as_str),
                Some("hello, edited")
            );
            assert_eq!(this.composer_text(cx).as_deref(), Some(""), "sent");
        });

        // A create that lands after another task was opened: nothing is
        // sent, the text goes back into the empty composer and into the
        // history, and the unused task's delete is requested.
        chat.update(cx, |this, cx| {
            this.set_active_session(
                summary_at("other", "Other", &root),
                Vec::new(),
                HashMap::new(),
                cx,
            );
            this.awaiting_first_token = false;
            this.session_setup_pending = true;
            this.pending_first_send = pending("late");
            let stale = this.selection_generation - 1;
            this.finish_new_session(summary_at("late-task", "New Task", &root), stale, cx);
            assert!(!this.session_setup_pending);
            assert!(this.pending_first_send.is_none());
            assert!(!this.awaiting_first_token, "nothing was sent");
            assert_eq!(this.selected_session.as_deref(), Some("other"));
            assert_eq!(this.composer_text(cx).as_deref(), Some("late"));
            assert!(this.prompt_history.contains(&"late".to_string()));
            assert!(
                this.notice
                    .as_ref()
                    .is_some_and(|notice| notice.contains("back in the composer")),
                "{:?}",
                this.notice
            );

            // With text of its own in the composer, the message waits in
            // the history instead of replacing it.
            this.session_setup_pending = true;
            this.pending_first_send = pending("later");
            let stale = this.selection_generation - 1;
            this.finish_new_session(summary_at("later-task", "New Task", &root), stale, cx);
            assert_eq!(this.composer_text(cx).as_deref(), Some("late"), "untouched");
            assert_eq!(
                this.prompt_history.last().map(String::as_str),
                Some("later")
            );
            assert!(
                this.notice
                    .as_ref()
                    .is_some_and(|notice| notice.contains("Press Up")),
                "{:?}",
                this.notice
            );
        });
    }

    /// The composer has one action button. It is Send, and it becomes Stop
    /// only while a run is active and nothing is waiting to go out. A draft
    /// turns it back into Send, including an image that is still encoding.
    /// The runtime's starting spinner stays Send too.
    #[gpui::test]
    fn test_the_composer_button_is_send_or_stop(cx: &mut TestAppContext) {
        let (chat, cx) = chat_window(cx, |_, _| {});
        let composer = chat.update(cx, |this, _| this.composer.clone().unwrap());

        fn shown(cx: &mut gpui::VisualTestContext) -> &'static str {
            // The second frame sees a composer observer that landed after
            // the text change.
            let mut send = false;
            let mut stop = false;
            for _ in 0..2 {
                cx.run_until_parked();
                cx.update(|window, _| window.refresh());
                cx.run_until_parked();
                send = cx.debug_bounds("send-message").is_some();
                stop = cx.debug_bounds("stop-run").is_some();
            }
            match (send, stop) {
                (true, false) => "send",
                (false, true) => "stop",
                (true, true) => "both",
                (false, false) => "neither",
            }
        }

        assert_eq!(shown(cx), "send", "an idle empty composer offers Send");

        chat.update(cx, |this, cx| {
            this.active_runs
                .insert("s1".to_string(), "run-1".to_string());
            cx.notify();
        });
        assert_eq!(
            shown(cx),
            "stop",
            "a running composer with nothing queued offers Stop"
        );

        composer.update(cx, |input, cx| input.set_text("next turn", cx));
        assert_eq!(
            shown(cx),
            "send",
            "a draft during a run offers Send, which queues"
        );

        composer.update(cx, |input, cx| input.set_text("   ", cx));
        assert_eq!(shown(cx), "stop", "whitespace is not a draft");

        chat.update(cx, |this, cx| {
            this.draft_images = vec![DraftImage {
                id: 1,
                name: "shot.png".into(),
                data_url: None,
                thumbnail: None,
            }];
            cx.notify();
        });
        assert_eq!(
            shown(cx),
            "send",
            "an image that is still encoding keeps Send instead of Stop"
        );

        chat.update(cx, |this, cx| {
            this.draft_images.clear();
            this.booting = true;
            cx.notify();
        });
        assert_eq!(
            shown(cx),
            "send",
            "starting the runtime keeps the Send spinner"
        );
    }

    /// A typed command that needs a task becomes the draft's first send
    /// and, like a message, stays in the composer until it runs; a create
    /// that is refused leaves it there too. A command that needs no task
    /// still clears. Before, the composer cleared once the create started,
    /// so text typed meanwhile went out in the command's place and the
    /// command never ran.
    #[gpui::test]
    fn test_a_typed_command_stays_on_show_until_it_runs(cx: &mut TestAppContext) {
        let root = absolute_fixture_root("work");
        let (chat, cx) = chat_window(cx, |this, cx| {
            this.project_root = Some(root.clone());
            this.selected_session = None;
            this.new_session(cx);
        });
        chat.update(cx, |this, cx| {
            this.booting = false;
            this.slash_commands = vec![AgentSlashCommand {
                name: "deploy".to_string(),
                description: "Deploy the app".to_string(),
                input_hint: None,
            }];
            let composer = this.composer.clone().unwrap();

            composer.update(cx, |input, cx| input.set_text("/help", cx));
            this.send_text("/help".to_string(), cx);
            assert_eq!(this.composer_text(cx).as_deref(), Some(""), "/help ran");

            // Refused while a project selection lands: the command stays.
            this.root_selecting = true;
            composer.update(cx, |input, cx| input.set_text("/deploy staging", cx));
            this.send_text("/deploy staging".to_string(), cx);
            assert!(!this.session_setup_pending);
            assert_eq!(this.composer_text(cx).as_deref(), Some("/deploy staging"));
            this.root_selecting = false;

            // The create starts, and the command stays on show meanwhile.
            this.send_text("/deploy staging".to_string(), cx);
            assert!(this.session_setup_pending);
            assert_eq!(
                this.pending_first_send,
                Some(FirstSend::Command("/deploy staging".to_string()))
            );
            assert_eq!(this.composer_text(cx).as_deref(), Some("/deploy staging"));

            // The task lands and the command runs, not a plain send.
            let generation = this.selection_generation;
            this.finish_new_session(summary_at("created", "New Task", &root), generation, cx);
            assert_eq!(
                this.notice.as_ref().map(SharedString::as_ref),
                Some("Loading skill…")
            );
            assert!(!this.awaiting_first_token, "no plain message went out");
            assert_eq!(this.composer_text(cx).as_deref(), Some(""));
        });
    }

    /// Opening another task while the first send is still creating its
    /// task takes the message out of the composer at once, into the
    /// history: the task opened never shows it, and the late create, whose
    /// task is deleted, puts nothing back. Before, the message stayed live
    /// in the other task's composer, and a late create put it back even
    /// after it had been sent there, inviting a duplicate.
    #[gpui::test]
    fn test_opening_another_task_takes_the_first_send_along(cx: &mut TestAppContext) {
        let root = absolute_fixture_root("work");
        let (chat, cx) = chat_window(cx, |this, cx| {
            this.project_root = Some(root.clone());
            this.selected_session = None;
            this.new_session(cx);
        });
        chat.update(cx, |this, cx| {
            this.booting = false;
            let composer = this.composer.clone().unwrap();
            composer.update(cx, |input, cx| input.set_text("hello", cx));
            this.send_text("hello".to_string(), cx);
            assert!(this.session_setup_pending);
            let generation = this.selection_generation;

            // Task B is opened while the create is in flight.
            this.select_session("b", cx);
            assert!(this.pending_first_send.is_none());
            assert_eq!(this.composer_text(cx).as_deref(), Some(""));
            assert_eq!(
                this.prompt_history.last().map(String::as_str),
                Some("hello")
            );
            assert!(
                this.notice
                    .as_ref()
                    .is_some_and(|notice| notice.starts_with("Message not sent")),
                "{:?}",
                this.notice
            );

            // B lands, and what is sent there stays sent: the late create
            // puts nothing back.
            this.set_active_session(summary_at("b", "B", &root), Vec::new(), HashMap::new(), cx);
            assert_eq!(this.composer_text(cx).as_deref(), Some(""));
            composer.update(cx, |input, cx| input.set_text("for b", cx));
            this.send_text("for b".to_string(), cx);
            assert_eq!(this.composer_text(cx).as_deref(), Some(""));
            this.finish_new_session(summary_at("late", "New Task", &root), generation, cx);
            assert!(!this.session_setup_pending);
            assert_eq!(this.selected_session.as_deref(), Some("b"));
            assert_eq!(
                this.composer_text(cx).as_deref(),
                Some(""),
                "nothing put back"
            );
        });
    }

    /// The project cannot be switched while the first send is creating its
    /// task in the visible one. Before, the draft moved away with the
    /// message still in it, waited for the old create to land, and was then
    /// told another task had been opened.
    #[gpui::test]
    fn test_project_switch_waits_for_the_first_send(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.project_root = Some("/work/alpha".to_string());
            this.selected_session = None;
            this.session_setup_pending = true;

            this.select_project_root(absolute_fixture_root("beta"), cx);
            assert!(!this.root_selecting, "the switch did not start");
            assert_eq!(this.project_root.as_deref(), Some("/work/alpha"));
            assert_eq!(
                this.notice.as_ref().map(SharedString::as_ref),
                Some("Wait for the new task to be created, then switch projects")
            );

            // From a task on screen the switch goes ahead as before.
            this.selected_session = Some("s1".to_string());
            this.select_project_root(absolute_fixture_root("beta"), cx);
            assert!(this.root_selecting);
        });
    }

    /// A failed create leaves nothing behind and gives the message back.
    #[gpui::test]
    fn test_failed_create_keeps_the_draft(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.booting = false;
            this.selected_session = None;
            this.project_root = Some("/work/alpha".to_string());
            this.send_text("hello".to_string(), cx);
            assert!(this.session_setup_pending);

            this.fail_new_session("no runtime".to_string(), cx);

            assert!(!this.session_setup_pending);
            assert!(this.pending_first_send.is_none());
            assert_eq!(this.selected_session, None);
            assert_eq!(
                this.notice.as_ref().map(SharedString::as_ref),
                Some("no runtime")
            );
        });
    }

    /// Commands that act on a task, and `/btw`, create the task first on
    /// the empty screen; commands that do not need one run at once.
    #[gpui::test]
    fn test_commands_on_the_empty_screen_create_the_task_first(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.booting = false;
            this.selected_session = None;
            this.project_root = Some("/work/alpha".to_string());
            this.web_enabled = true;

            // No task needed: the draft's web chip flips on screen only.
            assert!(this.try_command(None, "/web", cx));
            assert!(!this.web_enabled);
            assert!(!this.session_setup_pending);

            this.send_text("/btw what is this".to_string(), cx);
            assert!(this.session_setup_pending);
            assert_eq!(
                this.pending_first_send,
                Some(FirstSend::SideQuestion("what is this".to_string()))
            );
            this.fail_new_session("no runtime".to_string(), cx);

            this.slash_commands = vec![AgentSlashCommand {
                name: "deploy".to_string(),
                description: "Deploy the app".to_string(),
                input_hint: None,
            }];
            assert!(this.try_command(None, "/deploy staging", cx));
            assert_eq!(
                this.pending_first_send,
                Some(FirstSend::Command("/deploy staging".to_string()))
            );
            // Unknown commands still fall through to a normal send.
            this.fail_new_session("no runtime".to_string(), cx);
            assert!(!this.try_command(None, "/definitely-not-a-command", cx));
        });
    }

    /// A configured MCP server, as the settings page saves it.
    fn mcp_server(name: &str, enabled: bool) -> AgentMcpServer {
        AgentMcpServer {
            name: name.to_string(),
            description: String::new(),
            enabled,
            timeout_seconds: 30,
            transport: AgentMcpTransport::Stdio {
                command: format!("{name}-mcp"),
                environment: Vec::new(),
            },
        }
    }

    /// The first task uses the displayed switches even before project saves
    /// complete, and refreshing the catalog preserves draft edits.
    #[gpui::test]
    fn test_draft_chips_preserve_the_first_tasks_selection(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.project_root = Some("/work/alpha".to_string());
            this.selected_session = None;
            this.draft = true;
            this.web_enabled = true;
            this.set_web_enabled(false, cx);
            assert!(!this.web_enabled);
            assert_eq!(
                this.new_session_request().unwrap().mcp_server_names,
                None,
                "before the rows load the task takes the defaults"
            );

            this.set_draft_mcp_defaults(vec![
                draft_mcp_row(mcp_server("docs", true)),
                draft_mcp_row(mcp_server("wiki", false)),
            ]);
            assert_eq!(this.mcp_enabled_count, 1);
            assert_eq!(
                this.new_session_request().unwrap().mcp_server_names,
                Some(vec!["docs".to_string()])
            );
            this.toggle_session_mcp(
                "docs".to_string(),
                AgentSessionIntegrationKind::Mcp,
                false,
                cx,
            );
            assert_eq!(this.mcp_enabled_count, 0);
            assert!(!this.session_mcp[0].enabled);
            assert_eq!(
                this.draft_mcp_changes,
                vec![DraftMcpChange {
                    name: "docs".to_string(),
                    kind: AgentSessionIntegrationKind::Mcp,
                    enabled: false,
                }]
            );
            assert_eq!(
                this.new_session_request().unwrap().mcp_server_names,
                Some(Vec::new()),
                "a server switched off is left out, not defaulted back in"
            );
            // The rows reload (the chip opened again, settings returned):
            // the switch survives, and a row that went away drops its toggle.
            this.set_draft_mcp_defaults(vec![
                draft_mcp_row(mcp_server("docs", true)),
                draft_mcp_row(mcp_server("wiki", false)),
            ]);
            assert!(!this.session_mcp[0].enabled, "the toggle carries over");
            assert_eq!(this.mcp_enabled_count, 0);
            this.set_draft_mcp_defaults(vec![draft_mcp_row(mcp_server("wiki", false))]);
            assert!(this.draft_mcp_changes.is_empty());
            // Toggling back replaces the entry instead of stacking.
            this.toggle_session_mcp(
                "wiki".to_string(),
                AgentSessionIntegrationKind::Mcp,
                true,
                cx,
            );
            this.toggle_session_mcp(
                "wiki".to_string(),
                AgentSessionIntegrationKind::Mcp,
                false,
                cx,
            );
            assert_eq!(this.draft_mcp_changes.len(), 1);
            assert!(!this.draft_mcp_changes[0].enabled);
            // A task selection ends the draft.
            this.clear_selected_session_presentation(cx);
            assert!(this.draft_mcp_changes.is_empty());
            assert!(this.draft_mcp_defaults.is_none());
        });
    }

    #[gpui::test]
    fn test_draft_mcp_spelling_changes_preserve_pending_choices(cx: &mut TestAppContext) {
        let screen = screen(cx);
        screen.update(cx, |this, _cx| {
            let root = absolute_fixture_root("work");
            this.project_root = Some(root.clone());
            this.selected_session = None;
            this.draft = true;
            this.web_enabled = true;
            this.project_mcp_save_pending = true;
            for enabled in [true, false] {
                this.project_mcp_saves.push_back(ProjectMcpSave {
                    root: root.clone(),
                    name: "Docs".into(),
                    enabled,
                });
            }
            this.draft_mcp_changes = ["Docs", "docs-extra"]
                .into_iter()
                .map(|name| DraftMcpChange {
                    name: name.into(),
                    kind: AgentSessionIntegrationKind::Mcp,
                    enabled: false,
                })
                .collect();
            this.set_draft_mcp_defaults(vec![
                draft_mcp_row(mcp_server("D o c s", true)),
                draft_mcp_row(mcp_server("docs-extra", true)),
                draft_mcp_row(mcp_server("docsextra", true)),
            ]);
            assert_eq!(this.draft_mcp_changes.len(), 2);
            assert!(!this.session_mcp[0].enabled, "the renamed server keeps OFF");
            assert!(!this.session_mcp[1].enabled);
            assert!(
                this.session_mcp[2].enabled,
                "distinct server keys remain separate"
            );
            assert_eq!(this.draft_mcp_names(), Some(vec!["docsextra".into()]));
            let task_mcp = this.created_task_mcp(Some(&["Docs".into(), "docsextra".into()]));
            assert_eq!(
                this.new_session_changes(&summary_at("created", "New Task", &root), &task_mcp)
                    .mcp,
                vec![DraftMcpChange {
                    name: "D o c s".into(),
                    kind: AgentSessionIntegrationKind::Mcp,
                    enabled: false,
                }],
                "an ON selection under the old spelling still needs its OFF restriction"
            );
        });
        screen.update(cx, |this, cx| {
            for enabled in [true, false] {
                this.toggle_session_mcp(
                    "D o c s".into(),
                    AgentSessionIntegrationKind::Mcp,
                    enabled,
                    cx,
                );
            }
            assert_eq!(
                this.project_mcp_saves.len(),
                2,
                "the same queued identity coalesces"
            );
            assert!(!this.project_mcp_saves.back().unwrap().enabled);
            assert_eq!(this.draft_mcp_changes.len(), 2);
        });
    }

    /// Run the executor until `done` holds. Backend calls answer from the
    /// tokio runtime, which the test executor does not drive, so the wait
    /// polls with a bound instead of parking once.
    fn wait_until(
        chat: &Entity<ChatScreen>,
        cx: &mut gpui::VisualTestContext,
        what: &str,
        done: impl Fn(&ChatScreen) -> bool,
    ) {
        for _ in 0..500 {
            cx.run_until_parked();
            if chat.update(cx, |this, _| done(this)) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let state = chat.update(cx, |this, _| {
            format!(
                "draft={}, project={:?}, defaults_loaded={}, queued_saves={}, save_pending={}, \
                 defaults_generation={}, selection_generation={}, notice={:?}",
                this.draft,
                this.project_root,
                this.draft_mcp_defaults.is_some(),
                this.project_mcp_saves.len(),
                this.project_mcp_save_pending,
                this.draft_mcp_generation,
                this.selection_generation,
                this.notice,
            )
        });
        panic!("{what} did not happen: {state}");
    }

    /// A backend whose configuration and data live under a temporary XDG
    /// root, with `servers` saved through its own API, so a test can read
    /// them back without touching the real configuration.
    fn backend_with_mcp_servers(
        cx: &mut TestAppContext,
        tag: &str,
        servers: Vec<AgentMcpServer>,
    ) -> Arc<crate::backend::AgentBackend> {
        let dir =
            std::env::temp_dir().join(format!("maple-agent-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let backend = {
            // The roots are read at construction; no other test may build
            // a backend or read settings while the swap is live.
            let _guard = SETTINGS_LOCK.lock();
            let previous_config = std::env::var_os("XDG_CONFIG_HOME");
            let previous_data = std::env::var_os("XDG_DATA_HOME");
            unsafe {
                std::env::set_var("XDG_CONFIG_HOME", dir.join("config"));
                std::env::set_var("XDG_DATA_HOME", dir.join("data"));
            }
            let backend = crate::backend::AgentBackend::for_test(
                "http://127.0.0.1:9".to_string(),
                String::new(),
            );
            unsafe {
                match previous_config {
                    Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
                    None => std::env::remove_var("XDG_CONFIG_HOME"),
                }
                match previous_data {
                    Some(value) => std::env::set_var("XDG_DATA_HOME", value),
                    None => std::env::remove_var("XDG_DATA_HOME"),
                }
            }
            Arc::new(backend.expect("backend"))
        };
        // The save runs on the backend's own runtime; wait for it here.
        let (sender, receiver) = std::sync::mpsc::channel();
        {
            let backend = backend.clone();
            backend.clone().spawn(async move {
                let _ = sender.send(backend.save_mcp_servers("user", servers).await);
            });
        }
        receiver
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("save task")
            .expect("servers saved");
        cx.executor().allow_parking();
        backend
    }

    fn await_backend_result<T: Send + 'static>(
        backend: &Arc<crate::backend::AgentBackend>,
        request: impl std::future::Future<Output = Result<T, String>> + Send + 'static,
    ) -> T {
        let (sender, receiver) = std::sync::mpsc::channel();
        backend.spawn(async move {
            let _ = sender.send(request.await);
        });
        receiver
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("backend task finished")
            .expect("backend operation succeeded")
    }

    fn register_test_project(
        backend: &Arc<crate::backend::AgentBackend>,
        root: &str,
    ) -> Vec<RecentProjectRoot> {
        let owner = backend.clone();
        let root = root.to_string();
        vec![
            await_backend_result(backend, async move {
                owner.select_project_root("user", root).await
            })
            .project,
        ]
    }

    fn mcp_project(tag: &str) -> String {
        let root = std::env::temp_dir().join(format!(
            "maple-agent-test-mcp-project-{tag}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root.canonicalize().unwrap().to_string_lossy().to_string()
    }

    /// Choosing another folder keeps the Maple workspace in the roots the
    /// screen applies: the selection answer offers it like the list does.
    /// Before, the workspace left both project menus until the next start.
    #[gpui::test]
    fn test_project_selection_keeps_offering_the_maple_workspace(cx: &mut TestAppContext) {
        let Some(workspace) = maple_agent::agent::default_maple_workspace_path() else {
            return;
        };
        let backend = backend_with_mcp_servers(cx, "maple-workspace-offer", Vec::new());
        let other = std::env::temp_dir().join(format!(
            "maple-agent-test-other-root-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&other).unwrap();
        let other = other.canonicalize().unwrap().to_string_lossy().to_string();
        let (sender, receiver) = std::sync::mpsc::channel();
        {
            let backend = backend.clone();
            let other = other.clone();
            backend.clone().spawn(async move {
                let result = async {
                    let selected = backend.select_project_root("user", other).await?;
                    let page = backend
                        .list_projects(
                            "user",
                            crate::backend::ProjectPageRequest {
                                cursor: None,
                                limit: 50,
                            },
                        )
                        .await?;
                    Ok::<_, String>((selected, page))
                }
                .await;
                let _ = sender.send(result);
            });
        }
        let (registration, page) = receiver
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("select task")
            .expect("project selected");
        assert_eq!(registration.project_root, other);
        let roots: Vec<String> = page.projects.into_iter().map(|root| root.root).collect();
        assert_eq!(
            roots,
            vec![other.clone(), workspace.to_string_lossy().to_string()]
        );
        let _ = std::fs::remove_dir_all(&other);
    }

    /// A curated integration card as the runtime catalog projects it.
    fn integration_card(
        id: &str,
        availability: maple_agent::agent::AgentIntegrationAvailability,
        backend: Option<maple_agent::agent::AgentIntegrationBackend>,
        enabled_for_new_tasks: bool,
    ) -> maple_agent::agent::AgentIntegration {
        maple_agent::agent::AgentIntegration {
            id: id.to_string(),
            name: id.to_uppercase(),
            description: format!("{id} card"),
            availability,
            backend,
            version: None,
            permissions: None,
            setup_available: false,
            enabled_for_new_tasks,
            detail: None,
        }
    }

    /// The draft's rows are the rows a new task would get: the configured
    /// servers, the external agents enabled in Settings (off until
    /// selected for the task, available when installed), and the CUA
    /// integration under its own id, on when it is the default. Before,
    /// only custom servers were listed, so Codex, Claude Code and CUA
    /// could not be enabled for the first turn.
    #[test]
    fn test_draft_rows_include_the_curated_integrations() {
        use maple_agent::agent::{AgentIntegrationAvailability, AgentIntegrationBackend};
        let rows = draft_mcp_rows(
            vec![mcp_server("docs", true), mcp_server("Cua Driver", true)],
            &[
                integration_card(
                    "cua-driver",
                    AgentIntegrationAvailability::Available,
                    Some(AgentIntegrationBackend::Embedded),
                    true,
                ),
                integration_card("codex", AgentIntegrationAvailability::Available, None, true),
                integration_card(
                    "claude",
                    AgentIntegrationAvailability::SetupRequired,
                    None,
                    true,
                ),
                integration_card("other", AgentIntegrationAvailability::Available, None, true),
            ],
        );
        let summary: Vec<(&str, AgentSessionIntegrationKind, bool, bool, &str)> = rows
            .iter()
            .map(|row| {
                (
                    row.name.as_str(),
                    row.kind,
                    row.enabled,
                    row.available,
                    row.transport.as_str(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                (
                    "docs",
                    AgentSessionIntegrationKind::Mcp,
                    true,
                    true,
                    "stdio"
                ),
                (
                    "codex",
                    AgentSessionIntegrationKind::ExternalAgent,
                    false,
                    true,
                    "external_agent"
                ),
                (
                    "claude",
                    AgentSessionIntegrationKind::ExternalAgent,
                    false,
                    false,
                    "external_agent"
                ),
                (
                    "cua-driver",
                    AgentSessionIntegrationKind::Mcp,
                    true,
                    true,
                    "embedded"
                ),
            ],
            "the CUA-named custom server is shadowed; Claude Code is listed but not installed; an unknown card is not an external agent"
        );
        assert_eq!(rows[1].display_name, "CODEX");

        // Without a stored CUA choice the row is unconfigured and off; an
        // embedded choice whose grants are missing is on but unavailable.
        let rows = draft_mcp_rows(
            Vec::new(),
            &[integration_card(
                "cua-driver",
                AgentIntegrationAvailability::NotDetected,
                None,
                false,
            )],
        );
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].enabled && !rows[0].available);
        assert_eq!(rows[0].transport, "unconfigured");
        let rows = draft_mcp_rows(
            Vec::new(),
            &[integration_card(
                "cua-driver",
                AgentIntegrationAvailability::SetupRequired,
                Some(AgentIntegrationBackend::Embedded),
                true,
            )],
        );
        assert!(rows[0].enabled && !rows[0].available);
        assert_eq!(rows[0].transport, "embedded");
        // No catalog at all: the servers alone, as before.
        let rows = draft_mcp_rows(vec![mcp_server("docs", false)], &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "docs");
    }

    /// The integrations chip on the draft goes through the same handler
    /// as on a task. It shows the rows a new task would get, read from the
    /// configuration; a switch flipped there survives the chip being
    /// opened again and decides the create request; and the Send button
    /// starts the create with that request. Before, opening the chip
    /// emptied the rows ("No integrations available for this task").
    #[gpui::test]
    fn test_integrations_chip_lists_and_toggles_the_drafts_rows(cx: &mut TestAppContext) {
        let backend = backend_with_mcp_servers(
            cx,
            "draft-chip",
            vec![mcp_server("docs", true), mcp_server("wiki", false)],
        );
        let (chat, cx) = chat_window_in(cx, backend, |this, cx| {
            this.project_root = Some(mcp_project("draft-chip"));
            this.selected_session = None;
            this.new_session(cx);
        });
        wait_until(&chat, cx, "the draft's rows loading", |this| {
            this.draft_mcp_defaults.is_some()
        });
        chat.update(cx, |this, _| {
            let rows: Vec<(&str, bool)> = this
                .session_mcp
                .iter()
                .map(|row| (row.name.as_str(), row.enabled))
                .collect();
            // The curated CUA row follows the servers, unconfigured here;
            // no external agent is enabled in this fresh configuration.
            assert_eq!(
                rows,
                vec![("docs", true), ("wiki", false), ("cua-driver", false)]
            );
            assert!(!this.session_mcp[2].available);
            assert_eq!(this.mcp_enabled_count, 1);
        });

        press(cx, "mcp-menu");
        assert_eq!(open_menu(&chat, cx), "Integrations");
        chat.update(cx, |this, _| {
            assert_eq!(this.session_mcp.len(), 3, "opening the chip keeps the rows");
        });
        assert!(
            cx.debug_bounds("mcp-Mcp-docs").is_some(),
            "the docs row is in the menu"
        );

        press(cx, "mcp-Mcp-docs");
        chat.update(cx, |this, _| {
            assert!(!this.session_mcp[0].enabled);
            assert_eq!(this.mcp_enabled_count, 0);
            assert_eq!(
                this.new_session_request().unwrap().mcp_server_names,
                Some(Vec::new())
            );
        });

        // Opening the chip again reloads the rows; the switch stays off.
        chat.update(cx, |this, cx| {
            this.popup.close(cx);
            this.draft_mcp_defaults = None;
        });
        press(cx, "mcp-menu");
        wait_until(&chat, cx, "the rows reloading", |this| {
            this.draft_mcp_defaults.is_some()
        });
        chat.update(cx, |this, _| {
            assert!(
                !this.session_mcp[0].enabled,
                "the toggle survived the reload"
            );
        });
        press(cx, "mcp-Mcp-wiki");
        chat.update(cx, |this, _| {
            assert_eq!(
                this.new_session_request().unwrap().mcp_server_names,
                Some(vec!["wiki".to_string()])
            );
        });

        // The Send button starts the create with the draft's request.
        chat.update(cx, |this, cx| {
            this.popup.close(cx);
            this.composer
                .clone()
                .unwrap()
                .update(cx, |input, cx| input.set_text("hello", cx));
        });
        press(cx, "send-message");
        // The create runs against a backend without a runtime and fails
        // at once; that failure is the proof the button reached it.
        wait_until(&chat, cx, "the create settling", |this| {
            !this.session_setup_pending
        });
        chat.update(cx, |this, cx| {
            assert!(this.notice.is_some(), "the create's failure is reported");
            assert_eq!(
                this.composer.clone().unwrap().read(cx).text(),
                "hello",
                "the text is back in the composer"
            );
            assert!(this.draft, "the draft is still on screen");
        });
    }

    #[gpui::test]
    fn test_draft_mcp_defaults_follow_the_project_and_save_the_last_click(cx: &mut TestAppContext) {
        let backend = backend_with_mcp_servers(
            cx,
            "project-mcp-drafts",
            vec![mcp_server("docs", true), mcp_server("wiki", false)],
        );
        let alpha = mcp_project("draft-alpha");
        let beta = mcp_project("draft-beta");
        let (chat, cx) = chat_window_in(cx, backend, |this, cx| {
            this.project_root = Some(alpha.clone());
            this.new_session(cx);
        });
        wait_until(&chat, cx, "alpha's integration defaults", |this| {
            this.draft_mcp_defaults.is_some()
        });
        chat.update(cx, |this, cx| {
            this.web_enabled = false;
            this.selected_model = Some("chosen-model".to_string());
            this.composer
                .clone()
                .unwrap()
                .update(cx, |input, cx| input.set_text("keep this draft", cx));
            for enabled in [false, true, false] {
                this.toggle_session_mcp(
                    "docs".to_string(),
                    AgentSessionIntegrationKind::Mcp,
                    enabled,
                    cx,
                );
            }
            assert_eq!(this.project_mcp_saves.len(), 2, "waiting clicks coalesce");
            assert!(!this.project_mcp_saves.back().unwrap().enabled);
            assert_eq!(
                this.new_session_request().unwrap().mcp_server_names,
                Some(Vec::new()),
                "pending project writes do not change the first task's selection"
            );
            this.toggle_session_mcp(
                CUA_DRIVER_ID.to_string(),
                AgentSessionIntegrationKind::Mcp,
                true,
                cx,
            );
            assert_eq!(this.project_mcp_saves.len(), 2, "CUA is task state");
            this.land_project_selection(beta.clone(), this.selection_generation, cx);
            assert!(this.draft_mcp_defaults.is_none());
            assert!(
                this.session_mcp
                    .iter()
                    .all(|row| !is_custom_mcp(&row.name, row.kind))
            );
        });
        wait_until(&chat, cx, "beta's defaults and alpha's saves", |this| {
            this.draft_mcp_defaults.is_some() && this.project_mcp_saves.is_empty()
        });
        chat.update(cx, |this, cx| {
            assert!(
                this.session_mcp
                    .iter()
                    .find(|row| row.name == "docs")
                    .unwrap()
                    .enabled
            );
            assert!(
                !this
                    .session_mcp
                    .iter()
                    .find(|row| row.name == "wiki")
                    .unwrap()
                    .enabled
            );
            assert!(
                this.session_mcp
                    .iter()
                    .find(|row| row.name == CUA_DRIVER_ID)
                    .unwrap()
                    .enabled
            );
            assert_eq!(
                this.composer.clone().unwrap().read(cx).text(),
                "keep this draft"
            );
            assert_eq!(this.selected_model.as_deref(), Some("chosen-model"));
            assert!(!this.web_enabled);
            this.land_project_selection(alpha.clone(), this.selection_generation, cx);
        });
        wait_until(&chat, cx, "alpha's saved default", |this| {
            this.draft_mcp_defaults.is_some()
        });
        chat.update(cx, |this, cx| {
            assert!(
                !this
                    .session_mcp
                    .iter()
                    .find(|row| row.name == "docs")
                    .unwrap()
                    .enabled
            );
            this.new_session(cx);
        });
        wait_until(&chat, cx, "another draft in alpha", |this| {
            this.draft_mcp_defaults.is_some()
        });
        chat.update(cx, |this, _| {
            assert_eq!(
                this.new_session_request().unwrap().mcp_server_names,
                Some(Vec::new())
            );
            assert!(this.notice.is_none(), "project saves succeeded");
        });
    }

    #[gpui::test]
    fn test_fresh_draft_send_waits_for_pending_project_mcp_choices(cx: &mut TestAppContext) {
        let backend = backend_with_mcp_servers(
            cx,
            "pending-project-mcp-send",
            vec![mcp_server("docs", false)],
        );
        let (chat, cx) = chat_window_in(cx, backend, |this, cx| {
            this.project_root = Some(mcp_project("pending-send"));
            this.new_session(cx);
        });
        wait_until(&chat, cx, "initial draft defaults", |this| {
            this.draft_mcp_defaults.is_some()
        });
        chat.update(cx, |this, cx| {
            for enabled in [true, false] {
                this.toggle_session_mcp(
                    "docs".into(),
                    AgentSessionIntegrationKind::Mcp,
                    enabled,
                    cx,
                );
            }
            this.new_session(cx);
            this.send_text("hello".into(), cx);
            assert!(this.first_send_waiting_for_mcp);
            assert!(this.session_setup_pending);
            assert!(this.pending_first_send.is_some());
        });
        // No runtime is started: the resumed create fails, returning the text.
        wait_until(&chat, cx, "resumed first send", |this| {
            !this.session_setup_pending && this.project_mcp_saves.is_empty()
        });
        chat.update(cx, |this, cx| {
            assert!(!this.first_send_waiting_for_mcp);
            assert!(this.draft_mcp_defaults.is_some());
            assert_eq!(
                this.new_session_request().unwrap().mcp_server_names,
                Some(Vec::new())
            );
            assert_eq!(this.composer.clone().unwrap().read(cx).text(), "hello");
            assert!(
                this.notice.is_some(),
                "the create resumed and reported its failure"
            );

            for enabled in [true, false] {
                this.toggle_session_mcp(
                    "docs".into(),
                    AgentSessionIntegrationKind::Mcp,
                    enabled,
                    cx,
                );
            }
            this.new_session(cx);
            this.send_text("another message".into(), cx);
            assert!(this.first_send_waiting_for_mcp);
            this.select_session("another-task", cx);
            assert!(!this.first_send_waiting_for_mcp);
            assert!(
                !this.session_setup_pending,
                "navigation releases the catalog wait"
            );
            assert!(this.pending_first_send.is_none());
        });
    }

    #[gpui::test]
    fn reconnect_cancels_a_draft_catalog_wait_and_reloads_project_defaults(
        cx: &mut TestAppContext,
    ) {
        let backend =
            backend_with_mcp_servers(cx, "project-mcp-reconnect", vec![mcp_server("docs", true)]);
        let root = mcp_project("project-mcp-reconnect");
        let (chat, cx) = chat_window_in(cx, backend.clone(), |this, cx| {
            this.project_root = Some(root.clone());
            this.new_session(cx);
        });
        wait_until(&chat, cx, "initial project defaults", |this| {
            this.draft_mcp_defaults.is_some()
        });
        chat.update(cx, |this, cx| {
            // A first send is waiting for its draft's catalog, before any task
            // or prompt has reached the host.
            this.draft_mcp_defaults = None;
            this.session_setup_pending = true;
            this.first_send_waiting_for_mcp = true;
            this.pending_first_send = Some(FirstSend::Message {
                text: "keep this unsent message".into(),
                steer: false,
            });
            this.fill_empty_composer("keep this unsent message", cx);
            this.handle_service_event(AgentServiceEvent::HostReconnecting, cx);
            assert!(
                !this.session_setup_pending,
                "a disconnected catalog wait must end"
            );
            assert!(!this.first_send_waiting_for_mcp);
            assert!(
                this.pending_first_send.is_none(),
                "recovery must not send later"
            );
        });

        // Another client changes the host-owned default while desktop is away.
        let (sender, receiver) = std::sync::mpsc::channel();
        let target = root.clone();
        backend.clone().spawn(async move {
            let _ = sender.send(
                backend
                    .set_project_mcp_server_enabled("user", &target, "docs", false)
                    .await,
            );
        });
        receiver
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap()
            .unwrap();
        chat.update(cx, |this, cx| {
            this.apply_recovery_snapshot(
                crate::backend::RecoverySnapshot {
                    bootstrap: crate::backend::LocalBootstrap {
                        project_root: Some(root.clone()),
                        recent_roots: Vec::new(),
                        latest: None,
                        sessions: Vec::new(),
                        questions: Vec::new(),
                    },
                    status: maple_agent::agent::AgentRuntimeStatus {
                        running: true,
                        project_root: Some(root),
                        model: None,
                        active_runs: HashMap::new(),
                    },
                    selected: None,
                },
                this.selection_generation,
                cx,
            );
        });
        wait_until(&chat, cx, "project defaults after reconnect", |this| {
            this.draft_mcp_defaults.is_some()
        });
        chat.update(cx, |this, cx| {
            assert_eq!(this.draft_mcp_names(), Some(Vec::new()));
            assert_eq!(
                this.composer_text(cx).as_deref(),
                Some("keep this unsent message")
            );
            assert!(this.selected_session.is_none());
            assert!(!this.session_setup_pending);
            assert!(this.pending_first_send.is_none());
        });
    }

    #[gpui::test]
    fn test_removing_project_cancels_catalog_wait_and_opens_empty_fallback(
        cx: &mut TestAppContext,
    ) {
        let backend = backend_with_mcp_servers(cx, "remove-waiting-project", Vec::new());
        let alpha = mcp_project("remove-waiting-alpha");
        let beta = mcp_project("remove-waiting-beta");
        let (chat, cx) = chat_window_in(cx, backend, |this, cx| {
            this.selected_session = None;
            this.project_root = Some(alpha.clone());
            this.recent_roots = vec![recent_root(&alpha), recent_root(&beta)];
            this.draft = true;
            this.session_setup_pending = true;
            this.first_send_waiting_for_mcp = true;
            this.pending_first_send = Some(FirstSend::Message {
                text: "keep the unsent prompt".into(),
                steer: false,
            });
            this.composer.clone().unwrap().update(cx, |input, cx| {
                input.set_text("keep the unsent prompt", cx);
            });
            this.archive_root(&alpha, cx);
        });
        wait_until(&chat, cx, "empty fallback after project removal", |this| {
            this.project_root.as_deref() == Some(beta.as_str())
                && this.draft
                && this.draft_mcp_defaults.is_some()
        });
        chat.update(cx, |this, _| {
            assert!(!this.first_send_waiting_for_mcp);
            assert!(!this.session_setup_pending);
            assert!(this.pending_first_send.is_none());
            assert_eq!(this.selected_session, None);
            assert!(
                this.sessions.is_empty(),
                "no task was created for the withdrawn send"
            );
            assert!(
                this.prompt_history
                    .iter()
                    .any(|text| text == "keep the unsent prompt")
            );
        });
    }

    /// An empty task persisted by an older build is not opened by the
    /// boot auto-select: the new-task screen stands in for it.
    #[gpui::test]
    fn test_auto_select_skips_an_empty_task(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.project_root = Some("/work/alpha".to_string());
            this.selected_session = None;
            let mut empty = summary_at("blank", "New Task", "/work/alpha");
            empty.message_count = 0;
            let requested = this.selection_generation;

            this.apply_session_list(vec![empty], requested, cx);

            assert_eq!(this.selected_session, None);
            assert!(!this.session_setup_pending);
            assert_eq!(this.selected_title.as_ref(), "New Task");
            assert!(this.draft, "the fallback is a draft like any other");

            // On a fresh screen (the draft left), a task with a message
            // under the same root still opens.
            this.clear_selected_session_presentation(cx);
            this.selection_generation = requested;
            this.apply_session_list(
                vec![
                    summary_at("blank", "New Task", "/work/alpha"),
                    summary_at("real", "Real", "/work/alpha"),
                ]
                .into_iter()
                .map(|mut summary| {
                    if summary.id == "blank" {
                        summary.message_count = 0;
                    }
                    summary
                })
                .collect(),
                requested,
                cx,
            );
            assert_eq!(this.loading_session.as_deref(), Some("real"));
        });
    }

    /// "New Task" during boot is a draft: the boot's list refresh, which
    /// captures the generation after the click, and the local bootstrap's
    /// newest transcript both leave it alone. Before, the old task opened
    /// under the text being typed, and Enter sent it there.
    #[gpui::test]
    fn test_boot_auto_select_does_not_replace_a_draft(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.project_root = Some("/work/alpha".to_string());
            this.selected_session = None;
            this.booting = true;

            this.new_session(cx);
            assert!(this.draft, "New Task works while the runtime boots");
            assert_eq!(this.selected_session, None);

            let generation = this.selection_generation;
            let old = summary_at("old", "Old", "/work/alpha");
            this.apply_session_list(vec![old.clone()], generation, cx);
            assert!(this.draft);
            assert_eq!(this.selected_session, None);
            assert_eq!(this.loading_session, None, "no task load started");
            assert_eq!(this.sessions.len(), 1, "the list still updates");

            this.apply_bootstrap(
                crate::backend::LocalBootstrap {
                    questions: Vec::new(),
                    project_root: Some("/work/alpha".to_string()),
                    sessions: vec![old.clone()],
                    recent_roots: Vec::new(),
                    latest: Some(maple_agent::agent::AgentSessionDetail {
                        timeline_revision: 0,
                        session: old.clone(),
                        timeline: vec![user_item("u1", "earlier")],
                        mcp_errors: Vec::new(),
                        queue: maple_agent::agent::AgentDesktopQueueSnapshot {
                            revision: 0,
                            items: Vec::new(),
                        },
                    }),
                },
                HashMap::new(),
                cx,
            );
            assert!(this.draft);
            assert_eq!(this.selected_session, None);
            assert!(this.timeline.is_empty());

            // Opening a task ends the draft; the next list may auto-select.
            this.set_active_session(old, Vec::new(), HashMap::new(), cx);
            assert!(!this.draft);
            this.clear_selected_session_presentation(cx);
            assert!(!this.draft);
            let generation = this.selection_generation;
            this.apply_session_list(
                vec![summary_at("old", "Old", "/work/alpha")],
                generation,
                cx,
            );
            assert_eq!(this.loading_session.as_deref(), Some("old"));
        });
    }

    /// A project switched while a draft shows moves the draft: its chips
    /// stay, and the new project's latest task is not opened over it.
    /// Without a draft the switch clears the screen and the list refresh
    /// auto-selects as before.
    #[gpui::test]
    fn test_project_switch_keeps_the_draft(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.project_root = Some("/work/alpha".to_string());
            this.default_web_enabled = true;
            this.new_session(cx);
            this.set_web_enabled(false, cx);
            let generation = this.selection_generation;

            this.land_project_selection("/work/beta".to_string(), generation, cx);

            assert!(this.draft);
            assert_eq!(this.selected_session, None);
            assert_eq!(this.project_root.as_deref(), Some("/work/beta"));
            assert!(!this.web_enabled, "the draft's chips stay");
            let generation = this.selection_generation;
            this.apply_session_list(vec![summary_at("b1", "Beta", "/work/beta")], generation, cx);
            assert_eq!(this.selected_session, None);
            assert_eq!(this.loading_session, None);

            // A stale landing (a task clicked meanwhile) changes nothing.
            this.land_project_selection("/work/stale".to_string(), generation - 1, cx);
            assert_eq!(this.project_root.as_deref(), Some("/work/beta"));

            this.set_active_session(
                summary_at("b1", "Beta", "/work/beta"),
                Vec::new(),
                HashMap::new(),
                cx,
            );
            let generation = this.selection_generation;
            this.land_project_selection("/work/gamma".to_string(), generation, cx);
            assert!(!this.draft);
            assert_eq!(this.selected_session, None);
            let generation = this.selection_generation;
            this.apply_session_list(
                vec![summary_at("g1", "Gamma", "/work/gamma")],
                generation,
                cx,
            );
            assert_eq!(this.loading_session.as_deref(), Some("g1"));
        });
    }

    #[gpui::test]
    fn test_history_reload_keeps_a_task_switch_alive(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.select_session("s2", cx);
            let switching = this.selection_generation;
            // Compaction on the task still on screen must not cancel the
            // load of the task the user just clicked.
            this.reload_timeline("s1", cx);
            assert_eq!(this.selection_generation, switching);
            let reload = this.reload_generation;
            this.select_session("s3", cx);
            assert_ne!(this.selection_generation, switching);
            assert_ne!(this.reload_generation, reload, "a select drops reloads");
        });
    }

    #[gpui::test]
    fn test_second_picker_click_is_ignored(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.popup.open(ChatPopup::Project, cx);
            this.choose_root_dialog(cx);
            let first = this.project_folder_dialog.clone().expect("folder dialog");
            assert!(!this.popup.is_open(&ChatPopup::Project));
            this.choose_root_dialog(cx);
            assert_eq!(this.project_folder_dialog.as_ref(), Some(&first));
            assert_eq!(this.selected_session.as_deref(), Some("s1"));
            this.set_visible(false, cx);
            assert!(this.project_folder_dialog.is_none());
        });
    }

    #[gpui::test]
    fn catalog_folder_picker_does_not_activate_runtime_or_replace_a_task_before_confirmation(
        cx: &mut TestAppContext,
    ) {
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.navigation_only = true;
            this.navigation_ready = true;
            this.draft = false;
            let generation = this.selection_generation;
            this.navigate(Destination::DraftNewProject, cx);
            assert!(this.navigation_only);
            assert!(!this.draft);
            assert_eq!(this.selected_session.as_deref(), Some("s1"));
            assert_eq!(this.selection_generation, generation);
            assert!(this.project_folder_dialog.is_some());
        });
    }

    #[gpui::test]
    fn stale_folder_selection_cannot_change_a_newer_navigation(cx: &mut TestAppContext) {
        let screen = screen(cx);
        let dialog = screen.update(cx, |this, cx| {
            this.choose_root_dialog(cx);
            this.project_folder_dialog.clone().unwrap()
        });
        screen.update(cx, |this, _| {
            this.begin_navigation();
            this.selected_session = Some("newer".into());
        });
        dialog.update(cx, |_, cx| {
            cx.emit(project_folder::FolderDialogEvent::Selected(
                crate::backend::SelectedProject {
                    project_root: "/stale".into(),
                    project: recent_root("/stale"),
                },
            ))
        });
        cx.run_until_parked();
        screen.update(cx, |this, _| {
            assert_eq!(this.selected_session.as_deref(), Some("newer"));
            assert!(!this.recent_roots.iter().any(|root| root.path == "/stale"));
        });
    }

    #[gpui::test]
    fn folder_picker_blocks_initial_auto_selection_and_stale_cancel_still_dismisses(
        cx: &mut TestAppContext,
    ) {
        let screen = screen(cx);
        let dialog = screen.update(cx, |this, cx| {
            this.selected_session = None;
            this.draft = false;
            this.project_root = Some("/tmp/proj".into());
            this.choose_root_dialog(cx);
            this.apply_session_list(
                vec![summary("latest", "Latest task")],
                this.selection_generation,
                cx,
            );
            assert!(this.selected_session.is_none());
            assert!(this.loading_session.is_none());
            this.selection_generation += 1;
            this.project_folder_dialog.clone().unwrap()
        });
        dialog.update(cx, |_, cx| {
            cx.emit(project_folder::FolderDialogEvent::Cancelled)
        });
        cx.run_until_parked();
        assert!(screen.update(cx, |this, _| this.project_folder_dialog.is_none()));
    }

    #[test]
    fn test_git_branch_reads_head_and_worktree_pointer() {
        struct TempDir(std::path::PathBuf);
        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let branch = |root: &std::path::Path| git_dir(root).as_deref().and_then(git_branch);
        let guard =
            TempDir(std::env::temp_dir().join(format!("maple-branch-{}", std::process::id())));
        let dir = &guard.0;
        let repo = dir.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        assert_eq!(branch(&repo).as_deref(), Some("feature/x"));

        std::fs::write(repo.join(".git/HEAD"), "0123456789abcdef\n").unwrap();
        assert_eq!(branch(&repo).as_deref(), Some("0123456"));

        std::fs::write(repo.join(".git/HEAD"), "garbage-héad\n").unwrap();
        assert_eq!(branch(&repo), None);
        std::fs::write(repo.join(".git/HEAD"), "0123456789abcdef\n").unwrap();

        let worktree = dir.join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", repo.join(".git").display()),
        )
        .unwrap();
        assert_eq!(branch(&worktree).as_deref(), Some("0123456"));

        let plain = dir.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(branch(&plain), None);
    }

    /// Issue #945: the branch watcher must ignore access-only HEAD events
    /// (open, read, close). The branch read they trigger emits those same
    /// events again under Linux inotify, looping at ~200% CPU while idle.
    #[test]
    fn test_head_watch_ignores_read_access_events() {
        use notify::EventKind;
        use notify::event::{
            AccessKind, AccessMode, CreateKind, DataChange, Flag, MetadataKind, ModifyKind,
            RemoveKind, RenameMode,
        };

        let head = std::path::PathBuf::from("/repo/.git/HEAD");
        let event = |kind: EventKind| notify::Event::new(kind).add_path(head.clone());

        // The read side of the loop, as emitted by Linux inotify.
        for kind in [
            EventKind::Access(AccessKind::Open(AccessMode::Read)),
            EventKind::Access(AccessKind::Read),
            EventKind::Access(AccessKind::Close(AccessMode::Read)),
            EventKind::Access(AccessKind::Close(AccessMode::Write)),
            EventKind::Access(AccessKind::Any),
            EventKind::Access(AccessKind::Other),
        ] {
            assert!(!head_change_event(&event(kind)), "access {kind:?}");
        }

        // Real changes still refresh the label: in-place write, create,
        // remove, and the rename pair of an atomic replacement, plus the
        // unclassified kinds imprecise backends emit for real changes.
        for kind in [
            EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            EventKind::Modify(ModifyKind::Any),
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)),
            EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            EventKind::Modify(ModifyKind::Name(RenameMode::To)),
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
            EventKind::Create(CreateKind::File),
            EventKind::Create(CreateKind::Any),
            EventKind::Remove(RemoveKind::File),
            EventKind::Remove(RemoveKind::Any),
            EventKind::Any,
            EventKind::Other,
        ] {
            assert!(head_change_event(&event(kind)), "change {kind:?}");
        }

        // Unrelated paths never refresh, even with a change kind.
        let unrelated =
            notify::Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content)))
                .add_path(std::path::PathBuf::from("/repo/.git/index"));
        assert!(!head_change_event(&unrelated));

        // A required rescan refreshes even without a HEAD path.
        let event = notify::Event::new(EventKind::Other).set_flag(Flag::Rescan);
        assert!(head_change_event(&event));
    }

    #[gpui::test]
    fn test_escape_closes_root_menu(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.popup.open(ChatPopup::Project, cx);
            this.escape(cx);
            assert_eq!(this.popup.open_key(), None);
        });
    }

    #[gpui::test]
    fn test_idle_events_do_not_redraw(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            let status = maple_agent::agent::AgentRuntimeStatus {
                running: true,
                project_root: None,
                model: None,
                active_runs: HashMap::new(),
            };
            assert!(
                !this.apply_service_event(AgentServiceEvent::RuntimeStatus(status.clone()), cx)
            );
            assert!(
                this.apply_service_event(AgentServiceEvent::SessionCreated(summary("s1", "A")), cx)
            );
            assert!(
                !this
                    .apply_service_event(AgentServiceEvent::SessionCreated(summary("s1", "A")), cx)
            );
            assert!(this.apply_service_event(
                AgentServiceEvent::SessionCreated(summary("s1", "A renamed")),
                cx
            ));
        });
    }

    #[gpui::test]
    fn test_timeline_index_tracks_items(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, _cx| {
            assert_eq!(
                this.apply_timeline_item("s1", item("a", "message", Some("1"))),
                Some(0)
            );
            assert_eq!(
                this.apply_timeline_item("s1", item("b", "message", Some("2"))),
                Some(1)
            );
            assert_eq!(
                this.apply_timeline_item("s1", item("a", "message", Some("3"))),
                Some(0)
            );
            assert_eq!(this.timeline_index.get("a"), Some(&(0, 1)));
            assert_eq!(this.timeline_index.get("b"), Some(&(1, 0)));
            this.replace_timeline(vec![item("z", "message", Some("9"))]);
            assert_eq!(this.timeline_index.get("z"), Some(&(0, 0)));
            assert!(!this.timeline_index.contains_key("a"));
        });
    }

    #[gpui::test]
    fn test_attachments_requested_from_the_arriving_item(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            let mut sent = user_item("u1", "see this");
            sent.input = Some(serde_json::json!({
                "imageAttachments": [{"id": "att-1", "name": "a.png"}]
            }));
            this.apply_incoming_item("s1", sent, cx);
            assert!(this.attachment_requests.contains("att-1"));
        });
    }

    #[test]
    fn test_diff_lines_stop_at_the_cap() {
        let mut tool = item("edit", "tool", None);
        tool.input = Some(serde_json::json!({
            "path": "big.txt",
            "content": (0..1000).map(|n| n.to_string()).collect::<Vec<_>>().join("\n"),
        }));
        let lines = diff_lines_for(&tool);
        assert_eq!(lines.len(), MAX_DIFF_LINES);
        assert_eq!(lines[0], (' ', SharedString::from("big.txt")));
        assert_eq!(lines[1], ('+', SharedString::from("0")));
    }

    #[test]
    fn test_markdown_cache_keys_on_revision() {
        // Unattached: every parse is inline.
        let cache = MarkdownCache::default();
        let first = cache.get("m", MarkdownKind::Body, 0, "hello");
        assert!(Arc::ptr_eq(
            &first,
            &cache.get("m", MarkdownKind::Body, 0, "hello")
        ));
        // Same length, new revision: parsed again.
        assert!(!Arc::ptr_eq(
            &first,
            &cache.get("m", MarkdownKind::Body, 1, "jello")
        ));
        // Kinds do not share entries.
        assert!(!Arc::ptr_eq(
            &first,
            &cache.get("m", MarkdownKind::ToolOutput, 0, "hello")
        ));
    }

    #[gpui::test]
    fn test_markdown_cache_parses_long_sources_in_the_background(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        // The helper builds an unattached screen; attach so parses spawn.
        screen.update(cx, |this, cx| {
            this.markdown_cache
                .attach(cx.entity().downgrade(), cx.to_async());
        });
        let long = "**bold** ".repeat(INLINE_PARSE_LIMIT / 4);
        let longer = format!("{long} more");
        let (provisional, coalesced) = screen.update(cx, |this, _cx| {
            // A short cold source parses inline and is final at once.
            this.markdown_cache.get("s", MarkdownKind::Body, 0, "hello");
            assert!(
                this.markdown_cache
                    .is_current("s", MarkdownKind::Body, 0, 5)
            );
            // A long cold source shows its raw text while the parse runs.
            let provisional = this.markdown_cache.get("m", MarkdownKind::Body, 0, &long);
            assert_eq!(provisional.blocks.len(), 1);
            assert!(
                this.markdown_cache
                    .is_current("m", MarkdownKind::Body, 0, long.len())
            );
            // A chunk that lands mid-parse is served the same document
            // instead of starting a second parse.
            let coalesced = this.markdown_cache.get("m", MarkdownKind::Body, 1, &longer);
            (provisional, coalesced)
        });
        assert!(Arc::ptr_eq(&provisional, &coalesced));
        cx.run_until_parked();
        screen.update(cx, |this, _cx| {
            // The parse landed for the source it started with; the newer
            // source parses on the next render, which asks again.
            assert!(
                this.markdown_cache
                    .is_current("m", MarkdownKind::Body, 0, long.len())
            );
            let parsed = this.markdown_cache.get("m", MarkdownKind::Body, 0, &long);
            assert!(!Arc::ptr_eq(&provisional, &parsed));
            assert!(!parsed.blocks.is_empty());
            let next = this.markdown_cache.get("m", MarkdownKind::Body, 1, &longer);
            assert!(
                Arc::ptr_eq(&parsed, &next),
                "the previous document stays up while the next parse runs"
            );
        });
        cx.run_until_parked();
        screen.update(cx, |this, _cx| {
            assert!(
                this.markdown_cache
                    .is_current("m", MarkdownKind::Body, 1, longer.len())
            );
        });
    }

    #[test]
    fn test_ordinal_bases_are_stable_and_spaced() {
        let cache = MarkdownCache::default();
        let a = cache.ordinal_for("a");
        let b = cache.ordinal_for("b");
        assert_eq!(cache.ordinal_for("a"), a);
        assert!(b >= a + ORDINAL_SPACING);
        cache.clear();
        assert_eq!(cache.ordinal_for("c"), a);
    }

    #[gpui::test]
    fn test_composer_change_updates_slash_entries(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        let input = cx.new(|cx| TextInput::new("", cx));
        screen.update(cx, |this, cx| {
            assert!(!this.composer_has_text);
            input.update(cx, |input, cx| input.set_text("/co", cx));
            this.composer_changed(&input, cx);
            assert!(this.composer_has_text);
            assert_eq!(this.slash_entries.len(), 1);
            assert_eq!(this.slash_entries[0].name, "compact");
            input.update(cx, |input, cx| input.set_text("/compact now", cx));
            this.composer_changed(&input, cx);
            assert!(this.slash_entries.is_empty());
            input.update(cx, |input, cx| input.clear(cx));
            this.composer_changed(&input, cx);
            assert!(!this.composer_has_text);
        });
    }
    #[test]
    fn test_tool_label_title_strips_descriptive_detail() {
        // Known friendly labels drop the ": detail" part before the model
        // summary replaces the title.
        assert_eq!(tool_label_title("Terminal: cargo test"), "Terminal");
        assert_eq!(tool_label_title("Read file: src/main.rs"), "Read file");
        assert_eq!(tool_label_title("Web Search: rust async"), "Web Search");
        assert_eq!(tool_label_title("Terminal"), "Terminal");
        // Titles without a known label stay as-is: skill loads, generated
        // titles, and raw or extension-prefixed tool names.
        assert_eq!(
            tool_label_title("Loading skill: rust-error-handling"),
            "Loading skill: rust-error-handling"
        );
        assert_eq!(tool_label_title("Loaded skill: x"), "Loaded skill: x");
        assert_eq!(tool_label_title("developer: shell"), "developer: shell");
        assert_eq!(
            tool_label_title("todo write: one, two"),
            "todo write: one, two"
        );
    }

    #[gpui::test]
    fn test_switcher_lists_saved_roots_then_fresh_alphabetically(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.recent_roots = vec![recent_root("/z")];
            this.sessions = vec![
                summary_at("s1", "One", "/m"),
                summary_at("s2", "Two", "/a"),
                summary_at("s3", "Three", "/m"),
            ];
            this.sync_sidebar(cx);
            // Saved roots keep their order; roots only tasks know about append
            // alphabetically so their position never moves.
            assert_eq!(this.switcher_root_paths(cx), ["/z", "/a", "/m"]);
        });
    }

    /// Tasks split into the inbox sections: pinned tasks first, the
    /// active inbox, then the settled rest, each newest activity first.
    #[gpui::test]
    fn test_sections_split_pinned_active_and_settled(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            let mut idle = summary_at("s1", "Idle", "/a");
            idle.updated_ms = 10;
            let mut newer = summary_at("s2", "Newer", "/a");
            newer.updated_ms = 20;
            this.sessions = vec![idle, newer];
            this.sidebar.update(cx, |sidebar, _| {
                sidebar.set_pinned_tasks_for_test(vec!["s1".to_string()]);
            });
            this.active_runs
                .insert("s2".to_string(), "run-1".to_string());
            this.sync_sidebar(cx);
            assert_eq!(this.sidebar_pinned(cx), vec![0]);
            assert_eq!(this.sidebar_active(cx), vec![1]);
            assert_eq!(this.sidebar_settled(cx), Vec::<usize>::new());

            // A task with no live run and no unseen completion is active
            // until it is settled away by hand; reading it never settles it.
            this.active_runs.clear();
            this.sidebar.update(cx, |sidebar, _| {
                sidebar.set_pinned_tasks_for_test(Vec::new());
            });
            this.settle_task("s1", cx);
            assert_eq!(this.sidebar_active(cx), vec![1]);
            assert_eq!(this.sidebar_settled(cx), vec![0]);

            // Sections read newest activity first.
            let mut older = summary_at("s3", "Older", "/b");
            older.updated_ms = 5;
            this.sessions.push(older);
            this.sync_sidebar(cx);
            assert_eq!(this.sidebar_active(cx), vec![1, 2]);
            assert_eq!(this.sidebar_settled(cx), vec![0]);
        });
    }

    /// A new task is a draft until its first message is sent: it stays
    /// out of the sidebar, then joins the active inbox once it lands.
    #[gpui::test]
    fn test_draft_tasks_stay_out_until_the_first_message(cx: &mut TestAppContext) {
        use maple_agent::agent::AgentRunEvent;

        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            let mut draft = summary_at("s1", "New task", "/a");
            draft.message_count = 0;
            this.sessions = vec![draft, summary_at("s2", "Real task", "/a")];
            this.sync_sidebar(cx);
            assert_eq!(this.sidebar_active(cx), vec![1]);
            assert!(this.sidebar_settled(cx).is_empty());
            // The draft's project still feeds the switcher.
            assert_eq!(this.switcher_root_paths(cx), ["/a"]);

            // A first run is visible before the cached count catches up.
            this.handle_run_event("s1", "run-1", AgentRunEvent::Started, cx);
            assert_eq!(this.sessions[0].message_count, 0);
            assert_eq!(this.sidebar_active(cx), vec![0, 1]);

            // The generated title arrives while that count is still zero.
            let mut named = this.sessions[0].clone();
            named.title = "First task".into();
            this.handle_run_event(
                "s1",
                "run-1",
                AgentRunEvent::SessionUpdated(named.clone()),
                cx,
            );
            assert_eq!(this.sidebar_active(cx), vec![0, 1]);

            // Completion refreshes the count before retiring the run.
            named.message_count = 2;
            this.handle_run_event("s1", "run-1", AgentRunEvent::SessionUpdated(named), cx);
            this.handle_run_event(
                "s1",
                "run-1",
                AgentRunEvent::Finished(maple_agent::agent::AgentRunTerminal::Completed),
                cx,
            );
            assert_eq!(this.sidebar_active(cx), vec![0, 1]);
        });
    }

    /// The project filter scopes the sections to one project; the
    /// switcher still lists every known root.
    #[gpui::test]
    fn test_project_filter_scopes_the_sections(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.sessions = vec![summary_at("s1", "One", "/a"), summary_at("s2", "Two", "/b")];
            this.settle_task("s1", cx);
            assert_eq!(this.sidebar_settled(cx), vec![0]);
            assert_eq!(this.sidebar_active(cx), vec![1]);
            assert_eq!(this.switcher_root_paths(cx), ["/a", "/b"]);

            this.sidebar.update(cx, |sidebar, cx| {
                sidebar.set_project_filter(Some("/b".to_string()), cx);
            });
            assert_eq!(this.sidebar_settled(cx), Vec::<usize>::new());
            assert_eq!(this.sidebar_active(cx), vec![1]);
            // The switcher is not itself filtered.
            assert_eq!(this.switcher_root_paths(cx), ["/a", "/b"]);
        });
    }

    /// A settled task sits outside the active inbox until the runtime
    /// makes it active again, by a run or by hand.
    #[gpui::test]
    fn test_settling_a_task_moves_it_out_of_the_inbox(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.sessions = vec![summary_at("s1", "One", "/a")];
            this.completed_unread_sessions.insert("s1".to_string());
            this.settle_task("s1", cx);
            // The manual settle outranks the unseen completion...
            assert_eq!(this.sidebar_settled(cx), vec![0]);
            assert!(this.sidebar_active(cx).is_empty());
            // ...but a live run is activity: the runtime makes the task
            // active, and the running set covers the frame before its
            // updated record lands.
            this.active_runs
                .insert("s1".to_string(), "run-1".to_string());
            this.sync_sidebar(cx);
            assert_eq!(this.sidebar_active(cx), vec![0]);

            this.active_runs.clear();
            this.unsettle_task("s1", cx);
            assert_eq!(this.sidebar_active(cx), vec![0]);
            assert!(this.sidebar_settled(cx).is_empty());
            // Reading the task (no live run, no unseen completion) leaves it
            // in the active inbox; only an explicit settle moves it out.
            this.completed_unread_sessions.clear();
            this.sync_sidebar(cx);
            assert_eq!(this.sidebar_active(cx), vec![0]);
            assert!(this.sidebar_settled(cx).is_empty());
        });
    }

    /// The runtime's updated records move a row between Active and
    /// Settled and back, any number of times.
    #[gpui::test]
    fn test_state_records_move_a_task_back_and_forth(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.sessions = vec![summary("s1", "One")];
            this.sync_sidebar(cx);
            assert_eq!(this.sidebar_active(cx), vec![0]);
            assert!(this.sidebar_settled(cx).is_empty());

            this.settle_task("s1", cx);
            assert_eq!(this.sidebar_settled(cx), vec![0]);
            assert!(this.sidebar_active(cx).is_empty());

            this.unsettle_task("s1", cx);
            assert_eq!(this.sidebar_active(cx), vec![0]);
            assert!(this.sidebar_settled(cx).is_empty());
            this.settle_task("s1", cx);
            this.unsettle_task("s1", cx);
            assert_eq!(this.sidebar_active(cx), vec![0]);
            assert!(this.sidebar_settled(cx).is_empty());
        });
    }

    /// The hover button steps a task one rung forward along active,
    /// settled, archived, deleted. The overflow menu reaches every later
    /// rung and comes back only to active: no settle or unsettle on an
    /// archived task, no pin on a settled one.
    #[gpui::test]
    fn test_task_ladder_actions_follow_the_rung(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.sessions = vec![
                summary("s1", "Active"),
                summary("s2", "Settled"),
                summary("s3", "Archived"),
            ];
            this.sessions[2].state = AgentTaskState::Archived;
            this.sync_sidebar(cx);
            this.settle_task("s2", cx);
            this.sidebar
                .update(cx, |sidebar, cx| sidebar.set_archived_expanded(true, cx));
            let sidebar = this.sidebar.read(cx);

            assert_eq!(sidebar.row_step_label_for_test("s1"), Some("Settle"));
            assert_eq!(
                sidebar.task_menu_labels_for_test("s1"),
                vec![
                    "Rename task",
                    "Pin task",
                    "Settle task",
                    "Archive task",
                    "Delete task"
                ]
            );

            assert_eq!(sidebar.row_step_label_for_test("s2"), Some("Archive"));
            assert_eq!(
                sidebar.task_menu_labels_for_test("s2"),
                vec!["Rename task", "Reopen task", "Archive task", "Delete task"]
            );

            assert_eq!(sidebar.row_step_label_for_test("s3"), Some("Delete"));
            assert_eq!(
                sidebar.task_menu_labels_for_test("s3"),
                vec!["Rename task", "Reopen task", "Delete task"]
            );
        });
    }

    /// The moves themselves: every move to another rung asks the runtime
    /// for that state, a move to the rung the task is on asks nothing,
    /// leaving active drops the pin at once, and delete asks the screen,
    /// which asks the user.
    #[gpui::test]
    fn test_ladder_moves_emit_the_right_events(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::<SidebarEvent>::new()));
        screen.update(cx, |this, cx| {
            let sink = events.clone();
            cx.subscribe(&this.sidebar, move |_, _, event: &SidebarEvent, _| {
                // This presentation fixture uses synthetic task IDs; pin I/O
                // has separate account-backed tests. Observe ladder intents.
                if matches!(
                    event,
                    SidebarEvent::SetState { .. } | SidebarEvent::DeleteTask(_)
                ) {
                    sink.borrow_mut().push(event.clone());
                }
            })
            .detach();
            this.sessions = vec![
                summary("s1", "Active"),
                summary("s2", "Settled"),
                summary("s3", "Archived"),
            ];
            this.sessions[1].state = AgentTaskState::Settled;
            this.sessions[2].state = AgentTaskState::Archived;
            this.sync_sidebar(cx);
            this.sidebar
                .update(cx, |sidebar, cx| sidebar.toggle_task_pin("s1", cx));
            assert_eq!(this.sidebar_pinned(cx), vec![0]);
        });
        let take = |cx: &mut TestAppContext| -> Vec<SidebarEvent> {
            cx.run_until_parked();
            std::mem::take(&mut *events.borrow_mut())
        };
        let move_task = |this: &mut ChatScreen, cx: &mut Context<ChatScreen>, id: &str, to| {
            this.sidebar.update(cx, |s, cx| s.move_task(id, to, cx));
        };
        let state_of = |emitted: &[SidebarEvent], id: &str, expected: AgentTaskState| {
            matches!(
                emitted,
                [SidebarEvent::SetState { session_id, state }] if session_id == id && *state == expected
            )
        };

        // Active -> settled asks for Settled and drops the pin now.
        screen.update(cx, |this, cx| {
            move_task(this, cx, "s1", TaskMove::Settled);
            assert!(this.sidebar_pinned(cx).is_empty());
        });
        let emitted = take(cx);
        assert!(
            state_of(&emitted, "s1", AgentTaskState::Settled),
            "{emitted:?}"
        );

        // Settled -> active asks for Active.
        screen.update(cx, |this, cx| move_task(this, cx, "s2", TaskMove::Active));
        let emitted = take(cx);
        assert!(
            state_of(&emitted, "s2", AgentTaskState::Active),
            "{emitted:?}"
        );

        // Any rung -> archived asks for Archived.
        for id in ["s1", "s2"] {
            screen.update(cx, |this, cx| move_task(this, cx, id, TaskMove::Archived));
            let emitted = take(cx);
            assert!(
                state_of(&emitted, id, AgentTaskState::Archived),
                "{emitted:?}"
            );
        }

        // Archived -> active asks for Active.
        screen.update(cx, |this, cx| move_task(this, cx, "s3", TaskMove::Active));
        let emitted = take(cx);
        assert!(
            state_of(&emitted, "s3", AgentTaskState::Active),
            "{emitted:?}"
        );

        // The rung a task is on already asks nothing.
        screen.update(cx, |this, cx| {
            move_task(this, cx, "s1", TaskMove::Active);
            move_task(this, cx, "s2", TaskMove::Settled);
            move_task(this, cx, "s3", TaskMove::Archived);
        });
        assert!(take(cx).is_empty());

        // Any rung -> deleted asks the screen, which asks the user.
        for id in ["s1", "s2", "s3"] {
            screen.update(cx, |this, cx| move_task(this, cx, id, TaskMove::Deleted));
            let emitted = take(cx);
            assert!(
                matches!(emitted.as_slice(), [SidebarEvent::DeleteTask(got)] if got == id),
                "{emitted:?}"
            );
            screen.update(cx, |this, _cx| {
                assert_eq!(
                    this.confirm,
                    Some(dialogs::ConfirmDialog::DeleteTask(id.to_string()))
                );
                this.confirm = None;
            });
        }
    }

    /// The runtime bumps a session's update time when its state changes,
    /// so once its record lands a reopened task sits at the top of
    /// Active, above other recent activity.
    #[gpui::test]
    fn test_reopened_task_lands_on_top_of_active(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.sessions = vec![summary("s1", "Newer"), summary("s2", "Old")];
            this.sessions[0].updated_ms = 10;
            this.sessions[1].state = AgentTaskState::Archived;
            this.sync_sidebar(cx);
            assert_eq!(this.archived_indices(cx), vec![1]);

            this.sidebar
                .update(cx, |s, cx| s.move_task("s2", TaskMove::Active, cx));
            let mut session = summary("s2", "Old");
            session.updated_ms = 20;
            assert!(this.apply_service_event(
                AgentServiceEvent::SessionUpdated {
                    session_id: "s2".to_string(),
                    run_id: None,
                    session,
                },
                cx
            ));
            assert!(this.archived_indices(cx).is_empty());
            assert!(this.sidebar_settled(cx).is_empty());
            assert_eq!(this.sidebar_active(cx), vec![1, 0]);
        });
    }

    /// Escape closes the confirmation without acting; accepting runs the
    /// step the dialog stands for.
    #[gpui::test]
    fn test_confirm_dialog_cancels_and_accepts(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.sessions = vec![summary("s1", "One")];
            this.sync_sidebar(cx);
            this.request_delete_task("s1", cx);
            assert!(this.confirm.is_some());
            this.escape(cx);
            assert!(this.confirm.is_none());
            assert_eq!(this.sessions.len(), 1);

            // Accepting dispatches on the dialog kind: a root removal
            // reaches archive_root, which refuses mid-selection.
            this.root_selecting = true;
            this.request_remove_root("/tmp/proj", cx);
            this.accept_confirm(cx);
            assert!(this.confirm.is_none());
            assert!(this.notice.is_some());
        });
    }

    /// Pinned is a view of active work: settling a pinned task drops
    /// the pin so the task really moves to Settled, and it does not come
    /// back pinned when it is reopened.
    #[gpui::test]
    fn test_settling_a_pinned_task_unpins_it(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.sessions = vec![summary("s1", "One"), summary("s2", "Two")];
            this.sync_sidebar(cx);
            this.sidebar
                .update(cx, |sidebar, cx| sidebar.toggle_task_pin("s1", cx));
            assert_eq!(this.sidebar_pinned(cx), vec![0]);
            assert_eq!(
                this.sidebar.read(cx).row_step_label_for_test("s1"),
                Some("Settle")
            );

            // The pin drops at once; the row moves when the runtime's
            // record lands.
            this.sidebar.update(cx, |sidebar, cx| {
                sidebar.move_task("s1", TaskMove::Settled, cx)
            });
            assert!(this.sidebar_pinned(cx).is_empty());
            assert_eq!(this.sidebar_active(cx), vec![0, 1]);
            this.settle_task("s1", cx);
            assert_eq!(this.sidebar_settled(cx), vec![0]);
            assert_eq!(this.sidebar_active(cx), vec![1]);
            assert_eq!(
                this.sidebar.read(cx).row_step_label_for_test("s1"),
                Some("Archive")
            );

            this.unsettle_task("s1", cx);
            assert!(this.sidebar_pinned(cx).is_empty());
            assert_eq!(this.sidebar_active(cx), vec![0, 1]);
        });
    }

    /// Deleting asks first; the confirmation names the task.
    #[gpui::test]
    fn test_delete_task_asks_before_it_runs(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.sessions = vec![summary("s1", "One")];
            this.sync_sidebar(cx);
            this.request_delete_task("s1", cx);
            assert_eq!(
                this.confirm,
                Some(dialogs::ConfirmDialog::DeleteTask("s1".to_string()))
            );
            assert_eq!(this.sessions.len(), 1);
        });
    }

    /// Once the backend has deleted a task, the row leaves every section
    /// and a deleted selection moves to the newest live task in its root.
    #[gpui::test]
    fn test_remove_session_drops_the_row_and_moves_the_selection(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.sessions = vec![summary("s1", "One"), summary("s2", "Two")];
            this.completed_unread_sessions.insert("s1".to_string());
            this.sync_sidebar(cx);
            this.settle_task("s1", cx);
            assert_eq!(this.sidebar_settled(cx), vec![0]);

            this.remove_session("s1", cx);
            assert_eq!(
                this.sessions
                    .iter()
                    .map(|s| s.id.as_str())
                    .collect::<Vec<_>>(),
                vec!["s2"]
            );
            // The next task loads asynchronously; the old selection is gone.
            assert!(this.selected_session.is_none());
            assert_eq!(this.loading_session.as_deref(), Some("s2"));
            assert!(!this.completed_unread_sessions.contains("s1"));
            assert!(this.sidebar_settled(cx).is_empty());
            assert_eq!(this.sidebar_active(cx), vec![0]);

            // Removing a task the list no longer has is a no-op.
            this.remove_session("s1", cx);
            assert_eq!(this.sessions.len(), 1);
        });
    }

    /// A run on a settled task shows it active from the start: the
    /// running set covers the first frame, the runtime's record keeps it
    /// there, and finishing leaves it active with its unread marker.
    #[gpui::test]
    fn test_a_run_shows_a_settled_task_active(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.selected_session = None;
            this.sessions = vec![summary("s1", "One")];
            this.settle_task("s1", cx);
            assert_eq!(this.sidebar_settled(cx), vec![0]);
            assert!(this.sidebar_active(cx).is_empty());

            this.handle_run_event(
                "s1",
                "run-1",
                maple_agent::agent::AgentRunEvent::Started,
                cx,
            );
            assert_eq!(this.sidebar_active(cx), vec![0]);

            // The runtime's record arrives while the run works.
            this.unsettle_task("s1", cx);
            assert_eq!(this.sidebar_active(cx), vec![0]);

            this.handle_run_event(
                "s1",
                "run-1",
                maple_agent::agent::AgentRunEvent::Finished(
                    maple_agent::agent::AgentRunTerminal::Completed,
                ),
                cx,
            );
            assert_eq!(this.sidebar_active(cx), vec![0]);
            assert!(this.sidebar_settled(cx).is_empty());
            assert!(this.completed_unread_sessions.contains("s1"));
        });
    }

    /// The task menu takes the keyboard: arrows walk it and Enter runs
    /// the highlighted action. Under Application Vim, j and k walk it too.
    #[gpui::test]
    fn test_keyboard_drives_the_task_menu(cx: &mut TestAppContext) {
        let (chat, cx) = chat_window(cx, |this, cx| {
            this.sessions = vec![summary("s1", "One")];
            this.sync_sidebar(cx);
        });
        let sidebar = chat.update(cx, |this, _| this.sidebar.clone());
        sidebar.update(cx, |sidebar, cx| sidebar.open_task_menu_for_test("s1", cx));
        cx.run_until_parked();
        // "Rename task" comes first.
        cx.simulate_keystrokes("down enter");
        assert!(matches!(
            sidebar.update(cx, |sidebar, _| sidebar.rename_target()),
            Some(RenameTarget::Task(ref target)) if target == "s1"
        ));
        assert!(sidebar.update(cx, |sidebar, _| sidebar.task_menu().is_none()));

        chat.update(cx, |this, cx| {
            this.cancel_rename(cx);
            this.set_application_vim_enabled(true, cx);
        });
        sidebar.update(cx, |sidebar, cx| sidebar.open_task_menu_for_test("s1", cx));
        cx.run_until_parked();
        cx.simulate_keystrokes("j j");
        assert_eq!(
            sidebar.update(cx, |sidebar, _| sidebar.popup_highlight()),
            Some("pin-task-s1".into())
        );
        cx.simulate_keystrokes("k");
        assert_eq!(
            sidebar.update(cx, |sidebar, _| sidebar.popup_highlight()),
            Some("rename-task-s1".into())
        );
    }

    /// Choosing any task-menu item closes the menu: a ladder move and a
    /// pin toggle alike.
    #[gpui::test]
    fn test_task_menu_closes_after_an_item_runs(cx: &mut TestAppContext) {
        let (chat, cx) = chat_window(cx, |this, cx| {
            this.sessions = vec![summary("s1", "One")];
            this.sync_sidebar(cx);
        });
        let sidebar = chat.update(cx, |this, _| this.sidebar.clone());
        let pick = |cx: &mut gpui::VisualTestContext, keys: &str| {
            sidebar.update(cx, |sidebar, cx| sidebar.open_task_menu_for_test("s1", cx));
            cx.run_until_parked();
            cx.simulate_keystrokes(keys);
            assert!(sidebar.update(cx, |sidebar, _| sidebar.task_menu().is_none()));
        };
        // Rows: Rename, Pin, Settle, Archive, Delete.
        pick(cx, "down down down enter");
        // Rows: Rename, Reopen, Archive, Delete.
        chat.update(cx, |this, cx| this.settle_task("s1", cx));
        pick(cx, "down down enter");
        chat.update(cx, |this, cx| this.unsettle_task("s1", cx));
        pick(cx, "down down enter");
        assert_eq!(chat.update(cx, |this, cx| this.sidebar_pinned(cx)), vec![0]);
    }

    /// Opened from the keyboard, the switcher starts on the current scope;
    /// Application Vim's j reaches a project and Enter scopes the sidebar
    /// to it.
    #[gpui::test]
    fn test_application_vim_drives_the_switcher_menu(cx: &mut TestAppContext) {
        let (chat, cx) = chat_window(cx, |this, cx| {
            this.sessions = vec![summary_at("s1", "One", "/a")];
            this.recent_roots = vec![recent_root("/b")];
            this.sync_sidebar(cx);
            this.set_application_vim_enabled(true, cx);
        });
        let sidebar = chat.update(cx, |this, _| this.sidebar.clone());
        sidebar.update(cx, |sidebar, cx| sidebar.set_switcher_menu_open(true, cx));
        cx.run_until_parked();
        assert_eq!(
            sidebar.update(cx, |sidebar, _| sidebar.popup_highlight()),
            Some("switcher-all-projects".into())
        );
        // Rows: "All projects", "/b", "/a", "New project…".
        cx.simulate_keystrokes("j enter");
        assert_eq!(
            sidebar.update(cx, |sidebar, _| sidebar.project_filter().map(str::to_owned)),
            Some("/b".to_string())
        );
        assert!(!sidebar.update(cx, |sidebar, _| sidebar.switcher_menu_open()));
    }

    /// What menu, if any, the chat and its sidebar show.
    fn open_menu(chat: &Entity<ChatScreen>, cx: &mut gpui::VisualTestContext) -> String {
        chat.update(cx, |this, cx| {
            let sidebar = this.sidebar.read(cx);
            let mut open = Vec::new();
            if let Some(popup) = this.popup.open_key() {
                open.push(format!("{popup:?}"));
            }
            if sidebar.switcher_menu_open() {
                open.push("Switcher".to_string());
            }
            if let Some(task) = sidebar.task_menu() {
                open.push(format!("Task({task})"));
            }
            if let Some(root) = sidebar.project_menu() {
                open.push(format!("ProjectMenu({root})"));
            }
            open.join(" + ")
        })
    }

    /// Press the middle of the element with `selector`. The sidebar is a
    /// cached view, so refresh first: a reused view keeps its listeners but
    /// not its debug bounds. After a refresh the next paint has to put
    /// those bounds back; one extra frame covers a park that finished
    /// before that paint.
    fn press(cx: &mut gpui::VisualTestContext, selector: &'static str) {
        let bounds = (0..2)
            .find_map(|_| {
                cx.update(|window, _| window.refresh());
                cx.run_until_parked();
                cx.debug_bounds(selector)
            })
            .unwrap_or_else(|| panic!("{selector} is not on screen"));
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
    }

    #[gpui::test]
    fn unsaved_local_work_has_a_retry_beside_both_chat_composers(cx: &mut TestAppContext) {
        use crate::ui::store_gate::RetryStorePersistence;
        use maple_agent::store::{PersistenceFailure, PersistenceFailureReason};
        use std::cell::Cell;
        use std::rc::Rc;

        let (chat, cx) = chat_window(cx, |this, _| {
            this.persistence_status.failure = Some(PersistenceFailure {
                store_id: Some("local-store".into()),
                revision: 1,
                reason: PersistenceFailureReason::DiskFull,
            });
        });
        let retries = Rc::new(Cell::new(0));
        let observed = retries.clone();
        let _subscription = cx.update(|_, app| {
            app.subscribe(&chat, move |_, _: &RetryStorePersistence, _| {
                observed.set(observed.get() + 1);
            })
        });
        for has_history in [false, true] {
            if has_history {
                chat.update(cx, |this, cx| {
                    this.replace_timeline(vec![user_item("saved", "Earlier message")]);
                    cx.notify();
                });
            }
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
            let composer = cx.debug_bounds("composer-box").expect("composer");
            let feedback = cx
                .debug_bounds("chat-persistence-feedback")
                .expect("unsaved-work feedback");
            assert!(
                feedback.top() >= composer.bottom(),
                "save feedback stays beside the composer, outside the window header"
            );
            press(cx, "retry-local-store-persistence");
            cx.run_until_parked();
        }
        assert_eq!(
            retries.get(),
            2,
            "each click requests a desktop-owned retry"
        );
        chat.update(cx, |this, cx| {
            this.persistence_status.failure = None;
            cx.notify();
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(cx.debug_bounds("chat-persistence-feedback").is_none());
        assert!(cx.debug_bounds("retry-local-store-persistence").is_none());
    }

    /// The composer offers the model and integration chips; there is no
    /// approval-mode chip because every tool call runs without asking.
    #[gpui::test]
    fn composer_offers_no_approval_mode(cx: &mut TestAppContext) {
        let (_chat, cx) = chat_window(cx, |this, cx| {
            this.sessions = vec![summary("s1", "One")];
            this.models = vec!["voxtral-small-24b".to_string()];
            this.sync_sidebar(cx);
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(cx.debug_bounds("model-picker").is_some());
        assert!(cx.debug_bounds("mcp-menu").is_some());
        assert!(cx.debug_bounds("permission-mode-toggle").is_none());
    }

    /// Issue #997: a second press on the button that opened a menu closes
    /// it. Before, the menu's outside-press handler closed it on the way
    /// down and the button's click opened it again.
    #[gpui::test]
    fn test_a_second_press_on_each_trigger_closes_its_menu(cx: &mut TestAppContext) {
        let (chat, cx) = chat_window(cx, |this, cx| {
            this.sessions = vec![summary("s1", "One")];
            this.models = vec!["voxtral-small-24b".to_string()];
            this.sync_sidebar(cx);
        });
        for (trigger, menu) in [
            ("projects-header", "Switcher"),
            ("menu-session-s1", "Task(s1)"),
            ("model-picker", "Model"),
            ("mcp-menu", "Integrations"),
            ("root-picker", "Project"),
        ] {
            press(cx, trigger);
            assert_eq!(open_menu(&chat, cx), menu, "{trigger} opens its menu");
            press(cx, trigger);
            assert_eq!(
                open_menu(&chat, cx),
                "",
                "a second press on {trigger} closes it"
            );
        }

        // The project menu opens from inside the switcher.
        press(cx, "projects-header");
        press(cx, "menu-project-/tmp/proj");
        assert_eq!(open_menu(&chat, cx), "Switcher + ProjectMenu(/tmp/proj)");
        press(cx, "menu-project-/tmp/proj");
        assert_eq!(open_menu(&chat, cx), "Switcher");
    }

    /// One menu at a time: pressing another menu's button closes the open
    /// menu, whichever view owns either of them.
    #[gpui::test]
    fn test_opening_a_menu_closes_the_open_one(cx: &mut TestAppContext) {
        let (chat, cx) = chat_window(cx, |this, cx| {
            this.sessions = vec![summary("s1", "One")];
            this.models = vec!["voxtral-small-24b".to_string()];
            this.sync_sidebar(cx);
        });
        for (first, then, open) in [
            ("projects-header", "model-picker", "Model"),
            ("menu-session-s1", "model-picker", "Model"),
            ("model-picker", "menu-session-s1", "Task(s1)"),
            ("model-picker", "projects-header", "Switcher"),
            ("menu-session-s1", "projects-header", "Switcher"),
            ("root-picker", "mcp-menu", "Integrations"),
        ] {
            press(cx, first);
            press(cx, then);
            assert_eq!(open_menu(&chat, cx), open, "{first} then {then}");
            chat.update(cx, |this, cx| {
                this.popup.close(cx);
                this.sidebar
                    .update(cx, |sidebar, cx| sidebar.close_popups(cx));
            });
        }

        // A menu opened without the pointer closes the open one too: it
        // takes the focus.
        press(cx, "menu-session-s1");
        cx.simulate_keystrokes("secondary-p");
        assert_eq!(open_menu(&chat, cx), "Project");
    }

    /// A task menu reaches over the rows below it. A press on one of its
    /// items lands on the item, not on the overflow button of the row
    /// underneath.
    #[gpui::test]
    fn test_a_press_on_a_menu_item_reaches_the_item(cx: &mut TestAppContext) {
        let (chat, cx) = chat_window(cx, |this, cx| {
            this.sessions = vec![
                summary("s1", "One"),
                summary("s2", "Two"),
                summary("s3", "Three"),
            ];
            this.sync_sidebar(cx);
        });
        press(cx, "menu-session-s1");
        let beneath = cx.debug_bounds("menu-session-s2").expect("s2's button");
        let rename = cx.debug_bounds("rename-task-s1").expect("s1's Rename item");
        assert!(
            rename.contains(&beneath.center()),
            "the fixture puts s2's button under s1's Rename item"
        );
        cx.simulate_click(beneath.center(), gpui::Modifiers::default());
        assert_eq!(open_menu(&chat, cx), "", "s2's menu did not open");
        let sidebar = chat.update(cx, |this, _| this.sidebar.clone());
        assert!(
            matches!(
                sidebar.update(cx, |sidebar, _| sidebar.rename_target()),
                Some(RenameTarget::Task(ref target)) if target == "s1"
            ),
            "the press ran s1's Rename"
        );
    }

    /// The header's project menu closes on a press outside it, like every
    /// other menu. It used to stay open.
    #[gpui::test]
    fn test_the_project_menu_closes_on_an_outside_press(cx: &mut TestAppContext) {
        let (chat, cx) = chat_window(cx, |this, cx| {
            this.recent_roots = vec![recent_root(&absolute_fixture_root("one"))];
            this.sync_sidebar(cx);
        });
        press(cx, "root-picker");
        assert_eq!(open_menu(&chat, cx), "Project");
        cx.simulate_click(gpui::point(px(900.), px(500.)), gpui::Modifiers::default());
        assert_eq!(open_menu(&chat, cx), "");
    }

    /// Folder browsing is a separate modal; typing and Enter stay out of
    /// the composer and never select a project until Use folder succeeds.
    #[gpui::test]
    fn test_the_project_path_field_keeps_its_typing(cx: &mut TestAppContext) {
        let (chat, cx) = chat_window(cx, |_, _| {});
        chat.update(cx, |this, cx| this.choose_root_dialog(cx));
        cx.run_until_parked();
        let dialog = chat.update(cx, |this, _| this.project_folder_dialog.clone().unwrap());
        let input = dialog.update(cx, |this, _| this.path_input.clone());
        let handle = cx.update(|_, app| input.read(app).focus_handle(app));
        assert_eq!(cx.update(|window, app| window.focused(app)), Some(handle));
        input.update(cx, |input, cx| input.clear(cx));
        cx.simulate_input("relative");
        assert_eq!(input.update(cx, |input, _| input.text()), "relative");
        let composer = chat.update(cx, |this, _| this.composer.clone().expect("composer"));
        assert_eq!(composer.update(cx, |composer, _| composer.text()), "");
        assert_eq!(open_menu(&chat, cx), "");
        cx.simulate_keystrokes("enter");
        chat.update(cx, |this, _| {
            assert_eq!(this.selected_session.as_deref(), Some("s1"));
            assert!(this.project_folder_dialog.is_some());
        });
    }

    #[gpui::test]
    fn test_escape_in_the_path_field_closes_the_folder_dialog(cx: &mut TestAppContext) {
        let (chat, cx) = chat_window(cx, |_, _| {});
        let composer = chat.update(cx, |this, cx| {
            this.composer.clone().unwrap().read(cx).focus_handle(cx)
        });
        cx.update(|window, app| window.focus(&composer, app));
        chat.update(cx, |this, cx| this.choose_root_dialog(cx));
        cx.run_until_parked();
        cx.simulate_keystrokes("escape");
        assert!(chat.update(cx, |this, _| this.project_folder_dialog.is_none()));
        assert_eq!(cx.update(|window, app| window.focused(app)), Some(composer));
    }

    #[gpui::test]
    fn test_application_vim_leaves_folder_path_typing_alone(cx: &mut TestAppContext) {
        let (chat, cx) = chat_window(cx, |this, cx| this.set_application_vim_enabled(true, cx));
        chat.update(cx, |this, cx| this.choose_root_dialog(cx));
        cx.run_until_parked();
        let dialog = chat.update(cx, |this, _| this.project_folder_dialog.clone().unwrap());
        let input = dialog.update(cx, |this, _| this.path_input.clone());
        input.update(cx, |input, cx| input.clear(cx));
        cx.simulate_keystrokes("l o g g i n g");
        assert_eq!(input.update(cx, |input, _| input.text()), "logging");
    }

    #[gpui::test]
    fn folder_dialog_blocks_chat_navigation_shortcuts(cx: &mut TestAppContext) {
        let (chat, cx) = chat_window(cx, |_, _| {});
        chat.update(cx, |this, cx| this.choose_root_dialog(cx));
        cx.run_until_parked();
        let shortcut = if cfg!(target_os = "macos") {
            "cmd-n cmd-p cmd-k"
        } else {
            "ctrl-n ctrl-p ctrl-k"
        };
        cx.simulate_keystrokes(shortcut);
        chat.update(cx, |this, _| {
            assert_eq!(this.selected_session.as_deref(), Some("s1"));
            assert!(this.project_folder_dialog.is_some());
            assert!(this.popup.open_key().is_none());
        });
    }

    /// The native accessibility builder rejects two focused nodes in one
    /// frame. A cached modal must acquire focus before the background starts
    /// prepainting, not later when its cached subtree first renders.
    #[gpui::test]
    fn folder_dialog_takes_focus_before_background_prepaint(cx: &mut TestAppContext) {
        use std::{cell::RefCell, rc::Rc};

        struct FocusProbeHost {
            chat: Entity<ChatScreen>,
            observed: Rc<RefCell<Vec<Option<gpui::FocusHandle>>>>,
        }
        impl Render for FocusProbeHost {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let observed = self.observed.clone();
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .child(
                        gpui::canvas(
                            move |_, window, cx| {
                                observed.borrow_mut().push(window.focused(cx));
                            },
                            |_, _, _, _| {},
                        )
                        .h(px(1.))
                        .w_full(),
                    )
                    .child(self.chat.clone())
            }
        }

        cx.executor().allow_parking();
        let chat = cx.new(|cx| {
            let _guard = SETTINGS_LOCK.lock();
            crate::desktop::register_key_bindings(cx);
            let backend = Arc::new(
                AgentBackend::for_test("http://127.0.0.1:9".into(), String::new()).unwrap(),
            );
            let mut chat = ChatScreen::new_without_start(backend, "user".into(), cx);
            chat.booting = false;
            chat.trust_prompts = false;
            chat.draft = true;
            chat.fill_empty_composer("Keep this draft", cx);
            chat
        });
        let composer = chat.update(cx, |this, cx| {
            this.composer.as_ref().unwrap().read(cx).focus_handle(cx)
        });
        let observed = Rc::new(RefCell::new(Vec::new()));
        let probe = observed.clone();
        let (_host, cx) = cx.add_window_view(|window, cx| {
            window.focus(&composer, cx);
            FocusProbeHost {
                chat: chat.clone(),
                observed: probe,
            }
        });
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        for _ in 0..2 {
            observed.borrow_mut().clear();
            let sidebar = chat.update(cx, |this, _| this.sidebar.clone());
            sidebar.update(cx, |_, cx| cx.emit(SidebarEvent::ChooseProject));
            cx.run_until_parked();
            let path = chat.update(cx, |this, cx| {
                this.project_folder_dialog
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .path_input
                    .read(cx)
                    .focus_handle(cx)
            });
            assert!(!observed.borrow().is_empty());
            assert!(
                observed
                    .borrow()
                    .iter()
                    .all(|focus| focus.as_ref() == Some(&path)),
                "the background must see the modal focus from its first prepaint"
            );
            cx.simulate_keystrokes("escape");
            assert_eq!(
                cx.update(|window, cx| window.focused(cx)),
                Some(composer.clone())
            );
            chat.update(cx, |this, cx| {
                assert!(this.project_folder_dialog.is_none());
                assert!(this.draft);
                assert_eq!(this.composer_text(cx).as_deref(), Some("Keep this draft"));
            });
        }
    }

    #[gpui::test]
    fn test_the_folder_path_context_menu_takes_its_presses(cx: &mut TestAppContext) {
        let (chat, cx) = chat_window(cx, |_, _| {});
        chat.update(cx, |this, cx| this.choose_root_dialog(cx));
        cx.run_until_parked();
        let dialog = chat.update(cx, |this, _| this.project_folder_dialog.clone().unwrap());
        let input = dialog.update(cx, |this, _| this.path_input.clone());
        input.update(cx, |input, cx| input.clear(cx));
        cx.simulate_input("/tmp/somewhere");
        let field = cx.debug_bounds("project-folder-path").expect("path field");
        for item in ["text-input-select-all", "text-input-cut"] {
            cx.simulate_mouse_down(
                field.center(),
                gpui::MouseButton::Right,
                gpui::Modifiers::default(),
            );
            cx.simulate_mouse_up(
                field.center(),
                gpui::MouseButton::Right,
                gpui::Modifiers::default(),
            );
            let row = cx.debug_bounds(item).expect("the field's menu row");
            cx.simulate_click(row.center(), gpui::Modifiers::default());
            assert!(chat.update(cx, |this, _| this.project_folder_dialog.is_some()));
        }
        assert_eq!(input.update(cx, |input, _| input.text()), "");
    }

    fn identical_path_project_catalog() -> Arc<Vec<computers::Computer>> {
        Arc::new(
            [
                (None, "This computer", "Local project"),
                (Some("east"), "East computer", "East project"),
                (Some("west"), "West computer", "West project"),
            ]
            .into_iter()
            .map(|(computer, label, name)| computers::Computer {
                key: computer.map(str::to_string),
                label: label.into(),
                backend: None,
                host_epoch: 0,
                project_names: HashMap::new(),
                projects: vec![computers::Project {
                    path: "/shared".into(),
                    label: name.into(),
                }],
                sessions: vec![summary_at("same-id", "Existing task", "/shared")],
                running: HashSet::new(),
                unread: HashSet::new(),
                pinned: Vec::new(),
                loading: false,
                error: None,
            })
            .collect(),
        )
    }

    #[gpui::test]
    fn inactive_project_rename_controls_are_scoped_and_do_not_navigate(cx: &mut TestAppContext) {
        use std::cell::RefCell;
        use std::rc::Rc;
        let (chat, cx) = chat_window(cx, |this, cx| {
            this.selected_session = None;
            this.draft = true;
            this.project_root = Some("/shared".into());
            this.sessions = vec![summary_at("same-id", "Existing task", "/shared")];
            this.apply_recent_roots(vec![named_root("/shared", "Local project", 1)], cx);
            this.set_computers(identical_path_project_catalog(), cx);
            this.composer
                .clone()
                .unwrap()
                .update(cx, |input, cx| input.set_text("Keep my draft", cx));
        });
        let sidebar = chat.update(cx, |this, _| this.sidebar.clone());
        let generation = chat.update(cx, |this, _| this.selection_generation);
        let events = Rc::new(RefCell::new(Vec::new()));
        let observed = events.clone();
        let _subscription = cx.update(|_, app| {
            app.subscribe(&sidebar, move |_, event: &SidebarEvent, _| {
                observed.borrow_mut().push(event.clone());
            })
        });
        sidebar.update(cx, |sidebar, cx| {
            for computer in [Some("east"), Some("west")] {
                let target = ProjectTarget {
                    computer: computer.map(str::to_string),
                    path: "/shared".into(),
                };
                assert_eq!(
                    sidebar.project_menu_labels_for_test(&target),
                    vec!["Rename project"]
                );
            }
            let local = ProjectTarget {
                computer: None,
                path: "/shared".into(),
            };
            let own_menu = sidebar.project_menu_labels_for_test(&local);
            assert!(own_menu.contains(&"Rename project"));
            assert!(own_menu.contains(&"Open folder"));
            assert!(own_menu.contains(&"Remove project"));
            sidebar.set_switcher_menu_open(true, cx);
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let local = cx
            .debug_bounds("switcher-/shared")
            .expect("local project row");
        let east = cx
            .debug_bounds("switcher-computer-east-/shared")
            .expect("east project row");
        let west = cx
            .debug_bounds("switcher-computer-west-/shared")
            .expect("west project row");
        assert_ne!(local.center(), east.center());
        assert_ne!(east.center(), west.center());
        for selector in [
            "menu-project-/shared",
            "menu-project-computer-east-/shared",
            "menu-project-computer-west-/shared",
        ] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "distinct overflow control: {selector}"
            );
        }
        press(cx, "menu-project-computer-east-/shared");
        assert!(
            cx.debug_bounds("rename-project-computer-east-/shared")
                .is_some()
        );
        assert!(
            cx.debug_bounds("open-project-computer-east-/shared")
                .is_none()
        );
        assert!(cx.debug_bounds("remove-project-/shared").is_none());
        press(cx, "rename-project-computer-east-/shared");
        assert_eq!(
            sidebar.update(cx, |sidebar, _| sidebar.rename_target()),
            Some(RenameTarget::Project(ProjectTarget {
                computer: Some("east".into()),
                path: "/shared".into(),
            }))
        );
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_input("East renamed inline");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        let events = events.borrow();
        let renames = events
            .iter()
            .filter_map(|event| match event {
                SidebarEvent::RenameProject(request) => Some(request),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(renames.len(), 1);
        assert_eq!(
            renames[0].target,
            ProjectTarget {
                computer: Some("east".into()),
                path: "/shared".into()
            }
        );
        assert_eq!(renames[0].name, "East renamed inline");
        assert!(!events.iter().any(|event| matches!(
            event,
            SidebarEvent::Navigate(_)
                | SidebarEvent::Select(_)
                | SidebarEvent::RemoveRoot(_)
                | SidebarEvent::SetTrust { .. }
                | SidebarEvent::ChooseProject
        )));
        chat.update(cx, |this, cx| {
            assert!(this.draft);
            assert!(this.selected_session.is_none());
            assert_eq!(this.project_root.as_deref(), Some("/shared"));
            assert_eq!(this.selection_generation, generation);
            assert_eq!(this.composer_text(cx).as_deref(), Some("Keep my draft"));
            assert!(this.timeline.is_empty());
            assert_eq!(this.sidebar.read(cx).root_name("/shared"), "Local project");
            assert_eq!(
                this.sidebar.read(cx).project_name_for_test(&ProjectTarget {
                    computer: Some("east".into()),
                    path: "/shared".into()
                }),
                "East renamed inline"
            );
            assert_eq!(
                this.sidebar.read(cx).project_name_for_test(&ProjectTarget {
                    computer: Some("west".into()),
                    path: "/shared".into()
                }),
                "West project"
            );
        });
    }

    #[gpui::test]
    fn inactive_project_rename_previews_and_failures_are_isolated_by_computer(
        cx: &mut TestAppContext,
    ) {
        use std::cell::RefCell;
        use std::rc::Rc;
        let (chat, cx) = chat_window(cx, |this, cx| {
            this.selected_session = Some("same-id".into());
            this.project_root = Some("/shared".into());
            this.sessions = vec![summary_at("same-id", "Existing task", "/shared")];
            this.apply_recent_roots(vec![named_root("/shared", "Local project", 1)], cx);
            this.replace_timeline(vec![user_item("existing-message", "Keep the conversation")]);
            this.set_computers(identical_path_project_catalog(), cx);
            this.composer
                .clone()
                .unwrap()
                .update(cx, |input, cx| input.set_text("Keep my reply", cx));
        });
        let sidebar = chat.update(cx, |this, _| this.sidebar.clone());
        let generation = chat.update(cx, |this, _| this.selection_generation);
        let east = ProjectTarget {
            computer: Some("east".into()),
            path: "/shared".into(),
        };
        let west = ProjectTarget {
            computer: Some("west".into()),
            path: "/shared".into(),
        };
        let events = Rc::new(RefCell::new(Vec::new()));
        let observed = events.clone();
        let _subscription = cx.update(|_, app| {
            app.subscribe(&sidebar, move |_, event: &SidebarEvent, _| {
                observed.borrow_mut().push(event.clone());
            })
        });
        sidebar.update(cx, |sidebar, cx| {
            for (target, name) in [(&east, "East preview"), (&west, "West preview")] {
                sidebar.begin_rename(RenameTarget::Project(target.clone()), cx);
                sidebar
                    .inputs()
                    .last()
                    .unwrap()
                    .clone()
                    .update(cx, |input, cx| input.set_text(name, cx));
                sidebar.commit_rename(cx);
            }
            assert_eq!(sidebar.project_name_for_test(&east), "East preview");
            assert_eq!(sidebar.project_name_for_test(&west), "West preview");
            assert_eq!(sidebar.root_name("/shared"), "Local project");
            sidebar.begin_rename(RenameTarget::Project(east.clone()), cx);
            assert!(
                sidebar.rename_target().is_none(),
                "a pending target cannot be renamed twice"
            );
        });
        chat.update(cx, |this, cx| {
            let mut catalog = this.computers.as_ref().clone();
            catalog[2].projects[0].label = "West confirmed by owner".into();
            this.set_computers(Arc::new(catalog), cx);
            assert_eq!(
                this.sidebar.read(cx).project_name_for_test(&west),
                "West preview"
            );
            this.finish_project_rename(east.clone(), Err("East is unavailable".into()), cx);
            assert_eq!(
                this.sidebar.read(cx).project_name_for_test(&east),
                "East project"
            );
            assert_eq!(
                this.sidebar.read(cx).project_name_for_test(&west),
                "West preview",
                "failure only removes its own overlay"
            );
            this.finish_project_rename(west.clone(), Ok(()), cx);
            assert_eq!(
                this.sidebar.read(cx).project_name_for_test(&west),
                "West confirmed by owner"
            );
            assert_eq!(this.sidebar.read(cx).root_name("/shared"), "Local project");
            assert_eq!(this.selected_session.as_deref(), Some("same-id"));
            assert_eq!(this.project_root.as_deref(), Some("/shared"));
            assert_eq!(this.selection_generation, generation);
            assert_eq!(this.composer_text(cx).as_deref(), Some("Keep my reply"));
            assert_eq!(this.timeline[0].id, "existing-message");
        });
        cx.run_until_parked();
        let events = events.borrow();
        let renames = events
            .iter()
            .filter_map(|event| match event {
                SidebarEvent::RenameProject(request) => Some(request),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(renames.len(), 2);
        assert_eq!(renames[0].target, east);
        assert_eq!(renames[1].target, west);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, SidebarEvent::Navigate(_) | SidebarEvent::Select(_)))
        );
        assert!(events.iter().any(|event| matches!(event, SidebarEvent::Notice(message) if message.as_ref() == "East is unavailable")));
    }

    /// A task's rename ends when its field loses focus or the pointer
    /// presses anywhere outside it, and keeps what was typed, as Enter
    /// does: a click anywhere else never leaves the row stuck in its
    /// field. Escape still discards it.
    #[gpui::test]
    fn test_a_task_rename_commits_when_the_user_leaves_it(cx: &mut TestAppContext) {
        let (chat, cx) = chat_window(cx, |this, cx| {
            this.sessions = vec![summary("s1", "One"), summary("s2", "Two")];
            this.sync_sidebar(cx);
        });
        let sidebar = chat.update(cx, |this, _| this.sidebar.clone());
        let sent = |cx: &mut gpui::VisualTestContext| {
            sidebar.update(cx, |sidebar, _| sidebar.renames_sent().to_vec())
        };
        let rename = |cx: &mut gpui::VisualTestContext, typed: &str| {
            sidebar.update(cx, |sidebar, cx| {
                sidebar.begin_rename(RenameTarget::Task("s1".into()), cx)
            });
            cx.run_until_parked();
            cx.simulate_input(typed);
        };
        let ended = |cx: &mut gpui::VisualTestContext| {
            sidebar.update(cx, |sidebar, _| sidebar.rename_target().is_none())
        };

        // Escape, then focus moving on: nothing is sent.
        rename(cx, "Discarded");
        cx.simulate_keystrokes("escape");
        cx.update(|window, cx| sidebar.update(cx, |sidebar, cx| sidebar.focus_search(window, cx)));
        cx.run_until_parked();
        assert!(ended(cx));
        assert!(sent(cx).is_empty(), "Escape discards the rename");

        // Focus moving away commits what was typed, once.
        rename(cx, "Moved");
        cx.update(|window, cx| sidebar.update(cx, |sidebar, cx| sidebar.focus_search(window, cx)));
        cx.run_until_parked();
        assert!(ended(cx), "focus leaving ends the rename");
        assert_eq!(sent(cx), vec![("s1".to_string(), "Moved".to_string())]);

        // A press on another row, which takes no focus, commits too.
        rename(cx, "Pressed");
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let other_row = cx.debug_bounds("menu-session-s2").expect("s2's row");
        let on_row = other_row.center() - gpui::point(gpui::px(80.), gpui::px(0.));
        cx.simulate_mouse_down(on_row, gpui::MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_up(on_row, gpui::MouseButton::Left, gpui::Modifiers::default());
        cx.run_until_parked();
        assert!(ended(cx), "a press outside the field ends the rename");
        assert_eq!(sent(cx).len(), 2, "one request per rename");
        assert_eq!(sent(cx)[1], ("s1".to_string(), "Pressed".to_string()));
    }

    /// Under Application Vim, the Escape that leaves the field discards the
    /// rename before focus moves on, so the move commits nothing.
    #[gpui::test]
    fn test_application_vim_escape_discards_a_task_rename(cx: &mut TestAppContext) {
        let (chat, cx) = chat_window(cx, |this, cx| {
            this.sessions = vec![summary("s1", "One")];
            this.sync_sidebar(cx);
            this.set_application_vim_enabled(true, cx);
        });
        let sidebar = chat.update(cx, |this, _| this.sidebar.clone());
        sidebar.update(cx, |sidebar, cx| {
            sidebar.begin_rename(RenameTarget::Task("s1".into()), cx)
        });
        cx.run_until_parked();
        cx.simulate_input("Changed");
        // The first Escape leaves Insert; the next leaves the field.
        cx.simulate_keystrokes("escape escape");
        cx.run_until_parked();
        assert!(sidebar.update(cx, |sidebar, _| sidebar.rename_target().is_none()));
        assert!(
            sidebar.update(cx, |sidebar, _| sidebar.renames_sent().is_empty()),
            "Escape discards the rename"
        );
    }

    /// The field's own right-click menu (Paste, Cut) keeps the rename open;
    /// it ends only once the user really leaves.
    #[gpui::test]
    fn test_a_task_rename_survives_its_fields_menu(cx: &mut TestAppContext) {
        let (chat, cx) = chat_window(cx, |this, cx| {
            this.sessions = vec![summary("s1", "One")];
            this.sync_sidebar(cx);
        });
        let sidebar = chat.update(cx, |this, _| this.sidebar.clone());
        sidebar.update(cx, |sidebar, cx| {
            sidebar.begin_rename(RenameTarget::Task("s1".into()), cx)
        });
        cx.run_until_parked();
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let field = cx.debug_bounds("rename-field").expect("the rename field");
        cx.simulate_mouse_down(
            field.center(),
            gpui::MouseButton::Right,
            gpui::Modifiers::default(),
        );
        cx.simulate_mouse_up(
            field.center(),
            gpui::MouseButton::Right,
            gpui::Modifiers::default(),
        );
        cx.run_until_parked();
        let input = sidebar.update(cx, |sidebar, _| sidebar.inputs().last().cloned());
        assert!(
            input.is_some_and(|input| input.read_with(cx, |input, _| input.menu_open())),
            "the field's menu opened"
        );
        assert!(
            sidebar.update(cx, |sidebar, _| sidebar.rename_target().is_some()),
            "the field's menu keeps the rename open"
        );
        assert!(sidebar.update(cx, |sidebar, _| sidebar.renames_sent().is_empty()));
    }

    /// A task's rename opens with the old name selected, so typing replaces
    /// it, and the row drops its hover actions while it is a text field.
    #[gpui::test]
    fn test_a_task_rename_opens_selected_without_hover_actions(cx: &mut TestAppContext) {
        let (chat, cx) = chat_window(cx, |this, cx| {
            this.sessions = vec![summary("s1", "One")];
            this.sync_sidebar(cx);
        });
        let sidebar = chat.update(cx, |this, _| this.sidebar.clone());
        let rename_button = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
            cx.debug_bounds("rename-session-s1")
        };
        assert!(
            rename_button(cx).is_some(),
            "a listed row has its hover actions"
        );

        sidebar.update(cx, |sidebar, cx| {
            sidebar.begin_rename(RenameTarget::Task("s1".into()), cx)
        });
        cx.run_until_parked();
        assert!(rename_button(cx).is_none(), "a row being renamed has none");
        cx.simulate_input("Fresh");
        let field = sidebar.update(cx, |sidebar, _| sidebar.inputs().last().cloned());
        assert_eq!(
            field.map(|field| field.read_with(cx, |field, _| field.text())),
            Some("Fresh".to_string()),
            "typing replaces the selected old name"
        );

        chat.update(cx, |this, cx| this.cancel_rename(cx));
        assert!(rename_button(cx).is_some(), "the actions come back after");
    }

    /// A project's rename field sits in the switcher. Escape ends the
    /// rename and Enter commits it; either way the switcher stays open and
    /// has the keyboard again.
    #[gpui::test]
    fn test_a_project_rename_ends_inside_the_switcher(cx: &mut TestAppContext) {
        let backend = backend_with_mcp_servers(cx, "rename-keyboard", Vec::new());
        let root = mcp_project("rename-keyboard");
        let roots = register_test_project(&backend, &root);
        let (chat, cx) = chat_window_in(cx, backend.clone(), |this, cx| {
            this.project_root = Some(root.clone());
            this.sessions = vec![summary_at("s1", "One", &root)];
            this.apply_recent_roots(roots, cx);
        });
        let sidebar = chat.update(cx, |this, _| this.sidebar.clone());
        for commit in [false, true] {
            sidebar.update(cx, |sidebar, cx| {
                sidebar.set_switcher_menu_open(true, cx);
                sidebar.begin_rename(
                    RenameTarget::Project(ProjectTarget {
                        computer: None,
                        path: root.clone(),
                    }),
                    cx,
                );
            });
            cx.run_until_parked();
            if commit {
                cx.simulate_keystrokes("secondary-a");
                cx.simulate_input("Nice");
                cx.simulate_keystrokes("enter");
            } else {
                cx.simulate_keystrokes("escape");
            }
            assert!(sidebar.update(cx, |sidebar, _| sidebar.rename_target().is_none()));
            assert_eq!(open_menu(&chat, cx), "Switcher", "commit: {commit}");
            cx.simulate_keystrokes("down");
            assert_eq!(
                sidebar.update(cx, |sidebar, _| sidebar.popup_highlight()),
                Some(format!("switcher-{root}").into()),
                "the switcher has the keyboard, commit: {commit}"
            );
            cx.simulate_keystrokes("escape");
            assert_eq!(open_menu(&chat, cx), "");
        }
        wait_until(&chat, cx, "project name committed", |this| {
            this.recent_roots.iter().any(|project| {
                project.path == root && project.name == "Nice" && project.name_revision > 0
            })
        });
        assert_eq!(
            sidebar.update(cx, |sidebar, _| sidebar.root_name(&root)),
            "Nice"
        );
        let owner = backend.clone();
        let roots = await_backend_result(&backend, async move {
            owner
                .list_projects(
                    "user",
                    crate::backend::ProjectPageRequest {
                        cursor: None,
                        limit: 50,
                    },
                )
                .await
                .map(|page| {
                    page.projects
                        .into_iter()
                        .map(sidebar::project_from_summary)
                        .collect::<Vec<_>>()
                })
        });
        assert_eq!(
            roots
                .iter()
                .find(|project| project.path == root)
                .unwrap()
                .name,
            "Nice"
        );
    }

    #[gpui::test]
    fn project_rename_preview_survives_refresh_until_the_owner_commits(cx: &mut TestAppContext) {
        let backend = backend_with_mcp_servers(cx, "rename-preview", Vec::new());
        let root = mcp_project("rename-preview");
        let roots = register_test_project(&backend, &root);
        let (chat, cx) = chat_window_in(cx, backend.clone(), |this, cx| {
            this.project_root = Some(root.clone());
            this.sessions = vec![summary_at("s1", "One", &root)];
            this.apply_recent_roots(roots.clone(), cx);
        });
        chat.update(cx, |this, cx| {
            this.sidebar.update(cx, |sidebar, cx| {
                sidebar.begin_rename(
                    RenameTarget::Project(ProjectTarget {
                        computer: None,
                        path: root.clone(),
                    }),
                    cx,
                );
                let input = sidebar.inputs().last().unwrap().clone();
                input.update(cx, |input, cx| input.set_text("Optimistic owner name", cx));
                sidebar.commit_rename(cx);
                assert!(sidebar.project_rename_pending(&root));
                assert_eq!(sidebar.root_name(&root), "Optimistic owner name");
                sidebar.begin_rename(
                    RenameTarget::Project(ProjectTarget {
                        computer: None,
                        path: root.clone(),
                    }),
                    cx,
                );
                assert!(
                    sidebar.rename_target().is_none(),
                    "cannot start a second rename while pending"
                );
            });
            // A refresh or unrelated task update must not flash the old name
            // while its owner is still acknowledging the rename.
            this.sync_sidebar(cx);
            this.apply_service_event(
                AgentServiceEvent::ProjectsChanged {
                    user_id: "user".into(),
                    roots: roots.clone(),
                },
                cx,
            );
            assert_eq!(
                this.sidebar.read(cx).root_name(&root),
                "Optimistic owner name"
            );
            this.refresh_project_caption(cx);
            assert_eq!(this.project_label.as_ref(), "Optimistic owner name");
            assert_eq!(
                this.computer_snapshot("This computer".into(), cx)
                    .projects
                    .iter()
                    .find(|project| project.path == root)
                    .unwrap()
                    .label
                    .as_ref(),
                "Optimistic owner name"
            );
        });
        wait_until(&chat, cx, "optimistic name confirmed by owner", |this| {
            this.recent_roots.iter().any(|project| {
                project.path == root
                    && project.name == "Optimistic owner name"
                    && project.name_revision > 0
            })
        });
        chat.update(cx, |this, cx| {
            assert!(!this.sidebar.read(cx).project_rename_pending(&root));
            assert_eq!(
                this.sidebar.read(cx).root_name(&root),
                "Optimistic owner name"
            );
            assert_eq!(this.project_label.as_ref(), "Optimistic owner name");
        });
        let owner = backend.clone();
        let persisted = await_backend_result(&backend, async move {
            owner
                .list_projects(
                    "user",
                    crate::backend::ProjectPageRequest {
                        cursor: None,
                        limit: 50,
                    },
                )
                .await
                .map(|page| {
                    page.projects
                        .into_iter()
                        .map(sidebar::project_from_summary)
                        .collect::<Vec<_>>()
                })
        });
        assert_eq!(
            persisted
                .iter()
                .find(|project| project.path == root)
                .unwrap()
                .name,
            "Optimistic owner name"
        );
    }

    #[gpui::test]
    fn failed_project_rename_reverts_to_the_latest_pushed_owner_name(cx: &mut TestAppContext) {
        let backend = backend_with_mcp_servers(cx, "rename-rejected", Vec::new());
        let root = mcp_project("rename-rejected");
        let roots = register_test_project(&backend, &root);
        let (chat, cx) = chat_window_in(cx, backend.clone(), |this, cx| {
            this.project_root = Some(root.clone());
            this.sessions = vec![summary_at("s1", "One", &root)];
            this.apply_recent_roots(roots, cx);
        });
        chat.update(cx, |this, cx| {
            let invalid_name = "x".repeat(129);
            this.sidebar.update(cx, |sidebar, cx| {
                sidebar.begin_rename(
                    RenameTarget::Project(ProjectTarget {
                        computer: None,
                        path: root.clone(),
                    }),
                    cx,
                );
                let input = sidebar.inputs().last().unwrap().clone();
                input.update(cx, |input, cx| input.set_text(&invalid_name, cx));
                sidebar.commit_rename(cx);
                assert!(sidebar.project_rename_pending(&root));
                assert_eq!(sidebar.root_name(&root), invalid_name);
            });
            // Let a different controller's successful rename reach the owner,
            // then deliver its push before pumping this UI's failed response.
            let owner = backend.clone();
            let path = root.clone();
            let current = await_backend_result(&backend, async move {
                owner
                    .rename_project_root("user", path, "Concurrent owner name".into())
                    .await
            });
            assert!(this.apply_service_event(
                AgentServiceEvent::ProjectsChanged {
                    user_id: "user".into(),
                    roots: vec![current],
                },
                cx
            ));
            this.sync_sidebar(cx);
            assert_eq!(
                this.sidebar.read(cx).root_name(&root),
                invalid_name,
                "pending preview survives the push"
            );
        });
        wait_until(&chat, cx, "invalid rename rejected", |this| {
            this.notice
                .as_deref()
                .is_some_and(|notice| notice.contains("128"))
        });
        chat.update(cx, |this, cx| {
            assert!(!this.sidebar.read(cx).project_rename_pending(&root));
            assert_eq!(
                this.sidebar.read(cx).root_name(&root),
                "Concurrent owner name"
            );
            assert_eq!(this.project_label.as_ref(), "Concurrent owner name");
            assert_eq!(
                this.sidebar.read(cx).rows()[0].project_name.as_ref(),
                "Concurrent owner name"
            );
        });
        let owner = backend.clone();
        let persisted = await_backend_result(&backend, async move {
            owner
                .list_projects(
                    "user",
                    crate::backend::ProjectPageRequest {
                        cursor: None,
                        limit: 50,
                    },
                )
                .await
                .map(|page| {
                    page.projects
                        .into_iter()
                        .map(sidebar::project_from_summary)
                        .collect::<Vec<_>>()
                })
        });
        assert_eq!(
            persisted
                .iter()
                .find(|project| project.path == root)
                .unwrap()
                .name,
            "Concurrent owner name"
        );
    }

    /// Hiding the sidebar while one of its menus has the keyboard hands the
    /// keyboard back, though the hidden sidebar no longer renders.
    #[gpui::test]
    fn test_hiding_the_sidebar_hands_its_menus_focus_back(cx: &mut TestAppContext) {
        let (chat, cx) = chat_window(cx, |this, cx| {
            this.sessions = vec![summary("s1", "One")];
            this.sync_sidebar(cx);
        });
        let composer = chat.update(cx, |this, cx| {
            this.composer
                .clone()
                .expect("composer")
                .read(cx)
                .focus_handle(cx)
        });
        cx.update(|window, app| window.focus(&composer, app));
        press(cx, "menu-session-s1");
        assert_eq!(open_menu(&chat, cx), "Task(s1)");
        cx.simulate_keystrokes("secondary-b");
        assert_eq!(open_menu(&chat, cx), "");
        assert_eq!(cx.update(|window, app| window.focused(app)), Some(composer));
    }

    /// Escape closes whichever sidebar popup is open.
    #[gpui::test]
    fn test_escape_closes_the_sidebar_popups(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.sidebar
                .update(cx, |sidebar, cx| sidebar.set_switcher_menu_open(true, cx));
            this.escape(cx);
            assert!(!this.sidebar.read(cx).switcher_menu_open());
            this.sidebar
                .update(cx, |sidebar, cx| sidebar.open_task_menu_for_test("s1", cx));
            this.escape(cx);
            assert!(this.sidebar.read(cx).task_menu().is_none());
        });
    }

    #[gpui::test]
    fn test_late_send_ack_does_not_revive_a_finished_run(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.handle_run_event(
                "s1",
                "run-1",
                maple_agent::agent::AgentRunEvent::Started,
                cx,
            );
            this.handle_run_event(
                "s1",
                "run-1",
                maple_agent::agent::AgentRunEvent::Finished(
                    maple_agent::agent::AgentRunTerminal::Completed,
                ),
                cx,
            );
            assert!(this.active_runs.is_empty());
            assert!(this.finished_runs.contains(&"run-1".to_string()));
            // Bounded: old ids fall off the front.
            for ix in 0..FINISHED_RUNS_KEPT + 5 {
                this.handle_run_event(
                    "s1",
                    &format!("run-x{ix}"),
                    maple_agent::agent::AgentRunEvent::Finished(
                        maple_agent::agent::AgentRunTerminal::Completed,
                    ),
                    cx,
                );
            }
            assert_eq!(this.finished_runs.len(), FINISHED_RUNS_KEPT);
            assert!(!this.finished_runs.contains(&"run-1".to_string()));
        });
    }

    #[gpui::test]
    fn test_another_task_finishing_keeps_the_waiting_dots(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.awaiting_first_token = true;
            this.handle_run_event(
                "s2",
                "run-2",
                maple_agent::agent::AgentRunEvent::Finished(
                    maple_agent::agent::AgentRunTerminal::Completed,
                ),
                cx,
            );
            assert!(this.awaiting_first_token);
            this.handle_run_event(
                "s1",
                "run-1",
                maple_agent::agent::AgentRunEvent::Finished(
                    maple_agent::agent::AgentRunTerminal::Completed,
                ),
                cx,
            );
            assert!(!this.awaiting_first_token);
        });
    }

    #[gpui::test]
    fn test_promoted_queue_item_requests_its_attachments(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            let mut promoted = user_item("u1", "queued text");
            promoted.input = Some(serde_json::json!({
                "imageAttachments": [{ "id": "att-9", "name": "shot.png" }]
            }));
            this.handle_run_event(
                "s1",
                "run-1",
                maple_agent::agent::AgentRunEvent::QueuePromoted {
                    queue_id: "q1".to_string(),
                    snapshot: maple_agent::agent::AgentDesktopQueueSnapshot {
                        revision: 1,
                        items: Vec::new(),
                    },
                    item: promoted,
                },
                cx,
            );
            assert_eq!(this.timeline.len(), 1);
            assert!(this.attachment_requests.contains("att-9"));
        });
    }

    #[gpui::test]
    fn test_toggle_tool_finds_its_row_by_id(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.replace_timeline(vec![item("t1", "tool", None), item("t2", "tool", None)]);
            this.toggle_tool("t2", cx);
            assert!(this.toggled_tools.contains("t2"));
            // An id that left the timeline toggles nothing and does not
            // touch the list.
            this.toggle_tool("gone", cx);
            assert!(this.toggled_tools.contains("gone"));
            this.toggle_tool("t2", cx);
            assert!(!this.toggled_tools.contains("t2"));
        });
    }

    #[gpui::test]
    fn test_usage_poller_follows_the_selected_session(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.start_usage_poller("s1".to_string(), cx);
            assert_eq!(this.usage_poller_session.as_deref(), Some("s1"));
            let first = this.usage_poller_generation;
            // Same session: the running poller is kept.
            this.start_usage_poller("s1".to_string(), cx);
            assert_eq!(this.usage_poller_generation, first);
            // A run on another session that is not on screen gets none.
            this.start_usage_poller("s2".to_string(), cx);
            assert_eq!(this.usage_poller_session.as_deref(), Some("s1"));
            // Once that session is selected, its run replaces the poller.
            this.selected_session = Some("s2".to_string());
            this.start_usage_poller("s2".to_string(), cx);
            assert_eq!(this.usage_poller_session.as_deref(), Some("s2"));
            assert_ne!(this.usage_poller_generation, first);
        });
    }

    #[gpui::test]
    fn test_second_mic_click_while_opening_is_ignored(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.toggle_recording(cx);
            assert!(this.recording_starting);
            // The first start has not landed: the second click must not
            // open the microphone again or stop a recording that is not
            // there yet.
            this.toggle_recording(cx);
            assert!(this.recording_starting);
            assert!(!this.recording);
        });
    }

    #[gpui::test]
    fn test_leaving_the_selected_task_clears_its_composer_state(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.booting = false;
            this.ask_side_question("s1", "why?", cx);
            this.queue_edit = Some(QueueEdit {
                queue_id: "q1".to_string(),
                draft: String::new(),
            });
            this.leave_selected_session(cx);
            assert_eq!(this.selected_session, None);
            assert!(this.btw.is_none());
            assert!(this.queue.is_empty());
            assert!(this.queue_edit.is_none());
        });
    }

    #[gpui::test]
    fn test_project_selection_clears_view_but_preserves_background_work(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.booting = false;
            this.active_runs
                .insert("s1".to_string(), "run-1".to_string());
            this.pending_questions = vec![crate::backend::PendingQuestion {
                session_id: "s1".to_string(),
                request_id: "question-1".to_string(),
                questions: Vec::new(),
            }];
            this.queue_edit = Some(QueueEdit {
                queue_id: "q1".to_string(),
                draft: String::new(),
            });

            let left = this.clear_selected_session_presentation(cx);

            assert_eq!(left.as_deref(), Some("s1"));
            assert_eq!(this.selected_session, None);
            assert!(this.queue.is_empty());
            assert!(this.queue_edit.is_none());
            assert_eq!(
                this.active_runs.get("s1").map(String::as_str),
                Some("run-1")
            );
            assert_eq!(this.pending_questions.len(), 1);
        });
    }

    #[gpui::test]
    fn test_archive_root_during_project_selection_sets_a_notice(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.root_selecting = true;
            this.archive_root("/tmp/proj", cx);
            assert!(this.notice.is_some());
        });
    }

    #[gpui::test]
    fn test_switching_tasks_drops_item_caches(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.tool_summaries.insert("t1".to_string(), "old".into());
            this.attachment_requests.insert("att-1".to_string());
            this.toggled_tools.insert("t1".to_string());
            let stored = HashMap::from([("t9".to_string(), SharedString::from("stored"))]);
            this.set_active_session(summary("s2", "B"), Vec::new(), stored, cx);
            assert!(!this.tool_summaries.contains_key("t1"));
            assert_eq!(
                this.tool_summaries.get("t9").map(|s| s.as_ref()),
                Some("stored")
            );
            assert!(this.attachment_requests.is_empty());
            assert!(this.toggled_tools.is_empty());
            // Re-selecting the same task keeps them.
            this.toggled_tools.insert("t2".to_string());
            this.set_active_session(summary("s2", "B"), Vec::new(), HashMap::new(), cx);
            assert!(this.toggled_tools.contains("t2"));
            assert!(this.tool_summaries.contains_key("t9"));
        });
    }

    #[gpui::test]
    fn test_sidebar_rows_follow_titles_and_project_names(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.sessions = vec![summary("s1", "Fix Login")];
            this.sync_sidebar(cx);
            {
                let sidebar = this.sidebar.read(cx);
                assert_eq!(sidebar.rows().len(), 1);
                assert_eq!(sidebar.rows()[0].search, "fix login");
                assert_eq!(sidebar.rows()[0].project_name.as_ref(), "proj");
            }
            // The filter matches the project name case-insensitively.
            this.set_sidebar_filter(&"PROJ".to_lowercase(), cx);
            assert_eq!(this.sidebar_active(cx), vec![0]);
            this.set_sidebar_filter("", cx);
            // A rename moves the row strings.
            this.upsert_session(summary("s1", "Renamed"), cx);
            assert_eq!(this.sidebar.read(cx).rows()[0].title.as_ref(), "Renamed");
            this.sidebar.update(cx, |sidebar, _| {
                sidebar.set_project_name_for_test("/tmp/proj", "Nice");
            });
            assert_eq!(
                this.sidebar.read(cx).rows()[0].project_name.as_ref(),
                "Nice"
            );
        });
    }

    /// Task stepping walks the sidebar rows across projects and stops at both ends.
    #[gpui::test]
    fn test_task_stepping_walks_the_whole_list(cx: &mut TestAppContext) {
        let local = |id: &str| Navigate {
            computer: None,
            destination: Destination::Task(id.into()),
        };
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            let mut other = summary("s3", "C");
            other.project_root = "/work/beta".to_string();
            this.sessions = vec![summary("s1", "A"), summary("s2", "B"), other];
            this.project_root = Some("/tmp/proj".to_string());
            this.sync_sidebar(cx);
            // The screen fixture selects s1: one step moves to s2, and
            // back stops at the top.
            assert_eq!(this.task_step_target(1, cx), Some((4, local("s2"))));
            assert_eq!(
                this.task_step_target(-1, cx),
                None,
                "the first task is the top"
            );
            this.selected_session = Some("s2".to_string());
            this.sync_sidebar_selection(cx);
            assert_eq!(this.task_step_target(1, cx), Some((5, local("s3"))));
            assert_eq!(this.task_step_target(-1, cx), Some((3, local("s1"))));
            // With nothing selected, each direction enters from its end.
            this.selected_session = None;
            this.sync_sidebar_selection(cx);
            assert_eq!(this.task_step_target(1, cx), Some((3, local("s1"))));
            assert_eq!(this.task_step_target(-1, cx), Some((5, local("s3"))));
        });
    }

    #[gpui::test]
    fn test_cross_project_task_selection_preserves_all_active_runs(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.project_root = Some("/work/alpha".to_string());
            this.sessions = vec![
                summary_at("s1", "A", "/work/alpha"),
                summary_at("s2", "B", "/work/beta"),
            ];
            this.selected_session = Some("s1".to_string());
            this.active_runs = HashMap::from([
                ("s1".to_string(), "run-a".to_string()),
                ("s2".to_string(), "run-b".to_string()),
            ]);
            this.tool_summaries.insert("old".to_string(), "old".into());
            this.attachment_requests.insert("old".to_string());

            this.set_active_session(
                summary_at("s2", "B", "/work/beta"),
                Vec::new(),
                HashMap::new(),
                cx,
            );

            assert_eq!(this.selected_session.as_deref(), Some("s2"));
            assert_eq!(this.project_root.as_deref(), Some("/work/beta"));
            assert_eq!(this.active_runs.len(), 2);
            assert_eq!(
                this.active_runs.get("s1").map(String::as_str),
                Some("run-a")
            );
            assert_eq!(
                this.active_runs.get("s2").map(String::as_str),
                Some("run-b")
            );
            assert!(this.tool_summaries.is_empty());
            assert!(this.attachment_requests.is_empty());
            assert!(!this.root_selecting);
        });
    }

    #[gpui::test]
    fn test_new_task_request_always_names_the_visible_project(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, _cx| {
            assert!(this.new_session_request().is_none());
            this.project_root = Some("/work/beta".to_string());
            let request = this.new_session_request().expect("explicit root request");
            assert_eq!(request.project_root.as_deref(), Some("/work/beta"));
        });
    }

    #[gpui::test]
    fn test_stale_new_task_completion_keeps_its_row_without_stealing_selection(
        cx: &mut TestAppContext,
    ) {
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.project_root = Some("/work/beta".to_string());
            this.selected_session = Some("newer".to_string());
            this.selection_generation = 2;
            this.session_setup_pending = true;
            this.pending_first_send = Some(FirstSend::Message {
                text: "hello".to_string(),
                steer: false,
            });

            this.finish_new_session(summary_at("created", "Created", "/work/alpha"), 1, cx);

            assert!(!this.session_setup_pending);
            assert_eq!(this.selected_session.as_deref(), Some("newer"));
            assert_eq!(this.project_root.as_deref(), Some("/work/beta"));
            assert!(this.sessions.iter().any(|session| session.id == "created"));
            // The message it was created for is not sent into a task the
            // user left; it is reported instead.
            assert!(this.pending_first_send.is_none());
            assert!(!this.awaiting_first_token);
            assert!(this.notice.is_some());
        });
    }

    #[gpui::test]
    fn test_running_and_completed_unread_are_session_scoped(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        use maple_agent::agent::{AgentRunEvent, AgentRunTerminal};

        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.sessions = vec![
                summary_at("s1", "A", "/work/alpha"),
                summary_at("s2", "B", "/work/beta"),
            ];
            this.selected_session = Some("s1".to_string());

            this.handle_run_event("s1", "run-a", AgentRunEvent::Started, cx);
            this.handle_run_event("s2", "run-b", AgentRunEvent::Started, cx);
            assert_eq!(this.session_activity("s1"), Some(SessionActivity::Running));
            assert_eq!(this.session_activity("s2"), Some(SessionActivity::Running));

            this.handle_run_event(
                "s2",
                "run-b",
                AgentRunEvent::Finished(AgentRunTerminal::Completed),
                cx,
            );
            assert_eq!(this.session_activity("s1"), Some(SessionActivity::Running));
            assert_eq!(
                this.session_activity("s2"),
                Some(SessionActivity::CompletedUnread)
            );

            // The marker survives the click; the snapshot landing reads it.
            this.select_session("s2", cx);
            assert_eq!(
                this.session_activity("s2"),
                Some(SessionActivity::CompletedUnread)
            );
            this.set_active_session(
                summary_at("s2", "B", "/work/beta"),
                Vec::new(),
                HashMap::new(),
                cx,
            );
            assert_eq!(this.session_activity("s2"), None);
            assert_eq!(this.session_activity("s1"), Some(SessionActivity::Running));
        });
    }

    /// A queued turn that starts after a completion and then fails must
    /// not leave the earlier completion's marker behind.
    #[gpui::test]
    fn test_later_failed_run_clears_an_earlier_unread_completion(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        use maple_agent::agent::{AgentRunEvent, AgentRunTerminal};

        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.handle_run_event("s2", "run-1", AgentRunEvent::Started, cx);
            this.handle_run_event(
                "s2",
                "run-1",
                AgentRunEvent::Finished(AgentRunTerminal::Completed),
                cx,
            );
            this.handle_run_event("s2", "run-2", AgentRunEvent::Started, cx);
            assert_eq!(this.session_activity("s2"), Some(SessionActivity::Running));
            this.handle_run_event(
                "s2",
                "run-2",
                AgentRunEvent::Finished(AgentRunTerminal::Failed),
                cx,
            );
            assert_eq!(this.session_activity("s2"), None);
        });
    }

    #[gpui::test]
    fn test_only_successful_background_completion_becomes_unread(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        use maple_agent::agent::{AgentRunEvent, AgentRunTerminal};

        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.selected_session = Some("shown".to_string());
            for (session, run, terminal) in [
                ("failed", "run-failed", AgentRunTerminal::Failed),
                ("cancelled", "run-cancelled", AgentRunTerminal::Cancelled),
            ] {
                this.handle_run_event(session, run, AgentRunEvent::Started, cx);
                this.handle_run_event(session, run, AgentRunEvent::Finished(terminal), cx);
            }
            this.handle_run_event("shown", "run-shown", AgentRunEvent::Started, cx);
            this.handle_run_event(
                "shown",
                "run-shown",
                AgentRunEvent::Finished(AgentRunTerminal::Completed),
                cx,
            );

            assert!(this.completed_unread_sessions.is_empty());
        });
    }

    #[gpui::test]
    fn test_terminal_run_cannot_be_resurrected_by_an_older_status(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        use maple_agent::agent::{AgentRunEvent, AgentRunTerminal, AgentRuntimeStatus};

        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.handle_run_event("s2", "run-2", AgentRunEvent::Started, cx);
            this.handle_run_event(
                "s2",
                "run-2",
                AgentRunEvent::Finished(AgentRunTerminal::Completed),
                cx,
            );
            assert!(!this.active_runs.contains_key("s2"));

            this.apply_service_event(
                AgentServiceEvent::RuntimeStatus(AgentRuntimeStatus {
                    running: true,
                    project_root: Some("/work/alpha".to_string()),
                    model: None,
                    active_runs: HashMap::from([("s2".to_string(), "run-2".to_string())]),
                }),
                cx,
            );

            assert!(!this.active_runs.contains_key("s2"));
            assert_eq!(
                this.session_activity("s2"),
                Some(SessionActivity::CompletedUnread)
            );
        });
    }

    #[gpui::test]
    fn test_finished_only_clears_its_own_run(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.active_runs
                .insert("s1".to_string(), "run-old".to_string());
            this.active_runs
                .insert("s1".to_string(), "run-new".to_string());
            this.handle_run_event(
                "s1",
                "run-old",
                maple_agent::agent::AgentRunEvent::Finished(
                    maple_agent::agent::AgentRunTerminal::Completed,
                ),
                cx,
            );
            assert_eq!(
                this.active_runs.get("s1").map(String::as_str),
                Some("run-new")
            );
            assert!(!this.completed_unread_sessions.contains("s1"));
        });
    }

    fn png_bytes() -> Vec<u8> {
        // Only the signature matters: the draft checks the magic bytes.
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend_from_slice(&[0; 16]);
        bytes
    }

    /// The lightbox draws in a debug build, where gpui asserts each
    /// element has at most one tooltip, and Escape closes it.
    #[gpui::test]
    fn test_lightbox_draws_and_escape_closes_it(cx: &mut TestAppContext) {
        let (chat, cx) = chat_window(cx, |this, cx| {
            let image = gpui::Image::from_bytes(gpui::ImageFormat::Png, png_bytes());
            this.open_lightbox(Arc::new(image), cx);
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        chat.update(cx, |this, cx| {
            assert!(this.lightbox.is_some());
            this.escape(cx);
            assert!(this.lightbox.is_none());
        });
    }

    #[gpui::test]
    fn test_paste_image_stages_a_draft(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            let image = gpui::Image::from_bytes(gpui::ImageFormat::Png, png_bytes());
            this.paste_image(image, cx);
            assert_eq!(this.draft_images.len(), 1);
            assert_eq!(this.draft_images[0].name, "pasted-1.png");
            // The encode runs off the UI thread; the draft is staged at
            // once and cannot be sent until it lands.
            assert!(!this.draft_images[0].ready());
            assert!(this.notice.is_none());
        });
    }

    #[test]
    fn test_data_url_encodes_the_signature() {
        let url = encode_data_url("image/png", &png_bytes());
        assert!(url.starts_with("data:image/png;base64,iVBORw0KGgo"));
    }

    #[gpui::test]
    fn test_paste_image_rejects_unsupported_format(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            let image = gpui::Image::from_bytes(gpui::ImageFormat::Gif, b"GIF89a".to_vec());
            this.paste_image(image, cx);
            assert!(this.draft_images.is_empty());
            assert!(
                this.notice
                    .as_deref()
                    .is_some_and(|notice| notice.contains("not a PNG, JPEG, or WebP"))
            );
        });
    }

    #[gpui::test]
    fn test_paste_image_respects_limit(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            for _ in 0..=MAX_DRAFT_IMAGES {
                let image = gpui::Image::from_bytes(gpui::ImageFormat::Png, png_bytes());
                this.paste_image(image, cx);
            }
            assert_eq!(this.draft_images.len(), MAX_DRAFT_IMAGES);
            assert!(
                this.notice
                    .as_deref()
                    .is_some_and(|notice| notice.contains("at most"))
            );
        });
    }

    /// The sidebar list builds its rows by updating the chat entity from
    /// inside the list callback during a draw; that must be legal and
    /// must only build the rows in view.
    #[gpui::test]
    fn test_sidebar_list_draws_rows_from_the_entity(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        struct SidebarHost {
            chat: Entity<ChatScreen>,
        }
        impl Render for SidebarHost {
            fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                div()
                    .w(px(300.))
                    .h(px(400.))
                    .flex()
                    .flex_col()
                    .child(self.chat.read(cx).sidebar.clone())
            }
        }

        // This gpui's test scheduler flags activity on other threads unless
        // parking is allowed; the backend runtime and image encoder run on tokio.
        cx.executor().allow_parking();
        let chat = cx.new(|cx| {
            let _guard = SETTINGS_LOCK.lock();
            let backend = std::sync::Arc::new(
                crate::backend::AgentBackend::for_test(
                    "http://127.0.0.1:9".to_string(),
                    String::new(),
                )
                .expect("backend"),
            );
            let mut this = ChatScreen::new_inner(backend, "user".to_string(), cx);
            this.selected_session = Some("s1".to_string());
            this.sessions = (0..200)
                .map(|n| summary(&format!("s{n}"), &format!("Task {n}")))
                .collect();
            this.sync_sidebar(cx);
            this
        });
        let (_host, cx) = cx.add_window_view(|_window, _cx| SidebarHost { chat: chat.clone() });
        cx.simulate_resize(gpui::size(px(300.), px(400.)));
        cx.update(|_window, app| {
            let sidebar = chat.read(app).sidebar.read(app);
            assert_eq!(sidebar.list().item_count(), sidebar.entries().len());
            assert_eq!(
                sidebar.vim_target(0),
                None,
                "the sidebar must not build an application-navigation cache while it is off"
            );
            assert_eq!(sidebar.vim_target(1), None);
            let top = sidebar.list().logical_scroll_top();
            assert_eq!(top.item_ix, 0);
        });
        // The list scrolls: a wheel moves the logical top down the rows.
        let center = cx.update(|window, _cx| window.bounds().center());
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: center,
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-300.))),
            ..Default::default()
        });
        cx.update(|_window, app| {
            let top = chat.read(app).sidebar.read(app).list().logical_scroll_top();
            assert!(
                top.item_ix > 0,
                "wheel must scroll the sidebar, got {top:?}"
            );
        });
    }

    #[gpui::test]
    fn test_application_vim_off_keeps_chat_projection_empty(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let chat = screen(cx);
        chat.update(cx, |this, cx| {
            this.set_application_vim_enabled(false, cx);
            this.sessions = vec![summary("s1", "Task")];
            this.sync_sidebar(cx);
            this.replace_timeline(vec![
                user_item("u1", "question"),
                item("a1", "message", Some("answer")),
            ]);

            assert!(
                this.application_vim_projection_is_empty(cx),
                "loading a timeline and rebuilding the sidebar must not populate disabled navigation state"
            );

            this.set_application_vim_enabled(true, cx);
            assert!(!this.application_vim_projection_is_empty(cx));
            this.set_application_vim_enabled(false, cx);
            assert!(
                this.application_vim_projection_is_empty(cx),
                "disabling the feature must release its cached projection"
            );
        });
    }

    /// A transcript several screens tall, in rows of varying height.
    fn long_timeline() -> Vec<AgentTimelineItem> {
        (0..120)
            .map(|i| {
                item(
                    &format!("m{i}"),
                    "message",
                    Some(&format!(
                        "Message {i}\n\n{}",
                        "lorem ipsum dolor sit amet ".repeat(8 + i % 5 * 10)
                    )),
                )
            })
            .collect()
    }

    fn wheel_pixels(dy: f32) -> gpui::ScrollDelta {
        gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(dy)))
    }

    fn wheel_at(cx: &mut gpui::VisualTestContext, x: f32, y: f32, delta: gpui::ScrollDelta) {
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: gpui::point(px(x), px(y)),
            delta,
            ..Default::default()
        });
        cx.run_until_parked();
    }

    /// The row at the top of the viewport, which a wheel up keeps on
    /// screen.
    fn reference_row(chat: &Entity<ChatScreen>, cx: &mut gpui::VisualTestContext) -> usize {
        cx.update(|_window, app| chat.read(app).list_state.logical_scroll_top().item_ix)
    }

    /// Where transcript row `ix` sits in the window, while it is on screen.
    fn row_top(
        chat: &Entity<ChatScreen>,
        cx: &mut gpui::VisualTestContext,
        ix: usize,
    ) -> Option<gpui::Pixels> {
        cx.update(|_window, app| {
            chat.read(app)
                .list_state
                .bounds_for_item(ix)
                .map(|bounds| bounds.top())
        })
    }

    fn following_tail(chat: &Entity<ChatScreen>, cx: &mut gpui::VisualTestContext) -> bool {
        cx.update(|_window, app| chat.read(app).list_state.is_following_tail())
    }

    /// In the 1200 px test window the transcript and the composer share
    /// the middle 900 px column. A wheel that nothing under the pointer
    /// takes scrolls the transcript exactly as far as a wheel over the
    /// transcript itself: the margins, the gutter beside the text, the
    /// scrollbar strip, the space beside the composer. The header and the
    /// composer keep their own wheel.
    #[gpui::test]
    fn test_wheel_anywhere_in_the_pane_scrolls_the_transcript(cx: &mut TestAppContext) {
        let (chat, cx) = chat_window(cx, |chat, _cx| {
            chat.sidebar_collapsed = true;
            chat.replace_timeline(long_timeline());
        });
        // Off the tail, so a row near the top is a fixed point.
        wheel_at(cx, 600., 300., wheel_pixels(400.));
        for (place, x, y, moves) in [
            ("the transcript", 600., 300., 100.),
            ("the left margin", 40., 300., 100.),
            ("the gutter beside the text", 160., 300., 100.),
            ("the scrollbar strip", 1195., 300., 100.),
            ("the margin beside the composer", 1150., 790., 100.),
            ("the header", 600., 20., 0.),
            ("the composer", 600., 790., 0.),
        ] {
            let ix = reference_row(&chat, cx);
            let before = row_top(&chat, cx, ix).expect("the reference row is on screen");
            wheel_at(cx, x, y, wheel_pixels(100.));
            let after = row_top(&chat, cx, ix).expect("the reference row is on screen");
            assert_eq!(after - before, px(moves), "a wheel over {place}");
        }

        // Lines use the list's 20 px line.
        let ix = reference_row(&chat, cx);
        let before = row_top(&chat, cx, ix).expect("the reference row is on screen");
        wheel_at(
            cx,
            1150.,
            790.,
            gpui::ScrollDelta::Lines(gpui::point(0., 3.)),
        );
        let after = row_top(&chat, cx, ix).expect("the reference row is on screen");
        assert_eq!(after - before, px(60.), "three lines beside the composer");

        // Up lets go of the tail; back at the bottom, the list takes it again.
        assert!(!following_tail(&chat, cx));
        for _ in 0..10 {
            wheel_at(cx, 1150., 790., wheel_pixels(-400.));
        }
        assert!(
            following_tail(&chat, cx),
            "a wheel down to the end beside the composer takes the tail again"
        );
    }

    /// The transcript list is only as wide as its column, so on a wide
    /// window opening the sidebar (or resizing) keeps every measured row:
    /// the scroll range holds and the scrollbar thumb dragged to the top
    /// reaches the first row.
    #[gpui::test]
    fn test_sidebar_toggle_keeps_the_transcript_measured(cx: &mut TestAppContext) {
        let (chat, cx) = chat_window(cx, |chat, _cx| {
            chat.sidebar_collapsed = true;
            chat.replace_timeline(vec![item("m0", "message", Some("hello"))]);
        });
        // A task opened after the first layout, with row estimates.
        cx.update(|_window, app| {
            chat.update(app, |chat, cx| {
                chat.replace_timeline(long_timeline());
                cx.notify();
            })
        });
        cx.run_until_parked();
        for _ in 0..10 {
            wheel_at(cx, 600., 300., wheel_pixels(400.));
        }
        let range = |cx: &mut gpui::VisualTestContext| {
            cx.update(|_window, app| chat.read(app).list_state.max_offset_for_scrollbar().y)
        };
        let before = range(cx);
        cx.update(|_window, app| {
            chat.update(app, |chat, cx| chat.set_sidebar_collapsed(false, cx))
        });
        cx.run_until_parked();
        assert_eq!(
            range(cx),
            before,
            "opening the sidebar keeps the measured rows"
        );

        cx.update(|_window, app| {
            chat.update(app, |chat, cx| {
                chat.list_state
                    .set_offset_from_scrollbar(gpui::point(px(0.), px(0.)));
                cx.notify();
            })
        });
        cx.run_until_parked();
        assert_eq!(
            cx.update(|_window, app| chat.read(app).list_state.logical_scroll_top().item_ix),
            0,
            "the thumb dragged to the top reaches the first row"
        );
    }

    /// At the bottom the list's scroll top names no row. A wheel beside
    /// the composer still reports the rows on screen, the same rows the
    /// list's own wheel reports, so images in view keep loading.
    #[gpui::test]
    fn test_wheel_at_the_bottom_reports_the_rows_on_screen(cx: &mut TestAppContext) {
        let (chat, cx) = chat_window(cx, |chat, _cx| {
            chat.sidebar_collapsed = true;
            chat.replace_timeline(
                (0..40)
                    .map(|i| item(&format!("m{i}"), "message", Some(&format!("Short {i}"))))
                    .collect(),
            );
        });
        let viewport = |cx: &mut gpui::VisualTestContext| {
            cx.update(|_window, app| chat.read(app).attachment_viewport.clone())
        };
        wheel_at(cx, 600., 300., wheel_pixels(-100.));
        let from_list = viewport(cx);
        assert!(
            from_list.len() > 10,
            "many short rows are on screen, got {from_list:?}"
        );
        cx.update(|_window, app| chat.update(app, |chat, _cx| chat.attachment_viewport = 0..0));
        wheel_at(cx, 1150., 790., wheel_pixels(-100.));
        assert_eq!(viewport(cx), from_list);
    }

    /// An open image covers the screen: a wheel anywhere leaves the
    /// transcript underneath where it was.
    #[gpui::test]
    fn test_open_lightbox_holds_the_transcript(cx: &mut TestAppContext) {
        let (chat, cx) = chat_window(cx, |chat, _cx| {
            chat.sidebar_collapsed = true;
            chat.replace_timeline(long_timeline());
        });
        wheel_at(cx, 600., 300., wheel_pixels(400.));
        cx.update(|_window, app| {
            chat.update(app, |chat, cx| {
                let image = gpui::Image::from_bytes(gpui::ImageFormat::Png, png_bytes());
                chat.open_lightbox(Arc::new(image), cx);
            })
        });
        cx.run_until_parked();
        let ix = reference_row(&chat, cx);
        let before = row_top(&chat, cx, ix);
        for (x, y) in [(600., 300.), (40., 300.), (1150., 790.)] {
            wheel_at(cx, x, y, wheel_pixels(100.));
        }
        assert_eq!(row_top(&chat, cx, ix), before);
    }

    /// A message being sent shows in a bubble that scrolls itself. A
    /// wheel over the bubble scrolls only the bubble; one beside it, the
    /// transcript.
    #[gpui::test]
    fn test_sending_bubble_keeps_its_wheel(cx: &mut TestAppContext) {
        let (chat, cx) = chat_window(cx, |chat, _cx| {
            chat.sidebar_collapsed = true;
            chat.replace_timeline(long_timeline());
            chat.sending_previews.insert(
                "send-1".to_string(),
                sending::SendPreview::new(
                    Some("s1".to_string()),
                    &"A long message\n".repeat(40),
                    &[],
                ),
            );
        });
        wheel_at(cx, 600., 200., wheel_pixels(400.));
        let bubble = cx
            .debug_bounds("pending-send-body")
            .expect("the bubble shows");
        let (x, y) = (f32::from(bubble.center().x), f32::from(bubble.center().y));
        let ix = reference_row(&chat, cx);
        let before = row_top(&chat, cx, ix).expect("the reference row is on screen");
        wheel_at(cx, x, y, wheel_pixels(-100.));
        assert_eq!(
            row_top(&chat, cx, ix),
            Some(before),
            "a wheel over the bubble leaves the transcript alone"
        );
        wheel_at(cx, f32::from(bubble.left()) - 100., y, wheel_pixels(100.));
        assert_eq!(
            row_top(&chat, cx, ix).map(|after| after - before),
            Some(px(100.)),
            "a wheel beside the bubble scrolls the transcript"
        );
    }

    /// A question card keeps its own wheel like the composer; the space
    /// beside it scrolls the transcript.
    #[gpui::test]
    fn test_question_card_keeps_its_wheel(cx: &mut TestAppContext) {
        let (chat, cx) = chat_window(cx, |chat, _cx| {
            chat.sidebar_collapsed = true;
            chat.replace_timeline(long_timeline());
            chat.pending_questions = vec![crate::backend::PendingQuestion {
                session_id: "s1".to_string(),
                request_id: "q1".to_string(),
                questions: vec![maple_agent::agent::AgentQuestion {
                    multi_select: false,
                    id: "pick".to_string(),
                    header: "Question".to_string(),
                    question: "Which one?".to_string(),
                    options: Vec::new(),
                }],
            }];
        });
        wheel_at(cx, 600., 200., wheel_pixels(400.));
        let card = cx
            .debug_bounds("question-card")
            .expect("the question card shows");
        let y = f32::from(card.center().y);
        let ix = reference_row(&chat, cx);
        let before = row_top(&chat, cx, ix).expect("the reference row is on screen");
        wheel_at(cx, f32::from(card.center().x), y, wheel_pixels(100.));
        assert_eq!(
            row_top(&chat, cx, ix),
            Some(before),
            "a wheel over the card leaves the transcript alone"
        );
        wheel_at(cx, 40., y, wheel_pixels(100.));
        assert_eq!(
            row_top(&chat, cx, ix).map(|after| after - before),
            Some(px(100.)),
            "a wheel beside the card scrolls the transcript"
        );
    }

    #[gpui::test]
    fn test_streaming_chunk_keeps_wheel_scrolling_up(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        assert_streaming_chunk_keeps_wheel_scrolling_up(cx, false);
    }

    #[gpui::test]
    fn test_application_vim_streaming_chunk_keeps_wheel_scrolling_up(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        assert_streaming_chunk_keeps_wheel_scrolling_up(cx, true);
    }

    fn assert_streaming_chunk_keeps_wheel_scrolling_up(
        cx: &mut TestAppContext,
        application_vim_enabled: bool,
    ) {
        /// A paragraph long enough to need real vertical space when rendered.
        const PARA: &str = "The quick brown fox jumps over the lazy dog. \
            Pack my box with five dozen liquor jugs. How vexingly quick daft zebras jump! ";

        // Hosts only the transcript so the list gets a realistic viewport,
        // independent of the rest of the screen's layout.
        struct TranscriptHost {
            chat: Entity<ChatScreen>,
        }
        impl Render for TranscriptHost {
            fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                // Fixed size: the test window's own bounds are not applied
                // to the root view in the harness, and a percentage height
                // collapses to zero.
                div().w(px(1200.)).h(px(800.)).flex().flex_col().child(
                    self.chat
                        .update(cx, |chat, cx| chat.render_transcript(window, cx)),
                )
            }
        }

        let chat = cx.new(move |cx| {
            let _guard = SETTINGS_LOCK.lock();
            let backend = std::sync::Arc::new(
                crate::backend::AgentBackend::for_test(
                    "http://127.0.0.1:9".to_string(),
                    String::new(),
                )
                .expect("backend"),
            );
            let mut this = ChatScreen::new_inner(backend, "user".to_string(), cx);
            this.selected_session = Some("s1".to_string());
            this.application_vim_enabled = application_vim_enabled;
            this.application_focus = application_vim_enabled.then(|| cx.focus_handle());
            this
        });

        // A long history, then a streaming answer that is already tall
        // while the view is pinned to the newest item.
        chat.update(cx, |this, _cx| {
            let mut timeline = Vec::new();
            for i in 0..20 {
                timeline.push(item(
                    &format!("u{i}"),
                    "message",
                    Some(&format!("User {i} {PARA}")),
                ));
                timeline.push(item(
                    &format!("a{i}"),
                    "message",
                    Some(&format!("Reply {i} {PARA}{PARA}")),
                ));
            }
            this.replace_timeline(timeline);
            this.apply_timeline_item("s1", item("stream", "message", Some("Answer: ")));
            for _ in 0..30 {
                this.apply_timeline_item(
                    "s1",
                    AgentTimelineItem {
                        merge: "append".to_string(),
                        text: Some(PARA.repeat(3)),
                        ..item("stream", "message", None)
                    },
                );
            }
        });

        let (_host, cx) = cx.add_window_view(|_window, _cx| TranscriptHost { chat: chat.clone() });
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        let center = cx.update(|window, _cx| window.bounds().center());

        let pinned = cx.update(|_window, app| chat.read(app).list_state.logical_scroll_top());
        assert_eq!(
            (pinned.item_ix, pinned.offset_in_item),
            (41, px(0.)),
            "the transcript must start pinned to the newest item, got {pinned:?}"
        );

        let wheel_up = |cx: &mut gpui::VisualTestContext| {
            cx.simulate_event(gpui::ScrollWheelEvent {
                position: center,
                delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(100.))),
                ..Default::default()
            });
        };

        // Control: an idle wheel moves the viewport up.
        wheel_up(cx);
        let moved = cx.update(|_window, app| chat.read(app).list_state.logical_scroll_top());
        assert_ne!(
            (moved.item_ix, moved.offset_in_item),
            (pinned.item_ix, pinned.offset_in_item),
            "an idle wheel must move the viewport, got {moved:?}"
        );

        // A streamed chunk lands. No repaint runs before the user's wheel
        // event, exactly like a frame that is still pending.
        cx.update(|_window, app| {
            chat.update(app, |this, cx| {
                this.apply_incoming_item(
                    "s1",
                    AgentTimelineItem {
                        merge: "append".to_string(),
                        text: Some(PARA.to_string()),
                        ..item("stream", "message", None)
                    },
                    cx,
                );
            })
        });
        wheel_up(cx);

        let after = cx.update(|_window, app| chat.read(app).list_state.logical_scroll_top());
        assert_ne!(
            (after.item_ix, after.offset_in_item),
            (pinned.item_ix, pinned.offset_in_item),
            "a wheel between a streamed chunk and the next paint must move the viewport up, got {after:?}"
        );

        // The row at the top of the viewport changes in place. The list
        // remeasures it rather than splicing, so the scroll anchor (row and
        // offset within it) stays exactly where the user left it.
        let anchored_id = match after.item_ix {
            40 => "stream".to_string(),
            ix if ix % 2 == 0 => format!("u{}", ix / 2),
            ix => format!("a{}", ix / 2),
        };
        cx.update(|_window, app| {
            chat.update(app, |this, cx| {
                this.apply_incoming_item(
                    "s1",
                    AgentTimelineItem {
                        merge: "append".to_string(),
                        text: Some(PARA.to_string()),
                        ..item(&anchored_id, "message", None)
                    },
                    cx,
                );
            })
        });
        let held = cx.update(|_window, app| chat.read(app).list_state.logical_scroll_top());
        assert_eq!(
            (held.item_ix, held.offset_in_item),
            (after.item_ix, after.offset_in_item),
            "a row changing under the viewport must not move the scroll anchor, got {held:?}"
        );
    }

    /// Plain typing while the transcript holds focus must land in the
    /// composer, including the first character, without a click first.
    /// Chords and enter/tab keep their meaning instead of stealing focus.
    #[gpui::test]
    fn test_typing_with_transcript_focused_lands_in_composer(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        // Hosts the whole screen so the chat root's key listener is on the
        // dispatch path, exactly like the real window.
        struct ChatHost {
            chat: Entity<ChatScreen>,
        }
        impl Render for ChatHost {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut Context<Self>,
            ) -> impl IntoElement {
                div().w(px(1200.)).h(px(800.)).child(self.chat.clone())
            }
        }

        let chat = cx.new(|cx| {
            let _guard = SETTINGS_LOCK.lock();
            let backend = std::sync::Arc::new(
                crate::backend::AgentBackend::for_test(
                    "http://127.0.0.1:9".to_string(),
                    String::new(),
                )
                .expect("backend"),
            );
            ChatScreen::new(backend, "user".to_string(), cx)
        });
        chat.update(cx, |this, _cx| {
            // The real constructor bootstraps and may open the project-trust
            // dialog; that modal would take the keys.
            this.trust_prompts = false;
            this.selected_session = Some("s1".to_string());
            this.replace_timeline(vec![user_item("u1", "hello")]);
        });

        let (_host, cx) = cx.add_window_view(|_window, _cx| ChatHost { chat: chat.clone() });
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));

        let transcript_focus =
            cx.update(|_window, app| chat.read(app).transcript_focus.clone().unwrap());
        let composer_handle =
            cx.update(|_window, app| chat.read(app).composer.clone().unwrap().focus_handle(app));

        // Focus the transcript the way a text-selection press does.
        cx.update(|window, app| window.focus(&transcript_focus, app));

        // Typing routes straight into the composer, first character
        // included; the second arrives through the normal input path.
        cx.simulate_input("hi");
        cx.update(|_window, app| {
            chat.update(app, |this, cx| {
                assert_eq!(this.composer.as_ref().unwrap().read(cx).text(), "hi");
            })
        });
        assert_eq!(
            cx.update(|window, app| window.focused(app)),
            Some(composer_handle.clone()),
            "typing must move focus to the composer"
        );

        // A modifier chord keeps focus where it is.
        cx.update(|window, app| window.focus(&transcript_focus, app));
        cx.simulate_keystrokes("alt-h");
        cx.update(|_window, app| {
            chat.update(app, |this, cx| {
                assert_eq!(this.composer.as_ref().unwrap().read(cx).text(), "hi");
            })
        });
        assert_eq!(
            cx.update(|window, app| window.focused(app)),
            Some(transcript_focus.clone()),
            "a chord must not steal focus from the transcript"
        );

        // Enter and tab never route to the composer, even where the
        // platform reports a key_char for them.
        cx.simulate_keystrokes("enter tab");
        cx.update(|_window, app| {
            chat.update(app, |this, cx| {
                assert_eq!(this.composer.as_ref().unwrap().read(cx).text(), "hi");
            })
        });
        assert_eq!(
            cx.update(|window, app| window.focused(app)),
            Some(transcript_focus),
            "enter and tab must not steal focus from the transcript"
        );
    }

    #[gpui::test]
    fn test_application_vim_moves_over_the_semantic_transcript_projection(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        struct ChatHost {
            chat: Entity<ChatScreen>,
        }
        impl Render for ChatHost {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut Context<Self>,
            ) -> impl IntoElement {
                div().w(px(1200.)).h(px(800.)).child(self.chat.clone())
            }
        }

        // This gpui's test scheduler flags activity on other threads unless
        // parking is allowed; the backend runtime and image encoder run on tokio.
        cx.executor().allow_parking();
        let chat = cx.new(|cx| {
            let _guard = SETTINGS_LOCK.lock();
            let backend = std::sync::Arc::new(
                crate::backend::AgentBackend::for_test(
                    "http://127.0.0.1:9".to_string(),
                    String::new(),
                )
                .expect("backend"),
            );
            crate::desktop::register_key_bindings(cx);
            let mut chat = ChatScreen::new_inner(backend, "user".to_string(), cx);
            chat.selected_session = Some("s1".to_string());
            chat.application_vim_enabled = true;
            chat.application_focus = Some(cx.focus_handle());
            chat.screen_focus_pending = true;
            chat.booting = false;

            let mut assistant = item("a1", "message", Some("answer"));
            assistant.role = Some("assistant".to_string());
            chat.replace_timeline(vec![
                user_item("u1", "question"),
                item("internal", "internal", Some("state")),
                item("empty", "message", Some("   ")),
                todo_item("todo", serde_json::json!([])),
                assistant,
                item("tool", "tool", None),
            ]);
            chat
        });
        let (_host, cx) = cx.add_window_view(|_window, _cx| ChatHost { chat: chat.clone() });
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));

        assert_eq!(
            cx.update(|_window, app| chat.read(app).navigable_timeline_ids()),
            vec!["u1", "a1", "tool"]
        );
        assert_eq!(
            cx.update(|_window, app| chat.read(app).selected_transcript_id().map(str::to_owned)),
            Some("tool".to_string()),
            "a fresh semantic projection follows its newest visible item"
        );

        cx.simulate_keystrokes("g g");
        assert_eq!(
            cx.update(|_window, app| chat.read(app).selected_transcript_id().map(str::to_owned)),
            Some("u1".to_string())
        );
        cx.simulate_keystrokes("2 j");
        assert_eq!(
            cx.update(|_window, app| chat.read(app).selected_transcript_id().map(str::to_owned)),
            Some("tool".to_string()),
            "counts apply to semantic rows and skip non-renderable backing items"
        );
        cx.simulate_keystrokes("k");
        assert_eq!(
            cx.update(|_window, app| chat.read(app).selected_transcript_id().map(str::to_owned)),
            Some("a1".to_string())
        );

        // Expanding a transcript leaf is an expected Vim no-op, not an
        // application error that should leave a persistent banner behind.
        chat.update(cx, |this, _cx| this.notice = None);
        cx.simulate_keystrokes("l");
        cx.update(|_window, app| {
            let chat = chat.read(app);
            assert_eq!(chat.selected_transcript_id(), Some("a1"));
            assert!(chat.notice.is_none());
        });

        // The first move reaches the sidebar; repeating it at the left edge
        // is likewise a silent boundary no-op.
        cx.simulate_keystrokes("ctrl-w h");
        assert_eq!(
            cx.update(|_window, app| chat.read(app).application_vim.region),
            crate::ui::chat::navigation::ChatRegion::Sidebar
        );
        cx.simulate_keystrokes("ctrl-w h");
        cx.update(|_window, app| {
            let chat = chat.read(app);
            assert_eq!(
                chat.application_vim.region,
                crate::ui::chat::navigation::ChatRegion::Sidebar
            );
            assert!(chat.notice.is_none());
        });
    }

    #[gpui::test]
    fn test_application_vim_escape_assistant_focus_and_plain_command_count(
        cx: &mut TestAppContext,
    ) {
        struct ChatHost {
            chat: Entity<ChatScreen>,
        }
        impl Render for ChatHost {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut Context<Self>,
            ) -> impl IntoElement {
                div().w(px(1200.)).h(px(800.)).child(self.chat.clone())
            }
        }

        let chat = cx.new(|cx| {
            let _guard = SETTINGS_LOCK.lock();
            let backend = std::sync::Arc::new(
                crate::backend::AgentBackend::for_test(
                    "http://127.0.0.1:9".to_string(),
                    String::new(),
                )
                .expect("backend"),
            );
            crate::desktop::register_key_bindings(cx);
            let mut chat = ChatScreen::new_without_start(backend, "user".to_string(), cx);
            chat.selected_session = Some("s1".to_string());
            chat.booting = false;
            chat.application_vim_enabled = false;
            chat.composer_vim_enabled = false;
            if let Some(composer) = chat.composer.clone() {
                composer.update(cx, |input, cx| input.set_vim_enabled(false, cx));
            }
            let mut assistant = item("a1", "message", Some("answer"));
            assistant.role = Some("assistant".to_string());
            chat.replace_timeline(vec![
                user_item("u1", "question"),
                assistant,
                item("tool", "tool", None),
            ]);
            chat.set_application_vim_enabled(true, cx);
            chat
        });

        let (_host, cx) = cx.add_window_view(|_window, _cx| ChatHost { chat: chat.clone() });
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        let application_focus =
            cx.update(|_window, app| chat.read(app).application_focus.clone().unwrap());
        let composer = cx.update(|_window, app| chat.read(app).composer.clone().unwrap());
        let composer_focus = cx.update(|_window, app| composer.read(app).focus_handle(app));

        // A plain command always cancels an unfinished application count.
        cx.update(|window, app| {
            chat.update(app, |this, cx| {
                this.application_vim.count.push(4);
                this.execute_command(commands::ChatCommand::ToggleSidebar, window, cx);
                assert_eq!(this.application_vim.count.pending(), None);
            });
        });

        // With composer Vim disabled, Escape hands focus back to the
        // application proxy instead of being swallowed by TextInput.
        cx.update(|window, app| window.focus(&composer_focus, app));
        cx.simulate_keystrokes("escape");
        assert_eq!(
            cx.update(|window, app| window.focused(app)),
            Some(application_focus.clone())
        );

        // Once no focus transition consumes Escape, the same central route
        // still reaches Chat's legacy menu-close behavior.
        chat.update(cx, |this, cx| this.toggle_root_menu(cx));
        assert!(cx.update(|_window, app| { chat.read(app).popup.is_open(&ChatPopup::Project) }));
        cx.simulate_keystrokes("escape");
        assert_eq!(
            cx.update(|_window, app| chat.read(app).popup.open_key().copied()),
            None
        );

        // ga from composer Normal selects the newest assistant and returns
        // focus to the application proxy rather than leaving a stale chord
        // highlight behind in the editor.
        composer.update(cx, |input, cx| input.set_vim_enabled(true, cx));
        cx.update(|window, app| window.focus(&composer_focus, app));
        cx.simulate_keystrokes("g a");
        assert_eq!(
            cx.update(|_window, app| chat.read(app).selected_transcript_id().map(str::to_owned)),
            Some("a1".to_string())
        );
        assert_eq!(
            cx.update(|window, app| window.focused(app)),
            Some(application_focus)
        );
    }

    #[gpui::test]
    fn test_application_vim_region_moves_follow_live_composer_focus(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        struct ChatHost {
            chat: Entity<ChatScreen>,
        }
        impl Render for ChatHost {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut Context<Self>,
            ) -> impl IntoElement {
                div().w(px(1200.)).h(px(800.)).child(self.chat.clone())
            }
        }

        let chat = cx.new(|cx| {
            let _guard = SETTINGS_LOCK.lock();
            let backend = std::sync::Arc::new(
                crate::backend::AgentBackend::for_test(
                    "http://127.0.0.1:9".to_string(),
                    String::new(),
                )
                .expect("backend"),
            );
            crate::desktop::register_key_bindings(cx);
            let mut chat = ChatScreen::new_without_start(backend, "user".to_string(), cx);
            chat.selected_session = Some("s1".to_string());
            chat.booting = false;
            chat.application_vim_enabled = false;
            chat.composer_vim_enabled = true;
            if let Some(composer) = chat.composer.clone() {
                composer.update(cx, |input, cx| input.set_vim_enabled(true, cx));
            }
            chat.replace_timeline(vec![user_item("u1", "question")]);
            chat.set_application_vim_enabled(true, cx);
            chat
        });

        let (_host, cx) = cx.add_window_view(|_window, _cx| ChatHost { chat: chat.clone() });
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        let (composer_focus, application_focus) = cx.update(|_window, app| {
            let chat = chat.read(app);
            (
                chat.composer.clone().unwrap().focus_handle(app),
                chat.application_focus.clone().unwrap(),
            )
        });

        // Clicking the composer changes the real GPUI focus, but historically
        // left this bookmark on the sidebar. The chord must honor its focused
        // composer context rather than planning Left from the stale bookmark.
        chat.update(cx, |this, _cx| {
            this.application_vim.region = crate::ui::chat::navigation::ChatRegion::Sidebar;
            this.notice = None;
        });
        cx.update(|window, app| window.focus(&composer_focus, app));
        cx.simulate_keystrokes("ctrl-w h");
        cx.update(|window, app| {
            let chat = chat.read(app);
            assert_eq!(
                chat.application_vim.region,
                crate::ui::chat::navigation::ChatRegion::Sidebar
            );
            assert!(chat.notice.is_none());
            assert_eq!(window.focused(app), Some(application_focus.clone()));
        });

        // The same stale state must still plan Up from the focused composer
        // and land on its non-empty transcript.
        chat.update(cx, |this, _cx| {
            this.application_vim.region = crate::ui::chat::navigation::ChatRegion::Sidebar;
            this.notice = None;
        });
        cx.update(|window, app| window.focus(&composer_focus, app));
        cx.simulate_keystrokes("ctrl-w k");
        cx.update(|window, app| {
            let chat = chat.read(app);
            assert_eq!(
                chat.application_vim.region,
                crate::ui::chat::navigation::ChatRegion::Transcript
            );
            assert_eq!(chat.selected_transcript_id(), Some("u1"));
            assert!(chat.notice.is_none());
            assert_eq!(window.focused(app), Some(application_focus));
        });
    }

    #[gpui::test]
    fn test_application_vim_reconciles_stable_timeline_ids_per_task(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let chat = screen(cx);
        chat.update(cx, |this, _cx| {
            this.application_vim_enabled = true;
            this.replace_timeline(vec![
                item("a", "message", Some("a")),
                item("b", "message", Some("b")),
                item("c", "message", Some("c")),
            ]);
            assert_eq!(this.selected_transcript_id(), Some("c"));

            // Reordering keeps the stable ID. Removing it chooses the next
            // item from the old semantic order before falling back backward.
            this.replace_timeline(vec![
                item("c", "message", Some("c")),
                item("a", "message", Some("a")),
                item("b", "message", Some("b")),
            ]);
            assert_eq!(this.selected_transcript_id(), Some("c"));
            this.replace_timeline(vec![
                item("a", "message", Some("a")),
                item("b", "message", Some("b")),
            ]);
            assert_eq!(this.selected_transcript_id(), Some("a"));

            this.selected_session = Some("s2".to_string());
            this.replace_timeline(vec![
                item("x", "message", Some("x")),
                item("y", "message", Some("y")),
            ]);
            assert_eq!(this.selected_transcript_id(), Some("y"));

            this.selected_session = Some("s1".to_string());
            this.replace_timeline(vec![
                item("a", "message", Some("a")),
                item("b", "message", Some("b")),
            ]);
            assert_eq!(this.selected_transcript_id(), Some("a"));
        });
    }

    #[gpui::test]
    fn test_returning_from_settings_reclaims_chat_focus_without_a_preference_change(
        cx: &mut TestAppContext,
    ) {
        let chat = screen(cx);
        chat.update(cx, |this, cx| {
            let settings = crate::settings::AppSettings {
                application_vim_enabled: true,
                ..Default::default()
            };
            this.application_vim_enabled = true;
            this.screen_focus_pending = false;

            let _typography = crate::ui::typography::APPLY_LOCK.lock();
            this.apply_defaults(&settings, cx);

            assert!(
                this.screen_focus_pending,
                "the remounted Chat screen must not retain Settings' stale focus handle"
            );
        });
    }

    #[gpui::test]
    fn test_returning_from_settings_reclaims_standard_chat_focus(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let chat = screen(cx);
        chat.update(cx, |this, cx| {
            let settings = crate::settings::AppSettings {
                application_vim_enabled: false,
                ..Default::default()
            };
            this.application_vim_enabled = false;
            this.screen_focus_pending = false;

            let _typography = crate::ui::typography::APPLY_LOCK.lock();
            this.apply_defaults(&settings, cx);

            assert!(
                this.screen_focus_pending,
                "Standard mode intentionally returns Settings focus to the Chat composer"
            );
        });
    }

    #[gpui::test]
    fn test_persisted_application_vim_starts_an_empty_chat_on_the_sidebar(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let chat = screen(cx);
        chat.update(cx, |this, _cx| {
            this.application_vim_enabled = true;
            this.application_vim = Default::default();

            this.initialize_application_vim_surface();

            assert_eq!(
                this.application_vim.region,
                crate::ui::chat::navigation::ChatRegion::Sidebar
            );
        });
    }

    /// Ctrl-P opens the project menu, takes the focus off the composer,
    /// and walks its rows with plain arrow keys. Closing the menu gives
    /// the composer its focus back.
    #[gpui::test]
    fn test_project_menu_walks_with_arrows_and_application_vim_jk(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        struct ChatHost {
            chat: Entity<ChatScreen>,
        }
        impl Render for ChatHost {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut Context<Self>,
            ) -> impl IntoElement {
                div().w(px(1200.)).h(px(800.)).child(self.chat.clone())
            }
        }

        let chat = cx.new(|cx| {
            let _guard = SETTINGS_LOCK.lock();
            let backend = std::sync::Arc::new(
                crate::backend::AgentBackend::for_test(
                    "http://127.0.0.1:9".to_string(),
                    String::new(),
                )
                .expect("backend"),
            );
            crate::desktop::register_key_bindings(cx);
            let mut chat = ChatScreen::new_without_start(backend, "user".to_string(), cx);
            chat.selected_session = Some("s1".to_string());
            chat.recent_roots = vec![
                recent_root(&absolute_fixture_root("one")),
                recent_root(&absolute_fixture_root("two")),
            ];
            chat.booting = false;
            chat
        });

        let (_host, cx) = cx.add_window_view(|_window, _cx| ChatHost { chat: chat.clone() });
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        let composer_handle =
            cx.update(|_window, app| chat.read(app).composer.clone().unwrap().focus_handle(app));
        cx.update(|window, app| window.focus(&composer_handle, app));

        cx.simulate_keystrokes("secondary-p");
        assert!(cx.update(|_window, app| { chat.read(app).popup.is_open(&ChatPopup::Project) }));
        assert_ne!(
            cx.update(|window, app| window.focused(app)),
            Some(composer_handle.clone()),
            "the open menu must hold the focus, or the arrows type instead"
        );

        // Two recent roots and the "New project…" row: down, down, down
        // wraps back to the first.
        let highlighted = |cx: &mut gpui::VisualTestContext| {
            cx.update(|_window, app| chat.read(app).popup.highlighted().cloned())
        };
        let root = |name: &str| {
            Some(gpui::ElementId::from(SharedString::from(format!(
                "root-{}",
                absolute_fixture_root(name)
            ))))
        };
        let choose = || Some(gpui::ElementId::from("root-choose"));
        cx.simulate_keystrokes("down");
        assert_eq!(highlighted(cx), root("one"));
        cx.simulate_keystrokes("down down");
        assert_eq!(highlighted(cx), choose());
        cx.simulate_keystrokes("down");
        assert_eq!(highlighted(cx), root("one"));
        cx.simulate_keystrokes("up");
        assert_eq!(highlighted(cx), choose());

        // Enter on a recent root asks for the switch and closes the menu;
        // the composer takes the typing back.
        cx.simulate_keystrokes("up up");
        assert_eq!(highlighted(cx), root("one"));
        cx.simulate_keystrokes("enter");
        assert!(!cx.update(|_window, app| { chat.read(app).popup.is_open(&ChatPopup::Project) }));
        assert_eq!(
            cx.update(|window, app| window.focused(app)),
            Some(composer_handle),
            "closing the menu must hand the focus back"
        );

        // Application Vim adds its own context to the focused menu, so its
        // j/k aliases are live without disturbing the legacy arrow bindings.
        chat.update(cx, |this, cx| this.set_application_vim_enabled(true, cx));
        cx.simulate_keystrokes("secondary-p");
        assert!(cx.update(|_window, app| { chat.read(app).popup.is_open(&ChatPopup::Project) }));
        cx.simulate_keystrokes("j j");
        assert_eq!(highlighted(cx), root("two"));
        cx.simulate_keystrokes("k");
        assert_eq!(highlighted(cx), root("one"));
    }

    /// The open composer menu (model picker and friends) floats above the
    /// chip row as an overlay overlaying the composer contents. Regression
    /// test: an anchor beyond the chip row would push the panel down into
    /// the chips, and a menu that dropped out of the stack would hide
    /// under them.
    #[gpui::test]
    fn test_composer_menu_floats_above_the_composer(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        struct ChatHost {
            chat: Entity<ChatScreen>,
        }
        impl Render for ChatHost {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut Context<Self>,
            ) -> impl IntoElement {
                div().w(px(1200.)).h(px(800.)).child(self.chat.clone())
            }
        }

        let chat = cx.new(|cx| {
            let _guard = SETTINGS_LOCK.lock();
            let backend = std::sync::Arc::new(
                crate::backend::AgentBackend::for_test(
                    "http://127.0.0.1:9".to_string(),
                    String::new(),
                )
                .expect("backend"),
            );
            crate::desktop::register_key_bindings(cx);
            let mut chat = ChatScreen::new_without_start(backend, "user".to_string(), cx);
            chat.selected_session = Some("s1".to_string());
            chat.booting = false;
            chat.replace_timeline(vec![user_item("u1", "hello")]);
            chat.models = vec!["voxtral-small-24b".to_string()];
            chat.popup.open(ChatPopup::Model, cx);
            chat
        });

        let (_host, cx) = cx.add_window_view(|_window, _cx| ChatHost { chat: chat.clone() });
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));

        let menu = cx
            .debug_bounds("model-menu")
            .expect("the models menu renders while it is open");
        let chips = cx
            .debug_bounds("composer-chips")
            .expect("the composer chip row renders");
        assert!(
            menu.size.width > px(0.) && menu.size.height > px(0.),
            "the menu overlay must lay out with real bounds, got {menu:?}"
        );
        assert!(
            menu.bottom() <= chips.top(),
            "the composer menu must sit above the chip row \
             (menu {menu:?}, chips {chips:?})"
        );
    }

    /// Tool-card and thinking-row summaries must claim their row's width
    /// before any ellipsis: the title grows to fill the row (`flex_1` +
    /// `min_w_0`) and truncates at its end, never collapsing to its
    /// longest word while the row sits empty.
    #[gpui::test]
    fn test_tool_summary_title_fills_the_row(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        struct ChatHost {
            chat: Entity<ChatScreen>,
        }
        impl Render for ChatHost {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut Context<Self>,
            ) -> impl IntoElement {
                // Same shape as the production root: a definite-size
                // flex column, so the transcript column stretches.
                div()
                    .w(px(1200.))
                    .h(px(800.))
                    .flex()
                    .flex_col()
                    .child(self.chat.clone())
            }
        }

        let summary = "Searched the whole repository for existing middle-truncation \
                       helpers and found none of them anywhere in the tree at all";
        let chat = cx.new(|cx| {
            let _guard = SETTINGS_LOCK.lock();
            let backend = std::sync::Arc::new(
                crate::backend::AgentBackend::for_test(
                    "http://127.0.0.1:9".to_string(),
                    String::new(),
                )
                .expect("backend"),
            );
            crate::desktop::register_key_bindings(cx);
            let mut chat = ChatScreen::new_without_start(backend, "user".to_string(), cx);
            chat.selected_session = Some("s1".to_string());
            chat.booting = false;
            chat.replace_timeline(vec![
                AgentTimelineItem {
                    status: Some("completed".to_string()),
                    title: Some("Terminal: cargo test".to_string()),
                    ..item("tool-1", "tool", None)
                },
                AgentTimelineItem {
                    role: Some("thought".to_string()),
                    title: Some("Thinking".to_string()),
                    text: Some("long reasoning text".to_string()),
                    status: Some("completed".to_string()),
                    ..item("think-1", "thinking", None)
                },
            ]);
            chat.tool_summaries
                .insert("tool-1".to_string(), SharedString::from(summary));
            chat.tool_summaries
                .insert("think-1".to_string(), SharedString::from(summary));
            chat
        });

        let (_host, cx) = cx.add_window_view(|_window, _cx| ChatHost { chat: chat.clone() });
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));

        let card = cx.debug_bounds("tool-card").expect("the tool card renders");
        let tool_title = cx
            .debug_bounds("tool-title")
            .expect("the tool card title renders");
        let thinking_title = cx
            .debug_bounds("thinking-title")
            .expect("the thinking row title renders");

        assert!(
            tool_title.size.width > card.size.width * 0.75,
            "a summary too long for the row must claim the row's width \
             before truncating (title {tool_title:?}, card {card:?})"
        );
        assert!(
            thinking_title.size.width > card.size.width * 0.75,
            "a thinking summary too long for its row must claim the row's \
             width before truncating (title {thinking_title:?}, card {card:?})"
        );
    }
    #[gpui::test]
    fn long_update_failure_wraps_without_hiding_download_or_later(cx: &mut TestAppContext) {
        struct ChatHost {
            chat: Entity<ChatScreen>,
        }
        impl Render for ChatHost {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().size_full().flex().flex_col().child(self.chat.clone())
            }
        }
        cx.executor().allow_parking();
        let chat = cx.new(|cx| {
            let _guard = SETTINGS_LOCK.lock();
            let backend = Arc::new(crate::backend::AgentBackend::for_test("http://127.0.0.1:9".into(), String::new()).unwrap());
            let mut chat = ChatScreen::new_without_start(backend, "user".into(), cx);
            chat.update = crate::update::Snapshot {
                state: crate::update::State::Error,
                label: "Could not install the update: permission denied. Quit and reopen Maple, then retry. If this keeps happening, download the latest version manually.".into(),
                version: Some("0.0.6".into()),
                remote_candidate: None,
                primary: Some(crate::update::UpdateAction::ManualDownload),
                primary_label: "Download",
                manual_download_url: Some("https://updates.maple.ai/download/agent/stable/macos".into()),
                show_banner: true,
            };
            chat
        });
        let (_host, cx) = cx.add_window_view(|_, _| ChatHost { chat: chat.clone() });
        for width in [1213., 900.] {
            cx.simulate_resize(gpui::size(px(width), px(800.)));
            let banner = cx.debug_bounds("update-banner").expect("update banner");
            let label = cx
                .debug_bounds("update-banner-label")
                .expect("failure advice");
            assert!(
                label.size.height > px(25.),
                "failure advice must wrap: {label:?}"
            );
            for selector in ["update-primary", "update-later"] {
                let action = cx.debug_bounds(selector).expect("update action");
                assert!(
                    action.left() >= banner.left() && action.right() <= banner.right(),
                    "{selector}: {action:?}, {banner:?}"
                );
                assert!(
                    action.top() >= label.bottom() && action.bottom() <= banner.bottom(),
                    "{selector}: {action:?}, {label:?}"
                );
            }
        }
    }

    #[gpui::test]
    fn receipt_completion_uncertainty_never_restores_or_replays_the_original_draft(
        cx: &mut TestAppContext,
    ) {
        use crate::host::receipts::Outcome;
        let chat = screen(cx);
        chat.update(cx, |chat, cx| {
            let composer = cx.new(|cx| TextInput::new("Message", cx));
            chat.composer = Some(composer.clone());
            chat.selected_session = Some("s1".into());
            // Receipt transport is covered by host's native IPC regression.
            // Keep this presentation check deterministic without a live query.
            chat.host_recovery.unavailable = true;
            for draft in ["", "a newer draft"] {
                composer.update(cx, |input, cx| input.set_text(draft, cx));
                let id = uuid::Uuid::new_v4().to_string();
                chat.finish_send(id.clone(), UnresolvedSend {
                    outcome: None,
                    session: "s1".into(),
                    text: "already admitted original message".into(),
                    from_composer: true,
                    images: vec![DraftImage {
                        id: 50,
                        name: "image.png".into(),
                        data_url: Some(Arc::from("data:image/png;base64,cGljdHVyZQ==")),
                        thumbnail: None,
                    }],
                }, Err(format!("Send outcome unknown ({id}). Reload this task before sending again; it may already be running.")), cx);
                assert_eq!(composer.read(cx).text_ref(), draft);
                assert!(chat.draft_images.is_empty());
                assert_eq!(chat.unresolved_sends[&id].images.len(), 1);
                assert!(chat.unresolved_sends[&id].awaiting_receipt());
                chat.apply_send_receipt(&id, Some(Outcome::Pending), cx);
                assert_eq!(composer.read(cx).text_ref(), draft);
                assert!(chat.draft_images.is_empty());
                assert!(chat.unresolved_sends[&id].awaiting_receipt());
                chat.apply_send_receipt(&id, Some(Outcome::Finished(Ok(serde_json::json!("accepted-run")))), cx);
                assert_eq!(composer.read(cx).text_ref(), draft);
                assert!(chat.draft_images.is_empty());
                assert!(!chat.unresolved_sends.contains_key(&id));
            }
        });
    }

    #[gpui::test]
    fn uncertain_send_keeps_images_and_does_not_overwrite_another_draft(cx: &mut TestAppContext) {
        use crate::host::receipts::Outcome;
        let chat = screen(cx);
        chat.update(cx, |chat, cx| {
            let composer = cx.new(|cx| TextInput::new("Message", cx));
            composer.update(cx, |input, cx| input.set_text("new draft", cx));
            chat.composer = Some(composer.clone());
            chat.unresolved_sends.insert(
                "send-original".into(),
                UnresolvedSend {
                    outcome: None,
                    session: "s1".into(),
                    text: "original message".into(),
                    from_composer: true,
                    images: vec![DraftImage {
                        id: 50,
                        name: "image.png".into(),
                        data_url: Some(Arc::from("data:image/png;base64,cGljdHVyZQ==")),
                        thumbnail: None,
                    }],
                },
            );
            chat.apply_send_receipt("send-original", None, cx);
            assert_eq!(chat.unresolved_sends["send-original"].images.len(), 1);
            chat.apply_send_receipt("send-original", Some(Outcome::Missing), cx);
            assert_eq!(composer.read(cx).text_ref(), "new draft");
            assert!(chat.unresolved_sends.contains_key("send-original"));
            composer.update(cx, |input, cx| input.clear(cx));
            chat.selected_session = Some("another-task".into());
            chat.apply_send_receipt("send-original", Some(Outcome::Missing), cx);
            assert!(composer.read(cx).text_ref().is_empty());
            chat.selected_session = Some("s1".into());
            chat.apply_send_receipt("send-original", Some(Outcome::Missing), cx);
            assert_eq!(composer.read(cx).text_ref(), "original message");
            assert_eq!(chat.draft_images[0].name, "image.png");
            assert!(chat.unresolved_sends.is_empty());
        });
    }
    #[gpui::test]
    fn failed_and_interrupted_sends_preserve_newer_text_and_release_the_send_gate(
        cx: &mut TestAppContext,
    ) {
        use crate::host::receipts::Outcome;
        let chat = screen(cx);
        chat.update(cx, |chat, cx| {
            let composer = cx.new(|cx| TextInput::new("Message", cx));
            chat.composer = Some(composer.clone());
            chat.selected_session = Some("s1".into());
            for outcome in [
                Outcome::Interrupted,
                Outcome::Finished(Err("Not admitted".into())),
            ] {
                composer.update(cx, |input, cx| input.set_text("newer draft", cx));
                chat.unresolved_sends.insert(
                    "original".into(),
                    UnresolvedSend {
                        outcome: None,
                        session: "s1".into(),
                        text: "original draft".into(),
                        images: vec![],
                        from_composer: true,
                    },
                );
                assert!(chat.unresolved_sends["original"].awaiting_receipt());
                chat.apply_send_receipt("original", Some(outcome.clone()), cx);
                assert_eq!(composer.read(cx).text_ref(), "newer draft");
                assert!(
                    !chat.unresolved_sends["original"].awaiting_receipt(),
                    "a settled receipt cannot block the next send"
                );
                if outcome == Outcome::Interrupted {
                    assert!(
                        chat.notice
                            .as_ref()
                            .unwrap()
                            .contains("may have taken effect")
                    );
                }
                composer.update(cx, |input, cx| input.clear(cx));
                chat.apply_send_receipt("original", Some(outcome), cx);
                assert_eq!(composer.read(cx).text_ref(), "original draft");
                assert!(chat.unresolved_sends.is_empty());
            }
        });
    }
    mod remote_draft_navigation_tests {
        use super::*;

        /// This also exercises Settings' first "Open projects" after pairing.
        #[gpui::test]
        fn opening_a_computer_catalog_does_not_open_its_latest_task(cx: &mut TestAppContext) {
            let chat = screen(cx);
            chat.update(cx, |this, cx| {
                this.selected_session = None;
                this.project_root = Some(absolute_fixture_root("remote"));
                this.navigation_only = true;
                this.navigation_ready = false;
                this.host_recovery.events = Some(Vec::new());
                this.navigate(Destination::Overview, cx);
                assert!(this.pending_navigation.is_some());
                this.host_recovery.events = None;
                this.finish_navigation_bootstrap(cx);

                assert!(!this.navigation_only);
                assert!(this.draft);
                this.apply_session_list(
                    vec![summary_at(
                        "existing",
                        "Existing remote task",
                        this.project_root.as_deref().unwrap(),
                    )],
                    this.selection_generation,
                    cx,
                );
                assert!(this.selected_session.is_none());
                assert!(this.loading_session.is_none());
                assert!(this.timeline.is_empty());
                assert_eq!(
                    this.sessions.len(),
                    1,
                    "the sidebar still receives the task"
                );
            });
        }

        /// A New Task selector click carries its meaning to a previously
        /// visited computer, even if that computer showed an existing task.
        #[gpui::test]
        fn draft_project_navigation_supersedes_a_cached_task(cx: &mut TestAppContext) {
            let chat = screen(cx);
            chat.update(cx, |this, cx| {
                this.navigation_ready = true;
                this.project_root = Some(absolute_fixture_root("old"));
                this.navigate(
                    Destination::DraftProject(absolute_fixture_root("target")),
                    cx,
                );
                assert!(this.draft);
                assert!(this.selected_session.is_none());
                assert!(this.timeline.is_empty());

                // Runtime lists can land before and after project registration.
                this.apply_session_list(
                    vec![summary_at(
                        "old-task",
                        "Old task",
                        &absolute_fixture_root("old"),
                    )],
                    this.selection_generation,
                    cx,
                );
                assert!(this.loading_session.is_none());
                let generation = this.selection_generation;
                this.root_selecting = false;
                this.land_project_selection(absolute_fixture_root("target"), generation, cx);
                this.apply_session_list(
                    vec![summary_at(
                        "target-task",
                        "Target task",
                        &absolute_fixture_root("target"),
                    )],
                    this.selection_generation,
                    cx,
                );
                assert!(this.draft);
                assert!(this.selected_session.is_none());
                assert!(this.loading_session.is_none());
                assert_eq!(
                    this.project_root.as_deref(),
                    Some(absolute_fixture_root("target").as_str())
                );
            });
        }

        #[gpui::test]
        fn draft_project_intent_survives_a_pending_catalog(cx: &mut TestAppContext) {
            let chat = screen(cx);
            chat.update(cx, |this, cx| {
                this.navigation_only = true;
                this.navigation_ready = false;
                this.selected_session = None;
                this.host_recovery.events = Some(Vec::new());
                let destination = Destination::DraftProject(absolute_fixture_root("target"));
                this.navigate(destination.clone(), cx);
                assert_eq!(
                    this.pending_navigation.as_ref().map(|(_, target)| target),
                    Some(&destination)
                );
                this.host_recovery.events = None;
                this.finish_navigation_bootstrap(cx);
                assert!(this.pending_navigation.is_none());
                assert!(this.draft);
                assert!(this.root_selecting);
                assert!(this.selected_session.is_none());
            });
        }

        /// Explicit sidebar task selection still loads the requested task.
        #[gpui::test]
        fn opening_a_catalog_task_still_selects_that_task(cx: &mut TestAppContext) {
            let chat = screen(cx);
            chat.update(cx, |this, cx| {
                this.navigation_only = true;
                this.navigation_ready = true;
                this.selected_session = None;
                this.navigate(Destination::Task("chosen".into()), cx);
                assert!(!this.navigation_only);
                assert!(!this.draft);
                assert_eq!(this.loading_session.as_deref(), Some("chosen"));
            });
        }

        /// Exercise the actual menu actions. Both remote targets and the
        /// new-folder action carry draft intent; ordinary browsing stays ordinary.
        #[gpui::test]
        fn project_menu_carries_draft_intent_between_computers(cx: &mut TestAppContext) {
            use crate::ui::chat::computers::Project;
            let requests = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let (chat, cx) = chat_window(cx, |this, cx| {
                this.selected_session = None;
                this.draft = true;
                let computers = [None, Some("remote-one"), Some("remote-two")]
                    .into_iter()
                    .map(|key| Computer {
                        key: key.map(str::to_owned),
                        label: key.unwrap_or("Local").into(),
                        backend: None,
                        host_epoch: 0,
                        project_names: HashMap::new(),
                        projects: vec![Project {
                            path: absolute_fixture_root("project"),
                            label: "Project".into(),
                        }],
                        sessions: Vec::new(),
                        running: HashSet::new(),
                        unread: HashSet::new(),
                        pinned: Vec::new(),
                        loading: false,
                        error: None,
                    })
                    .collect();
                this.set_computers(Arc::new(computers), cx);
            });
            cx.update(|_, cx| {
                let requests = requests.clone();
                cx.subscribe(&chat, move |_, request: &Navigate, _| {
                    requests.borrow_mut().push(request.clone());
                })
                .detach();
            });
            for (row, computer, destination) in [
                (
                    "computer-1-project-0",
                    "remote-one",
                    Destination::DraftProject(absolute_fixture_root("project")),
                ),
                (
                    "computer-2-project-0",
                    "remote-two",
                    Destination::DraftProject(absolute_fixture_root("project")),
                ),
                (
                    "computer-2-new-project",
                    "remote-two",
                    Destination::DraftNewProject,
                ),
            ] {
                press(cx, "root-picker");
                press(cx, row);
                assert_eq!(
                    requests.borrow_mut().pop(),
                    Some(Navigate {
                        computer: Some(computer.into()),
                        destination
                    })
                );
            }
            chat.update(cx, |this, _| {
                this.draft = false;
                this.selected_session = Some("existing".into());
            });
            press(cx, "root-picker");
            press(cx, "computer-1-project-0");
            assert_eq!(
                requests.borrow_mut().pop(),
                Some(Navigate {
                    computer: Some("remote-one".into()),
                    destination: Destination::Project(absolute_fixture_root("project")),
                })
            );
        }
    }

    mod send_feedback_tests {
        use super::*;

        #[gpui::test]
        fn first_send_immediately_leaves_the_hero_without_inventing_history(
            cx: &mut TestAppContext,
        ) {
            let (chat, cx) = chat_window(cx, |this, cx| {
                this.selected_session = None;
                this.project_root = Some(absolute_fixture_root("draft"));
                this.new_session(cx);
            });
            chat.update(cx, |this, cx| {
                let composer = this.composer.clone().unwrap();
                composer.update(cx, |input, cx| input.set_text("first message", cx));
                assert!(this.shows_empty_state());
                this.send_inner(cx);
                assert!(
                    !this.shows_empty_state(),
                    "the click leaves the hero immediately"
                );
                assert!(this.send_pending());
                assert!(this.timeline.is_empty(), "only the host writes history");
                assert!(
                    this.active_runs.is_empty(),
                    "a preview is not an admitted run"
                );
                assert_eq!(this.composer_text(cx).as_deref(), Some("first message"));
                this.fail_new_session("creation failed".into(), cx);
                assert!(this.shows_empty_state());
                assert!(!this.send_pending());
                assert_eq!(this.composer_text(cx).as_deref(), Some("first message"));
            });
        }

        #[gpui::test]
        fn existing_send_feedback_is_immediate_and_blocks_only_duplicate_admission(
            cx: &mut TestAppContext,
        ) {
            let (chat, cx) = chat_window(cx, |this, _| {
                this.selected_session = Some("s1".into());
                this.replace_timeline(vec![user_item("old", "earlier")]);
            });
            chat.update(cx, |this, cx| {
                let composer = this.composer.clone().unwrap();
                composer.update(cx, |input, cx| input.set_text("next message", cx));
                this.send_inner(cx);
                assert!(this.send_pending());
                assert_eq!(this.sending_previews.len(), 1);
                assert!(this.composer_text(cx).unwrap().is_empty());
                assert_eq!(this.timeline.len(), 1);
                composer.update(cx, |input, cx| input.set_text("a newer draft", cx));
                this.send_inner(cx);
                assert_eq!(this.sending_previews.len(), 1, "no duplicate request");
                assert_eq!(this.composer_text(cx).as_deref(), Some("a newer draft"));
                this.selected_session = Some("other".into());
                assert!(!this.send_pending(), "feedback is scoped to its task");
            });
        }

        fn pending(this: &mut ChatScreen, id: &str) {
            this.sending_previews.insert(
                id.into(),
                sending::SendPreview::new(Some("s1".into()), "new message", &[])
                    .capture_timeline(&this.timeline),
            );
        }

        #[gpui::test]
        fn resolved_skill_waiting_for_admission_preserves_the_command_and_newer_draft(
            cx: &mut TestAppContext,
        ) {
            let (chat, cx) = chat_window(cx, |this, _| {
                this.selected_session = Some("s1".into());
                this.replace_timeline(vec![user_item("old", "earlier")]);
                pending(this, "first-send");
            });
            chat.update(cx, |this, cx| {
                let composer = this.composer.clone().unwrap();
                // Resolving a skill is asynchronous, and its invocation has
                // already been cleared by the time its prompt arrives here.
                for (selected, draft, expected) in [
                    ("s1", "", "/review changes"),
                    ("s1", "a newer draft", "a newer draft"),
                    ("other", "", ""),
                ] {
                    this.selected_session = Some(selected.into());
                    composer.update(cx, |input, cx| input.set_text(draft, cx));
                    this.send_resolved_skill(
                        "s1",
                        "/review changes",
                        "expanded review instructions".into(),
                        cx,
                    );
                    assert_eq!(this.composer_text(cx).as_deref(), Some(expected));
                    assert_eq!(
                        this.prompt_history.last().map(String::as_str),
                        Some("/review changes")
                    );
                    assert_eq!(
                        this.sending_previews.len(),
                        1,
                        "no second admission or retry"
                    );
                    assert!(this.sending_previews.contains_key("first-send"));
                    assert_eq!(this.timeline.len(), 1, "no invented history");
                    assert!(
                        this.notice
                            .as_deref()
                            .unwrap()
                            .starts_with("Skill not sent")
                    );
                }
            });
        }

        #[gpui::test]
        fn acknowledgement_waits_for_its_run_projection(cx: &mut TestAppContext) {
            use maple_agent::agent::AgentRunEvent;
            let chat = screen(cx);
            chat.update(cx, |this, cx| {
                this.replace_timeline(vec![user_item("old", "previous")]);
                pending(this, "send");
                this.acknowledge_send_preview("send", "our-run");
                assert!(
                    !this.send_pending(),
                    "admission no longer blocks next draft"
                );
                assert!(this.sending_previews["send"].body_visible());
                this.handle_run_event(
                    "s1",
                    "other-run",
                    AgentRunEvent::TimelineItem(user_item("other", "another controller")),
                    cx,
                );
                assert!(
                    this.sending_previews["send"].body_visible(),
                    "unrelated run cannot retire our body"
                );
                this.handle_run_event(
                    "s1",
                    "our-run",
                    AgentRunEvent::TimelineItem(user_item("old", "previous")),
                    cx,
                );
                assert!(
                    this.sending_previews["send"].body_visible(),
                    "replayed old row is not our projection"
                );
                this.handle_run_event(
                    "s1",
                    "our-run",
                    AgentRunEvent::TimelineItem(user_item("sent", "new message")),
                    cx,
                );
                assert!(this.sending_previews.is_empty());
                assert_eq!(
                    this.timeline
                        .iter()
                        .filter(|item| item.id == "sent")
                        .count(),
                    1
                );
            });
        }

        #[gpui::test]
        fn projection_before_acknowledgement_yields_to_the_canonical_body(cx: &mut TestAppContext) {
            let chat = screen(cx);
            chat.update(cx, |this, cx| {
                pending(this, "send");
                this.handle_service_event(
                    AgentServiceEvent::TimelineItem {
                        session_id: "s1".into(),
                        run_id: Some("run".into()),
                        item: user_item("sent", "new message"),
                    },
                    cx,
                );
                assert!(
                    this.send_pending(),
                    "event does not pretend to acknowledge admission"
                );
                assert!(
                    !this.sending_previews["send"].body_visible(),
                    "one visible user bubble"
                );
                this.acknowledge_send_preview("send", "run");
                assert!(this.sending_previews.is_empty());
                assert_eq!(this.timeline.len(), 1);
            });
        }

        #[gpui::test]
        fn snapshots_and_terminal_events_retire_presentation_without_replaying(
            cx: &mut TestAppContext,
        ) {
            use maple_agent::agent::{AgentRunEvent, AgentRunTerminal};
            let chat = screen(cx);
            chat.update(cx, |this, cx| {
                this.replace_timeline(vec![user_item("old", "previous")]);
                pending(this, "send");
                this.acknowledge_send_preview("send", "run");
                this.replace_timeline(vec![user_item("old", "previous")]);
                assert!(this.sending_previews["send"].body_visible());
                this.replace_timeline(vec![
                    user_item("old", "previous"),
                    user_item("new", "new message"),
                ]);
                assert!(this.sending_previews.is_empty());

                pending(this, "failed-run");
                this.acknowledge_send_preview("failed-run", "terminal");
                this.handle_run_event(
                    "s1",
                    "terminal",
                    AgentRunEvent::Finished(AgentRunTerminal::Completed),
                    cx,
                );
                assert!(
                    this.sending_previews.is_empty(),
                    "terminal without user row cannot strand feedback"
                );
                pending(this, "fast-run");
                this.handle_run_event(
                    "s1",
                    "fast",
                    AgentRunEvent::Finished(AgentRunTerminal::Completed),
                    cx,
                );
                this.acknowledge_send_preview("fast-run", "fast");
                assert!(
                    this.sending_previews.is_empty(),
                    "terminal may beat admission reply"
                );
            });
        }

        #[gpui::test]
        fn queued_or_steering_admission_needs_no_new_user_row(cx: &mut TestAppContext) {
            let chat = screen(cx);
            chat.update(cx, |this, _| {
                this.sending_previews.insert(
                    "queued".into(),
                    sending::SendPreview::new(Some("s1".into()), "queue me", &[]).status_only(),
                );
                assert!(this.send_pending());
                assert!(!this.sending_previews["queued"].body_visible());
                this.acknowledge_send_preview("queued", "existing-run");
                assert!(!this.send_pending());
                assert!(!this.has_send_feedback());
            });
        }

        #[gpui::test]
        fn late_failure_keeps_the_new_draft_and_belongs_to_its_original_task(
            cx: &mut TestAppContext,
        ) {
            let (chat, cx) = chat_window(cx, |_, _| {});
            chat.update(cx, |this, cx| {
                pending(this, "send");
                this.selected_session = Some("other".into());
                this.replace_timeline(vec![user_item("other-message", "another task")]);
                this.awaiting_first_token = true;
                this.composer
                    .clone()
                    .unwrap()
                    .update(cx, |input, cx| input.set_text("newer draft", cx));
                this.draft_images = vec![DraftImage {
                    id: 2,
                    name: "new.png".into(),
                    data_url: None,
                    thumbnail: None,
                }];
                this.finish_send(
                    "send".into(),
                    UnresolvedSend {
                        outcome: None,
                        session: "s1".into(),
                        text: "original".into(),
                        images: vec![DraftImage {
                            id: 1,
                            name: "original.png".into(),
                            data_url: None,
                            thumbnail: None,
                        }],
                        from_composer: true,
                    },
                    Err("failed".into()),
                    cx,
                );
                assert!(this.sending_previews.is_empty());
                assert_eq!(this.timeline.len(), 1, "no failure row in another task");
                assert!(
                    this.awaiting_first_token,
                    "another task's waiting state stays intact"
                );
                assert_eq!(this.composer_text(cx).as_deref(), Some("newer draft"));
                assert_eq!(this.draft_images[0].name, "new.png");
                assert_eq!(this.unresolved_sends["send"].text, "original");
                assert_eq!(this.unresolved_sends["send"].images[0].name, "original.png");
                // Returning to an empty original task recovers the retained draft.
                this.selected_session = Some("s1".into());
                this.composer
                    .clone()
                    .unwrap()
                    .update(cx, |input, cx| input.clear(cx));
                this.draft_images.clear();
                this.apply_send_receipt(
                    "send",
                    Some(crate::host::receipts::Outcome::Finished(Err(
                        "failed".into()
                    ))),
                    cx,
                );
                assert_eq!(this.composer_text(cx).as_deref(), Some("original"));
                assert_eq!(this.draft_images[0].name, "original.png");
            });
        }

        #[gpui::test]
        fn send_button_preserves_first_draft_when_task_creation_fails(cx: &mut TestAppContext) {
            let (chat, cx) = chat_window(cx, |this, cx| {
                this.selected_session = None;
                this.project_root = Some(absolute_fixture_root("draft"));
                this.new_session(cx);
            });
            chat.update(cx, |this, cx| {
                this.composer
                    .clone()
                    .unwrap()
                    .update(cx, |input, cx| input.set_text("first message", cx));
                cx.notify();
            });
            press(cx, "send-message");
            // The isolated backend deliberately has no running agent. Wait
            // for that exact outcome rather than racing its fast callback.
            wait_until(&chat, cx, "first create rejection", |this| {
                this.notice.as_deref() == Some("Agent runtime is not running")
            });
            chat.update(cx, |this, cx| {
                assert!(this.pending_first_send.is_none());
                assert!(!this.send_pending());
                assert!(this.shows_empty_state());
                assert_eq!(
                    this.composer_text(cx).as_deref(),
                    Some("first message"),
                    "button must not double-clear or lose the deferred first message"
                );
                assert!(
                    this.prompt_history
                        .iter()
                        .any(|prompt| prompt == "first message")
                );
            });
        }

        #[gpui::test]
        fn thinking_icon_frame_is_stable_across_loading_and_model_change(cx: &mut TestAppContext) {
            let (chat, cx) = chat_window(cx, |this, _| {
                this.thinking = None;
                this.thinking_loading = true;
            });
            let initial = cx.debug_bounds("thinking-picker").expect("loading icon");
            chat.update(cx, |this, cx| {
                this.thinking_loading = false;
                this.thinking = Some(AgentThinkingState {
                    levels: vec![ThinkingLevel::Low, ThinkingLevel::High],
                    current: ThinkingLevel::High,
                });
                cx.notify();
            });
            cx.run_until_parked();
            assert_eq!(cx.debug_bounds("thinking-picker"), Some(initial));
            chat.update(cx, |this, cx| {
                this.selected_model = Some("different-model".into());
                this.thinking = None;
                this.thinking_loading = true;
                cx.notify();
            });
            cx.run_until_parked();
            let changed = cx
                .debug_bounds("thinking-picker")
                .expect("icon after model change");
            assert_eq!(changed.size, initial.size);
            assert_eq!(changed.origin.y, initial.origin.y);
        }

        #[gpui::test]
        fn pending_first_send_renders_at_the_conversation_position(cx: &mut TestAppContext) {
            let (chat, cx) = chat_window(cx, |this, cx| {
                this.selected_session = None;
                this.draft = true;
                this.session_setup_pending = true;
                this.pending_first_send = Some(FirstSend::Message {
                    text: "hello".into(),
                    steer: false,
                });
                this.first_send_preview =
                    Some(sending::SendPreview::new(None, "hello", &[]).status_only());
                let composer = cx.new(|cx| TextInput::new("Message", cx));
                composer.update(cx, |input, cx| input.set_text("hello", cx));
                this.composer = Some(composer);
            });
            cx.run_until_parked();
            assert!(
                cx.debug_bounds("pending-send-body").is_none(),
                "the visible editable draft is not duplicated"
            );
            let input_focus = chat.update(cx, |this, cx| {
                this.composer.as_ref().unwrap().read(cx).focus_handle(cx)
            });
            cx.update(|window, app| window.focus(&input_focus, app));
            cx.simulate_keystrokes("end x");
            assert_eq!(
                chat.update(cx, |this, cx| this.composer_text(cx)),
                Some("hellox".into()),
                "focused composer remains mounted while creating"
            );
            let preview = cx.debug_bounds("pending-send").expect("immediate preview");
            let composer = cx.debug_bounds("composer-box").expect("composer");
            assert!(preview.bottom() <= composer.top());
            assert!(
                composer.top() > px(500.),
                "composer stays at bottom, not hero center"
            );
        }
    }
}
