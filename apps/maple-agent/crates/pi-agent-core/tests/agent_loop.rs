mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use common::{RecordingSink, ScriptedQueues, echo_tool, result_text, text_schema, user};
use pi_agent_core::{
    AfterToolCall, AfterToolCallResult, AgentContext, AgentError, AgentHooks, AgentLoopConfig,
    AgentMessage, AgentTool, AgentToolResult, BeforeToolCall, BeforeToolCallResult, FnTool,
    NoHooks, RequestContext, RequestUpdate, RunToolCall, ToolError, ToolInvocation, ToolUpdates,
    TurnContext, TurnDecision, TurnUpdate, run_agent_loop, run_agent_loop_continue, run_tool_call,
};
use pi_ai::faux::{FauxProvider, faux_message, faux_tool_call};
use pi_ai::transcript::current_tools;
use pi_ai::{
    AssistantContent, AssistantMessage, Content, Message, StopReason, SystemMessage, ThinkingLevel,
    Tool, UserMessage,
};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

fn config(faux: &FauxProvider) -> AgentLoopConfig<Message> {
    AgentLoopConfig::new(faux.model(), Arc::new(faux.clone()))
}

fn context(tools: Vec<Arc<dyn AgentTool>>) -> AgentContext<Message> {
    AgentContext::new(Vec::new(), tools)
}

async fn run(
    faux: &FauxProvider,
    config: AgentLoopConfig<Message>,
    context: AgentContext<Message>,
    prompt: &str,
) -> (Vec<Message>, RecordingSink<Message>) {
    let sink = RecordingSink::default();
    let messages = run_agent_loop(
        vec![user(prompt)],
        context,
        config,
        &sink,
        &CancellationToken::new(),
    )
    .await;
    let _ = faux;
    (messages, sink)
}

fn roles(messages: &[Message]) -> Vec<&'static str> {
    messages.iter().map(Message::role).collect()
}

#[tokio::test]
async fn a_text_reply_emits_the_full_lifecycle() {
    let faux = FauxProvider::new();
    faux.push_text("Hello there");
    let (messages, sink) = run(&faux, config(&faux), context(Vec::new()), "hi").await;

    assert_eq!(roles(&messages), ["user", "assistant"]);
    assert_eq!(
        sink.lifecycle(),
        [
            "agent_start",
            "turn_start",
            "message_start:user",
            "message_end:user",
            "message_start:assistant",
            "message_end:assistant",
            "turn_end",
            "agent_end"
        ]
    );
    assert!(sink.kinds().contains(&"message_update".to_string()));
    let Message::Assistant(reply) = &messages[1] else {
        panic!()
    };
    assert_eq!(reply.text(), "Hello there");
    assert_eq!(reply.thinking_level, Some(ThinkingLevel::Off));
}

#[tokio::test]
async fn the_request_is_built_from_the_transcript_and_declares_new_tools() {
    let faux = FauxProvider::new();
    faux.push_text("ok");
    let system = Message::System(SystemMessage {
        content: "be brief".into(),
        ..SystemMessage::default()
    });
    let context = AgentContext::new(vec![system], vec![echo_tool()]);
    let (messages, _) = run(&faux, config(&faux), context, "hi").await;

    let request = &faux.requests()[0];
    assert_eq!(
        roles(&request.context.messages),
        ["system", "system", "user"]
    );
    let tools: Vec<String> = current_tools(&request.context.messages)
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    assert_eq!(tools, ["echo"]);
    // The declaration is part of what the run added.
    assert_eq!(roles(&messages), ["system", "user", "assistant"]);
}

#[derive(Clone, Debug)]
enum Note {
    Llm(Box<Message>),
    Remark(String),
}

impl AgentMessage for Note {
    fn from_message(message: Message) -> Self {
        Note::Llm(Box::new(message))
    }

