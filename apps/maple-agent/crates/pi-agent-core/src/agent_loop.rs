//! Turn scheduling, tool execution, and lifecycle events from `agent-loop.ts`.
use crate::stream_fn::get_default_stream_fn;
use crate::types::*;
use futures_util::FutureExt;
use futures_util::future::{join_all, poll_fn};
use futures_util::stream::{FuturesUnordered, StreamExt};
use pi_ai::types::{
    AssistantContent, AssistantMessageEvent, Context, SharedAssistantMessage, StopReason,
    SystemMessage, TextContent, ToolCall, ToolResultMessage, UserContent,
};
use pi_ai::utils::event_stream::EventStream;
use pi_ai::utils::transcript::{
    ToolStateChanges, get_current_tools, get_tool_state_changes, normalize_context,
    to_tool_declaration,
};
use pi_ai::utils::validation::validate_tool_arguments;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;

pub type AgentEventStream = EventStream<AgentEvent, AgentMessages>;
pub use crate::types::AgentEventSink;
#[derive(Clone, Default)]
pub struct ToolCallHooks {
    pub before_tool_call: Option<BeforeToolCall>,
    pub after_tool_call: Option<AfterToolCall>,
}
impl From<&AgentLoopConfig> for ToolCallHooks {
    fn from(v: &AgentLoopConfig) -> Self {
        Self {
            before_tool_call: v.before_tool_call.clone(),
            after_tool_call: v.after_tool_call.clone(),
        }
    }
}
#[derive(Clone)]
pub struct RunToolCallOptions {
    pub hooks: ToolCallHooks,
    pub tools: AgentTools,
    pub assistant_message: SharedAssistantMessage,
    pub context: AgentContext,
    pub signal: Option<CancellationToken>,
    pub on_update: Option<ToolUpdateSink>,
}

