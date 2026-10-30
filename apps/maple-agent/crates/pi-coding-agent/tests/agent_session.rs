mod common;

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Harness, echo_tool, entry_kinds, last_assistant_text, record, roles};
use pi_agent_core::AgentEvent;
use pi_agent_core::QueueMode;
use pi_agent_core::{AgentToolResult, FnTool};
use pi_ai::faux::{FauxProvider, faux_message};
use pi_ai::{AssistantContent, Content, ImageContent, Message, StopReason, ThinkingLevel, Usage};
use pi_coding_agent::compaction::{CompactionResult, estimate_tokens};
use pi_coding_agent::extensions::{
    BeforeCompactResult, BeforeForkResult, BeforeTreeResult, CustomMessageDraft, ForkPosition,
    Input, InputAction, ModelSelect, SessionBeforeCompact, SessionBeforeFork, SessionBeforeTree,
    SlashCommandSource, extension,
};
use pi_coding_agent::resources::{PromptTemplate, ResourceSource, Resources, Skill};
use pi_coding_agent::session::{EntryKind, SessionEntry, SessionHeader, SessionManager};
use pi_coding_agent::store::{JsonlStore, MemoryStore, SessionStore};
use pi_coding_agent::{
    AgentSession, AgentSessionError, AgentSessionEvent, PromptOptions, PromptOutcome,
    SessionMessage, StreamingBehavior, TreeMove, TreeNavigation,
};
use serde_json::json;

#[tokio::test]
async fn a_partial_resume_preserves_the_inherited_thinking_level() {
    let harness = Harness::new();
    let model = harness.faux.model();
    let mut tree = SessionManager::in_memory("/work");
    tree.append_model_change(&model.provider, &model.id);
    tree.append_thinking_level_change(ThinkingLevel::High);
    tree.append_message(SessionMessage::Llm(Message::User(
        pi_ai::UserMessage::text("old"),
    )));
    let boundary = tree.append_message(SessionMessage::Llm(Message::User(
        pi_ai::UserMessage::text("kept"),
    )));
    tree.append_compaction(
        "summary".into(),
        Some(boundary.clone()),
        100,
        None,
        None,
        false,
    );
    let base = tree.base_before(&boundary).unwrap();
    let tail = tree.entries()[3..].to_vec();
    let mut options = harness.options();
    options.session =
        SessionManager::open_partial(tree.header().clone(), base, tail, Box::new(MemoryStore));
    options.settings.default_thinking_level = Some(ThinkingLevel::Low);
    let resumed = AgentSession::new(options).await.unwrap();
    assert_eq!(resumed.thinking_level(), ThinkingLevel::High);
    assert!(resumed.with_session(|tree| {
        tree.entries()
            .iter()
            .all(|entry| !matches!(entry.kind, EntryKind::ThinkingLevelChange { .. }))
    }));
}

#[tokio::test]
async fn a_prompt_records_the_system_prompt_and_the_exchange() {
    let harness = Harness::new();
    harness.faux.push_text("hello");
    harness.faux.push_text("again");
    let session = harness
        .session_with(|options| options.tools = vec![echo_tool(true)])
        .await;
    let events = record(&session);

    assert_eq!(
        session
            .prompt("hi", PromptOptions::default())
            .await
            .unwrap(),
        PromptOutcome::Completed
    );
    // Ordinary messages are persisted before MessageEnd without broadening Pi's
    // EntryAppended event, which remains reserved for explicit special entries.
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .all(|event| !event.starts_with("entry:"))
    );
    // A new session records the model and thinking level it starts with, as in Pi.
    assert_eq!(
        entry_kinds(&session),
        [
            "model_change",
            "thinking_level_change",
            "message:system",
            "message:user",
            "message:assistant"
        ]
    );
    let prompt = session.system_prompt();
    assert!(prompt.contains("inside Maple"), "{prompt}");
    assert!(prompt.contains("- echo: Echo text back"));
    assert!(prompt.contains("- Echo only when asked"));
    assert!(prompt.contains("<cwd>\n/work\n</cwd>"));
    let request = &harness.faux.requests()[0];
    assert_eq!(
        pi_ai::transcript::current_tools(&request.context.messages)[0].name,
        "echo"
    );

    // An unchanged prompt is not sent again.
    session
        .prompt("more", PromptOptions::default())
        .await
        .unwrap();
    assert_eq!(
        entry_kinds(&session)[5..],
        ["message:user", "message:assistant"]
    );
    assert_eq!(
        events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| *event == "settled")
            .count(),
        2
    );
}

#[tokio::test]
async fn changing_tools_is_declared_once_in_the_transcript() {
    let harness = Harness::new();
    harness.faux.push_text("one");
    harness.faux.push_text("two");
    let session = harness
        .session_with(|options| options.tools = vec![echo_tool(true)])
        .await;
    session
        .prompt("first", PromptOptions::default())
        .await
        .unwrap();
    session.set_active_tools(&[]);
    session
        .prompt("second", PromptOptions::default())
        .await
        .unwrap();

    let update = session.with_session(|tree| {
        tree.entries()
            .iter()
            .filter_map(|entry| match &entry.kind {
                EntryKind::Message {
                    message: SessionMessage::Llm(Message::System(system)),
                } => Some(system.clone()),
                _ => None,
            })
            .nth(1)
            .unwrap()
    });
    assert_eq!(update.tools_removed[0].name, "echo");
    // The tool list section changes with the tools.
    assert!(update.sections.contains_key("tools"));
    assert!(
        pi_ai::transcript::current_tools(&harness.faux.requests()[1].context.messages).is_empty()
    );
}

#[tokio::test]
async fn a_stored_session_resumes_with_its_messages_and_model() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let harness = Harness::new();
    harness.faux.push_text("remembered");
    let first = harness
        .session_with(|options| {
            options.session = SessionManager::create("/work", Box::new(JsonlStore::new(&path)))
        })
        .await;
    first
        .prompt("remember this", PromptOptions::default())
        .await
        .unwrap();
    first.set_thinking_level(ThinkingLevel::High).await;
    let id = first.session_id();

    let (header, entries) = JsonlStore::load(&path).unwrap().unwrap();
    let resumed = harness
        .session_with(|options| {
            options.model = None;
            options.session =
                SessionManager::open(header, entries, Box::new(JsonlStore::new(&path)));
        })
        .await;
    assert_eq!(resumed.session_id(), id);
    assert_eq!(resumed.messages(), first.messages());
    assert_eq!(
        resumed.model().map(|model| model.id),
        Some("faux-1".to_string())
    );
    assert_eq!(resumed.thinking_level(), ThinkingLevel::High);
}

#[tokio::test]
async fn transient_errors_are_retried_and_the_failed_attempt_leaves_the_context() {
    let harness = Harness::new();
    harness.faux.push_error("503 Service Unavailable");
    harness.faux.push_text("recovered");
    let session = harness.session().await;
    let events = record(&session);

    session
        .prompt("go", PromptOptions::default())
        .await
        .unwrap();
    assert_eq!(last_assistant_text(&session), "recovered");
    assert_eq!(roles(&session.messages()), ["system", "user", "assistant"]);
    let events = events.lock().unwrap();
    assert!(events.contains(&"retry_start:1".to_string()));
    assert!(events.contains(&"retry_end:true:1".to_string()));
    // As in Pi, the failed attempt's run says a retry follows, and the edit that hides
    // the attempt is reported.
    let ends: Vec<&String> = events
        .iter()
        .filter(|event| event.starts_with("agent_end") || event.as_str() == "entry:context_edit")
        .collect();
    assert_eq!(
        ends,
        ["agent_end:true", "entry:context_edit", "agent_end:false"]
    );
    // The tree keeps the failed attempt and the edit that hides it.
    assert_eq!(
        entry_kinds(&session)[2..],
        [
            "message:system",
            "message:user",
            "message:assistant",
            "context_edit",
            "message:assistant"
        ]
    );
}

#[tokio::test]
async fn retries_give_up_after_the_budget() {
    let harness = Harness::new();
    for _ in 0..3 {
        harness.faux.push_error("overloaded");
    }
    let session = harness
        .session_with(|options| options.settings.retry.max_retries = 2)
        .await;
    let events = record(&session);
    session
        .prompt("go", PromptOptions::default())
        .await
        .unwrap();
    assert_eq!(harness.faux.requests().len(), 3);
    assert!(
        events
            .lock()
            .unwrap()
            .contains(&"retry_end:false:2".to_string())
    );
}

