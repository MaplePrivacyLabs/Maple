//! Maple's model provider: Pi's Chat Completions API over OpenSecret's
//! encrypted transport, and Maple's model catalog as Pi models.
//!
//! Pi's session owns retries and context-overflow recovery, and it decides
//! both by reading the error text of a failed response. This adapter turns
//! every OpenSecret and HTTP failure into one fixed Maple message per
//! category, worded so those checks classify it: a rate limit or a server or
//! network failure reads as transient, an oversized request as an overflow,
//! and authentication, credit and secure-connection failures as neither. The
//! fixed wording also keeps response bodies, which may echo decrypted
//! content, out of the timeline and the logs.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use maple_sdk::{InferenceRequest, InferenceResponse, OpenSecretClient, OpenSecretResponseBody};
use pi_ai::openai::{HttpRequest, HttpResponse, HttpTransport, OpenAiCompletions};
use pi_ai::{
    AssistantContent, AssistantMessageEvent, AssistantMessageStream, Content, Context,
    InputModality, MaxTokensField, Message, Model, ModelCompat, StreamFn, StreamOptions,
};
use pi_coding_agent::{ApiKeySource, ModelRegistry};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// The provider name Maple's models carry in Pi.
pub(crate) const MAPLE_PROVIDER: &str = "maple";
/// The API name Maple's stream function is registered under.
pub(crate) const MAPLE_API: &str = "maple";
/// The base URL of Maple's models. OpenSecret routes by path, so the
/// transport only ever sends to [`CHAT_COMPLETIONS_PATH`].
const MAPLE_BASE_URL: &str = "/v1";
const CHAT_COMPLETIONS_PATH: &str = "/v1/chat/completions";

pub(crate) const AUTHENTICATION_ERROR_MESSAGE: &str = "Maple authentication failed";
pub(crate) const CREDITS_EXHAUSTED_MESSAGE: &str = "Maple credits are exhausted";
/// Pi's overflow check recognizes "exceeds the context window".
pub(crate) const CONTEXT_OVERFLOW_MESSAGE: &str =
    "The request exceeds the context window of this model";
pub(crate) const RATE_LIMIT_MESSAGE: &str = "Maple rate limit exceeded";
pub(crate) const ENDPOINT_NOT_FOUND_MESSAGE: &str = "The Maple inference endpoint was not found";
pub(crate) const REQUEST_TIMEOUT_MESSAGE: &str = "The Maple request timed out";
pub(crate) const CONNECT_ERROR_MESSAGE: &str = "Network error: could not connect to Maple";
pub(crate) const NETWORK_ERROR_MESSAGE: &str = "Network error: the Maple request failed";
pub(crate) const STREAM_TIMEOUT_MESSAGE: &str = "Maple's response stream timed out";
pub(crate) const STREAM_ERROR_MESSAGE: &str =
    "Network error: Maple's encrypted response stream failed";
pub(crate) const ATTESTATION_VERIFICATION_ERROR_MESSAGE: &str =
    "Maple could not verify the secure server connection";
pub(crate) const DEVICE_CLOCK_ERROR_MESSAGE: &str = "Maple could not verify the secure server connection because this device's date or time looks wrong. Check the device's date, time and time zone settings, then try again";
pub(crate) const SECURE_CONNECTION_ERROR_MESSAGE: &str =
    "Maple's encrypted connection could not be recovered";
const PREPARE_ERROR_MESSAGE: &str = "Maple could not prepare the encrypted request";
const CANCELLED_MESSAGE: &str = "Maple request cancelled";

const ERROR_CONTRACT_HEADER: &str = "x-opensecret-error-contract";
const ERROR_CODE_HEADER: &str = "x-opensecret-error-code";
const ERROR_CONTRACT_VERSION: &[u8] = b"1";
const SESSION_NOT_FOUND_ERROR_CODE: &[u8] = b"session_not_found";

const MAX_ERROR_BODY_BYTES: usize = 16 * 1024;
#[cfg(not(test))]
const RESPONSE_START_TIMEOUT: Duration = Duration::from_secs(300);
#[cfg(test)]
const RESPONSE_START_TIMEOUT: Duration = Duration::from_millis(200);
#[cfg(not(test))]
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
#[cfg(test)]
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_millis(200);

