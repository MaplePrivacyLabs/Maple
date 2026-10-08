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
            Self::Custom(_) => "custom",
            Self::CompactionSummary(_) => "compactionSummary",
            Self::BranchSummary(_) => "branchSummary",
        }
    }

    pub fn timestamp(&self) -> Timestamp {
        match self {
            Self::Llm(message) => message.timestamp(),
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

/// The messages a model sees for a session transcript.
pub fn convert_to_llm(messages: &[SessionMessage]) -> Vec<Message> {
    messages.iter().map(SessionMessage::to_llm).collect()
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
}
