//! The runtime end to end, against Pi's scripted provider: a task is
//! created, runs with Maple's tools, is saved and listed, comes back the
//! same after a restart, stops on request, and takes queued and steered
//! messages.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_ai::AssistantContent;
use pi_ai::faux::{FauxProvider, faux_tool_call};
use serde_json::json;

use super::*;
use crate::maple_api::test_maple_api_session;

const USER: &str = "slice@example.com";

#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<AgentServiceEvent>>,
}

impl AgentEventSink for Recorder {
    fn emit(&self, event: &AgentServiceEvent) {
        self.events.lock().unwrap().push(event.clone());
    }
}

impl Recorder {
    /// The events of one run, in order.
    fn run_events(&self, run_id: &str) -> Vec<AgentRunEvent> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                AgentServiceEvent::Run {
                    run_id: id, event, ..
                } if id == run_id => Some(event.clone()),
                _ => None,
            })
            .collect()
    }

    /// The rows a run streamed, merged the way the app merges them.
    fn live_rows(&self, run_id: &str) -> Vec<AgentTimelineItem> {
        let mut rows = Vec::new();
        for event in self.run_events(run_id) {
            if let AgentRunEvent::TimelineItem(item) | AgentRunEvent::Error(item) = event {
                timeline::merge_into(&mut rows, item);
            }
        }
        rows
    }
}

struct Harness {
    service: MapleAgentService,
    handle: AgentRuntimeHandle,
    recorder: Arc<Recorder>,
    faux: FauxProvider,
    data: tempfile::TempDir,
    project: tempfile::TempDir,
}

fn service(data: &Path, recorder: Arc<Recorder>) -> MapleAgentService {
    MapleAgentService::new(MapleAgentHostResources::new(
        AgentPathLayout::from_app_roots(data.join("config"), data.join("local")),
        recorder,
        AgentToolContextSpec::default(),
        "You are Maple.".to_string(),
    ))
}

impl Harness {
    async fn new() -> Self {
        Self::with_faux(FauxProvider::new()).await
    }

    async fn with_faux(faux: FauxProvider) -> Self {
        let data = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let recorder = Arc::new(Recorder::default());
        let service = service(data.path(), recorder.clone());
        let handle = service.handle_for_user(USER).await.unwrap();
        let harness = Self {
            service,
            handle,
            recorder,
            faux,
            data,
            project,
        };
        harness.start().await;
        harness
    }

    async fn start(&self) {
        self.handle
            .start(
                test_maple_api_session(USER),
                Some(AgentStartRequest {
                    project_root: Some(self.project.path().to_string_lossy().into_owned()),
                    model: Some("glm-5-3".to_string()),
                }),
            )
            .await
            .unwrap();
        let runtime = self.service.state.runtime.lock().await.clone().unwrap();
        runtime.use_stream_fn(Arc::new(self.faux.clone()));
    }

    /// A service over the same files, as after the app restarts.
    async fn restart(&mut self) {
        self.handle.stop().await.unwrap();
        self.recorder = Arc::new(Recorder::default());
        self.service = service(self.data.path(), self.recorder.clone());
        self.handle = self.service.handle_for_user(USER).await.unwrap();
    }

    async fn create_task(&self) -> String {
        self.handle.create_session(None).await.unwrap().session.id
    }

    fn request(&self, session_id: &str, text: &str) -> AgentSendMessageRequest {
        AgentSendMessageRequest {
            session_id: session_id.to_string(),
            text: text.to_string(),
            model: Some("glm-5-3".to_string()),
            context_limit: Some(200_000),
            vision_capable: false,
            steer: false,
            queue_id: None,
            attachments: Vec::new(),
        }
    }

    async fn send(&self, session_id: &str, text: &str) -> AgentRunHandle {
        self.handle
            .send_message(self.request(session_id, text))
            .await
            .unwrap()
    }
}

async fn finished(run: &mut AgentRunHandle) -> AgentRunTerminal {
    let terminal = tokio::time::timeout(
        Duration::from_secs(10),
        run.terminal.wait_for(Option::is_some),
    )
    .await
    .expect("the run ends")
    .expect("the run reports how it ended");
    terminal.expect("checked above")
}

