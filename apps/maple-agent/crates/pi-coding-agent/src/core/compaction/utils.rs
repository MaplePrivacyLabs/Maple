//! Pi's shared compaction serialization and file-operation tracking.
use indexmap::IndexSet;
use pi_agent_core::types::{AgentError, AgentMessage, AgentResult};
use pi_ai::types::{JsString, JsValue, Message};
use pi_ai::utils::{
    js_json::{js_object_entries, stringify},
    js_value::to_js_value,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FileOperations {
    pub read: IndexSet<JsString>,
    pub written: IndexSet<JsString>,
    pub edited: IndexSet<JsString>,
}

pub fn create_file_ops() -> FileOperations {
    FileOperations::default()
}

fn add_file_op(name: Option<&JsValue>, args: Option<&JsValue>, file_ops: &mut FileOperations) {
    let Some(path) = args
        .and_then(|a| a.get("path"))
        .and_then(JsValue::as_js_str)
        .filter(|p| !p.is_empty())
    else {
        return;
    };
    match name.and_then(JsValue::as_str) {
        Some("read") => {
            file_ops.read.insert(path.clone());
        }
        Some("write") => {
            file_ops.written.insert(path.clone());
        }
        Some("edit") => {
            file_ops.edited.insert(path.clone());
        }
        _ => {}
    }
}

pub fn extract_file_ops_from_message(message: &AgentMessage, file_ops: &mut FileOperations) {
    let raw = to_js_value(message).expect("agent messages serialize to JavaScript values");
    match raw.get("role").and_then(JsValue::as_str) {
        Some("toolResult") => {
            if let Some(calls) = raw
                .get("nestedCalls")
                .and_then(|n| n.get("calls"))
                .and_then(JsValue::as_array)
            {
                for call in calls {
                    add_file_op(call.get("name"), call.get("arguments"), file_ops);
                }
            }
        }
        Some("assistant") => {
            if let Some(content) = raw.get("content").and_then(JsValue::as_array) {
                for block in content {
                    if block.get("type").and_then(JsValue::as_str) == Some("toolCall") {
                        add_file_op(block.get("name"), block.get("arguments"), file_ops);
                    }
                }
            }
        }
        _ => {}
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileLists {
    pub read_files: Vec<JsString>,
    pub modified_files: Vec<JsString>,
}

pub fn compute_file_lists(file_ops: &FileOperations) -> FileLists {
    let modified: IndexSet<_> = file_ops
        .edited
        .iter()
        .chain(&file_ops.written)
        .cloned()
        .collect();
    let mut read_files: Vec<_> = file_ops
        .read
        .iter()
        .filter(|f| !modified.contains(*f))
        .cloned()
        .collect();
    let mut modified_files: Vec<_> = modified.into_iter().collect();
    // JsString ordering is the UTF-16 ordering used by Array.prototype.sort.
    read_files.sort();
    modified_files.sort();
    FileLists {
        read_files,
        modified_files,
    }
}

pub fn format_file_operations(read_files: &[JsString], modified_files: &[JsString]) -> JsString {
    let mut sections = Vec::new();
    for (tag, files) in [
        ("read-files", read_files),
        ("modified-files", modified_files),
    ] {
        if !files.is_empty() {
            let mut section = JsString::from(format!("<{tag}>\n"));
            section.push(&JsString::join(files, "\n"));
            section.push_str(&format!("\n</{tag}>"));
            sections.push(section);
        }
    }
    if sections.is_empty() {
        return JsString::default();
    }
    let mut output = JsString::from("\n\n");
    output.push(&JsString::join(&sections, "\n\n"));
    output
}

const TOOL_RESULT_MAX_CHARS: usize = 2000;
fn truncate_for_summary(text: &JsString, max_chars: usize) -> JsString {
    let len = text.utf16_len();
    if len <= max_chars {
        return text.clone();
    }
    let mut result = text.slice(0, max_chars);
    result.push_str(&format!(
        "\n\n[... {} more characters truncated]",
        len - max_chars
    ));
    result
}

fn serialization_type_error(message: impl Into<JsString>) -> AgentError {
    AgentError::type_error(message)
}

// JavaScript Array#join coerces non-nullish values and leaves nullish slots empty.
fn join_value(value: Option<&JsValue>) -> JsString {
    match value {
        None | Some(JsValue::Null) => JsString::default(),
        Some(JsValue::String(text)) => text.clone(),
        Some(JsValue::Array(values)) => {
            let strings: Vec<_> = values.iter().map(|v| join_value(Some(v))).collect();
            JsString::join(&strings, ",")
        }
        Some(JsValue::Object(_)) => "[object Object]".into(),
        Some(JsValue::Number(number)) if number.is_nan() => "NaN".into(),
        Some(JsValue::Number(number)) if *number == f64::INFINITY => "Infinity".into(),
        Some(JsValue::Number(number)) if *number == f64::NEG_INFINITY => "-Infinity".into(),
        Some(value) => stringify(value).into(),
    }
}
fn nullish_property_error(value: Option<&JsValue>, property: &str) -> AgentError {
    serialization_type_error(format!(
        "Cannot read properties of {} (reading '{property}')",
        if value.is_none() { "undefined" } else { "null" }
    ))
}
fn block_type(block: &JsValue) -> AgentResult<Option<&str>> {
    if block.is_null() {
        return Err(nullish_property_error(Some(block), "type"));
    }
    Ok(block.get("type").and_then(JsValue::as_str))
}
fn raw_content_text(content: Option<&JsValue>, separator: &str) -> AgentResult<JsString> {
    if let Some(JsValue::String(text)) = content {
        return Ok(text.clone());
    }
    let blocks = match content {
        None | Some(JsValue::Null) => return Err(nullish_property_error(content, "filter")),
        Some(JsValue::Array(blocks)) => blocks,
        _ => return Err(serialization_type_error("content.filter is not a function")),
    };
    let mut text = Vec::new();
    for block in blocks {
        if block_type(block)? == Some("text") {
            text.push(join_value(block.get("text")));
        }
    }
    Ok(JsString::join(&text, separator))
}
fn argument_entries(arguments: Option<&JsValue>) -> AgentResult<Vec<(JsString, JsValue)>> {
    Ok(match arguments {
        None | Some(JsValue::Null) => {
            return Err(serialization_type_error(
                "Cannot convert undefined or null to object",
            ));
        }
        Some(JsValue::Object(object)) => js_object_entries(object)
            .into_iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        Some(JsValue::Array(values)) => values
            .iter()
            .enumerate()
            .map(|(i, v)| (i.to_string().into(), v.clone()))
            .collect(),
        Some(JsValue::String(value)) => (0..value.utf16_len())
            .map(|i| (i.to_string().into(), value.slice(i, i + 1).into()))
            .collect(),
        Some(_) => Vec::new(),
    })
}

pub fn serialize_conversation(messages: &[Message]) -> AgentResult<JsString> {
    let mut parts = Vec::new();
    for message in messages {
        // Rendering reads a snapshot only: historical raw messages and their unknown
        // fields stay shared and unmodified at the session boundary.
        let raw = to_js_value(message).map_err(|error| AgentError::new(error.to_string()))?;
        match raw.get("role").and_then(JsValue::as_str) {
            Some("user" | "toolResult") => {
                let content = raw_content_text(raw.get("content"), "")?;
                if !content.is_empty() {
                    let is_tool = raw.get("role").and_then(JsValue::as_str) == Some("toolResult");
                    let mut part = JsString::from(if is_tool {
                        "[Tool result]: "
                    } else {
                        "[User]: "
                    });
                    part.push(&if is_tool {
                        truncate_for_summary(&content, TOOL_RESULT_MAX_CHARS)
                    } else {
                        content
                    });
                    parts.push(part);
                }
            }
            Some("assistant") => {
                let mut thinking = Vec::new();
                let mut calls = Vec::new();
                let mut has_text = false;
                if matches!(raw.get("content"), Some(JsValue::String(_))) {
                    return Err(serialization_type_error(
                        "msg.content.some is not a function",
                    ));
                }
                let blocks = raw
                    .get("content")
                    .and_then(JsValue::as_array)
                    .ok_or_else(|| serialization_type_error("msg.content is not iterable"))?;
                for block in blocks {
                    match block_type(block)? {
                        Some("thinking") => thinking.push(join_value(block.get("thinking"))),
                        Some("text") => has_text = true,
                        Some("toolCall") => {
                            let mut args = Vec::new();
                            for (key, value) in argument_entries(block.get("arguments"))? {
                                let mut arg = key;
                                arg.push_str("=");
                                arg.push_str(&stringify(&value));
                                args.push(arg);
                            }
                            let mut call = match block.get("name") {
                                None => JsString::from("undefined"),
                                Some(JsValue::Null) => JsString::from("null"),
                                value => join_value(value),
                            };
                            call.push_str("(");
                            call.push(&JsString::join(&args, ", "));
                            call.push_str(")");
                            calls.push(call);
                        }
                        _ => {}
                    }
                }
                if !thinking.is_empty() {
                    let mut part = JsString::from("[Assistant thinking]: ");
                    part.push(&JsString::join(&thinking, "\n"));
                    parts.push(part);
                }
                if has_text {
                    let mut part = JsString::from("[Assistant]: ");
                    part.push(&raw_content_text(raw.get("content"), "\n")?);
                    parts.push(part);
                }
                if !calls.is_empty() {
                    let mut part = JsString::from("[Assistant tool calls]: ");
                    part.push(&JsString::join(&calls, "; "));
                    parts.push(part);
                }
            }
            _ => {}
        }
    }
    Ok(JsString::join(&parts, "\n\n"))
}

pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";
