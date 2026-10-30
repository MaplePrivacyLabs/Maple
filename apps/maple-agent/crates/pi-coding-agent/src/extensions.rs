//! The extension API.
//!
//! An extension registers handlers for session events, tools, commands and model
//! providers. Handlers run in load order and registration order, each with an
//! [`ExtensionContext`] that can read the session and act on it. How their answers
//! combine depends on the event:
//!
//! - observers ([`SessionStart`], [`AgentEventSeen`], ...) all run, and a failing handler
//!   is reported without affecting the others;
//! - a [`ToolCall`] gate stops at the first block, and a failing gate blocks the call;
//! - rewrites ([`ToolResult`], [`MessageEnd`], [`Context`], [`ContextWithSystem`],
//!   [`BeforeProviderRequest`], [`BeforeProviderHeaders`], [`Input`], [`BeforeAgentStart`])
//!   chain, each handler seeing the previous one's output;
//! - `before` events ([`SessionBeforeCompact`], [`SessionBeforeTree`]) stop at the first
//!   cancellation.

use std::any::{Any, TypeId};
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use futures_util::FutureExt;
use futures_util::future::BoxFuture;
use pi_agent_core::{
    AfterToolCallResult, AgentEvent, AgentMessage, AgentTool, BeforeToolCallResult,
};
use pi_ai::transcript::current_system_message;
use pi_ai::{
    AssistantMessage, ClassifierModel, ClassifyFn, Content, ImageContent, Message, Model, StreamFn,
    ThinkingLevel, ToolResultMessage, Usage,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::agent_session::{CompactionReason, SessionCore, StreamingBehavior};
use crate::bash_executor::BashResult;
use crate::compaction::{CompactionPreparation, CompactionResult};
use crate::messages::{CustomMessage, SessionMessage};
use crate::session::{ProjectedEntry, SessionEntry};
use crate::system_prompt::SystemPromptOptions;
use crate::tools::BashOperations;

pub type ExtensionError = Box<dyn std::error::Error + Send + Sync>;
pub type HandlerResult<T> = Result<T, ExtensionError>;

/// An event extensions can handle. A handler answers with `Output`; the default answer
/// changes nothing.
pub trait ExtensionEvent: Clone + Send + Sync + 'static {
    const NAME: &'static str;
    type Output: Default + Send + 'static;
}

/// Why a session starts, as Pi's `session_start` gives it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SessionStartReason {
    /// The host's first session.
    #[default]
    Startup,
    /// The same session, built again after the host reloaded what it is made of.
    Reload,
    /// A new session in place of the one before.
    New,
    /// A stored session in place of the one before.
    Resume,
    /// A fork of the one before.
    Fork,
}

#[derive(Clone, Debug, Default)]
pub struct SessionStart {
    pub reason: SessionStartReason,
    /// The file of the session before, for a new, resumed or forked one, when its store
    /// kept one.
    pub previous_session_file: Option<PathBuf>,
}

/// Where a fork starts, as Pi's `fork` takes it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ForkPosition {
    /// Before a user message: the new session holds the path up to it, and its text
    /// comes back to be edited. Pi's default.
    #[default]
    Before,
    /// At an entry: the new session holds the path up to and including it.
    At,
}

/// A fork about to happen. Return `cancel` to refuse it.
#[derive(Clone, Debug)]
pub struct SessionBeforeFork {
    pub entry_id: String,
    pub position: ForkPosition,
}

#[derive(Clone, Debug, Default)]
pub struct BeforeForkResult {
    pub cancel: bool,
}

/// Why a session ends, as Pi's `session_shutdown` gives it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SessionShutdownReason {
    /// The host is done with it.
    #[default]
    Quit,
    /// It is built again after the host reloaded what it is made of.
    Reload,
    /// A new session replaces it.
    New,
    /// A stored session replaces it.
    Resume,
    /// A fork of it replaces it.
    Fork,
}

/// The session is ending, as Pi's `session_shutdown`.
#[derive(Clone, Debug, Default)]
pub struct SessionShutdown {
    pub reason: SessionShutdownReason,
    /// The file of the session that replaces this one, when its store keeps one.
    pub target_session_file: Option<PathBuf>,
}

/// Why a session is about to be replaced, as Pi's `session_before_switch` gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionSwitchReason {
    /// By a new session.
    New,
    /// By a stored one.
    Resume,
}

/// A session about to be replaced by a new or a stored one. Return `cancel` to keep it.
#[derive(Clone, Debug)]
pub struct SessionBeforeSwitch {
    pub reason: SessionSwitchReason,
    /// The file to switch to; none for a new session, as in Pi.
    pub target_session_file: Option<PathBuf>,
}

#[derive(Clone, Debug, Default)]
pub struct BeforeSwitchResult {
    pub cancel: bool,
}

/// The session's name changed; `None` when it was cleared.
#[derive(Clone, Debug)]
pub struct SessionInfoChanged {
    pub name: Option<String>,
}

/// How the run that reached a boundary went, as Pi's `AgentActivityOutcome`: from the
/// last reply's stop reason.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ActivityOutcome {
    #[default]
    Completed,
    Aborted,
    Error,
}

/// An entry an extension asks to add at a boundary, as Pi's `SessionBoundaryDraft`.
#[derive(Clone, Debug, PartialEq)]
pub enum BoundaryDraft {
    /// Extension state, never in the context.
    Custom {
        custom_type: String,
        data: Option<Value>,
    },
    /// An extension message in the context.
    CustomMessage {
        custom_type: String,
        content: Vec<Content>,
        display: bool,
        details: Option<Value>,
    },
    /// Replace an earlier entry's content in the context, or omit it with `None`.
    ContextEdit {
        target_id: String,
        replacement: Option<Vec<Content>>,
    },
    /// A compaction the extension made. Without `first_kept_entry_id` it keeps no
    /// earlier entry.
    Compaction {
        summary: String,
        first_kept_entry_id: Option<String>,
        details: Option<Value>,
        usage: Option<Usage>,
    },
}