/// Wait until `condition` holds, checking every few milliseconds.
async fn eventually(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the condition holds in time");
}

/// What a person sees of rows, without their timing.
fn shown(items: &[AgentTimelineItem]) -> Vec<(String, String, Option<String>, Option<String>)> {
    items
        .iter()
        .map(|item| {
            (
                item.id.clone(),
                item.item_type.clone(),
                item.text.clone(),
                item.status.clone(),
            )
        })
        .collect()
}

fn last_user_text(request: &pi_ai::faux::FauxRequest) -> String {
    request
        .context
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            pi_ai::Message::User(user) => Some(pi_ai::content_text(&user.content)),
            _ => None,
        })
        .unwrap_or_default()
}

fn user_texts(request: &pi_ai::faux::FauxRequest) -> Vec<String> {
    request
        .context
        .messages
        .iter()
        .filter_map(|message| match message {
            pi_ai::Message::User(user) => Some(pi_ai::content_text(&user.content)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_task_runs_saves_and_comes_back_after_a_restart() {
    let mut harness = Harness::new().await;
    std::fs::create_dir_all(harness.project.path().join("src")).unwrap();
    std::fs::write(
        harness.project.path().join("src/main.rs"),
        "fn main() { println!(\"hello\"); }\n",
    )
    .unwrap();
    harness.faux.push_message(vec![
        AssistantContent::thinking("Look at the file first."),
        AssistantContent::text("Reading it."),
        faux_tool_call("read", json!({"path": "src/main.rs"})),
    ]);
    harness.faux.push_text("It prints hello.");

    let task = harness.create_task().await;
    let mut run = harness.send(&task, "What does main do?").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);

    // The model saw the file through Maple's read tool.
    let requests = harness.faux.requests();
    assert_eq!(requests.len(), 2);
    let tool_result = requests[1]
        .context
        .messages
        .iter()
        .find_map(|message| match message {
            pi_ai::Message::ToolResult(result) => Some(pi_ai::content_text(&result.content)),
            _ => None,
        })
        .unwrap();
    assert!(tool_result.contains("println!(\"hello\")"), "{tool_result}");
    // Harness instructions follow Pi's own prompt.
    let system = requests[0]
        .context
        .messages
        .iter()
        .find_map(|message| match message {
            pi_ai::Message::System(system) => Some(serde_json::to_string(system).unwrap()),
            _ => None,
        })
        .unwrap();
    assert!(system.contains("You are Maple."), "{system}");
    assert!(system.contains("operating inside Maple"), "{system}");

    let events = harness.recorder.run_events(&run.run_id);
    assert!(matches!(
        events.first(),
        Some(AgentRunEvent::SessionUpdated(_))
    ));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentRunEvent::Started))
    );
    assert!(matches!(
        events.last(),
        Some(AgentRunEvent::Finished(AgentRunTerminal::Completed))
    ));
    let live = harness.recorder.live_rows(&run.run_id);
    let kinds: Vec<&str> = live.iter().map(|row| row.item_type.as_str()).collect();
    assert_eq!(kinds, ["message", "thinking", "message", "tool", "message"]);

    // The index lists the task, named from its prompt.
    let listed = harness.handle.list_sessions(None).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].title, "What does main do?");
    assert_eq!(listed[0].message_count, 3);
    assert_eq!(listed[0].model.as_deref(), Some("glm-5-3"));

    // After a restart the task lists and reads the same while stopped.
    harness.restart().await;
    let listed = harness.handle.list_sessions(None).await.unwrap();
    assert_eq!(listed[0].id, task);
    let detail = harness.handle.load_session(task.clone()).await.unwrap();
    assert_eq!(shown(&detail.timeline), shown(&live));

    // A follow-up continues with the earlier context.
    harness.start().await;
    harness.faux.push_text("You're welcome.");
    let mut run = harness.send(&task, "Thanks").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    let follow_up = harness.faux.requests().pop().unwrap();
    assert_eq!(
        user_texts(&follow_up),
        ["What does main do?".to_string(), "Thanks".to_string()]
    );
    let detail = harness.handle.load_session(task).await.unwrap();
    assert_eq!(
        detail.timeline.last().unwrap().text.as_deref(),
        Some("You're welcome.")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn bash_runs_commands_in_the_task_folder() {
    let harness = Harness::new().await;
    harness.faux.push_message(vec![faux_tool_call(
        "bash",
        json!({"command": "pwd; printf '%s' \"$AGENT_SESSION_ID\"", "timeout": 5}),
    )]);
    harness.faux.push_message(vec![faux_tool_call(
        "bash",
        json!({"command": "printf oops >&2; exit 4"}),
    )]);
    harness.faux.push_text("Done.");
    let task = harness.create_task().await;
    let mut run = harness.send(&task, "Where am I?").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);

    // The model reads each command's output, and whether it failed.
    let requests = harness.faux.requests();
    assert_eq!(requests.len(), 3);
    let results: Vec<(String, bool)> = requests[2]
        .context
        .messages
        .iter()
        .filter_map(|message| match message {
            pi_ai::Message::ToolResult(result) => {
                Some((pi_ai::content_text(&result.content), result.is_error))
            }
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    let (output, failed) = &results[0];
    assert!(!failed, "{output}");
    let mut lines = output.lines();
    let folder = std::fs::canonicalize(lines.next().unwrap()).unwrap();
    assert_eq!(
        folder,
        std::fs::canonicalize(harness.project.path()).unwrap()
    );
    assert_eq!(lines.next(), Some(task.as_str()));
    let (output, failed) = &results[1];
    assert!(failed);
    assert_eq!(output, "oops\n\nCommand exited with code 4");

    // The task's rows show each command and how it ended.
    let detail = harness.handle.load_session(task).await.unwrap();
    let tools: Vec<&AgentTimelineItem> = detail
        .timeline
        .iter()
        .filter(|row| row.item_type == "tool")
        .collect();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].status.as_deref(), Some("completed"));
    assert_eq!(
        tools[0].title.as_deref(),
        Some("Terminal: pwd; printf '%s' \"$AGENT_SESSION_ID\"")
    );
    assert_eq!(tools[1].status.as_deref(), Some("failed"));
}

