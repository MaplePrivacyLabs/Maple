use pi_agent_core::AgentMessage;
use pi_ai::{Content, Message, Timestamp, UserMessage, content_text};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";
pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

/// A message an extension adds to the conversation. The model sees it as user content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomMessage {
    pub custom_type: String,
    pub content: Vec<Content>,
    /// Whether interfaces show it.
    pub display: bool,
    /// Extension data that is never sent to the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    pub timestamp: Timestamp,
}

/// A shell command the user ran (`!command`) and its output. The model sees it as user
/// content, unless it was run with `!!`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashExecutionMessage {
    pub command: String,
    pub output: String,
    /// `None` when the command was cancelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub cancelled: bool,
    pub truncated: bool,
    /// Where the whole output is, when it was cut.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
    pub timestamp: Timestamp,
    /// Kept out of the model's context (`!!command`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exclude_from_context: bool,
}

/// A user shell command as the model reads it.
pub fn bash_execution_to_text(message: &BashExecutionMessage) -> String {
    let mut text = format!("Ran `{}`\n", message.command);
    if message.output.is_empty() {
        text.push_str("(no output)");
    } else {
        text.push_str(&format!("```\n{}\n```", message.output));
    }
    if message.cancelled {
        text.push_str("\n\n(command cancelled)");
    } else if let Some(code) = message.exit_code.filter(|code| *code != 0) {
        text.push_str(&format!("\n\nCommand exited with code {code}"));
    }
    if message.truncated
        && let Some(path) = &message.full_output_path
    {
        text.push_str(&format!("\n\n[Output truncated. Full output: {path}]"));
    }
    text
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSummaryMessage {
    pub summary: String,
    pub tokens_before: u64,
    pub timestamp: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryMessage {
    pub summary: String,
    pub from_id: String,
    pub timestamp: Timestamp,
}

/// A transcript entry of a session: the model-facing messages plus the session's own.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum SessionMessage {
    BashExecution(BashExecutionMessage),
    Custom(CustomMessage),
    CompactionSummary(CompactionSummaryMessage),
    BranchSummary(BranchSummaryMessage),
    #[serde(untagged)]
    Llm(Message),
}

impl AgentMessage for SessionMessage {
    fn from_message(message: Message) -> Self {
        Self::Llm(message)
    }

    fn as_message(&self) -> Option<&Message> {
        match self {
            Self::Llm(message) => Some(message),
            _ => None,
        }
    }
}

impl From<Message> for SessionMessage {
    fn from(message: Message) -> Self {
        Self::Llm(message)
    }
}

impl SessionMessage {
    pub fn role(&self) -> &'static str {
        match self {
            Self::Llm(message) => message.role(),
            Self::BashExecution(_) => "bashExecution",
            Self::Custom(_) => "custom",
            Self::CompactionSummary(_) => "compactionSummary",
            Self::BranchSummary(_) => "branchSummary",
        }
    }

    pub fn timestamp(&self) -> Timestamp {
        match self {
            Self::Llm(message) => message.timestamp(),
            Self::BashExecution(message) => message.timestamp,
            Self::Custom(message) => message.timestamp,
            Self::CompactionSummary(message) => message.timestamp,
            Self::BranchSummary(message) => message.timestamp,
        }
    }

    /// The text a person would read, for queues and listings.
    pub fn text(&self) -> String {
        match self {
            Self::Llm(Message::User(user)) => content_text(&user.content),
            Self::Llm(Message::Assistant(assistant)) => assistant.text(),
            Self::Llm(Message::ToolResult(result)) => content_text(&result.content),
            Self::Llm(Message::System(system)) => system.content.clone(),
            Self::BashExecution(bash) => bash_execution_to_text(bash),
            Self::Custom(custom) => content_text(&custom.content),
            Self::CompactionSummary(summary) => summary.summary.clone(),
            Self::BranchSummary(summary) => summary.summary.clone(),
        }
    }

    /// The message as the model sees it.
    pub fn to_llm(&self) -> Message {
        let user =
            |content: Vec<Content>, timestamp| Message::User(UserMessage { content, timestamp });
        match self {
            Self::Llm(message) => message.clone(),
            Self::BashExecution(bash) => user(
                vec![Content::text(bash_execution_to_text(bash))],
                bash.timestamp,
            ),
            Self::Custom(custom) => user(custom.content.clone(), custom.timestamp),
            Self::CompactionSummary(summary) => user(
                vec![Content::text(format!(
                    "{COMPACTION_SUMMARY_PREFIX}{}{COMPACTION_SUMMARY_SUFFIX}",
                    summary.summary
                ))],
                summary.timestamp,
            ),
            Self::BranchSummary(summary) => user(
                vec![Content::text(format!(
                    "{BRANCH_SUMMARY_PREFIX}{}{BRANCH_SUMMARY_SUFFIX}",
                    summary.summary
                ))],
                summary.timestamp,
            ),
        }
    }
}

