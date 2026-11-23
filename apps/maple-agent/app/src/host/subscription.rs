//! Per-connection task deltas. An untouched filter preserves legacy delivery.

use std::collections::HashSet;

use maple_agent::agent::{AgentRunEvent, AgentServiceEvent};
use serde::{Deserialize, Serialize};

const MAX_SUBSCRIBED_TASKS: usize = 200;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SubscribeRequest {
    /// Allocated synchronously at the frontend's navigation intent, before any
    /// asynchronous bootstrap or stream can delay delivery. Zero supports
    /// callers that have not started sequencing; it cannot replace a positive
    /// intent already accepted on this connection.
    #[serde(default)]
    pub intent: u64,
    pub tasks: Vec<String>,
}

#[derive(Default)]
pub(crate) struct Subscription {
    /// None means the connection has never subscribed; Some(empty) wants summaries.
    tasks: Option<HashSet<String>>,
    intent: u64,
}

impl Subscription {
    pub(crate) fn set(&mut self, request: SubscribeRequest) -> Result<(), String> {
        if request.tasks.len() > MAX_SUBSCRIBED_TASKS
            || request.tasks.iter().any(|task| !canonical_uuid(task))
        {
            return Err("Subscribe requires at most 200 canonical task UUIDs".into());
        }
        let tasks = request.tasks.into_iter().collect::<HashSet<_>>();
        if request.intent < self.intent {
            return Ok(());
        }
        if request.intent != 0 && request.intent == self.intent {
            return if self.tasks.as_ref() == Some(&tasks) {
                Ok(())
            } else {
                Err("A subscription intent cannot select different tasks".into())
            };
        }
        self.intent = request.intent;
        self.tasks = Some(tasks);
        Ok(())
    }

    pub(crate) fn allows(&self, event: &AgentServiceEvent) -> bool {
        let Some(tasks) = &self.tasks else {
            return true;
        };
        match event {
            AgentServiceEvent::TimelineItem { session_id, .. } => tasks.contains(session_id),
            // A compaction's start and end reach every task, open or not:
            // the frame keeps each task's compaction until it hears the end.
            AgentServiceEvent::Run {
                session_id, event, ..
            } => {
                matches!(
                    event,
                    AgentRunEvent::Started
                        | AgentRunEvent::Finished(_)
                        | AgentRunEvent::SessionUpdated(_)
                        | AgentRunEvent::HistoryReplaced
                        | AgentRunEvent::Compacting
                        | AgentRunEvent::Compacted
                        | AgentRunEvent::CompactionStopped
                ) || tasks.contains(session_id)
            }
            _ => true,
        }
    }
}

