//! Text-only port of Moonshot's pinned Kimi K3 XTML encoder.
//!
//! Reference: `moonshotai/Kimi-K3@a590ce090cb049c93a33dfe8c208ec652aa20503`,
//! `encoding_k3.py` and `tokenization_kimi.py`. Segments intentionally remain
//! separate: structural markers allow special tokens; caller text never does.

use crate::render::PromptSegment;
use crate::{RenderError, SpecialTokenPolicy};
use serde_json::Value;
use std::collections::BTreeMap;

type Result<T> = std::result::Result<T, RenderError>;

fn invalid(message: &'static str) -> RenderError {
    RenderError::InvalidRequest(message)
}

#[derive(Default)]
struct Builder {
    segments: Vec<PromptSegment>,
}

impl Builder {
    fn text(&mut self, text: impl Into<String>) {
        self.segment(text, SpecialTokenPolicy::Ordinary);
    }

    fn control(&mut self, text: &str) {
        self.segment(text, SpecialTokenPolicy::AllowAll);
    }

    fn segment(&mut self, text: impl Into<String>, policy: SpecialTokenPolicy) {
        let text = text.into();
        if !text.is_empty() {
            self.segments.push(PromptSegment { text, policy });
        }
    }

    fn open(&mut self, tag: &str, attrs: &[(&str, String)]) {
        self.control("<|open|>");
        self.text(tag);
        for (name, value) in attrs {
            // Do not coalesce these four ordinary pieces. The source tokenizer
            // encodes each piece independently, so BPE cannot cross boundaries.
            self.text(format!(" {name}"));
            self.text("=\"");
            self.text(value.replace('&', "&amp;").replace('"', "&quot;"));
            self.text("\"");
        }
        self.control("<|sep|>");
    }

    fn close(&mut self, tag: &str) {
        self.control("<|close|>");
        self.text(tag);
        self.control("<|sep|>");
    }

    fn end(&mut self) {
        self.control("<|end_of_msg|>");
    }

    fn content(&mut self, content: Option<&Value>) -> Result<()> {
        match content {
            None | Some(Value::Null) => Ok(()),
            Some(Value::String(text)) => {
                self.text(text.clone());
                Ok(())
            }
            Some(Value::Array(parts)) => {
                for part in parts {
                    let part = part
                        .as_object()
                        .ok_or_else(|| invalid("Kimi content parts must be objects"))?;
                    match part.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            self.text(
                                part.get("text")
                                    .and_then(Value::as_str)
                                    .ok_or_else(|| invalid("Kimi text parts need string text"))?
                                    .to_owned(),
                            );
                        }
                        _ => {
                            return Err(RenderError::Unsupported(
                                "Kimi media requires the model processor; text tokenization is insufficient",
                            ));
                        }
                    }
                }
                Ok(())
            }
            _ => Err(invalid(
                "Kimi content must be a string, text parts, or null",
            )),
        }
    }

    fn internal(&mut self, kind: &str, body: &str) {
        self.open(
            "message",
            &[("role", "system".into()), ("type", kind.into())],
        );
        self.text(body.trim().to_owned());
        self.close("message");
        self.end();
    }

    fn tools(&mut self, tools: &Value, dynamic: bool) -> Result<()> {
        let json = python_json(&deep_sorted(tools), false)?;
        let body = if dynamic {
            format!(
                "## New Tools Available\nThe system dynamically extends the toolset via lazy-loading.\nYou have access to all existing and extended tools.\nHere are the specs for the extended tools.\n\n```json\n{json}\n```"
            )
        } else {
            format!(
                "# Tools\nHere are the available tools, described in JSONSchema.\n\n```json\n{json}\n```"
            )
        };
        self.open(
            "message",
            &[("role", "system".into()), ("type", "tool-declare".into())],
        );
        self.text(body);
        self.close("message");
        self.end();
        Ok(())
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn string(value: &Value, _field: &str) -> Result<String> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| invalid("Kimi field must be a string"))
}