#[tokio::test]
async fn the_model_shows_its_plan_and_asks_the_user() {
    let harness = Harness::new().await;
    let todos = json!({"todos": [{"content": "Pick a folder", "status": "in_progress"}]});
    harness
        .faux
        .push_message(vec![faux_tool_call("todo_write", todos.clone())]);
    harness.faux.push_message(vec![faux_tool_call(
        "request_user_input",
        json!({"question": "Which folder?", "options": [{"label": "src"}]}),
    )]);
    harness.faux.push_text("Done.");
    let task = harness.create_task().await;
    let mut run = harness.send(&task, "Plan it").await;

    // The question reaches the interface as a card for this task.
    let question = || {
        harness
            .recorder
            .events
            .lock()
            .unwrap()
            .iter()
            .find_map(|event| match event {
                AgentServiceEvent::Question {
                    session_id,
                    request_id,
                    questions,
                } if *session_id == task => Some((request_id.clone(), questions.clone())),
                _ => None,
            })
    };
    eventually(|| question().is_some()).await;
    let (request_id, questions) = question().unwrap();
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].id, "question_0");
    assert_eq!(questions[0].header, "Question");
    assert_eq!(questions[0].options[0].label, "src");
    let answer = r#"{"answers":{"question_0":{"answers":["src"]}}}"#;
    assert!(
        harness
            .service
            .answer_question(&request_id, answer.to_string())
            .await
    );
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);

    // The model sees its plan confirmed, then the answer.
    let requests = harness.faux.requests();
    let results: Vec<String> = requests[2]
        .context
        .messages
        .iter()
        .filter_map(|message| match message {
            pi_ai::Message::ToolResult(result) => Some(pi_ai::content_text(&result.content)),
            _ => None,
        })
        .collect();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&results[0]).unwrap(),
        todos
    );
    assert_eq!(results[1], answer);

    // The plan's row carries the list the interface draws.
    let detail = harness.handle.load_session(task).await.unwrap();
    assert!(
        detail
            .timeline
            .iter()
            .any(|row| row.item_type == "tool" && row.input.as_ref() == Some(&todos)),
        "{:?}",
        detail.timeline
    );
}

