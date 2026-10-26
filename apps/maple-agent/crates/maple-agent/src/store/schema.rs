//! The one migration chain for all new-store state.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use super::{StoreError, StoreResult};

pub(super) const APPLICATION_ID: i64 = 1_296_126_028;
pub(super) const SCHEMA_EPOCH: i64 = 1;
pub(super) const VERSION: i64 = 4;

/// Original migration schema, including partial indexes and constraints.
/// Validation executes the complete append-only migration chain as its reference.
pub(super) const REFERENCE_DDL: &str = r#"
-- migration 1 (during development each store PR appends a migration; see §19.1)

-- ───────────── store ─────────────
CREATE TABLE store_meta (
  key    TEXT PRIMARY KEY,             -- 'store_id' (16 random bytes, for diagnostics), 'created_ms',
  value  ANY NOT NULL                  -- 'schema_epoch', 'last_integrity_ms', 'setup_operation_id'
) STRICT, WITHOUT ROWID;

-- ───────────── accounts and credentials ─────────────
CREATE TABLE accounts (
  id            INTEGER PRIMARY KEY,
  user_key      TEXT NOT NULL UNIQUE,  -- trim + ASCII-lowercase(user id): the one normalization
  user_id       TEXT NOT NULL,         -- as the server returns it
  created_ms    INTEGER NOT NULL,
  last_seen_ms  INTEGER NOT NULL,
  deleting_ms   INTEGER,
  deletion_phase TEXT CHECK (deletion_phase IN ('pending_server', 'confirmed')),
  deletion_generation TEXT             -- random operation id; only confirmed permits collection (§4.4)
) STRICT;

CREATE TABLE credentials (             -- today's auth.json: the profile's one signed-in session
  slot           INTEGER PRIMARY KEY CHECK (slot = 1),
  account_id     INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  api_url        TEXT NOT NULL,
  session_id     TEXT,
  revision       INTEGER NOT NULL,     -- compare-and-swap on rotation and sign-out
  access_token   BLOB NOT NULL,        -- plaintext bytes in plain mode, like auth.json; encrypted with the DB in password mode
  refresh_token  BLOB NOT NULL,
  updated_ms     INTEGER NOT NULL
) STRICT;

-- ───────────── settings ─────────────
CREATE TABLE app_settings (            -- install scope; one serde struct per key, defaults from Rust
  key         TEXT PRIMARY KEY,        -- 'ui' (AppSettings minus moved fields), 'agent.harness', 'notifications'
  value       TEXT NOT NULL CHECK (json_valid(value)),
  updated_ms  INTEGER NOT NULL
) STRICT;

CREATE TABLE account_settings (        -- account scope
  account_id  INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  key         TEXT NOT NULL,           -- 'agent.defaults', 'agent.integrations', 'acp', 'cache.model_catalog',
  value       TEXT NOT NULL CHECK (json_valid(value)),   -- 'remote.sidebars'
  updated_ms  INTEGER NOT NULL,
  PRIMARY KEY (account_id, key)
) STRICT;

CREATE TABLE dictionary_words (
  word  TEXT PRIMARY KEY
) STRICT, WITHOUT ROWID;

-- ───────────── projects and MCP ─────────────
CREATE TABLE projects (                -- recent roots + removed roots + project names + trust + project MCP defaults
  id            INTEGER PRIMARY KEY,
  account_id    INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  root          TEXT NOT NULL,         -- canonical absolute path on this computer
  display_name  TEXT,                  -- NULL shows the folder name
  position      INTEGER,               -- order in the recent list; NULL = known only from its tasks
  last_used_ms  INTEGER NOT NULL,
  removed_ms    INTEGER,               -- hidden from the switcher; its tasks stay
  trusted       INTEGER CHECK (trusted IN (0, 1)),             -- NULL = not decided
  mcp_defaults  TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(mcp_defaults)),   -- {server_key: enabled}
  UNIQUE (account_id, root)
) STRICT;
CREATE INDEX projects_navigation ON projects (account_id, (position IS NULL), coalesce(position, 0), root, id)
  WHERE removed_ms IS NULL;

CREATE TABLE mcp_servers (
  id          INTEGER PRIMARY KEY,
  account_id  INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  key         TEXT NOT NULL,           -- name_to_key(name), used by task and project switches
  position    INTEGER NOT NULL,
  definition  TEXT NOT NULL CHECK (json_valid(definition)),    -- AgentMcpServer with env/header NAMES, no values
  UNIQUE (account_id, key)
) STRICT;