fn optional_bool(value: Option<&Value>, default: bool, _field: &str) -> Result<bool> {
    match value {
        None => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        _ => Err(invalid("Kimi field must be a boolean")),
    }
}

fn call_function(call: &Value) -> Result<&Value> {
    if !call.is_object() {
        return Err(invalid("Kimi tool calls must be objects"));
    }
    let function = call.get("function").unwrap_or(call);
    if !function.is_object() {
        return Err(invalid("Kimi tool functions must be objects"));
    }
    Ok(function)
}

/// Match and reorder complete runs of tool results using opaque call IDs.
/// A partially unmatched run remains in original order, as in upstream.
fn normalize_tool_results(messages: &[Value]) -> Result<Vec<Value>> {
    let mut output = Vec::with_capacity(messages.len());
    let mut calls: BTreeMap<String, (usize, Option<String>)> = BTreeMap::new();
    let mut cursor = 0;
    while cursor < messages.len() {
        let message = &messages[cursor];
        let role = message.get("role").and_then(Value::as_str);
        if role == Some("assistant") {
            calls.clear();
            if let Some(tool_calls) = message.get("tool_calls").filter(|v| truthy(v)) {
                for (position, call) in tool_calls
                    .as_array()
                    .ok_or_else(|| invalid("Kimi tool_calls must be an array"))?
                    .iter()
                    .enumerate()
                {
                    let Some(id) = call.get("id").filter(|v| !v.is_null()) else {
                        continue;
                    };
                    let id = string(id, "tool call ID")?;
                    let name = call_function(call)?
                        .get("name")
                        .filter(|v| !v.is_null())
                        .map(|v| string(v, "tool name"))
                        .transpose()?;
                    calls.entry(id).or_insert((position + 1, name));
                }
            }
            output.push(message.clone());
            cursor += 1;
        } else if role != Some("tool") {
            output.push(message.clone());
            cursor += 1;
        } else {
            let begin = cursor;
            while cursor < messages.len()
                && messages[cursor].get("role").and_then(Value::as_str) == Some("tool")
            {
                cursor += 1;
            }
            let mut run = Vec::new();
            let mut unresolved = false;
            for (offset, tool) in messages[begin..cursor].iter().enumerate() {
                let id = tool.get("tool_call_id").or_else(|| tool.get("id"));
                let matched = id.and_then(Value::as_str).and_then(|id| calls.get(id));
                if matched.is_none() {
                    unresolved = true;
                }
                run.push((offset, tool.clone(), matched.cloned()));
            }
            if !unresolved {
                run.sort_by_key(|(offset, _, matched)| (matched.as_ref().unwrap().0, *offset));
                for (_, tool, matched) in &mut run {
                    if let Some((_, Some(name))) = matched {
                        let object = tool.as_object_mut().unwrap();
                        object.insert("tool".into(), Value::String(name.clone()));
                        if object.contains_key("name") {
                            object.insert("name".into(), Value::String(name.clone()));
                        }
                    }
                }
            }
            output.extend(run.into_iter().map(|(_, tool, _)| tool));
        }
    }
    Ok(output)
}

fn xtml_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn deep_sorted(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut keys: Vec<_> = object.keys().collect();
            keys.sort();
            Value::Object(
                keys.into_iter()
                    .map(|key| (key.clone(), deep_sorted(&object[key])))
                    .collect(),
            )
        }
        Value::Array(values) => Value::Array(values.iter().map(deep_sorted).collect()),
        _ => value.clone(),
    }
}

