//! Pi's append-only source tree, persisted with its timeline and cached facts.
//!
//! Retrying a partial session verifies its real retained suffix. It never
//! replaces an older prefix, and no synthetic snapshot entry reaches SQLite.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::Arc;

use pi_agent_core::AgentMessage;
use pi_ai::{Message, SystemMessage};
use pi_coding_agent::SessionMessage;
use pi_coding_agent::compaction::estimate_tokens;
use pi_coding_agent::session::{
    EntryKind, SessionBase, SessionEntry, SessionHeader, SessionManager, SessionProjection,
    entry_messages, project_with_base,
};
use pi_coding_agent::store::{MemoryStore, SessionStore};
use rusqlite::{OptionalExtension, Transaction, params};

use super::attachments::PreparedEntryImages;
use super::tasks::{TaskKey, task_key_tx, writable_account};
use super::{
    AccountId, Durability, Store, StoreChange, StoreError, StoreResult, StoredRowChange,
    StoredRowIdentity, codec,
};

mod receipts;
pub(crate) use receipts::{SourceReceipt, SourceReceipts, WeakSourceReceipts, message_signature};

const LOAD_BATCH: i64 = 128;
const LOAD_BATCH_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionLoad {
    /// Source history, for an explicit whole-tree operation or parity check.
    #[cfg(any(test, feature = "store-bench"))]
    Full,
    /// Model context, from the latest compaction boundary and its typed base.
    Context,
}

pub(crate) struct LoadedSession {
    pub(crate) header: Option<SessionHeader>,
    pub(crate) base: SessionBase,
    pub(crate) entries: Vec<SessionEntry>,
    pub(crate) store: SqliteSessionStore,
}

impl LoadedSession {
    pub(crate) fn source_receipts(&self) -> SourceReceipts {
        self.store.receipts.clone()
    }

    pub(crate) fn into_manager(self, cwd: &str) -> SessionManager {
        match self.header {
            Some(header) => {
                SessionManager::open_partial(header, self.base, self.entries, Box::new(self.store))
            }
            None => {
                SessionManager::create_with_id(self.store.uuid.clone(), cwd, Box::new(self.store))
            }
        }
    }
}

/// One adapter belongs to one loaded session. All task/account checks are
/// repeated inside the transaction that performs the effect.
pub(crate) struct SqliteSessionStore {
    store: Arc<Store>,
    account: AccountId,
    uuid: String,
    task: TaskKey,
    header: Option<SessionHeader>,
    state: SessionState,
    receipts: SourceReceipts,
}

#[cfg_attr(feature = "store-bench", derive(Clone))]
struct SessionState {
    base: SessionBase,
    entries: Vec<SessionEntry>,
    index: HashMap<String, usize>,
    cache: ContextCache,
    message_count: u64,
    last_seq: i64,
}

#[cfg(feature = "store-bench")]
#[path = "session_fixture.rs"]
pub(super) mod fixture;

#[derive(Clone, Default)]
struct ContextCache {
    model: Option<String>,
    thinking: String,
    system: Option<SystemMessage>,
    non_system_estimate: u64,
    measured: Option<u64>,
    after_measurement: u64,
    has_compaction: bool,
    measured_after_compaction: bool,
}

