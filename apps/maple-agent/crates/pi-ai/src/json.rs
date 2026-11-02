use serde_json::{Map, Value};

/// Parse tool-call arguments that may have been cut off mid-stream.
///
/// Complete JSON parses normally. Strings with raw control characters or backslashes
/// that start no valid escape are repaired. Truncated JSON is closed off at the last
/// complete value: an unfinished string is closed, a dangling key is dropped, a partial
/// literal is completed and open containers are closed. Anything that still does not
/// parse, or is not an object, yields an empty object.
pub fn parse_streaming_json(raw: &str) -> Map<String, Value> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Map::new();
    }
    let parse = |json: &str| match serde_json::from_str(json) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    };
    let strings_repaired = repair_strings(trimmed);
    let changed = strings_repaired != trimmed;
    parse(trimmed)
        .or_else(|| changed.then(|| parse(&strings_repaired)).flatten())
        .or_else(|| parse(&repair(trimmed)))
        .or_else(|| changed.then(|| parse(&repair(&strings_repaired))).flatten())
        .unwrap_or_default()
}

/// Parse the complete argument text of a finished tool call, as Pi's
/// `parseToolCallArguments`. Unlike [`parse_streaming_json`], text that is not a JSON
/// object, even after its strings are repaired, is not cut back to a valid prefix: the
/// error describes it and includes the text, so validation can show the model what it
/// sent.
pub fn parse_tool_call_arguments(raw: &str) -> Result<Map<String, Value>, String> {
    if raw.trim().is_empty() {
        return Ok(Map::new());
    }
    let parsed = serde_json::from_str::<Value>(raw).or_else(|error| {
        let repaired = repair_strings(raw);
        if repaired == raw {
            return Err(error);
        }
        serde_json::from_str::<Value>(&repaired)
    });
    match parsed {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(format!(
            "Tool arguments must be a JSON object\n\nReceived arguments:\n{raw}"
        )),
        Err(error) => Err(format!("{error}\n\nReceived arguments:\n{raw}")),
    }
}

/// Escape what JSON does not allow inside strings: raw control characters, and
/// backslashes that start no valid escape.
fn repair_strings(json: &str) -> String {
    let mut repaired = String::with_capacity(json.len());
    let mut in_string = false;
    let mut chars = json.char_indices().peekable();
    while let Some((offset, ch)) = chars.next() {
        if !in_string {
            repaired.push(ch);
            in_string = ch == '"';
            continue;
        }
        match ch {
            '"' => {
                repaired.push(ch);
                in_string = false;
            }
            '\\' => match chars.peek().map(|(_, next)| *next) {
                Some('u')
                    if json
                        .get(offset + 2..offset + 6)
                        .is_some_and(|digits| digits.bytes().all(|b| b.is_ascii_hexdigit())) =>
                {
                    repaired.push_str(&json[offset..offset + 6]);
                    chars.nth(4);
                }
                // `\u` without four hex digits, as in `\usepackage`, is no escape either.
                Some(next @ ('"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't')) => {
                    repaired.push('\\');
                    repaired.push(next);
                    chars.next();
                }
                // Any other backslash stands for itself.
                _ => repaired.push_str("\\\\"),
            },
            '\u{8}' => repaired.push_str("\\b"),
            '\u{c}' => repaired.push_str("\\f"),
            '\n' => repaired.push_str("\\n"),
            '\r' => repaired.push_str("\\r"),
            '\t' => repaired.push_str("\\t"),
            control if control < ' ' => {
                repaired.push_str(&format!("\\u{:04x}", u32::from(control)));
            }
            other => repaired.push(other),
        }
    }
    repaired
}

#[derive(Clone, Copy, PartialEq)]
enum Frame {
    Object,
    Array,
}

fn repair(input: &str) -> String {
    let mut out = String::from(input);
    let mut stack = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    let mut string_start = 0;
    let mut string_is_key = false;
    // In an object, whether the next string is a key (after `{` or `,`).
    let mut expecting_key = false;
    // The byte offset of a complete key still waiting for its `:`.
    let mut pending_key: Option<usize> = None;

    for (offset, ch) in input.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
                if string_is_key {
                    pending_key = Some(string_start);
                }
            }
            continue;
        }
        match ch {
            '"' => {
                in_string = true;
                string_start = offset;
                string_is_key = stack.last() == Some(&Frame::Object) && expecting_key;
            }
            '{' => {
                stack.push(Frame::Object);
                expecting_key = true;
            }
            '[' => {
                stack.push(Frame::Array);
                expecting_key = false;
            }
            '}' | ']' => {
                stack.pop();
                expecting_key = false;
            }
            ',' => expecting_key = stack.last() == Some(&Frame::Object),
            ':' => {
                expecting_key = false;
                pending_key = None;
            }
            _ => {}
        }
    }

    if in_string {
        if string_is_key {
            out.truncate(string_start);
        } else {
            drop_partial_escape(&mut out, string_start);
            out.push('"');
        }
    } else if let Some(key_start) = pending_key {
        out.truncate(key_start);
    }

    loop {
        let before = out.len();
        let trimmed_len = out.trim_end().len();
        out.truncate(trimmed_len);
        if out.ends_with(',') {
            out.pop();
        } else if out.ends_with(':') {
            out.push_str("null");
        } else {
            complete_literal(&mut out);
            strip_partial_number(&mut out);
        }
        if out.len() == before {
            break;
        }
    }

    for frame in stack.iter().rev() {
        out.push(match frame {
            Frame::Object => '}',
            Frame::Array => ']',
        });
    }
    out
}

