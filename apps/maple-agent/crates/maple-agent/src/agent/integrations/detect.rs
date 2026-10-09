//! What Maple finds out about the Codex and Claude Code command lines on
//! this device: where they are, their version, and whether they are signed
//! in. Nothing here changes an installation.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use serde::Deserialize;

use crate::agent::bounded_process::read_bounded_stdout;

pub(super) const CODEX_SIGN_IN_HINT: &str =
    "Codex is not signed in. Run `codex login` in a terminal, then try again.";
pub(super) const CLAUDE_SIGN_IN_HINT: &str =
    "Claude Code is not signed in. Run `claude auth login` in a terminal, then try again.";

/// The oldest Codex whose app-server speaks the methods Maple uses.
const CODEX_MIN_VERSION: (u64, u64, u64) = (0, 143, 0);
/// How long one probe may run.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// How long ending a probe may take.
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_VERSION_BYTES: usize = 4 * 1024;
const MAX_AUTH_BYTES: usize = 16 * 1024;

/// What Maple found out about one command line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct CliDetection {
    pub(super) executable: Option<PathBuf>,
    pub(super) version: Option<String>,
    /// `None` when Maple could not tell.
    pub(super) signed_in: Option<bool>,
    /// Why the installation cannot be used, if it cannot.
    pub(super) problem: Option<String>,
}

/// Find `codex`, read its version, and see whether a sign-in exists.
pub(super) async fn detect_codex(search_path: Option<&str>) -> CliDetection {
    let Some(executable) = find_executable("codex", search_path) else {
        return CliDetection::default();
    };
    let version = match probe_version(&executable, search_path).await {
        Ok(version) => version,
        Err(error) => {
            return CliDetection {
                executable: Some(executable),
                problem: Some(format!("Maple could not run `codex --version`: {error}")),
                ..CliDetection::default()
            };
        }
    };
    let (major, minor, patch) = CODEX_MIN_VERSION;
    let problem = match parse_version(&version) {
        Some(parsed) if parsed < CODEX_MIN_VERSION => Some(format!(
            "Codex {version} is older than the {major}.{minor}.{patch} that Maple needs. Update Codex."
        )),
        Some(_) => None,
        None => Some(format!(
            "Maple could not read the Codex version from `{version}`"
        )),
    };
    CliDetection {
        executable: Some(executable),
        version: Some(version),
        signed_in: Some(codex_auth_file_exists()),
        problem,
    }
}

/// Find `claude`, read its version, and ask it whether it is signed in.
pub(super) async fn detect_claude(search_path: Option<&str>) -> CliDetection {
    let Some(executable) = find_executable("claude", search_path) else {
        return CliDetection::default();
    };
    match probe_version(&executable, search_path).await {
        Ok(version) => CliDetection {
            signed_in: probe_claude_auth(&executable, search_path).await,
            executable: Some(executable),
            version: Some(version),
            problem: None,
        },
        Err(_) => CliDetection {
            executable: Some(executable),
            problem: Some(
                "Maple could not run `claude --version`. Check the Claude Code installation."
                    .to_string(),
            ),
            ..CliDetection::default()
        },
    }
}

/// `codex-cli 0.153.4` and plain `0.153.4` both read as (0, 153, 4).
pub(super) fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    text.split_whitespace().rev().find_map(|token| {
        let core = token.trim_start_matches('v').split(['-', '+']).next()?;
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next().unwrap_or("0").parse().ok()?;
        Some((major, minor, patch))
    })
}

/// Codex keeps its sign-in in `auth.json` under its home. Only whether the
/// file exists is read, never the file.
fn codex_auth_file_exists() -> bool {
    codex_home().is_some_and(|home| home.join("auth.json").is_file())
}

fn codex_home() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("CODEX_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(home));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|value| !value.is_empty())?;
    Some(PathBuf::from(home).join(".codex"))
}

/// The first `name` on `search_path`, or else on the process's PATH.
pub(in crate::agent) fn find_executable(name: &str, search_path: Option<&str>) -> Option<PathBuf> {
    let path = match search_path {
        Some(path) => OsString::from(path),
        None => std::env::var_os("PATH")?,
    };
    let candidates = executable_candidates(name);
    std::env::split_paths(&path).find_map(|dir| {
        candidates
            .iter()
            .map(|candidate| dir.join(candidate))
            .find(|candidate| candidate.is_file())
    })
}