/// Python's `json.dumps(ensure_ascii=False)` spacing is observable in XTML
/// dictionary arguments; compact tools/schema use the same routine without spaces.
fn python_json(value: &Value, spaces: bool) -> Result<String> {
    let separator = if spaces { ", " } else { "," };
    let colon = if spaces { ": " } else { ":" };
    match value {
        Value::Array(values) => Ok(format!(
            "[{}]",
            values
                .iter()
                .map(|value| python_json(value, spaces))
                .collect::<Result<Vec<_>>>()?
                .join(separator)
        )),
        Value::Object(values) => Ok(format!(
            "{{{}}}",
            values
                .iter()
                .map(|(key, value)| {
                    Ok(format!(
                        "{}{colon}{}",
                        serde_json::to_string(key)
                            .map_err(|_| invalid("Kimi JSON serialization failed"))?,
                        python_json(value, spaces)?
                    ))
                })
                .collect::<Result<Vec<_>>>()?
                .join(separator)
        )),
        Value::Number(number) if number.is_f64() => Ok(python_float(number)),
        _ => serde_json::to_string(value).map_err(|_| invalid("Kimi JSON serialization failed")),
    }
}

pub(crate) fn python_float(number: &serde_json::Number) -> String {
    let representation = number.to_string();
    let (sign, unsigned) = if let Some(unsigned) = representation.strip_prefix('-') {
        ("-", unsigned)
    } else {
        ("", representation.as_str())
    };
    let (mantissa, exponent) = unsigned
        .split_once(['e', 'E'])
        .map(|(mantissa, exponent)| (mantissa, exponent.parse::<i32>().unwrap()))
        .unwrap_or((unsigned, 0));
    let decimal = mantissa.find('.').unwrap_or(mantissa.len());
    let raw: String = mantissa
        .chars()
        .filter(|character| *character != '.')
        .collect();
    let first = raw.find(|character| character != '0');
    let Some(first) = first else {
        return format!("{sign}0.0");
    };
    let exponent = exponent + decimal as i32 - first as i32 - 1;
    let digits = raw[first..].trim_end_matches('0');
    if !(-4..16).contains(&exponent) {
        let fraction = if digits.len() > 1 {
            format!(".{}", &digits[1..])
        } else {
            String::new()
        };
        format!(
            "{sign}{}{fraction}e{}{:02}",
            &digits[..1],
            if exponent < 0 { '-' } else { '+' },
            exponent.abs()
        )
    } else if exponent < 0 {
        format!("{sign}0.{}{digits}", "0".repeat((-exponent - 1) as usize))
    } else {
        let point = exponent as usize + 1;
        if point >= digits.len() {
            format!("{sign}{digits}{}.0", "0".repeat(point - digits.len()))
        } else {
            format!("{sign}{}.{}", &digits[..point], &digits[point..])
        }
    }
}

#[derive(Debug)]
struct Argument {
    key: String,
    kind: &'static str,
    text: String,
}

fn whitespace(bytes: &[u8], cursor: &mut usize) {
    while bytes
        .get(*cursor)
        .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r'))
    {
        *cursor += 1;
    }
}

fn valid_json_number(bytes: &[u8]) -> bool {
    let mut index = usize::from(bytes.first() == Some(&b'-'));
    match bytes.get(index) {
        Some(b'0') => index += 1,
        Some(b'1'..=b'9') => {
            index += 1;
            while bytes.get(index).is_some_and(u8::is_ascii_digit) {
                index += 1;
            }
        }
        _ => return false,
    }
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let begin = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if begin == index {
            return false;
        }
    }
    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        index += 1;
        if matches!(bytes.get(index), Some(b'+' | b'-')) {
            index += 1;
        }
        let begin = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if begin == index {
            return false;
        }
    }
    index == bytes.len()
}

