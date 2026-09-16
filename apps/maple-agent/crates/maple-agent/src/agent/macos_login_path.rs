use process_wrap::tokio::{ChildWrapper, CommandWrap, ProcessSession};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

const LOGIN_SHELL_PATH_TIMEOUT: Duration = Duration::from_secs(5);
const LOGIN_SHELL_CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_LOGIN_SHELL_OUTPUT_BYTES: usize = 64 * 1024;
const LOGIN_SHELL_PATH_MARKER_ENV: &str = "MAPLE_LOGIN_SHELL_PATH_MARKER";
const LOGIN_SHELL_PATH_MARKER: &str = "__MAPLE_LOGIN_SHELL_PATH_V1__";
const LOGIN_SHELL_PATH_QUERY: &str =
    "/usr/bin/printenv MAPLE_LOGIN_SHELL_PATH_MARKER && /usr/bin/printenv PATH";

/// Recover the search directories a macOS user gets in their interactive login shell.
///
/// Finder and Dock launches inherit launchd's minimal PATH rather than shell startup files.
/// Goose consumes these directories through its supported `GOOSE_SEARCH_PATHS` setting; Maple's
/// process environment is deliberately left unchanged.
pub(super) async fn resolve_login_shell_search_paths() -> Vec<String> {
    let shell = selected_login_shell();
    match query_login_shell_search_paths(&shell, LOGIN_SHELL_PATH_TIMEOUT).await {
        Ok(paths) => {
            log::debug!(
                "Recovered {} macOS login-shell search paths for Goose STDIO extensions",
                paths.len()
            );
            paths
        }
        Err(error) => {
            log::warn!(
                "Failed to recover the macOS login-shell PATH for Goose STDIO extensions; \
                 using Goose's built-in and inherited search paths: {error}"
            );
            Vec::new()
        }
    }
}

fn selected_login_shell() -> PathBuf {
    std::env::var_os("SHELL")
        .filter(|shell| !shell.is_empty())
        .map(PathBuf::from)
        .filter(|shell| shell.is_absolute())
        .unwrap_or_else(|| PathBuf::from("/bin/zsh"))
}

async fn query_login_shell_search_paths(
    shell: &Path,
    timeout: Duration,
) -> Result<Vec<String>, String> {
    let output = run_login_shell_path_query(shell, timeout).await?;
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
            log::debug!("Failed to terminate macOS login-shell PATH probe: {error}");
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
            log::debug!("Failed to terminate dropped macOS login-shell PATH probe: {error}");
        }
    }
}

