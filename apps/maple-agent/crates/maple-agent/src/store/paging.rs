//! Bounded navigation, durable row identities and account-bound keyset cursors.
//!
//! The host supplies live run/queue state around these committed snapshots. It
//! subscribes before opening a page and retries when its live run marker changes.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::agent::AgentTimelineItem;

use super::{AccountId, Store, StoreError, StoreResult, projector, tasks::TaskKey};

mod navigation;
pub use navigation::*;

const MIB: usize = 1024 * 1024;
const MAX_CURSOR_BYTES: usize = 64 * 1024;
const MAX_ROW_RANGE_BYTES: usize = MIB;
const FULL_ROW_CACHE_BYTES: usize = 64 * MIB;
const MAX_CACHE_ENTRIES: usize = 64;
const MIN_PAGE_BYTES: usize = 4096;
const ENVELOPE_RESERVE: usize = 2048;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageTransport {
    Local,
    Remote,
}

#[derive(Clone, Copy, Debug)]
pub struct TransportCaps {
    pub max_reply_bytes: usize,
    pub default_open_bytes: usize,
    pub inline_row_bytes: usize,
    pub default_history_rows: u16,
    pub default_history_bytes: usize,
    pub max_history_rows: u16,
}

impl PageTransport {
    pub fn caps(self) -> TransportCaps {
        match self {
            Self::Local => TransportCaps {
                max_reply_bytes: 4 * MIB,
                default_open_bytes: 4 * MIB,
                inline_row_bytes: 2 * MIB,
                default_history_rows: 200,
                default_history_bytes: 4 * MIB,
                max_history_rows: 500,
            },
            Self::Remote => TransportCaps {
                max_reply_bytes: 4 * MIB,
                default_open_bytes: MIB / 2,
                inline_row_bytes: MIB / 4,
                default_history_rows: 100,
                default_history_bytes: MIB,
                max_history_rows: 500,
            },
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Cursor {
    pub(super) v: u8,
    pub(super) kind: String,
    pub(super) account: i64,
    pub(super) scope: Value,
    pub(super) position: Value,
}

pub(super) fn cursor_scope(value: &impl Serialize) -> StoreResult<Value> {
    let mut hasher = Sha256::new();
    hasher.update(b"maple.store.cursor.scope.v1\0");
    hasher.update(serde_json::to_vec(value).map_err(|_| StoreError::InvalidInput)?);
    Ok(Value::String(URL_SAFE_NO_PAD.encode(hasher.finalize())))
}

pub(super) fn encode_cursor(cursor: &Cursor) -> StoreResult<String> {
    let json = serde_json::to_vec(cursor).map_err(|_| StoreError::InvalidInput)?;
    if json.len() > MAX_CURSOR_BYTES {
        return Err(StoreError::InvalidInput);
    }
    Ok(URL_SAFE_NO_PAD.encode(json))
}

pub(super) fn decode_cursor(token: &str) -> StoreResult<Cursor> {
    if token.is_empty() || token.len() > MAX_CURSOR_BYTES.div_ceil(3) * 4 {
        return Err(StoreError::InvalidInput);
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(token)
        .map_err(|_| StoreError::InvalidInput)?;
    if bytes.len() > MAX_CURSOR_BYTES {
        return Err(StoreError::InvalidInput);
    }
    let cursor: Cursor = serde_json::from_slice(&bytes).map_err(|_| StoreError::InvalidInput)?;
    if cursor.v != 1 || cursor.kind.len() > 64 || cursor.account <= 0 {
        return Err(StoreError::InvalidInput);
    }
    Ok(cursor)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OpenTaskPageRequest {
    pub task: String,
    pub limit_rows: Option<u16>,
    pub limit_bytes: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HistoryPageRequest {
    pub task: String,
    pub cursor: Option<String>,
    pub limit_rows: Option<u16>,
    pub limit_bytes: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HistoryAnchorRequest {
    pub task: String,
    pub rows_epoch: i64,
    pub row_seq: i64,
    pub limit_rows: Option<u16>,
    pub limit_bytes: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimelineRow {
    pub rows_epoch: i64,
    pub row_seq: i64,
    pub update_seq: i64,
    pub item: AgentTimelineItem,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary_truncated: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryPage {
    pub rows: Vec<TimelineRow>,
    /// A backward page returns another before cursor; a forward page an after
    /// cursor. Cursors validate direction, task, epoch and account on every call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    pub rows_epoch: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenTaskPage {
    pub summary: TaskSummary,
    pub rows: Vec<TimelineRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    pub rows_epoch: i64,
    pub timeline_format: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<TimelineRow>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskFactsReply {
    pub model: Option<String>,
    pub thinking_level: Option<String>,
    pub context_usage: Option<u64>,
    pub context_estimate: u64,
    pub context_tokens: u64,
    pub message_count: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskHistoryStamp {
    pub rows_epoch: i64,
    pub last_seq: i64,
}

#[derive(Clone, Copy)]
enum Direction {
    Before,
    Forward,
}

impl Direction {
    fn kind(self) -> &'static str {
        match self {
            Self::Before => "history_before",
            Self::Forward => "history_forward",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryPosition {
    epoch: i64,
    row_seq: i64,
}

struct TaskSnapshot {
    key: TaskKey,
    epoch: i64,
    format: i64,
}

impl Store {
    /// A format migration can intentionally remove the old derived rows. The
    /// host must reconstruct that task before presenting it as an empty chat.
    /// Ordinary background rebuilds retain usable rows and need no foreground
    /// wait. This read never instantiates a Pi session or loads source payloads.
    pub(crate) fn history_needs_rebuild(
        &self,
        account: AccountId,
        task: &str,
    ) -> StoreResult<bool> {
        validate_task(task)?;
        self.read(|tx| {
            let snapshot = task_snapshot(tx, account, task)?;
            history_needs_rebuild(tx, &snapshot)
        })
    }

    pub fn task_history_stamp(
        &self,
        account: AccountId,
        task: &str,
    ) -> StoreResult<TaskHistoryStamp> {
        validate_task(task)?;
        self.read(|tx| Ok(tx.query_row("SELECT rows_epoch,last_seq FROM tasks WHERE account_id=?1 AND uuid=?2 AND deleted_ms IS NULL",
            params![account.0,task],|row|Ok(TaskHistoryStamp {rows_epoch:row.get(0)?,last_seq:row.get(1)?}))?))
    }

    pub fn store_tool_summary(
        &self,
        account: AccountId,
        task: &str,
        epoch: i64,
        row_seq: i64,
        summary: &str,
    ) -> StoreResult<()> {
        validate_task(task)?;
        if epoch < 0 || row_seq < 1 || summary.len() > MIB / 4 {
            return Err(StoreError::InvalidInput);
        }
        self.write_with_change(super::Durability::Normal,|tx| {
            super::tasks::writable_account(tx,account)?;
            let snapshot=task_snapshot(tx,account,task)?;
            if snapshot.epoch!=epoch {return Err(StoreError::EpochChanged);}
            let (entry_seq,item_index):(i64,i64)=tx.query_row("SELECT entry_seq,item_index FROM timeline_rows
                WHERE task_id=?1 AND epoch=?2 AND row_seq=?3 AND hidden=0",params![snapshot.key.0,epoch,row_seq],
                |row|Ok((row.get(0)?,row.get(1)?)))?;
            tx.execute("INSERT INTO tool_summaries(task_id,entry_seq,item_index,bytes,summary) VALUES (?1,?2,?3,?4,?5)
                ON CONFLICT(task_id,entry_seq,item_index) DO UPDATE SET bytes=excluded.bytes,summary=excluded.summary",
                params![snapshot.key.0,entry_seq,item_index,summary.len() as i64,summary])?;
            Ok(((),Some(super::StoreChange {account:Some(account),task:Some(task.into()),summary_changed:true,..Default::default()})))
        })
    }

    pub fn open_task_page(
        &self,
        account: AccountId,
        request: &OpenTaskPageRequest,
        transport: PageTransport,
    ) -> StoreResult<OpenTaskPage> {
        validate_task(&request.task)?;
        let caps = transport.caps();
        let rows = request
            .limit_rows
            .unwrap_or(if transport == PageTransport::Local {
                100
            } else {
                50
            });
        let bytes = request.limit_bytes.unwrap_or(caps.default_open_bytes);
        validate_page_limits(rows, bytes, 500, caps.max_reply_bytes)?;
        self.read(|tx| {
            let task = task_snapshot(tx, account, &request.task)?;
            if history_needs_rebuild(tx, &task)? {
                return Err(StoreError::Busy);
            }
            let mut summary = navigation::task_summary_tx(tx, account, &request.task)?
                .ok_or(StoreError::NotFound)?;
            let inline = caps.inline_row_bytes.min((bytes / 3).max(256));
            let (plan, plan_bytes) =
                match latest_plan(tx, task.key, task.epoch, inline.min((bytes / 4).max(512)))? {
                    Some((plan, size)) => (Some(plan), size),
                    None => (None, 0),
                };
            // Immutable canonical roots can be large or heavily escaped. Give
            // their actual footprint the envelope space rather than rejecting
            // a valid page because it exceeds an arbitrary fraction of it.
            let summary_budget = bytes
                .checked_sub(plan_bytes + ENVELOPE_RESERVE + 512)
                .ok_or(StoreError::InvalidInput)?;
            navigation::fit_task_summary(&mut summary, summary_budget)?;
            let reserved = serialized_len(&summary)? + plan_bytes + ENVELOPE_RESERVE;
            let budget = bytes
                .checked_sub(reserved)
                .ok_or(StoreError::InvalidInput)?;
            let page = history_rows(
                tx,
                account,
                &request.task,
                &task,
                Direction::Before,
                None,
                rows,
                budget,
                inline,
            )?;
            let response = OpenTaskPage {
                summary,
                rows: page.rows,
                before: page.next,
                rows_epoch: task.epoch,
                timeline_format: task.format,
                plan,
            };
            if serialized_len(&response)? > bytes {
                return Err(StoreError::InvalidData);
            }
            Ok(response)
        })
    }

    pub fn history_before_page(
        &self,
        account: AccountId,
        request: &HistoryPageRequest,
        transport: PageTransport,
    ) -> StoreResult<HistoryPage> {
        if request.cursor.is_none() {
            return Err(StoreError::InvalidInput);
        }
        self.history_page(account, request, transport, Direction::Before)
    }

    pub fn history_forward_page(
        &self,
        account: AccountId,
        request: &HistoryPageRequest,
        transport: PageTransport,
    ) -> StoreResult<HistoryPage> {
        self.history_page(account, request, transport, Direction::Forward)
    }

    /// Resume from a durable visible row after the client has evicted its newer
    /// window. Continuation responses use the ordinary opaque forward cursor.
    pub fn history_forward_from(
        &self,
        account: AccountId,
        request: &HistoryAnchorRequest,
        transport: PageTransport,
    ) -> StoreResult<HistoryPage> {
        self.history_from_anchor(account, request, transport, Direction::Forward)
    }

    pub fn history_before_from(
        &self,
        account: AccountId,
        request: &HistoryAnchorRequest,
        transport: PageTransport,
    ) -> StoreResult<HistoryPage> {
        self.history_from_anchor(account, request, transport, Direction::Before)
    }

    fn history_from_anchor(
        &self,
        account: AccountId,
        request: &HistoryAnchorRequest,
        transport: PageTransport,
        direction: Direction,
    ) -> StoreResult<HistoryPage> {
        validate_task(&request.task)?;
        if request.rows_epoch < 0 || request.row_seq < 0 {
            return Err(StoreError::InvalidInput);
        }
        let caps = transport.caps();
        let rows = request.limit_rows.unwrap_or(caps.default_history_rows);
        let bytes = request.limit_bytes.unwrap_or(caps.default_history_bytes);
        validate_page_limits(rows, bytes, caps.max_history_rows, caps.max_reply_bytes)?;
        self.read(|tx| {
            let task = task_snapshot(tx, account, &request.task)?;
            if task.epoch != request.rows_epoch {
                return Err(StoreError::EpochChanged);
            }
            let page = history_rows(
                tx,
                account,
                &request.task,
                &task,
                direction,
                Some(request.row_seq),
                rows,
                bytes - ENVELOPE_RESERVE,
                caps.inline_row_bytes.min((bytes - ENVELOPE_RESERVE) / 2),
            )?;
            if serialized_len(&page)? > bytes {
                return Err(StoreError::InvalidData);
            }
            Ok(page)
        })
    }

    fn history_page(
        &self,
        account: AccountId,
        request: &HistoryPageRequest,
        transport: PageTransport,
        direction: Direction,
    ) -> StoreResult<HistoryPage> {
        validate_task(&request.task)?;
        let caps = transport.caps();
        let rows = request.limit_rows.unwrap_or(caps.default_history_rows);
        let bytes = request.limit_bytes.unwrap_or(caps.default_history_bytes);
        validate_page_limits(rows, bytes, caps.max_history_rows, 4 * MIB)?;
        let position = request
            .cursor
            .as_deref()
            .map(|token| history_position(token, account, &request.task, direction))
            .transpose()?;
        self.read(|tx| {
            let task = task_snapshot(tx, account, &request.task)?;
            if position.is_some_and(|position| position.epoch != task.epoch) {
                return Err(StoreError::EpochChanged);
            }
            let page = history_rows(
                tx,
                account,
                &request.task,
                &task,
                direction,
                position.map(|p| p.row_seq),
                rows,
                bytes - ENVELOPE_RESERVE,
                caps.inline_row_bytes.min((bytes - ENVELOPE_RESERVE) / 2),
            )?;
            if serialized_len(&page)? > bytes {
                return Err(StoreError::InvalidData);
            }
            Ok(page)
        })
    }

    /// Facts are independent of the size of the source history and context base.
    pub fn task_facts(&self, account: AccountId, task: &str) -> StoreResult<TaskFactsReply> {
        validate_task(task)?;
        self.read(|tx| {
            let raw:(Option<String>,Option<String>,Option<i64>,i64,i64)=tx.query_row(
                "SELECT model,thinking_level,context_usage,context_estimate,message_count FROM tasks
                 WHERE account_id=?1 AND uuid=?2 AND deleted_ms IS NULL",params![account.0,task],
                |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)))?;
            let usage = raw.2.map(nonnegative).transpose()?;
            let estimate = nonnegative(raw.3)?;
            Ok(TaskFactsReply {
                model: raw.0,
                thinking_level: raw.1,
                context_usage: usage,
                context_estimate: estimate,
                context_tokens: usage.unwrap_or(estimate),
                message_count: nonnegative(raw.4)?,
            })
        })
    }
}

fn history_needs_rebuild(tx: &Transaction<'_>, task: &TaskSnapshot) -> StoreResult<bool> {
    if task.format == projector::TIMELINE_FORMAT {
        return Ok(false);
    }
    Ok(tx.query_row(
        "SELECT last_seq>0 AND NOT EXISTS(SELECT 1 FROM timeline_rows WHERE task_id=?1 AND epoch=?2) FROM tasks WHERE id=?1",
        params![task.key.0, task.epoch],
        |row| row.get(0),
    )?)
}

fn task_snapshot(
    tx: &Transaction<'_>,
    account: AccountId,
    uuid: &str,
) -> StoreResult<TaskSnapshot> {
    Ok(tx.query_row("SELECT id,rows_epoch,timeline_format FROM tasks WHERE account_id=?1 AND uuid=?2 AND deleted_ms IS NULL",
        params![account.0,uuid],|row|Ok(TaskSnapshot {key:TaskKey(row.get(0)?),epoch:row.get(1)?,format:row.get(2)?}))?)
}

fn history_position(
    token: &str,
    account: AccountId,
    task: &str,
    direction: Direction,
) -> StoreResult<HistoryPosition> {
    let cursor = decode_cursor(token)?;
    if cursor.kind != direction.kind()
        || cursor.account != account.0
        || cursor.scope != cursor_scope(&json!({"task":task}))?
    {
        return Err(StoreError::InvalidInput);
    }
    let position: HistoryPosition =
        serde_json::from_value(cursor.position).map_err(|_| StoreError::InvalidInput)?;
    if position.epoch < 0 || position.row_seq < 1 {
        return Err(StoreError::InvalidInput);
    }
    Ok(position)
}

fn history_cursor(
    account: AccountId,
    task: &str,
    direction: Direction,
    epoch: i64,
    row_seq: i64,
) -> StoreResult<String> {
    encode_cursor(&Cursor {
        v: 1,
        kind: direction.kind().into(),
        account: account.0,
        scope: cursor_scope(&json!({"task":task}))?,
        position: json!({"epoch":epoch,"row_seq":row_seq}),
    })
}

#[allow(clippy::too_many_arguments)]
fn history_rows(
    tx: &Transaction<'_>,
    account: AccountId,
    uuid: &str,
    task: &TaskSnapshot,
    direction: Direction,
    position: Option<i64>,
    limit: u16,
    budget: usize,
    inline: usize,
) -> StoreResult<HistoryPage> {
    let (comparison, order) = match direction {
        Direction::Before => ("<", "DESC"),
        Direction::Forward => (">", "ASC"),
    };
    let boundary = position.unwrap_or(match direction {
        Direction::Before => i64::MAX,
        Direction::Forward => 0,
    });
    let sql=format!("SELECT r.row_seq,r.update_seq,r.codec,r.bytes,r.item,s.summary
        FROM timeline_rows r LEFT JOIN tool_summaries s ON s.task_id=r.task_id AND s.entry_seq=r.entry_seq AND s.item_index=r.item_index
        WHERE r.task_id=?1 AND r.epoch=?2 AND r.hidden=0 AND r.row_seq {comparison} ?3
        ORDER BY r.row_seq {order} LIMIT ?4");
    let mut statement = tx.prepare_cached(&sql)?;
    let mut candidates = statement.query(params![
        task.key.0,
        task.epoch,
        boundary,
        i64::from(limit) + 1
    ])?;
    let mut rows = Vec::new();
    let mut wire_bytes = 2usize;
    let mut stored_bytes = 0usize;
    let mut more = false;
    while let Some(row) = candidates.next()? {
        let stored = usize::try_from(row.get::<_, i64>(3)?).map_err(|_| StoreError::InvalidData)?;
        if rows.len() >= usize::from(limit)
            || (!rows.is_empty() && stored_bytes.saturating_add(stored) > budget)
        {
            more = true;
            break;
        }
        let payload: Vec<u8> = row.get(4)?;
        let item_codec: i64 = row.get(2)?;
        let (item, item_bytes) = decode_page_item(item_codec, &payload)?;
        let mut entry = TimelineRow {
            rows_epoch: task.epoch,
            row_seq: row.get(0)?,
            update_seq: row.get(1)?,
            item,
            summary: row.get(5)?,
            summary_truncated: None,
        };
        let mut size = fit_row(
            &mut entry,
            inline.min(budget.saturating_sub(128)),
            item_bytes,
        )?;
        if !rows.is_empty() && wire_bytes.saturating_add(size.row_bytes + 1) > budget {
            more = true;
            break;
        }
        if wire_bytes.saturating_add(size.row_bytes + 1) > budget {
            size = fit_row(&mut entry, budget.saturating_sub(2), size.item_bytes)?;
        }
        wire_bytes = wire_bytes.saturating_add(size.row_bytes + 1);
        stored_bytes = stored_bytes.saturating_add(stored);
        rows.push(entry);
    }
    let next = if more {
        rows.last()
            .map(|row| history_cursor(account, uuid, direction, task.epoch, row.row_seq))
            .transpose()?
    } else {
        None
    };
    if matches!(direction, Direction::Before) {
        rows.reverse();
    }
    Ok(HistoryPage {
        rows,
        next,
        rows_epoch: task.epoch,
    })
}

fn latest_plan(
    tx: &Transaction<'_>,
    task: TaskKey,
    epoch: i64,
    inline: usize,
) -> StoreResult<Option<(TimelineRow, usize)>> {
    type StoredPlanRow = (i64, i64, i64, Vec<u8>, Option<String>);
    let raw:Option<StoredPlanRow>=tx.query_row(
        "SELECT r.row_seq,r.update_seq,r.codec,r.item,s.summary FROM timeline_rows r
         LEFT JOIN tool_summaries s ON s.task_id=r.task_id AND s.entry_seq=r.entry_seq AND s.item_index=r.item_index
         WHERE r.task_id=?1 AND r.epoch=?2 AND r.is_plan=1 AND r.hidden=0 ORDER BY r.row_seq DESC LIMIT 1",
        params![task.0,epoch],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?))).optional()?;
    raw.map(|(seq, update, codec, payload, summary)| {
        let (item, item_bytes) = decode_page_item(codec, &payload)?;
        let mut row = TimelineRow {
            rows_epoch: epoch,
            row_seq: seq,
            update_seq: update,
            item,
            summary,
            summary_truncated: None,
        };
        let size = fit_row(&mut row, inline, item_bytes)?;
        Ok((row, size.row_bytes))
    })
    .transpose()
}

fn decode_page_item(codec: i64, payload: &[u8]) -> StoreResult<(AgentTimelineItem, usize)> {
    let (mut item, original, mut bytes) = projector::decode_bounded_row(codec, payload)?;
    if original > bytes {
        item.truncated = Some(original);
        // Adding the preview marker invalidates the decoder's exact size.
        bytes = serialized_len(&item)?;
    }
    Ok((item, bytes))
}

struct FittedRowSize {
    item_bytes: usize,
    row_bytes: usize,
}

/// `item_bytes` is exact for the current item. Return updated exact sizes so
/// page admission and its running byte count do not serialize the row again.
fn fit_row(row: &mut TimelineRow, cap: usize, item_bytes: usize) -> StoreResult<FittedRowSize> {
    if let Some(summary) = &mut row.summary {
        let summary_cap = cap / 8;
        let original = summary.len();
        while serialized_len(summary)? > summary_cap && !summary.is_empty() {
            truncate_utf8(summary, summary.len() / 2);
        }
        if summary.len() != original {
            row.summary_truncated.get_or_insert(original);
        }
    }
    let overhead = serialized_len(&(
        row.rows_epoch,
        row.row_seq,
        row.update_seq,
        &row.summary,
        &row.summary_truncated,
    ))? + 128;
    let item_cap = cap.checked_sub(overhead).ok_or(StoreError::InvalidInput)?;
    let original = row.item.truncated.unwrap_or(item_bytes);
    let item_bytes = fit_item(&mut row.item, original, item_cap, item_bytes)?;
    let row_bytes = serialized_len(row)?;
    if row_bytes > cap {
        return Err(StoreError::InvalidData);
    }
    Ok(FittedRowSize {
        item_bytes,
        row_bytes,
    })
}

/// Long strings shrink together in a bounded number of passes. If structural
/// JSON alone exceeds the budget, a payload preview replaces that subtree; the
/// full source remains available by durable identity through read_row.
fn fit_item(
    item: &mut AgentTimelineItem,
    original: usize,
    cap: usize,
    current_bytes: usize,
) -> StoreResult<usize> {
    if current_bytes <= cap {
        return Ok(current_bytes);
    }
    item.truncated = Some(original);
    let mut value = serde_json::to_value(&*item).map_err(|_| StoreError::InvalidData)?;
    let metadata_cap = (cap / 16).clamp(16, 256);
    for key in ["id", "role", "status", "itemType", "merge"] {
        if let Some(Value::String(text)) = value.get_mut(key)
            && text.len() > cap / 4
        {
            truncate_utf8(text, metadata_cap);
        }
    }
    let mut previous = usize::MAX;
    for _ in 0..16 {
        let len = serialized_len(&value)?;
        if len <= cap {
            *item = serde_json::from_value(value).map_err(|_| StoreError::InvalidData)?;
            return serialized_len(item);
        }
        if len >= previous {
            break;
        }
        previous = len;
        let ratio = (cap as f64 / len as f64 * 0.75).clamp(0.0, 0.8);
        for key in ["text", "input", "output", "title"] {
            if let Some(payload) = value.get_mut(key) {
                shrink_strings(payload, ratio);
            }
        }
    }
    // A large collection of tiny objects has no long string to shorten.
    for key in ["output", "input", "text", "title"] {
        if value.get(key).is_some() {
            value[key] = Value::Null;
        }
        if serialized_len(&value)? <= cap {
            *item = serde_json::from_value(value).map_err(|_| StoreError::InvalidData)?;
            return serialized_len(item);
        }
    }
    // Provider call ids and custom notice ids are not authoritative identity.
    // A hostile oversized legacy key must not prevent access to the task. The
    // durable row identity above remains exact; read_row restores every field.
    for key in ["id", "role", "status", "itemType", "merge"] {
        if let Some(Value::String(text)) = value.get_mut(key) {
            truncate_utf8(text, metadata_cap);
        }
    }
    if serialized_len(&value)? <= cap {
        *item = serde_json::from_value(value).map_err(|_| StoreError::InvalidData)?;
        return serialized_len(item);
    }
    Err(StoreError::InvalidInput)
}

fn shrink_strings(value: &mut Value, ratio: f64) {
    match value {
        Value::String(text) => truncate_utf8(text, (text.len() as f64 * ratio) as usize),
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| shrink_strings(item, ratio)),
        Value::Object(items) => items
            .values_mut()
            .for_each(|item| shrink_strings(item, ratio)),
        _ => {}
    }
}

fn truncate_utf8(text: &mut String, mut bytes: usize) {
    bytes = bytes.min(text.len());
    while !text.is_char_boundary(bytes) {
        bytes -= 1;
    }
    text.truncate(bytes);
}

fn serialized_len(value: &impl Serialize) -> StoreResult<usize> {
    #[derive(Default)]
    struct Counter(usize);

    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 = self
                .0
                .checked_add(bytes.len())
                .ok_or_else(|| io::Error::other("JSON size overflow"))?;
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let mut counter = Counter::default();
    serde_json::to_writer(&mut counter, value).map_err(|_| StoreError::InvalidData)?;
    Ok(counter.0)
}

fn validate_task(task: &str) -> StoreResult<()> {
    if task.len() != 36
        || !task.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
    {
        return Err(StoreError::InvalidInput);
    }
    Ok(())
}

fn validate_page_limits(
    rows: u16,
    bytes: usize,
    max_rows: u16,
    max_bytes: usize,
) -> StoreResult<()> {
    if rows == 0 || rows > max_rows || !(MIN_PAGE_BYTES..=max_bytes).contains(&bytes) {
        Err(StoreError::InvalidInput)
    } else {
        Ok(())
    }
}

fn nonnegative(value: i64) -> StoreResult<u64> {
    u64::try_from(value).map_err(|_| StoreError::InvalidData)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RowRangeRequest {
    pub task: String,
    pub rows_epoch: i64,
    pub row_seq: i64,
    /// Send the row's page identity on the first range when available, and the
    /// returned version on every subsequent range. In-place updates keep epoch.
    #[serde(default)]
    pub update_seq: Option<i64>,
    pub offset: u64,
    pub len: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RowRange {
    pub total: u64,
    pub update_seq: i64,
    pub data: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentRangeReply {
    pub mime: String,
    pub size: u64,
    pub data: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CacheKey {
    store_id: [u8; 16],
    account: AccountId,
    task: String,
    epoch: i64,
    row_seq: i64,
    update_seq: i64,
}

struct CachedRow {
    key: CacheKey,
    bytes: Zeroizing<Vec<u8>>,
}

#[derive(Default)]
struct RowCache {
    rows: VecDeque<CachedRow>,
    bytes: usize,
}

/// Own this cache beside the unlocked host session. Drain reads and call clear
/// before locking, changing account or dropping the unlocked session. Cache
/// entries wipe their byte allocation when evicted or cleared.
pub struct RowReader {
    store: Arc<Store>,
    cache: Mutex<RowCache>,
}

impl RowReader {
    pub fn new(store: Arc<Store>) -> Self {
        Self {
            store,
            cache: Mutex::new(RowCache::default()),
        }
    }

    pub fn clear(&self) -> StoreResult<()> {
        let mut cache = self.cache.lock().map_err(|_| StoreError::Busy)?;
        cache.rows.clear();
        cache.bytes = 0;
        Ok(())
    }

    pub fn cached_bytes(&self) -> StoreResult<usize> {
        Ok(self.cache.lock().map_err(|_| StoreError::Busy)?.bytes)
    }

    pub fn read_row(
        &self,
        account: AccountId,
        request: &RowRangeRequest,
        _transport: PageTransport,
    ) -> StoreResult<RowRange> {
        validate_task(&request.task)?;
        if request.len > MAX_ROW_RANGE_BYTES
            || request.rows_epoch < 0
            || request.row_seq < 1
            || (request.offset > 0 && request.update_seq.is_none())
        {
            return Err(StoreError::InvalidInput);
        }
        self.store.read(|tx| {
            let task=task_snapshot(tx,account,&request.task)?;
            if task.epoch!=request.rows_epoch {return Err(StoreError::EpochChanged);}
            let update:i64=tx.query_row("SELECT update_seq FROM timeline_rows WHERE task_id=?1 AND epoch=?2 AND row_seq=?3 AND hidden=0",
                params![task.key.0,task.epoch,request.row_seq],|row|row.get(0))?;
            if request.update_seq.is_some_and(|expected|expected!=update) {return Err(StoreError::Conflict);}
            let store_id:Vec<u8>=tx.query_row("SELECT value FROM store_meta WHERE key='store_id'",[],|row|row.get(0))?;
            let store_id=store_id.try_into().map_err(|_|StoreError::InvalidData)?;
            let key=CacheKey {store_id,account,task:request.task.clone(),epoch:task.epoch,row_seq:request.row_seq,update_seq:update};
            {
                let mut cache=self.cache.lock().map_err(|_|StoreError::Busy)?;
                if let Some(index)=cache.rows.iter().position(|row|row.key==key) {
                    // Validate before moving the entry: a rejected range must
                    // neither evict it nor leave phantom cache byte accounting.
                    let response=row_range(&cache.rows[index].bytes,update,request.offset,request.len)?;
                    let cached=cache.rows.remove(index).ok_or(StoreError::InvalidData)?;
                    cache.rows.push_back(cached);
                    return Ok(response);
                }
                // Earlier versions of this occurrence cannot be useful again.
                let mut removed=0;
                cache.rows.retain(|row| {
                    let keep=!(row.key.account==account && row.key.task==request.task && row.key.row_seq==request.row_seq);
                    if !keep {removed+=row.bytes.len();}keep
                });
                cache.bytes=cache.bytes.saturating_sub(removed);
            }
            let (codec,payload):(i64,Vec<u8>)=tx.query_row("SELECT codec,item FROM timeline_rows WHERE task_id=?1 AND epoch=?2 AND row_seq=?3 AND hidden=0",
                params![task.key.0,task.epoch,request.row_seq],|row|Ok((row.get(0)?,row.get(1)?)))?;
            let total=projector::stored_row_size(codec,&payload)? as u64;
            if request.offset>total {return Err(StoreError::InvalidInput);}
            if request.len==0 {return Ok(RowRange {total,update_seq:update,data:String::new()});}
            let item=projector::decode_row(tx,task.key,codec,&payload,true)?;
            let mut bytes=Zeroizing::new(serde_json::to_vec(&item).map_err(|_|StoreError::InvalidData)?);
            let response=row_range(&bytes,update,request.offset,request.len)?;
            if bytes.len()<=FULL_ROW_CACHE_BYTES {
                // Serialization grows geometrically. Retaining spare capacity
                // would let several medium rows exceed the cache memory cap.
                bytes.shrink_to_fit();
                let mut cache=self.cache.lock().map_err(|_|StoreError::Busy)?;
                while cache.rows.len()>=MAX_CACHE_ENTRIES || cache.bytes.saturating_add(bytes.len())>FULL_ROW_CACHE_BYTES {
                    let Some(evicted)=cache.rows.pop_front() else {break};
                    cache.bytes=cache.bytes.saturating_sub(evicted.bytes.len());
                }
                cache.bytes=cache.bytes.saturating_add(bytes.len());
                cache.rows.push_back(CachedRow {key,bytes});
            }
            // A single row larger than the cache limit is served but never
            // retained. Such a row must be reconstructed for another range.
            Ok(response)
        })
    }

    pub fn read_attachment(
        &self,
        account: AccountId,
        task: &str,
        attachment: &str,
        offset: u64,
        len: usize,
        _transport: PageTransport,
    ) -> StoreResult<AttachmentRangeReply> {
        if len > MAX_ROW_RANGE_BYTES {
            return Err(StoreError::InvalidInput);
        }
        let range = self
            .store
            .read_attachment(account, task, attachment, offset, len)?;
        Ok(AttachmentRangeReply {
            mime: range.mime,
            size: range.size,
            data: STANDARD.encode(range.data),
        })
    }
}

fn row_range(bytes: &[u8], update_seq: i64, offset: u64, len: usize) -> StoreResult<RowRange> {
    let offset = usize::try_from(offset).map_err(|_| StoreError::InvalidInput)?;
    if offset > bytes.len() {
        return Err(StoreError::InvalidInput);
    }
    let end = offset.saturating_add(len).min(bytes.len());
    Ok(RowRange {
        total: bytes.len() as u64,
        update_seq,
        data: STANDARD.encode(&bytes[offset..end]),
    })
}

#[cfg(test)]
#[path = "paging/tests.rs"]
mod tests;
