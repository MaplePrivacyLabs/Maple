//! Read-only context planning and checkpoint projection.
//!
//! For one request, every canonical source item is replayed verbatim, covered by
//! a published summary, or archived outside the active window. The active window
//! is the scope the sliding-window strategy would show this model: compaction
//! never widens it. Summary input comes only from that window, so an archived
//! item stays available in canonical storage but never returns to model context.
//! This module performs no persistence, inference, billing, or tool execution.

use super::{policy::CompactionPolicy, summary};
use crate::tokens::count_tokens;
use crate::web::responses::constants::{ACTIVE_WINDOW_ROWS, TRUNCATION_MARKER};
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
    /// A summary frees budget, so a recomputed window would otherwise walk back
    /// into source it already excluded. Uncovered items before this covered key
    /// stay archived; deleting it invalidates the checkpoint, so it cannot dangle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_start: Option<ContextItemKey>,
}

impl Default for CheckpointPayload {
    fn default() -> Self {
        Self {
            version: CHECKPOINT_VERSION,
            history: None,
            active: None,
            active_start: None,
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
    /// The previous active section's anchor left the window: its coverage is
    /// retained, but its memory is not carried forward.
    pub drops_previous_active: bool,
    /// Set when this work skips source outside the window. The first item after
    /// that gap becomes the checkpoint's durable active start.
    pub active_start: Option<ContextItemKey>,
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

/// The model-visible scope of one request. Selection reads only metadata and the
/// estimates already on the snapshot; it performs no inference or decryption.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActiveContext {
    archived: HashSet<ContextItemKey>,
    head: Option<ContextItemKey>,
    truncated: bool,
}

impl ActiveContext {
    /// Apply the sliding-window boundary. `input_budget` is `None` only when the
    /// caller explicitly disabled compaction, which reports an overflow rather
    /// than windowing; a published `active_start` still applies.
    pub fn select(
        snapshot: &ContextSnapshot,
        checkpoint: &Option<CheckpointPayload>,
        input_budget: Option<usize>,
    ) -> Result<Self, ContextError> {
        Self::fit(snapshot, checkpoint, input_budget, None)
    }

    /// Fit again with hydrated estimates, like the sliding window's second pass.
    /// This can only narrow: a lower estimate never admits an unloaded item.
    pub fn refit(
        &self,
        snapshot: &ContextSnapshot,
        checkpoint: &Option<CheckpointPayload>,
        input_budget: Option<usize>,
    ) -> Result<Self, ContextError> {
        Self::fit(snapshot, checkpoint, input_budget, Some(self))
    }

    pub fn is_archived(&self, key: &ContextItemKey) -> bool {
        self.archived.contains(key)
    }

    pub fn archived_items(&self) -> usize {
        self.archived.len()
    }

    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    fn fit(
        snapshot: &ContextSnapshot,
        checkpoint: &Option<CheckpointPayload>,
        input_budget: Option<usize>,
        previous: Option<&Self>,
    ) -> Result<Self, ContextError> {
        let covered = validated_coverage(snapshot, checkpoint)?;
        let items = &snapshot.items;
        let current = item_position(snapshot, &snapshot.current_user)?;
        let archive = |range: std::ops::Range<usize>, archived: &mut HashSet<_>| {
            archived.extend(
                items[range]
                    .iter()
                    .map(|item| item.key)
                    .filter(|key| !covered.contains(key)),
            );
        };
        let mut result = previous.cloned().unwrap_or_default();
        if let Some(start) = checkpoint.as_ref().and_then(|value| value.active_start) {
            archive(0..item_position(snapshot, &start)?, &mut result.archived);
        }
        let Some(budget) = input_budget else {
            return Ok(result);
        };
        let groups = atomic_groups(snapshot);
        let history = checkpoint.as_ref().and_then(|value| value.history.as_ref());
        let old_active = previous_active(snapshot, checkpoint);
        let units = window_units(snapshot, checkpoint, &groups, current);
        let required_from = units.last().map_or(0, |unit| unit.end + 1);
        // The sliding window reads only these rows, so its first user message and
        // its budget are measured within them. Current-turn rows are not history.
        // Cut between units: half an exchange can neither be sent nor covered.
        let first_row = current.saturating_sub(ACTIVE_WINDOW_ROWS);
        let window_start =
            units
                .iter()
                .find(|unit| unit.end >= first_row)
                .map_or(required_from, |unit| {
                    if unit.start < first_row {
                        unit.end + 1
                    } else {
                        unit.start
                    }
                });
        archive(0..window_start, &mut result.archived);

        let tokens_of = |unit: &WindowUnit, archived: &HashSet<ContextItemKey>| {
            let raw = items[unit.start..=unit.end]
                .iter()
                .filter(|item| !covered.contains(&item.key) && !archived.contains(&item.key))
                .map(|item| item.estimated_tokens)
                .fold(0usize, usize::saturating_add);
            match (unit.carries_active_memory, old_active) {
                (true, Some(section)) if raw > 0 => raw.saturating_add(section_tokens(section)),
                _ => raw,
            }
        };
        let mut required = items[required_from..]
            .iter()
            .filter(|item| !covered.contains(&item.key))
            .map(|item| item.estimated_tokens)
            .fold(snapshot.instruction_tokens, usize::saturating_add);
        for section in history.into_iter().chain(
            checkpoint
                .as_ref()
                .and_then(|value| value.active.as_ref())
                .filter(|_| old_active.is_none()),
        ) {
            required = required.saturating_add(section_tokens(section));
        }
        let visible: Vec<_> = units
            .iter()
            .map(|unit| (unit, tokens_of(unit, &result.archived)))
            .filter(|(_, tokens)| *tokens > 0)
            .collect();
        let marker = count_tokens(TRUNCATION_MARKER);
        let total = visible
            .iter()
            .map(|(_, tokens)| *tokens)
            .fold(required, usize::saturating_add);
        if total.saturating_add(if result.truncated { marker } else { 0 }) <= budget {
            return Ok(result);
        }

        let mut available = budget.saturating_sub(required.saturating_add(marker));
        // A summary already stands in for the conversation's opening.
        let head = visible
            .iter()
            .find(|(unit, _)| match previous {
                Some(previous) => previous.head == Some(items[unit.start].key),
                None => unit.starts_with_user,
            })
            .filter(|(_, tokens)| history.is_none() && *tokens <= available);
        if let Some((_, tokens)) = head {
            available -= *tokens;
        }
        let head_start = head.map(|(unit, _)| unit.start);
        let mut tail = Vec::new();
        let mut tail_tokens = 0usize;
        for (unit, tokens) in visible.iter().rev() {
            if Some(unit.start) == head_start {
                continue;
            }
            if tail_tokens.saturating_add(*tokens) > available {
                break;
            }
            tail_tokens = tail_tokens.saturating_add(*tokens);
            tail.push(*unit);
        }
        tail.reverse();
        // The window opens on a user message, so it never begins mid-exchange.
        let opening = tail
            .iter()
            .find(|unit| unit.starts_with_user)
            .map_or(required_from, |unit| unit.start);
        for unit in units.iter().take_while(|unit| unit.start < opening) {
            if Some(unit.start) != head_start {
                archive(unit.start..unit.end + 1, &mut result.archived);
            }
        }
        result.head = head.map(|(unit, _)| items[unit.start].key);
        result.truncated = true;
        Ok(result)
    }
}

fn item_position(snapshot: &ContextSnapshot, key: &ContextItemKey) -> Result<usize, ContextError> {
    snapshot
        .items
        .iter()
        .position(|item| item.key == *key)
        .ok_or(ContextError::Snapshot("source item is absent"))
}

/// An active section anchored to an earlier Response, which must become history.
fn previous_active<'a>(
    snapshot: &ContextSnapshot,
    checkpoint: &'a Option<CheckpointPayload>,
) -> Option<&'a CheckpointSection> {
    checkpoint
        .as_ref()
        .and_then(|value| value.active.as_ref())
        .filter(|section| section.anchor != Some(snapshot.current_user))
}

