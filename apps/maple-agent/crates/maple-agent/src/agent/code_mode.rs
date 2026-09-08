//! Maple's task capability for the GPUI-free Python runtime.
//!
//! A binding is installed before asynchronous agent preparation. Retirement
//! closes this capability immediately, even if its first call is still repairing
//! PATH and has not created a runtime handle yet.
#[cfg(test)]
mod ordered_tests;

use super::image_mediation::prioritized_text;
use super::tool_context::{AgentToolContextSnapshot, SENSITIVE_BRIDGE_ENV, SharedAgentToolContext};
use goose::agents::ToolCallContext;
use maple_code_mode::{
    Error, Holder, LaunchSpec, Outcome, OutcomeStatus, PackagedPython, ResetOutcome, Runtime,
    TaskHandle, TaskStatus, WorkerPhase,
};
use rmcp::model::{CallToolResult, Tool, ToolAnnotations};
use rmcp::object;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt::Write;
use std::future::Future;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

pub(super) const PYTHON_TOOL_NAME: &str = "python_code";
pub(super) const MAX_PYTHON_BATCH_CALLS: NonZeroUsize = NonZeroUsize::new(32).unwrap();
const RESULT_VERSION: u8 = 1;
const MAX_ERROR_BYTES: usize = 8 * 1024;
const RESET_REASON: &str =
    "Python was explicitly reset; previous variables and background work were discarded";

pub(super) type PythonCleanup =
    Pin<Box<dyn Future<Output = Result<ResetOutcome, Error>> + Send + 'static>>;
pub(super) type CapacityDiagnostics = Arc<
    dyn Fn(Vec<Holder>) -> Pin<Box<dyn Future<Output = String> + Send + 'static>> + Send + Sync,
>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PythonParams {
    pub code: String,
    #[serde(default)]
    pub reset: bool,
}

