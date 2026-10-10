//! External agents in the running runtime, with a scripted model and the
//! fake Codex: which tasks get the tools, a background agent's result
//! reaching the model and the transcript, and an agent that ends with its
//! task.

use pi_ai::faux::faux_tool_call;

use super::*;
use crate::agent::config::account_local_data_dir_path;
use crate::agent::tests::{Harness, eventually, finished, last_user_text};
use crate::agent::{
    AgentRunTerminal, AgentSessionIntegrationKind, AgentSetSessionMcpServerRequest,
};

/// Switch external agents on in Settings, as the Integrations page saves
/// them.
fn switch_on_in_settings(harness: &Harness, providers: &[&str]) {
    let dir = account_local_data_dir_path(harness.handle.paths(), &harness.handle.user_id).unwrap();
    fs::create_dir_all(&dir).unwrap();
    let entries: Vec<Value> = providers
        .iter()
        .map(|id| json!({"id": id, "enabled": true, "backend": "embedded"}))
        .collect();
    fs::write(
        dir.join("integrations.json"),
        json!({"version": 2, "integrations": entries}).to_string(),
    )
    .unwrap();
}

/// Switch external agents on for a task, as its MCP menu does once the
/// agent is installed.
fn choose_for_task(runtime: &AgentRuntime, task: &str, providers: &[&str]) {
    runtime
        .store
        .update(task, |row| {
            task::set_chosen_providers(
                row,
                providers
                    .iter()
                    .map(|provider| provider.to_string())
                    .collect(),
            )
        })
        .unwrap();
}

