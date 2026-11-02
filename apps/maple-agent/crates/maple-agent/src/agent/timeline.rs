//! Pi's sessions and events as the timeline Maple's surfaces render.
//!
//! A row's id is derived from the message it shows, the same way for a live
//! event and for the stored session, so a row streamed during a run is the
//! row a reload produces: `u{timestamp}` for a user message, `a{timestamp}`
//! plus `-thinking`, `-text` or `-error` for a reply, `a{timestamp}-{call id}`
//! for a tool call and its result. These are merge lookup keys; repeated
//! timestamps across process restarts may repeat them. Persisted rows have a
//! separate (task, epoch, row sequence) identity. Rows of
//! Maple's own session entries, such as the "Stopped by user" notice, carry
//! the id the runtime gave the entry.
//!
//! Live text and thinking arrive as `append` rows that the host concatenates;
//! everything else, and every stored row, is a `replace` row.

use std::collections::HashMap;

use pi_agent_core::AgentEvent;
use pi_ai::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, Content, Message, StopReason,
    Timestamp, ToolCall, content_text,
};
use pi_coding_agent::SessionMessage;
use pi_coding_agent::session::{EntryKind, SessionManager};
use serde::Serialize;
use serde_json::{Value, json};

use super::attachments::split_attachment_prompt;
use super::external_agents;
use super::types::AgentTimelineItem;

pub(crate) const MAX_AGENT_SESSION_TITLE_CHARS: usize = 80;
pub(crate) const MAX_AGENT_ERROR_CHARS: usize = 1_200;

/// The custom entry type of a notice Maple adds to a session, such as
/// "Stopped by user". Notices are shown, never sent to the model.
pub(crate) const MAPLE_NOTICE_ENTRY: &str = "maple.notice";
pub(crate) const STOPPED_NOTICE_TEXT: &str = "Stopped by user";

/// The data of a notice entry.
pub(crate) fn notice_entry_data(id: &str, text: &str) -> Value {
    json!({ "id": id, "text": text })
}

fn user_row_id(timestamp: Timestamp) -> String {
    format!("u{timestamp}")
}

fn reply_row_id(timestamp: Timestamp, part: &str) -> String {
    format!("a{timestamp}-{part}")
}

fn tool_row_id(reply_timestamp: Timestamp, call_id: &str) -> String {
    format!("a{reply_timestamp}-{call_id}")
}

/// The row of the tool call `call_id` in a stored session: that of the
/// latest reply that made it, which the session stores before the call runs.
pub(crate) fn stored_tool_row_id(session: &SessionManager, call_id: &str) -> Option<String> {
    session
        .branch()
        .iter()
        .rev()
        .find_map(|entry| match &entry.kind {
            EntryKind::Message {
                message: SessionMessage::Llm(Message::Assistant(reply)),
            } if reply.tool_calls().any(|call| call.id == call_id) => {
                Some(tool_row_id(reply.timestamp, call_id))
            }
            _ => None,
        })
}

fn created_ms(timestamp: Timestamp) -> u128 {
    u128::try_from(timestamp).unwrap_or_default()
}