/// Drop a trailing backslash or an incomplete `\u` escape from an unterminated string.
fn drop_partial_escape(out: &mut String, string_start: usize) {
    let body = &out[string_start + 1..];
    if let Some(slash) = body.rfind('\\') {
        let escape = &body[slash..];
        let complete = match escape.chars().nth(1) {
            None => false,
            Some('u') => escape.len() >= 6,
            Some(_) => true,
        };
        // A backslash that is itself escaped (`\\`) ends a complete escape.
        let preceding = body[..slash]
            .chars()
            .rev()
            .take_while(|c| *c == '\\')
            .count();
        if !complete && preceding % 2 == 0 {
            out.truncate(string_start + 1 + slash);
        }
    }
}

fn complete_literal(out: &mut String) {
    for literal in ["true", "false", "null"] {
        for len in 1..literal.len() {
            let prefix = &literal[..len];
            if out.ends_with(prefix) {
                let before = out[..out.len() - len].chars().next_back();
                if matches!(before, Some(':' | ',' | '[' | ' ' | '\n' | '\t' | '\r')) {
                    out.push_str(&literal[len..]);
                    return;
                }
            }
        }
    }
}

fn strip_partial_number(out: &mut String) {
    while out.ends_with(['-', '+', '.', 'e', 'E']) {
        let before = out[..out.len() - 1].chars().next_back();
        // `e`/`E` only belong to a number when a digit precedes them.
        if out.ends_with(['e', 'E']) && !before.is_some_and(|c| c.is_ascii_digit()) {
            break;
        }
        out.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(raw: &str) -> Value {
        Value::Object(parse_streaming_json(raw))
    }

    #[test]
    fn complete_json_parses_as_is() {
        assert_eq!(
            parse(r#"{"a": [1, {"b": "c"}]}"#),
            json!({"a": [1, {"b": "c"}]})
        );
    }

    #[test]
    fn truncated_json_is_closed_at_the_last_complete_value() {
        assert_eq!(parse(r#"{"path": "/tmp/fi"#), json!({"path": "/tmp/fi"}));
        assert_eq!(parse(r#"{"a": 1, "b"#), json!({"a": 1}));
        assert_eq!(parse(r#"{"a": 1, "b""#), json!({"a": 1}));
        assert_eq!(parse(r#"{"a": [1, 2"#), json!({"a": [1, 2]}));
        assert_eq!(parse(r#"{"a": tr"#), json!({"a": true}));
        assert_eq!(parse(r#"{"a": nu"#), json!({"a": null}));
        assert_eq!(parse(r#"{"a":"#), json!({"a": null}));
        assert_eq!(parse(r#"{"a": 1,"#), json!({"a": 1}));
        assert_eq!(parse(r#"{"a": -1."#), json!({"a": -1}));
        assert_eq!(parse(r#"{"a": "x\"#), json!({"a": "x"}));
        assert_eq!(parse(r#"{"a": "x\u00"#), json!({"a": "x"}));
        assert_eq!(parse(r#"{"a": "x\\"#), json!({"a": "x\\"}));
        assert_eq!(
            parse(r#"{"a": {"b": {"c": "d"#),
            json!({"a": {"b": {"c": "d"}}})
        );
    }

    #[test]
    fn raw_control_characters_and_invalid_escapes_are_repaired() {
        assert_eq!(
            parse("{\"path\":\"A\\H\",\"text\":\"col1\tcol2\"}"),
            json!({ "path": "A\\H", "text": "col1\tcol2" })
        );
        assert_eq!(
            parse("{\"a\": \"line1\nline2\u{1}\", \"b\": \"x\\\"y\\\\z\\u00e9\\/\"}"),
            json!({ "a": "line1\nline2\u{1}", "b": "x\"y\\z\u{e9}/" })
        );
        // Cut off as well.
        assert_eq!(
            parse("{\"a\": \"line1\nline2"),
            json!({ "a": "line1\nline2" })
        );
        assert_eq!(
            parse("{\"a\": \"C:\\dir\", \"b\": [1"),
            json!({ "a": "C:\\dir", "b": [1] })
        );
    }

    #[test]
    fn a_backslash_u_without_hex_digits_stays_literal() {
        assert_eq!(
            parse(r#"{"text":"\url{x}"}"#),
            json!({ "text": "\\url{x}" })
        );
        // The fields after it are kept.
        assert_eq!(
            parse(r#"{"path":"a.tex","content":"\usepackage{x}","mode":"w"}"#),
            json!({ "path": "a.tex", "content": "\\usepackage{x}", "mode": "w" })
        );
        assert_eq!(
            parse(r#"{"text":"caf\u00e9 \url"}"#),
            json!({ "text": "caf\u{e9} \\url" })
        );
        assert_eq!(parse(r#"{"text":"ab\u00"#), json!({ "text": "ab" }));
    }

    #[test]
    fn finished_arguments_are_parsed_strictly() {
        assert_eq!(
            parse_tool_call_arguments(r#"{"kind":"seek","note":"\url"}"#),
            Ok(json!({ "kind": "seek", "note": "\\url" })
                .as_object()
                .cloned()
                .unwrap())
        );
        assert_eq!(parse_tool_call_arguments(" "), Ok(Map::new()));
        // Neither the valid prefix of malformed JSON nor cut-off JSON is kept.
        for raw in [
            r#"{"action":"apply","actions":[{"kind":"seek","time":.5}]}"#,
            r#"{"kind":"seek","clipId":"clip-fl"#,
        ] {
            let error = parse_tool_call_arguments(raw).unwrap_err();
            assert!(
                error.ends_with(&format!("\n\nReceived arguments:\n{raw}")),
                "{error}"
            );
        }
        assert_eq!(
            parse_tool_call_arguments("[1,2]"),
            Err("Tool arguments must be a JSON object\n\nReceived arguments:\n[1,2]".into())
        );
    }

    #[test]
    fn non_objects_and_garbage_become_empty() {
        assert_eq!(parse(""), json!({}));
        assert_eq!(parse("[1, 2]"), json!({}));
        assert_eq!(parse("not json"), json!({}));
    }
}
