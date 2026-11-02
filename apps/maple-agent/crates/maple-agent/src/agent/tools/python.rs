//! `python`: a persistent CPython scratchpad for each task.
//!
//! A task's interpreter starts with its first call and keeps its variables,
//! imports and background asyncio work between calls. The calls of one
//! batch run one at a time, in the order the model made them. It is Maple's
//! bundled CPython (`maple-code-mode`), started in the task's project with
//! the shell's environment: ordinary native Python, not a sandbox.
//!
//! At most `MAX_WORKERS` interpreters run at once. When another task needs
//! one, the least recently used idle scratchpad is discarded, and that task's
//! next result says so. Archiving the task discards it too; unloading the
//! task or stopping Maple retires it. A task loaded again with history starts
//! with a fresh interpreter and is told that earlier state is gone.

use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::fmt::Write;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;

use async_trait::async_trait;
use maple_code_mode::{
    Error, Holder, LaunchSpec, MAX_SOURCE_BYTES, MAX_WORKERS, Outcome, OutcomeStatus,
    PackagedPython, ResetOutcome, Runtime, TaskHandle, WorkerPhase,
};
use pi_agent_core::{AgentTool, AgentToolResult, ToolError, ToolExecutionMode, ToolInvocation};
use pi_ai::Tool;
use pi_coding_agent::extensions::{
    Extension, RegisteredTool, SessionShutdown, ToolPrompt, extension,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_util::sync::CancellationToken;

pub(crate) const PYTHON_TOOL_NAME: &str = "python";
const RESULT_VERSION: u8 = 1;
const MAX_ERROR_BYTES: usize = 8 * 1024;
const RESET_REASON: &str =
    "Python was explicitly reset; previous variables and background work were discarded";
const FRESH_REASON: &str = "This is a fresh Python scratchpad: Maple restarted or reloaded this task, so variables, imports and background work from any earlier python calls are gone";
const MAKE_ROOM_REASON: &str =
    "Python state was discarded to make room for another task's scratchpad";
pub(crate) const ARCHIVED_REASON: &str = "Python state was discarded when this task was archived";
pub(crate) const PROJECT_REMOVED_REASON: &str =
    "Python state was discarded when this task's project was archived or removed";
const UNLOADED_REASON: &str = "This task was unloaded";

type Cleanup = Pin<Box<dyn Future<Output = Result<ResetOutcome, Error>> + Send + 'static>>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PythonParams {
    code: String,
    #[serde(default)]
    reset: bool,
}

impl PythonParams {
    fn validate(&self) -> Result<(), Error> {
        if self.code.len() > MAX_SOURCE_BYTES {
            return Err(Error::InvalidInput("Python code exceeds 256 KiB".into()));
        }
        if self.code.trim().is_empty() && !self.reset {
            return Err(Error::InvalidInput(
                "Python code must not be empty unless reset is true".into(),
            ));
        }
        Ok(())
    }
}

/// Maple's bundled CPython and the scratchpads of the loaded tasks.
pub(crate) struct PythonWorkers {
    runtime: Runtime,
    python: PackagedPython,
    tasks: Mutex<HashMap<String, Weak<TaskPython>>>,
}

impl PythonWorkers {
    /// The CPython bundled beside this executable, or `None` in a build
    /// without one, which then offers no `python` tool.
    pub(crate) fn bundled() -> Option<Arc<Self>> {
        let python = std::env::current_exe()
            .map_err(|error| Error::Unavailable(error.to_string()))
            .and_then(PackagedPython::for_application_executable);
        match python {
            Ok(python) => Some(Self::new(python)),
            Err(error) => {
                log::info!("The python tool is unavailable: {error}");
                None
            }
        }
    }

    pub(crate) fn new(python: PackagedPython) -> Arc<Self> {
        Arc::new(Self {
            runtime: Runtime::new(Default::default()),
            python,
            tasks: Mutex::default(),
        })
    }

    /// The scratchpad of a task being loaded. A task with history is told on
    /// its first call that any earlier state is gone.
    pub(crate) fn task(
        self: &Arc<Self>,
        session_id: &str,
        root: PathBuf,
        login_path: Option<&str>,
        has_history: bool,
        lifetime: CancellationToken,
    ) -> Arc<TaskPython> {
        let task = Arc::new(TaskPython {
            workers: Arc::clone(self),
            key: session_id.to_string(),
            root,
            env: task_environment(std::env::vars_os().collect(), login_path, session_id),
            lifetime,
            state: Mutex::new(BindingState {
                closed: false,
                handle: None,
                state_loss_reason: has_history.then(|| FRESH_REASON.to_string()),
                last_used: Instant::now(),
            }),
        });
        self.tasks
            .lock()
            .unwrap()
            .insert(task.key.clone(), Arc::downgrade(&task));
        task
    }

