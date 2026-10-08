use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

use futures_util::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Map;
use tokio::sync::mpsc;

use crate::json::parse_streaming_json;
use crate::types::{
    AssistantContent, AssistantMessage, Model, StopReason, TextContent, ThinkingContent, ToolCall,
    Usage, now_ms,
};

/// One step of a streamed assistant response.
///
/// A stream starts with `Start`, carries block events in content order and ends with
/// `Done` or `Error`, whose message is the authoritative result. A request that fails
/// before generation may end with `Error` alone. Block events carry deltas; apply them
/// with [`apply_event`] to follow the response as it grows.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantMessageEvent {
    Start {
        message: AssistantMessage,
    },
    TextStart {
        index: usize,
    },
    TextDelta {
        index: usize,
        delta: String,
    },
    TextEnd {
        index: usize,
    },
    ThinkingStart {
        index: usize,
    },
    ThinkingDelta {
        index: usize,
        delta: String,
    },
    ThinkingEnd {
        index: usize,
    },
    ToolCallStart {
        index: usize,
        id: String,
        name: String,
    },
    ToolCallDelta {
        index: usize,
        delta: String,
    },
    ToolCallEnd {
        index: usize,
        tool_call: ToolCall,
    },
    Done {
        message: AssistantMessage,
    },
    Error {
        message: AssistantMessage,
    },
}

impl AssistantMessageEvent {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done { .. } | Self::Error { .. })
    }
}

/// Apply a block event to a response in progress.
pub fn apply_event(partial: &mut AssistantMessage, event: &AssistantMessageEvent) {
    match event {
        AssistantMessageEvent::Start { message }
        | AssistantMessageEvent::Done { message }
        | AssistantMessageEvent::Error { message } => *partial = message.clone(),
        AssistantMessageEvent::TextStart { .. } => partial.content.push(AssistantContent::text("")),
        AssistantMessageEvent::ThinkingStart { .. } => {
            partial.content.push(AssistantContent::thinking(""))
        }
        AssistantMessageEvent::ToolCallStart { id, name, .. } => {
            partial.content.push(AssistantContent::ToolCall(ToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: Map::new(),
            }))
        }
        AssistantMessageEvent::TextDelta { index, delta } => {
            if let Some(AssistantContent::Text(text)) = partial.content.get_mut(*index) {
                text.text.push_str(delta);
            }
        }
        AssistantMessageEvent::ThinkingDelta { index, delta } => {
            if let Some(AssistantContent::Thinking(thinking)) = partial.content.get_mut(*index) {
                thinking.thinking.push_str(delta);
            }
        }
        AssistantMessageEvent::ToolCallEnd { index, tool_call } => {
            if let Some(block) = partial.content.get_mut(*index) {
                *block = AssistantContent::ToolCall(tool_call.clone());
            }
        }
        AssistantMessageEvent::TextEnd { .. }
        | AssistantMessageEvent::ThinkingEnd { .. }
        | AssistantMessageEvent::ToolCallDelta { .. } => {}
    }
}

/// The sending half of an [`AssistantMessageStream`].
pub type AssistantStreamSender = mpsc::UnboundedSender<AssistantMessageEvent>;

/// A streamed assistant response. Read events with [`AssistantMessageStream::next`] or as a
/// [`Stream`], or wait for the final message with [`AssistantMessageStream::result`].
pub struct AssistantMessageStream {
    receiver: mpsc::UnboundedReceiver<AssistantMessageEvent>,
    started: Option<AssistantMessage>,
    finished: bool,
}

impl AssistantMessageStream {
    pub fn channel() -> (AssistantStreamSender, Self) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (
            sender,
            Self {
                receiver,
                started: None,
                finished: false,
            },
        )
    }

    /// The next event. After the terminal event, or when the producer goes away without
    /// one, this returns `None`; [`Self::result`] reports the missing end as an error.
    pub async fn next(&mut self) -> Option<AssistantMessageEvent> {
        std::future::poll_fn(|cx| self.poll_event(cx)).await
    }

    fn poll_event(&mut self, cx: &mut TaskContext<'_>) -> Poll<Option<AssistantMessageEvent>> {
        if self.finished {
            return Poll::Ready(None);
        }
        let event = std::task::ready!(self.receiver.poll_recv(cx));
        match &event {
            Some(AssistantMessageEvent::Start { message }) => self.started = Some(message.clone()),
            Some(event) if event.is_terminal() => self.finished = true,
            None => self.finished = true,
            _ => {}
        }
        Poll::Ready(event)
    }

    /// Drain the stream and return the final message.
    pub async fn result(mut self) -> AssistantMessage {
        while let Some(event) = self.next().await {
            if let AssistantMessageEvent::Done { message }
            | AssistantMessageEvent::Error { message } = event
            {
                return message;
            }
        }
        self.unterminated()
    }

    /// The error a stream that ended without `Done` or `Error` stands for.
    pub fn unterminated(&self) -> AssistantMessage {
        let mut message = self.started.clone().unwrap_or_else(|| AssistantMessage {
            content: Vec::new(),
            api: String::new(),
            provider: String::new(),
            model: String::new(),
            response_id: None,
            thinking_level: None,
            usage: Usage::default(),
            stop_reason: StopReason::Error,
            error_message: None,
            timestamp: now_ms(),
        });
        message.stop_reason = StopReason::Error;
        message.error_message = Some("Provider stream ended without a final message".into());
        message
    }
}

