//! Provider-format prompt rendering, independent of storage and routing.
//!
//! A profile identifies pinned source behavior, not a claim about an active
//! third-party endpoint. Media processors and caller-supplied templates are
//! deliberately outside these text profiles.

use chrono::{Datelike, NaiveDate, Utc};
use minijinja::value::{Enumerator, Kwargs, Object, Serde, ValueKind};
use minijinja::{Environment, Error as JinjaError, ErrorKind, Value as JinjaValue};
use serde_json::{Map, Value};
use std::sync::Arc;

use crate::{count_segment, RenderError, SpecialTokenPolicy, TokenizerId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RendererId {
    DeepSeekV41Flash1017,
    Glm53Official,
    Glm53FlashOfficial,
    Glm53TinfoilNvfp4,
    Glm53FlashTinfoilNvfp4,
    /// ConfigMap template recovered by matching its deployment-enforced hash.
    Glm53Continuum,
    Glm53FlashContinuum,
    KimiK3,
    /// Direct Moonshot encoder reference, before the native vLLM wrapper.
    KimiK3OfficialReference,
    GptOss120bHarmony,
    GptOssSafeguard120bHarmony,
    /// Official HF Jinja reference. Native vLLM Harmony serving is a different
    /// renderer, so this profile is not an active GPT-OSS route counter.
    GptOss120bJinjaReference,
    GptOssSafeguard120bJinjaReference,
    Gemma4Tinfoil,
    Llama33Tinfoil,
}

impl RendererId {
    /// Advisory default model reference. Callers which know a provider
    /// must qualify its deployment profile before treating counts as its limit.
    /// Callers which know the selected provider must
    /// select its deployment profile explicitly instead of treating this as
    /// active-provider discovery.
    pub fn for_model(model: &str) -> Option<Self> {
        Some(match model {
            "deepseek-v4-1-flash" => Self::DeepSeekV41Flash1017,
            "glm-5-3" => Self::Glm53Official,
            "glm-5-3-flash" => Self::Glm53FlashOfficial,
            "kimi-k3" => Self::KimiK3,
            "gpt-oss-120b" => Self::GptOss120bHarmony,
            "gpt-oss-safeguard-120b" => Self::GptOssSafeguard120bHarmony,
            "gemma4-31b" => Self::Gemma4Tinfoil,
            "llama3-3-70b" => Self::Llama33Tinfoil,
            _ => return None,
        })
    }

    pub const fn tokenizer(self) -> TokenizerId {
        match self {
            Self::DeepSeekV41Flash1017 => TokenizerId::DeepSeekV41Flash,
            Self::Glm53Official | Self::Glm53TinfoilNvfp4 | Self::Glm53Continuum => {
                TokenizerId::Glm53
            }
            Self::Glm53FlashOfficial | Self::Glm53FlashContinuum | Self::Glm53FlashTinfoilNvfp4 => {
                TokenizerId::Glm53Flash
            }
            Self::KimiK3 | Self::KimiK3OfficialReference => TokenizerId::KimiK3,
            Self::GptOss120bJinjaReference | Self::GptOss120bHarmony => TokenizerId::GptOss120b,
            Self::GptOssSafeguard120bJinjaReference | Self::GptOssSafeguard120bHarmony => {
                TokenizerId::GptOssSafeguard120b
            }
            Self::Gemma4Tinfoil => TokenizerId::Gemma431b,
            Self::Llama33Tinfoil => TokenizerId::Llama3370b,
        }
    }

    /// Revision-bearing identity suitable for count-cache keys and diagnostics.
    pub const fn profile(self) -> &'static str {
        match self {
            Self::DeepSeekV41Flash1017 => {
                "deepseek-v41-1017/vllm-db9527a/tinfoil-released-reasoning-v1"
            }
            Self::Glm53Official => "glm53/ba32f8c/template-0e2b6268/normalizer-v1",
            Self::Glm53FlashOfficial => "glm53-flash/46a050b/template-2614a3c8/normalizer-v1",
            Self::Glm53TinfoilNvfp4 => "glm53-tinfoil/ce67b36/template-69bb3ab5/normalizer-v1",
            Self::Glm53FlashTinfoilNvfp4 => {
                "glm53-flash-tinfoil/240131d6/template-0c4099f3/string/vllm-db9527a-v1"
            }
            Self::Glm53Continuum => {
                "glm53-continuum/17ad265a/template-3740abce/string/vllm-db9527a-v1"
            }
            Self::Glm53FlashContinuum => {
                "glm53-flash-continuum/17ad265a/template-0c4099f3/openai/vllm-db9527a-v1"
            }
            Self::KimiK3 => "kimi-k3/a590ce0/vllm-db9527a/serving-normalizer-v1",
            Self::KimiK3OfficialReference => "kimi-k3/a590ce0/encoding-k3-v1",
            Self::GptOss120bHarmony => "gpt-oss-120b/vllm-0b3ba88/harmony-abd677f/native-chat-v1",
            Self::GptOssSafeguard120bHarmony => {
                "gpt-oss-safeguard-120b/vllm-0b3ba88/harmony-abd677f/native-chat-v1"
            }
            Self::GptOss120bJinjaReference => {
                "gpt-oss-120b/b5c939d/template-a4c9919c/normalizer-v1"
            }
            Self::GptOssSafeguard120bJinjaReference => {
                "gpt-oss-safeguard-120b/3c73911/template-589c73db/normalizer-v1"
            }
            Self::Gemma4Tinfoil => "gemma4/vllm-db9527a/template-afdbb2ab/normalizer-v1",
            Self::Llama33Tinfoil => "llama33/vllm-0b3ba88/template-c22af47e/normalizer-vllm022-v1",
        }
    }

    fn template(self) -> Result<&'static str, RenderError> {
        Ok(match self {
            Self::Glm53Official => include_str!("../profiles/glm53-official.jinja"),
            Self::Glm53FlashOfficial => include_str!("../profiles/glm53-flash-official.jinja"),
            Self::Glm53TinfoilNvfp4 => include_str!("../profiles/glm53-tinfoil-nvfp4.jinja"),
            Self::Glm53Continuum => include_str!("../profiles/glm53-continuum.jinja"),
            Self::Glm53FlashContinuum | Self::Glm53FlashTinfoilNvfp4 => {
                include_str!("../profiles/glm53-flash-continuum.jinja")
            }
            Self::GptOss120bJinjaReference => include_str!("../profiles/gpt-oss-120b.jinja"),
            Self::GptOssSafeguard120bJinjaReference => {
                include_str!("../profiles/gpt-oss-safeguard-120b.jinja")
            }
            Self::Gemma4Tinfoil => include_str!("../profiles/gemma4-tinfoil.jinja"),
            Self::Llama33Tinfoil => include_str!("../profiles/llama33-tinfoil.jinja"),
            _ => return Err(RenderError::Unsupported("profile does not use Jinja")),
        })
    }
}

