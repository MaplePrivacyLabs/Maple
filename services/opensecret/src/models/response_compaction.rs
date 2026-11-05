//! User-scoped encrypted Responses checkpoints. Canonical items are never rewritten.
//!
//! Source identities, ciphertext digests and rendering metadata fence publication;
//! timestamps are ordering data, never a coverage boundary.
use super::responses::{RawThreadMessage, Response, ResponsesError, UserMessage};
use chrono::{DateTime, Utc};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::sql_types::{Array, BigInt, Binary, Integer, Nullable, Text, Timestamptz};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ContextItemKind {
    User,
    Assistant,
    ToolCall,
    ToolOutput,
    Reasoning,
}

impl ContextItemKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::ToolCall => "tool_call",
            Self::ToolOutput => "tool_output",
            Self::Reasoning => "reasoning",
        }
    }

    pub fn from_db_type(value: &str) -> Option<Self> {
        match value {
            "user" => Some(Self::User),
            "assistant" => Some(Self::Assistant),
            "tool_call" => Some(Self::ToolCall),
            "tool_output" => Some(Self::ToolOutput),
            "reasoning" => Some(Self::Reasoning),
            _ => None,
        }
    }

    fn table(self) -> &'static str {
        match self {
            Self::User => "user_messages",
            Self::Assistant => "assistant_messages",
            Self::ToolCall => "tool_calls",
            Self::ToolOutput => "tool_outputs",
            Self::Reasoning => "reasoning_items",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContextItemKey {
    pub kind: ContextItemKind,
    pub uuid: Uuid,
}

#[derive(Debug, thiserror::Error)]
pub enum CompactionError {
    #[error("Compaction database query failed")]
    Database(#[from] diesel::result::Error),
    #[error("Compaction source conversation was not found")]
    ConversationNotFound,
    #[error("Compaction source selection is invalid")]
    InvalidSelection,
    #[error("Compaction checkpoint is invalid")]
    InvalidCheckpoint,
    #[error("Compaction source serialization failed")]
    Serialization,
    #[error("Compaction source response error")]
    Responses(#[from] ResponsesError),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompactionSourceMetadata {
    pub key: ContextItemKey,
    pub id: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub token_count: Option<i32>,
    pub item_status: Option<String>,
    pub response_uuid: Option<Uuid>,
    pub response_status: Option<String>,
    pub tool_call_id: Option<Uuid>,
    pub tool_name: Option<String>,
    pub model: Option<String>,
    pub finish_reason: Option<String>,
    pub encrypted_bytes: i64,
    pub content_fingerprint: String,
}

impl CompactionSourceMetadata {
    /// A terminal response owns an immutable transcript even when its last item
    /// was interrupted. Legacy standalone rows have no execution to consult, so
    /// only their explicit terminal item states establish historical finality.
    /// Unknown and still-running executions must never be inferred complete.
    pub fn is_immutable_historical(&self) -> bool {
        match self.response_uuid {
            Some(_) => matches!(
                self.response_status.as_deref(),
                Some("completed" | "failed" | "cancelled")
            ),
            None => matches!(
                self.item_status.as_deref(),
                Some("completed" | "incomplete")
            ),
        }
    }

    /// The current execution may contribute only completed steps. Other
    /// executions contribute their partial items only after becoming terminal;
    /// concurrent or unknown executions remain entirely verbatim.
    pub fn is_settled_for(
        &self,
        current_response: Option<Uuid>,
        active_response_uuids: &[Uuid],
    ) -> bool {
        if self.response_uuid.is_some_and(|uuid| {
            Some(uuid) != current_response && active_response_uuids.contains(&uuid)
        }) {
            return false;
        }
        if self.response_uuid.is_some() && self.response_uuid == current_response {
            self.item_status.as_deref() == Some("completed")
        } else {
            self.is_immutable_historical()
        }
    }
}

#[derive(Debug)]
pub struct CompactionSnapshot {
    pub conversation_id: i64,
    pub conversation_uuid: Uuid,
    pub user_id: Uuid,
    pub items: Vec<CompactionSourceMetadata>,
    pub active_response_uuids: Vec<Uuid>,
    pub fingerprint: [u8; 32],
    pub checkpoint: Option<CompactionCheckpoint>,
}

#[derive(Debug, Clone, QueryableByName)]
pub struct CompactionCheckpoint {
    #[diesel(sql_type = BigInt)]
    pub id: i64,
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    pub uuid: Uuid,
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    pub user_id: Uuid,
    #[diesel(sql_type = BigInt)]
    pub conversation_id: i64,
    #[diesel(sql_type = Binary)]
    pub content_enc: Vec<u8>,
    #[diesel(sql_type = Integer)]
    pub content_tokens: i32,
    #[diesel(sql_type = Nullable<BigInt>)]
    pub previous_summary_id: Option<i64>,
    #[diesel(sql_type = Timestamptz)]
    pub created_at: DateTime<Utc>,
    #[diesel(sql_type = Integer)]
    pub format_version: i32,
    #[diesel(sql_type = BigInt)]
    pub generation: i64,
    #[diesel(sql_type = Binary)]
    pub source_fingerprint: Vec<u8>,
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    pub origin_response_uuid: Uuid,
    #[diesel(sql_type = Nullable<diesel::sql_types::Uuid>)]
    pub after_item_uuid: Option<Uuid>,
}

/// Public marker projection. Its SQL deliberately never selects checkpoint
/// ciphertext, summaries, source fingerprints or covered conversation items.
#[derive(Debug, Clone, QueryableByName)]
pub struct CompactionMarkerMetadata {
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    pub uuid: Uuid,
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    pub origin_response_uuid: Uuid,
    #[diesel(sql_type = Nullable<diesel::sql_types::Uuid>)]
    pub after_item_uuid: Option<Uuid>,
    #[diesel(sql_type = Timestamptz)]
    pub created_at: DateTime<Utc>,
    #[diesel(sql_type = BigInt)]
    pub generation: i64,
}

/// The envelope is encrypted by the user-key owner, including the row/conversation
/// identities and exact covered keys. Plaintext coverage is used only for this CAS.
pub struct NewCompactionCheckpoint {
    pub uuid: Uuid,
    pub conversation_id: i64,
    pub user_id: Uuid,
    pub content_enc: Vec<u8>,
    pub content_tokens: i32,
    pub expected_head_uuid: Option<Uuid>,
    pub covered_items: Vec<ContextItemKey>,
    /// Previous authenticated envelope coverage; verified against the current head digest.
    pub previous_covered_items: Vec<ContextItemKey>,
    pub source_fingerprint: [u8; 32],
    pub current_response_uuid: Option<Uuid>,
    pub origin_response_uuid: Uuid,
    pub after_item_uuid: Option<Uuid>,
}

#[derive(Debug)]
pub enum CompactionPublishResult {
    Published(CompactionCheckpoint),
    Conflict,
}

#[derive(QueryableByName)]
struct ConversationIdentity {
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    uuid: Uuid,
}

#[derive(QueryableByName)]
struct ActiveResponse {
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    uuid: Uuid,
}

#[derive(QueryableByName)]
struct LockedId {
    #[diesel(sql_type = BigInt)]
    _id: i64,
}

#[derive(QueryableByName)]
struct SourceRow {
    #[diesel(sql_type = Text)]
    message_type: String,
    #[diesel(sql_type = BigInt)]
    id: i64,
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    uuid: Uuid,
    #[diesel(sql_type = Timestamptz)]
    created_at: DateTime<Utc>,
    #[diesel(sql_type = Timestamptz)]
    updated_at: DateTime<Utc>,
    #[diesel(sql_type = Nullable<Integer>)]
    token_count: Option<i32>,
    #[diesel(sql_type = Nullable<Text>)]
    item_status: Option<String>,
    #[diesel(sql_type = Nullable<diesel::sql_types::Uuid>)]
    response_uuid: Option<Uuid>,
    #[diesel(sql_type = Nullable<Text>)]
    response_status: Option<String>,
    #[diesel(sql_type = Nullable<diesel::sql_types::Uuid>)]
    tool_call_id: Option<Uuid>,
    #[diesel(sql_type = Nullable<Text>)]
    tool_name: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    model: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    finish_reason: Option<String>,
    #[diesel(sql_type = BigInt)]
    encrypted_bytes: i64,
    #[diesel(sql_type = Text)]
    content_fingerprint: String,
}

impl TryFrom<SourceRow> for CompactionSourceMetadata {
    type Error = CompactionError;
    fn try_from(row: SourceRow) -> Result<Self, Self::Error> {
        Ok(Self {
            key: ContextItemKey {
                kind: ContextItemKind::from_db_type(&row.message_type)
                    .ok_or(CompactionError::InvalidSelection)?,
                uuid: row.uuid,
            },
            id: row.id,
            created_at: row.created_at,
            updated_at: row.updated_at,
            token_count: row.token_count,
            item_status: row.item_status,
            response_uuid: row.response_uuid,
            response_status: row.response_status,
            tool_call_id: row.tool_call_id,
            tool_name: row.tool_name,
            model: row.model,
            finish_reason: row.finish_reason,
            encrypted_bytes: row.encrypted_bytes,
            content_fingerprint: row.content_fingerprint,
        })
    }
}

// Ciphertext hashes avoid transferring/decrypting already-covered history. Core
// PostgreSQL sha256(bytea) needs no extension. Ownership is checked in every arm.
const SOURCE_UNION: &str = r#"
SELECT 'user'::text AS message_type, m.id, m.uuid, m.created_at, m.updated_at,
 m.prompt_tokens AS token_count, 'completed'::text AS item_status,
 r.uuid AS response_uuid, r.status::text AS response_status, NULL::uuid AS tool_call_id,
 NULL::text AS tool_name, r.model, NULL::text AS finish_reason,
 octet_length(m.content_enc)::bigint AS encrypted_bytes,
 encode(sha256(m.content_enc), 'hex') || ':' ||
 COALESCE(encode(sha256(m.attachment_text_enc), 'hex'), '-') AS content_fingerprint
FROM user_messages m LEFT JOIN responses r ON r.id=m.response_id AND r.user_id=$2 AND r.conversation_id=$1
WHERE m.conversation_id=$1 AND m.user_id=$2
UNION ALL
SELECT 'assistant', m.id, m.uuid, m.created_at, m.updated_at, m.completion_tokens, m.status,
 r.uuid, r.status::text, NULL::uuid, NULL::text, r.model, m.finish_reason,
 COALESCE(octet_length(m.content_enc),0)::bigint,
 COALESCE(encode(sha256(m.content_enc),'hex'),'-')
FROM assistant_messages m LEFT JOIN responses r ON r.id=m.response_id AND r.user_id=$2 AND r.conversation_id=$1
WHERE m.conversation_id=$1 AND m.user_id=$2
UNION ALL
SELECT 'tool_call', m.id, m.uuid, m.created_at, m.updated_at, m.argument_tokens, m.status,
 r.uuid, r.status::text, m.uuid, m.name, NULL::text, NULL::text,
 COALESCE(octet_length(m.arguments_enc),0)::bigint,
 COALESCE(encode(sha256(m.arguments_enc),'hex'),'-')
FROM tool_calls m LEFT JOIN responses r ON r.id=m.response_id AND r.user_id=$2 AND r.conversation_id=$1
WHERE m.conversation_id=$1 AND m.user_id=$2
UNION ALL
SELECT 'tool_output', m.id, m.uuid, m.created_at, m.updated_at, m.output_tokens, m.status,
 r.uuid, r.status::text, tc.uuid, tc.name, NULL::text, NULL::text,
 octet_length(m.output_enc)::bigint, encode(sha256(m.output_enc),'hex')
FROM tool_outputs m JOIN tool_calls tc ON tc.id=m.tool_call_fk AND tc.user_id=$2 AND tc.conversation_id=$1
LEFT JOIN responses r ON r.id=m.response_id AND r.user_id=$2 AND r.conversation_id=$1
WHERE m.conversation_id=$1 AND m.user_id=$2
UNION ALL
SELECT 'reasoning', m.id, m.uuid, m.created_at, m.updated_at, m.reasoning_tokens, m.status,
 r.uuid, r.status::text, NULL::uuid, NULL::text, NULL::text, NULL::text,
 COALESCE(octet_length(m.content_enc),0)::bigint,
 COALESCE(encode(sha256(m.content_enc),'hex'),'-')
FROM reasoning_items m LEFT JOIN responses r ON r.id=m.response_id AND r.user_id=$2 AND r.conversation_id=$1
WHERE m.conversation_id=$1 AND m.user_id=$2
"#;

const CHECKPOINT_COLUMNS: &str = "id, uuid, user_id, conversation_id, content_enc, content_tokens, previous_summary_id, created_at, format_version, generation, source_fingerprint, origin_response_uuid, after_item_uuid";

fn conversation_identity(
    conn: &mut PgConnection,
    conversation_id: i64,
    user_id: Uuid,
    lock: bool,
) -> Result<Uuid, CompactionError> {
    let suffix = if lock { " FOR KEY SHARE" } else { "" };
    diesel::sql_query(format!(
        "SELECT uuid FROM conversations WHERE id=$1 AND user_id=$2{suffix}"
    ))
    .bind::<BigInt, _>(conversation_id)
    .bind::<diesel::sql_types::Uuid, _>(user_id)
    .get_result::<ConversationIdentity>(conn)
    .optional()?
    .map(|row| row.uuid)
    .ok_or(CompactionError::ConversationNotFound)
}

// This fence serializes only our checkpoint publication/invalidation. Source row
// locks still validate ordinary canonical writers. Parent KEY SHARE locks block
// cascaded deletion while permitting existing non-key recency UPDATE triggers.
fn checkpoint_fence(conn: &mut PgConnection, conversation_id: i64) -> Result<(), CompactionError> {
    #[derive(QueryableByName)]
    struct FenceAcquired {
        #[diesel(sql_type = Integer)]
        _acquired: i32,
    }
    let mut digest = Sha256::new();
    digest.update(b"opensecret-responses-compaction-lock-v1");
    digest.update(conversation_id.to_be_bytes());
    let digest: [u8; 32] = digest.finalize().into();
    let key = i64::from_be_bytes(digest[..8].try_into().expect("fixed digest prefix"));
    diesel::sql_query("SELECT 1 AS _acquired FROM pg_advisory_xact_lock($1)")
        .bind::<BigInt, _>(key)
        .get_result::<FenceAcquired>(conn)?;
    Ok(())
}

fn metadata(
    conn: &mut PgConnection,
    conversation_id: i64,
    user_id: Uuid,
    keys: Option<&[ContextItemKey]>,
) -> Result<Vec<CompactionSourceMetadata>, CompactionError> {
    let order = " ORDER BY created_at ASC, id ASC, message_type ASC, uuid ASC";
    let rows = if let Some(keys) = keys {
        let uuids: Vec<_> = keys.iter().map(|key| key.uuid).collect();
        diesel::sql_query(format!(
            "SELECT * FROM ({SOURCE_UNION}) source WHERE uuid=ANY($3){order}"
        ))
        .bind::<BigInt, _>(conversation_id)
        .bind::<diesel::sql_types::Uuid, _>(user_id)
        .bind::<Array<diesel::sql_types::Uuid>, _>(uuids)
        .load::<SourceRow>(conn)?
    } else {
        diesel::sql_query(format!("SELECT * FROM ({SOURCE_UNION}) source{order}"))
            .bind::<BigInt, _>(conversation_id)
            .bind::<diesel::sql_types::Uuid, _>(user_id)
            .load::<SourceRow>(conn)?
    };
    let selected = keys.map(|keys| keys.iter().copied().collect::<HashSet<_>>());
    rows.into_iter()
        .map(CompactionSourceMetadata::try_from)
        .filter(|row| match row {
            Ok(row) => selected.as_ref().is_none_or(|keys| keys.contains(&row.key)),
            Err(_) => true,
        })
        .collect()
}

fn coverage_fingerprint(
    items: &[CompactionSourceMetadata],
    keys: &[ContextItemKey],
) -> Result<[u8; 32], CompactionError> {
    let selected: HashSet<_> = keys.iter().copied().collect();
    if selected.len() != keys.len() {
        return Err(CompactionError::InvalidSelection);
    }
    let covered: Vec<_> = items
        .iter()
        .filter(|item| selected.contains(&item.key))
        .collect();
    if covered.len() != selected.len() {
        return Err(CompactionError::InvalidSelection);
    }
    let mut digest = Sha256::new();
    digest.update(b"opensecret-responses-compaction-source-v1");
    for item in covered {
        // Lifecycle status, usage annotations and reactions do not change source
        // content. Item status and rendering relations do affect the projection.
        let data = serde_json::to_vec(&(
            item.key,
            item.id,
            item.created_at,
            &item.item_status,
            item.response_uuid,
            item.tool_call_id,
            &item.tool_name,
            &item.model,
            &item.finish_reason,
            &item.content_fingerprint,
        ))
        .map_err(|_| CompactionError::Serialization)?;
        digest.update((data.len() as u64).to_be_bytes());
        digest.update(data);
    }
    Ok(digest.finalize().into())
}

impl CompactionSnapshot {
    pub fn load(
        conn: &mut PgConnection,
        conversation_id: i64,
        user_id: Uuid,
    ) -> Result<Self, CompactionError> {
        conn.build_transaction()
            .read_only()
            .repeatable_read()
            .run(|conn| {
                let conversation_uuid =
                    conversation_identity(conn, conversation_id, user_id, false)?;
                let items = metadata(conn, conversation_id, user_id, None)?;
                let active_response_uuids = diesel::sql_query(
                    "SELECT uuid FROM responses WHERE conversation_id=$1 AND user_id=$2 \
                     AND status IN ('queued','in_progress') ORDER BY id",
                )
                .bind::<BigInt, _>(conversation_id)
                .bind::<diesel::sql_types::Uuid, _>(user_id)
                .load::<ActiveResponse>(conn)?
                .into_iter()
                .map(|row| row.uuid)
                .collect();
                let keys: Vec<_> = items.iter().map(|item| item.key).collect();
                let fingerprint = coverage_fingerprint(&items, &keys)?;
                let checkpoint = CompactionCheckpoint::head(conn, conversation_id, user_id)?;
                Ok(Self {
                    conversation_id,
                    conversation_uuid,
                    user_id,
                    items,
                    active_response_uuids,
                    fingerprint,
                    checkpoint,
                })
            })
    }

    pub fn coverage_fingerprint(
        &self,
        keys: &[ContextItemKey],
    ) -> Result<[u8; 32], CompactionError> {
        coverage_fingerprint(&self.items, keys)
    }

    /// Loads only requested ciphertext, preserving exact caller key order.
    pub fn load_items(
        conn: &mut PgConnection,
        conversation_id: i64,
        user_id: Uuid,
        keys: &[ContextItemKey],
    ) -> Result<Vec<RawThreadMessage>, CompactionError> {
        conversation_identity(conn, conversation_id, user_id, false)?;
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        if keys.iter().copied().collect::<HashSet<_>>().len() != keys.len() {
            return Err(CompactionError::InvalidSelection);
        }
        // Bind typed keys; dynamic SQL fragments below come only from our enum.
        let mut result = HashMap::new();
        for kind in [
            ContextItemKind::User,
            ContextItemKind::Assistant,
            ContextItemKind::ToolCall,
            ContextItemKind::ToolOutput,
            ContextItemKind::Reasoning,
        ] {
            let ids: Vec<_> = keys
                .iter()
                .filter(|key| key.kind == kind)
                .map(|key| key.uuid)
                .collect();
            if ids.is_empty() {
                continue;
            }
            let (content, status, tokens, call_id, name, model, finish, join) = match kind {
                ContextItemKind::User => ("m.content_enc", "'completed'::text", "m.prompt_tokens", "NULL::uuid", "NULL::text", "r.model", "NULL::text", ""),
                ContextItemKind::Assistant => ("m.content_enc", "m.status", "m.completion_tokens", "NULL::uuid", "NULL::text", "r.model", "m.finish_reason", ""),
                ContextItemKind::ToolCall => ("m.arguments_enc", "m.status", "m.argument_tokens", "m.uuid", "m.name", "NULL::text", "NULL::text", ""),
                ContextItemKind::ToolOutput => ("m.output_enc", "m.status", "m.output_tokens", "tc.uuid", "tc.name", "NULL::text", "NULL::text", " JOIN tool_calls tc ON tc.id=m.tool_call_fk AND tc.user_id=$2 AND tc.conversation_id=$1"),
                ContextItemKind::Reasoning => ("m.content_enc", "m.status", "m.reasoning_tokens", "NULL::uuid", "NULL::text", "NULL::text", "NULL::text", ""),
            };
            let query = format!("SELECT '{label}'::text AS message_type,m.id,m.uuid,r.uuid AS response_uuid,{content} AS content_enc,{status} AS status,m.created_at,{model} AS model,{tokens} AS token_count,{call_id} AS tool_call_id,{finish} AS finish_reason,{name} AS tool_name FROM {table} m{join} LEFT JOIN responses r ON r.id=m.response_id AND r.user_id=$2 AND r.conversation_id=$1 WHERE m.conversation_id=$1 AND m.user_id=$2 AND m.uuid=ANY($3)", label=kind.as_str(), table=kind.table());
            let rows = diesel::sql_query(query)
                .bind::<BigInt, _>(conversation_id)
                .bind::<diesel::sql_types::Uuid, _>(user_id)
                .bind::<Array<diesel::sql_types::Uuid>, _>(ids)
                .load::<RawThreadMessage>(conn)?;
            for row in rows {
                result.insert(
                    ContextItemKey {
                        kind,
                        uuid: row.uuid,
                    },
                    row,
                );
            }
        }
        keys.iter()
            .map(|key| result.remove(key).ok_or(CompactionError::InvalidSelection))
            .collect()
    }
}

impl CompactionCheckpoint {
    /// Generation is the stable cursor order; canonical item cursors remain
    /// independent. `limit` includes the handler's single look-ahead row.
    pub fn list_markers(
        conn: &mut PgConnection,
        conversation_id: i64,
        user_id: Uuid,
        limit: i64,
        after: Option<Uuid>,
        order: &str,
    ) -> Result<Vec<CompactionMarkerMetadata>, CompactionError> {
        if !(1..=101).contains(&limit) || !matches!(order, "asc" | "desc") {
            return Err(CompactionError::InvalidSelection);
        }
        conversation_identity(conn, conversation_id, user_id, false)?;
        #[derive(QueryableByName)]
        struct Cursor {
            #[diesel(sql_type = BigInt)]
            generation: i64,
        }
        let generation = match after {
            Some(uuid) => Some(
                diesel::sql_query("SELECT generation FROM conversation_summaries s WHERE conversation_id=$1 AND user_id=$2 AND uuid=$3 AND format_version=1 AND EXISTS (SELECT 1 FROM responses r WHERE r.uuid=s.origin_response_uuid AND r.conversation_id=s.conversation_id AND r.user_id=s.user_id)")
                    .bind::<BigInt, _>(conversation_id)
                    .bind::<diesel::sql_types::Uuid, _>(user_id)
                    .bind::<diesel::sql_types::Uuid, _>(uuid)
                    .get_result::<Cursor>(conn)
                    .optional()?
                    .ok_or(CompactionError::InvalidSelection)?
                    .generation,
            ),
            None => None,
        };
        // These SQL fragments come only from the validated order enum above.
        let (comparison, direction) = if order == "asc" {
            (">", "ASC")
        } else {
            ("<", "DESC")
        };
        diesel::sql_query(format!(
            "SELECT uuid, origin_response_uuid, after_item_uuid, created_at, generation FROM conversation_summaries s \
             WHERE conversation_id=$1 AND user_id=$2 AND format_version=1 \
             AND EXISTS (SELECT 1 FROM responses r WHERE r.uuid=s.origin_response_uuid \
                 AND r.conversation_id=s.conversation_id AND r.user_id=s.user_id) \
             AND ($3::bigint IS NULL OR generation {comparison} $3) ORDER BY generation {direction} LIMIT $4"
        ))
        .bind::<BigInt, _>(conversation_id)
        .bind::<diesel::sql_types::Uuid, _>(user_id)
        .bind::<Nullable<BigInt>, _>(generation)
        .bind::<BigInt, _>(limit)
        .load(conn)
        .map_err(CompactionError::from)
    }

    fn head(
        conn: &mut PgConnection,
        conversation_id: i64,
        user_id: Uuid,
    ) -> Result<Option<Self>, CompactionError> {
        diesel::sql_query(format!("SELECT {CHECKPOINT_COLUMNS} FROM conversation_summaries WHERE conversation_id=$1 AND user_id=$2 AND format_version=1 ORDER BY generation DESC LIMIT 1"))
            .bind::<BigInt,_>(conversation_id).bind::<diesel::sql_types::Uuid,_>(user_id)
            .get_result(conn).optional().map_err(CompactionError::from)
    }

    pub fn list_for_response(
        conn: &mut PgConnection,
        conversation_id: i64,
        user_id: Uuid,
        response_uuid: Uuid,
    ) -> Result<Vec<Self>, CompactionError> {
        conversation_identity(conn, conversation_id, user_id, false)?;
        diesel::sql_query(format!("SELECT {CHECKPOINT_COLUMNS} FROM conversation_summaries WHERE conversation_id=$1 AND user_id=$2 AND origin_response_uuid=$3 AND format_version=1 ORDER BY generation ASC"))
            .bind::<BigInt,_>(conversation_id)
            .bind::<diesel::sql_types::Uuid,_>(user_id)
            .bind::<diesel::sql_types::Uuid,_>(response_uuid)
            .load(conn).map_err(CompactionError::from)
    }

    pub fn publish(
        conn: &mut PgConnection,
        new: NewCompactionCheckpoint,
    ) -> Result<CompactionPublishResult, CompactionError> {
        if new.covered_items.is_empty()
            || new.content_enc.is_empty()
            || new.content_tokens < 0
            || new
                .covered_items
                .iter()
                .copied()
                .collect::<HashSet<_>>()
                .len()
                != new.covered_items.len()
        {
            return Err(CompactionError::InvalidCheckpoint);
        }
        conn.transaction(|conn| {
            conversation_identity(conn, new.conversation_id, new.user_id, false)?;
            checkpoint_fence(conn, new.conversation_id)?;
            // Lock FK ancestry before children. KEY SHARE permits the non-key
            // conversation recency UPDATE used by existing child-write triggers.
            diesel::sql_query("SELECT uuid FROM users WHERE uuid=$1 FOR KEY SHARE")
                .bind::<diesel::sql_types::Uuid, _>(new.user_id)
                .get_result::<ConversationIdentity>(conn)?;
            conversation_identity(conn, new.conversation_id, new.user_id, true)?;
            let head = Self::head(conn, new.conversation_id, new.user_id)?;
            if head.as_ref().map(|row| row.uuid) != new.expected_head_uuid {
                return Ok(CompactionPublishResult::Conflict);
            }
            // No provider work or decryption happens while these locks are held.
            for kind in [
                ContextItemKind::User,
                ContextItemKind::Assistant,
                ContextItemKind::ToolCall,
                ContextItemKind::ToolOutput,
                ContextItemKind::Reasoning,
            ] {
                let ids: Vec<_> = new.covered_items.iter()
                    .filter(|key| key.kind == kind)
                    .map(|key| key.uuid)
                    .collect();
                if ids.is_empty() {
                    continue;
                }
                let query = format!(
                    "SELECT id AS _id FROM {} WHERE conversation_id=$1 AND user_id=$2 \
                     AND uuid=ANY($3) ORDER BY id FOR SHARE",
                    kind.table(),
                );
                diesel::sql_query(query)
                    .bind::<BigInt, _>(new.conversation_id)
                    .bind::<diesel::sql_types::Uuid, _>(new.user_id)
                    .bind::<Array<diesel::sql_types::Uuid>, _>(ids)
                    .load::<LockedId>(conn)?;
            }
            let before = metadata(conn, new.conversation_id, new.user_id, Some(&new.covered_items))?;
            let response_ids: Vec<_> = before.iter().filter_map(|row| row.response_uuid).collect();
            if !response_ids.is_empty() {
                diesel::sql_query(
                    "SELECT id AS _id FROM responses WHERE conversation_id=$1 AND user_id=$2 \
                     AND uuid=ANY($3) ORDER BY id FOR SHARE",
                )
                .bind::<BigInt, _>(new.conversation_id)
                .bind::<diesel::sql_types::Uuid, _>(new.user_id)
                .bind::<Array<diesel::sql_types::Uuid>, _>(response_ids)
                .load::<LockedId>(conn)?;
            }
            let items = metadata(conn, new.conversation_id, new.user_id, Some(&new.covered_items))?;
            let previous: HashSet<_> = new.previous_covered_items.iter().copied().collect();
            let covered: HashSet<_> = new.covered_items.iter().copied().collect();
            let previous_valid = match &head {
                Some(head) => {
                    previous.is_subset(&covered)
                        && coverage_fingerprint(&items, &new.previous_covered_items)
                            .is_ok_and(|digest| digest.as_slice() == head.source_fingerprint)
                }
                None => new.previous_covered_items.is_empty(),
            };
            if !previous_valid
                || coverage_fingerprint(&items, &new.covered_items).ok() != Some(new.source_fingerprint)
                || items.iter().any(|row| {
                    let frozen_previous = previous.contains(&row.key)
                        && (row.item_status.as_deref() == Some("completed")
                            || row.is_immutable_historical());
                    !frozen_previous
                        && !row.is_settled_for(new.current_response_uuid, &[])
                })
            {
                return Ok(CompactionPublishResult::Conflict);
            }
            let from = items.first().ok_or(CompactionError::InvalidCheckpoint)?.created_at;
            let to = items.last().ok_or(CompactionError::InvalidCheckpoint)?.created_at;
            let count = i32::try_from(items.len()).map_err(|_| CompactionError::InvalidCheckpoint)?;
            let generation = match &head {
                Some(row) => row.generation.checked_add(1).ok_or(CompactionError::InvalidCheckpoint)?,
                None => 1,
            };
            let previous = head.as_ref().map(|row| row.id);
            let query = format!(
                "INSERT INTO conversation_summaries (uuid,user_id,conversation_id,from_created_at,\
                 to_created_at,message_count,content_enc,content_tokens,previous_summary_id,\
                 format_version,generation,source_fingerprint,origin_response_uuid,after_item_uuid) \
                 VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,1,$10,$11,$12,$13) RETURNING {CHECKPOINT_COLUMNS}",
            );
            let row = diesel::sql_query(query)
                .bind::<diesel::sql_types::Uuid, _>(new.uuid)
                .bind::<diesel::sql_types::Uuid, _>(new.user_id)
                .bind::<BigInt, _>(new.conversation_id)
                .bind::<Timestamptz, _>(from)
                .bind::<Timestamptz, _>(to)
                .bind::<Integer, _>(count)
                .bind::<Binary, _>(new.content_enc)
                .bind::<Integer, _>(new.content_tokens)
                .bind::<Nullable<BigInt>, _>(previous)
                .bind::<BigInt, _>(generation)
                .bind::<Binary, _>(new.source_fingerprint.to_vec())
                .bind::<diesel::sql_types::Uuid, _>(new.origin_response_uuid)
                .bind::<Nullable<diesel::sql_types::Uuid>, _>(new.after_item_uuid)
                .get_result(conn)?;
            Ok(CompactionPublishResult::Published(row))
        })
    }

    /// Must be called in the same fenced transaction as destructive source edits.
    pub fn invalidate(
        conn: &mut PgConnection,
        conversation_id: i64,
        user_id: Uuid,
    ) -> Result<(), CompactionError> {
        conn.transaction(|conn| {
            conversation_identity(conn, conversation_id, user_id, false)?;
            checkpoint_fence(conn, conversation_id)?;
            diesel::sql_query("DELETE FROM conversation_summaries WHERE conversation_id=$1 AND user_id=$2 AND format_version=1")
                .bind::<BigInt,_>(conversation_id).bind::<diesel::sql_types::Uuid,_>(user_id).execute(conn)?;
            Ok(())
        })
    }

    pub fn delete_response(
        conn: &mut PgConnection,
        uuid: Uuid,
        user_id: Uuid,
    ) -> Result<(), CompactionError> {
        conn.transaction(|conn| {
            let response = Response::get_by_uuid_and_user(conn, uuid, user_id)?;
            Self::invalidate(conn, response.conversation_id, user_id).map_err(
                |error| match error {
                    CompactionError::ConversationNotFound => {
                        CompactionError::Responses(ResponsesError::ResponseNotFound)
                    }
                    error => error,
                },
            )?;
            Response::delete_by_uuid_and_user(conn, uuid, user_id)?;
            Ok(())
        })
    }

    pub fn delete_user_message(
        conn: &mut PgConnection,
        uuid: Uuid,
        user_id: Uuid,
    ) -> Result<(), CompactionError> {
        conn.transaction(|conn| {
            let message = UserMessage::get_by_uuid_and_user(conn, uuid, user_id)?;
            Self::invalidate(conn, message.conversation_id, user_id).map_err(
                |error| match error {
                    CompactionError::ConversationNotFound => {
                        CompactionError::Responses(ResponsesError::UserMessageNotFound)
                    }
                    error => error,
                },
            )?;
            UserMessage::delete_by_id_and_user(conn, message.id, user_id)?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests;
