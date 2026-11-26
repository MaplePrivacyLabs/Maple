//! Each external agent's record in its task's session, which outlives
//! Maple's process. The agent's own session lives on with its vendor; the
//! record keeps which agent it belongs to, so after a restart `agent_send`
//! opens it again instead of the task forgetting the agent. Paseo keeps its
//! agents the same way. A record is an entry the model and the transcript
//! never see, and it goes with the task.

use std::collections::HashMap;
use std::path::PathBuf;

use pi_coding_agent::session::{EntryKind, SessionManager};
use serde::{Deserialize, Serialize};

use super::ExternalAgentActivity;

/// The custom entry type of an agent's record.
pub(super) const AGENT_RECORD_ENTRY: &str = "maple.external-agent-record";

/// What the model is told of a turn a restart cut short.
pub(super) const INTERRUPTED_ERROR: &str = "Maple stopped while this agent was working, so its last turn did not finish. Its session is kept: agent_send continues it.";

/// What the model is told when the agent never had a session to keep.
pub(super) const INTERRUPTED_UNRESUMABLE: &str = "Maple stopped before this agent had a session to keep, so it cannot be continued. Start a new agent with agent_start if the work is still needed.";

/// One agent of the task as it last stood.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AgentRecord {
    /// The first line of the prompt that started the agent.
    pub(super) task: String,
    /// The folder the agent works in.
    pub(super) cwd: PathBuf,
    pub(super) activity: ExternalAgentActivity,
}

/// The latest record of each agent on the session's current branch, in the
/// order the agents first appear.
pub(super) fn latest(session: &SessionManager) -> Vec<AgentRecord> {
    let mut order: Vec<String> = Vec::new();
    let mut records: HashMap<String, AgentRecord> = HashMap::new();
    for entry in session.branch() {
        let EntryKind::Custom {
            custom_type,
            data: Some(data),
        } = &entry.kind
        else {
            continue;
        };
        if custom_type != AGENT_RECORD_ENTRY {
            continue;
        }
        let Ok(record) = serde_json::from_value::<AgentRecord>(data.clone()) else {
            continue;
        };
        let id = record.activity.agent_id.clone();
        if !records.contains_key(&id) {
            order.push(id.clone());
        }
        records.insert(id, record);
    }
    order
        .into_iter()
        .filter_map(|id| records.remove(&id))
        .collect()
}

/// The number at the end of an agent id such as `grok-3`.
pub(super) fn number(agent_id: &str) -> Option<u64> {
    agent_id.rsplit_once('-')?.1.parse().ok()
}

/// Whether the record was left while the agent worked or waited on its
/// caller.
pub(super) fn was_working(activity: &ExternalAgentActivity) -> bool {
    matches!(activity.status.as_str(), "running" | "waiting")
}

/// The activity of an agent whose process is gone: a turn it was working on
/// did not finish, and what it waited on went with the process.
pub(super) fn after_restart(mut activity: ExternalAgentActivity) -> ExternalAgentActivity {
    if was_working(&activity) {
        activity.status = "cancelled".to_string();
        activity.error = Some(
            if activity.thread_id.is_some() {
                INTERRUPTED_ERROR
            } else {
                INTERRUPTED_UNRESUMABLE
            }
            .to_string(),
        );
        for command in &mut activity.commands {
            if command.status == "running" {
                command.status = "failed".to_string();
            }
        }
        // A question that held the turn went with the process. Questions
        // a finished turn ended with still wait for their answers.
        activity.questions.clear();
    }
    activity.pending_permission = None;
    activity.elapsed_ms = 0;
    activity
}

