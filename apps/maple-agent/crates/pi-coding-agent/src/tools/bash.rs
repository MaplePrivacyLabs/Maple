//! `bash`, and `powershell` on Windows: one command in the session's folder.
//!
//! Output streams to the interface as it arrives. The model gets the last 2000 lines or
//! 50KB, whichever is less, and when that cuts anything the full output is saved to a
//! file it can read. The embedding host can choose a default timeout. Stop or a
//! timeout ends the command's process group (its process tree on Windows).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use pi_agent_core::{AgentTool, AgentToolResult, ToolError, ToolInvocation, ToolUpdates};
use pi_ai::{Content, Tool};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::output::{OutputAccumulator, OutputSnapshot};
use super::shell::{
    CommandTransport, ShellConfig, kill_process_tree, powershell_config, set_env_var, shell_config,
    shell_env, track_child, untrack_child,
};
use super::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, format_size};
use super::{PREFER_STRICT, ToolContext};

/// The longest timeout a call can ask for, in seconds.
const MAX_TIMEOUT_SECONDS: f64 = 2_147_483_647.0 / 1000.0;
/// How often output updates reach the interface at most.
const UPDATE_THROTTLE: Duration = Duration::from_millis(100);
/// Quiet commands still report their execution status to the interface.
const STATUS_INTERVAL: Duration = Duration::from_secs(5);
/// After the shell exits, how long its output may stay quiet before collection stops.
/// A background process can keep the output open; output still arriving keeps it open.
const EXIT_STDIO_GRACE: Duration = Duration::from_millis(100);
/// Run first in every PowerShell command, so its output is UTF-8.
const POWERSHELL_UTF8_PREFIX: &str =
    "try { [Console]::OutputEncoding=[System.Text.Encoding]::UTF8 } catch {}\n";

pub const BASH_SNIPPET: &str = "Execute bash commands (ls, grep, find, etc.)";
pub const POWERSHELL_SNIPPET: &str = "Execute PowerShell commands";

/// What a command runs as: the command line, its folder and its environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BashSpawnContext {
    pub command: String,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
}

/// Adjusts a command, its folder or its environment before it runs.
pub type BashSpawnHook = Arc<dyn Fn(BashSpawnContext) -> BashSpawnContext + Send + Sync>;

/// Receives each chunk of a command's output.
pub type OnData = Arc<dyn Fn(&[u8]) + Send + Sync>;

pub struct ExecOptions {
    /// Called with each chunk of output, stdout and stderr together.
    pub on_data: OnData,
    pub cancel: CancellationToken,
    /// Seconds before the command is stopped.
    pub timeout: Option<f64>,
    /// The whole environment; this process's when `None`.
    pub env: Option<BTreeMap<String, String>>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExecError {
    /// Stopped by the call's cancellation.
    Aborted,
    /// Stopped after this many seconds.
    TimedOut(f64),
    Failed(String),
}

/// Where commands run. Replace it to run them somewhere else, for example over SSH.
#[async_trait]
pub trait BashOperations: Send + Sync {
    /// Run `command` in `cwd`, streaming its output. The exit code is `None` when the
    /// command ended without one; a command killed by a signal reports 128 plus the
    /// signal's number.
    /// A selected working directory that is unavailable must fail, never fall back
    /// to the host's working directory or a temporary folder.
    async fn exec(
        &self,
        command: &str,
        cwd: &Path,
        options: ExecOptions,
    ) -> Result<Option<i32>, ExecError>;
}

#[derive(Clone, Debug)]
enum LocalShell {
    Bash { shell_path: Option<PathBuf> },
    PowerShell,
}

/// Commands on this machine, in their own process group (a process tree on Windows)
/// so stopping one reaches the group's members, including pipeline processes.
#[derive(Clone, Debug)]
pub struct LocalShellOperations {
    shell: LocalShell,
}

impl LocalShellOperations {
    /// `bash` through `shell_path`, or the shell [`shell_config`] finds.
    pub fn bash(shell_path: Option<PathBuf>) -> Self {
        Self {
            shell: LocalShell::Bash { shell_path },
        }
    }

