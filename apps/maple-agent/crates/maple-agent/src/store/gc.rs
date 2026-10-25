//! Resumable, byte-bounded collection after a durable deletion tombstone.
//!
//! Source entries are removed only here, after explicit deletion. A pending
//! server account-deletion attempt never grants permission to collect data.

use std::time::Instant;

use rusqlite::{OptionalExtension, Transaction, params, params_from_iter};

use super::{AccountId, Store, StoreError, StoreResult};

const ROW_CAP: usize = 1_000;
const BYTE_CAP: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CollectionStage {
    Timeline,
    Entries,
    Summaries,
    Attachments,
    TaskMetadata,
    AccountMetadata,
    Done,
}

/// An oversized source row is permitted only as a transaction of its own. Its
/// bytes and elapsed time are reported for the implementation performance gate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionProgress {
    pub stage: CollectionStage,
    pub rows: usize,
    pub bytes: u64,
    pub oversized_row: bool,
    pub elapsed_micros: u64,
    pub done: bool,
}

impl CollectionProgress {
    fn new(stage: CollectionStage, rows: usize, bytes: u64) -> Self {
        Self {
            stage,
            rows,
            bytes,
            oversized_row: rows == 1 && bytes > BYTE_CAP,
            elapsed_micros: 0,
            done: false,
        }
    }