/// Capture the serving date once for reproducible counting/rendering. Override
/// it in tests, or if the serving profile uses a separately supplied date.
#[derive(Debug, Clone)]
pub struct RenderContext {
    pub date: Option<String>,
}

impl Default for RenderContext {
    fn default() -> Self {
        Self {
            date: Some(Utc::now().format("%Y-%m-%d").to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSegment {
    pub text: String,
    pub policy: SpecialTokenPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedPrompt {
    /// Boundaries are significant for Kimi. Never concatenate before encoding.
    pub segments: Vec<PromptSegment>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptAccuracy {
    /// Exactness is scoped to the identified text profile, not an attestation
    /// or an assertion about mutable shared-provider deployments.
    ExactForPinnedTextProfile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptCount {
    pub tokens: usize,
    pub profile: &'static str,
    pub accuracy: PromptAccuracy,
}

pub fn count_chat_request(
    id: RendererId,
    request: &Value,
    context: &RenderContext,
) -> Result<PromptCount, RenderError> {
    let rendered = render_chat(id, request, context)?;
    let mut tokens = 0usize;
    for segment in rendered.segments {
        let count = count_segment(id.tokenizer(), &segment.text, segment.policy)
            .map_err(|_| RenderError::Tokenizer("pinned profile encoding failed"))?;
        tokens = tokens
            .checked_add(count)
            .ok_or(RenderError::InvalidRequest("prompt token count overflow"))?;
    }
    Ok(PromptCount {
        tokens,
        profile: id.profile(),
        accuracy: PromptAccuracy::ExactForPinnedTextProfile,
    })
}

pub fn render_chat(
    id: RendererId,
    request: &Value,
    context: &RenderContext,
) -> Result<RenderedPrompt, RenderError> {
    if request.get("add_special_tokens").and_then(Value::as_bool) == Some(true) {
        return Err(RenderError::Unsupported(
            "additional tokenizer BOS/EOS processing",
        ));
    }
    if request
        .get("truncate_prompt_tokens")
        .is_some_and(|value| !value.is_null())
    {
        return Err(RenderError::Unsupported("server-side prompt truncation"));
    }
    if request
        .get("chat_template_kwargs")
        .is_some_and(|value| !value.is_null() && !value.is_object())
    {
        return Err(invalid("chat template kwargs must be an object"));
    }
    if let Some(kwargs) = request
        .get("chat_template_kwargs")
        .and_then(Value::as_object)
    {
        for key in [
            "tokenize",
            "chat_template",
            "padding",
            "truncation",
            "max_length",
            "return_tensors",
            "return_dict",
            "return_assistant_tokens_mask",
            "add_special_tokens",
        ] {
            if kwargs.contains_key(key) {
                return Err(RenderError::Unsupported("template tokenizer override"));
            }
        }
    }
    if request.get("chat_template").is_some_and(|v| !v.is_null()) {
        return Err(RenderError::Unsupported("caller-supplied chat template"));
    }
    if !matches!(
        id,
        RendererId::GptOss120bHarmony | RendererId::GptOssSafeguard120bHarmony
    ) && request
        .get("continue_final_message")
        .and_then(Value::as_bool)
        == Some(true)
    {
        return Err(RenderError::Unsupported("assistant-prefill continuation"));
    }
    if id == RendererId::KimiK3 {
        return Ok(RenderedPrompt {
            segments: render_kimi_serving(request)?,
        });
    }
    if id == RendererId::KimiK3OfficialReference {
        return Ok(RenderedPrompt {
            segments: crate::kimi::render_kimi_segments(request)?,
        });
    }
    if matches!(
        id,
        RendererId::GptOss120bHarmony | RendererId::GptOssSafeguard120bHarmony
    ) {
        return crate::harmony::render_chat(request, context);
    }
    let text = if id == RendererId::DeepSeekV41Flash1017 {
        crate::deepseek::render_chat(request)?
    } else {
        render_jinja(id, request, context)?
    };
    Ok(RenderedPrompt {
        segments: vec![PromptSegment {
            text,
            policy: SpecialTokenPolicy::RawPipeline,
        }],
    })
}

fn invalid(reason: &'static str) -> RenderError {
    RenderError::InvalidRequest(reason)
}

/// Pinned vLLM's text-only content and tool-argument normalization. This does
/// not process images or fetch external resources.
fn normalize_messages(id: RendererId, request: &Value) -> Result<Value, RenderError> {
    let messages = request
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("messages must be an array"))?;
    if messages.is_empty() {
        return Err(invalid("messages must not be empty"));
    }
    let openai_content = matches!(
        id,
        RendererId::Glm53Official
            | RendererId::Glm53FlashOfficial
            | RendererId::Glm53TinfoilNvfp4
            | RendererId::Glm53FlashContinuum
            | RendererId::Gemma4Tinfoil
            | RendererId::Llama33Tinfoil
    );
    let mut result = Vec::with_capacity(messages.len());
    for message in messages {
        let source = message
            .as_object()
            .ok_or_else(|| invalid("message must be an object"))?;
        let role = source
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("message role must be a string"))?;
        let mut texts = Vec::new();
        match source.get("content") {
            None | Some(Value::Null) => {}
            Some(Value::String(text)) => texts.push(text.clone()),
            Some(Value::Array(parts)) => {
                for part in parts {
                    if let Some(text) = part.as_str() {
                        texts.push(text.to_owned());
                        continue;
                    }
                    match part.get("type").and_then(Value::as_str) {
                        Some("text") | Some("input_text") | Some("output_text") => texts.push(
                            part.get("text")
                                .and_then(Value::as_str)
                                .ok_or_else(|| invalid("text part must contain text"))?
                                .to_owned(),
                        ),
                        Some("refusal") => texts.push(
                            part.get("refusal")
                                .and_then(Value::as_str)
                                .ok_or_else(|| invalid("refusal part must contain text"))?
                                .to_owned(),
                        ),
                        Some("thinking") => texts.push(
                            part.get("thinking")
                                .and_then(Value::as_str)
                                .ok_or_else(|| invalid("thinking part must contain text"))?
                                .to_owned(),
                        ),
                        _ => {
                            return Err(RenderError::Unsupported(
                                "non-text content requires a modality processor",
                            ))
                        }
                    }
                }
            }
            _ => return Err(invalid("unsupported message content shape")),
        }
        if !openai_content {
            texts.retain(|text| !text.is_empty());
        }
        let content = if openai_content && role != "tool" {
            Value::Array(
                texts
                    .into_iter()
                    .map(|text| serde_json::json!({"type":"text","text":text}))
                    .collect(),
            )
        } else {
            Value::String(texts.join("\n"))
        };
        let mut target = Map::new();
        target.insert("role".into(), Value::String(role.to_owned()));
        target.insert("content".into(), content);
        for key in ["name", "task"] {
            if let Some(value @ Value::String(_)) = source.get(key) {
                target.insert(key.into(), value.clone());
            }
        }
        if role == "developer" {
            target.insert(
                "tools".into(),
                source.get("tools").cloned().unwrap_or(Value::Null),
            );
        }
        if role == "tool" {
            if let Some(value) = source.get("tool_call_id") {
                target.insert("tool_call_id".into(), value.clone());
            }
        }
        if role == "assistant" {
            if let Some(reasoning) = source
                .get("reasoning")
                .filter(|v| !v.is_null())
                .or_else(|| source.get("reasoning_content").filter(|v| !v.is_null()))
            {
                if !reasoning.is_string() {
                    return Err(invalid("reasoning must be a string"));
                }
                target.insert("reasoning".into(), reasoning.clone());
                target.insert("reasoning_content".into(), reasoning.clone());
            }
            if let Some(calls) = source.get("tool_calls").filter(|v| !v.is_null()) {
                let calls = calls
                    .as_array()
                    .ok_or_else(|| invalid("tool_calls must be an array"))?;
                let mut normalized = Vec::with_capacity(calls.len());
                for call in calls {
                    let mut call = call
                        .as_object()
                        .ok_or_else(|| invalid("tool call must be an object"))?
                        .clone();
                    if call
                        .get("type")
                        .and_then(Value::as_str)
                        .is_some_and(|s| s != "function")
                    {
                        return Err(RenderError::Unsupported("non-function tool call"));
                    }
                    let function = call
                        .get_mut("function")
                        .and_then(Value::as_object_mut)
                        .ok_or_else(|| invalid("tool call requires function object"))?;
                    let arguments = if id == RendererId::Llama33Tinfoil {
                        match function.get("arguments") {
                            Some(value @ Value::Object(_)) if !value.as_object().unwrap().is_empty() => value.clone(),
                            Some(value @ Value::Array(_)) if !value.as_array().unwrap().is_empty() => value.clone(),
                            Some(Value::String(text)) if !text.is_empty() => serde_json::from_str::<Value>(text).map_err(|_|invalid("invalid historical tool arguments for pinned Llama profile"))?,
                            Some(Value::Bool(true)) => return Err(invalid("invalid historical tool arguments for pinned Llama profile")),
                            Some(Value::Number(number)) if number.as_f64()!=Some(0.0) => return Err(invalid("invalid historical tool arguments for pinned Llama profile")),
                            _ => Value::Object(Map::new()),
                        }
                    } else {
                        match function.get("arguments") {
                            Some(value @ Value::Object(_)) => value.clone(),
                            Some(Value::String(text)) => serde_json::from_str::<Value>(text)
                                .ok()
                                .filter(Value::is_object)
                                .unwrap_or_else(|| Value::Object(Map::new())),
                            _ => Value::Object(Map::new()),
                        }
                    };
                    function.insert("arguments".into(), arguments);
                    normalized.push(Value::Object(call));
                }
                if !normalized.is_empty() {
                    target.insert("tool_calls".into(), Value::Array(normalized));
                }
            }
        }
        result.push(Value::Object(target));
    }
    // The native Kimi renderer does not run HF's developer-role fallback.
    // Its official encoder rejects developer messages instead.
    if id == RendererId::KimiK3 {
        return Ok(Value::Array(result));
    }
    let template = id.template()?;
    if id != RendererId::Llama33Tinfoil
        && result
            .iter()
            .any(|message| message.get("role").and_then(Value::as_str) == Some("developer"))
        && !template.contains("\"developer\"")
        && !template.contains("'developer'")
    {
        for message in &mut result {
            if message.get("role").and_then(Value::as_str) == Some("developer") {
                let message = message.as_object_mut().expect("normalized object");
                message.insert("role".into(), Value::String("system".into()));
                message.remove("tools");
            }
        }
        if result.iter().enumerate().any(|(index, message)| {
            index > 0 && message.get("role").and_then(Value::as_str) == Some("system")
        }) {
            let mut systems = Vec::new();
            let mut others = Vec::new();
            for message in result {
                if message.get("role").and_then(Value::as_str) == Some("system") {
                    let content = message.get("content").expect("normalized content");
                    let text = if let Some(text) = content.as_str() {
                        text.to_owned()
                    } else {
                        content
                            .as_array()
                            .expect("normalized content array")
                            .iter()
                            .filter_map(|part| part.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n")
                    };
                    if !text.is_empty() {
                        systems.push(text);
                    }
                } else {
                    others.push(message);
                }
            }
            result = vec![serde_json::json!({"role":"system","content":systems.join("\n\n")})];
            result.extend(others);
        }
    }
    Ok(Value::Array(result))
}

/// Pydantic-owned function fields have an insertion order independent of the
/// incoming JSON. Keep that order local to Jinja, without a global serde flag.
#[derive(Debug)]
struct OrderedObject(Vec<(String, JinjaValue)>);

impl Object for OrderedObject {
    fn get_value(self: &Arc<Self>, key: &JinjaValue) -> Option<JinjaValue> {
        let key = key.as_str()?;
        self.0
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
    }
    fn enumerate(self: &Arc<Self>) -> Enumerator {
        Enumerator::Values(
            self.0
                .iter()
                .map(|(key, _)| JinjaValue::from(key.clone()))
                .collect(),
        )
    }
}

/// Preserve the actual request Value's iteration order. Some consumers already
/// enable serde_json/preserve_order; MiniJinja's Serde map conversion otherwise
/// sorts nested maps and would count a different tool-schema serialization.
fn jinja_json(value: &Value) -> JinjaValue {
    match value {
        Value::Object(fields) => JinjaValue::from_object(OrderedObject(
            fields
                .iter()
                .map(|(key, value)| (key.clone(), jinja_json(value)))
                .collect(),
        )),
        Value::Array(values) => JinjaValue::from(values.iter().map(jinja_json).collect::<Vec<_>>()),
        _ => JinjaValue::from(Serde(value)),
    }
}

/// JSON object order inside a historical *string* tool argument is visible to
/// Python json.loads. Parse that string locally without changing serde's global
/// map representation or changing the request sent to the provider.
struct OrderedRawFields(Vec<(String, Box<serde_json::value::RawValue>)>);

impl<'de> serde::Deserialize<'de> for OrderedRawFields {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = OrderedRawFields;
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("JSON object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut fields = Vec::<(String, Box<serde_json::value::RawValue>)>::new();
                let mut positions = std::collections::HashMap::<String, usize>::new();
                while let Some((key, value)) =
                    map.next_entry::<String, Box<serde_json::value::RawValue>>()?
                {
                    // Python keeps the original position of duplicate keys and
                    // replaces their value with the last occurrence.
                    if let Some(position) = positions.get(&key) {
                        fields[*position].1 = value;
                    } else {
                        positions.insert(key.clone(), fields.len());
                        fields.push((key, value));
                    }
                }
                Ok(OrderedRawFields(fields))
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}

fn ordered_json(text: &str, depth: usize) -> Result<JinjaValue, RenderError> {
    if depth > 128 {
        return Err(RenderError::Unsupported("historical tool argument nesting"));
    }
    let text = text.trim();
    match text.as_bytes().first() {
        Some(b'{') => {
            let fields: OrderedRawFields =
                serde_json::from_str(text).map_err(|_| invalid("invalid historical tool JSON"))?;
            let mut result = Vec::with_capacity(fields.0.len());
            for (key, value) in fields.0 {
                result.push((key, ordered_json(value.get(), depth + 1)?));
            }
            Ok(JinjaValue::from_object(OrderedObject(result)))
        }
        Some(b'[') => {
            let values: Vec<Box<serde_json::value::RawValue>> =
                serde_json::from_str(text).map_err(|_| invalid("invalid historical tool JSON"))?;
            Ok(JinjaValue::from(
                values
                    .into_iter()
                    .map(|value| ordered_json(value.get(), depth + 1))
                    .collect::<Result<Vec<_>, _>>()?,
            ))
        }
        Some(b'"') => Ok(JinjaValue::from(
            serde_json::from_str::<String>(text)
                .map_err(|_| invalid("invalid historical tool JSON string"))?,
        )),
        _ => {
            if text == "true" {
                return Ok(JinjaValue::from(true));
            }
            if text == "false" {
                return Ok(JinjaValue::from(false));
            }
            if text == "null" {
                return Ok(JinjaValue::from(()));
            }
            if !text.contains(['.', 'e', 'E']) {
                if let Ok(number) = text.parse::<i64>() {
                    return Ok(JinjaValue::from(number));
                }
                if let Ok(number) = text.parse::<u64>() {
                    return Ok(JinjaValue::from(number));
                }
                return Err(RenderError::Unsupported(
                    "historical arbitrary-precision JSON integer",
                ));
            }
            let number = text
                .parse::<f64>()
                .map_err(|_| invalid("invalid historical tool JSON number"))?;
            if !number.is_finite() {
                return Err(RenderError::Unsupported(
                    "historical non-finite JSON number",
                ));
            }
            Ok(JinjaValue::from(number))
        }
    }
}

fn normalized_messages_jinja(id: RendererId, request: &Value) -> Result<JinjaValue, RenderError> {
    let normalized = normalize_messages(id, request)?;
    // System consolidation never reorders assistant calls. Pair their original
    // raw arguments with normalized calls in encounter order, independent of IDs.
    let mut original_calls = request["messages"]
        .as_array()
        .expect("validated messages")
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
        .filter_map(|message| message.get("tool_calls").and_then(Value::as_array))
        .flatten();
    let mut messages = Vec::new();
    for message in normalized.as_array().expect("normalized messages") {
        let mut fields = message
            .as_object()
            .expect("normalized object")
            .iter()
            .map(|(key, value)| (key.clone(), jinja_json(value)))
            .collect::<std::collections::BTreeMap<_, _>>();
        if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
            let mut jinja_calls = Vec::new();
            for call in calls {
                let original = original_calls
                    .next()
                    .ok_or_else(|| invalid("normalized tool call ordering mismatch"))?;
                let mut call_fields = call
                    .as_object()
                    .expect("normalized call")
                    .iter()
                    .map(|(key, value)| (key.clone(), jinja_json(value)))
                    .collect::<std::collections::BTreeMap<_, _>>();
                if let Some(text) = original
                    .get("function")
                    .and_then(|function| function.get("arguments"))
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    let decoded = ordered_json(text, 0);
                    let arguments = match decoded {
                        Ok(value)
                            if id == RendererId::Llama33Tinfoil
                                || value.kind() == ValueKind::Map =>
                        {
                            Some(value)
                        }
                        Ok(_) => None,
                        Err(RenderError::Unsupported(reason)) => {
                            return Err(RenderError::Unsupported(reason))
                        }
                        Err(error) if id == RendererId::Llama33Tinfoil => return Err(error),
                        Err(_) => None,
                    };
                    if let Some(arguments) = arguments {
                        let mut function = call["function"]
                            .as_object()
                            .expect("normalized function")
                            .iter()
                            .map(|(key, value)| (key.clone(), jinja_json(value)))
                            .collect::<std::collections::BTreeMap<_, _>>();
                        function.insert("arguments".into(), arguments);
                        call_fields.insert("function".into(), JinjaValue::from(function));
                    }
                }
                jinja_calls.push(JinjaValue::from(call_fields));
            }
            fields.insert("tool_calls".into(), JinjaValue::from(jinja_calls));
        }
        messages.push(JinjaValue::from(fields));
    }
    Ok(JinjaValue::from(messages))
}

fn normalized_tools(id: RendererId, tools: &Value) -> Result<JinjaValue, RenderError> {
    let tools = tools
        .as_array()
        .ok_or_else(|| invalid("tools must be an array"))?;
    let mut result = Vec::with_capacity(tools.len());
    for tool in tools {
        if tool
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind != "function")
        {
            return Err(RenderError::Unsupported("non-function tool definition"));
        }
        let function = tool
            .get("function")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid("tool requires a function object"))?;
        if function.get("name").and_then(Value::as_str).is_none() {
            return Err(invalid("tool function name must be a string"));
        }
        let mut fields = Vec::new();
        for key in ["name", "description", "parameters"] {
            fields.push((
                key.to_owned(),
                jinja_json(function.get(key).unwrap_or(&Value::Null)),
            ));
        }
        let optional_fields: &[&str] = if id == RendererId::Llama33Tinfoil {
            &["defer_loading"]
        } else {
            &["strict", "defer_loading"]
        };
        for key in optional_fields.iter().copied() {
            if let Some(value) = function.get(key).filter(|v| !v.is_null()).or_else(|| {
                if key == "defer_loading" {
                    tool.get(key).filter(|v| !v.is_null())
                } else {
                    None
                }
            }) {
                fields.push((key.to_owned(), jinja_json(value)));
            }
        }
        if id == RendererId::Llama33Tinfoil {
            // vLLM 0.22's FunctionDefinition preserves Pydantic extra fields;
            // 0.31 prunes them. Preserve the incoming wire order of extras.
            for (key, value) in function {
                if !["name", "description", "parameters", "defer_loading"].contains(&key.as_str()) {
                    fields.push((key.clone(), jinja_json(value)));
                }
            }
        }
        let mut fields = vec![
            ("type".into(), JinjaValue::from("function")),
            (
                "function".into(),
                JinjaValue::from_object(OrderedObject(fields)),
            ),
        ];
        if let Some(value) = tool.get("defer_loading").filter(|v| !v.is_null()) {
            fields.push(("defer_loading".into(), jinja_json(value)));
        }
        if id == RendererId::Llama33Tinfoil {
            let tool = tool
                .as_object()
                .expect("tool function object was validated");
            for (key, value) in tool {
                if !["type", "function", "defer_loading"].contains(&key.as_str()) {
                    fields.push((key.clone(), jinja_json(value)));
                }
            }
        }
        result.push(JinjaValue::from_object(OrderedObject(fields)));
    }
    Ok(JinjaValue::from(result))
}

/// Final ChatParams.with_defaults merge: None and the sentinel "auto" do not
/// override renderer defaults. OnlineRenderer always supplies tokenize=false,
/// so even an otherwise empty defaults dictionary takes this merge path.
fn filter_template_defaults(variables: &mut Map<String, Value>) {
    variables.retain(|_, value| !value.is_null() && value.as_str() != Some("auto"));
}

fn json_string(value: &JinjaValue) -> Result<String, RenderError> {
    let mut text = String::new();
    write_python_json(value, &mut text, None, 0, ", ", ": ", false, false)
        .map_err(|_| invalid("normalized JSON serialization failed"))?;
    Ok(text)
}

fn has_nonfinite_json_constant(text: &str) -> bool {
    let mut quoted = false;
    let mut escaped = false;
    for (offset, character) in text.char_indices() {
        if quoted {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                quoted = false;
            }
            continue;
        }
        if character == '"' {
            quoted = true;
            continue;
        }
        let before = text[..offset].chars().next_back();
        if before.is_some_and(|c| !matches!(c, ' ' | '\t' | '\r' | '\n' | '[' | '{' | ':' | ',')) {
            continue;
        }
        for constant in ["NaN", "Infinity", "-Infinity"] {
            if let Some(rest) = text[offset..].strip_prefix(constant) {
                if rest
                    .chars()
                    .next()
                    .is_none_or(|c| matches!(c, ' ' | '\t' | '\r' | '\n' | ',' | ']' | '}'))
                {
                    return true;
                }
            }
        }
    }
    false
}

/// Preserve Python json.loads insertion order and its numeric normalization
/// without changing serde_json's process-wide map representation. The official
/// encoder accepts JSON argument strings and preserves their nested literals;
/// supply a synthetic json.dumps-equivalent string after serving normalization.
fn kimi_history_arguments(request: &Value, messages: &mut Value) -> Result<(), RenderError> {
    let mut original_calls = request["messages"]
        .as_array()
        .expect("validated messages")
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
        .filter_map(|message| message.get("tool_calls").and_then(Value::as_array))
        .flatten();
    for message in messages.as_array_mut().expect("normalized messages") {
        let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) else {
            continue;
        };
        for call in calls {
            let original = original_calls
                .next()
                .ok_or_else(|| invalid("normalized tool call ordering mismatch"))?;
            let original_arguments = original.get("function").and_then(|f| f.get("arguments"));
            let decoded = match original_arguments {
                Some(Value::String(text)) if !text.is_empty() => {
                    if has_nonfinite_json_constant(text) {
                        return Err(RenderError::Unsupported(
                            "historical non-finite JSON number",
                        ));
                    }
                    match ordered_json(text, 0) {
                        Ok(value) if value.kind() == ValueKind::Map => value,
                        Err(RenderError::Unsupported(reason)) => {
                            return Err(RenderError::Unsupported(reason))
                        }
                        _ => jinja_json(&serde_json::json!({})),
                    }
                }
                Some(value @ Value::Object(_)) => jinja_json(value),
                _ => jinja_json(&serde_json::json!({})),
            };
            call["function"]["arguments"] = Value::String(json_string(&decoded)?);
        }
    }
    Ok(())
}

fn kimi_tool_result_aliases(messages: Value) -> Value {
    let messages = messages.as_array().expect("normalized messages");
    let mut normalized = Vec::with_capacity(messages.len());
    let mut index = 0;
    while index < messages.len() {
        let message = &messages[index];
        normalized.push(message.clone());
        index += 1;
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(calls) = message
            .get("tool_calls")
            .and_then(Value::as_array)
            .filter(|v| !v.is_empty())
        else {
            continue;
        };
        let mut targets = std::collections::HashMap::<String, (usize, String)>::new();
        for (position, call) in calls.iter().enumerate() {
            let Some(name) = call
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            if let Some(id) = call
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                targets
                    .entry(id.to_owned())
                    .or_insert((position, name.to_owned()));
            }
            targets
                .entry(format!("{name}:{position}"))
                .or_insert((position, name.to_owned()));
        }
        let start = index;
        while index < messages.len()
            && messages[index].get("role").and_then(Value::as_str) == Some("tool")
        {
            index += 1;
        }
        let resolved = messages[start..index]
            .iter()
            .enumerate()
            .map(|(order, message)| {
                let target = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .and_then(|id| targets.get(id))?;
                let mut enriched = message.clone();
                enriched["tool"] = Value::String(target.1.clone());
                enriched["index"] = Value::from(target.0 + 1);
                Some((target.0, order, enriched))
            })
            .collect::<Option<Vec<_>>>();
        if let Some(mut resolved) = resolved {
            resolved.sort_by_key(|(position, order, _)| (*position, *order));
            normalized.extend(resolved.into_iter().map(|(_, _, message)| message));
        } else {
            normalized.extend_from_slice(&messages[start..index]);
        }
    }
    Value::Array(normalized)
}

