//! Pi tool argument conversion and TypeBox-compatible validation messages.
//!
//! Port of packages/ai/src/utils/validation.ts at Pi v1.0.4. Conversion
//! follows the TypeBox 1.3.27 subset used by Pi; it is intentionally separate
//! from JSON Schema validation, whose coercion rules are different.

use std::{collections::BTreeMap, fmt};

use jsonschema::{Keyword, Validator};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Value, json};

use super::js_json::{js_object_entries, stringify, stringify_pretty};
use super::js_value::{JsObject, JsString, JsValue};
use crate::types::{Tool, ToolCall};

/// JSON Schema plus JavaScript-only metadata used by Pi's conversion branch.
///
/// TypeBox 1.3.27 stores non-enumerable `~kind` strings on individual schemas.
/// Pi separately tests the legacy `Symbol.for("TypeBox.Kind")` on the root.
/// Neither metadata field is emitted in model requests or persisted schemas.
#[derive(Clone, Debug, PartialEq)]
pub struct Schema {
    pub schema: JsValue,
    pub is_typebox: bool,
    pub typebox_kinds: BTreeMap<JsString, String>,
}
impl Default for Schema {
    fn default() -> Self {
        Self::json_schema(json!({}))
    }
}
/// Inputs accepted by schema constructors. serde JSON numbers explicitly use
/// JavaScript binary64 rounding; compatibility values retain their exact domain.
pub trait IntoSchemaValue {
    fn into_schema_value(self) -> JsValue;
}
impl IntoSchemaValue for JsValue {
    fn into_schema_value(self) -> JsValue {
        self
    }
}
impl IntoSchemaValue for Value {
    fn into_schema_value(self) -> JsValue {
        JsValue::from_json_with_js_numbers(self)
    }
}
impl Schema {
    pub fn json_schema(schema: impl IntoSchemaValue) -> Self {
        Self {
            schema: schema.into_schema_value(),
            is_typebox: false,
            typebox_kinds: BTreeMap::new(),
        }
    }
    /// Construct the subset of modern TypeBox types used by Pi. Explicit
    /// metadata may be edited for mixed plain/TypeBox sub-schemas.
    pub fn typebox(schema: impl IntoSchemaValue) -> Self {
        fn visit(schema: &JsValue, path: &JsString, kinds: &mut BTreeMap<JsString, String>) {
            let kind = if schema.get("const").is_some() {
                Some("Literal")
            } else if schema.get("anyOf").is_some() {
                Some("Union")
            } else if schema.get("allOf").is_some() {
                Some("Intersect")
            } else if schema.get("enum").is_some() {
                Some("Enum")
            } else if schema.get("$ref").is_some() {
                Some("Ref")
            } else if schema.get("patternProperties").is_some() {
                Some("Record")
            } else {
                match schema.get("type").and_then(JsValue::as_str) {
                    Some("object") => Some("Object"),
                    Some("array") if schema.get("items").is_some_and(JsValue::is_array) => {
                        Some("Tuple")
                    }
                    Some("array") => Some("Array"),
                    Some("string") => Some("String"),
                    Some("number") => Some("Number"),
                    Some("integer") => Some("Integer"),
                    Some("boolean") => Some("Boolean"),
                    Some("null") => Some("Null"),
                    _ => None,
                }
            };
            if let Some(kind) = kind {
                kinds.insert(path.clone(), kind.to_owned());
            }
            for keyword in ["properties", "patternProperties", "$defs", "definitions"] {
                if let Some(children) = schema.get(keyword).and_then(JsValue::as_object) {
                    for (key, child) in children {
                        visit(
                            child,
                            &child_path(&child_path(path, keyword), pointer_segment_js(key)),
                            kinds,
                        );
                    }
                }
            }
            for keyword in ["anyOf", "allOf", "oneOf", "prefixItems"] {
                if let Some(children) = schema.get(keyword).and_then(JsValue::as_array) {
                    for (index, child) in children.iter().enumerate() {
                        visit(
                            child,
                            &child_path(&child_path(path, keyword), index.to_string()),
                            kinds,
                        );
                    }
                }
            }
            for keyword in ["items", "additionalProperties"] {
                if let Some(child) = schema.get(keyword) {
                    if let Some(items) = child.as_array() {
                        for (index, child) in items.iter().enumerate() {
                            visit(
                                child,
                                &child_path(&child_path(path, keyword), index.to_string()),
                                kinds,
                            );
                        }
                    } else {
                        visit(child, &child_path(path, keyword), kinds);
                    }
                }
            }
        }
        let mut result = Self::json_schema(schema);
        visit(
            &result.schema,
            &JsString::default(),
            &mut result.typebox_kinds,
        );
        result
    }
}
impl From<Value> for Schema {
    fn from(schema: Value) -> Self {
        Self::json_schema(schema)
    }
}
impl From<JsValue> for Schema {
    fn from(schema: JsValue) -> Self {
        Self::json_schema(schema)
    }
}
impl Serialize for Schema {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.schema.serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for Schema {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        JsValue::deserialize(deserializer).map(Self::json_schema)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationError(pub JsString);
impl ValidationError {
    pub fn message(&self) -> &JsString {
        &self.0
    }
    pub fn into_message(self) -> JsString {
        self.0
    }
}
impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.to_string_lossy())
    }
}
impl std::error::Error for ValidationError {}

/// Find the named tool and validate its arguments without mutating the call.
pub fn validate_tool_call(tools: &[Tool], call: &ToolCall) -> Result<JsValue, ValidationError> {
    let tool = tools
        .iter()
        .find(|tool| tool.name == call.name)
        .ok_or_else(|| {
            let mut message = JsString::from("Tool \"");
            message.push(&call.name);
            message.push_str("\" not found");
            ValidationError(message)
        })?;
    validate_tool_arguments(tool, call)
}
/// Validate and coerce a cloned argument object, preserving Pi's error text.
pub fn validate_tool_arguments(tool: &Tool, call: &ToolCall) -> Result<JsValue, ValidationError> {
    let original = call.arguments.snapshot();
    let mut args = original.clone();
    normalize_optional_nulls(&mut args, &tool.parameters.schema);
    // Pi discards Convert's return while retaining mutations of the input.
    apply_typebox_mutations(
        &tool.parameters,
        &JsString::default(),
        &tool.parameters.schema,
        &mut args,
    );
    let validator = compile(&tool.parameters.schema)?;
    if !tool.parameters.is_typebox {
        let coerced = coerce_json_schema(args.clone(), &tool.parameters.schema);
        if !matches!(
            (&args, &coerced),
            (
                JsValue::Object(_) | JsValue::Array(_),
                JsValue::Object(_) | JsValue::Array(_)
            )
        ) && coerced != args
        {
            // Pi returns early for changed primitive roots, including the
            // original primitive when its coerced candidate fails validation.
            return Ok(if check(&tool.parameters.schema, &coerced) {
                coerced
            } else {
                args
            });
        }
        args = coerced;
    }
    let valid = match (validator, args.to_json()) {
        (Some(validator), Ok(value)) => validator.is_valid(&value),
        _ => check_compatible(&tool.parameters.schema, &tool.parameters.schema, &args),
    };
    if valid {
        return Ok(args);
    }
    let mut errors = Vec::new();
    errors_at(
        &tool.parameters.schema,
        &tool.parameters.schema,
        &args,
        &JsString::default(),
        &mut errors,
    );
    let mut message = JsString::from("Validation failed for tool \"");
    message.push(&call.name);
    message.push_str("\":\n");
    if errors.is_empty() {
        message.push_str("Unknown validation error");
    }
    for (index, error) in errors.iter().enumerate() {
        if index > 0 {
            message.push_str("\n");
        }
        message.push(error);
    }
    message.push_str("\n\nReceived arguments:\n");
    message.push_str(&stringify_pretty(&original, 2));
    Err(ValidationError(message))
}

/// jsonschema handles ordinary schemas; exact ECMAScript record patterns and
/// schemas outside serde JSON's domain use the shared TypeBox-compatible walk.
fn compile(schema: &JsValue) -> Result<Option<Validator>, ValidationError> {
    if schema_needs_compatibility(schema)? {
        return Ok(None);
    }
    let Ok(projected) = schema.to_json() else {
        return Ok(None);
    };
    // Match the binary64 integer spellings accepted by schema keyword parsers.
    let projected: Value =
        serde_json::from_str(&stringify(&projected)).expect("strict JSON schema");
    compile_json(&projected).map(Some)
}
fn schema_needs_compatibility(schema: &JsValue) -> Result<bool, ValidationError> {
    if let Some(reference) = schema.get("$ref").and_then(JsValue::as_js_str)
        && reference.units().next() != Some(35)
    {
        let mut message =
            JsString::from("External schema reference unavailable in offline validation: ");
        message.push(reference);
        return Err(ValidationError(message));
    }
    let mut result = schema.get("patternProperties").is_some() || schema.get("$ref").is_some();
    for keyword in [
        "properties",
        "patternProperties",
        "$defs",
        "definitions",
        "dependentSchemas",
    ] {
        if let Some(children) = schema.get(keyword).and_then(JsValue::as_object) {
            for child in children.values() {
                result |= schema_needs_compatibility(child)?;
            }
        }
    }
    for keyword in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if let Some(children) = schema.get(keyword).and_then(JsValue::as_array) {
            for child in children {
                result |= schema_needs_compatibility(child)?;
            }
        }
    }
    for keyword in [
        "items",
        "additionalItems",
        "additionalProperties",
        "contains",
        "propertyNames",
        "not",
        "if",
        "then",
        "else",
        "unevaluatedItems",
        "unevaluatedProperties",
    ] {
        if let Some(child) = schema.get(keyword) {
            if let Some(children) = child.as_array() {
                for child in children {
                    result |= schema_needs_compatibility(child)?;
                }
            } else {
                result |= schema_needs_compatibility(child)?;
            }
        }
    }
    Ok(result)
}
fn compile_json(schema: &Value) -> Result<Validator, ValidationError> {
    // Always install an offline retriever, even when another workspace
    // dependency enables jsonschema's HTTP/file features through unification.
    jsonschema::options()
        .offline()
        .with_draft(jsonschema::Draft::Draft202012)
        .should_validate_formats(true)
        .should_ignore_unknown_formats(true)
        .with_keyword("pattern", |_, value, _| {
            regress::Regex::with_flags(value.as_str().unwrap_or(""), "u")
                .map(|regex| Box::new(PiKeyword::Pattern(regex)) as Box<dyn for<'i> Keyword<'i>>)
                .map_err(|error| jsonschema::ValidationError::custom(error.to_string()))
        })
        .with_keyword("uniqueItems", |_, value, _| {
            Ok(Box::new(PiKeyword::UniqueItems(
                value.as_bool().unwrap_or(false),
            )))
        })
        .with_keyword("minLength", |_, v, _| {
            Ok(Box::new(PiKeyword::MinLength(v.as_u64().unwrap_or(0))))
        })
        .with_keyword("maxLength", |_, v, _| {
            Ok(Box::new(PiKeyword::MaxLength(
                v.as_u64().unwrap_or(u64::MAX),
            )))
        })
        .with_keyword("multipleOf", |_, v, _| {
            Ok(Box::new(PiKeyword::MultipleOf(v.as_f64().unwrap_or(1.0))))
        })
        .build(&schema_for_validator(schema))
        .map_err(|error| ValidationError(error.to_string().into()))
}

