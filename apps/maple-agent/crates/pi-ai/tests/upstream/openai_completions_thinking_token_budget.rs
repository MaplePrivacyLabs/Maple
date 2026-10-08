use super::common::*;
use pi_ai::api::openai_completions::{ScriptedTransport, stream_simple};
use pi_ai::types::{Model, ProviderResponse};
use pi_ai::utils::transcript::normalize_context;
use serde_json::{Value, json as j};
use std::sync::Arc;

fn vllm_model(compat: Option<Value>) -> Model {
    json(
        j!({"id":"zai-org/glm-5.2","name":"GLM 5.2 (local vLLM)","api":"openai-completions",
        "provider":"local-vllm","baseUrl":"http://localhost:8000/v1","reasoning":true,"input":["text"],
        "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"contextWindow":262144,"maxTokens":16384,
        "compat":compat.unwrap_or_else(||j!({"thinkingFormat":"zai","supportsThinkingTokenBudget":true}))}),
    )
}

async fn capture(model: Model, mut options: Value) -> Value {
    options["apiKey"] = j!("test");
    let transport = Arc::new(ScriptedTransport::default());
    transport.push_response(ProviderResponse { status:200,..Default::default() },[Ok(js(j!({
        "choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,
        "prompt_tokens_details":{"cached_tokens":0},"completion_tokens_details":{"reasoning_tokens":0}}
    })))]);
    let ctx = normalize_context(json(
        j!({"messages":[{"role":"user","content":"Hi","timestamp":0}]}),
    ));
    stream_simple(model, ctx, Some(json(options)), transport.clone(), env())
        .unwrap()
        .result()
        .await;
    let requests = transport.requests();
    assert_eq!(requests.len(), 1);
    serde_json::from_str(&pi_ai::utils::js_json::stringify(&requests[0].params)).unwrap()
}

// Retain both the upstream file and describe-block namespaces in coverage paths.
#[allow(clippy::module_inception)]
mod openai_completions_thinking_token_budget {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn sends_the_configured_budget_for_the_requested_level() {
        let params = capture(
            vllm_model(None),
            j!({"reasoning":"medium","thinkingBudgets":{"medium":4096}}),
        )
        .await;
        assert_eq!(params["thinking_token_budget"], 4096);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn omits_the_budget_when_neither_the_field_nor_the_alias_is_set() {
        let params = capture(
            vllm_model(Some(j!({"thinkingFormat":"zai"}))),
            j!({"reasoning":"medium","thinkingBudgets":{"medium":4096}}),
        )
        .await;
        for key in [
            "thinking_token_budget",
            "thinking_budget",
            "thinking_budget_tokens",
        ] {
            assert!(params.get(key).is_none(), "{key}");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn omits_the_budget_when_thinking_is_off() {
        let params = capture(vllm_model(None), j!({"thinkingBudgets":{"high":8192}})).await;
        assert!(params.get("thinking_token_budget").is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn clamps_xhigh_and_max_to_the_high_budget() {
        let xhigh = capture(
            vllm_model(None),
            j!({"reasoning":"xhigh","thinkingBudgets":{"high":8192}}),
        )
        .await;
        let max = capture(
            vllm_model(None),
            j!({"reasoning":"max","thinkingBudgets":{"high":8192}}),
        )
        .await;
        assert_eq!(xhigh["thinking_token_budget"], 8192);
        assert_eq!(max["thinking_token_budget"], 8192);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn leaves_room_for_the_answer_when_the_budget_meets_the_response_ceiling() {
        let params = capture(vllm_model(None), j!({"reasoning":"high"})).await;
        assert_eq!(params["thinking_token_budget"], 16384 - 1024);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn uses_the_caller_max_tokens_as_the_ceiling_when_it_is_lower_than_the_model_cap() {
        let params = capture(
            vllm_model(None),
            j!({"reasoning":"high","thinkingBudgets":{"high":8192},"maxTokens":4096}),
        )
        .await;
        assert_eq!(params["thinking_token_budget"], 4096 - 1024);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sends_thinking_budget_when_thinking_token_budget_field_is_set() {
        let params = capture(
            vllm_model(Some(
                j!({"thinkingFormat":"qwen","thinkingTokenBudgetField":"thinking_budget"}),
            )),
            j!({"reasoning":"medium","thinkingBudgets":{"medium":4096}}),
        )
        .await;
        assert_eq!(params["thinking_budget"], 4096);
        assert!(params.get("thinking_token_budget").is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sends_thinking_budget_tokens_when_thinking_token_budget_field_is_set() {
        let params = capture(
            vllm_model(Some(
                j!({"thinkingFormat":"qwen","thinkingTokenBudgetField":"thinking_budget_tokens"}),
            )),
            j!({"reasoning":"medium","thinkingBudgets":{"medium":4096}}),
        )
        .await;
        assert_eq!(params["thinking_budget_tokens"], 4096);
        assert!(params.get("thinking_token_budget").is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn lets_thinking_token_budget_field_win_over_the_boolean_alias() {
        let params = capture(vllm_model(Some(j!({"thinkingFormat":"zai","supportsThinkingTokenBudget":true,"thinkingTokenBudgetField":"thinking_budget"}))),j!({"reasoning":"medium","thinkingBudgets":{"medium":4096}})).await;
        assert_eq!(params["thinking_budget"], 4096);
        assert!(params.get("thinking_token_budget").is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn puts_the_clamped_budget_in_chat_template_kwargs_when_var_is_thinking_budget() {
        let params = capture(vllm_model(Some(j!({"thinkingFormat":"chat-template","chatTemplateKwargs":{"enable_thinking":{"$var":"thinking.enabled"},"thinking_budget":{"$var":"thinking.budget"}}}))),j!({"reasoning":"high"})).await;
        assert_eq!(
            params["chat_template_kwargs"],
            j!({"enable_thinking":true,"thinking_budget":16384-1024})
        );
        assert!(params.get("thinking_token_budget").is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn omits_thinking_budget_from_chat_template_kwargs_when_thinking_is_off() {
        let params = capture(vllm_model(Some(j!({"thinkingFormat":"chat-template","chatTemplateKwargs":{"enable_thinking":{"$var":"thinking.enabled"},"thinking_budget":{"$var":"thinking.budget"}}}))),j!({})).await;
        assert_eq!(
            params["chat_template_kwargs"],
            j!({"enable_thinking":false})
        );
    }
}
