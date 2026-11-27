//! OpenAI Codex CLI as an external agent, driven through `codex app-server`.
//!
//! Codex uses the user's own sign-in and `~/.codex` configuration,
//! including its sandbox and approval settings. Maple passes it only the
//! prompt, the working directory, and one feature flag. Finding and probing
//! the CLI is the integration catalog's (`integrations::detect`).

use super::super::tools::parse_user_questions;
use super::super::{AgentPermissionDecision, AgentQuestion};
use super::driver::{
    AgentEvent, AgentItem, AgentRequest, AsyncQuestion, FileChangeEntry, TurnOutcome, TurnRequest,
    card_answers, command_line, parse_todo,
};
use serde_json::{Map, Value, json};
use std::path::Path;

/// What Codex says Maple is asked to approve.
const COMMAND_TOOL: &str = "codex_command";
const FILE_CHANGE_TOOL: &str = "codex_file_change";
/// The name the handshake reports. It is the reserved non-originating
/// client name that Paseo uses, so Codex attributes the requests to the
/// user's own client rather than to a third-party product.
const CLIENT_NAME: &str = "codex_app_server_daemon";
const CLIENT_TITLE: &str = "Codex App Server Daemon";
const CLIENT_VERSION: &str = "0.0.0";

/// The command line that starts the app-server on stdio.
pub(super) fn app_server_args() -> [&'static str; 1] {
    ["app-server"]
}

pub(super) fn initialize_params() -> Value {
    json!({
        "clientInfo": {
            "name": CLIENT_NAME,
            "title": CLIENT_TITLE,
            "version": CLIENT_VERSION,
        },
        "capabilities": {
            "experimentalApi": true,
        },
    })
}

/// Configuration Maple sets for every thread and turn. Codex offers its
/// `request_user_input` tool only in plan mode unless this feature is on;
/// Maple hands those questions to the model that called the agent, so it
/// turns the feature on for the default mode too.
fn session_config() -> Value {
    json!({
        "features": {
            "default_mode_request_user_input": true,
        },
    })
}

pub(super) fn thread_start_params(cwd: &Path, model: Option<&str>) -> Value {
    let mut params = json!({
        "cwd": cwd.to_string_lossy(),
        "config": session_config(),
    });
    if let Some(model) = model {
        params["model"] = json!(model);
    }
    params
}

pub(super) fn thread_resume_params(thread_id: &str) -> Value {
    json!({ "threadId": thread_id })
}

pub(super) fn turn_start_params(request: &TurnRequest<'_>) -> Value {
    let mut params = json!({
        "threadId": request.thread_id,
        "input": [{ "type": "text", "text": request.prompt, "text_elements": [] }],
        "cwd": request.cwd.to_string_lossy(),
        "config": session_config(),
    });
    if let Some(model) = request.model {
        params["model"] = json!(model);
    }
    if let Some(effort) = request.effort {
        params["effort"] = json!(effort);
    }
    params
}

pub(super) fn turn_interrupt_params(thread_id: &str, turn_id: &str) -> Value {
    json!({ "threadId": thread_id, "turnId": turn_id })
}

