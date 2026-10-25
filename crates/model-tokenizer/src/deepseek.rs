// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright contributors to the vLLM project
// SPDX-FileCopyrightText: Copyright 2026 Maple Privacy Labs

//! DeepSeek V4.1 Flash 1017's text-only prompt protocol.
//!
//! Port of the Python serving encoder and normalizer at vLLM
//! `db9527a46873454610df6dbedf79a36d6bf1a7f6`, with Tinfoil's released
//! reasoning-effort mapping (low=25, high=50). This intentionally follows that
//! Python serving path, rather than the separate Rust vLLM renderer, whose
//! continuation and tool-argument behavior differs at this revision.
//!
//! The returned string already includes BOS and the assistant transition.
//! Tokenize it with `add_special_tokens=false`. Literal special-marker text is
//! preserved exactly as in the reference; it must not be escaped or stripped.
//! Image placeholders are insufficient to count processor-expanded image
//! tokens, so multimodal requests are rejected rather than labeled exact.

use std::collections::HashMap;
use std::fmt::{self, Write as _};

use serde::de::{self, Deserialize, Deserializer, MapAccess, Visitor};
use serde_json::value::RawValue;
use serde_json::{Map, Number, Value};

use crate::RenderError;

const BOS: &str = "<｜begin▁of▁sentence｜>";
const EOS: &str = "<｜end▁of▁sentence｜>";
const USER: &str = "<｜User｜>";
const SYSTEM: &str = "<｜System｜>";
const ASSISTANT: &str = "<｜Assistant｜>";

const TOOLS_PREFIX: &str = r#"## Tools

You have access to a set of tools to help answer the user's question. You can invoke tools by writing a "<｜DSML｜ calls>" block like the following:

<｜DSML｜ calls>
<｜DSML｜ invoke name="$TOOL_NAME">
<｜DSML｜ parameter name="$PARAMETER_NAME" string="true|false">$PARAMETER_VALUE</｜DSML｜ parameter>
...
</｜DSML｜ invoke>
<｜DSML｜ invoke name="$TOOL_NAME2">
...
</｜DSML｜ invoke>
</｜DSML｜ calls>

String parameters should be specified as is and set `string="true"`. For all other types (numbers, booleans, arrays, objects), pass the value in JSON format and set `string="false"`.

If thinking_mode is enabled (triggered by <think>), you MUST output your complete reasoning inside <think>...</think> BEFORE any tool calls or final response.

Otherwise, output directly after </think> with tool calls or final response.

### Available Tool Schemas

"#;
const TOOLS_SUFFIX: &str = "\n\nYou MUST strictly follow the above defined tool name and parameter schemas to invoke tool calls.\n";

#[derive(Clone)]
struct Message {
    fields: Map<String, Value>,
    normalized_tools: bool,
}

impl std::ops::Deref for Message {
    type Target = Map<String, Value>;
    fn deref(&self) -> &Self::Target {
        &self.fields
    }
}

impl std::ops::DerefMut for Message {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.fields
    }
}

impl From<Map<String, Value>> for Message {
    fn from(fields: Map<String, Value>) -> Self {
        Self {
            fields,
            normalized_tools: false,
        }
    }
}

