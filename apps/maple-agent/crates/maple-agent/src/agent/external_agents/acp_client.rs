//! Agents that speak the Agent Client Protocol over stdio, driven the way
//! Paseo drives them: Maple is the client, at protocol version 1.
//!
//! Messages are read as JSON values, leniently: harnesses add fields of
//! their own, repeat others in snake case, and send vendor notifications.
//! Session updates become Maple's activity; a permission request becomes an
//! approval; a form elicitation becomes questions for the model that called
//! the agent. Maple offers no file system and no terminal, so the agent uses
//! its own tools under its own configuration. A request Maple does not know
//! gets "method not found".

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex};

use serde_json::{Map, Value, json};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::super::{AgentPermissionDecision, AgentQuestion, AgentQuestionOption};
use super::app_server::{Dialect, InOrder, RpcClient, RpcFailure, ServerMessage};
use super::catalog::CatalogEntry;
use super::driver::{
    AgentEvent, AgentItem, AgentReply, AgentRequest, DriverMessage, FileChangeEntry, OpenedThread,
    TurnOutcome, TurnRequest, command_line, parse_todo,
};
use super::lock;

/// The protocol version Maple speaks.
const PROTOCOL_VERSION: u64 = 1;
/// Messages waiting for the host.
const MESSAGE_CAPACITY: usize = 256;
/// JSON-RPC's code for a method the receiver does not have.
const METHOD_NOT_FOUND: i64 = -32601;
/// ACP's code for a session that needs sign-in.
const AUTH_REQUIRED: i64 = -32000;

/// One ACP agent process.
pub(super) struct Client {
    entry: &'static CatalogEntry,
    rpc: Arc<RpcClient>,
    state: Arc<StdMutex<Session>>,
    /// The host's channel, for the turn ends this client reports itself.
    events: mpsc::Sender<DriverMessage>,
}

/// What this client knows of the agent and its session.
#[derive(Default)]
struct Session {
    id: Option<String>,
    can_resume: bool,
    can_load: bool,
    options: Vec<ConfigOption>,
    /// Set while a reopened session replays its history, whose updates
    /// belong to turns that are over.
    replaying: bool,
    /// The current prompt's number, for message ids.
    prompt: u64,
    /// Which run of agent text within the prompt arrives now: a tool call
    /// between two runs of text splits them into two messages.
    segment: u64,
    text_open: bool,
    calls: HashMap<String, ToolCall>,
    /// Requests the agent made that wait for the host, by id.
    waiting: HashMap<String, Waiting>,
}

/// One of the session's settings, as `configOptions` lists it.
#[derive(Debug, Clone, PartialEq)]
struct ConfigOption {
    id: String,
    category: Option<String>,
    current: Option<String>,
    values: Vec<String>,
}

/// A tool call, merged from every update about it.
#[derive(Debug, Default)]
struct ToolCall {
    kind: Option<String>,
    title: String,
    status: Option<String>,
    raw_input: Option<Value>,
    raw_output: Option<Value>,
    content: Vec<Value>,
    locations: Vec<String>,
    started: bool,
    finished: bool,
}

/// How a request the agent waits on is answered.
#[derive(Debug, Clone)]
enum Waiting {
    Permission { options: Vec<Value> },
    Form { schema: Value },
}

impl Client {
    /// Speak ACP to a started process.
    pub(super) fn new(
        entry: &'static CatalogEntry,
        stdin: ChildStdin,
        stdout: ChildStdout,
    ) -> (
        Arc<Self>,
        mpsc::Receiver<DriverMessage>,
        tokio::task::JoinHandle<()>,
    ) {
        let (sender, receiver) = mpsc::channel(MESSAGE_CAPACITY);
        let state = Arc::new(StdMutex::new(Session::default()));
        let decoding = Arc::clone(&state);
        let agent = entry.id.clone();
        let (rpc, reader) = RpcClient::new(
            stdin,
            stdout,
            Dialect { jsonrpc: true },
            Box::new(move |message| decode(&agent, &mut lock(&decoding), message)),
            sender.clone(),
        );
        let client = Arc::new(Self {
            entry,
            rpc,
            state,
            events: sender,
        });
        (client, receiver, reader)
    }

