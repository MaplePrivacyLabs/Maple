//! Per-session delivery of shared host events. The account broadcast receiver
//! never waits for history replay or the caller's outbound socket credits.

use super::*;
use crate::agent::AgentServiceEvent;
use serde::Serialize;
use std::io::Write;
use std::sync::atomic::AtomicU8;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

pub(super) const MAX_ACP_SESSION_OPERATIONS: usize = 32;
pub(super) const MAX_OBSERVER_EVENTS: usize = 1024;
const MAX_OBSERVER_BYTES: usize = 4 * 1024 * 1024;
const OBSERVER_EVENT_OVERHEAD: usize = 256;
const FAILURE_NOTICE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);

#[derive(Clone, Copy)]
#[repr(u8)]
pub(super) enum ObserverFailure {
    Backlog = 1,
    HistoryChanged = 2,
    UpdateTooLarge = 3,
    BroadcastLagged = 4,
    ProjectionLimit = 5,
}

impl ObserverFailure {
    fn message(self) -> &'static str {
        match self {
            Self::Backlog => {
                "Shared updates exceeded this ACP session's backlog limit. Reload this session; its task continues."
            }
            Self::HistoryChanged => {
                "The task's history was replaced while ACP was attached. Reload this session; its task continues."
            }
            Self::UpdateTooLarge => {
                "A shared update exceeded this ACP session's transport limit. Reload this session; its task continues."
            }
            Self::BroadcastLagged => {
                "Shared updates fell behind before they could be routed. Reload this session; its task continues."
            }
            Self::ProjectionLimit => convert::ACP_PROJECTION_LIMIT_MESSAGE,
        }
    }
}

pub(super) struct ObservedEvent {
    event: AgentServiceEvent,
    // Evaluate origin ownership before queueing, as the original observer did.
    // A later prompt admission must not reinterpret an earlier shared event.
    suppress_run: bool,
}

struct QueuedEvent {
    observed: ObservedEvent,
    _event: OwnedSemaphorePermit,
    _bytes: OwnedSemaphorePermit,
}

pub(super) struct ObserverMailbox {
    sender: mpsc::Sender<QueuedEvent>,
    receiver: std::sync::Mutex<Option<mpsc::Receiver<QueuedEvent>>>,
    events: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
    failure: AtomicU8,
}

impl ObserverMailbox {
    pub(super) fn new() -> Self {
        let (sender, receiver) = mpsc::channel(MAX_OBSERVER_EVENTS);
        Self {
            sender,
            receiver: std::sync::Mutex::new(Some(receiver)),
            events: Arc::new(Semaphore::new(MAX_OBSERVER_EVENTS)),
            bytes: Arc::new(Semaphore::new(MAX_OBSERVER_BYTES)),
            failure: AtomicU8::new(0),
        }
    }

    fn take_receiver(&self) -> mpsc::Receiver<QueuedEvent> {
        self.receiver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .expect("an ACP operation starts one observer worker")
    }

    fn try_enqueue(&self, observed: ObservedEvent) -> Result<(), ()> {
        if self.failure.load(Ordering::Acquire) != 0 {
            return Err(());
        }
        let encoded_bytes = bounded_encoded_bytes(&observed.event)?;
        let charged =
            u32::try_from(encoded_bytes.saturating_add(OBSERVER_EVENT_OVERHEAD)).map_err(|_| ())?;
        // Permits include the currently processing item, not merely entries
        // still inside mpsc. Neither a blocked replay nor a slow socket grows
        // an uncounted local backlog.
        let event = Arc::clone(&self.events)
            .try_acquire_owned()
            .map_err(|_| ())?;
        let bytes = Arc::clone(&self.bytes)
            .try_acquire_many_owned(charged)
            .map_err(|_| ())?;
        self.sender
            .try_send(QueuedEvent {
                observed,
                _event: event,
                _bytes: bytes,
            })
            .map_err(|_| ())
    }

    pub(super) fn fail(&self, reason: ObserverFailure) {
        let _ = self
            .failure
            .compare_exchange(0, reason as u8, Ordering::AcqRel, Ordering::Acquire);
    }

