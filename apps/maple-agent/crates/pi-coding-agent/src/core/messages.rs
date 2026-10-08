//! Coding-agent roles and their selected LLM conversion from `core/messages.ts`.
use crate::utils::dates::parse_timestamp;
use pi_agent_core::types::{AgentMessage, AgentMessageValue};
use pi_ai::types::{JsObject, JsString, JsValue, Message};
use pi_ai::utils::{js_value::to_js_value, raw_message::string as coerce_string};
pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";
pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";
pub type BashExecutionMessage = JsObject;
pub type CustomMessage = JsObject;
pub type BranchSummaryMessage = JsObject;
pub type CompactionSummaryMessage = JsObject;
pub fn raw_message(raw: JsObject) -> AgentMessage {
    AgentMessage::new(AgentMessageValue::Custom(raw.into()))
}
pub fn message_value(message: &AgentMessage) -> JsValue {
    to_js_value(message).expect("AgentMessage is serializable")
}
pub(crate) fn object<const N: usize>(fields: [(&str, JsValue); N]) -> JsValue {
    JsObject::from(fields).into()
}
pub(crate) fn string(value: Option<&JsValue>) -> JsString {
    value
        .and_then(JsValue::as_js_str)
        .cloned()
        .unwrap_or_default()
}
pub(crate) fn truthy(value: &JsValue) -> bool {
    match value {
        JsValue::Null => false,
        JsValue::Bool(b) => *b,
        JsValue::Number(n) => *n != 0.0 && !n.is_nan(),
        JsValue::String(s) => !s.is_empty(),
        _ => true,
    }
}
pub fn bash_execution_to_text(message: &JsObject) -> JsString {
    let mut text = JsString::from("Ran `");
    text.push(&coerce_string(message.get("command")));
    text.push_str("`\n");
    let output = coerce_string(message.get("output"));
    if message.get("output").is_some_and(truthy) {
        text.push_str("```\n");
        text.push(&output);
        text.push_str("\n```");
    } else {
        text.push_str("(no output)");
    }
    if message.get("cancelled").is_some_and(truthy) {
        text.push_str("\n\n(command cancelled)");
    } else if let Some(exit) = message
        .get("exitCode")
        .filter(|v| !v.is_null() && v.as_f64() != Some(0.0))
    {
        text.push_str("\n\nCommand exited with code ");
        text.push(&coerce_string(Some(exit)));
    }
    if message.get("truncated").is_some_and(truthy)
        && message.get("fullOutputPath").is_some_and(truthy)
    {
        text.push_str("\n\n[Output truncated. Full output: ");
        text.push(&coerce_string(message.get("fullOutputPath")));
        text.push_str("]");
    }
    text
}
pub fn create_branch_summary_message(
    summary: JsString,
    from_id: JsString,
    timestamp: &JsString,
) -> AgentMessage {
    raw_message(JsObject::from([
        ("role", "branchSummary".into()),
        ("summary", summary.into()),
        ("fromId", from_id.into()),
        ("timestamp", parse_timestamp(timestamp).into()),
    ]))
}
pub fn create_compaction_summary_message(
    summary: JsString,
    tokens_before: f64,
    timestamp: &JsString,
) -> AgentMessage {
    create_compaction_summary_message_raw_tokens(summary, Some(tokens_before.into()), timestamp)
}
pub fn create_compaction_summary_message_raw_tokens(
    summary: JsString,
    tokens_before: Option<JsValue>,
    timestamp: &JsString,
) -> AgentMessage {
    let mut value = JsObject::from([
        ("role", "compactionSummary".into()),
        ("summary", summary.into()),
    ]);
    if let Some(tokens_before) = tokens_before {
        value.insert("tokensBefore", tokens_before);
    }
    value.insert("timestamp", parse_timestamp(timestamp).into());
    raw_message(value)
}
pub fn create_custom_message(
    custom_type: JsString,
    content: JsValue,
    display: bool,
    details: Option<JsValue>,
    timestamp: &JsString,
) -> AgentMessage {
    let mut raw = JsObject::from([
        ("role", "custom".into()),
        ("customType", custom_type.into()),
        ("content", content),
        ("display", display.into()),
    ]);
    if let Some(details) = details {
        raw.insert("details", details);
    }
    raw.insert("timestamp", parse_timestamp(timestamp).into());
    raw_message(raw)
}
pub fn convert_to_llm(messages: &[AgentMessage]) -> Vec<Message> {
    messages
        .iter()
        .filter_map(|message| {
            if matches!(
                message.role().as_str(),
                Some("system" | "user" | "assistant" | "toolResult")
            ) {
                return message.as_llm();
            }
            convert_to_llm_raw(std::slice::from_ref(message))
                .into_iter()
                .next()
                .map(|value| {
                    Message::Raw(
                        value
                            .as_object()
                            .expect("projected message is an object")
                            .clone()
                            .into(),
                    )
                })
        })
        .collect()
}
/// Lossless companion for storage/corpus observations before typed provider input.
pub fn convert_to_llm_raw(messages: &[AgentMessage]) -> Vec<JsValue> {
    messages
        .iter()
        .filter_map(|message| {
            let value = message_value(message);
            let raw = value.as_object()?;
            let role = raw.get("role").and_then(JsValue::as_str)?;
            let content = match role {
                "bashExecution" => {
                    if raw.get("excludeFromContext").is_some_and(truthy) {
                        return None;
                    }
                    Some(
                        vec![object([
                            ("type", "text".into()),
                            ("text", bash_execution_to_text(raw).into()),
                        ])]
                        .into(),
                    )
                }
                "custom" => raw.get("content").cloned().map(|content| {
                    if content.is_string() {
                        vec![object([("type", "text".into()), ("text", content)])].into()
                    } else {
                        content
                    }
                }),
                "branchSummary" | "compactionSummary" => {
                    let mut text = JsString::from(if role == "branchSummary" {
                        BRANCH_SUMMARY_PREFIX
                    } else {
                        COMPACTION_SUMMARY_PREFIX
                    });
                    text.push(&coerce_string(raw.get("summary")));
                    text.push_str(if role == "branchSummary" {
                        BRANCH_SUMMARY_SUFFIX
                    } else {
                        COMPACTION_SUMMARY_SUFFIX
                    });
                    Some(vec![object([("type", "text".into()), ("text", text.into())])].into())
                }
                "system" | "user" | "assistant" | "toolResult" => return Some(value),
                _ => return None,
            };
            let mut user = JsObject::from([("role", "user".into())]);
            if let Some(content) = content {
                user.insert("content", content);
            }
            if let Some(timestamp) = raw.get("timestamp") {
                user.insert("timestamp", timestamp.clone());
            }
            Some(user.into())
        })
        .collect()
}
