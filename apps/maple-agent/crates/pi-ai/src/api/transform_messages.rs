//! Pi v1.0.4 `api/transform-messages.ts`.
use crate::{env::PiEnv, types::*};
use std::collections::{HashMap, HashSet};
pub const NON_VISION_USER_IMAGE_PLACEHOLDER: &str =
    "(image omitted: model does not support images)";
pub const NON_VISION_TOOL_IMAGE_PLACEHOLDER: &str =
    "(tool image omitted: model does not support images)";
pub type NormalizeToolCallId<'a> = dyn Fn(&JsString, &Model, &Message) -> JsString + 'a;
fn replace_images(content: Vec<UserContent>, placeholder: &str) -> Vec<UserContent> {
    let mut result = Vec::new();
    let mut previous_placeholder = false;
    for block in content {
        match block {
            UserContent::Image(_) => {
                if !previous_placeholder {
                    result.push(TextContent::new(placeholder).into());
                }
                previous_placeholder = true;
            }
            UserContent::Text(text) => {
                previous_placeholder = text.text == placeholder;
                result.push(text.into());
            }
        }
    }
    result
}
pub(crate) fn has_non_whitespace(text: &JsString) -> bool {
    text.units().any(|unit| !matches!(unit, 0x0009..=0x000d | 0x0020 | 0x00a0 | 0x1680 | 0x2000..=0x200a | 0x2028 | 0x2029 | 0x202f | 0x205f | 0x3000 | 0xfeff))
}
/// Repair replay histories while retaining Pi's source-order tool accounting.
/// Use `transform_messages_raw` for histories whose content is null or absent;
/// that adapter repairs content here without changing imported session records.
pub fn transform_messages(
    messages: &[Message],
    model: &Model,
    normalize: Option<&NormalizeToolCallId<'_>>,
    env: &dyn PiEnv,
) -> Result<Vec<Message>, JsString> {
    if messages
        .iter()
        .any(|message| matches!(message, Message::Raw(_)))
    {
        let raw = crate::utils::js_value::to_js_value(messages)
            .map_err(|error| JsString::from(error.to_string()))?;
        let mut transformed =
            transform_messages_raw_inner(&raw, model, normalize, env, Some(messages))
                .map_err(|error| JsString::from(error.to_string()))?;
        // The raw compatibility path must retain existing shared tool arguments.
        // Assistant order is unchanged by repair; only errored turns are removed.
        let mut originals = messages.iter().filter(|message| match message {
            Message::Assistant(assistant) => !matches!(
                assistant.stop_reason,
                StopReason::Error | StopReason::Aborted
            ),
            Message::Raw(raw) => raw.read(|object| {
                object.get("role").and_then(JsValue::as_str) == Some("assistant")
                    && !matches!(
                        object.get("stopReason").and_then(JsValue::as_str),
                        Some("error" | "aborted")
                    )
            }),
            _ => false,
        });
        for message in &mut transformed {
            let Message::Raw(raw) = message else { continue };
            if raw.role() != "assistant" {
                continue;
            }
            let Some(Message::Assistant(original)) = originals.next() else {
                continue;
            };
            let mut assistant: AssistantMessage =
                crate::utils::js_value::from_js_value(raw.snapshot().into())
                    .map_err(|error| JsString::from(error.to_string()))?;
            let mut calls = original.content.iter().filter_map(|block| match block {
                AssistantContent::ToolCall(call) => Some(call),
                _ => None,
            });
            for block in &mut assistant.content {
                if let AssistantContent::ToolCall(call) = block {
                    call.arguments = calls
                        .next()
                        .expect("transform preserves typed tool call order")
                        .arguments
                        .clone();
                }
            }
            *message = Message::Assistant(assistant);
        }
        return Ok(transformed);
    }
    let mut ids = HashMap::<JsString, JsString>::new();
    let vision = model.input.contains(&InputModality::Image);
    let transformed = messages
        .iter()
        .cloned()
        .map(|mut message| {
            match &mut message {
                Message::User(user) if !vision => {
                    if let UserMessageContent::Blocks(blocks) = &mut user.content {
                        *blocks = replace_images(
                            std::mem::take(blocks),
                            NON_VISION_USER_IMAGE_PLACEHOLDER,
                        );
                    }
                }
                Message::ToolResult(result) => {
                    if !vision {
                        result.content = replace_images(
                            std::mem::take(&mut result.content),
                            NON_VISION_TOOL_IMAGE_PLACEHOLDER,
                        );
                    }
                    if let Some(id) = ids.get(&result.tool_call_id).filter(|id| !id.is_empty()) {
                        result.tool_call_id = id.clone();
                    }
                }
                Message::Assistant(assistant) => {
                    let same = assistant.provider == model.provider
                        && assistant.api == model.api
                        && assistant.model == model.id;
                    assistant.content = assistant
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            AssistantContent::Thinking(thinking) => {
                                if thinking.redacted == Some(true) {
                                    return same.then(|| block.clone());
                                }
                                if same
                                    && thinking
                                        .thinking_signature
                                        .as_ref()
                                        .is_some_and(|s| !s.is_empty())
                                {
                                    return Some(block.clone());
                                }
                                if !has_non_whitespace(&thinking.thinking) {
                                    return None;
                                }
                                Some(if same {
                                    block.clone()
                                } else {
                                    TextContent::new(thinking.thinking.clone()).into()
                                })
                            }
                            AssistantContent::Text(text) => Some(if same {
                                block.clone()
                            } else {
                                TextContent::new(text.text.clone()).into()
                            }),
                            AssistantContent::ToolCall(call) => {
                                let mut call = call.clone();
                                if !same {
                                    if call
                                        .thought_signature
                                        .as_ref()
                                        .is_some_and(|s| !s.is_empty())
                                    {
                                        call.thought_signature = None;
                                    }
                                    if let Some(normalize) = normalize {
                                        let id = normalize(
                                            &call.id,
                                            model,
                                            &Message::Assistant(assistant.clone()),
                                        );
                                        if id != call.id {
                                            ids.insert(call.id.clone(), id.clone());
                                            call.id = id;
                                        }
                                    }
                                }
                                Some(call.into())
                            }
                        })
                        .collect();
                }
                _ => {}
            }
            message
        })
        .collect::<Vec<_>>();
    let mut result = Vec::new();
    let mut pending = Vec::<ToolCall>::new();
    let mut existing = HashSet::new();
    let mut held = Vec::new();
    let close = |result: &mut Vec<Message>,
                 pending: &mut Vec<ToolCall>,
                 existing: &mut HashSet<JsString>,
                 held: &mut Vec<Message>| {
        for call in pending.drain(..) {
            if !existing.contains(&call.id) {
                result.push(
                    ToolResultMessage {
                        tool_call_id: call.id,
                        tool_name: call.name,
                        content: vec![TextContent::new("No result provided").into()],
                        is_error: true,
                        timestamp: env.now_ms() as f64,
                        ..Default::default()
                    }
                    .into(),
                );
            }
        }
        existing.clear();
        result.append(held);
    };
    for message in transformed {
        match &message {
            Message::Assistant(assistant) => {
                close(&mut result, &mut pending, &mut existing, &mut held);
                if matches!(
                    assistant.stop_reason,
                    StopReason::Error | StopReason::Aborted
                ) {
                    continue;
                }
                pending = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        AssistantContent::ToolCall(call) => Some(call.clone()),
                        _ => None,
                    })
                    .collect();
            }
            Message::ToolResult(tool) => {
                existing.insert(tool.tool_call_id.clone());
            }
            Message::System(_) if !pending.is_empty() => {
                held.push(message);
                continue;
            }
            Message::User(_) => close(&mut result, &mut pending, &mut existing, &mut held),
            _ => {}
        }
        result.push(message);
    }
    close(&mut result, &mut pending, &mut existing, &mut held);
    Ok(result)
}