/// Context window for a model the catalog says nothing about.
pub(crate) const DEFAULT_CONTEXT_WINDOW: u64 = 128_000;

/// How many tool-produced images the newest turns may still carry as pixels.
///
/// A desktop task calls an observation tool on nearly every step, and each
/// result holds a full screenshot. The transcript keeps tool results
/// verbatim, so without a bound every screenshot would be re-uploaded on
/// every later turn.
const MAX_RETAINED_TOOL_RESULT_IMAGES: usize = 3;
const SUPERSEDED_IMAGE_MARKER: &str = "[Earlier screenshot omitted from this request. Take a new observation if the current screen matters.]";

const KIMI_K3_MODEL_ID: &str = "kimi-k3";

/// Authenticated, encrypted delivery for a caller-owned OpenSecret inference request.
///
/// The provider knows nothing about token storage or refresh. The account's
/// auth session implements this trait and selects its current SDK client at
/// the start of every call, retries included.
#[async_trait]
pub(crate) trait MapleInferenceTransport: Send + Sync {
    async fn send_inference_request(
        self: Arc<Self>,
        request: InferenceRequest,
        cancel_token: CancellationToken,
    ) -> maple_sdk::Result<InferenceResponse>;
}

/// A direct SDK client is also a valid transport.
#[async_trait]
impl MapleInferenceTransport for OpenSecretClient {
    async fn send_inference_request(
        self: Arc<Self>,
        request: InferenceRequest,
        cancel_token: CancellationToken,
    ) -> maple_sdk::Result<InferenceResponse> {
        tokio::select! {
            biased;
            _ = cancel_token.cancelled() => {
                Err(maple_sdk::Error::Other("Inference request was cancelled".to_string()))
            }
            response = OpenSecretClient::send_inference_request(&self, request) => response,
        }
    }
}

/// The registry of Maple's models for one account: Maple's stream function
/// over that account's transport, and the catalog's models.
pub(crate) fn maple_model_registry(
    transport: Arc<dyn MapleInferenceTransport>,
    models: impl IntoIterator<Item = Model>,
) -> ModelRegistry {
    let registry = ModelRegistry::new(Arc::new(NoKeys));
    registry.register_api(MAPLE_API, Arc::new(MapleStreamFn::new(transport)));
    registry.register_models(models);
    registry
}

/// OpenSecret authenticates the account itself; Pi passes no key.
struct NoKeys;

#[async_trait]
impl ApiKeySource for NoKeys {
    async fn api_key(&self, _provider: &str) -> Option<String> {
        None
    }
}

/// What the catalog says about one model, after resolving an alias.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CatalogEntry {
    /// The context window in tokens; `None` when the catalog does not say.
    pub context_window: Option<u64>,
    /// Whether the model sees images; `None` when the catalog does not say.
    pub vision: Option<bool>,
}

/// The catalog's view of `model_id`. An alias resolves to its target; an
/// explicit alias capability, `false` included, overrides the target's.
pub(crate) fn catalog_entry(
    catalog: &maple_sdk::ModelCatalogResponse,
    model_id: &str,
) -> Option<CatalogEntry> {
    let mut concrete_id = model_id;
    let mut alias_vision = None;
    for alias in &catalog.aliases {
        if alias.id == model_id {
            alias_vision = alias
                .capabilities
                .as_ref()
                .map(|capabilities| capabilities.vision);
            if let Some(target) = alias.target_model.as_deref()
                && !target.trim().is_empty()
            {
                concrete_id = target;
            }
            break;
        }
    }
    let model = catalog.data.iter().find(|model| model.id == concrete_id)?;
    Some(CatalogEntry {
        context_window: reconcile_context_limit(model.context_window, model.max_context_tokens),
        vision: alias_vision.or_else(|| {
            model
                .capabilities
                .as_ref()
                .map(|capabilities| capabilities.vision)
        }),
    })
}

