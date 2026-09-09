//! The controller bridge between Code Mode / model programs and the host.
//!
//! In the prototype this seam was split across `crates/maple-code-mode/src/
//! controller.rs` (the `UiControllerTransport` trait, request disposition and
//! delivery fences) and `app/src/harness/controller.rs` (a bounded GPUI
//! request/response bridge). The Python `maple_gpui` SDK serialised an
//! [`ActionCall`]-shaped request into the worker pipe; the kernel attached
//! provenance; the UI thread claimed the request, ran it through the one
//! [`ActionHost`](crate::ActionHost), and wrote the response frame back.
//!
//! This module is a from-scratch, runtime-free restatement of that bridge.
//! It keeps the three properties that made the prototype safe:
//!
//! - **Bounded admission.** The queue has a fixed capacity and rejects,
//!   never blocks, when the UI has fallen behind.
//! - **One owner per request.** A [`RequestDisposition`] fence decides,
//!   exactly once, whether the transport abandoned a request (timeout, drop,
//!   Stop) or the UI claimed it. A claimed request always gets its exact
//!   response; an abandoned one never enters the host.
//! - **Delivery before destruction.** A [`DeliveryBarrier`] marks when the
//!   response frame reached the worker. Terminal host actions (`app.quit`)
//!   commit shutdown only after the barrier succeeds, so Python observes
//!   `accepted_terminal` instead of a broken pipe.
//!
//! Wire requests never carry authority: the bridge stores the origin the
//! kernel minted and the host reads its *current* policy at dispatch.

use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
};

use thiserror::Error;

use crate::{ActionCall, ActionResponse, ActionStatus, ControllerOrigin, UiControllerAccess};

/// Default bound on requests waiting for the UI thread.
pub const DEFAULT_CONTROLLER_QUEUE_CAPACITY: usize = 64;

const PENDING: u8 = 0;
const UI_CLAIMED: u8 = 1;
const ABANDONED: u8 = 2;

/// One-shot ownership decision for a request at the transport/UI seam.
///
/// Process-local; never serialised into the worker protocol.
#[derive(Clone, Debug, Default)]
pub struct RequestDisposition(Arc<AtomicU8>);