/// Entry point for untyped callers. Pi repairs null or absent content here.
/// Untouched message and content-block properties retain their exact values.
pub fn transform_messages_raw(
    messages: &JsValue,
    model: &Model,
    normalize: Option<&NormalizeToolCallId<'_>>,
    env: &dyn PiEnv,
) -> Result<Vec<Message>, crate::utils::js_value::JsonConversionError> {
    transform_messages_raw_inner(messages, model, normalize, env, None)
}
fn transform_messages_raw_inner(
    messages: &JsValue,
    model: &Model,
    normalize: Option<&NormalizeToolCallId<'_>>,
    env: &dyn PiEnv,
    source_messages: Option<&[Message]>,
) -> Result<Vec<Message>, crate::utils::js_value::JsonConversionError> {
    use crate::utils::js_value::JsonConversionError;
    let error = |message: &str| JsonConversionError::new(message);
    let array = messages
        .as_array()
        .ok_or_else(|| error("message history must be an array"))?;
    let vision = model.input.contains(&InputModality::Image);
    let mut ids = HashMap::<JsString, JsString>::new();
    let mut normalized = Vec::new();
    for message in array {
        let mut message = message
            .as_object()
            .ok_or_else(|| error("message must be an object"))?
            .clone();
        if message.get("content").is_none_or(JsValue::is_null) {
            message.insert("content", JsValue::Array(Vec::new()));
        }
        normalized.push(message);
    }
    // Pi completes image downgrade for the entire history before invoking any
    // normalization callback, including when a later image block throws.
    for message in &mut normalized {
        let role = message
            .get("role")
            .and_then(JsValue::as_str)
            .unwrap_or("")
            .to_owned();
        if !vision
            && (role == "toolResult"
                || (role == "user" && message.get("content").is_some_and(JsValue::is_array)))
        {
            let blocks = match message.get("content") {
                Some(JsValue::Array(blocks)) => blocks.clone(),
                Some(JsValue::String(text)) => raw_string_iterator(text),
                _ => return Err(error("content is not iterable")),
            };
            let placeholder = if role == "user" {
                NON_VISION_USER_IMAGE_PLACEHOLDER
            } else {
                NON_VISION_TOOL_IMAGE_PLACEHOLDER
            };
            let mut result = Vec::new();
            let mut previous = false;
            for block in &blocks {
                let kind =
                    crate::utils::raw_message::property(Some(block), "type").map_err(|error| {
                        JsonConversionError::new(
                            error.as_str().expect("property error text is ASCII"),
                        )
                    })?;
                if kind.and_then(JsValue::as_str) == Some("image") {
                    if !previous {
                        result.push(raw_text(placeholder.into()));
                    }
                    previous = true;
                } else {
                    result.push(block.clone());
                    previous = block
                        .get("text")
                        .and_then(JsValue::as_js_str)
                        .is_some_and(|text| text == placeholder);
                }
            }
            message.insert("content", result.into());
        }
    }
    let mut transformed = Vec::new();
    for (source_index, mut message) in normalized.into_iter().enumerate() {
        let role = message
            .get("role")
            .and_then(JsValue::as_str)
            .unwrap_or("")
            .to_owned();
        if role == "toolResult" {
            if let Some(id) = message
                .get("toolCallId")
                .and_then(JsValue::as_js_str)
                .and_then(|id| ids.get(id))
                .filter(|id| !id.is_empty())
            {
                message.insert("toolCallId", id.clone().into());
            }
        } else if role == "assistant" {
            let same = message.get("provider").and_then(JsValue::as_str)
                == Some(model.provider.as_str())
                && message.get("api").and_then(JsValue::as_str) == Some(model.api.as_str())
                && message.get("model").and_then(JsValue::as_str) == Some(model.id.as_str());
            let blocks = message
                .get("content")
                .and_then(JsValue::as_array)
                .ok_or_else(|| error("assistantMsg.content.flatMap is not a function"))?;
            let mut content = Vec::new();
            for original in blocks {
                let mut block = original.clone();
                let kind =
                    crate::utils::raw_message::property(Some(&block), "type").map_err(|error| {
                        JsonConversionError::new(
                            error.as_str().expect("property error text is ASCII"),
                        )
                    })?;
                match kind.and_then(JsValue::as_str) {
                    Some("thinking") => {
                        if block.get("redacted").is_some_and(raw_truthy) {
                            if same {
                                content.push(block);
                            }
                            continue;
                        }
                        if same && block.get("thinkingSignature").is_some_and(raw_truthy) {
                            content.push(block);
                            continue;
                        }
                        let Some(thinking) =
                            block.get("thinking").filter(|value| raw_truthy(value))
                        else {
                            continue;
                        };
                        let thinking = thinking
                            .as_js_str()
                            .ok_or_else(|| error("block.thinking.trim is not a function"))?;
                        if !has_non_whitespace(thinking) {
                            continue;
                        }
                        if !same {
                            block = raw_text(thinking.clone());
                        }
                    }
                    Some("text") if !same => {
                        let mut next = JsObject::new();
                        next.insert("type", "text".into());
                        if let Some(text) = block.get("text") {
                            next.insert("text", text.clone());
                        }
                        block = next.into();
                    }
                    Some("toolCall") if !same => {
                        if block.get("thoughtSignature").is_some_and(raw_truthy) {
                            block
                                .as_object_mut()
                                .expect("tool call object")
                                .remove("thoughtSignature");
                        }
                        if let Some(normalize) = normalize {
                            let id = block
                                .get("id")
                                .and_then(JsValue::as_js_str)
                                .ok_or_else(|| error("tool call id must be a string"))?
                                .clone();
                            let source = Message::Raw(RawMessage::new(message.clone()));
                            let source = source_messages
                                .and_then(|messages| messages.get(source_index))
                                .filter(|message| matches!(message, Message::Assistant(_)))
                                .unwrap_or(&source);
                            let next = normalize(&id, model, source);
                            if next != id {
                                ids.insert(id, next.clone());
                                block
                                    .as_object_mut()
                                    .expect("tool call object")
                                    .insert("id", next.into());
                            }
                        }
                    }
                    _ => {}
                }
                // Array.prototype.flatMap flattens an unknown returned array
                // once, even though ordinary typed content blocks are objects.
                if let JsValue::Array(blocks) = block {
                    content.extend(blocks);
                } else {
                    content.push(block);
                }
            }
            message.insert("content", content.into());
        }
        transformed.push(message);
    }
    let mut result = Vec::new();
    let mut pending = Vec::<JsValue>::new();
    let mut existing = Vec::<Option<JsValue>>::new();
    let mut held = Vec::<JsObject>::new();
    let close = |result: &mut Vec<JsObject>,
                 pending: &mut Vec<JsValue>,
                 existing: &mut Vec<Option<JsValue>>,
                 held: &mut Vec<JsObject>| {
        for call in pending.drain(..) {
            let id = call.get("id");
            if !existing
                .iter()
                .any(|other| raw_set_key_equal(id, other.as_ref()))
            {
                let mut tool = JsObject::new();
                tool.insert("role", "toolResult".into());
                if let Some(id) = call.get("id") {
                    tool.insert("toolCallId", id.clone());
                }
                if let Some(name) = call.get("name") {
                    tool.insert("toolName", name.clone());
                }
                tool.insert(
                    "content",
                    vec![raw_text("No result provided".into())].into(),
                );
                tool.insert("isError", true.into());
                tool.insert("timestamp", JsValue::Number(env.now_ms() as f64));
                result.push(tool);
            }
        }
        existing.clear();
        result.append(held);
    };
    for message in transformed {
        match message.get("role").and_then(JsValue::as_str) {
            Some("assistant") => {
                close(&mut result, &mut pending, &mut existing, &mut held);
                if matches!(
                    message.get("stopReason").and_then(JsValue::as_str),
                    Some("error" | "aborted")
                ) {
                    continue;
                }
                pending.clear();
                for block in message
                    .get("content")
                    .and_then(JsValue::as_array)
                    .expect("validated content")
                {
                    let kind = crate::utils::raw_message::property(Some(block), "type").map_err(
                        |error| {
                            JsonConversionError::new(
                                error.as_str().expect("property error text is ASCII"),
                            )
                        },
                    )?;
                    if kind.and_then(JsValue::as_str) == Some("toolCall") {
                        pending.push(block.clone());
                    }
                }
            }
            Some("toolResult") => {
                existing.push(message.get("toolCallId").cloned());
            }
            Some("system") if !pending.is_empty() => {
                held.push(message);
                continue;
            }
            Some("user") => close(&mut result, &mut pending, &mut existing, &mut held),
            _ => {}
        }
        result.push(message);
    }
    close(&mut result, &mut pending, &mut existing, &mut held);
    Ok(result
        .into_iter()
        .map(|message| Message::Raw(RawMessage::new(message)))
        .collect())
}
// JSON-imported object-valued IDs are distinct JavaScript references. Primitive
// keys use Set's SameValueZero semantics, including undefined and NaN.
fn raw_set_key_equal(left: Option<&JsValue>, right: Option<&JsValue>) -> bool {
    match (left, right) {
        (None, None) | (Some(JsValue::Null), Some(JsValue::Null)) => true,
        (Some(JsValue::Number(a)), Some(JsValue::Number(b))) => {
            a == b || (a.is_nan() && b.is_nan())
        }
        (Some(JsValue::Bool(a)), Some(JsValue::Bool(b))) => a == b,
        (Some(JsValue::String(a)), Some(JsValue::String(b))) => a == b,
        _ => false,
    }
}
fn raw_string_iterator(text: &JsString) -> Vec<JsValue> {
    let mut units = text.units().peekable();
    let mut output = Vec::new();
    while let Some(first) = units.next() {
        let mut next = vec![first];
        if (0xd800..=0xdbff).contains(&first)
            && units
                .peek()
                .is_some_and(|last| (0xdc00..=0xdfff).contains(last))
        {
            next.push(units.next().expect("peeked surrogate"));
        }
        output.push(JsString::from_utf16(next).into());
    }
    output
}
fn raw_text(text: JsString) -> JsValue {
    JsObject::from_iter([("type", "text".into()), ("text", text.into())]).into()
}
fn raw_truthy(value: &JsValue) -> bool {
    match value {
        JsValue::Null => false,
        JsValue::Bool(value) => *value,
        JsValue::Number(value) => *value != 0.0 && !value.is_nan(),
        JsValue::String(value) => !value.is_empty(),
        _ => true,
    }
}

