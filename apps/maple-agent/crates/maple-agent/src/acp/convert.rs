//! Conversions between Maple's agent types and the ACP wire shapes: config
//! options, the project-trust prompts, and timeline items rendered as session
//! updates.

use super::transport::AcpOutboundSendError;
use crate::agent::{
    AgentImageUpload, AgentRunTerminal, AgentRunUsage, AgentSlashCommand, AgentTimelineItem,
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
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub(super) const MAX_ACP_ERROR_CHARS: usize = 500;
pub(super) const MAX_ACP_TOOL_TEXT_CHARS: usize = 16_000;

/// What a session's caller has been sent of the rows of one stream: the
/// tool calls it knows, and the text of each streamed message so far.
#[derive(Default)]
pub(super) struct AcpProjection {
    tools: HashSet<String>,
    revisions: HashMap<String, u64>,
    pub(super) snapshot_revision: u64,
    streamed: HashMap<String, String>,
    /// Logical retained string bytes in keys, aliases and streamed prefixes.
    /// Map entry counts separately bound fixed allocator/container overhead.
    retained_string_bytes: usize,
    live_wire_ids: HashMap<crate::agent::AgentLiveRowIdentity, String>,
    persisted_aliases: HashMap<String, String>,
    /// Canonical rows already emitted by the immutable initial replay. This
    /// compact boundary lets a later completion update an old tool call without
    /// retaining a set of every historical tool ID in application memory.
    replayed_history: Option<(String, i64, i64)>,
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

pub(super) fn add_thinking_option(
    options: &mut Vec<SessionConfigOption>,
    state: &crate::agent::AgentThinkingState,
) {
    if state.levels.len() < 2 {
        return;
    }
    options.push(
        SessionConfigOption::select(
            "thought_level",
            "Thinking",
            state.current.as_str(),
            state
                .levels
                .iter()
                .map(|level| SessionConfigSelectOption::new(level.as_str(), level.as_str()))
                .collect::<Vec<_>>(),
        )
        .category(SessionConfigOptionCategory::ThoughtLevel),
    );
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

/// Agent text that tells the caller a compaction began, as the reference
/// adapters (claude-agent-acp, codex-acp) do: compaction is a full model
/// round-trip with no other output.
pub(super) const COMPACTING_NOTICE: &str = "Compacting…\n";
pub(super) const COMPACTION_COMPLETED_NOTICE: &str = "Compaction completed.\n";
pub(super) const NOTHING_TO_COMPACT_NOTICE: &str = "Nothing to compact yet.\n";
/// The title of the row a stored compaction shows as.
const COMPACTED_ROW_TITLE: &str = "Context compacted";

/// The update a timeline row makes for the caller, if any. A live run's
/// text and thinking stream as `append` rows, each sent as it comes; the
/// `replace` row that settles a message sends only what did not stream.
/// A `replay` of a stored task also sends the user's messages and the
/// failures; a live run's failure is sent once the run ends, since Pi may
/// still retry it.
pub(super) fn timeline_update(
    item: &AgentTimelineItem,
    projection: &mut AcpProjection,
    replay: bool,
) -> Option<SessionUpdate> {
    let mut translated = std::borrow::Cow::Borrowed(item);
    let persisted_id = item
        .persisted
        .as_ref()
        .map(|row| super::paged::replay_row_id(&row.task_id, row.rows_epoch, row.row_seq));
    let identity = if let Some(live) = &item.live {
        let identity = if let Some(known) = projection.live_wire_ids.get(live) {
            known.clone()
        } else {
            // A different interface can attach its own live identity to an
            // already persisted occurrence. Keep the wire ID first advertised
            // for that occurrence rather than announcing another tool/message.
            let identity = persisted_id
                .as_ref()
                .and_then(|persisted| projection.persisted_aliases.get(persisted))
                .cloned()
                .or_else(|| persisted_id.clone())
                .unwrap_or_else(|| format!("maple-live:{}:{}", live.run_id, live.occurrence));
            projection.add_retained_bytes(live.run_id.len().saturating_add(identity.len()));
            projection
                .live_wire_ids
                .insert(live.clone(), identity.clone());
            identity
        };
        if let Some(persisted) = persisted_id {
            let key_bytes = persisted.len();
            let previous = projection
                .persisted_aliases
                .insert(persisted, identity.clone());
            match previous {
                Some(previous) => projection.replace_retained_bytes(previous.len(), identity.len()),
                None => projection.add_retained_bytes(key_bytes.saturating_add(identity.len())),
            }
        }
        Some(identity)
    } else {
        persisted_id.map(|persisted| {
            projection
                .persisted_aliases
                .get(&persisted)
                .cloned()
                .unwrap_or(persisted)
        })
    };
    if let Some(identity) = identity {
        translated.to_mut().id = identity;
    }
    let item = translated.as_ref();
    if item.revision > 0 {
        if !projection.revisions.contains_key(&item.id) {
            projection.add_retained_bytes(item.id.len());
        }
        let previous = projection.revisions.entry(item.id.clone()).or_default();
        if item.revision <= *previous {
            return None;
        }
        *previous = item.revision;
    }
    match item.item_type.as_str() {
        "message" if item.role.as_deref() == Some("user") => {
            let text = item.text.as_ref().filter(|_| replay)?;
            let _ = text;
            projection
                .unsent_text(item)
                .map(|text| SessionUpdate::UserMessageChunk(text_chunk(text, &item.id)))
        }
        "message" if item.role.as_deref() == Some("assistant") => projection
            .unsent_text(item)
            .map(|text| SessionUpdate::AgentMessageChunk(text_chunk(text, &item.id))),
        "thinking" => projection
            .unsent_text(item)
            .map(|text| SessionUpdate::AgentThoughtChunk(text_chunk(text, &item.id))),
        "tool" => Some(acp_tool_update(item, projection)),
        "error" if replay => event_error_text(item)
            .map(|text| SessionUpdate::AgentMessageChunk(text_chunk(text, &item.id))),
        "system" if item.title.as_deref() == Some(COMPACTED_ROW_TITLE) => {
            Some(SessionUpdate::AgentMessageChunk(text_chunk(
                COMPACTION_COMPLETED_NOTICE.to_string(),
                &item.id,
            )))
        }
        // Other runtime notices stay out of the ACP stream.
        _ => None,
    }
}

const MAX_PROJECTION_ENTRIES: usize = 4096;
const MAX_PROJECTION_STRING_BYTES: usize = 32 * 1024 * 1024;
pub(super) const ACP_PROJECTION_LIMIT_MESSAGE: &str = "This ACP session reached its live projection limit. Reload this session to continue viewing its task; saved history and shared work are unchanged.";

/// Retaining projections fail explicitly instead of forgetting a streamed
/// prefix or alias. Ordinary `timeline_update` remains available for disposable
/// one-row history reconstruction, whose independent row bound still applies.
pub(super) fn checked_timeline_update(
    item: &AgentTimelineItem,
    projection: &mut AcpProjection,
    replay: bool,
) -> Result<Option<SessionUpdate>, &'static str> {
    checked_update_with_limits(
        item,
        projection,
        replay,
        MAX_PROJECTION_ENTRIES,
        MAX_PROJECTION_STRING_BYTES,
    )
}

#[cfg(test)]
pub(super) fn checked_timeline_update_with_limits(
    item: &AgentTimelineItem,
    projection: &mut AcpProjection,
    replay: bool,
    max_entries: usize,
    max_bytes: usize,
) -> Result<Option<SessionUpdate>, &'static str> {
    checked_update_with_limits(item, projection, replay, max_entries, max_bytes)
}

fn checked_update_with_limits(
    item: &AgentTimelineItem,
    projection: &mut AcpProjection,
    replay: bool,
    max_entries: usize,
    max_bytes: usize,
) -> Result<Option<SessionUpdate>, &'static str> {
    if projection.exceeds_limits(max_entries, max_bytes) {
        return Err(ACP_PROJECTION_LIMIT_MESSAGE);
    }
    let update = timeline_update(item, projection, replay);
    // At most one already bounded input can overshoot. The caller retires the
    // operation on Err and must not enqueue the update returned internally.
    if projection.exceeds_limits(max_entries, max_bytes) {
        return Err(ACP_PROJECTION_LIMIT_MESSAGE);
    }
    Ok(update)
}

