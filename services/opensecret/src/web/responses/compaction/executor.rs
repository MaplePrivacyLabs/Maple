//! Paid summary execution through the existing model/provider boundary.
//! No tool is exposed or executed by these requests. A caller publishes coverage
//! only after every fragment has been successfully folded into the summary.

use super::{context::SummaryWork, policy::CompactionPolicy, summary, CompactionBudget};
use crate::{
    inference::{AttemptFailureKind, AttemptStage, AttemptTerminal},
    models::users::User,
    tokens::count_tokens,
    web::{
        openai::{
            get_bounded_chat_completion_response_for_execution, BillingContext,
            CompletionCachePolicy, CompletionChunk, CompletionExecutionContext,
            CompletionExecutionError, InferenceRoutingContext,
        },
        openai_auth::AuthMethod,
        responses::{
            handlers::{build_compaction_model_request, ResponsesCreateRequest},
            ResponseExecution,
        },
    },
    ApiError, AppState,
};
use axum::http::HeaderMap;
use std::{collections::VecDeque, sync::Arc};
use tokio::time::Instant;

const MAX_FRAGMENT_REDUCTIONS: usize = 3;
const MAX_SUMMARY_TEXT_BYTES: usize = 256 * 1024;

fn log_summary_rejection(
    error: summary::SummaryError,
    response: &serde_json::Value,
    user_id: uuid::Uuid,
    response_id: uuid::Uuid,
    elapsed_ms: u64,
) {
    tracing::warn!(event = "responses.compaction.helper_rejected", %user_id,
        %response_id, reason = error.category(), elapsed_ms,
        metadata = ?summary::response_metadata(response),
        "Compaction summary response was rejected");
}

fn log_summary_provider_failure(
    error: &CompletionExecutionError,
    user_id: uuid::Uuid,
    response_id: uuid::Uuid,
    elapsed_ms: u64,
) {
    // The wrapped error and upstream diagnostics may contain private strings.
    // Only implementation-owned enums, UUIDs and the numeric status escape.
    let (public_error, terminal) = match error {
        CompletionExecutionError::Request(error) => (error, None),
        CompletionExecutionError::Attempt {
            public_error,
            terminal,
        } => (public_error, Some(terminal)),
    };
    if let Some(crate::inference::AttemptTerminal::Failed { attempt, failure }) = terminal {
        tracing::warn!(event = "responses.compaction.helper_failed", %user_id, %response_id,
            request_id = %attempt.request_id, execution_id = %attempt.execution_id,
            attempt_id = %attempt.attempt_id, reason = super::telemetry::api_error_category(public_error),
            failure_kind = ?failure.kind, failure_stage = ?failure.stage, status = failure.status,
            replay_safety = ?failure.replay_safety, elapsed_ms,
            "Compaction summary provider call failed");
    } else {
        tracing::warn!(event = "responses.compaction.helper_failed", %user_id, %response_id,
            reason = super::telemetry::api_error_category(public_error), elapsed_ms,
            "Compaction summary provider call could not finish");
    }
}

/// Keep advisory availability failures separate from conditions that must stop
/// the entire logical Response. Public errors never contain summary/source text.
#[derive(Debug)]
pub(super) enum SummaryExecutionError {
    Cancelled,
    Deadline,
    Capacity,
    InputLimit,
    InvalidSummary,
    Provider(Box<CompletionExecutionError>),
    Unsafe(ApiError),
}

impl SummaryExecutionError {
    pub(super) fn category(&self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Deadline => "deadline",
            Self::Capacity => "helper_call_limit",
            Self::InputLimit => "helper_input_limit",
            Self::InvalidSummary => "invalid_summary",
            Self::Provider(_) => "provider_failure",
            Self::Unsafe(_) => "safety_failure",
        }
    }

    pub(super) fn can_fallback_to_uncompacted(&self) -> bool {
        match self {
            Self::Capacity | Self::InputLimit | Self::InvalidSummary => true,
            Self::Provider(error) => match error.as_ref() {
                CompletionExecutionError::Request(ApiError::ServiceUnavailable)
                | CompletionExecutionError::Request(ApiError::InferenceCapacity { .. }) => true,
                CompletionExecutionError::Attempt {
                    terminal: crate::inference::AttemptTerminal::Failed { failure, .. },
                    public_error,
                } => {
                    if matches!(
                        public_error,
                        ApiError::Unauthorized
                            | ApiError::UsageLimitReached
                            | ApiError::FreeTokenLimitExceeded
                            | ApiError::ModelNotAvailableOnPlan
                            | ApiError::ModelNotAvailable
                    ) {
                        return false;
                    }
                    match failure.kind {
                        AttemptFailureKind::Connect
                        | AttemptFailureKind::Transport
                        | AttemptFailureKind::ResponseStartTimeout
                        | AttemptFailureKind::ResponseBody
                        | AttemptFailureKind::InvalidResponse
                        | AttemptFailureKind::StreamTimeout
                        | AttemptFailureKind::UpstreamResponseError
                        | AttemptFailureKind::UpstreamStreamError
                        | AttemptFailureKind::UnexpectedEof
                        | AttemptFailureKind::CapacityRejected
                        | AttemptFailureKind::ProviderUnavailable => true,
                        AttemptFailureKind::HttpStatus => {
                            matches!(failure.status, Some(408 | 429 | 500..=599))
                        }
                        AttemptFailureKind::RequestBuild | AttemptFailureKind::ConsumerDropped => {
                            false
                        }
                    }
                }
                _ => false,
            },
            Self::Cancelled | Self::Deadline | Self::Unsafe(_) => false,
        }
    }

    pub(super) fn into_api_error(self) -> ApiError {
        match self {
            Self::Provider(error) => (*error).into_api_error(),
            Self::Unsafe(error) => error,
            Self::InputLimit => ApiError::MessageExceedsContextLimit,
            Self::Cancelled | Self::Deadline | Self::Capacity | Self::InvalidSummary => {
                ApiError::ServiceUnavailable
            }
        }
    }
}