impl PythonParams {
    fn validate(&self) -> Result<(), Error> {
        if self.code.len() > maple_code_mode::MAX_SOURCE_BYTES {
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

pub(super) fn python_tool() -> Tool {
    Tool::new(
        PYTHON_TOOL_NAME.to_string(),
        r#"Execute Python in this task's persistent bundled CPython scratchpad. Variables, functions, imports, and background asyncio work survive calls and model turns. A non-None final expression is displayed and retained in _. Use top-level await; a final expression that merely returns an awaitable is not automatically awaited. asyncio.run() and synchronous wrappers that start another loop cannot run here; use their awaitable APIs or a separate script through the existing execution tools.

Python calls in one tool batch execute one at a time in the order submitted, after those Python calls' permissions are resolved. Up to 32 approved calls are admitted per batch; excess calls fail without execution. Denied calls are skipped. Use asyncio.gather within a cell for concurrent async work. Other tools can run concurrently with Python; do not rely on them finishing before a Python cell.

The bundle guarantees the Python standard library, not project packages or a writable shared installation. The task root is available for explicit project imports. Inspect sys.executable and explicitly add a known compatible dependency directory to sys.path when needed. Creating or activating a venv in a shell does not retarget this retained worker. There is no Maple Python SDK yet.

Errors preserve partial assignments and external effects. Output is bounded and truncation is reported; use files for large results. Synchronous blocking calls are allowed but pause other asyncio work for their duration. Stop retires an unfinished Python cell and loses its namespace; force termination can skip cleanup handlers and lose buffered output. Stop after a settled cell preserves its namespace and background work. Reset explicitly ends retained Python work. Set reset=true to start fresh before supplied code, or pass code="" with reset=true to release this task's worker without starting another. App restart and task/owner retirement also discard state. This is ordinary native Python execution with the same machine access as other execution tools."#,
        object!({
            "type": "object",
            "additionalProperties": false,
            "required": ["code"],
            "properties": {
                "code": {
                    "type": "string",
                    "description": "Python source, at most 256 KiB of UTF-8. Empty only for reset=true."
                },
                "reset": {
                    "type": "boolean",
                    "default": false,
                    "description": "Retire the current interpreter before running code; empty code releases it without a replacement."
                }
            }
        }),
    )
    .annotate(ToolAnnotations::from_raw(
        Some("Python".to_string()),
        Some(false),
        Some(true),
        Some(false),
        Some(true),
    ))
}

#[derive(Default)]
struct BindingState {
    closed: bool,
    handle: Option<TaskHandle>,
    state_loss_reason: Option<String>,
}

pub(super) struct PythonTaskBinding {
    runtime: Runtime,
    key: String,
    session_id: String,
    root: PathBuf,
    context: SharedAgentToolContext,
    state: Mutex<BindingState>,
    capacity_diagnostics: Option<CapacityDiagnostics>,
    #[cfg(test)]
    test_package: Option<PackagedPython>,
}

impl PythonTaskBinding {
    /// Installs authority only. Package reads, PATH repair, binding and spawn
    /// happen lazily in an approved, non-reset-only invocation.
    pub(super) fn new(
        runtime: Runtime,
        key: String,
        session_id: String,
        root: PathBuf,
        context: SharedAgentToolContext,
    ) -> Self {
        Self {
            runtime,
            key,
            session_id,
            root,
            context,
            state: Mutex::new(BindingState::default()),
            capacity_diagnostics: None,
            #[cfg(test)]
            test_package: None,
        }
    }

    pub(super) fn with_capacity_diagnostics(mut self, diagnostics: CapacityDiagnostics) -> Self {
        self.capacity_diagnostics = Some(diagnostics);
        self
    }

    pub(super) fn with_state_loss_reason(self, reason: Option<String>) -> Self {
        self.state.lock().unwrap().state_loss_reason = reason.map(bound_error);
        self
    }

    #[cfg(test)]
    pub(super) fn with_packaged_python(mut self, package: PackagedPython) -> Self {
        self.test_package = Some(package);
        self
    }

    pub(super) fn key(&self) -> &str {
        &self.key
    }

    pub(super) fn matches_root(&self, root: &Path) -> bool {
        self.root == root
    }

    pub(super) fn is_closed(&self) -> bool {
        self.state.lock().unwrap().closed || self.context.is_revoked()
    }

    pub(super) fn status(&self) -> TaskStatus {
        let state = self.state.lock().unwrap();
        let mut status = state
            .handle
            .as_ref()
            .map(TaskHandle::status)
            .unwrap_or(TaskStatus {
                generation: None,
                phase: WorkerPhase::Empty,
                closed: false,
                state_loss_reason: None,
            });
        status.closed |= state.closed || self.context.is_revoked();
        if status.state_loss_reason.is_none() {
            status
                .state_loss_reason
                .clone_from(&state.state_loss_reason);
        }
        status
    }

    /// Closes even an unstarted capability synchronously. The runtime retains
    /// cleanup ownership if the observation future is dropped or times out.
    pub(super) fn retire(&self, reason: impl Into<String>) -> PythonCleanup {
        let mut state = self.state.lock().unwrap();
        let reason = bound_error(reason.into());
        state.closed = true;
        state.state_loss_reason = Some(reason.clone());
        state.handle.as_ref().map_or_else(
            || {
                Box::pin(async {
                    Ok(ResetOutcome {
                        retired_generation: None,
                    })
                }) as PythonCleanup
            },
            |handle| handle.retire(reason),
        )
    }

    /// Menu reset has already checked current account/run/lease authority.
    /// The context launch fence also excludes a concurrently revoked owner.
    pub(super) fn reset(&self, reason: impl Into<String>) -> PythonCleanup {
        self.reset_for_call(reason.into(), &CancellationToken::new())
    }

    fn reset_for_call(&self, reason: String, run: &CancellationToken) -> PythonCleanup {
        let mut state = self.state.lock().unwrap();
        let snapshot = self.context.snapshot();
        let _launch = match snapshot.begin_process_launch(run) {
            Ok(guard) if !state.closed => guard,
            Ok(_) => return Box::pin(async { Err(Error::Retired) }),
            Err(error) => return Box::pin(async move { Err(Error::Unavailable(error)) }),
        };
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

    fn validate_call(
        &self,
        state: &BindingState,
        ctx: &ToolCallContext,
        run: &CancellationToken,
    ) -> Result<(), Error> {
        if state.closed || self.context.is_revoked() {
            return Err(Error::Retired);
        }
        if run.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if ctx.session_id != self.session_id {
            return Err(Error::InvalidInput(
                "Python capability does not belong to this task".into(),
            ));
        }
        if ctx
            .working_dir
            .as_deref()
            .is_some_and(|root| !self.matches_root(root))
        {
            return Err(Error::LaunchMismatch);
        }
        Ok(())
    }

    pub(super) async fn call(
        &self,
        params: PythonParams,
        ctx: &ToolCallContext,
        login_path: impl Future<Output = Option<String>> + Send,
        run: CancellationToken,
    ) -> CallToolResult {
        match self.invoke(params, ctx, login_path, run).await {
            Ok(response) => response.into_result(),
            Err(Error::Capacity { holders }) => {
                let message = match &self.capacity_diagnostics {
                    Some(diagnostics) => diagnostics(holders).await,
                    None => "All four Python worker slots are occupied. Use Reset Python on an idle Maple task, or close the owning ACP session or connection. A reset-only call {\"code\":\"\",\"reset\":true} releases the worker in that session after cleanup; reset with code keeps a slot occupied.".into(),
                };
                python_error(message)
            }
            Err(error) => python_error(error.to_string()),
        }
    }

    async fn invoke(
        &self,
        params: PythonParams,
        ctx: &ToolCallContext,
        login_path: impl Future<Output = Option<String>> + Send,
        run: CancellationToken,
    ) -> Result<PythonResponse, Error> {
        params.validate()?;
        {
            let state = self.state.lock().unwrap();
            self.validate_call(&state, ctx, &run)?;
        }
        if params.reset {
            let reset = self.reset_for_call(RESET_REASON.into(), &run).await?;
            self.validate_call(&self.state.lock().unwrap(), ctx, &run)?;
            if params.code.trim().is_empty() {
                return Ok(PythonResponse::Reset(reset));
            }
        }

        let needs_launch = self.state.lock().unwrap().handle.is_none();
        let prepared = if needs_launch {
            // Both futures start only after permission dispatch. Neither runs
            // under lifecycle, logical-binding or context launch locks.
            let (package, login_path) = tokio::join!(self.resolve_package(), login_path);
            let snapshot = self.context.snapshot();
            let env = project_environment(
                std::env::vars_os().collect(),
                login_path.as_deref(),
                &snapshot,
                &self.session_id,
            );
            Some((package?, env))
        } else {
            None
        };

        let execution = {
            let mut state = self.state.lock().unwrap();
            self.validate_call(&state, ctx, &run)?;
            let snapshot = self.context.snapshot();
            let launch_guard = snapshot
                .begin_process_launch(&run)
                .map_err(Error::Unavailable)?;
            if state.handle.is_none() {
                let (python, env) = prepared.ok_or_else(|| {
                    Error::Unavailable("Python launch configuration was not prepared".into())
                })?;
                state.handle = Some(self.runtime.bind(
                    self.key.clone(),
                    LaunchSpec {
                        python,
                        cwd: self.root.clone(),
                        env,
                    },
                    self.context.lifetime_token(),
                )?);
            }
            state
                .handle
                .as_ref()
                .unwrap()
                .execute_guarded(params.code, run, || Ok(launch_guard))
        };
        let mut outcome = execution.await?;
        {
            let mut state = self.state.lock().unwrap();
            // A completed run token may be cancelled later without revoking
            // its settled worker. Only owner retirement suppresses its result.
            if state.closed || self.context.is_revoked() {
                return Err(Error::Retired);
            }
            if outcome.state_loss_reason.is_none() {
                outcome.state_loss_reason = state.state_loss_reason.take();
            } else {
                state.state_loss_reason = None;
            }
        }
        Ok(PythonResponse::Execution(Box::new(outcome)))
    }

    async fn resolve_package(&self) -> Result<PackagedPython, Error> {
        #[cfg(test)]
        if let Some(package) = &self.test_package {
            return Ok(package.clone());
        }
        tokio::task::spawn_blocking(|| {
            let executable = std::env::current_exe().map_err(|error| {
                Error::Unavailable(format!("Could not locate Maple's bundled Python: {error}"))
            })?;
            PackagedPython::for_application_executable(executable)
        })
        .await
        .map_err(|error| {
            Error::Unavailable(format!(
                "Could not read Maple's Python installation: {error}"
            ))
        })?
    }
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

fn project_environment(
    mut parent: BTreeMap<OsString, OsString>,
    login_path: Option<&str>,
    context: &AgentToolContextSnapshot,
    session_id: &str,
) -> BTreeMap<OsString, OsString> {
    if let Some(path) = login_path {
        replace_environment(&mut parent, "PATH", path);
    }
    parent.retain(|key, _| {
        !context
            .scrub_from_parent
            .iter()
            .any(|name| env_name_matches(key, name))
            && !SENSITIVE_BRIDGE_ENV
                .iter()
                .any(|name| env_name_matches(key, name))
    });
    for (key, value) in &context.values {
        if !SENSITIVE_BRIDGE_ENV
            .iter()
            .any(|name| env_name_matches(OsStr::new(key), name))
        {
            replace_environment(&mut parent, key, value);
        }
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
    fn into_result(self) -> CallToolResult {
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
        let mut result = if is_error {
            CallToolResult::error(vec![prioritized_text(text)])
        } else {
            CallToolResult::success(vec![prioritized_text(text)])
        };
        let mut body = serde_json::to_value(self).expect("Python outcomes are JSON values");
        body.as_object_mut()
            .unwrap()
            .insert("version".into(), RESULT_VERSION.into());
        result.structured_content = Some(serde_json::json!({ "maple_python": body }));
        result
    }
}

pub(super) fn python_error(message: impl Into<String>) -> CallToolResult {
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
    use crate::agent::AgentToolContextSpec;
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn context(values: &[(&str, &str)], scrub: &[&str]) -> SharedAgentToolContext {
        SharedAgentToolContext::new(
            AgentToolContextSpec::try_new(
                values
                    .iter()
                    .map(|(key, value)| ((*key).into(), (*value).into()))
                    .collect(),
                scrub
                    .iter()
                    .map(|key| (*key).into())
                    .collect::<BTreeSet<_>>(),
                true,
            )
            .unwrap(),
        )
    }

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

    fn binding(
        runtime: Runtime,
        root: &Path,
        context: SharedAgentToolContext,
    ) -> PythonTaskBinding {
        PythonTaskBinding::new(
            runtime,
            "test-task".into(),
            "session".into(),
            root.to_path_buf(),
            context,
        )
    }

    fn call_context(root: &Path) -> ToolCallContext {
        ToolCallContext::new("session".into(), Some(root.to_path_buf()), None)
    }

    fn params(code: &str, reset: bool) -> PythonParams {
        PythonParams {
            code: code.into(),
            reset,
        }
    }

    fn body(result: &CallToolResult) -> &serde_json::Value {
        &result.structured_content.as_ref().unwrap()["maple_python"]
    }

    #[test]
    fn environment_projection_scrubs_only_named_keys_and_keeps_context_authority() {
        let context = context(
            &[
                ("PATH", "explicit-path"),
                ("CUSTOM_VALUE", "custom"),
                ("BUZZ_RELAY_URL", "supplied-relay"),
                ("BUZZ_PRIVATE_KEY", "supplied-key"),
                ("BUZZ_AUTH_TAG", "supplied-tag"),
                ("BUZZ_API_TOKEN", "supplied-token"),
                ("BUZZ_ACP_DISPLAY_NAME", "supplied-name"),
                ("AGENT_SESSION_ID", "untrusted-session"),
            ],
            &["OLD_TOKEN"],
        );
        let snapshot = context.snapshot();
        let mut parent = BTreeMap::from([
            (OsString::from("PATH"), OsString::from("parent-path")),
            ("AGENT_SESSION_ID".into(), "parent-session".into()),
            ("OLD_TOKEN".into(), "old".into()),
            ("OTHER_TOKEN".into(), "ordinary-custom".into()),
        ]);
        for key in SENSITIVE_BRIDGE_ENV {
            parent.insert(key.into(), "ambient-value".into());
        }
        let env = project_environment(parent, Some("login-path"), &snapshot, "trusted-session");
        assert_eq!(
            env.get(OsStr::new("PATH")),
            Some(&OsString::from("explicit-path"))
        );
        assert_eq!(
            env.get(OsStr::new("AGENT_SESSION_ID")),
            Some(&OsString::from("trusted-session"))
        );
        assert_eq!(
            env.get(OsStr::new("CUSTOM_VALUE")),
            Some(&OsString::from("custom"))
        );
        assert_eq!(
            env.get(OsStr::new("OTHER_TOKEN")),
            Some(&OsString::from("ordinary-custom"))
        );
        assert!(!env.contains_key(OsStr::new("OLD_TOKEN")));
        for key in SENSITIVE_BRIDGE_ENV {
            assert!(!env.contains_key(OsStr::new(key)));
        }
        assert!(snapshot.ephemeral);
        assert!(!context.is_revoked());
        context.revoke();
        assert!(snapshot.revoked.is_cancelled());
    }

    #[test]
    fn path_repair_precedes_context_scrubbing_and_platform_name_matching() {
        let context = context(&[("CUSTOM", "replacement")], &["PATH"]);
        let parent = BTreeMap::from([
            (OsString::from("Path"), OsString::from("ambient")),
            ("Custom".into(), "old-custom".into()),
            ("buzz_api_token".into(), "ambient-buzz".into()),
            ("agent_session_id".into(), "ambient-session".into()),
        ]);
        let env = project_environment(parent, Some("repaired"), &context.snapshot(), "trusted");
        assert!(!env.contains_key(OsStr::new("PATH")));
        assert_eq!(
            env.get(OsStr::new("AGENT_SESSION_ID")),
            Some(&OsString::from("trusted"))
        );
        assert_eq!(
            env.get(OsStr::new("CUSTOM")),
            Some(&OsString::from("replacement"))
        );
        for key in ["Path", "Custom", "buzz_api_token", "agent_session_id"] {
            assert_eq!(env.contains_key(OsStr::new(key)), !cfg!(windows));
        }
    }

    #[tokio::test]
    async fn reset_only_and_rejected_calls_do_not_prepare_python_or_path() {
        let root = tempfile::tempdir().unwrap();
        let context = context(&[], &[]);
        let runtime = Runtime::default();
        let binding = binding(runtime.clone(), root.path(), context.clone());
        let calls = AtomicUsize::new(0);
        for parameters in [params("", true), params("", true), params("", false)] {
            let result = binding
                .call(
                    parameters,
                    &call_context(root.path()),
                    async {
                        calls.fetch_add(1, Ordering::SeqCst);
                        panic!("reset-only and invalid calls must not prepare PATH")
                    },
                    CancellationToken::new(),
                )
                .await;
            assert_eq!(body(&result)["version"], RESULT_VERSION);
        }
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let result = binding
            .call(
                params("1 + 1", false),
                &call_context(root.path()),
                async { panic!("cancelled calls must not prepare PATH") },
                cancelled,
            )
            .await;
        assert!(result.is_error.unwrap());
        context.revoke();
        let result = binding
            .call(
                params("", true),
                &call_context(root.path()),
                async { panic!("revoked calls must not prepare PATH") },
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error.unwrap());
        assert!(runtime.snapshot().holders.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        binding.retire("revoked test owner").await.unwrap();
        runtime.shutdown("test complete").await.unwrap();
    }

    #[tokio::test]
    async fn archive_fences_a_call_while_its_path_probe_is_pending() {
        let root = tempfile::tempdir().unwrap();
        let runtime = Runtime::default();
        let binding = Arc::new(
            binding(runtime.clone(), root.path(), context(&[], &[]))
                .with_packaged_python(fixture()),
        );
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let invocation_binding = binding.clone();
        let ctx = call_context(root.path());
        let invocation = tokio::spawn(async move {
            invocation_binding
                .call(
                    params("must_not_exist = True", false),
                    &ctx,
                    async {
                        started_tx.send(()).unwrap();
                        release_rx.await.unwrap();
                        None
                    },
                    CancellationToken::new(),
                )
                .await
        });
        started_rx.await.unwrap();
        let cleanup = binding.retire("task was archived");
        assert!(binding.is_closed());
        release_tx.send(()).unwrap();
        let result = invocation.await.unwrap();
        assert!(result.is_error.unwrap());
        assert_eq!(body(&result)["kind"], "error");
        cleanup.await.unwrap();
        assert!(runtime.snapshot().holders.is_empty());
        runtime.shutdown("test complete").await.unwrap();
    }

    #[tokio::test]
    async fn native_binding_persists_across_calls_and_unrelated_stop_then_resets() {
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        let runtime = Runtime::default();
        let context = context(
            &[
                ("CUSTOM_VALUE", "supplied"),
                ("BUZZ_API_TOKEN", "must-not-reach-python"),
            ],
            &[],
        );
        let binding = binding(runtime.clone(), &root, context).with_packaged_python(fixture());
        let ctx = call_context(&root);
        let first_run = CancellationToken::new();
        let first = binding.call(params("import os\nanswer = 40\nassert os.getenv('CUSTOM_VALUE') == 'supplied'\nassert os.getenv('BUZZ_API_TOKEN') is None\nassert os.getenv('AGENT_SESSION_ID') == 'session'\nanswer", false), &ctx, std::future::ready(None), first_run.clone()).await;
        assert!(!first.is_error.unwrap_or(false), "{first:?}");
        assert_eq!(body(&first)["value"], "40");
        let generation = body(&first)["generation"].clone();
        first_run.cancel();
        let second = binding
            .call(
                params("answer + 2", false),
                &ctx,
                async { panic!("retained worker must not probe PATH again") },
                CancellationToken::new(),
            )
            .await;
        assert!(!second.is_error.unwrap_or(false), "{second:?}");
        assert_eq!(body(&second)["generation"], generation);
        assert_eq!(body(&second)["value"], "42");
        let reset = binding
            .call(
                params("", true),
                &ctx,
                async { panic!("reset-only must not probe PATH") },
                CancellationToken::new(),
            )
            .await;
        assert_eq!(body(&reset)["kind"], "reset");
        assert_eq!(body(&reset)["retired_generation"], generation);
        assert!(runtime.snapshot().holders.is_empty());
        let third = binding
            .call(
                params("'answer' in globals()", false),
                &ctx,
                async { panic!("immutable retained launch configuration needs no new probe") },
                CancellationToken::new(),
            )
            .await;
        assert!(!third.is_error.unwrap_or(false), "{third:?}");
        assert_ne!(body(&third)["generation"], generation);
        assert_eq!(body(&third)["value"], "False");
        assert!(
            body(&third)["state_loss_reason"]
                .as_str()
                .unwrap()
                .contains("reset")
        );
        binding.retire("test complete").await.unwrap();
        runtime.shutdown("test complete").await.unwrap();
    }

    #[tokio::test]
    async fn mismatched_task_and_root_are_rejected_before_preparation() {
        let root = tempfile::tempdir().unwrap();
        let runtime = Runtime::default();
        let binding = binding(runtime.clone(), root.path(), context(&[], &[]));
        for ctx in [
            ToolCallContext::new(
                "other-session".into(),
                Some(root.path().to_path_buf()),
                None,
            ),
            ToolCallContext::new("session".into(), Some(root.path().join("other-root")), None),
        ] {
            let result = binding
                .call(
                    params("1", false),
                    &ctx,
                    async { panic!("mismatched capabilities must not prepare PATH") },
                    CancellationToken::new(),
                )
                .await;
            assert!(result.is_error.unwrap());
        }
        assert!(runtime.snapshot().holders.is_empty());
        runtime.shutdown("test complete").await.unwrap();
    }
}
