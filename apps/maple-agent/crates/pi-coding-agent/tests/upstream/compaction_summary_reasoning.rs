use super::compaction_common::*;
use pi_agent_core::types::ThinkingLevel;
use pi_ai::types::{DoneReason, SimpleStreamOptions};
use pi_ai::utils::transcript::normalize_context;
use pi_coding_agent::core::compaction::compaction::{
    CompactionPreparation, SummaryOptions, compact, complete_summarization, generate_summary,
    generate_summary_with_usage,
};
use serde_json::json as j;

fn options() -> SummaryOptions {
    SummaryOptions {
        api_key: Some("test-key".into()),
        ..Default::default()
    }
}
fn split_preparation(
    previous_summary: Option<&str>,
    with_history: bool,
    reserve: f64,
) -> CompactionPreparation {
    let mut value = j!({"firstKeptEntryId":"entry-keep","messagesToSummarize":if with_history {observed(&summary_messages())} else {j!([])},
        "turnPrefixMessages":observed(&summary_messages()),"isSplitTurn":true,"tokensBefore":if with_history {600000} else {100},
        "fileOps":{"read":[],"written":[],"edited":[]},
        "settings":{"enabled":true,"reserveTokens":reserve,"keepRecentTokens":if with_history {20000} else {20}}});
    if let Some(summary) = previous_summary {
        value["previousSummary"] = j!(summary);
    }
    json(value)
}

