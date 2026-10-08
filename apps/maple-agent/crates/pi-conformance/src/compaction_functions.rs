//! Input-only compaction replay matching compaction-dispatch.ts observation wrappers.
//! Decoder/script errors are harness failures; only selected Pi errors become observations.
use pi_agent_core::types::{AgentError, AgentMessage, AgentResult, StreamFn};
use pi_ai::{
    env::{CancellationToken, PiEnv},
    types::{
        AssistantMessage, AssistantMessageEvent, Context, DoneReason, ErrorReason, Message, Model,
        SimpleStreamOptions, StopReason, TranscriptContext,
    },
    utils::{
        event_stream::create_assistant_message_event_stream,
        js_value::{JsObject, JsString, JsValue, from_js_value, to_js_value},
        json_parse,
        transcript::normalize_context,
        uuid::UuidV7Generator,
    },
};
use pi_coding_agent::core::{
    compaction::{
        branch_summarization::{self as branch, GenerateBranchSummaryOptions},
        compaction::{self as compaction, CompactionPreparation, SummaryOptions, SummaryRuntime},
        utils,
    },
    session_manager::{
        ReadonlySessionManager, SessionEntry, SessionHeader, SessionProjection, SessionTreeNode,
        build_session_projection,
    },
};
use serde::{Serialize, de::DeserializeOwned};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

type ReplayResult<T> = Result<T, String>;

pub const FUNCTION_IDS: &[&str] = &[
    "compaction.calculateContextTokens",
    "compaction.getLastAssistantUsage",
    "compaction.estimateContextTokens",
    "compaction.estimateProjectedContextTokens",
    "compaction.estimateTokens",
    "compaction.shouldCompact",
    "compaction.findCutPoint",
    "compaction.prepareCompaction",
    "compaction-utils.serializeConversation",
    "compaction.serializeConversation",
    "compaction-utils.fileOperations",
    "branch-summarization.prepareBranchEntries",
    "branch-summarization.collectEntriesForBranchSummary",
    "compaction.summarization",
    "branch-summarization.generateBranchSummary",
    "compaction.retryRequestOwnership",
];

fn typed<T: DeserializeOwned>(value: &JsValue) -> ReplayResult<T> {
    from_js_value(value.clone()).map_err(|error| error.to_string())
}
fn encoded(value: &impl Serialize) -> ReplayResult<JsValue> {
    to_js_value(value).map_err(|error| error.to_string())
}
fn object(entries: impl IntoIterator<Item = (&'static str, JsValue)>) -> JsValue {
    let mut result = JsObject::new();
    for (key, value) in entries {
        result.insert(key, value);
    }
    JsValue::Object(result)
}
fn required<'a>(value: &'a JsValue, key: &str) -> ReplayResult<&'a JsValue> {
    value
        .get(key)
        .ok_or_else(|| format!("Missing fixture field {key}"))
}
fn field<T: DeserializeOwned>(value: &JsValue, key: &str) -> ReplayResult<T> {
    typed(required(value, key)?)
}
fn optional<T: DeserializeOwned>(value: &JsValue, key: &str) -> ReplayResult<Option<T>> {
    value
        .get(key)
        .filter(|v| !v.is_null())
        .map(typed)
        .transpose()
}
fn number(value: &JsValue, key: &str) -> ReplayResult<f64> {
    required(value, key)?
        .as_f64()
        .ok_or_else(|| format!("{key} must be a number"))
}
fn index(value: &JsValue, key: &str) -> ReplayResult<usize> {
    let n = number(value, key)?;
    if n.is_finite() && n >= 0.0 && n.fract() == 0.0 && n <= usize::MAX as f64 {
        Ok(n as usize)
    } else {
        Err(format!("{key} must be a nonnegative integer"))
    }
}
fn messages<T: DeserializeOwned>(input: &JsValue) -> ReplayResult<Vec<T>> {
    if let Some(raw) = input.get("messagesJson") {
        let text = raw.as_js_str().ok_or("messagesJson must be a string")?;
        let parsed = json_parse::parse_json_utf16(text).map_err(|error| error.to_string())?;
        typed(&parsed)
    } else {
        field(input, "messages")
    }
}
fn value<T: Serialize>(output: T) -> ReplayResult<JsValue> {
    Ok(object([("value", encoded(&output)?)]))
}
fn error(error: AgentError) -> JsValue {
    object([
        ("error", error.message.into()),
        ("errorClass", error.name.into()),
    ])
}
fn invocation<T: Serialize>(output: AgentResult<T>) -> ReplayResult<JsValue> {
    match output {
        Ok(output) => value(output),
        Err(cause) => Ok(error(cause)),
    }
}
fn maybe_value<T: Serialize>(output: Option<T>) -> ReplayResult<JsValue> {
    match output {
        Some(output) => value(output),
        None => Ok(JsValue::Object(JsObject::new())),
    }
}
fn maybe_invocation<T: Serialize>(output: AgentResult<Option<T>>) -> ReplayResult<JsValue> {
    match output {
        Ok(output) => maybe_value(output),
        Err(cause) => Ok(error(cause)),
    }
}