CREATE TABLE mcp_server_secrets (      -- every environment and header value; never returned by the API (§14.4)
  server_id  INTEGER NOT NULL REFERENCES mcp_servers(id) ON DELETE CASCADE,
  kind       TEXT NOT NULL CHECK (kind IN ('env', 'header')),
  name       TEXT NOT NULL,
  target     BLOB NOT NULL CHECK (length(target) = 32),   -- SHA-256 of the server's canonical command line or URL when saved
  value      BLOB NOT NULL,            -- plaintext in plain mode; encrypted with the DB in password mode
  PRIMARY KEY (server_id, kind, name)
) STRICT, WITHOUT ROWID;

-- ───────────── tasks ─────────────
CREATE TABLE tasks (
  id                 INTEGER PRIMARY KEY,
  uuid               TEXT NOT NULL UNIQUE,     -- Pi session id (UUIDv7) = the external task id
  account_id         INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  project_id         INTEGER NOT NULL REFERENCES projects(id),   -- every task root has a projects row
  kind               TEXT NOT NULL CHECK (kind IN ('desktop', 'acp')),
  state              TEXT NOT NULL CHECK (state IN ('active', 'settled', 'archived')),
  provisional        INTEGER NOT NULL DEFAULT 0 CHECK (provisional IN (0, 1)),  -- was settings.surfaceProvisional
  title              TEXT NOT NULL,
  title_fold         TEXT NOT NULL,            -- title lowercased in Rust (Unicode) whenever it changes; for search
  title_user_set     INTEGER NOT NULL DEFAULT 0,
  pinned_ms          INTEGER,                  -- NULL = not pinned
  web_enabled        INTEGER NOT NULL DEFAULT 1,
  settings           TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(settings)),   -- mcpServers, computerUse, externalAgents
  created_ms         INTEGER NOT NULL,
  updated_ms         INTEGER NOT NULL,
  -- Pi
  pi_header          TEXT CHECK (pi_header IS NULL OR json_valid(pi_header)),  -- SessionHeader; NULL until the first write
  last_seq           INTEGER NOT NULL DEFAULT 0,
  -- caches written in every append transaction
  message_count      INTEGER NOT NULL DEFAULT 0,   -- user + assistant messages
  model              TEXT,                         -- the model lock
  thinking_level     TEXT,                         -- latest ThinkingLevelChange
  context_usage      INTEGER,                      -- valid measured usage under current Pi projected-context rules (§8.6)
  context_estimate   INTEGER NOT NULL DEFAULT 0,   -- projection estimate; measured usage invalidated by later context edits
  context_start_seq  INTEGER,                      -- first kept entry of the latest compaction (bounded load, §8.3)
  context_base       TEXT CHECK (context_base IS NULL OR json_valid(context_base)), -- typed Pi base facts immediately before that boundary
  -- timeline bookkeeping
  timeline_format    INTEGER NOT NULL DEFAULT 0,   -- structure version of this task's rows; mismatch = background rebuild
  rows_epoch         INTEGER NOT NULL DEFAULT 0,   -- the epoch readers use; bumped by a rebuild flip
  last_row_seq       INTEGER NOT NULL DEFAULT 0,   -- in rows_epoch
  rebuild_epoch      INTEGER,                      -- shadow epoch under construction; NULL when none (§8.5)
  rebuild_seq        INTEGER,                      -- last entry seq folded into the shadow epoch
  rebuild_item_index INTEGER NOT NULL DEFAULT 0,    -- next operation within a partially replayed entry
  deleted_ms         INTEGER                       -- set when deletion starts; the collector removes the rest
) STRICT;
-- One sidebar section = one equality on state, keyset on (updated_ms, id). Listed = not provisional, has messages, not deleted.
CREATE INDEX tasks_section ON tasks (account_id, state, updated_ms DESC, id DESC)
  WHERE provisional = 0 AND message_count > 0 AND deleted_ms IS NULL;
CREATE INDEX tasks_project_section ON tasks (account_id, project_id, state, updated_ms DESC, id DESC)
  WHERE provisional = 0 AND message_count > 0 AND deleted_ms IS NULL;
