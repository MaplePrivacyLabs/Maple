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

#[path = "image_persistence_tests.rs"]
mod image_persistence_tests;

const USER: &str = "slice@example.com";

#[derive(Default)]
pub(super) struct Recorder {
    events: Mutex<Vec<AgentServiceEvent>>,
}

impl AgentEventSink for Recorder {
    fn emit(&self, event: &AgentServiceEvent) {
        self.events.lock().unwrap().push(event.clone());
    }
}

impl Recorder {
    /// Every event so far, in order.
    pub(super) fn events(&self) -> Vec<AgentServiceEvent> {
        self.events.lock().unwrap().clone()
    }

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

/// A running runtime whose tasks' model is scripted. Other modules' tests
/// use it too.
pub(crate) struct Harness {
    pub(super) service: MapleAgentService,
    pub(crate) handle: AgentRuntimeHandle,
    pub(super) recorder: Arc<Recorder>,
    /// The tasks' model.
    pub(crate) faux: FauxProvider,
    /// The side model of titles and summaries, scripted apart so a task's
    /// script does not depend on when they are asked.
    side: FauxProvider,
    data: tempfile::TempDir,
    pub(crate) project: tempfile::TempDir,
}

/// Side-model requests to their own script, the rest to the task's.
pub(super) struct SplitStream {
    pub(super) task: FauxProvider,
    pub(super) side: FauxProvider,
}

impl pi_ai::StreamFn for SplitStream {
    fn stream(
        &self,
        model: &pi_ai::Model,
        context: pi_ai::Context,
        options: pi_ai::StreamOptions,
    ) -> pi_ai::AssistantMessageStream {
        if side_models::SIDE_MODELS.contains(&model.id.as_str()) {
            self.side.stream(model, context, options)
        } else {
            self.task.stream(model, context, options)
        }
    }
}

fn service(data: &Path, recorder: Arc<Recorder>) -> MapleAgentService {
    MapleAgentService::new(
        MapleAgentHostResources::new(
            // A home of its own, so no skill or instruction file of the real
            // one reaches the tasks.
            AgentPathLayout::from_app_roots(data.join("config"), data.join("local"))
                .with_home(Some(data.join("home"))),
            recorder,
            "You are Maple.".to_string(),
        )
        .with_store(Arc::new(
            crate::store::Store::open(crate::store::StorePaths::new(data.join("local"))).unwrap(),
        )),
    )
}

impl Harness {
    #[cfg(feature = "acp")]
    pub(crate) fn emit_service_event(&self, event: AgentServiceEvent) {
        emit_agent_event(&self.service.host.events, event);
    }

    pub(crate) fn emit_timeline_item(&self, session_id: &str, item: AgentTimelineItem) {
        emit_agent_event(
            &self.service.host.events,
            AgentServiceEvent::TimelineItem {
                session_id: session_id.into(),
                run_id: None,
                item,
            },
        );
    }

    pub(crate) async fn new() -> Self {
        Self::with_faux(FauxProvider::new()).await
    }

    pub(crate) async fn with_faux(faux: FauxProvider) -> Self {
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
            side: FauxProvider::new(),
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
        runtime.use_stream_fn(Arc::new(SplitStream {
            task: self.faux.clone(),
            side: self.side.clone(),
        }));
    }

    /// A service over the same files, as after the app restarts.
    async fn restart(&mut self) {
        self.handle.stop().await.unwrap();
        self.recorder = Arc::new(Recorder::default());
        self.service = service(self.data.path(), self.recorder.clone());
        self.handle = self.service.handle_for_user(USER).await.unwrap();
    }

    /// The running runtime.
    pub(super) async fn runtime(&self) -> Arc<AgentRuntime> {
        self.service.state.runtime.lock().await.clone().unwrap()
    }

    /// Serve `catalog` as the backend's model catalog.
    pub(crate) async fn serve_catalog(&self, catalog: serde_json::Value) {
        self.runtime()
            .await
            .api
            .set_test_catalog(serde_json::from_value(catalog).unwrap());
    }

