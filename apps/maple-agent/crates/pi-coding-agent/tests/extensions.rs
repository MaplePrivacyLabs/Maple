mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Harness, echo_tool, entry_kinds, last_assistant_text, record};
use pi_agent_core::{
    AfterToolCallResult, AgentEvent, AgentToolResult, BeforeToolCallResult, FnTool,
};
use pi_ai::faux::{FauxProvider, faux_message};
use pi_ai::{AssistantContent, Content, ImageContent, Message, ThinkingLevel, Tool, content_text};
use pi_coding_agent::compaction::CompactionResult;
use pi_coding_agent::extensions::{
    AfterProviderResponse, AgentEventSeen, AgentSettled, BeforeAgentStart, BeforeAgentStartResult,
    BeforeCompactResult, BeforeProviderHeaders, Context, ContextWithSystem, CustomMessageDraft,
    Input, InputAction, MessageEnd, ProviderStreamEvent, RunTrigger, SessionBeforeCompact,
    SessionCompact, SessionCompactFailed, SessionInfoChanged, SessionStart, SlashCommandInfo,
    SlashCommandSource, ToolCall, ToolPrompt, ToolResult, extension,
};
use pi_coding_agent::session::{EntryKind, SessionManager};
use pi_coding_agent::store::JsonlStore;
use pi_coding_agent::{AgentSessionError, Delivery, PromptOptions, PromptOutcome, SessionMessage};
use serde_json::json;

