mod common;

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use common::{Harness, echo_tool, entry_kinds, last_assistant_text, record, roles};
use pi_agent_core::AgentEvent;
use pi_ai::faux::{FauxProvider, faux_message};
use pi_ai::{AssistantContent, Content, ImageContent, Message, StopReason, ThinkingLevel, Usage};
use pi_coding_agent::extensions::CustomMessageDraft;
use pi_coding_agent::resources::{PromptTemplate, ResourceSource, Resources};
use pi_coding_agent::session::{EntryKind, SessionEntry, SessionHeader, SessionManager};
use pi_coding_agent::store::{JsonlStore, MemoryStore, SessionStore};
use pi_coding_agent::{
    AgentSession, AgentSessionError, AgentSessionEvent, PromptOptions, PromptOutcome,
    SessionMessage, StreamingBehavior,
};
use serde_json::json;

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
    assert_eq!(
        entry_kinds(&session),
        ["message:system", "message:user", "message:assistant"]
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
        entry_kinds(&session)[3..],
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
    // The tree keeps the failed attempt and the edit that hides it.
    assert_eq!(
        entry_kinds(&session),
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
            .contains(&"compaction_end:Overflow:true".to_string())
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
            .contains(&"compaction_end:Threshold:true".to_string())
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

    session.navigate_tree(&target, true, None).await.unwrap();
    let messages = session.messages();
    assert_eq!(
        roles(&messages),
        ["system", "user", "assistant", "branchSummary"]
    );
    assert!(messages[3].text().contains("explored a second question"));
    // The abandoned branch is still in the tree.
    assert_eq!(session.with_session(|tree| tree.children(&target).len()), 2);
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
        .fork(&fork_point, Box::new(MemoryStore))
        .await
        .unwrap();
    assert_ne!(session.session_id(), original);
    assert_eq!(last_assistant_text(&session), "first answer");
    assert_eq!(
        session.with_session(|tree| tree.header().parent_session.clone()),
        Some(original)
    );
}

#[tokio::test]
async fn the_host_can_steer_and_queue_follow_ups_during_a_run() {
    let harness = Harness::new();
    let faux = harness.faux.clone();
    faux.push_tool_call("echo", json!({ "text": "a" }));
    faux.push_text("steered");
    faux.push_text("followed up");
    let session = harness
        .session_with(|options| options.tools = vec![echo_tool(true)])
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
            host.steer(
                "use b",
                vec![ImageContent {
                    data: "aW1n".into(),
                    mime_type: "image/png".into(),
                }],
            );
            host.follow_up("then summarize", Vec::new());
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
    assert_eq!(texts, ["go", "use b", "then summarize"]);
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
    harness.faux.push_text("gone again");
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

    assert!(session.unregister_tool("echo"));
    assert!(!session.unregister_tool("echo"));
    assert!(session.active_tools().is_empty());
    session
        .prompt("three", PromptOptions::default())
        .await
        .unwrap();
    assert!(declared(3).is_empty());
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
    let session = harness.session().await;

    session.set_thinking_level(ThinkingLevel::High).await;
    assert_eq!(session.thinking_level(), ThinkingLevel::High);
    session.set_model(plain).await;
    assert_eq!(session.thinking_level(), ThinkingLevel::Off);
    assert_eq!(
        entry_kinds(&session),
        ["thinking_level_change", "model_change"]
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
    assert_eq!((usage.tokens, usage.context_window), (1_280, 128_000));
    assert!((usage.percent - 1.0).abs() < 0.001);
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
    assert!(matches!(
        session.compact(None).await,
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
            .contains(&"compaction_end:Manual:false".to_string())
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
            .contains(&"compaction_end:Overflow:true".to_string())
    );
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
        .fork(&fork_point, Box::new(ReadOnlyStore))
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
        tokio::spawn(async move { navigator.navigate_tree(&target, true, None).await });
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
        entry_kinds(&session)[3..],
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
