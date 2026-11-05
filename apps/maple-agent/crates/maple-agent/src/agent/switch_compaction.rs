//! Compaction after a task switched to a model with a smaller context
//! window, as a Pi extension.
//!
//! Pi compacts with the session's model, in one request. After a switch to
//! a smaller window, the history to summarize was written for the previous
//! model and may not fit the new one. The model that wrote the last reply
//! held all of it, so it writes the summary instead. It is read from the
//! stored reply, so this holds across restarts. When the catalog no longer
//! offers that model, or its summary fails, the new model summarizes the
//! history trimmed to its own window, oldest messages first.
//!
//! Every other compaction is Pi's own. So is this one when the new model
//! can take the whole history, or when both attempts fail: the extension
//! then returns no summary.

use std::sync::Arc;

use futures_util::future::BoxFuture;
use pi_ai::{Message, Model, StreamOptions};
use pi_coding_agent::compaction::{
    CompactionPreparation, CompactionResult, Summarizer, compact, serialize_conversation,
};
use pi_coding_agent::extensions::{
    BeforeCompactResult, Extension, ExtensionContext, SessionBeforeCompact, extension,
};
use pi_coding_agent::messages::convert_to_llm;
use pi_coding_agent::session::{EntryKind, SessionEntry};
use pi_coding_agent::{ModelRegistry, SessionMessage};

/// A model as the account's catalog offers it to tasks, by id, or `None`
/// when the catalog does not offer it.
pub(crate) type OfferedModel =
    Arc<dyn Fn(String) -> BoxFuture<'static, Option<Model>> + Send + Sync>;

/// Room for a summary request's system prompt, instructions and framing.
const PROMPT_TOKENS: u64 = 2_000;

pub(crate) fn switch_compaction_extension(offered: OfferedModel) -> Arc<dyn Extension> {
    extension("maple-switch-compaction", move |api| {
        let offered = Arc::clone(&offered);
        api.on(
            move |event: SessionBeforeCompact, context: ExtensionContext| {
                let offered = Arc::clone(&offered);
                async move { Ok(compact_after_switch(event, &context, &offered).await) }
            },
        );
    })
}

async fn compact_after_switch(
    event: SessionBeforeCompact,
    context: &ExtensionContext,
    offered: &OfferedModel,
) -> BeforeCompactResult {
    let (Some(current), Some(models)) = (context.model(), context.models()) else {
        return BeforeCompactResult::default();
    };
    let Some(previous_id) = switched_from(&event.branch_entries, &current) else {
        return BeforeCompactResult::default();
    };
    let previous = offered(previous_id.clone()).await;
    // A switch to a window at least as large leaves nothing to fit.
    if previous
        .as_ref()
        .is_some_and(|previous| previous.context_window <= current.context_window)
    {
        return BeforeCompactResult::default();
    }
    let summaries = Summaries {
        models,
        context,
        event: &event,
    };
    match &previous {
        Some(previous) => match summaries.write(previous, &event.preparation).await {
            Ok(result) => {
                log::info!(
                    "Compacted after a switch from {} to {}: summarized by {}",
                    previous.id,
                    current.id,
                    previous.id
                );
                return summarized(result);
            }
            Err(error) => log::warn!(
                "Compaction after a switch to {}: {} could not summarize: {error}",
                current.id,
                previous.id
            ),
        },
        None => log::info!(
            "Compaction after a switch to {}: {previous_id} is no longer offered",
            current.id
        ),
    }
    if event.signal.is_cancelled() {
        return cancelled();
    }
    let Some(trimmed) = fit_preparation(
        &event.preparation,
        &current,
        event.custom_instructions.as_deref(),
    ) else {
        // The new model takes all of it, as Pi's own summary does.
        return BeforeCompactResult::default();
    };
    match summaries.write(&current, &trimmed).await {
        Ok(result) => {
            log::info!(
                "Compacted after a switch to {}: summarized by {} from {} of {} messages",
                current.id,
                current.id,
                trimmed.messages_to_summarize.len() + trimmed.turn_prefix_messages.len(),
                event.preparation.messages_to_summarize.len()
                    + event.preparation.turn_prefix_messages.len()
            );
            summarized(result)
        }
        Err(_) if event.signal.is_cancelled() => cancelled(),
        Err(error) => {
            log::warn!(
                "Compaction after a switch to {}: the trimmed summary failed: {error}",
                current.id
            );
            BeforeCompactResult::default()
        }
    }
}

