use super::common::*;
use pi_ai::api::transform_messages::transform_messages;
use pi_ai::types::*;
use serde_json::{Value, json};

fn model() -> Model {
    json(
        json!({"id":"claude-sonnet-4.6","name":"Claude Sonnet 4.6","api":"anthropic-messages",
        "provider":"github-copilot","baseUrl":"https://api.individual.githubcopilot.com",
        "reasoning":true,"input":["text","image"],"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},
        "contextWindow":128000,"maxTokens":16000}),
    )
}
fn assistant(content: Value) -> Value {
    json!({"role":"assistant","content":content,"api":"openai-responses","provider":"github-copilot",
        "model":"gpt-5","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,
            "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},
        "stopReason":"toolUse","timestamp":0})
}
fn normalize(id: &JsString, _: &Model, _: &Message) -> JsString {
    // JavaScript /[^a-zA-Z0-9_-]/g replaces each UTF-16 code unit before slice(0, 64).
    JsString::from_utf16(
        id.as_utf16()
            .iter()
            .take(64)
            .map(|unit| {
                if matches!(*unit, 48..=57 | 65..=90 | 97..=122 | 45 | 95) {
                    *unit
                } else {
                    95
                }
            })
            .collect::<Vec<_>>(),
    )
}
fn transform(messages: Value) -> Vec<Message> {
    transform_messages(
        &json::<Vec<Message>>(messages),
        &model(),
        Some(&normalize),
        env().as_ref(),
    )
    .unwrap()
}
fn assistant_result(messages: &[Message]) -> &AssistantMessage {
    messages
        .iter()
        .find_map(|message| match message {
            Message::Assistant(message) => Some(message),
            _ => None,
        })
        .unwrap()
}

mod openai_to_anthropic_session_migration_for_copilot_claude {
    use super::*;

    #[test]
    fn converts_thinking_blocks_to_plain_text_when_source_model_differs() {
        let mut source = assistant(json!([
            {"type":"thinking","thinking":"Let me think about this...","thinkingSignature":"reasoning_content"},
            {"type":"text","text":"Hi there!"}
        ]));
        source["api"] = json!("openai-completions");
        source["model"] = json!("gpt-4o");
        source["stopReason"] = json!("stop");
        let result = transform(json!([{"role":"user","content":"hello","timestamp":0}, source]));
        let assistant = assistant_result(&result);
        assert_eq!(
            assistant
                .content
                .iter()
                .filter(|block| matches!(block, AssistantContent::Thinking(_)))
                .count(),
            0
        );
        assert!(
            assistant
                .content
                .iter()
                .filter(|block| matches!(block, AssistantContent::Text(_)))
                .count()
                >= 2
        );
    }

    #[test]
    fn removes_thought_signature_from_tool_calls_when_migrating_between_models() {
        let result = transform(json!([
            {"role":"user","content":"run a command","timestamp":0},
            assistant(json!([{"type":"toolCall","id":"call_123","name":"bash","arguments":{"command":"ls"},
                "thoughtSignature":json!({"type":"reasoning.encrypted","id":"call_123","data":"encrypted"}).to_string()}])),
            {"role":"toolResult","toolCallId":"call_123","toolName":"bash","content":[{"type":"text","text":"output"}],"isError":false,"timestamp":0}
        ]));
        let call = assistant_result(&result)
            .content
            .iter()
            .find_map(|block| match block {
                AssistantContent::ToolCall(call) => Some(call),
                _ => None,
            })
            .unwrap();
        assert_eq!(call.thought_signature, None);
    }

    #[test]
    fn adds_synthetic_tool_results_for_trailing_orphaned_tool_calls() {
        let result = transform(json!([
            {"role":"user","content":"read the file","timestamp":0},
            assistant(json!([{"type":"toolCall","id":"call_123|fc_123","name":"read","arguments":{"path":"README.md"}}]))
        ]));
        let last = serde_json::to_value(result.last().unwrap()).unwrap();
        assert_eq!(last["role"], "toolResult");
        assert_eq!(last["toolCallId"], "call_123_fc_123");
        assert_eq!(last["toolName"], "read");
        assert_eq!(last["isError"], true);
        assert_eq!(
            last["content"],
            json!([{"type":"text","text":"No result provided"}])
        );
    }

    #[test]
    fn adds_synthetic_results_only_for_trailing_tool_calls_that_are_still_missing_results() {
        let result = transform(json!([
            {"role":"user","content":"run commands","timestamp":0},
            assistant(json!([
                {"type":"toolCall","id":"call_1|fc_1","name":"read","arguments":{"path":"README.md"}},
                {"type":"toolCall","id":"call_2|fc_2","name":"bash","arguments":{"command":"pwd"}}
            ])),
            {"role":"toolResult","toolCallId":"call_1|fc_1","toolName":"read","content":[{"type":"text","text":"done"}],"isError":false,"timestamp":0}
        ]));
        let synthetic = result
            .iter()
            .filter_map(|message| match message {
                Message::ToolResult(message) if message.is_error => Some(message),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(synthetic.len(), 1);
        let synthetic = serde_json::to_value(Message::ToolResult((*synthetic[0]).clone())).unwrap();
        assert_eq!(synthetic["role"], "toolResult");
        assert_eq!(synthetic["toolCallId"], "call_2_fc_2");
        assert_eq!(synthetic["toolName"], "bash");
        assert_eq!(
            synthetic["content"],
            json!([{"type":"text","text":"No result provided"}])
        );
    }
}