    /// Discard a loaded task's scratchpad, keeping the tool usable.
    pub(crate) async fn discard(&self, session_id: &str, reason: &str) {
        let task = self
            .tasks
            .lock()
            .unwrap()
            .get(session_id)
            .and_then(Weak::upgrade);
        if let Some(task) = task {
            observe(task.reset(reason.to_string())).await;
        }
    }

    /// Discard the scratchpads of every loaded task in a project.
    pub(crate) async fn discard_project(&self, root: &Path, reason: &str) {
        let tasks = self
            .tasks
            .lock()
            .unwrap()
            .values()
            .filter_map(Weak::upgrade)
            .filter(|task| task.root == root)
            .collect::<Vec<_>>();
        let cleanups = tasks
            .iter()
            .map(|task| observe(task.reset(reason.to_string())));
        futures_util::future::join_all(cleanups).await;
    }

    /// Retire every interpreter; the returned future observes their cleanup.
    pub(crate) async fn shutdown(&self) {
        if let Err(error) = self.runtime.shutdown("Maple is stopping").await {
            log::warn!("Python scratchpads did not finish stopping: {error}");
        }
    }

    /// Free a slot for `requester` by discarding the least recently used
    /// idle scratchpad of another task.
    async fn make_room(&self, requester: &str, holders: &[Holder]) -> bool {
        let candidate = {
            let tasks = self.tasks.lock().unwrap();
            holders
                .iter()
                .filter(|holder| holder.key != requester && holder.phase == WorkerPhase::Idle)
                .filter_map(|holder| tasks.get(&holder.key).and_then(Weak::upgrade))
                .min_by_key(|task| task.last_used())
        };
        match candidate {
            Some(task) => task.reset(MAKE_ROOM_REASON.to_string()).await.is_ok(),
            None => false,
        }
    }
}

async fn observe(cleanup: Cleanup) {
    if let Err(error) = cleanup.await {
        log::warn!("Python scratchpad cleanup did not finish: {error}");
    }
}

struct BindingState {
    closed: bool,
    handle: Option<TaskHandle>,
    state_loss_reason: Option<String>,
    last_used: Instant,
}

/// One task's scratchpad. Python starts lazily on the first execution.
pub(crate) struct TaskPython {
    workers: Arc<PythonWorkers>,
    key: String,
    root: PathBuf,
    env: BTreeMap<OsString, OsString>,
    lifetime: CancellationToken,
    state: Mutex<BindingState>,
}

impl Drop for TaskPython {
    fn drop(&mut self) {
        // Retirement fences admission synchronously; the runtime keeps
        // ownership of the cleanup the dropped future would have observed.
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(handle) = state.handle.take() {
            drop(handle.retire(UNLOADED_REASON));
        }
    }
}

impl TaskPython {
    fn last_used(&self) -> Instant {
        self.state.lock().unwrap().last_used
    }

    /// Close the scratchpad for good when its session shuts down.
    async fn retire(&self, reason: &str) {
        let cleanup = {
            let mut state = self.state.lock().unwrap();
            state.closed = true;
            state.handle.take().map(|handle| handle.retire(reason))
        };
        self.lifetime.cancel();
        if let Some(cleanup) = cleanup
            && let Err(error) = cleanup.await
        {
            log::warn!("Python scratchpad cleanup did not finish: {error}");
        }
    }