/// Render an OpenAI chat-completion request using the pinned serving defaults.
///
/// Request-wide tools are normalized through the same function-definition
/// defaults as vLLM. Top-level `reasoning_effort` overrides template kwargs when
/// non-null. Top-level `response_format` and `tool_choice` do not change this
/// encoder's prompt at the pinned default server configuration.
pub fn render_chat(request: &Value) -> Result<String, RenderError> {
    let request = object(request, "request must be an object")?;
    let input_messages = request
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("messages must be an array"))?;
    if input_messages.is_empty() {
        return Err(invalid("messages must not be empty"));
    }
    let mut messages = input_messages
        .iter()
        .map(normalize_message)
        .collect::<Result<Vec<_>, _>>()?;

    if let Some(tools) = request.get("tools").filter(|value| !value.is_null()) {
        let tools = tools
            .as_array()
            .ok_or_else(|| invalid("tools must be an array"))?;
        if !tools.is_empty() {
            let tools = tools
                .iter()
                .map(normalize_tool)
                .collect::<Result<Vec<_>, _>>()?;
            let index = messages
                .iter()
                .position(|message| role(message) == "system")
                .unwrap_or_else(|| {
                    messages.insert(
                        0,
                        Map::from_iter([
                            ("role".to_owned(), Value::String("system".to_owned())),
                            ("content".to_owned(), Value::String(String::new())),
                        ])
                        .into(),
                    );
                    0
                });
            messages[index].insert("tools".to_owned(), Value::Array(tools));
            messages[index].normalized_tools = true;
        }
    }

    let mut kwargs = match request.get("chat_template_kwargs") {
        None | Some(Value::Null) => Map::new(),
        Some(value) => object(value, "chat_template_kwargs must be an object")?.clone(),
    };
    if let Some(effort) = request
        .get("reasoning_effort")
        .filter(|value| !value.is_null())
    {
        kwargs.insert("reasoning_effort".to_owned(), effort.clone());
        if !kwargs.contains_key("enable_thinking") {
            kwargs.insert("enable_thinking".to_owned(), Value::Bool(effort != "none"));
        }
    }
    // OnlineRenderer adds at least `tokenize=false` to server defaults, then
    // ChatParams.with_defaults skips null/"auto" request overrides. This merge
    // occurs *after* build_chat_params checks whether enable_thinking existed.
    kwargs.retain(|_, value| !value.is_null() && value != "auto");
    if kwargs.contains_key("tools") {
        return Err(RenderError::Unsupported(
            "DeepSeek template-level tools override",
        ));
    }
    let mut thinking =
        if !kwargs.contains_key("thinking") && !kwargs.contains_key("enable_thinking") {
            true
        } else {
            truthy(kwargs.get("thinking")) || truthy(kwargs.get("enable_thinking"))
        };
    let effort = match kwargs.get("reasoning_effort") {
        Some(Value::String(value)) if value == "none" => {
            thinking = false;
            50
        }
        None | Some(Value::Null) => 50,
        Some(Value::String(value)) => match value.as_str() {
            "low" => 25,
            "high" => 50,
            "xhigh" => 75,
            "max" => 100,
            _ => return Err(invalid("unsupported DeepSeek reasoning_effort")),
        },
        Some(Value::Number(value)) => value
            .as_u64()
            .filter(|value| (1..=100).contains(value))
            .ok_or_else(|| {
                invalid("DeepSeek numeric reasoning_effort must be an integer in 1..=100")
            })?,
        _ => return Err(invalid("invalid DeepSeek reasoning_effort type")),
    };
    let drop_thinking = kwargs
        .get("drop_thinking")
        .is_none_or(|value| truthy(Some(value)));
    encode_messages(messages, thinking, drop_thinking, effort)
}

fn invalid(message: &'static str) -> RenderError {
    RenderError::InvalidRequest(message)
}

fn object<'a>(
    value: &'a Value,
    message: &'static str,
) -> Result<&'a Map<String, Value>, RenderError> {
    value.as_object().ok_or_else(|| invalid(message))
}

fn role(message: &Message) -> &str {
    message.get("role").and_then(Value::as_str).unwrap_or("")
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(value)) => value.as_f64() != Some(0.0),
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Array(value)) => !value.is_empty(),
        Some(Value::Object(value)) => !value.is_empty(),
    }
}

fn optional_text(value: Option<&Value>, error: &'static str) -> Result<String, RenderError> {
    match value {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(value)) => Ok(value.clone()),
        _ => Err(invalid(error)),
    }
}

