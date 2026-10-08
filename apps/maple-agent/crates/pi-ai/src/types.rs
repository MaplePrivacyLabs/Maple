use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

/// Milliseconds since the Unix epoch.
pub type Timestamp = i64;

/// The current time as a [`Timestamp`].
pub fn now_ms() -> Timestamp {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as Timestamp)
        .unwrap_or_default()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextContent {
    pub text: String,
    /// Provider metadata needed to replay the block, when the API returns one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_signature: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageContent {
    /// Base64-encoded image bytes.
    pub data: String,
    pub mime_type: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingContent {
    pub thinking: String,
    /// Opaque provider data that lets the model reuse its reasoning on a later turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_signature: Option<String>,
    /// The reasoning was withheld by the provider; only the signature can be replayed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub redacted: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub arguments: Map<String, Value>,
}

/// Content of user messages and tool results.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Content {
    Text(TextContent),
    Image(ImageContent),
}

impl Content {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })
    }

    pub fn image(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self::Image(ImageContent {
            data: data.into(),
            mime_type: mime_type.into(),
        })
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(&text.text),
            Self::Image(_) => None,
        }
    }
}

/// Content of assistant messages.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AssistantContent {
    Text(TextContent),
    Thinking(ThinkingContent),
    ToolCall(ToolCall),
}

impl AssistantContent {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })
    }

    pub fn thinking(thinking: impl Into<String>) -> Self {
        Self::Thinking(ThinkingContent {
            thinking: thinking.into(),
            thinking_signature: None,
            redacted: false,
        })
    }

    pub fn tool_call(id: impl Into<String>, name: impl Into<String>, arguments: Value) -> Self {
        let arguments = match arguments {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        Self::ToolCall(ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
        })
    }
}

/// Join the text blocks of `content` with newlines.
pub fn content_text(content: &[Content]) -> String {
    content
        .iter()
        .filter_map(Content::as_text)
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}

/// Token usage of one provider response. `reasoning` is a subset of `output`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
    pub total_tokens: u64,
    pub cost: Cost,
}

impl Usage {
    /// Tokens the request occupied in the context window, input and output included.
    pub fn context_tokens(&self) -> u64 {
        if self.total_tokens > 0 {
            self.total_tokens
        } else {
            self.input + self.output + self.cache_read + self.cache_write
        }
    }

    pub fn add(&self, other: &Usage) -> Usage {
        let reasoning = match (self.reasoning, other.reasoning) {
            (None, None) => None,
            (left, right) => Some(left.unwrap_or_default() + right.unwrap_or_default()),
        };
        Usage {
            input: self.input + other.input,
            output: self.output + other.output,
            cache_read: self.cache_read + other.cache_read,
            cache_write: self.cache_write + other.cache_write,
            reasoning,
            total_tokens: self.total_tokens + other.total_tokens,
            cost: Cost {
                input: self.cost.input + other.cost.input,
                output: self.cost.output + other.cost.output,
                cache_read: self.cost.cache_read + other.cost.cache_read,
                cache_write: self.cost.cache_write + other.cost.cache_write,
                total: self.cost.total + other.cost.total,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
}

/// How hard a reasoning model should think. Models map each level to their own value.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    #[default]
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ThinkingLevel {
    pub const ALL: [ThinkingLevel; 7] = [
        Self::Off,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::Xhigh,
        Self::Max,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|level| level.as_str() == value)
    }
}

/// A tool the model may call. `parameters` is a JSON Schema object.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl Tool {
    pub fn new(name: impl Into<String>, description: impl Into<String>, parameters: Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolReference {
    pub name: String,
}

/// System instructions and tool declarations at one point in the transcript.
///
/// The leading system message is the system prompt. Later system messages change it:
/// `content` adds instructions from that point on, `sections` replace or remove named
/// prompt sections, and `tools_added`/`tools_removed` change the tool set. Replaying
/// every system message in order yields the current prompt and tools.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemMessage {
    #[serde(default)]
    pub content: String,
    /// Named, ordered prompt sections rendered after `content`; `None` removes a section.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub sections: IndexMap<String, Option<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools_added: Vec<Tool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools_removed: Vec<ToolReference>,
    #[serde(default)]
    pub timestamp: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    #[serde(deserialize_with = "content_from_text_or_list")]
    pub content: Vec<Content>,
    #[serde(default)]
    pub timestamp: Timestamp,
}

impl UserMessage {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![Content::text(text)],
            timestamp: now_ms(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    #[serde(default)]
    pub content: Vec<AssistantContent>,
    pub api: String,
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    /// The thinking level the agent requested for this response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<ThinkingLevel>,
    #[serde(default)]
    pub usage: Usage,
    pub stop_reason: StopReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(default)]
    pub timestamp: Timestamp,
}

impl AssistantMessage {
    /// An empty response from `model`, ready to be filled by a provider.
    pub fn empty(model: &Model) -> Self {
        Self {
            content: Vec::new(),
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            response_id: None,
            thinking_level: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: now_ms(),
        }
    }

    /// A failed response carrying `error`.
    pub fn failed(model: &Model, stop_reason: StopReason, error: impl Into<String>) -> Self {
        Self {
            stop_reason,
            error_message: Some(error.into()),
            ..Self::empty(model)
        }
    }

    /// The text blocks joined with newlines.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                AssistantContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.content.iter().filter_map(|block| match block {
            AssistantContent::ToolCall(call) => Some(call),
            _ => None,
        })
    }

    pub fn is_failure(&self) -> bool {
        matches!(self.stop_reason, StopReason::Error | StopReason::Aborted)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    #[serde(default)]
    pub content: Vec<Content>,
    /// Tool-specific data for hosts and extensions. Never sent to the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    /// Usage of the tool itself, such as a nested model call. Not context accounting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default)]
    pub timestamp: Timestamp,
}

