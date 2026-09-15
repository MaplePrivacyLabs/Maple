//! OpenAI Codex CLI as an external agent, driven through `codex app-server`.
//!
//! Codex uses the user's own sign-in and `~/.codex` configuration,
//! including its sandbox and approval settings. Maple passes it only the
//! prompt, the working directory, and one feature flag.

use super::super::AgentQuestion;
use super::super::developer_tools::parse_user_questions;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

pub(crate) const PROVIDER_ID: &str = "codex";
pub(crate) const PROVIDER_NAME: &str = "Codex";
const EXECUTABLE: &str = "codex";
/// The oldest Codex whose app-server speaks the v2 methods used here.
const MIN_VERSION: (u64, u64, u64) = (0, 143, 0);
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_VERSION_BYTES: usize = 4 * 1024;
/// The name the handshake reports. It is the reserved non-originating
/// client name that Paseo uses, so Codex attributes the requests to the
/// user's own client rather than to a third-party product.
const CLIENT_NAME: &str = "codex_app_server_daemon";
const CLIENT_TITLE: &str = "Codex App Server Daemon";
const CLIENT_VERSION: &str = "0.0.0";

/// What Maple found out about the Codex installation on this device.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CodexDetection {
    pub(crate) executable: Option<PathBuf>,
    pub(crate) version: Option<String>,
    /// `None` when Maple could not tell.
    pub(crate) signed_in: Option<bool>,
    /// Why the installation cannot be used, if it cannot.
    pub(crate) problem: Option<String>,
}

/// Find `codex` on `search_path`, run `codex --version`, and read whether
/// a sign-in exists. Nothing here changes the installation.
pub(crate) async fn detect(search_path: Option<&str>) -> CodexDetection {
    let Some(executable) = find_executable(search_path) else {
        return CodexDetection::default();
    };
    let version = match probe_version(&executable).await {
        Ok(version) => version,
        Err(error) => {
            return CodexDetection {
                executable: Some(executable),
                version: None,
                signed_in: None,
                problem: Some(format!("Maple could not run `codex --version`: {error}")),
            };
        }
    };
    let problem = match parse_version(&version) {
        Some(parsed) if parsed < MIN_VERSION => Some(format!(
            "Codex {version} is older than the {}.{}.{} that Maple needs. Update Codex.",
            MIN_VERSION.0, MIN_VERSION.1, MIN_VERSION.2
        )),
        Some(_) => None,
        None => Some(format!(
            "Maple could not read the Codex version from `{version}`"
        )),
    };
    CodexDetection {
        executable: Some(executable),
        version: Some(version),
        signed_in: Some(auth_file_exists()),
        problem,
    }
}

pub(crate) fn find_executable(search_path: Option<&str>) -> Option<PathBuf> {
    match search_path {
        Some(path) => super::super::developer_tools::executable_in_search_path(EXECUTABLE, path),
        None => super::super::developer_tools::executable_on_path(EXECUTABLE),
    }
}

async fn probe_version(executable: &Path) -> Result<String, String> {
    let mut command = tokio::process::Command::new(executable);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not start it: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "could not capture its output".to_string())?;
    let mut reader = tokio::spawn(super::super::bounded_process::read_bounded_stdout(
        stdout,
        MAX_VERSION_BYTES,
        "the Codex version",
    ));
    let status = match tokio::time::timeout(VERSION_PROBE_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            reader.abort();
            return Err(format!("could not wait for it: {error}"));
        }
        Err(_) => {
            reader.abort();
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err("it did not exit in time".to_string());
        }
    };
    let bytes = match tokio::time::timeout(VERSION_PROBE_TIMEOUT, &mut reader).await {
        Ok(Ok(result)) => result?,
        Ok(Err(error)) => return Err(format!("could not collect its output: {error}")),
        Err(_) => {
            reader.abort();
            return Err("its output did not close".to_string());
        }
    };
    if !status.success() {
        return Err(format!("it exited with {status}"));
    }
    let text = String::from_utf8_lossy(&bytes);
    let version = text.trim();
    if version.is_empty() {
        return Err("it printed nothing".to_string());
    }
    Ok(version.to_string())
}

