//! Conversions between Maple's agent types and the ACP wire shapes: config
//! options, the project-trust prompts, and timeline items rendered as session
//! updates.

use super::transport::AcpOutboundSendError;
use crate::agent::{
    AgentImageUpload, AgentRunTerminal, AgentRunUsage, AgentSlashCommand, AgentTimelineItem,
    compaction_notice_text,
};
use agent_client_protocol::schema::v1::{
    AvailableCommand, AvailableCommandsUpdate, BooleanPropertySchema, ContentBlock, ContentChunk,
    CreateElicitationRequest, ElicitationFormMode, ElicitationSchema, ElicitationSessionScope,
    InitializeRequest, PermissionOption, PermissionOptionKind, PromptResponse,
    RequestPermissionOutcome, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelectOption, SessionId, SessionUpdate, StopReason, TextContent, ToolCall,
    ToolCallContent, ToolCallLocation, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
    ToolKind, Usage,
};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub(super) const MAX_ACP_ERROR_CHARS: usize = 500;
pub(super) const MAX_ACP_TOOL_TEXT_CHARS: usize = 16_000;
#[derive(Default)]
pub(super) struct AcpToolProjection {
    seen: HashSet<String>,
}

/// The model selector is the only config option: Maple has no session
/// modes, every tool call runs without asking.
pub(super) fn acp_config_options(
    model: &str,
    available_models: &[String],
) -> Vec<SessionConfigOption> {
    let model_options = available_models
        .iter()
        .map(|model| SessionConfigSelectOption::new(model.clone(), model.clone()))
        .collect::<Vec<_>>();
    vec![
        SessionConfigOption::select("model", "Model", model.to_string(), model_options)
            .category(SessionConfigOptionCategory::Model),
    ]
}

pub(super) fn acp_session_config_options(
    model: &str,
    available_models: &[String],
    message_count: usize,
) -> Vec<SessionConfigOption> {
    if message_count == 0 {
        acp_config_options(model, available_models)
    } else {
        let locked_models = [model.to_string()];
        acp_config_options(model, &locked_models)
    }
}

/// Parse a whole message shaped like a slash command. Mirrors the desktop
/// composer: one leading `/`, a name without further slashes, optional args.
pub(super) fn parse_slash_command(text: &str) -> Option<(String, String)> {
    let body = text.trim().strip_prefix('/')?;
    let (name, args) = match body.split_once(char::is_whitespace) {
        Some((name, args)) => (name, args.trim()),
        None => (body, ""),
    };
    if name.is_empty() || name.contains('/') {
        return None;
    }
    Some((name.to_string(), args.to_string()))
}

/// The commands an ACP session offers its caller: the agent-side built-ins
/// plus the skills installed for the session's working directory.
pub(super) fn acp_available_commands(skills: &[AgentSlashCommand]) -> SessionUpdate {
    let mut commands = Vec::with_capacity(skills.len() + 1);
    commands.push(AvailableCommand::new(
        "compact",
        "Summarize the conversation to free context",
    ));
    commands.extend(
        skills
            .iter()
            .map(|skill| AvailableCommand::new(skill.name.clone(), skill.description.clone())),
    );
    SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(commands))
}

pub(super) fn acp_usage(usage: AgentRunUsage) -> Usage {
    Usage::new(usage.total_tokens, usage.input_tokens, usage.output_tokens)
        .cached_read_tokens(usage.cached_read_tokens)
        .cached_write_tokens(usage.cached_write_tokens)
}

pub(super) fn outbound_error(error: AcpOutboundSendError) -> agent_client_protocol::Error {
    match error {
        AcpOutboundSendError::Transport(error) => error,
        AcpOutboundSendError::UpdateTooLarge => agent_client_protocol::Error::internal_error()
            .data("A Maple ACP history update exceeded the transport limit"),
        AcpOutboundSendError::Cancelled => agent_client_protocol::Error::internal_error()
            .data("The Maple ACP connection closed while replaying history"),
    }
}

