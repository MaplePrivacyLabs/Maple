//! Canonical session/load-migrate-repair observations, using real session files.
use crate::{
    CheckResult,
    replay::{read_js_json, read_js_jsonl},
};
use pi_ai::types::{JsObject, JsValue};
use std::path::Path;

pub async fn replay(root: &Path) -> CheckResult {
    let id = "session/load-migrate-repair";
    let directory = root.join("corpus/scenarios").join(id);
    let input = read_js_json(&directory.join("scenario.json"))?;
    let epoch = input["clock"]["epochMs"]
        .as_i64()
        .ok_or("file scenario clock is not i64")?;
    let cwd = input["options"]["cwd"]
        .as_str()
        .ok_or("file scenario requires explicit cwd")?;
    let steps = input["steps"]
        .as_array()
        .ok_or("file scenario steps is not an array")?;
    let mut files = Vec::new();
    let mut events = Vec::new();
    for (seq, step) in steps.iter().enumerate() {
        let fixture = step
            .get("sessionFile")
            .ok_or("file scenario step lacks sessionFile")?;
        let observation = crate::session_files::observe_file(fixture, epoch, cwd).await?;
        let entries = observation["steps"]
            .as_array()
            .ok_or("file observation has no steps")?
            .iter()
            .rev()
            .find_map(|step| step.get("entries").and_then(JsValue::as_array))
            .map_or(0, Vec::len);
        let mut data = JsObject::from([("type", "session_file".into())]);
        data.extend(
            observation
                .as_object()
                .ok_or("file observation is not an object")?
                .clone(),
        );
        events.push(
            JsObject::from([
                ("seq", (seq as f64).into()),
                ("type", "session_file".into()),
                ("entries", (entries as f64).into()),
                ("data", data.into()),
            ])
            .into(),
        );
        files.push(observation);
    }
    let final_record: JsValue = JsObject::from([
        ("state", JsObject::from([("files", files.into())]).into()),
        ("queues", JsObject::new().into()),
        ("errors", Vec::<JsValue>::new().into()),
    ])
    .into();
    for (name, expected, actual) in [
        (
            "events.jsonl",
            JsValue::Array(read_js_jsonl(&directory.join("events.jsonl"))?),
            JsValue::Array(events),
        ),
        (
            "requests.jsonl",
            JsValue::Array(read_js_jsonl(&directory.join("requests.jsonl"))?),
            JsValue::Array(vec![]),
        ),
        (
            "final.json",
            read_js_json(&directory.join("final.json"))?,
            final_record,
        ),
    ] {
        crate::compare::compare_unmodified(&expected, &actual)
            .map_err(|error| format!("{id}/{name}: {error}"))?;
    }
    Ok(())
}
