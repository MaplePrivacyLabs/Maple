//! Narrow paired predicate for the provisional owned summary-request DTO boundary.
//! No field is ignored: source aliasing and Rust ownership are checked separately.
use pi_ai::utils::js_value::{JsObject, JsValue};

pub const FUNCTION_ID: &str = "compaction.retryRequestOwnership";
pub const RULE_ID: &str = "owned-summary-request-dtos";
pub const RULE_PATH: &str = "$.value";

fn object(entries: impl IntoIterator<Item = (&'static str, JsValue)>) -> JsValue {
    JsValue::Object(JsObject::from_iter(entries))
}
fn required<'a>(value: &'a JsValue, key: &str) -> Result<&'a JsValue, String> {
    value
        .get(key)
        .ok_or_else(|| format!("Ownership fixture lacks {key}"))
}
pub fn compare(
    input: &JsValue,
    expected: &JsValue,
    actual: &JsValue,
    compare: impl Fn(&JsValue, &JsValue) -> Result<(), String>,
) -> Result<(), String> {
    let retry = required(input, "retry")?;
    if retry.get("enabled") != Some(&JsValue::Bool(true))
        || retry.get("maxRetries") != Some(&JsValue::Number(1.0))
        || retry.get("baseDelayMs") != Some(&JsValue::Number(0.0))
    {
        return Err("Ownership rule applies only to one zero-delay retry".into());
    }
    let context = required(input, "context")?;
    if context.as_object().is_none_or(|v| v.len() != 1) {
        return Err("Ownership context must only contain messages".into());
    }
    let messages = required(context, "messages")?
        .as_array()
        .ok_or("Ownership messages must be an array")?;
    let responses = required(input, "responses")?
        .as_array()
        .ok_or("Ownership responses must be an array")?;
    if responses.len() != 2
        || responses[0]["stopReason"].as_str() != Some("error")
        || responses[0]["errorMessage"].as_str() != Some("terminated")
        || responses[1]["stopReason"].as_str() != Some("stop")
    {
        return Err("Ownership rule requires transient terminated error then stop".into());
    }
    let options = required(input, "options")?;
    let changes = required(input, "mutations")?;
    let header = required(changes, "headerName")?
        .as_js_str()
        .ok_or("Header name must be a string")?;
    let before = object([
        ("modelName", required(&input["model"], "name")?.clone()),
        ("messageCount", (messages.len() as f64).into()),
        ("maxTokens", required(options, "maxTokens")?.clone()),
        (
            "header",
            options["headers"]
                .get(header.clone())
                .ok_or("Initial header absent")?
                .clone(),
        ),
    ]);
    let after = object([
        ("modelName", required(changes, "modelName")?.clone()),
        ("messageCount", ((messages.len() + 1) as f64).into()),
        ("maxTokens", required(changes, "maxTokens")?.clone()),
        ("header", required(changes, "headerValue")?.clone()),
    ]);
    required(changes, "appendMessage")?;
    if before["modelName"] == after["modelName"]
        || before["maxTokens"] == after["maxTokens"]
        || before["header"] == after["header"]
    {
        return Err("Ownership fixture must change all declared mutation fields".into());
    }
    let mut source_caller = after.clone();
    source_caller["maxTokens"] = before["maxTokens"].clone();
    let source = object([(
        "value",
        object([
            ("calls", JsValue::Array(vec![before.clone(), after])),
            ("callerAfter", source_caller),
            ("stopReason", "stop".into()),
        ]),
    )]);
    compare(&source, expected).map_err(|error| {
        format!("{RULE_ID}: exact TypeScript alias-mutation predicate failed: {error}")
    })?;
    let owned = object([(
        "value",
        object([
            (
                "calls",
                JsValue::Array(vec![before.clone(), before.clone()]),
            ),
            ("callerAfter", before),
            ("stopReason", "stop".into()),
        ]),
    )]);
    compare(&owned, actual)
        .map_err(|error| format!("{RULE_ID}: exact Rust owned-request predicate failed: {error}"))
}
