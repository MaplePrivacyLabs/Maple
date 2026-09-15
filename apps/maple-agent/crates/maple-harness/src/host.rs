//! The single action host, re-expressed without a window.
//!
//! In the original `maple-gpui` Developer Preview this lived in
//! `app/src/harness/host.rs` (about 2,400 lines) and was welded to GPUI:
//! typed GPUI actions, pointer callbacks, the command palette, application
//! Vim, and the Code Mode controller bridge all funnelled into one executor.
//! This module is a from-scratch, GPUI-free re-statement of that executor so
//! the *ideas* compile and are tested even though nothing renders:
//!
//! 1. **One path.** Every origin becomes an [`ActionCall`] plus a
//!    host-minted [`TrustedInvocation`], and every call goes through
//!    [`ActionHost::dispatch`]. There is no second business-logic entry point.
//! 2. **Descriptor first.** Unknown IDs, schema-invalid arguments, and
//!    undeclared preconditions fail before any authority check.
//! 3. **Authority is host state.** Wire callers never carry authority. The
//!    host attaches the *current* [`ProgrammabilityAuthority`] and policy
//!    epoch; a lease minted under an older epoch fails closed.
//! 4. **Availability, then compare-and-set.** Executors report availability
//!    against live state; call preconditions are checked against the
//!    [`RevisionTracker`] so stale model programs cannot act on old state.
//! 5. **Generic actions are never authority shortcuts.** An executor may
//!    answer [`ExecutorOutcome::Delegate`] with the concrete call it resolved
//!    (for example `ui.activate_selected` resolving to `account.sign_out`);
//!    the host re-enters the full pipeline for the concrete descriptor with a
//!    derived invocation, so a Human Only target stays Human Only.
//! 6. **Audit is not optional.** Denials, starts, acceptances, and terminal
//!    outcomes all land in the bounded, redacting [`ActionAuditRing`].
//! 7. **Terminal host actions.** An action such as `app.quit` commits its
//!    audit record and returns `AcceptedTerminal`, after which the host stops
//!    admitting calls; orderly shutdown belongs to the embedding process.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{
    ActionAuditDraft, ActionAuditRecord, ActionAuditRing, ActionBudget, ActionCall,
    ActionDescriptor, ActionError, ActionErrorCode, ActionId, ActionRegistry, ActionResponse,
    ActionStatus, AuditOutcome, Availability, DEFAULT_ACTION_AUDIT_CAPACITY, DirectUserIngress,
    DirectUserProvenance, ExecutionId, InvocationActor, InvocationId, InvocationTransport,
    PolicyDecision, ProgramId, ProgrammabilityAuthority, PythonCodeMode, RevisionChange,
    RevisionDomain, RevisionTracker, RunId, StaleRevision, TaskIdentity, TrustedInvocation,
    TrustedInvocationError, UiControllerAccess,
};

/// Default per-invocation admission budget for a direct-user gesture: one
/// gesture, one action, plus room for a single generic-to-concrete delegation.
pub const DIRECT_USER_ACTION_BUDGET: u32 = 2;

/// Wall-clock source so tests can pin timestamps and durations.
pub trait HostClock: Send {
    fn now_ms(&self) -> i64;
}

/// System clock used by real hosts.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl HostClock for SystemClock {
    fn now_ms(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}

/// Host-owned mutable state that executors may read and bump.
///
/// Executors receive `&mut HostState` rather than the whole host so they can
/// never re-enter dispatch or touch the audit ring directly.
#[derive(Debug)]
pub struct HostState {
    revisions: RevisionTracker,
    authority: ProgrammabilityAuthority,
    policy_epoch: u64,
    shutdown_committed: bool,
    /// Free-form application state for executors. The real host owned typed
    /// GPUI models here; a JSON object is enough to preserve the contract.
    pub model: Value,
}

impl Default for HostState {
    fn default() -> Self {
        Self {
            revisions: RevisionTracker::new(),
            authority: ProgrammabilityAuthority::OFF,
            policy_epoch: 0,
            shutdown_committed: false,
            model: json!({}),
        }
    }
}

impl HostState {
    pub fn revisions(&self) -> &RevisionTracker {
        &self.revisions
    }

    pub fn authority(&self) -> ProgrammabilityAuthority {
        self.authority
    }