#[tokio::test]
async fn stop_keeps_the_reply_so_far_and_says_it_was_stopped() {
    let harness =
        Harness::with_faux(FauxProvider::new().with_chunk_delay(Duration::from_millis(20))).await;
    let reply = "Counting slowly: one two three four five six seven eight nine ten.";
    harness.faux.push_text(reply);
    let task = harness.create_task().await;
    let mut run = harness.send(&task, "Count").await;

    // Stop once some of the reply has arrived.
    let run_id = run.run_id.clone();
    eventually(|| {
        harness.recorder.live_rows(&run_id).iter().any(|row| {
            row.role.as_deref() == Some("assistant")
                && row.text.as_deref().is_some_and(|t| !t.is_empty())
        })
    })
    .await;
    harness
        .handle
        .cancel_desktop_run(run_id.clone())
        .await
        .unwrap();
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Cancelled);

    let detail = harness.handle.load_session(task.clone()).await.unwrap();
    let texts: Vec<Option<&str>> = detail
        .timeline
        .iter()
        .map(|item| item.text.as_deref())
        .collect();
    let partial = texts[1].expect("the stopped reply is kept");
    assert!(
        !partial.is_empty() && partial.len() < reply.len() && reply.starts_with(partial),
        "{partial}"
    );
    assert_eq!(texts.last().unwrap(), &Some(timeline::STOPPED_NOTICE_TEXT));
    // The live rows end the same way.
    let live = harness.recorder.live_rows(&run_id);
    assert_eq!(
        live.last().and_then(|row| row.text.as_deref()),
        Some(timeline::STOPPED_NOTICE_TEXT)
    );

    // The task is not running any more and takes the next prompt.
    harness.faux.push_text("Done.");
    let mut run = harness.send(&task, "Go on").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
}

#[tokio::test]
async fn a_message_sent_during_a_run_waits_as_a_chip_and_follows() {
    let harness =
        Harness::with_faux(FauxProvider::new().with_chunk_delay(Duration::from_millis(10))).await;
    harness
        .faux
        .push_text("First answer, streamed slowly enough to queue behind.");
    harness.faux.push_text("Second answer.");
    let task = harness.create_task().await;
    let mut run = harness.send(&task, "First").await;

    let staged = harness.send(&task, "Second").await;
    assert_eq!(staged.run_id, run.run_id);
    let chip = staged.queued.expect("the message waits as a chip");
    assert_eq!(chip.text, "Second");
    assert_eq!(staged.queue.items.len(), 1);

    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    let requests = harness.faux.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(last_user_text(&requests[1]), "Second");
    let events = harness.recorder.run_events(&run.run_id);
    let queues: Vec<usize> = events
        .iter()
        .filter_map(|event| match event {
            AgentRunEvent::QueueChanged(snapshot) => Some(snapshot.items.len()),
            _ => None,
        })
        .collect();
    assert_eq!(queues, [1, 0]);
    let detail = harness.handle.load_session(task).await.unwrap();
    let users: Vec<&str> = detail
        .timeline
        .iter()
        .filter(|item| item.role.as_deref() == Some("user"))
        .filter_map(|item| item.text.as_deref())
        .collect();
    assert_eq!(users, ["First", "Second"]);
    assert!(detail.queue.items.is_empty());
}

#[tokio::test]
async fn chips_can_be_cancelled_and_several_go_as_one_turn() {
    let harness =
        Harness::with_faux(FauxProvider::new().with_chunk_delay(Duration::from_millis(10))).await;
    harness
        .faux
        .push_text("First answer, streamed slowly enough to queue behind.");
    harness.faux.push_text("Both answered.");
    let task = harness.create_task().await;
    let mut run = harness.send(&task, "First").await;

    let dropped = harness.send(&task, "Never mind").await.queued.unwrap();
    harness.send(&task, "Second").await;
    harness.send(&task, "Third").await;
    let snapshot = harness
        .handle
        .cancel_queued_message(AgentQueueControlRequest {
            session_id: task.clone(),
            queue_id: dropped.queue_id,
        })
        .await
        .unwrap();
    assert_eq!(snapshot.items.len(), 2);

    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    let requests = harness.faux.requests();
    assert_eq!(requests.len(), 2, "the two chips make one turn");
    assert_eq!(
        user_texts(&requests[1]),
        [
            "First".to_string(),
            "Second".to_string(),
            "Third".to_string()
        ]
    );
}

