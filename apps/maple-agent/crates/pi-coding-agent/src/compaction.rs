//! Compaction: summarize the older part of a branch so the context fits again.
//!
//! The cut keeps roughly `keep_recent_tokens` of the newest context. It never separates a
//! tool result from its call; when it falls inside a turn, the start of that turn is
//! summarized separately. The summary and the kept entries replace everything before
//! them in the context, while the session keeps the full history.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use pi_agent_core::AgentMessage;
use pi_ai::{
    AssistantContent, Context, Message, Model, StopReason, StreamFn, StreamOptions, Usage,
    UserMessage, content_text, is_retryable_error, retry_delay_ms,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::messages::SessionMessage;
use crate::session::{
    ContextEdits, EntryKind, ProjectedEntry, SessionEntry, SessionProjection, project,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CompactionSettings {
    pub enabled: bool,
    /// Room kept free for the response and the summary request.
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
    /// Token settings for particular models, by `provider/model-id`, as Pi's
    /// `modelOverrides`; see [`CompactionSettings::for_model`].
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub model_overrides: BTreeMap<String, CompactionModelOverride>,
    /// Tools whose `path` argument a model reads, for the file lists kept in summaries.
    pub read_tools: Vec<String>,
    /// Tools whose `path` argument a model changes.
    pub write_tools: Vec<String>,
}

/// One model's own compaction token settings; what it leaves out follows the others.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CompactionModelOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reserve_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keep_recent_tokens: Option<u64>,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            reserve_tokens: 16_384,
            keep_recent_tokens: 20_000,
            model_overrides: BTreeMap::new(),
            read_tools: vec!["read".into()],
            write_tools: vec!["write".into(), "edit".into()],
        }
    }
}

impl CompactionSettings {
    /// The settings for `model`: its own token settings over the others, as Pi's
    /// `getCompactionSettings` resolves them.
    pub fn for_model(&self, model: Option<&Model>) -> CompactionSettings {
        let mut settings = self.clone();
        let entry = model.and_then(|model| {
            self.model_overrides
                .get(&format!("{}/{}", model.provider, model.id))
        });
        if let Some(entry) = entry {
            settings.reserve_tokens = entry.reserve_tokens.unwrap_or(self.reserve_tokens);
            settings.keep_recent_tokens =
                entry.keep_recent_tokens.unwrap_or(self.keep_recent_tokens);
        }
        settings
    }
}

pub fn should_compact(
    context_tokens: u64,
    context_window: u64,
    settings: &CompactionSettings,
) -> bool {
    settings.enabled
        && context_window > 0
        && context_tokens > context_window.saturating_sub(settings.reserve_tokens)
}

/// A message's size as Pi's compaction estimates it: its characters over four, which
/// overestimates. Request fitting in `pi-ai` counts its own way.
pub fn estimate_tokens(message: &SessionMessage) -> u64 {
    let chars = match message {
        SessionMessage::Llm(Message::System(system)) => {
            let sections: usize = system
                .sections
                .values()
                .flatten()
                .map(|text| text.chars().count())
                .sum();
            let tools = if system.tools_added.is_empty() {
                0
            } else {
                serde_json::to_string(&system.tools_added).map_or(0, |json| json.chars().count())
            };
            system.content.chars().count() + sections + tools
        }
        SessionMessage::Llm(Message::User(user)) => content_chars(&user.content),
        SessionMessage::Llm(Message::ToolResult(result)) => content_chars(&result.content),
        SessionMessage::Custom(custom) => content_chars(&custom.content),
        SessionMessage::Llm(Message::Assistant(assistant)) => assistant
            .content
            .iter()
            .map(|block| match block {
                AssistantContent::Text(text) => text.text.chars().count(),
                AssistantContent::Thinking(thinking) => thinking.thinking.chars().count(),
                AssistantContent::ToolCall(call) => {
                    call.name.chars().count()
                        + serde_json::to_string(&call.arguments)
                            .map_or(0, |json| json.chars().count())
                }
            })
            .sum(),
        SessionMessage::BashExecution(bash) => {
            bash.command.chars().count() + bash.output.chars().count()
        }
        SessionMessage::CompactionSummary(summary) => summary.summary.chars().count(),
        SessionMessage::BranchSummary(summary) => summary.summary.chars().count(),
    };
    chars.div_ceil(4) as u64
}

/// Text characters, and 4,800 for each image.
fn content_chars(content: &[pi_ai::Content]) -> usize {
    content
        .iter()
        .map(|block| match block {
            pi_ai::Content::Text(text) => text.text.chars().count(),
            pi_ai::Content::Image(_) => 4_800,
        })
        .sum()
}

/// The context size: the last reported usage plus estimates for what came after it.
pub fn estimate_context_tokens(messages: &[SessionMessage]) -> u64 {
    // Usage reported before the latest compaction measured the context it replaced.
    let compacted_at = messages.iter().rev().find_map(|message| match message {
        SessionMessage::CompactionSummary(summary) => Some(summary.timestamp),
        _ => None,
    });
    let last_usage = messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| match message {
            SessionMessage::Llm(Message::Assistant(assistant))
                if !assistant.is_failure()
                    && assistant.usage.context_tokens() > 0
                    && compacted_at.is_none_or(|at| assistant.timestamp > at) =>
            {
                Some((index, assistant.usage.context_tokens()))
            }
            _ => None,
        });
    match last_usage {
        Some((index, tokens)) => {
            tokens
                + messages[index + 1..]
                    .iter()
                    .map(estimate_tokens)
                    .sum::<u64>()
        }
        None => messages.iter().map(estimate_tokens).sum(),
    }
}

/// The context size of a branch's projection: the last reported usage plus estimates for
/// what came after it, unless a context edit or compaction on `branch` came after that
/// usage, so that it measured a context that is gone. Then the projection is estimated
/// instead, its prompt and tools once.
pub fn estimate_projected_context_tokens(
    projection: &SessionProjection,
    branch: &[&SessionEntry],
) -> u64 {
    let messages = &projection.messages;
    let last_usage = messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| match message {
            SessionMessage::Llm(Message::Assistant(assistant))
                if !assistant.is_failure() && assistant.usage.context_tokens() > 0 =>
            {
                Some((index, assistant.usage.context_tokens()))
            }
            _ => None,
        });
    if let Some((index, tokens)) = last_usage {
        let mut end = 0;
        let source = projection.entries.iter().find(|entry| {
            end += entry.messages.len();
            index < end
        });
        let measured_at =
            source.and_then(|source| branch.iter().position(|entry| entry.id == source.entry_id));
        let changed_at = branch.iter().rposition(|entry| {
            matches!(
                entry.kind,
                EntryKind::ContextEdit { .. } | EntryKind::Compaction { .. }
            )
        });
        if measured_at > changed_at {
            return tokens
                + messages[index + 1..]
                    .iter()
                    .map(estimate_tokens)
                    .sum::<u64>();
        }
    }
    let prompt = pi_ai::transcript::current_system_message(
        messages.iter().filter_map(AgentMessage::as_message),
    )
    .map_or(0, |system| {
        estimate_tokens(&SessionMessage::Llm(Message::System(system)))
    });
    prompt
        + messages
            .iter()
            .filter(|message| !matches!(message.as_message(), Some(Message::System(_))))
            .map(estimate_tokens)
            .sum::<u64>()
}

