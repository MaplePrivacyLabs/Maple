use super::common::*;
use pi_ai::api::openai_completions::{ScriptedTransport, stream};
use pi_ai::types::{AssistantMessage, Model, ProviderResponse, StopReason};
use pi_ai::utils::transcript::normalize_context;
use serde_json::{Value, json};
use std::sync::Arc;

fn open_router_auto() -> Model {
    json(json!({
        "id": "openrouter/auto", "name": "OpenRouter Auto", "api": "openai-completions",
        "provider": "openrouter", "baseUrl": "https://openrouter.ai/api/v1", "reasoning": false,
        "input": ["text"], "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": 200_000, "maxTokens": 8192
    }))
}

async fn complete(chunks: Vec<Value>) -> AssistantMessage {
    let transport = Arc::new(ScriptedTransport::default());
    transport.push_response(
        ProviderResponse {
            status: 200,
            ..Default::default()
        },
        chunks.into_iter().map(|c| Ok(js(c))),
    );
    stream(
        open_router_auto(),
        normalize_context(json(json!({
            "messages": [{"role": "user", "content": "hi", "timestamp": 1000}]
        }))),
        Some(json(json!({"apiKey": "test"}))),
        transport,
        env(),
    )
    .unwrap()
    .result()
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn surfaces_routed_chunk_model_on_response_model_without_changing_model() {
    let message = complete(vec![
        json!({"id": "chatcmpl-1", "model": "anthropic/claude-opus-4.8", "choices": [{"index": 0, "delta": {"content": "hi"}}]}),
        json!({"id": "chatcmpl-1", "model": "anthropic/claude-opus-4.8", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "prompt_tokens_details": {"cached_tokens": 0}, "completion_tokens_details": {"reasoning_tokens": 0}}}),
    ]).await;
    assert_eq!(message.model, "openrouter/auto");
    assert_eq!(
        message.response_model.as_ref().and_then(|s| s.as_str()),
        Some("anthropic/claude-opus-4.8")
    );
    assert_eq!(message.provider.as_str(), "openrouter");
    assert_eq!(message.stop_reason, StopReason::Stop);
}

#[tokio::test(flavor = "current_thread")]
async fn leaves_response_model_undefined_when_chunks_echo_the_requested_id() {
    let message = complete(vec![
        json!({"id": "chatcmpl-2", "model": "openrouter/auto", "choices": [{"index": 0, "delta": {"content": "hi"}}]}),
        json!({"id": "chatcmpl-2", "model": "openrouter/auto", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "prompt_tokens_details": {"cached_tokens": 0}, "completion_tokens_details": {"reasoning_tokens": 0}}}),
    ]).await;
    assert_eq!(message.model, "openrouter/auto");
    assert!(message.response_model.is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn ignores_empty_or_missing_chunk_model() {
    let message = complete(vec![
        json!({"id": "chatcmpl-3", "choices": [{"index": 0, "delta": {"content": "hi"}}]}),
        json!({"id": "chatcmpl-3", "model": "", "choices": [{"index": 0, "delta": {"content": "!"}}]}),
        json!({"id": "chatcmpl-3", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 2, "prompt_tokens_details": {"cached_tokens": 0}, "completion_tokens_details": {"reasoning_tokens": 0}}}),
    ]).await;
    assert_eq!(message.model, "openrouter/auto");
    assert!(message.response_model.is_none());
}
