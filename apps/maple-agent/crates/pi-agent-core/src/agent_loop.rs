use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::stream::FuturesUnordered;
use futures_util::{FutureExt, StreamExt};
use pi_ai::transcript::{current_tools, tool_changes};
use pi_ai::{
    AssistantMessage, AssistantMessageEvent, Context, Message, Model, StopReason, StreamFn,
    StreamOptions, SystemMessage, ThinkingLevel, Tool, ToolCall, ToolResultMessage, now_ms,
    validate_tool_arguments,
};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::hooks::{
    AfterToolCall, AgentHooks, BeforeToolCall, NoHooks, RequestContext, TurnContext, TurnDecision,
};
use crate::types::{
    AgentContext, AgentError, AgentEvent, AgentMessage, AgentTool, AgentToolResult,
    ToolExecutionMode, ToolInvocation, ToolUpdate, ToolUpdates, is_system,
};

/// Receives a run's events in order. The loop waits for each before it goes on.
#[async_trait]
pub trait AgentEventSink<M: AgentMessage>: Send + Sync {
    async fn emit(&self, event: AgentEvent<M>);
}

/// Where a run picks up messages queued while it works.
#[async_trait]
pub trait MessageQueues<M: AgentMessage>: Send + Sync {
    /// Polled after each turn: messages to deliver before the next response.
    async fn steering(&self) -> Vec<M> {
        Vec::new()
    }

    /// Polled when the agent would stop: messages that start another turn.
    async fn follow_ups(&self) -> Vec<M> {
        Vec::new()
    }
}

pub struct NoQueues;

impl<M: AgentMessage> MessageQueues<M> for NoQueues {}

/// Everything a run needs besides its context.
#[derive(Clone)]
pub struct AgentLoopConfig<M: AgentMessage> {
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    pub stream_fn: Arc<dyn StreamFn>,
    pub hooks: Arc<dyn AgentHooks<M>>,
    pub queues: Arc<dyn MessageQueues<M>>,
    pub tool_execution: ToolExecutionMode,
    /// Base request options; the loop sets the key, reasoning and cancellation.
    pub stream_options: StreamOptions,
}

impl<M: AgentMessage> AgentLoopConfig<M> {
    pub fn new(model: Model, stream_fn: Arc<dyn StreamFn>) -> Self {
        Self {
            model,
            thinking_level: ThinkingLevel::Off,
            stream_fn,
            hooks: Arc::new(NoHooks),
            queues: Arc::new(NoQueues),
            tool_execution: ToolExecutionMode::Parallel,
            stream_options: StreamOptions::default(),
        }
    }
}

/// Start a run with new prompt messages. Returns the messages the run added.
pub async fn run_agent_loop<M: AgentMessage>(
    prompts: Vec<M>,
    mut context: AgentContext<M>,
    config: AgentLoopConfig<M>,
    sink: &dyn AgentEventSink<M>,
    cancel: &CancellationToken,
) -> Vec<M> {
    let prompts = declare_tool_changes(&context, prompts);
    let mut new_messages = Vec::with_capacity(prompts.len());
    sink.emit(AgentEvent::AgentStart).await;
    sink.emit(AgentEvent::TurnStart).await;
    for message in prompts {
        let message = emit_message(message, &*config.hooks, sink).await;
        context.messages.push(message.clone());
        new_messages.push(message);
    }
    run_loop(context, &mut new_messages, config, sink, cancel).await;
    new_messages
}

/// Continue from the current context, for retries: the last message must be one the
/// model can answer, such as a user message or tool results.
pub async fn run_agent_loop_continue<M: AgentMessage>(
    context: AgentContext<M>,
    config: AgentLoopConfig<M>,
    sink: &dyn AgentEventSink<M>,
    cancel: &CancellationToken,
) -> Result<Vec<M>, AgentError> {
    let Some(last) = context.messages.last() else {
        return Err(AgentError::NoMessages);
    };
    if matches!(last.as_message(), Some(Message::Assistant(_))) {
        return Err(AgentError::CannotContinueFromAssistant);
    }
    let mut new_messages = Vec::new();
    sink.emit(AgentEvent::AgentStart).await;
    sink.emit(AgentEvent::TurnStart).await;
    run_loop(context, &mut new_messages, config, sink, cancel).await;
    Ok(new_messages)
}