fn kimi_response_format(value: &Value) -> Result<Value, RenderError> {
    let source = value
        .as_object()
        .ok_or_else(|| invalid("response format must be an object"))?;
    let kind = source
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("response format requires type"))?;
    if !matches!(kind, "text" | "json_object" | "json_schema") {
        return Err(RenderError::Unsupported("structural-tag response format"));
    }
    let mut format = Map::new();
    format.insert("type".into(), Value::String(kind.into()));
    if let Some(schema) = source.get("json_schema").filter(|v| !v.is_null()) {
        let schema = schema
            .as_object()
            .ok_or_else(|| invalid("response JSON schema must be an object"))?;
        // Unlike FunctionDefinition, this Pydantic model keeps extra fields.
        // They become prompt-visible when the schema itself is absent and the
        // official encoder falls back to the entire json_schema object.
        let mut normalized = schema
            .iter()
            .filter(|(key, value)| key.as_str() != "schema" && !value.is_null())
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<Map<_, _>>();
        if let Some(value) = schema
            .get("schema")
            .or_else(|| schema.get("json_schema"))
            .filter(|v| !v.is_null())
        {
            normalized.insert("json_schema".into(), value.clone());
        }
        format.insert("json_schema".into(), Value::Object(normalized));
    }
    Ok(Value::Object(format))
}