// TypeBox accepts the historical tuple `items` form alongside prefixItems.
// Normalize only schema locations, leaving const/enum/default data intact.
fn schema_for_validator(schema: &Value) -> Value {
    let Some(original) = schema.as_object() else {
        return schema.clone();
    };
    let mut schema = original.clone();
    for keyword in [
        "properties",
        "patternProperties",
        "$defs",
        "definitions",
        "dependentSchemas",
    ] {
        if let Some(Value::Object(children)) = schema.get_mut(keyword) {
            for child in children.values_mut() {
                *child = schema_for_validator(child);
            }
        }
    }
    for keyword in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if let Some(Value::Array(children)) = schema.get_mut(keyword) {
            for child in children {
                *child = schema_for_validator(child);
            }
        }
    }
    for keyword in [
        "items",
        "additionalItems",
        "additionalProperties",
        "contains",
        "propertyNames",
        "not",
        "if",
        "then",
        "else",
        "unevaluatedItems",
        "unevaluatedProperties",
    ] {
        if let Some(child) = schema.get_mut(keyword) {
            if let Value::Array(items) = child {
                for item in items {
                    *item = schema_for_validator(item);
                }
            } else {
                *child = schema_for_validator(child);
            }
        }
    }
    if schema.get("items").is_some_and(Value::is_array) {
        if let Some(items) = schema.remove("items") {
            schema.insert("prefixItems".into(), items);
        }
        if let Some(additional) = schema.remove("additionalItems") {
            schema.insert("items".into(), additional);
        }
    }
    Value::Object(schema)
}

