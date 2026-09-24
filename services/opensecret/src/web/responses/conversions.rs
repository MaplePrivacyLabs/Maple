//! Message content conversion utilities

use crate::{
    encrypt::decrypt_string,
    models::responses::RawThreadMessage,
    tokens::count_tokens,
    web::responses::{constants::*, error_mapping, types::*},
    ApiError,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use secp256k1::SecretKey;
use serde_json::{json, Value};
use tracing::error;
use uuid::Uuid;

/// Centralized message content conversion utilities
///
/// This struct provides a single source of truth for converting between
/// different message content formats used throughout the API:
/// - User input normalization
/// - OpenAI API format conversion
/// - Conversation API format conversion
/// - Token counting text extraction
pub struct MessageContentConverter;

/// Safe stand-in for legacy image-only messages after raw images are removed
/// from model context. It deliberately contains no URL, file ID, MIME type, or
/// other attachment metadata.
pub(crate) const MODEL_IMAGE_OMITTED_PLACEHOLDER: &str =
    "[Image 1 attachment omitted from model input.]";

pub(crate) fn model_image_omitted_placeholder(image_number: usize) -> String {
    format!("[Image {image_number} attachment omitted from model input.]")
}

pub(crate) const MAX_INPUT_IMAGES: usize = 10;
const MAX_INPUT_IMAGE_BYTES: usize = 20 * 1024 * 1024;

impl MessageContentConverter {
    pub(crate) fn image_count(content: &MessageContent) -> usize {
        match content {
            MessageContent::Text(_) => 0,
            MessageContent::Parts(parts) => parts
                .iter()
                .filter(|part| matches!(part, MessageContentPart::InputImage { .. }))
                .count(),
        }
    }

    /// Validate MessageContent parts to ensure unsupported features are rejected
    ///
    /// Currently validates:
    /// - file_id is not supported in InputImage (only image_url)
    /// - at most 10 images are present
    /// - image_url is a supported base64 data URL that decodes to at most 20 MiB
    ///
    /// # Arguments
    /// * `content` - The content to validate
    ///
    /// # Returns
    /// Ok(()) if valid, Err(ApiError) if validation fails
    pub fn validate_content(content: &MessageContent) -> Result<(), ApiError> {
        if let MessageContent::Parts(parts) = content {
            let image_count = Self::image_count(content);
            if image_count > MAX_INPUT_IMAGES {
                return Err(ApiError::PayloadTooLarge);
            }

            for part in parts {
                if let MessageContentPart::InputImage {
                    file_id, image_url, ..
                } = part
                {
                    if file_id.is_some() {
                        return Err(ApiError::BadRequest);
                    }

                    Self::validate_image_data_url(
                        image_url.as_deref().ok_or(ApiError::BadRequest)?,
                    )?;
                }
            }
        }
        Ok(())
    }

    fn validate_image_data_url(image_url: &str) -> Result<(), ApiError> {
        let Some(data_url) = image_url
            .get(..5)
            .filter(|prefix| prefix.eq_ignore_ascii_case("data:"))
        else {
            return Err(ApiError::BadRequest);
        };

        debug_assert_eq!(data_url.len(), 5);
        Self::validate_base64_data_url(&image_url[5..], MAX_INPUT_IMAGE_BYTES)
    }

    fn validate_base64_data_url(data_url: &str, max_decoded_bytes: usize) -> Result<(), ApiError> {
        let (metadata, payload) = data_url.split_once(',').ok_or(ApiError::BadRequest)?;
        let mut metadata_parts = metadata.split(';');
        let media_type = metadata_parts.next().unwrap_or_default();
        let is_supported_image = [
            "image/png",
            "image/jpeg",
            "image/jpg",
            "image/webp",
            "image/gif",
            "image/avif",
        ]
        .iter()
        .any(|supported| media_type.eq_ignore_ascii_case(supported));
        let is_exact_base64_form = metadata_parts
            .next()
            .is_some_and(|component| component.eq_ignore_ascii_case("base64"))
            && metadata_parts.next().is_none();

        if !is_supported_image || !is_exact_base64_form || payload.is_empty() {
            return Err(ApiError::BadRequest);
        }

        let decoded = STANDARD.decode(payload).map_err(|_| ApiError::BadRequest)?;
        if decoded.len() > max_decoded_bytes {
            return Err(ApiError::PayloadTooLarge);
        }

        Ok(())
    }

    /// Normalize MessageContent to always use Parts format
    ///
    /// Converts simple Text format to Parts with InputText, ensuring
    /// consistent internal representation.
    ///
    /// # Arguments
    /// * `content` - The content to normalize
    ///
    /// # Returns
    /// MessageContent in Parts format
    pub fn normalize_content(content: MessageContent) -> MessageContent {
        match content {
            MessageContent::Text(text) => {
                MessageContent::Parts(vec![MessageContentPart::InputText { text }])
            }
            MessageContent::Parts(parts) => MessageContent::Parts(parts),
        }
    }

    /// Convert MessageContent to the format sent to the main conversation model.
    ///
    /// Raw image parts are replaced by numbered, non-sensitive text markers while
    /// all remaining parts keep their original order. The markers correlate later
    /// `read_image` results without exposing URLs or bytes, and keep legacy
    /// image-only messages valid even when no description exists.
    pub fn to_model_format(content: &MessageContent) -> Value {
        match content {
            MessageContent::Text(text) => json!(text),
            MessageContent::Parts(parts) => {
                let mut model_parts = Vec::with_capacity(parts.len());
                let mut image_number = 0usize;

                for part in parts {
                    if matches!(part, MessageContentPart::InputImage { .. }) {
                        image_number += 1;
                        model_parts.push(json!({
                            "type": "text",
                            "text": model_image_omitted_placeholder(image_number)
                        }));
                    } else {
                        model_parts.push(Self::content_part_to_openai(part));
                    }
                }

                json!(model_parts)
            }
        }
    }

    /// Convert a single MessageContentPart to OpenAI format
    fn content_part_to_openai(part: &MessageContentPart) -> Value {
        match part {
            MessageContentPart::Text { text } | MessageContentPart::InputText { text } => {
                json!({
                    "type": "text",
                    "text": text
                })
            }
            MessageContentPart::InputImage {
                image_url, detail, ..
            } => {
                let mut image_obj = json!({
                    "url": image_url.as_ref().unwrap_or(&"".to_string())
                });
                if let Some(d) = detail {
                    image_obj["detail"] = json!(d);
                }
                json!({
                    "type": "image_url",
                    "image_url": image_obj
                })
            }
            MessageContentPart::InputFile {
                filename,
                file_data,
            } => {
                json!({
                    "type": "file",
                    "file": {
                        "filename": filename,
                        "file_data": file_data
                    }
                })
            }
        }
    }

    /// Convert assistant text to conversation content
    ///
    /// Helper specifically for assistant messages which are stored as plain text
    /// rather than structured MessageContent.
    ///
    /// # Arguments
    /// * `text` - The assistant's text response
    ///
    /// # Returns
    /// Vector containing a single OutputText content part
    pub fn assistant_text_to_content(text: String) -> Vec<ConversationContent> {
        vec![ConversationContent::OutputText { text }]
    }

    /// Extract text content for token counting purposes only
    ///
    /// Concatenates all text parts (ignoring images/files) to produce a string
    /// suitable for token counting. This does NOT represent the full content
    /// structure - use only for token estimation.
    ///
    /// # Arguments
    /// * `content` - The content to extract text from
    ///
    /// # Returns
    /// Concatenated text from all text content parts
    pub fn extract_text_for_token_counting(content: &MessageContent) -> String {
        match content {
            MessageContent::Text(text) => text.clone(),
            MessageContent::Parts(parts) => parts
                .iter()
                .filter_map(|part| match part {
                    MessageContentPart::Text { text } => Some(text.clone()),
                    MessageContentPart::InputText { text } => Some(text.clone()),
                    MessageContentPart::InputImage { .. } => None, // Ignore images
                    MessageContentPart::InputFile { .. } => None,  // Ignore files
                })
                .collect::<Vec<_>>()
                .join(" "),
        }
    }

    /// Estimate the prompt tokens sent to the main conversation model.
    ///
    /// Raw image bytes are excluded. Numbered marker text is counted because
    /// [`Self::to_model_format`] sends those safe markers to preserve placement.
    pub fn estimate_prompt_tokens(content: &MessageContent) -> usize {
        match content {
            MessageContent::Text(text) => count_tokens(text),
            MessageContent::Parts(parts) => {
                let mut image_number = 0usize;
                parts.iter().fold(0usize, |total, part| {
                    if matches!(part, MessageContentPart::InputImage { .. }) {
                        image_number += 1;
                        total.saturating_add(count_tokens(&model_image_omitted_placeholder(
                            image_number,
                        )))
                    } else {
                        total.saturating_add(Self::estimate_content_part_tokens(part))
                    }
                })
            }
        }
    }

    fn estimate_content_part_tokens(part: &MessageContentPart) -> usize {
        match part {
            MessageContentPart::Text { text } | MessageContentPart::InputText { text } => {
                count_tokens(text)
            }
            MessageContentPart::InputImage { .. } => 0,
            MessageContentPart::InputFile { filename, .. } => count_tokens(filename),
        }
    }
}

// ============================================================================
// Conversation Item Converter
// ============================================================================

/// Conversation item used in the Conversations API
///
/// This is defined here to avoid circular dependencies since it's only used
/// for conversion logic.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type")]
pub enum ConversationItem {
    #[serde(rename = "message")]
    Message {
        id: Uuid,
        /// Response that created this item, if it was created through the Responses API.
        #[serde(skip_serializing_if = "Option::is_none")]
        response_id: Option<Uuid>,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        role: String,
        content: Vec<ConversationContent>,
        #[serde(skip_serializing_if = "Option::is_none")]
        created_at: Option<i64>,
    },
    #[serde(rename = "function_call")]
    FunctionToolCall {
        id: Uuid,
        /// Response that created this item, if it was created through the Responses API.
        #[serde(skip_serializing_if = "Option::is_none")]
        response_id: Option<Uuid>,
        call_id: Uuid,
        name: String,
        arguments: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        created_at: Option<i64>,
    },
    #[serde(rename = "function_call_output")]
    FunctionToolCallOutput {
        id: Uuid,
        /// Response that created this item, if it was created through the Responses API.
        #[serde(skip_serializing_if = "Option::is_none")]
        response_id: Option<Uuid>,
        call_id: Uuid,
        output: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        created_at: Option<i64>,
    },
    /// Reasoning chain-of-thought from thinking models
    #[serde(rename = "reasoning")]
    Reasoning {
        id: Uuid,
        /// Response that created this item, if it was created through the Responses API.
        #[serde(skip_serializing_if = "Option::is_none")]
        response_id: Option<Uuid>,
        /// Reasoning text content
        content: Vec<ReasoningContentItem>,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        created_at: Option<i64>,
    },
}

/// Reasoning content item (text only for now)
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type")]
pub enum ReasoningContentItem {
    #[serde(rename = "text")]
    Text { text: String },
}

/// Centralized conversation item conversion utilities
///
/// Converts database Message models to ConversationItem API types,
/// handling decryption and format conversion for all message types.
pub struct ConversationItemConverter;

impl ConversationItemConverter {
    /// Convert database message to ConversationItem
    ///
    /// Handles decryption and format conversion for all message types.
    ///
    /// # Arguments
    /// * `msg` - The database message to convert
    /// * `user_key` - User's encryption key for decrypting content
    ///
    /// # Returns
    /// ConversationItem ready for API response
    ///
    /// # Errors
    /// Returns ApiError if decryption or deserialization fails
    pub fn message_to_item(
        msg: &RawThreadMessage,
        user_key: &SecretKey,
    ) -> Result<ConversationItem, ApiError> {
        // Decrypt content (handle nullable content_enc)
        let content = decrypt_string(user_key, msg.content_enc.as_ref())
            .map_err(|_| error_mapping::map_decryption_error("message content"))?
            .unwrap_or_default();

        match msg.message_type.as_str() {
            "user" => Self::user_message_to_item(msg, content),
            "assistant" => Self::assistant_message_to_item(msg, content),
            "tool_call" => Self::tool_call_to_item(msg, content),
            "tool_output" => Self::tool_output_to_item(msg, content),
            "reasoning" => Self::reasoning_to_item(msg, content),
            unknown => {
                error!("Unknown message type: {}", unknown);
                Err(ApiError::InternalServerError)
            }
        }
    }

    /// Convert user message to item
    fn user_message_to_item(
        msg: &RawThreadMessage,
        content: String,
    ) -> Result<ConversationItem, ApiError> {
        // User messages MUST be stored as MessageContent
        let message_content: MessageContent = serde_json::from_str(&content).map_err(|e| {
            error!(
                "Failed to deserialize message content: {}",
                crate::log_redaction::JsonErrorSummary(&e)
            );
            ApiError::InternalServerError
        })?;

        Ok(ConversationItem::Message {
            id: msg.uuid,
            response_id: msg.response_uuid,
            status: msg.status.clone(),
            role: ROLE_USER.to_string(),
            content: Vec::<ConversationContent>::from(message_content),
            created_at: Some(msg.created_at.timestamp()),
        })
    }

    /// Convert assistant message to item
    fn assistant_message_to_item(
        msg: &RawThreadMessage,
        content: String,
    ) -> Result<ConversationItem, ApiError> {
        // Assistant messages are plain text strings
        // If content is empty (in_progress), return empty content array
        let content_parts = if content.is_empty() {
            vec![]
        } else {
            MessageContentConverter::assistant_text_to_content(content)
        };

        Ok(ConversationItem::Message {
            id: msg.uuid,
            response_id: msg.response_uuid,
            status: msg.status.clone(),
            role: ROLE_ASSISTANT.to_string(),
            content: content_parts,
            created_at: Some(msg.created_at.timestamp()),
        })
    }

    /// Convert tool call to item
    fn tool_call_to_item(
        msg: &RawThreadMessage,
        content: String,
    ) -> Result<ConversationItem, ApiError> {
        Ok(ConversationItem::FunctionToolCall {
            id: msg.uuid,
            response_id: msg.response_uuid,
            call_id: msg.tool_call_id.ok_or_else(|| {
                error!("tool_call_id missing for tool call");
                ApiError::InternalServerError
            })?,
            name: msg.tool_name.clone().unwrap_or_else(|| {
                error!("tool_name missing for tool call, using default");
                DEFAULT_TOOL_FUNCTION_NAME.to_string()
            }),
            arguments: content,
            status: msg.status.clone(),
            created_at: Some(msg.created_at.timestamp()),
        })
    }

    /// Convert tool output to item
    fn tool_output_to_item(
        msg: &RawThreadMessage,
        content: String,
    ) -> Result<ConversationItem, ApiError> {
        Ok(ConversationItem::FunctionToolCallOutput {
            id: msg.uuid,
            response_id: msg.response_uuid,
            call_id: msg.tool_call_id.ok_or_else(|| {
                error!("tool_call_id missing for tool output");
                ApiError::InternalServerError
            })?,
            output: content,
            status: msg.status.clone(),
            created_at: Some(msg.created_at.timestamp()),
        })
    }

    /// Convert reasoning item to ConversationItem
    fn reasoning_to_item(
        msg: &RawThreadMessage,
        content: String,
    ) -> Result<ConversationItem, ApiError> {
        // Reasoning content is stored as plain text, wrap in content array
        let content_items = if content.is_empty() {
            vec![]
        } else {
            vec![ReasoningContentItem::Text { text: content }]
        };

        Ok(ConversationItem::Reasoning {
            id: msg.uuid,
            response_id: msg.response_uuid,
            content: content_items,
            status: msg.status.clone(),
            created_at: Some(msg.created_at.timestamp()),
        })
    }

    /// Convert multiple messages to items with pagination
    ///
    /// Processes messages starting from an offset and applies limit.
    ///
    /// # Arguments
    /// * `raw_messages` - All messages from the conversation
    /// * `user_key` - User's encryption key
    /// * `start_index` - Index to start from (for cursor-based pagination)
    /// * `limit` - Maximum number of items to return
    ///
    /// # Returns
    /// Vector of ConversationItems
    ///
    /// # Errors
    /// Returns ApiError if any message conversion fails
    pub fn messages_to_items(
        raw_messages: &[RawThreadMessage],
        user_key: &SecretKey,
        start_index: usize,
        limit: usize,
    ) -> Result<Vec<ConversationItem>, ApiError> {
        let mut items = Vec::new();

        for msg in raw_messages.iter().skip(start_index).take(limit) {
            items.push(Self::message_to_item(msg, user_key)?);
        }

        Ok(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encrypt::encrypt_with_key;

    async fn serialized_conversation_items(response_uuid: Option<Uuid>) -> Vec<Value> {
        let user_key = SecretKey::from_slice(&[0x42; 32]).expect("valid test key");
        let tool_call_id = Uuid::new_v4();
        let cases = [
            (
                "user",
                serde_json::to_string(&MessageContent::Text("hello".to_string()))
                    .expect("serialize user content"),
                None,
                None,
            ),
            ("assistant", "hello back".to_string(), None, None),
            (
                "tool_call",
                r#"{"query":"hello"}"#.to_string(),
                Some(tool_call_id),
                Some("search".to_string()),
            ),
            (
                "tool_output",
                "result".to_string(),
                Some(tool_call_id),
                Some("search".to_string()),
            ),
            ("reasoning", "thinking".to_string(), None, None),
        ];

        let mut values = Vec::with_capacity(cases.len());
        for (message_type, content, item_tool_call_id, tool_name) in cases {
            let raw_message = RawThreadMessage {
                message_type: message_type.to_string(),
                id: 1,
                uuid: Uuid::new_v4(),
                response_uuid,
                content_enc: Some(encrypt_with_key(&user_key, content.as_bytes()).await),
                status: Some("completed".to_string()),
                created_at: chrono::Utc::now(),
                model: None,
                token_count: Some(1),
                tool_call_id: item_tool_call_id,
                finish_reason: None,
                tool_name,
            };
            let item = ConversationItemConverter::message_to_item(&raw_message, &user_key)
                .expect("convert conversation item");
            values.push(serde_json::to_value(item).expect("serialize conversation item"));
        }

        values
    }

    fn input_image(image_url: Option<&str>) -> MessageContentPart {
        MessageContentPart::InputImage {
            image_url: image_url.map(str::to_string),
            file_id: None,
            detail: None,
        }
    }

    #[tokio::test]
    async fn response_id_is_serialized_for_every_response_backed_conversation_item() {
        let response_uuid = Uuid::new_v4();

        for item in serialized_conversation_items(Some(response_uuid)).await {
            assert_eq!(item.get("response_id"), Some(&json!(response_uuid)));
        }
    }

    #[tokio::test]
    async fn response_id_is_omitted_for_standalone_conversation_items() {
        for item in serialized_conversation_items(None).await {
            assert!(!item
                .as_object()
                .expect("conversation item object")
                .contains_key("response_id"));
        }
    }

    #[test]
    fn test_validate_content_accepts_ten_inline_images() {
        let content = MessageContent::Parts(
            (0..MAX_INPUT_IMAGES)
                .map(|_| input_image(Some("data:image/png;base64,aGVsbG8=")))
                .collect(),
        );

        assert!(MessageContentConverter::validate_content(&content).is_ok());
    }

    #[test]
    fn test_validate_content_rejects_more_than_ten_images() {
        let content = MessageContent::Parts(
            (0..=MAX_INPUT_IMAGES)
                .map(|_| input_image(Some("data:image/png;base64,aGVsbG8=")))
                .collect(),
        );

        assert!(matches!(
            MessageContentConverter::validate_content(&content),
            Err(ApiError::PayloadTooLarge)
        ));
    }

    #[test]
    fn test_validate_content_rejects_remote_and_non_data_image_urls() {
        for image_url in [
            "custom-image-source",
            "http://example.com/image.png",
            "https://example.com/image.png",
            "https://localhost/image.png",
            "https://127.0.0.1/image.png",
            "https://169.254.169.254/latest/meta-data/",
            "https://user:password@example.com/image.png",
        ] {
            let content = MessageContent::Parts(vec![input_image(Some(image_url))]);

            assert!(matches!(
                MessageContentConverter::validate_content(&content),
                Err(ApiError::BadRequest)
            ));
        }
    }

    #[test]
    fn test_validate_content_rejects_missing_or_empty_image_url() {
        for image_url in [None, Some("")] {
            let content = MessageContent::Parts(vec![input_image(image_url)]);

            assert!(matches!(
                MessageContentConverter::validate_content(&content),
                Err(ApiError::BadRequest)
            ));
        }
    }

    #[test]
    fn test_validate_content_accepts_supported_base64_image_data_urls() {
        for media_type in [
            "image/png",
            "image/jpeg",
            "image/jpg",
            "image/webp",
            "image/gif",
            "image/avif",
        ] {
            let content = MessageContent::Parts(vec![input_image(Some(&format!(
                "data:{media_type};base64,aGVsbG8="
            )))]);

            assert!(MessageContentConverter::validate_content(&content).is_ok());
        }
    }

    #[test]
    fn test_validate_content_rejects_malformed_base64_data_urls() {
        for image_url in [
            "data:image/png;base64",
            "data:image/png,not-base64",
            "data:image/png;base64,",
            "data:image/png;base64,%%%%",
            "data:text/plain;base64,aGVsbG8=",
            "data:image/svg+xml;base64,aGVsbG8=",
            "data:image/png;charset=utf-8;base64,aGVsbG8=",
            "data:image/png;#%zz;base64,aGVsbG8=",
            "data:image/png;\n;base64,aGVsbG8=",
        ] {
            let content = MessageContent::Parts(vec![input_image(Some(image_url))]);

            assert!(matches!(
                MessageContentConverter::validate_content(&content),
                Err(ApiError::BadRequest)
            ));
        }
    }

    #[test]
    fn test_validate_base64_data_url_enforces_decoded_byte_limit() {
        assert_eq!(MAX_INPUT_IMAGE_BYTES, 20 * 1024 * 1024);

        let at_limit = STANDARD.encode([0_u8; 8]);
        assert!(MessageContentConverter::validate_base64_data_url(
            &format!("image/png;base64,{at_limit}"),
            8
        )
        .is_ok());

        let over_limit = STANDARD.encode([0_u8; 9]);
        assert!(matches!(
            MessageContentConverter::validate_base64_data_url(
                &format!("image/png;base64,{over_limit}"),
                8
            ),
            Err(ApiError::PayloadTooLarge)
        ));
    }

    #[test]
    fn test_normalize_text_to_parts() {
        let content = MessageContent::Text("hello".to_string());
        let normalized = MessageContentConverter::normalize_content(content);

        match normalized {
            MessageContent::Parts(parts) => {
                assert_eq!(parts.len(), 1);
                match &parts[0] {
                    MessageContentPart::InputText { text } => {
                        assert_eq!(text, "hello");
                    }
                    _ => panic!("Expected InputText"),
                }
            }
            _ => panic!("Expected Parts"),
        }
    }

    #[test]
    fn test_normalize_parts_unchanged() {
        let content = MessageContent::Parts(vec![MessageContentPart::InputText {
            text: "hello".to_string(),
        }]);
        let normalized = MessageContentConverter::normalize_content(content);

        match normalized {
            MessageContent::Parts(parts) => {
                assert_eq!(parts.len(), 1);
            }
            _ => panic!("Expected Parts"),
        }
    }

    #[test]
    fn test_extract_text_for_token_counting() {
        let content = MessageContent::Parts(vec![
            MessageContentPart::InputText {
                text: "hello".to_string(),
            },
            MessageContentPart::InputText {
                text: "world".to_string(),
            },
        ]);

        let text = MessageContentConverter::extract_text_for_token_counting(&content);
        assert_eq!(text, "hello world");
    }

    #[test]
    fn test_extract_text_ignores_images() {
        let content = MessageContent::Parts(vec![
            MessageContentPart::InputText {
                text: "hello".to_string(),
            },
            MessageContentPart::InputImage {
                image_url: Some("http://example.com/image.jpg".to_string()),
                file_id: None,
                detail: None,
            },
            MessageContentPart::InputText {
                text: "world".to_string(),
            },
        ]);

        let text = MessageContentConverter::extract_text_for_token_counting(&content);
        assert_eq!(text, "hello world");
    }

    #[test]
    fn test_estimate_prompt_tokens_counts_safe_markers_not_image_bytes() {
        let content = MessageContent::Parts(vec![
            MessageContentPart::InputText {
                text: "hello world".to_string(),
            },
            MessageContentPart::InputImage {
                image_url: Some("data:image/png;base64,abcd".to_string()),
                file_id: None,
                detail: None,
            },
            MessageContentPart::InputImage {
                image_url: Some("data:image/png;base64,efgh".to_string()),
                file_id: None,
                detail: None,
            },
        ]);

        let tokens = MessageContentConverter::estimate_prompt_tokens(&content);
        assert_eq!(
            tokens,
            count_tokens("hello world")
                + count_tokens(&model_image_omitted_placeholder(1))
                + count_tokens(&model_image_omitted_placeholder(2))
        );
    }

    #[test]
    fn test_to_model_format_omits_images_and_preserves_text_order() {
        let content = MessageContent::Parts(vec![
            MessageContentPart::InputText {
                text: "before".to_string(),
            },
            MessageContentPart::InputImage {
                image_url: Some("data:image/png;base64,sensitive-image-bytes".to_string()),
                file_id: None,
                detail: Some("high".to_string()),
            },
            MessageContentPart::Text {
                text: "after".to_string(),
            },
        ]);

        let model_content = MessageContentConverter::to_model_format(&content);
        let parts = model_content.as_array().expect("model content array");

        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], json!({ "type": "text", "text": "before" }));
        assert_eq!(
            parts[1],
            json!({ "type": "text", "text": MODEL_IMAGE_OMITTED_PLACEHOLDER })
        );
        assert_eq!(parts[2], json!({ "type": "text", "text": "after" }));
        assert!(!model_content.to_string().contains("sensitive-image-bytes"));
        assert!(!model_content.to_string().contains("image_url"));
    }

    #[test]
    fn test_to_model_format_keeps_legacy_image_only_message_valid() {
        let content = MessageContent::Parts(vec![MessageContentPart::InputImage {
            image_url: Some("data:image/png;base64,sensitive-image-bytes".to_string()),
            file_id: None,
            detail: None,
        }]);

        let model_content = MessageContentConverter::to_model_format(&content);

        assert_eq!(
            model_content,
            json!([{ "type": "text", "text": MODEL_IMAGE_OMITTED_PLACEHOLDER }])
        );
        assert_eq!(
            MessageContentConverter::estimate_prompt_tokens(&content),
            count_tokens(MODEL_IMAGE_OMITTED_PLACEHOLDER)
        );
        assert!(!model_content.to_string().contains("sensitive-image-bytes"));
    }

    #[test]
    fn test_assistant_text_to_content() {
        let content = MessageContentConverter::assistant_text_to_content("response".to_string());
        assert_eq!(content.len(), 1);
        match &content[0] {
            ConversationContent::OutputText { text } => {
                assert_eq!(text, "response");
            }
            _ => panic!("Expected OutputText"),
        }
    }
}
