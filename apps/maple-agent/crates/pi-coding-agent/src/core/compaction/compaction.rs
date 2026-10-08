//! Context thresholds, cut points, preparation, and one-off summaries from Pi.
//! The host supplies the stream function; the upstream completeSimple fallback is cut.
use super::utils::{
    FileOperations, SUMMARIZATION_SYSTEM_PROMPT, compute_file_lists, create_file_ops,
    extract_file_ops_from_message, format_file_operations, serialize_conversation,
};
use crate::core::{
    messages::convert_to_llm,
    session_manager::{
        ProjectedSessionEntry, SessionEntry, SessionProjection, build_session_projection,
        session_entry_to_context_messages,
    },
    usage_totals::combine_usage,
};
use pi_agent_core::types::{AgentError, AgentMessage, AgentResult, StreamFn, ThinkingLevel};
use pi_ai::{
    env::{CancellationToken, PiEnv},
    types::{
        AssistantContent, AssistantMessage, CacheRetention, Context, JsString, JsValue, Message,
        Model, ProviderEnv, ProviderHeaders, ProviderRequestOptions, SimpleStreamOptions,
        StopReason, StreamOptions, TextContent, TranscriptContext, Usage, UserContent, UserMessage,
        UserMessageContent,
    },
    utils::{
        js_json::stringify,
        js_value::{from_js_value, to_js_value},
        retry::{RetryCallbacks, RetryPolicy, retry_assistant_call},
        text::content_text,
        transcript::{get_current_system_message, normalize_context},
        uuid::UuidV7Generator,
    },
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionDetails {
    pub read_files: Vec<JsString>,
    pub modified_files: Vec<JsString>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompactionResult<T = JsValue> {
    pub summary: JsString,
    pub first_kept_entry_id: JsString,
    pub tokens_before: f64,
    pub raw_tokens_before: Option<JsValue>,
    pub estimated_tokens_after: Option<f64>,
    pub usage: Option<Usage>,
    pub details: Option<T>,
}
fn deserialize_present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSettings {
    pub enabled: bool,
    pub reserve_tokens: f64,
    pub keep_recent_tokens: f64,
}
pub const DEFAULT_COMPACTION_SETTINGS: CompactionSettings = CompactionSettings {
    enabled: true,
    reserve_tokens: 16384.0,
    keep_recent_tokens: 20000.0,
};
impl Default for CompactionSettings {
    fn default() -> Self {
        DEFAULT_COMPACTION_SETTINGS
    }
}

pub fn calculate_context_tokens(usage: &Usage) -> f64 {
    if usage.total_tokens != 0.0 && !usage.total_tokens.is_nan() {
        usage.total_tokens
    } else {
        usage.input + usage.output + usage.cache_read + usage.cache_write
    }
}

fn message_raw(message: &AgentMessage) -> JsValue {
    to_js_value(message).expect("agent messages serialize to JavaScript values")
}
/// Lossless source usage record: historical/raw messages need not contain every
/// field required by a current provider response. The typed view is opt-in.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RetainedUsage(pub JsValue);
impl RetainedUsage {
    pub fn typed(&self) -> Result<Usage, pi_ai::utils::js_value::JsonConversionError> {
        from_js_value(self.0.clone())
    }
}
fn retain_non_numeric(value: JsValue) -> Option<JsValue> {
    (!matches!(value, JsValue::Number(_))).then_some(value)
}

fn raw_context_tokens(usage: &JsValue) -> JsValue {
    if let Some(total) = usage.get("totalTokens").filter(|value| js_truthy(value)) {
        return total.clone();
    }
    let mut total = js_add(usage.get("input"), usage.get("output"));
    total = js_add(Some(&total), usage.get("cacheRead"));
    js_add(Some(&total), usage.get("cacheWrite"))
}
/// Preserve JavaScript's dynamic return value at raw storage/replay boundaries.
pub fn calculate_context_tokens_raw(usage: &JsValue) -> AgentResult<JsValue> {
    if usage.is_null() {
        return Err(AgentError::type_error(
            "Cannot read properties of null (reading 'totalTokens')",
        ));
    }
    Ok(raw_context_tokens(usage))
}
fn get_assistant_usage(message: &AgentMessage) -> Option<RetainedUsage> {
    let raw = message_raw(message);
    if raw.get("role").and_then(JsValue::as_str) != Some("assistant")
        || matches!(
            raw.get("stopReason").and_then(JsValue::as_str),
            Some("aborted" | "error")
        )
    {
        return None;
    }
    let usage = raw.get("usage").filter(|usage| js_truthy(usage))?;
    (js_number(Some(&raw_context_tokens(usage))) > 0.0).then(|| RetainedUsage(usage.clone()))
}
pub fn get_last_assistant_usage(entries: &[SessionEntry]) -> Option<RetainedUsage> {
    entries.iter().rev().find_map(|entry| {
        if entry.kind() == "message" {
            entry.message().and_then(|m| get_assistant_usage(&m))
        } else {
            None
        }
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct ContextUsageEstimate {
    pub tokens: f64,
    pub usage_tokens: f64,
    pub trailing_tokens: f64,
    pub last_usage_index: Option<usize>,
    pub raw_tokens: Option<JsValue>,
    pub raw_usage_tokens: Option<JsValue>,
}

pub fn estimate_context_tokens(messages: &[AgentMessage]) -> AgentResult<ContextUsageEstimate> {
    if let Some((index, usage)) = messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, m)| get_assistant_usage(m).map(|u| (i, u)))
    {
        let raw_usage_tokens = raw_context_tokens(&usage.0);
        let usage_tokens = js_number(Some(&raw_usage_tokens));
        let trailing_tokens = estimate_messages(&messages[index + 1..])?;
        let raw_tokens = js_add(
            Some(&raw_usage_tokens),
            Some(&JsValue::Number(trailing_tokens)),
        );
        Ok(ContextUsageEstimate {
            tokens: js_number(Some(&raw_tokens)),
            raw_tokens: retain_non_numeric(raw_tokens),
            raw_usage_tokens: retain_non_numeric(raw_usage_tokens),
            usage_tokens,
            trailing_tokens,
            last_usage_index: Some(index),
        })
    } else {
        let tokens = estimate_messages(messages)?;
        Ok(ContextUsageEstimate {
            tokens,
            usage_tokens: 0.0,
            trailing_tokens: tokens,
            last_usage_index: None,
            raw_tokens: None,
            raw_usage_tokens: None,
        })
    }
}

pub fn estimate_projected_context_tokens(
    projection: &SessionProjection,
    branch_entries: &[SessionEntry],
) -> AgentResult<ContextUsageEstimate> {
    let estimate = estimate_context_tokens(&projection.messages)?;
    if let Some(index) = estimate.last_usage_index {
        let mut projected_index = 0;
        let mut usage_entry_id = None;
        for entry in &projection.entries {
            let next_index = projected_index + entry.messages.len();
            if index < next_index {
                usage_entry_id = Some(entry.source_entry.id());
                break;
            }
            projected_index = next_index;
        }
        let usage_index = usage_entry_id
            .filter(|id| !id.is_empty())
            .and_then(|id| branch_entries.iter().position(|e| e.id() == id))
            .map_or(-1, |i| i as isize);
        let invalidating_index = branch_entries
            .iter()
            .rposition(|e| matches!(e.kind().as_str(), Some("context_edit" | "compaction")))
            .map_or(-1, |i| i as isize);
        if usage_index > invalidating_index {
            return Ok(estimate);
        }
    }
    let systems: Vec<_> = projection
        .messages
        .iter()
        .filter(|m| m.role() == "system")
        .cloned()
        .collect();
    let llm = convert_to_llm(&systems);
    let mut tokens = match get_current_system_message(&llm).map_err(AgentError::type_error)? {
        Some(message) => estimate_tokens(&message.into())?,
        None => 0.0,
    };
    for message in &projection.messages {
        if message.role() != "system" {
            tokens += estimate_tokens(message)?;
        }
    }
    Ok(ContextUsageEstimate {
        tokens,
        usage_tokens: 0.0,
        trailing_tokens: tokens,
        last_usage_index: None,
        raw_tokens: None,
        raw_usage_tokens: None,
    })
}

pub fn should_compact(
    context_tokens: f64,
    context_window: f64,
    settings: &CompactionSettings,
) -> bool {
    settings.enabled && context_tokens > context_window - settings.reserve_tokens
}

fn js_string(value: Option<&JsValue>) -> JsString {
    match value {
        None => "undefined".into(),
        Some(JsValue::Null) => "null".into(),
        Some(JsValue::String(text)) => text.clone(),
        Some(JsValue::Object(_)) => "[object Object]".into(),
        Some(JsValue::Array(values)) => {
            let parts: Vec<_> = values
                .iter()
                .map(|v| {
                    if v.is_null() {
                        JsString::default()
                    } else {
                        js_string(Some(v))
                    }
                })
                .collect();
            JsString::join(&parts, ",")
        }
        Some(JsValue::Number(number)) => pi_ai::utils::js_json::number_to_string(*number).into(),
        Some(JsValue::Bool(value)) => value.to_string().into(),
    }
}
fn js_add(left: Option<&JsValue>, right: Option<&JsValue>) -> JsValue {
    let primitive = |value: Option<&JsValue>| match value {
        Some(JsValue::Object(_) | JsValue::Array(_)) => Some(JsValue::String(js_string(value))),
        value => value.cloned(),
    };
    let left = primitive(left);
    let right = primitive(right);
    if matches!(left, Some(JsValue::String(_))) || matches!(right, Some(JsValue::String(_))) {
        let mut text = js_string(left.as_ref());
        text.push(&js_string(right.as_ref()));
        text.into()
    } else {
        (js_number(left.as_ref()) + js_number(right.as_ref())).into()
    }
}
fn js_number(value: Option<&JsValue>) -> f64 {
    match value {
        None => f64::NAN,
        Some(JsValue::Null) => 0.0,
        Some(JsValue::Bool(value)) => {
            if *value {
                1.0
            } else {
                0.0
            }
        }
        Some(JsValue::Number(value)) => *value,
        value => {
            let text = js_string(value);
            let Some(text) = text.as_str() else {
                return f64::NAN;
            };
            let text = text.trim_matches(|c: char| matches!(c as u32, 0x0009..=0x000d | 0x0020 | 0x00a0 | 0x1680 | 0x2000..=0x200a | 0x2028 | 0x2029 | 0x202f | 0x205f | 0x3000 | 0xfeff));
            if text.is_empty() {
                return 0.0;
            }
            for (prefix, base) in [
                ("0x", 16),
                ("0X", 16),
                ("0o", 8),
                ("0O", 8),
                ("0b", 2),
                ("0B", 2),
            ] {
                if let Some(digits) = text.strip_prefix(prefix) {
                    if digits.is_empty() {
                        return f64::NAN;
                    }
                    return digits
                        .chars()
                        .try_fold(0.0, |number, c| {
                            c.to_digit(base)
                                .map(|digit| number * base as f64 + digit as f64)
                        })
                        .unwrap_or(f64::NAN);
                }
            }
            match text {
                "Infinity" | "+Infinity" => f64::INFINITY,
                "-Infinity" => f64::NEG_INFINITY,
                _ if text.contains(['i', 'I', 'n', 'N']) => f64::NAN,
                _ => text.parse().unwrap_or(f64::NAN),
            }
        }
    }
}
fn length_of(value: Option<&JsValue>) -> AgentResult<JsValue> {
    match value {
        None | Some(JsValue::Null) => Err(AgentError::type_error(format!(
            "Cannot read properties of {} (reading 'length')",
            if value.is_none() { "undefined" } else { "null" }
        ))),
        Some(JsValue::String(text)) => Ok((text.utf16_len() as f64).into()),
        Some(JsValue::Array(array)) => Ok((array.len() as f64).into()),
        Some(JsValue::Object(object)) => Ok(object
            .get("length")
            .cloned()
            .unwrap_or(JsValue::Number(f64::NAN))),
        Some(_) => Ok(JsValue::Number(f64::NAN)),
    }
}

fn token_block_type(block: &JsValue) -> AgentResult<Option<&str>> {
    if block.is_null() {
        return Err(AgentError::type_error(
            "Cannot read properties of null (reading 'type')",
        ));
    }
    Ok(block.get("type").and_then(JsValue::as_str))
}
fn text_image_chars(content: Option<&JsValue>) -> AgentResult<JsValue> {
    if let Some(JsValue::String(text)) = content {
        return Ok((text.utf16_len() as f64).into());
    }
    let blocks = content
        .and_then(JsValue::as_array)
        .ok_or_else(|| AgentError::type_error("content is not iterable"))?;
    let mut chars = JsValue::Number(0.0);
    for block in blocks {
        match token_block_type(block)? {
            Some("text") if block.get("text").is_some_and(js_truthy) => {
                chars = js_add(Some(&chars), Some(&length_of(block.get("text"))?))
            }
            Some("image") => chars = js_add(Some(&chars), Some(&JsValue::Number(4800.0))),
            _ => {}
        }
    }
    Ok(chars)
}
fn json_len(value: &JsValue) -> f64 {
    stringify(value).encode_utf16().count() as f64
}
fn enumerable_values(value: &JsValue) -> Vec<JsValue> {
    match value {
        JsValue::Object(object) => pi_ai::utils::js_json::js_object_entries(object)
            .into_iter()
            .map(|(_, v)| v.clone())
            .collect(),
        JsValue::Array(values) => values.clone(),
        JsValue::String(text) => (0..text.utf16_len())
            .map(|i| text.slice(i, i + 1).into())
            .collect(),
        _ => Vec::new(),
    }
}
fn estimate_messages(messages: &[AgentMessage]) -> AgentResult<f64> {
    messages
        .iter()
        .try_fold(0.0, |total, message| Ok(total + estimate_tokens(message)?))
}

pub fn estimate_tokens(message: &AgentMessage) -> AgentResult<f64> {
    let raw = message_raw(message);
    let chars = match raw.get("role").and_then(JsValue::as_str) {
        Some("system") => {
            let mut chars = text_image_chars(raw.get("content"))?;
            if let Some(sections) = raw.get("sections").filter(|value| js_truthy(value)) {
                for value in enumerable_values(sections) {
                    if js_truthy(&value) {
                        chars = js_add(Some(&chars), Some(&length_of(Some(&value))?));
                    }
                }
            }
            if let Some(tools) = raw.get("toolsAdded").filter(|value| js_truthy(value)) {
                chars = js_add(Some(&chars), Some(&JsValue::Number(json_len(tools))));
            }
            chars
        }
        Some("user" | "custom" | "toolResult") => text_image_chars(raw.get("content"))?,
        Some("assistant") => {
            // Strings are iterable in JavaScript. Their character values have no
            // block type, so this branch contributes no characters.
            if matches!(raw.get("content"), Some(JsValue::String(_))) {
                return Ok(0.0);
            }
            let blocks = raw
                .get("content")
                .and_then(JsValue::as_array)
                .ok_or_else(|| AgentError::type_error("assistant.content is not iterable"))?;
            let mut chars = JsValue::Number(0.0);
            for block in blocks {
                match token_block_type(block)? {
                    Some("text") => {
                        chars = js_add(Some(&chars), Some(&length_of(block.get("text"))?))
                    }
                    Some("thinking") => {
                        chars = js_add(Some(&chars), Some(&length_of(block.get("thinking"))?))
                    }
                    Some("toolCall") => {
                        let call_chars = js_add(
                            Some(&length_of(block.get("name"))?),
                            Some(&JsValue::Number(
                                block.get("arguments").map(json_len).ok_or_else(|| {
                                    AgentError::type_error(
                                        "Cannot read properties of undefined (reading 'length')",
                                    )
                                })?,
                            )),
                        );
                        chars = js_add(Some(&chars), Some(&call_chars));
                    }
                    _ => {}
                }
            }
            chars
        }
        Some("bashExecution") => js_add(
            Some(&length_of(raw.get("command"))?),
            Some(&length_of(raw.get("output"))?),
        ),
        Some("branchSummary" | "compactionSummary") => length_of(raw.get("summary"))?,
        _ => JsValue::Number(0.0),
    };
    Ok((js_number(Some(&chars)) / 4.0).ceil())
}

fn is_cut_point_message(message: &AgentMessage) -> bool {
    matches!(
        message.role().as_str(),
        Some(
            "user"
                | "assistant"
                | "bashExecution"
                | "custom"
                | "branchSummary"
                | "compactionSummary"
        )
    )
}
fn is_turn_start_message(message: &AgentMessage) -> bool {
    matches!(
        message.role().as_str(),
        Some("user" | "bashExecution" | "custom" | "branchSummary" | "compactionSummary")
    )
}
fn is_turn_start_entry(entry: &SessionEntry) -> bool {
    entry.kind() != "compaction"
        && session_entry_to_context_messages(entry)
            .iter()
            .any(is_turn_start_message)
}

pub fn find_turn_start_index(
    entries: &[SessionEntry],
    entry_index: usize,
    start_index: usize,
) -> AgentResult<isize> {
    if start_index <= entry_index && entry_index >= entries.len() {
        return Err(AgentError::type_error(
            "Cannot read properties of undefined (reading 'type')",
        ));
    }
    Ok((start_index..=entry_index)
        .rev()
        .find(|i| is_turn_start_entry(&entries[*i]))
        .map_or(-1, |i| i as isize))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CutPointResult {
    pub first_kept_entry_index: usize,
    pub turn_start_index: isize,
    pub is_split_turn: bool,
}

pub fn find_cut_point(
    entries: &[SessionEntry],
    start_index: usize,
    end_index: usize,
    keep_recent_tokens: f64,
) -> AgentResult<CutPointResult> {
    if start_index < end_index && end_index > entries.len() {
        return Err(AgentError::type_error(
            "Cannot read properties of undefined (reading 'type')",
        ));
    }
    let cuts: Vec<_> = (start_index..end_index)
        .filter(|i| {
            entries[*i].kind() != "compaction"
                && session_entry_to_context_messages(&entries[*i])
                    .iter()
                    .any(is_cut_point_message)
        })
        .collect();
    let Some(mut cut_index) = cuts.first().copied() else {
        return Ok(CutPointResult {
            first_kept_entry_index: start_index,
            turn_start_index: -1,
            is_split_turn: false,
        });
    };
    let mut accumulated = 0.0;
    for index in (start_index..end_index).rev() {
        let tokens = estimate_messages(&session_entry_to_context_messages(&entries[index]))?;
        if tokens == 0.0 {
            continue;
        }
        accumulated += tokens;
        if accumulated >= keep_recent_tokens {
            cut_index = cuts
                .iter()
                .find(|candidate| **candidate >= index)
                .copied()
                .unwrap_or(*cuts.last().unwrap());
            break;
        }
    }
    while cut_index > start_index {
        let previous = &entries[cut_index - 1];
        if previous.kind() == "compaction"
            || !session_entry_to_context_messages(previous).is_empty()
        {
            break;
        }
        cut_index -= 1;
    }
    let starts_turn = is_turn_start_entry(&entries[cut_index]);
    let turn_start_index = if starts_turn {
        -1
    } else {
        find_turn_start_index(entries, cut_index, start_index)?
    };
    Ok(CutPointResult {
        first_kept_entry_index: cut_index,
        turn_start_index,
        is_split_turn: !starts_turn && turn_start_index != -1,
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompactionPreparation {
    pub first_kept_entry_id: JsString,
    pub messages_to_summarize: Vec<AgentMessage>,
    pub turn_prefix_messages: Vec<AgentMessage>,
    pub is_split_turn: bool,
    pub tokens_before: f64,
    pub raw_tokens_before: Option<JsValue>,
    pub previous_summary: Option<JsString>,
    pub file_ops: FileOperations,
    pub settings: CompactionSettings,
}

fn projected_turn_start(entry: &ProjectedSessionEntry) -> bool {
    entry.source_entry.kind() != "compaction" && entry.messages.iter().any(is_turn_start_message)
}
fn intrinsically_visible(entry: &ProjectedSessionEntry) -> bool {
    entry.source_entry.kind() != "context_edit"
        && !session_entry_to_context_messages(&entry.source_entry).is_empty()
}
fn omitted(entry: &ProjectedSessionEntry) -> bool {
    intrinsically_visible(entry) && entry.messages.is_empty()
}

fn find_projected_cut_point(
    entries: &[ProjectedSessionEntry],
    start: usize,
    end: usize,
    keep: f64,
) -> AgentResult<CutPointResult> {
    let cuts: Vec<_> = (start..end)
        .filter(|i| {
            entries[*i].source_entry.kind() != "compaction"
                && entries[*i].messages.iter().any(is_cut_point_message)
        })
        .collect();
    let Some(mut cut) = cuts.first().copied() else {
        return Ok(CutPointResult {
            first_kept_entry_index: start,
            turn_start_index: -1,
            is_split_turn: false,
        });
    };
    let mut accumulated = 0.0;
    let mut exceeded = false;
    for index in (start..end).rev() {
        let tokens = estimate_messages(&entries[index].messages)?;
        if tokens == 0.0 {
            continue;
        }
        accumulated += tokens;
        if accumulated >= keep {
            exceeded = true;
            cut = cuts
                .iter()
                .find(|c| **c >= index)
                .copied()
                .unwrap_or(*cuts.last().unwrap());
            break;
        }
    }
    let suffix = &entries[cut + 1..end];
    let omitted_ids: Vec<_> = suffix
        .iter()
        .filter(|e| omitted(e))
        .map(|e| e.source_entry.id())
        .collect();
    let external_replacement = suffix.iter().any(|e| {
        e.source_entry.kind() == "context_edit"
            && e.source_entry.get("replacement") != Some(JsValue::Null)
            && !e
                .source_entry
                .get("targetId")
                .and_then(|v| v.as_js_str().cloned())
                .is_some_and(|id| omitted_ids.contains(&id))
    });
    let recovery_suffix = exceeded
        && !external_replacement
        && suffix.iter().any(|e| {
            e.source_entry.kind() == "message"
                && e.source_entry
                    .message()
                    .is_some_and(|m| m.role() == "assistant")
                && omitted(e)
        })
        && suffix.iter().all(|e| {
            e.source_entry.kind() != "compaction" && (!intrinsically_visible(e) || omitted(e))
        });
    if recovery_suffix {
        cut += 1;
    }
    while cut > start {
        let previous = &entries[cut - 1];
        if previous.source_entry.kind() == "compaction" || !previous.messages.is_empty() {
            break;
        }
        cut -= 1;
    }
    let starts_turn = projected_turn_start(&entries[cut]);
    let turn_start_index = if starts_turn {
        -1
    } else {
        (start..=cut)
            .rev()
            .find(|i| projected_turn_start(&entries[*i]))
            .map_or(-1, |i| i as isize)
    };
    Ok(CutPointResult {
        first_kept_entry_index: cut,
        turn_start_index,
        is_split_turn: !starts_turn && turn_start_index != -1,
    })
}

pub(super) fn add_stored_file_operations(entry: &SessionEntry, file_ops: &mut FileOperations) {
    if entry.get("fromHook").is_some_and(|v| js_truthy(&v)) {
        return;
    }
    if let Some(details) = entry.get("details") {
        for (field, files) in [
            ("readFiles", &mut file_ops.read),
            ("modifiedFiles", &mut file_ops.edited),
        ] {
            if let Some(values) = details.get(field).and_then(JsValue::as_array) {
                for value in values {
                    if let Some(file) = value.as_js_str() {
                        files.insert(file.clone());
                    }
                }
            }
        }
    }
}
pub(super) fn js_truthy(value: &JsValue) -> bool {
    match value {
        JsValue::Null => false,
        JsValue::Bool(v) => *v,
        JsValue::Number(v) => *v != 0.0 && !v.is_nan(),
        JsValue::String(v) => !v.is_empty(),
        _ => true,
    }
}
fn compaction_messages(entry: &ProjectedSessionEntry) -> Vec<AgentMessage> {
    if entry.source_entry.kind() == "compaction" {
        Vec::new()
    } else {
        entry
            .messages
            .iter()
            .filter(|m| m.role() != "system")
            .cloned()
            .collect()
    }
}

pub fn prepare_compaction(
    path_entries: &[SessionEntry],
    settings: &CompactionSettings,
) -> AgentResult<Option<CompactionPreparation>> {
    if path_entries
        .last()
        .is_some_and(|e| e.kind() == "compaction")
    {
        return Ok(None);
    }
    let projection = build_session_projection(path_entries, None, None);
    let entries = &projection.entries;
    let previous = entries
        .iter()
        .position(|e| e.source_entry.kind() == "compaction" && !e.messages.is_empty());
    let previous_summary = previous
        .and_then(|i| entries[i].source_entry.get("summary"))
        .and_then(|v| v.as_js_str().cloned());
    let start = previous.map_or(0, |i| i + 1);
    let estimate = estimate_projected_context_tokens(&projection, path_entries)?;
    let tokens_before = estimate.tokens;
    let raw_tokens_before = estimate.raw_tokens;
    let cut = find_projected_cut_point(entries, start, entries.len(), settings.keep_recent_tokens)?;
    let Some(first_kept) = entries.get(cut.first_kept_entry_index) else {
        return Ok(None);
    };
    let first_kept_entry_id = first_kept.source_entry.id();
    if first_kept_entry_id.is_empty() {
        return Ok(None);
    }
    let history_end = if cut.is_split_turn {
        cut.turn_start_index as usize
    } else {
        cut.first_kept_entry_index
    };
    let messages_to_summarize: Vec<_> = entries[start..history_end]
        .iter()
        .flat_map(compaction_messages)
        .collect();
    let turn_prefix_messages: Vec<_> = if cut.is_split_turn {
        entries[cut.turn_start_index as usize..cut.first_kept_entry_index]
            .iter()
            .flat_map(compaction_messages)
            .collect()
    } else {
        Vec::new()
    };
    if messages_to_summarize.is_empty() && turn_prefix_messages.is_empty() {
        return Ok(None);
    }
    let mut file_ops = create_file_ops();
    if let Some(previous) = previous {
        add_stored_file_operations(&entries[previous].source_entry, &mut file_ops);
    }
    for message in messages_to_summarize.iter().chain(&turn_prefix_messages) {
        extract_file_ops_from_message(message, &mut file_ops);
    }
    Ok(Some(CompactionPreparation {
        first_kept_entry_id,
        messages_to_summarize,
        turn_prefix_messages,
        is_split_turn: cut.is_split_turn,
        tokens_before,
        raw_tokens_before,
        previous_summary,
        file_ops,
        settings: *settings,
    }))
}

/// Per-host services. Share `uuid` with other users of the loaded Pi UUID module.
#[derive(Clone)]
pub struct SummaryRuntime {
    pub stream_fn: StreamFn,
    pub env: Arc<dyn PiEnv>,
    pub uuid: Arc<UuidV7Generator>,
}
impl SummaryRuntime {
    pub fn new(stream_fn: StreamFn, env: Arc<dyn PiEnv>) -> Self {
        Self {
            stream_fn,
            env,
            uuid: Arc::new(UuidV7Generator::new()),
        }
    }
    pub fn with_uuid(stream_fn: StreamFn, env: Arc<dyn PiEnv>, uuid: Arc<UuidV7Generator>) -> Self {
        Self {
            stream_fn,
            env,
            uuid,
        }
    }
}

#[derive(Clone, Default)]
pub struct SummaryOptions {
    pub api_key: Option<String>,
    pub headers: Option<ProviderHeaders>,
    pub signal: Option<CancellationToken>,
    pub custom_instructions: Option<JsString>,
    pub previous_summary: Option<JsString>,
    pub thinking_level: Option<ThinkingLevel>,
    pub env: Option<ProviderEnv>,
    pub retry: Option<RetryPolicy>,
    pub session_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SummaryWithUsage {
    pub text: JsString,
    pub usage: Usage,
}

pub fn get_summarization_failure(
    response: &AssistantMessage,
    label: impl Into<JsString>,
) -> Option<JsString> {
    let mut text = label.into();
    match response.stop_reason {
        StopReason::Error => {
            text.push_str(" failed: ");
            text.push(
                response
                    .error_message
                    .as_ref()
                    .filter(|v| !v.is_empty())
                    .unwrap_or(&JsString::from("Unknown error")),
            );
            Some(text)
        }
        StopReason::Length => {
            text.push_str(" failed: generation hit the token cap and the summary is incomplete");
            Some(text)
        }
        _ => None,
    }
}

fn create_summarization_options(
    model: &Model,
    max_tokens: f64,
    options: &SummaryOptions,
) -> SimpleStreamOptions {
    let mut result = SimpleStreamOptions {
        stream: StreamOptions {
            request: ProviderRequestOptions {
                signal: options.signal.clone(),
                api_key: options.api_key.clone(),
                headers: options.headers.clone(),
                env: options.env.clone(),
                ..Default::default()
            },
            max_tokens: Some(max_tokens),
            session_id: options.session_id.clone(),
            ..Default::default()
        },
        ..Default::default()
    };
    if model.reasoning {
        result.reasoning = options.thinking_level.and_then(|level| match level {
            ThinkingLevel::Off => None,
            other => from_js_value(JsValue::from(other.as_str())).ok(),
        });
    }
    result
}

pub async fn complete_summarization(
    model: &Model,
    context: TranscriptContext,
    mut options: SimpleStreamOptions,
    runtime: &SummaryRuntime,
    retry: Option<&RetryPolicy>,
    callbacks: Option<&mut dyn RetryCallbacks<AgentError>>,
) -> AgentResult<AssistantMessage> {
    options.cache_retention = Some(CacheRetention::None);
    if options.session_id.is_none() {
        options.session_id = Some(
            runtime
                .uuid
                .uuidv7(runtime.env.as_ref(), None)
                .map_err(|e| AgentError::new(e.to_string()))?,
        );
    }
    let signal = options.signal.clone();
    let produce = || {
        let stream_fn = runtime.stream_fn.clone();
        let model = model.clone();
        let context = context.clone();
        let options = options.clone();
        async move {
            Ok(stream_fn(model, context, Some(options))
                .await?
                .result()
                .await)
        }
    };
    retry_assistant_call(
        produce,
        retry,
        signal.as_ref(),
        callbacks,
        runtime.env.as_ref(),
    )
    .await
}

fn build_summarization_context(prompt: JsString, runtime: &SummaryRuntime) -> TranscriptContext {
    normalize_context(Context {
        system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.into()),
        messages: vec![Message::User(UserMessage {
            content: UserMessageContent::Blocks(vec![UserContent::Text(TextContent::new(prompt))]),
            timestamp: runtime.env.now_ms() as f64,
            ..Default::default()
        })],
        tools: None,
    })
}
fn capped_tokens(reserve: f64, factor: f64, model: &Model) -> f64 {
    let requested = (factor * reserve).floor();
    if requested.is_nan() {
        f64::NAN
    } else {
        requested.min(if model.max_tokens > 0.0 {
            model.max_tokens
        } else {
            f64::INFINITY
        })
    }
}

pub async fn generate_summary(
    messages: &[AgentMessage],
    model: &Model,
    reserve_tokens: f64,
    options: &SummaryOptions,
    runtime: &SummaryRuntime,
    callbacks: Option<&mut dyn RetryCallbacks<AgentError>>,
) -> AgentResult<JsString> {
    Ok(
        generate_summary_with_usage(messages, model, reserve_tokens, options, runtime, callbacks)
            .await?
            .text,
    )
}

pub async fn generate_summary_with_usage(
    messages: &[AgentMessage],
    model: &Model,
    reserve_tokens: f64,
    options: &SummaryOptions,
    runtime: &SummaryRuntime,
    callbacks: Option<&mut dyn RetryCallbacks<AgentError>>,
) -> AgentResult<SummaryWithUsage> {
    let previous = options.previous_summary.as_ref().filter(|s| !s.is_empty());
    let mut base_prompt = JsString::from(if previous.is_some() {
        UPDATE_SUMMARIZATION_PROMPT
    } else {
        SUMMARIZATION_PROMPT
    });
    if let Some(instructions) = options
        .custom_instructions
        .as_ref()
        .filter(|s| !s.is_empty())
    {
        base_prompt.push_str("\n\nAdditional focus: ");
        base_prompt.push(instructions);
    }
    let conversation = serialize_conversation(&convert_to_llm(messages))?;
    let mut prompt = JsString::from("<conversation>\n");
    prompt.push(&conversation);
    prompt.push_str("\n</conversation>\n\n");
    if let Some(previous) = previous {
        prompt.push_str("<previous-summary>\n");
        prompt.push(previous);
        prompt.push_str("\n</previous-summary>\n\n");
    }
    prompt.push(&base_prompt);
    let response = complete_summarization(
        model,
        build_summarization_context(prompt, runtime),
        create_summarization_options(model, capped_tokens(reserve_tokens, 0.8, model), options),
        runtime,
        options.retry.as_ref(),
        callbacks,
    )
    .await?;
    if let Some(failure) = get_summarization_failure(&response, "Summarization") {
        return Err(AgentError::new(failure));
    }
    if response
        .content
        .iter()
        .any(|b| matches!(b, AssistantContent::ToolCall(_)))
    {
        return Err("Summarization attempted to call a tool".into());
    }
    Ok(SummaryWithUsage {
        text: content_text(&response.content, "\n"),
        usage: response.usage,
    })
}

async fn generate_turn_prefix_summary(
    messages: &[AgentMessage],
    model: &Model,
    reserve_tokens: f64,
    options: &SummaryOptions,
    runtime: &SummaryRuntime,
    callbacks: Option<&mut dyn RetryCallbacks<AgentError>>,
) -> AgentResult<SummaryWithUsage> {
    let conversation = serialize_conversation(&convert_to_llm(messages))?;
    let mut prompt = JsString::from("# Conversation\n");
    prompt.push(&conversation);
    prompt.push_str("\n\n# Instructions\n");
    prompt.push_str(TURN_PREFIX_SUMMARIZATION_PROMPT);
    let response = complete_summarization(
        model,
        build_summarization_context(prompt, runtime),
        create_summarization_options(model, capped_tokens(reserve_tokens, 0.5, model), options),
        runtime,
        options.retry.as_ref(),
        callbacks,
    )
    .await?;
    if let Some(failure) = get_summarization_failure(&response, "Turn prefix summarization") {
        return Err(AgentError::new(failure));
    }
    if response
        .content
        .iter()
        .any(|b| matches!(b, AssistantContent::ToolCall(_)))
    {
        return Err("Turn prefix summarization attempted to call a tool".into());
    }
    Ok(SummaryWithUsage {
        text: content_text(&response.content, "\n"),
        usage: response.usage,
    })
}

pub async fn compact(
    preparation: &CompactionPreparation,
    model: &Model,
    options: &SummaryOptions,
    runtime: &SummaryRuntime,
    mut callbacks: Option<&mut dyn RetryCallbacks<AgentError>>,
) -> AgentResult<CompactionResult> {
    let mut options = options.clone();
    options.previous_summary = preparation.previous_summary.clone();
    let (mut summary, usage) =
        if preparation.is_split_turn && !preparation.turn_prefix_messages.is_empty() {
            let mut history_text = preparation
                .previous_summary
                .clone()
                .unwrap_or_else(|| "No prior history.".into());
            let mut history_usage = None;
            if !preparation.messages_to_summarize.is_empty() {
                let result = generate_summary_with_usage(
                    &preparation.messages_to_summarize,
                    model,
                    preparation.settings.reserve_tokens,
                    &options,
                    runtime,
                    callbacks
                        .as_mut()
                        .map(|cb| &mut **cb as &mut dyn RetryCallbacks<AgentError>),
                )
                .await?;
                history_text = result.text;
                history_usage = Some(result.usage);
            }
            let result = generate_turn_prefix_summary(
                &preparation.turn_prefix_messages,
                model,
                preparation.settings.reserve_tokens,
                &options,
                runtime,
                callbacks,
            )
            .await?;
            history_text.push_str("\n\n---\n\n**Turn Context (split turn):**\n\n");
            history_text.push(&result.text);
            let usage = history_usage.map_or_else(
                || result.usage.clone(),
                |history| combine_usage(&history, &result.usage),
            );
            (history_text, usage)
        } else {
            let result = generate_summary_with_usage(
                &preparation.messages_to_summarize,
                model,
                preparation.settings.reserve_tokens,
                &options,
                runtime,
                callbacks,
            )
            .await?;
            (result.text, result.usage)
        };
    let files = compute_file_lists(&preparation.file_ops);
    summary.push(&format_file_operations(
        &files.read_files,
        &files.modified_files,
    ));
    if preparation.first_kept_entry_id.is_empty() {
        return Err("First kept entry has no UUID - session may need migration".into());
    }
    Ok(CompactionResult {
        summary,
        first_kept_entry_id: preparation.first_kept_entry_id.clone(),
        tokens_before: preparation.tokens_before,
        raw_tokens_before: preparation.raw_tokens_before.clone(),
        estimated_tokens_after: None,
        usage: Some(usage),
        details: Some(
            to_js_value(&CompactionDetails {
                read_files: files.read_files,
                modified_files: files.modified_files,
            })
            .expect("file lists serialize"),
        ),
    })
}

const SUMMARIZATION_PROMPT: &str = include_str!("prompts/summary.txt");
const UPDATE_SUMMARIZATION_PROMPT: &str = include_str!("prompts/update_summary.txt");
const TURN_PREFIX_SUMMARIZATION_PROMPT: &str = include_str!("prompts/turn_prefix.txt");

impl Serialize for ContextUsageEstimate {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry(
            "tokens",
            &self
                .raw_tokens
                .clone()
                .unwrap_or(JsValue::Number(self.tokens)),
        )?;
        map.serialize_entry(
            "usageTokens",
            &self
                .raw_usage_tokens
                .clone()
                .unwrap_or(JsValue::Number(self.usage_tokens)),
        )?;
        map.serialize_entry("trailingTokens", &self.trailing_tokens)?;
        map.serialize_entry("lastUsageIndex", &self.last_usage_index)?;
        map.end()
    }
}
impl<T: Serialize> Serialize for CompactionResult<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("summary", &self.summary)?;
        map.serialize_entry("firstKeptEntryId", &self.first_kept_entry_id)?;
        map.serialize_entry(
            "tokensBefore",
            &self
                .raw_tokens_before
                .clone()
                .unwrap_or(JsValue::Number(self.tokens_before)),
        )?;
        if let Some(value) = &self.estimated_tokens_after {
            map.serialize_entry("estimatedTokensAfter", value)?;
        }
        if let Some(value) = &self.usage {
            map.serialize_entry("usage", value)?;
        }
        if let Some(value) = &self.details {
            map.serialize_entry("details", value)?;
        }
        map.end()
    }
}
impl Serialize for CompactionPreparation {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("firstKeptEntryId", &self.first_kept_entry_id)?;
        map.serialize_entry("messagesToSummarize", &self.messages_to_summarize)?;
        map.serialize_entry("turnPrefixMessages", &self.turn_prefix_messages)?;
        map.serialize_entry("isSplitTurn", &self.is_split_turn)?;
        map.serialize_entry(
            "tokensBefore",
            &self
                .raw_tokens_before
                .clone()
                .unwrap_or(JsValue::Number(self.tokens_before)),
        )?;
        if let Some(value) = &self.previous_summary {
            map.serialize_entry("previousSummary", value)?;
        }
        map.serialize_entry("fileOps", &self.file_ops)?;
        map.serialize_entry("settings", &self.settings)?;
        map.end()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EstimateWire {
    tokens: JsValue,
    usage_tokens: JsValue,
    trailing_tokens: f64,
    last_usage_index: Option<usize>,
}
impl<'de> Deserialize<'de> for ContextUsageEstimate {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = EstimateWire::deserialize(deserializer)?;
        Ok(Self {
            tokens: js_number(Some(&value.tokens)),
            usage_tokens: js_number(Some(&value.usage_tokens)),
            trailing_tokens: value.trailing_tokens,
            last_usage_index: value.last_usage_index,
            raw_tokens: retain_non_numeric(value.tokens),
            raw_usage_tokens: retain_non_numeric(value.usage_tokens),
        })
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreparationWire {
    first_kept_entry_id: JsString,
    messages_to_summarize: Vec<AgentMessage>,
    turn_prefix_messages: Vec<AgentMessage>,
    is_split_turn: bool,
    tokens_before: JsValue,
    previous_summary: Option<JsString>,
    file_ops: FileOperations,
    settings: CompactionSettings,
}
impl<'de> Deserialize<'de> for CompactionPreparation {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = PreparationWire::deserialize(deserializer)?;
        Ok(Self {
            first_kept_entry_id: value.first_kept_entry_id,
            messages_to_summarize: value.messages_to_summarize,
            turn_prefix_messages: value.turn_prefix_messages,
            is_split_turn: value.is_split_turn,
            tokens_before: js_number(Some(&value.tokens_before)),
            raw_tokens_before: retain_non_numeric(value.tokens_before),
            previous_summary: value.previous_summary,
            file_ops: value.file_ops,
            settings: value.settings,
        })
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", bound(deserialize = "T: Deserialize<'de>"))]
struct ResultWire<T> {
    summary: JsString,
    first_kept_entry_id: JsString,
    tokens_before: JsValue,
    estimated_tokens_after: Option<f64>,
    usage: Option<Usage>,
    #[serde(default, deserialize_with = "deserialize_present")]
    details: Option<T>,
}
impl<'de, T: Deserialize<'de>> Deserialize<'de> for CompactionResult<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = ResultWire::deserialize(deserializer)?;
        Ok(Self {
            summary: value.summary,
            first_kept_entry_id: value.first_kept_entry_id,
            tokens_before: js_number(Some(&value.tokens_before)),
            raw_tokens_before: retain_non_numeric(value.tokens_before),
            estimated_tokens_after: value.estimated_tokens_after,
            usage: value.usage,
            details: value.details,
        })
    }
}