/// The timeline of a stored session: its current branch, oldest first.
#[cfg(test)]
pub(crate) fn session_timeline(session: &SessionManager) -> Vec<AgentTimelineItem> {
    let mut items: Vec<AgentTimelineItem> = Vec::new();
    // Repeated legacy keys deliberately select their first visible occurrence.
    // Keep the first index when inserting a duplicate, matching merge_into.
    let mut first_by_key: HashMap<String, usize> = HashMap::new();
    // A tool result's row is its call's row, found by call id.
    let mut tool_rows: HashMap<String, String> = HashMap::new();
    let branch = session.branch();
    // Context edits that omit an entry hide it: Pi omits a failed attempt it
    // retries, and a reply it compacts away to answer again.
    let omitted: std::collections::HashSet<&str> = branch
        .iter()
        .filter_map(|entry| match &entry.kind {
            EntryKind::ContextEdit {
                target_id,
                replacement: None,
            } => Some(target_id.as_str()),
            _ => None,
        })
        .collect();
    for entry in &branch {
        if omitted.contains(entry.id.as_str()) {
            continue;
        }
        let start = items.len();
        match &entry.kind {
            EntryKind::Message { message } => match message {
                SessionMessage::Llm(Message::User(user)) => {
                    items.push(user_item(user.timestamp, &user.content, "replace"));
                }
                SessionMessage::Llm(Message::Assistant(reply)) => {
                    for call in reply.tool_calls() {
                        tool_rows.insert(call.id.clone(), tool_row_id(reply.timestamp, &call.id));
                    }
                    items.extend(reply_items(reply));
                }
                SessionMessage::Llm(Message::ToolResult(result)) => {
                    let id = tool_rows
                        .get(&result.tool_call_id)
                        .cloned()
                        .unwrap_or_else(|| format!("tool-{}", result.tool_call_id));
                    merge_indexed(
                        &mut items,
                        &mut first_by_key,
                        tool_result_item(
                            id,
                            &result.content,
                            result.details.as_ref(),
                            result.is_error,
                            result.timestamp,
                        ),
                    );
                }
                SessionMessage::Llm(Message::System(_)) => {}
                SessionMessage::Custom(custom) if custom.display => {
                    items.push(notice_item(
                        format!("custom{}", custom.timestamp),
                        "Agent notice",
                        &content_text(&custom.content),
                        custom.timestamp,
                    ));
                }
                // Maple's composer does not run the user's own shell commands yet.
                SessionMessage::BashExecution(_)
                | SessionMessage::Custom(_)
                | SessionMessage::CompactionSummary(_)
                | SessionMessage::BranchSummary(_) => {}
            },
            EntryKind::Custom {
                custom_type,
                data: Some(data),
            } if custom_type == MAPLE_NOTICE_ENTRY => {
                if let (Some(id), Some(text)) = (
                    data.get("id").and_then(Value::as_str),
                    data.get("text").and_then(Value::as_str),
                ) {
                    items.push(notice_item(
                        id.to_string(),
                        "Agent notice",
                        text,
                        entry.timestamp,
                    ));
                }
            }
            // An external agent's turn that ended after its tool call did
            // leaves what it did on the call's row.
            EntryKind::Custom {
                custom_type,
                data: Some(data),
            } if custom_type == external_agents::TURN_END_ENTRY => {
                if let Some(item) = external_agents::turn_end_row(data, created_ms(entry.timestamp))
                {
                    merge_indexed(&mut items, &mut first_by_key, item);
                }
            }
            EntryKind::Compaction { .. } => {
                items.push(notice_item(
                    format!("compaction-{}", entry.id),
                    "Context compacted",
                    "Earlier messages were summarized to make room in the context.",
                    entry.timestamp,
                ));
            }
            EntryKind::BranchSummary { summary, .. } if !summary.is_empty() => {
                items.push(notice_item(
                    format!("branch-summary-{}", entry.id),
                    "Branch summary",
                    summary,
                    entry.timestamp,
                ));
            }
            _ => {}
        }
        for (index, item) in items.iter().enumerate().skip(start) {
            first_by_key.entry(item.id.clone()).or_insert(index);
        }
    }
    items
}

/// A custom external-agent turn end, using the canonical external builder.
pub(crate) fn external_turn_end_row(
    data: &Value,
    timestamp: Timestamp,
) -> Option<AgentTimelineItem> {
    external_agents::turn_end_row(data, created_ms(timestamp))
}

pub(crate) fn is_external_turn_end(custom_type: &str) -> bool {
    custom_type == external_agents::TURN_END_ENTRY
}

#[cfg(test)]
fn merge_indexed(
    items: &mut Vec<AgentTimelineItem>,
    first_by_key: &mut HashMap<String, usize>,
    incoming: AgentTimelineItem,
) {
    if let Some(&index) = first_by_key.get(&incoming.id) {
        merge_item(&mut items[index], incoming);
    } else {
        first_by_key.insert(incoming.id.clone(), items.len());
        items.push(incoming);
    }
}

/// Merge `incoming` into the row with its id, or add it.
pub(crate) fn merge_into(items: &mut Vec<AgentTimelineItem>, incoming: AgentTimelineItem) {
    let Some(previous) = items.iter_mut().find(|item| item.id == incoming.id) else {
        items.push(incoming);
        return;
    };
    merge_item(previous, incoming);
}

