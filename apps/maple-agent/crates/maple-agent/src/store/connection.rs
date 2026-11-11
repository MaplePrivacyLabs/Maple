//! Connection roles and their invariant settings. Raw handles stay in store/.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, config::DbConfig, functions::FunctionFlags,
};

use super::{StoreError, StorePaths, StoreResult};

const _: () = assert!(rusqlite::ffi::SQLITE_VERSION_NUMBER >= 3_051_003);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Role {
    Writer,
    Interactive,
    Bulk,
    Checkpointer,
}

/// Keying runs before the first SQL statement. The engine module can install a
/// raw-key initializer here without ever exposing a Connection to app callers.
pub(super) type Initializer<'a> = dyn Fn(&Connection) -> StoreResult<()> + 'a;

pub(super) fn open(
    paths: &StorePaths,
    role: Role,
    created: bool,
    initialize: &Initializer<'_>,
) -> StoreResult<Connection> {
    let connection = connect(paths, initialize)?;
    configure(&connection, role, created)?;
    Ok(connection)
}

/// Admission for host startup precedes persistent writer configuration. Repair
/// opens retain their separately admitted path because derived schema damage may
/// be the specific object they are about to rebuild.
pub(super) fn open_initializing_writer(
    paths: &StorePaths,
    initialize: &Initializer<'_>,
) -> StoreResult<Connection> {
    let connection = connect(paths, initialize)?;
    let version = super::schema::preflight(&connection, true)?;
    configure(&connection, Role::Writer, version == 0)?;
    Ok(connection)
}

fn connect(paths: &StorePaths, initialize: &Initializer<'_>) -> StoreResult<Connection> {
    paths.verify_database()?;
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let connection = Connection::open_with_flags(paths.database(), flags)?;
    // A failed initializer or compatibility probe must not checkpoint an existing
    // WAL merely because this handle was the final connection to close.
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)?;
    initialize(&connection)?;
    Ok(connection)
}

pub(super) fn maple_workspace_path(connection: &Connection) -> StoreResult<Option<PathBuf>> {
    let marker: Option<String> = connection
        .prepare_cached("SELECT value FROM app_settings WHERE key=?1")?
        .query_row([super::schema::NESTED_MAPLE_WORKSPACE_KEY], |row| {
            row.get(0)
        })
        .optional()?;
    let nested = match marker.as_deref() {
        Some("true") => true,
        None => false,
        Some(_) => return Err(StoreError::InvalidData),
    };
    Ok(crate::agent::maple_workspace_path(nested))
}

fn configure(connection: &Connection, role: Role, created: bool) -> StoreResult<()> {
    let objects: i64 =
        connection.query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get(0))?;
    if matches!(role, Role::Interactive | Role::Bulk) {
        connection.pragma_update(None, "query_only", true)?;
    }
    connection.pragma_update(None, "foreign_keys", true)?;
    connection.pragma_update(None, "trusted_schema", false)?;
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    connection.pragma_update(None, "temp_store", "MEMORY")?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.pragma_update(
        None,
        "cache_size",
        if role == Role::Writer {
            -65_536
        } else {
            -32_768
        },
    )?;
    connection.pragma_update(None, "checkpoint_fullfsync", true)?;
    connection.pragma_update(None, "wal_autocheckpoint", 0)?;
    connection.set_prepared_statement_cache_capacity(64);
    // SQLite lower() folds ASCII only. Search uses the same Rust Unicode fold
    // as titles, and the same owner-side label projection as runtime/page data.
    // The query supplies the owning store's workspace path. The function
    // stays pure and can be registered before schema initialization.
    connection.create_scalar_function(
        "maple_project_label_fold",
        3,
        FunctionFlags::SQLITE_UTF8
            | FunctionFlags::SQLITE_DETERMINISTIC
            | FunctionFlags::SQLITE_INNOCUOUS,
        |context| {
            let root: String = context.get(0)?;
            let custom_name: Option<String> = context.get(1)?;
            let workspace: Option<String> = context.get(2)?;
            let is_default = workspace
                .as_deref()
                .is_some_and(|path| Path::new(path) == Path::new(&root));
            Ok(
                crate::agent::owner_project_display_name(&root, custom_name.as_deref(), is_default)
                    .to_lowercase(),
            )
        },
    )?;
    if role == Role::Writer {
        // A crash during the first open can leave a header but no schema or
        // application id. Completing that empty new-store initialization loses
        // no data; a nonempty or foreign application is refused by migrations.
        let unfinished_empty = objects == 0
            && connection.pragma_query_value::<i64, _>(None, "application_id", |r| r.get(0))? == 0
            && connection.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))? == 0;
        if created || unfinished_empty {
            // These must precede WAL and the first table: page_size is otherwise
            // ignored in WAL mode, and auto_vacuum requires a rebuild later.
            connection.pragma_update(None, "page_size", 4096)?;
            connection.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
            connection.pragma_update(None, "application_id", super::schema::APPLICATION_ID)?;
        }
        let mode: String = connection.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(super::StoreError::InvalidData);
        }
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "fullfsync", true)?;
        connection.pragma_update(None, "journal_size_limit", 67_108_864)?;
        connection.pragma_update(None, "secure_delete", true)?;
    }
    Ok(())
}

