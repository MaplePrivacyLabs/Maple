//! System-message and tool-state replay from `utils/transcript.ts`.

use indexmap::{IndexMap, IndexSet};

use super::raw_message;
use crate::types::{
    Context, JsObject, JsString, JsValue, Message, Schema, SystemContent, SystemMessage, Tool,
    ToolReference, TranscriptContext,
};
use crate::utils::js_json::{ordered_js_keys, stringify};
use crate::utils::js_value::{from_js_value, to_js_value};
use crate::utils::text::{content_text, get_system_message_text};

pub fn create_initial_system_message(
    system_prompt: Option<&JsString>,
    tools: Option<&[Tool]>,
) -> Option<SystemMessage> {
    let has_system_prompt = system_prompt.is_some_and(|prompt| !prompt.is_empty());
    let has_tools = tools.is_some_and(|tools| !tools.is_empty());
    if !has_system_prompt && !has_tools {
        return None;
    }
    Some(SystemMessage {
        content: SystemContent::Text(system_prompt.cloned().unwrap_or_default()),
        tools_added: has_tools.then(|| tools.expect("nonempty tools").to_vec()),
        timestamp: 0.0,
        ..SystemMessage::default()
    })
}

pub fn normalize_context(context: Context) -> TranscriptContext {
    let initial =
        create_initial_system_message(context.system_prompt.as_ref(), context.tools.as_deref());
    let mut messages = context.messages;
    if let Some(initial) = initial {
        messages.insert(0, initial.into());
    }
    TranscriptContext::new(messages)
}

pub fn get_initial_system_message(messages: &[Message]) -> Option<&Message> {
    messages
        .first()
        .filter(|message| message.role() == "system")
}

pub fn without_initial_system_message(messages: &[Message]) -> &[Message] {
    if get_initial_system_message(messages).is_some() {
        &messages[1..]
    } else {
        messages
    }
}

fn field<T: serde::de::DeserializeOwned>(
    message: &Message,
    name: &str,
) -> Result<Option<T>, JsString> {
    let Message::Raw(message) = message else {
        unreachable!("raw field accessor")
    };
    message.read(|object| match object.get(name) {
        None | Some(JsValue::Null) => Ok(None),
        Some(value) => from_js_value(value.clone())
            .map(Some)
            .map_err(|error| error.to_string().into()),
    })
}

pub fn system_tools_added(message: &Message) -> Result<Vec<Tool>, JsString> {
    match message {
        Message::System(message) => Ok(message.tools_added.clone().unwrap_or_default()),
        Message::Raw(_) if message.role() == "system" => {
            Ok(field(message, "toolsAdded")?.unwrap_or_default())
        }
        _ => Ok(Vec::new()),
    }
}

pub fn system_tools_removed(message: &Message) -> Result<Vec<ToolReference>, JsString> {
    match message {
        Message::System(message) => Ok(message.tools_removed.clone().unwrap_or_default()),
        Message::Raw(_) if message.role() == "system" => {
            Ok(field(message, "toolsRemoved")?.unwrap_or_default())
        }
        _ => Ok(Vec::new()),
    }
}

pub fn get_current_tools(messages: &[Message]) -> Result<Vec<Tool>, JsString> {
    let mut tools = IndexMap::new();
    for message in messages {
        for tool in system_tools_removed(message)? {
            tools.shift_remove(&tool.name);
        }
        for tool in system_tools_added(message)? {
            tools.insert(tool.name.clone(), tool);
        }
    }
    Ok(tools.into_values().collect())
}

