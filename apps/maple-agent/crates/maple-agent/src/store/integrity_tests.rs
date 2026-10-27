use std::io::Cursor;
#[cfg(unix)]
use std::sync::Arc;

use super::*;
use crate::store::schema;

fn fixture() -> (tempfile::TempDir, Store) {
    let directory = tempfile::tempdir().unwrap();
    let store =
        Store::unavailable(StorePaths::new(directory.path()), StoreState::Maintenance).unwrap();
    store.initialize_recovered().unwrap();
    store.upsert_account("PRIVATE-ACCOUNT-CANARY", 1).unwrap();
    (directory, store)
}

#[test]
fn quick_is_clean_and_full_schedule_requires_explicit_receipt() {
    let (_directory, store) = fixture();
    let cancel = IntegrityCancellation::default();
    // Allow CI scheduling latency while checking clean/receipt semantics.
    // Deadline and cancellation behavior have separate negative fixtures.
    let report = store
        .check_integrity(IntegrityKind::Quick, Duration::from_secs(10), &cancel)
        .unwrap();
    assert_eq!(report.outcome, IntegrityOutcome::Clean);
    assert!(report.complete);
    assert!(report.receipt.is_none());
    assert!(!report.recorded_last_integrity);
    assert!(store.integrity_schedule(10_000, true).unwrap().full);
    let seed_backoff = || {
        store.write(Durability::Durable, |tx| {
            tx.execute("INSERT INTO store_meta(key,value) VALUES ('integrity_incomplete_attempts',1),('integrity_retry_after_ms',20000)", [])?;
            Ok(())
        }).unwrap();
    };
    let backoff_rows = || {
        store.read(|connection| Ok(connection.query_row(
            "SELECT count(*) FROM store_meta WHERE key IN ('integrity_incomplete_attempts','integrity_retry_after_ms')",
            [], |row| row.get::<_, i64>(0),
        )?)).unwrap()
    };
    seed_backoff();
    let receipt = IntegrityReceipt {
        store_id: store.store_id().unwrap(),
        checked_ms: 10_000,
    };
    assert!(store.record_integrity_clean(&receipt, &cancel).unwrap());
    assert_eq!(backoff_rows(), 0);
    assert_eq!(
        store.integrity_schedule(10_001, false).unwrap(),
        IntegrityDue {
            quick: false,
            full: false,
            last_full_ms: Some(10_000)
        }
    );
    assert!(
        store
            .integrity_schedule(10_000 + WEEK_MS, false)
            .unwrap()
            .full
    );
    let other = IntegrityReceipt {
        store_id: [99; 16],
        checked_ms: 20_000,
    };
    seed_backoff();
    assert!(!store.record_integrity_clean(&other, &cancel).unwrap());
    assert_eq!(backoff_rows(), 2);
    cancel.cancel();
    assert!(!store.record_integrity_clean(&receipt, &cancel).unwrap());
    assert_eq!(backoff_rows(), 2);
}

#[test]
fn cancelled_check_is_partial_and_does_not_damage_store() {
    let (_directory, store) = fixture();
    let cancel = IntegrityCancellation::default();
    cancel.cancel();
    let report = store
        .check_integrity(IntegrityKind::Quick, Duration::from_millis(100), &cancel)
        .unwrap();
    assert_eq!(
        report.outcome,
        IntegrityOutcome::Partial {
            reason: CheckIncomplete::Cancelled
        }
    );
    assert_eq!(store.state(), StoreState::Open);
    assert!(store.upsert_account("still-writable", 2).is_ok());
}

#[test]
fn only_corruption_errors_are_database_damage() {
    use rusqlite::ffi;

    for code in [
        ffi::SQLITE_CORRUPT,
        ffi::SQLITE_CORRUPT_INDEX,
        ffi::SQLITE_NOTADB,
    ] {
        assert!(is_corruption_error(rusqlite::Error::SqliteFailure(
            ffi::Error::new(code),
            Some("private SQLite diagnostic".into()),
        )));
    }
    for code in [
        ffi::SQLITE_BUSY,
        ffi::SQLITE_LOCKED,
        ffi::SQLITE_IOERR,
        ffi::SQLITE_IOERR_LOCK,
        ffi::SQLITE_CANTOPEN,
        ffi::SQLITE_CANTOPEN_SYMLINK,
        ffi::SQLITE_READONLY,
        ffi::SQLITE_AUTH,
        ffi::SQLITE_FULL,
        ffi::SQLITE_NOMEM,
        ffi::SQLITE_INTERRUPT,
        ffi::SQLITE_ERROR,
    ] {
        assert!(!is_corruption_error(rusqlite::Error::SqliteFailure(
            ffi::Error::new(code),
            Some("private SQLite diagnostic".into()),
        )));
    }
}

