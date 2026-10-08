//! Field access for persisted messages that do not satisfy the typed shape.
//! These views never rewrite the stored object.
use super::js_json::{js_object_entries, number_to_string};
use crate::types::{JsObject, JsString, JsValue};

pub(crate) fn property<'a>(
    value: Option<&'a JsValue>,
    key: &str,
) -> Result<Option<&'a JsValue>, JsString> {
    match value {
        None => Err(format!("Cannot read properties of undefined (reading '{key}')").into()),
        Some(JsValue::Null) => {
            Err(format!("Cannot read properties of null (reading '{key}')").into())
        }
        Some(value) => Ok(value.get(key)),
    }
}
pub fn string(value: Option<&JsValue>) -> JsString {
    match value {
        None => "undefined".into(),
        Some(JsValue::Null) => "null".into(),
        Some(JsValue::Bool(value)) => value.to_string().into(),
        Some(JsValue::Number(value)) => number_to_string(*value).into(),
        Some(JsValue::String(value)) => value.clone(),
        Some(JsValue::Array(values)) => join_values(values.iter().map(Some), ","),
        Some(JsValue::Object(_)) => "[object Object]".into(),
    }
}
pub fn join_values<'a>(
    values: impl IntoIterator<Item = Option<&'a JsValue>>,
    separator: &str,
) -> JsString {
    let parts: Vec<_> = values
        .into_iter()
        .map(|value| match value {
            None | Some(JsValue::Null) => JsString::default(),
            value => string(value),
        })
        .collect();
    JsString::join(parts.iter(), separator)
}
pub(crate) fn array<'a>(
    value: Option<&'a JsValue>,
    expression: &str,
    method: &str,
) -> Result<&'a [JsValue], JsString> {
    property(value, method)?;
    value
        .and_then(JsValue::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| format!("{expression}.{method} is not a function").into())
}
pub(crate) fn content_text(
    content: Option<&JsValue>,
    separator: &str,
) -> Result<JsString, JsString> {
    if let Some(JsValue::String(text)) = content {
        return Ok(text.clone());
    }
    let blocks = array(content, "content", "filter")?;
    let mut parts = Vec::new();
    for block in blocks {
        if property(Some(block), "type")?.and_then(JsValue::as_str) == Some("text") {
            parts.push(block.get("text"));
        }
    }
    Ok(join_values(parts, separator))
}
pub(crate) fn entries(value: Option<&JsValue>) -> Vec<(JsString, JsValue)> {
    match value {
        Some(JsValue::Object(value)) => js_object_entries(value)
            .into_iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        Some(JsValue::Array(value)) => value
            .iter()
            .enumerate()
            .map(|(i, v)| (i.to_string().into(), v.clone()))
            .collect(),
        Some(JsValue::String(value)) => value
            .units()
            .enumerate()
            .map(|(i, v)| {
                (
                    i.to_string().into(),
                    JsValue::String(JsString::from_utf16(vec![v])),
                )
            })
            .collect(),
        _ => Vec::new(),
    }
}
pub(crate) fn system_text(message: &JsObject) -> Result<JsString, JsString> {
    let mut parts = vec![JsValue::String(content_text(message.get("content"), "\n")?)];
    parts.extend(
        entries(message.get("sections"))
            .into_iter()
            .filter_map(|(_, v)| (!v.is_null()).then_some(v)),
    );
    let mut included = Vec::new();
    for part in &parts {
        if length(Some(part))? > 0.0 {
            included.push(Some(part));
        }
    }
    Ok(join_values(included, "\n\n"))
}
pub(crate) fn length(value: Option<&JsValue>) -> Result<f64, JsString> {
    property(value, "length")?;
    Ok(match value {
        Some(JsValue::String(value)) => value.utf16_len() as f64,
        Some(JsValue::Array(value)) => value.len() as f64,
        Some(JsValue::Object(value)) => value
            .get("length")
            .and_then(JsValue::as_f64)
            .unwrap_or(f64::NAN),
        _ => f64::NAN,
    })
}

/// JavaScript numeric coercion used by timestamp comparisons and Math.max.
pub fn number(value: Option<&JsValue>) -> f64 {
    match value {
        None => f64::NAN,
        Some(JsValue::Null) => 0.0,
        Some(JsValue::Bool(value)) => {
            if *value {
                1.0
            } else {
                0.0
            }
        }
        Some(JsValue::Number(value)) => *value,
        value => {
            let text = string(value);
            let Some(text) = text.as_str() else {
                return f64::NAN;
            };
            let text = text.trim_matches(|c: char| matches!(c as u32, 0x0009..=0x000d | 0x0020 | 0x00a0 | 0x1680 | 0x2000..=0x200a | 0x2028 | 0x2029 | 0x202f | 0x205f | 0x3000 | 0xfeff));
            if text.is_empty() {
                return 0.0;
            }
            for (prefix, base) in [
                ("0x", 16),
                ("0X", 16),
                ("0o", 8),
                ("0O", 8),
                ("0b", 2),
                ("0B", 2),
            ] {
                if let Some(digits) = text.strip_prefix(prefix) {
                    if digits.is_empty() {
                        return f64::NAN;
                    }
                    return digits
                        .chars()
                        .try_fold(0.0, |number, c| {
                            c.to_digit(base)
                                .map(|digit| number * base as f64 + digit as f64)
                        })
                        .unwrap_or(f64::NAN);
                }
            }
            match text {
                "Infinity" | "+Infinity" => f64::INFINITY,
                "-Infinity" => f64::NEG_INFINITY,
                _ if text.contains(['i', 'I', 'n', 'N']) => f64::NAN,
                _ => text.parse().unwrap_or(f64::NAN),
            }
        }
    }
}
