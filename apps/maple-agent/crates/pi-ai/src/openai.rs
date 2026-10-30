//! The OpenAI-compatible Chat Completions API (`api: "openai-completions"`).
//!
//! The provider builds the request from the transcript, sends it through an
//! [`HttpTransport`] and turns the server-sent events into an assistant stream. The
//! transport is the host's: a plain HTTP client, an in-process proxy or a test double.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::constrained::{make_strict_json_schema, resolve_strict_sampling};
use crate::estimate::clamp_max_tokens_to_context;
use crate::provider::{ProviderEventHook, ProviderResponse, StreamFn, StreamOptions};
use crate::provider_retry::RequestFailure;
use crate::stream::{AssistantMessageBuilder, AssistantMessageStream};
use crate::transcript::{
    collapse_system_messages, current_tools, render_system_message_update, system_message_text,
};
use crate::types::{
    AssistantContent, AssistantMessage, Content, Context, MaxTokensField, Message, Model,
    SamplingParams, SessionAffinityFormat, StopReason, ThinkingLevel, Tool, ToolResultMessage,
    Usage,
};

pub const API: &str = "openai-completions";

/// The fields servers stream reasoning in. The one a response used becomes its thinking
/// signature, and the reasoning goes back to the model in that field.
const REASONING_FIELDS: [&str; 3] = ["reasoning_content", "reasoning", "reasoning_text"];

/// An HTTP request the provider wants sent.
#[derive(Clone, Debug)]
pub struct HttpRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// A response whose body arrives as a stream.
pub struct HttpResponse {
    pub status: u16,
    /// The response headers. Retries and response hooks read them.
    pub headers: Vec<(String, String)>,
    pub body: BoxStream<'static, Result<Bytes, String>>,
}

/// Sends provider requests. Return `Err` only for transport failures; HTTP error
/// statuses are responses.
///
/// `cancel` is the request's cancellation, as Pi hands its fetch an abort signal. The
/// provider stops waiting once it fires, so a transport with work that must finish (a
/// credential refresh, say) can see the cancellation and wind down on its own.
#[async_trait]
pub trait HttpTransport: Send + Sync {
    async fn post(
        &self,
        request: HttpRequest,
        cancel: CancellationToken,
    ) -> Result<HttpResponse, String>;
}

/// The Chat Completions provider.
#[derive(Clone)]
pub struct OpenAiCompletions {
    transport: Arc<dyn HttpTransport>,
}

impl OpenAiCompletions {
    pub fn new(transport: Arc<dyn HttpTransport>) -> Self {
        Self { transport }
    }

    /// The provider over a default HTTP client.
    #[cfg(feature = "reqwest")]
    pub fn with_reqwest(client: reqwest::Client) -> Self {
        Self::new(Arc::new(ReqwestTransport { client }))
    }
}

