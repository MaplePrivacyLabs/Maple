//! Credential-free Windows bootstrap, restart and installer error UI.
//! The installer owns destination/profile/code validation; this module only
//! delegates to its selected generation or waits for an exact retiring process.

use std::{
    ffi::OsString,
    os::windows::process::CommandExt,
    path::Path,
    process::{Command, Stdio},
};

use windows_sys::Win32::{
    Foundation::{CloseHandle, FILETIME, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::Threading::{
        CREATE_NO_WINDOW, GetCurrentProcess, GetProcessTimes, OpenProcess,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    },
    UI::WindowsAndMessaging::{
        IDOK, MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MB_TASKMODAL, MessageBoxW,
    },
};

/// UI acknowledgement only, never a successful installation or activation.
pub(crate) const INSTALLER_ERROR_SHOWN: i32 = 0x4DFF_0001;

#[derive(Clone, Copy)]
pub(crate) enum InstallerPhase {
    Preflight,
    Preparation,
    Activation,
}

fn installer_error_message(phase: InstallerPhase, error: &str) -> String {
    let phase = match phase {
        InstallerPhase::Preflight => "preflight",
        InstallerPhase::Preparation => "preparation",
        InstallerPhase::Activation => "activation",
    };
    // Native failures contain fixed validation labels and OS errors, never
    // account/store state. Bound even unexpected text before handing it to UI.
    let detail: String = error
        .chars()
        .filter(|character| !character.is_control())
        .take(2048)
        .collect();
    format!(
        "Installation stopped during {phase}.\r\n\r\n{detail}\r\n\r\nSend a screenshot of this message to Maple support."
    )
}

/// Called only for an explicitly interactive installer failure, before any
/// application, account or store initialization. A failed dialog leaves NSIS's
/// own fallback error visible instead of falsely acknowledging presentation.
pub(crate) fn show_installer_error(display_name: &str, phase: InstallerPhase, error: &str) -> bool {
    let text: Vec<u16> = installer_error_message(phase, error)
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let title: Vec<u16> = format!("{display_name} Setup")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    // SAFETY: both NUL-terminated UTF-16 buffers remain live until the modal
    // call returns. No borrowed parent-window handle or mutable data is used.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND | MB_TASKMODAL,
        ) == IDOK
    }
}

struct ProcessHandle(HANDLE);
impl Drop for ProcessHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn creation_time(handle: HANDLE) -> Result<u64, String> {
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    if unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
        return Err("Could not identify the retiring Maple process".into());
    }
    Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

fn restart_identity(pid: u32, created: u64) -> String {
    format!("{pid}:{created}")
}

fn parse_restart_identity(value: &str) -> Result<(u32, u64), String> {
    let (pid, created) = value
        .split_once(':')
        .filter(|_| value.len() <= 32)
        .ok_or("Invalid Windows restart identity")?;
    let pid: u32 = pid
        .parse()
        .map_err(|_| "Invalid Windows restart identity")?;
    let created: u64 = created
        .parse()
        .map_err(|_| "Invalid Windows restart identity")?;
    if pid == 0 || created == 0 || restart_identity(pid, created) != value {
        return Err("Invalid Windows restart identity".into());
    }
    Ok((pid, created))
}

/// Spawn the trusted stable bootstrap before retiring the current desktop.
/// A failed spawn leaves the committed update available for a deliberate retry.
pub(crate) fn schedule_restart(bootstrap: &Path, profile: &str) -> Result<(), String> {
    // Revalidate the actual bootstrap/selected generation before executing it.
    if maple_agent_updater::installer::resolve_windows_launcher(bootstrap, profile)
        .map_err(|_| "The Windows update launcher could not be verified")?
        .is_none()
    {
        return Err("The Windows update launcher is not an installed bootstrap".into());
    }
    let created = creation_time(unsafe { GetCurrentProcess() })?;
    Command::new(bootstrap)
        .arg("--internal-windows-restart")
        .arg(restart_identity(std::process::id(), created))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|_| "Could not start the Windows restart helper")?;
    Ok(())
}

