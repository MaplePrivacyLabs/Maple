//! The agent session: an [`Agent`] over a session tree, with extensions, resources,
//! settings, automatic retry and compaction.
//!
//! Every message the agent records is appended to the session as it ends, so the tree
//! is always the record. Prompts pass through extension commands, input handlers and
//! skill and template expansion; the system prompt is kept in the transcript as named
//! sections and only changed sections are sent again. After each run, transient
//! failures are retried with backoff, a context overflow is compacted and retried, and a
//! context near its limit is compacted.

use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::FutureExt;
use pi_agent_core::{
    AfterToolCall, AfterToolCallResult, Agent, AgentContext, AgentError, AgentEvent, AgentHooks,
    AgentListener, AgentMessage, AgentOptions, AgentTool, BeforeToolCall, BeforeToolCallResult,
    ToolError, TurnContext, TurnUpdate,
};
use pi_ai::transcript::current_system_message;
use pi_ai::{
    Content, ImageContent, Message, Model, StreamOptions, SystemMessage, ThinkingLevel,
    UserMessage, content_text, is_context_overflow, is_retryable_error, now_ms, retry_delay_ms,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::compaction::{
    CompactionResult, Summarizer, compact, estimate_context_tokens, prepare_compaction,
    should_compact, summarize_branch,
};
use crate::extensions::{
    AgentEventSeen, BeforeAgentStart, BeforeProviderRequest, Context, CustomMessageDraft,
    Extension, ExtensionContext, ExtensionErrorReport, ExtensionRunner, ExtensionUi, Input,
    InputAction, InputSource, MessageEnd, ModelSelect, RegisteredTool, SessionBeforeCompact,
    SessionBeforeTree, SessionCompact, SessionShutdown, SessionStart, SessionStartReason,
    SessionTree, ThinkingLevelSelect, ToolCall, ToolResult,
};
use crate::messages::{CustomMessage, SessionMessage, convert_to_llm};
use crate::models::ModelRegistry;
use crate::resources::{Resources, expand_prompt_template, expand_skill_command};
use crate::session::{EntryKind, SessionError, SessionManager};
use crate::settings::Settings;
use crate::store::SessionStore;
use crate::system_prompt::{
    SystemPromptOptions, ToolPromptInfo, build_prompt_state, diff_sections, render,
};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Why a session operation did not happen.
#[derive(Debug)]
pub enum AgentSessionError {
    /// A run or compaction is in progress.
    Busy,
    NoModel,
    NothingToCompact,
    /// An extension cancelled the operation.
    Cancelled,
    Agent(AgentError),
    Session(SessionError),
    Failed(String),
}

impl fmt::Display for AgentSessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => {
                formatter.write_str("The session is busy; wait for the current run or compaction")
            }
            Self::NoModel => formatter.write_str("No model selected"),
            Self::NothingToCompact => formatter.write_str("Nothing to compact"),
            Self::Cancelled => formatter.write_str("Cancelled by an extension"),
            Self::Agent(error) => error.fmt(formatter),
            Self::Session(error) => error.fmt(formatter),
            Self::Failed(error) => formatter.write_str(error),
        }
    }
}

impl std::error::Error for AgentSessionError {}

impl From<SessionError> for AgentSessionError {
    fn from(error: SessionError) -> Self {
        Self::Session(error)
    }
}