pub(super) fn prompt_text(blocks: &[ContentBlock]) -> Result<String, agent_client_protocol::Error> {
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(text) => parts.push(text.text.clone()),
            ContentBlock::ResourceLink(link) => {
                parts.push(format!("[Resource: {}]\n{}", link.name, link.uri));
            }
            // Image blocks carry no text; `prompt_images` collects them.
            ContentBlock::Image(_) => {}
            _ => {
                return Err(agent_client_protocol::Error::invalid_params()
                    .data("Maple ACP accepts text, image, and resource-link prompt blocks"));
            }
        }
    }
    let text = parts.join("\n\n");
    if text.trim().is_empty()
        && !blocks
            .iter()
            .any(|block| matches!(block, ContentBlock::Image(_)))
    {
        return Err(agent_client_protocol::Error::invalid_params()
            .data("Maple ACP requires at least one text or image prompt block"));
    }
    Ok(text)
}

/// The image blocks of a prompt, in order, as uploadable attachments.
pub(super) fn prompt_images(blocks: &[ContentBlock]) -> Vec<AgentImageUpload> {
    blocks
        .iter()
        .enumerate()
        .filter_map(|(index, block)| {
            let ContentBlock::Image(image) = block else {
                return None;
            };
            Some(AgentImageUpload {
                name: format!(
                    "acp-image-{index}.{}",
                    image_file_extension(&image.mime_type)
                ),
                data_url: format!("data:{};base64,{}", image.mime_type, image.data),
            })
        })
        .collect()
}

fn image_file_extension(mime_type: &str) -> &'static str {
    match mime_type.to_ascii_lowercase().as_str() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        _ => "img",
    }
}

pub(super) fn client_supports_form_elicitation(request: &InitializeRequest) -> bool {
    request
        .client_capabilities
        .elicitation
        .as_ref()
        .is_some_and(|capabilities| capabilities.form.is_some())
}

pub(super) fn project_trust_elicitation_request(
    session_id: SessionId,
    project_root: &Path,
) -> CreateElicitationRequest {
    let schema = ElicitationSchema::new()
        .title("Trust this project?")
        .description(
            "Choose whether Maple may use guidance supplied by this project. The decision is remembered and reversible.",
        )
        .property(
            "trustProject",
            BooleanPropertySchema::new()
                .title("Trust this project")
                .description(
                    "Allow Maple to use project-provided guidance, including agent skills. These instructions can influence how agents work and use tools.",
                )
                .default_value(false),
            true,
        );
    CreateElicitationRequest::new(
        ElicitationFormMode::new(ElicitationSessionScope::new(session_id), schema),
        format!(
            "Trust project '{}'? Maple runs every tool call without asking.",
            project_root.display()
        ),
    )
}

pub(super) fn project_trust_permission_options() -> Vec<PermissionOption> {
    vec![
        // Keep the fail-closed choice first for clients that present a default.
        // Two AllowOnce options deliberately make this a chooser in Paseo, so
        // its generic auto-accept feature cannot silently resolve project trust.
        PermissionOption::new(
            "keep_untrusted",
            "Keep project trust disabled",
            PermissionOptionKind::AllowOnce,
        ),
        PermissionOption::new(
            "trust_project",
            "Trust this project",
            PermissionOptionKind::AllowOnce,
        ),
        PermissionOption::new("cancel", "Cancel turn", PermissionOptionKind::RejectOnce),
    ]
}

pub(super) fn project_trust_permission_decision(
    outcome: &RequestPermissionOutcome,
) -> Result<Option<bool>, String> {
    match outcome {
        RequestPermissionOutcome::Selected(selected)
            if selected.option_id.0.as_ref() == "keep_untrusted" =>
        {
            Ok(Some(false))
        }
        RequestPermissionOutcome::Selected(selected)
            if selected.option_id.0.as_ref() == "trust_project" =>
        {
            Ok(Some(true))
        }
        RequestPermissionOutcome::Cancelled => Ok(None),
        RequestPermissionOutcome::Selected(selected)
            if selected.option_id.0.as_ref() == "cancel" =>
        {
            Ok(None)
        }
        _ => Err("ACP client selected an unknown Maple project trust option".to_string()),
    }
}

pub(super) const COMPACTION_COMPLETED_NOTICE: &str = "Compaction completed.\n";