    fn as_message(&self) -> Option<&Message> {
        match self {
            Note::Llm(message) => Some(message.as_ref()),
            Note::Remark(_) => None,
        }
    }
}

struct RemarksAsUserText;

#[async_trait]
impl AgentHooks<Note> for RemarksAsUserText {
    async fn convert_to_llm(&self, messages: &[Note]) -> Vec<Message> {
        messages
            .iter()
            .map(|message| match message {
                Note::Llm(message) => message.as_ref().clone(),
                Note::Remark(text) => Message::User(UserMessage::text(format!("[remark] {text}"))),
            })
            .collect()
    }
}

#[tokio::test]
async fn application_messages_reach_the_model_through_convert_to_llm() {
    let faux = FauxProvider::new();
    faux.push_text("noted");
    let mut config = AgentLoopConfig::<Note>::new(faux.model(), Arc::new(faux.clone()));
    config.hooks = Arc::new(RemarksAsUserText);
    let sink = RecordingSink::default();
    let prompts = vec![
        Note::Remark("remember".into()),
        Note::Llm(Box::new(user("hi"))),
    ];
    let messages = run_agent_loop(
        prompts,
        AgentContext::new(Vec::new(), Vec::new()),
        config,
        &sink,
        &CancellationToken::new(),
    )
    .await;

    assert_eq!(messages.len(), 3);
    let request = &faux.requests()[0];
    let Message::User(first) = &request.context.messages[0] else {
        panic!()
    };
    assert_eq!(pi_ai::content_text(&first.content), "[remark] remember");
    assert_eq!(sink.lifecycle()[2], "message_start:custom");
}

struct KeepLast;

#[async_trait]
impl AgentHooks<Message> for KeepLast {
    async fn transform_context(
        &self,
        messages: &[Message],
        _cancel: &CancellationToken,
    ) -> Option<Vec<Message>> {
        messages.last().cloned().map(|last| vec![last])
    }
}

#[tokio::test]
async fn transform_context_runs_before_conversion_and_leaves_the_transcript_alone() {
    let faux = FauxProvider::new();
    faux.push_text("ok");
    let mut config = config(&faux);
    config.hooks = Arc::new(KeepLast);
    let context = AgentContext::new(vec![user("old"), user("older")], Vec::new());
    let sink = RecordingSink::default();
    run_agent_loop(
        vec![user("new")],
        context,
        config,
        &sink,
        &CancellationToken::new(),
    )
    .await;

    let request = &faux.requests()[0];
    assert_eq!(request.context.messages.len(), 1);
    let Message::User(only) = &request.context.messages[0] else {
        panic!()
    };
    assert_eq!(pi_ai::content_text(&only.content), "new");
}

#[tokio::test]
async fn tool_calls_run_and_their_results_go_back_to_the_model() {
    let faux = FauxProvider::new();
    faux.push_tool_call("echo", json!({ "text": "ping" }));
    faux.push_text("done");
    let (messages, sink) = run(&faux, config(&faux), context(vec![echo_tool()]), "go").await;

    assert_eq!(
        roles(&messages),
        ["system", "user", "assistant", "toolResult", "assistant"]
    );
    assert_eq!(result_text(&messages[3]), "ping");
    let second = &faux.requests()[1];
    assert_eq!(result_text(second.context.messages.last().unwrap()), "ping");
    let lifecycle = sink.lifecycle();
    let start = lifecycle
        .iter()
        .position(|kind| kind == "tool_start:echo")
        .unwrap();
    let end = lifecycle
        .iter()
        .position(|kind| kind == "tool_end:echo")
        .unwrap();
    let result = lifecycle
        .iter()
        .position(|kind| kind == "message_start:toolResult")
        .unwrap();
    assert!(start < end && end < result);
    assert_eq!(
        lifecycle
            .iter()
            .filter(|kind| *kind == "turn_start")
            .count(),
        2
    );
}

