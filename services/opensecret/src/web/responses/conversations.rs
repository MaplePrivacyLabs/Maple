//! OpenAI Conversations API implementation
//! Provides server-side conversation state management compatible with OpenAI's Conversations API

use crate::{
    db::DBError,
    encrypt::{decrypt_content, encrypt_with_key},
    jwt::AuthContext,
    models::response_compaction::{CompactionError, CompactionMarkerMetadata},
    models::responses::{ConversationProjectFilter, NewConversation},
    models::users::User,
    web::{
        encryption_middleware::{decrypt_request, encrypt_response, Decrypted, TransportSession},
        responses::{
            constants::{
                self, DEFAULT_PAGINATION_LIMIT, DEFAULT_PAGINATION_ORDER, MAX_PAGINATION_LIMIT,
                OBJECT_TYPE_LIST, OBJECT_TYPE_LIST_DELETED,
            },
            error_mapping, ConversationBuilder, ConversationItem, ConversationItemConverter,
            DeletedObjectResponse, MessageContent, NullableField, Paginator,
        },
    },
    ApiError, AppState,
};
use axum::{
    extract::{Path, Query, State},
    middleware::from_fn_with_state,
    response::Response,
    routing::{delete, get, post},
    Extension, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};
use tracing::error;
use uuid::Uuid;

// ============================================================================
// Context Helper
// ============================================================================

/// Conversation context with decryption key
///
/// This helper encapsulates the common pattern of loading a conversation
/// and the user's encryption key together. Used by most conversation handlers.
struct ConversationContext {
    conversation: crate::models::responses::Conversation,
    user_key: secp256k1::SecretKey,
}

impl ConversationContext {
    /// Load conversation and user's encryption key in one operation
    ///
    /// Verifies conversation exists, user owns it, and retrieves encryption key.
    ///
    /// # Arguments
    /// * `state` - Application state
    /// * `conversation_id` - UUID of the conversation to load
    /// * `user_uuid` - UUID of the user requesting access
    ///
    /// # Returns
    /// ConversationContext containing the conversation and encryption key
    ///
    /// # Errors
    /// Returns ApiError if conversation not found, user doesn't own it, or key retrieval fails
    async fn load(
        state: &AppState,
        conversation_id: Uuid,
        user: &User,
        auth_context: &AuthContext,
    ) -> Result<Self, ApiError> {
        // Get conversation (verifies ownership)
        let conversation = state
            .db
            .get_conversation_by_uuid_and_user(conversation_id, user.uuid)
            .map_err(error_mapping::map_conversation_error)?;

        // Get user's encryption key
        let user_key = state
            .get_user_key(user, auth_context, None, None)
            .await
            .map_err(|_| error_mapping::map_key_retrieval_error())?;

        Ok(Self {
            conversation,
            user_key,
        })
    }

    /// Decrypt conversation metadata
    ///
    /// # Returns
    /// Option<Value> containing decrypted metadata, or None if no metadata exists
    ///
    /// # Errors
    /// Returns ApiError if decryption fails
    fn decrypt_metadata(&self) -> Result<Option<Value>, ApiError> {
        decrypt_content(&self.user_key, self.conversation.metadata_enc.as_ref())
            .map_err(|_| error_mapping::map_decryption_error("conversation metadata"))
    }
}

// ============================================================================
// Request/Response Types
// ============================================================================

/// Request to create a new conversation
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CreateConversationRequest {
    /// Metadata to attach to the conversation (max 16 key-value pairs)
    #[serde(default)]
    pub metadata: Option<Value>,

    /// Optional project to assign on creation
    #[serde(default)]
    pub project_id: Option<Uuid>,

    /// Whether the conversation should be pinned
    #[serde(default)]
    pub pinned: Option<bool>,

    /// Initial items to include in the conversation
    #[serde(default)]
    pub items: Option<Vec<ConversationInputItem>>,
}

/// Input item when creating conversation items
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type")]
pub enum ConversationInputItem {
    #[serde(rename = "message")]
    Message {
        role: String,
        content: MessageContent,
    },
}

/// Request to update a conversation
#[derive(Debug, Clone, Deserialize)]
pub struct UpdateConversationRequest {
    /// Replaced metadata blob when present
    #[serde(default)]
    pub metadata: Option<Value>,

    /// Project assignment update: omitted = unchanged, null = clear, UUID = assign
    #[serde(default)]
    pub project_id: NullableField<Uuid>,

    /// Pin state update
    #[serde(default)]
    pub pinned: Option<bool>,
}