fn create_agent_stream() -> AgentEventStream {
    EventStream::new(
        |event| matches!(event, AgentEvent::AgentEnd { .. }),
        |event| match event {
            AgentEvent::AgentEnd { messages } => messages.clone(),
            _ => AgentMessages::default(),
        },
    )
}
pub fn agent_loop(
    prompts: Vec<AgentMessage>,
    context: AgentContext,
    config: AgentLoopConfig,
    signal: Option<CancellationToken>,
    stream_fn: Option<StreamFn>,
    env: Arc<dyn PiEnv>,
) -> AgentEventStream {
    let stream = create_agent_stream();
    let writer = stream.writer();
    let sink_writer = writer.clone();
    let emit: AgentEventSink = Arc::new(move |event| {
        sink_writer.push(event);
        Box::pin(async { Ok(()) })
    });
    stream.set_producer(async move {
        if let Ok(messages) =
            run_agent_loop(prompts, context, config, emit, signal, stream_fn, env).await
        {
            writer.end(Some(messages));
        }
    });
    stream
}
pub fn agent_loop_continue(
    context: AgentContext,
    config: AgentLoopConfig,
    signal: Option<CancellationToken>,
    stream_fn: Option<StreamFn>,
    env: Arc<dyn PiEnv>,
) -> AgentResult<AgentEventStream> {
    validate_continuation(&context)?;
    let stream = create_agent_stream();
    let writer = stream.writer();
    let sink_writer = writer.clone();
    let emit: AgentEventSink = Arc::new(move |event| {
        sink_writer.push(event);
        Box::pin(async { Ok(()) })
    });
    stream.set_producer(async move {
        if let Ok(messages) =
            run_agent_loop_continue(context, config, emit, signal, stream_fn, env).await
        {
            writer.end(Some(messages));
        }
    });
    Ok(stream)
}
fn validate_continuation(context: &AgentContext) -> AgentResult<()> {
    let Some(last) = context.messages.last() else {
        return Err("Cannot continue: no messages in context".into());
    };
    if last.role() == "assistant" {
        return Err("Cannot continue from message role: assistant".into());
    }
    Ok(())
}
pub async fn run_agent_loop(
    prompts: Vec<AgentMessage>,
    context: AgentContext,
    config: AgentLoopConfig,
    emit: AgentEventSink,
    signal: Option<CancellationToken>,
    stream_fn: Option<StreamFn>,
    env: Arc<dyn PiEnv>,
) -> AgentResult<AgentMessages> {
    let (current_context, new_messages) =
        start_prompt_loop(prompts, context, &emit, env.as_ref()).await?;
    run_loop(
        current_context,
        new_messages.clone(),
        config,
        signal,
        emit,
        resolve_stream(stream_fn)?,
        env,
    )
    .await?;
    Ok(new_messages)
}
/// The model-independent startup prefix is also used when a caller explicitly
/// clears Agent.state.model, before rejecting at the typed stream boundary.
pub(crate) async fn start_prompt_loop(
    prompts: Vec<AgentMessage>,
    context: AgentContext,
    emit: &AgentEventSink,
    env: &dyn PiEnv,
) -> AgentResult<(AgentContext, AgentMessages)> {
    let initial_messages = declare_tool_changes(&context, prompts, env)?;
    let new_messages: AgentMessages = initial_messages.clone().into();
    let current_context = AgentContext {
        messages: context.messages.copy_array(),
        ..context
    };
    current_context
        .messages
        .extend(initial_messages.iter().cloned());
    emit(AgentEvent::AgentStart).await?;
    emit(AgentEvent::TurnStart).await?;
    for message in initial_messages {
        emit(AgentEvent::MessageStart {
            message: message.clone(),
        })
        .await?;
        emit(AgentEvent::MessageEnd { message }).await?;
    }
    Ok((current_context, new_messages))
}
pub async fn run_agent_loop_continue(
    context: AgentContext,
    config: AgentLoopConfig,
    emit: AgentEventSink,
    signal: Option<CancellationToken>,
    stream_fn: Option<StreamFn>,
    env: Arc<dyn PiEnv>,
) -> AgentResult<AgentMessages> {
    let new_messages = start_continue_loop(&context, &emit).await?;
    run_loop(
        context,
        new_messages.clone(),
        config,
        signal,
        emit,
        resolve_stream(stream_fn)?,
        env,
    )
    .await?;
    Ok(new_messages)
}
pub(crate) async fn start_continue_loop(
    context: &AgentContext,
    emit: &AgentEventSink,
) -> AgentResult<AgentMessages> {
    validate_continuation(context)?;
    emit(AgentEvent::AgentStart).await?;
    emit(AgentEvent::TurnStart).await?;
    Ok(AgentMessages::default())
}
fn resolve_stream(stream: Option<StreamFn>) -> AgentResult<StreamFn> {
    stream.map(Ok).unwrap_or_else(get_default_stream_fn)
}
async fn get_messages(get: &Option<GetMessages>) -> AgentResult<Vec<AgentMessage>> {
    match get {
        Some(get) => get().await,
        None => Ok(Vec::new()),
    }
}
fn update_thinking(config: &mut AgentLoopConfig, level: Option<ThinkingLevel>) {
    if let Some(level) = level {
        config.reasoning = match level {
            ThinkingLevel::Off => None,
            other => Some(match other {
                ThinkingLevel::Minimal => pi_ai::types::ThinkingLevel::Minimal,
                ThinkingLevel::Low => pi_ai::types::ThinkingLevel::Low,
                ThinkingLevel::Medium => pi_ai::types::ThinkingLevel::Medium,
                ThinkingLevel::High => pi_ai::types::ThinkingLevel::High,
                ThinkingLevel::Xhigh => pi_ai::types::ThinkingLevel::Xhigh,
                ThinkingLevel::Max => pi_ai::types::ThinkingLevel::Max,
                ThinkingLevel::Off => unreachable!(),
            }),
        };
    }
}
fn requested_thinking(config: &AgentLoopConfig) -> ThinkingLevel {
    match config.reasoning {
        None => ThinkingLevel::Off,
        Some(pi_ai::types::ThinkingLevel::Minimal) => ThinkingLevel::Minimal,
        Some(pi_ai::types::ThinkingLevel::Low) => ThinkingLevel::Low,
        Some(pi_ai::types::ThinkingLevel::Medium) => ThinkingLevel::Medium,
        Some(pi_ai::types::ThinkingLevel::High) => ThinkingLevel::High,
        Some(pi_ai::types::ThinkingLevel::Xhigh) => ThinkingLevel::Xhigh,
        Some(pi_ai::types::ThinkingLevel::Max) => ThinkingLevel::Max,
    }
}
async fn run_loop(
    mut context: AgentContext,
    new_messages: AgentMessages,
    mut config: AgentLoopConfig,
    signal: Option<CancellationToken>,
    emit: AgentEventSink,
    stream: StreamFn,
    env: Arc<dyn PiEnv>,
) -> AgentResult<()> {
    let mut last_completed_turn: Option<PrepareNextTurnContext> = None;
    let mut explicit_continuation = false;
    let mut pending_messages = get_messages(&config.get_steering_messages).await?;
    loop {
        let mut has_more_tool_calls = true;
        while has_more_tool_calls || !pending_messages.is_empty() {
            let mut prepared_messages = Vec::new();
            if let Some(last) = &last_completed_turn {
                if let Some(prepare) = &config.prepare_next_turn
                    && let Some(update) = prepare(last.clone()).await?
                {
                    if let Some(next) = update.context {
                        context = next;
                    }
                    prepared_messages = update.messages.unwrap_or_default();
                    if let Some(model) = update.model {
                        config.model = model;
                    }
                    update_thinking(&mut config, update.thinking_level);
                }
                if pending_messages.is_empty() {
                    pending_messages = get_messages(&config.get_steering_messages).await?;
                }
                emit(AgentEvent::TurnStart).await?;
            }
            prepared_messages.append(&mut pending_messages);
            for message in declare_tool_changes(&context, prepared_messages, env.as_ref())? {
                emit(AgentEvent::MessageStart {
                    message: message.clone(),
                })
                .await?;
                emit(AgentEvent::MessageEnd {
                    message: message.clone(),
                })
                .await?;
                context.messages.push(message.clone());
                new_messages.push(message);
            }
            if let Some(prepare) = &config.prepare_request
                && let Some(update) = prepare(
                    PrepareRequestContext {
                        context: context.clone(),
                        model: config.model.clone(),
                        thinking_level: requested_thinking(&config),
                    },
                    signal.clone(),
                )
                .await?
            {
                if let Some(next) = update.context {
                    context = next;
                }
                if let Some(model) = update.model {
                    config.model = model;
                }
                update_thinking(&mut config, update.thinking_level);
            }
            let message =
                stream_assistant_response(&context, &config, signal.clone(), &emit, &stream)
                    .await?;
            new_messages.push(message.clone().into());
            let snapshot = message.snapshot();
            if matches!(
                snapshot.stop_reason,
                StopReason::Error | StopReason::Aborted
            ) {
                let turn = AgentTurnContext {
                    message: message.clone(),
                    tool_results: Vec::new(),
                    context: context.clone(),
                    new_messages: new_messages.clone(),
                };
                if let Some(finish) = &config.finish_turn {
                    finish(turn, signal.clone()).await?;
                }
                emit(AgentEvent::TurnEnd {
                    message: message.into(),
                    tool_results: Vec::new(),
                })
                .await?;
                emit(AgentEvent::AgentEnd {
                    messages: new_messages,
                })
                .await?;
                return Ok(());
            }
            let calls = tool_calls(&message);
            let mut tool_results = Vec::new();
            has_more_tool_calls = false;
            if !calls.is_empty() {
                let batch = if snapshot.stop_reason == StopReason::Length {
                    fail_tool_calls_from_truncated_message(calls, &emit, env.as_ref()).await?
                } else {
                    execute_tool_calls(
                        &context,
                        message.clone(),
                        &config,
                        signal.clone(),
                        &emit,
                        env.clone(),
                    )
                    .await?
                };
                tool_results = batch.messages;
                has_more_tool_calls = !batch.terminate;
                for result in &tool_results {
                    let message: AgentMessage = result.clone().into();
                    context.messages.push(message.clone());
                    new_messages.push(message);
                }
            }
            let turn = AgentTurnContext {
                message: message.clone(),
                tool_results: tool_results.clone(),
                context: context.clone(),
                new_messages: new_messages.clone(),
            };
            last_completed_turn = Some(turn.clone());
            let decision = match &config.finish_turn {
                Some(finish) => finish(turn, signal.clone()).await?,
                None => None,
            };
            emit(AgentEvent::TurnEnd {
                message: message.into(),
                tool_results,
            })
            .await?;
            if decision == Some(AgentTurnDecision::End) {
                emit(AgentEvent::AgentEnd {
                    messages: new_messages,
                })
                .await?;
                return Ok(());
            }
            explicit_continuation = decision == Some(AgentTurnDecision::Continue);
            pending_messages = get_messages(&config.get_steering_messages).await?;
            if has_more_tool_calls || !pending_messages.is_empty() {
                explicit_continuation = false;
            }
        }
        let follow_ups = get_messages(&config.get_follow_up_messages).await?;
        if !follow_ups.is_empty() {
            explicit_continuation = false;
            pending_messages = follow_ups;
            continue;
        }
        if explicit_continuation {
            explicit_continuation = false;
            continue;
        }
        break;
    }
    emit(AgentEvent::AgentEnd {
        messages: new_messages,
    })
    .await
}
fn tool_calls(message: &SharedAssistantMessage) -> Vec<ToolCall> {
    message.read(|v| {
        v.content
            .iter()
            .filter_map(|block| match block {
                AssistantContent::ToolCall(call) => Some(call.clone()),
                _ => None,
            })
            .collect()
    })
}
fn with_tool_changes(mut message: SystemMessage, changes: ToolStateChanges) -> SystemMessage {
    message.tools_added = (!changes.tools_added.is_empty()).then_some(changes.tools_added);
    message.tools_removed = (!changes.tools_removed.is_empty()).then_some(changes.tools_removed);
    message
}
fn with_agent_tool_changes(message: &AgentMessage, changes: ToolStateChanges) -> AgentMessage {
    match message.snapshot() {
        AgentMessageValue::System(message) => with_tool_changes(message, changes).into(),
        AgentMessageValue::Custom(raw) => {
            let mut raw = raw.snapshot();
            raw.remove("toolsAdded");
            raw.remove("toolsRemoved");
            if !changes.tools_added.is_empty() {
                raw.insert(
                    "toolsAdded",
                    pi_ai::utils::js_value::to_js_value(&changes.tools_added)
                        .expect("tool declarations serialize"),
                );
            }
            if !changes.tools_removed.is_empty() {
                raw.insert(
                    "toolsRemoved",
                    pi_ai::utils::js_value::to_js_value(&changes.tools_removed)
                        .expect("tool references serialize"),
                );
            }
            raw.into()
        }
        _ => unreachable!("tool changes apply only to system messages"),
    }
}
fn has_declared_tool_changes(message: &AgentMessage) -> bool {
    match message.snapshot() {
        AgentMessageValue::System(message) => {
            message.tools_added.as_ref().is_some_and(|v| !v.is_empty())
                || message
                    .tools_removed
                    .as_ref()
                    .is_some_and(|v| !v.is_empty())
        }
        AgentMessageValue::Custom(raw) => raw.read(|raw| {
            ["toolsAdded", "toolsRemoved"].iter().any(|key| {
                raw.get(*key)
                    .and_then(JsValue::as_array)
                    .is_some_and(|v| !v.is_empty())
            })
        }),
        _ => false,
    }
}
fn declare_tool_changes(
    context: &AgentContext,
    pending: Vec<AgentMessage>,
    env: &dyn PiEnv,
) -> AgentResult<Vec<AgentMessage>> {
    let index = pending
        .iter()
        .rposition(|message| message.role() == "system");
    let mut baseline = pending.clone();
    if let Some(index) = index {
        baseline[index] = with_agent_tool_changes(&pending[index], ToolStateChanges::default());
    }
    let mut combined = context.llm_messages();
    combined.extend(baseline.iter().filter_map(AgentMessage::as_llm));
    let current = context
        .tools
        .as_ref()
        .map(|tools| {
            tools.read(|tools| {
                tools
                    .iter()
                    .map(|tool| tool.read(|tool| to_tool_declaration(&tool.tool)))
                    .collect::<Vec<_>>()
            })
        })
        .unwrap_or_default();
    let changes = get_tool_state_changes(
        &get_current_tools(&combined).map_err(AgentError::type_error)?,
        &current,
    );
    let unchanged = changes.tools_added.is_empty() && changes.tools_removed.is_empty();
    if let Some(index) = index {
        if unchanged && !has_declared_tool_changes(&pending[index]) {
            return Ok(pending);
        }
        baseline[index] = with_agent_tool_changes(&pending[index], changes);
        return Ok(baseline);
    }
    if unchanged {
        return Ok(pending);
    }
    let update = with_tool_changes(
        SystemMessage {
            timestamp: env.now_ms() as f64,
            ..SystemMessage::default()
        },
        changes,
    );
    let index = baseline
        .iter()
        .position(|message| message.role() != "system")
        .unwrap_or(baseline.len());
    baseline.insert(index, update.into());
    Ok(baseline)
}