    pub fn powershell() -> Self {
        Self {
            shell: LocalShell::PowerShell,
        }
    }
}

#[async_trait]
impl BashOperations for LocalShellOperations {
    async fn exec(
        &self,
        command: &str,
        cwd: &Path,
        options: ExecOptions,
    ) -> Result<Option<i32>, ExecError> {
        let timeout = resolve_timeout(options.timeout).map_err(ExecError::Failed)?;
        if options.cancel.is_cancelled() {
            return Err(ExecError::Aborted);
        }
        let (config, shell_name, command) = match &self.shell {
            LocalShell::Bash { shell_path } => (
                shell_config(shell_path.as_deref()).map_err(ExecError::Failed)?,
                "bash",
                command.to_string(),
            ),
            LocalShell::PowerShell => (
                powershell_config().map_err(ExecError::Failed)?,
                "PowerShell",
                format!("{POWERSHELL_UTF8_PREFIX}{command}"),
            ),
        };
        if !cwd.is_absolute() {
            return Err(ExecError::Failed(
                "Working directory must be an absolute path".into(),
            ));
        }
        if !tokio::fs::metadata(cwd)
            .await
            .is_ok_and(|metadata| metadata.is_dir())
        {
            return Err(ExecError::Failed(format!(
                "Working directory does not exist: {}\nCannot execute {shell_name} commands.",
                cwd.display()
            )));
        }
        run_local(&config, &command, cwd, timeout, options).await
    }
}

fn resolve_timeout(timeout: Option<f64>) -> Result<Option<Duration>, String> {
    let Some(seconds) = timeout else {
        return Ok(None);
    };
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err("Invalid timeout: must be a finite number of seconds".to_string());
    }
    if seconds > MAX_TIMEOUT_SECONDS {
        return Err(format!(
            "Invalid timeout: maximum is {MAX_TIMEOUT_SECONDS} seconds"
        ));
    }
    Ok(Some(Duration::from_secs_f64(seconds)))
}

/// Kills the command's process group or tree if the call is dropped before the command
/// ends, also when the future running it is dropped.
struct TreeGuard {
    pid: Option<u32>,
}

impl TreeGuard {
    fn new(pid: Option<u32>) -> Self {
        if let Some(pid) = pid {
            track_child(pid);
        }
        Self { pid }
    }

    fn release(&mut self) {
        if let Some(pid) = self.pid.take() {
            untrack_child(pid);
        }
    }
}

impl Drop for TreeGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid.take() {
            kill_process_tree(pid);
            untrack_child(pid);
        }
    }
}

