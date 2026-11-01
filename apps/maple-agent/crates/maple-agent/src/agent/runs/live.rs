//! Live occurrences and their exact source-to-row reconciliation.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Write};
use std::sync::Arc;

use pi_ai::{Message, content_text};
use pi_coding_agent::SessionMessage;
use serde_json::Value;
use tokio::sync::broadcast;

use crate::agent::{AgentLiveRowIdentity, AgentPersistedRowIdentity, AgentTimelineItem, timeline};
use crate::store::{
    AccountId, SourceReceipt, SourceReceipts, Store, StoreChange, StoredRowChange,
    message_signature,
};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Origin {
    entry_id: String,
    item_index: i64,
}

#[derive(Clone)]
struct Commit {
    identity: AgentPersistedRowIdentity,
    update_entry_id: String,
}

#[derive(Default)]
pub(super) struct LiveSnapshot {
    pub(super) epoch: u64,
    pub(super) items: Vec<AgentTimelineItem>,
    pub(super) changes: Option<broadcast::Receiver<StoreChange>>,
    pub(super) resolver: Option<(Arc<Store>, AccountId)>,
    pub(super) source_receipts: Option<SourceReceipts>,
    next_occurrence: u64,
    group: u64,
    groups: HashMap<(u64, String), u64>,
    origins: HashMap<Origin, u64>,
    commits: HashMap<Origin, Commit>,
    persisted: HashMap<u64, Commit>,
    tools: HashMap<String, (u64, String)>,
    tool_occurrences: HashMap<String, u64>,
    /// A tool result updates one selected row, which may have been created in
    /// an unloaded prefix. Match its real update entry, never a timestamp key.
    result_updates: HashMap<String, u64>,
    payload_sources: HashMap<u64, String>,
    sources: VecDeque<SourceReceipt>,
    completed_source: Option<String>,
    pub(super) resync_required: bool,
    needs_origin_check: bool,
}

impl LiveSnapshot {
    pub(super) fn begin_message(&mut self) {
        self.group += 1;
        self.completed_source = None;
    }

    pub(super) fn drain_sources(&mut self) {
        if let Some(receipts) = &self.source_receipts {
            for receipt in receipts.drain() {
                self.note_source(receipt);
            }
        }
    }

    pub(super) fn note_source(&mut self, receipt: SourceReceipt) {
        // Genuine upstream boundary events can name an entry already observed by
        // the adapter. One source ID must have only one candidate in this window.
        self.sources
            .retain(|source| source.entry_id != receipt.entry_id);
        if self.sources.len() == 64 {
            self.sources.pop_front();
        }
        self.sources.push_back(receipt);
    }

    /// A row of an entry Maple appended itself, such as a notice, as that
    /// entry's only row. The live row takes the stored row's identity, so a
    /// reload during the run shows it once.
    pub(super) fn note_entry_row(&mut self, entry_id: &str, row_id: &str) {
        let occurrence = self.occurrence(self.group, row_id);
        let origin = Origin {
            entry_id: entry_id.into(),
            item_index: 0,
        };
        self.origins.insert(origin.clone(), occurrence);
        if let Some(commit) = self.commits.get(&origin).cloned() {
            self.persisted.insert(occurrence, commit);
        }
    }

