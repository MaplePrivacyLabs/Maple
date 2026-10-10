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
//! - rewrites ([`ToolResult`], [`MessageEnd`], [`Context`], [`BeforeProviderRequest`],
//!   [`Input`], [`BeforeAgentStart`]) chain, each handler seeing the previous one's output;
//! - `before` events ([`SessionBeforeCompact`], [`SessionBeforeTree`]) stop at the first
//!   cancellation.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use futures_util::FutureExt;
use futures_util::future::BoxFuture;
use pi_agent_core::{AfterToolCallResult, AgentEvent, AgentTool, BeforeToolCallResult};
use pi_ai::{Content, ImageContent, Model, StreamFn, ThinkingLevel, Usage};
use serde_json::Value;

use crate::agent_session::SessionCore;
use crate::compaction::{CompactionPreparation, CompactionResult};
use crate::messages::SessionMessage;
use crate::system_prompt::SystemPromptOptions;

pub type ExtensionError = Box<dyn std::error::Error + Send + Sync>;
pub type HandlerResult<T> = Result<T, ExtensionError>;

/// An event extensions can handle. A handler answers with `Output`; the default answer
/// changes nothing.
pub trait ExtensionEvent: Clone + Send + Sync + 'static {
    const NAME: &'static str;
    type Output: Default + Send + 'static;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionStartReason {
    Startup,
    Fork,
}

#[derive(Clone, Debug)]
pub struct SessionStart {
    pub reason: SessionStartReason,
}

#[derive(Clone, Debug)]
pub struct SessionShutdown;

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

/// A prompt about to start a run.
#[derive(Clone, Debug)]
pub struct BeforeAgentStart {
    pub prompt: String,
    pub system_prompt: String,
    pub options: SystemPromptOptions,
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
    /// The prompt options for this and later prompts.
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

/// The transcript about to be sent, system messages included. A replacement must keep
/// the leading system message, which carries the prompt and tools.
#[derive(Clone, Debug)]
pub struct Context {
    pub messages: Vec<SessionMessage>,
}

/// A provider request body about to be sent.
#[derive(Clone, Debug)]
pub struct BeforeProviderRequest {
    pub payload: Value,
}

#[derive(Clone, Debug)]
pub struct SessionBeforeCompact {
    pub preparation: CompactionPreparation,
    pub custom_instructions: Option<String>,
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
}

#[derive(Clone, Debug)]
pub struct SessionBeforeTree {
    pub target_id: String,
    pub old_leaf_id: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct BeforeTreeResult {
    pub cancel: bool,
    /// Use this summary of the branch being left instead of generating one.
    pub summary: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SessionTree {
    pub new_leaf_id: Option<String>,
    pub old_leaf_id: Option<String>,
    pub summary_entry_id: Option<String>,
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
    Input => InputAction, "input";
    BeforeAgentStart => BeforeAgentStartResult, "before_agent_start";
    AgentEventSeen => (), "agent_event";
    MessageEnd => Option<SessionMessage>, "message_end";
    ToolCall => Option<BeforeToolCallResult>, "tool_call";
    ToolResult => Option<AfterToolCallResult>, "tool_result";
    Context => Option<Vec<SessionMessage>>, "context";
    BeforeProviderRequest => Option<Value>, "before_provider_request";
    SessionBeforeCompact => BeforeCompactResult, "session_before_compact";
    SessionCompact => (), "session_compact";
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
    Arc<dyn Fn(String, ExtensionContext) -> BoxFuture<'static, HandlerResult<()>> + Send + Sync>;

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

#[derive(Clone)]
pub struct RegisteredCommand {
    pub name: String,
    pub description: String,
    pub extension: String,
    handler: CommandHandler,
}

/// Models and an API implementation an extension provides.
#[derive(Clone)]
pub struct ProviderRegistration {
    pub models: Vec<Model>,
    pub api: Option<(String, Arc<dyn StreamFn>)>,
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

    fn notify(&self, _message: &str) {}

    async fn confirm(&self, _title: &str, _message: &str) -> bool {
        false
    }

    /// The index of the chosen option.
    async fn select(&self, _title: &str, _options: &[String]) -> Option<usize> {
        None
    }

    async fn input(&self, _title: &str, _placeholder: Option<&str>) -> Option<String> {
        None
    }
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

/// Handlers by event type: each holds the extension name and a boxed `Handler<E>`.
type HandlerTable = HashMap<TypeId, Vec<(Arc<str>, Box<dyn Any + Send + Sync>)>>;

#[derive(Default)]
pub(crate) struct ExtensionRegistry {
    handlers: HandlerTable,
    pub tools: Vec<RegisteredTool>,
    pub commands: Vec<RegisteredCommand>,
    pub providers: Vec<ProviderRegistration>,
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
        F: Fn(String, ExtensionContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HandlerResult<()>> + Send + 'static,
    {
        self.registry.commands.push(RegisteredCommand {
            name: name.to_string(),
            description: description.to_string(),
            extension: self.context.extension.to_string(),
            handler: Arc::new(move |args, context| handler(args, context).boxed()),
        });
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

    /// Chain transcript rewrites for one request.
    pub async fn context_messages(
        &self,
        messages: Vec<SessionMessage>,
        core: &Weak<SessionCore>,
    ) -> Vec<SessionMessage> {
        let mut current = messages;
        for (extension, handler) in self.handlers::<Context>() {
            let had_leading_system = current
                .first()
                .is_some_and(|message| message.role() == "system");
            match handler(
                Context {
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
                        self.report(&extension, Context::NAME, "the handler removed the leading system message, which carries the prompt and tools");
                    }
                    current = replacement;
                }
                Ok(None) => {}
                Err(error) => self.report(&extension, Context::NAME, error),
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
        let context = ExtensionContext {
            core: core.clone(),
            extension: Arc::from(command.extension.as_str()),
        };
        Some((command.handler)(args, context).await)
    }
}