    pub fn policy_epoch(&self) -> u64 {
        self.policy_epoch
    }

    pub fn is_shutdown_committed(&self) -> bool {
        self.shutdown_committed
    }

    /// Records a state change in one revision domain. Returns the new global
    /// and domain revisions so the executor can report `state_revision`.
    pub fn bump(&mut self, domain: RevisionDomain) -> RevisionChange {
        self.revisions.bump(domain)
    }
}

/// What a policy change revoked. Section 10.4 of the design: policy is
/// revisioned, and every downgrade cancels the work it no longer permits.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PolicyRevocation {
    pub new_policy_epoch: u64,
    /// Python Code Mode turned off: kernels stop, executions cancel.
    pub stop_kernels: bool,
    /// Controller lost Full Access: pending mutation leases are void.
    pub revoke_mutation_leases: bool,
    /// Controller turned off: all SDK requests and event waits are void.
    pub revoke_all_controller_requests: bool,
}

/// Result of one executor run.
#[derive(Debug)]
pub enum ExecutorOutcome {
    /// Synchronous completion. `bumped` is the revision change the executor
    /// applied, if any, and is echoed back as `state_revision`.
    Completed {
        result: Value,
        bumped: Option<RevisionChange>,
    },
    /// The executor handed the work to a retained asynchronous operation.
    /// The host records `Accepted`; the embedding process later calls
    /// [`ActionHost::finish_accepted`].
    Accepted {
        result: Value,
    },
    /// A generic action resolved to a concrete one. The host re-dispatches
    /// the concrete call under a derived invocation (final-action
    /// reauthorization).
    Delegate(ActionCall),
    Failed(ActionError),
}

/// The application-owned implementation of one semantic action.
pub trait Executor: Send {
    /// Live availability for this call. Called on every dispatch after policy.
    fn availability(&self, call: &ActionCall, state: &HostState) -> Availability {
        let _ = (call, state);
        Availability::Available
    }

    fn execute(
        &mut self,
        call: &ActionCall,
        invocation: &TrustedInvocation,
        state: &mut HostState,
    ) -> ExecutorOutcome;
}

/// Provenance attached by the Code Mode kernel to a controller request.
///
/// This mirrors the shape the Python SDK bridge supplied; none of these
/// fields grant authority. Authority is read from the host at dispatch time.
#[derive(Clone, Debug)]
pub struct ControllerOrigin {
    pub actor: InvocationActor,
    pub source_task: Option<TaskIdentity>,
    pub program_id: ProgramId,
    pub program_started_unix_ms: u64,
    pub execution_id: ExecutionId,
    pub model_run_id: Option<RunId>,
    pub kernel_generation: u64,
    /// Epoch the kernel observed when the program was admitted. If the host
    /// epoch moved on, the call fails closed with `PolicyDenied`.
    pub observed_policy_epoch: u64,
    pub cancellation: CancellationToken,
    pub budget: ActionBudget,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum HostBuildError {
    #[error("executor registered for unknown action {0}")]
    ExecutorForUnknownAction(ActionId),
    #[error("action {0} has no executor")]
    MissingExecutor(ActionId),
    #[error("duplicate executor for action {0}")]
    DuplicateExecutor(ActionId),
}

/// Builder that enforces "every descriptor has exactly one executor".
pub struct ActionHostBuilder {
    registry: ActionRegistry,
    executors: BTreeMap<ActionId, Box<dyn Executor>>,
    errors: Vec<HostBuildError>,
    audit_capacity: usize,
    clock: Box<dyn HostClock>,
}

impl ActionHostBuilder {
    pub fn new(registry: ActionRegistry) -> Self {
        Self {
            registry,
            executors: BTreeMap::new(),
            errors: Vec::new(),
            audit_capacity: DEFAULT_ACTION_AUDIT_CAPACITY,
            clock: Box::new(SystemClock),
        }
    }

    pub fn executor(mut self, action_id: ActionId, executor: impl Executor + 'static) -> Self {
        if !self.registry.contains(&action_id) {
            self.errors
                .push(HostBuildError::ExecutorForUnknownAction(action_id));
            return self;
        }
        if self.executors.contains_key(&action_id) {
            self.errors
                .push(HostBuildError::DuplicateExecutor(action_id));
            return self;
        }
        self.executors.insert(action_id, Box::new(executor));
        self
    }

