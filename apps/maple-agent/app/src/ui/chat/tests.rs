//! Tests for the chat screen.

mod state_tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use crate::ui::chat::cache::{INLINE_PARSE_LIMIT, MAX_DIFF_LINES, ORDINAL_SPACING};
    use crate::ui::chat::composer::SideThreadTurn;
    use crate::ui::chat::images::{MAX_DRAFT_IMAGES, encode_data_url};
    use crate::ui::chat::sidebar::RenameTarget;
    use crate::ui::chat::transcript::{diff_lines_for, maple_display_text, tool_label_title};
    use crate::ui::chat::*;
    use gpui::TestAppContext;

    fn summary(id: &str, title: &str) -> AgentSessionSummary {
        summary_at(id, title, "/tmp/proj")
    }

    fn summary_at(id: &str, title: &str, project_root: &str) -> AgentSessionSummary {
        AgentSessionSummary {
            web_enabled: true,
            archived: false,
            acp: false,
            id: id.to_string(),
            title: title.to_string(),
            project_root: project_root.to_string(),
            created_ms: 0,
            updated_ms: 0,
            message_count: 1,
            model: None,
            mode: "smart_approve".to_string(),
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

    fn item(id: &str, item_type: &str, text: Option<&str>) -> AgentTimelineItem {
        AgentTimelineItem {
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

    /// Serializes constructions that read the settings file: the
    /// persisted-defaults test swaps XDG_CONFIG_HOME process-wide, so no
    /// other test may read settings while the swap is live.
    static SETTINGS_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    fn screen(cx: &mut TestAppContext) -> Entity<ChatScreen> {
        let _guard = SETTINGS_LOCK.lock();
        let backend = std::sync::Arc::new(
            crate::backend::AgentBackend::new("http://127.0.0.1:9".to_string(), String::new())
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
        // Tests that settle or pin persist their fixture ids into the real
        // settings file, which this fixture just loaded; sections must not
        // depend on what an earlier test left there.
        screen.update(cx, |this, cx| {
            this.sidebar
                .update(cx, |sidebar, _| sidebar.reset_persisted_for_test());
        });
        screen
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
            assert!(this.render_subagents_card().is_none());
            this.handle_run_event(
                "s1",
                "r1",
                AgentRunEvent::SubagentStarted {
                    id: "d1".to_string(),
                    task: "Review the parser".to_string(),
                    background: false,
                },
                cx,
            );
            assert_eq!(this.subagents.len(), 1);
            assert!(this.render_subagents_card().is_some());

            // A subagent of a task that is not on screen stays off it.
            this.handle_run_event(
                "s2",
                "r2",
                AgentRunEvent::SubagentStarted {
                    id: "d2".to_string(),
                    task: "Other task".to_string(),
                    background: true,
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
            assert!(this.render_subagents_card().is_none());
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
            assert!(this.render_subagents_card().is_some());

            // Opening the task later rebuilds the card from the runtime.
            this.set_subagents(
                vec![AgentSubagent {
                    id: "in-background".to_string(),
                    task: "Build the release".to_string(),
                    background: true,
                    elapsed_ms: 90_000,
                    activity: Some("Terminal: cargo build".to_string()),
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
            assert!(this.render_subagents_card().is_none());
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
        let dir = std::env::temp_dir().join(format!("maple-gpui-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let config = dir.join("maple-gpui");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("settings.json"),
            r#"{"tool_details":false,"default_web_enabled":false}"#,
        )
        .unwrap();
        let previous = std::env::var_os("XDG_CONFIG_HOME");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
        // This gpui's test scheduler flags activity on other threads unless
        // parking is allowed; the backend runtime and image encoder run on tokio.
        cx.executor().allow_parking();
        let screen = cx.new(|cx| {
            ChatScreen::new_inner(
                std::sync::Arc::new(
                    crate::backend::AgentBackend::new(
                        "http://127.0.0.1:9".to_string(),
                        String::new(),
                    )
                    .expect("backend"),
                ),
                "user".to_string(),
                cx,
            )
        });
        match previous {
            Some(value) => unsafe { std::env::set_var("XDG_CONFIG_HOME", value) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        let _ = std::fs::remove_dir_all(&dir);
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
            this.sessions[1].archived = true;
            this.sync_sidebar(cx);
            assert_eq!(this.sidebar_active(cx), vec![0]);
            assert_eq!(this.archived_indices(cx), vec![1]);

            this.sessions[0].archived = true;
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
    fn test_decided_permission_row_clears_the_card(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.selected_session = Some("s1".to_string());
            this.pending_permissions.push(PendingPermission {
                session_id: "s1".to_string(),
                run_id: "r1".to_string(),
                request_id: "req-1".to_string(),
                tool_name: "shell".to_string(),
                prompt: None,
                arguments: "".into(),
            });
            this.permission_responding = true;
            // A row for another request must not clear the card.
            let mut other = item("permission-req-2", "permission", None);
            other.status = Some("allow_once".to_string());
            this.handle_service_event(
                AgentServiceEvent::TimelineItem {
                    session_id: "s1".to_string(),
                    run_id: None,
                    item: other,
                },
                cx,
            );
            assert!(this.current_permission().is_some());
            // The runtime approved the request (Allow all) and replaced
            // its row with a decision.
            let mut decided = item("permission-req-1", "permission", None);
            decided.status = Some("allow_once".to_string());
            this.handle_service_event(
                AgentServiceEvent::TimelineItem {
                    session_id: "s1".to_string(),
                    run_id: None,
                    item: decided,
                },
                cx,
            );
            assert!(this.current_permission().is_none());
            assert!(!this.permission_responding);
        });
    }

    #[gpui::test]
    fn test_permission_for_other_session_waits_until_selected(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.selected_session = Some("s1".to_string());
            let request = maple_agent::agent::AgentPermissionRequest {
                request_id: "req-2".to_string(),
                tool_name: "shell".to_string(),
                arguments: serde_json::Map::new(),
                prompt: None,
            };
            this.handle_service_event(
                AgentServiceEvent::Run {
                    session_id: "s2".to_string(),
                    run_id: "r2".to_string(),
                    event: maple_agent::agent::AgentRunEvent::PermissionRequested {
                        request,
                        item: item("permission-req-2", "permission", None),
                    },
                },
                cx,
            );
            // Not shown for the session on screen.
            assert!(this.current_permission().is_none());
            // Shown once that session is opened.
            this.selected_session = Some("s2".to_string());
            assert_eq!(
                this.current_permission().map(|p| p.request_id.as_str()),
                Some("req-2")
            );
            // Gone when its run ends.
            this.handle_service_event(
                AgentServiceEvent::Run {
                    session_id: "s2".to_string(),
                    run_id: "r2".to_string(),
                    event: maple_agent::agent::AgentRunEvent::Finished(
                        maple_agent::agent::AgentRunTerminal::Cancelled,
                    ),
                },
                cx,
            );
            assert!(this.current_permission().is_none());
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
                id: id.to_string(),
                header: "Question".to_string(),
                question: text.to_string(),
                options: Vec::new(),
            }],
        }
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
    fn test_question_typed_text_rides_along_with_picked_option(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        let event = AgentServiceEvent::Question {
            session_id: "s1".to_string(),
            request_id: "q3".to_string(),
            questions: vec![maple_agent::agent::AgentQuestion {
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
            assert_eq!(this.question_selected.get(&0), Some(&0));
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
            c.archived = true;
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

    /// Escape answers a showing prompt before it reaches the running
    /// task: a permission card is denied, not left behind by a stop.
    #[gpui::test]
    fn test_escape_denies_a_showing_permission(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.pending_permissions.push(PendingPermission {
                session_id: "s1".to_string(),
                run_id: "r1".to_string(),
                request_id: "req-1".to_string(),
                tool_name: "shell".to_string(),
                prompt: None,
                arguments: "".into(),
            });
            this.escape(cx);
            assert!(this.permission_responding);
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
            this.models_menu_open = true;
            this.mode_menu_open = true;
            this.mcp_menu_open = true;
            this.root_menu_open = true;
            this.send_text("hello".to_string(), cx);
            assert!(!this.models_menu_open);
            assert!(!this.mode_menu_open);
            assert!(!this.mcp_menu_open);
            assert!(!this.root_menu_open);
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
                    id: "first".to_string(),
                    header: "One".to_string(),
                    question: "First?".to_string(),
                    options: vec![maple_agent::agent::AgentQuestionOption {
                        label: "Yes".to_string(),
                        description: String::new(),
                    }],
                },
                maple_agent::agent::AgentQuestion {
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
                crate::backend::AgentBackend::new("http://127.0.0.1:9".to_string(), String::new())
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
            assert!(this.try_command("s1", "/web", cx));
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
            assert!(this.try_command("s1", "/web", cx));
            assert!(!this.web_enabled);
            assert!(this.try_command("s1", "/model", cx));
            assert!(this.models_menu_open);
            // Unknown commands fall through to a normal send.
            assert!(!this.try_command("s1", "/definitely-not-a-command", cx));
            // Paths that merely start with a slash are not commands.
            assert!(!this.try_command("s1", "/etc/hosts is a path", cx));
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
            assert!(this.try_command("s1", "/deploy staging", cx));
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
            let index = this.apply_timeline_item("s1", long_tool("tool-1", "running"));
            this.maybe_summarize_tool(index, cx);
            assert!(this.summary_requests.is_empty());
            // Completed tools with long output are queued once.
            let index = this.apply_timeline_item("s1", long_tool("tool-1", "completed"));
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
            let index = this.apply_timeline_item("s1", short);
            this.maybe_summarize_tool(index, cx);
            assert!(this.summary_requests.contains("tool-2"));
            let mut empty = item("tool-3", "tool", None);
            empty.status = Some("completed".to_string());
            empty.input = Some(serde_json::json!({"q": 1}));
            empty.output = Some(serde_json::json!({"stdout": "  "}));
            let index = this.apply_timeline_item("s1", empty);
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
            let index =
                this.apply_timeline_item("s1", item("th-1", "thinking", Some(&long_thought)));
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
                let index =
                    this.apply_timeline_item("s1", long_tool(&format!("t{n}"), "completed"));
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
            this.handle_service_event(one_question("a", "First?"), cx);
            this.select_question_option(0, 0, cx);
            this.handle_service_event(one_question("b", "Second?"), cx);
            assert_eq!(this.question_selected.get(&0), Some(&0));
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

    #[gpui::test]
    fn test_new_task_waits_for_a_project_selection(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.project_root = Some("/work/alpha".to_string());
            this.root_selecting = true;
            this.new_session(cx);
            assert!(!this.session_setup_pending);
            assert!(this.notice.is_some());
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
            this.root_menu_open = true;
            assert!(this.begin_root_picker(cx));
            assert!(this.root_picker_open);
            assert!(!this.root_menu_open);
            // A second click while the picker is open must not start another.
            this.root_menu_open = true;
            assert!(!this.begin_root_picker(cx));
            assert!(this.root_menu_open);
        });
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

    #[gpui::test]
    fn test_escape_closes_root_menu(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.root_menu_open = true;
            this.close_menus_on_escape(cx);
            assert!(!this.root_menu_open);
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
                mode: None,
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
                0
            );
            assert_eq!(
                this.apply_timeline_item("s1", item("b", "message", Some("2"))),
                1
            );
            assert_eq!(
                this.apply_timeline_item("s1", item("a", "message", Some("3"))),
                0
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
    fn test_maple_display_text_rebrands_compaction() {
        assert_eq!(
            maple_display_text("goose is compacting the conversation..."),
            "Compacting…"
        );
        assert_eq!(
            maple_display_text("Context limit reached. Compacting to continue conversation..."),
            "Context limit reached — compacting to continue…"
        );
        assert_eq!(maple_display_text("Anything else"), "Anything else");
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
            this.recent_roots = vec!["/z".to_string()];
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
                sidebar.set_settled_for_test(HashSet::from(["s1".to_string()]));
            });
            this.sync_sidebar(cx);
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

            // The first message moves it into the inbox.
            this.sessions[0].message_count = 1;
            this.sync_sidebar(cx);
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
            this.sidebar.update(cx, |sidebar, _| {
                sidebar.set_settled_for_test(HashSet::from(["s1".to_string()]));
            });
            this.sync_sidebar(cx);
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

    /// A manual settle parks a task outside the active inbox until new
    /// activity wakes it; a manual un-settle moves it back.
    #[gpui::test]
    fn test_settling_a_task_moves_it_out_of_the_inbox(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.sessions = vec![summary_at("s1", "One", "/a")];
            this.completed_unread_sessions.insert("s1".to_string());
            this.sidebar.update(cx, |sidebar, _| {
                sidebar.set_settled_for_test(HashSet::from(["s1".to_string()]));
            });
            this.sync_sidebar(cx);
            // The manual settle outranks the unseen completion...
            assert_eq!(this.sidebar_settled(cx), vec![0]);
            assert!(this.sidebar_active(cx).is_empty());
            // ...but a live run is activity: it wakes the task again.
            this.active_runs
                .insert("s1".to_string(), "run-1".to_string());
            this.sync_sidebar(cx);
            assert_eq!(this.sidebar_active(cx), vec![0]);

            this.active_runs.clear();
            this.sidebar.update(cx, |sidebar, _| {
                sidebar.set_settled_for_test(HashSet::new());
                sidebar.set_unsettled_for_test(HashSet::from(["s1".to_string()]));
            });
            this.sync_sidebar(cx);
            assert_eq!(this.sidebar_active(cx), vec![0]);
            assert!(this.sidebar_settled(cx).is_empty());
            // Reading the task (no live run, no unseen completion) leaves it
            // in the active inbox; only an explicit settle moves it out.
            this.completed_unread_sessions.clear();
            this.sidebar.update(cx, |sidebar, _| {
                sidebar.set_unsettled_for_test(HashSet::new());
            });
            this.sync_sidebar(cx);
            assert_eq!(this.sidebar_active(cx), vec![0]);
            assert!(this.sidebar_settled(cx).is_empty());
        });
    }

    /// The row buttons drive `settle_task` and `unsettle_task` back to
    /// back; a settle followed by an un-settle must return the task to
    /// the top of the active inbox.
    #[gpui::test]
    fn test_settle_buttons_toggle_a_task_back_and_forth(cx: &mut TestAppContext) {
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
            // An already-woken task can still be un-settled again.
            this.settle_task("s1", cx);
            this.unsettle_task("s1", cx);
            assert_eq!(this.sidebar_active(cx), vec![0]);
            assert!(this.sidebar_settled(cx).is_empty());
        });
    }

    /// A finished run wakes a settled task and the sections follow
    /// without any manual rebuild: the run events own the sidebar
    /// entries.
    #[gpui::test]
    fn test_a_finished_run_wakes_a_settled_task(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.selected_session = None;
            this.sessions = vec![summary("s1", "One")];
            this.sidebar.update(cx, |sidebar, _| {
                sidebar.set_settled_for_test(HashSet::from(["s1".to_string()]));
            });
            this.sync_sidebar(cx);
            assert_eq!(this.sidebar_settled(cx), vec![0]);
            assert!(this.sidebar_active(cx).is_empty());

            this.handle_run_event(
                "s1",
                "run-1",
                maple_agent::agent::AgentRunEvent::Started,
                cx,
            );
            assert_eq!(this.sidebar_active(cx), vec![0]);

            this.handle_run_event(
                "s1",
                "run-1",
                maple_agent::agent::AgentRunEvent::Finished(
                    maple_agent::agent::AgentRunTerminal::Completed,
                ),
                cx,
            );
            assert!(this.sidebar.read(cx).settled_tasks().is_empty());
            assert_eq!(this.sidebar_active(cx), vec![0]);
            assert!(this.sidebar_settled(cx).is_empty());
            assert!(this.completed_unread_sessions.contains("s1"));
        });
    }

    /// Application Vim drives the task popup: stepping moves the
    /// highlight and Enter runs the highlighted action.
    #[gpui::test]
    fn test_popup_vim_drives_the_task_menu(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.application_vim_enabled = true;
            this.sessions = vec![summary("s1", "One")];
            this.sync_sidebar(cx);
            this.sidebar
                .update(cx, |sidebar, cx| sidebar.open_task_menu_for_test("s1", cx));
            // The first step highlights "Rename task"; Enter runs it.
            assert!(this.step_sidebar_popup(1, 1, cx));
            this.activate_sidebar_popup(cx);
            assert!(matches!(
                this.sidebar.read(cx).rename_target(),
                Some(RenameTarget::Task(ref target)) if target == "s1"
            ));
            this.cancel_rename(cx);
            // No popup open: stepping reports nothing to do.
            this.sidebar
                .update(cx, |sidebar, cx| sidebar.close_popups(cx));
            assert!(!this.step_sidebar_popup(1, 1, cx));
        });
    }

    #[gpui::test]
    fn test_retained_python_is_resettable_on_a_settled_task(cx: &mut TestAppContext) {
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.sessions = vec![summary("s1", "Retained scratchpad")];
            this.sync_sidebar(cx);
            this.sidebar.update(cx, |sidebar, cx| {
                sidebar.settle_task("s1", cx);
                assert!(sidebar.settled_tasks().contains("s1"));
                sidebar.open_task_menu_for_test("s1", cx);
                assert!(
                    !sidebar
                        .task_menu_labels_for_test("s1")
                        .contains(&"Reset Python")
                );
                sidebar.set_python_resettable_for_test(true);
                assert!(
                    sidebar
                        .task_menu_labels_for_test("s1")
                        .contains(&"Reset Python")
                );
            });
        });
    }

    /// Application Vim drives the switcher popup: a count prefix reaches
    /// a project row and Enter scopes the sidebar to it.
    #[gpui::test]
    fn test_popup_vim_drives_the_switcher_menu(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.application_vim_enabled = true;
            this.sessions = vec![summary_at("s1", "One", "/a")];
            this.recent_roots = vec!["/b".to_string()];
            this.sync_sidebar(cx);
            this.sidebar
                .update(cx, |sidebar, cx| sidebar.set_switcher_menu_open(true, cx));
            // Rows: "All projects", "/b", "/a", "New project…"; a count of
            // two lands on "/b".
            assert!(this.step_sidebar_popup(1, 2, cx));
            this.activate_sidebar_popup(cx);
            assert_eq!(this.sidebar.read(cx).project_filter(), Some("/b"));
            assert!(!this.sidebar.read(cx).switcher_menu_open());
        });
    }

    /// Escape closes whichever sidebar popup is open.
    #[gpui::test]
    fn test_escape_closes_the_sidebar_popups(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let screen = screen(cx);
        screen.update(cx, |this, cx| {
            this.sidebar
                .update(cx, |sidebar, cx| sidebar.set_switcher_menu_open(true, cx));
            this.sidebar
                .update(cx, |sidebar, cx| sidebar.open_task_menu_for_test("s1", cx));
            this.escape(cx);
            assert!(!this.sidebar.read(cx).switcher_menu_open());
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
            this.pending_permissions.push(PendingPermission {
                session_id: "s1".to_string(),
                run_id: "run-1".to_string(),
                request_id: "req-1".to_string(),
                tool_name: "bash".to_string(),
                prompt: None,
                arguments: "".into(),
            });
            this.leave_selected_session(cx);
            assert_eq!(this.selected_session, None);
            assert!(this.btw.is_none());
            assert!(this.queue.is_empty());
            assert!(this.queue_edit.is_none());
            assert!(this.pending_permissions.is_empty());
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
            this.pending_permissions.push(PendingPermission {
                session_id: "s1".to_string(),
                run_id: "run-1".to_string(),
                request_id: "req-1".to_string(),
                tool_name: "bash".to_string(),
                prompt: None,
                arguments: "".into(),
            });
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
            assert_eq!(this.pending_permissions.len(), 1);
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

    /// Alt-Up and Alt-Down walk the task rows in the order the sidebar
    /// shows them, across projects, and stop at both ends.
    #[gpui::test]
    fn test_task_stepping_walks_the_whole_list(cx: &mut TestAppContext) {
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
            assert_eq!(this.task_step_target(1, cx), Some((4, "s2".to_string())));
            assert_eq!(
                this.task_step_target(-1, cx),
                None,
                "the first task is the top"
            );
            this.selected_session = Some("s2".to_string());
            this.sync_sidebar_selection(cx);
            assert_eq!(this.task_step_target(1, cx), Some((5, "s3".to_string())));
            assert_eq!(this.task_step_target(-1, cx), Some((3, "s1".to_string())));
            // With nothing selected, each direction enters from its end.
            this.selected_session = None;
            this.sync_sidebar_selection(cx);
            assert_eq!(this.task_step_target(1, cx), Some((3, "s1".to_string())));
            assert_eq!(this.task_step_target(-1, cx), Some((5, "s3".to_string())));
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

            this.finish_new_session(summary_at("created", "Created", "/work/alpha"), 1, cx);

            assert!(!this.session_setup_pending);
            assert_eq!(this.selected_session.as_deref(), Some("newer"));
            assert_eq!(this.project_root.as_deref(), Some("/work/beta"));
            assert!(this.sessions.iter().any(|session| session.id == "created"));
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
                    mode: None,
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
                crate::backend::AgentBackend::new("http://127.0.0.1:9".to_string(), String::new())
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
            fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                // Fixed size: the test window's own bounds are not applied
                // to the root view in the harness, and a percentage height
                // collapses to zero.
                div()
                    .w(px(1200.))
                    .h(px(800.))
                    .flex()
                    .flex_col()
                    .child(self.chat.update(cx, |chat, cx| chat.render_transcript(cx)))
            }
        }

        let chat = cx.new(move |cx| {
            let _guard = SETTINGS_LOCK.lock();
            let backend = std::sync::Arc::new(
                crate::backend::AgentBackend::new("http://127.0.0.1:9".to_string(), String::new())
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
                crate::backend::AgentBackend::new("http://127.0.0.1:9".to_string(), String::new())
                    .expect("backend"),
            );
            ChatScreen::new(backend, "user".to_string(), cx)
        });
        chat.update(cx, |this, _cx| {
            // The real constructor bootstraps and may ask to trust the
            // machine's home directory; that modal would take the keys.
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
                crate::backend::AgentBackend::new("http://127.0.0.1:9".to_string(), String::new())
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
                crate::backend::AgentBackend::new("http://127.0.0.1:9".to_string(), String::new())
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
        assert!(cx.update(|_window, app| chat.read(app).root_menu_open));
        cx.simulate_keystrokes("escape");
        assert!(!cx.update(|_window, app| chat.read(app).root_menu_open));

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
                crate::backend::AgentBackend::new("http://127.0.0.1:9".to_string(), String::new())
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
                crate::backend::AgentBackend::new("http://127.0.0.1:9".to_string(), String::new())
                    .expect("backend"),
            );
            crate::desktop::register_key_bindings(cx);
            let mut chat = ChatScreen::new_without_start(backend, "user".to_string(), cx);
            chat.selected_session = Some("s1".to_string());
            chat.recent_roots = vec![absolute_fixture_root("one"), absolute_fixture_root("two")];
            chat.booting = false;
            chat
        });

        let (_host, cx) = cx.add_window_view(|_window, _cx| ChatHost { chat: chat.clone() });
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        let composer_handle =
            cx.update(|_window, app| chat.read(app).composer.clone().unwrap().focus_handle(app));
        cx.update(|window, app| window.focus(&composer_handle, app));

        cx.simulate_keystrokes("secondary-p");
        assert!(cx.update(|_window, app| chat.read(app).root_menu_open));
        assert_ne!(
            cx.update(|window, app| window.focused(app)),
            Some(composer_handle.clone()),
            "the open menu must hold the focus, or the arrows type instead"
        );

        // Two recent roots and the "New project…" row: down, down, down
        // wraps back to the first.
        cx.simulate_keystrokes("down");
        assert_eq!(
            cx.update(|_window, app| chat.read(app).root_menu_selected),
            Some(0)
        );
        cx.simulate_keystrokes("down down");
        assert_eq!(
            cx.update(|_window, app| chat.read(app).root_menu_selected),
            Some(2)
        );
        cx.simulate_keystrokes("down");
        assert_eq!(
            cx.update(|_window, app| chat.read(app).root_menu_selected),
            Some(0)
        );
        cx.simulate_keystrokes("up");
        assert_eq!(
            cx.update(|_window, app| chat.read(app).root_menu_selected),
            Some(2)
        );

        // Enter on a recent root asks for the switch and closes the menu;
        // the composer takes the typing back.
        cx.simulate_keystrokes("up up");
        assert_eq!(
            cx.update(|_window, app| chat.read(app).root_menu_selected),
            Some(0)
        );
        cx.simulate_keystrokes("enter");
        assert!(!cx.update(|_window, app| chat.read(app).root_menu_open));
        assert_eq!(
            cx.update(|window, app| window.focused(app)),
            Some(composer_handle),
            "closing the menu must hand the focus back"
        );

        // Application Vim adds its own context to the focused menu, so its
        // j/k aliases are live without disturbing the legacy arrow bindings.
        chat.update(cx, |this, cx| this.set_application_vim_enabled(true, cx));
        cx.simulate_keystrokes("secondary-p");
        assert!(cx.update(|_window, app| chat.read(app).root_menu_open));
        cx.simulate_keystrokes("j j");
        assert_eq!(
            cx.update(|_window, app| chat.read(app).root_menu_selected),
            Some(1)
        );
        cx.simulate_keystrokes("k");
        assert_eq!(
            cx.update(|_window, app| chat.read(app).root_menu_selected),
            Some(0)
        );
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
                crate::backend::AgentBackend::new("http://127.0.0.1:9".to_string(), String::new())
                    .expect("backend"),
            );
            crate::desktop::register_key_bindings(cx);
            let mut chat = ChatScreen::new_without_start(backend, "user".to_string(), cx);
            chat.selected_session = Some("s1".to_string());
            chat.booting = false;
            chat.replace_timeline(vec![user_item("u1", "hello")]);
            chat.models = vec!["voxtral-small-24b".to_string()];
            chat.models_menu_open = true;
            chat
        });

        let (_host, cx) = cx.add_window_view(|_window, _cx| ChatHost { chat: chat.clone() });
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));

        let menu = cx
            .debug_bounds("composer-menu")
            .expect("the models menu renders while models_menu_open is set");
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
}