/// Two exchanges with long replies, so compaction has something to summarize.
fn summarized() -> TreeNavigation {
    TreeNavigation {
        summarize: true,
        ..TreeNavigation::default()
    }
}

async fn with_history(harness: &Harness, session: &AgentSession) {
    for turn in 0..2 {
        harness
            .faux
            .push_text(&format!("reply {turn} {}", "x".repeat(400)));
        session
            .prompt(&format!("question {turn}"), PromptOptions::default())
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn an_overflow_is_compacted_and_the_request_retried() {
    let harness = Harness::new();
    let session = harness
        .session_with(|options| options.settings.compaction.keep_recent_tokens = 50)
        .await;
    with_history(&harness, &session).await;
    harness
        .faux
        .push_error("prompt is too long: 213462 tokens > 200000 maximum");
    // The cut falls inside the last turn: one summary for the history, one for the turn.
    harness.faux.push_text("## Goal\nanswer questions");
    harness.faux.push_text("## Original Request\nquestion 1");
    harness.faux.push_text("recovered");
    let events = record(&session);

    session
        .prompt("question 2", PromptOptions::default())
        .await
        .unwrap();
    assert_eq!(last_assistant_text(&session), "recovered");
    assert!(
        events
            .lock()
            .unwrap()
            .contains(&"compaction_end:Overflow:ok:retry".to_string())
    );
    let messages = session.messages();
    assert!(
        messages
            .iter()
            .any(|message| message.role() == "compactionSummary")
    );
    assert!(!messages.iter().any(
        |message| matches!(message, SessionMessage::Llm(Message::Assistant(a)) if a.is_failure())
    ));
}

#[tokio::test]
async fn a_context_near_its_limit_is_compacted_after_the_turn() {
    let mut model = FauxProvider::default_model();
    model.context_window = 1_000;
    let harness = Harness::with_model(model);
    let mut reply = faux_message(vec![AssistantContent::text("big answer")]);
    reply.usage = Usage {
        input: 850,
        output: 50,
        total_tokens: 900,
        ..Usage::default()
    };
    harness.faux.push_reply(reply);
    harness.faux.push_text("## Goal\nsummarized");
    let session = harness
        .session_with(|options| {
            options.settings.compaction.reserve_tokens = 200;
            options.settings.compaction.keep_recent_tokens = 1;
        })
        .await;
    let events = record(&session);

    session
        .prompt("question", PromptOptions::default())
        .await
        .unwrap();
    assert!(
        events
            .lock()
            .unwrap()
            .contains(&"compaction_end:Threshold:ok".to_string())
    );
    assert!(entry_kinds(&session).contains(&"compaction".to_string()));
}

#[tokio::test]
async fn manual_compaction_summarizes_the_older_turns() {
    let harness = Harness::new();
    let session = harness
        .session_with(|options| options.settings.compaction.keep_recent_tokens = 50)
        .await;
    with_history(&harness, &session).await;
    harness.faux.push_text("## Goal\nanswer questions");
    harness.faux.push_text("## Original Request\nquestion 1");

    let result = session.compact(Some("keep it short")).await.unwrap();
    assert!(result.summary.starts_with("## Goal"));
    assert!(
        result
            .summary
            .contains("**Turn Context (split turn):**\n\n## Original Request")
    );
    let messages = session.messages();
    assert_eq!(roles(&messages)[..2], ["system", "compactionSummary"]);
    assert!(
        harness
            .sent_text(2)
            .contains("Additional focus: keep it short")
    );
    assert!(matches!(
        session.compact(None).await,
        Err(AgentSessionError::NothingToCompact)
    ));
}

#[tokio::test]
async fn a_summary_request_that_fails_for_a_moment_is_retried_and_reported() {
    let harness = Harness::new();
    let session = harness
        .session_with(|options| options.settings.compaction.keep_recent_tokens = 50)
        .await;
    with_history(&harness, &session).await;
    let events = record(&session);
    harness.faux.push_error("503 Service Unavailable");
    harness.faux.push_text("## Goal\nanswer questions");
    harness.faux.push_text("## Original Request\nquestion 1");

    let result = session.compact(None).await.unwrap();
    assert!(result.summary.starts_with("## Goal"));
    // As Pi reports a summary request's retries.
    let events = events.lock().unwrap();
    let retries: Vec<&String> = events
        .iter()
        .filter(|event| event.starts_with("summary_retry"))
        .collect();
    assert_eq!(
        retries,
        [
            "summary_retry:1",
            "summary_retry_start:Compaction(Manual)",
            "summary_retry_end"
        ]
    );
}

#[tokio::test]
async fn manual_compaction_stops_a_run_first() {
    let harness = Harness::new();
    let session = harness
        .session_with(|options| options.settings.compaction.keep_recent_tokens = 50)
        .await;
    with_history(&harness, &session).await;
    harness.faux.push_hang();
    let runner = session.clone();
    let run =
        tokio::spawn(async move { runner.prompt("question 2", PromptOptions::default()).await });
    while harness.faux.requests().len() < 3 {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    harness.faux.push_text("## Goal\nanswer questions");
    harness.faux.push_text("## Original Request\nquestion 2");

    // As in Pi, the run is stopped rather than the compaction refused.
    let result = session.compact(None).await.unwrap();
    assert!(result.summary.starts_with("## Goal"));
    run.await.unwrap().unwrap();
    assert!(!session.is_streaming());
    assert_eq!(
        roles(&session.messages())[..2],
        ["system", "compactionSummary"]
    );
    // The stopped turn is not continued.
    let Some(SessionMessage::Llm(Message::Assistant(last))) = session.messages().last().cloned()
    else {
        panic!()
    };
    assert_eq!(last.stop_reason, StopReason::Aborted);
}

#[tokio::test]
async fn compaction_thinks_at_the_sessions_level_and_a_branch_summary_does_not() {
    let harness = Harness::new();
    let session = harness
        .session_with(|options| options.settings.compaction.keep_recent_tokens = 50)
        .await;
    with_history(&harness, &session).await;
    session.set_thinking_level(ThinkingLevel::High).await;
    harness.faux.push_text("## Goal\nanswer questions");
    harness.faux.push_text("## Original Request\nquestion 1");
    session.compact(None).await.unwrap();
    let requests = harness.faux.requests();
    assert_eq!(requests.len(), 4);
    assert!(
        requests[2..]
            .iter()
            .all(|request| request.options.reasoning == Some(ThinkingLevel::High))
    );

    // As in Pi, a branch summary is written without thinking.
    let target = session.with_session(|tree| {
        tree.branch()
            .iter()
            .find(|entry| {
                matches!(&entry.kind, EntryKind::Message { message } if message.role() == "assistant")
            })
            .unwrap()
            .id
            .clone()
    });
    harness
        .faux
        .push_text("## Goal\nexplored a second question");
    session.navigate_tree(&target, summarized()).await.unwrap();
    let requests = harness.faux.requests();
    assert_eq!(requests.len(), 5);
    assert_eq!(requests[4].options.reasoning, None);
}

#[tokio::test]
async fn navigating_the_tree_can_summarize_the_branch_left_behind() {
    let harness = Harness::new();
    harness.faux.push_text("first answer");
    harness.faux.push_text("second answer");
    harness
        .faux
        .push_text("## Goal\nexplored a second question");
    let session = harness.session().await;
    session
        .prompt("first", PromptOptions::default())
        .await
        .unwrap();
    let target = session.with_session(|tree| tree.leaf_id().unwrap().to_string());
    session
        .prompt("second", PromptOptions::default())
        .await
        .unwrap();

    session.navigate_tree(&target, summarized()).await.unwrap();
    let messages = session.messages();
    assert_eq!(
        roles(&messages),
        ["system", "user", "assistant", "branchSummary"]
    );
    let summary = messages[3].text();
    assert!(
        summary.starts_with(
            "The user explored a different conversation branch before returning here.\n"
        )
    );
    assert!(summary.contains("explored a second question"));
    // The abandoned branch is still in the tree.
    assert_eq!(session.with_session(|tree| tree.children(&target).len()), 2);
    // The entry keeps the branch's file lists.
    let details =
        session.with_session(
            |tree| match &tree.entry(tree.leaf_id().unwrap()).unwrap().kind {
                EntryKind::BranchSummary { details, .. } => details.clone(),
                other => panic!("{other:?}"),
            },
        );
    assert_eq!(
        details,
        Some(json!({ "readFiles": [], "modifiedFiles": [] }))
    );
}

#[tokio::test]
async fn a_branch_summary_fits_the_window_less_the_reserve() {
    // A model that does not say its window gets Pi's 128k.
    let mut model = FauxProvider::default_model();
    model.context_window = 0;
    let harness = Harness::with_model(model);
    for answer in ["first answer", "second answer", "third answer"] {
        harness.faux.push_text(answer);
    }
    harness.faux.push_text("## Goal\nlong questions");
    let session = harness.session().await;
    session
        .prompt("first", PromptOptions::default())
        .await
        .unwrap();
    let target = session.with_session(|tree| tree.leaf_id().unwrap().to_string());
    for question in ["a", "b"] {
        session
            .prompt(&question.repeat(12_000), PromptOptions::default())
            .await
            .unwrap();
    }

    session.navigate_tree(&target, summarized()).await.unwrap();
    let prompt = harness.sent_text(3);
    assert!(prompt.contains(&"a".repeat(100)) && prompt.contains(&"b".repeat(100)));
}

#[tokio::test]
async fn moving_to_a_user_message_stands_before_it_as_in_pi() {
    let harness = Harness::new();
    harness.faux.push_text("first answer");
    harness.faux.push_text("second answer");
    harness.faux.push_text("third answer");
    let session = harness
        .session_with(|options| options.tools = vec![echo_tool(true)])
        .await;
    session
        .prompt("first", PromptOptions::default())
        .await
        .unwrap();
    // The second branch runs without the tool.
    session.set_active_tools(&[]);
    session
        .prompt("second", PromptOptions::default())
        .await
        .unwrap();
    let find = |text: &str| {
        session.with_session(|tree| {
            tree.branch()
                .iter()
                .find(|entry| matches!(&entry.kind, EntryKind::Message { message } if message.text() == text))
                .unwrap()
                .id
                .clone()
        })
    };
    let (first_answer, second) = (find("first answer"), find("second"));

    // Moving to the first answer brings back the tools that branch declared, as in Pi.
    let moved = session
        .navigate_tree(&first_answer, TreeNavigation::default())
        .await
        .unwrap();
    assert_eq!(moved, TreeMove::default());
    assert_eq!(session.active_tools(), ["echo"]);

    let moved = session
        .navigate_tree(
            &second,
            TreeNavigation {
                label: Some("retry here".into()),
                ..TreeNavigation::default()
            },
        )
        .await
        .unwrap();
    // The message's text comes back for the editor, and the session stands before it.
    assert_eq!(moved.editor_text.as_deref(), Some("second"));
    assert_eq!(moved.summary_entry, None);
    assert_eq!(last_assistant_text(&session), "first answer");
    assert_eq!(
        session.with_session(|tree| tree.label(&second).map(str::to_string)),
        Some("retry here".into())
    );
    // Before the second prompt, the tool it was sent without is off again.
    assert!(session.active_tools().is_empty());

    // Sending again starts a branch there, with no two user messages in a row.
    session
        .prompt("second, again", PromptOptions::default())
        .await
        .unwrap();
    let roles = roles(&session.messages());
    assert!(
        !roles.windows(2).any(|pair| pair == ["user", "user"]),
        "{roles:?}"
    );
}

#[tokio::test]
async fn extensions_change_how_a_tree_move_goes() {
    let harness = Harness::new();
    let tree = extension("tree", |api| {
        api.on(|event: SessionBeforeTree, _ctx| async move {
            assert!(event.preparation.user_wants_summary);
            assert!(!event.preparation.entries_to_summarize.is_empty());
            Ok(BeforeTreeResult {
                custom_instructions: Some("Only list the decisions.".into()),
                replace_instructions: Some(true),
                label: Some("decided".into()),
                ..BeforeTreeResult::default()
            })
        });
    });
    let session = harness.session_with_extensions(vec![tree]).await;
    harness.faux.push_text("first answer");
    session
        .prompt("first", PromptOptions::default())
        .await
        .unwrap();
    let target = session.with_session(|tree| tree.leaf_id().unwrap().to_string());
    harness.faux.push_text("second answer");
    session
        .prompt("second", PromptOptions::default())
        .await
        .unwrap();
    harness.faux.push_text("- decided things");

    let moved = session.navigate_tree(&target, summarized()).await.unwrap();
    // The handler's instructions replace the default ones, and its label goes on the
    // summary, as in Pi.
    let prompt = harness.sent_text(2);
    assert!(prompt.ends_with("Only list the decisions."), "{prompt}");
    assert!(!prompt.contains("Additional focus"));
    let summary = moved.summary_entry.unwrap();
    assert_eq!(
        session.with_session(|tree| tree.label(&summary.id).map(str::to_string)),
        Some("decided".into())
    );
}

#[tokio::test]
async fn a_fork_continues_in_a_new_session_with_the_path_only() {
    let harness = Harness::new();
    harness.faux.push_text("first answer");
    harness.faux.push_text("second answer");
    let session = harness.session().await;
    session
        .prompt("first", PromptOptions::default())
        .await
        .unwrap();
    let fork_point = session.with_session(|tree| tree.leaf_id().unwrap().to_string());
    session
        .prompt("second", PromptOptions::default())
        .await
        .unwrap();
    let original = session.session_id();

    session
        .fork(&fork_point, ForkPosition::At, Box::new(MemoryStore))
        .await
        .unwrap();
    assert_ne!(session.session_id(), original);
    assert_eq!(last_assistant_text(&session), "first answer");
    // A session kept in memory has no file for the fork to name, as in Pi.
    assert_eq!(
        session.with_session(|tree| tree.header().parent_session.clone()),
        None
    );
}

#[tokio::test]
async fn a_fork_before_a_user_message_gives_its_text_back() {
    let harness = Harness::new();
    harness.faux.push_text("first answer");
    harness.faux.push_text("second answer");
    let refusals = Arc::new(AtomicBool::new(true));
    let refuse = refusals.clone();
    let guard = extension("guard", move |api| {
        let refuse = refuse.clone();
        api.on(move |event: SessionBeforeFork, _ctx| {
            let refuse = refuse.clone();
            async move {
                assert_eq!(event.position, ForkPosition::Before);
                Ok(BeforeForkResult {
                    cancel: refuse.swap(false, Ordering::SeqCst),
                })
            }
        });
    });
    let session = harness.session_with_extensions(vec![guard]).await;
    for text in ["first", "second"] {
        session
            .prompt(text, PromptOptions::default())
            .await
            .unwrap();
    }
    let second = session.with_session(|tree| {
        tree.branch()
            .iter()
            .rev()
            .find(|entry| matches!(&entry.kind, EntryKind::Message { message } if message.text() == "second"))
            .unwrap()
            .id
            .clone()
    });

    // An extension can refuse a fork, as in Pi.
    assert!(matches!(
        session
            .fork(&second, ForkPosition::Before, Box::new(MemoryStore))
            .await,
        Err(AgentSessionError::Cancelled)
    ));
    let edited = session
        .fork(&second, ForkPosition::Before, Box::new(MemoryStore))
        .await
        .unwrap();
    assert_eq!(edited.as_deref(), Some("second"));
    assert_eq!(last_assistant_text(&session), "first answer");
    assert!(
        !session
            .messages()
            .iter()
            .any(|message| message.text() == "second")
    );
}

#[tokio::test]
async fn the_host_can_steer_and_queue_follow_ups_during_a_run() {
    let harness = Harness::new();
    let faux = harness.faux.clone();
    faux.push_tool_call("echo", json!({ "text": "a" }));
    faux.push_text("steered");
    faux.push_text("followed up");
    // The tool waits until the host has queued its messages.
    let release = Arc::new(tokio::sync::Notify::new());
    let mut waiting = echo_tool(true);
    let tool_release = release.clone();
    waiting.tool = FnTool::new(waiting.tool.declaration().clone(), move |_invocation| {
        let release = tool_release.clone();
        async move {
            release.notified().await;
            Ok(AgentToolResult::text("a"))
        }
    })
    .shared();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let behaviors = seen.clone();
    let input = extension("input", move |api| {
        let behaviors = behaviors.clone();
        api.on(move |event: Input, _ctx| {
            let behaviors = behaviors.clone();
            async move {
                behaviors.lock().unwrap().push(event.streaming_behavior);
                Ok(InputAction::Continue)
            }
        });
        api.register_command("note", "Add a note", |_args, _ctx| async { Ok(()) });
    });
    let session = harness
        .session_with(|options| {
            options.tools = vec![waiting];
            options.extensions = vec![input];
            options.resources.prompt_templates = vec![PromptTemplate {
                name: "fix".into(),
                description: "Fix an issue".into(),
                argument_hint: None,
                content: "Fix issue $1 carefully.".into(),
                file_path: "/work/.maple/prompts/fix.md".into(),
                source: ResourceSource::Project,
            }];
        })
        .await;
    let events = record(&session);
    let host = session.clone();
    let queued = AtomicBool::new(false);
    session.subscribe(move |event| {
        if matches!(
            event,
            AgentSessionEvent::Agent(AgentEvent::ToolExecutionStart { .. })
        ) && !queued.swap(true, Ordering::SeqCst)
        {
            let (host, release) = (host.clone(), release.clone());
            tokio::spawn(async move {
                // As in Pi, an extension command cannot wait in the queue.
                assert!(host.steer("/note later", Vec::new()).await.is_err());
                let image = ImageContent {
                    data: "aW1n".into(),
                    mime_type: "image/png".into(),
                };
                assert_eq!(
                    host.steer("/fix 7", vec![image]).await.unwrap(),
                    PromptOutcome::Queued
                );
                host.follow_up("then summarize", Vec::new()).await.unwrap();
                release.notify_one();
            });
        }
    });

    session
        .prompt("go", PromptOptions::default())
        .await
        .unwrap();
    let texts: Vec<String> = session
        .messages()
        .iter()
        .filter(|m| m.role() == "user")
        .map(SessionMessage::text)
        .collect();
    // Templates expand in steering, as in a prompt.
    assert_eq!(texts, ["go", "Fix issue 7 carefully.", "then summarize"]);
    // Input handlers see how each message waits for the run.
    assert_eq!(
        *seen.lock().unwrap(),
        [
            None,
            Some(StreamingBehavior::Steer),
            Some(StreamingBehavior::FollowUp)
        ]
    );
    // The steered image reaches the model with its text.
    let steered = harness.faux.requests()[1]
        .context
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::User(user) => Some(user.content.clone()),
            _ => None,
        })
        .unwrap();
    assert!(matches!(&steered[1], Content::Image(image) if image.data == "aW1n"));
    let events = events.lock().unwrap();
    assert!(events.contains(&"queue:1:1".to_string()));
    assert!(events.contains(&"queue:0:0".to_string()));
}

#[tokio::test]
async fn tools_registered_later_reach_the_next_prompt() {
    let harness = Harness::new();
    harness.faux.push_text("no tools yet");
    harness
        .faux
        .push_tool_call("echo", json!({ "text": "late" }));
    harness.faux.push_text("echoed");
    let session = harness.session().await;
    let declared = |index: usize| -> Vec<String> {
        pi_ai::transcript::current_tools(&harness.faux.requests()[index].context.messages)
            .into_iter()
            .map(|tool| tool.name)
            .collect()
    };

    session
        .prompt("one", PromptOptions::default())
        .await
        .unwrap();
    assert!(declared(0).is_empty());
    session.register_tool(echo_tool(true));
    assert_eq!(session.active_tools(), ["echo"]);
    session
        .prompt("two", PromptOptions::default())
        .await
        .unwrap();
    assert_eq!(declared(1), ["echo"]);
    let result = session
        .messages()
        .iter()
        .find_map(|message| match message {
            SessionMessage::Llm(Message::ToolResult(result)) => {
                Some(pi_ai::content_text(&result.content))
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(result, "late");
}

#[tokio::test]
async fn a_custom_message_the_user_does_not_see_starts_a_turn() {
    let harness = Harness::new();
    harness.faux.push_text("noted");
    let session = harness.session().await;
    let outcome = session
        .send_custom_message(CustomMessageDraft {
            custom_type: "background-result".into(),
            content: vec![Content::text("the agent finished")],
            display: false,
            details: None,
        })
        .await
        .unwrap();
    assert_eq!(outcome, PromptOutcome::Completed);
    // The model reads it as the user's turn; the transcript keeps it apart.
    let request = &harness.faux.requests()[0];
    let last_user = request
        .context
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::User(user) => Some(pi_ai::content_text(&user.content)),
            _ => None,
        })
        .unwrap();
    assert_eq!(last_user, "the agent finished");
    assert!(session.messages().iter().any(|message| matches!(
        message,
        SessionMessage::Custom(custom) if custom.custom_type == "background-result" && !custom.display
    )));
    assert_eq!(last_assistant_text(&session), "noted");
}

#[tokio::test]
async fn a_busy_session_queues_only_when_asked() {
    let harness = Harness::new();
    harness.faux.push_hang();
    let session = harness.session().await;
    let runner = session.clone();
    let run = tokio::spawn(async move { runner.prompt("first", PromptOptions::default()).await });
    while !session.is_streaming() {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    assert!(matches!(
        session.prompt("second", PromptOptions::default()).await,
        Err(AgentSessionError::Busy)
    ));
    let queued = PromptOptions {
        streaming_behavior: Some(StreamingBehavior::FollowUp),
        ..PromptOptions::default()
    };
    assert_eq!(
        session.prompt("later", queued).await.unwrap(),
        PromptOutcome::Queued
    );
    assert_eq!(
        session.clear_queue(),
        (Vec::new(), vec!["later".to_string()])
    );

    let events = record(&session);
    session.abort();
    run.await.unwrap().unwrap();
    let messages = session.messages();
    let Some(SessionMessage::Llm(Message::Assistant(last))) = messages.last() else {
        panic!()
    };
    assert_eq!(last.stop_reason, StopReason::Aborted);
    assert!(
        !events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event.starts_with("retry"))
    );
}

#[tokio::test]
async fn model_and_thinking_changes_are_recorded_and_clamped() {
    let harness = Harness::new();
    let mut plain = FauxProvider::default_model();
    plain.id = "plain".into();
    plain.reasoning = false;
    harness.models.register_models([plain.clone()]);
    let selected = Arc::new(Mutex::new(Vec::new()));
    let seen = selected.clone();
    let observer = extension("observer", move |api| {
        let models = seen.clone();
        api.on(move |event: ModelSelect, _ctx| {
            let models = models.clone();
            async move {
                models.lock().unwrap().push(event.model.id);
                Ok(())
            }
        });
    });
    let session = harness.session_with_extensions(vec![observer]).await;
    let events = record(&session);
    // Pi's default level, since the settings name none.
    assert_eq!(session.thinking_level(), ThinkingLevel::Medium);

    session.set_thinking_level(ThinkingLevel::High).await;
    assert_eq!(session.thinking_level(), ThinkingLevel::High);
    // Setting the same level again changes nothing.
    session.set_thinking_level(ThinkingLevel::High).await;
    session.set_model(plain.clone()).await;
    assert_eq!(session.thinking_level(), ThinkingLevel::Off);
    // The same model again is recorded but not announced.
    session.set_model(plain).await;
    assert_eq!(
        entry_kinds(&session),
        [
            "model_change",
            "thinking_level_change",
            "thinking_level_change",
            "model_change",
            "thinking_level_change",
            "model_change"
        ]
    );
    assert_eq!(*selected.lock().unwrap(), ["plain"]);
    assert_eq!(*events.lock().unwrap(), ["thinking:High", "thinking:Off"]);
}

#[tokio::test]
async fn a_model_switch_applies_the_default_thinking_level_again() {
    let harness = Harness::new();
    let mut other = FauxProvider::default_model();
    other.id = "other".into();
    harness.models.register_models([other.clone()]);
    let session = harness
        .session_with(|options| options.settings.default_thinking_level = Some(ThinkingLevel::Low))
        .await;
    assert_eq!(session.thinking_level(), ThinkingLevel::Low);
    session.set_thinking_level(ThinkingLevel::High).await;
    // As in Pi, the settings' default wins over the level the last model had.
    session.set_model(other).await;
    assert_eq!(session.thinking_level(), ThinkingLevel::Low);
}

#[tokio::test]
async fn a_model_with_its_own_level_starts_and_switches_at_it() {
    let harness = Harness::new();
    let mut other = FauxProvider::default_model();
    other.id = "other".into();
    harness.models.register_models([other.clone()]);
    let faux = harness.faux.model();
    let session = harness
        .session_with(|options| {
            options.settings.default_thinking_level = Some(ThinkingLevel::Low);
            options.settings.model_thinking_levels.insert(
                format!("{}/{}", faux.provider, faux.id),
                ThinkingLevel::High,
            );
        })
        .await;
    // As in Pi, a model's own level wins over the default, when a session starts with
    // it and when it switches to it.
    assert_eq!(session.thinking_level(), ThinkingLevel::High);
    session.set_model(other).await;
    assert_eq!(session.thinking_level(), ThinkingLevel::Low);
    session.set_model(faux).await;
    assert_eq!(session.thinking_level(), ThinkingLevel::High);
}

#[tokio::test]
async fn retry_compaction_and_queue_modes_change_for_the_session() {
    let harness = Harness::new();
    harness.faux.push_error("503 Service Unavailable");
    let session = harness.session().await;
    let events = record(&session);
    assert!(session.auto_retry_enabled() && session.auto_compaction_enabled());
    session.set_auto_retry_enabled(false);
    session.set_auto_compaction_enabled(false);
    session.set_steering_mode(QueueMode::All);
    session.set_follow_up_mode(QueueMode::All);
    assert_eq!(
        (session.steering_mode(), session.follow_up_mode()),
        (QueueMode::All, QueueMode::All)
    );
    assert!(!session.settings().retry.enabled && !session.settings().compaction.enabled);

    // With retries off, a transient error ends the run.
    session
        .prompt("go", PromptOptions::default())
        .await
        .unwrap();
    assert_eq!(harness.faux.requests().len(), 1);
    assert!(
        !events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event.starts_with("retry"))
    );
}

#[tokio::test]
async fn commands_list_templates_and_skills_after_extension_commands() {
    let harness = Harness::new();
    let commands = extension("commands", |api| {
        api.register_command("note", "Add a note", |_args, _ctx| async { Ok(()) });
    });
    let session = harness
        .session_with(|options| {
            options.extensions = vec![commands];
            options.resources.prompt_templates = vec![PromptTemplate {
                name: "fix".into(),
                description: "Fix an issue".into(),
                argument_hint: None,
                content: "Fix it.".into(),
                file_path: "/work/.maple/prompts/fix.md".into(),
                source: ResourceSource::Project,
            }];
            options.resources.skills = vec![Skill {
                name: "review".into(),
                description: "Review changes".into(),
                file_path: "/work/.maple/skills/review/SKILL.md".into(),
                base_dir: "/work/.maple/skills/review".into(),
                source: ResourceSource::Project,
                disable_model_invocation: false,
            }];
        })
        .await;
    let listed: Vec<(String, SlashCommandSource)> = session
        .commands()
        .into_iter()
        .map(|command| (command.name, command.source))
        .collect();
    assert_eq!(
        listed,
        [
            ("note".to_string(), SlashCommandSource::Extension),
            ("fix".to_string(), SlashCommandSource::Prompt),
            ("skill:review".to_string(), SlashCommandSource::Skill),
        ]
    );
}

#[tokio::test]
async fn templates_and_skills_expand_before_sending() {
    let dir = tempfile::tempdir().unwrap();
    let skill = dir.path().join("review/SKILL.md");
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(
        &skill,
        "---\ndescription: Review changes\n---\nRead the diff first.",
    )
    .unwrap();
    let harness = Harness::new();
    harness.faux.push_text("ok");
    harness.faux.push_text("ok");
    let session = harness
        .session_with(|options| {
            options.resources = Resources {
                prompt_templates: vec![PromptTemplate {
                    name: "fix".into(),
                    description: "Fix an issue".into(),
                    argument_hint: None,
                    content: "Fix issue $1 carefully.".into(),
                    file_path: dir.path().join("fix.md"),
                    source: ResourceSource::Project,
                }],
                ..Resources::load(
                    dir.path(),
                    &pi_coding_agent::resources::ResourcePaths {
                        agent_dir: dir.path().join("none"),
                        project_dir_name: ".maple".into(),
                        home_dir: None,
                    },
                    &pi_coding_agent::settings::Settings::default(),
                    std::slice::from_ref(&skill),
                    &[],
                    true,
                )
            };
            options.skill_load_hint = Some("Use the read tool to load a skill's file".into());
        })
        .await;

    session
        .prompt("/fix 42", PromptOptions::default())
        .await
        .unwrap();
    assert_eq!(harness.sent_text(0), "Fix issue 42 carefully.");
    session
        .prompt("/skill:review src/lib.rs", PromptOptions::default())
        .await
        .unwrap();
    assert!(
        harness
            .sent_text(1)
            .contains("Read the diff first.\n</skill>\n\nsrc/lib.rs")
    );
    assert!(session.system_prompt().contains("<name>review</name>"));
}

#[tokio::test]
async fn context_usage_reports_the_last_known_size() {
    let harness = Harness::new();
    let mut reply = faux_message(vec![AssistantContent::text("ok")]);
    reply.usage = Usage {
        input: 1_000,
        output: 280,
        total_tokens: 1_280,
        ..Usage::default()
    };
    harness.faux.push_reply(reply);
    let session = harness.session().await;
    session
        .prompt("hi", PromptOptions::default())
        .await
        .unwrap();
    let usage = session.context_usage().unwrap();
    assert_eq!((usage.tokens, usage.context_window), (Some(1_280), 128_000));
    assert!((usage.percent.unwrap() - 1.0).abs() < 0.001);
}

#[tokio::test]
async fn a_compaction_reports_the_size_it_leaves() {
    let harness = Harness::new();
    let session = harness
        .session_with(|options| options.settings.compaction.keep_recent_tokens = 50)
        .await;
    for turn in 0..2 {
        let mut reply = faux_message(vec![AssistantContent::text(format!(
            "reply {turn} {}",
            "x".repeat(400)
        ))]);
        reply.usage = Usage {
            input: 90_000,
            total_tokens: 90_000,
            ..Usage::default()
        };
        harness.faux.push_reply(reply);
        session
            .prompt(&format!("question {turn}"), PromptOptions::default())
            .await
            .unwrap();
    }
    harness.faux.push_text("## Goal\nanswer questions");
    harness.faux.push_text("## Original Request\nquestion 1");
    let ended = Arc::new(Mutex::new(Vec::new()));
    let sink = ended.clone();
    session.subscribe(move |event| {
        if let AgentSessionEvent::CompactionEnd {
            result: Some(result),
            ..
        } = event
        {
            sink.lock().unwrap().push(result.estimated_tokens_after);
        }
    });

    let result = session.compact(None).await.unwrap();
    let estimate: u64 = session.messages().iter().map(estimate_tokens).sum();
    assert_eq!(result.estimated_tokens_after, Some(estimate));
    assert_eq!(*ended.lock().unwrap(), [Some(estimate)]);
    // As in Pi, the size is unknown until a reply measures the context again: the kept
    // reply's usage measured it before the compaction.
    let usage = session.context_usage().unwrap();
    assert_eq!((usage.tokens, usage.percent), (None, None));
    harness.faux.push_text("measured");
    session
        .prompt("question 2", PromptOptions::default())
        .await
        .unwrap();
    assert!(session.context_usage().unwrap().tokens.is_some());
}

#[tokio::test]
async fn usage_from_before_a_context_edit_is_not_the_context_size() {
    let harness = Harness::new();
    let mut reply = faux_message(vec![AssistantContent::text("ok")]);
    reply.usage = Usage {
        input: 50_000,
        total_tokens: 50_000,
        ..Usage::default()
    };
    harness.faux.push_reply(reply);
    harness.faux.push_error("503 Service Unavailable");
    harness.faux.push_text("recovered");
    let session = harness
        .session_with(|options| options.settings.retry.base_delay_ms = 500)
        .await;
    session
        .prompt("first", PromptOptions::default())
        .await
        .unwrap();
    assert_eq!(session.context_usage().unwrap().tokens, Some(50_000));

    let events = record(&session);
    let runner = session.clone();
    let run = tokio::spawn(async move { runner.prompt("second", PromptOptions::default()).await });
    while !events
        .lock()
        .unwrap()
        .iter()
        .any(|event| event.starts_with("retry_start"))
    {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    // The failed attempt is omitted from the context by an edit made after that usage.
    let usage = session.context_usage().unwrap();
    assert!(usage.tokens.unwrap() < 10_000, "{usage:?}");
    run.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_retry_keeps_the_session_busy() {
    let harness = Harness::new();
    harness.faux.push_error("503 Service Unavailable");
    harness.faux.push_text("recovered");
    harness.faux.push_text("answered later");
    // Long enough for the checks below on a slow machine.
    let session = harness
        .session_with(|options| options.settings.retry.base_delay_ms = 500)
        .await;
    let events = record(&session);
    let runner = session.clone();
    let run = tokio::spawn(async move { runner.prompt("first", PromptOptions::default()).await });
    while !events
        .lock()
        .unwrap()
        .iter()
        .any(|event| event.starts_with("retry_start"))
    {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    // The agent waits out the backoff idle; the session does not take other work.
    assert!(session.is_streaming());
    assert!(matches!(
        session.prompt("second", PromptOptions::default()).await,
        Err(AgentSessionError::Busy)
    ));
    let follow_up = PromptOptions {
        streaming_behavior: Some(StreamingBehavior::FollowUp),
        ..PromptOptions::default()
    };
    assert_eq!(
        session.prompt("later", follow_up).await.unwrap(),
        PromptOutcome::Queued
    );

    run.await.unwrap().unwrap();
    assert_eq!(harness.faux.requests().len(), 3);
    assert_eq!(last_assistant_text(&session), "answered later");
    assert!(!session.is_streaming());
}

#[tokio::test]
async fn a_cancelled_retry_does_not_carry_into_the_next_prompt() {
    let harness = Harness::new();
    harness.faux.push_error("503 Service Unavailable");
    harness.faux.push_hang();
    let session = harness.session().await;
    let events = record(&session);
    let runner = session.clone();
    let run = tokio::spawn(async move { runner.prompt("first", PromptOptions::default()).await });
    while harness.faux.requests().len() < 2 {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    session.abort();
    run.await.unwrap().unwrap();
    assert!(
        events
            .lock()
            .unwrap()
            .contains(&"retry_end:false:1".to_string())
    );

    events.lock().unwrap().clear();
    harness.faux.push_text("fine");
    session
        .prompt("next", PromptOptions::default())
        .await
        .unwrap();
    assert!(
        !events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event.starts_with("retry"))
    );
}

#[tokio::test]
async fn the_starting_thinking_level_is_restored_on_resume() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let harness = Harness::new();
    harness.faux.push_text("hi");
    let first = harness
        .session_with(|options| {
            options.settings.default_thinking_level = Some(ThinkingLevel::High);
            options.session = SessionManager::create("/work", Box::new(JsonlStore::new(&path)));
        })
        .await;
    assert_eq!(first.thinking_level(), ThinkingLevel::High);
    first
        .prompt("hello", PromptOptions::default())
        .await
        .unwrap();

    let (header, entries) = JsonlStore::load(&path).unwrap().unwrap();
    let resumed = harness
        .session_with(|options| {
            options.session =
                SessionManager::open(header, entries, Box::new(JsonlStore::new(&path)));
        })
        .await;
    assert_eq!(resumed.thinking_level(), ThinkingLevel::High);
}

#[tokio::test]
async fn a_dropped_compaction_does_not_leave_the_session_busy() {
    let harness = Harness::new();
    let session = harness
        .session_with(|options| options.settings.compaction.keep_recent_tokens = 50)
        .await;
    with_history(&harness, &session).await;
    harness.faux.push_hang();
    let events = record(&session);

    let timed_out = tokio::time::timeout(Duration::from_millis(20), session.compact(None)).await;
    assert!(timed_out.is_err());
    assert!(
        events
            .lock()
            .unwrap()
            .contains(&"compaction_end:Manual:aborted".to_string())
    );
    harness.faux.push_text("## Goal\nanswer questions");
    harness.faux.push_text("## Original Request\nquestion 1");
    session.compact(None).await.unwrap();
}

#[tokio::test]
async fn a_reply_cut_off_by_a_full_context_is_compacted_and_asked_again() {
    let mut model = FauxProvider::default_model();
    model.context_window = 1_000;
    let harness = Harness::with_model(model);
    let session = harness
        .session_with(|options| {
            options.settings.compaction.reserve_tokens = 200;
            options.settings.compaction.keep_recent_tokens = 50;
        })
        .await;
    with_history(&harness, &session).await;
    // The server cut the input to fit and stopped before writing anything.
    let mut silent = faux_message(Vec::new());
    silent.stop_reason = StopReason::Length;
    silent.usage = Usage {
        input: 1_000,
        total_tokens: 1_000,
        ..Usage::default()
    };
    harness.faux.push_reply(silent);
    harness.faux.push_text("## Goal\nanswer questions");
    harness.faux.push_text("## Original Request\nquestion 1");
    harness.faux.push_text("recovered");
    let events = record(&session);

    session
        .prompt("question 2", PromptOptions::default())
        .await
        .unwrap();
    assert_eq!(last_assistant_text(&session), "recovered");
    assert!(
        events
            .lock()
            .unwrap()
            .contains(&"compaction_end:Overflow:ok:retry".to_string())
    );
}

#[tokio::test]
async fn recovering_a_cut_off_tool_call_omits_its_result_with_it() {
    // A resumed session ends with a reply cut off at the output limit, and a result for
    // its call saying it was not run (Pi #11227).
    let harness = Harness::new();
    let model = harness.faux.model();
    let mut tree = SessionManager::in_memory("/work");
    tree.append_model_change(&model.provider, &model.id);
    tree.append_message(SessionMessage::Llm(Message::User(
        pi_ai::UserMessage::text("check status"),
    )));
    let mut cut_off = faux_message(vec![AssistantContent::tool_call(
        "call_cut_off",
        "bash",
        json!({ "command": "echo" }),
    )]);
    cut_off.stop_reason = StopReason::Length;
    cut_off.usage = Usage {
        input: 100,
        output: 16,
        total_tokens: 116,
        ..Usage::default()
    };
    tree.append_message(SessionMessage::Llm(Message::Assistant(cut_off)));
    tree.append_message(SessionMessage::Llm(Message::ToolResult(
        pi_ai::ToolResultMessage {
            tool_call_id: "call_cut_off".into(),
            tool_name: "bash".into(),
            content: vec![Content::text("Not executed: output token limit reached.")],
            details: None,
            usage: None,
            is_error: true,
            timestamp: 0,
        },
    )));
    let supply = extension("supply", |api| {
        api.on(|event: SessionBeforeCompact, _ctx| async move {
            Ok(BeforeCompactResult {
                cancel: false,
                compaction: Some(CompactionResult {
                    summary: "summary".into(),
                    first_kept_entry_id: event.preparation.first_kept_entry_id,
                    tokens_before: event.preparation.tokens_before,
                    estimated_tokens_after: None,
                    usage: Default::default(),
                    details: None,
                }),
            })
        });
    });
    let session = harness
        .session_with(|options| {
            options.session = tree;
            options.extensions = vec![supply];
        })
        .await;
    harness.faux.push_text("done");
    let events = record(&session);

    session
        .prompt("continue", PromptOptions::default())
        .await
        .unwrap();
    let requests = harness.faux.requests();
    let sent = &requests.last().unwrap().context.messages;
    assert!(
        sent.iter().all(|message| match message {
            Message::ToolResult(_) => false,
            Message::Assistant(reply) => reply.stop_reason != StopReason::Length,
            _ => true,
        }),
        "{sent:?}"
    );
    // Both are omitted by entries of their own.
    let omitted = session.with_session(|tree| {
        tree.entries()
            .iter()
            .filter(|entry| {
                matches!(
                    entry.kind,
                    EntryKind::ContextEdit {
                        replacement: None,
                        ..
                    }
                )
            })
            .count()
    });
    assert_eq!(omitted, 2);
    let appended = events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| event.as_str() == "entry:context_edit")
        .count();
    assert_eq!(appended, 2);
}

/// A store whose every write fails.
struct ReadOnlyStore;

impl SessionStore for ReadOnlyStore {
    fn write_all(&mut self, _header: &SessionHeader, _entries: &[SessionEntry]) -> io::Result<()> {
        Err(io::Error::other("read-only"))
    }

    fn append(&mut self, _entry: &SessionEntry) -> io::Result<()> {
        Err(io::Error::other("read-only"))
    }
}

#[tokio::test]
async fn a_fork_that_cannot_be_written_still_moves_the_conversation() {
    let harness = Harness::new();
    harness.faux.push_text("first answer");
    harness.faux.push_text("second answer");
    let session = harness.session().await;
    session
        .prompt("first", PromptOptions::default())
        .await
        .unwrap();
    let fork_point = session.with_session(|tree| tree.leaf_id().unwrap().to_string());
    session
        .prompt("second", PromptOptions::default())
        .await
        .unwrap();
    let events = record(&session);

    session
        .fork(&fork_point, ForkPosition::At, Box::new(ReadOnlyStore))
        .await
        .unwrap();
    assert_eq!(last_assistant_text(&session), "first answer");
    assert!(
        events
            .lock()
            .unwrap()
            .contains(&"persistence_error".to_string())
    );
}

#[tokio::test]
async fn stopping_cancels_a_branch_summary() {
    let harness = Harness::new();
    harness.faux.push_text("first answer");
    harness.faux.push_text("second answer");
    harness.faux.push_hang();
    let session = harness.session().await;
    session
        .prompt("first", PromptOptions::default())
        .await
        .unwrap();
    let target = session.with_session(|tree| tree.leaf_id().unwrap().to_string());
    session
        .prompt("second", PromptOptions::default())
        .await
        .unwrap();
    let leaf = session.with_session(|tree| tree.leaf_id().unwrap().to_string());

    let navigator = session.clone();
    let navigation =
        tokio::spawn(async move { navigator.navigate_tree(&target, summarized()).await });
    while harness.faux.requests().len() < 3 {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    session.abort();
    assert!(matches!(
        navigation.await.unwrap(),
        Err(AgentSessionError::Cancelled)
    ));
    // The session stays where it was and is free again.
    assert_eq!(
        session.with_session(|tree| tree.leaf_id().map(str::to_string)),
        Some(leaf)
    );
    session.wait_for_idle().await;
    assert_eq!(last_assistant_text(&session), "second answer");
}

#[tokio::test]
async fn system_md_and_append_system_md_shape_the_prompt_unless_the_host_does() {
    let harness = Harness::new();
    let file = |name: &str, content: &str| {
        Some(pi_coding_agent::resources::ContextFile {
            path: name.into(),
            content: content.into(),
        })
    };
    let with_files = |options: &mut pi_coding_agent::AgentSessionOptions| {
        options.resources.system_prompt = file("SYSTEM.md", "You review pull requests.");
        options.resources.append_system_prompt = file("APPEND_SYSTEM.md", "Answer in French.");
    };
    let session = harness.session_with(with_files).await;
    let prompt = session.extension_context().next_system_prompt();
    assert!(prompt.starts_with("You review pull requests."), "{prompt}");
    assert!(prompt.contains("Answer in French."), "{prompt}");

    let session = harness
        .session_with(|options| {
            with_files(options);
            options.custom_prompt = Some("You write tests.".into());
            options.settings.append_system_prompt = Some("Answer in Dutch.".into());
        })
        .await;
    let prompt = session.extension_context().next_system_prompt();
    assert!(prompt.starts_with("You write tests."), "{prompt}");
    assert!(prompt.contains("Answer in Dutch."), "{prompt}");
    assert!(!prompt.contains("pull requests") && !prompt.contains("French"));
}

#[tokio::test]
async fn resources_set_between_prompts_reach_the_next_one_as_a_patch() {
    let dir = tempfile::tempdir().unwrap();
    let skill = dir.path().join("review/SKILL.md");
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(
        &skill,
        "---\ndescription: Review changes\n---\nRead the diff first.",
    )
    .unwrap();
    let file = |path: &str, content: &str| pi_coding_agent::resources::ContextFile {
        path: path.into(),
        content: content.into(),
    };
    let harness = Harness::new();
    harness.faux.push_text("one");
    harness.faux.push_text("two");
    let session = harness
        .session_with(|options| {
            options.resources.context_files = vec![file("/work/AGENTS.md", "Use tabs.")];
            options.settings.append_system_prompt = Some("Answer in Dutch.".into());
            options.skill_load_hint = Some("Use the read tool to load a skill's file".into());
        })
        .await;
    session
        .prompt("hi", PromptOptions::default())
        .await
        .unwrap();
    assert!(session.system_prompt().contains("Use tabs."));

    let mut resources = Resources::load(
        dir.path(),
        &pi_coding_agent::resources::ResourcePaths {
            agent_dir: dir.path().join("none"),
            project_dir_name: ".maple".into(),
            home_dir: None,
        },
        &pi_coding_agent::settings::Settings::default(),
        std::slice::from_ref(&skill),
        &[],
        true,
    );
    resources.context_files = vec![file("/work/AGENTS.md", "Use spaces.")];
    resources.append_system_prompt = Some(file("APPEND_SYSTEM.md", "Answer in French."));
    session.set_resources(resources);
    assert_eq!(session.resources().skills[0].name, "review");
    // A skill that is new by now expands.
    session
        .prompt("/skill:review", PromptOptions::default())
        .await
        .unwrap();
    assert!(harness.sent_text(1).contains("Read the diff first."));

    let prompt = session.system_prompt();
    assert!(prompt.contains("Use spaces.") && !prompt.contains("Use tabs."));
    assert!(prompt.contains("<name>review</name>"), "{prompt}");
    // The host's appended text still wins over APPEND_SYSTEM.md.
    assert!(prompt.contains("Answer in Dutch.") && !prompt.contains("French"));
    // Only the sections that changed went out again.
    assert_eq!(
        entry_kinds(&session)[5..],
        ["message:system", "message:user", "message:assistant"]
    );
    let patch = harness.faux.requests()[1]
        .context
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::System(system) => Some(system.sections.keys().cloned().collect::<Vec<_>>()),
            _ => None,
        })
        .unwrap();
    assert_eq!(patch, ["project_context", "skills"]);
}

#[tokio::test]
async fn images_a_tool_returns_are_made_to_fit_before_the_model_sees_them() {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use pi_agent_core::{AgentToolResult, FnTool};
    use pi_ai::{Content, Tool};
    use pi_coding_agent::extensions::{RegisteredTool, ToolPrompt};

    let mut png = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::new(3000, 10))
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    let data = BASE64.encode(png);
    let screenshot = FnTool::new(
        Tool::new("shot", "Take a screenshot", json!({ "type": "object" })),
        move |_| {
            let data = data.clone();
            async move {
                Ok(AgentToolResult {
                    content: vec![Content::image(data, "image/png")],
                    ..AgentToolResult::default()
                })
            }
        },
    );
    let harness = Harness::new();
    harness.faux.push_tool_call("shot", json!({}));
    harness.faux.push_text("A dark strip.");
    let session = harness
        .session_with(|options| {
            options.tools = vec![RegisteredTool {
                tool: screenshot.shared(),
                prompt: ToolPrompt::default(),
                active: true,
                extension: None,
            }];
        })
        .await;
    session
        .prompt("look", PromptOptions::default())
        .await
        .unwrap();

    let result = session
        .messages()
        .into_iter()
        .find_map(|message| match message {
            SessionMessage::Llm(Message::ToolResult(result)) => Some(result),
            _ => None,
        })
        .unwrap();
    let Content::Image(image) = &result.content[0] else {
        panic!("{:?}", result.content);
    };
    let sent = image::load_from_memory(&BASE64.decode(&image.data).unwrap()).unwrap();
    assert_eq!(sent.width(), 2000);
    assert!(pi_ai::content_text(&result.content).contains("[Image: original 3000x10"));
}