#[derive(Debug)]
enum PiKeyword {
    Pattern(regress::Regex),
    UniqueItems(bool),
    MinLength(u64),
    MaxLength(u64),
    MultipleOf(f64),
}
impl<'i> Keyword<'i> for PiKeyword {
    fn validate(&self, value: &'i Value) -> Result<(), jsonschema::ValidationError<'i>> {
        if self.is_valid(value) {
            Ok(())
        } else {
            Err(jsonschema::ValidationError::custom("TypeBox constraint"))
        }
    }
    fn is_valid(&self, value: &'i Value) -> bool {
        match self {
            Self::Pattern(regex) => value.as_str().is_none_or(|text| {
                regex
                    .find_from_utf16(&text.encode_utf16().collect::<Vec<_>>(), 0)
                    .next()
                    .is_some()
            }),
            Self::UniqueItems(enabled) => {
                !*enabled
                    || value.as_array().is_none_or(|items| {
                        let mut hashes = std::collections::HashSet::new();
                        items.iter().all(|item| {
                            hashes.insert(compatible_hash(&JsValue::from_json_with_js_numbers(
                                item.clone(),
                            )))
                        })
                    })
            }
            Self::MinLength(limit) => value.as_str().is_none_or(|s| grapheme_count(s) >= *limit),
            Self::MaxLength(limit) => value.as_str().is_none_or(|s| grapheme_count(s) <= *limit),
            Self::MultipleOf(divisor) => value.as_f64().is_none_or(|number| {
                if number.fract() == 0.0 && (1.0 / divisor).fract() == 0.0 {
                    return true;
                }
                let remainder = number % divisor;
                remainder
                    .abs()
                    .min((remainder - divisor).abs())
                    .min((remainder + divisor).abs())
                    < 1e-10
            }),
        }
    }
}

// TypeBox's own compact grapheme algorithm, not Unicode segmentation (which
// would change its treatment of skin-tone modifiers and several scripts).
fn grapheme_count(value: &str) -> u64 {
    fn modifier(c: char) -> bool {
        matches!(c as u32, 0x0300..=0x036f | 0x1ab0..=0x1aff | 0x1dc0..=0x1dff | 0xfe20..=0xfe2f | 0xfe00..=0xfe0f)
    }
    fn regional(c: char) -> bool {
        matches!(c as u32, 0x1f1e6..=0x1f1ff)
    }
    let mut chars = value.chars().peekable();
    let mut count = 0;
    while let Some(first) = chars.next() {
        count += 1;
        while chars.peek().is_some_and(|c| modifier(*c)) {
            chars.next();
        }
        while chars.peek() == Some(&'\u{200d}') {
            let mut probe = chars.clone();
            probe.next();
            if probe.next().is_none() {
                break;
            }
            chars = probe;
            while chars.peek().is_some_and(|c| modifier(*c)) {
                chars.next();
            }
        }
        if regional(first) && chars.peek().is_some_and(|c| regional(*c)) {
            chars.next();
        }
    }
    count
}

fn check(schema: &JsValue, value: &JsValue) -> bool {
    let Ok(validator) = compile(schema) else {
        return false;
    };
    match (validator, value.to_json()) {
        (Some(validator), Ok(value)) => validator.is_valid(&value),
        _ => check_compatible(schema, schema, value),
    }
}
fn normalize_optional_nulls(value: &mut JsValue, schema: &JsValue) {
    if let Some(items) = value.as_array_mut() {
        match schema.get("items") {
            Some(JsValue::Array(schemas)) => {
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
        }
        return;
    }
    let (Some(object), Some(properties)) = (
        value.as_object_mut(),
        schema.get("properties").and_then(JsValue::as_object),
    ) else {
        return;
    };
    for (key, property) in js_object_entries(properties) {
        let Some(value) = object.get_mut(key) else {
            continue;
        };
        let required = schema
            .get("required")
            .and_then(JsValue::as_array)
            .is_some_and(|items| items.iter().any(|item| item.as_js_str() == Some(key)));
        let nullable = compile(property)
            .ok()
            .map(|_| check(property, &JsValue::Null));
        if value.is_null()
            && !required
            && !property.get("$ref").is_some_and(JsValue::is_string)
            && nullable == Some(false)
        {
            object.shift_remove(key);
        } else {
            normalize_optional_nulls(value, property);
        }
    }
}
fn schema_types(schema: &JsValue) -> Vec<&str> {
    match schema.get("type") {
        Some(JsValue::String(value)) => value.as_str().into_iter().collect(),
        Some(JsValue::Array(values)) => values.iter().filter_map(JsValue::as_str).collect(),
        _ => Vec::new(),
    }
}
// Pi's JSON coercion guard uses typeof number, including nonfinite numbers.
// TypeBox's validator uses Number.isFinite instead; keep them separate.
fn matches_json_type(value: &JsValue, kind: &str) -> bool {
    match kind {
        "number" => value.is_number(),
        "integer" => value
            .as_f64()
            .is_some_and(|n| n.is_finite() && n.fract() == 0.0),
        "boolean" => value.is_boolean(),
        "string" => value.is_string(),
        "null" => value.is_null(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    }
}
fn matches_type(value: &JsValue, kind: &str) -> bool {
    if kind == "number" {
        value.as_f64().is_some_and(f64::is_finite)
    } else {
        matches_json_type(value, kind)
    }
}
fn number_value(number: f64) -> Option<JsValue> {
    number.is_finite().then_some(JsValue::Number(number))
}
fn trim_js(value: &str) -> &str {
    value.trim_matches(|c: char| matches!(c as u32, 0x0009..=0x000d | 0x0020 | 0x00a0 | 0x1680 | 0x2000..=0x200a | 0x2028 | 0x2029 | 0x202f | 0x205f | 0x3000 | 0xfeff))
}
fn js_number(value: &str) -> Option<f64> {
    let value = trim_js(value);
    if value.is_empty() {
        return Some(0.0);
    }
    for (prefix, base) in [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if let Some(digits) = value.strip_prefix(prefix) {
            if digits.is_empty() {
                return None;
            }
            return digits
                .chars()
                .try_fold(0.0, |acc, c| {
                    c.to_digit(base)
                        .map(|digit| acc * f64::from(base) + f64::from(digit))
                })
                .filter(|n| n.is_finite());
        }
    }
    if value.contains(['i', 'I', 'n', 'N']) {
        return None;
    }
    value.parse::<f64>().ok().filter(|n| n.is_finite())
}
fn typebox_bigint_number(value: &str) -> Option<f64> {
    let value = value.strip_suffix('n')?;
    let digits = value.strip_prefix('-').unwrap_or(value);
    if digits.is_empty()
        || (digits.len() > 1 && digits.starts_with('0'))
        || !digits.bytes().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    value
        .parse::<i64>()
        .ok()
        .filter(|n| n.unsigned_abs() <= 9_007_199_254_740_991)
        .map(|n| n as f64)
}
fn js_primitive_string(value: &JsValue) -> JsString {
    match value {
        JsValue::Number(n) if n.is_nan() => "NaN".into(),
        JsValue::Number(n) if *n == f64::INFINITY => "Infinity".into(),
        JsValue::Number(n) if *n == f64::NEG_INFINITY => "-Infinity".into(),
        _ => stringify(value).into(),
    }
}
fn coerce_primitive(value: JsValue, kind: &str) -> JsValue {
    match (kind, &value) {
        ("number" | "integer", JsValue::Null) => JsValue::Number(0.0),
        ("number" | "integer", JsValue::Bool(b)) => JsValue::Number(if *b { 1.0 } else { 0.0 }),
        ("number" | "integer", JsValue::String(text))
            if text.as_str().is_some_and(|s| !trim_js(s).is_empty()) =>
        {
            js_number(text.as_str().unwrap())
                .filter(|n| kind == "number" || n.fract() == 0.0)
                .and_then(number_value)
                .unwrap_or(value)
        }
        ("boolean", JsValue::Null) => JsValue::Bool(false),
        ("boolean", JsValue::String(text)) if text.as_str() == Some("true") => JsValue::Bool(true),
        ("boolean", JsValue::String(text)) if text.as_str() == Some("false") => {
            JsValue::Bool(false)
        }
        ("boolean", JsValue::Number(n)) if *n == 1.0 => JsValue::Bool(true),
        ("boolean", JsValue::Number(n)) if *n == 0.0 => JsValue::Bool(false),
        ("string", JsValue::Null) => JsValue::String("".into()),
        ("string", JsValue::Number(_) | JsValue::Bool(_)) => {
            JsValue::String(js_primitive_string(&value))
        }
        ("null", JsValue::String(text)) if text.utf16_len() == 0 => JsValue::Null,
        ("null", JsValue::Number(n)) if *n == 0.0 => JsValue::Null,
        ("null", JsValue::Bool(false)) => JsValue::Null,
        _ => value,
    }
}
fn coerce_union(value: JsValue, schemas: &[JsValue]) -> JsValue {
    if schemas.iter().any(|schema| check(schema, &value)) {
        return value;
    }
    for schema in schemas {
        let candidate = coerce_json_schema(value.clone(), schema);
        if check(schema, &candidate) {
            return candidate;
        }
    }
    value
}
fn coerce_json_schema(mut value: JsValue, schema: &JsValue) -> JsValue {
    if let Some(schemas) = schema.get("allOf").and_then(JsValue::as_array) {
        for schema in schemas {
            value = coerce_json_schema(value, schema);
        }
    }
    for keyword in ["anyOf", "oneOf"] {
        if let Some(schemas) = schema.get(keyword).and_then(JsValue::as_array) {
            value = coerce_union(value, schemas);
        }
    }
    let types = schema_types(schema);
    if !(types.len() > 1 && types.iter().any(|kind| matches_json_type(&value, kind))) {
        for kind in &types {
            let candidate = coerce_primitive(value.clone(), kind);
            if candidate != value {
                value = candidate;
                break;
            }
        }
    }
    if types.contains(&"object")
        && let Some(object) = value.as_object_mut()
    {
        let properties = schema.get("properties").and_then(JsValue::as_object);
        if let Some(properties) = properties {
            for (key, schema) in js_object_entries(properties) {
                if let Some(value) = object.get_mut(key) {
                    *value = coerce_json_schema(value.clone(), schema);
                }
            }
        }
        if let Some(additional) = schema.get("additionalProperties").filter(|v| v.is_object()) {
            for (key, value) in object.iter_mut() {
                if !properties.is_some_and(|p| p.contains_key(key)) {
                    *value = coerce_json_schema(value.clone(), additional);
                }
            }
        }
    }
    if types.contains(&"array")
        && let Some(values) = value.as_array_mut()
    {
        match schema.get("items") {
            Some(JsValue::Array(schemas)) => {
                for (value, schema) in values.iter_mut().zip(schemas) {
                    *value = coerce_json_schema(value.clone(), schema);
                }
            }
            Some(schema) if schema.is_object() => {
                for value in values {
                    *value = coerce_json_schema(value.clone(), schema);
                }
            }
            _ => {}
        }
    }
    value
}
/// Retain mutations on the original root while discarding Convert's result.
/// Arrays map to a new array but still mutate any referenced object elements;
/// tuples and objects write interior conversions back into their input.
fn apply_typebox_mutations(
    metadata: &Schema,
    path: &JsString,
    schema: &JsValue,
    value: &mut JsValue,
) {
    match metadata.typebox_kinds.get(path).map(String::as_str) {
        Some("Object" | "Record" | "Tuple") => {
            *value = convert_typebox(metadata, path, schema, value.clone())
        }
        Some("Array") => {
            let item_path = child_path(path, "items");
            if let JsValue::Array(items) = value {
                for item in items {
                    apply_typebox_mutations(metadata, &item_path, &schema["items"], item);
                }
            } else {
                apply_typebox_mutations(metadata, &item_path, &schema["items"], value);
            }
        }
        Some("Enum" | "Intersect" | "TemplateLiteral") => {
            let materialized = materialize_typebox(metadata, path, schema);
            let evaluated = schema_from_typebox_nodes(evaluate_typebox(&materialized));
            apply_typebox_mutations(&evaluated, &JsString::default(), &evaluated.schema, value);
        }
        _ => {}
    }
}

fn convert_typebox(
    metadata: &Schema,
    path: &JsString,
    schema: &JsValue,
    value: JsValue,
) -> JsValue {
    let Some(kind) = metadata.typebox_kinds.get(path).map(String::as_str) else {
        return value;
    };
    match kind {
        "Boolean" => match &value {
            JsValue::Null => JsValue::Bool(false),
            JsValue::String(s)
                if s.as_str()
                    .is_some_and(|s| s.eq_ignore_ascii_case("true") || s == "1") =>
            {
                JsValue::Bool(true)
            }
            JsValue::String(s)
                if s.as_str()
                    .is_some_and(|s| s.eq_ignore_ascii_case("false") || s == "0") =>
            {
                JsValue::Bool(false)
            }
            _ => coerce_primitive(value, "boolean"),
        },
        "Number" | "Integer" => {
            let number = match &value {
                JsValue::Null => Some(0.0),
                JsValue::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
                JsValue::Number(n) => n.is_finite().then_some(*n),
                JsValue::String(s) => s.as_str().and_then(|text| {
                    js_number(text).or_else(|| {
                        if text.eq_ignore_ascii_case("true") {
                            Some(1.0)
                        } else if text.eq_ignore_ascii_case("false") {
                            Some(0.0)
                        } else {
                            typebox_bigint_number(text)
                        }
                    })
                }),
                _ => None,
            };
            number
                .and_then(|n| number_value(if kind == "Integer" { n.trunc() } else { n }))
                .unwrap_or(value)
        }
        "String" => match &value {
            JsValue::Null => JsValue::String("null".into()),
            // TypeBox TryString recognizes only finite numbers. Pi's second
            // plain-schema pass handles nonfinite numbers unless symbol-tagged.
            JsValue::Bool(_) => JsValue::String(js_primitive_string(&value)),
            JsValue::Number(n) if n.is_finite() => JsValue::String(js_primitive_string(&value)),
            _ => value,
        },
        "Null" => match &value {
            JsValue::String(s)
                if s.as_str().is_some_and(|s| {
                    s.eq_ignore_ascii_case("undefined")
                        || s.eq_ignore_ascii_case("null")
                        || s.is_empty()
                        || s == "0"
                }) =>
            {
                JsValue::Null
            }
            _ => coerce_primitive(value, "null"),
        },
        "Literal" => {
            let expected = &schema["const"];
            let inferred = if expected.is_number() {
                "Number"
            } else if expected.is_boolean() {
                "Boolean"
            } else {
                "String"
            };
            let mut metadata = metadata.clone();
            metadata
                .typebox_kinds
                .insert(path.to_owned(), inferred.to_owned());
            let candidate = convert_typebox(&metadata, path, schema, value.clone());
            if candidate == *expected {
                candidate
            } else {
                value
            }
        }
        "Enum" | "Intersect" | "TemplateLiteral" => {
            let materialized = materialize_typebox(metadata, path, schema);
            let evaluated = schema_from_typebox_nodes(evaluate_typebox(&materialized));
            convert_typebox(&evaluated, &JsString::default(), &evaluated.schema, value)
        }
        "Union" => {
            let Some(schemas) = schema.get("anyOf").and_then(JsValue::as_array) else {
                return value;
            };
            if schemas.iter().any(|schema| check(schema, &value)) {
                return value;
            }
            let candidates: Vec<_> = schemas
                .iter()
                .enumerate()
                .map(|(index, child)| {
                    convert_typebox(
                        metadata,
                        &child_path(&child_path(path, "anyOf"), index.to_string()),
                        child,
                        value.clone(),
                    )
                })
                .collect();
            candidates
                .into_iter()
                .find(|candidate| check(schema, candidate))
                .unwrap_or(value)
        }
        "Array" => {
            let values = match value {
                JsValue::Array(values) => values,
                value => vec![value],
            };
            JsValue::Array(
                values
                    .into_iter()
                    .map(|value| {
                        convert_typebox(
                            metadata,
                            &child_path(path, "items"),
                            &schema["items"],
                            value,
                        )
                    })
                    .collect(),
            )
        }
        "Tuple" => {
            let JsValue::Array(mut values) = value else {
                return value;
            };
            if let Some(schemas) = schema.get("items").and_then(JsValue::as_array) {
                for (index, (value, child)) in values.iter_mut().zip(schemas).enumerate() {
                    *value = convert_typebox(
                        metadata,
                        &child_path(&child_path(path, "items"), index.to_string()),
                        child,
                        value.clone(),
                    );
                }
            }
            JsValue::Array(values)
        }
        "Object" | "Record" => {
            let JsValue::Object(mut object) = value else {
                return value;
            };
            let keyword = if kind == "Record" {
                "patternProperties"
            } else {
                "properties"
            };
            if let Some(properties) = schema.get(keyword).and_then(JsValue::as_object) {
                for (key, child) in js_object_entries(properties) {
                    let keys: Vec<_> = object.keys().cloned().collect();
                    for actual in keys {
                        if conversion_pattern_matches(key, &actual) {
                            let value = object.get_mut(&actual).expect("existing object key");
                            *value = convert_typebox(
                                metadata,
                                &child_path(&child_path(path, keyword), pointer_segment_js(key)),
                                child,
                                value.clone(),
                            );
                        }
                    }
                }
                // Preserve the pinned per-known-property additional pass.
                if let Some(additional) =
                    schema.get("additionalProperties").filter(|v| v.is_object())
                {
                    for (key, _) in js_object_entries(properties) {
                        for (actual, value) in object.iter_mut() {
                            if !conversion_pattern_matches(key, actual) {
                                *value = convert_typebox(
                                    metadata,
                                    &child_path(path, "additionalProperties"),
                                    additional,
                                    value.clone(),
                                );
                            }
                        }
                    }
                }
            }
            JsValue::Object(object)
        }
        "Ref" => value,
        _ => value,
    }
}

fn regex_for_js(pattern: &JsString, flags: &str) -> Result<regress::Regex, regress::Error> {
    let points: Vec<u32> = if flags.contains('u') {
        std::char::decode_utf16(pattern.units())
            .map(|point| {
                point
                    .map(u32::from)
                    .unwrap_or_else(|error| u32::from(error.unpaired_surrogate()))
            })
            .collect()
    } else {
        pattern.units().map(u32::from).collect()
    };
    regress::Regex::from_unicode(points.into_iter(), flags)
}
fn pattern_matches_js(pattern: impl Into<JsString>, value: &JsString) -> bool {
    regex_for_js(&pattern.into(), "u")
        .is_ok_and(|re| re.find_from_utf16(&value.as_utf16(), 0).next().is_some())
}
fn conversion_pattern_matches(pattern: &JsString, value: &JsString) -> bool {
    let mut anchored = JsString::from("^");
    anchored.push(pattern);
    anchored.push_str("$");
    regex_for_js(&anchored, "")
        .is_ok_and(|re| re.find_from_ucs2(&value.as_utf16(), 0).next().is_some())
}
fn format_matches_js(format: &str, value: &JsString) -> bool {
    let Some(value) = value.as_str() else {
        return format_matches_utf16(format, value);
    };
    compile_json(&json!({"format":format})).is_ok_and(|validator| validator.is_valid(&json!(value)))
}
fn check_at(root: &JsValue, schema: &JsValue, value: &JsValue) -> bool {
    let mut schema = schema.clone();
    if let Some(object) = schema.as_object_mut() {
        for keyword in ["$defs", "definitions"] {
            if let Some(definitions) = root.get(keyword) {
                object.entry(keyword).or_insert_with(|| definitions.clone());
            }
        }
    }
    check(&schema, value)
}
fn child_path(path: &JsString, key: impl Into<JsString>) -> JsString {
    let mut result = path.clone();
    result.push_str("/");
    result.push(&key.into());
    result
}
fn add_error(errors: &mut Vec<JsString>, path: &JsString, message: impl fmt::Display) {
    add_js_error(errors, path, &JsString::from(message.to_string()));
}
fn add_js_error(errors: &mut Vec<JsString>, path: &JsString, message: &JsString) {
    if errors.len() >= 8 {
        return;
    }
    let mut units = path.as_utf16();
    if units.first() == Some(&47) {
        units.remove(0);
    }
    for unit in &mut units {
        if *unit == 47 {
            *unit = 46;
        }
    }
    let path = if units.is_empty() {
        JsString::from("root")
    } else {
        JsString::from_utf16(units)
    };
    let mut error = JsString::from("  - ");
    error.push(&path);
    error.push_str(": ");
    error.push(message);
    errors.push(error);
}
fn add_required_error(
    errors: &mut Vec<JsString>,
    path: &JsString,
    property: &JsString,
    message: &JsString,
) {
    if property.is_empty() {
        add_js_error(errors, path, message);
        return;
    }
    if errors.len() >= 8 {
        return;
    }
    let mut units = path.as_utf16();
    if units.first() == Some(&47) {
        units.remove(0);
    }
    for unit in &mut units {
        if *unit == 47 {
            *unit = 46;
        }
    }
    let mut formatted = JsString::from_utf16(units);
    if !formatted.is_empty() {
        formatted.push_str(".");
    }
    formatted.push(property);
    let mut error = JsString::from("  - ");
    error.push(&formatted);
    error.push_str(": ");
    error.push(message);
    errors.push(error);
}
/// TypeBox resolves schema refs through WHATWG URL, then decodeURIComponent.
/// USVString conversion belongs only at this upstream-native URL boundary.
fn local_reference_pointer(reference: &JsString) -> Option<JsString> {
    let base = url::Url::parse("https://schema.local/").ok()?;
    let target = base.join(&reference.to_string_lossy()).ok()?;
    let fragment = target.fragment().unwrap_or_default().as_bytes();
    let mut bytes = Vec::new();
    let mut index = 0;
    while index < fragment.len() {
        if fragment[index] == b'%' {
            let pair = fragment.get(index + 1..index + 3)?;
            let high = (pair[0] as char).to_digit(16)?;
            let low = (pair[1] as char).to_digit(16)?;
            bytes.push((high * 16 + low) as u8);
            index += 3;
        } else {
            bytes.push(fragment[index]);
            index += 1;
        }
    }
    String::from_utf8(bytes).ok().map(JsString::from)
}
fn invalid_property_names_message(names: &[JsString]) -> JsString {
    let mut message = JsString::from("property names ");
    for (index, name) in names.iter().enumerate() {
        if index > 0 {
            message.push_str(", ");
        }
        message.push(name);
    }
    message.push_str(" are invalid");
    message
}
fn keyword_fails(schema: &JsValue, key: &str, value: &JsValue) -> bool {
    schema
        .get(key)
        .is_some_and(|constraint| !check(&tb_object([(key, constraint.clone())]), value))
}

/// Emit errors in TypeBox's fixed keyword order and property insertion order.
/// Using a schema walk avoids depending on jsonschema's different error
/// grouping, pointer escaping, keyword order, and required-property messages.
fn errors_at(
    root: &JsValue,
    schema: &JsValue,
    value: &JsValue,
    path: &JsString,
    errors: &mut Vec<JsString>,
) {
    if errors.len() >= 8 {
        return;
    }
    if schema == &JsValue::Bool(false) {
        add_error(errors, path, "schema is false");
        return;
    }
    if !schema.is_object() {
        return;
    }
    let types = schema_types(schema);
    if !types.is_empty() && !types.iter().any(|kind| matches_type(value, kind)) {
        let message = if schema["type"].is_string() {
            format!("must be {}", types[0])
        } else {
            format!("must be either {}", types.join(" or "))
        };
        add_error(errors, path, message);
    }
    if let Some(object) = value.as_object() {
        if let Some(required) = schema.get("required").and_then(JsValue::as_array) {
            let missing: Vec<_> = required
                .iter()
                .filter_map(JsValue::as_js_str)
                .filter(|key| !object.contains_key(*key))
                .collect();
            if let Some(first) = missing.first() {
                let mut message = JsString::from("must have required properties ");
                message.push(&JsString::join(missing.iter().copied(), ", "));
                add_required_error(errors, path, first, &message);
            }
        }
        if let Some(additional) = schema.get("additionalProperties") {
            let properties = schema.get("properties").and_then(JsValue::as_object);
            let patterns = schema.get("patternProperties").and_then(JsValue::as_object);
            let mut failed = false;
            for (key, child) in js_object_entries(object) {
                if properties.is_some_and(|props| props.contains_key(key))
                    || patterns.is_some_and(|patterns| {
                        patterns
                            .keys()
                            .any(|pattern| pattern_matches_js(pattern, key))
                    })
                {
                    continue;
                }
                if !check_at(root, additional, child) {
                    failed = true;
                }
                errors_at(root, additional, child, &child_path(path, key), errors);
            }
            if failed {
                add_error(errors, path, "must not have additional properties");
            }
        }
        for keyword in ["dependencies", "dependentRequired", "dependentSchemas"] {
            if let Some(dependencies) = schema.get(keyword).and_then(JsValue::as_object) {
                for (key, dependency) in js_object_entries(dependencies) {
                    if !object.contains_key(key) {
                        continue;
                    }
                    if let Some(keys) = dependency.as_array() {
                        let names: Vec<_> = keys.iter().filter_map(JsValue::as_js_str).collect();
                        for name in &names {
                            if !object.contains_key(*name) {
                                let mut message = JsString::from("must have properties ");
                                message.push(&JsString::join(names.iter().copied(), ", "));
                                message.push_str(" when property ");
                                message.push(key);
                                message.push_str(" is present");
                                add_js_error(errors, path, &message);
                                if keyword == "dependencies" {
                                    break;
                                }
                            }
                        }
                    } else {
                        errors_at(root, dependency, value, path, errors);
                    }
                }
            }
        }
        if let Some(patterns) = schema.get("patternProperties").and_then(JsValue::as_object) {
            for (pattern, child_schema) in js_object_entries(patterns) {
                for (key, child) in js_object_entries(object) {
                    if pattern_matches_js(pattern, key) {
                        errors_at(root, child_schema, child, &child_path(path, key), errors);
                    }
                }
            }
        }
        if let Some(properties) = schema.get("properties").and_then(JsValue::as_object) {
            for (key, child_schema) in js_object_entries(properties) {
                if let Some(child) = object.get(key) {
                    errors_at(root, child_schema, child, &child_path(path, key), errors);
                }
            }
        }
        if let Some(names_schema) = schema.get("propertyNames") {
            let mut invalid = Vec::new();
            for (key, _) in js_object_entries(object) {
                if !check_at(root, names_schema, &JsValue::String(key.clone())) {
                    invalid.push(key.clone());
                }
                errors_at(
                    root,
                    names_schema,
                    &JsValue::String(key.clone()),
                    &child_path(path, key),
                    errors,
                );
            }
            if !invalid.is_empty() {
                add_js_error(errors, path, &invalid_property_names_message(&invalid));
            }
        }
        for (keyword, comparison) in [("minProperties", "fewer"), ("maxProperties", "more")] {
            if keyword_fails(schema, keyword, value) {
                add_error(
                    errors,
                    path,
                    format!(
                        "must not have {comparison} than {} properties",
                        stringify(&schema[keyword])
                    ),
                );
            }
        }
    }
    if let Some(items) = value.as_array() {
        if let (Some(tuple), Some(additional)) = (
            schema.get("items").and_then(JsValue::as_array),
            schema.get("additionalItems"),
        ) {
            for (index, item) in items.iter().enumerate().skip(tuple.len()) {
                errors_at(
                    root,
                    additional,
                    item,
                    &child_path(path, index.to_string()),
                    errors,
                );
                // TypeBox's additionalItems uses Every rather than EveryAll.
                if !check_at(root, additional, item) {
                    break;
                }
            }
        }
        let contains_count = schema.get("contains").map(|contains| {
            items
                .iter()
                .filter(|item| check_at(root, contains, item))
                .count()
        });
        if contains_count == Some(0)
            && schema.get("minContains").and_then(JsValue::as_u64) != Some(0)
        {
            add_error(errors, path, "must contain at least 1 valid item");
        }
        match schema.get("items") {
            Some(JsValue::Array(schemas)) => {
                for (index, (item, schema)) in items.iter().zip(schemas).enumerate() {
                    errors_at(
                        root,
                        schema,
                        item,
                        &child_path(path, index.to_string()),
                        errors,
                    );
                }
            }
            Some(schema_items) => {
                let offset = schema
                    .get("prefixItems")
                    .and_then(JsValue::as_array)
                    .map_or(0, Vec::len);
                for (index, item) in items.iter().enumerate().skip(offset) {
                    errors_at(
                        root,
                        schema_items,
                        item,
                        &child_path(path, index.to_string()),
                        errors,
                    );
                }
            }
            None => {}
        }
        if let (Some(count), Some(max)) = (
            contains_count,
            schema.get("maxContains").and_then(JsValue::as_u64),
        ) && count as u64 > max
        {
            add_error(errors, path, "must contain at least 1 valid item");
        }
        if keyword_fails(schema, "maxItems", value) {
            add_error(
                errors,
                path,
                format!(
                    "must not have more than {} items",
                    stringify(&schema["maxItems"])
                ),
            );
        }
        if let (Some(count), Some(min)) = (
            contains_count,
            schema.get("minContains").and_then(JsValue::as_u64),
        ) && (count as u64) < min
        {
            add_error(errors, path, "must contain at least 1 valid item");
        }
        if keyword_fails(schema, "minItems", value) {
            add_error(
                errors,
                path,
                format!(
                    "must not have fewer than {} items",
                    stringify(&schema["minItems"])
                ),
            );
        }
        if let Some(schemas) = schema.get("prefixItems").and_then(JsValue::as_array) {
            for (index, (item, schema)) in items.iter().zip(schemas).enumerate() {
                errors_at(
                    root,
                    schema,
                    item,
                    &child_path(path, index.to_string()),
                    errors,
                );
            }
        }
        if keyword_fails(schema, "uniqueItems", value) {
            add_error(errors, path, "must not have duplicate items");
        }
    }
    if value.is_string() {
        for (keyword, comparison) in [("maxLength", "more"), ("minLength", "fewer")] {
            if keyword_fails(schema, keyword, value) {
                add_error(
                    errors,
                    path,
                    format!(
                        "must not have {comparison} than {} characters",
                        stringify(&schema[keyword])
                    ),
                );
            }
        }
        for keyword in ["format", "pattern"] {
            if keyword_fails(schema, keyword, value) {
                let mut message = JsString::from(format!("must match {keyword} \""));
                if let Some(text) = schema[keyword].as_js_str() {
                    message.push(text);
                }
                message.push_str("\"");
                add_js_error(errors, path, &message);
            }
        }
    }
    if value.as_f64().is_some_and(f64::is_finite) {
        for (keyword, comparison) in [
            ("exclusiveMaximum", "<"),
            ("exclusiveMinimum", ">"),
            ("maximum", "<="),
            ("minimum", ">="),
        ] {
            if keyword_fails(schema, keyword, value) {
                add_error(
                    errors,
                    path,
                    format!("must be {comparison} {}", stringify(&schema[keyword])),
                );
            }
        }
        if keyword_fails(schema, "multipleOf", value) {
            add_error(
                errors,
                path,
                format!("must be multiple of {}", stringify(&schema["multipleOf"])),
            );
        }
    }
    if let Some(reference) = schema.get("$ref").and_then(JsValue::as_js_str) {
        if let Some(target) =
            local_reference_pointer(reference).and_then(|pointer| js_pointer(root, &pointer))
        {
            errors_at(root, target, value, path, errors);
        } else {
            add_error(errors, path, "schema is false");
        }
    }
    if keyword_fails(schema, "const", value) {
        add_error(errors, path, "must be equal to constant");
    }
    if keyword_fails(schema, "enum", value) {
        add_error(errors, path, "must be equal to one of the allowed values");
    }
    if let Some(condition) = schema.get("if") {
        let keyword = if check_at(root, condition, value) {
            "then"
        } else {
            "else"
        };
        if let Some(branch) = schema.get(keyword)
            && !check_at(root, branch, value)
        {
            // TypeBox intentionally does not copy errors from its separate
            // true-branch context, but does copy the false-branch errors.
            if keyword == "else" {
                errors_at(root, branch, value, path, errors);
            }
            add_error(errors, path, format!("must match \"{keyword}\" schema"));
        }
    }
    if let Some(negated) = schema.get("not")
        && check_at(root, negated, value)
    {
        add_error(errors, path, "must not be valid");
    }
    for keyword in ["allOf", "anyOf", "oneOf"] {
        if let Some(schemas) = schema.get(keyword).and_then(JsValue::as_array) {
            let passing = schemas
                .iter()
                .filter(|schema| check_at(root, schema, value))
                .count();
            let valid = match keyword {
                "allOf" => passing == schemas.len(),
                "anyOf" => passing > 0,
                _ => passing == 1,
            };
            if !valid {
                if keyword == "allOf" || passing == 0 {
                    for child in schemas {
                        let mut nested = Vec::new();
                        errors_at(root, child, value, path, &mut nested);
                        for error in nested {
                            if errors.len() < 8 {
                                errors.push(error);
                            }
                        }
                    }
                }
                if keyword == "anyOf" {
                    add_error(errors, path, "must match a schema in anyOf");
                }
                if keyword == "oneOf" {
                    add_error(errors, path, "must match exactly one schema in oneOf");
                }
            }
        }
    }
    // jsonschema determines evaluated members across applicators and refs.
    if (schema.get("unevaluatedItems").is_some() || schema.get("unevaluatedProperties").is_some())
        && let Ok(Some(validator)) = compile(schema)
        && let Ok(projected) = value.to_json()
    {
        for error in validator.iter_errors(&projected) {
            match error.kind() {
                jsonschema::error::ValidationErrorKind::UnevaluatedItems { .. } => {
                    add_error(errors, path, "must not have unevaluated items")
                }
                jsonschema::error::ValidationErrorKind::UnevaluatedProperties { .. } => {
                    add_error(errors, path, "must not have unevaluated properties")
                }
                _ => {}
            }
        }
    }
}

// Build a temporary TypeBox tree for its type-level conversion operations.
// Metadata never becomes part of model-facing JSON schemas.
fn materialize_typebox(metadata: &Schema, path: &JsString, schema: &JsValue) -> JsValue {
    let mut result = schema.clone();
    let mut prefix = path.clone();
    prefix.push_str("/");
    let path_units = path.as_utf16();
    let prefix_units = prefix.as_utf16();
    for (pointer, kind) in &metadata.typebox_kinds {
        let units = pointer.as_utf16();
        let relative = if pointer == path {
            JsString::default()
        } else if units.starts_with(&prefix_units) {
            JsString::from_utf16(units[path_units.len()..].to_vec())
        } else {
            continue;
        };
        if let Some(node) = js_pointer_mut(&mut result, &relative).and_then(JsValue::as_object_mut)
        {
            node.insert("~kind", tb_string(kind));
        }
    }
    result
}
fn schema_from_typebox_nodes(mut value: JsValue) -> Schema {
    fn visit(value: &mut JsValue, path: &JsString, kinds: &mut BTreeMap<JsString, String>) {
        match value {
            JsValue::Object(object) => {
                if let Some(JsValue::String(kind)) = object.shift_remove("~kind")
                    && let Some(kind) = kind.as_str()
                {
                    kinds.insert(path.clone(), kind.to_owned());
                }
                for key in ["~optional", "~readonly", "~immutable"] {
                    object.shift_remove(key);
                }
                for (key, child) in object.iter_mut() {
                    if !matches!(
                        key.as_str(),
                        Some("const" | "enum" | "default" | "examples")
                    ) {
                        visit(child, &child_path(path, pointer_segment_js(key)), kinds);
                    }
                }
            }
            JsValue::Array(array) => {
                for (index, child) in array.iter_mut().enumerate() {
                    visit(child, &child_path(path, index.to_string()), kinds);
                }
            }
            _ => {}
        }
    }
    let mut kinds = BTreeMap::new();
    visit(&mut value, &JsString::default(), &mut kinds);
    Schema {
        schema: value,
        is_typebox: false,
        typebox_kinds: kinds,
    }
}

// RFC 6901 pointer operations retain exact JavaScript UTF-16 property names.
fn pointer_segment_js(key: &JsString) -> JsString {
    let mut result = Vec::new();
    for unit in key.units() {
        match unit {
            0x7e => result.extend([0x7e, 0x30]),
            0x2f => result.extend([0x7e, 0x31]),
            _ => result.push(unit),
        }
    }
    JsString::from_utf16(result)
}
fn js_pointer_segments(pointer: &JsString) -> Option<Vec<JsString>> {
    let units = pointer.as_utf16();
    if units.is_empty() {
        return Some(Vec::new());
    }
    if units[0] != 0x2f {
        return None;
    }
    Some(
        units[1..]
            .split(|unit| *unit == 0x2f)
            .map(|segment| {
                let mut decoded = Vec::with_capacity(segment.len());
                let mut index = 0;
                while index < segment.len() {
                    if segment[index] == 0x7e && index + 1 < segment.len() {
                        match segment[index + 1] {
                            0x30 => {
                                decoded.push(0x7e);
                                index += 2;
                                continue;
                            }
                            0x31 => {
                                decoded.push(0x2f);
                                index += 2;
                                continue;
                            }
                            _ => {}
                        }
                    }
                    decoded.push(segment[index]);
                    index += 1;
                }
                JsString::from_utf16(decoded)
            })
            .collect(),
    )
}
fn js_pointer_index(segment: &JsString) -> Option<usize> {
    let text = segment.as_str()?;
    if text.starts_with('+') || (text.starts_with('0') && text.len() != 1) {
        return None;
    }
    text.parse().ok()
}
fn js_pointer<'a>(value: &'a JsValue, pointer: &JsString) -> Option<&'a JsValue> {
    let mut value = value;
    for segment in js_pointer_segments(pointer)? {
        value = match value {
            JsValue::Object(object) => object.get(&segment)?,
            JsValue::Array(array) => array.get(js_pointer_index(&segment)?)?,
            _ => return None,
        };
    }
    Some(value)
}
fn js_pointer_mut<'a>(value: &'a mut JsValue, pointer: &JsString) -> Option<&'a mut JsValue> {
    let mut value = value;
    for segment in js_pointer_segments(pointer)? {
        value = match value {
            JsValue::Object(object) => object.get_mut(&segment)?,
            JsValue::Array(array) => array.get_mut(js_pointer_index(&segment)?)?,
            _ => return None,
        };
    }
    Some(value)
}

