//! One-to-one translations of `packages/agent/test/agent-loop.test.ts`.
use pi_agent_core::agent_loop::*;
use pi_agent_core::stream_fn::set_default_stream_fn;
use pi_agent_core::types::*;
use pi_ai::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, DoneReason, ErrorReason, Message,
    Model, Schema, SharedAssistantMessage, StopReason, SystemMessage, TextContent, Tool, ToolCall,
    ToolResultMessage, Usage, UsageCost, UserContent, UserMessage,
};
use pi_ai::utils::event_stream::{
    AssistantMessageEventStream, create_assistant_message_event_stream,
};
use pi_ai::utils::text::content_text;
use pi_testkit::env::VirtualEnv;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::Arc;

fn js(value: Value) -> JsValue {
    JsValue::from_json_with_js_numbers(value)
}
fn model() -> Model {
    serde_json::from_value(json!({"id":"mock","name":"mock","api":"openai-responses","provider":"openai","baseUrl":"https://example.invalid","reasoning":false,"input":["text"],"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"contextWindow":8192,"maxTokens":2048})).unwrap()
}
fn user(text: &str) -> AgentMessage {
    UserMessage {
        content: text.into(),
        timestamp: 0.0,
        ..UserMessage::default()
    }
    .into()
}
fn assistant(content: Vec<AssistantContent>, reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        content,
        stop_reason: reason,
        ..AssistantMessage::new(&model(), 0.0)
    }
}
fn answer(text: &str) -> AssistantMessage {
    assistant(
        vec![AssistantContent::Text(TextContent::new(text))],
        StopReason::Stop,
    )
}
fn call(id: &str, name: &str, value: Value) -> ToolCall {
    ToolCall::new(id, name, js(value))
}
fn calls(values: &[&str]) -> AssistantMessage {
    assistant(
        values
            .iter()
            .enumerate()
            .map(|(i, v)| {
                AssistantContent::ToolCall(call(
                    &format!("tool-{}", i + 1),
                    "echo",
                    json!({"value":v}),
                ))
            })
            .collect(),
        StopReason::ToolUse,
    )
}
fn converter() -> ConvertToLlm {
    Arc::new(|messages| {
        Box::pin(async move {
            Ok(messages.read(|v| v.iter().filter_map(AgentMessage::as_llm).collect()))
        })
    })
}
fn config() -> AgentLoopConfig {
    AgentLoopConfig::new(model(), converter())
}
fn response(message: AssistantMessage) -> AssistantMessageEventStream {
    let stream = create_assistant_message_event_stream();
    if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
        stream.push(AssistantMessageEvent::Error {
            reason: if message.stop_reason == StopReason::Aborted {
                ErrorReason::Aborted
            } else {
                ErrorReason::Error
            },
            error: message,
        });
    } else {
        let reason = match message.stop_reason {
            StopReason::ToolUse => DoneReason::ToolUse,
            StopReason::Length => DoneReason::Length,
            _ => DoneReason::Stop,
        };
        stream.push(AssistantMessageEvent::Done { reason, message });
    }
    stream
}
fn stream(messages: Vec<AssistantMessage>) -> (StreamFn, Shared<usize>) {
    let queue = Shared::new(VecDeque::from(messages));
    let count = Shared::new(0);
    let count2 = count.clone();
    (
        Arc::new(move |_, _, _| {
            count2.update(|n| *n += 1);
            let message = queue
                .update(|q| q.pop_front())
                .expect("unexpected provider call");
            Box::pin(async move { Ok(response(message)) })
        }),
        count,
    )
}
fn result(text: impl Into<JsString>) -> AgentToolResult {
    AgentToolResult {
        content: Some(vec![UserContent::Text(TextContent::new(text))]),
        details: Some(js(json!({}))),
        ..AgentToolResult::default()
    }
}
fn echo(executed: Shared<Vec<JsValue>>) -> AgentTool {
    AgentTool {
        tool: Tool {
            name: "echo".into(),
            description: "Echo tool".into(),
            parameters: Schema::typebox(
                json!({"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}),
            ),
            constrained_sampling: None,
        },
        label: "Echo".into(),
        prepare_arguments: None,
        output_schema: None,
        replay: None,
        execution_mode: None,
        execute: Arc::new(move |_, params, _, _| {
            let value = params.read(|v| v.get("value").unwrap().clone());
            executed.push(value.clone());
            Box::pin(async move {
                let mut out = result(
                    value
                        .as_js_str()
                        .cloned()
                        .unwrap_or_else(|| pi_ai::utils::js_json::stringify(&value).into()),
                );
                out.details = Some(js(json!({"value":null})));
                out.details.as_mut().unwrap()["value"] = value;
                Ok(out)
            })
        }),
    }
}
fn context(tools: Vec<AgentTool>) -> AgentContext {
    AgentContext::new(Vec::new(), Some(tools))
}
fn env() -> Arc<dyn PiEnv> {
    Arc::new(VirtualEnv::new(0))
}
async fn collect(mut stream: AgentEventStream) -> (Vec<AgentEvent>, AgentMessages) {
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    let messages = stream.result().await;
    (events, messages)
}
async fn run(
    config: AgentLoopConfig,
    tools: Vec<AgentTool>,
    stream: StreamFn,
) -> (Vec<AgentEvent>, AgentMessages) {
    collect(agent_loop(
        vec![user("run")],
        context(tools),
        config,
        None,
        Some(stream),
        env(),
    ))
    .await
}
fn roles(messages: &AgentMessages) -> Vec<JsString> {
    messages.read(|m| m.iter().map(AgentMessage::role).collect())
}
fn user_texts(messages: &[Message]) -> Vec<JsString> {
    messages
        .iter()
        .filter_map(|m| match m {
            Message::User(m) => Some(content_text(&m.content, "\n")),
            _ => None,
        })
        .collect()
}
fn tool_results(messages: &AgentMessages) -> Vec<ToolResultMessage> {
    messages.read(|m| {
        m.iter()
            .filter_map(|m| match m.as_llm() {
                Some(Message::ToolResult(m)) => Some(m),
                _ => None,
            })
            .collect()
    })
}
#[tokio::test]
#[allow(clippy::await_holding_lock)] // Only the two global default-slot fixtures share this lock.
async fn uses_the_configured_default_when_a_legacy_caller_omits_stream_fn() {
    let _guard = super::support::DEFAULT_STREAM_LOCK
        .lock()
        .expect("default stream test lock");
    let (provider, count) = stream(vec![answer("fallback")]);
    set_default_stream_fn(Some(provider));
    let result = agent_loop(
        vec![user("Hello")],
        context(vec![]),
        config(),
        None,
        None,
        env(),
    )
    .result()
    .await;
    set_default_stream_fn(None);
    assert_eq!(count.snapshot(), 1);
    assert_eq!(result.len(), 2);
}
#[tokio::test]
async fn should_emit_events_with_agent_message_types() {
    let (provider, _) = stream(vec![answer("Hi there!")]);
    let (events, messages) = run(config(), vec![], provider).await;
    assert_eq!(roles(&messages), ["user", "assistant"]);
    for kind in [
        "agent_start",
        "turn_start",
        "message_start",
        "message_end",
        "turn_end",
        "agent_end",
    ] {
        assert!(events.iter().any(|e| e.kind() == kind));
    }
}
#[tokio::test]
async fn should_build_provider_context_exclusively_from_transcript_messages() {
    let system: SystemMessage = SystemMessage {
        content: "Transcript prompt".into(),
        tools_added: Some(vec![]),
        timestamp: 1.0,
        ..SystemMessage::default()
    };
    let expected = system.clone();
    let provider: StreamFn = Arc::new(move |_, ctx, _| {
        assert_eq!(ctx.messages[0], Message::System(expected.clone()));
        assert_eq!(
            serde_json::to_value(&ctx)
                .unwrap()
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["messages"]
        );
        Box::pin(async { Ok(response(answer("done"))) })
    });
    collect(agent_loop(
        vec![system.into(), user("Hello")],
        context(vec![]),
        config(),
        None,
        Some(provider),
        env(),
    ))
    .await;
}
#[tokio::test]
async fn should_handle_custom_message_types_via_convert_to_llm() {
    let converted = Shared::new(Vec::new());
    let copy = converted.clone();
    let mut conf = config();
    conf.convert_to_llm = Arc::new(move |messages| {
        let values = messages.read(|v| {
            v.iter()
                .filter(|m| m.role() != "notification")
                .filter_map(AgentMessage::as_llm)
                .collect::<Vec<_>>()
        });
        copy.update(|v| *v = values.clone());
        Box::pin(async move { Ok(values) })
    });
    let notification: JsObject =
        match js(json!({"role":"notification","text":"This is a notification","timestamp":0})) {
            JsValue::Object(v) => v,
            _ => unreachable!(),
        };
    let ctx = AgentContext::new(vec![notification.into()], Some(vec![]));
    let (provider, _) = stream(vec![answer("Response")]);
    collect(agent_loop(
        vec![user("Hello")],
        ctx,
        conf,
        None,
        Some(provider),
        env(),
    ))
    .await;
    assert_eq!(converted.read(Vec::len), 1);
    assert!(matches!(converted.read(|v| v[0].clone()), Message::User(_)));
}
#[tokio::test]
async fn should_apply_transform_context_before_convert_to_llm() {
    let transformed = Shared::new(0);
    let converted = Shared::new(0);
    let mut conf = config();
    let count = transformed.clone();
    conf.transform_context = Some(Arc::new(move |messages, _| {
        let values = messages.snapshot();
        let sliced = values[values.len() - 2..].to_vec();
        count.update(|n| *n = sliced.len());
        Box::pin(async move { Ok(sliced.into()) })
    }));
    let count = converted.clone();
    conf.convert_to_llm = Arc::new(move |messages| {
        count.update(|n| *n = messages.len());
        Box::pin(async move {
            Ok(messages.read(|v| v.iter().filter_map(AgentMessage::as_llm).collect()))
        })
    });
    let ctx = AgentContext::new(
        vec![
            user("old message 1"),
            answer("old response 1").into(),
            user("old message 2"),
            answer("old response 2").into(),
        ],
        Some(vec![]),
    );
    let (provider, _) = stream(vec![answer("Response")]);
    collect(agent_loop(
        vec![user("new message")],
        ctx,
        conf,
        None,
        Some(provider),
        env(),
    ))
    .await;
    assert_eq!(transformed.snapshot(), 2);
    assert_eq!(converted.snapshot(), 2);
}
#[tokio::test]
async fn should_handle_tool_calls_and_results() {
    let executed = Shared::default();
    let mut tool = echo(executed.clone());
    let original = tool.execute.clone();
    let usage = Usage {
        input: 1.0,
        output: 2.0,
        cache_read: 3.0,
        cache_write: 4.0,
        total_tokens: 10.0,
        cost: UsageCost {
            input: 0.1,
            output: 0.2,
            cache_read: 0.3,
            cache_write: 0.4,
            total: 1.0,
        },
        ..Usage::default()
    };
    let expected = usage.clone();
    tool.execute = Arc::new(move |id, args, signal, updates| {
        let f = original(id, args, signal, updates);
        let usage = usage.clone();
        Box::pin(async move {
            let mut result = f.await?;
            result.usage = Some(usage);
            Ok(result)
        })
    });
    let patched = Usage {
        input: 5.0,
        output: 6.0,
        cache_read: 7.0,
        cache_write: 8.0,
        total_tokens: 26.0,
        cost: UsageCost {
            input: 0.5,
            output: 0.6,
            cache_read: 0.7,
            cache_write: 0.8,
            total: 2.6,
        },
        ..Usage::default()
    };
    let patch = patched.clone();
    let mut conf = config();
    conf.after_tool_call = Some(Arc::new(move |ctx, _| {
        assert_eq!(ctx.result.read(|r| r.usage.clone()), Some(expected.clone()));
        let patched = patch.clone();
        Box::pin(async move {
            Ok(Some(AfterToolCallResult {
                usage: Some(patched),
                ..Default::default()
            }))
        })
    }));
    let (provider, _) = stream(vec![calls(&["hello"]), answer("done")]);
    let (events, messages) = run(conf, vec![tool], provider).await;
    assert_eq!(executed.snapshot(), [JsValue::from("hello")]);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolExecutionStart { .. }))
    );
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::ToolExecutionEnd {
            is_error: false,
            ..
        }
    )));
    assert_eq!(tool_results(&messages)[0].usage, Some(patched));
}
#[tokio::test]
async fn should_not_execute_tool_calls_from_a_length_truncated_assistant_message() {
    let executed = Shared::default();
    let mut message = calls(&["hel"]);
    message.stop_reason = StopReason::Length;
    let (provider, count) = stream(vec![message, answer("done")]);
    let (events, messages) = run(config(), vec![echo(executed.clone())], provider).await;
    assert!(executed.is_empty());
    let error = events
        .iter()
        .find_map(|e| {
            if let AgentEvent::ToolExecutionEnd {
                result, is_error, ..
            } = e
            {
                assert!(*is_error);
                Some(result.read(|result| content_text(result.content.as_ref().unwrap(), "\n")))
            } else {
                None
            }
        })
        .unwrap();
    assert!(error.as_str().unwrap().contains("output token limit"));
    assert_eq!(count.snapshot(), 2);
    assert_eq!(messages.last().unwrap().role(), "assistant");
}
#[tokio::test]
async fn should_execute_mutated_before_tool_call_args_without_revalidation() {
    let executed = Shared::default();
    let mut conf = config();
    conf.before_tool_call = Some(Arc::new(|ctx, _| {
        ctx.args
            .update(|args| args["value"] = JsValue::Number(123.0));
        Box::pin(async { Ok(None) })
    }));
    let (provider, _) = stream(vec![calls(&["hello"]), answer("done")]);
    run(conf, vec![echo(executed.clone())], provider).await;
    assert_eq!(executed.snapshot(), [JsValue::Number(123.0)]);
}
#[tokio::test]
async fn should_prepare_tool_arguments_for_validation() {
    let executed = Shared::default();
    let capture = executed.clone();
    let mut tool = echo(Shared::default());
    tool.tool.name = "edit".into();
    tool.parameters = Schema::typebox(
        json!({"type":"object","properties":{"edits":{"type":"array","items":{"type":"object","properties":{"oldText":{"type":"string"},"newText":{"type":"string"}},"required":["oldText","newText"]}}},"required":["edits"]}),
    );
    tool.prepare_arguments = Some(Arc::new(|args| {
        let args = args.snapshot();
        let mut edits = args
            .get("edits")
            .and_then(JsValue::as_array)
            .cloned()
            .unwrap_or_default();
        if let (Some(old), Some(new)) = (args.get("oldText"), args.get("newText")) {
            edits.push(JsValue::Object(
                [("oldText", old.clone()), ("newText", new.clone())]
                    .into_iter()
                    .collect(),
            ));
        }
        Ok(JsValue::Object([("edits", JsValue::Array(edits))].into_iter().collect()).into())
    }));
    tool.execute = Arc::new(move |_, args, _, _| {
        capture.push(args.read(|v| v["edits"].clone()));
        Box::pin(async { Ok(result("edited 1")) })
    });
    let (provider, _) = stream(vec![
        assistant(
            vec![AssistantContent::ToolCall(call(
                "tool-1",
                "edit",
                json!({"oldText":"before","newText":"after"}),
            ))],
            StopReason::ToolUse,
        ),
        answer("done"),
    ]);
    run(config(), vec![tool], provider).await;
    assert_eq!(
        executed.snapshot(),
        [js(json!([{ "oldText":"before","newText":"after"}]))]
    );
}

