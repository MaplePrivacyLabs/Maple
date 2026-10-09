//! Host-side readers of the account stores.
//!
//! The Goose usage ledger is owned and written by the runtime; this module
//! opens it read-only. The tool summary store is owned by the host and
//! written here. Both are SQLite, so every call is blocking and runs on a
//! blocking thread.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::host::{UsageRow, UsageSummary};

/// Open the goose sessions database for reading. Returns `None` when the
/// file does not exist yet (read-only open never creates it). The busy
/// timeout covers the short locks goose takes for WAL checkpoints.
fn open_session_db_read_only(path: &Path) -> Option<rusqlite::Connection> {
    use rusqlite::OpenFlags;
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_URI;
    let conn = match rusqlite::Connection::open_with_flags(path, flags) {
        Ok(conn) => conn,
        Err(error) => {
            if path.exists() {
                log::warn!("Cannot open session db {}: {error}", path.display());
            }
            return None;
        }
    };
    if let Err(error) = conn.busy_timeout(std::time::Duration::from_secs(5)) {
        log::warn!("Cannot set busy timeout on {}: {error}", path.display());
    }
    Some(conn)
}

/// Open (and create) the tool summary store.
fn open_summary_db(path: &Path) -> Result<rusqlite::Connection, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("Cannot create {}: {error}", parent.display()))?;
    }
    let conn = rusqlite::Connection::open(path)
        .map_err(|error| format!("Cannot open {}: {error}", path.display()))?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; \
         CREATE TABLE IF NOT EXISTS tool_summaries ( \
             session_id TEXT NOT NULL, \
             item_id TEXT NOT NULL, \
             summary TEXT NOT NULL, \
             PRIMARY KEY (session_id, item_id) \
         );",
    )
    .map_err(|error| format!("Cannot init {}: {error}", path.display()))?;
    Ok(conn)
}

/// Open handles to one account's stores. The usage ledger is polled every
/// second during a run, so its connection is kept open rather than
/// reopened per query.
#[derive(Default)]
pub(super) struct AccountStores {
    usage_db: Mutex<Option<(PathBuf, rusqlite::Connection)>>,
    summary_db: Mutex<Option<(PathBuf, rusqlite::Connection)>>,
}

impl AccountStores {
    /// Run `f` against the usage ledger at `path`, or `None` when the
    /// ledger does not exist yet. Blocking.
    pub(super) fn with_usage_db<T>(
        &self,
        path: &Path,
        f: impl FnOnce(&rusqlite::Connection) -> T,
    ) -> Option<T> {
        let mut guard = self
            .usage_db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.as_ref().map(|(open, _)| open != path).unwrap_or(true) {
            let conn = open_session_db_read_only(path)?;
            *guard = Some((path.to_path_buf(), conn));
        }
        Some(f(&guard.as_ref().expect("usage db opened above").1))
    }

    /// Run `f` against the summary store at `path`. Blocking.
    pub(super) fn with_summary_db<T>(
        &self,
        path: &Path,
        f: impl FnOnce(&rusqlite::Connection) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut guard = self
            .summary_db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.as_ref().map(|(open, _)| open != path).unwrap_or(true) {
            *guard = Some((path.to_path_buf(), open_summary_db(path)?));
        }
        f(&guard.as_ref().expect("summary db opened above").1)
            .map_err(|error| format!("{error} ({})", path.display()))
    }
}

/// Latest context tokens for a session from the usage ledger: input plus
/// cache reads and writes of the newest non-compaction row.
pub(super) fn latest_context_tokens(conn: &rusqlite::Connection, session_id: &str) -> Option<i64> {
    conn.query_row(
        "SELECT COALESCE(input_tokens,0) + COALESCE(cache_read_tokens,0) \
         + COALESCE(cache_write_tokens,0) FROM usage_ledger \
         WHERE session_id = ?1 AND is_compaction = 0 \
         ORDER BY id DESC LIMIT 1",
        [session_id],
        |row| row.get::<_, i64>(0),
    )
    .ok()
}

