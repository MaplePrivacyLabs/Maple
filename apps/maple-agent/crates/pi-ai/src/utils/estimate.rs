//! Usage-anchored context estimates from `utils/estimate.ts`.

use super::raw_message;
use crate::types::{
    AssistantContent, JsObject, JsString, JsValue, Message, StopReason, TranscriptContext, Usage,
    UserContent, UserMessageContent,
};
use crate::utils::js_json::{stringify, utf16_len};
use crate::utils::js_value::to_js_value;
use crate::utils::text::get_system_message_text;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsageEstimate {
    pub tokens: f64,
    pub usage_tokens: f64,
    pub trailing_tokens: f64,
    pub last_usage_index: Option<usize>,
}

const CHARS_PER_TOKEN: f64 = 4.0;
const ESTIMATED_IMAGE_CHARS: usize = 4800;

pub fn calculate_context_tokens(usage: &Usage) -> f64 {
    if usage.total_tokens != 0.0 && !usage.total_tokens.is_nan() {
        usage.total_tokens
    } else {
        usage.input + usage.output + usage.cache_read + usage.cache_write
    }
}

fn safe_json_stringify(value: &impl serde::Serialize) -> String {
    to_js_value(value).map_or_else(|_| "[unserializable]".to_owned(), |value| stringify(&value))
}

fn block_chars(content: &[UserContent]) -> usize {
    content
        .iter()
        .map(|block| match block {
            UserContent::Text(block) => block.text.utf16_len(),
            UserContent::Image(_) => ESTIMATED_IMAGE_CHARS,
        })
        .sum()
}

pub fn estimate_text_tokens(text: &JsString) -> f64 {
    (text.utf16_len() as f64 / CHARS_PER_TOKEN).ceil()
}

pub fn estimate_text_and_image_content_tokens(content: &UserMessageContent) -> f64 {
    let chars = match content {
        UserMessageContent::Text(text) => text.utf16_len(),
        UserMessageContent::Blocks(blocks) => block_chars(blocks),
    };
    (chars as f64 / CHARS_PER_TOKEN).ceil()
}

pub fn estimate_message_tokens(message: &Message) -> Result<f64, JsString> {
    Ok(match message {
        Message::System(message) => {
            estimate_text_tokens(&get_system_message_text(message))
                + estimate_tools_tokens(message.tools_added.as_deref())
                + estimate_tools_tokens(message.tools_removed.as_deref())
        }
        Message::User(message) => estimate_text_and_image_content_tokens(&message.content),
        Message::ToolResult(message) => {
            (block_chars(&message.content) as f64 / CHARS_PER_TOKEN).ceil()
        }
        Message::Assistant(message) => {
            let chars: usize = message
                .content
                .iter()
                .map(|block| match block {
                    AssistantContent::Text(block) => block.text.utf16_len(),
                    AssistantContent::Thinking(block) => block.thinking.utf16_len(),
                    AssistantContent::ToolCall(block) => {
                        block.name.utf16_len()
                            + utf16_len(&block.arguments.read(safe_json_stringify))
                    }
                })
                .sum();
            (chars as f64 / CHARS_PER_TOKEN).ceil()
        }
        Message::Raw(message) => message.read(estimate_raw_message_tokens)?,
    })
}

fn estimate_raw_message_tokens(message: &JsObject) -> Result<f64, JsString> {
    let role = message.get("role").and_then(JsValue::as_str);
    if role == Some("system") {
        let mut tokens = estimate_text_tokens(&raw_message::system_text(message)?);
        for field in ["toolsAdded", "toolsRemoved"] {
            if let Some(value) = message.get(field).filter(|value| !value.is_null())
                && raw_message::length(Some(value))? > 0.0
            {
                tokens += estimate_text_tokens(&stringify(value).into());
            }
        }
        return Ok(tokens);
    }
    let content = message.get("content");
    if matches!(role, Some("user" | "toolResult"))
        && let Some(JsValue::String(text)) = content
    {
        return Ok(estimate_text_tokens(text));
    }
    let values: Vec<JsValue> = match content {
        Some(JsValue::Array(values)) => values.clone(),
        Some(JsValue::String(text)) => text
            .units()
            .map(|unit| JsValue::String(JsString::from_utf16(vec![unit])))
            .collect(),
        _ => {
            return Err(if matches!(role, Some("user" | "toolResult")) {
                "content is not iterable"
            } else {
                "message.content is not iterable"
            }
            .into());
        }
    };
    let mut chars = 0.0;
    for block in values {
        let kind = raw_message::property(Some(&block), "type")?.and_then(JsValue::as_str);
        chars += if matches!(role, Some("user" | "toolResult")) {
            if kind == Some("text") {
                raw_message::length(block.get("text"))?
            } else {
                ESTIMATED_IMAGE_CHARS as f64
            }
        } else {
            match kind {
                Some("text") => raw_message::length(block.get("text"))?,
                Some("thinking") => raw_message::length(block.get("thinking"))?,
                _ => {
                    raw_message::length(block.get("name"))?
                        + block
                            .get("arguments")
                            .map_or(9, |value| utf16_len(&stringify(value)))
                            as f64
                }
            }
        };
    }
    Ok((chars / CHARS_PER_TOKEN).ceil())
}

