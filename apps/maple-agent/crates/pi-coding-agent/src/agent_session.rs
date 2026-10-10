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
use std::path::{Path, PathBuf};
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
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::bash_executor::{BashResult, OnChunk, execute_bash_with_operations};
use crate::compaction::{
    CompactionResult, Summarizer, compact, estimate_context_tokens, prepare_compaction,
    should_compact, summarize_branch,
};
use crate::extensions::{
    AgentEventSeen, BeforeAgentStart, BeforeProviderRequest, Context, CustomMessageDraft,
    Extension, ExtensionContext, ExtensionErrorReport, ExtensionRunner, ExtensionUi, Input,
    InputAction, InputSource, MessageEnd, ModelSelect, RegisteredTool, SessionBeforeCompact,
    SessionBeforeTree, SessionCompact, SessionShutdown, SessionStart, SessionStartReason,
    SessionTree, ThinkingLevelSelect, ToolCall, ToolPrompt, ToolResult,
};
use crate::messages::{BashExecutionMessage, CustomMessage, SessionMessage, convert_to_llm};
use crate::models::ModelRegistry;
use crate::resources::{Resources, expand_prompt_template, expand_skill_command};
use crate::session::{EntryKind, SessionError, SessionManager};
use crate::settings::Settings;
use crate::store::SessionStore;
use crate::system_prompt::{
    SystemPromptOptions, ToolPromptInfo, build_prompt_state, diff_sections, render,
};
use crate::tools::{
    BashOperations, DEFAULT_TOOL_NAMES, LocalShellOperations, ToolContext, ToolsOptions,
    create_all_tools, expand_path, normalize_tool_result_images,
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
    /// An extension cancelled the operation, or stopping the session did.
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
            Self::Cancelled => formatter.write_str("Cancelled"),
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
    /// Output of a shell command the user runs, cleaned, as it comes.
    BashExecutionUpdate {
        /// The identifier the command was started with.
        id: Option<String>,
        delta: String,
    },
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

/// Token totals of a session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TokenTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// The four above together.
    pub total: u64,
}

/// What a session holds, over every entry and branch, as Pi's session stats count it.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionStats {
    pub session_file: Option<PathBuf>,
    pub session_id: String,
    pub user_messages: usize,
    pub assistant_messages: usize,
    pub tool_calls: usize,
    pub tool_results: usize,
    pub total_messages: usize,
    /// Every response's usage, a tool's own and summaries' included.
    pub tokens: TokenTotals,
    pub cost: f64,
    pub context_usage: Option<ContextUsage>,
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
    /// How Pi's built-in tools are set up. The settings' shell path, command prefix and
    /// image resizing fill in what these leave open.
    pub tool_options: ToolsOptions,
    /// The built-in tools the model gets at the start: `read`, `bash`, `edit` and
    /// `write` when `None`, none for an empty list. The rest stay registered and can be
    /// turned on.
    pub builtin_tools: Option<Vec<String>>,
    /// The host's tools. One with a built-in tool's name replaces it.
    pub tools: Vec<RegisteredTool>,
    pub extensions: Vec<Arc<dyn Extension>>,
    pub ui: Arc<dyn ExtensionUi>,
    /// Replaces the default preamble, tool list and rules, and `SYSTEM.md`.
    pub custom_prompt: Option<String>,
    /// How the model reads skill files. Without it, `read` or else `bash` does, and
    /// with neither, skills are left out of the prompt.
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
            tool_options: ToolsOptions::default(),
            builtin_tools: None,
            tools: Vec::new(),
            extensions: Vec::new(),
            ui: Arc::new(crate::extensions::NoUi),
            custom_prompt: None,
            skill_load_hint: None,
        }
    }
}

type Listener = Arc<dyn Fn(&AgentSessionEvent) + Send + Sync>;

/// What the session is doing. A prompt is active from its start until it settles, its
/// retries and compactions included; maintenance is a manual compaction, a move in the
/// tree or a fork.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Activity {
    Idle,
    Prompt,
    Maintenance,
}

