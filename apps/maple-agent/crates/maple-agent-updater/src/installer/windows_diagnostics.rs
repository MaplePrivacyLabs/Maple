//! Bounded installer diagnostics. No application state or arbitrary error text.
use super::*;
use std::fmt;
#[cfg(any(windows, test))]
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowsInstallOperation {
    Checker,
    Destination,
    Parent,
    ExistingInstallation,
    Preparation,
    InstallLock,
    Registration,
    Payload,
    Activation,
}

impl fmt::Display for WindowsInstallOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Checker => "verify-checker-image",
            Self::Destination => "validate-destination-path",
            Self::Parent => "validate-installation-parent",
            Self::ExistingInstallation => "inspect-existing-installation",
            Self::Preparation => "prepare-installation",
            Self::InstallLock => "acquire-installation-lock",
            Self::Registration => "register-installation",
            Self::Payload => "copy-verified-payload",
            Self::Activation => "activate-installation",
        })
    }
}

/// Source-owned labels only; never populate this from a filesystem path/error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowsStorageGuard {
    Unspecified,
    PathEncoding,
    NonlocalPath,
    NonnormalComponent,
    AmbiguousComponent,
    PathIdentity,
    ReparsePoint,
    HardLinks,
    AncestorType,
    Owner,
    NullDacl,
    InvalidDacl,
    DaclInformation,
    AceRead,
    AceType,
    AceSize,
    SidBounds,
    ForeignWriter,
    FileSize,
}

impl fmt::Display for WindowsStorageGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unspecified => "unspecified-guard",
            Self::PathEncoding => "path-encoding-or-length",
            Self::NonlocalPath => "nonlocal-or-relative-path",
            Self::NonnormalComponent => "nonnormal-path-component",
            Self::AmbiguousComponent => "ambiguous-path-component",
            Self::PathIdentity => "opened-path-identity-mismatch",
            Self::ReparsePoint => "reparse-point",
            Self::HardLinks => "file-hardlink-count",
            Self::AncestorType => "ancestor-not-directory",
            Self::Owner => "owner-outside-trusted-set",
            Self::NullDacl => "null-dacl",
            Self::InvalidDacl => "invalid-dacl",
            Self::DaclInformation => "dacl-information-or-count",
            Self::AceRead => "dacl-ace-read",
            Self::AceType => "unsupported-effective-ace-type",
            Self::AceSize => "short-allowed-ace",
            Self::SidBounds => "ace-sid-bounds-or-validity",
            Self::ForeignWriter => "foreign-write-ace",
            Self::FileSize => "file-size-limit",
        })
    }
}
impl std::error::Error for WindowsStorageGuard {}

/// A storage guard refusal plus, where the guard inspected an object, what it
/// found. The detail is built only from fixed words and numbers: the kind of
/// account an entry names (never its name, SID or domain), an access mask, a
/// reparse tag or a path depth. It never contains a path or a user name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowsGuardFailure {
    pub guard: WindowsStorageGuard,
    pub detail: Option<String>,
}

impl WindowsGuardFailure {
    pub fn new(guard: WindowsStorageGuard) -> Self {
        Self {
            guard,
            detail: None,
        }
    }

    /// An effective allow entry grants a write right to an account outside
    /// the installing user, SYSTEM and Administrators.
    pub fn foreign_writer(
        sid: &[u8],
        user: Option<&[u8]>,
        access: u32,
        inherited: bool,
        directory: bool,
    ) -> Self {
        Self {
            guard: WindowsStorageGuard::ForeignWriter,
            detail: Some(format!(
                "account: {}; access: 0x{access:08X}; inherited: {}; object: {}",
                sid_class(sid, user),
                if inherited { "yes" } else { "no" },
                if directory { "folder" } else { "file" },
            )),
        }
    }

    pub fn owner(sid: &[u8], user: Option<&[u8]>, directory: bool) -> Self {
        Self {
            guard: WindowsStorageGuard::Owner,
            detail: Some(format!(
                "owner: {}; object: {}",
                sid_class(sid, user),
                if directory { "folder" } else { "file" },
            )),
        }
    }

