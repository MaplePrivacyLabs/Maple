//! Response-local ciphertext reuse. Fresh source metadata remains authoritative;
//! neither cached payloads nor transport batches replace checkpoint validation.

use super::policy::MAX_SOURCE_BATCH_BYTES;
use crate::{
    db::{DBConnection, DBError},
    models::{
        response_compaction::{
            CompactionError, CompactionSnapshot, CompactionSourceMetadata, ContextItemKey,
            ContextItemKind,
        },
        responses::RawThreadMessage,
    },
};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use uuid::Uuid;

const MAX_BATCH_ROWS: usize = 128;
const MAX_CACHE_BYTES: usize = 16 * 1024 * 1024;
const MAX_CACHE_ROWS: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Scope {
    user: Uuid,
    conversation: i64,
    conversation_uuid: Uuid,
}

impl From<&CompactionSnapshot> for Scope {
    fn from(source: &CompactionSnapshot) -> Self {
        Self {
            user: source.user_id,
            conversation: source.conversation_id,
            conversation_uuid: source.conversation_uuid,
        }
    }
}

struct CachedItem {
    metadata: CompactionSourceMetadata,
    raw: RawThreadMessage,
}

/// Owned by a single logical Response, including its completed tool steps.
/// Large items remain valid input but bypass the bounded cache. Transport batches
/// limit estimated ciphertext bytes; a single larger row cannot be split in SQL.
#[derive(Default)]
pub(super) struct SourceLoader {
    scope: Option<Scope>,
    items: HashMap<ContextItemKey, CachedItem>,
    insertion_order: VecDeque<ContextItemKey>,
    cached_bytes: usize,
}

impl SourceLoader {
    /// Remove newly covered, disappeared, or changed items before planning again.
    pub(super) fn retain(
        &mut self,
        source: &CompactionSnapshot,
        uncovered: &HashSet<ContextItemKey>,
    ) {
        let scope = Scope::from(source);
        if self.scope != Some(scope) {
            self.items.clear();
            self.insertion_order.clear();
            self.cached_bytes = 0;
            self.scope = Some(scope);
            return;
        }
        let metadata: HashMap<_, _> = source.items.iter().map(|item| (item.key, item)).collect();
        self.items.retain(|key, cached| {
            uncovered.contains(key)
                && metadata
                    .get(key)
                    .is_some_and(|current| **current == cached.metadata)
        });
        self.insertion_order
            .retain(|key| self.items.contains_key(key));
        self.cached_bytes = self
            .items
            .values()
            .map(|item| payload_bytes(&item.raw))
            .sum();
    }

    /// Hydrate token planning only from rows validated against the fresh snapshot.
    pub(super) fn cached(&self, source: &CompactionSnapshot) -> Vec<RawThreadMessage> {
        if self.scope != Some(Scope::from(source)) {
            return Vec::new();
        }
        source
            .items
            .iter()
            .filter_map(|metadata| {
                self.items
                    .get(&metadata.key)
                    .filter(|item| item.metadata == *metadata)
                    .map(|item| item.raw.clone())
            })
            .collect()
    }

    pub(super) fn load<D: DBConnection + ?Sized>(
        &mut self,
        db: &D,
        source: &CompactionSnapshot,
        keys: &[ContextItemKey],
    ) -> Result<Vec<RawThreadMessage>, DBError> {
        self.load_with(source, keys, |batch| {
            db.load_compaction_items(source.conversation_id, source.user_id, batch)
        })
    }