fn python_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

/// vLLM db9527a's native Kimi boundary, followed by the separately tested
/// Moonshot encoder. This preprocessing never changes the outbound request.
fn render_kimi_serving(request: &Value) -> Result<Vec<PromptSegment>, RenderError> {
    let mut messages = normalize_messages(RendererId::KimiK3, request)?;
    kimi_history_arguments(request, &mut messages)?;
    let messages = kimi_tool_result_aliases(messages);
    let mut kwargs = request
        .get("chat_template_kwargs")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for protected in ["messages", "conversation", "tools"] {
        if kwargs.contains_key(protected) {
            return Err(RenderError::Unsupported(
                "Kimi tokenizer/template override is not supported",
            ));
        }
    }
    if let Some(effort) = request
        .get("reasoning_effort")
        .filter(|v| !v.is_null() && v.as_str() != Some("auto"))
    {
        kwargs.insert("reasoning_effort".into(), effort.clone());
        kwargs
            .entry("enable_thinking")
            .or_insert(Value::Bool(effort.as_str() != Some("none")));
    }
    for (key, default) in [
        ("add_generation_prompt", true),
        ("continue_final_message", false),
    ] {
        kwargs.insert(
            key.into(),
            request.get(key).cloned().unwrap_or(Value::Bool(default)),
        );
    }
    filter_template_defaults(&mut kwargs);
    if let Some(enable) = kwargs.remove("enable_thinking") {
        kwargs.entry("thinking").or_insert(enable);
    }
    if let Some(effort) = kwargs.remove("reasoning_effort") {
        if effort.as_str() == Some("none") {
            kwargs.entry("thinking").or_insert(Value::Bool(false));
        } else {
            kwargs.entry("thinking_effort").or_insert(effort);
        }
    }
    if let Some(effort) = kwargs.get("thinking_effort") {
        if !matches!(effort.as_str(), Some("low" | "high" | "max")) {
            return Err(invalid("Kimi thinking effort must be low, high, or max"));
        }
    }
    if let Some(thinking) = kwargs.get_mut("thinking") {
        *thinking = Value::Bool(python_truthy(thinking));
    }
    let mut normalized = Map::new();
    normalized.insert("messages".into(), messages);
    if let Some(tools) = request.get("tools").filter(|v| !v.is_null()) {
        let tools = serde_json::to_value(normalized_tools(RendererId::KimiK3, tools)?)
            .map_err(|_| invalid("normalized tool serialization failed"))?;
        normalized.insert("tools".into(), tools);
        if let Some(choice) = request.get("tool_choice").filter(|v| {
            !v.is_null()
                && v.as_str() != Some("auto")
                && request.get("tools").is_some_and(python_truthy)
        }) {
            kwargs.insert("tool_choice".into(), choice.clone());
        }
    }
    if let Some(format) = request.get("response_format").filter(|v| !v.is_null()) {
        kwargs.insert("response_format".into(), kimi_response_format(format)?);
    }
    normalized.insert("chat_template_kwargs".into(), Value::Object(kwargs));
    crate::kimi::render_kimi_segments(&Value::Object(normalized))
}

