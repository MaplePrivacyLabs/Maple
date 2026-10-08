//! Provider-neutral model messages, streaming events and providers.
//!
//! This crate follows the design of Pi's `pi-ai` package:
//!
//! - one message model for every provider ([`Message`]);
//! - system messages carry the prompt and the tool declarations, so replaying the
//!   transcript yields the current prompt and tools ([`transcript`]);
//! - a provider turns a [`Context`] into an [`AssistantMessageStream`] of
//!   [`AssistantMessageEvent`]s that always ends in `Done` or `Error`, so request
//!   failures are data, not errors ([`StreamFn`]);
//! - helpers the agent layers share: streaming JSON repair, tool-argument
//!   validation, context-overflow and retry classification, and token estimates.
//!
//! It has no dependency on any application. Hosts register providers in an
//! [`ApiRegistry`] keyed by [`Model::api`].

mod estimate;
pub mod faux;
mod json;
pub mod openai;
mod overflow;
mod provider;
mod stream;
pub mod transcript;
mod types;
mod validation;

pub use estimate::{estimate_message_tokens, estimate_text_tokens, estimate_tool_tokens};
pub use json::parse_streaming_json;
pub use overflow::{is_context_overflow, is_retryable_error, retry_delay_ms};
pub use provider::{ApiRegistry, PayloadHook, StreamFn, StreamOptions, error_stream};
pub use stream::{
    AssistantMessageBuilder, AssistantMessageEvent, AssistantMessageStream, AssistantStreamSender,
    apply_event,
};
pub use types::*;
pub use validation::validate_tool_arguments;