struct CompletedTurn {
    message: AssistantMessage,
    tool_results: Vec<ToolResultMessage>,
}

fn wrap<M: AgentMessage>(message: impl Into<Message>) -> M {
    M::from_message(message.into())
}

/// Emit a complete message and return it as `finalize_message` left it.
async fn emit_message<M: AgentMessage>(
    message: M,
    hooks: &dyn AgentHooks<M>,
    sink: &dyn AgentEventSink<M>,
) -> M {
    sink.emit(AgentEvent::MessageStart {
        message: message.clone(),
    })
    .await;
    let message = hooks.finalize_message(message).await;
    sink.emit(AgentEvent::MessageEnd {
        message: message.clone(),
    })
    .await;
    message
}

async fn run_loop<M: AgentMessage>(
    mut context: AgentContext<M>,
    new_messages: &mut Vec<M>,
    mut config: AgentLoopConfig<M>,
    sink: &dyn AgentEventSink<M>,
    cancel: &CancellationToken,
) {
    let mut last_turn: Option<CompletedTurn> = None;
    let mut explicit_continuation = false;
    // The user may have typed while the run was being set up.
    let mut pending = config.queues.steering().await;

    loop {
        let mut has_more_tool_calls = true;
        while has_more_tool_calls || !pending.is_empty() {
            let mut prepared = Vec::new();
            if let Some(turn) = &last_turn {
                let update = config
                    .hooks
                    .prepare_next_turn(
                        TurnContext {
                            message: &turn.message,
                            tool_results: &turn.tool_results,
                            context: &context,
                            new_messages,
                        },
                        cancel,
                    )
                    .await;
                if let Some(update) = update {
                    if let Some(replacement) = update.context {
                        context = replacement;
                    }
                    prepared = update.messages;
                    config.model = update.model.unwrap_or(config.model);
                    config.thinking_level = update.thinking_level.unwrap_or(config.thinking_level);
                }
                // Preparation can take a while (compaction); pick up steering queued
                // meanwhile, unless this turn already has some.
                if pending.is_empty() {
                    pending = config.queues.steering().await;
                }
                sink.emit(AgentEvent::TurnStart).await;
            }

            prepared.append(&mut pending);
            for message in declare_tool_changes(&context, prepared) {
                let message = emit_message(message, &*config.hooks, sink).await;
                context.messages.push(message.clone());
                new_messages.push(message);
            }

            let update = config
                .hooks
                .prepare_request(
                    RequestContext {
                        context: &context,
                        model: &config.model,
                        thinking_level: config.thinking_level,
                    },
                    cancel,
                )
                .await;
            if let Some(update) = update {
                if let Some(replacement) = update.context {
                    context = replacement;
                }
                config.model = update.model.unwrap_or(config.model);
                config.thinking_level = update.thinking_level.unwrap_or(config.thinking_level);
            }

            let message = stream_assistant_response(&mut context, &config, sink, cancel).await;
            new_messages.push(wrap(message.clone()));

            if message.is_failure() {
                let turn = CompletedTurn {
                    message,
                    tool_results: Vec::new(),
                };
                config
                    .hooks
                    .finish_turn(
                        TurnContext {
                            message: &turn.message,
                            tool_results: &[],
                            context: &context,
                            new_messages,
                        },
                        cancel,
                    )
                    .await;
                sink.emit(AgentEvent::TurnEnd {
                    message: wrap(turn.message),
                    tool_results: Vec::new(),
                })
                .await;
                sink.emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                })
                .await;
                return;
            }

            let calls: Vec<ToolCall> = message.tool_calls().cloned().collect();
            let mut tool_results = Vec::new();
            has_more_tool_calls = false;
            if !calls.is_empty() {
                // A length stop may have cut the arguments off: run none of the calls.
                let batch = if message.stop_reason == StopReason::Length {
                    fail_truncated_calls(&calls, &*config.hooks, sink).await
                } else {
                    execute_tool_calls(&context, &message, &calls, &config, sink, cancel).await
                };
                has_more_tool_calls = !batch.terminate;
                for result in batch.messages {
                    if let Some(Message::ToolResult(tool_result)) = result.as_message() {
                        tool_results.push(tool_result.clone());
                    }
                    context.messages.push(result.clone());
                    new_messages.push(result);
                }
            }

            let turn = CompletedTurn {
                message,
                tool_results,
            };
            let decision = config
                .hooks
                .finish_turn(
                    TurnContext {
                        message: &turn.message,
                        tool_results: &turn.tool_results,
                        context: &context,
                        new_messages,
                    },
                    cancel,
                )
                .await;
            sink.emit(AgentEvent::TurnEnd {
                message: wrap(turn.message.clone()),
                tool_results: turn.tool_results.clone(),
            })
            .await;
            last_turn = Some(turn);

            if decision == Some(TurnDecision::End) {
                sink.emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                })
                .await;
                return;
            }
            explicit_continuation = decision == Some(TurnDecision::Continue);
            pending = config.queues.steering().await;
            if has_more_tool_calls || !pending.is_empty() {
                explicit_continuation = false;
            }
        }

        // The agent would stop here.
        let follow_ups = config.queues.follow_ups().await;
        if !follow_ups.is_empty() {
            explicit_continuation = false;
            pending = follow_ups;
            continue;
        }
        // Nothing else asked for a request, so honor the continuation with one turn.
        if explicit_continuation {
            explicit_continuation = false;
            continue;
        }
        break;
    }

    sink.emit(AgentEvent::AgentEnd {
        messages: new_messages.clone(),
    })
    .await;
}