pub(super) fn load_tool_summaries(
    conn: &rusqlite::Connection,
    session_id: &str,
) -> Result<std::collections::HashMap<String, String>, String> {
    let mut stmt = conn
        .prepare("SELECT item_id, summary FROM tool_summaries WHERE session_id = ?1")
        .map_err(|error| format!("Cannot prepare the tool summary query: {error}"))?;
    let rows = stmt
        .query_map([session_id], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(|error| format!("Cannot read tool summaries for task {session_id}: {error}"))?;
    rows.collect::<Result<std::collections::HashMap<_, _>, _>>()
        .map_err(|error| format!("Cannot read tool summaries for task {session_id}: {error}"))
}

pub(super) fn store_tool_summary(
    conn: &rusqlite::Connection,
    session_id: &str,
    item_id: &str,
    summary: &str,
) -> Result<(), String> {
    conn.execute(
        "INSERT OR REPLACE INTO tool_summaries (session_id, item_id, summary) \
         VALUES (?1, ?2, ?3)",
        [session_id, item_id, summary],
    )
    .map(|_| ())
    .map_err(|error| format!("Cannot store the tool summary for task {session_id}: {error}"))
}

/// Aggregate one account's ledger.
///
/// A subagent has a session of its own, and its provider calls land in
/// the ledger under it. Every row counts against the task that delegated
/// the work, so the reader sees what a task cost in total. Goose refuses
/// a subagent of a subagent, so resolving one parent is enough.
pub(super) fn usage_from_ledger(conn: &rusqlite::Connection) -> UsageSummary {
    let mut summary = UsageSummary::default();

    if let Ok(mut stmt) = conn.prepare(
        "SELECT COUNT(*), COALESCE(SUM(total_tokens),0), COALESCE(SUM(cost),0) \
         FROM usage_ledger",
    ) && let Ok(row) = stmt.query_row([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, f64>(2)?,
        ))
    }) {
        summary.totals = UsageRow {
            label: "All activity".to_string(),
            sessions: 0,
            turns: row.0.max(0) as u64,
            total_tokens: row.1,
            cost: row.2,
        };
    }

    if let Ok(mut stmt) = conn.prepare(
        "SELECT u.model, COUNT(DISTINCT COALESCE(s.parent_session_id, u.session_id)), COUNT(*), \
         COALESCE(SUM(u.total_tokens),0), COALESCE(SUM(u.cost),0) \
         FROM usage_ledger u LEFT JOIN sessions s ON s.id = u.session_id \
         GROUP BY u.model ORDER BY SUM(u.total_tokens) DESC",
    ) && let Ok(rows) = stmt.query_map([], |row| {
        Ok(UsageRow {
            label: row
                .get::<_, Option<String>>(0)?
                .unwrap_or_else(|| "unknown".into()),
            sessions: row.get::<_, i64>(1)?.max(0) as u64,
            turns: row.get::<_, i64>(2)?.max(0) as u64,
            total_tokens: row.get::<_, i64>(3)?,
            cost: row.get::<_, f64>(4)?,
        })
    }) {
        for row in rows.flatten() {
            summary.totals.sessions += row.sessions;
            summary.by_model.push(row);
        }
    }

    if let Ok(mut stmt) = conn.prepare(
        "SELECT COALESCE(parent.name, s.name), COALESCE(s.parent_session_id, u.session_id) AS task, \
         COUNT(*), COALESCE(SUM(u.total_tokens),0), COALESCE(SUM(u.cost),0) \
         FROM usage_ledger u JOIN sessions s ON s.id = u.session_id \
         LEFT JOIN sessions parent ON parent.id = s.parent_session_id \
         GROUP BY task ORDER BY MAX(u.created_timestamp) DESC LIMIT 20",
    ) && let Ok(rows) = stmt.query_map([], |row| {
        Ok(UsageRow {
            label: {
                let name: String = row.get::<_, Option<String>>(0)?.unwrap_or_default();
                let id: String = row.get(1)?;
                if name.trim().is_empty() { id } else { name }
            },
            sessions: 1,
            turns: row.get::<_, i64>(2)?.max(0) as u64,
            total_tokens: row.get::<_, i64>(3)?,
            cost: row.get::<_, f64>(4)?,
        })
    }) {
        for row in rows.flatten() {
            summary.by_session.push(row);
        }
    }

    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A subagent bills to the task that delegated the work, so the
    /// usage screen shows one row per task and not one per subagent.
    #[test]
    fn subagent_usage_counts_against_its_parent_task() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                 id TEXT PRIMARY KEY,
                 name TEXT NOT NULL DEFAULT '',
                 parent_session_id TEXT
             );
             CREATE TABLE usage_ledger (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id TEXT NOT NULL,
                 created_timestamp INTEGER NOT NULL,
                 model TEXT,
                 total_tokens INTEGER,
                 cost REAL
             );
             INSERT INTO sessions VALUES ('task-1', 'Review the parser', NULL);
             INSERT INTO sessions VALUES ('sub-1', 'Delegated task', 'task-1');
             INSERT INTO sessions VALUES ('task-2', 'Other work', NULL);
             INSERT INTO usage_ledger (session_id, created_timestamp, model, total_tokens, cost)
             VALUES ('task-1', 10, 'maple-1', 100, 1.0),
                    ('sub-1',  20, 'maple-1', 400, 4.0),
                    ('task-2', 30, 'maple-1', 700, 7.0);",
        )
        .unwrap();

        let usage = usage_from_ledger(&conn);
        let rows = usage
            .by_session
            .iter()
            .map(|row| (row.label.as_str(), row.turns, row.total_tokens))
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            vec![("Other work", 1, 700), ("Review the parser", 2, 500)],
            "the subagent's tokens belong to the task that delegated them"
        );
        // Two tasks ran, not three sessions.
        assert_eq!(usage.by_model.len(), 1);
        assert_eq!(usage.by_model[0].sessions, 2);
        assert_eq!(usage.totals.total_tokens, 1200);
    }

    #[test]
    fn tool_summaries_round_trip_and_replace() {
        let dir = std::env::temp_dir().join(format!(
            "maple-summaries-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("tool_summaries.db");
        let stores = AccountStores::default();
        stores
            .with_summary_db(&path, |conn| store_tool_summary(conn, "s1", "i1", "first"))
            .unwrap();
        stores
            .with_summary_db(&path, |conn| store_tool_summary(conn, "s1", "i1", "second"))
            .unwrap();
        stores
            .with_summary_db(&path, |conn| store_tool_summary(conn, "s2", "i9", "other"))
            .unwrap();
        let loaded = stores
            .with_summary_db(&path, |conn| load_tool_summaries(conn, "s1"))
            .unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.get("i1").map(String::as_str), Some("second"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_missing_ledger_reads_as_none() {
        let stores = AccountStores::default();
        let missing = std::env::temp_dir()
            .join("maple-no-such-dir")
            .join("sessions.db");
        assert!(
            stores
                .with_usage_db(&missing, |conn| latest_context_tokens(conn, "s"))
                .is_none()
        );
    }
}
