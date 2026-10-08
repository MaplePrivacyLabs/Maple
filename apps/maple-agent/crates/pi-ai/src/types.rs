//! Chat contracts from Pi v1.0.4 `packages/ai/src/types.ts`.
//!
//! API and provider identifiers remain open strings. Other API implementations,
//! image generation, classifiers, telemetry, and nested tool execution are cut.
//! Nested tool-call records remain readable for historical session compaction.

use crate::env::CancellationToken;
use crate::utils::diagnostics::AssistantMessageDiagnostic;
use crate::utils::event_stream::AssistantMessageEventStream;
pub use crate::utils::js_value::{JsObject, JsString, JsValue};
use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{
    fmt,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};

pub type Api = String;
pub type ProviderId = String;
pub type JsonValue = JsValue;
pub type JsonObject = JsObject;
pub type SamplingParams = JsonObject;
pub type ProviderEnv = IndexMap<String, String>;
pub type ProviderHeaders = IndexMap<String, Option<String>>;
pub type ThinkingLevelMap = IndexMap<ModelThinkingLevel, Option<String>>;
pub type SamplingParamsByThinkingLevel = IndexMap<ModelThinkingLevel, SamplingParams>;
pub type ModelPromptCache = IndexMap<CacheRetention, f64>;
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

macro_rules! string_enum {
    ($name:ident { $first:ident => $first_value:literal $(, $variant:ident => $value:literal)* $(,)? }) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
        pub enum $name { #[serde(rename = $first_value)] $first, $(#[serde(rename = $value)] $variant,)* }
        impl $name { pub const fn as_str(self) -> &'static str { match self { Self::$first => $first_value, $(Self::$variant => $value,)* } } }
        impl Default for $name { fn default() -> Self { Self::$first } }
        impl fmt::Display for $name { fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(self.as_str()) } }
    };
}
string_enum!(ThinkingLevel { Minimal => "minimal", Low => "low", Medium => "medium", High => "high", Xhigh => "xhigh", Max => "max" });
string_enum!(ModelThinkingLevel { Off => "off", Minimal => "minimal", Low => "low", Medium => "medium", High => "high", Xhigh => "xhigh", Max => "max" });
impl From<ThinkingLevel> for ModelThinkingLevel {
    fn from(value: ThinkingLevel) -> Self {
        match value {
            ThinkingLevel::Minimal => Self::Minimal,
            ThinkingLevel::Low => Self::Low,
            ThinkingLevel::Medium => Self::Medium,
            ThinkingLevel::High => Self::High,
            ThinkingLevel::Xhigh => Self::Xhigh,
            ThinkingLevel::Max => Self::Max,
        }
    }
}
string_enum!(ToolChoice { Auto => "auto", None => "none" });
string_enum!(CacheRetention { None => "none", Short => "short", Long => "long" });
string_enum!(Transport { Sse => "sse", Websocket => "websocket", WebsocketCached => "websocket-cached", Auto => "auto" });
string_enum!(SessionAffinityFormat { Openai => "openai", OpenaiNosession => "openai-nosession", Openrouter => "openrouter" });
string_enum!(ThinkingTokenBudgetField { ThinkingTokenBudget => "thinking_token_budget", ThinkingBudget => "thinking_budget", ThinkingBudgetTokens => "thinking_budget_tokens" });
string_enum!(StopReason { Pending => "pending", Stop => "stop", Length => "length", ToolUse => "toolUse", Error => "error", Aborted => "aborted", Deferred => "deferred" });
string_enum!(DoneReason { Stop => "stop", Length => "length", ToolUse => "toolUse", Deferred => "deferred" });
string_enum!(ErrorReason { Error => "error", Aborted => "aborted" });
string_enum!(TextType { Text => "text" });
string_enum!(ThinkingType { Thinking => "thinking" });
string_enum!(ImageType { Image => "image" });
string_enum!(ToolCallType { ToolCall => "toolCall" });
string_enum!(SystemRole { System => "system" });
string_enum!(UserRole { User => "user" });
string_enum!(AssistantRole { Assistant => "assistant" });
string_enum!(ToolResultRole { ToolResult => "toolResult" });
string_enum!(ModelType { Chat => "chat" });
string_enum!(InputModality { Text => "text", Image => "image" });
string_enum!(TextPhase { Commentary => "commentary", FinalAnswer => "final_answer" });

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingBudgets {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimal: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub low: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub medium: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub high: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextSignatureV1 {
    pub v: u8,
    pub id: JsString,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<TextPhase>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextContent {
    pub r#type: TextType,
    pub text: JsString,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_signature: Option<JsString>,
}
impl TextContent {
    pub fn new(text: impl Into<JsString>) -> Self {
        Self {
            text: text.into(),
            ..Self::default()
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingContent {
    pub r#type: ThinkingType,
    pub thinking: JsString,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_signature: Option<JsString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redacted: Option<bool>,
}
impl ThinkingContent {
    pub fn new(thinking: impl Into<JsString>) -> Self {
        Self {
            thinking: thinking.into(),
            ..Self::default()
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageContent {
    pub r#type: ImageType,
    pub data: String,
    pub mime_type: String,
}
impl ImageContent {
    pub fn new(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self {
            r#type: ImageType::Image,
            data: data.into(),
            mime_type: mime_type.into(),
        }
    }
}
/// Shared identity for JavaScript tool arguments, including malformed scalar payloads.
/// Cloning preserves the object reference; assigning a newly parsed wrapper replaces it.
#[derive(Clone)]
pub struct SharedJsValue(Arc<Mutex<JsValue>>);
impl SharedJsValue {
    pub fn new(value: impl Into<JsValue>) -> Self {
        Self(Arc::new(Mutex::new(value.into())))
    }
    pub fn snapshot(&self) -> JsValue {
        self.read(Clone::clone)
    }
    pub fn read<R>(&self, read: impl FnOnce(&JsValue) -> R) -> R {
        read(&self.0.lock().expect("shared JavaScript value poisoned"))
    }
    pub fn update<R>(&self, update: impl FnOnce(&mut JsValue) -> R) -> R {
        update(&mut self.0.lock().expect("shared JavaScript value poisoned"))
    }
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Default for SharedJsValue {
    fn default() -> Self {
        Self::new(JsValue::Null)
    }
}
impl fmt::Debug for SharedJsValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.read(|v| v.fmt(f))
    }
}
impl PartialEq for SharedJsValue {
    fn eq(&self, other: &Self) -> bool {
        self.ptr_eq(other) || self.snapshot() == other.snapshot()
    }
}
impl PartialEq<JsValue> for SharedJsValue {
    fn eq(&self, other: &JsValue) -> bool {
        self.read(|v| v == other)
    }
}
impl From<JsValue> for SharedJsValue {
    fn from(value: JsValue) -> Self {
        Self::new(value)
    }
}
impl From<JsObject> for SharedJsValue {
    fn from(value: JsObject) -> Self {
        Self::new(value)
    }
}
impl Serialize for SharedJsValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.read(|v| v.serialize(serializer))
    }
}
impl<'de> Deserialize<'de> for SharedJsValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        JsValue::deserialize(deserializer).map(Self::new)
    }
}

/// An untyped message object retained until a source-defined normalization boundary.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct RawMessage(SharedJsValue);
impl RawMessage {
    pub fn new(value: JsObject) -> Self {
        Self(SharedJsValue::new(value))
    }
    pub fn snapshot(&self) -> JsObject {
        self.read(Clone::clone)
    }
    pub fn read<R>(&self, read: impl FnOnce(&JsObject) -> R) -> R {
        self.0
            .read(|value| read(value.as_object().expect("raw message remains an object")))
    }
    pub fn update<R>(&self, update: impl FnOnce(&mut JsObject) -> R) -> R {
        self.0.update(|value| {
            update(
                value
                    .as_object_mut()
                    .expect("raw message remains an object"),
            )
        })
    }
    pub fn ptr_eq(&self, other: &Self) -> bool {
        self.0.ptr_eq(&other.0)
    }
    pub fn role(&self) -> JsString {
        self.read(|value| {
            value
                .get("role")
                .and_then(JsValue::as_js_str)
                .cloned()
                .unwrap_or_default()
        })
    }
    pub fn timestamp(&self) -> f64 {
        self.read(|value| {
            value
                .get("timestamp")
                .and_then(JsValue::as_f64)
                .unwrap_or(f64::NAN)
        })
    }
}
impl From<JsObject> for RawMessage {
    fn from(value: JsObject) -> Self {
        Self::new(value)
    }
}
impl<'de> Deserialize<'de> for RawMessage {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        JsObject::deserialize(deserializer).map(Self::new)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub r#type: ToolCallType,
    pub id: JsString,
    pub name: JsString,
    pub arguments: SharedJsValue,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<JsString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<JsString>,
    /// Enumerable provider scratch fields remain visible in event-time snapshots.
    #[serde(flatten)]
    pub extra: JsObject,
}
impl ToolCall {
    pub fn new(
        id: impl Into<JsString>,
        name: impl Into<JsString>,
        arguments: impl Into<JsValue>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments: SharedJsValue::new(arguments),
            ..Self::default()
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AssistantContent {
    Text(TextContent),
    Thinking(ThinkingContent),
    ToolCall(ToolCall),
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserContent {
    Text(TextContent),
    Image(ImageContent),
}
pub type ToolResultContent = UserContent;
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SystemContent {
    Text(JsString),
    Blocks(Vec<TextContent>),
}
impl Default for SystemContent {
    fn default() -> Self {
        Self::Text(JsString::default())
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserMessageContent {
    Text(JsString),
    Blocks(Vec<UserContent>),
}
impl Default for UserMessageContent {
    fn default() -> Self {
        Self::Text(JsString::default())
    }
}
macro_rules! content_from {
    ($union:ident, $variant:ident, $payload:ty) => {
        impl From<$payload> for $union {
            fn from(value: $payload) -> Self {
                Self::$variant(value)
            }
        }
    };
}
content_from!(AssistantContent, Text, TextContent);
content_from!(AssistantContent, Thinking, ThinkingContent);
content_from!(AssistantContent, ToolCall, ToolCall);
content_from!(UserContent, Text, TextContent);
content_from!(UserContent, Image, ImageContent);
content_from!(SystemContent, Text, JsString);
impl From<String> for SystemContent {
    fn from(value: String) -> Self {
        Self::Text(value.into())
    }
}
impl From<&str> for SystemContent {
    fn from(value: &str) -> Self {
        Self::Text(value.into())
    }
}
content_from!(SystemContent, Blocks, Vec<TextContent>);
content_from!(UserMessageContent, Text, JsString);
impl From<String> for UserMessageContent {
    fn from(value: String) -> Self {
        Self::Text(value.into())
    }
}
impl From<&str> for UserMessageContent {
    fn from(value: &str) -> Self {
        Self::Text(value.into())
    }
}
content_from!(UserMessageContent, Blocks, Vec<UserContent>);

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}
#[derive(Clone, Debug, PartialEq)]
pub struct Usage {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub cache_write1h: Option<f64>,
    pub reasoning: Option<f64>,
    pub total_tokens: f64,
    /// Historical Pi sessions can omit totalTokens. Preserve that field's
    /// presence while exposing zero to the source's numeric fallback logic.
    pub total_tokens_present: bool,
    pub cost: UsageCost,
}
impl Default for Usage {
    fn default() -> Self {
        Self {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
            cache_write1h: None,
            reasoning: None,
            total_tokens: 0.0,
            total_tokens_present: true,
            cost: UsageCost::default(),
        }
    }
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageWire {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_write1h: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<f64>,
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    total_tokens: Option<f64>,
    cost: UsageCost,
}
impl Serialize for Usage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        UsageWire {
            input: self.input,
            output: self.output,
            cache_read: self.cache_read,
            cache_write: self.cache_write,
            cache_write1h: self.cache_write1h,
            reasoning: self.reasoning,
            total_tokens: self.total_tokens_present.then_some(self.total_tokens),
            cost: self.cost.clone(),
        }
        .serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for Usage {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = UsageWire::deserialize(deserializer)?;
        Ok(Self {
            input: wire.input,
            output: wire.output,
            cache_read: wire.cache_read,
            cache_write: wire.cache_write,
            cache_write1h: wire.cache_write1h,
            reasoning: wire.reasoning,
            total_tokens: wire.total_tokens.unwrap_or(0.0),
            total_tokens_present: wire.total_tokens.is_some(),
            cost: wire.cost,
        })
    }
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeferredHandle {
    pub provider: String,
    pub model_id: String,
    pub api: String,
    pub id: JsString,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poll_after_ms: Option<f64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub data: Option<JsValue>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemMessage {
    pub role: SystemRole,
    pub content: SystemContent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sections: Option<IndexMap<JsString, Option<JsString>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools_added: Option<Vec<Tool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools_removed: Option<Vec<ToolReference>>,
    pub timestamp: f64,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    pub role: UserRole,
    pub content: UserMessageContent,
    pub timestamp: f64,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    pub role: AssistantRole,
    pub content: Vec<AssistantContent>,
    pub api: Api,
    pub provider: ProviderId,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_model: Option<JsString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_id: Option<JsString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_thinking_level: Option<JsString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<ModelThinkingLevel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<Vec<AssistantMessageDiagnostic>>,
    pub usage: Usage,
    pub stop_reason: StopReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deferred: Option<DeferredHandle>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<JsString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_stop_reason: Option<JsString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_turn: Option<bool>,
    pub timestamp: f64,
}
impl AssistantMessage {
    pub fn new(model: &Model, timestamp: f64) -> Self {
        Self {
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            timestamp,
            ..Self::default()
        }
    }
}
// Retained for records in existing sessions even though nested execution is excluded.
string_enum!(NestedToolCallStatus { Ok => "ok", Error => "error", Unfinished => "unfinished" });
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NestedToolCallRecord {
    pub id: JsString,
    pub name: JsString,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<JsonObject>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments_bytes: Option<f64>,
    pub status: NestedToolCallStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsString>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NestedToolCalls {
    pub calls: Vec<NestedToolCallRecord>,
    pub complete: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    pub role: ToolResultRole,
    pub tool_call_id: JsString,
    pub tool_name: JsString,
    pub content: Vec<ToolResultContent>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub details: Option<JsValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nested_calls: Option<NestedToolCalls>,
    pub is_error: bool,
    pub timestamp: f64,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
// Message variants remain inline to preserve direct Pi-shaped pattern matching.
#[allow(clippy::large_enum_variant)]
pub enum Message {
    System(SystemMessage),
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
    Raw(RawMessage),
}
content_from!(Message, System, SystemMessage);
content_from!(Message, User, UserMessage);
content_from!(Message, Assistant, AssistantMessage);
content_from!(Message, ToolResult, ToolResultMessage);
content_from!(Message, Raw, RawMessage);
impl Message {
    pub fn timestamp(&self) -> f64 {
        match self {
            Self::System(m) => m.timestamp,
            Self::User(m) => m.timestamp,
            Self::Assistant(m) => m.timestamp,
            Self::ToolResult(m) => m.timestamp,
            Self::Raw(m) => m.timestamp(),
        }
    }
    pub fn role(&self) -> &'static str {
        match self {
            Self::System(_) => "system",
            Self::User(_) => "user",
            Self::Assistant(_) => "assistant",
            Self::ToolResult(_) => "toolResult",
            Self::Raw(m) => match m.role().as_str() {
                Some("system") => "system",
                Some("user") => "user",
                Some("assistant") => "assistant",
                Some("toolResult") => "toolResult",
                _ => "",
            },
        }
    }
}

pub use crate::utils::validation::Schema;
string_enum!(GrammarFormat { OpenaiLark => "openai_lark", OpenaiRegex => "openai_regex" });
string_enum!(StrictPreference { Prefer => "prefer", Require => "require" });
pub type GrammarVariants = IndexMap<GrammarFormat, JsString>;
/// Retain the original nested declaration order used by Pi's JSON-string comparison.
/// Typed views interpret the selected fields without reconstructing their object.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConstrainedSamplingConfig {
    pub raw: JsObject,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ConstrainedSamplingView<'a> {
    JsonSchema { strict: StrictPreference },
    Grammar { variants: &'a JsObject },
}
impl ConstrainedSamplingConfig {
    pub fn from_raw(raw: JsObject) -> Self {
        Self { raw }
    }
    pub fn json_schema(strict: StrictPreference) -> Self {
        let mut raw = JsObject::new();
        raw.insert("type", JsValue::String("json_schema".into()));
        raw.insert("strict", JsValue::String(strict.as_str().into()));
        Self { raw }
    }
    pub fn grammar(variants: GrammarVariants) -> Self {
        let variants = variants
            .into_iter()
            .map(|(format, text)| (JsString::from(format.as_str()), JsValue::String(text)))
            .collect();
        let mut raw = JsObject::new();
        raw.insert("type", JsValue::String("grammar".into()));
        raw.insert("variants", JsValue::Object(variants));
        Self { raw }
    }
    pub fn view(&self) -> Option<ConstrainedSamplingView<'_>> {
        match self.raw.get("type") {
            Some(JsValue::String(kind)) if kind == &JsString::from("json_schema") => {
                let strict = match self.raw.get("strict") {
                    Some(JsValue::String(strict)) if strict == &JsString::from("prefer") => {
                        StrictPreference::Prefer
                    }
                    Some(JsValue::String(strict)) if strict == &JsString::from("require") => {
                        StrictPreference::Require
                    }
                    _ => return None,
                };
                Some(ConstrainedSamplingView::JsonSchema { strict })
            }
            Some(JsValue::String(kind)) if kind == &JsString::from("grammar") => {
                match self.raw.get("variants") {
                    Some(JsValue::Object(variants)) => {
                        Some(ConstrainedSamplingView::Grammar { variants })
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }
    pub fn grammar_variant(&self, format: GrammarFormat) -> Option<&JsString> {
        match self.view()? {
            ConstrainedSamplingView::Grammar { variants } => match variants.get(format.as_str()) {
                Some(JsValue::String(text)) => Some(text),
                _ => None,
            },
            ConstrainedSamplingView::JsonSchema { .. } => None,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ConstrainedSampling {
    Disabled(bool),
    Config(ConstrainedSamplingConfig),
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub name: JsString,
    pub description: JsString,
    pub parameters: Schema,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub constrained_sampling: Option<ConstrainedSampling>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolReference {
    pub name: JsString,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Context {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<JsString>,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
}
/// Only transcript normalization creates this context in production code.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TranscriptContext {
    pub messages: Vec<Message>,
}
impl TranscriptContext {
    pub(crate) fn new(messages: Vec<Message>) -> Self {
        Self { messages }
    }
}

/// A content block shared by shallow message copies.
#[derive(Clone)]
pub struct SharedAssistantContent(Arc<Mutex<AssistantContent>>);
impl SharedAssistantContent {
    pub fn new(block: AssistantContent) -> Self {
        Self(Arc::new(Mutex::new(block)))
    }
    pub fn snapshot(&self) -> AssistantContent {
        self.0
            .lock()
            .expect("assistant block lock poisoned")
            .clone()
    }
    pub fn update<T>(&self, update: impl FnOnce(&mut AssistantContent) -> T) -> T {
        update(&mut self.0.lock().expect("assistant block lock poisoned"))
    }
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

#[derive(Clone)]
struct SharedAssistantState {
    // Content lives separately so shallow copies share the array and blocks.
    metadata: AssistantMessage,
    content: Arc<Mutex<Vec<SharedAssistantContent>>>,
}
impl SharedAssistantState {
    fn new(mut message: AssistantMessage) -> Self {
        let content = std::mem::take(&mut message.content)
            .into_iter()
            .map(SharedAssistantContent::new)
            .collect();
        Self {
            metadata: message,
            content: Arc::new(Mutex::new(content)),
        }
    }
}

/// The event protocol's live response-so-far reference. Cloning preserves the
/// whole message identity; `shallow_clone` corresponds to a JavaScript spread.
#[derive(Clone)]
pub struct SharedAssistantMessage(Arc<Mutex<SharedAssistantState>>);
impl SharedAssistantMessage {
    pub fn new(message: AssistantMessage) -> Self {
        Self(Arc::new(Mutex::new(SharedAssistantState::new(message))))
    }
    pub fn snapshot(&self) -> AssistantMessage {
        let state = self.0.lock().expect("assistant message lock poisoned");
        let mut message = state.metadata.clone();
        message.content = state
            .content
            .lock()
            .expect("assistant content lock poisoned")
            .iter()
            .map(SharedAssistantContent::snapshot)
            .collect();
        message
    }
    pub fn read<T>(&self, read: impl FnOnce(&AssistantMessage) -> T) -> T {
        read(&self.snapshot())
    }
    pub fn shallow_clone(&self) -> Self {
        Self(Arc::new(Mutex::new(
            self.0
                .lock()
                .expect("assistant message lock poisoned")
                .clone(),
        )))
    }
    pub fn content_block(&self, index: usize) -> Option<SharedAssistantContent> {
        let content = self
            .0
            .lock()
            .expect("assistant message lock poisoned")
            .content
            .clone();
        content
            .lock()
            .expect("assistant content lock poisoned")
            .get(index)
            .cloned()
    }
    /// Mutate an existing block shared by every shallow copy that contains it.
    pub fn update_block<T>(
        &self,
        index: usize,
        update: impl FnOnce(&mut AssistantContent) -> T,
    ) -> Option<T> {
        self.content_block(index).map(|block| block.update(update))
    }
    /// Equivalent to `message.content.push(block)`: mutate the shared array.
    pub fn push_content(&self, block: AssistantContent) {
        let content = self
            .0
            .lock()
            .expect("assistant message lock poisoned")
            .content
            .clone();
        content
            .lock()
            .expect("assistant content lock poisoned")
            .push(SharedAssistantContent::new(block));
    }
    /// Equivalent to `message.content = [...message.content, block]`: replace
    /// this message's array, retaining the existing block identities.
    pub fn append_content_copy(&self, block: AssistantContent) {
        let mut state = self.0.lock().expect("assistant message lock poisoned");
        let mut content = state
            .content
            .lock()
            .expect("assistant content lock poisoned")
            .clone();
        content.push(SharedAssistantContent::new(block));
        state.content = Arc::new(Mutex::new(content));
    }
    pub fn replace_content(&self, blocks: Vec<AssistantContent>) {
        self.0
            .lock()
            .expect("assistant message lock poisoned")
            .content = Arc::new(Mutex::new(
            blocks
                .into_iter()
                .map(SharedAssistantContent::new)
                .collect(),
        ));
    }
    /// Replace this whole message's fields. Clones of the message see the
    /// replacement; prior shallow copies retain their own fields and arrays.
    pub fn replace(&self, message: AssistantMessage) {
        *self.0.lock().expect("assistant message lock poisoned") =
            SharedAssistantState::new(message);
    }
    /// Replace scalar/metadata fields while retaining the shared content array.
    /// The supplied message's content is intentionally ignored; content changes
    /// use the explicit array/block methods above.
    pub fn replace_metadata(&self, mut message: AssistantMessage) {
        message.content.clear();
        self.0
            .lock()
            .expect("assistant message lock poisoned")
            .metadata = message;
    }
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl fmt::Debug for SharedAssistantMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.read(|m| m.fmt(f))
    }
}
impl PartialEq for SharedAssistantMessage {
    fn eq(&self, other: &Self) -> bool {
        self.ptr_eq(other) || self.snapshot() == other.snapshot()
    }
}
impl From<AssistantMessage> for SharedAssistantMessage {
    fn from(message: AssistantMessage) -> Self {
        Self::new(message)
    }
}
impl Serialize for SharedAssistantMessage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.read(|m| m.serialize(serializer))
    }
}
impl<'de> Deserialize<'de> for SharedAssistantMessage {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        AssistantMessage::deserialize(deserializer).map(Self::new)
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum AssistantMessageEvent {
    Start {
        partial: SharedAssistantMessage,
    },
    TextStart {
        content_index: usize,
        partial: SharedAssistantMessage,
    },
    TextDelta {
        content_index: usize,
        delta: JsString,
        partial: SharedAssistantMessage,
    },
    TextEnd {
        content_index: usize,
        content: JsString,
        partial: SharedAssistantMessage,
    },
    ThinkingStart {
        content_index: usize,
        partial: SharedAssistantMessage,
    },
    ThinkingDelta {
        content_index: usize,
        delta: JsString,
        partial: SharedAssistantMessage,
    },
    ThinkingEnd {
        content_index: usize,
        content: JsString,
        partial: SharedAssistantMessage,
    },
    #[serde(rename = "toolcall_start")]
    ToolcallStart {
        content_index: usize,
        partial: SharedAssistantMessage,
    },
    #[serde(rename = "toolcall_delta")]
    ToolcallDelta {
        content_index: usize,
        delta: JsString,
        partial: SharedAssistantMessage,
    },
    #[serde(rename = "toolcall_end")]
    ToolcallEnd {
        content_index: usize,
        tool_call: ToolCall,
        partial: SharedAssistantMessage,
    },
    Done {
        reason: DoneReason,
        message: AssistantMessage,
    },
    Error {
        reason: ErrorReason,
        error: AssistantMessage,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderResponse {
    pub status: u16,
    pub headers: IndexMap<String, String>,
}
pub type CallbackError = Arc<dyn std::error::Error + Send + Sync>;
pub type OnPayload =
    Arc<dyn Fn(JsValue, Model) -> BoxFuture<Result<Option<JsValue>, CallbackError>> + Send + Sync>;
pub type OnResponse =
    Arc<dyn Fn(ProviderResponse, Model) -> BoxFuture<Result<(), CallbackError>> + Send + Sync>;
pub type OnProviderStreamEvent =
    Arc<dyn Fn(JsValue, Model) -> BoxFuture<Result<(), CallbackError>> + Send + Sync>;
/// Fetch remains an injected host operation. The portable runtime never obtains credentials.
#[derive(Clone, Debug)]
pub struct FetchRequest {
    pub url: String,
    pub method: String,
    pub headers: IndexMap<String, String>,
    pub body: Vec<u8>,
    pub signal: Option<CancellationToken>,
}
#[derive(Clone, Debug)]
pub struct FetchResponse {
    pub status: u16,
    pub headers: IndexMap<String, String>,
    pub body: Vec<u8>,
}
pub type FetchFunction =
    Arc<dyn Fn(FetchRequest) -> BoxFuture<Result<FetchResponse, CallbackError>> + Send + Sync>;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRequestOptions {
    #[serde(skip)]
    pub signal: Option<CancellationToken>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip)]
    pub fetch: Option<FetchFunction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<ProviderEnv>,
    #[serde(skip)]
    pub on_payload: Option<OnPayload>,
    #[serde(skip)]
    pub on_response: Option<OnResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<ProviderHeaders>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_retry_delay_ms: Option<f64>,
}
impl fmt::Debug for ProviderRequestOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderRequestOptions")
            .field("signal", &self.signal)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field(
                "env",
                &self.env.as_ref().map(|env| env.keys().collect::<Vec<_>>()),
            )
            .field(
                "headers",
                &self
                    .headers
                    .as_ref()
                    .map(|headers| headers.keys().collect::<Vec<_>>()),
            )
            .field("timeout_ms", &self.timeout_ms)
            .field("max_retries", &self.max_retries)
            .field("max_retry_delay_ms", &self.max_retry_delay_ms)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamOptions {
    #[serde(flatten)]
    pub request: ProviderRequestOptions,
    #[serde(skip)]
    pub on_provider_stream_event: Option<OnProviderStreamEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sampling_params: Option<SamplingParams>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport: Option<Transport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_retention: Option<CacheRetention>,
    /// Host-selected environment key for the source cache-retention fallback.
    /// No ambient application-specific variable is read unless a key is supplied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_retention_env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub websocket_connect_timeout_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonObject>,
}
impl std::ops::Deref for StreamOptions {
    type Target = ProviderRequestOptions;
    fn deref(&self) -> &Self::Target {
        &self.request
    }
}
impl std::ops::DerefMut for StreamOptions {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.request
    }
}
fn deserialize_stream_option_extensions<'de, D>(deserializer: D) -> Result<JsObject, D::Error>
where
    D: Deserializer<'de>,
{
    let mut extra = JsObject::deserialize(deserializer)?;
    // Serde retains fields consumed by a flattened struct for subsequent
    // flattened maps. They must not become duplicate extension properties:
    // a later mutation of the typed field is the single source of its value.
    for field in [
        "signal",
        "apiKey",
        "fetch",
        "env",
        "onPayload",
        "onResponse",
        "headers",
        "timeoutMs",
        "maxRetries",
        "maxRetryDelayMs",
        "onProviderStreamEvent",
        "temperature",
        "samplingParams",
        "maxTokens",
        "transport",
        "cacheRetention",
        "cacheRetentionEnv",
        "sessionId",
        "websocketConnectTimeoutMs",
        "metadata",
    ] {
        extra.remove(field);
    }
    Ok(extra)
}
string_enum!(DeferredWindow { Minutes15 => "15m", Hour1 => "1h", Hours24 => "24h" });
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DeferredWindowOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<DeferredWindow>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DeferredRequest {
    Enabled(bool),
    Window(DeferredWindowOptions),
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SimpleStreamOptions {
    #[serde(flatten)]
    pub stream: StreamOptions,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<JsValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ThinkingLevel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deferred: Option<DeferredRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_budgets: Option<ThinkingBudgets>,
    /// Enumerable configuration fields forwarded by upstream object spreads.
    #[serde(flatten, deserialize_with = "deserialize_stream_option_extensions")]
    pub extra: JsObject,
}
impl std::ops::Deref for SimpleStreamOptions {
    type Target = StreamOptions;
    fn deref(&self) -> &Self::Target {
        &self.stream
    }
}
impl std::ops::DerefMut for SimpleStreamOptions {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.stream
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DeferredFetchOptions {
    #[serde(flatten)]
    pub request: ProviderRequestOptions,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait: Option<f64>,
}
pub type DeferredCancelOptions = ProviderRequestOptions;
pub type StreamFunction<O = StreamOptions> =
    Arc<dyn Fn(Model, TranscriptContext, Option<O>) -> AssistantMessageEventStream + Send + Sync>;
pub type DeferredStreamFunction = Arc<
    dyn Fn(Model, DeferredHandle, Option<DeferredFetchOptions>) -> AssistantMessageEventStream
        + Send
        + Sync,
>;
pub type DeferredCancelFunction = Arc<
    dyn Fn(
            Model,
            DeferredHandle,
            Option<DeferredCancelOptions>,
        ) -> BoxFuture<Result<(), CallbackError>>
        + Send
        + Sync,
>;
#[derive(Clone)]
pub struct ProviderStreams {
    pub stream: StreamFunction,
    pub stream_simple: StreamFunction<SimpleStreamOptions>,
    pub fetch_deferred: Option<DeferredStreamFunction>,
    pub cancel_deferred: Option<DeferredCancelFunction>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProviderStreamOptions {
    #[serde(flatten)]
    pub stream: StreamOptions,
    #[serde(flatten, deserialize_with = "deserialize_stream_option_extensions")]
    pub extra: JsonObject,
}
string_enum!(ThinkingVariable { Enabled => "thinking.enabled", Effort => "thinking.effort", Budget => "thinking.budget" });
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatTemplateVariable {
    #[serde(rename = "$var")]
    pub var: ThinkingVariable,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub omit_when_off: Option<bool>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ChatTemplateKwargValue {
    Variable(ChatTemplateVariable),
    String(JsString),
    Number(f64),
    Boolean(bool),
    Null,
}
string_enum!(MaxTokensField { MaxCompletionTokens => "max_completion_tokens", MaxTokens => "max_tokens" });
string_enum!(ThinkingFormat { Openai => "openai", Openrouter => "openrouter", Deepseek => "deepseek", Together => "together", Baseten => "baseten", Zai => "zai", Qwen => "qwen", ChatTemplate => "chat-template", QwenChatTemplate => "qwen-chat-template", StringThinking => "string-thinking", AntLing => "ant-ling" });
string_enum!(CacheControlFormat { Anthropic => "anthropic" });

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenAICompletionsCompat {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_store: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_developer_role: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_reasoning_effort: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_usage_in_streaming: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_finish_reason: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires_tool_result_name: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires_assistant_after_tool_result: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires_thinking_as_text: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires_reasoning_content_on_assistant_messages: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zai_tool_stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_thinking_token_budget: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "supportsOpenAIGrammarTools")]
    pub supports_open_ai_grammar_tools: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_mid_convo_system_messages: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_mid_convo_tool_additions: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_strict_mode: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub send_session_affinity_headers: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_long_cache_retention: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens_field: Option<MaxTokensField>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_format: Option<ThinkingFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_template_kwargs: Option<IndexMap<String, ChatTemplateKwargValue>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_template_args: Option<IndexMap<String, ChatTemplateKwargValue>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_router_routing: Option<OpenRouterRouting>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vercel_gateway_routing: Option<VercelGatewayRouting>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_token_budget_field: Option<ThinkingTokenBudgetField>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control_format: Option<CacheControlFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_affinity_format: Option<SessionAffinityFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vllm_priority: Option<f64>,
    /// Preserves catalog metadata from API families whose implementations are outside this port.
    #[serde(flatten)]
    pub extra: JsonObject,
}
string_enum!(DataCollection { Deny => "deny", Allow => "allow" });
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RoutingSortOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub partition: Option<Option<String>>,
}
/// Preserve the difference between an absent property and an explicit JSON null.
fn deserialize_present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RoutingSort {
    String(String),
    Options(RoutingSortOptions),
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum NumberOrString {
    Number(f64),
    String(String),
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RoutingPrice {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<NumberOrString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion: Option<NumberOrString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<NumberOrString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<NumberOrString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<NumberOrString>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Percentiles {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p50: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p75: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p90: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p99: Option<f64>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RoutingMetric {
    Number(f64),
    Percentiles(Percentiles),
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenRouterRouting {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_fallbacks: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub require_parameters: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_collection: Option<DataCollection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zdr: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enforce_distillable_text: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub only: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignore: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantizations: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort: Option<RoutingSort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_price: Option<RoutingPrice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_min_throughput: Option<RoutingMetric>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_max_latency: Option<RoutingMetric>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VercelGatewayRouting {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub only: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<Vec<String>>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostRates {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostTier {
    #[serde(flatten)]
    pub rates: ModelCostRates,
    pub input_tokens_above: f64,
}
impl std::ops::Deref for ModelCostTier {
    type Target = ModelCostRates;
    fn deref(&self) -> &Self::Target {
        &self.rates
    }
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
    #[serde(flatten)]
    pub rates: ModelCostRates,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tiers: Option<Vec<ModelCostTier>>,
}
impl std::ops::Deref for ModelCost {
    type Target = ModelCostRates;
    fn deref(&self) -> &Self::Target {
        &self.rates
    }
}
impl std::ops::DerefMut for ModelCost {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.rates
    }
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelImageResizeOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_width: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_height: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jpeg_quality: Option<f64>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelImageInputLimits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resize: Option<ModelImageResizeOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_per_message: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_per_request: Option<f64>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInputLimits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_request_bytes: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<ModelImageInputLimits>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    pub name: String,
    pub api: Api,
    pub provider: ProviderId,
    pub base_url: String,
    pub input: Vec<InputModality>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_limits: Option<ModelInputLimits>,
    pub cost: ModelCost,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<IndexMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#type: Option<ModelType>,
    pub reasoning: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<ThinkingLevelMap>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache: Option<ModelPromptCache>,
    pub context_window: f64,
    pub max_tokens: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sampling_params: Option<SamplingParams>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sampling_params_by_thinking_level: Option<SamplingParamsByThinkingLevel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compat: Option<OpenAICompletionsCompat>,
}
pub type AnyModel = Model;
/// Fields common to catalog model kinds; only the chat specialization is ported.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BaseModel {
    pub id: String,
    pub name: String,
    pub api: Api,
    pub provider: ProviderId,
    pub base_url: String,
    pub input: Vec<InputModality>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_limits: Option<ModelInputLimits>,
    pub cost: ModelCost,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<IndexMap<String, String>>,
}
impl From<DoneReason> for StopReason {
    fn from(value: DoneReason) -> Self {
        match value {
            DoneReason::Stop => Self::Stop,
            DoneReason::Length => Self::Length,
            DoneReason::ToolUse => Self::ToolUse,
            DoneReason::Deferred => Self::Deferred,
        }
    }
}
impl From<ErrorReason> for StopReason {
    fn from(value: ErrorReason) -> Self {
        match value {
            ErrorReason::Error => Self::Error,
            ErrorReason::Aborted => Self::Aborted,
        }
    }
}

impl fmt::Debug for StreamOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamOptions")
            .field("request", &self.request)
            .field("temperature", &self.temperature)
            .field("sampling_params", &self.sampling_params)
            .field("max_tokens", &self.max_tokens)
            .field("transport", &self.transport)
            .field("cache_retention", &self.cache_retention)
            .field("session_id", &self.session_id)
            .field(
                "websocket_connect_timeout_ms",
                &self.websocket_connect_timeout_ms,
            )
            .field("metadata", &self.metadata)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod contract_tests {
    use super::*;
    use crate::utils::{js_json, js_value::to_js_value};

    #[test]
    fn event_snapshots_keep_split_utf16_units_until_json_observation() {
        let delta = JsString::from_utf16(vec![0xd83d]);
        let mut message = AssistantMessage::default();
        message.content.push(TextContent::new(delta.clone()).into());
        let event = AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: delta.clone(),
            partial: SharedAssistantMessage::new(message),
        };
        let value = to_js_value(&event).unwrap();
        assert_eq!(
            value.get("delta").and_then(JsValue::as_js_str),
            Some(&delta)
        );
        assert!(js_json::stringify(&value).contains(r#""delta":"\ud83d""#));
        assert!(
            serde_json::to_string(&event)
                .unwrap()
                .contains(r#""delta":"\ud83d""#)
        );
    }

    #[test]
    fn tool_arguments_preserve_nonfinite_numbers_and_utf16_keys() {
        let key = JsString::from_utf16(vec![0xd800]);
        let mut arguments = JsObject::new();
        arguments.insert(key.clone(), JsValue::Number(f64::INFINITY));
        let call = ToolCall::new("call", "tool", arguments);
        let value = to_js_value(&call).unwrap();
        assert_eq!(
            value.get("arguments").unwrap().get(&key).unwrap().as_f64(),
            Some(f64::INFINITY)
        );
        assert!(js_json::stringify(&value).contains(r#""arguments":{"\ud800":null}"#));
    }

    #[test]
    fn constrained_sampling_retains_nested_key_order() {
        let mut raw = JsObject::new();
        raw.insert("strict", "prefer".into());
        raw.insert("type", "json_schema".into());
        let reversed = ConstrainedSamplingConfig::from_raw(raw);
        let constructed = ConstrainedSamplingConfig::json_schema(StrictPreference::Prefer);
        assert_eq!(
            reversed.view(),
            Some(ConstrainedSamplingView::JsonSchema {
                strict: StrictPreference::Prefer
            })
        );
        assert_eq!(constructed.view(), reversed.view());
        assert_eq!(
            js_json::stringify(&to_js_value(&reversed).unwrap()),
            r#"{"strict":"prefer","type":"json_schema"}"#
        );
        assert_eq!(
            js_json::stringify(&to_js_value(&constructed).unwrap()),
            r#"{"type":"json_schema","strict":"prefer"}"#
        );
    }

    #[test]
    fn lossless_decoder_handles_untagged_messages_and_private_key_objects() {
        use crate::utils::js_value::from_js_value;
        let lone = JsString::from_utf16(vec![0xd800]);
        let mut literal_object = JsObject::new();
        literal_object.insert("$serde_json::private::RawValue", "\"\\ud800\"".into());
        let mut arguments = JsObject::new();
        arguments.insert(lone.clone(), JsValue::Number(f64::INFINITY));
        arguments.insert("literal", JsValue::Object(literal_object));
        let message = Message::Assistant(AssistantMessage {
            content: vec![
                TextContent::new(lone.clone()).into(),
                ToolCall::new(lone, "tool", arguments).into(),
            ],
            ..AssistantMessage::default()
        });
        let restored: Message = from_js_value(to_js_value(&message).unwrap()).unwrap();
        assert_eq!(restored, message);
        let Message::Assistant(restored) = restored else {
            panic!("expected assistant message");
        };
        let AssistantContent::ToolCall(call) = &restored.content[1] else {
            panic!("expected tool call");
        };
        let arguments = call.arguments.snapshot();
        assert!(arguments["literal"].is_object());
        assert_eq!(
            arguments["literal"]
                .get("$serde_json::private::RawValue")
                .unwrap()
                .as_str(),
            Some("\"\\ud800\"")
        );
    }

    #[test]
    fn lossless_decoder_handles_flattened_metadata_and_compatibility() {
        use crate::utils::js_value::from_js_value;
        let lone = JsString::from_utf16(vec![0xd800]);
        let metadata = JsObject::from([(lone.clone(), JsValue::String(lone.clone()))]);
        let options = SimpleStreamOptions {
            stream: StreamOptions {
                metadata: Some(metadata.clone()),
                ..StreamOptions::default()
            },
            ..SimpleStreamOptions::default()
        };
        let restored: SimpleStreamOptions = from_js_value(to_js_value(&options).unwrap()).unwrap();
        assert_eq!(restored.metadata, Some(metadata.clone()));
        let model = Model {
            compat: Some(OpenAICompletionsCompat {
                extra: metadata,
                ..OpenAICompletionsCompat::default()
            }),
            ..Model::default()
        };
        let restored: Model = from_js_value(to_js_value(&model).unwrap()).unwrap();
        assert_eq!(restored, model);
    }

    #[test]
    fn option_extension_maps_do_not_override_mutated_typed_fields() {
        use crate::utils::js_value::from_json;
        let input = r#"{"cacheRetention":"long","sessionId":"caller","apiKey":"key","temperature":null,"hostOption":7}"#;
        let mut simple: SimpleStreamOptions = from_json(input).unwrap();
        assert_eq!(
            simple.extra,
            JsObject::from([("hostOption", JsValue::from(7.0))])
        );
        simple.cache_retention = None;
        simple.session_id = Some("updated".into());
        simple.api_key = None;
        let output = to_js_value(&simple).unwrap();
        assert!(output.get("cacheRetention").is_none());
        assert!(output.get("apiKey").is_none());
        assert!(output.get("temperature").is_none());
        assert_eq!(output["sessionId"], JsValue::from("updated"));
        assert_eq!(output["hostOption"], JsValue::from(7.0));

        let mut provider: ProviderStreamOptions = from_json(input).unwrap();
        assert_eq!(provider.extra, simple.extra);
        provider.stream.cache_retention = None;
        provider.stream.session_id = None;
        let output = to_js_value(&provider).unwrap();
        assert!(output.get("cacheRetention").is_none());
        assert!(output.get("sessionId").is_none());
        assert_eq!(output["hostOption"], JsValue::from(7.0));
    }

    #[test]
    fn json_import_restores_escaped_lone_surrogates_in_message_content() {
        use crate::utils::js_value::from_json;
        let message = Message::User(UserMessage {
            content: UserMessageContent::Blocks(vec![
                TextContent::new(JsString::from_utf16(vec![0xdc00])).into(),
            ]),
            ..UserMessage::default()
        });
        let encoded = js_json::stringify(&to_js_value(&message).unwrap());
        assert!(encoded.contains(r#""text":"\udc00""#));
        let restored: Message = from_json(&encoded).unwrap();
        assert_eq!(restored, message);
    }

    #[test]
    fn optional_json_preserves_explicit_null_separately_from_absence() {
        let message = ToolResultMessage {
            details: Some(JsValue::Null),
            ..ToolResultMessage::default()
        };
        let value = to_js_value(&message).unwrap();
        assert!(value.get("details").unwrap().is_null());
        let absent = to_js_value(&ToolResultMessage::default()).unwrap();
        assert!(absent.get("details").is_none());
        let decoded: ToolResultMessage =
            serde_json::from_value(serde_json::to_value(&message).unwrap()).unwrap();
        assert_eq!(decoded.details, Some(JsValue::Null));
    }
    #[test]
    fn tool_argument_alias_survives_until_parser_replaces_the_whole_value() {
        let mut call = ToolCall::new(
            "id",
            "tool",
            JsObject::from([("value", JsValue::Number(1.0))]),
        );
        let earlier = call.arguments.clone();
        earlier.update(|value| value["value"] = JsValue::Number(2.0));
        assert_eq!(
            call.arguments.read(|value| value["value"].clone()),
            JsValue::Number(2.0)
        );
        call.arguments = JsValue::Object(JsObject::from([("value", JsValue::Number(3.0))])).into();
        assert!(!call.arguments.ptr_eq(&earlier));
        assert_eq!(
            earlier.read(|value| value["value"].clone()),
            JsValue::Number(2.0)
        );
        assert_eq!(
            call.arguments.read(|value| value["value"].clone()),
            JsValue::Number(3.0)
        );
        assert_eq!(
            to_js_value(&call).unwrap()["arguments"]["value"],
            JsValue::Number(3.0)
        );
    }

    #[test]
    fn legacy_usage_retains_absent_total_tokens_on_roundtrip() {
        let raw = serde_json::json!({"input":1,"output":2,"cacheRead":3,"cacheWrite":4,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}});
        let usage: Usage = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(usage.total_tokens, 0.0);
        assert!(!usage.total_tokens_present);
        assert_eq!(
            to_js_value(&usage).unwrap(),
            JsValue::from_json_with_js_numbers(raw)
        );
        assert!(
            serde_json::to_value(Usage::default())
                .unwrap()
                .get("totalTokens")
                .is_some()
        );
    }
    #[test]
    fn raw_message_retains_null_content_unknown_fields_and_shared_identity() {
        let raw = JsObject::from([
            ("role", JsValue::from("user")),
            ("content", JsValue::Null),
            ("timestamp", JsValue::Number(1.0)),
            ("extra", JsValue::from("retained")),
        ]);
        let message: Message =
            crate::utils::js_value::from_js_value(JsValue::Object(raw.clone())).unwrap();
        let Message::Raw(message) = message else {
            panic!("malformed typed content must remain raw")
        };
        assert_eq!(to_js_value(&message).unwrap(), JsValue::Object(raw));
        let alias = message.clone();
        assert!(message.ptr_eq(&alias));
        alias.update(|object| {
            object.insert("observed", JsValue::Bool(true));
        });
        assert_eq!(
            message.read(|object| object.get("observed").cloned()),
            Some(JsValue::Bool(true))
        );
    }
}