impl SessionMessage {
    /// Whether the model never sees it: a shell command run with `!!`.
    pub fn excluded_from_context(&self) -> bool {
        matches!(self, Self::BashExecution(bash) if bash.exclude_from_context)
    }
}

/// The messages a model sees for a session transcript.
pub fn convert_to_llm(messages: &[SessionMessage]) -> Vec<Message> {
    messages
        .iter()
        .filter(|message| !message.excluded_from_context())
        .map(SessionMessage::to_llm)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn model_messages_and_session_messages_share_the_role_tag() {
        let user: SessionMessage =
            serde_json::from_value(json!({ "role": "user", "content": "hi", "timestamp": 1 }))
                .unwrap();
        assert!(matches!(user, SessionMessage::Llm(Message::User(_))));

        let custom = SessionMessage::Custom(CustomMessage {
            custom_type: "note".into(),
            content: vec![Content::text("remember")],
            display: true,
            details: None,
            timestamp: 2,
        });
        let value = serde_json::to_value(&custom).unwrap();
        assert_eq!(value["role"], "custom");
        assert_eq!(
            serde_json::from_value::<SessionMessage>(value).unwrap(),
            custom
        );
    }

    #[test]
    fn summaries_reach_the_model_as_framed_user_text() {
        let summary = SessionMessage::CompactionSummary(CompactionSummaryMessage {
            summary: "did things".into(),
            tokens_before: 10,
            timestamp: 3,
        });
        let Message::User(user) = summary.to_llm() else {
            panic!()
        };
        let text = content_text(&user.content);
        assert!(text.starts_with(COMPACTION_SUMMARY_PREFIX));
        assert!(text.contains("did things"));
    }

    #[test]
    fn user_shell_commands_read_as_pis_and_bang_bang_ones_stay_out() {
        let ran = BashExecutionMessage {
            command: "make".into(),
            output: "error: x".into(),
            exit_code: Some(2),
            cancelled: false,
            truncated: true,
            full_output_path: Some("/tmp/out.log".into()),
            timestamp: 5,
            exclude_from_context: false,
        };
        assert_eq!(
            bash_execution_to_text(&ran),
            "Ran `make`\n```\nerror: x\n```\n\nCommand exited with code 2\n\n[Output truncated. Full output: /tmp/out.log]"
        );
        let quiet = BashExecutionMessage {
            output: String::new(),
            exit_code: None,
            cancelled: true,
            truncated: false,
            ..ran.clone()
        };
        assert_eq!(
            bash_execution_to_text(&quiet),
            "Ran `make`\n(no output)\n\n(command cancelled)"
        );

        let message = SessionMessage::BashExecution(ran.clone());
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value["role"], "bashExecution");
        assert_eq!(value["exitCode"], 2);
        assert!(value.get("excludeFromContext").is_none());
        assert_eq!(
            serde_json::from_value::<SessionMessage>(value).unwrap(),
            message
        );

        let hidden = SessionMessage::BashExecution(BashExecutionMessage {
            exclude_from_context: true,
            ..ran
        });
        let llm = convert_to_llm(&[message, hidden]);
        assert_eq!(llm.len(), 1);
    }
}