    pub(super) fn failure_message(&self) -> Option<&'static str> {
        match self.failure.load(Ordering::Acquire) {
            1 => Some(ObserverFailure::Backlog.message()),
            2 => Some(ObserverFailure::HistoryChanged.message()),
            3 => Some(ObserverFailure::UpdateTooLarge.message()),
            4 => Some(ObserverFailure::BroadcastLagged.message()),
            5 => Some(ObserverFailure::ProjectionLimit.message()),
            _ => None,
        }
    }
}

// Count a bounded encoding without allocating a second event-sized buffer.
// The byte budget measures encoded payload, rather than exact allocator RSS.
// Count and byte limits bound queued envelopes and variable content separately.
// Stop as soon as this item cannot fit.
fn bounded_encoded_bytes(value: &impl Serialize) -> Result<usize, ()> {
    struct Counter(usize);
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            if self.0 > MAX_OBSERVER_BYTES.saturating_sub(OBSERVER_EVENT_OVERHEAD) {
                return Err(std::io::Error::other(
                    "ACP observer item exceeds its byte budget",
                ));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value).map_err(|_| ())?;
    Ok(counter.0)
}

impl AcpSessionOperation {
    pub(super) fn fail_observer(&self, reason: ObserverFailure) {
        self.observer.fail(reason);
        self.cancellation.cancel();
    }

    pub(super) fn failure_message(&self) -> Option<&'static str> {
        self.observer.failure_message()
    }
}

impl AcpConnectionContext {
    pub(super) async fn start_observing(self: &Arc<Self>, cx: ConnectionTo<Client>) {
        if self.observing.swap(true, Ordering::SeqCst) {
            return;
        }
        if self.client_supports_form_elicitation.load(Ordering::SeqCst) {
            *self.question_responder.lock().await = Some(self.agent.question_responder());
        }
        let context = self.clone();
        let mut events = self.agent.subscribe();
        self.background_tasks.lock().await.spawn(async move {
            loop {
                let event = tokio::select! {
                    _ = context.lifetime.cancelled() => break,
                    event = events.recv() => match event {
                        Ok(event) => event,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            // Lost broadcast entries no longer carry routing IDs.
                            // Invalidate potentially affected projections, while
                            // preserving the connection and authoritative work.
                            let operations: Vec<_> = context.session_operations.lock().await
                                .values().cloned().collect();
                            for operation in operations {
                                operation.fail_observer(ObserverFailure::BroadcastLagged);
                            }
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => { context.lifetime.cancel(); break; },
                    },
                };
                // This dispatch performs no replay, projection, or socket IO.
                if context.observe(&cx, event).await.is_err() {
                    context.lifetime.cancel();
                    break;
                }
                #[cfg(test)]
                context.observer_dispatches.fetch_add(1, Ordering::Release);
            }
        });
    }