    pub fn audit_capacity(mut self, capacity: usize) -> Self {
        self.audit_capacity = capacity;
        self
    }

    pub fn clock(mut self, clock: impl HostClock + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    pub fn build(mut self) -> Result<ActionHost, Vec<HostBuildError>> {
        for (id, _) in self.registry.iter() {
            if !self.executors.contains_key(id) {
                self.errors
                    .push(HostBuildError::MissingExecutor(id.clone()));
            }
        }
        if !self.errors.is_empty() {
            return Err(self.errors);
        }
        Ok(ActionHost {
            registry: self.registry,
            executors: self.executors,
            ingress: DirectUserIngress::new_window(),
            audit: ActionAuditRing::new(self.audit_capacity.max(1)).expect("capacity is non-zero"),
            state: HostState::default(),
            clock: self.clock,
        })
    }
}

/// The one action executor for an application instance.
pub struct ActionHost {
    registry: ActionRegistry,
    executors: BTreeMap<ActionId, Box<dyn Executor>>,
    ingress: DirectUserIngress,
    audit: ActionAuditRing,
    state: HostState,
    clock: Box<dyn HostClock>,
}

impl ActionHost {
    pub fn registry(&self) -> &ActionRegistry {
        &self.registry
    }

    pub fn state(&self) -> &HostState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut HostState {
        &mut self.state
    }

    pub fn audit(&self) -> &ActionAuditRing {
        &self.audit
    }

    /// The window-scoped direct-user ingress. Only physical input handlers
    /// may hold this; it is how pointer/keyboard/palette gestures prove they
    /// are human.
    pub fn direct_user_ingress(&self) -> &DirectUserIngress {
        &self.ingress
    }

    /// Changes session authority. Every change bumps the policy epoch so any
    /// invocation admitted earlier fails closed on its next action.
    pub fn set_authority(&mut self, authority: ProgrammabilityAuthority) -> PolicyRevocation {
        let previous = self.state.authority;
        let next = authority.with_code_mode(authority.code_mode);
        self.state.authority = next;
        self.state.policy_epoch = self.state.policy_epoch.saturating_add(1);
        PolicyRevocation {
            new_policy_epoch: self.state.policy_epoch,
            stop_kernels: previous.code_mode == PythonCodeMode::DeveloperPreview
                && next.code_mode != PythonCodeMode::DeveloperPreview,
            revoke_mutation_leases: previous.controller == UiControllerAccess::FullAccess
                && next.controller != UiControllerAccess::FullAccess,
            revoke_all_controller_requests: previous.controller != UiControllerAccess::Off
                && next.controller == UiControllerAccess::Off,
        }
    }

    /// Mints a direct-user invocation from opaque physical provenance. This is
    /// the *only* way to obtain `InvocationActor::DirectUser`.
    pub fn direct_user_invocation(
        &self,
        provenance: DirectUserProvenance,
        source_task: Option<TaskIdentity>,
    ) -> Result<TrustedInvocation, TrustedInvocationError> {
        self.ingress.trusted_invocation(
            InvocationId::new(),
            provenance,
            source_task,
            self.state.authority.controller,
            self.state.policy_epoch,
            CancellationToken::new(),
            ActionBudget::new(DIRECT_USER_ACTION_BUDGET).expect("non-zero budget"),
        )
    }

    /// Mints a controller (model / user code) invocation. The controller
    /// access recorded on the lease is the host's *current* value, never one
    /// supplied by the caller; the caller's observed epoch is preserved so a
    /// stale program fails closed at admission.
    pub fn controller_invocation(
        &self,
        origin: &ControllerOrigin,
    ) -> Result<TrustedInvocation, TrustedInvocationError> {
        let invocation = TrustedInvocation::new(
            InvocationId::new(),
            origin.source_task.clone(),
            Some(origin.program_id),
            origin.model_run_id.clone(),
            Some(origin.execution_id),
            Some(origin.kernel_generation),
            origin.actor,
            InvocationTransport::Python,
            self.state.authority.controller,
            origin.observed_policy_epoch,
            origin.cancellation.clone(),
            origin.budget.clone(),
        )?;
        Ok(invocation.with_program_started_unix_ms(origin.program_started_unix_ms))
    }

