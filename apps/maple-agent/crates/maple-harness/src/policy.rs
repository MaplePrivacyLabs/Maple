use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{
    ActionDescriptor, ActionEffect, ExecutionId, InvocationId, ProgramId, RunId, TaskIdentity,
};

#[derive(Clone, Copy, Debug, Eq, Hash, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvocationPolicy {
    ControllerCallable,
    HumanOnly,
}

#[derive(Clone, Copy, Debug, Eq, Hash, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvocationActor {
    DirectUser,
    Model,
    UserCode,
    Internal,
}

impl InvocationActor {
    pub const ALL: [Self; 4] = [
        Self::DirectUser,
        Self::Model,
        Self::UserCode,
        Self::Internal,
    ];
}

#[derive(Clone, Copy, Debug, Eq, Hash, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvocationTransport {
    Pointer,
    Keybinding,
    CommandPalette,
    Python,
    Macro,
    GeneratedUi,
    Internal,
}

impl InvocationTransport {
    pub const ALL: [Self; 7] = [
        Self::Pointer,
        Self::Keybinding,
        Self::CommandPalette,
        Self::Python,
        Self::Macro,
        Self::GeneratedUi,
        Self::Internal,
    ];
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiControllerAccess {
    #[default]
    Off,
    ReadOnly,
    FullAccess,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PythonCodeMode {
    #[default]
    Off,
    DeveloperPreview,
}

/// Normalized, session-only runtime authority.
#[derive(Clone, Copy, Debug, Default, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProgrammabilityAuthority {
    pub code_mode: PythonCodeMode,
    pub controller: UiControllerAccess,
}

impl<'de> Deserialize<'de> for ProgrammabilityAuthority {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireAuthority {
            code_mode: PythonCodeMode,
            controller: UiControllerAccess,
        }

        let wire = WireAuthority::deserialize(deserializer)?;
        Self::new(wire.code_mode, wire.controller).map_err(serde::de::Error::custom)
    }
}

impl ProgrammabilityAuthority {
    pub const OFF: Self = Self {
        code_mode: PythonCodeMode::Off,
        controller: UiControllerAccess::Off,
    };

    pub fn new(
        code_mode: PythonCodeMode,
        controller: UiControllerAccess,
    ) -> Result<Self, AuthorityTransitionError> {
        let authority = Self {
            code_mode,
            controller,
        };
        authority.validate()?;
        Ok(authority)
    }

    pub fn validate(self) -> Result<(), AuthorityTransitionError> {
        if self.code_mode == PythonCodeMode::Off && self.controller != UiControllerAccess::Off {
            return Err(AuthorityTransitionError::ControllerRequiresCodeMode);
        }
        Ok(())
    }

    /// Changes Code Mode and atomically revokes controller access when turning it off.
    pub fn with_code_mode(mut self, code_mode: PythonCodeMode) -> Self {
        self.code_mode = code_mode;
        if code_mode == PythonCodeMode::Off {
            self.controller = UiControllerAccess::Off;
        }
        self
    }

    pub fn with_controller(
        mut self,
        controller: UiControllerAccess,
    ) -> Result<Self, AuthorityTransitionError> {
        if self.code_mode == PythonCodeMode::Off && controller != UiControllerAccess::Off {
            return Err(AuthorityTransitionError::ControllerRequiresCodeMode);
        }
        self.controller = controller;
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum AuthorityTransitionError {
    #[error("Read Only or Full Access controller authority requires Developer Preview Code Mode")]
    ControllerRequiresCodeMode,
}

#[derive(Clone, Copy, Debug, Eq, Hash, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDenialCode {
    HumanOnly,
    ControllerOff,
    ReadOnlyMutation,
    InvalidDirectUserOrigin,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum PolicyDecision {
    Allowed,
    Denied {
        code: PolicyDenialCode,
        message: String,
    },
}

impl PolicyDecision {
    pub const fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed)
    }

