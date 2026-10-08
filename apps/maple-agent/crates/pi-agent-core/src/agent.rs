//! Stateful agent wrapper from `packages/agent/src/agent.ts`.
//!
//! Array assignment copies only the outer array. Message, model, state, and
//! subscription handles retain their identity across asynchronous callbacks.
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use futures_util::FutureExt;
use futures_util::future::Shared as SharedFuture;
use indexmap::IndexSet;
use pi_ai::types::{
    AssistantMessage, ImageContent, Model, OnPayload, OnProviderStreamEvent, OnResponse,
    ProviderRequestOptions, SimpleStreamOptions, StopReason, StreamOptions, TextContent,
    ThinkingBudgets, Transport, UserContent, UserMessage, UserMessageContent,
};
use pi_ai::utils::transcript::{
    create_initial_system_message, get_current_system_message, get_current_system_prompt,
    to_tool_declaration,
};

use crate::agent_loop::{
    run_agent_loop, run_agent_loop_continue, start_continue_loop, start_prompt_loop,
};
use crate::stream_fn::get_default_stream_fn;
use crate::types::*;

pub type LegacyPrepareNextTurn = Arc<
    dyn Fn(Option<CancellationToken>) -> AgentFuture<AgentResult<Option<AgentLoopTurnUpdate>>>
        + Send
        + Sync,
>;
pub type ContextPrepareNextTurn = Arc<
    dyn Fn(
            PrepareNextTurnContext,
            Option<CancellationToken>,
        ) -> AgentFuture<AgentResult<Option<AgentLoopTurnUpdate>>>
        + Send
        + Sync,
>;
pub type AgentListener =
    Arc<dyn Fn(AgentEvent, CancellationToken) -> AgentFuture<AgentResult<()>> + Send + Sync>;
pub type Unsubscribe = Box<dyn Fn() + Send + Sync>;

#[derive(Clone, Default)]
pub struct AgentInitialState {
    pub system_prompt: Option<JsString>,
    pub model: Option<Shared<Model>>,
    pub thinking_level: Option<ThinkingLevel>,
    pub tools: Option<AgentTools>,
    pub messages: Option<AgentMessages>,
}

#[derive(Clone, Default)]
pub struct AgentOptions {
    pub initial_state: Option<AgentInitialState>,
    pub convert_to_llm: Option<ConvertToLlm>,
    pub transform_context: Option<TransformContext>,
    pub stream_fn: Option<StreamFn>,
    pub get_api_key: Option<GetApiKey>,
    pub on_payload: Option<OnPayload>,
    pub on_response: Option<OnResponse>,
    pub on_provider_stream_event: Option<OnProviderStreamEvent>,
    pub before_tool_call: Option<BeforeToolCall>,
    pub after_tool_call: Option<AfterToolCall>,
    pub finish_turn: Option<FinishTurn>,
    pub prepare_request: Option<PrepareRequest>,
    pub prepare_next_turn: Option<LegacyPrepareNextTurn>,
    pub prepare_next_turn_with_context: Option<ContextPrepareNextTurn>,
    pub steering_mode: Option<QueueMode>,
    pub follow_up_mode: Option<QueueMode>,
    pub session_id: Option<String>,
    pub thinking_budgets: Option<ThinkingBudgets>,
    pub transport: Option<Transport>,
    pub max_retry_delay_ms: Option<f64>,
    pub tool_execution: Option<ToolExecutionMode>,
}

/// Public mutable settings. `Agent::config` returns the same shared handle.
#[derive(Clone)]
pub struct AgentConfiguration {
    pub convert_to_llm: ConvertToLlm,
    pub transform_context: Option<TransformContext>,
    pub stream_function: StreamFn,
    pub get_api_key: Option<GetApiKey>,
    pub on_payload: Option<OnPayload>,
    pub on_response: Option<OnResponse>,
    pub on_provider_stream_event: Option<OnProviderStreamEvent>,
    pub before_tool_call: Option<BeforeToolCall>,
    pub after_tool_call: Option<AfterToolCall>,
    pub finish_turn: Option<FinishTurn>,
    pub prepare_request: Option<PrepareRequest>,
    pub prepare_next_turn: Option<LegacyPrepareNextTurn>,
    pub prepare_next_turn_with_context: Option<ContextPrepareNextTurn>,
    pub session_id: Option<String>,
    pub thinking_budgets: Option<ThinkingBudgets>,
    pub transport: Transport,
    pub max_retry_delay_ms: Option<f64>,
    pub tool_execution: ToolExecutionMode,
}

#[derive(Clone)]
struct MutableAgentState {
    model: Option<Shared<Model>>,
    thinking_level: ThinkingLevel,
    tools: AgentTools,
    messages: AgentMessages,
    is_streaming: bool,
    streaming_message: Option<AgentMessage>,
    pending_tool_calls: Shared<IndexSet<JsString>>,
    error_message: Option<JsString>,
}