impl AcpProjection {
    pub(super) fn note_replayed_history(&mut self, task: &str, epoch: i64, through: i64) {
        let previous = self
            .replayed_history
            .as_ref()
            .map_or(0, |(task, _, _)| task.len());
        self.replace_retained_bytes(previous, task.len());
        self.replayed_history = Some((task.to_owned(), epoch, through));
    }

    fn add_retained_bytes(&mut self, added: usize) {
        self.retained_string_bytes = self.retained_string_bytes.saturating_add(added);
    }

    fn replace_retained_bytes(&mut self, previous: usize, next: usize) {
        self.retained_string_bytes = self
            .retained_string_bytes
            .saturating_sub(previous)
            .saturating_add(next);
    }

    fn exceeds_limits(&self, max_entries: usize, max_bytes: usize) -> bool {
        let entries = self
            .tools
            .len()
            .saturating_add(self.revisions.len())
            .saturating_add(self.streamed.len())
            .saturating_add(self.live_wire_ids.len())
            .saturating_add(self.persisted_aliases.len());
        entries > max_entries || self.retained_string_bytes > max_bytes
    }

    fn remember_tool(&mut self, id: &str) -> bool {
        if self.tools.insert(id.to_owned()) {
            self.add_retained_bytes(id.len());
            true
        } else {
            false
        }
    }