impl ContextCache {
    fn of(projection: &SessionProjection, branch: &[&SessionEntry]) -> Self {
        let compaction = branch
            .iter()
            .rposition(|entry| matches!(entry.kind, EntryKind::Compaction { .. }));
        let changed = branch.iter().rposition(|entry| {
            matches!(
                entry.kind,
                EntryKind::ContextEdit { .. } | EntryKind::Compaction { .. }
            )
        });
        let measuring = projection
            .messages
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, message)| measurement(message).map(|tokens| (i, tokens)));
        let source_position = measuring.and_then(|(i, _)| {
            let mut end = 0;
            let source = projection.entries.iter().find(|entry| {
                end += entry.messages.len();
                i < end
            })?;
            branch.iter().position(|entry| entry.id == source.entry_id)
        });
        let valid = source_position > changed;
        let measured_after_compaction = compaction.is_some_and(|at| {
            projection.entries.iter().any(|entry| {
                entry.messages.iter().any(|m| measurement(m).is_some())
                    && branch[at + 1..]
                        .iter()
                        .any(|source| source.id == entry.entry_id)
            })
        });
        let system = pi_ai::transcript::current_system_message(
            projection
                .messages
                .iter()
                .filter_map(AgentMessage::as_message),
        );
        let non_system_estimate = projection
            .messages
            .iter()
            .filter(|message| !matches!(message.as_message(), Some(Message::System(_))))
            .map(estimate_tokens)
            .sum();
        Self {
            model: projection.model.as_ref().map(|(_, model)| model.clone()),
            thinking: thinking_name(projection.thinking_level),
            system,
            non_system_estimate,
            measured: measuring.filter(|_| valid).map(|(_, tokens)| tokens),
            after_measurement: measuring.filter(|_| valid).map_or(0, |(i, _)| {
                projection.messages[i + 1..]
                    .iter()
                    .map(estimate_tokens)
                    .sum()
            }),
            has_compaction: compaction.is_some(),
            measured_after_compaction,
        }
    }

    fn append(&self, entry: &SessionEntry) -> Self {
        let mut next = self.clone();
        match &entry.kind {
            EntryKind::ModelChange { model_id, .. } => next.model = Some(model_id.clone()),
            EntryKind::ThinkingLevelChange { thinking_level } => {
                next.thinking = thinking_name(*thinking_level)
            }
            EntryKind::Message {
                message: SessionMessage::Llm(Message::Assistant(reply)),
            } => next.model = Some(reply.model.clone()),
            _ => {}
        }
        for message in entry_messages(entry) {
            if let SessionMessage::Llm(Message::System(system)) = &message {
                let mut systems = Vec::with_capacity(2);
                if let Some(previous) = next.system.take() {
                    systems.push(Message::System(previous));
                }
                systems.push(Message::System(system.clone()));
                next.system = pi_ai::transcript::current_system_message(systems.iter());
            } else {
                next.non_system_estimate = next
                    .non_system_estimate
                    .saturating_add(estimate_tokens(&message));
            }
            if let Some(tokens) = measurement(&message) {
                next.measured = Some(tokens);
                next.after_measurement = 0;
                if next.has_compaction {
                    next.measured_after_compaction = true;
                }
            } else if next.measured.is_some() {
                next.after_measurement = next
                    .after_measurement
                    .saturating_add(estimate_tokens(&message));
            }
        }
        next
    }

    fn estimate(&self) -> u64 {
        self.non_system_estimate
            .saturating_add(self.system.as_ref().map_or(0, |system| {
                estimate_tokens(&SessionMessage::Llm(Message::System(system.clone())))
            }))
    }

    fn tokens(&self) -> u64 {
        self.measured.map_or_else(
            || self.estimate(),
            |tokens| tokens.saturating_add(self.after_measurement),
        )
    }

    /// Matches Core::context_usage, including its distinction between a new
    /// post-compaction estimate and one measured by a retained assistant reply.
    fn usage(&self) -> Option<u64> {
        (!self.has_compaction || self.measured_after_compaction).then(|| self.tokens())
    }
}

fn measurement(message: &SessionMessage) -> Option<u64> {
    match message {
        SessionMessage::Llm(Message::Assistant(reply))
            if !reply.is_failure() && reply.usage.context_tokens() > 0 =>
        {
            Some(reply.usage.context_tokens())
        }
        _ => None,
    }
}

fn thinking_name(level: pi_ai::ThinkingLevel) -> String {
    serde_json::to_value(level)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "off".into())
}

fn conversation(entry: &SessionEntry) -> bool {
    matches!(
        &entry.kind,
        EntryKind::Message {
            message: SessionMessage::Llm(Message::User(_) | Message::Assistant(_))
        }
    )
}

fn branch<'a>(
    entries: &'a [SessionEntry],
    index: &HashMap<String, usize>,
    leaf: Option<&str>,
) -> Vec<&'a SessionEntry> {
    let mut result = Vec::new();
    let mut cursor = leaf;
    let mut seen = HashSet::new();
    while let Some(id) = cursor {
        if !seen.insert(id) {
            break;
        }
        let Some(position) = index.get(id) else {
            break;
        };
        let entry = &entries[*position];
        result.push(entry);
        cursor = entry.parent_id.as_deref();
    }
    result.reverse();
    result
}

impl SessionState {
    fn new(
        base: SessionBase,
        entries: Vec<SessionEntry>,
        message_count: u64,
        last_seq: i64,
    ) -> Self {
        let index = entries
            .iter()
            .enumerate()
            .map(|(i, entry)| (entry.id.clone(), i))
            .collect();
        let path = branch(
            &entries,
            &index,
            entries.last().map(|entry| entry.id.as_str()),
        );
        let projection = project_with_base(&path, &base);
        let cache = ContextCache::of(&projection, &path);
        Self {
            base,
            entries,
            index,
            cache,
            message_count,
            last_seq,
        }
    }

