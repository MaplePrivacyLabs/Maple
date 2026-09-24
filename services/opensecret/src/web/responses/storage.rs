//! Storage task components for persisting streaming response items.

use crate::{
    db::DBError,
    encrypt::encrypt_with_key,
    models::responses::{
        NewAssistantMessage, NewReasoningItem, NewToolCall, NewToolOutput, ResponseStatus,
        ResponsesError,
    },
    tokens::count_tokens,
    web::responses::constants::{
        FINISH_REASON_CANCELLED, STATUS_COMPLETED, STATUS_INCOMPLETE, STATUS_IN_PROGRESS,
    },
    DBConnection,
};
use chrono::Utc;
use secp256k1::SecretKey;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, warn};
use uuid::Uuid;

use super::handlers::{PublicResponseFailure, ResponseTerminal, StorageMessage};

/// Terminal result of the per-response storage worker. A terminal result is
/// returned only after pending items and the authoritative response status are
/// durable. The execution guard separately proves that the task has exited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageTaskOutcome {
    Completed,
    Cancelled,
    Failed,
}

impl StorageTaskOutcome {
    fn from_terminal(terminal: Option<&ResponseTerminal>) -> Self {
        match terminal {
            Some(ResponseTerminal::Completed { .. }) => Self::Completed,
            Some(ResponseTerminal::Cancelled) => Self::Cancelled,
            Some(ResponseTerminal::Failed(_)) | None => Self::Failed,
        }
    }
}

const TERMINAL_RETRY_INITIAL_DELAY: Duration = Duration::from_millis(100);
const TERMINAL_RETRY_MAX_DELAY: Duration = Duration::from_secs(5);
const INTERRUPTED_TOOL_OUTPUT: &str = "Tool execution was interrupted before completion.";

#[derive(Clone, Default)]
struct PendingAssistantMessage {
    content: String,
    created_at: Option<chrono::DateTime<chrono::Utc>>,
    completed_finish_reason: Option<String>,
}

impl PendingAssistantMessage {
    fn terminal_status_and_finish_reason(
        &self,
        incomplete_finish_reason: Option<String>,
    ) -> (&'static str, Option<String>) {
        match &self.completed_finish_reason {
            Some(finish_reason) => (STATUS_COMPLETED, Some(finish_reason.clone())),
            None => (STATUS_INCOMPLETE, incomplete_finish_reason),
        }
    }
}

#[derive(Clone, Default)]
struct PendingReasoningItem {
    content: String,
    created_at: Option<chrono::DateTime<chrono::Utc>>,
    completed: bool,
}

impl PendingReasoningItem {
    fn terminal_status(&self) -> &'static str {
        if self.completed {
            STATUS_COMPLETED
        } else {
            STATUS_INCOMPLETE
        }
    }
}

#[derive(Clone)]
struct PendingToolCall {
    name: String,
    arguments: serde_json::Value,
    created_at: chrono::DateTime<chrono::Utc>,
    output_id: Uuid,
    output: Option<String>,
    output_created_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl PendingToolCall {
    fn terminal_output(&self) -> &str {
        self.output.as_deref().unwrap_or(INTERRUPTED_TOOL_OUTPUT)
    }
}

fn clamp_token_count(text: &str, label: &str) -> i32 {
    let token_count = count_tokens(text);
    if token_count > i32::MAX as usize {
        warn!(
            "{} token count {} exceeds i32::MAX, clamping",
            label, token_count
        );
        i32::MAX
    } else {
        token_count as i32
    }
}

fn allocate_created_at(
    next_created_at: &mut chrono::DateTime<chrono::Utc>,
) -> chrono::DateTime<chrono::Utc> {
    let created_at = next_created_at.to_owned();
    *next_created_at = created_at + chrono::Duration::microseconds(1);
    created_at
}

fn create_assistant_message_if_missing(
    db: &Arc<dyn DBConnection + Send + Sync>,
    conversation_id: i64,
    response_id: i64,
    user_id: Uuid,
    item_id: Uuid,
    created_at: chrono::DateTime<chrono::Utc>,
) -> Result<(), String> {
    match db.get_assistant_message_by_uuid(item_id) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => db
            .create_assistant_message(NewAssistantMessage {
                uuid: item_id,
                conversation_id,
                response_id: Some(response_id),
                user_id,
                content_enc: None,
                completion_tokens: 0,
                status: STATUS_IN_PROGRESS.to_string(),
                finish_reason: None,
                created_at,
            })
            .map(|_| ())
            .map_err(|e| format!("Failed to create assistant message: {:?}", e)),
        Err(e) => Err(format!("Failed to look up assistant message: {:?}", e)),
    }
}

