//! The OpenAI-compatible Chat Completions API (`api: "openai-completions"`).
//!
//! The provider builds the request from the transcript, sends it through an
//! [`HttpTransport`] and turns the server-sent events into an assistant stream. The
//! transport is the host's: a plain HTTP client, an in-process proxy or a test double.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::provider::{StreamFn, StreamOptions};
use crate::stream::{AssistantMessageBuilder, AssistantMessageStream};
use crate::transcript::{
    collapse_system_messages, current_tools, render_system_message_update, system_message_text,
};
use crate::types::{
    Content, Context, MaxTokensField, Message, Model, StopReason, ThinkingLevel, ToolResultMessage,
    Usage,
};

pub const API: &str = "openai-completions";

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
    pub body: BoxStream<'static, Result<Bytes, String>>,
}

/// Sends provider requests. Return `Err` only for transport failures; HTTP error
/// statuses are responses.
#[async_trait]
pub trait HttpTransport: Send + Sync {
    async fn post(&self, request: HttpRequest) -> Result<HttpResponse, String>;
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
    options: StreamOptions,
    builder: AssistantMessageBuilder,
) {
    let cancel = options.cancel.clone();
    let mut body = build_request_body(model, &context, &options);
    if let Some(hook) = &options.on_payload {
        body = hook(body).await;
    }
    let request = HttpRequest {
        url: format!("{}/chat/completions", model.base_url.trim_end_matches('/')),
        headers: request_headers(&options),
        body: serde_json::to_vec(&body).unwrap_or_default(),
    };
    let response = tokio::select! {
        _ = cancel.cancelled() => {
            builder.fail(StopReason::Aborted, ABORTED);
            return;
        }
        response = transport.post(request) => response,
    };
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            builder.fail(StopReason::Error, error);
            return;
        }
    };
    if !(200..300).contains(&response.status) {
        let detail = tokio::select! {
            _ = cancel.cancelled() => {
                builder.fail(StopReason::Aborted, ABORTED);
                return;
            }
            detail = read_error_body(response.body) => detail,
        };
        let error = format!("{} {}", response.status, detail);
        builder.fail(StopReason::Error, error.trim());
        return;
    }
    read_events(model, response.body, builder, &cancel).await;
}

fn request_headers(options: &StreamOptions) -> Vec<(String, String)> {
    let mut headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".to_string(), "text/event-stream".to_string()),
    ];
    if let Some(key) = &options.api_key {
        headers.push(("authorization".to_string(), format!("Bearer {key}")));
    }
    for (name, value) in &options.headers {
        headers.retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
        headers.push((name.clone(), value.clone()));
    }
    headers
}

async fn read_error_body(mut body: BoxStream<'static, Result<Bytes, String>>) -> String {
    const LIMIT: usize = 4_096;
    let mut bytes = Vec::new();
    while let Some(Ok(chunk)) = body.next().await {
        bytes.extend_from_slice(&chunk);
        if bytes.len() >= LIMIT {
            bytes.truncate(LIMIT);
            break;
        }
    }
    let text = String::from_utf8_lossy(&bytes).trim().to_string();
    // Prefer the provider's own message when the body is the usual error object.
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .or_else(|| value.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or(text)
}

/// Build the Chat Completions request body for `context`.
pub fn build_request_body(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    let compat = &model.compat;
    let mut body = Map::new();
    body.insert("model".into(), json!(model.id));
    body.insert(
        "messages".into(),
        Value::Array(convert_messages(model, context)),
    );
    body.insert("stream".into(), json!(true));
    if compat.supports_usage_in_streaming {
        body.insert("stream_options".into(), json!({ "include_usage": true }));
    }
    let max_tokens = options.max_tokens.unwrap_or(model.max_tokens);
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
        let tools: Vec<Value> = tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    },
                })
            })
            .collect();
        body.insert("tools".into(), Value::Array(tools));
    }
    let level = options.reasoning.unwrap_or(ThinkingLevel::Off);
    if compat.supports_reasoning_effort
        && let Some(effort) = model.provider_thinking_value(level)
    {
        body.insert("reasoning_effort".into(), json!(effort));
    }
    Value::Object(body)
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
    let system_role = if model.compat.supports_developer_role {
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
            Message::User(user) => out.push(json!({
                "role": "user",
                "content": user_content(model, &user.content),
            })),
            Message::Assistant(assistant) => {
                let text = assistant.text();
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
                if !calls.is_empty() {
                    entry["tool_calls"] = Value::Array(calls);
                }
                out.push(entry);
            }
            Message::ToolResult(result) => {
                let text = crate::types::content_text(&result.content);
                let has_images = result
                    .content
                    .iter()
                    .any(|block| matches!(block, Content::Image(_)));
                if has_images && model.supports_images() {
                    images_from_tools.extend(result.content.iter().filter_map(image_part));
                }
                let text = match (text.is_empty(), has_images) {
                    (false, _) => text,
                    (true, true) => "(see attached image)".to_string(),
                    (true, false) => "(no output)".to_string(),
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

fn user_content(model: &Model, content: &[Content]) -> Value {
    if content
        .iter()
        .all(|block| matches!(block, Content::Text(_)))
    {
        return json!(crate::types::content_text(content));
    }
    let parts: Vec<Value> = content
        .iter()
        .map(|block| match block {
            Content::Text(text) => json!({ "type": "text", "text": text.text }),
            Content::Image(_) if !model.supports_images() => {
                json!({ "type": "text", "text": "(image omitted: this model does not accept images)" })
            }
            Content::Image(_) => image_part(block).unwrap_or(Value::Null),
        })
        .collect();
    Value::Array(parts)
}

/// Read the server-sent events of a streamed completion into `builder`.
async fn read_events(
    model: &Model,
    mut body: BoxStream<'static, Result<Bytes, String>>,
    mut builder: AssistantMessageBuilder,
    cancel: &CancellationToken,
) {
    let mut buffer = String::new();
    let mut decoder = Utf8Decoder::default();
    let mut tool_blocks: HashMap<u64, usize> = HashMap::new();
    let mut finish_reason: Option<String> = None;
    let mut usage: Option<Usage> = None;
    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => {
                builder.fail(StopReason::Aborted, ABORTED);
                return;
            }
            chunk = body.next() => chunk,
        };
        let chunk = match chunk {
            None => break,
            Some(Err(error)) => {
                builder.fail(StopReason::Error, error);
                return;
            }
            Some(Ok(chunk)) => chunk,
        };
        buffer.push_str(&decoder.decode(&chunk));
        while let Some(end) = buffer.find('\n') {
            let line = buffer[..end].trim_end_matches('\r').to_string();
            buffer.drain(..=end);
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data == "[DONE]" {
                return finish(model, builder, finish_reason, usage);
            }
            let Ok(event) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            if let Some(error) = event.get("error") {
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| error.to_string());
                builder.fail(StopReason::Error, message);
                return;
            }
            apply_chunk(
                &event,
                &mut builder,
                &mut tool_blocks,
                &mut finish_reason,
                &mut usage,
            );
        }
    }
    finish(model, builder, finish_reason, usage);
}

