//! The ACP server: over its framed stdio transport on the scripted runtime,
//! and its conversions and checks one by one.

use super::config::{MAX_ACP_CONNECTIONS, config_path, load_config, save_config};
use super::convert::{acp_tool_update, client_supports_form_elicitation, timeline_tool_text};
use super::session::completed_runtime_start;
use super::transport::is_session_update_line;
use super::*;
use crate::agent::tests::{Harness, eventually};
use crate::agent::{AgentRunUsage, AgentSessionSummary};
use agent_client_protocol::schema::v1::{
    ClientCapabilities, ElicitationCapabilities, ElicitationFormCapabilities, InitializeRequest,
    McpServer, McpServerHttp, McpServerStdio, RequestPermissionOutcome, SelectedPermissionOutcome,
};
use pi_ai::AssistantContent;
use pi_ai::faux::faux_tool_call;
use serde_json::{Value, json};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

/// Answers the agent's requests to the client: by method and params.
type Answer = Box<dyn Fn(&str, &Value) -> Value + Send>;

/// One ACP client on the same framed-line transport `serve_stdio` runs,
/// over in-memory pipes instead of stdin and stdout.
struct StdioAcpClient {
    write: tokio::io::DuplexStream,
    read: BufReader<tokio::io::DuplexStream>,
    next_id: u64,
    /// The `session/update` params seen so far, in order.
    updates: Vec<Value>,
    /// The agent's requests seen so far, by method.
    requests: Vec<String>,
    answer: Answer,
}

impl StdioAcpClient {
    async fn send_line(&mut self, message: Value) {
        self.write
            .write_all(message.to_string().as_bytes())
            .await
            .unwrap();
        self.write.write_all(b"\n").await.unwrap();
        self.write.flush().await.unwrap();
    }