pub(crate) fn merge_item(previous: &mut AgentTimelineItem, incoming: AgentTimelineItem) {
    let append = incoming.merge == "append"
        && matches!(incoming.item_type.as_str(), "message" | "thinking")
        && incoming.text.is_some();
    previous.title = merged_tool_title(previous, &incoming);
    if append {
        previous
            .text
            .get_or_insert_with(String::new)
            .push_str(incoming.text.as_deref().unwrap_or_default());
    } else if incoming.text.is_some() {
        previous.text = incoming.text;
    }
    previous.item_type = incoming.item_type;
    if incoming.role.is_some() {
        previous.role = incoming.role;
    }
    if incoming.status.is_some() {
        previous.status = incoming.status;
    }
    if incoming.input.is_some() {
        previous.input = incoming.input;
    }
    if incoming.output.is_some() {
        previous.output = incoming.output;
    }
    previous.created_ms = incoming.created_ms;
    previous.revision = incoming.revision;
    previous.merge = incoming.merge;
}

/// A user's message: what they typed, and the images they attached, which
/// the interface draws from the task's attachments.
pub(crate) fn user_item(
    timestamp: Timestamp,
    content: &[Content],
    merge: &str,
) -> AgentTimelineItem {
    let text = content_text(content);
    let (text, input) = match split_attachment_prompt(&text) {
        Some((typed, attachments)) => (typed, Some(json!({ "attachments": attachments }))),
        None => (text, None),
    };
    AgentTimelineItem {
        persisted: None,
        live: None,
        truncated: None,
        revision: 0,
        id: user_row_id(timestamp),
        item_type: "message".to_string(),
        role: Some("user".to_string()),
        title: None,
        text: Some(text),
        status: None,
        input,
        output: None,
        created_ms: created_ms(timestamp),
        merge: merge.to_string(),
    }
}

/// The rows of a finished reply: its thinking, its text, its tool calls and,
/// when it failed, its error.
pub(crate) fn reply_items(reply: &AssistantMessage) -> Vec<AgentTimelineItem> {
    let mut items = Vec::new();
    let thinking: Vec<&str> = reply
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantContent::Thinking(thinking) if thinking.redacted => {
                Some("Thinking redacted by provider.")
            }
            AssistantContent::Thinking(thinking) => Some(thinking.thinking.as_str()),
            _ => None,
        })
        .collect();
    if !thinking.is_empty() {
        items.push(thinking_item(
            reply.timestamp,
            thinking.join("\n"),
            "replace",
        ));
    }
    let text = reply.text();
    if reply
        .content
        .iter()
        .any(|block| matches!(block, AssistantContent::Text(_)))
    {
        items.push(text_item(reply.timestamp, text, "replace"));
    }
    for call in reply.tool_calls() {
        items.push(tool_call_item(reply.timestamp, call, "running"));
    }
    if reply.stop_reason == StopReason::Error {
        items.push(reply_error_item(reply));
    }
    items
}

fn thinking_item(timestamp: Timestamp, text: String, merge: &str) -> AgentTimelineItem {
    AgentTimelineItem {
        persisted: None,
        live: None,
        truncated: None,
        revision: 0,
        id: reply_row_id(timestamp, "thinking"),
        item_type: "thinking".to_string(),
        role: Some("thought".to_string()),
        title: Some("Thinking".to_string()),
        text: Some(text),
        status: None,
        input: None,
        output: None,
        created_ms: created_ms(timestamp),
        merge: merge.to_string(),
    }
}

fn text_item(timestamp: Timestamp, text: String, merge: &str) -> AgentTimelineItem {
    AgentTimelineItem {
        persisted: None,
        live: None,
        truncated: None,
        revision: 0,
        id: reply_row_id(timestamp, "text"),
        item_type: "message".to_string(),
        role: Some("assistant".to_string()),
        title: None,
        text: Some(text),
        status: None,
        input: None,
        output: None,
        created_ms: created_ms(timestamp),
        merge: merge.to_string(),
    }
}