fn render_jinja(
    id: RendererId,
    request: &Value,
    context: &RenderContext,
) -> Result<String, RenderError> {
    let template = id.template()?;
    let mut variables = request
        .get("chat_template_kwargs")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for protected in [
        "messages",
        "tools",
        "bos_token",
        "eos_token",
        "strftime_now",
        "raise_exception",
    ] {
        if variables.contains_key(protected) {
            return Err(invalid("chat template kwargs override a protected field"));
        }
    }
    let messages = normalized_messages_jinja(id, request)?;
    // Match ChatCompletionRequest.build_chat_params: non-null top-level effort
    // wins; explicitly supplied template kwargs remain the default otherwise.
    if let Some(value) = request.get("reasoning_effort").filter(|v| !v.is_null()) {
        variables.insert("reasoning_effort".into(), value.clone());
        variables
            .entry("enable_thinking")
            .or_insert(Value::Bool(value.as_str() != Some("none")));
    }
    for key in ["response_format", "documents"] {
        if let Some(value) = request.get(key).filter(|v| !v.is_null()) {
            variables.insert(key.into(), value.clone());
        }
    }
    if let Some(tools) = request.get("tools").filter(|v| !v.is_null()) {
        if !tools.is_array() {
            return Err(invalid("tools must be an array"));
        }
        if let Some(choice) = request.get("tool_choice").filter(|v| !v.is_null()) {
            variables.insert("tool_choice".into(), choice.clone());
        }
    }
    variables.insert(
        "add_generation_prompt".into(),
        request
            .get("add_generation_prompt")
            .cloned()
            .unwrap_or(Value::Bool(true)),
    );
    filter_template_defaults(&mut variables);
    let (bos, eos) = match id {
        RendererId::Llama33Tinfoil => ("<|begin_of_text|>", "<|eot_id|>"),
        RendererId::Gemma4Tinfoil => ("<bos>", "<eos>"),
        RendererId::GptOss120bJinjaReference | RendererId::GptOssSafeguard120bJinjaReference => {
            ("<|startoftext|>", "<|return|>")
        }
        _ => ("", "<|endoftext|>"),
    };
    variables.insert("bos_token".into(), Value::String(bos.into()));
    variables.insert("eos_token".into(), Value::String(eos.into()));
    let mut env = Environment::new();
    env.set_syntax(
        minijinja::syntax::SyntaxConfig::builder()
            .trim_blocks(true)
            .lstrip_blocks(true)
            .build()
            .map_err(|_| invalid("invalid pinned Jinja configuration"))?,
    );
    // Jinja2 formats bare float expressions with Python str(float), while
    // MiniJinja's default uses a different decimal/scientific threshold.
    // Gemma's argument macro writes numbers directly rather than via tojson.
    env.set_formatter(|out, state, value| {
        if value.kind() == ValueKind::Number {
            if let Ok(Value::Number(number)) = serde_json::to_value(value) {
                if number.is_f64() {
                    return minijinja::escape_formatter(
                        out,
                        state,
                        &JinjaValue::from(crate::kimi::python_float(&number)),
                    );
                }
            }
        }
        minijinja::escape_formatter(out, state, value)
    });
    env.set_unknown_method_callback(|state, value, method, args| {
        if let Some(text) = value.as_str() {
            if ["strip", "lstrip", "rstrip"].contains(&method) {
                let (chars,): (Option<&str>,) = minijinja::value::from_args(args)?;
                let predicate = |character: char| {
                    chars.map_or_else(
                        || python_whitespace(character),
                        |chars| chars.contains(character),
                    )
                };
                return Ok(JinjaValue::from(
                    match method {
                        "lstrip" => text.trim_start_matches(predicate),
                        "rstrip" => text.trim_end_matches(predicate),
                        _ => text.trim_matches(predicate),
                    }
                    .to_owned(),
                ));
            }
        }
        minijinja_contrib::pycompat::unknown_method_callback(state, value, method, args)
    });
    env.add_filter("trim", |value: JinjaValue, chars: Option<String>| {
        let text = value
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| value.to_string());
        let predicate = |character: char| {
            chars.as_deref().map_or_else(
                || python_whitespace(character),
                |chars| chars.contains(character),
            )
        };
        text.trim_matches(predicate).to_owned()
    });
    env.add_filter("tojson", python_tojson);
    env.add_filter("from_json", from_json);
    env.add_filter("fromjson", from_json);
    env.add_function(
        "raise_exception",
        |_value: JinjaValue| -> Result<JinjaValue, JinjaError> {
            Err(JinjaError::new(
                ErrorKind::InvalidOperation,
                "template rejected request",
            ))
        },
    );
    let date = context
        .date
        .as_deref()
        .map(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d"))
        .transpose()
        .map_err(|_| invalid("render date must be YYYY-MM-DD"))?;
    env.add_function(
        "strftime_now",
        move |format: &str| -> Result<String, JinjaError> {
            let date = date.ok_or_else(|| {
                JinjaError::new(
                    ErrorKind::InvalidOperation,
                    "profile requires a render date",
                )
            })?;
            match format {
                "%Y-%m-%d" => Ok(date.format(format).to_string()),
                "%d %b %Y" => {
                    let months = [
                        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct",
                        "Nov", "Dec",
                    ];
                    Ok(format!(
                        "{:02} {} {:04}",
                        date.day(),
                        months[date.month0() as usize],
                        date.year()
                    ))
                }
                _ => Err(JinjaError::new(
                    ErrorKind::InvalidOperation,
                    "unsupported date format",
                )),
            }
        },
    );
    env.add_template("profile", template)
        .map_err(|_| invalid("pinned chat template could not compile"))?;
    let mut jinja_variables = variables
        .into_iter()
        .map(|(key, value)| (key, jinja_json(&value)))
        .collect::<std::collections::BTreeMap<_, _>>();
    jinja_variables.insert("messages".into(), messages);
    if let Some(tools) = request.get("tools").filter(|v| !v.is_null()) {
        jinja_variables.insert("tools".into(), normalized_tools(id, tools)?);
    }
    env.get_template("profile")
        .and_then(|t| t.render(JinjaValue::from(jinja_variables)))
        .map_err(|_| invalid("pinned chat template rejected request"))
}

