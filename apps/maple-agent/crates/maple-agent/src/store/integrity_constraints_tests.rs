use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use rusqlite::Connection;

use super::super::{
    CheckIncomplete, IntegrityCancellation, IntegrityKind, IntegrityOutcome, check_connection,
    open_readonly,
};
use super::*;
use crate::store::{Store, StorePaths, keys};

fn fixture(encrypted: bool) -> (tempfile::TempDir, Store) {
    let directory = tempfile::tempdir().unwrap();
    let paths = StorePaths::new(directory.path());
    let store = if encrypted {
        Store::open_keyed(paths, keys::LockedKey::from_bytes([0x63; 32]).unwrap()).unwrap()
    } else {
        Store::open(paths).unwrap()
    };
    (directory, store)
}

// Test-only extraction from our trusted reference schema. Never used on the
// checked database or to generate a production query.
fn expressions(sql: &str) -> Vec<String> {
    let sql = sql
        .lines()
        .map(|line| line.split("--").next().unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    let bytes = sql.as_bytes();
    let mut found = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\'' || bytes[index] == b'"' {
            let quote = bytes[index];
            index += 1;
            loop {
                assert!(index < bytes.len());
                if bytes[index] == quote {
                    index += 1;
                    if bytes.get(index) != Some(&quote) {
                        break;
                    }
                }
                index += 1;
            }
        } else if bytes
            .get(index..index + 5)
            .is_some_and(|word| word.eq_ignore_ascii_case(b"check"))
            && (index == 0
                || !(bytes[index - 1].is_ascii_alphanumeric() || bytes[index - 1] == b'_'))
            && bytes
                .get(index + 5)
                .is_none_or(|byte| !(byte.is_ascii_alphanumeric() || *byte == b'_'))
        {
            index += 5;
            while bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            assert_eq!(bytes[index], b'(');
            index += 1;
            let start = index;
            let mut depth = 1;
            let mut quoted = None;
            while depth > 0 {
                assert!(index < bytes.len());
                if let Some(quote) = quoted {
                    if bytes[index] == quote {
                        if bytes.get(index + 1) == Some(&quote) {
                            index += 1;
                        } else {
                            quoted = None;
                        }
                    }
                } else {
                    match bytes[index] {
                        quote @ (b'\'' | b'"') => quoted = Some(quote),
                        b'(' => depth += 1,
                        b')' => depth -= 1,
                        _ => {}
                    }
                }
                index += 1;
            }
            found.push(
                sql[start..index - 1]
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "),
            );
        } else {
            index += 1;
        }
    }
    found.sort();
    found
}