pub(super) fn timeline_update(
    item: &AgentTimelineItem,
    tools: &mut AcpToolProjection,
    include_user_messages: bool,
) -> Option<SessionUpdate> {
    match item.item_type.as_str() {
        "message" if include_user_messages && item.role.as_deref() == Some("user") => {
            item.text.as_ref().map(|text| {
                SessionUpdate::UserMessageChunk(
                    ContentChunk::new(ContentBlock::Text(TextContent::new(text.clone())))
                        .message_id(item.id.as_str()),
                )
            })
        }
        "message" if item.role.as_deref() == Some("assistant") => item.text.as_ref().map(|text| {
            SessionUpdate::AgentMessageChunk(
                ContentChunk::new(ContentBlock::Text(TextContent::new(text.clone())))
                    .message_id(item.id.as_str()),
            )
        }),
        "thinking" => item.text.as_ref().map(|text| {
            SessionUpdate::AgentThoughtChunk(
                ContentChunk::new(ContentBlock::Text(TextContent::new(text.clone())))
                    .message_id(item.id.as_str()),
            )
        }),
        "tool" => Some(acp_tool_update(item, tools)),
        // Compaction is a full model round-trip with no other output. The
        // reference adapters (claude-agent-acp, codex-acp) tell the client
        // with a short agent-text notice, so clients render it the same way.
        "system" => item
            .text
            .as_deref()
            .and_then(compaction_notice_text)
            .map(|notice| {
                SessionUpdate::AgentMessageChunk(
                    ContentChunk::new(ContentBlock::Text(TextContent::new(format!("{notice}\n"))))
                        .message_id(item.id.as_str()),
                )
            }),
        _ => None,
    }
}

pub(super) fn acp_tool_update(
    item: &AgentTimelineItem,
    tools: &mut AcpToolProjection,
) -> SessionUpdate {
    let status = match item.status.as_deref() {
        Some("completed") => ToolCallStatus::Completed,
        Some("failed" | "cancelled") => ToolCallStatus::Failed,
        Some("pending") => ToolCallStatus::Pending,
        _ => ToolCallStatus::InProgress,
    };
    let kind = timeline_tool_kind(item);
    let content = timeline_tool_text(item).map(|text| {
        vec![ToolCallContent::from(ContentBlock::Text(TextContent::new(
            text,
        )))]
    });
    let locations = timeline_tool_locations(item);
    let raw_input = item.input.as_ref().map(bounded_raw_json);
    let raw_output = timeline_tool_raw_output(item);
    let title = item.title.clone();
    if tools.seen.insert(item.id.clone()) {
        let mut call = ToolCall::new(
            item.id.clone(),
            title.unwrap_or_else(|| "Maple tool".to_string()),
        )
        .kind(kind)
        .status(status);
        if let Some(content) = content {
            call = call.content(content);
        }
        if !locations.is_empty() {
            call = call.locations(locations);
        }
        if let Some(raw_input) = raw_input {
            call = call.raw_input(raw_input);
        }
        if let Some(raw_output) = raw_output {
            call = call.raw_output(raw_output);
        }
        SessionUpdate::ToolCall(call)
    } else {
        // A response carries no title of its own for a provider-made
        // call ID. Leaving the field out keeps the request's title, which
        // says what ran; a placeholder would overwrite it.
        let mut fields = ToolCallUpdateFields::new().kind(kind).status(status);
        if let Some(title) = title {
            fields = fields.title(title);
        }
        if let Some(content) = content {
            fields = fields.content(content);
        }
        if !locations.is_empty() {
            fields = fields.locations(locations);
        }
        if let Some(raw_input) = raw_input {
            fields = fields.raw_input(raw_input);
        }
        if let Some(raw_output) = raw_output {
            fields = fields.raw_output(raw_output);
        }
        SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(item.id.clone(), fields))
    }
}

pub(super) fn timeline_tool_kind(item: &AgentTimelineItem) -> ToolKind {
    let title = item
        .title
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let input = item.input.as_ref();
    if input.and_then(|value| value.get("command")).is_some() || title.contains("terminal") {
        ToolKind::Execute
    } else if input.and_then(|value| value.get("url")).is_some()
        || title.contains("web")
        || title.contains("url")
    {
        ToolKind::Fetch
    } else if input.and_then(|value| value.get("query")).is_some()
        || input.and_then(|value| value.get("pattern")).is_some()
        || title.contains("search")
        || title.contains("find")
    {
        ToolKind::Search
    } else if title.contains("read") {
        ToolKind::Read
    } else if input.and_then(tool_path).is_some()
        || title.contains("edit")
        || title.contains("write")
    {
        ToolKind::Edit
    } else {
        ToolKind::Other
    }
}

