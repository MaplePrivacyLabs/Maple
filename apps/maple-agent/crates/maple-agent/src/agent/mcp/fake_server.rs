//! A streamable HTTP MCP server for tests, enough of one for rmcp's client.
//! It answers `initialize` or, on the stateless revision, `server/discover`,
//! then `tools/list` and `tools/call`, and records what it was sent. Its tools:
//!
//! - `echo` answers with its `text` argument;
//! - `fail` answers with an error result;
//! - `slow` never answers, so a call can be stopped;
//! - `progress` reports progress, then answers;
//! - `grow` adds the tool `added`, says its tools changed, then answers.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use serde_json::{Value, json};
use tokio::sync::broadcast;

/// The revision of MCP a fake server speaks.
#[derive(Clone, Copy, PartialEq)]
enum Revision {
    /// The handshake; `server/discover` is an unknown method.
    Handshake,
    /// The handshake; `server/discover` gets a bare HTTP 400.
    HandshakeRefusingProbe,
    /// Stateless 2026-07-28: no handshake and no session.
    Stateless,
}

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
    /// The version `initialize` carried, and what the client offered with it.
    handshakes: Vec<(String, Value)>,
    /// The `MCP-Protocol-Version` and `Mcp-Method` headers of every message.
    protocol_headers: Vec<(Option<String>, Option<String>)>,
    /// Calls whose response stream the client closed before the answer.
    closed_streams: usize,
}