fn deny_check(connection: &Connection, denied: &'static str) {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

    connection
        .authorizer(Some(move |context: AuthContext<'_>| match context.action {
            AuthAction::Pragma { pragma_name, .. } if pragma_name.eq_ignore_ascii_case(denied) => {
                Authorization::Deny
            }
            _ => Authorization::Allow,
        }))
        .unwrap();
}

#[test]
fn unavailable_quick_check_does_not_quarantine_or_mint_a_receipt() {
    use rusqlite::hooks::{AuthContext, Authorization};

    let (_directory, store) = fixture();
    let generation = store.store_id().unwrap();
    {
        let guard = store.bulk.lock().unwrap();
        deny_check(guard.as_ref().unwrap(), "quick_check");
    }
    let report = store
        .check_integrity(
            IntegrityKind::Quick,
            Duration::from_secs(2),
            &IntegrityCancellation::default(),
        )
        .unwrap();
    assert_eq!(
        report.outcome,
        IntegrityOutcome::Partial {
            reason: CheckIncomplete::CheckerUnavailable,
        }
    );
    assert!(!report.complete);
    assert!(report.findings.is_empty());
    assert!(report.receipt.is_none());
    assert!(!report.recorded_last_integrity);
    assert_eq!(store.state(), StoreState::Open);
    assert_eq!(store.store_id().unwrap(), generation);
    assert!(!store.paths.setup().exists());
    store
        .bulk
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .unwrap();
    assert!(
        store
            .upsert_account("still-writable-after-unavailable-check", 2)
            .is_ok()
    );
}

#[test]
fn operational_errors_at_each_full_check_stage_are_incomplete() {
    for denied in [
        "integrity_check",
        "cipher_integrity_check",
        "foreign_key_check",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let encrypted = denied == "cipher_integrity_check";
        let paths = StorePaths::new(directory.path());
        let store = if encrypted {
            Store::open_keyed(paths, keys::LockedKey::from_bytes([0x62; 32]).unwrap()).unwrap()
        } else {
            Store::open(paths).unwrap()
        };
        store
            .with_authenticated_store_key(|key| {
                let connection = open_readonly(&store.paths, key)?;
                deny_check(&connection, denied);
                let report = check_connection(
                    &connection,
                    IntegrityKind::Full,
                    encrypted,
                    Duration::from_secs(2),
                    &IntegrityCancellation::default(),
                    &store.paths,
                );
                assert_eq!(
                    report.outcome,
                    IntegrityOutcome::Partial {
                        reason: CheckIncomplete::CheckerUnavailable,
                    },
                    "{denied}: {report:?}"
                );
                assert!(!report.complete);
                assert!(report.findings.is_empty());
                assert!(report.receipt.is_none());
                assert!(!report.recorded_last_integrity);
                Ok(())
            })
            .unwrap();
    }
}

#[test]
fn genuine_findings_survive_a_later_operational_error() {
    let (_directory, store) = fixture();
    let connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch(
            "CREATE TABLE constraint_fixture(value INTEGER NOT NULL CHECK(value > 0));
         PRAGMA ignore_check_constraints=ON;
         INSERT INTO constraint_fixture(value) VALUES (-1);
         PRAGMA ignore_check_constraints=OFF;",
        )
        .unwrap();
    assert_eq!(
        connection
            .pragma_query_value::<i64, _>(None, "ignore_check_constraints", |row| row.get(0))
            .unwrap(),
        0
    );
    let mut initial_findings = Vec::new();
    collect_check(
        &connection,
        "PRAGMA integrity_check(100)",
        false,
        &mut initial_findings,
    )
    .unwrap();
    assert!(
        initial_findings
            .iter()
            .any(|finding| finding.kind == FindingKind::Constraint),
        "{initial_findings:?}"
    );
    deny_check(&connection, "foreign_key_check");
    let report = check_connection(
        &connection,
        IntegrityKind::Full,
        false,
        Duration::from_secs(2),
        &IntegrityCancellation::default(),
        &store.paths,
    );
    assert_eq!(report.outcome, IntegrityOutcome::Damaged, "{report:?}");
    assert!(!report.complete);
    assert!(
        report
            .findings
            .iter()
            .any(|finding| finding.kind == FindingKind::Constraint)
    );
    assert!(report.receipt.is_none());
}

