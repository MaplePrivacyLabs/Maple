//! These tests require the exact staged distribution; missing Python is a failure.
//! `just test` prepares it, while raw Cargo invocations only consume the fixture.
use maple_code_mode::{
    Config, Error, LaunchSpec, MAX_SOURCE_BYTES, MAX_WORKERS, Outcome, OutcomeStatus,
    PackagedPython, Runtime, TaskHandle,
};
use std::{path::PathBuf, time::Duration};
use tokio_util::sync::CancellationToken;

fn packaged_python() -> PackagedPython {
    let manifest = std::env::var_os("MAPLE_CODE_MODE_RUNTIME_MANIFEST")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/debug/runtime/python/runtime.json")
        });
    PackagedPython::from_manifest(manifest).expect("run `just python-prepare` first")
}

fn runtime() -> Runtime {
    Runtime::new(Config {
        retirement_grace: Duration::from_millis(200),
        ..Config::default()
    })
}

fn bind(runtime: &Runtime, name: &str, root: &std::path::Path) -> TaskHandle {
    runtime
        .bind(
            name,
            LaunchSpec {
                python: packaged_python(),
                cwd: root.canonicalize().unwrap(),
                env: std::env::vars_os().collect(),
            },
            CancellationToken::new(),
        )
        .unwrap()
}

async fn cell(task: &TaskHandle, code: &str) -> Outcome {
    tokio::time::timeout(
        Duration::from_secs(15),
        task.execute(code, CancellationToken::new()),
    )
    .await
    .expect("native cell exceeded test deadline")
    .expect("native worker transport failed")
}

fn all_output(outcome: &Outcome) -> String {
    let mut text = format!("{}{}", outcome.stdout, outcome.stderr);
    for chunk in &outcome.background.chunks {
        text.push_str(&chunk.text);
    }
    text
}

#[tokio::test]
async fn persistent_main_namespace_and_task_isolation() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime();
    let first = bind(&runtime, "first", root.path());
    let second = bind(&runtime, "second", root.path());
    let initial = cell(
        &first,
        "import __main__\nanswer = 40\ndef add(x):\n    return answer + x\nassert __main__.__dict__ is globals()\nadd(2)",
    )
    .await;
    assert_eq!(initial.status, OutcomeStatus::Ok);
    assert_eq!(initial.value.as_deref(), Some("42"));
    assert_eq!(cell(&first, "_ + 1").await.value.as_deref(), Some("43"));
    assert_eq!(cell(&first, "None").await.value, None);
    assert_eq!(cell(&first, "_").await.value.as_deref(), Some("43"));
    assert_eq!(
        cell(&second, "'answer' in globals()")
            .await
            .value
            .as_deref(),
        Some("False")
    );
    let identity = initial.runtime.unwrap();
    assert_eq!(identity.implementation, "cpython");
    assert_eq!(identity.version, "3.13.15");
    assert_eq!(identity.cwd, root.path().canonicalize().unwrap());
    assert_eq!(identity.executable, packaged_python().executable);
    runtime.shutdown("test complete").await.unwrap();
}

#[tokio::test]
async fn errors_keep_partial_state_and_original_cell_source() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime();
    let task = bind(&runtime, "errors", root.path());
    let failed = cell(&task, "saved = 7\nraise ValueError('intentional')").await;
    assert_eq!(failed.status, OutcomeStatus::Error);
    let traceback = failed.traceback.unwrap();
    assert!(traceback.contains("ValueError"), "{traceback}");
    assert!(
        traceback.contains("raise ValueError('intentional')"),
        "{traceback}"
    );
    let syntax = cell(&task, "if :").await;
    assert_eq!(syntax.status, OutcomeStatus::Error);
    assert!(syntax.traceback.unwrap().contains("SyntaxError"));
    assert_eq!(cell(&task, "saved").await.value.as_deref(), Some("7"));
    assert_eq!(task.status().generation, Some(failed.generation));
    runtime.shutdown("test complete").await.unwrap();
}

#[tokio::test]
async fn continuous_loop_runs_between_cells_and_does_not_autoawait_values() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime();
    let task = bind(&runtime, "async", root.path());
    let first = cell(
        &task,
        "import asyncio, pathlib\nloop = asyncio.get_running_loop()\nqueue = asyncio.Queue()\nasync def background():\n    await asyncio.sleep(0.05)\n    print('late output')\n    pathlib.Path('background-finished').write_text('done')\n    await queue.put(42)\njob = asyncio.create_task(background())",
    )
    .await;
    assert_eq!(first.status, OutcomeStatus::Ok);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !root.path().join("background-finished").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("background task did not progress while foreground was idle");
    let second = cell(
        &task,
        "assert asyncio.get_running_loop() is loop\nawait job\nawait queue.get()",
    )
    .await;
    assert_eq!(second.value.as_deref(), Some("42"));
    assert!(second.background.chunks.iter().any(|chunk| {
        chunk.execution_id == Some(first.execution_id) && chunk.text.contains("late output")
    }));
    let third = cell(
        &task,
        "ran = False\nasync def ordinary_coroutine():\n    global ran\n    ran = True\n    return 9\npending = ordinary_coroutine()\npending",
    )
    .await;
    assert!(third.value.as_ref().unwrap().contains("coroutine object"));
    assert!(!all_output(&third).contains("late output"));
    assert_eq!(
        cell(&task, "pending.close()\nran").await.value.as_deref(),
        Some("False")
    );
    runtime.shutdown("test complete").await.unwrap();
}