    async fn request(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send_line(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        id
    }

    async fn notification(&mut self, method: &str, params: Value) {
        self.send_line(json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await;
    }

    /// The response for `id`. Updates on the way are kept, and the agent's
    /// own requests answered.
    async fn response(&mut self, id: u64) -> Value {
        loop {
            let mut line = String::new();
            let read =
                tokio::time::timeout(Duration::from_secs(30), self.read.read_line(&mut line))
                    .await
                    .expect("a response should arrive within 30s")
                    .expect("reading the agent's side should not fail");
            assert!(
                read > 0,
                "the agent closed the connection before answering id {id}"
            );
            let message: Value =
                serde_json::from_str(line.trim()).expect("every frame should be valid JSON");
            match message.get("method").and_then(Value::as_str) {
                Some("session/update") => self.updates.push(message["params"].clone()),
                Some(method) => {
                    if let Some(request_id) = message.get("id").cloned() {
                        self.requests.push(method.to_string());
                        let result = (self.answer)(method, &message["params"]);
                        self.send_line(
                            json!({"jsonrpc": "2.0", "id": request_id, "result": result}),
                        )
                        .await;
                    }
                }
                None if message.get("id").and_then(Value::as_u64) == Some(id) => {
                    return message;
                }
                None => {}
            }
        }
    }

    /// Send a request and wait for its result, which must not be an error.
    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.request(method, params).await;
        let response = self.response(id).await;
        assert!(
            response.get("error").is_none(),
            "{method} failed: {}",
            response["error"]
        );
        response["result"].clone()
    }

    async fn initialize(&mut self, capabilities: Value) -> Value {
        self.call(
            "initialize",
            json!({"protocolVersion": 2, "clientCapabilities": capabilities}),
        )
        .await
    }

    async fn new_session(&mut self, cwd: &Path) -> String {
        self.new_session_with(json!({"cwd": cwd, "mcpServers": []}))
            .await
    }

    async fn new_session_with(&mut self, params: Value) -> String {
        let result = self.call("session/new", params).await;
        result["sessionId"]
            .as_str()
            .expect("session/new returns a session id")
            .to_string()
    }

    async fn prompt(&mut self, session_id: &str, text: &str) -> Value {
        self.call(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": [{"type": "text", "text": text}]}),
        )
        .await
    }

    /// The updates of one kind, taken from those seen so far.
    fn take(&mut self, kind: &str) -> Vec<Value> {
        let (taken, kept) = std::mem::take(&mut self.updates)
            .into_iter()
            .partition(|update| update["update"]["sessionUpdate"] == kind);
        self.updates = kept;
        taken
    }

    /// The text of the message chunks of one kind, joined per message.
    fn texts(&mut self, kind: &str) -> Vec<(String, String)> {
        let mut texts: Vec<(String, String)> = Vec::new();
        for update in self.take(kind) {
            let id = update["update"]["messageId"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let text = update["update"]["content"]["text"]
                .as_str()
                .unwrap_or_default();
            match texts.iter_mut().find(|(seen, _)| *seen == id) {
                Some((_, joined)) => joined.push_str(text),
                None => texts.push((id, text.to_string())),
            }
        }
        texts
    }

    async fn shutdown(mut self) {
        self.write.shutdown().await.unwrap();
    }
}

/// A connection to `handle`'s runtime, served the way `serve_stdio` serves.
fn connect(
    handle: &AgentRuntimeHandle,
    answer: Answer,
) -> (StdioAcpClient, tokio::task::JoinHandle<Result<(), String>>) {
    let config = Arc::new(RwLock::new(
        normalize_config(AgentAcpConfig::default()).expect("the default ACP config is valid"),
    ));
    let context = AcpConnectionContext::new(
        handle.clone(),
        config,
        Arc::new(AgentAcpStats::default()),
        completed_runtime_start(),
    );
    let (client_write, agent_read) = tokio::io::duplex(64 * 1024);
    let (agent_write, client_read) = tokio::io::duplex(64 * 1024);
    let serving = tokio::spawn(serve(context, agent_read, agent_write));
    (
        StdioAcpClient {
            write: client_write,
            read: BufReader::new(client_read),
            next_id: 0,
            updates: Vec::new(),
            requests: Vec::new(),
            answer,
        },
        serving,
    )
}

fn no_answers() -> Answer {
    Box::new(|method, _| panic!("the agent asked {method} unexpectedly"))
}

async fn finish(serving: tokio::task::JoinHandle<Result<(), String>>) {
    let served = tokio::time::timeout(Duration::from_secs(30), serving)
        .await
        .expect("the connection ends after the client's EOF")
        .expect("serving does not panic");
    served.expect("serving completes cleanly after the client's EOF");
}

/// The stdio contract an editor depends on before it can do anything:
/// `initialize` answers, `session/new` and `session/load` offer the model
/// selector and no modes. A task created and never prompted is discarded
/// with its connection; any other task loads in a later one.
#[tokio::test]
async fn sessions_offer_the_model_selector_and_no_modes() {
    let harness = Harness::new().await;
    let project = harness.project.path().to_path_buf();
    let (mut client, serving) = connect(&harness.handle, no_answers());
    let initialize = client.initialize(json!({})).await;
    assert_eq!(initialize["agentInfo"]["name"], "maple");
    assert_eq!(initialize["protocolVersion"], 2);
    assert_eq!(initialize["agentCapabilities"]["loadSession"], true);

    let result = client
        .call(
            "session/new",
            json!({"cwd": project, "mcpServers": [], "additionalDirectories": ["/tmp"]}),
        )
        .await;
    let untouched = result["sessionId"].as_str().unwrap().to_string();
    assert!(result["modes"].is_null(), "{result}");
    let options = result["configOptions"].as_array().unwrap();
    assert_eq!(options.len(), 1, "only the model selector: {result}");
    assert_eq!(options[0]["id"], "model");
    let current = options[0]["currentValue"].as_str().unwrap().to_string();
    let offered: Vec<&str> = options[0]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|option| option["value"].as_str().unwrap())
        .collect();
    assert!(offered.contains(&current.as_str()), "{offered:?}");
    // The session's commands: the built-ins first.
    let commands = client.take("available_commands_update");
    assert_eq!(
        commands[0]["update"]["availableCommands"][0]["name"],
        "compact"
    );

    // Client EOF ends the connection and discards the untouched task.
    client.shutdown().await;
    finish(serving).await;
    assert!(harness.handle.load_session(untouched).await.is_err());

    let desktop = harness.create_task().await;
    let (mut client, serving) = connect(&harness.handle, no_answers());
    client.initialize(json!({})).await;
    let loaded = client
        .call(
            "session/load",
            json!({"sessionId": desktop, "cwd": project, "mcpServers": []}),
        )
        .await;
    assert!(loaded["modes"].is_null(), "{loaded}");
    assert_eq!(loaded["configOptions"][0]["id"], "model");
    client.shutdown().await;
    finish(serving).await;
    // A loaded task is never discarded.
    assert!(harness.handle.load_session(desktop).await.is_ok());
}

/// `session/set_mode` is answered with `invalid_params` for every mode id,
/// including the ids older builds advertised.
#[tokio::test]
async fn set_mode_is_rejected() {
    let harness = Harness::new().await;
    let (mut client, serving) = connect(&harness.handle, no_answers());
    client.initialize(json!({})).await;
    let session_id = client.new_session(harness.project.path()).await;
    for mode_id in ["approve_all", "interactive", "bogus"] {
        let id = client
            .request(
                "session/set_mode",
                json!({"sessionId": session_id, "modeId": mode_id}),
            )
            .await;
        let rejected = client.response(id).await;
        assert_eq!(rejected["error"]["code"], -32602, "{rejected}");
        assert!(
            rejected["error"]["data"]
                .as_str()
                .unwrap_or_default()
                .contains("no session modes"),
            "{rejected}"
        );
    }
    client.shutdown().await;
    finish(serving).await;
}

/// A turn streams to the caller as it happens: thinking and text once each,
/// tool calls with their results, the locked model selector and the task's
/// title; it ends with the turn's usage. The task is listed from then on,
/// and a later connection replays it.
#[tokio::test]
async fn a_prompt_streams_its_turn_and_a_later_load_replays_it() {
    let harness = Harness::new().await;
    let project = harness.project.path().to_path_buf();
    std::fs::write(project.join("notes.txt"), "Remember the milk.\n").unwrap();
    harness.faux.push_message(vec![
        AssistantContent::thinking("Look at the notes."),
        AssistantContent::text("Reading them."),
        faux_tool_call("read", json!({"path": "notes.txt"})),
    ]);
    harness.faux.push_text("They say to remember the milk.");

    let (mut client, serving) = connect(&harness.handle, no_answers());
    client.initialize(json!({})).await;
    let session_id = client.new_session(&project).await;
    client.updates.clear();
    let result = client.prompt(&session_id, "What do the notes say?").await;
    assert_eq!(result["stopReason"], "end_turn");
    assert!(result["usage"].is_object(), "{result}");

    let thoughts = client.texts("agent_thought_chunk");
    assert_eq!(thoughts.len(), 1);
    assert_eq!(thoughts[0].1, "Look at the notes.");
    let messages = client.texts("agent_message_chunk");
    let texts: Vec<&str> = messages.iter().map(|(_, text)| text.as_str()).collect();
    assert_eq!(texts, ["Reading them.", "They say to remember the milk."]);
    let calls = client.take("tool_call");
    assert_eq!(calls.len(), 1);
    let call_id = calls[0]["update"]["toolCallId"].clone();
    let updates = client.take("tool_call_update");
    assert_eq!(calls[0]["update"]["kind"], "read");
    // No update takes the kind back.
    assert!(
        updates
            .iter()
            .all(|update| update["update"]["kind"].is_null() || update["update"]["kind"] == "read"),
        "{updates:?}"
    );
    let last = updates.last().unwrap();
    assert_eq!(last["update"]["toolCallId"], call_id);
    assert_eq!(last["update"]["status"], "completed");
    assert!(
        last["update"]["content"][0]["content"]["text"]
            .as_str()
            .unwrap()
            .contains("Remember the milk."),
        "{last}"
    );
    let locked = client.take("config_option_update");
    assert_eq!(
        locked[0]["update"]["configOptions"][0]["options"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let titles = client.take("session_info_update");
    assert_eq!(titles[0]["update"]["title"], "What do the notes say?");
    // Prompted, the task is listed.
    let listed = client.call("session/list", json!({})).await;
    assert_eq!(listed["sessions"][0]["sessionId"], session_id);
    client.shutdown().await;
    finish(serving).await;

    let (mut client, serving) = connect(&harness.handle, no_answers());
    client.initialize(json!({})).await;
    client
        .call(
            "session/load",
            json!({"sessionId": session_id, "cwd": project, "mcpServers": []}),
        )
        .await;
    let users = client.texts("user_message_chunk");
    assert_eq!(users[0].1, "What do the notes say?");
    let replayed = client.texts("agent_message_chunk");
    let replayed: Vec<&str> = replayed.iter().map(|(_, text)| text.as_str()).collect();
    assert_eq!(
        replayed,
        ["Reading them.", "They say to remember the milk."]
    );
    let replayed_calls = client.take("tool_call");
    assert_eq!(replayed_calls[0]["update"]["status"], "completed");
    client.shutdown().await;
    finish(serving).await;
}

/// `session/cancel` stops a turn in progress; the next prompt runs.
#[tokio::test]
async fn cancel_stops_the_turn_in_progress() {
    let harness = Harness::new().await;
    harness.faux.push_hang();
    let (mut client, serving) = connect(&harness.handle, no_answers());
    client.initialize(json!({})).await;
    let session_id = client.new_session(harness.project.path()).await;
    let id = client
        .request(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "Wait"}]}),
        )
        .await;
    eventually(|| !harness.faux.requests().is_empty()).await;
    client
        .notification("session/cancel", json!({"sessionId": session_id}))
        .await;
    let cancelled = client.response(id).await;
    assert_eq!(
        cancelled["result"]["stopReason"], "cancelled",
        "{cancelled}"
    );

    harness.faux.push_text("Ready now.");
    let result = client.prompt(&session_id, "Answer").await;
    assert_eq!(result["stopReason"], "end_turn");
    client.shutdown().await;
    finish(serving).await;
}