fn tb_object<const N: usize>(fields: [(&str, JsValue); N]) -> JsValue {
    JsValue::Object(fields.into_iter().collect())
}
fn tb_string(value: &str) -> JsValue {
    JsValue::String(value.into())
}

// TypeBox 1.3.27 build/type/engine/evaluate and build/type/extends.
// Input/output are TEMPORARY schema trees: ~kind/~optional/~readonly/~immutable
// represent TypeBox's non-enumerable metadata and must never reach wire JSON.
// The caller instantiates Ref/Cyclic/Deferred forms before evaluating Intersect.
// Uses existing js_object_entries() for JavaScript property order.
fn evaluate_typebox(schema: &JsValue) -> JsValue {
    match tb_kind(schema) {
        "Intersect" => tb_intersect(tb_list(schema, "allOf")),
        "Union" => tb_union(tb_list(schema, "anyOf").to_vec()),
        "Enum" => tb_union(tb_list(schema, "enum").iter().map(tb_literal).collect()),
        "TemplateLiteral" => evaluate_template_literal(
            schema["pattern"]
                .as_js_str()
                .unwrap_or(&JsString::default()),
        ),
        "Dependent" => {
            let intersect = tb_intersect(&[schema["if"].clone(), schema["then"].clone()]);
            let otherwise = evaluate_typebox(&schema["else"]);
            let candidates = if tb_kind(&otherwise) == "Union" {
                tb_list(&otherwise, "anyOf").to_vec()
            } else {
                vec![otherwise]
            };
            let excluded = tb_union(
                candidates
                    .into_iter()
                    .filter(|s| !tb_extends(s, &schema["if"]))
                    .collect(),
            );
            tb_union(vec![intersect, excluded])
        }
        _ => schema.clone(),
    }
}
fn tb_kind(schema: &JsValue) -> &str {
    schema.get("~kind").and_then(JsValue::as_str).unwrap_or("")
}
fn tb_list<'a>(schema: &'a JsValue, key: &str) -> &'a [JsValue] {
    schema
        .get(key)
        .and_then(JsValue::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}
