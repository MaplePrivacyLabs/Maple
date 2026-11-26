//! ACP agents through the registry, against the fake ACP agent: the
//! handshake Maple offers, options, a turn's activity, the task's mode and
//! approvals, questions to the caller, cancel and resume, and a signed-out
//! agent.

use super::*;
use crate::agent::AgentPermissionReason;

const READ_ONLY: AgentPermissionMode = AgentPermissionMode::ReadOnly;
const AUTO: AgentPermissionMode = AgentPermissionMode::Auto;
const FULL_ACCESS: AgentPermissionMode = AgentPermissionMode::FullAccess;

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

/// The arguments of each process start the fake agent logged.
fn starts(log: &str) -> Vec<&str> {
    log.lines()
        .filter_map(|line| line.strip_prefix("args "))
        .collect()
}

fn cards_shown(harness: &Harness) -> usize {
    harness
        .sink
        .events()
        .iter()
        .filter(|event| matches!(event, AgentServiceEvent::Question { .. }))
        .count()
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
    // Started as the catalog says, in the agent's mode for the task's, with
    // telemetry and updates off.
    assert_eq!(starts(&log), ["--always-approve agent stdio"]);
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
    // No turn started: the agent's last turn stands as it was.
    let status = harness
        .registry
        .status(
            &harness.call("acp-1", "status"),
            AgentRefParams {
                provider: "grok".into(),
                agent_id: "grok-1".into(),
            },
        )
        .await;
    let activity = activity_of(&status);
    assert_eq!(activity["turns"], 1);
    assert!(
        activity["text"]
            .as_str()
            .unwrap()
            .contains("Done: Fix the parser"),
        "{activity}"
    );
    harness.registry.shutdown_all(Duration::from_secs(5)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_after_a_turn_ends_is_a_message_of_its_own() {
    let harness = Harness::new("acp-wakes");
    harness.install_fixture("grok", "acp-wakes");
    let result = harness
        .registry
        .start(
            harness.call("acp-9", "row-1"),
            acp_start("grok", "Start it in the background"),
        )
        .await;
    assert!(
        result_text(&result).starts_with("Status: completed"),
        "{}",
        result_text(&result)
    );
    // The agent speaks again once the turn is over, as Grok does when a
    // background command ends.
    let deadline = Instant::now() + Duration::from_secs(10);
    let text = loop {
        let status = harness
            .registry
            .status(
                &harness.call("acp-9", "status"),
                AgentRefParams {
                    provider: "grok".into(),
                    agent_id: "grok-1".into(),
                },
            )
            .await;
        let text = activity_of(&status)["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if text.contains("The background command finished.") || Instant::now() > deadline {
            break text;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(
        text.ends_with(")\n\nThe background command finished."),
        "{text}"
    );
    harness.registry.shutdown_all(Duration::from_secs(5)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_shut_down_mid_turn_ends_its_own_command_first() {
    let harness = Harness::new("acp-shutdown");
    harness.install_fixture("grok", "acp-slow");
    harness
        .registry
        .start(
            harness.call("acp-10", "row-1"),
            AgentStartParams {
                background: true,
                ..acp_start("grok", "Run a long command")
            },
        )
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while sent(&harness.log(), "session/prompt").is_empty() {
        assert!(Instant::now() < deadline, "{}", harness.log());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // As when Maple stops or the task is deleted.
    harness.registry.shutdown_session("acp-10").await;
    let log = harness.log();
    assert!(log.contains("ended its own command"), "{log}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_replayed_after_a_load_reply_is_not_news() {
    let harness = Harness::new("acp-load-replay");
    harness.install_fixture("grok", "acp-load-replay");
    let first = harness
        .registry
        .start(harness.call("acp-13", "row-1"), acp_start("grok", "First"))
        .await;
    assert!(
        result_text(&first).contains("Done: First"),
        "{}",
        result_text(&first)
    );
    // Grok takes its mode at launch: a new mode reopens the session, here
    // with `session/load`.
    harness.gate.mode.set(READ_ONLY);
    // Maple sets the agent's options before it prompts; that round trip is
    // what puts late history ahead of the prompt.
    let second = harness
        .registry
        .send(
            harness.call("acp-13", "row-2"),
            AgentSendParams {
                model: Some("fixture-fast".into()),
                ..acp_send("grok", "grok-1", "Second")
            },
        )
        .await;
    let text = result_text(&second);
    assert!(text.contains("Done: Second (fixture-fast"), "{text}");
    assert!(!text.contains("Earlier reply."), "{text}");
    assert_eq!(sent(&harness.log(), "session/load").len(), 1);
    harness.registry.shutdown_all(Duration::from_secs(5)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_prompt_reaches_the_agent_before_a_stop_that_follows_it() {
    let harness = Harness::new("acp-prompt-then-stop");
    harness.install_fixture("grok", "acp-slow");
    harness
        .registry
        .start(
            harness.call("acp-14", "row-1"),
            AgentStartParams {
                background: true,
                ..acp_start("grok", "Long job")
            },
        )
        .await
        .unwrap();
    // Stop at once, with nothing in between.
    let stopped = harness
        .registry
        .cancel_tool(
            &harness.call("acp-14", "row-2"),
            AgentRefParams {
                provider: "grok".into(),
                agent_id: "grok-1".into(),
            },
        )
        .await;
    assert!(
        result_text(&stopped).starts_with("Status: cancelled"),
        "{}\n{}",
        result_text(&stopped),
        harness.log()
    );
    let log = harness.log();
    let position = |method: &str| {
        log.lines()
            .position(|line| line.starts_with("<- ") && line.contains(&format!("\"{method}\"")))
    };
    let prompt = position("session/prompt").expect("no prompt");
    let cancel = position("session/cancel").expect("no cancel");
    assert!(prompt < cancel, "{log}");
    harness.registry.shutdown_all(Duration::from_secs(5)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_model_starts_no_turn_and_leaves_nothing_behind() {
    let harness = Harness::new("acp-refused-model");
    harness.install_fixture("grok", "acp");
    let refused = harness
        .registry
        .start(
            harness.call("acp-15", "row-1"),
            AgentStartParams {
                model: Some("fixture-huge".into()),
                ..acp_start("grok", "Fix it")
            },
        )
        .await;
    assert_eq!(
        result_text(&refused),
        "Grok Build has no model 'fixture-huge'. It offers: fixture-default, fixture-fast."
    );
    // The agent keeps no place in the task, and its process is gone.
    let pid = harness.fixture_pid().await;
    wait_for(|| (!process_alive(pid)).then_some(())).await;
    let status = harness
        .registry
        .status(
            &harness.call("acp-15", "status"),
            AgentRefParams {
                provider: "grok".into(),
                agent_id: "grok-1".into(),
            },
        )
        .await;
    assert!(
        result_text(&status).starts_with("No external agent"),
        "{}",
        result_text(&status)
    );
    assert!(sent(&harness.log(), "session/prompt").is_empty());
    harness.registry.shutdown_all(Duration::from_secs(5)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shutdown_that_runs_out_of_time_still_kills_the_agent() {
    let harness = Harness::new("acp-shutdown-timeout");
    harness.install_fixture("grok", "acp-slow");
    harness
        .registry
        .start(
            harness.call("acp-16", "row-1"),
            AgentStartParams {
                background: true,
                ..acp_start("grok", "Long job")
            },
        )
        .await
        .unwrap();
    let pid = harness.fixture_pid().await;
    // Too little time for the agent to end its turn on its own.
    harness
        .registry
        .shutdown_all(Duration::from_millis(1))
        .await;
    wait_for(|| (!process_alive(pid)).then_some(())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_that_was_not_allowed_does_not_stay_running() {
    let harness = Harness::new("acp-approve-command");
    harness.install_fixture("grok", "acp-approve-command");
    harness.gate.mode.set(READ_ONLY);
    answer_cards(
        &harness,
        r#"{"answers":{"permission":{"answers":["Deny"]}}}"#,
    );
    let result = harness
        .registry
        .start(
            harness.call("acp-17", "row-1"),
            acp_start("grok", "Clean up"),
        )
        .await;
    // The agent ends its turn without saying the command failed.
    assert!(result_text(&result).contains("Chose: reject-once"));
    let activity = activity_of(&result);
    assert_eq!(activity["commands"][0]["command"], "rm -rf build");
    assert_eq!(activity["commands"][0]["status"], "failed");
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
    // Full access allows it once, without a card.
    assert!(text.contains("Chose: allow-once"), "{text}");
    assert_eq!(cards_shown(&harness), 0);
    harness.registry.shutdown_all(Duration::from_secs(5)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_read_only_the_user_decides_what_an_acp_agent_asks_to_do() {
    let harness = Harness::new("acp-approve-ask");
    harness.install_fixture("grok", "acp-approve");
    harness.gate.mode.set(READ_ONLY);
    let cards = answer_cards(
        &harness,
        r#"{"answers":{"permission":{"answers":["Allow once"]}}}"#,
    );
    let result = harness
        .registry
        .start(harness.call("acp-6", "row-1"), acp_start("grok", "Edit it"))
        .await;
    let text = result_text(&result);
    assert!(text.contains("Chose: allow-once"), "{text}");
    assert!(!text.contains("Not allowed"), "{text}");
    {
        let cards = cards.lock().unwrap();
        assert_eq!(cards.len(), 1);
        let prompt = cards[0].permission.as_ref().unwrap();
        assert_eq!(prompt.tool, "grok edit");
        assert_eq!(prompt.action, "src/lib.rs");
        // There is no classifier here; one that cannot answer never allows.
        assert_eq!(prompt.reason, AgentPermissionReason::Unavailable);
    }
    // The agents panel says what the agent waits for, then that it works.
    let panel: Vec<String> = harness
        .sink
        .events()
        .into_iter()
        .filter_map(|event| match event {
            AgentServiceEvent::Run {
                event: AgentRunEvent::SubagentActivity { tool, .. },
                ..
            } => Some(tool),
            _ => None,
        })
        .collect();
    let waiting = panel
        .iter()
        .position(|label| label == "Waiting for you to approve src/lib.rs");
    assert!(
        waiting.is_some_and(|at| panel[at + 1..].iter().any(|label| label == "Working")),
        "{panel:?}"
    );
    harness.registry.shutdown_all(Duration::from_secs(5)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_offering_several_ways_to_allow_still_goes_to_the_gate() {
    let harness = Harness::new("acp-approve-repeated");
    harness.install_fixture("grok", "acp-approve-repeated");
    harness.gate.mode.set(READ_ONLY);
    let cards = answer_cards(
        &harness,
        r#"{"answers":{"permission":{"answers":["Allow once"]}}}"#,
    );
    let result = harness
        .registry
        .start(
            harness.call("acp-11", "row-1"),
            acp_start("grok", "Edit it"),
        )
        .await;
    let text = result_text(&result);
    // The user's card decided, not the calling model, and only once.
    assert!(text.starts_with("Status: completed"), "{text}");
    assert!(text.contains("Chose: allow-once"), "{text}");
    assert_eq!(cards.lock().unwrap().len(), 1);
    harness.registry.shutdown_all(Duration::from_secs(5)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_asking_to_change_its_own_mode_is_refused() {
    let harness = Harness::new("acp-switch-mode");
    harness.install_fixture("grok", "acp-switch-mode");
    // Even in full access, where its other requests are allowed.
    let result = harness
        .registry
        .start(harness.call("acp-12", "row-1"), acp_start("grok", "Go"))
        .await;
    let text = result_text(&result);
    assert!(text.contains("Chose: reject-once"), "{text}");
    assert!(
        text.contains("Not allowed:\n- Changing its own mode"),
        "{text}"
    );
    assert_eq!(cards_shown(&harness), 0);
    harness.registry.shutdown_all(Duration::from_secs(5)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_declined_request_is_refused_once_and_the_caller_hears_why() {
    let harness = Harness::new("acp-approve-deny");
    harness.install_fixture("grok", "acp-approve");
    harness.gate.mode.set(AUTO);
    answer_cards(
        &harness,
        r#"{"answers":{"permission":{"answers":["Deny","Additional note: not now"]}}}"#,
    );
    let result = harness
        .registry
        .start(harness.call("acp-7", "row-1"), acp_start("grok", "Edit it"))
        .await;
    let text = result_text(&result);
    assert!(text.contains("Chose: reject-once"), "{text}");
    assert!(
        text.contains("Not allowed:\n- src/lib.rs: the user declined it and said: not now\n"),
        "{text}"
    );
    harness.registry.shutdown_all(Duration::from_secs(5)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_nobody_to_ask_an_acp_agents_request_is_refused() {
    let harness = Harness::new("acp-approve-nobody");
    harness.install_fixture("grok", "acp-approve");
    harness.gate.mode.set(READ_ONLY);
    harness.gate.questions.manage_availability();
    let result = harness
        .registry
        .start(harness.call("acp-8", "row-1"), acp_start("grok", "Edit it"))
        .await;
    let text = result_text(&result);
    assert!(text.contains("Chose: reject-once"), "{text}");
    assert!(
        text.contains(
            "- src/lib.rs: it needed the user's approval and no connected interface could ask."
        ),
        "{text}"
    );
    harness.registry.shutdown_all(Duration::from_secs(5)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grok_starts_in_the_tasks_mode_and_restarts_when_it_changes() {
    let harness = Harness::new("acp-grok-modes");
    harness.install_fixture("grok", "acp");
    harness.gate.mode.set(READ_ONLY);
    let first = harness
        .registry
        .start(harness.call("acp-9", "row-1"), acp_start("grok", "One"))
        .await;
    assert!(result_text(&first).contains("Done: One"));
    harness.gate.mode.set(AUTO);
    for (row, prompt) in [("row-2", "Two"), ("row-3", "Three")] {
        let next = harness
            .registry
            .send(
                harness.call("acp-9", row),
                acp_send("grok", "grok-1", prompt),
            )
            .await;
        assert!(result_text(&next).contains(&format!("Done: {prompt}")));
    }
    let log = harness.log();
    // Grok takes its mode only at launch: the change starts a new process
    // on the same session, and a turn in the same mode keeps it.
    assert_eq!(
        starts(&log),
        [
            "--permission-mode default agent stdio",
            "--permission-mode auto agent stdio"
        ]
    );
    assert_eq!(
        sent(&log, "session/resume")[0]["params"]["sessionId"],
        "fixture-session"
    );
    assert!(
        sent(&log, "session/set_config_option")
            .iter()
            .all(|set| set["params"]["configId"] != "mode")
    );
    harness.registry.shutdown_all(Duration::from_secs(5)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn antigravity_sets_its_mode_option_from_the_tasks() {
    let harness = Harness::new("acp-agy-modes");
    harness.install_fixture("agy", "acp");
    for (row, mode) in [
        ("row-1", READ_ONLY),
        ("row-2", FULL_ACCESS),
        ("row-3", READ_ONLY),
    ] {
        harness.gate.mode.set(mode);
        let result = if row == "row-1" {
            harness
                .registry
                .start(harness.call("acp-10", row), acp_start("antigravity", row))
                .await
        } else {
            harness
                .registry
                .send(
                    harness.call("acp-10", row),
                    acp_send("antigravity", "antigravity-1", row),
                )
                .await
        };
        assert!(result_text(&result).contains(&format!("Done: {row}")));
    }
    let log = harness.log();
    assert_eq!(starts(&log).len(), 1, "{log}");
    // The mode is set before every prompt, even when it reads as set
    // already: the agent may have changed it on its own.
    let modes: Vec<Value> = sent(&log, "session/set_config_option")
        .into_iter()
        .filter(|set| set["params"]["configId"] == "mode")
        .map(|set| set["params"]["value"].clone())
        .collect();
    assert_eq!(modes, [json!("default"), json!("auto"), json!("default")]);
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
