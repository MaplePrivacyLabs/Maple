//! Per-Response context rollout diagnostics. Only implementation-owned labels,
//! identifiers and numeric counters may leave this module; no payload is retained.

use super::{compaction::telemetry, context_policy::ContextPolicy};
use crate::{web::openai::CompletionUsage, ApiError};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use uuid::Uuid;

/// Numeric accounting scoped to one logical Response, shared across preparations
/// and helper calls. These are observations, never a second billing publisher.
#[derive(Default)]
pub(super) struct ContextMetrics {
    flag_evaluation_recorded: AtomicBool,
    flag_evaluation_ms: AtomicU64,
    preparation_calls: AtomicU64,
    preparation_failures: AtomicU64,
    preparation_ms: AtomicU64,
    forced_preparations: AtomicU64,
    helper_calls: AtomicU64,
    helper_usage_records: AtomicU64,
    helper_input_tokens: AtomicU64,
    helper_output_tokens: AtomicU64,
    helper_cached_input_tokens: AtomicU64,
    published_checkpoints: AtomicU64,
    reused_checkpoints: AtomicU64,
    advisory_fallbacks: AtomicU64,
    legacy_source_observed: AtomicBool,
    legacy_fetched_rows: AtomicU64,
    legacy_fetch_batches: AtomicU64,
    legacy_fetched_bytes: AtomicU64,
    legacy_max_batch_bytes: AtomicU64,
    legacy_projected_bytes: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct ContextMetricsSnapshot {
    pub flag_evaluations: u64,
    pub flag_evaluation_ms: u64,
    pub preparation_calls: u64,
    pub preparation_failures: u64,
    pub preparation_ms: u64,
    pub forced_preparations: u64,
    pub helper_calls: u64,
    /// Usage frames observed by the helper consumer, once per helper. Missing
    /// frames (for example on cancellation) are unknown usage, not free work.
    pub helper_usage_records: u64,
    pub helper_input_tokens: u64,
    pub helper_output_tokens: u64,
    pub helper_cached_input_tokens: u64,
    pub published_checkpoints: u64,
    /// Preparations that reuse a durable checkpoint without publishing a new one.
    /// This is not a distinct-checkpoint or distinct-conversation count.
    pub reused_checkpoints: u64,
    pub advisory_fallbacks: u64,
    pub legacy_source_observed: bool,
    pub legacy_fetched_rows: u64,
    pub legacy_fetch_batches: u64,
    pub legacy_fetched_bytes: u64,
    pub legacy_max_batch_bytes: u64,
    pub legacy_projected_bytes: u64,
}

fn add(counter: &AtomicU64, amount: u64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(amount))
    });
}