fn summarized(result: CompactionResult) -> BeforeCompactResult {
    BeforeCompactResult {
        compaction: Some(result),
        ..BeforeCompactResult::default()
    }
}

fn cancelled() -> BeforeCompactResult {
    BeforeCompactResult {
        cancel: true,
        ..BeforeCompactResult::default()
    }
}

/// The model of the branch's last complete reply, when it is not `current`.
fn switched_from(branch: &[SessionEntry], current: &Model) -> Option<String> {
    let reply = branch.iter().rev().find_map(|entry| match &entry.kind {
        EntryKind::Message {
            message: SessionMessage::Llm(Message::Assistant(reply)),
        } if !reply.is_failure() => Some(reply),
        _ => None,
    })?;
    (reply.provider != current.provider || reply.model != current.id).then(|| reply.model.clone())
}

/// Summary requests made as Pi makes them, on a model of choice.
struct Summaries<'a> {
    models: ModelRegistry,
    context: &'a ExtensionContext,
    event: &'a SessionBeforeCompact,
}

impl Summaries<'_> {
    async fn write(
        &self,
        model: &Model,
        preparation: &CompactionPreparation,
    ) -> Result<CompactionResult, String> {
        let settings = self.context.settings();
        let stream_fn = self.models.stream_fn();
        let summarizer = Summarizer {
            model,
            stream_fn: &*stream_fn,
            options: StreamOptions {
                api_key: self.models.api_key(&model.provider).await,
                reasoning: Some(self.context.thinking_level()),
                session_id: self.context.session_id(),
                cancel: self.event.signal.clone(),
                ..StreamOptions::default()
            },
            max_retries: if settings.retry.enabled {
                settings.retry.max_retries
            } else {
                0
            },
            retry_base_ms: settings.retry.base_delay_ms,
            retry_max_ms: settings.retry.max_agent_delay_ms,
            on_retry: None,
        };
        compact(
            preparation,
            &summarizer,
            self.event.custom_instructions.as_deref(),
        )
        .await
    }
}

/// `preparation` with the oldest messages of each summary request left out
/// until the request fits `model`'s window with room for the summary, or
/// `None` when it fits whole.
fn fit_preparation(
    preparation: &CompactionPreparation,
    model: &Model,
    instructions: Option<&str>,
) -> Option<CompactionPreparation> {
    let budget = model
        .context_window
        .saturating_sub(preparation.settings.reserve_tokens)
        .saturating_sub(PROMPT_TOKENS)
        .saturating_sub(text_tokens(instructions.unwrap_or_default()));
    let history_budget = budget.saturating_sub(text_tokens(
        preparation.previous_summary.as_deref().unwrap_or_default(),
    ));
    let messages_to_summarize = newest_within(&preparation.messages_to_summarize, history_budget);
    let turn_prefix_messages = newest_within(&preparation.turn_prefix_messages, budget);
    let whole = messages_to_summarize.len() == preparation.messages_to_summarize.len()
        && turn_prefix_messages.len() == preparation.turn_prefix_messages.len();
    (!whole).then(|| CompactionPreparation {
        messages_to_summarize,
        turn_prefix_messages,
        ..preparation.clone()
    })
}

/// The newest of `messages` whose text, as a summary request shows it, fits
/// `budget` tokens.
fn newest_within(messages: &[SessionMessage], budget: u64) -> Vec<SessionMessage> {
    let mut total = 0u64;
    let start = messages
        .iter()
        .enumerate()
        .rev()
        .find(|(_, message)| {
            total = total.saturating_add(request_tokens(message));
            total > budget
        })
        .map_or(0, |(index, _)| index + 1);
    messages[start..].to_vec()
}

/// A message's share of a summary request: its rendered text and the
/// separator before it, estimated as Pi's compaction estimates.
fn request_tokens(message: &SessionMessage) -> u64 {
    text_tokens(&serialize_conversation(&convert_to_llm(
        std::slice::from_ref(message),
    ))) + 1
}

fn text_tokens(text: &str) -> u64 {
    text.chars().count().div_ceil(4) as u64
}

#[cfg(test)]
mod tests;
