//! Pi v1.0.4 `api/simple-options.ts`; telemetry forwarding is excluded.
use crate::{models::clamp_thinking_level, types::*, utils::estimate::estimate_context_tokens};
pub const CONTEXT_SAFETY_TOKENS: f64 = 4096.0;
pub const MIN_MAX_TOKENS: f64 = 1.0;
pub const MIN_ANSWER_TOKENS: f64 = 1024.0;
pub fn clamp_max_tokens_to_context(
    model: &Model,
    context: &TranscriptContext,
    max_tokens: f64,
) -> Result<f64, JsString> {
    if model.context_window <= 0.0 {
        return Ok(js_max(MIN_MAX_TOKENS, max_tokens));
    }
    let available = model.context_window
        - estimate_context_tokens(&context.messages)?.tokens
        - CONTEXT_SAFETY_TOKENS;
    Ok(js_min(max_tokens, js_max(MIN_MAX_TOKENS, available)))
}
pub fn resolve_sampling_params(
    model: &Model,
    thinking_level: ModelThinkingLevel,
    request_params: Option<&SamplingParams>,
) -> Option<SamplingParams> {
    let effective = clamp_thinking_level(model, thinking_level);
    let level = model
        .sampling_params_by_thinking_level
        .as_ref()
        .and_then(|params| params.get(&effective));
    if model.sampling_params.is_none() && level.is_none() && request_params.is_none() {
        return None;
    }
    let mut out = SamplingParams::new();
    for params in [model.sampling_params.as_ref(), level, request_params]
        .into_iter()
        .flatten()
    {
        for (key, value) in params {
            out.insert(key, value.clone());
        }
    }
    Some(out)
}
pub fn build_base_options(
    model: &Model,
    context: &TranscriptContext,
    options: Option<&SimpleStreamOptions>,
    api_key: Option<&str>,
) -> Result<StreamOptions, JsString> {
    let mut base = options.map(|o| o.stream.clone()).unwrap_or_default();
    base.sampling_params = resolve_sampling_params(
        model,
        options
            .and_then(|o| o.reasoning)
            .map(Into::into)
            .unwrap_or(ModelThinkingLevel::Off),
        base.sampling_params.as_ref(),
    );
    base.max_tokens = Some(clamp_max_tokens_to_context(
        model,
        context,
        base.max_tokens.unwrap_or(model.max_tokens),
    )?);
    if let Some(key) = api_key.filter(|key| !key.is_empty()) {
        base.api_key = Some(key.to_owned());
    }
    Ok(base)
}
pub fn clamp_reasoning(effort: ThinkingLevel) -> ThinkingLevel {
    match effort {
        ThinkingLevel::Xhigh | ThinkingLevel::Max => ThinkingLevel::High,
        other => other,
    }
}
pub fn thinking_budget_for_level(level: ThinkingLevel, custom: Option<&ThinkingBudgets>) -> f64 {
    let budgets = custom.cloned().unwrap_or_default();
    match clamp_reasoning(level) {
        ThinkingLevel::Minimal => budgets.minimal.unwrap_or(1024.0),
        ThinkingLevel::Low => budgets.low.unwrap_or(2048.0),
        ThinkingLevel::Medium => budgets.medium.unwrap_or(8192.0),
        _ => budgets.high.unwrap_or(16384.0),
    }
}
pub fn clamp_thinking_budget_to_answer_room(budget: f64, ceiling: f64) -> f64 {
    js_min(budget, js_max(0.0, ceiling - MIN_ANSWER_TOKENS))
}
pub fn adjust_max_tokens_for_thinking(
    base: Option<f64>,
    model_max: f64,
    level: ThinkingLevel,
    custom: Option<&ThinkingBudgets>,
) -> (f64, f64) {
    let mut budget = thinking_budget_for_level(level, custom);
    let max = base.map_or(model_max, |base| js_min(base + budget, model_max));
    if max <= budget {
        budget = clamp_thinking_budget_to_answer_room(budget, max);
    }
    (max, budget)
}
pub(crate) fn js_min(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a == 0.0 && b == 0.0 {
        if a.is_sign_negative() || b.is_sign_negative() {
            -0.0
        } else {
            0.0
        }
    } else {
        a.min(b)
    }
}
pub(crate) fn js_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a == 0.0 && b == 0.0 {
        if a.is_sign_positive() || b.is_sign_positive() {
            0.0
        } else {
            -0.0
        }
    } else {
        a.max(b)
    }
}

#[cfg(test)]
mod numeric_boundaries {
    use super::*;
    #[test]
    fn thinking_clamp_preserves_javascript_nan_and_negative_zero() {
        assert!(clamp_thinking_budget_to_answer_room(f64::NAN, 2048.0).is_nan());
        assert!(clamp_thinking_budget_to_answer_room(1024.0, f64::NAN).is_nan());
        assert_eq!(
            clamp_thinking_budget_to_answer_room(-0.0, 0.0).to_bits(),
            (-0.0_f64).to_bits()
        );
    }
}
