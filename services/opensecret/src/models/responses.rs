use crate::models::schema::{
    assistant_messages, conversation_projects, conversations, reasoning_items, responses,
    tool_calls, tool_outputs, user_instructions, user_messages,
};
use chrono::{DateTime, Utc};
use diesel::prelude::*;
use diesel::sql_query;
use diesel::sql_types::{Array, BigInt, Nullable};
use diesel_derive_enum::DbEnum;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

// Error types
#[derive(Error, Debug)]
pub enum ResponsesError {
    #[error("Database error: {0}")]
    DatabaseError(#[from] diesel::result::Error),
    #[error("Conversation not found")]
    ConversationNotFound,
    #[error("Conversation project not found")]
    ConversationProjectNotFound,
    #[error("Response not found")]
    ResponseNotFound,
    #[error("User message not found")]
    UserMessageNotFound,
    #[error("System prompt not found")]
    SystemPromptNotFound,
    #[error("Tool call not found")]
    ToolCallNotFound,
    #[error("Tool output not found")]
    ToolOutputNotFound,
    #[error("Assistant message not found")]
    AssistantMessageNotFound,
    #[error("Unauthorized access")]
    Unauthorized,
    #[error("Validation error")]
    ValidationError,
}

pub const MAX_CONVERSATION_PROJECTS_PER_USER: i64 = 10;

pub fn validate_conversation_project_limit(
    existing_project_count: i64,
) -> Result<(), ResponsesError> {
    if existing_project_count >= MAX_CONVERSATION_PROJECTS_PER_USER {
        Err(ResponsesError::ValidationError)
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationProjectFilter {
    Any,
    Assigned(i64),
    Unassigned,
}

// Response status enum matching the database enum
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, DbEnum)]
#[ExistingTypePath = "crate::models::schema::sql_types::ResponseStatus"]
#[serde(rename_all = "snake_case")]
pub enum ResponseStatus {
    Queued,
    InProgress,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone)]
pub enum ProjectInstructionUpdate {
    Unchanged,
    Set {
        prompt_enc: Vec<u8>,
        prompt_tokens: i32,
    },
    Clear,
}

// ============================================================================
// Responses (Job Tracker)
// ============================================================================

#[derive(Queryable, Selectable, Identifiable, Debug, Clone, Serialize, Deserialize)]
#[diesel(table_name = responses)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Response {
    pub id: i64,
    pub uuid: Uuid,
    pub user_id: Uuid,
    pub conversation_id: i64,
    pub status: ResponseStatus,
    pub model: String,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_output_tokens: Option<i32>,
    pub tool_choice: Option<String>,
    pub parallel_tool_calls: bool,
    pub store: bool,
    pub metadata_enc: Option<Vec<u8>>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Insertable, Debug)]
#[diesel(table_name = responses)]
pub struct NewResponse {
    pub uuid: Uuid,
    pub user_id: Uuid,
    pub conversation_id: i64,
    pub status: ResponseStatus,
    pub model: String,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_output_tokens: Option<i32>,
    pub tool_choice: Option<String>,
    pub parallel_tool_calls: bool,
    pub store: bool,
    pub metadata_enc: Option<Vec<u8>>,
}

/// A tool call and its directly linked output inserted as part of a response request.
#[derive(Debug, Clone)]
pub struct PersistedToolCallOutputPair {
    pub tool_call: ToolCall,
    pub tool_output: ToolOutput,
}

/// Records inserted atomically before a Responses API stream starts.
#[derive(Debug, Clone)]
pub struct PersistedResponseWithToolItems {
    pub response: Response,
    pub user_message: UserMessage,
    pub tool_items: Vec<PersistedToolCallOutputPair>,
    /// Latest durable item timestamp, used to continue monotonic response item ordering.
    pub last_item_created_at: DateTime<Utc>,
}

impl Response {
    pub fn get_by_uuid_and_user(
        conn: &mut PgConnection,
        uuid: Uuid,
        user_id: Uuid,
    ) -> Result<Response, ResponsesError> {
        responses::table
            .filter(responses::uuid.eq(uuid))
            .filter(responses::user_id.eq(user_id))
            .first::<Response>(conn)
            .map_err(|e| match e {
                diesel::result::Error::NotFound => ResponsesError::ResponseNotFound,
                _ => ResponsesError::DatabaseError(e),
            })
    }

    pub fn update_status(
        conn: &mut PgConnection,
        id: i64,
        status: ResponseStatus,
        completed_at: Option<DateTime<Utc>>,
    ) -> Result<(), ResponsesError> {
        diesel::update(responses::table.filter(responses::id.eq(id)))
            .set((
                responses::status.eq(status),
                responses::completed_at.eq(completed_at),
                responses::updated_at.eq(diesel::dsl::now),
            ))
            .execute(conn)
            .map(|_| ())
            .map_err(ResponsesError::DatabaseError)
    }

    pub fn update_status_if_current(
        conn: &mut PgConnection,
        id: i64,
        current_status: ResponseStatus,
        new_status: ResponseStatus,
        completed_at: Option<DateTime<Utc>>,
    ) -> Result<usize, ResponsesError> {
        diesel::update(
            responses::table
                .filter(responses::id.eq(id))
                .filter(responses::status.eq(current_status)),
        )
        .set((
            responses::status.eq(new_status),
            responses::completed_at.eq(completed_at),
            responses::updated_at.eq(diesel::dsl::now),
        ))
        .execute(conn)
        .map_err(ResponsesError::DatabaseError)
    }

    pub fn cancel_by_uuid_and_user(
        conn: &mut PgConnection,
        uuid: Uuid,
        user_id: Uuid,
    ) -> Result<Response, ResponsesError> {
        let cancelable_status = responses::status
            .eq(ResponseStatus::InProgress)
            .or(responses::status.eq(ResponseStatus::Queued));

        match diesel::update(
            responses::table
                .filter(responses::uuid.eq(uuid))
                .filter(responses::user_id.eq(user_id))
                .filter(cancelable_status),
        )
        .set((
            responses::status.eq(ResponseStatus::Cancelled),
            responses::completed_at.eq(diesel::dsl::now),
            responses::updated_at.eq(diesel::dsl::now),
        ))
        .get_result(conn)
        {
            Ok(response) => Ok(response),
            Err(diesel::result::Error::NotFound) => {
                Self::get_by_uuid_and_user(conn, uuid, user_id)?;
                Err(ResponsesError::ValidationError)
            }
            Err(e) => Err(ResponsesError::DatabaseError(e)),
        }
    }

    pub fn delete_by_uuid_and_user(
        conn: &mut PgConnection,
        uuid: Uuid,
        user_id: Uuid,
    ) -> Result<(), ResponsesError> {
        diesel::delete(
            responses::table
                .filter(responses::uuid.eq(uuid))
                .filter(responses::user_id.eq(user_id)),
        )
        .execute(conn)
        .map(|rows| {
            if rows == 0 {
                Err(ResponsesError::ResponseNotFound)
            } else {
                Ok(())
            }
        })?
    }
}

impl NewResponse {
    pub fn insert(&self, conn: &mut PgConnection) -> Result<Response, ResponsesError> {
        diesel::insert_into(responses::table)
            .values(self)
            .get_result(conn)
            .map_err(ResponsesError::DatabaseError)
    }