/// Maple's context-limit rule: both fields present and equal is the value,
/// exactly one present wins, and absent or disagreeing metadata is unknown.
pub(crate) fn reconcile_context_limit(
    context_window: Option<u64>,
    max_context_tokens: Option<u64>,
) -> Option<u64> {
    let context_window = context_window.filter(|value| *value > 0);
    let max_context_tokens = max_context_tokens.filter(|value| *value > 0);
    match (context_window, max_context_tokens) {
        (None, None) => None,
        (Some(value), None) | (None, Some(value)) => Some(value),
        (Some(a), Some(b)) if a == b => Some(a),
        (Some(_), Some(_)) => None,
    }
}

/// The Pi model for a Maple model id.
///
/// The first version sends no thinking control (owner decision, 2026-10-07),
/// so every model is declared without reasoning levels and thinks at its own
/// default. No output cap is sent either; the server applies its own.
/// `context_override` is the developer's `MAPLE_CONTEXT_LIMIT`.
pub(crate) fn maple_model(
    model_id: &str,
    entry: Option<&CatalogEntry>,
    context_override: Option<u64>,
) -> Model {
    let context_window = context_override
        .or_else(|| entry.and_then(|entry| entry.context_window))
        .unwrap_or(DEFAULT_CONTEXT_WINDOW);
    let mut input = vec![InputModality::Text];
    if entry.and_then(|entry| entry.vision).unwrap_or(false) {
        input.push(InputModality::Image);
    }
    Model {
        id: model_id.to_string(),
        name: model_id.to_string(),
        api: MAPLE_API.to_string(),
        provider: MAPLE_PROVIDER.to_string(),
        base_url: MAPLE_BASE_URL.to_string(),
        reasoning: false,
        input,
        cost: Default::default(),
        context_window,
        max_tokens: 0,
        thinking_levels: Default::default(),
        compat: ModelCompat {
            supports_developer_role: false,
            max_tokens_field: MaxTokensField::MaxTokens,
            supports_usage_in_streaming: true,
            supports_reasoning_effort: false,
            supports_mid_conversation_system_messages: false,
            // Maple's backends do not advertise strict tool schemas.
            supports_strict_mode: false,
        },
    }
}

/// Pi's Chat Completions provider over the account's transport, plus the
/// request and response adjustments Maple's models need.
pub(crate) struct MapleStreamFn {
    completions: OpenAiCompletions,
}

impl MapleStreamFn {
    pub(crate) fn new(transport: Arc<dyn MapleInferenceTransport>) -> Self {
        Self {
            completions: OpenAiCompletions::new(Arc::new(MapleTransport { transport })),
        }
    }
}

impl StreamFn for MapleStreamFn {
    fn stream(
        &self,
        model: &Model,
        mut context: Context,
        options: StreamOptions,
    ) -> AssistantMessageStream {
        bound_tool_result_images(&mut context.messages);
        let stream = self.completions.stream(model, context, options);
        if model.id == KIMI_K3_MODEL_ID {
            return replace_legacy_kimi_k3_tool_ids(stream);
        }
        stream
    }
}

/// Replace all but the newest tool-result images with a short text marker.
///
/// Only tool output is bounded. An image the user attached stays a real image
/// for as long as the conversation does, and the stored transcript keeps
/// every screenshot; this bounds only what one request carries.
fn bound_tool_result_images(messages: &mut [Message]) {
    let mut retained = 0usize;
    for message in messages.iter_mut().rev() {
        let Message::ToolResult(result) = message else {
            continue;
        };
        for block in result.content.iter_mut().rev() {
            if !matches!(block, Content::Image(_)) {
                continue;
            }
            if retained < MAX_RETAINED_TOOL_RESULT_IMAGES {
                retained += 1;
                continue;
            }
            *block = Content::text(SUPERSEDED_IMAGE_MARKER);
        }
    }
}