CREATE INDEX tasks_pinned  ON tasks (account_id, updated_ms DESC, id DESC)
  WHERE pinned_ms IS NOT NULL AND provisional = 0 AND message_count > 0 AND deleted_ms IS NULL;
CREATE INDEX tasks_recent  ON tasks (account_id, updated_ms DESC, id DESC)          -- ACP session/list and title search
  WHERE provisional = 0 AND deleted_ms IS NULL;
CREATE INDEX tasks_project ON tasks (project_id, updated_ms DESC, id DESC);          -- FK checks, ACP cwd lists
CREATE INDEX tasks_account ON tasks (account_id);                                    -- FK checks; account_id never changes
CREATE INDEX tasks_deleting ON tasks (deleted_ms) WHERE deleted_ms IS NOT NULL;

-- ───────────── Pi entries: the source of truth (never purged, T1) ─────────────
CREATE TABLE entries (
  id          INTEGER PRIMARY KEY,
  task_id     INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  seq         INTEGER NOT NULL,          -- append order from 1; Pi's leaf is the highest seq
  entry_id    TEXT NOT NULL,             -- Pi's 8-hex id, unique within the task
  kind        TEXT NOT NULL,             -- EntryKind tag, for diagnostics and the projector
  created_ms  INTEGER NOT NULL,
  flags       INTEGER NOT NULL DEFAULT 0,  -- bit 0: internal image encoding (blob references or escaped inline data)
  codec       INTEGER NOT NULL DEFAULT 0,  -- 0 = JSON text, 1 = zstd level 3 (payloads of 8 KiB or more)
  bytes       INTEGER NOT NULL,            -- stored payload size, for byte-capped collection and loads
  payload     BLOB NOT NULL              -- SessionEntry JSON (typed image data internally encoded), maybe compressed; last column
) STRICT;
CREATE UNIQUE INDEX entries_seq   ON entries (task_id, seq);
CREATE UNIQUE INDEX entries_pi_id ON entries (task_id, entry_id);

-- ───────────── materialized UI timeline (derived, rebuildable) ─────────────
CREATE TABLE timeline_rows (
  id          INTEGER PRIMARY KEY,       -- also the FTS rowid if search is added (§16)
  task_id     INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  epoch       INTEGER NOT NULL,          -- readers use tasks.rows_epoch; a rebuild writes rows_epoch + 1 beside it (§8.5)
  row_seq     INTEGER NOT NULL,          -- display order within the epoch
  row_key     TEXT NOT NULL,             -- legacy AgentTimelineItem.id; projector merge lookup only, never wire/UI identity
  entry_seq   INTEGER NOT NULL,          -- entry that created the row
  item_index  INTEGER NOT NULL,          -- stable ordinal within that creation entry, for derived summaries across rebuilds
  update_seq  INTEGER NOT NULL,          -- entry that last changed it (insert, merge or hide)
  call_id     TEXT,                      -- tool call id, so its result merges here
  hidden      INTEGER NOT NULL DEFAULT 0,  -- omitted by a ContextEdit
  is_plan     INTEGER NOT NULL DEFAULT 0,  -- a todo-list row
  codec       INTEGER NOT NULL DEFAULT 0,  -- as entries.codec
  bytes       INTEGER NOT NULL,            -- serialized item size, for byte-bounded pages and collection
  item        BLOB NOT NULL              -- whole AgentTimelineItem JSON in its stored form (§8.5), maybe compressed; last column
) STRICT;
CREATE UNIQUE INDEX timeline_rows_seq   ON timeline_rows (task_id, epoch, row_seq);
CREATE        INDEX timeline_rows_key   ON timeline_rows (task_id, epoch, row_key, row_seq);  -- NOT unique (below)
CREATE        INDEX timeline_rows_call  ON timeline_rows (task_id, epoch, call_id, row_seq) WHERE call_id IS NOT NULL;
CREATE        INDEX timeline_rows_entry ON timeline_rows (task_id, epoch, entry_seq);         -- hide check and hide update
CREATE        INDEX timeline_rows_plan  ON timeline_rows (task_id, epoch, row_seq) WHERE is_plan = 1 AND hidden = 0;