fn tb_never() -> JsValue {
    tb_object([
        ("~kind", tb_string("Never")),
        ("not", JsValue::Object(JsObject::new())),
    ])
}
fn tb_literal(value: &JsValue) -> JsValue {
    let type_name = match value {
        JsValue::Bool(_) => "boolean",
        JsValue::Number(_) => "number",
        _ => "string",
    };
    tb_object([
        ("~kind", tb_string("Literal")),
        ("type", tb_string(type_name)),
        ("const", value.clone()),
    ])
}
fn tb_intersect(types: &[JsValue]) -> JsValue {
    let distributed = tb_distribute(types, Vec::new());
    tb_union(tb_broaden(distributed))
}
fn tb_distribute(types: &[JsValue], mut result: Vec<JsValue>) -> Vec<JsValue> {
    for current in types {
        if tb_kind(current) == "Union" {
            result = tb_list(current, "anyOf")
                .iter()
                .flat_map(|branch| tb_distribute(std::slice::from_ref(branch), result.clone()))
                .collect();
        } else if result.is_empty() {
            result.push(current.clone());
        } else {
            result = result
                .into_iter()
                .map(|previous| {
                    let left = evaluate_typebox(&previous);
                    let right = evaluate_typebox(current);
                    if tb_kind(&left) == "Union" || tb_kind(&right) == "Union" {
                        tb_intersect(&[left, right])
                    } else {
                        tb_narrow(&left, &right)
                    }
                })
                .collect();
        }
    }
    result
}
fn tb_union(types: Vec<JsValue>) -> JsValue {
    let mut types = tb_broaden(types);
    match types.len() {
        0 => tb_never(),
        1 => types.remove(0),
        _ => tb_object([
            ("~kind", tb_string("Union")),
            ("anyOf", JsValue::Array(types)),
        ]),
    }
}
fn tb_broaden(types: Vec<JsValue>) -> Vec<JsValue> {
    let mut result = Vec::new();
    for current in types {
        let current = evaluate_typebox(&current);
        match tb_kind(&current) {
            "Any" | "Unknown" => return vec![current],
            "Never" => continue,
            "Object" => result.push(current),
            _ => {
                // Keep the original set if a member contains the new type;
                // earlier provisional removals must then be discarded.
                if result.iter().any(|existing| tb_extends(&current, existing)) {
                    continue;
                }
                result.retain(|existing| !tb_extends(existing, &current));
                result.push(current);
            }
        }
    }
    fn flatten(schema: JsValue, result: &mut Vec<JsValue>) {
        if tb_kind(&schema) == "Union" {
            for child in tb_list(&schema, "anyOf") {
                flatten(child.clone(), result);
            }
        } else {
            result.push(schema);
        }
    }
    let mut flattened = Vec::new();
    for schema in result {
        flatten(schema, &mut flattened);
    }
    flattened
}
fn tb_narrow(left: &JsValue, right: &JsValue) -> JsValue {
    match (tb_kind(left), tb_kind(right)) {
        ("Never" | "Any", _) => return left.clone(),
        ("Unknown", _) => return right.clone(),
        (_, "Never" | "Any") => return right.clone(),
        (_, "Unknown") => return left.clone(),
        _ => {}
    }
    let composite_left = matches!(tb_kind(left), "Object" | "Tuple");
    let composite_right = matches!(tb_kind(right), "Object" | "Tuple");
    match (composite_left, composite_right) {
        (true, true) => tb_composite(left, right),
        (true, false) => left.clone(),
        (false, true) => right.clone(),
        (false, false) => match (tb_extends(left, right), tb_extends(right, left)) {
            (true, false) => left.clone(),
            // Equal picks RIGHT. Comparison ignores numeric/string constraints.
            (_, true) => right.clone(),
            _ => tb_never(),
        },
    }
}
fn tb_properties(schema: &JsValue) -> JsObject {
    if tb_kind(schema) == "Tuple" {
        return tb_list(schema, "items")
            .iter()
            .enumerate()
            .map(|(i, s)| (i.to_string(), s.clone()))
            .collect();
    }
    let mut properties = schema
        .get("properties")
        .and_then(JsValue::as_object)
        .cloned()
        .unwrap_or_default();
    let required = tb_list(schema, "required");
    // Reconstruct optional metadata at the object boundary if only schema JSON
    // and kinds were supplied. Existing metadata is otherwise preserved.
    for (key, property) in &mut properties {
        if !required.iter().any(|v| v.as_js_str() == Some(key))
            && let Some(object) = property.as_object_mut()
        {
            object.insert("~optional".to_owned(), JsValue::Bool(true));
        }
    }
    properties
}
fn tb_composite(left: &JsValue, right: &JsValue) -> JsValue {
    let mut properties = tb_properties(left);
    for (key, r) in js_object_entries(&tb_properties(right)) {
        let property = if let Some(l) = properties.get(key) {
            let optional = l.get("~optional").is_some() && r.get("~optional").is_some();
            let readonly = l.get("~readonly").is_some() && r.get("~readonly").is_some();
            let mut result = tb_intersect(&[l.clone(), r.clone()]);
            if let Some(object) = result.as_object_mut() {
                object.remove("~optional");
                object.remove("~readonly");
                if optional {
                    object.insert("~optional".to_owned(), JsValue::Bool(true));
                }
                if readonly {
                    object.insert("~readonly".to_owned(), JsValue::Bool(true));
                }
            }
            result
        } else {
            r.clone()
        };
        properties.insert(key.clone(), property);
    }
    let required: Vec<_> = js_object_entries(&properties)
        .into_iter()
        .filter(|(_, p)| p.get("~optional").is_none())
        .map(|(k, _)| JsValue::String(k.clone()))
        .collect();
    // Composite intentionally discards both objects' additionalProperties and
    // all other object options, constructing a fresh Type.Object(properties).
    let mut result = tb_object([
        ("~kind", tb_string("Object")),
        ("type", tb_string("object")),
        ("properties", JsValue::Object(properties)),
    ]);
    if !required.is_empty() {
        result["required"] = JsValue::Array(required);
    }
    result
}
fn tb_record_value(schema: &JsValue) -> Option<&JsValue> {
    schema
        .get("patternProperties")
        .and_then(JsValue::as_object)
        .and_then(|p| js_object_entries(p).first().map(|(_, v)| *v))
}
fn tb_extends(left: &JsValue, right: &JsValue) -> bool {
    let left_kind = tb_kind(left);
    let right_kind = tb_kind(right);
    match left_kind {
        "Any" | "Never" => return true,
        "Unknown" => return matches!(right_kind, "Any" | "Unknown"),
        "Intersect" | "Enum" | "TemplateLiteral" => {
            return tb_extends(&evaluate_typebox(left), right);
        }
        "Union" => {
            let rights = if right_kind == "Union" {
                tb_list(right, "anyOf").to_vec()
            } else {
                vec![right.clone()]
            };
            return tb_list(left, "anyOf")
                .iter()
                .all(|l| rights.iter().any(|r| tb_extends(l, r)));
        }
        "Dependent" => {
            return if tb_extends(&left["if"], right) {
                tb_extends(&left["then"], right)
            } else {
                tb_extends(&left["else"], right)
            };
        }
        _ => {}
    }
    let direct = match (left_kind, right_kind) {
        ("Literal", "Literal") => Some(tb_literal_equal(&left["const"], &right["const"])),
        ("Literal", "Number") => Some(left["const"].is_number()),
        ("Literal", "String") => Some(left["const"].is_string()),
        ("Literal", "Boolean") => Some(left["const"].is_boolean()),
        ("Number", "Number")
        | ("Integer", "Integer" | "Number")
        | ("String", "String")
        | ("Boolean", "Boolean")
        | ("Null", "Null") => Some(true),
        ("Array", "Array") => Some(
            (left.get("~immutable").is_none() || right.get("~immutable").is_some())
                && tb_extends(&left["items"], &right["items"]),
        ),
        ("Tuple", "Array") => Some(
            tb_list(left, "items")
                .iter()
                .all(|l| tb_extends(l, &right["items"])),
        ),
        ("Tuple", "Tuple") => {
            let l = tb_list(left, "items");
            let r = tb_list(right, "items");
            Some(l.len() == r.len() && l.iter().zip(r).all(|(l, r)| tb_extends(l, r)))
        }
        ("Object", "Object") => {
            let l = tb_properties(left);
            let r = tb_properties(right);
            Some(r.iter().all(|(key, r)| match l.get(key) {
                Some(l) => {
                    tb_extends(l, r)
                        && (l.get("~optional").is_none() || r.get("~optional").is_some())
                }
                None => r.get("~optional").is_some(),
            }))
        }
        ("Object", "Record") => Some(
            tb_record_value(right)
                .is_some_and(|r| tb_properties(left).values().all(|l| tb_extends(l, r))),
        ),
        ("Record", "Record") => Some(
            tb_record_value(left)
                .zip(tb_record_value(right))
                .is_some_and(|(l, r)| tb_extends(l, r)),
        ),
        ("Record", "Object") => Some(tb_properties(right).is_empty()),
        _ => None,
    };
    if let Some(result) = direct {
        return result;
    }
    if left_kind == "Record" {
        return matches!(right_kind, "Any" | "Unknown");
    }
    // Unknown/untyped schemas do not enter ExtendsRight in TypeBox.
    if !matches!(
        left_kind,
        "Literal"
            | "Number"
            | "Integer"
            | "String"
            | "Boolean"
            | "Null"
            | "Array"
            | "Tuple"
            | "Object"
            | "Record"
    ) {
        return false;
    }
    match right_kind {
        "Any" | "Unknown" => true,
        "Enum" | "TemplateLiteral" => tb_extends(left, &evaluate_typebox(right)),
        "Intersect" => tb_list(right, "allOf").iter().all(|r| tb_extends(left, r)),
        "Union" => tb_list(right, "anyOf").iter().any(|r| tb_extends(left, r)),
        "Dependent" => {
            if tb_extends(left, &right["if"]) {
                tb_extends(left, &right["then"])
            } else {
                tb_extends(left, &right["else"])
            }
        }
        _ => false,
    }
}
fn tb_literal_equal(left: &JsValue, right: &JsValue) -> bool {
    match (left.as_f64(), right.as_f64()) {
        (Some(l), Some(r)) => l == r,
        _ => left == right,
    }
}