    /// `depth` is the 1-based position of the linked folder in the path and
    /// the path's component count, counting the drive root.
    pub fn reparse(tag: Option<u32>, depth: Option<(usize, usize)>) -> Self {
        let mut detail = format!("link: {}", tag.map_or("unknown".into(), reparse_tag_name));
        if let Some((position, count)) = depth {
            detail.push_str(&format!("; path component: {position} of {count}"));
        }
        Self {
            guard: WindowsStorageGuard::ReparsePoint,
            detail: Some(detail),
        }
    }
}

impl fmt::Display for WindowsGuardFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.guard.fmt(f)
    }
}
impl std::error::Error for WindowsGuardFailure {}

/// Classify a binary SID by the kind of account it names. Domain and machine
/// identifiers are dropped; only relative IDs of well-known accounts and
/// groups remain. `user` is the current user's SID, which tells another
/// account on this computer or domain apart from one from elsewhere, such as a
/// profile copied from an old computer.
pub fn sid_class(sid: &[u8], user: Option<&[u8]>) -> String {
    let Some((authority, subs)) = parse_sid(sid) else {
        return "malformed-sid".into();
    };
    let class = match (authority, subs.as_slice()) {
        (1, [0]) => "everyone",
        (2, [0]) => "local-logon",
        (2, [1]) => "console-logon",
        (3, [0]) => "creator-owner",
        (3, [1]) => "creator-group",
        (3, [4]) => "owner-rights",
        (5, [1]) => "dialup",
        (5, [2]) => "network",
        (5, [3]) => "batch",
        (5, [4]) => "interactive",
        (5, [5, ..]) => "logon-session",
        (5, [6]) => "service",
        (5, [7]) => "anonymous",
        (5, [10]) => "self",
        (5, [11]) => "authenticated-users",
        (5, [12]) => "restricted",
        (5, [13]) => "terminal-server-users",
        (5, [14]) => "remote-interactive",
        (5, [15]) => "this-organization",
        (5, [17]) => "iusr",
        (5, [18]) => "system",
        (5, [19]) => "local-service",
        (5, [20]) => "network-service",
        (5, [113]) => "local-account",
        (5, [114]) => "local-administrator-account",
        (5, [32, 544]) => "administrators",
        (5, [32, 545]) => "users",
        (5, [32, 546]) => "guests",
        (5, [32, 547]) => "power-users",
        (5, [32, 551]) => "backup-operators",
        (5, [32, 555]) => "remote-desktop-users",
        (5, [32, 568]) => "iis-users",
        (5, [32, rid]) => return format!("builtin-group-rid-{rid}"),
        (5, [21, a, b, c, rid]) => {
            let user = user.and_then(parse_sid);
            let scope = match &user {
                None => "domain",
                Some((5, user)) if user.len() == 5 && user[..4] == [21, *a, *b, *c] => {
                    if user[4] == *rid {
                        return "current-user".into();
                    }
                    "same-domain"
                }
                Some(_) => "other-domain",
            };
            let name = match rid {
                500 => "-administrator",
                501 => "-guest",
                512 => "-domain-admins",
                513 => "-domain-users",
                514 => "-domain-guests",
                515 => "-domain-computers",
                _ => "",
            };
            return format!("{scope}-account-rid-{rid}{name}");
        }
        (5, [80, ..]) => "service-account",
        (5, [82, ..]) => "iis-app-pool",
        (5, [83, ..]) => "virtual-machine",
        (5, [90, ..]) => "window-manager",
        (5, [96, ..]) => "font-driver-host",
        (12, [1, ..]) => "entra-id-account",
        (15, [2, 1]) => "all-application-packages",
        (15, [2, 2]) => "all-restricted-application-packages",
        (15, [2, ..]) => "application-package",
        (15, [3, ..]) => "capability",
        (16, _) => "integrity-label",
        (authority, [first, ..]) => return format!("authority-{authority}-subauthority-{first}"),
        (authority, []) => return format!("authority-{authority}"),
    };
    class.into()
}