/// A failed turn tells its error once and still ends the turn: Buzz takes
/// an error response as a turn that changed nothing, which it may retry.
#[tokio::test]
async fn a_failed_turn_tells_its_error_and_ends_the_turn() {
    let harness = Harness::new().await;
    harness.faux.push_error("Maple credits are exhausted");
    let (mut client, serving) = connect(&harness.handle, no_answers());
    client.initialize(json!({})).await;
    let session_id = client.new_session(harness.project.path()).await;
    let result = client.prompt(&session_id, "Hi").await;
    assert_eq!(result["stopReason"], "end_turn");
    let messages = client.texts("agent_message_chunk");
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(
        messages[0].1.contains("credits"),
        "the failure is told: {messages:?}"
    );
    client.shutdown().await;
    finish(serving).await;
}

/// `/compact` summarizes the task's history without a model turn of its
/// own, and says so; a history all recent enough to keep is left as it is.
#[tokio::test]
async fn the_compact_command_says_what_it_did() {
    let harness = Harness::new().await;
    let (mut client, serving) = connect(&harness.handle, no_answers());
    client.initialize(json!({})).await;
    let session_id = client.new_session(harness.project.path()).await;
    harness.faux.push_text("Short.");
    client.prompt(&session_id, "Hi").await;
    client.updates.clear();
    let result = client.prompt(&session_id, "/compact").await;
    assert_eq!(result["stopReason"], "end_turn");
    let messages = client.texts("agent_message_chunk");
    assert_eq!(messages[0].1, "Nothing to compact yet.\n");

    // A long history is summarized.
    harness.faux.push_text(&"A long answer. ".repeat(8_000));
    client.prompt(&session_id, "Tell me everything").await;
    // The summary of the history before the cut, and of the turn it cuts.
    harness.faux.push_text("The user asked for everything.");
    harness.faux.push_text("The answer went on at length.");
    client.updates.clear();
    let result = client.prompt(&session_id, "/compact").await;
    assert_eq!(result["stopReason"], "end_turn");
    let messages = client.texts("agent_message_chunk");
    assert_eq!(messages[0].1, "Compaction completed.\n");
    client.shutdown().await;
    finish(serving).await;
}