    /// End the current interpreter; the next call starts a fresh one and
    /// reports `reason` when earlier state existed. Admission is fenced
    /// before this returns; the future observes the cleanup.
    fn reset(&self, reason: String) -> Cleanup {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return Box::pin(async { Err(Error::Retired) });
        }
        let Some(handle) = state.handle.clone() else {
            return Box::pin(async {
                Ok(ResetOutcome {
                    retired_generation: None,
                })
            });
        };
        let reason = bound_error(reason);
        if handle.status().generation.is_some() {
            state.state_loss_reason = Some(reason.clone());
        }
        handle.reset(reason)
    }

    async fn call(&self, params: PythonParams, cancel: CancellationToken) -> AgentToolResult {
        match self.invoke(params, cancel).await {
            Ok(response) => response.into_result(),
            Err(Error::Capacity { .. }) => python_error(format!(
                "All {MAX_WORKERS} Python scratchpads are running cells in other tasks; try again when one finishes."
            )),
            Err(error) => python_error(error.to_string()),
        }
    }

    async fn invoke(
        &self,
        params: PythonParams,
        cancel: CancellationToken,
    ) -> Result<PythonResponse, Error> {
        params.validate()?;
        if self.state.lock().unwrap().closed {
            return Err(Error::Retired);
        }
        if params.reset {
            let reset = self.reset(RESET_REASON.into()).await?;
            if params.code.trim().is_empty() {
                return Ok(PythonResponse::Reset(reset));
            }
        }
        let mut outcome = match self.execute(&params.code, &cancel).await {
            Err(Error::Capacity { holders })
                if self.workers.make_room(&self.key, &holders).await =>
            {
                self.execute(&params.code, &cancel).await
            }
            result => result,
        }?;
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return Err(Error::Retired);
        }
        state.last_used = Instant::now();
        if outcome.state_loss_reason.is_none() {
            outcome.state_loss_reason = state.state_loss_reason.take();
        } else {
            state.state_loss_reason = None;
        }
        Ok(PythonResponse::Execution(Box::new(outcome)))
    }

    async fn execute(&self, code: &str, cancel: &CancellationToken) -> Result<Outcome, Error> {
        let execution = {
            let mut state = self.state.lock().unwrap();
            if state.closed {
                return Err(Error::Retired);
            }
            state.last_used = Instant::now();
            if state.handle.is_none() {
                state.handle = Some(self.workers.runtime.bind(
                    self.key.clone(),
                    LaunchSpec {
                        python: self.workers.python.clone(),
                        cwd: self.root.clone(),
                        env: self.env.clone(),
                    },
                    self.lifetime.clone(),
                )?);
            }
            state
                .handle
                .as_ref()
                .unwrap()
                .execute(code.to_string(), cancel.clone())
        };
        execution.await
    }
}

/// The tool. Its calls run one at a time because they share one interpreter.
pub(crate) fn python_tool(task: Arc<TaskPython>) -> RegisteredTool {
    RegisteredTool {
        tool: Arc::new(PythonTool {
            declaration: declaration(&task.workers.python.version),
            task,
        }),
        prompt: ToolPrompt {
            snippet: Some("Run Python in this task's persistent scratchpad".to_string()),
            guidelines: vec![
                "Use python for calculations, data processing and multi-step scripts that benefit from keeping state between calls; use the shell for project commands, tests and long-running processes.".to_string(),
            ],
        },
        active: true,
        extension: None,
    }
}

/// Retires the task's interpreter when its session shuts down.
pub(crate) fn python_extension(task: Arc<TaskPython>) -> Arc<dyn Extension> {
    extension("maple-python", move |api| {
        let task = Arc::clone(&task);
        api.on(move |_event: SessionShutdown, _context| {
            let task = Arc::clone(&task);
            async move {
                task.retire(UNLOADED_REASON).await;
                Ok(())
            }
        });
    })
}

struct PythonTool {
    declaration: Tool,
    task: Arc<TaskPython>,
}

#[async_trait]
impl AgentTool for PythonTool {
    fn declaration(&self) -> &Tool {
        &self.declaration
    }

    fn label(&self) -> &str {
        "Python"
    }

    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        Some(ToolExecutionMode::Sequential)
    }

    async fn execute(&self, invocation: ToolInvocation) -> Result<AgentToolResult, ToolError> {
        let params: PythonParams = serde_json::from_value(invocation.args)
            .map_err(|error| format!("Invalid python arguments: {error}"))?;
        Ok(self.task.call(params, invocation.cancel).await)
    }
}

