//! Server-owned conversation checkpoints. Planning is pure, helper inference is
//! explicit paid work, and a fenced DB publication is the only coverage commit.

mod attempt;
pub(crate) mod context;
mod executor;
pub(crate) mod policy;
mod source;
mod summary;
pub(super) mod telemetry;

use crate::{
    billing::{BillingError, ChatBillingAccess},
    db::DBError,
    encrypt::{decrypt_aead_v1, derive_key, encrypt_aead_v1},
    model_config::ReasoningReplay,
    models::response_compaction::{
        CompactionCheckpoint, CompactionError, CompactionPublishResult, CompactionSnapshot,
        NewCompactionCheckpoint,
    },
    tokens::{count_tokens, model_max_ctx},
    web::responses::{
        context_builder::{
            load_instruction_messages, normalize_tool_call_ids_for_model, prompt_source_keys,
            snapshot_from_metadata, snapshot_from_raw, snapshot_from_selected,
        },
        context_observability::ContextMetrics,
        handlers::ResponsesCreateRequest,
    },
    ApiError,
};
use attempt::{AttemptKey, SourceRaceAction, SourceRevision, StagedCheckpoint};
use base64::{engine::general_purpose::STANDARD, Engine};
use context::{CheckpointPayload, ContextItem, ContextItemKey, ContextItemKind};
use executor::SummaryExecutionError;
pub(super) use executor::{is_context_rejection, SummaryExecutor};
use policy::CompactionPolicy;
use secp256k1::SecretKey;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};
use tracing::Instrument;
use uuid::Uuid;

const MAX_SUMMARY_CALLS_PER_RESPONSE: usize = 32;
const MAX_PUBLICATION_CONFLICTS: usize = 2;
const MAX_SOURCE_RACE_RETRIES: usize = 2;
const MAX_PREPARATION_STEPS: usize =
    MAX_SUMMARY_CALLS_PER_RESPONSE + MAX_PUBLICATION_CONFLICTS + MAX_SOURCE_RACE_RETRIES + 2;
const ENVELOPE_VERSION: u32 = 1;

/// One allowance for the entire logical Response, including all tool turns and
/// checkpoint CAS conflicts. It never changes the main generation allowance.
#[derive(Default)]
pub(super) struct CompactionBudget {
    metrics: Arc<ContextMetrics>,
    calls: AtomicUsize,
    paid_work: AtomicBool,
    sources: Mutex<source::SourceLoader>,
    non_reducing: Mutex<HashSet<AttemptKey>>,
    advisory_failed: AtomicBool,
}

impl CompactionBudget {
    pub(super) fn metrics(&self) -> Arc<ContextMetrics> {
        self.metrics.clone()
    }

    fn begin_call(&self) -> Result<(), ApiError> {
        self.calls
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |calls| {
                (calls < MAX_SUMMARY_CALLS_PER_RESPONSE).then_some(calls + 1)
            })
            .map_err(|_| {
                tracing::warn!(
                    event = "responses.compaction.helper_budget_exhausted",
                    max_helper_calls = MAX_SUMMARY_CALLS_PER_RESPONSE,
                    "Responses compaction helper budget exhausted"
                );
                ApiError::ServiceUnavailable
            })?;
        self.paid_work.store(true, Ordering::Release);
        self.metrics.helper_call_started();
        Ok(())
    }

    pub fn has_paid_work(&self) -> bool {
        self.paid_work.load(Ordering::Acquire)
    }

    fn optional_work_enabled(&self, forced: bool) -> bool {
        forced || !self.advisory_failed.load(Ordering::Acquire)
    }

    fn suppress_optional_work(&self) {
        self.advisory_failed.store(true, Ordering::Release);
    }

    fn may_retry_candidate(&self, key: &AttemptKey, forced: bool) -> Result<bool, ApiError> {
        Ok(forced
            || !self
                .non_reducing
                .lock()
                .map_err(|_| ApiError::InternalServerError)?
                .contains(key))
    }

    fn remember_non_reducing(&self, key: AttemptKey) -> Result<(), ApiError> {
        let mut attempts = self
            .non_reducing
            .lock()
            .map_err(|_| ApiError::InternalServerError)?;
        // Each entry requires at least one paid helper call. Keep the memory
        // bound explicit even if execution policy changes independently later.
        if attempts.len() < MAX_SUMMARY_CALLS_PER_RESPONSE {
            attempts.insert(key);
        }
        Ok(())
    }
}

/// OpenAI-shaped opt-in controls. Omission uses the server's automatic policy;
/// an explicit empty list disables compaction and returns an overflow instead
/// of silently discarding history.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContextManagement {
    #[serde(rename = "compaction")]
    Compaction { compact_threshold: Option<usize> },
}

pub(super) fn policy_for(
    body: &ResponsesCreateRequest,
) -> Result<(CompactionPolicy, bool), ApiError> {
    let window = model_max_ctx(&body.model);
    let output = body
        .max_output_tokens
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(0);
    if output > window {
        return Err(ApiError::MessageExceedsContextLimit);
    }
    let mut policy = CompactionPolicy::from_context_window(window, output, 2_048.min(window / 16));
    let enabled = match body.context_management.as_deref() {
        None => true,
        Some([]) => false,
        Some([ContextManagement::Compaction { compact_threshold }]) => {
            if let Some(threshold) = compact_threshold {
                if *threshold == 0 || *threshold > window {
                    return Err(ApiError::BadRequest);
                }
                policy.trigger_tokens = (*threshold).min(policy.input_budget);
            }
            true
        }
        Some(_) => return Err(ApiError::BadRequest),
    };
    Ok((policy, enabled))
}

#[derive(Debug, Clone)]
pub struct CompactionOutput {
    pub id: Uuid,
    pub encrypted_content: String,
    pub after_item_uuid: Option<Uuid>,
}

impl From<&CompactionCheckpoint> for CompactionOutput {
    fn from(record: &CompactionCheckpoint) -> Self {
        Self {
            id: record.uuid,
            encrypted_content: STANDARD.encode(&record.content_enc),
            after_item_uuid: record.after_item_uuid,
        }
    }
}

impl CompactionOutput {
    pub(super) fn as_output_item(&self) -> super::handlers::OutputItem {
        super::handlers::OutputItem {
            output_type: "compaction".to_owned(),
            id: self.id.to_string(),
            status: String::new(),
            encrypted_content: Some(self.encrypted_content.clone()),
            role: None,
            content: None,
            call_id: None,
            name: None,
            arguments: None,
            output: None,
        }
    }
}

pub(super) struct PreparedPrompt {
    pub messages: Vec<Value>,
    pub estimated_tokens: usize,
    pub compactions: Vec<CompactionOutput>,
    pub allowed_urls: HashSet<String>,
}

#[derive(Serialize, Deserialize)]
struct CheckpointEnvelope {
    version: u32,
    checkpoint_uuid: Uuid,
    user_id: Uuid,
    conversation_uuid: Uuid,
    origin_response_uuid: Uuid,
    after_item_uuid: Option<Uuid>,
    source_fingerprint: [u8; 32],
    allowed_urls: Vec<String>,
    payload: CheckpointPayload,
}

fn checkpoint_aad(user: Uuid, conversation: Uuid, checkpoint: Uuid) -> Vec<u8> {
    let mut bytes = b"opensecret-responses-compaction-v1\0".to_vec();
    bytes.extend(user.as_bytes());
    bytes.extend(conversation.as_bytes());
    bytes.extend(checkpoint.as_bytes());
    bytes
}

fn covered_keys(payload: &CheckpointPayload) -> Vec<ContextItemKey> {
    payload
        .history
        .iter()
        .chain(payload.active.iter())
        .flat_map(|section| section.covered.iter().copied())
        .collect()
}

fn open_checkpoint(
    source: &CompactionSnapshot,
    key: &SecretKey,
) -> Result<Option<CheckpointEnvelope>, ApiError> {
    let Some(record) = &source.checkpoint else {
        return Ok(None);
    };
    let key = derive_key(&key.secret_bytes(), b"responses-compaction-v1")
        .map_err(|_| checkpoint_error("key_derivation"))?;
    let bytes = decrypt_aead_v1(
        &key,
        &record.content_enc,
        &checkpoint_aad(source.user_id, source.conversation_uuid, record.uuid),
    )
    .map_err(|_| checkpoint_error("authentication"))?;
    let envelope: CheckpointEnvelope =
        serde_json::from_slice(&bytes).map_err(|_| checkpoint_error("envelope_decode"))?;
    let digest = source
        .coverage_fingerprint(&covered_keys(&envelope.payload))
        .map_err(|_| checkpoint_error("source_coverage"))?;
    if record.format_version != ENVELOPE_VERSION as i32
        || envelope.version != ENVELOPE_VERSION
        || envelope.checkpoint_uuid != record.uuid
        || envelope.user_id != source.user_id
        || record.user_id != source.user_id
        || envelope.conversation_uuid != source.conversation_uuid
        || record.conversation_id != source.conversation_id
        || envelope.origin_response_uuid != record.origin_response_uuid
        || envelope.after_item_uuid != record.after_item_uuid
        || envelope.source_fingerprint != digest
        || record.source_fingerprint != digest
    {
        return Err(checkpoint_error("identity_version_or_fingerprint"));
    }
    if envelope.allowed_urls.len() > super::tools::MAX_PROMPT_ALLOWED_URLS {
        return Err(checkpoint_error("url_authority_capacity"));
    }
    Ok(Some(envelope))
}

/// Categories are implementation literals, never encryption errors or the
/// authenticated envelope. Even a corrupt record must not reach log formatting.
fn checkpoint_error(category: &'static str) -> ApiError {
    tracing::warn!(
        event = "responses.compaction.checkpoint_rejected",
        error_category = category,
        "Responses compaction checkpoint verification failed"
    );
    ApiError::InternalServerError
}

/// URL authority comes from canonical user/tool data, never generated summary
/// text. The authenticated checkpoint carries only previously established URLs.
fn authoritative_urls(
    snapshot: &context::ContextSnapshot,
    previous: Option<&CheckpointEnvelope>,
) -> HashSet<String> {
    let raw: Vec<_> = snapshot
        .items
        .iter()
        .flat_map(|item| item.prompt_messages.iter().cloned())
        .collect();
    let mut urls = super::tools::collect_kagi_allowed_urls_from_prompt(&raw);
    if let Some(previous) = previous {
        for url in &previous.allowed_urls {
            if urls.len() >= super::tools::MAX_PROMPT_ALLOWED_URLS {
                break;
            }
            urls.insert(url.clone());
        }
    }
    urls
}