#[tokio::test]
async fn images_attached_to_a_prompt_are_made_to_fit_with_notes() {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as BASE64;

    let mut png = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::new(3000, 10))
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    let harness = Harness::new();
    harness.faux.push_text("A dark strip.");
    let session = harness.session().await;
    let options = PromptOptions {
        images: vec![
            ImageContent {
                data: BASE64.encode(png),
                mime_type: "image/png".into(),
            },
            ImageContent {
                data: BASE64.encode(b"not an image"),
                mime_type: "image/png".into(),
            },
        ],
        ..PromptOptions::default()
    };
    session.prompt("What is this?", options).await.unwrap();

    let user = harness.faux.requests()[0]
        .context
        .messages
        .iter()
        .find_map(|message| match message {
            Message::User(user) => Some(user.content.clone()),
            _ => None,
        })
        .unwrap();
    // As in Pi, the image that fits is sent resized, the one that cannot be used is left
    // out, and the text carries both notes.
    let images: Vec<&ImageContent> = user
        .iter()
        .filter_map(|block| match block {
            Content::Image(image) => Some(image),
            _ => None,
        })
        .collect();
    assert_eq!(images.len(), 1);
    let bytes = BASE64.decode(&images[0].data).unwrap();
    let (width, _) = image::load_from_memory(&bytes)
        .unwrap()
        .to_rgb8()
        .dimensions();
    assert!(width <= 2000, "{width}");
    let text = pi_ai::content_text(&user);
    assert!(text.starts_with("What is this?\n\n"), "{text}");
    assert!(text.contains("[Image omitted:"), "{text}");
}

