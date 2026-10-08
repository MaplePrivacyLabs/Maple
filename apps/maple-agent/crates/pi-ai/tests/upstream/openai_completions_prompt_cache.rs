use super::common::*;
use pi_ai::api::openai_completions::{
    OpenAICompletionsOptions, build_params, get_compat, request_headers, resolve_cache_retention,
};
use pi_ai::types::*;
use pi_ai::utils::transcript::normalize_context;
use serde_json::{Value, json};

fn create_model(overrides: Value) -> Model {
    let mut model = serde_json::to_value(fixture_model("openai", "gpt-4o-mini")).unwrap();
    model.as_object_mut().unwrap().remove("compat");
    model["api"] = json!("openai-completions");
    model
        .as_object_mut()
        .unwrap()
        .extend(overrides.as_object().unwrap().clone());
    json(model)
}

fn capture_request(options: Value, model: Option<Model>) -> (Value, Value) {
    let model = model.unwrap_or_else(|| create_model(json!({})));
    let context = normalize_context(json(
        json!({"systemPrompt":"sys","messages":[{"role":"user","content":"hi","timestamp":0}]}),
    ));
    let mut all_options = json!({"apiKey":"test-key","cacheRetentionEnv":"MAPLE_CACHE_RETENTION","env":{"MAPLE_CACHE_RETENTION":"short"}});
    all_options
        .as_object_mut()
        .unwrap()
        .extend(options.as_object().unwrap().clone());
    let options: OpenAICompletionsOptions = json(all_options);
    let payload = build_params(&model, &context, Some(&options), env().as_ref()).unwrap();
    let retention = resolve_cache_retention(
        options.cache_retention,
        options.env.as_ref(),
        options.cache_retention_env.as_deref(),
    );
    let headers = request_headers(&model, &options, &get_compat(&model), retention);
    (
        serde_json::to_value(payload).unwrap(),
        serde_json::to_value(headers).unwrap(),
    )
}

#[test]
fn sets_prompt_cache_key_for_direct_open_ai_requests_when_caching_is_enabled() {
    let (payload, _) = capture_request(json!({"sessionId":"session-123"}), None);
    assert_eq!(payload["prompt_cache_key"], "session-123");
    assert!(payload.get("prompt_cache_retention").is_none());
}

#[test]
fn sets_prompt_cache_retention_to_24h_for_direct_open_ai_requests_when_cache_retention_is_long() {
    let (payload, _) = capture_request(
        json!({"cacheRetention":"long","sessionId":"session-456"}),
        None,
    );
    assert_eq!(payload["prompt_cache_key"], "session-456");
    assert_eq!(payload["prompt_cache_retention"], "24h");
}

#[test]
fn clamps_prompt_cache_key_to_open_ais_64_character_limit() {
    let (payload, _) = capture_request(json!({"sessionId":"x".repeat(67)}), None);
    assert_eq!(payload["prompt_cache_key"], "x".repeat(64));
}

#[test]
fn omits_prompt_cache_fields_when_cache_retention_is_none() {
    let (payload, _) = capture_request(
        json!({"cacheRetention":"none","sessionId":"session-789"}),
        None,
    );
    assert!(payload.get("prompt_cache_key").is_none());
    assert!(payload.get("prompt_cache_retention").is_none());
}

#[test]
fn omits_prompt_cache_fields_for_non_open_ai_base_urls_without_compatible_long_retention() {
    let model = create_model(
        json!({"baseUrl":"https://proxy.example.com/v1","compat":{"supportsLongCacheRetention":false}}),
    );
    let (payload, _) = capture_request(
        json!({"cacheRetention":"long","sessionId":"session-proxy"}),
        Some(model),
    );
    assert!(payload.get("prompt_cache_key").is_none());
    assert!(payload.get("prompt_cache_retention").is_none());
}

#[test]
fn uses_pi_cache_retention_for_direct_open_ai_requests() {
    let (payload, _) = capture_request(
        json!({"sessionId":"session-env","cacheRetentionEnv":"MAPLE_CACHE_RETENTION","env":{"MAPLE_CACHE_RETENTION":"long"}}),
        None,
    );
    assert_eq!(payload["prompt_cache_key"], "session-env");
    assert_eq!(payload["prompt_cache_retention"], "24h");
}

#[test]
fn sends_known_session_affinity_headers_when_compat_send_session_affinity_headers_is_enabled() {
    let model = create_model(
        json!({"baseUrl":"https://proxy.example.com/v1","compat":{"sendSessionAffinityHeaders":true}}),
    );
    let (_, headers) = capture_request(json!({"sessionId":"session-affinity"}), Some(model));
    assert_eq!(headers["session_id"], "session-affinity");
    assert_eq!(headers["x-client-request-id"], "session-affinity");
    assert_eq!(headers["x-session-affinity"], "session-affinity");
}