#[test]
fn report_never_contains_raw_sqlite_messages_or_user_content() {
    let (_directory, store) = fixture();
    let raw = "row PRIVATE-ACCOUNT-CANARY missing from index tasks_recent on page 42";
    let finding = classify(raw, false);
    assert_eq!(finding.kind, FindingKind::IndexMissing);
    assert_eq!(finding.table, Some(IntegrityTable::Tasks));
    assert_eq!(finding.page, Some(42));
    let mut report = store
        .check_integrity(
            IntegrityKind::Quick,
            Duration::from_secs(1),
            &IntegrityCancellation::default(),
        )
        .unwrap();
    report.findings.push(finding);
    let json = serde_json::to_string(&report).unwrap();
    assert!(!json.contains("PRIVATE-ACCOUNT-CANARY"));
    assert!(!json.contains("tasks_recent"));
    assert!(!json.contains(&store.paths.root().display().to_string()));
}

#[test]
fn checker_reads_existing_store_without_modification_and_refuses_missing_path() {
    let (directory, store) = fixture();
    store.close().unwrap();
    let before = fs::read(store.paths.database()).unwrap();
    let mut request = Vec::new();
    write_request(
        &mut request,
        IntegrityKind::Full,
        Duration::from_secs(2),
        None,
    )
    .unwrap();
    let mut output = Vec::new();
    run_checker(Cursor::new(request), &mut output, store.paths.root()).unwrap();
    let report = decode_report(&output, IntegrityKind::Full).unwrap();
    assert_eq!(report.outcome, IntegrityOutcome::Clean);
    assert!(report.complete);
    assert!(report.receipt.is_none());
    assert_eq!(before, fs::read(store.paths.database()).unwrap());
    let missing = directory.path().join("missing/store-v1");
    assert!(existing_paths(&missing).is_err());
    assert!(!missing.exists());
}

#[test]
fn checker_rejects_short_and_trailing_key_input_without_a_report() {
    let (_directory, store) = fixture();
    let key = keys::LockedKey::from_bytes([0x5a; 32]).unwrap();
    let mut request = Vec::new();
    write_request(
        &mut request,
        IntegrityKind::Full,
        Duration::from_secs(2),
        Some(&key),
    )
    .unwrap();
    assert_eq!(request.len(), 64 + 32);
    for key_length in 0..32 {
        let mut output = Vec::new();
        assert!(matches!(
            run_checker(
                Cursor::new(&request[..64 + key_length]),
                &mut output,
                store.paths.root()
            ),
            Err(StoreError::Io {
                kind: std::io::ErrorKind::UnexpectedEof
            })
        ));
        assert!(output.is_empty());
    }
    request.push(0x01);
    let mut output = Vec::new();
    assert_eq!(
        run_checker(Cursor::new(request), &mut output, store.paths.root()),
        Err(StoreError::InvalidInput)
    );
    assert!(output.is_empty());
}

#[test]
fn checker_rejects_invalid_parent_before_reading_key() {
    let (_directory, store) = fixture();
    let mut request = Vec::new();
    write_request(
        &mut request,
        IntegrityKind::Full,
        Duration::from_secs(2),
        None,
    )
    .unwrap();
    request[8] = 1;
    request[18..22].fill(0);
    let mut output = Vec::new();
    // Invalid parent admission must fail before the absent key is read.
    assert!(matches!(
        run_checker(Cursor::new(request), &mut output, store.paths.root()),
        Err(StoreError::InvalidInput)
    ));
    assert!(output.is_empty());
}

