//! Server-owned conversation checkpoints. Planning is pure, helper inference is
//! explicit paid work, and a fenced DB publication is the only coverage commit.

pub(crate) mod context;
mod executor;
pub(crate) mod policy;
mod summary;

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
            load_instruction_messages, normalize_tool_call_ids_for_model, snapshot_from_raw,
        },
        handlers::ResponsesCreateRequest,
    },
    ApiError,
};
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
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};
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
    calls: AtomicUsize,
    paid_work: AtomicBool,
}

impl CompactionBudget {
    fn begin_call(&self) -> Result<(), ApiError> {
        self.calls
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |calls| {
                (calls < MAX_SUMMARY_CALLS_PER_RESPONSE).then_some(calls + 1)
            })
            .map_err(|_| ApiError::ServiceUnavailable)?;
        self.paid_work.store(true, Ordering::Release);
        Ok(())
    }

    pub fn has_paid_work(&self) -> bool {
        self.paid_work.load(Ordering::Acquire)
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
        .map_err(|_| ApiError::InternalServerError)?;
    let bytes = decrypt_aead_v1(
        &key,
        &record.content_enc,
        &checkpoint_aad(source.user_id, source.conversation_uuid, record.uuid),
    )
    .map_err(|_| ApiError::InternalServerError)?;
    let envelope: CheckpointEnvelope =
        serde_json::from_slice(&bytes).map_err(|_| ApiError::InternalServerError)?;
    let digest = source
        .coverage_fingerprint(&covered_keys(&envelope.payload))
        .map_err(|_| ApiError::InternalServerError)?;
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
        return Err(ApiError::InternalServerError);
    }
    if envelope.allowed_urls.len() > super::tools::MAX_PROMPT_ALLOWED_URLS {
        return Err(ApiError::InternalServerError);
    }
    Ok(Some(envelope))
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