/// Declare tool-set changes to the model before the next request.
///
/// The context's tools are what can run; the transcript's system messages declare what
/// the model may call. When they differ, the difference is added to the last pending
/// system message, or a new system message is inserted before the first pending
/// non-system message, so replaying the transcript always yields the executable set.
fn declare_tool_changes<M: AgentMessage>(context: &AgentContext<M>, mut pending: Vec<M>) -> Vec<M> {
    let system_index = pending.iter().rposition(is_system);
    let without_pending_changes: Vec<Message> = pending
        .iter()
        .enumerate()
        .filter_map(|(index, message)| match message.as_message() {
            Some(Message::System(system)) if Some(index) == system_index => {
                Some(Message::System(SystemMessage {
                    tools_added: Vec::new(),
                    tools_removed: Vec::new(),
                    ..system.clone()
                }))
            }
            other => other.cloned(),
        })
        .collect();
    let declared = current_tools(
        context
            .messages
            .iter()
            .filter_map(AgentMessage::as_message)
            .chain(without_pending_changes.iter()),
    );
    let executable: Vec<Tool> = context
        .tools
        .iter()
        .map(|tool| tool.declaration().clone())
        .collect();
    let changes = tool_changes(&declared, &executable);

    match system_index {
        Some(index) => {
            let Some(Message::System(system)) = pending[index].as_message() else {
                return pending;
            };
            if changes.is_empty()
                && system.tools_added.is_empty()
                && system.tools_removed.is_empty()
            {
                return pending;
            }
            let updated = SystemMessage {
                tools_added: changes.added,
                tools_removed: changes.removed,
                ..system.clone()
            };
            pending[index] = wrap(updated);
            pending
        }
        None if changes.is_empty() => pending,
        None => {
            let update = SystemMessage {
                tools_added: changes.added,
                tools_removed: changes.removed,
                timestamp: now_ms(),
                ..SystemMessage::default()
            };
            let at = pending
                .iter()
                .position(|message| !is_system(message))
                .unwrap_or(pending.len());
            pending.insert(at, wrap(update));
            pending
        }
    }
}

