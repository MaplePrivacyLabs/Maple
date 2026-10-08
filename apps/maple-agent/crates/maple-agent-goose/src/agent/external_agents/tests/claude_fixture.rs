//! Native CLI fixture for the Claude transport. Reuses the driver test binary.

use super::{
    FIXTURE_ARGS, FIXTURE_LOG, FIXTURE_MARKER, FIXTURE_MODE, FIXTURE_PID_FILE, fixture_output,
};
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, Write};
use std::process::Command;

fn log_record(log: &mut File, message: &Value) {
    // Maple may kill the fixture at any time. Append the JSON and newline
    // together so a partial formatted record cannot swallow the next launch.
    let mut line = serde_json::to_vec(message).unwrap();
    line.push(b'\n');
    log.write_all(&line).unwrap();
}

fn send(out: &mut File, message: Value) {
    writeln!(out, "{message}").unwrap();
    out.flush().unwrap();
}

fn finish(out: &mut File, session: &str, text: &str) {
    send(
        out,
        json!({"type": "assistant", "message": {
            "id": "message-1", "role": "assistant", "model": "fixture",
            "content": [{"type": "text", "text": text}],
        }}),
    );
    send(
        out,
        json!({"type": "result", "subtype": "success", "is_error": false,
            "session_id": session, "result": text,
        }),
    );
}

