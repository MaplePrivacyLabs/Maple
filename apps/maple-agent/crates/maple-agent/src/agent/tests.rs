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
    /// The tasks' model.
    faux: FauxProvider,
    /// The side model of titles and summaries, scripted apart so a task's
    /// script does not depend on when they are asked.
    side: FauxProvider,
    data: tempfile::TempDir,
    project: tempfile::TempDir,
}

/// Side-model requests to their own script, the rest to the task's.
struct SplitStream {
    task: FauxProvider,
    side: FauxProvider,
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
    MapleAgentService::new(MapleAgentHostResources::new(
        // A home of its own, so no skill or instruction file of the real
        // one reaches the tasks.
        AgentPathLayout::from_app_roots(data.join("config"), data.join("local"))
            .with_home(Some(data.join("home"))),
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
    assert_eq!(request.model.id, side_models::SIDE_MODEL);
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
async fn tool_calls_and_thinking_are_summarized_by_the_side_model() {
    let harness = Harness::new().await;
    harness.side.push_text("\"Listed the files\"\nextra");
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
        .summarize_thinking(&task, "I should read main.rs before editing.")
        .await
        .unwrap();
    assert_eq!(summary.as_deref(), Some("Decided to read main.rs first"));
    let requests = harness.side.requests();
    assert_eq!(
        last_user_text(&requests[0]),
        "Tool: bash\nInput: {\"command\":\"ls\"}\nOutput: main.rs"
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

/// A 1x1 PNG.
const PNG_1X1: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

fn with_image(mut request: AgentSendMessageRequest, name: &str) -> AgentSendMessageRequest {
    request.attachments = vec![AgentImageUpload {
        name: name.to_string(),
        data_url: format!("data:image/png;base64,{PNG_1X1}"),
    }];
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

    let harness =
        Harness::with_faux(FauxProvider::new().with_chunk_delay(Duration::from_millis(10))).await;
    harness
        .faux
        .push_text("A dialog, streamed slowly enough to queue behind.");
    harness.faux.push_text("Also a dialog.");
    let task = harness.create_task().await;
    let first = with_image(harness.request(&task, "What is this?"), "dialog.png");
    let mut run = harness.handle.send_message(first).await.unwrap();
    // A message sent during the run waits as a chip with its image.
    let mut second = with_image(harness.request(&task, "And this?"), "second.png");
    second.vision_capable = true;
    let staged = harness.handle.send_message(second).await.unwrap();
    assert_eq!(staged.queued.unwrap().attachments[0].name, "second.png");
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);

    // A model without vision gets the image by source, to look with
    // read_image; one with vision gets it beside the text.
    let requests = harness.faux.requests();
    let first = last_user(&requests[0]);
    let text = pi_ai::content_text(&first.content);
    assert!(
        text.starts_with(
            "What is this?\n\nThe user attached the following image:\n- \"dialog.png\": maple-attachment://"
        ),
        "{text}"
    );
    assert!(text.ends_with("when you need visual details from that image."));
    assert_eq!(first.content.len(), 1);
    let second = last_user(&requests[1]);
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
                    .and_then(|input| input["imageAttachments"].as_array().cloned())
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

fn mcp_rows(rows: &[AgentSessionMcpServer]) -> Vec<(&str, bool, bool)> {
    rows.iter()
        .map(|row| (row.name.as_str(), row.enabled, row.available))
        .collect()
}

#[tokio::test]
async fn a_tasks_mcp_servers_give_the_model_their_tools() {
    let harness = Harness::new().await;
    let fake = mcp::fake_server::FakeServer::start(Some("Echo before you answer.")).await;
    harness
        .handle
        .save_mcp_servers(vec![
            http_mcp_server("Fake", &fake.url, true),
            http_mcp_server("Spare", &fake.url, false),
        ])
        .await
        .unwrap();

    // A new task gets the servers switched on for new tasks.
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

    // Switched off, its tools leave the task's next prompt.
    let rows = harness
        .handle
        .set_session_mcp_server_enabled(switch(&task, "Fake", false))
        .await
        .unwrap();
    assert_eq!(
        mcp_rows(&rows),
        [("Fake", false, true), ("Spare", false, true)]
    );
    harness.faux.push_text("No tools now.");
    let mut run = harness.send(&task, "Again").await;
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
    let last = harness.faux.requests().pop().unwrap();
    let declared = pi_ai::transcript::current_tools(&last.context.messages);
    assert!(
        !declared.iter().any(|tool| tool.name.starts_with("mcp__")),
        "{declared:?}"
    );
    assert!(
        !pi_ai::transcript::current_system_prompt(&last.context.messages)
            .contains("Echo before you answer.")
    );

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
    // Integrations that do not run in tasks yet cannot be switched on.
    let error = harness
        .handle
        .set_session_mcp_server_enabled(AgentSetSessionMcpServerRequest {
            kind: AgentSessionIntegrationKind::ExternalAgent,
            ..switch(&task, "codex", true)
        })
        .await
        .unwrap_err();
    assert_eq!(
        error,
        "This feature is not available in this build of Maple yet"
    );
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