#[derive(Default)]
struct ScriptState {
    responses: VecDeque<(AssistantMessage, DoneReason)>,
    requests: Vec<JsValue>,
    fixture_error: Option<String>,
}
struct Script {
    state: Arc<Mutex<ScriptState>>,
    stream_fn: StreamFn,
}
impl Script {
    fn new(input: &JsValue) -> ReplayResult<Self> {
        let responses: Vec<AssistantMessage> = field(input, "responses")?;
        let reasons: Vec<DoneReason> = field(input, "doneReasons")?;
        if responses.len() != reasons.len() {
            return Err("Response/done-reason counts differ".into());
        }
        let state = Arc::new(Mutex::new(ScriptState {
            responses: responses.into_iter().zip(reasons).collect(),
            ..Default::default()
        }));
        let capture = state.clone();
        let stream_fn: StreamFn = Arc::new(move |model, context, options| {
            let request = (|| {
                Ok::<_, String>(object([
                    ("model", encoded(&model)?),
                    ("context", encoded(&context)?),
                    ("options", encoded(&options)?),
                ]))
            })();
            let mut state = capture.lock().unwrap();
            let failure = match request {
                Ok(request) => {
                    state.requests.push(request);
                    None
                }
                Err(cause) => Some(cause),
            };
            let response = state.responses.pop_front();
            if let Some(cause) = failure.or_else(|| {
                response
                    .is_none()
                    .then(|| "Compaction scripted responses exhausted".into())
            }) {
                state.fixture_error = Some(cause.clone());
                return Box::pin(async move { Err(AgentError::new(cause)) });
            }
            drop(state);
            let (message, reason) = response.unwrap();
            let stream = create_assistant_message_event_stream();
            let writer = stream.writer();
            stream.set_producer(async move {
                tokio::task::yield_now().await;
                writer.push(AssistantMessageEvent::Done { reason, message });
            });
            Box::pin(async move { Ok(stream) })
        });
        Ok(Self { state, stream_fn })
    }
    fn requests(&self) -> ReplayResult<JsValue> {
        let state = self.state.lock().unwrap();
        if let Some(cause) = &state.fixture_error {
            return Err(cause.clone());
        }
        Ok(JsValue::Array(state.requests.clone()))
    }
}

fn summary_options(input: &JsValue) -> ReplayResult<SummaryOptions> {
    Ok(SummaryOptions {
        api_key: optional(input, "apiKey")?,
        headers: optional(input, "headers")?,
        signal: None,
        custom_instructions: optional(input, "customInstructions")?,
        previous_summary: optional(input, "previousSummary")?,
        thinking_level: optional(input, "thinkingLevel")?,
        env: optional(input, "env")?,
        retry: optional(input, "retry")?,
        session_id: optional(input, "sessionId")?,
    })
}
async fn summaries(
    input: &JsValue,
    env: Arc<dyn PiEnv>,
    uuid: Arc<UuidV7Generator>,
) -> ReplayResult<JsValue> {
    let script = Script::new(input)?;
    let runtime = SummaryRuntime::with_uuid(script.stream_fn.clone(), env, uuid);
    let model: Model = field(input, "model")?;
    let options = summary_options(required(input, "options")?)?;
    let current_messages: Vec<AgentMessage> = messages(input)?;
    let operation: String = field(input, "operation")?;
    let times = if input.get("times").is_some() {
        index(input, "times")?
    } else {
        1
    };
    if times == 0 {
        return Err("Invalid invocation count".into());
    }
    let preparation: Option<CompactionPreparation> = optional(input, "preparation")?;
    let context: Option<Context> = optional(input, "context")?;
    let context = context.map(normalize_context);
    let mut results = Vec::with_capacity(times);
    for _ in 0..times {
        let result = match operation.as_str() {
            "generateSummary" => invocation(
                compaction::generate_summary(
                    &current_messages,
                    &model,
                    number(input, "reserveTokens")?,
                    &options,
                    &runtime,
                    None,
                )
                .await,
            )?,
            "generateSummaryWithUsage" => invocation(
                compaction::generate_summary_with_usage(
                    &current_messages,
                    &model,
                    number(input, "reserveTokens")?,
                    &options,
                    &runtime,
                    None,
                )
                .await,
            )?,
            "compact" => invocation(
                compaction::compact(
                    preparation
                        .as_ref()
                        .ok_or("Missing compaction preparation")?,
                    &model,
                    &options,
                    &runtime,
                    None,
                )
                .await,
            )?,
            "completeSummarization" => {
                let request_options: SimpleStreamOptions = field(input, "options")?;
                invocation(
                    compaction::complete_summarization(
                        &model,
                        context
                            .as_ref()
                            .ok_or("Missing completeSummarization context")?
                            .clone(),
                        request_options,
                        &runtime,
                        options.retry.as_ref(),
                        None,
                    )
                    .await,
                )?
            }
            other => return Err(format!("Unsupported compaction operation {other}")),
        };
        // A mock construction failure must never be mistaken for a Pi error observation.
        script.requests()?;
        results.push(result);
    }
    value(object([
        ("results", JsValue::Array(results)),
        ("requests", script.requests()?),
    ]))
}