#[tokio::test]
async fn calls_from_a_length_truncated_response_are_not_run() {
    let faux = FauxProvider::new();
    let mut truncated = faux_message(vec![faux_tool_call("count", json!({ "text": "x" }))]);
    truncated.stop_reason = StopReason::Length;
    faux.push_reply(truncated);
    faux.push_text("retrying");
    let runs = Arc::new(AtomicUsize::new(0));
    let counter = runs.clone();
    let tool = FnTool::new(Tool::new("count", "Count", text_schema()), move |_| {
        let counter = counter.clone();
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(AgentToolResult::text("ran"))
        }
    })
    .shared();
    let (messages, _) = run(&faux, config(&faux), context(vec![tool]), "go").await;

    assert_eq!(runs.load(Ordering::SeqCst), 0);
    let text = result_text(&messages[3]);
    assert!(
        text.starts_with("Tool call \"count\" was not executed"),
        "{text}"
    );
    assert_eq!(faux.requests().len(), 2);
}

struct Gate {
    decision: Option<BeforeToolCallResult>,
}

#[async_trait]
impl AgentHooks<Message> for Gate {
    async fn before_tool_call(
        &self,
        _call: BeforeToolCall<'_, Message>,
        _cancel: &CancellationToken,
    ) -> Result<Option<BeforeToolCallResult>, ToolError> {
        Ok(self.decision.clone())
    }
}

#[tokio::test]
async fn a_gate_can_replace_arguments_without_revalidation() {
    let faux = FauxProvider::new();
    faux.push_tool_call("echo", json!({ "text": "original" }));
    faux.push_text("done");
    let mut config = config(&faux);
    config.hooks = Arc::new(Gate {
        decision: Some(BeforeToolCallResult::with_args(json!({ "text": 42 }))),
    });
    let (messages, _) = run(&faux, config, context(vec![echo_tool()]), "go").await;
    assert_eq!(result_text(&messages[3]), "42");
}

#[tokio::test]
async fn a_blocked_call_reports_the_reason_and_can_end_the_run() {
    let faux = FauxProvider::new();
    faux.push_tool_call("echo", json!({ "text": "x" }));
    faux.push_text("never requested");
    let mut config = config(&faux);
    let mut decision = BeforeToolCallResult::block("denied by policy");
    decision.terminate = true;
    config.hooks = Arc::new(Gate {
        decision: Some(decision),
    });
    let (messages, _) = run(&faux, config, context(vec![echo_tool()]), "go").await;

    let Message::ToolResult(result) = &messages[3] else {
        panic!()
    };
    assert!(result.is_error);
    assert_eq!(pi_ai::content_text(&result.content), "denied by policy");
    assert_eq!(faux.requests().len(), 1);
    assert_eq!(faux.pending(), 1);
}

struct FailingGate;

#[async_trait]
impl AgentHooks<Message> for FailingGate {
    async fn before_tool_call(
        &self,
        _call: BeforeToolCall<'_, Message>,
        _cancel: &CancellationToken,
    ) -> Result<Option<BeforeToolCallResult>, ToolError> {
        Err("permission store unavailable".into())
    }
}

#[tokio::test]
async fn a_failing_gate_fails_closed() {
    let faux = FauxProvider::new();
    faux.push_tool_call("count", json!({ "text": "x" }));
    faux.push_text("done");
    let runs = Arc::new(AtomicUsize::new(0));
    let counter = runs.clone();
    let tool = FnTool::new(Tool::new("count", "Count", text_schema()), move |_| {
        let counter = counter.clone();
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(AgentToolResult::text("ran"))
        }
    })
    .shared();
    let mut config = config(&faux);
    config.hooks = Arc::new(FailingGate);
    let (messages, _) = run(&faux, config, context(vec![tool]), "go").await;
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert_eq!(result_text(&messages[3]), "permission store unavailable");
}

/// Accepts `{ "value": .. }` from older callers and passes it on as `text`.
struct LegacyEcho {
    declaration: Tool,
}

