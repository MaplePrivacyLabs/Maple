use super::common::*;
use pi_ai::api::openai_completions::{ScriptedTransport, stream};
use pi_ai::types::{AssistantContent, AssistantMessage, Context, JsValue, Model, ProviderResponse};
use pi_ai::utils::transcript::normalize_context;
use serde_json::{Value, json};
use std::sync::Arc;

fn reasoning_detail() -> Value {
    json!({"type": "reasoning.encrypted", "id": "call_1", "data": "encrypted-signature"})
}

fn signed_reasoning_text_detail() -> Value {
    json!({"type": "reasoning.text", "text": "I should call the read tool.",
        "signature": "sha256:signed-text", "id": "reasoning-text-1", "format": "anthropic-claude-v1", "index": 0})
}

fn reasoning_summary_detail() -> Value {
    json!({"type": "reasoning.summary", "summary": "Decided to inspect the requested file.",
        "id": "reasoning-summary-1", "format": "anthropic-claude-v1", "index": 1})
}

fn model() -> Model {
    json(json!({
        "id": "google/gemini-test", "name": "Gemini Test", "api": "openai-completions",
        "provider": "openrouter", "baseUrl": "https://openrouter.ai/api/v1", "reasoning": true,
        "input": ["text"], "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": 100_000, "maxTokens": 4096
    }))
}

fn chunk(delta: Value, finish_reason: Option<&str>) -> JsValue {
    js(json!({"id": "chatcmpl-test", "model": "google/gemini-test",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]}))
}

fn tool_call_chunk() -> JsValue {
    chunk(
        json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
        "function": {"name": "read", "arguments": "{\"path\":\"README.md\"}"}}]}),
        None,
    )
}

fn scripted_transport(first_chunks: Vec<JsValue>) -> Arc<ScriptedTransport> {
    let transport = Arc::new(ScriptedTransport::default());
    transport.push_response(
        ProviderResponse {
            status: 200,
            ..Default::default()
        },
        first_chunks.into_iter().map(Ok),
    );
    transport.push_response(
        ProviderResponse {
            status: 200,
            ..Default::default()
        },
        [
            Ok(chunk(json!({"content": "ok"}), None)),
            Ok(chunk(json!({}), Some("stop"))),
        ],
    );
    transport
}

