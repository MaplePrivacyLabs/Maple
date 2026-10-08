use super::common::*;
use pi_ai::api::openai_completions::{CompletionsRequest, ScriptedTransport, stream_simple};
use pi_ai::types::*;
use pi_ai::utils::transcript::normalize_context;
use serde_json::{Value, json};
use std::sync::Arc;

fn create_model() -> Model {
    let mut model = fixture_model("openai", "gpt-4o-mini");
    model.compat = None;
    model.api = "openai-completions".into();
    model
}

fn context(content: &str, tools: Option<Value>) -> Context {
    let mut value = json!({"messages":[{"role":"user","content":content,"timestamp":0}]});
    if let Some(tools) = tools {
        value["tools"] = tools;
    }
    json(value)
}

async fn capture_request(model: Model, context: Context, options: Value) -> CompletionsRequest {
    let transport = Arc::new(ScriptedTransport::default());
    transport.push_response(ProviderResponse { status:200, ..Default::default() }, [Ok(js(json!({
        "choices":[{"delta":{},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":1,"completion_tokens":1,"prompt_tokens_details":{"cached_tokens":0},"completion_tokens_details":{"reasoning_tokens":0}}
    })))]);
    let mut all_options = json!({"cacheRetentionEnv":"MAPLE_CACHE_RETENTION","env":{"MAPLE_CACHE_RETENTION":"short"}});
    all_options
        .as_object_mut()
        .unwrap()
        .extend(options.as_object().unwrap().clone());
    let result = stream_simple(
        model,
        normalize_context(context),
        Some(json(all_options)),
        transport.clone(),
        env(),
    )
    .unwrap()
    .result()
    .await;
    assert_eq!(
        result.stop_reason,
        StopReason::Stop,
        "{:?}",
        result.error_message
    );
    let requests = transport.requests();
    assert_eq!(requests.len(), 1);
    requests.into_iter().next().unwrap()
}

fn payload(request: &CompletionsRequest) -> Value {
    serde_json::from_str(&pi_ai::utils::js_json::stringify_serializable(&request.params).unwrap())
        .unwrap()
}

// Regression for https://github.com/earendil-works/pi/issues/3649.
#[tokio::test(flavor = "current_thread")]
async fn omits_tools_field_when_context_tools_is_an_empty_array() {
    let request = capture_request(
        create_model(),
        context("hi", Some(json!([]))),
        json!({"apiKey":"test"}),
    )
    .await;
    assert!(payload(&request).get("tools").is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn omits_tools_field_when_context_tools_is_undefined() {
    let request = capture_request(
        create_model(),
        context("hi", None),
        json!({"apiKey":"test"}),
    )
    .await;
    assert!(payload(&request).get("tools").is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn sends_default_max_tokens() {
    let model = create_model();
    let max = model.max_tokens;
    let request = capture_request(model, context("hi", None), json!({"apiKey":"test"})).await;
    let params = payload(&request);
    assert!(params.get("max_tokens").is_none());
    assert_eq!(params["max_completion_tokens"], max);
}

#[tokio::test(flavor = "current_thread")]
async fn sends_explicit_max_tokens() {
    let request = capture_request(
        create_model(),
        context("hi", None),
        json!({"apiKey":"test","maxTokens":1234}),
    )
    .await;
    let params = payload(&request);
    assert!(params.get("max_tokens").is_none());
    assert_eq!(params["max_completion_tokens"], 1234);
}

#[tokio::test(flavor = "current_thread")]
async fn clamps_default_max_tokens_to_remaining_context() {
    let mut model = create_model();
    model.context_window = 10000.0;
    model.max_tokens = 8000.0;
    let request = capture_request(
        model,
        context(&"x".repeat(8000), None),
        json!({"apiKey":"test"}),
    )
    .await;
    let params = payload(&request);
    assert!(params.get("max_tokens").is_none());
    assert_eq!(params["max_completion_tokens"], 3904);
}

#[tokio::test(flavor = "current_thread")]
async fn clamps_explicit_max_tokens_to_remaining_context() {
    let mut model = create_model();
    model.context_window = 10000.0;
    model.max_tokens = 8000.0;
    let request = capture_request(
        model,
        context(&"x".repeat(8000), None),
        json!({"apiKey":"test","maxTokens":7000}),
    )
    .await;
    let params = payload(&request);
    assert!(params.get("max_tokens").is_none());
    assert_eq!(params["max_completion_tokens"], 3904);
}

// Authentication and endpoint resolution are host responsibilities. These three
// mixed cases retain the request-shape assertions using explicit resolved inputs,
// as prescribed by coverage/upstream-map.toml; they do not test credential lookup.
fn resolved_gateway_model(id: &str) -> Model {
    let mut model = fixture_model("cloudflare-ai-gateway", id);
    model.base_url = "https://gateway.ai.cloudflare.com/v1/account-id/gateway-id/compat".into();
    model
        .headers
        .get_or_insert_default()
        .insert("cf-aig-authorization".into(), "Bearer cf-token".into());
    model
}

#[tokio::test(flavor = "current_thread")]
async fn uses_conservative_open_ai_compatible_fields_for_cloudflare_ai_gateway_compat_models() {
    let model = resolved_gateway_model("workers-ai/@cf/moonshotai/kimi-k2.6");
    let mut context = context("hi", None);
    context.system_prompt = Some("You are helpful.".into());
    let request = capture_request(
        model,
        context,
        json!({"maxTokens":1234,"reasoning":"high","headers":{"Authorization":null}}),
    )
    .await;
    let params = payload(&request);
    assert_eq!(params["messages"][0]["role"], "system");
    assert_eq!(params["max_tokens"], 1234);
    assert!(params.get("max_completion_tokens").is_none());
    assert!(params.get("reasoning_effort").is_none());
    assert!(params.get("store").is_none());
    assert_eq!(
        request.model.base_url,
        "https://gateway.ai.cloudflare.com/v1/account-id/gateway-id/compat"
    );
    assert_eq!(request.headers.get("Authorization"), Some(&None));
    assert_eq!(
        request.headers.get("cf-aig-authorization"),
        Some(&Some("Bearer cf-token".into()))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn preserves_inline_upstream_authorization_for_cloudflare_ai_gateway_byok_requests() {
    let model = resolved_gateway_model("gpt-5.1");
    let request = capture_request(
        model,
        context("hi", None),
        json!({"headers":{"Authorization":"Bearer upstream-token"}}),
    )
    .await;
    assert_eq!(
        request.headers.get("Authorization"),
        Some(&Some("Bearer upstream-token".into()))
    );
    assert_eq!(
        request.headers.get("cf-aig-authorization"),
        Some(&Some("Bearer cf-token".into()))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn sends_session_affinity_headers_for_workers_ai_through_cloudflare_ai_gateway() {
    let model = resolved_gateway_model("workers-ai/@cf/moonshotai/kimi-k2.6");
    let request =
        capture_request(model, context("hi", None), json!({"sessionId":"session-1"})).await;
    assert_eq!(
        request.headers.get("session_id"),
        Some(&Some("session-1".into()))
    );
    assert_eq!(
        request.headers.get("x-client-request-id"),
        Some(&Some("session-1".into()))
    );
    assert_eq!(
        request.headers.get("x-session-affinity"),
        Some(&Some("session-1".into()))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn still_emits_tools_for_anthropic_litellm_proxy_when_conversation_has_tool_history() {
    let context = json(json!({"messages":[
        {"role":"user","content":"use the tool","timestamp":0},
        {"role":"assistant","content":[{"type":"toolCall","id":"t1","name":"noop","arguments":{}}],
         "stopReason":"toolUse","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},
         "api":"openai-completions","provider":"openai","model":"gpt-4o-mini","timestamp":0},
        {"role":"toolResult","toolCallId":"t1","toolName":"noop","content":[{"type":"text","text":"done"}],"isError":false,"timestamp":0}
    ],"tools":[]}));
    let request = capture_request(create_model(), context, json!({"apiKey":"test"})).await;
    let params = payload(&request);
    assert!(params["tools"].is_array());
    assert_eq!(params["tools"], json!([]));
}