/// Stream one assistant response, emitting its events, and append it to the context.
async fn stream_assistant_response<M: AgentMessage>(
    context: &mut AgentContext<M>,
    config: &AgentLoopConfig<M>,
    sink: &dyn AgentEventSink<M>,
    cancel: &CancellationToken,
) -> AssistantMessage {
    let transformed = config
        .hooks
        .transform_context(&context.messages, cancel)
        .await;
    let messages = transformed.as_deref().unwrap_or(&context.messages);
    let llm_messages = config.hooks.convert_to_llm(messages).await;
    let api_key = match config.hooks.api_key(&config.model.provider).await {
        Some(key) => Some(key),
        None => config.stream_options.api_key.clone(),
    };
    let options = StreamOptions {
        api_key,
        reasoning: (config.thinking_level != ThinkingLevel::Off).then_some(config.thinking_level),
        cancel: cancel.clone(),
        ..config.stream_options.clone()
    };
    let mut stream =
        config
            .stream_fn
            .stream(&config.model, Context::from_messages(llm_messages), options);

    let mut started = false;
    let mut result = None;
    while let Some(event) = stream.next().await {
        match event {
            AssistantMessageEvent::Start { message } => {
                started = true;
                sink.emit(AgentEvent::MessageStart {
                    message: wrap(message),
                })
                .await;
            }
            AssistantMessageEvent::Done { message } | AssistantMessageEvent::Error { message } => {
                result = Some(message);
                break;
            }
            event => {
                if started {
                    sink.emit(AgentEvent::MessageUpdate { event }).await;
                }
            }
        }
    }
    let mut message = result.unwrap_or_else(|| stream.unterminated());
    message.thinking_level = Some(config.thinking_level);
    if !started {
        sink.emit(AgentEvent::MessageStart {
            message: wrap(message.clone()),
        })
        .await;
    }
    let finalized = config.hooks.finalize_message(wrap(message.clone())).await;
    sink.emit(AgentEvent::MessageEnd {
        message: finalized.clone(),
    })
    .await;
    // A replacement keeps driving the run only if it is still an assistant message.
    if let Some(Message::Assistant(replacement)) = finalized.as_message() {
        message = replacement.clone();
    }
    context.messages.push(finalized);
    message
}

/// The outcome of one tool call.
#[derive(Clone, Debug)]
pub struct ToolCallOutcome {
    pub tool_call: ToolCall,
    pub result: AgentToolResult,
    pub is_error: bool,
}

struct ToolBatch<M> {
    /// Tool-result messages in call order, as `finalize_message` left them.
    messages: Vec<M>,
    terminate: bool,
}

/// What a tool call runs against.
struct CallScope<'a, M: AgentMessage> {
    context: &'a AgentContext<M>,
    tools: &'a [Arc<dyn AgentTool>],
    assistant: &'a AssistantMessage,
    hooks: &'a dyn AgentHooks<M>,
    cancel: &'a CancellationToken,
}

impl<M: AgentMessage> Clone for CallScope<'_, M> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<M: AgentMessage> Copy for CallScope<'_, M> {}

struct ReadyCall {
    call: ToolCall,
    tool: Arc<dyn AgentTool>,
    args: Value,
}

enum Prepared {
    Immediate(ToolCallOutcome),
    Ready(ReadyCall),
}

fn failed(call: &ToolCall, error: impl Into<String>) -> ToolCallOutcome {
    ToolCallOutcome {
        tool_call: call.clone(),
        result: AgentToolResult::error(error),
        is_error: true,
    }
}

const ABORTED: &str = "Operation aborted";