/// The older active section and its anchor are one indivisible unit, as groups.
fn previous_active_span(
    snapshot: &ContextSnapshot,
    section: &CheckpointSection,
    groups: &[AtomicGroup],
    current: usize,
) -> Option<(usize, usize)> {
    let end = section
        .covered
        .iter()
        .copied()
        .chain(section.anchor)
        .filter_map(|key| snapshot.items.iter().position(|item| item.key == key))
        .max()?;
    let start = section
        .anchor
        .and_then(|anchor| snapshot.items.iter().position(|item| item.key == anchor))?;
    if end >= current {
        return None;
    }
    let first = groups
        .iter()
        .position(|group| group.start <= start && start <= group.end)?;
    let last = groups
        .iter()
        .position(|group| group.start <= end && end <= group.end)?;
    Some((first, last))
}

/// One indivisible run of history before the current user. A window either keeps
/// all of it or none of it, so no tool or reasoning relationship can be bisected.
#[derive(Debug, Clone, Copy)]
struct WindowUnit {
    start: usize,
    end: usize,
    first_group: usize,
    last_group: usize,
    starts_with_user: bool,
    carries_active_memory: bool,
}

fn window_units(
    snapshot: &ContextSnapshot,
    checkpoint: &Option<CheckpointPayload>,
    groups: &[AtomicGroup],
    current: usize,
) -> Vec<WindowUnit> {
    let fold_span = previous_active(snapshot, checkpoint)
        .and_then(|section| previous_active_span(snapshot, section, groups, current));
    let mut units = Vec::new();
    let mut index = 0;
    while let Some(group) = groups.get(index) {
        if group.end >= current {
            break;
        }
        let last = fold_span
            .filter(|(first, _)| *first == index)
            .map_or(index, |(_, last)| last);
        units.push(WindowUnit {
            start: group.start,
            end: groups[last].end,
            first_group: index,
            last_group: last,
            starts_with_user: snapshot.items[group.start].key.kind == ContextItemKind::User,
            carries_active_memory: fold_span.is_some_and(|(first, _)| first == index),
        });
        index = last + 1;
    }
    units
}

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
        if checkpoint.active_start.is_some_and(|start| {
            !checkpoint
                .history
                .as_ref()
                .is_some_and(|section| section.covered.contains(&start))
        }) {
            return Err(ContextError::Checkpoint("active start is not covered"));
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

fn section_tokens(section: &CheckpointSection) -> usize {
    count_tokens(&summary_message(section).to_string())
}

fn truncation_message() -> Value {
    json!({ "role": "assistant", "content": TRUNCATION_MARKER })
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
    active: &ActiveContext,
) -> Result<(Vec<Value>, usize), ContextError> {
    let covered = validated_coverage(snapshot, checkpoint)?;
    if snapshot.items.iter().any(|item| {
        !covered.contains(&item.key) && !active.is_archived(&item.key) && !item.payload_loaded
    }) {
        return Err(ContextError::Snapshot("uncovered payload is not loaded"));
    }
    let mut messages = snapshot.instructions.clone();
    let active_section = visible_active_section(checkpoint, active);
    let active_after = active_section.and_then(|section| active_memory_after(snapshot, section));
    let history = checkpoint.as_ref().and_then(|value| value.history.as_ref());
    if let Some(section) = history {
        messages.push(summary_message(section));
    }
    if active.truncated && active.head.is_none() {
        messages.push(truncation_message());
    }
    for item in &snapshot.items {
        if !covered.contains(&item.key) && !active.is_archived(&item.key) {
            messages.extend(item.prompt_messages.iter().cloned());
        }
        if active.truncated && active.head == Some(item.key) {
            messages.push(truncation_message());
        }
        if let Some(section) = active_section {
            if active_after == Some(item.key) {
                messages.push(summary_message(section));
            }
        }
    }
    Ok((
        messages,
        projected_token_estimate(snapshot, checkpoint, &covered, active),
    ))
}

/// Active memory follows its anchor, so it leaves the prompt with that anchor.
fn visible_active_section<'a>(
    checkpoint: &'a Option<CheckpointPayload>,
    active: &ActiveContext,
) -> Option<&'a CheckpointSection> {
    checkpoint
        .as_ref()
        .and_then(|value| value.active.as_ref())
        .filter(|section| !section.anchor.is_some_and(|key| active.is_archived(&key)))
}

/// Estimate a complete projection from metadata without constructing messages.
/// Unlike `project`, this is safe for explicitly unloaded planning items.
pub fn estimate_tokens(
    snapshot: &ContextSnapshot,
    checkpoint: &Option<CheckpointPayload>,
    active: &ActiveContext,
) -> Result<usize, ContextError> {
    let covered = validated_coverage(snapshot, checkpoint)?;
    Ok(projected_token_estimate(
        snapshot, checkpoint, &covered, active,
    ))
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
    active: &ActiveContext,
) -> usize {
    let raw_tokens = snapshot
        .items
        .iter()
        .filter(|item| !covered.contains(&item.key) && !active.is_archived(&item.key))
        .map(|item| item.estimated_tokens)
        .fold(snapshot.instruction_tokens, usize::saturating_add);
    let marker = if active.truncated {
        count_tokens(TRUNCATION_MARKER)
    } else {
        0
    };
    checkpoint
        .iter()
        .flat_map(|checkpoint| checkpoint.history.iter())
        .chain(visible_active_section(checkpoint, active))
        .map(section_tokens)
        .fold(raw_tokens.saturating_add(marker), usize::saturating_add)
}