#[tokio::test]
async fn steering_joins_the_running_turn() {
    let harness =
        Harness::with_faux(FauxProvider::new().with_chunk_delay(Duration::from_millis(10))).await;
    harness.faux.push_message(vec![
        AssistantContent::text("Reading."),
        faux_tool_call("read", json!({"path": "missing.txt"})),
    ]);
    harness.faux.push_text("Adjusted.");
    let task = harness.create_task().await;
    let mut run = harness.send(&task, "Read it").await;

    let mut steer = harness.request(&task, "Use the other file");
    steer.steer = true;
    let joined = harness.handle.send_message(steer).await.unwrap();
    assert_eq!(joined.run_id, run.run_id);
    assert!(joined.queued.is_none());

    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    let requests = harness.faux.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(last_user_text(&requests[1]), "Use the other file");
}

#[tokio::test]
async fn a_failed_reply_fails_the_run_and_shows_its_error() {
    let harness = Harness::new().await;
    harness.faux.push_error("Maple credits are exhausted");
    let task = harness.create_task().await;
    let mut run = harness.send(&task, "Hi").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Failed);
    let detail = harness.handle.load_session(task).await.unwrap();
    let error = detail.timeline.last().unwrap();
    assert_eq!(error.item_type, "error");
    assert_eq!(error.title.as_deref(), Some("Credits exhausted"));
}

#[tokio::test]
async fn a_run_wakes_a_settled_task_and_a_started_task_keeps_its_model() {
    let harness = Harness::new().await;
    harness.faux.push_text("Hello.");
    let task = harness.create_task().await;
    harness
        .handle
        .set_session_state(task.clone(), AgentTaskState::Settled)
        .await
        .unwrap();
    let mut run = harness.send(&task, "Hi").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    let listed = harness.handle.list_sessions(None).await.unwrap();
    assert_eq!(listed[0].state, AgentTaskState::Active);

    let mut other_model = harness.request(&task, "Again");
    other_model.model = Some("kimi-k3".to_string());
    let error = harness
        .handle
        .send_message(other_model)
        .await
        .err()
        .unwrap();
    assert!(error.contains("locked to model glm-5-3"), "{error}");
}

#[tokio::test]
async fn tasks_rename_archive_and_delete_with_their_files() {
    let harness = Harness::new().await;
    harness.faux.push_text("Hello.");
    let task = harness.create_task().await;
    let mut run = harness.send(&task, "Hi").await;
    finished(&mut run).await;

    let renamed = harness
        .handle
        .rename_session(
            test_maple_api_session(USER),
            AgentRenameSessionRequest {
                session_id: task.clone(),
                title: "  Greeting  ".to_string(),
            },
        )
        .await
        .unwrap();
    assert_eq!(renamed.title, "Greeting");
    let archived = harness
        .handle
        .set_session_state(task.clone(), AgentTaskState::Archived)
        .await
        .unwrap();
    assert_eq!(archived.state, AgentTaskState::Archived);

    let session_file = account_sessions_dir(harness.handle.paths(), USER)
        .unwrap()
        .join(format!("{task}.jsonl"));
    assert!(session_file.exists());
    harness.handle.delete_session(task.clone()).await.unwrap();
    assert!(!session_file.exists());
    assert!(harness.handle.list_sessions(None).await.unwrap().is_empty());
    assert!(harness.handle.load_session(task).await.is_err());
}