    /// Convenience: dispatch a pointer gesture (button, menu item).
    pub fn invoke_pointer(&mut self, call: ActionCall) -> ActionResponse {
        let provenance = self.ingress.pointer();
        match self.direct_user_invocation(provenance, None) {
            Ok(invocation) => self.dispatch(call, invocation),
            Err(error) => self.wiring_failure(call, error),
        }
    }

    /// Convenience: dispatch a command-palette activation.
    pub fn invoke_palette(&mut self, call: ActionCall) -> ActionResponse {
        let provenance = self.ingress.command_palette();
        match self.direct_user_invocation(provenance, None) {
            Ok(invocation) => self.dispatch(call, invocation),
            Err(error) => self.wiring_failure(call, error),
        }
    }

    /// Convenience: dispatch a Code Mode / model controller request.
    pub fn invoke_controller(
        &mut self,
        call: ActionCall,
        origin: &ControllerOrigin,
    ) -> ActionResponse {
        match self.controller_invocation(origin) {
            Ok(invocation) => self.dispatch(call, invocation),
            Err(error) => self.wiring_failure(call, error),
        }
    }

    fn wiring_failure(&self, call: ActionCall, error: TrustedInvocationError) -> ActionResponse {
        ActionResponse::failed(
            InvocationId::new(),
            call.action_id,
            ActionError::new(ActionErrorCode::PolicyDenied, error.to_string())
                .expect("policy_denied needs no reason code"),
        )
        .expect("failed response is well-formed")
    }