    pub(super) fn closed(&self) -> &CancellationToken {
        self.rpc.closed()
    }

    /// Introduce Maple: no file system, no terminal, form questions.
    pub(super) async fn initialize(&self) -> Result<(), String> {
        let mut params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "clientCapabilities": {
                "fs": { "readTextFile": false, "writeTextFile": false },
                "terminal": false,
                "elicitation": { "form": {} },
            },
            "clientInfo": { "name": "Maple Work", "version": env!("CARGO_PKG_VERSION") },
        });
        if let Some(meta) = self
            .entry
            .acp
            .as_ref()
            .and_then(|acp| acp.initialize_meta.clone())
        {
            params["_meta"] = meta;
        }
        let result = self
            .rpc
            .call("initialize", params)
            .await
            .map_err(|failure| self.failure("initialize", &failure))?;
        let version = result.get("protocolVersion").and_then(Value::as_u64);
        if version != Some(PROTOCOL_VERSION) {
            return Err(format!(
                "{} speaks ACP version {}; Maple speaks version {PROTOCOL_VERSION}.",
                self.entry.name,
                version.map_or_else(|| "unknown".to_string(), |version| version.to_string())
            ));
        }
        let capabilities = result
            .get("agentCapabilities")
            .cloned()
            .unwrap_or(Value::Null);
        let mut state = lock(&self.state);
        state.can_load = flag(&capabilities, "loadSession")
            || flag(&capabilities, "load_session")
            || session_capability(&capabilities, "load");
        state.can_resume = session_capability(&capabilities, "resume");
        Ok(())
    }

    /// Reopen `existing`, by resuming it where the agent can and loading it
    /// otherwise, or open a new session in `cwd`.
    pub(super) async fn open_session(
        &self,
        existing: Option<&str>,
        cwd: &Path,
    ) -> Result<OpenedThread, String> {
        let (can_resume, can_load) = {
            let state = lock(&self.state);
            (state.can_resume, state.can_load)
        };
        let params = |session: Option<&str>| {
            let mut params = json!({ "cwd": cwd.to_string_lossy(), "mcpServers": [] });
            if let Some(session) = session {
                params["sessionId"] = json!(session);
            }
            params
        };
        let (session, result) = match existing {
            Some(session) if can_resume => (
                session.to_string(),
                self.rpc
                    .call("session/resume", params(Some(session)))
                    .await
                    .map_err(|failure| self.failure("session/resume", &failure))?,
            ),
            Some(session) if can_load => {
                // Replayed history is not news; it is ignored until the next
                // prompt, since some agents go on replaying after the reply.
                lock(&self.state).replaying = true;
                let result = self.rpc.call("session/load", params(Some(session))).await;
                (
                    session.to_string(),
                    result.map_err(|failure| self.failure("session/load", &failure))?,
                )
            }
            Some(_) => {
                return Err(format!(
                    "{} cannot reopen an earlier session. Start a new agent.",
                    self.entry.name
                ));
            }
            None => {
                let result = self
                    .rpc
                    .call("session/new", params(None))
                    .await
                    .map_err(|failure| self.failure("session/new", &failure))?;
                let session = result
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("{} did not name its session.", self.entry.name))?
                    .to_string();
                (session, result)
            }
        };
        let mut state = lock(&self.state);
        state.id = Some(session.clone());
        if let Some(options) = config_options(&result) {
            state.options = options;
        }
        Ok(OpenedThread {
            thread_id: session,
            confirmed: true,
        })
    }

    /// Apply the task's mode and the turn's model and effort, refusing a value
    /// the agent does not offer. Runs before the turn starts, so a refused
    /// value starts no turn.
    pub(super) async fn configure(&self, request: &TurnRequest<'_>) -> Result<(), String> {
        let mode = self
            .entry
            .mode(request.mode)
            .and_then(|mode| mode.option.as_deref());
        self.choose("mode", "permission mode", mode).await?;
        self.choose("model", "model", request.model).await?;
        self.choose("thought_level", "effort level", request.effort)
            .await
    }

    /// Send the turn's prompt, once [`Self::configure`] has applied its
    /// options. The prompt's reply ends the turn: the reader reports it to the host in the order
    /// the agent sent it, after the turn's last update and before anything
    /// the agent sends later. The prompt is written before this returns, so a
    /// cancel cannot reach the agent ahead of it.
    pub(super) async fn prompt(&self, request: &TurnRequest<'_>) -> Result<(), String> {
        let (session, prompt) = {
            let mut state = lock(&self.state);
            state.prompt += 1;
            state.segment = 0;
            state.text_open = false;
            // A reopened session's history can keep arriving after its load
            // reply, as Paseo also finds; it ends with the first new prompt.
            // Like Paseo's, this is a best effort: history the agent sends
            // after Maple's prompt is already on its way is not told apart.
            // Setting options first, as a turn usually does, makes that rare.
            state.replaying = false;
            (state.id.clone(), state.prompt)
        };
        let session = session.ok_or("The agent has no open session.")?;
        let _ = self
            .events
            .send(DriverMessage::Event(AgentEvent::TurnStarted {
                turn_id: Some(format!("prompt-{prompt}")),
            }))
            .await;
        let state = Arc::clone(&self.state);
        let entry = self.entry;
        let params = json!({
            "sessionId": session,
            "prompt": [{ "type": "text", "text": request.prompt }],
        });
        let on_response: InOrder = Box::new(move |reply| {
            {
                // Text after the reply comes from a turn the agent started
                // itself, such as Grok's when a background command ends: a
                // message of its own, not more of this one.
                let mut state = lock(&state);
                if state.text_open {
                    state.text_open = false;
                    state.segment += 1;
                }
            }
            let (outcome, error) = turn_end(entry, reply);
            vec![DriverMessage::Event(AgentEvent::TurnCompleted {
                outcome,
                error,
            })]
        });
        self.rpc
            .send_in_order("session/prompt", params, on_response)
            .await
            .map_err(|failure| self.failure("session/prompt", &failure))
    }

    /// Set the session's option of `category` to `wanted`, refusing a value
    /// the agent did not offer.
    async fn choose(
        &self,
        category: &str,
        label: &str,
        wanted: Option<&str>,
    ) -> Result<(), String> {
        let Some(wanted) = wanted.map(str::trim).filter(|wanted| !wanted.is_empty()) else {
            return Ok(());
        };
        let (session, option) = {
            let state = lock(&self.state);
            let option = state
                .options
                .iter()
                .find(|option| {
                    option.category.as_deref() == Some(category) || option.id == category
                })
                .cloned();
            (state.id.clone(), option)
        };
        let Some(option) = option else {
            return Err(format!("{} offers no {label} choice.", self.entry.name));
        };
        // The mode is set before every prompt even when it reads as set
        // already: Maple keeps the agent in the task's mode whatever the
        // agent did meanwhile.
        if option.current.as_deref() == Some(wanted) && category != "mode" {
            return Ok(());
        }
        if !option.values.iter().any(|value| value == wanted) {
            return Err(format!(
                "{} has no {label} '{wanted}'. It offers: {}.",
                self.entry.name,
                option.values.join(", ")
            ));
        }
        let result = self
            .rpc
            .call(
                "session/set_config_option",
                json!({ "sessionId": session, "configId": option.id, "value": wanted }),
            )
            .await
            .map_err(|failure| self.failure("session/set_config_option", &failure))?;
        let mut state = lock(&self.state);
        match config_options(&result) {
            Some(options) => state.options = options,
            None => {
                if let Some(option) = state.options.iter_mut().find(|known| known.id == option.id) {
                    option.current = Some(wanted.to_string());
                }
            }
        }
        Ok(())
    }

    /// Stop the turn. Requests the agent left waiting are answered first,
    /// as ACP asks of a client that cancels.
    pub(super) async fn cancel(&self) -> Result<(), String> {
        let (session, waiting) = {
            let mut state = lock(&self.state);
            let waiting = state.waiting.drain().collect::<Vec<_>>();
            (state.id.clone(), waiting)
        };
        let Some(session) = session else {
            return Ok(());
        };
        let mut params = json!({ "sessionId": session });
        if let Some(meta) = self
            .entry
            .acp
            .as_ref()
            .and_then(|acp| acp.cancel_meta.clone())
        {
            params["_meta"] = meta;
        }
        let sent = self.rpc.notify("session/cancel", params).await;
        for (id, waiting) in waiting {
            let id = serde_json::from_str(&id).unwrap_or(Value::String(id));
            let _ = self.rpc.respond(id, dismissed(&waiting)).await;
        }
        sent
    }

    /// Answer a request the agent made.
    pub(super) async fn reply(&self, id: Value, reply: AgentReply) -> Result<(), String> {
        let Some(waiting) = lock(&self.state).waiting.remove(&id.to_string()) else {
            // Already answered, as when the turn was cancelled.
            return Ok(());
        };
        self.rpc.respond(id, answer(&waiting, reply)).await
    }

    /// Refuse a request Maple does not serve.
    pub(super) async fn refuse(&self, id: Value) {
        lock(&self.state).waiting.remove(&id.to_string());
        self.rpc
            .respond_error(id, METHOD_NOT_FOUND, "Method not found")
            .await;
    }

    /// What a failed request means for the person reading the result.
    fn failure(&self, method: &str, failure: &RpcFailure) -> String {
        describe_failure(self.entry, method, failure)
    }
}

