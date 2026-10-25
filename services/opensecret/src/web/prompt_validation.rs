//! Reject-only admission for qualified raw Chat prompts.
//!
//! Unlike Responses history fitting, this boundary never truncates, normalizes
//! or rewrites caller-owned messages, tools, reasoning or generation options.

use crate::{
    model_config::{
        DEEPSEEK_V4_1_FLASH_MODEL_ID, GLM_5_3_FLASH_MODEL_ID, GLM_5_3_MODEL_ID, KIMI_K3_MODEL_ID,
        QUICK_MODEL_ID,
    },
    provider_registry::{ProviderId, PROVIDER_REGISTRY},
    tokens::model_max_ctx,
    ApiError,
};
use model_tokenizer::{
    count_chat_request, PromptAccuracy, PromptCount, RenderContext, RenderError, RendererId,
};
use serde_json::Value;
use tracing::error;

/// Explicit serving profiles; a model's HF reference is not interchangeable
/// with a provider's template. Unsupported provider/model pairs stay uncounted.
pub(crate) fn renderer_for_provider(model: &str, provider: ProviderId) -> Option<RendererId> {
    match (model, provider) {
        (DEEPSEEK_V4_1_FLASH_MODEL_ID, ProviderId::Tinfoil | ProviderId::Maple(_)) => {
            Some(RendererId::DeepSeekV41Flash1017)
        }
        (GLM_5_3_MODEL_ID, ProviderId::Tinfoil) => Some(RendererId::Glm53TinfoilNvfp4),
        (GLM_5_3_MODEL_ID, ProviderId::Continuum) => Some(RendererId::Glm53Continuum),
        (GLM_5_3_FLASH_MODEL_ID, ProviderId::Continuum) => Some(RendererId::Glm53FlashContinuum),
        (QUICK_MODEL_ID, ProviderId::Tinfoil) => Some(RendererId::GptOss120bHarmony),
        ("gpt-oss-safeguard-120b", ProviderId::Tinfoil) => {
            Some(RendererId::GptOssSafeguard120bHarmony)
        }
        (KIMI_K3_MODEL_ID, ProviderId::Tinfoil) => Some(RendererId::KimiK3),
        ("gemma4-31b", ProviderId::Tinfoil) => Some(RendererId::Gemma4Tinfoil),
        ("llama3-3-70b", ProviderId::Tinfoil) => Some(RendererId::Llama33Tinfoil),
        _ => None,
    }
}

/// Candidate profiles for a conservative cross-provider Responses bound. This
/// uses each provider's pinned source profile, not an interchangeable HF
/// reference. Request forms outside those text profiles remain explicit.
pub(crate) fn renderers_for_model(model: &str) -> Vec<RendererId> {
    let Some(model) = PROVIDER_REGISTRY.completion_model(model) else {
        return Vec::new();
    };
    let mut renderers: Vec<RendererId> = Vec::new();
    for route in model.routes {
        if !route.enabled
            || route.weight == 0
            || !PROVIDER_REGISTRY
                .provider(route.provider)
                .is_some_and(|provider| provider.enabled && provider.weight > 0)
        {
            continue;
        }
        if let Some(renderer) = renderer_for_provider(model.public_model_id, route.provider) {
            if !renderers
                .iter()
                .any(|existing| existing.profile() == renderer.profile())
            {
                renderers.push(renderer);
            }
        }
    }
    renderers
}

pub(crate) fn supports_counted_prompt(model: &str) -> bool {
    matches!(
        model,
        DEEPSEEK_V4_1_FLASH_MODEL_ID
            | GLM_5_3_MODEL_ID
            | GLM_5_3_FLASH_MODEL_ID
            | KIMI_K3_MODEL_ID
            | QUICK_MODEL_ID
            | "gpt-oss-safeguard-120b"
            | "gemma4-31b"
            | "llama3-3-70b"
    )
}

pub(crate) async fn validate_chat_prompt(
    model: &str,
    provider: ProviderId,
    request: Value,
) -> Result<Value, ApiError> {
    let Some(renderer) = renderer_for_provider(model, provider) else {
        return Ok(request);
    };
    let model = model.to_string();

    // Rendering and encoding large prompts must not occupy a Tokio worker.
    // The pinned text profile is a counting qualification, not an assertion
    // about multimedia expansion or a mutable provider deployment.
    tokio::task::spawn_blocking(move || {
        let context = RenderContext::default();
        validate_with_count(&model, provider, request, |renderer, request| {
            count_chat_request(renderer, request, &context)
        })
    })
    .await
    .map_err(|_| {
        error!(
            category = "prompt_count_worker_failed",
            profile = renderer.profile(),
            "Raw chat prompt validation failed"
        );
        ApiError::InternalServerError
    })?
}