/// The context as the drafts so far would leave it, as Pi's `BoundaryContextPreview`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BoundaryContext {
    pub context_entries: Vec<ProjectedEntry>,
    pub context_messages: Vec<SessionMessage>,
    pub llm_messages: Vec<Message>,
    /// Messages waiting to go in: the queued ones the next turn takes, and extension
    /// messages.
    pub pending_messages: Vec<SessionMessage>,
    /// Whether another request would have something to answer.
    pub can_continue: bool,
}

/// A turn ended, as Pi's `turn_end`: its reply and tool results are in the session.
/// Handlers can add entries and ask for one more request even when nothing else
/// would make one. Each sees what the handlers before it asked for.
#[derive(Clone, Debug)]
pub struct TurnEnd {
    /// The turn's place in the run, counted from zero.
    pub turn_index: usize,
    pub message: AssistantMessage,
    pub tool_results: Vec<ToolResultMessage>,
    pub message_entry_id: String,
    pub tool_result_entry_ids: Vec<String>,
    pub outcome: ActivityOutcome,
    /// The entries asked for so far.
    pub entries: Vec<BoundaryDraft>,
    /// Whether one more request was asked for so far.
    pub continue_run: bool,
    pub context: BoundaryContext,
}

/// A run is about to settle, as Pi's `agent_before_settle`: its retries and
/// compactions are done. Handlers can add entries and ask for one more request.
#[derive(Clone, Debug)]
pub struct AgentBeforeSettle {
    pub outcome: ActivityOutcome,
    pub entries: Vec<BoundaryDraft>,
    pub continue_run: bool,
    pub context: BoundaryContext,
}

/// A boundary handler's answer, as Pi's `BoundaryResult`: `entries` replace those asked
/// for so far, and `continue_run` whether one more request follows.
#[derive(Clone, Debug, Default)]
pub struct BoundaryResult {
    pub entries: Option<Vec<BoundaryDraft>>,
    pub continue_run: Option<bool>,
}

/// What the handlers at a boundary asked for, or nothing when their entries would
/// not apply.
pub(crate) struct BoundaryDispatch {
    pub entries: Vec<BoundaryDraft>,
    pub continue_run: bool,
}

/// A prompt's run, its retries and compactions are over, as Pi's `agent_settled`.
#[derive(Clone, Debug)]
pub struct AgentSettled {
    /// Whether it was stopped.
    pub aborted: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InputSource {
    #[default]
    Interactive,
    Extension,
}

/// User input before commands, skills or templates are expanded.
#[derive(Clone, Debug)]
pub struct Input {
    pub text: String,
    pub images: Vec<ImageContent>,
    pub source: InputSource,
    /// How the input waits for the run in progress; `None` when none is.
    pub streaming_behavior: Option<StreamingBehavior>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum InputAction {
    #[default]
    Continue,
    /// Go on with this text and these images instead.
    Transform {
        text: String,
        images: Vec<ImageContent>,
    },
    /// The extension took care of the input; nothing is sent.
    Handled,
}

/// A run about to start, from a prompt or from a custom message, as Pi's
/// `before_agent_start` for both of its message types.
#[derive(Clone, Debug)]
pub struct BeforeAgentStart {
    /// The message that starts the run.
    pub trigger: RunTrigger,
    pub system_prompt: String,
    pub options: SystemPromptOptions,
}

/// What starts a run, as Pi's `messageType` says.
#[derive(Clone, Debug, PartialEq)]
pub enum RunTrigger {
    /// A prompt: its text after expansion, and the images attached to it.
    User {
        prompt: String,
        images: Vec<ImageContent>,
    },
    /// A custom message that starts a turn, as Pi's `sendMessage` with `triggerTurn`.
    Custom(CustomMessage),
}

/// A message an extension asks to add.
#[derive(Clone, Debug, PartialEq)]
pub struct CustomMessageDraft {
    pub custom_type: String,
    pub content: Vec<Content>,
    pub display: bool,
    pub details: Option<Value>,
}

#[derive(Clone, Debug, Default)]
pub struct BeforeAgentStartResult {
    /// Sent with the prompt.
    pub message: Option<CustomMessageDraft>,
    /// The prompt options for this run. Listing other tools changes which are active.
    pub options: Option<SystemPromptOptions>,
    /// The complete system prompt for this run only.
    pub system_prompt: Option<String>,
}

/// Every agent lifecycle event, for observers.
#[derive(Clone, Debug)]
pub struct AgentEventSeen(pub AgentEvent<SessionMessage>);

/// A message about to be recorded; return a replacement of the same kind to change it.
#[derive(Clone, Debug)]
pub struct MessageEnd {
    pub message: SessionMessage,
}

/// A tool call about to run. Return a decision to block it or to change its input.
#[derive(Clone, Debug)]
pub struct ToolCall {
    pub tool_call_id: String,
    pub tool_name: String,
    pub input: Value,
}

/// A tool result about to be reported. Return replacements for parts of it.
#[derive(Clone, Debug)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub tool_name: String,
    pub input: Value,
    pub content: Vec<Content>,
    pub details: Option<Value>,
    pub is_error: bool,
    pub usage: Option<Usage>,
}

/// The conversation about to be sent, without its system messages, as Pi's `context`
/// event has it. The prompt and tools are the session's: they are put back in front of
/// a handler's replacement, so a handler cannot drop them.
#[derive(Clone, Debug)]
pub struct Context {
    pub messages: Vec<SessionMessage>,
}

/// The transcript about to be sent, system messages included, after every [`Context`]
/// handler, as Pi's `context_with_system` event has it. The handler owns the prompt and
/// tool declarations: a replacement is sent as returned and must keep the leading system
/// message, which carries them.
#[derive(Clone, Debug)]
pub struct ContextWithSystem {
    pub messages: Vec<SessionMessage>,
}

/// A provider request body about to be sent.
#[derive(Clone, Debug)]
pub struct BeforeProviderRequest {
    pub payload: Value,
}

/// The headers of a session's provider request about to be sent, as Pi's
/// `before_provider_headers` has them. A handler answers with the headers to send; a
/// `None` value removes that header.
#[derive(Clone, Debug)]
pub struct BeforeProviderHeaders {
    pub headers: BTreeMap<String, Option<String>>,
}

/// The start of a response to a session's provider request, before its body is read.
#[derive(Clone, Debug)]
pub struct AfterProviderResponse {
    pub status: u16,
    /// Header names are in lowercase.
    pub headers: BTreeMap<String, String>,
}

/// An event a provider streamed in reply to a session's request, as the provider parsed
/// it, before it is read.
#[derive(Clone, Debug)]
pub struct ProviderStreamEvent {
    pub provider: String,
    pub api: String,
    pub model: String,
    pub data: Value,
}

/// Why extensions are asked for resources.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourcesDiscoverReason {
    Startup,
    Reload,
}

