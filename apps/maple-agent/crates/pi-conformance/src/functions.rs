//! Function replay observes values through the same JSON boundary as the
//! TypeScript recorder while retaining escaped lone UTF-16 code units.

use crate::{CheckResult, read};
use pi_ai::{
    types::{AssistantMessage, Tool, ToolCall},
    utils::{
        estimate, js_json,
        js_value::{JsObject, JsString, JsValue, from_js_value, to_js_value},
        json_parse, overflow, retry, sanitize_unicode, transcript, validation,
    },
};
use std::path::Path;

pub struct Golden {
    pub case: String,
    pub input: JsValue,
    pub result: Result<JsValue, JsString>,
}

pub fn read_goldens(path: &Path) -> CheckResult<Vec<Golden>> {
    read(path)?
        .lines()
        .enumerate()
        .map(|(index, line)| {
            let value = json_parse::parse_json(line)
                .map_err(|error| format!("{}:{}: {error}", path.display(), index + 1))?;
            parse_golden(value)
                .map_err(|error| format!("{}:{}: {error}", path.display(), index + 1))
        })
        .collect()
}

pub fn parse_golden(value: JsValue) -> CheckResult<Golden> {
    let object = value.as_object().ok_or("golden row must be an object")?;
    if object
        .keys()
        .any(|key| !matches!(key.as_str(), Some("case" | "input" | "output" | "error")))
    {
        return Err("golden row has an unknown field".into());
    }
    let case = object
        .get("case")
        .and_then(JsValue::as_str)
        .ok_or("golden case must be a string")?
        .to_owned();
    let input = object.get("input").ok_or("golden input is absent")?.clone();
    let result = match (object.get("output"), object.get("error")) {
        (Some(output), None) => Ok(output.clone()),
        (None, Some(JsValue::String(error))) => Err(error.clone()),
        _ => return Err("golden row must record exactly one of output or string error".into()),
    };
    Ok(Golden {
        case,
        input,
        result,
    })
}

fn typed<T: serde::de::DeserializeOwned>(value: &JsValue) -> CheckResult<T> {
    from_js_value(value.clone()).map_err(|error| error.to_string())
}

fn number(value: &JsValue, key: &str) -> CheckResult<f64> {
    value[key]
        .as_f64()
        .ok_or_else(|| format!("{key} must be a number"))
}

fn string(value: &JsValue, key: &str) -> CheckResult<JsString> {
    match &value[key] {
        JsValue::String(value) => Ok(value.clone()),
        _ => Err(format!("{key} must be a string")),
    }
}

fn utf16(value: &JsValue) -> CheckResult<JsString> {
    let units = value
        .as_array()
        .ok_or("utf16 must be an array")?
        .iter()
        .map(|unit| {
            unit.as_u64()
                .and_then(|unit| u16::try_from(unit).ok())
                .ok_or_else(|| "invalid UTF-16 unit".to_owned())
        })
        .collect::<CheckResult<Vec<_>>>()?;
    Ok(JsString::from_utf16(units))
}

fn raw_or_value(input: &JsValue, key: &str, raw_key: &str) -> CheckResult<JsValue> {
    match input.get(raw_key) {
        Some(JsValue::String(raw)) => {
            json_parse::parse_json_utf16(raw).map_err(|error| error.to_string())
        }
        Some(_) => Err(format!("{raw_key} must be a string")),
        None => input
            .get(key)
            .cloned()
            .ok_or_else(|| format!("{key} is absent")),
    }
}