fn flag(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool) == Some(true)
}

/// Whether the agent lists `name` among its session capabilities, under
/// either spelling.
fn session_capability(capabilities: &Value, name: &str) -> bool {
    ["sessionCapabilities", "session_capabilities"]
        .iter()
        .any(|key| capabilities[key].get(name).is_some_and(Value::is_object))
}

fn describe_failure(entry: &CatalogEntry, method: &str, failure: &RpcFailure) -> String {
    let message = failure.message.to_ascii_lowercase();
    if failure.code == Some(AUTH_REQUIRED)
        || [
            "authenticat",
            "unauthorized",
            "sign in",
            "sign-in",
            "log in",
            "login",
        ]
        .iter()
        .any(|words| message.contains(words))
    {
        return entry.text.sign_in_hint.clone();
    }
    format!("{method} failed: {}", failure.message)
}

/// How the prompt's reply ends the turn.
fn turn_end(
    entry: &CatalogEntry,
    reply: Result<Value, RpcFailure>,
) -> (TurnOutcome, Option<String>) {
    let result = match reply {
        Ok(result) => result,
        Err(failure) => {
            return (
                TurnOutcome::Failed,
                Some(describe_failure(entry, "session/prompt", &failure)),
            );
        }
    };
    match result.get("stopReason").and_then(Value::as_str) {
        Some("end_turn") | None => (TurnOutcome::Completed, None),
        Some("cancelled") => (TurnOutcome::Cancelled, None),
        Some("max_tokens") => (
            TurnOutcome::Completed,
            Some("The agent stopped at its output limit.".to_string()),
        ),
        Some("max_turn_requests") => (
            TurnOutcome::Completed,
            Some("The agent stopped at its limit of model requests for one turn.".to_string()),
        ),
        Some("refusal") => (
            TurnOutcome::Failed,
            Some("The agent's model refused to go on.".to_string()),
        ),
        Some(other) => (
            TurnOutcome::Failed,
            Some(format!("The agent stopped: {other}.")),
        ),
    }
}