fn tool_call_item(reply_timestamp: Timestamp, call: &ToolCall, status: &str) -> AgentTimelineItem {
    AgentTimelineItem {
        persisted: None,
        live: None,
        truncated: None,
        revision: 0,
        id: tool_row_id(reply_timestamp, &call.id),
        item_type: "tool".to_string(),
        role: Some("assistant".to_string()),
        title: Some(
            descriptive_tool_title(&call.name, &call.arguments)
                .unwrap_or_else(|| format_tool_title(&call.name)),
        ),
        text: None,
        status: Some(status.to_string()),
        input: Some(Value::Object(call.arguments.clone())),
        output: None,
        created_ms: created_ms(reply_timestamp),
        merge: "replace".to_string(),
    }
}

pub(crate) fn tool_result_item(
    id: String,
    content: &[Content],
    details: Option<&Value>,
    is_error: bool,
    timestamp: Timestamp,
) -> AgentTimelineItem {
    AgentTimelineItem {
        persisted: None,
        live: None,
        truncated: None,
        revision: 0,
        id,
        item_type: "tool".to_string(),
        role: Some("assistant".to_string()),
        title: None,
        text: None,
        status: Some(if is_error { "failed" } else { "completed" }.to_string()),
        input: None,
        output: Some(json!({
            "text": content_text(content),
            "isError": is_error,
            "structuredContent": details.cloned(),
            "content": content.iter().map(summarize_tool_content).collect::<Vec<_>>(),
        })),
        created_ms: created_ms(timestamp),
        merge: "replace".to_string(),
    }
}

fn reply_error_item(reply: &AssistantMessage) -> AgentTimelineItem {
    let message = reply
        .error_message
        .clone()
        .unwrap_or_else(|| "The model's reply failed".to_string());
    let (title, text) = error_row_copy(&message);
    AgentTimelineItem {
        persisted: None,
        live: None,
        truncated: None,
        revision: 0,
        id: reply_row_id(reply.timestamp, "error"),
        item_type: "error".to_string(),
        role: Some("system".to_string()),
        title: Some(title.to_string()),
        text: Some(text),
        status: Some("failed".to_string()),
        input: None,
        output: None,
        created_ms: created_ms(reply.timestamp),
        merge: "replace".to_string(),
    }
}

/// The row heading and body for a failed reply. An out-of-usage refusal
/// drops Pi's status wrapper so the transcript shows the sentence itself.
fn error_row_copy(message: &str) -> (&'static str, String) {
    if message.contains(super::provider::USAGE_LIMIT_MESSAGE) {
        (
            "Out of usage",
            super::provider::USAGE_LIMIT_MESSAGE.to_string(),
        )
    } else {
        (
            error_title(message),
            bounded_timeline_text(message, MAX_AGENT_ERROR_CHARS),
        )
    }
}

/// The row heading for a failed reply.
fn error_title(message: &str) -> &'static str {
    use super::provider::{
        AUTHENTICATION_ERROR_MESSAGE, CONTEXT_OVERFLOW_MESSAGE, CREDITS_EXHAUSTED_MESSAGE,
    };
    if message.contains(AUTHENTICATION_ERROR_MESSAGE) {
        "Authentication failed"
    } else if message.contains(CREDITS_EXHAUSTED_MESSAGE) {
        "Credits exhausted"
    } else if message.contains(CONTEXT_OVERFLOW_MESSAGE) {
        "Context limit exceeded"
    } else {
        "Agent error"
    }
}

pub(crate) fn notice_item(
    id: String,
    title: &str,
    text: &str,
    timestamp: Timestamp,
) -> AgentTimelineItem {
    AgentTimelineItem {
        persisted: None,
        live: None,
        truncated: None,
        revision: 0,
        id,
        item_type: "system".to_string(),
        role: Some("system".to_string()),
        title: Some(title.to_string()),
        text: Some(bounded_timeline_text(text, 500)),
        status: None,
        input: None,
        output: None,
        created_ms: created_ms(timestamp),
        merge: "replace".to_string(),
    }
}