fn parse_sid(sid: &[u8]) -> Option<(u64, Vec<u32>)> {
    let (&revision, rest) = sid.split_first()?;
    let (&count, rest) = rest.split_first()?;
    if revision != 1 || count > 15 || rest.len() < 6 + 4 * count as usize {
        return None;
    }
    let authority = rest[..6]
        .iter()
        .fold(0u64, |value, byte| (value << 8) | u64::from(*byte));
    let subs = rest[6..6 + 4 * count as usize]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| u32::from_le_bytes(*chunk))
        .collect();
    Some((authority, subs))
}

fn reparse_tag_name(tag: u32) -> String {
    match tag {
        0xA000_0003 => "junction".into(),
        0xA000_000C => "symbolic-link".into(),
        0x8000_001B => "app-execution-alias".into(),
        0x8000_0013 => "deduplicated-file".into(),
        0x8000_0018 | 0x9000_1018 => "container-layer".into(),
        0x8000_0017 => "wof-compressed-file".into(),
        0x8000_0023 => "af-unix-socket".into(),
        tag if tag & 0xFFFF_0FFF == 0x9000_001A => "cloud-file".into(),
        tag => format!("tag-0x{tag:08X}"),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowsInstallerDiagnostic {
    pub operation: Option<WindowsInstallOperation>,
    reason: String,
    detail: Option<String>,
    pub native_error: Option<u32>,
    pub mutations_started: Option<bool>,
}

impl WindowsInstallerDiagnostic {
    pub fn unavailable() -> Self {
        Self {
            operation: None,
            reason: "checker-unavailable".into(),
            detail: None,
            native_error: None,
            mutations_started: None,
        }
    }
}

impl InstallError {
    pub fn windows_installer_diagnostic(&self) -> WindowsInstallerDiagnostic {
        let mut result = match self {
            Self::WindowsOperation {
                operation,
                source,
                mutations_started,
            } => {
                let mut result = source.windows_installer_diagnostic();
                if result.operation.is_none() {
                    result.operation = Some(*operation);
                }
                if mutations_started.is_some() {
                    result.mutations_started = *mutations_started;
                }
                return result;
            }
            Self::WindowsActivation { stage, source } => {
                let mut result = source.windows_installer_diagnostic();
                result.reason = format!("{stage}/{}", result.reason);
                result.operation = Some(WindowsInstallOperation::Activation);
                return result;
            }
            Self::Io(error) => {
                let inner = error.get_ref();
                let failure = inner
                    .and_then(|e| e.downcast_ref::<WindowsGuardFailure>())
                    .cloned()
                    .or_else(|| {
                        inner
                            .and_then(|e| e.downcast_ref::<WindowsStorageGuard>())
                            .map(|guard| WindowsGuardFailure::new(*guard))
                    });
                WindowsInstallerDiagnostic {
                    operation: None,
                    mutations_started: None,
                    reason: failure
                        .as_ref()
                        .map(|f| f.guard.to_string())
                        .unwrap_or_else(|| io_reason(error)),
                    detail: failure.and_then(|f| f.detail),
                    native_error: error
                        .raw_os_error()
                        .filter(|code| *code != 0)
                        .map(|code| code as u32),
                }
            }
            Self::WindowsCodeSignature { status } => WindowsInstallerDiagnostic {
                operation: None,
                mutations_started: None,
                reason: "code-trust".into(),
                detail: None,
                native_error: Some(*status),
            },
            Self::CodeSignature => WindowsInstallerDiagnostic {
                operation: None,
                mutations_started: None,
                reason: "code-identity".into(),
                detail: None,
                native_error: None,
            },
            Self::Busy => WindowsInstallerDiagnostic {
                operation: None,
                mutations_started: None,
                reason: "installation-busy".into(),
                detail: None,
                native_error: None,
            },
            Self::Invalid(_) => WindowsInstallerDiagnostic {
                operation: None,
                mutations_started: None,
                reason: "invalid-installation".into(),
                detail: None,
                native_error: None,
            },
            Self::NotifyOnly(_) => WindowsInstallerDiagnostic {
                operation: None,
                mutations_started: None,
                reason: "manual-installation-required".into(),
                detail: None,
                native_error: None,
            },
        };
        // Guard future changes against unbounded/non-ASCII support output.
        result.reason.truncate(128);
        if let Some(detail) = &mut result.detail {
            detail.truncate(160);
        }
        result
    }
}

/// The report reads every raw OS code as a Windows error. Windows reports a
/// file that another process holds open, such as a running Maple Work, as a
/// sharing or lock violation, which std leaves `Uncategorized`. Name it, so the
/// support code says what the user can fix.
fn io_reason(error: &io::Error) -> String {
    const ERROR_SHARING_VIOLATION: i32 = 32;
    const ERROR_LOCK_VIOLATION: i32 = 33;
    match error.raw_os_error() {
        Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION) => "file-in-use".into(),
        _ => format!("io-{:?}", error.kind()).to_ascii_lowercase(),
    }
}

#[cfg(any(windows, test))]
fn report_header(build: &serde_json::Value, system: &str) -> String {
    let mut text = String::from("Maple Windows installer report v1\r\n");
    for key in ["version", "build", "git_revision", "source_sha", "profile"] {
        let value = build[key]
            .as_str()
            .filter(|v| {
                v.len() <= 64
                    && v.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
            })
            .unwrap_or("unknown");
        text.push_str(&format!("{key}: {value}\r\n"));
    }
    text.push_str(&format!(
        "architecture: {}\r\n{system}\r\n",
        std::env::consts::ARCH
    ));
    text
}

#[cfg(any(windows, test))]
fn render_report(
    phase: &str,
    build: &serde_json::Value,
    failure: Option<&WindowsInstallerDiagnostic>,
    elapsed: Duration,
    system: &str,
) -> String {
    let operation = failure
        .and_then(|f| f.operation)
        .map(|o| o.to_string())
        .unwrap_or_else(|| phase.to_owned());
    let reason = failure.map(|f| f.reason.as_str()).unwrap_or("success");
    let native = failure
        .and_then(|f| f.native_error)
        .map(|code| format!("0x{code:08X}"))
        .unwrap_or_else(|| "none".into());
    // The support code stays stable; the detail line says what the guard saw.
    let detail = failure
        .and_then(|f| f.detail.as_deref())
        .map(|d| format!("detail: {d}\r\n"))
        .unwrap_or_default();
    let mut text = report_header(build, system);
    text.push_str(&format!("phase: {phase}\r\noperation: {operation}\r\nreason: {reason}\r\n{detail}Windows error: {native}\r\nsupport code: MWI/{phase}/{operation}/{reason}\r\nelapsed ms: {}\r\ninstallation changes: {}\r\n", elapsed.as_millis(), if phase == "preflight" || failure.is_some_and(|f| f.mutations_started == Some(false)) { "none" } else if failure.is_none() { "phase completed" } else { "may be partial; retained for safe retry" }));
    text
}

#[cfg(windows)]
fn system_details() -> String {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation},
        System::{
            Registry::*,
            Threading::{GetCurrentProcess, OpenProcessToken},
        },
    };
    let subkey: Vec<u16> = "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion"
        .encode_utf16()
        .chain([0])
        .collect();
    let name: Vec<u16> = "CurrentBuildNumber".encode_utf16().chain([0]).collect();
    let mut value = [0u16; 32];
    let mut length = std::mem::size_of_val(&value) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            value.as_mut_ptr().cast(),
            &mut length,
        )
    };
    let build = if status == 0 {
        String::from_utf16_lossy(
            &value[..value.iter().position(|v| *v == 0).unwrap_or(value.len())],
        )
    } else {
        "unknown".into()
    };
    let build = if build.bytes().all(|b| b.is_ascii_digit()) {
        build
    } else {
        "unknown".into()
    };
    let name: Vec<u16> = "UBR".encode_utf16().chain([0]).collect();
    let mut revision = 0u32;
    let mut length = 4;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut revision as *mut u32).cast(),
            &mut length,
        )
    };
    let revision = if status == 0 {
        revision.to_string()
    } else {
        "unknown".into()
    };
    let mut token = std::ptr::null_mut();
    let mut elevated = TOKEN_ELEVATION::default();
    let mut length = 0;
    let elevation = unsafe {
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) != 0 {
            let status = GetTokenInformation(
                token,
                TokenElevation,
                (&mut elevated as *mut TOKEN_ELEVATION).cast(),
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut length,
            );
            CloseHandle(token);
            if status != 0 {
                if elevated.TokenIsElevated != 0 {
                    "yes"
                } else {
                    "no"
                }
            } else {
                "unknown"
            }
        } else {
            "unknown"
        }
    };
    format!("Windows build: {build}.{revision}\r\nelevated: {elevation}")
}