    fn load_with(
        &mut self,
        source: &CompactionSnapshot,
        keys: &[ContextItemKey],
        mut fetch: impl FnMut(&[ContextItemKey]) -> Result<Vec<RawThreadMessage>, DBError>,
    ) -> Result<Vec<RawThreadMessage>, DBError> {
        // Reuse requires the full fresh metadata, including status, token count,
        // rendering fields and ciphertext digest, rather than just row identity.
        self.retain(source, &source.items.iter().map(|item| item.key).collect());
        let metadata: HashMap<_, _> = source.items.iter().map(|item| (item.key, item)).collect();
        if keys.iter().copied().collect::<HashSet<_>>().len() != keys.len()
            || keys.iter().any(|key| !metadata.contains_key(key))
        {
            return Err(invalid_selection());
        }

        let mut result = HashMap::with_capacity(keys.len());
        let mut batch = Vec::new();
        let mut batch_bytes = 0usize;
        for key in keys {
            if let Some(cached) = self.items.get(key) {
                result.insert(*key, cached.raw.clone());
                continue;
            }
            let bytes =
                usize::try_from(metadata[key].encrypted_bytes).map_err(|_| invalid_selection())?;
            if !batch.is_empty()
                && (batch.len() == MAX_BATCH_ROWS
                    || batch_bytes.saturating_add(bytes) > MAX_SOURCE_BATCH_BYTES)
            {
                self.fetch_batch(&batch, &metadata, &mut fetch, &mut result)?;
                batch.clear();
                batch_bytes = 0;
            }
            batch.push(*key);
            batch_bytes = batch_bytes.saturating_add(bytes);
        }
        if !batch.is_empty() {
            self.fetch_batch(&batch, &metadata, &mut fetch, &mut result)?;
        }
        keys.iter()
            .map(|key| result.remove(key).ok_or_else(invalid_selection))
            .collect()
    }

    fn fetch_batch(
        &mut self,
        batch: &[ContextItemKey],
        metadata: &HashMap<ContextItemKey, &CompactionSourceMetadata>,
        fetch: &mut impl FnMut(&[ContextItemKey]) -> Result<Vec<RawThreadMessage>, DBError>,
        result: &mut HashMap<ContextItemKey, RawThreadMessage>,
    ) -> Result<(), DBError> {
        let rows = fetch(batch)?;
        if rows.len() != batch.len() {
            return Err(invalid_selection());
        }
        let expected: HashSet<_> = batch.iter().copied().collect();
        let mut received = HashSet::new();
        // Validate the whole batch before caching any of it. Rows can change
        // between the metadata snapshot and payload SELECT; signal a replan.
        for row in &rows {
            let kind =
                ContextItemKind::from_db_type(&row.message_type).ok_or_else(invalid_selection)?;
            let key = ContextItemKey {
                kind,
                uuid: row.uuid,
            };
            if !expected.contains(&key)
                || !received.insert(key)
                || !matches_source(row, metadata[&key])
            {
                return Err(invalid_selection());
            }
        }
        for row in rows {
            let key = ContextItemKey {
                kind: ContextItemKind::from_db_type(&row.message_type)
                    .ok_or_else(invalid_selection)?,
                uuid: row.uuid,
            };
            self.insert(metadata[&key], &row);
            result.insert(key, row);
        }
        Ok(())
    }

    fn insert(&mut self, metadata: &CompactionSourceMetadata, row: &RawThreadMessage) {
        let bytes = payload_bytes(row);
        if bytes > MAX_CACHE_BYTES {
            return;
        }
        while self.cached_bytes.saturating_add(bytes) > MAX_CACHE_BYTES
            || self.items.len() >= MAX_CACHE_ROWS
        {
            let Some(oldest) = self.insertion_order.pop_front() else {
                break;
            };
            if let Some(removed) = self.items.remove(&oldest) {
                self.cached_bytes -= payload_bytes(&removed.raw);
            }
        }
        self.cached_bytes += bytes;
        self.insertion_order.push_back(metadata.key);
        self.items.insert(
            metadata.key,
            CachedItem {
                metadata: metadata.clone(),
                raw: row.clone(),
            },
        );
    }
}

fn invalid_selection() -> DBError {
    CompactionError::InvalidSelection.into()
}

fn payload_bytes(row: &RawThreadMessage) -> usize {
    row.content_enc.as_ref().map_or(0, Vec::len)
}