async fn run_login_shell_path_query(
    shell: &Path,
    timeout: Duration,
) -> Result<LoginShellOutput, String> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut command = tokio::process::Command::new(shell);
    command
        // `printenv` is shell-neutral; fish would corrupt PATH by space-joining `$PATH`.
        // macOS's BSD `printenv` accepts only one variable name, so query the marker and PATH in
        // separate invocations.
        // The marker makes the result unambiguous if startup or logout hooks write to stdout.
        .args(["-l", "-i", "-c", LOGIN_SHELL_PATH_QUERY])
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
    let mut stdout_task = tokio::spawn(super::bounded_process::read_bounded_stdout(
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

fn parse_login_shell_search_paths(stdout: &[u8]) -> Result<Vec<String>, String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use goose::config::search_path::SearchPaths;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;

    const COMMAND_NAME: &str = "maple-730-mcp-fixture";
    const CHILD_TEST_MARKER: &str = "MAPLE_730_GOOSE_PATH_CHILD";
    const CHILD_TEST_ROOT: &str = "MAPLE_730_GOOSE_PATH_ROOT";
    const CHILD_TEST_COMPLETION_FILE: &str = "goose-path-test-complete";
    const CHILD_TEST_NAME: &str =
        "agent::macos_login_path::tests::goose_search_paths_resolve_fixture_child";
    const RESTRICTED_GUI_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

    fn write_executable(path: &Path, contents: &str) {
        fs::write(path, contents).expect("fixture should be writable");
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).unwrap();
    }

    fn install_resolution_fixture(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let command_bin = root.join("command-bin");
        let interpreter_bin = root.join("interpreter-bin");
        fs::create_dir_all(&command_bin).unwrap();
        fs::create_dir_all(&interpreter_bin).unwrap();

        let shell = root.join("fixture-shell");
        write_executable(
            &shell,
            r#"#!/bin/sh
test "$1" = "-l" || exit 41
test "$2" = "-i" || exit 42
test "$3" = "-c" || exit 43
fixture_root=$(/usr/bin/dirname "$0")
export PATH="$fixture_root/command-bin:$fixture_root/interpreter-bin:/usr/bin:/bin:/usr/sbin:/sbin"
printf 'profile banner\n'
/bin/sh -c "$4"
status=$?
printf 'logout banner\n'
exit "$status"
"#,
        );
        write_executable(
            &command_bin.join(COMMAND_NAME),
            r#"#!/usr/bin/env maple-730-runtime-fixture
import json
import sys

if "--self-test" in sys.argv:
    print(f"interpreter-found:{__file__}")
    raise SystemExit(0)

for raw_line in sys.stdin:
    try:
        message = json.loads(raw_line)
    except json.JSONDecodeError:
        continue
    if "id" not in message:
        continue
    method = message.get("method")
    if method == "initialize":
        params = message.get("params") or {}
        result = {
            "protocolVersion": params.get("protocolVersion", "2025-06-18"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "maple-730-fixture", "version": "1.0.0"},
        }
        response = {"jsonrpc": "2.0", "id": message["id"], "result": result}
    elif method == "tools/list":
        response = {
            "jsonrpc": "2.0",
            "id": message["id"],
            "result": {"tools": []},
        }
    else:
        response = {
            "jsonrpc": "2.0",
            "id": message["id"],
            "error": {"code": -32601, "message": "Method not found"},
        }
    print(json.dumps(response), flush=True)
"#,
        );
        write_executable(
            &interpreter_bin.join("maple-730-runtime-fixture"),
            "#!/bin/sh\nexec /usr/bin/python3 \"$@\"\n",
        );
        (shell, command_bin, interpreter_bin)
    }

    #[test]
    fn parses_marked_path_and_ignores_banners_relative_and_duplicate_entries() {
        let paths = parse_login_shell_search_paths(
            b"profile banner\n__MAPLE_LOGIN_SHELL_PATH_V1__\n/custom/bin:relative:/usr/bin::/custom/bin:/bin\nlogout banner\n",
        )
        .unwrap();
        assert_eq!(paths, ["/custom/bin", "/usr/bin", "/bin"]);
    }

    #[tokio::test]
    async fn recovered_path_resolves_bare_command_and_env_interpreter() {
        let fixture = tempfile::tempdir().unwrap();
        let (shell, command_bin, interpreter_bin) = install_resolution_fixture(fixture.path());

        let missing = tokio::process::Command::new(COMMAND_NAME)
            .env("PATH", RESTRICTED_GUI_PATH)
            .output()
            .await;
        assert!(
            missing.is_err(),
            "the fixture must not resolve through the GUI-style PATH"
        );

        // This checks PATH resolution, not shell startup latency. Allow for process
        // scheduling on busy CI runners; dedicated tests below cover timeouts.
        let paths = query_login_shell_search_paths(&shell, Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(paths[0], command_bin.to_string_lossy());
        assert_eq!(paths[1], interpreter_bin.to_string_lossy());
        let effective_path = std::env::join_paths(paths.iter()).unwrap();
        let output = tokio::process::Command::new(COMMAND_NAME)
            .arg("--self-test")
            .env("PATH", effective_path)
            .output()
            .await
            .expect("the recovered path should resolve the bare command");
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!(
                "interpreter-found:{}\n",
                command_bin.join(COMMAND_NAME).display()
            )
        );
    }

    #[test]
    fn recovered_paths_flow_through_pinned_goose_resolution_and_child_path() {
        let fixture = tempfile::tempdir().unwrap();
        let (shell, _, _) = install_resolution_fixture(fixture.path());
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .arg(CHILD_TEST_NAME)
            .args(["--exact", "--ignored", "--nocapture", "--test-threads=1"])
            .env(CHILD_TEST_MARKER, "1")
            .env(CHILD_TEST_ROOT, fixture.path())
            .env("SHELL", shell)
            .env("PATH", RESTRICTED_GUI_PATH)
            .env_remove("GOOSE_SEARCH_PATHS")
            .output()
            .expect("isolated Goose PATH test process should start");
        assert!(
            output.status.success(),
            "isolated Goose PATH test failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            fixture.path().join(CHILD_TEST_COMPLETION_FILE).is_file(),
            "isolated Goose PATH test exited without reaching its final assertion"
        );
    }

    #[tokio::test]
    #[ignore = "isolated subprocess helper invoked by the parent regression test"]
    async fn goose_search_paths_resolve_fixture_child() {
        if std::env::var_os(CHILD_TEST_MARKER).is_none() {
            return;
        }
        let fixture_root = PathBuf::from(std::env::var_os(CHILD_TEST_ROOT).unwrap());
        let command_bin = fixture_root.join("command-bin");
        let interpreter_bin = fixture_root.join("interpreter-bin");
        assert_eq!(std::env::var("PATH").unwrap(), RESTRICTED_GUI_PATH);
        assert!(
            tokio::process::Command::new(COMMAND_NAME)
                .output()
                .await
                .is_err(),
            "negative control unexpectedly resolved the fixture from launchd PATH"
        );

        let recovered = resolve_login_shell_search_paths().await;
        assert_eq!(recovered[0], command_bin.to_string_lossy());
        assert_eq!(recovered[1], interpreter_bin.to_string_lossy());
        crate::agent::configure_embedded_goose(
            &fixture_root.join("goose-runtime"),
            crate::agent::DEFAULT_AGENT_MODEL,
            crate::agent::DEFAULT_GOOSE_MODE,
            Some(&recovered),
        )
        .unwrap();
        assert_eq!(
            std::env::var("PATH").unwrap(),
            RESTRICTED_GUI_PATH,
            "Maple must not mutate its process PATH"
        );
        assert_eq!(
            goose::config::Config::global()
                .get_goose_search_paths()
                .unwrap(),
            recovered
        );

        let resolved = SearchPaths::builder()
            .with_npm()
            .resolve(COMMAND_NAME)
            .expect("Goose should resolve the bare MCP executable");
        let output = tokio::process::Command::new(resolved)
            .arg("--self-test")
            .env("PATH", SearchPaths::builder().path().unwrap())
            .output()
            .await
            .expect("Goose's child PATH should resolve the fixture interpreter");
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!(
                "interpreter-found:{}\n",
                command_bin.join(COMMAND_NAME).display()
            )
        );

        let session_manager = std::sync::Arc::new(goose::session::SessionManager::new(
            fixture_root.join("sessions"),
        ));
        let permission_manager = std::sync::Arc::new(goose::config::PermissionManager::new(
            fixture_root.join("permissions"),
        ));
        let session = session_manager
            .create_session(
                fixture_root.clone(),
                "Maple 730 PATH fixture".to_string(),
                goose::session::SessionType::User,
                goose::config::GooseMode::SmartApprove,
            )
            .await
            .unwrap();
        let agent = std::sync::Arc::new(goose::agents::Agent::with_config(
            goose::agents::AgentConfig::new(
                std::sync::Arc::clone(&session_manager),
                permission_manager,
                None,
                goose::config::GooseMode::SmartApprove,
                true,
                goose::agents::GoosePlatform::GooseDesktop,
            ),
        ));
        let results = agent
            .add_extensions_bulk(
                vec![goose::agents::ExtensionConfig::Stdio {
                    name: "maple-730-fixture".to_string(),
                    description: "PATH regression fixture".to_string(),
                    cmd: COMMAND_NAME.to_string(),
                    args: Vec::new(),
                    envs: goose::agents::extension::Envs::new(std::collections::HashMap::new()),
                    env_keys: Vec::new(),
                    timeout: Some(5),
                    cwd: None,
                    bundled: Some(false),
                    available_tools: Vec::new(),
                }],
                &session.id,
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert!(
            results[0].success,
            "Goose STDIO initialization failed: {:?}",
            results[0].error
        );
        fs::write(fixture_root.join(CHILD_TEST_COMPLETION_FILE), b"complete\n").unwrap();
    }

    #[tokio::test]
    async fn hanging_login_shell_is_bounded_and_falls_back() {
        let fixture = tempfile::tempdir().unwrap();
        let shell = fixture.path().join("hanging-shell");
        write_executable(&shell, "#!/bin/sh\n/bin/sleep 30\n");

        let started = Instant::now();
        let result = query_login_shell_search_paths(&shell, Duration::from_millis(50)).await;
        assert!(result.unwrap_err().contains("did not finish"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    fn process_exists(pid: u32) -> bool {
        std::process::Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn kill_process(pid: u32) {
        let _ = std::process::Command::new("/bin/kill")
            .args(["-KILL", &pid.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    #[tokio::test]
    async fn same_group_stdout_holder_is_killed_on_output_timeout() {
        let fixture = tempfile::tempdir().unwrap();
        let shell = fixture.path().join("same-group-stdout-shell");
        let pid_file = fixture.path().join("same-group-child.pid");
        write_executable(
            &shell,
            r#"#!/bin/sh
fixture_root=${0%/*}
/bin/sleep 30 &
printf '%s\n' "$!" > "$fixture_root/same-group-child.pid"
printf '%s\n' "$MAPLE_LOGIN_SHELL_PATH_MARKER" '/usr/bin:/bin:/usr/sbin:/sbin'
exit 0
"#,
        );

        // This tests output cleanup after the leader exits, not how quickly macOS can start
        // several processes. Keep the fixture to shell builtins and the intentional stdout
        // holder, and allow the normal probe budget for a busy hosted runner to reap the leader.
        let started = Instant::now();
        let result = query_login_shell_search_paths(&shell, LOGIN_SHELL_PATH_TIMEOUT).await;
        let elapsed = started.elapsed();
        let pid = fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        let mut stopped = !process_exists(pid);
        for _ in 0..40 {
            if stopped {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
            stopped = !process_exists(pid);
        }
        if !stopped {
            kill_process(pid);
        }

        let error = result.unwrap_err();
        assert!(error.contains("output did not close"), "{error}");
        assert!(
            elapsed < Duration::from_secs(10),
            "probe waited for the stdout holder instead of its deadline: {elapsed:?}"
        );
        assert!(stopped, "same-process-group child {pid} survived cleanup");
    }

    #[tokio::test]
    #[ignore = "fails on GitHub macos runners: the detached holder does not keep the pipe \
              open there, so the probe succeeds instead of timing out; run it on a local Mac"]
    async fn detached_stdout_holder_cannot_extend_the_probe_deadline() {
        let fixture = tempfile::tempdir().unwrap();
        let shell = fixture.path().join("detached-stdout-shell");
        write_executable(
            &shell,
            r#"#!/usr/bin/python3
import os
import subprocess
import sys

subprocess.Popen(
    ["/bin/sleep", "2"],
    stdin=subprocess.DEVNULL,
    stdout=sys.stdout,
    stderr=subprocess.DEVNULL,
    start_new_session=True,
)
print(os.environ["MAPLE_LOGIN_SHELL_PATH_MARKER"])
print("/usr/bin:/bin", flush=True)
"#,
        );

        let started = Instant::now();
        let result = query_login_shell_search_paths(&shell, Duration::from_millis(500)).await;
        let error = result.unwrap_err();
        assert!(error.contains("output did not close"), "{error}");
        assert!(started.elapsed() < Duration::from_millis(1500));
    }
}