/// The session's settings from a reply that lists them.
fn config_options(result: &Value) -> Option<Vec<ConfigOption>> {
    let options = result.get("configOptions")?.as_array()?;
    Some(
        options
            .iter()
            .filter_map(|option| {
                let mut values = Vec::new();
                for choice in option.get("options")?.as_array()? {
                    match choice.get("options").and_then(Value::as_array) {
                        // A group of choices.
                        Some(group) => values.extend(
                            group
                                .iter()
                                .filter_map(|choice| choice.get("value")?.as_str())
                                .map(str::to_string),
                        ),
                        None => values.extend(
                            choice
                                .get("value")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                        ),
                    }
                }
                Some(ConfigOption {
                    id: option.get("id")?.as_str()?.to_string(),
                    category: option
                        .get("category")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    current: option
                        .get("currentValue")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    values,
                })
            })
            .collect(),
    )
}

/// One message from the agent in Maple's terms.
fn decode(agent: &str, state: &mut Session, message: ServerMessage) -> Vec<DriverMessage> {
    match message {
        ServerMessage::Notification { method, params } => {
            if method != "session/update" || state.replaying {
                return Vec::new();
            }
            session_update(state, params.get("update").unwrap_or(&Value::Null))
                .into_iter()
                .map(DriverMessage::Event)
                .collect()
        }
        ServerMessage::Request { id, method, params } => {
            let mut messages = Vec::new();
            let request = match method.as_str() {
                "session/request_permission" => {
                    let (events, request) = permission_request(agent, state, &id, &params);
                    messages.extend(events.into_iter().map(DriverMessage::Event));
                    request
                }
                "elicitation/create"
                    if params.get("mode").and_then(Value::as_str).unwrap_or("form") == "form" =>
                {
                    let schema = params
                        .get("requestedSchema")
                        .cloned()
                        .unwrap_or(Value::Null);
                    let questions = form_questions(
                        &schema,
                        params.get("message").and_then(Value::as_str).unwrap_or(""),
                    );
                    state
                        .waiting
                        .insert(id.to_string(), Waiting::Form { schema });
                    AgentRequest::UserInput { questions }
                }
                _ => AgentRequest::Unknown { method },
            };
            messages.push(DriverMessage::Request { id, request });
            messages
        }
    }
}

