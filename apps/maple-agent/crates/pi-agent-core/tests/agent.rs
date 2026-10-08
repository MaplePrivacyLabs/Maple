mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use common::{echo_tool, kind, user};
use pi_agent_core::{
    Agent, AgentError, AgentEvent, AgentListener, AgentOptions, AgentToolResult, FnTool, QueueMode,
};
use pi_ai::faux::FauxProvider;
use pi_ai::{Message, StopReason, Tool};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn agent(faux: &FauxProvider) -> Agent<Message> {
    let mut options = AgentOptions::new(faux.model(), Arc::new(faux.clone()));
    options.system_prompt = "You are terse.".into();
    options.tools = vec![echo_tool()];
    Agent::new(options)
}

fn roles(messages: &[Message]) -> Vec<&'static str> {
    messages.iter().map(Message::role).collect()
}

fn record(agent: &Agent<Message>) -> Arc<Mutex<Vec<String>>> {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    agent.subscribe_fn(move |event| {
        if !matches!(event, AgentEvent::MessageUpdate { .. }) {
            sink.lock().unwrap().push(kind(event));
        }
    });
    events
}

#[tokio::test]
async fn the_system_prompt_and_tools_lead_the_transcript() {
    let faux = FauxProvider::new();
    let agent = agent(&faux);
    let messages = agent.messages();
    assert_eq!(roles(&messages), ["system"]);
    let Message::System(system) = &messages[0] else {
        panic!()
    };
    assert_eq!(system.tools_added[0].name, "echo");
    assert_eq!(agent.system_prompt(), "You are terse.");
}

#[tokio::test]
async fn a_prompt_runs_to_completion_and_listeners_see_every_event() {
    let faux = FauxProvider::new();
    faux.push_tool_call("echo", json!({ "text": "hi" }));
    faux.push_text("done");
    let agent = agent(&faux);
    let events = record(&agent);

    agent.prompt_text("go").await.unwrap();

    assert!(!agent.is_streaming());
    assert_eq!(
        roles(&agent.messages()),
        ["system", "user", "assistant", "toolResult", "assistant"]
    );
    let events = events.lock().unwrap();
    assert_eq!(events.first().map(String::as_str), Some("agent_start"));
    assert_eq!(events.last().map(String::as_str), Some("agent_end"));
    assert!(events.contains(&"tool_end:echo".to_string()));
    assert!(agent.streaming_message().is_none());
    assert!(agent.pending_tool_calls().is_empty());
}

#[tokio::test]
async fn a_second_prompt_during_a_run_is_refused() {
    let faux = FauxProvider::new();
    faux.push_hang();
    let agent = agent(&faux);
    let started = Arc::new(tokio::sync::Notify::new());
    let notify = started.clone();
    agent.subscribe_fn(move |event| {
        if matches!(
            event,
            AgentEvent::MessageStart {
                message: Message::Assistant(_)
            }
        ) {
            notify.notify_one();
        }
    });
    let running = tokio::spawn({
        let agent = agent.clone();
        async move { agent.prompt_text("first").await }
    });
    started.notified().await;

    assert!(agent.is_streaming());
    assert!(agent.streaming_message().is_some());
    assert_eq!(
        agent.prompt_text("second").await.unwrap_err(),
        AgentError::AlreadyRunning
    );

    agent.abort();
    running.await.unwrap().unwrap();
    agent.wait_for_idle().await;
    let messages = agent.messages();
    let Message::Assistant(reply) = messages.last().unwrap() else {
        panic!()
    };
    assert_eq!(reply.stop_reason, StopReason::Aborted);
    assert_eq!(
        agent.error_message().as_deref(),
        Some("Request was aborted")
    );
}

#[tokio::test]
async fn steering_from_a_listener_lands_after_the_turn() {
    let faux = FauxProvider::new();
    faux.push_tool_call("echo", json!({ "text": "a" }));
    faux.push_text("adjusted");
    let agent = agent(&faux);
    let steerer = agent.clone();
    let steered = AtomicBool::new(false);
    agent.subscribe_fn(move |event| {
        if matches!(event, AgentEvent::ToolExecutionStart { .. })
            && !steered.swap(true, Ordering::SeqCst)
        {
            steerer.steer(user("use b instead"));
        }
    });

    agent.prompt_text("go").await.unwrap();
    assert_eq!(
        roles(&agent.messages()),
        [
            "system",
            "user",
            "assistant",
            "toolResult",
            "user",
            "assistant"
        ]
    );
}

#[tokio::test]
async fn follow_ups_wait_until_the_agent_would_stop() {
    let faux = FauxProvider::new();
    faux.push_tool_call("echo", json!({ "text": "a" }));
    faux.push_text("first answer");
    faux.push_text("second answer");
    let agent = agent(&faux);
    let queuer = agent.clone();
    let queued = AtomicBool::new(false);
    agent.subscribe_fn(move |event| {
        if matches!(event, AgentEvent::ToolExecutionStart { .. })
            && !queued.swap(true, Ordering::SeqCst)
        {
            queuer.follow_up(user("one more thing"));
        }
    });

    agent.prompt_text("go").await.unwrap();
    assert_eq!(
        roles(&agent.messages()),
        [
            "system",
            "user",
            "assistant",
            "toolResult",
            "assistant",
            "user",
            "assistant"
        ]
    );
}

