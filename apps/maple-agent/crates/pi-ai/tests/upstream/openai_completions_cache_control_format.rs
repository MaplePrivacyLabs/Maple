use super::common::*;
use pi_ai::api::openai_completions::{OpenAICompletionsOptions, build_params};
use pi_ai::types::*;
use pi_ai::utils::transcript::normalize_context;
use serde_json::{Value, json};

fn custom_model() -> Model {
    json(json!({
        "id": "custom-qwen", "name": "Custom Qwen", "api": "openai-completions",
        "provider": "openrouter", "baseUrl": "https://example.com/v1", "reasoning": true,
        "input": ["text"], "cost": {"input":0,"output":0,"cacheRead":0,"cacheWrite":0},
        "contextWindow":128000, "maxTokens":32000, "compat":{"cacheControlFormat":"anthropic"}
    }))
}

fn capture_payload(
    model: Model,
    retention: Option<CacheRetention>,
    messages: Option<Value>,
) -> Value {
    let context = normalize_context(json(json!({
        "systemPrompt":"System prompt",
        "messages":messages.unwrap_or_else(|| json!([{"role":"user","content":"Hello","timestamp":0}])),
        "tools":[{"name":"read","description":"Read a file","parameters":{
            "type":"object","properties":{"path":{"type":"string"}},"required":["path"]
        }}]
    })));
    let mut options: OpenAICompletionsOptions = json(
        json!({"apiKey":"test-key","cacheRetentionEnv":"MAPLE_CACHE_RETENTION","env":{"MAPLE_CACHE_RETENTION":"short"}}),
    );
    options.cache_retention = retention;
    serde_json::to_value(build_params(&model, &context, Some(&options), env().as_ref()).unwrap())
        .unwrap()
}

fn instruction_message(params: &Value) -> &Value {
    params["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| matches!(message["role"].as_str(), Some("system" | "developer")))
        .expect("instruction message should be defined")
}

fn expect_anthropic_cache_markers(params: &Value) {
    let instruction = instruction_message(params);
    assert!(instruction["content"].is_array());
    assert_eq!(
        instruction["content"][0]["cache_control"],
        json!({"type":"ephemeral"})
    );
    assert_eq!(params["tools"].as_array().unwrap().len(), 1);
    assert_eq!(
        params["tools"][0]["cache_control"],
        json!({"type":"ephemeral"})
    );
    let last = params["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(last["role"], "user");
    assert!(last["content"].is_array());
    assert_eq!(
        last["content"][0]["cache_control"],
        json!({"type":"ephemeral"})
    );
}

#[test]
fn applies_anthropic_style_cache_markers_when_model_compat_enables_them() {
    expect_anthropic_cache_markers(&capture_payload(custom_model(), None, None));
}

#[test]
fn preserves_anthropic_style_cache_markers_for_open_router_anthropic_batch_aliases() {
    expect_anthropic_cache_markers(&capture_payload(
        fixture_model("openrouter", "anthropic/claude-fable-5.1:batch"),
        None,
        None,
    ));
}

#[test]
fn moves_the_conversation_cache_marker_to_a_tool_result() {
    let model = fixture_model("openrouter", "anthropic/claude-fable-5.1:batch");
    let messages = json!([
        {"role":"user","content":"Read the file","timestamp":0},
        {"role":"assistant","content":[{"type":"toolCall","id":"call_1","name":"read","arguments":{"path":"README.md"}}],
         "api":"openai-completions","provider":"openrouter","model":model.id,
         "usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,
                  "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},
         "stopReason":"toolUse","timestamp":0},
        {"role":"toolResult","toolCallId":"call_1","toolName":"read","content":[{"type":"text","text":"file contents"}],"isError":false,"timestamp":0}
    ]);
    let params = capture_payload(model, None, Some(messages));
    let messages = params["messages"].as_array().unwrap();
    let user = messages
        .iter()
        .find(|message| message["role"] == "user")
        .unwrap();
    assert_eq!(user["content"], "Read the file");
    let tool = messages.last().unwrap();
    assert_eq!(tool["role"], "tool");
    assert!(tool["content"].is_array());
    assert_eq!(
        tool["content"][0]["cache_control"],
        json!({"type":"ephemeral"})
    );
}

#[test]
fn omits_anthropic_style_cache_markers_when_cache_retention_is_none() {
    let params = capture_payload(custom_model(), Some(CacheRetention::None), None);
    assert!(!instruction_message(&params)["content"].is_array());
    assert!(params["tools"][0].get("cache_control").is_none());
    assert!(params["messages"].as_array().unwrap().last().unwrap()["content"].is_string());
}