#[test]
fn catalog_matches_every_check_in_each_accepted_owned_schema() {
    for version in 1..=schema::VERSION {
        let mut reference = Connection::open_in_memory().unwrap();
        schema::migrate_to_version(&mut reference, 1, version).unwrap();
        let mut statement = reference
            .prepare("SELECT name,sql FROM sqlite_schema WHERE type='table'")
            .unwrap();
        let expected = statement
            .query_map([], |row| {
                let table: String = row.get(0)?;
                let sql: String = row.get(1)?;
                Ok((table, expressions(&sql)))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .into_iter()
            .filter(|(_, checks)| !checks.is_empty())
            .collect::<BTreeMap<_, _>>();
        let actual = CONSTRAINTS
            .iter()
            .filter(|table| table.removed.is_none_or(|removed| version < removed))
            .filter(|table| table.checks.iter().any(|(since, _)| *since <= version))
            .map(|table| {
                let mut checks = table
                    .checks
                    .iter()
                    .filter(|(since, _)| *since <= version)
                    .map(|(_, sql)| sql.split_whitespace().collect::<Vec<_>>().join(" "))
                    .collect::<Vec<_>>();
                checks.sort();
                (table.table.to_owned(), checks)
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(actual, expected, "schema version {version}");
    }
}

#[test]
fn readonly_plain_and_keyed_checks_detect_json_and_integer_constraints() {
    for encrypted in [false, true] {
        let (_directory, store) = fixture(encrypted);
        let account = store.upsert_account("constraint-fixture", 1).unwrap();
        {
            let guard = store.writer.lock().unwrap();
            let writer = guard.as_ref().unwrap();
            writer
                .pragma_update(None, "ignore_check_constraints", true)
                .unwrap();
            writer.execute("INSERT INTO app_settings(key,value,updated_ms) VALUES ('invalid-fixture','not-json',1)", []).unwrap();
            writer.execute("INSERT INTO projects(account_id,root,last_used_ms,name_revision) VALUES (?1,'constraint-fixture',1,-1)", [account.0]).unwrap();
            writer
                .pragma_update(None, "ignore_check_constraints", false)
                .unwrap();
        }
        store
            .with_authenticated_store_key(|key| {
                let connection = open_readonly(&store.paths, key)?;
                let report = check_connection(
                    &connection,
                    IntegrityKind::Full,
                    encrypted,
                    Duration::from_secs(5),
                    &IntegrityCancellation::default(),
                    &store.paths,
                );
                assert_eq!(report.outcome, IntegrityOutcome::Damaged, "{report:?}");
                for table in [IntegrityTable::AppSettings, IntegrityTable::Projects] {
                    assert!(
                        report
                            .findings
                            .iter()
                            .any(|finding| finding.kind == FindingKind::Constraint
                                && finding.table == Some(table))
                    );
                }
                assert!(report.receipt.is_none());
                Ok(())
            })
            .unwrap();
    }
}

#[test]
fn readonly_checks_accept_nullable_checks_and_version_one_without_new_column() {
    for version in [1, 2] {
        let directory = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(directory.path()).prepare().unwrap();
        paths.precreate_database().unwrap();
        let mut writer = Connection::open(paths.database()).unwrap();
        writer
            .pragma_update(None, "application_id", schema::APPLICATION_ID)
            .unwrap();
        schema::migrate_to_version(&mut writer, 1, version).unwrap();
        writer.execute("INSERT INTO accounts(id,user_key,user_id,created_ms,last_seen_ms) VALUES (1,'nullable-fixture','nullable-fixture',1,1)",[]).unwrap();
        writer.execute("INSERT INTO projects(account_id,root,last_used_ms,trusted,mcp_defaults) VALUES (1,'nullable-fixture',1,NULL,'{}')", []).unwrap();
        drop(writer);
        let store = Store::unavailable(paths, crate::store::StoreState::Maintenance).unwrap();
        let connection = open_readonly(&store.paths, None).unwrap();
        let report = check_connection(
            &connection,
            IntegrityKind::Full,
            false,
            Duration::from_secs(5),
            &IntegrityCancellation::default(),
            &store.paths,
        );
        assert_eq!(
            report.outcome,
            IntegrityOutcome::Clean,
            "version {version}: {report:?}"
        );
        assert!(report.complete);
    }
}

#[test]
fn denied_constraint_read_is_incomplete_without_findings_or_receipt() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

    let (_directory, store) = fixture(false);
    let connection = open_readonly(&store.paths, None).unwrap();
    let observed = Arc::new(AtomicBool::new(false));
    let denied = Arc::clone(&observed);
    connection
        .authorizer(Some(move |context: AuthContext<'_>| {
            if matches!(
                context.action,
                AuthAction::Read {
                    table_name: "app_settings",
                    ..
                }
            ) {
                denied.store(true, Ordering::Release);
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }))
        .unwrap();
    let report = check_connection(
        &connection,
        IntegrityKind::Full,
        false,
        Duration::from_secs(5),
        &IntegrityCancellation::default(),
        &store.paths,
    );
    assert!(observed.load(Ordering::Acquire));
    assert_eq!(
        report.outcome,
        IntegrityOutcome::Partial {
            reason: CheckIncomplete::CheckerUnavailable
        }
    );
    assert!(!report.complete);
    assert!(report.findings.is_empty());
    assert!(report.receipt.is_none());
}

#[test]
fn unknown_schema_is_incomplete_and_small_checks_observe_cancellation_and_deadline() {
    let (_directory, store) = fixture(false);
    let connection = open_readonly(&store.paths, None).unwrap();
    for cancelled in [false, true] {
        let cancel = IntegrityCancellation::default();
        if cancelled {
            cancel.cancel();
        }
        let report = check_connection(
            &connection,
            IntegrityKind::Full,
            false,
            Duration::from_nanos(1),
            &cancel,
            &store.paths,
        );
        assert_eq!(
            report.outcome,
            IntegrityOutcome::Partial {
                reason: if cancelled {
                    CheckIncomplete::Cancelled
                } else {
                    CheckIncomplete::Budget
                }
            }
        );
        assert!(!report.complete);
        assert!(report.receipt.is_none());
    }
    store
        .writer
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .pragma_update(None, "user_version", schema::VERSION + 1)
        .unwrap();
    let report = check_connection(
        &connection,
        IntegrityKind::Full,
        false,
        Duration::from_secs(5),
        &IntegrityCancellation::default(),
        &store.paths,
    );
    assert_eq!(
        report.outcome,
        IntegrityOutcome::Partial {
            reason: CheckIncomplete::CheckerUnavailable
        }
    );
    assert!(!report.complete);
    assert!(report.receipt.is_none());
}
#[test]
fn readonly_plain_and_keyed_checks_detect_orphan_foreign_keys() {
    for encrypted in [false, true] {
        let (_directory, store) = fixture(encrypted);
        {
            let guard = store.writer.lock().unwrap();
            let writer = guard.as_ref().unwrap();
            writer.pragma_update(None, "foreign_keys", false).unwrap();
            writer.execute(
                "INSERT INTO account_settings(account_id,key,value,updated_ms) VALUES (-1,'orphan-fixture','{}',1)",
                [],
            ).unwrap();
            writer.pragma_update(None, "foreign_keys", true).unwrap();
        }
        store.with_authenticated_store_key(|key| {
            let connection = open_readonly(&store.paths, key)?;
            assert_eq!(connection.query_row::<i64, _, _>(
                "SELECT count(*) FROM account_settings WHERE account_id=-1 AND key='orphan-fixture'", [], |row| row.get(0),
            )?, 1);
            let report = check_connection(
                &connection,
                IntegrityKind::Full,
                encrypted,
                Duration::from_secs(5),
                &IntegrityCancellation::default(),
                &store.paths,
            );
            assert_eq!(report.outcome, IntegrityOutcome::Damaged, "{report:?}");
            assert!(report.findings.iter().any(|finding|
                finding.kind == FindingKind::ForeignKey
            ));
            assert!(report.receipt.is_none());
            Ok(())
        }).unwrap();
    }
}