/// A narrow input-validation rejection can safely authorize a smaller prompt.
/// It never authorizes replay of an accepted stream, a tool or a generic error.
pub(crate) fn is_context_rejection(error: &CompletionExecutionError) -> bool {
    let Some(crate::inference::AttemptTerminal::Failed { failure, .. }) = error.terminal() else {
        return false;
    };
    failure.kind == AttemptFailureKind::HttpStatus
        && failure.stage == AttemptStage::AwaitingResponse
        && failure.status == Some(400)
        && failure
            .upstream_diagnostic
            .as_deref()
            .is_some_and(|diagnostic| diagnostic.is_input_context_overflow())
}

pub(crate) struct SummaryExecutor<'a> {
    pub state: &'a Arc<AppState>,
    pub user: &'a User,
    pub body: &'a ResponsesCreateRequest,
    pub routing: InferenceRoutingContext,
    pub cache_policy: &'a CompletionCachePolicy,
    pub execution: &'a ResponseExecution,
    pub budget: &'a CompactionBudget,
    pub deadline: Instant,
}

impl SummaryExecutor<'_> {
    pub(super) async fn summarize(
        &self,
        work: &SummaryWork,
        policy: &CompactionPolicy,
    ) -> Result<String, SummaryExecutionError> {
        let started = Instant::now();
        self.check_execution()?;
        let messages: Vec<_> = work
            .items
            .iter()
            .flat_map(|item| item.summary_messages.iter().cloned())
            .collect();
        if messages.is_empty() {
            return Ok(summary::identity_only_checkpoint(
                work.previous_summary.as_deref(),
            ));
        }
        // A conservative byte-sized first partition is followed by request-size
        // estimates and a narrowly classified provider-overflow safety net.
        let fragment_bytes = policy.summary_input_budget.saturating_mul(2).max(4);
        let pieces = summary::transcript_fragments(&messages, fragment_bytes).map_err(|error| {
            tracing::warn!(event = "responses.compaction.helper_failed", user_id = %self.user.uuid,
                response_id = %self.execution.response_id(), reason = error.category(),
                stage = "transcript_preparation", "Compaction summary transcript could not be prepared");
            SummaryExecutionError::Unsafe(ApiError::InternalServerError)
        })?;
        let mut fragments = fragment_batches(pieces, fragment_bytes);
        tracing::info!(event = "responses.compaction.helper_batch_started", user_id = %self.user.uuid,
            response_id = %self.execution.response_id(), source_items = work.items.len(),
            source_messages = messages.len(), fragments = fragments.len(), input_budget = policy.summary_input_budget,
            previous_summary_present = work.previous_summary.is_some(), "Compaction summary batch started");
        let mut previous = work.previous_summary.clone();
        let mut completed_fragments = 0usize;
        while let Some((fragment, reductions)) = fragments.pop_front() {
            self.check_execution()?;
            let messages = summary::fragment_summary_messages(&fragment, previous.as_deref());
            let estimate = count_tokens(
                &serde_json::to_string(&messages)
                    .map_err(|_| SummaryExecutionError::Unsafe(ApiError::InternalServerError))?,
            );
            if estimate > policy.summary_input_budget {
                tracing::debug!(event = "responses.compaction.helper_fragment_split", user_id = %self.user.uuid,
                    response_id = %self.execution.response_id(), estimated_tokens = estimate,
                    input_budget = policy.summary_input_budget, reductions, reason = "estimated_input_limit",
                    "Compaction summary fragment needs a smaller input");
                split_front_for_estimate(&mut fragments, fragment, reductions)?;
                continue;
            }
            self.budget
                .begin_call()
                .map_err(|_| SummaryExecutionError::Capacity)?;
            let request = build_compaction_model_request(self.body, &messages);
            let call_started = Instant::now();
            tracing::info!(event = "responses.compaction.helper_call_started", user_id = %self.user.uuid,
                response_id = %self.execution.response_id(), fragment_index = completed_fragments,
                fragments_pending = fragments.len(), estimated_tokens = estimate, reductions,
                "Compaction summary provider call started");
            let billing = BillingContext::new(AuthMethod::Jwt, self.body.model.clone());
            let execution =
                CompletionExecutionContext::new(billing, self.routing, self.cache_policy);
            // Once helper execution starts, the enclosing HTTP request must not
            // be labelled safe for automatic replay, even if preparation fails.
            let response = async {
                let mut completion = get_bounded_chat_completion_response_for_execution(
                    self.state,
                    self.user,
                    request,
                    &HeaderMap::new(),
                    execution,
                    self.execution.clone(),
                )
                .await
                .map_err(|error| SummaryExecutionError::Provider(Box::new(error)))?;
                collect_summary_stream(&mut completion.stream).await
            };
            let response = tokio::select! {
                biased;
                _ = self.execution.cancelled() => {
                    tracing::info!(event = "responses.compaction.helper_cancelled", user_id = %self.user.uuid,
                        response_id = %self.execution.response_id(), stage = "awaiting_provider",
                        elapsed_ms = call_started.elapsed().as_millis() as u64,
                        "Compaction summary provider call cancelled");
                    return Err(SummaryExecutionError::Cancelled);
                },
                _ = tokio::time::sleep_until(self.deadline) => {
                    tracing::warn!(event = "responses.compaction.helper_deadline", user_id = %self.user.uuid,
                        response_id = %self.execution.response_id(), stage = "awaiting_provider",
                        elapsed_ms = call_started.elapsed().as_millis() as u64,
                        "Compaction summary provider call reached its deadline");
                    return Err(SummaryExecutionError::Deadline);
                },
                response = response => response,
            };
            match response {
                Ok(response) => {
                    let next = summary::parse_summary_response(&response).map_err(|error| {
                        log_summary_rejection(
                            error,
                            &response,
                            self.user.uuid,
                            self.execution.response_id(),
                            call_started.elapsed().as_millis() as u64,
                        );
                        SummaryExecutionError::InvalidSummary
                    })?;
                    // Native model/provider generation limits are not cl100k
                    // limits. Keep only the independent untrusted byte bound.
                    if next.len() > MAX_SUMMARY_TEXT_BYTES {
                        tracing::warn!(event = "responses.compaction.helper_rejected", user_id = %self.user.uuid,
                            response_id = %self.execution.response_id(), reason = "output_byte_limit",
                            output_bytes = next.len(), output_byte_limit = MAX_SUMMARY_TEXT_BYTES,
                            "Compaction summary response exceeded its byte limit");
                        return Err(SummaryExecutionError::InvalidSummary);
                    }
                    if !summary_fits_next_helper_input(&next, policy) {
                        tracing::warn!(event = "responses.compaction.helper_rejected", user_id = %self.user.uuid,
                            response_id = %self.execution.response_id(), reason = "next_helper_input_limit",
                            output_bytes = next.len(), input_budget = policy.summary_input_budget,
                            "Compaction summary cannot fit a later bounded helper input");
                        return Err(SummaryExecutionError::InvalidSummary);
                    }
                    tracing::info!(event = "responses.compaction.helper_call_completed", user_id = %self.user.uuid,
                        response_id = %self.execution.response_id(), fragment_index = completed_fragments,
                        output_bytes = next.len(), elapsed_ms = call_started.elapsed().as_millis() as u64,
                        metadata = ?summary::response_metadata(&response),
                        "Compaction summary provider call completed");
                    completed_fragments += 1;
                    previous = Some(next);
                }
                Err(SummaryExecutionError::Provider(error)) if is_context_rejection(&error) => {
                    tracing::warn!(event = "responses.compaction.helper_fragment_split", user_id = %self.user.uuid,
                        response_id = %self.execution.response_id(), reason = "provider_input_limit", reductions,
                        elapsed_ms = call_started.elapsed().as_millis() as u64,
                        "Compaction summary provider rejected the input size");
                    split_front(&mut fragments, fragment, reductions)?;
                }
                Err(SummaryExecutionError::Provider(error)) => {
                    log_summary_provider_failure(
                        &error,
                        self.user.uuid,
                        self.execution.response_id(),
                        call_started.elapsed().as_millis() as u64,
                    );
                    return Err(SummaryExecutionError::Provider(error));
                }
                Err(error) => return Err(error),
            }
        }
        tracing::info!(event = "responses.compaction.helper_batch_completed", user_id = %self.user.uuid,
            response_id = %self.execution.response_id(), completed_fragments,
            elapsed_ms = started.elapsed().as_millis() as u64, "Compaction summary batch completed");
        previous.ok_or(SummaryExecutionError::Unsafe(
            ApiError::MessageExceedsContextLimit,
        ))
    }

    fn check_execution(&self) -> Result<(), SummaryExecutionError> {
        if self.execution.is_cancelled() {
            tracing::info!(event = "responses.compaction.helper_cancelled", user_id = %self.user.uuid,
                response_id = %self.execution.response_id(), stage = "before_fragment",
                "Compaction summary execution cancelled");
            return Err(SummaryExecutionError::Cancelled);
        }
        if Instant::now() >= self.deadline {
            tracing::warn!(event = "responses.compaction.helper_deadline", user_id = %self.user.uuid,
                response_id = %self.execution.response_id(), stage = "before_fragment",
                "Compaction summary execution reached its deadline");
            return Err(SummaryExecutionError::Deadline);
        }
        Ok(())
    }
}

