//! Claude Code's native stream-json transport, adapted from Goose's
//! `crates/goose/src/providers/claude_code.rs` at 785d655d110746147117d23690e09cc7023aa9dc.
//! Source: https://github.com/AnthonyRonning/goose.
//!
//! The control request/response types and permission exchange originate in
//! Goose's Rust SDK protocol implementation. Maple adapts process ownership,
//! bounded concurrent reads, question answers, and activity projection here;
//! it does not instantiate Goose's provider, whose subprocess is private.
//! Unlike Goose's Auto mode, we never set --dangerously-skip-permissions.

use super::super::developer_tools::{
    executable_in_search_path, executable_on_path, spawn_contained,
};
use super::app_server::{RequestMethod, ServerMessage};
use super::codex;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::codec::{FramedRead, LinesCodec};
use tokio_util::sync::CancellationToken;

pub(crate) const PROVIDER_ID: &str = "claude";
pub(crate) const PROVIDER_NAME: &str = "Claude Code";
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;
const AUTH_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_AUTH_BYTES: usize = 16 * 1024;
const FAILURE: &str =
    "Claude Code could not complete this request. Check its sign-in and configuration.";

#[derive(Debug, Clone, Default)]
pub(crate) struct ClaudeDetection {
    pub(crate) executable: Option<PathBuf>,
    pub(crate) version: Option<String>,
    /// `None` when the installed CLI cannot report its authentication state.
    pub(crate) signed_in: Option<bool>,
    pub(crate) problem: Option<String>,
}

pub(super) fn find_executable(search_path: Option<&str>) -> Option<PathBuf> {
    match search_path {
        Some(path) => executable_in_search_path("claude", path),
        None => executable_on_path("claude"),
    }
}

pub(crate) async fn detect(search_path: Option<&str>) -> ClaudeDetection {
    let Some(executable) = find_executable(search_path) else {
        return ClaudeDetection::default();
    };
    let mut detection = ClaudeDetection {
        executable: Some(executable.clone()),
        ..Default::default()
    };
    match codex::probe_version(&executable).await {
        Ok(version) => {
            detection.version = Some(version);
            detection.signed_in = probe_auth_status(&executable, search_path).await;
        }
        Err(_) => {
            detection.problem = Some(
                "Maple could not run `claude --version`. Check the Claude Code installation."
                    .into(),
            )
        }
    }
    detection
}

pub(crate) fn sign_in_hint() -> &'static str {
    "Claude Code is not signed in. Run `claude auth login` in a terminal, then try again."
}

