//! Session-owned attachment files. SQLite contains only bounded metadata.
//!
//! Upload operations are immutable and idempotent. A private file is flushed and
//! published before its metadata becomes ready; a retry reconciles that narrow
//! crash window. No database transaction is retained during file I/O.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use pi_ai::{Content, Message};
use pi_coding_agent::{
    SessionMessage,
    session::{EntryKind, SessionEntry},
};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::paths::{private_directory, private_new_file, sync_directory, verify_private_path};
use super::{AccountId, Durability, Store, StoreError, StoreResult};

pub const MAX_ATTACHMENT_BYTES: usize = 30 * 1024 * 1024;
pub const MAX_ATTACHMENT_RANGE_BYTES: usize = 1024 * 1024;
pub const MAX_ATTACHMENTS: usize = 10;
pub const MAX_UPLOAD_IMAGE_BYTES: usize = 10 * 1024 * 1024;
const MAX_GENERATED_IMAGE_BYTES: usize = 20 * 1024 * 1024;
const MAX_PENDING_FILES: usize = 64;
const MAX_CANCELLATION_TOMBSTONES: i64 = 4096;
const MAX_PENDING_BYTES: u64 = 300 * 1024 * 1024;
const STAGING_TTL_MS: i64 = 24 * 60 * 60 * 1000;
pub const ATTACHMENT_SOURCE_PREFIX: &str = "maple-attachment://";
pub const UNAVAILABLE_IMAGE_SOURCE: &str = "maple-attachment-unavailable:retired";

#[derive(Default)]
pub(crate) struct AttachmentLocks {
    held: Mutex<HashMap<String, Arc<AtomicBool>>>,
}

