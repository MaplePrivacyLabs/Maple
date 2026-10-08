//! Sessions, compaction, resources, extensions and the agent session.
//!
//! This crate follows the design of the core of Pi's coding agent:
//!
//! - [`session`]: the conversation is an append-only tree of entries, stored through a
//!   [`store::SessionStore`] (JSONL files here; any other store fits behind the trait);
//! - [`compaction`]: older context is summarized when it nears the window, and the
//!   branch being left can be summarized when moving in the tree;
//! - [`resources`]: context files, skills and prompt templates from folders the host names;
//! - [`system_prompt`]: the prompt as named sections the transcript can patch;
//! - [`extensions`]: a plugin API of typed events, tools, commands and providers;
//! - [`AgentSession`]: an agent over a session with all of the above, plus retry.
//!
//! It ships no tools of its own: hosts register theirs.

pub mod agent_session;
pub mod compaction;
pub mod extensions;
mod ids;
pub mod messages;
pub mod models;
pub mod resources;
pub mod session;
pub mod settings;
pub mod store;
pub mod system_prompt;

pub use agent_session::{
    AgentSession, AgentSessionError, AgentSessionEvent, AgentSessionOptions, CompactionReason,
    ContextUsage, Delivery, PromptOptions, PromptOutcome, StreamingBehavior,
};
pub use messages::SessionMessage;
pub use models::{ApiKeySource, ModelRegistry, StaticKeys};
