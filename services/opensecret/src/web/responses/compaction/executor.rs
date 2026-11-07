//! Paid summary execution through the existing model/provider boundary.
//! No tool is exposed or executed by these requests. One request summarizes one
//! bounded selection; a caller publishes coverage only after that request succeeds.

use super::{context::SummaryWork, policy::CompactionPolicy, summary, CompactionBudget};
use crate::web::responses::{constants::TRUNCATION_MARKER, context_observability::ContextMetrics};
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
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::time::Instant;

const MAX_SUMMARY_TEXT_BYTES: usize = 256 * 1024;

/// Separates how long one helper's upstream took from total preparation time,
/// including for a helper still pending when a deadline or cancellation stops it.
/// A value of zero means that stage was never reached. Numeric metadata only.
struct HelperTiming {
    user_id: uuid::Uuid,
    response_id: uuid::Uuid,
    started: Instant,
    stream_opened_ms: AtomicU64,
    first_chunk_ms: AtomicU64,
    first_text_ms: AtomicU64,
    last_chunk_ms: AtomicU64,
    chunks: AtomicU64,
    outcome: Option<&'static str>,
}

impl HelperTiming {
    fn new(user_id: uuid::Uuid, response_id: uuid::Uuid) -> Self {
        Self {
            user_id,
            response_id,
            started: Instant::now(),
            stream_opened_ms: AtomicU64::new(0),
            first_chunk_ms: AtomicU64::new(0),
            first_text_ms: AtomicU64::new(0),
            last_chunk_ms: AtomicU64::new(0),
            chunks: AtomicU64::new(0),
            outcome: None,
        }
    }

    fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis())
            .unwrap_or(u64::MAX)
            .max(1)
    }

    fn stream_opened(&self) {
        self.stream_opened_ms
            .store(self.elapsed_ms(), Ordering::Relaxed);
    }

    fn chunk(&self, added_text: bool) {
        let now = self.elapsed_ms();
        self.chunks.fetch_add(1, Ordering::Relaxed);
        self.last_chunk_ms.store(now, Ordering::Relaxed);
        let _ = self
            .first_chunk_ms
            .compare_exchange(0, now, Ordering::Relaxed, Ordering::Relaxed);
        if added_text {
            let _ =
                self.first_text_ms
                    .compare_exchange(0, now, Ordering::Relaxed, Ordering::Relaxed);
        }
    }

    fn finish(&mut self, outcome: &'static str) {
        self.outcome = Some(outcome);
    }
}

/// Explicit cancellation drops the worker holding this helper, so the record
/// is written here rather than on a path that a dropped future never reaches.
impl Drop for HelperTiming {
    fn drop(&mut self) {
        let (user_id, response_id) = (self.user_id, self.response_id);
        tracing::info!(event = "responses.compaction.helper_timing", %user_id, %response_id,
            outcome = self.outcome.unwrap_or("dropped"), elapsed_ms = self.elapsed_ms(),
            stream_opened_ms = self.stream_opened_ms.load(Ordering::Relaxed),
            first_chunk_ms = self.first_chunk_ms.load(Ordering::Relaxed),
            first_text_ms = self.first_text_ms.load(Ordering::Relaxed),
            last_chunk_ms = self.last_chunk_ms.load(Ordering::Relaxed),
            chunks = self.chunks.load(Ordering::Relaxed),
            "Compaction summary upstream timing");
    }
}

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
    InitialPreparationBudget,
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
            Self::InitialPreparationBudget => "initial_preparation_budget",
            Self::Capacity => "helper_call_limit",
            Self::InputLimit => "helper_input_limit",
            Self::InvalidSummary => "invalid_summary",
            Self::Provider(_) => "provider_failure",
            Self::Unsafe(_) => "safety_failure",
        }
    }

    pub(super) fn can_fallback_to_uncompacted(&self) -> bool {
        match self {
            Self::InitialPreparationBudget
            | Self::Capacity
            | Self::InputLimit
            | Self::InvalidSummary => true,
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
            Self::Cancelled
            | Self::Deadline
            | Self::InitialPreparationBudget
            | Self::Capacity
            | Self::InvalidSummary => ApiError::ServiceUnavailable,
        }
    }
}

