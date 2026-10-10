//! Strict tool schemas, after Pi's constrained sampling: a provider that supports it
//! samples a tool's arguments to fit its schema exactly.
//!
//! Strict mode accepts a subset of JSON Schema. Every property must be required and
//! objects must close, so an optional property becomes a required one that may be
//! `null` (argument validation drops such `null`s again). A schema strict mode cannot
//! express is sent as usual for a tool that only prefers strictness, and fails the
//! request for one that requires it.

use serde_json::{Map, Value, json};

use crate::types::{ConstrainedSampling, StrictSampling, Tool};

/// Keywords strict mode rejects.
const UNSUPPORTED_KEYWORDS: [&str; 16] = [
    "$ref",
    "$defs",
    "definitions",
    "allOf",
    "oneOf",
    "patternProperties",
    "dependentSchemas",
    "dependencies",
    "unevaluatedProperties",
    "propertyNames",
    "contains",
    "prefixItems",
    "not",
    "if",
    "then",
    "else",
];

fn types(schema: &Map<String, Value>) -> Vec<&str> {
    match schema.get("type") {
        Some(Value::String(kind)) => vec![kind.as_str()],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

fn is_structured(schema: &Value) -> bool {
    let Some(schema) = schema.as_object() else {
        return false;
    };
    let kinds = types(schema);
    kinds.contains(&"object")
        || kinds.contains(&"array")
        || schema.contains_key("properties")
        || schema.contains_key("items")
}

fn allows_null(schema: &Value) -> bool {
    let Some(object) = schema.as_object() else {
        return false;
    };
    types(object).contains(&"null")
        || object.get("const") == Some(&Value::Null)
        || object
            .get("enum")
            .and_then(Value::as_array)
            .is_some_and(|values| values.contains(&Value::Null))
        || object
            .get("anyOf")
            .and_then(Value::as_array)
            .is_some_and(|variants| variants.iter().any(allows_null))
}

fn make_strict(schema: &mut Value) -> Result<(), String> {
    let Some(object) = schema.as_object_mut() else {
        return Err("boolean schemas are unsupported".into());
    };
    if let Some(keyword) = UNSUPPORTED_KEYWORDS
        .iter()
        .find(|keyword| object.contains_key(**keyword))
    {
        return Err(format!("{keyword} schemas are unsupported"));
    }
    if let Some(any_of) = object.get_mut("anyOf") {
        let variants = any_of
            .as_array_mut()
            .filter(|variants| !variants.is_empty())
            .ok_or("anyOf must contain at least one schema")?;
        for variant in variants {
            if is_structured(variant) {
                return Err("object and array unions are unsupported".into());
            }
            make_strict(variant)?;
        }
    }
    if let Some(items) = object.get_mut("items") {
        if items.is_array() {
            return Err("tuple schemas are unsupported".into());
        }
        make_strict(items)?;
    }

    let is_object = object.get("type").and_then(Value::as_str) == Some("object");
    if object.contains_key("properties") && !is_object {
        return Err("properties require type object".into());
    }
    if !is_object {
        return Ok(());
    }
    if object
        .get("additionalProperties")
        .is_some_and(|additional| additional != &Value::Bool(false))
    {
        return Err("schema-valued or true additionalProperties is unsupported".into());
    }
    let required: Vec<String> = match object.get("required") {
        None => Vec::new(),
        Some(Value::Array(names)) => names
            .iter()
            .map(|name| name.as_str().map(str::to_string))
            .collect::<Option<_>>()
            .ok_or("object required must be a string array")?,
        Some(_) => return Err("object required must be a string array".into()),
    };
    let mut properties = match object.remove("properties") {
        None => Map::new(),
        Some(Value::Object(properties)) => properties,
        Some(_) => return Err("object properties must be a schema map".into()),
    };
    if required.iter().any(|name| !properties.contains_key(name)) {
        return Err("required contains an unknown property".into());
    }
    for (name, property) in properties.iter_mut() {
        make_strict(property)?;
        if !required.contains(name) && !allows_null(property) {
            *property = json!({ "anyOf": [property.take(), { "type": "null" }] });
        }
    }
    let names: Vec<Value> = properties.keys().cloned().map(Value::String).collect();
    object.insert("properties".into(), Value::Object(properties));
    object.insert("required".into(), Value::Array(names));
    object.insert("additionalProperties".into(), Value::Bool(false));
    Ok(())
}

/// `schema` rewritten for strict mode, or why strict mode cannot express it.
pub fn make_strict_json_schema(schema: &Value) -> Result<Value, String> {
    let mut strict = schema.clone();
    if strict.get("type").and_then(Value::as_str) != Some("object") {
        return Err("root schema must have type object".into());
    }
    make_strict(&mut strict)?;
    Ok(strict)
}

/// Whether to send `tool` in strict mode: `Some(true)` when the tool asks for it and the
/// provider and schema allow it, `None` otherwise. A tool that requires strict mode and
/// cannot have it is an error.
pub fn resolve_strict_sampling(
    tool: &Tool,
    supports_strict_mode: bool,
) -> Result<Option<bool>, String> {
    let Some(ConstrainedSampling::JsonSchema { strict }) = tool.constrained_sampling else {
        return Ok(None);
    };
    let reason = if supports_strict_mode {
        match make_strict_json_schema(&tool.parameters) {
            Ok(_) => return Ok(Some(true)),
            Err(reason) => reason,
        }
    } else {
        "strict tools are unsupported".to_string()
    };
    match strict {
        StrictSampling::Prefer => Ok(None),
        StrictSampling::Require => Err(format!(
            "Tool \"{}\" requires JSON-schema constrained sampling, but {reason}.",
            tool.name
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_properties_become_required_and_nullable() {
        let schema = json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "limit": { "type": "number" },
                "label": { "type": ["string", "null"] },
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": { "oldText": { "type": "string" } },
                        "required": ["oldText"]
                    }
                }
            },
            "required": ["path", "edits"]
        });
        let strict = make_strict_json_schema(&schema).unwrap();
        let mut required: Vec<&str> = strict["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        required.sort_unstable();
        assert_eq!(required, ["edits", "label", "limit", "path"]);
        assert_eq!(strict["additionalProperties"], false);
        assert_eq!(strict["properties"]["path"], json!({ "type": "string" }));
        assert_eq!(
            strict["properties"]["limit"],
            json!({ "anyOf": [{ "type": "number" }, { "type": "null" }] })
        );
        // Already nullable: left as it is.
        assert_eq!(
            strict["properties"]["label"],
            json!({ "type": ["string", "null"] })
        );
        assert_eq!(
            strict["properties"]["edits"]["items"]["additionalProperties"],
            false
        );
        // The original is unchanged.
        assert!(schema.get("additionalProperties").is_none());
    }

    #[test]
    fn schemas_strict_mode_cannot_express_are_refused() {
        for (schema, reason) in [
            (
                json!({ "type": "string" }),
                "root schema must have type object",
            ),
            (
                json!({ "type": "object", "properties": { "a": { "$ref": "#/x" } } }),
                "$ref schemas are unsupported",
            ),
            (
                json!({ "type": "object", "additionalProperties": true }),
                "schema-valued or true additionalProperties is unsupported",
            ),
            (
                json!({ "type": "object", "properties": { "a": { "anyOf": [{ "type": "object" }, { "type": "string" }] } } }),
                "object and array unions are unsupported",
            ),
            (
                json!({ "type": "object", "properties": {}, "required": ["missing"] }),
                "required contains an unknown property",
            ),
            (
                json!({ "type": "object", "properties": { "a": { "type": "array", "items": [{ "type": "string" }] } } }),
                "tuple schemas are unsupported",
            ),
        ] {
            assert_eq!(make_strict_json_schema(&schema).unwrap_err(), reason);
        }
    }

    #[test]
    fn strictness_is_preferred_or_required() {
        let schema = json!({ "type": "object", "properties": { "a": { "type": "string" } } });
        let prefer = Tool::new("t", "", schema.clone()).with_constrained_sampling(
            ConstrainedSampling::JsonSchema {
                strict: StrictSampling::Prefer,
            },
        );
        assert_eq!(resolve_strict_sampling(&prefer, true), Ok(Some(true)));
        assert_eq!(resolve_strict_sampling(&prefer, false), Ok(None));
        assert_eq!(
            resolve_strict_sampling(&Tool::new("t", "", schema), true),
            Ok(None)
        );

        let open = json!({ "type": "object", "additionalProperties": true });
        let prefer_open = Tool::new("t", "", open.clone()).with_constrained_sampling(
            ConstrainedSampling::JsonSchema {
                strict: StrictSampling::Prefer,
            },
        );
        assert_eq!(resolve_strict_sampling(&prefer_open, true), Ok(None));
        let require_open =
            Tool::new("t", "", open).with_constrained_sampling(ConstrainedSampling::JsonSchema {
                strict: StrictSampling::Require,
            });
        assert_eq!(
            resolve_strict_sampling(&require_open, true).unwrap_err(),
            "Tool \"t\" requires JSON-schema constrained sampling, but schema-valued or true additionalProperties is unsupported."
        );
        assert_eq!(
            resolve_strict_sampling(&require_open, false).unwrap_err(),
            "Tool \"t\" requires JSON-schema constrained sampling, but strict tools are unsupported."
        );
    }
}