/// Best effort; reports never authorize an installation or change its status.
#[cfg(windows)]
pub fn report_windows_installation(
    phase: &str,
    build: &serde_json::Value,
    failure: Option<&WindowsInstallerDiagnostic>,
    elapsed: Duration,
    report_path: Option<&Path>,
) -> String {
    let report = render_report(phase, build, failure, elapsed, &system_details());
    let directory = std::env::var_os("LOCALAPPDATA")
        .map(|p| PathBuf::from(p).join("Maple Work Installer Logs"));
    save_report(report, phase, directory.as_deref(), report_path)
}

#[cfg(windows)]
fn save_report(
    mut report: String,
    phase: &str,
    directory: Option<&Path>,
    report_path: Option<&Path>,
) -> String {
    let phase = match phase {
        "preflight" | "preparation" | "activation" | "installation" => phase,
        _ => "unknown",
    };
    let mut saved = false;
    if let Some(directory) = directory
        && windows_bundle::windows_private_directory(directory).is_ok()
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let name = format!("install-{now}-{}-{phase}.txt", std::process::id());
        if let Ok(mut file) = windows_bundle::windows_create_private(&directory.join(&name)) {
            let text = format!(
                "{report}support log: %LOCALAPPDATA%\\Maple Work Installer Logs\\{name}\r\n"
            );
            if file
                .write_all(text.as_bytes())
                .and_then(|_| file.sync_all())
                .is_ok()
            {
                report = text;
                saved = true;
            }
        }
        prune_logs(directory);
    }
    if !saved {
        report.push_str("support log: unavailable; copy this report instead\r\n");
    }
    // Exclusive creation prevents a caller-supplied report path overwriting a
    // file or following a pre-existing leaf link. Content is fixed/public only.
    if let Some(path) = report_path
        && let Ok(mut file) = OpenOptions::new().write(true).create_new(true).open(path)
    {
        let _ = file.write_all(report.as_bytes());
    }
    report
}