/// Asked after [`SessionStart`], as Pi's `resources_discover`: where the extension keeps
/// skills and prompt templates of its own. They join the session's resources after the
/// others.
#[derive(Clone, Debug)]
pub struct ResourcesDiscover {
    pub cwd: PathBuf,
    pub reason: ResourcesDiscoverReason,
}

/// Skill and prompt template files or folders; a relative path is relative to the
/// session's folder.
#[derive(Clone, Debug, Default)]
pub struct ResourcesDiscoverResult {
    pub skill_paths: Vec<PathBuf>,
    pub prompt_paths: Vec<PathBuf>,
}

/// A shell command the user runs with `!`, or `!!` to keep it out of the context, as
/// Pi's `user_bash` has it. Its host asks before it runs the command.
#[derive(Clone, Debug)]
pub struct UserBash {
    pub command: String,
    pub exclude_from_context: bool,
    pub cwd: PathBuf,
}

/// How an extension takes over a user's shell command.
#[derive(Clone)]
pub enum UserBashResult {
    /// Run it with these operations instead of this machine's shell.
    Operations(Arc<dyn BashOperations>),
    /// The extension ran it; this is what came of it.
    Result(BashResult),
}

/// A compaction about to happen, as Pi's `session_before_compact` describes it.
#[derive(Clone, Debug)]
pub struct SessionBeforeCompact {
    pub preparation: CompactionPreparation,
    /// The current branch, root first.
    pub branch_entries: Vec<SessionEntry>,
    pub custom_instructions: Option<String>,
    pub reason: CompactionReason,
    /// Whether the interrupted reply is asked again once compacted.
    pub will_retry: bool,
    /// Cancelled when the session is stopped, for a handler that summarizes itself.
    pub signal: CancellationToken,
}

#[derive(Clone, Debug, Default)]
pub struct BeforeCompactResult {
    pub cancel: bool,
    /// Use this compaction instead of summarizing with the model.
    pub compaction: Option<CompactionResult>,
}

#[derive(Clone, Debug)]
pub struct SessionCompact {
    pub entry_id: String,
    pub from_extension: bool,
    pub reason: CompactionReason,
    pub will_retry: bool,
}

/// A compaction that failed or was stopped, as Pi's `session_compact_failed`.
#[derive(Clone, Debug)]
pub struct SessionCompactFailed {
    pub reason: CompactionReason,
    /// `None` when it was stopped or an extension cancelled it.
    pub error_message: Option<String>,
    pub aborted: bool,
    pub will_retry: bool,
    pub from_extension: bool,
}

/// What a move in the tree is about to do, as Pi's `TreePreparation` describes it.
#[derive(Clone, Debug)]
pub struct TreePreparation {
    pub target_id: String,
    pub old_leaf_id: Option<String>,
    /// The deepest entry the old and the new position share.
    pub common_ancestor_id: Option<String>,
    /// The branch being left, oldest first.
    pub entries_to_summarize: Vec<SessionEntry>,
    pub user_wants_summary: bool,
    pub custom_instructions: Option<String>,
    pub replace_instructions: bool,
    pub label: Option<String>,
}

/// A move in the tree about to happen.
#[derive(Clone, Debug)]
pub struct SessionBeforeTree {
    pub preparation: TreePreparation,
    /// Cancelled when the session is stopped, for a handler that summarizes itself.
    pub signal: CancellationToken,
}

/// A summary of the branch being left that an extension wrote.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TreeSummary {
    pub summary: String,
    pub details: Option<Value>,
    pub usage: Option<Usage>,
}