fn declaration(version: &str) -> Tool {
    let minor = version.splitn(3, '.').take(2).collect::<Vec<_>>().join(".");
    let description = format!(
        r#"Run Python in this task's persistent scratchpad: bundled CPython {minor} with its standard library, started in the task's project directory. Variables, functions, imports and background asyncio work persist across calls until the scratchpad is reset, and a result says so when earlier state is gone. A non-None final expression is displayed and kept in `_`. Top-level `await` runs on the scratchpad's event loop: use awaitable APIs and `asyncio.gather` rather than `asyncio.run()`. Python calls run one at a time, in order.

This is ordinary Python with the same machine access as the shell. Run project tests, scripts and CLIs, and anything long-running or interactive, through the shell in the project's own environment: synchronous calls such as `subprocess.run()` pause the scratchpad until they return. Never install into the bundled runtime. To use a package, install a compatible wheel into a directory outside it and add that directory to `sys.path`, e.g. `subprocess.run([sys.executable, "-I", "-B", "-m", "pip", "--isolated", "install", "--only-binary=:all:", "--target", str(deps), "openpyxl"], check=True)`. Process pools need functions from importable `.py` modules, not functions defined in a cell.

Errors keep partial assignments and external effects. Output is bounded; write large results to files. Stopping a running cell discards the scratchpad. Set `reset` to start fresh before running `code`; empty `code` with `reset` only discards it."#
    );
    Tool::new(
        PYTHON_TOOL_NAME,
        description,
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["code"],
            "properties": {
                "code": {
                    "type": "string",
                    "description": "Python source, at most 256 KiB of UTF-8. Empty only with reset."
                },
                "reset": {
                    "type": "boolean",
                    "default": false,
                    "description": "Discard the scratchpad before running code; with empty code, only discard it."
                }
            }
        }),
    )
}

fn env_name_matches(actual: &OsStr, expected: &str) -> bool {
    #[cfg(windows)]
    {
        actual
            .to_str()
            .is_some_and(|actual| actual.eq_ignore_ascii_case(expected))
    }
    #[cfg(not(windows))]
    {
        actual == OsStr::new(expected)
    }
}

fn replace_environment(env: &mut BTreeMap<OsString, OsString>, name: &str, value: &str) {
    env.retain(|key, _| !env_name_matches(key, name));
    env.insert(name.into(), value.into());
}