#[tokio::test]
async fn session_stats_count_messages_and_add_up_usage() {
    let harness = Harness::new();
    let usage = |input, output, cost| pi_ai::Usage {
        input,
        output,
        cache_read: 5,
        cost: pi_ai::Cost {
            total: cost,
            ..pi_ai::Cost::default()
        },
        ..pi_ai::Usage::default()
    };
    let mut call = faux_message(vec![pi_ai::faux::faux_tool_call(
        "echo",
        json!({ "text": "hi" }),
    )]);
    call.usage = usage(100, 10, 0.5);
    let mut reply = faux_message(vec![AssistantContent::text("done")]);
    reply.usage = usage(120, 20, 0.25);
    harness.faux.push_reply(call);
    harness.faux.push_reply(reply);
    let session = harness
        .session_with(|options| options.tools = vec![echo_tool(true)])
        .await;
    session
        .prompt("say hi", PromptOptions::default())
        .await
        .unwrap();

    let stats = session.session_stats();
    assert_eq!(stats.session_id, session.session_id());
    assert_eq!(
        (
            stats.user_messages,
            stats.assistant_messages,
            stats.tool_calls,
            stats.tool_results,
            stats.total_messages
        ),
        (1, 2, 1, 1, 4)
    );
    assert_eq!(
        stats.tokens,
        pi_coding_agent::TokenTotals {
            input: 220,
            output: 30,
            cache_read: 10,
            cache_write: 0,
            total: 260,
        }
    );
    assert!((stats.cost - 0.75).abs() < 1e-9);
}