impl StreamFn for OpenAiCompletions {
    fn stream(
        &self,
        model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> AssistantMessageStream {
        let (sender, stream) = AssistantMessageStream::channel();
        let transport = self.transport.clone();
        let model = model.clone();
        tokio::spawn(async move {
            let builder = AssistantMessageBuilder::new(sender, &model);
            run(transport, &model, context, options, builder).await;
        });
        stream
    }
}

const ABORTED: &str = "Request was aborted";

async fn run(
    transport: Arc<dyn HttpTransport>,
    model: &Model,
    context: Context,
    mut options: StreamOptions,
    builder: AssistantMessageBuilder,
) {
    let cancel = options.cancel.clone();
    if let Some(hook) = options.transform_headers.clone() {
        options.headers = hook(std::mem::take(&mut options.headers)).await;
    }
    let mut body = match build_request_body(model, &context, &options) {
        Ok(body) => body,
        Err(error) => {
            builder.fail(StopReason::Error, error);
            return;
        }
    };
    if let Some(hook) = &options.on_payload {
        body = hook(body).await;
    }
    let request = HttpRequest {
        url: format!("{}/chat/completions", model.base_url.trim_end_matches('/')),
        headers: request_headers(model, &options),
        body: serde_json::to_vec(&body).unwrap_or_default(),
    };
    let response = match send(transport.as_ref(), request, &options).await {
        Ok(response) => response,
        Err((stop_reason, error)) => {
            builder.fail(stop_reason, error);
            return;
        }
    };
    if let Some(hook) = &options.on_response {
        let headers = response
            .headers
            .iter()
            .map(|(name, value)| (name.to_ascii_lowercase(), value.clone()))
            .collect();
        hook(ProviderResponse {
            status: response.status,
            headers,
        })
        .await;
    }
    let on_event = options.on_provider_stream_event.as_ref();
    read_events(model, response.body, builder, &cancel, on_event).await;
}

/// How long a response may take to start when the request does not say, as the OpenAI
/// SDK waits.
const DEFAULT_TIMEOUT_MS: u64 = 600_000;

/// The OpenAI SDK's error for a response that did not start in time.
const TIMED_OUT: &str = "Request timed out.";

/// Pi's error for a request stopped while it waited to be sent again.
const RETRY_ABORTED: &str = "Request aborted";

/// Send `request` until a response starts, again after a failure as the options allow,
/// as Pi sends one through `retryProviderRequest`. An error status ends with the
/// provider's error.
async fn send(
    transport: &dyn HttpTransport,
    request: HttpRequest,
    options: &StreamOptions,
) -> Result<HttpResponse, (StopReason, String)> {
    let max_retries = options.max_retries.unwrap_or(0);
    let mut retry_index = 0;
    loop {
        let failure = match attempt(transport, request.clone(), options).await? {
            Ok(response) => return Ok(response),
            Err(failure) => failure,
        };
        if retry_index >= max_retries || !failure.is_retryable() {
            return Err((StopReason::Error, failure.message));
        }
        let delay = failure
            .retry_delay(retry_index, options.max_retry_delay_ms)
            .map_err(|error| (StopReason::Error, error))?;
        retry_index += 1;
        tokio::select! {
            _ = options.cancel.cancelled() => {
                return Err((StopReason::Aborted, RETRY_ABORTED.to_string()));
            }
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

/// Send `request` once. It fails with an error status, or when no response starts.
async fn attempt(
    transport: &dyn HttpTransport,
    request: HttpRequest,
    options: &StreamOptions,
) -> Result<Result<HttpResponse, RequestFailure>, (StopReason, String)> {
    let cancel = &options.cancel;
    let aborted = || (StopReason::Aborted, ABORTED.to_string());
    // A timeout reaches the transport as the cancellation of this request alone.
    let request_cancel = cancel.child_token();
    let timeout = Duration::from_millis(options.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
    let response = tokio::select! {
        _ = cancel.cancelled() => return Err(aborted()),
        _ = tokio::time::sleep(timeout) => {
            request_cancel.cancel();
            return Ok(Err(RequestFailure::without_response(TIMED_OUT)));
        }
        response = transport.post(request, request_cancel.clone()) => response,
    };
    let response = match response {
        Ok(response) => response,
        Err(error) => return Ok(Err(RequestFailure::without_response(error))),
    };
    if (200..300).contains(&response.status) {
        return Ok(Ok(response));
    }
    let text = tokio::select! {
        _ = cancel.cancelled() => return Err(aborted()),
        text = read_error_body(response.body) => text,
    };
    Ok(Err(RequestFailure {
        status: Some(response.status),
        headers: response.headers,
        message: http_error_message(response.status, &text),
        sdk_message: sdk_error(response.status, &text).0,
    }))
}

fn request_headers(model: &Model, options: &StreamOptions) -> Vec<(String, String)> {
    let mut headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".to_string(), "text/event-stream".to_string()),
    ];
    if let Some(key) = &options.api_key {
        headers.push(("authorization".to_string(), format!("Bearer {key}")));
    }
    // The model's headers, the session's, then the request's own, each over the last.
    for (name, value) in &model.headers {
        set_header(&mut headers, name, Some(value));
    }
    if model.compat.send_session_affinity_headers
        && let Some(session_id) = session_id(options)
    {
        let names: &[&str] = match model.compat.session_affinity_format {
            SessionAffinityFormat::OpenAi => {
                &["session_id", "x-client-request-id", "x-session-affinity"]
            }
            SessionAffinityFormat::OpenAiNoSession => {
                &["x-client-request-id", "x-session-affinity"]
            }
            SessionAffinityFormat::OpenRouter => &["x-session-id"],
        };
        for name in names {
            set_header(&mut headers, name, Some(session_id));
        }
    }
    for (name, value) in &options.headers {
        set_header(&mut headers, name, value.as_deref());
    }
    headers
}

/// Set a header, replacing any of the same name; `None` removes it.
fn set_header(headers: &mut Vec<(String, String)>, name: &str, value: Option<&str>) {
    headers.retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
    if let Some(value) = value {
        headers.push((name.to_string(), value.to_string()));
    }
}

/// How much of an error body is read for the message.
const ERROR_TEXT_LIMIT: usize = 4_096;

async fn read_error_body(mut body: BoxStream<'static, Result<Bytes, String>>) -> String {
    let mut bytes = Vec::new();
    while let Some(Ok(chunk)) = body.next().await {
        bytes.extend_from_slice(&chunk);
        if bytes.len() >= ERROR_TEXT_LIMIT {
            bytes.truncate(ERROR_TEXT_LIMIT);
            break;
        }
    }
    String::from_utf8_lossy(&bytes).trim().to_string()
}

/// How much of an error object a message keeps, in characters, as in Pi.
const ERROR_BODY_CHARS: usize = 4_000;

/// The message for an error response, composed as Pi composes it from the OpenAI SDK's
/// error: the status and the provider's whole error object, or the body text when it is
/// not JSON, followed by OpenRouter's `metadata.raw`.
fn http_error_message(status: u16, text: &str) -> String {
    let (message, error) = sdk_error(status, text);
    let Some(error) = error else {
        return message;
    };
    let message = match error.as_object().filter(|object| !object.is_empty()) {
        Some(_) => {
            let body = truncate_error_text(&error.to_string());
            if message.contains(&body) {
                message
            } else {
                format!("{status}: {body}")
            }
        }
        None => message,
    };
    with_raw_metadata(message, &error)
}

/// The OpenAI SDK's message for an error response, and the error object it keeps: the
/// body's `error` field, or the whole body when it has none.
fn sdk_error(status: u16, text: &str) -> (String, Option<Value>) {
    let error = match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(body)) => {
            let inner = body.get("error").filter(|error| !error.is_null()).cloned();
            inner.unwrap_or(Value::Object(body))
        }
        Ok(list @ Value::Array(_)) => list,
        _ if text.is_empty() => return (format!("{status} status code (no body)"), None),
        _ => return (format!("{status} {text}"), None),
    };
    (
        format!("{status} {}", error_object_message(&error)),
        Some(error),
    )
}

/// The SDK's text for an error object: its `message`, or the object as JSON.
fn error_object_message(error: &Value) -> String {
    match error.get("message").filter(|message| is_set(message)) {
        Some(Value::String(message)) => message.clone(),
        Some(message) => message.to_string(),
        None if is_set(error) => error.to_string(),
        None => "(no status code or body)".into(),
    }
}

/// Add OpenRouter's `metadata.raw`, the upstream provider's own error, when the message
/// does not show it yet.
fn with_raw_metadata(mut message: String, error: &Value) -> String {
    if let Some(raw) = error.pointer("/metadata/raw").and_then(Value::as_str)
        && !raw.is_empty()
        && !message.contains(raw)
    {
        message.push('\n');
        message.push_str(raw);
    }
    message
}

pub(crate) fn truncate_error_text(text: &str) -> String {
    let length = text.chars().count();
    if length <= ERROR_BODY_CHARS {
        return text.to_string();
    }
    let kept: String = text.chars().take(ERROR_BODY_CHARS).collect();
    format!("{kept}... [truncated {} chars]", length - ERROR_BODY_CHARS)
}

/// Whether a value counts as given, as a JavaScript condition reads it: not null, false,
/// zero or empty text.
fn is_set(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64() != Some(0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// The provider's own message when `text` is the usual error object, otherwise `text`.
fn error_detail(text: &str) -> String {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .or_else(|| value.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| text.to_string())
}

/// A tool as Chat Completions declares it, in strict mode when it asks for that and the
/// provider and its schema allow it.
fn tool_declaration(tool: &Tool, supports_strict_mode: bool) -> Result<Value, String> {
    let strict = resolve_strict_sampling(tool, supports_strict_mode)?;
    let parameters = match strict {
        Some(true) => make_strict_json_schema(&tool.parameters)?,
        _ => tool.parameters.clone(),
    };
    let mut function = json!({
        "name": tool.name,
        "description": tool.description,
        "parameters": parameters,
    });
    // Only a provider that supports strict mode gets the field; some reject unknown ones.
    if supports_strict_mode {
        function["strict"] = json!(strict.unwrap_or(false));
    }
    Ok(json!({ "type": "function", "function": function }))
}

/// OpenAI's limit for `prompt_cache_key`.
const PROMPT_CACHE_KEY_CHARS: usize = 64;

fn session_id(options: &StreamOptions) -> Option<&str> {
    options.session_id.as_deref().filter(|id| !id.is_empty())
}

/// Build the Chat Completions request body for `context`. It fails only for a tool that
/// requires strict mode where it cannot have it.
pub fn build_request_body(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
) -> Result<Value, String> {
    let compat = &model.compat;
    let mut body = Map::new();
    body.insert("model".into(), json!(model.id));
    body.insert(
        "messages".into(),
        Value::Array(convert_messages(model, context)),
    );
    body.insert("stream".into(), json!(true));
    // OpenAI caches a session's prompts together under its key.
    if model.base_url.contains("api.openai.com")
        && let Some(session_id) = session_id(options)
    {
        let key: String = session_id.chars().take(PROMPT_CACHE_KEY_CHARS).collect();
        body.insert("prompt_cache_key".into(), json!(key));
    }
    if compat.supports_usage_in_streaming {
        body.insert("stream_options".into(), json!({ "include_usage": true }));
    }
    let max_tokens = clamp_max_tokens_to_context(
        model,
        context,
        options.max_tokens.unwrap_or(model.max_tokens),
    );
    if max_tokens > 0 {
        let field = match compat.max_tokens_field {
            MaxTokensField::MaxCompletionTokens => "max_completion_tokens",
            MaxTokensField::MaxTokens => "max_tokens",
        };
        body.insert(field.into(), json!(max_tokens));
    }
    if let Some(temperature) = options.temperature {
        body.insert("temperature".into(), json!(temperature));
    }
    let tools = current_tools(&context.messages);
    if !tools.is_empty() {
        let tools = tools
            .iter()
            .map(|tool| tool_declaration(tool, compat.supports_strict_mode))
            .collect::<Result<Vec<Value>, String>>()?;
        body.insert("tools".into(), Value::Array(tools));
    }
    if let Some(choice) = options.tool_choice {
        body.insert("tool_choice".into(), json!(choice));
    }
    let level = options.reasoning.unwrap_or(ThinkingLevel::Off);
    if compat.supports_reasoning_effort
        && let Some(effort) = model.provider_thinking_value(level)
    {
        body.insert("reasoning_effort".into(), json!(effort));
    }
    // Last, so sampling parameters override the named fields.
    let level = model.clamp_thinking_level(level);
    body.extend(sampling_params(model, level, &options.sampling_params));
    Ok(Value::Object(body))
}

/// The model's sampling parameters, its parameters for `level` over them, and the
/// request's over both.
fn sampling_params(
    model: &Model,
    level: ThinkingLevel,
    request: &SamplingParams,
) -> SamplingParams {
    let mut params = model.sampling_params.clone();
    if let Some(for_level) = model.sampling_params_by_thinking_level.get(&level) {
        params.extend(for_level.clone());
    }
    params.extend(request.clone());
    params
}

/// Make a transcript replayable: drop failed responses, answer every tool call exactly
/// once and drop results whose call is gone.
fn repair_transcript(messages: &[Message]) -> Vec<Message> {
    let mut repaired = Vec::with_capacity(messages.len());
    let mut pending: Vec<(String, String)> = Vec::new();
    let flush = |pending: &mut Vec<(String, String)>, repaired: &mut Vec<Message>| {
        for (id, name) in pending.drain(..) {
            repaired.push(Message::ToolResult(ToolResultMessage {
                tool_call_id: id,
                tool_name: name,
                content: vec![Content::text("No result provided")],
                details: None,
                usage: None,
                is_error: true,
                timestamp: 0,
            }));
        }
    };
    for message in messages {
        match message {
            Message::Assistant(assistant) if assistant.is_failure() => continue,
            Message::Assistant(assistant) => {
                flush(&mut pending, &mut repaired);
                pending.extend(
                    assistant
                        .tool_calls()
                        .map(|call| (call.id.clone(), call.name.clone())),
                );
                repaired.push(message.clone());
            }
            Message::ToolResult(result) => {
                if let Some(position) = pending
                    .iter()
                    .position(|(id, _)| *id == result.tool_call_id)
                {
                    pending.remove(position);
                    repaired.push(message.clone());
                }
            }
            Message::User(_) => {
                flush(&mut pending, &mut repaired);
                repaired.push(message.clone());
            }
            // A system message between a call and its results waits for the results.
            Message::System(_) => repaired.push(message.clone()),
        }
    }
    flush(&mut pending, &mut repaired);
    // Move system messages that landed inside a tool exchange after its results.
    let mut ordered: Vec<Message> = Vec::with_capacity(repaired.len());
    let mut held: Vec<Message> = Vec::new();
    for message in repaired {
        let in_exchange = matches!(ordered.last(), Some(Message::Assistant(a)) if a.tool_calls().next().is_some())
            || matches!(ordered.last(), Some(Message::ToolResult(_)));
        match message {
            Message::System(_) if in_exchange && !ordered.is_empty() => held.push(message),
            Message::ToolResult(_) => ordered.push(message),
            other => {
                ordered.append(&mut held);
                ordered.push(other);
            }
        }
    }
    ordered.append(&mut held);
    ordered
}

fn convert_messages(model: &Model, context: &Context) -> Vec<Value> {
    let context = if model.compat.supports_mid_conversation_system_messages {
        context.clone()
    } else {
        collapse_system_messages(context)
    };
    // Reasoning models take the system prompt as developer instructions.
    let system_role = if model.reasoning && model.compat.supports_developer_role {
        "developer"
    } else {
        "system"
    };
    let mut out = Vec::new();
    let mut images_from_tools: Vec<Value> = Vec::new();
    for (position, message) in repair_transcript(&context.messages).iter().enumerate() {
        if !matches!(message, Message::ToolResult(_)) && !images_from_tools.is_empty() {
            out.push(images_message(&mut images_from_tools));
        }
        match message {
            Message::System(system) => {
                let text = if position == 0 {
                    system_message_text(system)
                } else {
                    render_system_message_update(system)
                };
                if !text.is_empty() {
                    out.push(json!({ "role": system_role, "content": text }));
                }
            }
            Message::User(user) => {
                let content = user_content(model, &user.content);
                if !content.is_empty() {
                    out.push(json!({ "role": "user", "content": content }));
                }
            }
            Message::Assistant(assistant) => {
                // A model gets its own reasoning back, in the field it came in. Another
                // model's reasoning is dropped: sent as text, it reads as part of the reply,
                // and the new model can start writing its reasoning into its answers.
                let same_model = assistant.provider == model.provider
                    && assistant.api == model.api
                    && assistant.model == model.id;
                let mut text_parts = Vec::new();
                let mut reasoning = Vec::new();
                let mut reasoning_field = None;
                for block in &assistant.content {
                    match block {
                        AssistantContent::Text(text) if !text.text.trim().is_empty() => {
                            text_parts.push(text.text.as_str())
                        }
                        AssistantContent::Thinking(thinking)
                            if same_model
                                && !thinking.redacted
                                && !thinking.thinking.trim().is_empty() =>
                        {
                            reasoning.push(thinking.thinking.as_str());
                            reasoning_field =
                                reasoning_field.or(thinking.thinking_signature.as_deref());
                        }
                        _ => {}
                    }
                }
                // The blocks are pieces of one reply, so they join without a separator.
                let text = text_parts.concat();
                let calls: Vec<Value> = assistant
                    .tool_calls()
                    .map(|call| {
                        json!({
                            "id": call.id,
                            "type": "function",
                            "function": {
                                "name": call.name,
                                "arguments": Value::Object(call.arguments.clone()).to_string(),
                            },
                        })
                    })
                    .collect();
                if text.is_empty() && calls.is_empty() {
                    continue;
                }
                let mut entry = json!({ "role": "assistant", "content": if text.is_empty() { Value::Null } else { json!(text) } });
                if let Some(field) =
                    reasoning_field.filter(|field| REASONING_FIELDS.contains(field))
                {
                    entry[field] = json!(reasoning.join("\n"));
                }
                if !calls.is_empty() {
                    entry["tool_calls"] = Value::Array(calls);
                }
                out.push(entry);
            }
            Message::ToolResult(result) => {
                let content = downgrade_images(model, &result.content, TOOL_IMAGE_PLACEHOLDER);
                let text = crate::types::content_text(&content);
                let has_images = content
                    .iter()
                    .any(|block| matches!(block, Content::Image(_)));
                if has_images {
                    images_from_tools.extend(content.iter().filter_map(image_part));
                }
                let text = match (text.is_empty(), has_images) {
                    (false, _) => text,
                    (true, true) => "(see attached image)".to_string(),
                    (true, false) => "(no tool output)".to_string(),
                };
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": result.tool_call_id,
                    "content": text,
                }));
            }
        }
    }
    if !images_from_tools.is_empty() {
        out.push(images_message(&mut images_from_tools));
    }
    out
}

/// Tool messages cannot carry images, so they follow the results in a user message.
fn images_message(images: &mut Vec<Value>) -> Value {
    let mut content =
        vec![json!({ "type": "text", "text": "Attached image(s) from tool result:" })];
    content.append(images);
    json!({ "role": "user", "content": content })
}

fn image_part(block: &Content) -> Option<Value> {
    match block {
        Content::Image(image) => Some(json!({
            "type": "image_url",
            "image_url": { "url": format!("data:{};base64,{}", image.mime_type, image.data) },
        })),
        Content::Text(_) => None,
    }
}

/// The parts of a user message. Empty text is left out; a message left with no parts is
/// not sent.
fn user_content(model: &Model, content: &[Content]) -> Vec<Value> {
    downgrade_images(model, content, USER_IMAGE_PLACEHOLDER)
        .iter()
        .filter_map(|block| match block {
            Content::Text(text) if text.text.is_empty() => None,
            Content::Text(text) => Some(json!({ "type": "text", "text": text.text })),
            Content::Image(_) => image_part(block),
        })
        .collect()
}

const USER_IMAGE_PLACEHOLDER: &str = "(image omitted: model does not support images)";
const TOOL_IMAGE_PLACEHOLDER: &str = "(tool image omitted: model does not support images)";

/// `content` for a model without image input: each run of images is replaced by one
/// placeholder, where the images were.
fn downgrade_images<'a>(
    model: &Model,
    content: &'a [Content],
    placeholder: &str,
) -> Cow<'a, [Content]> {
    if model.supports_images() {
        return Cow::Borrowed(content);
    }
    let mut downgraded = Vec::with_capacity(content.len());
    let mut after_placeholder = false;
    for block in content {
        match block {
            Content::Image(_) => {
                if !after_placeholder {
                    downgraded.push(Content::text(placeholder));
                }
                after_placeholder = true;
            }
            Content::Text(text) => {
                after_placeholder = text.text == placeholder;
                downgraded.push(block.clone());
            }
        }
    }
    Cow::Owned(downgraded)
}

