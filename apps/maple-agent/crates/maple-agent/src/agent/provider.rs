use async_trait::async_trait;
use futures_util::{StreamExt, TryStreamExt};
use goose_providers::base::{MessageStream, Provider, collect_stream};
use goose_providers::conversation::message::{Message, MessageContent};
use goose_providers::conversation::token_usage::ProviderUsage;
use goose_providers::errors::ProviderError;
use goose_providers::formats::openai::{
    OpenAiFormatOptions, create_request_with_options, response_to_streaming_message,
};
use goose_providers::images::ImageFormat;
use goose_providers::model::ModelConfig;
use goose_providers::request_log::{LoggerHandleExt, start_log};
use goose_providers::retry::{
    DEFAULT_BACKOFF_MULTIPLIER, DEFAULT_INITIAL_RETRY_INTERVAL_MS, DEFAULT_MAX_RETRY_INTERVAL_MS,
    RetryConfig, should_retry,
};
use maple_sdk::{InferenceRequest, InferenceResponse, OpenSecretClient, OpenSecretResponseBody};
use rmcp::model::Tool;
use serde_json::{Value, json};
use std::cell::Cell;
use std::future::{Future, ready};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio_util::codec::{FramedRead, LinesCodec, LinesCodecError};
use tokio_util::io::StreamReader;
use tokio_util::sync::CancellationToken;

const CHAT_COMPLETIONS_PATH: &str = "/v1/chat/completions";
pub(super) const MAPLE_PROVIDER_NAME: &str = "maple";
const AUTHENTICATION_ERROR_MESSAGE: &str = "Maple authentication failed";
pub(super) const ATTESTATION_VERIFICATION_ERROR_MESSAGE: &str =
    "Maple could not verify the secure server connection";
pub(super) const DEVICE_CLOCK_ERROR_MESSAGE: &str = "Maple could not verify the secure server connection because this device's date or time looks wrong. Check the device's date, time and time zone settings, then try again";
pub(super) const SECURE_CONNECTION_ERROR_MESSAGE: &str =
    "Maple's encrypted connection could not be recovered";
const ERROR_CONTRACT_HEADER: &str = "x-opensecret-error-contract";
const ERROR_CODE_HEADER: &str = "x-opensecret-error-code";
const ERROR_CONTRACT_VERSION: &[u8] = b"1";
const SESSION_NOT_FOUND_ERROR_CODE: &[u8] = b"session_not_found";

/// Extra transient inference attempts after the first failure. Goose's 1s × 2^n
/// backoff, capped at 30s, then covers roughly three minutes of provider blips.
const TRANSIENT_MAX_RETRIES: usize = 10;
const KIMI_K3_MODEL_ID: &str = "kimi-k3";
// Agent Mode forwards the selected catalog ID unchanged for direct model
// selections. Keep Gemma's provider-specific opt-in scoped to that explicit
// selection; aliases and other reasoning models retain their existing behavior.
const GEMMA4_AGENT_MODEL_ID: &str = "gemma4-31b";
const MAX_ERROR_BODY_BYTES: usize = 16 * 1024;
const MAX_STREAM_LINE_BYTES: usize = 16 * 1024 * 1024;
const MAX_RETRY_AFTER_SECS: f64 = 3_600.0;
#[cfg(not(test))]
const RESPONSE_START_TIMEOUT: Duration = Duration::from_secs(300);
#[cfg(test)]
const RESPONSE_START_TIMEOUT: Duration = Duration::from_millis(100);
#[cfg(not(test))]
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
#[cfg(test)]
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_millis(100);

struct MapleRunContext {
    cancellation: CancellationToken,
    // Pinned Goose projects terminal provider errors as assistant messages.
    // Preserve only the SDK-owned, repair-exhausted categories that Maple must
    // return as failed runs instead.
    terminal_error: Cell<Option<&'static str>>,
}

tokio::task_local! {
    static MAPLE_RUN_CONTEXT: MapleRunContext;
}

pub(crate) async fn with_run_cancellation<F>(
    cancellation: CancellationToken,
    future: F,
) -> F::Output
where
    F: Future,
{
    MAPLE_RUN_CONTEXT
        .scope(
            MapleRunContext {
                cancellation,
                terminal_error: Cell::new(None),
            },
            future,
        )
        .await
}

fn current_run_cancellation() -> CancellationToken {
    MAPLE_RUN_CONTEXT
        .try_with(|context| context.cancellation.clone())
        .unwrap_or_default()
}

fn remember_terminal_run_error(error: &ProviderError) {
    let message = match error {
        ProviderError::Authentication(_) => AUTHENTICATION_ERROR_MESSAGE,
        ProviderError::ExecutionError(message)
            if message == ATTESTATION_VERIFICATION_ERROR_MESSAGE =>
        {
            ATTESTATION_VERIFICATION_ERROR_MESSAGE
        }
        ProviderError::ExecutionError(message) if message == SECURE_CONNECTION_ERROR_MESSAGE => {
            SECURE_CONNECTION_ERROR_MESSAGE
        }
        _ => return,
    };

    let _ = MAPLE_RUN_CONTEXT.try_with(|context| {
        context.terminal_error.set(Some(message));
    });
}

pub(super) fn take_terminal_run_error() -> Option<String> {
    MAPLE_RUN_CONTEXT
        .try_with(|context| context.terminal_error.take().map(str::to_string))
        .ok()
        .flatten()
}

fn cancellation_error() -> ProviderError {
    ProviderError::ExecutionError("Maple request cancelled".to_string())
}

/// Authenticated, encrypted delivery for a caller-owned OpenSecret inference request.
///
/// The provider intentionally knows nothing about token storage or refresh. The
/// application auth session can implement this trait and select its current SDK
/// client at the start of every call (including Goose retries).
#[async_trait]
pub(crate) trait MapleInferenceTransport: Send + Sync {
    async fn send_inference_request(
        self: Arc<Self>,
        request: InferenceRequest,
        cancel_token: CancellationToken,
    ) -> maple_sdk::Result<InferenceResponse>;
}

/// A direct SDK client is also a valid transport. Maple's account-scoped auth
/// session can wrap this implementation when it needs to atomically replace the
/// active client after browser credentials change.
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

pub(crate) struct MapleProvider {
    transport: Arc<dyn MapleInferenceTransport>,
    context_limit: Option<(String, usize)>,
    #[cfg(test)]
    test_retry_config: Option<RetryConfig>,
}

/// How many tool-produced images the newest turns may still carry as pixels.
///
/// A desktop task calls an observation tool on nearly every step, and each
/// result holds a full screenshot. Goose keeps tool results verbatim because
/// Maple disables its history rewrite, so without a bound every screenshot
/// would be re-uploaded on every later turn.
const MAX_RETAINED_TOOL_RESULT_IMAGES: usize = 3;

/// Replace all but the newest tool-result images with a short text marker.
///
/// Only tool output is bounded. An image the user attached stays a real image
/// for as long as the conversation does. The stored transcript is untouched,
/// so the task history and the user-visible timeline keep every screenshot;
/// this bounds only what one request carries.
fn bound_tool_result_images(messages: &[Message]) -> Vec<Message> {
    let mut retained = 0usize;
    let mut bounded = messages.to_vec();
    for message in bounded.iter_mut().rev() {
        for content in message.content.iter_mut().rev() {
            let MessageContent::ToolResponse(response) = content else {
                continue;
            };
            let Ok(result) = response.tool_result.as_mut() else {
                continue;
            };
            for block in result.content.iter_mut().rev() {
                if !matches!(block, rmcp::model::ContentBlock::Image(_)) {
                    continue;
                }
                if retained < MAX_RETAINED_TOOL_RESULT_IMAGES {
                    retained += 1;
                    continue;
                }
                *block = superseded_image_marker();
            }
        }
    }
    bounded
}

fn superseded_image_marker() -> rmcp::model::ContentBlock {
    rmcp::model::ContentBlock::Text(
        rmcp::model::TextContent::new(
            "[Earlier screenshot omitted from this request. Take a new observation if the current screen matters.]",
        )
        .with_annotations(rmcp::model::Annotations::default().with_priority(0.0)),
    )
}

impl MapleProvider {
    pub(crate) fn new<T>(transport: Arc<T>) -> Self
    where
        T: MapleInferenceTransport + 'static,
    {
        Self {
            transport,
            context_limit: None,
            #[cfg(test)]
            test_retry_config: None,
        }
    }

    pub(crate) fn with_context_limit(mut self, model: &str, limit: Option<usize>) -> Self {
        self.context_limit = limit
            .filter(|limit| *limit > 0)
            .map(|limit| (model.to_string(), limit));
        self
    }

    #[cfg(test)]
    fn with_test_retry_config(mut self, retry_config: RetryConfig) -> Self {
        self.test_retry_config = Some(retry_config);
        self
    }

    fn build_request(
        &self,
        model_config: &ModelConfig,
        system: &str,
        messages: &[Message],
        tools: &[Tool],
    ) -> Result<Value, ProviderError> {
        let messages = bound_tool_result_images(messages);
        create_request_with_options(
            model_config,
            system,
            &messages,
            tools,
            &ImageFormat::OpenAi,
            true,
            OpenAiFormatOptions {
                preserve_thinking_context: true,
                supports_vision: model_config.supports_vision.unwrap_or_default(),
                thinking_preservation_format: None,
            },
        )
        .map_err(|error| {
            ProviderError::RequestFailed(format!("Failed to create Maple request: {error}"))
        })
    }

    fn gemma_agent_model_config(
        model_config: &ModelConfig,
        primary_agent_turn: bool,
    ) -> ModelConfig {
        let mut model_config = model_config.clone();
        if model_config.model_name != GEMMA4_AGENT_MODEL_ID {
            return model_config;
        }

        let request_params = model_config.request_params.get_or_insert_default();
        request_params
            .entry("include_reasoning".to_string())
            .or_insert_with(|| json!(primary_agent_turn));
        let template_kwargs = request_params
            .entry("chat_template_kwargs".to_string())
            .or_insert_with(|| json!({}));
        if let Some(template_kwargs) = template_kwargs.as_object_mut() {
            template_kwargs
                .entry("enable_thinking".to_string())
                .or_insert_with(|| json!(primary_agent_turn));
        }

        model_config
    }

    fn replace_legacy_kimi_k3_tool_ids(
        message: &mut Message,
        replacements: &mut std::collections::HashMap<String, String>,
    ) {
        // Temporary compatibility for Kimi K3 deployments built before vLLM
        // ab98034d4, where tool IDs are scoped to one assistant message and can
        // repeat across turns. Goose treats them as conversation identifiers,
        // so mirror current vLLM with a random ID per distinct call in this
        // inference while preserving same-response duplicate detection.
        // Remove this after Tinfoil's attested K3 image includes vLLM #50420.
        for content in &mut message.content {
            if let MessageContent::ToolRequest(request) = content {
                let legacy_response_local_id = match request.tool_call.as_ref() {
                    Ok(tool_call) => {
                        let tool_name = tool_call.name.as_ref();
                        request.id == tool_name
                            || request
                                .id
                                .strip_prefix(tool_name)
                                .is_some_and(|suffix| suffix.starts_with(':') && suffix.len() > 1)
                    }
                    Err(_) => !request.id.starts_with("chatcmpl-tool-"),
                };
                if legacy_response_local_id {
                    request.id = replacements
                        .entry(request.id.clone())
                        .or_insert_with(|| format!("chatcmpl-tool-{:032x}", rand::random::<u128>()))
                        .clone();
                }
            }
        }
    }

