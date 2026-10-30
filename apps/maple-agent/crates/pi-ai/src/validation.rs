use serde_json::{Map, Number, Value};

use crate::types::{Tool, ToolCall};

/// Validate a tool call's arguments against the tool's JSON Schema.
///
/// Models often send `"5"` for a number or `null` for an omitted optional field, so
/// before validating, optional `null`s the schema rejects are dropped and primitives are
/// coerced to the schema's types where the conversion is lossless, through `allOf`,
/// `anyOf` and `oneOf` too. Returns the arguments to execute with, or an error message
/// written for the model.
pub fn validate_tool_arguments(tool: &Tool, call: &ToolCall) -> Result<Value, String> {
    if let Some(error) = &call.arguments_error {
        return Err(format!(
            "Invalid arguments for tool \"{}\": {error}",
            call.name
        ));
    }
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

/// Whether `value` is valid for `schema` taken on its own. A schema that does not compile
/// alone accepts nothing.
fn is_valid_for(schema: &Value, value: &Value) -> bool {
    jsonschema::validator_for(schema).is_ok_and(|validator| validator.is_valid(value))
}

/// Whether a property may be `null`. A reference, or a schema that does not compile on
/// its own, cannot be checked alone, so it may.
fn accepts_null(schema: &Value) -> bool {
    schema.get("$ref").is_some_and(Value::is_string)
        || jsonschema::validator_for(schema)
            .map_or(true, |validator| validator.is_valid(&Value::Null))
}

/// Drop `null` values of optional properties whose schema does not accept `null`, in
/// nested objects and in arrays too.
fn normalize_optional_nulls(value: &mut Value, schema: &Value) {
    match value {
        Value::Array(items) => match schema.get("items") {
            Some(Value::Array(schemas)) => {
                for (item, schema) in items.iter_mut().zip(schemas) {
                    normalize_optional_nulls(item, schema);
                }
            }
            Some(schema) => {
                for item in items {
                    normalize_optional_nulls(item, schema);
                }
            }
            None => {}
        },
        Value::Object(map) => {
            let Some(Value::Object(properties)) = schema.get("properties") else {
                return;
            };
            let required: Vec<&str> = schema
                .get("required")
                .and_then(Value::as_array)
                .map(|names| names.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            for (key, property) in properties {
                let optional_null = map.get(key).is_some_and(|value| {
                    value.is_null() && !required.contains(&key.as_str()) && !accepts_null(property)
                });
                if optional_null {
                    map.remove(key);
                } else if let Some(value) = map.get_mut(key) {
                    normalize_optional_nulls(value, property);
                }
            }
        }
        _ => {}
    }
}

/// Coerce `value` to `schema`: through every `allOf` schema, to an `anyOf` or `oneOf`
/// branch, to the schema's own types, and into the properties and items of a schema
/// typed as an object or array.
fn coerce(value: &mut Value, schema: &Value) {
    for nested in schema
        .get("allOf")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        coerce(value, nested);
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(branches) = schema.get(key).and_then(Value::as_array) {
            coerce_to_union(value, branches);
        }
    }
    let types = schema_types(schema);
    if !types.is_empty()
        && !types.iter().any(|kind| matches_type(value, kind))
        && let Some(converted) = types.iter().find_map(|kind| convert(value, kind))
    {
        *value = converted;
    }
    match value {
        Value::Object(map) if types.contains(&"object") => coerce_object(map, schema),
        Value::Array(items) if types.contains(&"array") => match schema.get("items") {
            // A tuple: each item has a schema of its own.
            Some(Value::Array(schemas)) => {
                for (item, schema) in items.iter_mut().zip(schemas) {
                    coerce(item, schema);
                }
            }
            Some(schema @ Value::Object(_)) => {
                for item in items {
                    coerce(item, schema);
                }
            }
            _ => {}
        },
        _ => {}
    }
}

/// A value valid for one of the branches stays as it is. Otherwise it takes the first
/// coercion that a branch accepts.
fn coerce_to_union(value: &mut Value, branches: &[Value]) {
    if branches.iter().any(|branch| is_valid_for(branch, value)) {
        return;
    }
    for branch in branches {
        let mut candidate = value.clone();
        coerce(&mut candidate, branch);
        if is_valid_for(branch, &candidate) {
            *value = candidate;
            return;
        }
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

/// Integers above this are not exact as `f64`.
const MAX_EXACT_FLOAT_INTEGER: f64 = 9_007_199_254_740_992.0;

/// Integer text as an exact JSON integer.
fn exact_integer(text: &str) -> Option<Value> {
    text.parse::<i64>()
        .map(Value::from)
        .or_else(|_| text.parse::<u64>().map(Value::from))
        .ok()
}

/// A lossless conversion of a primitive to `kind`. `null` stands for the type's empty
/// value, and an empty value for `null`.
fn convert(value: &Value, kind: &str) -> Option<Value> {
    match (kind, value) {
        ("number" | "integer", Value::Null) => Some(Value::from(0)),
        ("boolean", Value::Null) => Some(Value::Bool(false)),
        ("string", Value::Null) => Some(Value::String(String::new())),
        ("null", Value::String(text)) if text.is_empty() => Some(Value::Null),
        ("null", Value::Number(number)) if number.as_f64() == Some(0.0) => Some(Value::Null),
        ("null", Value::Bool(false)) => Some(Value::Null),
        ("number", Value::String(text)) => exact_integer(text.trim()).or_else(|| {
            let parsed: f64 = text.trim().parse().ok()?;
            parsed.is_finite().then(|| number_value(parsed))
        }),
        ("integer", Value::String(text)) => exact_integer(text.trim()).or_else(|| {
            // Forms like "1e3" or "10.0", only while the float holds the value exactly.
            let parsed: f64 = text.trim().parse().ok()?;
            (parsed.fract() == 0.0 && parsed.abs() < MAX_EXACT_FLOAT_INTEGER)
                .then(|| number_value(parsed))
        }),
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
            arguments_error: None,
            partial_json: None,
        }
    }

    #[test]
    fn valid_arguments_pass_through() {
        let args = validate_tool_arguments(&tool(), &call(json!({ "path": "a" }))).unwrap();
        assert_eq!(args, json!({ "path": "a" }));
    }

    #[test]
    fn malformed_argument_text_fails_with_its_error() {
        let mut malformed = call(json!({}));
        malformed.arguments_error = Some("Unexpected token".into());
        assert_eq!(
            validate_tool_arguments(&tool(), &malformed),
            Err("Invalid arguments for tool \"read\": Unexpected token".into())
        );
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
    fn optional_nulls_are_dropped_and_required_ones_coerced() {
        let args = validate_tool_arguments(&tool(), &call(json!({ "path": "a", "offset": null })))
            .unwrap();
        assert_eq!(args, json!({ "path": "a" }));
        let args = validate_tool_arguments(&tool(), &call(json!({ "path": null }))).unwrap();
        assert_eq!(args, json!({ "path": "" }));
    }

    /// A tool whose one required argument, `value`, has `schema`.
    fn validate_value(schema: Value, value: Value) -> Result<Value, String> {
        let tool = Tool::new(
            "echo",
            "Echo",
            json!({
                "type": "object",
                "properties": { "value": schema },
                "required": ["value"],
            }),
        );
        validate_tool_arguments(&tool, &call(json!({ "value": value })))
            .map(|args| args["value"].clone())
    }

    #[test]
    fn primitives_follow_pis_coercion_rules() {
        for (schema, input, expected) in [
            (json!({ "type": "number" }), json!("42"), json!(42)),
            (json!({ "type": "number" }), json!(true), json!(1)),
            (json!({ "type": "number" }), json!(null), json!(0)),
            (json!({ "type": "integer" }), json!("42"), json!(42)),
            (json!({ "type": "boolean" }), json!("true"), json!(true)),
            (json!({ "type": "boolean" }), json!("false"), json!(false)),
            (json!({ "type": "boolean" }), json!(1), json!(true)),
            (json!({ "type": "boolean" }), json!(0), json!(false)),
            (json!({ "type": "string" }), json!(null), json!("")),
            (json!({ "type": "string" }), json!(true), json!("true")),
            (json!({ "type": "null" }), json!(""), json!(null)),
            (json!({ "type": "null" }), json!(0), json!(null)),
            (json!({ "type": "null" }), json!(false), json!(null)),
            (
                json!({ "type": ["number", "string"] }),
                json!("1"),
                json!("1"),
            ),
            (
                json!({ "type": ["boolean", "number"] }),
                json!("1"),
                json!(1),
            ),
        ] {
            assert_eq!(
                validate_value(schema.clone(), input.clone()),
                Ok(expected),
                "{schema} {input}"
            );
        }
        for (schema, input) in [
            (json!({ "type": "boolean" }), json!("1")),
            (json!({ "type": "boolean" }), json!("0")),
            (json!({ "type": "null" }), json!("null")),
            (json!({ "type": "integer" }), json!("42.1")),
        ] {
            assert!(
                validate_value(schema.clone(), input.clone()).is_err(),
                "{schema} {input}"
            );
        }
    }

    #[test]
    fn values_are_coerced_through_unions() {
        // As MCP servers and pydantic write optional parameters.
        let nullable = json!({ "anyOf": [{ "type": "integer" }, { "type": "null" }] });
        assert_eq!(validate_value(nullable.clone(), json!("5")), Ok(json!(5)));
        // A value that already fits a branch stays as it is.
        assert_eq!(validate_value(nullable, json!(null)), Ok(json!(null)));
        let one_of = json!({ "oneOf": [{ "type": "number" }, { "type": "null" }] });
        assert_eq!(validate_value(one_of, json!(null)), Ok(json!(null)));
        let all_of = json!({ "allOf": [{ "type": "integer" }, { "minimum": 1 }] });
        assert_eq!(validate_value(all_of, json!("3")), Ok(json!(3)));
        let nullable_list = json!({ "type": ["array", "null"], "items": { "type": "string" } });
        assert_eq!(validate_value(nullable_list, json!(null)), Ok(json!(null)));
        // A branch that refers elsewhere cannot be checked alone, so it takes no coercion.
        let referring = Tool::new(
            "pick",
            "Pick",
            json!({
                "type": "object",
                "properties": {
                    "value": { "anyOf": [{ "$ref": "#/$defs/count" }, { "type": "null" }] },
                },
                "$defs": { "count": { "type": "integer" } },
            }),
        );
        assert!(validate_tool_arguments(&referring, &call(json!({ "value": "5" }))).is_err());
        assert_eq!(
            validate_tool_arguments(&referring, &call(json!({ "value": 5 }))),
            Ok(json!({ "value": 5 }))
        );
        // Tuple items, in the draft that writes them as a list.
        let tuple = Tool::new(
            "pair",
            "A pair",
            json!({
                "$schema": "http://json-schema.org/draft-07/schema#",
                "type": "object",
                "properties": {
                    "pair": { "type": "array", "items": [{ "type": "integer" }, { "type": "boolean" }] },
                },
            }),
        );
        assert_eq!(
            validate_tool_arguments(&tuple, &call(json!({ "pair": ["7", "true", "x"] }))),
            Ok(json!({ "pair": [7, true, "x"] }))
        );
    }

    #[test]
    fn optional_nulls_are_dropped_inside_objects_and_arrays() {
        let tool = Tool::new(
            "edit",
            "Edit files",
            json!({
                "type": "object",
                "properties": {
                    "edits": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "text": { "type": "string" },
                                "all": { "type": "boolean" },
                            },
                            "required": ["text"],
                        },
                    },
                    "metadata": {
                        "type": "object",
                        "properties": { "enabled": { "type": "boolean" } },
                    },
                    "note": { "$ref": "#/$defs/note" },
                },
                "required": ["edits"],
                "$defs": { "note": { "anyOf": [{ "type": "string" }, { "type": "null" }] } },
            }),
        );
        let args = validate_tool_arguments(
            &tool,
            &call(json!({
                "edits": [{ "text": "a", "all": null }],
                "metadata": { "enabled": null },
                "note": null,
            })),
        )
        .unwrap();
        // A reference is not resolved alone, so its null stays.
        assert_eq!(
            args,
            json!({ "edits": [{ "text": "a" }], "metadata": {}, "note": null })
        );
    }

    #[test]
    fn nulls_the_schema_allows_are_kept() {
        let tool = Tool::new(
            "assign",
            "Assign an issue",
            json!({
                "type": "object",
                "properties": {
                    "assignee": { "anyOf": [{ "type": "string" }, { "type": "null" }] },
                    "label": { "type": "string" },
                },
            }),
        );
        let args =
            validate_tool_arguments(&tool, &call(json!({ "assignee": null, "label": null })))
                .unwrap();
        assert_eq!(args, json!({ "assignee": null }));
    }

    #[test]
    fn large_integers_keep_every_digit() {
        let tool = Tool::new(
            "fetch",
            "Fetch a record",
            json!({
                "type": "object",
                "properties": { "id": { "type": "integer" }, "size": { "type": "number" } },
            }),
        );
        let args = validate_tool_arguments(
            &tool,
            &call(json!({ "id": "9007199254740993", "size": "18446744073709551615" })),
        )
        .unwrap();
        assert_eq!(
            args,
            json!({ "id": 9_007_199_254_740_993_i64, "size": u64::MAX })
        );
        assert!(validate_tool_arguments(&tool, &call(json!({ "id": "1e300" }))).is_err());
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