/// A live view of the agent state, including runtime-owned read-only fields.
#[derive(Clone)]
pub struct AgentState(Shared<MutableAgentState>);
impl AgentState {
    fn new(initial: AgentInitialState) -> Self {
        static DEFAULT_MODEL: OnceLock<Shared<Model>> = OnceLock::new();
        let tools = initial
            .tools
            .as_ref()
            .map(Shared::copy_array)
            .unwrap_or_default();
        let messages = initial
            .messages
            .as_ref()
            .map(Shared::copy_array)
            .unwrap_or_default();
        let declarations = tools.read(|tools| {
            tools
                .iter()
                .map(|tool| tool.read(|tool| to_tool_declaration(&tool.tool)))
                .collect::<Vec<_>>()
        });
        let initial_message =
            create_initial_system_message(initial.system_prompt.as_ref(), Some(&declarations));
        if messages
            .get(0)
            .is_none_or(|message| message.role() != "system")
            && let Some(message) = initial_message
        {
            messages.update(|messages| messages.insert(0, message.into()));
        }
        Self(Shared::new(MutableAgentState {
            model: Some(initial.model.unwrap_or_else(|| {
                DEFAULT_MODEL
                    .get_or_init(|| {
                        Shared::new(Model {
                            id: "unknown".into(),
                            name: "unknown".into(),
                            api: "unknown".into(),
                            provider: "unknown".into(),
                            ..Model::default()
                        })
                    })
                    .clone()
            })),
            thinking_level: initial.thinking_level.unwrap_or(ThinkingLevel::Off),
            tools,
            messages,
            is_streaming: false,
            streaming_message: None,
            pending_tool_calls: Shared::default(),
            error_message: None,
        }))
    }
    pub fn ptr_eq(&self, other: &Self) -> bool {
        self.0.ptr_eq(&other.0)
    }
    pub fn system_prompt(&self) -> AgentResult<JsString> {
        get_current_system_prompt(&self.messages().read(|messages| {
            messages
                .iter()
                .filter_map(AgentMessage::as_llm)
                .collect::<Vec<_>>()
        }))
        .map_err(AgentError::type_error)
    }
    pub fn model(&self) -> Option<Shared<Model>> {
        self.0.read(|state| state.model.clone())
    }
    pub fn set_model(&self, model: impl Into<Option<Shared<Model>>>) {
        self.0.update(|state| state.model = model.into());
    }
    pub fn thinking_level(&self) -> ThinkingLevel {
        self.0.read(|state| state.thinking_level)
    }
    pub fn set_thinking_level(&self, level: ThinkingLevel) {
        self.0.update(|state| state.thinking_level = level);
    }
    pub fn tools(&self) -> AgentTools {
        self.0.read(|state| state.tools.clone())
    }
    pub fn set_tools(&self, tools: &AgentTools) {
        let tools = tools.copy_array();
        self.0.update(|state| state.tools = tools);
    }
    pub fn messages(&self) -> AgentMessages {
        self.0.read(|state| state.messages.clone())
    }
    pub fn set_messages(&self, messages: &AgentMessages) {
        let messages = messages.copy_array();
        self.0.update(|state| state.messages = messages);
    }
    pub fn is_streaming(&self) -> bool {
        self.0.read(|state| state.is_streaming)
    }
    pub fn streaming_message(&self) -> Option<AgentMessage> {
        self.0.read(|state| state.streaming_message.clone())
    }
    pub fn pending_tool_calls(&self) -> Shared<IndexSet<JsString>> {
        self.0.read(|state| state.pending_tool_calls.clone())
    }
    pub fn error_message(&self) -> Option<JsString> {
        self.0.read(|state| state.error_message.clone())
    }
}

#[derive(Default)]
struct PendingMessageQueue {
    messages: Vec<AgentMessage>,
    mode: QueueMode,
}
impl PendingMessageQueue {
    fn peek(&self) -> Vec<AgentMessage> {
        match self.mode {
            QueueMode::All => self.messages.clone(),
            QueueMode::OneAtATime => self.messages.first().cloned().into_iter().collect(),
        }
    }
    fn drain(&mut self) -> Vec<AgentMessage> {
        let messages = self.peek();
        self.messages = self.messages[messages.len()..].to_vec();
        messages
    }
}

type RunCompletion = SharedFuture<AgentFuture<AgentResult<()>>>;
struct ActiveRun {
    completion: RunCompletion,
    signal: CancellationToken,
}
struct AgentRuntime {
    listeners: BTreeMap<u64, AgentListener>,
    next_listener_id: u64,
    steering_queue: PendingMessageQueue,
    follow_up_queue: PendingMessageQueue,
    active_run: Option<ActiveRun>,
}

pub enum PromptInput {
    Text(JsString, Option<Vec<ImageContent>>),
    Message(AgentMessage),
    Messages(Vec<AgentMessage>),
}
impl From<&str> for PromptInput {
    fn from(value: &str) -> Self {
        Self::Text(value.into(), None)
    }
}
impl From<String> for PromptInput {
    fn from(value: String) -> Self {
        Self::Text(value.into(), None)
    }
}
impl From<JsString> for PromptInput {
    fn from(value: JsString) -> Self {
        Self::Text(value, None)
    }
}
impl From<AgentMessage> for PromptInput {
    fn from(value: AgentMessage) -> Self {
        Self::Message(value)
    }
}
impl From<Vec<AgentMessage>> for PromptInput {
    fn from(value: Vec<AgentMessage>) -> Self {
        Self::Messages(value)
    }
}

/// Stateful wrapper around the low-level agent loop.
#[derive(Clone)]
pub struct Agent {
    state: AgentState,
    configuration: Shared<AgentConfiguration>,
    runtime: Shared<AgentRuntime>,
    env: Arc<dyn PiEnv>,
}
impl Agent {
    pub fn new(options: AgentOptions, env: Arc<dyn PiEnv>) -> AgentResult<Self> {
        let state = AgentState::new(options.initial_state.unwrap_or_default());
        let stream_function = match options.stream_fn {
            Some(stream) => stream,
            None => get_default_stream_fn()?,
        };
        Ok(Self {
            state,
            configuration: Shared::new(AgentConfiguration {
                convert_to_llm: options.convert_to_llm.unwrap_or_else(|| {
                    Arc::new(|messages| {
                        let messages = messages.read(|messages| {
                            messages.iter().filter_map(AgentMessage::as_llm).collect()
                        });
                        Box::pin(async move { Ok(messages) })
                    })
                }),
                transform_context: options.transform_context,
                stream_function,
                get_api_key: options.get_api_key,
                on_payload: options.on_payload,
                on_response: options.on_response,
                on_provider_stream_event: options.on_provider_stream_event,
                before_tool_call: options.before_tool_call,
                after_tool_call: options.after_tool_call,
                finish_turn: options.finish_turn,
                prepare_request: options.prepare_request,
                prepare_next_turn: options.prepare_next_turn,
                prepare_next_turn_with_context: options.prepare_next_turn_with_context,
                session_id: options.session_id,
                thinking_budgets: options.thinking_budgets,
                transport: options.transport.unwrap_or(Transport::Auto),
                max_retry_delay_ms: options.max_retry_delay_ms,
                tool_execution: options.tool_execution.unwrap_or_default(),
            }),
            runtime: Shared::new(AgentRuntime {
                listeners: BTreeMap::new(),
                next_listener_id: 0,
                steering_queue: PendingMessageQueue {
                    mode: options.steering_mode.unwrap_or_default(),
                    ..PendingMessageQueue::default()
                },
                follow_up_queue: PendingMessageQueue {
                    mode: options.follow_up_mode.unwrap_or_default(),
                    ..PendingMessageQueue::default()
                },
                active_run: None,
            }),
            env,
        })
    }