    fn measured(mut self, started: Instant) -> Self {
        self.elapsed_micros = started.elapsed().as_micros().try_into().unwrap_or(u64::MAX);
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeletedTask {
    pub account: AccountId,
    pub uuid: String,
}

impl Store {
    /// Startup and idle collection use only authorized tombstones. Pending
    /// account attempts are excluded even if a task had an earlier tombstone.
    pub fn deleted_tasks(&self, limit: u16) -> StoreResult<Vec<DeletedTask>> {
        if limit == 0 || limit > 200 {
            return Err(StoreError::InvalidInput);
        }
        self.read(|tx| {
            let mut statement = tx.prepare_cached(
                "SELECT t.account_id,t.uuid FROM tasks t INDEXED BY tasks_deleting
                JOIN accounts a ON a.id=t.account_id WHERE t.deleted_ms IS NOT NULL
                AND (a.deletion_phase IS NULL OR a.deletion_phase='confirmed')
                ORDER BY t.deleted_ms,t.id LIMIT ?1",
            )?;
            Ok(statement
                .query_map([limit], |row| {
                    Ok(DeletedTask {
                        account: AccountId(row.get(0)?),
                        uuid: row.get(1)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Perform at most one byte-capped payload transaction, or one attachment
    /// filesystem cleanup step. The caller yields to foreground work between calls.
    pub fn collect_task_step(
        &self,
        account: AccountId,
        uuid: &str,
    ) -> StoreResult<CollectionProgress> {
        let started = Instant::now();
        let payload = self.write_idle(|tx| {
            let task = collectable_task(tx, account, uuid)?;
            for (table, stage) in [
                ("timeline_rows", CollectionStage::Timeline),
                ("entries", CollectionStage::Entries),
            ] {
                if let Some(progress) =
                    delete_rowid_batch(tx, table, "task_id", task, "bytes", stage)?
                {
                    return Ok(Some(progress));
                }
            }
            if let Some(progress) = delete_summary_batch(tx, task)? {
                return Ok(Some(progress));
            }
            Ok(None)
        })?;
        if let Some(progress) = payload {
            return Ok(progress.measured(started));
        }

        // Attachment cleanup removes the exact owned files before their metadata.
        // Run outside the writer so filesystem I/O never holds a SQL transaction.
        let attachments = self.collect_task_attachments_step(account, uuid)?;
        if !attachments.done || attachments.files != 0 || attachments.links != 0 {
            return Ok(CollectionProgress::new(
                CollectionStage::Attachments,
                attachments.files + attachments.links,
                attachments.bytes as u64,
            )
            .measured(started));
        }

        self.write_idle(|tx| {
            let task = collectable_task(tx, account, uuid)?;
            let children: bool = tx.query_row(
                "SELECT
                EXISTS(SELECT 1 FROM entries WHERE task_id=?1) OR
                EXISTS(SELECT 1 FROM timeline_rows WHERE task_id=?1) OR
                EXISTS(SELECT 1 FROM tool_summaries WHERE task_id=?1) OR
                EXISTS(SELECT 1 FROM file_attachments WHERE task_id=?1)",
                [task],
                |row| row.get(0),
            )?;
            if children {
                return Err(StoreError::Conflict);
            }
            let rows = tx.execute(
                "DELETE FROM tasks WHERE id=?1 AND account_id=?2 AND deleted_ms IS NOT NULL",
                params![task, account.0],
            )?;
            let mut progress = CollectionProgress::new(CollectionStage::TaskMetadata, rows, 0);
            progress.done = true;
            Ok(progress)
        })
        .map(|progress| progress.measured(started))
    }

    /// After every task has finished collection, precollect account-owned
    /// variable-sized metadata before the final small cascade. This method
    /// requires the durable confirmed server outcome on every transaction.
    pub fn collect_account_step(&self, account: AccountId) -> StoreResult<CollectionProgress> {
        let started = Instant::now();
        self.write_idle(|tx| {
            let phase: Option<String> = tx.query_row("SELECT deletion_phase FROM accounts WHERE id=?1",
                [account.0], |row| row.get(0)).optional()?.flatten();
            if phase.as_deref() != Some("confirmed") { return Err(StoreError::Conflict); }
            let has_tasks: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM tasks WHERE account_id=?1)",
                [account.0], |row| row.get(0))?;
            if has_tasks { return Err(StoreError::Busy); }

            if let Some(progress) = delete_mcp_secret_batch(tx, account)? { return Ok(progress); }
            for (table, size) in [
                ("account_settings", "length(CAST(value AS BLOB))+length(CAST(key AS BLOB))"),
                ("projects", "length(CAST(root AS BLOB))+coalesce(length(CAST(display_name AS BLOB)),0)+length(CAST(mcp_defaults AS BLOB))"),
                ("mcp_servers", "length(CAST(key AS BLOB))+length(CAST(definition AS BLOB))"),
                ("send_receipts", "length(CAST(id AS BLOB))+length(fingerprint)+length(CAST(task_uuid AS BLOB))+coalesce(length(CAST(result AS BLOB)),0)"),
                ("remote_identities", "length(CAST(scope AS BLOB))+length(secret)+coalesce(length(CAST(name AS BLOB)),0)+length(CAST(record AS BLOB))"),
                ("credentials", "length(access_token)+length(refresh_token)+length(CAST(api_url AS BLOB))+coalesce(length(CAST(session_id AS BLOB)),0)"),
            ] {
                if let Some(progress) = delete_rowid_batch(tx, table, "account_id", account.0,
                    size, CollectionStage::AccountMetadata)? { return Ok(progress); }
            }
            let rows = tx.execute("DELETE FROM accounts WHERE id=?1 AND deletion_phase='confirmed'", [account.0])?;
            let mut progress = CollectionProgress::new(CollectionStage::Done, rows, 0);
            progress.done = true;
            Ok(progress)
        }).map(|progress| progress.measured(started))
    }
}

fn collectable_task(tx: &Transaction<'_>, account: AccountId, uuid: &str) -> StoreResult<i64> {
    let record: Option<(i64, Option<i64>, Option<String>)> = tx
        .query_row(
            "SELECT t.id,t.deleted_ms,a.deletion_phase
        FROM tasks t JOIN accounts a ON a.id=t.account_id WHERE t.account_id=?1 AND t.uuid=?2",
            params![account.0, uuid],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    match record {
        Some((id, Some(_), None)) => Ok(id),
        Some((id, Some(_), Some(phase))) if phase == "confirmed" => Ok(id),
        Some(_) => Err(StoreError::Conflict),
        None => Err(StoreError::NotFound),
    }
}

/// Identifiers and size expressions come only from constants in this module.
/// Selection and deletion share one writer transaction, so rows cannot change
/// size after they have been admitted to the byte budget.
fn delete_rowid_batch(
    tx: &Transaction<'_>,
    table: &str,
    owner_column: &str,
    owner: i64,
    size_expression: &str,
    stage: CollectionStage,
) -> StoreResult<Option<CollectionProgress>> {
    let sql =
        format!("SELECT rowid,{size_expression} FROM {table} WHERE {owner_column}=?1 LIMIT ?2");
    let mut statement = tx.prepare_cached(&sql)?;
    let mut candidates = statement.query(params![owner, ROW_CAP as i64])?;
    let mut ids = Vec::new();
    let mut bytes = 0_u64;
    while let Some(row) = candidates.next()? {
        let size = u64::try_from(row.get::<_, i64>(1)?).map_err(|_| StoreError::InvalidData)?;
        if !ids.is_empty() && bytes.saturating_add(size) > BYTE_CAP {
            break;
        }
        ids.push(row.get::<_, i64>(0)?);
        bytes = bytes.saturating_add(size);
        if bytes > BYTE_CAP {
            break;
        }
    }
    drop(candidates);
    drop(statement);
    if ids.is_empty() {
        return Ok(None);
    }
    let placeholders = std::iter::repeat_n("?", ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("DELETE FROM {table} WHERE rowid IN ({placeholders})");
    let rows = tx.execute(&sql, params_from_iter(ids))?;
    Ok(Some(CollectionProgress::new(stage, rows, bytes)))
}

fn delete_summary_batch(
    tx: &Transaction<'_>,
    task: i64,
) -> StoreResult<Option<CollectionProgress>> {
    let mut statement = tx.prepare_cached(
        "SELECT entry_seq,item_index,bytes FROM tool_summaries
        WHERE task_id=?1 ORDER BY entry_seq,item_index LIMIT ?2",
    )?;
    let mut candidates = statement.query(params![task, ROW_CAP as i64])?;
    let mut origins = Vec::new();
    let mut bytes = 0_u64;
    while let Some(row) = candidates.next()? {
        let size = u64::try_from(row.get::<_, i64>(2)?).map_err(|_| StoreError::InvalidData)?;
        if !origins.is_empty() && bytes.saturating_add(size) > BYTE_CAP {
            break;
        }
        origins.push((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?));
        bytes = bytes.saturating_add(size);
        if bytes > BYTE_CAP {
            break;
        }
    }
    drop(candidates);
    drop(statement);
    if origins.is_empty() {
        return Ok(None);
    }
    let mut deletion = tx.prepare_cached(
        "DELETE FROM tool_summaries WHERE task_id=?1 AND entry_seq=?2 AND item_index=?3",
    )?;
    let mut count = 0;
    for (entry, item) in origins {
        count += deletion.execute(params![task, entry, item])?;
    }
    Ok(Some(CollectionProgress::new(
        CollectionStage::Summaries,
        count,
        bytes,
    )))
}

fn delete_mcp_secret_batch(
    tx: &Transaction<'_>,
    account: AccountId,
) -> StoreResult<Option<CollectionProgress>> {
    let mut statement = tx.prepare_cached(
        "SELECT s.server_id,s.kind,s.name,
        length(s.value)+length(s.target)+length(CAST(s.kind AS BLOB))+length(CAST(s.name AS BLOB))
        FROM mcp_server_secrets s JOIN mcp_servers m ON m.id=s.server_id
        WHERE m.account_id=?1 ORDER BY s.server_id,s.kind,s.name LIMIT ?2",
    )?;
    let mut candidates = statement.query(params![account.0, ROW_CAP as i64])?;
    let mut keys = Vec::new();
    let mut bytes = 0_u64;
    while let Some(row) = candidates.next()? {
        let size = u64::try_from(row.get::<_, i64>(3)?).map_err(|_| StoreError::InvalidData)?;
        if !keys.is_empty() && bytes.saturating_add(size) > BYTE_CAP {
            break;
        }
        keys.push((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ));
        bytes = bytes.saturating_add(size);
        if bytes > BYTE_CAP {
            break;
        }
    }
    drop(candidates);
    drop(statement);
    if keys.is_empty() {
        return Ok(None);
    }
    let mut deletion = tx.prepare_cached(
        "DELETE FROM mcp_server_secrets WHERE server_id=?1 AND kind=?2 AND name=?3",
    )?;
    let mut count = 0;
    for (server, kind, name) in keys {
        count += deletion.execute(params![server, kind, name])?;
    }
    Ok(Some(CollectionProgress::new(
        CollectionStage::AccountMetadata,
        count,
        bytes,
    )))
}

#[cfg(test)]
#[path = "gc_tests.rs"]
mod tests;