#[tokio::test]
async fn queue_modes_decide_how_many_messages_a_turn_takes() {
    for (mode, requests) in [(QueueMode::OneAtATime, 2), (QueueMode::All, 1)] {
        let faux = FauxProvider::new();
        faux.push_text("one");
        faux.push_text("two");
        let agent = agent(&faux);
        agent.set_steering_mode(mode);
        agent.steer(user("first note"));
        agent.steer(user("second note"));
        assert_eq!(
            agent.peek_queued_messages().len(),
            if mode == QueueMode::All { 2 } else { 1 }
        );

        agent.prompt_text("go").await.unwrap();
        assert_eq!(faux.requests().len(), requests, "{mode:?}");
        assert!(!agent.has_queued_messages());
    }
}

#[tokio::test]
async fn continue_run_resumes_from_tool_results_or_queued_messages() {
    let faux = FauxProvider::new();
    let agent = agent(&faux);
    assert_eq!(
        agent.continue_run().await.unwrap_err(),
        AgentError::NoMessages
    );

    faux.push_text("answer");
    agent.append_message(user("question"));
    agent.continue_run().await.unwrap();
    assert_eq!(roles(&agent.messages()), ["system", "user", "assistant"]);

    assert_eq!(
        agent.continue_run().await.unwrap_err(),
        AgentError::CannotContinueFromAssistant
    );

    faux.push_text("follow-up answer");
    agent.follow_up(user("another"));
    agent.continue_run().await.unwrap();
    assert_eq!(
        roles(&agent.messages()),
        ["system", "user", "assistant", "user", "assistant"]
    );
}

#[tokio::test]
async fn reset_keeps_the_prompt_and_tools_and_clears_the_rest() {
    let faux = FauxProvider::new();
    faux.push_text("hello");
    let agent = agent(&faux);
    agent.prompt_text("hi").await.unwrap();
    agent.follow_up(user("later"));

    agent.reset().unwrap();
    assert_eq!(roles(&agent.messages()), ["system"]);
    assert_eq!(agent.system_prompt(), "You are terse.");
    assert!(!agent.has_queued_messages());
}

#[tokio::test]
async fn pending_tool_calls_are_tracked_while_tools_run() {
    let faux = FauxProvider::new();
    faux.push_tool_call("inspect", json!({}));
    faux.push_text("done");
    let mut options = AgentOptions::new(faux.model(), Arc::new(faux.clone()));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let agent_slot: Arc<Mutex<Option<Agent<Message>>>> = Arc::new(Mutex::new(None));
    let (slot, record) = (agent_slot.clone(), seen.clone());
    options.tools = vec![
        FnTool::new(
            Tool::new("inspect", "Inspect", json!({ "type": "object" })),
            move |invocation| {
                let (slot, record) = (slot.clone(), record.clone());
                async move {
                    let agent = slot.lock().unwrap().clone().unwrap();
                    let pending = agent.pending_tool_calls();
                    record
                        .lock()
                        .unwrap()
                        .push(pending.contains(&invocation.call_id));
                    Ok(AgentToolResult::text("ok"))
                }
            },
        )
        .shared(),
    ];
    let agent = Agent::new(options);
    *agent_slot.lock().unwrap() = Some(agent.clone());

    agent.prompt_text("go").await.unwrap();
    assert_eq!(*seen.lock().unwrap(), [true]);
    assert!(agent.pending_tool_calls().is_empty());
}

struct SlowListener {
    finished: Arc<AtomicBool>,
}

#[async_trait]
impl AgentListener<Message> for SlowListener {
    async fn on_event(&self, event: &AgentEvent<Message>, _cancel: &CancellationToken) {
        if matches!(event, AgentEvent::AgentEnd { .. }) {
            tokio::time::sleep(Duration::from_millis(20)).await;
            self.finished.store(true, Ordering::SeqCst);
        }
    }
}

#[tokio::test]
async fn a_run_settles_only_after_listeners_handle_its_end() {
    let faux = FauxProvider::new();
    faux.push_text("hi");
    let agent = agent(&faux);
    let finished = Arc::new(AtomicBool::new(false));
    agent.subscribe(Arc::new(SlowListener {
        finished: finished.clone(),
    }));
    agent.prompt_text("go").await.unwrap();
    assert!(finished.load(Ordering::SeqCst));
}

#[tokio::test]
async fn unsubscribed_listeners_stop_receiving_events() {
    let faux = FauxProvider::new();
    faux.push_text("one");
    faux.push_text("two");
    let agent = agent(&faux);
    let count = Arc::new(Mutex::new(0));
    let counter = count.clone();
    let id = agent.subscribe_fn(move |_| *counter.lock().unwrap() += 1);
    agent.prompt_text("first").await.unwrap();
    let after_first = *count.lock().unwrap();
    assert!(after_first > 0);

    agent.unsubscribe(id);
    agent.prompt_text("second").await.unwrap();
    assert_eq!(*count.lock().unwrap(), after_first);
}

#[tokio::test]
async fn changing_tools_declares_the_difference_on_the_next_request() {
    let faux = FauxProvider::new();
    faux.push_text("one");
    faux.push_text("two");
    let agent = agent(&faux);
    agent.prompt_text("first").await.unwrap();
    agent.set_tools(Vec::new());
    agent.prompt_text("second").await.unwrap();

    let messages = agent.messages();
    let Message::System(update) = &messages[3] else {
        panic!("expected a tool update, got {:?}", messages[3])
    };
    assert_eq!(update.tools_removed[0].name, "echo");
    let second = &faux.requests()[1];
    assert!(pi_ai::transcript::current_tools(&second.context.messages).is_empty());
}
