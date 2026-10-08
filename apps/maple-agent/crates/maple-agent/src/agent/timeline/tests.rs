use super::*;
use pi_agent_core::{AgentToolResult, FnTool, ToolInvocation};
use pi_ai::faux::{FauxProvider, faux_tool_call};
use pi_ai::{AssistantContent, Tool};
use pi_coding_agent::extensions::{RegisteredTool, ToolPrompt};
use pi_coding_agent::{
    AgentSession, AgentSessionEvent, AgentSessionOptions, ModelRegistry, PromptOptions, StaticKeys,
};
use std::sync::{Arc, Mutex};

fn echo_tool() -> RegisteredTool {
    let tool = FnTool::new(
        Tool::new(
            "read",
            "Read a file",
            json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        ),
        |invocation: ToolInvocation| async move {
            let path = invocation.args["path"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            Ok(AgentToolResult::text(format!("contents of {path}")))
        },
    );
    RegisteredTool {
        tool: Arc::new(tool),
        prompt: ToolPrompt::default(),
        active: true,
        extension: None,
    }
}

async fn new_session(faux: &FauxProvider) -> AgentSession {
    let registry = ModelRegistry::new(Arc::new(StaticKeys::default()));
    registry.register_api("faux", Arc::new(faux.clone()));
    registry.register_models([faux.model()]);
    let mut options = AgentSessionOptions::new(
        "/project",
        "Maple",
        SessionManager::in_memory("/project"),
        registry,
    );
    options.model = Some(faux.model());
    options.tools = vec![echo_tool()];
    options.settings.retry.base_delay_ms = 1;
    AgentSession::new(options).await.unwrap()
}

/// The rows a run streamed, merged the way a host merges them.
fn record_live(session: &AgentSession) -> Arc<Mutex<Vec<AgentTimelineItem>>> {
    let rows = Arc::new(Mutex::new(Vec::new()));
    let live = Mutex::new(LiveTimeline::default());
    let sink = Arc::clone(&rows);
    session.subscribe(move |event| {
        if let AgentSessionEvent::Agent(event) = event {
            for row in live.lock().unwrap().rows(event) {
                merge_into(&mut sink.lock().unwrap(), row);
            }
        }
    });
    rows
}

/// A row's id, type, role, text and status.
type Shown = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// What a person sees of a row, without its timing.
fn shown(items: &[AgentTimelineItem]) -> Vec<Shown> {
    items
        .iter()
        .map(|item| {
            (
                item.id.clone(),
                item.item_type.clone(),
                item.role.clone(),
                item.text.clone(),
                item.status.clone(),
            )
        })
        .collect()
}

#[tokio::test]
async fn live_rows_are_the_rows_a_reload_shows() {
    let faux = FauxProvider::new();
    faux.push_message(vec![
        AssistantContent::thinking("Look at the file first."),
        AssistantContent::text("Reading it."),
        faux_tool_call("read", json!({"path": "src/main.rs"})),
    ]);
    faux.push_text("It prints hello.");
    let session = new_session(&faux).await;
    let live = record_live(&session);

    session
        .prompt("What does main do?", PromptOptions::default())
        .await
        .unwrap();

    let stored = session.with_session(session_timeline);
    let live = live.lock().unwrap().clone();
    assert_eq!(shown(&live), shown(&stored));
    // Inputs and outputs agree too.
    for (live, stored) in live.iter().zip(&stored) {
        assert_eq!(live.input, stored.input, "{}", live.id);
        assert_eq!(live.output, stored.output, "{}", live.id);
    }

    let kinds: Vec<&str> = stored.iter().map(|item| item.item_type.as_str()).collect();
    assert_eq!(kinds, ["message", "thinking", "message", "tool", "message"]);
    assert_eq!(stored[0].text.as_deref(), Some("What does main do?"));
    assert!(stored[0].id.starts_with('u'));
    let tool = &stored[3];
    assert_eq!(tool.title.as_deref(), Some("read: src/main.rs"));
    assert_eq!(tool.status.as_deref(), Some("completed"));
    assert_eq!(
        tool.output.as_ref().unwrap()["text"],
        "contents of src/main.rs"
    );
    assert_eq!(stored[4].text.as_deref(), Some("It prints hello."));
}

#[tokio::test]
async fn a_failed_reply_shows_its_error_and_a_retried_attempt_does_not() {
    let faux = FauxProvider::new();
    faux.push_error("503 Maple server error (status 503)");
    faux.push_text("Recovered.");
    let session = new_session(&faux).await;
    let live = record_live(&session);
    session
        .prompt("Hi", PromptOptions::default())
        .await
        .unwrap();

    let stored = session.with_session(session_timeline);
    // The retried failure is omitted from the stored timeline.
    assert_eq!(
        stored
            .iter()
            .map(|item| item.item_type.as_str())
            .collect::<Vec<_>>(),
        ["message", "message"]
    );
    // Live, it showed as an error while the retry waited.
    assert!(
        live.lock()
            .unwrap()
            .iter()
            .any(|item| item.item_type == "error")
    );

    let faux = FauxProvider::new();
    faux.push_error("Maple credits are exhausted");
    let session = new_session(&faux).await;
    session
        .prompt("Hi", PromptOptions::default())
        .await
        .unwrap();
    let stored = session.with_session(session_timeline);
    let error = stored.last().unwrap();
    assert_eq!(error.item_type, "error");
    assert_eq!(error.title.as_deref(), Some("Credits exhausted"));
    assert_eq!(error.text.as_deref(), Some("Maple credits are exhausted"));
}

#[test]
fn notices_and_compactions_have_their_own_rows() {
    let mut session = SessionManager::in_memory("/project");
    session.append_message(SessionMessage::Llm(Message::User(pi_ai::UserMessage {
        content: vec![Content::text("Hi")],
        timestamp: 7,
    })));
    session.append_custom_entry(
        MAPLE_NOTICE_ENTRY,
        Some(notice_entry_data("stopped-1", STOPPED_NOTICE_TEXT)),
    );
    let first = session.entries()[0].id.clone();
    session.append_compaction("summary".into(), Some(first), 100, None, None, false);
    let items = session_timeline(&session);
    assert_eq!(items[0].id, "u7");
    assert_eq!(items[1].id, "stopped-1");
    assert_eq!(items[1].text.as_deref(), Some(STOPPED_NOTICE_TEXT));
    assert_eq!(items[1].item_type, "system");
    assert!(items[2].id.starts_with("compaction-"));
}

#[test]
fn tool_titles_name_what_the_call_is_about() {
    assert_eq!(
        descriptive_tool_title("shell", &json!({"command": "ls -la\nmore"})).as_deref(),
        Some("Terminal: ls -la")
    );
    assert_eq!(
        descriptive_tool_title("read", &json!({"path": "src/lib.rs"})).as_deref(),
        Some("read: src/lib.rs")
    );
    assert_eq!(
        descriptive_tool_title("load_skill", &json!({"name": "deploy"})).as_deref(),
        Some("Loading skill: deploy")
    );
    assert_eq!(descriptive_tool_title("todo_write", &json!({})), None);
    assert_eq!(
        format_tool_title("mcp__github__list_issues"),
        "mcp: github: list issues"
    );
}

#[test]
fn a_finished_skill_load_says_whether_it_loaded() {
    let loading = notice_item("t".into(), "x", "", 0);
    let mut previous = loading.clone();
    previous.title = Some("Loading skill: deploy".into());
    let mut done = loading;
    done.item_type = "tool".into();
    done.status = Some("completed".into());
    done.title = None;
    assert_eq!(
        merged_tool_title(&previous, &done).as_deref(),
        Some("Loaded skill: deploy")
    );
}