async fn probe_auth_status(executable: &Path, search_path: Option<&str>) -> Option<bool> {
    let mut command = tokio::process::Command::new(executable);
    command
        .args(["auth", "status", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(path) = search_path {
        command.env("PATH", path);
    }
    let mut child = spawn_contained(command).ok()?;
    let stdout = child.as_mut().stdout().take()?;
    let result = tokio::time::timeout(AUTH_PROBE_TIMEOUT, async {
        tokio::join!(
            child.as_mut().wait(),
            super::super::bounded_process::read_bounded_stdout(
                stdout,
                MAX_AUTH_BYTES,
                "Claude authentication status",
            )
        )
    })
    .await;
    child.kill_and_wait().await;
    let (status, output) = result.ok()?;
    // Read only the boolean; never retain or log account details from the CLI.
    #[derive(Deserialize)]
    struct AuthStatus {
        #[serde(rename = "loggedIn")]
        logged_in: bool,
    }
    let auth: AuthStatus = serde_json::from_slice(&output.ok()?).ok()?;
    match (status.ok()?.code(), auth.logged_in) {
        (Some(0), true) => Some(true),
        (Some(1), false) => Some(false),
        _ => None,
    }
}

pub(super) fn new_session_id() -> String {
    // Claude requires a UUID for --session-id. Generate RFC 4122 version 4
    // using the runtime's existing random source.
    let value = (rand::random::<u128>() & !(0xf_u128 << 76 | 0x3_u128 << 62))
        | 0x4_u128 << 76
        | 0x2_u128 << 62;
    let hex = format!("{value:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

pub(super) fn command_args(
    session: &str,
    resume: bool,
    model: Option<&str>,
    effort: Option<&str>,
) -> Vec<String> {
    let mut args: Vec<String> = [
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--permission-prompt-tool",
        "stdio",
        "--permission-mode",
        "default",
        if resume { "--resume" } else { "--session-id" },
        session,
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    if let Some(model) = model {
        args.extend(["--model".into(), model.into()]);
    }
    if let Some(effort) = effort {
        args.extend(["--effort".into(), effort.into()]);
    }
    args
}

// Adapted from Goose's control protocol types for Claude's SDK wire format.
#[derive(Serialize)]
struct ControlResponse<T: Serialize> {
    #[serde(rename = "type")]
    msg_type: &'static str,
    response: ControlResponseBody<T>,
}

#[derive(Serialize)]
struct ControlResponseBody<T: Serialize> {
    subtype: &'static str,
    request_id: String,
    response: T,
}

#[derive(Serialize)]
#[serde(tag = "behavior")]
enum PermissionResponse {
    #[serde(rename = "allow")]
    Allow {
        #[serde(rename = "updatedInput")]
        updated_input: serde_json::Map<String, Value>,
        #[serde(rename = "toolUseID")]
        tool_use_id: String,
    },
    #[serde(rename = "deny")]
    Deny { message: String },
}

#[derive(Serialize)]
struct ControlRequest {
    #[serde(rename = "type")]
    msg_type: &'static str,
    request_id: String,
    request: ControlRequestBody,
}

#[derive(Serialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
enum ControlRequestBody {
    Initialize,
    Interrupt,
}

#[derive(Deserialize)]
struct IncomingControlRequest {
    request_id: String,
    request: IncomingRequestBody,
}

#[derive(Deserialize)]
#[serde(tag = "subtype")]
enum IncomingRequestBody {
    #[serde(rename = "can_use_tool")]
    CanUseTool {
        tool_name: String,
        #[serde(default)]
        input: serde_json::Map<String, Value>,
        #[serde(default)]
        tool_use_id: String,
    },
}

impl<T: Serialize> ControlResponse<T> {
    fn success(request_id: String, response: T) -> Self {
        Self {
            msg_type: "control_response",
            response: ControlResponseBody {
                subtype: "success",
                request_id,
                response,
            },
        }
    }
}

type Pending = StdMutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>;

pub(super) struct Client {
    writer: Mutex<Option<ChildStdin>>,
    pending: Pending,
    permissions: StdMutex<HashMap<String, IncomingRequestBody>>,
    next_id: AtomicU64,
    session: String,
    interrupted: AtomicBool,
    closed: CancellationToken,
    sender: mpsc::Sender<ServerMessage>,
}

impl Client {
    pub(super) fn new(
        stdin: ChildStdin,
        stdout: ChildStdout,
        session: String,
    ) -> (
        Arc<Self>,
        mpsc::Receiver<ServerMessage>,
        tokio::task::JoinHandle<()>,
    ) {
        let (sender, receiver) = mpsc::channel(256);
        let client = Arc::new(Self {
            writer: Mutex::new(Some(stdin)),
            pending: StdMutex::new(HashMap::new()),
            permissions: StdMutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            session,
            interrupted: AtomicBool::new(false),
            closed: CancellationToken::new(),
            sender,
        });
        let reader_client = Arc::clone(&client);
        let reader = tokio::spawn(async move {
            reader_client.read(stdout).await;
        });
        (client, receiver, reader)
    }

    pub(super) fn closed(&self) -> &CancellationToken {
        &self.closed
    }

    async fn write(&self, message: impl Serialize) -> Result<(), String> {
        let mut bytes = serde_json::to_vec(&message).map_err(|_| FAILURE.to_string())?;
        bytes.push(b'\n');
        let mut writer = self.writer.lock().await;
        let writer = writer.as_mut().ok_or(FAILURE)?;
        writer
            .write_all(&bytes)
            .await
            .map_err(|_| FAILURE.to_string())?;
        writer.flush().await.map_err(|_| FAILURE.to_string())
    }

    async fn control(&self, request: ControlRequestBody) -> Result<Value, String> {
        let request_id = format!("req_{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(request_id.clone(), tx);
        let result = async {
            self.write(ControlRequest {
                msg_type: "control_request",
                request_id: request_id.clone(),
                request,
            })
            .await?;
            tokio::select! {
                result = rx => result.unwrap_or_else(|_| Err(FAILURE.into())),
                _ = self.closed.cancelled() => Err(FAILURE.into()),
            }
        }
        .await;
        self.pending.lock().unwrap().remove(&request_id);
        result
    }

    pub(super) async fn request(
        &self,
        method: RequestMethod,
        params: Value,
    ) -> Result<Value, String> {
        if self.closed.is_cancelled() {
            return Err(FAILURE.into());
        }
        match method {
            RequestMethod::Initialize => self.control(ControlRequestBody::Initialize).await,
            RequestMethod::ThreadStart | RequestMethod::ThreadResume => {
                Ok(json!({"thread": {"id": self.session}}))
            }
            RequestMethod::TurnStart => {
                self.notify("turn/started", json!({"turn": {"id": new_session_id()}}))
                    .await;
                self.write(json!({
                    "type": "user", "session_id": self.session,
                    "message": {"role": "user", "content": [{"type": "text", "text": params["input"][0]["text"]}]},
                })).await?;
                Ok(json!({}))
            }
            RequestMethod::TurnInterrupt => {
                self.interrupted.store(true, Ordering::Relaxed);
                self.control(ControlRequestBody::Interrupt).await
            }
            RequestMethod::TurnSteer => {
                Err("Claude does not support steering an active turn".into())
            }
        }
    }

    pub(super) async fn respond(&self, id: Value, result: Value) -> Result<(), String> {
        let id = id.as_str().ok_or(FAILURE)?;
        let request = self.permissions.lock().unwrap().remove(id).ok_or(FAILURE)?;
        let IncomingRequestBody::CanUseTool {
            tool_name,
            mut input,
            tool_use_id,
        } = request;
        let allow = if tool_name == "AskUserQuestion" {
            let mut answers = serde_json::Map::new();
            for (i, question) in input
                .get("questions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
            {
                let Some(text) = question["question"].as_str() else {
                    continue;
                };
                let answer = result["answers"][format!("q{i}")]["answers"]
                    .as_array()
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                answers.insert(text.into(), answer.into());
            }
            let answered = answers
                .values()
                .any(|answer| answer.as_str().is_some_and(|text| !text.is_empty()));
            input.insert("answers".into(), answers.into());
            answered
        } else {
            result["decision"] == "accept"
        };
        let response =
            if allow && !self.interrupted.load(Ordering::Relaxed) && !self.closed.is_cancelled() {
                PermissionResponse::Allow {
                    updated_input: input,
                    tool_use_id,
                }
            } else {
                PermissionResponse::Deny {
                    message: "Maple declined this action".into(),
                }
            };
        self.write(ControlResponse::success(id.into(), response))
            .await
    }

    async fn notify(&self, method: &str, params: Value) {
        let _ = self
            .sender
            .send(ServerMessage::Notification {
                method: method.into(),
                params,
            })
            .await;
    }

    async fn permission(&self, message: Value) -> Result<(), String> {
        let request: IncomingControlRequest =
            serde_json::from_value(message).map_err(|_| FAILURE)?;
        let IncomingRequestBody::CanUseTool {
            ref tool_name,
            ref input,
            ref tool_use_id,
        } = request.request;
        let (method, params) = if tool_name == "AskUserQuestion" {
            let questions: Vec<_> = input
                .get("questions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
                .filter(|(_, question)| question.is_object())
                .map(|(i, question)| {
                    let mut question = question.clone();
                    question["id"] = format!("q{i}").into();
                    question
                })
                .collect();
            (
                "item/tool/requestUserInput",
                json!({"questions": questions}),
            )
        } else {
            (
                "claude/tool/requestApproval",
                json!({"tool": tool_name, "input": input, "itemId": tool_use_id}),
            )
        };
        let id = request.request_id;
        {
            let mut pending = self.permissions.lock().unwrap();
            if pending.len() >= 64 || pending.contains_key(&id) {
                return Err(FAILURE.into());
            }
            pending.insert(id.clone(), request.request);
        }
        self.sender
            .send(ServerMessage::Request {
                id: id.into(),
                method: method.into(),
                params,
            })
            .await
            .map_err(|_| FAILURE.into())
    }

    async fn read(self: Arc<Self>, stdout: ChildStdout) {
        // Even task abortion closes requests. No pending permission can carry
        // into a replacement process or a subsequent turn.
        struct Close(Arc<Client>);
        impl Drop for Close {
            fn drop(&mut self) {
                self.0.closed.cancel();
                self.0.pending.lock().unwrap().clear();
                self.0.permissions.lock().unwrap().clear();
            }
        }
        let _close = Close(Arc::clone(&self));
        let mut lines = FramedRead::new(stdout, LinesCodec::new_with_max_length(MAX_LINE_BYTES));
        let mut activity = Activity::default();
        let mut completed = false;
        let mut session_confirmed = false;
        while let Some(line) = lines.next().await {
            let Ok(line) = line else {
                break;
            };
            if line.trim().is_empty() {
                continue;
            }
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                break;
            };
            match message["type"].as_str() {
                Some("control_response") => {
                    let response = &message["response"];
                    if let Some(id) = response["request_id"].as_str()
                        && let Some(tx) = self.pending.lock().unwrap().remove(id)
                    {
                        let result = if response["subtype"] == "success" {
                            Ok(response["response"].clone())
                        } else {
                            Err(FAILURE.into())
                        };
                        let _ = tx.send(result);
                    }
                }
                Some("control_request") => {
                    if self.permission(message).await.is_err() {
                        break;
                    }
                }
                _ => {
                    if !session_confirmed
                        && message["session_id"].as_str() == Some(self.session.as_str())
                        && ((message["type"] == "system" && message["subtype"] == "init")
                            || (message["type"] == "result"
                                && message["subtype"] == "success"
                                && message["is_error"] != true))
                    {
                        session_confirmed = true;
                        self.notify("thread/started", json!({"thread": {"id": self.session}}))
                            .await;
                    }
                    for (method, mut params) in activity.events(&message) {
                        if method == "turn/completed" {
                            completed = true;
                            // EOF lets Claude flush its persisted session and
                            // exit normally before the next turn resumes it.
                            self.writer.lock().await.take();
                            self.permissions.lock().unwrap().clear();
                        }
                        if method == "turn/completed" && self.interrupted.load(Ordering::Relaxed) {
                            params = json!({"turn": {"status": "interrupted"}});
                        }
                        self.notify(method, params).await;
                    }
                }
            }
        }
        // The client retains a sender, so premature EOF must explicitly finish
        // the turn. EOF after a result must not emit a second completion.
        if completed {
            return;
        }
        let turn = if self.interrupted.load(Ordering::Relaxed) {
            json!({"status": "interrupted"})
        } else {
            json!({"status": "failed", "error": {"message": FAILURE}})
        };
        self.notify("turn/completed", json!({"turn": turn})).await;
    }
}

#[derive(Default)]
struct Activity {
    message_id: String,
    tools: HashMap<String, (String, Value)>,
}

impl Activity {
    fn events(&mut self, message: &Value) -> Vec<(&'static str, Value)> {
        let mut events = Vec::new();
        // Nested Claude agents keep their own transcript. Only project the
        // delegated agent's top-level messages into Maple's activity row.
        if !message["parent_tool_use_id"].is_null() {
            return events;
        }
        match message["type"].as_str() {
            Some("stream_event") => {
                let event = &message["event"];
                if event["type"] == "message_start" {
                    self.message_id = event["message"]["id"].as_str().unwrap_or("message").into();
                } else if event["type"] == "content_block_delta"
                    && event["delta"]["type"] == "text_delta"
                {
                    events.push((
                        "item/agentMessage/delta",
                        json!({"itemId": self.message_id, "delta": event["delta"]["text"]}),
                    ));
                }
            }
            Some("assistant") => {
                let message = &message["message"];
                if let Some(id) = message["id"].as_str() {
                    self.message_id = id.into();
                }
                let blocks = message["content"].as_array().into_iter().flatten();
                let mut text = Vec::new();
                for block in blocks {
                    if block["type"] == "text" {
                        if let Some(value) = block["text"].as_str() {
                            text.push(value);
                        }
                    } else if block["type"] == "tool_use" {
                        let Some(id) = block["id"].as_str() else {
                            continue;
                        };
                        let name = block["name"].as_str().unwrap_or_default();
                        if matches!(name, "Bash" | "Edit" | "Write" | "NotebookEdit")
                            && self.tools.len() < 1024
                        {
                            self.tools
                                .insert(id.into(), (name.into(), block["input"].clone()));
                        }
                        match name {
                            "Bash" => events.push(("item/started", json!({"item": {"id": id, "type": "commandExecution", "command": block["input"]["command"]}}))),
                            "TodoWrite" => events.push(("item/started", json!({"item": {"id": id, "type": "todoList", "items": block["input"]["todos"]}}))),
                            _ => {}
                        }
                    }
                }
                if !text.is_empty() {
                    events.push(("item/completed", json!({"item": {"id": self.message_id, "type": "agentMessage", "text": text.join("\n")}})));
                }
            }
            Some("user") => {
                for block in message["message"]["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    if block["type"] != "tool_result" {
                        continue;
                    }
                    let Some(id) = block["tool_use_id"].as_str() else {
                        continue;
                    };
                    let Some((name, args)) = self.tools.remove(id) else {
                        continue;
                    };
                    let failed = block["is_error"] == true;
                    if name == "Bash" {
                        events.push(("item/completed", json!({"item": {"id": id, "type": "commandExecution", "command": args["command"], "status": if failed { "failed" } else { "completed" }}})));
                    } else if matches!(name.as_str(), "Edit" | "Write" | "NotebookEdit")
                        && !failed
                        && let Some(path) = args["file_path"]
                            .as_str()
                            .or(args["notebook_path"].as_str())
                    {
                        events.push(("item/completed", json!({"item": {"id": id, "type": "fileChange", "changes": [{"path": path, "kind": "update"}]}})));
                    }
                }
            }
            Some("result") | Some("error") => {
                let success = message["type"] == "result"
                    && message["subtype"] == "success"
                    && message["is_error"] != true;
                events.push(("turn/completed", json!({"turn": {"status": if success { "completed" } else { "failed" }, "error": if success { Value::Null } else { json!({"message": FAILURE}) }}})));
            }
            _ => {}
        }
        events
    }
}
