//! A scripted provider for tests.
//!
//! Queue responses, run code that streams through the provider, then assert on the
//! requests it received. Responses stream in small chunks, so consumers see the same
//! event sequence a real provider produces, and they honor cancellation.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::estimate::estimate_message_tokens;
use crate::provider::{StreamFn, StreamOptions};
use crate::stream::{AssistantMessageBuilder, AssistantMessageStream};
use crate::types::{
    AssistantContent, AssistantMessage, Context, Message, Model, StopReason, Usage,
};

/// One scripted reply.
#[derive(Clone)]
pub enum FauxResponse {
    /// Stream this message. Its model fields are replaced by the requested model's.
    Message(Box<AssistantMessage>),
    /// Build the reply from the request.
    Respond(Arc<dyn Fn(&FauxRequest) -> AssistantMessage + Send + Sync>),
    /// Start a reply and wait until the request is cancelled.
    Hang,
}

/// A request the provider received.
#[derive(Clone, Debug)]
pub struct FauxRequest {
    pub model: Model,
    pub context: Context,
    pub options: StreamOptions,
}

#[derive(Default)]
struct FauxState {
    responses: VecDeque<FauxResponse>,
    requests: Vec<FauxRequest>,
}

#[derive(Clone)]
pub struct FauxProvider {
    state: Arc<Mutex<FauxState>>,
    model: Model,
    chunk_chars: usize,
    chunk_delay: Option<Duration>,
}

impl Default for FauxProvider {
    fn default() -> Self {
        Self::new()
    }
}

static NEXT_CALL_ID: AtomicU64 = AtomicU64::new(1);

/// A tool-call block with a fresh id.
pub fn faux_tool_call(name: &str, arguments: Value) -> AssistantContent {
    let id = format!("call_{}", NEXT_CALL_ID.fetch_add(1, Ordering::Relaxed));
    AssistantContent::tool_call(id, name, arguments)
}

/// A reply with `content`; it stops for tool use when it calls a tool.
pub fn faux_message(content: Vec<AssistantContent>) -> AssistantMessage {
    let stop_reason = if content
        .iter()
        .any(|block| matches!(block, AssistantContent::ToolCall(_)))
    {
        StopReason::ToolUse
    } else {
        StopReason::Stop
    };
    AssistantMessage {
        content,
        stop_reason,
        ..AssistantMessage::empty(&FauxProvider::default_model())
    }
}

impl FauxProvider {
    pub fn new() -> Self {
        Self::with_model(Self::default_model())
    }

    pub fn with_model(model: Model) -> Self {
        Self {
            state: Arc::default(),
            model,
            chunk_chars: 4,
            chunk_delay: None,
        }
    }

    pub fn default_model() -> Model {
        serde_json::from_value(json!({
            "id": "faux-1",
            "name": "Faux Model",
            "api": "faux",
            "provider": "faux",
            "reasoning": true,
            "input": ["text", "image"],
            "contextWindow": 128000,
            "maxTokens": 16384,
        }))
        .expect("the faux model is valid")
    }

    /// Sleep between streamed chunks, to leave room for steering or cancellation.
    pub fn with_chunk_delay(mut self, delay: Duration) -> Self {
        self.chunk_delay = Some(delay);
        self
    }

