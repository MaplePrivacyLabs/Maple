use super::common::*;
use pi_ai::api::openai_completions::{ScriptedTransport, stream};
use pi_ai::types::{Model, ProviderResponse};
use pi_ai::utils::transcript::normalize_context;
use serde_json::{Value, json as j};
use std::sync::Arc;

fn create_model(compat: Option<Value>) -> Model {
    let mut model = fixture_model("openai", "gpt-4o-mini");
    model.api = "openai-completions".into();
    model.compat = compat.map(json);
    model
}

async fn capture_request(model: Model) -> Value {
    let transport = Arc::new(ScriptedTransport::default());
    transport.push_response(ProviderResponse { status:200,..Default::default() },[Ok(js(j!({
        "choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,
        "prompt_tokens_details":{"cached_tokens":0},"completion_tokens_details":{"reasoning_tokens":0}}
    })))]);
    let ctx = normalize_context(json(
        j!({"systemPrompt":"sys","messages":[{"role":"user","content":"hi","timestamp":0}]}),
    ));
    stream(
        model,
        ctx,
        Some(json(j!({"apiKey":"test-key"}))),
        transport.clone(),
        env(),
    )
    .unwrap()
    .result()
    .await;
    let requests = transport.requests();
    assert_eq!(requests.len(), 1);
    serde_json::from_str(&pi_ai::utils::js_json::stringify(&requests[0].params)).unwrap()
}

// Retain both the upstream file and describe-block namespaces in coverage paths.
#[allow(clippy::module_inception)]
mod openai_completions_vllm_priority {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn sends_compat_vllm_priority_as_the_top_level_priority_request_field() {
        let payload = capture_request(create_model(Some(j!({"vllmPriority":10})))).await;
        assert_eq!(payload["priority"], 10);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn omits_priority_when_vllm_priority_is_not_set() {
        let payload = capture_request(create_model(None)).await;
        assert!(payload.get("priority").is_none());
    }
}
