use pi_ai::api::simple_options::build_base_options;
use pi_ai::types::*;
use pi_ai::utils::estimate::{ContextUsageEstimate, estimate_context_tokens};
use pi_ai::utils::transcript::normalize_context;

fn assistant(timestamp: f64, total_tokens: f64) -> Message {
    AssistantMessage {
        content: vec![TextContent::new("kept").into()],
        api: "openai-responses".into(),
        provider: "openai".into(),
        model: "test-model".into(),
        usage: Usage {
            input: total_tokens,
            total_tokens,
            ..Default::default()
        },
        stop_reason: StopReason::Stop,
        timestamp,
        ..Default::default()
    }
    .into()
}

fn user(text: impl Into<JsString>, timestamp: f64) -> Message {
    UserMessage {
        content: UserMessageContent::Text(text.into()),
        timestamp,
        ..Default::default()
    }
    .into()
}

fn model() -> Model {
    Model {
        id: "test-model".into(),
        name: "Test Model".into(),
        api: "openai-responses".into(),
        provider: "openai".into(),
        base_url: "https://api.openai.com/v1".into(),
        input: vec![InputModality::Text],
        context_window: 10_000.0,
        max_tokens: 8_000.0,
        ..Default::default()
    }
}

mod context_token_estimation {
    use super::*;

    #[test]
    fn ignores_stale_assistant_usage_after_a_newer_message_is_inserted_before_it() {
        let context = normalize_context(Context {
            system_prompt: Some("system".into()),
            messages: vec![
                user("summary", 200.0),
                assistant(100.0, 9_500.0),
                user("x".repeat(4_000), 300.0),
            ],
            ..Default::default()
        });
        assert_eq!(
            estimate_context_tokens(&context).unwrap(),
            ContextUsageEstimate {
                tokens: 1_005.0,
                usage_tokens: 0.0,
                trailing_tokens: 1_005.0,
                last_usage_index: None,
            }
        );
        assert_eq!(
            build_base_options(&model(), &context, None, None)
                .unwrap()
                .max_tokens,
            Some(4_899.0)
        );
    }

    #[test]
    fn uses_assistant_usage_again_after_a_response_to_the_inserted_context() {
        let context = normalize_context(Context {
            messages: vec![
                user("summary", 200.0),
                assistant(100.0, 9_500.0),
                user("new prompt", 300.0),
                assistant(400.0, 2_000.0),
                user("tail", 500.0),
            ],
            ..Default::default()
        });
        assert_eq!(
            estimate_context_tokens(&context).unwrap(),
            ContextUsageEstimate {
                tokens: 2_001.0,
                usage_tokens: 2_000.0,
                trailing_tokens: 1.0,
                last_usage_index: Some(3),
            }
        );
    }
}
