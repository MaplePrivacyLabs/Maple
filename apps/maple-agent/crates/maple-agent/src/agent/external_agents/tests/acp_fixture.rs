//! A fake ACP agent for the ACP transport, standing in for Grok Build and
//! Antigravity. Reuses the driver test binary. It logs every line it reads,
//! and the environment that should switch off telemetry and updates.

use super::{
    FIXTURE_ARGS, FIXTURE_LOG, FIXTURE_MARKER, FIXTURE_MODE, FIXTURE_PID_FILE, fixture_output,
};
use serde_json::{Value, json};
use std::fs::{self, File};
use std::io::{BufRead, Write};

const SESSION: &str = "fixture-session";
const MODELS: [&str; 2] = ["fixture-default", "fixture-fast"];
const EFFORTS: [&str; 2] = ["low", "high"];
const MODES: [&str; 2] = ["default", "auto"];

struct Agent {
    out: File,
    log: Option<String>,
    model: String,
    effort: String,
    mode: String,
}

impl Agent {
    fn send(&mut self, mut message: Value) {
        message["jsonrpc"] = json!("2.0");
        writeln!(self.out, "{message}").unwrap();
        self.out.flush().unwrap();
    }

    fn log(&self, text: &str) {
        if let Some(log) = &self.log {
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log)
                .unwrap();
            writeln!(file, "{text}").unwrap();
        }
    }

    fn options(&self) -> Value {
        let choices = |values: &[&str]| {
            values
                .iter()
                .map(|value| json!({ "value": value, "name": value }))
                .collect::<Vec<_>>()
        };
        json!([
            { "id": "model", "category": "model", "type": "select",
              "currentValue": self.model, "options": choices(&MODELS) },
            { "id": "reasoning_effort", "category": "thought_level", "type": "select",
              "currentValue": self.effort, "options": choices(&EFFORTS) },
            { "id": "mode", "category": "mode", "type": "select",
              "currentValue": self.mode, "options": choices(&MODES) },
        ])
    }

    fn update(&mut self, update: Value) {
        self.send(json!({ "method": "session/update",
            "params": { "sessionId": SESSION, "update": update } }));
    }

    fn say(&mut self, text: &str) {
        self.update(
            json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": text } }),
        );
    }
}

/// Read lines until the reply to `id`, logging each.
fn wait_for_reply(
    agent: &Agent,
    lines: &mut impl Iterator<Item = std::io::Result<String>>,
    id: &Value,
) -> Value {
    for line in lines.by_ref().map_while(Result::ok) {
        agent.log(&format!("<- {line}"));
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if message.get("id") == Some(id) && message.get("method").is_none() {
            return message;
        }
    }
    Value::Null
}

