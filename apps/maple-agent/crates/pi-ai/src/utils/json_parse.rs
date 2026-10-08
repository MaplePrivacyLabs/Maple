//! Pi's JSON repair and streaming parser.
//!
//! The fallback ports `partial-json` 0.1.7's default `Allow.ALL` path, including
//! its permissive object, array and exponent handling. Both parsers preserve
//! binary64 numbers and UTF-16 code units until a JSON serialization boundary.

use super::js_value::{JsObject, JsString, JsValue};

/// A complete JSON parse failed. The position counts UTF-16 code units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonParseError {
    pub position: usize,
    pub message: &'static str,
}

impl JsonParseError {
    /// The JavaScript error class exposed by `JSON.parse`.
    pub fn class_name(&self) -> &'static str {
        "SyntaxError"
    }
}

impl std::fmt::Display for JsonParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} at position {}", self.message, self.position)
    }
}

impl std::error::Error for JsonParseError {}

/// Repair raw controls and invalid backslash escapes inside JSON strings.
pub fn repair_json(json: &str) -> String {
    let repaired = repair_units(&json.encode_utf16().collect::<Vec<_>>());
    String::from_utf16(&repaired).expect("repair preserves scalar Unicode input")
}

/// Repair an input that may itself contain lone UTF-16 surrogates.
pub fn repair_json_utf16(json: &JsString) -> JsString {
    JsString::from_utf16(repair_units(&json.units().collect::<Vec<_>>()))
}

fn repair_units(json: &[u16]) -> Vec<u16> {
    let mut repaired = Vec::with_capacity(json.len());
    let mut in_string = false;
    let mut index = 0;
    while index < json.len() {
        let ch = json[index];
        if !in_string {
            repaired.push(ch);
            if ch == b'"' as u16 {
                in_string = true;
            }
        } else if ch == b'"' as u16 {
            repaired.push(ch);
            in_string = false;
        } else if ch == b'\\' as u16 {
            match json.get(index + 1).copied() {
                None => repaired.extend([b'\\' as u16, b'\\' as u16]),
                Some(next)
                    if next == b'u' as u16
                        && json.get(index + 2..index + 6).is_some_and(|digits| {
                            digits.iter().all(|digit| hex_digit(*digit).is_some())
                        }) =>
                {
                    repaired.extend(&json[index..index + 6]);
                    index += 5;
                }
                Some(next) if b"\"\\/bfnrtu".iter().any(|valid| *valid as u16 == next) => {
                    repaired.extend([b'\\' as u16, next]);
                    index += 1;
                }
                Some(_) => repaired.extend([b'\\' as u16, b'\\' as u16]),
            }
        } else {
            let escape = match ch {
                8 => Some("\\b"),
                12 => Some("\\f"),
                10 => Some("\\n"),
                13 => Some("\\r"),
                9 => Some("\\t"),
                _ => None,
            };
            if let Some(escape) = escape {
                repaired.extend(escape.encode_utf16());
            } else if ch <= 0x1f {
                repaired.extend(format!("\\u{ch:04x}").encode_utf16());
            } else {
                repaired.push(ch);
            }
        }
        index += 1;
    }
    repaired
}

/// Parse complete JSON with JavaScript's number and UTF-16 string semantics.
///
/// This strict boundary does not repair malformed JSON or accept partial input.
pub fn parse_json(json: &str) -> Result<JsValue, JsonParseError> {
    parse_complete(&json.encode_utf16().collect::<Vec<_>>())
}

/// The UTF-16 input counterpart of [`parse_json`].
pub fn parse_json_utf16(json: &JsString) -> Result<JsValue, JsonParseError> {
    parse_complete(&json.units().collect::<Vec<_>>())
}

/// Parse complete JSON, retrying only when Pi's string repair changes it.
pub fn parse_json_with_repair(json: &str) -> Result<JsValue, JsonParseError> {
    parse_with_repair_units(&json.encode_utf16().collect::<Vec<_>>())
}

/// The UTF-16 input counterpart of [`parse_json_with_repair`].
pub fn parse_json_with_repair_utf16(json: &JsString) -> Result<JsValue, JsonParseError> {
    parse_with_repair_units(&json.units().collect::<Vec<_>>())
}

