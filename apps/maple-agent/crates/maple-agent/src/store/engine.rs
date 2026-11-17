//! The one linked SQLite/SQLCipher engine, with keying before any SQL.
//!
//! Provider metadata is public build information. Raw key bytes and the raw-key
//! text remain in dedicated locked mappings and never enter a SQL statement.

use std::fs::File;
use std::io::Read;
use std::sync::OnceLock;

use chrono::NaiveDate;
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;

use super::keys::{KeyError, LockedKey};
use super::{DamageReason, Store, StoreError, StorePaths, StoreResult, StoreState};

const _: () = assert!(rusqlite::ffi::SQLITE_VERSION_NUMBER >= 3_051_003);

pub const SQLCIPHER_VERSION: &str = "4.20.0 community";
pub const SQLITE_VERSION: &str = "3.54.1";
pub const OPENSSL_VERSION: &str = "4.0.3";

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EngineInfo {
    pub sqlite_version: String,
    pub cipher_version: String,
    pub cipher_provider: String,
    pub cipher_provider_version: String,
}

/// Validate even an unkeyed connection: plain mode uses the same linked engine.
pub(super) fn guard_plain(connection: &Connection) -> StoreResult<()> {
    inspect(connection).map(|_| ())
}

pub(super) fn inspect(connection: &Connection) -> StoreResult<EngineInfo> {
    let provider: Option<String> = connection
        .pragma_query_value(None, "cipher_provider", |row| row.get(0))
        .optional()?;
    if provider.is_some() {
        return inspect_keyed(connection);
    }
    // These two provider pragmas require a codec context, which an ordinary
    // unkeyed SQLite file deliberately has none of. Probe the same linked
    // engine in a disposable empty in-memory connection, never the user's
    // store. The fixed probe key protects no data and is not persisted.
    static PROVIDER: OnceLock<StoreResult<EngineInfo>> = OnceLock::new();
    let provider = PROVIDER.get_or_init(|| {
        let probe = Connection::open_in_memory()?;
        const PUBLIC_PROBE_KEY: &[u8; 67] =
            b"x'0000000000000000000000000000000000000000000000000000000000000000'";
        let code = unsafe {
            rusqlite::ffi::sqlite3_key_v2(
                probe.handle(),
                c"main".as_ptr(),
                PUBLIC_PROBE_KEY.as_ptr().cast(),
                67,
            )
        };
        if code != rusqlite::ffi::SQLITE_OK {
            return Err(StoreError::Database {
                code: code & 0xff,
                extended_code: code,
            });
        }
        inspect_keyed(&probe)
    });
    let mut information = provider.as_ref().map_err(Clone::clone)?.clone();
    information.cipher_version =
        connection.pragma_query_value(None, "cipher_version", |row| row.get(0))?;
    information.sqlite_version =
        connection.query_row("SELECT sqlite_version()", [], |row| row.get(0))?;
    validate(information)
}

fn inspect_keyed(connection: &Connection) -> StoreResult<EngineInfo> {
    let information = EngineInfo {
        cipher_version: connection.pragma_query_value(None, "cipher_version", |row| row.get(0))?,
        cipher_provider: connection
            .pragma_query_value(None, "cipher_provider", |row| row.get(0))?,
        cipher_provider_version: connection.pragma_query_value(
            None,
            "cipher_provider_version",
            |row| row.get(0),
        )?,
        sqlite_version: connection.query_row("SELECT sqlite_version()", [], |row| row.get(0))?,
    };
    validate(information)
}

fn validate(information: EngineInfo) -> StoreResult<EngineInfo> {
    let provider_version = information
        .cipher_provider_version
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>();
    if information.cipher_version != SQLCIPHER_VERSION
        || information.sqlite_version != SQLITE_VERSION
        || information.cipher_provider != "openssl"
        || provider_version != ["OpenSSL", OPENSSL_VERSION]
    {
        return Err(StoreError::InvalidData);
    }
    Ok(information)
}

fn key_buffer_error(error: KeyError) -> StoreError {
    match error {
        KeyError::NotEnoughMemory { .. } => StoreError::Io {
            kind: std::io::ErrorKind::OutOfMemory,
        },
        KeyError::MemoryLock => StoreError::Io {
            kind: std::io::ErrorKind::PermissionDenied,
        },
        KeyError::Io { kind } => StoreError::Io { kind },
        _ => StoreError::InvalidData,
    }
}