fn check_summary_lifetime(
    execution: &ResponseExecution,
    deadline: Instant,
    initial_preparation_deadline: Option<Instant>,
) -> Result<(), SummaryExecutionError> {
    if execution.is_cancelled() {
        return Err(SummaryExecutionError::Cancelled);
    }
    let now = Instant::now();
    // The absolute execution deadline always wins over the optional phase
    // budget: an expired or cancelled Response may never take a fallback.
    if now >= deadline {
        return Err(SummaryExecutionError::Deadline);
    }
    if initial_preparation_deadline.is_some_and(|deadline| now >= deadline) {
        return Err(SummaryExecutionError::InitialPreparationBudget);
    }
    Ok(())
}

async fn await_summary_step<T>(
    execution: &ResponseExecution,
    deadline: Instant,
    initial_preparation_deadline: Option<Instant>,
    step: impl std::future::Future<Output = Result<T, SummaryExecutionError>>,
) -> Result<T, SummaryExecutionError> {
    check_summary_lifetime(execution, deadline, initial_preparation_deadline)?;
    let step_deadline = initial_preparation_deadline.map_or(deadline, |phase| phase.min(deadline));
    tokio::select! {
        biased;
        _ = execution.cancelled() => Err(SummaryExecutionError::Cancelled),
        _ = tokio::time::sleep_until(step_deadline) => {
            if Instant::now() >= deadline {
                Err(SummaryExecutionError::Deadline)
            } else {
                Err(SummaryExecutionError::InitialPreparationBudget)
            }
        },
        result = step => result,
    }
}

/// A narrow input-validation rejection can safely authorize a smaller prompt.
/// It never authorizes replay of an accepted stream, a tool or a generic error.
pub(crate) fn is_context_rejection(error: &CompletionExecutionError) -> bool {
    let Some(crate::inference::AttemptTerminal::Failed { failure, .. }) = error.terminal() else {
        return false;
    };
    if failure.kind != AttemptFailureKind::HttpStatus
        || failure.stage != AttemptStage::AwaitingResponse
    {
        return false;
    }
    let Some(diagnostic) = failure.upstream_diagnostic.as_deref() else {
        return false;
    };
    match failure.status {
        Some(400) => diagnostic.is_input_context_overflow(),
        Some(500) => diagnostic.is_proxy_context_overflow(),
        _ => false,
    }
}

pub(crate) struct SummaryExecutor<'a> {
    pub state: &'a Arc<AppState>,
    pub user: &'a User,
    pub body: &'a ResponsesCreateRequest,
    pub routing: InferenceRoutingContext,
    pub cache_policy: &'a CompletionCachePolicy,
    pub client_app: Option<crate::client_app::ClientApp>,
    pub execution: &'a ResponseExecution,
    pub budget: &'a CompactionBudget,
    pub deadline: Instant,
}

