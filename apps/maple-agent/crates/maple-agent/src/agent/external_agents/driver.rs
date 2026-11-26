//! What Maple's external-agent host and the transports that drive each kind
//! of agent process say to each other. The host speaks only this; a
//! transport turns its own wire into it and back, so nothing in the host
//! depends on which agent runs.

use std::path::Path;

use serde_json::{Map, Value, json};

use super::super::{
    AgentPermissionDecision, AgentPermissionMode, AgentQuestion, AgentQuestionOption,
};

/// One thing an agent reported, reduced to what Maple shows.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum AgentEvent {
    /// The agent confirmed the thread it works in.
    ThreadStarted {
        thread_id: String,
    },
    TurnStarted {
        turn_id: Option<String>,
    },
    TurnCompleted {
        outcome: TurnOutcome,
        error: Option<String>,
    },
    AgentMessageDelta {
        item_id: String,
        delta: String,
    },
    ItemStarted(AgentItem),
    ItemCompleted(AgentItem),
    Other,
}

/// One piece of an agent's work.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum AgentItem {
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
    /// A question that does not hold the turn: it rides an agent message,
    /// the turn ends, and the answer comes back as the next input.
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

/// The end of one turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TurnOutcome {
    Completed,
    Failed,
    Cancelled,
}

impl TurnOutcome {
    pub(super) fn status(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub(super) fn verb(self) -> &'static str {
        match self {
            Self::Completed => "finished",
            Self::Failed => "failed",
            Self::Cancelled => "was interrupted",
        }
    }
}

/// Something the agent asks of Maple mid-turn, which waits for a
/// [`AgentReply`].
#[derive(Debug, Clone, PartialEq)]
pub(super) enum AgentRequest {
    /// Approve one action. The transport says what the action is in the
    /// terms Maple's permission request carries.
    Approval {
        /// The agent's id for the action, unique within the agent.
        item_id: String,
        tool_name: String,
        arguments: Map<String, Value>,
        prompt: Option<String>,
    },
    /// The agent asks to change its own mode. Maple refuses: it sets the
    /// agent's mode from the task's.
    ModeChange {
        /// The agent's id for the request's tool call.
        item_id: String,
    },
    /// Questions for the model that called the agent.
    UserInput { questions: Vec<AgentQuestion> },
    /// A request Maple does not serve.
    Unknown { method: String },
}

impl AgentRequest {
    /// What the request is, for the log.
    pub(super) fn kind(&self) -> &str {
        match self {
            Self::Approval { tool_name, .. } => tool_name,
            Self::ModeChange { .. } => "mode change",
            Self::UserInput { .. } => "user input",
            Self::Unknown { method } => method,
        }
    }
}

/// Maple's answer to an [`AgentRequest`].
#[derive(Debug, Clone, PartialEq)]
pub(super) enum AgentReply {
    Decision(AgentPermissionDecision),
    /// The caller's answers as JSON, `{"answers": {question id: {"answers":
    /// [...]}}}` as Maple's question card also answers; empty when nobody
    /// answered.
    Answers(String),
}

impl AgentReply {
    /// What was answered, for the log.
    pub(super) fn label(&self) -> &'static str {
        match self {
            Self::Decision(AgentPermissionDecision::AllowOnce) => "accept",
            Self::Decision(AgentPermissionDecision::Cancel) => "cancel",
            Self::Answers(_) => "answers",
        }
    }
}

/// What a transport hands the host from its agent process.
#[derive(Debug)]
pub(super) enum DriverMessage {
    Event(AgentEvent),
    /// The host answers with the same `id`.
    Request {
        id: Value,
        request: AgentRequest,
    },
}

/// One turn's input.
pub(super) struct TurnRequest<'a> {
    pub(super) thread_id: &'a str,
    pub(super) prompt: &'a str,
    pub(super) cwd: &'a Path,
    pub(super) model: Option<&'a str>,
    pub(super) effort: Option<&'a str>,
    /// The task's permission mode, for an agent that sets its own from it.
    pub(super) mode: AgentPermissionMode,
}

/// A thread the agent opened or reopened.
pub(super) struct OpenedThread {
    pub(super) thread_id: String,
    /// Whether the agent has confirmed the thread. One that has not reports
    /// [`AgentEvent::ThreadStarted`] once it does; until then a failed
    /// launch starts a fresh thread on retry.
    pub(super) confirmed: bool,
}

/// An async question's questions, with the ids `q0`, `q1`, ... that the
/// caller answers them by.
pub(super) fn async_question_prompts(questions: &[AsyncQuestion]) -> Vec<AgentQuestion> {
    questions
        .iter()
        .enumerate()
        .map(|(index, question)| AgentQuestion {
            multi_select: false,
            id: format!("q{index}"),
            header: format!("Question {}", index + 1),
            question: question.title.clone(),
            options: question
                .options
                .iter()
                .map(|label| AgentQuestionOption {
                    label: label.clone(),
                    description: String::new(),
                })
                .collect(),
            permission: None,
        })
        .collect()
}

/// The answers JSON of an [`AgentReply::Answers`], or no answers when it is
/// not one.
pub(super) fn card_answers(answer: &str) -> Value {
    if let Ok(value) = serde_json::from_str::<Value>(answer)
        && value.get("answers").is_some()
    {
        return value;
    }
    json!({ "answers": {} })
}

/// A command line sent as one string or as its words.
pub(super) fn command_line(command: Option<&Value>) -> String {
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

/// One to-do entry, under any of the names agents give its text and state.
pub(super) fn parse_todo(todo: &Value) -> Option<TodoEntry> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_answers_pass_through_and_default_to_none() {
        assert_eq!(
            card_answers(r#"{"answers":{"q1":{"answers":["A"]}}}"#)["answers"]["q1"]["answers"][0],
            "A"
        );
        assert_eq!(card_answers("")["answers"], json!({}));
        assert_eq!(card_answers(r#"{"other":1}"#)["answers"], json!({}));
    }

    #[test]
    fn todos_read_every_spelling() {
        assert_eq!(
            parse_todo(
                &json!({"content": "Ship it", "status": "completed", "activeForm": "Shipping"})
            ),
            Some(TodoEntry {
                text: "Ship it".into(),
                completed: true
            })
        );
        assert_eq!(
            parse_todo(&json!({"step": "Test", "status": "in_progress"})),
            Some(TodoEntry {
                text: "Test".into(),
                completed: false
            })
        );
        assert_eq!(parse_todo(&json!({"status": "done"})), None);
    }
}
