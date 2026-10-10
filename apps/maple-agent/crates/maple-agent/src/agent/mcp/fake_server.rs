//! A streamable HTTP MCP server for tests, enough of one for rmcp's client.
//! It answers `initialize`, `tools/list` and `tools/call`, and records what
//! it was sent. Its tools:
//!
//! - `echo` answers with its `text` argument;
//! - `fail` answers with an error result;
//! - `slow` never answers, so a call can be stopped;
//! - `progress` reports progress, then answers;
//! - `grow` adds the tool `added`, says its tools changed, then answers.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use serde_json::{Value, json};

#[derive(Default)]
struct Seen {
    /// The methods of every request and notification, in order.
    methods: Vec<String>,
    /// The `Authorization` header and query of every message.
    authorization: Vec<Option<String>>,
    queries: Vec<Option<String>>,
    /// Tool calls, by tool name and arguments.
    calls: Vec<(String, Value)>,
    /// The request ids clients cancelled.
    cancelled: Vec<Value>,
}

struct FakeState {
    instructions: Option<String>,
    tools: Mutex<Vec<Value>>,
    seen: Mutex<Seen>,
}

pub(crate) struct FakeServer {
    pub(crate) url: String,
    state: Arc<FakeState>,
    task: tokio::task::JoinHandle<()>,
}

fn tool(name: &str, description: &str) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": { "text": { "type": "string" } }
        }
    })
}

impl FakeServer {
    pub(crate) async fn start(instructions: Option<&str>) -> Self {
        let state = Arc::new(FakeState {
            instructions: instructions.map(str::to_string),
            tools: Mutex::new(
                [
                    ("echo", "Echo the text"),
                    ("fail", "Always fails"),
                    ("slow", "Never answers"),
                    ("progress", "Reports progress"),
                    ("grow", "Adds a tool"),
                ]
                .iter()
                .map(|(name, description)| tool(name, description))
                .collect(),
            ),
            seen: Mutex::new(Seen::default()),
        });
        let app = Router::new()
            .route("/mcp", any(handle))
            .with_state(Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { url, state, task }
    }

    fn seen<T>(&self, read: impl FnOnce(&Seen) -> T) -> T {
        read(&self.state.seen.lock().unwrap())
    }

    pub(crate) fn methods(&self) -> Vec<String> {
        self.seen(|seen| seen.methods.clone())
    }

    pub(crate) fn authorization(&self) -> Vec<Option<String>> {
        self.seen(|seen| seen.authorization.clone())
    }

    pub(crate) fn queries(&self) -> Vec<Option<String>> {
        self.seen(|seen| seen.queries.clone())
    }

    pub(crate) fn calls(&self) -> Vec<(String, Value)> {
        self.seen(|seen| seen.calls.clone())
    }

    pub(crate) fn cancelled(&self) -> Vec<Value> {
        self.seen(|seen| seen.cancelled.clone())
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn json_response(message: Value) -> Response {
    let mut response = (
        [
            (header::CONTENT_TYPE, "application/json"),
            (
                header::HeaderName::from_static("mcp-session-id"),
                "fake-session",
            ),
        ],
        message.to_string(),
    )
        .into_response();
    *response.status_mut() = StatusCode::OK;
    response
}

/// Messages sent as one event stream, the answer last.
fn event_stream(messages: Vec<Value>) -> Response {
    let body: String = messages
        .iter()
        .map(|message| format!("event: message\ndata: {message}\n\n"))
        .collect();
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}

fn result(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn text_result(text: &str, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

async fn handle(
    State(state): State<Arc<FakeState>>,
    method: axum::http::Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match method {
        axum::http::Method::POST => {}
        axum::http::Method::DELETE => return StatusCode::OK.into_response(),
        _ => return StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
    let Ok(message) = serde_json::from_slice::<Value>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let name = message["method"].as_str().unwrap_or_default().to_string();
    {
        let mut seen = state.seen.lock().unwrap();
        seen.methods.push(name.clone());
        seen.authorization.push(
            headers
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string),
        );
        seen.queries.push(uri.query().map(str::to_string));
        if name == "notifications/cancelled" {
            seen.cancelled.push(message["params"]["requestId"].clone());
        }
    }
    let Some(id) = message.get("id").filter(|id| !id.is_null()).cloned() else {
        // Notifications, and answers to the server's own requests.
        return StatusCode::ACCEPTED.into_response();
    };
    if name.is_empty() {
        return StatusCode::ACCEPTED.into_response();
    }
    let params = &message["params"];
    match name.as_str() {
        "initialize" => {
            let mut answer = json!({
                "protocolVersion": params["protocolVersion"],
                "capabilities": { "tools": { "listChanged": true } },
                "serverInfo": { "name": "fake", "version": "1.0.0" }
            });
            if let Some(instructions) = &state.instructions {
                answer["instructions"] = json!(instructions);
            }
            json_response(result(&id, answer))
        }
        "ping" => json_response(result(&id, json!({}))),
        "tools/list" => {
            let tools = state.tools.lock().unwrap().clone();
            json_response(result(&id, json!({ "tools": tools })))
        }
        "tools/call" => {
            let called = params["name"].as_str().unwrap_or_default().to_string();
            let arguments = params["arguments"].clone();
            state
                .seen
                .lock()
                .unwrap()
                .calls
                .push((called.clone(), arguments.clone()));
            match called.as_str() {
                "echo" => json_response(result(
                    &id,
                    text_result(arguments["text"].as_str().unwrap_or_default(), false),
                )),
                "fail" => json_response(result(&id, text_result("it broke", true))),
                "slow" => {
                    tokio::time::sleep(std::time::Duration::from_secs(600)).await;
                    StatusCode::GATEWAY_TIMEOUT.into_response()
                }
                "progress" => {
                    let token = params["_meta"]["progressToken"].clone();
                    event_stream(vec![
                        json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/progress",
                            "params": {
                                "progressToken": token,
                                "progress": 1,
                                "total": 2,
                                "message": "halfway"
                            }
                        }),
                        result(&id, text_result("done", false)),
                    ])
                }
                "grow" => {
                    state
                        .tools
                        .lock()
                        .unwrap()
                        .push(tool("added", "Added later"));
                    event_stream(vec![
                        json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/tools/list_changed"
                        }),
                        result(&id, text_result("grown", false)),
                    ])
                }
                _ => json_response(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32602, "message": format!("Unknown tool: {called}") }
                })),
            }
        }
        _ => json_response(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": "Method not found" }
        })),
    }
}