    pub(super) fn end_message(&mut self, message: &SessionMessage) -> Vec<AgentTimelineItem> {
        self.completed_source = None;
        let signature = message_signature(message);
        let mut candidates = self
            .sources
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.signature == signature)
            .map(|(position, _)| position);
        let Some(position) = candidates.next() else {
            // Pi deliberately does not call its store for provisional custom
            // messages before the first conversation. Keep those rows transient.
            return Vec::new();
        };
        if candidates.next().is_some() {
            // Concurrent identical custom payloads have no ordering guarantee.
            // Preserve a transient identity rather than guess a real source ID.
            return Vec::new();
        }
        let entry = self.sources.remove(position).expect("matching source");
        self.epoch += 1;
        self.completed_source = Some(entry.entry_id.clone());
        let mut rows = match message {
            SessionMessage::Llm(Message::User(user)) => vec![timeline::user_item(
                user.timestamp,
                &user.content,
                "replace",
            )],
            SessionMessage::Llm(Message::Assistant(reply)) => {
                for call in reply.tool_calls() {
                    self.tools.insert(
                        call.id.clone(),
                        (self.group, format!("a{}-{}", reply.timestamp, call.id)),
                    );
                }
                timeline::reply_items(reply)
            }
            SessionMessage::Custom(custom) if custom.display => vec![timeline::notice_item(
                format!("custom{}", custom.timestamp),
                "Agent notice",
                &content_text(&custom.content),
                custom.timestamp,
            )],
            SessionMessage::Llm(Message::ToolResult(result)) => {
                let id = self
                    .tools
                    .get(&result.tool_call_id)
                    .map(|(_, key)| key.clone())
                    .unwrap_or_else(|| format!("tool-{}", result.tool_call_id));
                vec![timeline::tool_result_item(
                    id,
                    &result.content,
                    result.details.as_ref(),
                    result.is_error,
                    result.timestamp,
                )]
            }
            _ => Vec::new(),
        };
        if let SessionMessage::Llm(Message::ToolResult(result)) = message {
            if let Some(row) = rows.first_mut() {
                let selected = self
                    .commits
                    .iter()
                    .find(|(_, commit)| commit.update_entry_id == entry.entry_id)
                    .map(|(origin, commit)| (origin.clone(), commit.clone()));
                let occurrence = self
                    .tool_occurrences
                    .get(&result.tool_call_id)
                    .copied()
                    .or_else(|| {
                        selected
                            .as_ref()
                            .and_then(|(origin, _)| self.origins.get(origin).copied())
                    })
                    .unwrap_or_else(|| self.occurrence(self.group, &row.id));
                self.tool_occurrences
                    .insert(result.tool_call_id.clone(), occurrence);
                self.result_updates
                    .insert(entry.entry_id.clone(), occurrence);
                row.live = Some(AgentLiveRowIdentity {
                    run_id: String::new(),
                    occurrence,
                });
                if let Some((origin, commit)) = selected {
                    self.origins.insert(origin, occurrence);
                    self.persisted.insert(occurrence, commit);
                }
            }
        } else {
            let mut seen = std::collections::HashSet::new();
            let mut calls = match message {
                SessionMessage::Llm(Message::Assistant(reply)) => Some(reply.tool_calls()),
                _ => None,
            };
            for (index, row) in rows.iter_mut().enumerate() {
                let occurrence = if seen.insert(row.id.clone()) {
                    self.occurrence(self.group, &row.id)
                } else {
                    self.next_occurrence += 1;
                    self.next_occurrence
                };
                row.live = Some(AgentLiveRowIdentity {
                    run_id: String::new(),
                    occurrence,
                });
                if row.item_type == "tool"
                    && let Some(call) = calls.as_mut().and_then(Iterator::next)
                {
                    self.tool_occurrences.insert(call.id.clone(), occurrence);
                }
                let origin = Origin {
                    entry_id: entry.entry_id.clone(),
                    item_index: index as i64,
                };
                self.origins.insert(origin.clone(), occurrence);
                if let Some(commit) = self.commits.get(&origin).cloned() {
                    self.persisted.insert(occurrence, commit);
                    self.remove_occurrence(occurrence);
                }
            }
        }
        rows
    }

    pub(super) fn drain_changes(&mut self, task: &str) {
        loop {
            let change = match self.changes.as_mut().map(broadcast::Receiver::try_recv) {
                Some(Ok(change)) => change,
                Some(Err(broadcast::error::TryRecvError::Lagged(_))) => {
                    // Keep every uncommitted row. A bounded consumer must refuse a
                    // lossless replay until the host resynchronizes these origins.
                    self.epoch += 1;
                    self.resync_required = true;
                    self.needs_origin_check = true;
                    continue;
                }
                _ => break,
            };
            if change.task.as_deref() == Some(task) {
                self.apply_change(task, &change);
            }
        }
        if self.needs_origin_check {
            self.repair_origins(task);
        }
    }

    fn repair_origins(&mut self, task: &str) {
        let before_len = self.items.len();
        let Some((store, account)) = self.resolver.clone() else {
            return;
        };
        let active: std::collections::HashSet<u64> = self
            .items
            .iter()
            .filter_map(|item| item.live.as_ref().map(|live| live.occurrence))
            .collect();
        let mut covered = std::collections::HashSet::new();
        let mut requests: Vec<_> = self
            .origins
            .iter()
            .filter_map(|(origin, occurrence)| {
                if active.contains(occurrence)
                    || self.completed_source.as_deref() == Some(origin.entry_id.as_str())
                {
                    covered.insert(*occurrence);
                    Some((origin.entry_id.clone(), origin.item_index))
                } else {
                    None
                }
            })
            .collect();
        for (entry, occurrence) in &self.result_updates {
            if active.contains(occurrence) {
                covered.insert(*occurrence);
                requests.push((entry.clone(), 0));
            }
        }
        let mut complete = active.iter().all(|occurrence| covered.contains(occurrence));
        for chunk in requests.chunks(500) {
            let Ok(results) = store.resolve_live_origins(account, task, chunk) else {
                self.resync_required = true;
                return;
            };
            for result in results {
                complete &= result.known;
                if let Some(row) = result.row {
                    self.commit_row(task, &row);
                } else if result.hidden {
                    let origin = Origin {
                        entry_id: result.entry_id,
                        item_index: result.item_index,
                    };
                    if let Some(&occurrence) = self
                        .origins
                        .get(&origin)
                        .or_else(|| self.result_updates.get(&origin.entry_id))
                    {
                        self.remove_occurrence(occurrence);
                    }
                }
            }
        }
        self.resync_required = !complete;
        if self.items.len() != before_len {
            self.epoch += 1;
        }
    }

    pub(super) fn apply_change(&mut self, task: &str, change: &StoreChange) {
        if change.flipped_epoch.is_some() {
            self.epoch += 1;
            self.commits.clear();
            self.persisted.clear();
            for row in &mut self.items {
                row.persisted = None;
            }
        }
        if !change.rows.is_empty() || !change.hidden_rows.is_empty() {
            self.epoch += 1;
        }
        for row in &change.rows {
            self.commit_row(task, row);
        }
        self.items.retain(|item| {
            !item.persisted.as_ref().is_some_and(|persisted| {
                change.hidden_rows.iter().any(|hidden| {
                    hidden.epoch == persisted.rows_epoch && hidden.row_seq == persisted.row_seq
                })
            })
        });
    }

    fn commit_row(&mut self, task: &str, row: &StoredRowChange) {
        let origin = Origin {
            entry_id: row.origin_entry_id.clone(),
            item_index: row.item_index,
        };
        let commit = Commit {
            identity: AgentPersistedRowIdentity {
                task_id: task.into(),
                rows_epoch: row.epoch,
                row_seq: row.row_seq,
                update_seq: row.update_seq,
            },
            update_entry_id: row.update_entry_id.clone(),
        };
        if let Some(occurrence) = self
            .origins
            .get(&origin)
            .copied()
            .or_else(|| self.result_updates.get(&row.update_entry_id).copied())
        {
            self.origins.insert(origin.clone(), occurrence);
            if self.persisted.get(&occurrence).is_none_or(|old| {
                old.identity != commit.identity || old.update_entry_id != commit.update_entry_id
            }) || self.payload_sources.get(&occurrence) == Some(&row.update_entry_id)
                && self.items.iter().any(|item| {
                    item.live
                        .as_ref()
                        .is_some_and(|live| live.occurrence == occurrence)
                })
            {
                self.epoch += 1;
            }
            self.persisted.insert(occurrence, commit.clone());
            if self.payload_sources.get(&occurrence) == Some(&row.update_entry_id) {
                self.remove_occurrence(occurrence);
            }
        }
        self.commits.insert(origin, commit);
    }

    fn remove_occurrence(&mut self, occurrence: u64) {
        self.items.retain(|item| {
            item.live
                .as_ref()
                .is_none_or(|live| live.occurrence != occurrence)
        });
    }

    fn occurrence(&mut self, group: u64, key: &str) -> u64 {
        *self.groups.entry((group, key.into())).or_insert_with(|| {
            self.next_occurrence += 1;
            self.next_occurrence
        })
    }

    pub(super) fn row(
        &mut self,
        mut incoming: AgentTimelineItem,
        run: &str,
        tool: Option<&str>,
        completed: bool,
    ) -> AgentTimelineItem {
        let group = tool
            .and_then(|tool| self.tools.get(tool).map(|(group, _)| *group))
            .unwrap_or(self.group);
        let occurrence = incoming
            .live
            .as_ref()
            .map(|live| live.occurrence)
            .or_else(|| tool.and_then(|tool| self.tool_occurrences.get(tool).copied()))
            .unwrap_or_else(|| self.occurrence(group, &incoming.id));
        let commit = self.persisted.get(&occurrence).cloned();
        incoming.live = Some(AgentLiveRowIdentity {
            run_id: run.into(),
            occurrence,
        });
        incoming.persisted = commit.as_ref().map(|commit| commit.identity.clone());
        if completed {
            if let Some(source) = &self.completed_source {
                self.payload_sources.insert(occurrence, source.clone());
            }
        } else {
            self.payload_sources.remove(&occurrence);
        }
        let durable = completed
            && commit.as_ref().is_some_and(|commit| {
                Some(&commit.update_entry_id) == self.completed_source.as_ref()
            });
        let mut full = match self.items.iter().find(|item| {
            item.live
                .as_ref()
                .is_some_and(|live| live.occurrence == occurrence)
        }) {
            Some(previous) => {
                let mut full = previous.clone();
                timeline::merge_item(&mut full, incoming.clone());
                full.live = incoming.live;
                full.persisted = incoming.persisted;
                full
            }
            None => incoming,
        };
        full.merge = "replace".into();
        if durable {
            self.remove_occurrence(occurrence);
        } else if let Some(previous) = self.items.iter_mut().find(|item| {
            item.live
                .as_ref()
                .is_some_and(|live| live.occurrence == occurrence)
        }) {
            *previous = full.clone();
        } else {
            self.items.push(full.clone());
        }
        full
    }

    pub(super) fn tool_for_message(message: &SessionMessage) -> Option<&str> {
        if let SessionMessage::Llm(Message::ToolResult(result)) = message {
            Some(&result.tool_call_id)
        } else {
            None
        }
    }
}