fn is_cut_point(message: &SessionMessage) -> bool {
    !matches!(
        message,
        SessionMessage::Llm(Message::ToolResult(_) | Message::System(_))
    )
}

fn is_turn_start(message: &SessionMessage) -> bool {
    !matches!(
        message,
        SessionMessage::Llm(Message::ToolResult(_) | Message::System(_) | Message::Assistant(_))
    )
}

struct CutPoint {
    first_kept: usize,
    /// The start of the turn the cut falls in, when it does not fall on a turn start.
    turn_start: Option<usize>,
}

fn find_cut_point(
    entries: &[ProjectedEntry],
    start: usize,
    end: usize,
    keep_recent_tokens: u64,
) -> CutPoint {
    let visible = |index: usize, test: fn(&SessionMessage) -> bool| {
        !entries[index].is_compaction && entries[index].messages.iter().any(test)
    };
    let cut_points: Vec<usize> = (start..end)
        .filter(|index| visible(*index, is_cut_point))
        .collect();
    let Some(&first) = cut_points.first() else {
        return CutPoint {
            first_kept: start,
            turn_start: None,
        };
    };
    let mut cut = first;
    let mut accumulated = 0;
    for index in (start..end).rev() {
        let tokens: u64 = entries[index].messages.iter().map(estimate_tokens).sum();
        if tokens == 0 {
            continue;
        }
        accumulated += tokens;
        if accumulated >= keep_recent_tokens {
            // The nearest cut point at or after this entry; past the last one, the last.
            cut = cut_points
                .iter()
                .copied()
                .find(|candidate| *candidate >= index)
                .unwrap_or(cut_points[cut_points.len() - 1]);
            break;
        }
    }
    // Whether the cut splits a turn is decided before moving it onto state-only entries,
    // which start no turn of their own.
    let splits_turn = !visible(cut, is_turn_start);
    // Keep state-only entries right before the cut with what follows them.
    while cut > start && !entries[cut - 1].is_compaction && entries[cut - 1].messages.is_empty() {
        cut -= 1;
    }
    let turn_start = if splits_turn {
        (start..=cut)
            .rev()
            .find(|index| visible(*index, is_turn_start))
    } else {
        None
    };
    CutPoint {
        first_kept: cut,
        turn_start,
    }
}

/// Files a summarized part of the conversation read or changed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FileOperations {
    pub read: BTreeSet<String>,
    pub modified: BTreeSet<String>,
}

impl FileOperations {
    fn record(&mut self, message: &SessionMessage, settings: &CompactionSettings) {
        let SessionMessage::Llm(Message::Assistant(assistant)) = message else {
            return;
        };
        for call in assistant.tool_calls() {
            let Some(path) = call.arguments.get("path").and_then(Value::as_str) else {
                continue;
            };
            if settings.read_tools.contains(&call.name) {
                self.read.insert(path.to_string());
            } else if settings.write_tools.contains(&call.name) {
                self.modified.insert(path.to_string());
            }
        }
    }

    /// Add the file lists an earlier summary kept in its details.
    fn add_details(&mut self, details: &Value) {
        for (key, set) in [
            ("readFiles", &mut self.read),
            ("modifiedFiles", &mut self.modified),
        ] {
            set.extend(
                details[key]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_string),
            );
        }
    }

    /// Files only read, and files changed.
    pub fn lists(&self) -> (Vec<String>, Vec<String>) {
        let read = self.read.difference(&self.modified).cloned().collect();
        (read, self.modified.iter().cloned().collect())
    }
}

fn format_file_operations(read: &[String], modified: &[String]) -> String {
    let mut sections = Vec::new();
    if !read.is_empty() {
        sections.push(format!("<read-files>\n{}\n</read-files>", read.join("\n")));
    }
    if !modified.is_empty() {
        sections.push(format!(
            "<modified-files>\n{}\n</modified-files>",
            modified.join("\n")
        ));
    }
    if sections.is_empty() {
        String::new()
    } else {
        format!("\n\n{}", sections.join("\n\n"))
    }
}

/// What a compaction would summarize and keep.
#[derive(Clone, Debug, PartialEq)]
pub struct CompactionPreparation {
    pub first_kept_entry_id: String,
    pub messages_to_summarize: Vec<SessionMessage>,
    /// The start of a turn the cut falls in, summarized on its own.
    pub turn_prefix_messages: Vec<SessionMessage>,
    pub tokens_before: u64,
    pub previous_summary: Option<String>,
    pub file_operations: FileOperations,
    pub settings: CompactionSettings,
}

