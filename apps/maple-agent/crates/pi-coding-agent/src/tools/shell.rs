//! Which shell runs commands, the environment it gets, and stopping what it started.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// How the command reaches the shell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandTransport {
    /// As the last argument.
    Argv,
    /// On standard input, for shells that cannot take it as an argument.
    Stdin,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellConfig {
    pub shell: PathBuf,
    pub args: Vec<String>,
    pub transport: CommandTransport,
}

impl ShellConfig {
    fn bash(shell: PathBuf) -> Self {
        // The old WSL `bash.exe` in System32 does not take `-c` reliably.
        if is_legacy_wsl_bash(&shell) {
            Self {
                shell,
                args: vec!["-s".to_string()],
                transport: CommandTransport::Stdin,
            }
        } else {
            Self {
                shell,
                args: vec!["-c".to_string()],
                transport: CommandTransport::Argv,
            }
        }
    }
}

fn is_legacy_wsl_bash(path: &Path) -> bool {
    is_legacy_wsl_bash_in(path, system_root().as_deref())
}

/// Whether `path` is the WSL launcher in the Windows folder `system_root`, or, when that
/// is unknown, in the conventional one on any drive.
fn is_legacy_wsl_bash_in(path: &Path, system_root: Option<&str>) -> bool {
    let normalize = |path: &str| -> String {
        let mut normalized = String::with_capacity(path.len());
        for ch in path.chars() {
            let ch = if ch == '/' { '\\' } else { ch };
            // Repeated separators are one, as Node's `win32.normalize` reads them.
            if !(ch == '\\' && normalized.ends_with('\\')) {
                normalized.push(ch.to_ascii_lowercase());
            }
        }
        normalized
    };
    let normalized = normalize(&path.to_string_lossy());
    if let Some(root) = system_root {
        let root = normalize(root);
        let bytes = root.as_bytes();
        // Only a drive path names the Windows folder.
        if !(bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && &bytes[1..3] == b":\\") {
            return false;
        }
        let root = root.trim_end_matches('\\');
        return ["system32", "sysnative"]
            .iter()
            .any(|dir| normalized == format!("{root}\\{dir}\\bash.exe"));
    }
    let Some((drive, rest)) = normalized.split_once(':') else {
        return false;
    };
    drive.len() == 1
        && drive.chars().all(|c| c.is_ascii_alphabetic())
        && matches!(
            rest,
            "\\windows\\system32\\bash.exe" | "\\windows\\sysnative\\bash.exe"
        )
}

/// The Windows folder, when the environment names one.
fn system_root() -> Option<String> {
    std::env::var("SystemRoot")
        .ok()
        .filter(|root| !root.is_empty())
}

/// Whether `path` is a Microsoft Store alias: a link that runs the app, which cannot be
/// opened through as an ordinary file or link can. Only Windows has them.
fn is_store_alias(path: &Path) -> bool {
    cfg!(windows) && std::fs::metadata(path).is_err() && std::fs::symlink_metadata(path).is_ok()
}

/// Whether a shell exists at `path`, as Pi checks it, Microsoft Store aliases included.
fn shell_path_exists(path: &Path) -> bool {
    path.exists() || is_store_alias(path)
}

/// The first file named `name` on PATH, a Microsoft Store alias included.
pub(crate) fn find_executable_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file() || is_store_alias(candidate))
}

/// The shell for `bash`: `custom_path` when given, else on Windows Git Bash, then
/// `bash.exe` on PATH; elsewhere `/bin/bash`, then `bash` on PATH, then `sh`.
pub fn shell_config(custom_path: Option<&Path>) -> Result<ShellConfig, String> {
    if let Some(path) = custom_path {
        if shell_path_exists(path) {
            return Ok(ShellConfig::bash(path.to_path_buf()));
        }
        return Err(format!("Custom shell path not found: {}", path.display()));
    }
    if cfg!(windows) {
        let mut searched = Vec::new();
        for variable in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(dir) = std::env::var_os(variable) {
                searched.push(PathBuf::from(dir).join("Git").join("bin").join("bash.exe"));
            }
        }
        if let Some(found) = searched.iter().find(|path| shell_path_exists(path)) {
            return Ok(ShellConfig::bash(found.clone()));
        }
        if let Some(found) = find_executable_on_path("bash.exe") {
            return Ok(ShellConfig::bash(found));
        }
        let searched = searched
            .iter()
            .map(|path| format!("  {}", path.display()))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(format!(
            "No bash shell found. Options:\n  1. Install Git for Windows: https://git-scm.com/download/win\n  2. Add your bash to PATH (Cygwin, MSYS2, etc.)\n  3. Set shellPath in settings.json\n\nSearched Git Bash in:\n{searched}"
        ));
    }
    let system_bash = Path::new("/bin/bash");
    if shell_path_exists(system_bash) {
        return Ok(ShellConfig::bash(system_bash.to_path_buf()));
    }
    if let Some(found) = find_executable_on_path("bash") {
        return Ok(ShellConfig::bash(found));
    }
    Ok(ShellConfig {
        shell: PathBuf::from("sh"),
        args: vec!["-c".to_string()],
        transport: CommandTransport::Argv,
    })
}

pub const POWERSHELL_ARGS: [&str; 5] = [
    "-NoProfile",
    "-NonInteractive",
    "-ExecutionPolicy",
    "Bypass",
    "-Command",
];