/// An error row that belongs to no message, such as a run that could not start.
pub(crate) fn error_item(message: String) -> AgentTimelineItem {
    let now = pi_ai::now_ms();
    let (title, text) = error_row_copy(&message);
    AgentTimelineItem {
        persisted: None,
        live: None,
        truncated: None,
        revision: 0,
        id: format!("error-{now}"),
        item_type: "error".to_string(),
        role: Some("system".to_string()),
        title: Some(title.to_string()),
        text: Some(text),
        status: Some("failed".to_string()),
        input: None,
        output: None,
        created_ms: created_ms(now),
        merge: "replace".to_string(),
    }
}

fn summarize_tool_content(content: &Content) -> Value {
    match content {
        Content::Text(text) => json!({ "type": "text", "text": text.text }),
        Content::Image(image) => json!({
            "type": "image",
            "mimeType": image.mime_type,
            "base64Chars": image.data.len(),
            "dataOmitted": true,
        }),
    }
}

/// Turns a run's events into the rows they change.
#[derive(Default)]
pub(crate) struct LiveTimeline {
    /// The reply streaming now.
    reply: Option<StreamingReply>,
    /// Tool call rows by call id, for their execution events.
    tool_rows: HashMap<String, String>,
}

struct StreamingReply {
    timestamp: Timestamp,
    /// Content index → whether it is a text, thinking or tool-call block.
    blocks: HashMap<usize, Block>,
    has_text: bool,
    has_thinking: bool,
}

#[derive(Clone)]
enum Block {
    Text,
    Thinking,
    ToolCall(String),
}