    fn inference_request(payload: Vec<u8>) -> Result<InferenceRequest, ProviderError> {
        let mut request = InferenceRequest::new(payload.into());
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
        Ok(request)
    }

    async fn send_attempt(
        &self,
        request: InferenceRequest,
        cancellation: &CancellationToken,
    ) -> Result<InferenceResponse, ProviderError> {
        // The transport owns authentication reconciliation and must get a chance
        // to finish it even when the parent run is cancelled or response headers
        // take too long. Cancelling and then awaiting the transport future keeps
        // a rotated SDK JWT from being stranded only in native memory.
        let transport_cancellation = cancellation.child_token();
        let response = Arc::clone(&self.transport)
            .send_inference_request(request, transport_cancellation.clone());
        tokio::pin!(response);
        let response_start_timeout = tokio::time::sleep(RESPONSE_START_TIMEOUT);
        tokio::pin!(response_start_timeout);

        tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                transport_cancellation.cancel();
                let _ = response.await;
                Err(cancellation_error())
            }
            _ = &mut response_start_timeout => {
                transport_cancellation.cancel();
                let _ = response.await;
                Err(ProviderError::NetworkError(
                    "The Maple request timed out".to_string()
                ))
            }
            response = &mut response => {
                response.map_err(map_opensecret_error)
            }
        }
    }

    fn message_stream_from_response(
        response: InferenceResponse,
        cancellation: CancellationToken,
    ) -> MessageStream {
        let parser_cancellation = cancellation.clone();
        let response_stream = futures_util::stream::unfold(
            (response.into_body(), cancellation, false),
            |(mut body, cancellation, finished)| async move {
                if finished {
                    return None;
                }
                let item = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "Maple request cancelled",
                    )),
                    next = tokio::time::timeout(STREAM_IDLE_TIMEOUT, body.next()) => {
                        match next {
                            Ok(Some(Ok(chunk))) => Ok(chunk),
                            Ok(Some(Err(error))) => Err(map_response_stream_error(error)),
                            Ok(None) => return None,
                            Err(_) => Err(std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                "Maple response stream timed out",
                            )),
                        }
                    }
                };
                let finished = item.is_err();
                Some((item, (body, cancellation, finished)))
            },
        );
        let reader = StreamReader::new(Box::pin(response_stream));
        let lines = FramedRead::new(
            reader,
            LinesCodec::new_with_max_length(MAX_STREAM_LINE_BYTES),
        )
        .map_err(anyhow::Error::from);
        let parsed = response_to_streaming_message(lines).filter_map(|result| async move {
            // Goose 1.51 adds a synthetic usage-only item at EOF when it saw a
            // response ID but no completed output. Do not turn an abandoned
            // tool call into the first successful stream item: Goose's empty
            // stream recovery still needs to see the empty response.
            if matches!(&result, Ok((None, Some(usage)))
                if usage.response_id.is_some()
                    && usage.finish_reasons.is_none()
                    && usage.usage == Default::default())
            {
                None
            } else {
                Some(result)
            }
        });

        Box::pin(parsed.map(move |result| {
            result.map_err(|error| {
                if parser_cancellation.is_cancelled() {
                    cancellation_error()
                } else if let Some(error) = secure_connection_stream_error(&error) {
                    remember_terminal_run_error(&error);
                    error
                } else {
                    invalid_stream_error()
                }
            })
        }))
    }

    async fn stream_attempt(
        &self,
        payload_bytes: &[u8],
        cancellation: &CancellationToken,
    ) -> Result<MessageStream, ProviderError> {
        let request = Self::inference_request(payload_bytes.to_vec())?;
        let response = self.send_attempt(request, cancellation).await?;
        let response = ensure_success(response).await?;
        Ok(Self::message_stream_from_response(
            response,
            cancellation.clone(),
        ))
    }

    async fn stream_with_retry(
        &self,
        payload_bytes: &[u8],
        cancellation: &CancellationToken,
    ) -> Result<MessageStream, ProviderError> {
        let config = Provider::retry_config(self);
        let mut attempts = 0;

        loop {
            let error = match self.stream_attempt(payload_bytes, cancellation).await {
                Ok(mut stream) => {
                    // TODO(upstream): Remove this Maple-specific bridge from Agent Mode once
                    // Maple's pinned Goose revision provides equivalent first-item handling:
                    // https://github.com/aaif-goose/goose/issues/10887
                    // If auxiliary complete() calls still need this protection, scope it to
                    // that path instead. Recovery after any successful item remains out of
                    // scope here:
                    // https://github.com/aaif-goose/goose/issues/10897
                    let first = tokio::select! {
                        biased;
                        _ = cancellation.cancelled() => return Err(cancellation_error()),
                        first = stream.next() => first,
                    };
                    match first {
                        Some(Ok(first)) => {
                            return Ok(Box::pin(
                                futures_util::stream::once(ready(Ok(first))).chain(stream),
                            ));
                        }
                        Some(Err(error)) => error,
                        None => return Ok(stream),
                    }
                }
                Err(error) => error,
            };

            if !should_retry(&error, &config) || attempts >= config.max_retries() {
                remember_terminal_run_error(&error);
                return Err(error);
            }
            attempts += 1;
            let delay = match &error {
                ProviderError::RateLimitExceeded {
                    retry_delay: Some(provider_delay),
                    ..
                } => *provider_delay,
                _ => config.delay_for_attempt(attempts),
            };
            let skip_backoff = std::env::var("GOOSE_PROVIDER_SKIP_BACKOFF")
                .unwrap_or_default()
                .parse::<bool>()
                .unwrap_or(false);
            if !skip_backoff {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return Err(cancellation_error()),
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }

    async fn stream_request(
        &self,
        model_config: &ModelConfig,
        system: &str,
        messages: &[Message],
        tools: &[Tool],
        enable_primary_agent_thinking: bool,
    ) -> Result<MessageStream, ProviderError> {
        let effective_model_config =
            Self::gemma_agent_model_config(model_config, enable_primary_agent_thinking);
        let payload = self.build_request(&effective_model_config, system, messages, tools)?;
        let payload_bytes = serde_json::to_vec(&payload).map_err(|error| {
            ProviderError::RequestFailed(format!("Failed to serialize Maple request: {error}"))
        })?;
        let mut request_log = start_log(&effective_model_config, &payload)?;
        let cancellation = current_run_cancellation();

        let stream = self
            .stream_with_retry(&payload_bytes, &cancellation)
            .await
            .inspect_err(|error| {
                let _ = request_log.error(error);
            })?;
        let replace_legacy_kimi_k3_tool_ids = effective_model_config.model_name == KIMI_K3_MODEL_ID;
        let mut kimi_k3_tool_id_replacements = std::collections::HashMap::new();

        let stream = stream.map(move |result| {
            let (mut message, usage) = result?;
            if replace_legacy_kimi_k3_tool_ids && let Some(message) = message.as_mut() {
                Self::replace_legacy_kimi_k3_tool_ids(message, &mut kimi_k3_tool_id_replacements);
            }
            request_log.write(&message, usage.as_ref().map(|value| &value.usage))?;
            Ok((message, usage))
        });

        Ok(Box::pin(stream))
    }
}

#[async_trait]
impl Provider for MapleProvider {
    fn get_name(&self) -> &str {
        MAPLE_PROVIDER_NAME
    }

    async fn get_context_limit(&self, model: &str, override_limit: Option<usize>) -> usize {
        goose_providers::context_limit::ContextLimitResolver::new(MAPLE_PROVIDER_NAME)
            .with_configured_limits(self.context_limit.clone())
            .resolve_local(model, override_limit)
    }

    fn retry_config(&self) -> RetryConfig {
        #[cfg(test)]
        if let Some(config) = &self.test_retry_config {
            return config.clone();
        }

        // Retrying deterministic client failures can repeat side effects and
        // causes the SDK to repeat its own stale-session recovery for a 400. One
        // shared transient budget covers both setup and pre-first-item failures.
        RetryConfig::new(
            TRANSIENT_MAX_RETRIES,
            DEFAULT_INITIAL_RETRY_INTERVAL_MS,
            DEFAULT_BACKOFF_MULTIPLIER,
            DEFAULT_MAX_RETRY_INTERVAL_MS,
        )
        .transient_only()
    }

    async fn stream(
        &self,
        model_config: &ModelConfig,
        system: &str,
        messages: &[Message],
        tools: &[Tool],
    ) -> Result<MessageStream, ProviderError> {
        // Goose uses stream for the interactive Agent loop. Selecting Gemma
        // directly in Agent Mode is the product-level opt-in to thinking.
        self.stream_request(model_config, system, messages, tools, true)
            .await
    }

    async fn complete(
        &self,
        model_config: &ModelConfig,
        system: &str,
        messages: &[Message],
        tools: &[Tool],
    ) -> Result<(Message, ProviderUsage), ProviderError> {
        // Goose and Maple use complete for auxiliary work such as compaction,
        // classifiers, and image descriptions; keep those requests non-thinking.
        let stream = self
            .stream_request(model_config, system, messages, tools, false)
            .await?;
        collect_stream(stream).await
    }
}

fn invalid_stream_error() -> ProviderError {
    // Goose's parser error may contain the decrypted SSE line. Keep both the
    // application log and the error returned to the UI on a fixed category.
    log::warn!("Failed to parse Maple inference response stream (openai_stream_parser)");
    ProviderError::NetworkError("Maple's response stream was invalid".to_string())
}

async fn ensure_success(response: InferenceResponse) -> Result<InferenceResponse, ProviderError> {
    if response.status().is_success() {
        return Ok(response);
    }

    let status = response.status();
    let terminal_session_failure = has_exact_session_not_found_contract(status, response.headers());
    let retry_after_header = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let (_parts, body) = response.into_parts();
    let (body, truncated) = collect_bounded_body(body).await?;
    let payload = error_payload(&body, truncated);
    let retry_delay = retry_after_delay(payload.as_ref(), retry_after_header.as_deref());
    let error = if terminal_session_failure {
        ProviderError::ExecutionError(SECURE_CONNECTION_ERROR_MESSAGE.to_string())
    } else {
        map_http_error(status, payload.as_ref())
    };

    match error {
        ProviderError::RateLimitExceeded { details, .. } => Err(ProviderError::RateLimitExceeded {
            details,
            retry_delay,
        }),
        error => Err(error),
    }
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
) -> Result<(Vec<u8>, bool), ProviderError> {
    let mut collected = Vec::new();
    let mut truncated = false;
    let cancellation = current_run_cancellation();

    loop {
        let next = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(cancellation_error()),
            next = tokio::time::timeout(STREAM_IDLE_TIMEOUT, body.next()) => {
                next.map_err(|_| ProviderError::NetworkError(
                    "Maple's error response stream timed out".to_string()
                ))?
            }
        };
        let Some(chunk) = next else {
            break;
        };
        let chunk = chunk.map_err(|error| {
            log::warn!(
                "Failed to read encrypted Maple error response ({})",
                opensecret_error_category(&error)
            );
            ProviderError::NetworkError("Maple's encrypted response stream failed".to_string())
        })?;
        if chunk.is_empty() {
            continue;
        }

        let remaining = MAX_ERROR_BODY_BYTES.saturating_sub(collected.len());
        if chunk.len() > remaining {
            collected.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        collected.extend_from_slice(&chunk);
    }

    Ok((collected, truncated))
}

fn error_payload(body: &[u8], truncated: bool) -> Option<Value> {
    if body.is_empty() {
        return None;
    }
    if !truncated && let Ok(payload) = serde_json::from_slice(body) {
        return Some(payload);
    }

    let mut message = String::from_utf8_lossy(body).into_owned();
    if truncated {
        message.push_str(" [response truncated]");
    }
    Some(json!({ "message": message }))
}

fn retry_after_delay(payload: Option<&Value>, header: Option<&str>) -> Option<Duration> {
    let body_seconds = payload
        .and_then(|payload| payload.get("error"))
        .and_then(|error| error.get("metadata"))
        .and_then(|metadata| metadata.get("retry_after_seconds"))
        .and_then(Value::as_f64);
    body_seconds
        .and_then(retry_duration_from_seconds)
        .or_else(|| header.and_then(parse_retry_after_header))
}

fn retry_duration_from_seconds(seconds: f64) -> Option<Duration> {
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }

    Some(Duration::from_secs_f64(seconds.min(MAX_RETRY_AFTER_SECS)))
}

