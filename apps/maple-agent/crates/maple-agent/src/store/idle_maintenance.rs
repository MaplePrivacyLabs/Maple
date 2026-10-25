//! Small physical-storage and planner maintenance chunks, admitted only while idle.

use super::{Store, StoreResult};

const VACUUM_PAGE_CAP: u16 = 64;

impl Store {
    /// Reclaim at most 64 freelist pages per call; physical truncation follows
    /// the normal checkpoint policy. Admission uses the same nonblocking writer
    /// gate as collectors, so queued foreground writes take precedence. The
    /// caller schedules planner maintenance less frequently than vacuum chunks.
    pub(crate) fn idle_storage_maintenance(&self, optimize: bool) -> StoreResult<()> {
        self.write_idle(|transaction| {
            // incremental_vacuum returns one row after each page. Executing only
            // its first step would reclaim one page regardless of the limit.
            let mut vacuum =
                transaction.prepare(&format!("PRAGMA incremental_vacuum({VACUUM_PAGE_CAP})"))?;
            let mut rows = vacuum.query([])?;
            while rows.next()?.is_some() {}
            drop(rows);
            drop(vacuum);

            if optimize {
                // The linked SQLite's default optimize mask applies its bounded
                // analysis_limit; do not substitute an unbounded ANALYZE.
                transaction.execute_batch("PRAGMA optimize")?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use rusqlite::params;

    use super::*;
    use crate::store::{Durability, StoreError, StorePaths, keys::LockedKey};

    fn fixture(keyed: bool) -> (tempfile::TempDir, Store) {
        let directory = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(directory.path());
        let store = if keyed {
            Store::open_keyed(paths, LockedKey::from_bytes([0x36; 32]).unwrap()).unwrap()
        } else {
            Store::open(paths).unwrap()
        };
        (directory, store)
    }

    fn free_pages(store: &Store) -> i64 {
        store
            .read(|transaction| {
                Ok(transaction.pragma_query_value(None, "freelist_count", |row| row.get(0))?)
            })
            .unwrap()
    }

    fn seed_deleted_source(store: &Store) -> Vec<u8> {
        let payload = |id, size| {
            serde_json::to_vec(&pi_coding_agent::session::SessionEntry {
                id: format!("entry{id}"),
                parent_id: None,
                timestamp: 1,
                kind: pi_coding_agent::session::EntryKind::CustomMessage {
                    custom_type: "vacuum-fixture".into(),
                    content: vec![pi_ai::Content::text("x".repeat(size))],
                    display: false,
                    details: None,
                },
            })
            .unwrap()
        };
        let retired = payload(1, 1024 * 1024);
        let live = payload(2, 64 * 1024);
        store
            .write(Durability::Durable, |transaction| {
                transaction.execute("INSERT INTO accounts(id,user_key,user_id,created_ms,last_seen_ms) VALUES (1,'a','A',1,1)", [])?;
                transaction.execute("INSERT INTO projects(id,account_id,root,last_used_ms) VALUES (1,1,'/vacuum-fixture',1)", [])?;
                transaction.execute("INSERT INTO tasks(id,uuid,account_id,project_id,kind,state,title,title_fold,created_ms,updated_ms) VALUES (1,'vacuum-task',1,1,'desktop','settled','vacuum','vacuum',1,1)", [])?;
                for (id, bytes) in [(1, &retired), (2, &live)] {
                    // Leave the fixture uncompressed to create real free pages.
                    transaction.execute(
                        "INSERT INTO entries(id,task_id,seq,entry_id,kind,created_ms,bytes,payload) \
                         VALUES (?1,1,?1,?2,'custom_message',1,?3,?4)",
                        params![id, format!("entry{id}"), bytes.len() as i64, bytes],
                    )?;
                }
                Ok(())
            })
            .unwrap();
        store
            .write(Durability::Durable, |transaction| {
                transaction.execute("DELETE FROM entries WHERE id=1", [])?;
                Ok(())
            })
            .unwrap();
        live
    }

    fn assert_live_source(store: &Store, expected: &[u8]) {
        let actual: Vec<u8> = store
            .read(|transaction| {
                Ok(
                    transaction.query_row("SELECT payload FROM entries WHERE id=2", [], |row| {
                        row.get(0)
                    })?,
                )
            })
            .unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn incremental_chunks_reclaim_multiple_pages_with_a_fixed_bound_and_preserve_live_source() {
        for keyed in [false, true] {
            let (_directory, store) = fixture(keyed);
            let live = seed_deleted_source(&store);
            let mut before = free_pages(&store);
            assert!(before > i64::from(VACUUM_PAGE_CAP) * 3);

            store.idle_storage_maintenance(false).unwrap();
            let after = free_pages(&store);
            assert_eq!(before - after, i64::from(VACUUM_PAGE_CAP));
            assert_live_source(&store, &live);
            before = after;

            for _ in 0..16 {
                store.idle_storage_maintenance(false).unwrap();
                let after = free_pages(&store);
                assert!((0..=i64::from(VACUUM_PAGE_CAP)).contains(&(before - after)));
                assert_live_source(&store, &live);
                if after == 0 {
                    break;
                }
                assert!(after < before);
                before = after;
            }
            assert_eq!(free_pages(&store), 0);
            // Repeated idle scheduling remains harmless once reclamation ends.
            store.idle_storage_maintenance(true).unwrap();
            assert_live_source(&store, &live);
            store.close().unwrap();
        }
    }

    #[test]
    fn maintenance_yields_to_foreground_and_refuses_a_busy_or_closed_writer() {
        let (_directory, store) = fixture(false);
        store.foreground_waiters.store(1, Ordering::Release);
        assert_eq!(store.idle_storage_maintenance(true), Err(StoreError::Busy));
        store.foreground_waiters.store(0, Ordering::Release);

        let writer = store.writer.lock().unwrap();
        assert_eq!(store.idle_storage_maintenance(true), Err(StoreError::Busy));
        drop(writer);

        store.close().unwrap();
        assert_eq!(store.idle_storage_maintenance(true), Err(StoreError::Busy));
    }
}