/// PowerShell on Windows, PowerShell 7 when it is installed. Each is also looked for in
/// its installation folder, which need not be on PATH or on drive C.
pub fn powershell_config() -> Result<ShellConfig, String> {
    if !cfg!(windows) {
        return Err("The powershell tool is only available on Windows.".to_string());
    }
    let installed = |dir: Option<PathBuf>, path: &[&str]| {
        dir.map(|dir| path.iter().fold(dir, |dir, part| dir.join(part)))
            .filter(|path| shell_path_exists(path))
    };
    let program_files = std::env::var_os("ProgramFiles").map(PathBuf::from);
    let system_root = system_root().map(PathBuf::from);
    let shell = find_executable_on_path("pwsh.exe")
        .or_else(|| installed(program_files, &["PowerShell", "7", "pwsh.exe"]))
        .or_else(|| find_executable_on_path("powershell.exe"))
        .or_else(|| {
            installed(
                system_root,
                &["System32", "WindowsPowerShell", "v1.0", "powershell.exe"],
            )
        })
        .ok_or_else(|| {
            "No PowerShell executable found. Install PowerShell or add powershell.exe/pwsh.exe to PATH."
                .to_string()
        })?;
    Ok(ShellConfig {
        shell,
        args: POWERSHELL_ARGS.iter().map(|arg| arg.to_string()).collect(),
        transport: CommandTransport::Argv,
    })
}

/// The environment commands start from: this process's, with variables that are not
/// valid UTF-8 left out.
pub fn shell_env() -> BTreeMap<String, String> {
    std::env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .collect()
}

/// Set `key` in `env`. On Windows, where names ignore case, it replaces `Path` when
/// setting `PATH`.
pub fn set_env_var(env: &mut BTreeMap<String, String>, key: &str, value: impl Into<String>) {
    if cfg!(windows) {
        env.retain(|existing, _| !existing.eq_ignore_ascii_case(key));
    }
    env.insert(key.to_string(), value.into());
}

/// The value of `key` in `env`, ignoring case on Windows.
pub fn env_var<'a>(env: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    if cfg!(windows) {
        env.iter()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(key))
            .map(|(_, value)| value.as_str())
    } else {
        env.get(key).map(String::as_str)
    }
}

fn tracked() -> &'static Mutex<HashSet<u32>> {
    static TRACKED: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();
    TRACKED.get_or_init(Default::default)
}

pub(crate) fn track_child(pid: u32) {
    tracked()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(pid);
}

pub(crate) fn untrack_child(pid: u32) {
    tracked()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(&pid);
}

/// Stop every command still running, with whatever it started. Call it when the
/// process is about to exit, as Pi does on SIGTERM and SIGHUP.
pub fn kill_tracked_children() {
    let pids: Vec<u32> = tracked()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .drain()
        .collect();
    for pid in pids {
        kill_process_tree(pid);
    }
}

/// Kill a command and everything it started: its process group on Unix, its process
/// tree on Windows.
pub fn kill_process_tree(pid: u32) {
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return;
        };
        // SAFETY: kill only sends a signal; a negative pid names the process group.
        if unsafe { libc::kill(-pid, libc::SIGKILL) } != 0 {
            // SAFETY: as above, for the process alone.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        // The trusted System32 copy, so cleanup does not depend on PATH.
        let system_root = system_root().unwrap_or_else(|| "C:\\Windows".into());
        let taskkill = PathBuf::from(system_root)
            .join("System32")
            .join("taskkill.exe");
        let _ = std::process::Command::new(taskkill)
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_wsl_bash_reads_the_command_from_stdin() {
        let config = ShellConfig::bash(PathBuf::from("C:\\Windows\\System32\\bash.exe"));
        assert_eq!(config.args, ["-s"]);
        assert_eq!(config.transport, CommandTransport::Stdin);
        let git_bash = ShellConfig::bash(PathBuf::from("C:\\Program Files\\Git\\bin\\bash.exe"));
        assert_eq!(git_bash.args, ["-c"]);
        assert_eq!(git_bash.transport, CommandTransport::Argv);
    }

    #[test]
    fn the_wsl_launcher_is_found_in_the_windows_folder() {
        let launcher =
            |path: &str, root: Option<&str>| is_legacy_wsl_bash_in(Path::new(path), root);
        // Wherever Windows is installed, when the environment says where.
        assert!(launcher("D:\\Win\\System32\\bash.exe", Some("D:\\Win")));
        assert!(launcher("d:/win//sysnative/BASH.EXE", Some("D:\\Win\\")));
        assert!(!launcher(
            "C:\\Windows\\System32\\bash.exe",
            Some("D:\\Win")
        ));
        // A Windows folder that is no drive path names no launcher.
        assert!(!launcher(
            "C:\\Windows\\System32\\bash.exe",
            Some("Windows")
        ));
        // Otherwise the conventional folder on any drive.
        assert!(launcher("E:\\Windows\\System32\\bash.exe", None));
        assert!(!launcher("C:\\Program Files\\Git\\bin\\bash.exe", None));
    }

    #[test]
    fn a_missing_custom_shell_is_an_error() {
        assert_eq!(
            shell_config(Some(Path::new("/custom/bash"))).unwrap_err(),
            "Custom shell path not found: /custom/bash"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_finds_a_shell() {
        let config = shell_config(None).unwrap();
        assert_eq!(config.args, ["-c"]);
        assert!(powershell_config().is_err());
    }

    #[test]
    fn setting_a_variable_replaces_its_other_spellings_on_windows() {
        let mut env = BTreeMap::from([("Path".to_string(), "old".to_string())]);
        set_env_var(&mut env, "PATH", "new");
        assert_eq!(env_var(&env, "PATH"), Some("new"));
        if cfg!(windows) {
            assert_eq!(env.len(), 1);
        }
    }
}
