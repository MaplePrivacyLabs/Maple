//! Port of `packages/ai/src/utils/diagnostics.ts`.

use super::js_value::{JsObject, JsString, JsValue};
use crate::env::PiEnv;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DiagnosticCode {
    String(JsString),
    Number(f64),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticErrorInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<JsString>,
    pub message: JsString,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack: Option<JsString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<DiagnosticCode>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessageDiagnostic {
    #[serde(rename = "type")]
    pub r#type: String,
    pub timestamp: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<DiagnosticErrorInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<JsObject>,
}

/// Rust's explicit counterpart to JavaScript Error versus arbitrary thrown values.
#[derive(Clone, Debug, PartialEq)]
pub enum ThrownValue {
    Error {
        name: JsString,
        message: JsString,
        stack: Option<JsString>,
        code: Option<JsValue>,
    },
    Value(JsValue),
    Undefined,
}

impl ThrownValue {
    pub fn error(name: impl Into<JsString>, message: impl Into<JsString>) -> Self {
        Self::Error {
            name: name.into(),
            message: message.into(),
            stack: None,
            code: None,
        }
    }
}

pub fn format_thrown_value(value: &ThrownValue) -> JsString {
    match value {
        ThrownValue::Error { name, message, .. } => {
            if message.is_empty() {
                name.clone()
            } else {
                message.clone()
            }
        }
        ThrownValue::Value(value) => string_coercion(value),
        ThrownValue::Undefined => "undefined".into(),
    }
}

fn string_coercion(value: &JsValue) -> JsString {
    match value {
        JsValue::Null => "null".into(),
        JsValue::String(value) => value.clone(),
        JsValue::Bool(value) => value.to_string().into(),
        JsValue::Number(value) => super::js_json::number_to_string(*value).into(),
        JsValue::Object(_) => "[object Object]".into(),
        JsValue::Array(values) => JsString::join(
            values
                .iter()
                .map(|value| match value {
                    JsValue::Null => JsString::default(),
                    _ => string_coercion(value),
                })
                .collect::<Vec<_>>()
                .iter(),
            ",",
        ),
    }
}

pub fn extract_diagnostic_error(error: &ThrownValue) -> DiagnosticErrorInfo {
    match error {
        ThrownValue::Error {
            name, stack, code, ..
        } => DiagnosticErrorInfo {
            name: (!name.is_empty()).then(|| name.clone()),
            message: format_thrown_value(error),
            stack: stack.clone(),
            code: match code {
                Some(JsValue::String(code)) => Some(DiagnosticCode::String(code.clone())),
                Some(JsValue::Number(code)) => Some(DiagnosticCode::Number(*code)),
                _ => None,
            },
        },
        _ => DiagnosticErrorInfo {
            name: Some("ThrownValue".into()),
            message: format_thrown_value(error),
            stack: None,
            code: None,
        },
    }
}

pub fn create_assistant_message_diagnostic(
    kind: impl Into<String>,
    error: &ThrownValue,
    details: Option<JsObject>,
    env: &dyn PiEnv,
) -> AssistantMessageDiagnostic {
    AssistantMessageDiagnostic {
        r#type: kind.into(),
        timestamp: env.now_ms() as f64,
        error: Some(extract_diagnostic_error(error)),
        details,
    }
}

/// The Rust field-level counterpart to Pi's generic object's diagnostics property.
pub fn append_assistant_message_diagnostic(
    diagnostics: &mut Option<Vec<AssistantMessageDiagnostic>>,
    diagnostic: AssistantMessageDiagnostic,
) {
    diagnostics.get_or_insert_with(Vec::new).push(diagnostic);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thrown_values_keep_javascript_string_coercion_and_utf16() {
        assert_eq!(
            format_thrown_value(&ThrownValue::Value(JsValue::Number(f64::INFINITY))),
            "Infinity"
        );
        assert_eq!(
            format_thrown_value(&ThrownValue::Value(JsValue::Number(f64::NAN))),
            "NaN"
        );
        let raw = JsString::from_utf16(vec![0xd800]);
        assert_eq!(
            format_thrown_value(&ThrownValue::Value(JsValue::String(raw.clone()))),
            raw
        );
        assert_eq!(
            format_thrown_value(&ThrownValue::Value(JsValue::Array(vec![
                JsValue::Null,
                JsValue::String(raw.clone()),
                JsValue::Array(vec![JsValue::Bool(true)])
            ])))
            .as_utf16(),
            vec![0x2c, 0xd800, 0x2c, 0x74, 0x72, 0x75, 0x65]
        );
        let error = ThrownValue::Error {
            name: "Error".into(),
            message: raw.clone(),
            stack: None,
            code: Some(JsValue::Number(f64::INFINITY)),
        };
        let extracted = extract_diagnostic_error(&error);
        assert_eq!(extracted.message, raw);
        assert!(
            matches!(extracted.code, Some(DiagnosticCode::Number(number)) if number == f64::INFINITY)
        );
    }
}
