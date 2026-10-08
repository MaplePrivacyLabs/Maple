//! Branch collection, budgeted preparation, and summaries for tree navigation.
pub use super::utils::FileOperations;
use super::{
    compaction::{
        SummaryRuntime, add_stored_file_operations, complete_summarization, estimate_tokens,
        get_summarization_failure, js_truthy,
    },
    utils::{
        SUMMARIZATION_SYSTEM_PROMPT, compute_file_lists, create_file_ops,
        extract_file_ops_from_message, format_file_operations, serialize_conversation,
    },
};
use crate::core::{
    messages::{
        convert_to_llm, create_branch_summary_message, create_compaction_summary_message,
        create_custom_message,
    },
    session_manager::{ReadonlySessionManager, SessionEntry},
};
use pi_agent_core::types::{AgentError, AgentMessage, AgentResult};
use pi_ai::{
    env::CancellationToken,
    types::{
        AssistantContent, Context, JsString, Message, Model, ProviderEnv, ProviderHeaders,
        ProviderRequestOptions, SimpleStreamOptions, StopReason, StreamOptions, TextContent, Usage,
        UserContent, UserMessage, UserMessageContent,
    },
    utils::{
        retry::{RetryCallbacks, RetryPolicy},
        text::content_text,
        transcript::normalize_context,
    },
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<JsString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_files: Option<Vec<JsString>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified_files: Option<Vec<JsString>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aborted: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsString>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryDetails {
    pub read_files: Vec<JsString>,
    pub modified_files: Vec<JsString>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchPreparation {
    pub messages: Vec<AgentMessage>,
    pub file_ops: FileOperations,
    pub total_tokens: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectEntriesResult {
    pub entries: Vec<SessionEntry>,
    pub common_ancestor_id: Option<JsString>,
}

#[derive(Clone)]
pub struct GenerateBranchSummaryOptions {
    pub model: Model,
    pub api_key: Option<String>,
    pub headers: Option<ProviderHeaders>,
    pub env: Option<ProviderEnv>,
    pub signal: CancellationToken,
    pub custom_instructions: Option<JsString>,
    pub replace_instructions: bool,
    pub reserve_tokens: Option<f64>,
    pub retry: Option<RetryPolicy>,
}
impl GenerateBranchSummaryOptions {
    pub fn new(model: Model, signal: CancellationToken) -> Self {
        Self {
            model,
            signal,
            api_key: None,
            headers: None,
            env: None,
            custom_instructions: None,
            replace_instructions: false,
            reserve_tokens: None,
            retry: None,
        }
    }
}

pub fn collect_entries_for_branch_summary(
    session: &dyn ReadonlySessionManager,
    old_leaf_id: Option<&JsString>,
    target_id: &JsString,
) -> CollectEntriesResult {
    let Some(old_leaf_id) = old_leaf_id.filter(|id| !id.is_empty()) else {
        return CollectEntriesResult {
            entries: Vec::new(),
            common_ancestor_id: None,
        };
    };
    let old_path: Vec<_> = session
        .get_branch(Some(old_leaf_id))
        .iter()
        .map(SessionEntry::id)
        .collect();
    let target_path = session.get_branch(Some(target_id));
    let common_ancestor_id = target_path
        .iter()
        .rev()
        .find(|e| old_path.contains(&e.id()))
        .map(SessionEntry::id);
    let mut entries = Vec::new();
    let mut current = Some(old_leaf_id.clone());
    while let Some(id) = current.filter(|id| !id.is_empty()) {
        if Some(&id) == common_ancestor_id.as_ref() {
            break;
        }
        let Some(entry) = session.get_entry(&id) else {
            break;
        };
        current = entry.parent_id();
        entries.push(entry);
    }
    entries.reverse();
    CollectEntriesResult {
        entries,
        common_ancestor_id,
    }
}

fn entry_string(entry: &SessionEntry, key: &str) -> JsString {
    entry
        .get(key)
        .and_then(|v| v.as_js_str().cloned())
        .unwrap_or_default()
}
fn get_message_from_entry(entry: &SessionEntry) -> Option<AgentMessage> {
    match entry.kind().as_str() {
        Some("message") => entry.message().filter(|m| m.role() != "toolResult"),
        Some("custom_message") => Some(create_custom_message(
            entry_string(entry, "customType"),
            entry.get("content").unwrap_or_default(),
            entry.get("display").is_some_and(|v| js_truthy(&v)),
            entry.get("details"),
            &entry_string(entry, "timestamp"),
        )),
        Some("branch_summary") => Some(create_branch_summary_message(
            entry_string(entry, "summary"),
            entry_string(entry, "fromId"),
            &entry_string(entry, "timestamp"),
        )),
        Some("compaction") => Some(create_compaction_summary_message(
            entry_string(entry, "summary"),
            entry
                .get("tokensBefore")
                .and_then(|v| v.as_f64())
                .unwrap_or(f64::NAN),
            &entry_string(entry, "timestamp"),
        )),
        _ => None,
    }
}

pub fn prepare_branch_entries(
    entries: &[SessionEntry],
    token_budget: f64,
) -> AgentResult<BranchPreparation> {
    let mut messages = Vec::new();
    let mut file_ops = create_file_ops();
    let mut total_tokens = 0.0;
    for entry in entries {
        if entry.kind() == "branch_summary" {
            add_stored_file_operations(entry, &mut file_ops);
        }
    }
    for entry in entries.iter().rev() {
        let Some(message) = get_message_from_entry(entry) else {
            continue;
        };
        extract_file_ops_from_message(&message, &mut file_ops);
        let tokens = estimate_tokens(&message)?;
        if token_budget > 0.0 && total_tokens + tokens > token_budget {
            if matches!(entry.kind().as_str(), Some("compaction" | "branch_summary"))
                && total_tokens < token_budget * 0.9
            {
                messages.insert(0, message);
                total_tokens += tokens;
            }
            break;
        }
        messages.insert(0, message);
        total_tokens += tokens;
    }
    Ok(BranchPreparation {
        messages,
        file_ops,
        total_tokens,
    })
}

pub async fn generate_branch_summary(
    entries: &[SessionEntry],
    options: &GenerateBranchSummaryOptions,
    runtime: &SummaryRuntime,
    callbacks: Option<&mut dyn RetryCallbacks<AgentError>>,
) -> AgentResult<BranchSummaryResult> {
    let model = &options.model;
    let context_window = if model.context_window == 0.0 || model.context_window.is_nan() {
        128000.0
    } else {
        model.context_window
    };
    let preparation = prepare_branch_entries(
        entries,
        context_window - options.reserve_tokens.unwrap_or(16384.0),
    )?;
    if preparation.messages.is_empty() {
        return Ok(BranchSummaryResult {
            summary: Some("No content to summarize".into()),
            ..Default::default()
        });
    }
    let conversation = serialize_conversation(&convert_to_llm(&preparation.messages))?;
    let custom = options
        .custom_instructions
        .as_ref()
        .filter(|s| !s.is_empty());
    let instructions = match custom {
        Some(custom) if options.replace_instructions => custom.clone(),
        Some(custom) => {
            let mut text = JsString::from(BRANCH_SUMMARY_PROMPT);
            text.push_str("\n\nAdditional focus: ");
            text.push(custom);
            text
        }
        None => BRANCH_SUMMARY_PROMPT.into(),
    };
    let mut prompt = JsString::from("<conversation>\n");
    prompt.push(&conversation);
    prompt.push_str("\n</conversation>\n\n");
    prompt.push(&instructions);
    let context = normalize_context(Context {
        system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.into()),
        messages: vec![Message::User(UserMessage {
            content: UserMessageContent::Blocks(vec![UserContent::Text(TextContent::new(prompt))]),
            timestamp: runtime.env.now_ms() as f64,
            ..Default::default()
        })],
        tools: None,
    });
    let request = SimpleStreamOptions {
        stream: StreamOptions {
            request: ProviderRequestOptions {
                api_key: options.api_key.clone(),
                headers: options.headers.clone(),
                env: options.env.clone(),
                signal: Some(options.signal.clone()),
                ..Default::default()
            },
            max_tokens: Some(4096.0_f64.min(if model.max_tokens > 0.0 {
                model.max_tokens
            } else {
                f64::INFINITY
            })),
            ..Default::default()
        },
        ..Default::default()
    };
    let response = complete_summarization(
        model,
        context,
        request,
        runtime,
        options.retry.as_ref(),
        callbacks,
    )
    .await?;
    if response.stop_reason == StopReason::Aborted {
        return Ok(BranchSummaryResult {
            aborted: Some(true),
            ..Default::default()
        });
    }
    if let Some(failure) = get_summarization_failure(&response, "Branch summarization") {
        return Ok(BranchSummaryResult {
            error: Some(failure),
            ..Default::default()
        });
    }
    if response
        .content
        .iter()
        .any(|b| matches!(b, AssistantContent::ToolCall(_)))
    {
        return Ok(BranchSummaryResult {
            error: Some("Branch summarization attempted to call a tool".into()),
            ..Default::default()
        });
    }
    let mut summary = JsString::from(BRANCH_SUMMARY_PREAMBLE);
    summary.push(&content_text(&response.content, "\n"));
    let files = compute_file_lists(&preparation.file_ops);
    summary.push(&format_file_operations(
        &files.read_files,
        &files.modified_files,
    ));
    if summary.is_empty() {
        summary = "No summary generated".into();
    }
    Ok(BranchSummaryResult {
        summary: Some(summary),
        usage: Some(response.usage),
        read_files: Some(files.read_files),
        modified_files: Some(files.modified_files),
        ..Default::default()
    })
}

const BRANCH_SUMMARY_PREAMBLE: &str = "The user explored a different conversation branch before returning here.\nSummary of that exploration:\n\n";
const BRANCH_SUMMARY_PROMPT: &str = include_str!("prompts/branch_summary.txt");
