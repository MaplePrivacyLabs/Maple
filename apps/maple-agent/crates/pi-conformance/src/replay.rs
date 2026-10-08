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
    pub system_prompt: Option<String>,
    pub thinking_level: Option<String>,
    pub provider: Value,
    #[serde(default)]
    pub tools: Vec<Value>,
    pub steering_mode: Option<String>,
    pub follow_up_mode: Option<String>,
    pub concurrency: Option<String>,
    #[serde(default)]
    pub platforms: Vec<String>,
    #[serde(default)]
    pub normalize: Vec<String>,
    #[serde(default)]
    pub variants: Vec<Value>,
    pub steps: Vec<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRef {
    #[serde(rename = "ref")]
    pub reference: String,
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
    pub input: Value,
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
    data: serde_json::Map<String, Value>,
}

fn check_events(rows: Vec<Value>) -> CheckResult {
    if rows.is_empty() {
        return Err("recorded scenario has no events".into());
    }
    for (index, row) in rows.into_iter().enumerate() {
        let event: EventEnvelope =
            serde_json::from_value(row).map_err(|error| format!("event {index}: {error}"))?;
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
    model: serde_json::Map<String, Value>,
    #[allow(dead_code)]
    context: serde_json::Map<String, Value>,
    #[allow(dead_code)]
    options: serde_json::Map<String, Value>,
}

fn check_requests(rows: Vec<Value>) -> CheckResult {
    for (index, row) in rows.into_iter().enumerate() {
        let request: RequestRecord =
            serde_json::from_value(row).map_err(|error| format!("request {index}: {error}"))?;
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
    state: serde_json::Map<String, Value>,
    #[allow(dead_code)]
    queues: serde_json::Map<String, Value>,
    #[allow(dead_code)]
    errors: Vec<Value>,
}

fn check_final(value: Value) -> CheckResult {
    serde_json::from_value::<FinalRecord>(value)
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
    let value: Value = json(&path)?;
    validate_dsl(root, &value)?;
    let scenario: Scenario =
        serde_json::from_value(value).map_err(|error| format!("{}: {error}", path.display()))?;
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
        let value: Value = json(&root.join("scenarios").join(path))?;
        validate_dsl(root, &value)?;
        if path.starts_with("models/") {
            let matrix: ModelMatrix =
                serde_json::from_value(value).map_err(|error| format!("{path}: {error}"))?;
            if matrix.dsl != 1
                || matrix.covers.is_empty()
                || matrix.clock.epoch_ms > 9_007_199_254_740_991
            {
                return Err(format!("invalid model matrix {}", matrix.id));
            }
            let mut models = BTreeSet::new();
            for model in matrix.models {
                safe_relative(&format!("{}/{}", model.provider, model.id))?;
                if !models.insert((model.provider, model.id)) {
                    return Err(format!(
                        "model matrix {} contains duplicate models",
                        matrix.id
                    ));
                }
            }
        } else if path.starts_with("functions/") {
            let matrix: FunctionMatrix =
                serde_json::from_value(value).map_err(|error| format!("{path}: {error}"))?;
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
                serde_json::from_value(value).map_err(|error| format!("{path}: {error}"))?;
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
            check_events(read_jsonl(&scenario_root.join(id).join("events.jsonl"))?)
                .map_err(|error| format!("scenario {id}: {error}"))?;
            check_requests(read_jsonl(&scenario_root.join(id).join("requests.jsonl"))?)
                .map_err(|error| format!("scenario {id}: {error}"))?;
            check_final(json(&scenario_root.join(id).join("final.json"))?)
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
            let rows = read_jsonl(&function_root.join(path))?;
            if rows.len() != matrix.cases.len() {
                return Err(format!(
                    "recorded function {id} has {} cases but its input declares {}",
                    rows.len(),
                    matrix.cases.len()
                ));
            }
            let mut cases = BTreeSet::new();
            for (row, input) in rows.into_iter().zip(&matrix.cases) {
                let golden = parse_golden(row).map_err(|error| format!("{path}: {error}"))?;
                if golden.case.is_empty() || !cases.insert(golden.case.clone()) {
                    return Err(format!(
                        "function {id} has empty or duplicate case {:?}",
                        golden.case
                    ));
                }
                if golden.case != input.case || !copied_input_equal(&input.input, &golden.input) {
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
            if let Err(error) = crate::compare::compare(&expected, &actual, &options) {
                failures.push(format!("{}: {error}; actual={actual}", golden.case));
            }
        }
        return if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("\n"))
        };
    }
    Err(format!(
        "Rust function dispatcher for {id} is not implemented yet"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