#[cfg(unix)]
#[test]
fn checker_rejects_symlink_and_untrusted_response_shapes() {
    let (_directory, store) = fixture();
    let other = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(store.paths.root(), other.path().join("store-v1")).unwrap();
    assert!(existing_paths(&other.path().join("store-v1")).is_err());
    assert!(
        decode_report(
            b"MPLINTREPORT1 {\"private\":\"content\"}\n",
            IntegrityKind::Full
        )
        .is_none()
    );
    let mut report =
        IntegrityReport::partial(IntegrityKind::Full, CheckIncomplete::Budget, Instant::now());
    report.outcome = IntegrityOutcome::Clean;
    let bytes = format!(
        "{REPORT_PREFIX}{}\n",
        serde_json::to_string(&report).unwrap()
    );
    assert!(decode_report(bytes.as_bytes(), IntegrityKind::Full).is_none());
}

#[cfg(unix)]
#[test]
fn deadline_and_cancellation_terminate_and_reap_checker() {
    let (_directory, store) = fixture();
    let command = || {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exec sleep 30"]);
        command
    };
    let start = Instant::now();
    let report = store
        .run_integrity_checker(
            command(),
            IntegrityKind::Full,
            Duration::from_millis(40),
            &IntegrityCancellation::default(),
        )
        .unwrap();
    assert_eq!(
        report.outcome,
        IntegrityOutcome::Partial {
            reason: CheckIncomplete::Budget
        }
    );
    assert!(start.elapsed() < Duration::from_secs(3));
    let cancel = IntegrityCancellation::default();
    let trigger = cancel.clone();
    let thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(30));
        trigger.cancel();
    });
    let report = store
        .run_integrity_checker(
            command(),
            IntegrityKind::Full,
            Duration::from_secs(10),
            &cancel,
        )
        .unwrap();
    thread.join().unwrap();
    assert_eq!(
        report.outcome,
        IntegrityOutcome::Partial {
            reason: CheckIncomplete::Cancelled
        }
    );
}

#[cfg(unix)]
#[test]
fn maintenance_transition_terminates_checker() {
    let (_directory, store) = fixture();
    let store = Arc::new(store);
    let close = Arc::clone(&store);
    let thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(30));
        close.close().unwrap();
    });
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "exec sleep 30"]);
    let report = store
        .run_integrity_checker(
            command,
            IntegrityKind::Full,
            Duration::from_secs(10),
            &IntegrityCancellation::default(),
        )
        .unwrap();
    thread.join().unwrap();
    assert_eq!(
        report.outcome,
        IntegrityOutcome::Partial {
            reason: CheckIncomplete::Cancelled
        }
    );
}

#[test]
#[ignore = "isolated checker subprocess fixture"]
fn checker_child() {
    let arguments = std::env::args_os().collect::<Vec<_>>();
    let position = arguments
        .iter()
        .position(|argument| argument == "--skip")
        .unwrap();
    let root = std::path::PathBuf::from(&arguments[position + 1]);
    let mut output = std::io::stdout().lock();
    output.write_all(b"\n").unwrap();
    run_checker(checker_stdin().unwrap(), &mut output, &root).unwrap();
}

#[test]
#[ignore = "isolated watchdog subprocess fixture"]
fn watchdog_child() {
    let arguments = std::env::args().collect::<Vec<_>>();
    let parent = arguments
        .iter()
        .position(|argument| argument == "--skip")
        .map(|position| CheckerParent {
            id: arguments[position + 1].parse::<u32>().unwrap(),
            #[cfg(windows)]
            created: arguments[position + 2].parse::<u64>().unwrap(),
            #[cfg(not(windows))]
            created: 0,
        });
    let _watchdog = CheckerWatchdog::start(
        parent.unwrap_or_else(|| CheckerParent::current().unwrap()),
        if parent.is_some() {
            Duration::from_secs(60)
        } else {
            Duration::from_millis(50)
        },
    )
    .unwrap();
    if parent.is_some() {
        let current = CheckerParent::current().unwrap();
        println!("CHECKER_WATCHDOG_READY {} {}", current.id, current.created);
        std::io::stdout().flush().unwrap();
    }
    std::thread::sleep(Duration::from_secs(3));
    panic!("watchdog did not terminate blocked native work");
}

