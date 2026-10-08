use serde_json::{Map, Number, Value};

use crate::types::{Tool, ToolCall};

/// Validate a tool call's arguments against the tool's JSON Schema.
///
/// Models often send `"5"` for a number or `null` for an omitted optional field, so
/// before validating, optional `null`s are dropped and primitives are coerced to the
/// schema's type where the conversion is lossless. Returns the arguments to execute
/// with, or an error message written for the model.
pub fn validate_tool_arguments(tool: &Tool, call: &ToolCall) -> Result<Value, String> {
    let mut args = Value::Object(call.arguments.clone());
    normalize_optional_nulls(&mut args, &tool.parameters);
    coerce(&mut args, &tool.parameters);

    let validator = jsonschema::validator_for(&tool.parameters).map_err(|error| {
        format!(
            "Tool \"{}\" has an invalid parameter schema: {error}",
            tool.name
        )
    })?;
    let errors: Vec<String> = validator
        .iter_errors(&args)
        .map(|error| {
            let path = error
                .instance_path()
                .as_str()
                .trim_start_matches('/')
                .replace('/', ".");
            let path = if path.is_empty() {
                "root".to_string()
            } else {
                path
            };
            format!("  - {path}: {error}")
        })
        .collect();
    if errors.is_empty() {
        return Ok(args);
    }
    let received = serde_json::to_string_pretty(&call.arguments).unwrap_or_default();
    Err(format!(
        "Validation failed for tool \"{}\":\n{}\n\nReceived arguments:\n{received}",
        call.name,
        errors.join("\n")
    ))
}

fn schema_types(schema: &Value) -> Vec<&str> {
    match schema.get("type") {
        Some(Value::String(kind)) => vec![kind.as_str()],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

fn matches_type(value: &Value, kind: &str) -> bool {
    match kind {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.is_i64() || value.is_u64(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    }
}

/// Drop `null` values of optional properties whose schema does not accept `null`.
fn normalize_optional_nulls(value: &mut Value, schema: &Value) {
    let (Value::Object(map), Some(Value::Object(properties))) = (value, schema.get("properties"))
    else {
        return;
    };
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    map.retain(|key, value| {
        !(value.is_null()
            && !required.contains(&key.as_str())
            && properties
                .get(key)
                .is_some_and(|property| !schema_types(property).contains(&"null")))
    });
    for (key, value) in map.iter_mut() {
        if let Some(property) = properties.get(key) {
            normalize_optional_nulls(value, property);
        }
    }
}

fn coerce(value: &mut Value, schema: &Value) {
    let types = schema_types(schema);
    if !types.is_empty()
        && !types.iter().any(|kind| matches_type(value, kind))
        && let Some(converted) = types.iter().find_map(|kind| convert(value, kind))
    {
        *value = converted;
    }
    match value {
        Value::Object(map) => coerce_object(map, schema),
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items").filter(|items| items.is_object()) {
                for item in items {
                    coerce(item, item_schema);
                }
            }
        }
        _ => {}
    }
}

fn coerce_object(map: &mut Map<String, Value>, schema: &Value) {
    let properties = schema.get("properties").and_then(Value::as_object);
    let additional = schema
        .get("additionalProperties")
        .filter(|additional| additional.is_object());
    for (key, value) in map.iter_mut() {
        match properties.and_then(|properties| properties.get(key)) {
            Some(property) => coerce(value, property),
            None => {
                if let Some(additional) = additional {
                    coerce(value, additional);
                }
            }
        }
    }
}

/// A lossless conversion of a primitive to `kind`.
fn convert(value: &Value, kind: &str) -> Option<Value> {
    match (kind, value) {
        ("number", Value::String(text)) => {
            let parsed: f64 = text.trim().parse().ok()?;
            if !parsed.is_finite() {
                return None;
            }
            Some(number_value(parsed))
        }
        ("integer", Value::String(text)) => {
            let parsed: f64 = text.trim().parse().ok()?;
            (parsed.fract() == 0.0 && parsed.is_finite()).then(|| number_value(parsed))
        }
        ("integer", Value::Number(number)) => {
            let parsed = number.as_f64()?;
            (parsed.fract() == 0.0).then(|| number_value(parsed))
        }
        ("number" | "integer", Value::Bool(flag)) => Some(Value::from(u8::from(*flag))),
        ("boolean", Value::String(text)) => match text.as_str() {
            "true" => Some(Value::Bool(true)),
            "false" => Some(Value::Bool(false)),
            _ => None,
        },
        ("boolean", Value::Number(number)) => match number.as_f64() {
            Some(1.0) => Some(Value::Bool(true)),
            Some(0.0) => Some(Value::Bool(false)),
            _ => None,
        },
        ("string", Value::Number(number)) => Some(Value::String(number.to_string())),
        ("string", Value::Bool(flag)) => Some(Value::String(flag.to_string())),
        _ => None,
    }
}

fn number_value(parsed: f64) -> Value {
    if parsed.fract() == 0.0 && parsed.abs() < 9.0e15 {
        Value::from(parsed as i64)
    } else {
        Number::from_f64(parsed)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool() -> Tool {
        Tool::new(
            "read",
            "Read a file",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "offset": { "type": "integer" },
                    "verbose": { "type": "boolean" },
                    "tags": { "type": "array", "items": { "type": "string" } },
                },
                "required": ["path"],
                "additionalProperties": false,
            }),
        )
    }

    fn call(arguments: Value) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: "read".into(),
            arguments: arguments.as_object().cloned().unwrap(),
        }
    }

    #[test]
    fn valid_arguments_pass_through() {
        let args = validate_tool_arguments(&tool(), &call(json!({ "path": "a" }))).unwrap();
        assert_eq!(args, json!({ "path": "a" }));
    }

    #[test]
    fn primitives_are_coerced_to_the_schema_type() {
        let args = validate_tool_arguments(
            &tool(),
            &call(json!({ "path": 7, "offset": "10", "verbose": "true", "tags": [1, true] })),
        )
        .unwrap();
        assert_eq!(
            args,
            json!({ "path": "7", "offset": 10, "verbose": true, "tags": ["1", "true"] })
        );
    }

    #[test]
    fn optional_nulls_are_dropped_but_required_ones_fail() {
        let args = validate_tool_arguments(&tool(), &call(json!({ "path": "a", "offset": null })))
            .unwrap();
        assert_eq!(args, json!({ "path": "a" }));
        assert!(validate_tool_arguments(&tool(), &call(json!({ "path": null }))).is_err());
    }

    #[test]
    fn errors_name_the_tool_the_path_and_the_arguments() {
        let error = validate_tool_arguments(&tool(), &call(json!({ "offset": "x", "extra": 1 })))
            .unwrap_err();
        assert!(
            error.starts_with("Validation failed for tool \"read\":"),
            "{error}"
        );
        assert!(error.contains("  - offset:"), "{error}");
        assert!(error.contains("  - root:"), "{error}");
        assert!(error.contains("Received arguments:"), "{error}");
    }
}