    /// Listener futures are awaited in subscription order, including `agent_end`.
    /// Removing or adding listeners during delivery follows JavaScript Set iteration.
    pub fn subscribe(&self, listener: AgentListener) -> Unsubscribe {
        self.runtime.update(|runtime| {
            if !runtime
                .listeners
                .values()
                .any(|current| Arc::ptr_eq(current, &listener))
            {
                let id = runtime.next_listener_id;
                runtime.next_listener_id += 1;
                runtime.listeners.insert(id, listener.clone());
            }
        });
        let runtime = self.runtime.clone();
        Box::new(move || {
            runtime.update(|runtime| {
                runtime
                    .listeners
                    .retain(|_, current| !Arc::ptr_eq(current, &listener));
            })
        })
    }
    pub fn state(&self) -> AgentState {
        self.state.clone()
    }
    pub fn config(&self) -> Shared<AgentConfiguration> {
        self.configuration.clone()
    }
    pub fn steering_mode(&self) -> QueueMode {
        self.runtime.read(|runtime| runtime.steering_queue.mode)
    }
    pub fn set_steering_mode(&self, mode: QueueMode) {
        self.runtime
            .update(|runtime| runtime.steering_queue.mode = mode);
    }
    pub fn follow_up_mode(&self) -> QueueMode {
        self.runtime.read(|runtime| runtime.follow_up_queue.mode)
    }
    pub fn set_follow_up_mode(&self, mode: QueueMode) {
        self.runtime
            .update(|runtime| runtime.follow_up_queue.mode = mode);
    }
    pub fn steer(&self, message: AgentMessage) {
        self.runtime
            .update(|runtime| runtime.steering_queue.messages.push(message));
    }
    pub fn follow_up(&self, message: AgentMessage) {
        self.runtime
            .update(|runtime| runtime.follow_up_queue.messages.push(message));
    }
    pub fn clear_steering_queue(&self) {
        self.runtime
            .update(|runtime| runtime.steering_queue.messages = Vec::new());
    }
    pub fn clear_follow_up_queue(&self) {
        self.runtime
            .update(|runtime| runtime.follow_up_queue.messages = Vec::new());
    }
    pub fn clear_all_queues(&self) {
        self.clear_steering_queue();
        self.clear_follow_up_queue();
    }
    pub fn has_queued_messages(&self) -> bool {
        self.runtime.read(|runtime| {
            !runtime.steering_queue.messages.is_empty()
                || !runtime.follow_up_queue.messages.is_empty()
        })
    }
    pub fn peek_queued_messages(&self) -> Vec<AgentMessage> {
        self.runtime.read(|runtime| {
            let steering = runtime.steering_queue.peek();
            if steering.is_empty() {
                runtime.follow_up_queue.peek()
            } else {
                steering
            }
        })
    }
    pub fn signal(&self) -> Option<CancellationToken> {
        self.runtime
            .read(|runtime| runtime.active_run.as_ref().map(|run| run.signal.clone()))
    }
    pub fn abort(&self) {
        if let Some(signal) = self.signal() {
            signal.cancel();
        }
    }
    pub fn wait_for_idle(&self) -> AgentFuture<()> {
        let completion = self.runtime.read(|runtime| {
            runtime
                .active_run
                .as_ref()
                .map(|run| run.completion.clone())
        });
        Box::pin(async move {
            if let Some(completion) = completion {
                let _ = completion.await;
            }
        })
    }
    pub fn reset(&self) -> AgentResult<()> {
        if self.signal().is_some() {
            return Err(
                "Agent is already processing. Wait for completion before resetting.".into(),
            );
        }
        let baseline = get_current_system_message(&self.state.messages().read(|messages| {
            messages
                .iter()
                .filter_map(AgentMessage::as_llm)
                .collect::<Vec<_>>()
        }))
        .map_err(AgentError::type_error)?;
        self.state.0.update(|state| {
            state.messages = baseline
                .into_iter()
                .map(AgentMessage::from)
                .collect::<Vec<_>>()
                .into();
            state.is_streaming = false;
            state.streaming_message = None;
            state.pending_tool_calls = Shared::default();
            state.error_message = None;
        });
        self.clear_follow_up_queue();
        self.clear_steering_queue();
        Ok(())
    }