/// The message for a stream that ends, at `[DONE]` or not, without a finish reason.
const STREAM_CUT_OFF: &str = "Stream ended without finish_reason";

/// The SDK's error for an event whose data is not JSON.
const MALFORMED_EVENT: &str = "Error reading response: malformed server-sent event JSON.";

/// What a streamed completion has reported besides content and usage.
#[derive(Default)]
struct StreamState {
    /// The builder index of each tool call, by the provider's index.
    tool_blocks: HashMap<u64, usize>,
    /// The latest tool call, for servers that leave out `index`.
    last_tool: Option<usize>,
    /// Tool calls with a made-up id because the server had not sent one yet.
    made_up_ids: HashSet<usize>,
    finish_reason: Option<String>,
}

/// Read the server-sent events of a streamed completion into `builder`.
async fn read_events(
    model: &Model,
    mut body: BoxStream<'static, Result<Bytes, String>>,
    mut builder: AssistantMessageBuilder,
    cancel: &CancellationToken,
    on_event: Option<&ProviderEventHook>,
) {
    let mut decoder = Utf8Decoder::default();
    let mut events = EventReader::default();
    let mut state = StreamState::default();
    let mut ended = false;
    'read: while !ended {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => {
                builder.fail(StopReason::Aborted, ABORTED);
                return;
            }
            chunk = body.next() => chunk,
        };
        match chunk {
            None => ended = true,
            Some(Err(error)) => {
                builder.fail(StopReason::Error, error);
                return;
            }
            Some(Ok(chunk)) => events.push(&decoder.decode(&chunk)),
        }
        while let Some(event) = events.next(ended) {
            if event.data == "[DONE]" {
                break 'read;
            }
            // As the OpenAI SDK reads an event: its data must be JSON, an `error` event
            // fails with its error, and so does a chunk whose error is set.
            let Ok(chunk) = serde_json::from_str::<Value>(&event.data) else {
                builder.fail(StopReason::Error, MALFORMED_EVENT);
                return;
            };
            let error = match event.name.as_deref() {
                Some("error") => Some(
                    chunk
                        .get("error")
                        .filter(|error| !error.is_null())
                        .unwrap_or(&chunk),
                ),
                _ => chunk.get("error").filter(|error| is_set(error)),
            };
            if let Some(error) = error {
                let message = with_raw_metadata(error_object_message(error), error);
                builder.fail(StopReason::Error, message);
                return;
            }
            if let Some(hook) = on_event {
                hook(chunk.clone()).await;
            }
            apply_chunk(model, &chunk, &mut builder, &mut state);
        }
    }
    let Some(finish_reason) = state.finish_reason else {
        // The response was cut off. Its tool calls could carry half their arguments.
        let detail = error_detail(events.stray.trim());
        let error = if detail.is_empty() {
            STREAM_CUT_OFF.to_string()
        } else {
            format!("{STREAM_CUT_OFF}: {detail}")
        };
        builder.fail(StopReason::Error, error);
        return;
    };
    match stop_reason(&finish_reason) {
        Ok(stop_reason) => {
            let usage = builder.partial().usage;
            builder.finish(stop_reason, usage);
        }
        Err(error) => {
            builder.fail(StopReason::Error, error);
        }
    }
}

/// One server-sent event.
struct ServerEvent {
    name: Option<String>,
    data: String,
}

/// Reads server-sent events as the OpenAI SDK's decoder does: lines end at `\n`, `\r\n`
/// or `\r`, an event's `data:` lines join until a blank line, and an event left open
/// when the stream ends still counts.
#[derive(Default)]
struct EventReader {
    text: String,
    /// The last line ended at a `\r` that ended the text, so a `\n` next belongs to it.
    after_return: bool,
    name: Option<String>,
    data: Vec<String>,
    /// Lines that are not server-sent events, such as a JSON error body.
    stray: String,
}

impl EventReader {
    fn push(&mut self, mut text: &str) {
        if self.after_return && !text.is_empty() {
            self.after_return = false;
            text = text.strip_prefix('\n').unwrap_or(text);
        }
        self.text.push_str(text);
    }

    /// The next whole event, or once the stream has `ended` the one it left open.
    fn next(&mut self, ended: bool) -> Option<ServerEvent> {
        while let Some(line) = self.next_line(ended) {
            if line.is_empty() {
                if let Some(event) = self.dispatch() {
                    return Some(event);
                }
            } else {
                self.field(&line);
            }
        }
        if ended { self.dispatch() } else { None }
    }

    fn next_line(&mut self, ended: bool) -> Option<String> {
        let Some(end) = self.text.find(['\r', '\n']) else {
            // The last line may come without its newline.
            return (ended && !self.text.is_empty()).then(|| std::mem::take(&mut self.text));
        };
        let line = self.text[..end].to_string();
        let rest = &self.text[end..];
        let newline = if rest.starts_with("\r\n") { 2 } else { 1 };
        self.after_return = rest == "\r";
        self.text.drain(..end + newline);
        Some(line)
    }

    fn field(&mut self, line: &str) {
        if line.starts_with(':') {
            return;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.name = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            "id" | "retry" => {}
            _ => push_capped(&mut self.stray, line),
        }
    }

    /// The event gathered so far, when it has a name or data.
    fn dispatch(&mut self) -> Option<ServerEvent> {
        if self.name.as_deref().is_none_or(str::is_empty) && self.data.is_empty() {
            self.name = None;
            return None;
        }
        let data = std::mem::take(&mut self.data).join("\n");
        Some(ServerEvent {
            name: self.name.take(),
            data,
        })
    }
}

fn push_capped(text: &mut String, line: &str) {
    let mut end = ERROR_TEXT_LIMIT.saturating_sub(text.len()).min(line.len());
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    if end > 0 {
        text.push_str(&line[..end]);
        text.push('\n');
    }
}

