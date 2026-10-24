//! Temporary legacy window strategy for the Responses compaction rollout.
//!
//! Window selection follows the pre-compaction implementation. Payload transport
//! is metadata-batched and each row is immediately projected before its raw
//! ciphertext and any inline image JSON are discarded. This does not cap total
//! transfer: one indivisible or concurrently grown row can exceed the target.
//! This module never reads, validates, plans or writes a compaction checkpoint.

use super::{
    compaction::{self, PreparedPrompt},
    constants::{ROLE_ASSISTANT, ROLE_SYSTEM, ROLE_USER},
    context_builder::{
        format_chat_messages, load_instruction_messages, normalize_tool_call_ids_for_model,
        prompt_token_budget, user_message_prompt_token_count, ChatMsg,
    },
    tools::collect_kagi_allowed_urls_from_prompt,
};
use crate::{
    encrypt::decrypt_with_key,
    model_config::ReasoningReplay,
    models::responses::{RawThreadMessage, RawThreadMessageMetadata},
    tokens::count_tokens,
    ApiError, DBConnection,
};
use secp256k1::SecretKey;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

const MAX_BATCH_ROWS: usize = 128;
const SOURCE_BATCH_BYTES_TARGET: usize = 8 * 1024 * 1024;
const TRUNCATION_MARKER: &str = "[Previous messages truncated due to context limits]";

type MessageKey = (String, i64);

#[derive(Debug, Default)]
pub(super) struct LegacyContextStats {
    pub metadata_rows: usize,
    pub selected_rows: usize,
    pub fetched_rows: usize,
    pub fetch_batches: usize,
    pub fetched_bytes: usize,
    /// Observed ciphertext held in a returned fetch batch, not a strict ceiling.
    pub max_batch_bytes: usize,
    pub projected_bytes: usize,
    pub truncated: bool,
}

pub(super) struct LegacyPreparedPrompt {
    pub prompt: PreparedPrompt,
    pub stats: LegacyContextStats,
}

#[derive(Clone)]
struct ContextEntry {
    message_type: String,
    ids: Vec<MessageKey>,
    tok: usize,
}

impl ContextEntry {
    fn single(message: &RawThreadMessageMetadata) -> Self {
        Self {
            message_type: message.message_type.clone(),
            ids: vec![(message.message_type.clone(), message.id)],
            tok: metadata_token_count(message),
        }
    }
}

fn metadata_token_count(message: &RawThreadMessageMetadata) -> usize {
    message
        .token_count
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(0)
}

#[derive(Clone)]
struct ProjectedMessage {
    role: &'static str,
    prompt: Value,
    tok: usize,
}