async fn run_local(
    config: &ShellConfig,
    command: &str,
    cwd: &Path,
    timeout: Option<Duration>,
    options: ExecOptions,
) -> Result<Option<i32>, ExecError> {
    let mut process = tokio::process::Command::new(&config.shell);
    process.args(&config.args);
    if config.transport == CommandTransport::Argv {
        process.arg(command);
    }
    process
        .current_dir(cwd)
        .env_clear()
        .envs(options.env.unwrap_or_else(shell_env))
        .stdin(match config.transport {
            CommandTransport::Stdin => Stdio::piped(),
            CommandTransport::Argv => Stdio::null(),
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    process.process_group(0);
    #[cfg(windows)]
    process.creation_flags(0x0800_0000); // CREATE_NO_WINDOW

    let mut child = process.spawn().map_err(|error| {
        ExecError::Failed(format!(
            "Could not start {}: {error}",
            config.shell.display()
        ))
    })?;
    let pid = child.id();
    let mut guard = TreeGuard::new(pid);
    let activity = Arc::new(Notify::new());
    let mut readers = tokio::task::JoinSet::new();
    if let Some(stdout) = child.stdout.take() {
        readers.spawn(pump(stdout, options.on_data.clone(), activity.clone()));
    }
    if let Some(stderr) = child.stderr.take() {
        readers.spawn(pump(stderr, options.on_data.clone(), activity.clone()));
    }
    let stdin = child.stdin.take();

    let kill = |pid: Option<u32>| {
        if let Some(pid) = pid {
            kill_process_tree(pid);
        }
    };
    let mut aborted = false;
    let mut timed_out = false;
    let deadline = async {
        match timeout {
            Some(timeout) => tokio::time::sleep(timeout).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(deadline);
    // Writing a large command to a stdin-based shell can block too. Keep that
    // write inside the same cancellation/deadline boundary as waiting for exit.
    let wait = async {
        if let Some(mut stdin) = stdin {
            let _ = stdin.write_all(command.as_bytes()).await;
        }
        child.wait().await
    };
    tokio::pin!(wait);
    let status = loop {
        tokio::select! {
            status = &mut wait => break status,
            _ = options.cancel.cancelled(), if !aborted => {
                aborted = true;
                kill(pid);
            }
            _ = &mut deadline, if !timed_out => {
                timed_out = true;
                kill(pid);
            }
        }
    };
    // Reaped: from here the pid may belong to another process, so nothing is killed
    // through it, and a background job the command left keeps running.
    guard.release();

    // The shell has exited. Collect output until both streams close, or until they have
    // been quiet for a moment: a background process may hold them open, and output it
    // keeps writing keeps collection going, as in Pi. Stop and the timeout still end the
    // call then and, as in Pi, what is left of the command's process group, whose id
    // stays the shell's while any member lives.
    loop {
        let quiet = tokio::time::timeout(EXIT_STDIO_GRACE, async {
            tokio::select! {
                finished = readers.join_next() => finished.is_none(),
                _ = activity.notified() => false,
            }
        });
        tokio::select! {
            quiet = quiet => match quiet {
                Ok(true) | Err(_) => break,
                Ok(false) => continue,
            },
            _ = options.cancel.cancelled(), if !aborted => {
                aborted = true;
                end_process_group(pid);
                break;
            }
            _ = &mut deadline, if !timed_out => {
                timed_out = true;
                end_process_group(pid);
                break;
            }
        }
    }
    readers.abort_all();

    let status = status
        .map_err(|error| ExecError::Failed(format!("Could not wait for the command: {error}")))?;
    if aborted || options.cancel.is_cancelled() {
        return Err(ExecError::Aborted);
    }
    if timed_out {
        return Err(ExecError::TimedOut(options.timeout.unwrap_or_default()));
    }
    Ok(Some(exit_code(status)))
}

/// End what is left of a finished command's process group. Only the group is
/// signalled: the shell itself is reaped, and its pid may be another process's now.
fn end_process_group(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid.and_then(|pid| libc::pid_t::try_from(pid).ok()) {
        // SAFETY: kill only sends a signal; a negative pid names the process group.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
    }
    #[cfg(not(unix))]
    let _ = pid;
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    1
}

async fn pump<R: AsyncRead + Unpin>(mut stream: R, on_data: OnData, activity: Arc<Notify>) {
    let mut buffer = vec![0u8; 8 * 1024];
    loop {
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                on_data(&buffer[..read]);
                activity.notify_one();
            }
        }
    }
}

/// Options for `bash`.
#[derive(Clone)]
pub struct BashToolOptions {
    /// Seconds before a command without an explicit timeout is stopped.
    /// Standalone Pi has no default; an embedding host can choose one.
    pub default_timeout: Option<f64>,
    /// Full output goes here when present, without falling back on a failed path.
    pub output_dir: Option<PathBuf>,
    /// Where commands run; this machine when `None`.
    pub operations: Option<Arc<dyn BashOperations>>,
    /// Run before every command, for example shell setup.
    pub command_prefix: Option<String>,
    /// The shell to use instead of the one found.
    pub shell_path: Option<PathBuf>,
    /// Give commands the session's id, file, model and thinking level as
    /// `<APP>_SESSION_ID` and the like.
    pub expose_session_environment: bool,
    pub spawn_hook: Option<BashSpawnHook>,
}

impl Default for BashToolOptions {
    fn default() -> Self {
        Self {
            default_timeout: None,
            output_dir: None,
            operations: None,
            command_prefix: None,
            shell_path: None,
            expose_session_environment: true,
            spawn_hook: None,
        }
    }
}

/// Options for `powershell`.
#[derive(Clone)]
pub struct PowerShellToolOptions {
    pub default_timeout: Option<f64>,
    pub output_dir: Option<PathBuf>,
    pub operations: Option<Arc<dyn BashOperations>>,
    pub expose_session_environment: bool,
    pub spawn_hook: Option<BashSpawnHook>,
}

impl Default for PowerShellToolOptions {
    fn default() -> Self {
        Self {
            default_timeout: None,
            output_dir: None,
            operations: None,
            expose_session_environment: true,
            spawn_hook: None,
        }
    }
}

/// `bash` or `powershell`.
pub struct ShellTool {
    declaration: Tool,
    name: &'static str,
    cwd: PathBuf,
    output_dir: Option<PathBuf>,
    operations: Arc<dyn BashOperations>,
    command_prefix: Option<String>,
    expose_session_environment: bool,
    spawn_hook: Option<BashSpawnHook>,
    default_timeout: Option<f64>,
    context: ToolContext,
}

#[derive(Deserialize)]
struct ShellParams {
    command: String,
    #[serde(default)]
    workdir: Option<PathBuf>,
    #[serde(default)]
    timeout: Option<f64>,
}

fn shell_declaration(name: &str, shell_name: &str, default_timeout: Option<f64>) -> Tool {
    let timeout_description = match default_timeout {
        Some(seconds) => format!(
            "Timeout in seconds (optional, defaults to {seconds} seconds). Set explicitly for longer commands."
        ),
        None => "Timeout in seconds (optional, no default timeout)".to_string(),
    };
    Tool::new(
        name,
        format!(
            "Execute a {shell_name} command in the session working directory, or an explicit absolute workdir. Each call starts in that folder; cd does not change later calls. Returns stdout and stderr. Output is truncated to last {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first). If truncated, full output is saved to a temp file. {timeout_description}",
            DEFAULT_MAX_BYTES / 1024
        ),
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command to execute" },
                "workdir": { "type": "string", "description": "Absolute working directory on the execution computer. For repository searches, set this to the intended checkout or worktree." },
                "timeout": {
                    "type": "number",
                    "description": timeout_description
                }
            },
            "required": ["command"]
        }),
    )
            .with_constrained_sampling(PREFER_STRICT)
}