async fn run_stream(
    transport: Arc<ScriptedTransport>,
    messages: Vec<AssistantMessage>,
) -> AssistantMessage {
    let context = Context {
        messages: messages.into_iter().map(Into::into).collect(),
        tools: Some(vec![json(
            json!({"name": "read", "description": "Read a file",
            "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}}),
        )]),
        ..Default::default()
    };
    stream(
        model(),
        normalize_context(context),
        Some(json(json!({"apiKey": "test"}))),
        transport,
        env(),
    )
    .unwrap()
    .result()
    .await
}

fn assistant_payload(payload: &JsValue) -> &JsValue {
    payload["messages"]
        .as_array()
        .expect("request messages")
        .iter()
        .find(|message| message["role"].as_str() == Some("assistant"))
        .expect("assistant request message")
}

#[tokio::test(flavor = "current_thread")]
async fn preserves_reasoning_details_in_the_thinking_signature() {
    let transport = scripted_transport(vec![
        chunk(json!({"reasoning_details": [reasoning_detail()]}), None),
        tool_call_chunk(),
        chunk(json!({}), Some("tool_calls")),
    ]);
    let assistant_message = run_stream(transport.clone(), vec![]).await;
    let thinking = assistant_message
        .content
        .iter()
        .find(|block| matches!(block, AssistantContent::Thinking(_)));
    assert_eq!(
        thinking,
        Some(&json::<AssistantContent>(
            json!({"type": "thinking", "thinking": "",
        "thinkingSignature": "[{\"type\":\"reasoning.encrypted\",\"id\":\"call_1\",\"data\":\"encrypted-signature\"}]"})
        ))
    );
    let tool_call = assistant_message
        .content
        .iter()
        .find(|block| matches!(block, AssistantContent::ToolCall(_)));
    assert_eq!(
        tool_call,
        Some(&json::<AssistantContent>(
            json!({"type": "toolCall", "id": "call_1", "name": "read", "arguments": {"path": "README.md"}})
        ))
    );

    run_stream(transport.clone(), vec![assistant_message]).await;
    assert_eq!(
        assistant_payload(&transport.requests()[1].params).get("reasoning_details"),
        Some(&js(json!([reasoning_detail()])))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn falls_back_to_encrypted_tool_call_signatures_for_older_stored_assistant_messages() {
    let transport = scripted_transport(vec![
        chunk(json!({"reasoning_details": [reasoning_detail()]}), None),
        tool_call_chunk(),
        chunk(json!({}), Some("tool_calls")),
    ]);
    let mut assistant_message = run_stream(transport.clone(), vec![]).await;
    assistant_message
        .content
        .retain(|block| !matches!(block, AssistantContent::Thinking(_)));
    let tool_call = assistant_message
        .content
        .iter_mut()
        .find_map(|block| match block {
            AssistantContent::ToolCall(call) => Some(call),
            _ => None,
        })
        .expect("Expected tool call");
    tool_call.thought_signature = Some(
        "{\"type\":\"reasoning.encrypted\",\"id\":\"call_1\",\"data\":\"encrypted-signature\"}"
            .into(),
    );

    run_stream(transport.clone(), vec![assistant_message]).await;
    assert_eq!(
        assistant_payload(&transport.requests()[1].params).get("reasoning_details"),
        Some(&js(json!([reasoning_detail()])))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn preserves_signed_text_and_summary_reasoning_details_in_their_original_sequence() {
    let transport = scripted_transport(vec![
        chunk(
            json!({"reasoning": "I should call the read tool.", "reasoning_details": [signed_reasoning_text_detail()]}),
            None,
        ),
        chunk(
            json!({"reasoning_details": [reasoning_detail(), reasoning_summary_detail()]}),
            None,
        ),
        tool_call_chunk(),
        chunk(json!({}), Some("tool_calls")),
    ]);
    let assistant_message = run_stream(transport.clone(), vec![]).await;
    let expected_reasoning_details = js(json!([
        signed_reasoning_text_detail(),
        reasoning_detail(),
        reasoning_summary_detail()
    ]));
    let thinking = assistant_message
        .content
        .iter()
        .find(|block| matches!(block, AssistantContent::Thinking(_)));
    assert_eq!(
        thinking,
        Some(&json::<AssistantContent>(
            json!({"type": "thinking", "thinking": "I should call the read tool.",
        "thinkingSignature": "[{\"type\":\"reasoning.text\",\"text\":\"I should call the read tool.\",\"signature\":\"sha256:signed-text\",\"id\":\"reasoning-text-1\",\"format\":\"anthropic-claude-v1\",\"index\":0},{\"type\":\"reasoning.encrypted\",\"id\":\"call_1\",\"data\":\"encrypted-signature\"},{\"type\":\"reasoning.summary\",\"summary\":\"Decided to inspect the requested file.\",\"id\":\"reasoning-summary-1\",\"format\":\"anthropic-claude-v1\",\"index\":1}]"})
        ))
    );

    run_stream(transport.clone(), vec![assistant_message]).await;
    let requests = transport.requests();
    let payload = assistant_payload(&requests[1].params);
    assert_eq!(
        payload.get("reasoning_details"),
        Some(&expected_reasoning_details)
    );
    assert!(payload.get("reasoning").is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn merges_consecutive_text_and_summary_reasoning_details_deltas_before_replay() {
    let text_delta = json!({"type": "reasoning.text", "text": "The", "index": 0});
    let text_delta_with_signature = json!({"type": "reasoning.text", "text": " user wants the time.",
        "signature": "sha256:text-signature", "format": "openai-responses-v1", "index": 0});
    let summary_delta = json!({"type": "reasoning.summary", "summary": "Looked", "index": 0});
    let summary_delta_with_format = json!({"type": "reasoning.summary", "summary": " up time.", "format": "openai-responses-v1", "index": 0});
    let later_summary_delta = json!({"type": "reasoning.summary", "summary": "After encrypted block.", "format": "openai-responses-v1", "index": 0});
    let expected_reasoning_details = js(json!([
        {"type": "reasoning.text", "text": "The user wants the time.", "index": 0, "signature": "sha256:text-signature", "format": "openai-responses-v1"},
        {"type": "reasoning.summary", "summary": "Looked up time.", "index": 0, "format": "openai-responses-v1"},
        reasoning_detail(), later_summary_delta.clone()
    ]));
    let transport = scripted_transport(vec![
        chunk(json!({"reasoning_details": [text_delta]}), None),
        chunk(
            json!({"reasoning_details": [text_delta_with_signature]}),
            None,
        ),
        chunk(json!({"reasoning_details": [summary_delta]}), None),
        chunk(
            json!({"reasoning_details": [summary_delta_with_format]}),
            None,
        ),
        chunk(json!({"reasoning_details": [reasoning_detail()]}), None),
        chunk(json!({"reasoning_details": [later_summary_delta]}), None),
        tool_call_chunk(),
        chunk(json!({}), Some("tool_calls")),
    ]);
    let assistant_message = run_stream(transport.clone(), vec![]).await;
    let thinking = assistant_message
        .content
        .iter()
        .find(|block| matches!(block, AssistantContent::Thinking(_)));
    assert_eq!(
        thinking,
        Some(&json::<AssistantContent>(
            json!({"type": "thinking", "thinking": "",
        "thinkingSignature": "[{\"type\":\"reasoning.text\",\"text\":\"The user wants the time.\",\"index\":0,\"signature\":\"sha256:text-signature\",\"format\":\"openai-responses-v1\"},{\"type\":\"reasoning.summary\",\"summary\":\"Looked up time.\",\"index\":0,\"format\":\"openai-responses-v1\"},{\"type\":\"reasoning.encrypted\",\"id\":\"call_1\",\"data\":\"encrypted-signature\"},{\"type\":\"reasoning.summary\",\"summary\":\"After encrypted block.\",\"format\":\"openai-responses-v1\",\"index\":0}]"})
        ))
    );

    run_stream(transport.clone(), vec![assistant_message]).await;
    assert_eq!(
        assistant_payload(&transport.requests()[1].params).get("reasoning_details"),
        Some(&expected_reasoning_details)
    );
}
