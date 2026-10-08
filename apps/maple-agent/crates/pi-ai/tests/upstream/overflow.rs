//! Translated from `packages/ai/test/overflow.test.ts` at Pi v1.0.4.
use pi_ai::types::{AssistantMessage, StopReason, Usage};
use pi_ai::utils::overflow::{is_context_overflow, is_recoverable_length};

fn create_error_message(error_message: &str, provider: &str) -> AssistantMessage {
    AssistantMessage {
        api: "openai-completions".into(),
        provider: provider.into(),
        model: "qwen3.5:35b".into(),
        stop_reason: StopReason::Error,
        error_message: Some(error_message.into()),
        ..AssistantMessage::default()
    }
}

fn create_length_stop_message(input: f64, cache_read: f64, output: f64) -> AssistantMessage {
    AssistantMessage {
        api: "openai-completions".into(),
        provider: "test-provider".into(),
        model: "test-model".into(),
        stop_reason: StopReason::Length,
        usage: Usage {
            input,
            output,
            cache_read,
            total_tokens: input + cache_read + output,
            ..Usage::default()
        },
        ..AssistantMessage::default()
    }
}

mod is_context_overflow {
    use super::*;

    #[test]
    fn detects_explicit_ollama_prompt_too_long_errors() {
        let message = create_error_message(
            "400 `prompt too long; exceeded max context length by 100918 tokens`",
            "ollama",
        );
        assert!(is_context_overflow(&message, Some(32768.0)));
    }

    #[test]
    fn detects_z_ai_prompt_too_long_errors() {
        let message =
            create_error_message(r#"400 {"code":"1261","message":"Prompt too long"}"#, "zai");
        assert!(is_context_overflow(&message, Some(1048576.0)));
    }

    #[test]
    fn detects_z_ai_cn_endpoint_prompt_exceeds_max_length_errors() {
        let message = create_error_message(
            r#"400 {"code":"1261","message":"Prompt exceeds max length"}"#,
            "zai",
        );
        assert!(is_context_overflow(&message, Some(1048576.0)));
    }

    #[test]
    fn detects_together_ai_context_length_errors() {
        let message = create_error_message(
            "400 The input (516368 tokens) is longer than the model's context length (262144 tokens).",
            "ollama",
        );
        assert!(is_context_overflow(&message, Some(262144.0)));
    }

    #[test]
    fn detects_litellm_wrapped_openai_maximum_context_length_errors() {
        let message = create_error_message(
            "Error: 503 litellm.ServiceUnavailableError: litellm.MidStreamFallbackError: litellm.APIConnectionError: APIConnectionError: OpenAIException - Requested token count exceeds the model's maximum context length of 131072 tokens.",
            "ollama",
        );
        assert!(is_context_overflow(&message, Some(131072.0)));
    }

    #[test]
    fn detects_openai_compatible_parenthesized_maximum_context_length_errors() {
        let message = create_error_message(
            "Error: 400 Input length (265330) exceeds model's maximum context length (262144).",
            "ollama",
        );
        assert!(is_context_overflow(&message, Some(262144.0)));
    }

    #[test]
    fn detects_openrouter_poolside_maximum_allowed_input_length_errors() {
        let message = create_error_message(
            "Provider returned error: Input length 131393 exceeds the maximum allowed input length of 131040 tokens.",
            "ollama",
        );
        assert!(is_context_overflow(&message, Some(131072.0)));
    }

    #[test]
    fn detects_ds4_configured_context_size_errors() {
        let message = create_error_message(
            "400 Prompt has 256468 tokens, but the configured context size is 256000 tokens",
            "ollama",
        );
        assert!(is_context_overflow(&message, Some(256000.0)));
        let comma_message = create_error_message(
            "Prompt has 5,958,968 tokens, but the configured context size is 256,000 tokens",
            "ollama",
        );
        assert!(is_context_overflow(&comma_message, Some(256000.0)));
    }

    #[test]
    fn does_not_treat_generic_non_overflow_ollama_errors_as_overflow() {
        let message = create_error_message("500 `model runner crashed unexpectedly`", "ollama");
        assert!(!is_context_overflow(&message, Some(32768.0)));
    }

    #[test]
    fn only_treats_bodyless_400_and_413_errors_as_overflow_for_cerebras() {
        for error_message in ["400 status code (no body)", "413 status code (no body)"] {
            assert!(is_context_overflow(
                &create_error_message(error_message, "cerebras"),
                Some(131072.0)
            ));
            assert!(!is_context_overflow(
                &create_error_message(error_message, "opencode-go"),
                Some(1000000.0)
            ));
        }
    }

    #[test]
    fn does_not_treat_bedrock_throttling_too_many_tokens_as_overflow() {
        let message = create_error_message(
            "Throttling error: Too many tokens, please wait before trying again.",
            "ollama",
        );
        assert!(!is_context_overflow(&message, Some(200000.0)));
    }

    #[test]
    fn does_not_treat_bedrock_service_unavailable_as_overflow() {
        let message = create_error_message(
            "Service unavailable: The service is temporarily unavailable.",
            "ollama",
        );
        assert!(!is_context_overflow(&message, Some(200000.0)));
    }

    #[test]
    fn does_not_treat_generic_rate_limit_errors_as_overflow() {
        let message = create_error_message(
            "Rate limit exceeded, please retry after 30 seconds.",
            "ollama",
        );
        assert!(!is_context_overflow(&message, Some(200000.0)));
    }

    #[test]
    fn does_not_treat_http_429_style_errors_as_overflow() {
        let message = create_error_message("Too many requests. Please slow down.", "ollama");
        assert!(!is_context_overflow(&message, Some(200000.0)));
    }

    #[test]
    fn detects_xiaomi_style_overflow_length_stop_with_zero_output_and_filled_context() {
        let mut message = create_length_stop_message(58.0, 1048512.0, 0.0);
        message.provider = "xiaomi".into();
        message.model = "mimo-v2.5-pro".into();
        assert!(is_context_overflow(&message, Some(1048576.0)));
    }

    #[test]
    fn treats_a_length_stop_below_the_desired_output_limit_as_recoverable() {
        let mut message = create_length_stop_message(3.0, 253584.0, 16.0);
        message.usage.cache_write = 25554.0;
        message.usage.total_tokens += 25554.0;
        message.api = "openai-responses".into();
        message.provider = "openai".into();
        message.model = "gpt-5.6-sol".into();
        assert!(is_recoverable_length(&message, 128000.0));
    }

    #[test]
    fn does_not_recover_a_length_stop_that_reached_the_desired_output_limit() {
        let message = create_length_stop_message(4062.0, 0.0, 1024.0);
        assert!(!is_recoverable_length(&message, 1024.0));
    }

    #[test]
    fn treats_zero_output_length_stops_as_recoverable_without_context_metadata() {
        let message = create_length_stop_message(100.0, 0.0, 0.0);
        assert!(is_recoverable_length(&message, 128000.0));
    }

    #[test]
    fn does_not_treat_normal_length_stops_with_output_as_context_overflow() {
        let message = create_length_stop_message(1000.0, 0.0, 4096.0);
        assert!(!is_context_overflow(&message, Some(200000.0)));
    }

    #[test]
    fn does_not_treat_zero_output_length_stops_far_below_context_as_context_overflow() {
        let message = create_length_stop_message(100.0, 0.0, 0.0);
        assert!(!is_context_overflow(&message, Some(200000.0)));
    }
}