impl LiveTimeline {
    /// The rows an event adds or changes.
    pub(crate) fn rows(&mut self, event: &AgentEvent<SessionMessage>) -> Vec<AgentTimelineItem> {
        match event {
            AgentEvent::MessageStart { message } => match message {
                SessionMessage::Llm(Message::User(user)) => {
                    vec![user_item(user.timestamp, &user.content, "replace")]
                }
                SessionMessage::Llm(Message::Assistant(reply)) => {
                    self.reply = Some(StreamingReply {
                        timestamp: reply.timestamp,
                        blocks: HashMap::new(),
                        has_text: false,
                        has_thinking: false,
                    });
                    Vec::new()
                }
                SessionMessage::Custom(custom) if custom.display => vec![notice_item(
                    format!("custom{}", custom.timestamp),
                    "Agent notice",
                    &content_text(&custom.content),
                    custom.timestamp,
                )],
                _ => Vec::new(),
            },
            AgentEvent::MessageUpdate { event } => self.reply_delta(event),
            AgentEvent::MessageEnd {
                message: SessionMessage::Llm(Message::Assistant(reply)),
            } => {
                self.reply = None;
                for call in reply.tool_calls() {
                    self.tool_rows
                        .insert(call.id.clone(), tool_row_id(reply.timestamp, &call.id));
                }
                // The finished reply settles every row it streamed.
                reply_items(reply)
            }
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                let Some(id) = self.tool_rows.get(tool_call_id) else {
                    return Vec::new();
                };
                let arguments = args.as_object().cloned().unwrap_or_default();
                vec![AgentTimelineItem {
                    persisted: None,
                    live: None,
                    truncated: None,
                    revision: 0,
                    id: id.clone(),
                    item_type: "tool".to_string(),
                    role: Some("assistant".to_string()),
                    title: Some(
                        descriptive_tool_title(tool_name, &arguments)
                            .unwrap_or_else(|| format_tool_title(tool_name)),
                    ),
                    text: None,
                    status: Some("running".to_string()),
                    input: Some(args.clone()),
                    output: None,
                    created_ms: created_ms(pi_ai::now_ms()),
                    merge: "replace".to_string(),
                }]
            }
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                partial_result,
                ..
            } => {
                let Some(id) = self.tool_rows.get(tool_call_id) else {
                    return Vec::new();
                };
                let mut item = tool_result_item(
                    id.clone(),
                    &partial_result.content,
                    partial_result.details.as_ref(),
                    false,
                    pi_ai::now_ms(),
                );
                item.status = Some("running".into());
                vec![item]
            }
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                result,
                is_error,
                ..
            } => {
                // Retire the occurrence so a delayed partial result cannot
                // overwrite its final status or output while other tools run.
                let Some(id) = self.tool_rows.remove(tool_call_id) else {
                    return Vec::new();
                };
                vec![tool_result_item(
                    id,
                    &result.content,
                    result.details.as_ref(),
                    *is_error,
                    pi_ai::now_ms(),
                )]
            }
            _ => Vec::new(),
        }
    }

    fn reply_delta(&mut self, event: &AssistantMessageEvent) -> Vec<AgentTimelineItem> {
        let Some(reply) = self.reply.as_mut() else {
            return Vec::new();
        };
        match event {
            AssistantMessageEvent::TextStart { index } => {
                reply.blocks.insert(*index, Block::Text);
                // Text blocks of one reply are joined with newlines.
                if std::mem::replace(&mut reply.has_text, true) {
                    return vec![text_item(reply.timestamp, "\n".to_string(), "append")];
                }
                vec![text_item(reply.timestamp, String::new(), "append")]
            }
            AssistantMessageEvent::TextDelta { delta, .. } => {
                vec![text_item(reply.timestamp, delta.clone(), "append")]
            }
            AssistantMessageEvent::ThinkingStart { index } => {
                reply.blocks.insert(*index, Block::Thinking);
                if std::mem::replace(&mut reply.has_thinking, true) {
                    return vec![thinking_item(reply.timestamp, "\n".to_string(), "append")];
                }
                vec![thinking_item(reply.timestamp, String::new(), "append")]
            }
            AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                vec![thinking_item(reply.timestamp, delta.clone(), "append")]
            }
            AssistantMessageEvent::ToolCallStart { index, id, name } => {
                reply.blocks.insert(*index, Block::ToolCall(id.clone()));
                let row = tool_row_id(reply.timestamp, id);
                self.tool_rows.insert(id.clone(), row.clone());
                vec![AgentTimelineItem {
                    persisted: None,
                    live: None,
                    truncated: None,
                    revision: 0,
                    id: row,
                    item_type: "tool".to_string(),
                    role: Some("assistant".to_string()),
                    title: Some(format_tool_title(name)),
                    text: None,
                    status: Some("running".to_string()),
                    input: None,
                    output: None,
                    created_ms: created_ms(reply.timestamp),
                    merge: "replace".to_string(),
                }]
            }
            AssistantMessageEvent::ToolCallEnd { index, tool_call } => {
                if let Some(Block::ToolCall(streamed_id)) = reply.blocks.get(index)
                    && streamed_id != &tool_call.id
                {
                    // The server named the call late; keep its first row.
                    self.tool_rows.insert(
                        tool_call.id.clone(),
                        tool_row_id(reply.timestamp, streamed_id),
                    );
                }
                let mut item = tool_call_item(reply.timestamp, tool_call, "running");
                if let Some(row) = self.tool_rows.get(&tool_call.id) {
                    item.id = row.clone();
                }
                vec![item]
            }
            _ => Vec::new(),
        }
    }
}

/// The text of `value`, cut to `max_chars` characters with an ellipsis.
pub(crate) fn bounded_timeline_text(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let bounded = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{bounded}…")
    } else {
        bounded
    }
}

pub(crate) fn skill_load_title<T: Serialize>(tool_name: &str, arguments: &T) -> Option<String> {
    if tool_name != "load_skill" {
        return None;
    }
    let arguments = serde_json::to_value(arguments).ok()?;
    let name = arguments.get("name")?.as_str()?.trim();
    if name.is_empty() {
        return None;
    }
    Some(format!(
        "Loading skill: {}",
        bounded_timeline_text(name, MAX_AGENT_SESSION_TITLE_CHARS)
    ))
}

/// Friendly display label for a tool name, e.g. `bash` -> "Terminal".
/// Falls back to the mechanically-cleaned name for anything unmapped.
pub(crate) fn friendly_tool_label(name: &str) -> String {
    let bare = name.rsplit("__").next().unwrap_or(name);
    match bare {
        "bash" | "powershell" | "shell" => "Terminal".to_string(),
        "python" => "Python".to_string(),
        "text_editor" | "str_replace_editor" | "str_replace_based_edit_tool" => {
            "Editor".to_string()
        }
        "web_search" => "Web Search".to_string(),
        "read_file" => "Read file".to_string(),
        "write_file" => "Write file".to_string(),
        "list_files" => "List files".to_string(),
        "glob" => "Find files".to_string(),
        "grep" => "Search".to_string(),
        _ => format_tool_title(name),
    }
}