    /// Starts eagerly on the current Tokio runtime; dropping the returned future
    /// does not cancel the run. `abort` controls cancellation explicitly.
    pub fn prompt(&self, input: impl Into<PromptInput>) -> AgentFuture<AgentResult<()>> {
        if self.signal().is_some() {
            return failed(
                "Agent is already processing a prompt. Use steer() or followUp() to queue messages, or wait for completion.",
            );
        }
        let messages = match input.into() {
            PromptInput::Messages(messages) => messages,
            PromptInput::Message(message) => vec![message],
            PromptInput::Text(text, images) => {
                let mut content = vec![UserContent::Text(TextContent::new(text))];
                content.extend(
                    images
                        .unwrap_or_default()
                        .into_iter()
                        .map(UserContent::Image),
                );
                vec![
                    UserMessage {
                        content: UserMessageContent::Blocks(content),
                        timestamp: self.env.now_ms() as f64,
                        ..UserMessage::default()
                    }
                    .into(),
                ]
            }
        };
        self.run_with_lifecycle(Some(messages), false)
    }
    pub fn prompt_text(
        &self,
        text: impl Into<JsString>,
        images: Option<Vec<ImageContent>>,
    ) -> AgentFuture<AgentResult<()>> {
        self.prompt(PromptInput::Text(text.into(), images))
    }
    pub fn continue_run(&self) -> AgentFuture<AgentResult<()>> {
        if self.signal().is_some() {
            return failed("Agent is already processing. Wait for completion before continuing.");
        }
        let messages = self.state.messages();
        let Some(last) = messages.last() else {
            return failed("No messages to continue from");
        };
        if messages.read(|messages| messages.iter().all(|message| message.role() == "system")) {
            return failed("No messages to continue from");
        }
        if last.role() == "assistant" {
            let steering = self
                .runtime
                .update(|runtime| runtime.steering_queue.drain());
            if !steering.is_empty() {
                return self.run_with_lifecycle(Some(steering), true);
            }
            let follow_ups = self
                .runtime
                .update(|runtime| runtime.follow_up_queue.drain());
            if !follow_ups.is_empty() {
                return self.run_with_lifecycle(Some(follow_ups), false);
            }
            return failed("Cannot continue from message role: assistant");
        }
        self.run_with_lifecycle(None, false)
    }