    /// Inserts a response, its user message, and precomputed tool call/output pairs atomically.
    ///
    /// `self.user_id` and `self.conversation_id` are the authoritative ownership values. The
    /// existing conversation is scoped to that user before any insert, and all child ownership
    /// and response-link fields are overwritten from the inserted response. Tool item timestamps
    /// are reassigned monotonically after the database-generated user message timestamp, and each
    /// output's `tool_call_fk` is overwritten with the directly inserted call's database ID.
    pub fn create_with_message_and_tool_items(
        self,
        conn: &mut PgConnection,
        mut user_message: NewUserMessage,
        tool_items: Vec<(NewToolCall, NewToolOutput)>,
    ) -> Result<PersistedResponseWithToolItems, ResponsesError> {
        conn.transaction(|tx| {
            // This method is only for existing conversations. Scope the parent row to the
            // authenticated user before creating any child records.
            Conversation::get_by_id_and_user(tx, self.conversation_id, self.user_id)?;

            let response = self.insert(tx)?;

            user_message.conversation_id = response.conversation_id;
            user_message.response_id = Some(response.id);
            user_message.user_id = response.user_id;
            let user_message = user_message.insert(tx)?;
            let mut last_item_created_at = user_message.created_at;

            let mut persisted_tool_items = Vec::with_capacity(tool_items.len());
            for (mut new_tool_call, mut new_tool_output) in tool_items {
                new_tool_call.created_at = last_item_created_at
                    .checked_add_signed(chrono::Duration::microseconds(1))
                    .ok_or(ResponsesError::ValidationError)?;
                new_tool_call.conversation_id = response.conversation_id;
                new_tool_call.response_id = Some(response.id);
                new_tool_call.user_id = response.user_id;
                let tool_call = new_tool_call.insert(tx)?;

                new_tool_output.created_at = tool_call
                    .created_at
                    .checked_add_signed(chrono::Duration::microseconds(1))
                    .ok_or(ResponsesError::ValidationError)?;
                new_tool_output.conversation_id = response.conversation_id;
                new_tool_output.response_id = Some(response.id);
                new_tool_output.user_id = response.user_id;
                new_tool_output.tool_call_fk = tool_call.id;
                let tool_output = new_tool_output.insert(tx)?;

                last_item_created_at = tool_output.created_at;
                persisted_tool_items.push(PersistedToolCallOutputPair {
                    tool_call,
                    tool_output,
                });
            }

            Ok(PersistedResponseWithToolItems {
                response,
                user_message,
                tool_items: persisted_tool_items,
                last_item_created_at,
            })
        })
    }
}

// ============================================================================
// Conversation Projects
// ============================================================================

#[derive(Queryable, Selectable, Identifiable, Debug, Clone, Serialize, Deserialize)]
#[diesel(table_name = conversation_projects)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ConversationProject {
    pub id: i64,
    pub uuid: Uuid,
    pub user_id: Uuid,
    pub name_enc: Vec<u8>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Insertable, Debug)]
#[diesel(table_name = conversation_projects)]
pub struct NewConversationProject {
    pub uuid: Uuid,
    pub user_id: Uuid,
    pub name_enc: Vec<u8>,
}

impl ConversationProject {
    pub fn get_by_id_and_user(
        conn: &mut PgConnection,
        project_id: i64,
        user_id: Uuid,
    ) -> Result<ConversationProject, ResponsesError> {
        conversation_projects::table
            .filter(conversation_projects::id.eq(project_id))
            .filter(conversation_projects::user_id.eq(user_id))
            .first::<ConversationProject>(conn)
            .map_err(|e| match e {
                diesel::result::Error::NotFound => ResponsesError::ConversationProjectNotFound,
                _ => ResponsesError::DatabaseError(e),
            })
    }

    pub fn get_by_uuid_and_user(
        conn: &mut PgConnection,
        project_uuid: Uuid,
        user_id: Uuid,
    ) -> Result<ConversationProject, ResponsesError> {
        conversation_projects::table
            .filter(conversation_projects::uuid.eq(project_uuid))
            .filter(conversation_projects::user_id.eq(user_id))
            .first::<ConversationProject>(conn)
            .map_err(|e| match e {
                diesel::result::Error::NotFound => ResponsesError::ConversationProjectNotFound,
                _ => ResponsesError::DatabaseError(e),
            })
    }

    pub fn delete_by_id_and_user(
        conn: &mut PgConnection,
        project_id: i64,
        user_id: Uuid,
    ) -> Result<(), ResponsesError> {
        let deleted = diesel::delete(
            conversation_projects::table
                .filter(conversation_projects::id.eq(project_id))
                .filter(conversation_projects::user_id.eq(user_id)),
        )
        .execute(conn)?;

        if deleted == 0 {
            return Err(ResponsesError::ConversationProjectNotFound);
        }

        Ok(())
    }

    pub fn count_for_user(conn: &mut PgConnection, user_id: Uuid) -> Result<i64, ResponsesError> {
        use diesel::dsl::count;

        conversation_projects::table
            .filter(conversation_projects::user_id.eq(user_id))
            .select(count(conversation_projects::id))
            .first(conn)
            .map_err(ResponsesError::DatabaseError)
    }

    pub fn list_for_user(
        conn: &mut PgConnection,
        user_id: Uuid,
        limit: i64,
        after: Option<Uuid>,
        order: &str,
    ) -> Result<Vec<ConversationProject>, ResponsesError> {
        let mut query = conversation_projects::table
            .filter(conversation_projects::user_id.eq(user_id))
            .into_boxed();

        if let Some(after_uuid) = after {
            let cursor_project = conversation_projects::table
                .filter(conversation_projects::uuid.eq(after_uuid))
                .filter(conversation_projects::user_id.eq(user_id))
                .select((conversation_projects::updated_at, conversation_projects::id))
                .first::<(DateTime<Utc>, i64)>(conn)
                .optional()?;

            if let Some((updated_at, id)) = cursor_project {
                if order == "desc" {
                    query = query.filter(
                        conversation_projects::updated_at.lt(updated_at).or(
                            conversation_projects::updated_at
                                .eq(updated_at)
                                .and(conversation_projects::id.lt(id)),
                        ),
                    );
                } else {
                    query = query.filter(
                        conversation_projects::updated_at.gt(updated_at).or(
                            conversation_projects::updated_at
                                .eq(updated_at)
                                .and(conversation_projects::id.gt(id)),
                        ),
                    );
                }
            }
        }

        if order == "desc" {
            query = query.order((
                conversation_projects::updated_at.desc(),
                conversation_projects::id.desc(),
            ));
        } else {
            query = query.order((
                conversation_projects::updated_at.asc(),
                conversation_projects::id.asc(),
            ));
        }

        query
            .limit(limit)
            .load::<ConversationProject>(conn)
            .map_err(ResponsesError::DatabaseError)
    }
}

impl NewConversationProject {
    pub fn insert(&self, conn: &mut PgConnection) -> Result<ConversationProject, ResponsesError> {
        diesel::insert_into(conversation_projects::table)
            .values(self)
            .get_result(conn)
            .map_err(ResponsesError::DatabaseError)
    }
}

// ============================================================================
// User Instructions (System Prompts)
// ============================================================================

#[derive(Queryable, Selectable, Identifiable, Debug, Clone, Serialize, Deserialize)]
#[diesel(table_name = user_instructions)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct UserInstruction {
    pub id: i64,
    pub uuid: Uuid,
    pub user_id: Uuid,
    pub name_enc: Option<Vec<u8>>,
    pub prompt_enc: Vec<u8>,
    pub prompt_tokens: i32,
    pub is_default: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub project_id: Option<i64>,
}

#[derive(Insertable, Debug)]
#[diesel(table_name = user_instructions)]
pub struct NewUserInstruction {
    pub uuid: Uuid,
    pub user_id: Uuid,
    pub project_id: Option<i64>,
    pub name_enc: Option<Vec<u8>>,
    pub prompt_enc: Vec<u8>,
    pub prompt_tokens: i32,
    pub is_default: bool,
}

// ============================================================================
// Conversations (formerly Chat Threads)
// ============================================================================

#[derive(Queryable, Selectable, Identifiable, Debug, Clone, Serialize, Deserialize)]
#[diesel(table_name = conversations)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Conversation {
    pub id: i64,
    pub uuid: Uuid,
    pub user_id: Uuid,
    pub metadata_enc: Option<Vec<u8>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub project_id: Option<i64>,
    pub is_pinned: bool,
    pub last_activity_at: DateTime<Utc>,
}

