//! Constants used throughout the Responses API

/// Channel buffer sizes
pub const STORAGE_CHANNEL_BUFFER: usize = 1024;
pub const CLIENT_CHANNEL_BUFFER: usize = 1024;

/// Response object types
pub const OBJECT_TYPE_RESPONSE: &str = "response";
pub const OBJECT_TYPE_RESPONSE_DELETED: &str = "response.deleted";
pub const OBJECT_TYPE_CONVERSATION: &str = "conversation";
pub const OBJECT_TYPE_CONVERSATION_DELETED: &str = "conversation.deleted";
pub const OBJECT_TYPE_CONVERSATION_PROJECT: &str = "conversation.project";
pub const OBJECT_TYPE_CONVERSATION_PROJECT_DELETED: &str = "conversation.project.deleted";
pub const OBJECT_TYPE_LIST: &str = "list";
pub const OBJECT_TYPE_LIST_DELETED: &str = "list.deleted";

/// Event types for SSE streaming
pub const EVENT_RESPONSE_CREATED: &str = "response.created";
pub const EVENT_RESPONSE_IN_PROGRESS: &str = "response.in_progress";
pub const EVENT_RESPONSE_OUTPUT_ITEM_ADDED: &str = "response.output_item.added";
pub const EVENT_RESPONSE_CONTENT_PART_ADDED: &str = "response.content_part.added";
pub const EVENT_RESPONSE_OUTPUT_TEXT_DELTA: &str = "response.output_text.delta";
pub const EVENT_RESPONSE_OUTPUT_TEXT_DONE: &str = "response.output_text.done";
pub const EVENT_RESPONSE_REASONING_TEXT_DELTA: &str = "response.reasoning_text.delta";
pub const EVENT_RESPONSE_REASONING_TEXT_DONE: &str = "response.reasoning_text.done";
pub const EVENT_RESPONSE_CONTENT_PART_DONE: &str = "response.content_part.done";
pub const EVENT_RESPONSE_OUTPUT_ITEM_DONE: &str = "response.output_item.done";
pub const EVENT_RESPONSE_COMPLETED: &str = "response.completed";
pub const EVENT_RESPONSE_CANCELLED: &str = "response.cancelled";
pub const EVENT_RESPONSE_FAILED: &str = "response.failed";

/// Error event data
pub const ERROR_DATA_ENCRYPTION_FAILED: &str = "encryption_failed";
pub const ERROR_DATA_SERIALIZATION_FAILED: &str = "serialization_failed";

/// Message statuses
pub const STATUS_IN_PROGRESS: &str = "in_progress";
pub const STATUS_COMPLETED: &str = "completed";
pub const STATUS_FAILED: &str = "failed";
pub const STATUS_INCOMPLETE: &str = "incomplete";
pub const STATUS_CANCELLED: &str = "cancelled";

/// Finish reasons
pub const FINISH_REASON_CANCELLED: &str = "cancelled";

/// Text format types
pub const TEXT_FORMAT_TYPE: &str = "text";

/// Truncation strategies
pub const TRUNCATION_DISABLED: &str = "disabled";

/// Tool choice strategies
pub const TOOL_CHOICE_AUTO: &str = "auto";

/// Output item types
pub const OUTPUT_TYPE_MESSAGE: &str = "message";

/// Content part types
pub const CONTENT_PART_TYPE_OUTPUT_TEXT: &str = "output_text";

/// Message roles
pub const ROLE_USER: &str = "user";
pub const ROLE_ASSISTANT: &str = "assistant";
pub const ROLE_SYSTEM: &str = "system";

/// Default pagination values
pub const DEFAULT_PAGINATION_LIMIT: i64 = 20;
pub const MAX_PAGINATION_LIMIT: i64 = 100;
pub const DEFAULT_PAGINATION_ORDER: &str = "desc";

/// Tool call defaults
pub const DEFAULT_TOOL_FUNCTION_NAME: &str = "function";

/// Tool-related event types
pub const EVENT_TOOL_CALL_CREATED: &str = "tool_call.created";
pub const EVENT_TOOL_OUTPUT_CREATED: &str = "tool_output.created";