/// A message a model can read.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    System(SystemMessage),
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
}

impl Message {
    pub fn timestamp(&self) -> Timestamp {
        match self {
            Self::System(message) => message.timestamp,
            Self::User(message) => message.timestamp,
            Self::Assistant(message) => message.timestamp,
            Self::ToolResult(message) => message.timestamp,
        }
    }

    pub fn role(&self) -> &'static str {
        match self {
            Self::System(_) => "system",
            Self::User(_) => "user",
            Self::Assistant(_) => "assistant",
            Self::ToolResult(_) => "toolResult",
        }
    }
}

impl From<SystemMessage> for Message {
    fn from(message: SystemMessage) -> Self {
        Self::System(message)
    }
}

impl From<UserMessage> for Message {
    fn from(message: UserMessage) -> Self {
        Self::User(message)
    }
}

impl From<AssistantMessage> for Message {
    fn from(message: AssistantMessage) -> Self {
        Self::Assistant(message)
    }
}

impl From<ToolResultMessage> for Message {
    fn from(message: ToolResultMessage) -> Self {
        Self::ToolResult(message)
    }
}

fn content_from_text_or_list<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<Content>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Repr {
        Text(String),
        List(Vec<Content>),
    }
    Ok(match Repr::deserialize(deserializer)? {
        Repr::Text(text) => vec![Content::text(text)],
        Repr::List(list) => list,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputModality {
    Text,
    Image,
}

/// Prices in dollars per million tokens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ModelCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaxTokensField {
    #[default]
    MaxCompletionTokens,
    MaxTokens,
}

/// What an OpenAI-compatible endpoint accepts beyond the common request shape.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ModelCompat {
    /// Send the system prompt with the `developer` role.
    pub supports_developer_role: bool,
    pub max_tokens_field: MaxTokensField,
    /// Ask for token usage in the final streamed chunk.
    pub supports_usage_in_streaming: bool,
    /// Send `reasoning_effort` for reasoning models.
    pub supports_reasoning_effort: bool,
    /// Keep later system messages in place instead of folding them into the first.
    pub supports_mid_conversation_system_messages: bool,
}

impl Default for ModelCompat {
    fn default() -> Self {
        Self {
            supports_developer_role: false,
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
            supports_usage_in_streaming: true,
            supports_reasoning_effort: true,
            supports_mid_conversation_system_messages: false,
        }
    }
}

fn default_input() -> Vec<InputModality> {
    vec![InputModality::Text]
}

/// A chat model. `api` selects the provider implementation in an [`crate::ApiRegistry`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    pub name: String,
    pub api: String,
    pub provider: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default = "default_input")]
    pub input: Vec<InputModality>,
    #[serde(default)]
    pub cost: ModelCost,
    pub context_window: u64,
    pub max_tokens: u64,
    /// Provider value for each thinking level. `None` marks a level the model rejects;
    /// unlisted levels up to `high` use their own name, `xhigh` and `max` must be listed.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub thinking_levels: BTreeMap<ThinkingLevel, Option<String>>,
    #[serde(default)]
    pub compat: ModelCompat,
}

impl Model {
    pub fn supports_images(&self) -> bool {
        self.input.contains(&InputModality::Image)
    }

    /// The thinking levels this model accepts, `off` first.
    pub fn thinking_levels(&self) -> Vec<ThinkingLevel> {
        if !self.reasoning {
            return vec![ThinkingLevel::Off];
        }
        ThinkingLevel::ALL
            .into_iter()
            .filter(|level| match level {
                ThinkingLevel::Off => true,
                ThinkingLevel::Xhigh | ThinkingLevel::Max => {
                    matches!(self.thinking_levels.get(level), Some(Some(_)))
                }
                other => !matches!(self.thinking_levels.get(other), Some(None)),
            })
            .collect()
    }

    /// `level` if the model accepts it, otherwise the highest accepted level below it.
    pub fn clamp_thinking_level(&self, level: ThinkingLevel) -> ThinkingLevel {
        let available = self.thinking_levels();
        if available.contains(&level) {
            return level;
        }
        available
            .into_iter()
            .filter(|candidate| *candidate <= level)
            .max()
            .unwrap_or_default()
    }