CREATE TABLE tool_summaries (          -- derived; task-owned; origin survives a deterministic timeline rebuild
  task_id    INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  entry_seq  INTEGER NOT NULL,
  item_index INTEGER NOT NULL,
  bytes      INTEGER NOT NULL,          -- serialized size for capped collection
  summary    TEXT NOT NULL,
  PRIMARY KEY (task_id, entry_seq, item_index)
) STRICT, WITHOUT ROWID;

-- ───────────── blobs: uploads and images taken out of entries, stored once ─────────────
CREATE TABLE blobs (
  id          INTEGER PRIMARY KEY,
  sha256      BLOB NOT NULL UNIQUE CHECK (length(sha256) = 32),   -- dedup key; never leaves the host
  mime        TEXT NOT NULL,
  size        INTEGER NOT NULL,
  complete    INTEGER NOT NULL DEFAULT 0,  -- chunks are written in their own transactions
  created_ms  INTEGER NOT NULL
) STRICT;

CREATE TABLE blob_chunks (
  blob_id  INTEGER NOT NULL REFERENCES blobs(id) ON DELETE CASCADE,
  idx      INTEGER NOT NULL,
  data     BLOB NOT NULL,                -- 256 KiB, except the last chunk
  PRIMARY KEY (blob_id, idx)
) STRICT;

CREATE TABLE task_blobs (              -- every task that references a blob (uploads and entry images): GC and authorization
  task_id  INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  blob_id  INTEGER NOT NULL REFERENCES blobs(id),
  PRIMARY KEY (task_id, blob_id)
) STRICT, WITHOUT ROWID;
CREATE INDEX task_blobs_blob ON task_blobs (blob_id);

CREATE TABLE attachments (             -- a user upload, named by maple-attachment://<public_id> in the prompt
  task_id     INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  public_id   TEXT NOT NULL,           -- 32 random hex, as today
  blob_id     INTEGER NOT NULL REFERENCES blobs(id),
  created_ms  INTEGER NOT NULL,
  PRIMARY KEY (task_id, public_id)
) STRICT, WITHOUT ROWID;
CREATE INDEX attachments_blob ON attachments (blob_id);   -- every blob delete checks it
CREATE INDEX attachments_task_blob ON attachments (task_id, blob_id, public_id); -- bounded task/blob collection

-- ───────────── host bookkeeping ─────────────
CREATE TABLE send_receipts (           -- was send-receipts.db; account-bound as today
  id           TEXT PRIMARY KEY,       -- client send id (UUID)
  account_id   INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  fingerprint  BLOB NOT NULL,          -- SHA-256 of the canonical send params
  task_uuid    TEXT NOT NULL,
  result       TEXT CHECK (result IS NULL OR json_valid(result)),          -- set: Finished
  interrupted  INTEGER NOT NULL DEFAULT 0 CHECK (interrupted IN (0, 1)),   -- 1: an earlier host ended before the result (Interrupted)
  created_ms   INTEGER NOT NULL
) STRICT;
CREATE INDEX send_receipts_account ON send_receipts (account_id);
CREATE INDEX send_receipts_age ON send_receipts (created_ms) WHERE result IS NOT NULL;   -- the 7-day prune, finished receipts only

-- ───────────── remote ─────────────
CREATE TABLE remote_identities (       -- was <data>/remote/<scope>.json
  scope       TEXT PRIMARY KEY,        -- length-delimited SHA-256 over ("maple.remote.identity.v1", api, client id, channel, user_key)
  account_id  INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  secret      BLOB NOT NULL CHECK (length(secret) = 32),   -- Iroh SecretKey; plaintext in plain mode, encrypted with the DB in password mode
  enabled     INTEGER NOT NULL DEFAULT 0,
  name        TEXT,                    -- this computer's advertised name; NULL = default_name(), derived from the hostname at run time
  record      TEXT NOT NULL CHECK (json_valid(record)),     -- {version, peers, attempted_windows}: today's Record minus secret, enabled and name;
                                                            -- peers: {endpoint id: {name, grant, controls_this_computer, can_connect, pending_confirmation}}
  updated_ms  INTEGER NOT NULL
) STRICT;
"#;

struct Migration {
    version: i64,
    sql: &'static str,
}

const PROJECT_NAME_REVISION_DDL: &str = "ALTER TABLE projects ADD COLUMN name_revision INTEGER NOT NULL DEFAULT 0 CHECK (name_revision >= 0);";

