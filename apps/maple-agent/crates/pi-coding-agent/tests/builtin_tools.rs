//! A session's built-in tools: which it gets, how the prompt names them, and a run that
//! uses them.
// The runs use Unix commands, so Windows builds only the first tests.
#![cfg_attr(not(unix), allow(unused_imports))]

mod common;

use std::sync::{Arc, Mutex};

use common::{Harness, echo_tool};
use pi_agent_core::AgentEvent;
use pi_ai::faux::faux_tool_call;
use pi_ai::{Message, content_text};
use pi_coding_agent::session::SessionManager;
use pi_coding_agent::{AgentSessionEvent, PromptOptions};
use serde_json::json;

fn with_builtins(options: &mut pi_coding_agent::AgentSessionOptions) {
    options.builtin_tools = None;
}

#[tokio::test]
async fn a_session_starts_with_read_bash_edit_and_write() {
    let harness = Harness::new();
    let session = harness.session_with(with_builtins).await;
    assert_eq!(session.active_tools(), ["read", "bash", "edit", "write"]);
    assert!(session.tool_names().contains(&"powershell".to_string()));

    let prompt = session.extension_context().next_system_prompt();
    assert!(prompt.contains("- read: Read file contents"), "{prompt}");
    assert!(prompt.contains("- bash: Execute bash commands (ls, grep, find, etc.)"));
    assert!(prompt.contains("- Use bash for file operations like ls, rg, find"));
    assert!(prompt.contains("- Use read to examine files instead of cat or sed."));
    assert!(prompt.contains(
        "- You can inspect MAPLE_* environment variables for current model and session details."
    ));

    // grep, find and ls are there to turn on; with them, the shell rule goes.
    for name in ["grep", "find", "ls"] {
        assert!(session.tool_names().contains(&name.to_string()), "{name}");
    }
    session.set_active_tools(&["read", "bash", "grep", "find", "ls"].map(String::from));
    let prompt = session.extension_context().next_system_prompt();
    assert!(prompt.contains("- grep: Search file contents for patterns (respects .gitignore)"));
    assert!(prompt.contains("- find: Find files by glob pattern (respects .gitignore)"));
    assert!(prompt.contains("- ls: List directory contents"));
    assert!(!prompt.contains("Use bash for file operations"), "{prompt}");

    let none = harness
        .session_with(|options| options.builtin_tools = Some(Vec::new()))
        .await;
    assert!(none.active_tools().is_empty());
}

#[tokio::test]
async fn a_host_tool_with_a_built_in_name_replaces_it() {
    let harness = Harness::new();
    let mut read = echo_tool(true);
    read.tool = {
        let echo = echo_tool(true).tool;
        Arc::new(Renamed {
            inner: echo,
            declaration: pi_ai::Tool::new("read", "The host's read", json!({"type": "object"})),
        })
    };
    let session = harness
        .session_with(|options| {
            with_builtins(options);
            options.tools = vec![read];
        })
        .await;
    assert_eq!(session.active_tools(), ["read", "bash", "edit", "write"]);
    assert!(
        session
            .extension_context()
            .next_system_prompt()
            .contains("- read: Echo text back")
    );
}

struct Renamed {
    inner: Arc<dyn pi_agent_core::AgentTool>,
    declaration: pi_ai::Tool,
}

#[async_trait::async_trait]
impl pi_agent_core::AgentTool for Renamed {
    fn declaration(&self) -> &pi_ai::Tool {
        &self.declaration
    }

    async fn execute(
        &self,
        invocation: pi_agent_core::ToolInvocation,
    ) -> Result<pi_agent_core::AgentToolResult, pi_agent_core::ToolError> {
        self.inner.execute(invocation).await
    }
}

#[cfg(unix)]
#[tokio::test]
async fn bash_runs_in_the_session_folder_with_its_variables() {
    let dir = tempfile::tempdir().unwrap();
    let harness = Harness::new();
    harness.faux.push_message(vec![faux_tool_call(
        "bash",
        json!({"command": "pwd; printf '%s|%s' \"$MAPLE_SESSION_ID\" \"$MAPLE_MODEL\""}),
    )]);
    harness.faux.push_message(vec![faux_tool_call(
        "write",
        json!({"path": "notes/hello.txt", "content": "hi"}),
    )]);
    harness.faux.push_text("Done.");
    let cwd = dir.path().to_path_buf();
    let session = harness
        .session_with(|options| {
            with_builtins(options);
            options.cwd = cwd.clone();
            options.session = SessionManager::in_memory(cwd.to_string_lossy());
        })
        .await;
    let session_id = session.session_id();
    session
        .prompt("Where?", PromptOptions::default())
        .await
        .unwrap();

    let request = &harness.faux.requests()[1];
    let output = request
        .context
        .messages
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(result) => Some(content_text(&result.content)),
            _ => None,
        })
        .unwrap();
    let mut lines = output.lines();
    assert_eq!(
        std::fs::canonicalize(lines.next().unwrap()).unwrap(),
        std::fs::canonicalize(dir.path()).unwrap()
    );
    assert_eq!(
        lines.next().unwrap(),
        format!("{session_id}|{}", harness.faux.model().id)
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("notes/hello.txt")).unwrap(),
        "hi"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn bash_streams_its_output_without_flooding_the_interface() {
    let dir = tempfile::tempdir().unwrap();
    let harness = Harness::new();
    harness.faux.push_message(vec![faux_tool_call(
        "bash",
        json!({"command": "for i in $(seq 1 5000); do echo line $i; done"}),
    )]);
    harness.faux.push_text("Done.");
    let cwd = dir.path().to_path_buf();
    let session = harness
        .session_with(|options| {
            with_builtins(options);
            options.cwd = cwd.clone();
        })
        .await;
    let updates: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = updates.clone();
    session.subscribe(move |event| {
        if let AgentSessionEvent::Agent(AgentEvent::ToolExecutionUpdate {
            partial_result, ..
        }) = event
        {
            sink.lock()
                .unwrap()
                .push(content_text(&partial_result.content));
        }
    });
    session
        .prompt("Count", PromptOptions::default())
        .await
        .unwrap();

    let updates = updates.lock().unwrap();
    assert!(!updates.is_empty());
    assert!(updates.len() < 25, "{} updates", updates.len());
    assert!(updates.last().unwrap().contains("line 5000"));
}

#[cfg(unix)]
#[tokio::test]
async fn the_settings_command_prefix_reaches_bash() {
    let dir = tempfile::tempdir().unwrap();
    let harness = Harness::new();
    harness.faux.push_message(vec![faux_tool_call(
        "bash",
        json!({"command": "echo $FROM_PREFIX"}),
    )]);
    harness.faux.push_text("Done.");
    let cwd = dir.path().to_path_buf();
    let session = harness
        .session_with(|options| {
            with_builtins(options);
            options.cwd = cwd.clone();
            options.settings.shell_command_prefix = Some("export FROM_PREFIX=prefixed".into());
        })
        .await;
    session
        .prompt("Go", PromptOptions::default())
        .await
        .unwrap();
    let request = &harness.faux.requests()[1];
    let output = request
        .context
        .messages
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(result) => Some(content_text(&result.content)),
            _ => None,
        })
        .unwrap();
    assert_eq!(output.trim(), "prefixed");
}