/// Decide what to compact on a branch. `None` when there is nothing to summarize.
pub fn prepare_compaction(
    path: &[&SessionEntry],
    settings: &CompactionSettings,
) -> Option<CompactionPreparation> {
    if path
        .last()
        .is_some_and(|entry| matches!(entry.kind, EntryKind::Compaction { .. }))
    {
        return None;
    }
    let projection = project(path);
    let entries = &projection.entries;
    // The newest compaction leads the projection when there is one.
    let previous = entries
        .first()
        .filter(|entry| entry.is_compaction && !entry.messages.is_empty())
        .and_then(|entry| path.iter().find(|candidate| candidate.id == entry.entry_id));
    let previous_summary = previous.and_then(|entry| match &entry.kind {
        EntryKind::Compaction { summary, .. } => Some(summary.clone()),
        _ => None,
    });
    let start = usize::from(previous.is_some());
    let cut = find_cut_point(entries, start, entries.len(), settings.keep_recent_tokens);
    let first_kept_entry_id = entries.get(cut.first_kept)?.entry_id.clone();
    let history_end = cut.turn_start.unwrap_or(cut.first_kept);
    let without_system = |range: &[ProjectedEntry]| -> Vec<SessionMessage> {
        range
            .iter()
            .flat_map(|entry| entry.messages.iter())
            .filter(|message| !matches!(message.as_message(), Some(Message::System(_))))
            .cloned()
            .collect()
    };
    let messages_to_summarize = without_system(&entries[start..history_end]);
    let turn_prefix_messages = match cut.turn_start {
        Some(turn_start) => without_system(&entries[turn_start..cut.first_kept]),
        None => Vec::new(),
    };
    if messages_to_summarize.is_empty() && turn_prefix_messages.is_empty() {
        return None;
    }
    let mut file_operations = FileOperations::default();
    // An extension's details are its own, whatever they hold.
    if let Some(EntryKind::Compaction {
        details: Some(details),
        from_extension: false,
        ..
    }) = previous.map(|entry| &entry.kind)
    {
        file_operations.add_details(details);
    }
    for message in messages_to_summarize.iter().chain(&turn_prefix_messages) {
        file_operations.record(message, settings);
    }
    Some(CompactionPreparation {
        first_kept_entry_id,
        messages_to_summarize,
        turn_prefix_messages,
        tokens_before: estimate_projected_context_tokens(&projection, path),
        previous_summary,
        file_operations,
        settings: settings.clone(),
    })
}

/// A compaction to record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionResult {
    pub summary: String,
    pub first_kept_entry_id: String,
    pub tokens_before: u64,
    /// The estimated context size once the compaction is recorded. The session fills
    /// it in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_tokens_after: Option<u64>,
    #[serde(default)]
    pub usage: Usage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

/// The model calls that write summaries.
pub struct Summarizer<'a> {
    pub model: &'a Model,
    pub stream_fn: &'a dyn StreamFn,
    /// Credentials, routing and cancellation for the requests, and the thinking level to
    /// summarize at, which goes only to a model that reasons.
    pub options: StreamOptions,
    /// Retries for transient failures: none when 0. Each delay doubles the first, up to
    /// the longest.
    pub max_retries: u32,
    pub retry_base_ms: u64,
    pub retry_max_ms: u64,
    /// Told of each retry, as Pi's summarization retry callbacks are.
    pub on_retry: Option<&'a (dyn Fn(SummaryRetry) + Send + Sync)>,
}

/// A summary request's retry, as Pi's `RetryCallbacks` report it.
#[derive(Clone, Debug, PartialEq)]
pub enum SummaryRetry {
    /// A transient failure; the request goes out again after the delay.
    Scheduled {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error: String,
    },
    /// The delay is over and the request goes out again.
    AttemptStart,
    /// The retries are over, whether the request then succeeded or not.
    Finished,
}

pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

const SUMMARY_PROMPT: &str = "The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.\n\nUse this EXACT format:\n\n## Goal\n[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned by user]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Current work]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [Ordered list of what should happen next]\n\n## Critical Context\n- [Any data, examples, or references needed to continue]\n- [Or \"(none)\" if not applicable]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

const UPDATE_PROMPT: &str = "The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.\n\nUpdate the existing structured summary with new information. RULES:\n- PRESERVE all existing information from the previous summary\n- ADD new progress, decisions, and context from the new messages\n- UPDATE the Progress section: move items from \"In Progress\" to \"Done\" when completed\n- UPDATE \"Next Steps\" based on what was accomplished\n- PRESERVE exact file paths, function names, and error messages\n- If something is no longer relevant, you may remove it\n\nUse this EXACT format:\n\n## Goal\n[Preserve existing goals, add new ones if the task expanded]\n\n## Constraints & Preferences\n- [Preserve existing, add new ones discovered]\n\n## Progress\n### Done\n- [x] [Include previously done items AND newly completed items]\n\n### In Progress\n- [ ] [Current work - update based on progress]\n\n### Blocked\n- [Current blockers - remove if resolved]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale] (preserve all previous, add new)\n\n## Next Steps\n1. [Update based on current state]\n\n## Critical Context\n- [Preserve important context, add new if needed]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

const TURN_PREFIX_PROMPT: &str = "The messages above are earlier context from an ongoing conversation. Later messages are stored separately and do not need to be reconstructed.\n\nCreate a concise checkpoint of the user's request and the progress shown above. This checkpoint will be placed before the later messages so the conversation can continue with the necessary context.\n\n## Original Request\n[What did the user ask for?]\n\n## Progress So Far\n- [Key decisions and work completed in these messages]\n\n## Context Needed to Continue\n- [Information from these messages needed to understand the later work]\n\nOnly summarize information explicitly present above. Do not infer or recreate later messages.";

const BRANCH_PROMPT: &str = "Create a structured summary of this conversation branch for context when returning later.\n\nUse this EXACT format:\n\n## Goal\n[What was the user trying to accomplish in this branch?]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Work that was started but not finished]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [What should happen next to continue this work]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

/// Leads a branch summary, so the model knows where it comes from.
const BRANCH_SUMMARY_PREAMBLE: &str = "The user explored a different conversation branch before returning here.\nSummary of that exploration:\n\n";

const TOOL_RESULT_MAX_CHARS: usize = 2_000;

/// Render messages as labelled text, so the summarizer reads them instead of continuing them.
///
/// Thinking is left out, as in Pi. Replayed thinking is often not part of the model's
/// live context (another model's is dropped, for one), and with it the request could be
/// larger than the context it summarizes. Text, tool calls and tool results record what
/// happened.
pub fn serialize_conversation(messages: &[Message]) -> String {
    let mut parts = Vec::new();
    for message in messages {
        match message {
            Message::User(user) => {
                let text = content_text(&user.content);
                if !text.is_empty() {
                    parts.push(format!("[User]: {text}"));
                }
            }
            Message::Assistant(assistant) => {
                let calls: Vec<String> = assistant
                    .tool_calls()
                    .map(|call| {
                        let args: Vec<String> = call
                            .arguments
                            .iter()
                            .map(|(key, value)| format!("{key}={value}"))
                            .collect();
                        format!("{}({})", call.name, args.join(", "))
                    })
                    .collect();
                let text = assistant.text();
                if !text.is_empty() {
                    parts.push(format!("[Assistant]: {text}"));
                }
                if !calls.is_empty() {
                    parts.push(format!("[Assistant tool calls]: {}", calls.join("; ")));
                }
            }
            Message::ToolResult(result) => {
                let text = content_text(&result.content);
                if !text.is_empty() {
                    let chars = text.chars().count();
                    let shown = if chars > TOOL_RESULT_MAX_CHARS {
                        let kept: String = text.chars().take(TOOL_RESULT_MAX_CHARS).collect();
                        format!(
                            "{kept}\n\n[... {} more characters truncated]",
                            chars - TOOL_RESULT_MAX_CHARS
                        )
                    } else {
                        text
                    };
                    parts.push(format!("[Tool result]: {shown}"));
                }
            }
            Message::System(_) => {}
        }
    }
    parts.join("\n\n")
}