/// Images reach every model: a model the catalog does not mark as seeing
/// images gets each one through the read_image helper. The scripted
/// runtime's catalog cannot be reached, which fails closed to the helper.
#[tokio::test]
async fn prompt_images_travel_through_the_read_image_helper() {
    let harness = Harness::new().await;
    harness.faux.push_text("A single pixel.");
    let (mut client, serving) = connect(&harness.handle, no_answers());
    client.initialize(json!({})).await;
    let session_id = client.new_session(harness.project.path()).await;
    let result = client
        .call(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [
                    {"type": "text", "text": "What do you see?"},
                    {"type": "image", "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAAAAAA6fptVAAAADElEQVR4nGNgYGAAAAAEAAH2FzhVAAAAAELFTkSuQmCC", "mimeType": "image/png"},
                ],
            }),
        )
        .await;
    assert_eq!(result["stopReason"], "end_turn");
    let detail = harness.handle.load_session(session_id).await.unwrap();
    let item = detail
        .timeline
        .iter()
        .find(|item| item.role.as_deref() == Some("user"))
        .unwrap();
    assert_eq!(item.text.as_deref(), Some("What do you see?"));
    let attachment = &item.input.as_ref().unwrap()["imageAttachments"][0];
    assert_eq!(attachment["name"], "acp-image-1.png");
    assert!(
        attachment["source"]
            .as_str()
            .unwrap()
            .starts_with("maple-attachment://"),
        "{attachment}"
    );
    // The model was told to look with read_image.
    let request = harness.faux.requests().pop().unwrap();
    let prompt = crate::agent::tests::last_user_text(&request);
    assert!(prompt.contains("read_image"), "{prompt}");
    client.shutdown().await;
    finish(serving).await;
}

/// A project with guidance of its own asks the caller's user once whether
/// to trust it, before the first turn, and keeps the answer.
#[tokio::test]
async fn project_trust_is_asked_once_and_kept() {
    let harness = Harness::new().await;
    let project = harness.project.path().to_path_buf();
    let skill = project.join(".claude/skills/deploy/SKILL.md");
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(
        &skill,
        "---\nname: deploy\ndescription: Deploy the project\n---\nDeploy it.",
    )
    .unwrap();
    let answer: Answer = Box::new(|method, params| {
        assert_eq!(method, "session/request_permission");
        assert_eq!(params["options"][0]["optionId"], "keep_untrusted");
        json!({"outcome": {"outcome": "selected", "optionId": "trust_project"}})
    });
    let (mut client, serving) = connect(&harness.handle, answer);
    client.initialize(json!({})).await;
    let session_id = client.new_session(&project).await;
    harness.faux.push_text("Trusted.");
    let result = client.prompt(&session_id, "Hi").await;
    assert_eq!(result["stopReason"], "end_turn");
    assert_eq!(client.requests, ["session/request_permission"]);
    let status = harness
        .handle
        .get_project_trust(project.to_string_lossy().into_owned())
        .await
        .unwrap();
    assert_eq!(status.decision, Some(true));
    // The project's skill is offered to the model now.
    let system =
        pi_ai::transcript::current_system_prompt(&harness.faux.requests()[0].context.messages);
    assert!(system.contains("deploy"), "{system}");

    harness.faux.push_text("Again.");
    client.prompt(&session_id, "Hi again").await;
    assert_eq!(client.requests.len(), 1, "asked once");
    client.shutdown().await;
    finish(serving).await;
}

/// `session/new` takes Buzz's system prompt and session title: the prompt
/// follows Maple's own, and the title stays the task's name.
#[tokio::test]
async fn the_callers_system_prompt_and_title_shape_the_task() {
    let harness = Harness::new().await;
    let (mut client, serving) = connect(&harness.handle, no_answers());
    client.initialize(json!({})).await;
    let session_id = client
        .new_session_with(json!({
            "cwd": harness.project.path(),
            "mcpServers": [],
            "systemPrompt": "You are Buzz's Maple persona.",
            "_meta": {"sessionTitle": "general"},
        }))
        .await;
    harness.faux.push_text("Hello.");
    client.prompt(&session_id, "Hi").await;
    let system =
        pi_ai::transcript::current_system_prompt(&harness.faux.requests()[0].context.messages);
    assert!(system.contains("You are Buzz's Maple persona."), "{system}");
    let listed = client.call("session/list", json!({})).await;
    assert_eq!(listed["sessions"][0]["title"], "general");
    client.shutdown().await;
    finish(serving).await;
}

#[test]
fn caller_session_fields_come_from_the_raw_session_new_params() {
    let fields = AcpCallerSessionFields::from_params(&json!({
        "cwd": "/tmp/project",
        "mcpServers": [],
        "systemPrompt": "  You are Buzz's Maple persona.  ",
        "_meta": { "sessionTitle": "general" },
    }));
    assert_eq!(
        fields,
        AcpCallerSessionFields {
            system_prompt: Some("You are Buzz's Maple persona.".to_string()),
            session_title: Some("general".to_string()),
        }
    );
    let empty = AcpCallerSessionFields::from_params(&json!({
        "cwd": "/tmp/project",
        "systemPrompt": "   ",
    }));
    assert_eq!(empty, AcpCallerSessionFields::default());
}

#[test]
fn default_config_allows_eight_connections() {
    let config = AgentAcpConfig::default();
    assert_eq!(config.max_connections, 8);
    assert!(config.allowed_project_roots.is_empty());
}