fn parse_retry_after_header(value: &str) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return retry_duration_from_seconds(seconds as f64);
    }

    let retry_at = httpdate::parse_http_date(value).ok()?;
    let delay = retry_at
        .duration_since(SystemTime::now())
        .unwrap_or(Duration::ZERO);
    retry_duration_from_seconds(delay.as_secs_f64())
}

fn map_http_error(status: http::StatusCode, payload: Option<&Value>) -> ProviderError {
    log::warn!(
        "Maple inference request failed (http_status_{})",
        status.as_u16()
    );

    match status {
        http::StatusCode::UNAUTHORIZED => {
            ProviderError::Authentication(AUTHENTICATION_ERROR_MESSAGE.to_string())
        }
        http::StatusCode::NOT_FOUND => ProviderError::EndpointNotFound(
            "The Maple inference endpoint was not found".to_string(),
        ),
        http::StatusCode::PAYMENT_REQUIRED => ProviderError::CreditsExhausted {
            details: "Maple credits are exhausted".to_string(),
            top_up_url: None,
        },
        http::StatusCode::PAYLOAD_TOO_LARGE => ProviderError::ContextLengthExceeded(
            "The Maple request exceeds the model's context window".to_string(),
        ),
        http::StatusCode::BAD_REQUEST
            if error_message(payload).is_some_and(is_context_length_exceeded_message) =>
        {
            ProviderError::ContextLengthExceeded(
                "The Maple request exceeds the model's context window".to_string(),
            )
        }
        http::StatusCode::BAD_REQUEST => {
            ProviderError::RequestFailed("Maple rejected the inference request (400)".to_string())
        }
        http::StatusCode::TOO_MANY_REQUESTS => ProviderError::RateLimitExceeded {
            details: "Maple rate limit exceeded".to_string(),
            retry_delay: None,
        },
        _ if status.is_server_error() => ProviderError::ServerError(format!(
            "Maple's server returned status {}",
            status.as_u16()
        )),
        _ => ProviderError::RequestFailed(format!(
            "Maple request failed with status {}",
            status.as_u16()
        )),
    }
}

fn error_message(payload: Option<&Value>) -> Option<&str> {
    payload.and_then(|payload| {
        payload
            .get("error")
            .and_then(|error| error.get("message"))
            .or_else(|| payload.get("message"))
            .and_then(Value::as_str)
    })
}