impl ShellTool {
    pub fn bash(cwd: impl Into<PathBuf>, options: BashToolOptions, context: ToolContext) -> Self {
        let operations = options
            .operations
            .unwrap_or_else(|| Arc::new(LocalShellOperations::bash(options.shell_path)));
        Self {
            declaration: shell_declaration("bash", "bash", options.default_timeout),
            name: "bash",
            cwd: cwd.into(),
            output_dir: options.output_dir,
            operations,
            command_prefix: options.command_prefix,
            expose_session_environment: options.expose_session_environment,
            spawn_hook: options.spawn_hook,
            default_timeout: options.default_timeout,
            context,
        }
    }

    pub fn powershell(
        cwd: impl Into<PathBuf>,
        options: PowerShellToolOptions,
        context: ToolContext,
    ) -> Self {
        let operations = options
            .operations
            .unwrap_or_else(|| Arc::new(LocalShellOperations::powershell()));
        Self {
            declaration: shell_declaration("powershell", "PowerShell", options.default_timeout),
            name: "powershell",
            cwd: cwd.into(),
            output_dir: options.output_dir,
            operations,
            command_prefix: None,
            expose_session_environment: options.expose_session_environment,
            spawn_hook: options.spawn_hook,
            default_timeout: options.default_timeout,
            context,
        }
    }

    /// The guideline that goes with the tool's snippet.
    pub fn guidelines(&self) -> Vec<String> {
        let mut rules = vec![
            "For repository searches, set workdir to the absolute intended checkout/worktree and search within it. Verify the folder before using parent paths; never guess ../../ from a temporary folder. Avoid recursive searches of filesystem roots or virtual filesystems such as /proc, /sys and /dev. head limits output, not runtime; use a timeout for searches.".to_string(),
        ];
        if self.expose_session_environment {
            rules.push(format!(
                "You can inspect {}_* environment variables for current model and session details.",
                self.context.env_prefix()
            ));
        }
        rules
    }

    fn spawn_context(&self, command: String, cwd: PathBuf) -> BashSpawnContext {
        let prefix = self.context.env_prefix();
        let keys = [
            format!("{prefix}_SESSION_ID"),
            format!("{prefix}_SESSION_FILE"),
            format!("{prefix}_PROVIDER"),
            format!("{prefix}_MODEL"),
            format!("{prefix}_REASONING_LEVEL"),
        ];
        let mut env = shell_env();
        for key in &keys {
            env.retain(|existing, _| !existing.eq_ignore_ascii_case(key));
        }
        if self.expose_session_environment && self.context.in_session() {
            if let Some(id) = self.context.session_id() {
                set_env_var(&mut env, &keys[0], id);
            }
            if let Some(file) = self.context.session_file() {
                set_env_var(&mut env, &keys[1], file.to_string_lossy());
            }
            if let Some(model) = self.context.model() {
                set_env_var(&mut env, &keys[2], model.provider);
                set_env_var(&mut env, &keys[3], model.id);
            }
            if let Some(level) = self.context.thinking_level() {
                set_env_var(&mut env, &keys[4], level);
            }
        }
        let context = BashSpawnContext { command, cwd, env };
        match &self.spawn_hook {
            Some(hook) => hook(context),
            None => context,
        }
    }
}