#[derive(Clone, Debug, Default)]
pub struct BeforeTreeResult {
    pub cancel: bool,
    /// Use this summary of the branch being left, when the user wants one, instead of
    /// generating one.
    pub summary: Option<TreeSummary>,
    /// Override the instructions and the label, as in Pi.
    pub custom_instructions: Option<String>,
    pub replace_instructions: Option<bool>,
    pub label: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SessionTree {
    pub new_leaf_id: Option<String>,
    pub old_leaf_id: Option<String>,
    pub summary_entry: Option<SessionEntry>,
    /// Whether an extension wrote the summary; `None` without one.
    pub from_extension: Option<bool>,
}

#[derive(Clone, Debug)]
pub struct ModelSelect {
    pub model: Model,
    pub previous: Option<Model>,
}

#[derive(Clone, Debug)]
pub struct ThinkingLevelSelect {
    pub level: ThinkingLevel,
    pub previous: ThinkingLevel,
}

macro_rules! events {
    ($($event:ty => $output:ty, $name:literal;)*) => {
        $(impl ExtensionEvent for $event {
            const NAME: &'static str = $name;
            type Output = $output;
        })*
    };
}

events! {
    SessionStart => (), "session_start";
    SessionShutdown => (), "session_shutdown";
    SessionInfoChanged => (), "session_info_changed";
    AgentSettled => (), "agent_settled";
    TurnEnd => BoundaryResult, "turn_end";
    AgentBeforeSettle => BoundaryResult, "agent_before_settle";
    Input => InputAction, "input";
    BeforeAgentStart => BeforeAgentStartResult, "before_agent_start";
    AgentEventSeen => (), "agent_event";
    MessageEnd => Option<SessionMessage>, "message_end";
    ToolCall => Option<BeforeToolCallResult>, "tool_call";
    ToolResult => Option<AfterToolCallResult>, "tool_result";
    Context => Option<Vec<SessionMessage>>, "context";
    ContextWithSystem => Option<Vec<SessionMessage>>, "context_with_system";
    BeforeProviderRequest => Option<Value>, "before_provider_request";
    ResourcesDiscover => ResourcesDiscoverResult, "resources_discover";
    UserBash => Option<UserBashResult>, "user_bash";
    BeforeProviderHeaders => Option<BTreeMap<String, Option<String>>>, "before_provider_headers";
    AfterProviderResponse => (), "after_provider_response";
    ProviderStreamEvent => (), "provider_stream_event";
    SessionBeforeCompact => BeforeCompactResult, "session_before_compact";
    SessionCompact => (), "session_compact";
    SessionCompactFailed => (), "session_compact_failed";
    SessionBeforeSwitch => BeforeSwitchResult, "session_before_switch";
    SessionBeforeFork => BeforeForkResult, "session_before_fork";
    SessionBeforeTree => BeforeTreeResult, "session_before_tree";
    SessionTree => (), "session_tree";
    ModelSelect => (), "model_select";
    ThinkingLevelSelect => (), "thinking_level_select";
}

type Handler<E> = Arc<
    dyn Fn(E, ExtensionContext) -> BoxFuture<'static, HandlerResult<<E as ExtensionEvent>::Output>>
        + Send
        + Sync,
>;

type CommandHandler =
    Arc<dyn Fn(String, CommandContext) -> BoxFuture<'static, HandlerResult<()>> + Send + Sync>;

/// A tool's contribution to the system prompt.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolPrompt {
    pub snippet: Option<String>,
    pub guidelines: Vec<String>,
}

#[derive(Clone)]
pub struct RegisteredTool {
    pub tool: Arc<dyn AgentTool>,
    pub prompt: ToolPrompt,
    /// Whether registering the tool activates it.
    pub active: bool,
    /// The extension that registered it; `None` for the host's tools.
    pub extension: Option<String>,
}

/// A registered tool, as Pi's `getAllTools` describes it.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    /// The JSON schema of its arguments.
    pub parameters: Value,
    /// The rules it adds to the prompt while it is active.
    pub guidelines: Vec<String>,
    /// Whether the model is offered it.
    pub active: bool,
    /// The extension that registered it; `None` for the built-in tools and the host's.
    pub extension: Option<String>,
}

/// Where a slash command comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlashCommandSource {
    Extension,
    Prompt,
    Skill,
}

/// Something a prompt starting with `/` runs, as Pi's `getCommands` lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct SlashCommandInfo {
    /// The name after the slash; `skill:<name>` for a skill.
    pub name: String,
    pub description: String,
    pub source: SlashCommandSource,
}

#[derive(Clone)]
pub struct RegisteredCommand {
    pub name: String,
    pub description: String,
    pub extension: String,
    handler: CommandHandler,
    completions: Option<CompletionHandler>,
}

/// A completion a host can offer for a command's arguments, as Pi's `AutocompleteItem`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutocompleteItem {
    /// What the completion puts in.
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

type CompletionHandler =
    Arc<dyn Fn(String) -> BoxFuture<'static, Option<Vec<AutocompleteItem>>> + Send + Sync>;

/// Models and an API implementation an extension provides.
#[derive(Clone)]
pub struct ProviderRegistration {
    pub models: Vec<Model>,
    pub api: Option<(String, Arc<dyn StreamFn>)>,
}

/// Classifier models and a classifier API an extension provides, as the classifiers of
/// Pi's `registerProvider`.
#[derive(Clone)]
pub struct ClassifierRegistration {
    pub models: Vec<ClassifierModel>,
    pub api: Option<(String, Arc<dyn ClassifyFn>)>,
}

/// An extension failure that did not stop the session.
#[derive(Clone, Debug, PartialEq)]
pub struct ExtensionErrorReport {
    pub extension: String,
    pub event: &'static str,
    pub error: String,
}

/// Dialogs and notices the host can show for extensions.
#[async_trait]
pub trait ExtensionUi: Send + Sync {
    fn has_ui(&self) -> bool {
        false
    }

    fn notify(&self, _message: &str, _level: NotifyLevel) {}

    async fn confirm(&self, _title: &str, _message: &str, _dialog: &DialogOptions) -> bool {
        false
    }

    /// The index of the chosen option.
    async fn select(
        &self,
        _title: &str,
        _options: &[String],
        _dialog: &DialogOptions,
    ) -> Option<usize> {
        None
    }

    async fn input(
        &self,
        _title: &str,
        _placeholder: Option<&str>,
        _dialog: &DialogOptions,
    ) -> Option<String> {
        None
    }

    /// A multi-line editor, filled with `prefill`: the edited text, or `None` when it
    /// is dismissed.
    async fn editor(&self, _title: &str, _prefill: Option<&str>) -> Option<String> {
        None
    }

    /// Status text under `key`, as in a status bar; `None` clears it.
    fn set_status(&self, _key: &str, _text: Option<&str>) {}

    /// What shows while a run streams; `None` restores the host's own.
    fn set_working_message(&self, _message: Option<&str>) {}

    /// Lines shown by the editor under `key`; `None` removes them.
    fn set_widget(&self, _key: &str, _lines: Option<&[String]>, _placement: WidgetPlacement) {}

    fn set_title(&self, _title: &str) {}