/// Local copy of goose 1.47's private classifier. Maple still needs this for
/// 400 bodies after goose stopped exporting `is_context_length_exceeded_message`.
fn is_context_length_exceeded_message(text: &str) -> bool {
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

fn map_opensecret_error(error: maple_sdk::Error) -> ProviderError {
    log::warn!(
        "OpenSecret inference transport failed ({})",
        opensecret_error_category(&error)
    );
    map_opensecret_error_kind(error)
}

fn map_opensecret_error_kind(error: maple_sdk::Error) -> ProviderError {
    match error {
        maple_sdk::Error::Authentication(_) | maple_sdk::Error::Api { status: 401, .. } => {
            ProviderError::Authentication(AUTHENTICATION_ERROR_MESSAGE.to_string())
        }
        maple_sdk::Error::Api {
            status: 402,
            message: _,
        } => ProviderError::CreditsExhausted {
            details: "Maple credits are exhausted".to_string(),
            top_up_url: None,
        },
        maple_sdk::Error::Api {
            status: 413,
            message: _,
        } => ProviderError::ContextLengthExceeded(
            "The Maple request exceeds the model's context window".to_string(),
        ),
        maple_sdk::Error::Api {
            status: 400,
            message,
        } if is_context_length_exceeded_message(&message) => ProviderError::ContextLengthExceeded(
            "The Maple request exceeds the model's context window".to_string(),
        ),
        maple_sdk::Error::Api {
            status: 429,
            message: _,
        } => ProviderError::RateLimitExceeded {
            details: "Maple rate limit exceeded".to_string(),
            retry_delay: None,
        },
        maple_sdk::Error::Api { status, message: _ } if (500..=599).contains(&status) => {
            ProviderError::ServerError(format!("Maple's server returned status {status}"))
        }
        maple_sdk::Error::Api {
            status: 404,
            message: _,
        } => ProviderError::EndpointNotFound(
            "The Maple inference endpoint was not found".to_string(),
        ),
        maple_sdk::Error::Api { status, message: _ } => {
            ProviderError::RequestFailed(format!("Maple request failed with status {status}"))
        }
        maple_sdk::Error::Http(error) => {
            if error.is_timeout() {
                ProviderError::NetworkError("The Maple request timed out".to_string())
            } else if error.is_connect() {
                ProviderError::NetworkError("Could not connect to Maple".to_string())
            } else {
                ProviderError::NetworkError("The Maple network request failed".to_string())
            }
        }
        maple_sdk::Error::AttestationVerificationFailed(_) if error.is_device_clock_problem() => {
            ProviderError::ExecutionError(DEVICE_CLOCK_ERROR_MESSAGE.to_string())
        }
        maple_sdk::Error::AttestationVerificationFailed(_) => {
            ProviderError::ExecutionError(ATTESTATION_VERIFICATION_ERROR_MESSAGE.to_string())
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
        | maple_sdk::Error::Base64Decode(_) => {
            ProviderError::ExecutionError(SECURE_CONNECTION_ERROR_MESSAGE.to_string())
        }
        maple_sdk::Error::Serialization(_)
        | maple_sdk::Error::Configuration(_)
        | maple_sdk::Error::Other(_) => ProviderError::ExecutionError(
            "Maple could not prepare the encrypted request".to_string(),
        ),
    }
}

fn map_response_stream_error(error: maple_sdk::Error) -> std::io::Error {
    log::warn!(
        "Failed to read encrypted Maple response stream ({})",
        opensecret_error_category(&error)
    );
    std::io::Error::other(map_opensecret_error_kind(error))
}

fn secure_connection_stream_error(error: &anyhow::Error) -> Option<ProviderError> {
    error.chain().find_map(|cause| {
        let provider_error = cause.downcast_ref::<ProviderError>().or_else(|| {
            let LinesCodecError::Io(error) = cause.downcast_ref::<LinesCodecError>()? else {
                return None;
            };
            error.get_ref()?.downcast_ref::<ProviderError>()
        })?;
        let ProviderError::ExecutionError(message) = provider_error else {
            return None;
        };
        (message == SECURE_CONNECTION_ERROR_MESSAGE)
            .then(|| ProviderError::ExecutionError(message.clone()))
    })
}

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
mod tests {
    use super::*;
    use goose_providers::conversation::message::MessageContent;
    use goose_providers::retry::should_retry;
    use rmcp::object;
    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;
    use tokio::sync::Notify;

    #[tokio::test]
    async fn context_limit_applies_only_to_the_selected_maple_model() {
        let provider = MapleProvider::new(Arc::new(FakeTransport::queued(vec![])))
            .with_context_limit("glm-5-2", Some(384_000));
        assert_eq!(provider.get_context_limit("glm-5-2", None).await, 384_000);
        assert_eq!(
            provider.get_context_limit("other-model", None).await,
            goose_providers::model::DEFAULT_CONTEXT_LIMIT
        );
        assert_eq!(
            provider.get_context_limit("glm-5-2", Some(64_000)).await,
            64_000
        );
    }

    #[derive(Debug)]
    struct CapturedRequest {
        method: String,
        uri: String,
        accept: Option<String>,
        raw_body: Vec<u8>,
        body: Value,
    }

    struct FakeTransport {
        requests: Mutex<Vec<CapturedRequest>>,
        responses: Mutex<VecDeque<maple_sdk::Result<InferenceResponse>>>,
        request_notify: Notify,
    }

    struct PendingTransport;

    #[async_trait]
    impl MapleInferenceTransport for PendingTransport {
        async fn send_inference_request(
            self: Arc<Self>,
            _request: InferenceRequest,
            cancel_token: CancellationToken,
        ) -> maple_sdk::Result<InferenceResponse> {
            cancel_token.cancelled().await;
            Err(maple_sdk::Error::Other(
                "Pending transport was cancelled".to_string(),
            ))
        }
    }

    impl FakeTransport {
        fn new(response: InferenceResponse) -> Self {
            Self::queued(vec![response])
        }

        fn with_responses(responses: Vec<InferenceResponse>) -> Self {
            Self::queued(responses)
        }

        fn queued(responses: Vec<InferenceResponse>) -> Self {
            Self::with_results(responses.into_iter().map(Ok).collect())
        }

        fn with_results(responses: Vec<maple_sdk::Result<InferenceResponse>>) -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
                responses: Mutex::new(responses.into()),
                request_notify: Notify::new(),
            }
        }

        fn request_count(&self) -> usize {
            self.requests.lock().expect("request lock").len()
        }

        fn remaining_response_count(&self) -> usize {
            self.responses.lock().expect("response lock").len()
        }

        async fn wait_for_request_count(&self, expected: usize) {
            loop {
                let notified = self.request_notify.notified();
                if self.request_count() >= expected {
                    return;
                }
                notified.await;
            }
        }
    }

    #[async_trait]
    impl MapleInferenceTransport for FakeTransport {
        async fn send_inference_request(
            self: Arc<Self>,
            request: InferenceRequest,
            _cancel_token: CancellationToken,
        ) -> maple_sdk::Result<InferenceResponse> {
            let (parts, body) = request.into_parts();
            let captured = CapturedRequest {
                method: parts.method.to_string(),
                uri: parts.uri.to_string(),
                accept: parts
                    .headers
                    .get("accept")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned),
                raw_body: body.to_vec(),
                body: serde_json::from_slice(&body).expect("request body should be JSON"),
            };
            self.requests.lock().expect("request lock").push(captured);
            self.request_notify.notify_one();
            self.responses
                .lock()
                .expect("response lock")
                .pop_front()
                .expect("a fake response should be queued")
        }
    }

    fn response_with_items(
        status: u16,
        items: Vec<maple_sdk::Result<Vec<u8>>>,
        retry_after: Option<&str>,
    ) -> InferenceResponse {
        let body: OpenSecretResponseBody = Box::pin(futures_util::stream::iter(
            items.into_iter().map(|item| item.map(Into::into)),
        ));
        let mut response = InferenceResponse::new(body);
        *response.status_mut() = http::StatusCode::from_u16(status).expect("valid fake status");
        if let Some(retry_after) = retry_after {
            response.headers_mut().insert(
                http::header::RETRY_AFTER,
                http::HeaderValue::from_str(retry_after).expect("valid retry header"),
            );
        }
        response
    }

    fn response(status: u16, chunks: Vec<Vec<u8>>, retry_after: Option<&str>) -> InferenceResponse {
        response_with_items(status, chunks.into_iter().map(Ok).collect(), retry_after)
    }

    fn error_contract_response(contract: Option<&str>, code: Option<&str>) -> InferenceResponse {
        let mut response = response(
            400,
            vec![br#"{"error":{"message":"private session detail"}}"#.to_vec()],
            None,
        );
        if let Some(contract) = contract {
            response.headers_mut().insert(
                ERROR_CONTRACT_HEADER,
                http::HeaderValue::from_str(contract).expect("valid contract fixture"),
            );
        }
        if let Some(code) = code {
            response.headers_mut().insert(
                ERROR_CODE_HEADER,
                http::HeaderValue::from_str(code).expect("valid error code fixture"),
            );
        }
        response
    }

    fn malformed_response(detail: &str) -> InferenceResponse {
        response(
            200,
            vec![format!("data: {detail}\n\ndata: [DONE]\n\n").into_bytes()],
            None,
        )
    }

    fn incomplete_tool_call_sse() -> Vec<u8> {
        concat!(
            "data: {\"id\":\"tools-incomplete\",\"object\":\"chat.completion.chunk\",",
            "\"created\":1,\"model\":\"test-model\",\"choices\":[{\"index\":0,",
            "\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,",
            "\"id\":\"call-1\",\"type\":\"function\",\"function\":{",
            "\"name\":\"web_search\",\"arguments\":\"{\\\"query\\\":\\\"map\"}}]},",
            "\"finish_reason\":null}]}\n\n"
        )
        .as_bytes()
        .to_vec()
    }

    fn complete_tool_call_event() -> Vec<u8> {
        concat!(
            "data: {\"id\":\"tools-complete\",\"object\":\"chat.completion.chunk\",",
            "\"created\":2,\"model\":\"test-model\",\"choices\":[{\"index\":0,",
            "\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,",
            "\"id\":\"call-1\",\"type\":\"function\",\"function\":{",
            "\"name\":\"web_search\",\"arguments\":\"{\\\"query\\\":\\\"maple\\\"}\"}}]},",
            "\"finish_reason\":\"tool_calls\"}],",
            "\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":3,\"total_tokens\":5}}\n\n"
        )
        .as_bytes()
        .to_vec()
    }

    fn complete_tool_call_sse() -> Vec<u8> {
        let mut event = complete_tool_call_event();
        event.extend_from_slice(b"data: [DONE]\n\n");
        event
    }

    fn fast_retry_config(max_retries: usize) -> RetryConfig {
        RetryConfig::new(max_retries, 0, 1.0, 0).transient_only()
    }

    fn fragmented_success_response() -> InferenceResponse {
        let response_bytes = concat!(
            "data: {\"id\":\"chunk-1\",\"object\":\"chat.completion.chunk\",",
            "\"created\":1,\"model\":\"test-model\",\"choices\":[{\"index\":0,",
            "\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"},",
            "\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chunk-2\",\"object\":\"chat.completion.chunk\",",
            "\"created\":2,\"model\":\"test-model\",\"choices\":[{\"index\":0,",
            "\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chunk-3\",\"object\":\"chat.completion.chunk\",",
            "\"created\":3,\"model\":\"test-model\",\"choices\":[{\"index\":0,",
            "\"delta\":{},\"finish_reason\":\"stop\"}],",
            "\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":3,\"total_tokens\":5}}\n\n",
            "data: [DONE]\n\n"
        )
        .as_bytes();
        response(
            200,
            vec![
                response_bytes[..23].to_vec(),
                response_bytes[23..91].to_vec(),
                response_bytes[91..response_bytes.len() - 7].to_vec(),
                response_bytes[response_bytes.len() - 7..].to_vec(),
            ],
            None,
        )
    }

    fn tool_call_response(completion_id: &str, tool_id: &str) -> InferenceResponse {
        let tool_chunk = json!({
            "id": completion_id,
            "object": "chat.completion.chunk",
            "created": 1,
            "model": KIMI_K3_MODEL_ID,
            "choices": [{
                "index": 0,
                "delta": {
                    "role": "assistant",
                    "tool_calls": [{
                        "index": 0,
                        "id": tool_id,
                        "type": "function",
                        "function": { "name": "shell", "arguments": "{}" }
                    }]
                },
                "finish_reason": null
            }]
        });
        let finish_chunk = json!({
            "id": completion_id,
            "object": "chat.completion.chunk",
            "created": 2,
            "model": KIMI_K3_MODEL_ID,
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        });
        let sse = format!("data: {tool_chunk}\n\ndata: {finish_chunk}\n\ndata: [DONE]\n\n");
        response(200, vec![sse.into_bytes()], None)
    }

    async fn streamed_tool_request_id(
        model_name: &str,
        completion_id: &str,
        tool_id: &str,
    ) -> String {
        let provider = MapleProvider::new(Arc::new(FakeTransport::new(tool_call_response(
            completion_id,
            tool_id,
        ))));
        let stream = provider
            .stream(
                &ModelConfig::new(model_name),
                "system",
                &[Message::user().with_text("use a tool")],
                &[],
            )
            .await
            .expect("stream should start");
        let (message, _) = collect_stream(stream)
            .await
            .expect("tool call should parse");
        message
            .content
            .iter()
            .find_map(|content| match content {
                MessageContent::ToolRequest(request) => Some(request.id.clone()),
                _ => None,
            })
            .expect("tool request should be present")
    }

    fn pending_success_response() -> InferenceResponse {
        let body: OpenSecretResponseBody = Box::pin(futures_util::stream::pending());
        let mut response = InferenceResponse::new(body);
        *response.status_mut() = http::StatusCode::OK;
        response
    }

    fn notifying_malformed_response(error_read: Arc<Notify>) -> InferenceResponse {
        let body: OpenSecretResponseBody = Box::pin(futures_util::stream::once(async move {
            error_read.notify_one();
            Ok(b"data: transient-invalid-stream\n\n".to_vec().into())
        }));
        let mut response = InferenceResponse::new(body);
        *response.status_mut() = http::StatusCode::OK;
        response
    }

    #[tokio::test]
    async fn formats_openai_request_and_preserves_images_and_thinking() {
        let transport = Arc::new(FakeTransport::new(fragmented_success_response()));
        let provider = MapleProvider::new(transport.clone());
        let messages = vec![
            Message::user()
                .with_text("What is in this image?")
                .with_image("aGVsbG8=", "image/png"),
            Message::assistant()
                .with_content(MessageContent::thinking("private chain", ""))
                .with_text("Prior answer"),
        ];
        let mut model_config =
            ModelConfig::new("test-model").with_merged_request_params(HashMap::from([
                ("include_reasoning".to_string(), json!(false)),
                (
                    "chat_template_kwargs".to_string(),
                    json!({ "enable_thinking": false }),
                ),
            ]));
        model_config.supports_vision = Some(true);

        let stream = provider
            .stream(&model_config, "Maple system prompt", &messages, &[])
            .await
            .expect("stream should start");
        let _ = collect_stream(stream).await.expect("stream should parse");

        assert_eq!(provider.get_name(), MAPLE_PROVIDER_NAME);
        let requests = transport.requests.lock().expect("request lock");
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.method, "POST");
        assert_eq!(request.uri, CHAT_COMPLETIONS_PATH);
        assert_eq!(request.accept.as_deref(), Some("text/event-stream"));
        assert_eq!(request.body["model"], "test-model");
        assert_eq!(request.body["stream"], true);
        assert_eq!(request.body["stream_options"]["include_usage"], true);
        assert_eq!(request.body["include_reasoning"], false);
        assert_eq!(
            request.body["chat_template_kwargs"]["enable_thinking"],
            false
        );
        assert_eq!(request.body["messages"][0]["role"], "system");
        assert_eq!(
            request.body["messages"][0]["content"],
            "Maple system prompt"
        );
        assert_eq!(
            request.body["messages"][2]["reasoning_content"],
            "private chain"
        );
        assert_eq!(
            request.body["messages"][1]["content"][1]["image_url"]["url"],
            "data:image/png;base64,aGVsbG8="
        );
    }

    fn screenshot_response(id: &str, sentinel: &str) -> Message {
        Message::user().with_tool_response(
            id,
            Ok(rmcp::model::CallToolResult::success(vec![
                rmcp::model::ContentBlock::text("window_id=7"),
                rmcp::model::ContentBlock::image(sentinel, "image/png"),
            ])),
        )
    }

    #[test]
    fn only_the_newest_tool_screenshots_stay_in_a_request() {
        let history = (0..MAX_RETAINED_TOOL_RESULT_IMAGES + 2)
            .map(|index| screenshot_response(&format!("call-{index}"), &format!("shot-{index}")))
            .collect::<Vec<_>>();

        let bounded = bound_tool_result_images(&history);
        let body = serde_json::to_string(&bounded).expect("bounded history should serialize");

        // The two oldest screenshots became markers; the newest three remain.
        assert!(!body.contains("shot-0"));
        assert!(!body.contains("shot-1"));
        for index in 2..MAX_RETAINED_TOOL_RESULT_IMAGES + 2 {
            assert!(body.contains(&format!("shot-{index}")));
        }
        assert!(body.contains("Earlier screenshot omitted"));
    }

    #[test]
    fn bounding_tool_screenshots_keeps_user_attached_images() {
        let attached = Message::user()
            .with_text("look at this")
            .with_image("user-attached-sentinel", "image/png");
        let history = (0..MAX_RETAINED_TOOL_RESULT_IMAGES + 1)
            .map(|index| screenshot_response(&format!("call-{index}"), &format!("shot-{index}")))
            .chain(std::iter::once(attached))
            .collect::<Vec<_>>();

        let body = serde_json::to_string(&bound_tool_result_images(&history))
            .expect("bounded history should serialize");

        assert!(body.contains("user-attached-sentinel"));
        assert!(!body.contains("shot-0"));
    }

    #[tokio::test]
    async fn mediated_cua_tool_result_does_not_emit_an_image_message() {
        let initial = Message::user().with_text("Inspect the Calculator window");
        let tool_request = Message::assistant().with_tool_request(
            "cua-call-1",
            Ok(rmcp::model::CallToolRequestParams::new(
                "cua-driver__get_window_state",
            )),
        );

        // Positive control: a vision-capable model receives the raw MCP image.
        // Maple's CUA adapter still mediates screenshots for text-only models.
        let raw_transport = Arc::new(FakeTransport::new(fragmented_success_response()));
        let raw_provider = MapleProvider::new(Arc::clone(&raw_transport));
        let raw_response = Message::user().with_tool_response(
            "cua-call-1",
            Ok(rmcp::model::CallToolResult::success(vec![
                rmcp::model::ContentBlock::text("window_id=7 size=900x600"),
                rmcp::model::ContentBlock::image("cua-image-sentinel", "image/png"),
            ])),
        );
        let mut vision_model = ModelConfig::new("vision-model");
        vision_model.supports_vision = Some(true);
        let raw_stream = raw_provider
            .stream(
                &vision_model,
                "system",
                &[initial.clone(), tool_request.clone(), raw_response],
                &[],
            )
            .await
            .expect("raw control request should serialize");
        let _ = collect_stream(raw_stream)
            .await
            .expect("raw control response should parse");
        let raw_body = serde_json::to_string(&raw_transport.requests.lock().unwrap()[0].body)
            .expect("raw request should serialize");
        assert!(raw_body.contains("image_url"));
        assert!(raw_body.contains("cua-image-sentinel"));

        let mediated_transport = Arc::new(FakeTransport::new(fragmented_success_response()));
        let mediated_provider = MapleProvider::new(Arc::clone(&mediated_transport));
        let mut mediated_result = rmcp::model::CallToolResult::success(vec![
            rmcp::model::ContentBlock::text("window_id=7 size=900x600"),
            rmcp::model::ContentBlock::text(
                "Computer-use vision helper description: Calculator shows 437.",
            ),
        ]);
        mediated_result.structured_content = Some(serde_json::json!({
            "window_id": 7,
            "screenshot_width": 900,
            "screenshot_height": 600
        }));
        let mediated_response =
            Message::user().with_tool_response("cua-call-1", Ok(mediated_result));
        let mediated_stream = mediated_provider
            .stream(
                &ModelConfig::new("deepseek-v4-1-flash"),
                "system",
                &[initial, tool_request, mediated_response],
                &[],
            )
            .await
            .expect("mediated request should serialize");
        let _ = collect_stream(mediated_stream)
            .await
            .expect("mediated response should parse");
        let mediated_body =
            serde_json::to_string(&mediated_transport.requests.lock().unwrap()[0].body)
                .expect("mediated request should serialize");
        assert!(!mediated_body.contains("image_url"));
        assert!(!mediated_body.contains("cua-image-sentinel"));
        assert!(mediated_body.contains("Calculator shows 437"));
    }

    #[tokio::test]
    async fn primary_agent_stream_enables_thinking_only_for_direct_gemma_selection() {
        let gemma_transport = Arc::new(FakeTransport::new(fragmented_success_response()));
        let gemma_provider = MapleProvider::new(gemma_transport.clone());
        let gemma_config = super::super::maple_model_config(GEMMA4_AGENT_MODEL_ID, None)
            .expect("Gemma model config");

        let gemma_stream = gemma_provider
            .stream(
                &gemma_config,
                "system",
                &[Message::user().with_text("reason carefully")],
                &[],
            )
            .await
            .expect("Gemma stream should start");
        let _ = collect_stream(gemma_stream)
            .await
            .expect("Gemma stream should parse");

        {
            let gemma_requests = gemma_transport.requests.lock().expect("request lock");
            assert_eq!(gemma_requests[0].body["include_reasoning"], true);
            assert_eq!(
                gemma_requests[0].body["chat_template_kwargs"]["enable_thinking"],
                true
            );
        }

        let llama_transport = Arc::new(FakeTransport::new(fragmented_success_response()));
        let llama_provider = MapleProvider::new(llama_transport.clone());
        let llama_config =
            super::super::maple_model_config("llama3-3-70b", None).expect("Llama model config");

        let llama_stream = llama_provider
            .stream(
                &llama_config,
                "system",
                &[Message::user().with_text("answer directly")],
                &[],
            )
            .await
            .expect("Llama stream should start");
        let _ = collect_stream(llama_stream)
            .await
            .expect("Llama stream should parse");

        let llama_requests = llama_transport.requests.lock().expect("request lock");
        assert!(llama_requests[0].body.get("include_reasoning").is_none());
        assert!(llama_requests[0].body.get("chat_template_kwargs").is_none());
    }

    #[tokio::test]
    async fn gemma_auxiliary_completion_keeps_thinking_disabled() {
        let transport = Arc::new(FakeTransport::new(fragmented_success_response()));
        let provider = MapleProvider::new(transport.clone());
        let model_config = super::super::maple_model_config(GEMMA4_AGENT_MODEL_ID, None)
            .expect("Gemma model config");

        provider
            .complete(
                &model_config,
                "system",
                &[Message::user().with_text("summarize")],
                &[],
            )
            .await
            .expect("Gemma completion should parse");

        let requests = transport.requests.lock().expect("request lock");
        assert_eq!(requests[0].body["include_reasoning"], false);
        assert_eq!(
            requests[0].body["chat_template_kwargs"]["enable_thinking"],
            false
        );
    }

    #[test]
    fn gemma_request_defaults_preserve_explicit_controls_and_skip_aliases() {
        let explicit_off =
            ModelConfig::new(GEMMA4_AGENT_MODEL_ID).with_merged_request_params(HashMap::from([
                ("include_reasoning".to_string(), json!(false)),
                (
                    "chat_template_kwargs".to_string(),
                    json!({ "enable_thinking": false, "custom": "kept" }),
                ),
            ]));
        let primary = MapleProvider::gemma_agent_model_config(&explicit_off, true);
        let primary_params = primary.request_params.expect("primary request params");
        assert_eq!(primary_params["include_reasoning"], false);
        assert_eq!(
            primary_params["chat_template_kwargs"]["enable_thinking"],
            false
        );
        assert_eq!(primary_params["chat_template_kwargs"]["custom"], "kept");

        let explicit_on =
            ModelConfig::new(GEMMA4_AGENT_MODEL_ID).with_merged_request_params(HashMap::from([
                ("include_reasoning".to_string(), json!(true)),
                (
                    "chat_template_kwargs".to_string(),
                    json!({ "enable_thinking": true }),
                ),
            ]));
        let auxiliary = MapleProvider::gemma_agent_model_config(&explicit_on, false);
        let auxiliary_params = auxiliary.request_params.expect("auxiliary request params");
        assert_eq!(auxiliary_params["include_reasoning"], true);
        assert_eq!(
            auxiliary_params["chat_template_kwargs"]["enable_thinking"],
            true
        );

        let alias =
            MapleProvider::gemma_agent_model_config(&ModelConfig::new("auto:powerful"), true);
        assert!(alias.request_params.is_none());
    }

    #[tokio::test]
    async fn reassembles_fragmented_sse_chunks_before_openai_parsing() {
        let provider =
            MapleProvider::new(Arc::new(FakeTransport::new(fragmented_success_response())));

        let stream = provider
            .stream(
                &ModelConfig::new("test-model"),
                "system",
                &[Message::user().with_text("hello")],
                &[],
            )
            .await
            .expect("stream should start");
        let (message, usage) = collect_stream(stream).await.expect("stream should parse");
        let text = message
            .content
            .iter()
            .filter_map(|content| match content {
                MessageContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<String>();

        assert_eq!(text, "Hello world");
        assert_eq!(usage.model, "test-model");
        assert_eq!(usage.usage.input_tokens, Some(2));
        assert_eq!(usage.usage.output_tokens, Some(3));
        assert_eq!(usage.usage.total_tokens, Some(5));
    }

    #[tokio::test]
    async fn replaces_only_legacy_kimi_k3_tool_ids_with_conversation_unique_ids() {
        let first = streamed_tool_request_id(KIMI_K3_MODEL_ID, "completion-1", "shell:0").await;
        let second = streamed_tool_request_id(KIMI_K3_MODEL_ID, "completion-2", "shell:0").await;

        assert!(first.starts_with("chatcmpl-tool-"));
        assert!(second.starts_with("chatcmpl-tool-"));
        assert_ne!(first, second);

        let missing_index =
            streamed_tool_request_id(KIMI_K3_MODEL_ID, "completion-3", "shell").await;
        assert!(missing_index.starts_with("chatcmpl-tool-"));

        let malformed_index =
            streamed_tool_request_id(KIMI_K3_MODEL_ID, "completion-4", "shell:not-a-number").await;
        assert!(malformed_index.starts_with("chatcmpl-tool-"));

        let already_unique = streamed_tool_request_id(
            KIMI_K3_MODEL_ID,
            "completion-5",
            "chatcmpl-tool-upstream-unique",
        )
        .await;
        assert_eq!(already_unique, "chatcmpl-tool-upstream-unique");

        let other_model = streamed_tool_request_id("glm-5-2", "completion-6", "shell:0").await;
        assert_eq!(other_model, "shell:0");

        let kimi_k2_6 = streamed_tool_request_id("kimi-k2-6", "completion-7", "shell:0").await;
        assert_eq!(kimi_k2_6, "shell:0");
    }

    #[test]
    fn normalizes_kimi_k3_ids_without_defeating_duplicate_detection() {
        let mut message = Message::assistant()
            .with_tool_request(
                "shell:0",
                Ok(rmcp::model::CallToolRequestParams::new("shell")),
            )
            .with_tool_request(
                "shell:0",
                Ok(rmcp::model::CallToolRequestParams::new("shell")),
            );
        let mut replacements = HashMap::new();

        MapleProvider::replace_legacy_kimi_k3_tool_ids(&mut message, &mut replacements);

        let ids = message
            .content
            .iter()
            .filter_map(|content| match content {
                MessageContent::ToolRequest(request) => Some(request.id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], ids[1]);
        assert!(ids[0].starts_with("chatcmpl-tool-"));

        let mut errored_message = Message::assistant()
            .with_tool_request(
                "shell:0",
                Err(rmcp::model::ErrorData::invalid_params(
                    "bad arguments",
                    None,
                )),
            )
            .with_tool_request(
                "chatcmpl-tool-upstream-unique",
                Err(rmcp::model::ErrorData::invalid_params(
                    "bad arguments",
                    None,
                )),
            );
        let mut errored_replacements = HashMap::new();
        MapleProvider::replace_legacy_kimi_k3_tool_ids(
            &mut errored_message,
            &mut errored_replacements,
        );
        let errored_ids = errored_message
            .content
            .iter()
            .filter_map(|content| match content {
                MessageContent::ToolRequest(request) => Some(request.id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(errored_ids[0].starts_with("chatcmpl-tool-"));
        assert_eq!(errored_ids[1], "chatcmpl-tool-upstream-unique");
    }

    #[tokio::test]
    async fn normalized_kimi_k3_id_round_trips_with_its_tool_response() {
        let transport = Arc::new(FakeTransport::with_responses(vec![
            tool_call_response("completion-1", "shell:0"),
            tool_call_response("completion-2", "shell:0"),
        ]));
        let provider = MapleProvider::new(Arc::clone(&transport));
        let model_config = ModelConfig::new(KIMI_K3_MODEL_ID);
        let initial_message = Message::user().with_text("use a tool twice");

        let first_stream = provider
            .stream(
                &model_config,
                "system",
                std::slice::from_ref(&initial_message),
                &[],
            )
            .await
            .expect("first stream should start");
        let (first_tool_message, _) = collect_stream(first_stream)
            .await
            .expect("first tool call should parse");
        let first_id = first_tool_message
            .content
            .iter()
            .find_map(|content| match content {
                MessageContent::ToolRequest(request) => Some(request.id.clone()),
                _ => None,
            })
            .expect("first tool request should be present");
        let first_tool_response = Message::user().with_tool_response(
            first_id.clone(),
            Ok(rmcp::model::CallToolResult::success(vec![
                rmcp::model::ContentBlock::text("done"),
            ])),
        );

        let second_stream = provider
            .stream(
                &model_config,
                "system",
                &[initial_message, first_tool_message, first_tool_response],
                &[],
            )
            .await
            .expect("second stream should start");
        let (second_tool_message, _) = collect_stream(second_stream)
            .await
            .expect("second tool call should parse");
        let second_id = second_tool_message
            .content
            .iter()
            .find_map(|content| match content {
                MessageContent::ToolRequest(request) => Some(request.id.clone()),
                _ => None,
            })
            .expect("second tool request should be present");

        assert_ne!(first_id, second_id);
        let requests = transport.requests.lock().expect("request lock");
        let second_request_messages = requests[1].body["messages"]
            .as_array()
            .expect("request messages should be an array");
        let serialized_tool_request = second_request_messages
            .iter()
            .find(|message| message["tool_calls"].is_array())
            .expect("assistant tool request should be serialized");
        let serialized_tool_response = second_request_messages
            .iter()
            .find(|message| message["role"] == "tool")
            .expect("tool response should be serialized");
        assert_eq!(serialized_tool_request["tool_calls"][0]["id"], first_id);
        assert_eq!(serialized_tool_response["tool_call_id"], first_id);
    }

    #[tokio::test]
    async fn retries_invalid_stream_before_first_item_with_the_same_request() {
        let transport = Arc::new(FakeTransport::queued(vec![
            malformed_response("transient-invalid-stream"),
            fragmented_success_response(),
        ]));
        let provider =
            MapleProvider::new(Arc::clone(&transport)).with_test_retry_config(fast_retry_config(3));

        let stream = provider
            .stream(
                &ModelConfig::new("test-model"),
                "system",
                &[Message::user().with_text("hello")],
                &[],
            )
            .await
            .expect("replacement stream should start");
        let (message, usage) = collect_stream(stream)
            .await
            .expect("replacement stream should parse");
        let text = message
            .content
            .iter()
            .filter_map(|content| match content {
                MessageContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<String>();

        assert_eq!(text, "Hello world");
        assert_eq!(usage.usage.total_tokens, Some(5));
        let requests = transport.requests.lock().expect("request lock");
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].raw_body, requests[1].raw_body);
        assert_eq!(requests[0].body, requests[1].body);
    }

    #[tokio::test]
    async fn does_not_retry_after_the_first_successful_item() {
        let first_chunk = concat!(
            "data: {\"id\":\"chunk-1\",\"object\":\"chat.completion.chunk\",",
            "\"created\":1,\"model\":\"test-model\",\"choices\":[{\"index\":0,",
            "\"delta\":{\"role\":\"assistant\",\"content\":\"partial\"},",
            "\"finish_reason\":null}]}\n\n"
        )
        .as_bytes()
        .to_vec();
        let interrupted = response_with_items(
            200,
            vec![
                Ok(first_chunk),
                Err(maple_sdk::Error::InvalidResponse(
                    "private post-item stream failure".to_string(),
                )),
            ],
            None,
        );
        let transport = Arc::new(FakeTransport::queued(vec![
            interrupted,
            fragmented_success_response(),
        ]));
        let provider =
            MapleProvider::new(Arc::clone(&transport)).with_test_retry_config(fast_retry_config(3));

        let mut stream = provider
            .stream(
                &ModelConfig::new("test-model"),
                "system",
                &[Message::user().with_text("hello")],
                &[],
            )
            .await
            .expect("the first item should start the stream");
        let (message, usage) = stream
            .next()
            .await
            .expect("first item")
            .expect("first item should parse");
        let message = message.expect("first item should contain text");
        assert!(usage.is_none());
        assert!(matches!(
            message.content.as_slice(),
            [MessageContent::Text(text)] if text.text == "partial"
        ));

        let error = stream
            .next()
            .await
            .expect("the interruption should be surfaced")
            .expect_err("the interruption should remain an error");
        assert_eq!(
            error,
            ProviderError::ExecutionError(SECURE_CONNECTION_ERROR_MESSAGE.to_string())
        );
        assert_eq!(transport.request_count(), 1);
        assert_eq!(transport.remaining_response_count(), 1);
    }

    #[tokio::test]
    async fn does_not_retry_after_a_usage_only_item() {
        let usage_chunk = concat!(
            "data: {\"id\":\"usage-1\",\"object\":\"chat.completion.chunk\",",
            "\"created\":1,\"model\":\"test-model\",\"choices\":[],",
            "\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":3,\"total_tokens\":5}}\n\n"
        )
        .as_bytes()
        .to_vec();
        let interrupted = response_with_items(
            200,
            vec![
                Ok(usage_chunk),
                Err(maple_sdk::Error::InvalidResponse(
                    "private post-usage stream failure".to_string(),
                )),
            ],
            None,
        );
        let transport = Arc::new(FakeTransport::queued(vec![
            interrupted,
            fragmented_success_response(),
        ]));
        let provider =
            MapleProvider::new(Arc::clone(&transport)).with_test_retry_config(fast_retry_config(3));

        let mut stream = provider
            .stream(
                &ModelConfig::new("test-model"),
                "system",
                &[Message::user().with_text("hello")],
                &[],
            )
            .await
            .expect("usage should count as the first item");
        let (message, usage) = stream
            .next()
            .await
            .expect("usage item")
            .expect("usage item should parse");
        assert!(message.is_none());
        assert_eq!(usage.expect("usage").usage.total_tokens, Some(5));
        assert!(matches!(
            stream.next().await,
            Some(Err(ProviderError::ExecutionError(message)))
                if message == SECURE_CONNECTION_ERROR_MESSAGE
        ));
        assert_eq!(transport.request_count(), 1);
        assert_eq!(transport.remaining_response_count(), 1);
    }

    #[tokio::test]
    async fn shares_one_retry_budget_across_status_and_first_item_failures() {
        let mut responses = vec![response(
            503,
            vec![br#"{"error":{"message":"temporarily unavailable"}}"#.to_vec()],
            None,
        )];
        responses.extend(
            (0..TRANSIENT_MAX_RETRIES)
                .map(|index| malformed_response(&format!("invalid-stream-{index}"))),
        );
        responses.push(fragmented_success_response());
        let transport = Arc::new(FakeTransport::queued(responses));
        let provider = MapleProvider::new(Arc::clone(&transport));
        let default_max_retries = Provider::retry_config(&provider).max_retries();
        assert_eq!(default_max_retries, TRANSIENT_MAX_RETRIES);
        let provider = provider.with_test_retry_config(fast_retry_config(default_max_retries));

        let result = provider
            .stream(
                &ModelConfig::new("test-model"),
                "system",
                &[Message::user().with_text("hello")],
                &[],
            )
            .await;
        let error = match result {
            Ok(_) => panic!("the shared retry budget should be exhausted"),
            Err(error) => error,
        };

        assert_eq!(
            error,
            ProviderError::NetworkError("Maple's response stream was invalid".to_string())
        );
        assert_eq!(transport.request_count(), TRANSIENT_MAX_RETRIES + 1);
        assert_eq!(transport.remaining_response_count(), 1);
    }

    #[tokio::test]
    async fn retries_an_incomplete_tool_call_before_it_is_yielded() {
        let interrupted = response_with_items(
            200,
            vec![
                Ok(incomplete_tool_call_sse()),
                Ok(b"data: transient-invalid-tool-stream\n\n".to_vec()),
            ],
            None,
        );
        let replacement = response(200, vec![complete_tool_call_sse()], None);
        let transport = Arc::new(FakeTransport::queued(vec![interrupted, replacement]));
        let provider =
            MapleProvider::new(Arc::clone(&transport)).with_test_retry_config(fast_retry_config(3));

        let stream = provider
            .stream(
                &ModelConfig::new("test-model"),
                "system",
                &[Message::user().with_text("search")],
                &[],
            )
            .await
            .expect("replacement tool stream should start");
        let (message, usage) = collect_stream(stream)
            .await
            .expect("replacement tool stream should parse");
        let calls = message
            .content
            .iter()
            .filter_map(|content| match content {
                MessageContent::ToolRequest(request) => request.tool_call.as_ref().ok(),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "web_search");
        assert_eq!(
            calls[0]
                .arguments
                .as_ref()
                .and_then(|arguments| arguments.get("query")),
            Some(&json!("maple"))
        );
        assert_eq!(usage.usage.total_tokens, Some(5));
        assert_eq!(transport.request_count(), 2);
    }

    #[tokio::test]
    async fn leaves_an_incomplete_tool_call_ending_in_done_as_an_empty_stream() {
        let mut incomplete_then_done = incomplete_tool_call_sse();
        incomplete_then_done.extend_from_slice(b"data: [DONE]\n\n");
        let transport = Arc::new(FakeTransport::queued(vec![
            response(200, vec![incomplete_then_done], None),
            fragmented_success_response(),
        ]));
        let provider =
            MapleProvider::new(Arc::clone(&transport)).with_test_retry_config(fast_retry_config(3));

        let mut stream = provider
            .stream(
                &ModelConfig::new("test-model"),
                "system",
                &[Message::user().with_text("search")],
                &[],
            )
            .await
            .expect("DONE should preserve Goose's empty-stream recovery path");

        assert!(stream.next().await.is_none());
        assert_eq!(transport.request_count(), 1);
        assert_eq!(transport.remaining_response_count(), 1);
    }

    #[tokio::test]
    async fn does_not_retry_after_a_complete_tool_call_is_yielded() {
        let interrupted = response_with_items(
            200,
            vec![
                Ok(complete_tool_call_event()),
                Err(maple_sdk::Error::InvalidResponse(
                    "private completed tool stream failure".to_string(),
                )),
            ],
            None,
        );
        let transport = Arc::new(FakeTransport::queued(vec![
            interrupted,
            fragmented_success_response(),
        ]));
        let provider =
            MapleProvider::new(Arc::clone(&transport)).with_test_retry_config(fast_retry_config(3));

        let mut stream = provider
            .stream(
                &ModelConfig::new("test-model"),
                "system",
                &[Message::user().with_text("search")],
                &[],
            )
            .await
            .expect("complete tool item should start the stream");
        let (message, _) = stream
            .next()
            .await
            .expect("tool item")
            .expect("tool item should parse");
        let message = message.expect("tool item should contain a message");
        assert!(
            message
                .content
                .iter()
                .any(|content| matches!(content, MessageContent::ToolRequest(_)))
        );

        let error = stream
            .next()
            .await
            .expect("the interruption should be surfaced")
            .expect_err("the interruption should remain an error");
        assert_eq!(
            error,
            ProviderError::ExecutionError(SECURE_CONNECTION_ERROR_MESSAGE.to_string())
        );
        assert_eq!(transport.request_count(), 1);
        assert_eq!(transport.remaining_response_count(), 1);
    }

    #[tokio::test]
    async fn maps_rate_limit_without_exposing_body_and_preserves_retry_hint() {
        let response = response(
            429,
            vec![br#"{"error":{"message":"private upstream detail"}}"#.to_vec()],
            Some("7"),
        );

        let error = match ensure_success(response).await {
            Ok(_) => panic!("429 should fail"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            ProviderError::RateLimitExceeded {
                details: "Maple rate limit exceeded".to_string(),
                retry_delay: Some(Duration::from_secs(7)),
            }
        );
    }

    #[test]
    fn invalid_body_retry_hint_falls_back_to_retry_after_header() {
        let valid = json!({
            "error": { "metadata": { "retry_after_seconds": 2.5 } }
        });
        assert_eq!(
            retry_after_delay(Some(&valid), Some("7")),
            Some(Duration::from_secs_f64(2.5))
        );

        let invalid = json!({
            "error": { "metadata": { "retry_after_seconds": "not-a-number" } }
        });
        assert_eq!(
            retry_after_delay(Some(&invalid), Some("7")),
            Some(Duration::from_secs(7))
        );

        let negative = json!({
            "error": { "metadata": { "retry_after_seconds": -1 } }
        });
        assert_eq!(
            retry_after_delay(Some(&negative), Some("9")),
            Some(Duration::from_secs(9))
        );
    }

    #[test]
    fn parses_http_date_retry_after_header() {
        let retry_at = SystemTime::now() + Duration::from_secs(120);
        let header = httpdate::fmt_http_date(retry_at);
        let delay = retry_after_delay(None, Some(&header)).expect("HTTP date should parse");

        assert!(delay >= Duration::from_secs(118));
        assert!(delay <= Duration::from_secs(120));
    }

    #[tokio::test]
    async fn maps_common_http_failures_to_typed_provider_errors() {
        let unauthorized = ensure_success(response(
            401,
            vec![br#"{"error":{"message":"expired"}}"#.to_vec()],
            None,
        ))
        .await;
        assert!(matches!(
            unauthorized,
            Err(ProviderError::Authentication(_))
        ));

        let context = ensure_success(response(
            400,
            vec![br#"{"error":{"message":"maximum context length exceeded"}}"#.to_vec()],
            None,
        ))
        .await;
        assert!(matches!(
            context,
            Err(ProviderError::ContextLengthExceeded(ref message))
                if message == "The Maple request exceeds the model's context window"
        ));

        let credits = ensure_success(response(
            402,
            vec![br#"{"error":{"message":"insufficient credits"}}"#.to_vec()],
            None,
        ))
        .await;
        assert!(matches!(
            credits,
            Err(ProviderError::CreditsExhausted { .. })
        ));

        let server = ensure_success(response(
            503,
            vec![br#"{"error":{"message":"temporarily unavailable"}}"#.to_vec()],
            None,
        ))
        .await;
        assert!(matches!(server, Err(ProviderError::ServerError(_))));
    }

    #[test]
    fn forbidden_errors_are_request_failures_not_authentication() {
        let retry_config = fast_retry_config(3);
        let errors = [
            map_http_error(http::StatusCode::FORBIDDEN, None),
            map_opensecret_error(maple_sdk::Error::Api {
                status: 403,
                message: "private plan detail".to_string(),
            }),
        ];

        for error in errors {
            assert_eq!(
                error,
                ProviderError::RequestFailed("Maple request failed with status 403".to_string())
            );
            assert!(!should_retry(&error, &retry_config));
        }
    }

    #[tokio::test]
    async fn session_not_found_contract_is_exact_and_fail_closed() {
        let exact = error_contract_response(Some("1"), Some("session_not_found"));
        assert!(has_exact_session_not_found_contract(
            exact.status(),
            exact.headers()
        ));

        let mut invalid = vec![
            error_contract_response(None, None),
            error_contract_response(None, Some("session_not_found")),
            error_contract_response(Some("01"), Some("session_not_found")),
            error_contract_response(Some("1, 1"), Some("session_not_found")),
            error_contract_response(Some("1"), None),
            error_contract_response(Some("1"), Some("unknown")),
            error_contract_response(Some("1"), Some("access_token_expired")),
        ];

        let mut duplicate_contract = error_contract_response(Some("1"), Some("session_not_found"));
        duplicate_contract
            .headers_mut()
            .append(ERROR_CONTRACT_HEADER, http::HeaderValue::from_static("1"));
        invalid.push(duplicate_contract);

        let mut duplicate_code = error_contract_response(Some("1"), Some("session_not_found"));
        duplicate_code.headers_mut().append(
            ERROR_CODE_HEADER,
            http::HeaderValue::from_static("session_not_found"),
        );
        invalid.push(duplicate_code);

        for response in invalid {
            assert!(!has_exact_session_not_found_contract(
                response.status(),
                response.headers()
            ));
            let error = match ensure_success(response).await {
                Ok(_) => panic!("a 400 response should fail"),
                Err(error) => error,
            };
            assert_eq!(
                error,
                ProviderError::RequestFailed(
                    "Maple rejected the inference request (400)".to_string()
                )
            );
        }
    }

    #[test]
    fn secure_connection_sdk_errors_are_fixed_and_non_transient() {
        let retry_config = fast_retry_config(3);
        let errors = vec![
            maple_sdk::Error::Session("private session detail".to_string()),
            maple_sdk::Error::KeyExchange("private key detail".to_string()),
            maple_sdk::Error::Encryption("private encryption detail".to_string()),
            maple_sdk::Error::Decryption("private decryption detail".to_string()),
            maple_sdk::Error::InvalidResponse("private response detail".to_string()),
            maple_sdk::Error::Crypto("private crypto detail".to_string()),
            maple_sdk::Error::Cbor("private cbor detail".to_string()),
            maple_sdk::Error::Io(std::io::Error::other("private io detail")),
            maple_sdk::Error::Utf8(
                String::from_utf8(vec![0xff]).expect_err("invalid UTF-8 fixture"),
            ),
            maple_sdk::Error::Base64Decode(base64::DecodeError::InvalidByte(0, b'%')),
        ];

        for error in errors {
            let error = map_opensecret_error(error);
            assert_eq!(
                error,
                ProviderError::ExecutionError(SECURE_CONNECTION_ERROR_MESSAGE.to_string())
            );
            assert!(!should_retry(&error, &retry_config));
        }
    }

    #[tokio::test]
    async fn terminal_authentication_is_one_send_and_is_latched_for_the_run() {
        let transport = Arc::new(FakeTransport::with_responses(vec![
            response(401, vec![br#"{"message":"Invalid JWT"}"#.to_vec()], None),
            fragmented_success_response(),
        ]));
        let provider =
            MapleProvider::new(Arc::clone(&transport)).with_test_retry_config(fast_retry_config(3));
        let model_config = ModelConfig::new("test-model");
        let messages = [Message::user().with_text("hello")];

        let (result, terminal_error) = with_run_cancellation(CancellationToken::new(), async {
            let result = provider
                .stream(&model_config, "system", &messages, &[])
                .await;
            (result, take_terminal_run_error())
        })
        .await;

        assert!(matches!(result, Err(ProviderError::Authentication(_))));
        assert_eq!(
            terminal_error.as_deref(),
            Some(AUTHENTICATION_ERROR_MESSAGE)
        );
        assert_eq!(transport.request_count(), 1);
        assert_eq!(transport.remaining_response_count(), 1);
    }

    #[tokio::test]
    async fn terminal_coded_session_failure_is_one_send_and_is_latched_for_the_run() {
        let transport = Arc::new(FakeTransport::with_responses(vec![
            error_contract_response(Some("1"), Some("session_not_found")),
            fragmented_success_response(),
        ]));
        let provider =
            MapleProvider::new(Arc::clone(&transport)).with_test_retry_config(fast_retry_config(3));
        let model_config = ModelConfig::new("test-model");
        let messages = [Message::user().with_text("hello")];

        let (result, terminal_error) = with_run_cancellation(CancellationToken::new(), async {
            let result = provider
                .stream(&model_config, "system", &messages, &[])
                .await;
            (result, take_terminal_run_error())
        })
        .await;

        let error = match result {
            Ok(_) => panic!("coded session failure should be terminal"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            ProviderError::ExecutionError(SECURE_CONNECTION_ERROR_MESSAGE.to_string())
        );
        assert_eq!(
            terminal_error.as_deref(),
            Some(SECURE_CONNECTION_ERROR_MESSAGE)
        );
        assert_eq!(transport.request_count(), 1);
        assert_eq!(transport.remaining_response_count(), 1);
    }

    #[test]
    fn device_clock_attestation_failures_get_clock_guidance_without_sdk_detail() {
        // The SDK marks clock-caused attestation failures with this prefix.
        let clock_error = maple_sdk::Error::AttestationVerificationFailed(
            "This device's clock is about 3 days ahead of the secure enclave (private detail)"
                .to_string(),
        );
        assert!(clock_error.is_device_clock_problem());
        let mapped = map_opensecret_error(clock_error);
        assert_eq!(
            mapped,
            ProviderError::ExecutionError(DEVICE_CLOCK_ERROR_MESSAGE.to_string())
        );
        assert!(!format!("{mapped:?}").contains("private detail"));

        let other = maple_sdk::Error::AttestationVerificationFailed(
            "private attestation detail".to_string(),
        );
        assert_eq!(
            map_opensecret_error(other),
            ProviderError::ExecutionError(ATTESTATION_VERIFICATION_ERROR_MESSAGE.to_string())
        );
    }

    #[tokio::test]
    async fn terminal_attestation_failure_is_one_send_and_is_latched_for_the_run() {
        let transport = Arc::new(FakeTransport::with_results(vec![
            Err(maple_sdk::Error::AttestationVerificationFailed(
                "private attestation detail".to_string(),
            )),
            Ok(fragmented_success_response()),
        ]));
        let provider =
            MapleProvider::new(Arc::clone(&transport)).with_test_retry_config(fast_retry_config(3));
        let model_config = ModelConfig::new("test-model");
        let messages = [Message::user().with_text("hello")];

        let (result, terminal_error) = with_run_cancellation(CancellationToken::new(), async {
            let result = provider
                .stream(&model_config, "system", &messages, &[])
                .await;
            (result, take_terminal_run_error())
        })
        .await;

        let error = match result {
            Ok(_) => panic!("attestation verification failure should be terminal"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            ProviderError::ExecutionError(ATTESTATION_VERIFICATION_ERROR_MESSAGE.to_string())
        );
        assert_eq!(
            terminal_error.as_deref(),
            Some(ATTESTATION_VERIFICATION_ERROR_MESSAGE)
        );
        assert_eq!(transport.request_count(), 1);
        assert_eq!(transport.remaining_response_count(), 1);
    }

    #[tokio::test]
    async fn terminal_secure_connection_failure_is_one_send_and_is_latched_for_the_run() {
        let transport = Arc::new(FakeTransport::with_results(vec![
            Err(maple_sdk::Error::Session(
                "private exhausted session detail".to_string(),
            )),
            Ok(fragmented_success_response()),
        ]));
        let provider =
            MapleProvider::new(Arc::clone(&transport)).with_test_retry_config(fast_retry_config(3));
        let model_config = ModelConfig::new("test-model");
        let messages = [Message::user().with_text("hello")];

        let (result, terminal_error) = with_run_cancellation(CancellationToken::new(), async {
            let result = provider
                .stream(&model_config, "system", &messages, &[])
                .await;
            (result, take_terminal_run_error())
        })
        .await;

        let error = match result {
            Ok(_) => panic!("secure connection failure should be terminal"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            ProviderError::ExecutionError(SECURE_CONNECTION_ERROR_MESSAGE.to_string())
        );
        assert_eq!(
            terminal_error.as_deref(),
            Some(SECURE_CONNECTION_ERROR_MESSAGE)
        );
        assert_eq!(transport.request_count(), 1);
        assert_eq!(transport.remaining_response_count(), 1);
    }

    #[tokio::test]
    async fn terminal_secure_stream_failure_is_one_send_and_is_latched_for_the_run() {
        let failed = response_with_items(
            200,
            vec![Err(maple_sdk::Error::Decryption(
                "private lazy decryption detail".to_string(),
            ))],
            None,
        );
        let transport = Arc::new(FakeTransport::queued(vec![
            failed,
            fragmented_success_response(),
        ]));
        let provider =
            MapleProvider::new(Arc::clone(&transport)).with_test_retry_config(fast_retry_config(3));
        let model_config = ModelConfig::new("test-model");
        let messages = [Message::user().with_text("hello")];

        let (result, terminal_error) = with_run_cancellation(CancellationToken::new(), async {
            let result = provider
                .stream(&model_config, "system", &messages, &[])
                .await;
            (result, take_terminal_run_error())
        })
        .await;

        let error = match result {
            Ok(_) => panic!("lazy secure stream failure should be terminal"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            ProviderError::ExecutionError(SECURE_CONNECTION_ERROR_MESSAGE.to_string())
        );
        assert_eq!(
            terminal_error.as_deref(),
            Some(SECURE_CONNECTION_ERROR_MESSAGE)
        );
        assert_eq!(transport.request_count(), 1);
        assert_eq!(transport.remaining_response_count(), 1);
    }

    #[tokio::test]
    async fn http_and_sdk_error_details_are_redacted_from_returned_errors() {
        const PRIVATE_DETAIL: &str = "tenant-secret-provider-debug-message";

        for status in [400, 401, 402, 404, 413, 429, 500] {
            let body = serde_json::to_vec(&json!({
                "error": { "message": PRIVATE_DETAIL }
            }))
            .expect("error payload should serialize");
            let error = match ensure_success(response(status, vec![body], None)).await {
                Ok(_) => panic!("non-success response should fail"),
                Err(error) => error,
            };
            assert!(!error.to_string().contains(PRIVATE_DETAIL));
            assert!(!format!("{error:?}").contains(PRIVATE_DETAIL));

            let sdk_error = map_opensecret_error(maple_sdk::Error::Api {
                status,
                message: PRIVATE_DETAIL.to_string(),
            });
            assert!(!sdk_error.to_string().contains(PRIVATE_DETAIL));
            assert!(!format!("{sdk_error:?}").contains(PRIVATE_DETAIL));
        }
    }

    #[test]
    fn sdk_context_error_is_classified_without_exposing_provider_message() {
        let error = map_opensecret_error(maple_sdk::Error::Api {
            status: 400,
            message: "maximum context length exceeded; private token counts".to_string(),
        });

        assert_eq!(
            error,
            ProviderError::ContextLengthExceeded(
                "The Maple request exceeds the model's context window".to_string()
            )
        );
        assert!(!error.to_string().contains("private token counts"));
    }

    #[tokio::test]
    async fn stalled_error_response_stream_has_a_bounded_idle_timeout() {
        let mut response = pending_success_response();
        *response.status_mut() = http::StatusCode::SERVICE_UNAVAILABLE;

        let error = match ensure_success(response).await {
            Ok(_) => panic!("stalled error body should time out"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            ProviderError::NetworkError("Maple's error response stream timed out".to_string())
        );
    }

    #[tokio::test]
    async fn malformed_sse_is_a_typed_stream_error() {
        const PRIVATE_MALFORMED_LINE: &str = "private-decrypted-malformed-completion";
        let provider = MapleProvider::new(Arc::new(FakeTransport::new(malformed_response(
            PRIVATE_MALFORMED_LINE,
        ))))
        .with_test_retry_config(fast_retry_config(0));
        let result = provider
            .stream(
                &ModelConfig::new("test-model"),
                "system",
                &[Message::user().with_text("hello")],
                &[],
            )
            .await;
        let error = match result {
            Ok(_) => panic!("malformed completion data should fail"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            ProviderError::NetworkError("Maple's response stream was invalid".to_string())
        );
        assert!(!error.to_string().contains(PRIVATE_MALFORMED_LINE));
        assert!(!format!("{error:?}").contains(PRIVATE_MALFORMED_LINE));
    }

    #[tokio::test]
    async fn deterministic_client_errors_are_not_retried() {
        let transport = Arc::new(FakeTransport::new(response(
            400,
            vec![br#"{"error":{"message":"invalid model argument"}}"#.to_vec()],
            None,
        )));
        let provider = MapleProvider::new(Arc::clone(&transport));

        let result = provider
            .stream(
                &ModelConfig::new("test-model"),
                "system",
                &[Message::user().with_text("hello")],
                &[],
            )
            .await;

        assert!(matches!(result, Err(ProviderError::RequestFailed(_))));
        assert_eq!(transport.requests.lock().expect("request lock").len(), 1);
        let retry_config = Provider::retry_config(&provider);
        assert!(!should_retry(
            &ProviderError::RequestFailed("invalid".to_string()),
            &retry_config
        ));
        assert!(should_retry(
            &ProviderError::ServerError("temporary".to_string()),
            &retry_config
        ));
        assert!(should_retry(
            &ProviderError::RateLimitExceeded {
                details: "slow down".to_string(),
                retry_delay: None,
            },
            &retry_config
        ));
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_request_before_response_start() {
        let provider = MapleProvider::new(Arc::new(PendingTransport));
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let result = with_run_cancellation(
            cancellation,
            provider.stream(
                &ModelConfig::new("test-model"),
                "system",
                &[Message::user().with_text("hello")],
                &[],
            ),
        )
        .await;

        assert!(
            matches!(result, Err(ProviderError::ExecutionError(message)) if message.contains("cancelled"))
        );
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_stalled_response_stream() {
        let transport = Arc::new(FakeTransport::new(pending_success_response()));
        let provider = MapleProvider::new(Arc::clone(&transport));
        let cancellation = CancellationToken::new();
        let model_config = ModelConfig::new("test-model");
        let messages = [Message::user().with_text("hello")];
        let stream = with_run_cancellation(
            cancellation.clone(),
            provider.stream(&model_config, "system", &messages, &[]),
        );
        tokio::pin!(stream);

        tokio::select! {
            _ = transport.wait_for_request_count(1) => {}
            result = &mut stream => panic!("stream unexpectedly finished before cancellation: {}", result.is_ok()),
        }

        cancellation.cancel();
        let result = stream.await;
        assert!(
            matches!(result, Err(ProviderError::ExecutionError(message)) if message.contains("cancelled"))
        );
        assert_eq!(transport.request_count(), 1);
    }

    #[tokio::test]
    async fn cancellation_interrupts_first_item_retry_backoff() {
        let error_read = Arc::new(Notify::new());
        let transport = Arc::new(FakeTransport::queued(vec![
            notifying_malformed_response(Arc::clone(&error_read)),
            fragmented_success_response(),
        ]));
        let retry_config = RetryConfig::new(3, 60_000, 1.0, 60_000).transient_only();
        let provider =
            MapleProvider::new(Arc::clone(&transport)).with_test_retry_config(retry_config);
        let cancellation = CancellationToken::new();
        let model_config = ModelConfig::new("test-model");
        let messages = [Message::user().with_text("hello")];
        let stream = with_run_cancellation(
            cancellation.clone(),
            provider.stream(&model_config, "system", &messages, &[]),
        );
        tokio::pin!(stream);

        tokio::select! {
            _ = error_read.notified() => {}
            result = &mut stream => panic!("stream unexpectedly finished before cancellation: {}", result.is_ok()),
        }

        cancellation.cancel();
        let result = tokio::time::timeout(Duration::from_secs(1), stream)
            .await
            .expect("cancellation should interrupt backoff");
        assert!(
            matches!(result, Err(ProviderError::ExecutionError(message)) if message.contains("cancelled"))
        );
        assert_eq!(transport.request_count(), 1);
        assert_eq!(transport.remaining_response_count(), 1);
    }

    #[tokio::test]
    async fn stalled_response_stream_has_a_bounded_idle_timeout() {
        let provider = MapleProvider::new(Arc::new(FakeTransport::new(pending_success_response())))
            .with_test_retry_config(fast_retry_config(0));
        let result = provider
            .stream(
                &ModelConfig::new("test-model"),
                "system",
                &[Message::user().with_text("hello")],
                &[],
            )
            .await;
        assert!(matches!(result, Err(ProviderError::NetworkError(_))));
    }

    #[tokio::test]
    async fn reconstructs_fragmented_parallel_tool_calls_with_empty_finish_reasons_and_formats_schema()
     {
        let sse = concat!(
            "data: {\"id\":\"tools-1\",\"object\":\"chat.completion.chunk\",",
            "\"created\":1,\"model\":\"test-model\",\"choices\":[{\"index\":0,",
            "\"delta\":{\"role\":\"assistant\",\"tool_calls\":[",
            "{\"index\":0,\"id\":\"call-1\",\"type\":\"function\",",
            "\"function\":{\"name\":\"web_search\",\"arguments\":\"{\\\"query\\\":\\\"map\"}},",
            "{\"index\":1,\"id\":\"call-2\",\"type\":\"function\",",
            "\"function\":{\"name\":\"web_search\",\"arguments\":\"{\\\"query\\\":\\\"kag\"}}]},",
            "\"finish_reason\":\"\"}]}\n\n",
            "data: {\"id\":\"tools-2\",\"object\":\"chat.completion.chunk\",",
            "\"created\":2,\"model\":\"test-model\",\"choices\":[{\"index\":0,",
            "\"delta\":{\"tool_calls\":[",
            "{\"index\":0,\"function\":{\"arguments\":\"le\\\"}\"}},",
            "{\"index\":1,\"function\":{\"arguments\":\"i\\\"}\"}}]},",
            "\"finish_reason\":\"\"}]}\n\n",
            "data: {\"id\":\"tools-3\",\"object\":\"chat.completion.chunk\",",
            "\"created\":3,\"model\":\"test-model\",\"choices\":[{\"index\":0,",
            "\"delta\":{},\"finish_reason\":\"tool_calls\"}],",
            "\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":5,\"total_tokens\":9}}\n\n",
            "data: [DONE]\n\n"
        );
        let split = sse.len() / 3;
        let transport = Arc::new(FakeTransport::new(response(
            200,
            vec![
                sse.as_bytes()[..split].to_vec(),
                sse.as_bytes()[split..split * 2].to_vec(),
                sse.as_bytes()[split * 2..].to_vec(),
            ],
            None,
        )));
        let provider = MapleProvider::new(Arc::clone(&transport));
        let tool = Tool::new(
            "web_search",
            "Search the web",
            object!({
                "type": "object",
                "$defs": {
                    "query_value": {
                        "oneOf": [
                            { "type": "string" },
                            { "type": "number" }
                        ]
                    }
                },
                "properties": {
                    "query": { "$ref": "#/$defs/query_value" }
                },
                "required": ["query"]
            }),
        );

        let stream = provider
            .stream(
                &ModelConfig::new("test-model"),
                "system",
                &[Message::user().with_text("search twice")],
                &[tool],
            )
            .await
            .expect("stream should start");
        let (message, usage) = collect_stream(stream).await.expect("tools should parse");
        let calls = message
            .content
            .iter()
            .filter_map(|content| match content {
                MessageContent::ToolRequest(request) => request.tool_call.as_ref().ok(),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "web_search");
        assert_eq!(
            calls[0]
                .arguments
                .as_ref()
                .and_then(|arguments| arguments.get("query")),
            Some(&json!("maple"))
        );
        assert_eq!(calls[1].name, "web_search");
        assert_eq!(
            calls[1]
                .arguments
                .as_ref()
                .and_then(|arguments| arguments.get("query")),
            Some(&json!("kagi"))
        );
        assert_eq!(usage.usage.total_tokens, Some(9));

        let requests = transport.requests.lock().expect("request lock");
        assert_eq!(requests[0].body["tools"][0]["type"], "function");
        assert_eq!(
            requests[0].body["tools"][0]["function"]["name"],
            "web_search"
        );
        assert_eq!(
            requests[0].body["tools"][0]["function"]["parameters"]["required"][0],
            "query"
        );
        let parameters = &requests[0].body["tools"][0]["function"]["parameters"];
        assert!(parameters["$defs"]["query_value"].get("oneOf").is_none());
        assert!(parameters["$defs"]["query_value"]["anyOf"].is_array());
    }

    #[tokio::test]
    async fn bounds_and_redacts_non_json_error_bodies() {
        let response = response(500, vec![vec![b'x'; MAX_ERROR_BODY_BYTES + 50]], None);
        let error = match ensure_success(response).await {
            Ok(_) => panic!("500 should fail"),
            Err(error) => error,
        };
        let ProviderError::ServerError(details) = error else {
            panic!("expected server error");
        };

        assert_eq!(details, "Maple's server returned status 500");
        assert!(!details.contains('x'));
    }
}