#[cfg(unix)]
#[test]
#[ignore = "isolated abrupt parent death fixture"]
#[allow(
    clippy::zombie_processes,
    reason = "The fixture must SIGKILL itself before waiting to test real parent death; the outer test waits for the child's inherited stdout to close."
)]
fn watchdog_parent() {
    Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "store::integrity::tests::watchdog_child",
            "--ignored",
            "--nocapture",
            "--skip",
            &std::process::id().to_string(),
        ])
        .spawn()
        .unwrap();
    unsafe {
        libc::kill(libc::getpid(), libc::SIGKILL);
    }
    unreachable!();
}

#[cfg(unix)]
#[test]
fn checker_exits_promptly_after_parent_sigkill() {
    let start = Instant::now();
    // output() waits for the descendant's inherited stdout pipe to close.
    // Parent death alone cannot complete this wait while the child survives.
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "store::integrity::tests::watchdog_parent",
            "--ignored",
            "--nocapture",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(start.elapsed() < Duration::from_secs(1));
}

#[cfg(windows)]
#[test]
fn checker_parent_creation_time_rejects_reused_pid_before_reading_key() {
    let (_directory, store) = fixture();
    let parent = CheckerParent::current().unwrap();
    assert!(
        windows_parent::Process::open(parent)
            .unwrap()
            .is_alive()
            .unwrap()
    );
    let mut request = Vec::new();
    write_request(
        &mut request,
        IntegrityKind::Full,
        Duration::from_secs(2),
        None,
    )
    .unwrap();
    request[8] = 1;
    request[56..64].copy_from_slice(&(parent.created ^ 1).to_le_bytes());
    let mut output = Vec::new();
    // There are deliberately no key bytes. Reading them would return Io;
    // the wrong process creation time must fail admission with InvalidInput.
    assert!(matches!(
        run_checker(Cursor::new(request), &mut output, store.paths.root()),
        Err(StoreError::InvalidInput)
    ));
    assert!(output.is_empty());
}

#[cfg(windows)]
#[test]
#[ignore = "isolated TerminateProcess parent fixture"]
fn windows_watchdog_parent() {
    let parent = CheckerParent::current().unwrap();
    let _child = ChildGuard(Some(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "store::integrity::tests::watchdog_child",
                "--ignored",
                "--nocapture",
                "--skip",
                &parent.id.to_string(),
                &parent.created.to_string(),
            ])
            .spawn()
            .unwrap(),
    ));
    // The outer test terminates this exact process only after the child says
    // its process handle and independent watchdog have both been installed.
    std::thread::sleep(Duration::from_secs(5));
    panic!("outer test did not terminate the parent fixture");
}

#[cfg(windows)]
#[test]
fn checker_exits_promptly_after_parent_terminate_process() {
    use std::io::BufRead;

    let mut parent = ChildGuard(Some(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "store::integrity::tests::windows_watchdog_parent",
                "--ignored",
                "--nocapture",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    ));
    let mut output = std::io::BufReader::new(parent.0.as_mut().unwrap().stdout.take().unwrap());
    let mut line = String::new();
    let checker = loop {
        assert!(output.read_line(&mut line).unwrap() > 0);
        if let Some((_, identity)) = line.split_once("CHECKER_WATCHDOG_READY ") {
            let mut fields = identity.split_whitespace();
            break windows_parent::Process::open(CheckerParent {
                id: fields.next().unwrap().parse().unwrap(),
                created: fields.next().unwrap().parse().unwrap(),
            })
            .unwrap();
        }
        line.clear();
    };
    assert!(checker.is_alive().unwrap());
    let start = Instant::now();
    // std::process::Child::kill uses TerminateProcess on Windows. The child
    // inherited this stdout pipe, so EOF also proves that it has terminated.
    parent.terminate();
    output.read_to_end(&mut Vec::new()).unwrap();
    while checker.is_alive().unwrap() && start.elapsed() < Duration::from_secs(1) {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(!checker.is_alive().unwrap());
    assert!(start.elapsed() < Duration::from_secs(1));
}

#[test]
fn checker_self_deadline_bounds_an_uninterruptible_native_call() {
    let start = Instant::now();
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "store::integrity::tests::watchdog_child",
            "--ignored",
            "--nocapture",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(124));
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[test]
fn readonly_checks_see_committed_wal_in_plain_and_keyed_stores() {
    for encrypted in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(directory.path());
        let key = encrypted.then(|| keys::LockedKey::from_bytes([0x51; 32]).unwrap());
        let store = match key {
            Some(key) => Store::open_keyed(paths.clone(), key).unwrap(),
            None => Store::open(paths.clone()).unwrap(),
        };
        let paths = store.paths.clone();
        store.upsert_account("readonly-wal-fixture", 1).unwrap();
        assert!(fs::metadata(paths.db().join("maple.db-wal")).unwrap().len() > 0);
        store
            .with_authenticated_store_key(|key| {
                let connection = open_readonly(&paths, key)?;
                let accounts: i64 = connection.query_row(
                    "SELECT count(*) FROM accounts WHERE user_key='readonly-wal-fixture'",
                    [],
                    |row| row.get(0),
                )?;
                assert_eq!(accounts, 1);
                schema::validate(&connection)?;
                let mut findings = Vec::new();
                collect_check(
                    &connection,
                    "PRAGMA integrity_check(100)",
                    false,
                    &mut findings,
                )
                .unwrap();
                assert!(findings.is_empty(), "{findings:?}");
                let report = check_connection(
                    &connection,
                    IntegrityKind::Full,
                    encrypted,
                    Duration::from_secs(10),
                    &IntegrityCancellation::default(),
                    &paths,
                );
                assert_eq!(report.outcome, IntegrityOutcome::Clean, "{report:?}");
                assert!(report.complete);
                assert_eq!(
                    report.diagnostics.page_size,
                    Some(4096),
                    "encrypted={encrypted}"
                );
                assert!(matches!(
                    connection.execute("DELETE FROM accounts", []),
                    Err(rusqlite::Error::SqliteFailure(error, _))
                        if error.code == rusqlite::ErrorCode::ReadOnly
                ));
                Ok(())
            })
            .unwrap();
    }
}