/// The thread ID a `thread/start` or `thread/resume` response carries.
pub(super) fn thread_id_from_response(response: &Value) -> Option<String> {
    response
        .get("thread")
        .and_then(|thread| thread.get("id"))
        .or_else(|| response.get("threadId"))
        .or_else(|| response.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// One Codex notification, reduced to what Maple shows.
pub(super) fn parse_notification(method: &str, params: &Value) -> AgentEvent {
    match method {
        "thread/started" => match params
            .get("thread")
            .and_then(|thread| thread.get("id"))
            .and_then(Value::as_str)
        {
            Some(thread_id) => AgentEvent::ThreadStarted {
                thread_id: thread_id.to_string(),
            },
            None => AgentEvent::Other,
        },
        "turn/started" => AgentEvent::TurnStarted {
            turn_id: params
                .get("turn")
                .and_then(|turn| turn.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string),
        },
        "turn/completed" => {
            let turn = params.get("turn").cloned().unwrap_or(Value::Null);
            let outcome = match turn
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("completed")
            {
                "completed" => TurnOutcome::Completed,
                "interrupted" | "cancelled" | "canceled" => TurnOutcome::Cancelled,
                _ => TurnOutcome::Failed,
            };
            AgentEvent::TurnCompleted {
                outcome,
                error: turn
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
            }
        }
        "item/agentMessage/delta" => {
            let item_id = params
                .get("itemId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let delta = params
                .get("delta")
                .or_else(|| params.get("textDelta"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            AgentEvent::AgentMessageDelta { item_id, delta }
        }
        "item/started" => match params.get("item").map(parse_item) {
            Some(item) => AgentEvent::ItemStarted(item),
            None => AgentEvent::Other,
        },
        "item/completed" => match params.get("item").map(parse_item) {
            Some(item) => AgentEvent::ItemCompleted(item),
            None => AgentEvent::Other,
        },
        _ => AgentEvent::Other,
    }
}

fn parse_item(item: &Value) -> AgentItem {
    let id = item
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
    match normalize_item_type(kind).as_str() {
        "agentmessage" => {
            let questions = item
                .get("questions")
                .and_then(Value::as_array)
                .map(|questions| {
                    questions
                        .iter()
                        .filter_map(parse_async_question)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if item.get("delivery").and_then(Value::as_str) == Some("async")
                && !questions.is_empty()
            {
                return AgentItem::AsyncQuestion { id, questions };
            }
            AgentItem::AgentMessage {
                id,
                text: item
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            }
        }
        "commandexecution" => AgentItem::CommandExecution {
            id,
            command: command_line(item.get("command")),
            exit_code: item
                .get("exitCode")
                .or_else(|| item.get("exit_code"))
                .and_then(Value::as_i64),
            status: item
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_string),
        },
        "filechange" => AgentItem::FileChange {
            id,
            changes: parse_file_changes(item.get("changes")),
            status: item
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_string),
        },
        "todolist" | "plan" => AgentItem::TodoList {
            id,
            items: item
                .get("items")
                .or_else(|| item.get("plan"))
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(parse_todo).collect())
                .unwrap_or_default(),
        },
        _ => AgentItem::Other {
            id,
            kind: kind.to_string(),
        },
    }
}

/// `CommandExecution`, `commandExecution`, and `command_execution` are one
/// type; Codex has spelled them all.
fn normalize_item_type(kind: &str) -> String {
    kind.chars()
        .filter(|character| *character != '_')
        .flat_map(char::to_lowercase)
        .collect()
}

/// Codex has sent `changes` both as a list of `{path, kind}` entries and as
/// a map from path to change; read either.
fn parse_file_changes(changes: Option<&Value>) -> Vec<FileChangeEntry> {
    match changes {
        Some(Value::Array(entries)) => entries.iter().filter_map(parse_file_change).collect(),
        Some(Value::Object(by_path)) => by_path
            .iter()
            .filter_map(|(path, change)| {
                let path = path.trim();
                if path.is_empty() {
                    return None;
                }
                Some(FileChangeEntry {
                    path: path.to_string(),
                    kind: change_kind(change.get("kind").or_else(|| change.get("type"))),
                })
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn change_kind(kind: Option<&Value>) -> String {
    match kind {
        Some(Value::String(kind)) => kind.clone(),
        Some(Value::Object(kind)) => kind
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("update")
            .to_string(),
        _ => "update".to_string(),
    }
}

fn parse_file_change(change: &Value) -> Option<FileChangeEntry> {
    let path = ["path", "file_path", "filePath"]
        .into_iter()
        .find_map(|key| change.get(key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|path| !path.is_empty())?;
    let kind = change_kind(change.get("kind").or_else(|| change.get("type")));
    Some(FileChangeEntry {
        path: path.to_string(),
        kind,
    })
}

fn parse_async_question(question: &Value) -> Option<AsyncQuestion> {
    let title = question.get("title").and_then(Value::as_str)?.trim();
    if title.is_empty() {
        return None;
    }
    Some(AsyncQuestion {
        title: title.to_string(),
        options: question
            .get("options")
            .and_then(Value::as_array)
            .map(|options| {
                options
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// A request Codex makes of its client mid-turn.
#[derive(Debug, Clone, PartialEq)]
enum CodexServerRequest {
    CommandApproval {
        item_id: String,
        command: String,
        cwd: Option<String>,
        reason: Option<String>,
    },
    FileChangeApproval {
        item_id: String,
        reason: Option<String>,
    },
    UserInput {
        questions: Vec<AgentQuestion>,
    },
    Unknown {
        method: String,
    },
}

fn parse_server_request(method: &str, params: &Value) -> CodexServerRequest {
    let item_id = params
        .get("itemId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let reason = params
        .get("reason")
        .and_then(Value::as_str)
        .map(str::to_string);
    match method {
        "item/commandExecution/requestApproval" => CodexServerRequest::CommandApproval {
            item_id,
            command: command_line(params.get("command")),
            cwd: params
                .get("cwd")
                .and_then(Value::as_str)
                .map(str::to_string),
            reason,
        },
        "item/fileChange/requestApproval" => {
            CodexServerRequest::FileChangeApproval { item_id, reason }
        }
        "item/tool/requestUserInput" | "tool/requestUserInput" => {
            let entries = params
                .get("questions")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            CodexServerRequest::UserInput {
                questions: parse_user_questions(&entries),
            }
        }
        _ => CodexServerRequest::Unknown {
            method: method.to_string(),
        },
    }
}

/// A request Codex makes, in Maple's terms: an approval says what Codex
/// wants to do the way Maple's permission request carries it.
pub(super) fn agent_request(method: &str, params: &Value) -> AgentRequest {
    match parse_server_request(method, params) {
        CodexServerRequest::CommandApproval {
            item_id,
            command,
            cwd,
            reason,
        } => {
            let mut arguments = Map::new();
            arguments.insert("command".to_string(), json!(command));
            if let Some(cwd) = cwd {
                arguments.insert("cwd".to_string(), json!(cwd));
            }
            if let Some(reason) = &reason {
                arguments.insert("reason".to_string(), json!(reason));
            }
            AgentRequest::Approval {
                item_id,
                tool_name: COMMAND_TOOL.to_string(),
                arguments,
                prompt: Some(format!("Codex wants to run: {command}")),
            }
        }
        CodexServerRequest::FileChangeApproval { item_id, reason } => {
            let mut arguments = Map::new();
            if let Some(reason) = &reason {
                arguments.insert("reason".to_string(), json!(reason));
            }
            AgentRequest::Approval {
                item_id,
                tool_name: FILE_CHANGE_TOOL.to_string(),
                arguments,
                prompt: Some("Codex wants to change files in the project".to_string()),
            }
        }
        CodexServerRequest::UserInput { questions } => AgentRequest::UserInput { questions },
        CodexServerRequest::Unknown { method } => AgentRequest::Unknown { method },
    }
}

/// The answer to an approval request. Maple never grants `acceptForSession`:
/// every acceptance is one-shot.
pub(super) fn approval_response(decision: AgentPermissionDecision) -> Value {
    let decision = match decision {
        AgentPermissionDecision::AllowOnce => "accept",
        AgentPermissionDecision::Cancel => "cancel",
    };
    json!({ "decision": decision })
}

/// The answer to a question. Maple's answers JSON is Codex's own response
/// shape, so it passes through.
pub(super) fn user_input_response(answer: &str) -> Value {
    card_answers(answer)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::agent::AgentPermissionMode;
    use crate::agent::external_agents::driver::TodoEntry;

    #[test]
    fn turn_start_carries_prompt_and_overrides_but_no_policy() {
        let params = turn_start_params(&TurnRequest {
            thread_id: "t1",
            prompt: "fix it",
            cwd: Path::new("/p/sub"),
            model: Some("gpt-5.4"),
            effort: Some("high"),
            mode: AgentPermissionMode::ReadOnly,
        });
        assert_eq!(params["threadId"], "t1");
        assert_eq!(params["input"][0]["text"], "fix it");
        assert_eq!(params["cwd"], "/p/sub");
        // Codex's own configuration decides sandbox and approvals.
        assert!(params.get("approvalPolicy").is_none());
        assert!(params.get("sandboxPolicy").is_none());
        assert_eq!(params["model"], "gpt-5.4");
        assert_eq!(params["effort"], "high");
        assert_eq!(
            params["config"]["features"]["default_mode_request_user_input"],
            true
        );
        let bare = turn_start_params(&TurnRequest {
            thread_id: "t1",
            prompt: "x",
            cwd: Path::new("/p"),
            model: None,
            effort: None,
            mode: AgentPermissionMode::ReadOnly,
        });
        assert!(bare.get("model").is_none());
        assert!(bare.get("effort").is_none());
    }

    #[test]
    fn parses_items_in_every_spelling() {
        let started = parse_notification(
            "item/started",
            &json!({"item": {"id": "c1", "type": "CommandExecution", "command": ["cargo", "test"]}}),
        );
        assert_eq!(
            started,
            AgentEvent::ItemStarted(AgentItem::CommandExecution {
                id: "c1".into(),
                command: "cargo test".into(),
                exit_code: None,
                status: None,
            })
        );
        let completed = parse_notification(
            "item/completed",
            &json!({"item": {"id": "c1", "type": "command_execution", "command": "cargo test", "exit_code": 1, "status": "failed"}}),
        );
        assert_eq!(
            completed,
            AgentEvent::ItemCompleted(AgentItem::CommandExecution {
                id: "c1".into(),
                command: "cargo test".into(),
                exit_code: Some(1),
                status: Some("failed".into()),
            })
        );
        let file = parse_notification(
            "item/completed",
            &json!({"item": {"id": "f1", "type": "fileChange", "status": "completed",
                "changes": [{"path": "src/a.rs", "kind": {"type": "add"}}, {"file_path": "b.rs"}]}}),
        );
        assert_eq!(
            file,
            AgentEvent::ItemCompleted(AgentItem::FileChange {
                id: "f1".into(),
                changes: vec![
                    FileChangeEntry {
                        path: "src/a.rs".into(),
                        kind: "add".into()
                    },
                    FileChangeEntry {
                        path: "b.rs".into(),
                        kind: "update".into()
                    },
                ],
                status: Some("completed".into()),
            })
        );
        let todo = parse_notification(
            "item/completed",
            &json!({"item": {"id": "p1", "type": "todoList", "items": [{"text": "one", "completed": true}, {"text": "two"}]}}),
        );
        assert_eq!(
            todo,
            AgentEvent::ItemCompleted(AgentItem::TodoList {
                id: "p1".into(),
                items: vec![
                    TodoEntry {
                        text: "one".into(),
                        completed: true
                    },
                    TodoEntry {
                        text: "two".into(),
                        completed: false
                    },
                ],
            })
        );
        assert_eq!(
            parse_notification(
                "item/agentMessage/delta",
                &json!({"itemId": "m", "delta": "hi"})
            ),
            AgentEvent::AgentMessageDelta {
                item_id: "m".into(),
                delta: "hi".into()
            }
        );
        assert_eq!(
            parse_notification(
                "turn/completed",
                &json!({"turn": {"status": "failed", "error": {"message": "boom"}}})
            ),
            AgentEvent::TurnCompleted {
                outcome: TurnOutcome::Failed,
                error: Some("boom".into())
            }
        );
        assert_eq!(
            parse_notification("thread/tokenUsage/updated", &json!({})),
            AgentEvent::Other
        );
    }

    #[test]
    fn turn_statuses_become_outcomes() {
        for (status, outcome) in [
            (json!("completed"), TurnOutcome::Completed),
            (json!("interrupted"), TurnOutcome::Cancelled),
            (json!("canceled"), TurnOutcome::Cancelled),
            (json!("failed"), TurnOutcome::Failed),
            (Value::Null, TurnOutcome::Completed),
        ] {
            let mut turn = json!({});
            if !status.is_null() {
                turn["status"] = status;
            }
            assert_eq!(
                parse_notification("turn/completed", &json!({ "turn": turn })),
                AgentEvent::TurnCompleted {
                    outcome,
                    error: None
                }
            );
        }
    }

    #[test]
    fn approvals_carry_codex_terms() {
        assert_eq!(
            agent_request(
                "item/commandExecution/requestApproval",
                &json!({"itemId": "i1", "command": ["rm", "-rf", "build"], "cwd": "/p"})
            ),
            AgentRequest::Approval {
                item_id: "i1".into(),
                tool_name: "codex_command".into(),
                arguments: json!({"command": "rm -rf build", "cwd": "/p"})
                    .as_object()
                    .cloned()
                    .unwrap(),
                prompt: Some("Codex wants to run: rm -rf build".into()),
            }
        );
        assert!(matches!(
            agent_request("item/fileChange/requestApproval", &json!({"itemId": "i2"})),
            AgentRequest::Approval { tool_name, prompt: Some(_), .. } if tool_name == "codex_file_change"
        ));
        assert!(matches!(
            agent_request("mcpServer/elicitation/request", &json!({})),
            AgentRequest::Unknown { method } if method == "mcpServer/elicitation/request"
        ));
    }

    #[test]
    fn parses_async_questions() {
        let event = parse_notification(
            "item/completed",
            &json!({"item": {"id": "aq", "type": "agentMessage", "text": "Tabs or spaces?", "delivery": "async",
                "questions": [{"title": "Tabs or spaces?", "options": ["Tabs", "Spaces"]}, {"title": "Why?", "options": null}]}}),
        );
        let AgentEvent::ItemCompleted(AgentItem::AsyncQuestion { id, questions }) = event else {
            panic!("expected an async question");
        };
        assert_eq!(id, "aq");
        assert_eq!(questions.len(), 2);
        let prompts = super::super::driver::async_question_prompts(&questions);
        assert_eq!(prompts[0].id, "q0");
        assert_eq!(prompts[0].options[1].label, "Spaces");
        assert!(prompts[1].options.is_empty());
        // A plain message keeps being a message.
        assert!(matches!(
            parse_notification(
                "item/completed",
                &json!({"item": {"id": "m", "type": "agentMessage", "text": "hi"}})
            ),
            AgentEvent::ItemCompleted(AgentItem::AgentMessage { .. })
        ));
    }

    #[test]
    fn parses_file_changes_keyed_by_path() {
        let mapped = parse_notification(
            "item/completed",
            &json!({"item": {"id": "f2", "type": "fileChange",
                "changes": {"src/c.rs": {"kind": {"type": "delete"}}, "d.rs": {"type": "add"}}}}),
        );
        match mapped {
            AgentEvent::ItemCompleted(AgentItem::FileChange { changes, .. }) => {
                let mut paths = changes
                    .iter()
                    .map(|change| (change.path.as_str(), change.kind.as_str()))
                    .collect::<Vec<_>>();
                paths.sort();
                assert_eq!(paths, vec![("d.rs", "add"), ("src/c.rs", "delete")]);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parses_server_requests() {
        assert_eq!(
            parse_server_request(
                "item/commandExecution/requestApproval",
                &json!({"itemId": "i1", "command": "rm -rf build", "cwd": "/p", "reason": "cleanup"})
            ),
            CodexServerRequest::CommandApproval {
                item_id: "i1".into(),
                command: "rm -rf build".into(),
                cwd: Some("/p".into()),
                reason: Some("cleanup".into()),
            }
        );
        assert_eq!(
            parse_server_request("item/fileChange/requestApproval", &json!({"itemId": "i2"})),
            CodexServerRequest::FileChangeApproval {
                item_id: "i2".into(),
                reason: None
            }
        );
        match parse_server_request(
            "item/tool/requestUserInput",
            &json!({"itemId": "i3", "questions": [{"id": "q1", "header": "Scope", "question": "Which?", "options": [{"label": "A", "description": "a"}]}]}),
        ) {
            CodexServerRequest::UserInput { questions } => {
                assert_eq!(questions.len(), 1);
                assert_eq!(questions[0].id, "q1");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(
            parse_server_request("mcpServer/elicitation/request", &json!({})),
            CodexServerRequest::Unknown {
                method: "mcpServer/elicitation/request".into()
            }
        );
    }

    #[test]
    fn responses_use_codex_decisions() {
        assert_eq!(
            approval_response(AgentPermissionDecision::AllowOnce)["decision"],
            "accept"
        );
        assert_eq!(
            approval_response(AgentPermissionDecision::Cancel)["decision"],
            "cancel"
        );
        assert_eq!(
            user_input_response(r#"{"answers":{"q1":{"answers":["A"]}}}"#)["answers"]["q1"]["answers"]
                [0],
            "A"
        );
        assert_eq!(user_input_response("")["answers"], json!({}));
    }
}