    fn create_context_snapshot(&self) -> AgentContext {
        AgentContext {
            messages: self.state.messages().copy_array(),
            tools: Some(self.state.tools().copy_array()),
        }
    }
    fn create_loop_config(
        &self,
        skip_initial_steering_poll: bool,
        model: Shared<Model>,
    ) -> AgentLoopConfig {
        let configuration = self.configuration.snapshot();
        let prepare_next_turn = if configuration.prepare_next_turn_with_context.is_some()
            || configuration.prepare_next_turn.is_some()
        {
            let agent = self.clone();
            Some(Arc::new(move |context| {
                let agent = agent.clone();
                Box::pin(async move {
                    let configuration = agent.configuration.snapshot();
                    if let Some(prepare) = configuration.prepare_next_turn_with_context {
                        prepare(context, agent.signal()).await
                    } else if let Some(prepare) = configuration.prepare_next_turn {
                        prepare(agent.signal()).await
                    } else {
                        Ok(None)
                    }
                }) as AgentFuture<AgentResult<Option<AgentLoopTurnUpdate>>>
            }) as PrepareNextTurn)
        } else {
            None
        };
        let agent = self.clone();
        let skip_initial = Shared::new(skip_initial_steering_poll);
        let get_steering_messages: GetMessages = Arc::new(move || {
            let messages = if skip_initial.update(std::mem::take) {
                Vec::new()
            } else {
                agent
                    .runtime
                    .update(|runtime| runtime.steering_queue.drain())
            };
            Box::pin(async move { Ok(messages) })
        });
        let agent = self.clone();
        let get_follow_up_messages: GetMessages = Arc::new(move || {
            let messages = agent
                .runtime
                .update(|runtime| runtime.follow_up_queue.drain());
            Box::pin(async move { Ok(messages) })
        });
        let options = SimpleStreamOptions {
            reasoning: match self.state.thinking_level() {
                ThinkingLevel::Off => None,
                ThinkingLevel::Minimal => Some(pi_ai::types::ThinkingLevel::Minimal),
                ThinkingLevel::Low => Some(pi_ai::types::ThinkingLevel::Low),
                ThinkingLevel::Medium => Some(pi_ai::types::ThinkingLevel::Medium),
                ThinkingLevel::High => Some(pi_ai::types::ThinkingLevel::High),
                ThinkingLevel::Xhigh => Some(pi_ai::types::ThinkingLevel::Xhigh),
                ThinkingLevel::Max => Some(pi_ai::types::ThinkingLevel::Max),
            },
            thinking_budgets: configuration.thinking_budgets,
            stream: StreamOptions {
                session_id: configuration.session_id,
                on_provider_stream_event: configuration.on_provider_stream_event,
                transport: Some(configuration.transport),
                request: ProviderRequestOptions {
                    on_payload: configuration.on_payload,
                    on_response: configuration.on_response,
                    max_retry_delay_ms: configuration.max_retry_delay_ms,
                    ..ProviderRequestOptions::default()
                },
                ..StreamOptions::default()
            },
            ..SimpleStreamOptions::default()
        };
        AgentLoopConfig {
            options,
            model,
            convert_to_llm: configuration.convert_to_llm,
            transform_context: configuration.transform_context,
            get_api_key: configuration.get_api_key,
            finish_turn: configuration.finish_turn,
            prepare_request: configuration.prepare_request,
            prepare_next_turn,
            get_steering_messages: Some(get_steering_messages),
            get_follow_up_messages: Some(get_follow_up_messages),
            tool_execution: Some(configuration.tool_execution),
            before_tool_call: configuration.before_tool_call,
            after_tool_call: configuration.after_tool_call,
        }
    }
    fn run_with_lifecycle(
        &self,
        messages: Option<Vec<AgentMessage>>,
        skip_initial_steering_poll: bool,
    ) -> AgentFuture<AgentResult<()>> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let completion: RunCompletion = (Box::pin(async move {
            receiver.await.unwrap_or_else(|_| {
                Err(AgentError::new(
                    "Agent run task terminated before completion",
                ))
            })
        }) as AgentFuture<AgentResult<()>>)
            .shared();
        let signal = CancellationToken::new();
        let started = self.runtime.update(|runtime| {
            if runtime.active_run.is_some() {
                return false;
            }
            runtime.active_run = Some(ActiveRun {
                completion: completion.clone(),
                signal: signal.clone(),
            });
            true
        });
        if !started {
            return failed("Agent is already processing.");
        }
        self.state.0.update(|state| {
            state.is_streaming = true;
            state.streaming_message = None;
            state.error_message = None;
        });
        // The source captures these before its first asynchronous boundary.
        let context = self.create_context_snapshot();
        let config = self
            .state
            .model()
            .map(|model| self.create_loop_config(skip_initial_steering_poll, model));
        let stream = self
            .configuration
            .read(|config| config.stream_function.clone());
        let agent = self.clone();
        let emit_agent = self.clone();
        let emit: AgentEventSink = Arc::new(move |event| {
            let agent = emit_agent.clone();
            Box::pin(async move { agent.process_events(event).await })
        });
        // Always release active state, even if the runtime drops the driver.
        let finish = FinishRun(agent.clone());
        let mut driver = Box::pin(async move {
            let result = if let Some(config) = config {
                match messages {
                    Some(messages) => {
                        run_agent_loop(
                            messages,
                            context,
                            config,
                            emit,
                            Some(signal.clone()),
                            Some(stream),
                            agent.env.clone(),
                        )
                        .await
                    }
                    None => {
                        run_agent_loop_continue(
                            context,
                            config,
                            emit,
                            Some(signal.clone()),
                            Some(stream),
                            agent.env.clone(),
                        )
                        .await
                    }
                }
            } else {
                // SDK state may be undefined after construction. The typed provider
                // boundary requires a model, but startup/history and cleanup remain
                // observable before the source's failure handler reads model.api.
                let started = match messages {
                    Some(messages) => {
                        start_prompt_loop(messages, context, &emit, agent.env.as_ref())
                            .await
                            .map(|_| ())
                    }
                    None => start_continue_loop(&context, &emit).await.map(|_| ()),
                };
                started.and_then(|()| {
                    Err(AgentError::type_error(
                        "Cannot read properties of undefined (reading 'provider')",
                    ))
                })
            };
            // The lifecycle awaits its executor, including immediately rejected
            // runs, before handling failure or clearing the active state.
            tokio::task::yield_now().await;
            let result = match result {
                Ok(_) => Ok(()),
                Err(error) => agent.handle_run_failure(error, signal.is_cancelled()).await,
            };
            drop(finish);
            let _ = sender.send(result);
        });
        // Calling a JavaScript async function executes its prefix immediately.
        // Poll once before handing ownership to Tokio; awaited event delivery
        // supplies the first suspension even when every callback is ready.
        let waker = futures_util::task::noop_waker();
        if driver
            .as_mut()
            .poll(&mut std::task::Context::from_waker(&waker))
            .is_pending()
        {
            tokio::spawn(driver);
        }
        Box::pin(completion)
    }
    async fn handle_run_failure(&self, error: AgentError, aborted: bool) -> AgentResult<()> {
        let model = self
            .state
            .model()
            .ok_or_else(|| {
                AgentError::type_error("Cannot read properties of undefined (reading 'api')")
            })?
            .snapshot();
        let failure: AgentMessage = AssistantMessage {
            content: vec![TextContent::new("").into()],
            api: model.api,
            provider: model.provider,
            model: model.id,
            stop_reason: if aborted {
                StopReason::Aborted
            } else {
                StopReason::Error
            },
            error_message: Some(error.message),
            timestamp: self.env.now_ms() as f64,
            ..AssistantMessage::default()
        }
        .into();
        self.process_events(AgentEvent::MessageStart {
            message: failure.clone(),
        })
        .await?;
        self.process_events(AgentEvent::MessageEnd {
            message: failure.clone(),
        })
        .await?;
        self.process_events(AgentEvent::TurnEnd {
            message: failure.clone(),
            tool_results: Vec::new(),
        })
        .await?;
        self.process_events(AgentEvent::AgentEnd {
            messages: vec![failure].into(),
        })
        .await
    }
    fn finish_run(&self) {
        self.state.0.update(|state| {
            state.is_streaming = false;
            state.streaming_message = None;
            state.pending_tool_calls = Shared::default();
        });
        self.runtime.update(|runtime| runtime.active_run = None);
    }
    async fn process_events(&self, event: AgentEvent) -> AgentResult<()> {
        match &event {
            AgentEvent::MessageStart { message } | AgentEvent::MessageUpdate { message, .. } => {
                self.state
                    .0
                    .update(|state| state.streaming_message = Some(message.clone()));
            }
            AgentEvent::MessageEnd { message } => {
                self.state.0.update(|state| state.streaming_message = None);
                self.state.messages().push(message.clone());
            }
            AgentEvent::ToolExecutionStart { tool_call_id, .. } => {
                let mut pending = self.state.pending_tool_calls().snapshot();
                pending.insert(tool_call_id.clone());
                self.state
                    .0
                    .update(|state| state.pending_tool_calls = Shared::new(pending));
            }
            AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                let mut pending = self.state.pending_tool_calls().snapshot();
                pending.shift_remove(tool_call_id);
                self.state
                    .0
                    .update(|state| state.pending_tool_calls = Shared::new(pending));
            }
            AgentEvent::TurnEnd { message, .. } => {
                if let Some(assistant) = message.assistant()
                    && let Some(error) = assistant.read(|message| message.error_message.clone())
                    && !error.is_empty()
                {
                    self.state
                        .0
                        .update(|state| state.error_message = Some(error));
                }
            }
            AgentEvent::AgentEnd { .. } => {
                self.state.0.update(|state| state.streaming_message = None);
            }
            _ => {}
        }
        let result = self.notify_listeners(event).await;
        // The loop awaits processEvents even when there are no listeners.
        tokio::task::yield_now().await;
        result
    }
    async fn notify_listeners(&self, event: AgentEvent) -> AgentResult<()> {
        let signal = self
            .signal()
            .ok_or_else(|| AgentError::new("Agent listener invoked outside active run"))?;
        // A live cursor, rather than a copied subscriber list, preserves Set's
        // add/delete/reinsert semantics across each awaited listener.
        let mut next_id = 0;
        loop {
            let next = self.runtime.read(|runtime| {
                runtime
                    .listeners
                    .range(next_id..)
                    .next()
                    .map(|(id, listener)| (*id, listener.clone()))
            });
            let Some((id, listener)) = next else {
                break;
            };
            next_id = id + 1;
            let result = listener(event.clone(), signal.clone()).await;
            // JS await schedules a continuation for both ready values and
            // already-rejected promises; neither can run through this boundary.
            tokio::task::yield_now().await;
            result?;
        }
        Ok(())
    }
}

