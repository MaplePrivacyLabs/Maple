//! Projection of Goose state onto what a host renders.
//!
//! A conversation, a single message, or a live tool exchange becomes a list
//! of [`AgentTimelineItem`]s here: titles for tool calls, coalescing of a
//! request with its response, the compaction notice, permission status
//! updates, and the live overlay a running turn keeps ahead of the
//! persisted history. Session summaries live here for the same reason:
//! they are the host's view of a session, not the runtime's.

use super::*;

#[derive(Default)]
pub(super) struct ConversationTimelineProjectionState {
    pub(super) surfaced_thinking_in_inference: bool,
}

/// Project a stored Goose conversation into Maple's presentation timeline.
///
/// Goose deliberately repeats reasoning blocks on each split tool-request
/// message. That replay belongs in the provider history, but it is not a second
/// user-visible thought. Keep this normalization local to a single conversation
/// so concurrent Agent sessions cannot affect one another and the
/// persisted/provider-facing history remains byte-for-byte unchanged.
pub(super) fn conversation_to_timeline_items(
    conversation: &Conversation,
) -> Vec<AgentTimelineItem> {
    let mut state = ConversationTimelineProjectionState::default();
    let mut items = Vec::new();
    let mut current_turn_item_start = 0;
    let mut resolved_permission_ids = HashSet::new();
    let messages = conversation.messages();

    for (index, message) in messages.iter().enumerate() {
        let role = message_role(message);
        let assistant = role == "assistant";
        let inference_ends = assistant && message.metadata.usage.is_some();

        // A real user message starts a new user turn. Tool responses are
        // intentionally chain-neutral because Goose interleaves them between
        // split requests from the same turn.
        if is_real_user_message(message, &role) {
            state.surfaced_thinking_in_inference = false;
            current_turn_item_start = items.len();
            resolved_permission_ids.clear();
        }

        for content in &message.content {
            match content {
                MessageContent::ToolResponse(response) => {
                    resolved_permission_ids.insert(response.id.clone());
                }
                MessageContent::ActionRequired(action) => {
                    if let ActionRequiredData::ElicitationResponse { id, .. } = &action.data {
                        resolved_permission_ids.insert(id.clone());
                    }
                }
                _ => {}
            }
        }
        settle_turn_permission_items(
            &mut items[current_turn_item_start..],
            &resolved_permission_ids,
            false,
        );

        let visible_message = message.user_visible_content();
        // Match Goose's own session presentation contract: agent-only grind,
        // retry, goal, and other internal messages stay in provider history but
        // never become user-facing Maple timeline rows.
        if !visible_message.is_user_visible() || visible_message.content.is_empty() {
            if inference_ends {
                state.surfaced_thinking_in_inference = false;
            }
            continue;
        }

        let mut thinking = message_thinking_projection(&visible_message);
        let has_tool_request = visible_message.content.iter().any(|content| {
            matches!(
                content,
                MessageContent::ToolRequest(_) | MessageContent::FrontendToolRequest(_)
            )
        });

        // Goose intentionally copies reasoning onto every persisted split
        // tool-request message for provider history. Its live AgentEvent stream
        // emits that reasoning only once per provider inference. Reconstruct the
        // same presentation boundary from the usage ledger Goose attaches to the
        // inference's final assistant message. If no ledger boundary is reachable
        // before the next real user turn, preserve every block rather than guess.
        // Replace this reconstruction if Goose adds an explicit persisted
        // inference ID or replay marker to its public message contract.
        let has_usage_boundary =
            assistant && provider_inference_has_usage_boundary(&messages[index..]);
        if assistant
            && has_tool_request
            && state.surfaced_thinking_in_inference
            && has_usage_boundary
        {
            thinking = None;
        } else if assistant && thinking.is_some() {
            state.surfaced_thinking_in_inference = true;
        }

        items.extend(message_to_timeline_items_with_thinking(
            &visible_message,
            false,
            thinking.as_deref(),
        ));
        settle_turn_permission_items(
            &mut items[current_turn_item_start..],
            &resolved_permission_ids,
            is_stopped_notice(message),
        );

        if inference_ends {
            state.surfaced_thinking_in_inference = false;
        }
    }

    coalesce_timeline_items(items)
}

pub(super) fn is_stopped_notice(message: &Message) -> bool {
    message.is_user_visible()
        && !message.is_agent_visible()
        && message.content.iter().any(|content| {
            matches!(
                content,
                MessageContent::SystemNotification(notification)
                    if notification.notification_type == SystemNotificationType::InlineMessage
                        && notification.msg == "Stopped by user"
            )
        })
}