    pub fn denial_code(&self) -> Option<PolicyDenialCode> {
        match self {
            Self::Allowed => None,
            Self::Denied { code, .. } => Some(*code),
        }
    }
}

/// Applies the complete actor/transport/controller matrix.
///
/// Only the three physical/direct-user tuples bypass controller authority.
/// `Internal` has no implicit privilege; it is evaluated exactly like any
/// other non-direct origin.
pub fn authorize(
    invocation_policy: InvocationPolicy,
    effect: ActionEffect,
    actor: InvocationActor,
    transport: InvocationTransport,
    controller_access: UiControllerAccess,
) -> PolicyDecision {
    let is_direct_user_origin = is_allowed_direct_user_origin(actor, transport);

    if actor == InvocationActor::DirectUser && !is_direct_user_origin {
        return PolicyDecision::Denied {
            code: PolicyDenialCode::InvalidDirectUserOrigin,
            message: "Direct-user authority requires a trusted pointer, physical keybinding, or command-palette activation".into(),
        };
    }

    if invocation_policy == InvocationPolicy::HumanOnly {
        return if is_direct_user_origin {
            PolicyDecision::Allowed
        } else {
            PolicyDecision::Denied {
                code: PolicyDenialCode::HumanOnly,
                message: "This action requires direct human activation".into(),
            }
        };
    }

    if is_direct_user_origin {
        return PolicyDecision::Allowed;
    }

    match controller_access {
        UiControllerAccess::Off => PolicyDecision::Denied {
            code: PolicyDenialCode::ControllerOff,
            message: "Maple UI Controller access is Off".into(),
        },
        UiControllerAccess::ReadOnly => match effect {
            ActionEffect::Observe | ActionEffect::Navigate => PolicyDecision::Allowed,
            ActionEffect::MutateMaple | ActionEffect::ExternalEffect => PolicyDecision::Denied {
                code: PolicyDenialCode::ReadOnlyMutation,
                message:
                    "Read Only controller access cannot mutate Maple or cause external effects"
                        .into(),
            },
        },
        UiControllerAccess::FullAccess => PolicyDecision::Allowed,
    }
}

pub fn is_allowed_direct_user_origin(
    actor: InvocationActor,
    transport: InvocationTransport,
) -> bool {
    actor == InvocationActor::DirectUser
        && matches!(
            transport,
            InvocationTransport::Pointer
                | InvocationTransport::Keybinding
                | InvocationTransport::CommandPalette
        )
}

/// A shared, bounded admission counter for one program and all derived calls.
#[derive(Clone, Debug)]
pub struct ActionBudget {
    inner: Arc<ActionBudgetInner>,
}

#[derive(Debug)]
struct ActionBudgetInner {
    limit: u32,
    consumed: AtomicU32,
}

impl ActionBudget {
    pub fn new(limit: u32) -> Result<Self, ActionBudgetError> {
        if limit == 0 {
            return Err(ActionBudgetError::ZeroLimit);
        }
        Ok(Self {
            inner: Arc::new(ActionBudgetInner {
                limit,
                consumed: AtomicU32::new(0),
            }),
        })
    }

    pub fn limit(&self) -> u32 {
        self.inner.limit
    }

    pub fn consumed(&self) -> u32 {
        self.inner.consumed.load(Ordering::Acquire)
    }

    pub fn remaining(&self) -> u32 {
        self.limit().saturating_sub(self.consumed())
    }

