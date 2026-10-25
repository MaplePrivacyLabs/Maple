//! Explicitly enabled synthetic-fixture diagnostics. No raw SQL, connection or
//! transaction escapes this API. Product builds do not enable store-bench.

use std::fs::{File, OpenOptions, TryLockError};
use std::sync::Arc;
use std::time::Instant;

use pi_coding_agent::session::{SESSION_VERSION, SessionEntry, SessionHeader};
use pi_coding_agent::store::SessionStore;
use rusqlite::params;
use serde::Serialize;

use super::projector::{PreparedRebuild, RebuildProgress};
use super::session::{SessionLoad, SqliteSessionStore};
use super::{AccountId, Durability, Store, StoreError, StorePaths, StoreResult};

pub struct FixtureOwner(File);

impl FixtureOwner {
    /// Uses the same OS ownership lock as the host. Never reads previous data.
    pub fn acquire(paths: &StorePaths) -> StoreResult<Self> {
        let paths = paths.prepare()?;
        let path = paths.owner_lock();
        let file = match super::paths::private_new_file(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                super::paths::verify_private_path(&path, false)?;
                let mut options = OpenOptions::new();
                options.read(true).write(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
                }
                options.open(path)?
            }
            Err(error) => return Err(error.into()),
        };
        match file.try_lock() {
            Ok(()) => Ok(Self(file)),
            Err(TryLockError::WouldBlock) => Err(StoreError::Busy),
            Err(TryLockError::Error(error)) => Err(error.into()),
        }
    }
}

impl Drop for FixtureOwner {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

pub struct SourceWriter {
    pub(super) adapter: SqliteSessionStore,
    pub(super) header: SessionHeader,
    pub(super) initialized: bool,
}

pub struct SeedEntry {
    pub writer: usize,
    pub entry: SessionEntry,
}

impl SourceWriter {
    /// Timed writes use exactly the production synchronous SessionStore path.
    pub fn append(&mut self, entry: &SessionEntry) -> StoreResult<()> {
        if self.initialized {
            self.adapter.append(entry)?;
        } else {
            self.adapter
                .write_all(&self.header, std::slice::from_ref(entry))?;
            self.initialized = true;
        }
        Ok(())
    }