#[derive(Insertable, Debug)]
#[diesel(table_name = conversations)]
pub struct NewConversation {
    pub uuid: Uuid,
    pub user_id: Uuid,
    pub project_id: Option<i64>,
    pub is_pinned: bool,
    pub metadata_enc: Option<Vec<u8>>,
}

#[derive(AsChangeset, Default)]
#[diesel(table_name = conversations)]
struct ConversationUpdateChanges {
    metadata_enc: Option<Vec<u8>>,
    project_id: Option<Option<i64>>,
    is_pinned: Option<bool>,
}

impl Conversation {
    pub fn get_by_id_and_user(
        conn: &mut PgConnection,
        conversation_id: i64,
        user_id: Uuid,
    ) -> Result<Conversation, ResponsesError> {
        conversations::table
            .filter(conversations::id.eq(conversation_id))
            .filter(conversations::user_id.eq(user_id))
            .first::<Conversation>(conn)
            .map_err(|e| match e {
                diesel::result::Error::NotFound => ResponsesError::ConversationNotFound,
                _ => ResponsesError::DatabaseError(e),
            })
    }

    pub fn get_by_uuid_and_user(
        conn: &mut PgConnection,
        conversation_uuid: Uuid,
        user_id: Uuid,
    ) -> Result<Conversation, ResponsesError> {
        conversations::table
            .filter(conversations::uuid.eq(conversation_uuid))
            .filter(conversations::user_id.eq(user_id))
            .first::<Conversation>(conn)
            .map_err(|e| match e {
                diesel::result::Error::NotFound => ResponsesError::ConversationNotFound,
                _ => ResponsesError::DatabaseError(e),
            })
    }

    pub fn update_metadata(
        conn: &mut PgConnection,
        conversation_id: i64,
        user_id: Uuid,
        metadata_enc: Vec<u8>,
    ) -> Result<Conversation, ResponsesError> {
        Self::update(
            conn,
            conversation_id,
            user_id,
            Some(metadata_enc),
            None,
            None,
        )
    }

    pub fn update(
        conn: &mut PgConnection,
        conversation_id: i64,
        user_id: Uuid,
        metadata_enc: Option<Vec<u8>>,
        project_id: Option<Option<i64>>,
        is_pinned: Option<bool>,
    ) -> Result<Conversation, ResponsesError> {
        let changes = ConversationUpdateChanges {
            metadata_enc,
            project_id,
            is_pinned,
        };

        if changes.metadata_enc.is_none()
            && changes.project_id.is_none()
            && changes.is_pinned.is_none()
        {
            return Self::get_by_id_and_user(conn, conversation_id, user_id);
        }

        let updated = diesel::update(
            conversations::table
                .filter(conversations::id.eq(conversation_id))
                .filter(conversations::user_id.eq(user_id)),
        )
        .set(changes)
        .get_result::<Conversation>(conn)
        .optional()?;

        updated.ok_or(ResponsesError::ConversationNotFound)
    }

    pub fn batch_update_project(
        conn: &mut PgConnection,
        conversation_uuids: &[Uuid],
        user_id: Uuid,
        target_project_id: Option<i64>,
    ) -> Result<(), ResponsesError> {
        use diesel::Connection;

        if conversation_uuids.is_empty() {
            return Err(ResponsesError::ValidationError);
        }

        let unique_ids = conversation_uuids
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        if unique_ids.len() != conversation_uuids.len() {
            return Err(ResponsesError::ValidationError);
        }

        conn.transaction(|tx| {
            if let Some(target_project_id) = target_project_id {
                let target_project = conversation_projects::table
                    .filter(conversation_projects::id.eq(target_project_id))
                    .filter(conversation_projects::user_id.eq(user_id))
                    .select(conversation_projects::id)
                    .for_update()
                    .first::<i64>(tx)
                    .optional()?;

                if target_project.is_none() {
                    return Err(ResponsesError::ConversationProjectNotFound);
                }
            }

            let existing_conversations = conversations::table
                .filter(conversations::user_id.eq(user_id))
                .filter(conversations::uuid.eq_any(conversation_uuids))
                .for_update()
                .load::<Conversation>(tx)?;

            if existing_conversations.len() != conversation_uuids.len() {
                return Err(ResponsesError::ConversationNotFound);
            }

            let source_project_id = existing_conversations
                .first()
                .map(|conversation| conversation.project_id)
                .ok_or(ResponsesError::ValidationError)?;

            if existing_conversations
                .iter()
                .any(|conversation| conversation.project_id != source_project_id)
            {
                return Err(ResponsesError::ValidationError);
            }

            let updated = diesel::update(
                conversations::table
                    .filter(conversations::user_id.eq(user_id))
                    .filter(conversations::uuid.eq_any(conversation_uuids)),
            )
            .set(conversations::project_id.eq(target_project_id))
            .execute(tx)?;

            if updated != conversation_uuids.len() {
                return Err(ResponsesError::ConversationNotFound);
            }

            Ok(())
        })
    }

    pub fn delete_by_id_and_user(
        conn: &mut PgConnection,
        conversation_id: i64,
        user_id: Uuid,
    ) -> Result<(), ResponsesError> {
        let deleted = diesel::delete(
            conversations::table
                .filter(conversations::id.eq(conversation_id))
                .filter(conversations::user_id.eq(user_id)),
        )
        .execute(conn)?;

        if deleted == 0 {
            return Err(ResponsesError::ConversationNotFound);
        }

        Ok(())
    }

    pub fn delete_all_for_user(
        conn: &mut PgConnection,
        user_id: Uuid,
    ) -> Result<(), ResponsesError> {
        diesel::delete(conversations::table.filter(conversations::user_id.eq(user_id)))
            .execute(conn)
            .map_err(ResponsesError::DatabaseError)?;

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn list_for_user(
        conn: &mut PgConnection,
        user_id: Uuid,
        limit: i64,
        after: Option<Uuid>,
        order: &str,
        project_filter: ConversationProjectFilter,
        pinned: Option<bool>,
    ) -> Result<Vec<Conversation>, ResponsesError> {
        let mut query = conversations::table
            .filter(conversations::user_id.eq(user_id))
            .into_boxed();

        match project_filter {
            ConversationProjectFilter::Any => {}
            ConversationProjectFilter::Assigned(project_id) => {
                query = query.filter(conversations::project_id.eq(Some(project_id)));
            }
            ConversationProjectFilter::Unassigned => {
                query = query.filter(conversations::project_id.is_null());
            }
        }

        if let Some(is_pinned) = pinned {
            query = query.filter(conversations::is_pinned.eq(is_pinned));
        }

        // If we have an after cursor, we need to find its timestamp and apply cursor-based pagination
        if let Some(after_uuid) = after {
            // Get the cursor conversation to find its last_activity_at and id
            let cursor_conv = conversations::table
                .filter(conversations::uuid.eq(after_uuid))
                .filter(conversations::user_id.eq(user_id))
                .select((conversations::last_activity_at, conversations::id))
                .first::<(DateTime<Utc>, i64)>(conn)
                .optional()?;

            if let Some((last_activity_at, id)) = cursor_conv {
                if order == "desc" {
                    // For desc order, get items with last_activity_at < cursor OR (last_activity_at = cursor AND id < cursor_id)
                    query = query.filter(
                        conversations::last_activity_at.lt(last_activity_at).or(
                            conversations::last_activity_at
                                .eq(last_activity_at)
                                .and(conversations::id.lt(id)),
                        ),
                    );
                } else {
                    // For asc order, get items with last_activity_at > cursor OR (last_activity_at = cursor AND id > cursor_id)
                    query = query.filter(
                        conversations::last_activity_at.gt(last_activity_at).or(
                            conversations::last_activity_at
                                .eq(last_activity_at)
                                .and(conversations::id.gt(id)),
                        ),
                    );
                }
            }
        }

        // Apply ordering
        if order == "desc" {
            query = query.order((
                conversations::last_activity_at.desc(),
                conversations::id.desc(),
            ));
        } else {
            query = query.order((
                conversations::last_activity_at.asc(),
                conversations::id.asc(),
            ));
        }

        query
            .limit(limit)
            .load::<Conversation>(conn)
            .map_err(ResponsesError::DatabaseError)
    }
}

impl NewConversation {
    pub fn insert(&self, conn: &mut PgConnection) -> Result<Conversation, ResponsesError> {
        diesel::insert_into(conversations::table)
            .values(self)
            .get_result(conn)
            .map_err(ResponsesError::DatabaseError)
    }