fn llm_messages(messages: &[SessionMessage]) -> Vec<Message> {
    crate::messages::convert_to_llm(messages)
}

/// Run one summary request. Transient failures are retried; a cut-off or tool-calling
/// response is an error, since a partial summary must not become a checkpoint.
async fn summarize(
    summarizer: &Summarizer<'_>,
    prompt: String,
    max_tokens: u64,
    label: &str,
) -> Result<(String, Usage), String> {
    let context = Context::new(
        SUMMARIZATION_SYSTEM_PROMPT,
        Vec::new(),
        vec![Message::User(UserMessage::text(prompt))],
    );
    let max_tokens = if summarizer.model.max_tokens > 0 {
        max_tokens.min(summarizer.model.max_tokens)
    } else {
        max_tokens
    };
    let report = |event: SummaryRetry| {
        if let Some(on_retry) = summarizer.on_retry {
            on_retry(event);
        }
    };
    let mut attempt = 0;
    loop {
        let options = StreamOptions {
            max_tokens: Some(max_tokens),
            reasoning: summarizer
                .options
                .reasoning
                .filter(|level| summarizer.model.reasoning && *level != pi_ai::ThinkingLevel::Off),
            ..summarizer.options.clone()
        };
        let response = summarizer
            .stream_fn
            .stream(summarizer.model, context.clone(), options)
            .result()
            .await;
        // As Pi's retryAssistantCall: a transient failure is tried again while the
        // budget lasts, and the end of the retries is reported however it ends.
        if is_retryable_error(&response)
            && attempt < summarizer.max_retries
            && !summarizer.options.cancel.is_cancelled()
        {
            attempt += 1;
            let delay_ms =
                retry_delay_ms(summarizer.retry_base_ms, summarizer.retry_max_ms, attempt);
            report(SummaryRetry::Scheduled {
                attempt,
                max_attempts: summarizer.max_retries,
                delay_ms,
                error: response
                    .error_message
                    .clone()
                    .unwrap_or_else(|| "Unknown error".into()),
            });
            let waited = tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => true,
                _ = summarizer.options.cancel.cancelled() => false,
            };
            if waited {
                report(SummaryRetry::AttemptStart);
                continue;
            }
        }
        if attempt > 0 {
            report(SummaryRetry::Finished);
        }
        return match response.stop_reason {
            StopReason::Error | StopReason::Aborted => Err(format!(
                "{label} failed: {}",
                response.error_message.as_deref().unwrap_or("Unknown error")
            )),
            StopReason::Length => Err(format!(
                "{label} failed: generation hit the token cap and the summary is incomplete"
            )),
            _ if response.tool_calls().next().is_some() => {
                Err(format!("{label} attempted to call a tool"))
            }
            _ => Ok((response.text(), response.usage)),
        };
    }
}

/// Summarize a prepared compaction.
pub async fn compact(
    preparation: &CompactionPreparation,
    summarizer: &Summarizer<'_>,
    custom_instructions: Option<&str>,
) -> Result<CompactionResult, String> {
    let reserve = preparation.settings.reserve_tokens;
    let focus = custom_instructions
        .map(|instructions| format!("\n\nAdditional focus: {instructions}"))
        .unwrap_or_default();
    let history = async {
        let conversation =
            serialize_conversation(&llm_messages(&preparation.messages_to_summarize));
        let mut prompt = format!("<conversation>\n{conversation}\n</conversation>\n\n");
        let instructions = match &preparation.previous_summary {
            Some(previous) => {
                prompt.push_str(&format!(
                    "<previous-summary>\n{previous}\n</previous-summary>\n\n"
                ));
                UPDATE_PROMPT
            }
            None => SUMMARY_PROMPT,
        };
        prompt.push_str(&format!("{instructions}{focus}"));
        summarize(summarizer, prompt, reserve * 4 / 5, "Summarization").await
    };
    let (mut summary, usage) = if preparation.turn_prefix_messages.is_empty() {
        history.await?
    } else {
        let (history_text, history_usage) = if preparation.messages_to_summarize.is_empty() {
            let previous = preparation.previous_summary.clone();
            (
                previous.unwrap_or_else(|| "No prior history.".into()),
                Usage::default(),
            )
        } else {
            history.await?
        };
        let conversation = serialize_conversation(&llm_messages(&preparation.turn_prefix_messages));
        let prompt =
            format!("# Conversation\n{conversation}\n\n# Instructions\n{TURN_PREFIX_PROMPT}");
        let (prefix, prefix_usage) =
            summarize(summarizer, prompt, reserve / 2, "Turn prefix summarization").await?;
        (
            format!("{history_text}\n\n---\n\n**Turn Context (split turn):**\n\n{prefix}"),
            history_usage.add(&prefix_usage),
        )
    };
    let (read, modified) = preparation.file_operations.lists();
    summary.push_str(&format_file_operations(&read, &modified));
    Ok(CompactionResult {
        summary,
        first_kept_entry_id: preparation.first_kept_entry_id.clone(),
        tokens_before: preparation.tokens_before,
        estimated_tokens_after: None,
        usage,
        details: Some(json!({ "readFiles": read, "modifiedFiles": modified })),
    })
}

/// A summary of a branch being left, with the files the branch read and changed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BranchSummaryResult {
    pub summary: String,
    pub usage: Usage,
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

