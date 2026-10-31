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
//!   validation, context-overflow and retry classification, strict tool schemas
//!   ([`constrained`]) and token estimates.
//!
//! It has no dependency on any application. Hosts register providers in an
//! [`ApiRegistry`] keyed by [`Model::api`], and classifiers ([`classifier`]), which answer
//! questions about a state with probabilities, in a [`ClassifierRegistry`].

pub mod classifier;
pub mod constrained;
mod estimate;
pub mod faux;
mod json;
pub mod openai;
mod overflow;
mod provider;
mod provider_retry;
mod stream;
pub mod system_one;
pub mod transcript;
mod types;
mod validation;

pub use classifier::{
    BoolCriteria, Calibration, ClassifierAnswer, ClassifierContext, ClassifierModel,
    ClassifierOptions, ClassifierQuestion, ClassifierRegistry, ClassifierResult,
    ClassifierStopReason, ClassifyFn,
};
pub use estimate::{estimate_message_tokens, estimate_text_tokens, estimate_tool_tokens};
pub use json::{parse_streaming_json, parse_tool_call_arguments};
pub use overflow::{
    is_context_overflow, is_recoverable_length, is_retryable_error, retry_delay_ms,
};
pub use provider::{
    ApiRegistry, HeadersHook, PayloadHook, ProviderEventHook, ProviderResponse, ResponseHook,
    StreamFn, StreamOptions, error_stream,
};
pub use stream::{
    AssistantMessageBuilder, AssistantMessageEvent, AssistantMessageStream, AssistantStreamSender,
    apply_event,
};
pub use types::*;
pub use validation::validate_tool_arguments;
