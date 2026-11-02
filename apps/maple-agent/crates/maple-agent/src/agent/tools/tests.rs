const TASK_ID: &str = "018ddbcc-b80b-7c20-8123-000000000001";

use std::path::PathBuf;

use pi_agent_core::{ToolInvocation, ToolUpdates};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::agent::{AgentEventDispatcher, AgentEventSink, AgentServiceEvent};

struct NullSink;

impl AgentEventSink for NullSink {
    fn emit(&self, _event: &AgentServiceEvent) {}
}

fn broker() -> QuestionBroker {
    QuestionBroker::new(AgentEventDispatcher::new(Arc::new(NullSink)))
}

/// A web provider no test reaches.
struct NoWeb;

#[async_trait::async_trait]
impl MapleWebTransport for NoWeb {
    async fn web_search(
        self: Arc<Self>,
        _request: maple_sdk::WebSearchRequest,
        _cancel_token: CancellationToken,
    ) -> maple_sdk::Result<maple_sdk::WebSearchResponse> {
        Err(maple_sdk::Error::Other("no web in tests".to_string()))
    }

    async fn web_extract(
        self: Arc<Self>,
        _request: maple_sdk::WebExtractRequest,
        _cancel_token: CancellationToken,
    ) -> maple_sdk::Result<maple_sdk::WebExtractResponse> {
        Err(maple_sdk::Error::Other("no web in tests".to_string()))
    }
}

/// A task of `kind`: no login PATH, a broker nobody answers, and the web
/// switch on.
fn task(kind: TaskKind) -> (tempfile::TempDir, TaskToolsFor) {
    let dir = tempfile::tempdir().unwrap();
    let attachments = Arc::new(crate::agent::attachments::fixture_attachment_store(
        dir.path(),
        &[TASK_ID],
    ));
    (
        dir,
        TaskToolsFor {
            session_id: TASK_ID.to_string(),
            kind,
            login_path: None,
            questions: broker(),
            web: Arc::new(NoWeb),
            web_enabled: true,
            read_image: ReadImageFor {
                session_id: TASK_ID.to_string(),
                cwd: PathBuf::from("/work"),
                attachments,
                models: pi_coding_agent::ModelRegistry::new(Arc::new(
                    pi_coding_agent::StaticKeys::default(),
                )),
                describe: false,
            },
        },
    )
}

fn spawn(env: &[(&str, &str)]) -> BashSpawnContext {
    BashSpawnContext {
        command: "true".to_string(),
        cwd: PathBuf::from("/work"),
        env: env
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect(),
    }
}

#[test]
fn commands_get_the_login_path_and_the_task_id() {
    let (_dir, task) = task(TaskKind::Desktop);
    let tools = task_tools(TaskToolsFor {
        login_path: Some("/login/bin:/usr/bin".to_string()),
        ..task
    });
    let hook = tools.options.bash.spawn_hook.unwrap();
    let started = hook(spawn(&[("PATH", "/usr/bin"), ("HOME", "/home/me")]));
    assert_eq!(
        started.env.get("PATH").map(String::as_str),
        Some("/login/bin:/usr/bin")
    );
    assert_eq!(
        started.env.get("AGENT_SESSION_ID").map(String::as_str),
        Some(TASK_ID)
    );
    assert_eq!(
        started.env.get("HOME").map(String::as_str),
        Some("/home/me")
    );
}

#[test]
fn without_a_login_path_commands_keep_the_path_they_start_with() {
    let (_dir, task) = task(TaskKind::Acp);
    let tools = task_tools(task);
    let hook = tools.options.bash.spawn_hook.unwrap();
    let started = hook(spawn(&[("PATH", "/client/bin:/usr/bin")]));
    assert_eq!(
        started.env.get("PATH").map(String::as_str),
        Some("/client/bin:/usr/bin")
    );
}

#[test]
fn the_model_gets_read_a_shell_edit_and_write() {
    let (_dir, task) = task(TaskKind::Desktop);
    let tools = task_tools(task);
    let shell = if cfg!(windows) {
        tools.builtin[1].as_str()
    } else {
        "bash"
    };
    assert_eq!(tools.builtin, ["read", shell, "edit", "write"]);
    assert!(tools.options.powershell.spawn_hook.is_some());
    assert!(tools.options.bash.operations.is_some());
    assert!(tools.options.powershell.operations.is_some());
}

fn tool(tools: &TaskTools, name: &str) -> Arc<dyn pi_agent_core::AgentTool> {
    tools
        .maple
        .iter()
        .find(|tool| tool.tool.name() == name)
        .map(|tool| tool.tool.clone())
        .unwrap()
}

fn desktop_tools() -> (tempfile::TempDir, TaskTools) {
    let (dir, task) = task(TaskKind::Desktop);
    (dir, task_tools(task))
}

fn maple_tool_names(tools: &TaskTools) -> Vec<(String, bool)> {
    tools
        .maple
        .iter()
        .map(|tool| (tool.tool.name().to_string(), tool.active))
        .collect()
}

