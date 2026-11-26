//! The ACP agents installed on this computer, driven with real prompts on
//! their own accounts. Never run in CI: run by hand, signed in, with
//! `MAPLE_LIVE_ACP=grok,antigravity` and `--ignored --nocapture`. Each test
//! gets a fresh git folder; nothing outside it is touched, except that the
//! stop test ends any leftover `sleep` it started.

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

/// The agents named in `MAPLE_LIVE_ACP`; none when it is unset.
fn live_providers() -> Vec<String> {
    std::env::var("MAPLE_LIVE_ACP")
        .map(|providers| {
            providers
                .split(',')
                .map(|provider| provider.trim().to_string())
                .filter(|provider| !provider.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// A harness in `mode` over a fresh git folder, with every card answered
/// with `answer`.
fn live_harness(
    mode: AgentPermissionMode,
    answer: &'static str,
) -> (Harness, Arc<StdMutex<Vec<AgentQuestion>>>) {
    let harness = Harness::new("live");
    harness.gate.mode.set(mode);
    let cards = answer_cards(&harness, answer);
    std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&harness.project)
        .status()
        .unwrap();
    (harness, cards)
}

fn live_start(provider: &str, prompt: &str, background: bool) -> AgentStartParams {
    AgentStartParams {
        provider: provider.into(),
        prompt: prompt.into(),
        background,
        model: None,
        effort: Some("low".into()),
        cwd: None,
    }
}

/// The processes on this computer whose command line contains `marker`.
fn processes_with(marker: &str) -> Vec<String> {
    let output = std::process::Command::new("ps")
        .args(["-axo", "pid=,command="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains(marker) && !line.contains("ps -axo"))
        .map(|line| line.trim().to_string())
        .collect()
}

/// Waits up to `limit` for `done`, checking twice a second.
async fn eventually(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    done()
}

fn agent_ref(provider: &str) -> AgentRefParams {
    AgentRefParams {
        provider: provider.into(),
        agent_id: format!("{provider}-1"),
    }
}

/// Stop ends the agent's command too, not only its turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "starts the installed vendor CLIs on their own accounts; run by hand"]
async fn live_acp_stop_ends_the_running_command() {
    for (index, provider) in live_providers().iter().enumerate() {
        let (harness, _cards) = live_harness(AgentPermissionMode::FullAccess, "{}");
        let marker = format!("sleep 30{index}7");
        println!("\n===== {provider}: stop during `{marker}`");
        let started = harness
            .registry
            .start(
                live_call(&harness, "live", "row-1"),
                live_start(
                    provider,
                    &format!("Run the shell command `{marker} && echo slept` in the foreground and wait for it. Do not run it in the background."),
                    true,
                ),
            )
            .await;
        println!("--- started\n{}", result_text(&started));
        assert!(
            eventually(Duration::from_secs(90), || !processes_with(&marker)
                .is_empty())
            .await,
            "the command never started"
        );
        println!("--- running {:?}", processes_with(&marker));
        let stopped = harness
            .registry
            .cancel_tool(&harness.call("live", "row-2"), agent_ref(provider))
            .await;
        println!("--- stopped\n{}", result_text(&stopped));
        assert!(result_text(&stopped).starts_with("Status: cancelled"));
        let ended = eventually(Duration::from_secs(10), || {
            processes_with(&marker).is_empty()
        })
        .await;
        println!("--- left after stop {:?}", processes_with(&marker));
        harness.registry.shutdown_all(Duration::from_secs(10)).await;
        let left = processes_with(&marker);
        println!("--- left after shutdown {left:?}");
        for line in &left {
            if let Some(pid) = line.split_whitespace().next() {
                let _ = std::process::Command::new("kill").arg(pid).status();
            }
        }
        assert!(
            ended,
            "{provider} left `{marker}` running after Stop: {left:?}"
        );
    }
}

/// What the caller sees when the agent leaves a command running in the
/// background and ends its turn. Paseo does nothing special here; this
/// records what reaches Maple.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "starts the installed vendor CLIs on their own accounts; run by hand"]
async fn live_acp_background_command() {
    for provider in live_providers() {
        let (harness, cards) = live_harness(AgentPermissionMode::FullAccess, "{}");
        let project = harness.project.clone();
        println!("\n===== {provider}: a background command");
        let begun = Instant::now();
        let result = harness
            .registry
            .start(
                live_call(&harness, "live", "row-1"),
                live_start(
                    &provider,
                    "Start the shell command `sleep 23 && echo finished > bg.txt` as a background task, so that it keeps running after you reply. Do not wait for it. Reply with the single word started.",
                    false,
                ),
            )
            .await;
        println!(
            "--- returned after {:?}\n{}",
            begun.elapsed(),
            result_text(&result)
        );
        println!("--- activity {}", activity_of(&result));
        let finished =
            eventually(Duration::from_secs(60), || project.join("bg.txt").exists()).await;
        // Give a turn the agent starts on its own time to arrive.
        tokio::time::sleep(Duration::from_secs(15)).await;
        let later = harness
            .registry
            .status(&harness.call("live", "status"), agent_ref(&provider))
            .await;
        println!("--- status later\n{}", result_text(&later));
        println!("--- activity later {}", activity_of(&later));
        println!("--- cards {}", cards.lock().unwrap().len());
        println!("--- bg.txt written: {finished}");
        harness.registry.shutdown_all(Duration::from_secs(10)).await;
        assert!(result_text(&result).starts_with("Status: completed"));
    }
}

/// A denial with a note in read only: the action does not happen, and the
/// caller hears what was refused and the note.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "starts the installed vendor CLIs on their own accounts; run by hand"]
async fn live_acp_denied_edit() {
    for provider in live_providers() {
        let (harness, cards) = live_harness(
            AgentPermissionMode::ReadOnly,
            r#"{"answers":{"permission":{"answers":["Deny","Additional note: not in this folder"]}}}"#,
        );
        println!("\n===== {provider}: a denied edit");
        let result = harness
            .registry
            .start(
                live_call(&harness, "live", "row-1"),
                live_start(
                    &provider,
                    "Create a file named denied.txt containing hello. If you are not allowed to, say so in one line and stop.",
                    false,
                ),
            )
            .await;
        let text = result_text(&result);
        println!("--- result\n{text}");
        println!("--- activity {}", activity_of(&result));
        println!("--- cards {}", cards.lock().unwrap().len());
        harness.registry.shutdown_all(Duration::from_secs(10)).await;
        assert!(!harness.project.join("denied.txt").exists());
        assert!(text.contains("Not allowed:"), "{text}");
        assert!(text.contains("not in this folder"), "{text}");
    }
}

/// Every agent named, at once, in one task; then an unknown model is
/// refused with the agent's own list before any prompt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "starts the installed vendor CLIs on their own accounts; run by hand"]
async fn live_acp_agents_at_once() {
    let providers = live_providers();
    let (harness, _cards) = live_harness(AgentPermissionMode::FullAccess, "{}");
    for (index, provider) in providers.iter().enumerate() {
        let started = harness
            .registry
            .start(
                live_call(&harness, "live", &format!("row-{index}")),
                live_start(
                    provider,
                    &format!("Create a file named {provider}.txt containing exactly the word {provider}. Reply with one line."),
                    true,
                ),
            )
            .await;
        println!("--- {provider} started\n{}", result_text(&started));
    }
    let project = harness.project.clone();
    let all = eventually(Duration::from_secs(180), || {
        providers
            .iter()
            .all(|provider| project.join(format!("{provider}.txt")).exists())
    })
    .await;
    for provider in &providers {
        let status = harness
            .registry
            .status(&harness.call("live", "status"), agent_ref(provider))
            .await;
        println!("--- {provider}\n{}", result_text(&status));
    }
    for (index, provider) in providers.iter().enumerate() {
        let unknown = harness
            .registry
            .start(
                live_call(&harness, "live", &format!("bad-{index}")),
                AgentStartParams {
                    model: Some("no-such-model".into()),
                    ..live_start(provider, "Say hi.", false)
                },
            )
            .await;
        println!(
            "--- {provider} with an unknown model\n{}",
            result_text(&unknown)
        );
        assert!(
            result_text(&unknown).contains("no-such-model"),
            "{}",
            result_text(&unknown)
        );
    }
    harness.registry.shutdown_all(Duration::from_secs(10)).await;
    assert!(all, "not every agent wrote its file");
}