fn update_terminal_response_status(
    db: &Arc<dyn DBConnection + Send + Sync>,
    response_id: i64,
    response_uuid: Uuid,
    user_id: Uuid,
    requested: ResponseTerminal,
    reason: &str,
) -> Result<Option<ResponseTerminal>, String> {
    let status = requested.status();
    match db.update_response_status_if_current(
        response_id,
        ResponseStatus::InProgress,
        status,
        Some(Utc::now()),
    ) {
        Ok(true) => Ok(Some(requested)),
        Ok(false) => {
            debug!(
                "Storage: skipped setting response {} ({}) to {:?} after {}; response was no longer in_progress",
                response_id, response_uuid, status, reason
            );
            match db.get_response_by_uuid_and_user(response_uuid, user_id) {
                Ok(response) => match response.status {
                    ResponseStatus::Completed => Ok(Some(match requested {
                        ResponseTerminal::Completed { .. } => requested,
                        _ => ResponseTerminal::Completed {
                            finish_reason: "stop".to_string(),
                        },
                    })),
                    ResponseStatus::Cancelled => Ok(Some(ResponseTerminal::Cancelled)),
                    ResponseStatus::Failed => Ok(Some(ResponseTerminal::Failed(
                        PublicResponseFailure::Internal,
                    ))),
                    ResponseStatus::Queued | ResponseStatus::InProgress => Err(format!(
                        "Response remained {:?} after terminal compare-and-set",
                        response.status
                    )),
                },
                Err(DBError::ResponsesError(ResponsesError::ResponseNotFound)) => {
                    debug!(
                        "Response {} ({}) was deleted before terminal persistence completed",
                        response_id, response_uuid
                    );
                    Ok(None)
                }
                Err(e) => {
                    error!(
                        "Failed to read authoritative response {} ({}) status after {}: {:?}",
                        response_id, response_uuid, reason, e
                    );
                    Err("Failed to read authoritative terminal response status".to_string())
                }
            }
        }
        Err(e) => {
            error!(
                "Failed to update response {} ({}) status to {:?} after {}: {:?}",
                response_id, response_uuid, status, reason, e
            );
            Err("Failed to persist terminal response status".to_string())
        }
    }
}

async fn finalize_assistant_message(
    db: &Arc<dyn DBConnection + Send + Sync>,
    user_key: &SecretKey,
    item_id: Uuid,
    content: String,
    status: &str,
    finish_reason: Option<String>,
) -> Result<(), String> {
    let content_enc = if content.is_empty() {
        None
    } else {
        Some(encrypt_with_key(user_key, content.as_bytes()).await)
    };

    db.update_assistant_message(
        item_id,
        content_enc,
        clamp_token_count(&content, "assistant message"),
        status.to_string(),
        finish_reason,
    )
    .map(|_| ())
    .map_err(|e| format!("Failed to update assistant message: {:?}", e))
}

fn create_reasoning_item_if_missing(
    db: &Arc<dyn DBConnection + Send + Sync>,
    conversation_id: i64,
    response_id: i64,
    user_id: Uuid,
    item_id: Uuid,
    created_at: chrono::DateTime<chrono::Utc>,
) -> Result<(), String> {
    match db.get_reasoning_item_by_uuid(item_id) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => db
            .create_reasoning_item(NewReasoningItem {
                uuid: item_id,
                conversation_id,
                response_id: Some(response_id),
                assistant_message_id: None,
                user_id,
                content_enc: None,
                summary_enc: None,
                reasoning_tokens: 0,
                status: STATUS_IN_PROGRESS.to_string(),
                created_at,
            })
            .map(|_| ())
            .map_err(|e| format!("Failed to create reasoning item: {:?}", e)),
        Err(e) => Err(format!("Failed to look up reasoning item: {:?}", e)),
    }
}