pub(super) fn checkpoint(connection: &Connection, truncate: bool) -> StoreResult<bool> {
    let sql = if truncate {
        "PRAGMA wal_checkpoint(TRUNCATE)"
    } else {
        "PRAGMA wal_checkpoint(PASSIVE)"
    };
    let (busy, _pages, _done): (i64, i64, i64) =
        connection.query_row(sql, [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
    Ok(busy == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{StoreError, engine, keys, schema};

    fn initialize(connection: &Connection, keyed: bool) -> StoreResult<()> {
        if keyed {
            engine::key_connection(
                connection,
                &keys::LockedKey::from_bytes([0xa1; 32]).unwrap(),
            )
        } else {
            engine::guard_plain(connection)
        }
    }

    fn fixture(
        keyed: bool,
        version: i64,
        change: impl FnOnce(&mut Connection),
    ) -> (tempfile::TempDir, StorePaths) {
        let temp = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(temp.path()).prepare().unwrap();
        paths.precreate_database().unwrap();
        let mut connection = connect(&paths, &|connection| initialize(connection, keyed)).unwrap();
        connection
            .pragma_update(None, "application_id", schema::APPLICATION_ID)
            .unwrap();
        if version == 1 {
            connection.execute_batch(schema::REFERENCE_DDL).unwrap();
            connection
                .execute(
                    "INSERT INTO store_meta(key,value) VALUES ('schema_epoch',?1)",
                    [schema::SCHEMA_EPOCH],
                )
                .unwrap();
            connection.pragma_update(None, "user_version", 1).unwrap();
        } else {
            schema::migrate(&mut connection, 1).unwrap();
        }
        change(&mut connection);
        let mode: String = connection
            .query_row("PRAGMA journal_mode = DELETE", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "delete");
        connection.close().unwrap();
        assert!(!paths.db().join("maple.db-wal").exists());
        assert!(!paths.db().join("maple.db-shm").exists());
        (temp, paths)
    }

    fn refused_without_persistent_changes(paths: &StorePaths, keyed: bool, expected: StoreError) {
        let before = std::fs::read(paths.database()).unwrap();
        let result = open_initializing_writer(paths, &|connection| initialize(connection, keyed));
        assert_eq!(result.map(|_| ()), Err(expected));
        assert_eq!(std::fs::read(paths.database()).unwrap(), before);
        for name in ["maple.db-wal", "maple.db-shm", "maple.db-journal"] {
            assert!(
                !paths.db().join(name).exists(),
                "unexpected sidecar: {name}"
            );
        }
    }

    #[test]
    fn plain_and_keyed_future_foreign_and_incompatible_stores_refuse_before_wal() {
        for keyed in [false, true] {
            let (_temp, paths) = fixture(keyed, schema::VERSION, |connection| {
                connection
                    .pragma_update(None, "user_version", schema::VERSION + 1)
                    .unwrap();
                // A future layout need not retain metadata understood by us.
                connection
                    .execute_batch("DROP TABLE store_meta; CREATE TABLE future_meta(value BLOB)")
                    .unwrap();
            });
            refused_without_persistent_changes(
                &paths,
                keyed,
                StoreError::NewerVersion {
                    found: schema::VERSION + 1,
                    supported: schema::VERSION,
                },
            );

            let (_temp, paths) = fixture(keyed, schema::VERSION, |connection| {
                connection
                    .pragma_update(None, "application_id", 123)
                    .unwrap();
            });
            refused_without_persistent_changes(&paths, keyed, StoreError::SchemaMismatch);

            let (_temp, paths) = fixture(keyed, schema::VERSION, |connection| {
                connection.pragma_update(None, "user_version", -1).unwrap();
            });
            refused_without_persistent_changes(&paths, keyed, StoreError::SchemaMismatch);

            let (_temp, paths) = fixture(keyed, schema::VERSION, |connection| {
                connection
                    .execute(
                        "UPDATE store_meta SET value=?1 WHERE key='schema_epoch'",
                        [schema::SCHEMA_EPOCH + 1],
                    )
                    .unwrap();
            });
            refused_without_persistent_changes(
                &paths,
                keyed,
                StoreError::SchemaEpoch {
                    found: schema::SCHEMA_EPOCH + 1,
                    supported: schema::SCHEMA_EPOCH,
                },
            );

            for sql in [
                "DELETE FROM store_meta WHERE key='schema_epoch'",
                "UPDATE store_meta SET value='unsupported' WHERE key='schema_epoch'",
                "DROP INDEX entries_pi_id",
            ] {
                let (_temp, paths) = fixture(keyed, schema::VERSION, |connection| {
                    connection.execute_batch(sql).unwrap();
                });
                refused_without_persistent_changes(&paths, keyed, StoreError::SchemaMismatch);
            }

            let (_temp, paths) = fixture(keyed, 1, |connection| {
                connection
                    .execute_batch(
                        "CREATE TRIGGER extra AFTER INSERT ON accounts BEGIN SELECT 1; END",
                    )
                    .unwrap();
            });
            refused_without_persistent_changes(&paths, keyed, StoreError::SchemaMismatch);
        }
    }

    #[test]
    fn plain_and_keyed_v1_are_accepted_then_migrated_without_losing_rows() {
        for keyed in [false, true] {
            let (_temp, paths) = fixture(keyed, 1, |connection| {
                connection.execute("INSERT INTO accounts(user_key,user_id,created_ms,last_seen_ms) VALUES ('retained','Retained',1,1)", []).unwrap();
            });
            let mut writer =
                open_initializing_writer(&paths, &|connection| initialize(connection, keyed))
                    .unwrap();
            schema::migrate(&mut writer, 2).unwrap();
            schema::validate(&writer).unwrap();
            let retained: String = writer
                .query_row(
                    "SELECT user_id FROM accounts WHERE user_key='retained'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(retained, "Retained");
            assert_eq!(
                writer
                    .pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
                    .unwrap(),
                schema::VERSION
            );
        }
    }

    #[test]
    fn plain_and_keyed_empty_initialization_and_header_recovery_remain_supported() {
        for keyed in [false, true] {
            for application in [0, schema::APPLICATION_ID] {
                let temp = tempfile::tempdir().unwrap();
                let paths = StorePaths::new(temp.path()).prepare().unwrap();
                paths.precreate_database().unwrap();
                if application != 0 {
                    let connection =
                        connect(&paths, &|connection| initialize(connection, keyed)).unwrap();
                    connection
                        .pragma_update(None, "application_id", application)
                        .unwrap();
                    connection.close().unwrap();
                }
                let mut writer =
                    open_initializing_writer(&paths, &|connection| initialize(connection, keyed))
                        .unwrap();
                schema::migrate(&mut writer, 2).unwrap();
                schema::validate(&writer).unwrap();
            }
        }
    }
}