/// The file names one command may be. Windows finds `codex` as `codex.exe`
/// or the npm shim `codex.cmd` through `PATHEXT`; a real executable comes
/// before a shim.
#[cfg(windows)]
pub(super) fn executable_candidates(name: &str) -> Vec<String> {
    if Path::new(name).extension().is_some() {
        return vec![name.to_string()];
    }
    let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".to_string());
    let mut extensions: Vec<String> = pathext
        .split(';')
        .map(str::trim)
        .filter(|extension| !extension.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    extensions.sort_by_key(|extension| extension != ".exe");
    extensions
        .into_iter()
        .map(|extension| format!("{name}{extension}"))
        .collect()
}

#[cfg(not(windows))]
pub(super) fn executable_candidates(name: &str) -> Vec<String> {
    vec![name.to_string()]
}

/// `executable --version`, trimmed.
async fn probe_version(executable: &Path, search_path: Option<&str>) -> Result<String, String> {
    let (status, output) =
        probe(executable, &["--version"], search_path, MAX_VERSION_BYTES).await?;
    if !status.success() {
        return Err(format!("it exited with {status}"));
    }
    let text = String::from_utf8_lossy(&output);
    let version = text.trim();
    if version.is_empty() {
        return Err("it printed nothing".to_string());
    }
    Ok(version.to_string())
}

/// `claude auth status --json`: signed in when it says so and exits 0, not
/// signed in when it says so and exits 1, unknown otherwise. Only the
/// boolean is read; account details are never kept or logged.
async fn probe_claude_auth(executable: &Path, search_path: Option<&str>) -> Option<bool> {
    #[derive(Deserialize)]
    struct AuthStatus {
        #[serde(rename = "loggedIn")]
        logged_in: bool,
    }
    let (status, output) = probe(
        executable,
        &["auth", "status", "--json"],
        search_path,
        MAX_AUTH_BYTES,
    )
    .await
    .ok()?;
    let auth: AuthStatus = serde_json::from_slice(&output).ok()?;
    match (status.code(), auth.logged_in) {
        (Some(0), true) => Some(true),
        (Some(1), false) => Some(false),
        _ => None,
    }
}

/// Run `executable` with `args` and no input, reading at most `max_bytes`
/// of its output, for up to [`PROBE_TIMEOUT`]. A command found on
/// `search_path` runs with it as its PATH, so a script finds its
/// interpreter there too.
async fn probe(
    executable: &Path,
    args: &[&str],
    search_path: Option<&str>,
    max_bytes: usize,
) -> Result<(ExitStatus, Vec<u8>), String> {
    let mut command = tokio::process::Command::new(executable);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(path) = search_path {
        command.env("PATH", path);
    }
    run_contained(command, max_bytes).await
}

/// The probe runs in its own process session, which is ended afterwards, so
/// nothing it started outlives it.
#[cfg(unix)]
async fn run_contained(
    command: tokio::process::Command,
    max_bytes: usize,
) -> Result<(ExitStatus, Vec<u8>), String> {
    use process_wrap::tokio::{CommandWrap, ProcessSession};

    let mut command = CommandWrap::from(command);
    command.wrap(ProcessSession);
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not start it: {error}"))?;
    let stdout = child
        .stdout()
        .take()
        .ok_or_else(|| "could not capture its output".to_string())?;
    let result =
        tokio::time::timeout(PROBE_TIMEOUT, collect(child.wait(), stdout, max_bytes)).await;
    if let Err(error) = child.start_kill() {
        log::debug!("Nothing left to stop after an integration probe: {error}");
    }
    let _ = tokio::time::timeout(CLEANUP_TIMEOUT, child.wait()).await;
    result.unwrap_or_else(|_| Err(TIMED_OUT.to_string()))
}

/// The probe's process tree is ended if it overruns.
#[cfg(windows)]
async fn run_contained(
    mut command: tokio::process::Command,
    max_bytes: usize,
) -> Result<(ExitStatus, Vec<u8>), String> {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not start it: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "could not capture its output".to_string())?;
    let result =
        tokio::time::timeout(PROBE_TIMEOUT, collect(child.wait(), stdout, max_bytes)).await;
    if result.is_err() {
        if let Some(pid) = child.id() {
            pi_coding_agent::tools::kill_process_tree(pid);
        }
        let _ = child.start_kill();
        let _ = tokio::time::timeout(CLEANUP_TIMEOUT, child.wait()).await;
    }
    result.unwrap_or_else(|_| Err(TIMED_OUT.to_string()))
}

const TIMED_OUT: &str = "it did not finish in time";

/// The probe's exit status and output, once it has exited and closed its
/// output.
async fn collect(
    wait: impl Future<Output = std::io::Result<ExitStatus>>,
    stdout: tokio::process::ChildStdout,
    max_bytes: usize,
) -> Result<(ExitStatus, Vec<u8>), String> {
    let (status, output) = tokio::join!(wait, read_bounded_stdout(stdout, max_bytes, "its output"));
    let status = status.map_err(|error| format!("could not wait for it: {error}"))?;
    Ok((status, output?))
}
