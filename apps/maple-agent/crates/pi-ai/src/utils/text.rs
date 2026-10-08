//! Port of `packages/ai/src/utils/text.ts`.
use super::{js_json::ordered_js_keys, js_value::JsString};
use crate::types::{
    AssistantContent, SystemContent, SystemMessage, TextContent, UserContent, UserMessageContent,
};

pub trait ContentText {
    fn content_text(&self, separator: &str) -> JsString;
}
impl ContentText for str {
    fn content_text(&self, _: &str) -> JsString {
        self.into()
    }
}
impl ContentText for String {
    fn content_text(&self, separator: &str) -> JsString {
        self.as_str().content_text(separator)
    }
}
impl ContentText for JsString {
    fn content_text(&self, _: &str) -> JsString {
        self.clone()
    }
}
impl ContentText for [AssistantContent] {
    fn content_text(&self, separator: &str) -> JsString {
        JsString::join(
            self.iter().filter_map(|block| match block {
                AssistantContent::Text(block) => Some(&block.text),
                _ => None,
            }),
            separator,
        )
    }
}
impl ContentText for [UserContent] {
    fn content_text(&self, separator: &str) -> JsString {
        JsString::join(
            self.iter().filter_map(|block| match block {
                UserContent::Text(block) => Some(&block.text),
                _ => None,
            }),
            separator,
        )
    }
}
impl ContentText for [TextContent] {
    fn content_text(&self, separator: &str) -> JsString {
        JsString::join(self.iter().map(|block| &block.text), separator)
    }
}
impl<T> ContentText for Vec<T>
where
    [T]: ContentText,
{
    fn content_text(&self, separator: &str) -> JsString {
        self.as_slice().content_text(separator)
    }
}
impl ContentText for SystemContent {
    fn content_text(&self, separator: &str) -> JsString {
        match self {
            Self::Text(text) => text.clone(),
            Self::Blocks(blocks) => blocks.content_text(separator),
        }
    }
}
impl ContentText for UserMessageContent {
    fn content_text(&self, separator: &str) -> JsString {
        match self {
            Self::Text(text) => text.clone(),
            Self::Blocks(blocks) => blocks.content_text(separator),
        }
    }
}

/// Extract and join text from message content. Pi's default separator is `"\n"`.
pub fn content_text<T: ContentText + ?Sized>(content: &T, separator: &str) -> JsString {
    content.content_text(separator)
}

pub fn get_system_message_text(message: &SystemMessage) -> JsString {
    let mut parts = vec![content_text(&message.content, "\n")];
    if let Some(sections) = &message.sections {
        for name in ordered_js_keys(sections.keys()) {
            if let Some(text) = &sections[name] {
                parts.push(text.clone());
            }
        }
    }
    JsString::join(parts.iter().filter(|part| !part.is_empty()), "\n\n")
}

pub fn render_system_message_update(message: &SystemMessage) -> JsString {
    let mut parts = Vec::new();
    let text = content_text(&message.content, "\n");
    if !text.is_empty() {
        parts.push(text);
    }
    if let Some(sections) = &message.sections {
        for name in ordered_js_keys(sections.keys()) {
            parts.push(match &sections[name] {
                None => {
                    let mut text = JsString::from("Removed system prompt section \"");
                    text.push(name);
                    text.push_str("\".");
                    text
                }
                Some(value) => {
                    let mut text = JsString::from("Updated system prompt section \"");
                    text.push(name);
                    text.push_str("\":\n\n");
                    text.push(value);
                    text
                }
            });
        }
    }
    JsString::join(parts.iter(), "\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_and_system_sections_preserve_raw_code_units() {
        let raw = JsString::from_utf16(vec![0xd800]);
        let blocks = vec![TextContent::new(&raw), TextContent::new("tail")];
        assert_eq!(
            content_text(&blocks, "\n").as_utf16(),
            vec![0xd800, 0x0a, 0x74, 0x61, 0x69, 0x6c]
        );
        let message = SystemMessage {
            content: SystemContent::Text(raw.clone()),
            sections: Some(indexmap::IndexMap::from([(
                "scope".into(),
                Some(raw.clone()),
            )])),
            ..Default::default()
        };
        assert_eq!(
            get_system_message_text(&message).as_utf16(),
            vec![0xd800, 10, 10, 0xd800]
        );
        let mut expected = raw;
        expected.push_str("\n\nUpdated system prompt section \"scope\":\n\n");
        expected.push(&JsString::from_utf16(vec![0xd800]));
        assert_eq!(render_system_message_update(&message), expected);

        let message = SystemMessage {
            sections: Some(indexmap::IndexMap::from([(
                JsString::from_utf16(vec![0xd800]),
                Some("x".into()),
            )])),
            ..Default::default()
        };
        assert_eq!(
            super::super::js_json::quote(&render_system_message_update(&message)),
            r#""Updated system prompt section \"\ud800\":\n\nx""#
        );
    }
}