async fn stream_assistant_response(
    context: &AgentContext,
    config: &AgentLoopConfig,
    signal: Option<CancellationToken>,
    emit: &AgentEventSink,
    stream: &StreamFn,
) -> AgentResult<SharedAssistantMessage> {
    let messages = match &config.transform_context {
        Some(transform) => transform(context.messages.clone(), signal.clone()).await?,
        None => context.messages.clone(),
    };
    let llm_messages = (config.convert_to_llm)(messages).await?;
    let llm_context = normalize_context(Context {
        messages: llm_messages,
        ..Context::default()
    });
    let key = match &config.get_api_key {
        Some(get) => get(config.model.read(|model| model.provider.clone())).await?,
        None => None,
    };
    let mut options = config.options.clone();
    // The source spreads the complete loop configuration into provider options.
    // Callback fields are runtime-only; its two additional data fields remain
    // observable to a custom stream function and in serialized request traces.
    options.extra.insert(
        "model",
        pi_ai::utils::js_value::to_js_value(&config.model.snapshot())
            .map_err(|error| AgentError::new(error.to_string()))?,
    );
    if let Some(mode) = config.tool_execution {
        options.extra.insert(
            "toolExecution",
            pi_ai::utils::js_value::to_js_value(&mode)
                .map_err(|error| AgentError::new(error.to_string()))?,
        );
    }
    if let Some(key) = key.filter(|s| !s.is_empty()) {
        options.api_key = Some(key);
    }
    options.signal = signal;
    let mut response = stream(config.model.snapshot(), llm_context, Some(options)).await?;
    let mut partial_message: Option<SharedAssistantMessage> = None;
    let mut added_partial = false;
    while let Some(event) = response.next().await {
        match &event {
            AssistantMessageEvent::Start { partial } => {
                partial_message = Some(partial.clone());
                context.messages.push(partial.clone().into());
                added_partial = true;
                emit(AgentEvent::MessageStart {
                    message: partial.shallow_clone().into(),
                })
                .await?;
            }
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => {
                break;
            }
            _ => {
                if partial_message.is_some() {
                    let partial = match &event {
                        AssistantMessageEvent::TextStart { partial, .. }
                        | AssistantMessageEvent::TextDelta { partial, .. }
                        | AssistantMessageEvent::TextEnd { partial, .. }
                        | AssistantMessageEvent::ThinkingStart { partial, .. }
                        | AssistantMessageEvent::ThinkingDelta { partial, .. }
                        | AssistantMessageEvent::ThinkingEnd { partial, .. }
                        | AssistantMessageEvent::ToolcallStart { partial, .. }
                        | AssistantMessageEvent::ToolcallDelta { partial, .. }
                        | AssistantMessageEvent::ToolcallEnd { partial, .. } => partial.clone(),
                        _ => unreachable!(),
                    };
                    partial_message = Some(partial.clone());
                    context.messages.replace_last(partial.clone().into());
                    emit(AgentEvent::MessageUpdate {
                        message: partial.shallow_clone().into(),
                        assistant_message_event: event,
                    })
                    .await?;
                }
            }
        }
    }
    let mut final_message = response.result().await;
    final_message.thinking_level = Some(requested_thinking(config));
    let final_message = SharedAssistantMessage::new(final_message);
    if added_partial {
        context.messages.replace_last(final_message.clone().into());
    } else {
        context.messages.push(final_message.clone().into());
        emit(AgentEvent::MessageStart {
            message: final_message.shallow_clone().into(),
        })
        .await?;
    }
    emit(AgentEvent::MessageEnd {
        message: final_message.clone().into(),
    })
    .await?;
    Ok(final_message)
}
struct ExecutedToolCallBatch {
    messages: Vec<SharedToolResultMessage>,
    terminate: bool,
}
fn aborted(signal: &Option<CancellationToken>) -> bool {
    signal.as_ref().is_some_and(CancellationToken::is_cancelled)
}
fn create_error_tool_result(message: impl Into<JsString>) -> AgentToolResult {
    AgentToolResult {
        content: Some(vec![UserContent::Text(TextContent::new(message))]),
        details: Some(JsValue::Object(JsObject::new())),
        ..AgentToolResult::default()
    }
}
async fn emit_tool_start(call: &ToolCall, emit: &AgentEventSink) -> AgentResult<()> {
    emit(AgentEvent::ToolExecutionStart {
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        args: call.arguments.clone(),
    })
    .await
}
async fn emit_tool_end(finalized: &AgentToolCallOutcome, emit: &AgentEventSink) -> AgentResult<()> {
    emit(AgentEvent::ToolExecutionEnd {
        tool_call_id: finalized.tool_call.id.clone(),
        tool_name: finalized.tool_call.name.clone(),
        result: finalized.result.clone(),
        is_error: finalized.is_error,
    })
    .await
}
fn create_tool_result_message(
    finalized: &AgentToolCallOutcome,
    env: &dyn PiEnv,
) -> SharedToolResultMessage {
    Shared::new(ToolResultMessage {
        tool_call_id: finalized.tool_call.id.clone(),
        tool_name: finalized.tool_call.name.clone(),
        content: finalized
            .result
            .read(|result| result.content.clone().unwrap_or_default()),
        details: finalized.result.read(|result| result.details.clone()),
        usage: finalized.result.read(|result| result.usage.clone()),
        is_error: finalized.is_error,
        timestamp: env.now_ms() as f64,
        ..ToolResultMessage::default()
    })
}
async fn emit_tool_result_message(
    message: &SharedToolResultMessage,
    emit: &AgentEventSink,
) -> AgentResult<()> {
    let message: AgentMessage = message.clone().into();
    emit(AgentEvent::MessageStart {
        message: message.clone(),
    })
    .await?;
    emit(AgentEvent::MessageEnd { message }).await
}
async fn fail_tool_calls_from_truncated_message(
    calls: Vec<ToolCall>,
    emit: &AgentEventSink,
    env: &dyn PiEnv,
) -> AgentResult<ExecutedToolCallBatch> {
    let mut messages = Vec::new();
    for call in calls {
        emit_tool_start(&call, emit).await?;
        let mut text = JsString::from("Tool call \"");
        text.push(&call.name);
        text.push_str("\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.");
        let finalized = AgentToolCallOutcome {
            tool_call: call,
            result: Shared::new(create_error_tool_result(text)),
            is_error: true,
        };
        emit_tool_end(&finalized, emit).await?;
        let message = create_tool_result_message(&finalized, env);
        emit_tool_result_message(&message, emit).await?;
        messages.push(message);
    }
    Ok(ExecutedToolCallBatch {
        messages,
        terminate: false,
    })
}
struct PreparedToolCall {
    tool_call: ToolCall,
    tool: SharedAgentTool,
    args: SharedArgs,
}
#[allow(clippy::large_enum_variant)] // Short-lived preflight values; no per-call allocation is needed.
enum Preparation {
    Prepared(PreparedToolCall),
    Immediate(AgentToolResult),
}
async fn prepare_tool_call(
    context: AgentContext,
    assistant: SharedAssistantMessage,
    call: ToolCall,
    hooks: &ToolCallHooks,
    signal: Option<CancellationToken>,
    tools: &AgentTools,
) -> Preparation {
    let Some(tool) = tools.read(|tools| {
        tools
            .iter()
            .find(|t| t.read(|tool| tool.name == call.name))
            .cloned()
    }) else {
        let mut text = JsString::from("Tool ");
        text.push(&call.name);
        text.push_str(" not found");
        return Preparation::Immediate(create_error_tool_result(text));
    };
    let prepare = async {
        let mut prepared_call = call.clone();
        if let Some(prepare) = tool.read(|tool| tool.prepare_arguments.clone()) {
            prepared_call.arguments = prepare(call.arguments.clone())?;
        }
        let args = SharedArgs::new(
            tool.read(|tool| validate_tool_arguments(&tool.tool, &prepared_call))
                .map_err(|e| AgentError::new(e.message().clone()))?,
        );
        if let Some(before) = &hooks.before_tool_call {
            let outcome = before(
                BeforeToolCallContext {
                    assistant_message: assistant,
                    tool_call: call.clone(),
                    args: args.clone(),
                    context,
                },
                signal.clone(),
            )
            .await?;
            if aborted(&signal) {
                return Ok(Preparation::Immediate(create_error_tool_result(
                    "Operation aborted",
                )));
            }
            if let Some(result) = outcome.filter(|r| r.block == Some(true)) {
                let reason = result
                    .reason
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "Tool execution was blocked".into());
                let mut result_value = create_error_tool_result(reason);
                if result.terminate == Some(true) {
                    result_value.terminate = Some(true);
                }
                return Ok(Preparation::Immediate(result_value));
            }
        }
        if aborted(&signal) {
            return Ok(Preparation::Immediate(create_error_tool_result(
                "Operation aborted",
            )));
        }
        Ok(Preparation::Prepared(PreparedToolCall {
            tool_call: call,
            tool,
            args,
        }))
    };
    let result: AgentResult<Preparation> = prepare.await;
    match result {
        Ok(value) => value,
        Err(error) => Preparation::Immediate(create_error_tool_result(error.message)),
    }
}
fn emit_tool_update(call: ToolCall, emit: AgentEventSink) -> ToolUpdateSink {
    Arc::new(move |partial_result| {
        emit(AgentEvent::ToolExecutionUpdate {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            args: call.arguments.clone(),
            partial_result,
        })
    })
}
struct ExecutedToolCallOutcome {
    result: AgentToolResult,
    is_error: bool,
}
async fn execute_prepared_tool_call(
    prepared: &PreparedToolCall,
    signal: Option<CancellationToken>,
    on_update: ToolUpdateSink,
) -> AgentResult<ExecutedToolCallOutcome> {
    let accepting = Arc::new(AtomicBool::new(true));
    let queued = Arc::new(Mutex::new(Vec::new()));
    let wake = Arc::new(futures_util::task::AtomicWaker::new());
    let callback: AgentToolUpdateCallback = {
        let accepting = accepting.clone();
        let queued = queued.clone();
        let wake = wake.clone();
        Arc::new(move |partial| {
            if accepting.load(Ordering::SeqCst) {
                let future = on_update(partial);
                queued.lock().expect("tool updates poisoned").push(future);
                wake.wake();
            }
        })
    };
    let mut execute = (prepared.tool.read(|tool| tool.execute.clone()))(
        prepared.tool_call.id.clone(),
        prepared.args.clone(),
        signal,
        Some(callback),
    );
    let mut updates = FuturesUnordered::new();
    let mut settled = None;
    let mut update_error = None;
    poll_fn(|cx| {
        wake.register(cx.waker());
        if settled.is_none()
            && let Poll::Ready(result) = execute.poll_unpin(cx)
        {
            accepting.store(false, Ordering::SeqCst);
            settled = Some(result);
        }
        for update in queued.lock().expect("tool updates poisoned").drain(..) {
            updates.push(update);
        }
        while let Poll::Ready(Some(result)) = updates.poll_next_unpin(cx) {
            if let Err(error) = result
                && update_error.is_none()
            {
                update_error = Some(error);
            }
        }
        if settled.is_some() && (updates.is_empty() || update_error.is_some()) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
    accepting.store(false, Ordering::SeqCst);
    if let Some(error) = update_error {
        // Promise.all rejects when the first observer rejects. The other
        // already-started observers keep running and their outcomes are handled.
        if !updates.is_empty() {
            tokio::spawn(async move { while updates.next().await.is_some() {} });
        }
        return Err(error);
    }
    Ok(match settled.expect("tool future settled") {
        Ok(result) => ExecutedToolCallOutcome {
            is_error: result.is_error == Some(true),
            result,
        },
        Err(error) => ExecutedToolCallOutcome {
            result: create_error_tool_result(error.message),
            is_error: true,
        },
    })
}
async fn finalize_executed_tool_call(
    context: AgentContext,
    assistant: SharedAssistantMessage,
    prepared: PreparedToolCall,
    executed: ExecutedToolCallOutcome,
    hooks: &ToolCallHooks,
    signal: Option<CancellationToken>,
) -> AgentToolCallOutcome {
    let shared_result = Shared::new(executed.result);
    let mut is_error = executed.is_error;
    if let Some(after) = &hooks.after_tool_call {
        match after(
            AfterToolCallContext {
                assistant_message: assistant,
                tool_call: prepared.tool_call.clone(),
                args: prepared.args,
                result: shared_result.clone(),
                is_error,
                context,
            },
            signal,
        )
        .await
        {
            Ok(Some(after)) => shared_result.update(|result| {
                let structured = after
                    .structured_content
                    .filter(|v| !matches!(v, JsValue::Null))
                    .or_else(|| {
                        if after.content.is_some() {
                            None
                        } else {
                            result.structured_content.clone()
                        }
                    });
                if let Some(content) = after.content {
                    result.content = Some(content);
                }
                if let Some(details) = after.details.filter(|v| !matches!(v, JsValue::Null)) {
                    result.details = Some(details);
                }
                if let Some(usage) = after.usage {
                    result.usage = Some(usage);
                }
                if let Some(terminate) = after.terminate {
                    result.terminate = Some(terminate);
                }
                result.structured_content = structured;
                if let Some(value) = after.is_error {
                    is_error = value;
                }
            }),
            Ok(None) => {}
            Err(error) => {
                shared_result.update(|result| *result = create_error_tool_result(error.message));
                is_error = true;
            }
        }
    }
    AgentToolCallOutcome {
        tool_call: prepared.tool_call,
        result: shared_result,
        is_error,
    }
}
fn should_terminate(calls: &[AgentToolCallOutcome]) -> bool {
    !calls.is_empty()
        && calls
            .iter()
            .all(|c| c.result.read(|result| result.terminate == Some(true)))
}
pub async fn run_tool_call(
    call: ToolCall,
    options: RunToolCallOptions,
) -> AgentResult<AgentToolCallOutcome> {
    match prepare_tool_call(
        options.context.clone(),
        options.assistant_message.clone(),
        call.clone(),
        &options.hooks,
        options.signal.clone(),
        &options.tools,
    )
    .await
    {
        Preparation::Immediate(result) => Ok(AgentToolCallOutcome {
            tool_call: call,
            result: Shared::new(result),
            is_error: true,
        }),
        Preparation::Prepared(prepared) => {
            let on_update = options
                .on_update
                .unwrap_or_else(|| Arc::new(|_| Box::pin(async { Ok(()) })));
            let executed =
                execute_prepared_tool_call(&prepared, options.signal.clone(), on_update).await?;
            Ok(finalize_executed_tool_call(
                options.context,
                options.assistant_message,
                prepared,
                executed,
                &options.hooks,
                options.signal,
            )
            .await)
        }
    }
}
async fn execute_tool_calls(
    context: &AgentContext,
    assistant: SharedAssistantMessage,
    config: &AgentLoopConfig,
    signal: Option<CancellationToken>,
    emit: &AgentEventSink,
    env: Arc<dyn PiEnv>,
) -> AgentResult<ExecutedToolCallBatch> {
    let calls = tool_calls(&assistant);
    let tools = context.tools.clone().unwrap_or_default();
    let hooks = ToolCallHooks::from(config);
    let sequential = config.tool_execution == Some(ToolExecutionMode::Sequential)
        || calls.iter().any(|call| {
            tools.read(|tools| {
                tools
                    .iter()
                    .find(|tool| tool.read(|tool| tool.name == call.name))
                    .is_some_and(|tool| {
                        tool.read(|tool| tool.execution_mode == Some(ToolExecutionMode::Sequential))
                    })
            })
        });
    if sequential {
        let mut finalized_calls = Vec::new();
        let mut messages = Vec::new();
        for call in calls {
            emit_tool_start(&call, emit).await?;
            let finalized = match prepare_tool_call(
                context.clone(),
                assistant.clone(),
                call.clone(),
                &hooks,
                signal.clone(),
                &tools,
            )
            .await
            {
                Preparation::Immediate(result) => AgentToolCallOutcome {
                    tool_call: call,
                    result: Shared::new(result),
                    is_error: true,
                },
                Preparation::Prepared(prepared) => {
                    let executed = execute_prepared_tool_call(
                        &prepared,
                        signal.clone(),
                        emit_tool_update(call, emit.clone()),
                    )
                    .await?;
                    finalize_executed_tool_call(
                        context.clone(),
                        assistant.clone(),
                        prepared,
                        executed,
                        &hooks,
                        signal.clone(),
                    )
                    .await
                }
            };
            emit_tool_end(&finalized, emit).await?;
            let message = create_tool_result_message(&finalized, env.as_ref());
            emit_tool_result_message(&message, emit).await?;
            messages.push(message);
            finalized_calls.push(finalized);
            if aborted(&signal) {
                break;
            }
        }
        return Ok(ExecutedToolCallBatch {
            messages,
            terminate: should_terminate(&finalized_calls),
        });
    }
    let mut entries: Vec<AgentFuture<AgentResult<AgentToolCallOutcome>>> = Vec::new();
    for call in calls {
        emit_tool_start(&call, emit).await?;
        match prepare_tool_call(
            context.clone(),
            assistant.clone(),
            call.clone(),
            &hooks,
            signal.clone(),
            &tools,
        )
        .await
        {
            Preparation::Immediate(result) => {
                let finalized = AgentToolCallOutcome {
                    tool_call: call,
                    result: Shared::new(result),
                    is_error: true,
                };
                emit_tool_end(&finalized, emit).await?;
                entries.push(Box::pin(async move { Ok(finalized) }));
            }
            Preparation::Prepared(prepared) => {
                let context = context.clone();
                let assistant = assistant.clone();
                let hooks = hooks.clone();
                let signal = signal.clone();
                let emit = emit.clone();
                entries.push(Box::pin(async move {
                    let finalized = if aborted(&signal) {
                        AgentToolCallOutcome {
                            tool_call: call,
                            result: Shared::new(create_error_tool_result("Operation aborted")),
                            is_error: true,
                        }
                    } else {
                        let executed = execute_prepared_tool_call(
                            &prepared,
                            signal.clone(),
                            emit_tool_update(call, emit.clone()),
                        )
                        .await?;
                        finalize_executed_tool_call(
                            context, assistant, prepared, executed, &hooks, signal,
                        )
                        .await
                    };
                    emit_tool_end(&finalized, &emit).await?;
                    Ok(finalized)
                }));
            }
        }
        if aborted(&signal) {
            break;
        }
    }
    let finalized = join_tool_outcomes(entries).await?;
    let mut messages = Vec::new();
    for result in &finalized {
        let message = create_tool_result_message(result, env.as_ref());
        emit_tool_result_message(&message, emit).await?;
        messages.push(message);
    }
    Ok(ExecutedToolCallBatch {
        messages,
        terminate: should_terminate(&finalized),
    })
}

/// Poll tool futures in source order and retain ordered successful results.
/// An observer failure rejects the batch promptly; already-started siblings
/// continue in an owned driver, just as the source Promise.all keeps its inputs.
async fn join_tool_outcomes<T: Send + 'static>(
    entries: Vec<AgentFuture<AgentResult<T>>>,
) -> AgentResult<Vec<T>> {
    let first_error = Shared::new(None::<AgentError>);
    let mut pending = Box::pin(join_all(entries.into_iter().map(|entry| {
        let first_error = first_error.clone();
        async move {
            let result = entry.await;
            if let Err(error) = &result {
                first_error.update(|first| {
                    if first.is_none() {
                        *first = Some(error.clone());
                    }
                });
            }
            result
        }
    })));
    let mut complete = false;
    let result = poll_fn(|cx| {
        // Poll the entire batch before reporting the failure so every entry's
        // synchronous prefix has started, including siblings after the failure.
        let result = pending.as_mut().poll(cx);
        if let Poll::Ready(results) = result {
            complete = true;
            return Poll::Ready(match first_error.snapshot() {
                Some(error) => Err(error),
                None => results.into_iter().collect(),
            });
        }
        first_error
            .snapshot()
            .map_or(Poll::Pending, |error| Poll::Ready(Err(error)))
    })
    .await;
    if !complete {
        tokio::spawn(async move {
            let _ = pending.await;
        });
    }
    result
}

#[cfg(test)]
mod identity_tests {
    use super::*;
    use pi_ai::types::{AssistantMessage, DoneReason, Model, Schema, Tool, UserMessage};
    use pi_ai::utils::event_stream::create_assistant_message_event_stream;
    use pi_ai::utils::text::content_text;
    use pi_testkit::VirtualEnv;

    #[tokio::test]
    async fn provider_options_retain_enumerable_loop_configuration() {
        let model = Model {
            id: "visible-config-model".into(),
            ..Model::default()
        };
        let expected_model = pi_ai::utils::js_value::to_js_value(&model).unwrap();
        let mut config = AgentLoopConfig::new(
            model,
            Arc::new(|messages| {
                Box::pin(async move {
                    Ok(messages.read(|v| v.iter().filter_map(AgentMessage::as_llm).collect()))
                })
            }),
        );
        config.tool_execution = Some(ToolExecutionMode::Sequential);
        config.temperature = Some(0.4);
        config
            .options
            .extra
            .insert("hostOption", JsValue::from("retained"));
        let stream: StreamFn = Arc::new(move |model, _, options| {
            let options = pi_ai::utils::js_value::to_js_value(&options.unwrap()).unwrap();
            assert_eq!(options["model"], expected_model);
            assert_eq!(options["toolExecution"], JsValue::from("sequential"));
            assert_eq!(options["temperature"], JsValue::from(0.4));
            assert_eq!(options["hostOption"], JsValue::from("retained"));
            Box::pin(async move {
                let stream = create_assistant_message_event_stream();
                stream.push(AssistantMessageEvent::Done {
                    reason: DoneReason::Stop,
                    message: AssistantMessage::new(&model, 0.0),
                });
                Ok(stream)
            })
        });
        let emit: AgentEventSink = Arc::new(|_| Box::pin(async { Ok(()) }));
        stream_assistant_response(&AgentContext::default(), &config, None, &emit, &stream)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn listener_result_mutations_reach_turn_context_and_history() {
        let model = Model::default();
        let call = ToolCall::new("call", "echo", JsValue::Object(JsObject::new()));
        let original_args = call.arguments.clone();
        let mut answer = AssistantMessage::new(&model, 0.0);
        answer.content = vec![AssistantContent::ToolCall(call)];
        answer.stop_reason = StopReason::ToolUse;
        let stream: StreamFn = Arc::new(move |_, _, _| {
            let answer = answer.clone();
            Box::pin(async move {
                let stream = create_assistant_message_event_stream();
                stream.push(AssistantMessageEvent::Done {
                    reason: DoneReason::ToolUse,
                    message: answer,
                });
                Ok(stream)
            })
        });
        let tool = AgentTool {
            tool: Tool {
                name: "echo".into(),
                description: "echo".into(),
                parameters: Schema::json_schema(
                    serde_json::json!({"type":"object","properties":{"fromEvent":{"type":"string"}},"required":["fromEvent"]}),
                ),
                constrained_sampling: None,
            },
            label: "echo".into(),
            prepare_arguments: None,
            output_schema: None,
            replay: None,
            execution_mode: None,
            execute: Arc::new(|_, args, _, _| {
                Box::pin(async move {
                    assert_eq!(
                        args.read(|args| args["fromEvent"].clone()),
                        JsValue::from("listener")
                    );
                    Ok(create_error_tool_result("original"))
                })
            }),
        };
        let mut config = AgentLoopConfig::new(
            model,
            Arc::new(|messages| {
                Box::pin(async move {
                    Ok(messages.read(|v| v.iter().filter_map(AgentMessage::as_llm).collect()))
                })
            }),
        );
        config.finish_turn = Some(Arc::new(|turn, _| {
            assert_eq!(
                turn.tool_results[0].read(|m| content_text(&m.content, "\n")),
                "message listener"
            );
            Box::pin(async { Ok(Some(AgentTurnDecision::End)) })
        }));
        let emit: AgentEventSink = Arc::new(|event| {
            match event {
                AgentEvent::ToolExecutionStart { args, .. } => args.update(|args| {
                    args.as_object_mut()
                        .unwrap()
                        .insert("fromEvent", JsValue::from("listener"));
                }),
                AgentEvent::ToolExecutionEnd { result, .. } => result.update(|result| {
                    result.content = Some(vec![UserContent::Text(TextContent::new(
                        "execution listener",
                    ))])
                }),
                AgentEvent::MessageEnd { message } => message.update(|value| {
                    if let AgentMessageValue::ToolResult(result) = value {
                        result.update(|result| {
                            assert_eq!(content_text(&result.content, "\n"), "execution listener");
                            result.content =
                                vec![UserContent::Text(TextContent::new("message listener"))];
                        });
                    }
                }),
                _ => {}
            }
            Box::pin(async { Ok(()) })
        });
        let messages = run_agent_loop(
            vec![
                UserMessage {
                    content: "run".into(),
                    ..UserMessage::default()
                }
                .into(),
            ],
            AgentContext::new(vec![], Some(vec![tool])),
            config,
            emit,
            None,
            Some(stream),
            Arc::new(VirtualEnv::new(0)),
        )
        .await
        .unwrap();
        let Some(pi_ai::types::Message::ToolResult(result)) = messages.last().unwrap().as_llm()
        else {
            panic!("last message must be tool result")
        };
        assert_eq!(content_text(&result.content, "\n"), "message listener");
        assert_eq!(
            original_args.read(|args| args["fromEvent"].clone()),
            JsValue::from("listener")
        );
    }
}