#[async_trait]
impl AgentTool for LegacyEcho {
    fn declaration(&self) -> &Tool {
        &self.declaration
    }

    fn prepare_arguments(&self, mut arguments: Map<String, Value>) -> Map<String, Value> {
        if let Some(value) = arguments.remove("value") {
            arguments.insert("text".into(), value);
        }
        arguments
    }

    async fn execute(&self, invocation: ToolInvocation) -> Result<AgentToolResult, ToolError> {
        Ok(AgentToolResult::text(
            invocation.args["text"].as_str().unwrap_or_default(),
        ))
    }
}

#[tokio::test]
async fn arguments_are_prepared_before_validation() {
    let faux = FauxProvider::new();
    faux.push_tool_call("echo", json!({ "value": "legacy" }));
    faux.push_text("done");
    let tool = Arc::new(LegacyEcho {
        declaration: Tool::new("echo", "Echo", text_schema()),
    });
    let (messages, _) = run(&faux, config(&faux), context(vec![tool]), "go").await;
    assert_eq!(result_text(&messages[3]), "legacy");
}

struct Explode {
    declaration: Tool,
}

#[async_trait]
impl AgentTool for Explode {
    fn declaration(&self) -> &Tool {
        &self.declaration
    }

    async fn execute(&self, _invocation: ToolInvocation) -> Result<AgentToolResult, ToolError> {
        panic!("boom")
    }
}

#[tokio::test]
async fn invalid_arguments_unknown_tools_and_panics_become_error_results() {
    let faux = FauxProvider::new();
    faux.push_message(vec![
        faux_tool_call("echo", json!({ "wrong": 1 })),
        faux_tool_call("missing", json!({})),
        faux_tool_call("explode", json!({})),
    ]);
    faux.push_text("done");
    let explode = Arc::new(Explode {
        declaration: Tool::new("explode", "Panics", json!({ "type": "object" })),
    });
    let (messages, _) = run(
        &faux,
        config(&faux),
        context(vec![echo_tool(), explode]),
        "go",
    )
    .await;

    let texts: Vec<String> = messages[3..6].iter().map(result_text).collect();
    assert!(
        texts[0].starts_with("Validation failed for tool \"echo\""),
        "{}",
        texts[0]
    );
    assert_eq!(texts[1], "Tool missing not found");
    assert_eq!(texts[2], "Tool explode panicked");
    assert!(
        messages[3..6]
            .iter()
            .all(|message| matches!(message, Message::ToolResult(result) if result.is_error))
    );
}

fn sleeper(
    name: &str,
    millis: u64,
    sequential: bool,
    log: Arc<Mutex<Vec<String>>>,
) -> Arc<dyn AgentTool> {
    let label = name.to_string();
    let tool = FnTool::new(
        Tool::new(name, "Sleeps", json!({ "type": "object" })),
        move |_| {
            let (label, log) = (label.clone(), log.clone());
            async move {
                log.lock().unwrap().push(format!("{label}:start"));
                tokio::time::sleep(Duration::from_millis(millis)).await;
                log.lock().unwrap().push(format!("{label}:end"));
                Ok(AgentToolResult::text(label))
            }
        },
    );
    if sequential {
        tool.sequential().shared()
    } else {
        tool.shared()
    }
}

#[tokio::test]
async fn parallel_calls_end_in_completion_order_but_results_keep_call_order() {
    let faux = FauxProvider::new();
    faux.push_message(vec![
        faux_tool_call("slow", json!({})),
        faux_tool_call("fast", json!({})),
    ]);
    faux.push_text("done");
    let log = Arc::new(Mutex::new(Vec::new()));
    let tools = vec![
        sleeper("slow", 60, false, log.clone()),
        sleeper("fast", 5, false, log.clone()),
    ];
    let (messages, sink) = run(&faux, config(&faux), context(tools), "go").await;

    let ends: Vec<String> = sink
        .lifecycle()
        .into_iter()
        .filter(|kind| kind.starts_with("tool_end"))
        .collect();
    assert_eq!(ends, ["tool_end:fast", "tool_end:slow"]);
    assert_eq!(result_text(&messages[3]), "slow");
    assert_eq!(result_text(&messages[4]), "fast");
    assert!(
        log.lock().unwrap()[..2]
            .iter()
            .all(|entry| entry.ends_with(":start"))
    );
}