    /// Creates a new conversation with optional response, first message, and placeholder assistant message in a transaction
    #[allow(clippy::too_many_arguments)]
    pub fn create_with_response_and_message(
        conn: &mut PgConnection,
        conversation_uuid: Uuid,
        user_id: Uuid,
        metadata_enc: Option<Vec<u8>>,
        response: Option<NewResponse>,
        first_message_content: Vec<u8>,
        first_message_tokens: i32,
        message_uuid: Uuid,
        assistant_message_uuid: Option<Uuid>,
    ) -> Result<(Conversation, Option<Response>, UserMessage), ResponsesError> {
        use diesel::Connection;

        conn.transaction(|tx| {
            // Create the conversation
            let new_conversation = NewConversation {
                uuid: conversation_uuid,
                user_id,
                project_id: None,
                is_pinned: false,
                metadata_enc,
            };
            let conversation = new_conversation.insert(tx)?;

            // Create the response if provided
            let response_result = if let Some(mut new_response) = response {
                new_response.conversation_id = conversation.id;
                Some(new_response.insert(tx)?)
            } else {
                None
            };

            // Create the first message with specified UUID
            let new_message = NewUserMessage {
                uuid: message_uuid,
                conversation_id: conversation.id,
                response_id: response_result.as_ref().map(|r| r.id),
                user_id,
                content_enc: first_message_content,
                prompt_tokens: first_message_tokens,
            };
            let user_message = new_message.insert(tx)?;

            // Create placeholder assistant message if UUID provided (for Responses API streaming)
            if let Some(assistant_uuid) = assistant_message_uuid {
                let placeholder_assistant = NewAssistantMessage {
                    uuid: assistant_uuid,
                    conversation_id: conversation.id,
                    response_id: response_result.as_ref().map(|r| r.id),
                    user_id,
                    content_enc: None,
                    completion_tokens: 0,
                    status: "in_progress".to_string(),
                    finish_reason: None,
                    created_at: Utc::now(),
                };
                placeholder_assistant.insert(tx)?;
            }

            Ok((conversation, response_result, user_message))
        })
    }
}

// ============================================================================
// User Messages
// ============================================================================

#[derive(Queryable, Selectable, Identifiable, Debug, Clone, Serialize, Deserialize)]
#[diesel(table_name = user_messages)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct UserMessage {
    pub id: i64,
    pub uuid: Uuid,
    pub conversation_id: i64,
    pub response_id: Option<i64>,
    pub user_id: Uuid,
    pub content_enc: Vec<u8>,
    pub prompt_tokens: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub attachment_text_enc: Option<Vec<u8>>,
    pub assistant_reaction: Option<String>,
}

#[derive(Insertable, Debug)]
#[diesel(table_name = user_messages)]
pub struct NewUserMessage {
    pub uuid: Uuid,
    pub conversation_id: i64,
    pub response_id: Option<i64>,
    pub user_id: Uuid,
    pub content_enc: Vec<u8>,
    pub prompt_tokens: i32,
}

impl UserMessage {
    pub fn uuid_exists(conn: &mut PgConnection, uuid: Uuid) -> Result<bool, ResponsesError> {
        diesel::select(diesel::dsl::exists(
            user_messages::table.filter(user_messages::uuid.eq(uuid)),
        ))
        .get_result(conn)
        .map_err(ResponsesError::DatabaseError)
    }

    pub fn get_by_id_and_user(
        conn: &mut PgConnection,
        id: i64,
        user_id: Uuid,
    ) -> Result<UserMessage, ResponsesError> {
        user_messages::table
            .filter(user_messages::id.eq(id))
            .filter(user_messages::user_id.eq(user_id))
            .first::<UserMessage>(conn)
            .map_err(|e| match e {
                diesel::result::Error::NotFound => ResponsesError::UserMessageNotFound,
                _ => ResponsesError::DatabaseError(e),
            })
    }

    pub fn get_by_uuid_and_user(
        conn: &mut PgConnection,
        uuid: Uuid,
        user_id: Uuid,
    ) -> Result<UserMessage, ResponsesError> {
        user_messages::table
            .filter(user_messages::uuid.eq(uuid))
            .filter(user_messages::user_id.eq(user_id))
            .first::<UserMessage>(conn)
            .map_err(|e| match e {
                diesel::result::Error::NotFound => ResponsesError::UserMessageNotFound,
                _ => ResponsesError::DatabaseError(e),
            })
    }

    // Note: status is now tracked on Response, not UserMessage

    pub fn delete_by_id_and_user(
        conn: &mut PgConnection,
        id: i64,
        user_id: Uuid,
    ) -> Result<(), ResponsesError> {
        diesel::delete(
            user_messages::table
                .filter(user_messages::id.eq(id))
                .filter(user_messages::user_id.eq(user_id)),
        )
        .execute(conn)
        .map(|rows| {
            if rows == 0 {
                Err(ResponsesError::UserMessageNotFound)
            } else {
                Ok(())
            }
        })?
    }

    // Note: cancellation is now handled at the Response level
}

impl NewUserMessage {
    pub fn insert(&self, conn: &mut PgConnection) -> Result<UserMessage, ResponsesError> {
        diesel::insert_into(user_messages::table)
            .values(self)
            .get_result(conn)
            .map_err(ResponsesError::DatabaseError)
    }
}

// ============================================================================
// Tool Calls
// ============================================================================

#[derive(Queryable, Selectable, Identifiable, Debug, Clone, Serialize, Deserialize)]
#[diesel(table_name = tool_calls)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ToolCall {
    pub id: i64,
    pub uuid: Uuid,
    pub conversation_id: i64,
    pub response_id: Option<i64>,
    pub user_id: Uuid,
    pub name: String,
    pub arguments_enc: Option<Vec<u8>>,
    pub argument_tokens: i32,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Insertable, Debug)]
#[diesel(table_name = tool_calls)]
pub struct NewToolCall {
    pub uuid: Uuid,
    pub conversation_id: i64,
    pub response_id: Option<i64>,
    pub user_id: Uuid,
    pub name: String,
    pub arguments_enc: Option<Vec<u8>>,
    pub argument_tokens: i32,
    pub status: String,
    pub created_at: DateTime<Utc>,
}

impl ToolCall {
    pub fn get_by_uuid(
        conn: &mut PgConnection,
        uuid: Uuid,
        user_id: Uuid,
    ) -> Result<ToolCall, ResponsesError> {
        tool_calls::table
            .filter(tool_calls::uuid.eq(uuid))
            .filter(tool_calls::user_id.eq(user_id))
            .first::<ToolCall>(conn)
            .map_err(|e| match e {
                diesel::result::Error::NotFound => ResponsesError::ToolCallNotFound,
                _ => ResponsesError::DatabaseError(e),
            })
    }
}

impl NewToolCall {
    pub fn insert(&self, conn: &mut PgConnection) -> Result<ToolCall, ResponsesError> {
        diesel::insert_into(tool_calls::table)
            .values(self)
            .get_result(conn)
            .map_err(ResponsesError::DatabaseError)
    }
}

// ============================================================================
// Tool Outputs
// ============================================================================

