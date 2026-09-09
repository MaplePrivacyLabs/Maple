//! Process-tree checks use the staged CPython distribution, never PATH Python.
use std::{path::PathBuf, time::Duration};

use maple_code_mode::{
    Config, Error, LaunchSpec, OutcomeStatus, PackagedPython, Runtime, TaskHandle,
};
use tokio_util::sync::CancellationToken;

fn bind(runtime: &Runtime, root: &std::path::Path) -> TaskHandle {
    let manifest = std::env::var_os("MAPLE_CODE_MODE_RUNTIME_MANIFEST")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/debug/runtime/python/runtime.json")
        });
    runtime
        .bind(
            "process-tree",
            LaunchSpec {
                python: PackagedPython::from_manifest(manifest)
                    .expect("run `just python-prepare` first"),
                cwd: root.canonicalize().unwrap(),
                env: std::env::vars_os().collect(),
            },
            CancellationToken::new(),
        )
        .unwrap()
}

async fn start_tree(task: &TaskHandle) -> (ProcessWitness, ProcessWitness) {
    let outcome = tokio::time::timeout(
        Duration::from_secs(15),
        task.execute(
            r#"import asyncio, os, pathlib, subprocess, sys
child_code = "import os, pathlib, sys, time; pathlib.Path(sys.argv[1]).write_text(str(os.getpid())); time.sleep(60)"
descendant = subprocess.Popen([sys.executable, '-I', '-B', '-c', child_code, str(pathlib.Path('descendant-ready').absolute())])
while not pathlib.Path('descendant-ready').exists():
    await asyncio.sleep(0.01)
async def retained_background():
    try:
        await asyncio.sleep(1000)
    finally:
        pathlib.Path('cooperative-finally').write_text('completed')
background = asyncio.create_task(retained_background())
await asyncio.sleep(0)
print(os.getpid(), descendant.pid)
"#,
            CancellationToken::new(),
        ),
    )
    .await
    .expect("native tree startup exceeded deadline")
    .expect("native tree startup transport failed");
    assert_eq!(outcome.status, OutcomeStatus::Ok, "{outcome:?}");
    let pids: Vec<u32> = outcome
        .stdout
        .split_whitespace()
        .map(|pid| pid.parse().unwrap())
        .collect();
    assert_eq!(pids.len(), 2, "unexpected PID response: {outcome:?}");
    // Windows keeps handles to these exact process objects before retirement,
    // so observing termination cannot be confused by later numeric PID reuse.
    (
        ProcessWitness::capture(pids[0]),
        ProcessWitness::capture(pids[1]),
    )
}

fn runtime() -> Runtime {
    Runtime::new(Config {
        retirement_grace: Duration::from_millis(500),
        ..Config::default()
    })
}

#[tokio::test]
async fn cooperative_retirement_ends_the_ordinary_descendant() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime();
    let task = bind(&runtime, root.path());
    let (worker, descendant) = start_tree(&task).await;
    tokio::time::timeout(Duration::from_secs(10), task.reset("tree test reset"))
        .await
        .unwrap()
        .unwrap();
    assert!(
        root.path().join("cooperative-finally").exists(),
        "responsive shutdown did not execute the background cleanup handler"
    );
    worker.assert_stopped(true).await;
    descendant.assert_stopped(false).await;
    assert!(runtime.snapshot().holders.is_empty());
    runtime.shutdown("test complete").await.unwrap();
}

#[tokio::test]
async fn blocked_loop_is_forced_down_with_its_ordinary_descendant() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime();
    let task = bind(&runtime, root.path());
    let (worker, descendant) = start_tree(&task).await;
    let stop = CancellationToken::new();
    let execution = task.execute(
        "pathlib.Path('sync-entered').write_text('entered')\nwhile True:\n    pass",
        stop.clone(),
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while !root.path().join("sync-entered").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the worker did not enter the synchronous infinite loop");
    stop.cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(10), execution)
        .await
        .expect("cancelled execution did not settle");
    assert!(
        matches!(outcome, Ok(ref value) if value.status == OutcomeStatus::Cancelled)
            || matches!(outcome, Err(Error::Cancelled)),
        "unexpected cancellation result: {outcome:?}"
    );
    task.reset("confirm forced cleanup").await.unwrap();
    worker.assert_stopped(true).await;
    descendant.assert_stopped(false).await;
    assert!(
        !root.path().join("cooperative-finally").exists(),
        "the non-yielding loop unexpectedly ran cooperative cleanup"
    );
    assert!(runtime.snapshot().holders.is_empty());
    runtime.shutdown("test complete").await.unwrap();
}

struct ProcessWitness {
    pid: u32,
    #[cfg(windows)]
    handle: windows::Win32::Foundation::HANDLE,
}

impl ProcessWitness {
    fn capture(pid: u32) -> Self {
        #[cfg(windows)]
        let handle = unsafe {
            windows::Win32::System::Threading::OpenProcess(
                windows::Win32::System::Threading::PROCESS_SYNCHRONIZE,
                false,
                pid,
            )
        }
        .expect("could not retain the running native process handle");
        Self {
            pid,
            #[cfg(windows)]
            handle,
        }
    }

    async fn assert_stopped(&self, direct_worker: bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !self.stopped(direct_worker) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("native process {} is still running", self.pid));
    }

    #[cfg(unix)]
    fn stopped(&self, direct_worker: bool) -> bool {
        // The direct child must have been reaped. Arbitrary Unix grandchildren
        // are not ours to reap: a zombie is terminated but awaits its own parent.
        let result = unsafe { libc::kill(self.pid as i32, 0) };
        if result == -1 {
            let error = std::io::Error::last_os_error();
            assert_eq!(error.raw_os_error(), Some(libc::ESRCH));
            return true;
        }
        if direct_worker {
            return false;
        }
        let output = std::process::Command::new("/bin/ps")
            .args(["-o", "stat=", "-p", &self.pid.to_string()])
            .output()
            .expect("could not inspect the Unix descendant state");
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .starts_with('Z')
    }

    #[cfg(windows)]
    fn stopped(&self, _direct_worker: bool) -> bool {
        use windows::Win32::{
            Foundation::{WAIT_FAILED, WAIT_OBJECT_0},
            System::Threading::WaitForSingleObject,
        };
        let result = unsafe { WaitForSingleObject(self.handle, 0) };
        assert_ne!(result, WAIT_FAILED, "native process wait failed");
        result == WAIT_OBJECT_0
    }
}

#[cfg(windows)]
impl Drop for ProcessWitness {
    fn drop(&mut self) {
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.handle) };
    }
}