pub fn dispatch(id: &str, input: &JsValue) -> CheckResult<Result<JsValue, JsString>> {
    let output = match id {
        "json-parse.parseStreamingJson" => {
            let text = input.get("utf16").map(utf16).transpose()?;
            if let Some(text) = text {
                json_parse::parse_streaming_json_utf16(Some(&text))
            } else {
                json_parse::parse_streaming_json(input.get("partialJson").and_then(JsValue::as_str))
            }
        }
        "estimate.estimateTextTokens" => {
            JsValue::Number(estimate::estimate_text_tokens(&string(input, "text")?))
        }
        "sanitize-unicode.sanitizeSurrogates" => {
            JsValue::String(sanitize_unicode::sanitize_surrogates(&utf16(&input["utf16"])?).into())
        }
        "json.stringify" => {
            let value = if let Some(units) = input.get("utf16") {
                JsValue::String(utf16(units)?)
            } else if let Some(entries) = input.get("entries") {
                let mut object = JsObject::new();
                for entry in entries.as_array().ok_or("entries must be an array")? {
                    let entry = entry
                        .as_array()
                        .filter(|entry| entry.len() == 2)
                        .ok_or("invalid object entry")?;
                    let JsValue::String(key) = &entry[0] else {
                        return Err("object key must be a string".into());
                    };
                    object.insert(key.clone(), entry[1].clone());
                }
                JsValue::Object(object)
            } else if let Some(text) = input.get("number") {
                JsValue::Number(
                    text.as_str()
                        .ok_or("number lexeme must be a string")?
                        .parse::<f64>()
                        .map_err(|error| error.to_string())?,
                )
            } else {
                input
                    .get("value")
                    .cloned()
                    .ok_or("stringify value is absent")?
            };
            match input.get("space") {
                Some(JsValue::String(space)) => {
                    JsValue::String(js_json::stringify_with_space(&value, space))
                }
                _ => {
                    let spaces = input.get("space").and_then(JsValue::as_u64).unwrap_or(0) as usize;
                    JsValue::String(js_json::stringify_pretty(&value, spaces).into())
                }
            }
        }
        "validation.validateToolArguments" => {
            let mut tool: Tool = typed(&input["tool"])?;
            if let Some(JsValue::String(raw)) = input.get("schemaJson") {
                tool.parameters.schema =
                    json_parse::parse_json_utf16(raw).map_err(|error| error.to_string())?;
            }
            if let Some(kinds) = input.get("schemaKinds") {
                tool.parameters.typebox_kinds = typed(kinds)?;
            }
            tool.parameters.is_typebox = input
                .get("legacySchemaSymbol")
                .and_then(JsValue::as_bool)
                .unwrap_or(false);
            let mut call: ToolCall = typed(&input["toolCall"])?;
            if let Some(raw) = input.get("argumentsJson") {
                let JsValue::String(raw) = raw else {
                    return Err("argumentsJson must be a string".into());
                };
                call.arguments = json_parse::parse_json_utf16(raw)
                    .map_err(|error| error.to_string())?
                    .into();
            }
            return Ok(validation::validate_tool_arguments(&tool, &call)
                .map_err(|error| error.into_message()));
        }
        "retry.retryDelayMs" => {
            let policy = retry::RetryPolicy {
                enabled: true,
                max_retries: 0.0,
                base_delay_ms: number(&input["policy"], "baseDelayMs")?,
                max_agent_delay_ms: input["policy"]
                    .get("maxAgentDelayMs")
                    .and_then(JsValue::as_f64),
            };
            JsValue::Number(retry::retry_delay_ms(&policy, number(input, "attempt")?))
        }
        "overflow.isContextOverflow" => {
            let message: AssistantMessage = typed(&input["message"])?;
            JsValue::Bool(overflow::is_context_overflow(
                &message,
                input.get("contextWindow").and_then(JsValue::as_f64),
            ))
        }
        "overflow.isRecoverableLength" => {
            let message: AssistantMessage = typed(&input["message"])?;
            JsValue::Bool(overflow::is_recoverable_length(
                &message,
                number(input, "desiredMaxOutput")?,
            ))
        }
        "retry.isRetryableAssistantError" => {
            let message: AssistantMessage = typed(&input["message"])?;
            JsValue::Bool(retry::is_retryable_assistant_error(&message))
        }
        "transcript.toolOwnership" => {
            let mut tool: Tool = typed(&input["tool"])?;
            let message =
                transcript::create_initial_system_message(None, Some(std::slice::from_ref(&tool)))
                    .ok_or("ownership fixture must create a system message")?;
            let before = to_js_value(&message).map_err(|error| error.to_string())?;
            match input["operation"].as_str() {
                Some("mutateSourceTool") => tool.description = string(input, "description")?,
                Some("mutateReplayedTool") => {
                    let mut tools = transcript::get_current_tools(&[message.clone().into()])
                        .map_err(|error| error.to_string_lossy())?;
                    tools
                        .first_mut()
                        .ok_or("ownership fixture lost its tool")?
                        .description = string(input, "description")?;
                }
                _ => return Err("unknown ownership operation".into()),
            }
            JsValue::Object(JsObject::from([
                ("before", before),
                (
                    "after",
                    to_js_value(&message).map_err(|error| error.to_string())?,
                ),
            ]))
        }
        "transcript.toolStateChanges" => {
            let previous: Vec<Tool> = typed(&raw_or_value(input, "previous", "previousJson")?)?;
            let current: Vec<Tool> = typed(&raw_or_value(input, "current", "currentJson")?)?;
            to_js_value(&transcript::get_tool_state_changes(&previous, &current))
                .map_err(|error| error.to_string())?
        }
        _ => return Err(format!("Rust function dispatcher for {id} is pending")),
    };
    Ok(Ok(output))
}

// Function records are JSON observations, as in recorder snapshot(). In-memory
// NaN/infinity therefore become null at this boundary; lone units stay escaped.
fn observed(value: &JsValue) -> CheckResult<JsValue> {
    json_parse::parse_json(&js_json::stringify(value)).map_err(|error| error.to_string())
}