fn parse_with_repair_units(json: &[u16]) -> Result<JsValue, JsonParseError> {
    match parse_complete(json) {
        Ok(value) => Ok(value),
        Err(error) => {
            let repaired = repair_units(json);
            if repaired != json {
                parse_complete(&repaired)
            } else {
                Err(error)
            }
        }
    }
}

/// Parse streamed JSON in Pi's original repair/fallback order.
///
/// Scalars and arrays are retained. Incomplete `nu` produces `{}`, whereas
/// complete `null` produces `null` because the first parse succeeds.
pub fn parse_streaming_json(partial_json: Option<&str>) -> JsValue {
    parse_streaming_units(
        partial_json
            .map(|json| json.encode_utf16().collect::<Vec<_>>())
            .as_deref(),
    )
}

/// The UTF-16 input counterpart of [`parse_streaming_json`].
pub fn parse_streaming_json_utf16(partial_json: Option<&JsString>) -> JsValue {
    parse_streaming_units(
        partial_json
            .map(|json| json.units().collect::<Vec<_>>())
            .as_deref(),
    )
}

fn parse_streaming_units(json: Option<&[u16]>) -> JsValue {
    let Some(json) = json.filter(|json| !js_trim(json).is_empty()) else {
        return empty_object();
    };
    if let Ok(value) = parse_with_repair_units(json) {
        return value;
    }
    if let Ok(value) = PartialParser::new(js_trim(json)).parse_any() {
        return if matches!(value, JsValue::Null) {
            empty_object()
        } else {
            value
        };
    }
    let repaired = repair_units(json);
    if let Ok(value) = PartialParser::new(js_trim(&repaired)).parse_any() {
        return if matches!(value, JsValue::Null) {
            empty_object()
        } else {
            value
        };
    }
    empty_object()
}

fn empty_object() -> JsValue {
    JsValue::Object(JsObject::new())
}

fn js_trim(value: &[u16]) -> &[u16] {
    let mut start = 0;
    let mut end = value.len();
    while start < end && js_whitespace(value[start]) {
        start += 1;
    }
    while end > start && js_whitespace(value[end - 1]) {
        end -= 1;
    }
    &value[start..end]
}

fn js_whitespace(ch: u16) -> bool {
    matches!(ch, 0x0009..=0x000d | 0x0020 | 0x00a0 | 0x1680 | 0x2000..=0x200a
        | 0x2028 | 0x2029 | 0x202f | 0x205f | 0x3000 | 0xfeff)
}

fn json_whitespace(ch: u16) -> bool {
    matches!(ch, 0x20 | 0x0a | 0x0d | 0x09)
}

fn hex_digit(ch: u16) -> Option<u16> {
    match ch {
        0x30..=0x39 => Some(ch - 0x30),
        0x41..=0x46 => Some(ch - 0x41 + 10),
        0x61..=0x66 => Some(ch - 0x61 + 10),
        _ => None,
    }
}

fn parse_complete(json: &[u16]) -> Result<JsValue, JsonParseError> {
    let mut parser = JsonParser {
        input: json,
        index: 0,
    };
    let value = parser.parse_value()?;
    parser.skip_blank();
    if parser.index != json.len() {
        return Err(parser.error("Unexpected non-whitespace character after JSON"));
    }
    Ok(value)
}

struct JsonParser<'a> {
    input: &'a [u16],
    index: usize,
}