fn validate_with_count(
    model: &str,
    provider: ProviderId,
    request: Value,
    count: impl FnOnce(RendererId, &Value) -> Result<PromptCount, RenderError>,
) -> Result<Value, ApiError> {
    let Some(renderer) = renderer_for_provider(model, provider) else {
        return Ok(request);
    };
    let Some(reserve) = output_reservation(&request) else {
        // Invalid caller caps retain the provider's existing validation path.
        return Ok(request);
    };
    let prompt_count = match count(renderer, &request) {
        Ok(count) => count,
        Err(RenderError::Unsupported(_) | RenderError::InvalidRequest(_)) => {
            // Unqualified media, custom rendering and malformed caller forms
            // remain provider-handled; no approximate count can reject them.
            return Ok(request);
        }
        Err(RenderError::Tokenizer(_)) => {
            error!(
                category = "pinned_tokenizer_failed",
                profile = renderer.profile(),
                "Raw chat prompt validation failed"
            );
            return Err(ApiError::InternalServerError);
        }
    };
    if prompt_count.profile != renderer.profile()
        || prompt_count.accuracy != PromptAccuracy::ExactForPinnedTextProfile
    {
        error!(
            category = "prompt_profile_mismatch",
            profile = renderer.profile(),
            "Raw chat prompt validation failed"
        );
        return Err(ApiError::InternalServerError);
    }
    if prompt_count
        .tokens
        .checked_add(reserve)
        .is_none_or(|total| total > model_max_ctx(model))
    {
        return Err(ApiError::MessageExceedsContextLimit);
    }
    Ok(request)
}