#[tokio::test]
async fn the_sessions_requests_carry_its_id_and_provider_settings() {
    let harness = Harness::new();
    harness.faux.push_text("one");
    harness.faux.push_text("two");
    let session = harness.session().await;
    session
        .prompt("hi", PromptOptions::default())
        .await
        .unwrap();
    let options = harness.faux.requests()[0].options.clone();
    assert_eq!(options.session_id, Some(session.session_id()));
    // Pi's five minutes for a response to start, and a minute's limit on a server's wait.
    assert_eq!(options.timeout_ms, Some(300_000));
    assert_eq!(options.max_retries, None);
    assert_eq!(options.max_retry_delay_ms, Some(60_000));

    let mut settings = harness.options().settings;
    settings.retry.provider.timeout_ms = Some(5_000);
    settings.retry.provider.max_retries = Some(2);
    settings.retry.provider.max_retry_delay_ms = 0;
    let mut options = harness.options();
    options.settings = settings;
    let session = AgentSession::new(options).await.unwrap();
    session
        .prompt("hi", PromptOptions::default())
        .await
        .unwrap();
    let options = harness.faux.requests()[1].options.clone();
    assert_eq!(options.timeout_ms, Some(5_000));
    assert_eq!(options.max_retries, Some(2));
    assert_eq!(options.max_retry_delay_ms, Some(0));
}