struct FakeState {
    revision: Revision,
    /// Tool-list changes, for open `subscriptions/listen` streams.
    changes: broadcast::Sender<()>,
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
        Self::start_revision(Revision::Handshake, instructions).await
    }

    pub(crate) async fn start_stateless(instructions: Option<&str>) -> Self {
        Self::start_revision(Revision::Stateless, instructions).await
    }

    pub(crate) async fn start_refusing_probe() -> Self {
        Self::start_revision(Revision::HandshakeRefusingProbe, None).await
    }

    async fn start_revision(revision: Revision, instructions: Option<&str>) -> Self {
        let state = Arc::new(FakeState {
            revision,
            changes: broadcast::channel(8).0,
            instructions: instructions.map(str::to_string),
            tools: Mutex::new(
                [
                    ("echo", "Echo the text"),
                    ("fail", "Always fails"),
                    ("slow", "Never answers"),
                    ("progress", "Reports progress"),
                    ("grow", "Adds a tool"),
                    ("shrink", "Withdraws echo"),
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

    /// Offer a tool that returns its `text` argument as image data unchanged,
    /// including noncanonical or invalid data from a third-party server.
    pub(crate) fn add_image_tool(&self) {
        self.state
            .tools
            .lock()
            .unwrap()
            .push(tool("image", "Returns image data unchanged"));
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

    pub(crate) fn handshakes(&self) -> Vec<(String, Value)> {
        self.seen(|seen| seen.handshakes.clone())
    }

    pub(crate) fn protocol_headers(&self) -> Vec<(Option<String>, Option<String>)> {
        self.seen(|seen| seen.protocol_headers.clone())
    }

    pub(crate) fn closed_streams(&self) -> usize {
        self.seen(|seen| seen.closed_streams)
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn json_response(message: Value) -> Response {
    let mut response = (
        [(header::CONTENT_TYPE, "application/json")],
        message.to_string(),
    )
        .into_response();
    *response.status_mut() = StatusCode::OK;
    response
}

/// An answer that gives the client a session, which only the handshake does.
fn session_response(message: Value) -> Response {
    let mut response = json_response(message);
    response.headers_mut().insert(
        header::HeaderName::from_static("mcp-session-id"),
        header::HeaderValue::from_static("fake-session"),
    );
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

fn event(message: &Value) -> Result<Bytes, Infallible> {
    Ok(Bytes::from(format!("event: message\ndata: {message}\n\n")))
}

/// A `subscriptions/listen` stream: its acknowledgment, then a notice for each
/// change to the tool list.
fn subscription(state: &FakeState, id: Value) -> Response {
    let notice = |method: &str, params: Value, id: &Value| {
        let mut params = params;
        params["_meta"] = json!({ "io.modelcontextprotocol/subscriptionId": id });
        json!({ "jsonrpc": "2.0", "method": method, "params": params })
    };
    let acknowledged = notice(
        "notifications/subscriptions/acknowledged",
        json!({ "notifications": { "toolsListChanged": true } }),
        &id,
    );
    let changes = state.changes.subscribe();
    let stream = futures_util::stream::unfold(
        (Some(acknowledged), changes, id),
        move |(first, mut changes, id)| async move {
            let message = match first {
                Some(message) => message,
                None => {
                    changes.recv().await.ok()?;
                    notice("notifications/tools/list_changed", json!({}), &id)
                }
            };
            Some((event(&message), (None, changes, id)))
        },
    );
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        Body::from_stream(stream),
    )
        .into_response()
}

/// Counts a response stream the client closed.
struct CountsClosing(Arc<FakeState>);

impl Drop for CountsClosing {
    fn drop(&mut self) {
        self.0.seen.lock().unwrap().closed_streams += 1;
    }
}

/// A response stream that never answers, as a call stopped on the stateless
/// revision has: closing it is what cancels the call.
fn stream_without_answer(state: Arc<FakeState>) -> Response {
    let stream = futures_util::stream::unfold(CountsClosing(state), |closing| async move {
        std::future::pending::<()>().await;
        Some((event(&Value::Null), closing))
    });
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        Body::from_stream(stream),
    )
        .into_response()
}

/// What a client hears of a changed tool list: inline on the handshake, on its
/// subscription on the stateless revision.
fn tools_changed(state: &FakeState) -> Vec<Value> {
    if state.revision == Revision::Stateless {
        let _ = state.changes.send(());
        Vec::new()
    } else {
        vec![json!({
            "jsonrpc": "2.0",
            "method": "notifications/tools/list_changed"
        })]
    }
}

fn result(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn text_result(text: &str, is_error: bool) -> Value {
    json!({
        "resultType": "complete",
        "content": [{ "type": "text", "text": text }],
        "isError": is_error
    })
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
        let text_header = |name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        };
        seen.protocol_headers.push((
            text_header("mcp-protocol-version"),
            text_header("mcp-method"),
        ));
        if name == "initialize" {
            seen.handshakes.push((
                message["params"]["protocolVersion"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                message["params"]["capabilities"].clone(),
            ));
        }
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
    let unknown_method = |id: &Value| {
        json_response(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": "Method not found" }
        }))
    };
    match name.as_str() {
        "server/discover" => match state.revision {
            Revision::Handshake => unknown_method(&id),
            Revision::HandshakeRefusingProbe => StatusCode::BAD_REQUEST.into_response(),
            Revision::Stateless => {
                let mut answer = json!({
                    "resultType": "complete",
                    "supportedVersions": ["2026-07-28"],
                    "capabilities": { "tools": { "listChanged": true } },
                    "ttlMs": 0,
                    "cacheScope": "private",
                    "_meta": {
                        "io.modelcontextprotocol/serverInfo": { "name": "fake", "version": "1.0.0" }
                    }
                });
                if let Some(instructions) = &state.instructions {
                    answer["instructions"] = json!(instructions);
                }
                json_response(result(&id, answer))
            }
        },
        "subscriptions/listen" if state.revision == Revision::Stateless => subscription(&state, id),
        "initialize" if state.revision == Revision::Stateless => unknown_method(&id),
        "initialize" => {
            let mut answer = json!({
                "protocolVersion": params["protocolVersion"],
                "capabilities": { "tools": { "listChanged": true } },
                "serverInfo": { "name": "fake", "version": "1.0.0" }
            });
            if let Some(instructions) = &state.instructions {
                answer["instructions"] = json!(instructions);
            }
            session_response(result(&id, answer))
        }
        "ping" => json_response(result(&id, json!({}))),
        "tools/list" => {
            let tools = state.tools.lock().unwrap().clone();
            json_response(result(
                &id,
                json!({
                    "resultType": "complete",
                    "tools": tools,
                    "ttlMs": 0,
                    "cacheScope": "private"
                }),
            ))
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
                "image" => json_response(result(
                    &id,
                    json!({
                        "content": [{
                            "type": "image",
                            "data": arguments["text"].as_str().unwrap_or_default(),
                            "mimeType": "image/png"
                        }],
                        "isError": false
                    }),
                )),
                "slow" if state.revision == Revision::Stateless => stream_without_answer(state),
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
                    let mut messages = tools_changed(&state);
                    messages.push(result(&id, text_result("grown", false)));
                    event_stream(messages)
                }
                "shrink" => {
                    state
                        .tools
                        .lock()
                        .unwrap()
                        .retain(|tool| tool["name"] != "echo");
                    let mut messages = tools_changed(&state);
                    messages.push(result(&id, text_result("shrunk", false)));
                    event_stream(messages)
                }
                _ => json_response(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32602, "message": format!("Unknown tool: {called}") }
                })),
            }
        }
        _ => unknown_method(&id),
    }
}