fn invalid_stream_summary(error: summary::SummaryError) -> SummaryExecutionError {
    tracing::warn!(
        event = "responses.compaction.helper_rejected",
        reason = error.category(),
        "Compaction summary stream was rejected"
    );
    SummaryExecutionError::InvalidSummary
}

async fn collect_summary_stream(
    stream: &mut tokio::sync::mpsc::Receiver<CompletionChunk>,
) -> Result<serde_json::Value, SummaryExecutionError> {
    let mut summary = summary::SummaryStreamAccumulator::new(MAX_SUMMARY_TEXT_BYTES);
    while let Some(chunk) = stream.recv().await {
        match chunk {
            CompletionChunk::StreamChunk(chunk) => {
                summary.observe(&chunk).map_err(invalid_stream_summary)?;
            }
            // The existing completion processor owns usage publication. Wait
            // for its terminal after billing, rather than treating finish_reason
            // or a partial answer as a completed, durable summary.
            CompletionChunk::Usage(_) => {}
            CompletionChunk::Terminal(AttemptTerminal::Failed { attempt, failure }) => {
                let public_error =
                    crate::web::provider_error::PublicProviderError::from_failure(&failure)
                        .map(ApiError::InferenceProvider)
                        .unwrap_or(ApiError::InternalServerError);
                return Err(SummaryExecutionError::Provider(Box::new(
                    CompletionExecutionError::Attempt {
                        terminal: AttemptTerminal::Failed { attempt, failure },
                        public_error,
                    },
                )));
            }
            CompletionChunk::Terminal(AttemptTerminal::Completed { .. }) => {
                return summary.into_response().map_err(invalid_stream_summary);
            }
            CompletionChunk::FullResponse(_) => {
                return Err(invalid_stream_summary(
                    summary::SummaryError::InvalidResponse,
                ));
            }
        }
    }
    Err(invalid_stream_summary(
        summary::SummaryError::IncompleteResponse,
    ))
}