fn apply_chunk(
    model: &Model,
    event: &Value,
    builder: &mut AssistantMessageBuilder,
    state: &mut StreamState,
) {
    if builder.partial().response_id.is_none()
        && let Some(id) = event.get("id").and_then(Value::as_str)
    {
        builder.set_response_id(id);
    }
    if let Some(reported) = event.get("usage").filter(|usage| usage.is_object()) {
        builder.set_usage(parse_usage(model, reported));
    }
    let Some(choice) = event.pointer("/choices/0") else {
        return;
    };
    // Moonshot reports usage in the choice instead.
    if event.get("usage").is_none_or(|usage| !is_set(usage))
        && let Some(reported) = choice.get("usage").filter(|usage| usage.is_object())
    {
        builder.set_usage(parse_usage(model, reported));
    }
    if let Some(delta) = choice.get("delta") {
        // Servers name the reasoning field differently, and some fill in two of them.
        if let Some((field, text)) = REASONING_FIELDS.iter().find_map(|field| {
            delta
                .get(*field)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(|text| (*field, text))
        }) {
            builder.thinking_delta(text);
            if newest_thinking_unsigned(builder.partial()) {
                builder.set_thinking_signature(field);
            }
        }
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            builder.text_delta(text);
        }
        for call in delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            apply_tool_call_delta(call, builder, state);
        }
    }
    if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
        state.finish_reason = Some(reason.to_string());
    }
}

/// Whether the newest thinking block has no signature yet.
fn newest_thinking_unsigned(message: &AssistantMessage) -> bool {
    message
        .content
        .iter()
        .rev()
        .find_map(|block| match block {
            AssistantContent::Thinking(thinking) => Some(thinking.thinking_signature.is_none()),
            _ => None,
        })
        .unwrap_or(false)
}

/// Add a tool-call fragment to its call. Fragments are matched by `index`. Servers that
/// leave it out send each call's fragments together, so there a new id starts a new
/// call. An id or name that comes after the first fragment is filled in.
fn apply_tool_call_delta(
    call: &Value,
    builder: &mut AssistantMessageBuilder,
    state: &mut StreamState,
) {
    let id = call
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty());
    let name = call
        .pointer("/function/name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty());
    let provider_index = call.get("index").and_then(Value::as_u64);
    let existing = match provider_index {
        Some(provider_index) => state.tool_blocks.get(&provider_index).copied(),
        None => state.last_tool.filter(|index| {
            id.is_none_or(|id| {
                state.made_up_ids.contains(index) || tool_call_id(builder, *index) == Some(id)
            })
        }),
    };
    let index = match existing {
        Some(index) => {
            let id = id.filter(|_| state.made_up_ids.remove(&index));
            builder.set_tool_call_identity(index, id, name);
            index
        }
        None => {
            let made_up = id.is_none();
            let id = id.map_or_else(
                || format!("call_{}", builder.partial().content.len()),
                str::to_string,
            );
            let index = builder.tool_call_start(id, name.unwrap_or_default());
            if made_up {
                state.made_up_ids.insert(index);
            }
            if let Some(provider_index) = provider_index {
                state.tool_blocks.insert(provider_index, index);
            }
            index
        }
    };
    state.last_tool = Some(index);
    if let Some(arguments) = call.pointer("/function/arguments").and_then(Value::as_str) {
        builder.tool_call_delta(index, arguments);
    }
}

fn tool_call_id(builder: &AssistantMessageBuilder, index: usize) -> Option<&str> {
    match builder.partial().content.get(index) {
        Some(AssistantContent::ToolCall(call)) => Some(&call.id),
        _ => None,
    }
}

/// Reported usage, priced with the model's costs. OpenAI and OpenRouter report cache
/// hits in `prompt_tokens_details`, DeepSeek as `prompt_cache_hit_tokens` and Kimi at the
/// top; OpenRouter reports cache writes beside its hits. Both are part of the prompt.
fn parse_usage(model: &Model, reported: &Value) -> Usage {
    let number = |pointer: &str| reported.pointer(pointer).and_then(Value::as_u64);
    let prompt = number("/prompt_tokens").unwrap_or(0);
    let output = number("/completion_tokens").unwrap_or(0);
    let cache_read = number("/prompt_tokens_details/cached_tokens")
        .or_else(|| number("/prompt_cache_hit_tokens"))
        .or_else(|| number("/cached_tokens"))
        .unwrap_or(0);
    let cache_write = number("/prompt_tokens_details/cache_write_tokens").unwrap_or(0);
    let input = prompt.saturating_sub(cache_read + cache_write);
    let mut usage = Usage {
        input,
        output,
        cache_read,
        cache_write,
        reasoning: number("/completion_tokens_details/reasoning_tokens"),
        total_tokens: input + output + cache_read + cache_write,
        ..Usage::default()
    };
    model.apply_cost(&mut usage);
    usage
}

/// The stop reason a finish reason stands for, or the error it is, as Pi maps them.
fn stop_reason(finish_reason: &str) -> Result<StopReason, String> {
    match finish_reason {
        "stop" | "end" => Ok(StopReason::Stop),
        "length" => Ok(StopReason::Length),
        "function_call" | "tool_calls" => Ok(StopReason::ToolUse),
        // Such as content_filter, network_error or z.ai's model_context_window_exceeded,
        // which the overflow and retry checks read.
        other => Err(format!("Provider finish_reason: {other}")),
    }
}

/// Decodes UTF-8 across chunk boundaries.
#[derive(Default)]
struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    fn decode(&mut self, chunk: &[u8]) -> String {
        self.pending.extend_from_slice(chunk);
        let valid = match std::str::from_utf8(&self.pending) {
            Ok(_) => self.pending.len(),
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            // Invalid bytes are not going to become valid; decode them lossily.
            Err(_) => self.pending.len(),
        };
        let text = String::from_utf8_lossy(&self.pending[..valid]).into_owned();
        self.pending.drain(..valid);
        text
    }
}

#[cfg(feature = "reqwest")]
struct ReqwestTransport {
    client: reqwest::Client,
}

#[cfg(feature = "reqwest")]
#[async_trait]
impl HttpTransport for ReqwestTransport {
    async fn post(
        &self,
        request: HttpRequest,
        _cancel: CancellationToken,
    ) -> Result<HttpResponse, String> {
        let mut builder = self.client.post(&request.url).body(request.body);
        for (name, value) in &request.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        let response = builder.send().await.map_err(|error| error.to_string())?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| Some((name.to_string(), value.to_str().ok()?.to_string())))
            .collect();
        let body = response
            .bytes_stream()
            .map(|chunk| chunk.map_err(|error| error.to_string()))
            .boxed();
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overflow::{is_context_overflow, is_retryable_error};
    use crate::types::{SystemMessage, ThinkingContent, Tool, ToolChoice, UserMessage};
    use futures_util::FutureExt;
    use std::sync::Mutex;

    /// Replies with canned SSE chunks and records each request.
    struct ScriptedTransport {
        status: u16,
        chunks: Vec<&'static str>,
        requests: Mutex<Vec<HttpRequest>>,
    }

    #[async_trait]
    impl HttpTransport for ScriptedTransport {
        async fn post(
            &self,
            request: HttpRequest,
            _cancel: CancellationToken,
        ) -> Result<HttpResponse, String> {
            self.requests.lock().unwrap().push(request);
            let chunks: Vec<Result<Bytes, String>> = self
                .chunks
                .iter()
                .map(|chunk| Ok(Bytes::from_static(chunk.as_bytes())))
                .collect();
            Ok(HttpResponse {
                status: self.status,
                headers: Vec::new(),
                body: futures_util::stream::iter(chunks).boxed(),
            })
        }
    }

    fn transport(status: u16, chunks: Vec<&'static str>) -> Arc<ScriptedTransport> {
        Arc::new(ScriptedTransport {
            status,
            chunks,
            requests: Mutex::new(Vec::new()),
        })
    }

    fn model() -> Model {
        serde_json::from_value(json!({
            "id": "gpt-x", "name": "GPT X", "api": API, "provider": "openai",
            "baseUrl": "https://example.test/v1/", "reasoning": true, "input": ["text", "image"],
            "contextWindow": 128000, "maxTokens": 4096, "cost": { "input": 1.0, "output": 2.0 },
        }))
        .unwrap()
    }

    fn tool() -> Tool {
        Tool::new(
            "read",
            "Read a file",
            json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
        )
    }