impl JsonParser<'_> {
    fn current(&self) -> Option<u16> {
        self.input.get(self.index).copied()
    }
    fn error(&self, message: &'static str) -> JsonParseError {
        JsonParseError {
            position: self.index,
            message,
        }
    }
    fn skip_blank(&mut self) {
        while self.current().is_some_and(json_whitespace) {
            self.index += 1;
        }
    }
    fn consume(&mut self, ch: u8) -> bool {
        if self.current() == Some(ch as u16) {
            self.index += 1;
            true
        } else {
            false
        }
    }
    fn parse_value(&mut self) -> Result<JsValue, JsonParseError> {
        self.skip_blank();
        match self.current() {
            Some(0x22) => self.parse_string().map(JsValue::String),
            Some(0x7b) => self.parse_object(),
            Some(0x5b) => self.parse_array(),
            Some(0x74) => self.literal("true", JsValue::Bool(true)),
            Some(0x66) => self.literal("false", JsValue::Bool(false)),
            Some(0x6e) => self.literal("null", JsValue::Null),
            Some(0x2d | 0x30..=0x39) => self.parse_number(),
            Some(_) => Err(self.error("Unexpected token")),
            None => Err(self.error("Unexpected end of JSON input")),
        }
    }
    fn literal(&mut self, literal: &str, value: JsValue) -> Result<JsValue, JsonParseError> {
        for expected in literal.bytes() {
            if !self.consume(expected) {
                return Err(self.error("Unexpected token"));
            }
        }
        Ok(value)
    }
    fn parse_string(&mut self) -> Result<JsString, JsonParseError> {
        self.index += 1;
        let mut output = Vec::new();
        while let Some(ch) = self.current() {
            self.index += 1;
            match ch {
                0x22 => return Ok(JsString::from_utf16(output)),
                0x5c => {
                    let escaped = self
                        .current()
                        .ok_or_else(|| self.error("Unterminated string"))?;
                    self.index += 1;
                    match escaped {
                        0x22 | 0x5c | 0x2f => output.push(escaped),
                        0x62 => output.push(8),
                        0x66 => output.push(12),
                        0x6e => output.push(10),
                        0x72 => output.push(13),
                        0x74 => output.push(9),
                        0x75 => {
                            let mut unit = 0;
                            for _ in 0..4 {
                                let digit = self
                                    .current()
                                    .and_then(hex_digit)
                                    .ok_or_else(|| self.error("Bad Unicode escape"))?;
                                self.index += 1;
                                unit = unit * 16 + digit;
                            }
                            output.push(unit);
                        }
                        _ => return Err(self.error("Bad escaped character")),
                    }
                }
                0..=0x1f => return Err(self.error("Bad control character in string")),
                _ => output.push(ch),
            }
        }
        Err(self.error("Unterminated string"))
    }
    fn parse_number(&mut self) -> Result<JsValue, JsonParseError> {
        let start = self.index;
        self.consume(b'-');
        match self.current() {
            Some(0x30) => self.index += 1,
            Some(0x31..=0x39) => {
                self.index += 1;
                while matches!(self.current(), Some(0x30..=0x39)) {
                    self.index += 1;
                }
            }
            _ => return Err(self.error("No number after minus sign")),
        }
        if self.consume(b'.') {
            let fraction = self.index;
            while matches!(self.current(), Some(0x30..=0x39)) {
                self.index += 1;
            }
            if self.index == fraction {
                return Err(self.error("Unterminated fractional number"));
            }
        }
        if self.consume(b'e') || self.consume(b'E') {
            if !self.consume(b'+') {
                self.consume(b'-');
            }
            let exponent = self.index;
            while matches!(self.current(), Some(0x30..=0x39)) {
                self.index += 1;
            }
            if self.index == exponent {
                return Err(self.error("Exponent part is missing a number"));
            }
        }
        let token: String = self.input[start..self.index]
            .iter()
            .map(|ch| char::from_u32(*ch as u32).expect("number is ASCII"))
            .collect();
        token
            .parse::<f64>()
            .map(JsValue::Number)
            .map_err(|_| self.error("Invalid number"))
    }
    fn parse_object(&mut self) -> Result<JsValue, JsonParseError> {
        self.index += 1;
        self.skip_blank();
        let mut object = JsObject::new();
        if self.consume(b'}') {
            return Ok(JsValue::Object(object));
        }
        loop {
            if self.current() != Some(b'"' as u16) {
                return Err(self.error("Expected property name"));
            }
            let key = self.parse_string()?;
            self.skip_blank();
            if !self.consume(b':') {
                return Err(self.error("Expected colon after property name"));
            }
            let value = self.parse_value()?;
            object.insert(key, value);
            self.skip_blank();
            if self.consume(b'}') {
                return Ok(JsValue::Object(object));
            }
            if !self.consume(b',') {
                return Err(self.error("Expected comma or closing brace"));
            }
            self.skip_blank();
        }
    }
    fn parse_array(&mut self) -> Result<JsValue, JsonParseError> {
        self.index += 1;
        self.skip_blank();
        let mut array = Vec::new();
        if self.consume(b']') {
            return Ok(JsValue::Array(array));
        }
        loop {
            array.push(self.parse_value()?);
            self.skip_blank();
            if self.consume(b']') {
                return Ok(JsValue::Array(array));
            }
            if !self.consume(b',') {
                return Err(self.error("Expected comma or closing bracket"));
            }
        }
    }
}

