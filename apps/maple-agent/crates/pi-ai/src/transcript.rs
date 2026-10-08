//! Replaying system messages into the current prompt and tool set.
//!
//! The transcript is the only source of prompt and tool state: the leading system
//! message declares them and later system messages change them. Providers that accept
//! system messages mid-conversation send each in place; the rest send
//! [`collapse_system_messages`], which folds them into one leading message.

use indexmap::IndexMap;

use crate::types::{Context, Message, SystemMessage, Tool, ToolReference};

/// The leading system message for a prompt and tool set, or `None` when both are empty.
pub fn initial_system_message(system_prompt: &str, tools: Vec<Tool>) -> Option<SystemMessage> {
    if system_prompt.is_empty() && tools.is_empty() {
        return None;
    }
    Some(SystemMessage {
        content: system_prompt.to_string(),
        tools_added: tools,
        ..SystemMessage::default()
    })
}

fn system_messages<'a>(
    messages: impl IntoIterator<Item = &'a Message>,
) -> impl Iterator<Item = &'a SystemMessage> {
    messages.into_iter().filter_map(|message| match message {
        Message::System(system) => Some(system),
        _ => None,
    })
}

/// The tools available after applying every declaration in order.
pub fn current_tools<'a>(messages: impl IntoIterator<Item = &'a Message>) -> Vec<Tool> {
    let mut tools: IndexMap<String, Tool> = IndexMap::new();
    for system in system_messages(messages) {
        for removed in &system.tools_removed {
            tools.shift_remove(&removed.name);
        }
        for added in &system.tools_added {
            tools.insert(added.name.clone(), added.clone());
        }
    }
    tools.into_values().collect()
}

/// Every system message replayed into one: later `content` is appended to the base prompt,
/// `sections` are patched by name and the tools are the current set.
pub fn current_system_message<'a>(
    messages: impl IntoIterator<Item = &'a Message> + Clone,
) -> Option<SystemMessage> {
    let mut content = Vec::new();
    let mut sections: IndexMap<String, Option<String>> = IndexMap::new();
    let mut timestamp = None;
    for system in system_messages(messages.clone()) {
        timestamp.get_or_insert(system.timestamp);
        if !system.content.is_empty() {
            content.push(system.content.clone());
        }
        for (name, value) in &system.sections {
            match value {
                Some(text) => {
                    sections.insert(name.clone(), Some(text.clone()));
                }
                None => {
                    sections.shift_remove(name);
                }
            }
        }
    }
    let tools = current_tools(messages);
    if timestamp.is_none() && tools.is_empty() {
        return None;
    }
    Some(SystemMessage {
        content: content.join("\n\n"),
        sections,
        tools_added: tools,
        tools_removed: Vec::new(),
        timestamp: timestamp.unwrap_or_default(),
    })
}

/// A system message rendered as one prompt: its content followed by its sections.
pub fn system_message_text(message: &SystemMessage) -> String {
    let mut parts = Vec::new();
    if !message.content.is_empty() {
        parts.push(message.content.as_str());
    }
    for text in message.sections.values().flatten() {
        if !text.is_empty() {
            parts.push(text.as_str());
        }
    }
    parts.join("\n\n")
}

/// The current system prompt text after replaying every system message.
pub fn current_system_prompt<'a>(
    messages: impl IntoIterator<Item = &'a Message> + Clone,
) -> String {
    current_system_message(messages)
        .map(|message| system_message_text(&message))
        .unwrap_or_default()
}

/// A later system message rendered for APIs that accept system messages mid-conversation.
/// Section changes are framed by name so the model can relate them to the leading prompt.
pub fn render_system_message_update(message: &SystemMessage) -> String {
    let mut parts = Vec::new();
    if !message.content.is_empty() {
        parts.push(message.content.clone());
    }
    for (name, value) in &message.sections {
        parts.push(match value {
            Some(text) => format!("Updated system prompt section \"{name}\":\n\n{text}"),
            None => format!("Removed system prompt section \"{name}\"."),
        });
    }
    parts.join("\n\n")
}

