//! Selected request option contracts and pure helpers from Pi v1.0.4 `models.ts`.
use crate::env::CancellationToken;
use crate::types::{
    AnyModel, BoxFuture, CallbackError, DeferredCancelOptions, DeferredFetchOptions, Model,
    ModelThinkingLevel, ProviderHeaders, ProviderStreamOptions, SimpleStreamOptions, Usage,
    UsageCost,
};
use indexmap::IndexMap;
use std::sync::Arc;

#[derive(Clone, Debug, Default)]
pub struct ModelsRefreshOptions {
    pub allow_network: Option<bool>,
    pub providers: Option<Vec<String>>,
    pub force: Option<bool>,
    pub signal: Option<CancellationToken>,
}
#[derive(Clone, Debug, Default)]
pub struct ModelsRefreshResult {
    pub aborted: bool,
    pub errors: IndexMap<String, CallbackError>,
}
pub type TransformHeaders =
    Arc<dyn Fn(ProviderHeaders) -> BoxFuture<Result<ProviderHeaders, CallbackError>> + Send + Sync>;
#[derive(Clone, Default)]
pub struct ModelsRequestTransforms {
    pub transform_headers: Option<TransformHeaders>,
}
#[derive(Clone, Default)]
pub struct ModelsRequestOptions<T> {
    pub options: T,
    pub transforms: ModelsRequestTransforms,
}
impl<T> std::ops::Deref for ModelsRequestOptions<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.options
    }
}
impl<T> std::ops::DerefMut for ModelsRequestOptions<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.options
    }
}
pub type ModelsApiStreamOptions = ModelsRequestOptions<ProviderStreamOptions>;
pub type ModelsSimpleStreamOptions = ModelsRequestOptions<SimpleStreamOptions>;
pub type ModelsDeferredFetchOptions = ModelsRequestOptions<DeferredFetchOptions>;
pub type ModelsDeferredCancelOptions = ModelsRequestOptions<DeferredCancelOptions>;

pub fn has_api(model: &AnyModel, api: &str) -> bool {
    // Every selected model is a chat model; other model families are excluded.
    model.api == api
}

pub fn calculate_cost<'a>(model: &AnyModel, usage: &'a mut Usage) -> &'a UsageCost {
    let input_tokens = usage.input + usage.cache_read + usage.cache_write;
    let mut rates = &model.cost.rates;
    let mut matched_threshold = -1.0;
    if let Some(tiers) = &model.cost.tiers {
        for tier in tiers {
            if input_tokens > tier.input_tokens_above && tier.input_tokens_above > matched_threshold
            {
                rates = &tier.rates;
                matched_threshold = tier.input_tokens_above;
            }
        }
    }
    let long_write = usage.cache_write1h.unwrap_or(0.0);
    let short_write = usage.cache_write - long_write;
    usage.cost.input = (rates.input / 1_000_000.0) * usage.input;
    usage.cost.output = (rates.output / 1_000_000.0) * usage.output;
    usage.cost.cache_read = (rates.cache_read / 1_000_000.0) * usage.cache_read;
    usage.cost.cache_write =
        (rates.cache_write * short_write + rates.input * 2.0 * long_write) / 1_000_000.0;
    usage.cost.total =
        usage.cost.input + usage.cost.output + usage.cost.cache_read + usage.cost.cache_write;
    &usage.cost
}

const EXTENDED_THINKING_LEVELS: [ModelThinkingLevel; 7] = [
    ModelThinkingLevel::Off,
    ModelThinkingLevel::Minimal,
    ModelThinkingLevel::Low,
    ModelThinkingLevel::Medium,
    ModelThinkingLevel::High,
    ModelThinkingLevel::Xhigh,
    ModelThinkingLevel::Max,
];
pub fn get_supported_thinking_levels(model: &Model) -> Vec<ModelThinkingLevel> {
    if !model.reasoning {
        return vec![ModelThinkingLevel::Off];
    }
    EXTENDED_THINKING_LEVELS
        .into_iter()
        .filter(|level| {
            let mapped = model
                .thinking_level_map
                .as_ref()
                .and_then(|map| map.get(level));
            if matches!(mapped, Some(None)) {
                return false;
            }
            if matches!(level, ModelThinkingLevel::Xhigh | ModelThinkingLevel::Max) {
                return mapped.is_some();
            }
            true
        })
        .collect()
}

pub fn clamp_thinking_level(model: &Model, level: ModelThinkingLevel) -> ModelThinkingLevel {
    let available_levels = get_supported_thinking_levels(model);
    if available_levels.contains(&level) {
        return level;
    }
    // The Rust enum makes the TypeScript defensive unknown-string branch unreachable.
    let requested_index = EXTENDED_THINKING_LEVELS
        .iter()
        .position(|candidate| *candidate == level)
        .expect("thinking level must be represented in its ordering");
    for candidate in &EXTENDED_THINKING_LEVELS[requested_index..] {
        if available_levels.contains(candidate) {
            return *candidate;
        }
    }
    for candidate in EXTENDED_THINKING_LEVELS[..requested_index].iter().rev() {
        if available_levels.contains(candidate) {
            return *candidate;
        }
    }
    available_levels
        .first()
        .copied()
        .unwrap_or(ModelThinkingLevel::Off)
}

pub fn models_are_equal(a: Option<&AnyModel>, b: Option<&AnyModel>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.id == b.id && a.provider == b.provider,
        _ => false,
    }
}