fn decode_at(source: &str, cursor: &mut usize) -> std::result::Result<Value, ()> {
    let mut stream = serde_json::Deserializer::from_str(&source[*cursor..]).into_iter::<Value>();
    if let Some(Ok(value)) = stream.next() {
        *cursor += stream.byte_offset();
        return Ok(value);
    }
    // Python's JSONDecoder(strict=False) also accepts literal controls inside
    // strings and its NaN/Infinity constants. Preserve source offsets while
    // validating an equivalent sanitized value; only strings/type are consumed.
    let raw = &source[*cursor..];
    let bytes = raw.as_bytes();
    let composite = matches!(bytes.first(), Some(b'{' | b'['));
    let mut quoted = false;
    let mut escaped = false;
    let mut depth = 0usize;
    let mut end = None;
    for (index, byte) in bytes.iter().enumerate() {
        if quoted {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                quoted = false;
                if index > 0 && bytes[0] == b'"' {
                    end = Some(index + 1);
                    break;
                }
            }
        } else {
            match *byte {
                b'"' => quoted = true,
                b'{' | b'[' => depth += 1,
                b'}' | b']' if depth > 0 => {
                    depth -= 1;
                    if composite && depth == 0 {
                        end = Some(index + 1);
                        break;
                    }
                }
                b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n' if depth == 0 => {
                    end = Some(index);
                    break;
                }
                _ => (),
            }
        }
    }
    let end = end.unwrap_or(raw.len());
    let original = &raw[..end];
    let mut sanitized = String::with_capacity(end);
    let mut quoted = false;
    let mut escaped = false;
    let mut index = 0;
    while index < original.len() {
        let character = original[index..].chars().next().ok_or(())?;
        if quoted {
            if escaped && character <= '\u{1f}' {
                return Err(());
            }
            if character <= '\u{1f}' {
                sanitized.push_str(&format!("\\u{:04x}", character as u32));
            } else {
                sanitized.push(character);
            }
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                quoted = false;
            }
        } else if character == '"' {
            quoted = true;
            sanitized.push(character);
        } else if let Some(constant) =
            ["-Infinity", "Infinity", "NaN"]
                .into_iter()
                .find(|constant| {
                    original[index..].starts_with(constant)
                        && original
                            .as_bytes()
                            .get(index + constant.len())
                            .is_none_or(|byte| {
                                matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | b',' | b'}' | b']')
                            })
                })
        {
            sanitized.push_str("0.0");
            index += constant.len();
            continue;
        } else if character == '-' || character.is_ascii_digit() {
            let end = original[index..]
                .find(|character: char| {
                    matches!(character, ' ' | '\t' | '\r' | '\n' | ',' | '}' | ']')
                })
                .map(|end| index + end)
                .unwrap_or(original.len());
            let number = &original[index..end];
            if valid_json_number(number.as_bytes()) {
                // Python accepts overflowing floats and arbitrary-precision
                // integers. Only the outer type is needed here; retain the
                // exact literal separately for the emitted text.
                sanitized.push_str("0.0");
                index = end;
                continue;
            }
            sanitized.push(character);
        } else {
            sanitized.push(character);
        }
        index += character.len_utf8();
    }
    let value = serde_json::from_str(&sanitized).map_err(|_| ())?;
    *cursor += end;
    Ok(value)
}

/// Upstream preserves non-string argument literals byte-for-byte and discards
/// trailing text after a valid object. Parsing the whole object into Value would
/// lose exponent spelling, whitespace, duplicate keys, and original key order.
fn parse_argument_string(source: &str) -> std::result::Result<Vec<Argument>, ()> {
    let mut cursor = 0;
    let bytes = source.as_bytes();
    whitespace(bytes, &mut cursor);
    if bytes.get(cursor) != Some(&b'{') {
        return Err(());
    }
    cursor += 1;
    whitespace(bytes, &mut cursor);
    let mut arguments = Vec::new();
    if bytes.get(cursor) == Some(&b'}') {
        return Ok(arguments);
    }
    loop {
        let key = decode_at(source, &mut cursor)?
            .as_str()
            .ok_or(())?
            .to_owned();
        whitespace(bytes, &mut cursor);
        if bytes.get(cursor) != Some(&b':') {
            return Err(());
        }
        cursor += 1;
        whitespace(bytes, &mut cursor);
        let begin = cursor;
        let value = decode_at(source, &mut cursor)?;
        arguments.push(Argument {
            key,
            kind: xtml_type(&value),
            text: match value {
                Value::String(value) => value,
                _ => source[begin..cursor].to_owned(),
            },
        });
        whitespace(bytes, &mut cursor);
        let delimiter = bytes.get(cursor).ok_or(())?;
        cursor += 1;
        whitespace(bytes, &mut cursor);
        match delimiter {
            b'}' => return Ok(arguments),
            b',' => (),
            _ => return Err(()),
        }
    }
}