async fn finalize_reasoning_item(
    db: &Arc<dyn DBConnection + Send + Sync>,
    user_key: &SecretKey,
    item_id: Uuid,
    content: String,
    status: &str,
) -> Result<(), String> {
    let content_enc = if content.is_empty() {
        None
    } else {
        Some(encrypt_with_key(user_key, content.as_bytes()).await)
    };

    db.update_reasoning_item(
        item_id,
        content_enc,
        clamp_token_count(&content, "reasoning"),
        status.to_string(),
    )
    .map(|_| ())
    .map_err(|e| format!("Failed to update reasoning item: {:?}", e))
}

#[allow(clippy::too_many_arguments)]
async fn persist_tool_call(
    db: &Arc<dyn DBConnection + Send + Sync>,
    user_key: &SecretKey,
    conversation_id: i64,
    response_id: i64,
    user_id: Uuid,
    tool_call_id: Uuid,
    name: String,
    arguments: serde_json::Value,
    created_at: chrono::DateTime<chrono::Utc>,
) -> Result<(), String> {
    let arguments_json = serde_json::to_string(&arguments)
        .map_err(|e| format!("Failed to serialize tool arguments: {:?}", e))?;
    let arguments_enc = encrypt_with_key(user_key, arguments_json.as_bytes()).await;

    db.create_tool_call(NewToolCall {
        uuid: tool_call_id,
        conversation_id,
        response_id: Some(response_id),
        user_id,
        name,
        arguments_enc: Some(arguments_enc),
        argument_tokens: clamp_token_count(&arguments_json, "tool arguments"),
        status: STATUS_COMPLETED.to_string(),
        created_at,
    })
    .map(|_| ())
    .map_err(|e| format!("Failed to persist tool_call: {:?}", e))
}

#[allow(clippy::too_many_arguments)]
async fn persist_tool_call_if_missing(
    db: &Arc<dyn DBConnection + Send + Sync>,
    user_key: &SecretKey,
    conversation_id: i64,
    response_id: i64,
    user_id: Uuid,
    tool_call_id: Uuid,
    name: String,
    arguments: serde_json::Value,
    created_at: chrono::DateTime<chrono::Utc>,
) -> Result<(), String> {
    match db.get_tool_call_by_uuid(tool_call_id, user_id) {
        Ok(_) => Ok(()),
        Err(DBError::ResponsesError(ResponsesError::ToolCallNotFound)) => {
            persist_tool_call(
                db,
                user_key,
                conversation_id,
                response_id,
                user_id,
                tool_call_id,
                name,
                arguments,
                created_at,
            )
            .await
        }
        Err(e) => Err(format!("Failed to look up tool_call: {:?}", e)),
    }
}

#[allow(clippy::too_many_arguments)]
async fn persist_tool_output(
    db: &Arc<dyn DBConnection + Send + Sync>,
    user_key: &SecretKey,
    conversation_id: i64,
    response_id: i64,
    user_id: Uuid,
    tool_output_id: Uuid,
    tool_call_id: Uuid,
    output: String,
    created_at: chrono::DateTime<chrono::Utc>,
) -> Result<(), String> {
    let tool_call_fk = db
        .get_tool_call_by_uuid(tool_call_id, user_id)
        .map_err(|e| format!("Tool call not found in database: {:?}", e))?
        .id;
    let output_enc = encrypt_with_key(user_key, output.as_bytes()).await;

    db.create_tool_output(NewToolOutput {
        uuid: tool_output_id,
        conversation_id,
        response_id: Some(response_id),
        user_id,
        tool_call_fk,
        output_enc,
        output_tokens: clamp_token_count(&output, "tool output"),
        status: STATUS_COMPLETED.to_string(),
        error: None,
        created_at,
    })
    .map(|_| ())
    .map_err(|e| format!("Failed to persist tool_output: {:?}", e))
}