#[tokio::test]
async fn one_sequential_tool_makes_the_whole_batch_sequential() {
    let faux = FauxProvider::new();
    faux.push_message(vec![
        faux_tool_call("slow", json!({})),
        faux_tool_call("fast", json!({})),
    ]);
    faux.push_text("done");
    let log = Arc::new(Mutex::new(Vec::new()));
    let tools = vec![
        sleeper("slow", 30, true, log.clone()),
        sleeper("fast", 5, false, log.clone()),
    ];
    run(&faux, config(&faux), context(tools), "go").await;
    assert_eq!(
        *log.lock().unwrap(),
        ["slow:start", "slow:end", "fast:start", "fast:end"]
    );
}

#[tokio::test]
async fn partial_results_are_reported_before_the_end() {
    let faux = FauxProvider::new();
    faux.push_tool_call("progress", json!({}));
    faux.push_text("done");
    let tool = FnTool::new(
        Tool::new("progress", "Reports progress", json!({ "type": "object" })),
        |invocation| async move {
            invocation.updates.send(AgentToolResult::text("25%"));
            invocation.updates.send(AgentToolResult::text("75%"));
            Ok(AgentToolResult::text("100%"))
        },
    )
    .shared();
    let (_, sink) = run(&faux, config(&faux), context(vec![tool]), "go").await;
    let tool_events: Vec<String> = sink
        .lifecycle()
        .into_iter()
        .filter(|kind| kind.contains("progress"))
        .collect();
    assert_eq!(
        tool_events,
        [
            "tool_start:progress",
            "tool_update:progress",
            "tool_update:progress",
            "tool_end:progress"
        ]
    );
}

#[tokio::test]
async fn steering_arrives_after_every_call_of_the_turn() {
    let faux = FauxProvider::new();
    faux.push_message(vec![
        faux_tool_call("echo", json!({ "text": "a" })),
        faux_tool_call("echo", json!({ "text": "b" })),
    ]);
    faux.push_text("adjusted");
    let queues = Arc::new(ScriptedQueues::default());
    queues
        .steering
        .lock()
        .unwrap()
        .extend([Vec::new(), vec![user("change of plan")]]);
    let mut config = config(&faux);
    config.queues = queues;
    let (messages, _) = run(&faux, config, context(vec![echo_tool()]), "go").await;
    assert_eq!(
        roles(&messages),
        [
            "system",
            "user",
            "assistant",
            "toolResult",
            "toolResult",
            "user",
            "assistant"
        ]
    );
}

#[tokio::test]
async fn follow_ups_start_another_turn_when_the_agent_would_stop() {
    let faux = FauxProvider::new();
    faux.push_text("first");
    faux.push_text("second");
    let queues = Arc::new(ScriptedQueues::default());
    queues
        .follow_ups
        .lock()
        .unwrap()
        .push_back(vec![user("and then?")]);
    let mut config = config(&faux);
    config.queues = queues;
    let (messages, sink) = run(&faux, config, context(Vec::new()), "go").await;
    assert_eq!(roles(&messages), ["user", "assistant", "user", "assistant"]);
    assert_eq!(
        sink.lifecycle()
            .iter()
            .filter(|kind| *kind == "agent_end")
            .count(),
        1
    );
}

struct TurnScript {
    decisions: Mutex<Vec<Option<TurnDecision>>>,
    seen_results: Mutex<Vec<usize>>,
}