    fn next_cache(&self, entry: &SessionEntry) -> ContextCache {
        let simple_parent =
            entry.parent_id.as_deref() == self.entries.last().map(|entry| entry.id.as_str());
        if simple_parent
            && !matches!(
                entry.kind,
                EntryKind::Compaction { .. } | EntryKind::ContextEdit { .. }
            )
        {
            return self.cache.append(entry);
        }
        let mut path = branch(&self.entries, &self.index, entry.parent_id.as_deref());
        path.push(entry);
        ContextCache::of(&project_with_base(&path, &self.base), &path)
    }

    fn push(&mut self, entry: SessionEntry, cache: ContextCache, message_count: u64, seq: i64) {
        self.index.insert(entry.id.clone(), self.entries.len());
        self.entries.push(entry);
        self.cache = cache;
        self.message_count = message_count;
        self.last_seq = seq;
    }
}

impl Store {
    pub(crate) fn load_session(
        self: &Arc<Self>,
        account: AccountId,
        uuid: &str,
        mode: SessionLoad,
    ) -> StoreResult<LoadedSession> {
        let (task, header, base, from, last, count) = self.read_bulk(|tx| {
            let task = task_key_tx(tx, account, uuid)?;
            let (header, start, base, last, count): (Option<String>, Option<i64>, Option<String>, i64, i64) = tx.query_row(
                "SELECT pi_header,context_start_seq,context_base,last_seq,message_count FROM tasks WHERE id=?1",
                [task.0], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)),
            )?;
            let header = header.map(|json| serde_json::from_str::<SessionHeader>(&json).map_err(|_| StoreError::InvalidData)).transpose()?;
            if header.as_ref().is_some_and(|header| header.id != uuid) || (header.is_none() && last != 0) { return Err(StoreError::InvalidData); }
            let (from, base) = match mode {
                #[cfg(any(test, feature = "store-bench"))]
                SessionLoad::Full => (1, SessionBase::default()),
                SessionLoad::Context => (start.unwrap_or(1), base.map(|json| serde_json::from_str::<SessionBase>(&json).map_err(|_| StoreError::InvalidData)).transpose()?.unwrap_or_default()),
            };
            if from < 1 || last < 0 || count < 0 || from > last.saturating_add(1) { return Err(StoreError::InvalidData); }
            Ok((task, header, base, from, last, count as u64))
        })?;
        let entries = self.load_source_range(account, uuid, task, from, last)?;
        let state = SessionState::new(base.clone(), entries.clone(), count, last);
        let adapter = SqliteSessionStore {
            store: Arc::clone(self),
            account,
            uuid: uuid.to_owned(),
            task,
            header: header.clone(),
            receipts: SourceReceipts::new(entries.last().map(|entry| entry.id.clone())),
            state,
        };
        Ok(LoadedSession {
            header,
            base,
            entries,
            store: adapter,
        })
    }

    fn load_source_range(
        &self,
        account: AccountId,
        uuid: &str,
        task: TaskKey,
        from: i64,
        through: i64,
    ) -> StoreResult<Vec<SessionEntry>> {
        let mut entries = Vec::new();
        let mut next = from;
        while next <= through {
            // Release the WAL snapshot before parsing or reading image chunks.
            let batch = self.read_bulk(|tx| {
                if task_key_tx(tx, account, uuid)? != task { return Err(StoreError::NotFound); }
                let mut statement = tx.prepare_cached("SELECT seq,flags,codec,bytes,payload FROM entries WHERE task_id=?1 AND seq>=?2 AND seq<=?3 ORDER BY seq LIMIT ?4")?;
                let mut rows = statement.query(params![task.0,next,through,LOAD_BATCH])?;
                let mut values = Vec::new();
                let mut bytes = 0usize;
                while let Some(row) = rows.next()? {
                    let value: (i64,i64,i64,i64,Vec<u8>) = (row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?);
                    if value.3 < 0 || value.3 as usize != value.4.len() { return Err(StoreError::InvalidData); }
                    bytes = bytes.saturating_add(value.4.len());
                    values.push(value);
                    if bytes >= LOAD_BATCH_BYTES { break; }
                }
                Ok(values)
            })?;
            if batch.is_empty() {
                return Err(StoreError::InvalidData);
            }
            for (seq, flags, encoded, _, payload) in batch {
                if seq != next || flags & !1 != 0 {
                    return Err(StoreError::InvalidData);
                }
                let entry = decode_entry(encoded, &payload)?;
                // Typed references stay on disk until the effective model context is selected.
                entries.push(entry);
                next += 1;
            }
        }
        Ok(entries)
    }
}