#[cfg(windows)]
fn prune_logs(directory: &Path) {
    if let Ok(entries) = fs::read_dir(directory) {
        let mut logs: Vec<_> = entries
            .take(1024)
            .filter_map(|e| e.ok())
            .filter(|e| owned_log_name(&e.file_name().to_string_lossy()))
            .collect();
        logs.sort_by_key(|e| e.file_name());
        let remove = logs.len().saturating_sub(10);
        for entry in logs.into_iter().take(remove) {
            if windows_bundle::windows_open_readonly(&entry.path()).is_ok() {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

/// Started before any install mutation; checkpoints survive a crashing child.
#[cfg(windows)]
pub struct WindowsInstallerTrace {
    file: Option<File>,
    export: Option<File>,
    alias: Option<String>,
    phase: &'static str,
    build: serde_json::Value,
    system: String,
    started: std::time::Instant,
    written: usize,
}
#[cfg(windows)]
impl WindowsInstallerTrace {
    pub fn begin(
        phase: &'static str,
        build: &serde_json::Value,
        report_path: Option<&Path>,
    ) -> Self {
        let phase = match phase {
            "preflight" | "preparation" | "activation" | "installation" => phase,
            _ => "unknown",
        };
        let mut trace = Self {
            file: None,
            export: report_path
                .and_then(|p| OpenOptions::new().write(true).create_new(true).open(p).ok()),
            alias: None,
            phase,
            build: build.clone(),
            system: system_details(),
            started: std::time::Instant::now(),
            written: 0,
        };
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            let directory = PathBuf::from(local).join("Maple Work Installer Logs");
            if windows_bundle::windows_private_directory(&directory).is_ok() {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos();
                let name = format!("install-{now}-{}-{phase}.txt", std::process::id());
                trace.file = windows_bundle::windows_create_private(&directory.join(&name)).ok();
                if trace.file.is_some() {
                    trace.alias =
                        Some(format!("%LOCALAPPDATA%\\Maple Work Installer Logs\\{name}"));
                }
                prune_logs(&directory);
            }
        }
        let header = format!(
            "{}phase: {phase}\r\nstate: running; checkpoints are not a final result\r\nsupport log: {}\r\n",
            report_header(build, &trace.system),
            trace.alias.as_deref().unwrap_or("unavailable")
        );
        trace.write_checkpoint(&header);
        trace
    }
    fn write_checkpoint(&mut self, text: &str) {
        if self.written + text.len() > 2048 {
            return;
        }
        self.written += text.len();
        for file in [&mut self.file, &mut self.export].into_iter().flatten() {
            let _ = file
                .write_all(text.as_bytes())
                .and_then(|_| file.sync_all());
        }
    }
    pub fn record(&mut self, operation: WindowsInstallOperation, mutations_started: bool) {
        self.write_checkpoint(&format!(
            "checkpoint: {operation}; mutations started: {mutations_started}; elapsed ms: {}\r\n",
            self.started.elapsed().as_millis()
        ));
    }
    pub fn finish(mut self, failure: Option<&WindowsInstallerDiagnostic>) -> String {
        let mut report = render_report(
            self.phase,
            &self.build,
            failure,
            self.started.elapsed(),
            &self.system,
        );
        if let (Some(file), Some(alias)) = (&mut self.file, &self.alias) {
            let text = format!("{report}support log: {alias}\r\n");
            if file
                .write_all(text.as_bytes())
                .and_then(|_| file.sync_all())
                .is_ok()
            {
                report = text;
            } else {
                report.push_str("support log: unavailable; copy this report instead\r\n");
            }
        } else {
            report.push_str("support log: unavailable; copy this report instead\r\n");
        }
        // Rewrite only our exclusively created, still-held export handle. On a
        // crash it contains checkpoints; normal exit leaves a compact dialog.
        if let Some(file) = &mut self.export {
            use std::io::{Seek, SeekFrom};
            let _ = file
                .set_len(0)
                .and_then(|_| file.seek(SeekFrom::Start(0)))
                .and_then(|_| file.write_all(report.as_bytes()));
        }
        report
    }
}

#[cfg(windows)]
fn owned_log_name(name: &str) -> bool {
    let Some(name) = name
        .strip_prefix("install-")
        .and_then(|n| n.strip_suffix(".txt"))
    else {
        return false;
    };
    let parts: Vec<_> = name.split('-').collect();
    parts.len() == 3
        && parts[0].parse::<u128>().is_ok()
        && parts[1].parse::<u32>().is_ok_and(|id| id != 0)
        && matches!(
            parts[2],
            "preflight" | "preparation" | "activation" | "installation" | "unknown"
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    #[test]
    fn report_survives_log_failure_preserves_existing_exports_and_bounds_retention() {
        let root = tempfile::tempdir().unwrap();
        let failed_directory = root.path().join("not-a-directory");
        fs::write(&failed_directory, b"preserve").unwrap();
        let export = root.path().join("export.txt");
        fs::write(&export, b"preserve export").unwrap();
        let text = save_report(
            "fixed diagnostic\r\n".into(),
            "preflight",
            Some(&failed_directory),
            Some(&export),
        );
        assert!(text.contains("copy this report"));
        assert_eq!(fs::read(&export).unwrap(), b"preserve export");
        let directory = root.path().join("logs");
        windows_bundle::windows_private_directory(&directory).unwrap();
        fs::write(
            directory.join("install-user-notes.txt"),
            b"keep unrelated file",
        )
        .unwrap();
        for _ in 0..12 {
            save_report(
                "fixed diagnostic\r\n".into(),
                "preflight",
                Some(&directory),
                None,
            );
        }
        assert_eq!(
            fs::read_dir(&directory)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| owned_log_name(&e.file_name().to_string_lossy()))
                .count(),
            10
        );
        assert_eq!(
            fs::read(directory.join("install-user-notes.txt")).unwrap(),
            b"keep unrelated file"
        );
        let export = root.path().join("new-export.txt");
        let text = save_report(
            "fixed diagnostic\r\n".into(),
            "preflight",
            Some(&failed_directory),
            Some(&export),
        );
        assert_eq!(fs::read_to_string(export).unwrap(), text);
    }

    #[test]
    fn installer_report_preserves_guard_context_and_native_status_without_private_text() {
        let secret = "CANARY-private-path-and-SID";
        for error in [
            InstallError::Io(io::Error::new(
                io::ErrorKind::PermissionDenied,
                WindowsStorageGuard::ForeignWriter,
            )),
            InstallError::Io(io::Error::other(secret)),
            InstallError::Invalid(secret),
            InstallError::Io(io::Error::from_raw_os_error(32)),
        ] {
            let code = error.windows_installer_exit_code();
            let error = error.during(WindowsInstallOperation::Parent);
            assert_eq!(error.windows_installer_exit_code(), code);
            let detail = error.windows_installer_diagnostic();
            assert_eq!(detail.operation, Some(WindowsInstallOperation::Parent));
            let report = render_report(
                "preflight",
                &serde_json::json!({"version":"0.0.18","git_revision":"568f790","profile":"prod","api_url":secret}),
                Some(&detail),
                Duration::from_millis(5),
                "Windows build: fixture",
            );
            assert!(!report.contains(secret));
            assert!(report.contains("installation changes: none"));
            assert!(report.len() < 2048);
        }
        let error = InstallError::Io(io::Error::from_raw_os_error(32));
        assert_eq!(error.windows_installer_diagnostic().native_error, Some(32));
    }

    #[test]
    fn installer_report_names_a_file_held_open_by_a_running_app() {
        // Seen on 0.0.19: upgrading while the old version ran was refused at
        // the generation guards with Windows error 32, reported as io-uncategorized.
        for code in [32, 33] {
            let error = InstallError::WindowsActivation {
                stage: WindowsActivationStage::GenerationGuards,
                source: Box::new(InstallError::Io(io::Error::from_raw_os_error(code))),
            };
            let detail = error.windows_installer_diagnostic();
            assert_eq!(detail.reason, "generation-guards/file-in-use");
            assert_eq!(detail.native_error, Some(code as u32));
            let report = render_report(
                "installation",
                &serde_json::json!({"version":"0.0.19","git_revision":"0713188","profile":"prod"}),
                Some(&detail),
                Duration::from_millis(5),
                "Windows build: fixture",
            );
            assert!(report.contains(
                "support code: MWI/installation/activate-installation/generation-guards/file-in-use"
            ));
            assert!(report.contains(&format!("Windows error: 0x{code:08X}")));
        }
        // Other native errors keep their std category.
        let denied =
            InstallError::Io(io::Error::from_raw_os_error(5)).windows_installer_diagnostic();
        assert_ne!(denied.reason, "file-in-use");
        assert!(denied.reason.starts_with("io-"));
    }

    fn sid(authority: u64, subs: &[u32]) -> Vec<u8> {
        let mut bytes = vec![1, subs.len() as u8];
        bytes.extend_from_slice(&authority.to_be_bytes()[2..]);
        for sub in subs {
            bytes.extend_from_slice(&sub.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn sid_classes_name_the_kind_of_account_without_its_identity() {
        let user = sid(5, &[21, 111, 222, 333, 1001]);
        let user = Some(user.as_slice());
        for (subject, expected) in [
            (sid(1, &[0]), "everyone"),
            (sid(5, &[11]), "authenticated-users"),
            (sid(5, &[32, 545]), "users"),
            (sid(5, &[32, 544]), "administrators"),
            (sid(5, &[32, 999]), "builtin-group-rid-999"),
            (sid(5, &[21, 111, 222, 333, 1001]), "current-user"),
            (
                sid(5, &[21, 111, 222, 333, 1002]),
                "same-domain-account-rid-1002",
            ),
            (
                sid(5, &[21, 111, 222, 333, 512]),
                "same-domain-account-rid-512-domain-admins",
            ),
            // A profile copied from another computer keeps its old owner.
            (
                sid(5, &[21, 7, 8, 9, 1001]),
                "other-domain-account-rid-1001",
            ),
            (sid(12, &[1, 5, 6, 7, 8]), "entra-id-account"),
            (sid(15, &[2, 1]), "all-application-packages"),
            (sid(15, &[3, 1024, 1, 2]), "capability"),
            (sid(5, &[80, 1, 2, 3, 4, 5]), "service-account"),
            (sid(9, &[4]), "authority-9-subauthority-4"),
        ] {
            assert_eq!(sid_class(&subject, user), expected);
        }
        assert_eq!(
            sid_class(&sid(5, &[21, 7, 8, 9, 1001]), None),
            "domain-account-rid-1001"
        );
        for malformed in [&[][..], &[1], &[1, 2, 0, 0, 0, 0, 0, 5, 1, 0, 0, 0]] {
            assert_eq!(sid_class(malformed, user), "malformed-sid");
        }
    }

    #[test]
    fn installer_report_says_which_account_could_write_without_naming_it() {
        let user = sid(5, &[21, 111, 222, 333, 1001]);
        let cases = [
            (
                WindowsGuardFailure::foreign_writer(
                    &sid(5, &[11]),
                    Some(&user),
                    0x0013_01BF,
                    true,
                    false,
                ),
                "detail: account: authenticated-users; access: 0x001301BF; inherited: yes; object: file",
            ),
            (
                WindowsGuardFailure::owner(&sid(5, &[21, 7, 8, 9, 1001]), Some(&user), true),
                "detail: owner: other-domain-account-rid-1001; object: folder",
            ),
            (
                WindowsGuardFailure::reparse(Some(0xA000_0003), Some((3, 7))),
                "detail: link: junction; path component: 3 of 7",
            ),
        ];
        for (failure, line) in cases {
            let label = failure.guard.to_string();
            let error = InstallError::Io(io::Error::new(io::ErrorKind::PermissionDenied, failure))
                .during(WindowsInstallOperation::Checker);
            let detail = error.windows_installer_diagnostic();
            assert_eq!(detail.reason, label);
            let report = render_report(
                "installation",
                &serde_json::json!({"version":"0.0.21","git_revision":"abcdef0","profile":"prod"}),
                Some(&detail),
                Duration::from_millis(5),
                "Windows build: fixture",
            );
            assert!(report.contains(&format!("{line}\r\n")), "{report}");
            // The support code users paste stays the same as before.
            assert!(report.contains(&format!(
                "support code: MWI/installation/verify-checker-image/{label}\r\n"
            )));
            assert!(!report.contains("111") && !report.contains("S-1-"));
        }
    }
}
