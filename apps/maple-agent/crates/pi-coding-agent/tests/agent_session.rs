mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use common::{Harness, echo_tool, entry_kinds, last_assistant_text, record, roles};
use pi_agent_core::AgentEvent;
use pi_ai::faux::{FauxProvider, faux_message};
use pi_ai::{AssistantContent, Message, StopReason, ThinkingLevel, Usage};
use pi_coding_agent::resources::{PromptTemplate, ResourceSource, Resources};
use pi_coding_agent::session::{EntryKind, SessionManager};
use pi_coding_agent::store::{JsonlStore, MemoryStore};
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
            host.steer("use b");
            host.follow_up("then summarize");
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
    let events = events.lock().unwrap();
    assert!(events.contains(&"queue:1:1".to_string()));
    assert!(events.contains(&"queue:0:0".to_string()));
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
                    },
                    std::slice::from_ref(&skill),
                    &[],
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