#[allow(clippy::too_many_arguments)]
async fn persist_tool_output_if_missing(
    db: &Arc<dyn DBConnection + Send + Sync>,
    user_key: &SecretKey,
    conversation_id: i64,
    response_id: i64,
    user_id: Uuid,
    tool_output_id: Uuid,
    tool_call_id: Uuid,
    output: String,
    created_at: chrono::DateTime<chrono::Utc>,
) -> Result<(), String> {
    match db.get_tool_output_by_uuid(tool_output_id, user_id) {
        Ok(_) => Ok(()),
        Err(DBError::ResponsesError(ResponsesError::ToolOutputNotFound)) => {
            persist_tool_output(
                db,
                user_key,
                conversation_id,
                response_id,
                user_id,
                tool_output_id,
                tool_call_id,
                output,
                created_at,
            )
            .await
        }
        Err(e) => Err(format!("Failed to look up tool_output: {:?}", e)),
    }
}

#[allow(clippy::too_many_arguments)]
async fn finalize_pending_items_for_terminal(
    db: &Arc<dyn DBConnection + Send + Sync>,
    user_key: &SecretKey,
    conversation_id: i64,
    response_id: i64,
    user_id: Uuid,
    next_created_at: &mut chrono::DateTime<chrono::Utc>,
    pending_messages: &mut HashMap<Uuid, PendingAssistantMessage>,
    pending_reasoning: &mut HashMap<Uuid, PendingReasoningItem>,
    message_finish_reason: Option<String>,
) -> Result<(), String> {
    let mut failures = Vec::new();
    let message_ids: Vec<_> = pending_messages.keys().copied().collect();
    for item_id in message_ids {
        let pending = {
            let Some(pending) = pending_messages.get_mut(&item_id) else {
                continue;
            };
            pending
                .created_at
                .get_or_insert_with(|| allocate_created_at(next_created_at));
            pending.clone()
        };
        let create_result = create_assistant_message_if_missing(
            db,
            conversation_id,
            response_id,
            user_id,
            item_id,
            pending
                .created_at
                .expect("pending message timestamp is set"),
        );
        let (status, finish_reason) =
            pending.terminal_status_and_finish_reason(message_finish_reason.clone());
        let result = match create_result {
            Ok(()) => {
                finalize_assistant_message(
                    db,
                    user_key,
                    item_id,
                    pending.content,
                    status,
                    finish_reason,
                )
                .await
            }
            Err(e) => Err(e),
        };
        match result {
            Ok(()) => {
                pending_messages.remove(&item_id);
            }
            Err(e) => {
                error!(
                    "Failed to finalize pending assistant message {}: {}",
                    item_id, e
                );
                failures.push(e);
            }
        }
    }

    let reasoning_ids: Vec<_> = pending_reasoning.keys().copied().collect();
    for item_id in reasoning_ids {
        let pending = {
            let Some(pending) = pending_reasoning.get_mut(&item_id) else {
                continue;
            };
            pending
                .created_at
                .get_or_insert_with(|| allocate_created_at(next_created_at));
            pending.clone()
        };
        let result = create_reasoning_item_if_missing(
            db,
            conversation_id,
            response_id,
            user_id,
            item_id,
            pending
                .created_at
                .expect("pending reasoning timestamp is set"),
        );
        let status = pending.terminal_status();
        let result = match result {
            Ok(()) => finalize_reasoning_item(db, user_key, item_id, pending.content, status).await,
            Err(e) => Err(e),
        };
        match result {
            Ok(()) => {
                pending_reasoning.remove(&item_id);
            }
            Err(e) => {
                error!(
                    "Failed to finalize pending reasoning item {}: {}",
                    item_id, e
                );
                failures.push(e);
            }
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Failed to finalize {} pending response item(s)",
            failures.len()
        ))
    }
}

