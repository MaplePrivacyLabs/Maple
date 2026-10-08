//! Pi v1.0.4 `api/constrained-sampling.ts`.
use crate::{
    api::transform_messages::has_non_whitespace,
    types::*,
    utils::js_json::{ordered_js_keys, quote, stringify},
};
use indexmap::IndexMap;
pub type UnsupportedStrictSchemaKeywordCheck<'a> = dyn Fn(&JsString, &JsValue) -> bool + 'a;
const UNSUPPORTED_KEYS: &[&str] = &[
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
fn structured(schema: &JsValue) -> bool {
    let Some(schema) = schema.as_object() else {
        return false;
    };
    let kinds = schema.get("type");
    kinds.is_some_and(|v| {
        matches!(v.as_str(), Some("object" | "array"))
            || v.as_array().is_some_and(|a| {
                a.iter()
                    .any(|v| matches!(v.as_str(), Some("object" | "array")))
            })
    }) || schema.contains_key("properties")
        || schema.contains_key("items")
}
fn allows_null(schema: &JsValue) -> bool {
    let Some(schema) = schema.as_object() else {
        return false;
    };
    schema.get("type").is_some_and(|v| {
        v.as_str() == Some("null")
            || v.as_array()
                .is_some_and(|a| a.iter().any(|v| v.as_str() == Some("null")))
    }) || schema.get("const") == Some(&JsValue::Null)
        || schema
            .get("enum")
            .and_then(JsValue::as_array)
            .is_some_and(|a| a.contains(&JsValue::Null))
        || schema
            .get("anyOf")
            .and_then(JsValue::as_array)
            .is_some_and(|a| a.iter().any(allows_null))
}
fn strict_node(
    schema: &mut JsValue,
    unsupported: Option<&UnsupportedStrictSchemaKeywordCheck<'_>>,
) -> Result<(), JsString> {
    let Some(object) = schema.as_object_mut() else {
        return Err("boolean schemas are unsupported".into());
    };
    for key in UNSUPPORTED_KEYS {
        if object.contains_key(*key) {
            return Err(format!("{key} schemas are unsupported").into());
        }
    }
    if let Some(unsupported) = unsupported {
        for key in ordered_js_keys(object.keys()) {
            let value = &object[key];
            if unsupported(key, value) {
                let mut error = key.clone();
                error.push_str(&format!(": {} is unsupported", stringify(value)));
                return Err(error);
            }
        }
    }
    if let Some(any) = object.get_mut("anyOf") {
        let Some(any) = any.as_array_mut().filter(|a| !a.is_empty()) else {
            return Err("anyOf must contain at least one schema".into());
        };
        for variant in any {
            if structured(variant) {
                return Err("object and array unions are unsupported".into());
            }
            strict_node(variant, unsupported)?;
        }
    }
    if let Some(items) = object.get_mut("items") {
        if items.is_array() {
            return Err("tuple schemas are unsupported".into());
        }
        strict_node(items, unsupported)?;
    }
    let is_object = object.get("type").and_then(JsValue::as_str) == Some("object");
    if object.contains_key("properties") && !is_object {
        return Err("properties require type object".into());
    }
    if !is_object {
        return Ok(());
    }
    if object
        .get("additionalProperties")
        .is_some_and(|v| v != &JsValue::Bool(false))
    {
        return Err("schema-valued or true additionalProperties is unsupported".into());
    }
    if object.get("properties").is_some_and(|v| !v.is_object()) {
        return Err("object properties must be a schema map".into());
    }
    if object.get("required").is_some_and(|v| {
        v.as_array()
            .is_none_or(|a| a.iter().any(|v| !v.is_string()))
    }) {
        return Err("object required must be a string array".into());
    }
    let names: Vec<JsString> = object
        .get("properties")
        .and_then(JsValue::as_object)
        .map(|p| ordered_js_keys(p.keys()).into_iter().cloned().collect())
        .unwrap_or_default();
    let required = object
        .get("required")
        .and_then(JsValue::as_array)
        .cloned()
        .unwrap_or_default();
    if required
        .iter()
        .any(|key| !names.iter().any(|name| Some(name) == key.as_js_str()))
    {
        return Err("required contains an unknown property".into());
    }
    if let Some(properties) = object
        .get_mut("properties")
        .and_then(JsValue::as_object_mut)
    {
        for key in &names {
            let property = properties.get_mut(key).expect("collected property exists");
            strict_node(property, unsupported)?;
            if !required.contains(&JsValue::String(key.clone())) && !allows_null(property) {
                *property = JsObject::from_iter([(
                    "anyOf",
                    JsValue::Array(vec![
                        property.take(),
                        JsObject::from_iter([("type", "null".into())]).into(),
                    ]),
                )])
                .into();
            }
        }
    }
    object.insert(
        "required",
        JsValue::Array(names.into_iter().map(JsValue::String).collect()),
    );
    object.insert("additionalProperties", JsValue::Bool(false));
    Ok(())
}
pub fn make_strict_json_schema(
    schema: &Schema,
    unsupported: Option<&UnsupportedStrictSchemaKeywordCheck<'_>>,
) -> Result<JsValue, JsString> {
    let mut cloned = schema.schema.clone();
    if !cloned.is_object() {
        return Err("root schema must have type object".into());
    }
    strict_node(&mut cloned, unsupported)?;
    if cloned.get("type").and_then(JsValue::as_str) != Some("object") {
        return Err("root schema must have type object".into());
    }
    Ok(cloned)
}
pub fn get_json_schema_tool_parameters(
    tool: &Tool,
    strict: Option<bool>,
) -> Result<JsValue, JsString> {
    if strict == Some(true) {
        make_strict_json_schema(&tool.parameters, None)
    } else {
        Ok(tool.parameters.schema.clone())
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct GrammarConstrainedSampling {
    pub format: &'static str,
    pub definition: JsString,
    pub input_property: JsString,
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GrammarToolInputJsonBuffer {
    pub input: JsString,
    pub started: bool,
    pub closed: bool,
}
pub fn get_grammar_tool_input(
    tool_name: &JsString,
    arguments: &JsValue,
    input_property: &JsString,
) -> Result<JsString, JsString> {
    arguments
        .get(input_property)
        .and_then(JsValue::as_js_str)
        .cloned()
        .ok_or_else(|| {
            let mut error = JsString::from("Grammar tool call \"");
            error.push(tool_name);
            error.push_str("\" requires argument \"");
            error.push(input_property);
            error.push_str("\" to be a string.");
            error
        })
}
pub fn append_grammar_tool_input_json_delta(
    buffer: &mut GrammarToolInputJsonBuffer,
    property: &JsString,
    next_input: &JsString,
    close: bool,
) -> Result<Option<JsString>, JsString> {
    let failure = |suffix: &str| {
        let mut error = JsString::from("grammar tool input for property \"");
        error.push(property);
        error.push_str(suffix);
        error
    };
    if buffer.closed {
        if close && next_input == &buffer.input {
            return Ok(None);
        }
        return Err(failure("\" changed after it was closed"));
    }
    let previous = buffer.input.as_utf16();
    let next = next_input.as_utf16();
    if !next.starts_with(&previous) {
        return Err(failure("\" changed non-monotonically"));
    }
    let input_delta = JsString::from_utf16(next[previous.len()..].to_vec());
    if !close && input_delta.is_empty() {
        return Ok(None);
    }
    let mut delta = String::new();
    if !buffer.started {
        delta.push('{');
        delta.push_str(&quote(property));
        delta.push_str(":\"");
        buffer.started = true;
    }
    let escaped = quote(&input_delta);
    delta.push_str(&escaped[1..escaped.len() - 1]);
    buffer.input = next_input.clone();
    if close {
        delta.push_str("\"}");
        buffer.closed = true;
    }
    Ok(Some(delta.into()))
}
fn infer_grammar_input_property(tool: &Tool) -> Result<JsString, JsString> {
    let schema = &tool.parameters.schema;
    if schema.get("type").and_then(JsValue::as_str) != Some("object") {
        return Err("grammar constrained sampling requires an object parameter schema".into());
    }
    let required = schema
        .get("required")
        .and_then(JsValue::as_array)
        .filter(|a| a.len() == 1)
        .and_then(|a| a[0].as_js_str())
        .ok_or("grammar constrained sampling requires exactly one required string property")?;
    let property = schema
        .get("properties")
        .and_then(|p| p.get(required))
        .filter(|v| match v {
            JsValue::Null => false,
            JsValue::Bool(value) => *value,
            JsValue::Number(value) => *value != 0.0 && !value.is_nan(),
            JsValue::String(value) => !value.is_empty(),
            _ => true,
        })
        .ok_or_else(|| {
            let mut error =
                JsString::from("grammar constrained sampling requires a properties entry for ");
            error.push(required);
            error
        })?;
    if property.get("type").and_then(JsValue::as_str) != Some("string") {
        return Err({
            let mut error = JsString::from("grammar constrained sampling property ");
            error.push(required);
            error.push_str(" must have type string");
            error
        });
    }
    Ok(required.into())
}
pub fn resolve_json_schema_strict_sampling(
    tool: &Tool,
    supported: bool,
    unsupported: Option<&UnsupportedStrictSchemaKeywordCheck<'_>>,
) -> Result<Option<bool>, JsString> {
    let Some(ConstrainedSampling::Config(config)) = &tool.constrained_sampling else {
        return Ok(None);
    };
    let Some(ConstrainedSamplingView::JsonSchema { strict }) = config.view() else {
        return Ok(None);
    };
    if supported {
        match make_strict_json_schema(&tool.parameters, unsupported) {
            Ok(_) => return Ok(Some(true)),
            Err(error) if strict == StrictPreference::Require => {
                return Err(tool_detail_error(
                    tool,
                    " requires JSON-schema constrained sampling, but ",
                    &error,
                ));
            }
            Err(_) => {}
        }
    } else if strict == StrictPreference::Require {
        return Err(tool_error(
            tool,
            " requires JSON-schema constrained sampling, but strict tools are unsupported.",
        ));
    }
    Ok(None)
}
pub fn resolve_grammar_constrained_sampling(
    tool: &Tool,
    supported: bool,
) -> Result<Option<GrammarConstrainedSampling>, JsString> {
    let Some(ConstrainedSampling::Config(config)) = &tool.constrained_sampling else {
        return Ok(None);
    };
    if !matches!(config.view(), Some(ConstrainedSamplingView::Grammar { .. })) || !supported {
        return Ok(None);
    }
    let variant = config
        .grammar_variant(GrammarFormat::OpenaiLark)
        .filter(|s| has_non_whitespace(s))
        .map(|s| ("lark", s))
        .or_else(|| {
            config
                .grammar_variant(GrammarFormat::OpenaiRegex)
                .filter(|s| has_non_whitespace(s))
                .map(|s| ("regex", s))
        });
    let Some((format, definition)) = variant else {
        return Err(tool_error(
            tool,
            " cannot use grammar constrained sampling: no supported grammar variant was provided.",
        ));
    };
    let input_property = infer_grammar_input_property(tool).map_err(|error| {
        tool_detail_error(tool, " cannot use grammar constrained sampling: ", &error)
    })?;
    Ok(Some(GrammarConstrainedSampling {
        format,
        definition: definition.clone(),
        input_property,
    }))
}
pub fn create_grammar_tool_input_properties(
    tools: &[Tool],
    supported: bool,
) -> Result<IndexMap<JsString, JsString>, JsString> {
    let mut properties = IndexMap::new();
    for tool in tools {
        if let Some(grammar) = resolve_grammar_constrained_sampling(tool, supported)? {
            properties.insert(tool.name.clone(), grammar.input_property);
        }
    }
    Ok(properties)
}

fn tool_error(tool: &Tool, suffix: &str) -> JsString {
    let mut error = JsString::from("Tool \"");
    error.push(&tool.name);
    error.push_str("\"");
    error.push_str(suffix);
    error
}

fn tool_detail_error(tool: &Tool, prefix: &str, detail: &JsString) -> JsString {
    let mut error = tool_error(tool, prefix);
    error.push(detail);
    error.push_str(".");
    error
}

#[cfg(test)]
mod grammar_schema_regressions {
    use super::*;
    #[test]
    fn falsy_property_definitions_keep_pis_missing_entry_error() {
        for property in [
            JsValue::Number(0.0),
            JsValue::String(JsString::default()),
            JsValue::Number(f64::NAN),
            JsValue::Bool(false),
            JsValue::Null,
        ] {
            let schema = JsObject::from_iter([
                ("type", "object".into()),
                (
                    "properties",
                    JsObject::from_iter([("payload", property)]).into(),
                ),
                ("required", JsValue::Array(vec!["payload".into()])),
            ]);
            let tool = Tool {
                name: "sample_tool".into(),
                parameters: Schema::json_schema(JsValue::Object(schema)),
                constrained_sampling: Some(ConstrainedSampling::Config(
                    ConstrainedSamplingConfig::grammar(IndexMap::from([(
                        GrammarFormat::OpenaiLark,
                        "start: /.+/".into(),
                    )])),
                )),
                ..Default::default()
            };
            assert_eq!(
                resolve_grammar_constrained_sampling(&tool, true).unwrap_err(),
                JsString::from(
                    "Tool \"sample_tool\" cannot use grammar constrained sampling: grammar constrained sampling requires a properties entry for payload."
                )
            );
        }
    }
}
