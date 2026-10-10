use async_trait::async_trait;
use pi_ai::{
    AssistantMessage, Content, Message, Model, ThinkingLevel, ToolCall, ToolResultMessage, Usage,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::types::{AgentContext, AgentMessage, AgentToolResult, ToolError};

/// A tool call about to run.
pub struct BeforeToolCall<'a, M> {
    pub assistant_message: &'a AssistantMessage,
    pub tool_call: &'a ToolCall,
    /// The validated arguments.
    pub args: &'a Value,
    pub context: &'a AgentContext<M>,
}

/// What a gate decided about a tool call. The default lets it run unchanged.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BeforeToolCallResult {
    pub block: bool,
    /// Told to the model when the call is blocked.
    pub reason: Option<String>,
    /// With `block`, ask the agent to stop after this batch.
    pub terminate: bool,
    /// Run with these arguments instead. They are not validated again.
    pub args: Option<Value>,
}

impl BeforeToolCallResult {
    pub fn block(reason: impl Into<String>) -> Self {
        Self {
            block: true,
            reason: Some(reason.into()),
            ..Self::default()
        }
    }

    pub fn with_args(args: Value) -> Self {
        Self {
            args: Some(args),
            ..Self::default()
        }
    }
}

/// A tool call that has run.
pub struct AfterToolCall<'a, M> {
    pub assistant_message: &'a AssistantMessage,
    pub tool_call: &'a ToolCall,
    pub args: &'a Value,
    pub result: &'a AgentToolResult,
    pub is_error: bool,
    pub context: &'a AgentContext<M>,
}

/// Replacements for parts of a tool result; unset fields stay as they are.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AfterToolCallResult {
    pub content: Option<Vec<Content>>,
    pub details: Option<Value>,
    pub usage: Option<Usage>,
    pub is_error: Option<bool>,
    pub terminate: Option<bool>,
}

/// A completed turn: the assistant message and the results of its tool calls.
pub struct TurnContext<'a, M> {
    pub message: &'a AssistantMessage,
    pub tool_results: &'a [ToolResultMessage],
    /// The context after the turn's messages were appended.
    pub context: &'a AgentContext<M>,
    /// The messages this run has added so far.
    pub new_messages: &'a [M],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnDecision {
    /// Make sure one more request follows, even when nothing else would cause one.
    Continue,
    /// End the run now, without polling the queues.
    End,
}

/// The state the next provider request will use.
pub struct RequestContext<'a, M> {
    pub context: &'a AgentContext<M>,
    pub model: &'a Model,
    pub thinking_level: ThinkingLevel,
}

/// Replacements for this and later requests of the run.
pub struct RequestUpdate<M> {
    pub context: Option<AgentContext<M>>,
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
}

impl<M> Default for RequestUpdate<M> {
    fn default() -> Self {
        Self {
            context: None,
            model: None,
            thinking_level: None,
        }
    }
}

/// Replacements for the next turn, and messages to append before it.
pub struct TurnUpdate<M> {
    pub context: Option<AgentContext<M>>,
    pub messages: Vec<M>,
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
}

impl<M> Default for TurnUpdate<M> {
    fn default() -> Self {
        Self {
            context: None,
            messages: Vec::new(),
            model: None,
            thinking_level: None,
        }
    }
}

/// How an application shapes a run. Every method has a neutral default.
///
/// Hooks must not fail the run: return a fallback instead. Tool-call hooks may return
/// an error, which becomes an error result for that call, so a failing gate fails closed.
#[async_trait]
pub trait AgentHooks<M: AgentMessage>: Send + Sync {
    /// The messages the model sees. The default keeps the model-facing ones.
    async fn convert_to_llm(&self, messages: &[M]) -> Vec<Message> {
        messages
            .iter()
            .filter_map(|message| message.as_message().cloned())
            .collect()
    }

    /// Rewrite the transcript for one request, before conversion. `None` keeps it.
    async fn transform_context(
        &self,
        _messages: &[M],
        _cancel: &CancellationToken,
    ) -> Option<Vec<M>> {
        None
    }

    /// A credential for each request, for tokens that expire during long runs.
    async fn api_key(&self, _provider: &str) -> Option<String> {
        None
    }

    /// Gate a tool call: let it run, change its arguments or block it.
    async fn before_tool_call(
        &self,
        _call: BeforeToolCall<'_, M>,
        _cancel: &CancellationToken,
    ) -> Result<Option<BeforeToolCallResult>, ToolError> {
        Ok(None)
    }

    /// Rewrite a tool result before it is reported and appended.
    async fn after_tool_call(
        &self,
        _call: AfterToolCall<'_, M>,
        _cancel: &CancellationToken,
    ) -> Result<Option<AfterToolCallResult>, ToolError> {
        Ok(None)
    }

    /// Decide what follows a completed turn, before `TurnEnd`.
    async fn finish_turn(
        &self,
        _turn: TurnContext<'_, M>,
        _cancel: &CancellationToken,
    ) -> Option<TurnDecision> {
        None
    }

    /// The message to record at `MessageEnd`, for every message the run adds. A
    /// replacement goes into the transcript and later requests; keep its kind.
    async fn finalize_message(&self, message: M) -> M {
        message
    }

    /// Adjust the context, model or thinking level right before each provider request.
    async fn prepare_request(
        &self,
        _request: RequestContext<'_, M>,
        _cancel: &CancellationToken,
    ) -> Option<RequestUpdate<M>> {
        None
    }

    /// Prepare the next turn after `TurnEnd`, for example by compacting the context.
    async fn prepare_next_turn(
        &self,
        _turn: TurnContext<'_, M>,
        _cancel: &CancellationToken,
    ) -> Option<TurnUpdate<M>> {
        None
    }
}

/// The neutral hooks.
pub struct NoHooks;

impl<M: AgentMessage> AgentHooks<M> for NoHooks {}