/// Holds the session's activity and returns it to idle when dropped, also when the
/// future holding it is dropped.
struct ActivityGuard<'a> {
    activity: &'a watch::Sender<Activity>,
}

impl Drop for ActivityGuard<'_> {
    fn drop(&mut self) {
        self.activity.send_replace(Activity::Idle);
    }
}

/// Clears a running compaction when it ends, also when the future running it is dropped,
/// so the session does not stay busy.
struct CompactionGuard<'a> {
    core: &'a SessionCore,
    reason: CompactionReason,
    ended: bool,
}

impl Drop for CompactionGuard<'_> {
    fn drop(&mut self) {
        let token = lock(&self.core.run).compaction_cancel.take();
        if !self.ended {
            // Its summary requests stop with it.
            if let Some(token) = token {
                token.cancel();
            }
            self.core.emit(AgentSessionEvent::CompactionEnd {
                reason: self.reason,
                result: None,
                error: Some("Compaction was cancelled".into()),
            });
        }
    }
}

#[derive(Default)]
struct RunState {
    abort_requested: bool,
    retry_attempt: u32,
    retry_cancel: Option<CancellationToken>,
    compaction_cancel: Option<CancellationToken>,
    branch_summary_cancel: Option<CancellationToken>,
    overflow_recovery_attempted: bool,
    /// A complete system prompt that replaces the transcript's for the current run.
    forced_prompt: Option<String>,
    next_turn_messages: Vec<SessionMessage>,
    /// Extension messages waiting for the current turn to end.
    pending_custom: Vec<SessionMessage>,
    /// User shell commands that ended during the run, added when it ends.
    pending_bash: Vec<SessionMessage>,
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
    host_prompt: HostPrompt,
    prompt_base: Mutex<SystemPromptOptions>,
    model: Mutex<Option<Model>>,
    tools: Mutex<Vec<RegisteredTool>>,
    active_tools: Mutex<Vec<String>>,
    listeners: Mutex<Vec<(u64, Listener)>>,
    next_listener: AtomicU64,
    activity: watch::Sender<Activity>,
    run: Mutex<RunState>,
    /// User shell commands running now, by a key of their own.
    bash_runs: Mutex<Vec<(u64, CancellationToken)>>,
    next_bash: AtomicU64,
    weak: Weak<SessionCore>,
}

/// A conversation with an agent, recorded in a session.
#[derive(Clone)]
pub struct AgentSession {
    core: Arc<SessionCore>,
}

/// The prompt and appended text the host gave, which win over `SYSTEM.md` and
/// `APPEND_SYSTEM.md`.
struct HostPrompt {
    custom: Option<String>,
    append: Option<String>,
}