/// Sends the output so far to the interface, at most once per [`UPDATE_THROTTLE`].
struct UpdateThrottle {
    dirty: Arc<AtomicBool>,
    wake: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
    output: Arc<Mutex<OutputAccumulator>>,
    updates: ToolUpdates,
    progress: Arc<ShellProgress>,
}

struct ShellProgress {
    cwd: PathBuf,
    timeout: Option<f64>,
    started: Instant,
    last_output: Mutex<Instant>,
}

impl UpdateThrottle {
    fn start(
        output: Arc<Mutex<OutputAccumulator>>,
        updates: ToolUpdates,
        cwd: PathBuf,
        timeout: Option<f64>,
    ) -> Self {
        let progress = Arc::new(ShellProgress {
            cwd,
            timeout,
            started: Instant::now(),
            last_output: Mutex::new(Instant::now()),
        });
        let dirty = Arc::new(AtomicBool::new(true));
        send_update(&dirty, &output, &updates, &progress);
        let wake = Arc::new(Notify::new());
        let task = tokio::spawn({
            let (dirty, wake, output, updates, progress) = (
                dirty.clone(),
                wake.clone(),
                output.clone(),
                updates.clone(),
                progress.clone(),
            );
            async move {
                let mut status = tokio::time::interval(STATUS_INTERVAL);
                status.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                status.tick().await;
                loop {
                    tokio::select! {
                        _ = wake.notified() => {},
                        _ = status.tick() => dirty.store(true, Ordering::Release),
                    }
                    send_update(&dirty, &output, &updates, &progress);
                    tokio::time::sleep(UPDATE_THROTTLE).await;
                }
            }
        });
        Self {
            dirty,
            wake,
            task,
            output,
            updates,
            progress,
        }
    }

    fn mark(&self) {
        *self
            .progress
            .last_output
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Instant::now();
        self.dirty.store(true, Ordering::Release);
        self.wake.notify_one();
    }

    /// Stop, sending whatever has not been sent.
    fn finish(self) {
        self.task.abort();
        send_update(&self.dirty, &self.output, &self.updates, &self.progress);
    }
}

impl Drop for UpdateThrottle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn send_update(
    dirty: &AtomicBool,
    output: &Mutex<OutputAccumulator>,
    updates: &ToolUpdates,
    progress: &ShellProgress,
) {
    if !dirty.swap(false, Ordering::AcqRel) {
        return;
    }
    let snapshot = output
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .snapshot(true);
    let mut details = snapshot_details(&snapshot).unwrap_or_else(|| json!({}));
    details["shellExecution"] = json!({
        "cwd": progress.cwd.to_string_lossy(),
        "timeoutSeconds": progress.timeout,
        "elapsedSeconds": progress.started.elapsed().as_secs(),
        "outputIdleSeconds": progress.last_output.lock().unwrap_or_else(|error| error.into_inner()).elapsed().as_secs(),
    });
    updates.send(AgentToolResult {
        content: vec![Content::text(snapshot.content.clone())],
        details: Some(details),
        ..AgentToolResult::default()
    });
}

fn snapshot_details(snapshot: &OutputSnapshot) -> Option<Value> {
    snapshot.truncation.truncated.then(|| {
        json!({
            "truncation": snapshot.truncation,
            "fullOutputPath": snapshot.full_output_path,
        })
    })
}