#[tokio::test]
async fn blocked_images_reach_the_model_as_a_note() {
    use base64::Engine;
    let mut png = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::new(2, 2))
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    let image = ImageContent {
        data: base64::engine::general_purpose::STANDARD.encode(png),
        mime_type: "image/png".into(),
    };
    let harness = Harness::new();
    harness.faux.push_text("one");
    let mut options = harness.options();
    options.settings.images.block_images = true;
    let session = AgentSession::new(options).await.unwrap();
    let prompt = PromptOptions {
        images: vec![image.clone(), image],
        ..PromptOptions::default()
    };
    session.prompt("Look", prompt).await.unwrap();

    let request = &harness.faux.requests()[0];
    let Some(Message::User(sent)) = request.context.messages.last() else {
        panic!("{:?}", request.context.messages.last())
    };
    // Two images in a row become one note.
    assert_eq!(
        sent.content,
        [
            Content::text("Look"),
            Content::text("Image reading is disabled.")
        ]
    );
    // The session keeps them.
    let kept = session
        .messages()
        .into_iter()
        .find_map(|message| match message {
            pi_coding_agent::SessionMessage::Llm(Message::User(user)) => Some(user),
            _ => None,
        });
    assert_eq!(kept.unwrap().content.len(), 3);
}

#[test]
fn a_model_can_have_its_own_compaction_token_settings() {
    let settings: pi_coding_agent::settings::Settings = serde_json::from_value(json!({
        "compaction": {
            "reserveTokens": 1000,
            "keepRecentTokens": 2000,
            "modelOverrides": { "faux/faux-1": { "keepRecentTokens": 50 } }
        }
    }))
    .unwrap();
    let model = FauxProvider::default_model();
    let own = settings.compaction.for_model(Some(&model));
    assert_eq!((own.reserve_tokens, own.keep_recent_tokens), (1000, 50));
    let other = settings.compaction.for_model(None);
    assert_eq!(
        (other.reserve_tokens, other.keep_recent_tokens),
        (1000, 2000)
    );
}

