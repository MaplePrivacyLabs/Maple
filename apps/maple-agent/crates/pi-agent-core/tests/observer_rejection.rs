//! Frozen-source observer rejection regressions; pending sibling work must survive.
use pi_agent_core::{
    agent::{Agent, AgentInitialState, AgentOptions},
    types::*,
};
use pi_ai::{
    types::{
        AssistantContent, AssistantMessage, AssistantMessageEvent, DoneReason, Model, Schema,
        StopReason, Tool, ToolCall,
    },
    utils::event_stream::create_assistant_message_event_stream,
};
use pi_testkit::VirtualEnv;
use serde_json::json;
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq)]
enum Failure {
    ToolEnd,
    Update,
}

async fn check_observer_rejection(failure: Failure) {
    let release = CancellationToken::new();
    let started = CancellationToken::new();
    let rejected = CancellationToken::new();
    let sibling_finished = CancellationToken::new();
    let settled = CancellationToken::new();
    let events = Shared::new(Vec::new());
    let names = if failure == Failure::ToolEnd {
        vec!["fast", "slow"]
    } else {
        vec!["updates"]
    };
    let tools: AgentTools = names
        .iter()
        .map(|&name| {
            let (release, started, sibling_finished) =
                (release.clone(), started.clone(), sibling_finished.clone());
            Shared::new(AgentTool {
                tool: Tool {
                    name: name.into(),
                    parameters: Schema::typebox(json!({"type":"object","properties":{}})),
                    ..Tool::default()
                },
                label: name.into(),
                prepare_arguments: None,
                output_schema: None,
                replay: None,
                execution_mode: None,
                execute: Arc::new(move |_, _, _, on_update| {
                    let (release, started, sibling_finished) =
                        (release.clone(), started.clone(), sibling_finished.clone());
                    Box::pin(async move {
                        if name == "slow" {
                            started.cancel();
                            release.cancelled().await;
                            sibling_finished.cancel();
                        }
                        if name == "updates" {
                            let update = on_update.expect("update callback");
                            for id in ["fast", "slow"] {
                                update(AgentToolResult {
                                    content: Some(vec![]),
                                    details: Some(JsValue::from_json_with_js_numbers(
                                        json!({"id":id}),
                                    )),
                                    ..Default::default()
                                });
                            }
                        }
                        Ok(AgentToolResult {
                            content: Some(vec![]),
                            ..Default::default()
                        })
                    })
                }),
            })
        })
        .collect::<Vec<_>>()
        .into();
    let stream: StreamFn = Arc::new(move |_, _, _| {
        let names = names.clone();
        Box::pin(async move {
            let stream = create_assistant_message_event_stream();
            stream.push(AssistantMessageEvent::Done {
                reason: DoneReason::ToolUse,
                message: AssistantMessage {
                    content: names
                        .into_iter()
                        .map(|name| {
                            AssistantContent::ToolCall(ToolCall {
                                id: name.into(),
                                name: name.into(),
                                arguments: JsValue::from_json_with_js_numbers(json!({})).into(),
                                ..Default::default()
                            })
                        })
                        .collect(),
                    stop_reason: StopReason::ToolUse,
                    ..Default::default()
                },
            });
            Ok(stream)
        })
    });
    let agent = Agent::new(
        AgentOptions {
            initial_state: Some(AgentInitialState {
                model: Some(Shared::new(Model::default())),
                tools: Some(tools),
                ..Default::default()
            }),
            stream_fn: Some(stream),
            ..Default::default()
        },
        Arc::new(VirtualEnv::new(1_700_000_000_000)),
    )
    .unwrap();
    let (observed_events, observed_rejection, observed_start, observed_release, observed_finish) = (
        events.clone(),
        rejected.clone(),
        started.clone(),
        release.clone(),
        sibling_finished.clone(),
    );
    let _unsubscribe = agent.subscribe(Arc::new(move |event, _| {
        let (events, rejected, started, release, sibling_finished) = (observed_events.clone(), observed_rejection.clone(), observed_start.clone(), observed_release.clone(), observed_finish.clone());
        Box::pin(async move {
            let id = match &event { AgentEvent::ToolExecutionStart {tool_call_id,..} | AgentEvent::ToolExecutionEnd {tool_call_id,..} | AgentEvent::ToolExecutionUpdate {tool_call_id,..} => Some(tool_call_id.to_string_lossy()), _ => None };
            events.push(format!("{}{}", event.kind(), id.map(|id| format!(":{id}")).unwrap_or_default()));
            if failure == Failure::ToolEnd && matches!(event, AgentEvent::ToolExecutionEnd { ref tool_call_id,.. } if tool_call_id == "fast") {
                rejected.cancel(); return Err(AgentError::new("observer exploded"));
            }
            if failure == Failure::Update && let AgentEvent::ToolExecutionUpdate { partial_result, .. } = event {
                if partial_result.details.as_ref().unwrap()["id"].as_str() == Some("fast") { rejected.cancel(); return Err(AgentError::new("observer exploded")); }
                started.cancel(); release.cancelled().await; sibling_finished.cancel();
            }
            Ok(())
        })
    }));
    let prompt = agent.prompt("run");
    let observed_settlement = settled.clone();
    let task = tokio::spawn(async move {
        let result = prompt.await;
        observed_settlement.cancel();
        result
    });
    started.cancelled().await;
    rejected.cancelled().await;
    // Both relevant callbacks have entered. Only ready continuations remain in
    // the failure lifecycle, while the sibling's explicit gate stays closed.
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
    assert!(
        settled.is_cancelled(),
        "observer rejection must settle the prompt before the sibling gate opens"
    );
    assert!(!agent.state().is_streaming());
    assert_eq!(
        agent.state().error_message(),
        Some("observer exploded".into())
    );
    assert!(!sibling_finished.is_cancelled());
    assert!(agent.state().pending_tool_calls().snapshot().is_empty());
    assert!(events.snapshot().ends_with(&[
        "message_start".into(),
        "message_end".into(),
        "turn_end".into(),
        "agent_end".into()
    ]));
    let before_release = events.snapshot();
    release.cancel();
    task.await.unwrap().unwrap();
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
    assert!(
        sibling_finished.is_cancelled(),
        "already-started sibling work must survive the rejected batch"
    );
    assert_eq!(
        events.snapshot(),
        before_release,
        "closed Agent run has no further subscriber deliveries"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn rejected_tool_end_observer_finishes_prompt_before_blocked_tool() {
    check_observer_rejection(Failure::ToolEnd).await;
}
#[tokio::test(flavor = "current_thread")]
async fn rejected_update_observer_finishes_prompt_before_blocked_update_observer() {
    check_observer_rejection(Failure::Update).await;
}