pub(super) fn decode_entry(encoded: i64, payload: &[u8]) -> StoreResult<SessionEntry> {
    let entry = serde_json::from_slice(&codec::decode(encoded, payload)?)
        .map_err(|_| StoreError::InvalidData)?;
    super::attachments::validate_stored_entry_images(&entry)?;
    Ok(entry)
}

/// Projector replay uses image references plus attachment metadata, never file bytes.
pub(super) fn read_entries_from(
    tx: &Transaction<'_>,
    task: TaskKey,
    from: i64,
) -> StoreResult<Vec<(i64, SessionEntry)>> {
    let mut statement = tx.prepare_cached(
        "SELECT seq,codec,payload FROM entries WHERE task_id=?1 AND seq>=?2 ORDER BY seq",
    )?;
    let values = statement
        .query_map(params![task.0, from], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    values
        .into_iter()
        .map(|(seq, encoded, payload)| Ok((seq, decode_entry(encoded, &payload)?)))
        .collect()
}

/// A source entry for full row-field expansion. The projector restores image
/// summary lengths from attachment metadata; timeline reads never fetch image bytes.
pub(super) fn read_entry(
    tx: &Transaction<'_>,
    task: TaskKey,
    seq: i64,
) -> StoreResult<SessionEntry> {
    let (flags, encoded, payload): (i64, i64, Vec<u8>) = tx.query_row(
        "SELECT flags,codec,payload FROM entries WHERE task_id=?1 AND seq=?2",
        params![task.0, seq],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if flags & !1 != 0 {
        return Err(StoreError::InvalidData);
    }
    decode_entry(encoded, &payload)
}

fn entry_kind(entry: &SessionEntry) -> &'static str {
    match entry.kind {
        EntryKind::Message { .. } => "message",
        EntryKind::ThinkingLevelChange { .. } => "thinking_level_change",
        EntryKind::ModelChange { .. } => "model_change",
        EntryKind::Compaction { .. } => "compaction",
        EntryKind::BranchSummary { .. } => "branch_summary",
        EntryKind::Custom { .. } => "custom",
        EntryKind::CustomMessage { .. } => "custom_message",
        EntryKind::ContextEdit { .. } => "context_edit",
        EntryKind::Label { .. } => "label",
        EntryKind::SessionInfo { .. } => "session_info",
    }
}

struct PreparedEntry {
    projection: super::projector::PreparedProjection,
    codec: i64,
    payload: Vec<u8>,
}

struct AppendFacts {
    model: Option<String>,
    thinking: String,
    usage: Option<u64>,
    estimate: u64,
}

impl AppendFacts {
    fn of(cache: &ContextCache) -> Self {
        Self {
            model: cache.model.clone(),
            thinking: cache.thinking.clone(),
            usage: cache.usage(),
            estimate: cache.estimate(),
        }
    }
}

struct AppendInput<'a> {
    entry: &'a SessionEntry,
    images: &'a PreparedEntryImages,
    prepared: &'a PreparedEntry,
    cache: &'a AppendFacts,
    count: u64,
    base: Option<&'a str>,
}

impl SqliteSessionStore {
    fn prepare(
        &self,
        entry: &SessionEntry,
        seq: i64,
    ) -> StoreResult<(PreparedEntryImages, PreparedEntry)> {
        if entry.id.is_empty()
            || entry.id.len() > 128
            || entry.parent_id.as_deref() == Some(entry.id.as_str())
        {
            return Err(StoreError::InvalidInput);
        }
        let images = self
            .store
            .prepare_entry_images(self.account, &self.uuid, entry)?;
        let json = serde_json::to_vec(&images.entry).map_err(|_| StoreError::InvalidInput)?;
        let (codec, payload) = codec::encode(&json)?;
        let projection = self.prepare_projection(&images.entry, seq)?;
        Ok((
            images,
            PreparedEntry {
                projection,
                codec,
                payload,
            },
        ))
    }

    fn prepare_projection(
        &self,
        entry: &SessionEntry,
        seq: i64,
    ) -> StoreResult<super::projector::PreparedProjection> {
        let sizes = self.store.read(|tx| {
            self.guard(tx)?;
            super::attachments::entry_image_sizes_tx(tx, self.task, entry)
        })?;
        super::projector::prepare_stored_entry(entry, seq, &sizes)
    }