// TypeBox 1.3.27: type/engine/template_literal/decode + script/parser.
// This parses TypeBox's pattern grammar, not general regular expressions.
#[derive(Clone)]
enum TemplatePatternPart {
    Literal(JsString),
    Union(Vec<TemplatePatternPart>),
    Infinite,
}

fn template_starts_with(input: &[u16], token: &str) -> bool {
    input.len() >= token.len()
        && input
            .iter()
            .copied()
            .zip(token.bytes().map(u16::from))
            .all(|(left, right)| left == right)
}
fn template_trim(mut input: &[u16]) -> &[u16] {
    // ECMAScript String.prototype.trimStart (not Rust's broader Unicode set).
    let is_space = |c| matches!(c, 0x0009..=0x000d | 0x0020 | 0x00a0 | 0x1680 | 0x2000..=0x200a | 0x2028 | 0x2029 | 0x202f | 0x205f | 0x3000 | 0xfeff);
    loop {
        let start = input
            .iter()
            .position(|unit| !is_space(*unit))
            .unwrap_or(input.len());
        input = &input[start..];
        if template_starts_with(input, "/*") {
            let comment = &input[2..];
            input = comment
                .windows(2)
                .position(|units| units == [0x2a, 0x2f])
                .map_or(&[][..], |end| &comment[end + 2..]);
        } else if template_starts_with(input, "//") {
            let comment = &input[2..];
            input = comment
                .iter()
                .position(|unit| *unit == 0x0a)
                .map_or(&[][..], |end| &comment[end..]);
        } else {
            return input;
        }
    }
}

fn template_const<'a>(input: &'a [u16], token: &str) -> Option<&'a [u16]> {
    let input = template_trim(input);
    template_starts_with(input, token).then(|| &input[token.len()..])
}

fn template_pattern_base(input: &[u16]) -> Option<(TemplatePatternPart, &[u16])> {
    // Keep the exact source ordering: bigint before integer, number before integer.
    const SPECIAL: [&str; 5] = [
        "-?(?:0|[1-9][0-9]*)n",
        ".*",
        "-?(?:0|[1-9][0-9]*)(?:\\.[0-9]+)?",
        "-?(?:0|[1-9][0-9]*)",
        "(?!)",
    ];
    for token in SPECIAL {
        if let Some(rest) = template_const(input, token) {
            return Some((TemplatePatternPart::Infinite, rest));
        }
    }
    if let Some(after_open) = template_const(input, "(") {
        let (parts, after_body) = template_pattern_body(after_open);
        if let Some(rest) = template_const(after_body, ")") {
            return Some((TemplatePatternPart::Union(parts), rest));
        }
    }
    // Until_1 advances one UTF-16 unit, does not trim, interprets no escapes,
    // and requires a sentinel. Lone surrogates remain ordinary literal units.
    for index in 0..input.len() {
        let rest = &input[index..];
        if SPECIAL
            .iter()
            .any(|token| template_starts_with(rest, token))
            || [0x28, 0x29, 0x24, 0x7c].contains(&rest[0])
        {
            return (index > 0).then(|| {
                (
                    TemplatePatternPart::Literal(JsString::from_utf16(input[..index].to_vec())),
                    rest,
                )
            });
        }
    }
    None
}

fn template_pattern_body(input: &[u16]) -> (Vec<TemplatePatternPart>, &[u16]) {
    // PatternTerm := PatternBase PatternBody. PatternUnion concatenates the
    // term lists separated by '|'; only PatternGroup creates a union node.
    let Some((first, after_first)) = template_pattern_base(input) else {
        return (Vec::new(), input);
    };
    let (tail, after_term) = template_pattern_body(after_first);
    let mut parts = Vec::with_capacity(tail.len() + 1);
    parts.push(first);
    parts.extend(tail);
    if let Some(after_bar) = template_const(after_term, "|") {
        let (right, rest) = template_pattern_body(after_bar);
        parts.extend(right);
        (parts, rest)
    } else {
        (parts, after_term)
    }
}

fn template_finite(part: &TemplatePatternPart) -> bool {
    match part {
        TemplatePatternPart::Literal(_) => true,
        TemplatePatternPart::Union(parts) => !parts.is_empty() && parts.iter().all(template_finite),
        TemplatePatternPart::Infinite => false,
    }
}

fn template_append(variants: &[JsString], part: &TemplatePatternPart) -> Vec<JsString> {
    match part {
        TemplatePatternPart::Literal(value) => {
            if variants.is_empty() {
                vec![value.clone()]
            } else {
                variants
                    .iter()
                    .map(|left| {
                        let mut result = left.clone();
                        result.push(value);
                        result
                    })
                    .collect()
            }
        }
        TemplatePatternPart::Union(parts) => parts
            .iter()
            .flat_map(|part| template_append(variants, part))
            .collect(),
        TemplatePatternPart::Infinite => unreachable!("finite check precedes expansion"),
    }
}