/// `codex-cli 0.153.4` and plain `0.153.4` both read as (0, 153, 4).
pub(crate) fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    text.split_whitespace().rev().find_map(|token| {
        let token = token.trim_start_matches('v');
        let core = token.split(['-', '+']).next()?;
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next().unwrap_or("0").parse().ok()?;
        Some((major, minor, patch))
    })
}

/// Codex keeps its sign-in in `auth.json` under its home. Maple only reads
/// whether the file exists; it never opens it.
fn auth_file_exists() -> bool {
    codex_home().is_some_and(|home| home.join("auth.json").is_file())
}

fn codex_home() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("CODEX_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(home));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|value| !value.is_empty())?;
    Some(PathBuf::from(home).join(".codex"))
}

pub(crate) fn sign_in_hint() -> &'static str {
    "Codex is not signed in. Run `codex login` in a terminal, then try again."
}

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
/// Maple answers those questions through its question card, so it turns
/// the feature on for the default mode too.
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

pub(super) struct TurnRequest<'a> {
    pub(super) thread_id: &'a str,
    pub(super) prompt: &'a str,
    pub(super) cwd: &'a Path,
    pub(super) model: Option<&'a str>,
    pub(super) effort: Option<&'a str>,
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
#[derive(Debug, Clone, PartialEq)]
pub(super) enum CodexEvent {
    ThreadStarted {
        thread_id: String,
    },
    TurnStarted {
        turn_id: Option<String>,
    },
    TurnCompleted {
        status: String,
        error: Option<String>,
    },
    AgentMessageDelta {
        item_id: String,
        delta: String,
    },
    ItemStarted(CodexItem),
    ItemCompleted(CodexItem),
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum CodexItem {
    AgentMessage {
        id: String,
        text: String,
    },
    CommandExecution {
        id: String,
        command: String,
        exit_code: Option<i64>,
        status: Option<String>,
    },
    FileChange {
        id: String,
        changes: Vec<FileChangeEntry>,
        status: Option<String>,
    },
    TodoList {
        id: String,
        items: Vec<TodoEntry>,
    },
    /// In the default collaboration mode Codex asks the user without
    /// blocking: the question rides an agent message, the turn ends, and
    /// the answer comes back as the next turn's input.
    AsyncQuestion {
        id: String,
        questions: Vec<AsyncQuestion>,
    },
    Other {
        id: String,
        kind: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AsyncQuestion {
    pub(super) title: String,
    pub(super) options: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FileChangeEntry {
    pub(super) path: String,
    pub(super) kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TodoEntry {
    pub(super) text: String,
    pub(super) completed: bool,
}

pub(super) fn parse_notification(method: &str, params: &Value) -> CodexEvent {
    match method {
        "thread/started" => match params
            .get("thread")
            .and_then(|thread| thread.get("id"))
            .and_then(Value::as_str)
        {
            Some(thread_id) => CodexEvent::ThreadStarted {
                thread_id: thread_id.to_string(),
            },
            None => CodexEvent::Other,
        },
        "turn/started" => CodexEvent::TurnStarted {
            turn_id: params
                .get("turn")
                .and_then(|turn| turn.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string),
        },
        "turn/completed" => {
            let turn = params.get("turn").cloned().unwrap_or(Value::Null);
            CodexEvent::TurnCompleted {
                status: turn
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("completed")
                    .to_string(),
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
            CodexEvent::AgentMessageDelta { item_id, delta }
        }
        "item/started" => match params.get("item").map(parse_item) {
            Some(item) => CodexEvent::ItemStarted(item),
            None => CodexEvent::Other,
        },
        "item/completed" => match params.get("item").map(parse_item) {
            Some(item) => CodexEvent::ItemCompleted(item),
            None => CodexEvent::Other,
        },
        _ => CodexEvent::Other,
    }
}

fn parse_item(item: &Value) -> CodexItem {
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
                return CodexItem::AsyncQuestion { id, questions };
            }
            CodexItem::AgentMessage {
                id,
                text: item
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            }
        }
        "commandexecution" => CodexItem::CommandExecution {
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
        "filechange" => CodexItem::FileChange {
            id,
            changes: parse_file_changes(item.get("changes")),
            status: item
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_string),
        },
        "todolist" | "plan" => CodexItem::TodoList {
            id,
            items: item
                .get("items")
                .or_else(|| item.get("plan"))
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(parse_todo).collect())
                .unwrap_or_default(),
        },
        _ => CodexItem::Other {
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

fn command_line(command: Option<&Value>) -> String {
    match command {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
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

/// The questions Maple shows for an async question, in the shape its
/// question card already understands.
pub(super) fn async_question_prompts(questions: &[AsyncQuestion]) -> Vec<AgentQuestion> {
    questions
        .iter()
        .enumerate()
        .map(|(index, question)| AgentQuestion {
            id: format!("q{index}"),
            header: format!("Question {}", index + 1),
            question: question.title.clone(),
            options: question
                .options
                .iter()
                .map(|label| super::super::AgentQuestionOption {
                    label: label.clone(),
                    description: String::new(),
                })
                .collect(),
        })
        .collect()
}

/// The user's answers as the next turn's input, or `None` when every
/// question was dismissed. `answer` is the card's response JSON.
pub(super) fn async_answer_prompt(questions: &[AsyncQuestion], answer: &str) -> Option<String> {
    let parsed = serde_json::from_str::<Value>(answer).ok()?;
    let answers = parsed.get("answers")?;
    let mut lines = Vec::new();
    for (index, question) in questions.iter().enumerate() {
        let values = answers
            .get(format!("q{index}"))
            .and_then(|entry| entry.get("answers"))
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        if values.is_empty() {
            continue;
        }
        lines.push(format!("{}\n{values}", question.title));
    }
    if lines.is_empty() {
        return None;
    }
    Some(format!(
        "Answers to your questions:\n\n{}",
        lines.join("\n\n")
    ))
}

pub(super) fn turn_steer_params(thread_id: &str, turn_id: &str, text: &str) -> Value {
    json!({
        "threadId": thread_id,
        "expectedTurnId": turn_id,
        "input": [{ "type": "text", "text": text, "text_elements": [] }],
    })
}

fn parse_todo(todo: &Value) -> Option<TodoEntry> {
    let text = ["text", "step", "content"]
        .into_iter()
        .find_map(|key| todo.get(key).and_then(Value::as_str))?
        .to_string();
    let completed = todo
        .get("completed")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| {
            matches!(
                todo.get("status").and_then(Value::as_str),
                Some("completed" | "done")
            )
        });
    Some(TodoEntry { text, completed })
}

/// A request Codex makes of its client mid-turn.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum CodexServerRequest {
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

pub(super) fn parse_server_request(method: &str, params: &Value) -> CodexServerRequest {
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

/// The answer to an approval request. Maple never grants `acceptForSession`:
/// every decision is one-shot, like Maple's own permissions.
pub(super) fn approval_response(decision: super::super::AgentPermissionDecision) -> Value {
    use super::super::AgentPermissionDecision;
    let decision = match decision {
        AgentPermissionDecision::AllowOnce => "accept",
        AgentPermissionDecision::DenyOnce => "decline",
        AgentPermissionDecision::Cancel => "cancel",
    };
    json!({ "decision": decision })
}

/// The answer to a question. The question card composes Codex's own
/// response shape, so an answer that already is one passes through.
pub(super) fn user_input_response(answer: &str) -> Value {
    if let Ok(value) = serde_json::from_str::<Value>(answer)
        && value.get("answers").is_some()
    {
        return value;
    }
    json!({ "answers": {} })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions_in_the_shapes_codex_prints() {
        assert_eq!(parse_version("codex-cli 0.153.4"), Some((0, 153, 4)));
        assert_eq!(parse_version("0.143.0"), Some((0, 143, 0)));
        assert_eq!(parse_version("v1.2.3-alpha.1"), Some((1, 2, 3)));
        assert_eq!(parse_version("codex"), None);
        assert!(parse_version("codex-cli 0.142.9").unwrap() < MIN_VERSION);
    }

    #[test]
    fn turn_start_carries_prompt_and_overrides_but_no_policy() {
        let params = turn_start_params(&TurnRequest {
            thread_id: "t1",
            prompt: "fix it",
            cwd: Path::new("/p/sub"),
            model: Some("gpt-5.4"),
            effort: Some("high"),
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
            CodexEvent::ItemStarted(CodexItem::CommandExecution {
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
            CodexEvent::ItemCompleted(CodexItem::CommandExecution {
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
            CodexEvent::ItemCompleted(CodexItem::FileChange {
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
            CodexEvent::ItemCompleted(CodexItem::TodoList {
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
            CodexEvent::AgentMessageDelta {
                item_id: "m".into(),
                delta: "hi".into()
            }
        );
        assert_eq!(
            parse_notification(
                "turn/completed",
                &json!({"turn": {"status": "failed", "error": {"message": "boom"}}})
            ),
            CodexEvent::TurnCompleted {
                status: "failed".into(),
                error: Some("boom".into())
            }
        );
        assert_eq!(
            parse_notification("thread/tokenUsage/updated", &json!({})),
            CodexEvent::Other
        );
    }

    #[test]
    fn parses_async_questions_and_builds_the_answer_turn() {
        let event = parse_notification(
            "item/completed",
            &json!({"item": {"id": "aq", "type": "agentMessage", "text": "Tabs or spaces?", "delivery": "async",
                "questions": [{"title": "Tabs or spaces?", "options": ["Tabs", "Spaces"]}, {"title": "Why?", "options": null}]}}),
        );
        let CodexEvent::ItemCompleted(CodexItem::AsyncQuestion { id, questions }) = event else {
            panic!("expected an async question");
        };
        assert_eq!(id, "aq");
        assert_eq!(questions.len(), 2);
        let prompts = async_question_prompts(&questions);
        assert_eq!(prompts[0].id, "q0");
        assert_eq!(prompts[0].options[1].label, "Spaces");
        assert!(prompts[1].options.is_empty());
        let prompt = async_answer_prompt(
            &questions,
            r#"{"answers":{"q0":{"answers":["Spaces"]},"q1":{"answers":["habit"]}}}"#,
        )
        .unwrap();
        assert_eq!(
            prompt,
            "Answers to your questions:\n\nTabs or spaces?\nSpaces\n\nWhy?\nhabit"
        );
        assert!(async_answer_prompt(&questions, "").is_none());
        assert!(async_answer_prompt(&questions, r#"{"answers":{}}"#).is_none());
        // A plain message keeps being a message.
        assert!(matches!(
            parse_notification(
                "item/completed",
                &json!({"item": {"id": "m", "type": "agentMessage", "text": "hi"}})
            ),
            CodexEvent::ItemCompleted(CodexItem::AgentMessage { .. })
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
            CodexEvent::ItemCompleted(CodexItem::FileChange { changes, .. }) => {
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
        use super::super::super::AgentPermissionDecision;
        assert_eq!(
            approval_response(AgentPermissionDecision::AllowOnce)["decision"],
            "accept"
        );
        assert_eq!(
            approval_response(AgentPermissionDecision::DenyOnce)["decision"],
            "decline"
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