fn compare_owned_tool_definitions(
    input: &JsValue,
    expected: &JsValue,
    actual: &JsValue,
) -> CheckResult {
    // The exception is executable evidence of the ownership decision, not an
    // ignored value: assert Pi's exact mutation AND Rust's exact retained copy.
    let before = expected.get("before").ok_or("missing ownership baseline")?;
    crate::compare::compare_unmodified(&input["tool"], &before["toolsAdded"][0])?;
    let mut changed = before.clone();
    changed["toolsAdded"][0]["description"] = JsValue::String(string(input, "description")?);
    let source = JsValue::Object(JsObject::from([
        ("before", before.clone()),
        ("after", changed),
    ]));
    crate::compare::compare_unmodified(&source, expected)?;
    let owned = JsValue::Object(JsObject::from([
        ("before", before.clone()),
        ("after", before.clone()),
    ]));
    crate::compare::compare_unmodified(&owned, actual)
}

pub fn replay(root: &Path, id: &str) -> CheckResult {
    let mut failures = Vec::new();
    for golden in read_goldens(&root.join("corpus/functions").join(format!("{id}.jsonl")))? {
        let actual = dispatch(id, &golden.input)?;
        let result = match (golden.result, actual) {
            (Ok(expected), Ok(actual)) => {
                let actual = observed(&actual)?;
                if id == "transcript.toolOwnership" {
                    if !crate::selection::permits_owned_tool_definitions(root)? {
                        return Err(
                            "tool ownership replay requires its recorded owner authorization"
                                .into(),
                        );
                    }
                    compare_owned_tool_definitions(&golden.input, &expected, &actual)
                } else {
                    crate::compare::compare_unmodified(&expected, &actual)
                }
            }
            (Err(expected), Err(actual)) if expected == actual => Ok(()),
            (expected, actual) => Err(format!(
                "output/error differs: expected {expected:?}, actual {actual:?}"
            )),
        };
        if let Err(error) = result {
            failures.push(format!("{}: {error}", golden.case));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::compare_unmodified as compare_value;

    #[test]
    fn escaped_lone_units_and_literal_objects_remain_distinct() {
        let high = json_parse::parse_json(r#""\ud800""#).unwrap();
        let low = json_parse::parse_json(r#""\udc00""#).unwrap();
        assert!(compare_value(&high, &high).is_ok());
        assert!(compare_value(&high, &low).is_err());
        assert!(
            compare_value(
                &high,
                &json_parse::parse_json(r#"{"utf16":[55296]}"#).unwrap()
            )
            .is_err()
        );
        assert_eq!(
            observed(&JsValue::Number(f64::INFINITY)).unwrap(),
            JsValue::Null
        );
        assert_eq!(observed(&high).unwrap(), high);
    }

    #[test]
    fn function_comparison_preserves_missing_null_and_numeric_rules() {
        let parse = |text| json_parse::parse_json(text).unwrap();
        assert!(compare_value(&parse("{}"), &parse(r#"{"x":null}"#)).is_err());
        assert!(compare_value(&parse("1"), &parse("2")).is_err());
        assert!(compare_value(&parse("1.5"), &parse("1.5000000000001")).is_ok());
        assert!(compare_value(&parse("0.000000000001"), &parse("0.000000000002")).is_err());
        assert!(
            parse_golden(parse(
                r#"{"case":"x","input":{},"output":null,"error":"x"}"#
            ))
            .is_err()
        );
    }

    #[test]
    fn ownership_rule_verifies_both_behaviors_without_ignoring_other_changes() {
        let parse = |s| json_parse::parse_json(s).unwrap();
        let input = parse(r#"{"tool":{"description":"before"},"description":"after"}"#);
        let expected = parse(
            r#"{"before":{"toolsAdded":[{"description":"before"}]},"after":{"toolsAdded":[{"description":"after"}]}}"#,
        );
        let actual = parse(
            r#"{"before":{"toolsAdded":[{"description":"before"}]},"after":{"toolsAdded":[{"description":"before"}]}}"#,
        );
        assert!(compare_owned_tool_definitions(&input, &expected, &actual).is_ok());
        assert!(compare_owned_tool_definitions(&input, &expected, &expected).is_err());
        assert!(compare_owned_tool_definitions(&input, &actual, &actual).is_err());
        let mut corrupt = actual.clone();
        corrupt["after"]["extra"] = JsValue::Bool(true);
        assert!(compare_owned_tool_definitions(&input, &expected, &corrupt).is_err());
        corrupt = actual.clone();
        corrupt["before"]["toolsAdded"][0]["description"] = "unrelated".into();
        assert!(compare_owned_tool_definitions(&input, &expected, &corrupt).is_err());
    }
}
