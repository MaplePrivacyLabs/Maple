//! User Instructions API implementation
//! Provides CRUD operations for custom user instructions (system prompts)

use crate::{
    encrypt::{decrypt_string, encrypt_with_key},
    jwt::AuthContext,
    models::responses::NewUserInstruction,
    models::users::User,
    tokens::count_tokens,
    web::{
        encryption_middleware::{decrypt_request, encrypt_response, Decrypted, TransportSession},
        responses::{
            constants::{
                DEFAULT_PAGINATION_LIMIT, DEFAULT_PAGINATION_ORDER, MAX_PAGINATION_LIMIT,
                OBJECT_TYPE_LIST,
            },
            error_mapping, DeletedObjectResponse, Paginator,
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
use std::sync::Arc;
use tracing::warn;
use tracing::{debug, error};
use uuid::Uuid;

// ============================================================================
// Context Helper
// ============================================================================

/// User instruction context with decryption key
///
/// This helper encapsulates the common pattern of loading a user instruction
/// and the user's encryption key together. Used by most instruction handlers.
struct InstructionContext {
    instruction: crate::models::responses::UserInstruction,
    user_key: secp256k1::SecretKey,
}

impl InstructionContext {
    /// Load instruction and user's encryption key in one operation
    ///
    /// Verifies instruction exists, user owns it, and retrieves encryption key.
    ///
    /// # Arguments
    /// * `state` - Application state
    /// * `instruction_id` - UUID of the instruction to load
    /// * `user_uuid` - UUID of the user requesting access
    ///
    /// # Returns
    /// InstructionContext containing the instruction and encryption key
    ///
    /// # Errors
    /// Returns ApiError if instruction not found, user doesn't own it, or key retrieval fails
    async fn load(
        state: &AppState,
        instruction_id: Uuid,
        user: &User,
        auth_context: &AuthContext,
    ) -> Result<Self, ApiError> {
        // Get instruction (verifies ownership)
        let instruction = state
            .db
            .get_user_instruction_by_uuid_and_user(instruction_id, user.uuid)
            .map_err(error_mapping::map_instruction_error)?;

        // Get user's encryption key
        let user_key = state
            .get_user_key(user, auth_context, None, None)
            .await
            .map_err(|_| error_mapping::map_key_retrieval_error())?;

        Ok(Self {
            instruction,
            user_key,
        })
    }

    /// Decrypt instruction name and prompt
    ///
    /// # Returns
    /// Tuple of (name, prompt)
    ///
    /// # Errors
    /// Returns ApiError if decryption fails
    fn decrypt_content(&self) -> Result<(String, String), ApiError> {
        let name = decrypt_string(&self.user_key, self.instruction.name_enc.as_ref())
            .map_err(|_| error_mapping::map_decryption_error("instruction name"))?
            .ok_or_else(|| {
                error!("Instruction name decryption returned None");
                ApiError::InternalServerError
            })?;

        let prompt = decrypt_string(&self.user_key, Some(&self.instruction.prompt_enc))
            .map_err(|_| error_mapping::map_decryption_error("instruction prompt"))?
            .ok_or_else(|| {
                error!("Instruction prompt decryption returned None");
                ApiError::InternalServerError
            })?;

        Ok((name, prompt))
    }
}

// ============================================================================
// Request/Response Types
// ============================================================================

/// Request to create a new user instruction
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CreateInstructionRequest {
    /// Name of the instruction
    pub name: String,

    /// The system prompt text
    pub prompt: String,

    /// Whether this should be the default instruction
    #[serde(default)]
    pub is_default: bool,
}

/// Request to update a user instruction
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UpdateInstructionRequest {
    /// Updated name (optional)
    pub name: Option<String>,

    /// Updated prompt text (optional)
    pub prompt: Option<String>,

    /// Whether this should be the default instruction (optional)
    pub is_default: Option<bool>,
}

/// Response for a user instruction object
#[derive(Debug, Clone, Serialize)]
pub struct InstructionResponse {
    /// Instruction ID
    pub id: Uuid,

    /// Object type (always "instruction")
    pub object: &'static str,

    /// Name of the instruction
    pub name: String,

    /// The system prompt text
    pub prompt: String,

    /// Token count for the prompt
    pub prompt_tokens: i32,

    /// Whether this is the default instruction
    pub is_default: bool,

    /// Unix timestamp when created
    pub created_at: i64,

    /// Unix timestamp when last updated
    pub updated_at: i64,
}

/// Response for listing user instructions
#[derive(Debug, Clone, Serialize)]
pub struct InstructionListResponse {
    pub object: &'static str,
    pub data: Vec<InstructionResponse>,
    pub has_more: bool,
    pub first_id: Option<Uuid>,
    pub last_id: Option<Uuid>,
}

/// Query parameters for listing instructions
#[derive(Debug, Deserialize)]
pub struct ListInstructionsParams {
    #[serde(default = "default_limit")]
    pub limit: i64,
    pub after: Option<Uuid>,
    #[serde(default = "default_order")]
    pub order: String,
}

fn default_limit() -> i64 {
    DEFAULT_PAGINATION_LIMIT
}

fn default_order() -> String {
    DEFAULT_PAGINATION_ORDER.to_string()
}

// ============================================================================
// Builder Pattern
// ============================================================================

/// Builder for InstructionResponse
pub struct InstructionResponseBuilder {
    instruction: crate::models::responses::UserInstruction,
    name: Option<String>,
    prompt: Option<String>,
}

impl InstructionResponseBuilder {
    pub fn new(instruction: crate::models::responses::UserInstruction) -> Self {
        Self {
            instruction,
            name: None,
            prompt: None,
        }
    }

    pub fn name(mut self, name: String) -> Self {
        self.name = Some(name);
        self
    }

    pub fn prompt(mut self, prompt: String) -> Self {
        self.prompt = Some(prompt);
        self
    }

    pub fn build(self) -> InstructionResponse {
        InstructionResponse {
            id: self.instruction.uuid,
            object: "instruction",
            name: self.name.unwrap_or_default(),
            prompt: self.prompt.unwrap_or_default(),
            prompt_tokens: self.instruction.prompt_tokens,
            is_default: self.instruction.is_default,
            created_at: self.instruction.created_at.timestamp(),
            updated_at: self.instruction.updated_at.timestamp(),
        }
    }
}

// ============================================================================
// Handlers
// ============================================================================

/// POST /v1/instructions - Create a new user instruction
async fn create_instruction(
    State(state): State<Arc<AppState>>,
    Extension(session_id): Extension<TransportSession>,
    Extension(user): Extension<User>,
    Extension(auth_context): Extension<AuthContext>,
    Decrypted(body): Decrypted<CreateInstructionRequest>,
) -> Result<Response, ApiError> {
    debug!("Creating new instruction for user: {}", user.uuid);

    // Validate input
    if body.name.trim().is_empty() {
        error!("Empty instruction name provided");
        return Err(ApiError::BadRequest);
    }

    if body.prompt.trim().is_empty() {
        error!("Empty instruction prompt provided");
        return Err(ApiError::BadRequest);
    }

    // Get user's encryption key
    let user_key = state
        .get_user_key(&user, &auth_context, None, None)
        .await
        .map_err(|_| error_mapping::map_key_retrieval_error())?;

    // Count tokens for the prompt
    let token_count = count_tokens(&body.prompt);
    let prompt_tokens = if token_count > i32::MAX as usize {
        warn!(
            "Prompt token count {} exceeds i32::MAX, clamping to i32::MAX",
            token_count
        );
        i32::MAX
    } else {
        token_count as i32
    };

    // Encrypt name and prompt
    let name_enc = encrypt_with_key(&user_key, body.name.as_bytes()).await;
    let prompt_enc = encrypt_with_key(&user_key, body.prompt.as_bytes()).await;

    let new_instruction = NewUserInstruction {
        uuid: Uuid::new_v4(),
        user_id: user.uuid,
        project_id: None,
        name_enc: Some(name_enc),
        prompt_enc,
        prompt_tokens,
        is_default: body.is_default,
    };

    let instruction = state
        .db
        .create_user_instruction(new_instruction)
        .map_err(error_mapping::map_generic_db_error)?;

    let response = InstructionResponseBuilder::new(instruction)
        .name(body.name.clone())
        .prompt(body.prompt.clone())
        .build();

    encrypt_response(&state, &session_id, &response).await
}

/// GET /v1/instructions/{id} - Retrieve a single user instruction
async fn get_instruction(
    State(state): State<Arc<AppState>>,
    Path(instruction_id): Path<Uuid>,
    Extension(session_id): Extension<TransportSession>,
    Extension(user): Extension<User>,
    Extension(auth_context): Extension<AuthContext>,
) -> Result<Response, ApiError> {
    debug!(
        "Getting instruction {} for user {}",
        instruction_id, user.uuid
    );

    let ctx = InstructionContext::load(&state, instruction_id, &user, &auth_context).await?;
    let (name, prompt) = ctx.decrypt_content()?;

    let response = InstructionResponseBuilder::new(ctx.instruction)
        .name(name)
        .prompt(prompt)
        .build();

    encrypt_response(&state, &session_id, &response).await
}

/// POST /v1/instructions/{id} - Update a user instruction
async fn update_instruction(
    State(state): State<Arc<AppState>>,
    Path(instruction_id): Path<Uuid>,
    Extension(session_id): Extension<TransportSession>,
    Extension(user): Extension<User>,
    Extension(auth_context): Extension<AuthContext>,
    Decrypted(body): Decrypted<UpdateInstructionRequest>,
) -> Result<Response, ApiError> {
    debug!(
        "Updating instruction {} for user {}",
        instruction_id, user.uuid
    );

    // Validate at least one field is being updated
    if body.name.is_none() && body.prompt.is_none() && body.is_default.is_none() {
        error!("No fields to update in instruction request");
        return Err(ApiError::BadRequest);
    }

    let ctx = InstructionContext::load(&state, instruction_id, &user, &auth_context).await?;
    let (current_name, current_prompt) = ctx.decrypt_content()?;

    // Determine final values (use existing if not provided in update)
    let final_name = body.name.as_ref().unwrap_or(&current_name);
    let final_prompt = body.prompt.as_ref().unwrap_or(&current_prompt);
    let final_is_default = body.is_default.unwrap_or(ctx.instruction.is_default);

    // Validate non-empty
    if final_name.trim().is_empty() {
        error!("Cannot update instruction with empty name");
        return Err(ApiError::BadRequest);
    }

    if final_prompt.trim().is_empty() {
        error!("Cannot update instruction with empty prompt");
        return Err(ApiError::BadRequest);
    }

    // Encrypt updated name and prompt (always update both for simplicity)
    let name_enc = encrypt_with_key(&ctx.user_key, final_name.as_bytes()).await;
    let prompt_enc = encrypt_with_key(&ctx.user_key, final_prompt.as_bytes()).await;

    // Count tokens for the final prompt
    let token_count = count_tokens(final_prompt);
    let prompt_tokens = if token_count > i32::MAX as usize {
        warn!(
            "Final prompt token count {} exceeds i32::MAX, clamping to i32::MAX",
            token_count
        );
        i32::MAX
    } else {
        token_count as i32
    };

    // Update instruction in database
    let updated_instruction = state
        .db
        .update_user_instruction(
            ctx.instruction.id,
            user.uuid,
            name_enc,
            prompt_enc,
            prompt_tokens,
            final_is_default,
        )
        .map_err(error_mapping::map_generic_db_error)?;

    let response = InstructionResponseBuilder::new(updated_instruction)
        .name(final_name.clone())
        .prompt(final_prompt.clone())
        .build();

    encrypt_response(&state, &session_id, &response).await
}

/// DELETE /v1/instructions/{id} - Delete a user instruction
async fn delete_instruction(
    State(state): State<Arc<AppState>>,
    Path(instruction_id): Path<Uuid>,
    Extension(session_id): Extension<TransportSession>,
    Extension(user): Extension<User>,
    Extension(auth_context): Extension<AuthContext>,
) -> Result<Response, ApiError> {
    debug!(
        "Deleting instruction {} for user {}",
        instruction_id, user.uuid
    );

    let ctx = InstructionContext::load(&state, instruction_id, &user, &auth_context).await?;

    // Delete the instruction
    state
        .db
        .delete_user_instruction(ctx.instruction.id, user.uuid)
        .map_err(error_mapping::map_generic_db_error)?;

    let response = DeletedObjectResponse {
        id: ctx.instruction.uuid,
        object: "instruction.deleted",
        deleted: true,
    };

    encrypt_response(&state, &session_id, &response).await
}

/// GET /v1/instructions - List all user instructions
async fn list_instructions(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ListInstructionsParams>,
    Extension(session_id): Extension<TransportSession>,
    Extension(user): Extension<User>,
    Extension(auth_context): Extension<AuthContext>,
) -> Result<Response, ApiError> {
    debug!("Listing instructions for user: {}", user.uuid);

    let limit = if params.limit <= 0 {
        DEFAULT_PAGINATION_LIMIT
    } else {
        params.limit.min(MAX_PAGINATION_LIMIT)
    };

    // Get instructions
    let instructions = state
        .db
        .list_user_instructions(user.uuid, limit + 1, params.after, &params.order)
        .map_err(error_mapping::map_generic_db_error)?;

    let has_more = instructions.len() > limit as usize;
    let instructions_to_return = if has_more {
        &instructions[..limit as usize]
    } else {
        &instructions[..]
    };

    // Get user's encryption key for decrypting content
    let user_key = state
        .get_user_key(&user, &auth_context, None, None)
        .await
        .map_err(|_| error_mapping::map_key_retrieval_error())?;

    // Convert to response format
    let data: Vec<InstructionResponse> = instructions_to_return
        .iter()
        .map(|inst| -> Result<InstructionResponse, ApiError> {
            let name = decrypt_string(&user_key, inst.name_enc.as_ref())
                .map_err(|_| error_mapping::map_decryption_error("instruction name"))?
                .ok_or_else(|| {
                    error!("Instruction name decryption returned None");
                    ApiError::InternalServerError
                })?;

            let prompt = decrypt_string(&user_key, Some(&inst.prompt_enc))
                .map_err(|_| error_mapping::map_decryption_error("instruction prompt"))?
                .ok_or_else(|| {
                    error!("Instruction prompt decryption returned None");
                    ApiError::InternalServerError
                })?;

            Ok(InstructionResponseBuilder::new(inst.clone())
                .name(name)
                .prompt(prompt)
                .build())
        })
        .collect::<Result<Vec<_>, _>>()?;

    // Extract cursor IDs using centralized utility
    let (first_id, last_id) = Paginator::get_cursor_ids(instructions_to_return, |inst| inst.uuid);

    let response = InstructionListResponse {
        object: OBJECT_TYPE_LIST,
        data,
        has_more,
        first_id,
        last_id,
    };

    encrypt_response(&state, &session_id, &response).await
}

/// POST /v1/instructions/{id}/set-default - Set an instruction as the default
async fn set_default_instruction(
    State(state): State<Arc<AppState>>,
    Path(instruction_id): Path<Uuid>,
    Extension(session_id): Extension<TransportSession>,
    Extension(user): Extension<User>,
    Extension(auth_context): Extension<AuthContext>,
) -> Result<Response, ApiError> {
    debug!(
        "Setting instruction {} as default for user {}",
        instruction_id, user.uuid
    );

    let ctx = InstructionContext::load(&state, instruction_id, &user, &auth_context).await?;

    // If already default, just return it
    if ctx.instruction.is_default {
        let (name, prompt) = ctx.decrypt_content()?;
        let response = InstructionResponseBuilder::new(ctx.instruction)
            .name(name)
            .prompt(prompt)
            .build();
        return encrypt_response(&state, &session_id, &response).await;
    }

    // Set this instruction as default (database handles clearing other defaults)
    let updated_instruction = state
        .db
        .set_default_user_instruction(ctx.instruction.id, user.uuid)
        .map_err(error_mapping::map_generic_db_error)?;

    let (name, prompt) = ctx.decrypt_content()?;
    let response = InstructionResponseBuilder::new(updated_instruction)
        .name(name)
        .prompt(prompt)
        .build();

    encrypt_response(&state, &session_id, &response).await
}

// ============================================================================
// Router Configuration
// ============================================================================

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/v1/instructions",
            post(create_instruction).layer(from_fn_with_state(
                state.clone(),
                decrypt_request::<CreateInstructionRequest>,
            )),
        )
        .route(
            "/v1/instructions",
            get(list_instructions).layer(from_fn_with_state(state.clone(), decrypt_request::<()>)),
        )
        .route(
            "/v1/instructions/:id",
            get(get_instruction).layer(from_fn_with_state(state.clone(), decrypt_request::<()>)),
        )
        .route(
            "/v1/instructions/:id",
            post(update_instruction).layer(from_fn_with_state(
                state.clone(),
                decrypt_request::<UpdateInstructionRequest>,
            )),
        )
        .route(
            "/v1/instructions/:id",
            delete(delete_instruction)
                .layer(from_fn_with_state(state.clone(), decrypt_request::<()>)),
        )
        .route(
            "/v1/instructions/:id/set-default",
            post(set_default_instruction)
                .layer(from_fn_with_state(state.clone(), decrypt_request::<()>)),
        )
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::ListInstructionsParams;
    use crate::web::responses::constants::{DEFAULT_PAGINATION_LIMIT, DEFAULT_PAGINATION_ORDER};
    use uuid::Uuid;

    #[test]
    fn list_params_ignore_legacy_before_cursor() {
        let params: ListInstructionsParams =
            serde_json::from_str(&format!(r#"{{"before":"{}"}}"#, Uuid::nil())).unwrap();

        assert_eq!(params.limit, DEFAULT_PAGINATION_LIMIT);
        assert!(params.after.is_none());
        assert_eq!(params.order, DEFAULT_PAGINATION_ORDER);
    }

    #[test]
    fn list_params_support_after_cursor_and_order() {
        let after = Uuid::new_v4();
        let params: ListInstructionsParams = serde_json::from_str(&format!(
            r#"{{"limit":5,"after":"{}","order":"asc"}}"#,
            after
        ))
        .unwrap();

        assert_eq!(params.limit, 5);
        assert_eq!(params.after, Some(after));
        assert_eq!(params.order, "asc");
    }
}