fn python_whitespace(character: char) -> bool {
    character.is_whitespace() || matches!(character, '\u{1c}'..='\u{1f}')
}

fn from_json(value: &str) -> Result<JinjaValue, JinjaError> {
    let parsed: Value = serde_json::from_str(value)
        .map_err(|_| JinjaError::new(ErrorKind::InvalidOperation, "invalid JSON argument"))?;
    Ok(jinja_json(&parsed))
}

/// Transformers replaces Jinja's HTML-safe `tojson` with Python json.dumps,
/// whose spaces, Unicode and escaping are significant to prompt tokenization.
fn python_tojson(value: JinjaValue, kwargs: Kwargs) -> Result<String, JinjaError> {
    let indent: Option<usize> = kwargs.get("indent")?;
    let ensure_ascii = kwargs.get::<Option<bool>>("ensure_ascii")?.unwrap_or(false);
    let sort_keys = kwargs.get::<Option<bool>>("sort_keys")?.unwrap_or(false);
    let separators: Option<Vec<String>> = kwargs.get("separators")?;
    kwargs.assert_all_used()?;
    let (item_separator, key_separator) = if let Some(separators) = separators.as_ref() {
        if separators.len() != 2 {
            return Err(JinjaError::new(
                ErrorKind::InvalidOperation,
                "invalid JSON separators",
            ));
        }
        (separators[0].as_str(), separators[1].as_str())
    } else if indent.is_some() {
        (",", ": ")
    } else {
        (", ", ": ")
    };
    let mut out = String::new();
    write_python_json(
        &value,
        &mut out,
        indent,
        0,
        item_separator,
        key_separator,
        ensure_ascii,
        sort_keys,
    )?;
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn write_python_json(
    value: &JinjaValue,
    out: &mut String,
    indent: Option<usize>,
    depth: usize,
    item_separator: &str,
    key_separator: &str,
    ascii: bool,
    sort_keys: bool,
) -> Result<(), JinjaError> {
    fn quote(value: &str, ascii: bool) -> String {
        let encoded = serde_json::to_string(value).expect("string serialization cannot fail");
        if !ascii {
            return encoded;
        }
        let mut escaped = String::new();
        for c in encoded.chars() {
            if c.is_ascii() {
                escaped.push(c);
            } else {
                for unit in c.encode_utf16(&mut [0; 2]).iter() {
                    escaped.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
        escaped
    }
    let newline = |out: &mut String, next_depth: usize| {
        if let Some(indent) = indent {
            out.push('\n');
            out.extend(std::iter::repeat_n(' ', indent.saturating_mul(next_depth)));
        }
    };
    match value.kind() {
        ValueKind::String => out.push_str(&quote(value.as_str().unwrap_or_default(), ascii)),
        ValueKind::Seq | ValueKind::Iterable => {
            let items = value.try_iter()?.collect::<Vec<_>>();
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(item_separator);
                }
                newline(out, depth + 1);
                write_python_json(
                    item,
                    out,
                    indent,
                    depth + 1,
                    item_separator,
                    key_separator,
                    ascii,
                    sort_keys,
                )?;
            }
            if !items.is_empty() {
                newline(out, depth);
            }
            out.push(']');
        }
        ValueKind::Map => {
            out.push('{');
            let mut keys = value.try_iter()?.collect::<Vec<_>>();
            if sort_keys {
                keys.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
            }
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push_str(item_separator);
                }
                newline(out, depth + 1);
                out.push_str(&quote(
                    key.as_str().ok_or_else(|| {
                        JinjaError::new(ErrorKind::InvalidOperation, "JSON key must be a string")
                    })?,
                    ascii,
                ));
                out.push_str(key_separator);
                write_python_json(
                    &value.get_item(key)?,
                    out,
                    indent,
                    depth + 1,
                    item_separator,
                    key_separator,
                    ascii,
                    sort_keys,
                )?;
            }
            if !keys.is_empty() {
                newline(out, depth);
            }
            out.push('}');
        }
        _ => {
            let value = serde_json::to_value(value).map_err(|_| {
                JinjaError::new(ErrorKind::InvalidOperation, "JSON serialization failed")
            })?;
            if let Value::Number(number) = &value {
                if number.is_f64() {
                    out.push_str(&crate::kimi::python_float(number));
                } else {
                    out.push_str(&value.to_string());
                }
            } else {
                out.push_str(&value.to_string());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kimi_actual_serving_python_goldens() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/kimi_serving.json")).unwrap();
        let mut failures = Vec::new();
        for case in fixture["cases"].as_array().unwrap() {
            let actual = render_chat(
                RendererId::KimiK3,
                &case["request"],
                &RenderContext::default(),
            );
            let label = case["name"].as_str().unwrap();
            if case
                .get("profile_unsupported")
                .is_some_and(|v| v == &Value::Bool(true))
            {
                if !matches!(actual, Err(RenderError::Unsupported(_))) {
                    failures.push(format!(
                        "{label}: expected explicit profile unsupported result"
                    ));
                }
                continue;
            }
            if case.get("error").is_some() {
                if actual.is_ok() {
                    failures.push(format!("{label}: expected source rejection"));
                }
                continue;
            }
            let expected = case["segments"]
                .as_array()
                .unwrap()
                .iter()
                .map(|segment| PromptSegment {
                    text: segment["text"].as_str().unwrap().to_owned(),
                    policy: if segment["allow_special"].as_bool().unwrap() {
                        SpecialTokenPolicy::AllowAll
                    } else {
                        SpecialTokenPolicy::Ordinary
                    },
                })
                .collect::<Vec<_>>();
            match actual {
                Ok(actual) if actual.segments == expected => {}
                Ok(actual) => {
                    let mismatch = actual
                        .segments
                        .iter()
                        .zip(&expected)
                        .position(|(a, b)| a != b)
                        .unwrap_or(actual.segments.len().min(expected.len()));
                    failures.push(format!(
                        "{label}: segment mismatch at {mismatch} (Rust {} vs Python {})",
                        actual.segments.len(),
                        expected.len()
                    ));
                }
                Err(error) => failures.push(format!("{label}: {error}")),
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn kimi_nonfinite_history_is_explicitly_unqualified() {
        for arguments in [
            r#"{"x":NaN}"#,
            r#"{"x":Infinity}"#,
            r#"{"x":-Infinity}"#,
            r#"{"x":1e999}"#,
        ] {
            let request = serde_json::json!({"messages":[{"role":"assistant","tool_calls":[{"id":"one","function":{"name":"lookup","arguments":arguments}}]}]});
            assert!(matches!(
                render_chat(RendererId::KimiK3, &request, &RenderContext::default()),
                Err(RenderError::Unsupported(_))
            ));
        }
        assert!(!has_nonfinite_json_constant(
            r#"{"x":"NaN Infinity -Infinity","y":"escaped \\\"NaN"}"#
        ));
    }

    #[test]
    fn jinja_conversion_preserves_consumer_json_map_order() {
        let request = serde_json::json!({"z":{"last":1,"first":2},"a":3});
        let expected = request
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let converted = jinja_json(&request);
        let actual = converted
            .try_iter()
            .unwrap()
            .map(|key| key.as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
        let nested = request["z"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let actual = converted
            .get_item(&JinjaValue::from("z"))
            .unwrap()
            .try_iter()
            .unwrap()
            .map(|key| key.as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(actual, nested);
    }

    fn fixture_profile(value: &str) -> RendererId {
        match value {
            "Glm53Official" => RendererId::Glm53Official,
            "Glm53FlashOfficial" => RendererId::Glm53FlashOfficial,
            "Glm53TinfoilNvfp4" => RendererId::Glm53TinfoilNvfp4,
            "Glm53FlashTinfoilNvfp4" => RendererId::Glm53FlashTinfoilNvfp4,
            "Glm53Continuum" => RendererId::Glm53Continuum,
            "Glm53FlashContinuum" => RendererId::Glm53FlashContinuum,
            "GptOss120bJinjaReference" => RendererId::GptOss120bJinjaReference,
            "GptOssSafeguard120bJinjaReference" => RendererId::GptOssSafeguard120bJinjaReference,
            "Gemma4Tinfoil" => RendererId::Gemma4Tinfoil,
            "Llama33Tinfoil" => RendererId::Llama33Tinfoil,
            _ => panic!("unmapped synthetic fixture profile"),
        }
    }

    #[test]
    fn hf_python_render_goldens() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/hf_chat.json")).unwrap();
        let mut failures = Vec::new();
        for case in fixture["cases"].as_array().unwrap() {
            let profile = fixture_profile(case["profile"].as_str().unwrap());
            let context = RenderContext {
                date: Some(case["date"].as_str().unwrap().to_owned()),
            };
            let rendered = render_chat(profile, &case["request"], &context);
            let label = format!("{:?}/{}", profile, case["name"].as_str().unwrap());
            if case.get("error").is_some() {
                if rendered.is_ok() {
                    failures.push(format!("{label}: expected Python template rejection"));
                }
                continue;
            }
            match rendered {
                Err(error) => failures.push(format!("{label}: {error}")),
                Ok(rendered) => {
                    assert_eq!(rendered.segments.len(), 1);
                    assert_eq!(rendered.segments[0].policy, SpecialTokenPolicy::RawPipeline);
                    let actual = &rendered.segments[0].text;
                    let expected = case["rendered"].as_str().unwrap();
                    if actual != expected {
                        let offset = actual
                            .bytes()
                            .zip(expected.bytes())
                            .position(|(a, b)| a != b)
                            .unwrap_or(actual.len().min(expected.len()));
                        failures.push(format!(
                            "{label}: rendered bytes differ at {offset} (Rust {} vs Python {})",
                            actual.len(),
                            expected.len()
                        ));
                    }
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn pinned_template_hashes_match_provenance_manifest() {
        use sha2::{Digest, Sha256};
        let manifest: Value =
            serde_json::from_str(include_str!("../profiles/manifest.json")).unwrap();
        for entry in manifest.as_array().unwrap() {
            let template = match entry["file"].as_str().unwrap() {
                "glm53-official.jinja" => RendererId::Glm53Official,
                "glm53-flash-official.jinja" => RendererId::Glm53FlashOfficial,
                "glm53-tinfoil-nvfp4.jinja" => RendererId::Glm53TinfoilNvfp4,
                "glm53-continuum.jinja" => RendererId::Glm53Continuum,
                "glm53-flash-continuum.jinja" => RendererId::Glm53FlashContinuum,
                "gpt-oss-120b.jinja" => RendererId::GptOss120bJinjaReference,
                "gpt-oss-safeguard-120b.jinja" => RendererId::GptOssSafeguard120bJinjaReference,
                "gemma4-tinfoil.jinja" => RendererId::Gemma4Tinfoil,
                "llama33-tinfoil.jinja" => RendererId::Llama33Tinfoil,
                _ => panic!("unmapped pinned template"),
            }
            .template()
            .unwrap();
            assert_eq!(
                format!("{:x}", Sha256::digest(template.as_bytes())),
                entry["sha256"].as_str().unwrap()
            );
        }
    }

    #[test]
    fn auto_is_unresolved_and_provider_profile_identities_are_distinct() {
        assert!(RendererId::for_model("auto").is_none());
        let request = serde_json::json!({"messages":[{"role":"user","content":"hello"}]});
        assert!(render_chat(
            RendererId::Glm53Continuum,
            &request,
            &RenderContext::default()
        )
        .is_ok());
        assert_ne!(
            RendererId::Glm53Continuum.profile(),
            RendererId::Glm53TinfoilNvfp4.profile()
        );
        assert_ne!(
            RendererId::Glm53FlashContinuum.profile(),
            RendererId::Glm53FlashTinfoilNvfp4.profile()
        );
    }

    #[test]
    fn images_are_not_counted_as_visible_placeholders() {
        let request = serde_json::json!({"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://example.invalid/private"}}]}]});
        assert!(matches!(
            render_chat(
                RendererId::Gemma4Tinfoil,
                &request,
                &RenderContext::default()
            ),
            Err(RenderError::Unsupported(_))
        ));
    }

    #[test]
    fn error_messages_do_not_include_prompt_or_template_exception_text() {
        let request =
            serde_json::json!({"messages":[{"role":"user","content":{"secret":"DO_NOT_LOG"}}]});
        let error = render_chat(
            RendererId::GptOss120bJinjaReference,
            &request,
            &RenderContext::default(),
        )
        .unwrap_err();
        assert!(!error.to_string().contains("DO_NOT_LOG"));
    }
}
