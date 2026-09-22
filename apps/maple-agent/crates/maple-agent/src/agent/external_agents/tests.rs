//! Driver tests against fake Codex and Claude Code CLIs.
//!
//! Each fixture is this test binary re-executed as an ignored test. Shell
//! shims on a private PATH forward to them, so the driver
//! resolves and spawns it exactly as it would the real CLI. Unix only until
//! a `.cmd` shim exists for Windows.

#![cfg(unix)]

use super::*;
use crate::agent::tool_context::default_tool_context_spec;
use crate::agent::{AgentEventSink, AgentPathLayout, MapleAgentHostResources};
use std::io::{BufRead, Write};
use std::os::fd::FromRawFd;
use std::os::unix::fs::PermissionsExt;

mod claude_fixture;

const FIXTURE_MARKER: &str = "MAPLE_FAKE_AGENT";
const FIXTURE_ARGS: &str = "MAPLE_FAKE_AGENT_ARGS";
const FIXTURE_MODE: &str = "MAPLE_FAKE_AGENT_MODE";
const FIXTURE_PID_FILE: &str = "MAPLE_FAKE_AGENT_PID_FILE";
const FIXTURE_LOG: &str = "MAPLE_FAKE_AGENT_LOG";
const WAIT: Duration = Duration::from_secs(20);

#[derive(Default)]
struct RecordingSink {
    events: StdMutex<Vec<AgentServiceEvent>>,
}

impl AgentEventSink for RecordingSink {
    fn emit(&self, event: &AgentServiceEvent) {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event.clone());
    }
}

impl RecordingSink {
    fn events(&self) -> Vec<AgentServiceEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

struct Harness {
    _temp: tempfile::TempDir,
    project: PathBuf,
    shim_dir: PathBuf,
    service: MapleAgentService,
    sink: Arc<RecordingSink>,
    host: ExternalAgentHost,
    registry: Arc<ExternalAgentRegistry>,
    pid_file: PathBuf,
    log_file: PathBuf,
}

impl Harness {
    fn new(mode: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let project = root.join("project");
        fs::create_dir_all(&project).unwrap();
        let history = root.join("history");
        fs::create_dir_all(&history).unwrap();
        let shim_dir = root.join("bin");
        fs::create_dir_all(&shim_dir).unwrap();
        let pid_file = root.join("agent.pid");
        let log_file = root.join("agent.log");

        let paths = AgentPathLayout::from_app_roots(root.join("config"), root.join("data"));
        let sink = Arc::new(RecordingSink::default());
        let service = MapleAgentService::new(MapleAgentHostResources::new(
            paths,
            sink.clone(),
            default_tool_context_spec().unwrap(),
            String::new(),
        ));
        let host = ExternalAgentHost {
            runtime: AgentRuntimeHandle {
                service: service.clone(),
                user_id: Arc::from("fixture-user"),
                account_scope: Arc::from("scope"),
                generation: 0,
            },
            service: service.clone(),
            session_manager: Arc::new(SessionManager::new(history)),
            permission_modes: Arc::new(Mutex::new(HashMap::new())),
            project_root: project.clone(),
            lifetime: CancellationToken::new(),
        };
        let registry = Arc::new(ExternalAgentRegistry::new(host.clone()));
        let harness = Self {
            _temp: temp,
            project,
            shim_dir,
            service,
            sink,
            host,
            registry,
            pid_file,
            log_file,
        };
        harness.install_fixture("codex", mode);
        harness.install_fixture("claude", mode);
        harness
    }

