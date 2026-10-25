//! Modular Responses API implementation
//!
//! This module contains the refactored components of the Responses API,
//! separated by concern for better maintainability and testability.

pub mod builders;
pub mod constants;
pub mod context_builder;
pub mod conversation_projects;
pub mod conversations;
pub mod conversions;
pub mod errors;
pub mod events;
mod execution;
pub mod handlers;
pub(crate) mod image_describer;
pub mod instructions;
pub mod pagination;
pub(crate) mod prompt_fit;
pub mod storage;
pub mod tools;
pub mod types;

// Re-export commonly used types
pub use builders::{
    build_usage, ContentPartBuilder, ConversationBuilder, OutputItemBuilder, ResponseBuilder,
};
pub use context_builder::build_prompt_with_token_reserve;
pub(crate) use context_builder::prompt_token_budget;
pub use conversions::{ConversationItem, ConversationItemConverter, MessageContentConverter};
pub use errors::error_mapping;
pub use events::{ResponseEvent, SseEventEmitter};
pub(crate) use execution::{
    ResponseExecution, ResponseExecutionRegistry, ResponseExecutionTaskGuard,
};
pub use pagination::Paginator;
pub use storage::storage_task;
// REMOVED: UpstreamStreamProcessor - no longer used with centralized billing architecture
pub use types::{DeletedObjectResponse, MessageContent, MessageContentPart, NullableField};

// Re-export routers
pub use conversation_projects::router as conversation_projects_router;
pub use conversations::router as conversations_router;
pub use handlers::router as responses_router;
pub use instructions::router as instructions_router;