#[test]
fn real_checker_child_returns_clean_without_serializing_receipt() {
    let (_directory, store) = fixture();
    let report = store
        .run_integrity_checker(
            checker_command(&store),
            IntegrityKind::Full,
            Duration::from_secs(10),
            &IntegrityCancellation::default(),
        )
        .unwrap();
    assert_eq!(report.outcome, IntegrityOutcome::Clean, "{report:?}");
    assert!(report.complete);
    assert!(report.receipt.is_none());
}

fn checker_command(store: &Store) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args([
        "--exact",
        "store::integrity::tests::checker_child",
        "--ignored",
        "--nocapture",
        "--skip",
    ]);
    command.arg(store.paths.root());
    command
}

#[test]
fn real_cipher_checker_uses_only_stdin_key_and_no_plain_fallback() {
    let directory = tempfile::tempdir().unwrap();
    let key = keys::LockedKey::from_bytes([81; 32]).unwrap();
    let store = Store::open_keyed(StorePaths::new(directory.path()), key).unwrap();
    store.upsert_account("ENCRYPTED-ACCOUNT-CANARY", 1).unwrap();
    let report = store
        .run_integrity_checker(
            checker_command(&store),
            IntegrityKind::Full,
            Duration::from_secs(10),
            &IntegrityCancellation::default(),
        )
        .unwrap();
    assert_eq!(report.outcome, IntegrityOutcome::Clean, "{report:?}");
    let mut no_key = Vec::new();
    write_request(
        &mut no_key,
        IntegrityKind::Full,
        Duration::from_secs(2),
        None,
    )
    .unwrap();
    let mut output = Vec::new();
    run_checker(Cursor::new(no_key), &mut output, store.paths.root()).unwrap();
    let rejected = decode_report(&output, IntegrityKind::Full).unwrap();
    assert_eq!(rejected.outcome, IntegrityOutcome::Damaged);
    assert!(
        !String::from_utf8(output)
            .unwrap()
            .contains("ENCRYPTED-ACCOUNT-CANARY")
    );
}

#[test]
fn staged_checker_refuses_unqualified_target_without_creating_files() {
    let (_directory, store) = fixture();
    let generation = encode_hex_id(store.store_id().unwrap());
    store.close().unwrap();
    let before = fs::read(store.paths.database()).unwrap();
    assert!(
        check_staged(
            &store.paths,
            None,
            &generation,
            None,
            Duration::from_secs(2),
            || false
        )
        .is_err()
    );
    assert!(!store.paths.root().join("db.tmp").exists());
    assert_eq!(before, fs::read(store.paths.database()).unwrap());
    let mut request = Vec::new();
    write_target_request(
        &mut request,
        IntegrityKind::Full,
        Duration::from_secs(2),
        None,
        CheckerTarget::Staged {
            store_id: [9; 16],
            operation: Some([8; 16]),
        },
    )
    .unwrap();
    let mut output = Vec::new();
    assert!(run_checker(Cursor::new(request), &mut output, store.paths.root()).is_err());
    assert!(output.is_empty());
    assert_eq!(before, fs::read(store.paths.database()).unwrap());
}