/// The message that tells the model which of its agents a restart cut
/// short, each named as `agent-id (provider: "id")`: those it can continue
/// and those that never had a session.
pub(super) fn interrupted_message(resumable: &[String], unresumable: &[String]) -> String {
    let mut parts = Vec::new();
    if !resumable.is_empty() {
        parts.push(format!(
            "Maple stopped while these external agents were working, so their turns did not finish: {}. Each keeps its session: agent_status shows what it did before it stopped, and agent_send with its provider and agent ID continues it.",
            resumable.join(", ")
        ));
    }
    if !unresumable.is_empty() {
        parts.push(format!(
            "Maple stopped before these external agents had a session to keep, so they cannot be continued: {}. Start a new agent with agent_start if the work is still needed.",
            unresumable.join(", ")
        ));
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::agent::external_agents::{ActivityCommand, ActivityQuestion};

    fn activity(agent_id: &str, status: &str) -> ExternalAgentActivity {
        ExternalAgentActivity {
            provider: "grok".into(),
            agent_id: agent_id.into(),
            thread_id: Some(format!("thread-{agent_id}")),
            status: status.into(),
            ..Default::default()
        }
    }

    fn record(agent_id: &str, status: &str) -> serde_json::Value {
        serde_json::to_value(AgentRecord {
            task: "Fix it".into(),
            cwd: PathBuf::from("/project"),
            activity: activity(agent_id, status),
        })
        .unwrap()
    }

    #[test]
    fn the_latest_record_of_each_agent_wins_in_first_seen_order() {
        let mut session = SessionManager::in_memory("/project");
        session.append_custom_entry(AGENT_RECORD_ENTRY, Some(record("grok-1", "running")));
        session.append_custom_entry(AGENT_RECORD_ENTRY, Some(record("antigravity-2", "running")));
        session.append_custom_entry("maple.notice", Some(json!({"id": "n"})));
        session.append_custom_entry(AGENT_RECORD_ENTRY, Some(json!({"not": "a record"})));
        session.append_custom_entry(AGENT_RECORD_ENTRY, Some(record("grok-1", "completed")));
        let latest = latest(&session);
        let seen: Vec<(&str, &str)> = latest
            .iter()
            .map(|record| {
                (
                    record.activity.agent_id.as_str(),
                    record.activity.status.as_str(),
                )
            })
            .collect();
        assert_eq!(
            seen,
            [("grok-1", "completed"), ("antigravity-2", "running")]
        );
    }

    #[test]
    fn agent_numbers_read_from_their_ids() {
        assert_eq!(number("grok-12"), Some(12));
        assert_eq!(number("claude-1"), Some(1));
        assert_eq!(number("grok"), None);
        assert_eq!(number("grok-x"), None);
    }

    #[test]
    fn a_turn_a_restart_cut_short_reads_as_cancelled() {
        let mut working = activity("grok-1", "waiting");
        working.commands.push(ActivityCommand {
            id: "c".into(),
            command: "sleep 30".into(),
            exit_code: None,
            status: "running".into(),
        });
        working.questions.push(ActivityQuestion {
            id: "q".into(),
            question: "Red or blue?".into(),
            options: Vec::new(),
            multi_select: false,
        });
        working.pending_permission = Some("approve x".into());
        working.elapsed_ms = 9_000;
        let after = after_restart(working);
        assert_eq!(after.status, "cancelled");
        assert_eq!(after.error.as_deref(), Some(INTERRUPTED_ERROR));
        assert_eq!(after.commands[0].status, "failed");
        assert!(after.questions.is_empty());
        assert!(after.pending_permission.is_none());
        assert_eq!(after.elapsed_ms, 0);

        // A finished turn keeps how it ended, and the questions it ended
        // with, which still wait for their answers.
        let mut finished = activity("codex-1", "completed");
        finished.questions.push(ActivityQuestion {
            id: "q".into(),
            question: "Which branch?".into(),
            options: Vec::new(),
            multi_select: false,
        });
        let done = after_restart(finished);
        assert_eq!(done.status, "completed");
        assert!(done.error.is_none());
        assert_eq!(done.questions.len(), 1);

        // An agent that never had a session cannot be continued.
        let mut starting = activity("claude-1", "running");
        starting.thread_id = None;
        let after = after_restart(starting);
        assert_eq!(after.error.as_deref(), Some(INTERRUPTED_UNRESUMABLE));
    }

    #[test]
    fn the_model_hears_which_agents_it_can_continue() {
        let both = interrupted_message(
            &[r#"grok-1 (provider: "grok")"#.to_string()],
            &[r#"claude-2 (provider: "claude")"#.to_string()],
        );
        assert!(both.starts_with("Maple stopped while these external agents were working, so their turns did not finish: grok-1 (provider: \"grok\")."), "{both}");
        assert!(
            both.contains("cannot be continued: claude-2 (provider: \"claude\")."),
            "{both}"
        );
        let one = interrupted_message(&[], &[r#"claude-2 (provider: "claude")"#.to_string()]);
        assert!(one.starts_with("Maple stopped before"), "{one}");
    }
}
