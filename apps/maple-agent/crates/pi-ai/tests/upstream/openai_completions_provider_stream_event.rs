use super::common::*;
use pi_ai::api::openai_completions::{ScriptedTransport, stream_simple};
use pi_ai::types::{AssistantContent, Model, ProviderResponse, SimpleStreamOptions};
use pi_ai::utils::transcript::normalize_context;
use serde_json::json;
use std::sync::{Arc, Mutex};

// Regression test for #9784.
#[tokio::test(flavor = "current_thread")]
async fn exposes_provider_chunks_including_openrouter_metadata() {
    let model: Model = json(json!({
        "id": "openrouter/auto", "name": "OpenRouter Auto", "api": "openai-completions",
        "provider": "openrouter", "baseUrl": "https://openrouter.ai/api/v1",
        "reasoning": false, "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": 200_000, "maxTokens": 8192
    }));
    let first_chunk = js(json!({
        "id": "chatcmpl-1", "model": "anthropic/claude-sonnet-4.6",
        "choices": [{"index": 0, "delta": {"content": "hello"}}]
    }));
    let final_chunk = js(json!({
        "id": "chatcmpl-1", "model": "anthropic/claude-sonnet-4.6",
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12,
                  "cost": 0.0012, "is_byok": false},
        "openrouter_metadata": {"strategy": "direct", "region": "iad"}
    }));
    let transport = Arc::new(ScriptedTransport::default());
    transport.push_response(
        json::<ProviderResponse>(json!({"status": 200, "headers": {"x-request-id": "req-1"}})),
        [Ok(first_chunk.clone()), Ok(final_chunk.clone())],
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = events.clone();
    let mut options: SimpleStreamOptions = json(json!({"apiKey": "test"}));
    options.on_provider_stream_event = Some(Arc::new(move |data, _| {
        captured.lock().unwrap().push(data);
        Box::pin(async { Ok(()) })
    }));
    let message = stream_simple(
        model,
        normalize_context(json(
            json!({"messages": [{"role": "user", "content": "hi", "timestamp": 1000}]}),
        )),
        Some(options),
        transport,
        env(),
    )
    .unwrap()
    .result()
    .await;

    assert_eq!(
        message.content,
        json::<Vec<AssistantContent>>(json!([{"type": "text", "text": "hello"}]))
    );
    assert_eq!(*events.lock().unwrap(), vec![first_chunk, final_chunk]);
}
