//! The search path of the user's login shell.
//!
//! An app started from the Dock, Finder or a desktop launcher inherits the
//! session's minimal PATH instead of the one the user's shell startup files
//! build, so tools such as `cargo` or `node` would not be found. Maple asks
//! a login shell for its PATH once per process and gives it to the commands
//! it starts. Maple's own environment is left unchanged.

#[cfg(not(windows))]
mod probe {
    use super::*;
    use process_wrap::tokio::{ChildWrapper, CommandWrap, ProcessSession};
    use std::collections::HashSet;
    use std::path::Path;
    use std::process::{ExitStatus, Stdio};
    use std::time::Duration;

    pub(super) const LOGIN_SHELL_PATH_TIMEOUT: Duration = Duration::from_secs(5);
    const LOGIN_SHELL_CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);
    const MAX_LOGIN_SHELL_OUTPUT_BYTES: usize = 64 * 1024;
    const LOGIN_SHELL_PATH_MARKER_ENV: &str = "MAPLE_LOGIN_SHELL_PATH_MARKER";
    const LOGIN_SHELL_PATH_MARKER: &str = "__MAPLE_LOGIN_SHELL_PATH_V1__";

    pub(super) async fn query_login_shell_search_paths(
        shell: &Path,
        timeout: Duration,
    ) -> Result<Vec<String>, String> {
        let output = run_login_shell_path_query(shell, login_shell_path_query(), timeout).await?;
        if !output.status.success() {
            return Err(format!("login shell exited with status {}", output.status));
        }
        parse_login_shell_search_paths(&output.stdout)
    }

    struct LoginShellOutput {
        status: ExitStatus,
        stdout: Vec<u8>,
    }

    /// Keeps the process-session kill path armed across every await in the probe.
    ///
    /// `kill_on_drop` reaches only the shell leader. Shell profiles can start descendants, so the
    /// process-wrap session (which is also a process group) must remain reachable on cancellation and
    /// timeout as well.
    struct ArmedLoginShellChild {
        child: Box<dyn ChildWrapper>,
        armed: bool,
    }

    impl ArmedLoginShellChild {
        fn new(child: Box<dyn ChildWrapper>) -> Self {
            Self { child, armed: true }
        }

        async fn wait(&mut self) -> std::io::Result<ExitStatus> {
            self.child.wait().await
        }

        async fn terminate_and_reap(&mut self) {
            if let Err(error) = self.child.start_kill() {
                log::debug!("Failed to terminate login-shell PATH probe: {error}");
            }
            if let Ok(Ok(_)) =
                tokio::time::timeout(LOGIN_SHELL_CLEANUP_TIMEOUT, self.child.wait()).await
            {
                self.armed = false;
            }
        }

        fn disarm(&mut self) {
            self.armed = false;
        }
    }

    impl Drop for ArmedLoginShellChild {
        fn drop(&mut self) {
            if self.armed
                && let Err(error) = self.child.start_kill()
            {
                log::debug!("Failed to terminate dropped login-shell PATH probe: {error}");
            }
        }
    }

    async fn run_login_shell_path_query(
        shell: &Path,
        query: &str,
        timeout: Duration,
    ) -> Result<LoginShellOutput, String> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut command = tokio::process::Command::new(shell);
        command
            // `printenv` is shell-neutral; fish would corrupt PATH by space-joining `$PATH`.
            // macOS's BSD `printenv` accepts only one variable name, so query the marker and PATH in
            // separate invocations.
            // The marker makes the result unambiguous if startup or logout hooks write to stdout.
            .args(["-l", "-i", "-c", query])
            .env(LOGIN_SHELL_PATH_MARKER_ENV, LOGIN_SHELL_PATH_MARKER)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        let mut command = CommandWrap::from(command);
        // A new session prevents interactive job-control setup from affecting Maple's terminal and
        // gives timeout cleanup a process group that includes profile-script descendants.
        command.wrap(ProcessSession);
        let mut child = ArmedLoginShellChild::new(
            command
                .spawn()
                .map_err(|error| format!("could not start login shell: {error}"))?,
        );
        let stdout = child
            .child
            .stdout()
            .take()
            .ok_or_else(|| "could not capture login-shell output".to_string())?;
        let mut stdout_task = tokio::spawn(crate::agent::bounded_process::read_bounded_stdout(
            stdout,
            MAX_LOGIN_SHELL_OUTPUT_BYTES,
            "login-shell output",
        ));

        let status = match tokio::time::timeout_at(deadline, child.wait()).await {
            Ok(Ok(status)) => status,
            Ok(Err(error)) => {
                stdout_task.abort();
                child.terminate_and_reap().await;
                return Err(format!("could not wait for login shell: {error}"));
            }
            Err(_) => {
                stdout_task.abort();
                child.terminate_and_reap().await;
                return Err(format!(
                    "login shell did not finish within {} seconds",
                    timeout.as_secs_f32()
                ));
            }
        };
        let stdout = match tokio::time::timeout_at(deadline, &mut stdout_task).await {
            Ok(Ok(Ok(stdout))) => stdout,
            Ok(Ok(Err(error))) => {
                child.terminate_and_reap().await;
                return Err(error);
            }
            Ok(Err(error)) => {
                child.terminate_and_reap().await;
                return Err(format!("could not collect login-shell output: {error}"));
            }
            Err(_) => {
                stdout_task.abort();
                child.terminate_and_reap().await;
                return Err(format!(
                    "login-shell output did not close within {} seconds",
                    timeout.as_secs_f32()
                ));
            }
        };
        child.disarm();
        Ok(LoginShellOutput { status, stdout })
    }

    pub(super) fn parse_login_shell_search_paths(stdout: &[u8]) -> Result<Vec<String>, String> {
        let stdout = std::str::from_utf8(stdout)
            .map_err(|_| "login shell returned a PATH that is not valid UTF-8".to_string())?;
        // Interactive startup and logout files can both print banners. The fixed query prints this
        // marker immediately before PATH, so later teardown output cannot be mistaken for a directory.
        let mut lines = stdout.lines();
        let path = loop {
            let line = lines
                .next()
                .ok_or_else(|| "login shell did not return the PATH marker".to_string())?;
            if line.trim() == LOGIN_SHELL_PATH_MARKER {
                break lines
                    .next()
                    .map(str::trim)
                    .filter(|path| !path.is_empty())
                    .ok_or_else(|| "login shell returned an empty PATH".to_string())?;
            }
        };

        let mut seen = HashSet::new();
        let paths = std::env::split_paths(path)
            .filter(|entry| !entry.as_os_str().is_empty())
            .filter(|entry| entry.is_absolute())
            .filter_map(|entry| entry.into_os_string().into_string().ok())
            .filter(|entry| seen.insert(entry.clone()))
            .collect::<Vec<_>>();
        if paths.is_empty() {
            return Err("login shell returned no usable search directories".to_string());
        }
        Ok(paths)
    }
}

