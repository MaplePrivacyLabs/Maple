use super::*;
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio_util::sync::CancellationToken;

fn options() -> ExecOptions {
    ExecOptions {
        on_data: Arc::new(|_| {}),
        cancel: CancellationToken::new(),
        timeout: None,
        env: None,
    }
}

#[tokio::test]
async fn unavailable_or_relative_folders_fail_before_execution() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("file"), "fixture").unwrap();
    let shell = bounded(LocalShellOperations::bash(None));
    for cwd in [
        dir.path().join("missing"),
        dir.path().join("file"),
        "../../".into(),
    ] {
        assert!(matches!(
            shell.exec("must-not-execute", &cwd, options()).await,
            Err(ExecError::Failed(_))
        ));
    }
}

struct InspectTimeout;
#[async_trait]
impl BashOperations for InspectTimeout {
    async fn exec(
        &self,
        _: &str,
        _: &Path,
        options: ExecOptions,
    ) -> Result<Option<i32>, ExecError> {
        assert_eq!(options.timeout, Some(DEFAULT_TIMEOUT_SECONDS));
        Ok(Some(0))
    }
}

#[tokio::test]
async fn maple_supplies_a_default_deadline_to_the_existing_operations_interface() {
    let dir = tempfile::tempdir().unwrap();
    let shell = BoundedShell {
        inner: Arc::new(InspectTimeout),
        default_timeout: DEFAULT_TIMEOUT_SECONDS,
    };
    assert_eq!(
        shell.exec("fixture", dir.path(), options()).await.unwrap(),
        Some(0)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn explicit_deadlines_override_the_default_and_calls_keep_the_saved_folder() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("window.rs"), "fixture").unwrap();
    let shell = BoundedShell {
        inner: Arc::new(LocalShellOperations::bash(None)),
        default_timeout: 0.05,
    };
    assert_eq!(
        shell.exec("sleep 30", dir.path(), options()).await,
        Err(ExecError::TimedOut(0.05))
    );
    assert_eq!(
        shell
            .exec(
                "sleep 0.1",
                dir.path(),
                ExecOptions {
                    timeout: Some(5.0),
                    ..options()
                }
            )
            .await
            .unwrap(),
        Some(0)
    );
    shell.exec("cd ..", dir.path(), options()).await.unwrap();
    assert_eq!(
        shell
            .exec("test -f ./window.rs", dir.path(), options())
            .await
            .unwrap(),
        Some(0)
    );
}

struct BlockedDelivery(Arc<AtomicBool>, Arc<tokio::sync::Notify>);
struct Dropped(Arc<AtomicBool>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
#[async_trait]
impl BashOperations for BlockedDelivery {
    async fn exec(&self, _: &str, _: &Path, _: ExecOptions) -> Result<Option<i32>, ExecError> {
        let _dropped = Dropped(self.0.clone());
        self.1.notify_one();
        // An operation stuck before its own cancellation/deadline select, as
        // with command delivery to a shell that does not read its stdin.
        std::future::pending().await
    }
}

#[tokio::test]
async fn uncooperative_command_delivery_is_bounded_and_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let dropped = Arc::new(AtomicBool::new(false));
    let shell = BoundedShell {
        inner: Arc::new(BlockedDelivery(dropped.clone(), Arc::default())),
        default_timeout: 0.01,
    };
    let parent_cancel = CancellationToken::new();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        shell.exec(
            "fixture",
            dir.path(),
            ExecOptions {
                cancel: parent_cancel.clone(),
                ..options()
            },
        ),
    )
    .await
    .unwrap();
    assert_eq!(result, Err(ExecError::TimedOut(0.01)));
    assert!(dropped.load(Ordering::SeqCst));
    assert!(
        !parent_cancel.is_cancelled(),
        "a command deadline must not cancel its caller"
    );
}

#[tokio::test]
async fn uncooperative_command_delivery_is_cancelled_and_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let dropped = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(tokio::sync::Notify::new());
    let shell = BoundedShell {
        inner: Arc::new(BlockedDelivery(dropped.clone(), entered.clone())),
        default_timeout: DEFAULT_TIMEOUT_SECONDS,
    };
    let cancel = CancellationToken::new();
    let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            shell.exec(
                "fixture",
                dir.path(),
                ExecOptions {
                    cancel: cancel.clone(),
                    ..options()
                }
            ),
            async {
                entered.notified().await;
                cancel.cancel();
            }
        )
    })
    .await
    .unwrap();
    assert_eq!(result, Err(ExecError::Aborted));
    assert!(dropped.load(Ordering::SeqCst));
}

/// A finite, disposable substitute for a kernel file that waits indefinitely for
/// new data. The held writer prevents EOF; neither side of the pipeline produces
/// output, so head cannot bound its runtime.
#[cfg(unix)]
#[tokio::test]
async fn blocked_pipeline_is_stopped_by_cancel_deadline_and_dropped_future() {
    use std::ffi::CString;
    use std::os::unix::fs::OpenOptionsExt;

    for end in ["cancel", "deadline", "drop"] {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("scratch/tmp")).unwrap();
        let fifo = CString::new(dir.path().join("blocked").as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a valid, NUL-terminated path inside a disposable test directory.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let _reader = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(dir.path().join("blocked"))
            .unwrap();
        let _writer = fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(dir.path().join("blocked"))
            .unwrap();
        let tool = Arc::new(BoundedShell {
            inner: Arc::new(LocalShellOperations::bash(None)),
            default_timeout: DEFAULT_TIMEOUT_SECONDS,
        });
        let cwd = dir.path().to_path_buf();
        let cancel = CancellationToken::new();
        let call = tokio::spawn({
            let (tool, cancel) = (tool.clone(), cancel.clone());
            async move {
                tool.exec(
                    "printf '%s' $$ > shell.pid; cd scratch/tmp && sh -c 'printf %s $$ > ../../reader.pid; exec cat ../../blocked' | sh -c 'printf %s $$ > ../../head.pid; exec head -c 1'",
                    &cwd,
                    ExecOptions { timeout: Some(if end == "deadline" { 0.5 } else { 30.0 }), cancel, ..options() },
                ).await
            }
        });
        let mut pids = Vec::new();
        for name in ["shell.pid", "reader.pid", "head.pid"] {
            let pid = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Some(pid) = fs::read_to_string(dir.path().join(name))
                        .ok()
                        .and_then(|text| text.parse::<libc::pid_t>().ok())
                    {
                        break pid;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("pipeline member records its pid");
            pids.push(pid);
        }
        match end {
            "cancel" => cancel.cancel(),
            "drop" => call.abort(),
            _ => {}
        }
        let result = tokio::time::timeout(Duration::from_secs(5), call)
            .await
            .expect("blocked call ends");
        match end {
            "drop" => assert!(result.unwrap_err().is_cancelled()),
            "cancel" => assert_eq!(result.unwrap().unwrap_err(), ExecError::Aborted),
            _ => assert_eq!(result.unwrap().unwrap_err(), ExecError::TimedOut(0.5)),
        }
        // Check every member, not just the shell. Linux PID 1 need not reap an
        // orphan immediately, so a zombie counts as terminated, never as alive.
        for pid in pids {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    // SAFETY: signal zero only checks existence.
                    if unsafe { libc::kill(pid, 0) } != 0 {
                        break;
                    }
                    #[cfg(target_os = "linux")]
                    if fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
                        stat.rsplit_once(") ")
                            .is_some_and(|(_, rest)| rest.starts_with("Z "))
                    }) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("pipeline member is terminated");
        }
    }
}
