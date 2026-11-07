//! Broker for agent-initiated questions (the ask_user tool). The runtime
//! registers a pending question and blocks; the UI answers through the
//! service and the tool call resumes with the user's text.

use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::{Arc, RwLock};

use tokio::sync::{Mutex, oneshot};

use crate::agent::{AgentEventDispatcher, AgentServiceEvent, emit_agent_event};

const MAX_PENDING_QUESTIONS: usize = 32;
const MAX_QUESTION_EVENT_BYTES: usize = 16 * 1024;
const QUESTION_TOO_LARGE: &str =
    "This question is too large to display. Ask a shorter question with fewer options.";
const TOO_MANY_QUESTIONS: &str = "Too many questions are waiting for an answer. Wait for a pending question to be answered before asking another.";

/// Count the bytes actually produced by JSON escaping, without retaining a
/// serialized copy or letting serialization run past the admission budget.
struct QuestionBudget {
    remaining: usize,
}

impl Write for QuestionBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.remaining {
            return Err(io::Error::other("question event exceeds its byte budget"));
        }
        self.remaining -= bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct PendingQuestion {
    sender: oneshot::Sender<String>,
    event: AgentServiceEvent,
}
type PendingQuestions = Arc<Mutex<HashMap<String, PendingQuestion>>>;

