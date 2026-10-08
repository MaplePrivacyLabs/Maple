//! ECMAScript JSON formatting for model-visible strings.
//!
//! Serialization is the explicit observation boundary: nonfinite numbers become
//! null and lone UTF-16 surrogates are escaped, just as `JSON.stringify` does.
use super::js_value::{JsObject, JsString, JsValue, JsonConversionError, to_js_value};
use serde::Serialize;
use serde_json::{Map, Value};

fn array_index(key: &str) -> Option<u32> {
    let index = key.parse::<u32>().ok()?;
    (index != u32::MAX && index.to_string() == key).then_some(index)
}

pub fn ordered_keys<'a>(keys: impl IntoIterator<Item = &'a String>) -> Vec<&'a String> {
    let mut keys: Vec<_> = keys.into_iter().collect();
    keys.sort_by_key(|key| array_index(key).map_or((1, 0), |index| (0, index)));
    keys
}

pub fn ordered_js_keys<'a>(keys: impl IntoIterator<Item = &'a JsString>) -> Vec<&'a JsString> {
    let mut keys: Vec<_> = keys.into_iter().collect();
    keys.sort_by_key(|key| {
        key.as_str()
            .and_then(array_index)
            .map_or((1, 0), |index| (0, index))
    });
    keys
}

pub fn object_entries(object: &Map<String, Value>) -> Vec<(&String, &Value)> {
    ordered_keys(object.keys())
        .into_iter()
        .map(|key| (key, &object[key]))
        .collect()
}

pub fn js_object_entries(object: &JsObject) -> Vec<(&JsString, &JsValue)> {
    ordered_js_keys(object.keys())
        .into_iter()
        .map(|key| (key, &object[key]))
        .collect()
}

/// Values that can be observed with ECMAScript JSON serialization.
/// Ordinary serde values are supported for schema/host metadata; runtime data
/// uses JsValue so no conversion precedes validation or observation.
pub trait JsJson {
    #[doc(hidden)]
    fn write_json(&self, indent: &str, depth: usize, output: &mut String);
}

pub fn stringify<T: JsJson + ?Sized>(value: &T) -> String {
    stringify_with_indent(value, "")
}

pub fn stringify_pretty<T: JsJson + ?Sized>(value: &T, spaces: usize) -> String {
    stringify_with_indent(value, &" ".repeat(spaces.min(10)))
}

/// JavaScript also accepts an indentation string, truncated to ten UTF-16
/// units. That truncation may split a surrogate pair, so the result is JsString.
pub fn stringify_with_space<T: JsJson + ?Sized>(value: &T, space: &JsString) -> JsString {
    if space.utf16_len() == 0 {
        return stringify(value).into();
    }
    // The pinned Node 22 serializer treats the gap as NUL-terminated while
    // keeping pretty-print line breaks and colon spacing for a nonempty gap.
    let indent: Vec<_> = space
        .units()
        .take(10)
        .take_while(|unit| *unit != 0)
        .collect();
    // Literal NUL cannot occur in serialized keys or values: write_units
    // escapes it. It therefore marks indentation positions without collisions.
    let formatted = stringify_with_indent(value, "\0");
    let mut units = Vec::new();
    for (index, part) in formatted.split('\0').enumerate() {
        if index != 0 {
            units.extend_from_slice(&indent);
        }
        units.extend(part.encode_utf16());
    }
    JsString::from_utf16(units)
}

pub fn stringify_serializable<T: Serialize + ?Sized>(
    value: &T,
) -> Result<String, JsonConversionError> {
    to_js_value(value).map(|value| stringify(&value))
}

fn stringify_with_indent<T: JsJson + ?Sized>(value: &T, indent: &str) -> String {
    let mut output = String::new();
    value.write_json(indent, 0, &mut output);
    output
}

/// `Number.prototype.toString` / JavaScript numeric string coercion.
/// Unlike JSON serialization this returns `NaN` and signed `Infinity`.
pub fn number_to_string(number: f64) -> String {
    ryu_js::Buffer::new().format(number).to_owned()
}

