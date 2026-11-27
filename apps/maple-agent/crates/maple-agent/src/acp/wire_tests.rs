//! The ACP wire as the clients that drive `maple-agent acp` see it: T3 Code's
//! negotiating handshake, Paseo's session setup, and golden frames for a
//! whole session, for MCP over ACP and for errors.
//!
//! Golden files live in `testdata/`. `MAPLE_ACP_WIRE_UPDATE=1` rewrites
//! them; a change to one is a change to what every ACP client receives, so
//! it belongs in review, never in a routine update.

use super::*;
use crate::acp::mcp_over_acp::wire;
use crate::agent::mcp::fake_server::FakeServer;
use std::sync::atomic::Ordering;

const TESTDATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/acp/testdata");

/// Frames with what varies between runs replaced: the project's path, ids
/// that are UUIDs or carry a run's start time, timestamps, Maple's version,
/// token counts (they follow the system prompt's length), and the ids of the
/// agent's own requests, which come from a counter every connection shares.
struct Normalizer {
    project: Vec<String>,
    ids: Vec<String>,
}

const TOKEN_COUNTS: [&str; 7] = [
    "totalTokens",
    "inputTokens",
    "outputTokens",
    "cachedReadTokens",
    "cachedWriteTokens",
    "used",
    "size",
];

impl Normalizer {
    fn new(project: &Path) -> Self {
        let mut project: Vec<String> = [Some(project.to_path_buf()), project.canonicalize().ok()]
            .into_iter()
            .flatten()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        // Longest first, so a canonical `/private/var/...` is replaced whole.
        project.sort_by_key(|path| std::cmp::Reverse(path.len()));
        project.dedup();
        Self {
            project,
            ids: Vec::new(),
        }
    }

    fn frames(&mut self, frames: &[Value]) -> Value {
        Value::Array(frames.iter().map(|frame| self.frame(frame)).collect())
    }

    fn frame(&mut self, frame: &Value) -> Value {
        let mut frame = self.value(None, frame);
        if frame.get("method").is_some() && frame.get("id").is_some() {
            frame["id"] = json!("<agent-request-id>");
        }
        frame
    }

    fn value(&mut self, key: Option<&str>, value: &Value) -> Value {
        match value {
            Value::Object(fields) => Value::Object(
                fields
                    .iter()
                    .map(|(key, value)| (key.clone(), self.value(Some(key), value)))
                    .collect(),
            ),
            Value::Array(items) => {
                Value::Array(items.iter().map(|item| self.value(None, item)).collect())
            }
            Value::String(_) if matches!(key, Some("updatedAt" | "createdAt")) => {
                json!("<time>")
            }
            Value::String(text) => Value::String(self.text(text)),
            Value::Number(_) if key.is_some_and(|key| TOKEN_COUNTS.contains(&key)) => {
                json!("<count>")
            }
            other => other.clone(),
        }
    }

    fn text(&mut self, text: &str) -> String {
        if text == env!("CARGO_PKG_VERSION") {
            return "<version>".to_string();
        }
        let mut text = text.to_string();
        for path in &self.project {
            text = text.replace(path.as_str(), "<project>");
        }
        let mut out = String::with_capacity(text.len());
        let mut rest = text.as_str();
        while let Some(ch) = rest.chars().next() {
            let id_length = if rest.len() >= 36 && is_uuid(&rest.as_bytes()[..36]) {
                36
            } else {
                run_id_length(rest)
            };
            if id_length > 0 {
                let id = &rest[..id_length];
                let index = match self.ids.iter().position(|seen| seen == id) {
                    Some(index) => index,
                    None => {
                        self.ids.push(id.to_string());
                        self.ids.len() - 1
                    }
                };
                out.push_str(&format!("<id-{index}>"));
                rest = &rest[id_length..];
            } else {
                out.push(ch);
                rest = &rest[ch.len_utf8()..];
            }
        }
        out
    }
}