#[tokio::test]
async fn the_branch_exports_as_a_session_file_of_its_own() {
    let harness = Harness::new();
    harness.faux.push_text("one");
    harness.faux.push_text("two");
    let session = harness.session().await;
    session
        .prompt("first", PromptOptions::default())
        .await
        .unwrap();
    session
        .prompt("second", PromptOptions::default())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();

    let path = session
        .export_to_jsonl(Some(&dir.path().join("out/branch.jsonl")))
        .unwrap();
    let (header, entries) = JsonlStore::load(&path).unwrap().unwrap();
    assert_eq!(header.id, session.session_id());
    assert_eq!(header.parent_session, None);
    let mut parent = None;
    for entry in &entries {
        assert_eq!(entry.parent_id, parent);
        parent = Some(entry.id.clone());
    }
    let messages = entries
        .iter()
        .filter(|entry| matches!(entry.kind, EntryKind::Message { .. }))
        .count();
    // The system prompt and the two exchanges.
    assert_eq!(messages, 5);
}

#[tokio::test]
async fn settings_changed_in_a_session_are_saved_where_the_host_says() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("settings.json");
    std::fs::write(
        &file,
        json!({ "retry": { "maxRetries": 5 }, "theme": "dark" }).to_string(),
    )
    .unwrap();
    let harness = Harness::new();
    let mut options = harness.options();
    options.settings_file = Some(file.clone());
    let session = AgentSession::new(options).await.unwrap();

    session.set_auto_retry_enabled(false);
    session.set_auto_compaction_enabled(false);
    session.set_steering_mode(QueueMode::All);
    session.set_follow_up_mode(QueueMode::All);
    session
        .set_thinking_level_as_default(ThinkingLevel::High)
        .await;
    session.set_model_as_default(harness.faux.model()).await;
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let model = harness.faux.model();
    assert_eq!(
        saved,
        json!({
            "retry": { "maxRetries": 5, "enabled": false },
            "theme": "dark",
            "compaction": { "enabled": false },
            "steeringMode": "all",
            "followUpMode": "all",
            "defaultThinkingLevel": "high",
            "defaultProvider": model.provider,
            "defaultModel": model.id,
        })
    );
    let settings = session.settings();
    assert_eq!(settings.default_thinking_level, Some(ThinkingLevel::High));
    assert_eq!(settings.default_provider.as_deref(), Some(&*model.provider));
    assert_eq!(settings.default_model.as_deref(), Some(&*model.id));

    // Without a file, changes stay in the session.
    let session = harness.session().await;
    session.set_auto_retry_enabled(false);
    assert!(!session.auto_retry_enabled());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}