#[test]
fn explicit_connection_limits_remain_configurable_below_the_default() {
    let one = normalize_config(AgentAcpConfig {
        max_connections: 1,
        ..AgentAcpConfig::default()
    })
    .unwrap();
    assert_eq!(one.max_connections, 1);

    let capped = normalize_config(AgentAcpConfig {
        max_connections: usize::MAX,
        ..AgentAcpConfig::default()
    })
    .unwrap();
    assert_eq!(capped.max_connections, MAX_ACP_CONNECTIONS);
}

#[test]
fn session_ids_are_canonicalized_and_empty_ids_are_rejected() {
    assert_eq!(
        canonical_session_id(&SessionId::new("  task-123  ")).unwrap(),
        "task-123"
    );
    assert!(canonical_session_id(&SessionId::new(" \n\t ")).is_err());
}

fn session_summary(id: &str) -> AgentSessionSummary {
    AgentSessionSummary {
        id: id.to_string(),
        title: id.to_string(),
        project_root: "/tmp/project".to_string(),
        created_ms: 1,
        updated_ms: 1,
        message_count: 0,
        model: Some("model".to_string()),
        web_enabled: false,
        state: AgentTaskState::Active,
        acp: false,
    }
}

/// Only a missing task is refused.
#[test]
fn session_load_preflight_rejects_missing_tasks() {
    let sessions = [session_summary("one"), session_summary("two")];
    for id in ["one", "two"] {
        assert_eq!(find_acp_session(&sessions, id).unwrap().id, id);
    }
    assert!(
        find_acp_session(&sessions, "missing")
            .unwrap_err()
            .contains("does not exist")
    );
}

#[tokio::test]
async fn session_operation_cancellation_keeps_close_behind_the_active_fence() {
    let connection_lifetime = CancellationToken::new();
    let operation = AcpSessionOperation::new(&connection_lifetime);
    let active = Arc::clone(&operation.gate).lock_owned().await;
    let waiting_gate = Arc::clone(&operation.gate);
    let waiting = tokio::spawn(async move { waiting_gate.lock_owned().await });

    operation.cancellation.cancel();
    assert!(operation.cancellation.is_cancelled());
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished());

    drop(active);
    let closing = tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .unwrap()
        .unwrap();
    drop(closing);
}

#[test]
fn timed_out_close_keeps_its_resurrection_fence() {
    assert!(close_registration_may_be_released(true, true, true));
    assert!(!close_registration_may_be_released(false, true, true));
    assert!(!close_registration_may_be_released(true, false, true));
    assert!(!close_registration_may_be_released(true, true, false));
}

fn row(
    id: &str,
    item_type: &str,
    role: Option<&str>,
    text: &str,
    merge: &str,
) -> AgentTimelineItem {
    AgentTimelineItem {
        id: id.to_string(),
        item_type: item_type.to_string(),
        role: role.map(str::to_string),
        title: None,
        text: Some(text.to_string()),
        status: None,
        input: None,
        output: None,
        created_ms: 1,
        merge: merge.to_string(),
    }
}

#[test]
fn completed_replayed_tool_prefers_result_over_request_summary() {
    let item = AgentTimelineItem {
        id: "tool-1".to_string(),
        item_type: "tool".to_string(),
        role: Some("assistant".to_string()),
        title: Some("Terminal".to_string()),
        text: Some("listing project root".to_string()),
        status: Some("completed".to_string()),
        input: Some(json!({ "command": "pwd" })),
        output: Some(json!({ "text": "/tmp/project" })),
        created_ms: 1,
        merge: "replace".to_string(),
    };
    let mut projection = AcpProjection::default();
    let encoded = serde_json::to_value(acp_tool_update(&item, &mut projection)).unwrap();

    assert_eq!(timeline_tool_text(&item).as_deref(), Some("/tmp/project"));
    assert_eq!(encoded["content"][0]["content"]["text"], "/tmp/project");
    assert_eq!(encoded["rawInput"]["command"], "pwd");
}

#[test]
fn slash_commands_parse_like_the_desktop_composer() {
    assert_eq!(
        parse_slash_command("/compact"),
        Some(("compact".to_string(), "".to_string()))
    );
    assert_eq!(
        parse_slash_command("  /skill-name some args  "),
        Some(("skill-name".to_string(), "some args".to_string()))
    );
    assert_eq!(parse_slash_command("plain text"), None);
    assert_eq!(parse_slash_command("/"), None);
    assert_eq!(parse_slash_command("/a/b nested"), None);
    assert_eq!(parse_slash_command("not/leading"), None);
}

#[test]
fn available_commands_lead_with_the_built_ins_plus_skills() {
    let encoded =
        serde_json::to_value(acp_available_commands(&[crate::agent::AgentSlashCommand {
            name: "buzz-worklog".to_string(),
            description: "Publish a coding update".to_string(),
            input_hint: None,
        }]))
        .unwrap();
    assert_eq!(encoded["sessionUpdate"], "available_commands_update");
    let commands = encoded["availableCommands"].as_array().unwrap();
    assert_eq!(commands[0]["name"], "compact");
    assert_eq!(commands[1]["name"], "buzz-worklog");
    assert_eq!(commands[1]["description"], "Publish a coding update");
}