fn durable_urls(
    snapshot: &context::ContextSnapshot,
    previous: Option<&CheckpointEnvelope>,
    covered: &[ContextItemKey],
) -> Vec<String> {
    let covered: HashSet<_> = covered.iter().copied().collect();
    let raw: Vec<_> = snapshot
        .items
        .iter()
        .filter(|item| covered.contains(&item.key))
        .flat_map(|item| item.prompt_messages.iter().cloned())
        .collect();
    let mut urls = super::tools::collect_kagi_allowed_urls_from_prompt(&raw);
    if let Some(previous) = previous {
        for url in &previous.allowed_urls {
            if urls.len() >= super::tools::MAX_PROMPT_ALLOWED_URLS {
                break;
            }
            urls.insert(url.clone());
        }
    }
    let mut urls: Vec<_> = urls.into_iter().collect();
    urls.sort();
    urls
}

fn validate_required_input(
    snapshot: &context::ContextSnapshot,
    policy: &CompactionPolicy,
) -> Result<(), ApiError> {
    let required = snapshot
        .items
        .iter()
        .filter(|item| {
            item.key == snapshot.current_user
                || (item.protected_input && item.response_uuid == Some(snapshot.current_response))
        })
        .map(|item| item.estimated_tokens)
        .fold(snapshot.instruction_tokens, usize::saturating_add);
    if required > policy.input_budget {
        tracing::warn!(
            event = "responses.compaction.required_input_exceeds_budget",
            required_tokens = required,
            input_budget = policy.input_budget,
            "Responses required input exceeds prompt budget"
        );
        return Err(ApiError::MessageExceedsContextLimit);
    }
    Ok(())
}

fn validate_billing_before_helpers(
    access: Option<ChatBillingAccess>,
    prompt_tokens: usize,
) -> Result<(), ApiError> {
    match access
        .map(|access| access.check_with_tokens(i32::try_from(prompt_tokens).unwrap_or(i32::MAX)))
    {
        Some(Err(BillingError::UsageLimitExceeded)) => Err(ApiError::UsageLimitReached),
        Some(Err(BillingError::FreeTokenLimitExceeded)) => Err(ApiError::FreeTokenLimitExceeded),
        _ => Ok(()),
    }
}

fn may_attempt_uncompacted_fallback(
    error: &SummaryExecutionError,
    forced: bool,
    cancelled: bool,
    expired: bool,
) -> bool {
    !forced && !cancelled && !expired && error.can_fallback_to_uncompacted()
}

fn may_use_original_prompt(
    error: &SummaryExecutionError,
    forced: bool,
    original_tokens: usize,
    input_budget: usize,
    cancelled: bool,
    expired: bool,
) -> bool {
    original_tokens <= input_budget
        && may_attempt_uncompacted_fallback(error, forced, cancelled, expired)
}

fn validate_forced_progress(
    forced: bool,
    progressed: bool,
    non_reducing: bool,
) -> Result<(), ApiError> {
    if forced && !progressed {
        return Err(if non_reducing {
            ApiError::ServiceUnavailable
        } else {
            ApiError::MessageExceedsContextLimit
        });
    }
    Ok(())
}

struct CandidateCheckpoint {
    payload: CheckpointPayload,
    source_tokens: Vec<(ContextItemKey, usize)>,
    attempt: AttemptKey,
}

/// Summarize one bounded prefix. A non-reducing candidate may be carried only
/// in memory into the next bounded batch; it is never published on its own.
async fn summarize_checkpoint(
    executor: &SummaryExecutor<'_>,
    checkpoint: &Option<CheckpointPayload>,
    policy: &CompactionPolicy,
    plan: context::ContextPlan,
    attempt: Option<AttemptKey>,
) -> Result<Option<CandidateCheckpoint>, SummaryExecutionError> {
    let Some(work) = plan.summary_work.into_iter().next() else {
        return Ok(None);
    };
    let attempt = attempt.ok_or(SummaryExecutionError::Unsafe(ApiError::InternalServerError))?;
    let section = match work.kind {
        context::SectionKind::History => "history",
        context::SectionKind::Active => "active",
    };
    tracing::info!(
        event = "responses.compaction.candidate_started",
        section,
        source_items = work.items.len(),
        covered_items = work.covered.len(),
        folds_previous_active = work.folds_previous_active,
        has_previous_summary = work.previous_summary.is_some(),
        "Responses compaction candidate started"
    );
    let summary = executor.summarize(&work, policy).await?;
    complete_candidate(checkpoint, work, summary, attempt).map(Some)
}

fn complete_candidate(
    checkpoint: &Option<CheckpointPayload>,
    work: context::SummaryWork,
    summary: String,
    attempt: AttemptKey,
) -> Result<CandidateCheckpoint, SummaryExecutionError> {
    let source_tokens = work
        .items
        .iter()
        .map(|item| (item.key, item.estimated_tokens))
        .collect();
    let one = context::ContextPlan {
        summary_work: vec![work],
    };
    let payload = context::complete_plan(checkpoint, &one, &[summary])
        .map_err(|_| SummaryExecutionError::Unsafe(ApiError::InternalServerError))?;
    Ok(CandidateCheckpoint {
        payload,
        source_tokens,
        attempt,
    })
}

fn attempt_key(
    source: &CompactionSnapshot,
    covered: &[ContextItemKey],
    model: &str,
    replay: ReasoningReplay,
) -> Result<AttemptKey, ApiError> {
    Ok(AttemptKey::new(
        source.checkpoint.as_ref().map(|record| record.uuid),
        source
            .coverage_fingerprint(covered)
            .map_err(|_| ApiError::InternalServerError)?,
        model,
        replay,
    ))
}

fn candidate_urls(
    snapshot: &context::ContextSnapshot,
    opened: Option<&CheckpointEnvelope>,
    covered: &[ContextItemKey],
    staged: Option<&StagedCheckpoint>,
) -> Vec<String> {
    let mut urls = staged
        .map(|draft| draft.allowed_urls.clone())
        .unwrap_or_default();
    let mut seen: HashSet<_> = urls.iter().cloned().collect();
    for url in durable_urls(snapshot, opened, covered) {
        if urls.len() >= super::tools::MAX_PROMPT_ALLOWED_URLS {
            break;
        }
        if seen.insert(url.clone()) {
            urls.push(url);
        }
    }
    urls
}

fn finish_prompt(
    executor: &SummaryExecutor<'_>,
    conversation_id: i64,
    current_response_uuid: Uuid,
    mut messages: Vec<Value>,
    estimated_tokens: usize,
    allowed_urls: HashSet<String>,
) -> Result<PreparedPrompt, ApiError> {
    normalize_tool_call_ids_for_model(&mut messages, &executor.body.model);
    let compactions = executor
        .state
        .db
        .list_response_compactions(conversation_id, executor.user.uuid, current_response_uuid)
        .map_err(|_| ApiError::InternalServerError)?
        .iter()
        .map(CompactionOutput::from)
        .collect();
    Ok(PreparedPrompt {
        messages,
        estimated_tokens,
        compactions,
        allowed_urls,
    })
}

/// Metadata and request-owned input remain available while individual batches
/// are hydrated. Only `complete=true` can produce a provider-ready projection.
struct PromptSource<'a> {
    source: &'a CompactionSnapshot,
    user_key: &'a SecretKey,
    model: &'a str,
    reasoning_replay: ReasoningReplay,
    current_user: ContextItemKey,
    current_response: Uuid,
    instructions: Vec<Value>,
    instruction_tokens: usize,
    incoming: &'a [ContextItem],
}

impl PromptSource<'_> {
    fn build(
        &self,
        raw: Vec<crate::models::responses::RawThreadMessage>,
        checkpoint: &Option<CheckpointPayload>,
        complete: bool,
    ) -> Result<context::ContextSnapshot, ApiError> {
        if !complete && raw.is_empty() {
            let mut snapshot = snapshot_from_metadata(
                self.instructions.clone(),
                self.instruction_tokens,
                &self.source.items,
                self.model,
                self.reasoning_replay,
                self.current_user,
                self.current_response,
                &self.source.active_response_uuids,
                checkpoint,
            )?;
            snapshot.items.extend(self.incoming.iter().cloned());
            return Ok(snapshot);
        }
        let builder = if complete {
            snapshot_from_raw
        } else {
            snapshot_from_selected
        };
        let mut snapshot = builder(
            self.instructions.clone(),
            self.instruction_tokens,
            &self.source.items,
            raw,
            self.user_key,
            self.model,
            self.reasoning_replay,
            self.current_user,
            self.current_response,
            &self.source.active_response_uuids,
            checkpoint,
        )?;
        snapshot.items.extend(self.incoming.iter().cloned());
        Ok(snapshot)
    }
}

fn load_source_items(
    executor: &SummaryExecutor<'_>,
    source: &CompactionSnapshot,
    keys: &[ContextItemKey],
) -> Result<Vec<crate::models::responses::RawThreadMessage>, DBError> {
    executor
        .budget
        .sources
        .lock()
        .map_err(|_| DBError::CompactionError(CompactionError::InvalidSelection))?
        .load(executor.state.db.as_ref(), source, keys)
}

/// Both initial and post-tool starts use this preparation service. `incoming`
/// is supplied only before the initial user/image items have been persisted.
#[allow(clippy::too_many_arguments)]
pub(super) async fn prepare_prompt(
    executor: &SummaryExecutor<'_>,
    conversation_id: i64,
    user_key: &SecretKey,
    current_user_uuid: Uuid,
    current_response_uuid: Uuid,
    internal_instructions: &str,
    reasoning_replay: ReasoningReplay,
    incoming: &[ContextItem],
    billing_access: Option<ChatBillingAccess>,
    force: bool,
) -> Result<PreparedPrompt, ApiError> {
    let started = std::time::Instant::now();
    let preparation = executor.budget.metrics.start_preparation(force);
    let span = tracing::info_span!(
        "responses_compaction",
        user_uuid = %executor.user.uuid,
        conversation_id,
        response_uuid = %current_response_uuid,
        model_id = telemetry::model_id(&executor.body.model),
        forced = force,
    );
    async {
        tracing::debug!(
            event = "responses.compaction.prepare_started",
            incoming_items = incoming.len(),
            helper_calls = executor.budget.calls.load(Ordering::Acquire),
            "Responses prompt preparation started"
        );
        let result = prepare_prompt_inner(
            executor,
            conversation_id,
            user_key,
            current_user_uuid,
            current_response_uuid,
            internal_instructions,
            reasoning_replay,
            incoming,
            billing_access,
            force,
        )
        .await;
        match &result {
            Ok(prompt) => tracing::info!(
                event = "responses.compaction.prepare_completed",
                prompt_messages = prompt.messages.len(),
                estimated_tokens = prompt.estimated_tokens,
                response_checkpoints = prompt.compactions.len(),
                helper_calls = executor.budget.calls.load(Ordering::Acquire),
                elapsed_ms = telemetry::elapsed_ms(started),
                "Responses prompt preparation completed"
            ),
            Err(error) => tracing::warn!(
                event = "responses.compaction.prepare_failed",
                error_category = telemetry::api_error_category(error),
                helper_calls = executor.budget.calls.load(Ordering::Acquire),
                elapsed_ms = telemetry::elapsed_ms(started),
                "Responses prompt preparation failed"
            ),
        }
        preparation.finish(result.is_err());
        result
    }
    .instrument(span)
    .await
}