fn normalize_message(value: &Value) -> Result<Message, RenderError> {
    let mut message = Message::from(object(value, "message must be an object")?.clone());
    if !matches!(
        role(&message),
        "system" | "developer" | "user" | "assistant" | "tool"
    ) {
        return Err(invalid("unsupported DeepSeek message role"));
    }
    // The HTTP request pre-validator first aliases non-null legacy reasoning
    // when canonical reasoning is absent/null. Then the tokenizer normalizer
    // copies that resolved canonical field back to reasoning_content.
    if let Some(reasoning) = message
        .get("reasoning")
        .filter(|value| !value.is_null())
        .cloned()
    {
        message.insert("reasoning_content".to_owned(), reasoning);
    }
    if let Some(Value::Array(blocks)) = message.get("content") {
        let mut parts = Vec::with_capacity(blocks.len());
        for block in blocks {
            let block = object(block, "content block must be an object")?;
            match block.get("type").and_then(Value::as_str) {
                Some("text" | "input_text" | "output_text") => {
                    parts.push(optional_text(
                        block.get("text"),
                        "content block text must be a string",
                    )?);
                }
                Some("image_url" | "input_image" | "image_pil") => {
                    return Err(RenderError::Unsupported(
                        "DeepSeek image processor token expansion",
                    ));
                }
                _ => return Err(RenderError::Unsupported("DeepSeek non-text content block")),
            }
        }
        message.insert("content".to_owned(), Value::String(parts.join("\n\n")));
    }
    optional_text(
        message.get("content"),
        "message content must be text or null",
    )?;
    optional_text(
        message.get("reasoning_content"),
        "reasoning content must be text or null",
    )?;
    Ok(message)
}

fn normalize_tool(value: &Value) -> Result<Value, RenderError> {
    let tool = object(value, "tool must be an object")?;
    if tool
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind != "function")
    {
        return Err(RenderError::Unsupported("DeepSeek non-function tool"));
    }
    let function = object(
        tool.get("function")
            .ok_or_else(|| invalid("tool function is missing"))?,
        "tool function must be an object",
    )?;
    if function.get("name").and_then(Value::as_str).is_none() {
        return Err(invalid("tool function name must be a string"));
    }
    let mut normalized = Map::new();
    // FunctionDefinition's serializer prunes unknown fields. Its field order
    // is reproduced by render_tools, independently of serde_json's map order.
    for key in ["name", "description", "parameters"] {
        normalized.insert(
            key.to_owned(),
            function.get(key).cloned().unwrap_or(Value::Null),
        );
    }
    for key in ["strict", "defer_loading"] {
        let value = function
            .get(key)
            .filter(|value| !value.is_null())
            .or_else(|| {
                if key == "defer_loading" {
                    tool.get(key)
                } else {
                    None
                }
            });
        if let Some(value) = value.filter(|value| !value.is_null()) {
            normalized.insert(key.to_owned(), value.clone());
        }
    }
    Ok(Value::Object(Map::from_iter([(
        "function".to_owned(),
        Value::Object(normalized),
    )])))
}

fn merge_tool_messages(messages: Vec<Message>) -> Result<Vec<Message>, RenderError> {
    let mut merged: Vec<Message> = Vec::with_capacity(messages.len());
    for mut message in messages {
        match role(&message) {
            "tool" => {
                let block = Value::Object(Map::from_iter([
                    ("type".to_owned(), Value::String("tool_result".to_owned())),
                    (
                        "tool_use_id".to_owned(),
                        message
                            .get("tool_call_id")
                            .cloned()
                            .unwrap_or_else(|| Value::String(String::new())),
                    ),
                    (
                        "content".to_owned(),
                        message
                            .get("content")
                            .cloned()
                            .unwrap_or_else(|| Value::String(String::new())),
                    ),
                ]));
                if let Some(previous) = merged.last_mut().filter(|previous| {
                    role(previous) == "user" && previous.contains_key("content_blocks")
                }) {
                    previous
                        .get_mut("content_blocks")
                        .and_then(Value::as_array_mut)
                        .ok_or_else(|| invalid("content_blocks must be an array"))?
                        .push(block);
                } else {
                    merged.push(
                        Map::from_iter([
                            ("role".to_owned(), Value::String("user".to_owned())),
                            ("content_blocks".to_owned(), Value::Array(vec![block])),
                        ])
                        .into(),
                    );
                }
            }
            "user" => {
                let blocks = match message.get("content_blocks") {
                    None | Some(Value::Null) => vec![Value::Object(Map::from_iter([
                        ("type".to_owned(), Value::String("text".to_owned())),
                        (
                            "text".to_owned(),
                            message
                                .get("content")
                                .cloned()
                                .unwrap_or_else(|| Value::String(String::new())),
                        ),
                    ]))],
                    Some(Value::Array(blocks)) => blocks.clone(),
                    _ => return Err(invalid("content_blocks must be an array")),
                };
                if let Some(previous) = merged.last_mut().filter(|previous| {
                    role(previous) == "user"
                        && previous.contains_key("content_blocks")
                        && previous.get("task").is_none_or(Value::is_null)
                }) {
                    previous
                        .get_mut("content_blocks")
                        .and_then(Value::as_array_mut)
                        .ok_or_else(|| invalid("content_blocks must be an array"))?
                        .extend(blocks);
                } else {
                    message.insert("content_blocks".to_owned(), Value::Array(blocks));
                    merged.push(message);
                }
            }
            _ => merged.push(message),
        }
    }
    Ok(merged)
}

