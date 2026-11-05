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
    QueueMode, ToolError, TurnContext, TurnDecision, TurnUpdate,
};
use pi_ai::transcript::current_system_message;
use pi_ai::{
    AssistantMessage, AssistantMessageStream, Content, Context, ImageContent, Message, Model,
    ProviderResponse, StreamFn, StreamOptions, SystemMessage, ThinkingLevel, UserMessage,
    content_text, is_context_overflow, is_recoverable_length, is_retryable_error, now_ms,
    retry_delay_ms,
};
use serde_json::Value;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::bash_executor::{BashResult, OnChunk, execute_bash_with_operations_in};
use crate::compaction::{
    CompactionPreparation, CompactionResult, Summarizer, SummaryRetry, compact,
    estimate_projected_context_tokens, estimate_tokens, prepare_compaction, should_compact,
    summarize_branch,
};
use crate::extensions::{
    ActivityOutcome, AfterProviderResponse, AgentBeforeSettle, AgentEventSeen, AgentSettled,
    AutocompleteItem, BeforeAgentStart, BeforeForkResult, BeforeProviderHeaders,
    BeforeProviderRequest, BeforeSwitchResult, BoundaryContext, BoundaryDraft, CommandContext,
    CustomMessageDraft, Extension, ExtensionContext, ExtensionErrorReport, ExtensionRunner,
    ExtensionUi, ForkPosition, Input, InputAction, InputSource, MessageEnd, ModelSelect,
    ProviderStreamEvent, RegisteredTool, ResourcesDiscover, ResourcesDiscoverReason,
    ResourcesDiscoverResult, RunTrigger, SessionBeforeCompact, SessionBeforeFork,
    SessionBeforeSwitch, SessionBeforeTree, SessionCompact, SessionCompactFailed,
    SessionInfoChanged, SessionShutdown, SessionStart, SessionStartReason, SessionTree,
    SlashCommandInfo, SlashCommandSource, ThinkingLevelSelect, ToolCall, ToolInfo, ToolPrompt,
    ToolResult, TreePreparation, TreeSummary, TurnEnd, UserBash, UserBashResult,
};
use crate::messages::{BashExecutionMessage, CustomMessage, SessionMessage, convert_to_llm};
use crate::models::ModelRegistry;
use crate::resources::{Resources, expand_prompt_template, expand_skill_command};
use crate::runtime::{ForkOptions, NewSessionOptions, SessionActions, SwitchSessionOptions};
use crate::session::{EntryKind, SessionEntry, SessionError, SessionManager, file_timestamp};
use crate::settings::Settings;
use crate::store::SessionStore;
use crate::system_prompt::{
    SystemPromptOptions, ToolPromptInfo, build_prompt_state, diff_sections, render,
};
use crate::tools::{
    BashOperations, DEFAULT_TOOL_NAMES, LocalShellOperations, ToolContext, ToolsOptions,
    create_all_tools, expand_path, normalize_prompt_images, normalize_tool_result_images,
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
    /// A stored session's working directory is gone, as Pi's `MissingSessionCwdError`;
    /// the host can ask to continue elsewhere with a `cwd_override`.
    MissingSessionCwd(SessionCwdIssue),
    /// The file to import is not there, as Pi's `SessionImportFileNotFoundError`.
    ImportFileNotFound(PathBuf),
    Failed(String),
}

/// A stored session whose working directory no longer exists, as Pi's
/// `SessionCwdIssue`.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionCwdIssue {
    pub session_file: PathBuf,
    pub session_cwd: String,
    /// Where the host is now, which it can continue in instead.
    pub fallback_cwd: PathBuf,
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
            Self::MissingSessionCwd(issue) => write!(
                formatter,
                "Stored session working directory does not exist: {}\nSession file: {}\nCurrent working directory: {}",
                issue.session_cwd,
                issue.session_file.display(),
                issue.fallback_cwd.display()
            ),
            Self::ImportFileNotFound(path) => {
                write!(formatter, "File not found: {}", path.display())
            }
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

/// The thinking level a session starts at when neither its record nor the settings name
/// one, as in Pi.
pub const DEFAULT_THINKING_LEVEL: ThinkingLevel = ThinkingLevel::Medium;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompactionReason {
    Manual,
    Threshold,
    Overflow,
}