#[tokio::test]
async fn project_imports_and_binary_native_subprocess_output_are_safe() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("project_helper.py"), "answer = 42\n").unwrap();
    std::fs::write(
        root.path().join("json.py"),
        "raise RuntimeError('shadowed bootstrap')\n",
    )
    .unwrap();
    let runtime = runtime();
    let task = bind(&runtime, "stdio", root.path());
    let output = cell(
        &task,
        "import os, sys, subprocess, project_helper\nassert project_helper.answer == 42\nassert sys.stdin.read() == ''\nprint('text marker')\nsys.stdout.buffer.write(b'binary marker\\xff\\n')\nos.write(1, b'raw marker\\xfe\\n')\nsubprocess.run([sys.executable, '-I', '-B', '-c', \"import os; os.write(2, b'child marker\\\\n')\"], check=True)\n42",
    )
    .await;
    assert_eq!(output.status, OutcomeStatus::Ok, "{:?}", output.traceback);
    assert_eq!(output.value.as_deref(), Some("42"));
    let text = all_output(&output);
    for marker in [
        "text marker",
        "binary marker",
        "raw marker",
        "child marker",
        "�",
    ] {
        assert!(text.contains(marker), "missing {marker}: {text}");
    }
    assert!(!root.path().join("__pycache__").exists());
    assert_eq!(cell(&task, "6 * 7").await.value.as_deref(), Some("42"));
    runtime.shutdown("test complete").await.unwrap();
}

#[tokio::test]
async fn flood_is_bounded_and_final_results_survive() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime();
    let task = bind(&runtime, "flood", root.path());
    let output = cell(
        &task,
        "import sys\nsys.stdout.write('x' * (2 * 1024 * 1024))\n42",
    )
    .await;
    assert_eq!(output.status, OutcomeStatus::Ok);
    assert_eq!(output.value.as_deref(), Some("42"));
    assert!(output.stdout.len() + output.stderr.len() <= 48 * 1024);
    assert!(output.dropped_stdout_bytes > 0);
    let error = cell(
        &task,
        "print('y' * 100000)\nraise ValueError('still visible')",
    )
    .await;
    assert_eq!(error.status, OutcomeStatus::Error);
    assert!(error.traceback.unwrap().contains("still visible"));
    assert_eq!(cell(&task, "40 + 2").await.value.as_deref(), Some("42"));
    runtime.shutdown("test complete").await.unwrap();
}

#[tokio::test]
async fn tiny_output_burst_is_retained_below_the_byte_limit() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime();
    let task = bind(&runtime, "tiny-output", root.path());
    let expected: String = (0..5_000).map(|i| format!("{i}\n")).collect();
    let output = cell(&task, "for i in range(5000):\n    print(i)\n42").await;
    runtime.shutdown("test complete").await.unwrap();
    assert_eq!(output.status, OutcomeStatus::Ok);
    assert_eq!(output.value.as_deref(), Some("42"));
    assert_eq!(
        output.stdout.len() as u64 + output.dropped_stdout_bytes,
        expected.len() as u64
    );
    assert_eq!(output.dropped_stderr_bytes, 0);
    assert_eq!(output.dropped_stdout_bytes, 0);
    assert_eq!(output.stdout, expected);
}

#[tokio::test]
async fn validation_and_reset_only_do_not_spawn_and_capacity_is_retained() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime();
    let empty = bind(&runtime, "empty", root.path());
    assert_eq!(
        empty.reset("empty reset").await.unwrap().retired_generation,
        None
    );
    assert!(matches!(
        empty
            .execute("x".repeat(MAX_SOURCE_BYTES + 1), CancellationToken::new())
            .await,
        Err(Error::InvalidInput(_))
    ));
    assert!(runtime.snapshot().holders.is_empty());
    let mut tasks = Vec::new();
    for index in 0..MAX_WORKERS {
        let task = bind(&runtime, &format!("holder-{index}"), root.path());
        assert_eq!(
            cell(&task, "saved = 42\nsaved").await.value.as_deref(),
            Some("42")
        );
        tasks.push(task);
    }
    assert_eq!(runtime.snapshot().holders.len(), MAX_WORKERS);
    assert!(matches!(
        empty.execute("1", CancellationToken::new()).await,
        Err(Error::Capacity { .. })
    ));
    assert_eq!(cell(&tasks[0], "saved").await.value.as_deref(), Some("42"));
    tasks[1].reset("free one slot").await.unwrap();
    assert_eq!(cell(&empty, "6 * 7").await.value.as_deref(), Some("42"));
    assert_eq!(runtime.snapshot().holders.len(), MAX_WORKERS);
    runtime.shutdown("test complete").await.unwrap();
    assert!(runtime.snapshot().holders.is_empty());
}