fn sort_tool_results(messages: &mut [Message]) -> Result<(), RenderError> {
    let mut order: HashMap<String, usize> = HashMap::new();
    for message in messages {
        if role(message) == "assistant" && truthy(message.get("tool_calls")) {
            let calls = message["tool_calls"]
                .as_array()
                .ok_or_else(|| invalid("tool_calls must be an array"))?;
            order.clear();
            for (index, call) in calls.iter().enumerate() {
                let id = call
                    .get("id")
                    .filter(|value| truthy(Some(value)))
                    .or_else(|| call.get("function").and_then(|function| function.get("id")));
                if let Some(id) = id.and_then(Value::as_str).filter(|id| !id.is_empty()) {
                    order.insert(id.to_owned(), index);
                }
            }
        } else if role(message) == "user" {
            if let Some(Value::Array(blocks)) = message.get_mut("content_blocks") {
                let mut tools: Vec<Value> = blocks
                    .iter()
                    .filter(|block| {
                        block.get("type").and_then(Value::as_str) == Some("tool_result")
                    })
                    .cloned()
                    .collect();
                if tools.len() > 1 && !order.is_empty() {
                    tools.sort_by_key(|block| {
                        block
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .and_then(|id| order.get(id))
                            .copied()
                            .unwrap_or(0)
                    });
                    let mut sorted = tools.into_iter();
                    for block in blocks {
                        if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                            *block = sorted.next().expect("tool block count is unchanged");
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn last_user_index(messages: &[Message]) -> Option<usize> {
    messages
        .iter()
        .enumerate()
        .rfind(|(index, message)| {
            matches!(role(message), "user" | "developer")
                || (role(message) == "system" && *index > 0)
        })
        .map(|(index, _)| index)
}

fn encode_messages(
    messages: Vec<Message>,
    thinking: bool,
    drop_thinking: bool,
    effort: u64,
) -> Result<String, RenderError> {
    let mut messages = merge_tool_messages(messages)?;
    sort_tool_results(&mut messages)?;
    let drop_thinking =
        drop_thinking && !messages.iter().any(|message| truthy(message.get("tools")));
    if thinking && drop_thinking {
        let last_user = last_user_index(&messages);
        messages = messages
            .into_iter()
            .enumerate()
            .filter_map(|(index, mut message)| {
                if matches!(
                    role(&message),
                    "user" | "system" | "tool" | "latest_reminder" | "direct_search_results"
                ) || last_user.is_none_or(|last| index >= last)
                {
                    Some(message)
                } else if role(&message) == "assistant" {
                    message.remove("reasoning_content");
                    Some(message)
                } else {
                    None
                }
            })
            .collect();
    }
    let last_user = last_user_index(&messages);
    let mut prompt = String::from(BOS);
    for (index, message) in messages.iter().enumerate() {
        if index == 0 && (thinking || role(message) == "system") {
            prompt.push_str(SYSTEM);
        }
        if index == 0 && thinking {
            write!(prompt, "Reasoning Effort: {effort} (range 1-100, the higher the value, the more thorough the reasoning)\n\n").expect("String write cannot fail");
        }
        let content = optional_text(message.get("content"), "content must be text")?;
        match role(message) {
            "system" | "developer" => {
                if role(message) == "developer" {
                    if content.is_empty() {
                        return Err(invalid("developer content must not be empty"));
                    }
                    prompt.push_str(USER);
                } else if index > 0 {
                    prompt.push_str(SYSTEM);
                }
                prompt.push_str(&content);
                if let Some(tools) = message.get("tools").filter(|tools| truthy(Some(tools))) {
                    prompt.push_str("\n\n");
                    render_tools(&mut prompt, tools, message.normalized_tools)?;
                }
                if let Some(format) = message
                    .get("response_format")
                    .filter(|format| truthy(Some(format)))
                {
                    prompt.push_str("\n\n## Response Format:\n\nYou MUST strictly adhere to the following schema to reply:\n");
                    write_json(&mut prompt, &OrderedValue::from(format));
                }
            }
            "user" => {
                prompt.push_str(USER);
                if let Some(Value::Array(blocks)) = message
                    .get("content_blocks")
                    .filter(|blocks| truthy(Some(blocks)))
                {
                    for (block_index, block) in blocks.iter().enumerate() {
                        if block_index > 0 {
                            prompt.push_str("\n\n");
                        }
                        match block.get("type").and_then(Value::as_str) {
                            Some("text") => prompt.push_str(&optional_text(
                                block.get("text"),
                                "text block must contain text",
                            )?),
                            Some("tool_result") => {
                                prompt.push_str("<tool_result>");
                                match block.get("content") {
                                    Some(Value::Array(parts)) => {
                                        for (part_index, part) in parts.iter().enumerate() {
                                            if part_index > 0 {
                                                prompt.push_str("\n\n");
                                            }
                                            if part.get("type").and_then(Value::as_str)
                                                == Some("text")
                                            {
                                                prompt.push_str(&optional_text(
                                                    part.get("text"),
                                                    "tool text block must contain text",
                                                )?);
                                            } else {
                                                return Err(RenderError::Unsupported(
                                                    "DeepSeek non-text tool result",
                                                ));
                                            }
                                        }
                                    }
                                    value => prompt.push_str(&optional_text(
                                        value,
                                        "tool result must contain text",
                                    )?),
                                }
                                prompt.push_str("</tool_result>");
                            }
                            _ => {
                                return Err(RenderError::Unsupported(
                                    "DeepSeek content_blocks type",
                                ))
                            }
                        }
                    }
                } else {
                    prompt.push_str(&content);
                }
            }
            "assistant" => {
                let previous_task = index
                    .checked_sub(1)
                    .and_then(|index| messages[index].get("task"))
                    .is_some_and(|value| !value.is_null());
                if thinking
                    && !previous_task
                    && (!drop_thinking || last_user.is_none_or(|last| index > last))
                {
                    prompt.push_str(&optional_text(
                        message.get("reasoning_content"),
                        "reasoning content must be text",
                    )?);
                    prompt.push_str("</think>");
                }
                prompt.push_str(&content);
                if let Some(calls) = message
                    .get("tool_calls")
                    .filter(|value| truthy(Some(value)))
                {
                    render_tool_calls(&mut prompt, calls)?;
                }
                if !truthy(message.get("wo_eos")) {
                    prompt.push_str(EOS);
                }
            }
            _ => return Err(invalid("unsupported normalized DeepSeek role")),
        }
        if messages
            .get(index + 1)
            .is_some_and(|next| !matches!(role(next), "assistant" | "latest_reminder"))
        {
            continue;
        }
        if let Some(task) = message.get("task").filter(|value| !value.is_null()) {
            let task = task
                .as_str()
                .ok_or_else(|| invalid("task must be a string"))?;
            if !matches!(
                task,
                "action" | "query" | "authority" | "domain" | "title" | "read_url"
            ) {
                return Err(invalid("unsupported DeepSeek task"));
            }
            if task == "action" {
                prompt.push_str(ASSISTANT);
                prompt.push_str(if thinking { "<think>" } else { "</think>" });
            }
            write!(prompt, "<｜{task}｜>").expect("String write cannot fail");
        } else if matches!(role(message), "user" | "developer")
            || (role(message) == "system" && index > 0)
        {
            prompt.push_str(ASSISTANT);
            prompt.push_str(
                if thinking && (!drop_thinking || last_user.is_none_or(|last| index >= last)) {
                    "<think>"
                } else {
                    "</think>"
                },
            );
        }
    }
    Ok(prompt)
}

fn render_tools(out: &mut String, tools: &Value, normalized: bool) -> Result<(), RenderError> {
    out.push_str(TOOLS_PREFIX);
    for (index, tool) in tools
        .as_array()
        .ok_or_else(|| invalid("message tools must be an array"))?
        .iter()
        .enumerate()
    {
        if index > 0 {
            out.push('\n');
        }
        let function = object(
            tool.get("function")
                .ok_or_else(|| invalid("tool function missing"))?,
            "tool function must be an object",
        )?;
        // Pydantic FunctionDefinition serializes in declaration order.
        if normalized {
            let fields = [
                "name",
                "description",
                "parameters",
                "strict",
                "defer_loading",
            ]
            .into_iter()
            .filter_map(|key| {
                function
                    .get(key)
                    .map(|value| (key.to_owned(), OrderedValue::from(value)))
            })
            .collect();
            write_json(out, &OrderedValue::Object(fields));
        } else {
            write_json(out, &OrderedValue::from(&Value::Object(function.clone())));
        }
    }
    out.push_str(TOOLS_SUFFIX);
    Ok(())
}

fn render_tool_calls(out: &mut String, calls: &Value) -> Result<(), RenderError> {
    let calls = calls
        .as_array()
        .ok_or_else(|| invalid("tool_calls must be an array"))?;
    out.push_str("\n\n<｜DSML｜ calls>\n");
    for (index, call) in calls.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let function = object(
            call.get("function")
                .ok_or_else(|| invalid("tool call function missing"))?,
            "tool call function must be an object",
        )?;
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("tool call name must be a string"))?;
        let original = function
            .get("arguments")
            .ok_or_else(|| invalid("tool call arguments missing"))?;
        let mut arguments = OrderedValue::from(original);
        for _ in 0..2 {
            if let OrderedValue::String(text) = &arguments {
                validate_argument_depth(text)?;
                match serde_json::from_str::<OrderedValue>(text) {
                    Ok(parsed) => arguments = parsed,
                    Err(_) => {
                        // Python also accepts NaN/Infinity in otherwise JSON
                        // payloads. Fail closed on those non-standard numbers.
                        if text.contains("NaN") || text.contains("Infinity") {
                            return Err(RenderError::Unsupported(
                                "non-standard JSON tool argument number",
                            ));
                        }
                        break;
                    }
                }
            } else {
                break;
            }
        }
        let fields = match arguments {
            OrderedValue::Object(fields) => fields,
            _ => vec![("arguments".to_owned(), OrderedValue::from(original))],
        };
        writeln!(out, "<｜DSML｜ invoke name=\"{name}\">").expect("String write cannot fail");
        for (index, (key, value)) in fields.iter().enumerate() {
            if index > 0 {
                out.push('\n');
            }
            write!(
                out,
                "<｜DSML｜ parameter name=\"{key}\" string=\"{}\">",
                matches!(value, OrderedValue::String(_))
            )
            .expect("String write cannot fail");
            match value {
                OrderedValue::String(text) => out.push_str(text),
                value => write_json(out, value),
            }
            out.push_str("</｜DSML｜ parameter>");
        }
        out.push_str("\n</｜DSML｜ invoke>");
    }
    out.push_str("\n</｜DSML｜ calls>");
    Ok(())
}

/// Object order in raw tool-argument strings must survive JSON parsing. The
/// ordinary Value map order still matches the actual outbound request order;
/// no crate-wide serde_json feature or public serialization change is needed.
#[derive(Debug, Clone)]
enum OrderedValue {
    Null,
    Bool(bool),
    Number(Number),
    RawNumber(String),
    String(String),
    Array(Vec<Self>),
    Object(Vec<(String, Self)>),
}

impl From<&Value> for OrderedValue {
    fn from(value: &Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Bool(value) => Self::Bool(*value),
            Value::Number(value) => Self::Number(value.clone()),
            Value::String(value) => Self::String(value.clone()),
            Value::Array(value) => Self::Array(value.iter().map(Self::from).collect()),
            Value::Object(value) => Self::Object(
                value
                    .iter()
                    .map(|(key, value)| (key.clone(), Self::from(value)))
                    .collect(),
            ),
        }
    }
}

impl<'de> Deserialize<'de> for OrderedValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        ordered_from_raw(&raw).map_err(de::Error::custom)
    }
}

fn ordered_from_raw(raw: &RawValue) -> Result<OrderedValue, serde_json::Error> {
    let text = raw.get().trim();
    match text.as_bytes()[0] {
        b'{' => {
            serde_json::from_str::<OrderedFields>(text).map(|fields| OrderedValue::Object(fields.0))
        }
        b'[' => serde_json::from_str::<Vec<OrderedValue>>(text).map(OrderedValue::Array),
        b'"' => serde_json::from_str::<String>(text).map(OrderedValue::String),
        b't' | b'f' => serde_json::from_str::<bool>(text).map(OrderedValue::Bool),
        b'n' => Ok(OrderedValue::Null),
        _ if !text.contains(['.', 'e', 'E']) => {
            // Preserve Python's arbitrary-precision integers after validating
            // the JSON grammar. Negative zero is normalized by json.loads.
            Ok(OrderedValue::RawNumber(
                if text == "-0" { "0" } else { text }.to_owned(),
            ))
        }
        _ => {
            let number: f64 = text
                .parse()
                .map_err(|_| <serde_json::Error as de::Error>::custom("invalid argument number"))?;
            Ok(OrderedValue::RawNumber(python_float(number)))
        }
    }
}

struct OrderedFields(Vec<(String, OrderedValue)>);

impl<'de> Deserialize<'de> for OrderedFields {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OrderedVisitor;
        impl<'de> Visitor<'de> for OrderedVisitor {
            type Value = OrderedFields;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON value")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut result: Vec<(String, OrderedValue)> = Vec::new();
                let mut slots: HashMap<String, usize> = HashMap::new();
                while let Some((key, value)) = map.next_entry::<String, OrderedValue>()? {
                    // Python json.loads keeps the first position of duplicate
                    // keys and the last associated value.
                    if let Some(slot) = slots.get(&key) {
                        result[*slot].1 = value;
                    } else {
                        slots.insert(key.clone(), result.len());
                        result.push((key, value));
                    }
                }
                Ok(OrderedFields(result))
            }
        }
        deserializer.deserialize_map(OrderedVisitor)
    }
}

