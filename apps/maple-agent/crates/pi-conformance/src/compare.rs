//! The shared corpus comparator. Normalization is deliberately structural: a
//! tool's arbitrary JSON is not a source of generated IDs or clock fields.

use pi_ai::utils::js_json::{quote, stringify};
use pi_ai::utils::js_value::{JsObject as Map, JsString, JsValue as Value};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug)]
pub struct GeneratedIdOptions {
    /// Discover IDs at the Pi protocol positions documented below. Disable
    /// this for fixtures whose session IDs are intentionally scripted.
    pub discover_protocol_ids: bool,
    /// Additional known generated IDs, in corresponding generation order.
    pub expected: Vec<JsString>,
    pub actual: Vec<JsString>,
}

impl Default for GeneratedIdOptions {
    fn default() -> Self {
        Self {
            discover_protocol_ids: true,
            expected: Vec::new(),
            actual: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CompareOptions {
    pub byte_exact: bool,
    pub normalize_timestamps: bool,
    pub generated_ids: GeneratedIdOptions,
    /// Scenario-declared immediate preflight failures. Ends before the last
    /// start are also recognized structurally. The last call's immediate end
    /// cannot be distinguished from an asynchronous no-update finish from
    /// events alone, so the scenario must declare it here.
    pub immediate_tool_call_ids: Vec<JsString>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Concurrency {
    #[default]
    Gated,
    Free,
}

/// Compare JSON structurally, retaining absent-versus-null and exact model
/// text. Integers and clock fields compare exactly. Nonintegral floats have
/// relative tolerance 1e-12 (there is no absolute floor).
pub fn compare(expected: &Value, actual: &Value, options: &CompareOptions) -> Result<(), String> {
    let (expected, actual) = normalized_pair(expected, actual, options)?;
    compare_value(&expected, &actual, "$")
}

/// Compare event streams. Free concurrency applies only within a validated
/// execution batch; all surrounding events and result messages remain ordered.
pub fn compare_events(
    expected: &[Value],
    actual: &[Value],
    options: &CompareOptions,
    concurrency: Concurrency,
) -> Result<(), String> {
    let expected_sequences =
        validate_envelopes(expected).map_err(|error| format!("expected events: {error}"))?;
    let actual_sequences =
        validate_envelopes(actual).map_err(|error| format!("actual events: {error}"))?;
    if expected_sequences != actual_sequences {
        return Err("event envelope sequence indexes differ".into());
    }
    let (expected, actual) = normalized_pair(
        &Value::Array(expected.to_vec()),
        &Value::Array(actual.to_vec()),
        options,
    )?;
    if concurrency == Concurrency::Gated {
        return compare_value(&expected, &actual, "$");
    }
    // Positions were checked globally; persistence counts remain attached to
    // each event when permitted interleavings move a call between positions.
    let without_sequences = |value: Value| -> Vec<Value> {
        value
            .as_array()
            .unwrap()
            .iter()
            .map(|event| {
                let mut event = event.clone();
                if is_envelope(&event) {
                    event.as_object_mut().unwrap().remove("seq");
                }
                event
            })
            .collect()
    };
    let expected = canonical_batches(&without_sequences(expected), options)
        .map_err(|error| format!("expected events: {error}"))?;
    let actual = canonical_batches(&without_sequences(actual), options)
        .map_err(|error| format!("actual events: {error}"))?;
    compare_value(&Value::Array(expected), &Value::Array(actual), "$")
}

fn is_envelope(value: &Value) -> bool {
    value
        .get("seq")
        .is_some_and(|value| value.as_u64().is_some())
        && value
            .get("entries")
            .is_some_and(|value| value.as_u64().is_some())
        && value.get("type").is_some_and(Value::is_string)
        && value.get("data").is_some_and(Value::is_object)
}

fn validate_envelopes(events: &[Value]) -> Result<Option<Vec<u64>>, String> {
    if !events.iter().any(|event| event.get("seq").is_some()) {
        return Ok(None);
    }
    let mut sequences: Vec<u64> = Vec::with_capacity(events.len());
    for (index, event) in events.iter().enumerate() {
        if !is_envelope(event) {
            return Err(format!("event {index}: malformed or mixed event envelope"));
        }
        let sequence = event["seq"].as_u64().unwrap();
        if let Some(previous) = sequences.last().copied()
            && previous.checked_add(1) != Some(sequence)
        {
            return Err(format!("event {index}: noncontiguous sequence index"));
        }
        if let Some(inner_type) = event["data"].get("type")
            && Some(inner_type) != event.get("type")
        {
            return Err(format!("event {index}: outer and inner event types differ"));
        }
        sequences.push(sequence);
    }
    Ok(Some(sequences))
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Context {
    Protocol,
    Message,
    Options,
    Headers,
    Arbitrary,
}

fn child_context(context: Context, key: &JsString) -> Context {
    if context == Context::Arbitrary || context == Context::Headers {
        return Context::Arbitrary;
    }
    match key.as_str().unwrap_or("") {
        "options" => Context::Options,
        "headers" => Context::Headers,
        "message" | "messages" | "toolResults" | "partial" => Context::Message,
        "entry"
        | "entries"
        | "header"
        | "session"
        | "sessionEntries"
        | "events"
        | "requests"
        | "http"
        | "context"
        | "state"
        | "assistantMessageEvent" => Context::Protocol,
        _ => Context::Arbitrary,
    }
}

fn contextual_child(parent: &Value, context: Context, key: &JsString) -> Context {
    if context == Context::Protocol && key == "data" && is_envelope(parent) {
        Context::Protocol
    } else {
        child_context(context, key)
    }
}

fn is_session_record(value: &Value, context: Context) -> bool {
    if context != Context::Protocol {
        return false;
    }
    let Some(kind) = value.get("type").and_then(Value::as_str) else {
        return false;
    };
    kind == "session"
        || (value.get("parentId").is_some()
            && matches!(
                kind,
                "message"
                    | "thinking_level_change"
                    | "model_change"
                    | "usage"
                    | "compaction"
                    | "branch_summary"
                    | "custom"
                    | "label"
                    | "session_info"
                    | "custom_message"
                    | "context_edit"
            ))
}

fn is_message(value: &Value, context: Context) -> bool {
    context != Context::Arbitrary
        && context != Context::Headers
        && matches!(
            value.get("role").and_then(Value::as_str),
            Some("user" | "assistant" | "toolResult" | "custom" | "bashExecution")
        )
}

fn is_affinity_header(key: &JsString) -> bool {
    matches!(
        key.as_str().unwrap_or("").to_ascii_lowercase().as_str(),
        "session_id" | "x-session-id" | "x-session-affinity" | "x-client-request-id"
    )
}

fn omitted_header(key: &JsString) -> bool {
    let key = key.as_str().unwrap_or("").to_ascii_lowercase();
    key.starts_with("x-stainless-") || matches!(key.as_str(), "idempotency-key" | "user-agent")
}

#[derive(Default)]
struct Discoveries {
    ids: BTreeMap<JsString, usize>,
    times: Vec<f64>,
}

impl Discoveries {
    fn add_id(&mut self, value: &Value) {
        if let Some(id) = value.as_js_str() {
            let next = self.ids.len();
            self.ids.entry(id.to_owned()).or_insert(next);
        }
    }

    fn visit(&mut self, value: &Value, context: Context, options: &CompareOptions) {
        match value {
            Value::Array(values) => {
                for value in values {
                    self.visit(value, context, options);
                }
            }
            Value::Object(object) => {
                let session = is_session_record(value, context);
                if options.generated_ids.discover_protocol_ids {
                    if session {
                        // Explicit field order makes discovery independent of
                        // object insertion order in either implementation.
                        for key in ["id", "parentId", "firstKeptEntryId", "fromId", "targetId"] {
                            if let Some(value) = object.get(key) {
                                self.add_id(value);
                            }
                        }
                    }
                    if context == Context::Options {
                        for key in ["sessionId", "routingSessionId", "routingId"] {
                            if let Some(value) = object.get(key) {
                                self.add_id(value);
                            }
                        }
                    }
                    if context == Context::Headers {
                        for key in sorted_keys(object) {
                            if is_affinity_header(key) {
                                self.add_id(&object[key]);
                            }
                        }
                    }
                }
                if options.normalize_timestamps
                    && (session || is_message(value, context))
                    && let Some(timestamp) = object.get("timestamp")
                {
                    if let Some(number) = timestamp.as_f64().filter(|n| n.is_finite()) {
                        self.times.push(number);
                    } else if let Some(time) = timestamp.as_str().and_then(iso_ms) {
                        self.times.push(time);
                    }
                }
                for key in sorted_keys(object) {
                    self.visit(&object[key], contextual_child(value, context, key), options);
                }
            }
            _ => {}
        }
    }

    fn finish(&mut self) {
        self.times.sort_by(f64::total_cmp);
        self.times.dedup_by(|left, right| *left == *right);
    }
}

fn sorted_keys(object: &Map) -> Vec<&JsString> {
    let mut keys: Vec<_> = object.keys().collect();
    keys.sort_unstable_by(|left, right| left.units().cmp(right.units()));
    keys
}

fn contains_prefix_text(value: &JsString, prefix: &str) -> bool {
    if let Some(text) = value.as_str() {
        return text.contains(prefix);
    }
    let needle: Vec<u16> = prefix.encode_utf16().collect();
    if needle.is_empty() {
        return true;
    }
    value
        .as_utf16()
        .windows(needle.len())
        .any(|part| part == needle)
}

fn starts_with(value: &JsString, prefix: &str) -> bool {
    let mut units = value.units();
    prefix
        .encode_utf16()
        .all(|expected| units.next() == Some(expected))
}

fn iso_ms(value: &str) -> Option<f64> {
    // Date.toISOString's ordinary four-digit year representation. Other
    // strings remain exact instead of quietly broadening the escape hatch.
    if value.len() != 24
        || !value.bytes().enumerate().all(|(index, byte)| match index {
            4 | 7 => byte == b'-',
            10 => byte == b'T',
            13 | 16 => byte == b':',
            19 => byte == b'.',
            23 => byte == b'Z',
            _ => byte.is_ascii_digit(),
        })
    {
        return None;
    }
    let number = |range: std::ops::Range<usize>| {
        value[range]
            .bytes()
            .fold(0_u32, |result, byte| result * 10 + u32::from(byte - b'0'))
    };
    let year = number(0..4);
    let month = number(5..7);
    let day = number(8..10);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => {
            29
        }
        2 => 28,
        _ => return None,
    };
    if !(1..=days).contains(&day)
        || number(11..13) >= 24
        || number(14..16) >= 60
        || number(17..19) >= 60
    {
        return None;
    }
    // Gregorian civil date to days from 1970-01-01, including year zero.
    // Ranking ISO and numeric times together preserves their relationship.
    let year = i64::from(year) - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let shifted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days_since_epoch = era * 146_097 + day_of_era - 719_468;
    let milliseconds = days_since_epoch * 86_400_000
        + i64::from(number(11..13)) * 3_600_000
        + i64::from(number(14..16)) * 60_000
        + i64::from(number(17..19)) * 1_000
        + i64::from(number(20..23));
    Some(milliseconds as f64)
}

fn contains_prefix(value: &Value, prefix: &str) -> bool {
    match value {
        Value::String(value) => contains_prefix_text(value, prefix),
        Value::Array(values) => values.iter().any(|value| contains_prefix(value, prefix)),
        Value::Object(values) => values.iter().any(|(key, value)| {
            contains_prefix_text(key, prefix) || contains_prefix(value, prefix)
        }),
        _ => false,
    }
}

fn normalized_pair(
    expected: &Value,
    actual: &Value,
    options: &CompareOptions,
) -> Result<(Value, Value), String> {
    // Choose a namespace absent from both inputs. Literal model text cannot
    // accidentally compare equal to an internal normalized ID or timestamp.
    let mut nonce = 0_u64;
    let prefix = loop {
        let prefix = format!("\0pi-normalization:{nonce}:");
        if !contains_prefix(expected, &prefix) && !contains_prefix(actual, &prefix) {
            break prefix;
        }
        nonce += 1;
    };
    let normalize = |value: &Value, seeds: &[JsString]| -> Result<Value, String> {
        let mut discovered = Discoveries::default();
        for seed in seeds {
            discovered.add_id(&Value::String(seed.clone()));
        }
        discovered.visit(value, Context::Protocol, options);
        discovered.finish();
        normalize_value(
            value,
            Context::Protocol,
            &discovered,
            options,
            &prefix,
            false,
        )
    };
    Ok((
        normalize(expected, &options.generated_ids.expected)?,
        normalize(actual, &options.generated_ids.actual)?,
    ))
}

fn normalize_value(
    value: &Value,
    context: Context,
    discovered: &Discoveries,
    options: &CompareOptions,
    prefix: &str,
    scripted_id: bool,
) -> Result<Value, String> {
    match value {
        Value::String(value) if !scripted_id => Ok(discovered.ids.get(value).map_or_else(
            || Value::String(value.clone()),
            |id| Value::String(format!("{prefix}id:{id}").into()),
        )),
        Value::Array(values) => values
            .iter()
            .map(|value| normalize_value(value, context, discovered, options, prefix, false))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Value::Object(object) => {
            let clock_object = is_session_record(value, context) || is_message(value, context);
            let tool_call = value.get("type").and_then(Value::as_str) == Some("toolCall");
            let completion = value
                .get("object")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind == "chat.completion" || kind == "chat.completion.chunk");
            let mut result = Map::new();
            for (key, child) in object {
                if (context == Context::Headers && omitted_header(key))
                    || (!options.byte_exact && context == Context::Protocol && key == "bodyRaw")
                {
                    continue;
                }
                if options.normalize_timestamps && clock_object && key == "timestamp" {
                    if let Some(number) = child.as_f64().filter(|n| n.is_finite()) {
                        let rank = discovered
                            .times
                            .iter()
                            .position(|time| *time == number)
                            .expect("timestamp was discovered");
                        result.insert(
                            key.clone(),
                            Value::String(format!("{prefix}time:number:{rank}").into()),
                        );
                        continue;
                    }
                    if let Some(time) = child.as_str().and_then(iso_ms) {
                        let rank = discovered
                            .times
                            .iter()
                            .position(|candidate| *candidate == time)
                            .expect("timestamp was discovered");
                        result.insert(
                            key.clone(),
                            Value::String(format!("{prefix}time:iso:{rank}").into()),
                        );
                        continue;
                    }
                }
                let scripted = key == "toolCallId"
                    || (key == "id"
                        && (tool_call
                            || completion
                            || child
                                .as_js_str()
                                .is_some_and(|id| starts_with(id, "chatcmpl"))));
                result.insert(
                    key.clone(),
                    normalize_value(
                        child,
                        contextual_child(value, context, key),
                        discovered,
                        options,
                        prefix,
                        scripted,
                    )?,
                );
            }
            Ok(Value::Object(result))
        }
        Value::Number(number) if !number.is_finite() => Err(format!(
            "non-finite or unrepresentable JSON number: {number}"
        )),
        _ => Ok(value.clone()),
    }
}

/// Compare a function result without applying protocol/header normalization.
pub fn compare_unmodified(expected: &Value, actual: &Value) -> Result<(), String> {
    compare_value(expected, actual, "$")
}

fn compare_value(expected: &Value, actual: &Value, path: &str) -> Result<(), String> {
    match (expected, actual) {
        (Value::Number(left), Value::Number(right)) => {
            if numbers_equal(left, right, path.ends_with("[\"timestamp\"]")) {
                Ok(())
            } else {
                Err(format!(
                    "{path}: expected {}, got {}",
                    stringify(expected),
                    stringify(actual)
                ))
            }
        }
        (Value::Array(left), Value::Array(right)) => {
            if left.len() != right.len() {
                return Err(format!(
                    "{path}: expected {} items, got {}",
                    left.len(),
                    right.len()
                ));
            }
            for (index, (left, right)) in left.iter().zip(right).enumerate() {
                compare_value(left, right, &format!("{path}[{index}]"))?;
            }
            Ok(())
        }
        (Value::Object(left), Value::Object(right)) => {
            for key in sorted_keys(left) {
                let child_path = format!("{path}[{}]", quote(key));
                let actual = right
                    .get(key)
                    .ok_or_else(|| format!("{child_path}: missing field"))?;
                compare_value(&left[key], actual, &child_path)?;
            }
            if let Some(extra) = sorted_keys(right)
                .into_iter()
                .find(|key| !left.contains_key(*key))
            {
                return Err(format!("{path}[{}]: unexpected field", quote(extra)));
            }
            Ok(())
        }
        _ if expected == actual => Ok(()),
        _ => Err(format!(
            "{path}: expected {}, got {}",
            stringify(expected),
            stringify(actual)
        )),
    }
}

fn numbers_equal(left: &f64, right: &f64, strict: bool) -> bool {
    if !left.is_finite() || !right.is_finite() {
        return false;
    }
    // Both inputs have already crossed a lossless JavaScript-value boundary.
    // Distinct integral binary64 values must never be hidden by tolerance.
    if left.fract() == 0.0 && right.fract() == 0.0 {
        return left == right;
    }
    left == right || (!strict && (left - right).abs() <= left.abs().max(right.abs()) * 1e-12)
}

fn event_type(event: &Value) -> Option<&str> {
    event.get("type").and_then(Value::as_str)
}

fn event_field<'a>(event: &'a Value, key: &str) -> Option<&'a Value> {
    // Free-batch preparation already removed seq from validated envelopes.
    if event
        .get("entries")
        .is_some_and(|value| value.as_u64().is_some())
        && event.get("data").is_some_and(Value::is_object)
    {
        event.get("data")?.get(key)
    } else {
        event.get(key)
    }
}

fn call_id(event: &Value) -> Result<&JsString, String> {
    event_field(event, "toolCallId")
        .and_then(Value::as_js_str)
        .ok_or_else(|| {
            format!(
                "{} has no string toolCallId",
                event_type(event).unwrap_or("event")
            )
        })
}

fn is_result_start(event: &Value) -> bool {
    event_type(event) == Some("message_start")
        && event_field(event, "message")
            .and_then(|message| message.get("role"))
            .and_then(Value::as_str)
            == Some("toolResult")
}

fn canonical_batches(events: &[Value], options: &CompareOptions) -> Result<Vec<Value>, String> {
    let declared: BTreeSet<&JsString> = options.immediate_tool_call_ids.iter().collect();
    let mut result = Vec::new();
    let mut position = 0;
    while position < events.len() {
        if !events[position].get("type").is_some_and(Value::is_string) {
            return Err(format!("event {position}: missing string event type"));
        }
        if event_type(&events[position]) != Some("tool_execution_start") {
            if matches!(
                event_type(&events[position]),
                Some("tool_execution_update" | "tool_execution_end")
            ) {
                return Err(format!("event {position}: execution event outside a batch"));
            }
            result.push(events[position].clone());
            position += 1;
            continue;
        }
        let end = (position..events.len())
            .find(|index| is_result_start(&events[*index]))
            .ok_or_else(|| format!("batch at event {position} has no toolResult message_start"))?;
        let segment = &events[position..end];
        let last_start = segment
            .iter()
            .rposition(|event| event_type(event) == Some("tool_execution_start"))
            .unwrap();
        let mut starts = Vec::new();
        let mut calls: BTreeMap<&JsString, Vec<Value>> = BTreeMap::new();
        let mut ended = BTreeSet::new();
        let mut preflight = Vec::new();
        let mut running = false;
        for (index, event) in segment.iter().enumerate() {
            let id = call_id(event)?;
            match event_type(event) {
                Some("tool_execution_start") => {
                    if running || calls.contains_key(id) {
                        return Err(format!(
                            "event {}: late or duplicate start for {}",
                            position + index,
                            quote(id)
                        ));
                    }
                    starts.push(id);
                    calls.insert(id, Vec::new());
                    preflight.push(event.clone());
                }
                Some("tool_execution_update" | "tool_execution_end") => {
                    let call = calls.get_mut(id).ok_or_else(|| {
                        format!("event {}: {} has not started", position + index, quote(id))
                    })?;
                    if ended.contains(id) {
                        return Err(format!(
                            "event {}: event after end for {}",
                            position + index,
                            quote(id)
                        ));
                    }
                    let is_end = event_type(event) == Some("tool_execution_end");
                    let immediate = is_end && (index < last_start || declared.contains(id));
                    if immediate {
                        if running
                            || index == 0
                            || event_type(&segment[index - 1]) != Some("tool_execution_start")
                            || call_id(&segment[index - 1])? != id
                        {
                            return Err(format!(
                                "event {}: preflight end for {} is not adjacent to its start",
                                position + index,
                                quote(id)
                            ));
                        }
                        preflight.push(event.clone());
                    } else {
                        if index < last_start || declared.contains(id) {
                            return Err(format!(
                                "event {}: runnable event during preflight for {}",
                                position + index,
                                quote(id)
                            ));
                        }
                        running = true;
                        call.push(event.clone());
                    }
                    if is_end {
                        ended.insert(id);
                    }
                }
                _ => {
                    return Err(format!(
                        "event {}: unrelated event inside free batch",
                        position + index
                    ));
                }
            }
        }
        if ended.len() != starts.len() {
            return Err(format!("batch at event {position}: not every call ended"));
        }
        // Pi emits one start/end message pair per source call after execution.
        for (index, id) in starts.iter().enumerate() {
            for (offset, kind) in [(0, "message_start"), (1, "message_end")] {
                let event_index = end + index * 2 + offset;
                let event = events
                    .get(event_index)
                    .ok_or_else(|| format!("missing result event for {}", quote(id)))?;
                if event_type(event) != Some(kind)
                    || event_field(event, "message")
                        .and_then(|message| message.get("role"))
                        .and_then(Value::as_str)
                        != Some("toolResult")
                    || event_field(event, "message")
                        .and_then(|message| message.get("toolCallId"))
                        .and_then(Value::as_js_str)
                        != Some(*id)
                {
                    return Err(format!(
                        "event {event_index}: result messages do not follow source order for {}",
                        quote(id)
                    ));
                }
            }
        }
        let per_call: Vec<_> = starts
            .iter()
            .map(|id| {
                Value::Object(Map::from([
                    ("toolCallId", Value::String((*id).clone())),
                    ("events", Value::Array(calls[id].clone())),
                ]))
            })
            .collect();
        result.push(Value::Object(Map::from([(
            "freeBatch",
            Value::Object(Map::from([
                ("preflight", Value::Array(preflight)),
                ("calls", Value::Array(per_call)),
            ])),
        )])));
        result.extend_from_slice(&events[end..end + starts.len() * 2]);
        position = end + starts.len() * 2;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    macro_rules! json {
        ($($json:tt)+) => { Value::try_from(serde_json::json!($($json)+)).expect("fixture must fit JavaScript exactly") };
    }

    fn equal(expected: Value, actual: Value) -> bool {
        compare(&expected, &actual, &CompareOptions::default()).is_ok()
    }

    #[test]
    fn structural_numbers_and_object_order() {
        assert!(equal(json!({"a": 1, "b": 2}), json!({"b": 2.0, "a": 1.0})));
        assert!(equal(json!(1.0), json!(1.0 + 0.5e-12)));
        assert!(!equal(json!(1.0), json!(1.0 + 2e-12)));
        assert!(!equal(json!(0), json!(1e-100)));
        assert!(!equal(json!(-1), json!(1)));
        // Integers that would collapse at the new JS-value boundary fail
        // conversion explicitly; distinct representable large integers stay exact.
        assert!(Value::try_from(serde_json::json!(u64::MAX)).is_err());
        assert!(Value::try_from(serde_json::json!(u64::MAX - 1)).is_err());
        assert!(!equal(
            json!(2.0_f64.powi(64)),
            json!(2.0_f64.powi(64) - 2048.0)
        ));
        assert!(!equal(
            json!(1_767_225_600_000_u64),
            json!(1_767_225_600_001_u64)
        ));
        assert!(!equal(
            json!({"timestamp":1_767_225_600_000_u64}),
            json!({"timestamp":1_767_225_600_001_u64})
        ));
        assert!(!equal(
            json!({"timestamp":1.1}),
            json!({"timestamp":1.1 + 0.5e-12})
        ));
    }

    #[test]
    fn null_missing_array_order_and_strings_remain_distinct() {
        assert!(!equal(json!({}), json!({"a": null})));
        assert!(!equal(json!([1, 2]), json!([2, 1])));
        assert!(!equal(
            json!({"content":"{\"a\":1}"}),
            json!({"content":"{ \"a\": 1 }"})
        ));
        assert!(!equal(json!({"durationMs": 1}), json!({"durationMs": 2})));
    }

    #[test]
    fn generated_ids_are_discovered_only_in_protocol_positions() {
        let expected = json!({"entries":[{"type":"session","id":"old"},{"type":"message","id":"entry-a","parentId":null,"message":{"role":"assistant","content":[{"type":"text","text":"old"}]}}],"state":{"selected":"entry-a"}});
        let actual = json!({"entries":[{"type":"session","id":"new"},{"type":"message","id":"entry-b","parentId":null,"message":{"role":"assistant","content":[{"type":"text","text":"new"}]}}],"state":{"selected":"entry-b"}});
        assert!(equal(expected, actual));
        assert!(!equal(json!({"id":"old"}), json!({"id":"new"})));
        assert!(!equal(
            json!({"args":{"type":"session","id":"old"}}),
            json!({"args":{"type":"session","id":"new"}})
        ));
        assert!(!equal(
            json!({"type":"session","id":"old","text":"prefix-old"}),
            json!({"type":"session","id":"new","text":"prefix-new"})
        ));
    }

    #[test]
    fn scripted_tool_and_completion_ids_are_never_renumbered() {
        assert!(!equal(
            json!({"entries":[{"type":"session","id":"a"}],"message":{"role":"assistant","content":[{"type":"toolCall","id":"a"}]}}),
            json!({"entries":[{"type":"session","id":"b"}],"message":{"role":"assistant","content":[{"type":"toolCall","id":"b"}]}})
        ));
        assert!(!equal(
            json!({"type":"session","id":"a","toolCallId":"a"}),
            json!({"type":"session","id":"b","toolCallId":"b"})
        ));
        assert!(!equal(
            json!({"options":{"sessionId":"chatcmpl-a"},"id":"chatcmpl-a"}),
            json!({"options":{"sessionId":"chatcmpl-b"},"id":"chatcmpl-b"})
        ));
    }

    #[test]
    fn normalization_tokens_cannot_collide_with_literal_text() {
        assert!(!equal(
            json!({"type":"session","id":"a","text":"a"}),
            json!({"type":"session","id":"b","text":"\u{0000}pi-normalization:0:id:0"})
        ));
    }

    #[test]
    fn caller_can_disable_discovery_or_supply_corresponding_ids() {
        let mut options = CompareOptions::default();
        options.generated_ids.discover_protocol_ids = false;
        assert!(
            compare(
                &json!({"type":"session","id":"a"}),
                &json!({"type":"session","id":"b"}),
                &options
            )
            .is_err()
        );
        options.generated_ids.expected = vec!["a".into()];
        options.generated_ids.actual = vec!["b".into()];
        assert!(compare(&json!({"routing":"a"}), &json!({"routing":"b"}), &options).is_ok());
    }

    #[test]
    fn headers_and_raw_body_exclusions_do_not_mask_tool_json() {
        assert!(equal(
            json!({"headers":{"x-stainless-os":"mac","user-agent":"node","authorization":"Bearer same"},"bodyRaw":"{ }"}),
            json!({"headers":{"x-stainless-os":"linux","authorization":"Bearer same"},"bodyRaw":"{}"})
        ));
        assert!(!equal(
            json!({"headers":{"authorization":"a"}}),
            json!({"headers":{"authorization":"b"}})
        ));
        assert!(!equal(
            json!({"args":{"headers":{"user-agent":"a"}}}),
            json!({"args":{"headers":{"user-agent":"b"}}})
        ));
        assert!(!equal(
            json!({"args":{"bodyRaw":"a"}}),
            json!({"args":{"bodyRaw":"b"}})
        ));
        let options = CompareOptions {
            byte_exact: true,
            ..Default::default()
        };
        assert!(
            compare(
                &json!({"bodyRaw":"{ }"}),
                &json!({"bodyRaw":"{}"}),
                &options
            )
            .is_err()
        );
    }

    #[test]
    fn affinity_references_share_generated_identity() {
        assert!(equal(
            json!({"options":{"sessionId":"a"},"headers":{"x-session-id":"a"}}),
            json!({"options":{"sessionId":"b"},"headers":{"x-session-id":"b"}})
        ));
        assert!(!equal(
            json!({"options":{"sessionId":"a"},"headers":{"x-session-id":"a"}}),
            json!({"options":{"sessionId":"b"},"headers":{"x-session-id":"c"}})
        ));
    }

    #[test]
    fn timestamp_escape_is_explicit_preserves_order_and_does_not_mask_payloads() {
        let expected = json!([{"role":"user","timestamp":10},{"role":"assistant","timestamp":20}]);
        let shifted = json!([{"role":"user","timestamp":100},{"role":"assistant","timestamp":200}]);
        assert!(!equal(expected.clone(), shifted.clone()));
        let options = CompareOptions {
            normalize_timestamps: true,
            ..Default::default()
        };
        assert!(compare(&expected, &shifted, &options).is_ok());
        assert!(
            compare(
                &expected,
                &json!([{"role":"user","timestamp":200},{"role":"assistant","timestamp":100}]),
                &options
            )
            .is_err()
        );
        assert!(
            compare(
                &expected,
                &json!([{"role":"user","timestamp":100},{"role":"assistant","timestamp":100}]),
                &options
            )
            .is_err()
        );
        assert!(
            compare(
                &json!({"args":{"timestamp":10}}),
                &json!({"args":{"timestamp":20}}),
                &options
            )
            .is_err()
        );
        assert!(
            compare(
                &json!({"type":"session","timestamp":"2026-01-01T00:00:00.000Z"}),
                &json!({"type":"session","timestamp":"2027-01-01T00:00:00.000Z"}),
                &options
            )
            .is_ok()
        );
    }

    #[test]
    fn timestamp_escape_keeps_numeric_and_iso_clocks_related() {
        assert_eq!(iso_ms("1970-01-01T00:00:00.000Z"), Some(0.0));
        assert_eq!(iso_ms("1969-12-31T23:59:59.999Z"), Some(-1.0));
        assert_eq!(iso_ms("2000-02-29T00:00:00.000Z"), Some(951_782_400_000.0));
        assert_eq!(iso_ms("1900-02-29T00:00:00.000Z"), None);
        assert_eq!(iso_ms("2026-02-30T00:00:00.000Z"), None);
        assert_eq!(iso_ms("2026-01-01T24:00:00.000Z"), None);
        let options = CompareOptions {
            normalize_timestamps: true,
            ..Default::default()
        };
        let expected = json!([{"role":"user","timestamp":0},{"type":"session","timestamp":"1970-01-01T00:00:00.000Z"}]);
        let actual = json!([{"role":"user","timestamp":0},{"type":"session","timestamp":"1970-01-01T00:00:00.001Z"}]);
        assert!(compare(&expected, &actual, &options).is_err());
        assert!(
            compare(
                &json!({"type":"session","timestamp":"2026-01-01T00:00:00.000Z"}),
                &json!({"type":"session","timestamp":"2026-99-01T00:00:00.000Z"}),
                &options
            )
            .is_err()
        );
    }

    fn execution(kind: &str, id: &str, number: usize) -> Value {
        json!({"type":format!("tool_execution_{kind}"),"toolCallId":id,"value":number})
    }

    fn results(ids: &[&str]) -> Vec<Value> {
        ids.iter()
            .flat_map(|id| {
                [
                    json!({"type":"message_start","message":{"role":"toolResult","toolCallId":id}}),
                    json!({"type":"message_end","message":{"role":"toolResult","toolCallId":id}}),
                ]
            })
            .collect()
    }

    fn batch(order: &[(&str, &str, usize)], ids: &[&str]) -> Vec<Value> {
        order
            .iter()
            .map(|(kind, id, number)| execution(kind, id, *number))
            .chain(results(ids))
            .collect()
    }

    fn envelopes(events: Vec<Value>) -> Vec<Value> {
        events
            .into_iter()
            .enumerate()
            .map(|(seq, mut data)| {
                let kind = data.as_object_mut().unwrap().remove("type").unwrap();
                json!({"seq": seq, "type": kind, "entries": 0, "data": data})
            })
            .collect()
    }

    #[test]
    fn free_envelopes_validate_positions_and_preserve_persistence_metadata() {
        let expected = envelopes(batch(
            &[
                ("start", "a", 0),
                ("start", "b", 0),
                ("end", "a", 1),
                ("end", "b", 1),
            ],
            &["a", "b"],
        ));
        let actual = envelopes(batch(
            &[
                ("start", "a", 0),
                ("start", "b", 0),
                ("end", "b", 1),
                ("end", "a", 1),
            ],
            &["a", "b"],
        ));
        let options = CompareOptions::default();
        assert!(compare_events(&expected, &actual, &options, Concurrency::Free).is_ok());
        assert!(compare_events(&expected, &actual, &options, Concurrency::Gated).is_err());
        let mut corrupt = actual.clone();
        corrupt[2]["entries"] = json!(1);
        assert!(compare_events(&expected, &corrupt, &options, Concurrency::Free).is_err());
        let mut corrupt = actual.clone();
        corrupt[2]["seq"] = json!(3);
        assert!(compare_events(&corrupt, &corrupt, &options, Concurrency::Free).is_err());
        let mut corrupt = actual.clone();
        corrupt[2]["data"]["type"] = json!("tool_execution_update");
        assert!(compare_events(&corrupt, &corrupt, &options, Concurrency::Free).is_err());
        let shifted: Vec<_> = actual
            .into_iter()
            .map(|mut event| {
                event["seq"] = json!(event["seq"].as_u64().unwrap() + 10);
                event
            })
            .collect();
        assert!(compare_events(&expected, &shifted, &options, Concurrency::Free).is_err());
    }

    #[test]
    fn envelope_normalization_uses_protocol_payload_but_not_arbitrary_data() {
        let expected = json!({"seq":0,"type":"entry_appended","entries":1,"data":{"entry":{"type":"message","id":"a","parentId":null,"message":{"role":"user","timestamp":100}}}});
        let actual = json!({"seq":0,"type":"entry_appended","entries":1,"data":{"entry":{"type":"message","id":"b","parentId":null,"message":{"role":"user","timestamp":200}}}});
        let options = CompareOptions {
            normalize_timestamps: true,
            ..Default::default()
        };
        assert!(compare(&expected, &actual, &options).is_ok());
        assert!(
            compare(
                &json!({"data":{"type":"session","id":"a"}}),
                &json!({"data":{"type":"session","id":"b"}}),
                &options
            )
            .is_err()
        );
    }

    #[test]
    fn free_batches_allow_only_cross_call_execution_interleavings() {
        let expected = batch(
            &[
                ("start", "a", 0),
                ("start", "b", 0),
                ("update", "a", 1),
                ("update", "a", 2),
                ("end", "a", 3),
                ("update", "b", 1),
                ("end", "b", 2),
            ],
            &["a", "b"],
        );
        let actual = batch(
            &[
                ("start", "a", 0),
                ("start", "b", 0),
                ("update", "b", 1),
                ("update", "a", 1),
                ("end", "b", 2),
                ("update", "a", 2),
                ("end", "a", 3),
            ],
            &["a", "b"],
        );
        let options = CompareOptions::default();
        assert!(compare_events(&expected, &actual, &options, Concurrency::Gated).is_err());
        assert!(compare_events(&expected, &actual, &options, Concurrency::Free).is_ok());
        let mut reordered = actual.clone();
        reordered.swap(3, 5);
        assert!(compare_events(&expected, &reordered, &options, Concurrency::Free).is_err());
        let mut wrong_start = actual.clone();
        wrong_start.swap(0, 1);
        assert!(compare_events(&expected, &wrong_start, &options, Concurrency::Free).is_err());
        let mut wrong_result = actual.clone();
        wrong_result.splice(7.., results(&["b", "a"]));
        assert!(compare_events(&expected, &wrong_result, &options, Concurrency::Free).is_err());
    }

    #[test]
    fn free_batches_reject_invalid_lifecycles_even_when_both_sides_match() {
        for events in [
            batch(
                &[("start", "a", 0), ("end", "a", 0), ("end", "a", 0)],
                &["a"],
            ),
            batch(
                &[("start", "a", 0), ("update", "b", 0), ("end", "a", 0)],
                &["a"],
            ),
            batch(&[("start", "a", 0), ("update", "a", 0)], &["a"]),
            batch(
                &[
                    ("start", "a", 0),
                    ("update", "a", 0),
                    ("start", "b", 0),
                    ("end", "a", 0),
                    ("end", "b", 0),
                ],
                &["a", "b"],
            ),
        ] {
            assert!(
                compare_events(
                    &events,
                    &events,
                    &CompareOptions::default(),
                    Concurrency::Free
                )
                .is_err()
            );
        }
    }

    #[test]
    fn immediate_preflight_ends_preserve_source_order() {
        let expected = batch(
            &[
                ("start", "a", 0),
                ("end", "a", 0),
                ("start", "b", 0),
                ("end", "b", 0),
            ],
            &["a", "b"],
        );
        let delayed = batch(
            &[
                ("start", "a", 0),
                ("start", "b", 0),
                ("end", "a", 0),
                ("end", "b", 0),
            ],
            &["a", "b"],
        );
        assert!(
            compare_events(
                &expected,
                &delayed,
                &CompareOptions::default(),
                Concurrency::Free
            )
            .is_err()
        );
        let options = CompareOptions {
            immediate_tool_call_ids: vec!["b".into()],
            ..Default::default()
        };
        let expected = batch(
            &[
                ("start", "a", 0),
                ("start", "b", 0),
                ("end", "b", 0),
                ("end", "a", 0),
            ],
            &["a", "b"],
        );
        let delayed = batch(
            &[
                ("start", "a", 0),
                ("start", "b", 0),
                ("end", "a", 0),
                ("end", "b", 0),
            ],
            &["a", "b"],
        );
        assert!(compare_events(&expected, &expected, &options, Concurrency::Free).is_ok());
        assert!(compare_events(&expected, &delayed, &options, Concurrency::Free).is_err());
    }

    fn raw(units: &[u16]) -> Value {
        Value::String(JsString::from_utf16(units.to_vec()))
    }

    #[test]
    fn raw_utf16_strings_and_keys_are_compared_without_replacement() {
        assert!(equal(raw(&[0xd800]), raw(&[0xd800])));
        assert!(!equal(raw(&[0xd800]), raw(&[0xdc00])));
        assert!(!equal(raw(&[0xd800]), json!("�")));
        assert!(equal(raw(&[0xd83d, 0xde48]), json!("🙈")));

        let high = JsString::from_utf16(vec![0xd800]);
        let low = JsString::from_utf16(vec![0xdc00]);
        let expected = Value::Object(Map::from([(high.clone(), raw(&[0xd800]))]));
        let actual = Value::Object(Map::from([(low, raw(&[0xd800]))]));
        let error = compare(&expected, &actual, &CompareOptions::default()).unwrap_err();
        assert_eq!(error, r#"$["\ud800"]: missing field"#);
        assert!(equal(expected.clone(), expected));
        let unexpected = Value::Object(Map::from([(high, Value::Null)]));
        assert_eq!(
            compare(&json!({}), &unexpected, &CompareOptions::default()).unwrap_err(),
            r#"$["\ud800"]: unexpected field"#
        );
    }

    #[test]
    fn raw_generated_ids_keep_exact_references_and_scripted_positions() {
        let high = JsString::from_utf16(vec![0xd800]);
        let low = JsString::from_utf16(vec![0xdc00]);
        let mut expected = json!({"type":"session","id":"a","state":{"selected":"a"}});
        expected["id"] = Value::String(high.clone());
        expected["state"]["selected"] = Value::String(high.clone());
        let mut actual = json!({"type":"session","id":"b","state":{"selected":"b"}});
        actual["id"] = Value::String(low.clone());
        actual["state"]["selected"] = Value::String(low.clone());
        assert!(equal(expected.clone(), actual.clone()));
        actual["state"]["selected"] = raw(&[0xfffd]);
        assert!(!equal(expected.clone(), actual));

        expected["toolCallId"] = Value::String(high);
        let mut actual = expected.clone();
        actual["id"] = Value::String(low.clone());
        actual["state"]["selected"] = Value::String(low.clone());
        actual["toolCallId"] = Value::String(low);
        assert!(!equal(expected, actual));

        let completion = |suffix: u16| {
            let mut id = JsString::from("chatcmpl");
            id.push(&JsString::from_utf16(vec![suffix]));
            Value::Object(Map::from([
                (
                    "options",
                    Value::Object(Map::from([("sessionId", Value::String(id.clone()))])),
                ),
                ("id", Value::String(id)),
            ]))
        };
        assert!(!equal(completion(0xd800), completion(0xdc00)));
    }

    #[test]
    fn raw_strings_cannot_hide_normalization_token_prefixes() {
        let mut value = JsString::from_utf16(vec![0xd800]);
        value.push_str("\0pi-normalization:0:id:0");
        assert!(contains_prefix(
            &Value::String(value.clone()),
            "\0pi-normalization:0:"
        ));
        assert!(contains_prefix(
            &Value::Object(Map::from([(value, Value::Null)])),
            "\0pi-normalization:0:"
        ));
        assert!(!contains_prefix(&raw(&[0xd800]), "\0pi-normalization:0:"));
    }

    #[test]
    fn protocol_key_matching_does_not_coerce_invalid_utf16_names() {
        let key = JsString::from_utf16(vec![0x69, 0x64, 0xd800]);
        let expected = Value::Object(Map::from([(key.clone(), Value::String("old".into()))]));
        let actual = Value::Object(Map::from([(key, Value::String("new".into()))]));
        assert!(!equal(expected, actual));

        let bad_header = JsString::from_utf16(vec![
            0x75, 0x73, 0x65, 0x72, 0x2d, 0x61, 0x67, 0x65, 0x6e, 0x74, 0xd800,
        ]);
        let with_header = |text: &str| {
            Value::Object(Map::from([(
                "headers",
                Value::Object(Map::from([(
                    bad_header.clone(),
                    Value::String(text.into()),
                )])),
            )]))
        };
        assert!(!equal(with_header("a"), with_header("b")));
    }

    fn replace_call_id(events: &mut [Value], old: &str, replacement: &JsString) {
        for event in events {
            if event.get("toolCallId").and_then(Value::as_str) == Some(old) {
                event["toolCallId"] = Value::String(replacement.clone());
            }
            if event
                .get("message")
                .and_then(|message| message.get("toolCallId"))
                .and_then(Value::as_str)
                == Some(old)
            {
                event["message"]["toolCallId"] = Value::String(replacement.clone());
            }
        }
    }

    #[test]
    fn free_batches_preserve_raw_call_ids_and_result_order() {
        let mut expected = batch(
            &[
                ("start", "a", 0),
                ("start", "b", 0),
                ("end", "a", 1),
                ("end", "b", 1),
            ],
            &["a", "b"],
        );
        let mut actual = batch(
            &[
                ("start", "a", 0),
                ("start", "b", 0),
                ("end", "b", 1),
                ("end", "a", 1),
            ],
            &["a", "b"],
        );
        let high = JsString::from_utf16(vec![0xd800]);
        let low = JsString::from_utf16(vec![0xdc00]);
        for events in [&mut expected, &mut actual] {
            replace_call_id(events, "a", &high);
            replace_call_id(events, "b", &low);
        }
        let options = CompareOptions::default();
        assert!(compare_events(&expected, &actual, &options, Concurrency::Free).is_ok());
        assert!(compare_events(&expected, &actual, &options, Concurrency::Gated).is_err());
        actual[4]["message"]["toolCallId"] = raw(&[0xfffd]);
        assert!(compare_events(&actual, &actual, &options, Concurrency::Free).is_err());
    }

    #[test]
    fn immediate_preflight_declarations_accept_exact_raw_call_ids() {
        let mut events = batch(
            &[
                ("start", "a", 0),
                ("start", "b", 0),
                ("end", "b", 1),
                ("end", "a", 1),
            ],
            &["a", "b"],
        );
        let raw_id = JsString::from_utf16(vec![0xd800]);
        replace_call_id(&mut events, "b", &raw_id);
        let options = CompareOptions {
            immediate_tool_call_ids: vec![raw_id],
            ..Default::default()
        };
        assert!(compare_events(&events, &events, &options, Concurrency::Free).is_ok());
        events.swap(2, 3);
        assert!(compare_events(&events, &events, &options, Concurrency::Free).is_err());
    }

    #[test]
    fn nonfinite_numbers_must_cross_the_explicit_json_observation_boundary() {
        for number in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let value = Value::Number(number);
            assert!(compare(&value, &value, &CompareOptions::default()).is_err());
            assert!(compare(&value, &Value::Null, &CompareOptions::default()).is_err());
        }
    }

    #[test]
    fn unrecognized_raw_event_types_remain_exact_strings() {
        let event = Value::Object(Map::from([
            ("type", raw(&[0xd800])),
            ("data", raw(&[0xdc00])),
        ]));
        assert!(
            compare_events(
                std::slice::from_ref(&event),
                std::slice::from_ref(&event),
                &CompareOptions::default(),
                Concurrency::Free
            )
            .is_ok()
        );
        let mut other = event.clone();
        other["type"] = raw(&[0xfffd]);
        assert!(
            compare_events(
                &[event],
                &[other],
                &CompareOptions::default(),
                Concurrency::Free
            )
            .is_err()
        );
    }
}