pub fn get_current_system_message(messages: &[Message]) -> Result<Option<Message>, JsString> {
    let mut content = Vec::new();
    let mut sections = IndexMap::new();
    let mut timestamp: Option<JsValue> = None;
    for message in messages {
        if message.role() != "system" {
            continue;
        }
        let (time, text, patch) = match message {
            Message::System(message) => (
                Some(JsValue::Number(message.timestamp)),
                content_text(&message.content, "\n"),
                message
                    .sections
                    .as_ref()
                    .map(|patch| {
                        ordered_js_keys(patch.keys())
                            .into_iter()
                            .map(|name| {
                                (
                                    name.clone(),
                                    patch[name].clone().map_or(JsValue::Null, JsValue::String),
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            ),
            Message::Raw(raw) => raw.read(|object| -> Result<_, JsString> {
                Ok((
                    object.get("timestamp").cloned(),
                    raw_message::content_text(object.get("content"), "\n")?,
                    raw_message::entries(object.get("sections")),
                ))
            })?,
            _ => unreachable!("system role"),
        };
        if timestamp.as_ref().is_none_or(JsValue::is_null) {
            timestamp = time;
        }
        if !text.is_empty() {
            content.push(text);
        }
        for (name, value) in patch {
            if value.is_null() {
                sections.shift_remove(&name);
            } else {
                sections.insert(name, value);
            }
        }
    }
    let tools = get_current_tools(messages)?;
    if timestamp.is_none() && tools.is_empty() {
        return Ok(None);
    }
    let mut output = JsObject::new();
    output.insert("role", "system".into());
    output.insert("content", join_text(content, "\n\n").into());
    if !sections.is_empty() {
        output.insert("sections", JsValue::Object(sections.into_iter().collect()));
    }
    if !tools.is_empty() {
        output.insert(
            "toolsAdded",
            to_js_value(&tools).map_err(|error| JsString::from(error.to_string()))?,
        );
    }
    output.insert(
        "timestamp",
        timestamp
            .filter(|value| !value.is_null())
            .unwrap_or(JsValue::Number(0.0)),
    );
    let mut output: Message = from_js_value(JsValue::Object(output))
        .map_err(|error| JsString::from(error.to_string()))?;
    // The owned declaration boundary still retains runtime schema metadata.
    if let Message::System(message) = &mut output {
        message.tools_added = (!tools.is_empty()).then_some(tools);
    }
    Ok(Some(output))
}

pub fn get_message_system_text(message: &Message) -> Result<JsString, JsString> {
    match message {
        Message::System(message) => Ok(get_system_message_text(message)),
        Message::Raw(message) => message.read(raw_message::system_text),
        _ => Err("Expected a system message".into()),
    }
}

pub fn render_message_system_update(message: &Message) -> Result<JsString, JsString> {
    match message {
        Message::System(message) => Ok(crate::utils::text::render_system_message_update(message)),
        Message::Raw(message) => message.read(|object| {
            let content = raw_message::content_text(object.get("content"), "\n")?;
            let mut parts = Vec::new();
            if !content.is_empty() {
                parts.push(content);
            }
            for (name, value) in raw_message::entries(object.get("sections")) {
                let mut text = JsString::from(if value.is_null() {
                    "Removed system prompt section \""
                } else {
                    "Updated system prompt section \""
                });
                text.push(&name);
                if value.is_null() {
                    text.push_str("\".");
                } else {
                    text.push_str("\":\n\n");
                    text.push(&raw_message::string(Some(&value)));
                }
                parts.push(text);
            }
            Ok(JsString::join(parts.iter(), "\n\n"))
        }),
        _ => Err("Expected a system message".into()),
    }
}

pub fn get_current_system_prompt(messages: &[Message]) -> Result<JsString, JsString> {
    get_current_system_message(messages)?
        .as_ref()
        .map(get_message_system_text)
        .transpose()
        .map(Option::unwrap_or_default)
}

pub fn collapse_system_messages(context: TranscriptContext) -> Result<TranscriptContext, JsString> {
    let head = get_current_system_message(&context.messages)?;
    let mut messages: Vec<_> = context
        .messages
        .into_iter()
        .filter(|message| message.role() != "system")
        .collect();
    if let Some(head) = head {
        messages.insert(0, head);
    }
    Ok(TranscriptContext::new(messages))
}

pub fn resolve_transcript(
    context: TranscriptContext,
    supports_mid_convo_system_messages: Option<bool>,
) -> Result<TranscriptContext, JsString> {
    if supports_mid_convo_system_messages.unwrap_or(false) {
        Ok(context)
    } else {
        collapse_system_messages(context)
    }
}

pub fn to_tool_declaration(tool: &Tool) -> Tool {
    Tool {
        name: tool.name.clone(),
        description: tool.description.clone(),
        // This is the TypeScript JSON roundtrip: runtime TypeBox identity is
        // omitted alongside executable/display-only fields.
        parameters: Schema::json_schema(
            crate::utils::json_parse::parse_json(&stringify(&tool.parameters.schema))
                .expect("stringified tool schema parses"),
        ),
        constrained_sampling: tool.constrained_sampling.clone(),
    }
}

pub fn declarations_equal(left: &Tool, right: &Tool) -> bool {
    // Compare serialized declarations, not Value equality: declaration property
    // order is observable in Pi's JSON.stringify comparison.
    stringify(&to_js_value(&to_tool_declaration(left)).expect("tool declaration serializes"))
        == stringify(
            &to_js_value(&to_tool_declaration(right)).expect("tool declaration serializes"),
        )
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolStateChanges {
    pub tools_added: Vec<Tool>,
    pub tools_removed: Vec<ToolReference>,
}

pub fn get_tool_state_changes(previous: &[Tool], current: &[Tool]) -> ToolStateChanges {
    let previous_tools: IndexMap<_, _> = previous
        .iter()
        .map(|tool| (tool.name.clone(), tool))
        .collect();
    let current_tools: IndexMap<_, _> = current
        .iter()
        .map(|tool| (tool.name.clone(), tool))
        .collect();
    ToolStateChanges {
        tools_added: current
            .iter()
            .filter(|tool| {
                previous_tools
                    .get(&tool.name)
                    .is_none_or(|previous| !declarations_equal(previous, tool))
            })
            .map(to_tool_declaration)
            .collect(),
        tools_removed: previous
            .iter()
            .filter(|tool| {
                current_tools
                    .get(&tool.name)
                    .is_none_or(|current| !declarations_equal(tool, current))
            })
            .map(|tool| ToolReference {
                name: tool.name.clone(),
            })
            .collect(),
    }
}

pub fn get_declared_tools(messages: &[Message]) -> Result<Vec<Tool>, JsString> {
    let mut definitions = IndexMap::new();
    for message in messages {
        for tool in system_tools_added(message)? {
            definitions.insert(tool.name.clone(), tool);
        }
    }
    Ok(definitions.into_values().collect())
}

pub fn has_tool_redefinitions(messages: &[Message]) -> Result<bool, JsString> {
    let mut declared = IndexMap::new();
    for message in messages {
        for tool in system_tools_added(message)? {
            if declared
                .get(&tool.name)
                .is_some_and(|previous| !declarations_equal(previous, &tool))
            {
                return Ok(true);
            }
            declared.insert(tool.name.clone(), tool);
        }
    }
    Ok(false)
}

pub fn has_non_additive_tool_changes(messages: &[Message]) -> Result<bool, JsString> {
    let mut declared = IndexSet::new();
    for message in messages {
        if !system_tools_removed(message)?.is_empty() {
            return Ok(true);
        }
        for tool in system_tools_added(message)? {
            if !declared.insert(tool.name) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptTools {
    pub request_tools: Vec<Tool>,
    pub anchors_additions: bool,
}

pub fn resolve_transcript_tools(
    messages: &[Message],
    supports_tool_additions: bool,
) -> Result<TranscriptTools, JsString> {
    let anchors_additions = supports_tool_additions && !has_non_additive_tool_changes(messages)?;
    Ok(TranscriptTools {
        request_tools: if anchors_additions {
            get_initial_system_message(messages)
                .map(system_tools_added)
                .transpose()?
                .unwrap_or_default()
        } else {
            get_current_tools(messages)?
        },
        anchors_additions,
    })
}

fn join_text(parts: Vec<JsString>, separator: &str) -> JsString {
    let mut output = JsString::default();
    for (index, part) in parts.into_iter().enumerate() {
        if index != 0 {
            output.push_str(separator);
        }
        output.push(&part);
    }
    output
}
