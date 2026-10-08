use std::collections::{BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use pi_ai::transcript::{current_system_message, current_system_prompt, initial_system_message};
use pi_ai::{
    AssistantMessage, Message, Model, StreamFn, StreamOptions, ThinkingLevel, UserMessage,
    apply_event,
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::agent_loop::{
    AgentEventSink, AgentLoopConfig, MessageQueues, run_agent_loop, run_agent_loop_continue,
};
use crate::hooks::{AgentHooks, NoHooks};
use crate::types::{
    AgentContext, AgentError, AgentEvent, AgentMessage, AgentTool, QueueMode, ToolExecutionMode,
    as_assistant,
};

/// Receives the agent's events. Listeners are awaited in subscription order, and a run
/// is not idle until they have handled its `AgentEnd`.
#[async_trait]
pub trait AgentListener<M: AgentMessage>: Send + Sync {
    async fn on_event(&self, event: &AgentEvent<M>, cancel: &CancellationToken);
}

struct FnListener<F>(F);

#[async_trait]
impl<M, F> AgentListener<M> for FnListener<F>
where
    M: AgentMessage,
    F: Fn(&AgentEvent<M>) + Send + Sync,
{
    async fn on_event(&self, event: &AgentEvent<M>, _cancel: &CancellationToken) {
        (self.0)(event)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ListenerId(u64);

type Listeners<M> = Vec<(ListenerId, Arc<dyn AgentListener<M>>)>;

/// How to build an [`Agent`].
pub struct AgentOptions<M: AgentMessage> {
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    /// With `tools`, becomes the leading system message unless `messages` has one.
    pub system_prompt: String,
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub messages: Vec<M>,
    pub stream_fn: Arc<dyn StreamFn>,
    pub hooks: Arc<dyn AgentHooks<M>>,
    pub tool_execution: ToolExecutionMode,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub stream_options: StreamOptions,
}

impl<M: AgentMessage> AgentOptions<M> {
    pub fn new(model: Model, stream_fn: Arc<dyn StreamFn>) -> Self {
        Self {
            model,
            thinking_level: ThinkingLevel::Off,
            system_prompt: String::new(),
            tools: Vec::new(),
            messages: Vec::new(),
            stream_fn,
            hooks: Arc::new(NoHooks),
            tool_execution: ToolExecutionMode::Parallel,
            steering_mode: QueueMode::OneAtATime,
            follow_up_mode: QueueMode::OneAtATime,
            stream_options: StreamOptions::default(),
        }
    }
}

struct PendingQueue<M> {
    messages: VecDeque<M>,
    mode: QueueMode,
}

impl<M: Clone> PendingQueue<M> {
    fn new(mode: QueueMode) -> Self {
        Self {
            messages: VecDeque::new(),
            mode,
        }
    }

    fn peek(&self) -> Vec<M> {
        match self.mode {
            QueueMode::All => self.messages.iter().cloned().collect(),
            QueueMode::OneAtATime => self.messages.front().cloned().into_iter().collect(),
        }
    }

    fn drain(&mut self) -> Vec<M> {
        match self.mode {
            QueueMode::All => self.messages.drain(..).collect(),
            QueueMode::OneAtATime => self.messages.pop_front().into_iter().collect(),
        }
    }
}

struct State<M: AgentMessage> {
    model: Model,
    thinking_level: ThinkingLevel,
    messages: Vec<M>,
    tools: Vec<Arc<dyn AgentTool>>,
    streaming_message: Option<AssistantMessage>,
    pending_tool_calls: BTreeSet<String>,
    error_message: Option<String>,
    stream_fn: Arc<dyn StreamFn>,
    hooks: Arc<dyn AgentHooks<M>>,
    tool_execution: ToolExecutionMode,
    stream_options: StreamOptions,
}

struct Inner<M: AgentMessage> {
    state: Mutex<State<M>>,
    steering: Mutex<PendingQueue<M>>,
    follow_up: Mutex<PendingQueue<M>>,
    listeners: Mutex<Listeners<M>>,
    next_listener: AtomicU64,
    /// The active run's cancellation token.
    run: Mutex<Option<CancellationToken>>,
    /// True while a run is active.
    running: watch::Sender<bool>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A stateful agent. Clones share the same agent.
pub struct Agent<M: AgentMessage> {
    inner: Arc<Inner<M>>,
}

impl<M: AgentMessage> Clone for Agent<M> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

enum RunKind<M> {
    Prompt(Vec<M>),
    Continue,
}

impl<M: AgentMessage> Agent<M> {
    pub fn new(options: AgentOptions<M>) -> Self {
        let mut messages = options.messages;
        if !messages.first().is_some_and(crate::types::is_system) {
            let declarations = options
                .tools
                .iter()
                .map(|tool| tool.declaration().clone())
                .collect();
            if let Some(system) = initial_system_message(&options.system_prompt, declarations) {
                messages.insert(0, M::from_message(Message::System(system)));
            }
        }
        let state = State {
            model: options.model,
            thinking_level: options.thinking_level,
            messages,
            tools: options.tools,
            streaming_message: None,
            pending_tool_calls: BTreeSet::new(),
            error_message: None,
            stream_fn: options.stream_fn,
            hooks: options.hooks,
            tool_execution: options.tool_execution,
            stream_options: options.stream_options,
        };
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(state),
                steering: Mutex::new(PendingQueue::new(options.steering_mode)),
                follow_up: Mutex::new(PendingQueue::new(options.follow_up_mode)),
                listeners: Mutex::new(Vec::new()),
                next_listener: AtomicU64::new(1),
                run: Mutex::new(None),
                running: watch::Sender::new(false),
            }),
        }
    }

    pub fn subscribe(&self, listener: Arc<dyn AgentListener<M>>) -> ListenerId {
        let id = ListenerId(self.inner.next_listener.fetch_add(1, Ordering::Relaxed));
        lock(&self.inner.listeners).push((id, listener));
        id
    }

    /// Subscribe a synchronous callback.
    pub fn subscribe_fn(
        &self,
        listener: impl Fn(&AgentEvent<M>) + Send + Sync + 'static,
    ) -> ListenerId {
        self.subscribe(Arc::new(FnListener(listener)))
    }

    pub fn unsubscribe(&self, id: ListenerId) {
        lock(&self.inner.listeners).retain(|(existing, _)| *existing != id);
    }

    // State

    pub fn model(&self) -> Model {
        lock(&self.inner.state).model.clone()
    }

    pub fn set_model(&self, model: Model) {
        lock(&self.inner.state).model = model;
    }

    pub fn thinking_level(&self) -> ThinkingLevel {
        lock(&self.inner.state).thinking_level
    }

    pub fn set_thinking_level(&self, level: ThinkingLevel) {
        lock(&self.inner.state).thinking_level = level;
    }

    pub fn messages(&self) -> Vec<M> {
        lock(&self.inner.state).messages.clone()
    }

    /// Replace the transcript, for example after compaction or switching branches.
    pub fn replace_messages(&self, messages: Vec<M>) {
        lock(&self.inner.state).messages = messages;
    }

    /// Append a message outside a run.
    pub fn append_message(&self, message: M) {
        lock(&self.inner.state).messages.push(message);
    }

    /// Run `read` against the transcript without copying it.
    pub fn with_messages<T>(&self, read: impl FnOnce(&[M]) -> T) -> T {
        read(&lock(&self.inner.state).messages)
    }

    pub fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        lock(&self.inner.state).tools.clone()
    }

    /// Set the executable tools. The next request declares the change to the model.
    pub fn set_tools(&self, tools: Vec<Arc<dyn AgentTool>>) {
        lock(&self.inner.state).tools = tools;
    }

    pub fn set_hooks(&self, hooks: Arc<dyn AgentHooks<M>>) {
        lock(&self.inner.state).hooks = hooks;
    }

    pub fn set_stream_fn(&self, stream_fn: Arc<dyn StreamFn>) {
        lock(&self.inner.state).stream_fn = stream_fn;
    }

    pub fn set_tool_execution(&self, mode: ToolExecutionMode) {
        lock(&self.inner.state).tool_execution = mode;
    }

    pub fn set_stream_options(&self, options: StreamOptions) {
        lock(&self.inner.state).stream_options = options;
    }

    /// The system prompt the transcript currently declares.
    pub fn system_prompt(&self) -> String {
        let state = lock(&self.inner.state);
        current_system_prompt(state.messages.iter().filter_map(AgentMessage::as_message))
    }

    /// The response being streamed, if any.
    pub fn streaming_message(&self) -> Option<AssistantMessage> {
        lock(&self.inner.state).streaming_message.clone()
    }

    pub fn pending_tool_calls(&self) -> BTreeSet<String> {
        lock(&self.inner.state).pending_tool_calls.clone()
    }

    /// The error of the last failed turn of the latest run.
    pub fn error_message(&self) -> Option<String> {
        lock(&self.inner.state).error_message.clone()
    }

    pub fn is_streaming(&self) -> bool {
        *self.inner.running.borrow()
    }

    // Queues

    /// Deliver `message` after the current turn's tool calls, before the next response.
    pub fn steer(&self, message: M) {
        lock(&self.inner.steering).messages.push_back(message);
    }

    /// Deliver `message` only once the agent would otherwise stop.
    pub fn follow_up(&self, message: M) {
        lock(&self.inner.follow_up).messages.push_back(message);
    }

    pub fn set_steering_mode(&self, mode: QueueMode) {
        lock(&self.inner.steering).mode = mode;
    }

    pub fn set_follow_up_mode(&self, mode: QueueMode) {
        lock(&self.inner.follow_up).mode = mode;
    }

    pub fn has_queued_messages(&self) -> bool {
        !lock(&self.inner.steering).messages.is_empty()
            || !lock(&self.inner.follow_up).messages.is_empty()
    }

    /// The messages the next turn would take, without taking them.
    pub fn peek_queued_messages(&self) -> Vec<M> {
        let steering = lock(&self.inner.steering).peek();
        if steering.is_empty() {
            lock(&self.inner.follow_up).peek()
        } else {
            steering
        }
    }

    /// Remove every queued message and return the steering and follow-up queues.
    pub fn clear_queues(&self) -> (Vec<M>, Vec<M>) {
        let steering = lock(&self.inner.steering).messages.drain(..).collect();
        let follow_up = lock(&self.inner.follow_up).messages.drain(..).collect();
        (steering, follow_up)
    }

    // Runs

    /// Start a run with a text prompt.
    pub async fn prompt_text(&self, text: impl Into<String>) -> Result<(), AgentError> {
        self.prompt(vec![M::from_message(Message::User(UserMessage::text(
            text,
        )))])
        .await
    }

    /// Start a run with `messages`.
    pub async fn prompt(&self, messages: Vec<M>) -> Result<(), AgentError> {
        self.run(RunKind::Prompt(messages), false).await
    }

    /// Continue from the transcript. After an assistant message, queued steering or
    /// follow-ups start the run; otherwise the last message must be one the model answers.
    pub async fn continue_run(&self) -> Result<(), AgentError> {
        let (empty, last_is_assistant) = {
            let state = lock(&self.inner.state);
            let empty = state.messages.iter().all(crate::types::is_system);
            (
                empty,
                state
                    .messages
                    .last()
                    .is_some_and(|message| as_assistant(message).is_some()),
            )
        };
        if empty {
            return Err(AgentError::NoMessages);
        }
        if !last_is_assistant {
            return self.run(RunKind::Continue, false).await;
        }
        let steering = lock(&self.inner.steering).drain();
        if !steering.is_empty() {
            return self.run(RunKind::Prompt(steering), true).await;
        }
        let follow_ups = lock(&self.inner.follow_up).drain();
        if !follow_ups.is_empty() {
            return self.run(RunKind::Prompt(follow_ups), false).await;
        }
        Err(AgentError::CannotContinueFromAssistant)
    }

    /// Cancel the active run, if any.
    pub fn abort(&self) {
        if let Some(cancel) = lock(&self.inner.run).as_ref() {
            cancel.cancel();
        }
    }

    /// Wait until no run is active and its listeners have finished.
    pub async fn wait_for_idle(&self) {
        let mut running = self.inner.running.subscribe();
        let _ = running.wait_for(|running| !running).await;
    }

    /// Clear the transcript and queues, keeping the replayed system prompt and tools.
    pub fn reset(&self) -> Result<(), AgentError> {
        if lock(&self.inner.run).is_some() {
            return Err(AgentError::AlreadyRunning);
        }
        let mut state = lock(&self.inner.state);
        let baseline =
            current_system_message(state.messages.iter().filter_map(AgentMessage::as_message));
        state.messages = baseline
            .map(|system| M::from_message(Message::System(system)))
            .into_iter()
            .collect();
        state.error_message = None;
        state.streaming_message = None;
        state.pending_tool_calls.clear();
        drop(state);
        self.clear_queues();
        Ok(())
    }

    async fn run(&self, kind: RunKind<M>, skip_initial_steering: bool) -> Result<(), AgentError> {
        let cancel = {
            let mut run = lock(&self.inner.run);
            if run.is_some() {
                return Err(AgentError::AlreadyRunning);
            }
            let cancel = CancellationToken::new();
            *run = Some(cancel.clone());
            cancel
        };
        // Clears the run even if a hook or listener panics.
        let _guard = RunGuard { inner: &self.inner };
        self.inner.running.send_replace(true);

        let (context, config) = {
            let mut state = lock(&self.inner.state);
            state.streaming_message = None;
            state.error_message = None;
            let context = AgentContext::new(state.messages.clone(), state.tools.clone());
            let config = AgentLoopConfig {
                model: state.model.clone(),
                thinking_level: state.thinking_level,
                stream_fn: state.stream_fn.clone(),
                hooks: state.hooks.clone(),
                queues: Arc::new(AgentQueues {
                    inner: self.inner.clone(),
                    skip_initial_steering: AtomicBool::new(skip_initial_steering),
                }) as Arc<dyn MessageQueues<M>>,
                tool_execution: state.tool_execution,
                stream_options: state.stream_options.clone(),
            };
            (context, config)
        };
        let sink = AgentSink {
            inner: self.inner.clone(),
            cancel: cancel.clone(),
        };
        match kind {
            RunKind::Prompt(messages) => {
                run_agent_loop(messages, context, config, &sink, &cancel).await;
                Ok(())
            }
            RunKind::Continue => run_agent_loop_continue(context, config, &sink, &cancel)
                .await
                .map(drop),
        }
    }
}

struct RunGuard<'a, M: AgentMessage> {
    inner: &'a Inner<M>,
}

