//! The JSON-RPC client that Codex's app-server and ACP agents are spoken to
//! through, and the client interface the external-agent host drives every
//! transport through.
//!
//! One reader task owns the child's stdout. It resolves responses to the
//! requests this client sent, and hands the owner the agent's notifications
//! and requests, decoded into Maple's terms by the transport, through one
//! channel. The owner answers a request with [`AgentClient::reply`].

use super::super::AgentPermissionDecision;
use super::codex;
use super::driver::{AgentReply, DriverMessage, OpenedThread, TurnRequest};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::io::AsyncWriteExt;
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::codec::{FramedRead, LinesCodec};
use tokio_util::sync::CancellationToken;

/// One protocol line. A file change can carry a whole diff.
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;
/// Messages waiting for the owner. The owner never blocks on a message for
/// long: requests run on their own tasks.
const SERVER_MESSAGE_CAPACITY: usize = 256;

/// A message the agent initiated, as it arrived.
#[derive(Debug)]
pub(super) enum ServerMessage {
    Notification {
        method: String,
        params: Value,
    },
    /// The client must answer with the same `id`.
    Request {
        id: Value,
        method: String,
        params: Value,
    },
}

/// Turns the agent's messages into Maple's terms, in the order they arrive.
pub(super) type Decoder = Box<dyn FnMut(ServerMessage) -> Vec<DriverMessage> + Send>;

impl ServerMessage {
    /// A Codex app-server message in Maple's terms.
    pub(super) fn decode_codex(self) -> Vec<DriverMessage> {
        vec![match self {
            Self::Notification { method, params } => {
                DriverMessage::Event(codex::parse_notification(&method, &params))
            }
            Self::Request { id, method, params } => DriverMessage::Request {
                id,
                request: codex::agent_request(&method, &params),
            },
        }]
    }
}

/// How one agent's JSON-RPC differs from another's.
#[derive(Debug, Clone, Copy)]
pub(super) struct Dialect {
    /// Whether messages carry `"jsonrpc": "2.0"`. Codex's app-server leaves
    /// it out; ACP requires it.
    pub(super) jsonrpc: bool,
}

/// A request the agent answered with an error.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct RpcFailure {
    pub(super) code: Option<i64>,
    pub(super) message: String,
}

/// A connected agent process, whichever transport speaks to it. The host
/// drives every agent through these operations and nothing else.
pub(super) enum AgentClient {
    Codex(Arc<RpcClient>),
    Claude(Arc<super::claude::Client>),
    Acp(Arc<super::acp_client::Client>),
}

impl AgentClient {
    pub(super) fn closed(&self) -> &CancellationToken {
        match self {
            Self::Codex(client) => client.closed(),
            Self::Claude(client) => client.closed(),
            Self::Acp(client) => client.closed(),
        }
    }

    /// The handshake.
    pub(super) async fn initialize(&self) -> Result<(), String> {
        match self {
            Self::Codex(client) => {
                client
                    .request("initialize", codex::initialize_params())
                    .await?;
                client.notify("initialized", json!({})).await
            }
            Self::Claude(client) => client.initialize().await,
            Self::Acp(client) => client.initialize().await,
        }
    }

    /// Reopen `existing`, or open a new thread in `cwd`.
    pub(super) async fn open_thread(
        &self,
        existing: Option<&str>,
        cwd: &Path,
        model: Option<&str>,
    ) -> Result<OpenedThread, String> {
        match self {
            Self::Codex(client) => match existing {
                Some(thread_id) => {
                    client
                        .request("thread/resume", codex::thread_resume_params(thread_id))
                        .await?;
                    Ok(OpenedThread {
                        thread_id: thread_id.to_string(),
                        confirmed: true,
                    })
                }
                None => {
                    let response = client
                        .request("thread/start", codex::thread_start_params(cwd, model))
                        .await?;
                    let thread_id = codex::thread_id_from_response(&response)
                        .ok_or_else(|| "The agent did not report a thread ID".to_string())?;
                    Ok(OpenedThread {
                        thread_id,
                        confirmed: true,
                    })
                }
            },
            Self::Claude(client) => client.open_thread(),
            Self::Acp(client) => client.open_session(existing, cwd).await,
        }
    }

    pub(super) async fn start_turn(&self, request: &TurnRequest<'_>) -> Result<(), String> {
        match self {
            Self::Codex(client) => client
                .request("turn/start", codex::turn_start_params(request))
                .await
                .map(drop),
            Self::Claude(client) => client.start_turn(request.prompt).await,
            Self::Acp(client) => client.prompt(request).await,
        }
    }

