use super::common::*;
use pi_ai::api::openai_completions::{ScriptedTransport, stream};
use pi_ai::types::{Model, ProviderResponse, StopReason};
use pi_ai::utils::transcript::normalize_context;
use serde_json::json;
use std::sync::Arc;

fn model() -> Model {
    json(json!({
        "id": "test-model", "name": "Test Model", "api": "openai-completions",
        "provider": "openai", "baseUrl": "https://api.openai.com/v1", "reasoning": false,
        "input": ["text"], "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": 128_000, "maxTokens": 4096
    }))
}

#[tokio::test(flavor = "current_thread")]
async fn preserves_raw_finish_reasons_for_successful_stops() {
    let transport = Arc::new(ScriptedTransport::default());
    transport.push_response(
        ProviderResponse {
            status: 200,
            ..Default::default()
        },
        [Ok(js(json!({
            "id": "chatcmpl-1", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
        })))],
    );
    let message = stream(
        model(),
        normalize_context(json(json!({
            "messages": [{"role": "user", "content": "hello", "timestamp": 1000}]
        }))),
        Some(json(json!({"apiKey": "test"}))),
        transport,
        env(),
    )
    .unwrap()
    .result()
    .await;

    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(
        message.raw_stop_reason.as_ref().and_then(|s| s.as_str()),
        Some("stop")
    );
    assert!(message.error_message.is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn preserves_raw_finish_reasons_for_provider_error_stops() {
    let transport = Arc::new(ScriptedTransport::default());
    transport.push_response(ProviderResponse { status: 200, ..Default::default() }, [Ok(js(json!({
        "id": "chatcmpl-2", "choices": [{"index": 0, "delta": {}, "finish_reason": "content_filter"}]
    })))]);
    let message = stream(
        model(),
        normalize_context(json(json!({
            "messages": [{"role": "user", "content": "hello", "timestamp": 1000}]
        }))),
        Some(json(json!({"apiKey": "test"}))),
        transport,
        env(),
    )
    .unwrap()
    .result()
    .await;

    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        message.raw_stop_reason.as_ref().and_then(|s| s.as_str()),
        Some("content_filter")
    );
    assert_eq!(
        message.error_message.as_ref().and_then(|s| s.as_str()),
        Some("Provider finish_reason: content_filter")
    );
}
