#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use pi_agent_core::types::{AgentMessage, StreamFn};
use pi_ai::types::{AssistantMessage, AssistantMessageEvent, DoneReason, Model};
use pi_ai::utils::event_stream::create_assistant_message_event_stream;
use pi_coding_agent::core::compaction::compaction::SummaryRuntime;
use pi_testkit::VirtualEnv;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json as j};

pub fn json<T: DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).expect("pinned upstream fixture matches the Rust contract")
}

pub fn observed(value: &impl Serialize) -> Value {
    serde_json::from_str(&pi_ai::utils::js_json::stringify_serializable(value).unwrap()).unwrap()
}

pub fn env() -> Arc<VirtualEnv> {
    Arc::new(VirtualEnv::new(1_767_225_600_000))
}

pub fn usage(input: f64, output: f64, cache_read: f64, cache_write: f64) -> Value {
    observed(
        &j!({"input":input,"output":output,"cacheRead":cache_read,"cacheWrite":cache_write,
        "totalTokens":input+output+cache_read+cache_write,
        "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}}),
    )
}

pub fn user(text: &str) -> Value {
    j!({"role":"user","content":text,"timestamp":0})
}

pub fn assistant(text: &str, custom_usage: Option<Value>) -> Value {
    j!({"role":"assistant","content":[{"type":"text","text":text}],
        "usage":custom_usage.unwrap_or_else(||usage(100.0,50.0,0.0,0.0)),
        "stopReason":"stop","timestamp":0,"api":"anthropic-messages",
        "provider":"anthropic","model":"claude-sonnet-4-5"})
}

pub fn tool_result(text: &str) -> Value {
    j!({"role":"toolResult","toolCallId":"tc1","toolName":"read",
        "content":[{"type":"text","text":text}],"isError":false,"timestamp":0})
}

pub fn summary_messages() -> Vec<AgentMessage> {
    vec![json(user("Summarize this."))]
}

pub fn summary_model(reasoning: bool, max_tokens: f64, compat: Option<Value>) -> Model {
    let mut model = j!({"id":if reasoning {"reasoning-model"} else {"non-reasoning-model"},
        "name":if reasoning {"Reasoning Model"} else {"Non-reasoning Model"},
        "api":"anthropic-messages","provider":"anthropic","baseUrl":"https://api.anthropic.com",
        "reasoning":reasoning,"input":["text"],"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},
        "contextWindow":200000,"maxTokens":max_tokens});
    if let Some(compat) = compat {
        model["compat"] = compat;
    }
    json(model)
}

pub fn summary_response() -> AssistantMessage {
    json(assistant(
        "## Goal\nTest summary",
        Some(usage(10.0, 10.0, 0.0, 0.0)),
    ))
}

pub fn tool_call_response() -> AssistantMessage {
    let mut response = observed(&summary_response());
    response["content"] =
        j!([{"type":"toolCall","id":"tool-call-1","name":"read","arguments":{"path":"README.md"}}]);
    response["stopReason"] = j!("toolUse");
    json(response)
}

pub fn length_response() -> AssistantMessage {
    let mut response = observed(&summary_response());
    response["content"] = j!([{"type":"text","text":"partial"}]);
    response["stopReason"] = j!("length");
    json(response)
}

#[derive(Clone, Debug)]
pub struct RecordedCall {
    pub model: Value,
    pub context: Value,
    pub options: Value,
}

/// The selected Rust API requires StreamFn. This replaces the upstream
/// completeSimple mock while preserving its captured arguments and response.
#[derive(Clone)]
pub struct SummaryMock {
    calls: Arc<Mutex<Vec<RecordedCall>>>,
    responses: Arc<Mutex<VecDeque<(AssistantMessage, DoneReason)>>>,
    fallback: AssistantMessage,
}

impl Default for SummaryMock {
    fn default() -> Self {
        Self::new()
    }
}

impl SummaryMock {
    pub fn new() -> Self {
        Self {
            calls: Arc::default(),
            responses: Arc::default(),
            fallback: summary_response(),
        }
    }
    pub fn once(&self, message: AssistantMessage, reason: DoneReason) {
        self.responses.lock().unwrap().push_back((message, reason));
    }
    pub fn calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().unwrap().clone()
    }
    pub fn stream_fn(&self) -> StreamFn {
        let state = self.clone();
        Arc::new(move |model, context, options| {
            state.calls.lock().unwrap().push(RecordedCall {
                model: observed(&model),
                context: observed(&context),
                options: observed(&options),
            });
            let (message, reason) = state
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| (state.fallback.clone(), DoneReason::Stop));
            let stream = create_assistant_message_event_stream();
            let writer = stream.writer();
            stream.set_producer(async move {
                tokio::task::yield_now().await;
                writer.push(AssistantMessageEvent::Done { reason, message });
            });
            Box::pin(async move { Ok(stream) })
        })
    }
    pub fn runtime(&self) -> SummaryRuntime {
        SummaryRuntime::new(self.stream_fn(), env())
    }
}

/// A fresh builder per test replaces beforeEach's entryCounter/lastId reset.
#[derive(Default)]
pub struct EntryBuilder {
    next: usize,
    last: Option<String>,
}

impl EntryBuilder {
    fn entry(&mut self, mut value: Value) -> Value {
        let id = format!("test-id-{}", self.next);
        self.next += 1;
        value["id"] = j!(id);
        value["parentId"] = j!(self.last);
        value["timestamp"] = j!("2026-01-01T00:00:00.000Z");
        self.last = Some(id);
        value
    }
    pub fn message(&mut self, message: Value) -> Value {
        self.entry(j!({"type":"message","message":message}))
    }
    pub fn compaction(&mut self, summary: &str, first_kept_id: &Value) -> Value {
        self.entry(j!({"type":"compaction","summary":summary,"firstKeptEntryId":first_kept_id,"tokensBefore":10000}))
    }
    pub fn model_change(&mut self, provider: &str, model_id: &str) -> Value {
        self.entry(j!({"type":"model_change","provider":provider,"modelId":model_id}))
    }
    pub fn thinking(&mut self, level: &str) -> Value {
        self.entry(j!({"type":"thinking_level_change","thinkingLevel":level}))
    }
    pub fn custom(&mut self, content: &str) -> Value {
        self.entry(
            j!({"type":"custom_message","customType":"test","content":content,"display":true}),
        )
    }
}

pub fn extract_text(messages: &impl Serialize) -> String {
    fn content(value: &Value) -> String {
        match value {
            Value::String(text) => text.clone(),
            Value::Array(blocks) => blocks
                .iter()
                .filter(|b| b["type"] == "text")
                .map(|b| b["text"].as_str().unwrap())
                .collect::<Vec<_>>()
                .join(" "),
            _ => String::new(),
        }
    }
    observed(messages)
        .as_array()
        .unwrap()
        .iter()
        .map(|message| match message["role"].as_str().unwrap() {
            "user" | "assistant" | "custom" | "toolResult" => content(&message["content"]),
            "branchSummary" | "compactionSummary" => {
                message["summary"].as_str().unwrap().to_owned()
            }
            "bashExecution" => format!(
                "{}\n{}",
                message["command"].as_str().unwrap(),
                message["output"].as_str().unwrap()
            ),
            _ => String::new(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}