pub(super) fn settle_turn_permission_items(
    items: &mut [AgentTimelineItem],
    resolved_ids: &HashSet<String>,
    cancel_unresolved: bool,
) {
    for item in items {
        if item.item_type != "permission" || item.status.as_deref() != Some("pending") {
            continue;
        }
        let resolved = item
            .id
            .strip_prefix("permission-")
            .or_else(|| item.id.strip_prefix("elicitation-"))
            .is_some_and(|id| resolved_ids.contains(id));
        if resolved {
            item.status = Some("completed".to_string());
        } else if cancel_unresolved {
            item.status = Some("cancelled".to_string());
        }
    }
}

pub(super) fn reconcile_desktop_permission_items(
    items: &mut [AgentTimelineItem],
    pending_routes: &HashMap<String, AgentPermissionRouting>,
    calling_surface_active: bool,
) {
    for item in items {
        if item.item_type != "permission" || item.status.as_deref() != Some("pending") {
            continue;
        }
        let request_id = item
            .id
            .strip_prefix("permission-")
            .or_else(|| item.id.strip_prefix("elicitation-"));
        let route = request_id.and_then(|id| pending_routes.get(id));
        item.status = match (calling_surface_active, route) {
            (false, Some(AgentPermissionRouting::Desktop)) => continue,
            (true, _) | (_, Some(AgentPermissionRouting::CallingSurface)) => {
                Some("controlled_externally".to_string())
            }
            (false, None) => Some("cancelled".to_string()),
        };
    }
}

pub(super) fn is_real_user_message(message: &Message, role: &str) -> bool {
    if role != "user" || !message.is_user_visible() {
        return false;
    }
    message
        .user_visible_content()
        .content
        .iter()
        .any(|content| !matches!(content, MessageContent::ToolResponse(_)))
}

pub(super) fn provider_inference_has_usage_boundary(messages: &[Message]) -> bool {
    for message in messages {
        let role = message_role(message);
        if is_real_user_message(message, &role) {
            return false;
        }
        if role == "assistant" && message.metadata.usage.is_some() {
            return true;
        }
    }
    false
}

pub(super) fn message_thinking_projection(message: &Message) -> Option<String> {
    // Match Goose Desktop's ACP adapter: concatenate adjacent thought chunks
    // by message without rewriting their text. The frontend decides whether
    // the fully merged thought is renderable, so a streamed punctuation or
    // whitespace suffix is never lost.
    let mut text = String::new();
    let mut found = false;

    for content in &message.content {
        match content {
            MessageContent::Thinking(thinking) => {
                found = true;
                text.push_str(&thinking.thinking);
            }
            MessageContent::RedactedThinking(_) => {
                found = true;
                text.push_str("Thinking redacted by provider.");
            }
            _ => {}
        }
    }
    found.then_some(text)
}

pub(super) fn message_to_timeline_items(message: &Message, live: bool) -> Vec<AgentTimelineItem> {
    if !message.is_user_visible() {
        return Vec::new();
    }
    let thinking = message_thinking_projection(message);
    message_to_timeline_items_with_thinking(message, live, thinking.as_deref())
}