// Version 3 is an intentional durable fence. Released readers stop here before
// any image source is rewritten; a new reader can resume the bounded retirement
// work with the original tables still present. Version 4 removes those tables
// only after source retirement and byte collection have completed.
const FILE_ATTACHMENTS_DDL: &str = r#"
CREATE TABLE file_attachments (
  public_id TEXT PRIMARY KEY CHECK (length(public_id) = 32),
  account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  operation_id TEXT NOT NULL,
  name TEXT NOT NULL,
  mime TEXT NOT NULL,
  size INTEGER NOT NULL CHECK (size >= 0 AND size <= 31457280),
  sha256 TEXT NOT NULL CHECK (length(sha256) = 64),
  state TEXT NOT NULL CHECK (state IN ('uploading', 'ready', 'cancelled')),
  bound INTEGER NOT NULL DEFAULT 0 CHECK (bound IN (0, 1)),
  created_ms INTEGER NOT NULL,
  updated_ms INTEGER NOT NULL,
  UNIQUE (account_id, operation_id)
) STRICT;
CREATE INDEX file_attachments_task ON file_attachments (task_id);
CREATE INDEX file_attachments_unbound ON file_attachments (updated_ms) WHERE bound = 0;
INSERT INTO store_meta(key,value) VALUES ('attachment_migration_phase',0);
INSERT INTO store_meta(key,value) VALUES ('attachment_migration_cursor',0);
"#;

const RETIRE_BLOBS_DDL: &str = r#"
DROP TABLE attachments;
DROP TABLE task_blobs;
DROP TABLE blob_chunks;
DROP TABLE blobs;
DELETE FROM store_meta WHERE key IN ('attachment_migration_phase','attachment_migration_cursor');
"#;

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        sql: REFERENCE_DDL,
    },
    Migration {
        version: 2,
        sql: PROJECT_NAME_REVISION_DDL,
    },
    Migration {
        version: 3,
        sql: FILE_ATTACHMENTS_DDL,
    },
    Migration {
        version: 4,
        sql: RETIRE_BLOBS_DDL,
    },
];

/// Check a declared schema using only reads before writer tuning or migrations.
/// Only a genuinely empty, unowned file may complete interrupted initialization.
/// `allow_uninitialized` is reserved for that first host-open admission.
pub(super) fn preflight(connection: &Connection, allow_uninitialized: bool) -> StoreResult<i64> {
    let application: i64 = connection.pragma_query_value(None, "application_id", |r| r.get(0))?;
    let version: i64 = connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if application != APPLICATION_ID && !(allow_uninitialized && application == 0 && version == 0) {
        return Err(StoreError::SchemaMismatch);
    }
    // Future versions can change store_meta itself. Do not query an unknown
    // layout or run our append-only migrations against it.
    if version > VERSION {
        return Err(StoreError::NewerVersion {
            found: version,
            supported: VERSION,
        });
    }
    if version < 0 {
        return Err(StoreError::SchemaMismatch);
    }
    if version == 0 {
        let objects: i64 =
            connection.query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get(0))?;
        if objects != 0 {
            return Err(StoreError::SchemaMismatch);
        }
    } else {
        // Validate the declared version before reading metadata or committing a
        // migration. A v1 file with extra triggers must not be migrated first.
        validate_version(connection, version)?;
        let epoch: Option<rusqlite::types::Value> = connection
            .query_row(
                "SELECT value FROM store_meta WHERE key = 'schema_epoch'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let Some(rusqlite::types::Value::Integer(epoch)) = epoch else {
            return Err(StoreError::SchemaMismatch);
        };
        if epoch != SCHEMA_EPOCH {
            return Err(StoreError::SchemaEpoch {
                found: epoch,
                supported: SCHEMA_EPOCH,
            });
        }
    }
    Ok(version)
}

