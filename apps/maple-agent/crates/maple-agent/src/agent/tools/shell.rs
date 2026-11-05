//! Maple's execution policy around Pi's unchanged shell operations.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pi_coding_agent::tools::{BashOperations, ExecError, ExecOptions, LocalShellOperations};

pub(crate) const DEFAULT_TIMEOUT_SECONDS: f64 = 30.0 * 60.0;
const CLEANUP_GRACE: Duration = Duration::from_secs(1);

pub(crate) const GUIDANCE: &str = "Maple shell policy: bash and powershell have a 30-minute default runtime deadline, overriding the generic tool description's no-default behavior. Request a longer timeout explicitly for slow builds. Each command starts in the task's saved folder; cd does not persist between calls. For repository searches, use explicit absolute paths to the intended checkout/worktree. Verify the folder and never guess ../../ from a temporary folder. Avoid recursive searches of filesystem roots, /proc, /sys and /dev. head limits output, not runtime: request a short timeout (normally 30 seconds) for searches and narrow the scope if it expires.";

pub(super) fn bounded(inner: LocalShellOperations) -> Arc<dyn BashOperations> {
    Arc::new(BoundedShell {
        inner: Arc::new(inner),
        default_timeout: DEFAULT_TIMEOUT_SECONDS,
    })
}

struct BoundedShell {
    inner: Arc<dyn BashOperations>,
    default_timeout: f64,
}

pub(crate) fn timeout_seconds(requested: Option<f64>) -> f64 {
    requested.unwrap_or(DEFAULT_TIMEOUT_SECONDS)
}

#[async_trait]
impl BashOperations for BoundedShell {
    async fn exec(
        &self,
        command: &str,
        cwd: &Path,
        mut options: ExecOptions,
    ) -> Result<Option<i32>, ExecError> {
        let seconds = options.timeout.unwrap_or(self.default_timeout);
        let duration = Duration::try_from_secs_f64(seconds)
            .ok()
            .filter(|duration| !duration.is_zero())
            .ok_or_else(|| {
                ExecError::Failed(
                    "Invalid timeout: must be a positive finite number of seconds".into(),
                )
            })?;
        let cancel = options.cancel.clone();
        if cancel.is_cancelled() {
            return Err(ExecError::Aborted);
        }
        let execution_cancel = cancel.child_token();
        options.cancel = execution_cancel.clone();
        options.timeout = Some(seconds);
        // Bound setup and command delivery as well as process waiting. In
        // particular, Pi's legacy stdin transport can block before its timer.
        let execution = async {
            if !cwd.is_absolute() {
                return Err(ExecError::Failed(
                    "Working directory must be an absolute path".into(),
                ));
            }
            if !tokio::fs::metadata(cwd)
                .await
                .is_ok_and(|metadata| metadata.is_dir())
            {
                return Err(ExecError::Failed(
                    "Working directory is unavailable or is not a directory".into(),
                ));
            }
            self.inner.exec(command, cwd, options).await
        };
        tokio::pin!(execution);
        let reason = tokio::select! {
            biased;
            _ = cancel.cancelled() => ExecError::Aborted,
            result = &mut execution => return result,
            _ = tokio::time::sleep(duration) => ExecError::TimedOut(seconds),
        };
        // Let Pi reap the shell and clean up remaining pipe writers normally.
        // If setup/stdin cannot respond, dropping the future invokes Pi's own
        // process-tree guard. Never wait indefinitely for that cooperation.
        execution_cancel.cancel();
        let _ = tokio::time::timeout(CLEANUP_GRACE, &mut execution).await;
        Err(reason)
    }
}

#[cfg(test)]
mod tests;