#[tokio::test]
async fn unfinished_cancellation_loses_state_but_completed_token_does_not() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime();
    let task = bind(&runtime, "cancel", root.path());
    let completed_token = CancellationToken::new();
    let first = task
        .execute("saved = 42", completed_token.clone())
        .await
        .unwrap();
    completed_token.cancel();
    assert_eq!(cell(&task, "saved").await.value.as_deref(), Some("42"));
    let stop = CancellationToken::new();
    let blocked = task.execute("while True:\n    pass", stop.clone());
    assert!(matches!(
        task.execute("1", CancellationToken::new()).await,
        Err(Error::Busy)
    ));
    stop.cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(10), blocked)
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        Ok(Outcome {
            status: OutcomeStatus::Cancelled,
            ..
        }) | Err(Error::Cancelled)
    ));
    task.reset("settle cancellation").await.unwrap();
    let fresh = cell(&task, "'saved' in globals()").await;
    assert_eq!(fresh.value.as_deref(), Some("False"));
    assert_ne!(fresh.generation, first.generation);
    runtime.shutdown("test complete").await.unwrap();
}

#[tokio::test]
async fn dropped_unpolled_execution_is_supervised_and_resettable() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime();
    let task = bind(&runtime, "dropped", root.path());
    drop(task.execute("while True:\n    pass", CancellationToken::new()));
    tokio::time::timeout(Duration::from_secs(10), task.reset("caller disappeared"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cell(&task, "42").await.value.as_deref(), Some("42"));
    runtime.shutdown("test complete").await.unwrap();
}

#[tokio::test]
async fn retirement_fences_stale_handles_before_cleanup_is_awaited() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime();
    let task = bind(&runtime, "retired", root.path());
    cell(&task, "saved = 42").await;
    let cleanup = task.retire("task archived");
    assert!(matches!(
        task.execute("saved", CancellationToken::new()).await,
        Err(Error::Retired)
    ));
    cleanup.await.unwrap();
    let replacement = bind(&runtime, "retired", root.path());
    assert_eq!(
        cell(&replacement, "'saved' in globals()")
            .await
            .value
            .as_deref(),
        Some("False")
    );
    assert!(matches!(
        task.execute("1", CancellationToken::new()).await,
        Err(Error::Retired)
    ));
    runtime.shutdown("test complete").await.unwrap();
}

#[tokio::test]
async fn owner_revocation_cleans_up_an_idle_worker() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime();
    let lifetime = CancellationToken::new();
    let task = runtime
        .bind(
            "owner",
            LaunchSpec {
                python: packaged_python(),
                cwd: root.path().canonicalize().unwrap(),
                env: std::env::vars_os().collect(),
            },
            lifetime.clone(),
        )
        .unwrap();
    cell(
        &task,
        "import asyncio\nbackground = asyncio.create_task(asyncio.sleep(1000))",
    )
    .await;
    lifetime.cancel();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !runtime.snapshot().holders.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        task.execute("1", CancellationToken::new()).await,
        Err(Error::Retired)
    ));
    runtime.shutdown("test complete").await.unwrap();
}

#[tokio::test]
async fn cancellation_during_handshake_prevents_code_and_releases_capacity() {
    let root = tempfile::tempdir().unwrap();
    let fake = root.path().join("delayed_ready.py");
    std::fs::write(
        &fake,
        r#"
import json, os, pathlib, platform, struct, sys, time
generation = int(sys.argv[-1])
pathlib.Path('bootstrap-started').touch()
while not pathlib.Path('allow-ready').exists():
    time.sleep(0.005)
ready = json.dumps(dict(type='ready', generation=generation, protocol_version=1,
    implementation='cpython', version=platform.python_version(),
    executable=sys.executable, cwd=os.getcwd())).encode()
os.write(1, struct.pack('>I', len(ready)) + ready)
header = sys.stdin.buffer.read(4)
if len(header) == 4:
    message = json.loads(sys.stdin.buffer.read(struct.unpack('>I', header)[0]))
    if message['type'] == 'execute':
        pathlib.Path('code-dispatched').touch()
"#,
    )
    .unwrap();
    let mut python = packaged_python();
    python.worker = fake.canonicalize().unwrap();
    let runtime = runtime();
    let task = runtime
        .bind(
            "handshake",
            LaunchSpec {
                python,
                cwd: root.path().canonicalize().unwrap(),
                env: std::env::vars_os().collect(),
            },
            CancellationToken::new(),
        )
        .unwrap();
    let stop = CancellationToken::new();
    let pending = task.execute("raise RuntimeError('must not execute')", stop.clone());
    tokio::time::timeout(Duration::from_secs(5), async {
        while !root.path().join("bootstrap-started").exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    stop.cancel();
    std::fs::write(root.path().join("allow-ready"), "").unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(10), pending)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome.status, OutcomeStatus::Cancelled);
    assert!(!root.path().join("code-dispatched").exists());
    assert!(runtime.snapshot().holders.is_empty());
    runtime.shutdown("test complete").await.unwrap();
}
