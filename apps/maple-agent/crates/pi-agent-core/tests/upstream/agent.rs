//! One-for-one translation of packages/agent/test/agent.test.ts (40 expanded cases).
//! The outer module is the source file; `agent` retains its `describe("Agent")` path.
//! Source wall-clock waits are replaced by current-thread task checkpoints and gates.

use std::sync::Arc;

use pi_agent_core::agent::{Agent, AgentInitialState, AgentOptions, PromptInput};
use pi_agent_core::stream_fn::set_default_stream_fn;
use pi_agent_core::types::*;
use pi_ai::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, DoneReason, ErrorReason, Message,
    Model, Schema, StopReason, TextContent, Tool, ToolCall, UserContent, UserMessage,
    UserMessageContent,
};
use pi_ai::utils::event_stream::{
    AssistantMessageEventStream, create_assistant_message_event_stream,
};
use pi_ai::utils::transcript::get_current_system_message;
use pi_testkit::{LocalTaskSet, VirtualEnv};
use serde_json::{Value, json};

const NOW: f64 = 1_700_000_000_000.0;

fn environment() -> Arc<VirtualEnv> {
    Arc::new(VirtualEnv::new(NOW as i64))
}

fn user(text: &str) -> AgentMessage {
    UserMessage {
        content: UserMessageContent::Text(text.into()),
        timestamp: NOW,
        ..UserMessage::default()
    }
    .into()
}

fn assistant(text: &str) -> AssistantMessage {
    AssistantMessage {
        content: vec![TextContent::new(text).into()],
        api: "openai-responses".into(),
        provider: "openai".into(),
        model: "mock".into(),
        stop_reason: StopReason::Stop,
        timestamp: NOW,
        ..AssistantMessage::default()
    }
}

fn tool_use(calls: &[(&str, &str)]) -> AssistantMessage {
    AssistantMessage {
        content: calls
            .iter()
            .map(|(id, name)| {
                AssistantContent::ToolCall(ToolCall {
                    id: (*id).into(),
                    name: (*name).into(),
                    arguments: JsValue::from_json_with_js_numbers(json!({})).into(),
                    ..ToolCall::default()
                })
            })
            .collect(),
        stop_reason: StopReason::ToolUse,
        ..assistant("")
    }
}

fn message(value: Value) -> AgentMessage {
    serde_json::from_value::<Message>(value)
        .expect("valid fixture message")
        .into()
}

fn tool_result(text: &str, status: Option<&str>, terminate: bool) -> AgentToolResult {
    AgentToolResult {
        content: Some(vec![UserContent::Text(TextContent::new(text))]),
        details: Some(JsValue::from_json_with_js_numbers(
            status.map_or_else(|| json!({}), |status| json!({ "status": status })),
        )),
        terminate: terminate.then_some(true),
        ..AgentToolResult::default()
    }
}

fn tool(name: &str) -> SharedAgentTool {
    let result = tool_result(name, None, false);
    Shared::new(AgentTool {
        tool: Tool {
            name: name.into(),
            description: format!("{name} tool").into(),
            parameters: Schema::typebox(json!({ "type": "object", "properties": {} })),
            ..Tool::default()
        },
        label: name.into(),
        prepare_arguments: None,
        output_schema: None,
        execute: Arc::new(move |_, _, _, _| {
            let result = result.clone();
            Box::pin(async move { Ok(result) })
        }),
        replay: None,
        execution_mode: None,
    })
}

fn echo_tool() -> SharedAgentTool {
    let tool = tool("echo");
    tool.update(|tool| {
        tool.label = "Echo".into();
        tool.description = "Echo input".into();
    });
    tool
}

fn complete_stream(message: AssistantMessage) -> AssistantMessageEventStream {
    let stream = create_assistant_message_event_stream();
    let reason = if message.stop_reason == StopReason::ToolUse {
        DoneReason::ToolUse
    } else {
        DoneReason::Stop
    };
    stream.push(AssistantMessageEvent::Done { reason, message });
    stream
}

fn answer(text: &str) -> StreamFn {
    let response = assistant(text);
    Arc::new(move |_, _, _| {
        let stream = complete_stream(response.clone());
        Box::pin(async move { Ok(stream) })
    })
}

fn unused_stream() -> StreamFn {
    Arc::new(|_, _, _| Box::pin(async { Err(AgentError::new("Unexpected stream call")) }))
}

fn options(stream: StreamFn) -> AgentOptions {
    AgentOptions {
        stream_fn: Some(stream),
        ..AgentOptions::default()
    }
}

fn new_agent(options: AgentOptions) -> Agent {
    Agent::new(options, environment()).expect("construct agent")
}

fn roles(messages: &AgentMessages) -> Vec<String> {
    messages.read(|messages| {
        messages
            .iter()
            .map(|message| message.role().as_str().unwrap().to_owned())
            .collect()
    })
}

fn record_events(agent: &Agent) -> Shared<Vec<AgentEvent>> {
    let events = Shared::new(Vec::new());
    let recorded = events.clone();
    let _unsubscribe = agent.subscribe(Arc::new(move |event, _| {
        recorded.push(event);
        Box::pin(async { Ok(()) })
    }));
    events
}

fn recording_user_requests(requests: Shared<Vec<Vec<String>>>) -> StreamFn {
    Arc::new(move |_, context, _| {
        requests.push(
            context
                .messages
                .iter()
                .filter_map(|message| match message {
                    Message::User(UserMessage {
                        content: UserMessageContent::Text(text),
                        ..
                    }) => Some(text.as_str().unwrap().to_owned()),
                    _ => None,
                })
                .collect(),
        );
        Box::pin(async { Ok(complete_stream(assistant("Processed"))) })
    })
}

#[derive(Clone, Default)]
struct Gate(CancellationToken);
impl Gate {
    fn release(&self) {
        self.0.cancel();
    }
    async fn wait(&self) {
        self.0.cancelled().await;
    }
}