struct PendingReasoning {
    content: String,
    tok: usize,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn prepare_initial<D: DBConnection + ?Sized>(
    db: &D,
    conversation_id: i64,
    user_id: Uuid,
    user_key: &SecretKey,
    model: &str,
    override_instructions: Option<&str>,
    internal_instructions: &str,
    reasoning_replay: ReasoningReplay,
    incoming_messages: &[Value],
    incoming_tokens: usize,
    requested_output_tokens: usize,
) -> Result<LegacyPreparedPrompt, ApiError> {
    prepare(
        db,
        conversation_id,
        user_id,
        user_key,
        model,
        override_instructions,
        internal_instructions,
        reasoning_replay,
        incoming_messages,
        incoming_tokens,
        requested_output_tokens,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn prepare_persisted<D: DBConnection + ?Sized>(
    db: &D,
    conversation_id: i64,
    user_id: Uuid,
    user_key: &SecretKey,
    model: &str,
    override_instructions: Option<&str>,
    internal_instructions: &str,
    reasoning_replay: ReasoningReplay,
    requested_output_tokens: usize,
) -> Result<LegacyPreparedPrompt, ApiError> {
    prepare(
        db,
        conversation_id,
        user_id,
        user_key,
        model,
        override_instructions,
        internal_instructions,
        reasoning_replay,
        &[],
        0,
        requested_output_tokens,
    )
}

#[allow(clippy::too_many_arguments)]
fn prepare<D: DBConnection + ?Sized>(
    db: &D,
    conversation_id: i64,
    user_id: Uuid,
    user_key: &SecretKey,
    model: &str,
    override_instructions: Option<&str>,
    internal_instructions: &str,
    reasoning_replay: ReasoningReplay,
    incoming_messages: &[Value],
    incoming_tokens: usize,
    requested_output_tokens: usize,
) -> Result<LegacyPreparedPrompt, ApiError> {
    let (instructions, instruction_tokens) = load_instruction_messages(
        db,
        conversation_id,
        user_id,
        user_key,
        override_instructions,
        Some(internal_instructions),
    )?;
    let metadata = db
        .get_conversation_context_metadata(conversation_id)
        .map_err(|_| ApiError::InternalServerError)?;
    let result = prepare_with_fetch(
        &metadata,
        instructions,
        instruction_tokens,
        user_key,
        model,
        reasoning_replay,
        incoming_messages,
        incoming_tokens,
        requested_output_tokens,
        |keys| {
            db.get_messages_by_ids(conversation_id, keys)
                .map_err(|_| ApiError::InternalServerError)
        },
    )?;
    tracing::info!(
        event = "responses.context.legacy_prepared",
        user_uuid = %user_id,
        conversation_id,
        model_id = compaction::telemetry::model_id(model),
        metadata_rows = result.stats.metadata_rows,
        selected_rows = result.stats.selected_rows,
        fetched_rows = result.stats.fetched_rows,
        fetch_batches = result.stats.fetch_batches,
        fetched_bytes = result.stats.fetched_bytes,
        max_batch_bytes = result.stats.max_batch_bytes,
        projected_bytes = result.stats.projected_bytes,
        truncated = result.stats.truncated,
        estimated_tokens = result.prompt.estimated_tokens,
        "Responses legacy window prepared",
    );
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
fn prepare_with_fetch(
    metadata: &[RawThreadMessageMetadata],
    instructions: Vec<Value>,
    instruction_tokens: usize,
    user_key: &SecretKey,
    model: &str,
    reasoning_replay: ReasoningReplay,
    incoming_messages: &[Value],
    incoming_tokens: usize,
    requested_output_tokens: usize,
    mut fetch: impl FnMut(&[MessageKey]) -> Result<Vec<RawThreadMessage>, ApiError>,
) -> Result<LegacyPreparedPrompt, ApiError> {
    let input_budget = prompt_token_budget(model)
        .checked_sub(requested_output_tokens)
        .ok_or(ApiError::MessageExceedsContextLimit)?;
    if instruction_tokens.saturating_add(incoming_tokens) > input_budget {
        return Err(ApiError::MessageExceedsContextLimit);
    }
    let reserve = incoming_tokens.saturating_add(requested_output_tokens);
    let (mut selected, did_truncate) = determine_needed_message_ids(
        metadata,
        model,
        instruction_tokens,
        reserve,
        reasoning_replay,
    )?;
    // The original loader's remove-by-key already emitted each row once when
    // its forced head/tail selections overlapped. Preserve that final behavior.
    let mut seen = HashSet::new();
    selected.retain(|key| seen.insert(key.clone()));
    let by_key: HashMap<_, _> = metadata
        .iter()
        .map(|row| ((row.message_type.clone(), row.id), row))
        .collect();
    let mut stats = LegacyContextStats {
        metadata_rows: metadata.len(),
        selected_rows: selected.len(),
        truncated: did_truncate,
        ..Default::default()
    };
    let mut messages = Vec::new();
    if !instructions.is_empty() {
        // The shared instruction loader emits one combined system message.
        if instructions.len() != 1 {
            return Err(ApiError::InternalServerError);
        }
        messages.push(ProjectedMessage {
            role: ROLE_SYSTEM,
            prompt: instructions.into_iter().next().unwrap(),
            tok: instruction_tokens,
        });
    }
    let mut pending_reasoning = None;
    let mut next = 0;
    while next < selected.len() {
        let start = next;
        let mut estimated_bytes = 0usize;
        while next < selected.len() {
            let metadata = by_key
                .get(&selected[next])
                .ok_or(ApiError::InternalServerError)?;
            let bytes = usize::try_from(metadata.encrypted_bytes)
                .map_err(|_| ApiError::InternalServerError)?;
            if next > start
                && (next - start >= MAX_BATCH_ROWS
                    || estimated_bytes.saturating_add(bytes) > SOURCE_BATCH_BYTES_TARGET)
            {
                break;
            }
            estimated_bytes = estimated_bytes.saturating_add(bytes);
            next += 1;
        }
        let keys = &selected[start..next];
        let raw = fetch(keys)?;
        let actual_bytes = raw
            .iter()
            .filter_map(|row| row.content_enc.as_ref())
            .map(Vec::len)
            .fold(0usize, usize::saturating_add);
        stats.fetch_batches += 1;
        stats.fetched_rows = stats.fetched_rows.saturating_add(raw.len());
        stats.fetched_bytes = stats.fetched_bytes.saturating_add(actual_bytes);
        stats.max_batch_bytes = stats.max_batch_bytes.max(actual_bytes);
        let expected: HashMap<_, _> = keys
            .iter()
            .map(|key| (key.clone(), by_key[key].uuid))
            .collect();
        let mut ordered = HashMap::with_capacity(raw.len());
        for row in raw {
            let key = (row.message_type.clone(), row.id);
            if expected.get(&key) != Some(&row.uuid) || ordered.insert(key, row).is_some() {
                return Err(ApiError::InternalServerError);
            }
        }
        // A concurrent source deletion can leave a selected row absent, as in
        // the old ordered targeted loader. Never substitute an unrelated row.
        for key in keys {
            if let Some(row) = ordered.remove(key) {
                project_row(
                    row,
                    user_key,
                    reasoning_replay,
                    &mut pending_reasoning,
                    &mut messages,
                )?;
            }
        }
        // Only this batch's raw rows ever coexist. The next fetch starts after
        // all ciphertext and serialized inline images from this one are gone.
    }
    if did_truncate {
        let insert = if messages.first().is_some_and(|row| row.role == ROLE_SYSTEM) {
            2
        } else {
            1
        };
        if messages.len() > insert {
            messages.insert(insert, truncation_message());
        }
    }
    let (messages, projected_tokens, trimmed_again) =
        fit_projected_window(messages, input_budget.saturating_sub(incoming_tokens))?;
    stats.truncated |= trimmed_again;
    let mut prompt: Vec<Value> = messages.into_iter().map(|message| message.prompt).collect();
    prompt.extend_from_slice(incoming_messages);
    normalize_tool_call_ids_for_model(&mut prompt, model);
    let estimated_tokens = projected_tokens.saturating_add(incoming_tokens);
    if estimated_tokens > input_budget {
        return Err(ApiError::MessageExceedsContextLimit);
    }
    stats.projected_bytes = serialized_size(&prompt)?;
    let allowed_urls = collect_kagi_allowed_urls_from_prompt(&prompt);
    Ok(LegacyPreparedPrompt {
        prompt: PreparedPrompt {
            messages: prompt,
            estimated_tokens,
            compactions: Vec::new(),
            allowed_urls,
        },
        stats,
    })
}

fn decrypted_content(row: &RawThreadMessage, key: &SecretKey) -> Result<Option<String>, ApiError> {
    row.content_enc
        .as_ref()
        .map(|ciphertext| {
            decrypt_with_key(key, ciphertext)
                .map(|plain| String::from_utf8_lossy(&plain).into_owned())
                .map_err(|_| ApiError::InternalServerError)
        })
        .transpose()
}

fn content_tokens(stored: Option<i32>, content: &str) -> usize {
    stored
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or_else(|| count_tokens(content))
}

fn formatted_message(
    role: &'static str,
    content: String,
    reasoning: Option<String>,
    tool_call_id: Option<Uuid>,
) -> Result<Value, ApiError> {
    format_chat_messages(vec![ChatMsg {
        role,
        content,
        reasoning,
        tool_call_id,
        #[cfg(test)]
        tok: 0,
    }])?
    .pop()
    .ok_or(ApiError::InternalServerError)
}

fn project_row(
    row: RawThreadMessage,
    key: &SecretKey,
    reasoning_replay: ReasoningReplay,
    pending: &mut Option<PendingReasoning>,
    messages: &mut Vec<ProjectedMessage>,
) -> Result<(), ApiError> {
    let Some(content) = decrypted_content(&row, key)? else {
        if matches!(row.message_type.as_str(), "assistant" | "tool_call") {
            *pending = None;
        }
        return Ok(());
    };
    match row.message_type.as_str() {
        "user" => {
            *pending = None;
            let tok = user_message_prompt_token_count(&content, row.token_count);
            // Shared user conversion strips image bytes now, not after every
            // selected image-bearing row has accumulated in a ChatMsg vector.
            let prompt = formatted_message(ROLE_USER, content, None, None)?;
            messages.push(ProjectedMessage {
                role: ROLE_USER,
                prompt,
                tok,
            });
        }
        "assistant" | "tool_call" => {
            let tok = content_tokens(row.token_count, &content);
            let content = if row.message_type == "tool_call" {
                let arguments: Value = serde_json::from_str(&content).unwrap_or_else(|_| json!({}));
                json!({"role":ROLE_ASSISTANT,"tool_calls":[{
                    "id":row.tool_call_id.unwrap_or_else(Uuid::new_v4).to_string(),
                    "type":"function","function":{
                        "name":row.tool_name.as_deref().unwrap_or("function"),
                        "arguments":serde_json::to_string(&arguments).map_err(|_| ApiError::InternalServerError)?,
                    }
                }]}).to_string()
            } else {
                content
            };
            let reasoning = pending.take();
            let tok = tok.saturating_add(reasoning.as_ref().map_or(0, |value| value.tok));
            let prompt = formatted_message(
                ROLE_ASSISTANT,
                content,
                reasoning.map(|value| value.content),
                None,
            )?;
            messages.push(ProjectedMessage {
                role: ROLE_ASSISTANT,
                prompt,
                tok,
            });
        }
        "tool_output" => {
            *pending = None;
            let tok = content_tokens(row.token_count, &content);
            let prompt = formatted_message("tool", content, None, row.tool_call_id)?;
            messages.push(ProjectedMessage {
                role: "tool",
                prompt,
                tok,
            });
        }
        "reasoning" if reasoning_replay != ReasoningReplay::None && !content.is_empty() => {
            let tok = content_tokens(row.token_count, &content);
            if let Some(reasoning) = pending {
                reasoning.content.push_str(&content);
                reasoning.tok = reasoning.tok.saturating_add(tok);
            } else {
                *pending = Some(PendingReasoning { content, tok });
            }
        }
        "reasoning" => {}
        _ => *pending = None,
    }
    Ok(())
}

fn truncation_message() -> ProjectedMessage {
    ProjectedMessage {
        role: ROLE_ASSISTANT,
        prompt: json!({"role":ROLE_ASSISTANT,"content":TRUNCATION_MARKER}),
        tok: count_tokens(TRUNCATION_MARKER),
    }
}

/// The old post-decryption second pass, operating on already projected messages.
fn fit_projected_window(
    mut messages: Vec<ProjectedMessage>,
    budget: usize,
) -> Result<(Vec<ProjectedMessage>, usize, bool), ApiError> {
    let original_count = messages.len();
    let total = messages
        .iter()
        .map(|message| message.tok)
        .fold(0usize, usize::saturating_add);
    if total <= budget {
        return Ok((messages, total, false));
    }
    let mut head = Vec::new();
    let mut has_system = false;
    for message in &messages {
        if message.role == ROLE_SYSTEM && !has_system {
            head.push(message.clone());
            has_system = true;
            continue;
        }
        if message.role == ROLE_USER {
            head.push(message.clone());
            break;
        }
    }
    let head_tokens = head
        .iter()
        .map(|message| message.tok)
        .fold(0usize, usize::saturating_add);
    let available =
        budget.saturating_sub(head_tokens.saturating_add(count_tokens(TRUNCATION_MARKER)));
    let mut tail = Vec::new();
    let mut tokens = 0usize;
    for (index, message) in messages.iter().rev().enumerate() {
        if index > 0 && tokens.saturating_add(message.tok) > available {
            break;
        }
        tokens = tokens.saturating_add(message.tok);
        tail.push(message.clone());
    }
    tail.reverse();
    let mut found_user = false;
    tail.retain(|message| {
        found_user |= message.role == ROLE_USER;
        found_user
    });
    messages = head;
    if messages.len().saturating_add(tail.len()) < original_count {
        messages.push(truncation_message());
    }
    messages.extend(tail.clone());
    let mut total = messages
        .iter()
        .map(|message| message.tok)
        .fold(0usize, usize::saturating_add);
    if total > budget {
        if has_system {
            let system = messages
                .iter()
                .find(|message| message.role == ROLE_SYSTEM)
                .cloned()
                .ok_or(ApiError::InternalServerError)?;
            let tail_tokens = tail
                .iter()
                .map(|message| message.tok)
                .fold(0usize, usize::saturating_add);
            if system.tok.saturating_add(tail_tokens) > budget {
                return Err(ApiError::MessageExceedsContextLimit);
            }
            messages = vec![system];
            messages.extend(tail);
        } else {
            messages = tail;
        }
        total = messages
            .iter()
            .map(|message| message.tok)
            .fold(0usize, usize::saturating_add);
    }
    if total > budget {
        return Err(ApiError::MessageExceedsContextLimit);
    }
    Ok((messages, total, true))
}

fn serialized_size(messages: &[Value]) -> Result<usize, ApiError> {
    #[derive(Default)]
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter::default();
    serde_json::to_writer(&mut counter, messages).map_err(|_| ApiError::InternalServerError)?;
    Ok(counter.0)
}

fn determine_needed_message_ids(
    metadata: &[crate::models::responses::RawThreadMessageMetadata],
    model: &str,
    system_tokens: usize,
    token_reserve: usize,
    reasoning_replay: ReasoningReplay,
) -> Result<(Vec<(String, i64)>, bool), crate::ApiError> {
    use tracing::debug;

    let entries = build_context_entries(metadata, reasoning_replay);

    // Calculate token budget
    let ctx_budget = prompt_token_budget(model).saturating_sub(token_reserve);

    // Calculate total tokens in all messages
    let total_msg_tokens: usize = entries.iter().map(|entry| entry.tok).sum();

    // If everything fits, return all IDs with no truncation
    if system_tokens + total_msg_tokens <= ctx_budget {
        debug!(
            "All {} messages fit in context budget ({}+ {} = {} <= {})",
            entries.len(),
            system_tokens,
            total_msg_tokens,
            system_tokens + total_msg_tokens,
            ctx_budget
        );
        return Ok((
            entries.into_iter().flat_map(|entry| entry.ids).collect(),
            false, // did not truncate
        ));
    }

    // Need to truncate - apply middle truncation logic
    debug!(
        "Truncating {} messages: total tokens {}+{} = {} exceeds budget {}",
        entries.len(),
        system_tokens,
        total_msg_tokens,
        system_tokens + total_msg_tokens,
        ctx_budget
    );

    let mut needed_entries: Vec<ContextEntry> = Vec::new();

    // Always preserve the first system message (already accounted for in system_tokens)
    // Then keep first user message only (first assistant gets removed)
    for entry in &entries {
        if entry.message_type == "user" {
            needed_entries.push(entry.clone());
            break; // Stop after first user - don't keep first assistant
        }
    }

    let head_tokens: usize = needed_entries.iter().map(|entry| entry.tok).sum();

    let truncation_msg_tokens = count_tokens("[Previous messages truncated due to context limits]");

    // Calculate how many tokens we have left for the tail
    let available_for_tail =
        ctx_budget.saturating_sub(system_tokens + head_tokens + truncation_msg_tokens);

    // Collect messages from the end until we hit the budget
    // CRITICAL: Always include the most recent message (even if it exceeds budget)
    // to ensure the current user query is never lost
    let mut potential_tail: Vec<ContextEntry> = Vec::new();
    let mut potential_tail_tokens = 0usize;

    for (i, entry) in entries.iter().rev().enumerate() {
        // Always include the most recent message (first iteration), even if it exceeds budget
        if i == 0 {
            potential_tail.push(entry.clone());
            potential_tail_tokens += entry.tok;
            continue;
        }

        // For subsequent messages, respect the budget
        if potential_tail_tokens + entry.tok > available_for_tail {
            break;
        }
        potential_tail.push(entry.clone());
        potential_tail_tokens += entry.tok;
    }
    potential_tail.reverse();

    // Ensure tail starts with a user message (drop leading assistant messages if needed)
    let mut found_user = false;
    for entry in potential_tail {
        if !found_user {
            if entry.message_type == "user" {
                found_user = true;
                needed_entries.push(entry);
            }
            // Skip any leading non-user messages
        } else {
            needed_entries.push(entry);
        }
    }

    // Add the truncation indicator to the needed IDs (it will be inserted by build_prompt_from_chat_messages)
    // Note: The truncation message itself is inserted by build_prompt_from_chat_messages if truncation occurred

    debug!(
        "Truncation: keeping {} out of {} messages",
        needed_entries.len(),
        entries.len()
    );

    Ok((
        needed_entries
            .into_iter()
            .flat_map(|entry| entry.ids)
            .collect(),
        true,
    )) // did truncate
}

fn build_context_entries(
    metadata: &[crate::models::responses::RawThreadMessageMetadata],
    reasoning_replay: ReasoningReplay,
) -> Vec<ContextEntry> {
    // Current-turn replay keeps only reasoning after the last user message.
    let last_user_index = metadata
        .iter()
        .rposition(|message| message.message_type == "user");
    let replays_reasoning_at = |index: usize| match reasoning_replay {
        ReasoningReplay::None => false,
        ReasoningReplay::CurrentTurn => last_user_index.is_none_or(|user| index > user),
        ReasoningReplay::All => true,
    };
    let mut entries = Vec::new();
    let mut pending_reasoning: Vec<crate::models::responses::RawThreadMessageMetadata> = Vec::new();
    let tool_outputs = metadata
        .iter()
        .filter(|message| message.message_type == "tool_output")
        .filter_map(|message| message.tool_call_id.map(|call_id| (call_id, message)))
        .collect::<HashMap<_, _>>();

    let mut index = 0usize;
    while index < metadata.len() {
        let message = &metadata[index];
        match message.message_type.as_str() {
            "reasoning" if replays_reasoning_at(index) => {
                pending_reasoning.push(message.clone());
                index += 1;
            }
            "reasoning" => {
                index += 1;
            }
            "assistant" => {
                let mut ids = Vec::new();
                let mut tok = 0usize;

                for reasoning in pending_reasoning.drain(..) {
                    tok += metadata_token_count(&reasoning);
                    ids.push((reasoning.message_type, reasoning.id));
                }

                tok += metadata_token_count(message);
                ids.push((message.message_type.clone(), message.id));
                entries.push(ContextEntry {
                    message_type: message.message_type.clone(),
                    ids,
                    tok,
                });
                index += 1;
            }
            "tool_call" => {
                let Some(tool_output) = message
                    .tool_call_id
                    .and_then(|call_id| tool_outputs.get(&call_id).copied())
                else {
                    // A historical tool call without its output cannot form a valid
                    // model-context sequence. This can happen after an interrupted
                    // response or at a bounded metadata-window edge, so omit it
                    // together with any reasoning that belonged to it.
                    pending_reasoning.clear();
                    index += 1;
                    continue;
                };

                let mut ids = Vec::new();
                let mut tok = 0usize;

                for reasoning in pending_reasoning.drain(..) {
                    tok += metadata_token_count(&reasoning);
                    ids.push((reasoning.message_type, reasoning.id));
                }

                tok += metadata_token_count(message);
                ids.push((message.message_type.clone(), message.id));
                tok += metadata_token_count(tool_output);
                ids.push((tool_output.message_type.clone(), tool_output.id));
                entries.push(ContextEntry {
                    message_type: message.message_type.clone(),
                    ids,
                    tok,
                });
                index += 1;
            }
            "tool_output" => {
                // Never send a tool result without the assistant tool call it answers.
                // The newest-1,000 metadata window may begin between a persisted call
                // and output; dropping the boundary output is safer than emitting an
                // invalid upstream message sequence.
                pending_reasoning.clear();
                index += 1;
            }
            _ => {
                pending_reasoning.clear();
                entries.push(ContextEntry::single(message));
                index += 1;
            }
        }
    }

    entries
}

#[cfg(test)]
mod tests;
