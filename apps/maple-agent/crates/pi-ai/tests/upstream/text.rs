//! Translated from `packages/ai/test/text.test.ts` at Pi v1.0.4.
use pi_ai::types::{
    AssistantContent, ImageContent, TextContent, ThinkingContent, ToolCall, ToolResultContent,
};
use pi_ai::utils::text::content_text;

fn content() -> Vec<AssistantContent> {
    vec![
        ThinkingContent::new("reasoning").into(),
        TextContent::new("first").into(),
        ToolCall::new("1", "read", pi_ai::utils::js_value::JsObject::new()).into(),
        TextContent::new("second").into(),
    ]
}

mod content_text {
    use super::*;

    #[test]
    fn extracts_assistant_text_blocks() {
        assert_eq!(content_text(&content(), "\n"), "first\nsecond");
    }

    #[test]
    fn supports_custom_separators() {
        assert_eq!(content_text(&content(), ""), "firstsecond");
    }

    #[test]
    fn passes_string_content_through() {
        assert_eq!(content_text("hello", "\n"), "hello");
    }

    #[test]
    fn extracts_text_from_tool_result_content() {
        let content: Vec<ToolResultContent> = vec![
            TextContent::new("first").into(),
            ImageContent::new("...", "image/png").into(),
            TextContent::new("second").into(),
        ];
        assert_eq!(content_text(&content, ""), "firstsecond");
    }
}