async fn concurrency_case(sequential: bool, multiple: bool) -> (bool, Vec<AgentEvent>) {
    let resolved = Shared::new(false);
    let parallel = Shared::new(false);
    let mut tool = echo(Shared::default());
    let first = resolved.clone();
    let seen = parallel.clone();
    tool.execution_mode = Some(if sequential {
        ToolExecutionMode::Sequential
    } else {
        ToolExecutionMode::Parallel
    });
    tool.execute = Arc::new(move |_, args, _, _| {
        let first = first.clone();
        let seen = seen.clone();
        let value = args.read(|v| v["value"].clone());
        Box::pin(async move {
            if value == JsValue::from("first") {
                tokio::task::yield_now().await;
                first.update(|v| *v = true);
            }
            if value == JsValue::from("second") && !first.snapshot() {
                seen.update(|v| *v = true);
            }
            Ok(result(value.as_js_str().unwrap().clone()))
        })
    });
    let mut tools = vec![tool.clone()];
    let mut message = calls(&["first", "second"]);
    if multiple {
        let mut other = tool;
        other.tool.name = "other".into();
        other.execution_mode = Some(ToolExecutionMode::Parallel);
        tools.push(other);
        if let AssistantContent::ToolCall(call) = &mut message.content[1] {
            call.name = "other".into();
        }
    }
    let (provider, _) = stream(vec![message, answer("done")]);
    let (events, _) = run(config(), tools, provider).await;
    (parallel.snapshot(), events)
}
#[tokio::test]
async fn should_emit_tool_execution_end_in_completion_order_but_persist_tool_results_in_source_order()
 {
    let (parallel, events) = concurrency_case(false, false).await;
    let ends = events
        .iter()
        .filter_map(|e| {
            if let AgentEvent::ToolExecutionEnd { tool_call_id, .. } = e {
                Some(tool_call_id.clone())
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    let results = events
        .iter()
        .filter_map(|e| {
            if let AgentEvent::MessageEnd { message } = e {
                match message.as_llm() {
                    Some(Message::ToolResult(m)) => Some(m.tool_call_id),
                    _ => None,
                }
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    let turn_results = events
        .iter()
        .flat_map(|e| {
            if let AgentEvent::TurnEnd { tool_results, .. } = e {
                tool_results
                    .iter()
                    .map(|m| m.read(|m| m.tool_call_id.clone()))
                    .collect::<Vec<_>>()
            } else {
                vec![]
            }
        })
        .collect::<Vec<_>>();
    assert!(parallel);
    assert_eq!(ends, ["tool-2", "tool-1"]);
    assert_eq!(results, ["tool-1", "tool-2"]);
    assert_eq!(turn_results, ["tool-1", "tool-2"]);
}
#[tokio::test]
async fn should_force_sequential_execution_when_a_tool_has_execution_mode_sequential_even_with_default_parallel_config()
 {
    assert!(!concurrency_case(true, false).await.0);
}
#[tokio::test]
async fn should_force_sequential_execution_when_one_of_multiple_tools_has_execution_mode_sequential()
 {
    assert!(!concurrency_case(true, true).await.0);
}
#[tokio::test]
async fn should_allow_parallel_execution_when_all_tools_have_execution_mode_parallel() {
    assert!(concurrency_case(false, true).await.0);
}
#[tokio::test]
async fn should_inject_queued_messages_after_all_tool_calls_complete() {
    let executed = Shared::default();
    let delivered = Shared::new(false);
    let mut conf = config();
    conf.tool_execution = Some(ToolExecutionMode::Sequential);
    let values = executed.clone();
    conf.get_steering_messages = Some(Arc::new(move || {
        let messages = if !values.is_empty() && !delivered.snapshot() {
            delivered.update(|v| *v = true);
            vec![user("interrupt")]
        } else {
            vec![]
        };
        Box::pin(async move { Ok(messages) })
    }));
    let (provider, count) = stream(vec![calls(&["first", "second"]), answer("done")]);
    let original = provider;
    let seen = Shared::new(false);
    let capture = seen.clone();
    let provider: StreamFn = Arc::new(move |model, ctx, opts| {
        if count.snapshot() == 1 {
            capture.update(|v| *v = user_texts(&ctx.messages).contains(&"interrupt".into()));
        }
        original(model, ctx, opts)
    });
    let (events, _) = run(conf, vec![echo(executed.clone())], provider).await;
    assert_eq!(
        executed.snapshot(),
        [JsValue::from("first"), JsValue::from("second")]
    );
    let sequence = events
        .iter()
        .filter_map(|e| {
            if let AgentEvent::MessageStart { message } = e {
                match message.as_llm() {
                    Some(Message::ToolResult(m)) => {
                        Some(format!("tool:{}", m.tool_call_id.as_str().unwrap()))
                    }
                    Some(Message::User(m)) => {
                        Some(content_text(&m.content, "\n").into_string().unwrap())
                    }
                    _ => None,
                }
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    let interrupt = sequence.iter().position(|v| v == "interrupt").unwrap();
    assert!(sequence.iter().position(|v| v == "tool:tool-1").unwrap() < interrupt);
    assert!(sequence.iter().position(|v| v == "tool:tool-2").unwrap() < interrupt);
    assert!(seen.snapshot());
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(
                e,
                AgentEvent::ToolExecutionEnd {
                    is_error: false,
                    ..
                }
            ))
            .count(),
        2
    );
}
#[tokio::test]
async fn runs_finish_turn_after_tool_result_messages_and_before_turn_end() {
    let ordering = Shared::new(Vec::<String>::new());
    let mut tool = echo(Shared::default());
    let original = tool.execute.clone();
    tool.execute = Arc::new(move |id, args, s, u| {
        let f = original(id, args, s, u);
        Box::pin(async move {
            let mut value = f.await?;
            value.terminate = Some(true);
            Ok(value)
        })
    });
    let mut conf = config();
    let order = ordering.clone();
    conf.finish_turn = Some(Arc::new(move |turn, _| {
        assert_eq!(turn.tool_results.len(), 1);
        assert_eq!(turn.context.messages.last().unwrap().role(), "toolResult");
        order.push("finishTurn".into());
        Box::pin(async { Ok(None) })
    }));
    let order = ordering.clone();
    let emit: AgentEventSink = Arc::new(move |e| {
        match e {
            AgentEvent::MessageEnd { message } => {
                order.push(format!("message_end:{}", message.role().as_str().unwrap()))
            }
            AgentEvent::TurnEnd { .. } => order.push("turn_end".into()),
            _ => {}
        }
        Box::pin(async { Ok(()) })
    });
    let (provider, _) = stream(vec![calls(&["hello"])]);
    run_agent_loop(
        vec![user("echo")],
        context(vec![tool]),
        conf,
        emit,
        None,
        Some(provider),
        env(),
    )
    .await
    .unwrap();
    let order = ordering.snapshot();
    assert_eq!(
        &order[order.len() - 3..],
        ["message_end:toolResult", "finishTurn", "turn_end"]
    );
}
async fn hard_exit(reason: StopReason) {
    let ordering = Shared::new(Vec::<String>::new());
    let steering = Shared::new(0);
    let follow = Shared::new(0);
    let mut conf = config();
    let order = ordering.clone();
    conf.finish_turn = Some(Arc::new(move |turn, _| {
        assert_eq!(turn.message.snapshot().stop_reason, reason);
        order.push("finishTurn".into());
        Box::pin(async { Ok(Some(AgentTurnDecision::Continue)) })
    }));
    let count = steering.clone();
    conf.get_steering_messages = Some(Arc::new(move || {
        count.update(|n| *n += 1);
        Box::pin(async { Ok(vec![]) })
    }));
    let count = follow.clone();
    conf.get_follow_up_messages = Some(Arc::new(move || {
        count.update(|n| *n += 1);
        Box::pin(async { Ok(vec![user("queued")]) })
    }));
    let order = ordering.clone();
    let emit: AgentEventSink = Arc::new(move |event| {
        if matches!(event, AgentEvent::TurnEnd { .. }) {
            order.push("turn_end".into());
        }
        Box::pin(async { Ok(()) })
    });
    let mut message = assistant(vec![], reason);
    message.error_message = Some(reason.as_str().into());
    let (provider, count) = stream(vec![message]);
    run_agent_loop(
        vec![user("run")],
        context(vec![]),
        conf,
        emit,
        None,
        Some(provider),
        env(),
    )
    .await
    .unwrap();
    assert_eq!(ordering.snapshot(), ["finishTurn", "turn_end"]);
    assert_eq!(count.snapshot(), 1);
    assert_eq!(steering.snapshot(), 1);
    assert_eq!(follow.snapshot(), 0);
}
#[tokio::test]
async fn runs_finish_turn_for_an_error_assistant_before_turn_end_without_changing_the_hard_exit() {
    hard_exit(StopReason::Error).await;
}
#[tokio::test]
async fn runs_finish_turn_for_an_aborted_assistant_before_turn_end_without_changing_the_hard_exit()
{
    hard_exit(StopReason::Aborted).await;
}
#[tokio::test]
async fn action_end_skips_queue_polling_and_next_turn_preparation() {
    let steering = Shared::new(0);
    let follow = Shared::new(0);
    let prepare = Shared::new(0);
    let mut conf = config();
    conf.finish_turn = Some(Arc::new(|_, _| {
        Box::pin(async { Ok(Some(AgentTurnDecision::End)) })
    }));
    let n = steering.clone();
    conf.get_steering_messages = Some(Arc::new(move || {
        n.update(|v| *v += 1);
        Box::pin(async { Ok(vec![]) })
    }));
    let n = follow.clone();
    conf.get_follow_up_messages = Some(Arc::new(move || {
        n.update(|v| *v += 1);
        Box::pin(async { Ok(vec![user("queued")]) })
    }));
    let n = prepare.clone();
    conf.prepare_next_turn = Some(Arc::new(move |_| {
        n.update(|v| *v += 1);
        Box::pin(async { Ok(None) })
    }));
    let (provider, count) = stream(vec![calls(&["hello"])]);
    run(conf, vec![echo(Shared::default())], provider).await;
    assert_eq!(count.snapshot(), 1);
    assert_eq!(steering.snapshot(), 1);
    assert_eq!(follow.snapshot(), 0);
    assert_eq!(prepare.snapshot(), 0);
}
fn continue_first(conf: &mut AgentLoopConfig) -> Shared<usize> {
    let count = Shared::new(0);
    let captured = count.clone();
    conf.finish_turn = Some(Arc::new(move |_, _| {
        let n = captured.update(|v| {
            *v += 1;
            *v
        });
        Box::pin(async move { Ok((n == 1).then_some(AgentTurnDecision::Continue)) })
    }));
    count
}
#[tokio::test]
async fn makes_exactly_one_context_only_request_when_no_natural_request_satisfies_continuation() {
    let mut conf = config();
    let finish = continue_first(&mut conf);
    let (provider, count) = stream(vec![answer("response 1"), answer("response 2")]);
    run(conf, vec![], provider).await;
    assert_eq!(count.snapshot(), 2);
    assert_eq!(finish.snapshot(), 2);
}
#[tokio::test]
async fn lets_a_natural_tool_result_request_satisfy_continuation() {
    let mut conf = config();
    let finish = continue_first(&mut conf);
    let (provider, count) = stream(vec![calls(&["hello"]), answer("done")]);
    run(conf, vec![echo(Shared::default())], provider).await;
    assert_eq!(count.snapshot(), 2);
    assert_eq!(finish.snapshot(), 2);
}
async fn natural_queue(steering: bool) {
    let mut conf = config();
    let finish = continue_first(&mut conf);
    let polls = Shared::new(0);
    let delivered = Shared::new(false);
    let text = if steering { "steering" } else { "follow-up" };
    conf.get_steering_messages = Some(Arc::new(move || {
        let count = polls.update(|n| {
            *n += 1;
            *n
        });
        Box::pin(async move {
            Ok(if steering && count == 2 {
                vec![user(text)]
            } else {
                vec![]
            })
        })
    }));
    conf.get_follow_up_messages = Some(Arc::new(move || {
        let send = !steering
            && !delivered.update(|v| {
                let old = *v;
                *v = true;
                old
            });
        Box::pin(async move { Ok(if send { vec![user(text)] } else { vec![] }) })
    }));
    let (provider, count) = stream(vec![answer("done"), answer("done")]);
    let original = provider;
    let seen = Shared::new(Vec::new());
    let capture = seen.clone();
    let n = count.clone();
    let provider: StreamFn = Arc::new(move |model, ctx, options| {
        if n.snapshot() == 1 {
            capture.update(|v| *v = user_texts(&ctx.messages));
        }
        original(model, ctx, options)
    });
    run(conf, vec![], provider).await;
    assert_eq!(count.snapshot(), 2);
    assert_eq!(finish.snapshot(), 2);
    assert!(seen.snapshot().contains(&text.into()));
}
#[tokio::test]
async fn lets_a_natural_steering_request_satisfy_continuation() {
    natural_queue(true).await;
}
#[tokio::test]
async fn lets_a_natural_follow_up_request_satisfy_continuation() {
    natural_queue(false).await;
}
#[tokio::test]
async fn prepares_the_initial_request_after_pending_messages_and_can_replace_request_state() {
    let steering = user("steering");
    let completed = Shared::new(Vec::<AgentMessage>::new());
    let delivered = Shared::new(false);
    let mut conf = config();
    let steered = steering.clone();
    conf.get_steering_messages = Some(Arc::new(move || {
        let sent = delivered.update(|v| {
            let old = *v;
            *v = true;
            old
        });
        let messages = if sent { vec![] } else { vec![steered.clone()] };
        Box::pin(async move { Ok(messages) })
    }));
    let count = Shared::new(0);
    let n = count.clone();
    let seen = completed.clone();
    conf.prepare_request = Some(Arc::new(move |request, _| {
        n.update(|v| *v += 1);
        assert!(seen.read(|v| v.iter().any(|m| m.ptr_eq(&steering))));
        assert!(
            request
                .context
                .messages
                .read(|v| v.iter().any(|m| m.ptr_eq(&steering)))
        );
        let mut replacement = model();
        replacement.id = "replacement".into();
        replacement.name = "replacement".into();
        Box::pin(async move {
            Ok(Some(AgentRequestUpdate {
                context: Some(AgentContext::new(vec![user("canonical projection")], None)),
                model: Some(Shared::new(replacement)),
                thinking_level: Some(ThinkingLevel::High),
            }))
        })
    }));
    let capture = completed.clone();
    let emit: AgentEventSink = Arc::new(move |event| {
        if let AgentEvent::MessageEnd { message } = event {
            capture.push(message);
        }
        Box::pin(async { Ok(()) })
    });
    let provider: StreamFn = Arc::new(|m, ctx, options| {
        assert_eq!(m.id, "replacement");
        assert_eq!(user_texts(&ctx.messages), ["canonical projection"]);
        assert_eq!(
            options.unwrap().reasoning,
            Some(pi_ai::types::ThinkingLevel::High)
        );
        Box::pin(async { Ok(response(answer("done"))) })
    });
    run_agent_loop(
        vec![user("prompt")],
        context(vec![]),
        conf,
        emit,
        None,
        Some(provider),
        env(),
    )
    .await
    .unwrap();
    assert_eq!(count.snapshot(), 1);
}
#[tokio::test]
async fn does_not_poll_steering_after_prepare_request() {
    let queued = Shared::new(Vec::new());
    let polls = Shared::new(0);
    let preparations = Shared::new(0);
    let mut conf = config();
    let queue = queued.clone();
    let n = polls.clone();
    conf.get_steering_messages = Some(Arc::new(move || {
        n.update(|v| *v += 1);
        let messages = queue.update(std::mem::take);
        Box::pin(async move { Ok(messages) })
    }));
    let n = preparations.clone();
    conf.prepare_request = Some(Arc::new(move |_, _| {
        if n.update(|v| {
            *v += 1;
            *v
        }) == 1
        {
            queued.push(user("late steering"));
        }
        Box::pin(async { Ok(None) })
    }));
    let included = Shared::new(Vec::new());
    let capture = included.clone();
    let provider: StreamFn = Arc::new(move |_, ctx, _| {
        capture.push(user_texts(&ctx.messages).contains(&"late steering".into()));
        Box::pin(async { Ok(response(answer("done"))) })
    });
    run(conf, vec![], provider).await;
    assert_eq!(included.snapshot(), [false, true]);
    assert_eq!(preparations.snapshot(), 2);
    assert_eq!(polls.snapshot(), 3);
}
#[tokio::test]
async fn should_use_prepare_next_turn_snapshot_before_continuing() {
    let count = Shared::new(0);
    let n = count.clone();
    let mut conf = config();
    conf.prepare_next_turn = Some(Arc::new(move |turn| {
        n.update(|v| *v += 1);
        let context = AgentContext {
            messages: turn.context.messages.copy_array(),
            tools: turn.context.tools,
        };
        Box::pin(async move {
            Ok(Some(AgentLoopTurnUpdate {
                context: Some(context),
                messages: Some(vec![
                    SystemMessage {
                        content: "updated guidance".into(),
                        timestamp: 1.0,
                        ..Default::default()
                    }
                    .into(),
                ]),
                ..Default::default()
            }))
        })
    }));
    let (provider, calls_count) = stream(vec![calls(&["hello"]), answer("done")]);
    let original = provider;
    let seen = Shared::new(false);
    let capture = seen.clone();
    let calls = calls_count.clone();
    let provider: StreamFn = Arc::new(move |model, ctx, options| {
        if calls.snapshot() == 1 {
            capture.update(|v|*v=ctx.messages.iter().any(|m|matches!(m,Message::System(m) if content_text(&m.content,"\n")=="updated guidance")));
        }
        original(model, ctx, options)
    });
    run(conf, vec![echo(Shared::default())], provider).await;
    assert_eq!(calls_count.snapshot(), 2);
    assert_eq!(count.snapshot(), 1);
    assert!(seen.snapshot());
}
#[tokio::test]
async fn picks_up_steering_queued_during_prepare_next_turn_before_the_next_request() {
    let queue = Shared::new(Vec::new());
    let mut conf = config();
    let queued = queue.clone();
    conf.prepare_next_turn = Some(Arc::new(move |_| {
        queued.push(user("late steering"));
        Box::pin(async { Ok(None) })
    }));
    conf.get_steering_messages = Some(Arc::new(move || {
        let messages = queue.update(std::mem::take);
        Box::pin(async move { Ok(messages) })
    }));
    let (provider, count) = stream(vec![calls(&["hello"]), answer("done")]);
    let original = provider;
    let n = count.clone();
    let seen = Shared::new(false);
    let capture = seen.clone();
    let provider: StreamFn = Arc::new(move |model, ctx, options| {
        if n.snapshot() == 1 {
            capture.update(|v| *v = user_texts(&ctx.messages).contains(&"late steering".into()));
        }
        original(model, ctx, options)
    });
    run(conf, vec![echo(Shared::default())], provider).await;
    assert_eq!(count.snapshot(), 2);
    assert!(seen.snapshot());
}
#[tokio::test]
async fn action_end_receives_finalized_turn_context_and_stops_before_queue_polling() {
    let executed = Shared::default();
    let steering = Shared::new(0);
    let follow = Shared::new(0);
    let mut conf = config();
    conf.finish_turn = Some(Arc::new(|turn, _| {
        assert_eq!(
            turn.tool_results
                .iter()
                .map(|m| m.read(|m| m.tool_call_id.clone()))
                .collect::<Vec<_>>(),
            ["tool-1"]
        );
        assert_eq!(
            roles(&turn.context.messages),
            ["system", "user", "assistant", "toolResult"]
        );
        Box::pin(async { Ok(Some(AgentTurnDecision::End)) })
    }));
    let n = steering.clone();
    conf.get_steering_messages = Some(Arc::new(move || {
        n.update(|v| *v += 1);
        Box::pin(async { Ok(vec![]) })
    }));
    let n = follow.clone();
    conf.get_follow_up_messages = Some(Arc::new(move || {
        n.update(|v| *v += 1);
        Box::pin(async { Ok(vec![user("follow up should stay queued")]) })
    }));
    let (provider, count) = stream(vec![calls(&["hello"])]);
    let (events, messages) = run(conf, vec![echo(executed.clone())], provider).await;
    assert_eq!(count.snapshot(), 1);
    assert_eq!(executed.snapshot(), [JsValue::from("hello")]);
    assert_eq!(steering.snapshot(), 1);
    assert_eq!(follow.snapshot(), 0);
    assert_eq!(
        roles(&messages),
        ["system", "user", "assistant", "toolResult"]
    );
    assert_eq!(
        events.iter().map(AgentEvent::kind).collect::<Vec<_>>(),
        [
            "agent_start",
            "turn_start",
            "message_start",
            "message_end",
            "message_start",
            "message_end",
            "message_start",
            "message_end",
            "tool_execution_start",
            "tool_execution_end",
            "message_start",
            "message_end",
            "turn_end",
            "agent_end"
        ]
    );
}
#[tokio::test]
async fn should_stop_after_a_tool_batch_when_every_tool_result_sets_terminate_true() {
    let mut tool = echo(Shared::default());
    let execute = tool.execute.clone();
    tool.execute = Arc::new(move |id, args, s, u| {
        let f = execute(id, args, s, u);
        Box::pin(async move {
            let mut r = f.await?;
            r.terminate = Some(true);
            Ok(r)
        })
    });
    let (provider, count) = stream(vec![calls(&["hello"])]);
    let (events, messages) = run(config(), vec![tool], provider).await;
    assert_eq!(count.snapshot(), 1);
    assert_eq!(
        roles(&messages),
        ["system", "user", "assistant", "toolResult"]
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::TurnEnd { .. }))
            .count(),
        1
    );
}
#[tokio::test]
async fn should_stop_after_a_blocked_tool_call_when_before_tool_call_sets_terminate_true() {
    let executed = Shared::default();
    let mut conf = config();
    conf.before_tool_call = Some(Arc::new(|_, _| {
        Box::pin(async {
            Ok(Some(BeforeToolCallResult {
                block: Some(true),
                reason: Some("Blocked by policy".into()),
                terminate: Some(true),
            }))
        })
    }));
    let (provider, count) = stream(vec![calls(&["hello"])]);
    let (_, messages) = run(conf, vec![echo(executed.clone())], provider).await;
    assert!(executed.is_empty());
    assert_eq!(count.snapshot(), 1);
    let result = &tool_results(&messages)[0];
    assert!(result.is_error);
    assert_eq!(content_text(&result.content, "\n"), "Blocked by policy");
}
#[tokio::test]
async fn should_continue_after_a_mixed_batch_with_one_terminating_blocked_call() {
    let executed = Shared::default();
    let mut conf = config();
    conf.tool_execution = Some(ToolExecutionMode::Parallel);
    conf.before_tool_call = Some(Arc::new(|ctx, _| {
        let block = ctx.args.read(|v| v["value"] == JsValue::from("first"));
        Box::pin(async move {
            Ok(block.then(|| BeforeToolCallResult {
                block: Some(true),
                reason: Some("Blocked first".into()),
                terminate: Some(true),
            }))
        })
    }));
    let (provider, count) = stream(vec![calls(&["first", "second"]), answer("done")]);
    run(conf, vec![echo(executed.clone())], provider).await;
    assert_eq!(executed.snapshot(), [JsValue::from("second")]);
    assert_eq!(count.snapshot(), 2);
}
#[tokio::test]
async fn should_continue_after_parallel_tool_calls_when_not_all_tool_results_terminate() {
    let mut tool = echo(Shared::default());
    let execute = tool.execute.clone();
    tool.execute = Arc::new(move |id, args, s, u| {
        let terminate = args.read(|v| v["value"] == JsValue::from("first"));
        let f = execute(id, args, s, u);
        Box::pin(async move {
            let mut r = f.await?;
            r.terminate = Some(terminate);
            Ok(r)
        })
    });
    let mut conf = config();
    conf.tool_execution = Some(ToolExecutionMode::Parallel);
    let (provider, count) = stream(vec![calls(&["first", "second"]), answer("done")]);
    let (_, messages) = run(conf, vec![tool], provider).await;
    assert_eq!(count.snapshot(), 2);
    assert_eq!(
        roles(&messages),
        [
            "system",
            "user",
            "assistant",
            "toolResult",
            "toolResult",
            "assistant"
        ]
    );
}
#[tokio::test]
async fn should_allow_after_tool_call_to_mark_a_tool_batch_as_terminating() {
    let mut conf = config();
    conf.after_tool_call = Some(Arc::new(|_, _| {
        Box::pin(async {
            Ok(Some(AfterToolCallResult {
                terminate: Some(true),
                ..Default::default()
            }))
        })
    }));
    let (provider, count) = stream(vec![calls(&["hello"])]);
    run(conf, vec![echo(Shared::default())], provider).await;
    assert_eq!(count.snapshot(), 1);
}
#[test]
fn should_throw_when_context_has_no_messages() {
    let (provider, _) = stream(vec![]);
    let error = agent_loop_continue(context(vec![]), config(), None, Some(provider), env())
        .err()
        .unwrap();
    assert_eq!(error.message, "Cannot continue: no messages in context");
}
#[tokio::test]
async fn should_continue_from_existing_context_without_emitting_user_message_events() {
    let ctx = AgentContext::new(vec![user("Hello")], Some(vec![]));
    let (provider, _) = stream(vec![answer("Response")]);
    let (events, messages) =
        collect(agent_loop_continue(ctx, config(), None, Some(provider), env()).unwrap()).await;
    assert_eq!(roles(&messages), ["assistant"]);
    let ended = events
        .iter()
        .filter_map(|e| {
            if let AgentEvent::MessageEnd { message } = e {
                Some(message.role())
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(ended, ["assistant"]);
}
#[tokio::test]
async fn should_allow_custom_message_types_as_last_message_caller_responsibility() {
    let custom = match js(json!({"role":"custom","text":"Hook content","timestamp":0})) {
        JsValue::Object(v) => v,
        _ => unreachable!(),
    };
    let mut conf = config();
    conf.convert_to_llm = Arc::new(|messages| {
        Box::pin(async move {
            Ok(messages.read(|messages| {
                messages
                    .iter()
                    .filter_map(|m| match m.snapshot() {
                        AgentMessageValue::Custom(raw) if raw.role() == "custom" => {
                            Some(Message::User(UserMessage {
                                content: raw
                                    .read(|raw| raw["text"].as_js_str().unwrap().clone())
                                    .into(),
                                timestamp: 0.0,
                                ..Default::default()
                            }))
                        }
                        _ => m.as_llm(),
                    })
                    .collect()
            }))
        })
    });
    let (provider, _) = stream(vec![answer("Response to custom message")]);
    let (_, messages) = collect(
        agent_loop_continue(
            AgentContext::new(vec![custom.into()], Some(vec![])),
            conf,
            None,
            Some(provider),
            env(),
        )
        .unwrap(),
    )
    .await;
    assert_eq!(roles(&messages), ["assistant"]);
}
fn standalone_tools() -> Vec<AgentTool> {
    let mut tool = echo(Shared::default());
    tool.output_schema = Some(tool.parameters.clone());
    tool.execute = Arc::new(|_, args, _, update| {
        if let Some(update) = update {
            update(result("partial"));
        }
        let value = args.read(|v| v["value"].clone());
        Box::pin(async move {
            let mut r = result(value.as_js_str().unwrap().clone());
            r.structured_content = Some(JsValue::Object([("value", value)].into_iter().collect()));
            Ok(r)
        })
    });
    let mut failing = echo(Shared::default());
    failing.tool.name = "failing".into();
    failing.parameters = Schema::typebox(json!({"type":"object","properties":{}}));
    failing.execute = Arc::new(|_, _, _, _| {
        Box::pin(async {
            let mut r = result("bad");
            r.details = Some(js(json!({"partial":true})));
            r.is_error = Some(true);
            Ok(r)
        })
    });
    vec![tool, failing]
}
#[tokio::test]
async fn validates_runs_the_hooks_and_reports_failures_as_error_outcomes() {
    let hooks = Shared::new(Vec::new());
    let updates = Shared::new(Vec::new());
    let h = hooks.clone();
    let before: BeforeToolCall = Arc::new(move |ctx, _| {
        h.push(format!("before {}", ctx.tool_call.id.as_str().unwrap()));
        let block = ctx
            .args
            .read(|v| v.get("value") == Some(&JsValue::from("blocked")));
        Box::pin(async move {
            Ok(block.then(|| BeforeToolCallResult {
                block: Some(true),
                reason: Some("nope".into()),
                terminate: None,
            }))
        })
    });
    let h = hooks.clone();
    let after: AfterToolCall = Arc::new(move |ctx, _| {
        h.push(format!("after {}", ctx.tool_call.id.as_str().unwrap()));
        Box::pin(async { Ok(None) })
    });
    let u = updates.clone();
    let options = RunToolCallOptions {
        hooks: ToolCallHooks {
            before_tool_call: Some(before),
            after_tool_call: Some(after),
        },
        tools: standalone_tools()
            .into_iter()
            .map(Shared::new)
            .collect::<Vec<_>>()
            .into(),
        assistant_message: SharedAssistantMessage::new(answer("")),
        context: AgentContext::default(),
        signal: None,
        on_update: Some(Arc::new(move |v| {
            u.push(v);
            Box::pin(async { Ok(()) })
        })),
    };
    let a = run_tool_call(call("a", "echo", json!({"value":"a"})), options.clone())
        .await
        .unwrap();
    assert!(!a.is_error);
    assert_eq!(a.tool_call.id, "a");
    assert_eq!(
        a.result.read(|result| result.structured_content.clone()),
        Some(js(json!({"value":"a"})))
    );
    assert!(
        run_tool_call(
            call("b", "echo", json!({"value":{"nested":true}})),
            options.clone()
        )
        .await
        .unwrap()
        .is_error
    );
    let c = run_tool_call(
        call("c", "echo", json!({"value":"blocked"})),
        options.clone(),
    )
    .await
    .unwrap();
    assert!(c.is_error);
    assert_eq!(
        c.result
            .read(|result| content_text(result.content.as_ref().unwrap(), "\n")),
        "nope"
    );
    let d = run_tool_call(call("d", "missing", json!({})), options.clone())
        .await
        .unwrap();
    assert!(d.is_error);
    assert_eq!(
        d.result
            .read(|result| content_text(result.content.as_ref().unwrap(), "\n")),
        "Tool missing not found"
    );
    let e = run_tool_call(call("e", "failing", json!({})), options)
        .await
        .unwrap();
    assert!(e.is_error);
    assert_eq!(
        e.result.read(|result| result.details.clone()),
        Some(js(json!({"partial":true})))
    );
    assert_eq!(updates.snapshot(), [result("partial")]);
    assert_eq!(
        hooks.snapshot(),
        ["before a", "after a", "before c", "before e", "after e"]
    );
}
#[tokio::test]
async fn lets_after_tool_call_replace_structured_content_and_drops_it_when_only_content_is_replaced()
 {
    let redacted = vec![UserContent::Text(TextContent::new("redacted"))];
    let overrides = vec![
        AfterToolCallResult {
            content: Some(redacted.clone()),
            ..Default::default()
        },
        AfterToolCallResult {
            structured_content: Some(js(json!({"value":"replaced"}))),
            ..Default::default()
        },
        AfterToolCallResult {
            content: Some(redacted),
            structured_content: Some(js(json!({"value":"both"}))),
            ..Default::default()
        },
        AfterToolCallResult {
            details: Some(js(json!({"note":"kept"}))),
            ..Default::default()
        },
    ];
    let mut seen = Vec::new();
    for after in overrides {
        let options = RunToolCallOptions {
            hooks: ToolCallHooks {
                after_tool_call: Some(Arc::new(move |_, _| {
                    let after = after.clone();
                    Box::pin(async move { Ok(Some(after)) })
                })),
                ..Default::default()
            },
            tools: standalone_tools()
                .into_iter()
                .map(Shared::new)
                .collect::<Vec<_>>()
                .into(),
            assistant_message: answer("").into(),
            context: AgentContext::default(),
            signal: None,
            on_update: None,
        };
        seen.push(
            run_tool_call(call("x", "echo", json!({"value":"original"})), options)
                .await
                .unwrap()
                .result
                .read(|result| result.structured_content.clone()),
        );
    }
    assert_eq!(
        seen,
        [
            None,
            Some(js(json!({"value":"replaced"}))),
            Some(js(json!({"value":"both"}))),
            Some(js(json!({"value":"original"})))
        ]
    );
}