/// What a session reports to its host.
#[derive(Clone, Debug)]
pub enum AgentSessionEvent {
    /// The agent's events, except the end of its run, which is [`Self::AgentEnd`].
    Agent(AgentEvent<SessionMessage>),
    /// The agent's run ended, with the messages it added. As in Pi, it says whether a
    /// retry follows.
    AgentEnd {
        messages: Vec<SessionMessage>,
        will_retry: bool,
    },
    QueueUpdate {
        steering: Vec<String>,
        follow_up: Vec<String>,
    },
    CompactionStart {
        reason: CompactionReason,
    },
    /// As Pi's `compaction_end`: `aborted` when it was stopped or an extension cancelled
    /// it, which leaves `error` empty, and `will_retry` when the interrupted reply is asked
    /// again.
    CompactionEnd {
        reason: CompactionReason,
        result: Option<CompactionResult>,
        aborted: bool,
        will_retry: bool,
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
    /// A summary request failed for a moment and goes out again after `delay_ms`, as
    /// Pi's `summarization_retry_scheduled`.
    SummarizationRetryScheduled {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error: String,
    },
    /// The retried summary request goes out.
    SummarizationRetryAttemptStart {
        source: SummarySource,
    },
    /// The summary request's retries are over.
    SummarizationRetryFinished,
    /// The thinking level changed, also when a new model needed another one.
    ThinkingLevelChanged {
        level: ThinkingLevel,
    },
    /// An entry outside the conversation's flow was added: an extension's own entry, or
    /// the edit that takes a failed attempt out of the context, as Pi reports them.
    EntryAppended {
        entry: SessionEntry,
    },
    /// The session's name changed; `None` when it was cleared.
    SessionInfoChanged {
        name: Option<String>,
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
    /// The run, its retries and compactions are over. `aborted` when it was stopped.
    Settled {
        aborted: bool,
    },
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

/// What a summary request is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SummarySource {
    Compaction(CompactionReason),
    BranchSummary,
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

/// How [`AgentSession::navigate_tree`] moves, as Pi's `navigateTree` options say.
#[derive(Clone, Debug, Default)]
pub struct TreeNavigation {
    /// Summarize the branch being left.
    pub summarize: bool,
    /// Instructions for that summary, added to the default ones.
    pub custom_instructions: Option<String>,
    /// The custom instructions replace the default ones instead.
    pub replace_instructions: bool,
    /// A label for the summary, or for the target when there is none.
    pub label: Option<String>,
}

/// Where a move in the tree left the session.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TreeMove {
    /// The text of the user or custom message moved to, for the editor: the session now
    /// stands before it, so sending it again starts another branch there.
    pub editor_text: Option<String>,
    /// The summary of the branch left, when one was written.
    pub summary_entry: Option<SessionEntry>,
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

/// How full the model's context window is, as Pi's `getContextUsage` reports it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ContextUsage {
    /// The estimated context size. After a compaction it is unknown until a reply
    /// measures the context again, as in Pi.
    pub tokens: Option<u64>,
    pub context_window: u64,
    pub percent: Option<f64>,
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
    /// Whether the project's own resources and settings were loaded, which extensions
    /// can ask, as Pi's `isProjectTrusted`.
    pub project_trusted: bool,
    /// The settings file the session saves what its setters change to, as Pi saves the
    /// user's settings file. Without one, changes stay in the session.
    pub settings_file: Option<PathBuf>,
    /// What extensions hear as the session starts, as Pi's `sessionStartEvent`: a
    /// startup unless the host says the session replaces another.
    pub session_start: SessionStart,
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
            project_trusted: false,
            settings_file: None,
            session_start: SessionStart::default(),
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
                aborted: true,
                will_retry: false,
                error: None,
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
    /// The prompt options a `before_agent_start` handler chose for the current run.
    prompt_options: Option<SystemPromptOptions>,
    next_turn_messages: Vec<SessionMessage>,
    /// Extension messages waiting for the current turn to end.
    pending_custom: Vec<SessionMessage>,
    /// User shell commands that ended during the run, added when it ends.
    pending_bash: Vec<SessionMessage>,
    steering_texts: Vec<String>,
    follow_up_texts: Vec<String>,
    /// How the latest turn went, which `agent_before_settle` reports.
    outcome: ActivityOutcome,
    /// The turns of the agent's current run so far, which `turn_end` counts.
    turn_index: usize,
    /// Entries a `turn_end` handler added, which the next turn's context must hold.
    boundary_changed: bool,
}

pub(crate) struct SessionCore {
    agent: Agent<SessionMessage>,
    session: Mutex<SessionManager>,
    runner: ExtensionRunner,
    models: ModelRegistry,
    ui: Arc<dyn ExtensionUi>,
    cwd: PathBuf,
    /// The host's task-owned overflow directory, also used by user `!` commands.
    output_dir: Option<PathBuf>,
    settings: Mutex<Settings>,
    settings_file: Option<PathBuf>,
    resources: Mutex<Resources>,
    /// The skill and template paths extensions gave in `resources_discover`.
    discovered: Mutex<ResourcesDiscoverResult>,
    host_prompt: HostPrompt,
    prompt_base: Mutex<SystemPromptOptions>,
    model: Mutex<Option<Model>>,
    tools: Mutex<Vec<RegisteredTool>>,
    active_tools: Mutex<Vec<String>>,
    listeners: Mutex<Vec<(u64, Listener)>>,
    next_listener: AtomicU64,
    activity: watch::Sender<Activity>,
    project_trusted: bool,
    run: Mutex<RunState>,
    /// User shell commands running now, by a key of their own.
    bash_runs: Mutex<Vec<(u64, CancellationToken)>>,
    next_bash: AtomicU64,
    /// What the session's commands replace it with: its runtime, when one owns it.
    actions: Mutex<Option<Weak<dyn SessionActions>>>,
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

/// Set each path in the JSON settings `file` to its value, keeping the rest of it, and
/// write it back as Pi writes settings: indented by two spaces. A file that does not
/// parse is left alone, as Pi does.
fn write_settings(file: &Path, changes: Vec<(&[&str], Value)>) -> std::io::Result<()> {
    let mut settings = match std::fs::read_to_string(file) {
        Ok(text) if !text.trim().is_empty() => {
            serde_json::from_str(text.strip_prefix('\u{feff}').unwrap_or(&text))
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?
        }
        Ok(_) => Value::Object(Default::default()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Value::Object(Default::default())
        }
        Err(error) => return Err(error),
    };
    for (path, value) in changes {
        let Some((last, parents)) = path.split_last() else {
            continue;
        };
        let mut current = &mut settings;
        for key in parents {
            if !current.is_object() {
                *current = Value::Object(Default::default());
            }
            let child = &mut current[*key];
            if !child.is_object() {
                *child = Value::Object(Default::default());
            }
            current = child;
        }
        if !current.is_object() {
            *current = Value::Object(Default::default());
        }
        current[*last] = value;
    }
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let staging = file.with_extension("json.tmp");
    std::fs::write(&staging, serde_json::to_string_pretty(&settings)?)?;
    std::fs::rename(&staging, file)
}

/// What an image becomes when images are blocked, as in Pi.
const IMAGES_BLOCKED: &str = "Image reading is disabled.";

/// `messages` with each image in a user message or a tool result replaced by a note,
/// one note for images in a row, as Pi sends them with `images.blockImages`.
fn block_images(messages: Vec<Message>) -> Vec<Message> {
    let without_images = |content: Vec<Content>| {
        if !content
            .iter()
            .any(|block| matches!(block, Content::Image(_)))
        {
            return content;
        }
        let mut kept: Vec<Content> = Vec::new();
        for block in content {
            let block = match block {
                Content::Image(_) => Content::text(IMAGES_BLOCKED),
                other => other,
            };
            let note = |block: &Content| matches!(block, Content::Text(text) if text.text == IMAGES_BLOCKED);
            if !(note(&block) && kept.last().is_some_and(note)) {
                kept.push(block);
            }
        }
        kept
    };
    messages
        .into_iter()
        .map(|message| match message {
            Message::User(mut user) => {
                user.content = without_images(user.content);
                Message::User(user)
            }
            Message::ToolResult(mut result) => {
                result.content = without_images(result.content);
                Message::ToolResult(result)
            }
            other => other,
        })
        .collect()
}

/// The session's own requests go out through this, as Pi's session stream function
/// sends them, with [`SessionCore::request_options`]. Compaction and summaries go to
/// the models directly, as in Pi.
struct SessionStream {
    core: Weak<SessionCore>,
    inner: Arc<dyn StreamFn>,
}

impl StreamFn for SessionStream {
    fn stream(
        &self,
        model: &Model,
        context: Context,
        mut options: StreamOptions,
    ) -> AssistantMessageStream {
        if let Some(core) = self.core.upgrade() {
            core.request_options(model, &mut options);
        }
        self.inner.stream(model, context, options)
    }
}

/// Add `drafts` to `session` as Pi's `_applyBoundaryDrafts` does, and the entries they
/// made; an error when one does not apply, such as an edit of an entry the branch
/// does not hold.
fn apply_drafts(
    session: &mut SessionManager,
    drafts: &[BoundaryDraft],
) -> Result<Vec<SessionEntry>, String> {
    let mut appended = Vec::new();
    for draft in drafts.iter().cloned() {
        let id = match draft {
            BoundaryDraft::Custom { custom_type, data } => {
                session.append_custom_entry(&custom_type, data)
            }
            BoundaryDraft::CustomMessage {
                custom_type,
                content,
                display,
                details,
            } => session.append_custom_message(&custom_type, content, display, details),
            BoundaryDraft::ContextEdit {
                target_id,
                replacement,
            } => session
                .append_context_edit(&target_id, replacement)
                .map_err(|error| error.to_string())?,
            BoundaryDraft::Compaction {
                summary,
                first_kept_entry_id,
                details,
                usage,
            } => {
                let tokens_before =
                    estimate_projected_context_tokens(&session.projection(), &session.branch());
                session.append_compaction(
                    summary,
                    first_kept_entry_id,
                    tokens_before,
                    details,
                    usage,
                    true,
                )
            }
        };
        appended.extend(session.entry(&id).cloned());
    }
    Ok(appended)
}

/// Where a fork at `entry_id` ends and the text it gives back to edit, as Pi's `fork`
/// decides: at an entry, up to and including it; before a user message, up to it, with
/// its text.
pub(crate) fn fork_target(
    session: &SessionManager,
    entry_id: &str,
    position: ForkPosition,
) -> Result<(Option<String>, Option<String>), AgentSessionError> {
    let entry = session
        .entry(entry_id)
        .ok_or_else(|| SessionError::NotFound(entry_id.into()))?;
    match (position, &entry.kind) {
        (ForkPosition::At, _) => Ok((Some(entry.id.clone()), None)),
        (
            ForkPosition::Before,
            EntryKind::Message {
                message: SessionMessage::Llm(Message::User(user)),
            },
        ) => Ok((entry.parent_id.clone(), Some(content_text(&user.content)))),
        (ForkPosition::Before, _) => Err(AgentSessionError::Failed(
            "Only a user message can be forked before".into(),
        )),
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
        headers: Default::default(),
        sampling_params: Default::default(),
        sampling_params_by_thinking_level: Default::default(),
        compat: Default::default(),
    }
}

/// The model and thinking level a session set up as Pi's `createAgentSession` asks to
/// start with: [`crate::sdk::create_agent_session`].
pub(crate) struct SetupChoice {
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
}

impl AgentSession {
    pub async fn new(options: AgentSessionOptions) -> Result<Self, AgentSessionError> {
        Ok(Self::start(options, None).await?.0)
    }

    /// Build the session. It starts with the model it recorded, when the registry has
    /// it, or else the host's; with `setup`, as Pi's `createAgentSession` chooses, which
    /// says why the session did not start with the model it recorded.
    pub(crate) async fn start(
        mut options: AgentSessionOptions,
        setup: Option<SetupChoice>,
    ) -> Result<(Self, Option<String>), AgentSessionError> {
        let session_start = std::mem::take(&mut options.session_start);
        let projection = options.session.projection();
        let has_messages = !projection.messages.is_empty();
        let has_thinking_entry = options.session.has_context_base()
            || options
                .session
                .branch()
                .iter()
                .any(|entry| matches!(entry.kind, EntryKind::ThinkingLevelChange { .. }));
        let models = options.models.clone();
        let model = projection
            .model
            .as_ref()
            .and_then(|(provider, id)| models.find(provider, id))
            .or(options.model.clone());
        // As Pi restores a session: one with a conversation keeps the level it recorded;
        // a new one starts at its model's own level, and anything else at the settings'
        // default level, or else at Pi's.
        let model_level = model
            .as_ref()
            .filter(|_| !has_messages)
            .and_then(|model| options.settings.model_thinking_level(model));
        let requested_thinking_level = if has_messages && has_thinking_entry {
            projection.thinking_level
        } else {
            model_level
                .or(options.settings.default_thinking_level)
                .unwrap_or(DEFAULT_THINKING_LEVEL)
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

        let core = Arc::new_cyclic(|weak: &Weak<SessionCore>| {
            let runner = ExtensionRunner::load(&options.extensions, weak.clone());
            let stream = SessionStream {
                core: weak.clone(),
                inner: models.stream_fn(),
            };
            let mut agent_options = AgentOptions::new(
                model.clone().unwrap_or_else(placeholder_model),
                Arc::new(stream),
            );
            agent_options.thinking_level = thinking_level;
            agent_options.messages = projection.messages.clone();
            agent_options.tool_execution = options.settings.tool_execution;
            agent_options.steering_mode = options.settings.steering_mode;
            agent_options.follow_up_mode = options.settings.follow_up_mode;
            agent_options.hooks = Arc::new(SessionHooks { core: weak.clone() });
            let context = ExtensionContext {
                core: weak.clone(),
                extension: Arc::from("builtin"),
            };
            let mut tool_options = options.tool_options;
            let output_dir = tool_options
                .bash
                .output_dir
                .clone()
                .or_else(|| tool_options.output_dir.clone());
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
                output_dir,
                settings: Mutex::new(options.settings),
                settings_file: options.settings_file,
                resources: Mutex::new(options.resources),
                discovered: Mutex::new(ResourcesDiscoverResult::default()),
                host_prompt,
                prompt_base: Mutex::new(prompt_base),
                model: Mutex::new(model),
                tools: Mutex::new(tools),
                active_tools: Mutex::new(active),
                listeners: Mutex::new(Vec::new()),
                next_listener: AtomicU64::new(1),
                activity: watch::channel(Activity::Idle).0,
                project_trusted: options.project_trusted,
                run: Mutex::new(RunState::default()),
                bash_runs: Mutex::new(Vec::new()),
                next_bash: AtomicU64::new(1),
                actions: Mutex::new(None),
                weak: weak.clone(),
            }
        });

        for provider in core.runner.providers() {
            models.register_models(provider.models.clone());
            if let Some((api, implementation)) = &provider.api {
                models.register_api(api, implementation.clone());
            }
        }
        for classifiers in core.runner.classifiers() {
            models.register_classifier_models(classifiers.models.clone());
            if let Some((api, implementation)) = &classifiers.api {
                models.register_classifier_api(api, implementation.clone());
            }
        }
        // A provider an extension registers can serve the session's model, so the model
        // is chosen again now that they are registered.
        let model_fallback_message = match setup {
            None => {
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
                None
            }
            Some(SetupChoice {
                model,
                thinking_level,
            }) => {
                let settings = lock(&core.settings).clone();
                let chosen = crate::sdk::choose_model(
                    &models,
                    &settings,
                    &projection,
                    has_thinking_entry,
                    model,
                    thinking_level,
                )
                .await;
                core.agent
                    .set_model(chosen.model.clone().unwrap_or_else(placeholder_model));
                core.agent.set_thinking_level(chosen.thinking_level);
                *lock(&core.model) = chosen.model;
                chosen.fallback_message
            }
        };
        {
            // Recorded so a resumed session starts with the same model and level, as in Pi:
            // a new session records both, an older one the level it now has.
            let thinking_level = core.agent.thinking_level();
            let mut session = lock(&core.session);
            if !has_messages {
                if let Some(model) = lock(&core.model).as_ref() {
                    session.append_model_change(&model.provider, &model.id);
                }
                session.append_thinking_level_change(thinking_level);
            } else if !has_thinking_entry {
                session.append_thinking_level_change(thinking_level);
            }
        }
        core.report_persistence();
        let sink = core.weak.clone();
        core.runner.set_error_sink(Arc::new(move |report| {
            if let Some(core) = sink.upgrade() {
                core.emit(AgentSessionEvent::ExtensionError(report));
            }
        }));
        core.agent.subscribe(Arc::new(SessionListener {
            core: core.weak.clone(),
        }));
        core.sync_tools();
        // As in Pi, resources are discovered again after a reload, and as at startup
        // for any other start.
        let discover_reason = match session_start.reason {
            SessionStartReason::Reload => ResourcesDiscoverReason::Reload,
            _ => ResourcesDiscoverReason::Startup,
        };
        core.runner.emit(&session_start, &core.weak).await;
        core.discover_resources(discover_reason).await;
        Ok((Self { core }, model_fallback_message))
    }

    /// Ask extensions about a shell command the user runs, before it runs, as Pi's hosts
    /// ask with `user_bash`: the first answer wins. `Err` is a failing handler's error,
    /// reported already; the command should then not run.
    pub async fn user_bash(
        &self,
        command: &str,
        exclude_from_context: bool,
    ) -> Result<Option<UserBashResult>, String> {
        let event = UserBash {
            command: command.to_string(),
            exclude_from_context,
            cwd: self.core.cwd.clone(),
        };
        self.core.runner.user_bash(event, &self.core.weak).await
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
    /// out of the transcript the user sees but reaches the model. The run starts as a
    /// prompt's does, with `before_agent_start` and the next-turn messages. During a
    /// prompt it is queued as steering instead.
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
        core.start_run(activity, RunTrigger::Custom(custom(draft)))
            .await?;
        Ok(PromptOutcome::Completed)
    }

    /// Deliver `text` and `images` after the current turn's tool calls. As in Pi's
    /// `steer`, input handlers, skills and templates apply first, and an extension
    /// command is refused, since it cannot wait in the queue.
    pub async fn steer(
        &self,
        text: &str,
        images: Vec<ImageContent>,
    ) -> Result<PromptOutcome, AgentSessionError> {
        self.core
            .queue_input(text, images, StreamingBehavior::Steer)
            .await
    }

    /// Deliver `text` and `images` once the agent would otherwise stop, after the same
    /// steps as [`Self::steer`], as Pi's `followUp` does.
    pub async fn follow_up(
        &self,
        text: &str,
        images: Vec<ImageContent>,
    ) -> Result<PromptOutcome, AgentSessionError> {
        self.core
            .queue_input(text, images, StreamingBehavior::FollowUp)
            .await
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

    /// Compact the current branch. As in Pi, whatever the session is doing is stopped
    /// first, and a stopped run is not continued.
    pub async fn compact(
        &self,
        custom_instructions: Option<&str>,
    ) -> Result<CompactionResult, AgentSessionError> {
        let _maintenance = loop {
            if let Some(maintenance) = self.core.begin(Activity::Maintenance) {
                break maintenance;
            }
            self.core.abort();
            self.wait_for_idle().await;
        };
        self.core
            .clone()
            .compact(CompactionReason::Manual, custom_instructions, false)
            .await
    }

    /// Move to another point of the tree, as Pi's `navigateTree` does: optionally
    /// summarizing the branch being left, and standing before a user or custom message
    /// moved to, whose text comes back to be edited. The target branch's tools become
    /// the active ones. Extensions can cancel the move or change how it goes.
    pub async fn navigate_tree(
        &self,
        target_id: &str,
        options: TreeNavigation,
    ) -> Result<TreeMove, AgentSessionError> {
        let _maintenance = self
            .core
            .begin(Activity::Maintenance)
            .ok_or(AgentSessionError::Busy)?;
        self.core.clone().navigate_tree(target_id, options).await
    }

    /// Continue in a new session holding only the path to `entry_id`, as Pi's `fork` does.
    ///
    /// As in Pi, a fork before a user message holds the path up to it and gives back its
    /// text to edit, and a fork at an entry holds the path up to and including it.
    /// Extensions can cancel it; a run in progress is stopped first. The session goes on
    /// in this same object, its extensions with it, so they hear of a fork as a
    /// [`SessionStart`] with no shutdown before. An
    /// [`AgentSessionRuntime`](crate::runtime::AgentSessionRuntime) forks as Pi does
    /// instead, replacing the session.
    pub async fn fork(
        &self,
        entry_id: &str,
        position: ForkPosition,
        store: Box<dyn SessionStore>,
    ) -> Result<Option<String>, AgentSessionError> {
        self.fork_here(entry_id, position, |session, leaf| {
            session.fork(leaf, store)
        })
        .await
    }

    /// [`Self::fork`], with `fork` turning the session into the fork.
    pub(crate) async fn fork_here(
        &self,
        entry_id: &str,
        position: ForkPosition,
        fork: impl FnOnce(&mut SessionManager, Option<&str>) -> Result<(), SessionError>,
    ) -> Result<Option<String>, AgentSessionError> {
        let before = self
            .before_fork(SessionBeforeFork {
                entry_id: entry_id.to_string(),
                position,
            })
            .await;
        if before.cancel {
            return Err(AgentSessionError::Cancelled);
        }
        let _maintenance = loop {
            if let Some(maintenance) = self.core.begin(Activity::Maintenance) {
                break maintenance;
            }
            self.core.abort();
            self.wait_for_idle().await;
        };
        let (previous_session_file, edited_text) = {
            let mut session = lock(&self.core.session);
            let (leaf, edited_text) = fork_target(&session, entry_id, position)?;
            let previous = session.session_file().map(Path::to_path_buf);
            fork(&mut session, leaf.as_deref())?;
            (previous, edited_text)
        };
        self.core.report_persistence();
        self.core.rebuild_messages();
        self.core
            .runner
            .emit(
                &SessionStart {
                    reason: SessionStartReason::Fork,
                    previous_session_file,
                },
                &self.core.weak,
            )
            .await;
        Ok(edited_text)
    }

    pub(crate) async fn before_fork(&self, event: SessionBeforeFork) -> BeforeForkResult {
        self.core.runner.before_fork(event, &self.core.weak).await
    }

    pub(crate) async fn before_switch(&self, event: SessionBeforeSwitch) -> BeforeSwitchResult {
        self.core.runner.before_switch(event, &self.core.weak).await
    }

    /// Let the session's commands replace it through `actions`.
    pub(crate) fn bind_actions(&self, actions: Weak<dyn SessionActions>) {
        *lock(&self.core.actions) = Some(actions);
    }

    pub(crate) fn unbind_actions(&self) {
        *lock(&self.core.actions) = None;
    }

    /// Change the session tree with `setup` and go on from the result, as Pi's
    /// `newSession` runs its `setup` and refreshes the context.
    pub(crate) fn setup_session(&self, setup: impl FnOnce(&mut SessionManager)) {
        setup(&mut lock(&self.core.session));
        self.core.report_persistence();
        self.core.rebuild_messages();
    }

    /// The file the session is kept in, when its store keeps one, as Pi's `sessionFile`.
    pub fn session_file(&self) -> Option<PathBuf> {
        lock(&self.core.session)
            .session_file()
            .map(Path::to_path_buf)
    }

    pub async fn set_model(&self, model: Model) {
        self.core.set_model(model).await;
    }

    pub async fn set_thinking_level(&self, level: ThinkingLevel) {
        self.core.set_thinking_level(level).await;
    }

    /// Switch to `model` and make it the model new sessions start with, as Pi's
    /// `setModel` with `persist`: saved as `defaultProvider` and `defaultModel`.
    pub async fn set_model_as_default(&self, model: Model) {
        let (provider, id) = (model.provider.clone(), model.id.clone());
        self.core.set_model(model).await;
        {
            let mut settings = lock(&self.core.settings);
            settings.default_provider = Some(provider.clone());
            settings.default_model = Some(id.clone());
        }
        self.core.save_settings(vec![
            (&["defaultProvider"], Value::String(provider)),
            (&["defaultModel"], Value::String(id)),
        ]);
    }

    /// Set the thinking level and make the requested level the default, as Pi's
    /// `setThinkingLevel` with `persist`: saved as `defaultThinkingLevel`.
    pub async fn set_thinking_level_as_default(&self, level: ThinkingLevel) {
        self.core.set_thinking_level(level).await;
        lock(&self.core.settings).default_thinking_level = Some(level);
        self.core.save_setting(
            &["defaultThinkingLevel"],
            serde_json::to_value(level).unwrap_or_default(),
        );
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

    /// Name the session, as Pi's `setSessionName` does.
    pub async fn set_session_name(&self, name: &str) {
        self.core.set_session_name(name).await;
    }

    /// The session's latest name, as Pi's `getSessionName` gives it.
    pub fn session_name(&self) -> Option<String> {
        lock(&self.core.session).session_name().map(str::to_string)
    }

    /// Read the session tree.
    pub fn with_session<T>(&self, read: impl FnOnce(&SessionManager) -> T) -> T {
        read(&lock(&self.core.session))
    }

    /// Retry pending source-history writes before the host unloads this session.
    /// A failure keeps the tree in memory and must prevent eviction or shutdown.
    pub fn flush(&self) -> std::io::Result<()> {
        lock(&self.core.session).flush()
    }

    pub fn has_unpersisted(&self) -> bool {
        lock(&self.core.session).has_unpersisted()
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

    /// Every registered tool, as Pi's `getAllTools` describes it.
    pub fn all_tools(&self) -> Vec<ToolInfo> {
        self.core.all_tools()
    }

    /// What a prompt starting with `/` runs, as Pi's `getCommands` lists it: extension
    /// commands, prompt templates, and skills as `skill:<name>`.
    pub fn commands(&self) -> Vec<SlashCommandInfo> {
        self.core.commands()
    }

    /// The completions an extension command offers for what was typed after it, as
    /// Pi's `getArgumentCompletions`; `None` when it offers none.
    pub async fn command_argument_completions(
        &self,
        name: &str,
        prefix: &str,
    ) -> Option<Vec<AutocompleteItem>> {
        self.core.runner.argument_completions(name, prefix).await
    }

    /// Write the current branch to a session file of its own, as Pi's `exportToJsonl`
    /// does, and its path. Without `path`, it is `session-<time>.jsonl`; a relative path
    /// is relative to the session's folder, where Pi takes its process's.
    pub fn export_to_jsonl(&self, path: Option<&Path>) -> std::io::Result<PathBuf> {
        let name = format!("session-{}.jsonl", file_timestamp(now_ms()));
        let path = self.core.cwd.join(path.unwrap_or(Path::new(&name)));
        lock(&self.core.session).export_branch(&path)?;
        Ok(path)
    }

    pub fn resources(&self) -> Resources {
        lock(&self.core.resources).clone()
    }

    /// Use `resources` from the next prompt on, as Pi's `reload` does with what its
    /// resource loader finds again: context files, skills and prompt templates, and
    /// `SYSTEM.md` and `APPEND_SYSTEM.md` where the host gave no text of its own. The
    /// transcript's prompt is brought up to date by the sections that changed.
    pub fn set_resources(&self, mut resources: Resources) {
        // What extensions discovered stays.
        let discovered = lock(&self.core.discovered).clone();
        resources.extend_from_paths(
            &self.core.cwd,
            &discovered.skill_paths,
            &discovered.prompt_paths,
        );
        self.core
            .host_prompt
            .apply(&mut lock(&self.core.prompt_base), &resources);
        *lock(&self.core.resources) = resources;
    }

    pub fn settings(&self) -> Settings {
        lock(&self.core.settings).clone()
    }

    /// Turn automatic compaction on or off for this session, as Pi's
    /// `setAutoCompactionEnabled` does. The settings files are the host's to change.
    pub fn set_auto_compaction_enabled(&self, enabled: bool) {
        lock(&self.core.settings).compaction.enabled = enabled;
        self.core
            .save_setting(&["compaction", "enabled"], Value::Bool(enabled));
    }

    pub fn auto_compaction_enabled(&self) -> bool {
        lock(&self.core.settings).compaction.enabled
    }

    /// Turn automatic retries on or off for this session, as Pi's `setAutoRetryEnabled`
    /// does.
    pub fn set_auto_retry_enabled(&self, enabled: bool) {
        lock(&self.core.settings).retry.enabled = enabled;
        self.core
            .save_setting(&["retry", "enabled"], Value::Bool(enabled));
    }

    pub fn auto_retry_enabled(&self) -> bool {
        lock(&self.core.settings).retry.enabled
    }

    /// Deliver steering all at once or one message per turn, as Pi's `setSteeringMode`
    /// sets it.
    pub fn set_steering_mode(&self, mode: QueueMode) {
        lock(&self.core.settings).steering_mode = mode;
        self.core.agent.set_steering_mode(mode);
        self.core.save_setting(
            &["steeringMode"],
            serde_json::to_value(mode).unwrap_or_default(),
        );
    }

    pub fn steering_mode(&self) -> QueueMode {
        lock(&self.core.settings).steering_mode
    }

    /// Deliver follow-ups all at once or one per turn, as Pi's `setFollowUpMode` sets it.
    pub fn set_follow_up_mode(&self, mode: QueueMode) {
        lock(&self.core.settings).follow_up_mode = mode;
        self.core.agent.set_follow_up_mode(mode);
        self.core.save_setting(
            &["followUpMode"],
            serde_json::to_value(mode).unwrap_or_default(),
        );
    }

    pub fn follow_up_mode(&self) -> QueueMode {
        lock(&self.core.settings).follow_up_mode
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

    /// The context command handlers get, as Pi's `createReplacedSessionContext` gives a
    /// session that replaced another.
    pub fn command_context(&self) -> CommandContext {
        CommandContext::new(self.extension_context())
    }

    /// Tell extensions the session is ending because the host is done with it.
    pub async fn shutdown(&self) {
        self.shutdown_with(SessionShutdown::default()).await;
    }

    /// Tell extensions the session is ending, and why, as Pi's `session_shutdown`. A
    /// run, retry, compaction or shell command in progress is stopped first.
    pub async fn shutdown_with(&self, event: SessionShutdown) {
        self.abort_bash();
        self.core.abort();
        self.wait_for_idle().await;
        self.core.runner.emit(&event, &self.core.weak).await;
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
        let result = execute_bash_with_operations_in(
            &resolved,
            &core.cwd,
            operations.as_ref(),
            &file_prefix,
            core.output_dir.as_deref(),
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

/// Summary requests retry as runs do, and not at all with retries off, as in Pi.
fn summary_retries(settings: &Settings) -> u32 {
    if settings.retry.enabled {
        settings.retry.max_retries
    } else {
        0
    }
}

/// Whether `message` is a reply whose usage measured the context, as Pi counts one.
fn measures_context(message: &SessionMessage) -> bool {
    matches!(
        message,
        SessionMessage::Llm(Message::Assistant(assistant))
            if assistant.stop_reason != pi_ai::StopReason::Aborted
                && assistant.stop_reason != pi_ai::StopReason::Error
                && assistant.usage.context_tokens() > 0
    )
}

fn user_message(text: &str, images: Vec<ImageContent>) -> SessionMessage {
    let mut content = vec![Content::text(text)];
    content.extend(images.into_iter().map(Content::Image));
    SessionMessage::Llm(Message::User(UserMessage {
        content,
        timestamp: now_ms(),
    }))
}

fn custom(draft: CustomMessageDraft) -> CustomMessage {
    CustomMessage {
        custom_type: draft.custom_type,
        content: draft.content,
        display: draft.display,
        details: draft.details,
        timestamp: now_ms(),
    }
}

fn custom_message(draft: CustomMessageDraft) -> SessionMessage {
    SessionMessage::Custom(custom(draft))
}

impl SessionCore {
    /// Add the skills and templates extensions name in `resources_discover`, after the
    /// session's own, as Pi's `extendResourcesFromExtensions` does, and update the
    /// prompt.
    async fn discover_resources(&self, reason: ResourcesDiscoverReason) {
        if !self.runner.has::<ResourcesDiscover>() {
            return;
        }
        let event = ResourcesDiscover {
            cwd: self.cwd.clone(),
            reason,
        };
        let answers = self.runner.emit(&event, &self.weak).await;
        let mut found = ResourcesDiscoverResult::default();
        for answer in answers {
            found.skill_paths.extend(answer.skill_paths);
            found.prompt_paths.extend(answer.prompt_paths);
        }
        if found.skill_paths.is_empty() && found.prompt_paths.is_empty() {
            return;
        }
        let mut resources = lock(&self.resources);
        resources.extend_from_paths(&self.cwd, &found.skill_paths, &found.prompt_paths);
        self.host_prompt
            .apply(&mut lock(&self.prompt_base), &resources);
        drop(resources);
        let mut discovered = lock(&self.discovered);
        discovered.skill_paths.extend(found.skill_paths);
        discovered.prompt_paths.extend(found.prompt_paths);
    }

    /// The options of the session's own request to `model`, as Pi's session gives them:
    /// the session's id, the provider settings (`buildRequestOptions`), and hooks for
    /// the provider events extensions handle. They are read as the request goes out.
    fn request_options(&self, model: &Model, options: &mut StreamOptions) {
        if options.session_id.is_none() {
            options.session_id = Some(self.session_id());
        }
        let settings = lock(&self.settings).clone();
        let provider = &settings.retry.provider;
        options.timeout_ms = options.timeout_ms.or(Some(settings.request_timeout_ms()));
        options.max_retries = options.max_retries.or(provider.max_retries);
        options.max_retry_delay_ms = options
            .max_retry_delay_ms
            .or(Some(provider.max_retry_delay_ms));
        if self.runner.has::<BeforeProviderHeaders>() {
            let weak = self.weak.clone();
            options.transform_headers = Some(Arc::new(move |headers| {
                let weak = weak.clone();
                async move {
                    match weak.upgrade() {
                        Some(core) => core.runner.provider_headers(headers, &weak).await,
                        None => headers,
                    }
                }
                .boxed()
            }));
        }
        if self.runner.has::<BeforeProviderRequest>() {
            let weak = self.weak.clone();
            options.on_payload = Some(Arc::new(move |payload: Value| {
                let weak = weak.clone();
                async move {
                    match weak.upgrade() {
                        Some(core) => core.runner.provider_payload(payload, &weak).await,
                        None => payload,
                    }
                }
                .boxed()
            }));
        }
        if self.runner.has::<AfterProviderResponse>() {
            let weak = self.weak.clone();
            options.on_response = Some(Arc::new(move |response: ProviderResponse| {
                let weak = weak.clone();
                async move {
                    if let Some(core) = weak.upgrade() {
                        let event = AfterProviderResponse {
                            status: response.status,
                            headers: response.headers,
                        };
                        core.runner.emit(&event, &weak).await;
                    }
                }
                .boxed()
            }));
        }
        if self.runner.has::<ProviderStreamEvent>() {
            let weak = self.weak.clone();
            let (provider, api, id) = (model.provider.clone(), model.api.clone(), model.id.clone());
            options.on_provider_stream_event = Some(Arc::new(move |data: Value| {
                let weak = weak.clone();
                let event = ProviderStreamEvent {
                    provider: provider.clone(),
                    api: api.clone(),
                    model: id.clone(),
                    data,
                };
                async move {
                    if let Some(core) = weak.upgrade() {
                        core.runner.emit(&event, &weak).await;
                    }
                }
                .boxed()
            }));
        }
    }

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

    /// Save one setting to the session's settings file, keeping everything else in it,
    /// as Pi's settings manager writes a changed setting.
    fn save_setting(&self, path: &[&str], value: Value) {
        self.save_settings(vec![(path, value)]);
    }

    /// Save settings that change together in one write.
    fn save_settings(&self, changes: Vec<(&[&str], Value)>) {
        let Some(file) = &self.settings_file else {
            return;
        };
        if let Err(error) = write_settings(file, changes) {
            self.emit(AgentSessionEvent::PersistenceError(format!(
                "Could not save {}: {error}",
                file.display()
            )));
        }
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

    fn all_tools(&self) -> Vec<ToolInfo> {
        let active = lock(&self.active_tools).clone();
        lock(&self.tools)
            .iter()
            .map(|tool| {
                let declaration = tool.tool.declaration();
                ToolInfo {
                    name: declaration.name.clone(),
                    description: declaration.description.clone(),
                    parameters: declaration.parameters.clone(),
                    guidelines: tool.prompt.guidelines.clone(),
                    active: active.contains(&declaration.name),
                    extension: tool.extension.clone(),
                }
            })
            .collect()
    }

    fn commands(&self) -> Vec<SlashCommandInfo> {
        let resources = lock(&self.resources);
        let commands = self
            .runner
            .commands()
            .iter()
            .map(|command| SlashCommandInfo {
                name: command.name.clone(),
                description: command.description.clone(),
                source: SlashCommandSource::Extension,
            });
        let templates = resources
            .prompt_templates
            .iter()
            .map(|template| SlashCommandInfo {
                name: template.name.clone(),
                description: template.description.clone(),
                source: SlashCommandSource::Prompt,
            });
        let skills = resources.skills.iter().map(|skill| SlashCommandInfo {
            name: format!("skill:{}", skill.name),
            description: skill.description.clone(),
            source: SlashCommandSource::Skill,
        });
        commands.chain(templates).chain(skills).collect()
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

    /// The prompt options for the current run: those a `before_agent_start` handler chose,
    /// with the active tools, whose texts from the handler win, as in Pi; without one, the
    /// session's own.
    fn run_prompt_options(&self) -> SystemPromptOptions {
        let live = self.prompt_options();
        let Some(mut options) = lock(&self.run).prompt_options.clone() else {
            return live;
        };
        options.tools = live
            .tools
            .into_iter()
            .map(|tool| {
                options
                    .tools
                    .iter()
                    .find(|chosen| chosen.name == tool.name)
                    .cloned()
                    .unwrap_or(tool)
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

    /// Whether a retry follows a run that added `messages`, as Pi decides it for its
    /// `agent_end` event.
    fn will_retry(&self, messages: &[SessionMessage]) -> bool {
        let retry = lock(&self.settings).retry.clone();
        if self.abort_requested()
            || !retry.enabled
            || lock(&self.run).retry_attempt >= retry.max_retries
        {
            return false;
        }
        let window = self.model().map(|model| model.context_window);
        messages
            .iter()
            .rev()
            .find_map(|message| match message {
                SessionMessage::Llm(Message::Assistant(assistant)) => Some(assistant),
                _ => None,
            })
            .is_some_and(|last| is_retryable_error(last) && !is_context_overflow(last, window))
    }

    /// Name the session, as Pi's `setSessionName` does, and tell listeners.
    async fn set_session_name(&self, name: &str) {
        let name = {
            let mut session = lock(&self.session);
            session.append_session_info(Some(name.to_string()));
            session.session_name().map(str::to_string)
        };
        self.report_persistence();
        self.emit(AgentSessionEvent::SessionInfoChanged { name: name.clone() });
        self.runner
            .emit(&SessionInfoChanged { name }, &self.weak)
            .await;
    }

    /// What tells the host of a summary request's retries, as Pi's summarization retry
    /// callbacks do.
    fn summary_retries(&self, source: SummarySource) -> impl Fn(SummaryRetry) + Send + Sync + '_ {
        move |retry| {
            self.emit(match retry {
                SummaryRetry::Scheduled {
                    attempt,
                    max_attempts,
                    delay_ms,
                    error,
                } => AgentSessionEvent::SummarizationRetryScheduled {
                    attempt,
                    max_attempts,
                    delay_ms,
                    error,
                },
                SummaryRetry::AttemptStart => {
                    AgentSessionEvent::SummarizationRetryAttemptStart { source }
                }
                SummaryRetry::Finished => AgentSessionEvent::SummarizationRetryFinished,
            })
        }
    }

    /// The current branch's context size, from usage the branch has not changed since.
    fn context_tokens(&self) -> u64 {
        let session = lock(&self.session);
        estimate_projected_context_tokens(&session.projection(), &session.branch())
    }

    fn context_usage(&self) -> Option<ContextUsage> {
        let window = self.model()?.context_window;
        if window == 0 {
            return None;
        }
        let tokens = {
            let session = lock(&self.session);
            let projection = session.projection();
            let branch = session.branch();
            // As in Pi: the usage from before a compaction measured the old context, so
            // only a reply since then, still in the context, can tell its size.
            let measured = match branch
                .iter()
                .rposition(|entry| matches!(entry.kind, EntryKind::Compaction { .. }))
            {
                None => true,
                Some(compaction) => {
                    let measuring: Vec<&str> = projection
                        .entries
                        .iter()
                        .filter(|entry| entry.messages.iter().any(measures_context))
                        .map(|entry| entry.entry_id.as_str())
                        .collect();
                    branch[compaction + 1..]
                        .iter()
                        .any(|entry| measuring.contains(&entry.id.as_str()))
                }
            };
            measured.then(|| estimate_projected_context_tokens(&projection, &branch))
        };
        Some(ContextUsage {
            tokens,
            context_window: window,
            percent: tokens.map(|tokens| tokens as f64 * 100.0 / window as f64),
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
            streaming_behavior: options
                .streaming_behavior
                .filter(|_| self.activity() == Activity::Prompt),
        };
        let (mut text, images) = match self.runner.input(input, &self.weak).await {
            InputAction::Handled => return Ok(PromptOutcome::Handled),
            InputAction::Transform { text, images } => (text, images),
            InputAction::Continue => (text.to_string(), options.images.clone()),
        };
        if options.expand {
            text = self.expand(&text);
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
        self.start_run(
            activity,
            RunTrigger::User {
                prompt: text,
                images,
            },
        )
        .await?;
        Ok(PromptOutcome::Completed)
    }

    /// Start a run from a prompt or a custom message, as Pi's `_startRun`. The session is
    /// busy from here on, so messages sent while `before_agent_start` handlers run join
    /// this run, and an abort requested meanwhile stops it once it starts.
    async fn start_run(
        self: &Arc<Self>,
        activity: ActivityGuard<'_>,
        trigger: RunTrigger,
    ) -> Result<(), AgentSessionError> {
        lock(&self.run).abort_requested = false;
        let prepared = self.prepare_run(trigger).await;
        // What extensions and the user's own commands added meanwhile goes before the
        // run's messages, as when idle; no run exists yet to split a call from its result.
        self.flush_pending_bash();
        self.flush_pending_custom();
        self.run_prompt(activity, prepared?).await
    }

    /// The messages that start a run, as Pi's `_prepareRun`: compact first when needed,
    /// ask `before_agent_start`, apply the run's prompt options and tools, then the
    /// starting message, the waiting next-turn messages and the handlers' messages, after
    /// a prompt update when the prompt changed.
    async fn prepare_run(
        self: &Arc<Self>,
        trigger: RunTrigger,
    ) -> Result<Vec<SessionMessage>, AgentSessionError> {
        if self.model().is_none() {
            return Err(AgentSessionError::NoModel);
        }
        // As in Pi, a stopped run, or a switch to a smaller model, can leave a context to
        // compact before the new message, which is the one that runs next.
        let last = self.agent.with_messages(|messages| {
            messages.iter().rev().find_map(|message| match message {
                SessionMessage::Llm(Message::Assistant(assistant)) => Some(assistant.clone()),
                _ => None,
            })
        });
        if let Some(last) = last {
            self.check_compaction(&last, false).await;
        }

        let base = self.prompt_options();
        let event = BeforeAgentStart {
            trigger: trigger.clone(),
            system_prompt: render(&base).unwrap_or_default(),
            options: base.clone(),
        };
        let (prompt_options, drafts, forced) =
            self.runner.before_agent_start(event, &self.weak).await;
        // As in Pi, the handlers' options hold for this run only, except that listing other
        // tools changes which are active.
        let names = |options: &SystemPromptOptions| -> Vec<String> {
            options.tools.iter().map(|tool| tool.name.clone()).collect()
        };
        if names(&prompt_options) != names(&base) {
            self.set_active_tools(&names(&prompt_options));
        }
        lock(&self.run).prompt_options = (prompt_options != base).then(|| prompt_options.clone());
        let mut messages: Vec<SessionMessage> = self
            .system_update(&self.run_prompt_options())?
            .into_iter()
            .collect();
        match trigger {
            RunTrigger::User { prompt, images } => {
                // As in Pi, attached images are made to fit, after the handlers, with notes
                // on what was done; one that cannot be used is left out, its note kept.
                let auto_resize = lock(&self.settings).images.auto_resize;
                let (images, hints) = normalize_prompt_images(images, auto_resize).await;
                let text = if hints.is_empty() {
                    prompt
                } else {
                    format!("{prompt}\n\n{}", hints.join("\n"))
                };
                messages.push(user_message(&text, images));
            }
            RunTrigger::Custom(message) => messages.push(SessionMessage::Custom(message)),
        }
        {
            let mut run = lock(&self.run);
            run.forced_prompt = forced.or(prompt_options.force_prompt.clone());
            messages.append(&mut run.next_turn_messages);
        }
        messages.extend(drafts.into_iter().map(custom_message));
        Ok(messages)
    }

    /// Expand a skill command, then a prompt template.
    fn expand(&self, text: &str) -> String {
        let resources = lock(&self.resources);
        expand_prompt_template(
            &expand_skill_command(text, &resources.skills),
            &resources.prompt_templates,
        )
    }

    /// Queue typed input as Pi's `steer` and `followUp` do.
    async fn queue_input(
        &self,
        text: &str,
        images: Vec<ImageContent>,
        behavior: StreamingBehavior,
    ) -> Result<PromptOutcome, AgentSessionError> {
        if let Some(command) = text.strip_prefix('/') {
            let name = command
                .split(char::is_whitespace)
                .next()
                .unwrap_or_default();
            if self
                .runner
                .commands()
                .iter()
                .any(|known| known.name == name)
            {
                return Err(AgentSessionError::Failed(format!(
                    "Extension command \"/{name}\" cannot be queued; send it as a prompt"
                )));
            }
        }
        let input = Input {
            text: text.to_string(),
            images: images.clone(),
            source: InputSource::Interactive,
            streaming_behavior: (self.activity() == Activity::Prompt).then_some(behavior),
        };
        let (text, images) = match self.runner.input(input, &self.weak).await {
            InputAction::Handled => return Ok(PromptOutcome::Handled),
            InputAction::Transform { text, images } => (text, images),
            InputAction::Continue => (text.to_string(), images),
        };
        self.queue(behavior, user_message(&self.expand(&text), images));
        Ok(PromptOutcome::Queued)
    }

    async fn run_prompt(
        self: &Arc<Self>,
        activity: ActivityGuard<'_>,
        messages: Vec<SessionMessage>,
    ) -> Result<(), AgentSessionError> {
        {
            let mut run = lock(&self.run);
            run.overflow_recovery_attempted = false;
            run.boundary_changed = false;
        }
        self.sync_tools();
        let result = match self.agent.prompt(messages).await {
            Ok(()) => self.after_run().await,
            Err(error) => Err(error.into()),
        };
        self.flush_pending_bash();
        self.flush_pending_custom();
        self.end_retry();
        let aborted = {
            let mut run = lock(&self.run);
            run.forced_prompt = None;
            run.prompt_options = None;
            run.abort_requested
        };
        drop(activity);
        self.runner
            .emit(&AgentSettled { aborted }, &self.weak)
            .await;
        self.emit(AgentSessionEvent::Settled { aborted });
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

    /// Retry, compact or continue with queued messages until the run is really over,
    /// then let extensions add to it before it settles, as Pi's run does.
    async fn after_run(self: &Arc<Self>) -> Result<(), AgentSessionError> {
        loop {
            if self.abort_requested() {
                return Ok(());
            }
            if self.handle_post_run().await {
                if self.abort_requested() {
                    return Ok(());
                }
                self.agent.continue_run().await?;
                continue;
            }
            if self.abort_requested()
                || !self.before_settle_boundary().await
                || self.abort_requested()
            {
                return Ok(());
            }
            self.agent.continue_run().await?;
        }
    }

    /// The context as `drafts` would leave it, as Pi's `_buildBoundaryContext`; an
    /// error when they would not apply. A turn's end can go on with queued messages; a
    /// run about to settle only after a reply, which they then answer.
    fn boundary_context(
        &self,
        drafts: &[BoundaryDraft],
        before_settle: bool,
    ) -> Result<BoundaryContext, String> {
        let mut preview = lock(&self.session).branch_copy();
        apply_drafts(&mut preview, drafts)?;
        let projection = preview.projection();
        let pending_custom = lock(&self.run).pending_custom.clone();
        let mut pending_messages = self.agent.peek_queued_messages();
        pending_messages.extend(pending_custom.iter().cloned());
        let llm_messages = convert_to_llm(&projection.messages);
        let ends_with_reply = matches!(llm_messages.last(), Some(Message::Assistant(_)));
        let has_conversation = llm_messages
            .iter()
            .any(|message| !matches!(message, Message::System(_)));
        let queued = self.agent.has_queued_messages();
        let can_continue = (has_conversation && !ends_with_reply)
            || !pending_custom.is_empty()
            || if before_settle {
                ends_with_reply && queued
            } else {
                queued
            };
        Ok(BoundaryContext {
            context_entries: projection.entries,
            context_messages: projection.messages,
            llm_messages,
            pending_messages,
            can_continue,
        })
    }

    /// Add what a boundary's handlers asked for to the session, and go on from it.
    fn commit_drafts(&self, drafts: &[BoundaryDraft]) {
        if drafts.is_empty() {
            return;
        }
        let appended = apply_drafts(&mut lock(&self.session), drafts);
        self.report_persistence();
        self.rebuild_messages();
        lock(&self.run).boundary_changed = true;
        match appended {
            Ok(entries) => {
                for entry in entries {
                    self.emit(AgentSessionEvent::EntryAppended { entry });
                }
            }
            Err(error) => self.runner.report("<boundary>", "boundary", error),
        }
    }

    fn report_invalid_continuation(&self, event: &'static str) {
        self.runner.report(
            "<boundary>",
            event,
            format!("{event} requested continuation without runnable model context"),
        );
    }

    /// Ask `turn_end` handlers about the turn just ended, as Pi's
    /// `_dispatchTurnEndBoundary`; true when they ask for one more request that has
    /// something to answer.
    async fn turn_end_boundary(
        self: &Arc<Self>,
        message: &AssistantMessage,
        tool_results: &[pi_ai::ToolResultMessage],
    ) -> bool {
        let outcome = match message.stop_reason {
            pi_ai::StopReason::Aborted => ActivityOutcome::Aborted,
            pi_ai::StopReason::Error => ActivityOutcome::Error,
            _ => ActivityOutcome::Completed,
        };
        let turn_index = {
            let mut run = lock(&self.run);
            run.outcome = outcome;
            run.turn_index
        };
        if !self.runner.has::<TurnEnd>() {
            return false;
        }
        let (message_entry_id, tool_result_entry_ids) = {
            let session = lock(&self.session);
            let branch = session.branch();
            let entry_of = |message: &Message| {
                branch.iter().rev().find_map(|entry| match &entry.kind {
                    EntryKind::Message {
                        message: SessionMessage::Llm(recorded),
                    } if recorded == message => Some(entry.id.clone()),
                    _ => None,
                })
            };
            (
                entry_of(&Message::Assistant(message.clone())),
                tool_results
                    .iter()
                    .filter_map(|result| entry_of(&Message::ToolResult(result.clone())))
                    .collect::<Vec<_>>(),
            )
        };
        let Some(message_entry_id) = message_entry_id else {
            self.runner.report(
                "<boundary>",
                "turn_end",
                "turn_end could not resolve the persisted assistant entry ID",
            );
            return false;
        };
        let dispatch = self
            .runner
            .emit_boundary(
                &self.weak,
                |entries, continue_run, context| TurnEnd {
                    turn_index,
                    message: message.clone(),
                    tool_results: tool_results.to_vec(),
                    message_entry_id: message_entry_id.clone(),
                    tool_result_entry_ids: tool_result_entry_ids.clone(),
                    outcome,
                    entries,
                    continue_run,
                    context,
                },
                |drafts| self.boundary_context(drafts, false),
            )
            .await;
        self.commit_drafts(&dispatch.entries);
        let runnable = self
            .boundary_context(&[], false)
            .is_ok_and(|context| context.can_continue);
        if dispatch.continue_run && !runnable {
            self.report_invalid_continuation("turn_end");
            return false;
        }
        dispatch.continue_run
    }

    /// Ask `agent_before_settle` handlers before the run settles, as Pi's
    /// `_runBeforeSettleBoundary`; true when the run goes on, for their request or for
    /// queued messages.
    async fn before_settle_boundary(self: &Arc<Self>) -> bool {
        if !self.runner.has::<AgentBeforeSettle>() {
            return self.agent.has_queued_messages();
        }
        let outcome = lock(&self.run).outcome;
        let dispatch = self
            .runner
            .emit_boundary(
                &self.weak,
                |entries, continue_run, context| AgentBeforeSettle {
                    outcome,
                    entries,
                    continue_run,
                    context,
                },
                |drafts| self.boundary_context(drafts, true),
            )
            .await;
        self.commit_drafts(&dispatch.entries);
        // A run that goes on starts from the session again, with the entries in it.
        lock(&self.run).boundary_changed = false;
        self.flush_pending_custom();
        if self.abort_requested() {
            return false;
        }
        let should_continue = dispatch.continue_run || self.agent.has_queued_messages();
        let runnable = self
            .boundary_context(&[], true)
            .is_ok_and(|context| context.can_continue);
        if should_continue && !runnable {
            if dispatch.continue_run {
                self.report_invalid_continuation("agent_before_settle");
            }
            return false;
        }
        should_continue
    }

    /// Decide what follows a finished run, as Pi's `_handlePostAgentRun` does: a retry, a
    /// compaction, or the queued messages. True when the agent should continue.
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
        let settings = lock(&self.settings).clone();
        let window = self.model().map(|model| model.context_window);
        // An overflow is compacted, never retried.
        if is_retryable_error(&last)
            && !is_context_overflow(&last, window)
            && settings.retry.enabled
        {
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
        if self.check_compaction(&last, true).await {
            return !self.abort_requested();
        }
        self.agent.has_queued_messages()
    }

    /// Compact when `last` calls for it, as Pi's `_checkCompaction` decides. True when the
    /// agent should continue: to ask an interrupted reply again, or for queued messages.
    ///
    /// An overflow, or a reply cut short below the model's output limit, is compacted and
    /// asked again once; a complete reply over the window is compacted and kept; a context
    /// near its limit is compacted. A reply from another model, or from before the latest
    /// compaction, is left alone. `skip_aborted` passes over a stopped reply, which the
    /// check before a prompt looks at.
    async fn check_compaction(
        self: &Arc<Self>,
        last: &AssistantMessage,
        skip_aborted: bool,
    ) -> bool {
        let Some(model) = self.model() else {
            return false;
        };
        let settings = lock(&self.settings).compaction.for_model(Some(&model));
        if !settings.enabled || (skip_aborted && last.stop_reason == pi_ai::StopReason::Aborted) {
            return false;
        }
        let same_model = last.provider == model.provider && last.model == model.id;
        let (stale, edited_after) = {
            let session = lock(&self.session);
            let branch = session.branch();
            let position = branch.iter().rposition(|entry| {
                matches!(
                    &entry.kind,
                    EntryKind::Message {
                        message: SessionMessage::Llm(Message::Assistant(assistant)),
                    } if assistant == last
                )
            });
            let is_compaction =
                |entry: &SessionEntry| matches!(entry.kind, EntryKind::Compaction { .. });
            // A reply from before the latest compaction is stale, as in Pi. Pi compares
            // timestamps; the reply's place in the branch says it exactly, and the
            // timestamps decide only for a reply the branch does not hold.
            let stale = match position {
                Some(position) => branch[position + 1..]
                    .iter()
                    .any(|entry| is_compaction(entry)),
                None => branch
                    .iter()
                    .rev()
                    .find(|entry| is_compaction(entry))
                    .is_some_and(|compaction| last.timestamp <= compaction.timestamp),
            };
            let edited_after = position.is_some_and(|position| {
                branch[position + 1..]
                    .iter()
                    .any(|entry| matches!(entry.kind, EntryKind::ContextEdit { .. }))
            });
            (stale, edited_after)
        };
        if stale {
            return false;
        }
        let explicit_overflow =
            last.stop_reason == pi_ai::StopReason::Error && is_context_overflow(last, None);
        // A silent overflow is read from the reply's usage, which an edit since has changed.
        let overflow = same_model
            && (explicit_overflow
                || (!edited_after && is_context_overflow(last, Some(model.context_window))));
        let recoverable_length = same_model && is_recoverable_length(last, model.max_tokens);
        if overflow || recoverable_length {
            let will_retry = last.stop_reason != pi_ai::StopReason::Stop;
            if !will_retry {
                // A complete reply cannot be continued: compact and keep it.
                return self
                    .clone()
                    .compact(CompactionReason::Overflow, None, false)
                    .await
                    .is_ok_and(|_| self.agent.has_queued_messages());
            }
            if std::mem::replace(&mut lock(&self.run).overflow_recovery_attempted, true) {
                let error = if overflow {
                    "Context overflow recovery failed after one compact-and-retry attempt. Try reducing context or switching to a larger-context model."
                } else {
                    "Truncated response recovery failed after one compact-and-retry attempt."
                };
                self.emit(AgentSessionEvent::CompactionEnd {
                    reason: CompactionReason::Overflow,
                    result: None,
                    aborted: false,
                    will_retry: false,
                    error: Some(error.to_string()),
                });
                self.runner
                    .emit(
                        &SessionCompactFailed {
                            reason: CompactionReason::Overflow,
                            error_message: Some(error.to_string()),
                            aborted: false,
                            will_retry: false,
                            from_extension: false,
                        },
                        &self.weak,
                    )
                    .await;
                return false;
            }
            // The reply leaves the context and is asked again after compaction.
            self.omit_last_assistant();
            return self
                .clone()
                .compact(CompactionReason::Overflow, None, true)
                .await
                .is_ok();
        }
        if should_compact(self.context_tokens(), model.context_window, &settings) {
            return self
                .clone()
                .compact(CompactionReason::Threshold, None, false)
                .await
                .is_ok_and(|_| self.agent.has_queued_messages());
        }
        false
    }

    /// Wait out the backoff, keeping the failed attempt in the tree but out of the context.
    async fn retry(&self, attempt: u32, settings: &Settings, error: &str) -> bool {
        let delay_ms = retry_delay_ms(
            settings.retry.base_delay_ms,
            settings.retry.max_agent_delay_ms,
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

    /// Omit the latest assistant message from the context with a context edit, and the
    /// tool results that answer its calls with it, as Pi's `_omitRecoveryAttempt` does: a
    /// result without its call would be sent on its own.
    fn omit_last_assistant(&self) {
        let edits = {
            let mut session = lock(&self.session);
            let targets: Vec<String> = {
                let branch = session.branch();
                let Some((position, assistant)) =
                    branch
                        .iter()
                        .enumerate()
                        .rev()
                        .find_map(|(position, entry)| match &entry.kind {
                            EntryKind::Message {
                                message: SessionMessage::Llm(Message::Assistant(assistant)),
                            } => Some((position, assistant)),
                            _ => None,
                        })
                else {
                    return;
                };
                let calls: Vec<&str> = assistant
                    .tool_calls()
                    .map(|call| call.id.as_str())
                    .collect();
                let results = branch[position + 1..]
                    .iter()
                    .filter_map(|entry| match &entry.kind {
                        EntryKind::Message {
                            message: SessionMessage::Llm(Message::ToolResult(result)),
                        } if calls.contains(&result.tool_call_id.as_str()) => {
                            Some(entry.id.clone())
                        }
                        _ => None,
                    });
                std::iter::once(branch[position].id.clone())
                    .chain(results)
                    .collect()
            };
            targets
                .iter()
                .filter_map(|target| {
                    let id = session.append_context_edit(target, None).ok()?;
                    session.entry(&id).cloned()
                })
                .collect::<Vec<_>>()
        };
        self.report_persistence();
        self.rebuild_messages();
        for entry in edits {
            self.emit(AgentSessionEvent::EntryAppended { entry });
        }
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

    /// Compact the current branch, as Pi's `compact` and `_runAutoCompaction` do. An
    /// automatic compaction with nothing to compact reports nothing, as in Pi.
    async fn compact(
        self: Arc<Self>,
        reason: CompactionReason,
        custom_instructions: Option<&str>,
        will_retry: bool,
    ) -> Result<CompactionResult, AgentSessionError> {
        let settings = lock(&self.settings).clone();
        let (preparation, branch_entries) = {
            let session = lock(&self.session);
            let branch = session.branch();
            (
                prepare_compaction(
                    &branch,
                    &settings.compaction.for_model(self.model().as_ref()),
                ),
                branch.into_iter().cloned().collect::<Vec<_>>(),
            )
        };
        if preparation.is_none() && reason != CompactionReason::Manual {
            return Err(AgentSessionError::NothingToCompact);
        }
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
        let mut from_extension = false;
        let result = match preparation {
            Some(preparation) => {
                self.compact_inner(
                    preparation,
                    branch_entries,
                    reason,
                    custom_instructions,
                    will_retry,
                    &token,
                    &mut from_extension,
                )
                .await
            }
            None => Err(AgentSessionError::NothingToCompact),
        };
        running.ended = true;
        drop(running);
        match &result {
            Ok(result) => self.emit(AgentSessionEvent::CompactionEnd {
                reason,
                result: Some(result.clone()),
                aborted: false,
                will_retry,
                error: None,
            }),
            Err(error) => {
                let aborted = token.is_cancelled() || matches!(error, AgentSessionError::Cancelled);
                // Pi's words for a failure, and none for a stop.
                let error_message = (!aborted).then(|| match reason {
                    CompactionReason::Manual => format!("Compaction failed: {error}"),
                    CompactionReason::Overflow => {
                        format!("Context overflow recovery failed: {error}")
                    }
                    CompactionReason::Threshold => format!("Auto-compaction failed: {error}"),
                });
                self.emit(AgentSessionEvent::CompactionEnd {
                    reason,
                    result: None,
                    aborted,
                    will_retry: false,
                    error: error_message.clone(),
                });
                self.runner
                    .emit(
                        &SessionCompactFailed {
                            reason,
                            error_message,
                            aborted,
                            will_retry: false,
                            from_extension,
                        },
                        &self.weak,
                    )
                    .await;
            }
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn compact_inner(
        &self,
        preparation: CompactionPreparation,
        branch_entries: Vec<SessionEntry>,
        reason: CompactionReason,
        custom_instructions: Option<&str>,
        will_retry: bool,
        cancel: &CancellationToken,
        from_extension: &mut bool,
    ) -> Result<CompactionResult, AgentSessionError> {
        let model = self.model().ok_or(AgentSessionError::NoModel)?;
        let settings = lock(&self.settings).clone();
        let before = self
            .runner
            .before_compact(
                SessionBeforeCompact {
                    preparation: preparation.clone(),
                    branch_entries,
                    custom_instructions: custom_instructions.map(str::to_string),
                    reason,
                    will_retry,
                    signal: cancel.clone(),
                },
                &self.weak,
            )
            .await;
        if before.cancel {
            return Err(AgentSessionError::Cancelled);
        }
        let mut result = match before.compaction {
            Some(result) => {
                *from_extension = true;
                result
            }
            None => {
                let stream_fn = self.models.stream_fn();
                let report = self.summary_retries(SummarySource::Compaction(reason));
                let summarizer = Summarizer {
                    model: &model,
                    stream_fn: &*stream_fn,
                    options: StreamOptions {
                        api_key: self.models.api_key(&model.provider).await,
                        reasoning: Some(self.agent.thinking_level()),
                        session_id: Some(self.session_id()),
                        cancel: cancel.clone(),
                        ..StreamOptions::default()
                    },
                    max_retries: summary_retries(&settings),
                    retry_base_ms: settings.retry.base_delay_ms,
                    retry_max_ms: settings.retry.max_agent_delay_ms,
                    on_retry: Some(&report),
                };
                compact(&preparation, &summarizer, custom_instructions)
                    .await
                    .map_err(AgentSessionError::Failed)?
            }
        };
        if cancel.is_cancelled() {
            return Err(AgentSessionError::Cancelled);
        }
        let entry_id = lock(&self.session).append_compaction(
            result.summary.clone(),
            Some(result.first_kept_entry_id.clone()),
            result.tokens_before,
            result.details.clone(),
            Some(result.usage),
            *from_extension,
        );
        self.report_persistence();
        self.rebuild_messages();
        result.estimated_tokens_after = Some(
            self.agent
                .with_messages(|messages| messages.iter().map(estimate_tokens).sum()),
        );
        self.runner
            .emit(
                &SessionCompact {
                    entry_id,
                    from_extension: *from_extension,
                    reason,
                    will_retry,
                },
                &self.weak,
            )
            .await;
        Ok(result)
    }

    async fn navigate_tree(
        self: Arc<Self>,
        target_id: &str,
        options: TreeNavigation,
    ) -> Result<TreeMove, AgentSessionError> {
        let (old_leaf, common_ancestor, abandoned) = {
            let session = lock(&self.session);
            if session.entry(target_id).is_none() {
                return Err(SessionError::NotFound(target_id.into()).into());
            }
            let old_leaf = session.leaf_id().map(str::to_string);
            if old_leaf.as_deref() == Some(target_id) {
                return Ok(TreeMove::default());
            }
            // As Pi collects them: the branch left, back to the deepest entry both
            // positions share.
            let target_path: Vec<String> = session
                .branch_to(target_id)
                .iter()
                .map(|entry| entry.id.clone())
                .collect();
            let branch = session.branch();
            let common_ancestor = branch
                .iter()
                .rev()
                .find(|entry| target_path.contains(&entry.id))
                .map(|entry| entry.id.clone());
            let abandoned: Vec<SessionEntry> = branch
                .into_iter()
                .filter(|entry| !target_path.contains(&entry.id))
                .cloned()
                .collect();
            (old_leaf, common_ancestor, abandoned)
        };
        if options.summarize && self.model().is_none() {
            return Err(AgentSessionError::NoModel);
        }
        // Registered so that stopping the session stops a summary too, the extensions'
        // included.
        let cancel = CancellationToken::new();
        lock(&self.run).branch_summary_cancel = Some(cancel.clone());
        let result = self
            .move_in_tree(
                target_id,
                options,
                old_leaf,
                common_ancestor,
                abandoned,
                &cancel,
            )
            .await;
        lock(&self.run).branch_summary_cancel = None;
        result
    }

    async fn move_in_tree(
        &self,
        target_id: &str,
        options: TreeNavigation,
        old_leaf: Option<String>,
        common_ancestor: Option<String>,
        abandoned: Vec<SessionEntry>,
        cancel: &CancellationToken,
    ) -> Result<TreeMove, AgentSessionError> {
        let before = self
            .runner
            .before_tree(
                SessionBeforeTree {
                    preparation: TreePreparation {
                        target_id: target_id.to_string(),
                        old_leaf_id: old_leaf.clone(),
                        common_ancestor_id: common_ancestor,
                        entries_to_summarize: abandoned.clone(),
                        user_wants_summary: options.summarize,
                        custom_instructions: options.custom_instructions.clone(),
                        replace_instructions: options.replace_instructions,
                        label: options.label.clone(),
                    },
                    signal: cancel.clone(),
                },
                &self.weak,
            )
            .await;
        if before.cancel {
            return Err(AgentSessionError::Cancelled);
        }
        let custom_instructions = before.custom_instructions.or(options.custom_instructions);
        let replace_instructions = before
            .replace_instructions
            .unwrap_or(options.replace_instructions);
        let label = before.label.or(options.label);

        let summary = match before.summary.filter(|_| options.summarize) {
            Some(summary) => Some((summary, true)),
            None if options.summarize && !abandoned.is_empty() => {
                let model = self.model().ok_or(AgentSessionError::NoModel)?;
                let settings = lock(&self.settings).clone();
                let stream_fn = self.models.stream_fn();
                let report = self.summary_retries(SummarySource::BranchSummary);
                let summarizer = Summarizer {
                    model: &model,
                    stream_fn: &*stream_fn,
                    options: StreamOptions {
                        api_key: self.models.api_key(&model.provider).await,
                        session_id: Some(self.session_id()),
                        cancel: cancel.clone(),
                        ..StreamOptions::default()
                    },
                    max_retries: summary_retries(&settings),
                    retry_base_ms: settings.retry.base_delay_ms,
                    retry_max_ms: settings.retry.max_agent_delay_ms,
                    on_retry: Some(&report),
                };
                // The window, or Pi's 128k when it is unknown, less the reserve; with
                // nothing left, there is no limit.
                let window = match model.context_window {
                    0 => 128_000,
                    window => window,
                };
                let budget = window.saturating_sub(settings.compaction.reserve_tokens);
                let entries: Vec<_> = abandoned.iter().collect();
                let summary = summarize_branch(
                    &entries,
                    &summarizer,
                    &settings.compaction,
                    budget,
                    custom_instructions.as_deref(),
                    replace_instructions,
                )
                .await;
                // A stopped summary leaves the session where it was.
                if cancel.is_cancelled() {
                    return Err(AgentSessionError::Cancelled);
                }
                let result = summary.map_err(AgentSessionError::Failed)?;
                let details = serde_json::json!({
                    "readFiles": result.read_files,
                    "modifiedFiles": result.modified_files,
                });
                Some((
                    TreeSummary {
                        summary: result.summary,
                        details: Some(details),
                        usage: Some(result.usage),
                    },
                    false,
                ))
            }
            None => None,
        };
        if cancel.is_cancelled() {
            return Err(AgentSessionError::Cancelled);
        }

        let (summary_entry, from_extension, editor_text) = {
            let mut session = lock(&self.session);
            let target = session
                .entry(target_id)
                .ok_or_else(|| SessionError::NotFound(target_id.into()))?;
            // As in Pi: moving to a user or custom message stands before it, with its text
            // for the editor; anything else becomes the leaf.
            let (new_leaf, editor_text) = match &target.kind {
                EntryKind::Message {
                    message: SessionMessage::Llm(Message::User(user)),
                } => (target.parent_id.clone(), Some(content_text(&user.content))),
                EntryKind::CustomMessage { content, .. } => {
                    (target.parent_id.clone(), Some(content_text(content)))
                }
                _ => (Some(target.id.clone()), None),
            };
            let (summary_entry, from_extension) = match summary {
                Some((summary, from_extension)) => {
                    let id = session.branch_with_summary(
                        new_leaf.as_deref(),
                        summary.summary,
                        summary.details,
                        summary.usage,
                        from_extension,
                    )?;
                    if let Some(label) = &label {
                        session.append_label(&id, Some(label.clone()))?;
                    }
                    (session.entry(&id).cloned(), Some(from_extension))
                }
                None => {
                    match &new_leaf {
                        Some(leaf) => session.set_leaf(leaf)?,
                        None => session.reset_leaf(),
                    }
                    if let Some(label) = &label {
                        session.append_label(target_id, Some(label.clone()))?;
                    }
                    (None, None)
                }
            };
            (summary_entry, from_extension, editor_text)
        };
        self.report_persistence();
        self.rebuild_messages();
        self.restore_tools_from_transcript();
        let new_leaf = lock(&self.session).leaf_id().map(str::to_string);
        self.runner
            .emit(
                &SessionTree {
                    new_leaf_id: new_leaf,
                    old_leaf_id: old_leaf,
                    summary_entry: summary_entry.clone(),
                    from_extension,
                },
                &self.weak,
            )
            .await;
        Ok(TreeMove {
            editor_text,
            summary_entry,
        })
    }

    /// Make the tools the transcript declares the active ones, as Pi does after a move in
    /// the tree; a transcript that declares none leaves them as they are.
    fn restore_tools_from_transcript(&self) {
        let declared = self.agent.with_messages(|messages| {
            current_system_message(messages.iter().filter_map(AgentMessage::as_message)).map(
                |system| {
                    system
                        .tools_added
                        .iter()
                        .map(|tool| tool.name.clone())
                        .collect::<Vec<_>>()
                },
            )
        });
        if let Some(names) = declared {
            self.set_active_tools(&names);
        }
    }

    /// As Pi's `setModel`: the model's own level from the settings applies, or else the
    /// settings' default, or else the current level, clamped to the new model. Extensions
    /// hear of a model that really changed.
    async fn set_model(&self, model: Model) {
        let level = {
            let settings = lock(&self.settings);
            settings
                .model_thinking_level(&model)
                .or(settings.default_thinking_level)
        }
        .unwrap_or_else(|| self.agent.thinking_level());
        let previous = lock(&self.model).replace(model.clone());
        self.agent.set_model(model.clone());
        lock(&self.session).append_model_change(&model.provider, &model.id);
        self.report_persistence();
        self.set_thinking_level(level).await;
        let changed = previous
            .as_ref()
            .is_none_or(|previous| previous.provider != model.provider || previous.id != model.id);
        if changed {
            self.runner
                .emit(&ModelSelect { model, previous }, &self.weak)
                .await;
        }
    }

    /// As Pi's `setThinkingLevel`: the level is clamped to the model, and recorded and
    /// announced only when it changes. Without a model, any level stands.
    async fn set_thinking_level(&self, level: ThinkingLevel) {
        let level = match self.model() {
            Some(model) => model.clamp_thinking_level(level),
            None => level,
        };
        let previous = self.agent.thinking_level();
        if level == previous {
            return;
        }
        self.agent.set_thinking_level(level);
        lock(&self.session).append_thinking_level_change(level);
        self.report_persistence();
        self.emit(AgentSessionEvent::ThinkingLevelChanged { level });
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
        let converted = convert_to_llm(messages);
        // Read for each request, so a change during the session applies to the next.
        match self.core.upgrade() {
            Some(core) if lock(&core.settings).images.block_images => block_images(converted),
            _ => converted,
        }
    }

    async fn transform_context(
        &self,
        messages: &[SessionMessage],
        _cancel: &CancellationToken,
    ) -> Option<Vec<SessionMessage>> {
        let core = self.core.upgrade()?;
        let forced = lock(&core.run).forced_prompt.clone();
        let handled = core.runner.has_context_handlers();
        if forced.is_none() && !handled {
            return None;
        }
        let mut messages = messages.to_vec();
        if handled {
            messages = core.runner.context_messages(messages, &self.core).await;
        }
        // After the handlers, as in Pi, so they see the transcript's own prompt.
        if let Some(prompt) = forced {
            force_prompt(&mut messages, &prompt);
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

    async fn finish_turn(
        &self,
        turn: TurnContext<'_, SessionMessage>,
        _cancel: &CancellationToken,
    ) -> Option<TurnDecision> {
        let core = self.core.upgrade()?;
        core.turn_end_boundary(turn.message, turn.tool_results)
            .await
            .then_some(TurnDecision::Continue)
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
        let settings = lock(&core.settings).compaction.for_model(Some(&model));
        let tokens = core.context_tokens();
        // A long tool loop can outgrow the context within one run. An aborted run ends
        // with its next request, so it is not compacted first.
        let compacted = !cancel.is_cancelled()
            && should_compact(tokens, model.context_window, &settings)
            && core
                .clone()
                .compact(CompactionReason::Threshold, None, false)
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
        // Entries a turn_end handler added are in the session, and so in the next
        // request's context.
        let boundary_changed = std::mem::take(&mut lock(&core.run).boundary_changed);
        let context = if compacted || boundary_changed {
            Some(AgentContext::new(core.agent.messages(), tools))
        } else if tools_changed {
            Some(AgentContext::new(turn.context.messages.clone(), tools))
        } else {
            None
        };
        messages.extend(
            core.system_update(&core.run_prompt_options())
                .ok()
                .flatten(),
        );
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
    async fn on_event(&self, event: &AgentEvent<SessionMessage>, cancel: &CancellationToken) {
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
                run.overflow_recovery_attempted = false;
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
            AgentEvent::AgentStart => {
                let mut run = lock(&core.run);
                run.turn_index = 0;
                // An abort requested while the run was starting, during
                // `before_agent_start` say, stops it here, before its first request.
                if run.abort_requested {
                    cancel.cancel();
                }
            }
            AgentEvent::TurnEnd { .. } => lock(&core.run).turn_index += 1,
            AgentEvent::AgentEnd { messages } => {
                let will_retry = core.will_retry(messages);
                core.emit(AgentSessionEvent::AgentEnd {
                    messages: messages.clone(),
                    will_retry,
                });
                return;
            }
            AgentEvent::MessageEnd { message } => {
                core.persist(message);
                if let SessionMessage::Llm(Message::Assistant(assistant)) = message
                    && assistant.stop_reason != pi_ai::StopReason::Error
                    && assistant.stop_reason != pi_ai::StopReason::Length
                {
                    lock(&core.run).overflow_recovery_attempted = false;
                }
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

    /// Add models, and the API they use when it is new, at once, as Pi's
    /// `registerProvider` does once extensions are loaded.
    pub fn register_provider(&self, models: Vec<Model>, api: Option<(String, Arc<dyn StreamFn>)>) {
        let Some(core) = self.core() else { return };
        if let Some((api, implementation)) = api {
            core.models.register_api(&api, implementation);
        }
        core.models.register_models(models);
    }

    /// Remove a provider's models at once, as Pi's `unregisterProvider`.
    pub fn unregister_provider(&self, provider: &str) {
        if let Some(core) = self.core() {
            core.models.unregister_provider(provider);
        }
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

    /// Whether the project's own resources and settings were loaded, as Pi's
    /// `isProjectTrusted`.
    pub fn is_project_trusted(&self) -> bool {
        self.core().is_some_and(|core| core.project_trusted)
    }

    /// The models and credentials the session uses, as Pi's `modelRegistry`.
    pub fn models(&self) -> Option<ModelRegistry> {
        Some(self.core()?.models.clone())
    }

    /// The running agent turn's cancellation, as Pi's `signal`; `None` while none runs.
    pub fn signal(&self) -> Option<CancellationToken> {
        self.core()?.agent.signal()
    }

    /// Compact the session without waiting for it, as Pi's `compact`: whatever runs is
    /// stopped first. Await the handle for the result, as Pi's `onComplete` and
    /// `onError` give it.
    pub fn compact(
        &self,
        custom_instructions: Option<String>,
    ) -> tokio::task::JoinHandle<Result<CompactionResult, AgentSessionError>> {
        let core = self.core();
        tokio::spawn(async move {
            let session = AgentSession {
                core: core
                    .ok_or_else(|| AgentSessionError::Failed("The session has ended".into()))?,
            };
            session.compact(custom_instructions.as_deref()).await
        })
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
            let entry = {
                let mut session = lock(&core.session);
                let id = session.append_custom_entry(custom_type, data);
                session.entry(&id).cloned()
            };
            core.report_persistence();
            if let Some(entry) = entry {
                core.emit(AgentSessionEvent::EntryAppended { entry });
            }
        }
    }

    pub async fn set_session_name(&self, name: &str) {
        if let Some(core) = self.core() {
            core.set_session_name(name).await;
        }
    }

    pub fn session_name(&self) -> Option<String> {
        self.read_session(|session| session.session_name().map(str::to_string))?
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

    /// Every registered tool, as Pi's `getAllTools` describes it.
    pub fn all_tools(&self) -> Vec<ToolInfo> {
        self.core().map(|core| core.all_tools()).unwrap_or_default()
    }

    /// What a prompt starting with `/` runs, as Pi's `getCommands` lists it.
    pub fn commands(&self) -> Vec<SlashCommandInfo> {
        self.core().map(|core| core.commands()).unwrap_or_default()
    }

    /// The session's settings, as Pi's `getSettings` gives them.
    pub fn settings(&self) -> Settings {
        self.core()
            .map(|core| lock(&core.settings).clone())
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
        let custom = custom(draft);
        let message = SessionMessage::Custom(custom.clone());
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
            if let Err(error) = core.start_run(activity, RunTrigger::Custom(custom)).await {
                core.runner.report(&extension, "send_message", error);
            }
        });
    }

    /// Send a user message, as Pi's `sendUserMessage` does. It always leads to a turn:
    /// it goes through input handlers like a typed prompt and runs now when idle,
    /// otherwise as steering or a follow-up.
    pub fn send_user_message(&self, text: &str, images: Vec<ImageContent>, delivery: Delivery) {
        let Some(core) = self.core() else { return };
        let behavior = if delivery == Delivery::FollowUp {
            StreamingBehavior::FollowUp
        } else {
            StreamingBehavior::Steer
        };
        let (text, extension) = (text.to_string(), self.extension.clone());
        tokio::spawn(async move {
            let options = PromptOptions {
                images,
                streaming_behavior: Some(behavior),
                expand: false,
                source: InputSource::Extension,
            };
            if let Err(error) = core.clone().prompt(&text, options).await {
                core.runner.report(&extension, "send_user_message", error);
            }
        });
    }
}

// What only a command can safely wait for, since no run is waiting for it.
impl CommandContext {
    fn session(&self) -> Result<AgentSession, AgentSessionError> {
        let core = self
            .core()
            .ok_or_else(|| AgentSessionError::Failed("The session has ended".into()))?;
        Ok(AgentSession { core })
    }

    /// Wait until no prompt, retry or compaction is in progress, as Pi's `waitForIdle`.
    pub async fn wait_for_idle(&self) {
        if let Ok(session) = self.session() {
            session.wait_for_idle().await;
        }
    }

    /// Move to another point of the tree, as Pi's `navigateTree`.
    pub async fn navigate_tree(
        &self,
        target_id: &str,
        options: TreeNavigation,
    ) -> Result<TreeMove, AgentSessionError> {
        self.session()?.navigate_tree(target_id, options).await
    }

    /// The runtime that owns the session, when one does.
    fn actions(&self) -> Option<Arc<dyn SessionActions>> {
        let core = self.core()?;
        let actions = lock(&core.actions).clone()?;
        actions.upgrade()
    }

    /// Start a new session in place of this one, as Pi's `newSession`. It is the
    /// runtime's to do; without one, nothing happens, as in Pi.
    pub async fn new_session(&self, options: NewSessionOptions) -> Result<(), AgentSessionError> {
        match self.actions() {
            Some(actions) => actions.new_session(options).await,
            None => Ok(()),
        }
    }

    /// Go on with the session stored in `path` instead, as Pi's `switchSession`. It is
    /// the runtime's to do; without one, nothing happens, as in Pi.
    pub async fn switch_session(
        &self,
        path: &Path,
        options: SwitchSessionOptions,
    ) -> Result<(), AgentSessionError> {
        match self.actions() {
            Some(actions) => actions.switch_session(path, options).await,
            None => Ok(()),
        }
    }

    /// Fork the session, as Pi's `fork`, and give back the text of a user message
    /// forked before. A runtime replaces the session with the fork; without one, the
    /// session goes on as the fork, as [`AgentSession::fork`] does, kept in a new file
    /// next to its own or in memory.
    pub async fn fork(
        &self,
        entry_id: &str,
        options: ForkOptions,
    ) -> Result<Option<String>, AgentSessionError> {
        if let Some(actions) = self.actions() {
            return actions.fork(entry_id, options).await;
        }
        let session = self.session()?;
        let edited = session
            .fork_here(entry_id, options.position, |tree, leaf| {
                match tree
                    .session_file()
                    .and_then(Path::parent)
                    .map(Path::to_path_buf)
                {
                    Some(dir) => tree.fork_in(leaf, &dir),
                    None => tree.fork(leaf, Box::new(crate::store::MemoryStore)),
                }
            })
            .await?;
        if let Some(with_session) = options.with_session {
            with_session(session.command_context()).await;
        }
        Ok(edited)
    }

    /// The options the session builds its system prompt from, as Pi's
    /// `getSystemPromptOptions`.
    pub fn system_prompt_options(&self) -> SystemPromptOptions {
        self.core()
            .map(|core| core.prompt_options())
            .unwrap_or_default()
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
