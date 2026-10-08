//! Compaction: summarize the older part of a branch so the context fits again.
//!
//! The cut keeps roughly `keep_recent_tokens` of the newest context. It never separates a
//! tool result from its call; when it falls inside a turn, the start of that turn is
//! summarized separately. The summary and the kept entries replace everything before
//! them in the context, while the session keeps the full history.

use std::collections::BTreeSet;
use std::time::Duration;

use pi_agent_core::AgentMessage;
use pi_ai::{
    AssistantContent, Context, Message, Model, StopReason, StreamFn, StreamOptions, Usage,
    UserMessage, content_text, estimate_message_tokens, is_retryable_error, retry_delay_ms,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::messages::SessionMessage;
use crate::session::{EntryKind, ProjectedEntry, SessionEntry, entry_messages, project};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CompactionSettings {
    pub enabled: bool,
    /// Room kept free for the response and the summary request.
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
    /// Tools whose `path` argument a model reads, for the file lists kept in summaries.
    pub read_tools: Vec<String>,
    /// Tools whose `path` argument a model changes.
    pub write_tools: Vec<String>,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            reserve_tokens: 16_384,
            keep_recent_tokens: 20_000,
            read_tools: vec!["read".into()],
            write_tools: vec!["write".into(), "edit".into()],
        }
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

pub fn estimate_tokens(message: &SessionMessage) -> u64 {
    estimate_message_tokens(&message.to_llm())
}

/// The context size: the last reported usage plus estimates for what came after it.
pub fn estimate_context_tokens(messages: &[SessionMessage]) -> u64 {
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
    // Keep state-only entries right before the cut with what follows them.
    while cut > start && !entries[cut - 1].is_compaction && entries[cut - 1].messages.is_empty() {
        cut -= 1;
    }
    let turn_start = if visible(cut, is_turn_start) {
        None
    } else {
        (start..=cut)
            .rev()
            .find(|index| visible(*index, is_turn_start))
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
    if let Some(EntryKind::Compaction {
        details: Some(details),
        ..
    }) = previous.map(|entry| &entry.kind)
    {
        for (key, set) in [
            ("readFiles", &mut file_operations.read),
            ("modifiedFiles", &mut file_operations.modified),
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
    for message in messages_to_summarize.iter().chain(&turn_prefix_messages) {
        file_operations.record(message, settings);
    }
    Some(CompactionPreparation {
        first_kept_entry_id,
        messages_to_summarize,
        turn_prefix_messages,
        tokens_before: estimate_context_tokens(&projection.messages),
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
    #[serde(default)]
    pub usage: Usage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

/// The model calls that write summaries.
pub struct Summarizer<'a> {
    pub model: &'a Model,
    pub stream_fn: &'a dyn StreamFn,
    /// Credentials, routing and cancellation for the requests.
    pub options: StreamOptions,
    /// Retries for transient failures.
    pub max_retries: u32,
    pub retry_base_ms: u64,
}

pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

const SUMMARY_FORMAT: &str = "## Goal\n[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned by user]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Current work]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [Ordered list of what should happen next]\n\n## Critical Context\n- [Any data, examples, or references needed to continue]\n- [Or \"(none)\" if not applicable]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

const SUMMARY_PROMPT: &str = "The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.\n\nUse this EXACT format:\n\n";

const UPDATE_PROMPT: &str = "The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.\n\nUpdate the existing structured summary with new information. RULES:\n- PRESERVE all existing information from the previous summary\n- ADD new progress, decisions, and context from the new messages\n- UPDATE the Progress section: move items from \"In Progress\" to \"Done\" when completed\n- UPDATE \"Next Steps\" based on what was accomplished\n- If something is no longer relevant, you may remove it\n\nUse this EXACT format:\n\n";

const TURN_PREFIX_PROMPT: &str = "The messages above are earlier context from an ongoing conversation. Later messages are stored separately and do not need to be reconstructed.\n\nCreate a concise checkpoint of the user's request and the progress shown above. This checkpoint will be placed before the later messages so the conversation can continue with the necessary context.\n\n## Original Request\n[What did the user ask for?]\n\n## Progress So Far\n- [Key decisions and work completed in these messages]\n\n## Context Needed to Continue\n- [Information from these messages needed to understand the later work]\n\nOnly summarize information explicitly present above. Do not infer or recreate later messages.";

const BRANCH_PROMPT: &str = "Create a structured summary of this conversation branch for context when returning later.\n\nUse this EXACT format:\n\n## Goal\n[What was the user trying to accomplish in this branch?]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Work that was started but not finished]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [What should happen next to continue this work]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

const TOOL_RESULT_MAX_CHARS: usize = 2_000;

/// Render messages as labelled text, so the summarizer reads them instead of continuing them.
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
                let mut thinking = Vec::new();
                let mut calls = Vec::new();
                for block in &assistant.content {
                    match block {
                        AssistantContent::Thinking(block) => thinking.push(block.thinking.as_str()),
                        AssistantContent::ToolCall(call) => {
                            let args: Vec<String> = call
                                .arguments
                                .iter()
                                .map(|(key, value)| format!("{key}={value}"))
                                .collect();
                            calls.push(format!("{}({})", call.name, args.join(", ")));
                        }
                        AssistantContent::Text(_) => {}
                    }
                }
                if !thinking.is_empty() {
                    parts.push(format!("[Assistant thinking]: {}", thinking.join("\n")));
                }
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
    let mut attempt = 0;
    loop {
        let options = StreamOptions {
            max_tokens: Some(max_tokens),
            ..summarizer.options.clone()
        };
        let response = summarizer
            .stream_fn
            .stream(summarizer.model, context.clone(), options)
            .result()
            .await;
        if is_retryable_error(&response)
            && attempt < summarizer.max_retries
            && !summarizer.options.cancel.is_cancelled()
        {
            attempt += 1;
            let delay = retry_delay_ms(summarizer.retry_base_ms, 60_000, attempt);
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(delay)) => continue,
                _ = summarizer.options.cancel.cancelled() => {}
            }
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
        prompt.push_str(&format!("{instructions}{SUMMARY_FORMAT}{focus}"));
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
        usage,
        details: Some(json!({ "readFiles": read, "modifiedFiles": modified })),
    })
}

/// Summarize the entries of a branch being left, newest first within `token_budget`.
pub async fn summarize_branch(
    entries: &[&SessionEntry],
    summarizer: &Summarizer<'_>,
    token_budget: u64,
    custom_instructions: Option<&str>,
) -> Result<(String, Usage), String> {
    let mut newest_first: Vec<Vec<SessionMessage>> = Vec::new();
    let mut used = 0;
    for entry in entries.iter().rev() {
        let entry_messages: Vec<SessionMessage> = entry_messages(entry)
            .into_iter()
            .filter(|message| !matches!(message.as_message(), Some(Message::System(_))))
            .collect();
        let tokens: u64 = entry_messages.iter().map(estimate_tokens).sum();
        if used + tokens > token_budget && !newest_first.is_empty() {
            break;
        }
        used += tokens;
        newest_first.push(entry_messages);
    }
    let messages: Vec<SessionMessage> = newest_first.into_iter().rev().flatten().collect();
    let conversation = serialize_conversation(&llm_messages(&messages));
    let focus = custom_instructions
        .map(|instructions| format!("\n\nAdditional focus: {instructions}"))
        .unwrap_or_default();
    let prompt =
        format!("<conversation>\n{conversation}\n</conversation>\n\n{BRANCH_PROMPT}{focus}");
    summarize(summarizer, prompt, 2_048, "Branch summarization").await
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
        };
        let error = compact(&preparation, &summarizer, None).await.unwrap_err();
        assert!(error.contains("token cap"), "{error}");
    }
}