impl AttachmentLocks {
    fn acquire(self: &Arc<Self>, key: String) -> StoreResult<UploadLease> {
        let mut held = self.held.lock().map_err(|_| StoreError::InvalidData)?;
        if held.contains_key(&key) {
            return Err(StoreError::Busy);
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        held.insert(key.clone(), Arc::clone(&cancelled));
        Ok(UploadLease {
            locks: Arc::clone(self),
            key,
            cancelled,
        })
    }
    fn cancel(&self, key: &str) -> StoreResult<()> {
        if let Some(cancelled) = self
            .held
            .lock()
            .map_err(|_| StoreError::InvalidData)?
            .get(key)
        {
            cancelled.store(true, Ordering::Release);
        }
        Ok(())
    }
}

struct UploadLease {
    locks: Arc<AttachmentLocks>,
    key: String,
    cancelled: Arc<AtomicBool>,
}
impl UploadLease {
    fn check(&self) -> StoreResult<()> {
        if self.cancelled.load(Ordering::Acquire) {
            Err(StoreError::Conflict)
        } else {
            Ok(())
        }
    }
}
impl Drop for UploadLease {
    fn drop(&mut self) {
        self.locks
            .held
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&self.key);
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImageUpload {
    pub name: String,
    pub data_url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttachmentUploadMetadata {
    pub name: String,
    pub mime_type: String,
    pub size: u64,
    /// Lower-case SHA-256 hex of the raw bytes, checked again while streaming.
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoredAttachment {
    pub id: String,
    pub name: String,
    pub mime_type: String,
    pub source: String,
    pub size: u64,
    pub execution_path: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "attachment", rename_all = "camelCase")]
pub enum AttachmentUploadStatus {
    Missing,
    Uploading,
    Complete(StoredAttachment),
    Cancelled,
}

pub enum AttachmentUploadStart {
    Complete(StoredAttachment),
    Writing(Box<AttachmentUploadWriter>),
}

pub struct AttachmentUploadWriter {
    account: AccountId,
    task_uuid: String,
    row: AttachmentRow,
    file: File,
    partial: PathBuf,
    published: PathBuf,
    written: u64,
    hash: Sha256,
    header: Vec<u8>,
    lease: UploadLease,
}

impl AttachmentUploadWriter {
    pub fn is_cancelled(&self) -> bool {
        self.lease.cancelled.load(Ordering::Acquire)
    }

    pub fn write_chunk(&mut self, bytes: &[u8]) -> StoreResult<()> {
        self.lease.check()?;
        if bytes.len() > MAX_ATTACHMENT_RANGE_BYTES
            || self.written.saturating_add(bytes.len() as u64) > self.row.metadata.size
        {
            return Err(StoreError::InvalidInput);
        }
        let header_bytes = (16 - self.header.len()).min(bytes.len());
        self.header.extend_from_slice(&bytes[..header_bytes]);
        self.file.write_all(bytes)?;
        self.hash.update(bytes);
        self.written += bytes.len() as u64;
        Ok(())
    }

    pub fn finish(self, store: &Store) -> StoreResult<StoredAttachment> {
        self.lease.check()?;
        if self.written != self.row.metadata.size
            || hex(&self.hash.clone().finalize()) != self.row.metadata.sha256
        {
            return Err(StoreError::InvalidInput);
        }
        if self.row.metadata.mime_type.starts_with("image/")
            && detected_image_mime(&self.header) != Some(self.row.metadata.mime_type.as_str())
        {
            return Err(StoreError::InvalidInput);
        }
        store.read(|tx| {
            let task = active_task(tx, self.account, &self.task_uuid)?;
            verify_upload_row(tx, task, &self.row, "uploading")
        })?;
        super::keys::sync_key_file(&self.file)?;
        // Windows cannot rename while our writable handle remains open.
        drop(self.file);
        self.lease.check()?;
        publish_file(&self.partial, &self.published)?;
        store.finish_attachment_metadata(self.account, &self.task_uuid, &self.row)?;
        Ok(self.row.descriptor(&self.published))
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct AttachmentRange {
    pub mime: String,
    pub size: u64,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AttachmentCollectionProgress {
    pub done: bool,
    pub files: usize,
    pub bytes: usize,
    pub links: usize,
}

#[derive(Clone)]
struct AttachmentRow {
    id: String,
    operation: String,
    metadata: AttachmentUploadMetadata,
    state: String,
}
impl AttachmentRow {
    fn descriptor(&self, path: &Path) -> StoredAttachment {
        StoredAttachment {
            id: self.id.clone(),
            name: self.metadata.name.clone(),
            mime_type: self.metadata.mime_type.clone(),
            source: format!("{ATTACHMENT_SOURCE_PREFIX}{}", self.id),
            size: self.metadata.size,
            execution_path: path.to_string_lossy().into_owned(),
        }
    }
}

pub(super) struct PreparedEntryImages {
    pub(super) entry: SessionEntry,
    account: AccountId,
    task_uuid: String,
    attachments: Vec<String>,
}
pub(super) struct PreparedEntriesImages {
    pub(super) entries: Vec<PreparedEntryImages>,
}
impl PreparedEntryImages {
    pub(super) fn has_images(&self) -> bool {
        !self.attachments.is_empty()
    }
    pub(super) fn link(&self, tx: &Transaction<'_>, task_id: i64) -> StoreResult<()> {
        if active_task(tx, self.account, &self.task_uuid)? != task_id {
            return Err(StoreError::NotFound);
        }
        pin_tx(tx, task_id, &self.attachments)
    }
}

impl Store {
    pub(super) fn cancel_attachment_writers(&self) -> StoreResult<bool> {
        let held = self
            .attachment_locks
            .held
            .lock()
            .map_err(|_| StoreError::InvalidData)?;
        for cancelled in held.values() {
            cancelled.store(true, Ordering::Release);
        }
        Ok(held.is_empty())
    }

    pub fn begin_attachment_upload(
        &self,
        account: AccountId,
        task_uuid: &str,
        operation_id: &str,
        metadata: &AttachmentUploadMetadata,
    ) -> StoreResult<AttachmentUploadStart> {
        self.begin_attachment_upload_inner(account, task_uuid, operation_id, metadata, false)
    }

    fn begin_attachment_upload_inner(
        &self,
        account: AccountId,
        task_uuid: &str,
        operation_id: &str,
        metadata: &AttachmentUploadMetadata,
        source_image: bool,
    ) -> StoreResult<AttachmentUploadStart> {
        validate_operation(operation_id)?;
        let metadata = normalized_metadata(metadata)?;
        if !source_image
            && metadata.mime_type.starts_with("image/")
            && metadata.size > MAX_UPLOAD_IMAGE_BYTES as u64
        {
            return Err(StoreError::InvalidInput);
        }
        let lease = self
            .attachment_locks
            .acquire(operation_key(account, operation_id))?;
        let row = self.write(Durability::Durable, |tx| {
            let task = active_task(tx, account, task_uuid)?;
            let existing = tx.query_row("SELECT public_id,operation_id,name,mime,size,sha256,state,task_id FROM file_attachments WHERE account_id=?1 AND operation_id=?2", params![account.0,operation_id], |row| Ok((attachment_row(row)?,row.get::<_,i64>(7)?))).optional()?;
            if let Some((row, stored_task)) = existing {
                if stored_task != task || row.metadata != metadata || row.state == "cancelled" { return Err(StoreError::Conflict); }
                return Ok(row);
            }
            let (count,bytes,task_count):(i64,i64,i64) = tx.query_row("SELECT coalesce(sum(state!='cancelled'),0),coalesce(sum(size),0),coalesce(sum(task_id=?1 AND state!='cancelled'),0) FROM file_attachments WHERE bound=0", [task], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
            if !source_image && (count >= MAX_PENDING_FILES as i64 || task_count >= MAX_ATTACHMENTS as i64 || bytes as u64 + metadata.size > MAX_PENDING_BYTES) { return Err(StoreError::Busy); }
            let tombstones:i64=tx.query_row("SELECT count(*) FROM file_attachments WHERE account_id=?1 AND state='cancelled'",[account.0],|row|row.get(0))?;
            if !source_image && tombstones>=MAX_CANCELLATION_TOMBSTONES {return Err(StoreError::Busy);}
            let row = AttachmentRow { id: format!("{:032x}",rand::random::<u128>()), operation: operation_id.to_owned(), metadata: metadata.clone(), state: "uploading".into() };
            tx.execute("INSERT INTO file_attachments(public_id,account_id,task_id,operation_id,name,mime,size,sha256,state,created_ms,updated_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'uploading',?9,?9)", params![row.id,account.0,task,row.operation,row.metadata.name,row.metadata.mime_type,i64::try_from(row.metadata.size).map_err(|_|StoreError::InvalidInput)?,row.metadata.sha256,pi_ai::now_ms()])?;
            Ok(row)
        })?;
        let path = self.attachment_path(account, task_uuid, &row, true)?;
        if row.state == "ready" {
            checked_file(&path, row.metadata.size)?;
            return Ok(AttachmentUploadStart::Complete(row.descriptor(&path)));
        }
        if path.try_exists()? {
            verify_file_digest(&path, &row.metadata)?;
            self.finish_attachment_metadata(account, task_uuid, &row)?;
            return Ok(AttachmentUploadStart::Complete(row.descriptor(&path)));
        }
        let partial = path.parent().ok_or(StoreError::UnsafePath)?.join(".part");
        remove_private_file_if_present(&partial)?;
        let file = private_new_file(&partial)?;
        Ok(AttachmentUploadStart::Writing(Box::new(
            AttachmentUploadWriter {
                account,
                task_uuid: task_uuid.to_owned(),
                row,
                file,
                partial,
                published: path,
                written: 0,
                hash: Sha256::new(),
                header: Vec::with_capacity(16),
                lease,
            },
        )))
    }

    fn finish_attachment_metadata(
        &self,
        account: AccountId,
        task_uuid: &str,
        row: &AttachmentRow,
    ) -> StoreResult<()> {
        self.write(Durability::Durable, |tx| {
            let task = active_task(tx, account, task_uuid)?;
            verify_upload_row(tx, task, row, "uploading")?;
            tx.execute(
                "UPDATE file_attachments SET state='ready',updated_ms=?2 WHERE public_id=?1",
                params![row.id, pi_ai::now_ms()],
            )?;
            Ok(())
        })
    }

    pub fn attachment_upload_status(
        &self,
        account: AccountId,
        task_uuid: &str,
        operation_id: &str,
    ) -> StoreResult<AttachmentUploadStatus> {
        validate_operation(operation_id)?;
        let row = self.read(|tx| {
            let task = active_task(tx,account,task_uuid)?;
            let found = tx.query_row("SELECT public_id,operation_id,name,mime,size,sha256,state,task_id FROM file_attachments WHERE account_id=?1 AND operation_id=?2",params![account.0,operation_id],|row| Ok((attachment_row(row)?,row.get::<_,i64>(7)?))).optional()?;
            match found { Some((_,owner)) if owner != task => Err(StoreError::NotFound), other => Ok(other.map(|(row,_)|row)) }
        })?;
        match row {
            None => Ok(AttachmentUploadStatus::Missing),
            Some(row) if row.state == "cancelled" => Ok(AttachmentUploadStatus::Cancelled),
            Some(row) if row.state == "ready" => {
                let path = self.attachment_path(account, task_uuid, &row, false)?;
                checked_file(&path, row.metadata.size)?;
                Ok(AttachmentUploadStatus::Complete(row.descriptor(&path)))
            }
            Some(_) => Ok(AttachmentUploadStatus::Uploading),
        }
    }

    pub fn cancel_attachment_upload(
        &self,
        account: AccountId,
        task_uuid: &str,
        operation_id: &str,
    ) -> StoreResult<()> {
        validate_operation(operation_id)?;
        self.write(Durability::Durable,|tx| {
            let task=active_task(tx,account,task_uuid)?;
            let existing:Option<(i64,bool)>=tx.query_row("SELECT task_id,bound FROM file_attachments WHERE account_id=?1 AND operation_id=?2",params![account.0,operation_id],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
            match existing {
                Some((owner,_)) if owner!=task => return Err(StoreError::NotFound),
                Some((_,true)) => return Err(StoreError::Conflict),
                Some(_) => {tx.execute("UPDATE file_attachments SET state='cancelled',updated_ms=?3 WHERE account_id=?1 AND operation_id=?2 AND bound=0",params![account.0,operation_id,pi_ai::now_ms()])?;},
                None => {
                    let count:i64=tx.query_row("SELECT count(*) FROM file_attachments WHERE account_id=?1 AND state='cancelled'",[account.0],|row|row.get(0))?;
                    if count>=MAX_CANCELLATION_TOMBSTONES {return Err(StoreError::Busy);}
                    tx.execute("INSERT INTO file_attachments(public_id,account_id,task_id,operation_id,name,mime,size,sha256,state,created_ms,updated_ms) VALUES(?1,?2,?3,?4,'attachment','application/octet-stream',0,?5,'cancelled',?6,?6)",params![format!("{:032x}",rand::random::<u128>()),account.0,task,operation_id,"0".repeat(64),pi_ai::now_ms()])?;
                }
            }
            Ok(())
        })?;
        self.attachment_locks
            .cancel(&operation_key(account, operation_id))?;
        // Cancellation is already durable. Reclaim an idle upload promptly in
        // one bounded step; active writers and retryable filesystem failures
        // remain for maintenance, with their reserved bytes still charged.
        let candidate = self.read(|tx| {
            let task = active_task(tx, account, task_uuid)?;
            Ok(tx.query_row(
                "SELECT public_id,operation_id,name,mime,size,sha256,state FROM file_attachments WHERE task_id=?1 AND operation_id=?2 AND state='cancelled' AND size>0",
                params![task, operation_id], attachment_row,
            ).optional()?)
        })?;
        if let Some(row) = candidate {
            let _ = self.collect_attachment_file(account, task_uuid, &row, false);
        }
        Ok(())
    }

    /// Refresh persisted model references after a store-generation recovery.
    /// This returns authorized metadata only; reads still use checked file APIs.
    pub fn attachment_metadata(
        &self,
        account: AccountId,
        task_uuid: &str,
        id: &str,
    ) -> StoreResult<StoredAttachment> {
        validate_id(id)?;
        let row = self.read(|tx| lookup_ready(tx, active_task(tx, account, task_uuid)?, id))?;
        let path = self.attachment_path_metadata(account, task_uuid, &row)?;
        Ok(row.descriptor(&path))
    }

    pub fn resolve_attachments(
        &self,
        account: AccountId,
        task_uuid: &str,
        ids: &[String],
    ) -> StoreResult<Vec<StoredAttachment>> {
        if ids.len() > MAX_ATTACHMENTS || ids.iter().collect::<HashSet<_>>().len() != ids.len() {
            return Err(StoreError::InvalidInput);
        }
        ids.iter()
            .map(|id| {
                validate_id(id)?;
                let row = self.read(|tx| {
                    let task = active_task(tx, account, task_uuid)?;
                    lookup_ready(tx, task, id)
                })?;
                let path = self.attachment_path(account, task_uuid, &row, false)?;
                checked_file(&path, row.metadata.size)?;
                Ok(row.descriptor(&path))
            })
            .collect()
    }

    pub fn pin_attachments(
        &self,
        account: AccountId,
        task_uuid: &str,
        ids: &[String],
    ) -> StoreResult<()> {
        if ids.len() > MAX_ATTACHMENTS {
            return Err(StoreError::InvalidInput);
        }
        let mut rows = self.read(|tx| {
            let task = active_task(tx, account, task_uuid)?;
            ids.iter()
                .map(|id| {
                    validate_id(id)?;
                    lookup_ready(tx, task, id)
                })
                .collect::<StoreResult<Vec<_>>>()
        })?;
        rows.sort_by(|a, b| a.operation.cmp(&b.operation));
        rows.dedup_by(|a, b| a.operation == b.operation);
        let _leases = rows
            .iter()
            .map(|row| {
                self.attachment_locks
                    .acquire(operation_key(account, &row.operation))
            })
            .collect::<StoreResult<Vec<_>>>()?;
        for row in &rows {
            checked_file(
                &self.attachment_path(account, task_uuid, row, false)?,
                row.metadata.size,
            )?;
        }
        self.write(Durability::Durable, |tx| {
            let task = active_task(tx, account, task_uuid)?;
            pin_tx(tx, task, ids)
        })
    }

    pub fn store_image_uploads(
        &self,
        account: AccountId,
        task_uuid: &str,
        uploads: &[ImageUpload],
    ) -> StoreResult<Vec<StoredAttachment>> {
        if uploads.len() > MAX_ATTACHMENTS {
            return Err(StoreError::InvalidInput);
        }
        let parsed = uploads
            .iter()
            .map(parse_upload)
            .collect::<StoreResult<Vec<_>>>()?;
        parsed
            .into_iter()
            .map(|(metadata, bytes)| {
                self.import_attachment_bytes(
                    account,
                    task_uuid,
                    &format!("{:032x}", rand::random::<u128>()),
                    metadata,
                    &bytes,
                    false,
                )
            })
            .collect()
    }

    fn import_attachment_bytes(
        &self,
        account: AccountId,
        task_uuid: &str,
        operation: &str,
        metadata: AttachmentUploadMetadata,
        bytes: &[u8],
        source_image: bool,
    ) -> StoreResult<StoredAttachment> {
        match self.begin_attachment_upload_inner(
            account,
            task_uuid,
            operation,
            &metadata,
            source_image,
        )? {
            AttachmentUploadStart::Complete(value) => Ok(value),
            AttachmentUploadStart::Writing(mut writer) => {
                for chunk in bytes.chunks(MAX_ATTACHMENT_RANGE_BYTES) {
                    writer.write_chunk(chunk)?;
                }
                writer.finish(self)
            }
        }
    }

    pub fn read_attachment(
        &self,
        account: AccountId,
        task_uuid: &str,
        public_id: &str,
        offset: u64,
        len: usize,
    ) -> StoreResult<AttachmentRange> {
        validate_id(public_id)?;
        if len > MAX_ATTACHMENT_RANGE_BYTES {
            return Err(StoreError::InvalidInput);
        }
        let row =
            self.read(|tx| lookup_ready(tx, active_task(tx, account, task_uuid)?, public_id))?;
        if offset > row.metadata.size {
            return Err(StoreError::InvalidInput);
        }
        let path = self.attachment_path(account, task_uuid, &row, false)?;
        let mut file = checked_file(&path, row.metadata.size)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut data = vec![0; len.min((row.metadata.size - offset) as usize)];
        file.read_exact(&mut data)?;
        Ok(AttachmentRange {
            mime: row.metadata.mime_type,
            size: row.metadata.size,
            data,
        })
    }

    /// Call only on the effective context after compaction and image filtering.
    /// A missing file is unavailable media; malformed metadata/source stays an error.
    pub fn materialize_message_images(
        &self,
        account: AccountId,
        task_uuid: &str,
        messages: &mut [Message],
    ) -> StoreResult<()> {
        let mut prepared = messages.to_vec();
        for message in &mut prepared {
            let content = match message {
                Message::User(m) => &mut m.content,
                Message::ToolResult(m) => &mut m.content,
                _ => continue,
            };
            for block in content {
                let Content::Image(image) = block else {
                    continue;
                };
                if image.data == UNAVAILABLE_IMAGE_SOURCE {
                    *block = Content::text("Image no longer available.");
                    continue;
                }
                let Some(id) = image.data.strip_prefix(ATTACHMENT_SOURCE_PREFIX) else {
                    continue;
                };
                validate_id(id).map_err(|_| StoreError::InvalidData)?;
                let row =
                    self.read(|tx| lookup_ready(tx, active_task(tx, account, task_uuid)?, id))?;
                if image.mime_type != row.metadata.mime_type {
                    return Err(StoreError::InvalidData);
                }
                let bytes = (|| {
                    let path = self.attachment_path(account, task_uuid, &row, false)?;
                    let mut file = checked_file(&path, row.metadata.size)?;
                    let mut bytes = vec![0; row.metadata.size as usize];
                    file.read_exact(&mut bytes)?;
                    if hex(&Sha256::digest(&bytes)) != row.metadata.sha256 {
                        return Err(StoreError::InvalidData);
                    }
                    Ok(bytes)
                })();
                match bytes {
                    Ok(bytes) => image.data = STANDARD.encode(bytes),
                    Err(StoreError::NotFound) => {
                        *block = Content::text("Image no longer available.")
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        messages.clone_from_slice(&prepared);
        Ok(())
    }

    pub(super) fn prepare_entry_images(
        &self,
        account: AccountId,
        task_uuid: &str,
        entry: &SessionEntry,
    ) -> StoreResult<PreparedEntryImages> {
        self.prepare_entries_images(account, task_uuid, std::slice::from_ref(entry))?
            .entries
            .pop()
            .ok_or(StoreError::InvalidData)
    }
    pub(super) fn prepare_entries_images(
        &self,
        account: AccountId,
        task_uuid: &str,
        entries: &[SessionEntry],
    ) -> StoreResult<PreparedEntriesImages> {
        self.read(|tx| active_task(tx, account, task_uuid))?;
        let mut prepared = Vec::with_capacity(entries.len());
        for source in entries {
            let mut entry = source.clone();
            let mut ids = Vec::new();
            let mut ordinal = 0;
            let entry_id = entry.id.clone();
            visit_images(&mut entry, |image| {
                ordinal += 1;
                if image.data == UNAVAILABLE_IMAGE_SOURCE {
                    return Ok(());
                }
                if let Some(id) = image.data.strip_prefix(ATTACHMENT_SOURCE_PREFIX) {
                    validate_id(id).map_err(|_| StoreError::InvalidData)?;
                    let bound = self.read(|tx| {
                        let task = active_task(tx, account, task_uuid)?;
                        let stored = lookup_ready(tx, task, id)?;
                        if stored.metadata.mime_type != image.mime_type {
                            return Err(StoreError::InvalidData);
                        }
                        Ok(tx.query_row(
                            "SELECT bound FROM file_attachments WHERE public_id=?1",
                            [id],
                            |row| row.get::<_, bool>(0),
                        )?)
                    })?;
                    if !bound {
                        self.pin_attachments(account, task_uuid, &[id.to_owned()])?;
                    }
                    ids.push(id.to_owned());
                    return Ok(());
                }
                if image.data.len() > MAX_GENERATED_IMAGE_BYTES.div_ceil(3) * 4
                    || !image.mime_type.starts_with("image/")
                {
                    image.data = UNAVAILABLE_IMAGE_SOURCE.into();
                    return Ok(());
                }
                let encoded_data = if image.data.starts_with("data:") {
                    let expected = format!("data:{};base64,", image.mime_type);
                    match image.data.strip_prefix(&expected) {
                        Some(data) => data,
                        None => {
                            image.data = UNAVAILABLE_IMAGE_SOURCE.into();
                            return Ok(());
                        }
                    }
                } else {
                    image.data.as_str()
                };
                let encoded: String = encoded_data
                    .chars()
                    .filter(|character| !character.is_ascii_whitespace())
                    .collect();
                let bytes = match STANDARD.decode(&encoded) {
                    Ok(bytes) if !bytes.is_empty() && bytes.len() <= MAX_GENERATED_IMAGE_BYTES => {
                        bytes
                    }
                    _ => {
                        image.data = UNAVAILABLE_IMAGE_SOURCE.into();
                        return Ok(());
                    }
                };
                if detected_image_mime(&bytes) != Some(image.mime_type.as_str()) {
                    image.data = UNAVAILABLE_IMAGE_SOURCE.into();
                    return Ok(());
                }
                let sha256 = hex(&Sha256::digest(&bytes));
                let mut identity = Sha256::new();
                for part in [
                    task_uuid,
                    entry_id.as_str(),
                    ordinal.to_string().as_str(),
                    sha256.as_str(),
                ] {
                    identity.update((part.len() as u64).to_le_bytes());
                    identity.update(part.as_bytes());
                }
                let operation = format!("source-{}", hex(&identity.finalize()));
                let metadata = AttachmentUploadMetadata {
                    name: format!("image-{ordinal}.{}", image_extension(&image.mime_type)),
                    mime_type: image.mime_type.clone(),
                    size: bytes.len() as u64,
                    sha256,
                };
                if normalized_metadata(&metadata).is_err() {
                    image.data = UNAVAILABLE_IMAGE_SOURCE.into();
                    return Ok(());
                }
                let stored = self.import_attachment_bytes(
                    account, task_uuid, &operation, metadata, &bytes, true,
                )?;
                self.pin_attachments(account, task_uuid, std::slice::from_ref(&stored.id))?;
                image.data = stored.source;
                ids.push(stored.id);
                Ok(())
            })?;
            prepared.push(PreparedEntryImages {
                entry,
                account,
                task_uuid: task_uuid.to_owned(),
                attachments: ids,
            });
        }
        Ok(PreparedEntriesImages { entries: prepared })
    }

    fn attachment_path_metadata(
        &self,
        account: AccountId,
        task_uuid: &str,
        row: &AttachmentRow,
    ) -> StoreResult<PathBuf> {
        validate_id(&row.id).map_err(|_| StoreError::InvalidData)?;
        if !safe_component(task_uuid) || normalized_name(&row.metadata.name) != row.metadata.name {
            return Err(StoreError::InvalidData);
        }
        let store_id: Vec<u8> = self.read(|tx| {
            Ok(tx.query_row(
                "SELECT value FROM store_meta WHERE key='store_id'",
                [],
                |row| row.get(0),
            )?)
        })?;
        if store_id.len() != 16 {
            return Err(StoreError::InvalidData);
        }
        Ok(self
            .paths
            .attachments()
            .join(hex(&store_id))
            .join(account.0.to_string())
            .join(task_uuid)
            .join(&row.id)
            .join(&row.metadata.name))
    }

    fn attachment_path(
        &self,
        account: AccountId,
        task_uuid: &str,
        row: &AttachmentRow,
        create: bool,
    ) -> StoreResult<PathBuf> {
        let path = self.attachment_path_metadata(account, task_uuid, row)?;
        let relative = path
            .strip_prefix(self.paths.root())
            .map_err(|_| StoreError::UnsafePath)?;
        let mut checked = self.paths.root().to_owned();
        verify_private_path(&checked, true)?;
        let components = relative.components().collect::<Vec<_>>();
        for component in &components[..components.len() - 1] {
            checked.push(component);
            if create {
                private_directory(&checked)?;
            } else {
                verify_private_path(&checked, true).map_err(missing_file)?;
            }
        }
        Ok(path)
    }

    pub(crate) fn collect_unbound_attachments_step(
        &self,
    ) -> StoreResult<AttachmentCollectionProgress> {
        let candidate=self.read_bulk(|tx| {
            Ok(tx.query_row("SELECT a.account_id,t.uuid,a.public_id,a.operation_id,a.name,a.mime,a.size,a.sha256,a.state FROM file_attachments a JOIN tasks t ON t.id=a.task_id JOIN accounts c ON c.id=a.account_id WHERE a.bound=0 AND ((a.state='cancelled' AND a.size>0) OR a.updated_ms<?1) AND c.deleting_ms IS NULL AND t.deleted_ms IS NULL ORDER BY a.updated_ms LIMIT 1",[pi_ai::now_ms().saturating_sub(STAGING_TTL_MS)],|row|Ok((AccountId(row.get(0)?),row.get::<_,String>(1)?,attachment_row_offset(row,2)?))).optional()?)
        })?;
        let Some((account, task, row)) = candidate else {
            return Ok(AttachmentCollectionProgress {
                done: true,
                ..Default::default()
            });
        };
        self.collect_attachment_file(account, &task, &row, false)
    }

    pub(super) fn collect_task_attachments_step(
        &self,
        account: AccountId,
        task_uuid: &str,
    ) -> StoreResult<AttachmentCollectionProgress> {
        let candidate=self.read_bulk(|tx| {
            let task=deleting_task(tx,account,task_uuid)?;
            Ok(tx.query_row("SELECT public_id,operation_id,name,mime,size,sha256,state FROM file_attachments WHERE task_id=?1 LIMIT 1",[task],attachment_row).optional()?)
        })?;
        let Some(row) = candidate else {
            return Ok(AttachmentCollectionProgress {
                done: true,
                ..Default::default()
            });
        };
        self.collect_attachment_file(account, task_uuid, &row, true)
    }

    fn collect_attachment_file(
        &self,
        account: AccountId,
        task_uuid: &str,
        row: &AttachmentRow,
        deleting: bool,
    ) -> StoreResult<AttachmentCollectionProgress> {
        let _lease = match self
            .attachment_locks
            .acquire(operation_key(account, &row.operation))
        {
            Ok(value) => value,
            Err(StoreError::Busy) => return Ok(AttachmentCollectionProgress::default()),
            Err(error) => return Err(error),
        };
        self.read(|tx| {
            let task=if deleting {deleting_task(tx,account,task_uuid)?} else {active_task(tx,account,task_uuid)?};
            let collectable:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM file_attachments WHERE task_id=?1 AND public_id=?2 AND (?3 OR (bound=0 AND ((state='cancelled' AND size>0) OR updated_ms<?4))))",params![task,row.id,deleting,pi_ai::now_ms().saturating_sub(STAGING_TTL_MS)],|row|row.get(0))?;
            if !collectable {return Err(StoreError::Conflict);} Ok(())
        })?;
        let cleanup = match self.attachment_path(account, task_uuid, row, false) {
            Ok(path) => super::attachment_cleanup::collect_owned_attachment_tree_step(
                path.parent().ok_or(StoreError::UnsafePath)?,
            )?,
            Err(StoreError::NotFound) => super::attachment_cleanup::AttachmentCleanupProgress {
                done: true,
                entries: 0,
            },
            Err(error) => return Err(error),
        };
        if !cleanup.done {
            return Ok(AttachmentCollectionProgress {
                files: cleanup.entries,
                ..Default::default()
            });
        }
        self.write(Durability::Durable, |tx| {
            let task = if deleting {
                deleting_task(tx, account, task_uuid)?
            } else {
                active_task(tx, account, task_uuid)?
            };
            let (state, updated): (String, i64) = tx.query_row(
                "SELECT state,updated_ms FROM file_attachments WHERE public_id=?1",
                [&row.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let links = if !deleting && state == "cancelled" {
                // Keep the cancellation identity after deleting its bytes, so
                // a delayed sender cannot resurrect the cancelled operation.
                let expired = updated < pi_ai::now_ms().saturating_sub(STAGING_TTL_MS);
                if expired {
                    tx.execute(
                        "DELETE FROM file_attachments WHERE task_id=?1 AND public_id=?2",
                        params![task, row.id],
                    )?
                } else {
                    tx.execute(
                        "UPDATE file_attachments SET size=0 WHERE task_id=?1 AND public_id=?2",
                        params![task, row.id],
                    )?
                }
            } else {
                tx.execute(
                    "DELETE FROM file_attachments WHERE task_id=?1 AND public_id=?2",
                    params![task, row.id],
                )?
            };
            Ok(AttachmentCollectionProgress {
                links,
                files: cleanup.entries,
                bytes: row.metadata.size as usize,
                ..Default::default()
            })
        })
    }
}

fn operation_key(account: AccountId, operation: &str) -> String {
    format!("{}:{operation}", account.0)
}
fn validate_operation(value: &str) -> StoreResult<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        Err(StoreError::InvalidInput)
    } else {
        Ok(())
    }
}
fn validate_id(value: &str) -> StoreResult<()> {
    if value.len() == 32
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Ok(())
    } else {
        Err(StoreError::InvalidInput)
    }
}
fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn normalized_name(value: &str) -> String {
    let last = value.rsplit(['/', '\\']).next().unwrap_or_default();
    let mut name = String::new();
    for c in last
        .trim()
        .chars()
        .filter(|c| !c.is_control() && !"<>:\"|?*".contains(*c))
    {
        if name.len() + c.len_utf8() > 180 {
            break;
        }
        name.push(c);
    }
    let name = name.trim_end_matches(['.', ' ']).to_owned();
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    if name.is_empty()
        || name.eq_ignore_ascii_case(".part")
        || matches!(
            stem.as_str(),
            "CON"
                | "PRN"
                | "AUX"
                | "NUL"
                | "COM1"
                | "COM2"
                | "COM3"
                | "COM4"
                | "COM5"
                | "COM6"
                | "COM7"
                | "COM8"
                | "COM9"
                | "LPT1"
                | "LPT2"
                | "LPT3"
                | "LPT4"
                | "LPT5"
                | "LPT6"
                | "LPT7"
                | "LPT8"
                | "LPT9"
        )
    {
        format!("attachment-{name}")
    } else {
        name
    }
}
fn normalized_metadata(value: &AttachmentUploadMetadata) -> StoreResult<AttachmentUploadMetadata> {
    if value.size > MAX_ATTACHMENT_BYTES as u64
        || value.sha256.len() != 64
        || !value
            .sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || value.mime_type.len() > 128
        || !value.mime_type.contains('/')
        || !value
            .mime_type
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/!#$&^_.+-".contains(&b))
    {
        return Err(StoreError::InvalidInput);
    }
    Ok(AttachmentUploadMetadata {
        name: normalized_name(&value.name),
        ..value.clone()
    })
}
fn attachment_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AttachmentRow> {
    attachment_row_offset(row, 0)
}
fn attachment_row_offset(
    row: &rusqlite::Row<'_>,
    offset: usize,
) -> rusqlite::Result<AttachmentRow> {
    let attachment = AttachmentRow {
        id: row.get(offset)?,
        operation: row.get(offset + 1)?,
        metadata: AttachmentUploadMetadata {
            name: row.get(offset + 2)?,
            mime_type: row.get(offset + 3)?,
            size: {
                let size = row.get::<_, i64>(offset + 4)?;
                u64::try_from(size)
                    .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(offset + 4, size))?
            },
            sha256: row.get(offset + 5)?,
        },
        state: row.get(offset + 6)?,
    };
    if validate_id(&attachment.id).is_err()
        || validate_operation(&attachment.operation).is_err()
        || normalized_metadata(&attachment.metadata).is_err()
        || normalized_name(&attachment.metadata.name) != attachment.metadata.name
        || !matches!(
            attachment.state.as_str(),
            "uploading" | "ready" | "cancelled"
        )
    {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            offset,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid attachment metadata",
            )),
        ));
    }
    Ok(attachment)
}
fn active_task(tx: &Transaction<'_>, account: AccountId, uuid: &str) -> StoreResult<i64> {
    Ok(tx.query_row("SELECT t.id FROM tasks t JOIN accounts a ON a.id=t.account_id WHERE t.uuid=?1 AND t.account_id=?2 AND t.deleted_ms IS NULL AND a.deleting_ms IS NULL",params![uuid,account.0],|row|row.get(0))?)
}
fn deleting_task(tx: &Transaction<'_>, account: AccountId, uuid: &str) -> StoreResult<i64> {
    Ok(tx.query_row("SELECT t.id FROM tasks t JOIN accounts a ON a.id=t.account_id WHERE t.uuid=?1 AND t.account_id=?2 AND t.deleted_ms IS NOT NULL AND (a.deletion_phase IS NULL OR a.deletion_phase='confirmed')",params![uuid,account.0],|row|row.get(0))?)
}
fn lookup_ready(tx: &Transaction<'_>, task: i64, id: &str) -> StoreResult<AttachmentRow> {
    Ok(tx.query_row("SELECT public_id,operation_id,name,mime,size,sha256,state FROM file_attachments WHERE task_id=?1 AND public_id=?2 AND state='ready'",params![task,id],attachment_row)?)
}
fn verify_upload_row(
    tx: &Transaction<'_>,
    task: i64,
    row: &AttachmentRow,
    state: &str,
) -> StoreResult<()> {
    let actual=tx.query_row("SELECT public_id,operation_id,name,mime,size,sha256,state FROM file_attachments WHERE task_id=?1 AND public_id=?2",params![task,row.id],attachment_row)?;
    if actual.operation != row.operation || actual.metadata != row.metadata || actual.state != state
    {
        Err(StoreError::Conflict)
    } else {
        Ok(())
    }
}
fn pin_tx(tx: &Transaction<'_>, task: i64, ids: &[String]) -> StoreResult<()> {
    for id in ids {
        validate_id(id)?;
        lookup_ready(tx, task, id)?;
    }
    for id in ids {
        tx.execute(
            "UPDATE file_attachments SET bound=1 WHERE task_id=?1 AND public_id=?2",
            params![task, id],
        )?;
    }
    Ok(())
}
fn missing_file(error: StoreError) -> StoreError {
    match error {
        StoreError::Io {
            kind: std::io::ErrorKind::NotFound,
        } => StoreError::NotFound,
        other => other,
    }
}
fn checked_file(path: &Path, size: u64) -> StoreResult<File> {
    checked_file_access(path, size, false)
}
fn checked_file_access(path: &Path, size: u64, writable: bool) -> StoreResult<File> {
    verify_private_path(path, false).map_err(missing_file)?;
    let mut options = OpenOptions::new();
    options.read(true).write(writable);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options
        .open(path)
        .map_err(StoreError::from)
        .map_err(missing_file)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() != size {
        return Err(StoreError::InvalidData);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let named = fs::symlink_metadata(path)?;
        if meta.dev() != named.dev() || meta.ino() != named.ino() || meta.nlink() != 1 {
            return Err(StoreError::UnsafePath);
        }
    }
    verify_private_path(path, false)?;
    Ok(file)
}
fn verify_file_digest(path: &Path, metadata: &AttachmentUploadMetadata) -> StoreResult<()> {
    let mut file = checked_file_access(path, metadata.size, true)?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0; MAX_ATTACHMENT_RANGE_BYTES];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    if hex(&hash.finalize()) != metadata.sha256 {
        return Err(StoreError::InvalidData);
    }
    super::keys::sync_key_file(&file)?;
    sync_directory(path.parent().ok_or(StoreError::UnsafePath)?)?;
    Ok(())
}
fn publish_file(partial: &Path, published: &Path) -> StoreResult<()> {
    if published.try_exists()? {
        return Err(StoreError::Conflict);
    }
    super::keys::replace_key_file(partial, published)?;
    sync_directory(published.parent().ok_or(StoreError::UnsafePath)?)?;
    Ok(())
}
fn remove_private_file_if_present(path: &Path) -> StoreResult<()> {
    match verify_private_path(path, false) {
        Ok(()) => fs::remove_file(path).map_err(StoreError::from),
        Err(StoreError::Io {
            kind: std::io::ErrorKind::NotFound,
        }) => Ok(()),
        Err(error) => Err(error),
    }
}
fn image_extension(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "image",
    }
}
fn detected_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else {
        None
    }
}

fn parse_upload(upload: &ImageUpload) -> StoreResult<(AttachmentUploadMetadata, Vec<u8>)> {
    let (header, data) = upload
        .data_url
        .split_once(',')
        .ok_or(StoreError::InvalidInput)?;
    let mime = header
        .strip_prefix("data:")
        .and_then(|h| h.strip_suffix(";base64"))
        .ok_or(StoreError::InvalidInput)?;
    if !mime.starts_with("image/")
        || data.is_empty()
        || data.len() > MAX_UPLOAD_IMAGE_BYTES.div_ceil(3) * 4
    {
        return Err(StoreError::InvalidInput);
    }
    let bytes = STANDARD
        .decode(data)
        .map_err(|_| StoreError::InvalidInput)?;
    if bytes.len() > MAX_UPLOAD_IMAGE_BYTES {
        return Err(StoreError::InvalidInput);
    }
    let detected = detected_image_mime(&bytes).ok_or(StoreError::InvalidInput)?;
    if mime != detected && !(mime == "image/jpg" && detected == "image/jpeg") {
        return Err(StoreError::InvalidInput);
    }
    let metadata = AttachmentUploadMetadata {
        name: upload.name.clone(),
        mime_type: detected.into(),
        size: bytes.len() as u64,
        sha256: hex(&Sha256::digest(&bytes)),
    };
    Ok((normalized_metadata(&metadata)?, bytes))
}

/// Stored source is reference-only. Unlike incoming provider content, a raw
/// image in a v4 source entry is a corrupt storage invariant, not an upload.
pub(super) fn validate_stored_entry_images(entry: &SessionEntry) -> StoreResult<()> {
    for image in entry_images(entry) {
        if image.data == UNAVAILABLE_IMAGE_SOURCE {
            continue;
        }
        let id = image
            .data
            .strip_prefix(ATTACHMENT_SOURCE_PREFIX)
            .ok_or(StoreError::InvalidData)?;
        validate_id(id).map_err(|_| StoreError::InvalidData)?;
    }
    Ok(())
}

pub(super) fn entry_image_sizes_tx(
    tx: &Transaction<'_>,
    task: super::tasks::TaskKey,
    entry: &SessionEntry,
) -> StoreResult<Vec<usize>> {
    entry_images(entry)
        .map(|image| {
            let size = if image.data == UNAVAILABLE_IMAGE_SOURCE {
                0
            } else {
                let id = image
                    .data
                    .strip_prefix(ATTACHMENT_SOURCE_PREFIX)
                    .ok_or(StoreError::InvalidData)?;
                validate_id(id).map_err(|_| StoreError::InvalidData)?;
                lookup_ready(tx, task.0, id)?.metadata.size as usize
            };
            size.div_ceil(3)
                .checked_mul(4)
                .ok_or(StoreError::InvalidData)
        })
        .collect()
}

fn entry_images(entry: &SessionEntry) -> impl Iterator<Item = &pi_ai::ImageContent> {
    let content = match &entry.kind {
        EntryKind::Message {
            message: SessionMessage::Llm(Message::User(message)),
        } => Some(&message.content),
        EntryKind::Message {
            message: SessionMessage::Llm(Message::ToolResult(message)),
        } => Some(&message.content),
        EntryKind::Message {
            message: SessionMessage::Custom(message),
        } => Some(&message.content),
        EntryKind::CustomMessage { content, .. } => Some(content),
        EntryKind::ContextEdit { replacement, .. } => replacement.as_ref(),
        _ => None,
    };
    content
        .into_iter()
        .flatten()
        .filter_map(|block| match block {
            Content::Image(image) => Some(image),
            _ => None,
        })
}

fn visit_images(
    entry: &mut SessionEntry,
    mut visit: impl FnMut(&mut pi_ai::ImageContent) -> StoreResult<()>,
) -> StoreResult<()> {
    let content = match &mut entry.kind {
        EntryKind::Message {
            message: SessionMessage::Llm(Message::User(message)),
        } => Some(&mut message.content),
        EntryKind::Message {
            message: SessionMessage::Llm(Message::ToolResult(message)),
        } => Some(&mut message.content),
        EntryKind::Message {
            message: SessionMessage::Custom(message),
        } => Some(&mut message.content),
        EntryKind::CustomMessage { content, .. } => Some(content),
        EntryKind::ContextEdit { replacement, .. } => replacement.as_mut(),
        _ => None,
    };
    if let Some(content) = content {
        for block in content {
            if let Content::Image(image) = block {
                visit(image)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "attachment_tests.rs"]
mod tests;
