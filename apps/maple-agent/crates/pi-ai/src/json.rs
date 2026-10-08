use serde_json::{Map, Value};

/// Parse tool-call arguments that may have been cut off mid-stream.
///
/// Complete JSON parses normally. Truncated JSON is closed off at the last complete
/// value: an unfinished string is closed, a dangling key is dropped, a partial literal
/// is completed and open containers are closed. Anything that still does not parse,
/// or is not an object, yields an empty object.
pub fn parse_streaming_json(raw: &str) -> Map<String, Value> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Map::new();
    }
    if let Ok(Value::Object(map)) = serde_json::from_str(trimmed) {
        return map;
    }
    match serde_json::from_str(&repair(trimmed)) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
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
    fn non_objects_and_garbage_become_empty() {
        assert_eq!(parse(""), json!({}));
        assert_eq!(parse("[1, 2]"), json!({}));
        assert_eq!(parse("not json"), json!({}));
    }
}