pub(super) fn migrate(connection: &mut Connection, now_ms: i64) -> StoreResult<()> {
    let version = preflight(connection, false)?;
    for migration in MIGRATIONS
        .iter()
        .filter(|migration| migration.version > version)
    {
        if migration.version == 4 {
            super::attachment_migration::retire_legacy_images(connection)?;
        }
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(migration.sql)?;
        if migration.version == 1 {
            let id: [u8; 16] = rand::random();
            transaction.execute(
                "INSERT INTO store_meta(key,value) VALUES ('store_id',?1)",
                [id.as_slice()],
            )?;
            transaction.execute(
                "INSERT INTO store_meta(key,value) VALUES ('created_ms',?1)",
                [now_ms],
            )?;
            transaction.execute(
                "INSERT INTO store_meta(key,value) VALUES ('schema_epoch',?1)",
                [SCHEMA_EPOCH],
            )?;
        }
        transaction.pragma_update(None, "user_version", migration.version)?;
        transaction.commit()?;
    }
    validate(connection)
}

/// Refuse schema drift instead of silently accepting missing constraints,
/// indexes, extra triggers or incompatible development files.
pub(super) fn validate(connection: &Connection) -> StoreResult<()> {
    validate_version(connection, VERSION)
}

fn validate_version(connection: &Connection, version: i64) -> StoreResult<()> {
    let reference = Connection::open_in_memory()?;
    for migration in MIGRATIONS
        .iter()
        .filter(|migration| migration.version <= version)
    {
        reference.execute_batch(migration.sql)?;
    }
    if schema_rows(connection)? != schema_rows(&reference)? {
        return Err(StoreError::SchemaMismatch);
    }
    Ok(())
}