/// A tool title that names the most relevant argument, e.g. "Terminal: ls -la",
/// or `None` when no useful argument is present.
pub(crate) fn descriptive_tool_title<T: Serialize>(
    tool_name: &str,
    arguments: &T,
) -> Option<String> {
    if let Some(skill) = skill_load_title(tool_name, arguments) {
        return Some(skill);
    }
    let arguments = serde_json::to_value(arguments).ok()?;
    // Most-descriptive argument per tool, in priority order. Only a shell
    // is described by its command; an editor call is about the file.
    let bare_name = tool_name.rsplit("__").next().unwrap_or(tool_name);
    if bare_name == "python" {
        let first_line = arguments
            .get("code")
            .and_then(|code| code.as_str())
            .map(str::trim)
            .filter(|code| !code.is_empty())
            .and_then(|code| code.lines().next());
        return Some(match first_line {
            Some(line) => format!(
                "Python: {}",
                bounded_timeline_text(line, MAX_AGENT_SESSION_TITLE_CHARS)
            ),
            None => "Python".to_string(),
        });
    }
    let keys: &[&str] = if matches!(bare_name, "bash" | "powershell" | "shell") {
        &[
            "command",
            "path",
            "file_path",
            "file",
            "pattern",
            "query",
            "url",
            "uri",
        ]
    } else {
        &[
            "path",
            "file_path",
            "file",
            "command",
            "pattern",
            "query",
            "url",
            "uri",
        ]
    };
    let detail = keys
        .iter()
        .find_map(|key| arguments.get(*key).and_then(|value| value.as_str()))
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let first_line = detail.lines().next().unwrap_or(detail).trim();
    let label = friendly_tool_label(tool_name);
    Some(format!(
        "{label}: {}",
        bounded_timeline_text(first_line, MAX_AGENT_SESSION_TITLE_CHARS)
    ))
}

/// When a skill-loading call finishes, its title says whether it loaded.
pub(crate) fn merged_tool_title(
    previous: &AgentTimelineItem,
    incoming: &AgentTimelineItem,
) -> Option<String> {
    const LOADING_SKILL_PREFIX: &str = "Loading skill: ";

    if incoming.item_type == "tool"
        && let Some(skill_name) = previous
            .title
            .as_deref()
            .and_then(|title| title.strip_prefix(LOADING_SKILL_PREFIX))
    {
        let prefix = match incoming.status.as_deref() {
            Some("completed") => Some("Loaded skill: "),
            Some("failed") => Some("Couldn’t load skill: "),
            _ => None,
        };
        if let Some(prefix) = prefix {
            return Some(format!("{prefix}{skill_name}"));
        }
    }
    incoming.title.clone().or_else(|| previous.title.clone())
}

pub(crate) fn format_tool_title(name: &str) -> String {
    if let Some(title) = external_agents::tool_title(name) {
        return title.to_string();
    }
    // An MCP tool, `mcp__<server>__<tool>`, reads as "server: tool".
    let name = name.strip_prefix("mcp__").unwrap_or(name);
    let normalized = name.replace("__", ": ").replace('_', " ");
    normalized
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod python_title_tests {
    use super::*;

    #[test]
    fn python_titles_keep_one_bounded_code_line_and_label_reset_only_calls() {
        assert_eq!(friendly_tool_label("python"), "Python");
        assert_eq!(
            descriptive_tool_title(
                "python",
                &json!({"code": "\nvalues = [2, 3, 5]\nsum(values)"})
            ),
            Some("Python: values = [2, 3, 5]".to_string())
        );
        assert_eq!(
            descriptive_tool_title("python", &json!({"code": "", "reset": true})),
            Some("Python".to_string())
        );
        let title = descriptive_tool_title("python", &json!({"code": "é".repeat(1_024)}))
            .expect("Python title");
        assert!(title.chars().count() <= "Python: ".len() + MAX_AGENT_SESSION_TITLE_CHARS + 1);
        assert!(!title.contains('\n'));
    }
}
