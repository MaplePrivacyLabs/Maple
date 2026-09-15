//! JSON-RPC 2.0 over newline-delimited stdio, as `codex app-server` speaks it.
//!
//! One reader task owns the child's stdout. It resolves responses to the
//! requests this client sent, and hands notifications and server-initiated
//! requests to the owner through one channel. The owner answers a server
//! request with [`AppServerClient::respond`].

use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::io::AsyncWriteExt;
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::codec::{FramedRead, LinesCodec};
use tokio_util::sync::CancellationToken;

/// One protocol line. A file change can carry a whole diff.
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;
/// Server messages waiting for the owner. The owner never blocks on a
/// message for long: approvals run on their own tasks.
const SERVER_MESSAGE_CAPACITY: usize = 256;

/// A message the server initiated.
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

type PendingResponses = Arc<StdMutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

pub(super) struct AppServerClient {
    writer: Mutex<ChildStdin>,
    pending: PendingResponses,
    next_id: AtomicU64,
    /// Cancelled when the server's stdout closes; every request fails
    /// after that.
    closed: CancellationToken,
}

impl AppServerClient {
    /// Wrap the child's pipes. The returned receiver carries every server
    /// message; the join handle is the reader task, which ends when stdout
    /// closes.
    pub(super) fn new(
        stdin: ChildStdin,
        stdout: ChildStdout,
    ) -> (
        Arc<Self>,
        mpsc::Receiver<ServerMessage>,
        tokio::task::JoinHandle<()>,
    ) {
        let (sender, receiver) = mpsc::channel(SERVER_MESSAGE_CAPACITY);
        let client = Arc::new(Self {
            writer: Mutex::new(stdin),
            pending: Arc::new(StdMutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            closed: CancellationToken::new(),
        });
        let reader = tokio::spawn(read_server_messages(
            stdout,
            Arc::clone(&client.pending),
            client.closed.clone(),
            sender,
        ));
        (client, receiver, reader)
    }

    pub(super) fn closed(&self) -> &CancellationToken {
        &self.closed
    }

    /// Send a request and wait for its response.
    pub(super) async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        if self.closed.is_cancelled() {
            return Err(format!("{method} failed: the agent process has exited"));
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
            return Err(format!("{method} failed: {error}"));
        }
        tokio::select! {
            biased;
            response = rx => match response {
                Ok(Ok(result)) => Ok(result),
                Ok(Err(error)) => Err(format!("{method} failed: {error}")),
                Err(_) => Err(format!("{method} failed: the agent process has exited")),
            },
            _ = self.closed.cancelled() => {
                Err(format!("{method} failed: the agent process has exited"))
            }
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

    pub(super) async fn respond_error(&self, id: Value, message: &str) {
        let _ = self
            .write(json!({ "id": id, "error": { "code": -32000, "message": message } }))
            .await;
    }

    async fn write(&self, message: Value) -> std::io::Result<()> {
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
    sender: mpsc::Sender<ServerMessage>,
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
                if sender.send(message).await.is_err() {
                    break;
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
        let _ = sender.send(Err("the agent process has exited".to_string()));
    }
}

enum Classified {
    Response {
        id: u64,
        result: Result<Value, String>,
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
                Some(error) if !error.is_null() => Err(error
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| error.to_string())),
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
                assert_eq!(result.unwrap_err(), "nope");
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
