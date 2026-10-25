//! Incremental, rebuildable history rows. Legacy keys select merge targets;
//! `(task, epoch, row_seq)` identifies an occurrence and never collapses repeats.
//!
//! Source entries and this projector run in the same writer transaction. Only
//! exceptional branch moves/edits inspect earlier entries. Large rebuilds are
//! prepared on the bulk reader and replayed in bounded idle transactions.

use std::collections::{HashMap, HashSet};

use pi_ai::{Message, content_text};
use pi_coding_agent::{
    SessionMessage,
    session::{EntryKind, SessionEntry},
};
use rusqlite::{OptionalExtension, Transaction, params};

use crate::agent::{AgentTimelineItem, timeline};

use super::{
    AccountId, Store, StoreError, StoreResult, codec, session,
    tasks::{self, TaskKey},
};

mod stored;
use stored::StoredItem;

/// Bump whenever the shared timeline builders or their merge rules change.
pub(crate) const TIMELINE_FORMAT: i64 = 2;
const CHUNK_ENTRIES: usize = 1_000;
const CHUNK_ROWS: usize = 1_000;
const CHUNK_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct RowIdentity {
    pub(crate) task: TaskKey,
    pub(crate) epoch: i64,
    pub(crate) row_seq: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct RowOrigin {
    pub(crate) entry_seq: i64,
    pub(crate) item_index: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PersistedRow {
    pub(crate) identity: RowIdentity,
    pub(crate) origin: RowOrigin,
    pub(crate) update_seq: i64,
    /// The bounded stored item. Full reads expand its field references.
    pub(crate) item: AgentTimelineItem,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct ProjectionChange {
    pub(super) rows: Vec<PersistedRow>,
    pub(super) hidden: Vec<RowIdentity>,
    pub(super) history_replaced: Option<i64>,
    pub(super) rebuild_pending: bool,
}

struct RowOp {
    item: AgentTimelineItem,
    call_id: Option<String>,
    merge: bool,
}

pub(super) struct PreparedProjection {
    seq: i64,
    operations: Vec<PreparedSourceOperation>,
}

struct PreparedSourceOperation {
    stored: StoredItem,
    call_id: Option<String>,
    merge: bool,
    result_call: Option<String>,
}

/// Raw-entry fixture formatting. Production projects the persisted image form.
#[cfg(test)]
pub(super) fn prepare_entry(entry: &SessionEntry, seq: i64) -> StoreResult<PreparedProjection> {
    prepare_operations(entry, seq, operations(entry, |_| Ok(None))?)
}

/// Complete source-sized formatting before entering the writer transaction.
/// Image lengths come from metadata collected separately, never file bytes.
pub(super) fn prepare_stored_entry(
    entry: &SessionEntry,
    seq: i64,
    image_sizes: &[usize],
) -> StoreResult<PreparedProjection> {
    let mut rows = operations(entry, |_| Ok(None))?;
    apply_image_sizes(entry, &mut rows, image_sizes)?;
    prepare_operations(entry, seq, rows)
}

fn prepare_operations(
    entry: &SessionEntry,
    seq: i64,
    rows: Vec<RowOp>,
) -> StoreResult<PreparedProjection> {
    if seq < 1 {
        return Err(StoreError::InvalidInput);
    }
    let result_call = match &entry.kind {
        EntryKind::Message {
            message: SessionMessage::Llm(Message::ToolResult(result)),
        } => Some(result.tool_call_id.clone()),
        _ => None,
    };
    let operations = rows
        .into_iter()
        .enumerate()
        .map(|(index, operation)| {
            Ok(PreparedSourceOperation {
                stored: StoredItem::new(&operation.item, seq, index as i64)?,
                call_id: operation.call_id,
                merge: operation.merge,
                result_call: result_call.clone(),
            })
        })
        .collect::<StoreResult<Vec<_>>>()?;
    Ok(PreparedProjection { seq, operations })
}

/// Reuse the canonical builders instead of maintaining a second formatter.
/// `call_key` implements latest-visible-call lookup for the chosen epoch.
fn operations(
    entry: &SessionEntry,
    mut call_key: impl FnMut(&str) -> StoreResult<Option<String>>,
) -> StoreResult<Vec<RowOp>> {
    let insert = |item| RowOp {
        item,
        call_id: None,
        merge: false,
    };
    Ok(match &entry.kind {
        EntryKind::Message {
            message: SessionMessage::Llm(Message::User(user)),
        } => {
            vec![insert(timeline::user_item(
                user.timestamp,
                &user.content,
                "replace",
            ))]
        }
        EntryKind::Message {
            message: SessionMessage::Llm(Message::Assistant(reply)),
        } => {
            let mut calls = reply.tool_calls();
            timeline::reply_items(reply)
                .into_iter()
                .map(|item| {
                    let call_id = if item.item_type == "tool" {
                        calls.next().map(|call| call.id.clone())
                    } else {
                        None
                    };
                    RowOp {
                        item,
                        call_id,
                        merge: false,
                    }
                })
                .collect()
        }
        EntryKind::Message {
            message: SessionMessage::Llm(Message::ToolResult(result)),
        } => {
            let key = call_key(&result.tool_call_id)?
                .unwrap_or_else(|| format!("tool-{}", result.tool_call_id));
            vec![RowOp {
                item: timeline::tool_result_item(
                    key,
                    &result.content,
                    result.details.as_ref(),
                    result.is_error,
                    result.timestamp,
                ),
                call_id: None,
                merge: true,
            }]
        }
        EntryKind::Message {
            message: SessionMessage::Custom(custom),
        } if custom.display => {
            vec![insert(timeline::notice_item(
                format!("custom{}", custom.timestamp),
                "Agent notice",
                &content_text(&custom.content),
                custom.timestamp,
            ))]
        }
        EntryKind::Custom {
            custom_type,
            data: Some(data),
        } if custom_type == timeline::MAPLE_NOTICE_ENTRY => {
            match (
                data.get("id").and_then(serde_json::Value::as_str),
                data.get("text").and_then(serde_json::Value::as_str),
            ) {
                (Some(id), Some(text)) => vec![insert(timeline::notice_item(
                    id.into(),
                    "Agent notice",
                    text,
                    entry.timestamp,
                ))],
                _ => Vec::new(),
            }
        }
        EntryKind::Custom {
            custom_type,
            data: Some(data),
        } if timeline::is_external_turn_end(custom_type) => {
            timeline::external_turn_end_row(data, entry.timestamp)
                .map(|item| RowOp {
                    item,
                    call_id: None,
                    merge: true,
                })
                .into_iter()
                .collect()
        }
        EntryKind::Compaction { .. } => vec![insert(timeline::notice_item(
            format!("compaction-{}", entry.id),
            "Context compacted",
            "Earlier messages were summarized to make room in the context.",
            entry.timestamp,
        ))],
        EntryKind::BranchSummary { summary, .. } if !summary.is_empty() => {
            vec![insert(timeline::notice_item(
                format!("branch-summary-{}", entry.id),
                "Branch summary",
                summary,
                entry.timestamp,
            ))]
        }
        _ => Vec::new(),
    })
}

// Image payloads in stored entries are references. Timeline rows expose only
// their length, so append and rebuild use metadata without loading image bytes.
fn operations_tx(
    tx: &Transaction<'_>,
    task: TaskKey,
    entry: &SessionEntry,
    call_key: impl FnMut(&str) -> StoreResult<Option<String>>,
) -> StoreResult<Vec<RowOp>> {
    let mut rows = operations(entry, call_key)?;
    if matches!(
        &entry.kind,
        EntryKind::Message {
            message: SessionMessage::Llm(Message::ToolResult(_))
        }
    ) {
        let sizes = super::attachments::entry_image_sizes_tx(tx, task, entry)?;
        apply_image_sizes(entry, &mut rows, &sizes)?;
    }
    Ok(rows)
}

fn apply_image_sizes(entry: &SessionEntry, rows: &mut [RowOp], sizes: &[usize]) -> StoreResult<()> {
    if matches!(
        &entry.kind,
        EntryKind::Message {
            message: SessionMessage::Llm(Message::ToolResult(_))
        }
    ) {
        let mut sizes = sizes.iter();
        for row in rows {
            if let Some(content) = row
                .item
                .output
                .as_mut()
                .and_then(|output| output.get_mut("content"))
                .and_then(serde_json::Value::as_array_mut)
            {
                for block in content {
                    if block.get("type").and_then(serde_json::Value::as_str) == Some("image") {
                        block["base64Chars"] =
                            serde_json::json!(sizes.next().ok_or(StoreError::InvalidData)?);
                    }
                }
            }
        }
        if sizes.next().is_some() {
            return Err(StoreError::InvalidData);
        }
    }
    Ok(())
}

/// The UI's todo predicate, shared with persisted plan-page selection.
pub(crate) fn is_plan_row(item: &AgentTimelineItem) -> bool {
    matches!(item.item_type.as_str(), "tool" | "toolCall")
        && item
            .input
            .as_ref()
            .and_then(|input| input.get("todos"))
            .is_some_and(serde_json::Value::is_array)
}

pub(super) fn append_prepared(
    tx: &Transaction<'_>,
    task: TaskKey,
    seq: i64,
    entry: &SessionEntry,
    prepared: &PreparedProjection,
) -> StoreResult<ProjectionChange> {
    if seq < 1 || prepared.seq != seq {
        return Err(StoreError::InvalidInput);
    }
    let (epoch, mut last_row, mut pending, format): (i64, i64, Option<i64>, i64) = tx.query_row(
        "SELECT rows_epoch,last_row_seq,rebuild_epoch,timeline_format FROM tasks WHERE id=?1 AND deleted_ms IS NULL", [task.0], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
    )?;
    let previous: Option<String> = tx
        .query_row(
            "SELECT entry_id FROM entries WHERE task_id=?1 AND seq=?2",
            params![task.0, seq - 1],
            |row| row.get(0),
        )
        .optional()?;
    let branch_move = entry.parent_id != previous;
    if format != TIMELINE_FORMAT && seq > 1 && pending.is_none() {
        pending = Some(schedule_rebuild(tx, task, epoch)?);
    }
    let mut change = ProjectionChange::default();
    if !branch_move
        && let EntryKind::ContextEdit {
            target_id,
            replacement: None,
        } = &entry.kind
        && simple_hide(tx, task, epoch, seq, target_id, &mut change)?
    {
        if pending.is_some() {
            schedule_rebuild(tx, task, epoch)?;
        }
        change.rebuild_pending = pending.is_some();
        return Ok(change);
    }
    let needs_rebuild = branch_move
        || matches!(
            &entry.kind,
            EntryKind::ContextEdit {
                replacement: None,
                ..
            }
        );
    if needs_rebuild {
        if let Some(inline) = inline_rebuild(tx, task, seq, epoch)? {
            return Ok(inline);
        }
        schedule_rebuild(tx, task, epoch)?;
        change.rebuild_pending = true;
    }
    // Pending rebuilds keep the current epoch usable. The replacement epoch is
    // built from the authoritative branch and eventually replaces these rows.
    for (index, operation) in prepared.operations.iter().enumerate() {
        let mut incoming = operation.stored.clone();
        if let Some(call) = &operation.result_call {
            incoming.set_row_key(
                latest_call(tx, task, epoch, call)?.unwrap_or_else(|| format!("tool-{call}")),
            );
        }
        let (row, _) = apply_stored(
            tx,
            task,
            epoch,
            &mut last_row,
            seq,
            index as i64,
            incoming,
            operation.call_id.as_deref(),
            operation.merge,
        )?;
        change.rows.push(row);
    }
    tx.execute("UPDATE tasks SET last_row_seq=?2,timeline_format=CASE WHEN rebuild_epoch IS NULL THEN ?3 ELSE timeline_format END WHERE id=?1",params![task.0,last_row,TIMELINE_FORMAT])?;
    change.rebuild_pending |= pending.is_some();
    Ok(change)
}

/// Fixture convenience. Production session adapters prepare before the writer.
#[cfg(test)]
pub(super) fn append(
    tx: &Transaction<'_>,
    task: TaskKey,
    seq: i64,
    entry: &SessionEntry,
) -> StoreResult<ProjectionChange> {
    let prepared = prepare_entry(entry, seq)?;
    append_prepared(tx, task, seq, entry, &prepared)
}

fn latest_call(
    tx: &Transaction<'_>,
    task: TaskKey,
    epoch: i64,
    call: &str,
) -> StoreResult<Option<String>> {
    Ok(tx.query_row("SELECT row_key FROM timeline_rows WHERE task_id=?1 AND epoch=?2 AND call_id=?3 AND hidden=0 ORDER BY row_seq DESC LIMIT 1",params![task.0,epoch,call],|row|row.get(0)).optional()?)
}

#[allow(clippy::too_many_arguments)]
fn apply_stored(
    tx: &Transaction<'_>,
    task: TaskKey,
    epoch: i64,
    last_row: &mut i64,
    seq: i64,
    index: i64,
    incoming: StoredItem,
    call_id: Option<&str>,
    merge: bool,
) -> StoreResult<(PersistedRow, usize)> {
    let target: Option<(i64, i64, i64, i64, i64, Vec<u8>)> = if merge {
        tx.query_row("SELECT id,row_seq,entry_seq,item_index,codec,item FROM timeline_rows WHERE task_id=?1 AND epoch=?2 AND row_key=?3 AND hidden=0 ORDER BY row_seq LIMIT 1",params![task.0,epoch,incoming.item().id],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?))).optional()?
    } else {
        None
    };
    if let Some((id, row_seq, entry_seq, item_index, item_codec, payload)) = target {
        let mut stored = StoredItem::decode(&codec::decode(item_codec, &payload)?)?;
        stored.merge(incoming)?;
        let raw = stored.encode()?;
        let (item_codec, payload) = codec::encode(&raw)?;
        tx.execute("UPDATE timeline_rows SET update_seq=?2,is_plan=?3,codec=?4,bytes=?5,item=?6 WHERE id=?1",params![id,seq,is_plan_row(stored.item()),item_codec,payload.len() as i64,payload])?;
        Ok((
            PersistedRow {
                identity: RowIdentity {
                    task,
                    epoch,
                    row_seq,
                },
                origin: RowOrigin {
                    entry_seq,
                    item_index,
                },
                update_seq: seq,
                item: stored.bounded_item()?,
            },
            raw.len(),
        ))
    } else {
        *last_row = last_row.checked_add(1).ok_or(StoreError::InvalidData)?;
        let bytes = insert_row(
            tx, task, epoch, *last_row, seq, index, seq, call_id, &incoming,
        )?;
        Ok((
            PersistedRow {
                identity: RowIdentity {
                    task,
                    epoch,
                    row_seq: *last_row,
                },
                origin: RowOrigin {
                    entry_seq: seq,
                    item_index: index,
                },
                update_seq: seq,
                item: incoming.bounded_item()?,
            },
            bytes,
        ))
    }
}

#[allow(clippy::too_many_arguments)]
fn insert_row(
    tx: &Transaction<'_>,
    task: TaskKey,
    epoch: i64,
    row_seq: i64,
    entry_seq: i64,
    item_index: i64,
    update_seq: i64,
    call: Option<&str>,
    stored: &StoredItem,
) -> StoreResult<usize> {
    let raw = stored.encode()?;
    let (item_codec, payload) = codec::encode(&raw)?;
    tx.execute("INSERT INTO timeline_rows(task_id,epoch,row_seq,row_key,entry_seq,item_index,update_seq,call_id,hidden,is_plan,codec,bytes,item) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,0,?9,?10,?11,?12)",params![task.0,epoch,row_seq,stored.item().id,entry_seq,item_index,update_seq,call,is_plan_row(stored.item()),item_codec,payload.len() as i64,payload])?;
    Ok(raw.len())
}

fn simple_hide(
    tx: &Transaction<'_>,
    task: TaskKey,
    epoch: i64,
    seq: i64,
    target: &str,
    change: &mut ProjectionChange,
) -> StoreResult<bool> {
    let target: Option<(i64, i64, i64)> = tx
        .query_row(
            "SELECT seq,codec,length(payload) FROM entries WHERE task_id=?1 AND entry_id=?2",
            params![task.0, target],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((target_seq, entry_codec, bytes)) = target else {
        return Ok(false);
    };
    // A compressed payload can expand far beyond the writer budget. Its safe
    // hide classification runs through the bulk-prepared rebuild instead.
    if entry_codec != 0 || bytes > CHUNK_BYTES as i64 {
        return Ok(false);
    }
    let payload: Vec<u8> = tx.query_row(
        "SELECT payload FROM entries WHERE task_id=?1 AND seq=?2",
        params![task.0, target_seq],
        |row| row.get(0),
    )?;
    let target: SessionEntry = serde_json::from_slice(&codec::decode(entry_codec, &payload)?)
        .map_err(|_| StoreError::InvalidData)?;
    if !matches!(
        &target.kind,
        EntryKind::Message {
            message: SessionMessage::Llm(Message::User(_) | Message::Assistant(_))
        }
    ) {
        return Ok(false);
    }
    // Omitting a call can retarget later results, including results merged into
    // an earlier occurrence with the same legacy row key. A check of this
    // entry's own rows cannot prove that those other rows are unaffected.
    if let EntryKind::Message {
        message: SessionMessage::Llm(Message::Assistant(reply)),
    } = &target.kind
        && reply.tool_calls().next().is_some()
    {
        return Ok(false);
    }
    let modified:i64=tx.query_row("SELECT count(*) FROM timeline_rows WHERE task_id=?1 AND epoch=?2 AND entry_seq=?3 AND update_seq<>?3 AND hidden=0",params![task.0,epoch,target_seq],|row|row.get(0))?;
    if modified != 0 {
        return Ok(false);
    }
    let mut statement=tx.prepare_cached("SELECT row_seq FROM timeline_rows WHERE task_id=?1 AND epoch=?2 AND entry_seq=?3 AND hidden=0 ORDER BY row_seq")?;
    let hidden = statement
        .query_map(params![task.0, epoch, target_seq], |row| {
            row.get::<_, i64>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    change
        .hidden
        .extend(hidden.into_iter().map(|row_seq| RowIdentity {
            task,
            epoch,
            row_seq,
        }));
    tx.execute("UPDATE timeline_rows SET hidden=1,update_seq=?4 WHERE task_id=?1 AND epoch=?2 AND entry_seq=?3 AND hidden=0",params![task.0,epoch,target_seq,seq])?;
    Ok(true)
}

fn schedule_rebuild(tx: &Transaction<'_>, task: TaskKey, current_epoch: i64) -> StoreResult<i64> {
    // Never reuse an abandoned shadow epoch: a restart is constant-time and old
    // rows can be collected without blocking the triggering foreground append.
    let maximum: i64 = tx.query_row(
        "SELECT coalesce(max(epoch),?2) FROM timeline_rows WHERE task_id=?1",
        params![task.0, current_epoch],
        |row| row.get(0),
    )?;
    let epoch = maximum
        .max(current_epoch)
        .checked_add(1)
        .ok_or(StoreError::InvalidData)?;
    tx.execute(
        "UPDATE tasks SET rebuild_epoch=?2,rebuild_seq=0,rebuild_item_index=0 WHERE id=?1",
        params![task.0, epoch],
    )?;
    Ok(epoch)
}

struct BuiltRow {
    stored: StoredItem,
    origin: RowOrigin,
    update_seq: i64,
    call_id: Option<String>,
}

fn branch_entries(entries: Vec<(i64, SessionEntry)>) -> StoreResult<Vec<(i64, SessionEntry)>> {
    let index: HashMap<String, usize> = entries
        .iter()
        .enumerate()
        .map(|(i, (_, entry))| (entry.id.clone(), i))
        .collect();
    let mut path = Vec::new();
    let mut next = entries.last().map(|(_, entry)| entry.id.as_str());
    let mut seen = HashSet::new();
    while let Some(id) = next {
        if !seen.insert(id) {
            return Err(StoreError::InvalidData);
        }
        let &position = index.get(id).ok_or(StoreError::InvalidData)?;
        path.push(position);
        next = entries[position].1.parent_id.as_deref();
    }
    path.reverse();
    let omitted: HashSet<&str> = path
        .iter()
        .filter_map(|&position| match &entries[position].1.kind {
            EntryKind::ContextEdit {
                target_id,
                replacement: None,
            } => Some(target_id.as_str()),
            _ => None,
        })
        .collect();
    Ok(path
        .into_iter()
        .filter(|&position| !omitted.contains(entries[position].1.id.as_str()))
        .map(|position| entries[position].clone())
        .collect())
}

fn build_rows(
    tx: &Transaction<'_>,
    task: TaskKey,
    entries: &[(i64, SessionEntry)],
) -> StoreResult<Vec<BuiltRow>> {
    let mut rows: Vec<BuiltRow> = Vec::new();
    let mut first: HashMap<String, usize> = HashMap::new();
    let mut calls: HashMap<String, String> = HashMap::new();
    for (seq, entry) in entries {
        for (index, operation) in
            operations_tx(tx, task, entry, |call| Ok(calls.get(call).cloned()))?
                .into_iter()
                .enumerate()
        {
            if let Some(call) = &operation.call_id {
                calls.insert(call.clone(), operation.item.id.clone());
            }
            let incoming = StoredItem::new(&operation.item, *seq, index as i64)?;
            if operation.merge
                && let Some(&position) = first.get(&operation.item.id)
            {
                rows[position].stored.merge(incoming)?;
                rows[position].update_seq = *seq;
                continue;
            }
            first.entry(operation.item.id.clone()).or_insert(rows.len());
            rows.push(BuiltRow {
                stored: incoming,
                origin: RowOrigin {
                    entry_seq: *seq,
                    item_index: index as i64,
                },
                update_seq: *seq,
                call_id: operation.call_id,
            });
        }
    }
    Ok(rows)
}

fn inline_rebuild(
    tx: &Transaction<'_>,
    task: TaskKey,
    last_seq: i64,
    current_epoch: i64,
) -> StoreResult<Option<ProjectionChange>> {
    let (count,bytes,compressed):(i64,i64,i64)=tx.query_row("SELECT count(*),coalesce(sum(bytes),0),coalesce(sum(codec<>0),0) FROM entries WHERE task_id=?1",[task.0],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
    // Stored bytes alone do not bound decompression. Conservatively defer any
    // compressed source to the bulk reader, even when its visible rows are small.
    if count > CHUNK_ENTRIES as i64 || bytes > CHUNK_BYTES as i64 || compressed > 0 {
        return Ok(None);
    }
    let entries = branch_entries(session::read_entries_from(tx, task, 1)?)?;
    let rows = build_rows(tx, task, &entries)?;
    if rows.len() > CHUNK_ROWS
        || rows.iter().try_fold(0usize, |sum, row| {
            Ok::<_, StoreError>(sum.saturating_add(row.stored.encode()?.len()))
        })? > CHUNK_BYTES
    {
        return Ok(None);
    }
    let epoch = schedule_rebuild(tx, task, current_epoch)?;
    for (index, row) in rows.iter().enumerate() {
        insert_row(
            tx,
            task,
            epoch,
            index as i64 + 1,
            row.origin.entry_seq,
            row.origin.item_index,
            row.update_seq,
            row.call_id.as_deref(),
            &row.stored,
        )?;
    }
    flip(tx, task, epoch, last_seq, rows.len() as i64)?;
    Ok(Some(ProjectionChange {
        history_replaced: Some(epoch),
        ..ProjectionChange::default()
    }))
}

fn flip(
    tx: &Transaction<'_>,
    task: TaskKey,
    epoch: i64,
    last_seq: i64,
    last_row: i64,
) -> StoreResult<()> {
    let changed=tx.execute("UPDATE tasks SET rows_epoch=?2,last_row_seq=?3,timeline_format=?4,rebuild_epoch=NULL,rebuild_seq=NULL,rebuild_item_index=0 WHERE id=?1 AND rebuild_epoch=?2 AND last_seq<=?5",params![task.0,epoch,last_row,TIMELINE_FORMAT,last_seq])?;
    if changed != 1 {
        return Err(StoreError::Conflict);
    }
    Ok(())
}

/// Only compact branch metadata survives the source prepass. Pi's eight-hex
/// ids are represented as u32s while resolving the branch; after that only
/// sequence numbers and omitted bits remain. Beyond the 64 MiB metadata budget
/// the slower branch-walk fallback keeps the same bounded payload memory.
const METADATA_BYTES: usize = 64 * 1024 * 1024;
const METADATA_ENTRY_BUDGET: usize = 128;
const SOURCE_BATCH_BYTES: usize = 4 * 1024 * 1024;
const SOURCE_BATCH_ENTRIES: usize = 128;

#[derive(Clone)]
pub(crate) struct RebuildTicket {
    task: TaskKey,
    account: AccountId,
    uuid: String,
    epoch: i64,
}

#[derive(Clone, Copy)]
struct OutlineEntry {
    seq: i64,
    omitted: bool,
}

enum Outline {
    Compact(Vec<OutlineEntry>),
    Rescan,
}

pub(crate) struct PreparedRebuild {
    ticket: RebuildTicket,
    last_seq: i64,
    outline: Outline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RebuildProgress {
    Pending { entries: usize, bytes: usize },
    NeedsSnapshot,
    Replaced { epoch: i64 },
}

#[derive(Clone)]
struct PreparedWrite {
    row_seq: i64,
    origin: RowOrigin,
    update_seq: i64,
    row_key: String,
    call_id: Option<String>,
    is_plan: bool,
    codec: i64,
    payload: Vec<u8>,
}

/// Fully formatted, reduced and encoded writes. Committing this object performs
/// no source decoding, item formatting, JSON serialization or compression.
pub(crate) struct PreparedChunk {
    ticket: RebuildTicket,
    source_last_seq: i64,
    expected_seq: i64,
    expected_index: usize,
    expected_last_row: i64,
    through: i64,
    next_index: usize,
    last_row: i64,
    complete: bool,
    entries: usize,
    bytes: usize,
    writes: Vec<PreparedWrite>,
}

struct WorkingRow {
    row_seq: i64,
    origin: RowOrigin,
    call_id: Option<String>,
    stored: StoredItem,
}

impl Store {
    pub(crate) fn timeline_rebuild_queue(
        &self,
        account: AccountId,
        limit: u16,
    ) -> StoreResult<Vec<String>> {
        if limit == 0 || limit > 200 {
            return Err(StoreError::InvalidInput);
        }
        self.read(|tx| {
            let mut statement=tx.prepare_cached("SELECT t.uuid FROM tasks t JOIN accounts a ON a.id=t.account_id WHERE t.account_id=?1 AND a.deleting_ms IS NULL AND t.deleted_ms IS NULL AND t.last_seq>0 AND (t.rebuild_epoch IS NOT NULL OR t.timeline_format<>?2) ORDER BY t.updated_ms DESC,t.id DESC LIMIT ?3")?;
            Ok(statement.query_map(params![account.0,TIMELINE_FORMAT,limit],|row|row.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Cursor-based discovery also finds epochs orphaned by a crash after flip.
    pub(crate) fn old_timeline_queue(
        &self,
        account: AccountId,
        limit: u16,
        after: Option<i64>,
    ) -> StoreResult<Vec<(i64, String)>> {
        if limit == 0 || limit > 200 {
            return Err(StoreError::InvalidInput);
        }
        self.read(|tx| {
            let mut statement=tx.prepare_cached("SELECT t.id,t.uuid FROM tasks t JOIN accounts a ON a.id=t.account_id WHERE t.account_id=?1 AND a.deleting_ms IS NULL AND t.deleted_ms IS NULL AND t.id>?2 AND EXISTS(SELECT 1 FROM timeline_rows r WHERE r.task_id=t.id AND r.epoch<>t.rows_epoch AND (t.rebuild_epoch IS NULL OR r.epoch<>t.rebuild_epoch)) ORDER BY t.id LIMIT ?3")?;
            Ok(statement.query_map(params![account.0,after.unwrap_or(0),limit],|row|Ok((row.get(0)?,row.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub(crate) fn collect_old_timeline_chunk(
        &self,
        account: AccountId,
        uuid: &str,
    ) -> StoreResult<usize> {
        self.collect_old_timeline_chunk_with_bytes(account, uuid)
            .map(|(rows, _)| rows)
    }

    pub(super) fn collect_old_timeline_chunk_with_bytes(
        &self,
        account: AccountId,
        uuid: &str,
    ) -> StoreResult<(usize, usize)> {
        self.write_idle(|tx| {
            tasks::writable_account(tx,account)?;
            let task=tasks::task_key_tx(tx,account,uuid)?;
            let mut statement=tx.prepare_cached("SELECT r.id,r.bytes FROM timeline_rows r JOIN tasks t ON t.id=r.task_id WHERE r.task_id=?1 AND r.epoch<>t.rows_epoch AND (t.rebuild_epoch IS NULL OR r.epoch<>t.rebuild_epoch) ORDER BY r.epoch,r.row_seq LIMIT ?2")?;
            let candidates=statement.query_map(params![task.0,CHUNK_ROWS as i64],|row|Ok((row.get::<_,i64>(0)?,row.get::<_,i64>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            let mut bytes=0usize;
            let mut count=0;
            for (id,size) in candidates {
                let size=usize::try_from(size).map_err(|_|StoreError::InvalidData)?;
                if count>0 && bytes.saturating_add(size)>CHUNK_BYTES {break;}
                tx.execute("DELETE FROM timeline_rows WHERE id=?1",[id])?;
                bytes=bytes.saturating_add(size);
                count+=1;
                if bytes>=CHUNK_BYTES {break;}
            }
            Ok((count, bytes))
        })
    }

    /// Queue only. The host holds run admission while invoking this short write.
    pub(crate) fn begin_timeline_rebuild(
        &self,
        account: AccountId,
        uuid: &str,
        active_run: bool,
    ) -> StoreResult<RebuildTicket> {
        if active_run {
            return Err(StoreError::Busy);
        }
        self.write_idle(|tx| {
            tasks::writable_account(tx, account)?;
            let task = tasks::task_key_tx(tx, account, uuid)?;
            let (current, pending): (i64, Option<i64>) = tx.query_row(
                "SELECT rows_epoch,rebuild_epoch FROM tasks WHERE id=?1",
                [task.0],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let epoch = match pending {
                Some(epoch) => epoch,
                None => schedule_rebuild(tx, task, current)?,
            };
            Ok(RebuildTicket {
                task,
                account,
                uuid: uuid.to_owned(),
                epoch,
            })
        })
    }

    /// Read-only source prepass outside run admission and the writer mutex.
    /// Every read releases its snapshot after <=128 source payloads/4 MiB;
    /// one oversized source entry is the explicit indivisible exception.
    pub(crate) fn prepare_timeline_rebuild(
        &self,
        ticket: &RebuildTicket,
    ) -> StoreResult<PreparedRebuild> {
        let last_seq = self.read_bulk(|tx| {
            authorize_ticket(tx, ticket)?;
            Ok(tx.query_row(
                "SELECT last_seq FROM tasks WHERE id=?1",
                [ticket.task.0],
                |row| row.get(0),
            )?)
        })?;
        let mut metadata = Vec::new();
        let mut after = 0;
        let mut compact = true;
        while after < last_seq {
            let entries =
                self.read_bulk(|tx| source_metadata_batch(tx, ticket.task, after, last_seq))?;
            if entries.is_empty() {
                return Err(StoreError::InvalidData);
            }
            for (seq, meta) in entries {
                after = seq;
                if metadata.len() >= METADATA_BYTES / METADATA_ENTRY_BUDGET {
                    compact = false;
                    break;
                }
                let Some(meta) = meta else {
                    compact = false;
                    break;
                };
                metadata.push(meta);
            }
            if !compact {
                break;
            }
        }
        let outline = if compact {
            Outline::Compact(resolve_outline(metadata)?)
        } else {
            Outline::Rescan
        };
        Ok(PreparedRebuild {
            ticket: ticket.clone(),
            last_seq,
            outline,
        })
    }

    /// Prepare at most one chunk on the bulk reader. Only reduced, encoded row
    /// writes cross into the subsequent writer transaction.
    pub(crate) fn prepare_timeline_chunk(
        &self,
        prepared: &PreparedRebuild,
    ) -> StoreResult<PreparedChunk> {
        let cursor = self.read_bulk(|tx| {
            authorize_ticket(tx, &prepared.ticket)?;
            rebuild_cursor(tx, &prepared.ticket)
        })?;
        let (selected, at_end) = match &prepared.outline {
            Outline::Compact(outline) => {
                let start = outline.partition_point(|entry| entry.seq <= cursor.0);
                let end = (start + CHUNK_ENTRIES).min(outline.len());
                (outline[start..end].to_vec(), end == outline.len())
            }
            Outline::Rescan => self.rescan_outline_chunk(prepared, cursor.0)?,
        };
        self.read_bulk(|tx| {
            authorize_ticket(tx, &prepared.ticket)?;
            if rebuild_cursor(tx, &prepared.ticket)? != cursor {
                return Err(StoreError::Conflict);
            }
            prepare_chunk(tx, prepared, cursor, &selected, at_end)
        })
    }

    /// Final admission check belongs to the host immediately around this call.
    /// This writer section executes already prepared SQL bindings only.
    pub(crate) fn commit_timeline_chunk(
        &self,
        chunk: &PreparedChunk,
        active_run: bool,
    ) -> StoreResult<RebuildProgress> {
        if active_run {
            return Err(StoreError::Busy);
        }
        self.write_idle_with_change(|tx| commit_chunk(tx, chunk))
    }

    fn walk_source_branch(
        &self,
        prepared: &PreparedRebuild,
        mut visit: impl FnMut(i64, &SessionEntry) -> StoreResult<()>,
    ) -> StoreResult<()> {
        let mut seq = prepared.last_seq;
        while seq > 0 {
            let entry = self.read_bulk(|tx| session::read_entry(tx, prepared.ticket.task, seq))?;
            visit(seq, &entry)?;
            seq = match entry.parent_id {
                Some(parent) => {
                    let parent_seq: i64 = self.read_bulk(|tx| {
                        Ok(tx.query_row(
                            "SELECT seq FROM entries WHERE task_id=?1 AND entry_id=?2",
                            params![prepared.ticket.task.0, parent],
                            |row| row.get(0),
                        )?)
                    })?;
                    if parent_seq >= seq {
                        return Err(StoreError::InvalidData);
                    }
                    parent_seq
                }
                None => 0,
            };
        }
        Ok(())
    }

    /// Rare >64 MiB metadata/noncanonical-id fallback: two bounded branch
    /// walks select the earliest remaining entries and then mark their edits.
    fn rescan_outline_chunk(
        &self,
        prepared: &PreparedRebuild,
        through: i64,
    ) -> StoreResult<(Vec<OutlineEntry>, bool)> {
        use std::collections::VecDeque;
        let mut selected = VecDeque::<(i64, String)>::new();
        let mut count = 0usize;
        let mut id_bytes = 0usize;
        self.walk_source_branch(prepared, |seq, entry| {
            if seq > through {
                count = count.saturating_add(1);
                id_bytes = id_bytes.saturating_add(entry.id.len());
                selected.push_front((seq, entry.id.clone()));
                while selected.len() > CHUNK_ENTRIES
                    || (id_bytes > METADATA_BYTES / 4 && selected.len() > 1)
                {
                    if let Some((_, id)) = selected.pop_back() {
                        id_bytes = id_bytes.saturating_sub(id.len());
                    }
                }
            }
            Ok(())
        })?;
        let at_end = count == selected.len();
        let candidates: HashMap<String, usize> = selected
            .iter()
            .enumerate()
            .map(|(index, (_, id))| (id.clone(), index))
            .collect();
        let mut entries: Vec<OutlineEntry> = selected
            .into_iter()
            .map(|(seq, _)| OutlineEntry {
                seq,
                omitted: false,
            })
            .collect();
        self.walk_source_branch(prepared, |_, entry| {
            if let EntryKind::ContextEdit {
                target_id,
                replacement: None,
            } = &entry.kind
                && let Some(&index) = candidates.get(target_id)
            {
                entries[index].omitted = true;
            }
            Ok(())
        })?;
        Ok((entries, at_end))
    }
}

struct CompactMeta {
    seq: i64,
    id: u32,
    parent: Option<u32>,
    omits: Option<u32>,
}

fn compact_id(value: &str) -> Option<u32> {
    if value.len() == 8
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        u32::from_str_radix(value, 16).ok()
    } else {
        None
    }
}

impl CompactMeta {
    fn from_entry(seq: i64, entry: &SessionEntry) -> Option<Self> {
        let parent = match entry.parent_id.as_deref() {
            Some(parent) => Some(compact_id(parent)?),
            None => None,
        };
        let omits = match &entry.kind {
            EntryKind::ContextEdit {
                target_id,
                replacement: None,
            } => Some(compact_id(target_id)?),
            _ => None,
        };
        Some(Self {
            seq,
            id: compact_id(&entry.id)?,
            parent,
            omits,
        })
    }
}

fn resolve_outline(metadata: Vec<CompactMeta>) -> StoreResult<Vec<OutlineEntry>> {
    let index: HashMap<u32, usize> = metadata
        .iter()
        .enumerate()
        .map(|(position, entry)| (entry.id, position))
        .collect();
    let mut path: Vec<usize> = Vec::new();
    let mut next = metadata.last().map(|entry| entry.id);
    while let Some(id) = next {
        let &position = index.get(&id).ok_or(StoreError::InvalidData)?;
        if path.len() >= metadata.len() {
            return Err(StoreError::InvalidData);
        }
        if let Some(&child) = path.last()
            && metadata[position].seq >= metadata[child].seq
        {
            return Err(StoreError::InvalidData);
        }
        path.push(position);
        next = metadata[position].parent;
    }
    let omitted: HashSet<u32> = path
        .iter()
        .filter_map(|&position| metadata[position].omits)
        .collect();
    path.reverse();
    Ok(path
        .into_iter()
        .map(|position| OutlineEntry {
            seq: metadata[position].seq,
            omitted: omitted.contains(&metadata[position].id),
        })
        .collect())
}

fn source_metadata_batch(
    tx: &Transaction<'_>,
    task: TaskKey,
    after: i64,
    through: i64,
) -> StoreResult<Vec<(i64, Option<CompactMeta>)>> {
    let mut statement=tx.prepare_cached("SELECT seq,length(payload) FROM entries WHERE task_id=?1 AND seq>?2 AND seq<=?3 ORDER BY seq LIMIT ?4")?;
    let metadata = statement
        .query_map(
            params![task.0, after, through, SOURCE_BATCH_ENTRIES as i64],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    let mut entries = Vec::new();
    let mut bytes = 0usize;
    for (seq, size) in metadata {
        let size = usize::try_from(size).map_err(|_| StoreError::InvalidData)?;
        if !entries.is_empty() && bytes.saturating_add(size) > SOURCE_BATCH_BYTES {
            break;
        }
        bytes = bytes.saturating_add(size);
        let entry = session::read_entry(tx, task, seq)?;
        entries.push((seq, CompactMeta::from_entry(seq, &entry)));
        if bytes >= SOURCE_BATCH_BYTES {
            break;
        }
    }
    Ok(entries)
}

fn authorize_ticket(tx: &Transaction<'_>, ticket: &RebuildTicket) -> StoreResult<()> {
    tasks::writable_account(tx, ticket.account)?;
    if tasks::task_key_tx(tx, ticket.account, &ticket.uuid)? != ticket.task {
        return Err(StoreError::NotFound);
    }
    let epoch: Option<i64> = tx.query_row(
        "SELECT rebuild_epoch FROM tasks WHERE id=?1",
        [ticket.task.0],
        |row| row.get(0),
    )?;
    if epoch != Some(ticket.epoch) {
        return Err(StoreError::Conflict);
    }
    Ok(())
}

fn rebuild_cursor(tx: &Transaction<'_>, ticket: &RebuildTicket) -> StoreResult<(i64, usize, i64)> {
    let (seq, index): (Option<i64>, i64) = tx.query_row(
        "SELECT rebuild_seq,rebuild_item_index FROM tasks WHERE id=?1",
        [ticket.task.0],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let last_row = tx.query_row(
        "SELECT coalesce(max(row_seq),0) FROM timeline_rows WHERE task_id=?1 AND epoch=?2",
        params![ticket.task.0, ticket.epoch],
        |row| row.get(0),
    )?;
    Ok((
        seq.unwrap_or(0),
        usize::try_from(index).map_err(|_| StoreError::InvalidData)?,
        last_row,
    ))
}

fn prepare_chunk(
    tx: &Transaction<'_>,
    prepared: &PreparedRebuild,
    cursor: (i64, usize, i64),
    selected: &[OutlineEntry],
    at_end: bool,
) -> StoreResult<PreparedChunk> {
    let ticket = &prepared.ticket;
    let mut chunk = PreparedChunk {
        ticket: ticket.clone(),
        source_last_seq: prepared.last_seq,
        expected_seq: cursor.0,
        expected_index: cursor.1,
        expected_last_row: cursor.2,
        through: cursor.0,
        next_index: cursor.1,
        last_row: cursor.2,
        complete: false,
        entries: 0,
        bytes: 0,
        writes: Vec::new(),
    };
    let mut calls: HashMap<String, Option<String>> = HashMap::new();
    let mut first: HashMap<String, WorkingRow> = HashMap::new();
    let mut inserted: HashMap<String, usize> = HashMap::new();
    'entries: for outline in selected {
        if outline.omitted {
            chunk.through = outline.seq;
            chunk.next_index = 0;
            chunk.entries += 1;
            continue;
        }
        let entry = session::read_entry(tx, ticket.task, outline.seq)?;
        let operations = operations_tx(tx, ticket.task, &entry, |call| {
            if let Some(key) = calls.get(call) {
                return Ok(key.clone());
            }
            let key = latest_call(tx, ticket.task, ticket.epoch, call)?;
            calls.insert(call.to_owned(), key.clone());
            Ok(key)
        })?;
        if chunk.next_index > operations.len() {
            return Err(StoreError::InvalidData);
        }
        let starting_index = chunk.next_index;
        for (index, operation) in operations.into_iter().enumerate().skip(starting_index) {
            if chunk.writes.len() >= CHUNK_ROWS || chunk.bytes >= CHUNK_BYTES {
                chunk.next_index = index;
                break 'entries;
            }
            let incoming = StoredItem::new(&operation.item, outline.seq, index as i64)?;
            let row_key = operation.item.id.clone();
            let mut target = None;
            if operation.merge {
                if let Some(previous) = first.remove(&row_key) {
                    target = Some(previous);
                } else {
                    type ExistingRow = (i64, i64, i64, Option<String>, i64, Vec<u8>);
                    let existing:Option<ExistingRow>=tx.query_row("SELECT row_seq,entry_seq,item_index,call_id,codec,item FROM timeline_rows WHERE task_id=?1 AND epoch=?2 AND row_key=?3 AND hidden=0 ORDER BY row_seq LIMIT 1",params![ticket.task.0,ticket.epoch,row_key],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?))).optional()?;
                    if let Some((row_seq, entry_seq, item_index, call_id, item_codec, payload)) =
                        existing
                    {
                        target = Some(WorkingRow {
                            row_seq,
                            origin: RowOrigin {
                                entry_seq,
                                item_index,
                            },
                            call_id,
                            stored: StoredItem::decode(&codec::decode(item_codec, &payload)?)?,
                        });
                    } else if let Some(&position) = inserted.get(&row_key) {
                        let previous = &chunk.writes[position];
                        target = Some(WorkingRow {
                            row_seq: previous.row_seq,
                            origin: previous.origin,
                            call_id: previous.call_id.clone(),
                            stored: StoredItem::decode(&codec::decode(
                                previous.codec,
                                &previous.payload,
                            )?)?,
                        });
                    }
                }
            }
            let (working, is_insert) = if let Some(mut previous) = target {
                previous.stored.merge(incoming)?;
                (previous, false)
            } else {
                (
                    WorkingRow {
                        row_seq: chunk.last_row + 1,
                        origin: RowOrigin {
                            entry_seq: outline.seq,
                            item_index: index as i64,
                        },
                        call_id: operation.call_id.clone(),
                        stored: incoming,
                    },
                    true,
                )
            };
            let raw = working.stored.encode()?;
            if !chunk.writes.is_empty() && chunk.bytes.saturating_add(raw.len()) > CHUNK_BYTES {
                chunk.next_index = index;
                break 'entries;
            }
            let (item_codec, payload) = codec::encode(&raw)?;
            if is_insert {
                chunk.last_row = working.row_seq;
                inserted
                    .entry(row_key.clone())
                    .or_insert(chunk.writes.len());
            }
            if let Some(call) = operation.call_id {
                calls.insert(call, Some(row_key.clone()));
            }
            chunk.writes.push(PreparedWrite {
                row_seq: working.row_seq,
                origin: working.origin,
                update_seq: outline.seq,
                row_key: row_key.clone(),
                call_id: working.call_id.clone(),
                is_plan: is_plan_row(working.stored.item()),
                codec: item_codec,
                payload,
            });
            chunk.bytes = chunk.bytes.saturating_add(raw.len());
            if operation.merge {
                first.insert(row_key, working);
            }
            chunk.next_index = index + 1;
        }
        chunk.through = outline.seq;
        chunk.next_index = 0;
        chunk.entries += 1;
    }
    chunk.complete = at_end
        && chunk.next_index == 0
        && selected
            .last()
            .is_none_or(|entry| entry.seq <= chunk.through);
    Ok(chunk)
}

fn commit_chunk(
    tx: &Transaction<'_>,
    chunk: &PreparedChunk,
) -> StoreResult<(RebuildProgress, Option<super::StoreChange>)> {
    authorize_ticket(tx, &chunk.ticket)?;
    if rebuild_cursor(tx, &chunk.ticket)?
        != (
            chunk.expected_seq,
            chunk.expected_index,
            chunk.expected_last_row,
        )
    {
        return Err(StoreError::Conflict);
    }
    for row in &chunk.writes {
        tx.execute("INSERT INTO timeline_rows(task_id,epoch,row_seq,row_key,entry_seq,item_index,update_seq,call_id,hidden,is_plan,codec,bytes,item) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,0,?9,?10,?11,?12) ON CONFLICT(task_id,epoch,row_seq) DO UPDATE SET update_seq=excluded.update_seq,is_plan=excluded.is_plan,codec=excluded.codec,bytes=excluded.bytes,item=excluded.item",params![chunk.ticket.task.0,chunk.ticket.epoch,row.row_seq,row.row_key,row.origin.entry_seq,row.origin.item_index,row.update_seq,row.call_id,row.is_plan,row.codec,row.payload.len() as i64,row.payload])?;
    }
    tx.execute(
        "UPDATE tasks SET rebuild_seq=?2,rebuild_item_index=?3 WHERE id=?1 AND rebuild_epoch=?4",
        params![
            chunk.ticket.task.0,
            chunk.through,
            chunk.next_index as i64,
            chunk.ticket.epoch
        ],
    )?;
    if chunk.complete {
        let last_seq: i64 = tx.query_row(
            "SELECT last_seq FROM tasks WHERE id=?1",
            [chunk.ticket.task.0],
            |row| row.get(0),
        )?;
        if last_seq != chunk.source_last_seq {
            return Ok((RebuildProgress::NeedsSnapshot, None));
        }
        flip(
            tx,
            chunk.ticket.task,
            chunk.ticket.epoch,
            last_seq,
            chunk.last_row,
        )?;
        return Ok((
            RebuildProgress::Replaced {
                epoch: chunk.ticket.epoch,
            },
            Some(super::StoreChange {
                account: Some(chunk.ticket.account),
                task: Some(chunk.ticket.uuid.clone()),
                flipped_epoch: Some(chunk.ticket.epoch),
                summary_changed: true,
                ..Default::default()
            }),
        ));
    }
    Ok((
        RebuildProgress::Pending {
            entries: chunk.entries,
            bytes: chunk.bytes,
        },
        None,
    ))
}

/// Exact full JSON size, derived from escaped-string length deltas without
/// fetching or inflating the authoritative source entries.
pub(super) fn stored_row_size(item_codec: i64, payload: &[u8]) -> StoreResult<usize> {
    StoredItem::decode(&codec::decode(item_codec, payload)?)?.original_json_bytes()
}

/// Page reads need both exact sizes and the bounded item. Decode and validate
/// once rather than reconstructing the same payload separately for its size.
pub(super) fn decode_bounded_row(
    item_codec: i64,
    payload: &[u8],
) -> StoreResult<(AgentTimelineItem, usize, usize)> {
    StoredItem::decode_bounded(&codec::decode(item_codec, payload)?)
}

/// Decode a stored row. Full reconstruction fetches only the entries referenced
/// by reduced fields, retaining original source bytes rather than rewriting them.
pub(super) fn decode_row(
    tx: &Transaction<'_>,
    task: TaskKey,
    item_codec: i64,
    payload: &[u8],
    full: bool,
) -> StoreResult<AgentTimelineItem> {
    let stored = StoredItem::decode(&codec::decode(item_codec, payload)?)?;
    if !full {
        return stored.bounded_item();
    }
    stored.expand(|seq, index| {
        let entry = session::read_entry(tx, task, seq)?;
        operations_tx(tx, task, &entry, |_| Ok(None))?
            .into_iter()
            .nth(usize::try_from(index).map_err(|_| StoreError::InvalidData)?)
            .map(|operation| operation.item)
            .ok_or(StoreError::InvalidData)
    })
}

#[cfg(test)]
#[path = "projector/tests.rs"]
mod tests;
