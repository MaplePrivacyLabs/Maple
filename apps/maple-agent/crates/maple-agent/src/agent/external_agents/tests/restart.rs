//! External agents across a restart of Maple, against the fake ACP agent:
//! an agent carries on in its own session, new agents are numbered past the
//! task's, and a turn the restart cut short reaches the model and the user
//! once.

use pi_ai::faux::faux_tool_call;

use super::end_to_end::{choose_for_task, switch_on_in_settings};
use super::*;
use crate::agent::AgentRunTerminal;
use crate::agent::tests::{Harness, eventually, finished, user_texts};

/// A harness whose task can call Grok Build, played by the fake ACP agent
/// in `mode`.
async fn grok_task(fixtures: &Fixtures, mode: &str) -> (Harness, String) {
    fixtures.install("grok", mode);
    let harness = Harness::new().await;
    let runtime = harness.runtime().await;
    runtime
        .external_agents
        .set_test_search_path(fixtures.search_path());
    switch_on_in_settings(&harness, &["grok"]);
    let task = harness.create_task().await;
    choose_for_task(&runtime, &task, &["grok"]);
    (harness, task)
}

/// Stop Maple and start it again over the same files.
async fn restart(harness: &mut Harness, fixtures: &Fixtures) {
    harness.restart().await;
    harness.start().await;
    harness
        .runtime()
        .await
        .external_agents
        .set_test_search_path(fixtures.search_path());
}

/// The activity on the transcript rows of the task's agent tool calls.
async fn agent_rows(harness: &Harness, task: &str) -> Vec<Value> {
    harness
        .handle
        .load_session(task.to_string())
        .await
        .unwrap()
        .timeline
        .iter()
        .filter_map(|item| Some(item.output.as_ref()?["structuredContent"][ACTIVITY_KEY].clone()))
        .filter(|activity| !activity.is_null())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_carries_on_in_its_session_after_maple_restarts() {
    let fixtures = Fixtures::new("acp");
    let (mut harness, task) = grok_task(&fixtures, "acp").await;
    harness.faux.push_message(vec![faux_tool_call(
        AGENT_START_TOOL,
        json!({"provider": "grok", "prompt": "Fix the parser"}),
    )]);
    harness.faux.push_text("Grok fixed it.");
    let mut run = harness.send(&task, "Delegate this").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);

    restart(&mut harness, &fixtures).await;
    harness.faux.push_message(vec![faux_tool_call(
        AGENT_SEND_TOOL,
        json!({"provider": "grok", "agent_id": "grok-1", "prompt": "Now the tests"}),
    )]);
    harness.faux.push_message(vec![faux_tool_call(
        AGENT_START_TOOL,
        json!({"provider": "grok", "prompt": "A second opinion"}),
    )]);
    harness.faux.push_text("Both done.");
    let mut run = harness.send(&task, "Carry on").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);

    // The agent the task started before the restart took the next turn, in
    // the same session, opened again in a new process.
    let rows = agent_rows(&harness, &task).await;
    let carried_on = rows
        .iter()
        .find(|activity| {
            activity["text"]
                .as_str()
                .is_some_and(|text| text.contains("Done: Now the tests"))
        })
        .unwrap_or_else(|| panic!("{rows:?}"));
    assert_eq!(carried_on["agentId"], "grok-1");
    assert_eq!(carried_on["threadId"], "fixture-session");
    assert_eq!(carried_on["status"], "completed");
    let log = fixtures.log();
    let resumed: Vec<&str> = log
        .lines()
        .filter(|line| line.contains("\"session/resume\""))
        .collect();
    assert_eq!(resumed.len(), 1, "{log}");
    assert!(resumed[0].contains("fixture-session"), "{log}");
    // A new agent is numbered past the one the task already had.
    assert!(
        rows.iter().any(|activity| activity["agentId"] == "grok-2"
            && activity["text"]
                .as_str()
                .is_some_and(|text| text.contains("Done: A second opinion"))),
        "{rows:?}"
    );
    // The model never sees the records.
    for request in harness.faux.requests() {
        assert!(
            user_texts(&request)
                .iter()
                .all(|text| !text.contains("external-agent-record")),
        );
    }
    harness
        .runtime()
        .await
        .external_agents
        .shutdown_all(WAIT)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_a_restart_cuts_short_is_reported_once() {
    let fixtures = Fixtures::new("acp");
    let (mut harness, task) = grok_task(&fixtures, "acp-slow").await;
    harness.faux.push_message(vec![faux_tool_call(
        AGENT_START_TOOL,
        json!({"provider": "grok", "prompt": "A long job", "background": true}),
    )]);
    harness.faux.push_text("Started it.");
    let mut run = harness.send(&task, "Delegate this").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    // The agent works on when Maple stops.
    let runtime = harness.runtime().await;
    eventually(|| {
        futures_util::FutureExt::now_or_never(runtime.external_agents.snapshot(&task))
            .is_some_and(|rows| !rows.is_empty())
    })
    .await;
    drop(runtime);

    restart(&mut harness, &fixtures).await;
    harness.faux.push_message(vec![faux_tool_call(
        AGENT_STATUS_TOOL,
        json!({"provider": "grok", "agent_id": "grok-1"}),
    )]);
    harness.faux.push_text("It was interrupted.");
    let mut run = harness.send(&task, "How is the agent doing?").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    harness.faux.push_text("Fine.");
    let mut run = harness.send(&task, "And now?").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);

    // The model hears of it once, before the user's next message.
    let request = harness.faux.requests().pop().unwrap();
    let told: Vec<String> = user_texts(&request)
        .into_iter()
        .filter(|text| text.starts_with("Maple stopped while these external agents were working"))
        .collect();
    assert_eq!(told.len(), 1, "{:?}", user_texts(&request));
    assert!(
        told[0].contains(r#"grok-1 (provider: "grok")"#),
        "{}",
        told[0]
    );
    // Its status says the turn was cut short, and the user sees a notice.
    let rows = agent_rows(&harness, &task).await;
    let status = rows.last().unwrap();
    assert_eq!(status["status"], "cancelled", "{rows:?}");
    assert_eq!(status["error"], records::INTERRUPTED_ERROR);
    let detail = harness.handle.load_session(task.clone()).await.unwrap();
    let notices = detail
        .timeline
        .iter()
        .filter(|item| {
            item.text.as_deref() == Some("External agent grok-1 (Grok Build) was interrupted.")
        })
        .count();
    assert_eq!(notices, 1);
    harness
        .runtime()
        .await
        .external_agents
        .shutdown_all(WAIT)
        .await;
}