#[test]
fn failed_replayed_tool_preserves_result_and_failure_badge_message() {
    let mut projection = AcpProjection::default();
    let pending = AgentTimelineItem {
        id: "tool-1".to_string(),
        item_type: "tool".to_string(),
        role: Some("assistant".to_string()),
        title: Some("Terminal".to_string()),
        text: Some("running command".to_string()),
        status: Some("pending".to_string()),
        input: Some(json!({ "command": "false" })),
        output: None,
        created_ms: 1,
        merge: "replace".to_string(),
    };
    let _ = acp_tool_update(&pending, &mut projection);
    let failed = AgentTimelineItem {
        text: Some("running command".to_string()),
        status: Some("failed".to_string()),
        output: Some(json!({
            "text": "command exited with status 1",
            "isError": true,
        })),
        ..pending
    };
    let encoded = serde_json::to_value(acp_tool_update(&failed, &mut projection)).unwrap();

    assert_eq!(encoded["sessionUpdate"], "tool_call_update");
    assert_eq!(
        encoded["content"][0]["content"]["text"],
        "command exited with status 1"
    );
    assert_eq!(
        encoded["rawOutput"]["message"],
        "command exited with status 1"
    );
}

#[test]
fn usage_update_carries_context_tokens_and_window() {
    let encoded = serde_json::to_value(SessionUpdate::UsageUpdate(UsageUpdate::new(
        53_000, 200_000,
    )))
    .unwrap();
    assert_eq!(encoded["sessionUpdate"], "usage_update");
    assert_eq!(encoded["used"], 53_000);
    assert_eq!(encoded["size"], 200_000);
    assert!(encoded.get("cost").is_none());
}

#[test]
fn prompt_usage_serializes_one_turn_without_session_accumulation() {
    let turn = AgentRunUsage {
        input_tokens: 10,
        output_tokens: 4,
        total_tokens: 14,
        cached_read_tokens: 3,
        cached_write_tokens: 1,
    };
    let encoded = serde_json::to_value(acp_usage(turn)).unwrap();

    assert_eq!(encoded["inputTokens"], 10);
    assert_eq!(encoded["outputTokens"], 4);
    assert_eq!(encoded["totalTokens"], 14);
    assert_eq!(encoded["cachedReadTokens"], 3);
    assert_eq!(encoded["cachedWriteTokens"], 1);
}

/// The model selector is the only config option, and it locks to the
/// persisted model after the first message.
#[test]
fn model_selector_locks_to_the_persisted_model_after_first_message() {
    let models = vec!["model-a".to_string(), "model-b".to_string()];
    let fresh = serde_json::to_value(acp_session_config_options("model-b", &models, 0)).unwrap();
    let locked = serde_json::to_value(acp_session_config_options("model-b", &models, 1)).unwrap();

    assert_eq!(
        fresh.as_array().unwrap().len(),
        1,
        "no mode option: {fresh}"
    );
    assert_eq!(fresh[0]["id"], "model");
    assert_eq!(fresh[0]["currentValue"], "model-b");
    assert_eq!(fresh[0]["options"].as_array().unwrap().len(), 2);
    assert_eq!(locked.as_array().unwrap().len(), 1);
    assert_eq!(locked[0]["currentValue"], "model-b");
    assert_eq!(locked[0]["options"].as_array().unwrap().len(), 1);
    assert_eq!(locked[0]["options"][0]["value"], "model-b");
}

#[test]
fn streamed_message_chunks_keep_the_timeline_item_id() {
    let mut projection = AcpProjection::default();
    for (role, item_type, expected_variant) in [
        (Some("assistant"), "message", "agent_message_chunk"),
        (Some("thought"), "thinking", "agent_thought_chunk"),
    ] {
        let item = row(
            &format!("stable-{expected_variant}"),
            item_type,
            role,
            "delta",
            "append",
        );
        let update = timeline_update(&item, &mut projection, false).unwrap();
        let encoded = serde_json::to_value(update).unwrap();
        assert_eq!(encoded["sessionUpdate"], expected_variant);
        assert_eq!(encoded["messageId"], item.id);
    }
    // A live user message is the caller's own prompt; a replay sends it.
    let user = row("u1", "message", Some("user"), "Hi", "replace");
    assert!(timeline_update(&user, &mut projection, false).is_none());
    let replayed = serde_json::to_value(timeline_update(&user, &mut projection, true)).unwrap();
    assert_eq!(replayed["sessionUpdate"], "user_message_chunk");
    assert_eq!(replayed["messageId"], "u1");
}

/// A message streams as `append` rows, then settles as one `replace` row
/// with the whole text: only what did not stream goes then.
#[test]
fn a_settled_message_sends_only_what_did_not_stream() {
    let mut projection = AcpProjection::default();
    let mut sent = String::new();
    for (text, merge) in [
        ("", "append"),
        ("Hello", "append"),
        (", world", "append"),
        ("Hello, world", "replace"),
    ] {
        if let Some(update) = timeline_update(
            &row("a1-text", "message", Some("assistant"), text, merge),
            &mut projection,
            false,
        ) {
            let encoded = serde_json::to_value(update).unwrap();
            sent.push_str(encoded["content"]["text"].as_str().unwrap());
        }
    }
    assert_eq!(sent, "Hello, world");
    // A message that did not stream is sent whole when it settles.
    let settled = timeline_update(
        &row(
            "a2-text",
            "message",
            Some("assistant"),
            "All at once",
            "replace",
        ),
        &mut projection,
        false,
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(settled).unwrap()["content"]["text"],
        "All at once"
    );
}