/// Keeps question handling available while a capable interface is attached.
pub struct QuestionResponder(Arc<std::sync::atomic::AtomicUsize>);
impl Drop for QuestionResponder {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[derive(Clone)]
pub(crate) struct QuestionBroker {
    pending: PendingQuestions,
    dispatcher: AgentEventDispatcher,
    next_id: Arc<std::sync::atomic::AtomicU64>,
    responders: Arc<std::sync::atomic::AtomicUsize>,
    managed: Arc<std::sync::atomic::AtomicBool>,
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
    dispatcher: AgentEventDispatcher,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        emit_agent_event(
            &self.dispatcher,
            AgentServiceEvent::QuestionResolved {
                request_id: self.request_id.clone(),
            },
        );
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
            responders: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            managed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    pub(crate) fn manage_availability(&self) {
        self.managed
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub(crate) fn responder(&self) -> QuestionResponder {
        self.responders
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        QuestionResponder(self.responders.clone())
    }

    pub(crate) async fn snapshot(&self) -> Vec<AgentServiceEvent> {
        let mut pending = self.pending.lock().await;
        pending.retain(|_, question| !question.sender.is_closed());
        pending
            .values()
            .map(|question| question.event.clone())
            .collect()
    }

    /// Ask the user one or more related questions and wait for the
    /// answers. Emits a service event the UI renders as a question card
    /// covering the whole batch.
    pub(crate) async fn ask(
        &self,
        session_id: &str,
        questions: Vec<crate::agent::AgentQuestion>,
    ) -> String {
        match self.try_ask(session_id, questions).await {
            Ok(answer) | Err(answer) => answer,
        }
    }

    /// As [`Self::ask`], with a question nobody can be shown as an error: the
    /// text the asker can pass on, never an answer.
    pub(crate) async fn try_ask(
        &self,
        session_id: &str,
        questions: Vec<crate::agent::AgentQuestion>,
    ) -> Result<String, String> {
        if self.managed.load(std::sync::atomic::Ordering::SeqCst)
            && self.responders.load(std::sync::atomic::Ordering::SeqCst) == 0
        {
            return Err("No connected interface can show questions. Ask in plain text, or have the user open Maple desktop or an ACP client with form elicitation.".into());
        }
        let request_id = format!(
            "question_{}_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_millis())
                .unwrap_or(0),
            self.next_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let event = AgentServiceEvent::Question {
            session_id: session_id.into(),
            request_id: request_id.clone(),
            questions,
        };
        // Count, byte validation and insertion share one critical section, so
        // simultaneous asks cannot reserve more than the bounded bootstrap can
        // return. Rejections never install a sender or emit a question event.
        let mut pending = self.pending.lock().await;
        pending.retain(|_, question| !question.sender.is_closed());
        if pending.len() >= MAX_PENDING_QUESTIONS {
            return Err(TOO_MANY_QUESTIONS.into());
        }
        let mut budget = QuestionBudget {
            remaining: MAX_QUESTION_EVENT_BYTES,
        };
        if serde_json::to_writer(&mut budget, &event).is_err() {
            return Err(QUESTION_TOO_LARGE.into());
        }
        let (tx, rx) = oneshot::channel();
        pending.insert(
            request_id.clone(),
            PendingQuestion {
                sender: tx,
                event: event.clone(),
            },
        );
        drop(pending);
        let mut guard = PendingGuard {
            pending: Arc::clone(&self.pending),
            request_id: request_id.clone(),
            armed: true,
            dispatcher: self.dispatcher.clone(),
        };
        emit_agent_event(&self.dispatcher, event);
        let answer = rx.await.unwrap_or_default();
        self.pending.lock().await.remove(&request_id);
        guard.armed = false;
        Ok(answer)
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
        pending.retain(|_, question| !question.sender.is_closed());
        let sender = pending.remove(request_id);
        drop(pending);
        match sender {
            Some(tx) => {
                let answered = tx.sender.send(answer).is_ok();
                emit_agent_event(
                    &self.dispatcher,
                    AgentServiceEvent::QuestionResolved {
                        request_id: request_id.into(),
                    },
                );
                answered
            }
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

    #[derive(Default)]
    struct Recorder(std::sync::Mutex<Vec<AgentServiceEvent>>);
    impl AgentEventSink for Recorder {
        fn emit(&self, event: &AgentServiceEvent) {
            self.0.lock().unwrap().push(event.clone());
        }
    }

    fn recording_broker() -> (QuestionBroker, Arc<Recorder>) {
        let recorder = Arc::new(Recorder::default());
        (
            QuestionBroker::new(AgentEventDispatcher::new(recorder.clone())),
            recorder,
        )
    }

    fn question(text: String) -> crate::agent::AgentQuestion {
        crate::agent::AgentQuestion {
            multi_select: false,
            id: "synthetic".into(),
            header: "Synthetic".into(),
            question: text,
            options: Vec::new(),
            permission: None,
        }
    }

    #[test]
    fn byte_budget_counts_json_escaping_and_accepts_exact_fit() {
        let event = AgentServiceEvent::Question {
            session_id: "session".into(),
            request_id: "question".into(),
            questions: vec![question("\0\\\"\n".repeat(500))],
        };
        let encoded = serde_json::to_vec(&event).unwrap();
        let mut exact = QuestionBudget {
            remaining: encoded.len(),
        };
        serde_json::to_writer(&mut exact, &event).unwrap();
        assert_eq!(exact.remaining, 0);
        let mut short = QuestionBudget {
            remaining: encoded.len() - 1,
        };
        assert!(serde_json::to_writer(&mut short, &event).is_err());
    }

    #[tokio::test]
    async fn oversized_escaped_questions_leave_no_pending_entry_or_ghost_event() {
        let (broker, recorder) = recording_broker();
        // This small raw input expands sixfold when JSON encodes NUL bytes.
        let text = "\0".repeat(3_000);
        assert!(text.len() < MAX_QUESTION_EVENT_BYTES);
        let response = broker.ask("session", vec![question(text)]).await;
        assert_eq!(response, QUESTION_TOO_LARGE);
        assert!(response.len() < 256);
        assert!(broker.snapshot().await.is_empty());
        assert_eq!(broker.pending_count().await, 0);
        assert!(
            recorder.0.lock().unwrap().is_empty(),
            "a rejected ask must not publish Question or QuestionResolved"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn simultaneous_admission_stays_bounded_and_rejections_emit_no_ghosts() {
        let (broker, recorder) = recording_broker();
        let arrivals = MAX_PENDING_QUESTIONS * 2;
        let barrier = Arc::new(tokio::sync::Barrier::new(arrivals + 1));
        let (responses, mut rejected) = tokio::sync::mpsc::unbounded_channel();
        let mut asks = Vec::new();
        for _ in 0..arrivals {
            let broker = broker.clone();
            let barrier = barrier.clone();
            let responses = responses.clone();
            asks.push(tokio::spawn(async move {
                barrier.wait().await;
                let response = broker
                    .ask("session", vec![question("Which synthetic option?".into())])
                    .await;
                responses.send(response).unwrap();
            }));
        }
        drop(responses);
        barrier.wait().await;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            for _ in 0..MAX_PENDING_QUESTIONS {
                assert_eq!(rejected.recv().await.as_deref(), Some(TOO_MANY_QUESTIONS));
                assert!(broker.pending_count().await <= MAX_PENDING_QUESTIONS);
            }
        })
        .await
        .expect("overflowing asks must reject without waiting for an answer");
        let pending = broker.snapshot().await;
        assert_eq!(pending.len(), MAX_PENDING_QUESTIONS);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while recorder.0.lock().unwrap().len() < MAX_PENDING_QUESTIONS {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("all admitted questions must be published");
        {
            let events = recorder.0.lock().unwrap();
            assert_eq!(events.len(), MAX_PENDING_QUESTIONS);
            assert!(
                events
                    .iter()
                    .all(|event| matches!(event, AgentServiceEvent::Question { .. }))
            );
            assert!(
                events
                    .iter()
                    .all(|event| serde_json::to_vec(event).unwrap().len()
                        <= MAX_QUESTION_EVENT_BYTES)
            );
        }
        for event in pending {
            let AgentServiceEvent::Question { request_id, .. } = event else {
                panic!("only question events are retained");
            };
            assert!(broker.answer(&request_id, "synthetic answer".into()).await);
        }
        for ask in asks {
            ask.await.unwrap();
        }
        assert!(broker.snapshot().await.is_empty());
        assert_eq!(broker.pending_count().await, 0);
    }

    #[tokio::test]
    async fn closed_senders_are_reclaimed_before_capacity_is_checked() {
        let broker = broker();
        {
            let mut pending = broker.pending.lock().await;
            for index in 0..MAX_PENDING_QUESTIONS {
                let (sender, receiver) = oneshot::channel();
                drop(receiver);
                pending.insert(
                    index.to_string(),
                    PendingQuestion {
                        sender,
                        event: AgentServiceEvent::QuestionResolved {
                            request_id: index.to_string(),
                        },
                    },
                );
            }
        }
        let ask = {
            let broker = broker.clone();
            tokio::spawn(async move {
                broker
                    .ask("session", vec![question("Synthetic?".into())])
                    .await
            })
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while broker.snapshot().await.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!ask.is_finished());
        assert_eq!(broker.pending_count().await, 1);
        ask.abort();
        let _ = ask.await;
        tokio::task::yield_now().await;
        assert!(broker.snapshot().await.is_empty());
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
        broker.pending.lock().await.insert(
            "stale".to_string(),
            PendingQuestion {
                sender: tx,
                event: AgentServiceEvent::QuestionResolved {
                    request_id: "stale".into(),
                },
            },
        );
        assert!(!broker.answer("missing", String::new()).await);
        assert_eq!(broker.pending_count().await, 0);
    }
}