/// The transcript for APIs without mid-conversation system messages: the replayed system
/// message leads and every later system message is dropped.
pub fn collapse_system_messages(context: &Context) -> Context {
    let head = current_system_message(&context.messages);
    let mut messages = Vec::with_capacity(context.messages.len());
    messages.extend(head.map(Message::System));
    messages.extend(
        context
            .messages
            .iter()
            .filter(|message| !matches!(message, Message::System(_)))
            .cloned(),
    );
    Context { messages }
}

/// Tool declarations that changed between two complete tool sets. A changed definition
/// is a removal followed by an addition.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolChanges {
    pub added: Vec<Tool>,
    pub removed: Vec<ToolReference>,
}

impl ToolChanges {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

pub fn tool_changes(previous: &[Tool], current: &[Tool]) -> ToolChanges {
    let find = |tools: &[Tool], name: &str| tools.iter().position(|tool| tool.name == name);
    ToolChanges {
        added: current
            .iter()
            .filter(|tool| match find(previous, &tool.name) {
                Some(index) => previous[index] != **tool,
                None => true,
            })
            .cloned()
            .collect(),
        removed: previous
            .iter()
            .filter(|tool| match find(current, &tool.name) {
                Some(index) => current[index] != **tool,
                None => true,
            })
            .map(|tool| ToolReference {
                name: tool.name.clone(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::UserMessage;
    use serde_json::json;

    fn tool(name: &str, description: &str) -> Tool {
        Tool::new(name, description, json!({ "type": "object" }))
    }

    fn system(content: &str) -> SystemMessage {
        SystemMessage {
            content: content.into(),
            ..SystemMessage::default()
        }
    }

    #[test]
    fn replay_applies_additions_removals_and_sections_in_order() {
        let mut base = system("base");
        base.tools_added = vec![tool("read", "r"), tool("bash", "b")];
        base.sections
            .insert("rules".into(), Some("<rules>a</rules>".into()));
        base.sections
            .insert("cwd".into(), Some("<cwd>/</cwd>".into()));
        let mut update = system("more");
        update.tools_removed = vec![ToolReference {
            name: "bash".into(),
        }];
        update.tools_added = vec![tool("edit", "e")];
        update
            .sections
            .insert("rules".into(), Some("<rules>b</rules>".into()));
        update.sections.insert("cwd".into(), None);
        let messages = vec![
            Message::System(base),
            Message::User(UserMessage::text("hi")),
            Message::System(update),
        ];

        let names: Vec<_> = current_tools(&messages)
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        assert_eq!(names, ["read", "edit"]);
        assert_eq!(
            current_system_prompt(&messages),
            "base\n\nmore\n\n<rules>b</rules>"
        );
    }

    #[test]
    fn collapsing_keeps_one_leading_system_message() {
        let messages = vec![
            Message::System(system("a")),
            Message::User(UserMessage::text("hi")),
            Message::System(system("b")),
        ];
        let collapsed = collapse_system_messages(&Context::from_messages(messages));
        assert_eq!(collapsed.messages.len(), 2);
        let Message::System(head) = &collapsed.messages[0] else {
            panic!("expected a leading system message")
        };
        assert_eq!(head.content, "a\n\nb");
    }

    #[test]
    fn changed_definitions_are_removed_and_added_again() {
        let previous = vec![tool("read", "old"), tool("bash", "b")];
        let current = vec![tool("read", "new"), tool("edit", "e")];
        let changes = tool_changes(&previous, &current);
        let added: Vec<_> = changes
            .added
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        let removed: Vec<_> = changes
            .removed
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        assert_eq!(added, ["read", "edit"]);
        assert_eq!(removed, ["read", "bash"]);
        assert!(tool_changes(&current, &current).is_empty());
    }

    #[test]
    fn section_updates_are_framed_by_name() {
        let mut update = system("");
        update
            .sections
            .insert("skills".into(), Some("<skills/>".into()));
        update.sections.insert("cwd".into(), None);
        assert_eq!(
            render_system_message_update(&update),
            "Updated system prompt section \"skills\":\n\n<skills/>\n\nRemoved system prompt section \"cwd\"."
        );
    }
}