/// Temporary compatibility for Kimi K3 deployments built before vLLM
/// ab98034d4, where tool IDs are scoped to one assistant message and repeat
/// across turns. Each distinct legacy ID in one response gets a random
/// `chatcmpl-tool-` ID, the way current vLLM names calls, while a repeat of
/// the same ID within the response stays a repeat. Remove this once the
/// attested K3 image includes vLLM #50420.
fn replace_legacy_kimi_k3_tool_ids(mut inner: AssistantMessageStream) -> AssistantMessageStream {
    let (sender, stream) = AssistantMessageStream::channel();
    tokio::spawn(async move {
        let mut replacements: HashMap<String, String> = HashMap::new();
        let mut names: HashMap<usize, String> = HashMap::new();
        while let Some(mut event) = inner.next().await {
            match &mut event {
                AssistantMessageEvent::ToolCallStart { index, id, name } => {
                    names.insert(*index, name.clone());
                    if is_legacy_kimi_tool_id(id, name) {
                        *id = kimi_replacement(&mut replacements, id);
                    }
                }
                AssistantMessageEvent::ToolCallEnd { tool_call, .. } => {
                    if is_legacy_kimi_tool_id(&tool_call.id, &tool_call.name) {
                        tool_call.id = kimi_replacement(&mut replacements, &tool_call.id);
                    }
                }
                AssistantMessageEvent::Start { message }
                | AssistantMessageEvent::Done { message }
                | AssistantMessageEvent::Error { message } => {
                    for block in &mut message.content {
                        if let AssistantContent::ToolCall(call) = block
                            && is_legacy_kimi_tool_id(&call.id, &call.name)
                        {
                            call.id = kimi_replacement(&mut replacements, &call.id);
                        }
                    }
                }
                _ => {}
            }
            if sender.send(event).is_err() {
                return;
            }
        }
    });
    stream
}

/// A response-local K3 ID names its tool: `shell` or `shell:3`. Current
/// servers send `chatcmpl-tool-…`, which is left alone.
fn is_legacy_kimi_tool_id(id: &str, name: &str) -> bool {
    if id.starts_with("chatcmpl-tool-") {
        return false;
    }
    if name.is_empty() {
        return true;
    }
    id == name
        || id
            .strip_prefix(name)
            .is_some_and(|suffix| suffix.starts_with(':') && suffix.len() > 1)
}

fn kimi_replacement(replacements: &mut HashMap<String, String>, id: &str) -> String {
    replacements
        .entry(id.to_string())
        .or_insert_with(|| format!("chatcmpl-tool-{:032x}", rand::random::<u128>()))
        .clone()
}

/// Sends Pi's requests as OpenSecret inference requests.
struct MapleTransport {
    transport: Arc<dyn MapleInferenceTransport>,
}

#[async_trait]
impl HttpTransport for MapleTransport {
    async fn post(
        &self,
        request: HttpRequest,
        cancel: CancellationToken,
    ) -> Result<HttpResponse, String> {
        if request.url != CHAT_COMPLETIONS_PATH {
            return Err(PREPARE_ERROR_MESSAGE.to_string());
        }
        let inference = inference_request(request.body);
        // The send runs in its own task. The transport owns credential
        // reconciliation and must get to finish it even when the run is
        // stopped, or a rotated token would be stranded in the SDK's memory:
        // on a stop or a timeout the task cancels the transport and still
        // waits for it to settle, also after this future is dropped.
        let transport = Arc::clone(&self.transport);
        let send = tokio::spawn(async move {
            let transport_cancel = cancel.child_token();
            let response = transport.send_inference_request(inference, transport_cancel.clone());
            tokio::pin!(response);
            let start_timeout = tokio::time::sleep(RESPONSE_START_TIMEOUT);
            tokio::pin!(start_timeout);
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    transport_cancel.cancel();
                    let _ = response.await;
                    Err(CANCELLED_MESSAGE.to_string())
                }
                _ = &mut start_timeout => {
                    transport_cancel.cancel();
                    let _ = response.await;
                    Err(REQUEST_TIMEOUT_MESSAGE.to_string())
                }
                response = &mut response => match response {
                    Ok(response) => into_http_response(response, cancel).await,
                    Err(error) => Err(opensecret_error_message(error)),
                },
            }
        });
        send.await
            .unwrap_or_else(|_| Err(NETWORK_ERROR_MESSAGE.to_string()))
    }
}

