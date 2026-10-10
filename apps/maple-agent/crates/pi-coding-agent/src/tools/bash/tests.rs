//! `bash`, after Pi's tool tests, plus what stopping a command reaches.

use std::fs;
#[cfg(unix)]
use std::time::Duration;

use pi_ai::content_text;

use super::*;

fn context() -> ToolContext {
    ToolContext::new("Test")
}

fn bash(dir: &Path, options: BashToolOptions) -> ShellTool {
    ShellTool::bash(dir, options, context())
}

async fn run(tool: &ShellTool, args: Value) -> Result<AgentToolResult, String> {
    run_with(tool, args, CancellationToken::new()).await
}

async fn run_with(
    tool: &ShellTool,
    args: Value,
    cancel: CancellationToken,
) -> Result<AgentToolResult, String> {
    tool.execute(ToolInvocation {
        call_id: "call".to_string(),
        args,
        cancel,
        updates: ToolUpdates::none(),
    })
    .await
    .map_err(|error| error.to_string())
}

/// Operations that print `lines` and then end with `outcome`.
struct Scripted {
    chunks: Vec<Vec<u8>>,
    outcome: Result<Option<i32>, ExecError>,
}

#[async_trait]
impl BashOperations for Scripted {
    async fn exec(
        &self,
        _command: &str,
        _cwd: &Path,
        options: ExecOptions,
    ) -> Result<Option<i32>, ExecError> {
        for chunk in &self.chunks {
            (options.on_data)(chunk);
        }
        self.outcome.clone()
    }
}

fn scripted(chunks: Vec<Vec<u8>>, outcome: Result<Option<i32>, ExecError>) -> BashToolOptions {
    BashToolOptions {
        operations: Some(Arc::new(Scripted { chunks, outcome })),
        ..BashToolOptions::default()
    }
}

fn lines(count: usize, format: impl Fn(usize) -> String) -> Vec<Vec<u8>> {
    (1..=count).map(|i| format(i).into_bytes()).collect()
}

fn full_output_path(text: &str) -> PathBuf {
    let start = text.find("Full output: ").expect("names the full output") + "Full output: ".len();
    let end = text[start..].find(']').unwrap() + start;
    PathBuf::from(&text[start..end])
}

#[cfg(unix)]
#[tokio::test]
async fn commands_run_and_report_their_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let tool = bash(dir.path(), BashToolOptions::default());
    let result = run(&tool, json!({"command": "echo 'test output'"}))
        .await
        .unwrap();
    assert_eq!(content_text(&result.content), "test output\n");
    assert!(result.details.is_none());
    assert!(!result.is_error);

    let failed = run(&tool, json!({"command": "echo out; exit 3"}))
        .await
        .unwrap();
    assert!(failed.is_error);
    assert_eq!(
        content_text(&failed.content),
        "out\n\n\nCommand exited with code 3"
    );

    let empty = run(&tool, json!({"command": "true"})).await.unwrap();
    assert_eq!(content_text(&empty.content), "(no output)");
}

#[cfg(unix)]
#[tokio::test]
async fn a_command_killed_by_a_signal_reports_128_plus_the_signal() {
    let dir = tempfile::tempdir().unwrap();
    let tool = bash(dir.path(), BashToolOptions::default());
    for (signal, code) in [("KILL", 137), ("TERM", 143)] {
        let result = run(
            &tool,
            json!({"command": format!("printf 'before-kill\\n'; kill -{signal} $$")}),
        )
        .await
        .unwrap();
        assert!(result.is_error);
        assert_eq!(
            content_text(&result.content),
            format!("before-kill\n\n\nCommand exited with code {code}")
        );
    }
}