fn canonical_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use maple_agent::agent::AgentRunTerminal;

    const A: &str = "01234567-89ab-7def-8123-456789abcdef";
    const B: &str = "11234567-89ab-7def-8123-456789abcdef";

    fn delta(task: &str) -> AgentServiceEvent {
        AgentServiceEvent::Run {
            session_id: task.into(),
            run_id: "run".into(),
            event: AgentRunEvent::SubagentActivity {
                id: "agent".into(),
                tool: "read".into(),
            },
        }
    }

    #[test]
    fn connections_that_never_subscribed_get_every_event() {
        let subscription = Subscription::default();
        assert!(subscription.allows(&delta(A)));
        assert!(subscription.allows(&delta(B)));
    }

    #[test]
    fn connections_that_subscribed_get_only_their_tasks_deltas() {
        let mut subscription = Subscription::default();
        subscription
            .set(SubscribeRequest {
                intent: 0,
                tasks: vec![A.into()],
            })
            .unwrap();
        assert!(subscription.allows(&delta(A)));
        assert!(!subscription.allows(&delta(B)));
        subscription
            .set(SubscribeRequest {
                intent: 0,
                tasks: Vec::new(),
            })
            .unwrap();
        assert!(!subscription.allows(&delta(A)));
    }

    #[test]
    fn summary_events_reach_every_client() {
        let mut subscription = Subscription::default();
        subscription
            .set(SubscribeRequest {
                intent: 0,
                tasks: Vec::new(),
            })
            .unwrap();
        for event in [
            AgentRunEvent::Started,
            AgentRunEvent::Finished(AgentRunTerminal::Completed),
            AgentRunEvent::HistoryReplaced,
        ] {
            assert!(subscription.allows(&AgentServiceEvent::Run {
                session_id: B.into(),
                run_id: "run".into(),
                event
            }));
        }
        assert!(subscription.allows(&AgentServiceEvent::QuestionResolved {
            request_id: "question".into()
        }));
        assert!(subscription.allows(&AgentServiceEvent::HistoryReplaced {
            user_id: "account".into(),
            session_id: B.into(),
            rows_epoch: 2
        }));
        assert!(subscription.allows(&AgentServiceEvent::StoreStateChanged {
            state: maple_agent::store::StoreState::Open
        }));
    }

    /// Leaving a task while it compacts must not leave it saying so: the
    /// compaction's end reaches the client with another task open.
    #[test]
    fn a_compaction_reaches_clients_watching_another_task() {
        let mut subscription = Subscription::default();
        subscription
            .set(SubscribeRequest {
                intent: 0,
                tasks: vec![A.into()],
            })
            .unwrap();
        for event in [
            AgentRunEvent::Compacting,
            AgentRunEvent::Compacted,
            AgentRunEvent::CompactionStopped,
        ] {
            assert!(subscription.allows(&AgentServiceEvent::Run {
                session_id: B.into(),
                run_id: "run".into(),
                event
            }));
        }
        assert!(!subscription.allows(&delta(B)), "other deltas stay out");
    }

    #[test]
    fn invalid_subscribe_does_not_replace_the_previous_filter() {
        let mut subscription = Subscription::default();
        subscription
            .set(SubscribeRequest {
                intent: 0,
                tasks: vec![A.into()],
            })
            .unwrap();
        for tasks in [vec!["../task".into()], vec![B.into(); 201]] {
            assert!(
                subscription
                    .set(SubscribeRequest { intent: 0, tasks })
                    .is_err()
            );
            assert!(subscription.allows(&delta(A)));
            assert!(!subscription.allows(&delta(B)));
        }
    }

    #[test]
    fn a_delayed_intent_cannot_replace_a_newer_selection() {
        let mut subscription = Subscription::default();
        subscription
            .set(SubscribeRequest {
                intent: 2,
                tasks: vec![B.into()],
            })
            .unwrap();
        subscription
            .set(SubscribeRequest {
                intent: 1,
                tasks: vec![A.into()],
            })
            .unwrap();
        assert!(subscription.allows(&delta(B)));
        assert!(!subscription.allows(&delta(A)));
        // An unsequenced compatibility caller cannot undo a sequenced intent.
        subscription
            .set(SubscribeRequest {
                intent: 0,
                tasks: Vec::new(),
            })
            .unwrap();
        assert!(subscription.allows(&delta(B)));
    }

    #[test]
    fn retries_are_idempotent_and_reusing_an_intent_cannot_select_other_tasks() {
        let mut subscription = Subscription::default();
        subscription
            .set(SubscribeRequest {
                intent: 1,
                tasks: vec![A.into()],
            })
            .unwrap();
        subscription
            .set(SubscribeRequest {
                intent: 1,
                tasks: vec![A.into(), A.into()],
            })
            .unwrap();
        assert!(
            subscription
                .set(SubscribeRequest {
                    intent: 1,
                    tasks: vec![B.into()]
                })
                .is_err()
        );
        assert!(subscription.allows(&delta(A)));
        assert!(!subscription.allows(&delta(B)));
        subscription
            .set(SubscribeRequest {
                intent: 2,
                tasks: Vec::new(),
            })
            .unwrap();
        subscription
            .set(SubscribeRequest {
                intent: 1,
                tasks: vec![A.into()],
            })
            .unwrap();
        assert!(!subscription.allows(&delta(A)));
    }
}