    pub(crate) async fn create_task(&self) -> String {
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

    pub(crate) async fn send(&self, session_id: &str, text: &str) -> AgentRunHandle {
        self.handle
            .send_message(self.request(session_id, text))
            .await
            .unwrap()
    }
}

pub(crate) async fn finished(run: &mut AgentRunHandle) -> AgentRunTerminal {
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
pub(crate) async fn eventually(mut condition: impl FnMut() -> bool) {
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

pub(crate) fn last_user_text(request: &pi_ai::faux::FauxRequest) -> String {
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
    // The transport times requests out; Pi waits without a limit.
    assert_eq!(requests[0].options.timeout_ms, Some(i32::MAX as u64));
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

    // The context ring reads Pi's count of the task's context.
    let context = harness
        .handle
        .session_context_tokens(&task)
        .await
        .unwrap()
        .expect("the task has context");
    let empty = harness.create_task().await;
    assert_eq!(
        harness.handle.session_context_tokens(&empty).await.unwrap(),
        None
    );
    harness.handle.delete_session(empty).await.unwrap();

    // After a restart the task lists and reads the same while stopped.
    harness.restart().await;
    let listed = harness.handle.list_sessions(None).await.unwrap();
    assert_eq!(listed[0].id, task);
    let detail = harness.handle.load_session(task.clone()).await.unwrap();
    assert_eq!(shown(&detail.timeline), shown(&live));
    assert_eq!(
        harness.handle.session_context_tokens(&task).await.unwrap(),
        Some(context),
        "the stored task counts the same"
    );

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
async fn the_web_tools_follow_the_tasks_web_switch() {
    let harness = Harness::new().await;
    let task = harness.create_task().await;
    let declared = |request: &pi_ai::faux::FauxRequest| -> Vec<String> {
        pi_ai::transcript::current_tools(&request.context.messages)
            .into_iter()
            .map(|tool| tool.name)
            .collect()
    };
    let switch = |enabled| AgentSetSessionWebRequest {
        session_id: task.clone(),
        enabled,
    };
    for (enabled, reply) in [(true, "On."), (false, "Off."), (true, "On again.")] {
        harness
            .handle
            .set_session_web_enabled(switch(enabled))
            .await
            .unwrap();
        harness.faux.push_text(reply);
        let mut run = harness.send(&task, "Look it up").await;
        assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    }
    let requests = harness.faux.requests();
    let tools: Vec<Vec<String>> = requests.iter().map(declared).collect();
    for name in ["todo_write", "request_user_input"] {
        assert!(
            tools
                .iter()
                .all(|tools| tools.iter().any(|tool| tool == name))
        );
    }
    let has_web = |tools: &[String]| {
        ["web_search", "open_url"]
            .iter()
            .all(|name| tools.iter().any(|tool| tool == name))
    };
    assert!(has_web(&tools[0]), "{:?}", tools[0]);
    assert!(
        !tools[1]
            .iter()
            .any(|tool| tool == "web_search" || tool == "open_url"),
        "{:?}",
        tools[1]
    );
    assert!(has_web(&tools[2]), "{:?}", tools[2]);
}

#[tokio::test]
async fn skills_reach_the_model_once_the_project_is_trusted_and_as_they_change() {
    let harness = Harness::new().await;
    let project = harness.project.path().canonicalize().unwrap();
    let root = project.to_string_lossy().into_owned();
    let write = |path: PathBuf, text: &str| {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    let skill = |name: &str| {
        write(
            project.join(".agents/skills").join(name).join("SKILL.md"),
            &format!("---\ndescription: The {name} skill\n---\nDo {name}."),
        )
    };
    write(project.join("AGENTS.md"), "Use tabs.");
    skill("build");

    let status = harness
        .handle
        .get_project_trust(root.clone())
        .await
        .unwrap();
    assert_eq!(status.decision, None);
    assert_eq!(
        status.protected_features,
        [AgentProjectTrustFeature::Skills]
    );
    let task = harness.create_task().await;
    let runtime = harness.service.state.runtime.lock().await.clone().unwrap();
    let mut prompts = Vec::new();
    for (index, reply) in ["One.", "Two.", "Three."].into_iter().enumerate() {
        match index {
            // Trusting opens the project's skills to its tasks and the `/` list.
            1 => {
                let status = harness
                    .handle
                    .set_project_trust(root.clone(), true)
                    .await
                    .unwrap();
                assert_eq!(status.decision, Some(true));
                let commands = harness.service.list_slash_commands(Some(USER), Some(&root));
                assert!(commands.iter().any(|command| command.name == "build"));
                let expanded = harness
                    .service
                    .resolve_slash_command(Some(USER), Some(&root), "build", "now")
                    .unwrap()
                    .unwrap();
                assert!(
                    expanded.contains("Do build.\n</skill>\n\nnow"),
                    "{expanded}"
                );
            }
            // A skill added while the task is loaded shows from its next run.
            2 => skill("test"),
            _ => {}
        }
        harness.faux.push_text(reply);
        let mut run = harness.send(&task, "Go").await;
        assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
        let session = runtime.loaded_session(&task).await.unwrap();
        prompts.push(session.system_prompt());
    }
    // Instruction files load whatever the decision.
    assert!(prompts.iter().all(|prompt| prompt.contains("Use tabs.")));
    let listed = |prompt: &String, name: &str| prompt.contains(&format!("<name>{name}</name>"));
    assert!(!listed(&prompts[0], "build"), "{}", prompts[0]);
    assert!(listed(&prompts[1], "build") && !listed(&prompts[1], "test"));
    assert!(listed(&prompts[2], "build") && listed(&prompts[2], "test"));
}

#[tokio::test]
async fn a_new_task_gets_a_generated_title_unless_renamed_first() {
    let harness = Harness::new().await;
    harness.side.push_text("\"Fix the login bug\"");
    harness.faux.push_text("Done.");
    let task = harness.create_task().await;
    let mut run = harness
        .send(&task, "please fix the login bug in auth.rs")
        .await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    let title = || async {
        harness
            .handle
            .load_session(task.clone())
            .await
            .unwrap()
            .session
            .title
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        while title().await != "Fix the login bug" {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the generated title is saved");
    let request = harness.side.requests().pop().unwrap();
    assert_eq!(request.model.id, side_models::TITLE_MODEL);
    assert_eq!(request.options.reasoning, None);
    assert!(last_user_text(&request).contains("please fix the login bug"));
    assert!(harness.recorder.events.lock().unwrap().iter().any(|event| {
        matches!(event, AgentServiceEvent::SessionUpdated { session, .. }
            if session.title == "Fix the login bug")
    }));

    // A title the user gives while the generated one is on its way stays.
    let harness = Harness::with_faux(FauxProvider::new()).await;
    let slow_side = FauxProvider::new().with_chunk_delay(Duration::from_millis(100));
    slow_side.push_text("A generated title that arrives late");
    let runtime = harness.service.state.runtime.lock().await.clone().unwrap();
    runtime.use_stream_fn(Arc::new(SplitStream {
        task: harness.faux.clone(),
        side: slow_side.clone(),
    }));
    harness.faux.push_text("Done.");
    let task = harness.create_task().await;
    let mut run = harness.send(&task, "rename me").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    harness
        .handle
        .rename_session(
            test_maple_api_session(USER),
            AgentRenameSessionRequest {
                session_id: task.clone(),
                title: "Mine".to_string(),
            },
        )
        .await
        .unwrap();
    eventually(|| slow_side.pending() == 0).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let detail = harness.handle.load_session(task).await.unwrap();
    assert_eq!(detail.session.title, "Mine");
}

#[tokio::test]
async fn tool_calls_and_thinking_are_summarized_by_deepseek_without_thinking() {
    let harness = Harness::new().await;
    harness.side.push_text("\"Listed the files\"\nextra");
    harness.side.push_text("Lists the files");
    harness.side.push_text("Decided to read main.rs first");
    let task = harness.create_task().await;
    let summary = harness
        .handle
        .summarize_tool_call(&task, "bash", Some(&json!({"command": "ls"})), "main.rs")
        .await
        .unwrap();
    assert_eq!(summary.as_deref(), Some("Listed the files"));
    let summary = harness
        .handle
        .summarize_tool_call_start(&task, "bash", Some(&json!({"command": "ls"})))
        .await
        .unwrap();
    assert_eq!(summary.as_deref(), Some("Lists the files"));
    let summary = harness
        .handle
        .summarize_thinking(&task, "I should read main.rs before editing.")
        .await
        .unwrap();
    assert_eq!(summary.as_deref(), Some("Decided to read main.rs first"));
    let requests = harness.side.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests {
        let body =
            pi_ai::openai::build_request_body(&request.model, &request.context, &request.options)
                .unwrap();
        assert_eq!(body["model"], "deepseek-v4-1-flash");
        assert_eq!(body["reasoning_effort"], "none");
        assert_eq!(body["max_tokens"], 48);
        assert!(body.get("tools").is_none());
    }
    // Legacy callers keep the output-based request; a call summarized as it
    // starts sends only its input.
    assert_eq!(
        last_user_text(&requests[0]),
        "Tool: bash\nInput: {\"command\":\"ls\"}\nOutput: main.rs"
    );
    assert_eq!(
        last_user_text(&requests[1]),
        "Tool: bash\nInput: {\"command\":\"ls\"}"
    );
    assert_ne!(
        pi_ai::transcript::current_system_prompt(&requests[0].context.messages),
        pi_ai::transcript::current_system_prompt(&requests[1].context.messages)
    );
    assert_eq!(requests[0].options.max_tokens, Some(48));
    assert!(
        harness.faux.requests().is_empty(),
        "the task's model is not asked"
    );
}

#[tokio::test]
async fn a_side_question_streams_an_answer_and_leaves_the_task_alone() {
    let harness = Harness::new().await;
    harness.faux.push_text("I read main.rs.");
    let task = harness.create_task().await;
    let mut run = harness.send(&task, "Read main.rs").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    let before = harness.handle.load_session(task.clone()).await.unwrap();

    harness.faux.push_text("Because it holds the entry point.");
    harness
        .handle
        .ask_side_question(
            &task,
            "side-1".to_string(),
            Vec::new(),
            "Why main.rs?".to_string(),
        )
        .await
        .unwrap();
    let side_events = || -> Vec<SideQuestionEvent> {
        harness
            .recorder
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                AgentServiceEvent::SideQuestion {
                    request_id, event, ..
                } if request_id == "side-1" => Some(event.clone()),
                _ => None,
            })
            .collect()
    };
    eventually(|| {
        side_events()
            .iter()
            .any(|event| !matches!(event, SideQuestionEvent::Chunk(_)))
    })
    .await;
    let events = side_events();
    assert!(
        matches!(events.last(), Some(SideQuestionEvent::Finished)),
        "{events:?}"
    );
    let answer: String = events
        .iter()
        .filter_map(|event| match event {
            SideQuestionEvent::Chunk(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(answer, "Because it holds the entry point.");

    // The question went with the task's history, framed, and nothing was saved.
    let request = harness.faux.requests().pop().unwrap();
    let texts = user_texts(&request);
    assert_eq!(texts[0], "Read main.rs");
    assert!(texts[1].ends_with("\n\nWhy main.rs?"), "{texts:?}");
    let after = harness.handle.load_session(task).await.unwrap();
    assert_eq!(after.timeline.len(), before.timeline.len());
}

#[tokio::test]
async fn persistence_retry_keeps_the_runtime_context_and_active_run() {
    let harness = Harness::new().await;
    let runtime = harness.runtime().await;
    let task = harness.create_task().await;
    harness.faux.push(pi_ai::faux::FauxResponse::Hang);
    let mut run = harness.send(&task, "Keep working").await;
    eventually(|| !harness.faux.requests().is_empty()).await;
    let session = runtime.loaded_session(&task).await.unwrap();
    let before = session.with_session(|manager| manager.entries().len());
    let store = harness.service.host.store.as_ref().unwrap();
    store.record_persistence_failure(&crate::store::StoreError::DiskFull);
    assert!(store.persistence_failure().is_some());

    harness.service.flush_persistence().await.unwrap();

    assert!(store.persistence_failure().is_none());
    assert!(Arc::ptr_eq(&runtime, &harness.runtime().await));
    assert!(!runtime.lifetime.is_cancelled());
    assert!(runtime.runs.is_running(&task));
    assert!(runtime.loaded_session(&task).await.is_some());
    assert_eq!(
        session.with_session(|manager| manager.entries().len()),
        before
    );
    assert!(!session.has_unpersisted());

    harness
        .handle
        .cancel_desktop_run(run.run_id.clone())
        .await
        .unwrap();
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Cancelled);
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
async fn a_hidden_message_reaches_the_model_but_not_the_timeline() {
    let harness =
        Harness::with_faux(FauxProvider::new().with_chunk_delay(Duration::from_millis(10))).await;
    let runtime = harness.service.state.runtime.lock().await.clone().unwrap();
    let task = harness.create_task().await;
    harness.faux.push_text("Hello.");
    let mut run = harness.send(&task, "Hi").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);

    // With no run, it starts one of its own.
    harness.faux.push_text("Thanks for the result.");
    runtime
        .deliver_hidden(&task, "maple.test", "the agent finished".to_string())
        .await
        .unwrap();
    eventually(|| harness.faux.requests().len() == 2 && !runtime.runs.is_running(&task)).await;
    assert_eq!(
        last_user_text(&harness.faux.requests()[1]),
        "the agent finished"
    );

    // During a run, it waits out of sight, then follows on its own turn.
    harness
        .faux
        .push_text("A long answer, streamed slowly enough to deliver behind it.");
    harness.faux.push_text("Noted.");
    let mut run = harness.send(&task, "More").await;
    runtime
        .deliver_hidden(&task, "maple.test", "another result".to_string())
        .await
        .unwrap();
    assert!(runtime.runs.queue_snapshot(&task).items.is_empty());
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    let requests = harness.faux.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(last_user_text(&requests[3]), "another result");

    let detail = harness.handle.load_session(task.clone()).await.unwrap();
    let texts: Vec<&str> = detail
        .timeline
        .iter()
        .filter_map(|item| item.text.as_deref())
        .collect();
    assert_eq!(
        texts,
        [
            "Hi",
            "Hello.",
            "Thanks for the result.",
            "More",
            "A long answer, streamed slowly enough to deliver behind it.",
            "Noted."
        ]
    );

    // Stopped before it went, it stays in the history the next prompt
    // reads, without a turn of its own.
    harness
        .faux
        .push_text("Counting slowly: one two three four five six seven eight nine ten.");
    let mut run = harness.send(&task, "Count").await;
    runtime
        .deliver_hidden(&task, "maple.test", "a stopped result".to_string())
        .await
        .unwrap();
    let run_id = run.run_id.clone();
    eventually(|| {
        harness.recorder.live_rows(&run_id).iter().any(|row| {
            row.role.as_deref() == Some("assistant")
                && row.text.as_deref().is_some_and(|text| !text.is_empty())
        })
    })
    .await;
    harness.handle.cancel_desktop_run(run_id).await.unwrap();
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Cancelled);
    harness.faux.push_text("Done.");
    let mut run = harness.send(&task, "Go on").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    let requests = harness.faux.requests();
    assert_eq!(requests.len(), 6);
    let sent = user_texts(&requests[5]);
    assert_eq!(sent[sent.len() - 2..], ["a stopped result", "Go on"]);
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

/// A 1x1 PNG.
const PNG_1X1: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

async fn with_image(
    harness: &Harness,
    mut request: AgentSendMessageRequest,
    name: &str,
) -> AgentSendMessageRequest {
    request.attachments = harness
        .handle
        .import_image_attachments(
            request.session_id.clone(),
            vec![AgentImageUpload {
                name: name.to_string(),
                data_url: format!("data:image/png;base64,{PNG_1X1}"),
            }],
        )
        .await
        .unwrap()
        .into_iter()
        .map(|attachment| attachment.id)
        .collect();
    request
}

/// The last user message of a request, as the model got it.
fn last_user(request: &pi_ai::faux::FauxRequest) -> pi_ai::UserMessage {
    request
        .context
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            pi_ai::Message::User(user) => Some(user.clone()),
            _ => None,
        })
        .unwrap()
}

#[tokio::test]
async fn attached_images_reach_the_model_and_show_with_their_message() {
    use base64::Engine as _;

    let harness = Harness::new().await;
    harness.faux.push_message(vec![faux_tool_call(
        "request_user_input",
        json!({"question":"Wait?","options":[{"label":"Continue"}]}),
    )]);
    harness.faux.push_text("A dialog.");
    harness.faux.push_text("Also a dialog.");
    let task = harness.create_task().await;
    let first = with_image(
        &harness,
        harness.request(&task, "What is this?"),
        "dialog.png",
    )
    .await;
    let mut run = harness.handle.send_message(first).await.unwrap();
    let question = || {
        harness
            .recorder
            .events()
            .iter()
            .find_map(|event| match event {
                AgentServiceEvent::Question {
                    session_id,
                    request_id,
                    ..
                } if session_id == &task => Some(request_id.clone()),
                _ => None,
            })
    };
    eventually(|| question().is_some()).await;
    // A message sent during the run waits as a chip with its image.
    // The question holds the first turn while the durable image import finishes.
    let mut second = with_image(&harness, harness.request(&task, "And this?"), "second.png").await;
    second.vision_capable = true;
    let staged = harness.handle.send_message(second).await.unwrap();
    assert_eq!(staged.run_id, run.run_id);
    assert_eq!(staged.queued.unwrap().attachments[0].name, "second.png");
    release_attachment_question(&harness, &question().unwrap()).await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);

    // A model without vision gets the image by source, to look with
    // read_image; one with vision gets it beside the text.
    let requests = harness.faux.requests();
    assert_eq!(
        requests.len(),
        3,
        "first question, its answer, then the queued image"
    );
    let first = last_user(&requests[0]);
    let text = pi_ai::content_text(&first.content);
    assert!(
        text.starts_with(
            "What is this?\n\nThe user attached these files on the execution computer:\n- "
        ),
        "{text}"
    );
    assert!(text.ends_with("when you need visual details from an image."));
    assert_eq!(first.content.len(), 1);
    let second = last_user(&requests[2]);
    assert!(!pi_ai::content_text(&second.content).contains("read_image"));
    assert!(matches!(
        &second.content[1],
        pi_ai::Content::Image(image) if image.mime_type == "image/png"
    ));

    // Live and after a reload, a message shows what the user typed and its
    // images, which the interface reads from the task.
    let users = |rows: &[AgentTimelineItem]| -> Vec<(String, Vec<(String, String)>)> {
        rows.iter()
            .filter(|row| row.role.as_deref() == Some("user"))
            .map(|row| {
                let images = row
                    .input
                    .as_ref()
                    .and_then(|input| input["attachments"].as_array().cloned())
                    .unwrap_or_default()
                    .iter()
                    .map(|image| {
                        (
                            image["id"].as_str().unwrap().to_string(),
                            image["name"].as_str().unwrap().to_string(),
                        )
                    })
                    .collect();
                (row.text.clone().unwrap_or_default(), images)
            })
            .collect()
    };
    let live = users(&harness.recorder.live_rows(&run.run_id));
    let detail = harness.handle.load_session(task.clone()).await.unwrap();
    assert_eq!(users(&detail.timeline), live);
    let shown: Vec<(&str, &str)> = live
        .iter()
        .map(|(text, images)| (text.as_str(), images[0].1.as_str()))
        .collect();
    assert_eq!(
        shown,
        [("What is this?", "dialog.png"), ("And this?", "second.png")]
    );
    let bytes = harness
        .handle
        .read_image_attachment(task, live[0].1[0].0.clone())
        .await
        .unwrap();
    assert_eq!(
        bytes,
        base64::engine::general_purpose::STANDARD
            .decode(PNG_1X1)
            .unwrap()
    );
}

async fn committed_file(
    harness: &Harness,
    task: &str,
    name: &str,
    mime: &str,
    bytes: &[u8],
) -> AgentAttachment {
    use sha2::{Digest, Sha256};
    let runtime = harness.runtime().await;
    let store = runtime.store.store();
    let metadata = crate::store::AttachmentUploadMetadata {
        name: name.into(),
        mime_type: mime.into(),
        size: bytes.len() as u64,
        sha256: format!("{:x}", Sha256::digest(bytes)),
    };
    let started = store
        .begin_attachment_upload(
            runtime.store.account(),
            task,
            &format!("{:032x}", rand::random::<u128>()),
            &metadata,
        )
        .unwrap();
    let crate::store::AttachmentUploadStart::Writing(mut writer) = started else {
        panic!("new upload")
    };
    writer.write_chunk(bytes).unwrap();
    writer.finish(store).unwrap().into()
}

#[tokio::test]
async fn mixed_files_and_images_use_committed_paths_and_preserve_refs_in_queue_edits_and_steering()
{
    let harness = Harness::new().await;
    let task = harness.create_task().await;
    let zip = committed_file(
        &harness,
        &task,
        "report.zip",
        "application/zip",
        b"PK archive bytes",
    )
    .await;
    let image = harness
        .handle
        .import_image_attachments(
            task.clone(),
            vec![AgentImageUpload {
                name: "screen.png".into(),
                data_url: format!("data:image/png;base64,{PNG_1X1}"),
            }],
        )
        .await
        .unwrap()
        .remove(0);
    harness.faux.push_message(vec![faux_tool_call(
        "request_user_input",
        json!({"question":"Wait?","options":[{"label":"Continue"}]}),
    )]);
    harness.faux.push_text("Reviewed.");
    let mut first = harness.request(&task, "Wait before reviewing");
    first.vision_capable = true;
    let mut run = harness.handle.send_message(first).await.unwrap();
    let question = || {
        harness
            .recorder
            .events()
            .iter()
            .find_map(|event| match event {
                AgentServiceEvent::Question {
                    session_id,
                    request_id,
                    ..
                } if session_id == &task => Some(request_id.clone()),
                _ => None,
            })
    };
    eventually(|| question().is_some()).await;
    let mut upload = harness.request(&task, "Review both");
    upload.vision_capable = true;
    upload.attachments = vec![zip.id.clone(), image.id.clone()];
    let chip = harness
        .handle
        .send_message(upload)
        .await
        .unwrap()
        .queued
        .unwrap();
    assert_eq!(chip.attachments, [zip.clone(), image.clone()]);
    let control = AgentQueueControlRequest {
        session_id: task.clone(),
        queue_id: chip.queue_id.clone(),
    };
    harness
        .handle
        .begin_queued_message_edit(control)
        .await
        .unwrap();
    let session = harness.runtime().await.loaded_session(&task).await.unwrap();
    let steered = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let listener = session.subscribe({
        let steered = steered.clone();
        move |event| {
            if matches!(event, pi_coding_agent::AgentSessionEvent::QueueUpdate { steering, .. }
                if steering.iter().any(|text| text.starts_with("Review the archive and screenshot")))
            {
                steered.store(true, std::sync::atomic::Ordering::Release);
            }
        }
    });
    // Editing the queued message only changes its text; the original committed
    // file identities and image references survive the steering boundary.
    let mut edit = harness.request(&task, "Review the archive and screenshot");
    edit.queue_id = Some(chip.queue_id);
    edit.steer = true;
    harness.handle.send_message(edit).await.unwrap();
    eventually(|| steered.load(std::sync::atomic::Ordering::Acquire)).await;
    session.unsubscribe(listener);
    assert!(
        harness
            .service
            .answer_question(
                &question().unwrap(),
                r#"{"answers":{"question_0":{"answers":["Continue"]}}}"#.into()
            )
            .await
    );
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    let request = harness.faux.requests().pop().unwrap();
    let user = last_user(&request);
    let text = pi_ai::content_text(&user.content);
    let (typed, shown) = attachments::split_attachment_prompt(&text).unwrap();
    assert_eq!(typed, "Review the archive and screenshot");
    assert_eq!(shown, [zip.clone(), image]);
    assert!(matches!(&user.content[1], pi_ai::Content::Image(image) if image.data == PNG_1X1));
    assert_eq!(
        std::fs::read(&zip.execution_path).unwrap(),
        b"PK archive bytes"
    );
    assert!(!harness.project.path().join("report.zip").exists());
    let detail = harness.handle.load_session(task).await.unwrap();
    let input = detail
        .timeline
        .iter()
        .rev()
        .find(|row| row.role.as_deref() == Some("user"))
        .unwrap()
        .input
        .as_ref()
        .unwrap();
    assert_eq!(input["attachments"][0]["executionPath"], zip.execution_path);
}

#[tokio::test]
async fn accepted_message_refreshes_project_recency_but_rejected_send_does_not() {
    let harness = Harness::new().await;
    let task = harness.create_task().await;
    let (store, account) = harness.handle.test_persistence().unwrap();
    let root = harness
        .handle
        .store()
        .unwrap()
        .get(&task)
        .unwrap()
        .unwrap()
        .project_root;
    store.register_explicit_project(account, &root, 1).unwrap();
    let before = store.project(account, &root).unwrap().unwrap();
    let mut invalid = harness.request(&task, "Reject invalid attachment");
    invalid.attachments = vec!["not-an-id".into()];
    assert!(harness.handle.send_message(invalid).await.is_err());
    assert_eq!(store.project(account, &root).unwrap().unwrap(), before);
    harness.faux.push_text("Done");
    let mut run = harness.send(&task, "Use this project again").await;
    let after = store.project(account, &root).unwrap().unwrap();
    assert!(after.last_used_ms > before.last_used_ms);
    assert_eq!(after.position, before.position);
    finished(&mut run).await;
    // Model completion is not a new user choice.
    assert_eq!(
        store.project(account, &root).unwrap().unwrap().last_used_ms,
        after.last_used_ms
    );
}

#[tokio::test]
async fn an_invalid_or_foreign_attachment_rejects_the_whole_send_before_inference() {
    let harness = Harness::new().await;
    let task = harness.create_task().await;
    let other = harness.create_task().await;
    let valid = committed_file(&harness, &task, "notes.txt", "text/plain", b"notes").await;
    let foreign = committed_file(&harness, &other, "foreign.zip", "application/zip", b"PK").await;
    for bad in ["not-an-id".to_string(), foreign.id] {
        let mut request = harness.request(&task, "Do not partially accept this");
        request.attachments = vec![valid.id.clone(), bad];
        assert!(harness.handle.send_message(request).await.is_err());
    }
    assert!(harness.faux.requests().is_empty());
    let detail = harness.handle.load_session(task).await.unwrap();
    assert!(
        !detail
            .timeline
            .iter()
            .any(|row| row.role.as_deref() == Some("user"))
    );
}

#[tokio::test]
async fn cold_native_image_history_materializes_only_for_inference_and_missing_files_are_nonfatal()
{
    let harness = Harness::new().await;
    let task = harness.create_task().await;
    let image = harness
        .handle
        .import_image_attachments(
            task.clone(),
            vec![AgentImageUpload {
                name: "screen.png".into(),
                data_url: format!("data:image/png;base64,{PNG_1X1}"),
            }],
        )
        .await
        .unwrap()
        .remove(0);
    harness.faux.push_text("Seen.");
    let mut first = harness.request(&task, "Inspect");
    first.vision_capable = true;
    first.attachments = vec![image.id];
    let mut run = harness.handle.send_message(first).await.unwrap();
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    let runtime = harness.runtime().await;
    runtime.unload_task(&task).await.unwrap();
    let source = || {
        runtime
            .store
            .store()
            .load_session(
                runtime.store.account(),
                &task,
                crate::store::SessionLoad::Full,
            )
            .unwrap()
            .entries
    };
    let saved = source();
    let saved_image_ids = saved
        .iter()
        .flat_map(|entry| match &entry.kind {
            pi_coding_agent::session::EntryKind::Message {
                message: pi_coding_agent::SessionMessage::Llm(pi_ai::Message::User(user)),
            } => user
                .content
                .iter()
                .filter_map(|block| match block {
                    pi_ai::Content::Image(image) => {
                        attachments::attachment_id_from_source(&image.data).map(str::to_string)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect::<Vec<_>>();
    assert!(!saved_image_ids.is_empty());
    // Pi may normalize the incoming image into a separate persisted copy.
    // Remove the pixels actually referenced by cold source history.
    for id in saved_image_ids {
        let image = runtime
            .store
            .store()
            .attachment_metadata(runtime.store.account(), &task, &id)
            .unwrap();
        std::fs::remove_file(image.execution_path).unwrap();
    }
    harness
        .faux
        .push_text("I can continue without the missing image.");
    let mut next = harness.request(&task, "Continue");
    next.vision_capable = true;
    let mut run = harness.handle.send_message(next).await.unwrap();
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    let request = harness.faux.requests().pop().unwrap();
    assert!(request.context.messages.iter().any(|message| matches!(message,
        pi_ai::Message::User(user) if user.content.iter().any(|block|
            matches!(block, pi_ai::Content::Text(text) if text.text == "Image no longer available.")))));
    let after = source();
    assert_eq!(&after[..saved.len()], saved);
}

#[tokio::test]
async fn cold_side_questions_materialize_images_and_degrade_missing_history() {
    let harness = Harness::new().await;
    let task = harness.create_task().await;
    let mut first = with_image(&harness, harness.request(&task, "Inspect"), "screen.png").await;
    first.vision_capable = true;
    harness.faux.push_text("Seen.");
    let mut run = harness.handle.send_message(first).await.unwrap();
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    let runtime = harness.runtime().await;
    runtime.unload_task(&task).await.unwrap();
    let saved = runtime
        .store
        .store()
        .load_session(
            runtime.store.account(),
            &task,
            crate::store::SessionLoad::Full,
        )
        .unwrap()
        .entries;
    let image_ids: Vec<String> = saved
        .iter()
        .flat_map(|entry| match &entry.kind {
            pi_coding_agent::session::EntryKind::Message {
                message: pi_coding_agent::SessionMessage::Llm(pi_ai::Message::User(user)),
            } => user
                .content
                .iter()
                .filter_map(|block| match block {
                    pi_ai::Content::Image(image) => {
                        attachments::attachment_id_from_source(&image.data).map(str::to_string)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect();
    assert!(!image_ids.is_empty());
    for missing in [false, true] {
        runtime.unload_task(&task).await.unwrap();
        if missing {
            for id in &image_ids {
                let image = runtime
                    .store
                    .store()
                    .attachment_metadata(runtime.store.account(), &task, id)
                    .unwrap();
                std::fs::remove_file(image.execution_path).unwrap();
            }
        }
        let request_id = format!("side-image-{missing}");
        harness.faux.push_text("Side answer.");
        harness
            .handle
            .ask_side_question(
                &task,
                request_id.clone(),
                Vec::new(),
                "What was in the image?".into(),
            )
            .await
            .unwrap();
        eventually(|| harness.recorder.events().iter().any(|event| matches!(event,
            AgentServiceEvent::SideQuestion { session_id, request_id: id, event }
                if session_id == &task && id == &request_id && !matches!(event, SideQuestionEvent::Chunk(_))
        ))).await;
        assert!(harness.recorder.events().iter().any(|event| matches!(event,
            AgentServiceEvent::SideQuestion { session_id, request_id: id, event: SideQuestionEvent::Finished }
                if session_id == &task && id == &request_id
        )));
        let request = harness.faux.requests().pop().unwrap();
        let content: Vec<_> = request
            .context
            .messages
            .iter()
            .flat_map(|message| match message {
                pi_ai::Message::User(user) => user.content.iter().collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect();
        if missing {
            assert!(content.iter().any(|block| matches!(block,
                pi_ai::Content::Text(text) if text.text == "Image no longer available.")));
            assert!(
                !content
                    .iter()
                    .any(|block| matches!(block, pi_ai::Content::Image(_)))
            );
        } else {
            assert!(content.iter().any(|block| matches!(block,
                pi_ai::Content::Image(image) if image.data == PNG_1X1)));
        }
        assert_eq!(
            runtime
                .store
                .store()
                .load_session(
                    runtime.store.account(),
                    &task,
                    crate::store::SessionLoad::Full
                )
                .unwrap()
                .entries,
            saved
        );
    }
}

#[tokio::test]
async fn pasted_foreign_file_listing_does_not_poison_later_requests() {
    let harness = Harness::new().await;
    let task = harness.create_task().await;
    let other = harness.create_task().await;
    let foreign = committed_file(
        &harness,
        &other,
        "notes.txt",
        "text/plain",
        b"private notes",
    )
    .await;
    let pasted = attachments::attachment_prompt("Earlier notes said:", &[foreign], false);
    for text in [pasted.as_str(), "hello", "are you there?"] {
        harness.faux.push_text("Yes.");
        let mut run = harness.send(&task, text).await;
        assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
        let request = harness.faux.requests().pop().unwrap();
        assert_eq!(last_user_text(&request), text);
        assert_eq!(user_texts(&request)[0], pasted);
    }
}

async fn attachment_question_barrier(harness: &Harness, task: &str) -> (AgentRunHandle, String) {
    harness.faux.push_message(vec![faux_tool_call(
        "request_user_input",
        json!({"question":"Wait?","options":[{"label":"Continue"}]}),
    )]);
    harness.faux.push_text("Released.");
    let mut first = harness.request(task, "Wait before continuing");
    first.vision_capable = true;
    let run = harness.handle.send_message(first).await.unwrap();
    let question = || {
        harness
            .recorder
            .events()
            .iter()
            .find_map(|event| match event {
                AgentServiceEvent::Question {
                    session_id,
                    request_id,
                    ..
                } if session_id == task => Some(request_id.clone()),
                _ => None,
            })
    };
    eventually(|| question().is_some()).await;
    (run, question().unwrap())
}

async fn release_attachment_question(harness: &Harness, question: &str) {
    assert!(
        harness
            .service
            .answer_question(
                question,
                r#"{"answers":{"question_0":{"answers":["Continue"]}}}"#.into(),
            )
            .await
    );
}

fn change_attachment_bytes(path: &str) {
    let mut bytes = std::fs::read(path).unwrap();
    *bytes.last_mut().unwrap() ^= 1;
    std::fs::write(path, bytes).unwrap();
}

#[tokio::test]
async fn failed_batch_image_preparation_preserves_unsent_messages_in_order() {
    for failed_index in [0, 1] {
        let harness = Harness::new().await;
        let task = harness.create_task().await;
        let (mut run, question) = attachment_question_barrier(&harness, &task).await;
        let mut failed_image = None;
        let texts = ["First queued", "Second queued", "Third queued"];
        for (index, text) in texts.iter().enumerate() {
            let mut request = harness.request(&task, text);
            request.vision_capable = true;
            if index == failed_index {
                request = with_image(&harness, request, "queued.png").await;
            }
            let chip = harness
                .handle
                .send_message(request)
                .await
                .unwrap()
                .queued
                .unwrap();
            if index == failed_index {
                failed_image = Some(chip.attachments[0].clone());
            }
        }
        let image = failed_image.unwrap();
        change_attachment_bytes(&image.execution_path);
        // The valid first message still runs when a later message cannot be prepared.
        harness.faux.push_text("First queued completed.");
        release_attachment_question(&harness, &question).await;
        assert_eq!(finished(&mut run).await, AgentRunTerminal::Failed);
        let detail = harness.handle.load_session(task.clone()).await.unwrap();
        let queued: Vec<_> = detail
            .queue
            .items
            .iter()
            .map(|chip| chip.text.as_str())
            .collect();
        assert_eq!(queued, texts[failed_index..]);
        assert_eq!(detail.queue.items[0].attachments, [image]);
        let users: Vec<_> = detail
            .timeline
            .iter()
            .filter(|row| row.role.as_deref() == Some("user"))
            .filter_map(|row| row.text.as_deref())
            .collect();
        let expected = if failed_index == 0 {
            vec!["Wait before continuing"]
        } else {
            vec!["Wait before continuing", "First queued"]
        };
        assert_eq!(users, expected);
        assert!(harness.recorder.run_events(&run.run_id).iter().any(|event| matches!(event,
            AgentRunEvent::Error(item) if item.text.as_deref().is_some_and(|text| text.contains("attachment"))
        )));
    }
}

#[tokio::test]
async fn failed_steered_image_preserves_accepted_messages_and_reports_an_error() {
    let harness = Harness::new().await;
    let task = harness.create_task().await;
    let (mut run, question) = attachment_question_barrier(&harness, &task).await;
    let mut request = with_image(
        &harness,
        harness.request(&task, "Inspect this image"),
        "steered.png",
    )
    .await;
    request.vision_capable = true;
    request.steer = true;
    let runtime = harness.runtime().await;
    let image: AgentAttachment = runtime
        .store
        .store()
        .attachment_metadata(runtime.store.account(), &task, &request.attachments[0])
        .unwrap()
        .into();
    // A same-size rewrite passes admission's file checks; image preparation
    // must detect it without losing the accepted steer.
    change_attachment_bytes(&image.execution_path);
    let joined = harness.handle.send_message(request).await.unwrap();
    assert_eq!(joined.run_id, run.run_id);
    let mut later = harness.request(&task, "Then use the result");
    later.steer = true;
    later.vision_capable = true;
    harness.handle.send_message(later).await.unwrap();
    eventually(|| {
        harness
            .recorder
            .run_events(&run.run_id)
            .iter()
            .any(|event| matches!(event, AgentRunEvent::Error(_)))
    })
    .await;
    release_attachment_question(&harness, &question).await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Failed);
    let detail = harness.handle.load_session(task).await.unwrap();
    assert_eq!(
        detail
            .queue
            .items
            .iter()
            .map(|chip| chip.text.as_str())
            .collect::<Vec<_>>(),
        ["Inspect this image", "Then use the result"]
    );
    assert_eq!(detail.queue.items[0].attachments, [image]);
    let users: Vec<_> = detail
        .timeline
        .iter()
        .filter(|row| row.role.as_deref() == Some("user"))
        .filter_map(|row| row.text.as_deref())
        .collect();
    assert_eq!(users, ["Wait before continuing"]);
    assert_eq!(
        harness
            .recorder
            .run_events(&run.run_id)
            .iter()
            .filter(|event| matches!(event, AgentRunEvent::Error(_)))
            .count(),
        1
    );
}

/// The first tool result the model got in `request`.
fn first_tool_result(request: &pi_ai::faux::FauxRequest) -> pi_ai::ToolResultMessage {
    request
        .context
        .messages
        .iter()
        .find_map(|message| match message {
            pi_ai::Message::ToolResult(result) => Some(result.clone()),
            _ => None,
        })
        .unwrap()
}

#[tokio::test]
async fn a_model_without_vision_looks_at_images_through_read_image() {
    use base64::Engine as _;

    let harness = Harness::new().await;
    let png = base64::engine::general_purpose::STANDARD
        .decode(PNG_1X1)
        .unwrap();
    std::fs::write(harness.project.path().join("shot.png"), png).unwrap();
    for _ in 0..2 {
        harness
            .faux
            .push_message(vec![faux_tool_call("read", json!({"path": "shot.png"}))]);
        harness.faux.push_text("Seen.");
    }
    let blind = harness.create_task().await;
    let mut run = harness.send(&blind, "What is in shot.png?").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    let sighted = harness.create_task().await;
    let mut request = harness.request(&sighted, "What is in shot.png?");
    request.vision_capable = true;
    let mut run = harness.handle.send_message(request).await.unwrap();
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);

    // Without vision, `read` sends the model to read_image for an image, as
    // Goose's runtime did, and the prompt says to look at images with it.
    let requests = harness.faux.requests();
    assert_eq!(requests.len(), 4);
    let result = first_tool_result(&requests[1]);
    assert_eq!(
        pi_ai::content_text(&result.content),
        "shot.png is an image. Use read_image to inspect it."
    );
    assert!(
        result
            .content
            .iter()
            .all(|block| matches!(block, pi_ai::Content::Text(_)))
    );
    let guideline = "Use read_image when you need to inspect an image.";
    let prompt = |request: &pi_ai::faux::FauxRequest| {
        serde_json::to_string(&request.context.messages).unwrap()
    };
    assert!(prompt(&requests[0]).contains(guideline));
    // With vision, `read` gives the image, as Pi's does.
    let result = first_tool_result(&requests[3]);
    assert!(
        result
            .content
            .iter()
            .any(|block| matches!(block, pi_ai::Content::Image(_)))
    );
    assert!(!prompt(&requests[2]).contains(guideline));
}

#[tokio::test]
async fn read_image_describes_exactly_while_the_runs_model_cannot_see() {
    let harness = Harness::new().await;
    let runtime = harness.runtime().await;
    let task = harness.create_task().await;
    // Loaded without vision first, as a side question or `/compact` loads
    // a task.
    let row = runtime.store.get(&task).unwrap().unwrap();
    runtime
        .task_session(
            &row,
            runtime.pi_model(&runtime.model, None, Some(false)).await,
        )
        .await
        .unwrap();
    harness.faux.push_text("Seen.");
    harness.faux.push_text("Seen again.");
    for vision in [true, false] {
        let mut request = harness.request(&task, "Look.");
        request.vision_capable = vision;
        let mut run = harness.handle.send_message(request).await.unwrap();
        assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    }

    // A run whose model sees images gets them from read_image; one whose
    // model cannot gets descriptions, focused by a context it must give.
    let required: Vec<serde_json::Value> = harness
        .faux
        .requests()
        .iter()
        .map(|request| {
            pi_ai::transcript::current_tools(&request.context.messages)
                .into_iter()
                .find(|tool| tool.name == "read_image")
                .unwrap()
                .parameters["required"]
                .clone()
        })
        .collect();
    assert_eq!(required, [json!(["source"]), json!(["source", "context"])]);
}

/// One catalog lookup gives a model's vision and window: an empty entry
/// for a model the catalog does not list, and none while it cannot be read.
#[tokio::test]
async fn the_catalog_gives_a_models_vision_and_window() {
    let harness = Harness::new().await;
    assert_eq!(
        harness.handle.model_catalog_entry("glm-5-3").await.unwrap(),
        None
    );
    harness
        .serve_catalog(json!({
            "object": "list",
            "data": [{"id": "glm-5-3", "context_window": 64000, "capabilities": {"vision": true}}],
            "aliases": [],
        }))
        .await;
    assert_eq!(
        harness.handle.model_catalog_entry("glm-5-3").await.unwrap(),
        Some(CatalogEntry {
            context_window: Some(64_000),
            vision: Some(true),
            ..CatalogEntry::default()
        })
    );
    assert_eq!(
        harness.handle.model_catalog_entry("missing").await.unwrap(),
        Some(CatalogEntry::default())
    );
    assert_eq!(
        harness
            .handle
            .context_limit_for_model("glm-5-3")
            .await
            .unwrap(),
        Some(64_000)
    );
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
    // Signed out, the session renames nothing.
    let signed_out = test_maple_api_session(USER);
    signed_out.invalidate_for_test().await;
    let refused = harness
        .handle
        .rename_session(
            signed_out,
            AgentRenameSessionRequest {
                session_id: task.clone(),
                title: "Other".to_string(),
            },
        )
        .await
        .err()
        .unwrap();
    assert!(refused.contains("no longer active"), "{refused}");
    let archived = harness
        .handle
        .set_session_state(task.clone(), AgentTaskState::Archived)
        .await
        .unwrap();
    assert_eq!(archived.state, AgentTaskState::Archived);

    let store = harness.handle.store().unwrap();
    assert!(!store.timeline(&task).unwrap().is_empty());
    harness.handle.delete_session(task.clone()).await.unwrap();
    assert!(store.get(&task).unwrap().is_none());
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

fn http_mcp_server(name: &str, url: &str, enabled: bool) -> AgentMcpServer {
    AgentMcpServer {
        name: name.into(),
        description: format!("{name} tools"),
        enabled,
        timeout_seconds: 30,
        transport: AgentMcpTransport::StreamableHttp {
            url: url.into(),
            environment: Vec::new(),
            headers: Vec::new(),
        },
    }
}

fn switch(task: &str, name: &str, enabled: bool) -> AgentSetSessionMcpServerRequest {
    AgentSetSessionMcpServerRequest {
        session_id: task.to_string(),
        kind: AgentSessionIntegrationKind::Mcp,
        name: name.to_string(),
        enabled,
    }
}

/// The menu's rows for the account's MCP servers; built-in CUA's comes
/// last and has tests of its own.
fn mcp_rows(rows: &[AgentSessionMcpServer]) -> Vec<(&str, bool, bool)> {
    rows.iter()
        .filter(|row| row.name != "cua-driver")
        .map(|row| (row.name.as_str(), row.enabled, row.available))
        .collect()
}

#[tokio::test]
async fn project_mcp_defaults_inherit_settings_until_explicitly_pinned() {
    let harness = Harness::new().await;
    let project = path_string(&normalize_project_root(harness.project.path()).unwrap());
    let other_project = tempfile::tempdir().unwrap();
    let other_root = path_string(other_project.path());
    let catalog = vec![
        http_mcp_server("Files", "https://mcp.example/files", true),
        http_mcp_server("Docs", "https://mcp.example/docs", false),
    ];
    harness
        .handle
        .save_mcp_servers(catalog.clone())
        .await
        .unwrap();
    assert_eq!(
        harness
            .handle
            .list_project_mcp_servers(project.clone())
            .await
            .unwrap(),
        catalog
    );

    // Pinning the current value is explicit too: later Settings edits cannot
    // change it, but untouched servers continue to inherit live defaults.
    harness
        .handle
        .set_project_mcp_server_enabled(project.clone(), " files ".into(), true)
        .await
        .unwrap();
    let mut updated = catalog;
    updated[0].enabled = false;
    updated[1].enabled = true;
    updated.push(http_mcp_server("New", "https://mcp.example/new", true));
    harness
        .handle
        .save_mcp_servers(updated.clone())
        .await
        .unwrap();
    let mut expected = updated.clone();
    expected[0].enabled = true;
    assert_eq!(
        harness
            .handle
            .list_project_mcp_servers(project.clone())
            .await
            .unwrap(),
        expected
    );
    assert_eq!(
        harness
            .handle
            .list_project_mcp_servers(other_root)
            .await
            .unwrap(),
        updated
    );
    harness
        .handle
        .set_project_mcp_server_enabled(project.clone(), "Files".into(), false)
        .await
        .unwrap();
    expected[0].enabled = false;
    assert_eq!(
        harness
            .handle
            .list_project_mcp_servers(project)
            .await
            .unwrap(),
        expected
    );
}

#[tokio::test]
async fn project_mcp_defaults_snapshot_new_tasks_without_changing_existing_tasks() {
    let mut harness = Harness::new().await;
    let root = path_string(harness.project.path());
    harness
        .handle
        .save_mcp_servers(vec![
            http_mcp_server("Files", "https://mcp.example/files", true),
            http_mcp_server("Docs", "https://mcp.example/docs", false),
        ])
        .await
        .unwrap();
    let old = harness.create_task().await;
    harness
        .handle
        .set_project_mcp_server_enabled(root.clone(), "Files".into(), false)
        .await
        .unwrap();
    harness
        .handle
        .set_project_mcp_server_enabled(root.clone(), "Docs".into(), true)
        .await
        .unwrap();
    let new = harness.create_task().await;
    assert_eq!(
        mcp_rows(
            &harness
                .handle
                .list_session_mcp_servers(old.clone())
                .await
                .unwrap()
        ),
        [("Files", true, true), ("Docs", false, true)]
    );
    assert_eq!(
        mcp_rows(
            &harness
                .handle
                .list_session_mcp_servers(new.clone())
                .await
                .unwrap()
        ),
        [("Files", false, true), ("Docs", true, true)]
    );

    let other_project = tempfile::tempdir().unwrap();
    let request = |project_root, names| AgentCreateSessionRequest {
        project_root: Some(project_root),
        title: None,
        model: None,
        context_limit: None,
        mcp_server_names: names,
    };
    let other = harness
        .handle
        .create_session(Some(request(path_string(other_project.path()), None)))
        .await
        .unwrap()
        .session
        .id;
    assert_eq!(
        mcp_rows(
            &harness
                .handle
                .list_session_mcp_servers(other)
                .await
                .unwrap()
        ),
        [("Files", true, true), ("Docs", false, true)]
    );
    let explicit = harness
        .handle
        .create_session(Some(request(root.clone(), Some(vec!["Files".into()]))))
        .await
        .unwrap()
        .session
        .id;
    assert_eq!(
        mcp_rows(
            &harness
                .handle
                .list_session_mcp_servers(explicit)
                .await
                .unwrap()
        ),
        [("Files", true, true), ("Docs", false, true)]
    );
    let empty = harness
        .handle
        .create_session(Some(request(root.clone(), Some(Vec::new()))))
        .await
        .unwrap()
        .session
        .id;
    assert_eq!(
        mcp_rows(
            &harness
                .handle
                .list_session_mcp_servers(empty)
                .await
                .unwrap()
        ),
        [("Files", false, true), ("Docs", false, true)]
    );

    harness
        .handle
        .set_session_mcp_server_enabled(switch(&new, "Docs", false))
        .await
        .unwrap();
    assert!(
        harness
            .handle
            .list_project_mcp_servers(root.clone())
            .await
            .unwrap()[1]
            .enabled
    );
    harness.restart().await;
    harness.start().await;
    assert_eq!(
        mcp_rows(&harness.handle.list_session_mcp_servers(old).await.unwrap()),
        [("Files", true, true), ("Docs", false, true)]
    );
    assert_eq!(
        mcp_rows(&harness.handle.list_session_mcp_servers(new).await.unwrap()),
        [("Files", false, true), ("Docs", false, true)]
    );
    let restarted = harness.create_task().await;
    assert_eq!(
        mcp_rows(
            &harness
                .handle
                .list_session_mcp_servers(restarted)
                .await
                .unwrap()
        ),
        [("Files", false, true), ("Docs", true, true)]
    );
}

#[tokio::test]
async fn project_mcp_defaults_are_private_account_local_and_reject_invalid_changes() {
    let harness = Harness::new().await;
    let root = path_string(&normalize_project_root(harness.project.path()).unwrap());
    let catalog = vec![http_mcp_server("Files", "https://mcp.example/files", true)];
    harness
        .handle
        .save_mcp_servers(catalog.clone())
        .await
        .unwrap();
    harness
        .handle
        .set_project_mcp_server_enabled(root.clone(), "Files".into(), false)
        .await
        .unwrap();
    let other = harness
        .service
        .handle_for_user("other@example.com")
        .await
        .unwrap();
    other.save_mcp_servers(catalog.clone()).await.unwrap();
    assert_eq!(
        other.list_project_mcp_servers(root.clone()).await.unwrap(),
        catalog
    );

    let sql = harness.handle.store().unwrap();
    let saved = sql.store().project(sql.account(), &root).unwrap().unwrap();
    assert_eq!(saved.mcp_defaults, json!({"files": false}));
    let config = account_generated_resource_dir(harness.handle.paths(), USER)
        .unwrap()
        .join("config.json");
    assert!(!config.exists());
    harness
        .handle
        .save_config(AgentConfig::default())
        .await
        .unwrap();
    assert!(
        !harness
            .handle
            .list_project_mcp_servers(root.clone())
            .await
            .unwrap()[0]
            .enabled
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let local = sql.store().paths().database();
        assert_eq!(
            std::fs::metadata(&local).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(local.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let alias = harness.data.path().join("project-alias");
        symlink(harness.project.path(), &alias).unwrap();
        assert!(
            !harness
                .handle
                .list_project_mcp_servers(path_string(&alias))
                .await
                .unwrap()[0]
                .enabled
        );
    }
    for name in ["", "Developer", "cua-driver", "Gone"] {
        assert!(
            harness
                .handle
                .set_project_mcp_server_enabled(root.clone(), name.into(), true)
                .await
                .is_err()
        );
    }
    let missing = path_string(&harness.data.path().join("missing"));
    assert!(
        harness
            .handle
            .list_project_mcp_servers(missing.clone())
            .await
            .is_err()
    );
    assert!(
        harness
            .handle
            .set_project_mcp_server_enabled(missing, "Files".into(), true)
            .await
            .is_err()
    );

    // Invalid values are rejected at the typed privileged boundary and cannot
    // replace the already saved project switches.
    assert!(
        sql.store()
            .set_project_mcp_defaults(sql.account(), &root, &json!({"files": "invalid"}), 1)
            .is_err()
    );
    assert_eq!(
        sql.store()
            .project(sql.account(), &root)
            .unwrap()
            .unwrap()
            .mcp_defaults,
        json!({"files": false})
    );
    harness
        .service
        .advance_generation(&account_scope(USER).unwrap());
    assert!(
        harness
            .handle
            .list_project_mcp_servers(root.clone())
            .await
            .unwrap_err()
            .contains("data changed")
    );
    assert!(
        harness
            .handle
            .set_project_mcp_server_enabled(root, "Files".into(), true)
            .await
            .unwrap_err()
            .contains("data changed")
    );
}

#[tokio::test]
async fn a_tasks_mcp_servers_give_the_model_their_tools() {
    let harness = Harness::new().await;
    let fake = mcp::fake_server::FakeServer::start(Some("Echo before you answer.")).await;
    harness
        .handle
        .save_mcp_servers(vec![
            http_mcp_server("Fake", &fake.url, false),
            http_mcp_server("Spare", &fake.url, false),
        ])
        .await
        .unwrap();

    harness
        .handle
        .set_project_mcp_server_enabled(path_string(harness.project.path()), "Fake".into(), true)
        .await
        .unwrap();

    // The project's override reaches actual MCP connections and model tools,
    // even when the global new-task default is off.
    let task = harness.create_task().await;
    let rows = harness
        .handle
        .list_session_mcp_servers(task.clone())
        .await
        .unwrap();
    assert_eq!(
        mcp_rows(&rows),
        [("Fake", true, true), ("Spare", false, true)]
    );

    harness.faux.push_message(vec![faux_tool_call(
        "mcp__fake__echo",
        json!({"text": "from the server"}),
    )]);
    harness.faux.push_text("It echoed.");
    let mut run = harness.send(&task, "Echo something").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);

    // The first request declared the server's tools and carried its
    // instructions; the call reached the server and its answer the model.
    let requests = harness.faux.requests();
    let declared: Vec<String> = pi_ai::transcript::current_tools(&requests[0].context.messages)
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    for name in ["mcp__fake__echo", "mcp__fake__fail", "read", "bash"] {
        assert!(declared.contains(&name.to_string()), "{declared:?}");
    }
    let system = pi_ai::transcript::current_system_prompt(&requests[0].context.messages);
    assert!(
        system.contains("## Fake (mcp__fake__*)\nEcho before you answer."),
        "{system}"
    );
    assert_eq!(
        fake.calls(),
        [("echo".to_string(), json!({"text": "from the server"}))]
    );
    let tool_result = requests[1]
        .context
        .messages
        .iter()
        .find_map(|message| match message {
            pi_ai::Message::ToolResult(result) => Some(pi_ai::content_text(&result.content)),
            _ => None,
        })
        .unwrap();
    assert_eq!(tool_result, "from the server");
    let live = harness.recorder.live_rows(&run.run_id);
    let tool_row = live.iter().find(|row| row.item_type == "tool").unwrap();
    assert_eq!(tool_row.title.as_deref(), Some("fake: echo"));

    // Switched off, its tools stay with the task, as in Pi a tool is never
    // taken from a session, and calling one fails. Its instructions leave.
    let rows = harness
        .handle
        .set_session_mcp_server_enabled(switch(&task, "Fake", false))
        .await
        .unwrap();
    assert_eq!(
        mcp_rows(&rows),
        [("Fake", false, true), ("Spare", false, true)]
    );
    harness.faux.push_message(vec![faux_tool_call(
        "mcp__fake__echo",
        json!({"text": "once more"}),
    )]);
    harness.faux.push_text("The server is off.");
    let mut run = harness.send(&task, "Again").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    let requests = harness.faux.requests();
    let asked = &requests[requests.len() - 2];
    let declared = pi_ai::transcript::current_tools(&asked.context.messages);
    assert!(
        declared.iter().any(|tool| tool.name == "mcp__fake__echo"),
        "{declared:?}"
    );
    assert!(
        !pi_ai::transcript::current_system_prompt(&asked.context.messages)
            .contains("Echo before you answer.")
    );
    assert_eq!(
        last_tool_result(&requests[requests.len() - 1]),
        ("MCP server \"Fake\" is disabled.".to_string(), true)
    );
    assert_eq!(fake.calls().len(), 1);

    // Switched on, a server connects at once. One removed from Settings
    // stays switched on but cannot run, and cannot be switched on again.
    let rows = harness
        .handle
        .set_session_mcp_server_enabled(switch(&task, "Spare", true))
        .await
        .unwrap();
    assert_eq!(mcp_rows(&rows)[1], ("Spare", true, true));
    harness
        .handle
        .save_mcp_servers(vec![http_mcp_server("Fake", &fake.url, true)])
        .await
        .unwrap();
    let rows = harness
        .handle
        .list_session_mcp_servers(task.clone())
        .await
        .unwrap();
    assert_eq!(
        mcp_rows(&rows),
        [("Fake", false, true), ("Spare", true, false)]
    );
    let rows = harness
        .handle
        .set_session_mcp_server_enabled(switch(&task, "Fake", true))
        .await
        .unwrap();
    assert_eq!(
        mcp_rows(&rows),
        [("Fake", true, true), ("Spare", true, false)]
    );
    let error = harness
        .handle
        .set_session_mcp_server_enabled(switch(&task, "Gone", true))
        .await
        .unwrap_err();
    assert_eq!(
        error,
        "MCP server 'Gone' is no longer configured and cannot be enabled"
    );
    let rows = harness
        .handle
        .set_session_mcp_server_enabled(switch(&task, "Spare", false))
        .await
        .unwrap();
    assert_eq!(mcp_rows(&rows), [("Fake", true, true)]);
}

/// The last tool result a request carries, and whether it failed.
fn last_tool_result(request: &pi_ai::faux::FauxRequest) -> (String, bool) {
    request
        .context
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            pi_ai::Message::ToolResult(result) => {
                Some((pi_ai::content_text(&result.content), result.is_error))
            }
            _ => None,
        })
        .unwrap()
}

#[tokio::test]
async fn a_tool_the_server_withdrew_stays_and_fails() {
    let harness = Harness::new().await;
    let fake = mcp::fake_server::FakeServer::start(None).await;
    harness
        .handle
        .save_mcp_servers(vec![http_mcp_server("Fake", &fake.url, true)])
        .await
        .unwrap();
    let task = harness.create_task().await;
    harness
        .faux
        .push_message(vec![faux_tool_call("mcp__fake__shrink", json!({}))]);
    harness.faux.push_text("Shrunk.");
    let mut run = harness.send(&task, "Shrink").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    let runtime = harness.service.state.runtime.lock().await.clone().unwrap();
    let mcp = runtime.loaded_mcp(&task).await.unwrap();
    eventually(|| !mcp.offers("fake", "echo")).await;

    // As in Pi, the withdrawn tool stays declared, and calling it fails
    // without reaching the server.
    harness.faux.push_message(vec![faux_tool_call(
        "mcp__fake__echo",
        json!({"text": "still there?"}),
    )]);
    harness.faux.push_text("It is gone.");
    let mut run = harness.send(&task, "Echo").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    let requests = harness.faux.requests();
    let declared = pi_ai::transcript::current_tools(&requests[requests.len() - 2].context.messages);
    assert!(
        declared.iter().any(|tool| tool.name == "mcp__fake__echo"),
        "{declared:?}"
    );
    assert_eq!(
        last_tool_result(&requests[requests.len() - 1]),
        (
            "MCP tool \"Fake/echo\" is no longer available.".to_string(),
            true
        )
    );
    assert!(fake.calls().iter().all(|(name, _)| name != "echo"));
}

#[tokio::test]
async fn only_the_idle_tasks_used_last_keep_their_servers() {
    let harness = Harness::new().await;
    let fake = mcp::fake_server::FakeServer::start(None).await;
    harness
        .handle
        .save_mcp_servers(vec![http_mcp_server("Fake", &fake.url, true)])
        .await
        .unwrap();
    let runtime = harness.service.state.runtime.lock().await.clone().unwrap();
    let mut tasks = Vec::new();
    for _ in 0..=mcp::MAX_IDLE_TASKS_WITH_SERVERS + 1 {
        let task = harness.create_task().await;
        harness.faux.push_text("Done.");
        let mut run = harness.send(&task, "Hello").await;
        assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
        tasks.push(task);
    }
    let initialized = || {
        fake.methods()
            .iter()
            .filter(|method| *method == "initialize")
            .count()
    };
    assert_eq!(initialized(), tasks.len());
    let has_servers = |mcp: Option<Arc<mcp::TaskMcp>>| mcp.unwrap().last_used().is_some();
    // The task idle longest stopped its servers when the last one ran.
    assert!(!has_servers(runtime.loaded_mcp(&tasks[0]).await));
    for task in &tasks[1..] {
        assert!(has_servers(runtime.loaded_mcp(task).await));
    }

    // Run again, it starts them anew, and the next idlest stops its own.
    harness.faux.push_text("Back.");
    let mut run = harness.send(&tasks[0], "Again").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    assert_eq!(initialized(), tasks.len() + 1);
    assert!(has_servers(runtime.loaded_mcp(&tasks[0]).await));
    assert!(!has_servers(runtime.loaded_mcp(&tasks[1]).await));
}

#[tokio::test]
async fn a_server_that_cannot_connect_is_reported_once_and_cannot_be_switched_on() {
    let harness = Harness::new().await;
    let broken = AgentMcpServer {
        name: "Broken".into(),
        description: String::new(),
        enabled: true,
        timeout_seconds: 30,
        transport: AgentMcpTransport::Stdio {
            command: "maple-test-no-such-mcp-server".into(),
            environment: Vec::new(),
        },
    };
    harness.handle.save_mcp_servers(vec![broken]).await.unwrap();
    let task = harness.create_task().await;
    harness.faux.push_text("Fine without it.");
    let mut run = harness.send(&task, "Hello").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    let warnings: Vec<String> = harness
        .recorder
        .run_events(&run.run_id)
        .into_iter()
        .filter_map(|event| match event {
            AgentRunEvent::SetupWarning(warning) => Some(warning),
            _ => None,
        })
        .collect();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].starts_with(
            "Some MCP servers could not connect: Broken: could not start maple-test-no-such-mcp-server"
        ),
        "{warnings:?}"
    );

    // The next run tries again quietly.
    harness.faux.push_text("Still fine.");
    let mut run = harness.send(&task, "Again").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    assert!(
        !harness
            .recorder
            .run_events(&run.run_id)
            .iter()
            .any(|event| matches!(event, AgentRunEvent::SetupWarning(_)))
    );

    // Switched off and on again, it is tried at once, and stays off.
    harness
        .handle
        .set_session_mcp_server_enabled(switch(&task, "Broken", false))
        .await
        .unwrap();
    let error = harness
        .handle
        .set_session_mcp_server_enabled(switch(&task, "Broken", true))
        .await
        .unwrap_err();
    assert!(
        error.starts_with("Failed to connect MCP server 'Broken': could not start"),
        "{error}"
    );
    let rows = harness
        .handle
        .list_session_mcp_servers(task.clone())
        .await
        .unwrap();
    assert_eq!(mcp_rows(&rows), [("Broken", false, true)]);
    // An external agent is switched on in Settings first, and built-in CUA
    // set up there first.
    let error = harness
        .handle
        .set_session_mcp_server_enabled(AgentSetSessionMcpServerRequest {
            kind: AgentSessionIntegrationKind::ExternalAgent,
            ..switch(&task, "codex", true)
        })
        .await
        .unwrap_err();
    assert!(
        error.starts_with("Enable this integration in Settings"),
        "{error}"
    );
    let error = harness
        .handle
        .set_session_mcp_server_enabled(switch(&task, "cua-driver", true))
        .await
        .unwrap_err();
    assert!(
        error.starts_with("Set up computer use in Settings"),
        "{error}"
    );
}

#[tokio::test]
async fn integrations_list_and_refuse_what_cannot_be_enabled() {
    let harness = Harness::new().await;
    let cards = harness.handle.list_integrations().await.unwrap();
    let ids: Vec<&str> = cards.iter().map(|card| card.id.as_str()).collect();
    assert_eq!(ids, ["cua-driver", "codex", "claude"]);
    // Built-in CUA reads as this device has it: not available without the
    // SDK, and needing setup until every permission is granted.
    let readiness = cua::readiness();
    assert_eq!(
        cards[0].availability,
        match &readiness {
            None => AgentIntegrationAvailability::NotDetected,
            Some(readiness) if readiness.permissions.ready() => {
                AgentIntegrationAvailability::Available
            }
            Some(_) => AgentIntegrationAvailability::SetupRequired,
        }
    );
    assert_eq!(cards[0].backend, None);
    let ready = cua::ready();

    let enabled = harness
        .handle
        .set_integration_enabled(AgentSetIntegrationEnabledRequest {
            id: "cua-driver".into(),
            enabled: true,
        })
        .await;
    match enabled {
        Ok(cards) => {
            assert!(ready);
            assert!(cards[0].enabled_for_new_tasks);
        }
        Err(error) => {
            assert!(!ready);
            assert!(error.contains("permissions"), "{error}");
        }
    }
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
    // Setup records the built-in backend only once the grants are in.
    assert_eq!(
        cards[0].backend,
        ready.then_some(AgentIntegrationBackend::Embedded)
    );
}

/// Save built-in CUA as set up on the device, as Settings does once its
/// permissions are granted, on or off for new tasks.
fn set_up_cua(harness: &Harness, on_for_new_tasks: bool) {
    let store = harness.handle.store().unwrap();
    store
        .store()
        .set_account_setting(
            store.account(),
            "agent.integrations",
            &json!({
                "version": 2,
                "integrations": [
                    {"id": "cua-driver", "enabled": on_for_new_tasks, "backend": "embedded"}
                ]
            }),
            1,
        )
        .unwrap();
}

fn cua_row(rows: &[AgentSessionMcpServer]) -> (&str, bool, bool) {
    let row = rows.iter().find(|row| row.name == "cua-driver").unwrap();
    (row.transport.as_str(), row.enabled, row.available)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn built_in_cua_follows_the_device_and_the_tasks_switch() {
    let harness = Harness::new().await;
    let runtime = harness.runtime().await;
    let switch = |task: &str, enabled| AgentSetSessionMcpServerRequest {
        session_id: task.to_string(),
        name: "cua-driver".to_string(),
        kind: AgentSessionIntegrationKind::Mcp,
        enabled,
    };
    let create = |names: Option<Vec<String>>| {
        harness
            .handle
            .create_session(Some(AgentCreateSessionRequest {
                project_root: None,
                title: None,
                model: None,
                context_limit: None,
                mcp_server_names: names,
            }))
    };

    // Not set up on the device: the row says so, and the switch refuses.
    let task = harness.create_task().await;
    let rows = harness
        .handle
        .list_session_mcp_servers(task.clone())
        .await
        .unwrap();
    assert_eq!(cua_row(&rows), ("unconfigured", false, false));
    let error = harness
        .handle
        .set_session_mcp_server_enabled(switch(&task, true))
        .await
        .unwrap_err();
    assert!(
        error.starts_with("Set up computer use in Settings"),
        "{error}"
    );

    // Set up and off for new tasks: a new task has it off, and available
    // as far as the device's permissions go.
    let ready = cua::ready();
    set_up_cua(&harness, false);
    let off = create(None).await.unwrap().session.id;
    let rows = harness.handle.list_session_mcp_servers(off).await.unwrap();
    assert_eq!(cua_row(&rows), ("embedded", false, ready));

    // On for new tasks: a new task has it on, unless the composer named
    // its servers without it.
    set_up_cua(&harness, true);
    let on = create(None).await.unwrap().session.id;
    let rows = harness
        .handle
        .list_session_mcp_servers(on.clone())
        .await
        .unwrap();
    assert_eq!(cua_row(&rows), ("embedded", true, ready));
    let named_others = create(Some(Vec::new())).await.unwrap().session.id;
    let rows = harness
        .handle
        .list_session_mcp_servers(named_others)
        .await
        .unwrap();
    assert_eq!(cua_row(&rows), ("embedded", false, ready));

    // Switched off, the task keeps that choice.
    let rows = harness
        .handle
        .set_session_mcp_server_enabled(switch(&on, false))
        .await
        .unwrap();
    assert_eq!(cua_row(&rows), ("embedded", false, ready));
    if !ready {
        let error = harness
            .handle
            .set_session_mcp_server_enabled(switch(&on, true))
            .await
            .unwrap_err();
        assert!(
            error.contains("permission") || error.contains("not available"),
            "{error}"
        );
        return;
    }

    // Where CUA can run, switching it on for a loaded task binds the task:
    // its session gets the tools.
    let row = runtime.store.get(&on).unwrap().unwrap();
    let session = runtime
        .task_session(
            &row,
            runtime.pi_model(&runtime.model, None, Some(false)).await,
        )
        .await
        .unwrap();
    let cua_tools = |session: &pi_coding_agent::AgentSession| {
        session
            .all_tools()
            .into_iter()
            .map(|tool| tool.name)
            .filter(|name| name.starts_with("cua-driver__"))
            .collect::<Vec<_>>()
    };
    assert!(cua_tools(&session).is_empty());
    let rows = harness
        .handle
        .set_session_mcp_server_enabled(switch(&on, true))
        .await
        .unwrap();
    assert_eq!(cua_row(&rows), ("embedded", true, true));
    let tools = cua_tools(&session);
    assert!(!tools.is_empty());
    assert!(!tools.iter().any(|name| name.ends_with("__start_session")));

    // Switched off, the binding closes. Its tools stay with the session, as
    // in Pi a tool is never taken from one, and calling one fails.
    harness
        .handle
        .set_session_mcp_server_enabled(switch(&on, false))
        .await
        .unwrap();
    assert_eq!(cua_tools(&session), tools);
    let without_arguments = session
        .all_tools()
        .into_iter()
        .find(|tool| {
            tool.name.starts_with("cua-driver__")
                && tool.parameters["required"]
                    .as_array()
                    .is_none_or(Vec::is_empty)
        })
        .unwrap();
    harness
        .faux
        .push_message(vec![faux_tool_call(&without_arguments.name, json!({}))]);
    harness.faux.push_text("Computer use is off.");
    let mut run = harness.send(&on, "Look at the screen").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    let requests = harness.faux.requests();
    assert_eq!(
        last_tool_result(&requests[requests.len() - 1]),
        (
            "Computer use is not connected to this task.".to_string(),
            true
        )
    );
}

#[tokio::test]
async fn answers_reach_the_question_broker_of_the_service() {
    let harness = Harness::new().await;
    let broker = harness.service.question_broker();
    assert!(!harness.service.answer_question("unknown", "x".into()).await);
    drop(broker);
}

#[tokio::test]
async fn pending_question_snapshots_respect_account_task_and_generation_authorization() {
    let data = tempfile::tempdir().unwrap();
    let service = service(data.path(), Arc::new(Recorder::default()));
    let alice = service.handle_for_user("synthetic-alice").await.unwrap();
    let bob = service.handle_for_user("synthetic-bob").await.unwrap();
    let store = service.host.store.as_ref().unwrap();
    let alice_account = store.upsert_account("synthetic-alice", 1).unwrap();
    let bob_account = store.upsert_account("synthetic-bob", 1).unwrap();
    let task = |account| {
        let uuid = uuid::Uuid::new_v4().to_string();
        store
            .create_task(
                account,
                &crate::store::NewTask {
                    uuid: uuid.clone(),
                    project_root: data.path().to_string_lossy().into_owned(),
                    kind: crate::store::TaskKind::Desktop,
                    title: "Synthetic question task".into(),
                    provisional: false,
                    model: None,
                    created_ms: 1,
                },
            )
            .unwrap();
        uuid
    };
    let alice_task = task(alice_account);
    let bob_task = task(bob_account);
    let mut asks = Vec::new();
    for task in [alice_task.clone(), bob_task.clone()] {
        let broker = service.question_broker();
        asks.push(tokio::spawn(
            async move { broker.ask(&task, Vec::new()).await },
        ));
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while service.pending_questions().await.len() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let alice_questions = alice.pending_questions().await;
    let bob_questions = bob.pending_questions().await;
    assert_eq!(alice_questions.len(), 1);
    assert_eq!(bob_questions.len(), 1);
    assert!(
        matches!(&alice_questions[0],AgentServiceEvent::Question {session_id,..} if session_id==&alice_task)
    );
    assert!(
        matches!(&bob_questions[0],AgentServiceEvent::Question {session_id,..} if session_id==&bob_task)
    );
    store
        .mark_task_deleted(alice_account, &alice_task, 2)
        .unwrap();
    assert!(alice.pending_questions().await.is_empty());
    assert_eq!(bob.pending_questions().await.len(), 1);
    service.advance_generation(&bob.account_scope);
    assert!(bob.pending_questions().await.is_empty());
    let bob = service.handle_for_user("synthetic-bob").await.unwrap();
    store
        .begin_account_deletion(bob_account, &uuid::Uuid::new_v4().to_string(), 3)
        .unwrap();
    assert!(bob.pending_questions().await.is_empty());
    for ask in asks {
        ask.abort();
        let _ = ask.await;
    }
}

#[tokio::test]
async fn thinking_picks_persist_in_pi_and_default_new_tasks_after_restart() {
    use ThinkingLevel::{High, Low, Max};
    let mut harness = Harness::new().await;
    let catalog = json!({"object":"list","data":[{
        "id":"glm-5-3", "reasoning":{"mandatory":true,"supported_efforts":["max","high","low"]}
    }],"aliases":[]});
    harness.serve_catalog(catalog.clone()).await;
    assert_eq!(
        harness
            .handle
            .thinking_state(None, "glm-5-3")
            .await
            .unwrap(),
        AgentThinkingState {
            levels: vec![Low, High, Max],
            current: High
        }
    );
    let task = harness.create_task().await;
    let selected = harness
        .handle
        .set_thinking_level(Some(&task), "glm-5-3", Low)
        .await
        .unwrap();
    assert_eq!(selected.current, Low);
    assert_eq!(
        harness
            .handle
            .load_config()
            .await
            .unwrap()
            .default_thinking_level,
        Some(Low)
    );
    harness.faux.push_text("Done.");
    let mut run = harness.send(&task, "Hello").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    assert_eq!(
        harness
            .runtime()
            .await
            .loaded_session(&task)
            .await
            .unwrap()
            .thinking_level(),
        Low
    );
    // A later default affects new tasks, not the task's persisted choice.
    harness
        .handle
        .set_thinking_level(None, "glm-5-3", Max)
        .await
        .unwrap();
    harness.restart().await;
    harness.start().await;
    harness.serve_catalog(catalog).await;
    assert_eq!(
        harness
            .handle
            .thinking_state(Some(&task), "glm-5-3")
            .await
            .unwrap()
            .current,
        Low
    );
    let fresh = harness.create_task().await;
    assert_eq!(
        harness
            .handle
            .thinking_state(Some(&fresh), "glm-5-3")
            .await
            .unwrap()
            .current,
        Max
    );
    // A nonreasoning model offers and saves only Off.
    assert_eq!(
        harness
            .handle
            .set_thinking_level(None, "plain", Max)
            .await
            .unwrap()
            .current,
        ThinkingLevel::Off
    );
}

#[tokio::test]
async fn viewing_unloaded_task_thinking_and_history_does_not_load_context() {
    let harness = Harness::new().await;
    let task = harness.create_task().await;
    let runtime = harness.runtime().await;
    assert!(runtime.loaded_session(&task).await.is_none());
    let before = runtime
        .store
        .store()
        .task(runtime.store.account(), &task)
        .unwrap()
        .unwrap();
    harness
        .handle
        .thinking_state(Some(&task), "glm-5-3")
        .await
        .unwrap();
    assert!(
        harness
            .handle
            .load_session(task.clone())
            .await
            .unwrap()
            .timeline
            .is_empty()
    );
    assert!(runtime.loaded_session(&task).await.is_none());
    let after = runtime
        .store
        .store()
        .task(runtime.store.account(), &task)
        .unwrap()
        .unwrap();
    assert_eq!(before.last_seq, after.last_seq);
    assert_eq!(before.facts, after.facts);
}

#[tokio::test]
async fn sending_before_open_rebuilds_retired_history_before_run_admission() {
    let harness = Harness::new().await;
    let task = harness.create_task().await;
    harness.faux.push_text("Old reply stays.");
    let mut first = harness.send(&task, "Old question stays.").await;
    assert_eq!(finished(&mut first).await, AgentRunTerminal::Completed);
    let (store, account) = harness.handle.test_persistence().unwrap();
    store.retire_task_timeline_for_test(account, &task).unwrap();
    assert!(store.history_needs_rebuild(account, &task).unwrap());

    // Do not open/load this task after invalidation. Send must prepare its old
    // history while idle, before registering an active run that defers rebuilds.
    harness.faux.push_text("New reply appears.");
    let mut second = harness.send(&task, "New question appears.").await;
    assert_eq!(finished(&mut second).await, AgentRunTerminal::Completed);
    assert!(!store.history_needs_rebuild(account, &task).unwrap());
    let page = harness
        .handle
        .open_task_page(
            crate::store::OpenTaskPageRequest {
                task: task.clone(),
                limit_rows: None,
                limit_bytes: None,
            },
            crate::store::PageTransport::Remote,
        )
        .await
        .unwrap();
    let text = serde_json::to_string(&page.rows).unwrap();
    for expected in [
        "Old question stays.",
        "Old reply stays.",
        "New question appears.",
        "New reply appears.",
    ] {
        assert!(text.contains(expected), "missing {expected}: {text}");
    }
    let entries = store
        .load_session(account, &task, crate::store::SessionLoad::Full)
        .unwrap()
        .entries;
    let source = serde_json::to_string(&entries).unwrap();
    assert_eq!(source.matches("Old question stays.").count(), 1);
    assert_eq!(source.matches("New question appears.").count(), 1);
    let requests = harness.faux.requests();
    assert_eq!(
        user_texts(requests.last().unwrap()),
        ["Old question stays.", "New question appears."]
    );
}

async fn history_preparation_fixture() -> (
    Harness,
    String,
    Arc<crate::store::Store>,
    crate::store::AccountId,
) {
    let harness = Harness::new().await;
    let task = harness.create_task().await;
    harness.faux.push_text("Saved history stays visible.");
    let mut run = harness.send(&task, "Keep this history.").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    let (store, account) = harness.handle.test_persistence().unwrap();
    (harness, task, store, account)
}

#[tokio::test]
async fn history_preparation_retries_brief_lifecycle_contention() {
    let (harness, task, store, account) = history_preparation_fixture().await;
    let admission = harness.service.state.lifecycle.lock().await;
    store.retire_task_timeline_for_test(account, &task).unwrap();
    let handle = harness.handle.clone();
    let preparing_task = task.clone();
    let (started, entered) = tokio::sync::oneshot::channel();
    let preparation = tokio::task::spawn_blocking(move || {
        started.send(()).unwrap();
        handle.prepare_history_for_read(
            &preparing_task,
            std::time::Instant::now() + Duration::from_secs(5),
        )
    });
    entered.await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !preparation.is_finished(),
        "brief contention must not fail the open"
    );
    drop(admission);
    tokio::time::timeout(Duration::from_secs(6), preparation)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!store.history_needs_rebuild(account, &task).unwrap());
    let page = store
        .open_task_page(
            account,
            &crate::store::OpenTaskPageRequest {
                task,
                limit_rows: None,
                limit_bytes: None,
            },
            crate::store::PageTransport::Local,
        )
        .unwrap();
    assert!(
        serde_json::to_string(&page.rows)
            .unwrap()
            .contains("Saved history stays visible.")
    );
}

#[tokio::test]
async fn history_preparation_contention_stops_at_its_existing_deadline() {
    let (harness, task, store, account) = history_preparation_fixture().await;
    let _admission = harness.service.state.lifecycle.lock().await;
    store.retire_task_timeline_for_test(account, &task).unwrap();
    let stamp = store.task_history_stamp(account, &task).unwrap();
    let handle = harness.handle.clone();
    let preparing_task = task.clone();
    let budget = Duration::from_millis(150);
    let preparation = tokio::task::spawn_blocking(move || {
        let started = std::time::Instant::now();
        let result = handle.prepare_history_for_read(&preparing_task, started + budget);
        (started.elapsed(), result)
    });
    let (elapsed, result) = tokio::time::timeout(Duration::from_secs(2), preparation)
        .await
        .unwrap()
        .unwrap();
    assert!(elapsed >= budget);
    assert_eq!(
        result.unwrap_err(),
        "Maple is preparing this task's saved history. Try opening it again shortly."
    );
    assert_eq!(store.task_history_stamp(account, &task).unwrap(), stamp);
    assert!(store.history_needs_rebuild(account, &task).unwrap());
}

async fn history_preparation_idle_write<T>(
    mut operation: impl FnMut() -> Result<T, crate::store::StoreError>,
) -> T {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match operation() {
                Ok(value) => return value,
                Err(crate::store::StoreError::Busy) => {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                Err(error) => panic!("history fixture idle write failed: {error}"),
            }
        }
    })
    .await
    .expect("fixture writer contention clears")
}

#[tokio::test]
async fn history_preparation_rechecks_completion_before_starting_another_epoch() {
    let (harness, task, store, account) = history_preparation_fixture().await;
    let admission = harness.service.state.lifecycle.lock().await;
    store.retire_task_timeline_for_test(account, &task).unwrap();
    let handle = harness.handle.clone();
    let preparing_task = task.clone();
    let (started, entered) = tokio::sync::oneshot::channel();
    let preparation = tokio::task::spawn_blocking(move || {
        started.send(()).unwrap();
        handle.prepare_history_for_read(
            &preparing_task,
            std::time::Instant::now() + Duration::from_secs(5),
        )
    });
    entered.await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!preparation.is_finished());

    // Complete the same work while the foreground request waits for admission.
    // This test owns the lifecycle gate that normal background commits take.
    let ticket =
        history_preparation_idle_write(|| store.begin_timeline_rebuild(account, &task, false))
            .await;
    let prepared = store.prepare_timeline_rebuild(&ticket).unwrap();
    let completed_epoch = loop {
        let chunk = store.prepare_timeline_chunk(&prepared).unwrap();
        match history_preparation_idle_write(|| store.commit_timeline_chunk(&chunk, false)).await {
            crate::store::RebuildProgress::Pending { .. } => {}
            crate::store::RebuildProgress::Replaced { epoch } => break epoch,
            crate::store::RebuildProgress::NeedsSnapshot => panic!("source did not change"),
        }
    };
    drop(admission);
    tokio::time::timeout(Duration::from_secs(6), preparation)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        store.task_history_stamp(account, &task).unwrap().rows_epoch,
        completed_epoch
    );
    assert!(
        !harness
            .recorder
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(
                event, AgentServiceEvent::HistoryReplaced { session_id, .. } if session_id == &task
            )),
        "waiting foreground open must not schedule a redundant rebuild"
    );
}
