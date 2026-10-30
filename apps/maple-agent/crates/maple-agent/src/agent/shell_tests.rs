//! Execution-host folder selection and quiet shell progress, without remote
//! accounts, live services or the real application's store.
#![cfg(unix)]

use super::*;

#[tokio::test]
async fn saved_task_worktree_survives_a_different_runtime_default_and_cold_load() {
    let mut harness = Harness::new().await;
    let worktree = tempfile::tempdir().unwrap();
    std::fs::write(worktree.path().join("window.rs"), "worktree-fixture").unwrap();
    let task = harness
        .handle
        .create_session(Some(AgentCreateSessionRequest {
            project_root: Some(worktree.path().to_string_lossy().into_owned()),
            title: None,
            model: None,
            context_limit: None,
            mcp_server_names: Some(Vec::new()),
        }))
        .await
        .unwrap()
        .session
        .id;
    for cold in [false, true] {
        if cold {
            harness.restart().await;
            harness.start().await;
        }
        harness.faux.push_message(vec![faux_tool_call(
            "bash",
            json!({"command": "pwd; cat ./window.rs"}),
        )]);
        harness.faux.push_text("Done.");
        let mut run = harness.send(&task, "Read the fixture").await;
        assert_eq!(finished(&mut run).await, AgentRunTerminal::Completed);
        let rows = harness.recorder.live_rows(&run.run_id);
        let row = rows
            .iter()
            .rev()
            .find(|row| row.item_type == "tool")
            .unwrap();
        let output = row.output.as_ref().unwrap()["text"].as_str().unwrap();
        let mut lines = output.lines();
        assert_eq!(
            std::fs::canonicalize(lines.next().unwrap()).unwrap(),
            worktree.path().canonicalize().unwrap()
        );
        assert_eq!(lines.next(), Some("worktree-fixture"));
        let detail = harness.handle.load_session(task.clone()).await.unwrap();
        assert_eq!(
            Path::new(&detail.session.project_root)
                .canonicalize()
                .unwrap(),
            worktree.path().canonicalize().unwrap()
        );
    }
    harness.handle.stop().await.unwrap();
}

#[tokio::test]
async fn quiet_shell_reports_effective_folder_deadline_and_idle_time_until_stop() {
    let harness = Harness::new().await;
    let worktree = tempfile::tempdir().unwrap();
    harness.faux.push_message(vec![faux_tool_call(
        "bash",
        json!({
            "command": "printf fixture-output; sleep 30 # fixture-private-command",
        }),
    )]);
    let task = harness
        .handle
        .create_session(Some(AgentCreateSessionRequest {
            project_root: Some(worktree.path().to_string_lossy().into_owned()),
            title: None,
            model: None,
            context_limit: None,
            mcp_server_names: Some(Vec::new()),
        }))
        .await
        .unwrap()
        .session
        .id;
    let mut run = harness.send(&task, "Wait for cancellation").await;
    let run_id = run.run_id.clone();
    eventually(|| {
        harness.recorder.run_events(&run_id).iter().any(|event| {
            let AgentRunEvent::TimelineItem(row) = event else {
                return false;
            };
            let Some(progress) = row
                .output
                .as_ref()
                .and_then(|output| output.get("structuredContent"))
                .and_then(|details| details.get("shellExecution"))
            else {
                return false;
            };
            progress["elapsedSeconds"]
                .as_u64()
                .is_some_and(|seconds| seconds >= 5)
        })
    })
    .await;
    let rows = harness.recorder.live_rows(&run_id);
    let tool = rows.iter().find(|row| row.item_type == "tool").unwrap();
    assert_eq!(tool.status.as_deref(), Some("running"));
    assert_eq!(tool.output.as_ref().unwrap()["text"], "fixture-output");
    let progress = &tool.output.as_ref().unwrap()["structuredContent"]["shellExecution"];
    assert_eq!(
        progress["cwd"],
        json!(worktree.path().canonicalize().unwrap())
    );
    assert_eq!(progress["timeoutSeconds"], 1800.0);
    assert!(progress["outputIdleSeconds"].as_u64().unwrap() >= 4);
    assert!(!progress.to_string().contains("fixture-private-command"));
    let identity = tool.live.clone();
    assert!(identity.is_some());
    harness
        .handle
        .cancel_desktop_run(run_id.clone())
        .await
        .unwrap();
    assert_eq!(finished(&mut run).await, AgentRunTerminal::Cancelled);
    // Partial results replace the original tool occurrence and never settle it
    // early or create an extra row. The final cancellation removes live status.
    let rows = harness.recorder.live_rows(&run_id);
    let tools: Vec<_> = rows.iter().filter(|row| row.item_type == "tool").collect();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].live, identity);
    assert_ne!(tools[0].status.as_deref(), Some("running"));
    assert!(
        tools[0].output.as_ref().unwrap()["structuredContent"]
            .get("shellExecution")
            .is_none()
    );
    harness.handle.stop().await.unwrap();
}