fn render_arguments(builder: &mut Builder, arguments: Option<&Value>) -> Result<()> {
    let arguments = match arguments {
        None | Some(Value::Null) => return Ok(()),
        Some(Value::String(value)) if value.is_empty() => return Ok(()),
        Some(Value::String(value)) => match parse_argument_string(value) {
            Ok(arguments) => arguments,
            Err(()) => {
                // Python accepts lone UTF-16 surrogates in JSON strings, but
                // Rust strings cannot represent them. Do not silently switch
                // a valid Python argument object to the malformed-JSON block.
                let lower = value.to_ascii_lowercase();
                if [
                    "\\ud8", "\\ud9", "\\uda", "\\udb", "\\udc", "\\udd", "\\ude", "\\udf",
                ]
                .iter()
                .any(|prefix| lower.contains(prefix))
                {
                    return Err(RenderError::Unsupported(
                        "Kimi unpaired Unicode surrogate arguments",
                    ));
                }
                builder.open("json", &[("type", "object".into())]);
                builder.text(value.clone());
                builder.close("json");
                return Ok(());
            }
        },
        Some(Value::Object(value)) => value
            .iter()
            .map(|(key, value)| {
                Ok(Argument {
                    key: key.clone(),
                    kind: xtml_type(value),
                    text: match value {
                        Value::String(value) => value.clone(),
                        _ => python_json(value, true)?,
                    },
                })
            })
            .collect::<Result<Vec<_>>>()?,
        _ => {
            return Err(invalid(
                "Kimi tool arguments must be an object, string, or null",
            ))
        }
    };
    for argument in arguments {
        builder.open(
            "argument",
            &[("key", argument.key), ("type", argument.kind.into())],
        );
        builder.text(argument.text);
        builder.close("argument");
    }
    Ok(())
}