/// This early mode opens no application/store/backend state. PID creation time
/// prevents a reused PID from becoming restart authority.
pub(crate) fn run_restart(identity: &str, profile: &str) -> Result<(), String> {
    let (pid, expected_creation) = parse_restart_identity(identity)?;
    let process = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        )
    };
    if !process.is_null() {
        let process = ProcessHandle(process);
        if creation_time(process.0)? != expected_creation {
            return Err("The retiring Windows process identity changed".into());
        }
        match unsafe { WaitForSingleObject(process.0, 120_000) } {
            WAIT_OBJECT_0 => {}
            WAIT_TIMEOUT => {
                return Err(
                    "Maple has not finished shutting down; reopen it after quitting".into(),
                );
            }
            _ => return Err("Could not wait for Maple to shut down".into()),
        }
    } else {
        let error = std::io::Error::last_os_error();
        // ERROR_INVALID_PARAMETER: the PID no longer exists. Access denial and
        // other errors never mean that the old process has finished.
        if error.raw_os_error() != Some(87) {
            return Err("Could not identify the retiring Windows process".into());
        }
    }
    let bootstrap =
        std::env::current_exe().map_err(|_| "Could not locate the Windows bootstrap")?;
    let target = maple_agent_updater::installer::resolve_windows_launcher(&bootstrap, profile)
        .map_err(|_| "The Windows update launcher could not be verified")?
        .ok_or("The restart helper is not the stable Windows bootstrap")?;
    Command::new(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|_| "Could not launch the updated Windows application")?;
    Ok(())
}

fn is_internal_entrypoint(arguments: &[OsString]) -> bool {
    arguments.first().is_some_and(|argument| {
        [
            "--internal-windows-installer-preflight",
            "--internal-windows-installer-prepare",
            "--internal-windows-installer-activate",
            "--internal-windows-uninstall-cleanup",
            "--internal-windows-restart",
        ]
        .iter()
        .any(|reserved| argument == *reserved)
    })
}

/// Delegate before Clap parses the application's CLI. The stable bootstrap may
/// be older than the selected generation and must not freeze its CLI grammar.
/// Only the exact current native helper entrypoints remain owned by this binary;
/// main parses and validates those arguments before executing their operation.
pub(crate) fn delegate_before_parse(profile: &str) -> Result<Option<i32>, String> {
    let arguments: Vec<OsString> = std::env::args_os().skip(1).collect();
    if is_internal_entrypoint(&arguments) {
        return Ok(None);
    }
    let executable = std::env::current_exe().map_err(|_| "Could not locate Maple")?;
    let Some(target) =
        maple_agent_updater::installer::resolve_windows_launcher(&executable, profile)
            .map_err(|_| "The installed Windows application could not be verified")?
    else {
        return Ok(None);
    };
    forward_verified_target(&target, &arguments, !arguments.is_empty()).map(Some)
}