#[test]
fn every_task_reads_images_and_the_web_and_desktop_tasks_plan_and_ask() {
    let on = |name: &str| (name.to_string(), true);
    let (_desktop_dir, desktop) = desktop_tools();
    assert_eq!(
        maple_tool_names(&desktop),
        [
            on("read_image"),
            on("todo_write"),
            on("request_user_input"),
            on("web_search"),
            on("open_url")
        ]
    );
    let (_acp_dir, task) = task(TaskKind::Acp);
    let acp = task_tools(TaskToolsFor {
        web_enabled: false,
        ..task
    });
    assert_eq!(
        maple_tool_names(&acp),
        [
            on("read_image"),
            ("web_search".to_string(), false),
            ("open_url".to_string(), false)
        ]
    );
}

#[tokio::test]
async fn the_plan_is_echoed_to_the_model() {
    let (_dir, desktop) = desktop_tools();
    let todo = tool(&desktop, "todo_write");
    let args = json!({"todos": [{"content": "Read the code", "status": "in_progress"}]});
    let result = todo
        .execute(ToolInvocation {
            call_id: "call-1".to_string(),
            args: args.clone(),
            cancel: CancellationToken::new(),
            updates: ToolUpdates::none(),
        })
        .await
        .unwrap();
    let text = pi_ai::content_text(&result.content);
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), args);
}

#[test]
fn near_miss_questions_take_the_schema_shape() {
    let (_dir, desktop) = desktop_tools();
    let ask = tool(&desktop, "request_user_input");
    let prepare = |args: Value| {
        let Value::Object(map) = args else {
            unreachable!()
        };
        Value::Object(ask.prepare_arguments(map))
    };
    let expected = json!({"questions": [{
        "id": "",
        "header": "",
        "question": "Which folder?",
        "options": [{"label": "src", "description": ""}],
    }]});
    // One question at the top level, with an option string and extra keys.
    assert_eq!(
        prepare(
            json!({"question": "Which folder?", "options": [{"label": "src"}, "docs"], "why": 1})
        ),
        expected
    );
    // `questions` as one object.
    assert_eq!(
        prepare(json!({"questions": {"question": "Which folder?", "options": [{"label": "src"}]}})),
        expected
    );
    // Nothing to ask is left for validation to refuse.
    assert_eq!(prepare(json!({"other": 1})), json!({"other": 1}));
    let validated = pi_ai::validate_tool_arguments(
        ask.declaration(),
        &pi_ai::ToolCall {
            id: "call-1".to_string(),
            arguments_error: None,
            partial_json: None,
            name: "request_user_input".to_string(),
            arguments: match expected {
                Value::Object(map) => map,
                _ => Map::new(),
            },
        },
    );
    assert!(validated.is_ok(), "{validated:?}");
}

#[test]
fn questions_are_deduplicated_capped_and_defaulted() {
    let questions = desktop::parse_user_questions(&[
        json!({"id": "question_1", "question": "First?"}),
        json!({"question": "Second?"}),
        json!({"id": "question_1", "question": "Third?"}),
        json!({"id": "extra", "question": "Fourth?"}),
    ]);
    let ids: Vec<&str> = questions.iter().map(|q| q.id.as_str()).collect();
    assert_eq!(ids, ["question_1", "question_1_2", "question_1_3"]);
    assert_eq!(questions[1].question, "Second?");

    let questions = desktop::parse_user_questions(&[
        json!({"question": "  "}),
        json!({"id": " a ", "question": "Real?", "options": [{"label": " "}, {"label": "Yes"}]}),
        json!({"question": "Pick several", "multiSelect": true}),
        json!({"question": "Invalid flag", "multiSelect": "true"}),
    ]);
    assert_eq!(questions.len(), 3);
    assert_eq!(questions[0].id, "a");
    assert_eq!(questions[0].header, "Question");
    assert_eq!(questions[0].options.len(), 1);
    assert!(!questions[0].multi_select);
    assert!(questions[1].multi_select);
    assert!(!questions[2].multi_select);
}

#[tokio::test]
async fn an_unanswered_question_is_taken_back_when_the_run_stops() {
    let questions = broker();
    let (_dir, task) = task(TaskKind::Desktop);
    let tools = task_tools(TaskToolsFor {
        questions: questions.clone(),
        ..task
    });
    let ask = tool(&tools, "request_user_input");
    let cancel = CancellationToken::new();
    let call = {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            ask.execute(ToolInvocation {
                call_id: "call-1".to_string(),
                args: json!({"questions": [{"id": "a", "header": "", "question": "Go?", "options": []}]}),
                cancel,
                updates: ToolUpdates::none(),
            })
            .await
            .unwrap()
        })
    };
    while questions.pending_count().await == 0 {
        tokio::task::yield_now().await;
    }
    cancel.cancel();
    let result = call.await.unwrap();
    assert!(result.is_error);
    assert_eq!(
        pi_ai::content_text(&result.content),
        "request_user_input cancelled"
    );
    assert_eq!(questions.pending_count().await, 0);
}
