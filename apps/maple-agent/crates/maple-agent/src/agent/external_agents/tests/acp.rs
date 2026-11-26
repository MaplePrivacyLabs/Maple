//! ACP agents through the registry, against the fake ACP agent: the
//! handshake Maple offers, options, a turn's activity, approvals, questions
//! to the caller, cancel and resume, and a signed-out agent.

use super::*;

fn acp_start(provider: &str, prompt: &str) -> AgentStartParams {
    AgentStartParams {
        provider: provider.into(),
        prompt: prompt.into(),
        background: false,
        model: None,
        effort: None,
        cwd: None,
    }
}

fn acp_send(provider: &str, agent_id: &str, prompt: &str) -> AgentSendParams {
    AgentSendParams {
        provider: provider.into(),
        agent_id: agent_id.into(),
        prompt: prompt.into(),
        answers: BTreeMap::new(),
        background: false,
        model: None,
        effort: None,
    }
}

/// The lines Maple sent the fake agent that call `method`.
fn sent(log: &str, method: &str) -> Vec<Value> {
    log.lines()
        .filter_map(|line| line.strip_prefix("<- "))
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|message| message["method"] == method)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_acp_agent_works_a_turn_with_the_chosen_model_and_effort() {
    let harness = Harness::new("acp");
    harness.install_fixture("grok", "acp");
    let result = harness
        .registry
        .start(
            harness.call("acp-1", "row-1"),
            AgentStartParams {
                model: Some("fixture-fast".into()),
                effort: Some("low".into()),
                ..acp_start("grok", "Fix the parser")
            },
        )
        .await;
    let text = result_text(&result);
    assert!(
        text.starts_with(
            "Status: completed\nProvider: grok\nAgent ID: grok-1\nThread ID: fixture-session\n"
        ),
        "{text}"
    );
    assert!(text.contains("Files changed (1): src/lib.rs"), "{text}");
    assert!(text.contains("Commands run: 1 (0 failed)"), "{text}");
    assert!(
        text.contains("Working on it.\n\nDone: Fix the parser (fixture-fast, low)"),
        "{text}"
    );
    let activity = activity_of(&result);
    assert_eq!(activity["commands"][0]["command"], "cargo test");
    assert_eq!(activity["commands"][0]["exitCode"], 0);

    let log = harness.log();
    // Started as the catalog says, with telemetry and updates off.
    assert!(log.contains("args agent stdio"), "{log}");
    assert!(log.contains("env GROK_TELEMETRY_ENABLED=false"), "{log}");
    assert!(
        log.contains("env GROK_TELEMETRY_TRACE_UPLOAD=false"),
        "{log}"
    );
    assert!(log.contains("env GROK_DISABLE_AUTOUPDATER=1"), "{log}");
    let initialize = &sent(&log, "initialize")[0];
    assert_eq!(initialize["jsonrpc"], "2.0");
    assert_eq!(initialize["params"]["protocolVersion"], 1);
    assert_eq!(initialize["params"]["clientInfo"]["name"], "Maple Work");
    assert_eq!(
        initialize["params"]["clientCapabilities"],
        json!({"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false, "elicitation": {"form": {}}})
    );
    assert_eq!(initialize["params"]["_meta"]["clientType"], "extension");
    assert!(sent(&log, "authenticate").is_empty());
    assert_eq!(
        sent(&log, "session/new")[0]["params"]["mcpServers"],
        json!([])
    );
    // The model first, then the effort.
    let set = sent(&log, "session/set_config_option");
    assert_eq!(set[0]["params"]["configId"], "model");
    assert_eq!(set[0]["params"]["value"], "fixture-fast");
    assert_eq!(set[1]["params"]["configId"], "reasoning_effort");
    assert_eq!(set[1]["params"]["value"], "low");
    assert_eq!(
        sent(&log, "session/prompt")[0]["params"]["prompt"],
        json!([{"type": "text", "text": "Fix the parser"}])
    );

    // A value the agent did not offer is refused before any prompt.
    let refused = harness
        .registry
        .send(
            harness.call("acp-1", "row-2"),
            AgentSendParams {
                model: Some("fixture-huge".into()),
                ..acp_send("grok", "grok-1", "Again")
            },
        )
        .await;
    assert_eq!(
        result_text(&refused),
        "Grok Build has no model 'fixture-huge'. It offers: fixture-default, fixture-fast."
    );
    assert_eq!(sent(&harness.log(), "session/prompt").len(), 1);
    harness.registry.shutdown_all(Duration::from_secs(5)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_signed_out_acp_agent_says_how_to_sign_in() {
    let harness = Harness::new("acp-signed-out");
    harness.install_fixture("agy", "acp-signed-out");
    let result = harness
        .registry
        .start(
            harness.call("acp-2", "row-1"),
            acp_start("antigravity", "Hello"),
        )
        .await;
    assert_eq!(
        result_text(&result),
        catalog::entry("antigravity").unwrap().text.sign_in_hint
    );
    let log = harness.log();
    assert!(log.contains("args acp --disable-telemetry"), "{log}");
    assert!(
        log.contains("env AGY_CLI_DISABLE_AUTO_UPDATE=true"),
        "{log}"
    );
    harness.registry.shutdown_all(Duration::from_secs(5)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_approvals_allow_once_and_never_for_longer() {
    let harness = Harness::new("acp-approve");
    harness.install_fixture("grok", "acp-approve");
    let result = harness
        .registry
        .start(harness.call("acp-3", "row-1"), acp_start("grok", "Edit it"))
        .await;
    let text = result_text(&result);
    assert!(text.contains("Chose: allow-once"), "{text}");
    harness.registry.shutdown_all(Duration::from_secs(5)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_acp_agents_form_question_goes_to_the_caller() {
    let harness = Harness::new("acp-question");
    harness.install_fixture("agy", "acp-question");
    let asked = harness
        .registry
        .start(
            harness.call("acp-4", "row-1"),
            acp_start("antigravity", "Ask me"),
        )
        .await;
    let text = result_text(&asked);
    assert!(text.starts_with("Status: waiting"), "{text}");
    assert!(
        text.contains("- q1: Do you prefer red or blue? Options: Red | Blue."),
        "{text}"
    );
    let answered = harness
        .registry
        .send(
            harness.call("acp-4", "row-2"),
            acp_send("antigravity", "antigravity-1", "blue"),
        )
        .await;
    let text = result_text(&answered);
    assert!(
        text.contains(r#"Answer: {"action":"accept","content":{"q1":"2"}}"#),
        "{text}"
    );
    assert!(text.starts_with("Status: completed"), "{text}");
    harness.registry.shutdown_all(Duration::from_secs(5)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_acp_turn_resumes_its_session_in_a_new_process() {
    let harness = Harness::new("acp-slow");
    harness.install_fixture("grok", "acp-slow");
    let registry = Arc::clone(&harness.registry);
    let call = harness.call("acp-5", "row-1");
    let turn =
        tokio::spawn(async move { registry.start(call, acp_start("grok", "Take long")).await });
    wait_for(|| harness.log().contains("session/prompt").then_some(())).await;
    let first = harness.fixture_pid().await;
    let cancelled = harness
        .registry
        .cancel_tool(
            &harness.call("acp-5", "row-2"),
            AgentRefParams {
                provider: "grok".into(),
                agent_id: "grok-1".into(),
            },
        )
        .await;
    assert!(result_text(&cancelled).starts_with("Status: cancelled"));
    // The agent never closed the command it was running.
    assert_eq!(activity_of(&cancelled)["commands"][0]["status"], "failed");
    assert!(
        result_text(&tokio::time::timeout(WAIT, turn).await.unwrap().unwrap())
            .starts_with("Status: cancelled")
    );
    let cancel = &sent(&harness.log(), "session/cancel")[0];
    assert_eq!(cancel["params"]["sessionId"], "fixture-session");
    assert_eq!(cancel["params"]["_meta"]["cancelTrigger"], "ctrl_c");
    wait_for(|| (!process_alive(first)).then_some(())).await;

    // The next send starts a new process on the same session.
    harness.remove_pid_file();
    harness.install_fixture("grok", "acp");
    let resumed = harness
        .registry
        .send(
            harness.call("acp-5", "row-3"),
            acp_send("grok", "grok-1", "Carry on"),
        )
        .await;
    let text = result_text(&resumed);
    assert!(text.contains("Done: Carry on"), "{text}");
    assert_eq!(
        sent(&harness.log(), "session/resume")[0]["params"]["sessionId"],
        "fixture-session"
    );
    harness.registry.shutdown_all(Duration::from_secs(5)).await;
}