fn schema_rows(connection: &Connection) -> StoreResult<Vec<(String, String, String, String)>> {
    let mut statement = connection.prepare(
        "SELECT type,name,tbl_name,sql FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name",
    )?;
    let rows = statement
        .query_map([], |row| {
            let sql: String = row.get(3)?;
            // SQLite preserves comments in CREATE statements. Compare tokens after
            // stripping line comments so whitespace-only changes are harmless.
            let normalized = sql
                .lines()
                .map(|line| line.split("--").next().unwrap_or(""))
                .collect::<Vec<_>>()
                .join(" ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, normalized))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub(super) fn mark_interrupted_receipts(connection: &mut Connection) -> StoreResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute(
        "UPDATE send_receipts SET interrupted=1 WHERE result IS NULL AND interrupted=0",
        [],
    )?;
    transaction.commit()?;
    Ok(())
}

#[cfg(test)]
pub(super) fn migrate_to_version(
    connection: &mut Connection,
    now_ms: i64,
    version: i64,
) -> StoreResult<()> {
    if !(1..=VERSION).contains(&version) {
        return Err(StoreError::InvalidInput);
    }
    connection.pragma_update(None, "application_id", APPLICATION_ID)?;
    for migration in MIGRATIONS
        .iter()
        .filter(|migration| migration.version <= version)
    {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(migration.sql)?;
        if migration.version == 1 {
            transaction.execute(
                "INSERT INTO store_meta(key,value) VALUES ('store_id',?1)",
                [rand::random::<[u8; 16]>().as_slice()],
            )?;
            transaction.execute(
                "INSERT INTO store_meta(key,value) VALUES ('created_ms',?1)",
                [now_ms],
            )?;
            transaction.execute(
                "INSERT INTO store_meta(key,value) VALUES ('schema_epoch',?1)",
                [SCHEMA_EPOCH],
            )?;
        }
        transaction.pragma_update(None, "user_version", migration.version)?;
        transaction.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn empty() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .pragma_update(None, "application_id", APPLICATION_ID)
            .unwrap();
        connection
            .pragma_update(None, "foreign_keys", true)
            .unwrap();
        connection
    }

    #[test]
    fn migrations_validate_from_empty_with_metadata_only_attachments() {
        let mut connection = empty();
        migrate(&mut connection, 42).unwrap();
        validate(&connection).unwrap();
        let tables: i64 = connection.query_row("SELECT count(*) FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%'", [], |r| r.get(0)).unwrap();
        assert_eq!(tables, 16);
        assert_eq!(
            connection
                .pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
                .unwrap(),
            VERSION
        );
    }

    #[test]
    fn project_revision_migration_preserves_existing_store_v1_labels() {
        let mut connection = empty();
        connection.execute_batch(REFERENCE_DDL).unwrap();
        connection
            .execute(
                "INSERT INTO store_meta(key,value) VALUES ('schema_epoch',?1)",
                [SCHEMA_EPOCH],
            )
            .unwrap();
        connection.execute("INSERT INTO accounts(id,user_key,user_id,created_ms,last_seen_ms) VALUES (1,'a','A',1,1)", []).unwrap();
        connection.execute("INSERT INTO projects(account_id,root,display_name,position,last_used_ms) VALUES (1,'/project','Existing label',0,1)", []).unwrap();
        connection.pragma_update(None, "user_version", 1).unwrap();
        migrate(&mut connection, 2).unwrap();
        let label: (String, i64) = connection
            .query_row(
                "SELECT display_name,name_revision FROM projects",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(label, ("Existing label".into(), 0));
        validate(&connection).unwrap();
    }

    #[test]
    fn opening_again_preserves_identity_and_account_data() {
        let mut connection = empty();
        migrate(&mut connection, 42).unwrap();
        let id: Vec<u8> = connection
            .query_row(
                "SELECT value FROM store_meta WHERE key='store_id'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        connection.execute("INSERT INTO accounts(user_key,user_id,created_ms,last_seen_ms) VALUES ('a','A',1,1)", []).unwrap();
        migrate(&mut connection, 99).unwrap();
        assert_eq!(
            connection
                .query_row::<Vec<u8>, _, _>(
                    "SELECT value FROM store_meta WHERE key='store_id'",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
            id
        );
        assert_eq!(
            connection
                .query_row::<String, _, _>("SELECT user_id FROM accounts", [], |r| r.get(0))
                .unwrap(),
            "A"
        );
    }

    #[test]
    fn newer_versions_and_other_schema_epochs_are_refused_without_changes() {
        let mut connection = empty();
        migrate(&mut connection, 42).unwrap();
        connection
            .pragma_update(None, "user_version", VERSION + 1)
            .unwrap();
        assert!(matches!(
            migrate(&mut connection, 43),
            Err(StoreError::NewerVersion { .. })
        ));
        connection
            .pragma_update(None, "user_version", VERSION)
            .unwrap();
        connection
            .execute("UPDATE store_meta SET value=0 WHERE key='schema_epoch'", [])
            .unwrap();
        assert_eq!(
            migrate(&mut connection, 43),
            Err(StoreError::SchemaEpoch {
                found: 0,
                supported: SCHEMA_EPOCH
            })
        );
        assert_eq!(
            connection
                .query_row::<i64, _, _>(
                    "SELECT value FROM store_meta WHERE key='created_ms'",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
            42
        );
    }

    #[test]
    fn missing_index_extra_trigger_and_wrong_application_are_refused() {
        let mut connection = empty();
        migrate(&mut connection, 1).unwrap();
        connection
            .execute_batch("DROP INDEX entries_pi_id")
            .unwrap();
        assert_eq!(validate(&connection), Err(StoreError::SchemaMismatch));
        let mut connection = empty();
        migrate(&mut connection, 1).unwrap();
        connection
            .execute_batch("CREATE TRIGGER surprise AFTER INSERT ON accounts BEGIN SELECT 1; END")
            .unwrap();
        assert_eq!(validate(&connection), Err(StoreError::SchemaMismatch));
        connection.pragma_update(None, "application_id", 0).unwrap();
        assert_eq!(migrate(&mut connection, 2), Err(StoreError::SchemaMismatch));
    }

    #[test]
    fn failed_migration_is_atomic() {
        let mut connection = empty();
        {
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            assert!(transaction.execute_batch("CREATE TABLE would_exist(id INTEGER); CREATE TABLE would_exist(id INTEGER);").is_err());
        }
        assert_eq!(
            connection
                .query_row::<i64, _, _>(
                    "SELECT count(*) FROM sqlite_schema WHERE name='would_exist'",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
            0
        );
    }

    #[test]
    fn receipt_recovery_reserves_every_unfinished_send() {
        let mut connection = empty();
        migrate(&mut connection, 1).unwrap();
        connection.execute("INSERT INTO accounts(id,user_key,user_id,created_ms,last_seen_ms) VALUES (1,'a','a',1,1)",[]).unwrap();
        connection.execute("INSERT INTO send_receipts(id,account_id,fingerprint,task_uuid,created_ms) VALUES ('send',1,?1,'task',1)",params![vec![0u8;32]]).unwrap();
        mark_interrupted_receipts(&mut connection).unwrap();
        assert_eq!(
            connection
                .query_row::<i64, _, _>(
                    "SELECT interrupted FROM send_receipts WHERE id='send'",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
            1
        );
    }
}