struct FinishRun(Agent);
impl Drop for FinishRun {
    fn drop(&mut self) {
        self.0.finish_run();
    }
}
fn failed(message: &'static str) -> AgentFuture<AgentResult<()>> {
    Box::pin(async move { Err(message.into()) })
}

#[cfg(test)]
mod fidelity_tests {
    use super::*;
    use pi_ai::types::{AssistantMessageEvent, DoneReason};
    use pi_ai::utils::event_stream::create_assistant_message_event_stream;
    use pi_testkit::VirtualEnv;

    fn response() -> StreamFn {
        Arc::new(|_, _, _| {
            let stream = create_assistant_message_event_stream();
            stream.push(AssistantMessageEvent::Done {
                reason: DoneReason::Stop,
                message: AssistantMessage {
                    stop_reason: StopReason::Stop,
                    ..AssistantMessage::default()
                },
            });
            Box::pin(async move { Ok(stream) })
        })
    }
    fn agent(options: AgentOptions) -> Agent {
        Agent::new(options, Arc::new(VirtualEnv::new(1_700_000_000_000))).unwrap()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn prompt_runs_the_first_listener_before_returning_and_then_suspends() {
        let agent = agent(AgentOptions {
            stream_fn: Some(response()),
            ..AgentOptions::default()
        });
        let observed = Shared::new(Vec::new());
        let first_observed = observed.clone();
        let first = agent.subscribe(Arc::new(move |event, _| {
            first_observed.push((1, event.kind()));
            Box::pin(async { Ok(()) })
        }));
        let second_observed = observed.clone();
        let second = agent.subscribe(Arc::new(move |event, _| {
            second_observed.push((2, event.kind()));
            Box::pin(async { Ok(()) })
        }));
        let prompt = agent.prompt("hello");
        assert_eq!(observed.snapshot(), vec![(1, "agent_start")]);
        assert!(agent.state().is_streaming());
        prompt.await.unwrap();
        assert_eq!(
            &observed.snapshot()[..2],
            &[(1, "agent_start"), (2, "agent_start")]
        );
        first();
        second();
    }

    #[test]
    fn raw_system_replay_errors_propagate_before_reset_mutates_state_or_queues() {
        let raw: AgentMessage = JsObject::from_iter([
            ("role", JsValue::String("system".into())),
            ("content", JsValue::String("raw prompt".into())),
            ("timestamp", JsValue::Number(0.0)),
            ("toolsAdded", JsValue::Number(42.0)),
        ])
        .into();
        let agent = agent(AgentOptions {
            stream_fn: Some(response()),
            initial_state: Some(AgentInitialState {
                messages: Some(vec![raw].into()),
                ..AgentInitialState::default()
            }),
            ..AgentOptions::default()
        });
        agent.steer(UserMessage::default().into());
        let original_messages = agent.state().messages();
        assert_eq!(agent.state().system_prompt().unwrap_err().name, "TypeError");
        assert_eq!(agent.reset().unwrap_err().name, "TypeError");
        assert!(agent.state().messages().ptr_eq(&original_messages));
        assert!(agent.has_queued_messages());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn raw_known_role_reaches_provider_unchanged_and_retains_shared_identity() {
        let raw = RawMessage::new(JsObject::from_iter([
            ("role", JsValue::String("assistant".into())),
            ("content", JsValue::Null),
            ("unknown", JsValue::String("retain".into())),
        ]));
        let original = AgentMessage::new(AgentMessageValue::Custom(raw.clone()));
        let unknown: AgentMessage = JsObject::from_iter([
            ("role", JsValue::String("notification".into())),
            ("content", JsValue::String("omit from provider".into())),
        ])
        .into();
        let calls = Shared::new(0);
        let provider_calls = calls.clone();
        let expected_raw = raw.clone();
        let stream: StreamFn = Arc::new(move |model, context, options| {
            provider_calls.update(|calls| *calls += 1);
            assert_eq!(context.messages.len(), 2);
            let pi_ai::types::Message::Raw(message) = &context.messages[0] else {
                panic!("raw known-role message must reach provider");
            };
            assert!(message.ptr_eq(&expected_raw));
            assert_eq!(
                message.read(|value| value.get("content").cloned()),
                Some(JsValue::Null)
            );
            assert_eq!(
                message.read(|value| value.get("unknown").cloned()),
                Some(JsValue::String("retain".into()))
            );
            message.update(|value| {
                value.insert("providerObserved", JsValue::Bool(true));
            });
            response()(model, context, options)
        });
        let agent = agent(AgentOptions {
            stream_fn: Some(stream),
            initial_state: Some(AgentInitialState {
                messages: Some(vec![original.clone(), unknown].into()),
                ..AgentInitialState::default()
            }),
            ..AgentOptions::default()
        });
        agent.prompt("next").await.unwrap();
        assert_eq!(calls.snapshot(), 1);
        assert!(agent.state().messages().get(0).unwrap().ptr_eq(&original));
        assert_eq!(
            raw.read(|value| value.get("providerObserved").cloned()),
            Some(JsValue::Bool(true))
        );
        assert!(agent.state().error_message().is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropping_prompt_completion_does_not_cancel_the_owned_run() {
        let agent = agent(AgentOptions {
            stream_fn: Some(response()),
            ..AgentOptions::default()
        });
        let ended = CancellationToken::new();
        let observed = ended.clone();
        let unsubscribe = agent.subscribe(Arc::new(move |event, _| {
            if matches!(event, AgentEvent::AgentEnd { .. }) {
                observed.cancel();
            }
            Box::pin(async { Ok(()) })
        }));
        drop(agent.prompt("hello"));
        assert!(agent.state().is_streaming());
        ended.cancelled().await;
        agent.wait_for_idle().await;
        assert!(!agent.state().is_streaming());
        assert!(agent.signal().is_none());
        assert_eq!(agent.state().messages().len(), 2);
        unsubscribe();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn subscriber_set_is_live_across_await_and_deduplicates_identity() {
        let agent = agent(AgentOptions {
            stream_fn: Some(response()),
            ..AgentOptions::default()
        });
        let calls = Shared::new(Vec::new());
        let entered = CancellationToken::new();
        let release = CancellationToken::new();
        let first_calls = calls.clone();
        let first_entered = entered.clone();
        let first_release = release.clone();
        let first: AgentListener = Arc::new(move |event, _| {
            let calls = first_calls.clone();
            let entered = first_entered.clone();
            let release = first_release.clone();
            Box::pin(async move {
                if matches!(event, AgentEvent::AgentStart) {
                    calls.push(1);
                    entered.cancel();
                    release.cancelled().await;
                }
                Ok(())
            })
        });
        let unsubscribe_first = agent.subscribe(first.clone());
        let unsubscribe_duplicate = agent.subscribe(first);
        let second_calls = calls.clone();
        let unsubscribe_second = agent.subscribe(Arc::new(move |event, _| {
            if matches!(event, AgentEvent::AgentStart) {
                second_calls.push(2);
            }
            Box::pin(async { Ok(()) })
        }));
        let prompt = agent.prompt("hello");
        entered.cancelled().await;
        unsubscribe_second();
        let third_calls = calls.clone();
        let unsubscribe_third = agent.subscribe(Arc::new(move |event, _| {
            if matches!(event, AgentEvent::AgentStart) {
                third_calls.push(3);
            }
            Box::pin(async { Ok(()) })
        }));
        release.cancel();
        prompt.await.unwrap();
        assert_eq!(calls.snapshot(), vec![1, 3]);
        unsubscribe_first();
        unsubscribe_duplicate();
        unsubscribe_third();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn captured_model_and_message_objects_remain_live_after_array_copy() {
        let model = Shared::new(Model {
            id: "before".into(),
            ..Model::default()
        });
        let initial: AgentMessage = UserMessage {
            content: UserMessageContent::Text("before".into()),
            ..UserMessage::default()
        }
        .into();
        let observed = Shared::new(None);
        let stream_observed = observed.clone();
        let stream: StreamFn = Arc::new(move |model, context, options| {
            stream_observed
                .update(|observed| *observed = Some((model.id, context.messages.clone())));
            response()(Model::default(), context, options)
        });
        let agent = agent(AgentOptions {
            stream_fn: Some(stream),
            initial_state: Some(AgentInitialState {
                model: Some(model.clone()),
                messages: Some(vec![initial.clone()].into()),
                ..AgentInitialState::default()
            }),
            ..AgentOptions::default()
        });
        let mutable_model = model.clone();
        let mutable_message = initial.clone();
        let unsubscribe = agent.subscribe(Arc::new(move |event, _| {
            if matches!(event, AgentEvent::AgentStart) {
                mutable_model.update(|model| model.id = "after".into());
                mutable_message.update(|message| {
                    let AgentMessageValue::User(message) = message else {
                        panic!("user fixture")
                    };
                    message.content = UserMessageContent::Text("after".into());
                });
            }
            Box::pin(async { Ok(()) })
        }));
        agent.prompt("next").await.unwrap();
        let (id, messages) = observed.snapshot().unwrap();
        assert_eq!(id, "after");
        let pi_ai::types::Message::User(message) = &messages[0] else {
            panic!("user fixture")
        };
        assert_eq!(message.content, UserMessageContent::Text("after".into()));
        assert!(agent.state().model().unwrap().ptr_eq(&model));
        assert!(agent.state().messages().get(0).unwrap().ptr_eq(&initial));
        unsubscribe();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn failure_listener_rejection_still_finishes_run_and_resolves_idle() {
        let agent = agent(AgentOptions {
            stream_fn: Some(response()),
            ..AgentOptions::default()
        });
        let unsubscribe = agent.subscribe(Arc::new(|event, _| {
            Box::pin(async move {
                match event {
                    AgentEvent::AgentStart => Err(AgentError::new("initial failure")),
                    AgentEvent::MessageStart { .. } => Err(AgentError::new("failure listener")),
                    _ => Ok(()),
                }
            })
        }));
        let prompt = agent.prompt("hello");
        let idle = agent.wait_for_idle();
        assert_eq!(prompt.await.unwrap_err().message, "failure listener");
        idle.await;
        assert!(!agent.state().is_streaming());
        assert!(agent.signal().is_none());
        unsubscribe();
    }
}

#[cfg(test)]
mod tool_identity_tests {
    use super::*;
    use pi_ai::types::{
        AssistantContent, AssistantMessageEvent, DoneReason, Schema, Tool, ToolCall,
    };
    use pi_ai::utils::event_stream::create_assistant_message_event_stream;
    use pi_testkit::VirtualEnv;

    #[tokio::test(flavor = "current_thread")]
    async fn tool_object_and_execution_result_mutations_reach_the_captured_context_and_history() {
        let tool = Shared::new(AgentTool {
            tool: Tool {
                name: "inspect".into(),
                description: "before".into(),
                parameters: Schema::typebox(serde_json::json!({"type":"object","properties":{}})),
                ..Tool::default()
            },
            label: "inspect".into(),
            prepare_arguments: None,
            output_schema: None,
            execute: Arc::new(|_, _, _, _| {
                Box::pin(async {
                    Ok(AgentToolResult {
                        content: Some(vec![UserContent::Text(TextContent::new("before"))]),
                        ..AgentToolResult::default()
                    })
                })
            }),
            replay: None,
            execution_mode: None,
        });
        let requests = Shared::new(0);
        let observed = Shared::new(None);
        let stream_requests = requests.clone();
        let stream_observed = observed.clone();
        let stream: StreamFn = Arc::new(move |_, context, _| {
            let request = stream_requests.update(|count| {
                *count += 1;
                *count
            });
            if request == 2 {
                stream_observed.update(|value| *value = context.messages.last().cloned());
            }
            let stream = create_assistant_message_event_stream();
            let (content, reason) = if request == 1 {
                (
                    vec![AssistantContent::ToolCall(ToolCall {
                        id: "call".into(),
                        name: "inspect".into(),
                        arguments: JsValue::Object(JsObject::new()).into(),
                        ..ToolCall::default()
                    })],
                    DoneReason::ToolUse,
                )
            } else {
                (Vec::new(), DoneReason::Stop)
            };
            stream.push(AssistantMessageEvent::Done {
                reason,
                message: AssistantMessage {
                    content,
                    stop_reason: reason.into(),
                    ..AssistantMessage::default()
                },
            });
            Box::pin(async move { Ok(stream) })
        });
        let agent = Agent::new(
            AgentOptions {
                stream_fn: Some(stream),
                initial_state: Some(AgentInitialState {
                    tools: Some(vec![tool.clone()].into()),
                    ..AgentInitialState::default()
                }),
                ..AgentOptions::default()
            },
            Arc::new(VirtualEnv::new(1_700_000_000_000)),
        )
        .unwrap();
        let captured_tool = tool.clone();
        let unsubscribe = agent.subscribe(Arc::new(move |event, _| {
            if matches!(event, AgentEvent::AgentStart) {
                captured_tool.update(|tool| {
                    tool.execute = Arc::new(|_, _, _, _| {
                        Box::pin(async {
                            Ok(AgentToolResult {
                                content: Some(vec![UserContent::Text(TextContent::new(
                                    "changed execute",
                                ))]),
                                ..AgentToolResult::default()
                            })
                        })
                    })
                });
            }
            if let AgentEvent::ToolExecutionEnd { result, .. } = event {
                result.update(|result| {
                    assert_eq!(
                        result.content,
                        Some(vec![UserContent::Text(TextContent::new("changed execute"))])
                    );
                    result.content = Some(vec![UserContent::Text(TextContent::new(
                        "changed listener",
                    ))]);
                });
            }
            Box::pin(async { Ok(()) })
        }));
        agent.prompt("run").await.unwrap();
        assert!(agent.state().tools().get(0).unwrap().ptr_eq(&tool));
        let Some(pi_ai::types::Message::ToolResult(result)) = observed.snapshot() else {
            panic!("provider must see tool result")
        };
        assert_eq!(
            result.content,
            vec![UserContent::Text(TextContent::new("changed listener"))]
        );
        let history = agent.state().messages().snapshot();
        let result = history
            .iter()
            .find_map(|message| match message.as_llm() {
                Some(pi_ai::types::Message::ToolResult(result)) => Some(result),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            result.content,
            vec![UserContent::Text(TextContent::new("changed listener"))]
        );
        unsubscribe();
    }
}

#[cfg(test)]
mod missing_model_tests {
    use super::*;
    use pi_testkit::VirtualEnv;
    #[tokio::test]
    async fn clearing_the_model_preserves_absence_startup_history_and_failure_cleanup() {
        let env = Arc::new(VirtualEnv::new(1_700_000_000_000));
        let called = Shared::new(0usize);
        let calls = called.clone();
        let agent = Agent::new(
            AgentOptions {
                stream_fn: Some(Arc::new(move |_, _, _| {
                    calls.update(|count| *count += 1);
                    Box::pin(async { Err(AgentError::new("unexpected typed provider call")) })
                })),
                ..Default::default()
            },
            env,
        )
        .unwrap();
        assert_eq!(
            agent
                .state()
                .model()
                .unwrap()
                .read(|model| model.id.clone()),
            "unknown"
        );
        agent.state().set_model(None);
        assert!(agent.state().model().is_none());
        let events = Shared::new(Vec::new());
        let recorded = events.clone();
        let _unsubscribe = agent.subscribe(Arc::new(move |event, _| {
            recorded.update(|events| events.push(event.kind()));
            Box::pin(async { Ok(()) })
        }));
        let message: AgentMessage = UserMessage {
            content: UserMessageContent::Text("hello".into()),
            timestamp: 1_700_000_000_000.0,
            ..UserMessage::default()
        }
        .into();
        let error = agent.prompt(message).await.unwrap_err();
        assert_eq!(error.name, "TypeError");
        assert_eq!(
            error.message,
            "Cannot read properties of undefined (reading 'api')"
        );
        assert_eq!(
            events.snapshot(),
            vec!["agent_start", "turn_start", "message_start", "message_end"]
        );
        assert_eq!(agent.state().messages().len(), 1);
        assert_eq!(agent.state().messages().get(0).unwrap().role(), "user");
        assert!(agent.state().model().is_none());
        assert!(!agent.state().is_streaming());
        assert!(agent.state().error_message().is_none());
        // Undefined is outside the typed StreamFn input contract; no unknown model is fabricated.
        assert_eq!(called.snapshot(), 0);
        agent.wait_for_idle().await;
    }
}