/// Render one OpenAI-compatible text conversation to K3's exact XTML segments.
/// Top-level reasoning_effort/enable_thinking are not aliases for the official
/// encoder's `thinking`/`thinking_effort` kwargs; no new mapping is invented here.
pub fn render_kimi_segments(request: &Value) -> Result<Vec<PromptSegment>> {
    let kwargs = match request.get("chat_template_kwargs") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_object()
                .ok_or_else(|| invalid("chat_template_kwargs must be an object"))?,
        ),
    };
    let get_kwarg = |key: &str| kwargs.and_then(|kwargs| kwargs.get(key));
    for key in [
        "messages",
        "conversation",
        "tools",
        "tokenize",
        "chat_template",
        "return_tensors",
        "return_dict",
        "padding",
        "truncation",
        "max_length",
    ] {
        if get_kwarg(key).is_some() {
            return Err(RenderError::Unsupported(
                "Kimi tokenizer/template override is not supported",
            ));
        }
    }
    if request.get("continue_final_message").is_some_and(truthy)
        || get_kwarg("continue_final_message").is_some_and(truthy)
    {
        return Err(RenderError::Unsupported(
            "Kimi assistant continuation is not supported",
        ));
    }
    if get_kwarg("image_prompts").is_some_and(|value| !value.is_null()) {
        return Err(RenderError::Unsupported(
            "Kimi image prompts require the media processor",
        ));
    }
    let thinking = optional_bool(get_kwarg("thinking"), true, "thinking")?;
    let add_generation_prompt = optional_bool(
        get_kwarg("add_generation_prompt").or_else(|| request.get("add_generation_prompt")),
        true,
        "add_generation_prompt",
    )?;
    let thinking_effort = get_kwarg("thinking_effort")
        .cloned()
        .unwrap_or_else(|| Value::String("max".into()));
    let messages = request
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("Kimi request needs a messages array"))?;
    let messages = normalize_tool_results(messages)?;
    let mut builder = Builder::default();
    if let Some(tools) = request.get("tools").filter(|value| truthy(value)) {
        if !tools.is_array() {
            return Err(invalid("Kimi tools must be an array"));
        }
        builder.tools(tools, false)?;
    }
    if thinking && !thinking_effort.is_null() {
        let effort = string(&thinking_effort, "thinking_effort")?;
        if !matches!(effort.as_str(), "low" | "high" | "max") {
            return Err(invalid("Kimi thinking_effort must be low, high, or max"));
        }
        builder.internal("thinking-effort", &format!("`thinking_effort` guides on how much to think in your thinking channel (not including the response channel), supported values include `low`, `medium`, `high`, and `max`.\nNow the system is invoked with `thinking_effort={effort}`."));
    }
    let mut previous_calls: Option<&Vec<Value>> = None;
    let mut tool_index = 0;
    for message in &messages {
        let object = message
            .as_object()
            .ok_or_else(|| invalid("Kimi messages must be objects"))?;
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("Kimi messages need a role"))?;
        if role == "system" && object.get("tools").is_some_and(truthy) {
            builder.tools(&object["tools"], true)?;
            continue;
        }
        let mut attrs = vec![("role", role.to_owned())];
        if role == "tool" {
            tool_index += 1;
            let name = object.get("tool").or_else(|| object.get("name"));
            let name = if name.is_none_or(Value::is_null) {
                previous_calls
                    .and_then(|calls| calls.get(tool_index - 1))
                    .map(call_function)
                    .transpose()?
                    .and_then(|function| function.get("name"))
            } else {
                name
            }
            .ok_or_else(|| invalid("Kimi tool result has no resolvable tool name"))?;
            attrs.push(("tool", string(name, "tool name")?));
            attrs.push(("index", tool_index.to_string()));
        } else if let Some(name) = object.get("name").filter(|value| truthy(value)) {
            attrs.push(("name", string(name, "message name")?));
        }
        if !matches!(role, "user" | "system" | "tool" | "assistant") {
            return Err(invalid("unsupported Kimi message role"));
        }
        builder.open("message", &attrs);
        if role == "assistant" {
            previous_calls = object
                .get("tool_calls")
                .filter(|value| !value.is_null())
                .map(|value| {
                    value
                        .as_array()
                        .ok_or_else(|| invalid("Kimi tool_calls must be an array"))
                })
                .transpose()?;
            tool_index = 0;
            if thinking {
                builder.open("think", &[]);
                let reasoning = object
                    .get("reasoning_content")
                    .filter(|value| truthy(value))
                    .or_else(|| object.get("reasoning"));
                if let Some(reasoning) = reasoning.filter(|value| !value.is_null()) {
                    builder.text(string(reasoning, "reasoning")?);
                }
                builder.close("think");
            }
            builder.open("response", &[]);
            builder.content(object.get("content"))?;
            builder.close("response");
            if let Some(calls) = previous_calls.filter(|calls| !calls.is_empty()) {
                builder.open("tools", &[]);
                for (index, call) in calls.iter().enumerate() {
                    let function = call_function(call)?;
                    let name = function
                        .get("name")
                        .ok_or_else(|| invalid("Kimi tool call needs a name"))?;
                    builder.open(
                        "call",
                        &[
                            ("tool", string(name, "tool name")?),
                            ("index", (index + 1).to_string()),
                        ],
                    );
                    render_arguments(&mut builder, function.get("arguments"))?;
                    builder.close("call");
                }
                builder.close("tools");
            }
        } else {
            builder.content(object.get("content"))?;
        }
        builder.close("message");
        builder.end();
    }
    // vLLM collapses its default tool_choice="none" for requests without tools.
    let tool_choice = if request.get("tools").is_some_and(truthy) {
        request
            .get("tool_choice")
            .or_else(|| get_kwarg("tool_choice"))
    } else {
        get_kwarg("tool_choice")
    };
    match tool_choice.and_then(Value::as_str) {
        Some("required") => builder.internal("tool-choice", "The system is invoked with `tool_choice=required`.\nYou MUST call tools in the next message."),
        Some("none") => builder.internal("tool-choice", "The system is invoked with `tool_choice=none`.\nYou MUST NOT call any tools in the next message."),
        _ => (),
    }
    let response_format = request
        .get("response_format")
        .or_else(|| get_kwarg("response_format"));
    let format_type = response_format
        .and_then(|value| value.get("type").or(Some(value)))
        .and_then(Value::as_str);
    match format_type {
        Some("json_object") => builder.internal("response-format", "The system is invoked with `response_format=json_object`.\nYour response must be raw JSON data without markdown code blocks (```json) or any additional formatting."),
        Some("json_schema") => {
            let schema = get_kwarg("response_schema").or_else(|| response_format.and_then(|value| value.get("json_schema")).map(|value| value.get("schema").or_else(|| value.get("json_schema")).unwrap_or(value)));
            let schema = python_json(&deep_sorted(schema.unwrap_or(&Value::Null)), false)?;
            builder.internal("response-format", &format!("The system is invoked with `response_format=json_schema`.\nYour response must be raw JSON data without markdown code blocks (```json) or any additional formatting.\nThe JSON data must match the following schema:\n```json\n{schema}\n```"));
        }
        _ => (),
    }
    if add_generation_prompt {
        builder.open("message", &[("role", "assistant".into())]);
        builder.open(if thinking { "think" } else { "response" }, &[]);
    }
    Ok(builder.segments)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_python_segment_goldens() {
        let fixtures: Value =
            serde_json::from_str(include_str!("../tests/fixtures/kimi_xtml.json")).unwrap();
        for case in fixtures["cases"].as_array().unwrap() {
            let actual = render_kimi_segments(&case["request"]).unwrap();
            let actual: Vec<Value> = actual.iter().map(|segment| serde_json::json!({"text":segment.text,"allow_special":matches!(segment.policy, SpecialTokenPolicy::AllowAll)})).collect();
            assert_eq!(Value::Array(actual), case["segments"], "{}", case["name"]);
        }
    }

    #[test]
    fn rejects_media_and_unimplemented_continuation() {
        for request in [
            serde_json::json!({"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://invalid.test/image"}}]}]}),
            serde_json::json!({"messages":[],"continue_final_message":true}),
            serde_json::json!({"messages":[{"role":"assistant","content":null,"tool_calls":[{"function":{"name":"f","arguments":"{\"s\":\"\\ud800\"}"}}]}]}),
        ] {
            assert!(matches!(
                render_kimi_segments(&request),
                Err(RenderError::Unsupported(_))
            ));
        }
    }

    #[test]
    fn python_json_float_goldens() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/python_float.json")).unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            // The formatter receives an existing outbound f64, not an inbound
            // JSON decimal. Construct the exact Python-equivalent value without
            // changing serde_json's feature-unified parsing policy for callers.
            let value: f64 = case["json"].as_str().unwrap().parse().unwrap();
            let number = serde_json::Number::from_f64(value).unwrap();
            assert_eq!(
                python_float(&number),
                case["expected"].as_str().unwrap(),
                "{}",
                case["json"]
            );
        }
    }
}