/// Summarize the entries of a branch being left, as the context showed them: edited or
/// omitted content stays out, and so do tool results, which their calls describe.
/// Entries are taken newest first while they fit `token_budget` (0 for no limit); an
/// earlier summary that does not fit is still taken while most of the budget is free.
/// With nothing to summarize, the summary is empty and no request is made.
pub async fn summarize_branch(
    entries: &[&SessionEntry],
    summarizer: &Summarizer<'_>,
    settings: &CompactionSettings,
    token_budget: u64,
    custom_instructions: Option<&str>,
    replace_instructions: bool,
) -> Result<BranchSummaryResult, String> {
    let mut file_operations = FileOperations::default();
    // Earlier summaries' file lists count, also for entries that do not fit.
    for entry in entries {
        if let EntryKind::BranchSummary {
            details: Some(details),
            from_extension: false,
            ..
        } = &entry.kind
        {
            file_operations.add_details(details);
        }
    }
    let edits = ContextEdits::of(entries);
    let mut newest_first: Vec<Vec<SessionMessage>> = Vec::new();
    let mut used: u64 = 0;
    for entry in entries.iter().rev() {
        let entry_messages: Vec<SessionMessage> = edits
            .messages(entry)
            .into_iter()
            .filter(|message| {
                !matches!(
                    message.as_message(),
                    Some(Message::System(_) | Message::ToolResult(_))
                )
            })
            .collect();
        if entry_messages.is_empty() {
            continue;
        }
        for message in &entry_messages {
            file_operations.record(message, settings);
        }
        let tokens: u64 = entry_messages.iter().map(estimate_tokens).sum();
        if token_budget > 0 && used + tokens > token_budget {
            let is_summary = matches!(
                entry.kind,
                EntryKind::Compaction { .. } | EntryKind::BranchSummary { .. }
            );
            if is_summary && used.saturating_mul(10) < token_budget.saturating_mul(9) {
                newest_first.push(entry_messages);
            }
            break;
        }
        used += tokens;
        newest_first.push(entry_messages);
    }
    let messages: Vec<SessionMessage> = newest_first.into_iter().rev().flatten().collect();
    let conversation = serialize_conversation(&llm_messages(&messages));
    if conversation.is_empty() {
        return Ok(BranchSummaryResult::default());
    }
    // As in Pi, custom instructions add a focus, or replace the default ones when asked.
    let instructions = match custom_instructions {
        Some(custom) if replace_instructions => custom.to_string(),
        Some(custom) => format!("{BRANCH_PROMPT}\n\nAdditional focus: {custom}"),
        None => BRANCH_PROMPT.to_string(),
    };
    let prompt = format!("<conversation>\n{conversation}\n</conversation>\n\n{instructions}");
    let (text, usage) = summarize(summarizer, prompt, 4_096, "Branch summarization").await?;
    let (read_files, modified_files) = file_operations.lists();
    let files = format_file_operations(&read_files, &modified_files);
    Ok(BranchSummaryResult {
        summary: format!("{BRANCH_SUMMARY_PREAMBLE}{text}{files}"),
        usage,
        read_files,
        modified_files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionManager;
    use pi_ai::faux::{FauxProvider, faux_tool_call};
    use pi_ai::{AssistantMessage, Content, ToolResultMessage};

    fn assistant(content: Vec<AssistantContent>) -> SessionMessage {
        SessionMessage::Llm(Message::Assistant(AssistantMessage {
            content,
            ..AssistantMessage::empty(&FauxProvider::default_model())
        }))
    }

    fn user(text: &str) -> SessionMessage {
        SessionMessage::Llm(Message::User(UserMessage::text(text)))
    }

    fn settings(keep_recent_tokens: u64) -> CompactionSettings {
        CompactionSettings {
            keep_recent_tokens,
            ..CompactionSettings::default()
        }
    }

    /// Three turns of about 250 tokens each.
    fn three_turns() -> SessionManager {
        let mut session = SessionManager::in_memory("/work");
        for turn in 0..3 {
            session.append_message(user(&format!("question {turn} {}", "x".repeat(500))));
            session.append_message(assistant(vec![AssistantContent::text(format!(
                "answer {turn} {}",
                "y".repeat(500)
            ))]));
        }
        session
    }

    #[test]
    fn the_threshold_leaves_room_for_the_reserve() {
        let settings = CompactionSettings::default();
        assert!(!should_compact(100_000, 128_000, &settings));
        assert!(should_compact(120_000, 128_000, &settings));
        assert!(!should_compact(
            120_000,
            128_000,
            &CompactionSettings {
                enabled: false,
                ..settings
            }
        ));
    }

    #[test]
    fn context_tokens_use_the_last_usage_and_estimate_the_rest() {
        let mut reply = AssistantMessage::empty(&FauxProvider::default_model());
        reply.usage.total_tokens = 1_000;
        let messages = vec![
            user("a"),
            SessionMessage::Llm(Message::Assistant(reply)),
            user("abcdefgh"),
        ];
        assert_eq!(estimate_context_tokens(&messages), 1_002);
    }

    #[test]
    fn usage_from_before_a_compaction_is_not_the_context_size() {
        let mut kept_reply = AssistantMessage::empty(&FauxProvider::default_model());
        kept_reply.usage.total_tokens = 100_000;
        kept_reply.timestamp = 10;
        let messages = vec![
            SessionMessage::CompactionSummary(crate::messages::CompactionSummaryMessage {
                summary: "summary".into(),
                tokens_before: 100_000,
                timestamp: 20,
            }),
            SessionMessage::Llm(Message::Assistant(kept_reply)),
            user("abcdefgh"),
        ];
        assert!(estimate_context_tokens(&messages) < 1_000);
    }

    fn reply(text: &str, context_tokens: u64) -> SessionMessage {
        let mut reply = AssistantMessage::empty(&FauxProvider::default_model());
        reply.content = vec![AssistantContent::text(text)];
        reply.usage.total_tokens = context_tokens;
        SessionMessage::Llm(Message::Assistant(reply))
    }

    fn projected_tokens(session: &SessionManager) -> u64 {
        estimate_projected_context_tokens(&session.projection(), &session.branch())
    }

    #[test]
    fn usage_from_before_a_later_context_edit_is_not_trusted() {
        let mut session = SessionManager::in_memory("/work");
        let large = session.append_message(user(&"discarded input ".repeat(2_000)));
        let answer = session.append_message(reply("small answer", 10_001));
        session.append_context_edit(&large, None).unwrap();
        assert!(projected_tokens(&session) < 100);
        session.append_context_edit(&answer, None).unwrap();
        assert_eq!(projected_tokens(&session), 0);
    }

    #[test]
    fn usage_from_after_the_latest_context_edit_is_trusted() {
        let mut session = SessionManager::in_memory("/work");
        let question = session.append_message(user("original"));
        session
            .append_context_edit(&question, Some(vec![Content::text("edited")]))
            .unwrap();
        session.append_message(reply("answer", 4_100));
        session.append_message(user("next"));
        assert_eq!(projected_tokens(&session), 4_101);
    }

    #[test]
    fn usage_from_before_a_later_compaction_is_not_trusted() {
        let mut session = SessionManager::in_memory("/work");
        let question = session.append_message(user("small input"));
        session
            .append_context_edit(&question, Some(vec![Content::text("edited input")]))
            .unwrap();
        session.append_message(reply("answer", 50_001));
        session.append_compaction(
            "small summary".into(),
            Some(question),
            50_001,
            None,
            None,
            false,
        );
        assert!(projected_tokens(&session) < 100);
    }

    #[test]
    fn an_estimate_includes_the_prompt_and_its_tools() {
        let mut session = SessionManager::in_memory("/work");
        session.append_message(SessionMessage::Llm(Message::System(pi_ai::SystemMessage {
            content: "system prompt ".repeat(3_000),
            tools_added: vec![pi_ai::Tool::new(
                "example",
                "tool declaration ".repeat(100),
                json!({ "type": "object", "properties": {} }),
            )],
            ..pi_ai::SystemMessage::default()
        })));
        let question = session.append_message(user("ask"));
        session.append_message(reply("done", 11));
        session
            .append_context_edit(&question, Some(vec![Content::text("ask")]))
            .unwrap();
        assert!(projected_tokens(&session) > 10_000);
    }

    #[test]
    fn tokens_before_leaves_out_usage_a_later_edit_changed() {
        let mut session = SessionManager::in_memory("/work");
        let large = session.append_message(user(&"discarded input ".repeat(2_000)));
        session.append_message(reply("first answer", 10_001));
        session.append_message(user("second question"));
        session.append_message(reply("second answer", 20_000));
        session.append_context_edit(&large, None).unwrap();
        let preparation = prepare_compaction(&session.branch(), &settings(1)).unwrap();
        assert!(preparation.tokens_before < 100);
        assert_eq!(preparation.tokens_before, projected_tokens(&session));
    }

    #[test]
    fn a_state_change_before_a_turn_does_not_split_it() {
        let mut session = SessionManager::in_memory("/work");
        for turn in 0..3 {
            if turn == 2 {
                session.append_model_change("faux", "faux-2");
            }
            session.append_message(user(&format!("question {turn} {}", "x".repeat(500))));
            session.append_message(assistant(vec![AssistantContent::text(format!(
                "answer {turn} {}",
                "y".repeat(500)
            ))]));
        }
        let preparation = prepare_compaction(&session.branch(), &settings(250)).unwrap();
        assert!(preparation.turn_prefix_messages.is_empty());
        assert_eq!(preparation.messages_to_summarize.len(), 4);
        let kept = session.entry(&preparation.first_kept_entry_id).unwrap();
        assert!(matches!(kept.kind, EntryKind::ModelChange { .. }));
    }

    #[test]
    fn the_cut_keeps_recent_turns_and_summarizes_the_rest() {
        let session = three_turns();
        let preparation = prepare_compaction(&session.branch(), &settings(250)).unwrap();
        let summarized: Vec<String> = preparation
            .messages_to_summarize
            .iter()
            .map(|message| message.text()[..10].to_string())
            .collect();
        assert_eq!(
            summarized,
            ["question 0", "answer 0 y", "question 1", "answer 1 y"]
        );
        assert!(preparation.turn_prefix_messages.is_empty());
        let kept = session.entry(&preparation.first_kept_entry_id).unwrap();
        assert!(
            matches!(&kept.kind, EntryKind::Message { message } if message.text().starts_with("question 2"))
        );
    }

    #[test]
    fn a_cut_inside_a_turn_summarizes_its_start_separately() {
        let mut session = SessionManager::in_memory("/work");
        session.append_message(user("first"));
        session.append_message(assistant(vec![AssistantContent::text("done")]));
        session.append_message(user("big task"));
        for step in 0..3 {
            let call = faux_tool_call("read", json!({ "path": format!("file{step}.rs") }));
            let AssistantContent::ToolCall(tool_call) = &call else {
                unreachable!()
            };
            let id = tool_call.id.clone();
            session.append_message(assistant(vec![call]));
            session.append_message(SessionMessage::Llm(Message::ToolResult(
                ToolResultMessage {
                    tool_call_id: id,
                    tool_name: "read".into(),
                    content: vec![Content::text("z".repeat(800))],
                    details: None,
                    usage: None,
                    is_error: false,
                    timestamp: 0,
                },
            )));
        }
        let preparation = prepare_compaction(&session.branch(), &settings(300)).unwrap();
        assert_eq!(preparation.messages_to_summarize.len(), 2);
        assert_eq!(preparation.turn_prefix_messages[0].text(), "big task");
        assert!(preparation.file_operations.read.contains("file0.rs"));
    }

    #[test]
    fn only_its_own_earlier_compaction_passes_file_lists_on() {
        for (from_extension, carried) in [(false, true), (true, false)] {
            let mut session = three_turns();
            let leaf = session.leaf_id().unwrap().to_string();
            session.append_compaction(
                "earlier summary".into(),
                Some(leaf),
                1,
                Some(json!({ "readFiles": ["notes.md"], "modifiedFiles": ["main.rs"] })),
                None,
                from_extension,
            );
            for turn in 3..6 {
                session.append_message(user(&format!("question {turn} {}", "x".repeat(500))));
                session.append_message(assistant(vec![AssistantContent::text(format!(
                    "answer {turn} {}",
                    "y".repeat(500)
                ))]));
            }
            let preparation = prepare_compaction(&session.branch(), &settings(250)).unwrap();
            assert_eq!(
                preparation.previous_summary.as_deref(),
                Some("earlier summary")
            );
            let files = &preparation.file_operations;
            assert_eq!(files.read.contains("notes.md"), carried);
            assert_eq!(files.modified.contains("main.rs"), carried);
        }
    }

    #[test]
    fn nothing_to_compact_after_a_compaction_or_in_a_short_session() {
        let session = three_turns();
        assert!(prepare_compaction(&session.branch(), &settings(1_000_000)).is_none());
        let mut session = three_turns();
        let leaf = session.leaf_id().unwrap().to_string();
        session.append_compaction("s".into(), Some(leaf), 1, None, None, false);
        assert!(prepare_compaction(&session.branch(), &settings(10)).is_none());
    }

    #[tokio::test]
    async fn compaction_asks_the_model_and_appends_file_lists() {
        let mut session = SessionManager::in_memory("/work");
        session.append_message(user("edit main.rs"));
        session.append_message(assistant(vec![faux_tool_call(
            "edit",
            json!({ "path": "src/main.rs" }),
        )]));
        session.append_message(user(&"recent ".repeat(200)));
        let preparation = prepare_compaction(&session.branch(), &settings(100)).unwrap();

        let faux = FauxProvider::new();
        faux.push_text("## Goal\nEdit main.rs");
        let model = faux.model();
        let summarizer = Summarizer {
            model: &model,
            stream_fn: &faux,
            options: StreamOptions::default(),
            max_retries: 0,
            retry_base_ms: 1,
            retry_max_ms: 60_000,
            on_retry: None,
        };
        let result = compact(&preparation, &summarizer, Some("keep paths"))
            .await
            .unwrap();
        assert!(result.summary.starts_with("## Goal\nEdit main.rs"));
        assert!(
            result
                .summary
                .ends_with("<modified-files>\nsrc/main.rs\n</modified-files>")
        );

        let request = &faux.requests()[0];
        let Message::User(prompt) = request.context.messages.last().unwrap() else {
            panic!()
        };
        let prompt = content_text(&prompt.content);
        assert!(prompt.contains("[User]: edit main.rs"));
        assert!(prompt.contains("Additional focus: keep paths"));
    }

    #[tokio::test]
    async fn an_update_asks_to_keep_the_previous_summary_in_its_own_format() {
        let preparation = CompactionPreparation {
            first_kept_entry_id: "kept".into(),
            messages_to_summarize: vec![user("a newer question")],
            turn_prefix_messages: Vec::new(),
            tokens_before: 100,
            previous_summary: Some("## Goal\nthe old goal".into()),
            file_operations: FileOperations::default(),
            settings: settings(20),
        };
        let faux = FauxProvider::new();
        faux.push_text("## Goal\nthe old goal, and more");
        let model = faux.model();
        let summarizer = Summarizer {
            model: &model,
            stream_fn: &faux,
            options: StreamOptions::default(),
            max_retries: 0,
            retry_base_ms: 1,
            retry_max_ms: 60_000,
            on_retry: None,
        };
        compact(&preparation, &summarizer, Some("keep paths"))
            .await
            .unwrap();
        let prompt = last_prompt(&faux);
        assert!(
            prompt.contains("<previous-summary>\n## Goal\nthe old goal\n</previous-summary>\n\nThe messages above are NEW conversation messages"),
            "{prompt}"
        );
        assert!(prompt.contains(
            "- PRESERVE exact file paths, function names, and error messages\n- If something is no longer relevant, you may remove it"
        ));
        assert!(
            prompt
                .contains("## Goal\n[Preserve existing goals, add new ones if the task expanded]")
        );
        assert!(prompt.ends_with("and error messages.\n\nAdditional focus: keep paths"));
    }

    #[tokio::test]
    async fn summaries_think_at_the_given_level_on_a_model_that_reasons() {
        use pi_ai::ThinkingLevel;
        let preparation = CompactionPreparation {
            first_kept_entry_id: "kept".into(),
            messages_to_summarize: vec![user("an older question")],
            turn_prefix_messages: vec![user("the start of a long task")],
            tokens_before: 100,
            previous_summary: None,
            file_operations: FileOperations::default(),
            settings: settings(20),
        };
        let mut plain = FauxProvider::default_model();
        plain.reasoning = false;
        let cases = [
            (
                FauxProvider::default_model(),
                ThinkingLevel::Medium,
                Some(ThinkingLevel::Medium),
            ),
            (FauxProvider::default_model(), ThinkingLevel::Off, None),
            (plain, ThinkingLevel::Medium, None),
        ];
        for (model, level, sent) in cases {
            let faux = FauxProvider::with_model(model.clone());
            faux.push_text("## Goal\nhistory");
            faux.push_text("## Original Request\ntask");
            let summarizer = Summarizer {
                model: &model,
                stream_fn: &faux,
                options: StreamOptions {
                    reasoning: Some(level),
                    ..StreamOptions::default()
                },
                max_retries: 0,
                retry_base_ms: 1,
                retry_max_ms: 60_000,
                on_retry: None,
            };
            compact(&preparation, &summarizer, None).await.unwrap();
            // The history and the turn's start alike.
            let requests = faux.requests();
            assert_eq!(requests.len(), 2);
            assert!(
                requests
                    .iter()
                    .all(|request| request.options.reasoning == sent),
                "{level:?} on {}",
                model.id
            );
        }
    }

    #[test]
    fn the_conversation_for_a_summary_leaves_thinking_out() {
        let reply = Message::Assistant(AssistantMessage {
            content: vec![
                AssistantContent::thinking("the user probably means the parser"),
                AssistantContent::text("Reading the parser."),
                faux_tool_call("read", json!({ "path": "src/parser.rs" })),
            ],
            ..AssistantMessage::empty(&FauxProvider::default_model())
        });
        let thought_only = Message::Assistant(AssistantMessage {
            content: vec![AssistantContent::thinking("nothing to add")],
            ..AssistantMessage::empty(&FauxProvider::default_model())
        });
        let text = serialize_conversation(&[
            Message::User(UserMessage::text("fix it")),
            reply,
            thought_only,
        ]);
        assert_eq!(
            text,
            "[User]: fix it\n\n[Assistant]: Reading the parser.\n\n[Assistant tool calls]: read(path=\"src/parser.rs\")"
        );
    }

    #[tokio::test]
    async fn a_branch_summary_sees_the_branch_as_the_context_did() {
        let mut session = SessionManager::in_memory("/work");
        let secret = session.append_message(user("my password is hunter2"));
        session.append_message(assistant(vec![AssistantContent::text("noted")]));
        session
            .append_context_edit(&secret, Some(vec![Content::text("[redacted]")]))
            .unwrap();
        let faux = FauxProvider::new();
        faux.push_text("## Goal\nRemember a password");
        let model = faux.model();
        let summarizer = Summarizer {
            model: &model,
            stream_fn: &faux,
            options: StreamOptions::default(),
            max_retries: 0,
            retry_base_ms: 1,
            retry_max_ms: 60_000,
            on_retry: None,
        };
        summarize_branch(
            &session.branch(),
            &summarizer,
            &settings(1),
            10_000,
            None,
            false,
        )
        .await
        .unwrap();
        let prompt = last_prompt(&faux);
        assert!(prompt.contains("[User]: [redacted]"), "{prompt}");
        assert!(!prompt.contains("hunter2"));

        // A branch without messages makes no request.
        let mut settings_only = SessionManager::in_memory("/work");
        settings_only.append_model_change("faux", "faux-2");
        let result = summarize_branch(
            &settings_only.branch(),
            &summarizer,
            &settings(1),
            10_000,
            None,
            false,
        )
        .await
        .unwrap();
        assert_eq!(result, BranchSummaryResult::default());
        assert_eq!(faux.requests().len(), 1);
    }

    fn last_prompt(faux: &FauxProvider) -> String {
        let requests = faux.requests();
        let Some(Message::User(prompt)) = requests.last().unwrap().context.messages.last() else {
            panic!()
        };
        content_text(&prompt.content)
    }

    #[tokio::test]
    async fn a_branch_summary_keeps_its_files_and_leaves_tool_results_out() {
        let mut session = SessionManager::in_memory("/work");
        let start = session.append_message(user("look around"));
        // An earlier summary's files count; an extension's do not.
        let earlier = session
            .branch_with_summary(
                Some(&start),
                "looked at the notes".into(),
                Some(json!({ "readFiles": ["notes.md"], "modifiedFiles": [] })),
                None,
                false,
            )
            .unwrap();
        session
            .branch_with_summary(
                Some(&earlier),
                "an extension's summary".into(),
                Some(json!({ "readFiles": ["secret.md"], "modifiedFiles": [] })),
                None,
                true,
            )
            .unwrap();
        session.append_message(user("fix the parser"));
        let read = faux_tool_call("read", json!({ "path": "src/parser.rs" }));
        let AssistantContent::ToolCall(call) = &read else {
            unreachable!()
        };
        let call_id = call.id.clone();
        let edit = faux_tool_call("edit", json!({ "path": "src/lexer.rs" }));
        session.append_message(assistant(vec![read, edit]));
        session.append_message(SessionMessage::Llm(Message::ToolResult(
            ToolResultMessage {
                tool_call_id: call_id,
                tool_name: "read".into(),
                content: vec![Content::text("fn parse() {}")],
                details: None,
                usage: None,
                is_error: false,
                timestamp: 0,
            },
        )));

        let faux = FauxProvider::new();
        faux.push_text("## Goal\nFix the parser");
        let model = faux.model();
        let summarizer = Summarizer {
            model: &model,
            stream_fn: &faux,
            options: StreamOptions::default(),
            max_retries: 0,
            retry_base_ms: 1,
            retry_max_ms: 60_000,
            on_retry: None,
        };
        let result = summarize_branch(&session.branch(), &summarizer, &settings(1), 0, None, false)
            .await
            .unwrap();
        assert_eq!(result.read_files, ["notes.md", "src/parser.rs"]);
        assert_eq!(result.modified_files, ["src/lexer.rs"]);
        assert_eq!(
            result.summary,
            format!(
                "{BRANCH_SUMMARY_PREAMBLE}## Goal\nFix the parser\n\n<read-files>\nnotes.md\nsrc/parser.rs\n</read-files>\n\n<modified-files>\nsrc/lexer.rs\n</modified-files>"
            )
        );
        let prompt = last_prompt(&faux);
        assert!(
            prompt.contains(
                "[Assistant tool calls]: read(path=\"src/parser.rs\"); edit(path=\"src/lexer.rs\")"
            ),
            "{prompt}"
        );
        assert!(!prompt.contains("[Tool result]") && !prompt.contains("fn parse"));
        assert_eq!(faux.requests()[0].options.max_tokens, Some(4_096));

        // The output cap is the model's when that is lower.
        let mut small = FauxProvider::default_model();
        small.max_tokens = 1_024;
        let faux = FauxProvider::with_model(small.clone());
        faux.push_text("## Goal\nFix the parser");
        let summarizer = Summarizer {
            model: &small,
            stream_fn: &faux,
            options: StreamOptions::default(),
            max_retries: 0,
            retry_base_ms: 1,
            retry_max_ms: 60_000,
            on_retry: None,
        };
        summarize_branch(&session.branch(), &summarizer, &settings(1), 0, None, false)
            .await
            .unwrap();
        assert_eq!(faux.requests()[0].options.max_tokens, Some(1_024));
    }

    #[tokio::test]
    async fn a_branch_summary_takes_the_newest_entries_that_fit() {
        // Oldest first: a question, an earlier compaction's summary of about 200 tokens, a
        // long question and a short answer.
        let branch = |long: usize| {
            let mut session = SessionManager::in_memory("/work");
            let first = session.append_message(user("an old question"));
            session.append_compaction("summary ".repeat(100), Some(first), 0, None, None, false);
            session.append_message(user(&"x".repeat(long)));
            session.append_message(assistant(vec![AssistantContent::text("short answer")]));
            session
        };
        let faux = FauxProvider::new();
        let model = faux.model();
        let summarizer = Summarizer {
            model: &model,
            stream_fn: &faux,
            options: StreamOptions::default(),
            max_retries: 0,
            retry_base_ms: 1,
            retry_max_ms: 60_000,
            on_retry: None,
        };
        let prompt = async |session: &SessionManager, budget: u64| {
            faux.push_text("## Goal\nsummary");
            summarize_branch(
                &session.branch(),
                &summarizer,
                &settings(1),
                budget,
                None,
                false,
            )
            .await
            .unwrap();
            last_prompt(&faux)
        };

        // The summary does not fit after about 800 tokens, but most of the budget is free.
        let text = prompt(&branch(3_200), 1_000).await;
        assert!(text.contains("summary summary") && text.contains("short answer"));
        assert!(!text.contains("an old question"));
        // After about 900, it is left out.
        let text = prompt(&branch(3_600), 1_000).await;
        assert!(text.contains("short answer") && !text.contains("summary summary"));
        // Without a limit, everything is taken.
        let text = prompt(&branch(3_600), 0).await;
        assert!(text.contains("summary summary") && text.contains("an old question"));
    }

    #[tokio::test]
    async fn a_cut_off_summary_is_an_error() {
        let session = three_turns();
        let preparation = prepare_compaction(&session.branch(), &settings(400)).unwrap();
        let faux = FauxProvider::new();
        let mut reply = pi_ai::faux::faux_message(vec![AssistantContent::text("## Goal\npart")]);
        reply.stop_reason = StopReason::Length;
        faux.push_reply(reply);
        let model = faux.model();
        let summarizer = Summarizer {
            model: &model,
            stream_fn: &faux,
            options: StreamOptions::default(),
            max_retries: 0,
            retry_base_ms: 1,
            retry_max_ms: 60_000,
            on_retry: None,
        };
        let error = compact(&preparation, &summarizer, None).await.unwrap_err();
        assert!(error.contains("token cap"), "{error}");
    }
}