/// This private primitive receives only a target already admitted by the native
/// installer. Native argv/stdio/status never pass through a shell command string.
fn forward_verified_target(
    target: &Path,
    arguments: &[OsString],
    wait_for_exit: bool,
) -> Result<i32, String> {
    let mut command = Command::new(target);
    command.args(arguments);
    if !wait_for_exit {
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command
        .spawn()
        .map_err(|_| "Could not start the installed Maple application")?;
    Ok(if wait_for_exit {
        child
            .wait()
            .map_err(|_| "Could not wait for the Maple command")?
            .code()
            .unwrap_or(1)
    } else {
        0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installer_error_text_keeps_native_failure_and_distinct_stage() {
        for (phase, label) in [
            (InstallerPhase::Preflight, "preflight"),
            (InstallerPhase::Preparation, "preparation"),
            (InstallerPhase::Activation, "activation"),
        ] {
            let message = installer_error_message(
                phase,
                "Windows could not verify Maple's code signature (WinTrust error 0x800B0109)",
            );
            assert!(message.starts_with(&format!("Installation stopped during {label}.")));
            assert!(message.contains("WinTrust error 0x800B0109"));
            assert!(message.contains("Maple support"));
        }
    }

    #[test]
    fn installer_error_text_bounds_and_sanitizes_unexpected_controls() {
        let error = format!("native\0error\n{}", "x".repeat(4096));
        let message = installer_error_message(InstallerPhase::Preflight, &error);
        assert!(message.contains("nativeerror"));
        assert!(!message.contains('\0'));
        assert!(message.len() < 2300);
    }

    #[test]
    fn windows_restart_identity_is_canonical_and_bounded() {
        assert_eq!(parse_restart_identity("42:12345"), Ok((42, 12345)));
        for value in [
            "0:123",
            "1:0",
            "01:2",
            "1:02",
            "1:2:3",
            "1:$(echo)",
            "4294967296:1",
            "1:18446744073709551616",
        ] {
            assert!(parse_restart_identity(value).is_err());
        }
    }

    #[test]
    fn windows_current_process_has_stable_creation_identity() {
        let handle = unsafe { GetCurrentProcess() };
        let created = creation_time(handle).unwrap();
        assert!(created > 0);
        assert_eq!(creation_time(handle).unwrap(), created);
    }

    #[test]
    fn windows_bootstrap_only_reserves_exact_internal_entrypoints() {
        assert!(is_internal_entrypoint(&[
            "--internal-windows-installer-prepare".into(),
            "C:\\Maple stage & data".into(),
            "0.0.10-702".into(),
        ]));
        assert!(is_internal_entrypoint(&[
            "--internal-windows-restart".into(),
            "42:12345".into(),
        ]));
        for arguments in [
            vec![],
            vec!["--help".into()],
            vec!["--version".into()],
            vec!["--future-generation-option".into()],
            vec!["--internal-windows-restart=42:12345".into()],
            vec!["--internal-windows-restart-other".into()],
            vec!["--".into(), "--internal-windows-restart".into()],
        ] {
            assert!(!is_internal_entrypoint(&arguments));
        }
    }

    #[test]
    fn windows_forward_preserves_native_argv_and_exit_status() {
        use std::os::windows::ffi::OsStrExt;

        let scratch = tempfile::tempdir().unwrap();
        let directory = scratch.path().join("Maple stage & Ω");
        std::fs::create_dir(&directory).unwrap();
        let source = directory.join("argv-fixture.rs");
        let target = directory.join("forward ' $ & ; fixture.exe");
        let output = directory.join("native-argv.bin");
        // The fixture records the actual argv delivered by Windows. It uses
        // only std, and never installs a certificate or bypasses admission of a
        // production target. Ordinary fixture execution is confined to cfg(test).
        std::fs::write(
            &source,
            r#"
use std::{fs::File, io::Write, os::windows::ffi::OsStrExt};
fn main() {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    let mut output = File::create(&arguments[0]).unwrap();
    output.write_all(&(arguments.len() as u32).to_le_bytes()).unwrap();
    for argument in &arguments {
        let units: Vec<_> = argument.encode_wide().collect();
        output.write_all(&(units.len() as u32).to_le_bytes()).unwrap();
        for unit in units {
            output.write_all(&unit.to_le_bytes()).unwrap();
        }
    }
    output.sync_all().unwrap();
    std::process::exit(37);
}
"#,
        )
        .unwrap();
        let mut compiler = Command::new("rustc");
        compiler.args([
            "--edition=2024",
            "--crate-name",
            "windows_bootstrap_argv_fixture",
            "--target",
            "x86_64-pc-windows-msvc",
            "-C",
            "debuginfo=0",
        ]);
        // Reuse the CI's selected native linker instead of rediscovering it.
        if let Some(linker) = std::env::var_os("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER") {
            let mut option = OsString::from("linker=");
            option.push(linker);
            compiler.arg("-C").arg(option);
        }
        let compiled = compiler
            .arg(&source)
            .arg("-o")
            .arg(&target)
            .output()
            .unwrap();
        assert!(
            compiled.status.success(),
            "native argv fixture compile failed: {}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        let arguments: Vec<OsString> = vec![
            output.clone().into_os_string(),
            "".into(),
            "space argument".into(),
            "quotes \"inside\"".into(),
            r"trailing\path\\".into(),
            r#"backslash quote \\" end"#.into(),
            "a&b;c$d`e".into(),
            "'single quotes'".into(),
            "世界 🪴".into(),
            "--future-option=with space".into(),
        ];
        assert_eq!(
            forward_verified_target(&target, &arguments, true).unwrap(),
            37
        );
        let mut expected = (arguments.len() as u32).to_le_bytes().to_vec();
        for argument in arguments {
            let units: Vec<_> = argument.encode_wide().collect();
            expected.extend((units.len() as u32).to_le_bytes());
            for unit in units {
                expected.extend(unit.to_le_bytes());
            }
        }
        assert_eq!(std::fs::read(output).unwrap(), expected);
    }
}