#[cfg(test)]
mod raw_history_regressions {
    use super::*;
    use crate::utils::{js_value::to_js_value, json_parse::parse_json};
    #[test]
    fn raw_image_downgrade_finishes_before_any_id_callback() {
        let input = parse_json(r#"[{"role":"assistant","content":[{"type":"toolCall","id":"one","name":"read","arguments":{}}]},{"role":"user","content":[null]}]"#).unwrap();
        let calls = std::cell::Cell::new(0);
        let normalize = |id: &JsString, _: &Model, _: &Message| {
            calls.set(calls.get() + 1);
            id.clone()
        };
        let error = transform_messages_raw(
            &input,
            &Model::default(),
            Some(&normalize),
            &crate::env::SystemEnv::default(),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Cannot read properties of null (reading 'type')"
        );
        assert_eq!(calls.get(), 0);
    }
    #[test]
    fn mixed_raw_history_preserves_shared_tool_arguments_and_callback_source() {
        let model = Model {
            id: "new".into(),
            ..Model::default()
        };
        let call = ToolCall::new("one", "read", parse_json(r#"{"value":1}"#).unwrap());
        let shared = call.arguments.clone();
        let assistant = AssistantMessage {
            content: vec![call.into()],
            model: "old".into(),
            stop_reason: StopReason::ToolUse,
            ..Default::default()
        };
        let raw = RawMessage::new(
            parse_json(r#"{"role":"user","content":"next","legacy":true}"#)
                .unwrap()
                .as_object()
                .unwrap()
                .clone(),
        );
        let normalize = |id: &JsString, _: &Model, source: &Message| {
            let Message::Assistant(source) = source else {
                panic!("typed callback source lost")
            };
            let AssistantContent::ToolCall(call) = &source.content[0] else {
                unreachable!()
            };
            assert!(call.arguments.ptr_eq(&shared));
            call.arguments.update(|value| {
                value.as_object_mut().unwrap().insert("value", 2.0.into());
            });
            id.clone()
        };
        let result = transform_messages(
            &[assistant.into(), Message::Raw(raw)],
            &model,
            Some(&normalize),
            &crate::env::SystemEnv::default(),
        )
        .unwrap();
        let Message::Assistant(assistant) = &result[0] else {
            panic!("typed assistant lost")
        };
        let AssistantContent::ToolCall(call) = &assistant.content[0] else {
            unreachable!()
        };
        assert!(call.arguments.ptr_eq(&shared));
        assert_eq!(call.arguments.snapshot()["value"], JsValue::Number(2.0));
    }
    #[test]
    fn primitive_tool_accounting_uses_same_value_zero() {
        assert!(raw_set_key_equal(
            Some(&JsValue::Number(f64::NAN)),
            Some(&JsValue::Number(f64::NAN))
        ));
        assert!(raw_set_key_equal(
            Some(&JsValue::Number(-0.0)),
            Some(&JsValue::Number(0.0))
        ));
        assert!(!raw_set_key_equal(None, Some(&JsValue::Null)));
        assert!(!raw_set_key_equal(
            Some(&JsValue::Array(vec![])),
            Some(&JsValue::Array(vec![]))
        ));
    }
    #[test]
    fn raw_transform_preserves_unknown_fields_and_repairs_only_at_boundary() {
        let env = crate::env::SystemEnv::default();
        let model = Model {
            provider: "fixture".into(),
            api: "openai-completions".into(),
            id: "fixture".into(),
            ..Model::default()
        };
        let raw = parse_json(r#"[{"role":"user","timestamp":1,"content":null,"legacy":{"key":"\ud800"}},{"role":"assistant","provider":"fixture","api":"openai-completions","model":"fixture","content":[{"type":"text","text":"answer","unknown":{"nested":true}},{"type":"extension","payload":[1,2]}],"legacyField":null,"stopReason":"stop"}]"#).unwrap();
        let output = transform_messages_raw(&raw, &model, None, &env).unwrap();
        let actual = to_js_value(&output).unwrap();
        let mut expected = raw.clone();
        expected.as_array_mut().unwrap()[0]
            .as_object_mut()
            .unwrap()
            .insert("content", JsValue::Array(Vec::new()));
        assert_eq!(actual, expected);
        assert!(raw.as_array().unwrap()[0]["content"].is_null());
    }
}