pub(super) fn message_to_timeline_items_with_thinking(
    message: &Message,
    live: bool,
    thinking: Option<&str>,
) -> Vec<AgentTimelineItem> {
    // Goose persists the canonical message for provider history but projects
    // content-level audience annotations before emitting live user events.
    // Apply the same projection when rebuilding Maple's timeline from storage.
    let message = message.user_visible_content();
    if !message.is_user_visible() || message.content.is_empty() {
        return Vec::new();
    }
    let role = message_role(&message);
    let base_id = message
        .id
        .clone()
        .unwrap_or_else(|| format!("message-{}-{}", role, message.created));
    let created_ms = if message.created > 0 {
        (message.created as u128) * 1000
    } else {
        unix_ms()
    };
    let merge = if live { "append" } else { "replace" }.to_string();
    let image_attachments = message_image_attachments(&message);
    let visible_text = message_original_user_text(&message).unwrap_or_else(|| {
        message
            .content
            .iter()
            .filter_map(|content| match content {
                MessageContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<String>()
    });
    let image_input = (!image_attachments.is_empty()).then(|| {
        json!({
            "imageAttachments": image_attachments,
        })
    });

    let mut emitted_text = false;
    let mut emitted_thinking = false;
    message
        .content
        .iter()
        .enumerate()
        .filter_map(|(index, content)| match content {
            MessageContent::Text(_) => {
                if emitted_text {
                    return None;
                }
                emitted_text = true;
                Some(AgentTimelineItem {
                    id: format!("{base_id}-text"),
                    item_type: "message".to_string(),
                    role: Some(role.clone()),
                    title: None,
                    text: Some(visible_text.clone()),
                    status: None,
                    input: image_input.clone(),
                    output: None,
                    created_ms,
                    merge: merge.clone(),
                })
            }
            MessageContent::Thinking(_) | MessageContent::RedactedThinking(_) => {
                if emitted_thinking {
                    return None;
                }
                emitted_thinking = true;
                thinking.map(|thinking| AgentTimelineItem {
                    id: format!("{base_id}-thinking"),
                    item_type: "thinking".to_string(),
                    role: Some("thought".to_string()),
                    title: Some("Thinking".to_string()),
                    text: Some(thinking.to_string()),
                    status: None,
                    input: None,
                    output: None,
                    created_ms,
                    merge: merge.clone(),
                })
            }
            MessageContent::ToolRequest(request) => Some(tool_request_item(request, created_ms)),
            MessageContent::ToolResponse(response) => {
                Some(tool_response_item(response, created_ms))
            }
            MessageContent::ToolConfirmationRequest(request) => Some(AgentTimelineItem {
                id: format!("permission-{}", request.id),
                item_type: "permission".to_string(),
                role: Some("system".to_string()),
                title: Some(
                    descriptive_tool_title(&request.tool_name, &request.arguments)
                        .unwrap_or_else(|| format_tool_title(&request.tool_name)),
                ),
                text: request.prompt.clone(),
                status: Some("pending".to_string()),
                input: Some(Value::Object(request.arguments.clone())),
                output: None,
                created_ms,
                merge: "replace".to_string(),
            }),
            MessageContent::ActionRequired(action) => action_required_item(action, created_ms),
            MessageContent::FrontendToolRequest(request) => {
                let (title, text, input, status) = match &request.tool_call {
                    Ok(call) => (
                        descriptive_tool_title(call.name.as_ref(), &call.arguments)
                            .unwrap_or_else(|| format_tool_title(call.name.as_ref())),
                        None,
                        Some(serde_json::to_value(&call.arguments).unwrap_or(Value::Null)),
                        "pending".to_string(),
                    ),
                    Err(error) => (
                        "Tool call parse failed".to_string(),
                        Some(bounded_timeline_text(
                            &error.to_string(),
                            MAX_AGENT_ERROR_CHARS,
                        )),
                        None,
                        "failed".to_string(),
                    ),
                };
                Some(AgentTimelineItem {
                    id: request.id.clone(),
                    item_type: "tool".to_string(),
                    role: Some("assistant".to_string()),
                    title: Some(title),
                    text,
                    status: Some(status),
                    input,
                    output: None,
                    created_ms,
                    merge: "replace".to_string(),
                })
            }
            MessageContent::SystemNotification(notification) => Some(system_notification_item(
                &base_id,
                index,
                notification,
                created_ms,
            )),
            MessageContent::Error(error) => {
                Some(message_error_item(&base_id, index, error, created_ms))
            }
            // Images are provider-history payloads, not timeline events. The
            // read_image tool request/result already gives users the useful,
            // bounded presentation without exposing base64 metadata.
            MessageContent::Image(_) => None,
        })
        .collect()
}

/// Maple wording for a Goose compaction notice, or `None` when the text
/// is not one. Maple users never see the goose name, and ACP clients get
/// the same wording.
pub fn compaction_notice_text(text: &str) -> Option<&'static str> {
    match text.trim() {
        "goose is compacting the conversation..." => Some("Compacting…"),
        "Context limit reached. Compacting to continue conversation..." => {
            Some("Context limit reached — compacting to continue…")
        }
        _ => None,
    }
}

pub(super) fn system_notification_item(
    base_id: &str,
    index: usize,
    notification: &SystemNotificationContent,
    created_ms: u128,
) -> AgentTimelineItem {
    let title = match notification.notification_type {
        SystemNotificationType::ThinkingMessage => "Thinking",
        SystemNotificationType::ProgressMessage => "Progress",
        SystemNotificationType::InlineMessage => "Agent notice",
        SystemNotificationType::CreditsExhausted => "Credits exhausted",
    };
    AgentTimelineItem {
        id: format!("{base_id}-system-{index}"),
        item_type: "system".to_string(),
        role: Some("system".to_string()),
        title: Some(title.to_string()),
        text: Some(bounded_timeline_text(&notification.msg, 500)),
        status: None,
        input: None,
        // Provider-specific structured data can contain raw request or model
        // payloads. The stable title/message above is the user-facing contract.
        output: None,
        created_ms,
        merge: "replace".to_string(),
    }
}

pub(super) fn tool_request_item(
    request: &goose::conversation::message::ToolRequest,
    created_ms: u128,
) -> AgentTimelineItem {
    match &request.tool_call {
        Ok(call) => AgentTimelineItem {
            id: request.id.clone(),
            item_type: "tool".to_string(),
            role: Some("assistant".to_string()),
            title: Some(
                descriptive_tool_title(call.name.as_ref(), &call.arguments).unwrap_or_else(|| {
                    request
                        .generated_title()
                        .unwrap_or_else(|| call.name.as_ref())
                        .to_string()
                }),
            ),
            text: request
                .generated_chain_summary()
                .map(|summary| summary.summary),
            status: Some("running".to_string()),
            input: Some(serde_json::to_value(&call.arguments).unwrap_or(Value::Null)),
            output: None,
            created_ms,
            merge: "replace".to_string(),
        },
        Err(error) => {
            // Derive the id from the request so a reload produces the same
            // row and two failures in one millisecond cannot collide.
            let mut item = error_item(format!("Tool call parse failed: {error}"));
            item.id = format!("{}-parse-error", request.id);
            item.created_ms = created_ms;
            item
        }
    }
}

pub(super) fn skill_load_title<T: Serialize>(tool_name: &str, arguments: &T) -> Option<String> {
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

/// Friendly display label for a raw goose tool name, e.g. `developer__shell`
/// -> "Terminal", `developer__text_editor` -> "Editor". Falls back to the
/// mechanically-cleaned name for anything unmapped.
pub(super) fn friendly_tool_label(name: &str) -> String {
    // Strip any `extension__` prefix so both `shell` and `developer__shell`
    // map the same way.
    let bare = name.rsplit("__").next().unwrap_or(name);
    match bare {
        "shell" => "Terminal".to_string(),
        "python_code" => "Python".to_string(),
        "delegate" => "Subagent".to_string(),
        "load" => "Load".to_string(),
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

/// Build a descriptive tool title that includes the most relevant argument so
/// the timeline shows *what* is running (e.g. "Terminal: ls -la") instead of a
/// bare, repeated tool name ("shell"). Returns `None` when no useful argument
/// is present, so callers can fall back to their existing title logic.
pub(super) fn descriptive_tool_title<T: Serialize>(
    tool_name: &str,
    arguments: &T,
) -> Option<String> {
    // Preserve the existing, dedicated skill wording.
    if let Some(skill) = skill_load_title(tool_name, arguments) {
        return Some(skill);
    }
    let arguments = serde_json::to_value(arguments).ok()?;
    // Most-descriptive argument per tool, in priority order. Only the shell
    // is described by its command; an editor call such as
    // `{command: "view", path: "src/main.rs"}` is about the file.
    let bare_name = tool_name.rsplit("__").next().unwrap_or(tool_name);
    if bare_name == "python_code" {
        let first_line = arguments
            .get("code")
            .and_then(Value::as_str)
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
    let keys: &[&str] = if bare_name == "shell" {
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
    } else if matches!(bare_name, "delegate" | "load") {
        // Collecting a background subagent names it by session ID, which
        // tells the user nothing.
        if bare_name == "load"
            && arguments
                .get("source")
                .and_then(|value| value.as_str())
                .is_some_and(|source| looks_like_session_id(source.trim()))
        {
            return Some("Subagent result".to_string());
        }
        // A subagent is described by the recipe it runs, or by the task
        // it was given.
        &["source", "instructions"]
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

    // Keep it to one readable line.
    let first_line = detail.lines().next().unwrap_or(detail).trim();
    let label = friendly_tool_label(tool_name);
    Some(format!(
        "{label}: {}",
        bounded_timeline_text(first_line, MAX_AGENT_SESSION_TITLE_CHARS)
    ))
}

/// Whether `value` is a Goose session ID (`20260831_4`), which is how
/// Goose names a background subagent.
pub(super) fn looks_like_session_id(value: &str) -> bool {
    let mut parts = value.split('_');
    let (Some(date), Some(ordinal), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    date.len() == 8
        && date.bytes().all(|byte| byte.is_ascii_digit())
        && !ordinal.is_empty()
        && ordinal.bytes().all(|byte| byte.is_ascii_digit())
}

/// The user-facing text of a `delegate` or `load` result.
///
/// Goose names a background subagent by its session ID and repeats it in
/// prose. The ID is the handle the model collects the result with, so it
/// stays in the message Goose persists; only the transcript projection
/// drops it. Returns `None` for text that carries no ID.
pub(super) fn subagent_text_without_ids(text: &str) -> Option<String> {
    // "Task 20260831_4 started in background: "Build the release"\n..."
    if let Some(rest) = text.strip_prefix("Task ")
        && let Some((id, rest)) = rest.split_once(' ')
        && looks_like_session_id(id)
        && let Some(description) = rest.strip_prefix("started in background: ")
    {
        let description = description
            .lines()
            .next()
            .unwrap_or(description)
            .trim()
            .trim_matches('"');
        return Some(format!("Started in the background: {description}"));
    }

    // "# Background Task Result: 20260831_4\n\n**Task:** ..."
    for heading in ["# Background Task Result", "# Background Task Status"] {
        if let Some(rest) = text.strip_prefix(heading)
            && let Some(rest) = rest.strip_prefix(": ")
            && let Some((id, rest)) = rest.split_once('\n')
            && looks_like_session_id(id.trim())
        {
            return Some(format!("{heading}\n{rest}"));
        }
    }

    None
}

pub(super) fn merged_tool_title(
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

pub(super) fn tool_response_item(
    response: &goose::conversation::message::ToolResponse,
    created_ms: u128,
) -> AgentTimelineItem {
    match &response.tool_result {
        Ok(result) => {
            let text = result
                .content
                .iter()
                .filter_map(|content| content.as_text().map(|text| text.text.to_string()))
                .collect::<Vec<_>>()
                .join("\n");
            // The transcript is a projection; Goose keeps the original.
            let text = subagent_text_without_ids(&text).unwrap_or(text);
            let content = result
                .content
                .iter()
                .map(summarize_tool_content)
                .collect::<Vec<_>>();
            AgentTimelineItem {
                id: response.id.clone(),
                item_type: "tool".to_string(),
                role: Some("assistant".to_string()),
                title: tool_response_title(&response.id),
                text: None,
                status: Some(
                    if result.is_error.unwrap_or(false) {
                        "failed"
                    } else {
                        "completed"
                    }
                    .to_string(),
                ),
                input: None,
                output: Some(json!({
                    "text": text,
                    "isError": result.is_error,
                    "structuredContent": result.structured_content,
                    "content": content,
                })),
                created_ms,
                merge: "replace".to_string(),
            }
        }
        Err(error) => AgentTimelineItem {
            id: response.id.clone(),
            item_type: "tool".to_string(),
            role: Some("assistant".to_string()),
            title: tool_response_title(&response.id),
            text: Some(bounded_timeline_text(
                &error.to_string(),
                MAX_AGENT_ERROR_CHARS,
            )),
            status: Some("failed".to_string()),
            input: None,
            output: None,
            created_ms,
            merge: "replace".to_string(),
        },
    }
}

pub(super) fn summarize_tool_content(content: &rmcp::model::ContentBlock) -> Value {
    if let Some(text) = content.as_text() {
        return json!({
            "type": "text",
            "text": text.text,
        });
    }

    if let Some(image) = content.as_image() {
        return image_metadata_value(&image.mime_type, image.data.len());
    }

    json!({
        "type": "other",
        "dataOmitted": true,
    })
}

pub(super) fn image_metadata_value(mime_type: &str, base64_chars: usize) -> Value {
    json!({
        "type": "image",
        "mimeType": mime_type,
        "base64Chars": base64_chars,
        "dataOmitted": true,
    })
}

pub(super) fn coalesce_timeline_items(items: Vec<AgentTimelineItem>) -> Vec<AgentTimelineItem> {
    items.into_iter().fold(Vec::new(), merge_timeline_item)
}

pub(super) fn merge_timeline_item(
    mut current: Vec<AgentTimelineItem>,
    incoming: AgentTimelineItem,
) -> Vec<AgentTimelineItem> {
    let Some(index) = current.iter().position(|item| item.id == incoming.id) else {
        current.push(incoming);
        return current;
    };

    let previous = current[index].clone();
    let append_text = incoming.merge == "append"
        && matches!(incoming.item_type.as_str(), "message" | "thinking")
        && incoming.text.is_some();

    let title = merged_tool_title(&previous, &incoming);
    current[index] = AgentTimelineItem {
        id: incoming.id,
        item_type: incoming.item_type,
        role: incoming.role.or(previous.role),
        title,
        text: if append_text {
            Some(format!(
                "{}{}",
                previous.text.unwrap_or_default(),
                incoming.text.unwrap_or_default()
            ))
        } else {
            incoming.text.or(previous.text)
        },
        status: incoming.status.or(previous.status),
        input: incoming.input.or(previous.input),
        output: incoming.output.or(previous.output),
        created_ms: incoming.created_ms,
        merge: incoming.merge,
    };

    current
}

pub(super) fn action_required_item(
    action: &goose::conversation::message::ActionRequired,
    created_ms: u128,
) -> Option<AgentTimelineItem> {
    match &action.data {
        ActionRequiredData::ToolConfirmation {
            id,
            tool_name,
            arguments,
            prompt,
        } => Some(AgentTimelineItem {
            id: format!("permission-{id}"),
            item_type: "permission".to_string(),
            role: Some("system".to_string()),
            title: Some(
                descriptive_tool_title(tool_name, arguments)
                    .unwrap_or_else(|| format_tool_title(tool_name)),
            ),
            text: prompt.clone(),
            status: Some("pending".to_string()),
            input: Some(Value::Object(arguments.clone())),
            output: None,
            created_ms,
            merge: "replace".to_string(),
        }),
        ActionRequiredData::Elicitation {
            id,
            message,
            requested_schema,
        } => Some(AgentTimelineItem {
            id: format!("elicitation-{id}"),
            item_type: "permission".to_string(),
            role: Some("system".to_string()),
            title: Some("Input requested".to_string()),
            text: Some(message.clone()),
            status: Some("pending".to_string()),
            input: Some(requested_schema.clone()),
            output: None,
            created_ms,
            merge: "replace".to_string(),
        }),
        ActionRequiredData::ElicitationResponse { id, .. } => Some(AgentTimelineItem {
            id: format!("elicitation-response-{id}"),
            item_type: "system".to_string(),
            role: Some("system".to_string()),
            title: Some("Input response".to_string()),
            text: None,
            status: Some("completed".to_string()),
            input: None,
            output: None,
            created_ms,
            merge: "replace".to_string(),
        }),
        // This is persisted state-machine resume bookkeeping, not a new user
        // permission request. Maple keeps Goose's experimental state machine
        // disabled and should not render an extra permission card for it.
        ActionRequiredData::ToolConfirmationResponse { .. } => None,
    }
}

pub(super) fn message_error_item(
    base_id: &str,
    index: usize,
    error: &ErrorContent,
    created_ms: u128,
) -> AgentTimelineItem {
    let title = match error.kind {
        MessageErrorKind::Authentication => "Authentication failed",
        MessageErrorKind::ContextLengthExceeded => "Context limit exceeded",
        MessageErrorKind::CreditsExhausted => "Credits exhausted",
        MessageErrorKind::Other => "Agent error",
    };
    AgentTimelineItem {
        id: format!("{base_id}-error-{index}"),
        item_type: "error".to_string(),
        role: Some("system".to_string()),
        title: Some(title.to_string()),
        text: Some(bounded_timeline_text(&error.message, MAX_AGENT_ERROR_CHARS)),
        status: Some("failed".to_string()),
        input: None,
        output: None,
        created_ms,
        merge: "replace".to_string(),
    }
}

pub(super) fn error_item(message: String) -> AgentTimelineItem {
    AgentTimelineItem {
        id: format!("error-{}", unix_ms()),
        item_type: "error".to_string(),
        role: Some("system".to_string()),
        title: Some("Agent error".to_string()),
        text: Some(bounded_timeline_text(&message, MAX_AGENT_ERROR_CHARS)),
        status: Some("failed".to_string()),
        input: None,
        output: None,
        created_ms: unix_ms(),
        merge: "replace".to_string(),
    }
}

pub(super) fn message_role(message: &Message) -> String {
    serde_json::to_value(&message.role)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| format!("{:?}", message.role).to_lowercase())
}

pub(super) fn format_tool_title(name: &str) -> String {
    let normalized = name.replace("__", ": ").replace('_', " ");
    normalized
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

pub(super) fn tool_name_from_id(id: &str) -> Option<String> {
    // Goose's `functions.<tool>:<sequence>` IDs encode a tool name. Provider
    // IDs such as `chatcmpl-tool-*` do not; returning a title for those would
    // overwrite the request's already-correct title during timeline merging.
    let name = id
        .strip_prefix("functions.")?
        .split(':')
        .next()
        .unwrap_or("")
        .trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

pub(super) fn tool_response_title(id: &str) -> Option<String> {
    tool_name_from_id(id).and_then(|name| {
        // Preserve the request's argument-aware title when the response is
        // merged into the same timeline row.
        (name != "load_skill").then(|| format_tool_title(&name))
    })
}

pub(super) fn permission_decision_from_str(
    decision: &str,
) -> Result<AgentPermissionDecision, String> {
    match decision {
        "allow_once" | "allow" => Ok(AgentPermissionDecision::AllowOnce),
        "deny_once" | "deny" => Ok(AgentPermissionDecision::DenyOnce),
        "cancel" | "cancelled" => Ok(AgentPermissionDecision::Cancel),
        "always_allow" | "always_deny" => {
            Err("Persistent tool permissions are not supported by Maple Agent Mode".to_string())
        }
        other => Err(format!("Unknown permission decision: {other}")),
    }
}

#[cfg(test)]
pub(super) fn permission_from_decision(decision: &str) -> Result<Permission, String> {
    permission_decision_from_str(decision).map(AgentPermissionDecision::goose_permission)
}

pub(super) fn session_summary(session: &Session) -> AgentSessionSummary {
    AgentSessionSummary {
        id: session.id.clone(),
        title: session.name.clone(),
        project_root: path_string(&session.working_dir),
        created_ms: session.created_at.timestamp_millis(),
        updated_ms: session.updated_at.timestamp_millis(),
        message_count: session.message_count,
        model: session
            .model_config
            .as_ref()
            .map(|model| model.model_name.clone()),
        mode: session.goose_mode.to_string(),
        web_enabled: session_web_enabled(session),
        archived: session.archived_at.is_some(),
        acp: session.session_type == SessionType::Acp,
    }
}

/// Read Maple's web flag from the session; absent means enabled.
pub(super) fn session_web_enabled(session: &Session) -> bool {
    session
        .extension_data
        .get_extension_state(MAPLE_WEB_STATE_KEY, MAPLE_WEB_STATE_VERSION)
        .and_then(|value| value.get("enabled"))
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

pub(super) fn sort_sessions_newest_first(sessions: &mut [AgentSessionSummary]) {
    sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_ms));
}

pub(super) async fn record_and_emit_timeline_item(
    events: &AgentRunEventPublisher,
    live_timelines: &LiveTimelines,
    session_id: &str,
    routing: AgentPermissionRouting,
    item: AgentTimelineItem,
) {
    record_timeline_item(live_timelines, session_id, routing, item.clone()).await;
    events.publish(AgentRunEvent::TimelineItem(item)).await;
}

pub(super) async fn record_timeline_item(
    live_timelines: &LiveTimelines,
    session_id: &str,
    routing: AgentPermissionRouting,
    item: AgentTimelineItem,
) {
    let mut timelines = live_timelines.lock().await;
    let current = match timelines.remove(session_id) {
        Some(LiveTimelineEntry {
            routing: owner,
            timeline: LiveTimeline::Streaming(items),
        }) if owner == routing => items,
        // A real user message starts a new live suffix. The preceding terminal
        // row is either already persisted or was a one-turn-only error/notice;
        // carrying it forward could duplicate it on a mid-run session reload.
        Some(LiveTimelineEntry {
            routing: owner,
            timeline: LiveTimeline::Completed(_) | LiveTimeline::Failed(_),
        }) if owner == routing && is_user_message_item(&item) => Vec::new(),
        Some(LiveTimelineEntry {
            routing: owner,
            timeline: LiveTimeline::Completed(candidate),
        }) if owner == routing => candidate.items,
        Some(LiveTimelineEntry {
            routing: owner,
            timeline: LiveTimeline::Failed(items),
        }) if owner == routing => items,
        // A new surface starts its own transient projection. Persisted Goose
        // history remains the shared handoff boundary between surfaces.
        Some(_) => Vec::new(),
        None => Vec::new(),
    };
    timelines.insert(
        session_id.to_string(),
        LiveTimelineEntry {
            routing,
            timeline: LiveTimeline::Streaming(merge_timeline_item(current, item)),
        },
    );
}

/// Goose replaces persisted history during compaction, so any live rows from
/// before that replacement are stale. Keep only the newest visible real-user
/// row as an ID boundary for later events in the still-running turn. A session
/// reload can then use Goose's live presentation suffix wholesale instead of
/// merging it with differently-IDed provider-history reasoning.
pub(super) async fn reseed_live_timeline_after_history_replaced(
    live_timelines: &LiveTimelines,
    session_id: &str,
    routing: AgentPermissionRouting,
    conversation: &Conversation,
) {
    let replacement_boundary = conversation
        .messages()
        .iter()
        .rev()
        .find(|message| {
            let role = message_role(message);
            is_real_user_message(message, &role)
        })
        .and_then(|message| {
            coalesce_timeline_items(message_to_timeline_items(message, false))
                .into_iter()
                .find(is_user_message_item)
        });

    let mut timelines = live_timelines.lock().await;
    match replacement_boundary {
        Some(replacement_boundary) => {
            // Prefer the existing live representation, but only for the user
            // ID confirmed by Goose's replacement history. That preserves the
            // authoritative presentation item without retaining a boundary
            // that compaction or an explicit history command removed.
            let boundary = timelines
                .get(session_id)
                .filter(|entry| entry.routing == routing)
                .and_then(|entry| {
                    entry.timeline.items().iter().rev().find(|item| {
                        is_user_message_item(item) && item.id == replacement_boundary.id
                    })
                })
                .cloned()
                .unwrap_or(replacement_boundary);
            timelines.insert(
                session_id.to_string(),
                LiveTimelineEntry {
                    routing,
                    timeline: LiveTimeline::Streaming(vec![boundary]),
                },
            );
        }
        None => {
            remove_live_timeline_for_routing(&mut timelines, session_id, routing);
        }
    }
}

pub(super) async fn overlay_live_timeline(
    live_timelines: &LiveTimelines,
    session_id: &str,
    routing: AgentPermissionRouting,
    conversation: &Conversation,
    persisted: Vec<AgentTimelineItem>,
) -> Vec<AgentTimelineItem> {
    let live_items = {
        let mut timelines = live_timelines.lock().await;
        let timeline = timelines
            .get(session_id)
            .filter(|entry| entry.routing == routing)
            .map(|entry| entry.timeline.clone());
        match timeline {
            Some(LiveTimeline::Streaming(items)) => items,
            Some(LiveTimeline::Completed(candidate)) => {
                // agent_load_session already paid to load Goose history. Use
                // that snapshot here instead of deserializing it a second time
                // at the end of every prompt.
                if terminal_message_is_persisted(conversation, &candidate) {
                    remove_live_timeline_for_routing(&mut timelines, session_id, routing);
                    Vec::new()
                } else {
                    candidate.items
                }
            }
            Some(LiveTimeline::Failed(items)) => items,
            None => Vec::new(),
        }
    };
    if live_items.is_empty() {
        return persisted;
    }

    overlay_live_timeline_items(persisted, live_items)
}

pub(super) fn overlay_live_timeline_items(
    persisted: Vec<AgentTimelineItem>,
    live_items: Vec<AgentTimelineItem>,
) -> Vec<AgentTimelineItem> {
    // AgentEvent is Goose's authoritative presentation stream. Once its first
    // user boundary also exists in persisted history, keep only the persisted
    // prefix before that turn and use the live suffix wholesale. This avoids
    // matching or rewriting reasoning text when Goose's provider-history copy
    // has a different message ID from the live thought.
    let persisted_boundary = live_items
        .iter()
        .filter(|item| is_user_message_item(item))
        .find_map(|live_user| persisted.iter().position(|item| item.id == live_user.id));
    let mut timeline = match persisted_boundary {
        Some(index) => persisted[..index].to_vec(),
        None => persisted,
    };
    timeline.extend(live_items.into_iter().map(live_overlay_item));
    coalesce_timeline_items(timeline)
}

pub(super) fn is_user_message_item(item: &AgentTimelineItem) -> bool {
    item.item_type == "message" && item.role.as_deref() == Some("user")
}

pub(super) fn live_overlay_item(mut item: AgentTimelineItem) -> AgentTimelineItem {
    item.merge = "replace".to_string();
    item
}

pub(super) async fn update_live_permission_status(
    live_timelines: &LiveTimelines,
    session_id: &str,
    routing: AgentPermissionRouting,
    request_id: &str,
    decision: &str,
) -> Option<AgentTimelineItem> {
    let permission_id = format!("permission-{request_id}");
    let mut timelines = live_timelines.lock().await;
    let entry = timelines.get_mut(session_id)?;
    if entry.routing != routing {
        return None;
    }
    let items = entry.timeline.items_mut();
    let item = items.iter_mut().find(|item| item.id == permission_id)?;
    item.status = Some(decision.to_string());
    item.merge = "replace".to_string();
    Some(item.clone())
}

#[cfg(test)]
mod python_title_tests {
    use super::*;

    #[test]
    fn python_titles_keep_one_bounded_code_line_and_label_reset_only_calls() {
        for name in ["python_code", "developer__python_code"] {
            assert_eq!(friendly_tool_label(name), "Python");
            assert_eq!(
                descriptive_tool_title(name, &json!({"code": "\nvalues = [2, 3, 5]\nsum(values)"})),
                Some("Python: values = [2, 3, 5]".to_string())
            );
            assert_eq!(
                descriptive_tool_title(name, &json!({"code": "", "reset": true})),
                Some("Python".to_string())
            );
            let title = descriptive_tool_title(name, &json!({"code": "é".repeat(1_024)}))
                .expect("Python title");
            assert!(title.chars().count() <= "Python: ".len() + MAX_AGENT_SESSION_TITLE_CHARS + 1);
            assert!(!title.contains('\n'));
        }
    }
}
