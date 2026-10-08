use super::common::*;
use pi_ai::api::transform_messages::transform_messages_raw;
use pi_ai::types::*;
use serde_json::json;

mod lax_message_content_handling {
    use super::*;

    #[test]
    fn normalizes_null_missing_content_to_an_empty_array_instead_of_crashing() {
        let model: Model = json(
            json!({"id":"test-model","name":"Test Model","api":"openai-completions",
            "provider":"openai","baseUrl":"https://example.invalid/v1","reasoning":false,"input":["text"],
            "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"contextWindow":128000,"maxTokens":16000}),
        );
        // Keep malformed history unchanged until the public transform boundary,
        // including its non-vision image replacement path.
        let messages = js(json!([
            {"role":"user","content":null,"timestamp":0},
            {"role":"assistant","content":null,"api":"openai-completions","provider":"openai","model":"test-model",
                "usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,
                    "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},
                "stopReason":"stop","timestamp":0},
            {"role":"toolResult","toolCallId":"call_1","toolName":"web_search","isError":false,"timestamp":0}
        ]));
        let result = transform_messages_raw(&messages, &model, None, env().as_ref()).unwrap();
        assert_eq!(result.len(), 3);
        for message in result {
            assert_eq!(serde_json::to_value(message).unwrap()["content"], json!([]));
        }
    }
}