impl RequestDisposition {
    /// Claim UI ownership immediately before dispatch. `false` means the
    /// transport already abandoned the request; skip it without entering the
    /// host.
    pub fn try_claim_ui(&self) -> bool {
        self.0
            .compare_exchange(PENDING, UI_CLAIMED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Abandon only while still queued. `false` means the UI owns it and the
    /// transport must wait for the exact response.
    pub fn abandon_if_pending(&self) -> bool {
        self.0
            .compare_exchange(PENDING, ABANDONED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub fn is_ui_claimed(&self) -> bool {
        self.0.load(Ordering::Acquire) == UI_CLAIMED
    }

    pub fn is_abandoned(&self) -> bool {
        self.0.load(Ordering::Acquire) == ABANDONED
    }
}

const DELIVERY_PENDING: u8 = 0;
const DELIVERY_SUCCEEDED: u8 = 1;
const DELIVERY_FAILED: u8 = 2;

#[derive(Debug, Default)]
struct DeliveryState {
    outcome: AtomicU8,
    terminal_committed: AtomicBool,
}

/// Completion fence for one response frame.
#[derive(Clone, Debug, Default)]
pub struct DeliveryBarrier(Arc<DeliveryState>);

impl DeliveryBarrier {
    /// The transport wrote the frame into the worker pipe.
    pub fn mark_delivered(&self) -> bool {
        self.0
            .outcome
            .compare_exchange(
                DELIVERY_PENDING,
                DELIVERY_SUCCEEDED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// The transport could not write the frame (worker gone, pipe closed).
    pub fn mark_failed(&self) -> bool {
        self.0
            .outcome
            .compare_exchange(
                DELIVERY_PENDING,
                DELIVERY_FAILED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    pub fn is_delivered(&self) -> bool {
        self.0.outcome.load(Ordering::Acquire) == DELIVERY_SUCCEEDED
    }

    pub fn is_settled(&self) -> bool {
        self.0.outcome.load(Ordering::Acquire) != DELIVERY_PENDING
    }

    /// Terminal host actions call this once the accepted-terminal frame is
    /// delivered (or delivery definitively failed) to release shutdown.
    pub fn commit_terminal(&self) -> Result<(), DeliveryError> {
        if !self.is_settled() {
            return Err(DeliveryError::TerminalBeforeDelivery);
        }
        self.0.terminal_committed.store(true, Ordering::Release);
        Ok(())
    }

    pub fn is_terminal_committed(&self) -> bool {
        self.0.terminal_committed.load(Ordering::Acquire)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum DeliveryError {
    #[error(
        "terminal shutdown may only commit after the response frame is delivered or delivery failed"
    )]
    TerminalBeforeDelivery,
}

/// Monotonic per-bridge request identity (distinct from the host's
/// `InvocationId`, which is minted only when the UI claims the request).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RequestId(u64);

/// A request queued for the UI thread.
#[derive(Clone, Debug)]
pub struct QueuedRequest {
    pub id: RequestId,
    pub call: ActionCall,
    pub origin: ControllerOrigin,
    pub disposition: RequestDisposition,
    pub delivery: DeliveryBarrier,
}

/// A request the UI has claimed and must now run through the host.
#[derive(Debug)]
pub struct ClaimedRequest {
    pub id: RequestId,
    pub call: ActionCall,
    pub origin: ControllerOrigin,
    pub delivery: DeliveryBarrier,
}

/// What the transport receives back for one request.
#[derive(Clone, Debug)]
pub struct Outcome {
    pub id: RequestId,
    pub response: ActionResponse,
    pub delivery: DeliveryBarrier,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum SubmitError {
    #[error("Maple UI Controller access is Off")]
    ControllerOff,
    #[error("controller request queue is full ({capacity})")]
    QueueFull { capacity: usize },
    #[error("controller bridge is revoked; no new requests are admitted")]
    Revoked,
}

/// Bounded request/response bridge owned by the host process.
#[derive(Debug)]
pub struct ControllerBridge {
    capacity: usize,
    next_id: u64,
    access: UiControllerAccess,
    revoked: bool,
    queue: VecDeque<QueuedRequest>,
    outcomes: Vec<Outcome>,
}

impl Default for ControllerBridge {
    fn default() -> Self {
        Self::new(DEFAULT_CONTROLLER_QUEUE_CAPACITY)
    }
}

impl ControllerBridge {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            next_id: 1,
            access: UiControllerAccess::Off,
            revoked: false,
            queue: VecDeque::new(),
            outcomes: Vec::new(),
        }
    }

    pub fn access(&self) -> UiControllerAccess {
        self.access
    }

    /// Mirrors a host authority change. Turning the controller Off revokes
    /// every queued request (section 10.4); a later `ReadOnly`/`FullAccess`
    /// re-arms the bridge for new programs.
    pub fn set_access(&mut self, access: UiControllerAccess) -> usize {
        self.access = access;
        if access == UiControllerAccess::Off {
            self.revoked = true;
            self.revoke_queued()
        } else {
            self.revoked = false;
            0
        }
    }

    /// Stop: abandon everything still queued. Claimed requests are not
    /// touched; their exact response still flows back.
    pub fn revoke_queued(&mut self) -> usize {
        let mut revoked = 0;
        for request in &self.queue {
            if request.disposition.abandon_if_pending() {
                revoked += 1;
            }
        }
        self.queue.clear();
        revoked
    }

    /// Transport side: enqueue a request minted by the kernel.
    pub fn submit(
        &mut self,
        call: ActionCall,
        origin: ControllerOrigin,
    ) -> Result<QueuedRequest, SubmitError> {
        if self.revoked {
            return Err(SubmitError::Revoked);
        }
        if self.access == UiControllerAccess::Off {
            return Err(SubmitError::ControllerOff);
        }
        if self.queue.len() >= self.capacity {
            return Err(SubmitError::QueueFull {
                capacity: self.capacity,
            });
        }
        let request = QueuedRequest {
            id: RequestId(self.next_id),
            call,
            origin,
            disposition: RequestDisposition::default(),
            delivery: DeliveryBarrier::default(),
        };
        self.next_id += 1;
        self.queue.push_back(request.clone());
        Ok(request)
    }

    /// UI side: claim the next live request. Abandoned requests are dropped
    /// silently; they never reach the host or the audit ring.
    pub fn claim_next(&mut self) -> Option<ClaimedRequest> {
        while let Some(request) = self.queue.pop_front() {
            if request.origin.cancellation.is_cancelled() {
                request.disposition.abandon_if_pending();
                continue;
            }
            if request.disposition.try_claim_ui() {
                return Some(ClaimedRequest {
                    id: request.id,
                    call: request.call,
                    origin: request.origin,
                    delivery: request.delivery,
                });
            }
        }
        None
    }

    /// UI side: publish the host's response for a claimed request.
    pub fn complete(&mut self, claimed: ClaimedRequest, response: ActionResponse) -> Outcome {
        let outcome = Outcome {
            id: claimed.id,
            response,
            delivery: claimed.delivery,
        };
        self.outcomes.push(outcome.clone());
        outcome
    }

    /// Transport side: drain responses ready to be framed to the worker.
    pub fn take_outcomes(&mut self) -> Vec<Outcome> {
        std::mem::take(&mut self.outcomes)
    }

    pub fn queued_len(&self) -> usize {
        self.queue.len()
    }
}

/// True when a response is the accepted-terminal frame that must be delivered
/// before the host may shut down.
pub fn is_terminal_frame(response: &ActionResponse) -> bool {
    response.status == ActionStatus::AcceptedTerminal
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ActionBudget, ActionId, ExecutionId, InvocationActor, InvocationId, ProgramId};
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    fn origin(cancellation: CancellationToken) -> ControllerOrigin {
        ControllerOrigin {
            actor: InvocationActor::Model,
            source_task: None,
            program_id: ProgramId::new(),
            program_started_unix_ms: 0,
            execution_id: ExecutionId::new(),
            model_run_id: None,
            kernel_generation: 1,
            observed_policy_epoch: 0,
            cancellation,
            budget: ActionBudget::new(8).unwrap(),
        }
    }

    fn call() -> ActionCall {
        ActionCall::new(ActionId::parse("task.list").unwrap())
    }

    fn bridge(capacity: usize) -> ControllerBridge {
        let mut bridge = ControllerBridge::new(capacity);
        bridge.set_access(UiControllerAccess::ReadOnly);
        bridge
    }

    #[test]
    fn off_and_full_reject_without_blocking() {
        let mut bridge = ControllerBridge::new(1);
        assert_eq!(
            bridge
                .submit(call(), origin(CancellationToken::new()))
                .err(),
            Some(SubmitError::ControllerOff)
        );
        bridge.set_access(UiControllerAccess::FullAccess);
        bridge
            .submit(call(), origin(CancellationToken::new()))
            .unwrap();
        assert_eq!(
            bridge
                .submit(call(), origin(CancellationToken::new()))
                .err(),
            Some(SubmitError::QueueFull { capacity: 1 })
        );
    }

    #[test]
    fn abandoned_requests_never_reach_the_ui() {
        let mut bridge = bridge(4);
        let queued = bridge
            .submit(call(), origin(CancellationToken::new()))
            .unwrap();
        assert!(queued.disposition.abandon_if_pending());
        assert!(bridge.claim_next().is_none());
        assert!(!queued.disposition.try_claim_ui());
    }

    #[test]
    fn claim_wins_the_race_and_gets_its_exact_response() {
        let mut bridge = bridge(4);
        let queued = bridge
            .submit(call(), origin(CancellationToken::new()))
            .unwrap();
        let claimed = bridge.claim_next().unwrap();
        assert!(
            !queued.disposition.abandon_if_pending(),
            "transport must now await the response"
        );
        let response = ActionResponse::completed(
            InvocationId::new(),
            claimed.call.action_id.clone(),
            json!({}),
            Some(1),
        );
        let outcome = bridge.complete(claimed, response.clone());
        assert_eq!(outcome.id, queued.id);
        assert_eq!(bridge.take_outcomes().len(), 1);
        assert_eq!(outcome.response, response);
    }

    #[test]
    fn cancelled_programs_are_skipped_at_claim_time() {
        let mut bridge = bridge(4);
        let token = CancellationToken::new();
        let queued = bridge.submit(call(), origin(token.clone())).unwrap();
        token.cancel();
        assert!(bridge.claim_next().is_none());
        assert!(queued.disposition.is_abandoned());
    }

    #[test]
    fn turning_the_controller_off_revokes_the_queue() {
        let mut bridge = bridge(4);
        bridge
            .submit(call(), origin(CancellationToken::new()))
            .unwrap();
        bridge
            .submit(call(), origin(CancellationToken::new()))
            .unwrap();
        assert_eq!(bridge.set_access(UiControllerAccess::Off), 2);
        assert_eq!(bridge.queued_len(), 0);
        assert_eq!(
            bridge
                .submit(call(), origin(CancellationToken::new()))
                .err(),
            Some(SubmitError::Revoked)
        );
        bridge.set_access(UiControllerAccess::ReadOnly);
        assert!(
            bridge
                .submit(call(), origin(CancellationToken::new()))
                .is_ok()
        );
    }

    #[test]
    fn terminal_shutdown_waits_for_delivery() {
        let barrier = DeliveryBarrier::default();
        assert_eq!(
            barrier.commit_terminal(),
            Err(DeliveryError::TerminalBeforeDelivery)
        );
        assert!(barrier.mark_delivered());
        assert!(!barrier.mark_failed(), "delivery settles exactly once");
        barrier.commit_terminal().unwrap();
        assert!(barrier.is_terminal_committed());
        let terminal = ActionResponse::accepted_terminal(
            InvocationId::new(),
            ActionId::parse("app.quit").unwrap(),
            json!({}),
        );
        assert!(is_terminal_frame(&terminal));
    }
}
