//! Paid summary execution through the existing model/provider boundary.
//! No tool is exposed or executed by these requests. A caller publishes coverage
//! only after every fragment has been successfully folded into the summary.

use super::{context::SummaryWork, policy::CompactionPolicy, summary, CompactionBudget};
use crate::{
    inference::{AttemptFailureKind, AttemptStage},
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

/// Keep advisory availability failures separate from conditions that must stop
/// the entire logical Response. Public errors never contain summary/source text.
#[derive(Debug)]
pub(super) enum SummaryExecutionError {
    Cancelled,
    Deadline,
    Capacity,
    InvalidSummary,
    Provider(Box<CompletionExecutionError>),
    Unsafe(ApiError),
}

impl SummaryExecutionError {
    pub(super) fn can_fallback_to_uncompacted(&self) -> bool {
        match self {
            Self::Capacity | Self::InvalidSummary => true,
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
        let pieces = summary::transcript_fragments(&messages, fragment_bytes)
            .map_err(|_| SummaryExecutionError::Unsafe(ApiError::InternalServerError))?;
        let mut fragments = fragment_batches(pieces, fragment_bytes);
        let mut previous = work.previous_summary.clone();
        while let Some((fragment, reductions)) = fragments.pop_front() {
            self.check_execution()?;
            let messages = summary::fragment_summary_messages(&fragment, previous.as_deref());
            let estimate = count_tokens(
                &serde_json::to_string(&messages)
                    .map_err(|_| SummaryExecutionError::Unsafe(ApiError::InternalServerError))?,
            );
            if estimate > policy.summary_input_budget {
                split_front(&mut fragments, fragment, reductions)?;
                continue;
            }
            self.budget
                .begin_call()
                .map_err(|_| SummaryExecutionError::Capacity)?;
            let request = build_compaction_model_request(self.body, &messages);
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
                .await?;
                match completion.stream.recv().await {
                    Some(CompletionChunk::FullResponse(response)) => Ok(response),
                    _ => Err(CompletionExecutionError::Request(
                        ApiError::ServiceUnavailable,
                    )),
                }
            };
            let response = tokio::select! {
                biased;
                _ = self.execution.cancelled() => return Err(SummaryExecutionError::Cancelled),
                _ = tokio::time::sleep_until(self.deadline) => return Err(SummaryExecutionError::Deadline),
                response = response => response,
            };
            match response {
                Ok(response) => {
                    let next = summary::parse_summary_response(&response)
                        .map_err(|_| SummaryExecutionError::InvalidSummary)?;
                    // Native model/provider generation limits are not cl100k
                    // limits. Keep only the independent untrusted byte bound.
                    if next.len() > MAX_SUMMARY_TEXT_BYTES {
                        return Err(SummaryExecutionError::InvalidSummary);
                    }
                    previous = Some(next);
                }
                Err(error) if is_context_rejection(&error) => {
                    split_front(&mut fragments, fragment, reductions)?;
                }
                Err(error) => return Err(SummaryExecutionError::Provider(Box::new(error))),
            }
        }
        previous.ok_or(SummaryExecutionError::Unsafe(
            ApiError::MessageExceedsContextLimit,
        ))
    }

    fn check_execution(&self) -> Result<(), SummaryExecutionError> {
        if self.execution.is_cancelled() {
            return Err(SummaryExecutionError::Cancelled);
        }
        if Instant::now() >= self.deadline {
            return Err(SummaryExecutionError::Deadline);
        }
        Ok(())
    }
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
    mut fragment: Vec<summary::TranscriptFragment>,
    reductions: usize,
) -> Result<(), SummaryExecutionError> {
    if reductions >= MAX_FRAGMENT_REDUCTIONS {
        return Err(SummaryExecutionError::Unsafe(
            ApiError::MessageExceedsContextLimit,
        ));
    }
    let (left, right) = if fragment.len() > 1 {
        let right = fragment.split_off(fragment.len() / 2);
        (fragment, right)
    } else {
        let piece = fragment.pop().ok_or(SummaryExecutionError::Unsafe(
            ApiError::MessageExceedsContextLimit,
        ))?;
        let (left, right) = piece
            .split()
            .map_err(|_| SummaryExecutionError::Unsafe(ApiError::MessageExceedsContextLimit))?;
        (vec![left], vec![right])
    };
    fragments.push_front((right, reductions + 1));
    fragments.push_front((left, reductions + 1));
    Ok(())
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
            Err(SummaryExecutionError::Unsafe(
                ApiError::MessageExceedsContextLimit
            ))
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