    fn install_fixture(&self, provider: &str, mode: &str) {
        let test = match provider {
            "codex" => "fake_codex_app_server",
            "claude" => "claude_fixture::run",
            _ => panic!("unknown fixture provider"),
        };
        // Keep libtest's status output off both protocol pipes. Its output
        // can race the fixture, so inserting a newline is not sufficient.
        let shim = format!(
            "#!/bin/sh\nexport {FIXTURE_MARKER}=1\nexport {FIXTURE_MODE}='{mode}'\nexport {FIXTURE_PID_FILE}='{}'\nexport {FIXTURE_LOG}='{}'\nexport {FIXTURE_ARGS}=\"$*\"\nexec '{}' 'agent::external_agents::tests::{test}' --exact --ignored --nocapture --test-threads=1 3>&1 1>/dev/null\n",
            self.pid_file.display(),
            self.log_file.display(),
            std::env::current_exe().unwrap().display(),
        );
        let path = self.shim_dir.join(provider);
        fs::write(&path, shim).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn call(&self, session_id: &str, row_id: &str) -> ExternalAgentCall {
        ExternalAgentCall {
            session_id: session_id.to_string(),
            working_dir: Some(self.project.clone()),
            row_id: Some(row_id.to_string()),
            login_path: Some(self.shim_dir.to_string_lossy().into_owned()),
            tool_context: SharedAgentToolContext::new(default_tool_context_spec().unwrap())
                .snapshot(),
            cancel_token: CancellationToken::new(),
        }
    }

    async fn set_mode(&self, session_id: &str, mode: GooseMode) {
        self.host
            .permission_modes
            .lock()
            .await
            .insert(session_id.to_string(), mode);
    }

    async fn fixture_pid(&self) -> i32 {
        wait_for(|| {
            fs::read_to_string(&self.pid_file)
                .ok()
                .and_then(|pid| pid.trim().parse::<i32>().ok())
        })
        .await
    }

    fn log(&self) -> String {
        fs::read_to_string(&self.log_file).unwrap_or_default()
    }
}

#[track_caller]
fn wait_for<T>(mut probe: impl FnMut() -> Option<T>) -> impl Future<Output = T> {
    let caller = std::panic::Location::caller();
    async move {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(value) = probe() {
                return value;
            }
            assert!(Instant::now() < deadline, "timed out waiting at {caller}");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

fn result_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|content| content.as_text().map(|text| text.text.clone()))
        .collect()
}

fn process_alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks whether the process can be signalled.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn fixture_output() -> fs::File {
    // SAFETY: install_fixture's shim duplicates the protocol pipe to fd 3
    // before redirecting libtest stdout. Each fixture calls this once and
    // this File is the sole owner of that descriptor in the child process.
    unsafe { fs::File::from_raw_fd(3) }
}

/// The fake app-server. It answers the handshake, starts a thread, and
/// on `turn/start` plays a short turn that asks for one command approval
/// and reports what decision it got in its final message. In `slow` mode
/// it waits for `turn/interrupt` instead.
#[test]
#[ignore = "fake codex app-server run by the driver tests"]
fn fake_codex_app_server() {
    if std::env::var_os(FIXTURE_MARKER).is_none() {
        return;
    }
    let mut out = fixture_output();
    let args = std::env::var(FIXTURE_ARGS).unwrap_or_default();
    if args.contains("--version") {
        writeln!(out, "codex-cli 0.150.0").unwrap();
        out.flush().unwrap();
        return;
    }
    if let Ok(pid_file) = std::env::var(FIXTURE_PID_FILE) {
        fs::write(pid_file, std::process::id().to_string()).unwrap();
    }
    let mode = std::env::var(FIXTURE_MODE).unwrap_or_default();
    let log = std::env::var(FIXTURE_LOG).ok();
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let mut send = |value: Value| {
        // Reproduce libtest status text arriving after fixture startup,
        // without a newline. It must not corrupt a protocol response.
        print!("fixture status");
        std::io::stdout().flush().unwrap();
        writeln!(out, "{value}").unwrap();
        out.flush().unwrap();
    };
    while let Some(Ok(line)) = lines.next() {
        if let Some(log) = &log {
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log)
                .unwrap();
            writeln!(file, "<- {line}").unwrap();
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = message.get("id").cloned();
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_string);
        match (id, method.as_deref()) {
            (Some(id), Some("initialize")) => send(json!({ "id": id, "result": {} })),
            (Some(id), Some("thread/start")) => {
                send(json!({ "id": id, "result": { "thread": { "id": "thread-1" } } }));
                send(
                    json!({ "method": "thread/started", "params": { "thread": { "id": "thread-1" } } }),
                );
            }
            (Some(id), Some("thread/resume")) => {
                send(
                    json!({ "id": id, "result": { "thread": { "id": message["params"]["threadId"] } } }),
                );
            }
            (Some(id), Some("turn/start")) => {
                if let Some(log) = &log {
                    let mut file = fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(log)
                        .unwrap();
                    writeln!(file, "{}", message["params"]).unwrap();
                }
                send(json!({ "id": id, "result": { "turn": { "id": "turn-1" } } }));
                send(
                    json!({ "method": "turn/started", "params": { "threadId": "thread-1", "turn": { "id": "turn-1" } } }),
                );
                if mode == "async-question" {
                    let prompt = message["params"]["input"][0]["text"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    if prompt.starts_with("Answers to your questions") {
                        let last = prompt.lines().last().unwrap_or_default().to_string();
                        send(
                            json!({ "method": "item/completed", "params": { "threadId": "thread-1", "item": { "id": "msg-a", "type": "agentMessage", "text": format!("Got: {last}") } } }),
                        );
                    } else {
                        send(
                            json!({ "method": "item/completed", "params": { "threadId": "thread-1", "item": { "id": "aq-1", "type": "agentMessage", "text": "Tabs or spaces?", "delivery": "async",
                            "questions": [{ "title": "Tabs or spaces?", "options": ["Tabs", "Spaces"] }] } } }),
                        );
                    }
                    send(
                        json!({ "method": "turn/completed", "params": { "threadId": "thread-1", "turn": { "id": "turn-1", "status": "completed" } } }),
                    );
                    continue;
                }
                if mode == "question" {
                    send(
                        json!({ "id": 200, "method": "item/tool/requestUserInput", "params": { "itemId": "q-1", "threadId": "thread-1", "turnId": "turn-1",
                        "questions": [{ "id": "style", "header": "Style", "question": "Tabs or spaces?", "options": [{ "label": "Tabs", "description": "t" }, { "label": "Spaces", "description": "s" }] }] } }),
                    );
                    let mut chosen = String::from("none");
                    for line in lines.by_ref().map_while(Result::ok) {
                        let Ok(message) = serde_json::from_str::<Value>(&line) else {
                            continue;
                        };
                        if message.get("id").and_then(Value::as_u64) == Some(200) {
                            chosen = message["result"]["answers"]["style"]["answers"][0]
                                .as_str()
                                .unwrap_or("none")
                                .to_string();
                            break;
                        }
                    }
                    send(
                        json!({ "method": "item/completed", "params": { "threadId": "thread-1", "item": { "id": "msg-q", "type": "agentMessage", "text": format!("Chosen: {chosen}") } } }),
                    );
                    send(
                        json!({ "method": "turn/completed", "params": { "threadId": "thread-1", "turn": { "id": "turn-1", "status": "completed" } } }),
                    );
                    continue;
                }
                if mode == "slow" {
                    for line in lines.by_ref().map_while(Result::ok) {
                        let Ok(message) = serde_json::from_str::<Value>(&line) else {
                            continue;
                        };
                        if message.get("method").and_then(Value::as_str) == Some("turn/interrupt") {
                            send(json!({ "id": message["id"], "result": {} }));
                            send(
                                json!({ "method": "turn/completed", "params": { "threadId": "thread-1", "turn": { "id": "turn-1", "status": "interrupted" } } }),
                            );
                            break;
                        }
                    }
                    continue;
                }
                send(
                    json!({ "method": "item/started", "params": { "threadId": "thread-1", "item": { "id": "cmd-1", "type": "commandExecution", "command": ["cargo", "test"] } } }),
                );
                send(
                    json!({ "id": 100, "method": "item/commandExecution/requestApproval", "params": { "itemId": "cmd-1", "threadId": "thread-1", "turnId": "turn-1", "command": "cargo test", "cwd": "." } }),
                );
                let mut decision = String::from("none");
                for line in lines.by_ref().map_while(Result::ok) {
                    let Ok(message) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    if message.get("id").and_then(Value::as_u64) == Some(100) {
                        decision = message["result"]["decision"]
                            .as_str()
                            .unwrap_or("none")
                            .to_string();
                        break;
                    }
                }
                let exit_code = if decision == "accept" { 0 } else { 1 };
                send(
                    json!({ "method": "item/completed", "params": { "threadId": "thread-1", "item": { "id": "cmd-1", "type": "commandExecution", "command": ["cargo", "test"], "exitCode": exit_code } } }),
                );
                send(
                    json!({ "method": "item/completed", "params": { "threadId": "thread-1", "item": { "id": "fc-1", "type": "fileChange", "status": "completed", "changes": [{ "path": "src/lib.rs", "kind": "update" }] } } }),
                );
                send(
                    json!({ "method": "item/agentMessage/delta", "params": { "threadId": "thread-1", "itemId": "msg-1", "delta": "Done: " } }),
                );
                send(
                    json!({ "method": "item/agentMessage/delta", "params": { "threadId": "thread-1", "itemId": "msg-1", "delta": decision } }),
                );
                send(
                    json!({ "method": "item/completed", "params": { "threadId": "thread-1", "item": { "id": "msg-1", "type": "agentMessage", "text": format!("Done: {decision}") } } }),
                );
                send(
                    json!({ "method": "turn/completed", "params": { "threadId": "thread-1", "turn": { "id": "turn-1", "status": "completed" } } }),
                );
            }
            (Some(id), Some("turn/interrupt")) => {
                send(json!({ "id": id, "result": {} }));
            }
            (Some(id), Some(method)) => {
                send(
                    json!({ "id": id, "error": { "code": -32601, "message": format!("{method} unsupported") } }),
                );
            }
            _ => {}
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn allow_all_turn_auto_approves_and_streams_activity() {
    let harness = Harness::new("approve");
    harness.set_mode("session-1", GooseMode::Auto).await;

    let result = harness
        .registry
        .start(
            harness.call("session-1", "row-1"),
            AgentStartParams {
                provider: "codex".into(),
                prompt: "Fix the parser".into(),
                background: false,
                model: Some("gpt-5.4".into()),
                effort: None,
                cwd: None,
            },
        )
        .await;
    let text = result_text(&result);
    assert!(
        text.starts_with(
            "Status: completed\nProvider: codex\nAgent ID: codex-1\nThread ID: thread-1\n"
        ),
        "{text}"
    );
    assert!(text.contains("Files changed (1): src/lib.rs"), "{text}");
    assert!(text.contains("Commands run: 1 (0 failed)"), "{text}");
    assert!(
        text.contains("<agent-response>\nDone: accept\n</agent-response>"),
        "{text}"
    );
    assert!(text.contains(AGENT_SEND_TOOL), "{text}");
    let activity = result.structured_content.as_ref().unwrap()[ACTIVITY_KEY].clone();
    assert_eq!(activity["status"], "completed");
    assert_eq!(activity["commands"][0]["exitCode"], 0);

    let log = harness.log();
    // Maple sends no policy; Codex's configuration decides.
    assert!(!log.contains("approvalPolicy"), "{log}");
    assert!(!log.contains("sandboxPolicy"), "{log}");
    assert!(log.contains("\"model\":\"gpt-5.4\""), "{log}");
    assert!(log.contains("Fix the parser"), "{log}");

    let events = harness.sink.events();
    assert!(events.iter().any(|event| matches!(
        event,
        AgentServiceEvent::Run { session_id, run_id, event: AgentRunEvent::SubagentStarted { id, background: false, external: Some(external), .. } }
            if session_id == "session-1" && run_id == "external-codex-1" && id == "external-agent-codex-1" && external.agent_id == "codex-1"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentServiceEvent::Run { event: AgentRunEvent::SubagentFinished { id }, .. } if id == "external-agent-codex-1"
    )));
    let rows = events
        .iter()
        .filter_map(|event| match event {
            AgentServiceEvent::TimelineItem { item, .. } if item.id == "row-1" => Some(item),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        rows.iter()
            .any(|row| row.status.as_deref() == Some("running"))
    );
    let last = rows.last().unwrap();
    assert_eq!(last.status.as_deref(), Some("completed"));
    assert_eq!(
        last.output.as_ref().unwrap()["structuredContent"][ACTIVITY_KEY]["fileChanges"][0]["path"],
        "src/lib.rs"
    );
    // No permission card was needed in Allow all.
    assert!(!events.iter().any(|event| matches!(
        event,
        AgentServiceEvent::Run {
            event: AgentRunEvent::PermissionRequested { .. },
            ..
        }
    )));

    harness.registry.shutdown_all(Duration::from_secs(5)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_turn_puts_the_approval_in_the_permission_table() {
    let harness = Harness::new("approve");
    let registry = Arc::clone(&harness.registry);
    let call = harness.call("session-2", "row-2");
    let turn = tokio::spawn(async move {
        registry
            .start(
                call,
                AgentStartParams {
                    provider: "codex".into(),
                    prompt: "Run the tests".into(),
                    background: false,
                    model: None,
                    effort: None,
                    cwd: None,
                },
            )
            .await
    });

    let pending = {
        let service = harness.service.clone();
        wait_for(|| {
            service
                .pending_permissions
                .try_lock()
                .ok()
                .and_then(|pending| {
                    pending
                        .iter()
                        .next()
                        .map(|(key, entry)| (key.clone(), entry.clone()))
                })
        })
        .await
    };
    let ((session_id, request_id), entry) = pending;
    assert_eq!(session_id, "session-2");
    assert_eq!(request_id, "codex-1-cmd-1");
    assert_eq!(entry.run_id, "external-codex-1");
    assert_eq!(entry.routing, AgentPermissionRouting::Desktop);
    assert_eq!(entry.request.tool_name, "codex_command");
    assert_eq!(entry.request.arguments["command"], "cargo test");
    let events = harness.sink.events();
    assert!(events.iter().any(|event| matches!(
        event,
        AgentServiceEvent::Run { run_id, event: AgentRunEvent::PermissionRequested { request, item }, .. }
            if run_id == "external-codex-1" && request.request_id == "codex-1-cmd-1" && item.id == "permission-codex-1-cmd-1"
    )));
    // The row shows the agent waiting on the user.
    assert!(events.iter().any(|event| matches!(
        event,
        AgentServiceEvent::TimelineItem { item, .. }
            if item.id == "row-2" && item.output.as_ref().unwrap()["structuredContent"][ACTIVITY_KEY]["pendingPermission"] == "run `cargo test`"
    )));

    // The user declines. In the app this comes through resolve_permission;
    // the responder is what that path resolves.
    let PendingPermissionOrigin::ExternalAgent(responder) = entry.origin else {
        panic!("expected an external origin");
    };
    harness
        .service
        .pending_permissions
        .lock()
        .await
        .remove(&(session_id, request_id));
    assert!(responder.resolve(AgentPermissionDecision::DenyOnce));

    let result = turn.await.unwrap();
    let text = result_text(&result);
    assert!(text.contains("Done: decline"), "{text}");
    assert!(text.contains("Commands run: 1 (1 failed)"), "{text}");
    harness.registry.shutdown_all(Duration::from_secs(5)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_the_run_interrupts_the_turn_and_shutdown_kills_the_process() {
    let harness = Harness::new("slow");
    harness.set_mode("session-3", GooseMode::Auto).await;
    let registry = Arc::clone(&harness.registry);
    let call = harness.call("session-3", "row-3");
    let cancel = call.cancel_token.clone();
    let mut turn = tokio::spawn(async move {
        registry
            .start(
                call,
                AgentStartParams {
                    provider: "codex".into(),
                    prompt: "Take your time".into(),
                    background: false,
                    model: None,
                    effort: None,
                    cwd: None,
                },
            )
            .await
    });
    let pid = tokio::select! {
        pid = harness.fixture_pid() => pid,
        result = &mut turn => panic!("Codex fixture stopped before startup: {}", result_text(&result.unwrap())),
    };
    // The initial timeline row precedes turn/start. Wait for the actual
    // turn ID instead of guessing when the notification has been consumed.
    let agent = harness
        .registry
        .agent("session-3", "codex-1")
        .await
        .unwrap();
    wait_for(|| {
        agent
            .state
            .try_lock()
            .ok()
            .and_then(|state| state.turn.as_ref()?.turn_id.as_ref().map(|_| ()))
    })
    .await;
    cancel.cancel();
    let result = turn.await.unwrap();
    let text = result_text(&result);
    assert!(text.starts_with("Status: cancelled"), "{text}");
    assert!(text.contains(AGENT_SEND_TOOL), "{text}");
    // A stop reclaims the process: Codex does not always end a sandboxed
    // command on interrupt, so the process group goes with the turn.
    wait_for(|| (!process_alive(pid)).then_some(())).await;
    let status = harness
        .registry
        .status(
            &harness.call("session-3", "row-3b"),
            AgentRefParams {
                provider: "codex".into(),
                agent_id: "codex-1".into(),
            },
        )
        .await;
    assert!(result_text(&status).starts_with("Status: cancelled"));

    // The agent is still there: the next send starts a fresh process and
    // resumes the same thread.
    fs::remove_file(&harness.pid_file).unwrap();
    let follow_up = harness
        .registry
        .send(
            harness.call("session-3", "row-3c"),
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
    assert!(result_text(&follow_up).starts_with("Status: running"));
    let second_pid = harness.fixture_pid().await;
    assert_ne!(second_pid, pid);
    assert!(process_alive(second_pid));
    let log = harness.log();
    assert!(log.contains("\"threadId\":\"thread-1\""), "{log}");

    // Stop from the row kills that process too.
    harness
        .registry
        .cancel("session-3", "codex-1")
        .await
        .unwrap();
    wait_for(|| (!process_alive(second_pid)).then_some(())).await;
    harness.registry.shutdown_all(Duration::from_secs(5)).await;
    wait_for(|| (!process_alive(pid)).then_some(())).await;
    assert!(harness.registry.snapshot("session-3").await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_turn_reports_its_end_into_the_transcript() {
    let harness = Harness::new("approve");
    let session = harness
        .host
        .session_manager
        .create_session(
            harness.project.clone(),
            "Background output test".into(),
            SessionType::User,
            GooseMode::Auto,
        )
        .await
        .unwrap();
    harness.set_mode(&session.id, GooseMode::Auto).await;
    let result = harness
        .registry
        .start(
            harness.call(&session.id, "row-4"),
            AgentStartParams {
                provider: "codex".into(),
                prompt: "Work in the background".into(),
                background: true,
                model: None,
                effort: None,
                cwd: None,
            },
        )
        .await;
    let text = result_text(&result);
    // The fixture answers instantly, so the turn may already be over when
    // the call returns; either way the guidance matches the status.
    if text.starts_with("Status: running") {
        assert!(text.contains("do not poll"), "{text}");
    } else {
        assert!(text.starts_with("Status: completed"), "{text}");
        assert!(text.contains(AGENT_SEND_TOOL), "{text}");
    }

    let sink = Arc::clone(&harness.sink);
    wait_for(|| {
        sink.events()
            .iter()
            .any(|event| {
                matches!(
                    event,
                    AgentServiceEvent::Run {
                        event: AgentRunEvent::SubagentFinished { .. },
                        ..
                    }
                )
            })
            .then_some(())
    })
    .await;
    let events = harness.sink.events();
    let rows = events
        .iter()
        .filter_map(|event| match event {
            AgentServiceEvent::TimelineItem { item, .. } if item.id == "row-4" => Some(item),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(rows.last().unwrap().status.as_deref(), Some("completed"));
    let status = harness
        .registry
        .status(
            &harness.call(&session.id, "row-4b"),
            AgentRefParams {
                provider: "codex".into(),
                agent_id: "codex-1".into(),
            },
        )
        .await;
    assert!(result_text(&status).contains("Done: accept"));
    // This harness has no running parent runtime, so the completion falls
    // back to history. The actual result must still be included exactly once.
    tokio::time::timeout(WAIT, async {
        loop {
            let saved = harness
                .host
                .session_manager
                .get_session(&session.id, true)
                .await
                .unwrap();
            let messages = saved.conversation.as_ref().unwrap().messages();
            let results = messages
                .iter()
                .filter(|message| {
                    !message.is_user_visible() && message.as_concat_text().contains("Done: accept")
                })
                .collect::<Vec<_>>();
            if !results.is_empty() {
                assert_eq!(results.len(), 1);
                assert!(
                    results[0]
                        .as_concat_text()
                        .contains("without fetching it again")
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    harness.registry.shutdown_all(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn registry_rejects_unknown_providers_bad_cwd_and_too_many_agents() {
    let harness = Harness::new("approve");
    let call = harness.call("session-5", "row-5");
    let start = |provider: &str, cwd: Option<&str>| AgentStartParams {
        provider: provider.into(),
        prompt: "x".into(),
        background: false,
        model: None,
        effort: None,
        cwd: cwd.map(str::to_string),
    };
    let unknown = harness
        .registry
        .start(harness.call("session-5", "r"), start("unknown", None))
        .await;
    assert_eq!(unknown.is_error, Some(true));
    assert!(result_text(&unknown).contains("Unknown agent provider"));

    fs::create_dir_all(harness.project.join("sub")).unwrap();
    let outside = harness.resolve_cwd_for_test(&call, Some(".."));
    assert!(outside.unwrap_err().contains("outside the project root"));
    let inside = harness.resolve_cwd_for_test(&call, Some("sub")).unwrap();
    assert!(inside.ends_with("sub"));

    let empty = harness
        .registry
        .start(
            harness.call("session-5", "r"),
            AgentStartParams {
                provider: "codex".into(),
                prompt: "   ".into(),
                background: false,
                model: None,
                effort: None,
                cwd: None,
            },
        )
        .await;
    assert!(result_text(&empty).contains("prompt must not be empty"));

    {
        let mut sessions = harness.registry.sessions.lock().await;
        let session = sessions.entry("session-5".to_string()).or_default();
        for index in 0..MAX_AGENTS_PER_SESSION {
            let agent_id = format!("codex-{index}");
            session.agents.insert(
                agent_id.clone(),
                Arc::new(ExternalAgent::new(
                    "codex".into(),
                    agent_id,
                    "session-5".into(),
                    "idle".into(),
                    harness.project.clone(),
                    harness.host.clone(),
                    Arc::clone(&harness.registry.issued_permission_ids),
                )),
            );
        }
    }
    let full = harness
        .registry
        .start(harness.call("session-5", "r"), start("codex", None))
        .await;
    assert!(result_text(&full).contains("already has 4 external agents"));
    let missing = harness
        .registry
        .status(
            &call,
            AgentRefParams {
                provider: "codex".into(),
                agent_id: "codex-9".into(),
            },
        )
        .await;
    assert!(result_text(&missing).contains("No external agent 'codex-9'"));
}

impl Harness {
    fn resolve_cwd_for_test(
        &self,
        call: &ExternalAgentCall,
        requested: Option<&str>,
    ) -> Result<PathBuf, String> {
        self.registry.resolve_cwd(call, requested)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_question_from_the_agent_goes_through_the_question_card() {
    let harness = Harness::new("question");
    harness.set_mode("session-6", GooseMode::Auto).await;
    let registry = Arc::clone(&harness.registry);
    let call = harness.call("session-6", "row-6");
    let turn = tokio::spawn(async move {
        registry
            .start(
                call,
                AgentStartParams {
                    provider: "codex".into(),
                    prompt: "Ask me something".into(),
                    background: false,
                    model: None,
                    effort: None,
                    cwd: None,
                },
            )
            .await
    });
    let sink = Arc::clone(&harness.sink);
    let (request_id, questions) = wait_for(|| {
        sink.events().iter().find_map(|event| match event {
            AgentServiceEvent::Question {
                session_id,
                request_id,
                questions,
            } if session_id == "session-6" => Some((request_id.clone(), questions.clone())),
            _ => None,
        })
    })
    .await;
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].id, "style");
    assert_eq!(questions[0].options[1].label, "Spaces");
    assert!(
        harness
            .service
            .answer_question(
                &request_id,
                r#"{"answers":{"style":{"answers":["Spaces"]}}}"#.to_string(),
            )
            .await
    );
    let result = turn.await.unwrap();
    let text = result_text(&result);
    assert!(text.contains("Chosen: Spaces"), "{text}");
    let log = harness.log();
    assert!(log.contains("default_mode_request_user_input"), "{log}");
    harness.registry.shutdown_all(Duration::from_secs(5)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_async_question_is_answered_in_a_turn_of_maples_own() {
    let harness = Harness::new("async-question");
    harness.set_mode("session-7", GooseMode::Auto).await;
    let result = harness
        .registry
        .start(
            harness.call("session-7", "row-7"),
            AgentStartParams {
                provider: "codex".into(),
                prompt: "Ask me something".into(),
                background: false,
                model: None,
                effort: None,
                cwd: None,
            },
        )
        .await;
    let text = result_text(&result);
    assert!(text.starts_with("Status: completed"), "{text}");
    // The turn ended, but the question is still open.
    assert!(
        text.contains("Waiting for the user to decide: answer a question"),
        "{text}"
    );

    let sink = Arc::clone(&harness.sink);
    let request_id = wait_for(|| {
        sink.events().iter().find_map(|event| match event {
            AgentServiceEvent::Question {
                session_id,
                request_id,
                questions,
            } if session_id == "session-7" && questions[0].question == "Tabs or spaces?" => {
                Some(request_id.clone())
            }
            _ => None,
        })
    })
    .await;
    assert!(
        harness
            .service
            .answer_question(
                &request_id,
                r#"{"answers":{"q0":{"answers":["Tabs"]}}}"#.to_string()
            )
            .await
    );
    // The answer starts a turn of Maple's own, with its own row.
    let sink = Arc::clone(&harness.sink);
    let row = wait_for(|| {
        sink.events().iter().find_map(|event| match event {
            AgentServiceEvent::TimelineItem { item, .. }
                if item.id == "external-codex-1-turn-2"
                    && item.status.as_deref() == Some("completed") =>
            {
                Some(item.clone())
            }
            _ => None,
        })
    })
    .await;
    assert_eq!(
        row.title.as_deref(),
        Some("External agent: answer delivered")
    );
    assert!(
        row.output.as_ref().unwrap()["structuredContent"][ACTIVITY_KEY]["text"]
            .as_str()
            .unwrap()
            .contains("Got: Tabs")
    );
    let log = harness.log();
    assert!(log.contains("Answers to your questions"), "{log}");
    let status = harness
        .registry
        .status(
            &harness.call("session-7", "row-7b"),
            AgentRefParams {
                provider: "codex".into(),
                agent_id: "codex-1".into(),
            },
        )
        .await;
    let status_text = result_text(&status);
    assert!(
        !status_text.contains("Waiting for the user"),
        "{status_text}"
    );
    harness.registry.shutdown_all(Duration::from_secs(5)).await;
}

fn claude_start(background: bool) -> AgentStartParams {
    AgentStartParams {
        provider: "claude".into(),
        prompt: "Fix the fixture".into(),
        background,
        model: Some("sonnet".into()),
        effort: Some("high".into()),
        cwd: Some("sub".into()),
    }
}

#[tokio::test]
async fn claude_detection_distinguishes_sign_in_from_probe_failures() {
    for (mode, expected) in [
        ("auth-in", Some(true)),
        ("auth-out", Some(false)),
        ("auth-error", None),
        ("auth-inconsistent", None),
        ("auth-missing", None),
        ("auth-oversized", None),
        ("auth-timeout", None),
    ] {
        let harness = Harness::new(mode);
        let detection = claude::detect(harness.shim_dir.to_str()).await;
        assert_eq!(detection.signed_in, expected, "{mode}");
        assert!(detection.version.is_some(), "{mode}");
        assert!(detection.problem.is_none(), "{mode}");
        assert!(!format!("{detection:?}").contains("secret-canary"));
        let pid = harness.fixture_pid().await;
        assert!(
            !process_alive(pid),
            "{mode}: authentication probe left running"
        );
    }
}

/// Exercises the native Rust transport against a deterministic CLI, with no inference.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_native_streams_resumes_and_binds_the_provider() {
    let harness = Harness::new("approve");
    fs::create_dir(harness.project.join("sub")).unwrap();
    harness.set_mode("claude-task", GooseMode::Auto).await;
    let result = harness
        .registry
        .start(harness.call("claude-task", "r1"), claude_start(false))
        .await;
    let activity = result
        .structured_content
        .as_ref()
        .unwrap_or_else(|| panic!("{}", result_text(&result)))[ACTIVITY_KEY]
        .clone();
    assert_eq!(activity["status"], "completed", "{activity}");
    assert_eq!(activity["provider"], "claude");
    assert_eq!(activity["text"], "Allowed");
    assert_eq!(activity["commands"][0]["status"], "completed");
    assert_eq!(activity["fileChanges"][0]["path"], "src/lib.rs");
    assert_eq!(activity["todos"][0]["completed"], true);
    let agent_id = activity["agentId"].as_str().unwrap().to_string();
    let thread_id = activity["threadId"].as_str().unwrap().to_string();
    let result = harness
        .registry
        .send(
            harness.call("claude-task", "r2"),
            AgentSendParams {
                provider: "claude".into(),
                agent_id: agent_id.clone(),
                prompt: "Continue".into(),
                background: false,
                model: None,
                effort: None,
            },
        )
        .await;
    assert!(
        result_text(&result).contains("Status: completed"),
        "{}",
        result_text(&result)
    );
    let log = harness.log();
    assert!(log.contains("--resume"));
    assert!(
        log.contains("stdin_closed"),
        "completed CLI should exit before resumption"
    );
    assert!(log.contains(&thread_id));
    assert!(log.contains("--effort"));
    assert!(log.contains(&harness.project.join("sub").to_string_lossy().into_owned()));
    assert!(!log.contains("dangerously-skip-permissions"));
    for provider in ["codex", "unknown"] {
        let result = harness
            .registry
            .cancel_tool(
                &harness.call("claude-task", "r3"),
                AgentRefParams {
                    provider: provider.into(),
                    agent_id: agent_id.clone(),
                },
            )
            .await;
        assert_eq!(result.is_error, Some(true));
    }
    let result = harness
        .registry
        .status(
            &harness.call("another-task", "r4"),
            AgentRefParams {
                provider: "claude".into(),
                agent_id,
            },
        )
        .await;
    assert_eq!(result.is_error, Some(true));
    let providers = harness
        .registry
        .list_providers(&harness.call("claude-task", "list"), &["claude".into()])
        .await;
    assert!(result_text(&providers).contains("- claude:"));
    assert!(!result_text(&providers).contains("- codex:"));
    harness.registry.shutdown_all(Duration::from_secs(5)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_native_retries_only_resume_confirmed_sessions() {
    for (mode, confirmed) in [
        ("pre-init-error", false),
        ("pre-init-eof", false),
        ("error", true),
        ("no-init-success", true),
    ] {
        let harness = Harness::new(mode);
        fs::create_dir(harness.project.join("sub")).unwrap();
        harness.set_mode("retry", GooseMode::Auto).await;
        let result = harness
            .registry
            .start(harness.call("retry", "r1"), claude_start(false))
            .await;
        let activity = &result.structured_content.as_ref().unwrap()[ACTIVITY_KEY];
        assert_eq!(
            activity["threadId"].is_string(),
            confirmed,
            "{mode}: {activity}"
        );
        assert_eq!(
            activity["status"],
            if mode == "no-init-success" {
                "completed"
            } else {
                "failed"
            }
        );
        let agent_id = activity["agentId"].as_str().unwrap().to_string();
        harness.install_fixture("claude", "approve");
        for row in ["r2", "r3"] {
            let result = harness
                .registry
                .send(
                    harness.call("retry", row),
                    AgentSendParams {
                        provider: "claude".into(),
                        agent_id: agent_id.clone(),
                        prompt: "Try again".into(),
                        background: false,
                        model: None,
                        effort: None,
                    },
                )
                .await;
            assert_eq!(
                result.structured_content.unwrap()[ACTIVITY_KEY]["status"],
                "completed"
            );
        }
        let args: Vec<Value> = harness
            .log()
            .lines()
            .map(|line| {
                serde_json::from_str::<Value>(line)
                    .unwrap_or_else(|error| panic!("{mode}: invalid fixture log record: {error}"))
            })
            .filter_map(|value| value.get("args").cloned())
            .collect();
        assert_eq!(args.len(), 3);
        let session = |args: &Value| {
            args.as_array()
                .unwrap()
                .windows(2)
                .find_map(|pair| {
                    (pair[0] == "--session-id" || pair[0] == "--resume").then(|| pair[1].clone())
                })
                .unwrap()
        };
        assert!(args[0].as_array().unwrap().contains(&json!("--session-id")));
        assert!(args[1].as_array().unwrap().contains(&json!(if confirmed {
            "--resume"
        } else {
            "--session-id"
        })));
        assert_eq!(session(&args[0]) == session(&args[1]), confirmed);
        assert!(args[2].as_array().unwrap().contains(&json!("--resume")));
        assert_eq!(session(&args[1]), session(&args[2]));
        harness.registry.shutdown_all(Duration::from_secs(5)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_native_cancellation_kills_cli_and_descendants() {
    let harness = Harness::new("slow");
    fs::create_dir(harness.project.join("sub")).unwrap();
    let result = harness
        .registry
        .start(harness.call("claude-stop", "r1"), claude_start(true))
        .await;
    assert!(
        result_text(&result).contains("claude-1"),
        "{}",
        result_text(&result)
    );
    let pid = harness.fixture_pid().await;
    let child_pid = wait_for(|| {
        fs::read_to_string(harness.pid_file.with_extension("pid.child"))
            .ok()
            .and_then(|pid| pid.parse::<i32>().ok())
    })
    .await;
    let activity = harness
        .registry
        .cancel("claude-stop", "claude-1")
        .await
        .unwrap();
    assert_eq!(activity.status, "cancelled");
    wait_for(|| (!process_alive(pid)).then_some(())).await;
    // A killed orphan can briefly remain a zombie before PID 1 reaps it.
    wait_for(|| {
        (!process_alive(child_pid)
            || fs::read_to_string(format!("/proc/{child_pid}/stat"))
                .is_ok_and(|stat| stat.contains(") Z ")))
        .then_some(())
    })
    .await;
    // Reconnect through a new CLI process and resume the session saved before Stop.
    harness.install_fixture("claude", "approve");
    harness.set_mode("claude-stop", GooseMode::Auto).await;
    let result = harness
        .registry
        .send(
            harness.call("claude-stop", "r2"),
            AgentSendParams {
                provider: "claude".into(),
                agent_id: "claude-1".into(),
                prompt: "Resume".into(),
                background: false,
                model: None,
                effort: None,
            },
        )
        .await;
    assert!(
        result_text(&result).contains("Status: completed"),
        "{}",
        result_text(&result)
    );
    assert_eq!(
        result.structured_content.unwrap()[ACTIVITY_KEY]["threadId"].as_str(),
        activity.thread_id.as_deref()
    );
    harness.registry.shutdown_all(Duration::from_secs(5)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_native_permission_denial_and_questions_use_maple_brokers() {
    for mode in ["approve", "question", "multi-question"] {
        let harness = Harness::new(mode);
        fs::create_dir(harness.project.join("sub")).unwrap();
        let registry = harness.registry.clone();
        let call = harness.call("claude-input", "r1");
        let turn = tokio::spawn(async move { registry.start(call, claude_start(false)).await });
        if mode == "approve" {
            let pending = wait_for(|| {
                harness
                    .service
                    .pending_permissions
                    .try_lock()
                    .ok()
                    .and_then(|entries| {
                        entries
                            .iter()
                            .next()
                            .map(|(id, entry)| (id.clone(), entry.clone()))
                    })
            })
            .await;
            let (key, entry) = pending;
            assert_eq!(entry.request.tool_name, "claude_tool");
            assert_eq!(entry.request.arguments["command"], "cargo test");
            harness
                .service
                .pending_permissions
                .lock()
                .await
                .remove(&key);
            let PendingPermissionOrigin::ExternalAgent(responder) = entry.origin else {
                panic!("external responder expected")
            };
            assert!(responder.resolve(AgentPermissionDecision::DenyOnce));
            assert!(!responder.resolve(AgentPermissionDecision::AllowOnce));
        } else {
            let (request_id, questions) = wait_for(|| {
                harness.sink.events().iter().find_map(|event| match event {
                    AgentServiceEvent::Question {
                        session_id,
                        request_id,
                        questions,
                    } if session_id == "claude-input" => {
                        Some((request_id.clone(), questions.clone()))
                    }
                    _ => None,
                })
            })
            .await;
            assert_eq!(questions[0].question, "Tabs or spaces?");
            assert_eq!(questions[0].multi_select, mode == "multi-question");
            assert!(
                harness
                    .service
                    .answer_question(
                        &request_id,
                        if mode == "multi-question" {
                            r#"{"answers":{"q0":{"answers":["Spaces","Tabs"]}}}"#
                        } else {
                            r#"{"answers":{"q0":{"answers":["Spaces"]}}}"#
                        }
                        .into()
                    )
                    .await
            );
        }
        let result = tokio::time::timeout(WAIT, turn).await.unwrap().unwrap();
        let text = result_text(&result);
        assert!(
            text.contains(if mode == "approve" {
                "Denied"
            } else if mode == "multi-question" {
                "Spaces, Tabs"
            } else {
                "Spaces"
            }),
            "{text}"
        );
        harness.registry.shutdown_all(Duration::from_secs(5)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_native_failures_are_not_success_or_unsanitized_output() {
    for mode in ["eof", "error", "init-error", "malformed", "oversized"] {
        let harness = Harness::new(mode);
        fs::create_dir(harness.project.join("sub")).unwrap();
        let result = harness
            .registry
            .start(harness.call("claude-fail", "r1"), claude_start(false))
            .await;
        let text = result_text(&result);
        assert!(
            text.contains("Status: failed") || result.is_error == Some(true),
            "{mode}: {text}"
        );
        assert!(!text.contains("secret-canary"));
        let pid = harness.fixture_pid().await;
        wait_for(|| (!process_alive(pid)).then_some(())).await;
        harness.registry.shutdown_all(Duration::from_secs(5)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_native_stop_withdraws_pending_permission() {
    let harness = Harness::new("approve");
    fs::create_dir(harness.project.join("sub")).unwrap();
    let result = harness
        .registry
        .start(harness.call("claude-pending", "r1"), claude_start(true))
        .await;
    assert!(result_text(&result).contains("claude-1"));
    let key = wait_for(|| {
        harness
            .service
            .pending_permissions
            .try_lock()
            .ok()
            .and_then(|pending| pending.keys().next().cloned())
    })
    .await;
    let pid = harness.fixture_pid().await;
    let activity = harness
        .registry
        .cancel("claude-pending", "claude-1")
        .await
        .unwrap();
    assert_eq!(activity.status, "cancelled");
    wait_for(|| (!process_alive(pid)).then_some(())).await;
    wait_for(|| {
        harness
            .service
            .pending_permissions
            .try_lock()
            .ok()
            .and_then(|pending| (!pending.contains_key(&key)).then_some(()))
    })
    .await;
    assert!(!harness.log().contains("\"behavior\":\"allow\""));
    harness.registry.shutdown_all(Duration::from_secs(5)).await;
}
