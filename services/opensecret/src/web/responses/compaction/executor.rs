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
    pub async fn summarize(
        &self,
        work: &SummaryWork,
        policy: &CompactionPolicy,
    ) -> Result<String, ApiError> {
        let messages: Vec<_> = work
            .items
            .iter()
            .flat_map(|item| item.summary_messages.iter().cloned())
            .collect();
        let transcript =
            summary::serialize_transcript(&messages).map_err(|_| ApiError::InternalServerError)?;
        if transcript.is_empty() {
            return work
                .previous_summary
                .clone()
                .filter(|summary| !summary.trim().is_empty())
                .ok_or(ApiError::MessageExceedsContextLimit);
        }
        // A conservative byte-sized first partition is followed by request-size
        // estimates and a narrowly classified provider-overflow safety net.
        let fragment_bytes = policy.summary_input_budget.saturating_mul(2).max(4);
        let mut fragments: VecDeque<_> = summary::split_transcript(&transcript, fragment_bytes)
            .map_err(|_| ApiError::MessageExceedsContextLimit)?
            .into_iter()
            .map(|fragment| (fragment.to_owned(), 0usize))
            .collect();
        let mut previous = work.previous_summary.clone();
        while let Some((fragment, reductions)) = fragments.pop_front() {
            let messages = summary::summary_messages(&fragment, previous.as_deref());
            let estimate = count_tokens(
                &serde_json::to_string(&messages).map_err(|_| ApiError::InternalServerError)?,
            );
            if estimate > policy.summary_input_budget {
                split_front(&mut fragments, fragment, reductions)?;
                continue;
            }
            self.budget.begin_call()?;
            let request =
                build_compaction_model_request(self.body, &messages, policy.summary_output_tokens);
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
                _ = self.execution.cancelled() => return Err(ApiError::ServiceUnavailable),
                _ = tokio::time::sleep_until(self.deadline) => return Err(ApiError::ServiceUnavailable),
                response = response => response,
            };
            match response {
                Ok(response) => {
                    let next = summary::parse_summary_response(&response)
                        .map_err(|_| ApiError::ServiceUnavailable)?;
                    if count_tokens(&next) > policy.summary_output_tokens
                        || next.len() > policy.summary_output_tokens.saturating_mul(16)
                    {
                        return Err(ApiError::ServiceUnavailable);
                    }
                    previous = Some(next);
                }
                Err(error) if is_context_rejection(&error) => {
                    split_front(&mut fragments, fragment, reductions)?;
                }
                Err(error) => return Err(error.into_api_error()),
            }
        }
        previous.ok_or(ApiError::MessageExceedsContextLimit)
    }
}

fn split_front(
    fragments: &mut VecDeque<(String, usize)>,
    fragment: String,
    reductions: usize,
) -> Result<(), ApiError> {
    if reductions >= MAX_FRAGMENT_REDUCTIONS {
        return Err(ApiError::MessageExceedsContextLimit);
    }
    let mut middle = fragment.len() / 2;
    while middle > 0 && !fragment.is_char_boundary(middle) {
        middle -= 1;
    }
    if middle == 0 || middle == fragment.len() {
        return Err(ApiError::MessageExceedsContextLimit);
    }
    fragments.push_front((fragment[middle..].to_owned(), reductions + 1));
    fragments.push_front((fragment[..middle].to_owned(), reductions + 1));
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