fn evaluate_template_literal(pattern: &JsString) -> JsValue {
    let string = || {
        tb_object([
            ("~kind", tb_string("String")),
            ("type", tb_string("string")),
        ])
    };
    let units = pattern.as_utf16();
    let Some(after_start) = template_const(&units, "^") else {
        return string();
    };
    let (parts, after_body) = template_pattern_body(after_start);
    // ParsePatternIntoTypes tests for a parse tuple, not exhaustion of input.
    if template_const(after_body, "$").is_none()
        || parts.is_empty()
        || !parts.iter().all(template_finite)
    {
        return string();
    }
    let mut variants = Vec::new();
    for part in &parts {
        variants = template_append(&variants, part);
    }
    let mut unique = Vec::new();
    for variant in variants {
        if !unique.contains(&variant) {
            unique.push(variant);
        }
    }
    let mut literals: Vec<JsValue> = unique
        .into_iter()
        .map(|value| tb_literal(&JsValue::String(value)))
        .collect();
    match literals.len() {
        0 => tb_never(),
        1 => literals.remove(0),
        _ => tb_object([
            ("~kind", tb_string("Union")),
            ("anyOf", JsValue::Array(literals)),
        ]),
    }
}

// Compatibility path for JavaScript values that cannot be projected to serde
// JSON without changing them. Ordinary subtrees use the offline validator.
// Schema values remain ordinary JSON.
fn check_compatible(root: &JsValue, schema: &JsValue, value: &JsValue) -> bool {
    compatible_check(root, schema, value).valid
}
#[derive(Default)]
struct CompatibleCheck {
    valid: bool,
    keys: std::collections::HashSet<JsString>,
    indices: std::collections::HashSet<usize>,
}
impl CompatibleCheck {
    fn valid() -> Self {
        Self {
            valid: true,
            ..Self::default()
        }
    }
    fn merge(&mut self, other: Self) {
        self.keys.extend(other.keys);
        self.indices.extend(other.indices);
    }
}
fn compatible_check(root: &JsValue, schema: &JsValue, value: &JsValue) -> CompatibleCheck {
    if let Some(boolean) = schema.as_bool() {
        return CompatibleCheck {
            valid: boolean,
            ..CompatibleCheck::default()
        };
    }
    let Some(schema_object) = schema.as_object() else {
        return CompatibleCheck::valid();
    };
    let mut evaluated = CompatibleCheck::valid();
    if let Some(types) = schema.get("type") {
        let matches = match types {
            JsValue::String(kind) => kind
                .as_str()
                .is_none_or(|kind| compatible_type(value, kind)),
            JsValue::Array(kinds) => kinds
                .iter()
                .filter_map(JsValue::as_str)
                .any(|kind| compatible_type(value, kind)),
            _ => true,
        };
        if !matches {
            evaluated.valid = false;
            return evaluated;
        }
    }
    match value {
        JsValue::Object(object) => {
            if let Some(required) = schema.get("required").and_then(JsValue::as_array)
                && required
                    .iter()
                    .filter_map(JsValue::as_js_str)
                    .any(|key| !object.contains_key(key))
            {
                evaluated.valid = false;
                return evaluated;
            }
            let properties = schema.get("properties").and_then(JsValue::as_object);
            let patterns = schema.get("patternProperties").and_then(JsValue::as_object);
            for (key, child) in object.iter() {
                let literal = properties.and_then(|p| p.get(key));
                let mut matched = literal.is_some();
                if let Some(child_schema) = literal {
                    if !check_compatible(root, child_schema, child) {
                        evaluated.valid = false;
                        return evaluated;
                    }
                    evaluated.keys.insert(key.clone());
                }
                if let Some(patterns) = patterns {
                    for (pattern, child_schema) in patterns {
                        if pattern_matches_js(pattern, key) {
                            matched = true;
                            if !check_compatible(root, child_schema, child) {
                                evaluated.valid = false;
                                return evaluated;
                            }
                            evaluated.keys.insert(key.clone());
                        }
                    }
                }
                if !matched && let Some(additional) = schema.get("additionalProperties") {
                    if !check_compatible(root, additional, child) {
                        evaluated.valid = false;
                        return evaluated;
                    }
                    evaluated.keys.insert(key.clone());
                }
                if let Some(names) = schema.get("propertyNames")
                    && !check_compatible(root, names, &JsValue::String(key.clone()))
                {
                    evaluated.valid = false;
                    return evaluated;
                }
            }
            for keyword in ["dependencies", "dependentRequired", "dependentSchemas"] {
                if let Some(dependencies) = schema.get(keyword).and_then(JsValue::as_object) {
                    for (key, dependency) in dependencies {
                        if !object.contains_key(key) {
                            continue;
                        }
                        if let Some(keys) = dependency.as_array() {
                            if keys
                                .iter()
                                .filter_map(JsValue::as_js_str)
                                .any(|key| !object.contains_key(key))
                            {
                                evaluated.valid = false;
                                return evaluated;
                            }
                        } else {
                            let result = compatible_check(root, dependency, value);
                            if !result.valid {
                                evaluated.valid = false;
                                return evaluated;
                            }
                            evaluated.merge(result);
                        }
                    }
                }
            }
            if !compatible_count(schema, "minProperties", object.len(), true)
                || !compatible_count(schema, "maxProperties", object.len(), false)
            {
                evaluated.valid = false;
                return evaluated;
            }
        }
        JsValue::Array(items) => {
            if let Some(prefix) = schema.get("prefixItems").and_then(JsValue::as_array) {
                for (index, (child, child_schema)) in items.iter().zip(prefix).enumerate() {
                    if !check_compatible(root, child_schema, child) {
                        evaluated.valid = false;
                        return evaluated;
                    }
                    evaluated.indices.insert(index);
                }
            }
            if let Some(item_schema) = schema.get("items") {
                if let Some(tuple) = item_schema.as_array() {
                    for (index, (child, child_schema)) in items.iter().zip(tuple).enumerate() {
                        if !check_compatible(root, child_schema, child) {
                            evaluated.valid = false;
                            return evaluated;
                        }
                        evaluated.indices.insert(index);
                    }
                    if let Some(additional) = schema.get("additionalItems") {
                        for (index, child) in items.iter().enumerate().skip(tuple.len()) {
                            if !check_compatible(root, additional, child) {
                                evaluated.valid = false;
                                return evaluated;
                            }
                            evaluated.indices.insert(index);
                        }
                    }
                } else {
                    let offset = schema
                        .get("prefixItems")
                        .and_then(JsValue::as_array)
                        .map_or(0, Vec::len);
                    for (index, child) in items.iter().enumerate().skip(offset) {
                        if !check_compatible(root, item_schema, child) {
                            evaluated.valid = false;
                            return evaluated;
                        }
                        evaluated.indices.insert(index);
                    }
                }
            }
            if let Some(contains) = schema.get("contains") {
                let matching: Vec<_> = items
                    .iter()
                    .enumerate()
                    .filter(|(_, child)| check_compatible(root, contains, child))
                    .map(|(index, _)| index)
                    .collect();
                let minimum = schema
                    .get("minContains")
                    .and_then(JsValue::as_u64)
                    .unwrap_or(1);
                if (matching.len() as u64) < minimum
                    || schema
                        .get("maxContains")
                        .and_then(JsValue::as_u64)
                        .is_some_and(|max| matching.len() as u64 > max)
                {
                    evaluated.valid = false;
                    return evaluated;
                }
                evaluated.indices.extend(matching);
            }
            if !compatible_count(schema, "minItems", items.len(), true)
                || !compatible_count(schema, "maxItems", items.len(), false)
            {
                evaluated.valid = false;
                return evaluated;
            }
            if schema.get("uniqueItems").and_then(JsValue::as_bool) == Some(true) {
                let mut hashes = std::collections::HashSet::new();
                if items
                    .iter()
                    .any(|item| !hashes.insert(compatible_hash(item)))
                {
                    evaluated.valid = false;
                    return evaluated;
                }
            }
        }
        JsValue::String(string) => {
            // TypeBox uses its own grapheme algorithm, with codePointAt keeping
            // lone surrogate units intact. No replacement text is used here.
            let length = compatible_grapheme_count(string);
            if !compatible_count(schema, "minLength", length, true)
                || !compatible_count(schema, "maxLength", length, false)
            {
                evaluated.valid = false;
                return evaluated;
            }
            if let Some(pattern) = schema.get("pattern").and_then(JsValue::as_js_str)
                && !pattern_matches_js(pattern, string)
            {
                evaluated.valid = false;
                return evaluated;
            }
            if let Some(format) = schema.get("format").and_then(JsValue::as_str)
                && !format_matches_js(format, string)
            {
                evaluated.valid = false;
                return evaluated;
            }
        }
        JsValue::Number(number) if number.is_finite() => {
            for keyword in [
                "minimum",
                "maximum",
                "exclusiveMinimum",
                "exclusiveMaximum",
                "multipleOf",
            ] {
                if let Some(limit) = schema
                    .get(keyword)
                    .and_then(JsValue::as_f64)
                    .filter(|n| n.is_finite())
                {
                    let valid = match keyword {
                        "minimum" => *number >= limit,
                        "maximum" => *number <= limit,
                        "exclusiveMinimum" => *number > limit,
                        "exclusiveMaximum" => *number < limit,
                        _ => {
                            let remainder = number % limit;
                            number.fract() == 0.0 && (1.0 / limit).fract() == 0.0
                                || remainder
                                    .abs()
                                    .min((remainder - limit).abs())
                                    .min((remainder + limit).abs())
                                    < 1e-10
                        }
                    };
                    if !valid {
                        evaluated.valid = false;
                        return evaluated;
                    }
                }
            }
        }
        // TypeBox guards numeric constraints with Number.isFinite.
        JsValue::Number(_) | JsValue::Null | JsValue::Bool(_) => {}
    }
    if let Some(reference) = schema.get("$ref").and_then(JsValue::as_js_str) {
        let target =
            local_reference_pointer(reference).and_then(|pointer| js_pointer(root, &pointer));
        let Some(target) = target else {
            evaluated.valid = false;
            return evaluated;
        };
        let result = compatible_check(root, target, value);
        if !result.valid {
            evaluated.valid = false;
            return evaluated;
        }
        evaluated.merge(result);
    }
    // Equality compares the exact JavaScript domain without JSON projection.
    if let Some(expected) = schema.get("const")
        && !compatible_equal(value, expected)
    {
        evaluated.valid = false;
        return evaluated;
    }
    if let Some(options) = schema.get("enum").and_then(JsValue::as_array)
        && !options.iter().any(|option| compatible_equal(value, option))
    {
        evaluated.valid = false;
        return evaluated;
    }
    if let Some(condition) = schema.get("if") {
        let condition_result = compatible_check(root, condition, value);
        let branch = if condition_result.valid {
            "then"
        } else {
            "else"
        };
        // CheckSchema keeps annotations collected while checking if even when
        // it fails; relevant only to unevaluated constraints on this instance.
        evaluated.merge(condition_result);
        if let Some(branch_schema) = schema.get(branch) {
            let result = compatible_check(root, branch_schema, value);
            if !result.valid {
                evaluated.valid = false;
                return evaluated;
            }
            evaluated.merge(result);
        }
    }
    if let Some(negated) = schema.get("not") {
        let result = compatible_check(root, negated, value);
        if result.valid {
            evaluated.valid = false;
            return evaluated;
        }
        evaluated.merge(result);
    }
    for keyword in ["allOf", "anyOf", "oneOf"] {
        if let Some(branches) = schema.get(keyword).and_then(JsValue::as_array) {
            let passing: Vec<_> = branches
                .iter()
                .map(|branch| compatible_check(root, branch, value))
                .filter(|result| result.valid)
                .collect();
            let valid = match keyword {
                "allOf" => passing.len() == branches.len(),
                "anyOf" => !passing.is_empty(),
                _ => passing.len() == 1,
            };
            if !valid {
                evaluated.valid = false;
                return evaluated;
            }
            for result in passing {
                evaluated.merge(result);
            }
        }
    }
    if let JsValue::Object(object) = value
        && let Some(unevaluated) = schema_object.get("unevaluatedProperties")
    {
        for (key, child) in object.iter() {
            if !evaluated.keys.contains(key) {
                if !check_compatible(root, unevaluated, child) {
                    evaluated.valid = false;
                    return evaluated;
                }
                evaluated.keys.insert(key.clone());
            }
        }
    }
    if let JsValue::Array(items) = value
        && let Some(unevaluated) = schema_object.get("unevaluatedItems")
    {
        for (index, child) in items.iter().enumerate() {
            if !evaluated.indices.contains(&index) {
                if !check_compatible(root, unevaluated, child) {
                    evaluated.valid = false;
                    return evaluated;
                }
                evaluated.indices.insert(index);
            }
        }
    }
    evaluated
}
fn compatible_type(value: &JsValue, kind: &str) -> bool {
    match (kind, value) {
        ("null", JsValue::Null)
        | ("boolean", JsValue::Bool(_))
        | ("string", JsValue::String(_))
        | ("array", JsValue::Array(_))
        | ("object", JsValue::Object(_)) => true,
        ("number", JsValue::Number(number)) => number.is_finite(),
        ("integer", JsValue::Number(number)) => number.is_finite() && number.fract() == 0.0,
        ("null" | "boolean" | "string" | "array" | "object" | "number" | "integer", _) => false,
        _ => true,
    }
}
fn compatible_count(schema: &JsValue, keyword: &str, count: usize, minimum: bool) -> bool {
    schema
        .get(keyword)
        .and_then(JsValue::as_u64)
        .is_none_or(|limit| {
            if minimum {
                count as u64 >= limit
            } else {
                count as u64 <= limit
            }
        })
}
fn compatible_equal(left: &JsValue, right: &JsValue) -> bool {
    match (left, right) {
        (JsValue::Null, JsValue::Null) => true,
        (JsValue::Bool(left), JsValue::Bool(right)) => left == right,
        (JsValue::Number(left), JsValue::Number(right)) => left == right,
        (JsValue::String(left), JsValue::String(right)) => left == right,
        (JsValue::Array(left), JsValue::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| compatible_equal(left, right))
        }
        (JsValue::Object(left), JsValue::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, left)| {
                    right
                        .get(key)
                        .is_some_and(|right| compatible_equal(left, right))
                })
        }
        _ => false,
    }
}
fn compatible_grapheme_count(string: &JsString) -> usize {
    let units: Vec<_> = string.units().collect();
    fn point(units: &[u16], index: usize) -> u32 {
        let first = u32::from(units[index]);
        if (0xd800..=0xdbff).contains(&first)
            && units
                .get(index + 1)
                .is_some_and(|second| (0xdc00..=0xdfff).contains(second))
        {
            0x10000 + ((first - 0xd800) << 10) + (u32::from(units[index + 1]) - 0xdc00)
        } else {
            first
        }
    }
    fn width(point: u32) -> usize {
        if point > 0xffff { 2 } else { 1 }
    }
    fn modifiers(units: &[u16], mut index: usize) -> usize {
        while index < units.len()
            && matches!(point(units, index), 0x0300..=0x036f | 0x1ab0..=0x1aff | 0x1dc0..=0x1dff | 0xfe20..=0xfe2f | 0xfe00..=0xfe0f)
        {
            index += width(point(units, index));
        }
        index
    }
    let mut index = 0;
    let mut count = 0;
    while index < units.len() {
        let start = point(&units, index);
        index = modifiers(&units, index + width(start));
        while index + 1 < units.len() && point(&units, index) == 0x200d {
            index = modifiers(&units, index + 1 + width(point(&units, index + 1)));
        }
        if (0x1f1e6..=0x1f1ff).contains(&start)
            && index < units.len()
            && (0x1f1e6..=0x1f1ff).contains(&point(&units, index))
        {
            index += width(point(&units, index));
        }
        count += 1;
    }
    count
}
// Exact TypeBox system/hashing/hash.mjs for JSON-like JavaScript values.
// Deliberately hashes raw IEEE754 (distinguishing -0 and NaN payloads), but
// TextEncoder replaces lone surrogates while hashing strings. Containers have
// no end marker, so existing upstream structural collisions remain observable.
fn compatible_hash(value: &JsValue) -> u64 {
    fn byte(state: &mut u64, value: u8) {
        *state = (*state ^ u64::from(value)).wrapping_mul(1099511628211);
    }
    fn string(state: &mut u64, value: &JsString) {
        byte(state, 10);
        for value in value.to_string_lossy().bytes() {
            byte(state, value);
        }
    }
    fn visit(state: &mut u64, value: &JsValue) {
        match value {
            JsValue::Null => byte(state, 6),
            JsValue::Bool(value) => {
                byte(state, 2);
                byte(state, u8::from(*value));
            }
            JsValue::Number(value) => {
                byte(state, 7);
                for value in value.to_le_bytes() {
                    byte(state, value);
                }
            }
            JsValue::String(value) => string(state, value),
            JsValue::Array(values) => {
                byte(state, 0);
                for value in values {
                    visit(state, value);
                }
            }
            JsValue::Object(object) => {
                byte(state, 8);
                let mut entries: Vec<_> = object.iter().collect();
                entries.sort_by(|(a, _), (b, _)| a.units().cmp(b.units()));
                for (key, value) in entries {
                    string(state, key);
                    visit(state, value);
                }
            }
        }
    }
    let mut state = 14695981039346656037;
    visit(&mut state, value);
    state
}