fn retry_request_snapshot(
    model: &Model,
    context: &TranscriptContext,
    options: &SimpleStreamOptions,
    header: &str,
) -> ReplayResult<JsValue> {
    Ok(object([
        ("modelName", model.name.clone().into()),
        ("messageCount", (context.messages.len() as f64).into()),
        ("maxTokens", encoded(&options.max_tokens)?),
        (
            "header",
            encoded(
                &options
                    .headers
                    .as_ref()
                    .and_then(|headers| headers.get(header))
                    .cloned()
                    .flatten(),
            )?,
        ),
    ]))
}
async fn retry_request_ownership(
    input: &JsValue,
    env: Arc<dyn PiEnv>,
    uuid: Arc<UuidV7Generator>,
) -> ReplayResult<JsValue> {
    let model: Model = field(input, "model")?;
    let context = normalize_context(field::<Context>(input, "context")?);
    let options: SimpleStreamOptions = field(input, "options")?;
    let retry = field(input, "retry")?;
    let changes = required(input, "mutations")?;
    let changed_name: String = field(changes, "modelName")?;
    let append: Message = field(changes, "appendMessage")?;
    let changed_tokens = number(changes, "maxTokens")?;
    let header: String = field(changes, "headerName")?;
    let changed_header: String = field(changes, "headerValue")?;
    let responses: Vec<AssistantMessage> = field(input, "responses")?;
    let requests: Arc<Mutex<Vec<JsValue>>> = Arc::default();
    let fixture_error: Arc<Mutex<Option<String>>> = Arc::default();
    let capture = requests.clone();
    let capture_error = fixture_error.clone();
    let header_for_callback = header.clone();
    let stream_fn: StreamFn = Arc::new(
        move |mut request_model, mut request_context, request_options| {
            let prepared = (|| -> ReplayResult<(usize, SimpleStreamOptions)> {
                let mut request_options =
                    request_options.ok_or("Retry ownership fixture requires request options")?;
                let mut calls = capture.lock().unwrap();
                let index = calls.len();
                calls.push(retry_request_snapshot(
                    &request_model,
                    &request_context,
                    &request_options,
                    &header_for_callback,
                )?);
                if index == 0 {
                    request_model.name = changed_name.clone();
                    request_context.messages.push(append.clone());
                    request_options.max_tokens = Some(changed_tokens);
                    request_options
                        .headers
                        .as_mut()
                        .ok_or("Retry ownership fixture requires headers")?
                        .insert(header_for_callback.clone(), Some(changed_header.clone()));
                }
                Ok((index, request_options))
            })();
            let result = prepared.and_then(|(index, _)| {
                responses
                    .get(index)
                    .cloned()
                    .ok_or_else(|| "Retry ownership responses exhausted".into())
            });
            let response = match result {
                Ok(response) => response,
                Err(cause) => {
                    *capture_error.lock().unwrap() = Some(cause.clone());
                    return Box::pin(async move { Err(AgentError::new(cause)) });
                }
            };
            let event = match response.stop_reason {
                StopReason::Error => AssistantMessageEvent::Error {
                    reason: ErrorReason::Error,
                    error: response,
                },
                StopReason::Aborted => AssistantMessageEvent::Error {
                    reason: ErrorReason::Aborted,
                    error: response,
                },
                StopReason::Length => AssistantMessageEvent::Done {
                    reason: DoneReason::Length,
                    message: response,
                },
                StopReason::ToolUse => AssistantMessageEvent::Done {
                    reason: DoneReason::ToolUse,
                    message: response,
                },
                StopReason::Stop => AssistantMessageEvent::Done {
                    reason: DoneReason::Stop,
                    message: response,
                },
                StopReason::Pending | StopReason::Deferred => {
                    let cause =
                        "Retry ownership fixture requires a terminal stop reason".to_owned();
                    *capture_error.lock().unwrap() = Some(cause.clone());
                    return Box::pin(async move { Err(AgentError::new(cause)) });
                }
            };
            let stream = create_assistant_message_event_stream();
            let writer = stream.writer();
            stream.set_producer(async move {
                writer.push(event);
            });
            Box::pin(async move { Ok(stream) })
        },
    );
    let runtime = SummaryRuntime::with_uuid(stream_fn, env, uuid);
    let response = compaction::complete_summarization(
        &model,
        context.clone(),
        options.clone(),
        &runtime,
        Some(&retry),
        None,
    )
    .await;
    if let Some(cause) = fixture_error.lock().unwrap().clone() {
        return Err(cause);
    }
    match response {
        Err(cause) => Ok(error(cause)),
        Ok(response) => value(object([
            ("calls", JsValue::Array(requests.lock().unwrap().clone())),
            (
                "callerAfter",
                retry_request_snapshot(&model, &context, &options, &header)?,
            ),
            ("stopReason", encoded(&response.stop_reason)?),
        ])),
    }
}