#[derive(Queryable, Selectable, Identifiable, Debug, Clone, Serialize, Deserialize)]
#[diesel(table_name = tool_outputs)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ToolOutput {
    pub id: i64,
    pub uuid: Uuid,
    pub conversation_id: i64,
    pub response_id: Option<i64>,
    pub user_id: Uuid,
    pub tool_call_fk: i64,
    pub output_enc: Vec<u8>,
    pub output_tokens: i32,
    pub status: String,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Insertable, Debug)]
#[diesel(table_name = tool_outputs)]
pub struct NewToolOutput {
    pub uuid: Uuid,
    pub conversation_id: i64,
    pub response_id: Option<i64>,
    pub user_id: Uuid,
    pub tool_call_fk: i64,
    pub output_enc: Vec<u8>,
    pub output_tokens: i32,
    pub status: String,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl ToolOutput {
    pub fn get_by_uuid(
        conn: &mut PgConnection,
        uuid: Uuid,
        user_id: Uuid,
    ) -> Result<ToolOutput, ResponsesError> {
        tool_outputs::table
            .filter(tool_outputs::uuid.eq(uuid))
            .filter(tool_outputs::user_id.eq(user_id))
            .first::<ToolOutput>(conn)
            .map_err(|e| match e {
                diesel::result::Error::NotFound => ResponsesError::ToolOutputNotFound,
                _ => ResponsesError::DatabaseError(e),
            })
    }
}

impl NewToolOutput {
    pub fn insert(&self, conn: &mut PgConnection) -> Result<ToolOutput, ResponsesError> {
        diesel::insert_into(tool_outputs::table)
            .values(self)
            .get_result(conn)
            .map_err(ResponsesError::DatabaseError)
    }
}

// ============================================================================
// Assistant Messages
// ============================================================================

#[derive(Queryable, Selectable, Identifiable, Debug, Clone, Serialize, Deserialize)]
#[diesel(table_name = assistant_messages)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct AssistantMessage {
    pub id: i64,
    pub uuid: Uuid,
    pub conversation_id: i64,
    pub response_id: Option<i64>,
    pub user_id: Uuid,
    pub content_enc: Option<Vec<u8>>,
    pub completion_tokens: i32,
    pub status: String,
    pub finish_reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub user_reaction: Option<String>,
}

#[derive(Insertable, Debug)]
#[diesel(table_name = assistant_messages)]
pub struct NewAssistantMessage {
    pub uuid: Uuid,
    pub conversation_id: i64,
    pub response_id: Option<i64>,
    pub user_id: Uuid,
    pub content_enc: Option<Vec<u8>>,
    pub completion_tokens: i32,
    pub status: String,
    pub finish_reason: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl AssistantMessage {
    pub fn update(
        conn: &mut PgConnection,
        message_uuid: Uuid,
        content_enc: Option<Vec<u8>>,
        completion_tokens: i32,
        status: String,
        finish_reason: Option<String>,
    ) -> Result<AssistantMessage, ResponsesError> {
        diesel::update(assistant_messages::table.filter(assistant_messages::uuid.eq(message_uuid)))
            .set((
                assistant_messages::content_enc.eq(content_enc),
                assistant_messages::completion_tokens.eq(completion_tokens),
                assistant_messages::status.eq(status),
                assistant_messages::finish_reason.eq(finish_reason),
                assistant_messages::updated_at.eq(diesel::dsl::now),
            ))
            .get_result(conn)
            .map_err(ResponsesError::DatabaseError)
    }
}

impl NewAssistantMessage {
    pub fn insert(&self, conn: &mut PgConnection) -> Result<AssistantMessage, ResponsesError> {
        diesel::insert_into(assistant_messages::table)
            .values(self)
            .get_result(conn)
            .map_err(ResponsesError::DatabaseError)
    }
}

// ============================================================================
// Reasoning Items
// ============================================================================

#[derive(Queryable, Selectable, Identifiable, Debug, Clone, Serialize, Deserialize)]
#[diesel(table_name = reasoning_items)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ReasoningItem {
    pub id: i64,
    pub uuid: Uuid,
    pub conversation_id: i64,
    pub response_id: Option<i64>,
    pub assistant_message_id: Option<i64>,
    pub user_id: Uuid,
    pub content_enc: Option<Vec<u8>>,
    pub summary_enc: Option<Vec<u8>>,
    pub reasoning_tokens: i32,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Insertable, Debug)]
#[diesel(table_name = reasoning_items)]
pub struct NewReasoningItem {
    pub uuid: Uuid,
    pub conversation_id: i64,
    pub response_id: Option<i64>,
    pub assistant_message_id: Option<i64>,
    pub user_id: Uuid,
    pub content_enc: Option<Vec<u8>>,
    pub summary_enc: Option<Vec<u8>>,
    pub reasoning_tokens: i32,
    pub status: String,
    pub created_at: DateTime<Utc>,
}

impl ReasoningItem {
    pub fn get_by_uuid(
        conn: &mut PgConnection,
        item_uuid: Uuid,
    ) -> Result<Option<ReasoningItem>, ResponsesError> {
        reasoning_items::table
            .filter(reasoning_items::uuid.eq(item_uuid))
            .first::<ReasoningItem>(conn)
            .optional()
            .map_err(ResponsesError::DatabaseError)
    }

    pub fn update(
        conn: &mut PgConnection,
        item_uuid: Uuid,
        content_enc: Option<Vec<u8>>,
        reasoning_tokens: i32,
        status: String,
    ) -> Result<ReasoningItem, ResponsesError> {
        diesel::update(reasoning_items::table.filter(reasoning_items::uuid.eq(item_uuid)))
            .set((
                reasoning_items::content_enc.eq(content_enc),
                reasoning_items::reasoning_tokens.eq(reasoning_tokens),
                reasoning_items::status.eq(status),
                reasoning_items::updated_at.eq(diesel::dsl::now),
            ))
            .get_result(conn)
            .map_err(ResponsesError::DatabaseError)
    }
}

impl NewReasoningItem {
    pub fn insert(&self, conn: &mut PgConnection) -> Result<ReasoningItem, ResponsesError> {
        diesel::insert_into(reasoning_items::table)
            .values(self)
            .get_result(conn)
            .map_err(ResponsesError::DatabaseError)
    }
}

// ============================================================================
// Helper structs for queries
// ============================================================================

// For the UNION ALL query to get all thread messages
#[derive(QueryableByName, Debug, Clone)]
pub struct RawThreadMessage {
    #[diesel(sql_type = diesel::sql_types::Text)]
    pub message_type: String,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    pub id: i64,
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    pub uuid: Uuid,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Uuid>)]
    pub response_uuid: Option<Uuid>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Bytea>)]
    pub content_enc: Option<Vec<u8>>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    pub status: Option<String>,
    #[diesel(sql_type = diesel::sql_types::Timestamptz)]
    pub created_at: DateTime<Utc>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    pub model: Option<String>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Integer>)]
    pub token_count: Option<i32>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Uuid>)]
    pub tool_call_id: Option<Uuid>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    pub finish_reason: Option<String>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    pub tool_name: Option<String>,
}

