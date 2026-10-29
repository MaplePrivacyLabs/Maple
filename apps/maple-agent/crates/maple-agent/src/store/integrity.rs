//! Integrity checks, redacted diagnostics and experimental staged recovery.
//!
//! SQLCipher computes cipher_integrity_check during statement preparation. Its
//! native page loop does not call SQLite's VM progress hook, so full checks run
//! in a read-only child that can be killed and reaped on cancellation/deadline.
//! No password/key enters argv, environment, a temporary file, or diagnostics.

use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::{
    DamageReason, Durability, Store, StoreError, StorePaths, StoreResult, StoreState, engine, keys,
};

#[path = "integrity_constraints.rs"]
mod constraints;
#[path = "integrity_salvage.rs"]
mod salvage;
pub use salvage::{
    SALVAGE_CONFIRMATION, SalvageError, SalvagePreview, SalvageReason, SalvageReport, SalvageRow,
    SalvageRowKey, SalvageTable,
};

const WEEK_MS: i64 = 7 * 24 * 60 * 60 * 1000;
const MAX_FINDINGS: usize = 100;
const MAX_REPORT_BYTES: u64 = 64 * 1024;
const REPORT_PREFIX: &str = "MPLINTREPORT1 ";
const REQUEST_MAGIC: &[u8; 8] = b"MPLINT04";
const MAX_CHECK_DURATION: Duration = Duration::from_secs(60 * 60);

#[derive(Clone, Copy)]
enum CheckerTarget {
    Canonical,
    Staged {
        store_id: [u8; 16],
        operation: Option<[u8; 16]>,
    },
}

#[derive(Clone, Default)]
pub struct IntegrityCancellation(Arc<AtomicBool>);