struct Counter(usize);
impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn wire_bytes(value: &impl serde::Serialize) -> usize {
    let mut counter = Counter(0);
    if serde_json::to_writer(&mut counter, value).is_err() {
        usize::MAX
    } else {
        counter.0
    }
}

/// Copy only the newest bounded tail. Large values are traversed by reference;
/// no complete giant string/JSON value is cloned merely to truncate it afterward.
pub(super) fn bounded_tail(
    items: &[AgentTimelineItem],
    rows: usize,
    bytes: usize,
) -> (Vec<AgentTimelineItem>, bool) {
    let mut result = Vec::new();
    let mut remaining = bytes.saturating_sub(2);
    let mut more = false;
    for item in items.iter().rev() {
        if result.len() == rows || remaining < 256 {
            more = true;
            break;
        }
        let original = wire_bytes(item);
        if original.saturating_add(1) <= remaining {
            result.push(item.clone());
            remaining = remaining.saturating_sub(original + 1);
        } else if result.is_empty() {
            if let Some(item) = bounded_item(item, remaining.saturating_sub(1), original) {
                result.push(item);
            }
            more = true;
            break;
        } else {
            more = true;
            break;
        }
    }
    result.reverse();
    (result, more)
}

fn prefix(text: &str, bytes: usize) -> String {
    let mut end = text.len().min(bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].into()
}