fn may_use_original_prompt(
    error: &SummaryExecutionError,
    forced: bool,
    original_tokens: usize,
    input_budget: usize,
    cancelled: bool,
    expired: bool,
) -> bool {
    !forced
        && original_tokens <= input_budget
        && !cancelled
        && !expired
        && error.can_fallback_to_uncompacted()
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

/// Select and summarize one independently publishable prefix. A tiny historical
/// exchange can be cheaper verbatim than a structured checkpoint; leave it raw
/// and still allow a large completed active tool prefix to make progress.
async fn reducing_checkpoint(
    executor: &SummaryExecutor<'_>,
    snapshot: &context::ContextSnapshot,
    checkpoint: &Option<CheckpointPayload>,
    policy: &CompactionPolicy,
    plan: context::ContextPlan,
    original_tokens: usize,
    non_reducing: &mut HashSet<Vec<ContextItemKey>>,
) -> Result<Option<CheckpointPayload>, SummaryExecutionError> {
    for work in plan.summary_work {
        let needs_history_fold = work.kind == context::SectionKind::Active
            && checkpoint
                .as_ref()
                .and_then(|value| value.active.as_ref())
                .is_some_and(|section| section.anchor != Some(snapshot.current_user));
        if needs_history_fold || non_reducing.contains(&work.covered) {
            continue;
        }
        let summary = executor.summarize(&work, policy).await?;
        let coverage = work.covered.clone();
        let one = context::ContextPlan {
            summary_work: vec![work],
        };
        let next = context::complete_plan(checkpoint, &one, &[summary])
            .map_err(|_| SummaryExecutionError::Unsafe(ApiError::InternalServerError))?;
        let (_, next_tokens) = context::project(snapshot, &Some(next.clone()))
            .map_err(|_| SummaryExecutionError::Unsafe(ApiError::InternalServerError))?;
        if next_tokens < original_tokens {
            return Ok(Some(next));
        }
        // This is a size/progress check, not an extra output-token allowance.
        // Never commit a summary that makes this projection larger.
        non_reducing.insert(coverage);
    }
    Ok(None)
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
    mut force: bool,
) -> Result<PreparedPrompt, ApiError> {
    let (mut policy, enabled) = policy_for(executor.body)?;
    if force {
        policy.keep_recent_tokens = 0;
    }
    let mut publication_conflicts = 0;
    let mut source_races = 0;
    let mut non_reducing = HashSet::new();
    let mut made_progress = false;
    for _ in 0..MAX_PREPARATION_STEPS {
        if executor.execution.is_cancelled() || tokio::time::Instant::now() >= executor.deadline {
            return Err(ApiError::ServiceUnavailable);
        }
        let source = executor
            .state
            .db
            .load_compaction_snapshot(conversation_id, executor.user.uuid)
            .map_err(|_| ApiError::InternalServerError)?;
        let opened = open_checkpoint(&source, user_key)?;
        let checkpoint = opened.as_ref().map(|envelope| envelope.payload.clone());
        let covered: HashSet<_> = checkpoint
            .as_ref()
            .map(covered_keys)
            .unwrap_or_default()
            .into_iter()
            .collect();
        let keys: Vec<_> = source
            .items
            .iter()
            .map(|item| item.key)
            .filter(|key| !covered.contains(key))
            .collect();
        let raw = match executor.state.db.load_compaction_items(
            conversation_id,
            executor.user.uuid,
            &keys,
        ) {
            Ok(raw) => raw,
            Err(DBError::CompactionError(CompactionError::InvalidSelection))
                if source_races < MAX_SOURCE_RACE_RETRIES =>
            {
                source_races += 1;
                continue;
            }
            Err(_) => return Err(ApiError::InternalServerError),
        };
        let (instructions, tokens) = load_instruction_messages(
            executor.state.db.as_ref(),
            conversation_id,
            executor.user.uuid,
            user_key,
            executor.body.instructions.as_deref(),
            Some(internal_instructions),
        )?;
        let mut snapshot = snapshot_from_raw(
            instructions,
            tokens,
            &source.items,
            raw,
            user_key,
            &executor.body.model,
            reasoning_replay,
            ContextItemKey {
                kind: ContextItemKind::User,
                uuid: current_user_uuid,
            },
            current_response_uuid,
            &source.active_response_uuids,
            &checkpoint,
        )?;
        snapshot.items.extend(incoming.iter().cloned());
        validate_required_input(&snapshot, &policy)?;
        let allowed_urls = authoritative_urls(&snapshot, opened.as_ref());
        let (original_messages, original_tokens) =
            context::project(&snapshot, &checkpoint).map_err(|_| ApiError::InternalServerError)?;
        validate_billing_before_helpers(billing_access, original_tokens)?;
        let plan = context::plan_batch(
            &snapshot,
            &checkpoint,
            &policy,
            force,
            policy.summary_input_budget,
        )
        .map_err(|_| ApiError::InternalServerError)?;
        let mut final_checkpoint = checkpoint.clone();
        let mut published = false;
        // Publish each complete section/prefix before beginning another. A later
        // helper failure retains this progress for the next request.
        let candidate = if enabled {
            match reducing_checkpoint(
                executor,
                &snapshot,
                &checkpoint,
                &policy,
                plan,
                original_tokens,
                &mut non_reducing,
            )
            .await
            {
                Ok(candidate) => candidate,
                Err(error)
                    if may_use_original_prompt(
                        &error,
                        force,
                        original_tokens,
                        policy.input_budget,
                        executor.execution.is_cancelled(),
                        tokio::time::Instant::now() >= executor.deadline,
                    ) =>
                {
                    tracing::warn!(response_uuid = %current_response_uuid,
                            "Advisory compaction unavailable; retaining complete fitting prompt");
                    return finish_prompt(
                        executor,
                        conversation_id,
                        current_response_uuid,
                        original_messages,
                        original_tokens,
                        allowed_urls,
                    );
                }
                Err(error) => return Err(error.into_api_error()),
            }
        } else {
            None
        };
        if let Some(next) = candidate {
            let covered_items = covered_keys(&next);
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
                allowed_urls: durable_urls(&snapshot, opened.as_ref(), &covered_items),
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
                    previous_covered_items: checkpoint
                        .as_ref()
                        .map(covered_keys)
                        .unwrap_or_default(),
                    source_fingerprint: digest,
                    current_response_uuid: Some(current_response_uuid),
                    origin_response_uuid: current_response_uuid,
                    after_item_uuid,
                })
                .map_err(|_| ApiError::InternalServerError)?;
            match publication {
                CompactionPublishResult::Conflict => {
                    publication_conflicts += 1;
                    if publication_conflicts > MAX_PUBLICATION_CONFLICTS {
                        return Err(ApiError::ServiceUnavailable);
                    }
                    tracing::debug!(response_uuid = %current_response_uuid, "Compaction checkpoint publication conflicted; replanning");
                    continue;
                }
                CompactionPublishResult::Published(record) => {
                    tracing::info!(response_uuid = %current_response_uuid, checkpoint_uuid = %record.uuid,
                        generation = record.generation, covered_items = covered_keys(&next).len(),
                        summary_tokens, forced = force, "Responses compaction checkpoint published");
                    final_checkpoint = Some(next);
                    published = true;
                    made_progress = true;
                }
            }
        }
        let (messages, estimated_tokens) = context::project(&snapshot, &final_checkpoint)
            .map_err(|_| ApiError::InternalServerError)?;
        if published && force {
            // A provider rejection can reveal tokenizer undercount even when our
            // estimate fits. Finish the eligible forced prefixes before the one
            // main retry rather than trusting that estimate after a single batch.
            continue;
        }
        if estimated_tokens > policy.input_budget {
            if published {
                // This prefix is durable even if the next bounded batch fails.
                continue;
            }
            if enabled && !force {
                policy.keep_recent_tokens = 0;
                policy.trigger_tokens = 0;
                force = true;
                // Replan on a fresh snapshot; no accepted main request is replayed.
                continue;
            }
            return Err(if non_reducing.is_empty() {
                ApiError::MessageExceedsContextLimit
            } else {
                ApiError::ServiceUnavailable
            });
        }
        validate_forced_progress(force, made_progress, !non_reducing.is_empty())?;
        return finish_prompt(
            executor,
            conversation_id,
            current_response_uuid,
            messages,
            estimated_tokens,
            allowed_urls,
        );
    }
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
    }
}