    fn guard(&self, tx: &Transaction<'_>) -> StoreResult<()> {
        writable_account(tx, self.account)?;
        if task_key_tx(tx, self.account, &self.uuid)? != self.task {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    fn compaction_base(
        &self,
        entry: &SessionEntry,
        supplied: Option<&SessionBase>,
    ) -> StoreResult<Option<SessionBase>> {
        let EntryKind::Compaction {
            first_kept_entry_id,
            ..
        } = &entry.kind
        else {
            return Ok(None);
        };
        if let Some(base) = supplied {
            return Ok(Some(base.clone()));
        }
        let mut entries = self.state.entries.clone();
        entries.push(entry.clone());
        let manager = SessionManager::open_partial(
            self.header.clone().ok_or(StoreError::InvalidData)?,
            self.state.base.clone(),
            entries,
            Box::new(MemoryStore),
        );
        manager
            .base_before(first_kept_entry_id)
            .or_else(|_| manager.base_before(&entry.id))
            .map(Some)
            .map_err(|_| StoreError::InvalidInput)
    }

    fn append_prepared(
        &self,
        tx: &Transaction<'_>,
        input: AppendInput<'_>,
    ) -> StoreResult<(i64, StoreChange)> {
        let last: i64 = tx.query_row(
            "SELECT last_seq FROM tasks WHERE id=?1",
            [self.task.0],
            |row| row.get(0),
        )?;
        if last != self.state.last_seq {
            return Err(StoreError::Conflict);
        }
        append_tx(tx, self.task, last + 1, input).map(|change| (last + 1, change))
    }

    fn append_one(&mut self, entry: &SessionEntry, base: Option<&SessionBase>) -> StoreResult<()> {
        if self.header.is_none() {
            return Err(StoreError::InvalidData);
        }
        let (images, prepared) = self.prepare(entry, self.state.last_seq + 1)?;
        let context_base = self.compaction_base(entry, base)?;
        let branch_move =
            entry.parent_id.as_deref() != self.state.entries.last().map(|entry| entry.id.as_str());
        // Branch moves are exceptional and can point behind the loaded context.
        // Read the source tree before taking the writer, never inside the UI.
        let replacement = if branch_move {
            let entries = self.store.load_source_range(
                self.account,
                &self.uuid,
                self.task,
                1,
                self.state.last_seq,
            )?;
            if entry
                .parent_id
                .as_ref()
                .is_some_and(|parent| !entries.iter().any(|entry| &entry.id == parent))
            {
                return Err(StoreError::InvalidInput);
            }
            Some(SessionState::new(
                SessionBase::default(),
                entries,
                self.state.message_count,
                self.state.last_seq,
            ))
        } else {
            None
        };
        let state = replacement.as_ref().unwrap_or(&self.state);
        let next = state.next_cache(entry);
        let count = if branch_move {
            branch(&state.entries, &state.index, entry.parent_id.as_deref())
                .iter()
                .filter(|entry| conversation(entry))
                .count() as u64
        } else {
            state.message_count
        }
        .saturating_add(u64::from(conversation(entry)));
        let facts = AppendFacts::of(&next);
        let context_base_json = context_base
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|_| StoreError::InvalidInput)?;
        let seq = self.store.write_with_change(Durability::Durable, |tx| {
            self.guard(tx)?;
            let (seq, mut change) = self.append_prepared(
                tx,
                AppendInput {
                    entry,
                    images: &images,
                    prepared: &prepared,
                    cache: &facts,
                    count,
                    base: context_base_json.as_deref(),
                },
            )?;
            change.account = Some(self.account);
            change.task = Some(self.uuid.clone());
            Ok((seq, Some(change)))
        })?;
        if let Some(state) = replacement {
            self.state = state;
        }
        self.state.push(entry.clone(), next, count, seq);
        self.trim_compacted(entry, context_base);
        Ok(())
    }

    fn trim_compacted(&mut self, entry: &SessionEntry, base: Option<SessionBase>) {
        if let (
            EntryKind::Compaction {
                first_kept_entry_id,
                ..
            },
            Some(base),
        ) = (&entry.kind, base)
        {
            let start = self
                .state
                .index
                .get(first_kept_entry_id)
                .copied()
                .unwrap_or(self.state.entries.len() - 1);
            self.state.entries.drain(..start);
            self.state.index = self
                .state
                .entries
                .iter()
                .enumerate()
                .map(|(i, entry)| (entry.id.clone(), i))
                .collect();
            self.state.base = base;
        }
    }

    fn retry(
        &mut self,
        header: &SessionHeader,
        initial_base: Option<&SessionBase>,
        entries: &[SessionEntry],
    ) -> StoreResult<()> {
        if header.id != self.uuid {
            return Err(StoreError::InvalidInput);
        }
        let mut ids = HashSet::new();
        if entries.iter().any(|entry| !ids.insert(entry.id.as_str())) {
            return Err(StoreError::Conflict);
        }
        if entries.iter().any(|entry| {
            entry.id.is_empty()
                || entry.id.len() > 128
                || entry.parent_id.as_deref() == Some(entry.id.as_str())
        }) {
            return Err(StoreError::InvalidInput);
        }
        // Entries are append-only. Verify the retained suffix on short reader
        // snapshots, then CAS all source/cache boundary facts in the writer.
        // No source JSON decoding or full projection happens under that mutex.
        let (saved_header, last, saved_count, start) = self.store.read(|tx| {
            self.guard(tx)?;
            let (saved_header, last, saved_count): (Option<String>, i64, i64) = tx.query_row(
                "SELECT pi_header,last_seq,message_count FROM tasks WHERE id=?1",
                [self.task.0],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            let start = match entries.first() {
                Some(first) => tx
                    .query_row(
                        "SELECT seq FROM entries WHERE task_id=?1 AND entry_id=?2",
                        params![self.task.0, first.id],
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()?
                    .unwrap_or(last + 1),
                None if last == 0 => 1,
                None => return Err(StoreError::Conflict),
            };
            if initial_base.is_none() && start != 1 {
                return Err(StoreError::Conflict);
            }
            if start > 1
                && entries.first().is_some_and(|entry| {
                    tx.query_row(
                        "SELECT entry_id FROM entries WHERE task_id=?1 AND seq=?2",
                        params![self.task.0, start - 1],
                        |row| row.get::<_, String>(0),
                    )
                    .ok()
                    .as_deref()
                        != entry.parent_id.as_deref()
                })
            {
                return Err(StoreError::Conflict);
            }
            Ok((saved_header, last, saved_count, start))
        })?;
        if let Some(saved) = &saved_header {
            let saved: SessionHeader =
                serde_json::from_str(saved).map_err(|_| StoreError::InvalidData)?;
            if saved != *header {
                return Err(StoreError::Conflict);
            }
        }
        let stored_len = last.saturating_sub(start).saturating_add(1) as usize;
        if (last > 0 && stored_len == 0) || stored_len > entries.len() {
            return Err(StoreError::Conflict);
        }
        let images = self
            .store
            .prepare_entries_images(self.account, &self.uuid, entries)?;
        for (offset, value) in images.entries.iter().take(stored_len).enumerate() {
            let (id,flags,encoded,payload):(String,i64,i64,Vec<u8>)=self.store.read_bulk(|tx| {
                self.guard(tx)?;
                Ok(tx.query_row("SELECT entry_id,flags,codec,payload FROM entries WHERE task_id=?1 AND seq=?2",params![self.task.0,start+offset as i64],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)))?)
            })?;
            if id != entries[offset].id
                || flags != i64::from(value.has_images())
                || decode_entry(encoded, &payload)? != value.entry
            {
                return Err(StoreError::Conflict);
            }
        }
        let prepared = images
            .entries
            .iter()
            .enumerate()
            .map(|(offset, images)| {
                let raw =
                    serde_json::to_vec(&images.entry).map_err(|_| StoreError::InvalidInput)?;
                let (codec, payload) = codec::encode(&raw)?;
                Ok(PreparedEntry {
                    projection: self.prepare_projection(&images.entry, start + offset as i64)?,
                    codec,
                    payload,
                })
            })
            .collect::<StoreResult<Vec<_>>>()?;
        let header_json = serde_json::to_string(header).map_err(|_| StoreError::InvalidInput)?;
        let base = initial_base.cloned().unwrap_or_default();
        let manager = SessionManager::open_partial(
            header.clone(),
            base.clone(),
            entries.to_vec(),
            Box::new(MemoryStore),
        );
        let stored_entries = &entries[..stored_len];
        let stored_index = stored_entries
            .iter()
            .enumerate()
            .map(|(i, entry)| (entry.id.clone(), i))
            .collect();
        let prior_count = branch(
            stored_entries,
            &stored_index,
            stored_entries.last().map(|entry| entry.id.as_str()),
        )
        .iter()
        .filter(|entry| conversation(entry))
        .count() as u64;
        let prefix_count = u64::try_from(saved_count)
            .map_err(|_| StoreError::InvalidData)?
            .saturating_sub(prior_count);
        let mut state = SessionState::new(base.clone(), Vec::new(), prefix_count, start - 1);
        let mut facts = Vec::with_capacity(entries.len().saturating_sub(stored_len));
        for (offset, entry) in entries.iter().enumerate() {
            let cache = state.next_cache(entry);
            let previous_count = if offset == 0
                || entry.parent_id.as_deref() == state.entries.last().map(|entry| entry.id.as_str())
            {
                state.message_count
            } else {
                let path = branch(&state.entries, &state.index, entry.parent_id.as_deref());
                let inherited = if path.first().is_some_and(|entry| entry.id == entries[0].id) {
                    prefix_count
                } else {
                    0
                };
                inherited
                    .saturating_add(path.iter().filter(|entry| conversation(entry)).count() as u64)
            };
            let count = previous_count.saturating_add(u64::from(conversation(entry)));
            if offset >= stored_len {
                let context_base = if let EntryKind::Compaction {
                    first_kept_entry_id,
                    ..
                } = &entry.kind
                {
                    Some(
                        manager
                            .base_before(first_kept_entry_id)
                            .or_else(|_| manager.base_before(&entry.id))
                            .map_err(|_| StoreError::InvalidInput)?,
                    )
                } else {
                    None
                };
                let base_json = context_base
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()
                    .map_err(|_| StoreError::InvalidInput)?;
                facts.push((AppendFacts::of(&cache), count, base_json));
            }
            state.push(entry.clone(), cache, count, start + offset as i64);
        }
        let final_count = state.message_count;
        let final_last = last.max(start + entries.len() as i64 - 1);
        drop(state);
        let final_state = SessionState::new(base, entries.to_vec(), final_count, final_last);
        let latest_compaction = entries
            .iter()
            .rev()
            .find(|entry| matches!(entry.kind, EntryKind::Compaction { .. }))
            .map(|entry| {
                let EntryKind::Compaction {
                    first_kept_entry_id,
                    ..
                } = &entry.kind
                else {
                    unreachable!()
                };
                manager
                    .base_before(first_kept_entry_id)
                    .or_else(|_| manager.base_before(&entry.id))
                    .map(|base| (entry, base))
                    .map_err(|_| StoreError::InvalidInput)
            })
            .transpose()?;
        self.store.write_with_change(Durability::Durable, |tx| {
            self.guard(tx)?;
            let current: (Option<String>, i64, i64) = tx.query_row(
                "SELECT pi_header,last_seq,message_count FROM tasks WHERE id=?1",
                [self.task.0],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            if current != (saved_header.clone(), last, saved_count) {
                return Err(StoreError::Conflict);
            }
            tx.execute(
                "UPDATE tasks SET pi_header=?2 WHERE id=?1",
                params![self.task.0, header_json],
            )?;
            let mut change = StoreChange {
                account: Some(self.account),
                task: Some(self.uuid.clone()),
                ..Default::default()
            };
            for (offset, (cache, count, base_json)) in facts.iter().enumerate() {
                let index = stored_len + offset;
                let next = append_tx(
                    tx,
                    self.task,
                    start + index as i64,
                    AppendInput {
                        entry: &entries[index],
                        images: &images.entries[index],
                        prepared: &prepared[index],
                        cache,
                        count: *count,
                        base: base_json.as_deref(),
                    },
                )?;
                change.rows.extend(next.rows);
                change.hidden_rows.extend(next.hidden_rows);
                change.flipped_epoch = next.flipped_epoch.or(change.flipped_epoch);
                change.summary_changed |= next.summary_changed;
                change.listed_changed |= next.listed_changed;
            }
            Ok(((), change.summary_changed.then_some(change)))
        })?;
        self.header = Some(header.clone());
        self.state = final_state;
        if let Some((entry, base)) = latest_compaction {
            self.trim_compacted(entry, Some(base));
        }
        Ok(())
    }
}

fn append_tx(
    tx: &Transaction<'_>,
    task: TaskKey,
    seq: i64,
    input: AppendInput<'_>,
) -> StoreResult<StoreChange> {
    let AppendInput {
        entry,
        images,
        prepared,
        cache,
        count,
        base,
    } = input;
    tx.execute("INSERT INTO entries(task_id,seq,entry_id,kind,created_ms,flags,codec,bytes,payload) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)", params![task.0,seq,entry.id,entry_kind(entry),entry.timestamp,i64::from(images.has_images()),prepared.codec,prepared.payload.len() as i64,prepared.payload])?;
    images.link(tx, task.0)?;
    let projection = super::projector::append_prepared(tx, task, seq, entry, &prepared.projection)?;
    let boundary = if let EntryKind::Compaction {
        first_kept_entry_id,
        ..
    } = &entry.kind
    {
        Some(
            tx.query_row(
                "SELECT seq FROM entries WHERE task_id=?1 AND entry_id=?2",
                params![task.0, first_kept_entry_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(seq),
        )
    } else {
        None
    };
    let base_json = base;
    tx.execute("UPDATE tasks SET last_seq=?2,message_count=?3,model=?4,thinking_level=?5,context_usage=?6,context_estimate=?7,context_start_seq=coalesce(?8,context_start_seq),context_base=coalesce(?9,context_base),updated_ms=max(updated_ms,?10) WHERE id=?1", params![task.0,seq,to_i64(count)?,cache.model,cache.thinking,cache.usage.map(to_i64).transpose()?,to_i64(cache.estimate)?,boundary,base_json,entry.timestamp])?;
    Ok(StoreChange {
        rows: projection
            .rows
            .into_iter()
            .map(|row| {
                let origin_entry_id = if row.origin.entry_seq == seq {
                    entry.id.clone()
                } else {
                    tx.query_row(
                        "SELECT entry_id FROM entries WHERE task_id=?1 AND seq=?2",
                        params![task.0, row.origin.entry_seq],
                        |row| row.get(0),
                    )?
                };
                Ok(StoredRowChange {
                    epoch: row.identity.epoch,
                    row_key: row.item.id,
                    row_seq: row.identity.row_seq,
                    entry_seq: row.origin.entry_seq,
                    item_index: row.origin.item_index,
                    origin_entry_id,
                    update_entry_id: entry.id.clone(),
                    update_seq: row.update_seq,
                })
            })
            .collect::<StoreResult<Vec<_>>>()?,
        hidden_rows: projection
            .hidden
            .into_iter()
            .map(|row| StoredRowIdentity {
                epoch: row.epoch,
                row_seq: row.row_seq,
            })
            .collect(),
        summary_changed: true,
        listed_changed: true,
        flipped_epoch: projection.history_replaced,
        ..Default::default()
    })
}

fn to_i64(value: u64) -> StoreResult<i64> {
    i64::try_from(value).map_err(|_| StoreError::InvalidInput)
}

impl SessionStore for SqliteSessionStore {
    fn write_all(&mut self, header: &SessionHeader, entries: &[SessionEntry]) -> io::Result<()> {
        self.receipts.observe(entries);
        let result = self.retry(header, None, entries);
        if let Err(error) = &result {
            self.store.record_persistence_failure(error);
        }
        result.map_err(Into::into)
    }
    fn write_all_with_base(
        &mut self,
        header: &SessionHeader,
        base: Option<&SessionBase>,
        entries: &[SessionEntry],
    ) -> io::Result<()> {
        self.receipts.observe(entries);
        let result = self.retry(header, base, entries);
        if let Err(error) = &result {
            self.store.record_persistence_failure(error);
        }
        result.map_err(Into::into)
    }
    fn append(&mut self, entry: &SessionEntry) -> io::Result<()> {
        self.receipts.observe(std::slice::from_ref(entry));
        let result = self.append_one(entry, None);
        if let Err(error) = &result {
            self.store.record_persistence_failure(error);
        }
        result.map_err(Into::into)
    }
    fn append_with_context(
        &mut self,
        entry: &SessionEntry,
        base: Option<&SessionBase>,
    ) -> io::Result<()> {
        self.receipts.observe(std::slice::from_ref(entry));
        let result = self.append_one(entry, base);
        if let Err(error) = &result {
            self.store.record_persistence_failure(error);
        }
        result.map_err(Into::into)
    }
    fn contains_entry(&self, id: &str) -> io::Result<bool> {
        self.store
            .read(|tx| {
                if task_key_tx(tx, self.account, &self.uuid)? != self.task {
                    return Err(StoreError::NotFound);
                }
                Ok(tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM entries WHERE task_id=?1 AND entry_id=?2)",
                    params![self.task.0, id],
                    |row| row.get(0),
                )?)
            })
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests;