// Each source RegExp constant is compiled once, as in the upstream module.
macro_rules! format_regex_utf16 {
    ($pattern:literal, $flags:literal, $units:expr) => {{
        static REGEX: std::sync::LazyLock<regress::Regex> = std::sync::LazyLock::new(|| {
            regress::Regex::with_flags($pattern, $flags).expect("pinned TypeBox format pattern")
        });
        if $flags.contains('u') {
            REGEX.find_from_utf16($units, 0).next().is_some()
        } else {
            REGEX.find_from_ucs2($units, 0).next().is_some()
        }
    }};
}
// TypeBox 1.3.27 built-in format checks, restricted to JsString values
// containing at least one unpaired UTF-16 surrogate. Ordinary strings use the
// scalar format path. Unknown formats pass by source rule.
// Dependency adapters: regress 0.12.0 (utf16), unicode-normalization, url.
fn format_matches_utf16(format: &str, value: &JsString) -> bool {
    let units: Vec<_> = value.units().collect();
    match format {
        // These source validators admit only ASCII, or their IDNA permitted
        // categories exclude every surrogate. Their normalization never removes
        // or combines an unpaired surrogate, so the result is always false.
        "date-time"
        | "date"
        | "duration"
        | "hostname"
        | "idn-hostname"
        | "ipv4"
        | "ipv6"
        | "json-pointer-uri-fragment"
        | "time"
        | "uri-reference"
        | "uri"
        | "uuid" => false,
        "email" => format_regex_utf16!(
            r###"^(?:[a-z0-9!#$%&'*+/=?^_`{|}~-]+(?:\.[a-z0-9!#$%&'*+/=?^_`{|}~-]+)*|"(?:[^"\\]|\\[\x20-\x7e])*")@(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?(?:\.[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)*|\[(?:IPv6:[a-f0-9:]+|(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])(?:\.(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])){3})\])$"###,
            "i",
            &units
        ),
        "idn-email" => format_regex_utf16!(
            r###"^(?:[A-Za-z0-9!#$%&'*+\/=?^_`{|}~\u{0080}-\u{10FFFF}-]+(?:\.[A-Za-z0-9!#$%&'*+\/=?^_`{|}~\u{0080}-\u{10FFFF}-]+)*|"(?:[^"\\]|\\.)*")@[\p{L}\p{N}](?:[\p{L}\p{N}-]{0,62})(?<!-)(?:\.[\p{L}\p{N}](?:[\p{L}\p{N}-]{0,62})(?<!-))*$"###,
            "iu",
            &format_nfc_utf16(&units)
        ),
        "json-pointer" => format_regex_utf16!(r###"^(?:\/(?:[^~/]|~0|~1)*)*$"###, "", &units),
        "relative-json-pointer" => format_regex_utf16!(
            r###"^(?:0|[1-9][0-9]*)(?:#|(?:\/(?:[^~/]|~0|~1)*)*)$"###,
            "",
            &units
        ),
        "uri-template" => format_regex_utf16!(
            r###"^(?:(?:[^\x00-\x20"<>%\\^`{|}\x7f]|%[0-9a-f]{2})|\{[+#./;?&=,!@|]?(?:[a-z0-9_]|%[0-9a-f]{2})+(?:\.(?:[a-z0-9_]|%[0-9a-f]{2})+)*(?::[1-9]\d{0,3}|\*)?(?:,(?:[a-z0-9_]|%[0-9a-f]{2})+(?:\.(?:[a-z0-9_]|%[0-9a-f]{2})+)*(?::[1-9]\d{0,3}|\*)?)*\})*$"###,
            "i",
            &units
        ),
        "regex" => {
            // RegExp(pattern, "u") parses Unicode codepoints while preserving
            // literal unpaired surrogates as their own codepoint values.
            let points = std::char::decode_utf16(units.iter().copied()).map(|point| {
                point
                    .map(u32::from)
                    .unwrap_or_else(|error| u32::from(error.unpaired_surrogate()))
            });
            regress::Regex::from_unicode(points, "u").is_ok()
        }
        "iri" => {
            if format_regex_utf16!(r"[\x00-\x20<>\^`{|}\\]", "", &units)
                || format_regex_utf16!(r"%(?![0-9a-fA-F]{2})", "", &units)
            {
                return false;
            }
            let mut units = units;
            if units.len() < 2048 {
                let ipv_future = regress::Regex::with_flags(r"\[[vV][0-9a-fA-F]+\.[^\]]+\]", "")
                    .expect("pinned TypeBox IPvFuture pattern");
                let range = ipv_future
                    .find_from_ucs2(&units, 0)
                    .next()
                    .map(|found| found.range());
                if let Some(range) = range {
                    units.splice(range, "[::1]".encode_utf16());
                }
            }
            // WHATWG URL receives a USVString, intentionally replacing lone
            // surrogates at this native parser boundary and nowhere earlier.
            url::Url::parse(&String::from_utf16_lossy(&units)).is_ok()
        }
        "iri-reference" => {
            if format_regex_utf16!(r"[\x00-\x20\x7F\\]|%(?![0-9a-fA-F]{2})", "", &units)
                || format_regex_utf16!(r"^[a-zA-Z][a-zA-Z0-9+\-.]*\/\/", "", &units)
            {
                return false;
            }
            let base = url::Url::parse("http://example.com").expect("fixed IRI-reference base");
            base.join(&String::from_utf16_lossy(&units)).is_ok()
        }
        "url" => url::Url::parse(&String::from_utf16_lossy(&units)).is_ok(),
        _ => true,
    }
}
fn format_nfc_utf16(units: &[u16]) -> Vec<u16> {
    use unicode_normalization::UnicodeNormalization;
    // String.normalize keeps unpaired surrogates. Normalize maximal scalar
    // segments, flushing at each unpaired code unit; never substitute U+FFFD.
    let mut normalized = Vec::new();
    let mut scalar_segment = String::new();
    for point in std::char::decode_utf16(units.iter().copied()) {
        match point {
            Ok(point) => scalar_segment.push(point),
            Err(error) => {
                normalized.extend(scalar_segment.nfc().collect::<String>().encode_utf16());
                scalar_segment.clear();
                normalized.push(error.unpaired_surrogate());
            }
        }
    }
    normalized.extend(scalar_segment.nfc().collect::<String>().encode_utf16());
    normalized
}