    pub(super) async fn interrupt(&self, thread_id: &str, turn_id: &str) -> Result<(), String> {
        match self {
            Self::Codex(client) => client
                .request(
                    "turn/interrupt",
                    codex::turn_interrupt_params(thread_id, turn_id),
                )
                .await
                .map(drop),
            Self::Claude(client) => client.interrupt().await,
            Self::Acp(client) => client.cancel().await,
        }
    }

    /// Answer a request the agent made.
    pub(super) async fn reply(&self, id: Value, reply: AgentReply) -> Result<(), String> {
        match self {
            Self::Codex(client) => {
                let result = match reply {
                    AgentReply::Decision(decision) => codex::approval_response(decision),
                    AgentReply::Answers(answer) => codex::user_input_response(&answer),
                };
                client.respond(id, result).await
            }
            Self::Claude(client) => client.reply(id, reply).await,
            Self::Acp(client) => client.reply(id, reply).await,
        }
    }

    /// Refuse a request Maple does not serve.
    pub(super) async fn refuse(&self, id: Value, message: &str) {
        match self {
            Self::Codex(client) => client.respond_error(id, -32000, message).await,
            Self::Acp(client) => client.refuse(id).await,
            Self::Claude(client) => {
                let _ = client
                    .reply(id, AgentReply::Decision(AgentPermissionDecision::Cancel))
                    .await;
            }
        }
    }
}

type PendingResponses = Arc<StdMutex<HashMap<u64, oneshot::Sender<Result<Value, RpcFailure>>>>>;

/// A JSON-RPC peer over a child's stdio, one message per line.
pub(super) struct RpcClient {
    dialect: Dialect,
    writer: Mutex<ChildStdin>,
    pending: PendingResponses,
    next_id: AtomicU64,
    /// Cancelled when the server's stdout closes; every request fails
    /// after that.
    closed: CancellationToken,
}

impl RpcClient {
    /// Codex's app-server over the child's pipes.
    pub(super) fn codex(
        stdin: ChildStdin,
        stdout: ChildStdout,
    ) -> (
        Arc<Self>,
        mpsc::Receiver<DriverMessage>,
        tokio::task::JoinHandle<()>,
    ) {
        let (sender, receiver) = mpsc::channel(SERVER_MESSAGE_CAPACITY);
        let (client, reader) = Self::new(
            stdin,
            stdout,
            Dialect { jsonrpc: false },
            Box::new(ServerMessage::decode_codex),
            sender,
        );
        (client, receiver, reader)
    }

    /// Wrap the child's pipes. `decode` turns each message the agent starts
    /// into what `sender` carries; the join handle is the reader task, which
    /// ends when stdout closes.
    pub(super) fn new(
        stdin: ChildStdin,
        stdout: ChildStdout,
        dialect: Dialect,
        decode: Decoder,
        sender: mpsc::Sender<DriverMessage>,
    ) -> (Arc<Self>, tokio::task::JoinHandle<()>) {
        let client = Arc::new(Self {
            dialect,
            writer: Mutex::new(stdin),
            pending: Arc::new(StdMutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            closed: CancellationToken::new(),
        });
        let reader = tokio::spawn(read_server_messages(
            stdout,
            Arc::clone(&client.pending),
            client.closed.clone(),
            decode,
            sender,
        ));
        (client, reader)
    }

    pub(super) fn closed(&self) -> &CancellationToken {
        &self.closed
    }

    /// Send a request and wait for its response.
    pub(super) async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        self.call(method, params)
            .await
            .map_err(|failure| format!("{method} failed: {}", failure.message))
    }

    /// Send a request and wait for its response, keeping an error's code.
    pub(super) async fn call(&self, method: &str, params: Value) -> Result<Value, RpcFailure> {
        let exited = || RpcFailure {
            code: None,
            message: "the agent process has exited".to_string(),
        };
        if self.closed.is_cancelled() {
            return Err(exited());
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, tx);
        if let Err(error) = self
            .write(json!({ "id": id, "method": method, "params": params }))
            .await
        {
            self.pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&id);
            return Err(RpcFailure {
                code: None,
                message: error.to_string(),
            });
        }
        tokio::select! {
            biased;
            response = rx => response.unwrap_or_else(|_| Err(exited())),
            _ = self.closed.cancelled() => Err(exited()),
        }
    }

    pub(super) async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        self.write(json!({ "method": method, "params": params }))
            .await
            .map_err(|error| format!("{method} failed: {error}"))
    }

    /// Answer a server-initiated request.
    pub(super) async fn respond(&self, id: Value, result: Value) -> Result<(), String> {
        self.write(json!({ "id": id, "result": result }))
            .await
            .map_err(|error| format!("response failed: {error}"))
    }

    pub(super) async fn respond_error(&self, id: Value, code: i64, message: &str) {
        let _ = self
            .write(json!({ "id": id, "error": { "code": code, "message": message } }))
            .await;
    }

    async fn write(&self, mut message: Value) -> std::io::Result<()> {
        if self.dialect.jsonrpc
            && let Value::Object(fields) = &mut message
        {
            fields.insert("jsonrpc".to_string(), json!("2.0"));
        }
        let mut line = serde_json::to_vec(&message)?;
        line.push(b'\n');
        let mut writer = self.writer.lock().await;
        writer.write_all(&line).await?;
        writer.flush().await
    }
}

