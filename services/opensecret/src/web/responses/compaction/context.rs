//! Read-only context planning and checkpoint projection.
//!
//! No source item disappears merely because a suffix advances. An item is either
//! replayed verbatim or explicitly covered by a successfully published summary.
//! This module performs no persistence, inference, billing, or tool execution.

use super::policy::{CompactionPolicy, MAX_SOURCE_BATCH_BYTES};
use crate::tokens::count_tokens;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

pub const CHECKPOINT_VERSION: u32 = 1;

pub use crate::models::response_compaction::{ContextItemKey, ContextItemKind};

#[derive(Debug, Clone)]
pub struct ContextItem {
    pub key: ContextItemKey,
    pub response_uuid: Option<Uuid>,
    /// The exact existing provider projection, including allowed reasoning replay.
    pub prompt_messages: Vec<Value>,
    /// Historical data only; private reasoning is never summary source material.
    pub summary_messages: Vec<Value>,
    pub estimated_tokens: usize,
    /// Database ciphertext size, independently of its projected model tokens.
    /// Images may contribute large ciphertext but only a small prompt marker.
    pub source_bytes: usize,
    pub settled: bool,
    pub tool_call_id: Option<Uuid>,
    /// Request-derived input provenance (for example, automatic descriptions).
    /// This is protected only while its owning Response is current; older input
    /// remains compactable and still determines active-memory placement.
    pub protected_input: bool,
    /// A finished historical tool attempt without its counterpart is retained as
    /// an identity, not replayed as an invalid provider tool exchange.
    pub historical_unmatched: bool,
    /// Metadata-only items participate in planning, but must never masquerade as
    /// an empty verbatim message in the final provider prompt.
    pub payload_loaded: bool,
}