struct BranchFixture {
    entries: JsObject,
    branches: JsObject,
}
impl ReadonlySessionManager for BranchFixture {
    fn get_entry(&self, id: &JsString) -> Option<SessionEntry> {
        self.entries
            .get(id.clone())
            .map(|entry| typed(entry).expect("entry fixture validated"))
    }
    fn get_branch(&self, from_id: Option<&JsString>) -> Vec<SessionEntry> {
        self.branches
            .get(from_id.cloned().unwrap_or_default())
            .map(|branch| typed(branch).expect("branch fixture validated"))
            .unwrap_or_default()
    }
    fn get_cwd(&self) -> &str {
        unreachable!("collection does not call get_cwd")
    }
    fn get_session_dir(&self) -> &str {
        unreachable!("collection does not call get_session_dir")
    }
    fn get_session_id(&self) -> JsString {
        unreachable!("collection does not call get_session_id")
    }
    fn get_session_file(&self) -> Option<String> {
        unreachable!("collection does not call get_session_file")
    }
    fn get_leaf_id(&self) -> Option<JsString> {
        unreachable!("collection does not call get_leaf_id")
    }
    fn get_leaf_entry(&self) -> Option<SessionEntry> {
        unreachable!("collection does not call get_leaf_entry")
    }
    fn get_label(&self, _: &JsString) -> Option<JsString> {
        unreachable!("collection does not call get_label")
    }
    fn build_context_entries(&self) -> Vec<SessionEntry> {
        unreachable!("collection does not call build_context_entries")
    }
    fn build_session_projection(&self) -> SessionProjection {
        unreachable!("collection does not call build_session_projection")
    }
    fn get_header(&self) -> Option<SessionHeader> {
        unreachable!("collection does not call get_header")
    }
    fn get_entries(&self) -> Vec<SessionEntry> {
        unreachable!("collection does not call get_entries")
    }
    fn get_tree(&self) -> Vec<SessionTreeNode> {
        unreachable!("collection does not call get_tree")
    }
    fn get_session_name(&self) -> Option<JsString> {
        unreachable!("collection does not call get_session_name")
    }
}