/// A small PNG, which the session sends on as it is.
fn tiny_png() -> String {
    use base64::Engine;
    let mut png = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::new(2, 2))
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    base64::engine::general_purpose::STANDARD.encode(png)
}

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
        [SlashCommandInfo {
            name: "note".into(),
            description: "Add a note".into(),
            source: SlashCommandSource::Extension,
        }]
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
    // After the model and thinking level the new session started with.
    assert_eq!(entry_kinds(&session)[2..], ["custom_message"]);

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
                system_prompt:
                    matches!(&event.trigger, RunTrigger::User { prompt, .. } if prompt == "forced")
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
        api.on(|event: ContextWithSystem, _ctx| async move {
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
async fn context_handlers_see_the_conversation_and_keep_the_prompt() {
    let harness = Harness::new();
    harness.faux.push_text("one");
    harness.faux.push_text("two");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let roles_seen = seen.clone();
    let trim = extension("trim", move |api| {
        let roles_seen = roles_seen.clone();
        api.on(move |event: Context, _ctx| {
            let roles_seen = roles_seen.clone();
            async move {
                roles_seen.lock().unwrap().push(
                    event
                        .messages
                        .iter()
                        .map(SessionMessage::role)
                        .collect::<Vec<_>>(),
                );
                // Dropping everything but the newest message cannot drop the prompt.
                Ok(event.messages.last().cloned().map(|last| vec![last]))
            }
        });
    });
    let session = harness
        .session_with(|options| {
            options.tools = vec![echo_tool(true)];
            options.extensions = vec![trim];
        })
        .await;
    session
        .prompt("first", PromptOptions::default())
        .await
        .unwrap();
    session
        .prompt("second", PromptOptions::default())
        .await
        .unwrap();
    // As in Pi, the handler sees no system messages.
    assert_eq!(
        *seen.lock().unwrap(),
        [vec!["user"], vec!["user", "assistant", "user"]]
    );
    let request = &harness.faux.requests()[1].context.messages;
    let roles: Vec<&str> = request.iter().map(Message::role).collect();
    assert_eq!(roles, ["system", "user"]);
    assert!(pi_ai::transcript::current_system_prompt(request).contains("inside Maple"));
    assert_eq!(pi_ai::transcript::current_tools(request)[0].name, "echo");
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
    // The built-in tools stay registered, turned off.
    let lookup = session
        .all_tools()
        .into_iter()
        .find(|tool| tool.name == "lookup")
        .unwrap();
    assert!(lookup.active);
    assert_eq!(lookup.extension.as_deref(), Some("tools"));
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
                    estimated_tokens_after: None,
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
async fn names_entries_and_settling_are_reported_as_in_pi() {
    let harness = Harness::new();
    harness.faux.push_hang();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let observer = extension("observer", move |api| {
        let (names, settles) = (sink.clone(), sink.clone());
        api.on(move |event: SessionInfoChanged, _ctx| {
            let names = names.clone();
            async move {
                names.lock().unwrap().push(format!("name:{:?}", event.name));
                Ok(())
            }
        });
        api.on(move |event: AgentSettled, _ctx| {
            let settles = settles.clone();
            async move {
                settles
                    .lock()
                    .unwrap()
                    .push(format!("settled:{}", event.aborted));
                Ok(())
            }
        });
    });
    let session = harness.session_with_extensions(vec![observer]).await;
    let events = record(&session);

    session.set_session_name("Planning").await;
    assert_eq!(session.session_name().as_deref(), Some("Planning"));
    session
        .extension_context()
        .append_entry("state", Some(json!({ "count": 1 })));
    let runner = session.clone();
    let run = tokio::spawn(async move { runner.prompt("go", PromptOptions::default()).await });
    while harness.faux.requests().is_empty() {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    session.abort();
    run.await.unwrap().unwrap();

    assert_eq!(
        *seen.lock().unwrap(),
        ["name:Some(\"Planning\")", "settled:true"]
    );
    let events = events.lock().unwrap();
    assert_eq!(events[..2], ["name:Planning", "entry:custom"]);
    assert_eq!(events.last().map(String::as_str), Some("settled:aborted"));
}

#[tokio::test]
async fn handlers_and_commands_get_pis_contexts() {
    let harness = Harness::new();
    harness.faux.push_tool_call("echo", json!({ "text": "a" }));
    harness.faux.push_text("done");
    for turn in 0..2 {
        harness
            .faux
            .push_text(&format!("reply {turn} {}", "x".repeat(400)));
    }
    harness.faux.push_text("## Goal\nsummary");
    harness.faux.push_text("## Original Request\nsummary");
    let signals = Arc::new(Mutex::new(Vec::new()));
    let seen = signals.clone();
    let contexts = extension("contexts", move |api| {
        let seen = seen.clone();
        api.on(move |_event: ToolCall, ctx| {
            let seen = seen.clone();
            async move {
                // A handler inside a run sees its cancellation, as Pi's signal.
                seen.lock()
                    .unwrap()
                    .push(ctx.signal().is_some_and(|signal| !signal.is_cancelled()));
                Ok(None)
            }
        });
        api.register_command("inspect", "Inspect the session", |_args, ctx| async move {
            assert!(ctx.is_project_trusted());
            assert!(ctx.models().unwrap().find("faux", "faux-1").is_some());
            assert_eq!(ctx.system_prompt_options().app_name, "Maple");
            assert!(ctx.signal().is_none());
            // A command can wait for the session; the compaction it starts stops nothing.
            ctx.wait_for_idle().await;
            let result = ctx.compact(None).await.unwrap()?;
            assert!(result.summary.starts_with("## Goal"));
            Ok(())
        });
    });
    let session = harness
        .session_with(|options| {
            options.tools = vec![echo_tool(true)];
            options.extensions = vec![contexts];
            options.project_trusted = true;
            options.settings.compaction.keep_recent_tokens = 50;
        })
        .await;
    for text in ["go", "question 0", "question 1"] {
        session
            .prompt(text, PromptOptions::default())
            .await
            .unwrap();
    }
    assert_eq!(*signals.lock().unwrap(), [true]);
    assert_eq!(
        session
            .prompt("/inspect", PromptOptions::default())
            .await
            .unwrap(),
        PromptOutcome::Handled
    );
    assert_eq!(roles_of(&session)[..2], ["system", "compactionSummary"]);
}

fn roles_of(session: &pi_coding_agent::AgentSession) -> Vec<&'static str> {
    session
        .messages()
        .iter()
        .map(SessionMessage::role)
        .collect()
}

/// An extension that writes every compaction itself, recording what it is told.
fn compacting_extension(
    seen: Arc<Mutex<Vec<String>>>,
    cancel: bool,
) -> Arc<dyn pi_coding_agent::extensions::Extension> {
    extension("compactor", move |api| {
        let (before, after, failed) = (seen.clone(), seen.clone(), seen.clone());
        api.on(move |event: SessionBeforeCompact, _ctx| {
            let before = before.clone();
            async move {
                before.lock().unwrap().push(format!(
                    "before:{:?}:{}:{}",
                    event.reason,
                    event.will_retry,
                    !event.branch_entries.is_empty()
                ));
                Ok(BeforeCompactResult {
                    cancel,
                    compaction: Some(CompactionResult {
                        summary: "summary from the extension".into(),
                        first_kept_entry_id: event.preparation.first_kept_entry_id.clone(),
                        tokens_before: event.preparation.tokens_before,
                        estimated_tokens_after: None,
                        usage: Default::default(),
                        details: None,
                    }),
                })
            }
        });
        api.on(move |event: SessionCompact, _ctx| {
            let after = after.clone();
            async move {
                after
                    .lock()
                    .unwrap()
                    .push(format!("compact:{:?}:{}", event.reason, event.will_retry));
                Ok(())
            }
        });
        api.on(move |event: SessionCompactFailed, _ctx| {
            let failed = failed.clone();
            async move {
                failed.lock().unwrap().push(format!(
                    "failed:{:?}:{}:{}",
                    event.reason,
                    event.aborted,
                    event.error_message.unwrap_or_default()
                ));
                Ok(())
            }
        });
    })
}

/// Two turns of about a hundred tokens each, to compact.
async fn history(harness: &Harness, session: &pi_coding_agent::AgentSession) {
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
async fn a_reply_cut_short_is_compacted_and_asked_again_once() {
    let harness = Harness::new();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let session = harness
        .session_with(|options| {
            options.extensions = vec![compacting_extension(seen.clone(), false)];
            options.settings.compaction.keep_recent_tokens = 50;
        })
        .await;
    history(&harness, &session).await;
    let events = record(&session);
    // Cut short well below the model's output limit, twice.
    let cut = || {
        let mut reply = faux_message(vec![AssistantContent::text("partial")]);
        reply.stop_reason = pi_ai::StopReason::Length;
        reply.usage.output = 100;
        reply
    };
    harness.faux.push_reply(cut());
    harness.faux.push_reply(cut());
    session
        .prompt("question 2", PromptOptions::default())
        .await
        .unwrap();

    // As in Pi, the first is compacted and asked again; the second is reported.
    assert_eq!(
        *seen.lock().unwrap(),
        [
            "before:Overflow:true:true",
            "compact:Overflow:true",
            "failed:Overflow:false:Truncated response recovery failed after one compact-and-retry attempt."
        ]
    );
    let events = events.lock().unwrap();
    assert!(events.contains(&"compaction_end:Overflow:ok:retry".to_string()));
    assert!(events.contains(&"compaction_end:Overflow:failed".to_string()));
    assert_eq!(harness.faux.requests().len(), 4);
}

#[tokio::test]
async fn a_prompt_after_a_switch_to_a_smaller_model_compacts_first() {
    let harness = Harness::new();
    let mut small = FauxProvider::default_model();
    small.id = "small".into();
    // Just over the default reserve, so a few hundred tokens are over the threshold.
    small.context_window = 16_500;
    harness.models.register_models([small.clone()]);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let session = harness
        .session_with(|options| {
            options.extensions = vec![compacting_extension(seen.clone(), false)];
            options.settings.compaction.keep_recent_tokens = 50;
        })
        .await;
    history(&harness, &session).await;
    assert!(seen.lock().unwrap().is_empty());

    // The context fits the old window but not the new one's threshold, so the prompt
    // compacts first, as Pi's prompt checks before it sends.
    session.set_model(small).await;
    harness.faux.push_text("answered");
    session
        .prompt("question 2", PromptOptions::default())
        .await
        .unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        ["before:Threshold:false:true", "compact:Threshold:false"]
    );
    // The request for the new prompt already carries the summary.
    let request = &harness.faux.requests()[2].context.messages;
    assert!(request.iter().any(|message| matches!(
        message,
        Message::User(user) if content_text(&user.content).contains("summary from the extension")
    )));
    assert!(harness.sent_text(2).contains("question 2"));
}

#[tokio::test]
async fn a_cancelled_compaction_is_reported_as_stopped() {
    let harness = Harness::new();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let session = harness
        .session_with(|options| {
            options.extensions = vec![compacting_extension(seen.clone(), true)];
            options.settings.compaction.keep_recent_tokens = 50;
        })
        .await;
    history(&harness, &session).await;
    let events = record(&session);
    assert!(matches!(
        session.compact(None).await,
        Err(AgentSessionError::Cancelled)
    ));
    assert_eq!(
        *seen.lock().unwrap(),
        ["before:Manual:false:true", "failed:Manual:true:"]
    );
    assert!(
        events
            .lock()
            .unwrap()
            .contains(&"compaction_end:Manual:aborted".to_string())
    );
}

#[tokio::test]
async fn nothing_to_compact_automatically_reports_nothing() {
    // A tiny window puts every reply over the threshold, but there is nothing to cut.
    let mut small = FauxProvider::default_model();
    small.context_window = 10;
    let harness = Harness::with_model(small);
    harness.faux.push_text("short");
    let session = harness.session().await;
    let events = record(&session);
    session
        .prompt("hi", PromptOptions::default())
        .await
        .unwrap();
    assert!(
        !events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event.starts_with("compaction"))
    );
}

#[tokio::test]
async fn context_actions_reach_the_session() {
    let harness = Harness::new();
    harness.faux.push_text("ok");
    let actions = extension("actions", |api| {
        api.on(|_event: SessionStart, ctx| async move {
            ctx.append_entry("state", Some(json!({ "count": 1 })));
            ctx.set_session_name("Planning").await;
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
    assert_eq!(entry_kinds(&session)[2..4], ["custom", "session_info"]);
}

#[tokio::test]
async fn a_user_message_from_an_extension_is_a_full_prompt() {
    let harness = Harness::new();
    harness.faux.push_text("reviewed");
    let review = extension("review", |api| {
        api.register_command("review", "Review the diff", |_args, ctx| async move {
            let screenshot = ImageContent {
                data: tiny_png(),
                mime_type: "image/png".into(),
            };
            ctx.send_user_message("Review the diff", vec![screenshot], Delivery::Steer);
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
    // Its images go with it, as with Pi's sendUserMessage.
    assert!(request.context.messages.iter().any(|message| matches!(
        message,
        Message::User(user) if matches!(user.content.last(), Some(Content::Image(image)) if image.data == tiny_png())
    )));
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

fn custom_draft(custom_type: &str, text: &str) -> CustomMessageDraft {
    CustomMessageDraft {
        custom_type: custom_type.into(),
        content: vec![Content::text(text)],
        display: true,
        details: None,
    }
}

/// The text of every user message of request `index`, in order.
fn user_texts(harness: &Harness, index: usize) -> Vec<String> {
    harness.faux.requests()[index]
        .context
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::User(user) => Some(content_text(&user.content)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_run_a_custom_message_starts_keeps_the_prompt_for_every_request() {
    // Pi #10739: such a run skipped `before_agent_start`, so its next request patched away
    // the sections the handlers had added.
    let harness = Harness::new();
    harness.faux.push_text("hi");
    harness.faux.push_tool_call("echo", json!({ "text": "a" }));
    harness.faux.push_text("done");
    let triggers = Arc::new(Mutex::new(Vec::new()));
    let seen = triggers.clone();
    let rules = extension("rules", move |api| {
        let seen = seen.clone();
        api.on(move |event: BeforeAgentStart, _ctx| {
            seen.lock().unwrap().push(event.trigger.clone());
            async move {
                let mut options = event.options;
                options
                    .sections
                    .insert("extension_rules".into(), "Answer in one word.".into());
                Ok(BeforeAgentStartResult {
                    options: Some(options),
                    ..BeforeAgentStartResult::default()
                })
            }
        });
    });
    let session = harness
        .session_with(|options| {
            options.tools = vec![echo_tool(true)];
            options.extensions = vec![rules];
        })
        .await;
    session
        .prompt("hello", PromptOptions::default())
        .await
        .unwrap();
    session
        .send_custom_message(custom_draft("task-done", "background task finished"))
        .await
        .unwrap();

    let prompts: Vec<String> = harness
        .faux
        .requests()
        .iter()
        .map(|request| pi_ai::transcript::current_system_prompt(&request.context.messages))
        .collect();
    assert_eq!(prompts.len(), 3);
    assert!(prompts[0].contains("Answer in one word."));
    assert!(prompts.iter().all(|prompt| *prompt == prompts[0]));
    let triggers = triggers.lock().unwrap();
    assert!(matches!(&triggers[0], RunTrigger::User { prompt, .. } if prompt == "hello"));
    assert!(
        matches!(&triggers[1], RunTrigger::Custom(message) if message.custom_type == "task-done")
    );
}

/// An extension whose `before_agent_start` handler waits for `release` when `blocks`
/// says so, after telling `started`.
fn blocking_start(
    blocks: fn(&RunTrigger) -> bool,
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
) -> Arc<dyn pi_coding_agent::extensions::Extension> {
    extension("blocking", move |api| {
        let (started, release) = (started.clone(), release.clone());
        api.on(move |event: BeforeAgentStart, _ctx| {
            let block = blocks(&event.trigger);
            let (started, release) = (started.clone(), release.clone());
            async move {
                if block {
                    started.notify_one();
                    release.notified().await;
                }
                Ok(BeforeAgentStartResult::default())
            }
        });
    })
}

#[tokio::test]
async fn a_custom_message_sent_while_a_run_starts_joins_it() {
    let harness = Harness::new();
    harness.faux.push_text("handled both");
    let (started, release) = (
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
    );
    let blocking = blocking_start(
        |trigger| matches!(trigger, RunTrigger::Custom(message) if message.custom_type == "first"),
        started.clone(),
        release.clone(),
    );
    let session = harness.session_with_extensions(vec![blocking]).await;
    let runner = session.clone();
    let first = tokio::spawn(async move {
        runner
            .send_custom_message(custom_draft("first", "task one finished"))
            .await
    });
    started.notified().await;
    assert!(session.is_streaming());
    assert_eq!(
        session
            .send_custom_message(custom_draft("second", "task two finished"))
            .await
            .unwrap(),
        PromptOutcome::Queued
    );
    release.notify_one();
    first.await.unwrap().unwrap();

    assert_eq!(harness.faux.requests().len(), 1);
    assert_eq!(
        user_texts(&harness, 0),
        ["task one finished", "task two finished"]
    );
}

#[tokio::test]
async fn a_message_added_while_a_run_starts_is_in_its_first_request() {
    let harness = Harness::new();
    harness.faux.push_text("done");
    let note = extension("note", |api| {
        api.on(|_event: BeforeAgentStart, ctx| {
            ctx.send_message(
                custom_draft("note", "context from handler"),
                Delivery::Append,
            );
            async { Ok(BeforeAgentStartResult::default()) }
        });
    });
    let session = harness.session_with_extensions(vec![note]).await;
    session
        .prompt("hello", PromptOptions::default())
        .await
        .unwrap();

    assert_eq!(harness.faux.requests().len(), 1);
    // It goes before the prompt, as it would have while idle.
    assert_eq!(user_texts(&harness, 0), ["context from handler", "hello"]);
}

#[tokio::test]
async fn an_abort_while_a_run_starts_stops_it_before_its_first_request() {
    let harness = Harness::new();
    harness.faux.push_text("should not complete");
    let (started, release) = (
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
    );
    let blocking = blocking_start(|_| true, started.clone(), release.clone());
    let session = harness.session_with_extensions(vec![blocking]).await;
    let events = record(&session);
    let runner = session.clone();
    let run = tokio::spawn(async move {
        runner
            .send_custom_message(custom_draft("task-done", "background task finished"))
            .await
    });
    started.notified().await;
    session.abort();
    release.notify_one();
    run.await.unwrap().unwrap();

    let last = session.messages().pop().unwrap();
    assert!(matches!(
        last,
        SessionMessage::Llm(Message::Assistant(reply)) if reply.stop_reason == pi_ai::StopReason::Aborted
    ));
    assert!(!session.is_streaming());
    assert!(
        events
            .lock()
            .unwrap()
            .contains(&"settled:aborted".to_string())
    );
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
async fn prompt_options_from_before_agent_start_last_for_one_run() {
    let harness = Harness::new();
    harness.faux.push_tool_call("echo", json!({ "text": "a" }));
    harness.faux.push_text("one");
    harness.faux.push_text("two");
    let notes = extension("notes", |api| {
        api.on(|event: BeforeAgentStart, _ctx| async move {
            if !matches!(&event.trigger, RunTrigger::User { prompt, .. } if prompt == "with notes")
            {
                return Ok(BeforeAgentStartResult::default());
            }
            let mut options = event.options;
            options
                .sections
                .insert("notes".into(), "Remember the deadline.".into());
            // Listing other tools changes which are active, beyond this run.
            options.tools.retain(|tool| tool.name != "echo");
            Ok(BeforeAgentStartResult {
                options: Some(options),
                ..BeforeAgentStartResult::default()
            })
        });
    });
    let session = harness
        .session_with(|options| {
            options.tools = vec![echo_tool(true), echo_tool(true)];
            options.tools[1].tool = FnTool::new(
                Tool::new("other", "Other", json!({ "type": "object" })),
                |_| async { Ok(AgentToolResult::text("other")) },
            )
            .shared();
            options.extensions = vec![notes];
        })
        .await;
    session
        .prompt("with notes", PromptOptions::default())
        .await
        .unwrap();
    let prompt = |index: usize| {
        pi_ai::transcript::current_system_prompt(&harness.faux.requests()[index].context.messages)
    };
    // Every request of the run has the run's prompt.
    assert!(prompt(0).contains("Remember the deadline."));
    assert!(prompt(1).contains("Remember the deadline."));
    session
        .prompt("plain", PromptOptions::default())
        .await
        .unwrap();
    assert!(!prompt(2).contains("Remember the deadline."));
    assert_eq!(session.active_tools(), ["other"]);
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

#[tokio::test]
async fn changes_made_during_a_run_reach_its_next_request() {
    let harness = Harness::new();
    let mut other = FauxProvider::default_model();
    other.id = "faux-2".into();
    harness.models.register_models([other.clone()]);
    harness.faux.push_tool_call("echo", json!({ "text": "a" }));
    harness.faux.push_text("done");
    let switch = extension("switch", move |api| {
        let other = other.clone();
        api.on(move |_event: ToolCall, ctx| {
            let other = other.clone();
            async move {
                ctx.set_active_tools(&[]);
                ctx.set_model(other).await;
                ctx.set_thinking_level(ThinkingLevel::High).await;
                Ok(None)
            }
        });
    });
    let session = harness
        .session_with(|options| {
            options.tools = vec![echo_tool(true)];
            options.extensions = vec![switch];
        })
        .await;
    session
        .prompt("go", PromptOptions::default())
        .await
        .unwrap();

    // The tool call that triggered the changes still ran; the next request saw them.
    assert_eq!(tool_result_text(&session), "a");
    let requests = harness.faux.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].model.id, "faux-2");
    assert_eq!(requests[1].options.reasoning, Some(ThinkingLevel::High));
    assert!(pi_ai::transcript::current_tools(&requests[1].context.messages).is_empty());
    assert!(
        !pi_ai::transcript::current_system_prompt(&requests[1].context.messages)
            .contains("- echo:")
    );
}

#[tokio::test]
async fn an_extension_registers_tools_it_finds_later() {
    let harness = Harness::new();
    harness.faux.push_text("nothing yet");
    harness
        .faux
        .push_tool_call("echo", json!({ "text": "found" }));
    harness.faux.push_text("echoed");
    let kept = Arc::new(Mutex::new(None));
    let keep = {
        let kept = Arc::clone(&kept);
        extension("finder", move |api| {
            *kept.lock().unwrap() = Some(api.context());
        })
    };
    let session = harness
        .session_with(|options| options.extensions = vec![keep])
        .await;
    session
        .prompt("one", PromptOptions::default())
        .await
        .unwrap();

    let context = kept.lock().unwrap().clone().unwrap();
    let echo = echo_tool(true);
    context.register_tool(echo.tool, echo.prompt, true);
    assert!(context.all_tools().iter().any(|tool| tool.name == "echo"));
    assert!(session.active_tools().contains(&"echo".to_string()));
    session
        .prompt("two", PromptOptions::default())
        .await
        .unwrap();
    assert_eq!(tool_result_text(&session), "found");
    assert_eq!(
        pi_ai::transcript::current_tools(&harness.faux.requests()[1].context.messages)
            .iter()
            .filter(|tool| tool.name == "echo")
            .count(),
        1
    );
}

#[tokio::test]
async fn provider_events_see_and_change_the_sessions_requests() {
    let harness = Harness::new();
    harness.faux.push_text("one");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let watcher = extension("watcher", move |api| {
        api.on(|event: BeforeProviderHeaders, _ctx| async move {
            let mut headers = event.headers;
            headers.insert("x-trace".into(), Some("abc".into()));
            headers.insert("x-drop".into(), None);
            Ok(Some(headers))
        });
        let responses = log.clone();
        api.on(move |event: AfterProviderResponse, _ctx| {
            let responses = responses.clone();
            async move {
                responses
                    .lock()
                    .unwrap()
                    .push(format!("response {} {:?}", event.status, event.headers));
                Ok(())
            }
        });
        let events = log.clone();
        api.on(move |event: ProviderStreamEvent, _ctx| {
            let events = events.clone();
            async move {
                events.lock().unwrap().push(format!(
                    "event {}/{} {} {}",
                    event.provider, event.model, event.api, event.data
                ));
                Ok(())
            }
        });
    });
    let session = harness.session_with_extensions(vec![watcher]).await;
    session
        .prompt("hi", PromptOptions::default())
        .await
        .unwrap();

    // The faux provider calls no hooks; call them as a provider would.
    let options = harness.faux.requests()[0].options.clone();
    let headers = (options.transform_headers.expect("a header hook"))(
        [("x-keep".to_string(), Some("1".to_string()))].into(),
    )
    .await;
    assert_eq!(
        headers,
        [
            ("x-drop".to_string(), None),
            ("x-keep".to_string(), Some("1".to_string())),
            ("x-trace".to_string(), Some("abc".to_string())),
        ]
        .into()
    );
    (options.on_response.expect("a response hook"))(pi_ai::ProviderResponse {
        status: 200,
        headers: [("x-request-id".to_string(), "r1".to_string())].into(),
    })
    .await;
    (options.on_provider_stream_event.expect("an event hook"))(json!({ "id": "c1" })).await;
    let model = harness.faux.model();
    assert_eq!(
        *seen.lock().unwrap(),
        [
            "response 200 {\"x-request-id\": \"r1\"}".to_string(),
            format!(
                "event {}/{} {} {}",
                model.provider, model.id, model.api, "{\"id\":\"c1\"}"
            ),
        ]
    );
    // Without handlers, no hooks are set.
    let plain = Harness::new();
    plain.faux.push_text("one");
    let session = plain.session().await;
    session
        .prompt("hi", PromptOptions::default())
        .await
        .unwrap();
    let options = plain.faux.requests()[0].options.clone();
    assert!(options.transform_headers.is_none());
    assert!(options.on_response.is_none());
    assert!(options.on_provider_stream_event.is_none());
    assert!(options.on_payload.is_none());
}

#[tokio::test]
async fn extensions_register_classifiers_that_handlers_can_ask() {
    use pi_ai::faux::FauxClassifier;
    use pi_ai::{
        Calibration, ClassifierAnswer, ClassifierContext, ClassifierOptions, ClassifierQuestion,
    };
    use pi_coding_agent::{ModelRegistry, StaticKeys};

    let classifier = FauxClassifier::new();
    classifier.push([(
        "urgent",
        ClassifierAnswer::Bool {
            probability: 0.9,
            calibration: Calibration::default(),
        },
    )]);
    let registered = classifier.clone();
    let judge = extension("judge", move |api| {
        api.register_classifiers(
            vec![FauxClassifier::default_model()],
            Some((FauxClassifier::API.into(), Arc::new(registered.clone()))),
        );
    });
    let keys = StaticKeys([("faux".to_string(), "secret".to_string())].into());
    let mut harness = Harness::new();
    harness.models = ModelRegistry::new(Arc::new(keys));
    harness
        .models
        .register_api(&harness.faux.model().api, Arc::new(harness.faux.clone()));
    harness.models.register_models([harness.faux.model()]);
    let session = harness.session_with_extensions(vec![judge]).await;

    let models = session.extension_context().models().unwrap();
    let model = models.find_classifier("faux", "faux-classifier-1").unwrap();
    let context = ClassifierContext {
        state: json!({ "ticket": "Charged twice" }),
        questions: [(
            "urgent".to_string(),
            ClassifierQuestion::bool("Is it urgent?"),
        )]
        .into_iter()
        .collect(),
        ..ClassifierContext::default()
    };
    let result = models
        .classify(&model, context, ClassifierOptions::default())
        .await;
    assert!(!result.is_failure(), "{:?}", result.error_message);
    assert!(matches!(
        result.answers["urgent"],
        ClassifierAnswer::Bool { probability, .. } if probability == 0.9
    ));
    // The provider's key is looked up for the request.
    assert_eq!(
        classifier.requests()[0].options.api_key.as_deref(),
        Some("secret")
    );
    assert_eq!(models.classifier_models(), [model]);
}

#[tokio::test]
async fn extensions_add_resources_they_discover_after_the_sessions_own() {
    use pi_coding_agent::extensions::{ResourcesDiscover, ResourcesDiscoverResult};
    use pi_coding_agent::resources::Resources;

    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    std::fs::create_dir_all(skills.join("audit")).unwrap();
    std::fs::write(
        skills.join("audit/SKILL.md"),
        "---\nname: audit\ndescription: Audits a change\n---\nSteps",
    )
    .unwrap();
    let template = dir.path().join("ship.md");
    std::fs::write(&template, "---\ndescription: Ship it\n---\nShip $ARGUMENTS").unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let (skill_dir, template_file) = (skills.clone(), template.clone());
    let discoverer = extension("discoverer", move |api| {
        let log = log.clone();
        let (skill_dir, template_file) = (skill_dir.clone(), template_file.clone());
        api.on(move |event: ResourcesDiscover, _ctx| {
            log.lock()
                .unwrap()
                .push(format!("{:?} {}", event.reason, event.cwd.display()));
            let result = ResourcesDiscoverResult {
                skill_paths: vec![skill_dir.clone()],
                prompt_paths: vec![template_file.clone()],
            };
            async move { Ok(result) }
        });
    });
    let harness = Harness::new();
    let session = harness.session_with_extensions(vec![discoverer]).await;
    assert_eq!(*seen.lock().unwrap(), ["Startup /work"]);
    let names = |session: &pi_coding_agent::AgentSession| {
        let resources = session.resources();
        (
            resources
                .skills
                .iter()
                .map(|skill| skill.name.clone())
                .collect::<Vec<_>>(),
            resources
                .prompt_templates
                .iter()
                .map(|template| template.name.clone())
                .collect::<Vec<_>>(),
        )
    };
    assert_eq!(
        names(&session),
        (vec!["audit".to_string()], vec!["ship".to_string()])
    );
    let commands: Vec<String> = session
        .commands()
        .into_iter()
        .map(|command| command.name)
        .collect();
    assert!(
        commands.contains(&"skill:audit".to_string()),
        "{commands:?}"
    );
    assert!(commands.contains(&"ship".to_string()), "{commands:?}");
    // Resources the host replaces keep what the extensions discovered.
    session.set_resources(Resources::default());
    assert_eq!(
        names(&session),
        (vec!["audit".to_string()], vec!["ship".to_string()])
    );
}

#[tokio::test]
async fn extensions_can_take_over_a_users_shell_command() {
    use pi_coding_agent::BashResult;
    use pi_coding_agent::extensions::{UserBash, UserBashResult};

    let harness = Harness::new();
    let session = harness.session().await;
    assert!(session.user_bash("ls", false).await.unwrap().is_none());

    let remote = extension("remote", |api| {
        api.on(|event: UserBash, _ctx| async move {
            if event.command == "fail" {
                return Err("the remote is down".into());
            }
            Ok(Some(UserBashResult::Result(BashResult {
                output: format!(
                    "ran {} remotely, kept: {}",
                    event.command, !event.exclude_from_context
                ),
                exit_code: Some(0),
                cancelled: false,
                truncated: false,
                full_output_path: None,
            })))
        });
    });
    let session = harness.session_with_extensions(vec![remote]).await;
    let Some(UserBashResult::Result(result)) = session.user_bash("ls", true).await.unwrap() else {
        panic!("expected a result")
    };
    assert_eq!(result.output, "ran ls remotely, kept: false");
    assert_eq!(
        session.user_bash("fail", false).await.err().as_deref(),
        Some("the remote is down")
    );
}

#[tokio::test]
async fn commands_offer_argument_completions_and_providers_can_come_later() {
    use pi_coding_agent::extensions::AutocompleteItem;

    let late = FauxProvider::with_model({
        let mut model = FauxProvider::default_model();
        model.id = "late-model".into();
        model.provider = "late".into();
        model
    });
    let deploy = extension("deploy", move |api| {
        let late = late.clone();
        api.register_command("deploy", "Deploy somewhere", move |_args, ctx| {
            let late = late.clone();
            async move {
                ctx.register_provider(vec![late.model()], None);
                Ok(())
            }
        });
        api.register_command_completions("deploy", |prefix| async move {
            let targets = ["production", "preview", "staging"];
            let items: Vec<AutocompleteItem> = targets
                .iter()
                .filter(|target| target.starts_with(&prefix))
                .map(|target| AutocompleteItem {
                    value: target.to_string(),
                    label: target.to_string(),
                    description: None,
                })
                .collect();
            (!items.is_empty()).then_some(items)
        });
    });
    let harness = Harness::new();
    let session = harness.session_with_extensions(vec![deploy]).await;
    let values = |items: Option<Vec<AutocompleteItem>>| {
        items.map(|items| items.into_iter().map(|item| item.value).collect::<Vec<_>>())
    };
    assert_eq!(
        values(session.command_argument_completions("deploy", "pr").await),
        Some(vec!["production".to_string(), "preview".to_string()])
    );
    assert_eq!(
        session.command_argument_completions("deploy", "x").await,
        None
    );
    assert_eq!(
        session.command_argument_completions("missing", "").await,
        None
    );

    // A provider registered after loading is there at once.
    let models = session.extension_context().models().unwrap();
    assert!(models.find("late", "late-model").is_none());
    session
        .prompt("/deploy", PromptOptions::default())
        .await
        .unwrap();
    assert!(models.find("late", "late-model").is_some());
    session.extension_context().unregister_provider("late");
    assert!(models.find("late", "late-model").is_none());
}

#[tokio::test]
async fn extensions_reach_the_hosts_interface() {
    use pi_coding_agent::extensions::{DialogOptions, ExtensionUi, NotifyLevel, WidgetPlacement};

    /// Records what extensions ask of it, and edits by adding a line.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);

    #[async_trait::async_trait]
    impl ExtensionUi for Recorder {
        fn has_ui(&self) -> bool {
            true
        }
        fn notify(&self, message: &str, level: NotifyLevel) {
            self.0
                .lock()
                .unwrap()
                .push(format!("notify {level:?}: {message}"));
        }
        async fn select(
            &self,
            title: &str,
            _options: &[String],
            dialog: &DialogOptions,
        ) -> Option<usize> {
            self.0
                .lock()
                .unwrap()
                .push(format!("select {title} within {:?}", dialog.timeout_ms));
            Some(1)
        }
        async fn editor(&self, title: &str, prefill: Option<&str>) -> Option<String> {
            self.0.lock().unwrap().push(format!("editor {title}"));
            Some(format!("{}\nedited", prefill.unwrap_or_default()))
        }
        fn set_status(&self, key: &str, text: Option<&str>) {
            self.0
                .lock()
                .unwrap()
                .push(format!("status {key}: {text:?}"));
        }
        fn set_widget(&self, key: &str, lines: Option<&[String]>, placement: WidgetPlacement) {
            self.0
                .lock()
                .unwrap()
                .push(format!("widget {key} {placement:?}: {lines:?}"));
        }
        fn set_title(&self, title: &str) {
            self.0.lock().unwrap().push(format!("title {title}"));
        }
    }

    let ui = Arc::new(Recorder::default());
    let asker = extension("asker", |api| {
        api.register_command("ask", "Ask the user", |_args, ctx| async move {
            let ui = ctx.ui();
            ui.notify("Starting", NotifyLevel::Warning);
            let dialog = DialogOptions {
                timeout_ms: Some(5_000),
                ..DialogOptions::default()
            };
            let choice = ui
                .select("Pick", &["a".to_string(), "b".to_string()], &dialog)
                .await;
            let text = ui.editor("Edit", Some("draft")).await;
            ui.set_status("asker", Some(&format!("{choice:?} {text:?}")));
            ui.set_widget(
                "asker",
                Some(&["one".to_string()]),
                WidgetPlacement::BelowEditor,
            );
            ui.set_title("Asking");
            Ok(())
        });
    });
    let harness = Harness::new();
    let recorder = ui.clone();
    let session = harness
        .session_with(move |options| {
            options.ui = recorder;
            options.extensions.push(asker);
        })
        .await;
    session
        .prompt("/ask", PromptOptions::default())
        .await
        .unwrap();
    assert_eq!(
        *ui.0.lock().unwrap(),
        [
            "notify Warning: Starting",
            "select Pick within Some(5000)",
            "editor Edit",
            "status asker: Some(\"Some(1) Some(\\\"draft\\\\nedited\\\")\")",
            "widget asker BelowEditor: Some([\"one\"])",
            "title Asking",
        ]
    );
}