    fn replayed_tool(&self, item: &AgentTimelineItem) -> bool {
        let Some((task, epoch, through)) = &self.replayed_history else {
            return false;
        };
        item.persisted.as_ref().is_some_and(|row| {
            &row.task_id == task && row.rows_epoch == *epoch && row.row_seq <= *through
        })
    }

    /// The text of a message row the caller has not been sent yet.
    fn unsent_text(&mut self, item: &AgentTimelineItem) -> Option<String> {
        let text = item.text.as_deref()?;
        if !self.streamed.contains_key(&item.id) {
            self.add_retained_bytes(item.id.len());
        }
        if item.merge == "append" {
            self.add_retained_bytes(text.len());
            self.streamed
                .entry(item.id.clone())
                .or_default()
                .push_str(text);
            return Some(text.to_string()).filter(|text| !text.is_empty());
        }
        // The settled text: what streamed is its start, and the rest goes
        // now. A caller cannot take back text, so a message that settled
        // differently from its stream is left as it streamed.
        let streamed = self.streamed.entry(item.id.clone()).or_default();
        let suffix = text
            .strip_prefix(streamed.as_str())
            .filter(|rest| !rest.is_empty())
            .map(str::to_string);
        if suffix.is_some() {
            let previous = streamed.len();
            *streamed = text.to_string();
            self.replace_retained_bytes(previous, text.len());
        }
        suffix
    }
}

pub(super) fn text_chunk(text: String, message_id: &str) -> ContentChunk {
    ContentChunk::new(ContentBlock::Text(TextContent::new(text))).message_id(message_id)
}

pub(super) fn acp_tool_update(
    item: &AgentTimelineItem,
    projection: &mut AcpProjection,
) -> SessionUpdate {
    let status = match item.status.as_deref() {
        Some("completed") => ToolCallStatus::Completed,
        Some("failed" | "cancelled") => ToolCallStatus::Failed,
        Some("pending") => ToolCallStatus::Pending,
        _ => ToolCallStatus::InProgress,
    };
    // A result row names neither the tool nor its input; the kind the call
    // was given stands.
    let kind = (item.title.is_some() || item.input.is_some()).then(|| timeline_tool_kind(item));
    let content = timeline_tool_text(item).map(|text| {
        vec![ToolCallContent::from(ContentBlock::Text(TextContent::new(
            text,
        )))]
    });
    let locations = timeline_tool_locations(item);
    let raw_input = item.input.as_ref().map(bounded_raw_json);
    let raw_output = timeline_tool_raw_output(item);
    let title = item.title.clone();
    if projection.remember_tool(&item.id) && !projection.replayed_tool(item) {
        let mut call = ToolCall::new(
            item.id.clone(),
            title.unwrap_or_else(|| "Maple tool".to_string()),
        )
        .kind(kind.unwrap_or(ToolKind::Other))
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
        let mut fields = ToolCallUpdateFields::new().status(status);
        if let Some(kind) = kind {
            fields = fields.kind(kind);
        }
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
        // A failed terminal is emitted only after a run was admitted. Pi may
        // already have saved output or run tools, while a client may take a
        // JSON-RPC error to mean nothing happened and send the turn again.
        // The preceding update carries the failure text; settle the turn
        // successfully here so non-idempotent work is never replayed.
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