/// Supply a fresh env/UUID per case when the TS recorder resets its module graph.
/// Within one case, scripted repeated invocations share this exact UUID state.
pub async fn dispatch(
    id: &str,
    input: &JsValue,
    env: Arc<dyn PiEnv>,
    uuid: Arc<UuidV7Generator>,
) -> ReplayResult<JsValue> {
    match id {
        "compaction.calculateContextTokens" => invocation(
            compaction::calculate_context_tokens_raw(required(input, "usage")?),
        ),
        "compaction.getLastAssistantUsage" => {
            maybe_value(compaction::get_last_assistant_usage(&field::<
                Vec<SessionEntry>,
            >(
                input, "entries"
            )?))
        }
        "compaction.estimateContextTokens" => {
            invocation(compaction::estimate_context_tokens(&messages::<
                AgentMessage,
            >(input)?))
        }
        "compaction.estimateTokens" => {
            invocation(compaction::estimate_tokens(&field(input, "message")?))
        }
        "compaction.shouldCompact" => value(compaction::should_compact(
            number(input, "contextTokens")?,
            number(input, "contextWindow")?,
            &field(input, "settings")?,
        )),
        "compaction.findCutPoint" => invocation(compaction::find_cut_point(
            &field::<Vec<SessionEntry>>(input, "entries")?,
            index(input, "startIndex")?,
            index(input, "endIndex")?,
            number(input, "keepRecentTokens")?,
        )),
        "compaction.prepareCompaction" => maybe_invocation(compaction::prepare_compaction(
            &field::<Vec<SessionEntry>>(input, "entries")?,
            &field(input, "settings")?,
        )),
        "compaction.estimateProjectedContextTokens" => {
            let entries: Vec<SessionEntry> = field(input, "entries")?;
            invocation(compaction::estimate_projected_context_tokens(
                &build_session_projection(&entries, None, None),
                &entries,
            ))
        }
        "compaction.serializeConversation" | "compaction-utils.serializeConversation" => {
            invocation(utils::serialize_conversation(&messages::<Message>(input)?))
        }
        "compaction-utils.fileOperations" => {
            let mut file_ops = utils::create_file_ops();
            for message in messages::<AgentMessage>(input)? {
                utils::extract_file_ops_from_message(&message, &mut file_ops);
            }
            let lists = utils::compute_file_lists(&file_ops);
            value(object([
                ("fileOps", encoded(&file_ops)?),
                ("readFiles", encoded(&lists.read_files)?),
                ("modifiedFiles", encoded(&lists.modified_files)?),
                (
                    "formatted",
                    utils::format_file_operations(&lists.read_files, &lists.modified_files).into(),
                ),
            ]))
        }
        "branch-summarization.prepareBranchEntries" => invocation(branch::prepare_branch_entries(
            &field::<Vec<SessionEntry>>(input, "entries")?,
            number(input, "tokenBudget")?,
        )),
        "branch-summarization.collectEntriesForBranchSummary" => {
            let entries: JsObject = field(input, "entriesById")?;
            let branches: JsObject = field(input, "branches")?;
            // Validate stub fixture decoding before invoking the selected function.
            for (_, entry) in entries.iter() {
                let _: SessionEntry = typed(entry)?;
            }
            for (_, items) in branches.iter() {
                let _: Vec<SessionEntry> = typed(items)?;
            }
            let session = BranchFixture { entries, branches };
            value(branch::collect_entries_for_branch_summary(
                &session,
                optional::<JsString>(input, "oldLeafId")?.as_ref(),
                &field(input, "targetId")?,
            ))
        }
        "compaction.summarization" => summaries(input, env, uuid).await,
        "compaction.retryRequestOwnership" => retry_request_ownership(input, env, uuid).await,
        "branch-summarization.generateBranchSummary" => {
            let script = Script::new(input)?;
            let runtime = SummaryRuntime::with_uuid(script.stream_fn.clone(), env, uuid);
            let input_options = required(input, "options")?;
            let mut options = GenerateBranchSummaryOptions::new(
                field(input_options, "model")?,
                CancellationToken::new(),
            );
            options.api_key = optional(input_options, "apiKey")?;
            options.headers = optional(input_options, "headers")?;
            options.env = optional(input_options, "env")?;
            options.custom_instructions = optional(input_options, "customInstructions")?;
            options.replace_instructions =
                optional(input_options, "replaceInstructions")?.unwrap_or(false);
            options.reserve_tokens = optional(input_options, "reserveTokens")?;
            options.retry = optional(input_options, "retry")?;
            let result = invocation(
                branch::generate_branch_summary(
                    &field::<Vec<SessionEntry>>(input, "entries")?,
                    &options,
                    &runtime,
                    None,
                )
                .await,
            )?;
            value(object([
                ("result", result),
                ("requests", script.requests()?),
            ]))
        }
        _ => Err(format!("Unsupported compaction function {id}")),
    }
}