/// A generation allowance is not a helper-input budget. Before publishing a
/// checkpoint, ensure that a later update can carry this whole summary plus the
/// existing instructions and a useful fragment of new transcript. This check
/// never changes the provider's output limit or the caller's answer allowance.
fn summary_fits_next_helper_input(text: &str, policy: &CompactionPolicy) -> bool {
    const NEXT_FRAGMENT_RESERVE_TOKENS: usize = 512;
    let request = summary::fragment_summary_messages(&[], Some(text));
    serde_json::to_string(&request)
        .map(|request| {
            count_tokens(&request).saturating_add(NEXT_FRAGMENT_RESERVE_TOKENS)
                <= policy.summary_input_budget
        })
        .unwrap_or(false)
}

fn fragment_batches(
    pieces: Vec<summary::TranscriptFragment>,
    max_bytes: usize,
) -> VecDeque<(Vec<summary::TranscriptFragment>, usize)> {
    let mut batches = VecDeque::new();
    let mut current = Vec::new();
    let mut bytes = 0usize;
    for piece in pieces {
        let next_bytes = piece.serialized_bytes();
        if !current.is_empty() && bytes.saturating_add(next_bytes) > max_bytes {
            batches.push_back((std::mem::take(&mut current), 0));
            bytes = 0;
        }
        current.push(piece);
        bytes = bytes.saturating_add(next_bytes);
    }
    if !current.is_empty() {
        batches.push_back((current, 0));
    }
    batches
}

fn split_front(
    fragments: &mut VecDeque<(Vec<summary::TranscriptFragment>, usize)>,
    fragment: Vec<summary::TranscriptFragment>,
    reductions: usize,
) -> Result<(), SummaryExecutionError> {
    if reductions >= MAX_FRAGMENT_REDUCTIONS {
        return Err(SummaryExecutionError::InputLimit);
    }
    let (left, right) = split_fragment(fragment)?;
    fragments.push_front((right, reductions + 1));
    fragments.push_front((left, reductions + 1));
    Ok(())
}

