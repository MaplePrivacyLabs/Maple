//! Shell commands the user runs (`!command` and `!!command`), after Pi's.

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use common::{Harness, roles};
use pi_ai::{Message, content_text};
use pi_coding_agent::messages::convert_to_llm;
use pi_coding_agent::tools::{BashOperations, ExecError, ExecOptions};
use pi_coding_agent::{AgentSessionEvent, BashCommandOptions, BashResult, PromptOptions};

/// A shell that prints its chunks, then exits or waits to be stopped.
struct Scripted {
    chunks: Vec<&'static [u8]>,
    exit: Option<i32>,
    wait_for_stop: bool,
    commands: Mutex<Vec<String>>,
}

impl Scripted {
    fn new(chunks: Vec<&'static [u8]>, exit: Option<i32>) -> Arc<Self> {
        Arc::new(Self {
            chunks,
            exit,
            wait_for_stop: false,
            commands: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl BashOperations for Scripted {
    async fn exec(
        &self,
        command: &str,
        _cwd: &Path,
        options: ExecOptions,
    ) -> Result<Option<i32>, ExecError> {
        self.commands.lock().unwrap().push(command.to_string());
        for chunk in &self.chunks {
            (options.on_data)(chunk);
        }
        if self.wait_for_stop {
            options.cancel.cancelled().await;
            return Err(ExecError::Aborted);
        }
        Ok(self.exit)
    }
}

fn last_llm_text(messages: &[pi_coding_agent::SessionMessage]) -> String {
    match convert_to_llm(messages).last() {
        Some(Message::User(user)) => content_text(&user.content),
        other => panic!("not a user message: {other:?}"),
    }
}

#[tokio::test]
async fn a_command_joins_the_conversation_with_its_output_cleaned() {
    let harness = Harness::new();
    let session = harness
        .session_with(|options| {
            options.settings.shell_command_prefix = Some("shopt -s expand_aliases".into());
        })
        .await;
    let updates = Arc::new(Mutex::new(Vec::new()));
    let sink = updates.clone();
    session.subscribe(move |event| {
        if let AgentSessionEvent::BashExecutionUpdate { id, delta } = event {
            sink.lock().unwrap().push((id.clone(), delta.clone()));
        }
    });
    let shell = Scripted::new(vec![b"\x1b[31mfail\x1b[0m\r\n", b"done\n"], Some(2));
    let options = BashCommandOptions {
        id: Some("b1".into()),
        operations: Some(shell.clone()),
        ..BashCommandOptions::default()
    };
    let result = session.execute_bash("make", options).await.unwrap();

    assert_eq!(
        result,
        BashResult {
            output: "fail\ndone\n".into(),
            exit_code: Some(2),
            cancelled: false,
            truncated: false,
            full_output_path: None,
        }
    );
    assert_eq!(
        *shell.commands.lock().unwrap(),
        ["shopt -s expand_aliases\nmake"]
    );
    assert_eq!(
        *updates.lock().unwrap(),
        [
            (Some("b1".to_string()), "fail\n".to_string()),
            (Some("b1".to_string()), "done\n".to_string())
        ]
    );
    let messages = session.messages();
    assert_eq!(roles(&messages).last(), Some(&"bashExecution"));
    assert_eq!(
        last_llm_text(&messages),
        "Ran `make`\n```\nfail\ndone\n\n```\n\nCommand exited with code 2"
    );

    // The next request carries it.
    harness.faux.push_text("It failed in make.");
    session
        .prompt("what failed?", PromptOptions::default())
        .await
        .unwrap();
    let context = &harness.faux.requests()[0].context.messages;
    assert!(context.iter().any(|message| matches!(
        message,
        Message::User(user) if content_text(&user.content).starts_with("Ran `make`")
    )));
}

#[tokio::test]
async fn a_bang_bang_command_is_kept_but_the_model_never_sees_it() {
    let harness = Harness::new();
    let session = harness.session().await;
    let options = BashCommandOptions {
        exclude_from_context: true,
        operations: Some(Scripted::new(vec![b"secret\n"], Some(0))),
        ..BashCommandOptions::default()
    };
    session.execute_bash("cat token", options).await.unwrap();
    assert_eq!(roles(&session.messages()).last(), Some(&"bashExecution"));

    harness.faux.push_text("ok");
    session
        .prompt("hi", PromptOptions::default())
        .await
        .unwrap();
    let context = &harness.faux.requests()[0].context.messages;
    assert!(!context.iter().any(|message| matches!(
        message,
        Message::User(user) if content_text(&user.content).contains("secret")
    )));
}

#[tokio::test]
async fn abort_bash_stops_a_running_command() {
    let harness = Harness::new();
    let session = harness.session().await;
    let shell = Arc::new(Scripted {
        chunks: vec![b"partial"],
        exit: None,
        wait_for_stop: true,
        commands: Mutex::new(Vec::new()),
    });
    let runner = session.clone();
    let running = tokio::spawn(async move {
        let options = BashCommandOptions {
            operations: Some(shell),
            ..BashCommandOptions::default()
        };
        runner.execute_bash("sleep 100", options).await
    });
    while !session.is_bash_running() {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    session.abort_bash();
    let result = running.await.unwrap().unwrap();
    assert!(result.cancelled);
    assert_eq!(result.exit_code, None);
    assert_eq!(result.output, "partial");
    assert!(!session.is_bash_running());
    assert_eq!(
        last_llm_text(&session.messages()),
        "Ran `sleep 100`\n```\npartial\n```\n\n(command cancelled)"
    );
}

#[tokio::test]
async fn a_command_that_ends_during_a_run_is_added_after_it() {
    let harness = Harness::new();
    harness.faux.push_hang();
    let session = harness.session().await;
    let runner = session.clone();
    let run = tokio::spawn(async move { runner.prompt("first", PromptOptions::default()).await });
    while !session.is_streaming() {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let result = BashResult {
        output: "ok".into(),
        exit_code: Some(0),
        cancelled: false,
        truncated: false,
        full_output_path: None,
    };
    session.record_bash_result("ls", &result, false);
    assert!(session.has_pending_bash_messages());
    assert!(!roles(&session.messages()).contains(&"bashExecution"));

    session.abort();
    let _ = run.await.unwrap();
    assert!(!session.has_pending_bash_messages());
    assert_eq!(roles(&session.messages()).last(), Some(&"bashExecution"));
}

#[cfg(unix)]
#[tokio::test]
async fn commands_run_in_the_session_folder_with_the_local_shell() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("marker"), "").unwrap();
    let harness = Harness::new();
    let cwd = dir.path().to_path_buf();
    let session = harness.session_with(|options| options.cwd = cwd).await;
    let result = session
        .execute_bash(
            "ls; printf 'a\\033[1mb'; exit 3",
            BashCommandOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(result.output, "marker\nab");
    assert_eq!(result.exit_code, Some(3));
}