#[test]
fn notices_reach_the_client_only_for_compaction_and_failures() {
    let mut projection = AcpProjection::default();
    let mut notice = row(
        "compaction-1",
        "system",
        Some("system"),
        "Earlier messages were summarized to make room in the context.",
        "replace",
    );
    notice.title = Some("Context compacted".to_string());
    let encoded =
        serde_json::to_value(timeline_update(&notice, &mut projection, true).unwrap()).unwrap();
    assert_eq!(encoded["sessionUpdate"], "agent_message_chunk");
    assert_eq!(encoded["content"]["text"], "Compaction completed.\n");
    // Other notices stay out of the ACP stream.
    let mut stopped = row(
        "stopped-1",
        "system",
        Some("system"),
        "Stopped by user",
        "replace",
    );
    stopped.title = Some("Agent notice".to_string());
    assert!(timeline_update(&stopped, &mut projection, true).is_none());
    // A stored failure replays as agent text; a live one waits for the end
    // of its run, which may still retry it.
    let failure = row(
        "a1-error",
        "error",
        Some("system"),
        "Maple credits are exhausted",
        "replace",
    );
    assert!(timeline_update(&failure, &mut projection, false).is_none());
    let encoded =
        serde_json::to_value(timeline_update(&failure, &mut projection, true).unwrap()).unwrap();
    assert_eq!(encoded["content"]["text"], "Maple credits are exhausted");
}

#[test]
fn project_trust_chooser_is_fail_closed_and_cannot_be_auto_accepted() {
    let encoded = serde_json::to_value(project_trust_permission_options()).unwrap();

    assert_eq!(
        encoded,
        json!([
            {
                "optionId": "keep_untrusted",
                "name": "Keep project trust disabled",
                "kind": "allow_once"
            },
            {
                "optionId": "trust_project",
                "name": "Trust this project",
                "kind": "allow_once"
            },
            { "optionId": "cancel", "name": "Cancel turn", "kind": "reject_once" }
        ])
    );
    assert_eq!(
        project_trust_permission_decision(&RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new("keep_untrusted")
        )),
        Ok(Some(false))
    );
    assert_eq!(
        project_trust_permission_decision(&RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new("trust_project")
        )),
        Ok(Some(true))
    );
    assert_eq!(
        project_trust_permission_decision(&RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new("cancel")
        )),
        Ok(None)
    );
    assert_eq!(
        project_trust_permission_decision(&RequestPermissionOutcome::Cancelled),
        Ok(None)
    );
    assert!(
        project_trust_permission_decision(&RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new("allow_always")
        ))
        .is_err()
    );
}

#[test]
fn project_trust_elicitation_is_session_scoped_reversible_and_defaults_off() {
    let request = project_trust_elicitation_request(
        SessionId::new("session-1"),
        Path::new("/tmp/maple-project"),
    );
    let encoded = serde_json::to_value(request).unwrap();

    assert_eq!(encoded["mode"], "form");
    assert_eq!(encoded["sessionId"], "session-1");
    assert_eq!(
        encoded["requestedSchema"]["required"],
        json!(["trustProject"])
    );
    assert_eq!(
        encoded["requestedSchema"]["properties"]["trustProject"]["type"],
        "boolean"
    );
    assert_eq!(
        encoded["requestedSchema"]["properties"]["trustProject"]["default"],
        false
    );
    assert!(
        encoded["message"]
            .as_str()
            .unwrap()
            .contains("Maple runs every tool call without asking")
    );
}

#[test]
fn form_elicitation_is_used_only_when_the_client_advertises_it() {
    let plain = InitializeRequest::new(agent_client_protocol::schema::ProtocolVersion::V1);
    assert!(!client_supports_form_elicitation(&plain));

    let mut form = InitializeRequest::new(agent_client_protocol::schema::ProtocolVersion::V1);
    form.client_capabilities = ClientCapabilities::new()
        .elicitation(ElicitationCapabilities::new().form(ElicitationFormCapabilities::new()));
    assert!(client_supports_form_elicitation(&form));
}

#[cfg(unix)]
#[tokio::test]
async fn outbound_tracker_releases_credit_only_after_a_socket_write_acknowledgement() {
    use futures_util::SinkExt as _;
    use tokio::io::AsyncReadExt as _;

    let tracker = AcpOutboundTracker::with_limits(1, 1024);
    let cancellation = CancellationToken::new();
    let first = tracker.reserve(1, &cancellation).await.unwrap();
    tracker
        .pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push_back(first);

    let waiting_tracker = Arc::clone(&tracker);
    let waiting_cancellation = cancellation.clone();
    let waiting =
        tokio::spawn(async move { waiting_tracker.reserve(1, &waiting_cancellation).await });
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished());

    let line = r#"{"jsonrpc":"2.0","method":"session/update","params":{}}"#;
    let (writer, mut reader) = tokio::io::duplex(1024);
    let mut sink = Box::pin(tracked_outgoing_lines(writer, Arc::clone(&tracker)));
    sink.send(line.to_string()).await.unwrap();
    let mut written = vec![0_u8; line.len() + 1];
    reader.read_exact(&mut written).await.unwrap();
    assert_eq!(written, format!("{line}\n").into_bytes());

    let second = waiting.await.unwrap().unwrap();
    drop(second);
}

