//! A stateful agent: a model, a transcript, tools and a loop that runs them.
//!
//! This crate follows the design of Pi's `pi-agent-core` package:
//!
//! - [`run_agent_loop`] streams a response, runs its tool calls (in parallel or in
//!   order), appends the results and asks again until the model stops, with steering
//!   messages delivered between turns and follow-ups once the agent would stop;
//! - every step is an [`AgentEvent`]; failures are messages, not errors;
//! - applications plug in through [`AgentHooks`]: message conversion, context
//!   transforms, per-request credentials, tool-call gates and result rewrites, and turn
//!   and request preparation;
//! - the transcript type is the application's: anything implementing [`AgentMessage`]
//!   can carry its own messages beside the model-facing ones;
//! - [`Agent`] owns the transcript and queues and exposes prompt, steer, follow-up,
//!   continue and abort.

mod agent;
mod agent_loop;
mod hooks;
mod types;

pub use agent::{Agent, AgentListener, AgentOptions, ListenerId};
pub use agent_loop::{
    AgentEventSink, AgentLoopConfig, MessageQueues, NoQueues, RunToolCall, ToolCallOutcome,
    run_agent_loop, run_agent_loop_continue, run_tool_call,
};
pub use hooks::{
    AfterToolCall, AfterToolCallResult, AgentHooks, BeforeToolCall, BeforeToolCallResult, NoHooks,
    RequestContext, RequestUpdate, TurnContext, TurnDecision, TurnUpdate,
};
pub use types::{
    AgentContext, AgentError, AgentEvent, AgentMessage, AgentTool, AgentToolResult, FnTool,
    QueueMode, ToolError, ToolExecutionMode, ToolInvocation, ToolUpdates,
};