/// A growing previous summary changes the useful source fragment size. Pure
/// input fitting is lossless and spends no provider rejection/retry allowance:
/// every split strictly reduces item count or UTF-8 text length, so it terminates
/// at a fitting fragment or a minimal fragment that cannot fit the instructions.
fn split_front_for_estimate(
    fragments: &mut VecDeque<(Vec<summary::TranscriptFragment>, usize)>,
    fragment: Vec<summary::TranscriptFragment>,
    provider_reductions: usize,
) -> Result<(), SummaryExecutionError> {
    let (left, right) = split_fragment(fragment)?;
    fragments.push_front((right, provider_reductions));
    fragments.push_front((left, provider_reductions));
    Ok(())
}

fn split_fragment(
    mut fragment: Vec<summary::TranscriptFragment>,
) -> Result<
    (
        Vec<summary::TranscriptFragment>,
        Vec<summary::TranscriptFragment>,
    ),
    SummaryExecutionError,
> {
    let (left, right) = if fragment.len() > 1 {
        let right = fragment.split_off(fragment.len() / 2);
        (fragment, right)
    } else {
        let piece = fragment.pop().ok_or(SummaryExecutionError::InputLimit)?;
        let (left, right) = piece
            .split()
            .map_err(|_| SummaryExecutionError::InputLimit)?;
        (vec![left], vec![right])
    };
    Ok((left, right))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        inference::{
            AttemptFailure, AttemptTerminal, CompletionEvidence, InferenceAttempt, InferenceIntent,
            InferenceSurface, ReplaySafety, RouteIdentity, WorkloadClass,
        },
        model_config::ModelPlan,
        provider_client::{ProviderResponse, UpstreamDiagnostic},
        provider_registry::{ProviderId, RouteSelectionSource},
    };
    use serde_json::json;
    use uuid::Uuid;

    fn summary_stream_chunk(content: &str, finish: serde_json::Value) -> CompletionChunk {
        CompletionChunk::StreamChunk(json!({"choices": [{"index": 0, "delta": {
            "role": "assistant", "content": content,
        }, "finish_reason": finish}]}))
    }

    #[tokio::test]
    async fn summary_stream_waits_for_successful_terminal_after_finish_and_usage() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        tx.send(summary_stream_chunk("Visible summary", json!("stop")))
            .await
            .unwrap();
        let collecting = tokio::spawn(async move { collect_summary_stream(&mut rx).await });
        tokio::task::yield_now().await;
        assert!(!collecting.is_finished());
        tx.send(CompletionChunk::Usage(
            crate::web::openai::CompletionUsage {
                prompt_tokens: 10,
                completion_tokens: 20,
                cached_prompt_tokens: None,
            },
        ))
        .await
        .unwrap();
        tx.send(CompletionChunk::Terminal(AttemptTerminal::Completed {
            attempt: attempt(),
            evidence: CompletionEvidence::ProviderDone,
        }))
        .await
        .unwrap();
        let result = collecting.await.unwrap().unwrap();
        assert_eq!(
            summary::parse_summary_response(&result).unwrap(),
            "Visible summary"
        );
    }

    #[tokio::test]
    async fn summary_stream_does_not_publish_finished_text_after_failed_or_missing_terminal() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        tx.send(summary_stream_chunk("partial", json!("stop")))
            .await
            .unwrap();
        tx.send(CompletionChunk::Terminal(AttemptTerminal::Failed {
            attempt: attempt(),
            failure: AttemptFailure::new(
                AttemptFailureKind::UnexpectedEof,
                AttemptStage::Stream,
                ReplaySafety::NotProvenPreAcceptance,
            ),
        }))
        .await
        .unwrap();
        drop(tx);
        assert!(matches!(
            collect_summary_stream(&mut rx).await,
            Err(SummaryExecutionError::Provider(_))
        ));

        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tx.send(summary_stream_chunk("partial", json!("stop")))
            .await
            .unwrap();
        drop(tx);
        assert!(matches!(
            collect_summary_stream(&mut rx).await,
            Err(SummaryExecutionError::InvalidSummary)
        ));
    }

    #[tokio::test]
    async fn summary_stream_rejects_full_response_and_oversized_visible_output() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tx.send(CompletionChunk::FullResponse(json!({"choices": []})))
            .await
            .unwrap();
        assert!(matches!(
            collect_summary_stream(&mut rx).await,
            Err(SummaryExecutionError::InvalidSummary)
        ));
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tx.send(summary_stream_chunk(
            &"x".repeat(MAX_SUMMARY_TEXT_BYTES + 1),
            json!("stop"),
        ))
        .await
        .unwrap();
        assert!(matches!(
            collect_summary_stream(&mut rx).await,
            Err(SummaryExecutionError::InvalidSummary)
        ));
    }

    #[test]
    fn checkpoint_must_fit_a_later_helper_input_without_capping_generation() {
        let policy = CompactionPolicy::from_context_window(20_000, 0, 2048);
        assert!(summary_fits_next_helper_input(
            "A useful compact checkpoint.",
            &policy
        ));
        let too_large = "historical fact ".repeat(10_000);
        assert!(too_large.len() < MAX_SUMMARY_TEXT_BYTES);
        assert!(!summary_fits_next_helper_input(&too_large, &policy));
        let no_room_for_instructions = CompactionPolicy {
            summary_input_budget: 512,
            ..policy
        };
        assert!(!summary_fits_next_helper_input(
            "small",
            &no_room_for_instructions
        ));
    }

    #[test]
    fn grown_summary_repartitions_large_new_input_without_spending_provider_reductions() {
        let policy = CompactionPolicy::from_context_window(20_000, 0, 2048);
        let previous = "fact ".repeat(8_000);
        assert!(summary_fits_next_helper_input(&previous, &policy));
        let message = json!({"role": "tool", "tool_call_id": "call_1", "content": "new data ".repeat(20_000)});
        let original = summary::serialize_transcript(std::slice::from_ref(&message)).unwrap();
        let pieces = summary::transcript_fragments(&[message], usize::MAX).unwrap();
        let mut pending = fragment_batches(pieces, usize::MAX);
        let mut splits = 0;
        let fitted = loop {
            let (fragment, reductions) = pending.pop_front().unwrap();
            assert_eq!(reductions, 0);
            let request = summary::fragment_summary_messages(&fragment, Some(&previous));
            if count_tokens(&serde_json::to_string(&request).unwrap())
                <= policy.summary_input_budget
            {
                break fragment;
            }
            split_front_for_estimate(&mut pending, fragment, reductions).unwrap();
            splits += 1;
        };
        assert!(
            splits > MAX_FRAGMENT_REDUCTIONS,
            "pure fitting must exceed the old paid-rejection split allowance"
        );
        let mut all = fitted;
        for (fragment, reductions) in pending {
            assert_eq!(reductions, 0);
            all.extend(fragment);
        }
        let request = summary::fragment_summary_messages(&all, None);
        let data: serde_json::Value =
            serde_json::from_str(request[1]["content"].as_str().unwrap()).unwrap();
        let restored: String = data["conversation"]
            .as_array()
            .unwrap()
            .iter()
            .map(|piece| piece["serialized_message_fragment"].as_str().unwrap())
            .collect();
        assert_eq!(restored, original);
    }

    #[test]
    fn rejected_summary_logs_only_allowlisted_metadata() {
        let user_id = Uuid::new_v4();
        let response_id = Uuid::new_v4();
        let response = json!({
            "id": "PRIVATE_PROVIDER_ID", "error": {"message": "PRIVATE_PROVIDER_ERROR"},
            "choices": [{"finish_reason": "PRIVATE_FINISH_REASON", "message": {
                "role": "PRIVATE_ROLE", "content": "PRIVATE_SUMMARY_AND_USER_DATA",
                "reasoning": "PRIVATE_REASONING", "refusal": "PRIVATE_REFUSAL",
                "tool_calls": [{"id": "PRIVATE_TOOL_ID", "function": {"name": "PRIVATE_TOOL_NAME", "arguments": "PRIVATE_TOOL_ARGUMENTS"}}]
            }}]
        });
        let text = super::super::telemetry::capture_logs(|| {
            log_summary_rejection(
                summary::SummaryError::IncompleteResponse,
                &response,
                user_id,
                response_id,
                42,
            );
        });
        assert!(text.contains("responses.compaction.helper_rejected"));
        assert!(text.contains("incomplete_response"));
        assert!(text.contains(&user_id.to_string()));
        assert!(text.contains(&response_id.to_string()));
        assert!(text.contains("elapsed_ms=42"));
        assert!(text.contains("finish: \"other\""));
        assert!(
            !text.contains("PRIVATE_"),
            "provider/user data escaped metadata filtering"
        );
    }

    #[test]
    fn provider_failure_logs_keep_correlation_without_upstream_strings() {
        let failure = AttemptFailure::new(
            AttemptFailureKind::HttpStatus,
            AttemptStage::AwaitingResponse,
            ReplaySafety::NotProvenPreAcceptance,
        )
        .with_upstream_response(503, None, Some("PRIVATE_UPSTREAM_ID".to_owned()));
        let error = failed(failure);
        let user_id = Uuid::new_v4();
        let response_id = Uuid::new_v4();
        let text = super::super::telemetry::capture_logs(|| {
            log_summary_provider_failure(&error, user_id, response_id, 57);
        });
        assert!(text.contains("responses.compaction.helper_failed"));
        assert!(text.contains("failure_kind=HttpStatus"));
        assert!(text.contains("status=503"));
        assert!(text.contains(&response_id.to_string()));
        assert!(!text.contains("PRIVATE_"));
    }

    const INPUT_OVERFLOW: &str = "This model's maximum context length is 100 tokens. However, you requested 1 output tokens and your prompt contains 100 input tokens, for a total of 101 tokens. Please reduce the length of the input prompt or the number of requested output tokens. (parameter=input_tokens, value=100)";

    async fn diagnostic(body: impl Into<String>) -> UpstreamDiagnostic {
        // Exercise the same bounded public response reader as production;
        // private diagnostic fields cannot manufacture the recovery signal.
        let response = axum::http::Response::builder()
            .status(400)
            .body(reqwest::Body::from(body.into()))
            .unwrap()
            .into();
        ProviderResponse::Standard(response)
            .error_diagnostic()
            .await
    }

    async fn strict_diagnostic() -> UpstreamDiagnostic {
        diagnostic(
            json!({"error": {
                "code": 400,
                "type": "BadRequestError",
                "param": "input_tokens",
                "message": INPUT_OVERFLOW
            }})
            .to_string(),
        )
        .await
    }

    fn attempt() -> InferenceAttempt {
        InferenceIntent::new(
            Uuid::nil(),
            "deepseek-v4-1-flash",
            "deepseek-v4-1-flash",
            ModelPlan::Paid,
            InferenceSurface::Responses,
            WorkloadClass::Interactive,
        )
        .begin_execution()
        .begin_attempt(RouteIdentity::new(
            ProviderId::Tinfoil,
            "deepseek-v4-1-flash",
            "deepseek-v4-1-flash",
            "deepseek-v4-1-flash",
            RouteSelectionSource::StaticSplit,
            None,
        ))
    }

    fn failed(failure: AttemptFailure) -> CompletionExecutionError {
        CompletionExecutionError::Attempt {
            terminal: AttemptTerminal::Failed {
                attempt: attempt(),
                failure,
            },
            public_error: ApiError::ServiceUnavailable,
        }
    }

    fn http_failure(diagnostic: Option<UpstreamDiagnostic>) -> AttemptFailure {
        AttemptFailure::new(
            AttemptFailureKind::HttpStatus,
            AttemptStage::AwaitingResponse,
            ReplaySafety::NotProvenPreAcceptance,
        )
        .with_upstream_response(400, None, None)
        .with_upstream_diagnostic(diagnostic)
    }

    #[test]
    fn advisory_fallback_excludes_cancellation_deadline_authorization_and_safety_failures() {
        for error in [
            SummaryExecutionError::Cancelled,
            SummaryExecutionError::Deadline,
            SummaryExecutionError::Unsafe(ApiError::InternalServerError),
            SummaryExecutionError::Unsafe(ApiError::MessageExceedsContextLimit),
            SummaryExecutionError::Provider(Box::new(CompletionExecutionError::Request(
                ApiError::Unauthorized,
            ))),
            SummaryExecutionError::Provider(Box::new(CompletionExecutionError::Request(
                ApiError::UsageLimitReached,
            ))),
            SummaryExecutionError::Provider(Box::new(CompletionExecutionError::Request(
                ApiError::ModelNotAvailableOnPlan,
            ))),
        ] {
            assert!(!error.can_fallback_to_uncompacted());
        }
        for error in [
            SummaryExecutionError::Capacity,
            SummaryExecutionError::InputLimit,
            SummaryExecutionError::InvalidSummary,
            SummaryExecutionError::Provider(Box::new(CompletionExecutionError::Request(
                ApiError::ServiceUnavailable,
            ))),
        ] {
            assert!(error.can_fallback_to_uncompacted());
        }
        for status in [400, 401, 403, 404, 413, 422] {
            let mut failure = http_failure(None);
            failure.status = Some(status);
            assert!(!SummaryExecutionError::Provider(Box::new(failed(failure)))
                .can_fallback_to_uncompacted());
        }
        for status in [408, 429, 500, 502, 503, 504] {
            let mut failure = http_failure(None);
            failure.status = Some(status);
            assert!(SummaryExecutionError::Provider(Box::new(failed(failure)))
                .can_fallback_to_uncompacted());
        }
    }

    #[test]
    fn batches_keep_small_messages_together_and_reductions_keep_every_fragment() {
        let messages: Vec<_> = (0..20)
            .map(|index| json!({ "role": "user", "content": format!("Fact {index}") }))
            .collect();
        let pieces = summary::transcript_fragments(&messages, 20_000).unwrap();
        let mut batches = fragment_batches(pieces, 20_000);
        assert_eq!(batches.len(), 1);
        let (whole, reductions) = batches.pop_front().unwrap();
        assert_eq!(whole.len(), messages.len());
        let original = summary::fragment_summary_messages(&whole, None);
        split_front(&mut batches, whole, reductions).unwrap();
        assert_eq!(batches.len(), 2);
        let reduced: Vec<_> = batches
            .into_iter()
            .flat_map(|(pieces, reduction)| {
                assert_eq!(reduction, 1);
                pieces
            })
            .collect();
        assert_eq!(summary::fragment_summary_messages(&reduced, None), original);
        let (fragment, _) = fragment_batches(
            summary::transcript_fragments(&[json!({ "role": "tool", "tool_call_id": "call_1", "content": "€🙂 tool output" })], 20_000).unwrap(),
            20_000,
        )
        .pop_front()
        .unwrap();
        let mut reduced = VecDeque::new();
        split_front(&mut reduced, fragment, 0).unwrap();
        assert_eq!(reduced.len(), 2);
        for (pieces, reduction) in &reduced {
            assert_eq!(*reduction, 1);
            let request = summary::fragment_summary_messages(pieces, None);
            let data: serde_json::Value =
                serde_json::from_str(request[1]["content"].as_str().unwrap()).unwrap();
            assert_eq!(data["conversation"][0]["provenance"]["role"], "tool");
            assert_eq!(
                data["conversation"][0]["provenance"]["tool_call_id"],
                "call_1"
            );
        }
        let (fragment, _) = reduced.pop_front().unwrap();
        assert!(matches!(
            split_front(&mut reduced, fragment, MAX_FRAGMENT_REDUCTIONS),
            Err(SummaryExecutionError::InputLimit)
        ));
    }

    #[tokio::test]
    async fn context_rejection_requires_every_http_validation_boundary() {
        let diagnostic = strict_diagnostic().await;
        assert!(diagnostic.is_input_context_overflow());
        let exact = http_failure(Some(diagnostic));
        assert!(is_context_rejection(&failed(exact.clone())));

        for status in [
            None,
            Some(200),
            Some(401),
            Some(408),
            Some(422),
            Some(429),
            Some(500),
            Some(503),
        ] {
            let mut failure = exact.clone();
            failure.status = status;
            assert!(!is_context_rejection(&failed(failure)));
        }
        for kind in [
            AttemptFailureKind::Connect,
            AttemptFailureKind::Transport,
            AttemptFailureKind::ResponseStartTimeout,
            AttemptFailureKind::UpstreamStreamError,
        ] {
            let mut failure = exact.clone();
            failure.kind = kind;
            assert!(!is_context_rejection(&failed(failure)));
        }
        assert!(!is_context_rejection(&failed(http_failure(None))));
    }

    #[tokio::test]
    async fn accepted_response_and_stream_failures_never_authorize_reactive_compaction() {
        let exact = http_failure(Some(strict_diagnostic().await));
        for stage in [
            AttemptStage::BeforeSend,
            AttemptStage::ResponseBody,
            AttemptStage::Stream,
        ] {
            let mut failure = exact.clone();
            failure.stage = stage;
            assert!(!is_context_rejection(&failed(failure)));
        }
        let completed = CompletionExecutionError::Attempt {
            terminal: AttemptTerminal::Completed {
                attempt: attempt(),
                evidence: CompletionEvidence::ProviderDone,
            },
            public_error: ApiError::ServiceUnavailable,
        };
        assert!(!is_context_rejection(&completed));
        assert!(!is_context_rejection(&CompletionExecutionError::Request(
            ApiError::MessageExceedsContextLimit,
        )));
    }

    #[tokio::test]
    async fn codes_summaries_and_output_limits_cannot_authorize_reactive_compaction() {
        for error in [
            json!({"code": "context_length_exceeded"}),
            json!({"code": "max_tokens_limit_exceeded", "message": INPUT_OVERFLOW}),
            json!({"message": "maximum context length exceeded"}),
            json!({"message": "prompt is too long"}),
            json!({"message": "max_tokens must be at most 32768"}),
            json!({"message": "This model's maximum context length is 100 tokens. However, you requested 100 output tokens and your prompt contains 1 input tokens, for a total of 101 tokens. Please reduce the length of the input prompt or the number of requested output tokens."}),
            json!({"message": INPUT_OVERFLOW.replace("total of 101", "total of 102")}),
            json!({"message": format!("{INPUT_OVERFLOW} private-prompt-canary")}),
        ] {
            let diagnostic = diagnostic(json!({"error": error}).to_string()).await;
            assert!(!is_context_rejection(&failed(http_failure(Some(
                diagnostic
            )))));
        }
    }

    #[tokio::test]
    async fn malformed_or_truncated_diagnostics_cannot_authorize_reactive_compaction() {
        let valid = json!({"error": {"message": INPUT_OVERFLOW}}).to_string();
        for body in [
            String::new(),
            "not-json-private-prompt-canary".to_owned(),
            valid[..valid.len() - 1].to_owned(),
            format!("{valid}{}", " ".repeat(8 * 1024)),
        ] {
            let diagnostic = diagnostic(body).await;
            assert!(!is_context_rejection(&failed(http_failure(Some(
                diagnostic
            )))));
        }
    }
}