pub(super) fn key_connection(connection: &Connection, key: &LockedKey) -> StoreResult<()> {
    let code = key
        .with_sqlcipher_key(|raw| {
            // The connection is exclusively owned during initialization. The
            // library consumes the 67-byte raw-key representation during this
            // call; the dedicated mapping is wiped immediately afterward.
            unsafe {
                rusqlite::ffi::sqlite3_key_v2(
                    connection.handle(),
                    c"main".as_ptr(),
                    raw.as_ptr().cast(),
                    67,
                )
            }
        })
        .map_err(key_buffer_error)?;
    if code != rusqlite::ffi::SQLITE_OK {
        return Err(StoreError::Database {
            code: code & 0xff,
            extended_code: code,
        });
    }
    inspect_keyed(connection).map(|_| ())
}

/// Attach an export destination without copying the raw-key representation to
/// SQLite's ordinary binding heap. The fixed SQL contains no secret. SQLCipher
/// 4.20 accepts the raw-key representation as a BLOB; its ATTACH implementation
/// reads its bytes without string conversion. Keep finalization inside the
/// locked-mapping callback, including every error and unwind path.
pub(super) fn attach_keyed(
    connection: &Connection,
    path: &str,
    key: &LockedKey,
) -> StoreResult<()> {
    use rusqlite::ffi;
    if path.as_bytes().contains(&0) {
        return Err(StoreError::InvalidInput);
    }
    struct Statement(*mut ffi::sqlite3_stmt);
    impl Statement {
        fn finish(mut self) -> i32 {
            let pointer = std::mem::replace(&mut self.0, std::ptr::null_mut());
            unsafe { ffi::sqlite3_finalize(pointer) }
        }
    }
    impl Drop for Statement {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    ffi::sqlite3_finalize(self.0);
                }
            }
        }
    }
    fn checked(code: i32) -> StoreResult<()> {
        if code == ffi::SQLITE_OK {
            Ok(())
        } else {
            Err(StoreError::Database {
                code: code & 0xff,
                extended_code: code,
            })
        }
    }
    key.with_sqlcipher_key(|raw| {
        let mut pointer = std::ptr::null_mut();
        // The caller exclusively owns this connection under its lifecycle gate.
        let code = unsafe {
            ffi::sqlite3_prepare_v2(
                connection.handle(),
                c"ATTACH DATABASE ?1 AS dst KEY ?2".as_ptr(),
                -1,
                &mut pointer,
                std::ptr::null_mut(),
            )
        };
        let statement = Statement(pointer);
        checked(code)?;
        checked(unsafe {
            ffi::sqlite3_bind_text64(
                statement.0,
                1,
                path.as_ptr().cast(),
                path.len() as u64,
                ffi::SQLITE_TRANSIENT(),
                ffi::SQLITE_UTF8 as u8,
            )
        })?;
        // A nonzero, ordinary BLOB avoids both a transient binding copy and any
        // implicit text termination/encoding allocation inside sqlite3_value_*.
        checked(unsafe {
            ffi::sqlite3_bind_blob64(
                statement.0,
                2,
                raw.as_ptr().cast(),
                raw.len() as u64,
                ffi::SQLITE_STATIC(),
            )
        })?;
        let stepped = unsafe { ffi::sqlite3_step(statement.0) };
        if stepped != ffi::SQLITE_DONE {
            return checked(stepped);
        }
        checked(statement.finish())
    })
    .map_err(key_buffer_error)?
}

impl Store {
    /// Open under an already authenticated/unwrapped DEK. Slot validation and
    /// password policy belong to keys; this method never reads a password.
    pub fn open_keyed(paths: StorePaths, key: LockedKey) -> StoreResult<Self> {
        let store = Self::unavailable(paths, StoreState::Maintenance)?;
        store.initialize_keyed(key)?;
        Ok(store)
    }

    /// Install keyed connections on the stable host-owned handle. The core's
    /// lifecycle gate owns key installation and connection publication together.
    pub fn initialize_keyed(&self, key: LockedKey) -> StoreResult<()> {
        self.initialize_key_material(key, key_connection)?;
        let result = (|| {
            let mut header = [0_u8; 16];
            self.paths.verify_database()?;
            File::open(self.paths.database())?.read_exact(&mut header)?;
            if &header == b"SQLite format 3\0" {
                return Err(StoreError::Damaged {
                    reason: DamageReason::Database,
                });
            }
            Ok(())
        })();
        if let Err(error) = &result {
            let _ = self.close();
            self.record_initialization_error(error);
        }
        result
    }

    pub fn engine_info(&self) -> StoreResult<EngineInfo> {
        self.read(|transaction| inspect(transaction))
    }
}