fn count(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

impl ContextMetrics {
    pub(super) fn record_flag_evaluation(&self, duration: Duration) {
        if self
            .flag_evaluation_recorded
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.flag_evaluation_ms.store(
                u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
                Ordering::Release,
            );
        }
    }

    pub(super) fn start_preparation(self: &Arc<Self>, forced: bool) -> PreparationObservation {
        add(&self.preparation_calls, 1);
        if forced {
            self.record_forced_preparation();
        }
        PreparationObservation {
            metrics: self.clone(),
            started: Instant::now(),
            finished: false,
        }
    }

    pub(super) fn record_forced_preparation(&self) {
        add(&self.forced_preparations, 1);
    }

    pub(super) fn helper_call_started(&self) {
        add(&self.helper_calls, 1);
    }

    pub(super) fn helper_usage(&self, usage: &CompletionUsage) {
        add(&self.helper_usage_records, 1);
        add(&self.helper_input_tokens, usage.prompt_tokens.max(0) as u64);
        add(
            &self.helper_output_tokens,
            usage.completion_tokens.max(0) as u64,
        );
        add(
            &self.helper_cached_input_tokens,
            usage.cached_prompt_tokens.unwrap_or(0).max(0) as u64,
        );
    }

    pub(super) fn checkpoint_published(&self) {
        add(&self.published_checkpoints, 1);
    }

    pub(super) fn checkpoint_reused(&self) {
        add(&self.reused_checkpoints, 1);
    }

    pub(super) fn advisory_fallback(&self) {
        add(&self.advisory_fallbacks, 1);
    }

    pub(super) fn legacy_source_loaded(
        &self,
        fetched_rows: usize,
        fetch_batches: usize,
        fetched_bytes: usize,
        max_batch_bytes: usize,
        projected_bytes: usize,
    ) {
        self.legacy_source_observed.store(true, Ordering::Relaxed);
        add(&self.legacy_fetched_rows, count(fetched_rows));
        add(&self.legacy_fetch_batches, count(fetch_batches));
        add(&self.legacy_fetched_bytes, count(fetched_bytes));
        self.legacy_max_batch_bytes
            .fetch_max(count(max_batch_bytes), Ordering::Relaxed);
        add(&self.legacy_projected_bytes, count(projected_bytes));
    }

    pub(super) fn snapshot(&self) -> ContextMetricsSnapshot {
        ContextMetricsSnapshot {
            flag_evaluations: u64::from(self.flag_evaluation_recorded.load(Ordering::Acquire)),
            flag_evaluation_ms: self.flag_evaluation_ms.load(Ordering::Acquire),
            preparation_calls: self.preparation_calls.load(Ordering::Relaxed),
            preparation_failures: self.preparation_failures.load(Ordering::Relaxed),
            preparation_ms: self.preparation_ms.load(Ordering::Relaxed),
            forced_preparations: self.forced_preparations.load(Ordering::Relaxed),
            helper_calls: self.helper_calls.load(Ordering::Relaxed),
            helper_usage_records: self.helper_usage_records.load(Ordering::Relaxed),
            helper_input_tokens: self.helper_input_tokens.load(Ordering::Relaxed),
            helper_output_tokens: self.helper_output_tokens.load(Ordering::Relaxed),
            helper_cached_input_tokens: self.helper_cached_input_tokens.load(Ordering::Relaxed),
            published_checkpoints: self.published_checkpoints.load(Ordering::Relaxed),
            reused_checkpoints: self.reused_checkpoints.load(Ordering::Relaxed),
            advisory_fallbacks: self.advisory_fallbacks.load(Ordering::Relaxed),
            legacy_source_observed: self.legacy_source_observed.load(Ordering::Relaxed),
            legacy_fetched_rows: self.legacy_fetched_rows.load(Ordering::Relaxed),
            legacy_fetch_batches: self.legacy_fetch_batches.load(Ordering::Relaxed),
            legacy_fetched_bytes: self.legacy_fetched_bytes.load(Ordering::Relaxed),
            legacy_max_batch_bytes: self.legacy_max_batch_bytes.load(Ordering::Relaxed),
            legacy_projected_bytes: self.legacy_projected_bytes.load(Ordering::Relaxed),
        }
    }
}

/// Dropping an interrupted preparation still accounts for its duration/failure.
/// It neither cancels work nor changes its result or execution allowance.
pub(super) struct PreparationObservation {
    metrics: Arc<ContextMetrics>,
    started: Instant,
    finished: bool,
}

impl PreparationObservation {
    pub(super) fn finish(mut self, failed: bool) {
        self.record(failed);
    }

    fn record(&mut self, failed: bool) {
        if self.finished {
            return;
        }
        self.finished = true;
        add(
            &self.metrics.preparation_ms,
            telemetry::elapsed_ms(self.started),
        );
        if failed {
            add(&self.metrics.preparation_failures, 1);
        }
    }
}