/// Request to create items in a conversation
#[derive(Debug, Clone, Deserialize, Serialize)]
#[allow(dead_code)]
pub struct CreateConversationItemsRequest {
    /// Items to add to the conversation (max 20 at a time)
    pub items: Vec<ConversationInputItem>,
}

/// Request to batch delete multiple conversations
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BatchDeleteConversationsRequest {
    /// IDs of conversations to delete
    pub ids: Vec<Uuid>,
}

/// Request to batch update conversation project assignments
#[derive(Debug, Clone, Deserialize)]
pub struct BatchUpdateConversationProjectRequest {
    /// IDs of conversations to update
    pub ids: Vec<Uuid>,

    /// Target project ID. Explicit null clears project assignment; omitted is invalid.
    #[serde(default)]
    pub project_id: NullableField<Uuid>,
}

/// Individual result for a batch delete operation
#[derive(Debug, Clone, Serialize)]
pub struct BatchDeleteItemResult {
    /// ID of the conversation
    pub id: Uuid,

    /// Object type (always "conversation.deleted")
    pub object: &'static str,

    /// Whether the deletion was successful
    pub deleted: bool,

    /// Error message if deletion failed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<&'static str>,
}

/// Response for batch delete operations
#[derive(Debug, Clone, Serialize)]
pub struct BatchDeleteConversationsResponse {
    /// Object type (always "list")
    pub object: &'static str,

    /// Results for each requested deletion
    pub data: Vec<BatchDeleteItemResult>,
}

/// Response for batch project update operations
#[derive(Debug, Clone, Serialize)]
pub struct BatchUpdateConversationProjectResponse {
    pub success: bool,
}

/// Response for a conversation object
#[derive(Debug, Clone, Serialize)]
pub struct ConversationResponse {
    /// Conversation ID
    pub id: Uuid,

    /// Object type (always "conversation")
    pub object: &'static str,

    /// Metadata attached to the conversation
    pub metadata: Option<Value>,

    /// Assigned project ID, if any
    pub project_id: Option<Uuid>,

    /// Whether the conversation is pinned
    pub pinned: bool,

    /// Unix timestamp when created
    pub created_at: i64,

    /// Unix timestamp when the conversation last had chat activity
    pub last_activity_at: i64,
}

/// Response for listing conversation items
#[derive(Debug, Clone, Serialize)]
pub struct ConversationItemListResponse {
    pub object: &'static str,
    pub data: Vec<ConversationItem>,
    pub has_more: bool,
    pub first_id: Option<Uuid>,
    pub last_id: Option<Uuid>,
}

/// Additive visibility metadata. Canonical conversation items are unchanged.
#[derive(Debug, Serialize)]
pub struct ConversationCompactionMarker {
    pub id: Uuid,
    pub object: &'static str,
    pub status: &'static str,
    pub response_id: Uuid,
    /// None for preparation before this response's user item is persisted.
    /// Otherwise the exact canonical item preceding this checkpoint.
    pub after_item_id: Option<Uuid>,
    pub created_at: i64,
    pub generation: i64,
}