struct PartialParser<'a> {
    input: &'a [u16],
    index: usize,
}

impl<'a> PartialParser<'a> {
    fn new(input: &'a [u16]) -> Self {
        Self { input, index: 0 }
    }
    fn current(&self) -> Option<u16> {
        self.input.get(self.index).copied()
    }
    // JavaScript substring clamps endpoints and swaps reversed endpoints.
    fn substring(&self, start: usize, end: usize) -> &[u16] {
        let start = start.min(self.input.len());
        let end = end.min(self.input.len());
        &self.input[start.min(end)..start.max(end)]
    }
    fn last_index_of(&self, needle: u8) -> usize {
        self.input
            .iter()
            .rposition(|ch| *ch == needle as u16)
            .unwrap_or(0)
    }
    fn skip_blank(&mut self) {
        while self.current().is_some_and(json_whitespace) {
            self.index += 1;
        }
    }
    fn parse_any(&mut self) -> Result<JsValue, ()> {
        self.skip_blank();
        match self.current().ok_or(())? {
            0x22 => return self.parse_string().map(JsValue::String),
            0x7b => return Ok(self.parse_object()),
            0x5b => return Ok(self.parse_array()),
            _ => {}
        }
        let remaining = self.substring(self.index, self.input.len());
        for atom in ["null", "true", "false", "Infinity", "-Infinity", "NaN"] {
            let units: Vec<_> = atom.encode_utf16().collect();
            let complete = remaining.starts_with(&units);
            let partial = remaining.len() < units.len()
                && units.starts_with(remaining)
                && (atom != "-Infinity" || remaining.len() > 1);
            if complete || partial {
                self.index += units.len();
                return Ok(match atom {
                    "true" => JsValue::Bool(true),
                    "false" => JsValue::Bool(false),
                    "Infinity" => JsValue::Number(f64::INFINITY),
                    "-Infinity" => JsValue::Number(f64::NEG_INFINITY),
                    "NaN" => JsValue::Number(f64::NAN),
                    _ => JsValue::Null,
                });
            }
        }
        self.parse_number()
    }
    fn parse_string(&mut self) -> Result<JsString, ()> {
        let start = self.index;
        let mut escape = false;
        self.index += 1;
        while self.index < self.input.len()
            && (self.current() != Some(0x22) || (escape && self.input[self.index - 1] == 0x5c))
        {
            escape = self.current() == Some(0x5c) && !escape;
            self.index += 1;
        }
        let parsed = if self.current() == Some(0x22) {
            self.index += 1;
            parse_complete(self.substring(start, self.index - usize::from(escape)))
        } else {
            let mut candidate = self
                .substring(start, self.index - usize::from(escape))
                .to_vec();
            candidate.push(0x22);
            parse_complete(&candidate).or_else(|_| {
                let mut truncated = self.substring(start, self.last_index_of(b'\\')).to_vec();
                truncated.push(0x22);
                parse_complete(&truncated)
            })
        };
        match parsed {
            Ok(JsValue::String(value)) => Ok(value),
            _ => Err(()),
        }
    }
    fn parse_object(&mut self) -> JsValue {
        self.index += 1;
        self.skip_blank();
        let mut object = JsObject::new();
        while self.current() != Some(0x7d) {
            self.skip_blank();
            if self.index >= self.input.len() {
                return JsValue::Object(object);
            }
            let Ok(key) = self.parse_string() else {
                return JsValue::Object(object);
            };
            self.skip_blank();
            self.index += 1; // Upstream does not check that this is a colon.
            let Ok(value) = self.parse_any() else {
                return JsValue::Object(object);
            };
            // Ordinary-object assignment invokes the inherited prototype
            // setter for this key; complete JSON.parse instead owns the key.
            if key.as_str() != Some("__proto__") {
                object.insert(key, value);
            }
            self.skip_blank();
            if self.current() == Some(0x2c) {
                self.index += 1;
            }
        }
        self.index += 1;
        JsValue::Object(object)
    }
    fn parse_array(&mut self) -> JsValue {
        self.index += 1;
        let mut array = Vec::new();
        while self.current() != Some(0x5d) {
            let Ok(value) = self.parse_any() else {
                return JsValue::Array(array);
            };
            array.push(value);
            self.skip_blank();
            if self.current() == Some(0x2c) {
                self.index += 1;
            }
        }
        self.index += 1;
        JsValue::Array(array)
    }
    fn parse_number(&mut self) -> Result<JsValue, ()> {
        if self.index == 0 {
            if self.input == [0x2d] {
                return Err(());
            }
            return parse_complete(self.input)
                .or_else(|_| parse_complete(self.substring(0, self.last_index_of(b'e'))))
                .map_err(|_| ());
        }
        let start = self.index;
        if self.current() == Some(0x2d) {
            self.index += 1;
        }
        while self
            .current()
            .is_some_and(|ch| !matches!(ch, 0x2c | 0x5d | 0x7d))
        {
            self.index += 1;
        }
        let candidate = self.substring(start, self.index);
        if let Ok(value) = parse_complete(candidate) {
            return Ok(value);
        }
        if candidate == [0x2d] {
            return Err(());
        }
        parse_complete(self.substring(start, self.last_index_of(b'e'))).map_err(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn streamed(json: &str) -> JsValue {
        parse_streaming_json(Some(json))
    }

    // Independently build finite scalar-Unicode expected values using serde.
    fn expected(json: &str) -> JsValue {
        fn convert(value: serde_json::Value) -> JsValue {
            match value {
                serde_json::Value::Null => JsValue::Null,
                serde_json::Value::Bool(value) => JsValue::Bool(value),
                serde_json::Value::Number(value) => JsValue::Number(value.as_f64().unwrap()),
                serde_json::Value::String(value) => JsValue::String(value.into()),
                serde_json::Value::Array(values) => {
                    JsValue::Array(values.into_iter().map(convert).collect())
                }
                serde_json::Value::Object(values) => JsValue::Object(
                    values
                        .into_iter()
                        .map(|(key, value)| (key, convert(value)))
                        .collect(),
                ),
            }
        }
        convert(serde_json::from_str(json).unwrap())
    }

    #[test]
    fn repair_preserves_valid_escapes_and_escapes_controls() {
        assert_eq!(
            repair_json("{\"x\":\"a\nb\t\\q\\u1234\\u12\"}"),
            "{\"x\":\"a\\nb\\t\\\\q\\u1234\\u12\"}"
        );
        assert_eq!(
            repair_json("\n\"\0\u{0008}\u{000c}\u{001f}🦀\"\n"),
            "\n\"\\u0000\\b\\f\\u001f🦀\"\n"
        );
        assert_eq!(repair_json("\"tail\\"), "\"tail\\\\");
    }

    #[test]
    fn parses_partial_atoms_and_distinguishes_complete_null() {
        for (input, result) in [
            ("tru", "true"),
            ("fal", "false"),
            ("nu", "{}"),
            ("null", "null"),
            ("[nu,tru,fal]", "[]"),
            ("[nu", "[null]"),
            ("[1,tru", "[1,true]"),
        ] {
            assert_eq!(streamed(input), expected(result), "{input}");
        }
    }

    #[test]
    fn retains_partial_nested_structure_and_truncates_unfinished_keys() {
        for (input, result) in [
            (
                "[{\"key1\":\"value1\",\"key2\":[\"value2",
                r#"[{"key1":"value1","key2":["value2"]}]"#,
            ),
            ("{\"a\":1,\"b\":", r#"{"a":1}"#),
            ("{\"a\":1,\"incomplete", r#"{"a":1}"#),
            ("[1,2,", "[1,2]"),
        ] {
            assert_eq!(streamed(input), expected(result), "{input}");
        }
    }

    #[test]
    fn keeps_upstream_exponent_and_string_truncation_behavior() {
        for (input, result) in [
            ("1e+", "1"),
            ("1E+", "{}"),
            ("123.", "{}"),
            ("[12,3e-", "[12,3]"),
            ("\"hello \\u12", r#""hello ""#),
            ("\"hello \\", r#""hello ""#),
            ("\"line\nraw", r#""line\nraw""#),
        ] {
            assert_eq!(streamed(input), expected(result), "{input}");
        }
    }

    #[test]
    fn applies_complete_repair_before_partial_parsing() {
        for (input, result) in [
            ("{\"path\":\"C:\\q\"}", r#"{"path":"C:\\q"}"#),
            ("\"a\\q", r#""a""#),
            ("{\"a\":1, nonsense", r#"{"a":1}"#),
            ("true trailing", "true"),
        ] {
            assert_eq!(streamed(input), expected(result), "{input}");
        }
    }

    #[test]
    fn preserves_binary64_rounding_negative_zero_and_nonfinite_atoms() {
        assert_eq!(
            streamed("9007199254740993"),
            JsValue::Number(9007199254740992.0)
        );
        for input in ["1e400", "Inf", "Infinity"] {
            assert_eq!(streamed(input), JsValue::Number(f64::INFINITY));
        }
        for input in ["-1e400", "-Inf"] {
            assert_eq!(streamed(input), JsValue::Number(f64::NEG_INFINITY));
        }
        assert!(matches!(streamed("Na"), JsValue::Number(number) if number.is_nan()));
        assert!(
            matches!(streamed("-0"), JsValue::Number(number) if number == 0.0 && number.is_sign_negative())
        );
        assert_eq!(
            streamed("[1e400,-1e400]"),
            JsValue::Array(vec![
                JsValue::Number(f64::INFINITY),
                JsValue::Number(f64::NEG_INFINITY)
            ])
        );
    }

    #[test]
    fn preserves_lone_surrogates_in_values_keys_and_partial_strings() {
        let high = JsString::from_utf16(vec![0xd800]);
        let low = JsString::from_utf16(vec![0xdc00]);
        assert_eq!(streamed(r#""\ud800""#), JsValue::String(high.clone()));
        assert_eq!(streamed(r#""\udc00""#), JsValue::String(low.clone()));
        assert_eq!(streamed(r#""\ud800"#), JsValue::String(high.clone()));
        assert_eq!(streamed(r#""\ud800\u12"#), JsValue::String(high.clone()));
        let mut object = JsObject::new();
        object.insert(high, JsValue::String(low));
        assert_eq!(
            streamed(r#"{"\ud800":"\udc00"}"#),
            JsValue::Object(object.clone())
        );
        assert_eq!(streamed(r#"{"\ud800":"\udc00"#), JsValue::Object(object));
        assert_eq!(streamed(r#""\ud83e\udd80""#), JsValue::String("🦀".into()));
    }

    #[test]
    fn preserves_raw_surrogates_on_utf16_input_path() {
        let input = JsString::from_utf16(vec![0x22, 0xd800, 0x22]);
        let result = JsValue::String(JsString::from_utf16(vec![0xd800]));
        assert_eq!(parse_json_with_repair_utf16(&input).unwrap(), result);
        assert_eq!(parse_streaming_json_utf16(Some(&input)), result);
        let input = JsString::from_utf16(vec![0x22, 0xd800, 0x0a]);
        // partial-json trims the trailing raw newline before recovering the string.
        let result = JsValue::String(JsString::from_utf16(vec![0xd800]));
        assert_eq!(parse_streaming_json_utf16(Some(&input)), result);
        assert_eq!(
            repair_json_utf16(&input),
            JsString::from_utf16(vec![0x22, 0xd800, 0x5c, 0x6e])
        );
    }

    #[test]
    fn prototype_setter_applies_only_on_partial_object_path() {
        assert_eq!(
            streamed(r#"{"__proto__":{},"a":1,"#),
            expected(r#"{"a":1}"#)
        );
        assert_eq!(
            streamed(r#"{"__proto__":{}}"#),
            expected(r#"{"__proto__":{}}"#)
        );
    }

    #[test]
    fn complete_parser_requires_json_grammar() {
        for input in [
            "01",
            "1.",
            "1e",
            "+1",
            "NaN",
            "Infinity",
            "true false",
            "[1,]",
            "{\"a\":1,}",
            "{\"a\" 1}",
            "[1 2]",
            "\u{feff}null",
            "\"\\u123\"",
            "\"\\q\"",
        ] {
            let units: Vec<_> = input.encode_utf16().collect();
            assert_eq!(
                parse_complete(&units).unwrap_err().class_name(),
                "SyntaxError",
                "{input}"
            );
        }
    }

    #[test]
    fn empty_and_malformed_inputs_fall_back_to_empty_object() {
        for input in [
            None,
            Some(""),
            Some(" \n\t"),
            Some("\u{feff}"),
            Some("wrong"),
            Some("-"),
        ] {
            assert_eq!(parse_streaming_json(input), empty_object());
        }
    }
}