    pub fn try_consume(&self) -> Result<u32, ActionBudgetError> {
        let mut consumed = self.inner.consumed.load(Ordering::Acquire);
        loop {
            if consumed >= self.inner.limit {
                return Err(ActionBudgetError::Exhausted {
                    limit: self.inner.limit,
                });
            }
            match self.inner.consumed.compare_exchange_weak(
                consumed,
                consumed + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(consumed + 1),
                Err(current) => consumed = current,
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ActionBudgetError {
    #[error("action budget must be greater than zero")]
    ZeroLimit,
    #[error("program action budget of {limit} calls is exhausted")]
    Exhausted { limit: u32 },
}

/// Window-scoped issuer held by the trusted UI host. Tokens minted by this
/// value are opaque, non-serializable, and move-only.
#[derive(Clone, Debug)]
pub struct DirectUserIngress {
    issuer_id: DirectUserIssuerId,
}

/// Opaque identity for the one direct-user issuer registered with an action
/// host. This value is deliberately non-serializable: it is host wiring, not a
/// capability a controller request may supply.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DirectUserIssuerId(uuid::Uuid);

impl DirectUserIngress {
    pub fn new_window() -> Self {
        Self {
            issuer_id: DirectUserIssuerId(uuid::Uuid::new_v4()),
        }
    }

    /// Returns the opaque identity that the owning UI host must register with
    /// its action dispatcher. Possessing an independently-created issuer with a
    /// different identity never grants direct-user authority.
    pub fn issuer_id(&self) -> DirectUserIssuerId {
        self.issuer_id
    }

    /// Mint only while handling the corresponding physical pointer callback.
    pub fn pointer(&self) -> DirectUserProvenance {
        DirectUserProvenance {
            issuer_id: self.issuer_id,
            transport: InvocationTransport::Pointer,
        }
    }

    /// Mint only for an explicit command-palette activation initiated by a
    /// direct-user pointer/key event.
    pub fn command_palette(&self) -> DirectUserProvenance {
        DirectUserProvenance {
            issuer_id: self.issuer_id,
            transport: InvocationTransport::CommandPalette,
        }
    }

    /// Starts provenance for a physical multi-stroke key sequence. The token
    /// remains valid across GPUI's shorter-prefix timeout only while all three
    /// host generations remain unchanged.
    pub fn begin_key_sequence(
        &self,
        focus_generation: u64,
        context_generation: u64,
        keymap_generation: u64,
    ) -> PendingKeyProvenance {
        PendingKeyProvenance {
            issuer_id: self.issuer_id,
            focus_generation,
            context_generation,
            keymap_generation,
            active: true,
        }
    }

    /// Constructs a DirectUser invocation and rejects provenance minted for a
    /// different window-scoped ingress. Keep this ingress object private to
    /// the UI host; do not expose it to model, Python, macro, generated UI, or
    /// generic internal dispatch code.
    #[allow(clippy::too_many_arguments)]
    pub fn trusted_invocation(
        &self,
        invocation_id: InvocationId,
        provenance: DirectUserProvenance,
        source_task: Option<TaskIdentity>,
        controller_access: UiControllerAccess,
        policy_epoch: u64,
        cancellation: CancellationToken,
        action_budget: ActionBudget,
    ) -> Result<TrustedInvocation, TrustedInvocationError> {
        if provenance.issuer_id != self.issuer_id {
            return Err(TrustedInvocationError::WrongWindowProvenance);
        }
        Ok(TrustedInvocation::new_direct_user(
            invocation_id,
            provenance,
            source_task,
            controller_access,
            policy_epoch,
            cancellation,
            action_budget,
        ))
    }
}

impl Default for DirectUserIngress {
    fn default() -> Self {
        Self::new_window()
    }
}

#[derive(Debug)]
pub struct DirectUserProvenance {
    issuer_id: DirectUserIssuerId,
    transport: InvocationTransport,
}

/// Physical-key provenance retained while GPUI resolves a pending sequence.
/// It is intentionally not cloneable or serializable.
#[derive(Debug)]
pub struct PendingKeyProvenance {
    issuer_id: DirectUserIssuerId,
    focus_generation: u64,
    context_generation: u64,
    keymap_generation: u64,
    active: bool,
}

impl PendingKeyProvenance {
    pub fn invalidate(&mut self) {
        self.active = false;
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Consumes a successfully resolved physical keybinding. Focus/context
    /// changes, profile reloads, recorder takeover, cancellation, or an
    /// unresolved timeout must invalidate or fail this token.
    pub fn consume_resolved(
        self,
        focus_generation: u64,
        context_generation: u64,
        keymap_generation: u64,
    ) -> Result<DirectUserProvenance, PendingKeyProvenanceError> {
        if !self.active {
            return Err(PendingKeyProvenanceError::Invalidated);
        }
        if self.focus_generation != focus_generation {
            return Err(PendingKeyProvenanceError::FocusChanged);
        }
        if self.context_generation != context_generation {
            return Err(PendingKeyProvenanceError::ContextChanged);
        }
        if self.keymap_generation != keymap_generation {
            return Err(PendingKeyProvenanceError::KeymapChanged);
        }
        Ok(DirectUserProvenance {
            issuer_id: self.issuer_id,
            transport: InvocationTransport::Keybinding,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum PendingKeyProvenanceError {
    #[error("pending key provenance was invalidated")]
    Invalidated,
    #[error("focus changed during pending key resolution")]
    FocusChanged,
    #[error("key context changed during pending key resolution")]
    ContextChanged,
    #[error("keymap generation changed during pending key resolution")]
    KeymapChanged,
}

/// Host-assigned execution provenance.
///
/// This type deliberately implements neither [`Serialize`] nor [`Deserialize`].
/// Controller wire requests contain only `ActionCall`; a trusted host creates
/// this value after attaching the current policy, identity, budget, and
/// cancellation state.
///
/// ```compile_fail
/// use maple_harness::TrustedInvocation;
/// let _: TrustedInvocation = serde_json::from_str("{}").unwrap();
/// ```
#[derive(Clone, Debug)]
pub struct TrustedInvocation {
    invocation_id: InvocationId,
    source_task: Option<TaskIdentity>,
    program_id: Option<ProgramId>,
    program_started_unix_ms: Option<u64>,
    model_run_id: Option<RunId>,
    execution_id: Option<ExecutionId>,
    kernel_generation: Option<u64>,
    actor: InvocationActor,
    transport: InvocationTransport,
    direct_user_issuer_id: Option<DirectUserIssuerId>,
    controller_access: UiControllerAccess,
    policy_epoch: u64,
    cancellation: CancellationToken,
    action_budget: ActionBudget,
}

impl TrustedInvocation {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        invocation_id: InvocationId,
        source_task: Option<TaskIdentity>,
        program_id: Option<ProgramId>,
        model_run_id: Option<RunId>,
        execution_id: Option<ExecutionId>,
        kernel_generation: Option<u64>,
        actor: InvocationActor,
        transport: InvocationTransport,
        controller_access: UiControllerAccess,
        policy_epoch: u64,
        cancellation: CancellationToken,
        action_budget: ActionBudget,
    ) -> Result<Self, TrustedInvocationError> {
        if actor == InvocationActor::DirectUser {
            return Err(TrustedInvocationError::DirectUserRequiresProvenance);
        }
        Ok(Self {
            invocation_id,
            source_task,
            program_id,
            program_started_unix_ms: None,
            model_run_id,
            execution_id,
            kernel_generation,
            actor,
            transport,
            direct_user_issuer_id: None,
            controller_access,
            policy_epoch,
            cancellation,
            action_budget,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn new_direct_user(
        invocation_id: InvocationId,
        provenance: DirectUserProvenance,
        source_task: Option<TaskIdentity>,
        controller_access: UiControllerAccess,
        policy_epoch: u64,
        cancellation: CancellationToken,
        action_budget: ActionBudget,
    ) -> Self {
        Self {
            invocation_id,
            source_task,
            program_id: None,
            program_started_unix_ms: None,
            model_run_id: None,
            execution_id: None,
            kernel_generation: None,
            actor: InvocationActor::DirectUser,
            transport: provenance.transport,
            direct_user_issuer_id: Some(provenance.issuer_id),
            controller_access,
            policy_epoch,
            cancellation,
            action_budget,
        }
    }

    pub fn invocation_id(&self) -> InvocationId {
        self.invocation_id
    }

    pub fn source_task(&self) -> Option<&TaskIdentity> {
        self.source_task.as_ref()
    }

    pub fn program_id(&self) -> Option<ProgramId> {
        self.program_id
    }

    /// Attach the immutable root-program start minted by the Code Mode
    /// kernel. This consumes the invocation so callers cannot mutate an
    /// already admitted provenance value in place.
    pub fn with_program_started_unix_ms(mut self, started_unix_ms: u64) -> Self {
        self.program_started_unix_ms = Some(started_unix_ms);
        self
    }

    pub fn program_started_unix_ms(&self) -> Option<u64> {
        self.program_started_unix_ms
    }

    pub fn model_run_id(&self) -> Option<RunId> {
        self.model_run_id.clone()
    }

    pub fn execution_id(&self) -> Option<ExecutionId> {
        self.execution_id
    }

    pub fn kernel_generation(&self) -> Option<u64> {
        self.kernel_generation
    }

    pub fn actor(&self) -> InvocationActor {
        self.actor
    }

    pub fn transport(&self) -> InvocationTransport {
        self.transport
    }

    /// Returns the issuer identity retained from opaque physical-input
    /// provenance. Non-direct invocations never have an issuer identity.
    pub fn direct_user_issuer_id(&self) -> Option<DirectUserIssuerId> {
        self.direct_user_issuer_id
    }

    pub fn controller_access(&self) -> UiControllerAccess {
        self.controller_access
    }

    pub fn policy_epoch(&self) -> u64 {
        self.policy_epoch
    }

    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    pub fn action_budget(&self) -> &ActionBudget {
        &self.action_budget
    }

    /// Evaluates this host-attached provenance against an action descriptor.
    /// Wire callers cannot choose or replace any of the authority fields used
    /// by this check.
    pub fn authorize(&self, descriptor: &ActionDescriptor) -> PolicyDecision {
        authorize(
            descriptor.invocation_policy,
            descriptor.effect,
            self.actor,
            self.transport,
            self.controller_access,
        )
    }

    /// Creates a follow-up invocation while preserving all authority and lease
    /// state. Only the per-action invocation ID changes.
    pub fn derived(&self, invocation_id: InvocationId) -> Self {
        let mut derived = self.clone();
        derived.invocation_id = invocation_id;
        derived
    }

    /// Checks cancellation and policy lease freshness without consuming budget.
    pub fn check_active(&self, current_policy_epoch: u64) -> Result<(), InvocationGuardError> {
        if self.cancellation.is_cancelled() {
            return Err(InvocationGuardError::Cancelled);
        }
        if self.policy_epoch != current_policy_epoch {
            return Err(InvocationGuardError::PolicyEpochChanged {
                expected: self.policy_epoch,
                current: current_policy_epoch,
            });
        }
        Ok(())
    }

    /// Final admission check for one action in a program or chain.
    pub fn admit_call(&self, current_policy_epoch: u64) -> Result<u32, InvocationGuardError> {
        self.check_active(current_policy_epoch)?;
        self.action_budget
            .try_consume()
            .map_err(InvocationGuardError::Budget)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum TrustedInvocationError {
    #[error("DirectUser invocations require opaque physical-input provenance")]
    DirectUserRequiresProvenance,
    #[error("direct-user provenance was minted for a different window")]
    WrongWindowProvenance,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum InvocationGuardError {
    #[error("program was cancelled")]
    Cancelled,
    #[error("controller policy epoch changed from {expected} to {current}")]
    PolicyEpochChanged { expected: u64, current: u64 },
    #[error(transparent)]
    Budget(#[from] ActionBudgetError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_and_full_access_follow_the_effect_matrix() {
        for actor in [InvocationActor::Model, InvocationActor::UserCode] {
            for transport in [
                InvocationTransport::Python,
                InvocationTransport::GeneratedUi,
            ] {
                for effect in [ActionEffect::Observe, ActionEffect::Navigate] {
                    assert!(
                        authorize(
                            InvocationPolicy::ControllerCallable,
                            effect,
                            actor,
                            transport,
                            UiControllerAccess::ReadOnly,
                        )
                        .is_allowed()
                    );
                }
                for effect in [ActionEffect::MutateMaple, ActionEffect::ExternalEffect] {
                    assert_eq!(
                        authorize(
                            InvocationPolicy::ControllerCallable,
                            effect,
                            actor,
                            transport,
                            UiControllerAccess::ReadOnly,
                        )
                        .denial_code(),
                        Some(PolicyDenialCode::ReadOnlyMutation)
                    );
                    assert!(
                        authorize(
                            InvocationPolicy::ControllerCallable,
                            effect,
                            actor,
                            transport,
                            UiControllerAccess::FullAccess,
                        )
                        .is_allowed()
                    );
                }
            }
        }
    }

    #[test]
    fn off_denies_every_controller_effect() {
        for actor in [
            InvocationActor::Model,
            InvocationActor::UserCode,
            InvocationActor::Internal,
        ] {
            for transport in InvocationTransport::ALL {
                for effect in [
                    ActionEffect::Observe,
                    ActionEffect::Navigate,
                    ActionEffect::MutateMaple,
                    ActionEffect::ExternalEffect,
                ] {
                    assert_eq!(
                        authorize(
                            InvocationPolicy::ControllerCallable,
                            effect,
                            actor,
                            transport,
                            UiControllerAccess::Off,
                        )
                        .denial_code(),
                        Some(PolicyDenialCode::ControllerOff),
                        "unexpected result for {actor:?}/{transport:?}/{effect:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn human_only_matrix_is_exhaustive() {
        for actor in InvocationActor::ALL {
            for transport in InvocationTransport::ALL {
                let decision = authorize(
                    InvocationPolicy::HumanOnly,
                    ActionEffect::Observe,
                    actor,
                    transport,
                    UiControllerAccess::FullAccess,
                );
                assert_eq!(
                    decision.is_allowed(),
                    is_allowed_direct_user_origin(actor, transport),
                    "unexpected Human Only result for {actor:?}/{transport:?}"
                );
            }
        }
    }

    #[test]
    fn internal_follow_up_inherits_provenance_policy_and_shared_budget() {
        let cancellation = CancellationToken::new();
        let invocation = TrustedInvocation::new(
            InvocationId::new(),
            Some(TaskIdentity::new("account", "task")),
            Some(ProgramId::new()),
            Some(RunId::new()),
            Some(ExecutionId::new()),
            Some(3),
            InvocationActor::Model,
            InvocationTransport::Python,
            UiControllerAccess::ReadOnly,
            7,
            cancellation,
            ActionBudget::new(2).unwrap(),
        )
        .unwrap()
        .with_program_started_unix_ms(1_000);
        let follow_up = invocation.derived(InvocationId::new());

        assert_eq!(follow_up.actor(), InvocationActor::Model);
        assert_eq!(follow_up.transport(), InvocationTransport::Python);
        assert_eq!(follow_up.direct_user_issuer_id(), None);
        assert_eq!(follow_up.controller_access(), UiControllerAccess::ReadOnly);
        assert_eq!(follow_up.policy_epoch(), 7);
        assert_eq!(follow_up.program_id(), invocation.program_id());
        assert_eq!(follow_up.program_started_unix_ms(), Some(1_000));
        assert_eq!(follow_up.admit_call(7).unwrap(), 1);
        assert_eq!(invocation.admit_call(7).unwrap(), 2);
        assert!(matches!(
            follow_up.admit_call(7),
            Err(InvocationGuardError::Budget(ActionBudgetError::Exhausted {
                limit: 2
            }))
        ));
    }

    #[test]
    fn cancellation_and_policy_revocation_prevent_subsequent_calls() {
        let cancellation = CancellationToken::new();
        let invocation = TrustedInvocation::new(
            InvocationId::new(),
            None,
            Some(ProgramId::new()),
            None,
            Some(ExecutionId::new()),
            None,
            InvocationActor::UserCode,
            InvocationTransport::Python,
            UiControllerAccess::FullAccess,
            4,
            cancellation.clone(),
            ActionBudget::new(5).unwrap(),
        )
        .unwrap();

        assert!(invocation.admit_call(5).is_err());
        assert_eq!(invocation.action_budget().consumed(), 0);
        assert!(invocation.admit_call(4).is_ok());
        cancellation.cancel();
        assert_eq!(
            invocation.admit_call(4),
            Err(InvocationGuardError::Cancelled)
        );
        assert_eq!(invocation.action_budget().consumed(), 1);
    }

    #[test]
    fn direct_user_requires_move_only_physical_input_provenance() {
        assert!(matches!(
            TrustedInvocation::new(
                InvocationId::new(),
                None,
                None,
                None,
                None,
                None,
                InvocationActor::DirectUser,
                InvocationTransport::Pointer,
                UiControllerAccess::Off,
                1,
                CancellationToken::new(),
                ActionBudget::new(1).unwrap(),
            ),
            Err(TrustedInvocationError::DirectUserRequiresProvenance)
        ));

        let ingress = DirectUserIngress::new_window();
        let invocation = ingress
            .trusted_invocation(
                InvocationId::new(),
                ingress.pointer(),
                None,
                UiControllerAccess::Off,
                1,
                CancellationToken::new(),
                ActionBudget::new(1).unwrap(),
            )
            .unwrap();
        assert_eq!(invocation.actor(), InvocationActor::DirectUser);
        assert_eq!(invocation.transport(), InvocationTransport::Pointer);
        assert_eq!(
            invocation.direct_user_issuer_id(),
            Some(ingress.issuer_id())
        );
    }

    #[test]
    fn pending_key_provenance_survives_resolved_timeout_but_not_host_state_changes() {
        let ingress = DirectUserIngress::new_window();
        let resolved = ingress
            .begin_key_sequence(10, 20, 30)
            .consume_resolved(10, 20, 30)
            .unwrap();
        let invocation = ingress
            .trusted_invocation(
                InvocationId::new(),
                resolved,
                None,
                UiControllerAccess::Off,
                1,
                CancellationToken::new(),
                ActionBudget::new(1).unwrap(),
            )
            .unwrap();
        assert_eq!(invocation.transport(), InvocationTransport::Keybinding);

        assert!(matches!(
            ingress
                .begin_key_sequence(10, 20, 30)
                .consume_resolved(11, 20, 30),
            Err(PendingKeyProvenanceError::FocusChanged)
        ));
        let mut cancelled = ingress.begin_key_sequence(10, 20, 30);
        cancelled.invalidate();
        assert!(matches!(
            cancelled.consume_resolved(10, 20, 30),
            Err(PendingKeyProvenanceError::Invalidated)
        ));
    }

    #[test]
    fn direct_user_provenance_is_window_scoped() {
        let first = DirectUserIngress::new_window();
        let second = DirectUserIngress::new_window();
        assert_ne!(first.issuer_id(), second.issuer_id());
        assert!(matches!(
            second.trusted_invocation(
                InvocationId::new(),
                first.pointer(),
                None,
                UiControllerAccess::Off,
                1,
                CancellationToken::new(),
                ActionBudget::new(1).unwrap(),
            ),
            Err(TrustedInvocationError::WrongWindowProvenance)
        ));
    }

    #[test]
    fn authority_tuple_is_normalized_and_fail_closed() {
        assert!(
            ProgrammabilityAuthority::new(PythonCodeMode::Off, UiControllerAccess::ReadOnly)
                .is_err()
        );
        let full = ProgrammabilityAuthority::new(
            PythonCodeMode::DeveloperPreview,
            UiControllerAccess::FullAccess,
        )
        .unwrap();
        assert_eq!(
            full.with_code_mode(PythonCodeMode::Off),
            ProgrammabilityAuthority::OFF
        );
        assert!(
            serde_json::from_value::<ProgrammabilityAuthority>(serde_json::json!({
                "code_mode": "off",
                "controller": "full_access"
            }))
            .is_err()
        );
    }
}