impl RawThreadMessage {
    pub fn get_conversation_context(
        conn: &mut PgConnection,
        conversation_id: i64,
        limit: i64,
        after: Option<Uuid>,
        order: &str,
    ) -> Result<Vec<RawThreadMessage>, ResponsesError> {
        // First, if we have an 'after' cursor, we need to find its created_at and id
        // We'll use a subquery to handle this
        let order_clause = if order == "desc" {
            "ORDER BY created_at DESC, id DESC"
        } else {
            "ORDER BY created_at ASC, id ASC"
        };

        let query = if after.is_some() {
            format!(
                r#"
                WITH conversation_messages AS (
                    -- User messages
                    SELECT
                        'user' as message_type,
                        um.id,
                        um.uuid,
                        r.uuid as response_uuid,
                        um.content_enc,
                        'completed'::text as status,
                        um.created_at,
                        r.model,
                        um.prompt_tokens as token_count,
                        NULL::uuid as tool_call_id,
                        NULL::text as finish_reason,
                        NULL::text as tool_name
                    FROM user_messages um
                    LEFT JOIN responses r ON um.response_id = r.id
                    WHERE um.conversation_id = $1

                    UNION ALL

                    -- Assistant messages
                    SELECT
                        'assistant' as message_type,
                        am.id,
                        am.uuid,
                        r.uuid as response_uuid,
                        am.content_enc,
                        am.status,
                        am.created_at,
                        r.model,
                        am.completion_tokens as token_count,
                        NULL::uuid as tool_call_id,
                        am.finish_reason,
                        NULL::text as tool_name
                    FROM assistant_messages am
                    LEFT JOIN responses r ON am.response_id = r.id
                    WHERE am.conversation_id = $1

                    UNION ALL

                    -- Tool calls
                    SELECT
                        'tool_call' as message_type,
                        tc.id,
                        tc.uuid,
                        r.uuid as response_uuid,
                        tc.arguments_enc as content_enc,
                        'completed'::text as status,
                        tc.created_at,
                        NULL::text as model,
                        tc.argument_tokens as token_count,
                        tc.uuid as tool_call_id,
                        NULL::text as finish_reason,
                        tc.name as tool_name
                    FROM tool_calls tc
                    LEFT JOIN responses r ON tc.response_id = r.id
                    WHERE tc.conversation_id = $1

                    UNION ALL

                    -- Tool outputs
                    SELECT
                        'tool_output' as message_type,
                        tto.id,
                        tto.uuid,
                        r.uuid as response_uuid,
                        tto.output_enc as content_enc,
                        'completed'::text as status,
                        tto.created_at,
                        NULL::text as model,
                        tto.output_tokens as token_count,
                        tc.uuid as tool_call_id,
                        NULL::text as finish_reason,
                        tc.name as tool_name
                    FROM tool_outputs tto
                    JOIN tool_calls tc ON tto.tool_call_fk = tc.id
                    LEFT JOIN responses r ON tto.response_id = r.id
                    WHERE tto.conversation_id = $1

                    UNION ALL

                    -- Reasoning items (for conversation items API, skipped in context builder)
                    SELECT
                        'reasoning' as message_type,
                        ri.id,
                        ri.uuid,
                        r.uuid as response_uuid,
                        ri.content_enc,
                        ri.status,
                        ri.created_at,
                        NULL::text as model,
                        ri.reasoning_tokens as token_count,
                        NULL::uuid as tool_call_id,
                        NULL::text as finish_reason,
                        NULL::text as tool_name
                    FROM reasoning_items ri
                    LEFT JOIN responses r ON ri.response_id = r.id
                    WHERE ri.conversation_id = $1
                ),
                cursor_message AS (
                    SELECT created_at, id
                    FROM conversation_messages
                    WHERE uuid = $2
                )
                SELECT cm.*
                FROM conversation_messages cm, cursor_message
                WHERE {}
                {}
                LIMIT $3
                "#,
                if order == "desc" {
                    "(cm.created_at < cursor_message.created_at) OR (cm.created_at = cursor_message.created_at AND cm.id < cursor_message.id)"
                } else {
                    "(cm.created_at > cursor_message.created_at) OR (cm.created_at = cursor_message.created_at AND cm.id > cursor_message.id)"
                },
                order_clause
            )
        } else {
            format!(
                r#"
                WITH conversation_messages AS (
                    -- User messages
                    SELECT
                        'user' as message_type,
                        um.id,
                        um.uuid,
                        r.uuid as response_uuid,
                        um.content_enc,
                        'completed'::text as status,
                        um.created_at,
                        r.model,
                        um.prompt_tokens as token_count,
                        NULL::uuid as tool_call_id,
                        NULL::text as finish_reason,
                        NULL::text as tool_name
                    FROM user_messages um
                    LEFT JOIN responses r ON um.response_id = r.id
                    WHERE um.conversation_id = $1

                    UNION ALL

                    -- Assistant messages
                    SELECT
                        'assistant' as message_type,
                        am.id,
                        am.uuid,
                        r.uuid as response_uuid,
                        am.content_enc,
                        am.status,
                        am.created_at,
                        r.model,
                        am.completion_tokens as token_count,
                        NULL::uuid as tool_call_id,
                        am.finish_reason,
                        NULL::text as tool_name
                    FROM assistant_messages am
                    LEFT JOIN responses r ON am.response_id = r.id
                    WHERE am.conversation_id = $1

                    UNION ALL

                    -- Tool calls
                    SELECT
                        'tool_call' as message_type,
                        tc.id,
                        tc.uuid,
                        r.uuid as response_uuid,
                        tc.arguments_enc as content_enc,
                        'completed'::text as status,
                        tc.created_at,
                        NULL::text as model,
                        tc.argument_tokens as token_count,
                        tc.uuid as tool_call_id,
                        NULL::text as finish_reason,
                        tc.name as tool_name
                    FROM tool_calls tc
                    LEFT JOIN responses r ON tc.response_id = r.id
                    WHERE tc.conversation_id = $1

                    UNION ALL

                    -- Tool outputs
                    SELECT
                        'tool_output' as message_type,
                        tto.id,
                        tto.uuid,
                        r.uuid as response_uuid,
                        tto.output_enc as content_enc,
                        'completed'::text as status,
                        tto.created_at,
                        NULL::text as model,
                        tto.output_tokens as token_count,
                        tc.uuid as tool_call_id,
                        NULL::text as finish_reason,
                        tc.name as tool_name
                    FROM tool_outputs tto
                    JOIN tool_calls tc ON tto.tool_call_fk = tc.id
                    LEFT JOIN responses r ON tto.response_id = r.id
                    WHERE tto.conversation_id = $1

                    UNION ALL

                    -- Reasoning items (for conversation items API, skipped in context builder)
                    SELECT
                        'reasoning' as message_type,
                        ri.id,
                        ri.uuid,
                        r.uuid as response_uuid,
                        ri.content_enc,
                        ri.status,
                        ri.created_at,
                        NULL::text as model,
                        ri.reasoning_tokens as token_count,
                        NULL::uuid as tool_call_id,
                        NULL::text as finish_reason,
                        NULL::text as tool_name
                    FROM reasoning_items ri
                    LEFT JOIN responses r ON ri.response_id = r.id
                    WHERE ri.conversation_id = $1
                )
                SELECT *
                FROM conversation_messages
                {}
                LIMIT $2
                "#,
                order_clause
            )
        };

        if let Some(after_uuid) = after {
            sql_query(query)
                .bind::<BigInt, _>(conversation_id)
                .bind::<diesel::sql_types::Uuid, _>(after_uuid)
                .bind::<BigInt, _>(limit)
                .load::<RawThreadMessage>(conn)
                .map_err(ResponsesError::DatabaseError)
        } else {
            sql_query(query)
                .bind::<BigInt, _>(conversation_id)
                .bind::<BigInt, _>(limit)
                .load::<RawThreadMessage>(conn)
                .map_err(ResponsesError::DatabaseError)
        }
    }