/// Find the tool, prepare and validate the arguments, and run the gate.
async fn prepare_tool_call<M: AgentMessage>(scope: CallScope<'_, M>, call: &ToolCall) -> Prepared {
    let Some(tool) = scope
        .tools
        .iter()
        .find(|tool| tool.name() == call.name)
        .cloned()
    else {
        return Prepared::Immediate(failed(call, format!("Tool {} not found", call.name)));
    };
    let prepared = ToolCall {
        arguments: tool.prepare_arguments(call.arguments.clone()),
        ..call.clone()
    };
    let mut args = match validate_tool_arguments(tool.declaration(), &prepared) {
        Ok(args) => args,
        Err(error) => return Prepared::Immediate(failed(call, error)),
    };
    let gate = scope
        .hooks
        .before_tool_call(
            BeforeToolCall {
                assistant_message: scope.assistant,
                tool_call: call,
                args: &args,
                context: scope.context,
            },
            scope.cancel,
        )
        .await;
    match gate {
        Err(error) => return Prepared::Immediate(failed(call, error.to_string())),
        Ok(Some(_)) | Ok(None) if scope.cancel.is_cancelled() => {
            return Prepared::Immediate(failed(call, ABORTED));
        }
        Ok(Some(decision)) if decision.block => {
            let reason = decision
                .reason
                .unwrap_or_else(|| "Tool execution was blocked".to_string());
            let mut outcome = failed(call, reason);
            outcome.result.terminate = decision.terminate;
            return Prepared::Immediate(outcome);
        }
        Ok(Some(decision)) => {
            if let Some(replacement) = decision.args {
                args = replacement;
            }
        }
        Ok(None) => {}
    }
    Prepared::Ready(ReadyCall {
        call: call.clone(),
        tool,
        args,
    })
}

/// Run a prepared call and apply `after_tool_call`.
async fn run_prepared<M: AgentMessage>(
    scope: CallScope<'_, M>,
    ready: ReadyCall,
    updates: ToolUpdates,
) -> ToolCallOutcome {
    let ReadyCall { call, tool, args } = ready;
    let invocation = ToolInvocation {
        call_id: call.id.clone(),
        args: args.clone(),
        cancel: scope.cancel.clone(),
        updates,
    };
    let (mut result, mut is_error) = match AssertUnwindSafe(tool.execute(invocation))
        .catch_unwind()
        .await
    {
        Ok(Ok(result)) => {
            let is_error = result.is_error;
            (result, is_error)
        }
        Ok(Err(error)) => (AgentToolResult::error(error.to_string()), true),
        Err(_) => (
            AgentToolResult::error(format!("Tool {} panicked", call.name)),
            true,
        ),
    };
    let rewrite = scope
        .hooks
        .after_tool_call(
            AfterToolCall {
                assistant_message: scope.assistant,
                tool_call: &call,
                args: &args,
                result: &result,
                is_error,
                context: scope.context,
            },
            scope.cancel,
        )
        .await;
    match rewrite {
        Ok(None) => {}
        Ok(Some(rewrite)) => {
            if let Some(content) = rewrite.content {
                result.content = content;
            }
            if rewrite.details.is_some() {
                result.details = rewrite.details;
            }
            if rewrite.usage.is_some() {
                result.usage = rewrite.usage;
            }
            if let Some(terminate) = rewrite.terminate {
                result.terminate = terminate;
            }
            is_error = rewrite.is_error.unwrap_or(is_error);
        }
        Err(error) => {
            result = AgentToolResult::error(error.to_string());
            is_error = true;
        }
    }
    result.is_error = is_error;
    ToolCallOutcome {
        tool_call: call,
        result,
        is_error,
    }
}

/// What [`run_tool_call`] runs a call against.
pub struct RunToolCall<'a, M: AgentMessage> {
    pub tools: &'a [Arc<dyn AgentTool>],
    /// Passed to the hooks as the message that issued the call.
    pub assistant_message: &'a AssistantMessage,
    pub context: &'a AgentContext<M>,
    pub hooks: &'a dyn AgentHooks<M>,
    pub cancel: &'a CancellationToken,
    pub updates: ToolUpdates,
}

/// Run one call through the same steps as a model-issued call: argument preparation,
/// validation, `before_tool_call`, execution and `after_tool_call`. It emits no events
/// and appends nothing; tools that call other tools use it so the gates apply to those
/// calls too. Failures come back as error outcomes.
pub async fn run_tool_call<M: AgentMessage>(
    call: &ToolCall,
    run: RunToolCall<'_, M>,
) -> ToolCallOutcome {
    let scope = CallScope {
        context: run.context,
        tools: run.tools,
        assistant: run.assistant_message,
        hooks: run.hooks,
        cancel: run.cancel,
    };
    match prepare_tool_call(scope, call).await {
        Prepared::Immediate(outcome) => outcome,
        Prepared::Ready(ready) => run_prepared(scope, ready, run.updates).await,
    }
}

