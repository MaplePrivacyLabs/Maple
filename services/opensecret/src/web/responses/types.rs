//! Shared message and conversation content types
//!
//! These types are used by both the Responses API and Conversations API
//! to represent message content in various formats.

use serde::{Deserialize, Deserializer, Serialize};

// ============================================================================
// Message Content Types (Input)
// ============================================================================

/// Content part for input messages
///
/// Supports multiple content types following OpenAI's Conversations API:
/// - Text (legacy and standard input_text)
/// - Images (input_image with an inline base64 data URL)
/// - Files (input_file for PDFs and documents)
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type")]
pub enum MessageContentPart {
    /// Legacy: Support "text" for backwards compatibility with chat completions
    #[serde(rename = "text")]
    Text { text: String },

    /// OpenAI Conversations API standard: "input_text"
    #[serde(rename = "input_text")]
    InputText { text: String },

    /// OpenAI Conversations API "input_image", restricted to inline data URLs.
    /// Remote URLs and file IDs are intentionally unsupported.
    #[serde(rename = "input_image")]
    InputImage {
        #[serde(skip_serializing_if = "Option::is_none")]
        image_url: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        file_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>, // "low" | "high" | "auto"
    },

    /// OpenAI Conversations API standard: "input_file" for PDFs and documents
    #[serde(rename = "input_file")]
    InputFile {
        filename: String,
        file_data: String, // data:application/pdf;base64,... or other MIME types
    },
    // TODO: Add support for other content types per OpenAI Conversations API:
    // - input_audio: { input_audio: {...} }
}

/// Content that can be either a string or array of content parts
///
/// This is the primary type for representing user input messages.
/// It supports both simple text and rich multimodal content.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum MessageContent {
    /// Simple text content
    Text(String),

    /// Rich multimodal content with multiple parts
    Parts(Vec<MessageContentPart>),
}

/// Field wrapper for PATCH-like request bodies that need to distinguish
/// between omitted fields, explicit nulls, and concrete values.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum NullableField<T> {
    #[default]
    Missing,
    Null,
    Value(T),
}

impl<T> NullableField<T> {
    pub fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }
}

impl<'de, T> Deserialize<'de> for NullableField<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match Option::<T>::deserialize(deserializer)? {
            Some(value) => Self::Value(value),
            None => Self::Null,
        })
    }
}

// ============================================================================
// Conversation Content Types (Output)
// ============================================================================

/// Content within a message for API responses
///
/// Used when returning conversation items to clients.
/// Distinguishes between input types (from user) and output types (from assistant).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum ConversationContent {
    /// Legacy text content
    #[serde(rename = "text")]
    Text { text: String },

    /// Input text from user
    #[serde(rename = "input_text")]
    InputText { text: String },

    /// Output text from assistant
    #[serde(rename = "output_text")]
    OutputText { text: String },

    /// Input image from user
    #[serde(rename = "input_image")]
    InputImage { image_url: String },

    /// Input file from user
    #[serde(rename = "input_file")]
    InputFile { filename: String },
}

/// Convert MessageContent (input format) to ConversationContent (output format)
///
/// This conversion is used when returning user messages in conversation items.
impl From<MessageContent> for Vec<ConversationContent> {
    fn from(content: MessageContent) -> Self {
        match content {
            MessageContent::Text(text) => vec![ConversationContent::InputText { text }],
            MessageContent::Parts(parts) => parts
                .into_iter()
                .filter_map(|part| match part {
                    MessageContentPart::Text { text } | MessageContentPart::InputText { text } => {
                        Some(ConversationContent::InputText { text })
                    }
                    MessageContentPart::InputImage { image_url, .. } => {
                        image_url.map(|url| ConversationContent::InputImage { image_url: url })
                    }
                    MessageContentPart::InputFile { filename, .. } => {
                        Some(ConversationContent::InputFile { filename })
                    }
                })
                .collect(),
        }
    }
}

// ============================================================================
// Delete Response Types
// ============================================================================

/// Generic response for deleted objects
///
/// Used for both conversation and response deletion to provide a consistent
/// response format across all delete operations.
#[derive(Debug, Clone, Serialize)]
pub struct DeletedObjectResponse {
    /// ID of the deleted object
    pub id: uuid::Uuid,

    /// Object type (e.g., "conversation.deleted", "response.deleted")
    pub object: &'static str,

    /// Always true for successful deletions
    pub deleted: bool,
}

impl DeletedObjectResponse {
    /// Create a deleted conversation response
    ///
    /// # Arguments
    /// * `id` - UUID of the deleted conversation
    ///
    /// # Returns
    /// DeletedObjectResponse with object type "conversation.deleted"
    pub fn conversation(id: uuid::Uuid) -> Self {
        Self {
            id,
            object: crate::web::responses::constants::OBJECT_TYPE_CONVERSATION_DELETED,
            deleted: true,
        }
    }

    /// Create a deleted conversation project response
    pub fn conversation_project(id: uuid::Uuid) -> Self {
        Self {
            id,
            object: crate::web::responses::constants::OBJECT_TYPE_CONVERSATION_PROJECT_DELETED,
            deleted: true,
        }
    }

    /// Create a deleted response object response
    ///
    /// # Arguments
    /// * `id` - UUID of the deleted response
    ///
    /// # Returns
    /// DeletedObjectResponse with object type "response.deleted"
    pub fn response(id: uuid::Uuid) -> Self {
        Self {
            id,
            object: crate::web::responses::constants::OBJECT_TYPE_RESPONSE_DELETED,
            deleted: true,
        }
    }
}