    /// Fetch one conversation item by UUID.
    ///
    /// Every branch filters by item UUID (unique per table) and conversation, so
    /// a lookup reads at most one row per item table regardless of history size.
    pub fn get_conversation_item(
        conn: &mut PgConnection,
        conversation_id: i64,
        item_uuid: Uuid,
    ) -> Result<Option<RawThreadMessage>, ResponsesError> {
        let query = r#"
            SELECT * FROM (
                -- User messages
                SELECT
                    'user' as message_type,
                    um.id,
                    um.uuid,
                    r.uuid as response_uuid,
                    um.content_enc,
                    'completed'::text as status,
                    um.created_at,
                    r.model,
                    um.prompt_tokens as token_count,
                    NULL::uuid as tool_call_id,
                    NULL::text as finish_reason,
                    NULL::text as tool_name
                FROM user_messages um
                LEFT JOIN responses r ON um.response_id = r.id
                WHERE um.uuid = $2 AND um.conversation_id = $1

                UNION ALL

                -- Assistant messages
                SELECT
                    'assistant' as message_type,
                    am.id,
                    am.uuid,
                    r.uuid as response_uuid,
                    am.content_enc,
                    am.status,
                    am.created_at,
                    r.model,
                    am.completion_tokens as token_count,
                    NULL::uuid as tool_call_id,
                    am.finish_reason,
                    NULL::text as tool_name
                FROM assistant_messages am
                LEFT JOIN responses r ON am.response_id = r.id
                WHERE am.uuid = $2 AND am.conversation_id = $1

                UNION ALL

                -- Tool calls
                SELECT
                    'tool_call' as message_type,
                    tc.id,
                    tc.uuid,
                    r.uuid as response_uuid,
                    tc.arguments_enc as content_enc,
                    'completed'::text as status,
                    tc.created_at,
                    NULL::text as model,
                    tc.argument_tokens as token_count,
                    tc.uuid as tool_call_id,
                    NULL::text as finish_reason,
                    tc.name as tool_name
                FROM tool_calls tc
                LEFT JOIN responses r ON tc.response_id = r.id
                WHERE tc.uuid = $2 AND tc.conversation_id = $1

                UNION ALL

                -- Tool outputs
                SELECT
                    'tool_output' as message_type,
                    tto.id,
                    tto.uuid,
                    r.uuid as response_uuid,
                    tto.output_enc as content_enc,
                    'completed'::text as status,
                    tto.created_at,
                    NULL::text as model,
                    tto.output_tokens as token_count,
                    tc.uuid as tool_call_id,
                    NULL::text as finish_reason,
                    tc.name as tool_name
                FROM tool_outputs tto
                JOIN tool_calls tc ON tto.tool_call_fk = tc.id
                LEFT JOIN responses r ON tto.response_id = r.id
                WHERE tto.uuid = $2 AND tto.conversation_id = $1

                UNION ALL

                -- Reasoning items
                SELECT
                    'reasoning' as message_type,
                    ri.id,
                    ri.uuid,
                    r.uuid as response_uuid,
                    ri.content_enc,
                    ri.status,
                    ri.created_at,
                    NULL::text as model,
                    ri.reasoning_tokens as token_count,
                    NULL::uuid as tool_call_id,
                    NULL::text as finish_reason,
                    NULL::text as tool_name
                FROM reasoning_items ri
                LEFT JOIN responses r ON ri.response_id = r.id
                WHERE ri.uuid = $2 AND ri.conversation_id = $1
            ) AS conversation_item
            ORDER BY created_at ASC, id ASC
            LIMIT 1
        "#;

        sql_query(query)
            .bind::<BigInt, _>(conversation_id)
            .bind::<diesel::sql_types::Uuid, _>(item_uuid)
            .get_result::<RawThreadMessage>(conn)
            .optional()
            .map_err(ResponsesError::DatabaseError)
    }

    pub fn get_response_context(
        conn: &mut PgConnection,
        response_id: i64,
    ) -> Result<Vec<RawThreadMessage>, ResponsesError> {
        let query = r#"
            WITH response_messages AS (
                -- User messages
                SELECT
                    'user' as message_type,
                    um.id,
                    um.uuid,
                    r.uuid as response_uuid,
                    um.content_enc,
                    'completed'::text as status,
                    um.created_at,
                    r.model,
                    um.prompt_tokens as token_count,
                    NULL::uuid as tool_call_id,
                    NULL::text as finish_reason,
                    NULL::text as tool_name
                FROM user_messages um
                LEFT JOIN responses r ON um.response_id = r.id
                WHERE um.response_id = $1

                UNION ALL

                -- Assistant messages
                SELECT
                    'assistant' as message_type,
                    am.id,
                    am.uuid,
                    r.uuid as response_uuid,
                    am.content_enc,
                    am.status,
                    am.created_at,
                    r.model,
                    am.completion_tokens as token_count,
                    NULL::uuid as tool_call_id,
                    am.finish_reason,
                    NULL::text as tool_name
                FROM assistant_messages am
                LEFT JOIN responses r ON am.response_id = r.id
                WHERE am.response_id = $1

                UNION ALL

                -- Tool calls
                SELECT
                    'tool_call' as message_type,
                    tc.id,
                    tc.uuid,
                    r.uuid as response_uuid,
                    tc.arguments_enc as content_enc,
                    'completed'::text as status,
                    tc.created_at,
                    NULL::text as model,
                    tc.argument_tokens as token_count,
                    tc.uuid as tool_call_id,
                    NULL::text as finish_reason,
                    tc.name as tool_name
                FROM tool_calls tc
                LEFT JOIN responses r ON tc.response_id = r.id
                WHERE tc.response_id = $1

                UNION ALL

                -- Tool outputs
                SELECT
                    'tool_output' as message_type,
                    tto.id,
                    tto.uuid,
                    r.uuid as response_uuid,
                    tto.output_enc as content_enc,
                    'completed'::text as status,
                    tto.created_at,
                    NULL::text as model,
                    tto.output_tokens as token_count,
                    tc.uuid as tool_call_id,
                    NULL::text as finish_reason,
                    tc.name as tool_name
                FROM tool_outputs tto
                JOIN tool_calls tc ON tto.tool_call_fk = tc.id
                LEFT JOIN responses r ON tto.response_id = r.id
                WHERE tto.response_id = $1

                UNION ALL

                -- Reasoning items (for conversation items API, skipped in context builder)
                SELECT
                    'reasoning' as message_type,
                    ri.id,
                    ri.uuid,
                    r.uuid as response_uuid,
                    ri.content_enc,
                    ri.status,
                    ri.created_at,
                    NULL::text as model,
                    ri.reasoning_tokens as token_count,
                    NULL::uuid as tool_call_id,
                    NULL::text as finish_reason,
                    NULL::text as tool_name
                FROM reasoning_items ri
                LEFT JOIN responses r ON ri.response_id = r.id
                WHERE ri.response_id = $1
            )
            SELECT * FROM response_messages
            ORDER BY created_at ASC
        "#;

        sql_query(query)
            .bind::<BigInt, _>(response_id)
            .load::<RawThreadMessage>(conn)
            .map_err(ResponsesError::DatabaseError)
    }