    /// The one execution path.
    pub fn dispatch(&mut self, call: ActionCall, invocation: TrustedInvocation) -> ActionResponse {
        let invocation_id = invocation.invocation_id();
        let action_id = call.action_id.clone();

        if self.state.shutdown_committed {
            return self.fail(
                invocation_id,
                action_id,
                ActionErrorCode::Failed,
                "host shutdown committed; no new actions admitted",
            );
        }

        // 1. Descriptor and structural validation come before authority.
        let Some(descriptor) = self.registry.descriptor(&action_id).cloned() else {
            return self.fail(
                invocation_id,
                action_id,
                ActionErrorCode::UnknownAction,
                "unknown action",
            );
        };
        if let Err(error) = descriptor.validate_arguments(&call.arguments) {
            return self.fail(
                invocation_id,
                action_id,
                ActionErrorCode::InvalidArguments,
                error.to_string(),
            );
        }
        if let Err(error) = descriptor.validate_precondition(&call) {
            return self.fail(
                invocation_id,
                action_id,
                ActionErrorCode::InvalidArguments,
                error.to_string(),
            );
        }

        // 2. Lease freshness and budget. A stale epoch or cancelled program
        //    never reaches an executor.
        if let Err(error) = invocation.admit_call(self.state.policy_epoch) {
            let code = if invocation.cancellation().is_cancelled() {
                ActionErrorCode::Cancelled
            } else {
                ActionErrorCode::PolicyDenied
            };
            return self.fail(invocation_id, action_id, code, error.to_string());
        }

        // 3. Authority matrix (actor x transport x controller mode x effect).
        let decision = invocation.authorize(&descriptor);
        let started_ms = self.clock.now_ms();
        if let PolicyDecision::Denied { message, .. } = &decision {
            self.record_denied(
                &descriptor,
                &call,
                &invocation,
                decision.clone(),
                started_ms,
            );
            return self.fail(
                invocation_id,
                action_id,
                ActionErrorCode::PolicyDenied,
                message.clone(),
            );
        }

        // 4. Live availability, then compare-and-set precondition.
        let executor = self
            .executors
            .get(&action_id)
            .expect("builder guarantees an executor per descriptor");
        if let Availability::Disabled { code, message } = executor.availability(&call, &self.state)
        {
            let error = ActionError::unavailable(code, message);
            return ActionResponse::failed(invocation_id, action_id, error)
                .expect("unavailable response is well-formed");
        }
        if let Some(precondition) = &call.precondition
            && let Err(StaleRevision {
                domain,
                expected,
                actual,
            }) = self
                .state
                .revisions
                .check(&precondition.domain, precondition.target_revision)
        {
            return self.fail(
                invocation_id,
                action_id,
                ActionErrorCode::StaleTarget,
                format!(
                    "{} changed from revision {expected} to {actual}",
                    domain_label(&domain)
                ),
            );
        }

        // 5. Audit "started", then run the executor.
        if let Err(error) = self.audit.record(ActionAuditDraft {
            invocation_id,
            program_id: invocation.program_id(),
            model_run_id: invocation.model_run_id(),
            timestamp_ms: started_ms,
            actor: invocation.actor().into(),
            transport: invocation.transport(),
            controller_access: invocation.controller_access(),
            policy_epoch: invocation.policy_epoch(),
            action_id: action_id.clone(),
            target: call.target.clone(),
            arguments: descriptor.audit.redact(&call.arguments),
            effect: descriptor.effect,
            recoverability: descriptor.recoverability,
            decision,
            outcome: AuditOutcome::Started,
            error_code: None,
        }) {
            // Audit backpressure: the ring is full of in-flight records. Fail
            // the call rather than lose the trail.
            return self.fail(
                invocation_id,
                action_id,
                ActionErrorCode::Failed,
                error.to_string(),
            );
        }

        let executor = self
            .executors
            .get_mut(&action_id)
            .expect("builder guarantees an executor per descriptor");
        let outcome = executor.execute(&call, &invocation, &mut self.state);
        let duration_ms = (self.clock.now_ms() - started_ms).max(0) as u64;

        match outcome {
            ExecutorOutcome::Completed { result, bumped } => {
                if descriptor.terminal_host_action {
                    self.state.shutdown_committed = true;
                    let _ = self.audit.finish(
                        invocation_id,
                        duration_ms,
                        AuditOutcome::AcceptedTerminal,
                        None,
                    );
                    return ActionResponse::accepted_terminal(invocation_id, action_id, result);
                }
                let outcome = if descriptor.recoverability == crate::Recoverability::Irreversible
                    && self.cancel_was_requested(invocation_id)
                {
                    AuditOutcome::CompletedAfterCancelRequest
                } else {
                    AuditOutcome::Completed
                };
                let _ = self.audit.finish(invocation_id, duration_ms, outcome, None);
                ActionResponse::completed(
                    invocation_id,
                    action_id,
                    result,
                    Some(
                        bumped.map_or(self.state.revisions.state_revision(), |c| c.state_revision),
                    ),
                )
            }
            ExecutorOutcome::Accepted { result } => {
                let _ = self.audit.accept(invocation_id);
                ActionResponse::accepted(invocation_id, action_id, result)
            }
            ExecutorOutcome::Delegate(concrete) => {
                // Final-action reauthorization: close the generic record and
                // run the concrete call through the whole pipeline again.
                let _ =
                    self.audit
                        .finish(invocation_id, duration_ms, AuditOutcome::Completed, None);
                let derived = invocation.derived(InvocationId::new());
                let mut response = self.dispatch(concrete, derived);
                // Report under the caller's invocation so the SDK correlates
                // the answer with the request it made.
                response.invocation_id = invocation_id;
                response
            }
            ExecutorOutcome::Failed(error) => {
                let outcome = if error.code == ActionErrorCode::Cancelled {
                    AuditOutcome::Cancelled
                } else {
                    AuditOutcome::Failed
                };
                let _ = self
                    .audit
                    .finish(invocation_id, duration_ms, outcome, Some(error.code));
                ActionResponse::failed(invocation_id, action_id, error)
                    .expect("executor errors carry the required metadata")
            }
        }
    }

    /// Completes an operation the executor previously returned as `Accepted`.
    pub fn finish_accepted(
        &mut self,
        invocation_id: InvocationId,
        outcome: AuditOutcome,
        error_code: Option<ActionErrorCode>,
    ) -> Option<ActionAuditRecord> {
        let started = self
            .audit
            .records()
            .rev()
            .find(|record| record.invocation_id == invocation_id)
            .map(|record| record.timestamp_ms)?;
        let duration_ms = (self.clock.now_ms() - started).max(0) as u64;
        self.audit
            .finish(invocation_id, duration_ms, outcome, error_code)
            .ok()
    }

    /// Records a user or policy cancel request against an in-flight invocation.
    pub fn request_cancel(&mut self, invocation_id: InvocationId) -> Option<ActionAuditRecord> {
        let now = self.clock.now_ms();
        self.audit.mark_cancel_requested(invocation_id, now).ok()
    }