async fn read_server_messages(
    stdout: ChildStdout,
    pending: PendingResponses,
    closed: CancellationToken,
    mut decode: Decoder,
    sender: mpsc::Sender<DriverMessage>,
) {
    use futures_util::StreamExt;

    let mut lines = FramedRead::new(stdout, LinesCodec::new_with_max_length(MAX_LINE_BYTES));
    while let Some(line) = lines.next().await {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                log::warn!("External agent protocol read failed: {error}");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(_) => {
                log::debug!("External agent sent a non-JSON line");
                continue;
            }
        };
        match classify(message) {
            Classified::Response { id, result } => {
                let sender = pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&id);
                if let Some(sender) = sender {
                    let _ = sender.send(result);
                }
            }
            Classified::Server(message) => {
                for message in decode(message) {
                    if sender.send(message).await.is_err() {
                        closed.cancel();
                        return;
                    }
                }
            }
            Classified::Ignored => {}
        }
    }
    closed.cancel();
    let stale = std::mem::take(
        &mut *pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    for (_, sender) in stale {
        let _ = sender.send(Err(RpcFailure {
            code: None,
            message: "the agent process has exited".to_string(),
        }));
    }
}

enum Classified {
    Response {
        id: u64,
        result: Result<Value, RpcFailure>,
    },
    Server(ServerMessage),
    Ignored,
}

/// Sort one line into a response to us, a request to us, or a notification.
fn classify(message: Value) -> Classified {
    let Value::Object(mut fields) = message else {
        return Classified::Ignored;
    };
    let method = fields
        .remove("method")
        .and_then(|value| value.as_str().map(str::to_string));
    let id = fields.remove("id");
    match (id, method) {
        (Some(id), Some(method)) => Classified::Server(ServerMessage::Request {
            id,
            method,
            params: fields.remove("params").unwrap_or(Value::Null),
        }),
        (None, Some(method)) => Classified::Server(ServerMessage::Notification {
            method,
            params: fields.remove("params").unwrap_or(Value::Null),
        }),
        (Some(id), None) => {
            let Some(id) = id.as_u64() else {
                return Classified::Ignored;
            };
            let result = match fields.remove("error") {
                Some(error) if !error.is_null() => Err(RpcFailure {
                    code: error.get("code").and_then(Value::as_i64),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| error.to_string()),
                }),
                _ => Ok(fields.remove("result").unwrap_or(Value::Null)),
            };
            Classified::Response { id, result }
        }
        (None, None) => Classified::Ignored,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_responses_requests_and_notifications() {
        match classify(json!({"id": 3, "result": {"ok": true}})) {
            Classified::Response { id, result } => {
                assert_eq!(id, 3);
                assert_eq!(result.unwrap(), json!({"ok": true}));
            }
            _ => panic!("expected a response"),
        }
        match classify(json!({"id": 4, "error": {"code": 1, "message": "nope"}})) {
            Classified::Response { id, result } => {
                assert_eq!(id, 4);
                assert_eq!(
                    result.unwrap_err(),
                    RpcFailure {
                        code: Some(1),
                        message: "nope".into()
                    }
                );
            }
            _ => panic!("expected an error response"),
        }
        match classify(
            json!({"id": 9, "method": "item/commandExecution/requestApproval", "params": {"itemId": "i"}}),
        ) {
            Classified::Server(ServerMessage::Request { id, method, params }) => {
                assert_eq!(id, json!(9));
                assert_eq!(method, "item/commandExecution/requestApproval");
                assert_eq!(params["itemId"], "i");
            }
            _ => panic!("expected a server request"),
        }
        match classify(
            json!({"method": "turn/completed", "params": {"turn": {"status": "completed"}}}),
        ) {
            Classified::Server(ServerMessage::Notification { method, params }) => {
                assert_eq!(method, "turn/completed");
                assert_eq!(params["turn"]["status"], "completed");
            }
            _ => panic!("expected a notification"),
        }
        assert!(matches!(
            classify(json!({"jsonrpc": "2.0"})),
            Classified::Ignored
        ));
        assert!(matches!(classify(json!("text")), Classified::Ignored));
    }
}