impl Drop for PreparationObservation {
    fn drop(&mut self) {
        self.record(true);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ContextOutcome {
    Completed,
    Failed,
    Cancelled,
}

impl ContextOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

struct ObservationInner {
    user_uuid: Uuid,
    conversation_uuid: Uuid,
    response_uuid: Uuid,
    initial_model: &'static str,
    prepared_model: Mutex<&'static str>,
    policy: ContextPolicy,
    metrics: Arc<ContextMetrics>,
    started: Instant,
    accepted: AtomicBool,
    outcome_recorded: AtomicBool,
}

#[derive(Clone)]
pub(super) struct ResponseContextObservation(Arc<ObservationInner>);

/// Observe a dropped admission future without changing execution ownership.
/// Accepted streams retain their authoritative terminal observation instead.
#[derive(Default)]
pub(super) struct ResponseAdmissionObservation {
    observation: Option<ResponseContextObservation>,
    returned: bool,
}

impl ResponseAdmissionObservation {
    pub(super) fn slot(&mut self) -> &mut Option<ResponseContextObservation> {
        &mut self.observation
    }

    pub(super) fn finish(mut self, error: Option<&ApiError>) {
        if let (Some(error), Some(observation)) = (error, self.observation.as_ref()) {
            observation.preaccept_failed(error);
        }
        self.returned = true;
    }
}

impl Drop for ResponseAdmissionObservation {
    fn drop(&mut self) {
        if self.returned {
            return;
        }
        if let Some(observation) = &self.observation {
            if !observation.0.accepted.load(Ordering::Acquire) {
                observation.record("preaccept_dropped", Some("handler_dropped"), None);
            }
        }
    }
}

impl ResponseContextObservation {
    pub(super) fn new(
        user_uuid: Uuid,
        conversation_uuid: Uuid,
        response_uuid: Uuid,
        model: &str,
        policy: ContextPolicy,
        metrics: Arc<ContextMetrics>,
    ) -> Self {
        let model = telemetry::model_id(model);
        tracing::info!(
            event = "responses.context.decision",
            %user_uuid, %conversation_uuid, %response_uuid,
            context_mode = policy.strategy.as_str(),
            decision_reason = policy.reason.as_str(),
            initial_model = model,
            "Responses context strategy selected"
        );
        Self(Arc::new(ObservationInner {
            user_uuid,
            conversation_uuid,
            response_uuid,
            initial_model: model,
            prepared_model: Mutex::new(model),
            policy,
            metrics,
            started: Instant::now(),
            accepted: AtomicBool::new(false),
            outcome_recorded: AtomicBool::new(false),
        }))
    }

    pub(super) fn prepared_model(&self, model: &str) {
        *self
            .0
            .prepared_model
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = telemetry::model_id(model);
    }

    pub(super) fn accepted(&self) {
        self.0.accepted.store(true, Ordering::Release);
    }

    pub(super) fn preaccept_failed(&self, error: &ApiError) {
        let code = match error {
            ApiError::MessageExceedsContextLimit => Some("message_exceeds_context_limit"),
            ApiError::InferenceProvider(error) => Some(error.code()),
            ApiError::TooManyRequests => Some("rate_limit_exceeded"),
            _ => None,
        };
        self.record(
            "preaccept_failed",
            Some(telemetry::api_error_category(error)),
            code,
        );
    }

    pub(super) fn terminal(
        &self,
        outcome: ContextOutcome,
        public_error_code: Option<&'static str>,
        failure_reason: Option<&'static str>,
    ) {
        self.record(
            outcome.as_str(),
            failure_reason.map(safe_failure_reason),
            public_error_code,
        );
    }

    pub(super) fn storage_failed(&self, reason: &'static str) {
        let reason = match reason {
            "ack_failed" | "ack_dropped" | "channel_closed" => reason,
            _ => "storage_failure",
        };
        self.record("storage_failed", Some(reason), None);
    }

    pub(super) fn deleted(&self) {
        self.record("deleted", None, None);
    }

    fn record(
        &self,
        outcome: &'static str,
        error_category: Option<&'static str>,
        public_error_code: Option<&'static str>,
    ) {
        if self.0.outcome_recorded.swap(true, Ordering::AcqRel) {
            return;
        }
        let metrics = self.0.metrics.snapshot();
        let model = *self
            .0
            .prepared_model
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        tracing::info!(
            event = "responses.context.outcome",
            user_uuid = %self.0.user_uuid,
            conversation_uuid = %self.0.conversation_uuid,
            response_uuid = %self.0.response_uuid,
            context_mode = self.0.policy.strategy.as_str(),
            decision_reason = self.0.policy.reason.as_str(),
            initial_model = self.0.initial_model,
            model_id = model,
            outcome,
            accepted = self.0.accepted.load(Ordering::Acquire),
            error_category = error_category.unwrap_or("none"),
            public_error_code = safe_public_code(public_error_code),
            // Selection itself happens before this observation is constructed.
            elapsed_ms = telemetry::elapsed_ms(self.0.started).saturating_add(metrics.flag_evaluation_ms),
            flag_evaluations = metrics.flag_evaluations,
            flag_evaluation_ms = metrics.flag_evaluation_ms,
            preparation_calls = metrics.preparation_calls,
            preparation_failures = metrics.preparation_failures,
            preparation_ms = metrics.preparation_ms,
            forced_preparations = metrics.forced_preparations,
            helper_calls = metrics.helper_calls,
            helper_usage_records = metrics.helper_usage_records,
            helper_input_tokens = metrics.helper_input_tokens,
            helper_output_tokens = metrics.helper_output_tokens,
            helper_cached_input_tokens = metrics.helper_cached_input_tokens,
            published_checkpoints = metrics.published_checkpoints,
            reused_checkpoints = metrics.reused_checkpoints,
            advisory_fallbacks = metrics.advisory_fallbacks,
            legacy_source_observed = metrics.legacy_source_observed,
            legacy_fetched_rows = metrics.legacy_fetched_rows,
            legacy_fetch_batches = metrics.legacy_fetch_batches,
            legacy_fetched_bytes = metrics.legacy_fetched_bytes,
            legacy_max_batch_bytes = metrics.legacy_max_batch_bytes,
            legacy_projected_bytes = metrics.legacy_projected_bytes,
            "Responses context execution finished"
        );
    }

    #[cfg(test)]
    pub(super) fn has_recorded_outcome(&self) -> bool {
        self.0.outcome_recorded.load(Ordering::Acquire)
    }
}

fn safe_failure_reason(reason: &'static str) -> &'static str {
    match reason {
        "deadline"
        | "repetition"
        | "context_limit"
        | "capacity_rate_limited"
        | "capacity_overloaded"
        | "provider"
        | "internal" => reason,
        _ => "other",
    }
}

fn safe_public_code(code: Option<&'static str>) -> &'static str {
    match code {
        None => "none",
        Some(
            code @ ("rate_limit_exceeded"
            | "repetition_detected"
            | "message_exceeds_context_limit"
            | "server_error"
            | "upstream_invalid_request"
            | "upstream_invalid_parameter"
            | "upstream_unsupported_parameter"
            | "upstream_invalid_messages"
            | "upstream_context_limit"
            | "upstream_payload_too_large"
            | "upstream_provider_error"
            | "upstream_timeout"),
        ) => code,
        Some(_) => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(
        policy: ContextPolicy,
        metrics: Arc<ContextMetrics>,
    ) -> ResponseContextObservation {
        ResponseContextObservation::new(
            Uuid::from_u128(1),
            Uuid::from_u128(2),
            Uuid::from_u128(3),
            "gpt-oss-120b",
            policy,
            metrics,
        )
    }

    #[test]
    fn metrics_account_finished_and_interrupted_preparations_without_double_counting() {
        let metrics = Arc::new(ContextMetrics::default());
        metrics.record_flag_evaluation(Duration::from_millis(23));
        metrics.record_flag_evaluation(Duration::from_millis(99));
        metrics.start_preparation(false).finish(false);
        metrics.start_preparation(true).finish(true);
        drop(metrics.start_preparation(false));
        metrics.helper_call_started();
        metrics.helper_usage(&CompletionUsage {
            prompt_tokens: 12,
            completion_tokens: 4,
            cached_prompt_tokens: Some(7),
        });
        metrics.checkpoint_published();
        metrics.checkpoint_reused();
        metrics.advisory_fallback();
        metrics.legacy_source_loaded(4, 2, 100, 80, 60);
        metrics.legacy_source_loaded(1, 1, 30, 30, 20);
        let stats = metrics.snapshot();
        assert_eq!(stats.flag_evaluations, 1);
        assert_eq!(stats.flag_evaluation_ms, 23);
        assert_eq!(stats.preparation_calls, 3);
        assert_eq!(stats.preparation_failures, 2);
        assert_eq!(stats.forced_preparations, 1);
        assert_eq!(stats.helper_calls, 1);
        assert_eq!(stats.helper_usage_records, 1);
        assert_eq!(stats.helper_input_tokens, 12);
        assert_eq!(stats.helper_output_tokens, 4);
        assert_eq!(stats.helper_cached_input_tokens, 7);
        assert_eq!(stats.published_checkpoints, 1);
        assert_eq!(stats.reused_checkpoints, 1);
        assert_eq!(stats.advisory_fallbacks, 1);
        assert!(stats.legacy_source_observed);
        assert_eq!(stats.legacy_fetched_rows, 5);
        assert_eq!(stats.legacy_fetch_batches, 3);
        assert_eq!(stats.legacy_fetched_bytes, 130);
        assert_eq!(stats.legacy_max_batch_bytes, 80);
        assert_eq!(stats.legacy_projected_bytes, 80);
    }

    #[test]
    fn numeric_usage_cannot_make_counters_negative_or_wrap() {
        let metrics = ContextMetrics::default();
        metrics
            .helper_input_tokens
            .store(u64::MAX - 1, Ordering::Relaxed);
        metrics.helper_usage(&CompletionUsage {
            prompt_tokens: 10,
            completion_tokens: -4,
            cached_prompt_tokens: Some(-9),
        });
        assert_eq!(metrics.snapshot().helper_input_tokens, u64::MAX);
        assert_eq!(metrics.snapshot().helper_output_tokens, 0);
        assert_eq!(metrics.snapshot().helper_cached_input_tokens, 0);
    }

    #[test]
    fn observation_clones_emit_one_decision_and_one_authoritative_outcome() {
        let metrics = Arc::new(ContextMetrics::default());
        let logs = telemetry::capture_logs(|| {
            let observation = observation(ContextPolicy::compaction(), metrics);
            let clone = observation.clone();
            assert!(!observation.has_recorded_outcome());
            observation.accepted();
            observation.prepared_model("glm-5-3");
            clone.terminal(
                ContextOutcome::Failed,
                Some("repetition_detected"),
                Some("repetition"),
            );
            assert!(observation.has_recorded_outcome());
            observation.terminal(ContextOutcome::Completed, None, None);
            observation.storage_failed("ack_dropped");
        });
        assert_eq!(
            logs.matches("event=\"responses.context.decision\"").count(),
            1
        );
        assert_eq!(
            logs.matches("event=\"responses.context.outcome\"").count(),
            1
        );
        assert!(logs.contains("context_mode=\"compaction\""));
        assert!(logs.contains("outcome=\"failed\""));
        assert!(logs.contains("accepted=true"));
        assert!(logs.contains("initial_model=\"gpt-oss-120b\""));
        assert!(logs.contains("model_id=\"glm-5-3\""));
        assert!(logs.contains("public_error_code=\"repetition_detected\""));
    }

    #[test]
    fn dropped_admission_future_records_one_metadata_only_outcome() {
        use std::future::Future;

        let metrics = Arc::new(ContextMetrics::default());
        let logs = telemetry::capture_logs(|| {
            let observation = ResponseContextObservation::new(
                Uuid::from_u128(1),
                Uuid::from_u128(2),
                Uuid::from_u128(3),
                "PRIVATE_MODEL_OUTPUT",
                ContextPolicy::compaction(),
                metrics.clone(),
            );
            let mut request = Box::pin(async {
                let mut admission = ResponseAdmissionObservation::default();
                *admission.slot() = Some(observation.clone());
                let _preparation = metrics.start_preparation(false);
                metrics.helper_call_started();
                std::future::pending::<()>().await;
                admission.finish(None);
            });
            let mut context = std::task::Context::from_waker(std::task::Waker::noop());
            assert!(request.as_mut().poll(&mut context).is_pending());
            assert!(!observation.has_recorded_outcome());
            drop(request);
            assert!(observation.has_recorded_outcome());
            observation.preaccept_failed(&ApiError::InternalServerError);
        });
        assert_eq!(
            logs.matches("event=\"responses.context.outcome\"").count(),
            1
        );
        assert!(logs.contains("outcome=\"preaccept_dropped\""));
        assert!(logs.contains("error_category=\"handler_dropped\""));
        assert!(logs.contains("accepted=false"));
        assert!(logs.contains("preparation_failures=1"));
        assert!(logs.contains("helper_calls=1"));
        // A started helper with no received usage remains distinguishable from
        // zero observed work; the observation cannot infer its upstream cost.
        assert!(logs.contains("helper_usage_records=0"));
        assert!(!logs.contains("PRIVATE_"));
    }

    #[test]
    fn returned_admission_error_keeps_its_failure_outcome() {
        let logs = telemetry::capture_logs(|| {
            let observation = observation(ContextPolicy::legacy(), Arc::default());
            let mut admission = ResponseAdmissionObservation::default();
            *admission.slot() = Some(observation.clone());
            admission.finish(Some(&ApiError::InternalServerError));
            observation.terminal(ContextOutcome::Failed, None, None);
        });
        assert_eq!(
            logs.matches("event=\"responses.context.outcome\"").count(),
            1
        );
        assert!(logs.contains("outcome=\"preaccept_failed\""));
        assert!(!logs.contains("preaccept_dropped"));
        assert!(logs.contains("accepted=false"));
    }

    #[test]
    fn admitted_stream_retains_terminal_ownership_on_return_or_drop() {
        for returns in [true, false] {
            let logs = telemetry::capture_logs(|| {
                let observation = observation(ContextPolicy::compaction(), Arc::default());
                let mut admission = ResponseAdmissionObservation::default();
                *admission.slot() = Some(observation.clone());
                observation.accepted();
                if returns {
                    admission.finish(None);
                } else {
                    drop(admission);
                }
                assert!(!observation.has_recorded_outcome());
                observation.terminal(ContextOutcome::Completed, None, None);
            });
            assert_eq!(
                logs.matches("event=\"responses.context.outcome\"").count(),
                1
            );
            assert!(logs.contains("outcome=\"completed\""));
            assert!(logs.contains("accepted=true"));
            assert!(!logs.contains("preaccept_dropped"));
        }
        let logs = telemetry::capture_logs(|| drop(ResponseAdmissionObservation::default()));
        assert!(logs.is_empty());
    }

    #[test]
    fn diagnostics_keep_only_canonical_labels_and_safe_error_metadata() {
        let logs = telemetry::capture_logs(|| {
            let observation = ResponseContextObservation::new(
                Uuid::from_u128(1),
                Uuid::from_u128(2),
                Uuid::from_u128(3),
                "PRIVATE_MODEL_OUTPUT_SECRET",
                ContextPolicy::legacy(),
                Arc::new(ContextMetrics::default()),
            );
            observation.prepared_model("PRIVATE_USER_INPUT_SECRET");
            observation.terminal(
                ContextOutcome::Failed,
                Some("PRIVATE_SUMMARY_SECRET"),
                Some("PRIVATE_REASON_SECRET"),
            );
        });
        assert!(!logs.contains("PRIVATE_"));
        assert!(logs.contains("model_id=\"uncataloged\""));
        assert!(logs.contains("public_error_code=\"other\""));
        let logs = telemetry::capture_logs(|| {
            let observation = observation(ContextPolicy::legacy(), Arc::default());
            observation.preaccept_failed(&ApiError::InternalServerError);
        });
        assert!(logs.contains("outcome=\"preaccept_failed\""));
        assert!(logs.contains("error_category=\"internal\""));
        assert!(logs.contains("accepted=false"));
    }

    #[test]
    fn deleted_and_storage_failures_are_separate_from_model_outcomes() {
        for (deleted, expected) in [(true, "deleted"), (false, "storage_failed")] {
            let logs = telemetry::capture_logs(|| {
                let observation = observation(ContextPolicy::legacy(), Arc::default());
                observation.accepted();
                if deleted {
                    observation.deleted();
                } else {
                    observation.storage_failed("PRIVATE_STORAGE_PAYLOAD");
                }
                observation.terminal(ContextOutcome::Cancelled, None, None);
            });
            assert!(logs.contains(&format!("outcome=\"{expected}\"")));
            assert_eq!(
                logs.matches("event=\"responses.context.outcome\"").count(),
                1
            );
            assert!(!logs.contains("PRIVATE_"));
        }
    }
}