impl HostPrompt {
    /// Fill the prompt's resource parts from `resources`, under the host's own text.
    fn apply(&self, base: &mut SystemPromptOptions, resources: &Resources) {
        let text = |file: &Option<crate::resources::ContextFile>| {
            file.as_ref().map(|file| file.content.clone())
        };
        base.custom_prompt = self
            .custom
            .clone()
            .or_else(|| text(&resources.system_prompt));
        base.append = self
            .append
            .clone()
            .or_else(|| text(&resources.append_system_prompt))
            .unwrap_or_default();
        base.context_files = resources.context_files.clone();
        base.skills = resources.skills.clone();
    }
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
        let requested_thinking_level = if has_history {
            projection.thinking_level
        } else {
            options.settings.default_thinking_level.unwrap_or_default()
        };
        let thinking_level = model.as_ref().map_or(ThinkingLevel::Off, |model| {
            model.clamp_thinking_level(requested_thinking_level)
        });
        let host_prompt = HostPrompt {
            custom: options.custom_prompt.clone(),
            append: options.settings.append_system_prompt.clone(),
        };
        let mut prompt_base = SystemPromptOptions {
            app_name: options.app_name.clone(),
            cwd: options.cwd.to_string_lossy().into_owned(),
            skill_load_hint: options.skill_load_hint.clone(),
            ..SystemPromptOptions::default()
        };
        host_prompt.apply(&mut prompt_base, &options.resources);
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
            let context = ExtensionContext {
                core: weak.clone(),
                extension: Arc::from("builtin"),
            };
            let mut tool_options = options.tool_options;
            tool_options.bash.shell_path = tool_options.bash.shell_path.or_else(|| {
                options
                    .settings
                    .shell_path
                    .as_ref()
                    .map(|path| expand_path(&path.to_string_lossy()))
            });
            tool_options.bash.command_prefix = tool_options
                .bash
                .command_prefix
                .or_else(|| options.settings.shell_command_prefix.clone());
            tool_options.read.auto_resize_images &= options.settings.images.auto_resize;
            let builtin_active = options.builtin_tools.unwrap_or_else(|| {
                DEFAULT_TOOL_NAMES
                    .iter()
                    .map(|name| name.to_string())
                    .collect()
            });
            let mut tools = create_all_tools(
                &options.cwd,
                &tool_options,
                &ToolContext::for_session(&options.app_name, context),
                &builtin_active,
            );
            let builtin_count = tools.len();
            for tool in options
                .tools
                .into_iter()
                .chain(runner.tools().iter().cloned())
            {
                // A host or extension tool with a built-in tool's name replaces it, as
                // in Pi.
                match tools[..builtin_count]
                    .iter()
                    .position(|builtin| builtin.tool.name() == tool.tool.name())
                {
                    Some(index) => tools[index] = tool,
                    None => tools.push(tool),
                }
            }
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
                host_prompt,
                prompt_base: Mutex::new(prompt_base),
                model: Mutex::new(model),
                tools: Mutex::new(tools),
                active_tools: Mutex::new(active),
                listeners: Mutex::new(Vec::new()),
                next_listener: AtomicU64::new(1),
                activity: watch::channel(Activity::Idle).0,
                run: Mutex::new(RunState::default()),
                bash_runs: Mutex::new(Vec::new()),
                next_bash: AtomicU64::new(1),
                weak: weak.clone(),
            }
        });

        for provider in core.runner.providers() {
            models.register_models(provider.models.clone());
            if let Some((api, implementation)) = &provider.api {
                models.register_api(api, implementation.clone());
            }
        }
        // A provider an extension registers can serve the session's model, so look it up
        // again now that they are registered.
        if let Some(model) = projection
            .model
            .as_ref()
            .and_then(|(provider, id)| models.find(provider, id))
        {
            core.agent.set_model(model.clone());
            core.agent
                .set_thinking_level(model.clamp_thinking_level(requested_thinking_level));
            *lock(&core.model) = Some(model);
        }
        let thinking_level = core.agent.thinking_level();
        if !has_history && thinking_level != ThinkingLevel::Off {
            // Recorded so a resumed session starts at the same level.
            lock(&core.session).append_thinking_level_change(thinking_level);
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

    /// Start a turn with a custom message and wait for it, as Pi's `sendCustomMessage`
    /// does with `triggerTurn`, for a host that hands the model something the user did
    /// not type, such as a background agent's result. A message not to `display` stays
    /// out of the transcript the user sees but reaches the model. During a prompt it is
    /// queued as steering instead.
    pub async fn send_custom_message(
        &self,
        draft: CustomMessageDraft,
    ) -> Result<PromptOutcome, AgentSessionError> {
        let core = self.core.clone();
        let activity = loop {
            if let Some(activity) = core.begin(Activity::Prompt) {
                break activity;
            }
            match core.activity() {
                Activity::Prompt => {
                    core.queue(StreamingBehavior::Steer, custom_message(draft));
                    return Ok(PromptOutcome::Queued);
                }
                // It settled in between.
                Activity::Idle => continue,
                _ => return Err(AgentSessionError::Busy),
            }
        };
        core.run_delivered(activity, custom_message(draft)).await?;
        Ok(PromptOutcome::Completed)
    }

    /// Deliver `text` and `images` after the current turn's tool calls.
    pub fn steer(&self, text: &str, images: Vec<ImageContent>) {
        self.core
            .queue(StreamingBehavior::Steer, user_message(text, images));
    }

    /// Deliver `text` and `images` once the agent would otherwise stop.
    pub fn follow_up(&self, text: &str, images: Vec<ImageContent>) {
        self.core
            .queue(StreamingBehavior::FollowUp, user_message(text, images));
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

    /// Wait until no prompt, retry or compaction is in progress.
    pub async fn wait_for_idle(&self) {
        let mut activity = self.core.activity.subscribe();
        let _ = activity
            .wait_for(|activity| *activity == Activity::Idle)
            .await;
    }

    /// Compact the current branch.
    pub async fn compact(
        &self,
        custom_instructions: Option<&str>,
    ) -> Result<CompactionResult, AgentSessionError> {
        let _maintenance = self
            .core
            .begin(Activity::Maintenance)
            .ok_or(AgentSessionError::Busy)?;
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
        let _maintenance = self
            .core
            .begin(Activity::Maintenance)
            .ok_or(AgentSessionError::Busy)?;
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
        let _maintenance = self
            .core
            .begin(Activity::Maintenance)
            .ok_or(AgentSessionError::Busy)?;
        lock(&self.core.session).fork(entry_id, store)?;
        self.core.report_persistence();
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

    /// Whether a prompt is in progress, including its retries and compactions.
    pub fn is_streaming(&self) -> bool {
        self.core.activity() == Activity::Prompt
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

    /// Offer `tool` from the next prompt on, as an extension's `registerTool` can once the
    /// session runs, for tools found later, such as an MCP server's once it connects. A tool
    /// with a registered tool's name replaces it and keeps whether it is active.
    pub fn register_tool(&self, tool: RegisteredTool) {
        self.core.register_tool(tool);
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

    /// Use `resources` from the next prompt on, as Pi's `reload` does with what its
    /// resource loader finds again: context files, skills and prompt templates, and
    /// `SYSTEM.md` and `APPEND_SYSTEM.md` where the host gave no text of its own. The
    /// transcript's prompt is brought up to date by the sections that changed.
    pub fn set_resources(&self, resources: Resources) {
        self.core
            .host_prompt
            .apply(&mut lock(&self.core.prompt_base), &resources);
        *lock(&self.core.resources) = resources;
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
        self.abort_bash();
        self.core.abort();
        self.wait_for_idle().await;
        self.core
            .runner
            .emit(&SessionShutdown, &self.core.weak)
            .await;
    }
}

/// How [`AgentSession::execute_bash`] runs a command.
#[derive(Clone, Default)]
pub struct BashCommandOptions {
    /// Keep the command and its output out of the model's context (`!!command`).
    pub exclude_from_context: bool,
    /// An identifier repeated in the command's [`AgentSessionEvent::BashExecutionUpdate`]s.
    pub id: Option<String>,
    /// Where to run it; this machine's bash, as the settings choose it, when `None`.
    pub operations: Option<Arc<dyn BashOperations>>,
}

/// Forgets a user shell command when it ends, also when the future running it is dropped.
struct BashRunGuard<'a> {
    core: &'a SessionCore,
    key: u64,
}

impl Drop for BashRunGuard<'_> {
    fn drop(&mut self) {
        lock(&self.core.bash_runs).retain(|(key, _)| *key != self.key);
    }
}

impl AgentSession {
    /// Run a shell command for the user (`!command`) in the session's folder, with the
    /// settings' shell and command prefix, and add it and its output to the conversation;
    /// with `exclude_from_context` (`!!command`) the model does not see it. During a run it
    /// is added when the run ends, so it cannot come between a tool call and its result.
    pub async fn execute_bash(
        &self,
        command: &str,
        options: BashCommandOptions,
    ) -> Result<BashResult, AgentSessionError> {
        let core = &self.core;
        let settings = lock(&core.settings).clone();
        let resolved = match settings.shell_command_prefix.as_deref() {
            Some(prefix) if !prefix.is_empty() => format!("{prefix}\n{command}"),
            _ => command.to_string(),
        };
        let operations = options.operations.clone().unwrap_or_else(|| {
            let shell_path = settings
                .shell_path
                .as_ref()
                .map(|path| expand_path(&path.to_string_lossy()));
            Arc::new(LocalShellOperations::bash(shell_path))
        });
        let file_prefix =
            ToolContext::new(&lock(&core.prompt_base).app_name).output_file_prefix("bash");
        let on_chunk: OnChunk = {
            let weak = core.weak.clone();
            let id = options.id.clone();
            Arc::new(move |delta: &str| {
                if let Some(core) = weak.upgrade() {
                    core.emit(AgentSessionEvent::BashExecutionUpdate {
                        id: id.clone(),
                        delta: delta.to_string(),
                    });
                }
            })
        };
        let cancel = CancellationToken::new();
        let key = core.next_bash.fetch_add(1, Ordering::Relaxed);
        lock(&core.bash_runs).push((key, cancel.clone()));
        let _running = BashRunGuard { core, key };
        let result = execute_bash_with_operations(
            &resolved,
            &core.cwd,
            operations.as_ref(),
            &file_prefix,
            Some(on_chunk),
            cancel,
        )
        .await
        .map_err(AgentSessionError::Failed)?;
        self.record_bash_result(command, &result, options.exclude_from_context);
        Ok(result)
    }

    /// Add a shell command's result to the conversation, as [`Self::execute_bash`] does,
    /// for hosts and extensions that run commands themselves.
    pub fn record_bash_result(
        &self,
        command: &str,
        result: &BashResult,
        exclude_from_context: bool,
    ) {
        let message = SessionMessage::BashExecution(BashExecutionMessage {
            command: command.to_string(),
            output: result.output.clone(),
            exit_code: result.exit_code,
            cancelled: result.cancelled,
            truncated: result.truncated,
            full_output_path: result
                .full_output_path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            timestamp: now_ms(),
            exclude_from_context,
        });
        if self.core.activity() == Activity::Prompt {
            lock(&self.core.run).pending_bash.push(message);
        } else {
            self.core.append_now(message);
        }
    }

    /// Message counts and token and cost totals over the whole session.
    pub fn session_stats(&self) -> SessionStats {
        let mut stats = SessionStats {
            session_file: None,
            session_id: self.session_id(),
            user_messages: 0,
            assistant_messages: 0,
            tool_calls: 0,
            tool_results: 0,
            total_messages: 0,
            tokens: TokenTotals::default(),
            cost: 0.0,
            context_usage: self.context_usage(),
        };
        fn add(usage: &pi_ai::Usage, stats: &mut SessionStats) {
            stats.tokens.input += usage.input;
            stats.tokens.output += usage.output;
            stats.tokens.cache_read += usage.cache_read;
            stats.tokens.cache_write += usage.cache_write;
            stats.cost += usage.cost.total;
        }
        self.with_session(|session| {
            stats.session_file = session.session_file().map(Path::to_path_buf);
            for entry in session.entries() {
                let message = match &entry.kind {
                    EntryKind::Compaction {
                        usage: Some(usage), ..
                    }
                    | EntryKind::BranchSummary {
                        usage: Some(usage), ..
                    } => {
                        add(usage, &mut stats);
                        continue;
                    }
                    // The prompt is kept in the transcript here, not counted as Pi's.
                    EntryKind::Message {
                        message: SessionMessage::Llm(Message::System(_)),
                    } => continue,
                    EntryKind::Message { message } => message,
                    _ => continue,
                };
                stats.total_messages += 1;
                match message {
                    SessionMessage::Llm(Message::User(_)) => stats.user_messages += 1,
                    SessionMessage::Llm(Message::Assistant(assistant)) => {
                        stats.assistant_messages += 1;
                        stats.tool_calls += assistant.tool_calls().count();
                        add(&assistant.usage, &mut stats);
                    }
                    SessionMessage::Llm(Message::ToolResult(result)) => {
                        stats.tool_results += 1;
                        if let Some(usage) = &result.usage {
                            add(usage, &mut stats);
                        }
                    }
                    _ => {}
                }
            }
        });
        let tokens = &mut stats.tokens;
        tokens.total = tokens.input + tokens.output + tokens.cache_read + tokens.cache_write;
        stats
    }

    /// Stop the user shell commands that are running.
    pub fn abort_bash(&self) {
        for (_, cancel) in lock(&self.core.bash_runs).iter() {
            cancel.cancel();
        }
    }

    pub fn is_bash_running(&self) -> bool {
        !lock(&self.core.bash_runs).is_empty()
    }

    /// Whether user shell command results wait for the current run to end.
    pub fn has_pending_bash_messages(&self) -> bool {
        !lock(&self.core.run).pending_bash.is_empty()
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
    fn activity(&self) -> Activity {
        *self.activity.borrow()
    }

    /// Take the session for `kind`, or `None` when it is not idle.
    fn begin(&self, kind: Activity) -> Option<ActivityGuard<'_>> {
        let mut started = false;
        self.activity.send_if_modified(|activity| {
            started = *activity == Activity::Idle;
            if started {
                *activity = kind;
            }
            started
        });
        // Built only on success: dropping a guard makes the session idle.
        if started {
            Some(ActivityGuard {
                activity: &self.activity,
            })
        } else {
            None
        }
    }

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

    /// The active tools, in order.
    fn executable_tools(&self) -> Vec<Arc<dyn AgentTool>> {
        let active = lock(&self.active_tools).clone();
        let tools = lock(&self.tools);
        active
            .iter()
            .filter_map(|name| tools.iter().find(|tool| tool.tool.name() == name))
            .map(|tool| tool.tool.clone())
            .collect()
    }

    fn sync_tools(&self) {
        self.agent.set_tools(self.executable_tools());
    }

    fn register_tool(&self, tool: RegisteredTool) {
        let name = tool.tool.name().to_string();
        let activate = {
            let mut tools = lock(&self.tools);
            match tools
                .iter()
                .position(|existing| existing.tool.name() == name)
            {
                Some(index) => {
                    tools[index] = tool;
                    false
                }
                None => {
                    let active = tool.active;
                    tools.push(tool);
                    active
                }
            }
        };
        if activate {
            lock(&self.active_tools).push(name);
        }
        self.sync_tools();
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
        // The queue lists typed prompts. Extension messages are delivered unlisted, since
        // only a user message's start takes its text off the list.
        let listed = matches!(message, SessionMessage::Llm(Message::User(_)));
        if listed {
            let text = message.text();
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
        if listed {
            self.emit_queue();
        }
    }

    fn abort(&self) {
        let mut run = lock(&self.run);
        run.abort_requested = true;
        for token in [
            run.retry_cancel.take(),
            run.compaction_cancel.clone(),
            run.branch_summary_cancel.clone(),
        ]
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
        let activity = loop {
            if let Some(activity) = self.begin(Activity::Prompt) {
                break activity;
            }
            // A prompt in progress takes this one as steering or a follow-up, when asked.
            match (self.activity(), options.streaming_behavior) {
                (Activity::Prompt, Some(behavior)) => {
                    self.queue(behavior, user_message(&text, images));
                    return Ok(PromptOutcome::Queued);
                }
                // It settled in between.
                (Activity::Idle, _) => continue,
                _ => return Err(AgentSessionError::Busy),
            }
        };
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
        self.run_prompt(activity, messages).await?;
        Ok(PromptOutcome::Completed)
    }

    /// Start a run with a message an extension sent, bringing the prompt up to date first.
    async fn run_delivered(
        self: &Arc<Self>,
        activity: ActivityGuard<'_>,
        message: SessionMessage,
    ) -> Result<(), AgentSessionError> {
        if self.model().is_none() {
            return Err(AgentSessionError::NoModel);
        }
        let mut messages: Vec<SessionMessage> = self
            .system_update(&self.prompt_options())?
            .into_iter()
            .collect();
        messages.push(message);
        self.run_prompt(activity, messages).await
    }

    async fn run_prompt(
        self: &Arc<Self>,
        activity: ActivityGuard<'_>,
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
        self.flush_pending_bash();
        self.flush_pending_custom();
        self.end_retry();
        lock(&self.run).forced_prompt = None;
        drop(activity);
        self.emit(AgentSessionEvent::Settled);
        result
    }

    /// End a retry the run left unfinished, so the next prompt counts from the start.
    fn end_retry(&self) {
        let attempt = std::mem::take(&mut lock(&self.run).retry_attempt);
        if attempt > 0 {
            let error = if self.abort_requested() {
                "Retry cancelled"
            } else {
                "The run ended before a retry succeeded"
            };
            self.emit(AgentSessionEvent::RetryEnd {
                success: false,
                attempt,
                error: Some(error.into()),
            });
        }
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
            // An error, or a reply cut off before any output, leaves the prompt unanswered:
            // the reply leaves the context and the prompt runs again after compaction.
            let unanswered = last.is_failure() || last.stop_reason == pi_ai::StopReason::Length;
            if unanswered {
                self.omit_last_assistant();
            }
            return match self.clone().compact(CompactionReason::Overflow, None).await {
                Ok(_) => unanswered || self.agent.has_queued_messages(),
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

    fn flush_pending_bash(&self) {
        let pending = std::mem::take(&mut lock(&self.run).pending_bash);
        for message in pending {
            self.append_now(message);
        }
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
                SessionMessage::Llm(_) | SessionMessage::BashExecution(_) => {
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
        let mut running = CompactionGuard {
            core: &self,
            reason,
            ended: false,
        };
        let result = self.compact_inner(custom_instructions, token).await;
        running.ended = true;
        drop(running);
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
                // Registered so that stopping the session stops the summary too.
                let cancel = CancellationToken::new();
                lock(&self.run).branch_summary_cancel = Some(cancel.clone());
                let summarizer = Summarizer {
                    model: &model,
                    stream_fn: &*stream_fn,
                    options: StreamOptions {
                        api_key: self.models.api_key(&model.provider).await,
                        session_id: Some(self.session_id()),
                        cancel: cancel.clone(),
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
                let summary =
                    summarize_branch(&entries, &summarizer, budget, custom_instructions).await;
                lock(&self.run).branch_summary_cancel = None;
                // A stopped summary leaves the session where it was.
                if cancel.is_cancelled() {
                    return Err(AgentSessionError::Cancelled);
                }
                let (text, usage) = summary.map_err(AgentSessionError::Failed)?;
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
        let rewrite = if core.runner.has::<ToolResult>() {
            let event = ToolResult {
                tool_call_id: call.tool_call.id.clone(),
                tool_name: call.tool_call.name.clone(),
                input: call.args.clone(),
                content: call.result.content.clone(),
                details: call.result.details.clone(),
                is_error: call.is_error,
                usage: call.result.usage,
            };
            core.runner.tool_result(event, &self.core).await
        } else {
            None
        };
        // After the extensions, so images they add or replace fit too.
        let content = rewrite
            .as_ref()
            .and_then(|rewrite| rewrite.content.as_deref())
            .unwrap_or(&call.result.content);
        let auto_resize = lock(&core.settings).images.auto_resize;
        let Some(content) = normalize_tool_result_images(content, auto_resize).await else {
            return Ok(rewrite);
        };
        Ok(Some(AfterToolCallResult {
            content: Some(content),
            ..rewrite.unwrap_or_default()
        }))
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
        cancel: &CancellationToken,
    ) -> Option<TurnUpdate<SessionMessage>> {
        let core = self.core.upgrade()?;
        // Extension messages queued during the turn go in now, after its tool results.
        let mut messages = std::mem::take(&mut lock(&core.run).pending_custom);
        let model = core.model()?;
        let settings = lock(&core.settings).compaction.clone();
        let tokens = estimate_context_tokens(&turn.context.messages);
        // A long tool loop can outgrow the context within one run. An aborted run ends
        // with its next request, so it is not compacted first.
        let compacted = !cancel.is_cancelled()
            && should_compact(tokens, model.context_window, &settings)
            && core
                .clone()
                .compact(CompactionReason::Threshold, None)
                .await
                .is_ok();
        // The next request uses the session's current model, thinking level, tools and
        // prompt, which extensions or the host may have changed during the turn. The
        // loop declares a changed tool set to the model.
        let tools = core.executable_tools();
        let tools_changed = !tools.iter().map(|tool| tool.name()).eq(turn
            .context
            .tools
            .iter()
            .map(|tool| tool.name()));
        let context = if compacted {
            Some(AgentContext::new(core.agent.messages(), tools))
        } else if tools_changed {
            Some(AgentContext::new(turn.context.messages.clone(), tools))
        } else {
            None
        };
        messages.extend(core.system_update(&core.prompt_options()).ok().flatten());
        Some(TurnUpdate {
            context,
            messages,
            model: Some(model),
            thinking_level: Some(core.agent.thinking_level()),
        })
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

    /// The file the session is kept in, when its store keeps one.
    pub fn session_file(&self) -> Option<PathBuf> {
        self.read_session(|session| session.session_file().map(Path::to_path_buf))?
    }

    pub fn model(&self) -> Option<Model> {
        self.core()?.model()
    }

    pub fn thinking_level(&self) -> ThinkingLevel {
        self.core()
            .map_or(ThinkingLevel::Off, |core| core.agent.thinking_level())
    }

    pub fn is_idle(&self) -> bool {
        self.core()
            .is_none_or(|core| core.activity() == Activity::Idle)
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

    /// Offer `tool` from the next prompt on, as Pi's `registerTool` can at any time, for
    /// tools found after the extension loaded, such as an MCP server's once it connects.
    /// A tool with a registered tool's name replaces it and keeps whether it is active.
    pub fn register_tool(&self, tool: Arc<dyn AgentTool>, prompt: ToolPrompt, active: bool) {
        if let Some(core) = self.core() {
            core.register_tool(RegisteredTool {
                tool,
                prompt,
                active,
                extension: Some(self.extension.to_string()),
            });
        }
    }

    /// Every registered tool's name.
    pub fn tool_names(&self) -> Vec<String> {
        self.core()
            .map(|core| {
                lock(&core.tools)
                    .iter()
                    .map(|tool| tool.tool.name().to_string())
                    .collect()
            })
            .unwrap_or_default()
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
        let Some(core) = self.core() else { return };
        let message = custom_message(draft);
        let prompting = core.activity() == Activity::Prompt;
        let behavior = match delivery {
            Delivery::NextTurn => {
                lock(&core.run).next_turn_messages.push(message);
                return;
            }
            Delivery::Append if prompting => {
                lock(&core.run).pending_custom.push(message);
                return;
            }
            Delivery::Append => {
                core.append_now(message);
                return;
            }
            Delivery::Steer => StreamingBehavior::Steer,
            Delivery::FollowUp => StreamingBehavior::FollowUp,
        };
        if prompting {
            core.queue(behavior, message);
            return;
        }
        let extension = self.extension.clone();
        tokio::spawn(async move {
            // A prompt that started meanwhile takes the message instead.
            let Some(activity) = core.begin(Activity::Prompt) else {
                core.queue(behavior, message);
                return;
            };
            if let Err(error) = core.run_delivered(activity, message).await {
                core.runner.report(&extension, "send_message", error);
            }
        });
    }

    /// Send a user message. It always leads to a turn: now when idle, going through input
    /// handlers like a typed prompt, otherwise as steering or a follow-up.
    pub fn send_user_message(&self, text: &str, delivery: Delivery) {
        let Some(core) = self.core() else { return };
        let behavior = if delivery == Delivery::FollowUp {
            StreamingBehavior::FollowUp
        } else {
            StreamingBehavior::Steer
        };
        if core.activity() == Activity::Prompt {
            core.queue(behavior, user_message(text, Vec::new()));
            return;
        }
        let (text, extension) = (text.to_string(), self.extension.clone());
        tokio::spawn(async move {
            let options = PromptOptions {
                streaming_behavior: Some(behavior),
                expand: false,
                source: InputSource::Extension,
                ..PromptOptions::default()
            };
            if let Err(error) = core.clone().prompt(&text, options).await {
                core.runner.report(&extension, "send_user_message", error);
            }
        });
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