/// The shell's environment for the task: the login PATH and the task's id.
fn task_environment(
    mut parent: BTreeMap<OsString, OsString>,
    login_path: Option<&str>,
    session_id: &str,
) -> BTreeMap<OsString, OsString> {
    if let Some(path) = login_path {
        replace_environment(&mut parent, "PATH", path);
    }
    replace_environment(&mut parent, "AGENT_SESSION_ID", session_id);
    parent
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PythonResponse {
    Execution(Box<Outcome>),
    Reset(ResetOutcome),
    Error { message: String },
}

impl PythonResponse {
    fn into_result(self) -> AgentToolResult {
        let (text, is_error) = match &self {
            Self::Execution(outcome) => {
                (format_outcome(outcome), outcome.status != OutcomeStatus::Ok)
            }
            Self::Reset(reset) => (
                match reset.retired_generation {
                    Some(generation) => format!(
                        "Python generation {generation} was reset after cleanup. No replacement interpreter was started."
                    ),
                    None => "Python is already empty. No interpreter was started.".into(),
                },
                false,
            ),
            Self::Error { message } => (format!("Python error: {message}"), true),
        };
        let mut body = serde_json::to_value(self).expect("Python outcomes are JSON values");
        body.as_object_mut()
            .unwrap()
            .insert("version".into(), RESULT_VERSION.into());
        let result = if is_error {
            AgentToolResult::error(text)
        } else {
            AgentToolResult::text(text)
        };
        result.with_details(json!({ "maple_python": body }))
    }
}

fn python_error(message: impl Into<String>) -> AgentToolResult {
    PythonResponse::Error {
        message: bound_error(message.into()),
    }
    .into_result()
}

fn bound_error(mut message: String) -> String {
    if message.len() > MAX_ERROR_BYTES {
        let mut end = MAX_ERROR_BYTES;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
        message.push_str("\n[error text truncated]");
    }
    message
}

fn format_outcome(outcome: &Outcome) -> String {
    let status = match outcome.status {
        OutcomeStatus::Ok => "completed",
        OutcomeStatus::Error => "error",
        OutcomeStatus::Cancelled => "cancelled",
        OutcomeStatus::WorkerLost => "worker lost",
    };
    let mut text = format!(
        "Python generation {}, execution {}: {status} ({} ms)",
        outcome.generation, outcome.execution_id, outcome.elapsed_ms
    );
    if let Some(runtime) = &outcome.runtime {
        let _ = write!(
            text,
            "\nCPython {} ({})\nExecutable: {}\nWorking directory: {}",
            runtime.version,
            runtime.distribution,
            runtime.executable.display(),
            runtime.cwd.display()
        );
    }
    for (label, output) in [
        ("stdout", Some(outcome.stdout.as_str())),
        ("stderr", Some(outcome.stderr.as_str())),
        ("value", outcome.value.as_deref()),
        ("traceback", outcome.traceback.as_deref()),
    ] {
        if let Some(output) = output.filter(|output| !output.is_empty()) {
            let _ = write!(text, "\n\n{label}:\n{output}");
        }
    }
    if outcome.dropped_stdout_bytes > 0 || outcome.dropped_stderr_bytes > 0 {
        let _ = write!(
            text,
            "\n\nOutput truncated: {} stdout bytes and {} stderr bytes omitted.",
            outcome.dropped_stdout_bytes, outcome.dropped_stderr_bytes
        );
    }
    for chunk in &outcome.background.chunks {
        let attribution = chunk.execution_id.map_or_else(
            || "unattributed native/subprocess output".into(),
            |id| format!("late output from execution {id}"),
        );
        let _ = write!(
            text,
            "\n\nBackground {} ({attribution}):\n{}",
            chunk.stream, chunk.text
        );
    }
    if outcome.background.dropped_stdout_bytes > 0 || outcome.background.dropped_stderr_bytes > 0 {
        let _ = write!(
            text,
            "\n\nBackground output truncated: {} stdout bytes and {} stderr bytes omitted.",
            outcome.background.dropped_stdout_bytes, outcome.background.dropped_stderr_bytes
        );
    }
    if let Some(reason) = &outcome.state_loss_reason {
        let _ = write!(text, "\n\nState loss: {reason}");
    }
    if outcome.cleanup_pending {
        text.push_str("\n\nPython cleanup is still pending; its capacity slot remains occupied.");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn fixture() -> PackagedPython {
        let manifest = std::env::var_os("MAPLE_CODE_MODE_RUNTIME_MANIFEST")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../target/debug/runtime/python/runtime.json")
            });
        PackagedPython::from_manifest(manifest)
            .expect("prepare the packaged Python fixture with `nix develop -c just python-prepare`")
    }

    fn params(code: &str, reset: bool) -> PythonParams {
        PythonParams {
            code: code.into(),
            reset,
        }
    }

    fn text(result: &AgentToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|content| content.as_text())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn task(
        workers: &Arc<PythonWorkers>,
        id: &str,
        root: &Path,
        has_history: bool,
    ) -> Arc<TaskPython> {
        workers.task(
            id,
            root.to_path_buf(),
            None,
            has_history,
            CancellationToken::new(),
        )
    }

    #[test]
    fn environment_uses_the_login_path_and_the_task_id() {
        let parent = BTreeMap::from([
            (OsString::from("PATH"), OsString::from("/usr/bin")),
            (OsString::from("HOME"), OsString::from("/home/test")),
            (OsString::from("AGENT_SESSION_ID"), OsString::from("stale")),
        ]);
        let env = task_environment(parent.clone(), Some("/login/bin:/usr/bin"), "task-1");
        assert_eq!(env[OsStr::new("PATH")], "/login/bin:/usr/bin");
        assert_eq!(env[OsStr::new("AGENT_SESSION_ID")], "task-1");
        assert_eq!(env[OsStr::new("HOME")], "/home/test");
        let env = task_environment(parent, None, "task-2");
        assert_eq!(env[OsStr::new("PATH")], "/usr/bin");
    }

    #[test]
    fn titles_and_validation_reject_empty_code_without_reset() {
        assert!(params("", false).validate().is_err());
        assert!(params("  \n", false).validate().is_err());
        assert!(params("", true).validate().is_ok());
        let long = "x".repeat(MAX_SOURCE_BYTES + 1);
        assert!(params(&long, false).validate().is_err());
        let declaration = declaration("3.14.7");
        assert_eq!(declaration.name, PYTHON_TOOL_NAME);
        assert!(declaration.description.contains("CPython 3.14 "));
    }

    #[tokio::test]
    async fn reset_only_calls_do_not_start_python() {
        let workers = PythonWorkers::new(fixture());
        let root = tempfile::tempdir().unwrap();
        let task = task(&workers, "reset-only", root.path(), false);
        let result = task.call(params("", true), CancellationToken::new()).await;
        assert!(!result.is_error, "{}", text(&result));
        assert!(text(&result).contains("already empty"));
        assert!(workers.runtime.snapshot().holders.is_empty());
    }

    #[tokio::test]
    async fn state_persists_until_reset_and_reloads_report_lost_state() {
        let workers = PythonWorkers::new(fixture());
        let root = tempfile::tempdir().unwrap();
        let first = task(&workers, "persist", root.path(), false);
        let result = first
            .call(
                params("values = [2, 3, 5]\nsum(values)", false),
                CancellationToken::new(),
            )
            .await;
        assert!(!result.is_error, "{}", text(&result));
        assert!(text(&result).contains("value:\n10"));
        assert!(text(&result).contains("Working directory"));
        let result = first
            .call(
                params("values.append(7)\nlen(values)", false),
                CancellationToken::new(),
            )
            .await;
        assert!(text(&result).contains("value:\n4"), "{}", text(&result));
        assert!(!text(&result).contains("State loss"));

        let result = first
            .call(
                params("'values' in globals()", true),
                CancellationToken::new(),
            )
            .await;
        assert!(text(&result).contains("value:\nFalse"), "{}", text(&result));
        assert!(text(&result).contains(RESET_REASON));

        // Unloading retires the interpreter; a reload with history says so once.
        first.retire(UNLOADED_REASON).await;
        assert!(
            first
                .call(params("1", false), CancellationToken::new())
                .await
                .is_error
        );
        let reloaded = task(&workers, "persist", root.path(), true);
        let result = reloaded
            .call(params("1 + 1", false), CancellationToken::new())
            .await;
        assert!(text(&result).contains(FRESH_REASON), "{}", text(&result));
        let result = reloaded
            .call(params("2 + 2", false), CancellationToken::new())
            .await;
        assert!(!text(&result).contains("State loss"));
        reloaded.retire(UNLOADED_REASON).await;
        workers.shutdown().await;
    }

    #[tokio::test]
    async fn a_full_runtime_discards_the_least_recently_used_idle_scratchpad() {
        let workers = PythonWorkers::new(fixture());
        let root = tempfile::tempdir().unwrap();
        let tasks = (0..=MAX_WORKERS)
            .map(|index| task(&workers, &format!("task-{index}"), root.path(), false))
            .collect::<Vec<_>>();
        for (index, task) in tasks.iter().take(MAX_WORKERS).enumerate() {
            let result = task
                .call(
                    params(&format!("marker = {index}"), false),
                    CancellationToken::new(),
                )
                .await;
            assert!(!result.is_error, "{}", text(&result));
        }
        // Touch task-0 so task-1 is the least recently used.
        tasks[0]
            .call(params("marker", false), CancellationToken::new())
            .await;
        let result = tasks[MAX_WORKERS]
            .call(params("'room'", false), CancellationToken::new())
            .await;
        assert!(!result.is_error, "{}", text(&result));
        let result = tasks[1]
            .call(
                params("'marker' in globals()", false),
                CancellationToken::new(),
            )
            .await;
        assert!(text(&result).contains("value:\nFalse"), "{}", text(&result));
        assert!(
            text(&result).contains(MAKE_ROOM_REASON),
            "{}",
            text(&result)
        );
        let result = tasks[0]
            .call(params("marker", false), CancellationToken::new())
            .await;
        assert!(text(&result).contains("value:\n0"), "{}", text(&result));
        for task in &tasks {
            task.retire(UNLOADED_REASON).await;
        }
        workers.shutdown().await;
    }

    #[tokio::test]
    async fn discarding_an_archived_task_reports_lost_state() {
        let workers = PythonWorkers::new(fixture());
        let root = tempfile::tempdir().unwrap();
        let task = task(&workers, "archived", root.path(), false);
        task.call(params("kept = 1", false), CancellationToken::new())
            .await;
        workers.discard("archived", ARCHIVED_REASON).await;
        let result = task
            .call(
                params("'kept' in globals()", false),
                CancellationToken::new(),
            )
            .await;
        assert!(text(&result).contains("value:\nFalse"), "{}", text(&result));
        assert!(text(&result).contains(ARCHIVED_REASON), "{}", text(&result));
        task.retire(UNLOADED_REASON).await;
        workers.shutdown().await;
    }
}
