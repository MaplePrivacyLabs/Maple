use std::collections::{BTreeMap, BTreeSet, VecDeque};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{
    ActionEffect, ActionErrorCode, ActionId, InvocationActor, InvocationId, InvocationTransport,
    PolicyDecision, ProgramId, Recoverability, RunId, SemanticTarget, UiControllerAccess,
};

pub const DEFAULT_ACTION_AUDIT_CAPACITY: usize = 512;

/// Descriptor-owned audit rules. Argument capture is deny-by-default and only
/// explicitly allowlisted top-level scalar fields can enter the audit ring.
#[derive(Clone, Debug, Default, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuditSpec {
    pub safe_argument_fields: BTreeSet<String>,
    /// Marks an action whose arguments may contain authentication, credential,
    /// or other secret material. Such actions must redact every argument.
    pub secret_bearing: bool,
}

impl AuditSpec {
    pub fn redact_all() -> Self {
        Self::default()
    }

    pub fn allow_fields(
        fields: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self, AuditSpecError> {
        let spec = Self {
            safe_argument_fields: fields.into_iter().map(Into::into).collect(),
            secret_bearing: false,
        };
        spec.validate()?;
        Ok(spec)
    }

    pub fn secret_bearing() -> Self {
        Self {
            safe_argument_fields: BTreeSet::new(),
            secret_bearing: true,
        }
    }

    pub fn validate(&self) -> Result<(), AuditSpecError> {
        if self.secret_bearing && !self.safe_argument_fields.is_empty() {
            return Err(AuditSpecError::SecretBearingAllowlist);
        }
        for field in &self.safe_argument_fields {
            if field.is_empty() || field.trim() != field {
                return Err(AuditSpecError::InvalidField(field.clone()));
            }
            if field_is_sensitive(field) {
                return Err(AuditSpecError::SensitiveField(field.clone()));
            }
        }
        Ok(())
    }

    pub fn redact(&self, arguments: &Value) -> RedactedArguments {
        let Some(object) = arguments.as_object() else {
            return RedactedArguments {
                fields: BTreeMap::new(),
                redacted_field_count: usize::from(!arguments.is_null()),
            };
        };

        let mut fields = BTreeMap::new();
        let mut redacted_field_count = 0;
        for (key, value) in object {
            if !self.secret_bearing
                && self.safe_argument_fields.contains(key)
                && !field_is_sensitive(key)
                && is_safe_scalar(value)
            {
                fields.insert(key.clone(), value.clone());
            } else {
                redacted_field_count += 1;
            }
        }
        RedactedArguments {
            fields,
            redacted_field_count,
        }
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum AuditSpecError {
    #[error("secret-bearing actions must redact every argument")]
    SecretBearingAllowlist,
    #[error("audit allowlist field {0:?} is empty or has surrounding whitespace")]
    InvalidField(String),
    #[error("audit allowlist field {0:?} appears sensitive and cannot be retained")]
    SensitiveField(String),
}

fn is_safe_scalar(value: &Value) -> bool {
    matches!(value, Value::Bool(_) | Value::Number(_) | Value::String(_))
}

fn field_is_sensitive(field: &str) -> bool {
    let normalized = field.to_ascii_lowercase();
    [
        "password",
        "passwd",
        "secret",
        "token",
        "credential",
        "authorization",
        "cookie",
        "oauth",
        "callback",
        "private_key",
        "api_key",
        "header",
        "environment",
        "python_code",
        "python_output",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

#[derive(Clone, Debug, Default, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RedactedArguments {
    fields: BTreeMap<String, Value>,
    redacted_field_count: usize,
}

impl RedactedArguments {
    pub fn fields(&self) -> &BTreeMap<String, Value> {
        &self.fields
    }

    pub fn redacted_field_count(&self) -> usize {
        self.redacted_field_count
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvocationActorSummary {
    DirectUser,
    Model,
    UserCode,
    Internal,
}

impl From<InvocationActor> for InvocationActorSummary {
    fn from(actor: InvocationActor) -> Self {
        match actor {
            InvocationActor::DirectUser => Self::DirectUser,
            InvocationActor::Model => Self::Model,
            InvocationActor::UserCode => Self::UserCode,
            InvocationActor::Internal => Self::Internal,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditOutcome {
    Denied,
    Started,
    Accepted,
    AcceptedTerminal,
    Completed,
    Failed,
    Cancelled,
    CompletedAfterCancelRequest,
}

impl AuditOutcome {
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Denied
                | Self::AcceptedTerminal
                | Self::Completed
                | Self::Failed
                | Self::Cancelled
                | Self::CompletedAfterCancelRequest
        )
    }
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActionAuditRecord {
    pub sequence: u64,
    pub invocation_id: InvocationId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_id: Option<ProgramId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_run_id: Option<RunId>,
    pub timestamp_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_requested_at_ms: Option<i64>,
    pub actor: InvocationActorSummary,
    pub transport: InvocationTransport,
    pub controller_access: UiControllerAccess,
    pub policy_epoch: u64,
    pub action_id: ActionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<SemanticTarget>,
    pub arguments: RedactedArguments,
    pub effect: ActionEffect,
    pub recoverability: Recoverability,
    pub decision: PolicyDecision,
    pub outcome: AuditOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<ActionErrorCode>,
}

#[derive(Clone, Debug)]
pub struct ActionAuditDraft {
    pub invocation_id: InvocationId,
    pub program_id: Option<ProgramId>,
    pub model_run_id: Option<RunId>,
    pub timestamp_ms: i64,
    pub actor: InvocationActorSummary,
    pub transport: InvocationTransport,
    pub controller_access: UiControllerAccess,
    pub policy_epoch: u64,
    pub action_id: ActionId,
    pub target: Option<SemanticTarget>,
    pub arguments: RedactedArguments,
    pub effect: ActionEffect,
    pub recoverability: Recoverability,
    pub decision: PolicyDecision,
    pub outcome: AuditOutcome,
    pub error_code: Option<ActionErrorCode>,
}

#[derive(Clone, Debug)]
pub struct ActionAuditRing {
    capacity: usize,
    next_sequence: u64,
    records: VecDeque<ActionAuditRecord>,
}

impl ActionAuditRing {
    pub fn new(capacity: usize) -> Result<Self, AuditRingError> {
        if capacity == 0 {
            return Err(AuditRingError::ZeroCapacity);
        }
        Ok(Self {
            capacity,
            next_sequence: 1,
            records: VecDeque::with_capacity(capacity),
        })
    }

    /// Retains a new audit record without ever evicting an invocation whose
    /// terminal outcome has not been observed yet.
    ///
    /// When the ring is full, the oldest terminal record is evicted. If every
    /// retained record is still pending, the caller must apply backpressure (or
    /// otherwise fail the attempted invocation) rather than losing the audit
    /// trail needed to finish an in-flight invocation.
    pub fn record(&mut self, draft: ActionAuditDraft) -> Result<ActionAuditRecord, AuditRingError> {
        if draft.outcome == AuditOutcome::CompletedAfterCancelRequest {
            return Err(AuditRingError::CompletedAfterCancelWithoutCancelRequest(
                draft.invocation_id,
            ));
        }
        if self.records.len() == self.capacity {
            let oldest_terminal = self
                .records
                .iter()
                .position(|record| record.outcome.is_terminal())
                .ok_or(AuditRingError::CapacityExceeded {
                    capacity: self.capacity,
                })?;
            self.records.remove(oldest_terminal);
        }

        let record = ActionAuditRecord {
            sequence: self.next_sequence,
            invocation_id: draft.invocation_id,
            program_id: draft.program_id,
            model_run_id: draft.model_run_id,
            timestamp_ms: draft.timestamp_ms,
            duration_ms: None,
            cancel_requested_at_ms: None,
            actor: draft.actor,
            transport: draft.transport,
            controller_access: draft.controller_access,
            policy_epoch: draft.policy_epoch,
            action_id: draft.action_id,
            target: draft.target,
            arguments: draft.arguments,
            effect: draft.effect,
            recoverability: draft.recoverability,
            decision: draft.decision,
            outcome: draft.outcome,
            error_code: draft.error_code,
        };
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.records.push_back(record.clone());
        Ok(record)
    }

    /// Records lifecycle evidence that cancellation was requested for an
    /// in-flight invocation. Repeated calls are idempotent and preserve the
    /// first request timestamp.
    pub fn mark_cancel_requested(
        &mut self,
        invocation_id: InvocationId,
        timestamp_ms: i64,
    ) -> Result<ActionAuditRecord, AuditRingError> {
        let record = self
            .records
            .iter_mut()
            .rev()
            .find(|record| record.invocation_id == invocation_id)
            .ok_or(AuditRingError::InvocationNotRetained(invocation_id))?;
        if record.outcome.is_terminal() {
            return Err(AuditRingError::AlreadyTerminal(invocation_id));
        }
        if timestamp_ms < record.timestamp_ms {
            return Err(AuditRingError::CancelRequestBeforeStart {
                invocation_id,
                started_at_ms: record.timestamp_ms,
                requested_at_ms: timestamp_ms,
            });
        }
        if record.cancel_requested_at_ms.is_none() {
            record.cancel_requested_at_ms = Some(timestamp_ms);
        }
        Ok(record.clone())
    }

    /// Records the nonterminal boundary where an executor handed ownership to
    /// a retained asynchronous operation. Replaying the same acceptance is
    /// idempotent so UI/controller delivery retries cannot manufacture a
    /// second lifecycle transition.
    pub fn accept(
        &mut self,
        invocation_id: InvocationId,
    ) -> Result<ActionAuditRecord, AuditRingError> {
        let record = self
            .records
            .iter_mut()
            .rev()
            .find(|record| record.invocation_id == invocation_id)
            .ok_or(AuditRingError::InvocationNotRetained(invocation_id))?;
        match record.outcome {
            AuditOutcome::Started => record.outcome = AuditOutcome::Accepted,
            AuditOutcome::Accepted => {}
            _ => return Err(AuditRingError::AlreadyTerminal(invocation_id)),
        }
        Ok(record.clone())
    }

    pub fn finish(
        &mut self,
        invocation_id: InvocationId,
        duration_ms: u64,
        outcome: AuditOutcome,
        error_code: Option<ActionErrorCode>,
    ) -> Result<ActionAuditRecord, AuditRingError> {
        if !outcome.is_terminal() {
            return Err(AuditRingError::NonTerminalFinish(outcome));
        }
        let record = self
            .records
            .iter_mut()
            .rev()
            .find(|record| record.invocation_id == invocation_id)
            .ok_or(AuditRingError::InvocationNotRetained(invocation_id))?;
        if record.outcome.is_terminal() {
            return Err(AuditRingError::AlreadyTerminal(invocation_id));
        }
        if outcome == AuditOutcome::CompletedAfterCancelRequest {
            if record.effect != ActionEffect::ExternalEffect
                && record.effect != ActionEffect::MutateMaple
            {
                return Err(AuditRingError::InvalidCompletedAfterCancelEffect);
            }
            if record.recoverability != Recoverability::Irreversible {
                return Err(AuditRingError::CompletedAfterCancelRequiresIrreversible);
            }
            if record.cancel_requested_at_ms.is_none() {
                return Err(AuditRingError::CompletedAfterCancelWithoutCancelRequest(
                    invocation_id,
                ));
            }
        } else if outcome == AuditOutcome::Completed
            && record.recoverability == Recoverability::Irreversible
            && record.cancel_requested_at_ms.is_some()
        {
            return Err(AuditRingError::CompletedAfterCancelOutcomeRequired(
                invocation_id,
            ));
        }
        record.duration_ms = Some(duration_ms);
        record.outcome = outcome;
        record.error_code = error_code;
        Ok(record.clone())
    }

    pub fn records(&self) -> impl DoubleEndedIterator<Item = &ActionAuditRecord> {
        self.records.iter()
    }

    pub fn recent(&self, limit: usize) -> Vec<ActionAuditRecord> {
        self.records.iter().rev().take(limit).cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

impl Default for ActionAuditRing {
    fn default() -> Self {
        Self::new(DEFAULT_ACTION_AUDIT_CAPACITY)
            .expect("default action audit ring capacity is nonzero")
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum AuditRingError {
    #[error("action audit ring capacity must be greater than zero")]
    ZeroCapacity,
    #[error("action audit ring capacity {capacity} is exhausted by nonterminal invocations")]
    CapacityExceeded { capacity: usize },
    #[error("cannot finish an audit record with nonterminal outcome {0:?}")]
    NonTerminalFinish(AuditOutcome),
    #[error("invocation {0} is not retained in the bounded audit ring")]
    InvocationNotRetained(InvocationId),
    #[error("invocation {0} already has a terminal audit outcome")]
    AlreadyTerminal(InvocationId),
    #[error(
        "cancel request for invocation {invocation_id} at {requested_at_ms} predates its audit start at {started_at_ms}"
    )]
    CancelRequestBeforeStart {
        invocation_id: InvocationId,
        started_at_ms: i64,
        requested_at_ms: i64,
    },
    #[error("completed_after_cancel_request is invalid for an observe/navigation action")]
    InvalidCompletedAfterCancelEffect,
    #[error("completed_after_cancel_request requires irreversible recoverability")]
    CompletedAfterCancelRequiresIrreversible,
    #[error("completed_after_cancel_request requires a recorded cancel request for invocation {0}")]
    CompletedAfterCancelWithoutCancelRequest(InvocationId),
    #[error(
        "invocation {0} completed after cancellation and requires completed_after_cancel_request"
    )]
    CompletedAfterCancelOutcomeRequired(InvocationId),
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{ActionId, PolicyDenialCode};

    fn draft(
        invocation_id: InvocationId,
        arguments: RedactedArguments,
        outcome: AuditOutcome,
    ) -> ActionAuditDraft {
        ActionAuditDraft {
            invocation_id,
            program_id: Some(ProgramId::new()),
            model_run_id: None,
            timestamp_ms: 1_000,
            actor: InvocationActorSummary::Model,
            transport: InvocationTransport::Python,
            controller_access: UiControllerAccess::FullAccess,
            policy_epoch: 3,
            action_id: ActionId::parse("permission.respond").unwrap(),
            target: Some(SemanticTarget::Permission {
                request_id: "request-1".into(),
            }),
            arguments,
            effect: ActionEffect::MutateMaple,
            recoverability: Recoverability::Reversible,
            decision: PolicyDecision::Allowed,
            outcome,
            error_code: None,
        }
    }

    #[test]
    fn redaction_is_descriptor_driven_and_deny_by_default() {
        let spec = AuditSpec::allow_fields(["task_id", "archived"]).unwrap();
        let redacted = spec.redact(&json!({
            "task_id": "task-1",
            "archived": true,
            "password": "do-not-log",
            "nested": {"token": "also-do-not-log"}
        }));
        assert_eq!(redacted.fields()["task_id"], "task-1");
        assert_eq!(redacted.fields()["archived"], true);
        assert_eq!(redacted.redacted_field_count(), 2);
        let encoded = serde_json::to_string(&redacted).unwrap();
        assert!(!encoded.contains("do-not-log"));
        assert!(!encoded.contains("also-do-not-log"));
        assert!(!encoded.contains("password"));
        assert!(!encoded.contains("token"));
    }

    #[test]
    fn unsafe_secret_bearing_allowlists_are_rejected() {
        assert!(AuditSpec::allow_fields(["access_token"]).is_err());
        let mut spec = AuditSpec::secret_bearing();
        spec.safe_argument_fields.insert("task_id".into());
        assert_eq!(spec.validate(), Err(AuditSpecError::SecretBearingAllowlist));
    }

    #[test]
    fn bounded_ring_evicts_oldest_and_keeps_monotonic_sequence() {
        let mut ring = ActionAuditRing::new(2).unwrap();
        let first = ring
            .record(draft(
                InvocationId::new(),
                AuditSpec::default().redact(&json!({})),
                AuditOutcome::Denied,
            ))
            .unwrap();
        ring.record(draft(
            InvocationId::new(),
            AuditSpec::default().redact(&json!({})),
            AuditOutcome::Denied,
        ))
        .unwrap();
        let third = ring
            .record(draft(
                InvocationId::new(),
                AuditSpec::default().redact(&json!({})),
                AuditOutcome::Denied,
            ))
            .unwrap();
        assert_eq!(ring.len(), 2);
        assert_eq!(first.sequence, 1);
        assert_eq!(third.sequence, 3);
        assert!(ring.records().all(|record| record.sequence != 1));
    }

    #[test]
    fn full_ring_never_evicts_nonterminal_records() {
        let first_id = InvocationId::new();
        let second_id = InvocationId::new();
        let rejected_id = InvocationId::new();
        let mut ring = ActionAuditRing::new(2).unwrap();
        let first = ring
            .record(draft(
                first_id,
                AuditSpec::default().redact(&json!({})),
                AuditOutcome::Started,
            ))
            .unwrap();
        let second = ring
            .record(draft(
                second_id,
                AuditSpec::default().redact(&json!({})),
                AuditOutcome::Accepted,
            ))
            .unwrap();

        assert_eq!(
            ring.record(draft(
                rejected_id,
                AuditSpec::default().redact(&json!({})),
                AuditOutcome::Denied,
            )),
            Err(AuditRingError::CapacityExceeded { capacity: 2 })
        );
        assert_eq!(
            ring.records()
                .map(|record| (record.sequence, record.invocation_id))
                .collect::<Vec<_>>(),
            vec![(first.sequence, first_id), (second.sequence, second_id)]
        );

        let completed = ring
            .finish(first_id, 17, AuditOutcome::Completed, None)
            .unwrap();
        assert_eq!(completed.sequence, first.sequence);

        let replacement = ring
            .record(draft(
                rejected_id,
                AuditSpec::default().redact(&json!({})),
                AuditOutcome::Denied,
            ))
            .unwrap();
        assert_eq!(replacement.sequence, 3);
        assert_eq!(
            ring.records()
                .map(|record| record.invocation_id)
                .collect::<Vec<_>>(),
            vec![second_id, rejected_id]
        );
    }

    #[test]
    fn full_ring_evicts_oldest_terminal_even_when_pending_record_is_older() {
        let pending_id = InvocationId::new();
        let first_terminal_id = InvocationId::new();
        let second_terminal_id = InvocationId::new();
        let replacement_id = InvocationId::new();
        let mut ring = ActionAuditRing::new(3).unwrap();

        ring.record(draft(
            pending_id,
            AuditSpec::default().redact(&json!({})),
            AuditOutcome::Started,
        ))
        .unwrap();
        ring.record(draft(
            first_terminal_id,
            AuditSpec::default().redact(&json!({})),
            AuditOutcome::Denied,
        ))
        .unwrap();
        ring.record(draft(
            second_terminal_id,
            AuditSpec::default().redact(&json!({})),
            AuditOutcome::Completed,
        ))
        .unwrap();

        ring.record(draft(
            replacement_id,
            AuditSpec::default().redact(&json!({})),
            AuditOutcome::Denied,
        ))
        .unwrap();

        assert_eq!(
            ring.records()
                .map(|record| record.invocation_id)
                .collect::<Vec<_>>(),
            vec![pending_id, second_terminal_id, replacement_id]
        );
        assert!(
            ring.finish(pending_id, 23, AuditOutcome::Completed, None)
                .is_ok()
        );
    }

    #[test]
    fn async_audit_is_not_completed_until_terminal_result_is_known() {
        let invocation_id = InvocationId::new();
        let mut ring = ActionAuditRing::new(8).unwrap();
        let started = ring
            .record(draft(
                invocation_id,
                AuditSpec::default().redact(&json!({})),
                AuditOutcome::Started,
            ))
            .unwrap();
        assert_eq!(started.outcome, AuditOutcome::Started);
        assert_eq!(started.duration_ms, None);

        let accepted = ring.accept(invocation_id).unwrap();
        assert_eq!(accepted.outcome, AuditOutcome::Accepted);
        assert_eq!(ring.accept(invocation_id).unwrap(), accepted);
        assert_eq!(accepted.duration_ms, None);

        let completed = ring
            .finish(invocation_id, 42, AuditOutcome::Completed, None)
            .unwrap();
        assert_eq!(completed.outcome, AuditOutcome::Completed);
        assert_eq!(completed.duration_ms, Some(42));
        assert!(matches!(
            ring.finish(invocation_id, 50, AuditOutcome::Cancelled, None),
            Err(AuditRingError::AlreadyTerminal(_))
        ));
    }

    #[test]
    fn completed_after_cancel_requires_irreversible_recoverability_and_cancel_evidence() {
        let reversible_id = InvocationId::new();
        let unmarked_irreversible_id = InvocationId::new();
        let raced_id = InvocationId::new();
        let mut ring = ActionAuditRing::new(4).unwrap();

        ring.record(draft(
            reversible_id,
            AuditSpec::default().redact(&json!({})),
            AuditOutcome::Accepted,
        ))
        .unwrap();
        ring.mark_cancel_requested(reversible_id, 1_100).unwrap();
        assert_eq!(
            ring.finish(
                reversible_id,
                100,
                AuditOutcome::CompletedAfterCancelRequest,
                None,
            ),
            Err(AuditRingError::CompletedAfterCancelRequiresIrreversible)
        );

        let mut unmarked_irreversible = draft(
            unmarked_irreversible_id,
            AuditSpec::default().redact(&json!({})),
            AuditOutcome::Accepted,
        );
        unmarked_irreversible.recoverability = Recoverability::Irreversible;
        ring.record(unmarked_irreversible).unwrap();
        assert_eq!(
            ring.finish(
                unmarked_irreversible_id,
                100,
                AuditOutcome::CompletedAfterCancelRequest,
                None,
            ),
            Err(AuditRingError::CompletedAfterCancelWithoutCancelRequest(
                unmarked_irreversible_id
            ))
        );

        let mut raced = draft(
            raced_id,
            AuditSpec::default().redact(&json!({})),
            AuditOutcome::Accepted,
        );
        raced.recoverability = Recoverability::Irreversible;
        ring.record(raced).unwrap();
        let marked = ring.mark_cancel_requested(raced_id, 1_125).unwrap();
        assert_eq!(marked.cancel_requested_at_ms, Some(1_125));
        let completed = ring
            .finish(
                raced_id,
                125,
                AuditOutcome::CompletedAfterCancelRequest,
                None,
            )
            .unwrap();
        assert_eq!(completed.recoverability, Recoverability::Irreversible);
        assert_eq!(completed.cancel_requested_at_ms, Some(1_125));
        assert_eq!(completed.outcome, AuditOutcome::CompletedAfterCancelRequest);
        let encoded = serde_json::to_value(completed).unwrap();
        assert_eq!(encoded["recoverability"], "irreversible");
        assert_eq!(encoded["cancel_requested_at_ms"], 1_125);
    }

    #[test]
    fn irreversible_completion_after_cancel_must_use_race_outcome() {
        let invocation_id = InvocationId::new();
        let mut ring = ActionAuditRing::new(2).unwrap();
        let mut accepted = draft(
            invocation_id,
            AuditSpec::default().redact(&json!({})),
            AuditOutcome::Accepted,
        );
        accepted.recoverability = Recoverability::Irreversible;
        ring.record(accepted).unwrap();
        ring.mark_cancel_requested(invocation_id, 1_050).unwrap();

        assert_eq!(
            ring.finish(invocation_id, 50, AuditOutcome::Completed, None),
            Err(AuditRingError::CompletedAfterCancelOutcomeRequired(
                invocation_id
            ))
        );
        assert!(
            ring.finish(
                invocation_id,
                50,
                AuditOutcome::CompletedAfterCancelRequest,
                None,
            )
            .is_ok()
        );
    }

    #[test]
    fn cancel_request_evidence_is_ordered_idempotent_and_only_for_inflight_records() {
        let invocation_id = InvocationId::new();
        let mut ring = ActionAuditRing::new(2).unwrap();
        ring.record(draft(
            invocation_id,
            AuditSpec::default().redact(&json!({})),
            AuditOutcome::Started,
        ))
        .unwrap();

        assert_eq!(
            ring.mark_cancel_requested(invocation_id, 999),
            Err(AuditRingError::CancelRequestBeforeStart {
                invocation_id,
                started_at_ms: 1_000,
                requested_at_ms: 999,
            })
        );
        ring.mark_cancel_requested(invocation_id, 1_025).unwrap();
        let repeated = ring.mark_cancel_requested(invocation_id, 1_030).unwrap();
        assert_eq!(repeated.cancel_requested_at_ms, Some(1_025));
        ring.finish(invocation_id, 25, AuditOutcome::Cancelled, None)
            .unwrap();
        assert_eq!(
            ring.mark_cancel_requested(invocation_id, 1_040),
            Err(AuditRingError::AlreadyTerminal(invocation_id))
        );
    }

    #[test]
    fn completed_after_cancel_cannot_be_inserted_without_lifecycle_evidence() {
        let invalid_id = InvocationId::new();
        let valid_id = InvocationId::new();
        let mut ring = ActionAuditRing::new(1).unwrap();
        let mut invalid = draft(
            invalid_id,
            AuditSpec::default().redact(&json!({})),
            AuditOutcome::CompletedAfterCancelRequest,
        );
        invalid.recoverability = Recoverability::Irreversible;
        assert_eq!(
            ring.record(invalid),
            Err(AuditRingError::CompletedAfterCancelWithoutCancelRequest(
                invalid_id
            ))
        );
        assert!(ring.is_empty());
        assert_eq!(
            ring.record(draft(
                valid_id,
                AuditSpec::default().redact(&json!({})),
                AuditOutcome::Denied,
            ))
            .unwrap()
            .sequence,
            1
        );
    }

    #[test]
    fn denied_decision_is_serialized_without_sensitive_arguments() {
        let invocation_id = InvocationId::new();
        let mut denied = draft(
            invocation_id,
            AuditSpec::default().redact(&json!({"secret": "never"})),
            AuditOutcome::Denied,
        );
        denied.decision = PolicyDecision::Denied {
            code: PolicyDenialCode::ReadOnlyMutation,
            message: "Read Only cannot respond".into(),
        };
        let record = ActionAuditRing::new(4).unwrap().record(denied).unwrap();
        let encoded = serde_json::to_string(&record).unwrap();
        assert!(encoded.contains("read_only_mutation"));
        assert!(!encoded.contains("never"));
    }
}