    /// Fetch messages by specific IDs (for targeted retrieval after metadata-based truncation)
    pub fn get_messages_by_ids(
        conn: &mut PgConnection,
        conversation_id: i64,
        message_ids: &[(String, i64)], // (message_type, id) pairs
    ) -> Result<Vec<RawThreadMessage>, ResponsesError> {
        if message_ids.is_empty() {
            return Ok(Vec::new());
        }

        // Separate IDs by message type
        let user_ids: Vec<i64> = message_ids
            .iter()
            .filter(|(t, _)| t == "user")
            .map(|(_, id)| *id)
            .collect();
        let assistant_ids: Vec<i64> = message_ids
            .iter()
            .filter(|(t, _)| t == "assistant")
            .map(|(_, id)| *id)
            .collect();
        let tool_call_ids: Vec<i64> = message_ids
            .iter()
            .filter(|(t, _)| t == "tool_call")
            .map(|(_, id)| *id)
            .collect();
        let tool_output_ids: Vec<i64> = message_ids
            .iter()
            .filter(|(t, _)| t == "tool_output")
            .map(|(_, id)| *id)
            .collect();
        let reasoning_ids: Vec<i64> = message_ids
            .iter()
            .filter(|(t, _)| t == "reasoning")
            .map(|(_, id)| *id)
            .collect();

        // Build WHERE clause with proper parameter binding
        // Always use all 5 parameters ($2-$6) to maintain consistent type signature
        let query = r#"
            WITH conversation_messages AS (
                -- User messages
                SELECT
                    'user' as message_type,
                    um.id,
                    um.uuid,
                    r.uuid as response_uuid,
                    um.content_enc,
                    'completed'::text as status,
                    um.created_at,
                    r.model,
                    um.prompt_tokens as token_count,
                    NULL::uuid as tool_call_id,
                    NULL::text as finish_reason,
                    NULL::text as tool_name
                FROM user_messages um
                LEFT JOIN responses r ON um.response_id = r.id
                WHERE um.conversation_id = $1

                UNION ALL

                -- Assistant messages
                SELECT
                    'assistant' as message_type,
                    am.id,
                    am.uuid,
                    r.uuid as response_uuid,
                    am.content_enc,
                    am.status,
                    am.created_at,
                    r.model,
                    am.completion_tokens as token_count,
                    NULL::uuid as tool_call_id,
                    am.finish_reason,
                    NULL::text as tool_name
                FROM assistant_messages am
                LEFT JOIN responses r ON am.response_id = r.id
                WHERE am.conversation_id = $1

                UNION ALL

                -- Tool calls
                SELECT
                    'tool_call' as message_type,
                    tc.id,
                    tc.uuid,
                    r.uuid as response_uuid,
                    tc.arguments_enc as content_enc,
                    'completed'::text as status,
                    tc.created_at,
                    NULL::text as model,
                    tc.argument_tokens as token_count,
                    tc.uuid as tool_call_id,
                    NULL::text as finish_reason,
                    tc.name as tool_name
                FROM tool_calls tc
                LEFT JOIN responses r ON tc.response_id = r.id
                WHERE tc.conversation_id = $1

                UNION ALL

                -- Tool outputs
                SELECT
                    'tool_output' as message_type,
                    tto.id,
                    tto.uuid,
                    r.uuid as response_uuid,
                    tto.output_enc as content_enc,
                    'completed'::text as status,
                    tto.created_at,
                    NULL::text as model,
                    tto.output_tokens as token_count,
                    tc.uuid as tool_call_id,
                    NULL::text as finish_reason,
                    tc.name as tool_name
                FROM tool_outputs tto
                JOIN tool_calls tc ON tto.tool_call_fk = tc.id
                LEFT JOIN responses r ON tto.response_id = r.id
                WHERE tto.conversation_id = $1

                UNION ALL

                -- Reasoning items (for conversation items API, skipped in context builder)
                SELECT
                    'reasoning' as message_type,
                    ri.id,
                    ri.uuid,
                    r.uuid as response_uuid,
                    ri.content_enc,
                    ri.status,
                    ri.created_at,
                    NULL::text as model,
                    ri.reasoning_tokens as token_count,
                    NULL::uuid as tool_call_id,
                    NULL::text as finish_reason,
                    NULL::text as tool_name
                FROM reasoning_items ri
                LEFT JOIN responses r ON ri.response_id = r.id
                WHERE ri.conversation_id = $1
            )
            SELECT * FROM conversation_messages
            WHERE (message_type = 'user' AND id = ANY($2::bigint[]))
               OR (message_type = 'assistant' AND id = ANY($3::bigint[]))
               OR (message_type = 'tool_call' AND id = ANY($4::bigint[]))
               OR (message_type = 'tool_output' AND id = ANY($5::bigint[]))
               OR (message_type = 'reasoning' AND id = ANY($6::bigint[]))
            ORDER BY created_at ASC, id ASC
        "#;

        sql_query(query)
            .bind::<BigInt, _>(conversation_id)
            .bind::<Array<BigInt>, _>(&user_ids)
            .bind::<Array<BigInt>, _>(&assistant_ids)
            .bind::<Array<BigInt>, _>(&tool_call_ids)
            .bind::<Array<BigInt>, _>(&tool_output_ids)
            .bind::<Array<BigInt>, _>(&reasoning_ids)
            .load::<RawThreadMessage>(conn)
            .map_err(ResponsesError::DatabaseError)
    }
}

// ============================================================================
// Lightweight metadata struct for efficient truncation decisions
// ============================================================================

/// Metadata-only version of RawThreadMessage for efficient context building.
/// Excludes `content_enc` to avoid fetching and decrypting messages that will be truncated.
#[derive(QueryableByName, Debug, Clone)]
pub struct RawThreadMessageMetadata {
    #[diesel(sql_type = diesel::sql_types::Text)]
    pub message_type: String,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    pub id: i64,
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    pub uuid: Uuid,
    #[diesel(sql_type = diesel::sql_types::Timestamptz)]
    pub created_at: DateTime<Utc>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Integer>)]
    pub token_count: Option<i32>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Uuid>)]
    pub tool_call_id: Option<Uuid>,
    /// Ciphertext transfer size, independent of model-projected token count.
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    pub encrypted_bytes: i64,
}

impl RawThreadMessageMetadata {
    /// Fetch only message metadata (no content) for efficient truncation logic
    pub fn get_conversation_context_metadata(
        conn: &mut PgConnection,
        conversation_id: i64,
    ) -> Result<Vec<RawThreadMessageMetadata>, ResponsesError> {
        let query = r#"
            WITH conversation_messages AS (
                -- User messages
                SELECT
                    'user' as message_type,
                    um.id,
                    um.uuid,
                    um.created_at,
                    um.prompt_tokens as token_count,
                    NULL::uuid as tool_call_id,
                    COALESCE(octet_length(um.content_enc), 0)::bigint as encrypted_bytes
                FROM user_messages um
                WHERE um.conversation_id = $1

                UNION ALL

                -- Assistant messages
                SELECT
                    'assistant' as message_type,
                    am.id,
                    am.uuid,
                    am.created_at,
                    am.completion_tokens as token_count,
                    NULL::uuid as tool_call_id,
                    COALESCE(octet_length(am.content_enc), 0)::bigint as encrypted_bytes
                FROM assistant_messages am
                WHERE am.conversation_id = $1

                UNION ALL

                -- Tool calls
                SELECT
                    'tool_call' as message_type,
                    tc.id,
                    tc.uuid,
                    tc.created_at,
                    tc.argument_tokens as token_count,
                    tc.uuid as tool_call_id,
                    COALESCE(octet_length(tc.arguments_enc), 0)::bigint as encrypted_bytes
                FROM tool_calls tc
                WHERE tc.conversation_id = $1

                UNION ALL

                -- Tool outputs
                SELECT
                    'tool_output' as message_type,
                    tto.id,
                    tto.uuid,
                    tto.created_at,
                    tto.output_tokens as token_count,
                    tc.uuid as tool_call_id,
                    COALESCE(octet_length(tto.output_enc), 0)::bigint as encrypted_bytes
                FROM tool_outputs tto
                JOIN tool_calls tc ON tto.tool_call_fk = tc.id
                WHERE tto.conversation_id = $1

                UNION ALL

                -- Reasoning items
                -- TODO: Currently included for completeness but dropped in context_builder
                -- until we confirm how models expect reasoning to be passed back
                SELECT
                    'reasoning' as message_type,
                    ri.id,
                    ri.uuid,
                    ri.created_at,
                    ri.reasoning_tokens as token_count,
                    NULL::uuid as tool_call_id,
                    COALESCE(octet_length(ri.content_enc), 0)::bigint as encrypted_bytes
                FROM reasoning_items ri
                WHERE ri.conversation_id = $1
            )
            SELECT * FROM conversation_messages
            ORDER BY created_at DESC, id DESC
            LIMIT 1000
        "#;

        let mut results = sql_query(query)
            .bind::<BigInt, _>(conversation_id)
            .load::<RawThreadMessageMetadata>(conn)
            .map_err(ResponsesError::DatabaseError)?;

        // Reverse to chronological order (oldest first) since query returns newest first
        results.reverse();

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::{validate_conversation_project_limit, ResponsesError};

    #[test]
    fn conversation_project_limit_allows_creating_the_tenth_project() {
        assert!(validate_conversation_project_limit(9).is_ok());
    }

    #[test]
    fn conversation_project_limit_rejects_creating_the_eleventh_project() {
        let error = validate_conversation_project_limit(10).unwrap_err();
        assert!(matches!(error, ResponsesError::ValidationError));
    }
}