impl From<AgentError> for AgentSessionError {
    fn from(error: AgentError) -> Self {
        Self::Agent(error)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompactionReason {
    Manual,
    Threshold,
    Overflow,
}

/// What a session reports to its host.
#[derive(Clone, Debug)]
pub enum AgentSessionEvent {
    Agent(AgentEvent<SessionMessage>),
    QueueUpdate {
        steering: Vec<String>,
        follow_up: Vec<String>,
    },
    CompactionStart {
        reason: CompactionReason,
    },
    CompactionEnd {
        reason: CompactionReason,
        result: Option<CompactionResult>,
        error: Option<String>,
    },
    RetryStart {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error: String,
    },
    RetryEnd {
        success: bool,
        attempt: u32,
        error: Option<String>,
    },
    ExtensionError(ExtensionErrorReport),
    /// Writing the session failed; the conversation goes on in memory.
    PersistenceError(String),
    /// The run, its retries and compactions are over.
    Settled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamingBehavior {
    Steer,
    FollowUp,
}

#[derive(Clone, Debug)]
pub struct PromptOptions {
    pub images: Vec<ImageContent>,
    /// How to deliver the prompt while a run is active; without it, a busy session refuses.
    pub streaming_behavior: Option<StreamingBehavior>,
    /// Run extension commands and expand skills and templates.
    pub expand: bool,
    pub source: InputSource,
}

impl Default for PromptOptions {
    fn default() -> Self {
        Self {
            images: Vec::new(),
            streaming_behavior: None,
            expand: true,
            source: InputSource::Interactive,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptOutcome {
    /// The prompt ran to completion.
    Completed,
    /// The prompt was queued on the active run.
    Queued,
    /// A command or extension handled it; nothing was sent.
    Handled,
}

/// How an extension message is delivered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// Add it without starting a turn; during a run, once the current turn ends.
    Append,
    /// Start a turn with it; during a run, deliver it after the current turn.
    Steer,
    /// Start a turn with it; during a run, deliver it once the agent would stop.
    FollowUp,
    /// Send it with the next prompt.
    NextTurn,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ContextUsage {
    pub tokens: u64,
    pub context_window: u64,
    pub percent: f64,
}

/// What to build a session from.
pub struct AgentSessionOptions {
    pub cwd: PathBuf,
    /// The product name the default system prompt uses.
    pub app_name: String,
    pub session: SessionManager,
    pub models: ModelRegistry,
    /// The model for a session that has not chosen one.
    pub model: Option<Model>,
    pub settings: Settings,
    pub resources: Resources,
    /// The host's tools.
    pub tools: Vec<RegisteredTool>,
    pub extensions: Vec<Arc<dyn Extension>>,
    pub ui: Arc<dyn ExtensionUi>,
    /// Replaces the default preamble, tool list and rules.
    pub custom_prompt: Option<String>,
    /// How the model reads skill files; without it, skills are left out of the prompt.
    pub skill_load_hint: Option<String>,
}

impl AgentSessionOptions {
    pub fn new(
        cwd: impl Into<PathBuf>,
        app_name: &str,
        session: SessionManager,
        models: ModelRegistry,
    ) -> Self {
        Self {
            cwd: cwd.into(),
            app_name: app_name.to_string(),
            session,
            models,
            model: None,
            settings: Settings::default(),
            resources: Resources::default(),
            tools: Vec::new(),
            extensions: Vec::new(),
            ui: Arc::new(crate::extensions::NoUi),
            custom_prompt: None,
            skill_load_hint: None,
        }
    }
}

type Listener = Arc<dyn Fn(&AgentSessionEvent) + Send + Sync>;

#[derive(Default)]
struct RunState {
    abort_requested: bool,
    retry_attempt: u32,
    retry_cancel: Option<CancellationToken>,
    compaction_cancel: Option<CancellationToken>,
    overflow_recovery_attempted: bool,
    /// A complete system prompt that replaces the transcript's for the current run.
    forced_prompt: Option<String>,
    next_turn_messages: Vec<SessionMessage>,
    /// Extension messages waiting for the current turn to end.
    pending_custom: Vec<SessionMessage>,
    steering_texts: Vec<String>,
    follow_up_texts: Vec<String>,
}

pub(crate) struct SessionCore {
    agent: Agent<SessionMessage>,
    session: Mutex<SessionManager>,
    runner: ExtensionRunner,
    models: ModelRegistry,
    ui: Arc<dyn ExtensionUi>,
    cwd: PathBuf,
    settings: Mutex<Settings>,
    resources: Mutex<Resources>,
    prompt_base: Mutex<SystemPromptOptions>,
    model: Mutex<Option<Model>>,
    tools: Mutex<Vec<RegisteredTool>>,
    active_tools: Mutex<Vec<String>>,
    listeners: Mutex<Vec<(u64, Listener)>>,
    next_listener: AtomicU64,
    run: Mutex<RunState>,
    weak: Weak<SessionCore>,
}

/// A conversation with an agent, recorded in a session.
#[derive(Clone)]
pub struct AgentSession {
    core: Arc<SessionCore>,
}

fn placeholder_model() -> Model {
    Model {
        id: "none".into(),
        name: "No model".into(),
        api: "none".into(),
        provider: "none".into(),
        base_url: String::new(),
        reasoning: false,
        input: vec![pi_ai::InputModality::Text],
        cost: Default::default(),
        context_window: 0,
        max_tokens: 0,
        thinking_levels: Default::default(),
        compat: Default::default(),
    }
}

impl AgentSession {
    pub async fn new(options: AgentSessionOptions) -> Result<Self, AgentSessionError> {
        let projection = options.session.projection();
        let has_history = !options.session.entries().is_empty();
        let models = options.models.clone();
        let model = projection
            .model
            .as_ref()
            .and_then(|(provider, id)| models.find(provider, id))
            .or(options.model.clone());
        let thinking_level = if has_history {
            projection.thinking_level
        } else {
            options.settings.default_thinking_level.unwrap_or_default()
        };
        let thinking_level = model.as_ref().map_or(ThinkingLevel::Off, |model| {
            model.clamp_thinking_level(thinking_level)
        });
        let prompt_base = SystemPromptOptions {
            app_name: options.app_name.clone(),
            custom_prompt: options.custom_prompt.clone(),
            append: options
                .settings
                .append_system_prompt
                .clone()
                .unwrap_or_default(),
            cwd: options.cwd.to_string_lossy().into_owned(),
            context_files: options.resources.context_files.clone(),
            skills: options.resources.skills.clone(),
            skill_load_hint: options.skill_load_hint.clone(),
            ..SystemPromptOptions::default()
        };
        let session_id = options.session.id().to_string();

        let core = Arc::new_cyclic(|weak: &Weak<SessionCore>| {
            let runner = ExtensionRunner::load(&options.extensions, weak.clone());
            let mut agent_options = AgentOptions::new(
                model.clone().unwrap_or_else(placeholder_model),
                models.stream_fn(),
            );
            agent_options.thinking_level = thinking_level;
            agent_options.messages = projection.messages.clone();
            agent_options.tool_execution = options.settings.tool_execution;
            agent_options.steering_mode = options.settings.steering_mode;
            agent_options.follow_up_mode = options.settings.follow_up_mode;
            agent_options.hooks = Arc::new(SessionHooks { core: weak.clone() });
            agent_options.stream_options = StreamOptions {
                session_id: Some(session_id),
                ..StreamOptions::default()
            };
            let mut tools = options.tools;
            tools.extend(runner.tools().iter().cloned());
            let active = tools
                .iter()
                .filter(|tool| tool.active)
                .map(|tool| tool.tool.name().to_string())
                .collect();
            SessionCore {
                agent: Agent::new(agent_options),
                session: Mutex::new(options.session),
                runner,
                models: models.clone(),
                ui: options.ui,
                cwd: options.cwd,
                settings: Mutex::new(options.settings),
                resources: Mutex::new(options.resources),
                prompt_base: Mutex::new(prompt_base),
                model: Mutex::new(model),
                tools: Mutex::new(tools),
                active_tools: Mutex::new(active),
                listeners: Mutex::new(Vec::new()),
                next_listener: AtomicU64::new(1),
                run: Mutex::new(RunState::default()),
                weak: weak.clone(),
            }
        });

        for provider in core.runner.providers() {
            models.register_models(provider.models.clone());
            if let Some((api, implementation)) = &provider.api {
                models.register_api(api, implementation.clone());
            }
        }
        if lock(&core.model).is_none() && projection.model.is_some() {
            // A provider an extension registers can serve the session's model.
            let model = projection
                .model
                .as_ref()
                .and_then(|(provider, id)| models.find(provider, id));
            if let Some(model) = model {
                core.agent.set_model(model.clone());
                *lock(&core.model) = Some(model);
            }
        }
        let sink = core.weak.clone();
        core.runner.set_error_sink(Arc::new(move |report| {
            if let Some(core) = sink.upgrade() {
                core.emit(AgentSessionEvent::ExtensionError(report));
            }
        }));
        if core.runner.has::<BeforeProviderRequest>() {
            let weak = core.weak.clone();
            let mut stream_options = StreamOptions {
                session_id: Some(core.session_id()),
                ..StreamOptions::default()
            };
            stream_options.on_payload = Some(Arc::new(move |payload: Value| {
                let weak = weak.clone();
                async move {
                    match weak.upgrade() {
                        Some(core) => core.runner.provider_payload(payload, &weak).await,
                        None => payload,
                    }
                }
                .boxed()
            }));
            core.agent.set_stream_options(stream_options);
        }
        core.agent.subscribe(Arc::new(SessionListener {
            core: core.weak.clone(),
        }));
        core.sync_tools();
        core.runner
            .emit(
                &SessionStart {
                    reason: SessionStartReason::Startup,
                },
                &core.weak,
            )
            .await;
        Ok(Self { core })
    }

    pub fn subscribe(&self, listener: impl Fn(&AgentSessionEvent) + Send + Sync + 'static) -> u64 {
        let id = self.core.next_listener.fetch_add(1, Ordering::Relaxed);
        lock(&self.core.listeners).push((id, Arc::new(listener)));
        id
    }

    pub fn unsubscribe(&self, id: u64) {
        lock(&self.core.listeners).retain(|(existing, _)| *existing != id);
    }

    /// Send a prompt. Commands, input handlers, skills and templates apply first; while
    /// a run is active the prompt is queued as `options.streaming_behavior` says.
    pub async fn prompt(
        &self,
        text: &str,
        options: PromptOptions,
    ) -> Result<PromptOutcome, AgentSessionError> {
        self.core.clone().prompt(text, options).await
    }

    /// Deliver `text` after the current turn's tool calls.
    pub fn steer(&self, text: &str) {
        self.core
            .queue(StreamingBehavior::Steer, user_message(text, Vec::new()));
    }

    /// Deliver `text` once the agent would otherwise stop.
    pub fn follow_up(&self, text: &str) {
        self.core
            .queue(StreamingBehavior::FollowUp, user_message(text, Vec::new()));
    }

    /// Remove queued messages; returns the steering and follow-up texts.
    pub fn clear_queue(&self) -> (Vec<String>, Vec<String>) {
        self.core.agent.clear_queues();
        let mut run = lock(&self.core.run);
        let texts = (
            std::mem::take(&mut run.steering_texts),
            std::mem::take(&mut run.follow_up_texts),
        );
        drop(run);
        self.core.emit_queue();
        texts
    }

    /// Stop the run, a pending retry and a running compaction.
    pub fn abort(&self) {
        self.core.abort();
    }

    pub async fn wait_for_idle(&self) {
        self.core.agent.wait_for_idle().await;
    }

    /// Compact the current branch.
    pub async fn compact(
        &self,
        custom_instructions: Option<&str>,
    ) -> Result<CompactionResult, AgentSessionError> {
        if self.core.agent.is_streaming() {
            return Err(AgentSessionError::Busy);
        }
        self.core
            .clone()
            .compact(CompactionReason::Manual, custom_instructions)
            .await
    }

    /// Move to another point of the tree, optionally summarizing the branch being left.
    pub async fn navigate_tree(
        &self,
        target_id: &str,
        summarize: bool,
        custom_instructions: Option<&str>,
    ) -> Result<(), AgentSessionError> {
        self.core
            .clone()
            .navigate_tree(target_id, summarize, custom_instructions)
            .await
    }

    /// Continue in a new session holding only the path to `entry_id`.
    pub async fn fork(
        &self,
        entry_id: &str,
        store: Box<dyn SessionStore>,
    ) -> Result<(), AgentSessionError> {
        if self.core.agent.is_streaming() {
            return Err(AgentSessionError::Busy);
        }
        lock(&self.core.session).fork(entry_id, store)?;
        self.core.rebuild_messages();
        self.core
            .runner
            .emit(
                &SessionStart {
                    reason: SessionStartReason::Fork,
                },
                &self.core.weak,
            )
            .await;
        Ok(())
    }

    pub async fn set_model(&self, model: Model) {
        self.core.set_model(model).await;
    }

    pub async fn set_thinking_level(&self, level: ThinkingLevel) {
        self.core.set_thinking_level(level).await;
    }

    pub fn model(&self) -> Option<Model> {
        lock(&self.core.model).clone()
    }

    pub fn thinking_level(&self) -> ThinkingLevel {
        self.core.agent.thinking_level()
    }

    pub fn is_streaming(&self) -> bool {
        self.core.agent.is_streaming()
    }

    /// The model context: the projection of the current branch.
    pub fn messages(&self) -> Vec<SessionMessage> {
        self.core.agent.messages()
    }

    pub fn session_id(&self) -> String {
        self.core.session_id()
    }

    /// Read the session tree.
    pub fn with_session<T>(&self, read: impl FnOnce(&SessionManager) -> T) -> T {
        read(&lock(&self.core.session))
    }

    /// The system prompt the transcript declares.
    pub fn system_prompt(&self) -> String {
        self.core.agent.system_prompt()
    }

    pub fn active_tools(&self) -> Vec<String> {
        lock(&self.core.active_tools).clone()
    }

    /// Choose the tools declared to the model; unknown names are ignored. The change is
    /// declared with the next request.
    pub fn set_active_tools(&self, names: &[String]) {
        self.core.set_active_tools(names);
    }

    /// Every registered tool's name.
    pub fn tool_names(&self) -> Vec<String> {
        lock(&self.core.tools)
            .iter()
            .map(|tool| tool.tool.name().to_string())
            .collect()
    }

    /// Extension commands as `(name, description)`.
    pub fn commands(&self) -> Vec<(String, String)> {
        self.core
            .runner
            .commands()
            .iter()
            .map(|command| (command.name.clone(), command.description.clone()))
            .collect()
    }

    pub fn resources(&self) -> Resources {
        lock(&self.core.resources).clone()
    }

    pub fn settings(&self) -> Settings {
        lock(&self.core.settings).clone()
    }

    pub fn context_usage(&self) -> Option<ContextUsage> {
        self.core.context_usage()
    }

    /// The context extension handlers get, for hosts that act on the session like one.
    pub fn extension_context(&self) -> ExtensionContext {
        ExtensionContext {
            core: self.core.weak.clone(),
            extension: Arc::from("host"),
        }
    }

    /// Tell extensions the session is ending.
    pub async fn shutdown(&self) {
        self.core.abort();
        self.core.agent.wait_for_idle().await;
        self.core
            .runner
            .emit(&SessionShutdown, &self.core.weak)
            .await;
    }
}

fn user_message(text: &str, images: Vec<ImageContent>) -> SessionMessage {
    let mut content = vec![Content::text(text)];
    content.extend(images.into_iter().map(Content::Image));
    SessionMessage::Llm(Message::User(UserMessage {
        content,
        timestamp: now_ms(),
    }))
}

fn custom_message(draft: CustomMessageDraft) -> SessionMessage {
    SessionMessage::Custom(CustomMessage {
        custom_type: draft.custom_type,
        content: draft.content,
        display: draft.display,
        details: draft.details,
        timestamp: now_ms(),
    })
}

impl SessionCore {
    fn emit(&self, event: AgentSessionEvent) {
        let listeners: Vec<Listener> = lock(&self.listeners)
            .iter()
            .map(|(_, listener)| listener.clone())
            .collect();
        for listener in listeners {
            listener(&event);
        }
    }

    fn emit_queue(&self) {
        let run = lock(&self.run);
        let event = AgentSessionEvent::QueueUpdate {
            steering: run.steering_texts.clone(),
            follow_up: run.follow_up_texts.clone(),
        };
        drop(run);
        self.emit(event);
    }

    fn session_id(&self) -> String {
        lock(&self.session).id().to_string()
    }

    fn model(&self) -> Option<Model> {
        lock(&self.model).clone()
    }

    fn report_persistence(&self) {
        let error = lock(&self.session).take_persist_error();
        if let Some(error) = error {
            self.emit(AgentSessionEvent::PersistenceError(error.to_string()));
        }
    }

    /// Replace the agent's transcript with the current branch's projection.
    fn rebuild_messages(&self) {
        let messages = lock(&self.session).projection().messages;
        self.agent.replace_messages(messages);
    }

    fn sync_tools(&self) {
        let active = lock(&self.active_tools).clone();
        let tools = lock(&self.tools);
        let executable: Vec<Arc<dyn AgentTool>> = active
            .iter()
            .filter_map(|name| tools.iter().find(|tool| tool.tool.name() == name))
            .map(|tool| tool.tool.clone())
            .collect();
        drop(tools);
        self.agent.set_tools(executable);
    }

    fn set_active_tools(&self, names: &[String]) {
        let known: Vec<String> = {
            let tools = lock(&self.tools);
            names
                .iter()
                .filter(|name| tools.iter().any(|tool| tool.tool.name() == name.as_str()))
                .cloned()
                .collect()
        };
        *lock(&self.active_tools) = known;
        self.sync_tools();
    }

    /// The prompt options with the active tools filled in.
    fn prompt_options(&self) -> SystemPromptOptions {
        let mut options = lock(&self.prompt_base).clone();
        let active = lock(&self.active_tools).clone();
        let tools = lock(&self.tools);
        options.tools = active
            .iter()
            .filter_map(|name| tools.iter().find(|tool| tool.tool.name() == name))
            .map(|tool| ToolPromptInfo {
                name: tool.tool.name().to_string(),
                snippet: tool.prompt.snippet.clone(),
                guidelines: tool.prompt.guidelines.clone(),
            })
            .collect();
        options
    }

    /// The system message that brings the transcript's prompt up to date, if needed.
    fn system_update(
        &self,
        options: &SystemPromptOptions,
    ) -> Result<Option<SessionMessage>, AgentSessionError> {
        let unforced = SystemPromptOptions {
            force_prompt: None,
            ..options.clone()
        };
        let (content, sections) =
            build_prompt_state(&unforced).map_err(AgentSessionError::Failed)?;
        let current = self.agent.with_messages(|messages| {
            current_system_message(messages.iter().filter_map(AgentMessage::as_message))
        });
        let message = match current {
            None => Some(SystemMessage {
                content,
                sections: sections
                    .into_iter()
                    .map(|(name, text)| (name, Some(text)))
                    .collect(),
                timestamp: now_ms(),
                ..SystemMessage::default()
            }),
            Some(current) => {
                diff_sections(&current.sections, &sections).map(|patch| SystemMessage {
                    sections: patch,
                    timestamp: now_ms(),
                    ..SystemMessage::default()
                })
            }
        };
        Ok(message.map(|system| SessionMessage::Llm(Message::System(system))))
    }

    fn queue(&self, behavior: StreamingBehavior, message: SessionMessage) {
        let text = message.text();
        {
            let mut run = lock(&self.run);
            match behavior {
                StreamingBehavior::Steer => run.steering_texts.push(text),
                StreamingBehavior::FollowUp => run.follow_up_texts.push(text),
            }
        }
        match behavior {
            StreamingBehavior::Steer => self.agent.steer(message),
            StreamingBehavior::FollowUp => self.agent.follow_up(message),
        }
        self.emit_queue();
    }

    fn abort(&self) {
        let mut run = lock(&self.run);
        run.abort_requested = true;
        for token in [run.retry_cancel.take(), run.compaction_cancel.clone()]
            .into_iter()
            .flatten()
        {
            token.cancel();
        }
        drop(run);
        self.agent.abort();
    }

    fn abort_requested(&self) -> bool {
        lock(&self.run).abort_requested
    }

    fn context_usage(&self) -> Option<ContextUsage> {
        let model = self.model()?;
        let tokens = self.agent.with_messages(estimate_context_tokens);
        let window = model.context_window;
        Some(ContextUsage {
            tokens,
            context_window: window,
            percent: if window > 0 {
                tokens as f64 * 100.0 / window as f64
            } else {
                0.0
            },
        })
    }

    async fn prompt(
        self: Arc<Self>,
        text: &str,
        options: PromptOptions,
    ) -> Result<PromptOutcome, AgentSessionError> {
        if options.expand && text.starts_with('/') {
            let (name, args) = text[1..]
                .split_once(char::is_whitespace)
                .unwrap_or((&text[1..], ""));
            if let Some(result) = self
                .runner
                .run_command(name, args.trim().to_string(), &self.weak)
                .await
            {
                result.map_err(|error| AgentSessionError::Failed(error.to_string()))?;
                return Ok(PromptOutcome::Handled);
            }
        }
        if lock(&self.run).compaction_cancel.is_some() {
            return Err(AgentSessionError::Busy);
        }
        let input = Input {
            text: text.to_string(),
            images: options.images.clone(),
            source: options.source,
        };
        let (mut text, images) = match self.runner.input(input, &self.weak).await {
            InputAction::Handled => return Ok(PromptOutcome::Handled),
            InputAction::Transform { text, images } => (text, images),
            InputAction::Continue => (text.to_string(), options.images.clone()),
        };
        if options.expand {
            let resources = lock(&self.resources).clone();
            text = expand_prompt_template(
                &expand_skill_command(&text, &resources.skills),
                &resources.prompt_templates,
            );
        }
        if self.agent.is_streaming() {
            let behavior = options.streaming_behavior.ok_or(AgentSessionError::Busy)?;
            self.queue(behavior, user_message(&text, images));
            return Ok(PromptOutcome::Queued);
        }
        if self.model().is_none() {
            return Err(AgentSessionError::NoModel);
        }

        let base = self.prompt_options();
        let event = BeforeAgentStart {
            prompt: text.clone(),
            system_prompt: render(&base).unwrap_or_default(),
            options: base.clone(),
        };
        let (prompt_options, drafts, forced) =
            self.runner.before_agent_start(event, &self.weak).await;
        if prompt_options != base {
            let mut stored = prompt_options.clone();
            stored.tools = Vec::new();
            *lock(&self.prompt_base) = stored;
        }
        let mut messages: Vec<SessionMessage> =
            self.system_update(&prompt_options)?.into_iter().collect();
        messages.push(user_message(&text, images));
        {
            let mut run = lock(&self.run);
            run.forced_prompt = forced.or(prompt_options.force_prompt.clone());
            messages.append(&mut run.next_turn_messages);
        }
        messages.extend(drafts.into_iter().map(custom_message));
        self.run_prompt(messages).await?;
        Ok(PromptOutcome::Completed)
    }

    async fn run_prompt(
        self: &Arc<Self>,
        messages: Vec<SessionMessage>,
    ) -> Result<(), AgentSessionError> {
        {
            let mut run = lock(&self.run);
            run.abort_requested = false;
            run.overflow_recovery_attempted = false;
        }
        self.sync_tools();
        let result = match self.agent.prompt(messages).await {
            Ok(()) => self.after_run().await,
            Err(error) => Err(error.into()),
        };
        self.flush_pending_custom();
        lock(&self.run).forced_prompt = None;
        self.emit(AgentSessionEvent::Settled);
        result
    }

    /// Retry, compact or continue with queued messages until the run is really over.
    async fn after_run(self: &Arc<Self>) -> Result<(), AgentSessionError> {
        loop {
            if self.abort_requested() || !self.handle_post_run().await || self.abort_requested() {
                return Ok(());
            }
            self.agent.continue_run().await?;
        }
    }

    /// Decide what follows a finished run. True when the agent should continue.
    async fn handle_post_run(self: &Arc<Self>) -> bool {
        let last = self.agent.with_messages(|messages| {
            messages.iter().rev().find_map(|message| match message {
                SessionMessage::Llm(Message::Assistant(assistant)) => Some(assistant.clone()),
                _ => None,
            })
        });
        let Some(last) = last else {
            return self.agent.has_queued_messages();
        };
        if last.stop_reason == pi_ai::StopReason::Aborted {
            return false;
        }
        let Some(model) = self.model() else {
            return false;
        };
        let settings = lock(&self.settings).clone();

        let overflow = is_context_overflow(&last, Some(model.context_window));
        let recovered = std::mem::replace(&mut lock(&self.run).overflow_recovery_attempted, true);
        if overflow && settings.compaction.enabled && !recovered {
            if last.is_failure() {
                self.omit_last_assistant();
            }
            return match self.clone().compact(CompactionReason::Overflow, None).await {
                Ok(_) => last.is_failure() || self.agent.has_queued_messages(),
                Err(_) => false,
            };
        }
        if !overflow {
            lock(&self.run).overflow_recovery_attempted = recovered;
        }

        if is_retryable_error(&last) && settings.retry.enabled {
            let attempt = {
                let mut run = lock(&self.run);
                run.retry_attempt += 1;
                run.retry_attempt
            };
            if attempt <= settings.retry.max_retries {
                return self
                    .retry(attempt, &settings, &last.error_message.unwrap_or_default())
                    .await;
            }
            lock(&self.run).retry_attempt -= 1;
        }
        let attempt = std::mem::take(&mut lock(&self.run).retry_attempt);
        if last.is_failure() && attempt > 0 {
            self.emit(AgentSessionEvent::RetryEnd {
                success: false,
                attempt,
                error: last.error_message.clone(),
            });
        }

        if !last.is_failure() {
            let tokens = self.agent.with_messages(estimate_context_tokens);
            if should_compact(tokens, model.context_window, &settings.compaction) {
                let _ = self
                    .clone()
                    .compact(CompactionReason::Threshold, None)
                    .await;
            }
        }
        self.agent.has_queued_messages()
    }

    /// Wait out the backoff, keeping the failed attempt in the tree but out of the context.
    async fn retry(&self, attempt: u32, settings: &Settings, error: &str) -> bool {
        let delay_ms = retry_delay_ms(
            settings.retry.base_delay_ms,
            settings.retry.max_delay_ms,
            attempt,
        );
        self.emit(AgentSessionEvent::RetryStart {
            attempt,
            max_attempts: settings.retry.max_retries,
            delay_ms,
            error: error.to_string(),
        });
        self.omit_last_assistant();
        let token = CancellationToken::new();
        lock(&self.run).retry_cancel = Some(token.clone());
        let completed = tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => true,
            _ = token.cancelled() => false,
        };
        lock(&self.run).retry_cancel = None;
        if !completed {
            lock(&self.run).retry_attempt = 0;
            self.emit(AgentSessionEvent::RetryEnd {
                success: false,
                attempt,
                error: Some("Retry cancelled".into()),
            });
        }
        completed && !self.abort_requested()
    }

    /// Omit the latest assistant message from the context with a context edit.
    fn omit_last_assistant(&self) {
        {
            let mut session = lock(&self.session);
            let target = session
                .branch()
                .iter()
                .rev()
                .find_map(|entry| match &entry.kind {
                    EntryKind::Message {
                        message: SessionMessage::Llm(Message::Assistant(_)),
                    } => Some(entry.id.clone()),
                    _ => None,
                });
            if let Some(target) = target {
                let _ = session.append_context_edit(&target, None);
            }
        }
        self.report_persistence();
        self.rebuild_messages();
    }

    fn flush_pending_custom(&self) {
        let pending = std::mem::take(&mut lock(&self.run).pending_custom);
        for message in pending {
            self.append_now(message);
        }
    }

    /// Add a message outside a run and tell listeners.
    fn append_now(&self, message: SessionMessage) {
        self.persist(&message);
        self.agent.append_message(message.clone());
        self.emit(AgentSessionEvent::Agent(AgentEvent::MessageStart {
            message: message.clone(),
        }));
        self.emit(AgentSessionEvent::Agent(AgentEvent::MessageEnd { message }));
    }

    fn persist(&self, message: &SessionMessage) {
        {
            let mut session = lock(&self.session);
            match message {
                SessionMessage::Llm(_) => {
                    session.append_message(message.clone());
                }
                SessionMessage::Custom(custom) => {
                    session.append_custom_message(
                        &custom.custom_type,
                        custom.content.clone(),
                        custom.display,
                        custom.details.clone(),
                    );
                }
                // Summaries are recorded as their own entries.
                SessionMessage::CompactionSummary(_) | SessionMessage::BranchSummary(_) => {}
            }
        }
        self.report_persistence();
    }

    async fn compact(
        self: Arc<Self>,
        reason: CompactionReason,
        custom_instructions: Option<&str>,
    ) -> Result<CompactionResult, AgentSessionError> {
        let token = {
            let mut run = lock(&self.run);
            if run.compaction_cancel.is_some() {
                return Err(AgentSessionError::Busy);
            }
            let token = CancellationToken::new();
            run.compaction_cancel = Some(token.clone());
            token
        };
        self.emit(AgentSessionEvent::CompactionStart { reason });
        let result = self.compact_inner(custom_instructions, token).await;
        lock(&self.run).compaction_cancel = None;
        self.emit(AgentSessionEvent::CompactionEnd {
            reason,
            result: result.as_ref().ok().cloned(),
            error: result.as_ref().err().map(ToString::to_string),
        });
        result
    }

    async fn compact_inner(
        &self,
        custom_instructions: Option<&str>,
        cancel: CancellationToken,
    ) -> Result<CompactionResult, AgentSessionError> {
        let model = self.model().ok_or(AgentSessionError::NoModel)?;
        let settings = lock(&self.settings).clone();
        let preparation = {
            let session = lock(&self.session);
            prepare_compaction(&session.branch(), &settings.compaction)
        }
        .ok_or(AgentSessionError::NothingToCompact)?;
        let before = self
            .runner
            .before_compact(
                SessionBeforeCompact {
                    preparation: preparation.clone(),
                    custom_instructions: custom_instructions.map(str::to_string),
                },
                &self.weak,
            )
            .await;
        if before.cancel {
            return Err(AgentSessionError::Cancelled);
        }
        let (result, from_extension) = match before.compaction {
            Some(result) => (result, true),
            None => {
                let stream_fn = self.models.stream_fn();
                let summarizer = Summarizer {
                    model: &model,
                    stream_fn: &*stream_fn,
                    options: StreamOptions {
                        api_key: self.models.api_key(&model.provider).await,
                        session_id: Some(self.session_id()),
                        cancel,
                        ..StreamOptions::default()
                    },
                    max_retries: settings.retry.max_retries,
                    retry_base_ms: settings.retry.base_delay_ms,
                };
                let result = compact(&preparation, &summarizer, custom_instructions)
                    .await
                    .map_err(AgentSessionError::Failed)?;
                (result, false)
            }
        };
        let entry_id = lock(&self.session).append_compaction(
            result.summary.clone(),
            Some(result.first_kept_entry_id.clone()),
            result.tokens_before,
            result.details.clone(),
            Some(result.usage),
            from_extension,
        );
        self.report_persistence();
        self.rebuild_messages();
        self.runner
            .emit(
                &SessionCompact {
                    entry_id,
                    from_extension,
                },
                &self.weak,
            )
            .await;
        Ok(result)
    }

    async fn navigate_tree(
        self: Arc<Self>,
        target_id: &str,
        summarize: bool,
        custom_instructions: Option<&str>,
    ) -> Result<(), AgentSessionError> {
        if self.agent.is_streaming() {
            return Err(AgentSessionError::Busy);
        }
        let (old_leaf, abandoned) = {
            let session = lock(&self.session);
            if session.entry(target_id).is_none() {
                return Err(SessionError::NotFound(target_id.into()).into());
            }
            let target_path: Vec<String> = session
                .branch_to(target_id)
                .iter()
                .map(|entry| entry.id.clone())
                .collect();
            let abandoned: Vec<_> = session
                .branch()
                .into_iter()
                .filter(|entry| !target_path.contains(&entry.id))
                .cloned()
                .collect();
            (session.leaf_id().map(str::to_string), abandoned)
        };
        let before = self
            .runner
            .before_tree(
                SessionBeforeTree {
                    target_id: target_id.to_string(),
                    old_leaf_id: old_leaf.clone(),
                },
                &self.weak,
            )
            .await;
        if before.cancel {
            return Err(AgentSessionError::Cancelled);
        }
        let summary = match before.summary {
            Some(summary) => Some((summary, None, true)),
            None if summarize && !abandoned.is_empty() => {
                let model = self.model().ok_or(AgentSessionError::NoModel)?;
                let settings = lock(&self.settings).clone();
                let stream_fn = self.models.stream_fn();
                let summarizer = Summarizer {
                    model: &model,
                    stream_fn: &*stream_fn,
                    options: StreamOptions {
                        api_key: self.models.api_key(&model.provider).await,
                        ..StreamOptions::default()
                    },
                    max_retries: settings.retry.max_retries,
                    retry_base_ms: settings.retry.base_delay_ms,
                };
                let budget = model
                    .context_window
                    .saturating_sub(settings.compaction.reserve_tokens)
                    .max(4_096);
                let entries: Vec<_> = abandoned.iter().collect();
                let (text, usage) =
                    summarize_branch(&entries, &summarizer, budget, custom_instructions)
                        .await
                        .map_err(AgentSessionError::Failed)?;
                Some((text, Some(usage), false))
            }
            None => None,
        };
        let summary_entry_id = {
            let mut session = lock(&self.session);
            match summary {
                Some((text, usage, from_extension)) => Some(session.branch_with_summary(
                    Some(target_id),
                    text,
                    None,
                    usage,
                    from_extension,
                )?),
                None => {
                    session.set_leaf(target_id)?;
                    None
                }
            }
        };
        self.report_persistence();
        self.rebuild_messages();
        let new_leaf = lock(&self.session).leaf_id().map(str::to_string);
        self.runner
            .emit(
                &SessionTree {
                    new_leaf_id: new_leaf,
                    old_leaf_id: old_leaf,
                    summary_entry_id,
                },
                &self.weak,
            )
            .await;
        Ok(())
    }

    async fn set_model(&self, model: Model) {
        let previous = lock(&self.model).replace(model.clone());
        self.agent.set_model(model.clone());
        let level = model.clamp_thinking_level(self.agent.thinking_level());
        self.agent.set_thinking_level(level);
        lock(&self.session).append_model_change(&model.provider, &model.id);
        self.report_persistence();
        self.runner
            .emit(&ModelSelect { model, previous }, &self.weak)
            .await;
    }

    async fn set_thinking_level(&self, level: ThinkingLevel) {
        let level = self.model().map_or(ThinkingLevel::Off, |model| {
            model.clamp_thinking_level(level)
        });
        let previous = self.agent.thinking_level();
        self.agent.set_thinking_level(level);
        lock(&self.session).append_thinking_level_change(level);
        self.report_persistence();
        self.runner
            .emit(&ThinkingLevelSelect { level, previous }, &self.weak)
            .await;
    }
}

/// Connects the agent's hooks to the session and its extensions.
struct SessionHooks {
    core: Weak<SessionCore>,
}

/// Replace the transcript's prompt with a forced one, keeping the tool declarations.
fn force_prompt(messages: &mut [SessionMessage], prompt: &str) {
    let mut first = true;
    for message in messages {
        if let SessionMessage::Llm(Message::System(system)) = message {
            system.content = if first {
                prompt.to_string()
            } else {
                String::new()
            };
            system.sections.clear();
            first = false;
        }
    }
}

#[async_trait]
impl AgentHooks<SessionMessage> for SessionHooks {
    async fn convert_to_llm(&self, messages: &[SessionMessage]) -> Vec<Message> {
        convert_to_llm(messages)
    }

    async fn transform_context(
        &self,
        messages: &[SessionMessage],
        _cancel: &CancellationToken,
    ) -> Option<Vec<SessionMessage>> {
        let core = self.core.upgrade()?;
        let forced = lock(&core.run).forced_prompt.clone();
        if forced.is_none() && !core.runner.has::<Context>() {
            return None;
        }
        let mut messages = messages.to_vec();
        if let Some(prompt) = forced {
            force_prompt(&mut messages, &prompt);
        }
        if core.runner.has::<Context>() {
            messages = core.runner.context_messages(messages, &self.core).await;
        }
        Some(messages)
    }

    async fn api_key(&self, provider: &str) -> Option<String> {
        self.core.upgrade()?.models.api_key(provider).await
    }

    async fn before_tool_call(
        &self,
        call: BeforeToolCall<'_, SessionMessage>,
        _cancel: &CancellationToken,
    ) -> Result<Option<BeforeToolCallResult>, ToolError> {
        let Some(core) = self.core.upgrade() else {
            return Ok(None);
        };
        if !core.runner.has::<ToolCall>() {
            return Ok(None);
        }
        let event = ToolCall {
            tool_call_id: call.tool_call.id.clone(),
            tool_name: call.tool_call.name.clone(),
            input: call.args.clone(),
        };
        core.runner.tool_call(event, &self.core).await
    }

    async fn after_tool_call(
        &self,
        call: AfterToolCall<'_, SessionMessage>,
        _cancel: &CancellationToken,
    ) -> Result<Option<AfterToolCallResult>, ToolError> {
        let Some(core) = self.core.upgrade() else {
            return Ok(None);
        };
        if !core.runner.has::<ToolResult>() {
            return Ok(None);
        }
        let event = ToolResult {
            tool_call_id: call.tool_call.id.clone(),
            tool_name: call.tool_call.name.clone(),
            input: call.args.clone(),
            content: call.result.content.clone(),
            details: call.result.details.clone(),
            is_error: call.is_error,
            usage: call.result.usage,
        };
        Ok(core.runner.tool_result(event, &self.core).await)
    }

    async fn finalize_message(&self, message: SessionMessage) -> SessionMessage {
        match self.core.upgrade() {
            Some(core) if core.runner.has::<MessageEnd>() => {
                core.runner.message_end(message, &self.core).await
            }
            _ => message,
        }
    }

    async fn prepare_next_turn(
        &self,
        turn: TurnContext<'_, SessionMessage>,
        _cancel: &CancellationToken,
    ) -> Option<TurnUpdate<SessionMessage>> {
        let core = self.core.upgrade()?;
        // Extension messages queued during the turn go in now, after its tool results.
        let pending = std::mem::take(&mut lock(&core.run).pending_custom);
        let model = core.model()?;
        let settings = lock(&core.settings).compaction.clone();
        let tokens = estimate_context_tokens(&turn.context.messages);
        let mut update = TurnUpdate {
            messages: pending,
            ..TurnUpdate::default()
        };
        // A long tool loop can outgrow the context within one run.
        if should_compact(tokens, model.context_window, &settings)
            && core
                .clone()
                .compact(CompactionReason::Threshold, None)
                .await
                .is_ok()
        {
            update.context = Some(AgentContext::new(
                core.agent.messages(),
                turn.context.tools.clone(),
            ));
        }
        (update.context.is_some() || !update.messages.is_empty()).then_some(update)
    }
}

/// Records the agent's messages in the session and relays its events.
struct SessionListener {
    core: Weak<SessionCore>,
}

#[async_trait]
impl AgentListener<SessionMessage> for SessionListener {
    async fn on_event(&self, event: &AgentEvent<SessionMessage>, _cancel: &CancellationToken) {
        let Some(core) = self.core.upgrade() else {
            return;
        };
        if core.runner.has::<AgentEventSeen>() {
            core.runner
                .emit(&AgentEventSeen(event.clone()), &self.core)
                .await;
        }
        match event {
            AgentEvent::MessageStart {
                message: SessionMessage::Llm(Message::User(user)),
            } => {
                let text = content_text(&user.content);
                let mut run = lock(&core.run);
                let removed = if let Some(index) =
                    run.steering_texts.iter().position(|queued| *queued == text)
                {
                    run.steering_texts.remove(index);
                    true
                } else if let Some(index) = run
                    .follow_up_texts
                    .iter()
                    .position(|queued| *queued == text)
                {
                    run.follow_up_texts.remove(index);
                    true
                } else {
                    false
                };
                drop(run);
                if removed {
                    core.emit_queue();
                }
            }
            AgentEvent::MessageEnd { message } => {
                core.persist(message);
                if let SessionMessage::Llm(Message::Assistant(assistant)) = message
                    && !assistant.is_failure()
                {
                    let attempt = std::mem::take(&mut lock(&core.run).retry_attempt);
                    if attempt > 0 {
                        core.emit(AgentSessionEvent::RetryEnd {
                            success: true,
                            attempt,
                            error: None,
                        });
                    }
                }
            }
            _ => {}
        }
        core.emit(AgentSessionEvent::Agent(event.clone()));
    }
}

// What extensions can do with their session.
impl ExtensionContext {
    pub fn cwd(&self) -> Option<PathBuf> {
        Some(self.core()?.cwd.clone())
    }

    pub fn session_id(&self) -> Option<String> {
        Some(self.core()?.session_id())
    }

    pub fn model(&self) -> Option<Model> {
        self.core()?.model()
    }

    pub fn thinking_level(&self) -> ThinkingLevel {
        self.core()
            .map_or(ThinkingLevel::Off, |core| core.agent.thinking_level())
    }

    pub fn is_idle(&self) -> bool {
        self.core().is_none_or(|core| !core.agent.is_streaming())
    }

    pub fn abort(&self) {
        if let Some(core) = self.core() {
            core.abort();
        }
    }

    pub fn has_pending_messages(&self) -> bool {
        self.core()
            .is_some_and(|core| core.agent.has_queued_messages())
    }

    /// The system prompt the transcript declares.
    pub fn system_prompt(&self) -> String {
        self.core()
            .map(|core| core.agent.system_prompt())
            .unwrap_or_default()
    }

    /// The system prompt the next prompt would declare.
    pub fn next_system_prompt(&self) -> String {
        self.core()
            .and_then(|core| render(&core.prompt_options()).ok())
            .unwrap_or_default()
    }

    pub fn ui(&self) -> Arc<dyn ExtensionUi> {
        match self.core() {
            Some(core) => core.ui.clone(),
            None => Arc::new(crate::extensions::NoUi),
        }
    }

    pub fn context_usage(&self) -> Option<ContextUsage> {
        self.core()?.context_usage()
    }

    /// Read the session tree.
    pub fn read_session<T>(&self, read: impl FnOnce(&SessionManager) -> T) -> Option<T> {
        Some(read(&lock(&self.core()?.session)))
    }

    /// Store extension state in the session. It is never sent to the model.
    pub fn append_entry(&self, custom_type: &str, data: Option<Value>) {
        if let Some(core) = self.core() {
            lock(&core.session).append_custom_entry(custom_type, data);
            core.report_persistence();
        }
    }

    pub fn set_session_name(&self, name: &str) {
        if let Some(core) = self.core() {
            lock(&core.session).append_session_info(Some(name.to_string()));
            core.report_persistence();
        }
    }

    pub fn set_label(&self, entry_id: &str, label: Option<&str>) -> Result<(), SessionError> {
        let Some(core) = self.core() else {
            return Ok(());
        };
        lock(&core.session).append_label(entry_id, label.map(str::to_string))?;
        core.report_persistence();
        Ok(())
    }

    pub fn active_tools(&self) -> Vec<String> {
        self.core()
            .map(|core| lock(&core.active_tools).clone())
            .unwrap_or_default()
    }

    pub fn set_active_tools(&self, names: &[String]) {
        if let Some(core) = self.core() {
            core.set_active_tools(names);
        }
    }

    pub async fn set_model(&self, model: Model) {
        if let Some(core) = self.core() {
            core.set_model(model).await;
        }
    }

    pub async fn set_thinking_level(&self, level: ThinkingLevel) {
        if let Some(core) = self.core() {
            core.set_thinking_level(level).await;
        }
    }

    /// Add a message to the conversation as `delivery` says.
    pub fn send_message(&self, draft: CustomMessageDraft, delivery: Delivery) {
        if let Some(core) = self.core() {
            deliver(core, custom_message(draft), delivery);
        }
    }

    /// Send a user message. It always leads to a turn: now when idle, otherwise as steering
    /// or a follow-up.
    pub fn send_user_message(&self, text: &str, delivery: Delivery) {
        let Some(core) = self.core() else { return };
        let delivery = if delivery == Delivery::FollowUp {
            Delivery::FollowUp
        } else {
            Delivery::Steer
        };
        deliver(core, user_message(text, Vec::new()), delivery);
    }
}

fn deliver(core: Arc<SessionCore>, message: SessionMessage, delivery: Delivery) {
    let streaming = core.agent.is_streaming();
    match delivery {
        Delivery::NextTurn => lock(&core.run).next_turn_messages.push(message),
        Delivery::Append if streaming => lock(&core.run).pending_custom.push(message),
        Delivery::Append => core.append_now(message),
        Delivery::Steer if streaming => core.queue(StreamingBehavior::Steer, message),
        Delivery::FollowUp if streaming => core.queue(StreamingBehavior::FollowUp, message),
        Delivery::Steer | Delivery::FollowUp => {
            tokio::spawn(async move {
                let _ = core.run_prompt(vec![message]).await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::transcript::system_message_text;

    #[test]
    fn a_forced_prompt_replaces_text_but_keeps_tools() {
        let mut leading = SystemMessage {
            content: String::new(),
            tools_added: vec![pi_ai::Tool::new("read", "Read", serde_json::json!({}))],
            ..SystemMessage::default()
        };
        leading
            .sections
            .insert("preamble".into(), Some("old".into()));
        let mut messages = vec![
            SessionMessage::Llm(Message::System(leading)),
            user_message("hi", Vec::new()),
            SessionMessage::Llm(Message::System(SystemMessage {
                content: "later".into(),
                ..SystemMessage::default()
            })),
        ];
        force_prompt(&mut messages, "Exactly this.");
        let llm = convert_to_llm(&messages);
        assert_eq!(
            pi_ai::transcript::current_system_prompt(&llm),
            "Exactly this."
        );
        let SessionMessage::Llm(Message::System(first)) = &messages[0] else {
            panic!()
        };
        assert_eq!(first.tools_added.len(), 1);
        assert_eq!(system_message_text(first), "Exactly this.");
    }
}