    /// Seed-only first write: all normal entry/projector/cache work still runs,
    /// with one Durable transaction for this synthetic initial tree.
    pub fn initial_seed(&mut self, entries: &[SessionEntry]) -> StoreResult<()> {
        if self.initialized {
            return Err(StoreError::Conflict);
        }
        self.adapter.write_all(&self.header, entries)?;
        self.initialized = true;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct EffectivePragmas {
    pub role: &'static str,
    pub synchronous: i64,
    pub fullfsync: i64,
    pub checkpoint_fullfsync: i64,
    pub wal_autocheckpoint: i64,
    pub cache_size: i64,
    pub query_only: i64,
    pub temp_store: i64,
    pub secure_delete: i64,
    pub journal_mode: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct PlanCheck {
    pub name: &'static str,
    pub statistics: bool,
    pub expected_index: &'static str,
    pub details: Vec<String>,
    pub passed: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ContextLoad {
    pub entries: usize,
    pub context_tokens: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct RebuildChunk {
    pub prepare_micros: u64,
    pub writer_micros: u64,
    pub entries: usize,
    pub bytes: usize,
    pub done: bool,
    pub wal_before_bytes: u64,
    pub wal_after_bytes: u64,
    pub wal_file_delta_bytes: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct OldEpochCollectionChunk {
    pub writer_micros: u64,
    pub rows: usize,
    pub bytes: usize,
    pub wal_before_bytes: u64,
    pub wal_after_bytes: u64,
    pub wal_file_delta_bytes: i64,
}

pub struct Rebuild {
    prepared: PreparedRebuild,
}

impl Store {
    pub fn bench_seed_batch(
        &self,
        writers: &mut [SourceWriter],
        inputs: &[SeedEntry],
    ) -> StoreResult<()> {
        super::session::fixture::seed_batch(self, writers, inputs)
    }

    pub fn bench_signed_in_account(&self) -> StoreResult<Option<AccountId>> {
        use rusqlite::OptionalExtension;
        self.read(|tx|Ok(tx.query_row("SELECT c.account_id FROM credentials c JOIN accounts a ON a.id=c.account_id WHERE c.slot=1 AND a.deletion_phase IS NULL AND a.deleting_ms IS NULL",[],|row|row.get(0)).optional()?.map(AccountId)))
    }

    pub fn bench_source_writer(
        self: &Arc<Self>,
        account: AccountId,
        uuid: &str,
    ) -> StoreResult<SourceWriter> {
        let task = self.task(account, uuid)?.ok_or(StoreError::NotFound)?;
        let loaded = self.load_session(account, uuid, SessionLoad::Context)?;
        let initialized = loaded.header.is_some();
        let project = self
            .project_by_key(account, task.project)?
            .ok_or(StoreError::NotFound)?;
        let header = loaded.header.unwrap_or(SessionHeader {
            version: SESSION_VERSION,
            id: uuid.to_owned(),
            timestamp: task.created_ms,
            cwd: project.root,
            parent_session: None,
        });
        Ok(SourceWriter {
            adapter: loaded.store,
            header,
            initialized,
        })
    }

    pub fn bench_context_load(
        self: &Arc<Self>,
        account: AccountId,
        uuid: &str,
        partial: bool,
    ) -> StoreResult<ContextLoad> {
        let loaded = self.load_session(
            account,
            uuid,
            if partial {
                SessionLoad::Context
            } else {
                SessionLoad::Full
            },
        )?;
        let entries = loaded.entries.len();
        let manager = loaded.into_manager("/synthetic");
        let context_tokens = pi_coding_agent::compaction::estimate_projected_context_tokens(
            &manager.projection(),
            &manager.branch(),
        );
        Ok(ContextLoad {
            entries,
            context_tokens,
        })
    }

    pub fn bench_effective_pragmas(&self) -> StoreResult<Vec<EffectivePragmas>> {
        fn inspect(
            connection: &rusqlite::Connection,
            role: &'static str,
        ) -> StoreResult<EffectivePragmas> {
            let integer =
                |name| connection.pragma_query_value(None, name, |row| row.get::<_, i64>(0));
            Ok(EffectivePragmas {
                role,
                synchronous: integer("synchronous")?,
                fullfsync: integer("fullfsync")?,
                checkpoint_fullfsync: integer("checkpoint_fullfsync")?,
                wal_autocheckpoint: integer("wal_autocheckpoint")?,
                cache_size: integer("cache_size")?,
                query_only: integer("query_only")?,
                temp_store: integer("temp_store")?,
                secure_delete: integer("secure_delete")?,
                journal_mode: connection
                    .pragma_query_value(None, "journal_mode", |row| row.get(0))?,
            })
        }
        let mut result = Vec::new();
        let writer = self.writer.lock().map_err(|_| StoreError::Busy)?;
        result.push(inspect(writer.as_ref().ok_or(StoreError::Busy)?, "writer")?);
        drop(writer);
        for (index, reader) in self.readers.iter().enumerate() {
            let reader = reader.lock().map_err(|_| StoreError::Busy)?;
            result.push(inspect(
                reader.as_ref().ok_or(StoreError::Busy)?,
                if index == 0 { "reader-1" } else { "reader-2" },
            )?);
        }
        let bulk = self.bulk.lock().map_err(|_| StoreError::Busy)?;
        result.push(inspect(bulk.as_ref().ok_or(StoreError::Busy)?, "bulk")?);
        drop(bulk);
        let checkpoint = self.checkpointer.lock().map_err(|_| StoreError::Busy)?;
        result.push(inspect(
            checkpoint.as_ref().ok_or(StoreError::Busy)?,
            "checkpointer",
        )?);
        Ok(result)
    }

    pub fn bench_checkpoint(&self, truncate: bool) -> StoreResult<bool> {
        let connection = self.checkpointer.lock().map_err(|_| StoreError::Busy)?;
        super::connection::checkpoint(connection.as_ref().ok_or(StoreError::Busy)?, truncate)
    }

    /// One fresh actual-role connection, independently from the five-connection
    /// host startup. The caller measures this call as raw-key connection open.
    pub fn bench_raw_connection_open(&self) -> StoreResult<()> {
        let key = self.dek.lock().map_err(|_| StoreError::Busy)?;
        let initialize = |connection: &rusqlite::Connection| match key.as_ref() {
            Some(key) => super::engine::key_connection(connection, key),
            None => super::engine::guard_plain(connection),
        };
        let _connection = super::connection::open(
            &self.paths,
            super::connection::Role::Bulk,
            false,
            &initialize,
        )?;
        Ok(())
    }

    /// The extra diagnostic reader has a real snapshot. Pool traffic during the
    /// action remains possible; no database handle crosses into the callback.
    pub fn bench_with_held_reader<T>(
        &self,
        action: impl FnOnce() -> StoreResult<T>,
    ) -> StoreResult<T> {
        let key = self.dek.lock().map_err(|_| StoreError::Busy)?;
        let initialize = |connection: &rusqlite::Connection| match key.as_ref() {
            Some(key) => super::engine::key_connection(connection, key),
            None => super::engine::guard_plain(connection),
        };
        let mut connection = super::connection::open(
            &self.paths,
            super::connection::Role::Bulk,
            false,
            &initialize,
        )?;
        drop(key);
        let snapshot = connection.transaction()?;
        snapshot.query_row("SELECT count(*) FROM tasks", [], |_| Ok(()))?;
        let value = action()?;
        snapshot.commit()?;
        Ok(value)
    }

    pub fn bench_begin_rebuild(&self, account: AccountId, uuid: &str) -> StoreResult<Rebuild> {
        let ticket = self.begin_timeline_rebuild(account, uuid, false)?;
        Ok(Rebuild {
            prepared: self.prepare_timeline_rebuild(&ticket)?,
        })
    }

    pub fn bench_rebuild_chunk(&self, rebuild: &mut Rebuild) -> StoreResult<RebuildChunk> {
        let started = Instant::now();
        let chunk = self.prepare_timeline_chunk(&rebuild.prepared)?;
        let prepare_micros = micros(started);
        let wal_before_bytes = wal_bytes(self);
        let started = Instant::now();
        let progress = self.commit_timeline_chunk(&chunk, false)?;
        let writer_micros = micros(started);
        let wal_after_bytes = wal_bytes(self);
        let wal_file_delta_bytes = wal_delta(wal_before_bytes, wal_after_bytes);
        match progress {
            RebuildProgress::Pending { entries, bytes } => Ok(RebuildChunk {
                prepare_micros,
                writer_micros,
                entries,
                bytes,
                done: false,
                wal_before_bytes,
                wal_after_bytes,
                wal_file_delta_bytes,
            }),
            RebuildProgress::Replaced { .. } => Ok(RebuildChunk {
                prepare_micros,
                writer_micros,
                entries: 0,
                bytes: 0,
                done: true,
                wal_before_bytes,
                wal_after_bytes,
                wal_file_delta_bytes,
            }),
            RebuildProgress::NeedsSnapshot => Err(StoreError::Conflict),
        }
    }

    pub fn bench_collect_old_epoch(
        &self,
        account: AccountId,
        uuid: &str,
    ) -> StoreResult<OldEpochCollectionChunk> {
        let wal_before_bytes = wal_bytes(self);
        let started = Instant::now();
        let (rows, bytes) = self.collect_old_timeline_chunk_with_bytes(account, uuid)?;
        let writer_micros = micros(started);
        let wal_after_bytes = wal_bytes(self);
        Ok(OldEpochCollectionChunk {
            writer_micros,
            rows,
            bytes,
            wal_before_bytes,
            wal_after_bytes,
            wal_file_delta_bytes: wal_delta(wal_before_bytes, wal_after_bytes),
        })
    }

    /// Deterministic planner guards with and without statistics. Fixed queries
    /// exercise source, paging, projector merge/hide/plan and attachment metadata.
    pub fn bench_query_plans(
        &self,
        account: AccountId,
        uuid: &str,
        statistics: bool,
    ) -> StoreResult<Vec<PlanCheck>> {
        self.write(Durability::Normal, |tx| {
            if statistics {
                tx.execute_batch("ANALYZE")?;
            } else {
                let exists: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='sqlite_stat1')",
                    [],
                    |row| row.get(0),
                )?;
                if exists {
                    tx.execute_batch("DELETE FROM sqlite_stat1; ANALYZE sqlite_schema;")?;
                }
            }
            Ok(())
        })?;
        self.read(|tx| {
            let task=super::tasks::task_key_tx(tx,account,uuid)?;
            let tests=[
                ("list", "tasks_section", "SELECT id FROM tasks WHERE account_id=?1 AND state='active' AND provisional=0 AND message_count>0 AND deleted_ms IS NULL ORDER BY updated_ms DESC,id DESC LIMIT 50",account.0),
                ("source", "entries_seq", "SELECT payload FROM entries WHERE task_id=?1 AND seq>=1 ORDER BY seq LIMIT 128",task.0),
                ("project-section", "tasks_project_section", "SELECT id FROM tasks WHERE account_id=?1 AND project_id=1 AND state='active' AND provisional=0 AND message_count>0 AND deleted_ms IS NULL ORDER BY updated_ms DESC,id DESC LIMIT 50",account.0),
                ("pinned", "tasks_pinned", "SELECT id FROM tasks WHERE account_id=?1 AND pinned_ms IS NOT NULL AND state<>'archived' AND provisional=0 AND message_count>0 AND deleted_ms IS NULL ORDER BY updated_ms DESC,id DESC LIMIT 50",account.0),
                ("title-search", "tasks_recent", "SELECT id FROM tasks INDEXED BY tasks_recent WHERE account_id=?1 AND provisional=0 AND deleted_ms IS NULL AND message_count>0 AND instr(title_fold,'synthetic')>0 ORDER BY updated_ms DESC,id DESC LIMIT 100",account.0),
                ("projects", "projects_navigation", "SELECT id FROM projects WHERE account_id=?1 AND removed_ms IS NULL ORDER BY position IS NULL,coalesce(position,0),root,id LIMIT 50",account.0),
                ("history", "timeline_rows_seq", "SELECT item FROM timeline_rows WHERE task_id=?1 AND epoch=0 AND hidden=0 AND row_seq<40000 ORDER BY row_seq DESC LIMIT 200",task.0),
                ("merge", "timeline_rows_key", "SELECT id FROM timeline_rows WHERE task_id=?1 AND epoch=0 AND row_key='key' AND hidden=0 ORDER BY row_seq LIMIT 1",task.0),
                ("call", "timeline_rows_call", "SELECT row_key FROM timeline_rows WHERE task_id=?1 AND epoch=0 AND call_id='call' AND hidden=0 ORDER BY row_seq DESC LIMIT 1",task.0),
                ("hide", "timeline_rows_entry", "SELECT id FROM timeline_rows WHERE task_id=?1 AND epoch=0 AND entry_seq=2",task.0),
                ("plan", "timeline_rows_plan", "SELECT item FROM timeline_rows WHERE task_id=?1 AND epoch=0 AND is_plan=1 AND hidden=0 ORDER BY row_seq DESC LIMIT 1",task.0),
                ("attachment-unbound", "file_attachments_unbound", "SELECT public_id FROM file_attachments WHERE bound=0 AND updated_ms<?1 ORDER BY updated_ms LIMIT 1",1),
                ("attachment-collection", "file_attachments_task", "SELECT public_id FROM file_attachments WHERE task_id=?1 LIMIT 1000",task.0),
            ];
            let mut result=Vec::new();
            for (name,expected_index,query,parameter) in tests {
                let mut statement=tx.prepare(&format!("EXPLAIN QUERY PLAN {query}"))?;
                let details=statement.query_map(params![parameter],|row|row.get::<_,String>(3))?.collect::<rusqlite::Result<Vec<_>>>()?;
                let passed=plan_matches(name,expected_index,&details);
                result.push(PlanCheck {name,statistics,expected_index,details,passed});
            }
            Ok(result)
        })
    }
}

fn plan_matches(name: &str, expected_index: &str, details: &[String]) -> bool {
    if details
        .iter()
        .any(|detail| detail.contains("USE TEMP B-TREE"))
    {
        return false;
    }
    if name != "attachment-collection" {
        return details.iter().any(|detail| detail.contains(expected_index));
    }
    if details.iter().any(|detail| {
        detail
            .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
            .any(|word| word == "SCAN")
    }) {
        return false;
    }
    // The payload is on disk. Collection must seek only this task's metadata;
    // the index need not cover the selected public identifier.
    details.iter().any(|detail| {
        detail == "SEARCH file_attachments USING INDEX file_attachments_task (task_id=?)"
    })
}

fn micros(started: Instant) -> u64 {
    started.elapsed().as_micros().try_into().unwrap_or(u64::MAX)
}

fn wal_bytes(store: &Store) -> u64 {
    std::fs::metadata(store.paths().db().join("maple.db-wal")).map_or(0, |metadata| metadata.len())
}

fn wal_delta(before: u64, after: u64) -> i64 {
    i64::try_from(i128::from(after) - i128::from(before)).unwrap_or(if after >= before {
        i64::MAX
    } else {
        i64::MIN
    })
}

/// Process-lifetime high-water RSS, including synthetic fixture generation.
/// This does not claim a per-scenario or production-host working set.
pub fn process_peak_rss_bytes() -> Option<u64> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        // getrusage initializes this fixed-size OS structure on success.
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
            return None;
        }
        let usage = unsafe { usage.assume_init() };
        let bytes = u64::try_from(usage.ru_maxrss).ok()?;
        if cfg!(target_os = "macos") {
            Some(bytes)
        } else {
            bytes.checked_mul(1024)
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{NewTask, TaskKind};

    const FIXTURE_A: &str = "018ddbcc-b80b-7c20-8123-000000000001";
    const FIXTURE_B: &str = "018ddbcc-b80b-7c20-8123-000000000002";

    fn user(id: &str, parent: Option<&str>, timestamp: i64) -> SessionEntry {
        SessionEntry {
            id: id.into(),
            parent_id: parent.map(str::to_owned),
            timestamp,
            kind: pi_coding_agent::session::EntryKind::Message {
                message: pi_coding_agent::SessionMessage::Llm(pi_ai::Message::User(
                    pi_ai::UserMessage {
                        content: vec![pi_ai::Content::text("synthetic fixture")],
                        timestamp,
                    },
                )),
            },
        }
    }

    fn fixture() -> (tempfile::TempDir, Arc<Store>, AccountId, Vec<SourceWriter>) {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(StorePaths::new(temp.path())).unwrap());
        let account = store.upsert_account("fixture", 1).unwrap();
        let mut writers = Vec::new();
        for id in [FIXTURE_A, FIXTURE_B] {
            store
                .create_task(
                    account,
                    &NewTask {
                        uuid: id.into(),
                        project_root: temp.path().to_string_lossy().into_owned(),
                        kind: TaskKind::Desktop,
                        title: id.into(),
                        provisional: false,
                        model: Some("fixture-model".into()),
                        created_ms: 1,
                    },
                )
                .unwrap();
            writers.push(store.bench_source_writer(account, id).unwrap());
        }
        (temp, store, account, writers)
    }

    #[test]
    fn seed_is_globally_interleaved_and_has_production_cache_facts() {
        let (_temp, store, account, mut writers) = fixture();
        store
            .bench_seed_batch(
                &mut writers,
                &[
                    SeedEntry {
                        writer: 0,
                        entry: user("a0", None, 10),
                    },
                    SeedEntry {
                        writer: 1,
                        entry: user("b0", None, 11),
                    },
                    SeedEntry {
                        writer: 0,
                        entry: user("a1", Some("a0"), 12),
                    },
                    SeedEntry {
                        writer: 1,
                        entry: user("b1", Some("b0"), 13),
                    },
                ],
            )
            .unwrap();
        let ids = store
            .read(|tx| {
                Ok(tx
                    .prepare("SELECT entry_id FROM entries ORDER BY id")?
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .unwrap();
        assert_eq!(ids, ["a0", "b0", "a1", "b1"]);
        for task in [FIXTURE_A, FIXTURE_B] {
            let facts = store.task_facts(account, task).unwrap();
            assert_eq!(facts.message_count, 2);
            assert_eq!(
                facts.context_tokens,
                store
                    .bench_context_load(account, task, false)
                    .unwrap()
                    .context_tokens
            );
        }
    }

    #[test]
    fn failed_seed_batch_rolls_back_every_task_and_keeps_adapter_state() {
        let (_temp, store, account, mut writers) = fixture();
        let result = store.bench_seed_batch(
            &mut writers,
            &[
                SeedEntry {
                    writer: 0,
                    entry: user("a0", None, 10),
                },
                SeedEntry {
                    writer: 1,
                    entry: user("b0", None, 11),
                },
                SeedEntry {
                    writer: 0,
                    entry: user("a1", Some("a0"), 12),
                },
                SeedEntry {
                    writer: 0,
                    entry: user("a0", Some("a1"), 13),
                },
            ],
        );
        assert!(result.is_err());
        assert!(!writers[0].initialized && !writers[1].initialized);
        assert_eq!(store.task(account, FIXTURE_A).unwrap().unwrap().last_seq, 0);
        assert_eq!(store.task(account, FIXTURE_B).unwrap().unwrap().last_seq, 0);
        store
            .bench_seed_batch(
                &mut writers,
                &[SeedEntry {
                    writer: 0,
                    entry: user("a0", None, 10),
                }],
            )
            .unwrap();
        assert_eq!(store.task(account, FIXTURE_A).unwrap().unwrap().last_seq, 1);
    }

    #[test]
    fn seed_refuses_an_existing_owner() {
        let temp = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(temp.path());
        let _first = FixtureOwner::acquire(&paths).unwrap();
        assert!(matches!(
            FixtureOwner::acquire(&paths),
            Err(StoreError::Busy)
        ));
    }

    #[test]
    fn attachment_plan_requires_the_task_metadata_index() {
        let (_temp, store, account, _writers) = fixture();
        for (predicate, accepted) in [
            ("task_id=?1", true),
            ("task_id>?1", false),
            ("account_id=?1", false),
        ] {
            let details = store.read(|tx| {
                    let task = super::super::tasks::task_key_tx(tx, account, FIXTURE_A)?;
                    let mut statement = tx.prepare(&format!(
                        "EXPLAIN QUERY PLAN SELECT public_id FROM file_attachments INDEXED BY file_attachments_task WHERE {predicate} LIMIT 1000"))?;
                    Ok(statement.query_map([task.0], |row| row.get::<_, String>(3))?
                        .collect::<rusqlite::Result<Vec<_>>>()?)
                }).unwrap();
            assert_eq!(
                plan_matches("attachment-collection", "file_attachments_task", &details),
                accepted,
                "{details:?}"
            );
        }
    }

    #[test]
    fn old_epoch_evidence_counts_the_payloads_deleted_by_the_real_collector() {
        let (_temp, store, account, mut writers) = fixture();
        writers[0].append(&user("a0", None, 10)).unwrap();
        writers[0].append(&user("a1", Some("a0"), 11)).unwrap();
        let expected = store
            .read(|tx| {
                let task = super::super::tasks::task_key_tx(tx, account, FIXTURE_A)?;
                Ok(tx.query_row(
                    "SELECT count(*),sum(bytes) FROM timeline_rows WHERE task_id=?1",
                    [task.0],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                )?)
            })
            .unwrap();
        let mut rebuild = store.bench_begin_rebuild(account, FIXTURE_A).unwrap();
        while !store.bench_rebuild_chunk(&mut rebuild).unwrap().done {}
        let chunk = store.bench_collect_old_epoch(account, FIXTURE_A).unwrap();
        assert_eq!(
            (
                i64::try_from(chunk.rows).unwrap(),
                i64::try_from(chunk.bytes).unwrap()
            ),
            expected
        );
        assert_eq!(
            chunk.wal_file_delta_bytes,
            wal_delta(chunk.wal_before_bytes, chunk.wal_after_bytes)
        );
        assert_eq!(
            store
                .bench_collect_old_epoch(account, FIXTURE_A)
                .unwrap()
                .rows,
            0
        );
    }
}