#[tokio::test]
async fn cancelled_trust_chooser_retains_credit_until_the_orphan_request_settles() {
    let tracker = AcpOutboundTracker::with_limits(1, 1024);
    let cancellation = CancellationToken::new();
    let first = tracker.reserve(1, &cancellation).await.unwrap();
    let (settled_tx, settled_rx) = tokio::sync::oneshot::channel::<()>();
    retain_cancelled_caller_request(
        async move {
            let _ = settled_rx.await;
        },
        first,
    );

    assert!(
        tokio::time::timeout(Duration::from_millis(10), tracker.reserve(1, &cancellation))
            .await
            .is_err()
    );

    settled_tx.send(()).unwrap();
    let second = tokio::time::timeout(Duration::from_secs(1), tracker.reserve(1, &cancellation))
        .await
        .unwrap()
        .unwrap();
    drop(second);
}

#[test]
fn outbound_credit_acknowledges_only_session_updates() {
    assert!(is_session_update_line(
        r#"{"jsonrpc":"2.0","method":"session/update","params":{}}"#
    ));
    assert!(!is_session_update_line(
        r#"{"jsonrpc":"2.0","id":1,"result":{}}"#
    ));
}

#[test]
fn allowed_project_root_returns_the_canonical_admitted_path() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    std::fs::create_dir(&project).unwrap();

    let admitted =
        ensure_allowed_project_root(&project, &[root.path().to_string_lossy().into_owned()])
            .unwrap();

    assert_eq!(admitted, project.canonicalize().unwrap());
}

#[test]
fn allowed_project_root_rejects_relative_paths() {
    assert!(ensure_allowed_project_root(Path::new("relative/project"), &[]).is_err());
}

/// Files from builds that still saved `enabled` and `permissionMode`
/// load; the retired fields are ignored.
#[test]
fn a_config_written_with_retired_fields_still_loads() {
    let root = tempfile::tempdir().unwrap();
    let user_id = "acp-legacy-user";
    let path = config_path(root.path(), user_id).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        br#"{"enabled":true,"permissionMode":"read_only","allowedProjectRoots":[],"maxConnections":4}"#,
    )
    .unwrap();

    let config = load_config(root.path(), user_id).unwrap();
    assert_eq!(config.max_connections, 4);
    assert!(config.allowed_project_roots.is_empty());
}

#[test]
fn save_config_writes_atomically_and_round_trips() {
    let root = tempfile::tempdir().unwrap();
    let user_id = "acp-config-user";
    let project_root = root.path().join("project");
    assert!(project_root.is_absolute());
    let config = AgentAcpConfig {
        allowed_project_roots: vec![project_root.to_string_lossy().into_owned()],
        ..AgentAcpConfig::default()
    };

    save_config(root.path(), user_id, &config).unwrap();
    // Overwrite once more: the replacement must not leave a temp file.
    save_config(root.path(), user_id, &config).unwrap();

    let path = config_path(root.path(), user_id).unwrap();
    let leftovers = path
        .parent()
        .unwrap()
        .read_dir()
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        })
        .count();
    assert_eq!(leftovers, 0);
    #[cfg(unix)]
    {
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
    assert_eq!(load_config(root.path(), user_id).unwrap(), config);
}

#[test]
fn bridge_environment_is_strictly_allowlisted() {
    let filtered = filter_bridge_environment(HashMap::from([
        (
            "BUZZ_RELAY_URL".to_string(),
            "ws://localhost:3000".to_string(),
        ),
        ("UNRELATED_SECRET".to_string(), "nope".to_string()),
    ]));
    assert_eq!(filtered.len(), 1);
    assert!(filtered.contains_key("BUZZ_RELAY_URL"));
}

#[test]
fn arbitrary_stdio_mcp_is_rejected_before_any_process_can_start() {
    let server = McpServer::Stdio(
        McpServerStdio::new("untrusted", "/bin/sh")
            .args(vec!["-c".to_string(), "exit 0".to_string()]),
    );

    assert!(prepare_session_mcp(&HashMap::new(), &[server]).is_err());
}

#[test]
fn an_http_mcp_server_runs_beside_the_tasks_own() {
    let server = McpServer::Http(McpServerHttp::new("bridge", "https://bridge.example/mcp"));
    let (environment, servers) = prepare_session_mcp(&HashMap::new(), &[server]).unwrap();
    assert!(environment.is_empty());
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].name, "bridge");
    assert!(servers[0].enabled);
    assert!(matches!(
        &servers[0].transport,
        crate::agent::AgentMcpTransport::StreamableHttp { url, .. } if url == "https://bridge.example/mcp"
    ));
}

#[test]
fn prompt_blocks_preserve_buzz_order() {
    let blocks = vec![
        ContentBlock::Text(TextContent::new("[Base]\nbase")),
        ContentBlock::Text(TextContent::new("[System]\nsystem")),
    ];
    assert_eq!(
        prompt_text(&blocks).unwrap(),
        "[Base]\nbase\n\n[System]\nsystem"
    );
}

#[test]
fn retained_terminal_results_preserve_all_stop_states() {
    let completed = prompt_result_from_terminal(AgentRunTerminal::Completed).unwrap();
    assert_eq!(
        serde_json::to_value(completed).unwrap()["stopReason"],
        "end_turn"
    );

    let cancelled = prompt_result_from_terminal(AgentRunTerminal::Cancelled).unwrap();
    assert_eq!(
        serde_json::to_value(cancelled).unwrap()["stopReason"],
        "cancelled"
    );

    let failed = prompt_result_from_terminal(AgentRunTerminal::Failed).unwrap();
    assert_eq!(
        serde_json::to_value(failed).unwrap()["stopReason"],
        "end_turn"
    );
}