static LOGIN_SEARCH_PATH: tokio::sync::OnceCell<Option<String>> =
    tokio::sync::OnceCell::const_new();

/// The login shell's search path, joined, or `None` when it could not be
/// read (or on Windows, where processes inherit the user's PATH).
pub(crate) async fn login_search_path() -> Option<String> {
    LOGIN_SEARCH_PATH
        .get_or_init(resolve_login_search_path)
        .await
        .clone()
}

/// The login shell's search path if it has been read already, without
/// waiting for it.
pub(crate) fn known_login_search_path() -> Option<String> {
    LOGIN_SEARCH_PATH.get().cloned().flatten()
}

#[cfg(windows)]
async fn resolve_login_search_path() -> Option<String> {
    None
}

#[cfg(not(windows))]
async fn resolve_login_search_path() -> Option<String> {
    use std::path::Path;

    // Inside Flatpak, commands run on the host through flatpak-spawn and
    // get the host's environment there.
    if Path::new("/.flatpak-info").exists() {
        return None;
    }
    let shell = login_shell();
    match probe::query_login_shell_search_paths(&shell, probe::LOGIN_SHELL_PATH_TIMEOUT).await {
        Ok(paths) => {
            log::debug!("Recovered {} login-shell search paths", paths.len());
            std::env::join_paths(paths)
                .ok()
                .and_then(|joined| joined.into_string().ok())
        }
        Err(error) => {
            log::warn!("Failed to recover the login-shell PATH; using the inherited one: {error}");
            None
        }
    }
}

/// The shell to ask: on macOS the user's login shell, elsewhere the bash the
/// `bash` tool runs.
#[cfg(target_os = "macos")]
fn login_shell() -> std::path::PathBuf {
    use std::path::PathBuf;

    std::env::var_os("SHELL")
        .filter(|shell| !shell.is_empty())
        .map(PathBuf::from)
        .filter(|shell| shell.is_absolute())
        .unwrap_or_else(|| PathBuf::from("/bin/zsh"))
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn login_shell() -> std::path::PathBuf {
    let custom = std::env::var_os(super::tools::SHELL_ENV)
        .filter(|shell| !shell.is_empty())
        .map(std::path::PathBuf::from);
    pi_coding_agent::tools::shell_config(custom.as_deref())
        .map(|config| config.shell)
        .unwrap_or_else(|_| std::path::PathBuf::from("sh"))
}

/// `printenv` is shell-neutral; fish would corrupt PATH by space-joining
/// `$PATH`. macOS's BSD `printenv` takes one variable name, so the marker
/// and PATH are separate invocations. macOS always has it in `/usr/bin`;
/// other systems may not (NixOS), so there it is found on the login PATH.
#[cfg(not(windows))]
fn login_shell_path_query() -> &'static str {
    if cfg!(target_os = "macos") {
        "/usr/bin/printenv MAPLE_LOGIN_SHELL_PATH_MARKER && /usr/bin/printenv PATH"
    } else {
        "printenv MAPLE_LOGIN_SHELL_PATH_MARKER && printenv PATH"
    }
}

#[cfg(all(test, unix))]
mod tests;
