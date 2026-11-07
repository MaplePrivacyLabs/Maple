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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowsInstallerDiagnostic {
    pub operation: Option<WindowsInstallOperation>,
    pub reason: String,
    pub native_error: Option<u32>,
}

impl InstallError {
    pub fn windows_installer_diagnostic(&self) -> WindowsInstallerDiagnostic {
        let mut result = match self {
            Self::WindowsOperation { operation, source } => {
                let mut result = source.windows_installer_diagnostic();
                if result.operation.is_none() {
                    result.operation = Some(*operation);
                }
                return result;
            }
            Self::WindowsActivation { stage, source } => {
                let mut result = source.windows_installer_diagnostic();
                result.reason = format!("{stage}/{}", result.reason);
                result.operation = Some(WindowsInstallOperation::Activation);
                return result;
            }
            Self::Io(error) => WindowsInstallerDiagnostic {
                operation: None,
                reason: error
                    .get_ref()
                    .and_then(|e| e.downcast_ref::<WindowsStorageGuard>())
                    .map(ToString::to_string)
                    .unwrap_or_else(|| format!("io-{:?}", error.kind()).to_ascii_lowercase()),
                native_error: error
                    .raw_os_error()
                    .filter(|code| *code != 0)
                    .map(|code| code as u32),
            },
            Self::WindowsCodeSignature { status } => WindowsInstallerDiagnostic {
                operation: None,
                reason: "code-trust".into(),
                native_error: Some(*status),
            },
            Self::CodeSignature => WindowsInstallerDiagnostic {
                operation: None,
                reason: "code-identity".into(),
                native_error: None,
            },
            Self::Busy => WindowsInstallerDiagnostic {
                operation: None,
                reason: "installation-busy".into(),
                native_error: None,
            },
            Self::Invalid(_) => WindowsInstallerDiagnostic {
                operation: None,
                reason: "invalid-installation".into(),
                native_error: None,
            },
            Self::NotifyOnly(_) => WindowsInstallerDiagnostic {
                operation: None,
                reason: "manual-installation-required".into(),
                native_error: None,
            },
        };
        // Guard future changes against unbounded/non-ASCII support output.
        result.reason.truncate(128);
        result
    }
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
    text.push_str(&format!("architecture: {}\r\n{system}\r\nphase: {phase}\r\noperation: {operation}\r\nreason: {reason}\r\nWindows error: {native}\r\nsupport code: MWI/{phase}/{operation}/{reason}\r\nelapsed ms: {}\r\ninstallation changes: {}\r\n", std::env::consts::ARCH, elapsed.as_millis(), if phase == "preflight" { "none" } else if failure.is_none() { "phase completed" } else { "may be partial; retained for safe retry" }));
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
}