pub(super) fn timeline_tool_text(item: &AgentTimelineItem) -> Option<String> {
    let output_text = || {
        item.output
            .as_ref()
            .and_then(|output| output.get("text"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let text = if matches!(
        item.status.as_deref(),
        Some("completed" | "failed" | "cancelled")
    ) {
        // Coalesced replay items retain the original request summary in
        // item.text while the terminal tool result lives in output.text.
        // Paseo renders ACP text content before rawOutput, so preferring the
        // summary here would hide the actual imported result.
        output_text().or_else(|| item.text.clone())
    } else {
        item.text.clone().or_else(output_text)
    };
    text.map(|text| bounded_chars(&text, MAX_ACP_TOOL_TEXT_CHARS))
}

pub(super) fn tool_path(value: &serde_json::Value) -> Option<&str> {
    ["path", "file_path", "file"]
        .into_iter()
        .find_map(|key| value.get(key).and_then(serde_json::Value::as_str))
}

pub(super) fn timeline_tool_locations(item: &AgentTimelineItem) -> Vec<ToolCallLocation> {
    item.input
        .as_ref()
        .and_then(tool_path)
        .filter(|path| Path::new(path).is_absolute())
        .map(|path| vec![ToolCallLocation::new(PathBuf::from(path))])
        .unwrap_or_default()
}

pub(super) fn timeline_tool_raw_output(item: &AgentTimelineItem) -> Option<serde_json::Value> {
    let output = item.output.as_ref()?;
    let failure_message = matches!(item.status.as_deref(), Some("failed" | "cancelled"))
        .then(|| {
            output
                .get("text")
                .and_then(serde_json::Value::as_str)
                .map(|text| bounded_chars(text, MAX_ACP_ERROR_CHARS))
        })
        .flatten()
        .filter(|message| !message.trim().is_empty());
    let mut bounded = bounded_raw_json(output);
    if let (Some(message), serde_json::Value::Object(fields)) = (failure_message, &mut bounded) {
        // Paseo derives the failure badge from rawOutput.message/error. Maple's
        // persisted tool shape uses output.text, so provide a bounded alias
        // without changing the canonical result or exposing any extra data.
        if !fields.contains_key("message") && !fields.contains_key("error") {
            fields.insert("message".to_string(), serde_json::Value::String(message));
        }
    }
    Some(bounded)
}

pub(super) fn bounded_raw_json(value: &serde_json::Value) -> serde_json::Value {
    match serde_json::to_vec(value) {
        Ok(encoded) if encoded.len() <= 64 * 1024 => value.clone(),
        Ok(encoded) => serde_json::json!({
            "truncated": true,
            "encodedBytes": encoded.len(),
        }),
        Err(_) => serde_json::json!({ "unavailable": true }),
    }
}

pub(super) fn event_error_text(item: &AgentTimelineItem) -> Option<String> {
    item.text
        .clone()
        .map(|message| bounded_chars(&message, MAX_ACP_ERROR_CHARS))
}
pub(super) fn prompt_result_from_terminal(
    terminal: AgentRunTerminal,
) -> Result<PromptResponse, agent_client_protocol::Error> {
    match terminal {
        AgentRunTerminal::Completed => Ok(PromptResponse::new(StopReason::EndTurn)),
        AgentRunTerminal::Cancelled => Ok(PromptResponse::new(StopReason::Cancelled)),
        // A failed terminal is emitted only after a run was admitted. Goose may
        // already have persisted output or executed tools, and Buzz treats a
        // JSON-RPC AgentError as pre-mutation/retryable. The preceding error
        // update carries the failure text; settle the turn successfully here so
        // non-idempotent work is never replayed automatically.
        AgentRunTerminal::Failed => Ok(PromptResponse::new(StopReason::EndTurn)),
    }
}

pub(super) fn internal_acp_error(error: String) -> agent_client_protocol::Error {
    agent_client_protocol::Error::internal_error().data(bounded_chars(&error, MAX_ACP_ERROR_CHARS))
}

/// Bound untrusted text to `max_chars` before it crosses the ACP wire.
pub(super) fn bounded_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}
