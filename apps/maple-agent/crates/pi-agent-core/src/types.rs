use std::fmt;
use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;
use pi_ai::{
    AssistantMessage, AssistantMessageEvent, Content, Message, Tool, ToolResultMessage, Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// A transcript entry. Model-facing messages enter through [`AgentMessage::from_message`];
/// an application's own message kinds are anything else the type can hold, and the
/// agent's [`crate::AgentHooks::convert_to_llm`] decides what the model sees of them.
pub trait AgentMessage: Clone + fmt::Debug + Send + Sync + 'static {
    fn from_message(message: Message) -> Self;
    /// The model-facing message this is, if it is one.
    fn as_message(&self) -> Option<&Message>;
}

impl AgentMessage for Message {
    fn from_message(message: Message) -> Self {
        message
    }

    fn as_message(&self) -> Option<&Message> {
        Some(self)
    }
}

pub(crate) fn as_assistant<M: AgentMessage>(message: &M) -> Option<&AssistantMessage> {
    match message.as_message() {
        Some(Message::Assistant(assistant)) => Some(assistant),
        _ => None,
    }
}

pub(crate) fn is_system<M: AgentMessage>(message: &M) -> bool {
    matches!(message.as_message(), Some(Message::System(_)))
}

/// What a tool returns. `content` goes to the model; `details` stays with the host.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AgentToolResult {
    pub content: Vec<Content>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    pub is_error: bool,
    /// Stop after this tool batch. Honored only when every result in the batch asks.
    pub terminate: bool,
}

impl AgentToolResult {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![Content::text(text)],
            ..Self::default()
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            is_error: true,
            ..Self::text(text)
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
}

/// An error a tool reports; its text becomes an error result for the model.
pub type ToolError = Box<dyn std::error::Error + Send + Sync>;

pub(crate) struct ToolUpdate {
    pub call_id: String,
    pub tool_name: String,
    pub args: Value,
    pub partial: AgentToolResult,
}

/// Reports partial results while a tool runs, as `ToolExecutionUpdate` events.
#[derive(Clone)]
pub struct ToolUpdates {
    sender: Option<mpsc::UnboundedSender<ToolUpdate>>,
    call_id: String,
    tool_name: String,
    args: Value,
}

impl ToolUpdates {
    /// Updates that go nowhere, for calling a tool outside the loop.
    pub fn none() -> Self {
        Self {
            sender: None,
            call_id: String::new(),
            tool_name: String::new(),
            args: Value::Null,
        }
    }

    pub(crate) fn new(
        sender: mpsc::UnboundedSender<ToolUpdate>,
        call_id: &str,
        tool_name: &str,
        args: &Value,
    ) -> Self {
        Self {
            sender: Some(sender),
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            args: args.clone(),
        }
    }

    pub fn send(&self, partial: AgentToolResult) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(ToolUpdate {
                call_id: self.call_id.clone(),
                tool_name: self.tool_name.clone(),
                args: self.args.clone(),
                partial,
            });
        }
    }
}

/// One call of a tool.
pub struct ToolInvocation {
    pub call_id: String,
    /// Arguments validated against the tool's schema.
    pub args: Value,
    pub cancel: CancellationToken,
    pub updates: ToolUpdates,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionMode {
    /// One call at a time, each result emitted as it finishes.
    Sequential,
    /// Calls run concurrently; results are appended in call order.
    #[default]
    Parallel,
}

/// How queued steering and follow-up messages are drained.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueMode {
    All,
    #[default]
    OneAtATime,
}

/// A tool the agent can run.
#[async_trait]
pub trait AgentTool: Send + Sync {
    /// The declaration the model sees.
    fn declaration(&self) -> &Tool;

    fn name(&self) -> &str {
        &self.declaration().name
    }

    /// A human-readable name for interfaces.
    fn label(&self) -> &str {
        self.name()
    }

    /// Force this tool to run on its own even when the agent runs tools in parallel.
    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        None
    }

    /// Adapt raw arguments before validation, for older argument shapes.
    fn prepare_arguments(&self, arguments: Map<String, Value>) -> Map<String, Value> {
        arguments
    }

    async fn execute(&self, invocation: ToolInvocation) -> Result<AgentToolResult, ToolError>;
}

/// A tool from a declaration and an async function.
pub struct FnTool<F> {
    declaration: Tool,
    execution_mode: Option<ToolExecutionMode>,
    run: F,
}

impl<F, Fut> FnTool<F>
where
    F: Fn(ToolInvocation) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<AgentToolResult, ToolError>> + Send + 'static,
{
    pub fn new(declaration: Tool, run: F) -> Self {
        Self {
            declaration,
            execution_mode: None,
            run,
        }
    }

    pub fn sequential(mut self) -> Self {
        self.execution_mode = Some(ToolExecutionMode::Sequential);
        self
    }

    pub fn shared(self) -> Arc<dyn AgentTool> {
        Arc::new(self)
    }
}

#[async_trait]
impl<F, Fut> AgentTool for FnTool<F>
where
    F: Fn(ToolInvocation) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<AgentToolResult, ToolError>> + Send + 'static,
{
    fn declaration(&self) -> &Tool {
        &self.declaration
    }

    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        self.execution_mode
    }

    async fn execute(&self, invocation: ToolInvocation) -> Result<AgentToolResult, ToolError> {
        (self.run)(invocation).await
    }
}

/// The transcript and the executable tools a run works on.
#[derive(Clone)]
pub struct AgentContext<M> {
    pub messages: Vec<M>,
    pub tools: Vec<Arc<dyn AgentTool>>,
}

impl<M> AgentContext<M> {
    pub fn new(messages: Vec<M>, tools: Vec<Arc<dyn AgentTool>>) -> Self {
        Self { messages, tools }
    }
}

impl<M: fmt::Debug> fmt::Debug for AgentContext<M> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentContext")
            .field("messages", &self.messages)
            .field(
                "tools",
                &self
                    .tools
                    .iter()
                    .map(|tool| tool.name())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// Lifecycle events of a run, in order.
#[derive(Clone, Debug)]
pub enum AgentEvent<M> {
    AgentStart,
    /// The last event of a run, with the messages it added.
    AgentEnd {
        messages: Vec<M>,
    },
    TurnStart,
    TurnEnd {
        message: M,
        tool_results: Vec<ToolResultMessage>,
    },
    MessageStart {
        message: M,
    },
    /// A delta of the assistant response in progress.
    MessageUpdate {
        event: AssistantMessageEvent,
    },
    MessageEnd {
        message: M,
    },
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: Value,
    },
    ToolExecutionUpdate {
        tool_call_id: String,
        tool_name: String,
        args: Value,
        partial_result: AgentToolResult,
    },
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        result: AgentToolResult,
        is_error: bool,
    },
}

/// Why the agent refused a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentError {
    /// A run is active; queue with steer or follow-up, or wait for it.
    AlreadyRunning,
    NoMessages,
    /// The transcript ends with an assistant message and nothing is queued.
    CannotContinueFromAssistant,
}

impl fmt::Display for AgentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::AlreadyRunning => {
                "Agent is already processing. Steer or queue a follow-up, or wait for it to finish."
            }
            Self::NoMessages => "No messages to continue from",
            Self::CannotContinueFromAssistant => "Cannot continue from an assistant message",
        })
    }
}

impl std::error::Error for AgentError {}