/// RawValue deliberately scans nested JSON without serde's recursion limit.
/// Our ordered adapter starts fresh parsers for subtrees, so enforce one limit
/// on the entire original argument before any recursive conversion occurs.
fn validate_argument_depth(text: &str) -> Result<(), RenderError> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for byte in text.bytes() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
        } else {
            match byte {
                b'"' => in_string = true,
                b'{' | b'[' => {
                    depth += 1;
                    if depth > 128 {
                        return Err(RenderError::Unsupported(
                            "DeepSeek tool argument nesting exceeds parser limit",
                        ));
                    }
                }
                b'}' | b']' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
    }
    Ok(())
}

/// Python json.dumps(ensure_ascii=False)'s separators and numeric spelling are
/// model-visible, including spaces after commas/colons and exponent padding.
fn write_json(out: &mut String, value: &OrderedValue) {
    match value {
        OrderedValue::Null => out.push_str("null"),
        OrderedValue::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
        OrderedValue::Number(number) => {
            if number.is_i64() || number.is_u64() {
                write!(out, "{number}").expect("String write cannot fail");
            } else {
                out.push_str(&python_float(
                    number.as_f64().expect("JSON number is finite"),
                ));
            }
        }
        OrderedValue::RawNumber(spelling) => out.push_str(spelling),
        OrderedValue::String(text) => out
            .push_str(&serde_json::to_string(text).expect("String JSON serialization cannot fail")),
        OrderedValue::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_json(out, item);
            }
            out.push(']');
        }
        OrderedValue::Object(fields) => {
            out.push('{');
            for (index, (key, item)) in fields.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                out.push_str(
                    &serde_json::to_string(key).expect("String JSON serialization cannot fail"),
                );
                out.push_str(": ");
                write_json(out, item);
            }
            out.push('}');
        }
    }
}