#[allow(clippy::too_many_arguments)]
async fn finalize_pending_tool_calls_for_terminal(
    db: &Arc<dyn DBConnection + Send + Sync>,
    user_key: &SecretKey,
    conversation_id: i64,
    response_id: i64,
    user_id: Uuid,
    next_created_at: &mut chrono::DateTime<chrono::Utc>,
    pending_tool_calls: &mut HashMap<Uuid, PendingToolCall>,
) -> Result<(), String> {
    let mut failures = Vec::new();
    let tool_call_ids: Vec<_> = pending_tool_calls.keys().copied().collect();

    for tool_call_id in tool_call_ids {
        let pending = {
            let Some(pending) = pending_tool_calls.get_mut(&tool_call_id) else {
                continue;
            };
            pending
                .output_created_at
                .get_or_insert_with(|| allocate_created_at(next_created_at));
            pending.clone()
        };
        let terminal_output = pending.terminal_output().to_string();

        let call_result = persist_tool_call_if_missing(
            db,
            user_key,
            conversation_id,
            response_id,
            user_id,
            tool_call_id,
            pending.name,
            pending.arguments,
            pending.created_at,
        )
        .await;
        let output_result = match call_result {
            Ok(()) => {
                persist_tool_output_if_missing(
                    db,
                    user_key,
                    conversation_id,
                    response_id,
                    user_id,
                    pending.output_id,
                    tool_call_id,
                    terminal_output,
                    pending
                        .output_created_at
                        .expect("pending tool output timestamp is set"),
                )
                .await
            }
            Err(e) => Err(e),
        };

        match output_result {
            Ok(()) => {
                pending_tool_calls.remove(&tool_call_id);
            }
            Err(e) => {
                error!(
                    "Failed to terminalize pending tool_call {}: {}",
                    tool_call_id, e
                );
                failures.push(e);
            }
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Failed to terminalize {} pending tool call(s)",
            failures.len()
        ))
    }
}

#[allow(clippy::too_many_arguments)]
async fn persist_terminal_until_authoritative(
    db: &Arc<dyn DBConnection + Send + Sync>,
    user_key: &SecretKey,
    conversation_id: i64,
    response_id: i64,
    response_uuid: Uuid,
    user_id: Uuid,
    next_created_at: &mut chrono::DateTime<chrono::Utc>,
    pending_messages: &mut HashMap<Uuid, PendingAssistantMessage>,
    pending_reasoning: &mut HashMap<Uuid, PendingReasoningItem>,
    pending_tool_calls: &mut HashMap<Uuid, PendingToolCall>,
    requested: ResponseTerminal,
    message_finish_reason: Option<String>,
    reason: &str,
) -> Option<ResponseTerminal> {
    let mut retry_delay = TERMINAL_RETRY_INITIAL_DELAY;

    loop {
        let item_cleanup_result = finalize_pending_items_for_terminal(
            db,
            user_key,
            conversation_id,
            response_id,
            user_id,
            next_created_at,
            pending_messages,
            pending_reasoning,
            message_finish_reason.clone(),
        )
        .await;
        let tool_cleanup_result = finalize_pending_tool_calls_for_terminal(
            db,
            user_key,
            conversation_id,
            response_id,
            user_id,
            next_created_at,
            pending_tool_calls,
        )
        .await;
        let cleanup_result = match (item_cleanup_result, tool_cleanup_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(e), Ok(())) | (Ok(()), Err(e)) => Err(e),
            (Err(item_error), Err(tool_error)) => Err(format!(
                "Pending item cleanup failed: {}; pending tool cleanup failed: {}",
                item_error, tool_error
            )),
        };

        let terminal_result = match cleanup_result {
            Ok(()) => update_terminal_response_status(
                db,
                response_id,
                response_uuid,
                user_id,
                requested.clone(),
                reason,
            ),
            Err(cleanup_error) => match db.get_response_by_uuid_and_user(response_uuid, user_id) {
                Err(DBError::ResponsesError(ResponsesError::ResponseNotFound)) => {
                    debug!(
                        "Response {} ({}) was deleted while pending item cleanup was retrying",
                        response_id, response_uuid
                    );
                    Ok(None)
                }
                Ok(_) => Err(cleanup_error),
                Err(e) => {
                    warn!(
                        "Failed to verify response existence after cleanup error: response_uuid={}, error={:?}",
                        response_uuid, e
                    );
                    Err(cleanup_error)
                }
            },
        };

        match terminal_result {
            Ok(authoritative) => return authoritative,
            Err(e) => {
                warn!(
                    "Storage terminal persistence will retry: response_uuid={}, reason={}, retry_delay_ms={}, error={}",
                    response_uuid,
                    reason,
                    retry_delay.as_millis(),
                    e
                );
                tokio::time::sleep(retry_delay).await;
                retry_delay = retry_delay.saturating_mul(2).min(TERMINAL_RETRY_MAX_DELAY);
            }
        }
    }
}