    #[tokio::test]
    async fn streams_text_reasoning_tool_calls_and_usage() {
        let transport = transport(
            200,
            vec![
                "data: {\"id\":\"r1\",\"choices\":[{\"delta\":{\"reasoning_content\":\"think\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"lo \\u00e9\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"pa\"}}]}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"th\\\":\\\"a\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
                "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":20,\"prompt_tokens_details\":{\"cached_tokens\":40},\"completion_tokens_details\":{\"reasoning_tokens\":5}}}\n\n",
                "data: [DONE]\n\n",
            ],
        );
        let provider = OpenAiCompletions::new(transport.clone());
        let context = Context::new(
            "sys",
            vec![tool()],
            vec![Message::User(UserMessage::text("hi"))],
        );
        let options = StreamOptions {
            api_key: Some("key".into()),
            reasoning: Some(ThinkingLevel::High),
            ..StreamOptions::default()
        };
        let message = provider.stream(&model(), context, options).result().await;

        assert_eq!(
            message.stop_reason,
            StopReason::ToolUse,
            "{:?}",
            message.error_message
        );
        assert_eq!(message.response_id.as_deref(), Some("r1"));
        assert_eq!(message.text(), "Hello é");
        assert!(
            matches!(&message.content[0], AssistantContent::Thinking(t) if t.thinking == "think")
        );
        let call = message.tool_calls().next().unwrap();
        assert_eq!(
            (call.id.as_str(), call.arguments["path"].as_str()),
            ("c1", Some("a"))
        );
        assert_eq!(
            (
                message.usage.input,
                message.usage.cache_read,
                message.usage.output
            ),
            (60, 40, 20)
        );
        assert_eq!(message.usage.reasoning, Some(5));
        assert!(message.usage.cost.total > 0.0);

        let request = transport.requests.lock().unwrap()[0].clone();
        assert_eq!(request.url, "https://example.test/v1/chat/completions");
        assert!(
            request
                .headers
                .contains(&("authorization".into(), "Bearer key".into()))
        );
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["max_completion_tokens"], 4096);
        assert_eq!(body["tools"][0]["function"]["name"], "read");
        assert_eq!(
            body["messages"][0],
            json!({ "role": "system", "content": "sys" })
        );
        assert_eq!(
            body["messages"][1],
            json!({ "role": "user", "content": [{ "type": "text", "text": "hi" }] })
        );
    }

    #[test]
    fn tools_that_ask_for_it_go_in_strict_mode_where_the_provider_has_it() {
        use crate::types::{ConstrainedSampling, StrictSampling};

        let prefer = tool().with_constrained_sampling(ConstrainedSampling::JsonSchema {
            strict: StrictSampling::Prefer,
        });
        let body = |model: &Model, tool: &Tool| {
            let context = Context::new("sys", vec![tool.clone()], Vec::new());
            build_request_body(model, &context, &StreamOptions::default())
        };

        // Off by default: the declaration is as it was, without the field.
        let plain = body(&model(), &prefer).unwrap();
        let function = &plain["tools"][0]["function"];
        assert!(function.get("strict").is_none());
        assert_eq!(function["parameters"], prefer.parameters);

        let mut strict_model = model();
        strict_model.compat.supports_strict_mode = true;
        let strict = body(&strict_model, &prefer).unwrap();
        let function = &strict["tools"][0]["function"];
        assert_eq!(function["strict"], true);
        assert_eq!(function["parameters"]["additionalProperties"], false);
        assert_eq!(function["parameters"]["required"], json!(["path"]));
        // A tool that does not ask is declared plainly, with strict off.
        let other = body(&strict_model, &tool()).unwrap();
        assert_eq!(other["tools"][0]["function"]["strict"], false);

        let require = tool().with_constrained_sampling(ConstrainedSampling::JsonSchema {
            strict: StrictSampling::Require,
        });
        assert_eq!(
            body(&model(), &require).unwrap_err(),
            "Tool \"read\" requires JSON-schema constrained sampling, but strict tools are unsupported."
        );
    }

    fn in_session(id: &str) -> StreamOptions {
        StreamOptions {
            session_id: Some(id.into()),
            ..StreamOptions::default()
        }
    }

    #[test]
    fn openai_caches_a_sessions_prompts_under_its_id() {
        let key = |model: &Model, options: &StreamOptions| {
            build_request_body(model, &Context::default(), options).unwrap()["prompt_cache_key"]
                .clone()
        };
        let mut openai = model();
        openai.base_url = "https://api.openai.com/v1".into();
        assert_eq!(key(&openai, &in_session("session-123")), "session-123");
        // Cut to OpenAI's limit.
        assert_eq!(
            key(&openai, &in_session(&"x".repeat(67))),
            json!("x".repeat(64))
        );
        assert_eq!(key(&openai, &StreamOptions::default()), Value::Null);
        // Other endpoints get no key.
        assert_eq!(key(&model(), &in_session("session-123")), Value::Null);
    }

    #[test]
    fn session_affinity_headers_follow_the_models_compat() {
        let sent = |model: &Model, options: &StreamOptions| {
            let headers = request_headers(model, options);
            [
                "session_id",
                "x-client-request-id",
                "x-session-affinity",
                "x-session-id",
            ]
            .into_iter()
            .filter(|name| headers.iter().any(|(sent, _)| sent == name))
            .collect::<Vec<_>>()
        };
        let options = in_session("s1");
        let mut model = model();
        assert!(sent(&model, &options).is_empty());
        model.compat.send_session_affinity_headers = true;
        assert_eq!(
            sent(&model, &options),
            ["session_id", "x-client-request-id", "x-session-affinity"]
        );
        assert!(
            request_headers(&model, &options).contains(&("x-session-affinity".into(), "s1".into()))
        );
        assert!(sent(&model, &StreamOptions::default()).is_empty());
        model.compat.session_affinity_format = SessionAffinityFormat::OpenAiNoSession;
        assert_eq!(
            sent(&model, &options),
            ["x-client-request-id", "x-session-affinity"]
        );
        model.compat.session_affinity_format = SessionAffinityFormat::OpenRouter;
        assert_eq!(sent(&model, &options), ["x-session-id"]);

        // A request's own headers win.
        let overridden = StreamOptions {
            headers: [("X-Session-Id".to_string(), Some("mine".to_string()))].into(),
            ..options
        };
        let headers = request_headers(&model, &overridden);
        assert!(headers.contains(&("X-Session-Id".into(), "mine".into())));
        assert!(!headers.iter().any(|(name, _)| name == "x-session-id"));
    }

    #[test]
    fn the_models_headers_come_under_the_requests_and_a_request_can_remove_one() {
        let mut model = model();
        model.headers = [
            ("X-Team".to_string(), "core".to_string()),
            ("X-Trace".to_string(), "model".to_string()),
        ]
        .into();
        let options = StreamOptions {
            api_key: Some("key".into()),
            headers: [
                ("x-trace".to_string(), Some("request".to_string())),
                ("Authorization".to_string(), None),
            ]
            .into(),
            ..StreamOptions::default()
        };
        let headers = request_headers(&model, &options);
        assert!(headers.contains(&("X-Team".into(), "core".into())));
        assert!(headers.contains(&("x-trace".into(), "request".into())));
        let traces = headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("x-trace"))
            .count();
        assert_eq!(traces, 1);
        assert!(!headers.iter().any(|(name, _)| name == "authorization"));
    }

    #[tokio::test]
    async fn a_header_hook_rewrites_the_requests_own_headers() {
        let transport = transport(200, vec![REPLY]);
        let options = StreamOptions {
            headers: [("X-Keep".to_string(), Some("1".to_string()))].into(),
            transform_headers: Some(Arc::new(|mut headers| {
                headers.insert("X-Added".into(), Some("2".into()));
                async move { headers }.boxed()
            })),
            ..StreamOptions::default()
        };
        let reply = OpenAiCompletions::new(transport.clone())
            .stream(&model(), Context::default(), options)
            .result()
            .await;
        assert_eq!(reply.text(), "hi");
        let headers = transport.requests.lock().unwrap()[0].headers.clone();
        assert!(headers.contains(&("X-Keep".into(), "1".into())));
        assert!(headers.contains(&("X-Added".into(), "2".into())));
    }

    #[test]
    fn tool_choice_and_sampling_parameters_reach_the_body() {
        let mut model = model();
        model.thinking_levels = [(ThinkingLevel::High, Some("high".into()))].into();
        model.sampling_params = json!({ "top_p": 0.9, "top_k": 40 })
            .as_object()
            .unwrap()
            .clone();
        model.sampling_params_by_thinking_level = [(
            ThinkingLevel::High,
            json!({ "top_k": 20, "temperature": 1.0 })
                .as_object()
                .unwrap()
                .clone(),
        )]
        .into();
        let options = StreamOptions {
            temperature: Some(0.2),
            tool_choice: Some(ToolChoice::None),
            reasoning: Some(ThinkingLevel::High),
            sampling_params: json!({ "min_p": 0.05, "top_p": 0.8 })
                .as_object()
                .unwrap()
                .clone(),
            ..StreamOptions::default()
        };
        let body = build_request_body(&model, &Context::default(), &options).unwrap();
        assert_eq!(body["tool_choice"], "none");
        // The level's parameters over the model's, the request's over both, and all of
        // them over the named fields.
        assert_eq!(body["top_p"], 0.8);
        assert_eq!(body["top_k"], 20);
        assert_eq!(body["min_p"], 0.05);
        assert_eq!(body["temperature"], 1.0);

        // A level the request does not run at adds nothing.
        let options = StreamOptions {
            temperature: Some(0.2),
            ..StreamOptions::default()
        };
        let body = build_request_body(&model, &Context::default(), &options).unwrap();
        assert_eq!(body["top_k"], 40);
        assert_eq!(body["temperature"], 0.2);
        assert_eq!(body.get("tool_choice"), None);
    }

    /// What a [`SequenceTransport`] answers a request with.
    enum Reply {
        Status(u16, Vec<(&'static str, &'static str)>, &'static str),
        Fail(&'static str),
        Hang,
    }

    /// Answers each request with the next reply, and records the requests and their
    /// cancellation tokens.
    #[derive(Default)]
    struct SequenceTransport {
        replies: Mutex<std::collections::VecDeque<Reply>>,
        cancels: Mutex<Vec<CancellationToken>>,
    }

    impl SequenceTransport {
        fn new(replies: Vec<Reply>) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(replies.into()),
                cancels: Mutex::new(Vec::new()),
            })
        }

        fn requests(&self) -> usize {
            self.cancels.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl HttpTransport for SequenceTransport {
        async fn post(
            &self,
            _request: HttpRequest,
            cancel: CancellationToken,
        ) -> Result<HttpResponse, String> {
            self.cancels.lock().unwrap().push(cancel);
            let reply = self.replies.lock().unwrap().pop_front();
            match reply.expect("an unexpected request") {
                Reply::Status(status, headers, body) => Ok(HttpResponse {
                    status,
                    headers: headers
                        .into_iter()
                        .map(|(name, value)| (name.to_string(), value.to_string()))
                        .collect(),
                    body: futures_util::stream::iter([Ok(Bytes::from_static(body.as_bytes()))])
                        .boxed(),
                }),
                Reply::Fail(error) => Err(error.to_string()),
                Reply::Hang => std::future::pending().await,
            }
        }
    }

    const REPLY: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

    const RATE_LIMITED: Reply = Reply::Status(
        429,
        vec![],
        "{\"error\":{\"message\":\"Too many requests\"}}",
    );

    /// How a rate limit's response fails: with the status and the provider's error.
    const TOO_MANY: &str = "429: {\"message\":\"Too many requests\"}";

    fn rate_limited(retry_after_ms: &'static str) -> Reply {
        Reply::Status(
            429,
            vec![("Retry-After-Ms", retry_after_ms)],
            "{\"error\":{\"message\":\"Too many requests\"}}",
        )
    }

    async fn reply_with(
        transport: Arc<SequenceTransport>,
        options: StreamOptions,
    ) -> AssistantMessage {
        OpenAiCompletions::new(transport)
            .stream(&model(), Context::default(), options)
            .result()
            .await
    }

    fn retries(count: u32) -> StreamOptions {
        StreamOptions {
            max_retries: Some(count),
            ..StreamOptions::default()
        }
    }

    #[tokio::test]
    async fn failed_requests_are_sent_again_when_retries_are_set() {
        let transport = SequenceTransport::new(vec![
            rate_limited("1"),
            Reply::Fail("connection reset"),
            Reply::Status(200, vec![], REPLY),
        ]);
        let reply = reply_with(transport.clone(), retries(2)).await;
        assert_eq!(
            reply.stop_reason,
            StopReason::Stop,
            "{:?}",
            reply.error_message
        );
        assert_eq!(reply.text(), "hi");
        assert_eq!(transport.requests(), 3);

        // Without retries, the first failure is the response's.
        let transport = SequenceTransport::new(vec![rate_limited("1")]);
        let reply = reply_with(transport.clone(), StreamOptions::default()).await;
        assert_eq!(reply.error_message.as_deref(), Some(TOO_MANY));
        assert_eq!(transport.requests(), 1);

        // Retries run out on the last failure.
        let transport = SequenceTransport::new(vec![rate_limited("1"), rate_limited("1")]);
        let reply = reply_with(transport.clone(), retries(1)).await;
        assert_eq!(reply.error_message.as_deref(), Some(TOO_MANY));
        assert_eq!(transport.requests(), 2);

        // A request the server refuses is not sent again.
        let transport = SequenceTransport::new(vec![Reply::Status(
            400,
            vec![],
            "{\"error\":{\"message\":\"bad request\"}}",
        )]);
        let reply = reply_with(transport.clone(), retries(3)).await;
        assert_eq!(
            reply.error_message.as_deref(),
            Some("400: {\"message\":\"bad request\"}")
        );
        assert_eq!(transport.requests(), 1);
    }

    #[tokio::test]
    async fn a_server_asking_too_long_a_wait_fails_the_request_at_once() {
        let transport = SequenceTransport::new(vec![Reply::Status(
            429,
            vec![("retry-after", "300")],
            "{\"error\":{\"message\":\"Too many requests\"}}",
        )]);
        let reply = reply_with(transport.clone(), retries(3)).await;
        assert_eq!(reply.stop_reason, StopReason::Error);
        assert_eq!(
            reply.error_message.as_deref(),
            Some("Server requested 300s retry delay (max: 60s). 429 Too many requests")
        );
        assert_eq!(transport.requests(), 1);
        // Such a reply is retried by the session instead.
        assert!(is_retryable_error(&reply));
    }

    #[tokio::test]
    async fn stopping_while_waiting_to_retry_aborts_the_request() {
        let transport = SequenceTransport::new(vec![RATE_LIMITED]);
        let cancel = CancellationToken::new();
        let options = StreamOptions {
            max_retries: Some(1),
            cancel: cancel.clone(),
            ..StreamOptions::default()
        };
        let stream =
            OpenAiCompletions::new(transport.clone()).stream(&model(), Context::default(), options);
        // The backoff for a first retry is at least 375 milliseconds.
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
        let reply = stream.result().await;
        assert_eq!(reply.stop_reason, StopReason::Aborted);
        assert_eq!(reply.error_message.as_deref(), Some("Request aborted"));
        assert_eq!(transport.requests(), 1);
    }

    #[tokio::test]
    async fn a_response_that_does_not_start_in_time_fails_or_is_retried() {
        let transport = SequenceTransport::new(vec![Reply::Hang]);
        let options = StreamOptions {
            timeout_ms: Some(20),
            ..StreamOptions::default()
        };
        let reply = reply_with(transport.clone(), options).await;
        assert_eq!(reply.stop_reason, StopReason::Error);
        assert_eq!(reply.error_message.as_deref(), Some("Request timed out."));
        assert!(is_retryable_error(&reply));
        // The transport sees the timeout as its request's cancellation.
        assert!(transport.cancels.lock().unwrap()[0].is_cancelled());

        let transport =
            SequenceTransport::new(vec![Reply::Hang, Reply::Status(200, vec![], REPLY)]);
        let options = StreamOptions {
            timeout_ms: Some(20),
            max_retries: Some(1),
            ..StreamOptions::default()
        };
        let reply = reply_with(transport.clone(), options).await;
        assert_eq!(reply.text(), "hi");
        assert_eq!(transport.requests(), 2);
    }

    #[tokio::test]
    async fn hooks_see_the_response_and_each_streamed_event() {
        let responses = Arc::new(Mutex::new(Vec::new()));
        let events = Arc::new(Mutex::new(Vec::new()));
        let (seen_responses, seen_events) = (responses.clone(), events.clone());
        let options = StreamOptions {
            on_response: Some(Arc::new(move |response: ProviderResponse| {
                seen_responses.lock().unwrap().push(response);
                async {}.boxed()
            })),
            on_provider_stream_event: Some(Arc::new(move |event: Value| {
                seen_events.lock().unwrap().push(event);
                async {}.boxed()
            })),
            ..StreamOptions::default()
        };
        let transport = SequenceTransport::new(vec![Reply::Status(
            200,
            vec![("X-Request-Id", "r-1")],
            REPLY,
        )]);
        let reply = reply_with(transport.clone(), options).await;
        assert_eq!(reply.text(), "hi");
        assert_eq!(
            *responses.lock().unwrap(),
            [ProviderResponse {
                status: 200,
                headers: [("x-request-id".to_string(), "r-1".to_string())].into(),
            }]
        );
        assert_eq!(
            *events.lock().unwrap(),
            [json!({ "choices": [{ "delta": { "content": "hi" }, "finish_reason": "stop" }] })]
        );
    }

    #[test]
    fn the_output_cap_is_fitted_to_the_context_window() {
        let mut model = model();
        model.context_window = 10_000;
        model.max_tokens = 8_000;
        let context = Context::new("sys", vec![], vec![Message::User(UserMessage::text("hi"))]);
        let cap = |options: &StreamOptions| {
            build_request_body(&model, &context, options).unwrap()["max_completion_tokens"].clone()
        };
        // The window less two estimated tokens and the 4,096 kept free.
        assert_eq!(cap(&StreamOptions::default()), 5_902);
        let small = StreamOptions {
            max_tokens: Some(1_000),
            ..StreamOptions::default()
        };
        assert_eq!(cap(&small), 1_000);
    }

    #[test]
    fn reasoning_effort_follows_the_models_thinking_levels() {
        let effort = |model: &Model, reasoning: Option<ThinkingLevel>| {
            let options = StreamOptions {
                reasoning,
                ..StreamOptions::default()
            };
            build_request_body(model, &Context::default(), &options).unwrap()["reasoning_effort"]
                .clone()
        };
        let mut model = model();
        assert_eq!(effort(&model, None), Value::Null);
        // An unsupported level is clamped up first.
        model.thinking_levels.insert(ThinkingLevel::Minimal, None);
        assert_eq!(effort(&model, Some(ThinkingLevel::Minimal)), "low");
        // Off goes out as the value the model maps it to.
        model
            .thinking_levels
            .insert(ThinkingLevel::Off, Some("none".into()));
        assert_eq!(effort(&model, None), "none");
        assert_eq!(effort(&model, Some(ThinkingLevel::Off)), "none");
        model.reasoning = false;
        assert_eq!(effort(&model, None), Value::Null);
    }

    #[tokio::test]
    async fn http_errors_become_error_responses() {
        let transport = transport(
            429,
            vec!["{\"error\":{\"message\":\"Rate limit reached\"}}"],
        );
        let provider = OpenAiCompletions::new(transport);
        let message = provider
            .stream(&model(), Context::default(), StreamOptions::default())
            .result()
            .await;
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(
            message.error_message.as_deref(),
            Some("429: {\"message\":\"Rate limit reached\"}")
        );
    }

    async fn http_error(status: u16, body: &'static str) -> AssistantMessage {
        OpenAiCompletions::new(transport(status, vec![body]))
            .stream(&model(), Context::default(), StreamOptions::default())
            .result()
            .await
    }

    #[tokio::test]
    async fn error_messages_keep_the_whole_error_as_pis_do() {
        let error = |message: AssistantMessage| message.error_message.unwrap_or_default();
        // OpenRouter wraps an upstream overflow; its raw error follows the object.
        let raw = "{\"error\":{\"message\":\"This endpoint's maximum context length is 131072 tokens. However, you requested about 140000 tokens\"}}";
        let wrapped = http_error(
            400,
            "{\"error\":{\"message\":\"Provider returned error\",\"code\":400,\"metadata\":{\"raw\":\"{\\\"error\\\":{\\\"message\\\":\\\"This endpoint's maximum context length is 131072 tokens. However, you requested about 140000 tokens\\\"}}\",\"provider_name\":\"DeepInfra\"}}}",
        )
        .await;
        assert!(is_context_overflow(&wrapped, None));
        let text = error(wrapped);
        assert!(
            text.starts_with("400: {\"message\":\"Provider returned error\",\"code\":400"),
            "{text}"
        );
        assert!(text.ends_with(&format!("\n{raw}")), "{text}");

        // A body without an `error` field is the error object itself.
        assert_eq!(
            error(http_error(400, "{\"object\":\"error\",\"message\":\"bad request\"}").await),
            "400: {\"object\":\"error\",\"message\":\"bad request\"}"
        );
        // Without a message, the object is the message.
        assert_eq!(
            error(http_error(400, "{\"error\":{\"code\":\"invalid\"}}").await),
            "400 {\"code\":\"invalid\"}"
        );
        assert_eq!(
            error(http_error(403, "{\"error\":\"blocked by gateway\"}").await),
            "403 \"blocked by gateway\""
        );
        assert_eq!(
            error(http_error(502, "Bad gateway").await),
            "502 Bad gateway"
        );
        assert_eq!(
            error(http_error(413, "").await),
            "413 status code (no body)"
        );
    }

    #[tokio::test]
    async fn stream_errors_carry_the_upstream_error_and_a_null_error_is_none() {
        let failed = reply_to(vec![
            "data: {\"error\":{\"message\":\"Provider returned error\",\"metadata\":{\"raw\":\"upstream overloaded\"}}}\n\n",
        ])
        .await;
        assert_eq!(
            failed.error_message.as_deref(),
            Some("Provider returned error\nupstream overloaded")
        );

        let fine = reply_to(vec![
            "data: {\"error\":null,\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n",
        ])
        .await;
        assert_eq!(fine.stop_reason, StopReason::Stop);
        assert_eq!(fine.text(), "ok");
    }

    #[tokio::test]
    async fn a_stream_without_done_still_finishes() {
        let transport = transport(
            200,
            vec![
                "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"length\"}]}\n",
            ],
        );
        let message = OpenAiCompletions::new(transport)
            .stream(&model(), Context::default(), StreamOptions::default())
            .result()
            .await;
        assert_eq!(message.stop_reason, StopReason::Length);
        assert_eq!(message.text(), "ok");
    }

    #[tokio::test]
    async fn a_body_that_ends_without_a_finish_reason_is_a_retryable_error() {
        // Cut off in the middle of a tool call: its arguments must not run.
        let cut_off = transport(
            200,
            vec![
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"write\",\"arguments\":\"{\\\"path\\\":\\\"a\"}}]}}]}\n\n",
            ],
        );
        let message = OpenAiCompletions::new(cut_off)
            .stream(&model(), Context::default(), StreamOptions::default())
            .result()
            .await;
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(message.error_message.as_deref(), Some(STREAM_CUT_OFF));
        assert!(is_retryable_error(&message));

        // A success status with an error object instead of events.
        let error_body = transport(200, vec!["{\"error\":{\"message\":\"Upstream gave up\"}}"]);
        let message = OpenAiCompletions::new(error_body)
            .stream(&model(), Context::default(), StreamOptions::default())
            .result()
            .await;
        assert_eq!(
            message.error_message.as_deref(),
            Some("Stream ended without finish_reason: Upstream gave up")
        );
    }

    #[tokio::test]
    async fn events_are_read_as_the_sdk_reads_them() {
        // Data over two lines joins, `\r` and `\r\n` end lines, also across chunks and an
        // empty one, and the last event needs no blank line after it.
        let joined = reply_to(vec![
            "data: {\"choices\":[{\"delta\":\r\ndata: {\"content\":\"o\"}}]}\r\n\r\n",
            "event: message\rdata: {\"choices\":[{\"delta\":\r",
            "",
            "\ndata: {\"content\":\"k\"}}]}\r\n\r\n: comment\nid: 7\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}",
        ])
        .await;
        assert_eq!(joined.stop_reason, StopReason::Stop);
        assert_eq!(joined.text(), "ok");

        let malformed = reply_to(vec![
            "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
            "data: {\"choices\":[\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        ])
        .await;
        assert_eq!(malformed.stop_reason, StopReason::Error);
        assert_eq!(malformed.error_message.as_deref(), Some(MALFORMED_EVENT));

        let error_event = reply_to(vec![
            "event: error\ndata: {\"message\":\"Overloaded\",\"metadata\":{\"raw\":\"try later\"}}\n\n",
        ])
        .await;
        assert_eq!(
            error_event.error_message.as_deref(),
            Some("Overloaded\ntry later")
        );
        let wrapped = reply_to(vec!["event: error\ndata: {\"error\":{\"code\":503}}\n\n"]).await;
        assert_eq!(wrapped.error_message.as_deref(), Some("{\"code\":503}"));
    }

    async fn reply_to(chunks: Vec<&'static str>) -> AssistantMessage {
        OpenAiCompletions::new(transport(200, chunks))
            .stream(&model(), Context::default(), StreamOptions::default())
            .result()
            .await
    }

    #[tokio::test]
    async fn usage_reads_each_servers_cache_fields() {
        let mut model = model();
        model.cost.cache_read = 0.5;
        model.cost.cache_write = 4.0;
        let usage_of = |chunk: &'static str| {
            let transport = transport(200, vec![chunk]);
            let model = model.clone();
            async move {
                OpenAiCompletions::new(transport)
                    .stream(&model, Context::default(), StreamOptions::default())
                    .result()
                    .await
                    .usage
            }
        };
        let deepseek = usage_of(
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":10,\"prompt_cache_hit_tokens\":60,\"prompt_cache_miss_tokens\":40}}\n\n",
        )
        .await;
        assert_eq!((deepseek.input, deepseek.cache_read), (40, 60));
        let kimi = usage_of(
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":10,\"cached_tokens\":30}}\n\n",
        )
        .await;
        assert_eq!((kimi.input, kimi.cache_read), (70, 30));
        let openrouter = usage_of(
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":10,\"prompt_tokens_details\":{\"cached_tokens\":20,\"cache_write_tokens\":50}}}\n\n",
        )
        .await;
        assert_eq!(
            (
                openrouter.input,
                openrouter.cache_read,
                openrouter.cache_write,
                openrouter.total_tokens
            ),
            (30, 20, 50, 110)
        );
        assert_eq!(openrouter.cost.cache_write, 4.0 * 50.0 / 1_000_000.0);
        let moonshot = usage_of(
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\",\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":3}}]}\n\n",
        )
        .await;
        assert_eq!((moonshot.input, moonshot.output), (12, 3));
    }

    #[tokio::test]
    async fn done_without_a_finish_reason_is_a_retryable_error() {
        let message = reply_to(vec![
            "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
            "data: [DONE]\n\n",
        ])
        .await;
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(message.error_message.as_deref(), Some(STREAM_CUT_OFF));
        assert_eq!(message.text(), "partial");
        assert!(is_retryable_error(&message));
    }

    #[tokio::test]
    async fn finish_reasons_map_as_pis_and_unknown_ones_are_errors() {
        let text = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n";
        let call = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"read\",\"arguments\":\"{}\"}}]}}]}\n\n";
        for (chunks, expected) in [
            (
                vec![
                    text,
                    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"end\"}]}\n\n",
                ],
                StopReason::Stop,
            ),
            // The reason decides, not the content.
            (
                vec![
                    call,
                    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                ],
                StopReason::Stop,
            ),
            (
                vec![
                    text,
                    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"function_call\"}]}\n\n",
                ],
                StopReason::ToolUse,
            ),
        ] {
            assert_eq!(reply_to(chunks).await.stop_reason, expected);
        }

        let network = reply_to(vec![
            text,
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"network_error\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n",
        ])
        .await;
        assert_eq!(network.stop_reason, StopReason::Error);
        assert_eq!(
            network.error_message.as_deref(),
            Some("Provider finish_reason: network_error")
        );
        assert!(is_retryable_error(&network));
        // A failed response keeps what was reported before it failed.
        assert_eq!(network.text(), "ok");
        assert_eq!((network.usage.input, network.usage.output), (10, 2));
        assert!(network.usage.cost.total > 0.0);

        let overflow = reply_to(vec![
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"model_context_window_exceeded\"}]}\n\n",
        ])
        .await;
        assert!(is_context_overflow(&overflow, None));

        let filtered = reply_to(vec![
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"content_filter\"}]}\n\n",
        ])
        .await;
        assert_eq!(
            filtered.error_message.as_deref(),
            Some("Provider finish_reason: content_filter")
        );
        assert!(!is_retryable_error(&filtered));
    }

    async fn tool_calls_of(chunks: Vec<&'static str>) -> Vec<(String, String, Value)> {
        let message = OpenAiCompletions::new(transport(200, chunks))
            .stream(&model(), Context::default(), StreamOptions::default())
            .result()
            .await;
        assert_eq!(
            message.stop_reason,
            StopReason::ToolUse,
            "{:?}",
            message.error_message
        );
        message
            .tool_calls()
            .map(|call| {
                (
                    call.id.clone(),
                    call.name.clone(),
                    Value::Object(call.arguments.clone()),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn tool_call_fragments_without_an_index_are_told_apart_by_id() {
        let calls = tool_calls_of(vec![
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"a\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"pa\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"function\":{\"arguments\":\"th\\\":\\\"x\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"b\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"y\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            // The last line may come without a newline.
            "data: [DONE]",
        ])
        .await;
        assert_eq!(
            calls,
            [
                ("a".into(), "read".into(), json!({ "path": "x" })),
                ("b".into(), "read".into(), json!({ "path": "y" })),
            ]
        );
    }

    #[tokio::test]
    async fn a_late_name_or_id_is_filled_in() {
        let calls = tool_calls_of(vec![
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"q\\\":\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c\",\"function\":{\"name\":\"grep\",\"arguments\":\"\\\"z\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        ])
        .await;
        assert_eq!(calls, [("c".into(), "grep".into(), json!({ "q": "z" }))]);
    }

    #[tokio::test]
    async fn reasoning_sent_in_two_fields_is_kept_once() {
        let transport = transport(
            200,
            vec![
                "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"plan\",\"reasoning\":\"plan\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            ],
        );
        let message = OpenAiCompletions::new(transport)
            .stream(&model(), Context::default(), StreamOptions::default())
            .result()
            .await;
        assert!(
            matches!(&message.content[0], AssistantContent::Thinking(t) if t.thinking == "plan")
        );
        assert_eq!(message.text(), "ok");
    }

    #[test]
    fn transcript_repair_answers_orphaned_calls_and_drops_failed_turns() {
        let model = model();
        let mut call = AssistantMessage::empty(&model);
        call.content = vec![AssistantContent::tool_call("c1", "read", json!({}))];
        call.stop_reason = StopReason::ToolUse;
        let failed = AssistantMessage::failed(&model, StopReason::Error, "boom");
        let orphan_result = ToolResultMessage {
            tool_call_id: "gone".into(),
            tool_name: "read".into(),
            content: vec![Content::text("x")],
            details: None,
            usage: None,
            is_error: false,
            timestamp: 0,
        };
        let context = Context::from_messages(vec![
            Message::User(UserMessage::text("a")),
            Message::Assistant(call),
            Message::System(SystemMessage {
                content: "later".into(),
                ..SystemMessage::default()
            }),
            Message::User(UserMessage::text("b")),
            Message::Assistant(failed),
            Message::ToolResult(orphan_result),
        ]);
        let mut model = model;
        model.compat.supports_mid_conversation_system_messages = true;
        let messages = convert_messages(&model, &context);
        let roles: Vec<&str> = messages
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["user", "assistant", "tool", "system", "user"]);
        assert_eq!(messages[2]["content"], "No result provided");
        assert_eq!(messages[3]["content"], "later");
    }

    #[test]
    fn tool_result_images_follow_in_a_user_message() {
        let model = model();
        let mut call = AssistantMessage::empty(&model);
        call.content = vec![AssistantContent::tool_call("c1", "screenshot", json!({}))];
        let result = ToolResultMessage {
            tool_call_id: "c1".into(),
            tool_name: "screenshot".into(),
            content: vec![Content::image("AAAA", "image/png")],
            details: None,
            usage: None,
            is_error: false,
            timestamp: 0,
        };
        let context = Context::from_messages(vec![
            Message::User(UserMessage::text("look")),
            Message::Assistant(call),
            Message::ToolResult(result),
        ]);
        let messages = convert_messages(&model, &context);
        assert_eq!(messages[2]["content"], "(see attached image)");
        assert_eq!(messages[3]["role"], "user");
        assert_eq!(
            messages[3]["content"][1]["image_url"]["url"],
            "data:image/png;base64,AAAA"
        );
    }

    #[tokio::test]
    async fn reasoning_keeps_the_field_it_came_in() {
        for (field, chunk) in [
            (
                "reasoning_content",
                "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think\"}}]}\n\n",
            ),
            (
                "reasoning",
                "data: {\"choices\":[{\"delta\":{\"reasoning\":\"think\"}}]}\n\n",
            ),
        ] {
            let transport = transport(
                200,
                vec![
                    chunk,
                    "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n",
                    "data: [DONE]\n\n",
                ],
            );
            let provider = OpenAiCompletions::new(transport);
            let context = Context::new("sys", vec![], vec![Message::User(UserMessage::text("hi"))]);
            let options = StreamOptions {
                api_key: Some("key".into()),
                ..StreamOptions::default()
            };
            let message = provider.stream(&model(), context, options).result().await;
            assert!(
                matches!(
                    &message.content[0],
                    AssistantContent::Thinking(t) if t.thinking_signature.as_deref() == Some(field)
                ),
                "{field}: {:?}",
                message.content
            );
        }
    }

    fn thinking(text: &str, field: Option<&str>) -> AssistantContent {
        AssistantContent::Thinking(ThinkingContent {
            thinking: text.into(),
            thinking_signature: field.map(Into::into),
            redacted: false,
        })
    }

    #[test]
    fn a_model_gets_its_own_reasoning_back_in_its_field() {
        let model = model();
        let mut answer = AssistantMessage::empty(&model);
        answer.content = vec![
            thinking("plan", Some("reasoning")),
            AssistantContent::text("answer"),
            AssistantContent::tool_call("c1", "read", json!({})),
        ];
        let mut unsigned = AssistantMessage::empty(&model);
        unsigned.content = vec![thinking("old", None), AssistantContent::text("done")];
        let context = Context::from_messages(vec![
            Message::User(UserMessage::text("a")),
            Message::Assistant(answer),
            Message::User(UserMessage::text("b")),
            Message::Assistant(unsigned),
        ]);
        let messages = convert_messages(&model, &context);
        assert_eq!(messages[1]["reasoning"], "plan");
        assert_eq!(messages[1]["content"], "answer");
        assert!(messages[1].get("reasoning_content").is_none());
        // Reasoning whose field is unknown is not sent, and its text stays out of the answer.
        let last = messages.last().unwrap();
        assert_eq!(last["content"], "done");
        assert!(
            REASONING_FIELDS
                .iter()
                .all(|field| last.get(*field).is_none())
        );
    }

    #[test]
    fn another_models_reasoning_is_dropped() {
        let model = model();
        let mut earlier = AssistantMessage::empty(&model);
        earlier.model = "other-model".into();
        earlier.content = vec![
            thinking("Candidates: proxy, IPv6, TLS.", Some("reasoning_content")),
            AssistantContent::text("Let me check the TLS versions."),
            AssistantContent::tool_call("c1", "bash", json!({ "command": "curl -v" })),
        ];
        let mut thought_only = AssistantMessage::empty(&model);
        thought_only.model = "other-model".into();
        thought_only.content = vec![thinking("nothing to say", Some("reasoning"))];
        let context = Context::from_messages(vec![
            Message::User(UserMessage::text("a")),
            Message::Assistant(earlier),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "c1".into(),
                tool_name: "bash".into(),
                content: vec![Content::text("TLSv1.2")],
                details: None,
                usage: None,
                is_error: false,
                timestamp: 0,
            }),
            Message::User(UserMessage::text("b")),
            Message::Assistant(thought_only),
            Message::User(UserMessage::text("c")),
        ]);
        let messages = convert_messages(&model, &context);
        // The text and the tool call stay; the reasoning goes in no field and not as text.
        assert_eq!(messages[1]["content"], "Let me check the TLS versions.");
        assert_eq!(messages[1]["tool_calls"][0]["id"], "c1");
        assert!(
            REASONING_FIELDS
                .iter()
                .all(|field| messages[1].get(*field).is_none())
        );
        // A reply that was only reasoning leaves nothing to send.
        let roles: Vec<&str> = messages
            .iter()
            .map(|message| message["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["user", "assistant", "tool", "user", "user"]);
    }

    #[test]
    fn only_the_target_models_reasoning_comes_back_when_models_alternate() {
        let model = model();
        let reply = |id: &str, thought: &str, text: &str| {
            let mut reply = AssistantMessage::empty(&model);
            reply.model = id.into();
            reply.content = vec![
                thinking(thought, Some("reasoning_content")),
                AssistantContent::text(text),
            ];
            Message::Assistant(reply)
        };
        let context = Context::from_messages(vec![
            Message::User(UserMessage::text("1")),
            reply(&model.id, "mine 1", "a"),
            Message::User(UserMessage::text("2")),
            reply("other-model", "theirs 1", "b"),
            Message::User(UserMessage::text("3")),
            reply(&model.id, "mine 2", "c"),
            Message::User(UserMessage::text("4")),
            reply("other-model", "theirs 2", "d"),
            Message::User(UserMessage::text("hi")),
        ]);
        let replies: Vec<(Value, Option<Value>)> = convert_messages(&model, &context)
            .into_iter()
            .filter(|message| message["role"] == "assistant")
            .map(|message| {
                (
                    message["content"].clone(),
                    message.get("reasoning_content").cloned(),
                )
            })
            .collect();
        assert_eq!(
            replies,
            [
                (json!("a"), Some(json!("mine 1"))),
                (json!("b"), None),
                (json!("c"), Some(json!("mine 2"))),
                (json!("d"), None),
            ]
        );
    }

    #[test]
    fn the_developer_role_is_for_reasoning_models() {
        let mut model = model();
        model.compat.supports_developer_role = true;
        let context = Context::new("sys", vec![], vec![Message::User(UserMessage::text("hi"))]);
        assert_eq!(convert_messages(&model, &context)[0]["role"], "developer");
        model.reasoning = false;
        assert_eq!(convert_messages(&model, &context)[0]["role"], "system");
    }

    fn tool_exchange(results: Vec<Vec<Content>>) -> Vec<Message> {
        let model = model();
        let mut call = AssistantMessage::empty(&model);
        call.content = (0..results.len())
            .map(|index| AssistantContent::tool_call(format!("c{index}"), "read", json!({})))
            .collect();
        let mut messages = vec![Message::User(UserMessage::text("look")), call.into()];
        for (index, content) in results.into_iter().enumerate() {
            messages.push(Message::ToolResult(ToolResultMessage {
                tool_call_id: format!("c{index}"),
                tool_name: "read".into(),
                content,
                details: None,
                usage: None,
                is_error: false,
                timestamp: 0,
            }));
        }
        messages
    }

    #[test]
    fn a_model_without_image_input_gets_placeholders_where_the_images_were() {
        let mut model = model();
        model.input = vec![crate::types::InputModality::Text];
        let image = || Content::image("AAAA", "image/png");
        let mut messages = vec![Message::User(UserMessage {
            content: vec![
                Content::text("compare"),
                image(),
                image(),
                Content::text("and"),
                image(),
            ],
            timestamp: 0,
        })];
        messages.extend(tool_exchange(vec![
            vec![Content::text("shot"), image()],
            vec![image()],
        ]));
        let messages = convert_messages(&model, &Context::from_messages(messages));
        let user = "(image omitted: model does not support images)";
        assert_eq!(
            messages[0]["content"],
            json!([
                { "type": "text", "text": "compare" },
                { "type": "text", "text": user },
                { "type": "text", "text": "and" },
                { "type": "text", "text": user },
            ])
        );
        let tool = "(tool image omitted: model does not support images)";
        assert_eq!(messages[3]["content"], format!("shot\n{tool}"));
        assert_eq!(messages[4]["content"], tool);
        // Nothing is attached after the results.
        assert_eq!(messages.len(), 5);
    }

    #[test]
    fn blank_text_is_left_out() {
        let model = model();
        let mut reply = AssistantMessage::empty(&model);
        reply.content = vec![
            AssistantContent::text("a"),
            AssistantContent::text(" \n"),
            AssistantContent::text("b"),
        ];
        let mut messages = vec![
            Message::User(UserMessage {
                content: vec![Content::text(""), Content::image("AAAA", "image/png")],
                timestamp: 0,
            }),
            reply.into(),
            Message::User(UserMessage::text("")),
        ];
        messages.extend(tool_exchange(vec![vec![Content::text("")]]));
        let messages = convert_messages(&model, &Context::from_messages(messages));
        assert_eq!(
            messages[0]["content"],
            json!([{ "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } }])
        );
        assert_eq!(messages[1]["content"], "ab");
        // A user message with nothing in it is not sent.
        assert_eq!(messages[2]["content"][0]["text"], "look");
        assert_eq!(messages[4]["content"], "(no tool output)");
    }

    #[test]
    fn utf8_split_across_chunks_is_reassembled() {
        let mut decoder = Utf8Decoder::default();
        let bytes = "é".as_bytes();
        assert_eq!(decoder.decode(&bytes[..1]), "");
        assert_eq!(decoder.decode(&bytes[1..]), "é");
    }
}
