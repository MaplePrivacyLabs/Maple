use crate::transcript::system_message_text;
use crate::types::{AssistantContent, Content, Message, Tool};

/// Characters per token for the conservative `chars / 4` estimate.
const CHARS_PER_TOKEN: usize = 4;
/// What an image is assumed to cost, in characters.
const IMAGE_CHARS: usize = 4_800;

pub fn estimate_text_tokens(text: &str) -> u64 {
    text.chars().count().div_ceil(CHARS_PER_TOKEN) as u64
}

fn content_chars(content: &[Content]) -> usize {
    content
        .iter()
        .map(|block| match block {
            Content::Text(text) => text.text.chars().count(),
            Content::Image(_) => IMAGE_CHARS,
        })
        .sum()
}

pub fn estimate_tool_tokens(tools: &[Tool]) -> u64 {
    tools
        .iter()
        .map(|tool| {
            let chars =
                tool.name.len() + tool.description.len() + tool.parameters.to_string().len();
            chars.div_ceil(CHARS_PER_TOKEN) as u64
        })
        .sum()
}

/// A conservative token estimate for one message: it overestimates rather than under.
pub fn estimate_message_tokens(message: &Message) -> u64 {
    let chars = match message {
        Message::System(system) => {
            return estimate_text_tokens(&system_message_text(system))
                + estimate_tool_tokens(&system.tools_added)
                + system.tools_removed.len() as u64;
        }
        Message::User(user) => content_chars(&user.content),
        Message::ToolResult(result) => content_chars(&result.content),
        Message::Assistant(assistant) => assistant
            .content
            .iter()
            .map(|block| match block {
                AssistantContent::Text(text) => text.text.chars().count(),
                AssistantContent::Thinking(thinking) => thinking.thinking.chars().count(),
                AssistantContent::ToolCall(call) => {
                    call.name.len()
                        + serde_json::to_string(&call.arguments)
                            .unwrap_or_default()
                            .len()
                }
            })
            .sum(),
    };
    chars.div_ceil(CHARS_PER_TOKEN) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ToolResultMessage, UserMessage};

    #[test]
    fn text_is_four_characters_per_token_rounded_up() {
        assert_eq!(estimate_text_tokens(""), 0);
        assert_eq!(estimate_text_tokens("abcde"), 2);
        assert_eq!(
            estimate_message_tokens(&Message::User(UserMessage::text("abcdefgh"))),
            2
        );
    }

    #[test]
    fn images_count_as_a_fixed_size() {
        let message = Message::ToolResult(ToolResultMessage {
            tool_call_id: "c".into(),
            tool_name: "t".into(),
            content: vec![Content::image("AAAA", "image/png")],
            details: None,
            usage: None,
            is_error: false,
            timestamp: 0,
        });
        assert_eq!(estimate_message_tokens(&message), 1_200);
    }
}
