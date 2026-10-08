use super::common::*;
use pi_ai::api::openai_completions::{OpenAICompletionsOptions, build_params};
use pi_ai::types::*;
use pi_ai::utils::transcript::normalize_context;
use serde_json::{Value, json};

fn create_completions_model(compat: Option<Value>) -> Model {
    let mut model = json!({
        "id":"test-model","name":"Test Model","api":"openai-completions",
        "provider":"test-openai-completions","baseUrl":"https://my-proxy.example.com/v1",
        "reasoning":false,"input":["text"],"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},
        "contextWindow":128000,"maxTokens":4096
    });
    if let Some(compat) = compat {
        model["compat"] = compat;
    }
    json(model)
}

fn context() -> TranscriptContext {
    normalize_context(json(
        json!({"systemPrompt":"You are a helpful assistant.","messages":[{"role":"user","content":"Hello","timestamp":0}]}),
    ))
}

fn capture_payload(model: &Model, context: TranscriptContext, options: Value) -> Value {
    let mut all_options = json!({"apiKey":"fake-key","cacheRetentionEnv":"MAPLE_CACHE_RETENTION","env":{"MAPLE_CACHE_RETENTION":"short"}});
    all_options
        .as_object_mut()
        .unwrap()
        .extend(options.as_object().unwrap().clone());
    let options: OpenAICompletionsOptions = json(all_options);
    serde_json::to_value(build_params(model, &context, Some(&options), env().as_ref()).unwrap())
        .unwrap()
}

#[test]
fn should_set_prompt_cache_retention_for_non_api_openai_com_base_url_by_default() {
    let payload = capture_payload(
        &create_completions_model(None),
        context(),
        json!({"cacheRetention":"long","sessionId":"session-completions"}),
    );
    assert!(payload.is_object());
    assert_eq!(payload["prompt_cache_key"], "session-completions");
    assert_eq!(payload["prompt_cache_retention"], "24h");
}

#[test]
fn should_omit_prompt_cache_retention_when_supports_long_cache_retention_is_false() {
    let payload = capture_payload(
        &create_completions_model(Some(json!({"supportsLongCacheRetention":false}))),
        context(),
        json!({"cacheRetention":"long","sessionId":"session-completions-false"}),
    );
    assert!(payload.is_object());
    assert!(payload.get("prompt_cache_key").is_none());
    assert!(payload.get("prompt_cache_retention").is_none());
}

fn expect_opencode_omits_long_cache_retention(id: &str) {
    let model = fixture_model("opencode", id);
    let payload = capture_payload(
        &model,
        context(),
        json!({"cacheRetention":"long","sessionId":"session-opencode-long-cache-unsupported"}),
    );
    assert_eq!(
        model
            .compat
            .as_ref()
            .and_then(|compat| compat.supports_long_cache_retention),
        Some(false)
    );
    assert!(payload.is_object());
    assert!(payload.get("prompt_cache_key").is_none());
    assert!(payload.get("prompt_cache_retention").is_none());
}

#[test]
fn should_omit_long_cache_retention_for_opencode_deepseek_v4_flash() {
    expect_opencode_omits_long_cache_retention("deepseek-v4-flash");
}

#[test]
fn should_omit_long_cache_retention_for_opencode_deepseek_v4_pro() {
    expect_opencode_omits_long_cache_retention("deepseek-v4-pro");
}

#[test]
fn should_omit_long_cache_retention_for_opencode_kimi_k2_5() {
    expect_opencode_omits_long_cache_retention("kimi-k2.5");
}

#[test]
fn should_omit_long_cache_retention_for_opencode_kimi_k2_6() {
    expect_opencode_omits_long_cache_retention("kimi-k2.6");
}

#[test]
fn should_omit_long_cache_retention_for_opencode_minimax_m2_7() {
    expect_opencode_omits_long_cache_retention("minimax-m2.7");
}

fn expect_cerebras_omits_strict_field(id: &str) {
    let model = fixture_model("cerebras", id);
    let context = normalize_context(json(json!({"messages":[
        {"role":"system","content":"test","toolsAdded":[
            {"name":"t1","description":"strict tool","parameters":{"type":"object","properties":{"x":{"type":"string"}},"required":["x"]},"constrainedSampling":{"type":"json_schema"}},
            {"name":"t2","description":"non-strict tool","parameters":{"type":"object","properties":{"y":{"type":"string"}},"required":["y"]}}
        ],"timestamp":0},
        {"role":"user","content":"hello","timestamp":1}
    ]})));
    let payload = capture_payload(&model, context, json!({"sessionId":"test"}));
    assert_eq!(
        model
            .compat
            .as_ref()
            .and_then(|compat| compat.supports_strict_mode),
        None
    );
    assert!(payload.is_object());
    let tools = payload
        .get("tools")
        .expect("tools should be defined")
        .as_array()
        .unwrap();
    for tool in tools {
        assert!(tool["function"].get("strict").is_none());
    }
}

#[test]
fn should_omit_strict_field_on_tools_for_cerebras_gpt_oss_120b() {
    expect_cerebras_omits_strict_field("gpt-oss-120b");
}

#[test]
fn should_omit_strict_field_on_tools_for_cerebras_qwen_3_8_27b() {
    expect_cerebras_omits_strict_field("qwen-3.8-27b");
}