fn write_number(number: f64, output: &mut String) {
    if number.is_finite() {
        output.push_str(ryu_js::Buffer::new().format(number));
    } else {
        output.push_str("null");
    }
}

impl JsJson for Value {
    fn write_json(&self, indent: &str, depth: usize, output: &mut String) {
        match self {
            Self::Null => output.push_str("null"),
            Self::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
            Self::Number(value) => {
                write_number(value.as_f64().expect("JSON number fits binary64"), output)
            }
            Self::String(value) => write_units(value.encode_utf16(), output),
            Self::Array(values) => write_array(values, indent, depth, output),
            Self::Object(values) => {
                output.push('{');
                for (index, (key, value)) in object_entries(values).into_iter().enumerate() {
                    write_entry_prefix(index, indent, depth + 1, output);
                    write_units(key.encode_utf16(), output);
                    write_colon(indent, output);
                    value.write_json(indent, depth + 1, output);
                }
                write_collection_end(values.is_empty(), indent, depth, '}', output);
            }
        }
    }
}

impl JsJson for JsValue {
    fn write_json(&self, indent: &str, depth: usize, output: &mut String) {
        match self {
            Self::Null => output.push_str("null"),
            Self::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
            Self::Number(value) => write_number(*value, output),
            Self::String(value) => write_units(value.units(), output),
            Self::Array(values) => write_array(values, indent, depth, output),
            Self::Object(values) => values.write_json(indent, depth, output),
        }
    }
}

impl JsJson for JsObject {
    fn write_json(&self, indent: &str, depth: usize, output: &mut String) {
        output.push('{');
        for (index, (key, value)) in js_object_entries(self).into_iter().enumerate() {
            write_entry_prefix(index, indent, depth + 1, output);
            write_units(key.units(), output);
            write_colon(indent, output);
            value.write_json(indent, depth + 1, output);
        }
        write_collection_end(self.is_empty(), indent, depth, '}', output);
    }
}

impl JsJson for JsString {
    fn write_json(&self, _: &str, _: usize, output: &mut String) {
        write_units(self.units(), output);
    }
}

fn write_array<T: JsJson>(values: &[T], indent: &str, depth: usize, output: &mut String) {
    output.push('[');
    for (index, value) in values.iter().enumerate() {
        write_entry_prefix(index, indent, depth + 1, output);
        value.write_json(indent, depth + 1, output);
    }
    write_collection_end(values.is_empty(), indent, depth, ']', output);
}

fn write_colon(indent: &str, output: &mut String) {
    output.push(':');
    if !indent.is_empty() {
        output.push(' ');
    }
}

fn write_entry_prefix(index: usize, indent: &str, depth: usize, output: &mut String) {
    if index != 0 {
        output.push(',');
    }
    if !indent.is_empty() {
        output.push('\n');
        output.push_str(&indent.repeat(depth));
    }
}

fn write_collection_end(empty: bool, indent: &str, depth: usize, end: char, output: &mut String) {
    if !empty && !indent.is_empty() {
        output.push('\n');
        output.push_str(&indent.repeat(depth));
    }
    output.push(end);
}

/// Quote a JavaScript string without replacing unpaired UTF-16 surrogates.
pub fn quote(value: &JsString) -> String {
    let mut output = String::new();
    write_units(value.units(), &mut output);
    output
}