// The test controls the provider response explicitly after observing the stream
// start. This replaces upstream's repeated setTimeout(checkAbort, 5).
fn held_stream(
    observed: Shared<Option<AssistantMessageEventStream>>,
    signal: Shared<Option<CancellationToken>>,
    started: Gate,
) -> StreamFn {
    Arc::new(move |_, _, options| {
        signal.update(|slot| *slot = options.and_then(|options| options.signal.clone()));
        let stream = create_assistant_message_event_stream();
        stream.push(AssistantMessageEvent::Start {
            partial: assistant("").into(),
        });
        observed.update(|slot| *slot = Some(stream.clone()));
        started.release();
        Box::pin(async move { Ok(stream) })
    })
}

fn finish_aborted(observed: &Shared<Option<AssistantMessageEventStream>>) {
    observed.read(|stream| {
        stream
            .as_ref()
            .expect("provider started")
            .push(AssistantMessageEvent::Error {
                reason: ErrorReason::Aborted,
                // Keep the upstream mock's stopReason: "stop" unchanged.
                error: assistant("Aborted"),
            })
    });
}

#[allow(clippy::module_inception, non_snake_case)]
mod agent {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::await_holding_lock)] // Serialize the process-global default fixture with the loop suite.
    async fn uses_the_configured_default_when_a_legacy_caller_omits_streamfn() {
        let _default_lock = super::super::support::DEFAULT_STREAM_LOCK.lock().unwrap();
        let calls = Shared::new(0);
        let recorded = calls.clone();
        set_default_stream_fn(Some(Arc::new(move |_, _, _| {
            recorded.update(|calls| *calls += 1);
            Box::pin(async { Ok(complete_stream(assistant("fallback"))) })
        })));
        struct ResetDefault;
        impl Drop for ResetDefault {
            fn drop(&mut self) {
                set_default_stream_fn(None);
            }
        }
        let _reset = ResetDefault;
        let agent = new_agent(AgentOptions::default());
        agent.prompt_text("Hello", None).await.unwrap();
        assert_eq!(calls.snapshot(), 1);
    }

    #[test]
    fn should_create_an_agent_instance_with_default_state() {
        let agent = new_agent(options(unused_stream()));
        let state = agent.state();
        assert_eq!(
            state.model().unwrap().read(|model| model.id.clone()),
            "unknown"
        );
        assert_eq!(state.thinking_level(), ThinkingLevel::Off);
        assert!(state.tools().is_empty());
        assert!(state.messages().is_empty());
        assert!(!state.is_streaming());
        assert!(state.streaming_message().is_none());
        assert!(state.pending_tool_calls().read(|calls| calls.is_empty()));
        assert!(state.error_message().is_none());
    }

    #[test]
    fn should_create_an_agent_instance_with_custom_initial_state() {
        // Catalog access is replaced by a local fixture; the assertion is that
        // the supplied model is retained, not a property of the live registry.
        let model = Shared::new(Model {
            id: "gpt-4o-mini".into(),
            provider: "openai".into(),
            ..Model::default()
        });
        let agent = new_agent(AgentOptions {
            initial_state: Some(AgentInitialState {
                system_prompt: Some("You are a helpful assistant.".into()),
                model: Some(model.clone()),
                thinking_level: Some(ThinkingLevel::Low),
                ..AgentInitialState::default()
            }),
            ..options(unused_stream())
        });
        assert_eq!(
            serde_json::to_value(agent.state().messages()).unwrap(),
            json!([
                { "role": "system", "content": "You are a helpful assistant.", "timestamp": 0.0 }
            ])
        );
        assert!(agent.state().model().unwrap().ptr_eq(&model));
        assert_eq!(agent.state().thinking_level(), ThinkingLevel::Low);
    }

    #[test]
    fn converts_initial_prompt_and_tools_into_transcript_state() {
        let agent = new_agent(AgentOptions {
            initial_state: Some(AgentInitialState {
                system_prompt: Some("You are helpful.".into()),
                tools: Some(vec![echo_tool()].into()),
                ..AgentInitialState::default()
            }),
            ..options(unused_stream())
        });
        let initial = agent.state().messages().get(0).unwrap();
        assert_eq!(initial.role(), "system");
        let initial = initial.system().unwrap();
        assert_eq!(
            serde_json::to_value(initial.content).unwrap(),
            json!("You are helpful.")
        );
        assert_eq!(
            initial
                .tools_added
                .unwrap()
                .iter()
                .map(|tool| tool.name.as_str().unwrap().to_owned())
                .collect::<Vec<_>>(),
            ["echo"]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn declares_tool_loadout_changes_to_the_model_before_the_next_request() {
        let requests = Shared::new(Vec::<Vec<String>>::new());
        let recorded = requests.clone();
        let agent = new_agent(AgentOptions {
            initial_state: Some(AgentInitialState {
                system_prompt: Some("You are helpful.".into()),
                tools: Some(vec![tool("first")].into()),
                ..AgentInitialState::default()
            }),
            ..options(Arc::new(move |_, context, _| {
                recorded.push(
                    context
                        .messages
                        .iter()
                        .flat_map(|message| match message {
                            Message::System(message) => vec![
                                format!(
                                    "+{}",
                                    message
                                        .tools_added
                                        .iter()
                                        .flatten()
                                        .map(|tool| tool.name.as_str().unwrap())
                                        .collect::<Vec<_>>()
                                        .join(",")
                                ),
                                format!(
                                    "-{}",
                                    message
                                        .tools_removed
                                        .iter()
                                        .flatten()
                                        .map(|tool| tool.name.as_str().unwrap())
                                        .collect::<Vec<_>>()
                                        .join(",")
                                ),
                            ],
                            _ => Vec::new(),
                        })
                        .collect(),
                );
                Box::pin(async { Ok(complete_stream(assistant("done"))) })
            }))
        });
        agent.prompt_text("one", None).await.unwrap();
        agent.state().set_tools(&vec![tool("second")].into());
        agent.prompt_text("two", None).await.unwrap();
        agent.prompt_text("three", None).await.unwrap();
        assert_eq!(
            requests.snapshot(),
            vec![
                vec!["+first", "-"],
                vec!["+first", "-", "+second", "-first"],
                vec!["+first", "-", "+second", "-first"],
            ]
        );
        let update = agent
            .state()
            .messages()
            .snapshot()
            .iter()
            .filter_map(AgentMessage::system)
            .find(|message| message.tools_removed.is_some())
            .unwrap();
        assert!(update.timestamp.is_finite());
        assert_eq!(
            serde_json::to_value(&update).unwrap(),
            json!({
                "role": "system", "content": "", "timestamp": update.timestamp,
                "toolsAdded": [{"name":"second","description":"second tool","parameters":{"type":"object","properties":{}}}],
                "toolsRemoved": [{"name":"first"}]
            })
        );
        let initial = agent.state().messages().get(0).unwrap().system().unwrap();
        assert!(
            serde_json::to_value(&initial.tools_added.unwrap()[0])
                .unwrap()
                .get("execute")
                .is_none()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn merges_tool_changes_into_a_pending_system_message() {
        let agent = new_agent(AgentOptions {
            initial_state: Some(AgentInitialState {
                system_prompt: Some("You are helpful.".into()),
                ..AgentInitialState::default()
            }),
            ..options(Arc::new(|_, context, _| {
                assert_eq!(
                    context
                        .messages
                        .iter()
                        .filter(|message| message.role() == "system")
                        .count(),
                    2
                );
                Box::pin(async { Ok(complete_stream(assistant("done"))) })
            }))
        });
        agent.state().set_tools(&vec![echo_tool()].into());
        agent.prompt(PromptInput::Messages(vec![
            message(json!({"role":"system","content":"","sections":{"skills":"<skills>x</skills>"},"timestamp":1})),
            message(json!({"role":"user","content":"hi","timestamp":2})),
        ])).await.unwrap();
        assert_eq!(
            serde_json::to_value(agent.state().messages().get(1).unwrap()).unwrap(),
            json!({
                "role":"system","content":"","sections":{"skills":"<skills>x</skills>"},
                "toolsAdded":[{"name":"echo","description":"Echo input","parameters":{"type":"object","properties":{}}}],
                "timestamp":1.0
            })
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rewrites_pending_tool_declarations_to_match_the_executable_set() {
        let agent = new_agent(AgentOptions {
            initial_state: Some(AgentInitialState {
                system_prompt: Some("You are helpful.".into()),
                tools: Some(vec![tool("first")].into()),
                ..AgentInitialState::default()
            }),
            ..options(answer("done"))
        });
        agent.prompt(PromptInput::Messages(vec![
            message(json!({"role":"system","content":"","sections":{"note":"<note>x</note>"},
                "toolsAdded":[tool("second").read(|tool| tool.tool.clone())],"toolsRemoved":[{"name":"first"}],"timestamp":1})),
            message(json!({"role":"user","content":"hi","timestamp":2})),
        ])).await.unwrap();
        assert_eq!(
            serde_json::to_value(agent.state().messages().get(1).unwrap()).unwrap(),
            json!({
                "role":"system","content":"","sections":{"note":"<note>x</note>"},"timestamp":1.0
            })
        );
        let llm = agent
            .state()
            .messages()
            .snapshot()
            .iter()
            .filter_map(AgentMessage::as_llm)
            .collect::<Vec<_>>();
        let current =
            serde_json::to_value(get_current_system_message(&llm).unwrap().unwrap()).unwrap();
        assert_eq!(
            current["toolsAdded"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["first"]
        );
    }

    #[test]
    fn restores_the_transcript_baseline_when_reset() {
        let agent = new_agent(AgentOptions {
            initial_state: Some(AgentInitialState {
                system_prompt: Some("You are helpful.".into()),
                tools: Some(vec![echo_tool()].into()),
                messages: Some(
                    vec![message(
                        json!({"role":"user","content":"old","timestamp":1}),
                    )]
                    .into(),
                ),
                ..AgentInitialState::default()
            }),
            ..options(unused_stream())
        });
        agent.reset().unwrap();
        assert_eq!(agent.state().messages().len(), 1);
        let initial = agent.state().messages().get(0).unwrap();
        assert_eq!(initial.role(), "system");
        let initial = initial.system().unwrap();
        assert_eq!(
            serde_json::to_value(initial.content).unwrap(),
            json!("You are helpful.")
        );
        assert_eq!(
            initial
                .tools_added
                .unwrap()
                .iter()
                .map(|tool| tool.name.as_str().unwrap().to_owned())
                .collect::<Vec<_>>(),
            ["echo"]
        );
    }

    #[test]
    fn should_subscribe_to_events() {
        let agent = new_agent(options(unused_stream()));
        let count = Shared::new(0);
        let observed = count.clone();
        let unsubscribe = agent.subscribe(Arc::new(move |_, _| {
            observed.update(|count| *count += 1);
            Box::pin(async { Ok(()) })
        }));
        assert_eq!(count.snapshot(), 0);
        agent.state().set_thinking_level(ThinkingLevel::Low);
        assert_eq!(count.snapshot(), 0);
        assert_eq!(agent.state().thinking_level(), ThinkingLevel::Low);
        unsubscribe();
        agent.state().set_thinking_level(ThinkingLevel::High);
        assert_eq!(count.snapshot(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn emits_full_lifecycle_events_for_thrown_run_failures() {
        let agent = new_agent(options(Arc::new(|_, _, _| {
            Box::pin(async { Err(AgentError::new("provider exploded")) })
        })));
        let events = record_events(&agent);
        agent.prompt_text("hello", None).await.unwrap();
        assert_eq!(
            events.read(|events| events.iter().map(AgentEvent::kind).collect::<Vec<_>>()),
            [
                "agent_start",
                "turn_start",
                "message_start",
                "message_end",
                "message_start",
                "message_end",
                "turn_end",
                "agent_end"
            ]
        );
        let last = agent.state().messages().last().unwrap();
        assert_eq!(last.role(), "assistant");
        let last = last.assistant().unwrap().snapshot();
        assert_eq!(last.stop_reason, StopReason::Error);
        assert_eq!(last.error_message, Some("provider exploded".into()));
        assert_eq!(
            agent.state().error_message(),
            Some("provider exploded".into())
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn should_await_async_subscribers_before_prompt_resolves() {
        let gate = Gate::default();
        let entered = Gate::default();
        let listener_finished = Shared::new(false);
        let prompt_resolved = Shared::new(false);
        let agent = new_agent(options(answer("ok")));
        let wait = gate.clone();
        let listener_entered = entered.clone();
        let finished = listener_finished.clone();
        let _unsubscribe = agent.subscribe(Arc::new(move |event, _| {
            let wait = wait.clone();
            let finished = finished.clone();
            let listener_entered = listener_entered.clone();
            Box::pin(async move {
                if matches!(event, AgentEvent::AgentEnd { .. }) {
                    listener_entered.release();
                    wait.wait().await;
                    finished.update(|done| *done = true);
                }
                Ok(())
            })
        }));
        let mut tasks = LocalTaskSet::new();
        let prompt = agent.prompt_text("hello", None);
        let resolved = prompt_resolved.clone();
        tasks.spawn(async move {
            prompt.await.unwrap();
            resolved.update(|done| *done = true);
        });
        entered.wait().await;
        tasks.checkpoint().await;
        assert!(!prompt_resolved.snapshot());
        assert!(!listener_finished.snapshot());
        assert!(agent.state().is_streaming());
        gate.release();
        agent.wait_for_idle().await;
        tasks.checkpoint().await;
        assert!(listener_finished.snapshot());
        assert!(prompt_resolved.snapshot());
        assert!(!agent.state().is_streaming());
        assert_eq!(tasks.pending_tasks(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn waitforidle_should_wait_for_async_subscribers() {
        let gate = Gate::default();
        let entered = Gate::default();
        let agent = new_agent(options(answer("ok")));
        let wait = gate.clone();
        let listener_entered = entered.clone();
        let _unsubscribe = agent.subscribe(Arc::new(move |event, _| {
            let wait = wait.clone();
            let listener_entered = listener_entered.clone();
            Box::pin(async move {
                if matches!(event, AgentEvent::MessageEnd { ref message } if message.role() == "assistant") { listener_entered.release(); wait.wait().await; }
                Ok(())
            })
        }));
        let mut tasks = LocalTaskSet::new();
        let prompt = agent.prompt_text("hello", None);
        tasks.spawn(async move {
            prompt.await.unwrap();
        });
        let idle = agent.wait_for_idle();
        let idle_resolved = Shared::new(false);
        let resolved = idle_resolved.clone();
        tasks.spawn(async move {
            idle.await;
            resolved.update(|done| *done = true);
        });
        entered.wait().await;
        tasks.checkpoint().await;
        assert!(!idle_resolved.snapshot());
        assert!(agent.state().is_streaming());
        gate.release();
        agent.wait_for_idle().await;
        tasks.checkpoint().await;
        assert!(idle_resolved.snapshot());
        assert!(!agent.state().is_streaming());
        assert_eq!(tasks.pending_tasks(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn should_pass_the_active_abort_signal_to_subscribers() {
        let received_signal = Shared::new(None::<CancellationToken>);
        let observed = Shared::new(None);
        let provider_signal = Shared::new(None);
        let started = Gate::default();
        let agent = new_agent(options(held_stream(
            observed.clone(),
            provider_signal.clone(),
            started.clone(),
        )));
        let received = received_signal.clone();
        let _unsubscribe = agent.subscribe(Arc::new(move |event, signal| {
            if matches!(event, AgentEvent::AgentStart) {
                received.update(|slot| *slot = Some(signal));
            }
            Box::pin(async { Ok(()) })
        }));
        let mut tasks = LocalTaskSet::new();
        let prompt = agent.prompt_text("hello", None);
        tasks.spawn(async move {
            prompt.await.unwrap();
        });
        started.wait().await;
        tasks.checkpoint().await;
        let received = received_signal
            .snapshot()
            .expect("subscriber receives the active signal");
        assert!(!received.is_cancelled());
        agent.abort();
        assert!(provider_signal.snapshot().unwrap().is_cancelled());
        finish_aborted(&observed);
        agent.wait_for_idle().await;
        tasks.checkpoint().await;
        assert!(received.is_cancelled());
        assert_eq!(tasks.pending_tasks(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn should_ignore_tool_updates_after_the_tool_execution_settles() {
        let delayed = Shared::new(None::<AgentToolUpdateCallback>);
        let capture = delayed.clone();
        let delayed_tool = tool("delayed_tool");
        delayed_tool.update(|tool| {
            tool.label = "Delayed Tool".into();
            tool.description = "Captures progress callbacks".into();
            tool.execute = Arc::new(move |_, _, _, on_update| {
                capture.update(|slot| *slot = on_update.clone());
                if let Some(update) = on_update {
                    update(tool_result("running", Some("running"), false));
                }
                Box::pin(async { Ok(tool_result("ok", Some("done"), true)) })
            });
        });
        let agent = new_agent(AgentOptions {
            initial_state: Some(AgentInitialState {
                tools: Some(vec![delayed_tool].into()),
                ..AgentInitialState::default()
            }),
            ..options(Arc::new(|_, _, _| {
                Box::pin(async { Ok(complete_stream(tool_use(&[("call-1", "delayed_tool")]))) })
            }))
        });
        let events = record_events(&agent);
        agent.prompt_text("run tool", None).await.unwrap();
        let count = events.len();
        // The Rust callback has no rejected promise. Its unit return and absence
        // of a panic are the synchronous equivalent of no unhandled rejection.
        let late_update = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            delayed.snapshot().expect("captured progress callback")(tool_result(
                "late",
                Some("late"),
                false,
            ));
        }));
        assert!(late_update.is_ok());
        let mut tasks = LocalTaskSet::new();
        tasks.checkpoint().await;
        assert_eq!(
            events.read(|events| events
                .iter()
                .filter(|event| matches!(event, AgentEvent::ToolExecutionUpdate { .. }))
                .count()),
            1
        );
        assert_eq!(events.len(), count);
        assert!(agent.state().error_message().is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn should_ignore_a_settled_parallel_tool_update_while_another_tool_is_still_running() {
        let slow_started = Gate::default();
        let settled_ended = Gate::default();
        let release_slow = Gate::default();
        let captured = Shared::new(None::<AgentToolUpdateCallback>);
        let callback = captured.clone();
        let settled_tool = tool("settled_tool");
        settled_tool.update(|tool| {
            tool.execute = Arc::new(move |_, _, _, update| {
                callback.update(|slot| *slot = update);
                Box::pin(async { Ok(tool_result("done", Some("done"), true)) })
            });
        });
        let slow_tool = tool("slow_tool");
        let started = slow_started.clone();
        let wait = release_slow.clone();
        slow_tool.update(|tool| {
            tool.execute = Arc::new(move |_, _, _, _| {
                let wait = wait.clone();
                let started = started.clone();
                Box::pin(async move {
                    started.release();
                    wait.wait().await;
                    Ok(tool_result("done", Some("done"), true))
                })
            });
        });
        let agent = new_agent(AgentOptions {
            initial_state: Some(AgentInitialState {
                tools: Some(vec![settled_tool, slow_tool].into()),
                ..AgentInitialState::default()
            }),
            ..options(Arc::new(|_, _, _| {
                Box::pin(async {
                    Ok(complete_stream(tool_use(&[
                        ("call-1", "settled_tool"),
                        ("call-2", "slow_tool"),
                    ])))
                })
            }))
        });
        let events = Shared::new(Vec::new());
        let recorded = events.clone();
        let ended = settled_ended.clone();
        let _unsubscribe = agent.subscribe(Arc::new(move |event, _| {
            if matches!(&event, AgentEvent::ToolExecutionEnd { tool_call_id, .. } if tool_call_id == "call-1") { ended.release(); }
            recorded.push(event);
            Box::pin(async { Ok(()) })
        }));
        let mut tasks = LocalTaskSet::new();
        let prompt = agent.prompt_text("run tools", None);
        tasks.spawn(async move {
            prompt.await.unwrap();
        });
        slow_started.wait().await;
        settled_ended.wait().await;
        tasks.checkpoint().await;
        let count = events.len();
        captured.snapshot().unwrap()(tool_result("late", Some("late"), false));
        tasks.checkpoint().await;
        assert_eq!(events.len(), count);
        release_slow.release();
        agent.wait_for_idle().await;
        tasks.checkpoint().await;
        assert_eq!(
            events.read(|events| events
                .iter()
                .filter(|event| matches!(event, AgentEvent::ToolExecutionUpdate { .. }))
                .count()),
            0
        );
        assert_eq!(tasks.pending_tasks(), 0);
    }

    #[test]
    fn should_update_state_with_mutators() {
        let agent = new_agent(options(unused_stream()));
        let model = Shared::new(Model {
            id: "gemini-2.5-flash".into(),
            provider: "google".into(),
            ..Model::default()
        });
        agent.state().set_model(model.clone());
        assert!(agent.state().model().unwrap().ptr_eq(&model));
        agent.state().set_thinking_level(ThinkingLevel::High);
        assert_eq!(agent.state().thinking_level(), ThinkingLevel::High);
        // Supply executable fields required by the Rust tool type. The source
        // casts its shortened fixture to `any`; only array-copy behavior is tested.
        let tools: AgentTools = vec![tool("test")].into();
        agent.state().set_tools(&tools);
        assert_eq!(
            agent.state().tools().read(|tools| tools
                .iter()
                .map(|tool| tool.read(|tool| tool.tool.clone()))
                .collect::<Vec<_>>()),
            tools.read(|tools| tools
                .iter()
                .map(|tool| tool.read(|tool| tool.tool.clone()))
                .collect::<Vec<_>>())
        );
        assert!(!agent.state().tools().ptr_eq(&tools));
        assert!(
            agent
                .state()
                .tools()
                .get(0)
                .unwrap()
                .ptr_eq(&tools.get(0).unwrap())
        );
        let messages: AgentMessages = vec![user("Hello")].into();
        agent.state().set_messages(&messages);
        assert_eq!(agent.state().messages(), messages);
        assert!(!agent.state().messages().ptr_eq(&messages));
        let new_message: AgentMessage = assistant("Hi").into();
        agent.state().messages().push(new_message.clone());
        assert_eq!(agent.state().messages().len(), 2);
        assert!(
            agent
                .state()
                .messages()
                .get(1)
                .unwrap()
                .ptr_eq(&new_message)
        );
        agent.state().set_messages(&Vec::new().into());
        assert!(agent.state().messages().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn should_support_steering_message_queue() {
        let agent = new_agent(options(unused_stream()));
        let message = user("Steering message");
        agent.steer(message.clone());
        assert!(!agent.state().messages().snapshot().contains(&message));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn should_support_follow_up_message_queue() {
        let agent = new_agent(options(unused_stream()));
        let message = user("Follow-up message");
        agent.follow_up(message.clone());
        assert!(!agent.state().messages().snapshot().contains(&message));
    }

    #[test]
    fn should_handle_abort_controller() {
        new_agent(options(unused_stream())).abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn should_reject_reset_while_processing_without_corrupting_the_transcript() {
        let observed = Shared::new(None);
        let started = Gate::default();
        let agent = new_agent(options(held_stream(
            observed.clone(),
            Shared::new(None),
            started.clone(),
        )));
        let mut tasks = LocalTaskSet::new();
        let prompt = agent.prompt_text("Hello", None);
        tasks.spawn(async move {
            prompt.await.unwrap();
        });
        started.wait().await;
        tasks.checkpoint().await;
        assert!(observed.read(Option::is_some));
        assert!(agent.state().is_streaming());
        assert_eq!(roles(&agent.state().messages()), ["user"]);
        assert_eq!(
            agent.reset().unwrap_err().to_string(),
            "Agent is already processing. Wait for completion before resetting."
        );
        assert!(agent.state().is_streaming());
        assert_eq!(roles(&agent.state().messages()), ["user"]);
        observed.read(|stream| {
            stream.as_ref().unwrap().push(AssistantMessageEvent::Done {
                reason: DoneReason::Stop,
                message: assistant("Done"),
            })
        });
        agent.wait_for_idle().await;
        tasks.checkpoint().await;
        assert!(!agent.state().is_streaming());
        assert_eq!(roles(&agent.state().messages()), ["user", "assistant"]);
        assert_eq!(tasks.pending_tasks(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn should_throw_when_prompt_called_while_streaming() {
        let observed = Shared::new(None);
        let signal = Shared::new(None);
        let started = Gate::default();
        let agent = new_agent(options(held_stream(
            observed.clone(),
            signal.clone(),
            started.clone(),
        )));
        let mut tasks = LocalTaskSet::new();
        let prompt = agent.prompt_text("First message", None);
        tasks.spawn(async move {
            let _ = prompt.await;
        });
        started.wait().await;
        tasks.checkpoint().await;
        assert!(agent.state().is_streaming());
        assert_eq!(
            agent
                .prompt_text("Second message", None)
                .await
                .unwrap_err()
                .to_string(),
            "Agent is already processing a prompt. Use steer() or followUp() to queue messages, or wait for completion."
        );
        agent.abort();
        assert!(signal.snapshot().unwrap().is_cancelled());
        finish_aborted(&observed);
        agent.wait_for_idle().await;
        tasks.checkpoint().await;
        assert_eq!(tasks.pending_tasks(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn should_throw_when_continue_called_while_streaming() {
        let observed = Shared::new(None);
        let signal = Shared::new(None);
        let started = Gate::default();
        let agent = new_agent(options(held_stream(
            observed.clone(),
            signal.clone(),
            started.clone(),
        )));
        let mut tasks = LocalTaskSet::new();
        let prompt = agent.prompt_text("First message", None);
        tasks.spawn(async move {
            let _ = prompt.await;
        });
        started.wait().await;
        tasks.checkpoint().await;
        assert!(agent.state().is_streaming());
        assert_eq!(
            agent.continue_run().await.unwrap_err().to_string(),
            "Agent is already processing. Wait for completion before continuing."
        );
        agent.abort();
        assert!(signal.snapshot().unwrap().is_cancelled());
        finish_aborted(&observed);
        agent.wait_for_idle().await;
        tasks.checkpoint().await;
        assert_eq!(tasks.pending_tasks(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn continue_should_process_queued_follow_up_messages_after_an_assistant_turn() {
        let agent = new_agent(options(answer("Processed")));
        agent.state().set_messages(&vec![
            message(json!({"role":"user","content":[{"type":"text","text":"Initial"}],"timestamp":NOW-10.0})),
            assistant("Initial response").into(),
        ].into());
        agent.follow_up(message(json!({"role":"user","content":[{"type":"text","text":"Queued follow-up"}],"timestamp":NOW})));
        agent.continue_run().await.unwrap();
        assert!(agent.state().messages().read(|messages| messages.iter().any(|message| match message.as_llm() {
            Some(Message::User(message)) => match message.content {
                UserMessageContent::Text(text) => text == "Queued follow-up",
                UserMessageContent::Blocks(parts) => parts.iter().any(|part| matches!(part, UserContent::Text(text) if text.text == "Queued follow-up")),
            },
            _ => false,
        })));
        assert_eq!(agent.state().messages().last().unwrap().role(), "assistant");
    }

    async fn assistant_tail_steering(mode: QueueMode, expected: usize) {
        let requests = Shared::new(Vec::new());
        let agent = new_agent(AgentOptions {
            steering_mode: Some(mode),
            ..options(recording_user_requests(requests.clone()))
        });
        agent
            .state()
            .set_messages(&vec![user("Initial"), assistant("Initial response").into()].into());
        agent.steer(user("Steering 1"));
        agent.steer(user("Steering 2"));
        agent.continue_run().await.unwrap();
        let requests = requests.snapshot();
        assert_eq!(requests.len(), expected);
        assert!(requests[0].contains(&"Steering 1".into()));
        if mode == QueueMode::OneAtATime {
            assert!(!requests[0].contains(&"Steering 2".into()));
            assert!(requests[1].contains(&"Steering 2".into()));
        } else {
            assert!(requests[0].contains(&"Steering 2".into()));
        }
    }
    #[tokio::test(flavor = "current_thread")]
    async fn continue_keeps_mode_steering_semantics_for_assistant_tail_fallback__one_at_a_time() {
        assistant_tail_steering(QueueMode::OneAtATime, 2).await;
    }
    #[tokio::test(flavor = "current_thread")]
    async fn continue_keeps_mode_steering_semantics_for_assistant_tail_fallback__all() {
        assistant_tail_steering(QueueMode::All, 1).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn keeps_legacy_preparenextturn_signal_callback_behavior() {
        let requests = Shared::new(0);
        let recorded = requests.clone();
        let saw_signal = Shared::new(false);
        let saw = saw_signal.clone();
        let agent = new_agent(AgentOptions {
            initial_state: Some(AgentInitialState {
                tools: Some(vec![tool("noop")].into()),
                ..AgentInitialState::default()
            }),
            prepare_next_turn: Some(Arc::new(move |signal| {
                saw.update(|saw| *saw = signal.is_some());
                Box::pin(async { Ok(None) })
            })),
            ..options(Arc::new(move |_, _, _| {
                let count = recorded.update(|count| {
                    *count += 1;
                    *count
                });
                Box::pin(async move {
                    Ok(complete_stream(if count == 1 {
                        tool_use(&[("tool-1", "noop")])
                    } else {
                        assistant("done")
                    }))
                })
            }))
        });
        agent.prompt_text("start", None).await.unwrap();
        assert_eq!(requests.snapshot(), 2);
        assert!(saw_signal.snapshot());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn forwards_finishturn_through_agentoptions_with_the_active_abort_signal() {
        let requests = Shared::new(0);
        let recorded = requests.clone();
        let saw_signal = Shared::new(false);
        let saw = saw_signal.clone();
        let callback_roles = Shared::new(Vec::new());
        let observed_roles = callback_roles.clone();
        let agent = new_agent(AgentOptions {
            initial_state: Some(AgentInitialState {
                tools: Some(vec![tool("noop")].into()),
                ..AgentInitialState::default()
            }),
            finish_turn: Some(Arc::new(move |context, signal| {
                saw.update(|saw| *saw = signal.is_some());
                observed_roles.update(|observed| *observed = roles(&context.context.messages));
                Box::pin(async { Ok(Some(AgentTurnDecision::End)) })
            })),
            ..options(Arc::new(move |_, _, _| {
                let count = recorded.update(|count| {
                    *count += 1;
                    *count
                });
                Box::pin(async move {
                    Ok(complete_stream(if count == 1 {
                        tool_use(&[("tool-1", "noop")])
                    } else {
                        assistant("should not run")
                    }))
                })
            }))
        });
        agent.prompt_text("start", None).await.unwrap();
        assert_eq!(requests.snapshot(), 1);
        assert!(saw_signal.snapshot());
        assert_eq!(
            callback_roles.snapshot(),
            ["system", "user", "assistant", "toolResult"]
        );
    }

    async fn invalid_continuation(messages: Vec<AgentMessage>) {
        let agent = new_agent(AgentOptions {
            initial_state: Some(AgentInitialState {
                messages: Some(messages.into()),
                ..AgentInitialState::default()
            }),
            ..options(unused_stream())
        });
        let steering = user("steering");
        let follow_up = user("follow-up");
        agent.steer(steering.clone());
        agent.follow_up(follow_up.clone());
        assert_eq!(
            agent.continue_run().await.unwrap_err().to_string(),
            "No messages to continue from"
        );
        assert_eq!(agent.peek_queued_messages(), vec![steering]);
        agent.clear_steering_queue();
        assert_eq!(agent.peek_queued_messages(), vec![follow_up]);
    }
    #[tokio::test(flavor = "current_thread")]
    async fn rejects_a_queued_continuation_from_name_context_without_draining_queues__empty() {
        invalid_continuation(Vec::new()).await;
    }
    #[tokio::test(flavor = "current_thread")]
    async fn rejects_a_queued_continuation_from_name_context_without_draining_queues__system_only()
    {
        invalid_continuation(vec![message(
            json!({"role":"system","content":"system only","timestamp":1}),
        )])
        .await;
    }

    async fn deferred_follow_up(messages: Vec<AgentMessage>) {
        let requests = Shared::new(Vec::new());
        let agent = new_agent(AgentOptions {
            initial_state: Some(AgentInitialState {
                messages: Some(messages.into()),
                ..AgentInitialState::default()
            }),
            ..options(recording_user_requests(requests.clone()))
        });
        agent.follow_up(user("follow-up"));
        agent.continue_run().await.unwrap();
        let requests = requests.snapshot();
        assert_eq!(requests.len(), 2);
        assert!(!requests[0].contains(&"follow-up".into()));
        assert!(requests[1].contains(&"follow-up".into()));
    }
    #[tokio::test(flavor = "current_thread")]
    async fn defers_follow_up_input_on_the_first_continuation_request_from_a_name_tail__user() {
        deferred_follow_up(vec![user("existing user")]).await;
    }
    #[tokio::test(flavor = "current_thread")]
    async fn defers_follow_up_input_on_the_first_continuation_request_from_a_name_tail__toolresult()
    {
        deferred_follow_up(vec![
            user("existing user"), tool_use(&[("call-1", "noop")]).into(),
            message(json!({"role":"toolResult","toolCallId":"call-1","toolName":"noop","content":[{"type":"text","text":"done"}],"isError":false,"timestamp":1})),
        ]).await;
    }

    async fn startup_steering(mode: QueueMode, expected: usize) {
        let requests = Shared::new(Vec::new());
        let agent = new_agent(AgentOptions {
            initial_state: Some(AgentInitialState {
                messages: Some(vec![user("existing")].into()),
                ..AgentInitialState::default()
            }),
            steering_mode: Some(mode),
            ..options(recording_user_requests(requests.clone()))
        });
        agent.steer(user("first"));
        agent.steer(user("second"));
        agent.continue_run().await.unwrap();
        let requests = requests.snapshot();
        assert_eq!(requests.len(), expected);
        assert!(requests[0].contains(&"first".into()));
        if mode == QueueMode::OneAtATime {
            assert!(!requests[0].contains(&"second".into()));
            assert!(requests[1].contains(&"second".into()));
        } else {
            assert!(requests[0].contains(&"second".into()));
        }
    }
    #[tokio::test(flavor = "current_thread")]
    async fn polls_mode_steering_at_continuation_startup__one_at_a_time() {
        startup_steering(QueueMode::OneAtATime, 2).await;
    }
    #[tokio::test(flavor = "current_thread")]
    async fn polls_mode_steering_at_continuation_startup__all() {
        startup_steering(QueueMode::All, 1).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn keeps_steering_ahead_of_follow_up_from_a_non_assistant_continuation_tail() {
        let requests = Shared::new(Vec::new());
        let agent = new_agent(AgentOptions {
            initial_state: Some(AgentInitialState {
                messages: Some(vec![user("existing")].into()),
                ..AgentInitialState::default()
            }),
            ..options(recording_user_requests(requests.clone()))
        });
        agent.steer(user("steering"));
        agent.follow_up(user("follow-up"));
        agent.continue_run().await.unwrap();
        let requests = requests.snapshot();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].contains(&"steering".into()));
        assert!(!requests[0].contains(&"follow-up".into()));
        assert!(requests[1].contains(&"follow-up".into()));
    }

    async fn terminal_response_keeps_queues(reason: ErrorReason) {
        let steering = user("steering");
        let follow_up = user("follow-up");
        let agent = new_agent(AgentOptions {
            finish_turn: Some(Arc::new(|_, _| {
                Box::pin(async { Ok(Some(AgentTurnDecision::Continue)) })
            })),
            ..options(Arc::new(move |_, _, _| {
                let stream = create_assistant_message_event_stream();
                let name = if reason == ErrorReason::Error {
                    "error"
                } else {
                    "aborted"
                };
                let mut error = assistant(name);
                error.stop_reason = if reason == ErrorReason::Error {
                    StopReason::Error
                } else {
                    StopReason::Aborted
                };
                error.error_message = Some(name.into());
                stream.push(AssistantMessageEvent::Error { reason, error });
                Box::pin(async move { Ok(stream) })
            }))
        });
        agent.follow_up(follow_up.clone());
        let queued = steering.clone();
        let subscribed_agent = agent.clone();
        let unsubscribe = agent.subscribe(Arc::new(move |event, _| {
            if matches!(event, AgentEvent::MessageEnd { ref message } if message.role() == "assistant") { subscribed_agent.steer(queued.clone()); }
            Box::pin(async { Ok(()) })
        }));
        agent.prompt_text("start", None).await.unwrap();
        assert_eq!(agent.peek_queued_messages(), vec![steering]);
        agent.clear_steering_queue();
        assert_eq!(agent.peek_queued_messages(), vec![follow_up]);
        unsubscribe();
    }
    #[tokio::test(flavor = "current_thread")]
    async fn keeps_queues_on_a_s_response_even_when_finishturn_requests_continuation__error() {
        terminal_response_keeps_queues(ErrorReason::Error).await;
    }
    #[tokio::test(flavor = "current_thread")]
    async fn keeps_queues_on_a_s_response_even_when_finishturn_requests_continuation__aborted() {
        terminal_response_keeps_queues(ErrorReason::Aborted).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn keeps_queues_when_finishturn_ends_the_run() {
        let steering = user("steering");
        let follow_up = user("follow-up");
        let agent = new_agent(AgentOptions {
            finish_turn: Some(Arc::new(|_, _| {
                Box::pin(async { Ok(Some(AgentTurnDecision::End)) })
            })),
            ..options(answer("done"))
        });
        agent.follow_up(follow_up.clone());
        let queued = steering.clone();
        let subscribed_agent = agent.clone();
        let unsubscribe = agent.subscribe(Arc::new(move |event, _| {
            if matches!(event, AgentEvent::MessageEnd { ref message } if message.role() == "assistant") { subscribed_agent.steer(queued.clone()); }
            Box::pin(async { Ok(()) })
        }));
        agent.prompt_text("start", None).await.unwrap();
        assert_eq!(agent.peek_queued_messages(), vec![steering]);
        agent.clear_steering_queue();
        assert_eq!(agent.peek_queued_messages(), vec![follow_up]);
        unsubscribe();
    }

    #[test]
    fn previews_the_next_selected_queued_messages_without_consuming_them() {
        let agent = new_agent(AgentOptions {
            steering_mode: Some(QueueMode::OneAtATime),
            follow_up_mode: Some(QueueMode::All),
            ..options(Arc::new(|_, _, _| {
                Box::pin(async { Ok(create_assistant_message_event_stream()) })
            }))
        });
        let first = user("first steering");
        let second = user("second steering");
        let follow_up = user("follow-up");
        agent.steer(first.clone());
        agent.steer(second);
        agent.follow_up(follow_up.clone());
        assert_eq!(agent.peek_queued_messages(), vec![first.clone()]);
        assert_eq!(agent.peek_queued_messages(), vec![first]);
        agent.clear_steering_queue();
        assert_eq!(agent.peek_queued_messages(), vec![follow_up]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn forwards_provider_stream_event_observers_through_agentoptions() {
        let provider_events = Shared::new(Vec::<JsValue>::new());
        let recorded = provider_events.clone();
        let agent = new_agent(AgentOptions {
            on_provider_stream_event: Some(Arc::new(move |data, _| {
                recorded.push(data);
                Box::pin(async { Ok(()) })
            })),
            ..options(Arc::new(|model, _, options| {
                Box::pin(async move {
                    if let Some(observer) =
                        options.and_then(|options| options.on_provider_stream_event.clone())
                    {
                        observer(
                            JsValue::from_json_with_js_numbers(json!({"request_cost":0.01})),
                            model,
                        )
                        .await
                        .unwrap();
                    }
                    Ok(complete_stream(assistant("ok")))
                })
            }))
        });
        agent.prompt_text("hello", None).await.unwrap();
        assert_eq!(
            provider_events.snapshot(),
            vec![JsValue::from_json_with_js_numbers(
                json!({"request_cost":0.01})
            )]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn forwards_sessionid_to_streamfunction_options() {
        let received = Shared::new(None::<String>);
        let recorded = received.clone();
        let agent = new_agent(AgentOptions {
            session_id: Some("session-abc".into()),
            ..options(Arc::new(move |_, _, options| {
                recorded
                    .update(|slot| *slot = options.and_then(|options| options.session_id.clone()));
                Box::pin(async { Ok(complete_stream(assistant("ok"))) })
            }))
        });
        agent.prompt_text("hello", None).await.unwrap();
        assert_eq!(received.snapshot().as_deref(), Some("session-abc"));
        agent
            .config()
            .update(|config| config.session_id = Some("session-def".into()));
        assert_eq!(
            agent
                .config()
                .read(|config| config.session_id.clone())
                .as_deref(),
            Some("session-def")
        );
        agent.prompt_text("hello again", None).await.unwrap();
        assert_eq!(received.snapshot().as_deref(), Some("session-def"));
    }
}