impl ContextItem {
    pub fn from_prompt(
        key: ContextItemKey,
        response_uuid: Option<Uuid>,
        prompt_messages: Vec<Value>,
        estimated_tokens: usize,
        settled: bool,
        tool_call_id: Option<Uuid>,
    ) -> Self {
        let summary_messages = if key.kind == ContextItemKind::Reasoning {
            Vec::new()
        } else {
            prompt_messages
                .iter()
                .cloned()
                .map(|mut message| {
                    if let Some(object) = message.as_object_mut() {
                        object.remove("reasoning");
                        object.remove("reasoning_content");
                    }
                    message
                })
                .collect()
        };
        Self {
            key,
            response_uuid,
            prompt_messages,
            summary_messages,
            estimated_tokens,
            source_bytes: estimated_tokens.saturating_mul(4),
            settled,
            tool_call_id,
            protected_input: false,
            historical_unmatched: false,
            payload_loaded: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ContextSnapshot {
    pub instructions: Vec<Value>,
    pub instruction_tokens: usize,
    /// Complete, deterministic source order, including already covered items.
    pub items: Vec<ContextItem>,
    pub current_user: ContextItemKey,
    pub current_response: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckpointSection {
    pub summary: String,
    pub covered: Vec<ContextItemKey>,
    /// History has no anchor; active memory follows this exact user item.
    pub anchor: Option<ContextItemKey>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckpointPayload {
    pub version: u32,
    pub history: Option<CheckpointSection>,
    pub active: Option<CheckpointSection>,
}

impl Default for CheckpointPayload {
    fn default() -> Self {
        Self {
            version: CHECKPOINT_VERSION,
            history: None,
            active: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionKind {
    History,
    Active,
}

#[derive(Debug, Clone)]
pub struct SummaryWork {
    pub kind: SectionKind,
    pub anchor: Option<ContextItemKey>,
    pub previous_summary: Option<String>,
    /// Newly covered source. When folding an active section, its authenticated
    /// memory follows its triggering user and request-derived input prefix.
    pub items: Vec<ContextItem>,
    /// Complete eventual coverage, including a previous checkpoint's coverage.
    pub covered: Vec<ContextItemKey>,
    pub folds_previous_active: bool,
}

#[derive(Debug, Clone)]
pub struct ContextPlan {
    /// Proposed work only. Final projection is performed after execution and
    /// publication; a plan never masquerades as an already compacted prompt.
    pub summary_work: Vec<SummaryWork>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextError {
    Snapshot(&'static str),
    Checkpoint(&'static str),
    Summary(&'static str),
}

impl std::fmt::Display for ContextError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Snapshot(reason) => {
                write!(formatter, "invalid context snapshot: {reason}")
            }
            Self::Checkpoint(reason) => write!(formatter, "invalid checkpoint: {reason}"),
            Self::Summary(reason) => write!(formatter, "invalid summary: {reason}"),
        }
    }
}

impl std::error::Error for ContextError {}

#[derive(Debug)]
struct AtomicGroup {
    start: usize,
    end: usize,
    complete: bool,
}

fn atomic_groups(snapshot: &ContextSnapshot) -> Vec<AtomicGroup> {
    let items = &snapshot.items;
    let mut ends: Vec<usize> = (0..items.len()).collect();
    let mut incomplete = HashSet::new();
    let mut calls = HashMap::new();
    let mut outputs = HashMap::new();
    for (index, item) in items.iter().enumerate() {
        if item.historical_unmatched && item.settled {
            continue;
        }
        match item.key.kind {
            ContextItemKind::ToolCall => {
                calls.insert(item.tool_call_id.unwrap_or(item.key.uuid), index);
            }
            ContextItemKind::ToolOutput => {
                if let Some(id) = item.tool_call_id {
                    outputs.insert(id, index);
                } else {
                    incomplete.insert(index);
                }
            }
            _ => {}
        }
    }
    for (id, &call) in &calls {
        if let Some(&output) = outputs.get(id).filter(|&&output| output > call) {
            ends[call] = output;
        } else {
            incomplete.insert(call);
        }
    }
    for (id, &output) in &outputs {
        if calls.get(id).is_none_or(|&call| call >= output) {
            incomplete.insert(output);
        }
    }
    // Reasoning keys are covered together with their following output, although
    // their text never enters summary_messages. Do not cross a user boundary or
    // attach reasoning to another concurrently running response.
    for (index, item) in items.iter().enumerate() {
        if item.key.kind != ContextItemKind::Reasoning {
            continue;
        }
        for (following, next) in items.iter().enumerate().skip(index + 1) {
            if next.response_uuid != item.response_uuid {
                continue;
            }
            match next.key.kind {
                ContextItemKind::Assistant | ContextItemKind::ToolCall => {
                    ends[index] = following;
                    break;
                }
                ContextItemKind::User | ContextItemKind::ToolOutput => break,
                ContextItemKind::Reasoning => {}
            }
        }
    }
    // Merge overlapping intervals. Parallel calls/results and interleaved rows
    // remain in source order, and a cut cannot bisect either relationship.
    let mut result = Vec::new();
    let mut start = 0;
    while start < items.len() {
        let mut end = ends[start];
        let mut cursor = start;
        while cursor <= end {
            end = end.max(ends[cursor]);
            cursor += 1;
        }
        result.push(AtomicGroup {
            start,
            end,
            complete: !(start..=end).any(|index| incomplete.contains(&index)),
        });
        start = end + 1;
    }
    result
}

fn validated_coverage(
    snapshot: &ContextSnapshot,
    checkpoint: &Option<CheckpointPayload>,
) -> Result<HashSet<ContextItemKey>, ContextError> {
    let mut positions = HashMap::new();
    for (index, item) in snapshot.items.iter().enumerate() {
        if positions.insert(item.key, index).is_some() {
            return Err(ContextError::Snapshot("duplicate item identity"));
        }
    }
    let current = positions
        .get(&snapshot.current_user)
        .copied()
        .ok_or(ContextError::Snapshot("current user is absent"))?;
    if snapshot.current_user.kind != ContextItemKind::User {
        return Err(ContextError::Snapshot("current input is not a user item"));
    }
    let mut covered = HashSet::new();
    if let Some(checkpoint) = checkpoint {
        if checkpoint.version != CHECKPOINT_VERSION {
            return Err(ContextError::Checkpoint("unsupported format version"));
        }
        for (kind, section) in [
            (SectionKind::History, checkpoint.history.as_ref()),
            (SectionKind::Active, checkpoint.active.as_ref()),
        ] {
            let Some(section) = section else { continue };
            if section.summary.trim().is_empty() || section.covered.is_empty() {
                return Err(ContextError::Checkpoint("empty section"));
            }
            let anchor = match (kind, section.anchor) {
                (SectionKind::History, None) => None,
                (SectionKind::Active, Some(key)) if key.kind == ContextItemKind::User => Some(
                    *positions
                        .get(&key)
                        .ok_or(ContextError::Checkpoint("active anchor is absent"))?,
                ),
                _ => return Err(ContextError::Checkpoint("invalid section anchor")),
            };
            for key in &section.covered {
                let position = *positions
                    .get(key)
                    .ok_or(ContextError::Checkpoint("covered source is absent"))?;
                if *key == snapshot.current_user
                    || !snapshot.items[position].settled
                    || (snapshot.items[position].protected_input
                        && snapshot.items[position].response_uuid
                            == Some(snapshot.current_response))
                {
                    return Err(ContextError::Checkpoint(
                        "required or unsettled source covered",
                    ));
                }
                if !covered.insert(*key) {
                    return Err(ContextError::Checkpoint("duplicate coverage"));
                }
                if kind == SectionKind::History && position >= current {
                    return Err(ContextError::Checkpoint("history crosses current input"));
                }
                if anchor.is_some_and(|anchor| position <= anchor) {
                    return Err(ContextError::Checkpoint("active coverage precedes anchor"));
                }
                if anchor.is_some_and(|anchor| anchor < current) && position >= current {
                    return Err(ContextError::Checkpoint(
                        "older active section crosses current input",
                    ));
                }
            }
        }
        if checkpoint.active.as_ref().is_some_and(|section| {
            section
                .anchor
                .is_some_and(|anchor| covered.contains(&anchor))
        }) {
            return Err(ContextError::Checkpoint("active anchor is covered"));
        }
    }
    for group in atomic_groups(snapshot) {
        let count = snapshot.items[group.start..=group.end]
            .iter()
            .filter(|item| covered.contains(&item.key))
            .count();
        if count != 0 && (count != group.end - group.start + 1 || !group.complete) {
            return Err(ContextError::Checkpoint(
                "partial tool or reasoning group covered",
            ));
        }
    }
    Ok(covered)
}

fn summary_message(section: &CheckpointSection) -> Value {
    // A summary is historical assistant data, never a replacement system prompt.
    json!({
        "role": "assistant",
        "content": format!("[Conversation memory: summarized earlier context]\n{}", section.summary)
    })
}

fn active_memory_after(
    snapshot: &ContextSnapshot,
    section: &CheckpointSection,
) -> Option<ContextItemKey> {
    let anchor = section.anchor?;
    let position = snapshot.items.iter().position(|item| item.key == anchor)?;
    let response = snapshot.items[position].response_uuid;
    let mut after = anchor;
    for item in snapshot.items.iter().skip(position + 1) {
        if item.response_uuid != response {
            continue;
        }
        if !item.protected_input {
            break;
        }
        after = item.key;
    }
    Some(after)
}

pub fn project(
    snapshot: &ContextSnapshot,
    checkpoint: &Option<CheckpointPayload>,
) -> Result<(Vec<Value>, usize), ContextError> {
    let covered = validated_coverage(snapshot, checkpoint)?;
    if snapshot
        .items
        .iter()
        .any(|item| !covered.contains(&item.key) && !item.payload_loaded)
    {
        return Err(ContextError::Snapshot("uncovered payload is not loaded"));
    }
    let mut messages = snapshot.instructions.clone();
    let active_after = checkpoint
        .as_ref()
        .and_then(|value| value.active.as_ref())
        .and_then(|section| active_memory_after(snapshot, section));
    if let Some(section) = checkpoint.as_ref().and_then(|value| value.history.as_ref()) {
        messages.push(summary_message(section));
    }
    for item in &snapshot.items {
        if !covered.contains(&item.key) {
            messages.extend(item.prompt_messages.iter().cloned());
        }
        if let Some(section) = checkpoint.as_ref().and_then(|value| value.active.as_ref()) {
            if active_after == Some(item.key) {
                messages.push(summary_message(section));
            }
        }
    }
    Ok((
        messages,
        projected_token_estimate(snapshot, checkpoint, &covered),
    ))
}

/// Estimate a complete projection from metadata without constructing messages.
/// Unlike `project`, this is safe for explicitly unloaded planning items.
pub fn estimate_tokens(
    snapshot: &ContextSnapshot,
    checkpoint: &Option<CheckpointPayload>,
) -> Result<usize, ContextError> {
    let covered = validated_coverage(snapshot, checkpoint)?;
    Ok(projected_token_estimate(snapshot, checkpoint, &covered))
}

/// Hydrate only the planned helper input. Pure planning may have inserted an
/// older active memory after its anchor; retain that insertion after replacing
/// the raw source placeholders. Reasoning identities need no private payload:
/// reasoning text is never summary source material.
pub fn hydrate_plan(
    plan: &ContextPlan,
    snapshot: &ContextSnapshot,
) -> Result<ContextPlan, ContextError> {
    let by_key: HashMap<_, _> = snapshot.items.iter().map(|item| (item.key, item)).collect();
    let mut hydrated = plan.clone();
    for work in &mut hydrated.summary_work {
        for item in &mut work.items {
            let source = by_key
                .get(&item.key)
                .ok_or(ContextError::Snapshot("summary source is absent"))?;
            if !source.payload_loaded && source.key.kind != ContextItemKind::Reasoning {
                return Err(ContextError::Snapshot("summary payload is not loaded"));
            }
            let raw_messages = if item.key.kind == ContextItemKind::Reasoning {
                0
            } else {
                item.prompt_messages.len()
            };
            let injected = item
                .summary_messages
                .get(raw_messages..)
                .ok_or(ContextError::Snapshot(
                    "summary source projection is invalid",
                ))?
                .to_vec();
            *item = (**source).clone();
            item.summary_messages.extend(injected);
        }
    }
    Ok(hydrated)
}

fn projected_token_estimate(
    snapshot: &ContextSnapshot,
    checkpoint: &Option<CheckpointPayload>,
    covered: &HashSet<ContextItemKey>,
) -> usize {
    let raw_tokens = snapshot
        .items
        .iter()
        .filter(|item| !covered.contains(&item.key))
        .map(|item| item.estimated_tokens)
        .fold(snapshot.instruction_tokens, usize::saturating_add);
    checkpoint
        .iter()
        .flat_map(|checkpoint| checkpoint.history.iter().chain(checkpoint.active.iter()))
        .map(|section| count_tokens(&summary_message(section).to_string()))
        .fold(raw_tokens, usize::saturating_add)
}

#[cfg(test)]
pub fn plan(
    snapshot: &ContextSnapshot,
    checkpoint: &Option<CheckpointPayload>,
    policy: &CompactionPolicy,
    force: bool,
) -> Result<ContextPlan, ContextError> {
    plan_batch(snapshot, checkpoint, policy, force, usize::MAX)
}

/// Plan one item-aligned history batch. A single atomic tool/reasoning group may
/// exceed the target; its complete transcript must still be summarized before
/// publication. An older active section and its anchor are one indivisible unit.
pub fn plan_batch(
    snapshot: &ContextSnapshot,
    checkpoint: &Option<CheckpointPayload>,
    policy: &CompactionPolicy,
    force: bool,
    max_history_batch_tokens: usize,
) -> Result<ContextPlan, ContextError> {
    let covered = validated_coverage(snapshot, checkpoint)?;
    let estimated_tokens = projected_token_estimate(snapshot, checkpoint, &covered);
    let groups = atomic_groups(snapshot);
    let current = snapshot
        .items
        .iter()
        .position(|item| item.key == snapshot.current_user)
        .ok_or(ContextError::Snapshot("current user is absent"))?;
    let mut retained = HashSet::new();
    let mut recent_tokens = 0usize;
    for (index, group) in groups.iter().enumerate().rev() {
        let raw_tokens = snapshot.items[group.start..=group.end]
            .iter()
            .filter(|item| !covered.contains(&item.key))
            .map(|item| item.estimated_tokens)
            .fold(0usize, usize::saturating_add);
        if raw_tokens > 0 && recent_tokens < policy.keep_recent_tokens {
            retained.insert(index);
            recent_tokens = recent_tokens.saturating_add(raw_tokens);
        }
    }
    let mut result = ContextPlan {
        summary_work: Vec::new(),
    };
    if !force && estimated_tokens <= policy.trigger_tokens {
        return Ok(result);
    }

    let old_active = checkpoint
        .as_ref()
        .and_then(|value| value.active.as_ref())
        .filter(|section| section.anchor != Some(snapshot.current_user));
    let fold_through = old_active
        .into_iter()
        .flat_map(|section| section.covered.iter().copied().chain(section.anchor))
        .filter_map(|key| snapshot.items.iter().position(|item| item.key == key))
        .max();
    let fold_span = old_active
        .and_then(|section| section.anchor)
        .and_then(|anchor| snapshot.items.iter().position(|item| item.key == anchor))
        .zip(fold_through)
        .filter(|(_, end)| *end < current)
        .and_then(|(start, end)| {
            let first = groups
                .iter()
                .position(|group| group.start <= start && start <= group.end)?;
            let last = groups
                .iter()
                .position(|group| group.start <= end && end <= group.end)?;
            Some((first, last))
        });
    let mut history = Vec::new();
    let mut active = Vec::new();
    let mut history_tokens = 0usize;
    let mut active_tokens = 0usize;
    let mut history_bytes = 0usize;
    let mut active_bytes = 0usize;
    let mut group_index = 0usize;
    while let Some(group) = groups.get(group_index) {
        if group.end >= current {
            break;
        }
        let last = fold_span
            .filter(|(first, _)| *first == group_index)
            .map_or(group_index, |(_, last)| last);
        let batch_groups = &groups[group_index..=last];
        let items = &snapshot.items[group.start..=groups[last].end];
        if items.iter().all(|item| covered.contains(&item.key)) {
            group_index = last + 1;
            continue;
        }
        let eligible = batch_groups.iter().all(|group| group.complete)
            && items.iter().all(|item| {
                item.settled
                    && !(item.protected_input
                        && item.response_uuid == Some(snapshot.current_response))
            });
        let must_fold = fold_through.is_some_and(|end| group.start <= end);
        let keep_raw = (group_index..=last).any(|index| retained.contains(&index));
        if !eligible || (keep_raw && !must_fold) {
            break;
        }
        let tokens = items
            .iter()
            .filter(|item| !covered.contains(&item.key))
            .map(|item| item.estimated_tokens)
            .fold(0usize, usize::saturating_add);
        let bytes = items
            .iter()
            .filter(|item| !covered.contains(&item.key))
            .map(|item| item.source_bytes)
            .fold(0usize, usize::saturating_add);
        if !history.is_empty()
            && (history_tokens.saturating_add(tokens) > max_history_batch_tokens
                || history_bytes.saturating_add(bytes) > MAX_SOURCE_BATCH_BYTES)
        {
            break;
        }
        history.extend(
            items
                .iter()
                .filter(|item| !covered.contains(&item.key))
                .cloned(),
        );
        history_tokens = history_tokens.saturating_add(tokens);
        history_bytes = history_bytes.saturating_add(bytes);
        group_index = last + 1;
    }
    for (index, group) in groups.iter().enumerate() {
        if group.start <= current {
            continue;
        }
        let items = &snapshot.items[group.start..=group.end];
        if items.iter().all(|item| covered.contains(&item.key)) {
            continue;
        }
        // Current image descriptions are input, rather than earlier model work.
        // Retain them without fencing off later completed tool exchanges.
        if items.iter().all(|item| {
            item.protected_input && item.response_uuid == Some(snapshot.current_response)
        }) {
            continue;
        }
        let eligible = group.complete
            && items.iter().all(|item| {
                item.settled
                    && !item.protected_input
                    && item.response_uuid == Some(snapshot.current_response)
                    && item.key.kind != ContextItemKind::User
            });
        if !eligible || retained.contains(&index) {
            break;
        }
        let tokens = items
            .iter()
            .filter(|item| !covered.contains(&item.key))
            .map(|item| item.estimated_tokens)
            .fold(0usize, usize::saturating_add);
        let bytes = items
            .iter()
            .filter(|item| !covered.contains(&item.key))
            .map(|item| item.source_bytes)
            .fold(0usize, usize::saturating_add);
        if !active.is_empty()
            && (active_tokens.saturating_add(tokens) > max_history_batch_tokens
                || active_bytes.saturating_add(bytes) > MAX_SOURCE_BATCH_BYTES)
        {
            break;
        }
        active.extend(
            items
                .iter()
                .filter(|item| !covered.contains(&item.key))
                .cloned(),
        );
        active_tokens = active_tokens.saturating_add(tokens);
        active_bytes = active_bytes.saturating_add(bytes);
    }
    let folds_previous_active = old_active.is_some_and(|section| {
        section
            .anchor
            .is_some_and(|anchor| history.iter().any(|item| item.key == anchor))
            && section.covered.iter().all(|key| {
                snapshot
                    .items
                    .iter()
                    .position(|item| item.key == *key)
                    .is_some_and(|position| {
                        fold_through.is_some_and(|end| position <= end) && position < current
                    })
            })
    });
    if !history.is_empty() {
        let previous_history = checkpoint.as_ref().and_then(|value| value.history.as_ref());
        let mut keys = previous_history
            .map(|section| section.covered.clone())
            .unwrap_or_default();
        if folds_previous_active {
            if let Some(section) = old_active {
                // This memory describes work after its triggering user input.
                // Supply it there, rather than presenting it as older memory
                // before the user's request to the incremental summarizer.
                let after = active_memory_after(snapshot, section);
                let anchor = history
                    .iter_mut()
                    .find(|item| Some(item.key) == after)
                    .ok_or(ContextError::Checkpoint(
                        "active memory insertion source is absent",
                    ))?;
                anchor.summary_messages.push(summary_message(section));
                keys.extend(section.covered.iter().copied());
            }
        }
        keys.extend(history.iter().map(|item| item.key));
        order_coverage(&mut keys, snapshot);
        result.summary_work.push(SummaryWork {
            kind: SectionKind::History,
            anchor: None,
            previous_summary: previous_history.map(|section| section.summary.clone()),
            items: history,
            covered: keys,
            folds_previous_active,
        });
    }
    // There is one anchored active section. Never overwrite an older anchored
    // section until it has been incorporated into history successfully.
    if !active.is_empty() && (old_active.is_none() || folds_previous_active) {
        let previous = checkpoint
            .as_ref()
            .and_then(|value| value.active.as_ref())
            .filter(|section| section.anchor == Some(snapshot.current_user));
        let mut keys = previous
            .map(|section| section.covered.clone())
            .unwrap_or_default();
        keys.extend(active.iter().map(|item| item.key));
        order_coverage(&mut keys, snapshot);
        result.summary_work.push(SummaryWork {
            kind: SectionKind::Active,
            anchor: Some(snapshot.current_user),
            previous_summary: previous.map(|section| section.summary.clone()),
            items: active,
            covered: keys,
            folds_previous_active: false,
        });
    }
    Ok(result)
}

fn order_coverage(keys: &mut Vec<ContextItemKey>, snapshot: &ContextSnapshot) {
    let positions: HashMap<_, _> = snapshot
        .items
        .iter()
        .enumerate()
        .map(|(i, item)| (item.key, i))
        .collect();
    keys.sort_by_key(|key| positions.get(key).copied());
    keys.dedup();
}

/// Construct a candidate only after every planned summary completed. Publication
/// and source revalidation still belong to the executor/storage boundary.
pub fn complete_plan(
    checkpoint: &Option<CheckpointPayload>,
    plan: &ContextPlan,
    summaries: &[String],
) -> Result<CheckpointPayload, ContextError> {
    if summaries.len() != plan.summary_work.len() {
        return Err(ContextError::Summary("not every work item completed"));
    }
    let mut next = checkpoint.clone().unwrap_or_default();
    for (work, summary) in plan.summary_work.iter().zip(summaries) {
        if summary.trim().is_empty() {
            return Err(ContextError::Summary("empty generated summary"));
        }
        let section = Some(CheckpointSection {
            summary: summary.clone(),
            covered: work.covered.clone(),
            anchor: work.anchor,
        });
        match work.kind {
            SectionKind::History => {
                next.history = section;
                if work.folds_previous_active {
                    next.active = None;
                }
            }
            SectionKind::Active => next.active = section,
        }
    }
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(kind: ContextItemKind, id: u128) -> ContextItemKey {
        ContextItemKey {
            kind,
            uuid: Uuid::from_u128(id),
        }
    }

    fn item(kind: ContextItemKind, id: u128, tokens: usize) -> ContextItem {
        let role = match kind {
            ContextItemKind::User => "user",
            ContextItemKind::ToolOutput => "tool",
            _ => "assistant",
        };
        ContextItem::from_prompt(
            key(kind, id),
            Some(Uuid::from_u128(1_000)),
            vec![json!({"role": role, "content": format!("item-{id}")})],
            tokens,
            true,
            None,
        )
    }

    fn snapshot(items: Vec<ContextItem>, current: u128) -> ContextSnapshot {
        ContextSnapshot {
            instructions: vec![json!({"role":"system","content":"actual instructions"})],
            instruction_tokens: 10,
            items,
            current_user: key(ContextItemKind::User, current),
            current_response: Uuid::from_u128(1_000),
        }
    }

    fn policy() -> CompactionPolicy {
        CompactionPolicy {
            input_budget: 1_000,
            trigger_tokens: 800,
            keep_recent_tokens: 200,
            summary_input_budget: 800,
        }
    }

    fn tool_pair(call: u128, output: u128, tokens: usize) -> Vec<ContextItem> {
        let mut call_item = item(ContextItemKind::ToolCall, call, tokens);
        call_item.tool_call_id = Some(Uuid::from_u128(call));
        call_item.prompt_messages = vec![json!({"role":"assistant","tool_calls":[{
            "id": call.to_string(), "type":"function", "function":{"name":"lookup","arguments":"{}"}
        }]})];
        call_item.summary_messages = call_item.prompt_messages.clone();
        let mut output_item = item(ContextItemKind::ToolOutput, output, tokens);
        output_item.tool_call_id = Some(Uuid::from_u128(call));
        output_item.prompt_messages[0]["tool_call_id"] = json!(call.to_string());
        output_item.summary_messages = output_item.prompt_messages.clone();
        vec![call_item, output_item]
    }

    #[test]
    fn below_threshold_projects_every_item_without_windowing() {
        let snapshot = snapshot(
            vec![
                item(ContextItemKind::User, 1, 100),
                item(ContextItemKind::Assistant, 2, 100),
                item(ContextItemKind::User, 3, 100),
            ],
            3,
        );
        let plan = plan(&snapshot, &None, &policy(), false).unwrap();
        assert!(plan.summary_work.is_empty());
        assert_eq!(project(&snapshot, &None).unwrap().0.len(), 4);
        assert_eq!(project(&snapshot, &None).unwrap().1, 310);
    }

    #[test]
    fn gpt_oss_default_threshold_only_plans_eligible_history_after_the_boundary() {
        let body: crate::web::responses::handlers::ResponsesCreateRequest =
            serde_json::from_value(json!({
                "model": "gpt-oss-120b",
                "input": "Continue the conversation.",
                "conversation": Uuid::new_v4(),
            }))
            .unwrap();
        assert!(body.context_management.is_none());
        assert!(body.max_output_tokens.is_none());
        let (policy, enabled) = super::super::policy_for(&body).unwrap();
        assert!(enabled);
        assert_eq!(policy.trigger_tokens, 103_219);

        for (total_tokens, should_compact) in [(103_218, false), (103_219, false), (103_220, true)]
        {
            let snapshot = snapshot(
                vec![
                    item(ContextItemKind::User, 1, 40_000),
                    item(ContextItemKind::Assistant, 2, total_tokens - 60_010),
                    item(ContextItemKind::User, 3, 20_000),
                ],
                3,
            );
            let planned = plan_batch(
                &snapshot,
                &None,
                &policy,
                false,
                policy.summary_input_budget,
            )
            .unwrap();
            assert_eq!(
                !planned.summary_work.is_empty(),
                should_compact,
                "input tokens: {total_tokens}",
            );
            if should_compact {
                assert_eq!(planned.summary_work.len(), 1);
                assert_eq!(planned.summary_work[0].kind, SectionKind::History);
                assert_eq!(
                    planned.summary_work[0].covered,
                    vec![key(ContextItemKind::User, 1)],
                );
                assert!(!planned.summary_work[0]
                    .covered
                    .contains(&snapshot.current_user));
            }
            // Planning alone neither drops history nor changes the current
            // request; only a successful checkpoint can replace these items.
            let (messages, projected_tokens) = project(&snapshot, &None).unwrap();
            assert_eq!(projected_tokens, total_tokens);
            assert_eq!(messages.len(), 4);
            assert_eq!(messages[3], snapshot.items[2].prompt_messages[0]);
        }
    }

    #[test]
    fn planning_does_not_discard_unsummarized_source_even_above_budget() {
        let snapshot = snapshot(
            vec![
                item(ContextItemKind::User, 1, 600),
                item(ContextItemKind::Assistant, 2, 600),
                item(ContextItemKind::User, 3, 100),
            ],
            3,
        );
        let plan = plan(&snapshot, &None, &policy(), false).unwrap();
        assert_eq!(project(&snapshot, &None).unwrap().0.len(), 4);
        assert_eq!(plan.summary_work.len(), 1);
        assert_eq!(plan.summary_work[0].items.len(), 1);
        assert_eq!(
            project(&snapshot, &None).unwrap().0.last().unwrap(),
            &snapshot.items.last().unwrap().prompt_messages[0]
        );
        assert!(plan
            .summary_work
            .iter()
            .all(|work| !work.covered.contains(&snapshot.current_user)));
    }

    #[test]
    fn current_input_is_protected_by_identity_even_with_identical_text() {
        let mut prior = item(ContextItemKind::User, 1, 800);
        let current = item(ContextItemKind::User, 2, 100);
        prior.prompt_messages = current.prompt_messages.clone();
        let snapshot = snapshot(
            vec![prior, item(ContextItemKind::Assistant, 3, 100), current],
            2,
        );
        let plan = plan(&snapshot, &None, &policy(), true).unwrap();
        assert_eq!(
            plan.summary_work[0].covered,
            vec![key(ContextItemKind::User, 1)]
        );
        assert!(!plan.summary_work[0]
            .covered
            .contains(&snapshot.current_user));
    }

    #[test]
    fn successful_history_summary_is_before_recent_raw_history() {
        let snapshot = snapshot(
            vec![
                item(ContextItemKind::User, 1, 600),
                item(ContextItemKind::Assistant, 2, 600),
                item(ContextItemKind::User, 3, 100),
            ],
            3,
        );
        let plan = plan(&snapshot, &None, &policy(), true).unwrap();
        let next = Some(complete_plan(&None, &plan, &["prior question memory".into()]).unwrap());
        let messages = project(&snapshot, &next).unwrap().0;
        assert_eq!(messages[0], snapshot.instructions[0]);
        assert!(messages[1]["content"]
            .as_str()
            .unwrap()
            .contains("prior question memory"));
        assert_eq!(messages[2], snapshot.items[1].prompt_messages[0]);
        assert_eq!(messages[3], snapshot.items[2].prompt_messages[0]);
    }

    #[test]
    fn active_turn_compaction_preserves_current_input_and_latest_tool_exchange() {
        let mut items = vec![item(ContextItemKind::User, 1, 100)];
        items.extend(tool_pair(2, 3, 600));
        items.extend(tool_pair(4, 5, 100));
        let snapshot = snapshot(items, 1);
        let plan = plan(&snapshot, &None, &policy(), true).unwrap();
        assert_eq!(plan.summary_work.len(), 1);
        assert_eq!(plan.summary_work[0].kind, SectionKind::Active);
        assert_eq!(
            plan.summary_work[0].covered,
            vec![
                key(ContextItemKind::ToolCall, 2),
                key(ContextItemKind::ToolOutput, 3)
            ]
        );
        let next = Some(complete_plan(&None, &plan, &["completed first lookup".into()]).unwrap());
        let messages = project(&snapshot, &next).unwrap().0;
        assert_eq!(messages[1], snapshot.items[0].prompt_messages[0]);
        assert!(messages[2]["content"]
            .as_str()
            .unwrap()
            .contains("completed first lookup"));
        assert_eq!(
            &messages[3..],
            &[
                snapshot.items[3].prompt_messages[0].clone(),
                snapshot.items[4].prompt_messages[0].clone()
            ]
        );
    }

    #[test]
    fn next_checkpoint_uses_previous_summary_only_for_covered_source() {
        let mut items = vec![item(ContextItemKind::User, 1, 100)];
        items.extend(tool_pair(2, 3, 600));
        items.extend(tool_pair(4, 5, 100));
        let mut snapshot = snapshot(items, 1);
        let first = plan(&snapshot, &None, &policy(), true).unwrap();
        let checkpoint = Some(complete_plan(&None, &first, &["first memory".into()]).unwrap());
        snapshot.items.extend(tool_pair(6, 7, 100));
        let next = plan(&snapshot, &checkpoint, &policy(), true).unwrap();
        assert_eq!(next.summary_work.len(), 1);
        let work = &next.summary_work[0];
        assert_eq!(work.previous_summary.as_deref(), Some("first memory"));
        assert_eq!(
            work.items
                .iter()
                .map(|item| item.key.uuid)
                .collect::<Vec<_>>(),
            vec![Uuid::from_u128(4), Uuid::from_u128(5)]
        );
        assert_eq!(work.covered.len(), 4);
    }

    #[test]
    fn old_active_memory_folds_into_history_before_new_active_memory() {
        let mut items = vec![item(ContextItemKind::User, 1, 100)];
        items.extend(tool_pair(2, 3, 600));
        items.extend(tool_pair(4, 5, 100));
        let mut snapshot = snapshot(items, 1);
        let first = plan(&snapshot, &None, &policy(), true).unwrap();
        let checkpoint = Some(complete_plan(&None, &first, &["old active memory".into()]).unwrap());
        snapshot.items.push(item(ContextItemKind::User, 6, 100));
        snapshot.current_user = key(ContextItemKind::User, 6);
        snapshot.items.extend(tool_pair(7, 8, 600));
        snapshot.items.extend(tool_pair(9, 10, 100));
        let next = plan(&snapshot, &checkpoint, &policy(), true).unwrap();
        assert_eq!(next.summary_work.len(), 2);
        assert_eq!(next.summary_work[0].kind, SectionKind::History);
        assert!(next.summary_work[0].folds_previous_active);
        assert!(next.summary_work[0].previous_summary.is_none());
        assert_eq!(
            next.summary_work[0].items[0].summary_messages[0]["content"],
            "item-1"
        );
        assert!(next.summary_work[0].items[0].summary_messages[1]["content"]
            .as_str()
            .unwrap()
            .contains("old active memory"));
        assert_eq!(next.summary_work[1].anchor, Some(snapshot.current_user));
        let candidate = Some(
            complete_plan(
                &checkpoint,
                &next,
                &["past turn".into(), "new turn progress".into()],
            )
            .unwrap(),
        );
        let projected = project(&snapshot, &candidate).unwrap().0;
        let current_position = projected
            .iter()
            .position(|message| message["content"] == "item-6")
            .unwrap();
        assert!(projected[current_position + 1]["content"]
            .as_str()
            .unwrap()
            .contains("new turn progress"));
        assert!(projected[1]["content"]
            .as_str()
            .unwrap()
            .contains("past turn"));
    }

    #[test]
    fn reload_with_history_and_active_sections_preserves_complete_coverage_union() {
        let mut items = vec![
            item(ContextItemKind::User, 1, 100),
            item(ContextItemKind::Assistant, 2, 100),
            item(ContextItemKind::User, 3, 100),
        ];
        items.extend(tool_pair(4, 5, 600));
        items.extend(tool_pair(6, 7, 100));
        items.push(item(ContextItemKind::User, 8, 100));
        items.extend(tool_pair(9, 10, 600));
        items.extend(tool_pair(11, 12, 100));
        let snapshot = snapshot(items, 8);
        let checkpoint = Some(CheckpointPayload {
            version: CHECKPOINT_VERSION,
            history: Some(CheckpointSection {
                summary: "older memory".into(),
                covered: vec![
                    key(ContextItemKind::User, 1),
                    key(ContextItemKind::Assistant, 2),
                ],
                anchor: None,
            }),
            active: Some(CheckpointSection {
                summary: "prior active progress".into(),
                covered: vec![
                    key(ContextItemKind::ToolCall, 4),
                    key(ContextItemKind::ToolOutput, 5),
                ],
                anchor: Some(key(ContextItemKind::User, 3)),
            }),
        });
        let plan = plan(&snapshot, &checkpoint, &policy(), true).unwrap();
        assert_eq!(plan.summary_work.len(), 2);
        assert_eq!(
            plan.summary_work[0].previous_summary.as_deref(),
            Some("older memory")
        );
        assert_eq!(
            plan.summary_work[0].items[0].summary_messages[0]["content"],
            "item-3"
        );
        assert!(plan.summary_work[0].items[0].summary_messages[1]["content"]
            .as_str()
            .unwrap()
            .contains("prior active progress"));
        assert_eq!(
            plan.summary_work[0]
                .items
                .iter()
                .map(|item| item.key.uuid)
                .collect::<Vec<_>>(),
            vec![Uuid::from_u128(3), Uuid::from_u128(6), Uuid::from_u128(7)]
        );
        assert_eq!(
            plan.summary_work[0]
                .covered
                .iter()
                .map(|key| key.uuid)
                .collect::<Vec<_>>(),
            (1..=7).map(Uuid::from_u128).collect::<Vec<_>>()
        );
        let candidate = Some(
            complete_plan(
                &checkpoint,
                &plan,
                &["combined history".into(), "current progress".into()],
            )
            .unwrap(),
        );
        let projected = project(&snapshot, &candidate).unwrap().0;
        assert_eq!(projected.len(), 6);
        assert!(projected[1]["content"]
            .as_str()
            .unwrap()
            .contains("combined history"));
        assert_eq!(projected[2]["content"], "item-8");
        assert!(projected[3]["content"]
            .as_str()
            .unwrap()
            .contains("current progress"));
        assert_eq!(projected[4], snapshot.items[10].prompt_messages[0]);
        assert_eq!(projected[5], snapshot.items[11].prompt_messages[0]);
    }

    #[test]
    fn parallel_tool_relationships_are_atomic_without_reordering_source() {
        let first = tool_pair(2, 4, 300);
        let second = tool_pair(3, 5, 300);
        let items = vec![
            item(ContextItemKind::User, 1, 100),
            first[0].clone(),
            second[0].clone(),
            first[1].clone(),
            second[1].clone(),
            item(ContextItemKind::Assistant, 6, 200),
        ];
        let snapshot = snapshot(items, 1);
        let plan = plan(&snapshot, &None, &policy(), true).unwrap();
        assert_eq!(
            plan.summary_work[0]
                .items
                .iter()
                .map(|item| item.key.uuid)
                .collect::<Vec<_>>(),
            vec![
                Uuid::from_u128(2),
                Uuid::from_u128(3),
                Uuid::from_u128(4),
                Uuid::from_u128(5)
            ]
        );
        assert_eq!(
            project(&snapshot, &None).unwrap().0[2],
            snapshot.items[1].prompt_messages[0]
        );
        assert_eq!(
            project(&snapshot, &None).unwrap().0[3],
            snapshot.items[2].prompt_messages[0]
        );
    }

    #[test]
    fn incomplete_or_unsettled_exchange_stops_prefix_compaction() {
        let mut items = vec![item(ContextItemKind::User, 1, 100)];
        let mut pair = tool_pair(2, 3, 600);
        pair[1].settled = false;
        items.extend(pair);
        items.extend(tool_pair(4, 5, 600));
        items.extend(tool_pair(6, 7, 100));
        let snapshot = snapshot(items, 1);
        let plan = plan(&snapshot, &None, &policy(), true).unwrap();
        assert!(plan.summary_work.is_empty());
        assert!(plan
            .summary_work
            .iter()
            .all(|work| !work.covered.contains(&key(ContextItemKind::ToolCall, 2))));
        assert!(plan
            .summary_work
            .iter()
            .all(|work| !work.covered.contains(&key(ContextItemKind::ToolOutput, 3))));
        assert_eq!(project(&snapshot, &None).unwrap().0.len(), 8);
    }

    #[test]
    fn pending_call_is_protected_even_when_not_last() {
        let mut items = vec![item(ContextItemKind::User, 1, 100)];
        items.push(tool_pair(2, 3, 600).remove(0));
        items.extend(tool_pair(4, 5, 100));
        let snapshot = snapshot(items, 1);
        let plan = plan(&snapshot, &None, &policy(), true).unwrap();
        assert!(plan.summary_work.is_empty());
        assert!(plan
            .summary_work
            .iter()
            .all(|work| !work.covered.contains(&key(ContextItemKind::ToolCall, 2))));
    }

    #[test]
    fn unrelated_mutable_response_is_never_summarized() {
        let mut prior = item(ContextItemKind::Assistant, 2, 1_000);
        prior.response_uuid = Some(Uuid::from_u128(2_000));
        prior.settled = false;
        let snapshot = snapshot(
            vec![
                item(ContextItemKind::User, 1, 100),
                prior,
                item(ContextItemKind::User, 3, 100),
            ],
            3,
        );
        let plan = plan(&snapshot, &None, &policy(), true).unwrap();
        assert_eq!(plan.summary_work.len(), 1);
        assert_eq!(
            plan.summary_work[0].covered,
            vec![key(ContextItemKind::User, 1)]
        );
        assert!(plan
            .summary_work
            .iter()
            .all(|work| !work.covered.contains(&key(ContextItemKind::Assistant, 2))));
    }

    #[test]
    fn failed_or_empty_summary_does_not_create_partial_checkpoint() {
        let snapshot = snapshot(
            vec![
                item(ContextItemKind::User, 1, 600),
                item(ContextItemKind::Assistant, 2, 600),
                item(ContextItemKind::User, 3, 100),
            ],
            3,
        );
        let plan = plan(&snapshot, &None, &policy(), true).unwrap();
        assert!(complete_plan(&None, &plan, &[]).is_err());
        assert!(complete_plan(&None, &plan, &[" \n".into()]).is_err());
        assert_eq!(project(&snapshot, &None).unwrap().0.len(), 4);
    }

    #[test]
    fn rejects_deleted_duplicate_or_required_coverage() {
        let snapshot = snapshot(
            vec![
                item(ContextItemKind::User, 1, 600),
                item(ContextItemKind::Assistant, 2, 600),
                item(ContextItemKind::User, 3, 100),
            ],
            3,
        );
        let section = |covered| {
            Some(CheckpointPayload {
                history: Some(CheckpointSection {
                    summary: "memory".into(),
                    covered,
                    anchor: None,
                }),
                ..CheckpointPayload::default()
            })
        };
        assert!(project(&snapshot, &section(vec![key(ContextItemKind::User, 99)])).is_err());
        assert!(project(&snapshot, &section(vec![snapshot.current_user])).is_err());
        assert!(project(&snapshot, &section(vec![key(ContextItemKind::User, 1); 2])).is_err());
    }

    #[test]
    fn rejects_partial_tool_coverage() {
        let mut items = vec![item(ContextItemKind::User, 1, 100)];
        items.extend(tool_pair(2, 3, 600));
        items.push(item(ContextItemKind::User, 4, 100));
        let snapshot = snapshot(items, 4);
        let checkpoint = Some(CheckpointPayload {
            history: Some(CheckpointSection {
                summary: "memory".into(),
                covered: vec![key(ContextItemKind::ToolCall, 2)],
                anchor: None,
            }),
            ..CheckpointPayload::default()
        });
        assert!(project(&snapshot, &checkpoint).is_err());
    }

    #[test]
    fn oversized_required_input_is_never_shortened() {
        let snapshot = snapshot(vec![item(ContextItemKind::User, 1, 40_000)], 1);
        let plan = plan(&snapshot, &None, &policy(), true).unwrap();
        assert!(plan.summary_work.is_empty());
        assert_eq!(project(&snapshot, &None).unwrap().1, 40_010);
        assert_eq!(
            project(&snapshot, &None).unwrap().0[1],
            snapshot.items[0].prompt_messages[0]
        );
    }

    #[test]
    fn hard_budget_retry_can_compact_the_latest_completed_oversized_exchange() {
        let mut items = vec![item(ContextItemKind::User, 1, 100)];
        items.extend(tool_pair(2, 3, 40_000));
        let snapshot = snapshot(items, 1);
        let normal = plan(&snapshot, &None, &policy(), false).unwrap();
        assert!(normal.summary_work.is_empty());
        let mut recovery_policy = policy();
        recovery_policy.keep_recent_tokens = 0;
        let forced = plan(&snapshot, &None, &recovery_policy, true).unwrap();
        assert_eq!(forced.summary_work.len(), 1);
        assert_eq!(
            forced.summary_work[0].covered,
            vec![
                key(ContextItemKind::ToolCall, 2),
                key(ContextItemKind::ToolOutput, 3)
            ]
        );
        assert!(!forced.summary_work[0]
            .covered
            .contains(&snapshot.current_user));
        let candidate = Some(
            complete_plan(
                &None,
                &forced,
                &["complete large lookup, retaining its findings".into()],
            )
            .unwrap(),
        );
        let (messages, tokens) = project(&snapshot, &candidate).unwrap();
        assert_eq!(messages[1], snapshot.items[0].prompt_messages[0]);
        assert!(tokens < recovery_policy.input_budget);
    }

    #[test]
    fn forced_zero_suffix_does_not_cover_required_or_pending_work() {
        let mut items = vec![item(ContextItemKind::User, 1, 40_000)];
        let mut pending_pair = tool_pair(2, 3, 20_000);
        pending_pair[1].settled = false;
        items.extend(pending_pair);
        let snapshot = snapshot(items, 1);
        let mut forced_policy = policy();
        forced_policy.keep_recent_tokens = 0;
        let plan = plan(&snapshot, &None, &forced_policy, true).unwrap();
        assert!(plan.summary_work.is_empty());
        let (messages, tokens) = project(&snapshot, &None).unwrap();
        assert_eq!(tokens, 80_010);
        assert_eq!(messages[1], snapshot.items[0].prompt_messages[0]);
        assert_eq!(messages[2], snapshot.items[1].prompt_messages[0]);
        assert_eq!(messages[3], snapshot.items[2].prompt_messages[0]);
    }

    #[test]
    fn huge_unicode_tool_source_is_chunked_completely_without_private_reasoning() {
        let mut pair = tool_pair(2, 3, 20_000);
        let text = format!(
            "{} end citation https://example.test/reference",
            "🦀数据".repeat(3_000)
        );
        pair[1].summary_messages =
            vec![json!({"role":"tool","content":text,"reasoning_content":"private secret"})];
        let reasoning = ContextItem::from_prompt(
            key(ContextItemKind::Reasoning, 9),
            Some(Uuid::from_u128(1_000)),
            vec![json!({"role":"assistant","content":"private thinking"})],
            100,
            true,
            None,
        );
        pair.insert(0, reasoning);
        let work = SummaryWork {
            kind: SectionKind::Active,
            anchor: Some(key(ContextItemKind::User, 1)),
            previous_summary: None,
            covered: pair.iter().map(|item| item.key).collect(),
            items: pair,
            folds_previous_active: false,
        };
        let messages: Vec<_> = work
            .items
            .iter()
            .flat_map(|item| item.summary_messages.iter().cloned())
            .collect();
        let transcript = super::super::summary::serialize_transcript(&messages).unwrap();
        let chunks = super::super::summary::split_transcript(&transcript, 2_000).unwrap();
        assert!(chunks.len() > 2);
        assert!(chunks.iter().all(|chunk| chunk.len() <= 2_000));
        let full = chunks.concat();
        assert!(full.contains(&text));
        assert!(!full.contains("private secret"));
        assert!(!full.contains("private thinking"));
        assert!(full.contains("https://example.test/reference"));
        assert!(super::super::summary::split_transcript(&transcript, 0).is_err());
    }

    #[test]
    fn reasoning_keys_follow_output_coverage_but_never_enter_summary() {
        let mut reasoning = item(ContextItemKind::Reasoning, 2, 100);
        reasoning.prompt_messages.clear();
        let snapshot = snapshot(
            vec![
                item(ContextItemKind::User, 1, 100),
                reasoning,
                item(ContextItemKind::Assistant, 3, 600),
                item(ContextItemKind::User, 4, 200),
            ],
            4,
        );
        let plan = plan(&snapshot, &None, &policy(), true).unwrap();
        let work = &plan.summary_work[0];
        assert!(work.covered.contains(&key(ContextItemKind::Reasoning, 2)));
        assert!(work.covered.contains(&key(ContextItemKind::Assistant, 3)));
        let messages: Vec<_> = work
            .items
            .iter()
            .flat_map(|item| item.summary_messages.iter().cloned())
            .collect();
        let transcript = super::super::summary::serialize_transcript(&messages).unwrap();
        assert!(!transcript.contains("item-2"));
    }

    #[test]
    fn bounded_history_batches_make_progress_without_splitting_tool_pairs() {
        let mut items = vec![item(ContextItemKind::User, 1, 100)];
        items.extend(tool_pair(2, 3, 300));
        items.push(item(ContextItemKind::Assistant, 4, 100));
        items.push(item(ContextItemKind::User, 5, 100));
        let snapshot = snapshot(items, 5);
        let mut policy = policy();
        policy.keep_recent_tokens = 0;
        let first = plan_batch(&snapshot, &None, &policy, true, 500).unwrap();
        assert_eq!(
            first.summary_work[0].covered,
            vec![key(ContextItemKind::User, 1)]
        );
        let checkpoint = Some(complete_plan(&None, &first, &["first memory".into()]).unwrap());
        let second = plan_batch(&snapshot, &checkpoint, &policy, true, 500).unwrap();
        assert_eq!(
            second.summary_work[0]
                .items
                .iter()
                .map(|item| item.key)
                .collect::<Vec<_>>(),
            vec![
                key(ContextItemKind::ToolCall, 2),
                key(ContextItemKind::ToolOutput, 3)
            ]
        );
        let checkpoint =
            Some(complete_plan(&checkpoint, &second, &["pair memory".into()]).unwrap());
        let third = plan_batch(&snapshot, &checkpoint, &policy, true, 500).unwrap();
        assert_eq!(
            third.summary_work[0].items[0].key,
            key(ContextItemKind::Assistant, 4)
        );
        assert!(!third.summary_work[0]
            .covered
            .contains(&snapshot.current_user));
    }

    #[test]
    fn bounded_active_batches_advance_one_complete_tool_prefix_at_a_time() {
        let mut items = vec![item(ContextItemKind::User, 1, 100)];
        items.extend(tool_pair(2, 3, 300));
        items.extend(tool_pair(4, 5, 300));
        items.extend(tool_pair(6, 7, 300));
        let snapshot = snapshot(items, 1);
        let mut policy = policy();
        policy.keep_recent_tokens = 0;
        let mut checkpoint = None;
        for (call, output) in [(2, 3), (4, 5), (6, 7)] {
            // An atomic pair exceeds this budget, but it remains indivisible.
            let plan = plan_batch(&snapshot, &checkpoint, &policy, true, 500).unwrap();
            assert_eq!(plan.summary_work.len(), 1);
            let work = &plan.summary_work[0];
            assert_eq!(work.kind, SectionKind::Active);
            assert_eq!(
                work.items.iter().map(|item| item.key).collect::<Vec<_>>(),
                vec![
                    key(ContextItemKind::ToolCall, call),
                    key(ContextItemKind::ToolOutput, output)
                ],
            );
            checkpoint =
                Some(complete_plan(&checkpoint, &plan, &["completed lookup".into()]).unwrap());
        }
        assert!(plan_batch(&snapshot, &checkpoint, &policy, true, 500)
            .unwrap()
            .summary_work
            .is_empty());
        let projected = project(&snapshot, &checkpoint).unwrap().0;
        assert_eq!(projected[1], snapshot.items[0].prompt_messages[0]);
        assert_eq!(checkpoint.unwrap().active.unwrap().covered.len(), 6);
    }

    #[test]
    fn ciphertext_byte_budget_bounds_low_token_history_and_active_batches() {
        let mut first = item(ContextItemKind::User, 1, 100);
        let mut second = item(ContextItemKind::User, 2, 100);
        first.source_bytes = 6 * 1024 * 1024;
        second.source_bytes = 6 * 1024 * 1024;
        let history = snapshot(vec![first, second, item(ContextItemKind::User, 3, 100)], 3);
        let mut policy = policy();
        policy.keep_recent_tokens = 0;
        let first = plan_batch(&history, &None, &policy, true, 64_000).unwrap();
        assert_eq!(first.summary_work[0].items.len(), 1);
        assert_eq!(
            first.summary_work[0].items[0].key,
            key(ContextItemKind::User, 1)
        );
        let checkpoint =
            Some(complete_plan(&None, &first, &["first image history".into()]).unwrap());
        let second = plan_batch(&history, &checkpoint, &policy, true, 64_000).unwrap();
        assert_eq!(second.summary_work[0].items.len(), 1);
        assert_eq!(
            second.summary_work[0].items[0].key,
            key(ContextItemKind::User, 2)
        );

        let mut items = vec![item(ContextItemKind::User, 1, 100)];
        for (call, output) in [(2, 3), (4, 5)] {
            let mut pair = tool_pair(call, output, 100);
            pair[0].source_bytes = 0;
            pair[1].source_bytes = 6 * 1024 * 1024;
            items.extend(pair);
        }
        let active = snapshot(items, 1);
        let plan = plan_batch(&active, &None, &policy, true, 64_000).unwrap();
        assert_eq!(plan.summary_work[0].items.len(), 2);
        assert_eq!(
            plan.summary_work[0].items[1].key,
            key(ContextItemKind::ToolOutput, 3)
        );
    }

    #[test]
    fn plan_hydration_preserves_folded_active_memory_after_its_source() {
        let mut items = vec![item(ContextItemKind::User, 1, 100)];
        items.extend(tool_pair(2, 3, 300));
        items.push(item(ContextItemKind::User, 4, 100));
        let hydrated = snapshot(items, 4);
        let checkpoint = Some(CheckpointPayload {
            active: Some(CheckpointSection {
                summary: "previous active lookup".into(),
                covered: vec![
                    key(ContextItemKind::ToolCall, 2),
                    key(ContextItemKind::ToolOutput, 3),
                ],
                anchor: Some(key(ContextItemKind::User, 1)),
            }),
            ..CheckpointPayload::default()
        });
        let mut metadata = hydrated.clone();
        for item in &mut metadata.items {
            item.prompt_messages.clear();
            item.summary_messages.clear();
            item.payload_loaded = false;
        }
        let mut policy = policy();
        policy.keep_recent_tokens = 0;
        let plan = plan_batch(&metadata, &checkpoint, &policy, true, 50).unwrap();
        assert!(plan.summary_work[0].folds_previous_active);
        assert_eq!(plan.summary_work[0].items[0].summary_messages.len(), 1);
        let work = hydrate_plan(&plan, &hydrated).unwrap();
        let messages = &work.summary_work[0].items[0].summary_messages;
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["content"], "item-1");
        assert!(messages[1]["content"]
            .as_str()
            .unwrap()
            .contains("previous active lookup"));
        // Hydrating an already hydrated plan also preserves one insertion.
        let repeated = hydrate_plan(&work, &hydrated).unwrap();
        assert_eq!(
            repeated.summary_work[0].items[0].summary_messages,
            *messages
        );
    }

    #[test]
    fn helper_hydration_never_requires_or_exposes_private_reasoning_payload() {
        let mut reasoning = item(ContextItemKind::Reasoning, 2, 0);
        reasoning.prompt_messages.clear();
        reasoning.payload_loaded = false;
        let hydrated = snapshot(
            vec![
                item(ContextItemKind::User, 1, 100),
                reasoning,
                item(ContextItemKind::Assistant, 3, 600),
                item(ContextItemKind::User, 4, 100),
            ],
            4,
        );
        let mut metadata = hydrated.clone();
        for item in &mut metadata.items {
            item.prompt_messages.clear();
            item.summary_messages.clear();
            item.payload_loaded = false;
        }
        let mut policy = policy();
        policy.keep_recent_tokens = 0;
        let plan = plan_batch(&metadata, &None, &policy, true, 1_000).unwrap();
        let work = hydrate_plan(&plan, &hydrated).unwrap();
        assert!(work.summary_work[0]
            .items
            .iter()
            .find(|item| item.key.kind == ContextItemKind::Reasoning)
            .unwrap()
            .summary_messages
            .is_empty());
        assert!(project(&hydrated, &None).is_err());
    }

    #[test]
    fn batch_boundary_never_covers_active_anchor_without_its_complete_memory() {
        let mut items = vec![
            item(ContextItemKind::User, 1, 100),
            item(ContextItemKind::User, 2, 100),
        ];
        items.extend(tool_pair(3, 4, 300));
        items.push(item(ContextItemKind::Assistant, 5, 100));
        items.push(item(ContextItemKind::User, 6, 100));
        let snapshot = snapshot(items, 6);
        let checkpoint = Some(CheckpointPayload {
            active: Some(CheckpointSection {
                summary: "lookup after question two".into(),
                covered: vec![
                    key(ContextItemKind::ToolCall, 3),
                    key(ContextItemKind::ToolOutput, 4),
                ],
                anchor: Some(key(ContextItemKind::User, 2)),
            }),
            ..CheckpointPayload::default()
        });
        let mut policy = policy();
        policy.keep_recent_tokens = 0;
        let first = plan_batch(&snapshot, &checkpoint, &policy, true, 150).unwrap();
        assert_eq!(
            first.summary_work[0].covered,
            vec![key(ContextItemKind::User, 1)]
        );
        assert!(!first.summary_work[0].folds_previous_active);
        let checkpoint =
            Some(complete_plan(&checkpoint, &first, &["older memory".into()]).unwrap());
        let second = plan_batch(&snapshot, &checkpoint, &policy, true, 50).unwrap();
        let work = &second.summary_work[0];
        assert!(work.folds_previous_active);
        assert_eq!(work.previous_summary.as_deref(), Some("older memory"));
        assert_eq!(work.items[0].key, key(ContextItemKind::User, 2));
        assert_eq!(work.items[0].summary_messages[0]["content"], "item-2");
        assert!(work.items[0].summary_messages[1]["content"]
            .as_str()
            .unwrap()
            .contains("lookup after question two"));
        assert!(work.covered.contains(&key(ContextItemKind::ToolCall, 3)));
        assert!(work.covered.contains(&key(ContextItemKind::ToolOutput, 4)));
        assert!(!work.covered.contains(&key(ContextItemKind::Assistant, 5)));
    }

    #[test]
    fn blocked_active_fold_keeps_anchor_and_memory_out_of_a_partial_batch() {
        let mut items = vec![item(ContextItemKind::User, 1, 100)];
        let mut intervening = item(ContextItemKind::Assistant, 2, 100);
        intervening.settled = false;
        items.push(intervening);
        items.extend(tool_pair(3, 4, 300));
        items.push(item(ContextItemKind::User, 5, 100));
        let snapshot = snapshot(items, 5);
        let checkpoint = Some(CheckpointPayload {
            active: Some(CheckpointSection {
                summary: "completed lookup".into(),
                covered: vec![
                    key(ContextItemKind::ToolCall, 3),
                    key(ContextItemKind::ToolOutput, 4),
                ],
                anchor: Some(key(ContextItemKind::User, 1)),
            }),
            ..CheckpointPayload::default()
        });
        let mut policy = policy();
        policy.keep_recent_tokens = 0;
        assert!(plan_batch(&snapshot, &checkpoint, &policy, true, 100)
            .unwrap()
            .summary_work
            .is_empty());
    }

    #[test]
    fn current_image_input_survives_later_active_compaction_and_reload_in_order() {
        let mut items = vec![item(ContextItemKind::User, 1, 100)];
        let mut images = tool_pair(2, 3, 300);
        for item in &mut images {
            item.protected_input = true;
        }
        items.extend(images);
        items.extend(tool_pair(4, 5, 600));
        let mut snapshot = snapshot(items, 1);
        let mut policy = policy();
        policy.keep_recent_tokens = 0;
        let first = plan_batch(&snapshot, &None, &policy, true, 1_000).unwrap();
        assert_eq!(
            first.summary_work[0].covered,
            vec![
                key(ContextItemKind::ToolCall, 4),
                key(ContextItemKind::ToolOutput, 5)
            ]
        );
        let checkpoint = Some(complete_plan(&None, &first, &["completed lookup".into()]).unwrap());
        let messages = project(&snapshot, &checkpoint).unwrap().0;
        assert_eq!(messages[1], snapshot.items[0].prompt_messages[0]);
        assert_eq!(messages[2], snapshot.items[1].prompt_messages[0]);
        assert_eq!(messages[3], snapshot.items[2].prompt_messages[0]);
        assert!(messages[4]["content"]
            .as_str()
            .unwrap()
            .contains("completed lookup"));
        snapshot.items.push(item(ContextItemKind::User, 6, 100));
        snapshot.current_user = key(ContextItemKind::User, 6);
        snapshot.current_response = Uuid::from_u128(2_000);
        let reloaded = project(&snapshot, &checkpoint).unwrap().0;
        assert_eq!(&reloaded[1..5], &messages[1..5]);
        let next = plan_batch(&snapshot, &checkpoint, &policy, true, 100).unwrap();
        let work = &next.summary_work[0];
        assert!(work.folds_previous_active);
        assert!(work.covered.contains(&key(ContextItemKind::ToolCall, 2)));
        assert!(work.covered.contains(&key(ContextItemKind::ToolOutput, 3)));
        assert_eq!(work.items[0].summary_messages.len(), 1);
        assert_eq!(work.items[1].summary_messages.len(), 1);
        assert_eq!(work.items[2].summary_messages[0]["role"], "tool");
        assert!(work.items[2].summary_messages[1]["content"]
            .as_str()
            .unwrap()
            .contains("completed lookup"));
    }

    #[test]
    fn settled_historical_unmatched_tool_is_coverable_without_invalid_replay() {
        let mut orphan = tool_pair(2, 3, 600).remove(0);
        orphan.response_uuid = Some(Uuid::from_u128(500));
        orphan.historical_unmatched = true;
        orphan.prompt_messages.clear();
        orphan.summary_messages.clear();
        orphan.estimated_tokens = 0;
        let snapshot = snapshot(
            vec![
                item(ContextItemKind::User, 1, 100),
                orphan,
                item(ContextItemKind::User, 4, 100),
            ],
            4,
        );
        let mut policy = policy();
        policy.keep_recent_tokens = 0;
        let plan = plan_batch(&snapshot, &None, &policy, true, 1_000).unwrap();
        assert_eq!(
            plan.summary_work[0].covered,
            vec![
                key(ContextItemKind::User, 1),
                key(ContextItemKind::ToolCall, 2)
            ]
        );
        let checkpoint = Some(complete_plan(&None, &plan, &["historical request".into()]).unwrap());
        let messages = project(&snapshot, &checkpoint).unwrap().0;
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2]["content"], "item-4");
    }
}
