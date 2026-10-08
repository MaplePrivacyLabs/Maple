//! Structural audit of the selected pinned source and deviation register.

use crate::{
    CheckResult,
    coverage::MapMeta,
    dependencies::PI_CRATES,
    integrity::{check_hash, safe_relative},
    json, toml,
};
use serde::Deserialize;
use std::{collections::BTreeSet, path::Path};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Selection {
    schema_version: u32,
    files: Vec<Source>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Source {
    upstream: String,
    class: String,
    #[serde(rename = "crate")]
    krate: String,
    sha256: String,
    rust_module: String,
    adaptations: Vec<String>,
    cut: Vec<String>,
    ranges: Vec<String>,
    keep: Vec<String>,
    status: String,
    reason: String,
}

fn range(value: &str) -> CheckResult {
    let (first, last) = value
        .split_once('-')
        .ok_or_else(|| format!("invalid selected line range {value:?}"))?;
    let first: usize = first
        .parse()
        .map_err(|_| format!("invalid selected line range {value:?}"))?;
    let last: usize = last
        .parse()
        .map_err(|_| format!("invalid selected line range {value:?}"))?;
    if first == 0 || last < first {
        return Err(format!("invalid selected line range {value:?}"));
    }
    Ok(())
}

/// During construction source modules may not exist yet. The final port gate
/// requires every declared source mapping to resolve to a real Rust module.
pub fn check_sources(reference: &Path, agent: &Path, complete: bool) -> CheckResult<usize> {
    let selection: Selection = json(&reference.join("selection/manifest.json"))?;
    let pin: serde_json::Value = json(&reference.join("pin.json"))?;
    if selection.schema_version != 1 {
        return Err("unsupported source selection schema".into());
    }
    if pin["tag"] == "v1.0.4" && selection.files.len() != 81 {
        return Err(format!(
            "the v1.0.4 source selection requires 81 files, found {}",
            selection.files.len()
        ));
    }
    let mut upstream = BTreeSet::new();
    let mut rust = BTreeSet::new();
    let mut pending = 0;
    for source in selection.files {
        safe_relative(&source.upstream)?;
        safe_relative(&source.rust_module)?;
        if !source.upstream.starts_with("packages/")
            || !source.upstream.contains("/src/")
            || !source.upstream.ends_with(".ts")
            || !upstream.insert(source.upstream.clone())
        {
            return Err(format!(
                "invalid or repeated upstream source {}",
                source.upstream
            ));
        }
        if !matches!(source.class.as_str(), "Port" | "Adapt")
            || !PI_CRATES[..4].contains(&source.krate.as_str())
            || !check_hash(&source.sha256)
            || !matches!(source.status.as_str(), "pending" | "ported" | "adapted")
            || source.reason.trim().is_empty()
        {
            return Err(format!(
                "invalid source classification or hash for {}",
                source.upstream
            ));
        }
        let expected_prefix = format!("crates/{}/src/", source.krate);
        if !source.rust_module.starts_with(&expected_prefix)
            || !source.rust_module.ends_with(".rs")
            || !rust.insert(source.rust_module.clone())
        {
            return Err(format!(
                "source mapping must have a distinct Rust module within its crate: {}",
                source.upstream
            ));
        }
        for value in source.ranges.iter().chain(&source.keep) {
            range(value)?;
        }
        if source.cut.iter().any(|name| name.trim().is_empty())
            || source.adaptations.iter().any(|note| note.trim().is_empty())
        {
            return Err(format!("empty cut or adaptation for {}", source.upstream));
        }
        if source.status == "pending" {
            pending += 1;
            if complete {
                return Err(format!(
                    "selected source {} is still pending",
                    source.upstream
                ));
            }
        } else if !agent.join(&source.rust_module).is_file() {
            return Err(format!(
                "selected source {} has no Rust module {}",
                source.upstream, source.rust_module
            ));
        } else if source.status == "adapted" && source.adaptations.is_empty() {
            return Err(format!(
                "adapted source {} needs an adaptation record",
                source.upstream
            ));
        }
    }
    Ok(pending)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Deviations {
    meta: MapMeta,
    #[serde(default)]
    deviation: Vec<Deviation>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Deviation {
    scenario: String,
    json_path: String,
    kind: String,
    rule: String,
    reason: String,
    approved_in: String,
}

pub fn check_deviations(reference: &Path) -> CheckResult {
    let deviations: Deviations = toml(&reference.join("coverage/deviations.toml"))?;
    let pin: serde_json::Value = json(&reference.join("pin.json"))?;
    if pin["tag"] != deviations.meta.pin || pin["rev"] != deviations.meta.revision {
        return Err("deviations pin differs from pin.json".into());
    }
    let mut locations = BTreeSet::new();
    for deviation in &deviations.deviation {
        safe_relative(&deviation.scenario)?;
        let exists = if let Some(function) = deviation.scenario.strip_prefix("functions/") {
            reference
                .join("corpus/functions")
                .join(format!("{function}.jsonl"))
                .is_file()
        } else {
            reference
                .join("corpus/scenarios")
                .join(&deviation.scenario)
                .join("scenario.json")
                .is_file()
        };
        if !exists {
            return Err(format!(
                "deviation references missing scenario {}",
                deviation.scenario
            ));
        }
        if !locations.insert((deviation.scenario.clone(), deviation.json_path.clone())) {
            return Err(format!(
                "duplicate deviation for {} {}",
                deviation.scenario, deviation.json_path
            ));
        }
        if !matches!(deviation.kind.as_str(), "language" | "host")
            || !deviation.json_path.starts_with('$')
            || deviation.reason.trim().is_empty()
            || deviation.rule.trim().is_empty()
        {
            return Err(format!(
                "invalid deviation at {} {}",
                deviation.scenario, deviation.json_path
            ));
        }
        let approval = deviation.approved_in.trim();
        if approval.is_empty()
            || ["pending", "tbd", "todo", "self", "unapproved"]
                .iter()
                .any(|word| approval.to_ascii_lowercase().contains(word))
        {
            return Err(format!(
                "deviation for {} needs a recorded owner approval",
                deviation.scenario
            ));
        }
        if !supported_deviation(deviation) {
            return Err(format!(
                "deviation rule {:?} for {} has no implemented comparison handler",
                deviation.rule, deviation.scenario
            ));
        }
    }
    Ok(())
}

fn supported_deviation(deviation: &Deviation) -> bool {
    owned_tool_definitions(deviation)
        || gated_live_partials(deviation)
        || cleared_model(deviation)
        || owned_summary_requests(deviation)
}

fn owned_summary_requests(deviation: &Deviation) -> bool {
    deviation.scenario == "functions/compaction.retryRequestOwnership"
        && deviation.json_path == crate::retry_request_ownership::RULE_PATH
        && deviation.kind == "language"
        && deviation.rule == crate::retry_request_ownership::RULE_ID
}

pub(crate) fn permits_owned_summary_requests(reference: &Path) -> CheckResult<bool> {
    check_deviations(reference)?;
    let deviations: Deviations = toml(&reference.join("coverage/deviations.toml"))?;
    Ok(deviations.deviation.iter().any(owned_summary_requests))
}

fn owned_tool_definitions(deviation: &Deviation) -> bool {
    deviation.scenario == "functions/transcript.toolOwnership"
        && deviation.json_path == "$.after"
        && deviation.kind == "language"
        && deviation.rule == "owned-tool-definitions"
}

/// Behavior-specific replay handlers require the corresponding recorded owner
/// authorization. An unknown or relocated rule is never a generic ignore path.
pub(crate) fn permits_owned_tool_definitions(reference: &Path) -> CheckResult<bool> {
    check_deviations(reference)?;
    let deviations: Deviations = toml(&reference.join("coverage/deviations.toml"))?;
    Ok(deviations.deviation.iter().any(owned_tool_definitions))
}

const GATED_WIRE_SCENARIOS: &[&str] = &[
    "wire/basic-text-usage",
    "wire/streamed-tool-calls",
    "wire/reasoning-fields",
    "wire/system-collapse-and-tools",
    "wire/stream-failures",
    "wire/request-repair",
];

fn gated_live_partials(deviation: &Deviation) -> bool {
    GATED_WIRE_SCENARIOS.contains(&deviation.scenario.as_str())
        && deviation.json_path == "$.events[*].data.partial"
        && deviation.kind == "language"
        && deviation.rule == "gated-live-partial-observation"
}

/// Authorizes an observation schedule, never a field exclusion. Wire replay
/// validates explicit input gates and compares every recorded value normally.
pub(crate) fn permits_gated_live_partials(reference: &Path, id: &str) -> CheckResult<bool> {
    check_deviations(reference)?;
    let deviations: Deviations = toml(&reference.join("coverage/deviations.toml"))?;
    Ok(deviations
        .deviation
        .iter()
        .any(|deviation| deviation.scenario == id && gated_live_partials(deviation)))
}

fn cleared_model(deviation: &Deviation) -> bool {
    deviation.scenario == "functions/agent.clearedModel"
        && deviation.json_path == "$.providerInvocations"
        && deviation.kind == "language"
        && deviation.rule == "agent-cleared-model-typed-provider-boundary"
}

pub(crate) fn permits_cleared_model(reference: &Path) -> CheckResult<bool> {
    check_deviations(reference)?;
    let deviations: Deviations = toml(&reference.join("coverage/deviations.toml"))?;
    Ok(deviations.deviation.iter().any(cleared_model))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduling_authorization_cannot_be_relocated_or_used_for_owned_inputs() {
        let mut deviation = Deviation {
            scenario: GATED_WIRE_SCENARIOS[0].into(),
            json_path: "$.events[*].data.partial".into(),
            kind: "language".into(),
            rule: "gated-live-partial-observation".into(),
            reason: "test".into(),
            approved_in: "owner".into(),
        };
        assert!(supported_deviation(&deviation));
        assert!(!owned_tool_definitions(&deviation));
        deviation.scenario = "wire/unregistered".into();
        assert!(!supported_deviation(&deviation));
        deviation.scenario = GATED_WIRE_SCENARIOS[0].into();
        deviation.json_path = "$.events".into();
        assert!(!supported_deviation(&deviation));
    }
}
