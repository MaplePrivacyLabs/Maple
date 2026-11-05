//! Run-owned shell status. Pi's partial output contract stays unchanged.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pi_agent_core::AgentEvent;
use pi_ai::Content;
use pi_coding_agent::SessionMessage;
use serde_json::json;
use tokio::sync::Notify;

use super::{AgentRunEvent, AgentTimelineItem, RunEvents, lock};
use crate::agent::tools::shell::timeout_seconds;

const STATUS_INTERVAL: Duration = Duration::from_secs(5);

pub(super) struct ShellProgress {
    cwd: Option<PathBuf>,
    calls: Mutex<HashMap<String, RunningShell>>,
    wake: Notify,
}

struct RunningShell {
    row: AgentTimelineItem,
    started: Instant,
    last_output: Instant,
    timeout: f64,
}

impl RunningShell {
    fn decorate(&self, row: &mut AgentTimelineItem, cwd: &std::path::Path) {
        let output = row.output.get_or_insert_with(|| json!({}));
        let details = &mut output["structuredContent"];
        if !details.is_object() {
            *details = json!({});
        }
        details["shellExecution"] = json!({
            "cwd": cwd.to_string_lossy(),
            "timeoutSeconds": self.timeout,
            "elapsedSeconds": self.started.elapsed().as_secs(),
            "outputIdleSeconds": self.last_output.elapsed().as_secs(),
        });
    }
}

impl ShellProgress {
    pub(super) fn new(cwd: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            cwd,
            calls: Mutex::new(HashMap::new()),
            wake: Notify::new(),
        })
    }

    pub(super) fn publish(
        &self,
        event: &AgentEvent<SessionMessage>,
        mut rows: Vec<AgentTimelineItem>,
        tool: Option<&str>,
        completed: bool,
        events: &RunEvents,
    ) {
        // Hold this through publication: a heartbeat and End cannot overtake
        // each other between checking ownership and sending their rows.
        let mut calls = lock(&self.calls);
        match event {
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } if matches!(tool_name.as_str(), "bash" | "powershell") => {
                if self.cwd.is_some()
                    && let Some(row) = rows.first()
                {
                    let now = Instant::now();
                    calls.insert(
                        tool_call_id.clone(),
                        RunningShell {
                            row: row.clone(),
                            started: now,
                            last_output: now,
                            timeout: timeout_seconds(args.get("timeout").and_then(|v| v.as_f64())),
                        },
                    );
                    self.wake.notify_one();
                }
            }
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                partial_result,
                tool_name,
                ..
            } => {
                // Projection and publication are separate steps. An update
                // projected before End may arrive here after it; fence that
                // stale row using the same lock as terminal publication.
                if matches!(tool_name.as_str(), "bash" | "powershell")
                    && self.cwd.is_some()
                    && !calls.contains_key(tool_call_id)
                {
                    return;
                }
                if let Some(call) = calls.get_mut(tool_call_id)
                    && partial_result.content.iter().any(
                        |content| matches!(content, Content::Text(text) if !text.text.is_empty()),
                    )
                {
                    call.last_output = Instant::now();
                }
            }
            AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                calls.remove(tool_call_id);
            }
            _ => {}
        }
        for row in &mut rows {
            if let Some(call) = tool.and_then(|tool| calls.get_mut(tool))
                && let Some(cwd) = &self.cwd
            {
                call.decorate(row, cwd);
                call.row = row.clone();
            }
            events.publish_row(AgentRunEvent::TimelineItem(row.clone()), tool, completed);
        }
    }

    pub(super) fn start(self: &Arc<Self>, events: RunEvents) -> ShellStatusTask {
        let progress = Arc::clone(self);
        ShellStatusTask(tokio::spawn(async move {
            loop {
                progress.wake.notified().await;
                loop {
                    tokio::time::sleep(STATUS_INTERVAL).await;
                    if !progress.heartbeat(&events) {
                        break;
                    }
                }
            }
        }))
    }

    fn heartbeat(&self, events: &RunEvents) -> bool {
        let calls = lock(&self.calls);
        if let Some(cwd) = &self.cwd {
            for (id, call) in calls.iter() {
                let mut row = call.row.clone();
                call.decorate(&mut row, cwd);
                events.publish_row(AgentRunEvent::TimelineItem(row), Some(id), false);
            }
        }
        !calls.is_empty()
    }
}

pub(super) struct ShellStatusTask(tokio::task::JoinHandle<()>);

impl ShellStatusTask {
    pub(super) async fn stop(mut self) {
        self.0.abort();
        let _ = (&mut self.0).await;
    }
}

impl Drop for ShellStatusTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentEventDispatcher, tests::Recorder, timeline};
    use pi_agent_core::AgentToolResult;

    #[test]
    fn heartbeat_keeps_output_and_truncation_but_never_revives_a_finished_call() {
        let dir = tempfile::tempdir().unwrap();
        let progress = ShellProgress::new(Some(dir.path().to_path_buf()));
        let (events, mut receiver) = RunEvents::new(
            AgentEventDispatcher::new(Arc::new(Recorder::default())),
            "task",
            "run",
        );
        for id in ["first", "second"] {
            let mut row = timeline::tool_result_item(id.into(), &[], None, false, 0);
            row.status = Some("running".into());
            progress.publish(
                &AgentEvent::ToolExecutionStart {
                    tool_call_id: id.into(),
                    tool_name: "bash".into(),
                    args: json!({"command": "fixture"}),
                },
                vec![row],
                Some(id),
                false,
                &events,
            );
        }
        let partial = AgentToolResult::text("retained output")
            .with_details(json!({"fullOutputPath": "fixture.log"}));
        let mut row = timeline::tool_result_item(
            "second".into(),
            &partial.content,
            partial.details.as_ref(),
            false,
            0,
        );
        row.status = Some("running".into());
        progress.publish(
            &AgentEvent::ToolExecutionUpdate {
                tool_call_id: "second".into(),
                tool_name: "bash".into(),
                args: json!({}),
                partial_result: partial,
            },
            vec![row],
            Some("second"),
            false,
            &events,
        );
        progress.publish(
            &AgentEvent::ToolExecutionEnd {
                tool_call_id: "first".into(),
                tool_name: "bash".into(),
                result: AgentToolResult::text("done"),
                is_error: false,
            },
            vec![timeline::tool_result_item(
                "first".into(),
                &[Content::text("done")],
                None,
                false,
                0,
            )],
            Some("first"),
            false,
            &events,
        );
        while receiver.try_recv().is_ok() {}
        let mut stale =
            timeline::tool_result_item("first".into(), &[Content::text("late")], None, false, 0);
        stale.status = Some("running".into());
        progress.publish(
            &AgentEvent::ToolExecutionUpdate {
                tool_call_id: "first".into(),
                tool_name: "bash".into(),
                args: json!({}),
                partial_result: AgentToolResult::text("late"),
            },
            vec![stale],
            Some("first"),
            false,
            &events,
        );
        assert!(
            receiver.try_recv().is_err(),
            "a previously projected update cannot overtake End"
        );
        assert!(progress.heartbeat(&events));
        let AgentRunEvent::TimelineItem(row) = receiver.try_recv().unwrap() else {
            panic!("heartbeat row")
        };
        assert_eq!(row.id, "second");
        let output = row.output.unwrap();
        assert_eq!(output["text"], "retained output");
        assert_eq!(output["structuredContent"]["fullOutputPath"], "fixture.log");
        assert!(
            receiver.try_recv().is_err(),
            "the finished call emits no heartbeat"
        );
    }
}