fn apply_chunk(
    event: &Value,
    builder: &mut AssistantMessageBuilder,
    tool_blocks: &mut HashMap<u64, usize>,
    finish_reason: &mut Option<String>,
    usage: &mut Option<Usage>,
) {
    if builder.partial().response_id.is_none()
        && let Some(id) = event.get("id").and_then(Value::as_str)
    {
        builder.set_response_id(id);
    }
    if let Some(reported) = event.get("usage").filter(|usage| usage.is_object()) {
        *usage = Some(parse_usage(reported));
    }
    let Some(choice) = event.pointer("/choices/0") else {
        return;
    };
    if let Some(delta) = choice.get("delta") {
        for field in ["reasoning_content", "reasoning"] {
            if let Some(text) = delta.get(field).and_then(Value::as_str) {
                builder.thinking_delta(text);
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
            let provider_index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
            let index = match tool_blocks.get(&provider_index) {
                Some(index) => *index,
                None => {
                    let id = call
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("call_{provider_index}"));
                    let name = call
                        .pointer("/function/name")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let index = builder.tool_call_start(id, name);
                    tool_blocks.insert(provider_index, index);
                    index
                }
            };
            if let Some(arguments) = call.pointer("/function/arguments").and_then(Value::as_str) {
                builder.tool_call_delta(index, arguments);
            }
        }
    }
    if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
        *finish_reason = Some(reason.to_string());
    }
}

fn parse_usage(reported: &Value) -> Usage {
    let number = |pointer: &str| {
        reported
            .pointer(pointer)
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let prompt = number("/prompt_tokens");
    let completion = number("/completion_tokens");
    let cached = number("/prompt_tokens_details/cached_tokens").min(prompt);
    let reasoning = reported
        .pointer("/completion_tokens_details/reasoning_tokens")
        .and_then(Value::as_u64);
    Usage {
        input: prompt - cached,
        output: completion,
        cache_read: cached,
        cache_write: 0,
        reasoning,
        total_tokens: prompt + completion,
        ..Usage::default()
    }
}

fn finish(
    model: &Model,
    builder: AssistantMessageBuilder,
    finish_reason: Option<String>,
    usage: Option<Usage>,
) {
    let has_tool_calls = builder.partial().tool_calls().next().is_some();
    let stop_reason = match finish_reason.as_deref() {
        Some("length") => StopReason::Length,
        Some("content_filter") => {
            builder.fail(
                StopReason::Error,
                "The provider stopped the response: content_filter",
            );
            return;
        }
        _ if has_tool_calls => StopReason::ToolUse,
        _ => StopReason::Stop,
    };
    let mut usage = usage.unwrap_or_default();
    model.apply_cost(&mut usage);
    builder.finish(stop_reason, usage);
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
    async fn post(&self, request: HttpRequest) -> Result<HttpResponse, String> {
        let mut builder = self.client.post(&request.url).body(request.body);
        for (name, value) in &request.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        let response = builder.send().await.map_err(|error| error.to_string())?;
        let status = response.status().as_u16();
        let body = response
            .bytes_stream()
            .map(|chunk| chunk.map_err(|error| error.to_string()))
            .boxed();
        Ok(HttpResponse { status, body })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AssistantContent, AssistantMessage, SystemMessage, Tool, UserMessage};
    use std::sync::Mutex;

    /// Replies with canned SSE chunks and records each request.
    struct ScriptedTransport {
        status: u16,
        chunks: Vec<&'static str>,
        requests: Mutex<Vec<HttpRequest>>,
    }

    #[async_trait]
    impl HttpTransport for ScriptedTransport {
        async fn post(&self, request: HttpRequest) -> Result<HttpResponse, String> {
            self.requests.lock().unwrap().push(request);
            let chunks: Vec<Result<Bytes, String>> = self
                .chunks
                .iter()
                .map(|chunk| Ok(Bytes::from_static(chunk.as_bytes())))
                .collect();
            Ok(HttpResponse {
                status: self.status,
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
            json!({ "role": "user", "content": "hi" })
        );
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
            Some("429 Rate limit reached")
        );
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

    #[test]
    fn utf8_split_across_chunks_is_reassembled() {
        let mut decoder = Utf8Decoder::default();
        let bytes = "é".as_bytes();
        assert_eq!(decoder.decode(&bytes[..1]), "");
        assert_eq!(decoder.decode(&bytes[1..]), "é");
    }
}