#[tokio::test]
async fn a_running_task_cannot_be_deleted_or_settled() {
    let harness = Harness::new().await;
    harness.faux.push_hang();
    let task = harness.create_task().await;
    let mut run = harness.send(&task, "Wait").await;
    let error = harness
        .handle
        .delete_session(task.clone())
        .await
        .unwrap_err();
    assert!(error.contains("Stop the running agent"), "{error}");
    let error = harness
        .handle
        .set_session_state(task.clone(), AgentTaskState::Settled)
        .await
        .unwrap_err();
    assert!(error.contains("Stop the running agent"), "{error}");
    let status = harness.handle.status().await.unwrap();
    assert_eq!(status.active_runs.get(&task), Some(&run.run_id));

    harness
        .handle
        .cancel_desktop_run(run.run_id.clone())
        .await
        .unwrap();
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Cancelled);
}

#[tokio::test]
async fn stopping_the_runtime_ends_its_runs() {
    let harness = Harness::new().await;
    harness.faux.push_hang();
    let task = harness.create_task().await;
    let mut run = harness.send(&task, "Wait").await;
    harness.handle.stop().await.unwrap();
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Cancelled);
    assert!(!harness.handle.status().await.unwrap().running);
}

#[tokio::test]
async fn a_cleared_account_revokes_its_handles() {
    let harness = Harness::new().await;
    harness
        .service
        .advance_generation(&account_scope(USER).unwrap());
    let error = harness.handle.list_sessions(None).await.unwrap_err();
    assert!(error.contains("data changed"), "{error}");
}

#[tokio::test]
async fn mcp_servers_are_saved_apart_from_the_other_settings() {
    let harness = Harness::new().await;
    let server = AgentMcpServer {
        name: " Files ".into(),
        description: String::new(),
        enabled: true,
        timeout_seconds: 300,
        transport: AgentMcpTransport::Stdio {
            command: "npx server-files".into(),
            environment: Vec::new(),
        },
    };
    let saved = harness.handle.save_mcp_servers(vec![server]).await.unwrap();
    assert_eq!(saved[0].name, "Files");
    assert_eq!(harness.handle.list_mcp_servers().await.unwrap(), saved);

    // A project or model save made from an older copy keeps the servers.
    let stale = AgentConfig {
        mcp_servers: Vec::new(),
        ..harness.handle.load_config().await.unwrap()
    };
    harness.handle.save_config(stale).await.unwrap();
    assert_eq!(harness.handle.list_mcp_servers().await.unwrap(), saved);
    let error = harness
        .handle
        .save_mcp_servers(vec![saved[0].clone(), saved[0].clone()])
        .await
        .unwrap_err();
    assert!(error.contains("conflicts with another"), "{error}");
}

#[tokio::test]
async fn integrations_list_and_refuse_what_cannot_be_enabled() {
    let harness = Harness::new().await;
    let cards = harness.handle.list_integrations().await.unwrap();
    let ids: Vec<&str> = cards.iter().map(|card| card.id.as_str()).collect();
    assert_eq!(ids, ["cua-driver", "codex", "claude"]);
    assert_eq!(
        cards[0].availability,
        AgentIntegrationAvailability::NotDetected
    );
    assert_eq!(cards[0].backend, None);

    let error = harness
        .handle
        .set_integration_enabled(AgentSetIntegrationEnabledRequest {
            id: "cua-driver".into(),
            enabled: true,
        })
        .await
        .unwrap_err();
    assert!(error.contains("permissions"), "{error}");
    let error = harness
        .handle
        .set_integration_enabled(AgentSetIntegrationEnabledRequest {
            id: "elsewhere".into(),
            enabled: true,
        })
        .await
        .unwrap_err();
    assert_eq!(error, "Unknown integration 'elsewhere'");
    let cards = harness
        .handle
        .setup_integration(AgentSetupIntegrationRequest {
            id: "cua-driver".into(),
        })
        .await
        .unwrap();
    assert_eq!(cards[0].backend, None, "setup cannot finish in this build");
}

#[tokio::test]
async fn answers_reach_the_question_broker_of_the_service() {
    let harness = Harness::new().await;
    let broker = harness.service.question_broker();
    assert!(!harness.service.answer_question("unknown", "x".into()).await);
    drop(broker);
}