async fn execute_tool_calls<M: AgentMessage>(
    context: &AgentContext<M>,
    assistant: &AssistantMessage,
    calls: &[ToolCall],
    config: &AgentLoopConfig<M>,
    sink: &dyn AgentEventSink<M>,
    cancel: &CancellationToken,
) -> ToolBatch<M> {
    let sequential = config.tool_execution == ToolExecutionMode::Sequential
        || calls.iter().any(|call| {
            context.tools.iter().any(|tool| {
                tool.name() == call.name
                    && tool.execution_mode() == Some(ToolExecutionMode::Sequential)
            })
        });
    if sequential {
        execute_sequential(context, assistant, calls, config, sink, cancel).await
    } else {
        execute_parallel(context, assistant, calls, config, sink, cancel).await
    }
}

async fn emit_start<M: AgentMessage>(call: &ToolCall, sink: &dyn AgentEventSink<M>) {
    sink.emit(AgentEvent::ToolExecutionStart {
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        args: Value::Object(call.arguments.clone()),
    })
    .await;
}

async fn emit_end<M: AgentMessage>(outcome: &ToolCallOutcome, sink: &dyn AgentEventSink<M>) {
    sink.emit(AgentEvent::ToolExecutionEnd {
        tool_call_id: outcome.tool_call.id.clone(),
        tool_name: outcome.tool_call.name.clone(),
        result: outcome.result.clone(),
        is_error: outcome.is_error,
    })
    .await;
}

async fn emit_update<M: AgentMessage>(update: ToolUpdate, sink: &dyn AgentEventSink<M>) {
    sink.emit(AgentEvent::ToolExecutionUpdate {
        tool_call_id: update.call_id,
        tool_name: update.tool_name,
        args: update.args,
        partial_result: update.partial,
    })
    .await;
}

async fn drain_updates<M: AgentMessage>(
    updates: &mut mpsc::UnboundedReceiver<ToolUpdate>,
    sink: &dyn AgentEventSink<M>,
) {
    while let Ok(update) = updates.try_recv() {
        emit_update(update, sink).await;
    }
}

/// Wait for one tool while reporting its partial results.
async fn drive<M: AgentMessage>(
    future: impl Future<Output = ToolCallOutcome>,
    updates: &mut mpsc::UnboundedReceiver<ToolUpdate>,
    sink: &dyn AgentEventSink<M>,
) -> ToolCallOutcome {
    let mut future = std::pin::pin!(future);
    loop {
        tokio::select! {
            biased;
            outcome = &mut future => {
                drain_updates(updates, sink).await;
                return outcome;
            }
            Some(update) = updates.recv() => emit_update(update, sink).await,
        }
    }
}

fn result_message(outcome: &ToolCallOutcome) -> ToolResultMessage {
    ToolResultMessage {
        tool_call_id: outcome.tool_call.id.clone(),
        tool_name: outcome.tool_call.name.clone(),
        content: outcome.result.content.clone(),
        details: outcome.result.details.clone(),
        usage: outcome.result.usage,
        is_error: outcome.is_error,
        timestamp: now_ms(),
    }
}

fn should_terminate(outcomes: &[ToolCallOutcome]) -> bool {
    !outcomes.is_empty() && outcomes.iter().all(|outcome| outcome.result.terminate)
}