fn matches_source(row: &RawThreadMessage, metadata: &CompactionSourceMetadata) -> bool {
    let fingerprint = row.content_enc.as_ref().map_or_else(
        || "-".to_owned(),
        |content| hex::encode(Sha256::digest(content)),
    );
    // User metadata also commits to the separately stored attachment text.
    // RawThreadMessage carries only content_enc; retain compares the complete
    // metadata fingerprint and publication independently fences all source rows.
    let expected_fingerprint = match metadata.key.kind {
        ContextItemKind::User => metadata.content_fingerprint.split(':').next().unwrap_or(""),
        _ => metadata.content_fingerprint.as_str(),
    };
    row.id == metadata.id
        && row.uuid == metadata.key.uuid
        && row.message_type == metadata.key.kind.as_str()
        && row.response_uuid == metadata.response_uuid
        && row.created_at == metadata.created_at
        && row.status == metadata.item_status
        && row.model == metadata.model
        && row.token_count == metadata.token_count
        && row.tool_call_id == metadata.tool_call_id
        && row.finish_reason == metadata.finish_reason
        && row.tool_name == metadata.tool_name
        && i64::try_from(payload_bytes(row)).ok() == Some(metadata.encrypted_bytes)
        && fingerprint == expected_fingerprint
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model_config::ReasoningReplay,
        web::responses::{
            compaction::{context, policy::CompactionPolicy},
            context_builder::snapshot_from_metadata,
        },
    };
    use chrono::{TimeZone, Utc};

    fn row(id: u128, bytes: usize) -> RawThreadMessage {
        RawThreadMessage {
            message_type: "tool_output".into(),
            id: id as i64,
            uuid: Uuid::from_u128(id),
            response_uuid: Some(Uuid::from_u128(10_000)),
            content_enc: Some(vec![id as u8; bytes]),
            status: Some("completed".into()),
            created_at: Utc.timestamp_opt(1_700_000_000 + id as i64, 0).unwrap(),
            model: None,
            token_count: Some(20),
            tool_call_id: Some(Uuid::from_u128(id + 20_000)),
            finish_reason: None,
            tool_name: Some("search".into()),
        }
    }

    fn metadata(row: &RawThreadMessage) -> CompactionSourceMetadata {
        let kind = ContextItemKind::from_db_type(&row.message_type).unwrap();
        let mut fingerprint = row.content_enc.as_ref().map_or_else(
            || "-".to_owned(),
            |content| hex::encode(Sha256::digest(content)),
        );
        if kind == ContextItemKind::User {
            fingerprint.push_str(":-");
        }
        CompactionSourceMetadata {
            key: ContextItemKey {
                kind,
                uuid: row.uuid,
            },
            id: row.id,
            created_at: row.created_at,
            updated_at: row.created_at,
            token_count: row.token_count,
            item_status: row.status.clone(),
            response_uuid: row.response_uuid,
            response_status: Some("in_progress".into()),
            tool_call_id: row.tool_call_id,
            tool_name: row.tool_name.clone(),
            model: row.model.clone(),
            finish_reason: row.finish_reason.clone(),
            encrypted_bytes: payload_bytes(row) as i64,
            content_fingerprint: fingerprint,
        }
    }

    fn snapshot(rows: &[RawThreadMessage]) -> CompactionSnapshot {
        CompactionSnapshot {
            conversation_id: 1,
            conversation_uuid: Uuid::from_u128(1_000),
            user_id: Uuid::from_u128(2_000),
            items: rows.iter().map(metadata).collect(),
            active_response_uuids: Vec::new(),
            fingerprint: [0; 32],
            checkpoint: None,
        }
    }

    fn keys(source: &CompactionSnapshot) -> Vec<ContextItemKey> {
        source.items.iter().map(|item| item.key).collect()
    }

    fn fetch(rows: &[RawThreadMessage], selected: &[ContextItemKey]) -> Vec<RawThreadMessage> {
        selected
            .iter()
            .map(|key| {
                rows.iter()
                    .find(|row| metadata(row).key == *key)
                    .unwrap()
                    .clone()
            })
            .collect()
    }

    fn is_invalid(result: Result<Vec<RawThreadMessage>, DBError>) -> bool {
        matches!(
            result,
            Err(DBError::CompactionError(CompactionError::InvalidSelection))
        )
    }

    #[test]
    fn repeated_selection_reuses_ciphertext_and_preserves_caller_order() {
        let rows = vec![row(1, 100), row(2, 200), row(3, 300)];
        let source = snapshot(&rows);
        let selected = vec![
            source.items[2].key,
            source.items[0].key,
            source.items[1].key,
        ];
        let mut loader = SourceLoader::default();
        let mut executions = 0;
        let mut returned_bytes = 0;
        let loaded = loader
            .load_with(&source, &selected, |batch| {
                executions += 1;
                let loaded = fetch(&rows, batch);
                returned_bytes += loaded.iter().map(payload_bytes).sum::<usize>();
                Ok(loaded)
            })
            .unwrap();
        assert_eq!(
            loaded.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![3, 1, 2]
        );
        assert_eq!(executions, 1);
        assert_eq!(returned_bytes, 600);
        assert_eq!(
            loader
                .load_with(&source, &selected, |_| panic!("must reuse rows"))
                .unwrap()
                .len(),
            3
        );
        assert_eq!(loader.cached(&source).len(), 3);
    }

    #[test]
    fn repeated_tool_turn_fetches_only_new_items() {
        let mut rows = vec![row(1, 100), row(2, 200)];
        let mut source = snapshot(&rows);
        let mut loader = SourceLoader::default();
        loader
            .load_with(&source, &keys(&source), |batch| Ok(fetch(&rows, batch)))
            .unwrap();
        rows.push(row(3, 300));
        source.items.push(metadata(&rows[2]));
        let mut fetched = Vec::new();
        let loaded = loader
            .load_with(&source, &keys(&source), |batch| {
                fetched.extend_from_slice(batch);
                Ok(fetch(&rows, batch))
            })
            .unwrap();
        assert_eq!(fetched, vec![source.items[2].key]);
        assert_eq!(loaded.iter().map(payload_bytes).sum::<usize>(), 600);
    }

    #[test]
    fn metadata_planning_then_checkpoint_and_tool_turn_read_each_payload_once() {
        let current_response = Uuid::from_u128(30_000);
        // Low recorded token counts must not cause image-sized ciphertext to be
        // fetched wholesale before the planner applies its independent byte cap.
        let mut rows: Vec<_> = (1..=6)
            .map(|id| {
                let mut raw = row(id, 2 * 1024 * 1024);
                raw.message_type = "assistant".into();
                raw.model = Some("deepseek-v4-1-flash".into());
                raw.tool_call_id = None;
                raw.tool_name = None;
                raw
            })
            .collect();
        let mut current_raw = row(7, 128);
        current_raw.message_type = "user".into();
        current_raw.response_uuid = Some(current_response);
        current_raw.model = Some("deepseek-v4-1-flash".into());
        current_raw.tool_call_id = None;
        current_raw.tool_name = None;
        let current_user = metadata(&current_raw).key;
        rows.push(current_raw);
        let incoming = context::ContextItem::from_prompt(
            current_user,
            Some(current_response),
            vec![serde_json::json!({"role":"user", "content":"Continue the investigation"})],
            20,
            true,
            None,
        );
        let policy = CompactionPolicy {
            input_budget: 20_000,
            trigger_tokens: 1,
            keep_recent_tokens: 40,
            summary_input_budget: 10_000,
        };
        let make_source = |rows: &[RawThreadMessage]| {
            let mut source = snapshot(rows);
            for item in &mut source.items {
                if item.response_uuid != Some(current_response) {
                    item.response_status = Some("completed".into());
                }
            }
            source
        };
        let make_planning =
            |source: &CompactionSnapshot, checkpoint: &Option<context::CheckpointPayload>| {
                let mut planning = snapshot_from_metadata(
                    Vec::new(),
                    0,
                    &source.items,
                    "deepseek-v4-1-flash",
                    ReasoningReplay::None,
                    current_user,
                    current_response,
                    &source.active_response_uuids,
                    checkpoint,
                )
                .unwrap();
                // The current request already supplies this prepared input; the
                // historical rows deliberately remain metadata-only in this test.
                let current = planning
                    .items
                    .iter_mut()
                    .find(|item| item.key == current_user)
                    .unwrap();
                *current = incoming.clone();
                planning
            };
        let covered_keys = |checkpoint: &Option<context::CheckpointPayload>| -> HashSet<_> {
            checkpoint
                .iter()
                .flat_map(|payload| {
                    payload
                        .history
                        .iter()
                        .chain(payload.active.iter())
                        .flat_map(|section| section.covered.iter().copied())
                })
                .collect()
        };
        let total_bytes: usize = rows.iter().map(payload_bytes).sum();
        let mut checkpoint = None;
        let mut loader = SourceLoader::default();
        // These are payload-loader closure invocations, not PostgreSQL execution
        // counts: storage may execute separate SELECTs for different item kinds.
        let mut payload_fetches: Vec<(Vec<ContextItemKey>, usize)> = Vec::new();
        let mut publications = 0;
        loop {
            // Every preparation iteration starts with a fresh metadata snapshot.
            let source = make_source(&rows);
            let covered = covered_keys(&checkpoint);
            let uncovered: HashSet<_> = keys(&source)
                .into_iter()
                .filter(|key| !covered.contains(key))
                .collect();
            loader.retain(&source, &uncovered);
            assert!(loader
                .cached(&source)
                .iter()
                .all(|raw| !covered.contains(&metadata(raw).key)));
            let planning = make_planning(&source, &checkpoint);
            let plan = context::plan_batch(
                &planning,
                &checkpoint,
                &policy,
                false,
                policy.summary_input_budget,
            )
            .unwrap();
            if plan.summary_work.is_empty() {
                let suffix: Vec<_> = keys(&source)
                    .into_iter()
                    .filter(|key| uncovered.contains(key))
                    .collect();
                let loaded = loader
                    .load_with(&source, &suffix, |batch| {
                        let loaded = fetch(&rows, batch);
                        payload_fetches
                            .push((batch.to_vec(), loaded.iter().map(payload_bytes).sum()));
                        Ok(loaded)
                    })
                    .unwrap();
                assert_eq!(
                    loaded.iter().map(|raw| raw.id).collect::<Vec<_>>(),
                    vec![6, 7]
                );
                break;
            }
            let planned: Vec<_> = plan
                .summary_work
                .iter()
                .flat_map(|work| {
                    assert!(
                        work.items
                            .iter()
                            .map(|item| item.source_bytes)
                            .sum::<usize>()
                            <= MAX_SOURCE_BATCH_BYTES
                    );
                    assert!(
                        work.items
                            .iter()
                            .map(|item| item.estimated_tokens)
                            .sum::<usize>()
                            <= policy.summary_input_budget
                    );
                    work.items.iter().map(|item| item.key)
                })
                .chain(std::iter::once(current_user))
                .collect();
            assert!(planned.iter().all(|key| !covered.contains(key)));
            loader
                .load_with(&source, &planned, |batch| {
                    let loaded = fetch(&rows, batch);
                    payload_fetches.push((batch.to_vec(), loaded.iter().map(payload_bytes).sum()));
                    Ok(loaded)
                })
                .unwrap();
            let summaries = vec![
                "Earlier investigation facts and decisions are retained.".into();
                plan.summary_work.len()
            ];
            // Simulate successful publication. This exercises the real coverage
            // transition, while deliberately excluding inference and encryption.
            checkpoint = Some(context::complete_plan(&checkpoint, &plan, &summaries).unwrap());
            publications += 1;
            assert!(publications < 4);
        }
        assert_eq!(publications, 2);
        assert_eq!(payload_fetches[0].1, MAX_SOURCE_BATCH_BYTES);
        assert!(payload_fetches[0].1 < total_bytes);
        assert_eq!(
            payload_fetches
                .iter()
                .map(|(_, bytes)| bytes)
                .sum::<usize>(),
            total_bytes
        );
        let counts = payload_fetches.iter().flat_map(|(batch, _)| batch).fold(
            HashMap::new(),
            |mut counts, key| {
                *counts.entry(*key).or_insert(0) += 1;
                counts
            },
        );
        assert_eq!(counts.len(), rows.len());
        assert!(counts.values().all(|count| *count == 1));
        assert_eq!(covered_keys(&checkpoint).len(), 5);

        // A completed tool step adds a paired exchange to the same Response.
        // Fresh metadata + the existing checkpoint must reuse the old suffix
        // and fetch only this pair, never the already summarized 10 MiB prefix.
        let mut call = row(8, 100);
        call.message_type = "tool_call".into();
        call.response_uuid = Some(current_response);
        call.tool_call_id = Some(call.uuid);
        let mut output = row(9, 1024 * 1024);
        output.response_uuid = Some(current_response);
        output.tool_call_id = Some(call.uuid);
        rows.extend([call, output]);
        let source = make_source(&rows);
        let covered = covered_keys(&checkpoint);
        let suffix: Vec<_> = keys(&source)
            .into_iter()
            .filter(|key| !covered.contains(key))
            .collect();
        loader.retain(&source, &suffix.iter().copied().collect());
        let planning = make_planning(&source, &checkpoint);
        let quiet_policy = CompactionPolicy {
            trigger_tokens: 20_000,
            ..policy
        };
        assert!(context::plan_batch(
            &planning,
            &checkpoint,
            &quiet_policy,
            false,
            quiet_policy.summary_input_budget
        )
        .unwrap()
        .summary_work
        .is_empty());
        let before_tool_fetches = payload_fetches.len();
        let loaded = loader
            .load_with(&source, &suffix, |batch| {
                let loaded = fetch(&rows, batch);
                payload_fetches.push((batch.to_vec(), loaded.iter().map(payload_bytes).sum()));
                Ok(loaded)
            })
            .unwrap();
        assert_eq!(
            loaded.iter().map(|raw| raw.id).collect::<Vec<_>>(),
            vec![6, 7, 8, 9]
        );
        assert_eq!(payload_fetches.len() - before_tool_fetches, 1);
        let (new_keys, new_bytes) = payload_fetches.last().unwrap();
        assert_eq!(*new_keys, vec![source.items[7].key, source.items[8].key]);
        assert_eq!(*new_bytes, 100 + 1024 * 1024);
        assert_eq!(
            payload_fetches
                .iter()
                .map(|(_, bytes)| bytes)
                .sum::<usize>(),
            rows.iter().map(payload_bytes).sum::<usize>()
        );
    }

    #[test]
    fn every_source_metadata_field_participates_in_reuse_validation() {
        type Change = fn(&mut CompactionSourceMetadata);
        let changes: &[Change] = &[
            |item| item.id += 1,
            |item| item.created_at += chrono::Duration::seconds(1),
            |item| item.updated_at += chrono::Duration::seconds(1),
            |item| item.token_count = Some(99),
            |item| item.item_status = Some("in_progress".into()),
            |item| item.response_uuid = None,
            |item| item.response_status = Some("completed".into()),
            |item| item.tool_call_id = None,
            |item| item.tool_name = Some("read_image".into()),
            |item| item.model = Some("another-model".into()),
            |item| item.finish_reason = Some("length".into()),
            |item| item.encrypted_bytes += 1,
            |item| item.content_fingerprint = "different".into(),
        ];
        for change in changes {
            let rows = vec![row(1, 100)];
            let mut source = snapshot(&rows);
            let mut loader = SourceLoader::default();
            loader
                .load_with(&source, &keys(&source), |batch| Ok(fetch(&rows, batch)))
                .unwrap();
            change(&mut source.items[0]);
            assert!(loader.cached(&source).is_empty());
            loader.retain(&source, &keys(&source).into_iter().collect());
            assert!(loader.items.is_empty());
            assert_eq!(loader.cached_bytes, 0);
        }
    }

    #[test]
    fn changed_ciphertext_is_refetched_and_checked_against_fresh_source() {
        let mut rows = vec![row(1, 100)];
        let mut source = snapshot(&rows);
        let mut loader = SourceLoader::default();
        loader
            .load_with(&source, &keys(&source), |batch| Ok(fetch(&rows, batch)))
            .unwrap();
        rows[0].content_enc.as_mut().unwrap()[0] ^= 1;
        source.items[0] = metadata(&rows[0]);
        let mut executions = 0;
        let loaded = loader
            .load_with(&source, &keys(&source), |batch| {
                executions += 1;
                Ok(fetch(&rows, batch))
            })
            .unwrap();
        assert_eq!(executions, 1);
        assert_eq!(loaded[0].content_enc, rows[0].content_enc);
    }

    #[test]
    fn covered_and_disappeared_rows_are_pruned() {
        let rows = vec![row(1, 100), row(2, 200), row(3, 300)];
        let mut source = snapshot(&rows);
        let mut loader = SourceLoader::default();
        loader
            .load_with(&source, &keys(&source), |batch| Ok(fetch(&rows, batch)))
            .unwrap();
        let uncovered = HashSet::from([source.items[1].key, source.items[2].key]);
        source.items.pop();
        loader.retain(&source, &uncovered);
        assert_eq!(
            loader
                .cached(&source)
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            vec![2]
        );
        assert_eq!(loader.cached_bytes, 200);
    }

    #[test]
    fn user_and_both_conversation_identities_scope_cached_rows() {
        type Change = fn(&mut CompactionSnapshot);
        let changes: &[Change] = &[
            |source| source.user_id = Uuid::from_u128(3_000),
            |source| source.conversation_id += 1,
            |source| source.conversation_uuid = Uuid::from_u128(4_000),
        ];
        for change in changes {
            let rows = vec![row(1, 100)];
            let mut source = snapshot(&rows);
            let mut loader = SourceLoader::default();
            loader
                .load_with(&source, &keys(&source), |batch| Ok(fetch(&rows, batch)))
                .unwrap();
            change(&mut source);
            assert!(loader.cached(&source).is_empty());
            let mut executions = 0;
            loader
                .load_with(&source, &keys(&source), |batch| {
                    executions += 1;
                    Ok(fetch(&rows, batch))
                })
                .unwrap();
            assert_eq!(executions, 1);
        }
    }

    #[test]
    fn transport_batches_bound_rows_without_dropping_any() {
        let rows: Vec<_> = (1..=257).map(|id| row(id, 1)).collect();
        let source = snapshot(&rows);
        let mut loader = SourceLoader::default();
        let mut counts = Vec::new();
        let loaded = loader
            .load_with(&source, &keys(&source), |batch| {
                counts.push(batch.len());
                Ok(fetch(&rows, batch))
            })
            .unwrap();
        assert_eq!(counts, vec![128, 128, 1]);
        assert_eq!(loaded.len(), rows.len());
        assert_eq!(loaded.iter().map(payload_bytes).sum::<usize>(), 257);
    }

    #[test]
    fn transport_batches_bound_estimated_ciphertext_bytes() {
        let rows: Vec<_> = (1..=5).map(|id| row(id, 3 * 1024 * 1024)).collect();
        let source = snapshot(&rows);
        let mut loader = SourceLoader::default();
        let mut sizes = Vec::new();
        let loaded = loader
            .load_with(&source, &keys(&source), |batch| {
                let loaded = fetch(&rows, batch);
                sizes.push(loaded.iter().map(payload_bytes).sum::<usize>());
                Ok(loaded)
            })
            .unwrap();
        assert_eq!(
            sizes,
            vec![6 * 1024 * 1024, 6 * 1024 * 1024, 3 * 1024 * 1024]
        );
        assert_eq!(loaded.len(), 5);
    }

    #[test]
    fn oversized_single_row_is_returned_without_caching_or_truncating() {
        let rows = vec![row(1, MAX_CACHE_BYTES + 1)];
        let source = snapshot(&rows);
        let mut loader = SourceLoader::default();
        for _ in 0..2 {
            let mut executions = 0;
            let loaded = loader
                .load_with(&source, &keys(&source), |batch| {
                    executions += 1;
                    assert_eq!(batch.len(), 1);
                    Ok(fetch(&rows, batch))
                })
                .unwrap();
            assert_eq!(executions, 1);
            assert_eq!(payload_bytes(&loaded[0]), MAX_CACHE_BYTES + 1);
            assert!(loader.cached(&source).is_empty());
            assert_eq!(loader.cached_bytes, 0);
        }
    }

    #[test]
    fn cache_evicts_old_rows_at_byte_cap_and_reuses_retained_rows() {
        let rows = vec![row(1, 9 * 1024 * 1024), row(2, 9 * 1024 * 1024)];
        let source = snapshot(&rows);
        let mut loader = SourceLoader::default();
        loader
            .load_with(&source, &keys(&source), |batch| Ok(fetch(&rows, batch)))
            .unwrap();
        assert_eq!(
            loader
                .cached(&source)
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            vec![2]
        );
        assert!(loader.cached_bytes <= MAX_CACHE_BYTES);
        loader
            .load_with(&source, &[source.items[1].key], |_| {
                panic!("retained row must hit cache")
            })
            .unwrap();
        let mut executions = 0;
        loader
            .load_with(&source, &[source.items[0].key], |batch| {
                executions += 1;
                Ok(fetch(&rows, batch))
            })
            .unwrap();
        assert_eq!(executions, 1);
        assert_eq!(
            loader
                .cached(&source)
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            vec![1]
        );
    }

    #[test]
    fn empty_payloads_are_valid_but_cache_entry_count_is_bounded() {
        let mut rows: Vec<_> = (1..=(MAX_CACHE_ROWS + 1) as u128)
            .map(|id| row(id, 0))
            .collect();
        for row in &mut rows {
            row.message_type = "assistant".into();
            row.content_enc = None;
        }
        let source = snapshot(&rows);
        let mut loader = SourceLoader::default();
        let loaded = loader
            .load_with(&source, &keys(&source), |batch| Ok(fetch(&rows, batch)))
            .unwrap();
        assert_eq!(loaded.len(), MAX_CACHE_ROWS + 1);
        assert_eq!(loader.cached(&source).len(), MAX_CACHE_ROWS);
        assert_eq!(loader.cached_bytes, 0);
    }

    #[test]
    fn invalid_or_duplicate_selection_fails_without_database_access() {
        let rows = vec![row(1, 100)];
        let source = snapshot(&rows);
        let key = source.items[0].key;
        let missing = ContextItemKey {
            uuid: Uuid::from_u128(999),
            ..key
        };
        let mut loader = SourceLoader::default();
        for selection in [vec![key, key], vec![missing]] {
            assert!(is_invalid(loader.load_with(
                &source,
                &selection,
                |_| panic!("invalid selection must not query")
            )));
        }
    }

    #[test]
    fn changed_or_missing_payload_fails_closed_before_cache_insertion() {
        let rows = vec![row(1, 100)];
        let source = snapshot(&rows);
        let mut loader = SourceLoader::default();
        assert!(is_invalid(loader.load_with(
            &source,
            &keys(&source),
            |_| Ok(Vec::new())
        )));
        assert!(loader.cached(&source).is_empty());
        let mut changed = rows[0].clone();
        changed.content_enc.as_mut().unwrap()[0] ^= 1;
        assert!(is_invalid(loader.load_with(
            &source,
            &keys(&source),
            |_| Ok(vec![changed.clone()])
        )));
        changed = rows[0].clone();
        changed.token_count = Some(99);
        assert!(is_invalid(loader.load_with(
            &source,
            &keys(&source),
            |_| Ok(vec![changed.clone()])
        )));
        assert!(loader.cached(&source).is_empty());
    }

    #[test]
    fn all_five_storage_projections_match_source_metadata() {
        let rows: Vec<_> = [
            ContextItemKind::User,
            ContextItemKind::Assistant,
            ContextItemKind::ToolCall,
            ContextItemKind::ToolOutput,
            ContextItemKind::Reasoning,
        ]
        .into_iter()
        .enumerate()
        .map(|(index, kind)| {
            let mut raw = row(index as u128 + 1, 100);
            raw.message_type = kind.as_str().into();
            raw.model = matches!(kind, ContextItemKind::User | ContextItemKind::Assistant)
                .then(|| "deepseek-v4-1-flash".into());
            raw.tool_call_id = match kind {
                ContextItemKind::ToolCall => Some(raw.uuid),
                ContextItemKind::ToolOutput => Some(Uuid::from_u128(3)),
                _ => None,
            };
            raw.tool_name = matches!(
                kind,
                ContextItemKind::ToolCall | ContextItemKind::ToolOutput
            )
            .then(|| "search".into());
            raw.finish_reason = (kind == ContextItemKind::Assistant).then(|| "stop".into());
            raw
        })
        .collect();
        let source = snapshot(&rows);
        let mut loader = SourceLoader::default();
        let loaded = loader
            .load_with(&source, &keys(&source), |batch| {
                let mut loaded = fetch(&rows, batch);
                loaded.reverse();
                Ok(loaded)
            })
            .unwrap();
        assert_eq!(
            loaded
                .iter()
                .map(|row| row.message_type.as_str())
                .collect::<Vec<_>>(),
            vec!["user", "assistant", "tool_call", "tool_output", "reasoning"]
        );
        assert_eq!(loader.cached(&source).len(), 5);
    }

    #[test]
    fn duplicate_or_unselected_fetched_rows_fail_closed() {
        let rows = vec![row(1, 100), row(2, 200)];
        let source = snapshot(&rows);
        let mut loader = SourceLoader::default();
        assert!(is_invalid(loader.load_with(
            &source,
            &keys(&source),
            |_| { Ok(vec![rows[0].clone(), rows[0].clone()]) }
        )));
        assert!(loader.cached(&source).is_empty());
        assert!(is_invalid(loader.load_with(
            &source,
            &[source.items[0].key],
            |_| { Ok(vec![rows[1].clone()]) }
        )));
        assert!(loader.cached(&source).is_empty());
    }

    #[test]
    fn user_content_hash_is_validated_and_attachment_digest_invalidates_reuse() {
        let mut raw = row(1, 100);
        raw.message_type = "user".into();
        let rows = vec![raw];
        let mut source = snapshot(&rows);
        let mut loader = SourceLoader::default();
        loader
            .load_with(&source, &keys(&source), |batch| Ok(fetch(&rows, batch)))
            .unwrap();
        source.items[0].content_fingerprint = source.items[0]
            .content_fingerprint
            .replace(":-", ":changed-attachment");
        assert!(loader.cached(&source).is_empty());
        loader.retain(&source, &keys(&source).into_iter().collect());
        assert!(loader.items.is_empty());
    }
}