impl<M: AgentMessage> Drop for RunGuard<'_, M> {
    fn drop(&mut self) {
        {
            let mut state = lock(&self.inner.state);
            state.streaming_message = None;
            state.pending_tool_calls.clear();
        }
        *lock(&self.inner.run) = None;
        self.inner.running.send_replace(false);
    }
}

struct AgentQueues<M: AgentMessage> {
    inner: Arc<Inner<M>>,
    skip_initial_steering: AtomicBool,
}

#[async_trait]
impl<M: AgentMessage> MessageQueues<M> for AgentQueues<M> {
    async fn steering(&self) -> Vec<M> {
        if self.skip_initial_steering.swap(false, Ordering::Relaxed) {
            return Vec::new();
        }
        lock(&self.inner.steering).drain()
    }

    async fn follow_ups(&self) -> Vec<M> {
        lock(&self.inner.follow_up).drain()
    }
}

struct AgentSink<M: AgentMessage> {
    inner: Arc<Inner<M>>,
    cancel: CancellationToken,
}

#[async_trait]
impl<M: AgentMessage> AgentEventSink<M> for AgentSink<M> {
    async fn emit(&self, event: AgentEvent<M>) {
        {
            let mut state = lock(&self.inner.state);
            match &event {
                AgentEvent::MessageStart { message } => {
                    state.streaming_message = as_assistant(message).cloned();
                }
                AgentEvent::MessageUpdate { event } => {
                    if let Some(partial) = state.streaming_message.as_mut() {
                        apply_event(partial, event);
                    }
                }
                AgentEvent::MessageEnd { message } => {
                    state.streaming_message = None;
                    state.messages.push(message.clone());
                }
                AgentEvent::ToolExecutionStart { tool_call_id, .. } => {
                    state.pending_tool_calls.insert(tool_call_id.clone());
                }
                AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                    state.pending_tool_calls.remove(tool_call_id);
                }
                AgentEvent::TurnEnd { message, .. } => {
                    if let Some(error) =
                        as_assistant(message).and_then(|message| message.error_message.clone())
                    {
                        state.error_message = Some(error);
                    }
                }
                AgentEvent::AgentEnd { .. } => state.streaming_message = None,
                _ => {}
            }
        }
        let listeners: Vec<_> = lock(&self.inner.listeners)
            .iter()
            .map(|(_, listener)| listener.clone())
            .collect();
        for listener in listeners {
            listener.on_event(&event, &self.cancel).await;
        }
    }
}