    pub fn model(&self) -> Model {
        self.model.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FauxState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn push(&self, response: FauxResponse) {
        self.lock().responses.push_back(response);
    }

    /// Queue `reply` as it is, for replies with a particular stop reason or usage.
    pub fn push_reply(&self, reply: AssistantMessage) {
        self.push(FauxResponse::Message(Box::new(reply)));
    }

    pub fn push_message(&self, content: Vec<AssistantContent>) {
        self.push_reply(faux_message(content));
    }

    pub fn push_text(&self, text: &str) {
        self.push_message(vec![AssistantContent::text(text)]);
    }

    pub fn push_tool_call(&self, name: &str, arguments: Value) {
        self.push_message(vec![faux_tool_call(name, arguments)]);
    }

    pub fn push_error(&self, error: &str) {
        self.push_reply(AssistantMessage::failed(
            &self.model,
            StopReason::Error,
            error,
        ));
    }

    pub fn push_fn(
        &self,
        respond: impl Fn(&FauxRequest) -> AssistantMessage + Send + Sync + 'static,
    ) {
        self.push(FauxResponse::Respond(Arc::new(respond)));
    }

    pub fn push_hang(&self) {
        self.push(FauxResponse::Hang);
    }

    pub fn requests(&self) -> Vec<FauxRequest> {
        self.lock().requests.clone()
    }

    /// Replies still queued.
    pub fn pending(&self) -> usize {
        self.lock().responses.len()
    }
}

impl StreamFn for FauxProvider {
    fn stream(
        &self,
        model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> AssistantMessageStream {
        let (sender, stream) = AssistantMessageStream::channel();
        let request = FauxRequest {
            model: model.clone(),
            context,
            options,
        };
        let response = {
            let mut state = self.lock();
            state.requests.push(request.clone());
            state.responses.pop_front()
        };
        let (chunk_chars, chunk_delay) = (self.chunk_chars, self.chunk_delay);
        tokio::spawn(async move {
            let builder = AssistantMessageBuilder::new(sender, &request.model);
            let cancel = request.options.cancel.clone();
            let reply = match response {
                None => {
                    builder.fail(StopReason::Error, "No faux response queued");
                    return;
                }
                Some(FauxResponse::Hang) => {
                    let mut builder = builder;
                    builder.start();
                    cancel.cancelled().await;
                    builder.fail(StopReason::Aborted, "Request was aborted");
                    return;
                }
                Some(FauxResponse::Message(message)) => *message,
                Some(FauxResponse::Respond(respond)) => respond(&request),
            };
            play(builder, reply, &request, chunk_chars, chunk_delay, &cancel).await;
        });
        stream
    }
}

fn chunks(text: &str, size: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(size.max(1))
        .map(|chunk| chunk.iter().collect())
        .collect()
}

/// Wait between chunks; false once the request is cancelled.
async fn pause(delay: Option<Duration>, cancel: &CancellationToken) -> bool {
    match delay {
        Some(delay) => {
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = cancel.cancelled() => return false,
            }
        }
        None => tokio::task::yield_now().await,
    }
    !cancel.is_cancelled()
}

async fn play(
    mut builder: AssistantMessageBuilder,
    reply: AssistantMessage,
    request: &FauxRequest,
    chunk_chars: usize,
    chunk_delay: Option<Duration>,
    cancel: &CancellationToken,
) {
    const ABORTED: &str = "Request was aborted";
    if cancel.is_cancelled() {
        builder.fail(StopReason::Aborted, ABORTED);
        return;
    }
    if reply.is_failure() && reply.content.is_empty() {
        builder.fail(reply.stop_reason, reply.error_message.unwrap_or_default());
        return;
    }
    builder.start();
    for block in &reply.content {
        let pieces = match block {
            AssistantContent::Text(text) => chunks(&text.text, chunk_chars),
            AssistantContent::Thinking(thinking) => chunks(&thinking.thinking, chunk_chars),
            AssistantContent::ToolCall(call) => {
                let arguments = serde_json::to_string(&call.arguments).unwrap_or_default();
                chunks(&arguments, chunk_chars * 4)
            }
        };
        let tool_index = match block {
            AssistantContent::ToolCall(call) => Some(builder.tool_call_start(&call.id, &call.name)),
            _ => None,
        };
        for piece in pieces {
            if !pause(chunk_delay, cancel).await {
                builder.fail(StopReason::Aborted, ABORTED);
                return;
            }
            match (block, tool_index) {
                (AssistantContent::Text(_), _) => builder.text_delta(&piece),
                (AssistantContent::Thinking(_), _) => builder.thinking_delta(&piece),
                (AssistantContent::ToolCall(_), Some(index)) => {
                    builder.tool_call_delta(index, &piece)
                }
                (AssistantContent::ToolCall(_), None) => {}
            }
        }
        if let AssistantContent::Thinking(thinking) = block
            && let Some(signature) = &thinking.thinking_signature
        {
            builder.set_thinking_signature(signature);
        }
    }
    if reply.is_failure() {
        builder.fail(reply.stop_reason, reply.error_message.unwrap_or_default());
        return;
    }
    let usage = if reply.usage == Usage::default() {
        estimated_usage(&request.context, &reply)
    } else {
        reply.usage
    };
    builder.finish(reply.stop_reason, usage);
}

fn estimated_usage(context: &Context, reply: &AssistantMessage) -> Usage {
    let input = context.messages.iter().map(estimate_message_tokens).sum();
    let output = estimate_message_tokens(&Message::Assistant(reply.clone()));
    Usage {
        input,
        output,
        total_tokens: input + output,
        ..Usage::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::AssistantMessageEvent;
    use crate::types::UserMessage;

    #[tokio::test]
    async fn replies_stream_in_order_and_requests_are_recorded() {
        let faux = FauxProvider::new();
        faux.push_text("first");
        faux.push_tool_call("read", json!({ "path": "a.txt" }));
        let model = faux.model();
        let context = Context::new(
            "be brief",
            Vec::new(),
            vec![Message::User(UserMessage::text("hi"))],
        );

        let first = faux
            .stream(&model, context.clone(), StreamOptions::default())
            .result()
            .await;
        assert_eq!(first.text(), "first");
        assert_eq!(first.stop_reason, StopReason::Stop);
        assert!(first.usage.input > 0);

        let second = faux
            .stream(&model, context, StreamOptions::default())
            .result()
            .await;
        assert_eq!(second.stop_reason, StopReason::ToolUse);
        assert_eq!(
            second.tool_calls().next().unwrap().arguments["path"],
            "a.txt"
        );

        let third = faux
            .stream(&model, Context::default(), StreamOptions::default())
            .result()
            .await;
        assert_eq!(
            third.error_message.as_deref(),
            Some("No faux response queued")
        );
        assert_eq!(faux.requests().len(), 3);
        assert_eq!(faux.pending(), 0);
    }

    #[tokio::test]
    async fn a_hanging_reply_aborts_on_cancel() {
        let faux = FauxProvider::new();
        faux.push_hang();
        let cancel = CancellationToken::new();
        let options = StreamOptions {
            cancel: cancel.clone(),
            ..StreamOptions::default()
        };
        let mut stream = faux.stream(&faux.model(), Context::default(), options);
        assert!(matches!(
            stream.next().await,
            Some(AssistantMessageEvent::Start { .. })
        ));
        cancel.cancel();
        let message = stream.result().await;
        assert_eq!(message.stop_reason, StopReason::Aborted);
    }

    #[tokio::test]
    async fn reply_functions_see_the_request() {
        let faux = FauxProvider::new();
        faux.push_fn(|request| {
            let count = request.context.messages.len();
            faux_message(vec![AssistantContent::text(format!("{count} messages"))])
        });
        let context = Context::from_messages(vec![Message::User(UserMessage::text("a"))]);
        let reply = faux
            .stream(&faux.model(), context, StreamOptions::default())
            .result()
            .await;
        assert_eq!(reply.text(), "1 messages");
    }
}