async fn execute_sequential<M: AgentMessage>(
    context: &AgentContext<M>,
    assistant: &AssistantMessage,
    calls: &[ToolCall],
    config: &AgentLoopConfig<M>,
    sink: &dyn AgentEventSink<M>,
    cancel: &CancellationToken,
) -> ToolBatch<M> {
    let (sender, mut updates) = mpsc::unbounded_channel();
    let mut outcomes = Vec::new();
    let mut messages = Vec::new();
    let scope = CallScope {
        context,
        tools: &context.tools,
        assistant,
        hooks: &*config.hooks,
        cancel,
    };
    for call in calls {
        emit_start(call, sink).await;
        let outcome = match prepare_tool_call(scope, call).await {
            Prepared::Immediate(outcome) => outcome,
            Prepared::Ready(ready) => {
                let tool_updates = ToolUpdates::new(
                    sender.clone(),
                    &ready.call.id,
                    &ready.call.name,
                    &ready.args,
                );
                drive(run_prepared(scope, ready, tool_updates), &mut updates, sink).await
            }
        };
        emit_end(&outcome, sink).await;
        let message = result_message(&outcome);
        messages.push(emit_message(wrap::<M>(message), &*config.hooks, sink).await);
        outcomes.push(outcome);
        if cancel.is_cancelled() {
            break;
        }
    }
    ToolBatch {
        messages,
        terminate: should_terminate(&outcomes),
    }
}

async fn execute_parallel<M: AgentMessage>(
    context: &AgentContext<M>,
    assistant: &AssistantMessage,
    calls: &[ToolCall],
    config: &AgentLoopConfig<M>,
    sink: &dyn AgentEventSink<M>,
    cancel: &CancellationToken,
) -> ToolBatch<M> {
    let (sender, mut updates) = mpsc::unbounded_channel();
    let scope = CallScope {
        context,
        tools: &context.tools,
        assistant,
        hooks: &*config.hooks,
        cancel,
    };
    let mut slots: Vec<Option<ToolCallOutcome>> = Vec::new();
    let mut running = FuturesUnordered::new();

    // Gates run in call order before anything executes.
    for call in calls {
        emit_start(call, sink).await;
        let index = slots.len();
        slots.push(None);
        match prepare_tool_call(scope, call).await {
            Prepared::Immediate(outcome) => {
                emit_end(&outcome, sink).await;
                slots[index] = Some(outcome);
            }
            Prepared::Ready(ready) => {
                let tool_updates = ToolUpdates::new(
                    sender.clone(),
                    &ready.call.id,
                    &ready.call.name,
                    &ready.args,
                );
                running.push(async move {
                    let outcome = if scope.cancel.is_cancelled() {
                        failed(&ready.call, ABORTED)
                    } else {
                        run_prepared(scope, ready, tool_updates).await
                    };
                    (index, outcome)
                });
            }
        }
        if cancel.is_cancelled() {
            break;
        }
    }

    // Ends are reported as calls finish; results are appended in call order.
    while !running.is_empty() {
        tokio::select! {
            biased;
            Some((index, outcome)) = running.next() => {
                drain_updates(&mut updates, sink).await;
                emit_end(&outcome, sink).await;
                slots[index] = Some(outcome);
            }
            Some(update) = updates.recv() => emit_update(update, sink).await,
        }
    }

    let outcomes: Vec<ToolCallOutcome> = slots.into_iter().flatten().collect();
    let mut messages = Vec::with_capacity(outcomes.len());
    for outcome in &outcomes {
        let message = result_message(outcome);
        messages.push(emit_message(wrap::<M>(message), &*config.hooks, sink).await);
    }
    ToolBatch {
        messages,
        terminate: should_terminate(&outcomes),
    }
}

/// Report every call of a length-truncated response as failed without running it.
async fn fail_truncated_calls<M: AgentMessage>(
    calls: &[ToolCall],
    hooks: &dyn AgentHooks<M>,
    sink: &dyn AgentEventSink<M>,
) -> ToolBatch<M> {
    let mut messages = Vec::new();
    for call in calls {
        emit_start(call, sink).await;
        let outcome = failed(
            call,
            format!(
                "Tool call \"{}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
                call.name
            ),
        );
        emit_end(&outcome, sink).await;
        let message = result_message(&outcome);
        messages.push(emit_message(wrap::<M>(message), hooks, sink).await);
    }
    ToolBatch {
        messages,
        terminate: false,
    }
}