#[test]
#[ignore = "fake Claude CLI run by the driver tests"]
fn run() {
    if std::env::var_os(FIXTURE_MARKER).is_none() {
        return;
    }
    let mut out = fixture_output();
    let args = std::env::var(FIXTURE_ARGS).unwrap();
    let args: Vec<_> = args.split_whitespace().collect();
    if args.contains(&"--version") {
        writeln!(out, "2.1.270 (Claude Code)").unwrap();
        return;
    }
    let mode = std::env::var(FIXTURE_MODE).unwrap();
    if mode == "sleeper" {
        // A descendant the "slow" CLI leaves behind: this same binary,
        // so the test assumes no `sleep` on the host's paths.
        std::thread::sleep(std::time::Duration::from_secs(1000));
        return;
    }
    if args == ["auth", "status", "--json"] {
        fs::write(
            std::env::var(FIXTURE_PID_FILE).unwrap(),
            std::process::id().to_string(),
        )
        .unwrap();
        let exit = match mode.as_str() {
            "auth-out" => {
                send(&mut out, json!({"loggedIn": false}));
                1
            }
            "auth-error" => {
                writeln!(out, "secret-canary: unsupported command").unwrap();
                1
            }
            "auth-inconsistent" => {
                send(&mut out, json!({"loggedIn": true}));
                1
            }
            "auth-missing" => {
                send(&mut out, json!({"email": "secret-canary"}));
                0
            }
            "auth-oversized" => {
                send(
                    &mut out,
                    json!({"loggedIn": true, "extra": "x".repeat(20 * 1024)}),
                );
                0
            }
            "auth-timeout" => {
                std::thread::sleep(std::time::Duration::from_secs(30));
                0
            }
            _ => {
                send(
                    &mut out,
                    json!({"loggedIn": true, "email": "secret-canary"}),
                );
                0
            }
        };
        std::process::exit(exit);
    }
    let session = args
        .windows(2)
        .find_map(|pair| matches!(pair[0], "--session-id" | "--resume").then_some(pair[1]))
        .expect("a new or resumed Claude session");
    let pid_file = std::env::var(FIXTURE_PID_FILE).unwrap();
    fs::write(&pid_file, std::process::id().to_string()).unwrap();
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::var(FIXTURE_LOG).unwrap())
        .unwrap();
    log_record(
        &mut log,
        &json!({"args": args, "cwd": std::env::current_dir().unwrap()}),
    );

    for line in std::io::stdin().lock().lines() {
        let message: Value = serde_json::from_str(&line.unwrap()).unwrap();
        log_record(&mut log, &message);
        match message["type"].as_str() {
            Some("control_request") => {
                if mode == "init-error" {
                    send(
                        &mut out,
                        json!({"type": "control_response", "response": {
                            "subtype": "error", "request_id": message["request_id"], "error": "secret-canary",
                        }}),
                    );
                    continue;
                }
                send(
                    &mut out,
                    json!({"type": "control_response", "response": {
                        "subtype": "success", "request_id": message["request_id"], "response": {},
                    }}),
                );
                if message["request"]["subtype"] == "interrupt" {
                    break;
                }
            }
            Some("user") => {
                if mode == "pre-init-eof" {
                    return;
                }
                if mode == "pre-init-error" {
                    send(
                        &mut out,
                        json!({"type": "result", "subtype": "error_during_execution",
                            "is_error": true, "session_id": session, "errors": ["secret-canary"],
                        }),
                    );
                    continue;
                }
                if mode != "no-init-success" {
                    send(
                        &mut out,
                        json!({"type": "system", "subtype": "init", "session_id": session}),
                    );
                }
                match mode.as_str() {
                    "eof" => return,
                    "malformed" | "oversized" => {
                        let line = if mode == "malformed" {
                            "not-json".into()
                        } else {
                            "x".repeat(4 * 1024 * 1024 + 1)
                        };
                        writeln!(out, "{line}").unwrap();
                        out.flush().unwrap();
                        continue;
                    }
                    "error" => {
                        send(
                            &mut out,
                            json!({"type": "result", "subtype": "error_during_execution",
                                "is_error": true, "session_id": session, "errors": ["secret-canary"],
                            }),
                        );
                        continue;
                    }
                    "slow" => {
                        // Deliberately outlives the CLI to test Maple's process
                        // group cleanup, including descendants after interrupt.
                        #[allow(clippy::zombie_processes)]
                        let child = Command::new(std::env::current_exe().unwrap())
                            .args(std::env::args().skip(1))
                            .env(FIXTURE_MODE, "sleeper")
                            .spawn()
                            .unwrap();
                        fs::write(format!("{pid_file}.child"), child.id().to_string()).unwrap();
                        continue;
                    }
                    _ => {}
                }
                send(
                    &mut out,
                    json!({"type": "stream_event", "event": {
                        "type": "message_start", "message": {"id": "message-1"},
                    }}),
                );
                send(
                    &mut out,
                    json!({"type": "stream_event", "event": {
                        "type": "content_block_delta", "delta": {"type": "text_delta", "text": "Working"},
                    }}),
                );
                let (tool, input) = if matches!(mode.as_str(), "question" | "multi-question") {
                    (
                        "AskUserQuestion",
                        json!({"questions": [{"header": "Style", "question": "Tabs or spaces?",
                            "multiSelect": mode == "multi-question",
                            "options": [{"label": "Spaces", "description": "Use spaces"},
                                {"label": "Tabs", "description": "Use tabs"}],
                        }]}),
                    )
                } else {
                    ("Bash", json!({"command": "cargo test"}))
                };
                send(
                    &mut out,
                    json!({"type": "control_request", "request_id": "permission-1",
                        "request": {"subtype": "can_use_tool", "tool_name": tool, "input": input, "tool_use_id": "tool-1"},
                    }),
                );
            }
            Some("control_response") => {
                let answer = &message["response"]["response"];
                if matches!(mode.as_str(), "question" | "multi-question") {
                    finish(
                        &mut out,
                        session,
                        &answer["updatedInput"]["answers"].to_string(),
                    );
                    continue;
                }
                let allowed = answer["behavior"] == "allow";
                if allowed {
                    send(
                        &mut out,
                        json!({"type": "assistant", "message": {"id": "tools", "content": [
                            {"type": "tool_use", "id": "c1", "name": "Bash", "input": {"command": "cargo test"}},
                            {"type": "tool_use", "id": "f1", "name": "Edit", "input": {"file_path": "src/lib.rs"}},
                            {"type": "tool_use", "id": "todo", "name": "TodoWrite", "input": {"todos": [{"content": "Test", "status": "completed"}]}},
                        ]}}),
                    );
                    send(
                        &mut out,
                        json!({"type": "user", "message": {"content": [
                            {"type": "tool_result", "tool_use_id": "c1", "content": "ok"},
                            {"type": "tool_result", "tool_use_id": "f1", "content": "ok"},
                        ]}}),
                    );
                }
                finish(
                    &mut out,
                    session,
                    if allowed { "Allowed" } else { "Denied" },
                );
            }
            _ => panic!("unexpected Claude fixture input"),
        }
    }
    log_record(&mut log, &json!({"event": "stdin_closed"}));
}