fn expect_fireworks_affinity(model_id: &str) {
    let (_, headers) = capture_request(
        json!({"sessionId":"fireworks-session"}),
        Some(fixture_model("fireworks", model_id)),
    );
    assert_eq!(headers["x-session-affinity"], "fireworks-session");
}

#[test]
fn sends_fireworks_session_affinity_for_accounts_fireworks_models_glm_5p3() {
    expect_fireworks_affinity("accounts/fireworks/models/glm-5p3");
}

#[test]
fn sends_fireworks_session_affinity_for_accounts_fireworks_routers_glm_5p3_fast() {
    expect_fireworks_affinity("accounts/fireworks/routers/glm-5p3-fast");
}

#[test]
fn sends_baseten_session_affinity_for_built_in_catalog_models() {
    let (_, headers) = capture_request(
        json!({"sessionId":"baseten-catalog-session"}),
        Some(fixture_model("baseten", "zai-org/GLM-5.2")),
    );
    assert_eq!(headers["x-session-affinity"], "baseten-catalog-session");
    assert_eq!(headers["x-client-request-id"], "baseten-catalog-session");
}

#[test]
fn uses_open_ai_no_session_format_when_configured() {
    let model = create_model(
        json!({"compat":{"sendSessionAffinityHeaders":true,"sessionAffinityFormat":"openai-nosession"}}),
    );
    let (payload, headers) = capture_request(json!({"sessionId":"session-nosession"}), Some(model));
    assert!(payload.get("session_id").is_none());
    assert_eq!(payload["prompt_cache_key"], "session-nosession");
    assert!(headers.get("session_id").is_none());
    assert_eq!(headers["x-client-request-id"], "session-nosession");
    assert_eq!(headers["x-session-affinity"], "session-nosession");
    assert!(headers.get("x-session-id").is_none());
}

#[test]
fn uses_open_router_session_affinity_header_when_configured() {
    let model = create_model(
        json!({"baseUrl":"https://proxy.example.com/v1","compat":{"sendSessionAffinityHeaders":true,"sessionAffinityFormat":"openrouter"}}),
    );
    let (payload, headers) = capture_request(json!({"sessionId":"session-proxy"}), Some(model));
    assert!(payload.get("session_id").is_none());
    assert!(payload.get("prompt_cache_key").is_none());
    assert_eq!(headers["x-session-id"], "session-proxy");
    assert!(headers.get("session_id").is_none());
    assert!(headers.get("x-client-request-id").is_none());
    assert!(headers.get("x-session-affinity").is_none());
}

#[test]
fn sends_open_router_session_affinity_header_by_default_for_built_in_open_router_models() {
    let (payload, headers) = capture_request(
        json!({"sessionId":"session-openrouter"}),
        Some(fixture_model("openrouter", "auto")),
    );
    assert!(payload.get("session_id").is_none());
    assert!(payload.get("prompt_cache_key").is_none());
    assert_eq!(headers["x-session-id"], "session-openrouter");
    assert!(headers.get("session_id").is_none());
    assert!(headers.get("x-client-request-id").is_none());
    assert!(headers.get("x-session-affinity").is_none());
}

#[test]
fn omits_open_router_session_affinity_data_when_disabled() {
    let model = create_model(
        json!({"provider":"openrouter","baseUrl":"https://openrouter.ai/api/v1","compat":{"sendSessionAffinityHeaders":false}}),
    );
    let (payload, headers) =
        capture_request(json!({"sessionId":"session-openrouter"}), Some(model));
    assert!(payload.get("session_id").is_none());
    assert!(payload.get("prompt_cache_key").is_none());
    assert!(headers.get("x-session-id").is_none());
}

#[test]
fn omits_session_affinity_headers_when_cache_retention_is_none() {
    let model = create_model(
        json!({"baseUrl":"https://proxy.example.com/v1","compat":{"sendSessionAffinityHeaders":true}}),
    );
    let (_, headers) = capture_request(
        json!({"cacheRetention":"none","sessionId":"session-affinity"}),
        Some(model),
    );
    assert!(headers.get("session_id").is_none());
    assert!(headers.get("x-client-request-id").is_none());
    assert!(headers.get("x-session-affinity").is_none());
}

#[test]
fn lets_explicit_headers_override_generated_session_affinity_headers() {
    let model = create_model(
        json!({"baseUrl":"https://proxy.example.com/v1","compat":{"sendSessionAffinityHeaders":true}}),
    );
    let (_, headers) = capture_request(
        json!({"sessionId":"session-affinity","headers":{
            "session_id":"override-session","x-client-request-id":"override-request","x-session-affinity":"override-affinity"
        }}),
        Some(model),
    );
    assert_eq!(headers["session_id"], "override-session");
    assert_eq!(headers["x-client-request-id"], "override-request");
    assert_eq!(headers["x-session-affinity"], "override-affinity");
}