/// The text the model reads, with a note on what was cut and where the rest is.
fn format_output(snapshot: &OutputSnapshot, empty_text: &str, last_line_bytes: usize) -> String {
    let truncation = &snapshot.truncation;
    let mut text = if snapshot.content.is_empty() {
        empty_text.to_string()
    } else {
        snapshot.content.clone()
    };
    if truncation.truncated {
        let full = snapshot.full_output_path.as_ref().map_or_else(
            || "(could not be saved)".to_string(),
            |path| path.display().to_string(),
        );
        let end = truncation.total_lines;
        let start = end.saturating_sub(truncation.output_lines) + 1;
        if truncation.last_line_partial {
            text.push_str(&format!(
                "\n\n[Showing last {} of line {end} (line is {}). Full output: {full}]",
                format_size(truncation.output_bytes),
                format_size(last_line_bytes)
            ));
        } else if truncation.truncated_by == Some(TruncatedBy::Lines) {
            text.push_str(&format!(
                "\n\n[Showing lines {start}-{end} of {}. Full output: {full}]",
                truncation.total_lines
            ));
        } else {
            text.push_str(&format!(
                "\n\n[Showing lines {start}-{end} of {} ({} limit). Full output: {full}]",
                truncation.total_lines,
                format_size(DEFAULT_MAX_BYTES)
            ));
        }
    }
    text
}

fn append_status(text: &str, status: &str) -> String {
    if text.is_empty() {
        status.to_string()
    } else {
        format!("{text}\n\n{status}")
    }
}

#[async_trait]
impl AgentTool for ShellTool {
    fn declaration(&self) -> &Tool {
        &self.declaration
    }

    fn label(&self) -> &str {
        self.name
    }

    async fn execute(&self, invocation: ToolInvocation) -> Result<AgentToolResult, ToolError> {
        let params: ShellParams = serde_json::from_value(invocation.args)
            .map_err(|error| format!("Invalid arguments: {error}"))?;
        let command = match &self.command_prefix {
            Some(prefix) => format!("{prefix}\n{}", params.command),
            None => params.command,
        };
        let timeout = params.timeout.or(self.default_timeout);
        resolve_timeout(timeout)?;
        if params
            .workdir
            .as_ref()
            .is_some_and(|cwd| !cwd.is_absolute())
        {
            return Err("Working directory must be an absolute path".into());
        }
        let cwd = params
            .workdir
            .unwrap_or_else(|| self.context.cwd().unwrap_or_else(|| self.cwd.clone()));
        let spawn = self.spawn_context(command, cwd);

        let output = Arc::new(Mutex::new(OutputAccumulator::new_with_output_dir(
            &self.context.output_file_prefix(self.name),
            self.output_dir.clone(),
        )));
        let throttle = Arc::new(Mutex::new(Some(UpdateThrottle::start(
            output.clone(),
            invocation.updates.clone(),
            spawn.cwd.clone(),
            timeout,
        ))));
        let on_data: OnData = {
            let (output, throttle) = (output.clone(), throttle.clone());
            Arc::new(move |data: &[u8]| {
                output
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .append(data);
                if let Some(throttle) = throttle
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .as_ref()
                {
                    throttle.mark();
                }
            })
        };
        let result = self
            .operations
            .exec(
                &spawn.command,
                &spawn.cwd,
                ExecOptions {
                    on_data,
                    cancel: invocation.cancel.clone(),
                    timeout,
                    env: Some(spawn.env),
                },
            )
            .await;

        let (snapshot, last_line_bytes) = {
            let mut output = output.lock().unwrap_or_else(|error| error.into_inner());
            output.finish();
            (output.snapshot(true), output.last_line_bytes())
        };
        if let Some(throttle) = throttle
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            throttle.finish();
        }
        // The kept output reaches the model either way; a failed save loses only the
        // rest, and the note says the file could not be saved.
        let _ = output
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .close_file();

        let exit_code = match result {
            Ok(code) => code,
            Err(error) => {
                let text = format_output(&snapshot, "", last_line_bytes);
                let status = match error {
                    ExecError::Aborted => "Command aborted".to_string(),
                    ExecError::TimedOut(seconds) => {
                        format!("Command timed out after {seconds} seconds")
                    }
                    ExecError::Failed(message) => return Err(message.into()),
                };
                return Err(append_status(&text, &status).into());
            }
        };
        let text = format_output(&snapshot, "(no output)", last_line_bytes);
        let details = snapshot_details(&snapshot);
        match exit_code {
            None => Err(append_status(&text, "Command terminated without an exit code").into()),
            Some(0) => Ok(AgentToolResult {
                content: vec![Content::text(text)],
                details,
                ..AgentToolResult::default()
            }),
            Some(code) => Ok(AgentToolResult {
                content: vec![Content::text(append_status(
                    &text,
                    &format!("Command exited with code {code}"),
                ))],
                details,
                is_error: true,
                ..AgentToolResult::default()
            }),
        }
    }
}

#[cfg(test)]
mod tests;