#[test]
#[ignore = "fake ACP agent run by the driver tests"]
fn run() {
    if std::env::var_os(FIXTURE_MARKER).is_none() {
        return;
    }
    let mut out = fixture_output();
    let args = std::env::var(FIXTURE_ARGS).unwrap_or_default();
    if args.contains("--version") {
        writeln!(out, "fixture 1.5.17 (acp)").unwrap();
        return;
    }
    if let Ok(pid_file) = std::env::var(FIXTURE_PID_FILE) {
        fs::write(pid_file, std::process::id().to_string()).unwrap();
    }
    let mode = std::env::var(FIXTURE_MODE).unwrap_or_default();
    let mut agent = Agent {
        out,
        log: std::env::var(FIXTURE_LOG).ok(),
        model: MODELS[0].to_string(),
        effort: EFFORTS[1].to_string(),
        mode: MODES[0].to_string(),
    };
    agent.log(&format!("args {args}"));
    for name in [
        "GROK_TELEMETRY_ENABLED",
        "GROK_TELEMETRY_TRACE_UPLOAD",
        "GROK_DISABLE_AUTOUPDATER",
        "AGY_CLI_DISABLE_AUTO_UPDATE",
    ] {
        if let Ok(value) = std::env::var(name) {
            agent.log(&format!("env {name}={value}"));
        }
    }
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    while let Some(Ok(line)) = lines.next() {
        agent.log(&format!("<- {line}"));
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = message.get("id").cloned();
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = id else {
            continue;
        };
        match method {
            "initialize" => {
                // A load-only agent reopens sessions with `session/load`.
                let capabilities = if mode == "acp-load-replay" {
                    json!({ "loadSession": true })
                } else {
                    json!({ "loadSession": true, "sessionCapabilities": { "resume": {} } })
                };
                agent.send(json!({ "id": id, "result": {
                    "protocolVersion": 1,
                    "agentCapabilities": capabilities,
                    "authMethods": [],
                } }));
            }
            "session/new" if mode == "acp-signed-out" => agent.send(json!({ "id": id,
                "error": { "code": -32000, "message": "Authentication required" } })),
            "session/new" => {
                let options = agent.options();
                agent.send(
                    json!({ "id": id, "result": { "sessionId": SESSION, "configOptions": options } }),
                );
            }
            "session/resume" => {
                let options = agent.options();
                agent.send(json!({ "id": id, "result": { "configOptions": options } }));
            }
            "session/load" => {
                let options = agent.options();
                agent.send(json!({ "id": id, "result": { "configOptions": options } }));
                // As some agents do, history keeps arriving after the reply.
                agent.say("Earlier reply.");
            }
            "session/set_config_option" => {
                let value = params["value"].as_str().unwrap_or_default().to_string();
                match params["configId"].as_str() {
                    Some("model") => agent.model = value,
                    Some("reasoning_effort") => agent.effort = value,
                    Some("mode") => agent.mode = value,
                    _ => {}
                }
                let options = agent.options();
                agent.send(json!({ "id": id, "result": { "configOptions": options } }));
            }
            "session/prompt" => {
                let prompt = params["prompt"][0]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let stop = match mode.as_str() {
                    "acp-approve" => {
                        let request = json!("permission-1");
                        agent.send(json!({ "id": request, "method": "session/request_permission",
                            "params": { "sessionId": SESSION,
                                "toolCall": { "toolCallId": "e1", "kind": "edit", "title": "Edit `src/lib.rs`",
                                    "rawInput": { "file_path": "src/lib.rs" } },
                                "options": [
                                    { "optionId": "allow-always", "name": "Always", "kind": "allow_always" },
                                    { "optionId": "allow-once", "name": "Once", "kind": "allow_once" },
                                    { "optionId": "reject-once", "name": "Reject", "kind": "reject_once" }] } }));
                        let reply = wait_for_reply(&agent, &mut lines, &request);
                        let chosen = reply["result"]["outcome"]["optionId"]
                            .as_str()
                            .unwrap_or("none")
                            .to_string();
                        agent.say(&format!("Chose: {chosen}"));
                        "end_turn"
                    }
                    "acp-question" => {
                        let request = json!("question-1");
                        agent.send(json!({ "id": request, "method": "elicitation/create",
                            "params": { "sessionId": SESSION, "toolCallId": "q", "mode": "form",
                                "message": "The agent has a question.",
                                "requestedSchema": { "type": "object", "properties": {
                                    "q1": { "type": "string", "title": "Do you prefer red or blue?",
                                        "oneOf": [{ "const": "1", "title": "Red" }, { "const": "2", "title": "Blue" }] },
                                    "q1_other": { "type": "string", "title": "Other" } } } } }));
                        let reply = wait_for_reply(&agent, &mut lines, &request);
                        agent.say(&format!("Answer: {}", reply["result"]));
                        "end_turn"
                    }
                    "acp-slow" => {
                        agent.update(json!({ "sessionUpdate": "tool_call", "toolCallId": "s1",
                            "kind": "execute", "status": "in_progress", "title": "sleep 30",
                            "rawInput": { "command": "sleep 30" } }));
                        for line in lines.by_ref().map_while(Result::ok) {
                            agent.log(&format!("<- {line}"));
                            if line.contains("\"session/cancel\"") {
                                break;
                            }
                        }
                        // As Grok does, end the command it started, which
                        // takes a moment, before the turn ends.
                        std::thread::sleep(std::time::Duration::from_millis(300));
                        agent.log("ended its own command");
                        "cancelled"
                    }
                    _ => {
                        agent.say("Working on it.");
                        agent.update(json!({ "sessionUpdate": "tool_call", "toolCallId": "c1",
                            "kind": "execute", "status": "in_progress", "title": "Execute `cargo test`",
                            "rawInput": { "command": "cargo test" } }));
                        agent.update(
                            json!({ "sessionUpdate": "tool_call_update", "toolCallId": "c1",
                            "status": "completed", "rawOutput": { "exit_code": 0 } }),
                        );
                        agent.update(json!({ "sessionUpdate": "tool_call", "toolCallId": "e1",
                            "kind": "edit", "status": "completed", "title": "Edit `src/lib.rs`",
                            "content": [{ "type": "diff", "path": "src/lib.rs", "oldText": "a", "newText": "b" }] }));
                        let model = agent.model.clone();
                        let effort = agent.effort.clone();
                        agent.say(&format!("Done: {prompt} ({model}, {effort})"));
                        "end_turn"
                    }
                };
                agent.send(json!({ "id": id, "result": { "stopReason": stop } }));
                if mode == "acp-wakes" {
                    // As Grok does when a background command ends: a turn
                    // of its own, with no prompt open.
                    agent.say("The background command finished.");
                }
            }
            _ => agent.send(
                json!({ "id": id, "error": { "code": -32601, "message": "Method not found" } }),
            ),
        }
    }
}
