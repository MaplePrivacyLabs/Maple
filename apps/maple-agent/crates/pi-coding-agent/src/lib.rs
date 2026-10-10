//! Sessions, compaction, resources, extensions and the agent session.
//!
//! This crate follows the design of the core of Pi's coding agent:
//!
//! - [`session`]: the conversation is an append-only tree of entries, stored through a
//!   [`store::SessionStore`] (JSONL files here; any other store fits behind the trait);
//! - [`compaction`]: older context is summarized when it nears the window, and the
//!   branch being left can be summarized when moving in the tree;
//! - [`resources`]: context files, skills, prompt templates, `SYSTEM.md` and
//!   `APPEND_SYSTEM.md` from folders the host names;
//! - [`trust`]: whether a project's own resources may load, decided once per folder;
//! - [`system_prompt`]: the prompt as named sections the transcript can patch;
//! - [`extensions`]: a plugin API of typed events, tools, commands and providers;
//! - [`tools`]: Pi's built-in tools, `read`, `bash`, `edit`, `write`, `grep`, `find` and
//!   `ls`, and `powershell`;
//! - [`AgentSession`]: an agent over a session with all of the above, plus retry and the
//!   user's own shell commands ([`bash_executor`]).
//!
//! A session gets the built-in tools for its folder; hosts configure them, add their
//! own, and replace one by registering a tool with its name.

pub mod agent_session;
pub mod bash_executor;
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
pub mod tools;
pub mod trust;

pub use agent_session::{
    AgentSession, AgentSessionError, AgentSessionEvent, AgentSessionOptions, BashCommandOptions,
    CompactionReason, ContextUsage, Delivery, PromptOptions, PromptOutcome, SessionStats,
    StreamingBehavior, TokenTotals,
};
pub use bash_executor::BashResult;
pub use messages::SessionMessage;
pub use models::{ApiKeySource, ModelRegistry, StaticKeys};
