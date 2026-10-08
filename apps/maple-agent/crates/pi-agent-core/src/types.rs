//! Agent messages, hooks, executable tools and events from `packages/agent/src/types.ts`.
//!
//! Shared handles express JavaScript object and array identity. A `clone` keeps
//! the same object; `snapshot` or `copy_array` makes the explicit source copy.

pub use pi_ai::env::{CancellationToken, PiEnv};
use pi_ai::types::{
    AssistantMessage, AssistantMessageEvent, Context, Message, Model, Schema,
    SharedAssistantMessage, SimpleStreamOptions, SystemMessage, Tool, ToolCall, ToolResultMessage,
    TranscriptContext, Usage, UserContent, UserMessage,
};
pub use pi_ai::types::{
    JsObject, JsString, JsValue, ModelThinkingLevel as ThinkingLevel, RawMessage,
};
use pi_ai::utils::event_stream::AssistantMessageEventStream;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};

pub type AgentFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;
pub type AgentResult<T> = Result<T, AgentError>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentError {
    pub name: JsString,
    pub message: JsString,
}
impl AgentError {
    pub fn new(message: impl Into<JsString>) -> Self {
        Self::with_name("Error", message)
    }
    pub fn with_name(name: impl Into<JsString>, message: impl Into<JsString>) -> Self {
        Self {
            name: name.into(),
            message: message.into(),
        }
    }
    pub fn type_error(message: impl Into<JsString>) -> Self {
        Self::with_name("TypeError", message)
    }
}
impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message.to_string_lossy())
    }
}
impl std::error::Error for AgentError {}
impl From<&str> for AgentError {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}
impl From<String> for AgentError {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

pub struct Shared<T>(Arc<Mutex<T>>);
/// Non-owning back-reference for callbacks stored inside their own shared owner.
pub struct WeakShared<T>(Weak<Mutex<T>>);
impl<T> WeakShared<T> {
    pub fn upgrade(&self) -> Option<Shared<T>> {
        self.0.upgrade().map(Shared)
    }
}
impl<T> Clone for WeakShared<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<T> Shared<T> {
    pub fn new(value: T) -> Self {
        Self(Arc::new(Mutex::new(value)))
    }
    pub fn read<R>(&self, read: impl FnOnce(&T) -> R) -> R {
        read(&self.0.lock().expect("agent shared value poisoned"))
    }
    pub fn update<R>(&self, update: impl FnOnce(&mut T) -> R) -> R {
        update(&mut self.0.lock().expect("agent shared value poisoned"))
    }
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub fn downgrade(&self) -> WeakShared<T> {
        WeakShared(Arc::downgrade(&self.0))
    }
}
impl<T: Clone> Shared<T> {
    pub fn snapshot(&self) -> T {
        self.read(Clone::clone)
    }
}
impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<T: Default> Default for Shared<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}
impl<T: fmt::Debug> fmt::Debug for Shared<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.read(|v| v.fmt(f))
    }
}
impl<T: Serialize> Serialize for Shared<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.read(|v| v.serialize(serializer))
    }
}
impl<'de, T: Deserialize<'de>> Deserialize<'de> for Shared<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        T::deserialize(d).map(Self::new)
    }
}
impl<T: Clone + PartialEq> PartialEq for Shared<T> {
    fn eq(&self, other: &Self) -> bool {
        self.ptr_eq(other) || self.snapshot() == other.snapshot()
    }
}
impl<T> Shared<Vec<T>> {
    pub fn push(&self, value: T) {
        self.update(|v| v.push(value));
    }
    pub fn len(&self) -> usize {
        self.read(Vec::len)
    }
    pub fn is_empty(&self) -> bool {
        self.read(Vec::is_empty)
    }
    pub fn clear(&self) {
        self.update(Vec::clear);
    }
}
impl<T: Clone> Shared<Vec<T>> {
    pub fn copy_array(&self) -> Self {
        Self::new(self.snapshot())
    }
    pub fn get(&self, index: usize) -> Option<T> {
        self.read(|v| v.get(index).cloned())
    }
    pub fn last(&self) -> Option<T> {
        self.read(|v| v.last().cloned())
    }
    pub fn extend(&self, values: impl IntoIterator<Item = T>) {
        self.update(|v| v.extend(values));
    }
    pub fn replace_last(&self, value: T) {
        self.update(|v| *v.last_mut().expect("agent message array is empty") = value);
    }
}
impl<T> From<Vec<T>> for Shared<Vec<T>> {
    fn from(value: Vec<T>) -> Self {
        Self::new(value)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
#[allow(clippy::large_enum_variant)]
pub enum AgentMessageValue {
    System(SystemMessage),
    User(UserMessage),
    Assistant(SharedAssistantMessage),
    ToolResult(Shared<ToolResultMessage>),
    Custom(RawMessage),
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentMessage(pub Shared<AgentMessageValue>);
impl AgentMessage {
    pub fn new(value: AgentMessageValue) -> Self {
        Self(Shared::new(value))
    }
    pub fn snapshot(&self) -> AgentMessageValue {
        self.0.snapshot()
    }
    pub fn update<R>(&self, update: impl FnOnce(&mut AgentMessageValue) -> R) -> R {
        self.0.update(update)
    }
    pub fn ptr_eq(&self, other: &Self) -> bool {
        self.0.ptr_eq(&other.0)
            || match (self.snapshot(), other.snapshot()) {
                (AgentMessageValue::Assistant(a), AgentMessageValue::Assistant(b)) => a.ptr_eq(&b),
                (AgentMessageValue::Custom(a), AgentMessageValue::Custom(b)) => a.ptr_eq(&b),
                (AgentMessageValue::ToolResult(a), AgentMessageValue::ToolResult(b)) => {
                    a.ptr_eq(&b)
                }
                _ => false,
            }
    }
    pub fn role(&self) -> JsString {
        self.0.read(|v| match v {
            AgentMessageValue::System(_) => "system".into(),
            AgentMessageValue::User(_) => "user".into(),
            AgentMessageValue::Assistant(_) => "assistant".into(),
            AgentMessageValue::ToolResult(_) => "toolResult".into(),
            AgentMessageValue::Custom(v) => v.role(),
        })
    }
    pub fn as_llm(&self) -> Option<Message> {
        self.0.read(|v| match v {
            AgentMessageValue::System(v) => Some(Message::System(v.clone())),
            AgentMessageValue::User(v) => Some(Message::User(v.clone())),
            AgentMessageValue::Assistant(v) => Some(Message::Assistant(v.snapshot())),
            AgentMessageValue::ToolResult(v) => Some(Message::ToolResult(v.snapshot())),
            AgentMessageValue::Custom(v)
                if matches!(
                    v.role().as_str(),
                    Some("system" | "user" | "assistant" | "toolResult")
                ) =>
            {
                Some(Message::Raw(v.clone()))
            }
            AgentMessageValue::Custom(_) => None,
        })
    }
    pub fn assistant(&self) -> Option<SharedAssistantMessage> {
        self.0.read(|v| match v {
            AgentMessageValue::Assistant(v) => Some(v.clone()),
            _ => None,
        })
    }
    pub fn system(&self) -> Option<SystemMessage> {
        self.0.read(|v| match v {
            AgentMessageValue::System(v) => Some(v.clone()),
            _ => None,
        })
    }
}
impl From<Message> for AgentMessage {
    fn from(value: Message) -> Self {
        match value {
            Message::System(v) => v.into(),
            Message::User(v) => v.into(),
            Message::Assistant(v) => v.into(),
            Message::ToolResult(v) => v.into(),
            Message::Raw(v) => Self::new(AgentMessageValue::Custom(v)),
        }
    }
}
impl From<SystemMessage> for AgentMessage {
    fn from(value: SystemMessage) -> Self {
        Self::new(AgentMessageValue::System(value))
    }
}
impl From<UserMessage> for AgentMessage {
    fn from(value: UserMessage) -> Self {
        Self::new(AgentMessageValue::User(value))
    }
}
impl From<AssistantMessage> for AgentMessage {
    fn from(value: AssistantMessage) -> Self {
        SharedAssistantMessage::new(value).into()
    }
}
impl From<SharedAssistantMessage> for AgentMessage {
    fn from(value: SharedAssistantMessage) -> Self {
        Self::new(AgentMessageValue::Assistant(value))
    }
}
impl From<ToolResultMessage> for AgentMessage {
    fn from(value: ToolResultMessage) -> Self {
        Self::new(AgentMessageValue::ToolResult(Shared::new(value)))
    }
}
impl From<SharedToolResultMessage> for AgentMessage {
    fn from(value: SharedToolResultMessage) -> Self {
        Self::new(AgentMessageValue::ToolResult(value))
    }
}
impl From<JsObject> for AgentMessage {
    fn from(value: JsObject) -> Self {
        Self::new(AgentMessageValue::Custom(value.into()))
    }
}
pub type AgentMessages = Shared<Vec<AgentMessage>>;
pub type AgentToolCall = ToolCall;
pub type SharedAgentTool = Shared<AgentTool>;
pub type AgentTools = Shared<Vec<SharedAgentTool>>;
pub type SharedToolResultMessage = Shared<ToolResultMessage>;
pub type SharedArgs = pi_ai::types::SharedJsValue;
pub type SharedToolResult = Shared<AgentToolResult>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolExecutionMode {
    Sequential,
    #[default]
    Parallel,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QueueMode {
    All,
    #[default]
    OneAtATime,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReplayPolicy {
    Never,
    Safe,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum AgentTurnDecision {
    Continue,
    End,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BeforeToolCallResult {
    pub block: Option<bool>,
    pub reason: Option<JsString>,
    pub terminate: Option<bool>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AfterToolCallResult {
    pub content: Option<Vec<UserContent>>,
    pub details: Option<JsValue>,
    pub structured_content: Option<JsValue>,
    pub is_error: Option<bool>,
    pub usage: Option<Usage>,
    pub terminate: Option<bool>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentToolResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<UserContent>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<JsValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<JsValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminate: Option<bool>,
}
#[derive(Clone, Debug)]
pub struct BeforeToolCallContext {
    pub assistant_message: SharedAssistantMessage,
    pub tool_call: AgentToolCall,
    pub args: SharedArgs,
    pub context: AgentContext,
}
#[derive(Clone, Debug)]
pub struct AfterToolCallContext {
    pub assistant_message: SharedAssistantMessage,
    pub tool_call: AgentToolCall,
    pub args: SharedArgs,
    pub result: SharedToolResult,
    pub is_error: bool,
    pub context: AgentContext,
}
#[derive(Clone, Debug)]
pub struct AgentTurnContext {
    pub message: SharedAssistantMessage,
    pub tool_results: Vec<SharedToolResultMessage>,
    pub context: AgentContext,
    pub new_messages: AgentMessages,
}
pub type PrepareNextTurnContext = AgentTurnContext;
#[derive(Clone, Debug, Default)]
pub struct AgentLoopTurnUpdate {
    pub context: Option<AgentContext>,
    pub messages: Option<Vec<AgentMessage>>,
    pub model: Option<Shared<Model>>,
    pub thinking_level: Option<ThinkingLevel>,
}
#[derive(Clone, Debug, Default)]
pub struct AgentRequestUpdate {
    pub context: Option<AgentContext>,
    pub model: Option<Shared<Model>>,
    pub thinking_level: Option<ThinkingLevel>,
}
#[derive(Clone, Debug)]
pub struct PrepareRequestContext {
    pub context: AgentContext,
    pub model: Shared<Model>,
    pub thinking_level: ThinkingLevel,
}

pub type StreamFn = Arc<
    dyn Fn(
            Model,
            TranscriptContext,
            Option<SimpleStreamOptions>,
        ) -> AgentFuture<AgentResult<AssistantMessageEventStream>>
        + Send
        + Sync,
>;
pub type ConvertToLlm =
    Arc<dyn Fn(AgentMessages) -> AgentFuture<AgentResult<Vec<Message>>> + Send + Sync>;
pub type TransformContext = Arc<
    dyn Fn(AgentMessages, Option<CancellationToken>) -> AgentFuture<AgentResult<AgentMessages>>
        + Send
        + Sync,
>;
pub type GetApiKey = Arc<dyn Fn(String) -> AgentFuture<AgentResult<Option<String>>> + Send + Sync>;
pub type FinishTurn = Arc<
    dyn Fn(
            AgentTurnContext,
            Option<CancellationToken>,
        ) -> AgentFuture<AgentResult<Option<AgentTurnDecision>>>
        + Send
        + Sync,
>;
pub type PrepareRequest = Arc<
    dyn Fn(
            PrepareRequestContext,
            Option<CancellationToken>,
        ) -> AgentFuture<AgentResult<Option<AgentRequestUpdate>>>
        + Send
        + Sync,
>;
pub type PrepareNextTurn = Arc<
    dyn Fn(PrepareNextTurnContext) -> AgentFuture<AgentResult<Option<AgentLoopTurnUpdate>>>
        + Send
        + Sync,
>;
pub type GetMessages = Arc<dyn Fn() -> AgentFuture<AgentResult<Vec<AgentMessage>>> + Send + Sync>;
pub type BeforeToolCall = Arc<
    dyn Fn(
            BeforeToolCallContext,
            Option<CancellationToken>,
        ) -> AgentFuture<AgentResult<Option<BeforeToolCallResult>>>
        + Send
        + Sync,
>;
pub type AfterToolCall = Arc<
    dyn Fn(
            AfterToolCallContext,
            Option<CancellationToken>,
        ) -> AgentFuture<AgentResult<Option<AfterToolCallResult>>>
        + Send
        + Sync,
>;
pub type PrepareArguments = Arc<dyn Fn(SharedArgs) -> AgentResult<SharedArgs> + Send + Sync>;
pub type AgentToolUpdateCallback = Arc<dyn Fn(AgentToolResult) + Send + Sync>;
pub type ToolExecute = Arc<
    dyn Fn(
            JsString,
            SharedArgs,
            Option<CancellationToken>,
            Option<AgentToolUpdateCallback>,
        ) -> AgentFuture<AgentResult<AgentToolResult>>
        + Send
        + Sync,
>;
pub type AgentEventSink = Arc<dyn Fn(AgentEvent) -> AgentFuture<AgentResult<()>> + Send + Sync>;
pub type ToolUpdateSink =
    Arc<dyn Fn(AgentToolResult) -> AgentFuture<AgentResult<()>> + Send + Sync>;

#[derive(Clone)]
pub struct AgentTool {
    pub tool: Tool,
    pub label: JsString,
    pub prepare_arguments: Option<PrepareArguments>,
    pub output_schema: Option<Schema>,
    pub execute: ToolExecute,
    pub replay: Option<ReplayPolicy>,
    pub execution_mode: Option<ToolExecutionMode>,
}
impl Deref for AgentTool {
    type Target = Tool;
    fn deref(&self) -> &Tool {
        &self.tool
    }
}
impl DerefMut for AgentTool {
    fn deref_mut(&mut self) -> &mut Tool {
        &mut self.tool
    }
}
impl fmt::Debug for AgentTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentTool")
            .field("tool", &self.tool)
            .field("label", &self.label)
            .field("execution_mode", &self.execution_mode)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Debug, Default)]
pub struct AgentContext {
    pub messages: AgentMessages,
    pub tools: Option<AgentTools>,
}
impl AgentContext {
    pub fn new(messages: Vec<AgentMessage>, tools: Option<Vec<AgentTool>>) -> Self {
        Self {
            messages: messages.into(),
            tools: tools.map(|tools| Shared::new(tools.into_iter().map(Shared::new).collect())),
        }
    }
    pub fn llm_messages(&self) -> Vec<Message> {
        self.messages
            .read(|v| v.iter().filter_map(AgentMessage::as_llm).collect())
    }
    pub fn to_context(&self) -> Context {
        Context {
            messages: self.llm_messages(),
            ..Context::default()
        }
    }
}
#[derive(Clone)]
pub struct AgentLoopConfig {
    pub options: SimpleStreamOptions,
    pub model: Shared<Model>,
    pub convert_to_llm: ConvertToLlm,
    pub transform_context: Option<TransformContext>,
    pub get_api_key: Option<GetApiKey>,
    pub finish_turn: Option<FinishTurn>,
    pub prepare_request: Option<PrepareRequest>,
    pub prepare_next_turn: Option<PrepareNextTurn>,
    pub get_steering_messages: Option<GetMessages>,
    pub get_follow_up_messages: Option<GetMessages>,
    pub tool_execution: Option<ToolExecutionMode>,
    pub before_tool_call: Option<BeforeToolCall>,
    pub after_tool_call: Option<AfterToolCall>,
}
impl AgentLoopConfig {
    pub fn new(model: Model, convert_to_llm: ConvertToLlm) -> Self {
        Self {
            model: Shared::new(model),
            convert_to_llm,
            options: SimpleStreamOptions::default(),
            transform_context: None,
            get_api_key: None,
            finish_turn: None,
            prepare_request: None,
            prepare_next_turn: None,
            get_steering_messages: None,
            get_follow_up_messages: None,
            tool_execution: None,
            before_tool_call: None,
            after_tool_call: None,
        }
    }
}
impl Deref for AgentLoopConfig {
    type Target = SimpleStreamOptions;
    fn deref(&self) -> &Self::Target {
        &self.options
    }
}
impl DerefMut for AgentLoopConfig {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.options
    }
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentToolCallOutcome {
    pub tool_call: AgentToolCall,
    pub result: SharedToolResult,
    pub is_error: bool,
}
#[derive(Clone, Debug, Serialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
#[allow(clippy::large_enum_variant)] // Keep Pi event payloads directly accessible, like pi-ai events.
pub enum AgentEvent {
    AgentStart,
    AgentEnd {
        messages: AgentMessages,
    },
    TurnStart,
    TurnEnd {
        message: AgentMessage,
        tool_results: Vec<SharedToolResultMessage>,
    },
    MessageStart {
        message: AgentMessage,
    },
    MessageUpdate {
        message: AgentMessage,
        assistant_message_event: AssistantMessageEvent,
    },
    MessageEnd {
        message: AgentMessage,
    },
    ToolExecutionStart {
        tool_call_id: JsString,
        tool_name: JsString,
        args: SharedArgs,
    },
    ToolExecutionUpdate {
        tool_call_id: JsString,
        tool_name: JsString,
        args: SharedArgs,
        partial_result: AgentToolResult,
    },
    ToolExecutionEnd {
        tool_call_id: JsString,
        tool_name: JsString,
        result: SharedToolResult,
        is_error: bool,
    },
}
impl AgentEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::AgentStart => "agent_start",
            Self::AgentEnd { .. } => "agent_end",
            Self::TurnStart => "turn_start",
            Self::TurnEnd { .. } => "turn_end",
            Self::MessageStart { .. } => "message_start",
            Self::MessageUpdate { .. } => "message_update",
            Self::MessageEnd { .. } => "message_end",
            Self::ToolExecutionStart { .. } => "tool_execution_start",
            Self::ToolExecutionUpdate { .. } => "tool_execution_update",
            Self::ToolExecutionEnd { .. } => "tool_execution_end",
        }
    }
}

/// Adapt a synchronous Pi provider function into the agent's sync-or-async stream contract.
pub fn synchronous_stream(stream: pi_ai::types::StreamFunction<SimpleStreamOptions>) -> StreamFn {
    Arc::new(move |model, context, options| {
        let response = stream(model, context, options);
        Box::pin(async move { Ok(response) })
    })
}