fn limited_value(value: &Value, budget: &mut usize) -> Value {
    if *budget < 8 {
        return Value::Null;
    }
    *budget -= 8;
    match value {
        Value::String(text) => {
            let text = prefix(text, *budget);
            *budget = budget.saturating_sub(text.len());
            Value::String(text)
        }
        Value::Array(values) => {
            let mut result = Vec::new();
            for value in values {
                if *budget < 8 {
                    break;
                }
                result.push(limited_value(value, budget));
            }
            Value::Array(result)
        }
        Value::Object(values) => {
            let mut result = serde_json::Map::new();
            for (key, value) in values {
                if *budget < 16 {
                    break;
                }
                let key = prefix(key, (*budget / 2).min(128));
                *budget = budget.saturating_sub(key.len());
                result.insert(key, limited_value(value, budget));
            }
            Value::Object(result)
        }
        other => other.clone(),
    }
}

fn bounded_item(
    item: &AgentTimelineItem,
    cap: usize,
    original: usize,
) -> Option<AgentTimelineItem> {
    let mut payload_budget = cap / 8;
    loop {
        let mut budget = payload_budget;
        let mut text = |value: &Option<String>| {
            value.as_ref().map(|value| {
                let value = prefix(value, budget);
                budget = budget.saturating_sub(value.len());
                value
            })
        };
        let title = text(&item.title);
        let text_content = text(&item.text);
        let mut bounded = AgentTimelineItem {
            persisted: item.persisted.clone(),
            live: item.live.clone(),
            truncated: Some(item.truncated.unwrap_or(original).max(original)),
            revision: item.revision,
            id: prefix(&item.id, 128),
            item_type: prefix(&item.item_type, 64),
            role: item.role.as_ref().map(|role| prefix(role, 64)),
            title,
            text: text_content,
            status: item.status.as_ref().map(|status| prefix(status, 64)),
            input: item
                .input
                .as_ref()
                .map(|value| limited_value(value, &mut budget)),
            output: item
                .output
                .as_ref()
                .map(|value| limited_value(value, &mut budget)),
            created_ms: item.created_ms,
            merge: "replace".into(),
        };
        if wire_bytes(&bounded) <= cap {
            return Some(bounded);
        }
        if payload_budget == 0 {
            bounded.input = None;
            bounded.output = None;
            bounded.title = None;
            bounded.text = None;
            return (wire_bytes(&bounded) <= cap).then_some(bounded);
        }
        payload_budget /= 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_coding_agent::session::{EntryKind, SessionEntry};

    fn message(text: &str) -> SessionMessage {
        let mut user = pi_ai::UserMessage::text(text);
        user.timestamp = 7;
        SessionMessage::Llm(Message::User(user))
    }

    fn source(id: &str, message: &SessionMessage) -> SessionEntry {
        SessionEntry {
            id: id.into(),
            parent_id: None,
            timestamp: 7,
            kind: EntryKind::Message {
                message: message.clone(),
            },
        }
    }

    fn append_live(
        live: &mut LiveSnapshot,
        id: &str,
        message: &SessionMessage,
    ) -> AgentTimelineItem {
        live.begin_message();
        live.note_source(SourceReceipt::of(&source(id, message)).unwrap());
        let rows = live.end_message(message);
        live.row(rows[0].clone(), "run", None, true)
    }

    fn commit(id: &str, seq: i64) -> StoreChange {
        StoreChange {
            task: Some("task".into()),
            rows: vec![StoredRowChange {
                epoch: 3,
                row_key: "u7".into(),
                row_seq: seq,
                entry_seq: seq,
                item_index: 0,
                origin_entry_id: id.into(),
                update_entry_id: id.into(),
                update_seq: seq,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn committed_rows_leave_only_the_matching_live_occurrence() {
        let mut live = LiveSnapshot::default();
        let first = append_live(&mut live, "first", &message("same timestamp first"));
        let second = append_live(&mut live, "second", &message("same timestamp second"));
        assert_eq!(first.id, second.id);
        assert_ne!(first.live, second.live);
        assert_eq!(live.items.len(), 2);
        live.apply_change("task", &commit("first", 11));
        assert_eq!(live.items.len(), 1);
        assert_eq!(live.items[0].live, second.live);
        assert_eq!(live.items[0].text.as_deref(), Some("same timestamp second"));
    }

    #[test]
    fn events_for_stored_rows_carry_row_sequence_and_the_live_alias() {
        let mut live = LiveSnapshot::default();
        let message = message("persisted");
        let original = append_live(&mut live, "entry", &message);
        live.apply_change("task", &commit("entry", 29));
        let row = live.row(
            timeline::user_item(7, &[pi_ai::Content::text("persisted")], "replace"),
            "run",
            None,
            true,
        );
        assert_eq!(row.live, original.live);
        assert_eq!(
            row.persisted,
            Some(AgentPersistedRowIdentity {
                task_id: "task".into(),
                rows_epoch: 3,
                row_seq: 29,
                update_seq: 29
            })
        );
        assert!(live.items.is_empty());
    }

    #[test]
    fn failed_append_and_epoch_flip_keep_uncommitted_rows() {
        let mut live = LiveSnapshot::default();
        append_live(&mut live, "unwritten", &message("kept during disk failure"));
        assert_eq!(live.items.len(), 1);
        let before = live.epoch;
        live.apply_change(
            "task",
            &StoreChange {
                task: Some("task".into()),
                flipped_epoch: Some(4),
                ..Default::default()
            },
        );
        assert!(live.epoch > before);
        assert_eq!(
            live.items[0].text.as_deref(),
            Some("kept during disk failure")
        );
        live.apply_change("task", &commit("unwritten", 30));
        assert!(live.items.is_empty());
    }

    #[test]
    fn commit_before_source_binding_still_reconciles_the_exact_origin() {
        let mut live = LiveSnapshot::default();
        live.apply_change("task", &commit("entry", 31));
        let row = append_live(&mut live, "entry", &message("committed before MessageEnd"));
        assert_eq!(row.persisted.unwrap().row_seq, 31);
        assert!(live.items.is_empty());
    }

    #[test]
    fn a_boundary_event_and_adapter_receipt_name_one_source() {
        let mut live = LiveSnapshot::default();
        let message = SessionMessage::Custom(pi_coding_agent::messages::CustomMessage {
            custom_type: "notice".into(),
            content: vec![pi_ai::Content::text("boundary notice")],
            display: true,
            details: None,
            timestamp: 7,
        });
        let entry = source("boundary", &message);
        live.begin_message();
        live.note_source(SourceReceipt::of(&entry).unwrap());
        live.note_source(SourceReceipt::of(&entry).unwrap());
        let rows = live.end_message(&message);
        assert_eq!(rows.len(), 1);
        let row = live.row(rows[0].clone(), "run", None, true);
        assert!(row.persisted.is_none());
        live.apply_change("task", &commit("boundary", 31));
        assert!(live.items.is_empty());
    }

    #[test]
    fn out_of_order_callbacks_keep_other_observed_sources_available() {
        let mut live = LiveSnapshot::default();
        let first = message("first");
        let second = message("second");
        for (id, message) in [("first", &first), ("second", &second)] {
            live.note_source(SourceReceipt::of(&source(id, message)).unwrap());
        }
        for (id, message) in [("second", &second), ("first", &first)] {
            live.begin_message();
            assert_eq!(live.end_message(message).len(), 1);
            assert_eq!(live.completed_source.as_deref(), Some(id));
        }
    }

    #[test]
    fn provisional_and_concurrent_identical_custom_messages_never_guess_an_origin() {
        let message = SessionMessage::Custom(pi_coding_agent::messages::CustomMessage {
            custom_type: "notice".into(),
            content: vec![pi_ai::Content::text("identical")],
            display: true,
            details: None,
            timestamp: 7,
        });
        let mut live = LiveSnapshot::default();
        // Pi has not attempted storage before the first conversation. No leaf
        // lookup or timestamp matching may fabricate a source for this callback.
        live.begin_message();
        assert!(live.end_message(&message).is_empty());
        assert!(live.completed_source.is_none());
        // Two different actual entries can have indistinguishable custom payloads,
        // and their concurrent MessageEnd callbacks need not be emitted in order.
        for id in ["first", "second"] {
            live.note_source(SourceReceipt::of(&source(id, &message)).unwrap());
        }
        assert!(live.end_message(&message).is_empty());
        assert!(live.completed_source.is_none());
        let transient = live.row(
            timeline::notice_item("custom7".into(), "Agent notice", "identical", 7),
            "run",
            None,
            true,
        );
        live.apply_change("task", &commit("first", 31));
        live.apply_change("task", &commit("second", 32));
        assert_eq!(live.items.len(), 1);
        assert_eq!(live.items[0].live, transient.live);
        assert!(live.items[0].persisted.is_none());
    }

    #[test]
    fn a_creation_commit_does_not_remove_newer_live_tool_progress() {
        let mut live = LiveSnapshot::default();
        append_live(&mut live, "entry", &message("initial"));
        live.row(
            timeline::user_item(7, &[pi_ai::Content::text("new progress")], "replace"),
            "run",
            None,
            false,
        );
        live.apply_change("task", &commit("entry", 32));
        assert_eq!(live.items.len(), 1);
        assert_eq!(live.items[0].text.as_deref(), Some("new progress"));
    }

    #[test]
    fn tool_result_in_an_unloaded_prefix_reconciles_by_exact_update_entry() {
        let mut live = LiveSnapshot::default();
        let result = SessionMessage::Llm(Message::ToolResult(pi_ai::ToolResultMessage {
            tool_call_id: "old-call".into(),
            tool_name: "read".into(),
            content: vec![pi_ai::Content::text("result")],
            details: None,
            usage: None,
            is_error: false,
            timestamp: 7,
        }));
        let original = append_live(&mut live, "result-entry", &result);
        let mut change = commit("prefix-assistant", 33);
        change.rows[0].update_entry_id = "result-entry".into();
        change.rows[0].update_seq = 34;
        change.rows[0].item_index = 5;
        live.apply_change("task", &change);
        assert!(live.items.is_empty());
        let final_row = live.row(original.clone(), "run", Some("old-call"), true);
        assert_eq!(final_row.live, original.live);
        assert_eq!(final_row.persisted.unwrap().row_seq, 33);
    }

    #[test]
    fn lagged_broadcast_repairs_committed_origins_and_keeps_unwritten_occurrences() {
        use crate::store::{NewTask, SessionLoad, StorePaths, TaskKind};
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(StorePaths::new(directory.path())).unwrap());
        let account = store.upsert_account("live@example.test", 1).unwrap();
        let task = "00000000-0000-7000-8000-000000000013";
        store
            .create_task(
                account,
                &NewTask {
                    uuid: task.into(),
                    project_root: if cfg!(windows) { "C:/live" } else { "/live" }.into(),
                    kind: TaskKind::Desktop,
                    title: "Live".into(),
                    provisional: false,
                    model: None,
                    created_ms: 1,
                },
            )
            .unwrap();
        let mut manager = store
            .load_session(account, task, SessionLoad::Context)
            .unwrap()
            .into_manager("/live");
        let saved = message("saved");
        let id = manager.append_message(saved.clone());
        manager.flush().unwrap();
        let mut live = LiveSnapshot::default();
        append_live(&mut live, &id, &saved);
        let pending = append_live(&mut live, "unwritten", &message("disk failure"));
        let (sender, receiver) = broadcast::channel(1);
        live.changes = Some(receiver);
        live.resolver = Some((store, account));
        for _ in 0..3 {
            sender.send(StoreChange::default()).unwrap();
        }
        live.drain_changes(task);
        assert!(!live.resync_required);
        assert_eq!(live.items.len(), 1);
        assert_eq!(live.items[0].live, pending.live);
        assert_eq!(live.items[0].text.as_deref(), Some("disk failure"));
    }

    #[test]
    fn bounded_active_pages_never_clone_an_entire_oversized_payload() {
        let mut item = timeline::user_item(7, &[pi_ai::Content::text("large")], "replace");
        item.text = Some("\\\"💚".repeat(200_000));
        item.output =
            Some(serde_json::json!({"values": ["x".repeat(2_000_000), "another large value"]}));
        let (items, more) = bounded_tail(&[item], 10, 4096);
        assert!(more);
        assert_eq!(items.len(), 1);
        assert!(wire_bytes(&items) <= 4096);
        assert!(items[0].truncated.unwrap() > 2_000_000);
        assert!(items[0].text.as_ref().unwrap().len() < 4096);
    }
}