/// Main storage task that orchestrates per-item persistence.
#[allow(clippy::too_many_arguments)]
pub async fn storage_task(
    mut rx: mpsc::Receiver<StorageMessage>,
    tool_persist_ack: Option<mpsc::Sender<Result<(), String>>>,
    terminal_persist_ack: Option<oneshot::Sender<Result<Option<ResponseTerminal>, String>>>,
    db: Arc<dyn DBConnection + Send + Sync>,
    response_id: i64,
    response_uuid: Uuid,
    first_item_created_at: chrono::DateTime<chrono::Utc>,
    conversation_id: i64,
    user_id: Uuid,
    user_key: SecretKey,
) -> StorageTaskOutcome {
    let tool_ack = tool_persist_ack;
    let mut terminal_ack = terminal_persist_ack;
    let mut pending_messages: HashMap<Uuid, PendingAssistantMessage> = HashMap::new();
    let mut pending_reasoning: HashMap<Uuid, PendingReasoningItem> = HashMap::new();
    let mut pending_tool_calls: HashMap<Uuid, PendingToolCall> = HashMap::new();
    let mut next_item_created_at = first_item_created_at;
    let mut storage_failed = false;

    while let Some(msg) = rx.recv().await {
        match msg {
            StorageMessage::MessageStarted { item_id } => {
                let pending = pending_messages.entry(item_id).or_default();
                let created_at = pending
                    .created_at
                    .get_or_insert_with(|| allocate_created_at(&mut next_item_created_at))
                    .to_owned();
                if let Err(e) = create_assistant_message_if_missing(
                    &db,
                    conversation_id,
                    response_id,
                    user_id,
                    item_id,
                    created_at,
                ) {
                    error!("{}", e);
                    storage_failed = true;
                }
            }
            StorageMessage::ContentDelta { item_id, delta } => {
                pending_messages
                    .entry(item_id)
                    .or_default()
                    .content
                    .push_str(&delta);
            }
            StorageMessage::MessageDone {
                item_id,
                finish_reason,
            } => {
                debug!(
                    "Storage: message done {} with finish_reason={}",
                    item_id, finish_reason
                );
                let (created_at, content) = {
                    let pending = pending_messages.entry(item_id).or_default();
                    pending.completed_finish_reason = Some(finish_reason.clone());
                    let created_at = pending
                        .created_at
                        .get_or_insert_with(|| allocate_created_at(&mut next_item_created_at))
                        .to_owned();
                    (created_at, pending.content.clone())
                };
                let create_result = create_assistant_message_if_missing(
                    &db,
                    conversation_id,
                    response_id,
                    user_id,
                    item_id,
                    created_at,
                );
                if let Err(e) = &create_result {
                    error!("{}", e);
                    storage_failed = true;
                }
                if create_result.is_ok() {
                    match finalize_assistant_message(
                        &db,
                        &user_key,
                        item_id,
                        content,
                        STATUS_COMPLETED,
                        Some(finish_reason),
                    )
                    .await
                    {
                        Ok(()) => {
                            pending_messages.remove(&item_id);
                        }
                        Err(e) => {
                            error!("{}", e);
                            storage_failed = true;
                        }
                    }
                }
            }
            StorageMessage::ReasoningStarted { item_id } => {
                let pending = pending_reasoning.entry(item_id).or_default();
                let created_at = pending
                    .created_at
                    .get_or_insert_with(|| allocate_created_at(&mut next_item_created_at))
                    .to_owned();
                if let Err(e) = create_reasoning_item_if_missing(
                    &db,
                    conversation_id,
                    response_id,
                    user_id,
                    item_id,
                    created_at,
                ) {
                    error!("{}", e);
                    storage_failed = true;
                }
            }
            StorageMessage::ReasoningDelta { item_id, delta } => {
                pending_reasoning
                    .entry(item_id)
                    .or_default()
                    .content
                    .push_str(&delta);
            }
            StorageMessage::ReasoningDone { item_id } => {
                let reasoning_finalize_started = Instant::now();
                debug!(
                    "Storage: finalizing reasoning item {} for response {}",
                    item_id, response_id
                );
                let content = {
                    let pending = pending_reasoning.entry(item_id).or_default();
                    pending.completed = true;
                    pending.content.clone()
                };
                if let Err(e) =
                    finalize_reasoning_item(&db, &user_key, item_id, content, STATUS_COMPLETED)
                        .await
                {
                    error!(
                        "Failed to finalize reasoning item {} for response {} after {} ms: {}",
                        item_id,
                        response_id,
                        reasoning_finalize_started.elapsed().as_millis(),
                        e
                    );
                    storage_failed = true;
                } else {
                    pending_reasoning.remove(&item_id);
                    debug!(
                        "Storage: finalized reasoning item {} for response {} in {} ms",
                        item_id,
                        response_id,
                        reasoning_finalize_started.elapsed().as_millis()
                    );
                }
            }
            StorageMessage::Usage { .. } => {}
            StorageMessage::Terminal(mut requested) => {
                if storage_failed && matches!(requested, ResponseTerminal::Completed { .. }) {
                    requested = ResponseTerminal::Failed(PublicResponseFailure::Internal);
                }

                if (!pending_messages.is_empty()
                    || !pending_reasoning.is_empty()
                    || !pending_tool_calls.is_empty())
                    && matches!(requested, ResponseTerminal::Completed { .. })
                {
                    error!(
                        "Completed terminal received with {} pending message(s), {} pending reasoning item(s), and {} pending tool call(s); failing response",
                        pending_messages.len(),
                        pending_reasoning.len(),
                        pending_tool_calls.len()
                    );
                    requested = ResponseTerminal::Failed(PublicResponseFailure::Internal);
                }

                let finish_reason = matches!(requested, ResponseTerminal::Cancelled)
                    .then(|| FINISH_REASON_CANCELLED.to_string());
                let effective = persist_terminal_until_authoritative(
                    &db,
                    &user_key,
                    conversation_id,
                    response_id,
                    response_uuid,
                    user_id,
                    &mut next_item_created_at,
                    &mut pending_messages,
                    &mut pending_reasoning,
                    &mut pending_tool_calls,
                    requested,
                    finish_reason,
                    "supervised terminal",
                )
                .await;
                let outcome = StorageTaskOutcome::from_terminal(effective.as_ref());
                if let Some(ack) = terminal_ack.take() {
                    let _ = ack.send(Ok(effective));
                }
                return outcome;
            }
            StorageMessage::ToolCall {
                tool_call_id,
                tool_output_id,
                name,
                arguments,
            } => {
                let tool_name = name.clone();
                let tool_call_persist_started = Instant::now();
                debug!(
                    "Storage: persisting tool_call {} ({}) for response {}",
                    tool_call_id, tool_name, response_id
                );
                let created_at = allocate_created_at(&mut next_item_created_at);
                pending_tool_calls.insert(
                    tool_call_id,
                    PendingToolCall {
                        name: name.clone(),
                        arguments: arguments.clone(),
                        created_at,
                        output_id: tool_output_id,
                        output: None,
                        output_created_at: None,
                    },
                );
                match persist_tool_call_if_missing(
                    &db,
                    &user_key,
                    conversation_id,
                    response_id,
                    user_id,
                    tool_call_id,
                    name,
                    arguments,
                    created_at,
                )
                .await
                {
                    Ok(()) => debug!(
                        "Storage: persisted tool_call {} ({}) for response {} in {} ms",
                        tool_call_id,
                        tool_name,
                        response_id,
                        tool_call_persist_started.elapsed().as_millis()
                    ),
                    Err(e) => {
                        error!(
                            "Failed to persist tool_call {} ({}) for response {} after {} ms: {}",
                            tool_call_id,
                            tool_name,
                            response_id,
                            tool_call_persist_started.elapsed().as_millis(),
                            e
                        );
                        storage_failed = true;
                        if let Some(ack) = &tool_ack {
                            let _ = ack.send(Err(e)).await;
                        }
                    }
                }
            }
            StorageMessage::ToolOutput {
                tool_output_id,
                tool_call_id,
                output,
            } => {
                let tool_output_persist_started = Instant::now();
                debug!(
                    "Storage: persisting tool_output {} for tool_call {} on response {}",
                    tool_output_id, tool_call_id, response_id
                );
                let created_at = allocate_created_at(&mut next_item_created_at);
                if let Some(pending) = pending_tool_calls.get_mut(&tool_call_id) {
                    pending.output_id = tool_output_id;
                    pending.output = Some(output.clone());
                    pending.output_created_at = Some(created_at);
                }
                match persist_tool_output_if_missing(
                    &db,
                    &user_key,
                    conversation_id,
                    response_id,
                    user_id,
                    tool_output_id,
                    tool_call_id,
                    output,
                    created_at,
                )
                .await
                {
                    Ok(()) => {
                        pending_tool_calls.remove(&tool_call_id);
                        debug!(
                            "Storage: persisted tool_output {} for tool_call {} on response {} in {} ms",
                            tool_output_id,
                            tool_call_id,
                            response_id,
                            tool_output_persist_started.elapsed().as_millis()
                        );
                        if let Some(ack) = &tool_ack {
                            let _ = ack.send(Ok(())).await;
                        }
                    }
                    Err(e) => {
                        error!(
                            "Failed to persist tool_output {} for tool_call {} on response {} after {} ms: {}",
                            tool_output_id,
                            tool_call_id,
                            response_id,
                            tool_output_persist_started.elapsed().as_millis(),
                            e
                        );
                        storage_failed = true;
                        if let Some(ack) = &tool_ack {
                            let _ = ack.send(Err(e)).await;
                        }
                    }
                }
            }
        }
    }

    warn!(
        "Storage channel closed before receiving a supervised terminal for response {} ({})",
        response_id, response_uuid
    );
    let effective = persist_terminal_until_authoritative(
        &db,
        &user_key,
        conversation_id,
        response_id,
        response_uuid,
        user_id,
        &mut next_item_created_at,
        &mut pending_messages,
        &mut pending_reasoning,
        &mut pending_tool_calls,
        ResponseTerminal::Failed(PublicResponseFailure::Internal),
        None,
        "premature storage channel close",
    )
    .await;
    let outcome = StorageTaskOutcome::from_terminal(effective.as_ref());
    if let Some(ack) = terminal_ack.take() {
        let _ = ack.send(Ok(effective));
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_item_status_preserves_done_boundaries() {
        let active_message = PendingAssistantMessage::default();
        let (status, finish_reason) = active_message
            .terminal_status_and_finish_reason(Some(FINISH_REASON_CANCELLED.to_string()));
        assert_eq!(status, STATUS_INCOMPLETE);
        assert_eq!(finish_reason.as_deref(), Some(FINISH_REASON_CANCELLED));

        let done_message = PendingAssistantMessage {
            completed_finish_reason: Some("stop".to_string()),
            ..PendingAssistantMessage::default()
        };
        let (status, finish_reason) = done_message
            .terminal_status_and_finish_reason(Some(FINISH_REASON_CANCELLED.to_string()));
        assert_eq!(status, STATUS_COMPLETED);
        assert_eq!(finish_reason.as_deref(), Some("stop"));

        assert_eq!(
            PendingReasoningItem::default().terminal_status(),
            STATUS_INCOMPLETE
        );
        assert_eq!(
            PendingReasoningItem {
                completed: true,
                ..PendingReasoningItem::default()
            }
            .terminal_status(),
            STATUS_COMPLETED
        );
    }

    #[test]
    fn pending_tool_call_uses_real_output_or_interrupted_fallback() {
        let mut pending = PendingToolCall {
            name: "web_search".to_string(),
            arguments: serde_json::json!({"query": "maple"}),
            created_at: Utc::now(),
            output_id: Uuid::new_v4(),
            output: None,
            output_created_at: None,
        };

        assert_eq!(pending.terminal_output(), INTERRUPTED_TOOL_OUTPUT);
        pending.output = Some("real tool output".to_string());
        assert_eq!(pending.terminal_output(), "real tool output");
    }
}
