//! Claude Code's native stream-json transport, adapted from Goose's
//! `crates/goose/src/providers/claude_code.rs` at 785d655d110746147117d23690e09cc7023aa9dc.
//! Source: https://github.com/AnthonyRonning/goose.
//!
//! The control request/response types and permission exchange originate in
//! Goose's Rust SDK protocol implementation. Maple adapts process ownership,
//! bounded concurrent reads, question answers, and activity projection here;
//! it does not instantiate Goose's provider, whose subprocess is private.
//! Unlike Goose's Auto mode, we never set --dangerously-skip-permissions.
//! Finding and probing the CLI is the integration catalog's
//! (`integrations::detect`).

use super::super::AgentPermissionDecision;
use super::super::tools::parse_user_questions;
use super::driver::{
    AgentEvent, AgentItem, AgentReply, AgentRequest, DriverMessage, FileChangeEntry, OpenedThread,
    TurnOutcome, card_answers, command_line, parse_todo,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::io::AsyncWriteExt;
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::codec::{FramedRead, LinesCodec};
use tokio_util::sync::CancellationToken;

/// What Claude Code asks Maple to approve, as Maple's permission request
/// names it.
const TOOL_APPROVAL: &str = "claude_tool";
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;
const FAILURE: &str =
    "Claude Code could not complete this request. Check its sign-in and configuration.";

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
    sender: mpsc::Sender<DriverMessage>,
}