    /// The provider's value for `level`, or `None` when reasoning stays off.
    pub fn provider_thinking_value(&self, level: ThinkingLevel) -> Option<String> {
        if !self.reasoning || level == ThinkingLevel::Off {
            return None;
        }
        match self.thinking_levels.get(&level) {
            Some(value) => value.clone(),
            None => Some(level.as_str().to_string()),
        }
    }

    /// Fill in `usage.cost` from this model's prices.
    pub fn apply_cost(&self, usage: &mut Usage) {
        let per_token = |price: f64, tokens: u64| price * tokens as f64 / 1_000_000.0;
        usage.cost.input = per_token(self.cost.input, usage.input);
        usage.cost.output = per_token(self.cost.output, usage.output);
        usage.cost.cache_read = per_token(self.cost.cache_read, usage.cache_read);
        usage.cost.cache_write = per_token(self.cost.cache_write, usage.cache_write);
        usage.cost.total =
            usage.cost.input + usage.cost.output + usage.cost.cache_read + usage.cost.cache_write;
    }
}

/// The transcript a provider receives. The system prompt and tool declarations travel in
/// its system messages, never beside them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Context {
    pub messages: Vec<Message>,
}

impl Context {
    /// Fold a system prompt and tool set into a leading system message.
    pub fn new(system_prompt: &str, tools: Vec<Tool>, messages: Vec<Message>) -> Self {
        let mut all = Vec::with_capacity(messages.len() + 1);
        if let Some(system) = crate::transcript::initial_system_message(system_prompt, tools) {
            all.push(Message::System(system));
        }
        all.extend(messages);
        Self { messages: all }
    }

    pub fn from_messages(messages: Vec<Message>) -> Self {
        Self { messages }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn model() -> Model {
        serde_json::from_value(json!({
            "id": "m", "name": "M", "api": "openai-completions", "provider": "p",
            "reasoning": true, "contextWindow": 1000, "maxTokens": 100,
            "cost": { "input": 2.0, "output": 10.0 },
        }))
        .unwrap()
    }

    #[test]
    fn messages_round_trip_with_role_and_type_tags() {
        let message = Message::Assistant(AssistantMessage {
            content: vec![
                AssistantContent::text("hi"),
                AssistantContent::tool_call("c1", "read", json!({ "path": "a" })),
            ],
            ..AssistantMessage::empty(&model())
        });
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value["role"], "assistant");
        assert_eq!(value["content"][1]["type"], "toolCall");
        assert_eq!(value["stopReason"], "stop");
        let back: Message = serde_json::from_value(value).unwrap();
        assert_eq!(back, message);
    }

    #[test]
    fn user_content_accepts_a_plain_string() {
        let message: Message =
            serde_json::from_value(json!({ "role": "user", "content": "hello", "timestamp": 1 }))
                .unwrap();
        let Message::User(user) = message else {
            panic!("expected a user message")
        };
        assert_eq!(content_text(&user.content), "hello");
    }

    #[test]
    fn thinking_levels_follow_the_model_map() {
        let mut model = model();
        assert_eq!(
            model.thinking_levels(),
            vec![
                ThinkingLevel::Off,
                ThinkingLevel::Minimal,
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High
            ]
        );
        model.thinking_levels.insert(ThinkingLevel::Minimal, None);
        model
            .thinking_levels
            .insert(ThinkingLevel::Xhigh, Some("extra".into()));
        assert!(!model.thinking_levels().contains(&ThinkingLevel::Minimal));
        assert_eq!(
            model.provider_thinking_value(ThinkingLevel::Xhigh),
            Some("extra".into())
        );
        assert_eq!(
            model.clamp_thinking_level(ThinkingLevel::Max),
            ThinkingLevel::Xhigh
        );
        assert_eq!(
            model.clamp_thinking_level(ThinkingLevel::Minimal),
            ThinkingLevel::Off
        );
        model.reasoning = false;
        assert_eq!(
            model.clamp_thinking_level(ThinkingLevel::High),
            ThinkingLevel::Off
        );
        assert_eq!(model.provider_thinking_value(ThinkingLevel::High), None);
    }

    #[test]
    fn cost_is_priced_per_million_tokens() {
        let mut usage = Usage {
            input: 1_000_000,
            output: 500_000,
            ..Usage::default()
        };
        model().apply_cost(&mut usage);
        assert_eq!(usage.cost.input, 2.0);
        assert_eq!(usage.cost.output, 5.0);
        assert_eq!(usage.cost.total, 7.0);
    }

    #[test]
    fn usage_adds_component_wise() {
        let left = Usage {
            input: 1,
            output: 2,
            total_tokens: 3,
            ..Usage::default()
        };
        let right = Usage {
            input: 4,
            reasoning: Some(1),
            total_tokens: 4,
            ..Usage::default()
        };
        let sum = left.add(&right);
        assert_eq!((sum.input, sum.output, sum.total_tokens), (5, 2, 7));
        assert_eq!(sum.reasoning, Some(1));
    }
}