impl Stream for AssistantMessageStream {
    type Item = AssistantMessageEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().poll_event(cx)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpenBlock {
    Text(usize),
    Thinking(usize),
    ToolCall(usize),
}

/// Builds a response and emits its events in protocol order, so providers only report
/// what arrived: text, reasoning, tool-call fragments, the stop reason and usage.
pub struct AssistantMessageBuilder {
    sender: AssistantStreamSender,
    partial: AssistantMessage,
    open: Option<OpenBlock>,
    /// Raw argument JSON for each tool-call block, by content index.
    arguments: Vec<(usize, String)>,
    started: bool,
}

impl AssistantMessageBuilder {
    pub fn new(sender: AssistantStreamSender, model: &Model) -> Self {
        Self {
            sender,
            partial: AssistantMessage::empty(model),
            open: None,
            arguments: Vec::new(),
            started: false,
        }
    }

    pub fn partial(&self) -> &AssistantMessage {
        &self.partial
    }

    pub fn set_response_id(&mut self, id: impl Into<String>) {
        self.partial.response_id = Some(id.into());
    }

    fn send(&self, event: AssistantMessageEvent) {
        // A dropped receiver means nobody is listening any more; the provider task
        // notices through its cancellation token, not through send errors.
        let _ = self.sender.send(event);
    }

    /// Emit `Start` once. Block methods call it on demand.
    pub fn start(&mut self) {
        if !self.started {
            self.started = true;
            self.send(AssistantMessageEvent::Start {
                message: self.partial.clone(),
            });
        }
    }

    pub fn text_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        let index = match self.open {
            Some(OpenBlock::Text(index)) => index,
            _ => {
                self.close_block();
                self.start();
                let index = self.partial.content.len();
                self.partial.content.push(AssistantContent::text(""));
                self.open = Some(OpenBlock::Text(index));
                self.send(AssistantMessageEvent::TextStart { index });
                index
            }
        };
        if let Some(AssistantContent::Text(TextContent { text, .. })) =
            self.partial.content.get_mut(index)
        {
            text.push_str(delta);
        }
        self.send(AssistantMessageEvent::TextDelta {
            index,
            delta: delta.to_string(),
        });
    }

