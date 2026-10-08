mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Harness, echo_tool, entry_kinds, last_assistant_text, record};
use pi_agent_core::{
    AfterToolCallResult, AgentEvent, AgentToolResult, BeforeToolCallResult, FnTool,
};
use pi_ai::faux::FauxProvider;
use pi_ai::{AssistantContent, Content, Message, Tool, content_text};
use pi_coding_agent::compaction::CompactionResult;
use pi_coding_agent::extensions::{
    AgentEventSeen, BeforeAgentStart, BeforeAgentStartResult, BeforeCompactResult, Context,
    CustomMessageDraft, Input, InputAction, MessageEnd, SessionBeforeCompact, SessionStart,
    ToolCall, ToolPrompt, ToolResult, extension,
};
use pi_coding_agent::session::{EntryKind, SessionManager};
use pi_coding_agent::store::JsonlStore;
use pi_coding_agent::{AgentSessionError, Delivery, PromptOptions, PromptOutcome, SessionMessage};
use serde_json::json;

fn tool_result_text(session: &pi_coding_agent::AgentSession) -> String {
    session
        .messages()
        .iter()
        .find_map(|message| match message {
            SessionMessage::Llm(Message::ToolResult(result)) => Some(content_text(&result.content)),
            _ => None,
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn tool_call_gates_block_and_a_failing_gate_fails_closed() {
    let harness = Harness::new();
    harness
        .faux
        .push_tool_call("echo", json!({ "text": "rm -rf /" }));
    harness.faux.push_text("understood");
    let gate = extension("policy", |api| {
        api.on(|event: ToolCall, _ctx| async move {
            let dangerous = event.input["text"]
                .as_str()
                .is_some_and(|text| text.contains("rm -rf"));
            Ok(dangerous.then(|| BeforeToolCallResult::block("Blocked by policy")))
        });
    });
    let session = harness
        .session_with(|options| {
            options.tools = vec![echo_tool(true)];
            options.extensions = vec![gate];
        })
        .await;
    session
        .prompt("go", PromptOptions::default())
        .await
        .unwrap();
    assert_eq!(tool_result_text(&session), "Blocked by policy");

    let harness = Harness::new();
    harness.faux.push_tool_call("echo", json!({ "text": "hi" }));
    harness.faux.push_text("ok");
    let broken = extension("broken", |api| {
        api.on(|_event: ToolCall, _ctx| async move {
            Err::<Option<BeforeToolCallResult>, _>("policy store unavailable".into())
        });
    });
    let session = harness
        .session_with(|options| {
            options.tools = vec![echo_tool(true)];
            options.extensions = vec![broken];
        })
        .await;
    session
        .prompt("go", PromptOptions::default())
        .await
        .unwrap();
    assert_eq!(tool_result_text(&session), "policy store unavailable");
}

#[tokio::test]
async fn gates_can_rewrite_input_and_result_rewrites_chain() {
    let harness = Harness::new();
    harness
        .faux
        .push_tool_call("echo", json!({ "text": "draft" }));
    harness.faux.push_text("done");
    let rewrite_input = extension("input", |api| {
        api.on(|_event: ToolCall, _ctx| async move {
            Ok(Some(BeforeToolCallResult::with_args(
                json!({ "text": "final" }),
            )))
        });
    });
    let first = extension("first", |api| {
        api.on(|event: ToolResult, _ctx| async move {
            let text = content_text(&event.content);
            Ok(Some(AfterToolCallResult {
                content: Some(vec![Content::text(format!("{text}+first"))]),
                ..AfterToolCallResult::default()
            }))
        });
    });
    let second = extension("second", |api| {
        api.on(|event: ToolResult, _ctx| async move {
            let text = content_text(&event.content);
            Ok(Some(AfterToolCallResult {
                content: Some(vec![Content::text(format!("{text}+second"))]),
                ..AfterToolCallResult::default()
            }))
        });
    });
    let session = harness
        .session_with(|options| {
            options.tools = vec![echo_tool(true)];
            options.extensions = vec![rewrite_input, first, second];
        })
        .await;
    session
        .prompt("go", PromptOptions::default())
        .await
        .unwrap();
    assert_eq!(tool_result_text(&session), "final+first+second");
}

#[tokio::test]
async fn message_end_replacements_are_recorded_and_kind_changes_rejected() {
    let harness = Harness::new();
    harness.faux.push_text("the secret is 42");
    let redact = extension("redact", |api| {
        api.on(|event: MessageEnd, _ctx| async move {
            Ok(match event.message {
                SessionMessage::Llm(Message::Assistant(mut reply))
                    if reply.text().contains("secret") =>
                {
                    reply.content = vec![AssistantContent::text("[redacted]")];
                    Some(SessionMessage::Llm(Message::Assistant(reply)))
                }
                SessionMessage::Llm(Message::User(_)) => {
                    Some(SessionMessage::Llm(Message::Assistant(
                        pi_ai::AssistantMessage::empty(&FauxProvider::default_model()),
                    )))
                }
                _ => None,
            })
        });
    });
    let session = harness.session_with_extensions(vec![redact]).await;
    let events = record(&session);
    session
        .prompt("tell me", PromptOptions::default())
        .await
        .unwrap();

    assert_eq!(last_assistant_text(&session), "[redacted]");
    let stored = session.with_session(|tree| {
        tree.entries()
            .iter()
            .rev()
            .find_map(|entry| match &entry.kind {
                EntryKind::Message { message } => Some(message.text()),
                _ => None,
            })
    });
    assert_eq!(stored.as_deref(), Some("[redacted]"));
    assert!(
        events
            .lock()
            .unwrap()
            .contains(&"extension_error:redact:message_end".to_string())
    );
}

#[tokio::test]
async fn input_handlers_transform_or_take_over_prompts() {
    let harness = Harness::new();
    harness.faux.push_text("ok");
    let input = extension("input", |api| {
        api.on(|event: Input, _ctx| async move {
            Ok(match event.text.as_str() {
                "ping" => InputAction::Handled,
                text => InputAction::Transform {
                    text: text.to_uppercase(),
                    images: event.images,
                },
            })
        });
    });
    let session = harness.session_with_extensions(vec![input]).await;
    assert_eq!(
        session
            .prompt("ping", PromptOptions::default())
            .await
            .unwrap(),
        PromptOutcome::Handled
    );
    assert!(harness.faux.requests().is_empty());
    session
        .prompt("hello", PromptOptions::default())
        .await
        .unwrap();
    assert_eq!(harness.sent_text(0), "HELLO");
}

#[tokio::test]
async fn commands_run_in_place_of_a_prompt_and_can_add_messages() {
    let harness = Harness::new();
    let seen = Arc::new(Mutex::new(String::new()));
    let record_args = seen.clone();
    let commands = extension("notes", move |api| {
        let record_args = record_args.clone();
        api.register_command("note", "Add a note", move |args, ctx| {
            let record_args = record_args.clone();
            async move {
                *record_args.lock().unwrap() = args.clone();
                ctx.send_message(
                    CustomMessageDraft {
                        custom_type: "note".into(),
                        content: vec![Content::text(args)],
                        display: true,
                        details: None,
                    },
                    Delivery::Append,
                );
                Ok(())
            }
        });
    });
    let session = harness.session_with_extensions(vec![commands]).await;
    assert_eq!(
        session.commands(),
        [("note".to_string(), "Add a note".to_string())]
    );
    assert_eq!(
        session
            .prompt("/note buy milk", PromptOptions::default())
            .await
            .unwrap(),
        PromptOutcome::Handled
    );
    assert_eq!(*seen.lock().unwrap(), "buy milk");
    assert!(harness.faux.requests().is_empty());
    assert_eq!(entry_kinds(&session), ["custom_message"]);

    harness.faux.push_text("noted");
    session
        .prompt("what did I note?", PromptOptions::default())
        .await
        .unwrap();
    let request = &harness.faux.requests()[0];
    let texts: Vec<String> = request
        .context
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::User(user) => Some(content_text(&user.content)),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["buy milk", "what did I note?"]);
}

#[tokio::test]
async fn before_agent_start_adds_messages_and_can_force_the_prompt_for_one_run() {
    let harness = Harness::new();
    harness.faux.push_text("ok");
    harness.faux.push_text("ok");
    let start = extension("start", |api| {
        api.on(|event: BeforeAgentStart, _ctx| async move {
            assert!(event.system_prompt.contains("inside Maple"));
            Ok(BeforeAgentStartResult {
                message: Some(CustomMessageDraft {
                    custom_type: "context".into(),
                    content: vec![Content::text("The user prefers metric units.")],
                    display: false,
                    details: None,
                }),
                system_prompt: (event.prompt == "forced")
                    .then(|| "You only answer in haiku.".to_string()),
                options: None,
            })
        });
    });
    let session = harness.session_with_extensions(vec![start]).await;
    session
        .prompt("forced", PromptOptions::default())
        .await
        .unwrap();
    let first = &harness.faux.requests()[0];
    assert_eq!(
        pi_ai::transcript::current_system_prompt(&first.context.messages),
        "You only answer in haiku."
    );
    assert!(first.context.messages.iter().any(|m| matches!(m, Message::User(user) if content_text(&user.content) == "The user prefers metric units.")));
    // The transcript keeps the regular prompt, and the next run uses it.
    assert!(session.system_prompt().contains("inside Maple"));
    session
        .prompt("normal", PromptOptions::default())
        .await
        .unwrap();
    let second = &harness.faux.requests()[1];
    assert!(
        pi_ai::transcript::current_system_prompt(&second.context.messages).contains("inside Maple")
    );
}

#[tokio::test]
async fn context_handlers_reshape_requests_without_touching_the_session() {
    let harness = Harness::new();
    harness.faux.push_text("one");
    harness.faux.push_text("two");
    let trim = extension("trim", |api| {
        api.on(|event: Context, _ctx| async move {
            let mut messages = event.messages;
            let last = messages.pop();
            messages.retain(|message| message.role() == "system");
            messages.extend(last);
            Ok(Some(messages))
        });
    });
    let session = harness.session_with_extensions(vec![trim]).await;
    session
        .prompt("first", PromptOptions::default())
        .await
        .unwrap();
    session
        .prompt("second", PromptOptions::default())
        .await
        .unwrap();
    let roles: Vec<&str> = harness.faux.requests()[1]
        .context
        .messages
        .iter()
        .map(Message::role)
        .collect();
    assert_eq!(roles, ["system", "user"]);
    assert_eq!(session.messages().len(), 5);
}

#[tokio::test]
async fn extensions_register_tools_and_providers() {
    let harness = Harness::new();
    let provider = FauxProvider::with_model({
        let mut model = FauxProvider::default_model();
        model.id = "ext-model".into();
        model.provider = "ext".into();
        model.api = "ext-api".into();
        model
    });
    provider.push_tool_call("lookup", json!({}));
    provider.push_text("served by the extension");
    let ext_provider = provider.clone();
    let tools = extension("tools", move |api| {
        let tool = FnTool::new(
            Tool::new("lookup", "Look things up", json!({ "type": "object" })),
            |_| async move { Ok(AgentToolResult::text("found it")) },
        );
        api.register_tool(
            tool.shared(),
            ToolPrompt {
                snippet: Some("Look things up".into()),
                guidelines: Vec::new(),
            },
            true,
        );
        api.register_provider(
            vec![ext_provider.model()],
            Some(("ext-api".into(), Arc::new(ext_provider.clone()))),
        );
    });
    let session = harness.session_with_extensions(vec![tools]).await;
    assert_eq!(session.tool_names(), ["lookup"]);
    assert_eq!(session.active_tools(), ["lookup"]);

    session.set_model(provider.model()).await;
    session
        .prompt("look it up", PromptOptions::default())
        .await
        .unwrap();
    assert_eq!(tool_result_text(&session), "found it");
    assert_eq!(last_assistant_text(&session), "served by the extension");
    assert!(harness.faux.requests().is_empty());
    assert!(session.system_prompt().contains("- lookup: Look things up"));
}

#[tokio::test]
async fn failing_observers_are_reported_and_the_run_goes_on() {
    let harness = Harness::new();
    harness.faux.push_text("fine");
    let flaky = extension("flaky", |api| {
        api.on(|event: AgentEventSeen, _ctx| async move {
            match event.0 {
                AgentEvent::AgentStart => Err("observer crashed".into()),
                _ => Ok(()),
            }
        });
    });
    let session = harness.session_with_extensions(vec![flaky]).await;
    let events = record(&session);
    session
        .prompt("go", PromptOptions::default())
        .await
        .unwrap();
    assert_eq!(last_assistant_text(&session), "fine");
    assert!(
        events
            .lock()
            .unwrap()
            .contains(&"extension_error:flaky:agent_event".to_string())
    );
}

#[tokio::test]
async fn extensions_can_cancel_or_supply_a_compaction() {
    let harness = Harness::new();
    let cancel = extension("cancel", |api| {
        api.on(|_event: SessionBeforeCompact, _ctx| async move {
            Ok(BeforeCompactResult {
                cancel: true,
                compaction: None,
            })
        });
    });
    let session = harness
        .session_with(|options| {
            options.settings.compaction.keep_recent_tokens = 1;
            options.extensions = vec![cancel];
        })
        .await;
    harness.faux.push_text(&"long reply ".repeat(50));
    session
        .prompt("question", PromptOptions::default())
        .await
        .unwrap();
    assert!(matches!(
        session.compact(None).await,
        Err(AgentSessionError::Cancelled)
    ));

    let harness = Harness::new();
    let supply = extension("supply", |api| {
        api.on(|event: SessionBeforeCompact, _ctx| async move {
            Ok(BeforeCompactResult {
                cancel: false,
                compaction: Some(CompactionResult {
                    summary: "summary from the extension".into(),
                    first_kept_entry_id: event.preparation.first_kept_entry_id,
                    tokens_before: event.preparation.tokens_before,
                    usage: Default::default(),
                    details: None,
                }),
            })
        });
    });
    let session = harness
        .session_with(|options| {
            options.settings.compaction.keep_recent_tokens = 1;
            options.extensions = vec![supply];
        })
        .await;
    harness.faux.push_text(&"long reply ".repeat(50));
    session
        .prompt("question", PromptOptions::default())
        .await
        .unwrap();
    let requests_before = harness.faux.requests().len();
    session.compact(None).await.unwrap();
    assert_eq!(harness.faux.requests().len(), requests_before);
    let from_extension = session.with_session(|tree| {
        tree.entries().iter().any(|entry| matches!(&entry.kind, EntryKind::Compaction { from_extension: true, summary, .. } if summary == "summary from the extension"))
    });
    assert!(from_extension);
}

#[tokio::test]
async fn context_actions_reach_the_session() {
    let harness = Harness::new();
    harness.faux.push_text("ok");
    let actions = extension("actions", |api| {
        api.on(|_event: SessionStart, ctx| async move {
            ctx.append_entry("state", Some(json!({ "count": 1 })));
            ctx.set_session_name("Planning");
            ctx.send_message(
                CustomMessageDraft {
                    custom_type: "reminder".into(),
                    content: vec![Content::text("Remember the deadline.")],
                    display: false,
                    details: None,
                },
                Delivery::NextTurn,
            );
            assert!(ctx.is_idle());
            assert_eq!(ctx.cwd().as_deref(), Some(std::path::Path::new("/work")));
            Ok(())
        });
    });
    let session = harness.session_with_extensions(vec![actions]).await;
    assert_eq!(
        session.with_session(|tree| tree.session_name().map(str::to_string)),
        Some("Planning".into())
    );
    session
        .prompt("plan my week", PromptOptions::default())
        .await
        .unwrap();
    let request = &harness.faux.requests()[0];
    assert!(request.context.messages.iter().any(|m| matches!(m, Message::User(user) if content_text(&user.content) == "Remember the deadline.")));
    // Extension state stays out of the model context.
    assert!(
        !session
            .messages()
            .iter()
            .any(|message| message.text().contains("count"))
    );
    assert_eq!(entry_kinds(&session)[..2], ["custom", "session_info"]);
}

#[tokio::test]
async fn a_user_message_from_an_extension_is_a_full_prompt() {
    let harness = Harness::new();
    harness.faux.push_text("reviewed");
    let review = extension("review", |api| {
        api.register_command("review", "Review the diff", |_args, ctx| async move {
            ctx.send_user_message("Review the diff", Delivery::Steer);
            Ok(())
        });
    });
    let session = harness.session_with_extensions(vec![review]).await;
    session
        .prompt("/review", PromptOptions::default())
        .await
        .unwrap();
    while harness.faux.requests().is_empty() {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    session.wait_for_idle().await;

    // A fresh session's first run still declares the system prompt.
    let request = &harness.faux.requests()[0];
    assert!(
        pi_ai::transcript::current_system_prompt(&request.context.messages)
            .contains("inside Maple")
    );
    assert_eq!(harness.sent_text(0), "Review the diff");
    assert_eq!(last_assistant_text(&session), "reviewed");
}

#[tokio::test]
async fn extension_messages_queued_during_a_run_are_not_listed() {
    let harness = Harness::new();
    harness.faux.push_hang();
    let session = harness.session().await;
    let runner = session.clone();
    let run = tokio::spawn(async move { runner.prompt("go", PromptOptions::default()).await });
    while harness.faux.requests().is_empty() {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    session.extension_context().send_message(
        CustomMessageDraft {
            custom_type: "note".into(),
            content: vec![Content::text("from an extension")],
            display: true,
            details: None,
        },
        Delivery::Steer,
    );
    assert_eq!(session.clear_queue(), (Vec::new(), Vec::new()));
    session.abort();
    run.await.unwrap().unwrap();
}

fn provider_extension(provider: FauxProvider) -> Arc<dyn pi_coding_agent::extensions::Extension> {
    extension("provider", move |api| {
        api.register_provider(
            vec![provider.model()],
            Some(("ext-api".into(), Arc::new(provider.clone()))),
        );
    })
}

#[tokio::test]
async fn a_session_resumes_with_a_model_an_extension_provides() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let harness = Harness::new();
    let provider = FauxProvider::with_model({
        let mut model = FauxProvider::default_model();
        model.id = "ext-model".into();
        model.provider = "ext".into();
        model.api = "ext-api".into();
        model
    });
    provider.push_text("served by the extension");
    let first = harness
        .session_with(|options| {
            options.extensions = vec![provider_extension(provider.clone())];
            options.session = SessionManager::create("/work", Box::new(JsonlStore::new(&path)));
        })
        .await;
    first.set_model(provider.model()).await;
    first.prompt("hi", PromptOptions::default()).await.unwrap();

    // The host passes its default model, as it does for every session.
    let (header, entries) = JsonlStore::load(&path).unwrap().unwrap();
    let resumed = harness
        .session_with(|options| {
            options.extensions = vec![provider_extension(provider.clone())];
            options.session =
                SessionManager::open(header, entries, Box::new(JsonlStore::new(&path)));
        })
        .await;
    assert_eq!(
        resumed.model().map(|model| model.id),
        Some("ext-model".to_string())
    );
}

#[tokio::test]
async fn prompt_options_that_cannot_build_a_prompt_are_refused() {
    let harness = Harness::new();
    harness.faux.push_text("one");
    harness.faux.push_text("two");
    let broken = extension("sections", |api| {
        api.on(|event: BeforeAgentStart, _ctx| async move {
            let mut options = event.options;
            options
                .sections
                .insert("Project Notes".into(), "notes".into());
            Ok(BeforeAgentStartResult {
                options: Some(options),
                ..BeforeAgentStartResult::default()
            })
        });
    });
    let session = harness.session_with_extensions(vec![broken]).await;
    let events = record(&session);
    for text in ["first", "second"] {
        assert_eq!(
            session
                .prompt(text, PromptOptions::default())
                .await
                .unwrap(),
            PromptOutcome::Completed
        );
    }
    assert!(
        events
            .lock()
            .unwrap()
            .contains(&"extension_error:sections:before_agent_start".to_string())
    );
}
