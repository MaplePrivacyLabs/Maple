use crate::model_config::{ModelPlan, AUTO_POWERFUL_MODEL_ID, AUTO_QUICK_MODEL_ID};
use crate::provider_registry::{ProviderId, RouteSelectionSource};
use auto_model::{AutoModelDecision, AutoModelReason};
use std::fmt;
use std::time::Duration;
use uuid::Uuid;

pub(crate) mod auto_model;
pub(crate) mod health;
pub(crate) mod sticky_routes;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelSelectionMode {
    AutoQuick,
    AutoPowerful,
    Explicit,
}

impl ModelSelectionMode {
    pub(crate) fn from_requested_model(model: &str) -> Self {
        match model {
            AUTO_QUICK_MODEL_ID => Self::AutoQuick,
            AUTO_POWERFUL_MODEL_ID => Self::AutoPowerful,
            _ => Self::Explicit,
        }
    }

    pub(crate) const fn is_auto(self) -> bool {
        matches!(self, Self::AutoQuick | Self::AutoPowerful)
    }

    /// The public alias string for an Auto mode, the inverse of
    /// [`Self::from_requested_model`].
    pub(crate) const fn alias(self) -> Option<&'static str> {
        match self {
            Self::AutoQuick => Some(AUTO_QUICK_MODEL_ID),
            Self::AutoPowerful => Some(AUTO_POWERFUL_MODEL_ID),
            Self::Explicit => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum InferenceSurface {
    ChatCompletions,
    Responses,
    Internal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkloadClass {
    Interactive,
    Background,
}

macro_rules! inference_id {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub(crate) struct $name(Uuid);

        impl $name {
            fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

inference_id!(InferenceRequestId);
inference_id!(InferenceExecutionId);
inference_id!(InferenceAttemptId);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InferenceIntent {
    pub(crate) request_id: InferenceRequestId,
    pub(crate) account_uuid: Uuid,
    /// The caller's selector: an Auto alias or an explicit public model.
    pub(crate) requested_model_id: String,
    /// The model this logical request executes. Provider planning, context
    /// preparation, persistence, and usage all use this identity.
    pub(crate) public_model_id: String,
    /// The Router v2 Auto decision that produced `public_model_id`, when one
    /// ran. Explicit selections and Router v1 requests carry none.
    pub(crate) auto_model: Option<AutoModelDecision>,
    pub(crate) selection_mode: ModelSelectionMode,
    pub(crate) model_plan: ModelPlan,
    pub(crate) surface: InferenceSurface,
    pub(crate) workload_class: WorkloadClass,
}

impl InferenceIntent {
    pub(crate) fn new(
        account_uuid: Uuid,
        requested_model_id: impl Into<String>,
        public_model_id: impl Into<String>,
        model_plan: ModelPlan,
        surface: InferenceSurface,
        workload_class: WorkloadClass,
    ) -> Self {
        let requested_model_id = requested_model_id.into();
        Self {
            request_id: InferenceRequestId::new(),
            account_uuid,
            selection_mode: ModelSelectionMode::from_requested_model(&requested_model_id),
            requested_model_id,
            public_model_id: public_model_id.into(),
            auto_model: None,
            model_plan,
            surface,
            workload_class,
        }
    }

    /// Applies a finished Router v2 Auto model decision. The chosen model
    /// becomes the executed identity while the decision stays visible as
    /// intent metadata.
    pub(crate) fn with_auto_model_decision(mut self, decision: AutoModelDecision) -> Self {
        debug_assert_eq!(decision.selector, self.requested_model_id);
        self.public_model_id = decision.chosen_model_id.to_string();
        self.auto_model = Some(decision);
        self
    }

    /// The model the selector resolves to when healthy. Differs from
    /// `public_model_id` only after an Auto decision chose an alternate.
    pub(crate) fn preferred_model_id(&self) -> &str {
        self.auto_model
            .as_ref()
            .map_or(self.public_model_id.as_str(), |decision| {
                decision.preferred_model_id
            })
    }

    pub(crate) fn auto_model_reason(&self) -> Option<AutoModelReason> {
        self.auto_model.as_ref().map(|decision| decision.reason)
    }

    pub(crate) fn begin_execution(&self) -> InferenceExecution {
        InferenceExecution {
            request_id: self.request_id,
            execution_id: InferenceExecutionId::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RouteIdentity {
    pub(crate) provider: ProviderId,
    pub(crate) public_model_id: String,
    pub(crate) provider_model_id: String,
    pub(crate) response_model_id: String,
    pub(crate) selection_source: RouteSelectionSource,
    pub(crate) bucket: Option<u8>,
}

impl RouteIdentity {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        provider: ProviderId,
        public_model_id: impl Into<String>,
        provider_model_id: impl Into<String>,
        response_model_id: impl Into<String>,
        selection_source: RouteSelectionSource,
        bucket: Option<u8>,
    ) -> Self {
        Self {
            provider,
            public_model_id: public_model_id.into(),
            provider_model_id: provider_model_id.into(),
            response_model_id: response_model_id.into(),
            selection_source,
            bucket,
        }
    }

    pub(crate) fn route_key(&self) -> RouteKey {
        RouteKey {
            provider: self.provider,
            provider_model_id: self.provider_model_id.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RouteKey {
    pub(crate) provider: ProviderId,
    pub(crate) provider_model_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InferenceExecution {
    pub(crate) request_id: InferenceRequestId,
    pub(crate) execution_id: InferenceExecutionId,
}

impl InferenceExecution {
    pub(crate) fn begin_attempt(&self, route: RouteIdentity) -> InferenceAttempt {
        InferenceAttempt {
            request_id: self.request_id,
            execution_id: self.execution_id,
            attempt_id: InferenceAttemptId::new(),
            route,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InferenceAttempt {
    pub(crate) request_id: InferenceRequestId,
    pub(crate) execution_id: InferenceExecutionId,
    pub(crate) attempt_id: InferenceAttemptId,
    pub(crate) route: RouteIdentity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttemptStage {
    BeforeSend,
    AwaitingResponse,
    ResponseBody,
    Stream,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplaySafety {
    ProvenPreAcceptance,
    NotProvenPreAcceptance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttemptFailureKind {
    ProviderUnavailable,
    RequestBuild,
    Connect,
    Transport,
    ResponseStartTimeout,
    HttpStatus,
    CapacityRejected,
    ResponseBody,
    InvalidResponse,
    UpstreamResponseError,
    UpstreamStreamError,
    StreamTimeout,
    UnexpectedEof,
    ConsumerDropped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttemptFailure {
    pub(crate) kind: AttemptFailureKind,
    pub(crate) stage: AttemptStage,
    pub(crate) replay_safety: ReplaySafety,
    pub(crate) status: Option<u16>,
    pub(crate) retry_after: Option<Duration>,
    pub(crate) upstream_request_id: Option<String>,
    pub(crate) upstream_code: Option<String>,
}

impl AttemptFailure {
    pub(crate) const fn new(
        kind: AttemptFailureKind,
        stage: AttemptStage,
        replay_safety: ReplaySafety,
    ) -> Self {
        Self {
            kind,
            stage,
            replay_safety,
            status: None,
            retry_after: None,
            upstream_request_id: None,
            upstream_code: None,
        }
    }

    pub(crate) fn with_upstream_response(
        mut self,
        status: u16,
        retry_after: Option<Duration>,
        upstream_request_id: Option<String>,
    ) -> Self {
        self.status = Some(status);
        self.retry_after = retry_after;
        self.upstream_request_id = upstream_request_id;
        self
    }

    pub(crate) fn with_upstream_code(mut self, upstream_code: Option<String>) -> Self {
        self.upstream_code = upstream_code;
        self
    }

    pub(crate) const fn client_message(&self) -> &'static str {
        match self.kind {
            AttemptFailureKind::InvalidResponse => "Invalid response from inference provider",
            AttemptFailureKind::StreamTimeout => "Inference provider stream timed out",
            AttemptFailureKind::UnexpectedEof => "Inference provider stream ended unexpectedly",
            _ => "Inference provider request failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompletionEvidence {
    NonStreamingResponse,
    ProviderDone,
    FinishSignalThenEof,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AttemptTerminal {
    Completed {
        attempt: InferenceAttempt,
        evidence: CompletionEvidence,
    },
    Failed {
        attempt: InferenceAttempt,
        failure: AttemptFailure,
    },
}

impl AttemptTerminal {
    pub(crate) fn attempt(&self) -> &InferenceAttempt {
        match self {
            Self::Completed { attempt, .. } | Self::Failed { attempt, .. } => attempt,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AttemptOutcome {
    ResponseStarted {
        attempt: InferenceAttempt,
        status: u16,
    },
    Terminal(AttemptTerminal),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route() -> RouteIdentity {
        RouteIdentity::new(
            ProviderId::Tinfoil,
            "kimi-k3",
            "kimi-k3",
            "kimi-k3",
            RouteSelectionSource::StaticSplit,
            None,
        )
    }

    #[test]
    fn intent_preserves_requested_auto_selector_and_resolved_public_model() {
        let intent = InferenceIntent::new(
            Uuid::nil(),
            AUTO_POWERFUL_MODEL_ID,
            "glm-5-3",
            ModelPlan::Paid,
            InferenceSurface::Responses,
            WorkloadClass::Interactive,
        );

        assert_eq!(intent.requested_model_id, AUTO_POWERFUL_MODEL_ID);
        assert_eq!(intent.public_model_id, "glm-5-3");
        assert_eq!(intent.selection_mode, ModelSelectionMode::AutoPowerful);
        assert!(intent.selection_mode.is_auto());
    }

    #[test]
    fn explicit_intent_remains_explicit() {
        let intent = InferenceIntent::new(
            Uuid::nil(),
            "kimi-k3",
            "kimi-k3",
            ModelPlan::Paid,
            InferenceSurface::ChatCompletions,
            WorkloadClass::Interactive,
        );

        assert_eq!(intent.selection_mode, ModelSelectionMode::Explicit);
        assert!(!intent.selection_mode.is_auto());
    }

    #[test]
    fn executions_and_attempts_keep_parent_ids_and_get_unique_ids() {
        let intent = InferenceIntent::new(
            Uuid::nil(),
            "glm-5-3",
            "glm-5-3",
            ModelPlan::Paid,
            InferenceSurface::Responses,
            WorkloadClass::Interactive,
        );
        let first_execution = intent.begin_execution();
        let second_execution = intent.begin_execution();
        let first_attempt = first_execution.begin_attempt(route());
        let second_attempt = first_execution.begin_attempt(route());

        assert_eq!(first_execution.request_id, intent.request_id);
        assert_eq!(second_execution.request_id, intent.request_id);
        assert_ne!(first_execution.execution_id, second_execution.execution_id);
        assert_eq!(first_attempt.execution_id, first_execution.execution_id);
        assert_eq!(second_attempt.execution_id, first_execution.execution_id);
        assert_ne!(first_attempt.attempt_id, second_attempt.attempt_id);
        assert_ne!(
            first_attempt.attempt_id.to_string(),
            Uuid::nil().to_string()
        );
    }

    #[test]
    fn route_key_uses_typed_provider_and_upstream_model() {
        let route = RouteIdentity::new(
            ProviderId::Continuum,
            "glm-5-3",
            "glm-5.3",
            "glm-5-3",
            RouteSelectionSource::FeatureFlag,
            None,
        );

        assert_eq!(
            route.route_key(),
            RouteKey {
                provider: ProviderId::Continuum,
                provider_model_id: "glm-5.3".to_string(),
            }
        );
    }
}