#[async_trait]
impl AgentHooks<Message> for TurnScript {
    async fn finish_turn(
        &self,
        turn: TurnContext<'_, Message>,
        _cancel: &CancellationToken,
    ) -> Option<TurnDecision> {
        self.seen_results
            .lock()
            .unwrap()
            .push(turn.tool_results.len());
        let mut decisions = self.decisions.lock().unwrap();
        if decisions.is_empty() {
            None
        } else {
            decisions.remove(0)
        }
    }
}

#[tokio::test]
async fn finish_turn_sees_the_results_and_can_end_the_run_without_polling_queues() {
    let faux = FauxProvider::new();
    faux.push_tool_call("echo", json!({ "text": "x" }));
    faux.push_text("never");
    let hooks = Arc::new(TurnScript {
        decisions: Mutex::new(vec![Some(TurnDecision::End)]),
        seen_results: Mutex::new(Vec::new()),
    });
    let queues = Arc::new(ScriptedQueues::default());
    let mut config = config(&faux);
    config.hooks = hooks.clone();
    config.queues = queues.clone();
    let (_, sink) = run(&faux, config, context(vec![echo_tool()]), "go").await;

    assert_eq!(*hooks.seen_results.lock().unwrap(), [1]);
    assert_eq!(faux.requests().len(), 1);
    assert_eq!(*queues.steering_polls.lock().unwrap(), 1);
    let lifecycle = sink.lifecycle();
    assert_eq!(lifecycle[lifecycle.len() - 2..], ["turn_end", "agent_end"]);
}

#[tokio::test]
async fn continue_makes_exactly_one_more_request() {
    let faux = FauxProvider::new();
    faux.push_text("first");
    faux.push_text("second");
    let hooks = Arc::new(TurnScript {
        decisions: Mutex::new(vec![Some(TurnDecision::Continue)]),
        seen_results: Mutex::new(Vec::new()),
    });
    let mut config = config(&faux);
    config.hooks = hooks;
    run(&faux, config, context(Vec::new()), "go").await;
    assert_eq!(faux.requests().len(), 2);
}

struct SwitchModel;

#[async_trait]
impl AgentHooks<Message> for SwitchModel {
    async fn prepare_request(
        &self,
        request: RequestContext<'_, Message>,
        _cancel: &CancellationToken,
    ) -> Option<RequestUpdate<Message>> {
        let mut model = request.model.clone();
        model.id = "routed".into();
        Some(RequestUpdate {
            model: Some(model),
            thinking_level: Some(ThinkingLevel::Low),
            ..RequestUpdate::default()
        })
    }
}

#[tokio::test]
async fn prepare_request_can_route_each_request() {
    let faux = FauxProvider::new();
    faux.push_text("ok");
    let mut config = config(&faux);
    config.hooks = Arc::new(SwitchModel);
    let (messages, _) = run(&faux, config, context(Vec::new()), "go").await;
    let request = &faux.requests()[0];
    assert_eq!(request.model.id, "routed");
    assert_eq!(request.options.reasoning, Some(ThinkingLevel::Low));
    let Message::Assistant(reply) = &messages[1] else {
        panic!()
    };
    assert_eq!(reply.model, "routed");
}

struct InjectBeforeNextTurn;

#[async_trait]
impl AgentHooks<Message> for InjectBeforeNextTurn {
    async fn prepare_next_turn(
        &self,
        turn: TurnContext<'_, Message>,
        _cancel: &CancellationToken,
    ) -> Option<TurnUpdate<Message>> {
        assert_eq!(turn.tool_results.len(), 1);
        Some(TurnUpdate {
            messages: vec![user("context refreshed")],
            thinking_level: Some(ThinkingLevel::High),
            ..TurnUpdate::default()
        })
    }
}

