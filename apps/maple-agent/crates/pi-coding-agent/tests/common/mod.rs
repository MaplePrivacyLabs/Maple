#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use pi_agent_core::{AgentEvent, AgentToolResult, FnTool};
use pi_ai::faux::FauxProvider;
use pi_ai::{Message, Model, Tool, content_text};
use pi_coding_agent::extensions::{Extension, RegisteredTool, ToolPrompt};
use pi_coding_agent::session::{EntryKind, SessionManager};
use pi_coding_agent::{
    AgentSession, AgentSessionEvent, AgentSessionOptions, ModelRegistry, SessionMessage, StaticKeys,
};
use serde_json::{Value, json};

pub struct Harness {
    pub faux: FauxProvider,
    pub models: ModelRegistry,
}

impl Harness {
    pub fn new() -> Self {
        Self::with_model(FauxProvider::default_model())
    }

    pub fn with_model(model: Model) -> Self {
        let faux = FauxProvider::with_model(model.clone());
        let models = ModelRegistry::new(Arc::new(StaticKeys::default()));
        models.register_api(&model.api, Arc::new(faux.clone()));
        models.register_models([model]);
        Self { faux, models }
    }

    pub fn options(&self) -> AgentSessionOptions {
        let mut options = AgentSessionOptions::new(
            "/work",
            "Maple",
            SessionManager::in_memory("/work"),
            self.models.clone(),
        );
        options.model = Some(self.faux.model());
        options.settings.retry.base_delay_ms = 1;
        // These tests use the host's tools alone; the built-in tools have their own.
        options.builtin_tools = Some(Vec::new());
        options
    }

    pub async fn session(&self) -> AgentSession {
        AgentSession::new(self.options()).await.unwrap()
    }

    pub async fn session_with(
        &self,
        configure: impl FnOnce(&mut AgentSessionOptions),
    ) -> AgentSession {
        let mut options = self.options();
        configure(&mut options);
        AgentSession::new(options).await.unwrap()
    }

    pub async fn session_with_extensions(
        &self,
        extensions: Vec<Arc<dyn Extension>>,
    ) -> AgentSession {
        self.session_with(|options| options.extensions = extensions)
            .await
    }

    /// The text of the last user message of request `index`.
    pub fn sent_text(&self, index: usize) -> String {
        let request = &self.faux.requests()[index];
        request
            .context
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::User(user) => Some(content_text(&user.content)),
                _ => None,
            })
            .unwrap_or_default()
    }
}

pub fn echo_tool(active: bool) -> RegisteredTool {
    let tool = FnTool::new(
        Tool::new(
            "echo",
            "Echo the text",
            json!({ "type": "object", "properties": { "text": { "type": "string" } }, "required": ["text"] }),
        ),
        |invocation| async move {
            Ok(AgentToolResult::text(
                invocation.args["text"].as_str().unwrap_or_default(),
            ))
        },
    );
    RegisteredTool {
        tool: tool.shared(),
        prompt: ToolPrompt {
            snippet: Some("Echo text back".into()),
            guidelines: vec!["Echo only when asked".into()],
        },
        active,
        extension: None,
    }
}

/// Short names of the events a session reports.
pub fn record(session: &AgentSession) -> Arc<Mutex<Vec<String>>> {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    session.subscribe(move |event| {
        let name = match event {
            AgentSessionEvent::Agent(AgentEvent::MessageUpdate { .. }) => return,
            AgentSessionEvent::Agent(AgentEvent::MessageEnd { message }) => {
                format!("message_end:{}", message.role())
            }
            AgentSessionEvent::Agent(AgentEvent::ToolExecutionEnd { tool_name, .. }) => {
                format!("tool_end:{tool_name}")
            }
            AgentSessionEvent::Agent(_) => return,
            AgentSessionEvent::QueueUpdate {
                steering,
                follow_up,
            } => format!("queue:{}:{}", steering.len(), follow_up.len()),
            AgentSessionEvent::CompactionStart { reason } => format!("compaction_start:{reason:?}"),
            AgentSessionEvent::CompactionEnd { reason, error, .. } => {
                format!("compaction_end:{reason:?}:{}", error.is_none())
            }
            AgentSessionEvent::RetryStart { attempt, .. } => format!("retry_start:{attempt}"),
            AgentSessionEvent::RetryEnd {
                success, attempt, ..
            } => format!("retry_end:{success}:{attempt}"),
            AgentSessionEvent::ExtensionError(report) => {
                format!("extension_error:{}:{}", report.extension, report.event)
            }
            AgentSessionEvent::PersistenceError(_) => "persistence_error".into(),
            AgentSessionEvent::BashExecutionUpdate { delta, .. } => format!("bash:{delta}"),
            AgentSessionEvent::Settled => "settled".into(),
        };
        sink.lock().unwrap().push(name);
    });
    events
}

pub fn roles(messages: &[SessionMessage]) -> Vec<&'static str> {
    messages.iter().map(SessionMessage::role).collect()
}

/// The kinds of the session's entries, with message roles.
pub fn entry_kinds(session: &AgentSession) -> Vec<String> {
    session.with_session(|tree| {
        tree.entries()
            .iter()
            .map(|entry| match &entry.kind {
                EntryKind::Message { message } => format!("message:{}", message.role()),
                other => serde_json::to_value(other).unwrap()["type"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            })
            .collect()
    })
}

pub fn last_assistant_text(session: &AgentSession) -> String {
    session
        .messages()
        .iter()
        .rev()
        .find_map(|message| match message {
            SessionMessage::Llm(Message::Assistant(assistant)) => Some(assistant.text()),
            _ => None,
        })
        .unwrap_or_default()
}

pub fn json_text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_string()
}