mod generate_summary_reasoning_options {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn uses_the_provided_thinking_level_for_reasoning_capable_models() {
        let mock = SummaryMock::new();
        let runtime = mock.runtime();
        let options = SummaryOptions {
            thinking_level: Some(ThinkingLevel::Medium),
            ..options()
        };
        let result = generate_summary_with_usage(
            &summary_messages(),
            &summary_model(true, 8192.0, None),
            2000.0,
            &options,
            &runtime,
            None,
        )
        .await
        .unwrap();
        assert_eq!(observed(&result.text), "## Goal\nTest summary");
        assert_eq!(result.usage, summary_response().usage);
        let calls = mock.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].options["reasoning"], "medium");
        assert_eq!(calls[0].options["apiKey"], "test-key");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn preserves_the_string_result_from_generate_summary() {
        let mock = SummaryMock::new();
        let result = generate_summary(
            &summary_messages(),
            &summary_model(false, 8192.0, None),
            2000.0,
            &options(),
            &mock.runtime(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(observed(&result), "## Goal\nTest summary");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn uses_fresh_routing_sessions_without_prompt_caching() {
        let mock = SummaryMock::new();
        let runtime = mock.runtime();
        let model = summary_model(false, 8192.0, None);
        generate_summary(
            &summary_messages(),
            &model,
            2000.0,
            &options(),
            &runtime,
            None,
        )
        .await
        .unwrap();
        generate_summary(
            &summary_messages(),
            &model,
            2000.0,
            &options(),
            &runtime,
            None,
        )
        .await
        .unwrap();
        let calls = mock.calls();
        assert_eq!(calls.len(), 2);
        assert!(
            calls
                .iter()
                .all(|call| call.options["cacheRetention"] == "none")
        );
        assert_ne!(
            calls[0].options.get("sessionId"),
            calls[1].options.get("sessionId")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn honors_caller_supplied_routing_session_and_tool_choice_without_prompt_caching() {
        let mock = SummaryMock::new();
        let context = normalize_context(json(j!({"systemPrompt":"Summarize","messages":[]})));
        let options: SimpleStreamOptions = json(
            j!({"sessionId":"current-routing-session","cacheRetention":"long","toolChoice":"auto"}),
        );
        complete_summarization(
            &summary_model(false, 8192.0, None),
            context,
            options,
            &mock.runtime(),
            None,
            None,
        )
        .await
        .unwrap();
        let calls = mock.calls();
        assert_eq!(calls[0].options["sessionId"], "current-routing-session");
        assert_eq!(calls[0].options["cacheRetention"], "none");
        assert_eq!(calls[0].options["toolChoice"], "auto");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn preserves_the_previous_summary_without_an_empty_history_request_for_a_split_turn() {
        let mock = SummaryMock::new();
        let preparation = split_preparation(Some("previous checkpoint"), false, 2000.0);
        let result = compact(
            &preparation,
            &summary_model(false, 8192.0, None),
            &options(),
            &mock.runtime(),
            None,
        )
        .await
        .unwrap();
        let calls = mock.calls();
        assert_eq!(calls.len(), 1);
        assert!(
            observed(&result.summary)
                .as_str()
                .unwrap()
                .contains("previous checkpoint")
        );
        let prompt = serde_json::to_string(&calls[0].context["messages"]).unwrap();
        // Upstream regression test for #9652: clear boundaries and continuation wording.
        assert!(prompt.contains("# Conversation\\n[User]: Summarize this."));
        assert!(prompt.contains(
            "# Instructions\\nThe messages above are earlier context from an ongoing conversation."
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rejects_tool_calls_from_conversation_summaries() {
        let mock = SummaryMock::new();
        mock.once(tool_call_response(), DoneReason::ToolUse);
        let error = generate_summary_with_usage(
            &summary_messages(),
            &summary_model(false, 8192.0, None),
            2000.0,
            &options(),
            &mock.runtime(),
            None,
        )
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Summarization attempted to call a tool")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rejects_tool_calls_from_split_turn_summaries() {
        let mock = SummaryMock::new();
        mock.once(tool_call_response(), DoneReason::ToolUse);
        let error = compact(
            &split_preparation(None, false, 2000.0),
            &summary_model(false, 8192.0, None),
            &options(),
            &mock.runtime(),
            None,
        )
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Turn prefix summarization attempted to call a tool")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rejects_a_length_limited_history_summary() {
        let mock = SummaryMock::new();
        mock.once(length_response(), DoneReason::Length);
        let error = generate_summary_with_usage(
            &summary_messages(),
            &summary_model(false, 8192.0, None),
            2000.0,
            &options(),
            &mock.runtime(),
            None,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("generation hit the token cap"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rejects_a_length_limited_split_turn_summary() {
        let mock = SummaryMock::new();
        mock.once(length_response(), DoneReason::Length);
        let error = compact(
            &split_preparation(None, false, 2000.0),
            &summary_model(false, 8192.0, None),
            &options(),
            &mock.runtime(),
            None,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("generation hit the token cap"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn does_not_set_reasoning_when_thinking_is_off() {
        let mock = SummaryMock::new();
        let options = SummaryOptions {
            thinking_level: Some(ThinkingLevel::Off),
            ..options()
        };
        generate_summary(
            &summary_messages(),
            &summary_model(true, 8192.0, None),
            2000.0,
            &options,
            &mock.runtime(),
            None,
        )
        .await
        .unwrap();
        let calls = mock.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].options["apiKey"], "test-key");
        assert!(calls[0].options.get("reasoning").is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn does_not_set_reasoning_for_non_reasoning_models() {
        let mock = SummaryMock::new();
        let options = SummaryOptions {
            thinking_level: Some(ThinkingLevel::Medium),
            ..options()
        };
        generate_summary(
            &summary_messages(),
            &summary_model(false, 8192.0, None),
            2000.0,
            &options,
            &mock.runtime(),
            None,
        )
        .await
        .unwrap();
        let calls = mock.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].options["apiKey"], "test-key");
        assert!(calls[0].options.get("reasoning").is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn leaves_anthropic_refusal_fallback_handling_to_pi_ai_model_metadata() {
        let mock = SummaryMock::new();
        let model = summary_model(
            true,
            8192.0,
            Some(
                j!({"allowedFallbackModels":[{"provider":"anthropic","model":"claude-opus-4-8",
            "cost":{"input":5,"output":25,"cacheRead":0.5,"cacheWrite":6.25}}]}),
            ),
        );
        generate_summary(
            &summary_messages(),
            &model,
            2000.0,
            &options(),
            &mock.runtime(),
            None,
        )
        .await
        .unwrap();
        let calls = mock.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].options.get("refusalFallbacks").is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn does_not_set_anthropic_refusal_fallback_for_models_without_allowed_fallback_targets() {
        let mock = SummaryMock::new();
        generate_summary(
            &summary_messages(),
            &summary_model(true, 8192.0, None),
            2000.0,
            &options(),
            &mock.runtime(),
            None,
        )
        .await
        .unwrap();
        let calls = mock.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].options.get("refusalFallbacks").is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn clamps_compaction_summary_max_tokens_to_the_model_output_cap() {
        let mock = SummaryMock::new();
        let result = compact(
            &split_preparation(None, true, 500000.0),
            &summary_model(false, 128000.0, None),
            &options(),
            &mock.runtime(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(observed(&result.usage), usage(20.0, 20.0, 0.0, 0.0));
        let caps: Vec<_> = mock
            .calls()
            .iter()
            .map(|call| call.options["maxTokens"].clone())
            .collect();
        assert_eq!(caps, j!([128000, 128000]).as_array().unwrap().clone());
    }
}
