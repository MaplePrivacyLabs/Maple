//! The ACP agents installed on this computer, driven with real prompts on
//! their own accounts. Never run in CI: run by hand, signed in, with
//! `MAPLE_LIVE_ACP=grok,antigravity` and `--ignored --nocapture`. Each agent
//! gets a fresh git folder; nothing outside it is touched. The task starts
//! in read only, where every card is allowed, then moves to full access and
//! auto.

use super::*;

fn live_call(harness: &Harness, session: &str, row: &str) -> ExternalAgentCall {
    ExternalAgentCall {
        login_path: std::env::var("MAPLE_LIVE_PATH")
            .or_else(|_| std::env::var("PATH"))
            .ok(),
        ..harness.call(session, row)
    }
}

fn live_send(provider: &str, prompt: &str, background: bool) -> AgentSendParams {
    AgentSendParams {
        provider: provider.into(),
        agent_id: format!("{provider}-1"),
        prompt: prompt.into(),
        answers: BTreeMap::new(),
        background,
        model: None,
        effort: None,
    }
}

/// Whether the agent's current turn has a command running.
async fn running_a_command(harness: &Harness, provider: &str) -> bool {
    let status = harness
        .registry
        .status(
            &harness.call("live", "status"),
            AgentRefParams {
                provider: provider.into(),
                agent_id: format!("{provider}-1"),
            },
        )
        .await;
    activity_of(&status)["commands"]
        .as_array()
        .is_some_and(|commands| {
            commands
                .iter()
                .any(|command| command["status"] == "running")
        })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "starts the installed vendor CLIs on their own accounts; run by hand"]
async fn live_acp_agents() {
    let Ok(providers) = std::env::var("MAPLE_LIVE_ACP") else {
        return;
    };
    for provider in providers.split(',').map(str::trim) {
        let harness = Harness::new("live");
        harness.gate.mode.set(AgentPermissionMode::ReadOnly);
        let cards = answer_cards(
            &harness,
            r#"{"answers":{"permission":{"answers":["Allow once"]}}}"#,
        );
        let project = &harness.project;
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(project)
            .status()
            .unwrap();
        println!("\n===== {provider} in {}", project.display());

        // A turn that edits and runs a command, at low effort.
        let result = harness
            .registry
            .start(
                live_call(&harness, "live", "row-1"),
                AgentStartParams {
                    provider: provider.into(),
                    prompt: "Create a file named maple-live.txt containing exactly the word ready, then run `cat maple-live.txt` in the shell and tell me what it printed. Keep your reply to one line.".into(),
                    background: false,
                    model: None,
                    effort: Some("low".into()),
                    cwd: None,
                },
            )
            .await;
        println!("--- edit and command\n{}", result_text(&result));
        println!("--- activity {}", activity_of(&result));
        assert!(
            result_text(&result).starts_with("Status: completed"),
            "{}",
            result_text(&result)
        );
        let written = fs::read_to_string(project.join("maple-live.txt")).unwrap_or_default();
        assert_eq!(written.trim(), "ready");
        // Read only asks before the agent edits.
        let asked: Vec<String> = cards
            .lock()
            .unwrap()
            .iter()
            .filter_map(|card| card.permission.as_ref())
            .map(|prompt| format!("{}: {}", prompt.tool, prompt.action))
            .collect();
        println!("--- cards in read only {asked:?}");
        assert!(!asked.is_empty());

        // Full access: Grok starts again with its own, on the same session.
        harness.gate.mode.set(AgentPermissionMode::FullAccess);
        let shown = cards.lock().unwrap().len();

        // A question: the call returns waiting, or the agent asks in words.
        let asked = harness
            .registry
            .send(
                live_call(&harness, "live", "row-2"),
                live_send(
                    provider,
                    "Use your tool for asking the user a question to ask me whether I prefer red or blue, offering those two choices. Wait for my answer, then write it alone into color.txt.",
                    false,
                ),
            )
            .await;
        let text = result_text(&asked);
        println!("--- question\n{text}");
        let answer = if text.starts_with("Status: waiting") {
            "blue"
        } else {
            "Blue. Write it into color.txt now."
        };
        let answered = harness
            .registry
            .send(
                live_call(&harness, "live", "row-3"),
                live_send(provider, answer, false),
            )
            .await;
        println!("--- answered\n{}", result_text(&answered));
        let color = fs::read_to_string(project.join("color.txt")).unwrap_or_default();
        println!("--- color.txt: {color:?}");
        assert!(color.to_ascii_lowercase().contains("blue"), "{color:?}");
        assert_eq!(cards.lock().unwrap().len(), shown, "full access asked");

        // Stop a long command, then resume the session in a new process.
        let background = harness
            .registry
            .send(
                live_call(&harness, "live", "row-4"),
                live_send(
                    provider,
                    "Run the shell command `sleep 45 && echo slept` and tell me when it finishes.",
                    true,
                ),
            )
            .await;
        println!("--- background\n{}", result_text(&background));
        let deadline = Instant::now() + Duration::from_secs(90);
        while !running_a_command(&harness, provider).await {
            assert!(Instant::now() < deadline, "the command never started");
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let stopped = harness
            .registry
            .cancel_tool(
                &harness.call("live", "row-5"),
                AgentRefParams {
                    provider: provider.into(),
                    agent_id: format!("{provider}-1"),
                },
            )
            .await;
        println!("--- stopped\n{}", result_text(&stopped));
        assert!(result_text(&stopped).starts_with("Status: cancelled"));
        harness.gate.mode.set(AgentPermissionMode::Auto);
        let resumed = harness
            .registry
            .send(
                live_call(&harness, "live", "row-6"),
                live_send(
                    provider,
                    "What word did maple-live.txt contain? Answer with the word only.",
                    false,
                ),
            )
            .await;
        println!("--- resumed\n{}", result_text(&resumed));
        assert!(
            result_text(&resumed).to_ascii_lowercase().contains("ready"),
            "{}",
            result_text(&resumed)
        );
        harness.registry.shutdown_all(Duration::from_secs(10)).await;
    }
}