fn write_units(units: impl IntoIterator<Item = u16>, output: &mut String) {
    use std::fmt::Write;
    let mut units = units.into_iter().peekable();
    output.push('"');
    while let Some(unit) = units.next() {
        match unit {
            0x22 => output.push_str("\\\""),
            0x5c => output.push_str("\\\\"),
            0x08 => output.push_str("\\b"),
            0x0c => output.push_str("\\f"),
            0x0a => output.push_str("\\n"),
            0x0d => output.push_str("\\r"),
            0x09 => output.push_str("\\t"),
            0..=0x1f => {
                write!(output, "\\u{unit:04x}").expect("writing to String cannot fail");
            }
            0xd800..=0xdbff
                if units
                    .peek()
                    .is_some_and(|unit| (0xdc00..=0xdfff).contains(unit)) =>
            {
                let low = units.next().expect("low surrogate checked");
                let scalar =
                    0x10000 + ((u32::from(unit) - 0xd800) << 10) + (u32::from(low) - 0xdc00);
                output.push(char::from_u32(scalar).expect("paired UTF-16 encodes a scalar"));
            }
            0xd800..=0xdfff => {
                write!(output, "\\u{unit:04x}").expect("writing to String cannot fail");
            }
            _ => {
                output.push(char::from_u32(u32::from(unit)).expect("non-surrogate u16 is a scalar"))
            }
        }
    }
    output.push('"');
}

pub trait Utf16Length {
    fn utf16_length(&self) -> usize;
}
impl Utf16Length for str {
    fn utf16_length(&self) -> usize {
        self.encode_utf16().count()
    }
}
impl Utf16Length for String {
    fn utf16_length(&self) -> usize {
        self.as_str().utf16_length()
    }
}
impl Utf16Length for JsString {
    fn utf16_length(&self) -> usize {
        self.utf16_len()
    }
}
pub fn utf16_len<T: Utf16Length + ?Sized>(value: &T) -> usize {
    value.utf16_length()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn key_order_observes_array_index_limits_and_insertion_order() {
        let value: Value = serde_json::from_str(
            r#"{"10":1,"02":2,"2":3,"4294967295":4,"0":5,"4294967294":6,"-0":7,"a":8}"#,
        )
        .unwrap();
        assert_eq!(
            stringify(&value),
            r#"{"0":5,"2":3,"10":1,"4294967294":6,"02":2,"4294967295":4,"-0":7,"a":8}"#
        );
        let value = JsValue::try_from(value).unwrap();
        assert_eq!(
            stringify(&value),
            r#"{"0":5,"2":3,"10":1,"4294967294":6,"02":2,"4294967295":4,"-0":7,"a":8}"#
        );
    }

    #[test]
    fn numbers_have_javascript_rounding_and_exponent_boundaries() {
        assert_eq!(
            stringify(&json!([-0.0, 1e-7, 1e-6, 1e20, 1e21, 9007199254740993_u64])),
            "[0,1e-7,0.000001,100000000000000000000,1e+21,9007199254740992]"
        );
        assert_eq!(
            stringify(&JsValue::Array(vec![
                JsValue::Number(f64::NAN),
                JsValue::Number(f64::INFINITY),
                JsValue::Number(f64::NEG_INFINITY)
            ])),
            "[null,null,null]"
        );
        assert_eq!(number_to_string(f64::INFINITY), "Infinity");
        assert_eq!(number_to_string(f64::NAN), "NaN");
    }

    #[test]
    fn strings_preserve_unicode_and_escape_only_json_controls() {
        assert_eq!(
            stringify(&json!("🙈\u{2028}\u{2029}/\0\u{001f}\n\"\\")),
            "\"🙈\u{2028}\u{2029}/\\u0000\\u001f\\n\\\"\\\\\""
        );
        assert_eq!(utf16_len("a🙈é"), 4);
        let raw = JsString::from_utf16(vec![0xd800, 0x61, 0xdc00, 0xd83d, 0xde48]);
        assert_eq!(quote(&raw), r#""\ud800a\udc00🙈""#);
        let object = JsObject::from([(raw, JsValue::String(JsString::from_utf16(vec![0xdc01])))]);
        assert_eq!(stringify(&object), r#"{"\ud800a\udc00🙈":"\udc01"}"#);
    }

    #[test]
    fn pretty_format_preserves_empty_containers_and_indentation() {
        assert_eq!(
            stringify_pretty(&json!({"a": [1, {}], "b": []}), 2),
            "{\n  \"a\": [\n    1,\n    {}\n  ],\n  \"b\": []\n}"
        );
        assert_eq!(stringify_pretty(&json!([1]), 99), "[\n          1\n]");
    }
}