    /// Put text into the editor as a paste does.
    fn paste_to_editor(&self, _text: &str) {}

    fn set_editor_text(&self, _text: &str) {}

    fn editor_text(&self) -> String {
        String::new()
    }
}

/// How serious a notice is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NotifyLevel {
    #[default]
    Info,
    Warning,
    Error,
}

/// How a dialog may end without an answer, as Pi's dialog options: the host closes it
/// when `cancel` fires or `timeout_ms` passes, showing the time left.
#[derive(Clone, Debug, Default)]
pub struct DialogOptions {
    pub cancel: Option<CancellationToken>,
    pub timeout_ms: Option<u64>,
}

/// Where a widget goes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WidgetPlacement {
    #[default]
    AboveEditor,
    BelowEditor,
}

/// No interface: confirmations are declined and questions go unanswered.
pub struct NoUi;

impl ExtensionUi for NoUi {}

/// An extension's handle on its session: reads its state and acts on it.
#[derive(Clone)]
pub struct ExtensionContext {
    pub(crate) core: Weak<SessionCore>,
    pub(crate) extension: Arc<str>,
}

impl ExtensionContext {
    /// The extension this context belongs to.
    pub fn extension(&self) -> &str {
        &self.extension
    }

    pub(crate) fn core(&self) -> Option<Arc<SessionCore>> {
        self.core.upgrade()
    }
}

/// What a command's handler gets, as Pi's `ExtensionCommandContext`: the extension's
/// context, and what only a command the user started can safely wait for, such as the
/// end of a run, a move in the tree or a fork.
#[derive(Clone)]
pub struct CommandContext {
    context: ExtensionContext,
}

impl CommandContext {
    pub(crate) fn new(context: ExtensionContext) -> Self {
        Self { context }
    }
}

impl std::ops::Deref for CommandContext {
    type Target = ExtensionContext;

    fn deref(&self) -> &ExtensionContext {
        &self.context
    }
}

/// Handlers by event type: each holds the extension name and a boxed `Handler<E>`.
type HandlerTable = HashMap<TypeId, Vec<(Arc<str>, Box<dyn Any + Send + Sync>)>>;

#[derive(Default)]
pub(crate) struct ExtensionRegistry {
    handlers: HandlerTable,
    pub tools: Vec<RegisteredTool>,
    pub commands: Vec<RegisteredCommand>,
    pub providers: Vec<ProviderRegistration>,
    pub classifiers: Vec<ClassifierRegistration>,
}

/// What an extension registers with while it loads.
pub struct ExtensionApi<'a> {
    registry: &'a mut ExtensionRegistry,
    context: ExtensionContext,
}