    fn cancel_was_requested(&self, invocation_id: InvocationId) -> bool {
        self.audit
            .records()
            .rev()
            .find(|record| record.invocation_id == invocation_id)
            .is_some_and(|record| record.cancel_requested_at_ms.is_some())
    }

    fn record_denied(
        &mut self,
        descriptor: &ActionDescriptor,
        call: &ActionCall,
        invocation: &TrustedInvocation,
        decision: PolicyDecision,
        timestamp_ms: i64,
    ) {
        let _ = self.audit.record(ActionAuditDraft {
            invocation_id: invocation.invocation_id(),
            program_id: invocation.program_id(),
            model_run_id: invocation.model_run_id(),
            timestamp_ms,
            actor: invocation.actor().into(),
            transport: invocation.transport(),
            controller_access: invocation.controller_access(),
            policy_epoch: invocation.policy_epoch(),
            action_id: descriptor.id.clone(),
            target: call.target.clone(),
            arguments: descriptor.audit.redact(&call.arguments),
            effect: descriptor.effect,
            recoverability: descriptor.recoverability,
            decision,
            outcome: AuditOutcome::Denied,
            error_code: Some(ActionErrorCode::PolicyDenied),
        });
    }

    fn fail(
        &self,
        invocation_id: InvocationId,
        action_id: ActionId,
        code: ActionErrorCode,
        message: impl Into<String>,
    ) -> ActionResponse {
        let error = ActionError::new(code, message)
            .expect("non-unavailable error codes need no reason code");
        ActionResponse::failed(invocation_id, action_id, error)
            .expect("failed response is well-formed")
    }
}

fn domain_label(domain: &RevisionDomain) -> String {
    serde_json::to_value(domain)
        .ok()
        .and_then(|v| v.get("kind").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| "revision domain".to_owned())
}

impl ActionResponse {
    /// True when the host admitted the call and the executor ran to a
    /// terminal or accepted state.
    pub fn is_success(&self) -> bool {
        matches!(
            self.status,
            ActionStatus::Accepted | ActionStatus::AcceptedTerminal | ActionStatus::Completed
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ActionEffect, AuditSpec, InvocationPolicy, Recoverability, RegistryBuilder, SCHEMA_VERSION,
    };

    struct FixedClock;
    impl HostClock for FixedClock {
        fn now_ms(&self) -> i64 {
            1_700_000_000_000
        }
    }

    fn descriptor(id: &str, effect: ActionEffect, policy: InvocationPolicy) -> ActionDescriptor {
        ActionDescriptor {
            schema_version: SCHEMA_VERSION,
            id: ActionId::parse(id).unwrap(),
            label: id.to_owned(),
            description: format!("test action {id}"),
            category: "test".into(),
            argument_schema: json!({"type": "object", "additionalProperties": false}),
            result_schema: json!({"type": "object"}),
            contexts: Vec::new(),
            effect,
            invocation_policy: policy,
            recoverability: Recoverability::Reversible,
            audit: AuditSpec::redact_all(),
            default_bindings: Vec::new(),
            precondition_domain: Some(crate::PreconditionDomainSelector::Global),
            bindable: false,
            terminal_host_action: false,
        }
    }

    /// Bumps the global revision and records that it ran.
    struct Counter;
    impl Executor for Counter {
        fn execute(
            &mut self,
            _: &ActionCall,
            _: &TrustedInvocation,
            state: &mut HostState,
        ) -> ExecutorOutcome {
            let bumped = state.bump(RevisionDomain::Global);
            let runs = state.model["runs"].as_u64().unwrap_or(0) + 1;
            state.model["runs"] = json!(runs);
            ExecutorOutcome::Completed {
                result: json!({"runs": runs}),
                bumped: Some(bumped),
            }
        }
    }

    /// `ui.activate_selected`: resolves to whatever is selected.
    struct ActivateSelected;
    impl Executor for ActivateSelected {
        fn execute(
            &mut self,
            _: &ActionCall,
            _: &TrustedInvocation,
            state: &mut HostState,
        ) -> ExecutorOutcome {
            let selected = state.model["selected"]
                .as_str()
                .unwrap_or("chat.toggle_sidebar");
            ExecutorOutcome::Delegate(ActionCall::new(ActionId::parse(selected).unwrap()))
        }
    }

    struct Quit;
    impl Executor for Quit {
        fn execute(
            &mut self,
            _: &ActionCall,
            _: &TrustedInvocation,
            _: &mut HostState,
        ) -> ExecutorOutcome {
            ExecutorOutcome::Completed {
                result: json!({}),
                bumped: None,
            }
        }
    }

    fn host() -> ActionHost {
        let mut registry = RegistryBuilder::new();
        registry.register(descriptor(
            "chat.toggle_sidebar",
            ActionEffect::MutateMaple,
            InvocationPolicy::ControllerCallable,
        ));
        registry.register(descriptor(
            "task.list",
            ActionEffect::Observe,
            InvocationPolicy::ControllerCallable,
        ));
        registry.register(descriptor(
            "account.sign_out",
            ActionEffect::ExternalEffect,
            InvocationPolicy::HumanOnly,
        ));
        registry.register(descriptor(
            "ui.activate_selected",
            ActionEffect::Navigate,
            InvocationPolicy::ControllerCallable,
        ));
        let mut quit = descriptor(
            "app.quit",
            ActionEffect::MutateMaple,
            InvocationPolicy::ControllerCallable,
        );
        quit.terminal_host_action = true;
        registry.register(quit);
        let registry = registry.build().unwrap();
        ActionHostBuilder::new(registry)
            .executor(ActionId::parse("chat.toggle_sidebar").unwrap(), Counter)
            .executor(ActionId::parse("task.list").unwrap(), Counter)
            .executor(ActionId::parse("account.sign_out").unwrap(), Counter)
            .executor(
                ActionId::parse("ui.activate_selected").unwrap(),
                ActivateSelected,
            )
            .executor(ActionId::parse("app.quit").unwrap(), Quit)
            .clock(FixedClock)
            .build()
            .unwrap()
    }

    fn call(id: &str) -> ActionCall {
        ActionCall::new(ActionId::parse(id).unwrap())
    }

    fn model_origin(host: &ActionHost) -> ControllerOrigin {
        ControllerOrigin {
            actor: InvocationActor::Model,
            source_task: None,
            program_id: ProgramId::new(),
            program_started_unix_ms: 1,
            execution_id: ExecutionId::new(),
            model_run_id: None,
            kernel_generation: 1,
            observed_policy_epoch: host.state().policy_epoch(),
            cancellation: CancellationToken::new(),
            budget: ActionBudget::new(16).unwrap(),
        }
    }

    fn full_access() -> ProgrammabilityAuthority {
        ProgrammabilityAuthority::new(
            PythonCodeMode::DeveloperPreview,
            UiControllerAccess::FullAccess,
        )
        .unwrap()
    }

    #[test]
    fn builder_requires_one_executor_per_descriptor() {
        let mut registry = RegistryBuilder::new();
        registry.register(descriptor(
            "chat.toggle_sidebar",
            ActionEffect::MutateMaple,
            InvocationPolicy::ControllerCallable,
        ));
        let errors = ActionHostBuilder::new(registry.build().unwrap())
            .build()
            .err()
            .unwrap();
        assert_eq!(
            errors,
            vec![HostBuildError::MissingExecutor(
                ActionId::parse("chat.toggle_sidebar").unwrap()
            )]
        );
    }

    #[test]
    fn direct_user_bypasses_controller_mode_but_not_availability() {
        let mut host = host();
        assert_eq!(host.state().authority(), ProgrammabilityAuthority::OFF);
        let response = host.invoke_pointer(call("chat.toggle_sidebar"));
        assert_eq!(response.status, ActionStatus::Completed);
        assert_eq!(response.state_revision, Some(1));
        assert_eq!(host.audit().len(), 1);
        assert_eq!(host.audit().recent(1)[0].outcome, AuditOutcome::Completed);
    }

    #[test]
    fn controller_off_denies_model_and_audits_the_denial() {
        let mut host = host();
        let origin = model_origin(&host);
        let response = host.invoke_controller(call("task.list"), &origin);
        assert_eq!(response.status, ActionStatus::Failed);
        assert_eq!(response.error.unwrap().code, ActionErrorCode::PolicyDenied);
        assert_eq!(host.audit().recent(1)[0].outcome, AuditOutcome::Denied);
    }

    #[test]
    fn read_only_allows_observe_and_denies_mutation() {
        let mut host = host();
        host.set_authority(
            ProgrammabilityAuthority::new(
                PythonCodeMode::DeveloperPreview,
                UiControllerAccess::ReadOnly,
            )
            .unwrap(),
        );
        let origin = model_origin(&host);
        assert!(
            host.invoke_controller(call("task.list"), &origin)
                .is_success()
        );
        let denied = host.invoke_controller(call("chat.toggle_sidebar"), &origin);
        assert_eq!(denied.error.unwrap().code, ActionErrorCode::PolicyDenied);
    }

    #[test]
    fn human_only_stays_human_only_through_generic_delegation() {
        let mut host = host();
        host.set_authority(full_access());
        host.state_mut().model["selected"] = json!("account.sign_out");
        let origin = model_origin(&host);
        let response = host.invoke_controller(call("ui.activate_selected"), &origin);
        assert_eq!(
            response.error.as_ref().unwrap().code,
            ActionErrorCode::PolicyDenied
        );
        assert_eq!(response.action_id.as_str(), "account.sign_out");
        // The same gesture from a human succeeds.
        let human = host.invoke_pointer(call("ui.activate_selected"));
        assert_eq!(human.status, ActionStatus::Completed);
    }

    #[test]
    fn policy_change_bumps_epoch_and_stale_leases_fail_closed() {
        let mut host = host();
        host.set_authority(full_access());
        let origin = model_origin(&host);
        assert!(
            host.invoke_controller(call("chat.toggle_sidebar"), &origin)
                .is_success()
        );
        let revocation = host.set_authority(ProgrammabilityAuthority::OFF);
        assert!(
            revocation.stop_kernels
                && revocation.revoke_all_controller_requests
                && revocation.revoke_mutation_leases
        );
        let stale = host.invoke_controller(call("task.list"), &origin);
        assert_eq!(stale.error.unwrap().code, ActionErrorCode::PolicyDenied);
    }

    #[test]
    fn stale_precondition_is_rejected_before_execution() {
        let mut host = host();
        host.invoke_pointer(call("chat.toggle_sidebar"));
        let stale = call("chat.toggle_sidebar").with_precondition(crate::ActionPrecondition {
            domain: RevisionDomain::Global,
            target_revision: 0,
        });
        let response = host.invoke_pointer(stale);
        assert_eq!(response.error.unwrap().code, ActionErrorCode::StaleTarget);
        let fresh = call("chat.toggle_sidebar").with_precondition(crate::ActionPrecondition {
            domain: RevisionDomain::Global,
            target_revision: 1,
        });
        assert!(host.invoke_pointer(fresh).is_success());
    }

    #[test]
    fn unknown_and_invalid_calls_fail_before_authority() {
        let mut host = host();
        let unknown = host.invoke_pointer(call("nope.missing"));
        assert_eq!(unknown.error.unwrap().code, ActionErrorCode::UnknownAction);
        let invalid = host.invoke_pointer(call("task.list").with_arguments(json!({"extra": 1})));
        assert_eq!(
            invalid.error.unwrap().code,
            ActionErrorCode::InvalidArguments
        );
        assert!(
            host.audit().is_empty(),
            "structural failures are not audited actions"
        );
    }

    #[test]
    fn terminal_host_action_stops_admission() {
        let mut host = host();
        let response = host.invoke_pointer(call("app.quit"));
        assert_eq!(response.status, ActionStatus::AcceptedTerminal);
        assert!(host.state().is_shutdown_committed());
        let after = host.invoke_pointer(call("task.list"));
        assert_eq!(after.status, ActionStatus::Failed);
    }

    #[test]
    fn direct_user_budget_bounds_a_single_gesture() {
        let host = host();
        let invocation = host
            .direct_user_invocation(host.direct_user_ingress().pointer(), None)
            .unwrap();
        assert_eq!(
            invocation.action_budget().limit(),
            DIRECT_USER_ACTION_BUDGET
        );
        let foreign = DirectUserIngress::new_window();
        assert_eq!(
            host.direct_user_invocation(foreign.pointer(), None).err(),
            Some(TrustedInvocationError::WrongWindowProvenance)
        );
    }
}