fn inference_request(body: Vec<u8>) -> InferenceRequest {
    let mut request = InferenceRequest::new(body.into());
    *request.method_mut() = http::Method::POST;
    *request.uri_mut() = http::Uri::from_static(CHAT_COMPLETIONS_PATH);
    request.headers_mut().insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("text/event-stream"),
    );
    request.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    request
}

/// A successful response streams on; a failed one becomes its fixed Maple
/// message, which Pi reports after the status.
async fn into_http_response(
    response: InferenceResponse,
    cancel: CancellationToken,
) -> Result<HttpResponse, String> {
    let status = response.status();
    if status.is_success() {
        return Ok(HttpResponse {
            status: status.as_u16(),
            body: guarded_body(response.into_body(), cancel),
        });
    }
    if has_exact_session_not_found_contract(status, response.headers()) {
        return Err(SECURE_CONNECTION_ERROR_MESSAGE.to_string());
    }
    let body = collect_bounded_body(response.into_body(), &cancel).await?;
    let message = http_error_message(status, error_message(&body).as_deref());
    Ok(HttpResponse {
        status: status.as_u16(),
        body: futures_util::stream::once(async move {
            Ok(Bytes::from(
                json!({ "error": { "message": message } }).to_string(),
            ))
        })
        .boxed(),
    })
}

/// The decrypted body with an idle timeout per chunk and fixed error texts.
fn guarded_body(
    body: OpenSecretResponseBody,
    cancel: CancellationToken,
) -> BoxStream<'static, Result<Bytes, String>> {
    futures_util::stream::unfold(
        (body, cancel, false),
        |(mut body, cancel, finished)| async move {
            if finished {
                return None;
            }
            let item = tokio::select! {
                biased;
                _ = cancel.cancelled() => Err(CANCELLED_MESSAGE.to_string()),
                next = tokio::time::timeout(STREAM_IDLE_TIMEOUT, body.next()) => match next {
                    Ok(Some(Ok(chunk))) => Ok(chunk),
                    Ok(Some(Err(error))) => Err(stream_error_message(error)),
                    Ok(None) => return None,
                    Err(_) => Err(STREAM_TIMEOUT_MESSAGE.to_string()),
                },
            };
            let finished = item.is_err();
            Some((item, (body, cancel, finished)))
        },
    )
    .boxed()
}

fn has_exact_session_not_found_contract(
    status: http::StatusCode,
    headers: &http::HeaderMap,
) -> bool {
    if status != http::StatusCode::BAD_REQUEST {
        return false;
    }
    let mut contract_values = headers.get_all(ERROR_CONTRACT_HEADER).iter();
    let Some(contract_version) = contract_values.next() else {
        return false;
    };
    if contract_values.next().is_some() || contract_version.as_bytes() != ERROR_CONTRACT_VERSION {
        return false;
    }
    let mut code_values = headers.get_all(ERROR_CODE_HEADER).iter();
    let Some(code) = code_values.next() else {
        return false;
    };
    code_values.next().is_none() && code.as_bytes() == SESSION_NOT_FOUND_ERROR_CODE
}

async fn collect_bounded_body(
    mut body: OpenSecretResponseBody,
    cancel: &CancellationToken,
) -> Result<Vec<u8>, String> {
    let mut collected = Vec::new();
    loop {
        let next = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(CANCELLED_MESSAGE.to_string()),
            next = tokio::time::timeout(STREAM_IDLE_TIMEOUT, body.next()) => {
                next.map_err(|_| STREAM_TIMEOUT_MESSAGE.to_string())?
            }
        };
        let Some(chunk) = next else {
            break;
        };
        let chunk = chunk.map_err(|error| {
            log::warn!(
                "Failed to read an encrypted Maple error response ({})",
                opensecret_error_category(&error)
            );
            STREAM_ERROR_MESSAGE.to_string()
        })?;
        let remaining = MAX_ERROR_BODY_BYTES.saturating_sub(collected.len());
        if chunk.len() >= remaining {
            collected.extend_from_slice(&chunk[..remaining]);
            break;
        }
        collected.extend_from_slice(&chunk);
    }
    Ok(collected)
}

