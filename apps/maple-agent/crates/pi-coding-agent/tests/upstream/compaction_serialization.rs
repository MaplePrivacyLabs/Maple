use super::compaction_common::*;
use pi_ai::types::Message;
use pi_coding_agent::core::compaction::utils::serialize_conversation;
use serde_json::json as j;

mod serialize_conversation {
    use super::*;

    #[test]
    fn should_truncate_long_tool_results() {
        let messages: Vec<Message> = vec![json(tool_result(&"x".repeat(5000)))];
        let value = observed(&serialize_conversation(&messages).unwrap());
        let result = value.as_str().unwrap();
        assert!(result.contains("[Tool result]:"));
        assert!(result.contains("[... 3000 more characters truncated]"));
        assert!(!result.contains(&"x".repeat(3000)));
        assert!(result.contains(&"x".repeat(2000)));
    }

    #[test]
    fn should_not_truncate_short_tool_results() {
        let text = "x".repeat(1500);
        let messages: Vec<Message> = vec![json(tool_result(&text))];
        let value = observed(&serialize_conversation(&messages).unwrap());
        let result = value.as_str().unwrap();
        assert_eq!(result, format!("[Tool result]: {text}"));
        assert!(!result.contains("truncated"));
    }

    #[test]
    fn should_not_truncate_assistant_or_user_messages() {
        let text = "y".repeat(5000);
        let mut reply = assistant(&text, Some(usage(0.0, 0.0, 0.0, 0.0)));
        reply["api"] = j!("anthropic");
        reply["model"] = j!("test");
        let messages: Vec<Message> = json(j!([
            {"role":"user","content":[{"type":"text","text":text}],"timestamp":0}, reply
        ]));
        let value = observed(&serialize_conversation(&messages).unwrap());
        let result = value.as_str().unwrap();
        assert!(!result.contains("truncated"));
        assert!(result.contains(&text));
    }
}