/// The length of a run id (`run_<start ms>_<n>`) at the start of `text`, or 0.
fn run_id_length(text: &str) -> usize {
    let Some(after) = text.strip_prefix("run_") else {
        return 0;
    };
    let digits = |text: &str| text.bytes().take_while(u8::is_ascii_digit).count();
    let start = digits(after);
    if start == 0 || after.as_bytes().get(start) != Some(&b'_') {
        return 0;
    }
    let count = digits(&after[start + 1..]);
    if count == 0 { 0 } else { 4 + start + 1 + count }
}

fn is_uuid(bytes: &[u8]) -> bool {
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

/// Compare with `testdata/<name>.json`, or write it under
/// `MAPLE_ACP_WIRE_UPDATE=1`. A mismatch names the first frame that differs.
fn assert_golden(name: &str, actual: &Value) {
    let path = format!("{TESTDATA}/{name}.json");
    if std::env::var_os("MAPLE_ACP_WIRE_UPDATE").is_some() {
        std::fs::create_dir_all(TESTDATA).unwrap();
        let mut text = serde_json::to_string_pretty(actual).unwrap();
        text.push('\n');
        std::fs::write(&path, text).unwrap();
        return;
    }
    let expected: Value = serde_json::from_str(
        &std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{path}: {error}; MAPLE_ACP_WIRE_UPDATE=1 writes it")),
    )
    .unwrap();
    if *actual == expected {
        return;
    }
    let pretty = |value: &Value| serde_json::to_string_pretty(value).unwrap();
    if let (Value::Array(actual), Value::Array(expected)) = (actual, &expected) {
        let first = actual
            .iter()
            .zip(expected)
            .position(|(actual, expected)| actual != expected)
            .unwrap_or(actual.len().min(expected.len()));
        panic!(
            "the ACP wire changed in {name} at frame {first} ({} frames now, {} before)\nbefore:\n{}\nnow:\n{}",
            actual.len(),
            expected.len(),
            expected.get(first).map(pretty).unwrap_or_default(),
            actual.get(first).map(pretty).unwrap_or_default(),
        );
    }
    panic!(
        "the ACP wire changed in {name}\nbefore:\n{}\nnow:\n{}",
        pretty(&expected),
        pretty(actual)
    );
}

/// T3 Code asks for protocol version 2 and sends the v2 field names beside
/// the v1 ones (`toNegotiatingInitializeRequest` in its `effect-acp`, with the
/// capabilities `AcpSessionRuntime` builds). It reads a reply that carries
/// `info` as v2, so Maple answers v1-shaped and still reads the v1
/// capabilities it relies on.
#[tokio::test]
async fn t3_codes_negotiating_initialize_gets_a_v1_reply() {
    let harness = Harness::new().await;
    let (mut client, serving, context) = connect_with_context(&harness.handle, no_answers());
    let reply = client
        .call(
            "initialize",
            json!({
                "protocolVersion": 2,
                "info": {"name": "t3-code", "version": "0.0.0"},
                "capabilities": {"elicitation": {"form": {}}},
                "clientCapabilities": {
                    "fs": {"readTextFile": false, "writeTextFile": false},
                    "terminal": false,
                    "session": {"notices": {}, "compaction": {}},
                    "elicitation": {"form": {}},
                },
                "clientInfo": {"name": "t3-code", "version": "0.0.0"},
            }),
        )
        .await;
    assert_eq!(reply["protocolVersion"], 1, "{reply}");
    assert!(
        reply.get("info").is_none() && reply.get("capabilities").is_none(),
        "T3 Code would read this reply as v2: {reply}"
    );
    assert_eq!(reply["agentInfo"]["name"], "maple");
    assert_eq!(reply["agentCapabilities"]["loadSession"], true);
    assert_eq!(reply["agentCapabilities"]["mcpCapabilities"]["acp"], true);
    assert!(
        context
            .client_supports_form_elicitation
            .load(Ordering::SeqCst),
        "the v1 form elicitation capability must still be read"
    );
    let session_id = client.new_session(harness.project.path()).await;
    assert!(!session_id.is_empty());
    client.shutdown().await;
    finish(serving).await;
}

/// Everything a client receives across one session: the handshake, setup
/// with the model selector, a prompt with its updates and usage, a model
/// change, the session list, and a reload on a second connection.
#[tokio::test]
async fn a_sessions_wire_is_unchanged() {
    let harness = Harness::new().await;
    harness
        .serve_catalog(json!({
            "object": "list",
            "data": [{"id": "glm-5-3"}, {"id": "kimi-k3"}],
            "aliases": [],
        }))
        .await;
    harness.faux.push_text("Hello back.");
    let project = harness.project.path().to_path_buf();

    let (mut client, serving) = connect(&harness.handle, no_answers());
    client.initialize(json!({})).await;
    let session_id = client.new_session(&project).await;
    let result = client.prompt(&session_id, "Hello").await;
    assert_eq!(result["stopReason"], "end_turn", "{result}");
    client
        .call(
            "session/set_config_option",
            json!({"sessionId": session_id, "configId": "model", "value": "kimi-k3"}),
        )
        .await;
    client.call("session/list", json!({})).await;
    let mut frames = std::mem::take(&mut client.frames);
    client.shutdown().await;
    finish(serving).await;

    let (mut second, serving) = connect(&harness.handle, no_answers());
    second.initialize(json!({})).await;
    second
        .call(
            "session/load",
            json!({"sessionId": session_id, "cwd": project, "mcpServers": []}),
        )
        .await;
    frames.extend(std::mem::take(&mut second.frames));
    second.shutdown().await;
    finish(serving).await;

    assert_golden("session", &Normalizer::new(&project).frames(&frames));
}

/// The connection-based MCP-over-ACP binding, message by message, as
/// T3 Code's bridge decodes it (`mcpOverAcpBridge.ts`).
#[test]
fn mcp_over_acp_messages_keep_the_connection_binding() {
    let arguments = json!({"name": "echo", "arguments": {"text": "hi"}});
    let messages = [
        wire::connect("t3-code"),
        wire::request("conn-1", "tools/call", arguments.as_object().cloned()),
        wire::request("conn-1", "tools/list", None),
        wire::notification("conn-1", "notifications/initialized", None),
        wire::disconnect("conn-1"),
    ];
    let encoded: Vec<Value> = messages
        .into_iter()
        .map(|message| {
            let message = message.unwrap();
            json!({"method": message.method(), "params": message.params()})
        })
        .collect();
    assert_golden("mcp-over-acp-messages", &Value::Array(encoded));
    assert_eq!(
        wire::connection_id(&json!({"connectionId": "conn-1"})).unwrap(),
        "conn-1"
    );
    assert!(wire::connection_id(&json!({})).is_err());
}

/// The MCP-over-ACP frames of a session whose run calls one of the
/// client's tools: the whole JSON-RPC envelope, as it leaves Maple.
#[tokio::test]
async fn mcp_over_acp_frames_of_a_session_are_unchanged() {
    let harness = Harness::new().await;
    let project = harness.project.path().to_path_buf();
    let recorded = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (mut client, serving) = connect(&harness.handle, acp_mcp_answers(&recorded));
    client.initialize(json!({})).await;
    let session_id = client
        .new_session_with(json!({
            "cwd": project,
            "mcpServers": [{"type": "acp", "name": "t3-code", "serverId": "t3-code"}],
        }))
        .await;
    harness.faux.push_message(vec![faux_tool_call(
        "mcp__t3_code__echo",
        json!({"text": "hello"}),
    )]);
    harness.faux.push_text("Done.");
    let result = client.prompt(&session_id, "Echo hello.").await;
    assert_eq!(result["stopReason"], "end_turn", "{result}");
    let frames: Vec<Value> = client
        .frames
        .iter()
        .filter(|frame| {
            frame["method"]
                .as_str()
                .is_some_and(|method| method.starts_with("mcp/"))
        })
        .cloned()
        .collect();
    client.shutdown().await;
    finish(serving).await;
    assert_golden(
        "mcp-over-acp-session",
        &Normalizer::new(&project).frames(&frames),
    );
}

/// Paseo gives every agent its own HTTP tools server with a bearer token
/// (`runtime-mcp-config.ts`), on a client that offers a terminal and no file
/// system. The server is reached with the token, and its tools join the run.
#[tokio::test]
async fn paseos_http_tools_server_gets_its_bearer_token() {
    let server = FakeServer::start(None).await;
    let harness = Harness::new().await;
    let project = harness.project.path().to_path_buf();
    let (mut client, serving) = connect(&harness.handle, no_answers());
    client
        .call(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": {"readTextFile": false, "writeTextFile": false},
                    "terminal": true,
                },
                "clientInfo": {"name": "Paseo", "version": "dev"},
            }),
        )
        .await;
    let session_id = client
        .new_session_with(json!({
            "cwd": project,
            "mcpServers": [{
                "type": "http",
                "name": "paseo",
                "url": format!("{}?callerAgentId=parent-agent", server.url),
                "headers": [{"name": "Authorization", "value": "Bearer paseo-token"}],
            }],
        }))
        .await;
    harness.faux.push_message(vec![faux_tool_call(
        "mcp__paseo__echo",
        json!({"text": "hi"}),
    )]);
    harness.faux.push_text("Done.");
    let result = client.prompt(&session_id, "Echo hi.").await;
    assert_eq!(result["stopReason"], "end_turn", "{result}");
    let calls = server.calls();
    assert!(
        calls.iter().any(|(name, _)| name == "echo"),
        "the run called Paseo's tool: {calls:?}"
    );
    let authorization = server.authorization();
    assert!(
        !authorization.is_empty()
            && authorization
                .iter()
                .all(|header| header.as_deref() == Some("Bearer paseo-token")),
        "every request carried Paseo's token: {authorization:?}"
    );
    assert!(
        server
            .queries()
            .iter()
            .all(|query| query.as_deref() == Some("callerAgentId=parent-agent")),
        "the URL's query is kept"
    );
    client.shutdown().await;
    finish(serving).await;
}