#[test]
fn physical_index_damage_is_detected_and_source_stays_durably_quarantined() {
    let (_directory, store) = fixture();
    let account = store.upsert_account("index-owner", 1).unwrap();
    let root:i64=store.write(Durability::Durable,|tx| {
        tx.execute("INSERT INTO projects(account_id,root,last_used_ms) VALUES(?1,'/index-fixture',1)",[account.0])?;
        let project=tx.last_insert_rowid();
        tx.execute("INSERT INTO tasks(uuid,account_id,project_id,kind,state,title,title_fold,created_ms,updated_ms,last_seq) VALUES('index-task',?1,?2,'desktop','active','SOURCE-CANARY','source-canary',111111111,111111111,1)",rusqlite::params![account.0,project])?;
        let task=tx.last_insert_rowid();
        tx.execute("INSERT INTO entries(task_id,seq,entry_id,kind,created_ms,bytes,payload) VALUES(?1,1,'cafebabe','custom',1,20,?2)",rusqlite::params![task,b"AUTHORITATIVE-CANARY".as_slice()])?;
        Ok(tx.query_row("SELECT rootpage FROM sqlite_schema WHERE name='tasks_recent'",[],|row|row.get(0))?)
    }).unwrap();
    let paths = store.paths.clone();
    store.close().unwrap();
    let mut bytes = fs::read(paths.database()).unwrap();
    let offset = (root as usize - 1) * 4096;
    assert_eq!(bytes[offset], 0x0a, "fixture must have one index leaf page");
    assert_eq!(
        u16::from_be_bytes([bytes[offset + 3], bytes[offset + 4]]),
        1
    );
    let cell = offset + usize::from(u16::from_be_bytes([bytes[offset + 8], bytes[offset + 9]]));
    let payload = usize::from(bytes[cell]);
    assert!(payload < 128);
    bytes[cell + payload] ^= 1;
    fs::write(paths.database(), bytes).unwrap();
    let damaged = Store::unavailable(
        paths.clone(),
        StoreState::Damaged {
            reason: DamageReason::Database,
        },
    )
    .unwrap();
    let connection = open_readonly(&paths, None).unwrap();
    let detected = check_connection(
        &connection,
        IntegrityKind::Full,
        false,
        Duration::from_secs(2),
        &IntegrityCancellation::default(),
        &paths,
    );
    assert_eq!(detected.outcome, IntegrityOutcome::Damaged);
    assert!(detected.findings.iter().any(|finding| matches!(
        finding.kind,
        FindingKind::IndexMissing | FindingKind::IndexCount
    )));
    drop(connection);
    damaged.quarantine_generation().unwrap();
    assert!(matches!(
        damaged.upsert_account("blocked", 2),
        Err(StoreError::Damaged { .. })
    ));
    let source = fs::read(paths.database()).unwrap();
    assert!(paths.setup().exists());
    damaged.close().unwrap();
    assert_eq!(fs::read(paths.database()).unwrap(), source);
    let reopened = Store::unavailable(paths.clone(), StoreState::Maintenance).unwrap();
    reopened.initialize_recovered().unwrap();
    assert!(matches!(reopened.state(), StoreState::Damaged { .. }));
    assert!(reopened.upsert_account("still-blocked", 3).is_err());
    assert_eq!(fs::read(paths.database()).unwrap(), source);
    assert!(paths.setup().exists());
}