impl IntegrityCancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrityKind {
    Quick,
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckIncomplete {
    Budget,
    Cancelled,
    CheckerUnavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum IntegrityOutcome {
    Clean,
    Partial { reason: CheckIncomplete },
    Damaged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    IndexMissing,
    IndexCount,
    IndexDuplicate,
    Constraint,
    Structure,
    Authentication,
    ForeignKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrityTable {
    StoreMeta,
    Accounts,
    Credentials,
    AppSettings,
    AccountSettings,
    DictionaryWords,
    Projects,
    McpServers,
    McpServerSecrets,
    Tasks,
    Entries,
    TimelineRows,
    ToolSummaries,
    /// Read-only diagnostics for a pre-file-store schema.
    Blobs,
    FileAttachments,
    SendReceipts,
    RemoteIdentities,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrityFinding {
    pub kind: FindingKind,
    pub table: Option<IntegrityTable>,
    pub page: Option<u64>,
}

/// This shape contains only counters, build versions and fixed enums. SQLite's
/// raw diagnostic strings can contain names or values and never leave this file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrityOpenStage {
    OpenFile,
    Engine,
    ConnectionConfiguration,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrityOpenFailure {
    pub stage: IntegrityOpenStage,
    pub sqlite_code: Option<i32>,
    pub sqlite_extended_code: Option<i32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrityDiagnostics {
    pub sqlite_version: String,
    pub sqlcipher_version: String,
    pub openssl_version: String,
    pub page_size: Option<u64>,
    pub page_count: Option<u64>,
    pub freelist_count: Option<u64>,
    pub schema_version: Option<u64>,
    pub database_bytes: Option<u64>,
    pub wal_bytes: Option<u64>,
    /// Bounded codes only: never retain a SQLite message, SQL or file path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_failure: Option<IntegrityOpenFailure>,
}

impl Default for IntegrityDiagnostics {
    fn default() -> Self {
        Self {
            sqlite_version: engine::SQLITE_VERSION.into(),
            sqlcipher_version: engine::SQLCIPHER_VERSION.into(),
            openssl_version: engine::OPENSSL_VERSION.into(),
            page_size: None,
            page_count: None,
            freelist_count: None,
            schema_version: None,
            database_bytes: None,
            wal_bytes: None,
            open_failure: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrityReport {
    pub kind: IntegrityKind,
    pub outcome: IntegrityOutcome,
    pub complete: bool,
    pub findings: Vec<IntegrityFinding>,
    pub diagnostics: IntegrityDiagnostics,
    pub elapsed_ms: u64,
    pub recorded_last_integrity: bool,
    /// Minted only by this process after a complete full check. The host must
    /// reacquire its idle admission guard before committing this receipt.
    #[serde(skip)]
    pub receipt: Option<IntegrityReceipt>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntegrityReceipt {
    store_id: [u8; 16],
    checked_ms: i64,
}

impl IntegrityReport {
    fn partial(kind: IntegrityKind, reason: CheckIncomplete, start: Instant) -> Self {
        Self {
            kind,
            outcome: IntegrityOutcome::Partial { reason },
            complete: false,
            findings: Vec::new(),
            diagnostics: IntegrityDiagnostics::default(),
            elapsed_ms: elapsed(start),
            recorded_last_integrity: false,
            receipt: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntegrityDue {
    pub quick: bool,
    pub full: bool,
    pub last_full_ms: Option<i64>,
}

impl Store {
    /// The coordinator supplies idle/run-admission protection. A partial quick
    /// check is a retry request, never a declaration that the store is clean.
    pub fn integrity_schedule(
        &self,
        now_ms: i64,
        unclean_shutdown: bool,
    ) -> StoreResult<IntegrityDue> {
        if now_ms < 0 {
            return Err(StoreError::InvalidInput);
        }
        let last = self.read(|tx| {
            let value: Option<i64> = tx
                .query_row(
                    "SELECT value FROM store_meta WHERE key='last_integrity_ms'",
                    [],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(value)
        })?;
        Ok(IntegrityDue {
            quick: unclean_shutdown,
            full: last.is_none_or(|last| now_ms.saturating_sub(last) >= WEEK_MS),
            last_full_ms: last,
        })
    }

    /// Full checks use the trusted current executable's hidden checker command.
    /// This must run on a blocking worker, never the UI or async executor thread.
    pub fn check_integrity(
        &self,
        kind: IntegrityKind,
        budget: Duration,
        cancellation: &IntegrityCancellation,
    ) -> StoreResult<IntegrityReport> {
        validate_budget(budget)?;
        ensure_checkable(self.state())?;
        let start = Instant::now();
        let generation = if self.state() == StoreState::Open {
            Some(self.store_id()?)
        } else {
            None
        };
        if cancellation.is_cancelled() {
            return Ok(IntegrityReport::partial(
                kind,
                CheckIncomplete::Cancelled,
                start,
            ));
        }
        let mut report = match kind {
            IntegrityKind::Quick => {
                let guard = self.bulk.try_lock().map_err(|_| StoreError::Busy)?;
                let connection = guard.as_ref().ok_or(StoreError::Busy)?;
                check_connection(connection, kind, false, budget, cancellation, &self.paths)
            }
            IntegrityKind::Full => {
                self.paths.verify_database()?;
                let command = checker_command(&self.paths)?;
                self.run_integrity_checker(command, kind, budget, cancellation)?
            }
        };
        if report.outcome == IntegrityOutcome::Damaged && !cancellation.is_cancelled() {
            // Reacquire lifecycle admission only for the final small state
            // transition. A check of an old replacement must not quarantine a
            // newly installed generation.
            let lifecycle = self.lifecycle.try_lock().map_err(|_| StoreError::Busy)?;
            let same_generation = match generation {
                Some(generation) => {
                    self.state() == StoreState::Open
                        && self.persistence_status().store_id == Some(encode_hex_id(generation))
                }
                None => matches!(self.state(), StoreState::Damaged { .. }),
            };
            if same_generation {
                self.quarantine_integrity_damage()?;
            }
            drop(lifecycle);
            if same_generation {
                self.quarantine_generation()?;
            }
        }
        if report.outcome == IntegrityOutcome::Clean
            && report.complete
            && kind == IntegrityKind::Full
            && !cancellation.is_cancelled()
            && self.state() == StoreState::Open
            && let Some(generation) = generation
            && self.store_id()? == generation
        {
            report.receipt = Some(IntegrityReceipt {
                store_id: generation,
                checked_ms: super::now_ms(),
            });
        }
        Ok(report)
    }

    /// Call only while holding the host's fresh global idle/admission guard.
    /// The transaction checks generation again so a completed old-generation
    /// check can never stamp a replacement database.
    pub fn record_integrity_clean(
        &self,
        receipt: &IntegrityReceipt,
        cancellation: &IntegrityCancellation,
    ) -> StoreResult<bool> {
        self.write(Durability::Durable,|tx| {
            if cancellation.is_cancelled(){return Ok(false);}
            let generation:Vec<u8>=tx.query_row("SELECT value FROM store_meta WHERE key='store_id'",[],|row|row.get(0))?;
            if generation.as_slice()!=receipt.store_id{return Ok(false);}
            tx.execute("INSERT INTO store_meta(key,value) VALUES ('last_integrity_ms',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[receipt.checked_ms])?;
            tx.execute("DELETE FROM store_meta WHERE key IN ('integrity_incomplete_attempts','integrity_retry_after_ms')",[])?;
            Ok(true)
        })
    }

    fn run_integrity_checker(
        &self,
        mut command: Command,
        kind: IntegrityKind,
        budget: Duration,
        cancellation: &IntegrityCancellation,
    ) -> StoreResult<IntegrityReport> {
        let start = Instant::now();
        let initial_state = self.state();
        if prepare_checker_command(&mut command).is_err() {
            return Ok(IntegrityReport::partial(
                kind,
                CheckIncomplete::CheckerUnavailable,
                start,
            ));
        }
        // Hold lifecycle/key admission only while writing a bounded request,
        // never for the lifetime of the expensive child.
        let spawned = self.with_authenticated_store_key(|key| {
            let mut child = ChildGuard(Some(command.spawn()?));
            let process = child.0.as_mut().ok_or(StoreError::InvalidData)?;
            let mut input = process.stdin.take().ok_or(StoreError::InvalidData)?;
            write_request(&mut input, kind, budget, key)?;
            drop(input);
            let output = process.stdout.take().ok_or(StoreError::InvalidData)?;
            Ok((child, output))
        });
        let (child, output) = match spawned {
            Ok(value) => value,
            Err(super::lifecycle::LifecycleError::Store(StoreError::Io { .. })) => {
                return Ok(IntegrityReport::partial(
                    kind,
                    CheckIncomplete::CheckerUnavailable,
                    start,
                ));
            }
            Err(error) => return Err(lifecycle_store_error(error)),
        };
        wait_checker(child, output, kind, budget, start, || {
            cancellation.is_cancelled() || self.state() != initial_state
        })
    }

    /// Freeze admission before draining the writer. Existing authenticated
    /// connections and the DEK remain available for read-only recovery/checks.
    fn quarantine_integrity_damage(&self) -> StoreResult<()> {
        *self.state.write().map_err(|_| StoreError::Busy)? = StoreState::Damaged {
            reason: DamageReason::Database,
        };
        let worker = self
            .checkpoint_worker
            .lock()
            .map_err(|_| StoreError::Busy)?
            .take();
        if let Some(worker) = worker {
            let _ = worker.sender.send(super::CheckpointMessage::Stop);
            let _ = worker.thread.join();
        }
        if let Some(writer) = self.writer.lock().map_err(|_| StoreError::Busy)?.as_ref() {
            writer.pragma_update(None, "query_only", true)?;
        }
        Ok(())
    }
}

fn wait_checker(
    mut child: ChildGuard,
    output: std::process::ChildStdout,
    kind: IntegrityKind,
    budget: Duration,
    start: Instant,
    should_cancel: impl Fn() -> bool,
) -> StoreResult<IntegrityReport> {
    let (send, receive) = mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = output
            .take(MAX_REPORT_BYTES + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
            .map_err(StoreError::from);
        let _ = send.send(result);
    });
    let mut received = None;
    let outcome = loop {
        if should_cancel() {
            break Some(CheckIncomplete::Cancelled);
        }
        if start.elapsed() >= budget {
            break Some(CheckIncomplete::Budget);
        }
        if received.is_none() {
            match receive.try_recv() {
                Ok(result) => received = Some(result),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    break Some(CheckIncomplete::CheckerUnavailable);
                }
            }
        }
        if received.as_ref().is_some_and(|result| match result {
            Err(_) => true,
            Ok(bytes) => bytes.len() as u64 > MAX_REPORT_BYTES,
        }) {
            break Some(CheckIncomplete::CheckerUnavailable);
        }
        match child.0.as_mut().ok_or(StoreError::InvalidData)?.try_wait() {
            Ok(Some(status)) => {
                child.0.take();
                if !status.success() {
                    break Some(CheckIncomplete::CheckerUnavailable);
                }
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => break Some(CheckIncomplete::CheckerUnavailable),
        }
    };
    if outcome.is_some() {
        child.terminate();
    }
    reader.join().map_err(|_| StoreError::InvalidData)?;
    if let Some(reason) = outcome {
        return Ok(IntegrityReport::partial(kind, reason, start));
    }
    let output = match received {
        Some(result) => result?,
        None => receive.recv().map_err(|_| StoreError::InvalidData)??,
    };
    decode_report(&output, kind).ok_or(StoreError::InvalidData)
}

/// The lifecycle owns the authenticated key and journal while a staged copy is
/// checked. Only the exact, generation-qualified db.tmp target is admitted.
pub(in crate::store) fn check_staged(
    paths: &StorePaths,
    key: Option<&keys::LockedKey>,
    expected_store_id: &str,
    expected_setup_op: Option<&str>,
    budget: Duration,
    should_cancel: impl Fn() -> bool,
) -> StoreResult<IntegrityReport> {
    validate_budget(budget)?;
    let start = Instant::now();
    if should_cancel() {
        return Ok(IntegrityReport::partial(
            IntegrityKind::Full,
            CheckIncomplete::Cancelled,
            start,
        ));
    }
    let target = CheckerTarget::Staged {
        store_id: decode_hex_id(expected_store_id)?,
        operation: expected_setup_op.map(decode_hex_id).transpose()?,
    };
    checker_target_path(paths, target)?;
    let mut command = checker_command(paths)?;
    if prepare_checker_command(&mut command).is_err() {
        return Ok(IntegrityReport::partial(
            IntegrityKind::Full,
            CheckIncomplete::CheckerUnavailable,
            start,
        ));
    }
    let mut child = ChildGuard(Some(command.spawn()?));
    let process = child.0.as_mut().ok_or(StoreError::InvalidData)?;
    let mut input = process.stdin.take().ok_or(StoreError::InvalidData)?;
    write_target_request(&mut input, IntegrityKind::Full, budget, key, target)?;
    drop(input);
    let output = process.stdout.take().ok_or(StoreError::InvalidData)?;
    let report = wait_checker(
        child,
        output,
        IntegrityKind::Full,
        budget,
        start,
        should_cancel,
    )?;
    checker_target_path(paths, target)?;
    Ok(report)
}

/// Keep credentials and caller configuration out of the checker environment.
/// Windows system crypto providers need their native OS directory to initialize.
fn prepare_checker_command(command: &mut Command) -> StoreResult<()> {
    command.env_clear();
    #[cfg(windows)]
    command.env("SystemRoot", native_windows_directory()?);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    Ok(())
}

#[cfg(windows)]
fn native_windows_directory() -> StoreResult<std::ffi::OsString> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::SystemInformation::GetWindowsDirectoryW;

    let mut directory = vec![0_u16; 32_768];
    // The API writes only within this initialized buffer and returns its length.
    let length =
        unsafe { GetWindowsDirectoryW(directory.as_mut_ptr(), directory.len() as u32) } as usize;
    if length == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if length >= directory.len() {
        return Err(StoreError::InvalidData);
    }
    Ok(std::ffi::OsString::from_wide(&directory[..length]))
}

fn checker_command(paths: &StorePaths) -> StoreResult<Command> {
    // Keep the checker as our direct child, including bundled-loader packages.
    // Bypass launcher wrappers so watchdog parent verification and direct
    // cancellation/reaping are preserved. No key enters argv or environment.
    let mut command = crate::self_launch::child_command()
        .map_err(|_| std::io::Error::other("integrity checker executable is unavailable"))?;
    #[cfg(test)]
    command.args([
        "--exact",
        "store::integrity::tests::checker_child",
        "--ignored",
        "--nocapture",
        "--skip",
    ]);
    #[cfg(not(test))]
    command.arg("--internal-store-integrity-check");
    command.arg(paths.root());
    Ok(command)
}

fn checker_target_path(
    paths: &StorePaths,
    target: CheckerTarget,
) -> StoreResult<std::path::PathBuf> {
    match target {
        CheckerTarget::Canonical => {
            paths.verify_database()?;
            Ok(paths.database())
        }
        CheckerTarget::Staged {
            store_id,
            operation,
        } => super::lifecycle::validate_staged_checker(
            paths,
            &encode_hex_id(store_id),
            operation.map(encode_hex_id).as_deref(),
        ),
    }
}

fn decode_hex_id(value: &str) -> StoreResult<[u8; 16]> {
    if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(StoreError::InvalidInput);
    }
    let mut decoded = [0_u8; 16];
    for (index, byte) in decoded.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| StoreError::InvalidInput)?;
    }
    Ok(decoded)
}

fn encode_hex_id(value: [u8; 16]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    value
        .iter()
        .flat_map(|byte| {
            [
                HEX[(byte >> 4) as usize] as char,
                HEX[(byte & 15) as usize] as char,
            ]
        })
        .collect()
}

/// Duplicate stdin's OS handle without reading through its global buffered
/// reader. The owned File reads directly into the caller's destination and
/// closes only its duplicate, leaving the process's original stdin intact.
fn checker_stdin() -> std::io::Result<fs::File> {
    let input = std::io::stdin();
    #[cfg(unix)]
    {
        use std::os::fd::AsFd;
        input.as_fd().try_clone_to_owned().map(fs::File::from)
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsHandle;
        input.as_handle().try_clone_to_owned().map(fs::File::from)
    }
}

/// Hidden CLI entry. Key bytes never pass through std's global stdin buffer.
pub fn run_checker_stdio(root: &Path) -> StoreResult<()> {
    run_checker(checker_stdin()?, std::io::stdout().lock(), root)
}

/// Verify existing paths only; never create a store. Keep the generic reader
/// private so production callers cannot accidentally supply a buffered input.
fn run_checker(mut input: impl Read, mut output: impl Write, root: &Path) -> StoreResult<()> {
    harden_checker()?;
    let paths = existing_root(root)?;
    let mut header = [0_u8; 64];
    input.read_exact(&mut header)?;
    if &header[..8] != REQUEST_MAGIC || header[8] > 1 || header[9] != 1 {
        return Err(StoreError::InvalidInput);
    }
    let budget = Duration::from_millis(u64::from_le_bytes(
        header[10..18]
            .try_into()
            .map_err(|_| StoreError::InvalidInput)?,
    ));
    validate_budget(budget)?;
    let parent = CheckerParent {
        id: u32::from_le_bytes(
            header[18..22]
                .try_into()
                .map_err(|_| StoreError::InvalidInput)?,
        ),
        created: u64::from_le_bytes(
            header[56..64]
                .try_into()
                .map_err(|_| StoreError::InvalidInput)?,
        ),
    };
    let _watchdog = CheckerWatchdog::start(parent, budget)?;
    let target = match (header[22], header[39]) {
        (0, 0)
            if header[23..39].iter().all(|byte| *byte == 0)
                && header[40..56].iter().all(|byte| *byte == 0) =>
        {
            CheckerTarget::Canonical
        }
        (1, operation) if operation <= 1 => CheckerTarget::Staged {
            store_id: header[23..39]
                .try_into()
                .map_err(|_| StoreError::InvalidInput)?,
            operation: if operation == 1 {
                Some(
                    header[40..56]
                        .try_into()
                        .map_err(|_| StoreError::InvalidInput)?,
                )
            } else {
                None
            },
        },
        _ => return Err(StoreError::InvalidInput),
    };
    let database = checker_target_path(&paths, target)?;
    let key = if header[8] == 1 {
        Some(
            keys::LockedKey::read_from(&mut input).map_err(|error| match error {
                keys::KeyError::Io { kind } => std::io::Error::from(kind).into(),
                _ => StoreError::InvalidData,
            })?,
        )
    } else {
        None
    };
    let mut trailing = [0_u8; 1];
    if input.read(&mut trailing)? != 0 {
        return Err(StoreError::InvalidInput);
    }
    let open_start = Instant::now();
    let mut report = match open_readonly_path_diagnostic(&database, key.as_ref()) {
        Ok(connection) => check_connection(
            &connection,
            IntegrityKind::Full,
            key.is_some(),
            budget,
            &IntegrityCancellation::default(),
            &paths,
        ),
        Err((stage, error)) => open_failure_report(stage, error, key.is_some(), open_start)?,
    };
    checker_target_path(&paths, target)?;
    report.diagnostics.database_bytes = fs::metadata(&database).ok().map(|metadata| metadata.len());
    report.diagnostics.wal_bytes = fs::metadata(database.with_file_name("maple.db-wal"))
        .ok()
        .map(|metadata| metadata.len());
    let encoded = serde_json::to_vec(&report).map_err(|_| StoreError::InvalidData)?;
    if encoded.len() as u64 > MAX_REPORT_BYTES - REPORT_PREFIX.len() as u64 - 1 {
        return Err(StoreError::InvalidData);
    }
    output.write_all(REPORT_PREFIX.as_bytes())?;
    output.write_all(&encoded)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

fn check_connection(
    connection: &Connection,
    kind: IntegrityKind,
    keyed: bool,
    budget: Duration,
    cancel: &IntegrityCancellation,
    paths: &StorePaths,
) -> IntegrityReport {
    let start = Instant::now();
    let interrupted = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&interrupted);
    let cancellation = cancel.clone();
    if connection
        .progress_handler(
            1000,
            Some(move || {
                let stop = cancellation.is_cancelled() || start.elapsed() >= budget;
                if stop {
                    observed.store(true, Ordering::Release);
                }
                stop
            }),
        )
        .is_err()
    {
        return IntegrityReport::partial(kind, CheckIncomplete::CheckerUnavailable, start);
    }
    let _progress = ProgressReset(connection);
    let mut findings = Vec::new();
    let mut complete = true;
    let mut unavailable = false;
    let sqlite = match kind {
        IntegrityKind::Quick => "PRAGMA quick_check(100)",
        IntegrityKind::Full => "PRAGMA integrity_check(100)",
    };
    if let Err(error) = collect_check(connection, sqlite, false, &mut findings) {
        complete = false;
        // The progress hook distinguishes our cancellation/deadline from an
        // unrelated operational error returned by SQLite.
        if !interrupted.load(Ordering::Acquire) {
            if is_corruption_error(error) {
                findings.push(finding(FindingKind::Structure));
            } else {
                unavailable = true;
            }
        }
    }
    if kind == IntegrityKind::Full
        && keyed
        && complete
        && let Err(error) = collect_check(
            connection,
            "PRAGMA cipher_integrity_check",
            true,
            &mut findings,
        )
    {
        complete = false;
        if !interrupted.load(Ordering::Acquire) {
            if is_corruption_error(error) {
                findings.push(finding(FindingKind::Authentication));
            } else {
                unavailable = true;
            }
        }
    }
    if kind == IntegrityKind::Full && complete {
        let foreign = (|| -> rusqlite::Result<()> {
            let mut statement = connection.prepare("PRAGMA foreign_key_check")?;
            let mut rows = statement.query([])?;
            while rows.next()?.is_some() {
                findings.push(finding(FindingKind::ForeignKey));
                if findings.len() >= MAX_FINDINGS {
                    break;
                }
            }
            Ok(())
        })();
        if let Err(error) = foreign {
            complete = false;
            if !interrupted.load(Ordering::Acquire) {
                if is_corruption_error(error) {
                    findings.push(finding(FindingKind::Structure));
                } else {
                    unavailable = true;
                }
            }
        }
    }
    if kind == IntegrityKind::Full && complete {
        let constraints = constraints::collect(connection, &mut findings, || {
            let stopped = cancel.is_cancelled() || start.elapsed() >= budget;
            if stopped {
                interrupted.store(true, Ordering::Release);
            }
            stopped
        });
        if let Err(error) = constraints {
            complete = false;
            if !interrupted.load(Ordering::Acquire) {
                if is_corruption_error(error) {
                    findings.push(finding(FindingKind::Structure));
                } else {
                    unavailable = true;
                }
            }
        }
    }
    let diagnostics = diagnostics(connection, paths);
    drop(_progress);
    let expired = start.elapsed() >= budget;
    if cancel.is_cancelled() || expired {
        complete = false;
    }
    if findings.len() >= MAX_FINDINGS {
        complete = false;
    }
    findings.truncate(MAX_FINDINGS);
    let outcome = if !findings.is_empty() {
        IntegrityOutcome::Damaged
    } else if !complete {
        IntegrityOutcome::Partial {
            reason: if cancel.is_cancelled() {
                CheckIncomplete::Cancelled
            } else if expired {
                CheckIncomplete::Budget
            } else if unavailable {
                CheckIncomplete::CheckerUnavailable
            } else {
                CheckIncomplete::Budget
            },
        }
    } else {
        IntegrityOutcome::Clean
    };
    IntegrityReport {
        kind,
        outcome,
        complete,
        findings,
        diagnostics,
        elapsed_ms: elapsed(start),
        recorded_last_integrity: false,
        receipt: None,
    }
}

fn is_corruption_error(error: rusqlite::Error) -> bool {
    // The central conversion only labels CORRUPT/NOTADB as damage. Failure to
    // read or lock the store is not evidence that its contents are corrupt.
    matches!(StoreError::from(error), StoreError::Damaged { .. })
}

fn collect_check(
    connection: &Connection,
    sql: &str,
    cipher: bool,
    findings: &mut Vec<IntegrityFinding>,
) -> rusqlite::Result<()> {
    let mut statement = connection.prepare(sql)?;
    let mut rows = statement.query([])?;
    let mut messages = 0;
    while let Some(row) = rows.next()? {
        let message: String = row.get(0)?;
        messages += 1;
        if message != "ok" && findings.len() < MAX_FINDINGS {
            findings.push(classify(&message, cipher));
        }
        if findings.len() >= MAX_FINDINGS {
            break;
        }
    }
    if !cipher && messages == 0 {
        findings.push(finding(FindingKind::Structure));
    }
    Ok(())
}

fn classify(message: &str, cipher: bool) -> IntegrityFinding {
    let kind = if cipher {
        FindingKind::Authentication
    } else if message.contains("missing from index") {
        FindingKind::IndexMissing
    } else if message.contains("wrong # of entries in index") {
        FindingKind::IndexCount
    } else if message.contains("non-unique entry in index") {
        FindingKind::IndexDuplicate
    } else if message.contains("constraint") || message.contains("NULL value") {
        FindingKind::Constraint
    } else {
        FindingKind::Structure
    };
    let table = [
        ("store_meta", IntegrityTable::StoreMeta),
        ("accounts", IntegrityTable::Accounts),
        ("credentials", IntegrityTable::Credentials),
        ("app_settings", IntegrityTable::AppSettings),
        ("account_settings", IntegrityTable::AccountSettings),
        ("dictionary_words", IntegrityTable::DictionaryWords),
        ("projects", IntegrityTable::Projects),
        ("mcp_servers", IntegrityTable::McpServers),
        ("mcp_server_secrets", IntegrityTable::McpServerSecrets),
        ("tasks", IntegrityTable::Tasks),
        ("entries", IntegrityTable::Entries),
        ("timeline_rows", IntegrityTable::TimelineRows),
        ("tool_summaries", IntegrityTable::ToolSummaries),
        ("file_attachments", IntegrityTable::FileAttachments),
        ("send_receipts", IntegrityTable::SendReceipts),
        ("remote_identities", IntegrityTable::RemoteIdentities),
    ]
    .into_iter()
    .find_map(|(name, table)| {
        message
            .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
            .any(|part| {
                part == name
                    || part
                        .strip_prefix(name)
                        .is_some_and(|tail| tail.starts_with('_'))
            })
            .then_some(table)
    });
    let page = message
        .split_whitespace()
        .collect::<Vec<_>>()
        .windows(2)
        .find_map(|pair| {
            (pair[0].eq_ignore_ascii_case("page"))
                .then(|| pair[1].parse::<u64>().ok())
                .flatten()
        });
    IntegrityFinding { kind, table, page }
}

fn finding(kind: FindingKind) -> IntegrityFinding {
    IntegrityFinding {
        kind,
        table: None,
        page: None,
    }
}
fn elapsed(start: Instant) -> u64 {
    start.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}
fn validate_budget(budget: Duration) -> StoreResult<()> {
    if budget.is_zero() || budget > MAX_CHECK_DURATION {
        Err(StoreError::InvalidInput)
    } else {
        Ok(())
    }
}
fn lifecycle_store_error(error: super::lifecycle::LifecycleError) -> StoreError {
    match error {
        super::lifecycle::LifecycleError::Store(error) => error,
        super::lifecycle::LifecycleError::Key(_) => StoreError::Locked,
        super::lifecycle::LifecycleError::Generation => StoreError::Damaged {
            reason: DamageReason::Generation,
        },
        super::lifecycle::LifecycleError::Cancelled => StoreError::Busy,
        _ => StoreError::InvalidData,
    }
}
fn ensure_checkable(state: StoreState) -> StoreResult<()> {
    match state {
        StoreState::Open | StoreState::Damaged { .. } => Ok(()),
        StoreState::Locked => Err(StoreError::Locked),
        StoreState::NeedsSetup => Err(StoreError::NeedsSetup),
        StoreState::Maintenance => Err(StoreError::Busy),
        StoreState::Refused { reason } => Err(StoreError::Refused { reason }),
    }
}

fn diagnostics(connection: &Connection, paths: &StorePaths) -> IntegrityDiagnostics {
    let counter = |sql: &str| {
        connection
            // SQLCipher returns keyed page_size as decimal TEXT.
            .query_row(sql, [], |row| {
                Ok(match row.get_ref(0)? {
                    rusqlite::types::ValueRef::Integer(value) => u64::try_from(value).ok(),
                    rusqlite::types::ValueRef::Text(value)
                        if !value.is_empty() && value.iter().all(u8::is_ascii_digit) =>
                    {
                        std::str::from_utf8(value)
                            .ok()
                            .and_then(|value| value.parse::<u64>().ok())
                    }
                    _ => None,
                })
            })
            .ok()
            .flatten()
    };
    IntegrityDiagnostics {
        page_size: counter("PRAGMA page_size"),
        page_count: counter("PRAGMA page_count"),
        freelist_count: counter("PRAGMA freelist_count"),
        schema_version: counter("PRAGMA user_version"),
        database_bytes: fs::metadata(paths.database())
            .ok()
            .map(|metadata| metadata.len()),
        wal_bytes: fs::metadata(paths.db().join("maple.db-wal"))
            .ok()
            .map(|metadata| metadata.len()),
        ..Default::default()
    }
}

#[cfg(test)]
fn existing_paths(root: &Path) -> StoreResult<StorePaths> {
    let paths = existing_root(root)?;
    paths.verify_database()?;
    Ok(paths)
}

fn existing_root(root: &Path) -> StoreResult<StorePaths> {
    if !root.is_absolute()
        || root.file_name() != Some(std::ffi::OsStr::new("store-v1"))
        || root.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
    {
        return Err(StoreError::UnsafePath);
    }
    let paths = StorePaths::new(root.parent().ok_or(StoreError::UnsafePath)?);
    super::paths::verify_private_path(paths.root(), true)?;
    Ok(paths)
}

fn open_readonly(paths: &StorePaths, key: Option<&keys::LockedKey>) -> StoreResult<Connection> {
    paths.verify_database()?;
    open_readonly_path(&paths.database(), key)
}

fn open_readonly_path(path: &Path, key: Option<&keys::LockedKey>) -> StoreResult<Connection> {
    open_readonly_path_diagnostic(path, key).map_err(|(_, error)| error)
}

fn open_readonly_path_diagnostic(
    path: &Path,
    key: Option<&keys::LockedKey>,
) -> Result<Connection, (IntegrityOpenStage, StoreError)> {
    let file_error = |error| (IntegrityOpenStage::OpenFile, error);
    super::paths::verify_private_path(path, false).map_err(file_error)?;
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|error| file_error(StoreError::from(error)))?;
    let initialized = if let Some(key) = key {
        engine::key_connection(&connection, key)
    } else {
        engine::guard_plain(&connection)
    };
    initialized.map_err(|error| (IntegrityOpenStage::Engine, error))?;
    let configured = (|| -> StoreResult<()> {
        connection.pragma_update(None, "query_only", true)?;
        connection.pragma_update(None, "trusted_schema", false)?;
        connection.pragma_update(None, "temp_store", "MEMORY")?;
        connection.busy_timeout(Duration::from_secs(1))?;
        Ok(())
    })();
    configured.map_err(|error| (IntegrityOpenStage::ConnectionConfiguration, error))?;
    Ok(connection)
}

fn open_failure_report(
    stage: IntegrityOpenStage,
    error: StoreError,
    keyed: bool,
    start: Instant,
) -> StoreResult<IntegrityReport> {
    let codes = match &error {
        StoreError::Database {
            code,
            extended_code,
        } => (Some(*code), Some(*extended_code)),
        _ => (None, None),
    };
    let mut report = match error {
        StoreError::Damaged { .. }
        | StoreError::Database {
            code: rusqlite::ffi::SQLITE_CORRUPT | rusqlite::ffi::SQLITE_NOTADB,
            ..
        } => {
            let mut report = IntegrityReport::partial(
                IntegrityKind::Full,
                CheckIncomplete::CheckerUnavailable,
                start,
            );
            report.outcome = IntegrityOutcome::Damaged;
            report.findings.push(finding(if keyed {
                FindingKind::Authentication
            } else {
                FindingKind::Structure
            }));
            report
        }
        StoreError::Database { .. } | StoreError::InvalidData => IntegrityReport::partial(
            IntegrityKind::Full,
            CheckIncomplete::CheckerUnavailable,
            start,
        ),
        // Path/admission failures keep their existing hard-error boundary.
        other => return Err(other),
    };
    report.diagnostics.open_failure = Some(IntegrityOpenFailure {
        stage,
        sqlite_code: codes.0,
        sqlite_extended_code: codes.1,
    });
    Ok(report)
}

fn write_request(
    output: &mut impl Write,
    kind: IntegrityKind,
    budget: Duration,
    key: Option<&keys::LockedKey>,
) -> StoreResult<()> {
    write_target_request(output, kind, budget, key, CheckerTarget::Canonical)
}

fn write_target_request(
    output: &mut impl Write,
    kind: IntegrityKind,
    budget: Duration,
    key: Option<&keys::LockedKey>,
    target: CheckerTarget,
) -> StoreResult<()> {
    let mut header = [0_u8; 64];
    header[..8].copy_from_slice(REQUEST_MAGIC);
    header[8] = u8::from(key.is_some());
    header[9] = match kind {
        IntegrityKind::Quick => 0,
        IntegrityKind::Full => 1,
    };
    header[10..18]
        .copy_from_slice(&(budget.as_millis().min(u128::from(u64::MAX)) as u64).to_le_bytes());
    let parent = CheckerParent::current()?;
    header[18..22].copy_from_slice(&parent.id.to_le_bytes());
    header[56..64].copy_from_slice(&parent.created.to_le_bytes());
    if let CheckerTarget::Staged {
        store_id,
        operation,
    } = target
    {
        header[22] = 1;
        header[23..39].copy_from_slice(&store_id);
        if let Some(operation) = operation {
            header[39] = 1;
            header[40..56].copy_from_slice(&operation);
        }
    }
    output.write_all(&header)?;
    if let Some(key) = key {
        output.write_all(key.as_bytes())?;
    }
    output.flush()?;
    Ok(())
}

fn decode_report(output: &[u8], kind: IntegrityKind) -> Option<IntegrityReport> {
    if output.len() as u64 > MAX_REPORT_BYTES {
        return None;
    }
    let text = std::str::from_utf8(output).ok()?;
    let mut reports = text
        .lines()
        .filter_map(|line| line.strip_prefix(REPORT_PREFIX));
    let report: IntegrityReport = serde_json::from_str(reports.next()?).ok()?;
    if reports.next().is_some()
        || report.kind != kind
        || report.findings.len() > MAX_FINDINGS
        || (report.outcome == IntegrityOutcome::Clean
            && (!report.complete || !report.findings.is_empty()))
        || report.diagnostics.sqlite_version != engine::SQLITE_VERSION
        || report.diagnostics.sqlcipher_version != engine::SQLCIPHER_VERSION
        || report.diagnostics.openssl_version != engine::OPENSSL_VERSION
    {
        return None;
    }
    Some(report)
}

struct ProgressReset<'a>(&'a Connection);
impl Drop for ProgressReset<'_> {
    fn drop(&mut self) {
        let _ = self.0.progress_handler(0, None::<fn() -> bool>);
    }
}
struct ChildGuard(Option<Child>);
impl ChildGuard {
    fn terminate(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[derive(Clone, Copy)]
struct CheckerParent {
    id: u32,
    /// Windows process creation FILETIME. Other platforms require zero.
    created: u64,
}
impl CheckerParent {
    fn current() -> StoreResult<Self> {
        #[cfg(windows)]
        let created = windows_parent::creation_time(unsafe {
            windows_sys::Win32::System::Threading::GetCurrentProcess()
        })?;
        #[cfg(not(windows))]
        let created = 0;
        Ok(Self {
            id: std::process::id(),
            created,
        })
    }
}

/// SQLCipher's native cipher check cannot be interrupted by SQLite. This
/// independent thread bounds secret lifetime even if the owner is abruptly
/// terminated before its parent-side ChildGuard can run.
struct CheckerWatchdog {
    stop: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl CheckerWatchdog {
    fn start(parent: CheckerParent, budget: Duration) -> StoreResult<Self> {
        if parent.id == 0 {
            return Err(StoreError::InvalidInput);
        }
        // Establish parent liveness before the caller reads any key bytes.
        // The retained Windows handle follows the exact process object, and
        // its creation time rejects PID reuse before that handle was opened.
        #[cfg(windows)]
        let parent_process = windows_parent::Process::open(parent)?;
        #[cfg(not(windows))]
        if parent.created != 0 {
            return Err(StoreError::InvalidInput);
        }
        let (stop, receive) = mpsc::channel();
        let deadline = Instant::now() + budget + Duration::from_millis(100);
        let thread = std::thread::Builder::new()
            .name("store-integrity-watchdog".into())
            .spawn(move || {
                loop {
                    if Instant::now() >= deadline {
                        std::process::exit(124);
                    }
                    #[cfg(unix)]
                    if parent.id != std::process::id()
                        && unsafe { libc::getppid() } as u32 != parent.id
                    {
                        std::process::exit(125);
                    }
                    #[cfg(windows)]
                    if !parent_process.is_alive().unwrap_or(false) {
                        std::process::exit(125);
                    }
                    match receive.recv_timeout(Duration::from_millis(25)) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

#[cfg(windows)]
mod windows_parent {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

    use windows_sys::Win32::{
        Foundation::{FILETIME, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::Threading::{
            GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
            WaitForSingleObject,
        },
    };

    use super::{CheckerParent, StoreError, StoreResult};

    pub(super) fn creation_time(process: HANDLE) -> StoreResult<u64> {
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        if unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) }
            == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    }

    pub(super) struct Process(OwnedHandle);
    impl Process {
        pub(super) fn open(parent: CheckerParent) -> StoreResult<Self> {
            let handle = unsafe {
                OpenProcess(
                    PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                    0,
                    parent.id,
                )
            };
            if handle.is_null() {
                return Err(std::io::Error::last_os_error().into());
            }
            // OpenProcess returns a new owned, non-inheritable process handle.
            let process = Self(unsafe { OwnedHandle::from_raw_handle(handle) });
            if creation_time(process.0.as_raw_handle())? != parent.created || !process.is_alive()? {
                return Err(StoreError::InvalidInput);
            }
            Ok(process)
        }

        pub(super) fn is_alive(&self) -> StoreResult<bool> {
            match unsafe { WaitForSingleObject(self.0.as_raw_handle(), 0) } {
                WAIT_TIMEOUT => Ok(true),
                WAIT_OBJECT_0 => Ok(false),
                _ => Err(std::io::Error::last_os_error().into()),
            }
        }
    }
}
impl Drop for CheckerWatchdog {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn harden_checker() -> StoreResult<()> {
    #[cfg(unix)]
    {
        let limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &limit) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    #[cfg(target_os = "linux")]
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(test)]
#[path = "integrity_tests.rs"]
mod tests;