impl Client {
    pub(super) fn new(
        stdin: ChildStdin,
        stdout: ChildStdout,
        session: String,
    ) -> (
        Arc<Self>,
        mpsc::Receiver<DriverMessage>,
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

    fn usable(&self) -> Result<(), String> {
        if self.closed.is_cancelled() {
            Err(FAILURE.into())
        } else {
            Ok(())
        }
    }

    pub(super) async fn initialize(&self) -> Result<(), String> {
        self.usable()?;
        self.control(ControlRequestBody::Initialize).await.map(drop)
    }

    /// The session given on the command line. Claude confirms it once it
    /// starts, so it stays provisional until then.
    pub(super) fn open_thread(&self) -> Result<OpenedThread, String> {
        self.usable()?;
        Ok(OpenedThread {
            thread_id: self.session.clone(),
            confirmed: false,
        })
    }

    pub(super) async fn start_turn(&self, prompt: &str) -> Result<(), String> {
        self.usable()?;
        self.emit(AgentEvent::TurnStarted {
            turn_id: Some(new_session_id()),
        })
        .await;
        self.write(json!({
            "type": "user", "session_id": self.session,
            "message": {"role": "user", "content": [{"type": "text", "text": prompt}]},
        }))
        .await
    }

    pub(super) async fn interrupt(&self) -> Result<(), String> {
        self.usable()?;
        self.interrupted.store(true, Ordering::Relaxed);
        self.control(ControlRequestBody::Interrupt).await.map(drop)
    }

    pub(super) async fn reply(&self, id: Value, reply: AgentReply) -> Result<(), String> {
        let id = id.as_str().ok_or(FAILURE)?;
        let request = self.permissions.lock().unwrap().remove(id).ok_or(FAILURE)?;
        let IncomingRequestBody::CanUseTool {
            tool_name,
            mut input,
            tool_use_id,
        } = request;
        let allow = if tool_name == "AskUserQuestion" {
            let result = match &reply {
                AgentReply::Answers(answer) => card_answers(answer),
                AgentReply::Decision(_) => card_answers(""),
            };
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
            matches!(
                reply,
                AgentReply::Decision(AgentPermissionDecision::AllowOnce)
            )
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

    async fn emit(&self, event: AgentEvent) {
        let _ = self.sender.send(DriverMessage::Event(event)).await;
    }

    async fn permission(&self, message: Value) -> Result<(), String> {
        let request: IncomingControlRequest =
            serde_json::from_value(message).map_err(|_| FAILURE)?;
        let IncomingRequestBody::CanUseTool {
            ref tool_name,
            ref input,
            ..
        } = request.request;
        let agent_request = if tool_name == "AskUserQuestion" {
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
            AgentRequest::UserInput {
                questions: parse_user_questions(&questions),
            }
        } else {
            AgentRequest::Approval {
                item_id: request.request_id.clone(),
                tool_name: TOOL_APPROVAL.to_string(),
                arguments: input.clone(),
                prompt: Some(format!("Claude Code wants to use {tool_name}")),
            }
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
            .send(DriverMessage::Request {
                id: id.into(),
                request: agent_request,
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
                        self.emit(AgentEvent::ThreadStarted {
                            thread_id: self.session.clone(),
                        })
                        .await;
                    }
                    for mut event in activity.events(&message) {
                        if matches!(event, AgentEvent::TurnCompleted { .. }) {
                            completed = true;
                            // EOF lets Claude flush its persisted session and
                            // exit normally before the next turn resumes it.
                            self.writer.lock().await.take();
                            self.permissions.lock().unwrap().clear();
                            if self.interrupted.load(Ordering::Relaxed) {
                                event = AgentEvent::TurnCompleted {
                                    outcome: TurnOutcome::Cancelled,
                                    error: None,
                                };
                            }
                        }
                        self.emit(event).await;
                    }
                }
            }
        }
        // The client retains a sender, so premature EOF must explicitly finish
        // the turn. EOF after a result must not emit a second completion.
        if completed {
            return;
        }
        let event = if self.interrupted.load(Ordering::Relaxed) {
            AgentEvent::TurnCompleted {
                outcome: TurnOutcome::Cancelled,
                error: None,
            }
        } else {
            AgentEvent::TurnCompleted {
                outcome: TurnOutcome::Failed,
                error: Some(FAILURE.into()),
            }
        };
        self.emit(event).await;
    }
}

#[derive(Default)]
struct Activity {
    message_id: String,
    tools: HashMap<String, (String, Value)>,
}

impl Activity {
    fn events(&mut self, message: &Value) -> Vec<AgentEvent> {
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
                    events.push(AgentEvent::AgentMessageDelta {
                        item_id: self.message_id.clone(),
                        delta: event["delta"]["text"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                    });
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
                            "Bash" => {
                                events.push(AgentEvent::ItemStarted(AgentItem::CommandExecution {
                                    id: id.into(),
                                    command: command_line(Some(&block["input"]["command"])),
                                    exit_code: None,
                                    status: None,
                                }))
                            }
                            "TodoWrite" => {
                                events.push(AgentEvent::ItemStarted(AgentItem::TodoList {
                                    id: id.into(),
                                    items: block["input"]["todos"]
                                        .as_array()
                                        .map(|items| items.iter().filter_map(parse_todo).collect())
                                        .unwrap_or_default(),
                                }))
                            }
                            _ => {}
                        }
                    }
                }
                if !text.is_empty() {
                    events.push(AgentEvent::ItemCompleted(AgentItem::AgentMessage {
                        id: self.message_id.clone(),
                        text: text.join("\n"),
                    }));
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
                        events.push(AgentEvent::ItemCompleted(AgentItem::CommandExecution {
                            id: id.into(),
                            command: command_line(Some(&args["command"])),
                            exit_code: None,
                            status: Some(if failed { "failed" } else { "completed" }.into()),
                        }));
                    } else if matches!(name.as_str(), "Edit" | "Write" | "NotebookEdit")
                        && !failed
                        && let Some(path) = args["file_path"]
                            .as_str()
                            .or(args["notebook_path"].as_str())
                    {
                        let path = path.trim();
                        events.push(AgentEvent::ItemCompleted(AgentItem::FileChange {
                            id: id.into(),
                            changes: (!path.is_empty())
                                .then(|| FileChangeEntry {
                                    path: path.to_string(),
                                    kind: "update".to_string(),
                                })
                                .into_iter()
                                .collect(),
                            status: None,
                        }));
                    }
                }
            }
            Some("result") | Some("error") => {
                let success = message["type"] == "result"
                    && message["subtype"] == "success"
                    && message["is_error"] != true;
                events.push(AgentEvent::TurnCompleted {
                    outcome: if success {
                        TurnOutcome::Completed
                    } else {
                        TurnOutcome::Failed
                    },
                    error: (!success).then(|| FAILURE.to_string()),
                });
            }
            _ => {}
        }
        events
    }
}
