#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pi_agent_core::{
    AgentEvent, AgentEventSink, AgentMessage, AgentTool, AgentToolResult, FnTool, MessageQueues,
};
use pi_ai::{Message, Tool, UserMessage};
use serde_json::{Value, json};

pub fn user(text: &str) -> Message {
    Message::User(UserMessage::text(text))
}

/// A short name for an event, with the message role or tool name.
pub fn kind<M: AgentMessage>(event: &AgentEvent<M>) -> String {
    let role = |message: &M| {
        message
            .as_message()
            .map(Message::role)
            .unwrap_or("custom")
            .to_string()
    };
    match event {
        AgentEvent::AgentStart => "agent_start".into(),
        AgentEvent::AgentEnd { .. } => "agent_end".into(),
        AgentEvent::TurnStart => "turn_start".into(),
        AgentEvent::TurnEnd { .. } => "turn_end".into(),
        AgentEvent::MessageStart { message } => format!("message_start:{}", role(message)),
        AgentEvent::MessageUpdate { .. } => "message_update".into(),
        AgentEvent::MessageEnd { message } => format!("message_end:{}", role(message)),
        AgentEvent::ToolExecutionStart { tool_name, .. } => format!("tool_start:{tool_name}"),
        AgentEvent::ToolExecutionUpdate { tool_name, .. } => format!("tool_update:{tool_name}"),
        AgentEvent::ToolExecutionEnd { tool_name, .. } => format!("tool_end:{tool_name}"),
    }
}

type Callback<M> = Box<dyn Fn(&AgentEvent<M>) + Send + Sync>;

/// Records every event a run emits.
pub struct RecordingSink<M> {
    pub events: Mutex<Vec<AgentEvent<M>>>,
    on_event: Option<Callback<M>>,
}

impl<M> Default for RecordingSink<M> {
    fn default() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            on_event: None,
        }
    }
}

impl<M: AgentMessage> RecordingSink<M> {
    pub fn with_callback(callback: impl Fn(&AgentEvent<M>) + Send + Sync + 'static) -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            on_event: Some(Box::new(callback)),
        }
    }

    pub fn kinds(&self) -> Vec<String> {
        self.events.lock().unwrap().iter().map(kind).collect()
    }

    /// Event kinds without streaming deltas.
    pub fn lifecycle(&self) -> Vec<String> {
        self.kinds()
            .into_iter()
            .filter(|kind| kind != "message_update")
            .collect()
    }
}

#[async_trait]
impl<M: AgentMessage> AgentEventSink<M> for RecordingSink<M> {
    async fn emit(&self, event: AgentEvent<M>) {
        if let Some(callback) = &self.on_event {
            callback(&event);
        }
        self.events.lock().unwrap().push(event);
    }
}

/// Queues that hand out one scripted batch per poll.
pub struct ScriptedQueues<M> {
    pub steering: Mutex<VecDeque<Vec<M>>>,
    pub follow_ups: Mutex<VecDeque<Vec<M>>>,
    pub steering_polls: Mutex<usize>,
}

impl<M> Default for ScriptedQueues<M> {
    fn default() -> Self {
        Self {
            steering: Mutex::new(VecDeque::new()),
            follow_ups: Mutex::new(VecDeque::new()),
            steering_polls: Mutex::new(0),
        }
    }
}

#[async_trait]
impl<M: AgentMessage> MessageQueues<M> for ScriptedQueues<M> {
    async fn steering(&self) -> Vec<M> {
        *self.steering_polls.lock().unwrap() += 1;
        self.steering
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_default()
    }

    async fn follow_ups(&self) -> Vec<M> {
        self.follow_ups
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_default()
    }
}

pub fn text_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "text": { "type": "string" } },
        "required": ["text"],
    })
}

/// A tool that answers with its `text` argument.
pub fn echo_tool() -> Arc<dyn AgentTool> {
    FnTool::new(
        Tool::new("echo", "Echo the text", text_schema()),
        |invocation| async move {
            let text = match &invocation.args["text"] {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            Ok(AgentToolResult::text(text))
        },
    )
    .shared()
}

/// The text of a tool result message.
pub fn result_text(message: &Message) -> String {
    match message {
        Message::ToolResult(result) => pi_ai::content_text(&result.content),
        other => panic!("expected a tool result, got {other:?}"),
    }
}