#[derive(Debug, Serialize)]
pub struct ConversationCompactionListResponse {
    pub object: &'static str,
    pub data: Vec<ConversationCompactionMarker>,
    pub has_more: bool,
    pub first_id: Option<Uuid>,
    pub last_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
pub struct ListCompactionsParams {
    #[serde(default = "default_limit")]
    pub limit: i64,
    pub after: Option<Uuid>,
    #[serde(default = "default_order")]
    pub order: String,
}

impl ListCompactionsParams {
    fn validate(&self) -> Result<(), ApiError> {
        if !(1..=MAX_PAGINATION_LIMIT).contains(&self.limit)
            || !matches!(self.order.as_str(), "asc" | "desc")
        {
            return Err(ApiError::BadRequest);
        }
        Ok(())
    }
}

fn compaction_marker_page(
    mut rows: Vec<CompactionMarkerMetadata>,
    limit: usize,
) -> ConversationCompactionListResponse {
    let has_more = rows.len() > limit;
    rows.truncate(limit);
    let data: Vec<_> = rows
        .into_iter()
        .map(|row| ConversationCompactionMarker {
            id: row.uuid,
            object: "conversation.compaction",
            status: "completed",
            response_id: row.origin_response_uuid,
            after_item_id: row.after_item_uuid,
            created_at: row.created_at.timestamp(),
            generation: row.generation,
        })
        .collect();
    ConversationCompactionListResponse {
        object: OBJECT_TYPE_LIST,
        first_id: data.first().map(|item| item.id),
        last_id: data.last().map(|item| item.id),
        has_more,
        data,
    }
}

/// Response for listing conversations (custom OpenSecret extension)
#[derive(Debug, Clone, Serialize)]
pub struct ConversationListResponse {
    pub object: &'static str,
    pub data: Vec<ConversationResponse>,
    pub has_more: bool,
    pub first_id: Option<Uuid>,
    pub last_id: Option<Uuid>,
}

/// Query parameters for listing conversations (custom extension)
#[derive(Debug, Deserialize)]
pub struct ListConversationsParams {
    #[serde(default = "default_limit")]
    pub limit: i64,
    pub after: Option<Uuid>,
    #[serde(default = "default_order")]
    pub order: String,
    pub project_id: Option<Uuid>,
    pub unassigned_project: Option<bool>,
    pub pinned: Option<bool>,
}

impl ListConversationsParams {
    fn validate(&self) -> Result<(), ApiError> {
        if self.project_id.is_some() && self.unassigned_project == Some(true) {
            return Err(ApiError::BadRequest);
        }

        Ok(())
    }
}

/// Query parameters for listing conversation items
#[derive(Debug, Deserialize)]
pub struct ListItemsParams {
    #[serde(default = "default_limit")]
    pub limit: i64,
    pub after: Option<Uuid>,
    #[serde(default = "default_order")]
    pub order: String,
    /// Additional fields to include in the response
    /// TODO: Not yet implemented - will be used to include additional data like logprobs, sources, etc.
    #[allow(dead_code)]
    pub include: Option<Vec<String>>,
}

fn default_limit() -> i64 {
    DEFAULT_PAGINATION_LIMIT
}

fn default_order() -> String {
    DEFAULT_PAGINATION_ORDER.to_string()
}

fn resolve_conversation_project_filter(
    state: &AppState,
    user_uuid: Uuid,
    params: &ListConversationsParams,
) -> Result<ConversationProjectFilter, ApiError> {
    params.validate()?;

    if params.unassigned_project == Some(true) {
        return Ok(ConversationProjectFilter::Unassigned);
    }

    if let Some(project_uuid) = params.project_id {
        let project_id = state
            .db
            .get_conversation_project_by_uuid_and_user(project_uuid, user_uuid)
            .map_err(error_mapping::map_conversation_project_error)?
            .id;

        return Ok(ConversationProjectFilter::Assigned(project_id));
    }

    Ok(ConversationProjectFilter::Any)
}

fn validate_metadata(metadata: &Value) -> Result<(), ApiError> {
    if metadata.is_object() {
        Ok(())
    } else {
        Err(ApiError::BadRequest)
    }
}

fn resolve_project_uuid(
    state: &AppState,
    user_uuid: Uuid,
    project_id: Option<i64>,
    cache: &mut HashMap<i64, Uuid>,
) -> Result<Option<Uuid>, ApiError> {
    let Some(project_id) = project_id else {
        return Ok(None);
    };

    if let Some(project_uuid) = cache.get(&project_id) {
        return Ok(Some(*project_uuid));
    }

    let project_uuid = state
        .db
        .get_conversation_project_by_id_and_user(project_id, user_uuid)
        .map_err(error_mapping::map_conversation_project_error)?
        .uuid;

    cache.insert(project_id, project_uuid);
    Ok(Some(project_uuid))
}

fn build_conversation_response(
    state: &AppState,
    user_uuid: Uuid,
    conversation: &crate::models::responses::Conversation,
    user_key: &secp256k1::SecretKey,
    metadata: Option<Value>,
    project_cache: &mut HashMap<i64, Uuid>,
) -> Result<ConversationResponse, ApiError> {
    let metadata = match metadata {
        Some(metadata) => Some(metadata),
        None => decrypt_content(user_key, conversation.metadata_enc.as_ref())
            .map_err(|_| error_mapping::map_decryption_error("conversation metadata"))?,
    };

    let project_id =
        resolve_project_uuid(state, user_uuid, conversation.project_id, project_cache)?;

    Ok(ConversationBuilder::from_conversation(conversation)
        .metadata(metadata)
        .project_id(project_id)
        .build())
}

// ============================================================================
// Handlers
// ============================================================================

/// POST /v1/conversations - Create a new conversation
async fn create_conversation(
    State(state): State<Arc<AppState>>,
    Extension(session_id): Extension<TransportSession>,
    Extension(user): Extension<User>,
    Extension(auth_context): Extension<AuthContext>,
    Decrypted(body): Decrypted<CreateConversationRequest>,
) -> Result<Response, ApiError> {
    // Reject initial items - not supported in our simplified flow
    // Users must use POST /v1/responses to add messages to conversations
    if let Some(items) = &body.items {
        if !items.is_empty() {
            error!(
                "Initial items not supported in conversation creation for user: {}",
                user.uuid
            );
            return Err(ApiError::BadRequest);
        }
    }

    // Get user's encryption key
    let user_key = state
        .get_user_key(&user, &auth_context, None, None)
        .await
        .map_err(|_| error_mapping::map_key_retrieval_error())?;

    let project = if let Some(project_uuid) = body.project_id {
        Some(
            state
                .db
                .get_conversation_project_by_uuid_and_user(project_uuid, user.uuid)
                .map_err(error_mapping::map_conversation_project_error)?,
        )
    } else {
        None
    };

    // Create the conversation
    let conversation_uuid = Uuid::new_v4();

    // Create metadata with title
    let mut metadata = body.metadata.clone().unwrap_or_else(|| json!({}));
    validate_metadata(&metadata)?;
    if metadata.get("title").is_none() {
        metadata["title"] = json!("New Conversation");
    }

    // Encrypt the entire metadata object
    let metadata_json = serde_json::to_string(&metadata).map_err(|e| {
        error!("Failed to serialize metadata: {:?}", e);
        ApiError::InternalServerError
    })?;
    let metadata_enc = Some(encrypt_with_key(&user_key, metadata_json.as_bytes()).await);

    let new_conversation = NewConversation {
        uuid: conversation_uuid,
        user_id: user.uuid,
        project_id: project.as_ref().map(|project| project.id),
        is_pinned: body.pinned.unwrap_or(false),
        metadata_enc,
    };

    let conversation = state
        .db
        .create_conversation(new_conversation)
        .map_err(error_mapping::map_generic_db_error)?;

    let mut project_cache = HashMap::new();
    if let Some(project) = project {
        project_cache.insert(project.id, project.uuid);
    }

    let response = build_conversation_response(
        &state,
        user.uuid,
        &conversation,
        &user_key,
        Some(metadata),
        &mut project_cache,
    )?;

    encrypt_response(&state, &session_id, &response).await
}

/// GET /v1/conversations/{id} - Retrieve a conversation
async fn get_conversation(
    State(state): State<Arc<AppState>>,
    Path(conversation_id): Path<Uuid>,
    Extension(session_id): Extension<TransportSession>,
    Extension(user): Extension<User>,
    Extension(auth_context): Extension<AuthContext>,
) -> Result<Response, ApiError> {
    let ctx = ConversationContext::load(&state, conversation_id, &user, &auth_context).await?;
    let mut project_cache = HashMap::new();
    let response = build_conversation_response(
        &state,
        user.uuid,
        &ctx.conversation,
        &ctx.user_key,
        ctx.decrypt_metadata()?,
        &mut project_cache,
    )?;

    encrypt_response(&state, &session_id, &response).await
}

/// POST /v1/conversations/{id} - Update a conversation
async fn update_conversation(
    State(state): State<Arc<AppState>>,
    Path(conversation_id): Path<Uuid>,
    Extension(session_id): Extension<TransportSession>,
    Extension(user): Extension<User>,
    Extension(auth_context): Extension<AuthContext>,
    Decrypted(body): Decrypted<UpdateConversationRequest>,
) -> Result<Response, ApiError> {
    if body.metadata.is_none() && body.project_id.is_missing() && body.pinned.is_none() {
        return Err(ApiError::BadRequest);
    }

    let ctx = ConversationContext::load(&state, conversation_id, &user, &auth_context).await?;

    let metadata_enc = if let Some(metadata) = body.metadata.as_ref() {
        validate_metadata(metadata)?;
        let metadata_json = serde_json::to_string(metadata).map_err(|e| {
            error!("Failed to serialize metadata: {:?}", e);
            ApiError::InternalServerError
        })?;
        Some(encrypt_with_key(&ctx.user_key, metadata_json.as_bytes()).await)
    } else {
        None
    };

    let mut project_cache = HashMap::new();
    let project_update = match body.project_id {
        NullableField::Value(project_uuid) => {
            let project = state
                .db
                .get_conversation_project_by_uuid_and_user(project_uuid, user.uuid)
                .map_err(error_mapping::map_conversation_project_error)?;
            project_cache.insert(project.id, project.uuid);
            Some(Some(project.id))
        }
        NullableField::Null => Some(None),
        NullableField::Missing => None,
    };

    let updated_conversation = state
        .db
        .update_conversation(
            ctx.conversation.id,
            user.uuid,
            metadata_enc,
            project_update,
            body.pinned,
        )
        .map_err(error_mapping::map_conversation_error)?;

    let response = build_conversation_response(
        &state,
        user.uuid,
        &updated_conversation,
        &ctx.user_key,
        body.metadata.clone(),
        &mut project_cache,
    )?;

    encrypt_response(&state, &session_id, &response).await
}

/// DELETE /v1/conversations/{id} - Delete a conversation
async fn delete_conversation(
    State(state): State<Arc<AppState>>,
    Path(conversation_id): Path<Uuid>,
    Extension(session_id): Extension<TransportSession>,
    Extension(user): Extension<User>,
    Extension(auth_context): Extension<AuthContext>,
) -> Result<Response, ApiError> {
    let ctx = ConversationContext::load(&state, conversation_id, &user, &auth_context).await?;

    // Delete the conversation (cascades will delete all associated responses)
    state
        .db
        .delete_conversation(ctx.conversation.id, user.uuid)
        .map_err(error_mapping::map_generic_db_error)?;

    let response = DeletedObjectResponse::conversation(ctx.conversation.uuid);

    encrypt_response(&state, &session_id, &response).await
}

/// GET /v1/conversations/{id}/items - List conversation items
async fn list_conversation_items(
    State(state): State<Arc<AppState>>,
    Path(conversation_id): Path<Uuid>,
    Query(params): Query<ListItemsParams>,
    Extension(session_id): Extension<TransportSession>,
    Extension(user): Extension<User>,
    Extension(auth_context): Extension<AuthContext>,
) -> Result<Response, ApiError> {
    let ctx = ConversationContext::load(&state, conversation_id, &user, &auth_context).await?;

    // Validate limit
    let limit = if params.limit <= 0 {
        DEFAULT_PAGINATION_LIMIT
    } else {
        params.limit.min(MAX_PAGINATION_LIMIT)
    };

    // Fetch messages with database-level pagination
    // We fetch limit + 1 to check if there are more results
    let raw_messages = state
        .db
        .get_conversation_context_messages(
            ctx.conversation.id,
            limit + 1,
            params.after,
            &params.order,
        )
        .map_err(error_mapping::map_message_error)?;

    // Check if there are more results
    let has_more = raw_messages.len() > limit as usize;

    // Take only the requested limit
    let messages_to_return = if has_more {
        &raw_messages[..limit as usize]
    } else {
        &raw_messages[..]
    };

    // Convert messages to items
    let items = ConversationItemConverter::messages_to_items(
        messages_to_return,
        &ctx.user_key,
        0, // Start from beginning since pagination is already applied at DB level
        messages_to_return.len(),
    )?;

    // Extract cursor IDs
    let (first_id, last_id) = Paginator::get_cursor_ids(&items, |item| match item {
        ConversationItem::Message { id, .. } => *id,
        ConversationItem::FunctionToolCall { id, .. } => *id,
        ConversationItem::FunctionToolCallOutput { id, .. } => *id,
        ConversationItem::Reasoning { id, .. } => *id,
    });

    let response = ConversationItemListResponse {
        object: OBJECT_TYPE_LIST,
        data: items.clone(),
        has_more,
        first_id,
        last_id,
    };

    encrypt_response(&state, &session_id, &response).await
}

/// GET /v1/conversations/{id}/compactions - Paginated durable markers only.
async fn list_conversation_compactions(
    State(state): State<Arc<AppState>>,
    Path(conversation_id): Path<Uuid>,
    Query(params): Query<ListCompactionsParams>,
    Extension(session_id): Extension<TransportSession>,
    Extension(user): Extension<User>,
) -> Result<Response, ApiError> {
    params.validate()?;
    let conversation = state
        .db
        .get_conversation_by_uuid_and_user(conversation_id, user.uuid)
        .map_err(error_mapping::map_conversation_error)?;
    let rows = state
        .db
        .list_conversation_compaction_markers(
            conversation.id,
            user.uuid,
            params.limit + 1,
            params.after,
            &params.order,
        )
        .map_err(|error| match error {
            DBError::CompactionError(CompactionError::InvalidSelection) => ApiError::BadRequest,
            DBError::CompactionError(CompactionError::ConversationNotFound) => ApiError::NotFound,
            _ => ApiError::InternalServerError,
        })?;
    let response = compaction_marker_page(rows, params.limit as usize);
    encrypt_response(&state, &session_id, &response).await
}

/// GET /v1/conversations/{conversation_id}/items/{item_id} - Retrieve a single item
async fn get_conversation_item(
    State(state): State<Arc<AppState>>,
    Path((conversation_id, item_id)): Path<(Uuid, Uuid)>,
    Extension(session_id): Extension<TransportSession>,
    Extension(user): Extension<User>,
    Extension(auth_context): Extension<AuthContext>,
) -> Result<Response, ApiError> {
    let ctx = ConversationContext::load(&state, conversation_id, &user, &auth_context).await?;

    // Fetch only the requested item, scoped to the authorized conversation
    let raw_message = state
        .db
        .get_conversation_item(ctx.conversation.id, item_id)
        .map_err(error_mapping::map_message_error)?
        .ok_or(ApiError::NotFound)?;

    let item = ConversationItemConverter::message_to_item(&raw_message, &ctx.user_key)?;
    encrypt_response(&state, &session_id, &item).await
}

/// GET /v1/conversations - List all conversations (OpenSecret custom extension)
async fn list_conversations(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ListConversationsParams>,
    Extension(session_id): Extension<TransportSession>,
    Extension(user): Extension<User>,
    Extension(auth_context): Extension<AuthContext>,
) -> Result<Response, ApiError> {
    // Validate limit
    let limit = if params.limit <= 0 {
        DEFAULT_PAGINATION_LIMIT
    } else {
        params.limit.min(MAX_PAGINATION_LIMIT)
    };

    let project_filter = resolve_conversation_project_filter(&state, user.uuid, &params)?;

    // Fetch conversations with database-level pagination
    // We fetch limit + 1 to check if there are more results
    let conversations = state
        .db
        .list_conversations(
            user.uuid,
            limit + 1,
            params.after,
            &params.order,
            project_filter,
            params.pinned,
        )
        .map_err(error_mapping::map_generic_db_error)?;

    // Check if there are more results
    let has_more = conversations.len() > limit as usize;

    // Take only the requested limit
    let conversations_to_return = if has_more {
        &conversations[..limit as usize]
    } else {
        &conversations[..]
    };

    // Get user's encryption key for decrypting metadata
    let user_key = state
        .get_user_key(&user, &auth_context, None, None)
        .await
        .map_err(|_| ApiError::InternalServerError)?;

    let mut project_cache = HashMap::new();

    // Convert to response format
    let mut data = Vec::with_capacity(conversations_to_return.len());
    for conv in conversations_to_return {
        data.push(build_conversation_response(
            &state,
            user.uuid,
            conv,
            &user_key,
            None,
            &mut project_cache,
        )?);
    }

    // Extract cursor IDs
    let (first_id, last_id) = Paginator::get_cursor_ids(conversations_to_return, |c| c.uuid);

    let response = ConversationListResponse {
        object: OBJECT_TYPE_LIST,
        data: data.clone(),
        has_more,
        first_id,
        last_id,
    };

    encrypt_response(&state, &session_id, &response).await
}

/// DELETE /v1/conversations - Delete all conversations
async fn delete_all_conversations(
    State(state): State<Arc<AppState>>,
    Extension(session_id): Extension<TransportSession>,
    Extension(user): Extension<User>,
) -> Result<Response, ApiError> {
    state
        .db
        .delete_all_conversations(user.uuid)
        .map_err(error_mapping::map_generic_db_error)?;

    let response = json!({
        "object": OBJECT_TYPE_LIST_DELETED,
        "deleted": true
    });

    encrypt_response(&state, &session_id, &response).await
}

/// Maximum number of conversations allowed in a single batch operation request
const MAX_CONVERSATION_BATCH_SIZE: usize = 20;

/// POST /v1/conversations/batch-delete - Delete multiple specific conversations
async fn batch_delete_conversations(
    State(state): State<Arc<AppState>>,
    Extension(session_id): Extension<TransportSession>,
    Extension(user): Extension<User>,
    Decrypted(body): Decrypted<BatchDeleteConversationsRequest>,
) -> Result<Response, ApiError> {
    // Validate batch size
    if body.ids.is_empty() || body.ids.len() > MAX_CONVERSATION_BATCH_SIZE {
        return Err(ApiError::BadRequest);
    }

    let mut results = Vec::with_capacity(body.ids.len());

    for conversation_id in body.ids {
        // Try to get the conversation first to verify ownership
        match state
            .db
            .get_conversation_by_uuid_and_user(conversation_id, user.uuid)
        {
            Ok(conversation) => {
                // Delete the conversation
                match state.db.delete_conversation(conversation.id, user.uuid) {
                    Ok(()) => {
                        results.push(BatchDeleteItemResult {
                            id: conversation_id,
                            object: constants::OBJECT_TYPE_CONVERSATION_DELETED,
                            deleted: true,
                            error: None,
                        });
                    }
                    Err(e) => {
                        error!("Failed to delete conversation {}: {:?}", conversation_id, e);
                        results.push(BatchDeleteItemResult {
                            id: conversation_id,
                            object: constants::OBJECT_TYPE_CONVERSATION_DELETED,
                            deleted: false,
                            error: Some("delete_failed"),
                        });
                    }
                }
            }
            Err(_) => {
                results.push(BatchDeleteItemResult {
                    id: conversation_id,
                    object: constants::OBJECT_TYPE_CONVERSATION_DELETED,
                    deleted: false,
                    error: Some("not_found"),
                });
            }
        }
    }

    let response = BatchDeleteConversationsResponse {
        object: OBJECT_TYPE_LIST,
        data: results,
    };

    encrypt_response(&state, &session_id, &response).await
}

/// POST /v1/conversations/batch-update-project - Update project assignment for multiple conversations
async fn batch_update_conversation_project(
    State(state): State<Arc<AppState>>,
    Extension(session_id): Extension<TransportSession>,
    Extension(user): Extension<User>,
    Decrypted(body): Decrypted<BatchUpdateConversationProjectRequest>,
) -> Result<Response, ApiError> {
    if body.ids.is_empty() || body.ids.len() > MAX_CONVERSATION_BATCH_SIZE {
        return Err(ApiError::BadRequest);
    }

    let (_, target_project_id) = match body.project_id {
        NullableField::Value(project_uuid) => (
            Some(project_uuid),
            Some(
                state
                    .db
                    .get_conversation_project_by_uuid_and_user(project_uuid, user.uuid)
                    .map_err(error_mapping::map_conversation_project_error)?
                    .id,
            ),
        ),
        NullableField::Null => (None, None),
        NullableField::Missing => return Err(ApiError::BadRequest),
    };

    state
        .db
        .batch_update_conversation_project(&body.ids, user.uuid, target_project_id)
        .map_err(error_mapping::map_batch_conversation_project_error)?;

    let response = BatchUpdateConversationProjectResponse { success: true };

    encrypt_response(&state, &session_id, &response).await
}

// ============================================================================
// Router Configuration
// ============================================================================

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/v1/conversations",
            post(create_conversation).layer(from_fn_with_state(
                state.clone(),
                decrypt_request::<CreateConversationRequest>,
            )),
        )
        .route(
            "/v1/conversations",
            get(list_conversations).layer(from_fn_with_state(state.clone(), decrypt_request::<()>)),
        )
        .route(
            "/v1/conversations",
            delete(delete_all_conversations)
                .layer(from_fn_with_state(state.clone(), decrypt_request::<()>)),
        )
        .route(
            "/v1/conversations/batch-delete",
            post(batch_delete_conversations).layer(from_fn_with_state(
                state.clone(),
                decrypt_request::<BatchDeleteConversationsRequest>,
            )),
        )
        .route(
            "/v1/conversations/batch-update-project",
            post(batch_update_conversation_project).layer(from_fn_with_state(
                state.clone(),
                decrypt_request::<BatchUpdateConversationProjectRequest>,
            )),
        )
        .route(
            "/v1/conversations/:id",
            get(get_conversation).layer(from_fn_with_state(state.clone(), decrypt_request::<()>)),
        )
        .route(
            "/v1/conversations/:id",
            post(update_conversation).layer(from_fn_with_state(
                state.clone(),
                decrypt_request::<UpdateConversationRequest>,
            )),
        )
        .route(
            "/v1/conversations/:id",
            delete(delete_conversation)
                .layer(from_fn_with_state(state.clone(), decrypt_request::<()>)),
        )
        /* TODO IGNORE FOR NOW - TOO CONFUSING AND NOT NEEDED
        .route(
            "/v1/conversations/:id/items",
            post(create_conversation_items).layer(from_fn_with_state(
                state.clone(),
                decrypt_request::<CreateConversationItemsRequest>,
            )),
        )
        */
        .route(
            "/v1/conversations/:id/items",
            get(list_conversation_items)
                .layer(from_fn_with_state(state.clone(), decrypt_request::<()>)),
        )
        .route(
            "/v1/conversations/:id/compactions",
            get(list_conversation_compactions)
                .layer(from_fn_with_state(state.clone(), decrypt_request::<()>)),
        )
        .route(
            "/v1/conversations/:id/items/:item_id",
            get(get_conversation_item)
                .layer(from_fn_with_state(state.clone(), decrypt_request::<()>)),
        )
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::{compaction_marker_page, ListCompactionsParams};
    use super::{
        default_limit, default_order, BatchUpdateConversationProjectRequest,
        ListConversationsParams, UpdateConversationRequest,
    };
    use crate::models::response_compaction::CompactionMarkerMetadata;
    use crate::web::responses::NullableField;
    use crate::ApiError;
    use uuid::Uuid;

    #[test]
    fn compaction_visibility_rejects_unbounded_pages_and_unknown_order() {
        for limit in [-1, 0, 101, i64::MAX] {
            let params = ListCompactionsParams {
                limit,
                after: None,
                order: "desc".into(),
            };
            assert!(matches!(params.validate(), Err(ApiError::BadRequest)));
        }
        for order in ["asc", "desc"] {
            for limit in [1, 20, 100] {
                assert!(ListCompactionsParams {
                    limit,
                    after: None,
                    order: order.into()
                }
                .validate()
                .is_ok());
            }
        }
        assert!(ListCompactionsParams {
            limit: 20,
            after: None,
            order: "ASC; SELECT 1".into()
        }
        .validate()
        .is_err());
        let defaults: ListCompactionsParams = serde_json::from_str("{}").unwrap();
        assert_eq!(defaults.limit, 20);
        assert_eq!(defaults.order, "desc");
    }

    #[test]
    fn compaction_marker_projection_is_bounded_and_contains_only_safe_metadata() {
        let rows: Vec<_> = (1..=3)
            .map(|generation| CompactionMarkerMetadata {
                uuid: Uuid::new_v4(),
                origin_response_uuid: Uuid::new_v4(),
                after_item_uuid: (generation == 2).then(Uuid::new_v4),
                created_at: chrono::Utc::now(),
                generation,
            })
            .collect();
        let first = rows[0].uuid;
        let last = rows[1].uuid;
        let page = compaction_marker_page(rows, 2);
        assert!(page.has_more);
        assert_eq!(page.first_id, Some(first));
        assert_eq!(page.last_id, Some(last));
        assert_eq!(page.data.len(), 2);
        let wire = serde_json::to_value(page).unwrap();
        for marker in wire["data"].as_array().unwrap() {
            let keys: std::collections::HashSet<_> = marker
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(
                keys,
                [
                    "id",
                    "object",
                    "status",
                    "response_id",
                    "after_item_id",
                    "created_at",
                    "generation"
                ]
                .into_iter()
                .collect()
            );
            assert_eq!(marker["object"], "conversation.compaction");
            assert_eq!(marker["status"], "completed");
        }
        assert!(wire["data"][0]["after_item_id"].is_null());
        assert!(wire["data"][1]["after_item_id"].is_string());
        let empty = compaction_marker_page(Vec::new(), 20);
        assert!(!empty.has_more);
        assert!(empty.first_id.is_none() && empty.last_id.is_none());
    }

    #[test]
    fn update_request_distinguishes_null_from_omitted_project_id() {
        let request: UpdateConversationRequest =
            serde_json::from_str(r#"{"project_id":null}"#).unwrap();
        assert!(matches!(request.project_id, NullableField::Null));

        let request: UpdateConversationRequest =
            serde_json::from_str(r#"{"project_id":"550e8400-e29b-41d4-a716-446655440000"}"#)
                .unwrap();
        assert!(matches!(request.project_id, NullableField::Value(_)));

        let omitted: UpdateConversationRequest = serde_json::from_str(r#"{}"#).unwrap();
        assert!(matches!(omitted.project_id, NullableField::Missing));
    }

    #[test]
    fn batch_update_request_requires_explicit_project_id() {
        let request: BatchUpdateConversationProjectRequest = serde_json::from_str(
            r#"{"ids":["550e8400-e29b-41d4-a716-446655440000"],"project_id":null}"#,
        )
        .unwrap();
        assert!(matches!(request.project_id, NullableField::Null));

        let request: BatchUpdateConversationProjectRequest =
            serde_json::from_str(
                r#"{"ids":["550e8400-e29b-41d4-a716-446655440000"],"project_id":"550e8400-e29b-41d4-a716-446655440001"}"#,
            )
            .unwrap();
        assert!(matches!(request.project_id, NullableField::Value(_)));

        let omitted: BatchUpdateConversationProjectRequest =
            serde_json::from_str(r#"{"ids":["550e8400-e29b-41d4-a716-446655440000"]}"#).unwrap();
        assert!(matches!(omitted.project_id, NullableField::Missing));
    }

    #[test]
    fn list_conversation_params_allow_unassigned_project_without_project_id() {
        let params = ListConversationsParams {
            limit: default_limit(),
            after: None,
            order: default_order(),
            project_id: None,
            unassigned_project: Some(true),
            pinned: None,
        };

        assert!(params.validate().is_ok());
    }

    #[test]
    fn list_conversation_params_reject_conflicting_project_filters() {
        let params = ListConversationsParams {
            limit: default_limit(),
            after: None,
            order: default_order(),
            project_id: Some(Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap()),
            unassigned_project: Some(true),
            pinned: None,
        };

        assert!(matches!(params.validate(), Err(ApiError::BadRequest)));
    }
}
