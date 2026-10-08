//! Corpus discovery and explicit progress accounting during the port.
//!
//! An unsupported replay is a pending item, never conformance evidence. As
//! interpreters land they return compared output, and passing pending items
//! must be promoted before CI succeeds.

use crate::{
    CheckResult,
    coverage::MapMeta,
    integrity::{files, safe_relative},
    json, read, toml,
};
use pi_ai::utils::{
    js_value::{JsObject, JsValue, from_js_value},
    json_parse::parse_json,
};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Progress {
    Pending,
    Passing,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub id: String,
    pub status: Progress,
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorpusStatus {
    pub meta: MapMeta,
    #[serde(default)]
    pub scenario: Vec<Entry>,
    #[serde(default, rename = "function")]
    pub functions: Vec<Entry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Scenario {
    pub dsl: u32,
    pub id: String,
    pub layer: String,
    pub covers: Vec<String>,
    pub model: ModelRef,
    pub clock: Clock,
    pub system_prompt: Option<pi_ai::types::JsString>,
    pub thinking_level: Option<String>,
    pub provider: JsValue,
    #[serde(default)]
    pub tools: Vec<JsValue>,
    pub steering_mode: Option<String>,
    pub follow_up_mode: Option<String>,
    pub concurrency: Option<String>,
    #[serde(default)]
    pub platforms: Vec<String>,
    #[serde(default)]
    pub normalize: Vec<String>,
    #[serde(default)]
    pub variants: Vec<JsValue>,
    pub steps: Vec<JsValue>,
    #[serde(default)]
    pub initial_messages: Vec<JsValue>,
    pub options: Option<JsValue>,
    pub active_tools: Option<Vec<pi_ai::types::JsString>>,
    pub tool_execution: Option<String>,
    pub hooks: Option<JsValue>,
    #[serde(default)]
    pub subscribers: Vec<JsValue>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRef {
    #[serde(rename = "ref")]
    pub reference: String,
    pub value: Option<JsValue>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Clock {
    pub epoch_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FunctionMatrix {
    pub dsl: u32,
    pub id: String,
    pub clock: Clock,
    pub cases: Vec<FunctionCase>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FunctionCase {
    pub case: String,
    pub input: JsValue,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelMatrix {
    dsl: u32,
    id: String,
    clock: Clock,
    covers: Vec<String>,
    models: Vec<CatalogModel>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogModel {
    provider: String,
    id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Golden {
    pub case: String,
    pub input: Value,
    // Value preserves an explicit null result; an Option would conflate it
    // with an absent output. Presence is checked before deserialization.
    #[serde(default)]
    pub output: Value,
    pub error: Option<String>,
}

/// Preserve an explicit null output while rejecting null/absent error values.
pub fn parse_golden(value: Value) -> CheckResult<Golden> {
    let object = value.as_object().ok_or("golden row must be an object")?;
    if object.contains_key("output") == object.contains_key("error") {
        return Err("golden row must record exactly one of output or error".into());
    }
    if object.get("error").is_some_and(|error| !error.is_string()) {
        return Err("golden error must be a string".into());
    }
    serde_json::from_value(value).map_err(|error| format!("invalid golden row: {error}"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EventEnvelope {
    seq: u64,
    #[serde(rename = "type")]
    kind: String,
    #[allow(dead_code)]
    entries: u64,
    data: JsObject,
}

fn check_events(rows: Vec<JsValue>) -> CheckResult {
    if rows.is_empty() {
        return Err("recorded scenario has no events".into());
    }
    for (index, row) in rows.into_iter().enumerate() {
        let event: EventEnvelope =
            from_js_value(row).map_err(|error| format!("event {index}: {error}"))?;
        if event.seq != index as u64 || event.kind.trim().is_empty() {
            return Err(format!(
                "event {index}: invalid sequence index or event type"
            ));
        }
        if let Some(inner) = event.data.get("type")
            && inner.as_str() != Some(event.kind.as_str())
        {
            return Err(format!("event {index}: outer and inner event types differ"));
        }
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestRecord {
    call: u64,
    #[allow(dead_code)]
    model: JsObject,
    #[allow(dead_code)]
    context: JsObject,
    #[allow(dead_code)]
    options: JsObject,
}

fn check_requests(rows: Vec<JsValue>) -> CheckResult {
    for (index, row) in rows.into_iter().enumerate() {
        let request: RequestRecord =
            from_js_value(row).map_err(|error| format!("request {index}: {error}"))?;
        if request.call != index as u64 {
            return Err(format!("request {index}: invalid call index"));
        }
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FinalRecord {
    #[allow(dead_code)]
    state: JsObject,
    #[allow(dead_code)]
    queues: JsObject,
    #[allow(dead_code)]
    errors: Vec<JsValue>,
}

fn check_final(value: JsValue) -> CheckResult {
    from_js_value::<FinalRecord>(value)
        .map(|_| ())
        .map_err(|error| format!("invalid final record: {error}"))
}

// Input metadata is copied through JSON.parse/JSON.stringify by the recorder.
// Integral spellings may change (1.0 -> 1), but values must not. The behavior
// comparator's numeric tolerance and HTTP/ID normalization do not apply here.
fn copied_input_equal(expected: &Value, actual: &Value) -> bool {
    match (expected, actual) {
        (Value::Number(left), Value::Number(right)) => {
            let integer = |number: &serde_json::Number| {
                number
                    .as_i64()
                    .map(i128::from)
                    .or_else(|| number.as_u64().map(i128::from))
            };
            match (integer(left), integer(right)) {
                (Some(left), Some(right)) => left == right,
                (Some(integer), None) | (None, Some(integer)) => {
                    let number = if left.is_f64() { left } else { right };
                    number.as_f64().is_some_and(|float| {
                        float.is_finite()
                            && float.fract() == 0.0
                            && (i64::MIN as f64..=u64::MAX as f64).contains(&float)
                            && integer == float as i128
                    })
                }
                (None, None) => match (left.as_f64(), right.as_f64()) {
                    (Some(left), Some(right)) => {
                        left.is_finite() && right.is_finite() && left == right
                    }
                    _ => false,
                },
            }
        }
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| copied_input_equal(left, right))
        }
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, left)| {
                    right
                        .get(key)
                        .is_some_and(|right| copied_input_equal(left, right))
                })
        }
        _ => expected == actual,
    }
}

pub fn status(root: &Path) -> CheckResult<CorpusStatus> {
    let status: CorpusStatus = toml(&root.join("coverage/corpus-status.toml"))?;
    let pin: Value = json(&root.join("pin.json"))?;
    if pin["tag"] != status.meta.pin || pin["rev"] != status.meta.revision {
        return Err("corpus-status pin differs from pin.json".into());
    }
    for entries in [&status.scenario, &status.functions] {
        let mut ids = BTreeSet::new();
        for entry in entries {
            safe_relative(&entry.id)?;
            if !ids.insert(&entry.id) {
                return Err(format!("duplicate corpus status for {}", entry.id));
            }
            if entry.status == Progress::Pending
                && entry
                    .reason
                    .as_deref()
                    .is_none_or(|reason| reason.trim().is_empty())
            {
                return Err(format!("pending item {} needs a concrete reason", entry.id));
            }
        }
    }
    Ok(status)
}

pub fn load_scenario(root: &Path, id: &str) -> CheckResult<Scenario> {
    safe_relative(id)?;
    let path = root.join("corpus/scenarios").join(id).join("scenario.json");
    let value = read_js_json(&path)?;
    validate_js_dsl(root, &value)?;
    let scenario: Scenario =
        from_js_value(value).map_err(|error| format!("{}: {error}", path.display()))?;
    if scenario.id != id {
        return Err(format!(
            "recorded scenario ID {} differs from directory {id}",
            scenario.id
        ));
    }
    Ok(scenario)
}

pub fn validate_dsl(root: &Path, value: &Value) -> CheckResult {
    let schema: Value = json(&root.join("scenarios/schema.json"))?;
    let validator = jsonschema::options()
        .offline()
        .build(&schema)
        .map_err(|error| format!("invalid DSL schema: {error}"))?;
    validator
        .validate(value)
        .map_err(|error| format!("invalid DSL at {}: {error}", error.instance_path()))
}

/// Read runtime data without passing lone UTF-16 units through Rust String.
pub fn read_js_json(path: &Path) -> CheckResult<JsValue> {
    parse_json(&read(path)?).map_err(|error| format!("{}: {error}", path.display()))
}

pub fn read_js_jsonl(path: &Path) -> CheckResult<Vec<JsValue>> {
    read(path)?
        .lines()
        .enumerate()
        .map(|(line, text)| {
            if text.trim().is_empty() {
                return Err(format!(
                    "{}:{}: blank JSONL record",
                    path.display(),
                    line + 1
                ));
            }
            parse_json(text).map_err(|error| format!("{}:{}: {error}", path.display(), line + 1))
        })
        .collect()
}

// The DSL schema checks structure and fixed ASCII discriminants. Its JSON
// Schema library requires Unicode scalar strings; project only for that check.
// Execution, copied-input equality, and all expected/actual comparisons retain
// the original JsValue. Unknown UTF-16 object keys receive unique placeholders
// so even this structural projection cannot collapse two properties.
fn schema_projection(value: &JsValue) -> CheckResult<Value> {
    Ok(match value {
        JsValue::String(text) => Value::String(text.to_string_lossy()),
        JsValue::Array(items) => Value::Array(
            items
                .iter()
                .map(schema_projection)
                .collect::<CheckResult<_>>()?,
        ),
        JsValue::Object(object) => {
            let mut result = serde_json::Map::new();
            let mut used: BTreeSet<String> = object
                .keys()
                .filter_map(|key| key.as_str().map(str::to_owned))
                .collect();
            for (key, value) in object.iter() {
                let key = match key.as_str() {
                    Some(key) => key.to_owned(),
                    None => {
                        let mut key = format!("\0utf16:{}", pi_ai::utils::js_json::quote(key));
                        while !used.insert(key.clone()) {
                            key.push('\0');
                        }
                        key
                    }
                };
                result.insert(key, schema_projection(value)?);
            }
            Value::Object(result)
        }
        _ => value.to_json().map_err(|error| error.to_string())?,
    })
}
fn validate_js_dsl(root: &Path, value: &JsValue) -> CheckResult {
    validate_dsl(root, &schema_projection(value)?)
}

pub fn read_jsonl(path: &Path) -> CheckResult<Vec<Value>> {
    read(path)?
        .lines()
        .enumerate()
        .map(|(line, text)| {
            if text.trim().is_empty() {
                return Err(format!(
                    "{}:{}: blank JSONL record",
                    path.display(),
                    line + 1
                ));
            }
            serde_json::from_str(text)
                .map_err(|error| format!("{}:{}: {error}", path.display(), line + 1))
        })
        .collect()
}

/// Check every recorded/input artifact before pending behavior can suppress a
/// replay mismatch. Corrupt data and unknown fields are always failures.
pub fn check_structure(root: &Path) -> CheckResult {
    let status = status(root)?;
    let scenarios: BTreeSet<_> = status
        .scenario
        .iter()
        .map(|entry| entry.id.as_str())
        .collect();
    let functions: BTreeSet<_> = status
        .functions
        .iter()
        .map(|entry| entry.id.as_str())
        .collect();
    let mut function_inputs = BTreeMap::new();
    for path in files(&root.join("scenarios"))?.keys() {
        if path == "schema.json" || !path.ends_with(".json") {
            continue;
        }
        let value = read_js_json(&root.join("scenarios").join(path))?;
        validate_js_dsl(root, &value)?;
        if path.starts_with("models/") {
            let matrix: ModelMatrix =
                from_js_value(value).map_err(|error| format!("{path}: {error}"))?;
            if matrix.dsl != 1
                || matrix.covers.is_empty()
                || matrix.clock.epoch_ms > 9_007_199_254_740_991
            {
                return Err(format!("invalid model matrix {}", matrix.id));
            }
            let mut models = BTreeSet::new();
            for model in matrix.models {
                // Provider/model identifiers are catalog data, never file paths.
                // A model ID may legitimately contain slashes or a :batch suffix.
                if model.provider.is_empty() || model.id.is_empty() {
                    return Err(format!(
                        "model matrix {} has an empty identifier",
                        matrix.id
                    ));
                }
                if !models.insert((model.provider, model.id)) {
                    return Err(format!(
                        "model matrix {} contains duplicate models",
                        matrix.id
                    ));
                }
            }
        } else if path.starts_with("functions/") {
            let matrix: FunctionMatrix =
                from_js_value(value).map_err(|error| format!("{path}: {error}"))?;
            if !functions.contains(matrix.id.as_str()) {
                return Err(format!("function input {} has no corpus status", matrix.id));
            }
            let mut cases = BTreeSet::new();
            if matrix.cases.iter().any(|case| !cases.insert(&case.case)) {
                return Err(format!(
                    "function input {} contains duplicate cases",
                    matrix.id
                ));
            }
            if function_inputs.insert(matrix.id.clone(), matrix).is_some() {
                return Err(format!("duplicate function matrix in {path}"));
            }
        } else {
            let scenario: Scenario =
                from_js_value(value).map_err(|error| format!("{path}: {error}"))?;
            if !scenarios.contains(scenario.id.as_str()) {
                return Err(format!(
                    "scenario input {} has no corpus status",
                    scenario.id
                ));
            }
        }
    }
    let scenario_root = root.join("corpus/scenarios");
    if scenario_root.is_dir() {
        for path in files(&scenario_root)?
            .keys()
            .filter(|path| path.ends_with("/scenario.json"))
        {
            let id = path
                .strip_suffix("/scenario.json")
                .expect("filtered suffix");
            if !scenarios.contains(id) {
                return Err(format!("recorded scenario {id} has no corpus status"));
            }
            load_scenario(root, id)?;
            let input = root.join("scenarios").join(format!("{id}.json"));
            if read(&input)? != read(&scenario_root.join(path))? {
                return Err(format!("recorded scenario {id} is not an exact input copy"));
            }
            check_events(read_js_jsonl(&scenario_root.join(id).join("events.jsonl"))?)
                .map_err(|error| format!("scenario {id}: {error}"))?;
            check_requests(read_js_jsonl(
                &scenario_root.join(id).join("requests.jsonl"),
            )?)
            .map_err(|error| format!("scenario {id}: {error}"))?;
            check_final(read_js_json(&scenario_root.join(id).join("final.json"))?)
                .map_err(|error| format!("scenario {id}: {error}"))?;
        }
    }
    let function_root = root.join("corpus/functions");
    if function_root.is_dir() {
        for path in files(&function_root)?.keys() {
            let id = path
                .strip_suffix(".jsonl")
                .ok_or_else(|| format!("unexpected function artifact {path}"))?;
            if !functions.contains(id) {
                return Err(format!("recorded function {id} has no corpus status"));
            }
            let matrix = function_inputs
                .get(id)
                .ok_or_else(|| format!("recorded function {id} has no input matrix"))?;
            let rows = crate::functions::read_goldens(&function_root.join(path))?;
            if rows.len() != matrix.cases.len() {
                return Err(format!(
                    "recorded function {id} has {} cases but its input declares {}",
                    rows.len(),
                    matrix.cases.len()
                ));
            }
            let mut cases = BTreeSet::new();
            for (golden, input) in rows.into_iter().zip(&matrix.cases) {
                if golden.case.is_empty() || !cases.insert(golden.case.clone()) {
                    return Err(format!(
                        "function {id} has empty or duplicate case {:?}",
                        golden.case
                    ));
                }
                let preserved = match (input.input.to_json(), golden.input.to_json()) {
                    (Ok(input), Ok(copied)) => copied_input_equal(&input, &copied),
                    _ => input.input == golden.input,
                };
                if golden.case != input.case || !preserved {
                    return Err(format!(
                        "recorded function {id} does not preserve input case {:?}",
                        input.case
                    ));
                }
            }
            if cases.is_empty() {
                return Err(format!("function {id} has no recorded cases"));
            }
        }
    }
    for entry in &status.scenario {
        if entry.status == Progress::Passing
            && !scenario_root
                .join(&entry.id)
                .join("scenario.json")
                .is_file()
        {
            return Err(format!("passing scenario {} has no recording", entry.id));
        }
    }
    for entry in &status.functions {
        if entry.status == Progress::Passing
            && !function_root.join(format!("{}.jsonl", entry.id)).is_file()
        {
            return Err(format!("passing function {} has no recording", entry.id));
        }
    }
    Ok(())
}

pub fn check_progress(entry: &Entry, result: CheckResult) -> CheckResult {
    match (&entry.status, result) {
        (Progress::Pending, Ok(())) => Err(format!(
            "pending {} now passes; promote it to passing",
            entry.id
        )),
        (Progress::Pending, Err(error)) => {
            eprintln!("PENDING {}: {error}", entry.id);
            Ok(())
        }
        (Progress::Passing, result) => result,
    }
}

pub async fn replay_scenario(root: &Path, id: &str) -> CheckResult {
    if !root
        .join("corpus/scenarios")
        .join(id)
        .join("scenario.json")
        .is_file()
    {
        return Err("TypeScript scenario recording and Rust interpreter are pending".into());
    }
    let scenario = load_scenario(root, id)?;
    if id == "session/load-migrate-repair" {
        return crate::session_file_replay::replay(root).await;
    }
    if scenario.layer == "wire" {
        return crate::wire_replay::replay(root, id).await;
    }
    if scenario.layer == "agent" {
        let directory = root.join("corpus/scenarios").join(id);
        let input = read_js_json(&directory.join("scenario.json"))?;
        let actual = crate::agent_replay::replay(&input).await?;
        let options = crate::compare::CompareOptions::default();
        let concurrency = if scenario.concurrency.as_deref() == Some("free") {
            crate::compare::Concurrency::Free
        } else {
            crate::compare::Concurrency::Gated
        };
        crate::compare::compare_events(
            &read_js_jsonl(&directory.join("events.jsonl"))?,
            &actual.events,
            &options,
            concurrency,
        )
        .map_err(|error| format!("{id}/events.jsonl: {error}"))?;
        crate::compare::compare(
            &read_js_jsonl(&directory.join("requests.jsonl"))?.into(),
            &actual.requests.into(),
            &options,
        )
        .map_err(|error| format!("{id}/requests.jsonl: {error}"))?;
        return crate::compare::compare(
            &read_js_json(&directory.join("final.json"))?,
            &actual.final_record,
            &options,
        )
        .map_err(|error| format!("{id}/final.json: {error}"));
    }
    Err(format!(
        "Rust {} scenario interpreter is not implemented yet",
        scenario.layer
    ))
}

pub async fn replay_function(root: &Path, id: &str) -> CheckResult {
    if !root
        .join("corpus/functions")
        .join(format!("{id}.jsonl"))
        .is_file()
    {
        return Err(
            "TypeScript function recording and Rust function dispatcher are pending".into(),
        );
    }
    if crate::functions_step4::supports(id) {
        return crate::functions_step4::replay(root, id).await;
    }
    if id == "agent.clearedModel" {
        return crate::cleared_model::replay_recorded(root).await;
    }
    if id == "env.virtualTimers" {
        let matrix: FunctionMatrix =
            json(&root.join("scenarios/functions/env.virtualTimers.json"))?;
        let mut failures = Vec::new();
        for value in read_jsonl(&root.join("corpus/functions/env.virtualTimers.jsonl"))? {
            let golden = parse_golden(value)?;
            let actual = crate::timers::replay(matrix.clock.epoch_ms, &golden.input).await;
            let expected = golden.error.map_or_else(
                || serde_json::json!({"output": golden.output}),
                |error| serde_json::json!({"error": error}),
            );
            let actual = actual.map_or_else(
                |error| serde_json::json!({"error": error}),
                |output| serde_json::json!({"output": output}),
            );
            let mut options = crate::compare::CompareOptions {
                byte_exact: true,
                ..Default::default()
            };
            options.generated_ids.discover_protocol_ids = false;
            let expected = pi_ai::utils::js_value::JsValue::try_from(expected)
                .map_err(|error| error.to_string())?;
            let actual = pi_ai::utils::js_value::JsValue::try_from(actual)
                .map_err(|error| error.to_string())?;
            if let Err(error) = crate::compare::compare(&expected, &actual, &options) {
                failures.push(format!(
                    "{}: {error}; actual={}",
                    golden.case,
                    pi_ai::utils::js_json::stringify(&actual)
                ));
            }
        }
        return if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("\n"))
        };
    }
    if matches!(
        id,
        "api.transformMessages" | "api.buildParams" | "api.convertMessages"
    ) {
        return crate::functions_step2::replay(root, id);
    }
    crate::functions::replay(root, id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn check_events(rows: Vec<Value>) -> CheckResult {
        super::check_events(
            rows.into_iter()
                .map(JsValue::from_json_with_js_numbers)
                .collect(),
        )
    }
    fn check_requests(rows: Vec<Value>) -> CheckResult {
        super::check_requests(
            rows.into_iter()
                .map(JsValue::from_json_with_js_numbers)
                .collect(),
        )
    }
    fn check_final(value: Value) -> CheckResult {
        super::check_final(JsValue::from_json_with_js_numbers(value))
    }

    #[test]
    fn opaque_records_retain_lone_utf16_strings_and_distinct_keys() {
        let source = r#"{"seq":0,"type":"message_start","entries":0,"data":{"text":"\ud800","\ud800":1,"\ufffd":2}}"#;
        let event = parse_json(source).unwrap();
        super::check_events(vec![event.clone()]).unwrap();
        let projected = schema_projection(&event).unwrap();
        assert_eq!(projected["data"].as_object().unwrap().len(), 3);
        assert_eq!(
            event["data"]["text"]
                .as_js_str()
                .unwrap()
                .units()
                .collect::<Vec<_>>(),
            vec![0xd800]
        );
        let replacement = parse_json(&source.replace("\\ud800", "\\ufffd")).unwrap();
        assert_ne!(event, replacement);
        assert!(crate::compare::compare_unmodified(&event, &replacement).is_err());
    }

    #[test]
    fn event_artifacts_require_complete_zero_based_envelopes() {
        let valid = json!({"seq":0,"type":"agent_start","entries":0,"data":{}});
        assert!(check_events(vec![valid.clone()]).is_ok());
        assert!(check_events(Vec::new()).is_err());
        assert!(check_events(vec![Value::Null]).is_err());
        assert!(check_events(vec![valid.clone(), valid.clone()]).is_err());
        for (key, value) in [
            ("seq", json!(1)),
            ("seq", json!(0.5)),
            ("entries", json!(-1)),
            ("entries", Value::Null),
            ("type", json!("")),
            ("data", Value::Null),
            ("data", json!([])),
            ("data", json!({"type":"agent_end"})),
            ("data", json!({"type":null})),
            ("unknown", json!(true)),
        ] {
            let mut invalid = valid.clone();
            invalid[key] = value;
            assert!(check_events(vec![invalid]).is_err(), "{key}");
        }
        let mut matching_type = valid;
        matching_type["data"] = json!({"type":"agent_start"});
        assert!(check_events(vec![matching_type]).is_ok());
    }

    #[test]
    fn request_and_final_artifacts_reject_wrong_shapes() {
        let request = json!({"call":0,"model":{},"context":{},"options":{}});
        assert!(check_requests(vec![request.clone()]).is_ok());
        assert!(check_requests(Vec::new()).is_ok());
        assert!(check_requests(vec![request.clone(), request.clone()]).is_err());
        for key in ["model", "context", "options"] {
            let mut invalid = request.clone();
            invalid[key] = Value::Null;
            assert!(check_requests(vec![invalid]).is_err(), "{key}");
        }
        let final_record = json!({"state":{},"queues":{},"errors":[]});
        assert!(check_final(final_record.clone()).is_ok());
        assert!(check_final(Value::Null).is_err());
        assert!(check_final(json!({})).is_err());
        for key in ["state", "queues", "errors"] {
            let mut invalid = final_record.clone();
            invalid[key] = Value::Null;
            assert!(check_final(invalid).is_err(), "{key}");
        }
        let mut unknown = final_record;
        unknown["unknown"] = json!(true);
        assert!(check_final(unknown).is_err());
    }

    #[test]
    fn golden_rows_distinguish_null_outputs_from_missing_or_null_errors() {
        assert!(parse_golden(json!({"case":"null-result","input":{},"output":null})).is_ok());
        assert!(
            parse_golden(json!({"case":"error","input":{},"error":"expected failure"})).is_ok()
        );
        for value in [
            json!({"case":"null-error","input":{},"error":null}),
            json!({"case":"numeric-error","input":{},"error":1}),
            json!({"case":"missing","input":{}}),
            json!({"case":"both","input":{},"output":null,"error":"failure"}),
            json!({"case":"unknown","input":{},"output":true,"unknown":true}),
        ] {
            assert!(parse_golden(value).is_err());
        }
    }

    #[test]
    fn copied_inputs_allow_number_spelling_but_no_behavior_normalization() {
        assert!(copied_input_equal(
            &json!({"number":1}),
            &json!({"number":1.0})
        ));
        assert!(copied_input_equal(&json!(-0.0), &json!(0)));
        assert!(copied_input_equal(
            &json!({"a":1,"b":2}),
            &json!({"b":2.0,"a":1.0})
        ));
        assert!(!copied_input_equal(&json!(u64::MAX), &json!(u64::MAX - 1)));
        assert!(!copied_input_equal(
            &json!(u64::MAX),
            &json!(u64::MAX as f64)
        ));
        assert!(!copied_input_equal(
            &json!({"delayMs":0.5}),
            &json!({"delayMs":0.500_000_000_000_1})
        ));
        assert!(!copied_input_equal(&json!({}), &json!({"missing":null})));
        assert!(!copied_input_equal(
            &json!({"headers":{"user-agent":"a"}}),
            &json!({"headers":{"user-agent":"b"}})
        ));
        assert!(!copied_input_equal(
            &json!({"bodyRaw":"a"}),
            &json!({"bodyRaw":"b"})
        ));
        assert!(!copied_input_equal(
            &json!({"type":"session","id":"a"}),
            &json!({"type":"session","id":"b"})
        ));
    }

    #[test]
    fn pending_success_cannot_silently_remain_pending() {
        let entry = Entry {
            id: "agent/example".into(),
            status: Progress::Pending,
            reason: Some("interpreter pending".into()),
        };
        assert!(
            check_progress(&entry, Ok(()))
                .unwrap_err()
                .contains("promote")
        );
        assert!(check_progress(&entry, Err("unimplemented".into())).is_ok());
    }

    #[test]
    fn implemented_failures_are_never_suppressed() {
        let entry = Entry {
            id: "agent/example".into(),
            status: Progress::Passing,
            reason: None,
        };
        assert_eq!(
            check_progress(&entry, Err("events[3] differs".into())),
            Err("events[3] differs".into())
        );
    }
}