#[tokio::test]
async fn prepare_next_turn_can_append_messages_and_change_settings() {
    let faux = FauxProvider::new();
    faux.push_tool_call("echo", json!({ "text": "x" }));
    faux.push_text("done");
    let mut config = config(&faux);
    config.hooks = Arc::new(InjectBeforeNextTurn);
    run(&faux, config, context(vec![echo_tool()]), "go").await;
    let second = &faux.requests()[1];
    let Message::User(last) = second.context.messages.last().unwrap() else {
        panic!()
    };
    assert_eq!(pi_ai::content_text(&last.content), "context refreshed");
    assert_eq!(second.options.reasoning, Some(ThinkingLevel::High));
}

fn terminating(name: &str, terminate: bool) -> Arc<dyn AgentTool> {
    FnTool::new(
        Tool::new(name, "Maybe stops", json!({ "type": "object" })),
        move |_| async move {
            Ok(AgentToolResult {
                terminate,
                ..AgentToolResult::text("done")
            })
        },
    )
    .shared()
}

#[tokio::test]
async fn a_batch_ends_the_run_only_when_every_result_asks() {
    let faux = FauxProvider::new();
    faux.push_message(vec![
        faux_tool_call("stop", json!({})),
        faux_tool_call("stop", json!({})),
    ]);
    run(
        &faux,
        config(&faux),
        context(vec![terminating("stop", true)]),
        "go",
    )
    .await;
    assert_eq!(faux.requests().len(), 1);

    let faux = FauxProvider::new();
    faux.push_message(vec![
        faux_tool_call("stop", json!({})),
        faux_tool_call("go", json!({})),
    ]);
    faux.push_text("continued");
    run(
        &faux,
        config(&faux),
        context(vec![terminating("stop", true), terminating("go", false)]),
        "go",
    )
    .await;
    assert_eq!(faux.requests().len(), 2);
}

struct Redact;

#[async_trait]
impl AgentHooks<Message> for Redact {
    async fn after_tool_call(
        &self,
        call: AfterToolCall<'_, Message>,
        _cancel: &CancellationToken,
    ) -> Result<Option<AfterToolCallResult>, ToolError> {
        assert_eq!(pi_ai::content_text(&call.result.content), "secret");
        Ok(Some(AfterToolCallResult {
            content: Some(vec![Content::text("[redacted]")]),
            terminate: Some(true),
            ..AfterToolCallResult::default()
        }))
    }
}

#[tokio::test]
async fn after_tool_call_rewrites_results_and_can_terminate() {
    let faux = FauxProvider::new();
    faux.push_tool_call("echo", json!({ "text": "secret" }));
    let mut config = config(&faux);
    config.hooks = Arc::new(Redact);
    let (messages, _) = run(&faux, config, context(vec![echo_tool()]), "go").await;
    assert_eq!(result_text(&messages[3]), "[redacted]");
    assert_eq!(faux.requests().len(), 1);
}

#[tokio::test]
async fn continuing_requires_a_message_the_model_can_answer() {
    let faux = FauxProvider::new();
    let sink = RecordingSink::default();
    let cancel = CancellationToken::new();
    let empty = run_agent_loop_continue(context(Vec::new()), config(&faux), &sink, &cancel).await;
    assert_eq!(empty.unwrap_err(), AgentError::NoMessages);

    let assistant = Message::Assistant(faux_message(vec![AssistantContent::text("hi")]));
    let after_assistant = run_agent_loop_continue(
        AgentContext::new(vec![assistant], Vec::new()),
        config(&faux),
        &sink,
        &cancel,
    )
    .await;
    assert_eq!(
        after_assistant.unwrap_err(),
        AgentError::CannotContinueFromAssistant
    );
    assert!(sink.kinds().is_empty());

    faux.push_text("answer");
    let added = run_agent_loop_continue(
        AgentContext::new(vec![user("question")], Vec::new()),
        config(&faux),
        &sink,
        &cancel,
    )
    .await
    .unwrap();
    assert_eq!(roles(&added), ["assistant"]);
    assert_eq!(
        sink.lifecycle()[..3],
        ["agent_start", "turn_start", "message_start:assistant"]
    );
}