/// The external agent tools a run of the task would declare to the model.
async fn declared_agent_tools(runtime: &AgentRuntime, task: &str) -> Vec<String> {
    let row = runtime.store.get(task).unwrap().unwrap();
    let model = runtime.pi_model(&runtime.model, None, false);
    runtime
        .task_session(&row, model)
        .await
        .unwrap()
        .active_tools()
        .into_iter()
        .filter(|name| EXTERNAL_AGENT_TOOLS.contains(&name.as_str()))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_tools_follow_settings_and_the_tasks_choice() {
    let harness = Harness::new().await;
    let runtime = harness.runtime().await;
    let task = harness.create_task().await;

    // Chosen for the task, but off in Settings.
    choose_for_task(&runtime, &task, &["codex"]);
    assert!(declared_agent_tools(&runtime, &task).await.is_empty());
    let rows = harness
        .handle
        .list_session_mcp_servers(task.clone())
        .await
        .unwrap();
    assert!(
        rows.iter()
            .all(|row| row.kind != AgentSessionIntegrationKind::ExternalAgent)
    );
    let switch = |provider: &str, enabled| AgentSetSessionMcpServerRequest {
        session_id: task.clone(),
        name: provider.to_string(),
        kind: AgentSessionIntegrationKind::ExternalAgent,
        enabled,
    };
    let error = harness
        .handle
        .set_session_mcp_server_enabled(switch("codex", true))
        .await
        .unwrap_err();
    assert!(
        error.contains("Enable this integration in Settings"),
        "{error}"
    );
    let error = harness
        .handle
        .set_session_mcp_server_enabled(switch("cursor", false))
        .await
        .unwrap_err();
    assert_eq!(error, "Unknown external agent integration");

    // On in Settings and chosen: the model sees the tools from the next run.
    switch_on_in_settings(&harness, &["codex"]);
    assert_eq!(
        declared_agent_tools(&runtime, &task).await,
        EXTERNAL_AGENT_TOOLS
    );
    let skills = crate::agent::config::account_config_dir_path(
        harness.handle.paths(),
        &harness.handle.user_id,
    )
    .unwrap()
    .join("skills");
    // Settings is where the skills follow the switch; the page was not used.
    assert!(!skills.join("handoff").exists());

    // Switched off for the task: gone again, and the row says so.
    let rows = harness
        .handle
        .set_session_mcp_server_enabled(switch("codex", false))
        .await
        .unwrap();
    let codex = rows
        .iter()
        .find(|row| row.kind == AgentSessionIntegrationKind::ExternalAgent)
        .unwrap();
    assert_eq!(codex.name, "codex");
    assert_eq!(codex.display_name, "Codex");
    assert!(!codex.enabled);
    assert!(declared_agent_tools(&runtime, &task).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_task_delegates_in_the_background_and_hears_back() {
    let fixtures = Fixtures::new("approve");
    let harness = Harness::new().await;
    let runtime = harness.runtime().await;
    runtime
        .external_agents
        .set_test_search_path(fixtures.search_path());
    switch_on_in_settings(&harness, &["codex"]);
    let task = harness.create_task().await;
    choose_for_task(&runtime, &task, &["codex"]);

    harness.faux.push_message(vec![faux_tool_call(
        AGENT_START_TOOL,
        json!({"provider": "codex", "prompt": "Work in the background", "background": true}),
    )]);
    harness.faux.push_text("Started it.");
    harness.faux.push_text("The agent is done.");
    let mut run = harness.send(&task, "Delegate this").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    let requests = harness.faux.requests();
    assert!(
        pi_ai::transcript::current_tools(&requests[0].context.messages)
            .iter()
            .any(|tool| tool.name == AGENT_START_TOOL)
    );

    // The result reaches the model as a message of its own, behind the run
    // or in a run of its own.
    eventually(|| harness.faux.requests().len() >= 3).await;
    eventually(|| !runtime.runs.is_running(&task)).await;
    let requests = harness.faux.requests();
    let result = last_user_text(&requests[2]);
    assert!(
        result.starts_with("A background agent has finished."),
        "{result}"
    );
    assert!(result.contains("Done: accept"), "{result}");
    assert!(result.contains("\"agent\":\"external agent codex-1 (codex)\""));

    // The call's row carries what the agent did; the user never sees the
    // message, and a notice says the agent finished.
    let detail = harness.handle.load_session(task.clone()).await.unwrap();
    let row = detail
        .timeline
        .iter()
        .find(|item| item.item_type == "tool")
        .unwrap();
    assert_eq!(row.title.as_deref(), Some("External agent: start"));
    let output = row.output.as_ref().unwrap();
    assert_eq!(
        output["structuredContent"][ACTIVITY_KEY]["status"],
        "completed"
    );
    assert_eq!(
        output["structuredContent"][ACTIVITY_KEY]["fileChanges"][0]["path"],
        "src/lib.rs"
    );
    assert!(
        detail.timeline.iter().any(|item| {
            item.text.as_deref() == Some("External agent codex-1 (Codex) finished.")
        })
    );
    assert!(!detail.timeline.iter().any(|item| {
        item.text
            .as_deref()
            .is_some_and(|text| text.contains("A background agent has finished"))
    }));
    // The agent's progress joined the call's row as it streamed.
    assert!(harness.recorder.events().iter().any(|event| matches!(
        event,
        AgentServiceEvent::TimelineItem { item, .. }
            if item.id == row.id && item.output.as_ref().is_some_and(|output| {
                output["structuredContent"][ACTIVITY_KEY]["agentId"] == "codex-1"
            })
    )));
    assert!(harness.handle.session_subagents(&task).await.is_empty());
    runtime.external_agents.shutdown_all(WAIT).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_working_agent_shows_until_it_stops_and_goes_with_its_task() {
    let fixtures = Fixtures::new("slow");
    let harness = Harness::new().await;
    let runtime = harness.runtime().await;
    let task = harness.create_task().await;
    let call = || ExternalAgentCall {
        session_id: task.clone(),
        working_dir: harness.project.path().to_path_buf(),
        row_id: Some("row-1".to_string()),
        login_path: Some(fixtures.search_path()),
        tool_context: SharedAgentToolContext::new(default_tool_context_spec().unwrap()).snapshot(),
        cancel_token: CancellationToken::new(),
    };
    let started = runtime
        .external_agents
        .start(call(), codex_start("Take your time", true))
        .await;
    assert!(result_text(&started).starts_with("Status: running"));
    let pid = fixtures.fixture_pid().await;

    // A task opened again shows the agent above its composer, and its row
    // as the agent left it.
    let working = harness.handle.session_subagents(&task).await;
    assert_eq!(working.len(), 1);
    assert_eq!(working[0].external.as_ref().unwrap().provider, "codex");
    let detail = harness.handle.load_session(task.clone()).await.unwrap();
    let row = detail
        .timeline
        .iter()
        .find(|item| item.id == "row-1")
        .unwrap();
    assert_eq!(row.status.as_deref(), Some("running"));

    // Stopped from its row: the model hears of it, and its process goes.
    harness.faux.push_text("Noted.");
    harness
        .handle
        .cancel_external_agent(&task, "codex-1")
        .await
        .unwrap();
    wait_for(|| (!process_alive(pid)).then_some(())).await;
    assert!(harness.handle.session_subagents(&task).await.is_empty());
    eventually(|| harness.faux.requests().len() == 1).await;
    assert!(last_user_text(&harness.faux.requests()[0]).contains("\"status\":\"cancelled\""));
    eventually(|| !runtime.runs.is_running(&task)).await;

    // Deleting the task ends its agents, and tells no one.
    fs::remove_file(&fixtures.pid_file).unwrap();
    let resumed = runtime
        .external_agents
        .send(
            call(),
            AgentSendParams {
                provider: "codex".into(),
                agent_id: "codex-1".into(),
                prompt: "Carry on".into(),
                background: true,
                model: None,
                effort: None,
            },
        )
        .await;
    assert!(result_text(&resumed).starts_with("Status: running"));
    let pid = fixtures.fixture_pid().await;
    harness.handle.delete_session(task.clone()).await.unwrap();
    wait_for(|| (!process_alive(pid)).then_some(())).await;
    assert!(harness.handle.session_subagents(&task).await.is_empty());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(harness.faux.requests().len(), 1);
}