fn python_float(number: f64) -> String {
    if number.is_infinite() {
        return if number.is_sign_negative() {
            "-Infinity"
        } else {
            "Infinity"
        }
        .to_owned();
    }
    // Share the Python-compatible finite formatter used by the other prompt
    // renderers, including Python's decimal/scientific notation thresholds.
    crate::kimi::python_float(&Number::from_f64(number).expect("JSON float is finite"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn matches_patched_python_reference_golden_prompts() {
        let fixtures: Value =
            serde_json::from_str(include_str!("../tests/fixtures/deepseek_v41_golden.json"))
                .unwrap();
        for fixture in fixtures["cases"].as_array().unwrap() {
            assert_eq!(
                render_chat(&fixture["request"]).unwrap(),
                fixture["prompt"].as_str().unwrap(),
                "fixture {}",
                fixture["id"]
            );
        }
    }

    #[test]
    fn rejects_media_instead_of_claiming_placeholder_count_is_exact() {
        let request = json!({"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://example.invalid/synthetic.png"}}]}]});
        assert!(matches!(
            render_chat(&request),
            Err(RenderError::Unsupported(_))
        ));
    }

    #[test]
    fn invalid_request_errors_do_not_include_private_content() {
        let request = json!({"messages":[{"role":"private-synthetic-marker","content":"private-synthetic-marker"}]});
        assert!(!render_chat(&request)
            .unwrap_err()
            .to_string()
            .contains("private-synthetic-marker"));
    }

    #[test]
    fn rejects_unsupported_efforts_instead_of_silently_choosing_a_budget() {
        for effort in [
            json!("medium"),
            json!("minimal"),
            json!(0),
            json!(101),
            json!(true),
            json!(1.0),
        ] {
            let request = json!({"messages":[{"role":"user","content":"synthetic"}],"chat_template_kwargs":{"reasoning_effort":effort}});
            assert!(render_chat(&request).is_err());
        }
    }

    #[test]
    fn rejects_template_tools_override_instead_of_undercounting() {
        let request = json!({"messages":[{"role":"user","content":"synthetic"}],"chat_template_kwargs":{"tools":[{"function":{"name":"override"}}]}});
        assert!(matches!(
            render_chat(&request),
            Err(RenderError::Unsupported(_))
        ));
    }

    #[test]
    fn bounds_nesting_in_opaque_tool_argument_strings() {
        let deep = format!("{}0{}", "[".repeat(10_000), "]".repeat(10_000));
        let request = json!({"messages":[{"role":"assistant","tool_calls":[{"function":{"name":"synthetic","arguments":deep}}]}]});
        assert!(matches!(
            render_chat(&request),
            Err(RenderError::Unsupported(_))
        ));
        // Brackets and escaped quotes inside a JSON string do not add depth.
        assert!(
            validate_argument_depth(&serde_json::to_string(&"[\\\"".repeat(10_000)).unwrap())
                .is_ok()
        );
    }

    #[test]
    fn matches_python_finite_float_reference_corpus() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/python_float.json")).unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            // Compare the same binary64 value. Default serde_json decimal
            // parsing is intentionally independent of the outbound formatter.
            let number: f64 = case["json"].as_str().unwrap().parse().unwrap();
            assert_eq!(python_float(number), case["expected"].as_str().unwrap());
        }
    }
}