#[test]
fn open_failure_diagnostics_are_numeric_redacted_and_additive() {
    const CANARY: &str = "PRIVATE-SQLITE-OPEN-DIAGNOSTIC-CANARY";
    for stage in [
        IntegrityOpenStage::OpenFile,
        IntegrityOpenStage::Engine,
        IntegrityOpenStage::ConnectionConfiguration,
    ] {
        let raw = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN_SYMLINK),
            Some(CANARY.into()),
        );
        let report =
            open_failure_report(stage, StoreError::from(raw), false, Instant::now()).unwrap();
        assert_eq!(
            report.outcome,
            IntegrityOutcome::Partial {
                reason: CheckIncomplete::CheckerUnavailable
            }
        );
        assert!(!report.complete);
        assert!(report.receipt.is_none());
        assert!(report.findings.is_empty());
        assert_eq!(
            report.diagnostics.open_failure,
            Some(IntegrityOpenFailure {
                stage,
                sqlite_code: Some(rusqlite::ffi::SQLITE_CANTOPEN),
                sqlite_extended_code: Some(rusqlite::ffi::SQLITE_CANTOPEN_SYMLINK),
            })
        );
        let encoded = serde_json::to_string(&report).unwrap();
        assert!(!encoded.contains(CANARY));
        let decoded: IntegrityReport = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, report);
    }
    let legacy = serde_json::to_value(IntegrityDiagnostics::default()).unwrap();
    assert!(legacy.get("open_failure").is_none());
    let decoded: IntegrityDiagnostics = serde_json::from_value(legacy).unwrap();
    assert!(decoded.open_failure.is_none());
}

#[test]
fn open_diagnostics_preserve_invalid_provider_damage_and_admission_boundaries() {
    for code in [rusqlite::ffi::SQLITE_CORRUPT, rusqlite::ffi::SQLITE_NOTADB] {
        let report = open_failure_report(
            IntegrityOpenStage::Engine,
            StoreError::Database {
                code,
                extended_code: code,
            },
            true,
            Instant::now(),
        )
        .unwrap();
        assert_eq!(report.outcome, IntegrityOutcome::Damaged);
        assert_eq!(report.findings[0].kind, FindingKind::Authentication);
        assert!(!report.complete);
        assert!(report.receipt.is_none());
        assert_eq!(
            report.diagnostics.open_failure.unwrap().sqlite_code,
            Some(code)
        );
    }
    let invalid = open_failure_report(
        IntegrityOpenStage::Engine,
        StoreError::InvalidData,
        true,
        Instant::now(),
    )
    .unwrap();
    assert_eq!(
        invalid.outcome,
        IntegrityOutcome::Partial {
            reason: CheckIncomplete::CheckerUnavailable
        }
    );
    assert!(!invalid.complete);
    assert!(invalid.receipt.is_none());
    assert_eq!(invalid.diagnostics.open_failure.unwrap().sqlite_code, None);
    let damaged = open_failure_report(
        IntegrityOpenStage::Engine,
        StoreError::Damaged {
            reason: DamageReason::Database,
        },
        true,
        Instant::now(),
    )
    .unwrap();
    assert_eq!(damaged.outcome, IntegrityOutcome::Damaged);
    assert_eq!(damaged.findings[0].kind, FindingKind::Authentication);
    assert!(!damaged.complete);
    assert!(damaged.receipt.is_none());
    for error in [StoreError::UnsafePath, StoreError::InvalidInput] {
        assert_eq!(
            open_failure_report(
                IntegrityOpenStage::OpenFile,
                error.clone(),
                false,
                Instant::now()
            )
            .unwrap_err(),
            error
        );
    }
}
#[test]
fn checker_commands_discard_supplied_environment() {
    let mut command = Command::new(std::env::current_exe().unwrap());
    for name in [
        "MAPLE_CHECKER_PRIVATE_ENV_TEST",
        "OPENSSL_CONF",
        "SystemRoot",
        "WINDIR",
        "TEMP",
        "TMP",
    ] {
        command.env(name, "caller-supplied-environment-canary");
    }
    prepare_checker_command(&mut command).unwrap();
    let environment = command.get_envs().collect::<Vec<_>>();
    #[cfg(not(windows))]
    assert!(environment.is_empty());
    #[cfg(windows)]
    {
        assert_eq!(environment.len(), 1);
        assert_eq!(environment[0].0, std::ffi::OsStr::new("SystemRoot"));
        let directory = native_windows_directory().unwrap();
        assert_eq!(environment[0].1, Some(directory.as_os_str()));
        assert_ne!(
            environment[0].1,
            Some(std::ffi::OsStr::new("caller-supplied-environment-canary"))
        );
    }
}