impl SummaryExecutor<'_> {
    /// One helper request for the whole selection. A provider that rejects its
    /// size returns `InputLimit`, so the caller can narrow that selection.
    pub(super) async fn summarize(
        &self,
        work: &SummaryWork,
        policy: &CompactionPolicy,
    ) -> Result<String, SummaryExecutionError> {
        self.check_execution()?;
        if work
            .items
            .iter()
            .all(|item| item.summary_messages.is_empty())
        {
            return Ok(summary::identity_only_checkpoint(
                work.previous_summary.as_deref(),
            ));
        }
        let mut messages = Vec::new();
        for item in &work.items {
            if work.active_start == Some(item.key) {
                messages.push(json!({ "role": "assistant", "content": TRUNCATION_MARKER }));
            }
            messages.extend(item.summary_messages.iter().cloned());
        }
        let transcript = summary::serialize_transcript(&messages).map_err(|error| {
            tracing::warn!(event = "responses.compaction.helper_failed", user_id = %self.user.uuid,
                response_id = %self.execution.response_id(), reason = error.category(),
                stage = "transcript_preparation", "Compaction summary transcript could not be prepared");
            SummaryExecutionError::Unsafe(ApiError::InternalServerError)
        })?;
        let source_messages = transcript.len();
        let messages = summary::summary_messages(transcript, work.previous_summary.as_deref());
        if !fits_provider_request(&messages)? {
            tracing::warn!(event = "responses.compaction.helper_input_rejected", user_id = %self.user.uuid,
                response_id = %self.execution.response_id(), reason = "request_byte_limit",
                "Compaction summary request exceeds the provider request size");
            return Err(SummaryExecutionError::InputLimit);
        }
        self.budget
            .begin_call()
            .map_err(|_| SummaryExecutionError::Capacity)?;
        let request = build_compaction_model_request(self.body, &messages);
        let (user_id, response_id) = (self.user.uuid, self.execution.response_id());
        tracing::info!(event = "responses.compaction.helper_call_started", %user_id, %response_id,
            source_items = work.items.len(), source_messages,
            previous_summary_present = work.previous_summary.is_some(),
            "Compaction summary provider call started");
        let billing =
            BillingContext::new(AuthMethod::Jwt, self.body.model.clone(), self.client_app);
        let execution = CompletionExecutionContext::new(billing, self.routing, self.cache_policy);
        let mut timing = HelperTiming::new(user_id, response_id);
        // Helpers are independently billed work under this owned Response; their
        // failure never authorizes replaying the caller's request.
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
            timing.stream_opened();
            collect_summary_stream_with_progress(
                &mut completion.stream,
                self.budget.metrics.as_ref(),
                Some(&self.budget.progress),
                Some(&timing),
            )
            .await
        };
        // Dropping this helper future is local cancellation, not an invented
        // provider failure. Ordinary provider observation/usage guards retain
        // their established terminal semantics.
        let response = await_summary_step(
            self.execution,
            self.deadline,
            self.budget.initial_preparation_deadline(),
            response,
        )
        .await;
        let elapsed_ms = timing.elapsed_ms();
        match response {
            Err(SummaryExecutionError::Cancelled) => {
                timing.finish("cancelled");
                tracing::info!(event = "responses.compaction.helper_cancelled", %user_id, %response_id,
                    stage = "awaiting_provider", elapsed_ms,
                    "Compaction summary provider call cancelled");
                Err(SummaryExecutionError::Cancelled)
            }
            Err(SummaryExecutionError::Deadline) => {
                timing.finish("deadline");
                tracing::warn!(event = "responses.compaction.helper_deadline", %user_id, %response_id,
                    stage = "awaiting_provider", elapsed_ms,
                    "Compaction summary provider call reached its deadline");
                Err(SummaryExecutionError::Deadline)
            }
            Err(SummaryExecutionError::InitialPreparationBudget) => {
                timing.finish("initial_preparation_budget");
                tracing::warn!(event = "responses.compaction.helper_initial_preparation_budget",
                    %user_id, %response_id, stage = "awaiting_provider", elapsed_ms,
                    "Compaction summary initial preparation budget exhausted");
                Err(SummaryExecutionError::InitialPreparationBudget)
            }
            Ok(response) => {
                timing.finish("completed");
                let next = summary::parse_summary_response(&response).map_err(|error| {
                    log_summary_rejection(error, &response, user_id, response_id, elapsed_ms);
                    SummaryExecutionError::InvalidSummary
                })?;
                // Native model/provider generation limits are not cl100k limits.
                // Keep only the independent untrusted byte bound.
                if next.len() > MAX_SUMMARY_TEXT_BYTES {
                    tracing::warn!(event = "responses.compaction.helper_rejected", %user_id, %response_id,
                        reason = "output_byte_limit", output_bytes = next.len(),
                        output_byte_limit = MAX_SUMMARY_TEXT_BYTES,
                        "Compaction summary response exceeded its byte limit");
                    return Err(SummaryExecutionError::InvalidSummary);
                }
                if !summary_fits_next_helper_input(&next, policy) {
                    tracing::warn!(event = "responses.compaction.helper_rejected", %user_id, %response_id,
                        reason = "next_helper_input_limit", output_bytes = next.len(),
                        input_budget = policy.summary_input_budget,
                        "Compaction summary cannot fit a later helper input");
                    return Err(SummaryExecutionError::InvalidSummary);
                }
                tracing::info!(event = "responses.compaction.helper_call_completed", %user_id, %response_id,
                    output_bytes = next.len(), elapsed_ms,
                    metadata = ?summary::response_metadata(&response),
                    "Compaction summary provider call completed");
                Ok(next)
            }
            Err(SummaryExecutionError::Provider(error)) if is_context_rejection(&error) => {
                timing.finish("input_limit");
                tracing::warn!(event = "responses.compaction.helper_input_rejected", %user_id, %response_id,
                    reason = "provider_input_limit", elapsed_ms,
                    "Compaction summary provider rejected the input size");
                Err(SummaryExecutionError::InputLimit)
            }
            Err(SummaryExecutionError::Provider(error)) => {
                timing.finish("provider_failure");
                log_summary_provider_failure(&error, user_id, response_id, elapsed_ms);
                Err(SummaryExecutionError::Provider(error))
            }
            Err(error) => {
                timing.finish("failed");
                Err(error)
            }
        }
    }

    fn check_execution(&self) -> Result<(), SummaryExecutionError> {
        if let Err(error) = check_summary_lifetime(
            self.execution,
            self.deadline,
            self.budget.initial_preparation_deadline(),
        ) {
            tracing::warn!(event = "responses.compaction.helper_stopped", user_id = %self.user.uuid,
                response_id = %self.execution.response_id(), stage = "before_helper",
                reason = error.category(), "Compaction summary execution cannot start");
            return Err(error);
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

#[cfg(test)]
async fn collect_summary_stream(
    stream: &mut tokio::sync::mpsc::Receiver<CompletionChunk>,
    metrics: &ContextMetrics,
) -> Result<serde_json::Value, SummaryExecutionError> {
    collect_summary_stream_with_progress(stream, metrics, None, None).await
}

async fn collect_summary_stream_with_progress(
    stream: &mut tokio::sync::mpsc::Receiver<CompletionChunk>,
    metrics: &ContextMetrics,
    progress: Option<&super::progress::ProgressPublisher>,
    timing: Option<&HelperTiming>,
) -> Result<serde_json::Value, SummaryExecutionError> {
    let mut summary = summary::SummaryStreamAccumulator::new(MAX_SUMMARY_TEXT_BYTES);
    let mut usage_recorded = false;
    while let Some(chunk) = stream.recv().await {
        match chunk {
            CompletionChunk::StreamChunk(chunk) => {
                let previous_bytes = summary.visible_text_bytes();
                summary.observe(&chunk).map_err(invalid_stream_summary)?;
                let added_text = summary.visible_text_bytes() > previous_bytes;
                if let Some(timing) = timing {
                    timing.chunk(added_text);
                }
                if added_text {
                    if let Some(progress) = progress {
                        progress.sampled().await;
                    }
                }
            }
            // The existing completion processor owns usage publication. Wait
            // for its terminal after billing, rather than treating finish_reason
            // or a partial answer as a completed, durable summary.
            CompletionChunk::Usage(usage) => {
                // Numeric observation only. The completion processor has already
                // published billing, and duplicate consumer chunks must not
                // inflate the per-helper diagnostic counters.
                if !usage_recorded {
                    metrics.helper_usage(&usage);
                    usage_recorded = true;
                }
            }
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

/// Continuum's proxy refuses a request past 4,000,000 bytes of content, whatever
/// the model's window. Measure the real request, with a margin, rather than send
/// what it would refuse: narrowing here costs no provider call.
fn fits_provider_request(messages: &[Value]) -> Result<bool, SummaryExecutionError> {
    const MAX_REQUEST_BYTES: usize = 3_900_000;
    serde_json::to_vec(messages)
        .map(|request| request.len() <= MAX_REQUEST_BYTES)
        .map_err(|_| SummaryExecutionError::Unsafe(ApiError::InternalServerError))
}

/// A generation allowance is not a helper-input budget. Before publishing a
/// checkpoint, ensure that a later update can carry this whole summary plus the
/// existing instructions and useful new transcript. This check never changes the
/// provider's output limit or the caller's answer allowance.
fn summary_fits_next_helper_input(text: &str, policy: &CompactionPolicy) -> bool {
    const NEXT_SOURCE_RESERVE_TOKENS: usize = 512;
    let request = summary::summary_messages(Vec::<Value>::new(), Some(text));
    serde_json::to_string(&request)
        .map(|request| {
            count_tokens(&request).saturating_add(NEXT_SOURCE_RESERVE_TOKENS)
                <= policy.summary_input_budget
        })
        .unwrap_or(false)
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

    #[tokio::test]
    async fn initial_preparation_budget_drops_pending_helper_without_cancelling_the_response() {
        use crate::web::responses::ResponseExecutionRegistry;
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::time::{timeout, Duration};

        struct DropMarker<'a>(&'a AtomicBool);
        impl Drop for DropMarker<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let registry = ResponseExecutionRegistry::default();
        let registration = registry.register(Uuid::new_v4(), Uuid::new_v4()).unwrap();
        let execution = registration.execution();
        let polled = AtomicBool::new(false);
        let dropped = AtomicBool::new(false);
        let deadline = Instant::now() + Duration::from_secs(1);
        let phase_deadline = Instant::now() + Duration::from_millis(20);
        let result = timeout(
            Duration::from_secs(1),
            await_summary_step(&execution, deadline, Some(phase_deadline), async {
                let _marker = DropMarker(&dropped);
                polled.store(true, Ordering::Release);
                std::future::pending::<Result<(), SummaryExecutionError>>().await
            }),
        )
        .await
        .expect("the earlier initial preparation budget must bound a slow helper");
        assert!(matches!(
            result,
            Err(SummaryExecutionError::InitialPreparationBudget)
        ));
        assert!(polled.load(Ordering::Acquire));
        assert!(dropped.load(Ordering::Acquire));
        assert!(!execution.is_cancelled());
        assert!(Instant::now() < deadline);
    }

    #[tokio::test]
    async fn expired_initial_preparation_budget_never_polls_work_and_absolute_deadline_stays_strict(
    ) {
        use crate::web::responses::ResponseExecutionRegistry;
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::time::Duration;

        let registry = ResponseExecutionRegistry::default();
        let registration = registry.register(Uuid::new_v4(), Uuid::new_v4()).unwrap();
        let execution = registration.execution();
        let expired = Instant::now() - Duration::from_millis(1);
        let future = Instant::now() + Duration::from_secs(1);
        let polled = AtomicBool::new(false);
        let result = await_summary_step(&execution, future, Some(expired), async {
            polled.store(true, Ordering::Release);
            Ok(())
        })
        .await;
        assert!(matches!(
            result,
            Err(SummaryExecutionError::InitialPreparationBudget)
        ));
        assert!(!polled.load(Ordering::Acquire));
        assert!(matches!(
            check_summary_lifetime(&execution, expired, Some(expired)),
            Err(SummaryExecutionError::Deadline)
        ));
        execution.cancel();
        assert!(matches!(
            check_summary_lifetime(&execution, expired, Some(expired)),
            Err(SummaryExecutionError::Cancelled)
        ));
    }

    #[tokio::test]
    async fn clearing_initial_preparation_budget_restores_later_helper_execution_allowance() {
        use crate::web::responses::ResponseExecutionRegistry;
        use tokio::time::Duration;

        let registry = ResponseExecutionRegistry::default();
        let registration = registry.register(Uuid::new_v4(), Uuid::new_v4()).unwrap();
        let execution = registration.execution();
        let budget = CompactionBudget::default();
        budget.set_initial_preparation_deadline(Instant::now() - Duration::from_millis(1));
        budget.clear_initial_preparation_deadline();
        let deadline = Instant::now() + Duration::from_secs(1);
        let result = await_summary_step(
            &execution,
            deadline,
            budget.initial_preparation_deadline(),
            async {
                tokio::time::sleep(Duration::from_millis(20)).await;
                Ok(42)
            },
        )
        .await;
        assert!(matches!(result, Ok(42)));
        assert!(!execution.is_cancelled());
    }

    fn summary_stream_chunk(content: &str, finish: serde_json::Value) -> CompletionChunk {
        CompletionChunk::StreamChunk(json!({"choices": [{"index": 0, "delta": {
            "role": "assistant", "content": content,
        }, "finish_reason": finish}]}))
    }

    #[tokio::test]
    async fn compaction_progress_requires_valid_new_visible_summary_text() {
        use super::super::{progress::ProgressPublisher, CompactionProgress};
        use crate::web::responses::handlers::StorageMessage;

        for visible in [false, true] {
            let (client_tx, mut client_rx) = tokio::sync::mpsc::channel(8);
            let progress = ProgressPublisher::default();
            progress.set_sender(client_tx);
            let id = progress.begin().await;
            assert!(matches!(
                client_rx.recv().await,
                Some(StorageMessage::CompactionProgress(CompactionProgress::Started { item_id })) if item_id == id
            ));

            let (tx, mut rx) = tokio::sync::mpsc::channel(8);
            tx.send(CompletionChunk::StreamChunk(
                json!({"choices": [{"index": 0, "delta": {
                    "role": "assistant", "reasoning_content": "PRIVATE_REASONING", "content": null
                }}]}),
            ))
            .await
            .unwrap();
            tx.send(CompletionChunk::Usage(
                crate::web::openai::CompletionUsage {
                    prompt_tokens: 10,
                    completion_tokens: 20,
                    cached_prompt_tokens: None,
                },
            ))
            .await
            .unwrap();
            tx.send(summary_stream_chunk("", serde_json::Value::Null))
                .await
                .unwrap();
            if visible {
                tx.send(summary_stream_chunk(
                    "PRIVATE_SUMMARY",
                    serde_json::Value::Null,
                ))
                .await
                .unwrap();
                // A second sampled chunk within the interval cannot emit more
                // progress, and even successful sampling is not publication.
                tx.send(summary_stream_chunk(" continued", json!("stop")))
                    .await
                    .unwrap();
            } else {
                tx.send(summary_stream_chunk("", json!("stop")))
                    .await
                    .unwrap();
            }
            tx.send(CompletionChunk::Terminal(AttemptTerminal::Completed {
                attempt: attempt(),
                evidence: CompletionEvidence::ProviderDone,
            }))
            .await
            .unwrap();
            drop(tx);
            let _ = collect_summary_stream_with_progress(
                &mut rx,
                &ContextMetrics::default(),
                Some(&progress),
                None,
            )
            .await;
            if visible {
                assert!(matches!(
                    client_rx.try_recv(),
                    Ok(StorageMessage::CompactionProgress(CompactionProgress::Compacting { item_id })) if item_id == id
                ));
            }
            assert!(client_rx.try_recv().is_err());
            progress.abandon().await;
            assert!(matches!(
                client_rx.try_recv(),
                Ok(StorageMessage::CompactionProgress(CompactionProgress::Abandoned { item_id })) if item_id == id
            ));
            assert!(client_rx.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn invalid_summary_chunk_cannot_report_new_sampling() {
        use super::super::{progress::ProgressPublisher, CompactionProgress};
        use crate::web::responses::handlers::StorageMessage;

        let (client_tx, mut client_rx) = tokio::sync::mpsc::channel(8);
        let progress = ProgressPublisher::default();
        progress.set_sender(client_tx);
        let id = progress.begin().await;
        let _ = client_rx.recv().await;
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tx.send(CompletionChunk::StreamChunk(json!({"choices": [{"index": 0, "delta": {
            "role": "assistant", "content": "PRIVATE_PARTIAL", "tool_calls": [{"id": "unsupported"}]
        }}]})))
        .await
        .unwrap();
        drop(tx);
        assert!(matches!(
            collect_summary_stream_with_progress(
                &mut rx,
                &ContextMetrics::default(),
                Some(&progress),
                None,
            )
            .await,
            Err(SummaryExecutionError::InvalidSummary)
        ));
        assert!(client_rx.try_recv().is_err());
        progress.abandon().await;
        assert!(matches!(
            client_rx.recv().await,
            Some(StorageMessage::CompactionProgress(CompactionProgress::Abandoned { item_id })) if item_id == id
        ));
    }

    #[tokio::test]
    async fn summary_stream_waits_for_successful_terminal_after_finish_and_usage() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        tx.send(summary_stream_chunk("Visible summary", json!("stop")))
            .await
            .unwrap();
        let metrics = Arc::new(ContextMetrics::default());
        let stream_metrics = metrics.clone();
        let collecting =
            tokio::spawn(
                async move { collect_summary_stream(&mut rx, stream_metrics.as_ref()).await },
            );
        tokio::task::yield_now().await;
        assert!(!collecting.is_finished());
        tx.send(CompletionChunk::Usage(
            crate::web::openai::CompletionUsage {
                prompt_tokens: 10,
                completion_tokens: 20,
                cached_prompt_tokens: Some(7),
            },
        ))
        .await
        .unwrap();
        tx.send(CompletionChunk::Usage(
            crate::web::openai::CompletionUsage {
                prompt_tokens: 10,
                completion_tokens: 20,
                cached_prompt_tokens: Some(7),
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
        let stats = metrics.snapshot();
        assert_eq!(stats.helper_usage_records, 1);
        assert_eq!(stats.helper_input_tokens, 10);
        assert_eq!(stats.helper_output_tokens, 20);
        assert_eq!(stats.helper_cached_input_tokens, 7);
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
            collect_summary_stream(&mut rx, &ContextMetrics::default()).await,
            Err(SummaryExecutionError::Provider(_))
        ));

        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tx.send(summary_stream_chunk("partial", json!("stop")))
            .await
            .unwrap();
        drop(tx);
        assert!(matches!(
            collect_summary_stream(&mut rx, &ContextMetrics::default()).await,
            Err(SummaryExecutionError::InvalidSummary)
        ));
    }

    #[tokio::test]
    async fn failed_summary_stream_retains_numeric_usage_once_without_becoming_successful() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        for _ in 0..2 {
            tx.send(CompletionChunk::Usage(
                crate::web::openai::CompletionUsage {
                    prompt_tokens: 31,
                    completion_tokens: 9,
                    cached_prompt_tokens: Some(20),
                },
            ))
            .await
            .unwrap();
        }
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
        let metrics = ContextMetrics::default();
        assert!(matches!(
            collect_summary_stream(&mut rx, &metrics).await,
            Err(SummaryExecutionError::Provider(_))
        ));
        let stats = metrics.snapshot();
        assert_eq!(stats.helper_usage_records, 1);
        assert_eq!(stats.helper_input_tokens, 31);
        assert_eq!(stats.helper_output_tokens, 9);
        assert_eq!(stats.helper_cached_input_tokens, 20);
    }

    #[tokio::test]
    async fn summary_stream_rejects_full_response_and_oversized_visible_output() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tx.send(CompletionChunk::FullResponse(json!({"choices": []})))
            .await
            .unwrap();
        assert!(matches!(
            collect_summary_stream(&mut rx, &ContextMetrics::default()).await,
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
            collect_summary_stream(&mut rx, &ContextMetrics::default()).await,
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

    #[tokio::test]
    async fn helper_timing_separates_stream_stages_and_survives_a_dropped_helper() {
        use crate::web::responses::ResponseExecutionRegistry;
        use tokio::time::Duration;

        let registry = ResponseExecutionRegistry::default();
        let registration = registry.register(Uuid::new_v4(), Uuid::new_v4()).unwrap();
        let execution = registration.execution();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        tx.send(CompletionChunk::StreamChunk(
            json!({"choices": [{"index": 0, "delta": {
                "role": "assistant", "reasoning_content": "PRIVATE_REASONING", "content": null
            }}]}),
        ))
        .await
        .unwrap();
        tx.send(summary_stream_chunk(
            "PRIVATE_SUMMARY",
            serde_json::Value::Null,
        ))
        .await
        .unwrap();
        let timing = HelperTiming::new(Uuid::new_v4(), Uuid::new_v4());
        let metrics = ContextMetrics::default();
        tokio::time::sleep(Duration::from_millis(5)).await;
        // The stream never ends: this is the stalled helper a deadline stops.
        let result = await_summary_step(
            &execution,
            Instant::now() + Duration::from_secs(1),
            Some(Instant::now() + Duration::from_millis(30)),
            async {
                timing.stream_opened();
                collect_summary_stream_with_progress(&mut rx, &metrics, None, Some(&timing)).await
            },
        )
        .await;
        assert!(matches!(
            result,
            Err(SummaryExecutionError::InitialPreparationBudget)
        ));
        assert_eq!(timing.chunks.load(Ordering::Relaxed), 2);
        let opened = timing.stream_opened_ms.load(Ordering::Relaxed);
        let first_chunk = timing.first_chunk_ms.load(Ordering::Relaxed);
        let first_text = timing.first_text_ms.load(Ordering::Relaxed);
        let last_chunk = timing.last_chunk_ms.load(Ordering::Relaxed);
        assert!(opened >= 5 && first_chunk >= opened);
        assert!(first_text >= first_chunk && last_chunk >= first_text);
        // The pending stage stays visible: elapsed outlives the last chunk.
        assert!(timing.elapsed_ms() > last_chunk);

        // A worker dropped by cancellation still reports what its helper reached.
        let logs = crate::web::responses::compaction::telemetry::capture_logs(move || drop(timing));
        assert!(logs.contains("responses.compaction.helper_timing"));
        assert!(logs.contains("outcome=\"dropped\""));
        assert!(logs.contains("first_text_ms"));
        assert!(!logs.contains("PRIVATE"));
    }

    #[test]
    fn an_unreached_helper_stage_reports_zero() {
        let timing = HelperTiming::new(Uuid::new_v4(), Uuid::new_v4());
        timing.chunk(false);
        assert_eq!(timing.stream_opened_ms.load(Ordering::Relaxed), 0);
        assert_eq!(timing.first_text_ms.load(Ordering::Relaxed), 0);
        assert!(timing.first_chunk_ms.load(Ordering::Relaxed) > 0);
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
            SummaryExecutionError::InitialPreparationBudget,
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

    const PROXY_OVERFLOW: &str = "mutating request: applying endpoint-specific mutator: mutating request: generating shard key: context too large: ~1062098 tokens";

    #[tokio::test]
    async fn the_proxy_size_refusal_authorizes_recovery_only_as_its_own_500() {
        let refusal =
            diagnostic(json!({"error": {"message": PROXY_OVERFLOW, "type": ""}}).to_string()).await;
        let mut failure = http_failure(Some(refusal));
        failure.status = Some(500);
        assert!(is_context_rejection(&failed(failure.clone())));
        for status in [None, Some(400), Some(502), Some(503)] {
            let mut failure = failure.clone();
            failure.status = status;
            assert!(!is_context_rejection(&failed(failure)));
        }
        let mut other_500 = http_failure(Some(strict_diagnostic().await));
        other_500.status = Some(500);
        assert!(!is_context_rejection(&failed(other_500)));
        let mut plain_500 = http_failure(None);
        plain_500.status = Some(500);
        assert!(!is_context_rejection(&failed(plain_500)));
    }

    #[test]
    fn a_helper_request_past_the_proxy_size_is_not_sent() {
        let sized = |bytes: usize| vec![json!({"role": "user", "content": "x".repeat(bytes)})];
        assert!(fits_provider_request(&sized(3_800_000)).unwrap());
        assert!(!fits_provider_request(&sized(4_000_000)).unwrap());
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