#[tokio::test]
async fn a_missing_exit_code_is_an_error_that_keeps_the_output() {
    let dir = tempfile::tempdir().unwrap();
    let tool = bash(dir.path(), scripted(vec![b"partial\n".to_vec()], Ok(None)));
    let error = run(&tool, json!({"command": "remote"})).await.unwrap_err();
    assert_eq!(
        error,
        "partial\n\n\nCommand terminated without an exit code"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_timeout_stops_the_command() {
    let dir = tempfile::tempdir().unwrap();
    let tool = bash(dir.path(), BashToolOptions::default());
    let started = std::time::Instant::now();
    let error = run(&tool, json!({"command": "sleep 30", "timeout": 0.05}))
        .await
        .unwrap_err();
    assert_eq!(error, "Command timed out after 0.05 seconds");
    assert!(started.elapsed() < Duration::from_secs(5));

    let error = run(&tool, json!({"command": "true", "timeout": -1}))
        .await
        .unwrap_err();
    assert_eq!(error, "Invalid timeout: must be a finite number of seconds");
}

#[tokio::test]
async fn a_stopped_or_timed_out_command_points_at_its_full_output() {
    let dir = tempfile::tempdir().unwrap();
    for (outcome, status) in [
        (
            ExecError::TimedOut(5.0),
            "Command timed out after 5 seconds",
        ),
        (ExecError::Aborted, "Command aborted"),
    ] {
        let tool = bash(
            dir.path(),
            scripted(lines(3000, |i| format!("{i}\n")), Err(outcome)),
        );
        let error = run(&tool, json!({"command": "chatty-fail"}))
            .await
            .unwrap_err();
        assert!(error.ends_with(status), "{error}");
        assert!(
            error.contains("[Showing lines 1001-3000 of 3000. Full output: "),
            "{error}"
        );
        let saved = fs::read_to_string(full_output_path(&error)).unwrap();
        assert!(saved.starts_with("1\n2\n3\n"));
        assert!(saved.ends_with("2998\n2999\n3000\n"));
    }
}

#[tokio::test]
async fn a_missing_folder_or_shell_is_an_error() {
    let missing = Path::new("/this/directory/definitely/does/not/exist/12345");
    let tool = bash(missing, BashToolOptions::default());
    let error = run(&tool, json!({"command": "echo test"}))
        .await
        .unwrap_err();
    assert!(
        error.starts_with("Working directory does not exist"),
        "{error}"
    );

    let dir = tempfile::tempdir().unwrap();
    let tool = bash(
        dir.path(),
        BashToolOptions {
            shell_path: Some(PathBuf::from("/custom/bash")),
            ..BashToolOptions::default()
        },
    );
    let error = run(&tool, json!({"command": "echo test"}))
        .await
        .unwrap_err();
    assert_eq!(error, "Custom shell path not found: /custom/bash");
}

#[cfg(unix)]
#[tokio::test]
async fn the_command_prefix_runs_first() {
    let dir = tempfile::tempdir().unwrap();
    let tool = bash(
        dir.path(),
        BashToolOptions {
            command_prefix: Some("export TEST_VAR=hello".to_string()),
            ..BashToolOptions::default()
        },
    );
    let result = run(&tool, json!({"command": "echo $TEST_VAR"}))
        .await
        .unwrap();
    assert_eq!(content_text(&result.content).trim(), "hello");

    let tool = bash(
        dir.path(),
        BashToolOptions {
            command_prefix: Some("echo prefix-output".to_string()),
            ..BashToolOptions::default()
        },
    );
    let result = run(&tool, json!({"command": "echo command-output"}))
        .await
        .unwrap();
    assert_eq!(
        content_text(&result.content).trim(),
        "prefix-output\ncommand-output"
    );
}

#[tokio::test]
async fn long_output_keeps_its_last_lines_and_saves_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let tool = bash(
        dir.path(),
        scripted(lines(4000, |i| format!("line-{i:04}\n")), Ok(Some(0))),
    );
    let result = run(&tool, json!({"command": "many-lines"})).await.unwrap();
    let text = content_text(&result.content);
    let details = result.details.unwrap();
    assert_eq!(details["truncation"]["totalLines"], 4000);
    assert_eq!(details["truncation"]["outputLines"], 2000);
    assert!(text.starts_with("line-2001\n"));
    assert!(text.contains("line-4000"));
    assert!(
        text.contains("[Showing lines 2001-4000 of 4000. Full output: "),
        "{text}"
    );
    assert!(!text.contains("4001"));
    assert_eq!(
        PathBuf::from(details["fullOutputPath"].as_str().unwrap()),
        full_output_path(&text)
    );
    assert!(
        full_output_path(&text)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("test-bash-")
    );
}

#[tokio::test]
async fn a_character_split_between_chunks_arrives_whole() {
    let dir = tempfile::tempdir().unwrap();
    let euro = "€\n".as_bytes();
    let tool = bash(
        dir.path(),
        scripted(vec![euro[..1].to_vec(), euro[1..].to_vec()], Ok(Some(0))),
    );
    let result = run(&tool, json!({"command": "split"})).await.unwrap();
    assert_eq!(content_text(&result.content).trim(), "€");
}

#[cfg(unix)]
#[tokio::test]
async fn local_operations_take_the_environment_given() {
    let dir = tempfile::tempdir().unwrap();
    let output = Arc::new(Mutex::new(Vec::new()));
    let mut env = shell_env();
    env.insert(
        "TEST_LOCAL_BASH_OPS".to_string(),
        "from-local-ops".to_string(),
    );
    let code = LocalShellOperations::bash(None)
        .exec(
            "echo $TEST_LOCAL_BASH_OPS",
            dir.path(),
            ExecOptions {
                on_data: {
                    let output = output.clone();
                    Arc::new(move |data: &[u8]| output.lock().unwrap().extend_from_slice(data))
                },
                cancel: CancellationToken::new(),
                timeout: None,
                env: Some(env),
            },
        )
        .await
        .unwrap();
    assert_eq!(code, Some(0));
    assert_eq!(
        String::from_utf8(output.lock().unwrap().clone()).unwrap(),
        "from-local-ops\n"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn the_spawn_hook_sets_the_environment_and_folder() {
    let dir = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let target = other.path().to_path_buf();
    let tool = bash(
        dir.path(),
        BashToolOptions {
            spawn_hook: Some(Arc::new(move |mut context: BashSpawnContext| {
                set_env_var(&mut context.env, "FROM_HOOK", "hooked");
                context.cwd = target.clone();
                context
            })),
            ..BashToolOptions::default()
        },
    );
    let result = run(
        &tool,
        json!({"command": "printf '%s ' \"$FROM_HOOK\"; pwd"}),
    )
    .await
    .unwrap();
    let text = content_text(&result.content);
    let (value, folder) = text.trim().split_once(' ').unwrap();
    assert_eq!(value, "hooked");
    assert_eq!(
        fs::canonicalize(folder).unwrap(),
        fs::canonicalize(other.path()).unwrap()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_finished_command_may_leave_a_background_job_running() {
    let dir = tempfile::tempdir().unwrap();
    let sentinel = dir.path().join("background-finished");
    let tool = bash(dir.path(), BashToolOptions::default());
    let started = std::time::Instant::now();
    run(
        &tool,
        json!({"command": format!("(sleep 1; printf done > '{}') &", sentinel.display())}),
    )
    .await
    .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the job did not hold the call"
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(fs::read_to_string(&sentinel).unwrap(), "done");
}

#[cfg(unix)]
#[tokio::test]
async fn stopping_a_command_stops_everything_it_started() {
    let dir = tempfile::tempdir().unwrap();
    let started = dir.path().join("started");
    let sentinel = dir.path().join("descendant-survived");
    let tool = Arc::new(bash(dir.path(), BashToolOptions::default()));
    let command = format!(
        "(sleep 1; printf survived > '{}') & printf started > '{}'; sleep 30",
        sentinel.display(),
        started.display()
    );
    let cancel = CancellationToken::new();
    let call = tokio::spawn({
        let (tool, cancel) = (tool.clone(), cancel.clone());
        async move { run_with(&tool, json!({"command": command}), cancel).await }
    });
    wait_for(&started).await;
    cancel.cancel();
    let error = call.await.unwrap().unwrap_err();
    assert!(error.ends_with("Command aborted"), "{error}");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!sentinel.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_a_call_stops_everything_it_started() {
    let dir = tempfile::tempdir().unwrap();
    let started = dir.path().join("started");
    let sentinel = dir.path().join("descendant-survived");
    let tool = Arc::new(bash(dir.path(), BashToolOptions::default()));
    let command = format!(
        "(sleep 1; printf survived > '{}') & printf started > '{}'; sleep 30",
        sentinel.display(),
        started.display()
    );
    let call = tokio::spawn({
        let tool = tool.clone();
        async move { run(&tool, json!({"command": command})).await }
    });
    wait_for(&started).await;
    call.abort();
    assert!(call.await.unwrap_err().is_cancelled());
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!sentinel.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn stopping_a_command_ends_a_background_job_that_keeps_printing() {
    // A busy machine can pause the job for longer than the quiet window, so that the call
    // ends on its own before Stop. Such a run proves nothing and is run again.
    for _ in 0..5 {
        let dir = tempfile::tempdir().unwrap();
        let (shell_pid, ticks) = (dir.path().join("shell-pid"), dir.path().join("ticks"));
        let tool = Arc::new(bash(dir.path(), BashToolOptions::default()));
        let cancel = CancellationToken::new();
        let call = tokio::spawn({
            let (tool, cancel) = (tool.clone(), cancel.clone());
            let command = printing_job(&shell_pid, &ticks);
            async move { run_with(&tool, json!({"command": command}), cancel).await }
        });
        wait_for_exit(&shell_pid).await;
        wait_for(&ticks).await;
        cancel.cancel();
        let result = tokio::time::timeout(Duration::from_secs(5), call)
            .await
            .expect("Stop ends the call")
            .unwrap();
        if let Err(error) = result {
            assert!(error.ends_with("Command aborted"), "{error}");
            assert_stopped(&ticks).await;
            return;
        }
    }
    panic!("the job never kept the call reading");
}

#[cfg(unix)]
#[tokio::test]
async fn a_timeout_ends_a_background_job_that_keeps_printing() {
    // As above, a run in which the call ended on its own is run again.
    for _ in 0..5 {
        let dir = tempfile::tempdir().unwrap();
        let (shell_pid, ticks) = (dir.path().join("shell-pid"), dir.path().join("ticks"));
        let tool = bash(dir.path(), BashToolOptions::default());
        let command = printing_job(&shell_pid, &ticks);
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            run(&tool, json!({"command": command, "timeout": 0.5})),
        )
        .await
        .expect("the timeout ends the call");
        if let Err(error) = result {
            assert!(
                error.ends_with("Command timed out after 0.5 seconds"),
                "{error}"
            );
            assert_stopped(&ticks).await;
            return;
        }
    }
    panic!("the job never kept the call reading until the timeout");
}

/// A command whose shell writes its pid to `shell_pid` and exits at once, leaving a job
/// that counts in `ticks`, then prints, every 500 turns of a loop that runs for several
/// seconds. The job starts no process, so a busy machine is less likely to pause it.
#[cfg(unix)]
fn printing_job(shell_pid: &Path, ticks: &Path) -> String {
    format!(
        "(i=0; while [ $i -lt 3000000 ]; do i=$((i+1)); if [ $((i % 500)) -eq 0 ]; then printf x >> '{}'; echo tick; fi; done) & printf %s $$ > '{}'",
        ticks.display(),
        shell_pid.display()
    )
}

/// Wait until the shell whose pid is in `path` has exited and been reaped.
#[cfg(unix)]
async fn wait_for_exit(path: &Path) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let pid = fs::read_to_string(path)
                .ok()
                .and_then(|pid| pid.parse().ok());
            // SAFETY: signal 0 only checks whether the process exists.
            if pid.is_some_and(|pid: libc::pid_t| unsafe { libc::kill(pid, 0) } != 0) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the shell exits");
}

/// Check that the job counting in `ticks` has stopped.
#[cfg(unix)]
async fn assert_stopped(ticks: &Path) {
    tokio::time::sleep(Duration::from_millis(100)).await;
    let counted = fs::metadata(ticks).unwrap().len();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        fs::metadata(ticks).unwrap().len(),
        counted,
        "the job still runs"
    );
}

#[cfg(unix)]
async fn wait_for(path: &Path) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the command starts");
}