    pub(super) async fn observe(
        self: &Arc<Self>,
        cx: &ConnectionTo<Client>,
        event: AgentServiceEvent,
    ) -> Result<(), AcpOutboundSendError> {
        use AgentServiceEvent as Event;
        match event {
            Event::AccountChanged(_) => self.lifetime.cancel(),
            Event::Question {
                session_id,
                request_id,
                questions,
            } => {
                // Elicitation must not wait for replay or a whole model turn.
                // Its task remains deduplicated and is tied to this operation.
                let operation = self
                    .session_operations
                    .lock()
                    .await
                    .get(&session_id)
                    .cloned();
                if let Some(operation) = operation
                    && !operation.cancellation.is_cancelled()
                    && self.client_supports_form_elicitation.load(Ordering::SeqCst)
                    && self.sessions.lock().await.contains_key(&session_id)
                    && self.eliciting.lock().await.insert(request_id.clone())
                {
                    let Ok(slot) = Arc::clone(&self.elicitation_slots).try_acquire_owned() else {
                        self.eliciting.lock().await.remove(&request_id);
                        operation.fail_observer(ObserverFailure::Backlog);
                        return Ok(());
                    };
                    let context = self.clone();
                    let cx = cx.clone();
                    let mut tasks = self.background_tasks.lock().await;
                    while tasks.try_join_next().is_some() {}
                    tasks.spawn(async move {
                        let _slot = slot;
                        context
                            .elicit_question(
                                &cx,
                                session_id,
                                request_id.clone(),
                                questions,
                                &operation.cancellation,
                            )
                            .await;
                        context.eliciting.lock().await.remove(&request_id);
                    });
                }
            }
            Event::HistoryReplaced {
                user_id,
                session_id,
                rows_epoch,
            } => {
                if user_id == self.agent.user_id()
                    && let Some(operation) = self.session_operations.lock().await.get(&session_id)
                    && rows_epoch > operation.history_epoch
                {
                    operation.fail_observer(ObserverFailure::HistoryChanged);
                }
            }
            event => {
                let session_id = match &event {
                    Event::TimelineItem { session_id, .. }
                    | Event::SessionUpdated { session_id, .. }
                    | Event::ThinkingChanged { session_id, .. }
                    | Event::Run { session_id, .. } => session_id.clone(),
                    _ => return Ok(()),
                };
                let Some(operation) = self
                    .session_operations
                    .lock()
                    .await
                    .get(&session_id)
                    .cloned()
                else {
                    return Ok(());
                };
                if operation.cancellation.is_cancelled() {
                    return Ok(());
                }
                let suppress_run = if let Event::Run { run_id, .. } = &event {
                    self.owned_runs.lock().await.get(&session_id) == Some(run_id)
                        || self.prompt_states.lock().await.contains_key(&session_id)
                } else {
                    false
                };
                if matches!(
                    &event,
                    Event::Run {
                        event: AgentRunEvent::HistoryReplaced,
                        ..
                    }
                ) {
                    if !suppress_run {
                        operation.fail_observer(ObserverFailure::HistoryChanged);
                    }
                    return Ok(());
                }
                // Irrelevant run events do not consume this session's budget.
                if matches!(&event, Event::Run { event, .. }
                    if !matches!(event, AgentRunEvent::TimelineItem(_) | AgentRunEvent::Error(_) | AgentRunEvent::SessionUpdated(_)))
                {
                    return Ok(());
                }
                if suppress_run
                    && !matches!(
                        &event,
                        Event::Run {
                            event: AgentRunEvent::SessionUpdated(_),
                            ..
                        }
                    )
                {
                    return Ok(());
                }
                if operation
                    .observer
                    .try_enqueue(ObservedEvent {
                        event,
                        suppress_run,
                    })
                    .is_err()
                {
                    operation.fail_observer(ObserverFailure::Backlog);
                }
            }
        }
        Ok(())
    }

    pub(super) fn spawn_session_observer(
        self: &Arc<Self>,
        tasks: &mut tokio::task::JoinSet<()>,
        cx: ConnectionTo<Client>,
        session_id: String,
        operation: Arc<AcpSessionOperation>,
        slot: OwnedSemaphorePermit,
    ) {
        let mut receiver = operation.observer.take_receiver();
        let context = self.clone();
        tasks.spawn(async move {
            let _slot = slot;
            // The creator acquires this gate before registering the operation.
            // Even metadata cannot be dropped before the session is published.
            let ready = tokio::select! {
                biased;
                _ = operation.cancellation.cancelled() => false,
                guard = operation.replay_gate.lock() => { drop(guard); true },
            };
            if ready {
                loop {
                    let queued = tokio::select! {
                        biased;
                        _ = operation.cancellation.cancelled() => break,
                        event = receiver.recv() => match event { Some(event) => event, None => break },
                    };
                    match context.observe_session_event(&cx, &session_id, &operation, queued.observed).await {
                        Ok(()) => {},
                        Err(AcpOutboundSendError::Cancelled) => break,
                        Err(AcpOutboundSendError::UpdateTooLarge) => {
                            operation.fail_observer(ObserverFailure::UpdateTooLarge);
                            break;
                        }
                        Err(AcpOutboundSendError::Transport(_)) => {
                            context.lifetime.cancel();
                            break;
                        }
                    }
                    // The queued count/byte permits remain held through send.
                }
            }
            drop(receiver); // release unsent payloads before any cleanup await
            if let Some(message) = operation.failure_message() {
                // A pending load returns the latched reason in its RPC error.
                // An attached idle client receives one best-effort static notice.
                if !operation.replaying.load(Ordering::Acquire)
                    && context.session_operation_is_current(&session_id, &operation).await
                {
                    let _ = tokio::time::timeout(FAILURE_NOTICE_TIMEOUT,
                        context.send_observer_failure(&cx, &session_id, &operation, message)).await;
                }
                context.retire_session_if_same(&session_id, &operation).await;
            }
        });
    }