/// A known valid explicit cap, or one generation token when none was supplied.
/// `None` means leave an invalid selected cap to existing provider validation.
fn output_reservation(request: &Value) -> Option<usize> {
    let cap = request
        .get("max_completion_tokens")
        .filter(|value| !value.is_null())
        .or_else(|| request.get("max_tokens").filter(|value| !value.is_null()));
    match cap {
        None => Some(1),
        Some(value) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .map(|value| value.max(1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_registry::MapleInstanceId;
    use serde_json::json;
    use std::{cell::Cell, collections::BTreeSet};

    const MODELS: [(&str, ProviderId, RendererId); 8] = [
        (
            DEEPSEEK_V4_1_FLASH_MODEL_ID,
            ProviderId::Tinfoil,
            RendererId::DeepSeekV41Flash1017,
        ),
        (
            GLM_5_3_MODEL_ID,
            ProviderId::Tinfoil,
            RendererId::Glm53TinfoilNvfp4,
        ),
        (
            GLM_5_3_FLASH_MODEL_ID,
            ProviderId::Continuum,
            RendererId::Glm53FlashContinuum,
        ),
        (KIMI_K3_MODEL_ID, ProviderId::Tinfoil, RendererId::KimiK3),
        (
            QUICK_MODEL_ID,
            ProviderId::Tinfoil,
            RendererId::GptOss120bHarmony,
        ),
        (
            "gpt-oss-safeguard-120b",
            ProviderId::Tinfoil,
            RendererId::GptOssSafeguard120bHarmony,
        ),
        ("gemma4-31b", ProviderId::Tinfoil, RendererId::Gemma4Tinfoil),
        (
            "llama3-3-70b",
            ProviderId::Tinfoil,
            RendererId::Llama33Tinfoil,
        ),
    ];

    fn request(model: &str) -> Value {
        json!({
            "model": model,
            "messages": [
                {"role": "user", "content": "Please preserve this exact text."},
                {"role": "assistant", "reasoning": "Keep this historical reasoning.",
                 "tool_calls": [{"id": "synthetic-call", "type": "function", "function": {
                     "name": "lookup", "arguments": "{\"query\":\"synthetic\"}"
                 }}]},
                {"role": "tool", "tool_call_id": "synthetic-call", "content": "Synthetic result."},
                {"role": "user", "content": "Keep the entire supplied history."}
            ],
            "tools": [{"type": "function", "function": {
                "name": "lookup", "parameters": {"type": "object", "properties": {}}
            }}],
            "reasoning_effort": "high", "temperature": 0.6, "max_tokens": 64,
            "chat_template_kwargs": {"drop_thinking": false}
        })
    }

    fn counted(renderer: RendererId, tokens: usize) -> Result<PromptCount, RenderError> {
        Ok(PromptCount {
            tokens,
            profile: renderer.profile(),
            accuracy: PromptAccuracy::ExactForPinnedTextProfile,
        })
    }

    #[test]
    fn all_eight_models_and_enabled_registered_text_routes_have_explicit_profiles() {
        let expected = MODELS
            .iter()
            .map(|(model, _, _)| *model)
            .collect::<BTreeSet<_>>();
        let registered = PROVIDER_REGISTRY
            .completion_models()
            .iter()
            .map(|model| model.public_model_id)
            .collect::<BTreeSet<_>>();
        assert_eq!(registered, expected);
        for model in PROVIDER_REGISTRY.completion_models() {
            assert!(supports_counted_prompt(model.public_model_id));
            assert!(!renderers_for_model(model.public_model_id).is_empty());
            for route in model
                .routes
                .iter()
                .filter(|route| route.enabled && route.weight > 0)
            {
                if PROVIDER_REGISTRY
                    .provider(route.provider)
                    .is_some_and(|provider| provider.enabled && provider.weight > 0)
                {
                    assert!(
                        renderer_for_provider(model.public_model_id, route.provider).is_some(),
                        "missing renderer: {} / {}",
                        model.public_model_id,
                        route.provider.as_str()
                    );
                }
            }
        }
        for (model, provider, renderer) in MODELS {
            assert_eq!(renderer_for_provider(model, provider), Some(renderer));
        }
    }

    #[test]
    fn profiles_follow_actual_provider_topology_and_deduplicate_maple_replicas() {
        for instance in MapleInstanceId::ALL {
            assert_eq!(
                renderer_for_provider(DEEPSEEK_V4_1_FLASH_MODEL_ID, ProviderId::Maple(instance)),
                Some(RendererId::DeepSeekV41Flash1017)
            );
        }
        assert_eq!(
            renderers_for_model(DEEPSEEK_V4_1_FLASH_MODEL_ID),
            vec![RendererId::DeepSeekV41Flash1017]
        );
        assert_eq!(
            renderers_for_model(GLM_5_3_MODEL_ID),
            vec![RendererId::Glm53Continuum, RendererId::Glm53TinfoilNvfp4]
        );
        assert_eq!(
            renderers_for_model(GLM_5_3_FLASH_MODEL_ID),
            vec![RendererId::Glm53FlashContinuum]
        );
        assert_eq!(
            renderer_for_provider(GLM_5_3_FLASH_MODEL_ID, ProviderId::Tinfoil),
            None
        );
        assert_eq!(
            renderer_for_provider(KIMI_K3_MODEL_ID, ProviderId::Maple(MapleInstanceId::I01)),
            None
        );
        assert_eq!(
            renderer_for_provider(DEEPSEEK_V4_1_FLASH_MODEL_ID, ProviderId::Continuum),
            None
        );
        assert_eq!(
            renderer_for_provider(QUICK_MODEL_ID, ProviderId::Continuum),
            None
        );
    }

    #[test]
    fn deployment_profiles_do_not_silently_use_official_or_jinja_references() {
        assert_ne!(
            renderer_for_provider(GLM_5_3_MODEL_ID, ProviderId::Tinfoil),
            Some(RendererId::Glm53Official)
        );
        assert_ne!(
            renderer_for_provider(GLM_5_3_MODEL_ID, ProviderId::Continuum),
            Some(RendererId::Glm53Official)
        );
        assert_ne!(
            renderer_for_provider(GLM_5_3_FLASH_MODEL_ID, ProviderId::Continuum),
            Some(RendererId::Glm53FlashOfficial)
        );
        assert_ne!(
            renderer_for_provider(QUICK_MODEL_ID, ProviderId::Tinfoil),
            Some(RendererId::GptOss120bJinjaReference)
        );
        assert_ne!(
            renderer_for_provider("gpt-oss-safeguard-120b", ProviderId::Tinfoil),
            Some(RendererId::GptOssSafeguard120bJinjaReference)
        );
    }

    #[test]
    fn exact_model_windows_are_admitted_and_one_token_over_is_rejected_for_all_eight() {
        for (model, provider, renderer) in MODELS {
            let original = request(model);
            let window = model_max_ctx(model);
            let admitted =
                validate_with_count(model, provider, original.clone(), |selected, body| {
                    assert_eq!(selected, renderer);
                    assert_eq!(body, &original);
                    counted(selected, window - 64)
                })
                .unwrap();
            assert_eq!(admitted, original);
            assert!(
                matches!(
                    validate_with_count(model, provider, original, |selected, _| counted(
                        selected,
                        window - 63
                    )),
                    Err(ApiError::MessageExceedsContextLimit)
                ),
                "overflow must use {model}'s own context window"
            );
        }
    }

    #[test]
    fn absent_null_and_zero_caps_reserve_one_without_changing_history_tools_or_reasoning() {
        for (model, provider, _) in MODELS {
            for cap in [None, Some(Value::Null), Some(json!(0))] {
                let mut original = request(model);
                match cap {
                    Some(value) => original["max_tokens"] = value,
                    None => {
                        original.as_object_mut().unwrap().remove("max_tokens");
                    }
                }
                original["max_completion_tokens"] = Value::Null;
                let window = model_max_ctx(model);
                let output =
                    validate_with_count(model, provider, original.clone(), |renderer, _| {
                        counted(renderer, window - 1)
                    })
                    .unwrap();
                assert_eq!(output, original);
                assert!(matches!(
                    validate_with_count(model, provider, original, |renderer, _| counted(
                        renderer, window
                    )),
                    Err(ApiError::MessageExceedsContextLimit)
                ));
            }
        }
    }

    #[test]
    fn completion_cap_precedes_legacy_cap_and_null_falls_back_for_all_eight() {
        for (model, provider, _) in MODELS {
            let mut original = request(model);
            let window = model_max_ctx(model);
            original["max_tokens"] = json!(window);
            original["max_completion_tokens"] = json!(16);
            let output = validate_with_count(model, provider, original.clone(), |renderer, _| {
                counted(renderer, window - 16)
            })
            .unwrap();
            assert_eq!(output, original);
            original["max_completion_tokens"] = Value::Null;
            assert!(matches!(
                validate_with_count(model, provider, original, |renderer, _| counted(
                    renderer, 1
                )),
                Err(ApiError::MessageExceedsContextLimit)
            ));
        }
    }

    #[test]
    fn large_valid_caps_and_count_overflow_are_rejected_without_lowering_caps() {
        for (model, provider, _) in MODELS {
            let mut original = request(model);
            original["max_completion_tokens"] = json!(u64::MAX);
            let before = original.clone();
            assert!(matches!(
                validate_with_count(model, provider, original.clone(), |renderer, _| counted(
                    renderer, 1
                )),
                Err(ApiError::MessageExceedsContextLimit)
            ));
            assert_eq!(original, before);
            assert!(matches!(
                validate_with_count(model, provider, request(model), |renderer, _| counted(
                    renderer,
                    usize::MAX
                )),
                Err(ApiError::MessageExceedsContextLimit)
            ));
        }
    }

    #[test]
    fn invalid_selected_cap_types_are_provider_handled_without_counting_for_all_eight() {
        for (model, provider, _) in MODELS {
            for invalid in [
                json!(-1),
                json!(1.5),
                json!("64"),
                json!(false),
                json!([]),
                json!({}),
            ] {
                for field in ["max_tokens", "max_completion_tokens"] {
                    let mut original = request(model);
                    original[field] = invalid.clone();
                    let calls = Cell::new(0);
                    let output =
                        validate_with_count(model, provider, original.clone(), |renderer, _| {
                            calls.set(calls.get() + 1);
                            counted(renderer, usize::MAX)
                        })
                        .unwrap();
                    assert_eq!(calls.get(), 0);
                    assert_eq!(output, original);
                }
            }
        }
    }

    #[test]
    fn unsupported_media_and_invalid_renderer_forms_remain_unchanged_for_all_profiles() {
        for (model, provider, _) in MODELS {
            let mut original = request(model);
            original["messages"] = json!([{"role": "user", "content": [{
                "type": "image_url", "image_url": {"url": "https://example.invalid/synthetic.png"}
            }]}]);
            for error in [
                RenderError::Unsupported("synthetic unqualified feature"),
                RenderError::InvalidRequest("synthetic malformed caller form"),
            ] {
                let output =
                    validate_with_count(model, provider, original.clone(), |_, _| Err(error))
                        .unwrap();
                assert_eq!(output, original);
            }
        }
    }

    #[test]
    fn unqualified_provider_pairs_and_unresolved_selectors_do_not_count_or_change_requests() {
        for (model, provider) in [
            ("auto:quick", ProviderId::Tinfoil),
            ("auto:powerful", ProviderId::Tinfoil),
            ("deepseek-v4-1-flash-1017", ProviderId::Tinfoil),
            ("unknown", ProviderId::Tinfoil),
            (GLM_5_3_FLASH_MODEL_ID, ProviderId::Tinfoil),
            (QUICK_MODEL_ID, ProviderId::Continuum),
        ] {
            let original = request(model);
            let output = validate_with_count(model, provider, original.clone(), |_, _| {
                panic!("unsupported pairs must not count")
            })
            .unwrap();
            assert_eq!(output, original);
        }
        for model in [
            "auto:quick",
            "auto:powerful",
            "deepseek-v4-1-flash-1017",
            "unknown",
        ] {
            assert!(!supports_counted_prompt(model));
            assert!(renderers_for_model(model).is_empty());
        }
    }

    #[test]
    fn tokenizer_failure_fails_closed_for_all_provider_profiles() {
        for (model, provider, _) in MODELS {
            assert!(matches!(
                validate_with_count(model, provider, request(model), |_, _| {
                    Err(RenderError::Tokenizer("synthetic internal failure"))
                }),
                Err(ApiError::InternalServerError)
            ));
        }
    }

    #[test]
    fn a_count_from_another_template_profile_cannot_admit_a_provider_request() {
        for (model, provider, reference) in [
            (
                GLM_5_3_MODEL_ID,
                ProviderId::Tinfoil,
                RendererId::Glm53Official,
            ),
            (
                GLM_5_3_MODEL_ID,
                ProviderId::Continuum,
                RendererId::Glm53Official,
            ),
            (
                GLM_5_3_FLASH_MODEL_ID,
                ProviderId::Continuum,
                RendererId::Glm53FlashOfficial,
            ),
            (
                QUICK_MODEL_ID,
                ProviderId::Tinfoil,
                RendererId::GptOss120bJinjaReference,
            ),
            (
                "gpt-oss-safeguard-120b",
                ProviderId::Tinfoil,
                RendererId::GptOssSafeguard120bJinjaReference,
            ),
        ] {
            assert!(matches!(
                validate_with_count(model, provider, request(model), |_, _| counted(
                    reference, 1
                )),
                Err(ApiError::InternalServerError)
            ));
        }
    }

    #[tokio::test]
    async fn all_compiled_provider_profiles_count_actual_text_without_silent_passthrough() {
        for (model, provider, renderer) in MODELS {
            let mut original = json!({"model": model, "messages": [{"role": "user", "content": "Hello."}], "max_tokens": 16});
            let count = count_chat_request(renderer, &original, &RenderContext::default())
                .unwrap_or_else(|_| panic!("compiled text profile must count: {renderer:?}"));
            assert!(count.tokens > 0);
            assert_eq!(count.profile, renderer.profile());
            let remaining = model_max_ctx(model) - count.tokens;
            original["max_tokens"] = json!(remaining);
            let output = validate_chat_prompt(model, provider, original.clone())
                .await
                .unwrap();
            assert_eq!(output, original);
            original["max_tokens"] = json!(remaining + 1);
            assert!(
                matches!(
                    validate_chat_prompt(model, provider, original).await,
                    Err(ApiError::MessageExceedsContextLimit)
                ),
                "{model} / {provider:?}"
            );
        }
    }

    #[tokio::test]
    async fn actual_deepseek_small_text_is_counted_and_unqualified_forms_pass_through() {
        let text = request(DEEPSEEK_V4_1_FLASH_MODEL_ID);
        assert!(
            count_chat_request(
                RendererId::DeepSeekV41Flash1017,
                &text,
                &RenderContext::default()
            )
            .expect("small text uses its pinned renderer")
            .tokens
                > 0
        );
        let mut media = text.clone();
        media["messages"] = json!([{"role": "user", "content": [{
            "type": "image_url", "image_url": {"url": "https://example.invalid/synthetic.png"}
        }]}]);
        let mut server_truncation = text.clone();
        server_truncation["truncate_prompt_tokens"] = json!(64);
        for original in [text, media, server_truncation] {
            let output = validate_chat_prompt(
                DEEPSEEK_V4_1_FLASH_MODEL_ID,
                ProviderId::Tinfoil,
                original.clone(),
            )
            .await
            .unwrap();
            assert_eq!(output, original);
        }
    }
}