#[tokio::test]
async fn cancelling_mid_stream_ends_the_run_with_an_aborted_response() {
    let faux = FauxProvider::new();
    faux.push_hang();
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let sink = RecordingSink::with_callback(move |event| {
        if matches!(
            event,
            pi_agent_core::AgentEvent::MessageStart {
                message: Message::Assistant(_)
            }
        ) {
            trigger.cancel();
        }
    });
    let messages = run_agent_loop(
        vec![user("go")],
        context(Vec::new()),
        config(&faux),
        &sink,
        &cancel,
    )
    .await;
    let Message::Assistant(reply) = &messages[1] else {
        panic!()
    };
    assert_eq!(reply.stop_reason, StopReason::Aborted);
    let lifecycle = sink.lifecycle();
    assert_eq!(lifecycle[lifecycle.len() - 2..], ["turn_end", "agent_end"]);
}

#[tokio::test]
async fn run_tool_call_applies_the_same_gates_outside_the_loop() {
    let assistant = AssistantMessage::empty(&FauxProvider::default_model());
    let context = context(vec![echo_tool()]);
    let tools = context.tools.clone();
    let cancel = CancellationToken::new();
    let call = pi_ai::ToolCall {
        id: "nested/1".into(),
        name: "echo".into(),
        arguments: json!({ "text": "hi" }).as_object().cloned().unwrap(),
    };
    let allowed = run_tool_call(
        &call,
        RunToolCall {
            tools: &tools,
            assistant_message: &assistant,
            context: &context,
            hooks: &NoHooks,
            cancel: &cancel,
            updates: ToolUpdates::none(),
        },
    )
    .await;
    assert!(!allowed.is_error);
    assert_eq!(pi_ai::content_text(&allowed.result.content), "hi");

    let gate = Gate {
        decision: Some(BeforeToolCallResult::block("no nesting")),
    };
    let blocked = run_tool_call(
        &call,
        RunToolCall {
            tools: &tools,
            assistant_message: &assistant,
            context: &context,
            hooks: &gate,
            cancel: &cancel,
            updates: ToolUpdates::none(),
        },
    )
    .await;
    assert!(blocked.is_error);
    assert_eq!(pi_ai::content_text(&blocked.result.content), "no nesting");
}

struct RedactAssistant;

#[async_trait]
impl AgentHooks<Message> for RedactAssistant {
    async fn finalize_message(&self, message: Message) -> Message {
        match message {
            Message::Assistant(mut reply) if reply.text().contains("secret") => {
                for block in &mut reply.content {
                    if let AssistantContent::Text(text) = block {
                        text.text = "[redacted]".into();
                    }
                }
                Message::Assistant(reply)
            }
            other => other,
        }
    }
}

#[tokio::test]
async fn finalize_message_replaces_what_the_transcript_and_later_requests_see() {
    let faux = FauxProvider::new();
    faux.push_message(vec![
        AssistantContent::text("the secret is 42"),
        faux_tool_call("echo", json!({ "text": "x" })),
    ]);
    faux.push_text("done");
    let mut config = config(&faux);
    config.hooks = Arc::new(RedactAssistant);
    let (messages, sink) = run(&faux, config, context(vec![echo_tool()]), "go").await;

    let Message::Assistant(first) = &messages[2] else {
        panic!()
    };
    assert_eq!(first.text(), "[redacted]");
    let Message::Assistant(sent) = &faux.requests()[1].context.messages[2] else {
        panic!()
    };
    assert_eq!(sent.text(), "[redacted]");
    let ended = sink.events.lock().unwrap().iter().any(|event| matches!(
        event,
        pi_agent_core::AgentEvent::MessageEnd { message: Message::Assistant(reply) } if reply.text() == "[redacted]"
    ));
    assert!(ended);
}