/// The errors clients branch on: a session they do not have, a value Maple
/// refuses, and a method it does not serve.
#[tokio::test]
async fn error_replies_are_unchanged() {
    let harness = Harness::new().await;
    let project = harness.project.path().to_path_buf();
    let (mut client, serving) = connect(&harness.handle, no_answers());
    client.initialize(json!({})).await;
    let session_id = client.new_session(&project).await;
    let mut errors = Vec::new();
    for (method, params) in [
        (
            "_session/steering",
            json!({"sessionId": "no-such-session", "prompt": [{"type": "text", "text": "x"}]}),
        ),
        (
            "session/set_config_option",
            json!({"sessionId": "no-such-session", "configId": "model", "value": "x"}),
        ),
        (
            "session/set_config_option",
            json!({"sessionId": session_id, "configId": "model", "value": "no-such-model"}),
        ),
        (
            "session/set_mode",
            json!({"sessionId": session_id, "modeId": "no-such-mode"}),
        ),
        (
            "session/load",
            json!({"sessionId": "no-such-session", "cwd": project, "mcpServers": []}),
        ),
        ("session/close", json!({"sessionId": session_id})),
        ("no/such-method", json!({})),
    ] {
        let id = client.request(method, params).await;
        let response = client.response(id).await;
        errors.push(json!({"method": method, "error": response["error"]}));
    }
    client.shutdown().await;
    finish(serving).await;
    assert_golden("errors", &Normalizer::new(&project).frames(&errors));
}
