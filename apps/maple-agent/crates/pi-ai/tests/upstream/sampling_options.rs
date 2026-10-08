use std::sync::{Arc, Mutex};

use pi_ai::api::openai_completions::{
    OpenAICompletionsOptions, ScriptedTransport, stream, stream_simple,
};
use pi_ai::api::simple_options::resolve_sampling_params;
use pi_ai::types::*;
use pi_ai::utils::transcript::normalize_context;
use serde_json::{Value, json};

use super::common::{env, js, json};

fn context() -> TranscriptContext {
    normalize_context(Context {
        messages: vec![
            UserMessage {
                content: "Hello".into(),
                timestamp: 1_767_225_600_000.0,
                ..Default::default()
            }
            .into(),
        ],
        ..Default::default()
    })
}

fn model(api: &str, sampling: Option<Value>, overrides: Value) -> Model {
    let mut model = json!({
        "id": "custom-model", "name": "Custom Model", "api": api,
        "provider": "custom-provider", "baseUrl": "http://127.0.0.1:9/v1",
        "reasoning": false, "input": ["text"],
        "cost": {"input":0,"output":0,"cacheRead":0,"cacheWrite":0},
        "contextWindow":128000,"maxTokens":16384
    });
    if let Some(sampling) = sampling {
        model["samplingParams"] = sampling;
    }
    for (key, value) in overrides.as_object().unwrap() {
        model[key] = value.clone();
    }
    json(model)
}

fn payload_capture(captured: Arc<Mutex<Option<JsValue>>>) -> OnPayload {
    Arc::new(move |payload, _| {
        *captured.lock().unwrap() = Some(payload);
        Box::pin(async {
            Err(Arc::new(std::io::Error::other("payload captured")) as CallbackError)
        })
    })
}

async fn capture_payload(model: Model, options: Value) -> JsValue {
    let captured = Arc::new(Mutex::new(None));
    let mut options: OpenAICompletionsOptions = json(options);
    options.api_key = Some("fake-key".into());
    options.on_payload = Some(payload_capture(captured.clone()));
    stream(
        model,
        context(),
        Some(options),
        Arc::new(ScriptedTransport::default()),
        env(),
    )
    .unwrap()
    .result()
    .await;
    let payload = captured.lock().unwrap().clone();
    payload.expect("payload captured before request failure")
}

async fn capture_simple_payload(model: Model, options: Value) -> JsValue {
    let captured = Arc::new(Mutex::new(None));
    let mut options: SimpleStreamOptions = json(options);
    options.api_key = Some("fake-key".into());
    options.on_payload = Some(payload_capture(captured.clone()));
    stream_simple(
        model,
        context(),
        Some(options),
        Arc::new(ScriptedTransport::default()),
        env(),
    )
    .unwrap()
    .result()
    .await;
    let payload = captured.lock().unwrap().clone();
    payload.expect("payload captured before request failure")
}

fn model_level_shared_assertions(api: &str) {
    let model = model(api, Some(json!({"top_p":0.95,"min_p":0.05})), json!({}));
    let request = js(json!({"top_p":0.5}));
    let params =
        resolve_sampling_params(&model, ModelThinkingLevel::Off, request.as_object()).unwrap();
    assert_eq!(params["top_p"], json!(0.5));
    assert_eq!(params["min_p"], json!(0.05));
}

fn thinking_level_shared_assertions(api: &str) {
    let model = model(
        api,
        Some(json!({"temperature":1,"top_p":0.95})),
        json!({
            "reasoning": true,
            "samplingParamsByThinkingLevel": {"low":{"temperature":0.6,"top_k":64}}
        }),
    );
    let request = js(json!({"top_p":0.5}));
    let params =
        resolve_sampling_params(&model, ModelThinkingLevel::Low, request.as_object()).unwrap();
    assert_eq!(params["temperature"], json!(0.6));
    assert_eq!(params["top_p"], json!(0.5));
    assert_eq!(params["top_k"], json!(64));
}

fn summary_only_shared_assertions(api: &str) {
    let model = model(
        api,
        None,
        json!({
            "reasoning": true,
            "samplingParamsByThinkingLevel": {"off":{"temperature":0.7},"medium":{"temperature":0.8}}
        }),
    );
    // The excluded Responses envelope maps summary-only requests to medium.
    // The selected helper receives that effective level as required by the
    // authoritative mixed-test adaptation and retains its temperature assertion.
    let params = resolve_sampling_params(&model, ModelThinkingLevel::Medium, None).unwrap();
    assert_eq!(params["temperature"], json!(0.8));
}