    pub fn thinking_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        let index = match self.open {
            Some(OpenBlock::Thinking(index)) => index,
            _ => {
                self.close_block();
                self.start();
                let index = self.partial.content.len();
                self.partial.content.push(AssistantContent::thinking(""));
                self.open = Some(OpenBlock::Thinking(index));
                self.send(AssistantMessageEvent::ThinkingStart { index });
                index
            }
        };
        if let Some(AssistantContent::Thinking(ThinkingContent { thinking, .. })) =
            self.partial.content.get_mut(index)
        {
            thinking.push_str(delta);
        }
        self.send(AssistantMessageEvent::ThinkingDelta {
            index,
            delta: delta.to_string(),
        });
    }

    /// Open a tool-call block and return its content index.
    pub fn tool_call_start(&mut self, id: impl Into<String>, name: impl Into<String>) -> usize {
        self.close_block();
        self.start();
        let index = self.partial.content.len();
        let (id, name) = (id.into(), name.into());
        self.partial
            .content
            .push(AssistantContent::ToolCall(ToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: Map::new(),
            }));
        self.arguments.push((index, String::new()));
        self.open = Some(OpenBlock::ToolCall(index));
        self.send(AssistantMessageEvent::ToolCallStart { index, id, name });
        index
    }

    /// Set the id or name of the tool call at `index` when it arrives after the first
    /// fragment. Its `ToolCallEnd` and the final message carry them.
    pub fn set_tool_call_identity(&mut self, index: usize, id: Option<&str>, name: Option<&str>) {
        if let Some(AssistantContent::ToolCall(call)) = self.partial.content.get_mut(index) {
            if let Some(id) = id {
                call.id = id.to_string();
            }
            if let Some(name) = name {
                call.name = name.to_string();
            }
        }
    }

    /// Append argument JSON to the tool call at `index`.
    pub fn tool_call_delta(&mut self, index: usize, delta: &str) {
        if delta.is_empty() {
            return;
        }
        if let Some((_, raw)) = self.arguments.iter_mut().find(|(at, _)| *at == index) {
            raw.push_str(delta);
            self.send(AssistantMessageEvent::ToolCallDelta {
                index,
                delta: delta.to_string(),
            });
        }
    }

    fn finish_tool_call(&mut self, index: usize) -> Option<ToolCall> {
        let raw = self
            .arguments
            .iter()
            .find(|(at, _)| *at == index)
            .map(|(_, raw)| raw.clone())?;
        let AssistantContent::ToolCall(call) = self.partial.content.get_mut(index)? else {
            return None;
        };
        call.arguments = parse_streaming_json(&raw);
        Some(call.clone())
    }

    fn close_block(&mut self) {
        match self.open.take() {
            Some(OpenBlock::Text(index)) => self.send(AssistantMessageEvent::TextEnd { index }),
            Some(OpenBlock::Thinking(index)) => {
                self.send(AssistantMessageEvent::ThinkingEnd { index })
            }
            Some(OpenBlock::ToolCall(index)) => {
                if let Some(tool_call) = self.finish_tool_call(index) {
                    self.send(AssistantMessageEvent::ToolCallEnd { index, tool_call });
                }
            }
            None => {}
        }
    }

    /// Re-parse every tool call's arguments, including fragments that arrived for a block
    /// after it was closed.
    fn settle_tool_calls(&mut self) {
        let indexes: Vec<usize> = self.arguments.iter().map(|(index, _)| *index).collect();
        for index in indexes {
            self.finish_tool_call(index);
        }
    }

    /// Set the reasoning signature of the most recent thinking block.
    pub fn set_thinking_signature(&mut self, signature: impl Into<String>) {
        if let Some(AssistantContent::Thinking(thinking)) = self
            .partial
            .content
            .iter_mut()
            .rev()
            .find(|block| matches!(block, AssistantContent::Thinking(_)))
        {
            thinking.thinking_signature = Some(signature.into());
        }
    }

    /// End the response successfully and return the final message.
    pub fn finish(mut self, stop_reason: StopReason, usage: Usage) -> AssistantMessage {
        self.close_block();
        self.start();
        self.settle_tool_calls();
        self.partial.stop_reason = stop_reason;
        self.partial.usage = usage;
        let message = self.partial.clone();
        self.send(AssistantMessageEvent::Done {
            message: message.clone(),
        });
        message
    }

    /// End the response with an error or abort, keeping the content received so far.
    pub fn fail(mut self, stop_reason: StopReason, error: impl Into<String>) -> AssistantMessage {
        self.close_block();
        self.settle_tool_calls();
        self.partial.stop_reason = stop_reason;
        self.partial.error_message = Some(error.into());
        let message = self.partial.clone();
        self.send(AssistantMessageEvent::Error {
            message: message.clone(),
        });
        message
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn model() -> Model {
        serde_json::from_value(json!({
            "id": "m", "name": "M", "api": "test", "provider": "p",
            "contextWindow": 1000, "maxTokens": 100,
        }))
        .unwrap()
    }

    async fn collect(mut stream: AssistantMessageStream) -> Vec<AssistantMessageEvent> {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    }

    #[tokio::test]
    async fn builder_emits_blocks_in_protocol_order() {
        let (sender, stream) = AssistantMessageStream::channel();
        let mut builder = AssistantMessageBuilder::new(sender, &model());
        builder.thinking_delta("hmm");
        builder.text_delta("Hel");
        builder.text_delta("lo");
        let call = builder.tool_call_start("c1", "read");
        builder.tool_call_delta(call, "{\"path\":");
        builder.tool_call_delta(call, "\"a.txt\"}");
        let message = builder.finish(StopReason::ToolUse, Usage::default());

        let events = collect(stream).await;
        let kinds: Vec<_> = events
            .iter()
            .map(|event| {
                serde_json::to_value(event).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "start",
                "thinking_start",
                "thinking_delta",
                "thinking_end",
                "text_start",
                "text_delta",
                "text_delta",
                "text_end",
                "tool_call_start",
                "tool_call_delta",
                "tool_call_delta",
                "tool_call_end",
                "done"
            ]
        );
        assert_eq!(message.text(), "Hello");
        let call = message.tool_calls().next().unwrap();
        assert_eq!(call.arguments["path"], "a.txt");

        let mut partial = AssistantMessage::empty(&model());
        for event in &events[..events.len() - 1] {
            apply_event(&mut partial, event);
        }
        assert_eq!(partial.content, message.content);
    }

    #[tokio::test]
    async fn result_reports_a_stream_that_ends_without_a_final_message() {
        let (sender, stream) = AssistantMessageStream::channel();
        let mut builder = AssistantMessageBuilder::new(sender, &model());
        builder.start();
        drop(builder);
        let message = stream.result().await;
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(message.model, "m");
    }

    #[tokio::test]
    async fn failure_keeps_partial_content() {
        let (sender, stream) = AssistantMessageStream::channel();
        let mut builder = AssistantMessageBuilder::new(sender, &model());
        builder.text_delta("partial");
        builder.fail(StopReason::Aborted, "Request was aborted");
        let message = stream.result().await;
        assert_eq!(message.stop_reason, StopReason::Aborted);
        assert_eq!(message.text(), "partial");
        assert_eq!(
            message.error_message.as_deref(),
            Some("Request was aborted")
        );
    }
}