/// The provider's own message from an error body, used only to classify it.
fn error_message(body: &[u8]) -> Option<String> {
    let payload: Value = serde_json::from_slice(body).ok()?;
    payload
        .pointer("/error/message")
        .or_else(|| payload.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn http_error_message(status: http::StatusCode, provider_message: Option<&str>) -> String {
    log::warn!(
        "Maple inference request failed (http_status_{})",
        status.as_u16()
    );
    match status {
        http::StatusCode::UNAUTHORIZED => AUTHENTICATION_ERROR_MESSAGE.to_string(),
        http::StatusCode::PAYMENT_REQUIRED => CREDITS_EXHAUSTED_MESSAGE.to_string(),
        http::StatusCode::NOT_FOUND => ENDPOINT_NOT_FOUND_MESSAGE.to_string(),
        http::StatusCode::PAYLOAD_TOO_LARGE => CONTEXT_OVERFLOW_MESSAGE.to_string(),
        http::StatusCode::BAD_REQUEST
            if provider_message.is_some_and(is_context_length_exceeded_message) =>
        {
            CONTEXT_OVERFLOW_MESSAGE.to_string()
        }
        http::StatusCode::TOO_MANY_REQUESTS => RATE_LIMIT_MESSAGE.to_string(),
        _ if status.is_server_error() => {
            format!("Maple server error (status {})", status.as_u16())
        }
        _ => format!(
            "Maple rejected the inference request (status {})",
            status.as_u16()
        ),
    }
}

/// The fixed message for a failed OpenSecret send.
fn opensecret_error_message(error: maple_sdk::Error) -> String {
    log::warn!(
        "OpenSecret inference transport failed ({})",
        opensecret_error_category(&error)
    );
    opensecret_error_kind_message(error)
}

fn opensecret_error_kind_message(error: maple_sdk::Error) -> String {
    match error {
        maple_sdk::Error::Authentication(_) | maple_sdk::Error::Api { status: 401, .. } => {
            AUTHENTICATION_ERROR_MESSAGE.to_string()
        }
        maple_sdk::Error::Api { status, message } => {
            let status =
                http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::BAD_GATEWAY);
            http_error_message(status, Some(&message))
        }
        maple_sdk::Error::Http(error) => {
            if error.is_timeout() {
                REQUEST_TIMEOUT_MESSAGE.to_string()
            } else if error.is_connect() {
                CONNECT_ERROR_MESSAGE.to_string()
            } else {
                NETWORK_ERROR_MESSAGE.to_string()
            }
        }
        maple_sdk::Error::AttestationVerificationFailed(_) if error.is_device_clock_problem() => {
            DEVICE_CLOCK_ERROR_MESSAGE.to_string()
        }
        maple_sdk::Error::AttestationVerificationFailed(_) => {
            ATTESTATION_VERIFICATION_ERROR_MESSAGE.to_string()
        }
        maple_sdk::Error::Session(_)
        | maple_sdk::Error::KeyExchange(_)
        | maple_sdk::Error::Encryption(_)
        | maple_sdk::Error::Decryption(_)
        | maple_sdk::Error::InvalidResponse(_)
        | maple_sdk::Error::Crypto(_)
        | maple_sdk::Error::Cbor(_)
        | maple_sdk::Error::Io(_)
        | maple_sdk::Error::Utf8(_)
        | maple_sdk::Error::Base64Decode(_) => SECURE_CONNECTION_ERROR_MESSAGE.to_string(),
        maple_sdk::Error::Serialization(_)
        | maple_sdk::Error::Configuration(_)
        | maple_sdk::Error::Other(_) => PREPARE_ERROR_MESSAGE.to_string(),
    }
}

/// A failure while the encrypted response streams. A broken secure session
/// cannot be retried; anything else reads as a transient network failure.
fn stream_error_message(error: maple_sdk::Error) -> String {
    log::warn!(
        "Failed to read the encrypted Maple response stream ({})",
        opensecret_error_category(&error)
    );
    match opensecret_error_kind_message(error) {
        message if message == SECURE_CONNECTION_ERROR_MESSAGE => message,
        message if message == AUTHENTICATION_ERROR_MESSAGE => message,
        _ => STREAM_ERROR_MESSAGE.to_string(),
    }
}

/// Whether a provider's 400 text says the input was too long.
pub(crate) fn is_context_length_exceeded_message(text: &str) -> bool {
    let text_lower = text.to_lowercase();

    let direct_context_phrases = [
        "context length",
        "context_length_exceeded",
        "context window",
        "context_window_exceeded",
        "context limit",
        "maximum context",
        "max context",
        "maximum prompt length",
        "max prompt length",
    ];
    if direct_context_phrases
        .iter()
        .any(|phrase| text_lower.contains(phrase))
    {
        return true;
    }

    if text_lower.contains("reduce the length")
        && ["message", "messages", "input", "prompt"]
            .iter()
            .any(|word| text_lower.contains(word))
    {
        return true;
    }

    if [
        "input is too long",
        "input too long",
        "prompt is too long",
        "prompt too long",
    ]
    .iter()
    .any(|phrase| text_lower.contains(phrase))
    {
        return true;
    }

    let mentions_prompt_input_tokens = [
        "input token",
        "input length",
        "prompt token",
        "prompt length",
        "message token",
        "messages token",
        "request token",
        "total token",
    ]
    .iter()
    .any(|phrase| text_lower.contains(phrase));
    let mentions_limit = [
        "model limit",
        "model's limit",
        "maximum allowed",
        "max allowed",
        "maximum number of tokens",
        "token limit",
        "tokens limit",
    ]
    .iter()
    .any(|phrase| text_lower.contains(phrase));
    let mentions_overflow = ["exceed", "too long", "too large", "over the limit"]
        .iter()
        .any(|phrase| text_lower.contains(phrase));

    let words = text_lower.split(|character: char| !character.is_ascii_alphanumeric());
    let mentions_request = words.clone().any(|word| word == "request");
    let mentions_bytes = words.clone().any(|word| matches!(word, "byte" | "bytes"));
    let mentions_content_length = ["content length", "content-length"]
        .iter()
        .any(|phrase| text_lower.contains(phrase));
    let mentions_request_data_size = [
        "request size",
        "requestsize",
        "request body size",
        "request payload size",
        "payload size",
        "body size",
    ]
    .iter()
    .any(|phrase| text_lower.contains(phrase));
    let request_data_too_large = [
        "request body is too large",
        "request body too large",
        "request payload is too large",
        "request payload too large",
        "payload is too large",
        "payload too large",
    ]
    .iter()
    .any(|phrase| text_lower.contains(phrase));
    let mentions_byte_limit = mentions_request_data_size
        || request_data_too_large
        || (mentions_content_length && (mentions_request || mentions_bytes));
    if mentions_byte_limit && mentions_overflow {
        return true;
    }

    mentions_prompt_input_tokens && mentions_limit && mentions_overflow
}

/// A stable category for an SDK failure, safe to log.
pub(crate) fn opensecret_error_category(error: &maple_sdk::Error) -> &'static str {
    match error {
        maple_sdk::Error::Http(_) => "http",
        maple_sdk::Error::Serialization(_) => "serialization",
        maple_sdk::Error::Cbor(_) => "cbor",
        maple_sdk::Error::Crypto(_) => "crypto",
        maple_sdk::Error::AttestationVerificationFailed(_) => "attestation",
        maple_sdk::Error::Session(_) => "session",
        maple_sdk::Error::KeyExchange(_) => "key_exchange",
        maple_sdk::Error::Encryption(_) => "encryption",
        maple_sdk::Error::Decryption(_) => "decryption",
        maple_sdk::Error::Authentication(_) => "authentication",
        maple_sdk::Error::InvalidResponse(_) => "invalid_response",
        maple_sdk::Error::Api { .. } => "api",
        maple_sdk::Error::Configuration(_) => "configuration",
        maple_sdk::Error::Io(_) => "io",
        maple_sdk::Error::Utf8(_) => "utf8",
        maple_sdk::Error::Base64Decode(_) => "base64",
        maple_sdk::Error::Other(_) => "other",
    }
}

#[cfg(test)]
mod tests;