#[allow(clippy::too_many_arguments)]
async fn prepare_prompt_inner(
    executor: &SummaryExecutor<'_>,
    conversation_id: i64,
    user_key: &SecretKey,
    current_user_uuid: Uuid,
    current_response_uuid: Uuid,
    internal_instructions: &str,
    reasoning_replay: ReasoningReplay,
    incoming: &[ContextItem],
    billing_access: Option<ChatBillingAccess>,
    mut force: bool,
) -> Result<PreparedPrompt, ApiError> {
    let (mut policy, enabled) = policy_for(executor.body)?;
    if force {
        policy.keep_recent_tokens = 0;
    }
    tracing::debug!(
        event = "responses.compaction.policy",
        enabled,
        trigger_tokens = policy.trigger_tokens,
        input_budget = policy.input_budget,
        keep_recent_tokens = policy.keep_recent_tokens,
        summary_input_budget = policy.summary_input_budget,
        "Responses compaction policy selected"
    );
    let mut publication_conflicts = 0;
    let mut source_races = 0;
    let mut non_reducing = false;
    let mut staged: Option<StagedCheckpoint> = None;
    let mut fallback_only = false;
    let mut made_progress = false;
    for preparation_step in 0..MAX_PREPARATION_STEPS {
        if executor.execution.is_cancelled() || tokio::time::Instant::now() >= executor.deadline {
            tracing::warn!(
                event = "responses.compaction.preparation_stopped",
                reason = if executor.execution.is_cancelled() {
                    "cancelled"
                } else {
                    "deadline"
                },
                preparation_step,
                "Responses compaction preparation stopped"
            );
            return Err(ApiError::ServiceUnavailable);
        }
        let source_started = std::time::Instant::now();
        let source = executor
            .state
            .db
            .load_compaction_snapshot(conversation_id, executor.user.uuid)
            .map_err(|error| {
                tracing::warn!(
                    event = "responses.compaction.metadata_failed",
                    error_category = telemetry::db_error_category(&error),
                    preparation_step,
                    elapsed_ms = telemetry::elapsed_ms(source_started),
                    "Responses compaction source metadata read failed"
                );
                ApiError::InternalServerError
            })?;
        tracing::debug!(
            event = "responses.compaction.metadata_loaded",
            conversation_uuid = %source.conversation_uuid,
            preparation_step,
            source_items = source.items.len(),
            source_ciphertext_bytes = source.items.iter().map(|item| u64::try_from(item.encrypted_bytes.max(0)).unwrap_or(u64::MAX)).fold(0u64, u64::saturating_add),
            active_responses = source.active_response_uuids.len(),
            has_checkpoint = source.checkpoint.is_some(),
            elapsed_ms = telemetry::elapsed_ms(source_started),
            "Responses compaction source metadata read completed"
        );
        let checkpoint_started = std::time::Instant::now();
        let opened = open_checkpoint(&source, user_key)?;
        let durable_checkpoint = opened.as_ref().map(|envelope| envelope.payload.clone());
        let revision = SourceRevision::new(&source);
        if staged
            .as_ref()
            .is_some_and(|draft| !draft.matches(&revision))
        {
            staged = None;
        }
        let checkpoint = staged
            .as_ref()
            .map(|draft| draft.payload.clone())
            .or_else(|| durable_checkpoint.clone());
        let covered: HashSet<_> = checkpoint
            .as_ref()
            .map(covered_keys)
            .unwrap_or_default()
            .into_iter()
            .collect();
        let uncovered: HashSet<_> = source
            .items
            .iter()
            .map(|item| item.key)
            .filter(|key| !covered.contains(key))
            .collect();
        if let Some(record) = &source.checkpoint {
            tracing::info!(
                event = "responses.compaction.checkpoint_verified",
                checkpoint_uuid = %record.uuid,
                checkpoint_origin_response_uuid = %record.origin_response_uuid,
                generation = record.generation,
                covered_items = covered.len(),
                uncovered_items = uncovered.len(),
                summary_tokens = record.content_tokens,
                elapsed_ms = telemetry::elapsed_ms(checkpoint_started),
                "Responses compaction checkpoint verified for recall"
            );
        }
        let (instructions, tokens) = load_instruction_messages(
            executor.state.db.as_ref(),
            conversation_id,
            executor.user.uuid,
            user_key,
            executor.body.instructions.as_deref(),
            Some(internal_instructions),
        )?;
        let input = PromptSource {
            source: &source,
            user_key,
            model: &executor.body.model,
            reasoning_replay,
            current_user: ContextItemKey {
                kind: ContextItemKind::User,
                uuid: current_user_uuid,
            },
            current_response: current_response_uuid,
            instructions,
            instruction_tokens: tokens,
            incoming,
        };
        // Validate fresh metadata before reuse. This state belongs to one logical
        // Response, including its initial preparation, retries and tool steps.
        let cached = {
            let mut loader = executor
                .budget
                .sources
                .lock()
                .map_err(|_| ApiError::InternalServerError)?;
            loader.retain(&source, &uncovered);
            loader.cached(&source)
        };
        let mut snapshot = input.build(cached, &checkpoint, false)?;
        if let Some(draft) = &staged {
            draft.restore_source_tokens(&mut snapshot);
        }
        let planning_tokens = context::estimate_tokens(&snapshot, &checkpoint).ok();
        let plan = if enabled && !fallback_only && executor.budget.optional_work_enabled(force) {
            context::plan_batch(
                &snapshot,
                &checkpoint,
                &policy,
                force,
                policy.summary_input_budget,
            )
            .map_err(|_| ApiError::InternalServerError)?
        } else {
            context::ContextPlan {
                summary_work: Vec::new(),
            }
        };
        let mut planned_work = Vec::new();
        let mut selected_attempt = None;
        for work in plan.summary_work {
            let needs_history_fold = work.kind == context::SectionKind::Active
                && checkpoint
                    .as_ref()
                    .and_then(|value| value.active.as_ref())
                    .is_some_and(|section| section.anchor != Some(snapshot.current_user));
            let key = attempt_key(
                &source,
                &work.covered,
                &executor.body.model,
                reasoning_replay,
            )?;
            if !needs_history_fold && executor.budget.may_retry_candidate(&key, force)? {
                selected_attempt = Some(key);
                planned_work.push(work);
                // Hydrate only this bounded batch, never both a historical
                // image batch and an active batch at the same time.
                break;
            }
        }
        let plan = context::ContextPlan {
            summary_work: planned_work,
        };
        let planned_items = plan
            .summary_work
            .iter()
            .map(|work| work.items.len())
            .sum::<usize>();
        let planned_source_bytes = plan
            .summary_work
            .iter()
            .flat_map(|work| &work.items)
            .map(|item| item.source_bytes)
            .fold(0usize, usize::saturating_add);
        if plan.summary_work.is_empty() {
            tracing::debug!(
                event = "responses.compaction.plan_noop",
                reason = if !enabled {
                    "disabled"
                } else if !force
                    && planning_tokens.is_some_and(|tokens| tokens < policy.trigger_tokens)
                {
                    "below_threshold"
                } else if non_reducing {
                    "no_reducing_candidate"
                } else {
                    "no_eligible_prefix"
                },
                estimated_tokens = planning_tokens.unwrap_or_default(),
                estimate_available = planning_tokens.is_some(),
                trigger_tokens = policy.trigger_tokens,
                input_budget = policy.input_budget,
                source_items = snapshot.items.len(),
                "Responses compaction plan requires no helper work"
            );
        } else {
            tracing::info!(
                event = "responses.compaction.plan_ready",
                estimated_tokens = planning_tokens.unwrap_or_default(),
                estimate_available = planning_tokens.is_some(),
                trigger_tokens = policy.trigger_tokens,
                input_budget = policy.input_budget,
                summary_input_budget = policy.summary_input_budget,
                planned_batches = plan.summary_work.len(),
                planned_items,
                planned_source_bytes,
                forced = force,
                preparation_step,
                "Responses compaction plan ready"
            );
        }
        // Fetch only the planned batches and required current input. The raw
        // suffix is loaded only once we have a complete fitting main prompt.
        let selected: HashSet<_> = plan
            .summary_work
            .iter()
            .flat_map(|work| {
                work.items
                    .iter()
                    .filter(|item| item.key.kind != ContextItemKind::Reasoning)
                    .map(|item| item.key)
            })
            .chain(
                snapshot
                    .items
                    .iter()
                    .filter(|item| {
                        !plan.summary_work.is_empty()
                            && (item.key == snapshot.current_user
                                || (item.protected_input
                                    && item.response_uuid == Some(current_response_uuid)))
                    })
                    .map(|item| item.key),
            )
            .collect();
        let keys: Vec<_> = source
            .items
            .iter()
            .map(|item| item.key)
            .filter(|key| uncovered.contains(key) && selected.contains(key))
            .collect();
        let raw = match load_source_items(executor, &source, &keys) {
            Ok(raw) => raw,
            Err(DBError::CompactionError(CompactionError::InvalidSelection)) => {
                match attempt::source_race_action(
                    &mut source_races,
                    MAX_SOURCE_RACE_RETRIES,
                    force,
                    fallback_only,
                ) {
                    SourceRaceAction::Replan => {}
                    SourceRaceAction::FreshFallback => {
                        fallback_only = true;
                        staged = None;
                    }
                    SourceRaceAction::Fail => return Err(ApiError::InternalServerError),
                }
                tracing::warn!(
                    event = "responses.compaction.source_replan",
                    phase = "planned_batch",
                    source_races,
                    "Responses source changed during payload read; replanning"
                );
                continue;
            }
            Err(_) => return Err(ApiError::InternalServerError),
        };
        // Keep cached suffix estimates without asking the DB for that suffix.
        let mut raw_by_key = executor
            .budget
            .sources
            .lock()
            .map_err(|_| ApiError::InternalServerError)?
            .cached(&source)
            .into_iter()
            .map(|row| ((row.message_type.clone(), row.uuid), row))
            .collect::<std::collections::HashMap<_, _>>();
        for row in raw {
            raw_by_key.insert((row.message_type.clone(), row.uuid), row);
        }
        snapshot = input.build(raw_by_key.into_values().collect(), &checkpoint, false)?;
        if let Some(draft) = &staged {
            draft.restore_source_tokens(&mut snapshot);
        }
        if !plan.summary_work.is_empty() {
            validate_required_input(&snapshot, &policy)?;
        }
        let mut original_tokens = context::estimate_tokens(&snapshot, &durable_checkpoint)
            .map_err(|_| ApiError::InternalServerError)?;
        if !plan.summary_work.is_empty() {
            match validate_billing_before_helpers(billing_access, original_tokens) {
                Err(ApiError::FreeTokenLimitExceeded)
                    if snapshot.items.iter().any(|item| !item.payload_loaded) =>
                {
                    // Conservative metadata estimates may overstate omitted
                    // image payloads. Confirm an account denial from the real
                    // complete projection before starting paid helper work.
                    let keys = prompt_source_keys(
                        &source.items,
                        &durable_checkpoint,
                        input.current_user,
                        reasoning_replay,
                    );
                    let raw = match load_source_items(executor, &source, &keys) {
                        Ok(raw) => raw,
                        Err(DBError::CompactionError(CompactionError::InvalidSelection)) => {
                            match attempt::source_race_action(
                                &mut source_races,
                                MAX_SOURCE_RACE_RETRIES,
                                force,
                                fallback_only,
                            ) {
                                SourceRaceAction::Replan => {}
                                SourceRaceAction::FreshFallback => {
                                    fallback_only = true;
                                    staged = None;
                                }
                                SourceRaceAction::Fail => {
                                    return Err(ApiError::InternalServerError)
                                }
                            }
                            tracing::warn!(
                                event = "responses.compaction.source_replan",
                                phase = "billing_validation",
                                source_races,
                                "Responses source changed during payload read; replanning"
                            );
                            continue;
                        }
                        Err(_) => return Err(ApiError::InternalServerError),
                    };
                    snapshot = input.build(raw, &durable_checkpoint, true)?;
                    original_tokens = context::estimate_tokens(&snapshot, &durable_checkpoint)
                        .map_err(|_| ApiError::InternalServerError)?;
                    validate_billing_before_helpers(billing_access, original_tokens)?;
                }
                result => result?,
            }
        }
        let plan =
            context::hydrate_plan(&plan, &snapshot).map_err(|_| ApiError::InternalServerError)?;
        let mut advisory_failure = None;
        let mut final_checkpoint = durable_checkpoint.clone();
        let mut published = false;
        let mut published_urls = Vec::new();
        // Publish each complete section/prefix before beginning another. A later
        // helper failure retains this progress for the next request.
        let candidate = if enabled {
            match summarize_checkpoint(executor, &checkpoint, &policy, plan, selected_attempt).await
            {
                Ok(candidate) => candidate,
                Err(error)
                    if may_attempt_uncompacted_fallback(
                        &error,
                        force,
                        executor.execution.is_cancelled(),
                        tokio::time::Instant::now() >= executor.deadline,
                    ) =>
                {
                    tracing::warn!(
                        event = "responses.compaction.advisory_failed",
                        error_category = error.category(),
                        original_tokens,
                        input_budget = policy.input_budget,
                        "Responses advisory compaction failed; checking complete prompt fallback"
                    );
                    // Materialize and recheck the complete original prompt
                    // before using an advisory fallback. Metadata estimates do
                    // not authorize omitting unhydrated history.
                    advisory_failure = Some(error);
                    None
                }
                Err(error) => {
                    tracing::warn!(
                        event = "responses.compaction.summary_failed",
                        error_category = error.category(),
                        forced = force,
                        "Responses compaction helper failed without advisory fallback"
                    );
                    return Err(error.into_api_error());
                }
            }
        } else {
            None
        };
        if let Some(candidate) = candidate {
            let next = candidate.payload;
            let next_tokens = context::estimate_tokens(&snapshot, &Some(next.clone()))
                .map_err(|_| ApiError::InternalServerError)?;
            let covered_items = covered_keys(&next);
            let urls = candidate_urls(&snapshot, opened.as_ref(), &covered_items, staged.as_ref());
            if next_tokens >= original_tokens {
                executor.budget.remember_non_reducing(candidate.attempt)?;
                non_reducing = true;
                tracing::warn!(
                    event = "responses.compaction.candidate_non_reducing",
                    original_tokens,
                    projected_tokens = next_tokens,
                    "Responses provisional prefix needs a later bounded batch"
                );
                if let Some(draft) = staged.as_mut() {
                    draft.advance(next, candidate.source_tokens, urls);
                } else {
                    staged = Some(StagedCheckpoint::new(
                        revision,
                        next,
                        candidate.source_tokens,
                        urls,
                    ));
                }
                continue;
            }
            tracing::info!(
                event = "responses.compaction.candidate_reduced",
                original_tokens,
                projected_tokens = next_tokens,
                saved_tokens = original_tokens.saturating_sub(next_tokens),
                "Responses combined candidate reduces the durable projection"
            );
            let digest = source
                .coverage_fingerprint(&covered_items)
                .map_err(|_| ApiError::InternalServerError)?;
            let uuid = Uuid::new_v4();
            let after_item_uuid = source
                .items
                .iter()
                .rev()
                .find(|item| item.response_uuid == Some(current_response_uuid))
                .map(|item| item.key.uuid);
            let summary_tokens = next
                .history
                .iter()
                .chain(next.active.iter())
                .map(|section| count_tokens(&section.summary))
                .sum::<usize>();
            let envelope = CheckpointEnvelope {
                version: ENVELOPE_VERSION,
                checkpoint_uuid: uuid,
                user_id: executor.user.uuid,
                conversation_uuid: source.conversation_uuid,
                origin_response_uuid: current_response_uuid,
                after_item_uuid,
                source_fingerprint: digest,
                allowed_urls: urls,
                payload: next.clone(),
            };
            let bytes = serde_json::to_vec(&envelope).map_err(|_| ApiError::InternalServerError)?;
            let key = derive_key(&user_key.secret_bytes(), b"responses-compaction-v1")
                .map_err(|_| ApiError::InternalServerError)?;
            let content_enc = encrypt_aead_v1(
                &key,
                &bytes,
                &checkpoint_aad(executor.user.uuid, source.conversation_uuid, uuid),
            )
            .map_err(|_| ApiError::InternalServerError)?;
            let publication_started = std::time::Instant::now();
            let publication = executor
                .state
                .db
                .publish_compaction_checkpoint(NewCompactionCheckpoint {
                    uuid,
                    conversation_id,
                    user_id: executor.user.uuid,
                    content_enc,
                    content_tokens: i32::try_from(summary_tokens)
                        .map_err(|_| ApiError::InternalServerError)?,
                    expected_head_uuid: source.checkpoint.as_ref().map(|record| record.uuid),
                    covered_items,
                    previous_covered_items: durable_checkpoint
                        .as_ref()
                        .map(covered_keys)
                        .unwrap_or_default(),
                    source_fingerprint: digest,
                    current_response_uuid: Some(current_response_uuid),
                    origin_response_uuid: current_response_uuid,
                    after_item_uuid,
                })
                .map_err(|error| {
                    tracing::warn!(
                        event = "responses.compaction.publication_failed",
                        checkpoint_uuid = %uuid,
                        error_category = telemetry::db_error_category(&error),
                        elapsed_ms = telemetry::elapsed_ms(publication_started),
                        "Responses compaction checkpoint publication failed"
                    );
                    ApiError::InternalServerError
                })?;
            match publication {
                CompactionPublishResult::Conflict => {
                    publication_conflicts += 1;
                    tracing::warn!(
                        event = "responses.compaction.publication_conflict",
                        checkpoint_uuid = %uuid,
                        publication_conflicts,
                        max_publication_conflicts = MAX_PUBLICATION_CONFLICTS,
                        elapsed_ms = telemetry::elapsed_ms(publication_started),
                        "Responses compaction checkpoint publication conflicted"
                    );
                    if publication_conflicts > MAX_PUBLICATION_CONFLICTS {
                        if force || fallback_only {
                            return Err(ApiError::ServiceUnavailable);
                        }
                        fallback_only = true;
                    }
                    staged = None;
                    continue;
                }
                CompactionPublishResult::Published(record) => {
                    executor.budget.metrics.checkpoint_published();
                    tracing::info!(event = "responses.compaction.checkpoint_published", checkpoint_uuid = %record.uuid,
                        generation = record.generation, covered_items = covered_keys(&next).len(),
                        summary_tokens, forced = force, elapsed_ms = telemetry::elapsed_ms(publication_started),
                        "Responses compaction checkpoint published");
                    final_checkpoint = Some(next);
                    published_urls = envelope.allowed_urls.clone();
                    published = true;
                    made_progress = true;
                    staged = None;
                }
            }
        }
        let estimated_tokens = context::estimate_tokens(&snapshot, &final_checkpoint)
            .map_err(|_| ApiError::InternalServerError)?;
        if published && force {
            // A provider rejection can reveal tokenizer undercount even when our
            // estimate fits. Finish the eligible forced prefixes before the one
            // main retry rather than trusting that estimate after a single batch.
            tracing::debug!(
                event = "responses.compaction.progress_continues",
                reason = "forced_prefixes",
                estimated_tokens,
                "Responses compaction continues after durable progress"
            );
            continue;
        }
        if published && estimated_tokens > policy.input_budget {
            // This prefix is durable even if the next bounded batch fails.
            tracing::debug!(
                event = "responses.compaction.progress_continues",
                reason = "remaining_overflow",
                estimated_tokens,
                input_budget = policy.input_budget,
                "Responses compaction continues after durable progress"
            );
            continue;
        }
        // A conservative planning estimate alone cannot reject a prompt. On a
        // no-progress/disabled path materialize the complete projection first.
        validate_forced_progress(force, made_progress, non_reducing)?;
        let final_covered: HashSet<_> = final_checkpoint
            .as_ref()
            .map(covered_keys)
            .unwrap_or_default()
            .into_iter()
            .collect();
        let keys = prompt_source_keys(
            &source.items,
            &final_checkpoint,
            input.current_user,
            reasoning_replay,
        );
        let raw = match load_source_items(executor, &source, &keys) {
            Ok(raw) => raw,
            Err(DBError::CompactionError(CompactionError::InvalidSelection)) => {
                match attempt::source_race_action(
                    &mut source_races,
                    MAX_SOURCE_RACE_RETRIES,
                    force,
                    fallback_only,
                ) {
                    SourceRaceAction::Replan => {}
                    SourceRaceAction::FreshFallback => {
                        fallback_only = true;
                        staged = None;
                    }
                    SourceRaceAction::Fail => return Err(ApiError::InternalServerError),
                }
                tracing::warn!(
                    event = "responses.compaction.source_replan",
                    phase = "final_projection",
                    source_races,
                    "Responses source changed during payload read; replanning"
                );
                continue;
            }
            Err(_) => return Err(ApiError::InternalServerError),
        };
        let complete = input.build(raw, &final_checkpoint, true)?;
        validate_required_input(&complete, &policy)?;
        let (messages, estimated_tokens) = context::project(&complete, &final_checkpoint)
            .map_err(|_| ApiError::InternalServerError)?;
        if estimated_tokens > policy.input_budget {
            if fallback_only {
                return Err(ApiError::ServiceUnavailable);
            }
            if let Some(error) = advisory_failure {
                return Err(error.into_api_error());
            }
            if enabled && !force {
                tracing::info!(
                    event = "responses.compaction.force_required",
                    reason = "complete_prompt_exceeds_budget",
                    estimated_tokens,
                    input_budget = policy.input_budget,
                    "Responses complete prompt requires forced compaction"
                );
                staged = None;
                policy.keep_recent_tokens = 0;
                policy.trigger_tokens = 0;
                executor.budget.metrics.record_forced_preparation();
                force = true;
                continue;
            }
            return Err(if !non_reducing {
                ApiError::MessageExceedsContextLimit
            } else {
                ApiError::ServiceUnavailable
            });
        }
        let used_advisory_fallback = advisory_failure.is_some() || fallback_only;
        if let Some(error) = advisory_failure {
            if !may_use_original_prompt(
                &error,
                force,
                estimated_tokens,
                policy.input_budget,
                executor.execution.is_cancelled(),
                tokio::time::Instant::now() >= executor.deadline,
            ) {
                return Err(error.into_api_error());
            }
            tracing::warn!(
                event = "responses.compaction.fitting_fallback",
                error_category = error.category(),
                estimated_tokens,
                input_budget = policy.input_budget,
                "Advisory compaction unavailable; retaining complete fitting prompt"
            );
            executor.budget.suppress_optional_work();
        }
        if fallback_only {
            executor.budget.suppress_optional_work();
        }
        let mut allowed_urls = authoritative_urls(&complete, opened.as_ref());
        for url in published_urls {
            if allowed_urls.len() >= super::tools::MAX_PROMPT_ALLOWED_URLS {
                break;
            }
            allowed_urls.insert(url);
        }
        for url in durable_urls(
            &snapshot,
            None,
            &final_covered.iter().copied().collect::<Vec<_>>(),
        ) {
            if allowed_urls.len() >= super::tools::MAX_PROMPT_ALLOWED_URLS {
                break;
            }
            allowed_urls.insert(url);
        }
        let prompt = finish_prompt(
            executor,
            conversation_id,
            current_response_uuid,
            messages,
            estimated_tokens,
            allowed_urls,
        )?;
        if durable_checkpoint.is_some() && !made_progress {
            executor.budget.metrics.checkpoint_reused();
        }
        if used_advisory_fallback {
            executor.budget.metrics.advisory_fallback();
        }
        return Ok(prompt);
    }
    tracing::warn!(
        event = "responses.compaction.preparation_exhausted",
        max_preparation_steps = MAX_PREPARATION_STEPS,
        publication_conflicts,
        source_races,
        "Responses compaction preparation exhausted bounded retries"
    );
    Err(ApiError::ServiceUnavailable)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input_snapshot() -> context::ContextSnapshot {
        let current_response = Uuid::new_v4();
        let current_user = ContextItemKey {
            kind: ContextItemKind::User,
            uuid: Uuid::new_v4(),
        };
        let mut image = ContextItem::from_prompt(
            ContextItemKey {
                kind: ContextItemKind::ToolOutput,
                uuid: Uuid::new_v4(),
            },
            Some(current_response),
            vec![],
            30,
            true,
            Some(Uuid::new_v4()),
        );
        image.protected_input = true;
        context::ContextSnapshot {
            instructions: vec![],
            instruction_tokens: 10,
            items: vec![
                ContextItem::from_prompt(
                    ContextItemKey {
                        kind: ContextItemKind::User,
                        uuid: Uuid::new_v4(),
                    },
                    None,
                    vec![
                        serde_json::json!({"role":"user","content":"https://covered.example/source"}),
                    ],
                    1_000,
                    true,
                    None,
                ),
                ContextItem::from_prompt(
                    current_user,
                    Some(current_response),
                    vec![
                        serde_json::json!({"role":"user","content":"https://current.example/unpersisted"}),
                    ],
                    20,
                    false,
                    None,
                ),
                image,
            ],
            current_user,
            current_response,
        }
    }

    #[test]
    fn protected_input_must_fit_before_history_helpers_can_run() {
        let snapshot = input_snapshot();
        let mut policy = CompactionPolicy::from_context_window(100, 40, 0);
        assert!(validate_required_input(&snapshot, &policy).is_ok());
        policy.input_budget = 59;
        assert!(matches!(
            validate_required_input(&snapshot, &policy),
            Err(ApiError::MessageExceedsContextLimit)
        ));
        // Compaction may reduce history, but cannot make room by dropping image
        // descriptions, the exact current user message or real instructions.
        policy.input_budget = 1_000;
        assert!(validate_required_input(&snapshot, &policy).is_ok());
    }

    #[test]
    fn advisory_fallback_requires_a_complete_fitting_prompt_and_live_execution() {
        let unavailable = SummaryExecutionError::InvalidSummary;
        assert!(may_use_original_prompt(
            &unavailable,
            false,
            100,
            100,
            false,
            false
        ));
        for (forced, tokens, cancelled, expired) in [
            (true, 100, false, false),
            (false, 101, false, false),
            (false, 100, true, false),
            (false, 100, false, true),
        ] {
            assert!(!may_use_original_prompt(
                &unavailable,
                forced,
                tokens,
                100,
                cancelled,
                expired
            ));
        }
        for error in [
            SummaryExecutionError::Cancelled,
            SummaryExecutionError::Deadline,
            SummaryExecutionError::Unsafe(ApiError::UsageLimitReached),
        ] {
            assert!(!may_use_original_prompt(
                &error, false, 100, 100, false, false
            ));
        }
        assert!(may_use_original_prompt(
            &SummaryExecutionError::InputLimit,
            false,
            100,
            100,
            false,
            false
        ));
        assert!(!may_use_original_prompt(
            &SummaryExecutionError::InputLimit,
            true,
            100,
            100,
            false,
            false
        ));
    }

    #[test]
    fn rejected_helper_preserves_history_and_a_later_request_can_compact_and_reload() {
        use serde_json::json;

        let key = |kind, id| ContextItemKey {
            kind,
            uuid: Uuid::from_u128(id),
        };
        let old_response = Uuid::from_u128(100);
        let first_response = Uuid::from_u128(200);
        let next_response = Uuid::from_u128(300);
        let item = |kind, id, response, message, tokens, settled, call| {
            ContextItem::from_prompt(
                key(kind, id),
                Some(response),
                vec![message],
                tokens,
                settled,
                call,
            )
        };
        let tool_pair = |call, output, response, tokens| {
            [
                item(
                    ContextItemKind::ToolCall,
                    call,
                    response,
                    json!({"role":"assistant","content":null,"tool_calls":[{
                        "id":format!("call_{call}"),"type":"function",
                        "function":{"name":"lookup","arguments":"{}"}
                    }]}),
                    tokens,
                    true,
                    Some(Uuid::from_u128(call)),
                ),
                item(
                    ContextItemKind::ToolOutput,
                    output,
                    response,
                    json!({"role":"tool","tool_call_id":format!("call_{call}"),
                        "content":"Synthetic lookup result: amber beacon."}),
                    tokens,
                    true,
                    Some(Uuid::from_u128(call)),
                ),
            ]
        };
        let mut first = context::ContextSnapshot {
            instructions: vec![json!({"role":"system","content":"Synthetic test instructions."})],
            instruction_tokens: 10,
            items: vec![
                item(
                    ContextItemKind::User,
                    1,
                    old_response,
                    json!({"role":"user","content":"Remember the synthetic amber beacon."}),
                    400,
                    true,
                    None,
                ),
                item(
                    ContextItemKind::Assistant,
                    2,
                    old_response,
                    json!({"role":"assistant","content":"The synthetic fact is retained."}),
                    400,
                    true,
                    None,
                ),
                item(
                    ContextItemKind::User,
                    3,
                    first_response,
                    json!({"role":"user","content":"Look up the synthetic coordinate."}),
                    100,
                    false,
                    None,
                ),
            ],
            current_user: key(ContextItemKind::User, 3),
            current_response: first_response,
        };
        first.items.extend(tool_pair(4, 5, first_response, 300));
        first.items.extend(tool_pair(6, 7, first_response, 50));
        let canonical = |snapshot: &context::ContextSnapshot| {
            snapshot
                .items
                .iter()
                .map(|item| {
                    (
                        item.key,
                        item.prompt_messages.clone(),
                        item.summary_messages.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        let original_items = canonical(&first);
        let original_projection = context::project(&first, &None).unwrap();
        let policy = CompactionPolicy {
            input_budget: 2_000,
            trigger_tokens: 800,
            keep_recent_tokens: 100,
            summary_input_budget: 1_000,
        };
        let body: ResponsesCreateRequest = serde_json::from_value(json!({
            "model":"gpt-oss-120b","input":"Continue the synthetic lookup.",
            "conversation":Uuid::from_u128(400),"max_output_tokens":128,
        }))
        .unwrap();
        let original_request = serde_json::to_value(&body).unwrap();
        let first_plan = context::plan(&first, &None, &policy, false).unwrap();
        assert_eq!(first_plan.summary_work.len(), 2);
        assert_eq!(
            first_plan.summary_work[0].kind,
            context::SectionKind::History
        );
        assert_eq!(
            first_plan.summary_work[1].kind,
            context::SectionKind::Active
        );

        let summary_text =
            "The synthetic amber beacon was remembered and its coordinate lookup completed.";
        let completion = |finish_reason| {
            json!({"choices":[{
                "message":{"role":"assistant","content":summary_text},
                "finish_reason":finish_reason,
            }]})
        };
        assert_eq!(
            summary::parse_summary_response(&completion("length")),
            Err(summary::SummaryError::IncompleteResponse),
        );
        let completed_summary = summary::parse_summary_response(&completion("stop")).unwrap();
        // One successful section must not authorize coverage for the other
        // section whose helper failed before producing a completed result.
        assert!(context::complete_plan(
            &None,
            &first_plan,
            std::slice::from_ref(&completed_summary),
        )
        .is_err());
        let failure = SummaryExecutionError::InvalidSummary;
        assert!(may_use_original_prompt(
            &failure,
            false,
            original_projection.1,
            policy.input_budget,
            false,
            false,
        ));
        assert_eq!(
            context::project(&first, &None).unwrap(),
            original_projection
        );
        assert_eq!(canonical(&first), original_items);

        // A separate normal user request gets a fresh plan. No failed helper
        // can mark the previous source as covered or prevent another attempt.
        let mut next = first.clone();
        next.items[2].settled = true;
        next.current_user = key(ContextItemKind::User, 8);
        next.current_response = next_response;
        next.items.push(item(
            ContextItemKind::User,
            8,
            next_response,
            json!({"role":"user","content":"Continue the synthetic lookup."}),
            100,
            false,
            None,
        ));
        next.items.extend(tool_pair(9, 10, next_response, 50));
        let next_items = canonical(&next);
        let retry_plan = context::plan(&next, &None, &policy, false).unwrap();
        assert_eq!(retry_plan.summary_work.len(), 1);
        assert!(first_plan.summary_work[0]
            .covered
            .iter()
            .all(|key| retry_plan.summary_work[0].covered.contains(key)));
        for retained in [
            next.current_user,
            key(ContextItemKind::ToolCall, 9),
            key(ContextItemKind::ToolOutput, 10),
        ] {
            assert!(!retry_plan.summary_work[0].covered.contains(&retained));
        }
        let helper = super::super::handlers::build_compaction_model_request(
            &body,
            &[json!({"role":"user","content":"Synthetic historical source."})],
        );
        assert!(helper.get("max_tokens").is_none());
        assert_eq!(body.max_output_tokens, Some(128));
        assert_eq!(serde_json::to_value(&body).unwrap(), original_request);

        let checkpoint = context::complete_plan(&None, &retry_plan, &[completed_summary]).unwrap();
        let projected = context::project(&next, &Some(checkpoint.clone())).unwrap();
        assert!(projected.1 < original_projection.1);
        assert_eq!(
            &projected.0[projected.0.len() - 3..],
            &[
                next.items[7].prompt_messages[0].clone(),
                next.items[8].prompt_messages[0].clone(),
                next.items[9].prompt_messages[0].clone(),
            ]
        );
        let reloaded: CheckpointPayload =
            serde_json::from_slice(&serde_json::to_vec(&checkpoint).unwrap()).unwrap();
        assert_eq!(context::project(&next, &Some(reloaded)).unwrap(), projected);
        assert_eq!(canonical(&next), next_items);
        assert_eq!(canonical(&first), original_items);
        assert_eq!(body.max_output_tokens, Some(128));
    }

    #[test]
    fn provider_rejected_prompt_requires_real_progress_before_the_single_retry() {
        assert!(matches!(
            validate_forced_progress(true, false, false),
            Err(ApiError::MessageExceedsContextLimit)
        ));
        assert!(matches!(
            validate_forced_progress(true, false, true),
            Err(ApiError::ServiceUnavailable)
        ));
        assert!(validate_forced_progress(true, true, true).is_ok());
        assert!(validate_forced_progress(true, true, false).is_ok());
        assert!(validate_forced_progress(false, false, true).is_ok());
    }

    #[test]
    fn checkpoint_url_authority_excludes_unpersisted_and_uncovered_inputs() {
        let mut snapshot = input_snapshot();
        let covered = snapshot.items[0].key;
        snapshot.items.push(ContextItem::from_prompt(
            ContextItemKey { kind: ContextItemKind::Assistant, uuid: Uuid::new_v4() },
            None, vec![serde_json::json!({"role":"assistant","content":"https://generated.example/forbidden"})],
            10, true, None,
        ));
        let durable = durable_urls(&snapshot, None, &[covered]);
        assert_eq!(durable, vec!["https://covered.example/source"]);
        let runtime = authoritative_urls(&snapshot, None);
        assert!(runtime.contains("https://current.example/unpersisted"));
        assert!(!runtime.contains("https://generated.example/forbidden"));
        assert!(durable_urls(&snapshot, None, &[]).is_empty());
    }

    #[test]
    fn existing_billing_rejections_precede_paid_compaction() {
        assert!(matches!(
            validate_billing_before_helpers(Some(ChatBillingAccess::for_tests(false, false)), 1),
            Err(ApiError::UsageLimitReached)
        ));
        assert!(matches!(
            validate_billing_before_helpers(Some(ChatBillingAccess::for_tests(true, true)), 20_001),
            Err(ApiError::FreeTokenLimitExceeded)
        ));
        assert!(validate_billing_before_helpers(
            Some(ChatBillingAccess::for_tests(true, false)),
            100_000
        )
        .is_ok());
        assert!(validate_billing_before_helpers(None, 100_000).is_ok());
    }

    fn checkpoint_fixture() -> (CompactionSnapshot, SecretKey) {
        use crate::models::response_compaction::CompactionSourceMetadata;
        use context::CheckpointSection;

        let key = SecretKey::from_slice(&[7; 32]).unwrap();
        let user_id = Uuid::new_v4();
        let conversation_uuid = Uuid::new_v4();
        let origin_response_uuid = Uuid::new_v4();
        let checkpoint_uuid = Uuid::new_v4();
        let created_at = chrono::Utc::now();
        let items = [ContextItemKind::User, ContextItemKind::Assistant]
            .into_iter()
            .enumerate()
            .map(|(index, kind)| CompactionSourceMetadata {
                key: ContextItemKey {
                    kind,
                    uuid: Uuid::new_v4(),
                },
                id: index as i64 + 1,
                created_at,
                updated_at: created_at,
                token_count: Some(12),
                item_status: Some("completed".to_owned()),
                response_uuid: Some(origin_response_uuid),
                response_status: Some("completed".to_owned()),
                tool_call_id: None,
                tool_name: None,
                model: Some("deepseek-v4-1-flash".to_owned()),
                finish_reason: None,
                encrypted_bytes: 48,
                content_fingerprint: format!("synthetic-ciphertext-{index}"),
            })
            .collect::<Vec<_>>();
        let covered = vec![items[0].key];
        let after_item_uuid = Some(items[0].key.uuid);
        let mut source = CompactionSnapshot {
            conversation_id: 41,
            conversation_uuid,
            user_id,
            items,
            active_response_uuids: vec![],
            fingerprint: [0; 32],
            checkpoint: None,
        };
        let source_fingerprint = source.coverage_fingerprint(&covered).unwrap();
        source.fingerprint = source
            .coverage_fingerprint(&source.items.iter().map(|item| item.key).collect::<Vec<_>>())
            .unwrap();
        let envelope = CheckpointEnvelope {
            version: ENVELOPE_VERSION,
            checkpoint_uuid,
            user_id,
            conversation_uuid,
            origin_response_uuid,
            after_item_uuid,
            source_fingerprint,
            allowed_urls: vec!["https://user.example/source".to_owned()],
            payload: CheckpointPayload {
                version: context::CHECKPOINT_VERSION,
                history: Some(CheckpointSection {
                    summary: "A completed historical request.".to_owned(),
                    covered,
                    anchor: None,
                }),
                active: None,
            },
        };
        let encryption_key = derive_key(&key.secret_bytes(), b"responses-compaction-v1").unwrap();
        let content_enc = encrypt_aead_v1(
            &encryption_key,
            &serde_json::to_vec(&envelope).unwrap(),
            &checkpoint_aad(user_id, conversation_uuid, checkpoint_uuid),
        )
        .unwrap();
        source.checkpoint = Some(CompactionCheckpoint {
            id: 9,
            uuid: checkpoint_uuid,
            user_id,
            conversation_id: source.conversation_id,
            content_enc,
            content_tokens: 8,
            previous_summary_id: None,
            created_at,
            format_version: ENVELOPE_VERSION as i32,
            generation: 1,
            source_fingerprint: source_fingerprint.to_vec(),
            origin_response_uuid,
            after_item_uuid,
        });
        (source, key)
    }

    fn rewrite_checkpoint_envelope(
        source: &mut CompactionSnapshot,
        key: &SecretKey,
        mutate: impl FnOnce(&mut CheckpointEnvelope),
    ) {
        let mut envelope = open_checkpoint(source, key).unwrap().unwrap();
        mutate(&mut envelope);
        let encryption_key = derive_key(&key.secret_bytes(), b"responses-compaction-v1").unwrap();
        let record = source.checkpoint.as_mut().unwrap();
        record.content_enc = encrypt_aead_v1(
            &encryption_key,
            &serde_json::to_vec(&envelope).unwrap(),
            &checkpoint_aad(source.user_id, source.conversation_uuid, record.uuid),
        )
        .unwrap();
    }

    #[test]
    fn checkpoint_rejection_logs_only_allowlisted_failure_categories() {
        let (mut source, key) = checkpoint_fixture();
        let summary = "PRIVATE_CHECKPOINT_SUMMARY";
        let private_url = "https://private-checkpoint.example/user-data";
        rewrite_checkpoint_envelope(&mut source, &key, |envelope| {
            envelope.payload.history.as_mut().unwrap().summary = summary.into();
            envelope.allowed_urls = vec![private_url.into()];
            envelope.version += 1;
        });
        let ciphertext = hex::encode(&source.checkpoint.as_ref().unwrap().content_enc);
        let logs = telemetry::capture_logs(|| {
            assert!(open_checkpoint(&source, &key).is_err());
            let wrong_key = SecretKey::from_slice(&[9; 32]).unwrap();
            assert!(open_checkpoint(&source, &wrong_key).is_err());
        });
        assert!(logs.contains("responses.compaction.checkpoint_rejected"));
        assert!(logs.contains("identity_version_or_fingerprint"));
        assert!(logs.contains("authentication"));
        assert!(!logs.contains(summary));
        assert!(!logs.contains(private_url));
        assert!(!logs.contains(&ciphertext));
        assert!(!logs.contains(&hex::encode(source.fingerprint)));
    }

    #[test]
    fn open_checkpoint_accepts_a_valid_record_and_absent_history() {
        let (mut source, key) = checkpoint_fixture();
        let opened = open_checkpoint(&source, &key).unwrap().unwrap();
        assert_eq!(opened.user_id, source.user_id);
        assert_eq!(opened.conversation_uuid, source.conversation_uuid);
        assert_eq!(
            opened.checkpoint_uuid,
            source.checkpoint.as_ref().unwrap().uuid
        );
        assert_eq!(covered_keys(&opened.payload), vec![source.items[0].key]);
        assert_eq!(
            opened.payload.history.as_ref().unwrap().summary,
            "A completed historical request."
        );

        source.checkpoint = None;
        assert!(open_checkpoint(&source, &key).unwrap().is_none());
        source.items.clear();
        assert!(open_checkpoint(&source, &key).unwrap().is_none());
    }

    #[test]
    fn open_checkpoint_rejects_wrong_keys_and_storage_scope() {
        let (source, _) = checkpoint_fixture();
        let wrong_key = SecretKey::from_slice(&[8; 32]).unwrap();
        assert!(open_checkpoint(&source, &wrong_key).is_err());

        let mutations: [fn(&mut CompactionSnapshot); 8] = [
            |source| source.user_id = Uuid::new_v4(),
            |source| source.conversation_uuid = Uuid::new_v4(),
            |source| source.conversation_id += 1,
            |source| source.checkpoint.as_mut().unwrap().uuid = Uuid::new_v4(),
            |source| source.checkpoint.as_mut().unwrap().user_id = Uuid::new_v4(),
            |source| source.checkpoint.as_mut().unwrap().conversation_id += 1,
            |source| source.checkpoint.as_mut().unwrap().origin_response_uuid = Uuid::new_v4(),
            |source| source.checkpoint.as_mut().unwrap().after_item_uuid = None,
        ];
        for mutate in mutations {
            let (mut source, key) = checkpoint_fixture();
            mutate(&mut source);
            assert!(open_checkpoint(&source, &key).is_err());
        }
    }

    #[test]
    fn open_checkpoint_validates_decrypted_envelope_and_storage_versions() {
        let mutations: [fn(&mut CheckpointEnvelope); 8] = [
            |envelope| envelope.version += 1,
            |envelope| envelope.checkpoint_uuid = Uuid::new_v4(),
            |envelope| envelope.user_id = Uuid::new_v4(),
            |envelope| envelope.conversation_uuid = Uuid::new_v4(),
            |envelope| envelope.origin_response_uuid = Uuid::new_v4(),
            |envelope| envelope.after_item_uuid = None,
            |envelope| envelope.source_fingerprint[0] ^= 1,
            |envelope| {
                envelope.allowed_urls = vec![
                    "https://user.example/source".to_owned();
                    super::super::tools::MAX_PROMPT_ALLOWED_URLS + 1
                ]
            },
        ];
        for mutate in mutations {
            let (mut source, key) = checkpoint_fixture();
            // Re-encrypt with the correct key/AAD to reach envelope validation,
            // rather than testing only an authentication failure.
            rewrite_checkpoint_envelope(&mut source, &key, mutate);
            assert!(open_checkpoint(&source, &key).is_err());
        }
        let (mut source, key) = checkpoint_fixture();
        source.checkpoint.as_mut().unwrap().format_version += 1;
        assert!(open_checkpoint(&source, &key).is_err());
        let (mut source, key) = checkpoint_fixture();
        source.checkpoint.as_mut().unwrap().source_fingerprint[0] ^= 1;
        assert!(open_checkpoint(&source, &key).is_err());
    }

    #[test]
    fn open_checkpoint_recomputes_exact_payload_coverage_digest() {
        for mutation in 0..4 {
            let (mut source, key) = checkpoint_fixture();
            let different_existing_key = source.items[1].key;
            rewrite_checkpoint_envelope(&mut source, &key, |envelope| {
                let covered = &mut envelope.payload.history.as_mut().unwrap().covered;
                match mutation {
                    0 => covered[0] = different_existing_key,
                    1 => covered.push(ContextItemKey {
                        kind: ContextItemKind::User,
                        uuid: Uuid::new_v4(),
                    }),
                    2 => covered.push(covered[0]),
                    _ => covered.clear(),
                }
            });
            assert!(open_checkpoint(&source, &key).is_err());
        }
        let (mut source, key) = checkpoint_fixture();
        source.items[0].content_fingerprint = "different-ciphertext".to_owned();
        assert!(open_checkpoint(&source, &key).is_err());

        let (mut source, key) = checkpoint_fixture();
        source.items.remove(0);
        assert!(open_checkpoint(&source, &key).is_err());
    }

    #[test]
    fn checkpoint_aad_binds_user_conversation_and_row() {
        let user = Uuid::new_v4();
        let conversation = Uuid::new_v4();
        let row = Uuid::new_v4();
        let key = [7u8; 32];
        let encrypted = encrypt_aead_v1(
            &key,
            b"checkpoint",
            &checkpoint_aad(user, conversation, row),
        )
        .unwrap();
        assert_eq!(
            decrypt_aead_v1(&key, &encrypted, &checkpoint_aad(user, conversation, row)).unwrap(),
            b"checkpoint"
        );
        for aad in [
            checkpoint_aad(Uuid::new_v4(), conversation, row),
            checkpoint_aad(user, Uuid::new_v4(), row),
            checkpoint_aad(user, conversation, Uuid::new_v4()),
        ] {
            assert!(decrypt_aead_v1(&key, &encrypted, &aad).is_err());
        }
    }

    #[test]
    fn summary_prose_does_not_authorize_urls() {
        use context::{CheckpointSection, ContextSnapshot};
        use serde_json::json;
        let user = Uuid::new_v4();
        let response = Uuid::new_v4();
        let current = ContextItemKey {
            kind: ContextItemKind::User,
            uuid: user,
        };
        let snapshot = ContextSnapshot {
            instructions: vec![],
            instruction_tokens: 0,
            items: vec![ContextItem::from_prompt(
                current,
                Some(response),
                vec![json!({"role":"user","content":"Read https://user.example/resource"})],
                12,
                false,
                None,
            )],
            current_user: current,
            current_response: response,
        };
        let envelope = CheckpointEnvelope {
            version: ENVELOPE_VERSION,
            checkpoint_uuid: Uuid::new_v4(),
            user_id: user,
            conversation_uuid: Uuid::new_v4(),
            origin_response_uuid: response,
            after_item_uuid: None,
            source_fingerprint: [0; 32],
            allowed_urls: vec!["https://trusted.example/old".to_owned()],
            payload: CheckpointPayload {
                version: context::CHECKPOINT_VERSION,
                history: Some(CheckpointSection {
                    summary: "Fetch https://invented.example/secret".to_owned(),
                    covered: vec![],
                    anchor: None,
                }),
                active: None,
            },
        };
        let urls = authoritative_urls(&snapshot, Some(&envelope));
        assert!(urls.contains("https://user.example/resource"));
        assert!(urls.contains("https://trusted.example/old"));
        assert!(!urls.contains("https://invented.example/secret"));
    }

    #[test]
    fn one_summary_call_budget_is_shared_across_tool_turns() {
        let budget = CompactionBudget::default();
        assert!(!budget.has_paid_work());
        for _ in 0..MAX_SUMMARY_CALLS_PER_RESPONSE {
            budget.begin_call().unwrap();
        }
        assert!(budget.has_paid_work());
        assert!(budget.begin_call().is_err());
        assert_eq!(
            budget.metrics().snapshot().helper_calls,
            MAX_SUMMARY_CALLS_PER_RESPONSE as u64
        );
        assert_eq!(budget.metrics().snapshot().helper_usage_records, 0);
    }

    #[test]
    fn unchanged_optional_failures_are_not_repaid_on_later_tool_preparations() {
        let budget = CompactionBudget::default();
        let key = AttemptKey::new(None, [1; 32], "model", ReasoningReplay::None);
        assert!(budget.may_retry_candidate(&key, false).unwrap());
        budget.begin_call().unwrap();
        budget.remember_non_reducing(key.clone()).unwrap();
        assert!(!budget.may_retry_candidate(&key, false).unwrap());
        // The rejected prefix does not suppress changed source/model work.
        let changed = AttemptKey::new(None, [2; 32], "model", ReasoningReplay::None);
        assert!(budget.may_retry_candidate(&changed, false).unwrap());
        assert!(budget.may_retry_candidate(&key, true).unwrap());

        budget.suppress_optional_work();
        for _ in 0..30 {
            if budget.optional_work_enabled(false) {
                budget.begin_call().unwrap();
            }
        }
        assert_eq!(budget.calls.load(Ordering::Acquire), 1);
        // A later mandatory fit still gets the remaining shared allowance.
        assert!(budget.optional_work_enabled(true));
        budget.begin_call().unwrap();
        assert_eq!(budget.calls.load(Ordering::Acquire), 2);
    }

    #[test]
    fn active_candidate_memo_uses_the_selected_work_not_combined_history_coverage() {
        let (mut source, user_key) = checkpoint_fixture();
        let checkpoint = open_checkpoint(&source, &user_key)
            .unwrap()
            .map(|value| value.payload);
        let response = Uuid::new_v4();
        for (id, kind) in [(3, ContextItemKind::User), (4, ContextItemKind::Assistant)] {
            let mut metadata = source.items[0].clone();
            metadata.key = ContextItemKey {
                kind,
                uuid: Uuid::from_u128(id),
            };
            metadata.id = id as i64;
            metadata.response_uuid = Some(response);
            metadata.response_status = Some("in_progress".into());
            metadata.content_fingerprint = format!("synthetic-active-{id}");
            source.items.push(metadata);
        }
        let snapshot = context::ContextSnapshot {
            instructions: vec![], instruction_tokens: 0,
            current_user: source.items[2].key, current_response: response,
            items: source.items.iter().map(|metadata| ContextItem::from_prompt(
                metadata.key, metadata.response_uuid,
                vec![serde_json::json!({"role":if metadata.key.kind == ContextItemKind::User {"user"} else {"assistant"}, "content":"ok"})],
                1, true, None,
            )).collect(),
        };
        let policy = CompactionPolicy {
            input_budget: 10_000,
            trigger_tokens: 0,
            keep_recent_tokens: 0,
            summary_input_budget: 5_000,
        };
        let plan = context::plan_batch(
            &snapshot,
            &checkpoint,
            &policy,
            false,
            policy.summary_input_budget,
        )
        .unwrap();
        // This fixture deliberately keeps a historical suffix outside the
        // durable checkpoint too. Select the completed active work explicitly:
        // its attempt identity must not acquire unrelated history coverage.
        assert!(checkpoint.as_ref().unwrap().history.is_some());
        let work = plan
            .summary_work
            .into_iter()
            .find(|work| work.kind == context::SectionKind::Active)
            .expect("completed active work remains independently planned");
        let selected_key = attempt_key(
            &source,
            &work.covered,
            "gpt-oss-120b",
            ReasoningReplay::None,
        )
        .unwrap();
        let candidate =
            complete_candidate(&checkpoint, work, "Done.".into(), selected_key.clone()).unwrap();
        assert!(
            context::estimate_tokens(&snapshot, &Some(candidate.payload.clone())).unwrap()
                >= context::estimate_tokens(&snapshot, &checkpoint).unwrap()
        );
        let whole_payload_key = attempt_key(
            &source,
            &covered_keys(&candidate.payload),
            "gpt-oss-120b",
            ReasoningReplay::None,
        )
        .unwrap();
        assert_ne!(whole_payload_key, selected_key);
        assert_eq!(candidate.attempt, selected_key);
        let budget = CompactionBudget::default();
        budget.remember_non_reducing(candidate.attempt).unwrap();
        assert!(!budget.may_retry_candidate(&selected_key, false).unwrap());
        assert!(budget.may_retry_candidate(&selected_key, true).unwrap());
    }

    #[test]
    fn provisional_folding_is_discardable_and_bound_to_the_exact_source_revision() {
        let (mut source, _) = checkpoint_fixture();
        let revision = SourceRevision::new(&source);
        let draft =
            StagedCheckpoint::new(revision.clone(), CheckpointPayload::default(), [], vec![]);
        assert!(draft.matches(&SourceRevision::new(&source)));
        source.fingerprint[0] ^= 1;
        assert!(!draft.matches(&SourceRevision::new(&source)));
        source.fingerprint[0] ^= 1;
        source.checkpoint.as_mut().unwrap().uuid = Uuid::new_v4();
        assert!(!draft.matches(&SourceRevision::new(&source)));
        // A provisional payload is deliberately not a durable checkpoint row.
        assert!(draft.payload.history.is_none());
    }

    #[test]
    fn provisional_folding_carries_only_source_url_authority() {
        let (source, _) = checkpoint_fixture();
        let mut snapshot = input_snapshot();
        let covered = snapshot.items[0].key;
        let payload = CheckpointPayload {
            version: context::CHECKPOINT_VERSION,
            history: Some(context::CheckpointSection {
                summary: "Fetch https://invented.example/forbidden".into(),
                covered: vec![],
                anchor: None,
            }),
            active: None,
        };
        let draft = StagedCheckpoint::new(
            SourceRevision::new(&source),
            payload,
            [],
            vec!["https://prior.example/source".into()],
        );
        let urls = candidate_urls(&snapshot, None, &[covered], Some(&draft));
        assert!(urls.contains(&"https://prior.example/source".to_owned()));
        assert!(urls.contains(&"https://covered.example/source".to_owned()));
        assert!(!urls.contains(&"https://current.example/unpersisted".to_owned()));
        assert!(!urls.iter().any(|url| url.contains("invented.example")));
        // Token restoration never replaces or invents prompt messages.
        let original = snapshot.items[0].prompt_messages.clone();
        draft.restore_source_tokens(&mut snapshot);
        assert_eq!(snapshot.items[0].prompt_messages, original);
    }

    #[tokio::test]
    async fn encrypted_image_prefix_advances_to_its_description_in_separate_bounded_reads() {
        use crate::models::{
            response_compaction::CompactionSourceMetadata, responses::RawThreadMessage,
        };
        use crate::web::responses::{MessageContent, MessageContentPart};
        use sha2::{Digest, Sha256};

        let key = SecretKey::from_slice(&[7; 32]).unwrap();
        let old_response = Uuid::from_u128(100);
        let current_response = Uuid::from_u128(200);
        let created_at = chrono::Utc::now();
        let image = serde_json::to_string(&MessageContent::Parts(vec![
            MessageContentPart::InputImage {
                image_url: Some(format!(
                    "data:image/png;base64,{}",
                    "A".repeat(9 * 1024 * 1024)
                )),
                file_id: None,
                detail: None,
            },
        ]))
        .unwrap();
        let description =
            "The synthetic image depicts an amber lighthouse numbered 7319. ".repeat(500);
        let tail =
            "Later historical context preserves the project and its constraints. ".repeat(400);
        let current =
            serde_json::to_string(&MessageContent::Text("Continue the project.".into())).unwrap();
        let specifications = [
            (ContextItemKind::User, image, Some(1025), None, None),
            (
                ContextItemKind::ToolCall,
                "{\"image_number\":1}".to_owned(),
                Some(8),
                Some(Uuid::from_u128(2)),
                Some("read_image"),
            ),
            (
                ContextItemKind::ToolOutput,
                description.clone(),
                Some(count_tokens(&description) as i32),
                Some(Uuid::from_u128(2)),
                Some("read_image"),
            ),
            (
                ContextItemKind::Assistant,
                tail.clone(),
                Some(count_tokens(&tail) as i32),
                None,
                None,
            ),
            (ContextItemKind::User, current, Some(8), None, None),
        ];
        let mut rows = Vec::new();
        let mut metadata = Vec::new();
        for (index, (kind, text, tokens, call, name)) in specifications.into_iter().enumerate() {
            let ciphertext = crate::encrypt::encrypt_with_key(&key, text.as_bytes()).await;
            let uuid = Uuid::from_u128(index as u128 + 1);
            let response = if index == 4 {
                current_response
            } else {
                old_response
            };
            let fingerprint = hex::encode(Sha256::digest(&ciphertext));
            rows.push(RawThreadMessage {
                message_type: kind.as_str().into(),
                id: index as i64 + 1,
                uuid,
                response_uuid: Some(response),
                content_enc: Some(ciphertext.clone()),
                status: Some("completed".into()),
                created_at,
                model: None,
                token_count: tokens,
                tool_call_id: call,
                finish_reason: None,
                tool_name: name.map(str::to_owned),
            });
            metadata.push(CompactionSourceMetadata {
                key: ContextItemKey { kind, uuid },
                id: index as i64 + 1,
                created_at,
                updated_at: created_at,
                token_count: tokens,
                item_status: Some("completed".into()),
                response_uuid: Some(response),
                response_status: Some(
                    if index == 4 {
                        "in_progress"
                    } else {
                        "completed"
                    }
                    .into(),
                ),
                tool_call_id: call,
                tool_name: name.map(str::to_owned),
                model: None,
                finish_reason: None,
                encrypted_bytes: ciphertext.len() as i64,
                content_fingerprint: fingerprint,
            });
        }
        let mut source = CompactionSnapshot {
            conversation_id: 1,
            conversation_uuid: Uuid::from_u128(300),
            user_id: Uuid::from_u128(400),
            items: metadata,
            active_response_uuids: vec![current_response],
            fingerprint: [0; 32],
            checkpoint: None,
        };
        source.fingerprint = source
            .coverage_fingerprint(&source.items.iter().map(|item| item.key).collect::<Vec<_>>())
            .unwrap();
        let input = PromptSource {
            source: &source,
            user_key: &key,
            model: "gpt-oss-120b",
            reasoning_replay: ReasoningReplay::None,
            current_user: source.items[4].key,
            current_response,
            instructions: vec![],
            instruction_tokens: 0,
            incoming: &[],
        };
        let policy = CompactionPolicy {
            input_budget: 2_000,
            trigger_tokens: 1_600,
            keep_recent_tokens: 0,
            summary_input_budget: 512,
        };
        let first = input
            .build(vec![rows[0].clone(), rows[4].clone()], &None, false)
            .unwrap();
        assert_eq!(first.items[0].estimated_tokens, 10); // Ignore the legacy image estimate.
        assert!(first.items[0].source_bytes > policy::MAX_SOURCE_BATCH_BYTES);
        let first_plan =
            context::plan_batch(&first, &None, &policy, true, policy.summary_input_budget).unwrap();
        assert_eq!(first_plan.summary_work[0].items.len(), 1);
        assert_eq!(first_plan.summary_work[0].items[0].key, source.items[0].key);
        let first_payload =
            context::complete_plan(&None, &first_plan, &["Image uploaded.".into()]).unwrap();
        let original_tokens = context::estimate_tokens(&first, &None).unwrap();
        assert!(
            context::estimate_tokens(&first, &Some(first_payload.clone())).unwrap()
                >= original_tokens
        );
        let draft = StagedCheckpoint::new(
            SourceRevision::new(&source),
            first_payload.clone(),
            [(first.items[0].key, first.items[0].estimated_tokens)],
            vec![],
        );

        // The next materialization has only the tool pair and required input;
        // the 9 MiB image row is neither selected again nor held in the draft.
        let mut second = input
            .build(
                vec![rows[1].clone(), rows[2].clone(), rows[4].clone()],
                &Some(first_payload.clone()),
                false,
            )
            .unwrap();
        draft.restore_source_tokens(&mut second);
        let second_plan = context::plan_batch(
            &second,
            &Some(first_payload.clone()),
            &policy,
            true,
            policy.summary_input_budget,
        )
        .unwrap();
        assert_eq!(
            second_plan.summary_work[0]
                .items
                .iter()
                .map(|item| item.key)
                .collect::<Vec<_>>(),
            vec![source.items[1].key, source.items[2].key]
        );
        assert!(
            second_plan.summary_work[0]
                .items
                .iter()
                .map(|item| item.source_bytes)
                .sum::<usize>()
                < policy::MAX_SOURCE_BATCH_BYTES
        );
        assert!(second_plan.summary_work[0].items[0].summary_messages[0]["tool_calls"].is_array());
        assert!(
            second_plan.summary_work[0].items[1].summary_messages[0]["content"]
                .as_str()
                .unwrap()
                .contains("7319")
        );
        let combined = context::complete_plan(
            &Some(first_payload),
            &second_plan,
            &["The uploaded image depicts an amber lighthouse numbered 7319.".into()],
        )
        .unwrap();
        assert!(
            context::estimate_tokens(&second, &Some(combined.clone())).unwrap()
                < context::estimate_tokens(&second, &None).unwrap()
        );
        assert_eq!(
            covered_keys(&combined),
            vec![
                source.items[0].key,
                source.items[1].key,
                source.items[2].key
            ]
        );
        // Discarding the unpublished draft still reconstructs every original row.
        let full = input.build(rows.clone(), &None, true).unwrap();
        assert_eq!(context::project(&full, &None).unwrap().0.len(), 5);
        assert_eq!(source.items.len(), 5);
        assert!(rows[0].content_enc.as_ref().unwrap().len() > 9 * 1024 * 1024);
    }
}