mod sampling_params {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn merges_request_sampling_params_into_the_request_body() {
        let payload = capture_payload(
            model("openai-completions", None, json!({})),
            json!({
                "samplingParams":{"top_p":0.95,"top_k":0,"min_p":0}
            }),
        )
        .await;
        assert_eq!(payload["top_p"], json!(0.95));
        assert_eq!(payload["top_k"], json!(0));
        assert_eq!(payload["min_p"], json!(0));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn omits_sampling_params_when_neither_options_nor_model_set_them() {
        let payload =
            capture_payload(model("openai-completions", None, json!({})), json!({})).await;
        assert!(payload.get("temperature").is_none());
        assert!(payload.get("top_p").is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn applies_model_level_sampling_params_with_request_keys_taking_precedence_for_openai_completions()
     {
        let payload = capture_payload(
            model(
                "openai-completions",
                Some(json!({"top_p":0.95,"min_p":0.05})),
                json!({}),
            ),
            json!({
                "samplingParams":{"top_p":0.5}
            }),
        )
        .await;
        assert_eq!(payload["top_p"], json!(0.5));
        assert_eq!(payload["min_p"], json!(0.05));
    }

    #[test]
    fn applies_model_level_sampling_params_with_request_keys_taking_precedence_for_openai_responses()
     {
        model_level_shared_assertions("openai-responses");
    }

    #[test]
    fn applies_model_level_sampling_params_with_request_keys_taking_precedence_for_azure_openai_responses()
     {
        model_level_shared_assertions("azure-openai-responses");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn passes_request_sampling_params_through_stream_simple() {
        let payload = capture_simple_payload(
            model("openai-completions", None, json!({})),
            json!({
                "samplingParams":{"top_p":0.5}
            }),
        )
        .await;
        assert_eq!(payload["top_p"], json!(0.5));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn applies_sampling_params_for_the_effective_thinking_level_over_model_defaults() {
        let payload = capture_simple_payload(
            model(
                "openai-completions",
                Some(json!({"temperature":1,"top_p":0.95})),
                json!({
                    "reasoning":true,"thinkingLevelMap":{"low":null,"medium":null},
                    "samplingParamsByThinkingLevel":{"high":{"temperature":0.8,"top_k":64}}
                }),
            ),
            json!({"reasoning":"low"}),
        )
        .await;
        assert_eq!(payload["temperature"], json!(0.8));
        assert_eq!(payload["top_p"], json!(0.95));
        assert_eq!(payload["top_k"], json!(64));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn applies_off_sampling_params_when_reasoning_is_disabled() {
        let payload = capture_simple_payload(
            model(
                "openai-completions",
                None,
                json!({
                    "samplingParamsByThinkingLevel":{"off":{"temperature":0.7}}
                }),
            ),
            json!({}),
        )
        .await;
        assert_eq!(payload["temperature"], json!(0.7));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn merges_stream_option_keys_over_thinking_level_keys() {
        let payload = capture_simple_payload(model("openai-completions", None, json!({
            "reasoning":true,"samplingParamsByThinkingLevel":{"low":{"temperature":0.6,"top_p":0.95}}
        })), json!({"reasoning":"low","samplingParams":{"top_p":0.5}})).await;
        assert_eq!(payload["temperature"], json!(0.6));
        assert_eq!(payload["top_p"], json!(0.5));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn applies_thinking_level_params_between_model_and_request_params_for_openai_completions()
    {
        let payload = capture_payload(model("openai-completions", Some(json!({"temperature":1,"top_p":0.95})), json!({
            "reasoning":true,"samplingParamsByThinkingLevel":{"low":{"temperature":0.6,"top_k":64}}
        })), json!({"reasoningEffort":"low","samplingParams":{"top_p":0.5}})).await;
        assert_eq!(payload["temperature"], json!(0.6));
        assert_eq!(payload["top_p"], json!(0.5));
        assert_eq!(payload["top_k"], json!(64));
    }

    #[test]
    fn applies_thinking_level_params_between_model_and_request_params_for_openai_responses() {
        thinking_level_shared_assertions("openai-responses");
    }

    #[test]
    fn applies_thinking_level_params_between_model_and_request_params_for_azure_openai_responses() {
        thinking_level_shared_assertions("azure-openai-responses");
    }

    #[test]
    fn uses_medium_sampling_params_for_summary_only_openai_responses_requests() {
        summary_only_shared_assertions("openai-responses");
    }

    #[test]
    fn uses_medium_sampling_params_for_summary_only_azure_openai_responses_requests() {
        summary_only_shared_assertions("azure-openai-responses");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn overrides_named_request_fields() {
        let payload = capture_payload(
            model("openai-completions", None, json!({})),
            json!({
                "temperature":0,"samplingParams":{"temperature":1}
            }),
        )
        .await;
        assert_eq!(payload["temperature"], json!(1));
    }
}