/// A release-validation policy, not a clock-dependent block on users' history.
pub fn provider_supported_on(date: NaiveDate) -> bool {
    date <= NaiveDate::from_ymd_opt(2027, 5, 14).expect("valid OpenSSL support deadline")
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_KEY: [u8; 32] = [0x63; 32];

    #[test]
    fn attached_export_uses_the_locked_raw_key_and_binds_the_path() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("destination's.db");
        let key = LockedKey::from_bytes(TEST_KEY).unwrap();
        let source = Connection::open_in_memory().unwrap();
        guard_plain(&source).unwrap();
        source
            .execute_batch(
                "CREATE TABLE secret(value TEXT); INSERT INTO secret VALUES ('fixture');",
            )
            .unwrap();
        attach_keyed(&source, path.to_str().unwrap(), &key).unwrap();
        source
            .query_row("SELECT sqlcipher_export('dst')", [], |_| Ok(()))
            .unwrap();
        source.execute_batch("DETACH DATABASE dst").unwrap();
        let output = Connection::open(&path).unwrap();
        key_connection(&output, &key).unwrap();
        let value: String = output
            .query_row("SELECT value FROM secret", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, "fixture");
        drop(output);
        let wrong = Connection::open(&path).unwrap();
        let guarded = key_connection(&wrong, &LockedKey::from_bytes([0x64; 32]).unwrap());
        assert!(
            guarded.is_err()
                || wrong
                    .query_row("SELECT count(*) FROM sqlite_schema", [], |row| row
                        .get::<_, i64>(0))
                    .is_err()
        );
    }

    #[test]
    fn link_guard_reports_expected_versions_for_plain_and_keyed_stores() {
        let plain_directory = tempfile::tempdir().unwrap();
        let plain = Store::open(StorePaths::new(plain_directory.path())).unwrap();
        let keyed_directory = tempfile::tempdir().unwrap();
        let keyed = Store::open_keyed(
            StorePaths::new(keyed_directory.path()),
            LockedKey::from_bytes(TEST_KEY).unwrap(),
        )
        .unwrap();
        for information in [plain.engine_info().unwrap(), keyed.engine_info().unwrap()] {
            assert_eq!(information.sqlite_version, SQLITE_VERSION);
            assert_eq!(information.cipher_version, SQLCIPHER_VERSION);
            assert_eq!(information.cipher_provider, "openssl");
            assert!(
                information
                    .cipher_provider_version
                    .starts_with("OpenSSL 4.0.3 ")
            );
        }
    }

    #[test]
    fn keyed_file_has_no_sqlite_header_or_plaintext_and_survives_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(directory.path());
        let store =
            Store::open_keyed(paths.clone(), LockedKey::from_bytes(TEST_KEY).unwrap()).unwrap();
        let marker = "maple-fixture-private-account-that-must-remain-ciphertext";
        store.upsert_account(marker, 1).unwrap();
        for path in [paths.database(), paths.db().join("maple.db-wal")] {
            if path.exists() {
                let bytes = std::fs::read(path).unwrap();
                assert!(
                    !bytes
                        .windows(marker.len())
                        .any(|window| window == marker.as_bytes())
                );
                assert!(!bytes.starts_with(b"SQLite format 3\0"));
            }
        }
        store.close().unwrap();
        let reopened = Store::open_keyed(paths, LockedKey::from_bytes(TEST_KEY).unwrap()).unwrap();
        assert!(reopened.account_id(marker).unwrap().is_some());
    }

    #[test]
    fn wrong_key_fails_with_notadb_without_revealing_key_text() {
        let directory = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(directory.path());
        Store::open_keyed(paths.clone(), LockedKey::from_bytes(TEST_KEY).unwrap())
            .unwrap()
            .close()
            .unwrap();
        let connection = Connection::open(paths.database()).unwrap();
        let wrong = LockedKey::from_bytes([0x27; 32]).unwrap();
        let code = wrong
            .with_sqlcipher_key(|raw| unsafe {
                rusqlite::ffi::sqlite3_key_v2(
                    connection.handle(),
                    c"main".as_ptr(),
                    raw.as_ptr().cast(),
                    67,
                )
            })
            .unwrap();
        assert_eq!(code, rusqlite::ffi::SQLITE_OK);
        let error = connection
            .query_row("SELECT count(*) FROM sqlite_schema", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap_err();
        assert!(matches!(
            &error,
            rusqlite::Error::SqliteFailure(failure, _)
                if failure.code == rusqlite::ErrorCode::NotADatabase
        ));
        let error = StoreError::from(error);
        assert!(!format!("{error:?}").contains(&"27".repeat(32)));
    }

    #[test]
    fn openssl_line_is_within_upstream_support() {
        assert!(provider_supported_on(chrono::Utc::now().date_naive()));
        assert!(provider_supported_on(
            NaiveDate::from_ymd_opt(2027, 5, 14).unwrap()
        ));
        assert!(!provider_supported_on(
            NaiveDate::from_ymd_opt(2027, 5, 15).unwrap()
        ));
    }
}