/// Helper input for one source item: its content plus its JSON transcript line.
fn summary_input_tokens(item: &ContextItem) -> usize {
    const TRANSCRIPT_LINE_TOKENS: usize = 24;
    item.estimated_tokens.saturating_add(TRANSCRIPT_LINE_TOKENS)
}

/// Plan the summary of the active window's older portion. One history section
/// updates any previous summary in a single helper operation; the recent tail
/// stays verbatim. An older active section and its anchor are one unit.
pub fn plan(
    snapshot: &ContextSnapshot,
    checkpoint: &Option<CheckpointPayload>,
    active: &ActiveContext,
    policy: &CompactionPolicy,
    force: bool,
) -> Result<ContextPlan, ContextError> {
    let covered = validated_coverage(snapshot, checkpoint)?;
    let estimated_tokens = projected_token_estimate(snapshot, checkpoint, &covered, active);
    let groups = atomic_groups(snapshot);
    let current = item_position(snapshot, &snapshot.current_user)?;
    let replayed =
        |item: &&ContextItem| !covered.contains(&item.key) && !active.is_archived(&item.key);
    let mut retained = HashSet::new();
    let mut recent_tokens = 0usize;
    for (index, group) in groups.iter().enumerate().rev() {
        let raw_tokens = snapshot.items[group.start..=group.end]
            .iter()
            .filter(replayed)
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

    let old_active = previous_active(snapshot, checkpoint);
    let fold_through = old_active
        .into_iter()
        .flat_map(|section| section.covered.iter().copied().chain(section.anchor))
        .filter_map(|key| snapshot.items.iter().position(|item| item.key == key))
        .max();
    let units = window_units(snapshot, checkpoint, &groups, current);
    let mut older = Vec::new();
    for unit in &units {
        let items = &snapshot.items[unit.start..=unit.end];
        if !items.iter().any(|item| replayed(&item)) {
            continue;
        }
        let eligible = groups[unit.first_group..=unit.last_group]
            .iter()
            .all(|group| group.complete)
            && items.iter().all(|item| {
                item.settled
                    && !(item.protected_input
                        && item.response_uuid == Some(snapshot.current_response))
            });
        let must_fold = fold_through.is_some_and(|end| unit.start <= end);
        let keep_raw = (unit.first_group..=unit.last_group).any(|index| retained.contains(&index));
        if !eligible || (keep_raw && !must_fold) {
            break;
        }
        older.push(unit);
    }

    let previous_history = checkpoint.as_ref().and_then(|value| value.history.as_ref());
    let helper_budget =
        policy
            .summary_input_budget
            .saturating_sub(summary::request_overhead_tokens(
                previous_history.map(|section| section.summary.as_str()),
            ));
    let unit_input = |unit: &WindowUnit| {
        let source = snapshot.items[unit.start..=unit.end]
            .iter()
            .filter(replayed)
            .map(summary_input_tokens)
            .fold(0usize, usize::saturating_add);
        match (unit.carries_active_memory, old_active) {
            (true, Some(section)) => source.saturating_add(section_tokens(section)),
            _ => source,
        }
    };
    // The window guarantees the main prompt fits, not that a helper still has
    // room to generate. Narrow from the old end, keeping the first user message
    // like the window does, rather than split the remainder into another call.
    let head = older.first().copied().filter(|unit| {
        active.head == Some(snapshot.items[unit.start].key) && unit_input(unit) <= helper_budget
    });
    let mut input = head.map_or(0, unit_input);
    let mut selected = Vec::new();
    for unit in older.iter().rev() {
        if head.is_some_and(|head| head.start == unit.start) {
            continue;
        }
        input = input.saturating_add(unit_input(unit));
        if input > helper_budget {
            break;
        }
        selected.push(*unit);
    }
    selected.extend(head);
    selected.reverse();
    let history: Vec<_> = selected
        .iter()
        .flat_map(|unit| snapshot.items[unit.start..=unit.end].iter())
        .filter(|item| replayed(item))
        .cloned()
        .collect();

    let mut active_items = Vec::new();
    let mut active_input = 0usize;
    let active_previous = checkpoint
        .as_ref()
        .and_then(|value| value.active.as_ref())
        .filter(|section| section.anchor == Some(snapshot.current_user));
    let active_budget =
        policy
            .summary_input_budget
            .saturating_sub(summary::request_overhead_tokens(
                active_previous.map(|section| section.summary.as_str()),
            ));
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
            .map(summary_input_tokens)
            .fold(0usize, usize::saturating_add);
        if !active_items.is_empty() && active_input.saturating_add(tokens) > active_budget {
            break;
        }
        active_items.extend(
            items
                .iter()
                .filter(|item| !covered.contains(&item.key))
                .cloned(),
        );
        active_input = active_input.saturating_add(tokens);
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
    let new_start = history_active_start(snapshot, checkpoint, &covered, &history)?;
    let drops_previous_active = !history.is_empty()
        && !folds_previous_active
        && old_active
            .and_then(|section| section.anchor)
            .is_some_and(|anchor| {
                active.is_archived(&anchor)
                    || new_start.is_some_and(|start| {
                        snapshot.items.iter().position(|item| item.key == anchor)
                            < snapshot.items.iter().position(|item| item.key == start)
                    })
            });
    let closes_previous_active = folds_previous_active || drops_previous_active;
    if !history.is_empty() {
        let mut history = history;
        let mut keys = previous_history
            .map(|section| section.covered.clone())
            .unwrap_or_default();
        if closes_previous_active {
            if let Some(section) = old_active {
                if folds_previous_active {
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
                }
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
            drops_previous_active,
            active_start: new_start,
        });
    }
    // There is one anchored active section. Never overwrite an older anchored
    // section until it has been incorporated into history successfully.
    if !active_items.is_empty() && (old_active.is_none() || closes_previous_active) {
        let mut keys = active_previous
            .map(|section| section.covered.clone())
            .unwrap_or_default();
        keys.extend(active_items.iter().map(|item| item.key));
        order_coverage(&mut keys, snapshot);
        result.summary_work.push(SummaryWork {
            kind: SectionKind::Active,
            anchor: Some(snapshot.current_user),
            previous_summary: active_previous.map(|section| section.summary.clone()),
            items: active_items,
            covered: keys,
            folds_previous_active: false,
            drops_previous_active: false,
            active_start: None,
        });
    }
    Ok(result)
}

/// The first selected item after the latest source this summary skips. Skipped
/// source already behind a published start needs no new boundary.
fn history_active_start(
    snapshot: &ContextSnapshot,
    checkpoint: &Option<CheckpointPayload>,
    covered: &HashSet<ContextItemKey>,
    history: &[ContextItem],
) -> Result<Option<ContextItemKey>, ContextError> {
    let Some(last) = history.last() else {
        return Ok(None);
    };
    let selected: HashSet<_> = history.iter().map(|item| item.key).collect();
    let published = checkpoint
        .as_ref()
        .and_then(|value| value.active_start)
        .map(|start| item_position(snapshot, &start))
        .transpose()?
        .unwrap_or(0);
    let last = item_position(snapshot, &last.key)?;
    let gap = (published..last).rev().find(|index| {
        let key = snapshot.items[*index].key;
        !covered.contains(&key) && !selected.contains(&key)
    });
    Ok(gap.and_then(|gap| {
        snapshot.items[gap + 1..=last]
            .iter()
            .find(|item| selected.contains(&item.key))
            .map(|item| item.key)
    }))
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
                if work.folds_previous_active || work.drops_previous_active {
                    next.active = None;
                }
                next.active_start = work.active_start.or(next.active_start);
            }
            SectionKind::Active => next.active = section,
        }
    }
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(
        snapshot: &ContextSnapshot,
        checkpoint: &Option<CheckpointPayload>,
        policy: &CompactionPolicy,
        force: bool,
    ) -> Result<ContextPlan, ContextError> {
        let active = ActiveContext::select(snapshot, checkpoint, None)?;
        super::plan(snapshot, checkpoint, &active, policy, force)
    }

    fn project(
        snapshot: &ContextSnapshot,
        checkpoint: &Option<CheckpointPayload>,
    ) -> Result<(Vec<Value>, usize), ContextError> {
        let active = ActiveContext::select(snapshot, checkpoint, None)?;
        super::project(snapshot, checkpoint, &active)
    }

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
            summary_input_budget: 10_000,
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
            let planned = plan(&snapshot, &None, &policy, false).unwrap();
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
                    vec![
                        key(ContextItemKind::User, 1),
                        key(ContextItemKind::Assistant, 2),
                    ],
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
            active_start: None,
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
    fn huge_unicode_tool_source_reaches_the_helper_whole_without_private_reasoning() {
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
            drops_previous_active: false,
            active_start: None,
        };
        let messages: Vec<_> = work
            .items
            .iter()
            .flat_map(|item| item.summary_messages.iter().cloned())
            .collect();
        let transcript = super::super::summary::serialize_transcript(&messages).unwrap();
        assert_eq!(transcript.len(), work.items.len() - 1);
        let full = Value::Array(transcript).to_string();
        assert!(full.contains(&text));
        assert!(!full.contains("private secret"));
        assert!(!full.contains("private thinking"));
        assert!(full.contains("https://example.test/reference"));
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
        assert!(!Value::Array(transcript).to_string().contains("item-2"));
    }

    #[test]
    fn one_history_selection_covers_the_older_portion_without_splitting_tool_pairs() {
        let mut items = vec![item(ContextItemKind::User, 1, 100)];
        items.extend(tool_pair(2, 3, 300));
        items.push(item(ContextItemKind::Assistant, 4, 100));
        items.push(item(ContextItemKind::User, 5, 100));
        let snapshot = snapshot(items, 5);
        let mut policy = policy();
        policy.keep_recent_tokens = 0;
        let planned = plan(&snapshot, &None, &policy, true).unwrap();
        assert_eq!(planned.summary_work.len(), 1);
        assert_eq!(
            planned.summary_work[0].covered,
            vec![
                key(ContextItemKind::User, 1),
                key(ContextItemKind::ToolCall, 2),
                key(ContextItemKind::ToolOutput, 3),
                key(ContextItemKind::Assistant, 4),
            ]
        );
    }

    #[test]
    fn one_active_selection_covers_every_completed_tool_step() {
        let mut items = vec![item(ContextItemKind::User, 1, 100)];
        items.extend(tool_pair(2, 3, 100));
        items.extend(tool_pair(4, 5, 100));
        items.extend(tool_pair(6, 7, 100));
        let snapshot = snapshot(items, 1);
        let mut policy = policy();
        policy.keep_recent_tokens = 0;
        let planned = plan(&snapshot, &None, &policy, true).unwrap();
        assert_eq!(planned.summary_work.len(), 1);
        assert_eq!(planned.summary_work[0].kind, SectionKind::Active);
        assert_eq!(planned.summary_work[0].items.len(), 6);
        let payload = complete_plan(&None, &planned, &["completed lookups".into()]).unwrap();
        assert_eq!(payload.active.as_ref().unwrap().covered.len(), 6);
        let checkpoint = Some(payload);
        assert!(plan(&snapshot, &checkpoint, &policy, true)
            .unwrap()
            .summary_work
            .is_empty());
        let projected = project(&snapshot, &checkpoint).unwrap().0;
        assert_eq!(projected[1], snapshot.items[0].prompt_messages[0]);
    }

    #[test]
    fn helper_room_keeps_active_tool_steps_from_the_oldest_so_coverage_has_no_hole() {
        let mut items = vec![item(ContextItemKind::User, 1, 100)];
        items.extend(tool_pair(2, 3, 300));
        items.extend(tool_pair(4, 5, 300));
        let snapshot = snapshot(items, 1);
        let mut policy = policy();
        policy.keep_recent_tokens = 0;
        policy.summary_input_budget =
            super::summary::request_overhead_tokens(None).saturating_add(800);
        let planned = plan(&snapshot, &None, &policy, true).unwrap();
        assert_eq!(
            planned.summary_work[0].covered,
            vec![
                key(ContextItemKind::ToolCall, 2),
                key(ContextItemKind::ToolOutput, 3),
            ]
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
        let plan = plan(&metadata, &checkpoint, &policy, true).unwrap();
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
        let plan = plan(&metadata, &None, &policy, true).unwrap();
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

    fn active_memory_fixture() -> (ContextSnapshot, Option<CheckpointPayload>) {
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
        (snapshot, checkpoint)
    }

    #[test]
    fn history_selection_folds_an_older_active_anchor_with_its_complete_memory() {
        let (snapshot, checkpoint) = active_memory_fixture();
        let mut policy = policy();
        policy.keep_recent_tokens = 0;
        let planned = plan(&snapshot, &checkpoint, &policy, true).unwrap();
        let work = &planned.summary_work[0];
        assert!(work.folds_previous_active);
        assert_eq!(work.items[0].key, key(ContextItemKind::User, 1));
        assert_eq!(work.items[1].key, key(ContextItemKind::User, 2));
        assert_eq!(work.items[1].summary_messages[0]["content"], "item-2");
        assert!(work.items[1].summary_messages[1]["content"]
            .as_str()
            .unwrap()
            .contains("lookup after question two"));
        assert!(work.covered.contains(&key(ContextItemKind::ToolCall, 3)));
        assert!(work.covered.contains(&key(ContextItemKind::ToolOutput, 4)));
        assert!(work.covered.contains(&key(ContextItemKind::Assistant, 5)));
        assert!(complete_plan(&checkpoint, &planned, &["memory".into()])
            .unwrap()
            .active
            .is_none());
    }

    #[test]
    fn narrowing_past_an_older_active_anchor_closes_that_section_with_its_coverage() {
        let (snapshot, checkpoint) = active_memory_fixture();
        let mut policy = policy();
        policy.keep_recent_tokens = 0;
        policy.summary_input_budget =
            super::summary::request_overhead_tokens(None).saturating_add(130);
        let planned = plan(&snapshot, &checkpoint, &policy, true).unwrap();
        let work = &planned.summary_work[0];
        assert!(!work.folds_previous_active);
        assert!(work.drops_previous_active);
        assert_eq!(
            work.items.iter().map(|item| item.key).collect::<Vec<_>>(),
            vec![key(ContextItemKind::Assistant, 5)]
        );
        assert_eq!(work.active_start, Some(key(ContextItemKind::Assistant, 5)));
        // Publication requires the new coverage to contain the previous one.
        assert!(work.covered.contains(&key(ContextItemKind::ToolCall, 3)));
        assert!(work.covered.contains(&key(ContextItemKind::ToolOutput, 4)));
        let next = complete_plan(&checkpoint, &planned, &["memory".into()]).unwrap();
        assert!(next.active.is_none());
        let next = Some(next);
        let (messages, _) = super::project(
            &snapshot,
            &next,
            &ActiveContext::select(&snapshot, &next, Some(2_000)).unwrap(),
        )
        .unwrap();
        let text = Value::Array(messages).to_string();
        assert!(!text.contains("lookup after question two"));
        assert!(!text.contains("item-1"));
        assert!(!text.contains("item-2"));
        assert!(text.contains("item-6"));
    }

    #[test]
    fn blocked_active_fold_keeps_anchor_and_memory_out_of_the_selection() {
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
        assert!(plan(&snapshot, &checkpoint, &policy, true)
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
        let first = plan(&snapshot, &None, &policy, true).unwrap();
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
        let next = plan(&snapshot, &checkpoint, &policy, true).unwrap();
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
        let plan = plan(&snapshot, &None, &policy, true).unwrap();
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

    const WINDOW: usize = 262_144;
    const ARCHIVED_SENTINEL: &str = "ARCHIVED-SENTINEL";
    const ACTIVE_FACT: &str = "ACTIVE-FACT";

    /// A stored conversation about twelve times the model window, in real turns
    /// of user, a completed tool exchange, and an answer.
    fn over_window_snapshot(turns: u128, tokens_per_item: usize) -> ContextSnapshot {
        let mut items = Vec::new();
        let archive_end = turns - turns / 8;
        for turn in 0..turns {
            let label = if turn >= archive_end {
                ACTIVE_FACT
            } else {
                ARCHIVED_SENTINEL
            };
            let base = 10 + turn * 10;
            let text =
                |role: &str, id: u128| json!({"role": role, "content": format!("{label}-{id}")});
            let mut user = item(ContextItemKind::User, base, tokens_per_item);
            user.prompt_messages = vec![text("user", base)];
            user.summary_messages = user.prompt_messages.clone();
            items.push(user);
            let mut pair = tool_pair(base + 1, base + 2, tokens_per_item / 2);
            pair[1].prompt_messages[0]["content"] = json!(format!("{label}-{}", base + 2));
            pair[1].summary_messages = pair[1].prompt_messages.clone();
            items.extend(pair);
            let mut answer = item(ContextItemKind::Assistant, base + 3, tokens_per_item);
            answer.prompt_messages = vec![text("assistant", base + 3)];
            answer.summary_messages = answer.prompt_messages.clone();
            items.push(answer);
        }
        let mut current = item(ContextItemKind::User, 5, 40);
        current.prompt_messages = vec![json!({"role":"user","content":"CURRENT-QUESTION"})];
        current.settled = false;
        current.response_uuid = Some(Uuid::from_u128(1_000));
        items.push(current);
        let mut snapshot = snapshot(items, 5);
        snapshot.instruction_tokens = 500;
        for item in &mut snapshot.items {
            if item.key.kind != ContextItemKind::User || item.key.uuid != Uuid::from_u128(5) {
                item.response_uuid = Some(Uuid::from_u128(2_000));
            }
        }
        snapshot
    }

    fn helper_prompt(work: &SummaryWork) -> String {
        let messages: Vec<_> = work
            .items
            .iter()
            .flat_map(|item| item.summary_messages.iter().cloned())
            .collect();
        let transcript = super::summary::serialize_transcript(&messages).unwrap();
        Value::Array(super::summary::summary_messages(
            transcript,
            work.previous_summary.as_deref(),
        ))
        .to_string()
    }

    fn assert_tool_relationships_intact(items: &[ContextItem]) {
        let calls: HashSet<_> = items
            .iter()
            .filter(|item| item.key.kind == ContextItemKind::ToolCall)
            .filter_map(|item| item.tool_call_id)
            .collect();
        let outputs: HashSet<_> = items
            .iter()
            .filter(|item| item.key.kind == ContextItemKind::ToolOutput)
            .filter_map(|item| item.tool_call_id)
            .collect();
        assert_eq!(calls, outputs);
    }

    fn real_policy(window: usize, output: usize) -> CompactionPolicy {
        CompactionPolicy::from_context_window(window, output, 2_048.min(window / 16))
    }

    #[test]
    fn stored_history_far_past_the_window_never_reaches_the_helper_or_returns_later() {
        let policy = real_policy(WINDOW, 0);
        let budget = Some(policy.input_budget);
        // 200 turns of 10k+5k+5k+10k: 6M tokens against a 262k window, in few
        // enough rows that the token budget is what bounds this window.
        let snapshot = over_window_snapshot(200, 10_000);
        let stored: usize = snapshot
            .items
            .iter()
            .map(|item| item.estimated_tokens)
            .sum();
        assert!(stored > 20 * WINDOW);

        let first_active = ActiveContext::select(&snapshot, &None, budget).unwrap();
        assert!(first_active.is_truncated());
        let (fallback, fallback_tokens) = super::project(&snapshot, &None, &first_active).unwrap();
        assert!(fallback_tokens <= policy.input_budget);
        assert_eq!(fallback[0], snapshot.instructions[0]);
        assert_eq!(fallback[2]["content"], TRUNCATION_MARKER);

        let planned = super::plan(&snapshot, &None, &first_active, &policy, false).unwrap();
        assert_eq!(planned.summary_work.len(), 1);
        let work = &planned.summary_work[0];
        assert_eq!(work.kind, SectionKind::History);
        assert_tool_relationships_intact(&work.items);
        let prompt = helper_prompt(work);
        // Only the conversation's opening is kept from the archive region, like
        // the sliding window keeps its first user message.
        assert_eq!(prompt.matches(ARCHIVED_SENTINEL).count(), 1);
        assert!(prompt.contains(&format!("{ARCHIVED_SENTINEL}-10")));
        assert!(prompt.contains(ACTIVE_FACT));
        assert!(!prompt.contains("CURRENT-QUESTION"));
        assert!(!work.covered.contains(&snapshot.current_user));
        let input: usize = work.items.iter().map(summary_input_tokens).sum();
        assert!(
            input + super::summary::request_overhead_tokens(None) <= policy.summary_input_budget
        );
        assert!(work.items.len() < snapshot.items.len() / 8);
        assert!(work.active_start.is_some());

        let first =
            Some(complete_plan(&None, &planned, &["## Goal\nACTIVE-SUMMARY".into()]).unwrap());
        // The summary freed nearly the whole budget. Archived source must not
        // slide back in even though much of it would now fit.
        let reloaded = ActiveContext::select(&snapshot, &first, budget).unwrap();
        assert!(!reloaded.is_truncated());
        let (messages, tokens) = super::project(&snapshot, &first, &reloaded).unwrap();
        let text = Value::Array(messages.clone()).to_string();
        assert!(!text.contains(ARCHIVED_SENTINEL));
        assert!(text.contains("ACTIVE-SUMMARY"));
        assert!(text.contains(ACTIVE_FACT));
        assert_eq!(messages.last().unwrap()["content"], "CURRENT-QUESTION");
        assert!(tokens <= policy.trigger_tokens);
        assert!(tokens < fallback_tokens);

        // A later compaction updates the summary from the recent tail alone.
        let everything = CompactionPolicy {
            keep_recent_tokens: 0,
            ..policy
        };
        let forced = super::plan(&snapshot, &first, &reloaded, &everything, true).unwrap();
        assert_eq!(forced.summary_work.len(), 1);
        let later = &forced.summary_work[0];
        assert!(later.previous_summary.is_some());
        assert!(later.active_start.is_none());
        let prompt = helper_prompt(later);
        assert!(!prompt.contains(ARCHIVED_SENTINEL));
        assert!(prompt.contains("ACTIVE-SUMMARY"));
        let second = complete_plan(&first, &forced, &["## Goal\nUPDATED".into()]).unwrap();
        assert_eq!(second.active_start, first.as_ref().unwrap().active_start);
    }

    #[test]
    fn summary_and_retained_tail_fit_every_window_and_caller_output_reservation() {
        for window in [131_072, WINDOW, 1_048_576] {
            for output in [0, 8_192, 32_000] {
                let policy = real_policy(window, output);
                let budget = Some(policy.input_budget);
                let snapshot = over_window_snapshot(800, 8_000);
                let active = ActiveContext::select(&snapshot, &None, budget).unwrap();
                let (_, windowed) = super::project(&snapshot, &None, &active).unwrap();
                assert!(windowed <= policy.input_budget, "{window}/{output}");
                let planned = super::plan(&snapshot, &None, &active, &policy, false).unwrap();
                let summary = "A checkpoint section. ".repeat(4_000);
                let next = Some(complete_plan(&None, &planned, &[summary]).unwrap());
                let next_active = ActiveContext::select(&snapshot, &next, budget).unwrap();
                let (_, compacted) = super::project(&snapshot, &next, &next_active).unwrap();
                assert!(compacted < policy.trigger_tokens, "{window}/{output}");
                let input: usize = planned.summary_work[0]
                    .items
                    .iter()
                    .map(summary_input_tokens)
                    .sum();
                // The helper leaves generation room inside the model's own window.
                assert!(input <= policy.summary_input_budget, "{window}/{output}");
                assert!(window - policy.summary_input_budget >= 16_384);
            }
        }
    }

    #[test]
    fn newest_rows_bound_the_window_and_the_first_user_message_inside_them() {
        let mut items = Vec::new();
        for id in 0..1_500 {
            items.push(item(
                if id % 2 == 0 {
                    ContextItemKind::User
                } else {
                    ContextItemKind::Assistant
                },
                id + 10,
                100,
            ));
        }
        items.push(item(ContextItemKind::User, 5, 100));
        let snapshot = snapshot(items, 5);
        // 150k tokens fit this budget, so only the row limit applies. It leaves
        // no marker, like the sliding window, which simply never read older rows.
        let active = ActiveContext::select(&snapshot, &None, Some(1_000_000)).unwrap();
        assert_eq!(active.archived_items(), 500);
        assert!(!active.is_truncated());
        assert!(active.is_archived(&snapshot.items[499].key));
        assert!(!active.is_archived(&snapshot.items[500].key));
        // An over-budget window opens its head inside those rows.
        let active = ActiveContext::select(&snapshot, &None, Some(20_000)).unwrap();
        assert!(active.is_truncated());
        assert_eq!(active.head, Some(snapshot.items[500].key));
    }

    #[test]
    fn checkpoint_summary_replaces_the_head_and_a_new_gap_moves_the_start_forward() {
        let mut items = Vec::new();
        for id in 1..=12 {
            items.push(item(
                if id % 2 == 1 {
                    ContextItemKind::User
                } else {
                    ContextItemKind::Assistant
                },
                id,
                100,
            ));
        }
        items.push(item(ContextItemKind::User, 13, 10));
        let snapshot = snapshot(items, 13);
        let checkpoint = Some(CheckpointPayload {
            history: Some(CheckpointSection {
                summary: "first".into(),
                covered: vec![key(ContextItemKind::User, 1)],
                anchor: None,
            }),
            ..CheckpointPayload::default()
        });
        let budget = 10 + count_tokens("first") + 400;
        let active = ActiveContext::select(&snapshot, &checkpoint, Some(budget)).unwrap();
        assert!(active.is_truncated());
        assert_eq!(active.head, None);
        assert!(active.is_archived(&key(ContextItemKind::Assistant, 2)));
        let mut policy = policy();
        policy.keep_recent_tokens = 100;
        let planned = super::plan(&snapshot, &checkpoint, &active, &policy, true).unwrap();
        let work = &planned.summary_work[0];
        assert_eq!(work.active_start, Some(work.items[0].key));
        let next = Some(complete_plan(&checkpoint, &planned, &["second".into()]).unwrap());
        assert_eq!(next.as_ref().unwrap().active_start, Some(work.items[0].key));
        let reloaded = ActiveContext::select(&snapshot, &next, Some(1_000_000)).unwrap();
        assert!(reloaded.is_archived(&key(ContextItemKind::Assistant, 2)));
        assert!(!reloaded.is_truncated());
    }

    #[test]
    fn helper_room_keeps_the_newest_older_source_and_the_opening_message() {
        let mut items = Vec::new();
        for id in 1..=20 {
            items.push(item(
                if id % 2 == 1 {
                    ContextItemKind::User
                } else {
                    ContextItemKind::Assistant
                },
                id,
                100,
            ));
        }
        items.push(item(ContextItemKind::User, 21, 10));
        let snapshot = snapshot(items, 21);
        let budget = 1_700;
        let active = ActiveContext::select(&snapshot, &None, Some(budget)).unwrap();
        let mut policy = policy();
        policy.input_budget = budget;
        policy.trigger_tokens = 1;
        policy.keep_recent_tokens = 100;
        policy.summary_input_budget =
            super::summary::request_overhead_tokens(None).saturating_add(620);
        let planned = super::plan(&snapshot, &None, &active, &policy, false).unwrap();
        let work = &planned.summary_work[0];
        let first = work.items[0].key;
        assert_eq!(first, key(ContextItemKind::User, 1));
        assert_eq!(work.items.len(), 5);
        assert_eq!(work.active_start, Some(work.items[1].key));
        assert_eq!(
            work.items.last().unwrap().key,
            key(ContextItemKind::User, 19)
        );
    }

    #[test]
    fn explicit_opt_out_reports_overflow_instead_of_windowing_but_keeps_a_published_start() {
        let items = vec![
            item(ContextItemKind::User, 1, 900),
            item(ContextItemKind::Assistant, 2, 900),
            item(ContextItemKind::User, 3, 900),
            item(ContextItemKind::Assistant, 4, 900),
            item(ContextItemKind::User, 5, 10),
        ];
        let snapshot = snapshot(items, 5);
        let active = ActiveContext::select(&snapshot, &None, None).unwrap();
        assert_eq!(active.archived_items(), 0);
        assert_eq!(super::project(&snapshot, &None, &active).unwrap().1, 3_620);

        let checkpoint = Some(CheckpointPayload {
            history: Some(CheckpointSection {
                summary: "memory".into(),
                covered: vec![key(ContextItemKind::User, 3)],
                anchor: None,
            }),
            active_start: Some(key(ContextItemKind::User, 3)),
            ..CheckpointPayload::default()
        });
        let active = ActiveContext::select(&snapshot, &checkpoint, None).unwrap();
        assert_eq!(active.archived_items(), 2);
        assert!(!active.is_truncated());
    }

    #[test]
    fn refit_with_hydrated_estimates_only_narrows_and_keeps_the_head() {
        let mut items = Vec::new();
        for id in 1..=8 {
            items.push(item(
                if id % 2 == 1 {
                    ContextItemKind::User
                } else {
                    ContextItemKind::Assistant
                },
                id,
                100,
            ));
        }
        items.push(item(ContextItemKind::User, 9, 10));
        let mut snapshot = snapshot(items, 9);
        let budget = Some(500);
        let selected = ActiveContext::select(&snapshot, &None, budget).unwrap();
        assert!(selected.is_truncated());
        // A lower real estimate must never admit an unloaded archived item.
        for item in &mut snapshot.items {
            item.estimated_tokens = 1;
        }
        let same = selected.refit(&snapshot, &None, budget).unwrap();
        assert_eq!(same, selected);
        // A higher one narrows further, like the sliding window's second pass.
        for item in &mut snapshot.items {
            item.estimated_tokens = 300;
        }
        snapshot.items.last_mut().unwrap().estimated_tokens = 10;
        let narrowed = selected.refit(&snapshot, &None, budget).unwrap();
        assert!(narrowed.archived_items() > selected.archived_items());
        assert_eq!(narrowed.head, selected.head);
        assert!(selected
            .archived
            .iter()
            .all(|key| narrowed.is_archived(key)));
    }

    #[test]
    fn active_start_must_be_covered_history_and_older_payloads_still_decode() {
        let items = vec![
            item(ContextItemKind::User, 1, 100),
            item(ContextItemKind::User, 2, 100),
        ];
        let snapshot = snapshot(items, 2);
        let checkpoint = Some(CheckpointPayload {
            active_start: Some(key(ContextItemKind::User, 1)),
            ..CheckpointPayload::default()
        });
        assert_eq!(
            ActiveContext::select(&snapshot, &checkpoint, None),
            Err(ContextError::Checkpoint("active start is not covered"))
        );

        let published = json!({"version": 1, "history": null, "active": null});
        let decoded: CheckpointPayload = serde_json::from_value(published.clone()).unwrap();
        assert_eq!(decoded.active_start, None);
        assert_eq!(serde_json::to_value(&decoded).unwrap(), published);
    }

    #[test]
    fn window_never_bisects_a_tool_exchange_or_opens_on_an_assistant_message() {
        let mut items = vec![item(ContextItemKind::User, 1, 100)];
        items.push(item(ContextItemKind::Assistant, 2, 100));
        items.push(item(ContextItemKind::User, 3, 100));
        items.extend(tool_pair(4, 5, 200));
        items.push(item(ContextItemKind::Assistant, 6, 100));
        items.push(item(ContextItemKind::User, 7, 100));
        items.extend(tool_pair(8, 9, 200));
        items.push(item(ContextItemKind::Assistant, 10, 100));
        items.push(item(ContextItemKind::User, 11, 10));
        let snapshot = snapshot(items, 11);
        // Budget reaches into the middle of the first tool turn.
        let active = ActiveContext::select(&snapshot, &None, Some(900)).unwrap();
        for key in [
            key(ContextItemKind::User, 3),
            key(ContextItemKind::ToolCall, 4),
            key(ContextItemKind::ToolOutput, 5),
            key(ContextItemKind::Assistant, 6),
        ] {
            assert!(active.is_archived(&key));
        }
        assert!(!active.is_archived(&key(ContextItemKind::User, 7)));
        assert!(!active.is_archived(&key(ContextItemKind::ToolCall, 8)));
        assert!(!active.is_archived(&key(ContextItemKind::ToolOutput, 9)));
    }

    #[test]
    fn empty_rows_outside_the_window_are_archived_so_their_payloads_are_never_read() {
        let mut empty = item(ContextItemKind::Assistant, 3, 0);
        empty.prompt_messages.clear();
        empty.summary_messages.clear();
        empty.payload_loaded = false;
        let items = vec![
            item(ContextItemKind::User, 1, 100),
            item(ContextItemKind::Assistant, 2, 900),
            empty,
            item(ContextItemKind::User, 4, 100),
            item(ContextItemKind::Assistant, 5, 100),
            item(ContextItemKind::User, 6, 10),
        ];
        let snapshot = snapshot(items, 6);
        let active = ActiveContext::select(&snapshot, &None, Some(500)).unwrap();
        assert!(active.is_archived(&key(ContextItemKind::Assistant, 3)));
        assert!(super::project(&snapshot, &None, &active).is_ok());
    }

    /// Rows of ordinary turns, so the 1,000-row cut lands where a test places it.
    fn filler_rows(first_id: u128, rows: u128) -> Vec<ContextItem> {
        (0..rows)
            .map(|index| {
                let kind = if index % 2 == 0 {
                    ContextItemKind::User
                } else {
                    ContextItemKind::Assistant
                };
                item(kind, first_id + index, 10)
            })
            .collect()
    }

    #[test]
    fn row_cutoff_keeps_a_tool_exchange_whole_rather_than_orphan_its_result() {
        let mut items = vec![item(ContextItemKind::User, 10, 10)];
        items.extend(tool_pair(11, 12, 10));
        items.extend(filler_rows(100, 999));
        items.push(item(ContextItemKind::User, 5, 10));
        let snapshot = snapshot(items, 5);
        // 1,002 history rows put the cut between the call and its result, and
        // the budget fits, so nothing but the row limit acts on this window.
        let active = ActiveContext::select(&snapshot, &None, Some(100_000)).unwrap();
        assert!(!active.is_truncated());
        assert!(active.is_archived(&key(ContextItemKind::ToolCall, 11)));
        assert!(active.is_archived(&key(ContextItemKind::ToolOutput, 12)));
        assert_eq!(active.archived_items(), 3);

        let (messages, _) = super::project(&snapshot, &None, &active).unwrap();
        let mut calls = HashSet::new();
        for message in &messages {
            for call in message["tool_calls"].as_array().into_iter().flatten() {
                calls.insert(call["id"].as_str().unwrap().to_owned());
            }
            if message["role"] == "tool" {
                assert!(calls.contains(message["tool_call_id"].as_str().unwrap()));
            }
        }
    }

    #[test]
    fn row_cutoff_between_reasoning_and_its_answer_leaves_a_publishable_checkpoint() {
        let mut items = vec![
            item(ContextItemKind::User, 10, 10),
            item(ContextItemKind::Reasoning, 11, 0),
            item(ContextItemKind::Assistant, 12, 10),
        ];
        items.extend(filler_rows(100, 999));
        items.push(item(ContextItemKind::User, 5, 10));
        let snapshot = snapshot(items, 5);
        let budget = Some(100_000);
        let active = ActiveContext::select(&snapshot, &None, budget).unwrap();
        let mut policy = policy();
        policy.keep_recent_tokens = 0;
        policy.summary_input_budget = 1_000_000;
        let planned = super::plan(&snapshot, &None, &active, &policy, true).unwrap();
        let next = Some(complete_plan(&None, &planned, &["memory".into()]).unwrap());
        // A checkpoint covering half of this group would be rejected here, on
        // the path that runs after the helper has already been paid for.
        let reloaded = ActiveContext::select(&snapshot, &next, budget).unwrap();
        assert!(super::estimate_tokens(&snapshot, &next, &reloaded).is_ok());
        assert!(reloaded.is_archived(&key(ContextItemKind::Reasoning, 11)));
        assert!(reloaded.is_archived(&key(ContextItemKind::Assistant, 12)));
    }

    #[test]
    fn row_cutoff_never_reaches_into_a_pending_exchange_around_the_current_input() {
        let mut items = vec![item(ContextItemKind::User, 10, 10)];
        let mut pair = tool_pair(11, 12, 10);
        let output = pair.pop().unwrap();
        items.append(&mut pair);
        items.extend(filler_rows(100, 1_000));
        items.push(item(ContextItemKind::User, 5, 10));
        items.push(output);
        let snapshot = snapshot(items, 5);
        let active = ActiveContext::select(&snapshot, &None, Some(100_000)).unwrap();
        assert!(!active.is_archived(&snapshot.current_user));
        assert!(!active.is_archived(&key(ContextItemKind::ToolCall, 11)));
        assert!(!active.is_archived(&key(ContextItemKind::ToolOutput, 12)));
    }

    #[test]
    fn required_current_turn_is_never_windowed_out() {
        let mut items = vec![
            item(ContextItemKind::User, 1, 100),
            item(ContextItemKind::Assistant, 2, 100),
            item(ContextItemKind::User, 3, 50),
        ];
        items.extend(tool_pair(4, 5, 300));
        let snapshot = snapshot(items, 3);
        let active = ActiveContext::select(&snapshot, &None, Some(200)).unwrap();
        assert!(!active.is_archived(&snapshot.current_user));
        assert!(!active.is_archived(&key(ContextItemKind::ToolCall, 4)));
        assert!(!active.is_archived(&key(ContextItemKind::ToolOutput, 5)));
        assert!(active.is_archived(&key(ContextItemKind::User, 1)));
    }
}