    // Reserve before taking the publication fence. A stale worker that waited
    // for socket credit must not enqueue a notice into a replacement load.
    async fn send_observer_failure(
        &self,
        cx: &ConnectionTo<Client>,
        session_id: &str,
        operation: &Arc<AcpSessionOperation>,
        message: &'static str,
    ) -> Result<(), AcpOutboundSendError> {
        let notification = SessionNotification::new(
            session_id.to_owned(),
            SessionUpdate::AgentMessageChunk(text_chunk(message.to_owned(), &notice_id())),
        );
        let encoded = serde_json::to_vec(&notification)
            .map_err(|_| AcpOutboundSendError::UpdateTooLarge)?
            .len();
        let reservation = self.outbound.reserve(encoded, &self.lifetime).await?;
        let _finalization = self.finalization.lock().await;
        if !self
            .session_operation_is_current(session_id, operation)
            .await
        {
            return Ok(());
        }
        self.outbound.enqueue(cx, notification, reservation)
    }

    async fn observe_session_event(
        &self,
        cx: &ConnectionTo<Client>,
        session_id: &str,
        operation: &Arc<AcpSessionOperation>,
        observed: ObservedEvent,
    ) -> Result<(), AcpOutboundSendError> {
        if !self
            .session_operation_is_current(session_id, operation)
            .await
            || operation.cancellation.is_cancelled()
        {
            return Ok(());
        }
        use AgentServiceEvent as Event;
        let summary = match &observed.event {
            Event::SessionUpdated { session, .. }
            | Event::Run {
                event: AgentRunEvent::SessionUpdated(session),
                ..
            } => Some(session),
            _ => None,
        };
        if let Some(summary) = summary {
            let changed = {
                let mut sessions = self.sessions.lock().await;
                sessions.get_mut(session_id).and_then(|session| {
                    let changed = session.message_count != summary.message_count
                        || summary
                            .model
                            .as_ref()
                            .is_some_and(|model| model != &session.model);
                    session.message_count = summary.message_count;
                    if let Some(model) = &summary.model
                        && model != &session.model
                    {
                        session.model = model.clone();
                        session.catalog = None;
                        session.thinking = None;
                    }
                    changed.then(|| session.config_options())
                })
            };
            if let Some(options) = changed {
                self.send_session_update(
                    cx,
                    SessionNotification::new(
                        session_id.to_owned(),
                        SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(options)),
                    ),
                    &operation.cancellation,
                )
                .await?;
            }
        }
        let item = match observed.event {
            Event::ThinkingChanged { state, .. } => {
                let update = {
                    let mut sessions = self.sessions.lock().await;
                    let Some(session) = sessions.get_mut(session_id) else {
                        return Ok(());
                    };
                    session.thinking = Some(state);
                    SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(
                        session.config_options(),
                    ))
                };
                return self
                    .send_session_update(
                        cx,
                        SessionNotification::new(session_id.to_owned(), update),
                        &operation.cancellation,
                    )
                    .await;
            }
            Event::Run { event, .. } => {
                if observed.suppress_run {
                    return Ok(());
                }
                match event {
                    AgentRunEvent::TimelineItem(item) | AgentRunEvent::Error(item) => item,
                    AgentRunEvent::SessionUpdated(summary) => {
                        return self
                            .send_observed_title(cx, session_id, summary.title, operation)
                            .await;
                    }
                    _ => return Ok(()),
                }
            }
            Event::TimelineItem { item, .. } => item,
            Event::SessionUpdated { session, .. } => {
                return self
                    .send_observed_title(cx, session_id, session.title, operation)
                    .await;
            }
            _ => return Ok(()),
        };
        let _gate = tokio::select! {
            biased;
            _ = operation.cancellation.cancelled() => return Err(AcpOutboundSendError::Cancelled),
            gate = operation.replay_gate.lock() => gate,
        };
        if !self
            .session_operation_is_current(session_id, operation)
            .await
        {
            return Ok(());
        }
        let update = {
            let mut sessions = self.sessions.lock().await;
            let Some(session) = sessions.get_mut(session_id) else {
                return Ok(());
            };
            if item.revision == 0 || item.revision > session.projection.snapshot_revision {
                match checked_timeline_update(&item, &mut session.projection, true) {
                    Ok(update) => update,
                    Err(_) => {
                        operation.fail_observer(ObserverFailure::ProjectionLimit);
                        return Err(AcpOutboundSendError::Cancelled);
                    }
                }
            } else {
                None
            }
        };
        if let Some(update) = update {
            self.send_session_update(
                cx,
                SessionNotification::new(session_id.to_owned(), update),
                &operation.cancellation,
            )
            .await?;
        }
        Ok(())
    }

    async fn send_observed_title(
        &self,
        cx: &ConnectionTo<Client>,
        session_id: &str,
        title: String,
        operation: &AcpSessionOperation,
    ) -> Result<(), AcpOutboundSendError> {
        if let Some(update) = self.title_update(session_id, title).await {
            self.send_session_update(
                cx,
                SessionNotification::new(session_id.to_owned(), update),
                &operation.cancellation,
            )
            .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timeline(text: &str, merge: &str) -> ObservedEvent {
        let item = serde_json::from_value(serde_json::json!({
            "id": "same-row", "itemType": "message", "role": "assistant",
            "text": text, "createdMs": 1, "merge": merge,
        }))
        .unwrap();
        ObservedEvent {
            event: AgentServiceEvent::TimelineItem {
                session_id: "task".into(),
                run_id: None,
                item,
            },
            suppress_run: false,
        }
    }

    #[tokio::test]
    async fn queued_append_deltas_keep_fifo_and_final_suffixes() {
        let mailbox = ObserverMailbox::new();
        let mut receiver = mailbox.take_receiver();
        for (text, merge) in [("A", "append"), ("B", "append"), ("ABC", "replace")] {
            mailbox.try_enqueue(timeline(text, merge)).unwrap();
        }
        let mut projection = AcpProjection::default();
        let mut received = String::new();
        for expected in ["A", "B", "ABC"] {
            let queued = receiver.recv().await.unwrap();
            let AgentServiceEvent::TimelineItem { item, .. } = &queued.observed.event else {
                panic!("expected timeline event");
            };
            assert_eq!(item.text.as_deref(), Some(expected));
            let mut update = timeline_update(item, &mut projection, true).unwrap();
            received.push_str(paged::notification_text(&mut update).unwrap());
        }
        assert_eq!(received, "ABC");
        assert_eq!(mailbox.events.available_permits(), MAX_OBSERVER_EVENTS);
        assert_eq!(mailbox.bytes.available_permits(), MAX_OBSERVER_BYTES);
    }

    #[tokio::test]
    async fn an_inflight_event_still_owns_its_count_and_byte_budget() {
        let mailbox = ObserverMailbox::new();
        let mut receiver = mailbox.take_receiver();
        for _ in 0..MAX_OBSERVER_EVENTS {
            mailbox.try_enqueue(timeline("x", "append")).unwrap();
        }
        let inflight = receiver.recv().await.unwrap();
        assert!(mailbox.try_enqueue(timeline("x", "append")).is_err());
        assert_eq!(mailbox.events.available_permits(), 0);
        drop(inflight);
        mailbox.try_enqueue(timeline("y", "append")).unwrap();
        drop(receiver);
        assert_eq!(mailbox.events.available_permits(), MAX_OBSERVER_EVENTS);
        assert_eq!(mailbox.bytes.available_permits(), MAX_OBSERVER_BYTES);
    }

    #[test]
    fn an_oversized_raw_event_is_refused_without_a_second_payload_allocation() {
        let mailbox = ObserverMailbox::new();
        assert!(
            mailbox
                .try_enqueue(timeline(&"x".repeat(MAX_OBSERVER_BYTES), "replace"))
                .is_err()
        );
        assert_eq!(mailbox.events.available_permits(), MAX_OBSERVER_EVENTS);
        assert_eq!(mailbox.bytes.available_permits(), MAX_OBSERVER_BYTES);
    }
}