impl ExtensionApi<'_> {
    pub fn name(&self) -> &str {
        &self.context.extension
    }

    /// A context to keep for later, for example in a tool.
    pub fn context(&self) -> ExtensionContext {
        self.context.clone()
    }

    /// Handle an event.
    pub fn on<E, F, Fut>(&mut self, handler: F)
    where
        E: ExtensionEvent,
        F: Fn(E, ExtensionContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HandlerResult<E::Output>> + Send + 'static,
    {
        let handler: Handler<E> = Arc::new(move |event, context| handler(event, context).boxed());
        self.registry
            .handlers
            .entry(TypeId::of::<E>())
            .or_default()
            .push((self.context.extension.clone(), Box::new(handler)));
    }

    /// Offer a tool to the model. Active tools are declared to it.
    pub fn register_tool(&mut self, tool: Arc<dyn AgentTool>, prompt: ToolPrompt, active: bool) {
        self.registry.tools.push(RegisteredTool {
            tool,
            prompt,
            active,
            extension: Some(self.context.extension.to_string()),
        });
    }

    /// Handle `/name args` typed as a prompt.
    pub fn register_command<F, Fut>(&mut self, name: &str, description: &str, handler: F)
    where
        F: Fn(String, CommandContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HandlerResult<()>> + Send + 'static,
    {
        self.registry.commands.push(RegisteredCommand {
            name: name.to_string(),
            description: description.to_string(),
            extension: self.context.extension.to_string(),
            handler: Arc::new(move |args, context| handler(args, context).boxed()),
            completions: None,
        });
    }

    /// Offer completions for the arguments of `name`, a command this extension
    /// registered, as Pi's `getArgumentCompletions`: given what has been typed after the
    /// command, the completions, or `None` for none.
    pub fn register_command_completions<F, Fut>(&mut self, name: &str, completions: F)
    where
        F: Fn(String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Option<Vec<AutocompleteItem>>> + Send + 'static,
    {
        let extension = &*self.context.extension;
        if let Some(command) = self
            .registry
            .commands
            .iter_mut()
            .rev()
            .find(|command| command.name == name && command.extension == extension)
        {
            command.completions = Some(Arc::new(move |prefix| completions(prefix).boxed()));
        }
    }

    /// Add models, and the API implementation they use when it is new.
    pub fn register_provider(
        &mut self,
        models: Vec<Model>,
        api: Option<(String, Arc<dyn StreamFn>)>,
    ) {
        self.registry
            .providers
            .push(ProviderRegistration { models, api });
    }

    /// Add classifier models, and the classifier API they use when it is new.
    pub fn register_classifiers(
        &mut self,
        models: Vec<ClassifierModel>,
        api: Option<(String, Arc<dyn ClassifyFn>)>,
    ) {
        self.registry
            .classifiers
            .push(ClassifierRegistration { models, api });
    }
}

/// `conversation` behind one system message holding the prompt and tools `transcript`
/// declares, as Pi restores them after a `context` handler.
fn with_current_system(
    transcript: &[SessionMessage],
    conversation: Vec<SessionMessage>,
) -> Vec<SessionMessage> {
    let head = current_system_message(transcript.iter().filter_map(AgentMessage::as_message));
    head.map(|system| SessionMessage::Llm(Message::System(system)))
        .into_iter()
        .chain(conversation)
        .collect()
}

/// A plugin.
pub trait Extension: Send + Sync {
    fn name(&self) -> &str;
    fn register(&self, api: &mut ExtensionApi<'_>);
}

struct FnExtension<F> {
    name: String,
    register: F,
}

impl<F: Fn(&mut ExtensionApi<'_>) + Send + Sync> Extension for FnExtension<F> {
    fn name(&self) -> &str {
        &self.name
    }

    fn register(&self, api: &mut ExtensionApi<'_>) {
        (self.register)(api)
    }
}

/// An extension from a registration function.
pub fn extension(
    name: &str,
    register: impl Fn(&mut ExtensionApi<'_>) + Send + Sync + 'static,
) -> Arc<dyn Extension> {
    Arc::new(FnExtension {
        name: name.to_string(),
        register,
    })
}

type ErrorSink = Arc<dyn Fn(ExtensionErrorReport) + Send + Sync>;

/// Runs the loaded extensions' handlers.
pub(crate) struct ExtensionRunner {
    registry: ExtensionRegistry,
    on_error: Mutex<Option<ErrorSink>>,
}

impl ExtensionRunner {
    /// Load `extensions` in order against the session behind `core`.
    pub fn load(extensions: &[Arc<dyn Extension>], core: Weak<SessionCore>) -> Self {
        let mut registry = ExtensionRegistry::default();
        for extension in extensions {
            let mut api = ExtensionApi {
                registry: &mut registry,
                context: ExtensionContext {
                    core: core.clone(),
                    extension: Arc::from(extension.name()),
                },
            };
            extension.register(&mut api);
        }
        Self {
            registry,
            on_error: Mutex::new(None),
        }
    }

    pub fn set_error_sink(&self, sink: ErrorSink) {
        *self
            .on_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(sink);
    }

    pub fn report(&self, extension: &str, event: &'static str, error: impl ToString) {
        let sink = self
            .on_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(sink) = sink {
            sink(ExtensionErrorReport {
                extension: extension.to_string(),
                event,
                error: error.to_string(),
            });
        }
    }

    pub fn tools(&self) -> &[RegisteredTool] {
        &self.registry.tools
    }

    pub fn commands(&self) -> &[RegisteredCommand] {
        &self.registry.commands
    }

    pub fn providers(&self) -> &[ProviderRegistration] {
        &self.registry.providers
    }

    pub fn classifiers(&self) -> &[ClassifierRegistration] {
        &self.registry.classifiers
    }

    pub fn has<E: ExtensionEvent>(&self) -> bool {
        self.registry
            .handlers
            .get(&TypeId::of::<E>())
            .is_some_and(|handlers| !handlers.is_empty())
    }

    fn handlers<E: ExtensionEvent>(&self) -> Vec<(Arc<str>, Handler<E>)> {
        self.registry
            .handlers
            .get(&TypeId::of::<E>())
            .into_iter()
            .flatten()
            .filter_map(|(extension, handler)| {
                handler
                    .downcast_ref::<Handler<E>>()
                    .map(|handler| (extension.clone(), handler.clone()))
            })
            .collect()
    }

    fn context(&self, core: &Weak<SessionCore>, extension: &Arc<str>) -> ExtensionContext {
        ExtensionContext {
            core: core.clone(),
            extension: extension.clone(),
        }
    }

    /// Run every handler and return the answers that succeeded, in order. Failures are
    /// reported and skipped.
    pub async fn emit<E: ExtensionEvent>(
        &self,
        event: &E,
        core: &Weak<SessionCore>,
    ) -> Vec<E::Output> {
        let mut answers = Vec::new();
        for (extension, handler) in self.handlers::<E>() {
            match handler(event.clone(), self.context(core, &extension)).await {
                Ok(answer) => answers.push(answer),
                Err(error) => self.report(&extension, E::NAME, error),
            }
        }
        answers
    }

    /// Gate a tool call. The first block wins; input changes carry over to later
    /// handlers; a failing handler blocks the call.
    pub async fn tool_call(
        &self,
        mut event: ToolCall,
        core: &Weak<SessionCore>,
    ) -> HandlerResult<Option<BeforeToolCallResult>> {
        let mut changed = false;
        for (extension, handler) in self.handlers::<ToolCall>() {
            let Some(decision) = handler(event.clone(), self.context(core, &extension)).await?
            else {
                continue;
            };
            if let Some(input) = decision.args.clone() {
                event.input = input;
                changed = true;
            }
            if decision.block {
                return Ok(Some(decision));
            }
        }
        Ok(changed.then(|| BeforeToolCallResult::with_args(event.input)))
    }

    /// Chain result rewrites. Replacing the content alone keeps the other fields.
    pub async fn tool_result(
        &self,
        mut event: ToolResult,
        core: &Weak<SessionCore>,
    ) -> Option<AfterToolCallResult> {
        let mut merged: Option<AfterToolCallResult> = None;
        for (extension, handler) in self.handlers::<ToolResult>() {
            match handler(event.clone(), self.context(core, &extension)).await {
                Ok(Some(rewrite)) => {
                    let merged = merged.get_or_insert_with(AfterToolCallResult::default);
                    if let Some(content) = rewrite.content {
                        event.content = content.clone();
                        merged.content = Some(content);
                    }
                    if let Some(details) = rewrite.details {
                        event.details = Some(details.clone());
                        merged.details = Some(details);
                    }
                    if let Some(is_error) = rewrite.is_error {
                        event.is_error = is_error;
                        merged.is_error = Some(is_error);
                    }
                    if let Some(usage) = rewrite.usage {
                        event.usage = Some(usage);
                        merged.usage = Some(usage);
                    }
                    if rewrite.terminate.is_some() {
                        merged.terminate = rewrite.terminate;
                    }
                }
                Ok(None) => {}
                Err(error) => self.report(&extension, ToolResult::NAME, error),
            }
        }
        merged
    }

    /// Chain message replacements; a replacement of another kind is rejected.
    pub async fn message_end(
        &self,
        message: SessionMessage,
        core: &Weak<SessionCore>,
    ) -> SessionMessage {
        let mut current = message;
        for (extension, handler) in self.handlers::<MessageEnd>() {
            match handler(
                MessageEnd {
                    message: current.clone(),
                },
                self.context(core, &extension),
            )
            .await
            {
                Ok(Some(replacement)) if replacement.role() == current.role() => {
                    current = replacement
                }
                Ok(Some(_)) => self.report(
                    &extension,
                    MessageEnd::NAME,
                    "message_end handlers must return a message with the same role",
                ),
                Ok(None) => {}
                Err(error) => self.report(&extension, MessageEnd::NAME, error),
            }
        }
        current
    }

    pub fn has_context_handlers(&self) -> bool {
        self.has::<Context>() || self.has::<ContextWithSystem>()
    }

    /// Chain transcript rewrites for one request, as Pi does in two phases: [`Context`]
    /// handlers see the conversation alone and the prompt and tools are put back after
    /// each, then [`ContextWithSystem`] handlers see everything and are taken as returned.
    pub async fn context_messages(
        &self,
        messages: Vec<SessionMessage>,
        core: &Weak<SessionCore>,
    ) -> Vec<SessionMessage> {
        let mut current = messages;
        for (extension, handler) in self.handlers::<Context>() {
            let visible: Vec<SessionMessage> = current
                .iter()
                .filter(|message| message.role() != "system")
                .cloned()
                .collect();
            match handler(
                Context {
                    messages: visible.clone(),
                },
                self.context(core, &extension),
            )
            .await
            {
                Ok(Some(replacement)) if replacement != visible => {
                    current = with_current_system(&current, replacement);
                }
                Ok(_) => {}
                Err(error) => self.report(&extension, Context::NAME, error),
            }
        }
        for (extension, handler) in self.handlers::<ContextWithSystem>() {
            let had_leading_system = current
                .first()
                .is_some_and(|message| message.role() == "system");
            match handler(
                ContextWithSystem {
                    messages: current.clone(),
                },
                self.context(core, &extension),
            )
            .await
            {
                Ok(Some(replacement)) => {
                    if had_leading_system
                        && replacement
                            .first()
                            .is_none_or(|message| message.role() != "system")
                    {
                        self.report(&extension, ContextWithSystem::NAME, "the handler removed the leading system message, which carries the prompt and tools");
                    }
                    current = replacement;
                }
                Ok(None) => {}
                Err(error) => self.report(&extension, ContextWithSystem::NAME, error),
            }
        }
        current
    }

    pub async fn provider_payload(&self, payload: Value, core: &Weak<SessionCore>) -> Value {
        let mut current = payload;
        for (extension, handler) in self.handlers::<BeforeProviderRequest>() {
            match handler(
                BeforeProviderRequest {
                    payload: current.clone(),
                },
                self.context(core, &extension),
            )
            .await
            {
                Ok(Some(replacement)) => current = replacement,
                Ok(None) => {}
                Err(error) => self.report(&extension, BeforeProviderRequest::NAME, error),
            }
        }
        current
    }

    /// Ask about a user's shell command: the first answer wins. A failing handler is
    /// reported, and its error returned, so the command does not run, as in Pi.
    pub async fn user_bash(
        &self,
        event: UserBash,
        core: &Weak<SessionCore>,
    ) -> Result<Option<UserBashResult>, String> {
        for (extension, handler) in self.handlers::<UserBash>() {
            match handler(event.clone(), self.context(core, &extension)).await {
                Ok(None) => {}
                Ok(Some(answer)) => return Ok(Some(answer)),
                Err(error) => {
                    let message = error.to_string();
                    self.report(&extension, UserBash::NAME, error);
                    return Err(message);
                }
            }
        }
        Ok(None)
    }

    /// Chain header rewrites; each handler sees the headers the last one left.
    pub async fn provider_headers(
        &self,
        headers: BTreeMap<String, Option<String>>,
        core: &Weak<SessionCore>,
    ) -> BTreeMap<String, Option<String>> {
        let mut current = headers;
        for (extension, handler) in self.handlers::<BeforeProviderHeaders>() {
            match handler(
                BeforeProviderHeaders {
                    headers: current.clone(),
                },
                self.context(core, &extension),
            )
            .await
            {
                Ok(Some(replacement)) => current = replacement,
                Ok(None) => {}
                Err(error) => self.report(&extension, BeforeProviderHeaders::NAME, error),
            }
        }
        current
    }

    /// Chain input transforms until a handler handles the input.
    pub async fn input(&self, mut event: Input, core: &Weak<SessionCore>) -> InputAction {
        let mut transformed = false;
        for (extension, handler) in self.handlers::<Input>() {
            match handler(event.clone(), self.context(core, &extension)).await {
                Ok(InputAction::Handled) => return InputAction::Handled,
                Ok(InputAction::Transform { text, images }) => {
                    event.text = text;
                    event.images = images;
                    transformed = true;
                }
                Ok(InputAction::Continue) => {}
                Err(error) => self.report(&extension, Input::NAME, error),
            }
        }
        if transformed {
            InputAction::Transform {
                text: event.text,
                images: event.images,
            }
        } else {
            InputAction::Continue
        }
    }

    /// Chain prompt option changes and collect messages; the last prompt override wins.
    pub async fn before_agent_start(
        &self,
        mut event: BeforeAgentStart,
        core: &Weak<SessionCore>,
    ) -> (SystemPromptOptions, Vec<CustomMessageDraft>, Option<String>) {
        let mut messages = Vec::new();
        let mut system_prompt = None;
        for (extension, handler) in self.handlers::<BeforeAgentStart>() {
            match handler(event.clone(), self.context(core, &extension)).await {
                Ok(result) => {
                    // Options that cannot build a prompt are refused, not kept for later prompts.
                    if let Some(options) = result.options {
                        match crate::system_prompt::build_sections(&options) {
                            Ok(_) => event.options = options,
                            Err(error) => self.report(&extension, BeforeAgentStart::NAME, error),
                        }
                    }
                    messages.extend(result.message);
                    if let Some(prompt) = result.system_prompt {
                        event.system_prompt = prompt.clone();
                        system_prompt = Some(prompt);
                    }
                }
                Err(error) => self.report(&extension, BeforeAgentStart::NAME, error),
            }
        }
        (event.options, messages, system_prompt)
    }

    pub async fn before_compact(
        &self,
        event: SessionBeforeCompact,
        core: &Weak<SessionCore>,
    ) -> BeforeCompactResult {
        let mut outcome = BeforeCompactResult::default();
        for answer in self
            .emit_until_cancel(event, core, |answer: &BeforeCompactResult| answer.cancel)
            .await
        {
            outcome.cancel |= answer.cancel;
            outcome.compaction = answer.compaction.or(outcome.compaction);
        }
        outcome
    }

    /// Ask the handlers of a boundary event in turn, as Pi's `emitBoundary`: `event`
    /// makes the event from what was asked so far, and `context` previews it, which
    /// fails when the entries would not apply. A failing preview is reported, and the
    /// last decides: when it failed, nothing is asked for.
    pub async fn emit_boundary<E>(
        &self,
        core: &Weak<SessionCore>,
        event: impl Fn(Vec<BoundaryDraft>, bool, BoundaryContext) -> E,
        context: impl Fn(&[BoundaryDraft]) -> Result<BoundaryContext, String>,
    ) -> BoundaryDispatch
    where
        E: ExtensionEvent<Output = BoundaryResult>,
    {
        let mut entries = Vec::new();
        let mut continue_run = false;
        let mut preview = context(&entries).unwrap_or_default();
        let mut valid = true;
        for (extension, handler) in self.handlers::<E>() {
            let current = event(entries.clone(), continue_run, preview.clone());
            match handler(current, self.context(core, &extension)).await {
                Ok(answer) => {
                    if let Some(next) = answer.entries {
                        entries = next;
                    }
                    if let Some(next) = answer.continue_run {
                        continue_run = next;
                    }
                }
                Err(error) => self.report(&extension, E::NAME, error),
            }
            match context(&entries) {
                Ok(next) => {
                    preview = next;
                    valid = true;
                }
                Err(error) => {
                    valid = false;
                    self.report(
                        &extension,
                        E::NAME,
                        format!("Invalid boundary entries: {error}"),
                    );
                }
            }
        }
        if valid {
            BoundaryDispatch {
                entries,
                continue_run,
            }
        } else {
            BoundaryDispatch {
                entries: Vec::new(),
                continue_run: false,
            }
        }
    }

    pub async fn before_switch(
        &self,
        event: SessionBeforeSwitch,
        core: &Weak<SessionCore>,
    ) -> BeforeSwitchResult {
        let cancel = self
            .emit_until_cancel(event, core, |answer: &BeforeSwitchResult| answer.cancel)
            .await
            .iter()
            .any(|answer| answer.cancel);
        BeforeSwitchResult { cancel }
    }

    pub async fn before_fork(
        &self,
        event: SessionBeforeFork,
        core: &Weak<SessionCore>,
    ) -> BeforeForkResult {
        let cancel = self
            .emit_until_cancel(event, core, |answer: &BeforeForkResult| answer.cancel)
            .await
            .iter()
            .any(|answer| answer.cancel);
        BeforeForkResult { cancel }
    }

    pub async fn before_tree(
        &self,
        event: SessionBeforeTree,
        core: &Weak<SessionCore>,
    ) -> BeforeTreeResult {
        let mut outcome = BeforeTreeResult::default();
        for answer in self
            .emit_until_cancel(event, core, |answer: &BeforeTreeResult| answer.cancel)
            .await
        {
            outcome.cancel |= answer.cancel;
            outcome.summary = answer.summary.or(outcome.summary);
            outcome.custom_instructions =
                answer.custom_instructions.or(outcome.custom_instructions);
            outcome.replace_instructions =
                answer.replace_instructions.or(outcome.replace_instructions);
            outcome.label = answer.label.or(outcome.label);
        }
        outcome
    }

    async fn emit_until_cancel<E: ExtensionEvent>(
        &self,
        event: E,
        core: &Weak<SessionCore>,
        cancels: impl Fn(&E::Output) -> bool,
    ) -> Vec<E::Output> {
        let mut answers = Vec::new();
        for (extension, handler) in self.handlers::<E>() {
            match handler(event.clone(), self.context(core, &extension)).await {
                Ok(answer) => {
                    let stop = cancels(&answer);
                    answers.push(answer);
                    if stop {
                        break;
                    }
                }
                Err(error) => self.report(&extension, E::NAME, error),
            }
        }
        answers
    }

    /// The completions the command `name` offers for `prefix`; `None` without any.
    pub async fn argument_completions(
        &self,
        name: &str,
        prefix: &str,
    ) -> Option<Vec<AutocompleteItem>> {
        let command = self
            .registry
            .commands
            .iter()
            .find(|command| command.name == name)?;
        let completions = command.completions.as_ref()?;
        completions(prefix.to_string()).await
    }

    /// Run a command; `None` when no extension registered it.
    pub async fn run_command(
        &self,
        name: &str,
        args: String,
        core: &Weak<SessionCore>,
    ) -> Option<HandlerResult<()>> {
        let command = self
            .registry
            .commands
            .iter()
            .find(|command| command.name == name)?;
        let context = CommandContext {
            context: ExtensionContext {
                core: core.clone(),
                extension: Arc::from(command.extension.as_str()),
            },
        };
        Some((command.handler)(args, context).await)
    }
}