fn get_last_assistant_usage_info(messages: &[Message]) -> Result<Option<(f64, usize)>, JsString> {
    let mut latest_prefix_timestamp = f64::NEG_INFINITY;
    let mut usage_info = None;
    for (index, message) in messages.iter().enumerate() {
        let timestamp = match message {
            Message::Raw(raw) => raw.read(|object| raw_message::number(object.get("timestamp"))),
            _ => message.timestamp(),
        };
        if let Message::Assistant(assistant) = message {
            let usage_applies_to_prefix = assistant.timestamp >= latest_prefix_timestamp;
            if usage_applies_to_prefix
                && !matches!(
                    assistant.stop_reason,
                    StopReason::Aborted | StopReason::Error
                )
                && calculate_context_tokens(&assistant.usage) > 0.0
            {
                usage_info = Some((calculate_context_tokens(&assistant.usage), index));
            }
        }
        if let Message::Raw(raw) = message
            && raw.role() == "assistant"
            && timestamp >= latest_prefix_timestamp
        {
            let info = raw.read(|object| -> Result<Option<f64>, JsString> {
                if matches!(
                    object.get("stopReason").and_then(JsValue::as_str),
                    Some("aborted" | "error")
                ) {
                    return Ok(None);
                }
                let usage = object.get("usage");
                let total = raw_message::property(usage, "totalTokens")?
                    .and_then(JsValue::as_f64)
                    .unwrap_or(0.0);
                let tokens = if total != 0.0 && !total.is_nan() {
                    total
                } else {
                    ["input", "output", "cacheRead", "cacheWrite"]
                        .into_iter()
                        .map(|name| {
                            usage
                                .and_then(|value| value.get(name))
                                .and_then(JsValue::as_f64)
                                .unwrap_or(f64::NAN)
                        })
                        .sum()
                };
                Ok((tokens > 0.0).then_some(tokens))
            })?;
            if let Some(usage) = info {
                usage_info = Some((usage, index));
            }
        }
        // JavaScript Math.max propagates NaN rather than Rust f64::max's
        // preference for the non-NaN operand.
        latest_prefix_timestamp = if latest_prefix_timestamp.is_nan() || timestamp.is_nan() {
            f64::NAN
        } else {
            latest_prefix_timestamp.max(timestamp)
        };
    }
    Ok(usage_info)
}

impl AsRef<[Message]> for TranscriptContext {
    fn as_ref(&self) -> &[Message] {
        &self.messages
    }
}

pub fn estimate_context_tokens(
    context: impl AsRef<[Message]>,
) -> Result<ContextUsageEstimate, JsString> {
    let messages = context.as_ref();
    if let Some((usage, index)) = get_last_assistant_usage_info(messages)? {
        let usage_tokens = usage;
        let trailing_tokens = messages[index + 1..]
            .iter()
            .map(estimate_message_tokens)
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .sum::<f64>();
        Ok(ContextUsageEstimate {
            tokens: usage_tokens + trailing_tokens,
            usage_tokens,
            trailing_tokens,
            last_usage_index: Some(index),
        })
    } else {
        let tokens = messages
            .iter()
            .map(estimate_message_tokens)
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .sum();
        Ok(ContextUsageEstimate {
            tokens,
            usage_tokens: 0.0,
            trailing_tokens: tokens,
            last_usage_index: None,
        })
    }
}

fn estimate_tools_tokens<T: serde::Serialize>(tools: Option<&[T]>) -> f64 {
    match tools {
        Some(tools) if !tools.is_empty() => {
            estimate_text_tokens(&safe_json_stringify(&tools).into())
        }
        _ => 0.0,
    }
}
