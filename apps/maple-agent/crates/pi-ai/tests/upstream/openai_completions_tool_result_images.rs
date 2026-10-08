use super::common::*;
use pi_ai::api::openai_completions::convert_messages;
use pi_ai::types::{
    AssistantContent, AssistantMessage, Context, InputModality, Message, Model,
    OpenAICompletionsCompat, StopReason,
};
use pi_ai::utils::transcript::normalize_context;
use serde_json::json;

fn compat() -> OpenAICompletionsCompat {
    json(json!({
        "supportsStore": true, "supportsDeveloperRole": true, "supportsReasoningEffort": true,
        "supportsUsageInStreaming": true, "supportsFinishReason": true, "maxTokensField": "max_completion_tokens",
        "requiresToolResultName": false, "requiresAssistantAfterToolResult": false, "requiresThinkingAsText": false,
        "requiresReasoningContentOnAssistantMessages": false, "thinkingFormat": "openai",
        "openRouterRouting": {}, "vercelGatewayRouting": {}, "chatTemplateKwargs": {}, "chatTemplateArgs": {},
        "zaiToolStream": false, "supportsThinkingTokenBudget": false, "supportsStrictMode": true,
        "supportsOpenAIGrammarTools": false, "supportsMidConvoSystemMessages": false,
        "supportsMidConvoToolAdditions": false, "cacheControlFormat": "anthropic", "sendSessionAffinityHeaders": false,
        "sessionAffinityFormat": "openai", "supportsLongCacheRetention": true
    }))
}

fn model() -> Model {
    let mut model = fixture_model("openai", "gpt-4o-mini");
    model.compat = None;
    model.api = "openai-completions".into();
    model.input = vec![InputModality::Text, InputModality::Image];
    model
}

fn build_tool_result(tool_call_id: &str, timestamp: f64) -> Message {
    json(
        json!({"role": "toolResult", "toolCallId": tool_call_id, "toolName": "read",
        "content": [{"type": "text", "text": "Read image file [image/png]"},
                    {"type": "image", "data": "ZmFrZQ==", "mimeType": "image/png"}],
        "isError": false, "timestamp": timestamp}),
    )
}

// Regression test for https://github.com/earendil-works/pi/issues/9797.
#[test]
fn omits_empty_text_parts_from_user_messages_with_images() {
    let context = normalize_context(json(json!({"messages": [{"role": "user", "content": [
        {"type": "text", "text": ""}, {"type": "image", "data": "ZmFrZQ==", "mimeType": "image/png"}
    ], "timestamp": 1000}]})));
    assert_eq!(
        convert_messages(&model(), &context, &compat(), None, env().as_ref()).unwrap(),
        vec![js(json!({
            "role": "user", "content": [{"type": "image_url", "image_url": {"url": "data:image/png;base64,ZmFrZQ=="}}]
        }))]
    );
}

#[test]
fn batches_tool_result_images_after_consecutive_tool_results() {
    let model = model();
    let now = 1000.0;
    let mut assistant_message = AssistantMessage::new(&model, now);
    assistant_message.content = json::<Vec<AssistantContent>>(json!([
        {"type": "toolCall", "id": "tool-1", "name": "read", "arguments": {"path": "img-1.png"}},
        {"type": "toolCall", "id": "tool-2", "name": "read", "arguments": {"path": "img-2.png"}}
    ]));
    assistant_message.stop_reason = StopReason::ToolUse;
    let context = normalize_context(Context {
        messages: vec![
            json(json!({"role": "user", "content": "Read the images", "timestamp": now - 2.0})),
            assistant_message.into(),
            build_tool_result("tool-1", now + 1.0),
            build_tool_result("tool-2", now + 2.0),
        ],
        ..Default::default()
    });
    let messages = convert_messages(&model, &context, &compat(), None, env().as_ref()).unwrap();
    let roles = messages
        .iter()
        .map(|message| message["role"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(roles, ["user", "assistant", "tool", "tool", "user"]);
    let image_message = messages.last().unwrap();
    assert_eq!(image_message["role"].as_str(), Some("user"));
    assert!(image_message["content"].is_array());
    let image_parts = image_message["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|part| part.get("type").and_then(|value| value.as_str()) == Some("image_url"))
        .collect::<Vec<_>>();
    assert_eq!(image_parts.len(), 2);
}

#[test]
fn uses_no_tool_output_placeholder_for_empty_tool_results_without_images() {
    let model = model();
    let now = 1000.0;
    let mut assistant_message = AssistantMessage::new(&model, now);
    assistant_message.content = json::<Vec<AssistantContent>>(json!([
        {"type": "toolCall", "id": "tool-1", "name": "bash", "arguments": {"command": "true"}}
    ]));
    assistant_message.stop_reason = StopReason::ToolUse;
    let context = normalize_context(Context {
        messages: vec![
            json(json!({"role": "user", "content": "Run the command", "timestamp": now - 1.0})),
            assistant_message.into(),
            json(
                json!({"role": "toolResult", "toolCallId": "tool-1", "toolName": "bash",
                "content": [{"type": "text", "text": ""}], "isError": false, "timestamp": now + 1.0}),
            ),
        ],
        ..Default::default()
    });
    let messages = convert_messages(&model, &context, &compat(), None, env().as_ref()).unwrap();
    let tool_message = messages
        .iter()
        .find(|message| message["role"].as_str() == Some("tool"));
    assert!(tool_message.is_some());
    let content = tool_message.unwrap()["content"].as_str().unwrap();
    assert_eq!(content, "(no tool output)");
    assert!(!content.contains("see attached image"));
}
