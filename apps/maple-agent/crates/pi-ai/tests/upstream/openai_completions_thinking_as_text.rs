use super::common::*;
use pi_ai::api::openai_completions::{ScriptedTransport, convert_messages, stream};
use pi_ai::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, Context, Model,
    OpenAICompletionsCompat, ProviderResponse, StopReason,
};
use pi_ai::utils::transcript::normalize_context;
use serde_json::json;
use std::sync::Arc;

fn compat() -> OpenAICompletionsCompat {
    json(json!({
        "supportsStore": true, "supportsDeveloperRole": true, "supportsReasoningEffort": true,
        "supportsUsageInStreaming": true, "supportsFinishReason": true, "maxTokensField": "max_completion_tokens",
        "requiresToolResultName": false, "requiresAssistantAfterToolResult": false, "requiresThinkingAsText": true,
        "requiresReasoningContentOnAssistantMessages": false, "thinkingFormat": "openai",
        "openRouterRouting": {}, "vercelGatewayRouting": {}, "chatTemplateKwargs": {}, "chatTemplateArgs": {},
        "zaiToolStream": false, "supportsThinkingTokenBudget": false, "supportsStrictMode": true,
        "supportsOpenAIGrammarTools": false, "supportsMidConvoSystemMessages": false,
        "supportsMidConvoToolAdditions": false, "sendSessionAffinityHeaders": false,
        "sessionAffinityFormat": "openai", "supportsLongCacheRetention": true
    }))
}

fn build_model() -> Model {
    let mut model: Model = json(json!({
        "id": "repro-model", "name": "Repro Model", "api": "openai-completions",
        "provider": "repro-provider", "baseUrl": "http://127.0.0.1:1", "reasoning": true,
        "input": ["text"], "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": 128000, "maxTokens": 4096
    }));
    model.compat = Some(compat());
    model
}

fn build_context(content: Vec<AssistantContent>) -> Context {
    let mut assistant = AssistantMessage::new(&build_model(), 2.0);
    assistant.content = content;
    assistant.stop_reason = StopReason::Stop;
    Context {
        messages: vec![
            json(json!({"role": "user", "content": "hello", "timestamp": 1})),
            assistant.into(),
            json(json!({"role": "user", "content": "continue", "timestamp": 3})),
        ],
        ..Default::default()
    }
}

#[test]
fn serializes_same_model_thinking_plus_text_replay_as_assistant_text_parts() {
    let messages = convert_messages(&build_model(), &normalize_context(build_context(json(json!([
        {"type": "thinking", "thinking": "internal reasoning"}, {"type": "text", "text": "visible answer"}
    ])))), &compat(), None, env().as_ref()).unwrap();
    assert_eq!(
        messages[1],
        js(json!({"role": "assistant", "content": [
            {"type": "text", "text": "internal reasoning"}, {"type": "text", "text": "visible answer"}
        ]}))
    );
}

#[test]
fn serializes_same_model_thinking_only_replay_as_assistant_text_parts() {
    let messages = convert_messages(
        &build_model(),
        &normalize_context(build_context(json(json!([
            {"type": "thinking", "thinking": "internal reasoning"}
        ])))),
        &compat(),
        None,
        env().as_ref(),
    )
    .unwrap();
    assert_eq!(
        messages[1],
        js(json!({"role": "assistant", "content": [
            {"type": "text", "text": "internal reasoning"}
        ]}))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn reaches_the_endpoint_when_replay_contains_both_thinking_and_text() {
    // The injected host transport owns HTTP/SSE; preserve the endpoint's captured
    // request and parsed response chunks at that boundary.
    let transport = Arc::new(ScriptedTransport::default());
    transport.push_response(ProviderResponse { status: 200, ..Default::default() }, [
        Ok(js(json!({"id": "chatcmpl-repro", "object": "chat.completion.chunk", "created": 0,
            "model": "repro-model", "choices": [{"index": 0, "delta": {"role": "assistant", "content": "ok"}, "finish_reason": null}]}))),
        Ok(js(json!({"id": "chatcmpl-repro", "object": "chat.completion.chunk", "created": 0,
            "model": "repro-model", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}}))),
    ]);
    let mut events_stream = stream(
        build_model(),
        normalize_context(build_context(json(json!([
            {"type": "thinking", "thinking": "internal reasoning"}, {"type": "text", "text": "visible answer"}
        ])))),
        Some(json(json!({"apiKey": "test-key"}))),
        transport.clone(),
        env(),
    ).unwrap();
    let mut events = Vec::new();
    while let Some(event) = events_stream.next().await {
        events.push(event);
    }

    let request_bodies = transport.requests();
    assert_eq!(request_bodies.len(), 1);
    assert_eq!(
        request_bodies[0].params["messages"][1],
        js(json!({"role": "assistant", "content": [
            {"type": "text", "text": "internal reasoning"}, {"type": "text", "text": "visible answer"}
        ]}))
    );
    assert!(matches!(
        events.last(),
        Some(AssistantMessageEvent::Done { .. })
    ));
}
