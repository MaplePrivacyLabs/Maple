//! Broker for agent-initiated questions (the ask_user tool). The runtime
//! registers a pending question and blocks; the UI answers through the
//! service and the tool call resumes with the user's text.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use tokio::sync::{Mutex, oneshot};

use crate::agent::{AgentEventDispatcher, AgentServiceEvent, emit_agent_event};

type PendingQuestions = Arc<Mutex<HashMap<String, oneshot::Sender<String>>>>;

#[derive(Clone)]
pub(crate) struct QuestionBroker {
    pending: PendingQuestions,
    dispatcher: AgentEventDispatcher,
    next_id: Arc<std::sync::atomic::AtomicU64>,
}

/// Process-wide broker: each service initializes it; tool clients fetch it
/// lazily. A rebuilt service replaces the broker so questions reach the
/// dispatcher that is currently wired to the UI.
static GLOBAL_BROKER: RwLock<Option<QuestionBroker>> = RwLock::new(None);

/// Install a new broker as the process global and return it to the
/// owning service.
pub(crate) fn init_global(dispatcher: AgentEventDispatcher) -> QuestionBroker {
    let broker = QuestionBroker::new(dispatcher);
    match GLOBAL_BROKER.write() {
        Ok(mut slot) => *slot = Some(broker.clone()),
        Err(poisoned) => *poisoned.into_inner() = Some(broker.clone()),
    }
    broker
}

pub(crate) fn global() -> Option<QuestionBroker> {
    match GLOBAL_BROKER.read() {
        Ok(slot) => slot.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// Removes a pending entry when the asking future is dropped before the
/// answer arrives, so a cancelled run does not leak its sender.
struct PendingGuard {
    pending: PendingQuestions,
    request_id: String,
    armed: bool,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(mut pending) = self.pending.try_lock() {
            pending.remove(&self.request_id);
            return;
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let pending = Arc::clone(&self.pending);
            let request_id = std::mem::take(&mut self.request_id);
            handle.spawn(async move {
                pending.lock().await.remove(&request_id);
            });
        }
    }
}

impl QuestionBroker {
    pub(crate) fn new(dispatcher: AgentEventDispatcher) -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            dispatcher,
            next_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        }
    }

    /// Ask the user one or more related questions and wait for the
    /// answers. Emits a service event the UI renders as a question card
    /// covering the whole batch.
    pub(crate) async fn ask(
        &self,
        session_id: &str,
        questions: Vec<crate::agent::AgentQuestion>,
    ) -> String {
        let request_id = format!(
            "question_{}_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_millis())
                .unwrap_or(0),
            self.next_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(request_id.clone(), tx);
        let mut guard = PendingGuard {
            pending: Arc::clone(&self.pending),
            request_id: request_id.clone(),
            armed: true,
        };
        emit_agent_event(
            &self.dispatcher,
            AgentServiceEvent::Question {
                session_id: session_id.to_string(),
                request_id: request_id.clone(),
                questions,
            },
        );
        let answer = rx.await.unwrap_or_default();
        self.pending.lock().await.remove(&request_id);
        guard.armed = false;
        answer
    }

    /// True when both handles share one pending map.
    pub(crate) fn same_as(&self, other: &QuestionBroker) -> bool {
        Arc::ptr_eq(&self.pending, &other.pending)
    }

    /// Number of questions still waiting for an answer.
    #[cfg(test)]
    pub(crate) async fn pending_count(&self) -> usize {
        self.pending.lock().await.len()
    }
}

impl QuestionBroker {
    /// Deliver the user's answer; true when a pending question matched.
    /// Entries whose asker has gone away are dropped on the way.
    pub(crate) async fn answer(&self, request_id: &str, answer: String) -> bool {
        let mut pending = self.pending.lock().await;
        pending.retain(|_, tx| !tx.is_closed());
        let sender = pending.remove(request_id);
        drop(pending);
        match sender {
            Some(tx) => tx.send(answer).is_ok(),
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentEventSink;

    struct NullSink;

    impl AgentEventSink for NullSink {
        fn emit(&self, _event: &AgentServiceEvent) {}
    }

    fn broker() -> QuestionBroker {
        QuestionBroker::new(AgentEventDispatcher::new(Arc::new(NullSink)))
    }

    #[tokio::test]
    async fn cancelled_ask_removes_pending_entry() {
        let broker = broker();
        let ask = {
            let broker = broker.clone();
            tokio::spawn(async move { broker.ask("session", Vec::new()).await })
        };
        tokio::task::yield_now().await;
        assert_eq!(broker.pending_count().await, 1);
        ask.abort();
        let _ = ask.await;
        tokio::task::yield_now().await;
        assert_eq!(broker.pending_count().await, 0);
    }

    #[tokio::test]
    async fn answer_drops_closed_senders() {
        let broker = broker();
        let (tx, rx) = oneshot::channel();
        drop(rx);
        broker.pending.lock().await.insert("stale".to_string(), tx);
        assert!(!broker.answer("missing", String::new()).await);
        assert_eq!(broker.pending_count().await, 0);
    }
}