/// A permission request: always the approval of one action, which the
/// task's gate decides, never a question for the calling model however many
/// ways to allow it offers. Only its once-options are ever chosen. A request
/// to change the agent's own mode is refused: Maple sets the mode from the
/// task's. The request can say more about its call than the call's updates
/// did; what that shows comes first.
fn permission_request(
    agent: &str,
    state: &mut Session,
    id: &Value,
    params: &Value,
) -> (Vec<AgentEvent>, AgentRequest) {
    let tool = params.get("toolCall").cloned().unwrap_or(Value::Null);
    let call_id = tool
        .get("toolCallId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let events = merge_tool_call(state, &call_id, &tool);
    let call = state.calls.get(&call_id);
    let options = params
        .get("options")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let title = call.map(|call| call.title.clone()).unwrap_or_default();
    let kind = call
        .and_then(|call| call.kind.clone())
        .unwrap_or_else(|| "other".to_string());
    let arguments = call.map(request_arguments).unwrap_or_default();
    state
        .waiting
        .insert(id.to_string(), Waiting::Permission { options });
    if kind == "switch_mode" {
        return (events, AgentRequest::ModeChange { item_id: call_id });
    }
    (
        events,
        AgentRequest::Approval {
            item_id: call_id,
            tool_name: format!("{agent} {kind}"),
            arguments,
            prompt: (!title.is_empty()).then_some(title),
        },
    )
}

/// One `session/update` in Maple's terms.
fn session_update(state: &mut Session, update: &Value) -> Vec<AgentEvent> {
    match update.get("sessionUpdate").and_then(Value::as_str) {
        Some("agent_message_chunk") => {
            let Some(text) = update
                .get("content")
                .filter(|content| content["type"] == "text")
                .and_then(|content| content.get("text"))
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            else {
                return Vec::new();
            };
            state.text_open = true;
            vec![AgentEvent::AgentMessageDelta {
                item_id: format!("message-{}-{}", state.prompt, state.segment),
                delta: text.to_string(),
            }]
        }
        Some("tool_call" | "tool_call_update") => {
            let Some(id) = update.get("toolCallId").and_then(Value::as_str) else {
                return Vec::new();
            };
            if state.text_open {
                state.text_open = false;
                state.segment += 1;
            }
            merge_tool_call(state, id, update)
        }
        Some("current_mode_update") => {
            if let Some(mode) = update.get("currentModeId").and_then(Value::as_str)
                && let Some(option) = state.options.iter_mut().find(|option| {
                    option.category.as_deref() == Some("mode") || option.id == "mode"
                })
            {
                option.current = Some(mode.to_string());
            }
            Vec::new()
        }
        Some("config_option_update") => {
            if let Some(options) = config_options(update) {
                state.options = options;
            }
            Vec::new()
        }
        Some("plan") => {
            let items = update
                .get("entries")
                .and_then(Value::as_array)
                .map(|entries| entries.iter().filter_map(parse_todo).collect())
                .unwrap_or_default();
            vec![AgentEvent::ItemCompleted(AgentItem::TodoList {
                id: "plan".to_string(),
                items,
            })]
        }
        _ => Vec::new(),
    }
}

/// Merge an update into the tool call it is about, as Paseo merges them:
/// title, kind and status stay when absent; input, output, content and
/// locations are replaced when present. Returns what the merge shows.
fn merge_tool_call(state: &mut Session, id: &str, update: &Value) -> Vec<AgentEvent> {
    if id.is_empty() {
        return Vec::new();
    }
    let call = state.calls.entry(id.to_string()).or_default();
    let text = |key: &str| update.get(key).and_then(Value::as_str).map(str::to_string);
    if let Some(kind) = text("kind") {
        call.kind = Some(kind);
    }
    if let Some(title) = text("title") {
        call.title = title;
    }
    if let Some(status) = text("status") {
        call.status = Some(status);
    }
    if let Some(input) = update.get("rawInput").filter(|input| !input.is_null()) {
        call.raw_input = Some(input.clone());
    }
    if let Some(output) = update.get("rawOutput").filter(|output| !output.is_null()) {
        call.raw_output = Some(output.clone());
    }
    if let Some(content) = update.get("content").and_then(Value::as_array) {
        call.content = content.clone();
    }
    if let Some(locations) = update.get("locations").and_then(Value::as_array) {
        call.locations = locations
            .iter()
            .filter_map(|location| location.get("path").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
    }
    let done = matches!(call.status.as_deref(), Some("completed" | "failed"));
    let mut events = Vec::new();
    match call.kind.as_deref() {
        Some("execute") => {
            let command = command_of(call);
            if !call.started {
                call.started = true;
                events.push(AgentEvent::ItemStarted(AgentItem::CommandExecution {
                    id: id.to_string(),
                    command: command.clone(),
                    exit_code: None,
                    status: None,
                }));
            }
            if done && !call.finished {
                call.finished = true;
                events.push(AgentEvent::ItemCompleted(AgentItem::CommandExecution {
                    id: id.to_string(),
                    command,
                    exit_code: call.raw_output.as_ref().and_then(exit_code),
                    status: call.status.clone(),
                }));
            }
        }
        // A change counts once it is made: a refused edit changed nothing.
        Some(kind @ ("edit" | "delete" | "move")) if done && !call.finished => {
            call.finished = true;
            let changes = file_changes(call, kind);
            if call.status.as_deref() == Some("completed") && !changes.is_empty() {
                events.push(AgentEvent::ItemCompleted(AgentItem::FileChange {
                    id: id.to_string(),
                    changes,
                    status: call.status.clone(),
                }));
            }
        }
        _ => {}
    }
    events
}

/// The command a call runs, under any of the names agents give it.
fn command_of(call: &ToolCall) -> String {
    input_command(call).unwrap_or_else(|| call.title.clone())
}

/// The command in a call's input, under any of the names agents give it.
fn input_command(call: &ToolCall) -> Option<String> {
    let input = call.raw_input.as_ref();
    ["command", "CommandLine", "cmd"].iter().find_map(|key| {
        let line = command_line(input.and_then(|input| input.get(*key)));
        (!line.is_empty()).then_some(line)
    })
}

/// The file a call works on: in its input, under any of the names agents
/// give it, else its first location or diff.
fn input_path(call: &ToolCall) -> Option<String> {
    let input = call.raw_input.as_ref();
    ["path", "file_path", "filePath", "Path", "TargetFile"]
        .iter()
        .find_map(|key| input?.get(*key)?.as_str().map(str::to_string))
        .or_else(|| call.locations.first().cloned())
        .or_else(|| {
            call.content
                .iter()
                .find(|content| content["type"] == "diff")
                .and_then(|diff| Some(diff.get("path")?.as_str()?.to_string()))
        })
}

/// What an approval request carries: the call's input, with its command or
/// file also under the names Maple reads when the agent spells them its own
/// way. Both come from the input or the call's locations, never from the
/// agent's title for it.
fn request_arguments(call: &ToolCall) -> Map<String, Value> {
    let mut arguments = match &call.raw_input {
        Some(Value::Object(input)) => input.clone(),
        _ => Map::new(),
    };
    if !arguments.contains_key("command")
        && let Some(command) = input_command(call)
    {
        arguments.insert("command".into(), command.into());
    }
    if !["command", "path", "file_path"]
        .iter()
        .any(|key| arguments.contains_key(*key))
        && let Some(path) = input_path(call)
    {
        arguments.insert("path".into(), path.into());
    }
    arguments
}

/// A command's exit code, as a number or in its output's words.
fn exit_code(output: &Value) -> Option<i64> {
    if let Some(code) = ["exit_code", "exitCode"]
        .iter()
        .find_map(|key| output.get(*key).and_then(Value::as_i64))
    {
        return Some(code);
    }
    let text = output.get("output").and_then(Value::as_str)?;
    let rest = &text[text.find("exited with code ")? + "exited with code ".len()..];
    let digits = rest
        .chars()
        .take_while(|character| character.is_ascii_digit() || *character == '-')
        .collect::<String>();
    digits.parse().ok()
}

/// The files a call changed: its diffs' paths, else its locations.
fn file_changes(call: &ToolCall, kind: &str) -> Vec<FileChangeEntry> {
    let diffs = call
        .content
        .iter()
        .filter(|content| content["type"] == "diff")
        .filter_map(|diff| {
            let path = diff.get("path")?.as_str()?.to_string();
            let added = diff.get("oldText").is_none_or(Value::is_null);
            Some(FileChangeEntry {
                path,
                kind: match kind {
                    "edit" if added => "add",
                    "edit" => "update",
                    other => other,
                }
                .to_string(),
            })
        })
        .collect::<Vec<_>>();
    if !diffs.is_empty() {
        return diffs;
    }
    call.locations
        .iter()
        .map(|path| FileChangeEntry {
            path: path.clone(),
            kind: if kind == "edit" { "update" } else { kind }.to_string(),
        })
        .collect()
}

/// A form's fields, `q1` before `q2` before `q10`, each as one question. A
/// field `x_other` beside `x` is the free-text answer to `x`, not a question
/// of its own.
fn form_questions(schema: &Value, message: &str) -> Vec<AgentQuestion> {
    form_fields(schema)
        .into_iter()
        .map(|(key, field)| {
            let text = |key: &str| field.get(key).and_then(Value::as_str).unwrap_or("").trim();
            let question = [text("title"), text("description"), message.trim()]
                .into_iter()
                .find(|text| !text.is_empty())
                .unwrap_or("The agent has a question.")
                .to_string();
            AgentQuestion {
                multi_select: field["type"] == "array",
                id: key.clone(),
                header: key,
                question,
                options: choices(field)
                    .into_iter()
                    .map(|(_, label)| AgentQuestionOption {
                        label,
                        description: String::new(),
                    })
                    .collect(),
                permission: None,
            }
        })
        .collect()
}

/// The form's question fields in order.
fn form_fields(schema: &Value) -> Vec<(String, &Value)> {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut fields = properties
        .iter()
        .filter(|(key, _)| {
            key.strip_suffix("_other")
                .is_none_or(|base| !properties.contains_key(base))
        })
        .map(|(key, field)| (key.clone(), field))
        .collect::<Vec<_>>();
    fields.sort_by_key(|(key, _)| natural_key(key));
    fields
}

/// `q2` sorts before `q10`.
fn natural_key(key: &str) -> (String, u64, String) {
    let digits = key.trim_start_matches(|character: char| !character.is_ascii_digit());
    let prefix = &key[..key.len() - digits.len()];
    let number = digits
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    (
        prefix.to_string(),
        number.parse().unwrap_or(0),
        key.to_string(),
    )
}

/// A field's choices as (value, label): `oneOf` and `anyOf` entries with a
/// `const` and a `title`, or an `enum`, directly or for an array's items.
fn choices(field: &Value) -> Vec<(Value, String)> {
    let field = if field["type"] == "array" {
        field.get("items").unwrap_or(&Value::Null)
    } else {
        field
    };
    for key in ["oneOf", "anyOf"] {
        if let Some(entries) = field.get(key).and_then(Value::as_array) {
            return entries
                .iter()
                .filter_map(|entry| {
                    let value = entry.get("const")?.clone();
                    let label = entry
                        .get("title")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| value.as_str().unwrap_or_default().to_string());
                    Some((value, label))
                })
                .collect();
        }
    }
    field
        .get("enum")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(|value| Some((value.clone(), value.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// The caller's answers to a form, as `elicitation/create`'s result. An
/// answer that names a choice sends its value; other words go to the
/// field's free-text twin, or to the field itself when it takes text.
fn form_response(schema: &Value, answers: &str) -> Value {
    let Ok(parsed) = serde_json::from_str::<Value>(answers) else {
        return json!({ "action": "cancel" });
    };
    let properties = schema.get("properties").and_then(Value::as_object);
    let mut content = Map::new();
    for (key, field) in form_fields(schema) {
        let given = parsed["answers"][&key]["answers"]
            .as_array()
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if given.is_empty() {
            continue;
        }
        let choices = choices(field);
        let chosen = given
            .iter()
            .map(|answer| {
                choices
                    .iter()
                    .find(|(_, label)| label.eq_ignore_ascii_case(answer))
                    .map(|(value, _)| value.clone())
            })
            .collect::<Option<Vec<_>>>();
        let other = format!("{key}_other");
        match chosen {
            Some(values) if !choices.is_empty() => {
                let value = if field["type"] == "array" {
                    Value::Array(values)
                } else {
                    values.into_iter().next().unwrap_or(Value::Null)
                };
                content.insert(key, value);
            }
            _ if properties.is_some_and(|properties| properties.contains_key(&other)) => {
                content.insert(other, json!(given.join(", ")));
            }
            _ if choices.is_empty() => {
                content.insert(key, typed_answer(field, &given.join(", ")));
            }
            // Words that name no choice, for a field that takes only its
            // choices: that question goes unanswered.
            _ => {}
        }
    }
    if content.is_empty() {
        return json!({ "action": "decline" });
    }
    json!({ "action": "accept", "content": content })
}

/// Free text as the field's type asks.
fn typed_answer(field: &Value, text: &str) -> Value {
    match field["type"].as_str() {
        Some("number") => text
            .parse::<f64>()
            .map_or(json!(text), |number| json!(number)),
        Some("integer") => text
            .parse::<i64>()
            .map_or(json!(text), |number| json!(number)),
        Some("boolean") => match text.to_ascii_lowercase().as_str() {
            "yes" | "true" | "y" => json!(true),
            "no" | "false" | "n" => json!(false),
            _ => json!(text),
        },
        _ => json!(text),
    }
}

/// The result that answers a waiting request with Maple's reply.
fn answer(waiting: &Waiting, reply: AgentReply) -> Value {
    match (waiting, reply) {
        (Waiting::Permission { options }, AgentReply::Decision(decision)) => {
            let kind = match decision {
                AgentPermissionDecision::AllowOnce => "allow_once",
                AgentPermissionDecision::Cancel => "reject_once",
            };
            // Options that outlive the turn are never chosen for the user.
            selected(options.iter().find(|option| option["kind"] == kind))
        }
        (Waiting::Form { schema }, AgentReply::Answers(answers)) => form_response(schema, &answers),
        (waiting, _) => dismissed(waiting),
    }
}

fn selected(option: Option<&Value>) -> Value {
    match option.and_then(|option| option.get("optionId")) {
        Some(id) => json!({ "outcome": { "outcome": "selected", "optionId": id } }),
        None => json!({ "outcome": { "outcome": "cancelled" } }),
    }
}

/// The answer to a request nobody will answer now.
fn dismissed(waiting: &Waiting) -> Value {
    match waiting {
        Waiting::Permission { .. } => {
            json!({ "outcome": { "outcome": "cancelled" } })
        }
        Waiting::Form { .. } => json!({ "action": "cancel" }),
    }
}

#[cfg(test)]
#[path = "acp_client_tests.rs"]
mod tests;
