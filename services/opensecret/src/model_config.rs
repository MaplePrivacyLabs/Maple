//! Central model-specific configuration and public model catalog.

use crate::os_flags::PAID_POWERFUL_GLM_5_3_ALIAS_FLAG_KEY;
use serde_json::{json, Value};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelConfig {
    pub context_window: usize,
    pub responses: ResponsesModelConfig,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResponsesModelConfig {
    pub sampling: SamplingConfig,
    pub include_reasoning: bool,
    pub enable_thinking: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SamplingConfig {
    pub temperature: f32,
    pub top_p: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningHistoryStrategy {
    KimiPreserveThinking,
    GlmClearThinking,
}

impl SamplingConfig {
    pub fn with_overrides(self, temperature: Option<f32>, top_p: Option<f32>) -> Self {
        Self {
            temperature: temperature.unwrap_or(self.temperature),
            top_p: top_p.unwrap_or(self.top_p),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ModelConfigEntry {
    id: &'static str,
    provider_id: &'static str,
    catalog_provider: &'static str,
    catalog_provider_id: &'static str,
    display_name: &'static str,
    short_name: &'static str,
    description: &'static str,
    access: ModelAccessTier,
    capabilities: ModelCapabilities,
    badges: &'static [&'static str],
    listed: bool,
    api_listed: bool,
    enabled: bool,
    deprecated: bool,
    sort_order: u16,
    config: ModelConfig,
    catalog_metadata: Option<ModelCatalogMetadata>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelAccessTier {
    Free,
    Starter,
    Pro,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ModelPlan {
    #[default]
    Free,
    Paid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelCapabilities {
    pub chat: bool,
    pub vision: bool,
    pub reasoning: bool,
    pub tool_use: bool,
}

#[derive(Debug, Clone, Copy)]
struct ModelCatalogMetadata {
    input_modalities: &'static [&'static str],
    output_modalities: &'static [&'static str],
    parameter_size: Option<&'static str>,
    active_parameter_size: Option<&'static str>,
}

#[derive(Debug, Clone, Copy)]
struct ModelAliasEntry {
    id: &'static str,
    label: &'static str,
    short_name: &'static str,
    description: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ModelAliasTargets {
    quick: &'static str,
    powerful: &'static str,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PaidModelAliasOverrides {
    powerful_glm_5_3: bool,
}

pub const DEFAULT_CONTEXT_WINDOW: usize = 64_000;
pub const DEFAULT_TEMPERATURE: f32 = 0.7;
pub const DEFAULT_TOP_P: f32 = 1.0;
pub const AUTO_QUICK_MODEL_ID: &str = "auto:quick";
pub const AUTO_POWERFUL_MODEL_ID: &str = "auto:powerful";
pub const QUICK_MODEL_ID: &str = "gpt-oss-120b";
pub const GLM_5_3_MODEL_ID: &str = "glm-5-3";
pub const GLM_5_3_FLASH_MODEL_ID: &str = "glm-5-3-flash";
pub const POWERFUL_MODEL_ID: &str = GLM_5_3_MODEL_ID;
pub const KIMI_K3_MODEL_ID: &str = "kimi-k3";
pub const KIMI_K2_6_MODEL_ID: &str = "kimi-k2-6";
pub const DEEPSEEK_V4_FLASH_MODEL_ID: &str = "deepseek-v4-flash";
pub const DEEPSEEK_V4_1_FLASH_MODEL_ID: &str = "deepseek-v4-1-flash";

const FREE_MODEL_ALIAS_TARGETS: ModelAliasTargets = ModelAliasTargets {
    quick: QUICK_MODEL_ID,
    powerful: POWERFUL_MODEL_ID,
};

const PAID_MODEL_ALIAS_TARGETS: ModelAliasTargets = ModelAliasTargets {
    quick: DEEPSEEK_V4_1_FLASH_MODEL_ID,
    powerful: POWERFUL_MODEL_ID,
};

const DEFAULT_SAMPLING_CONFIG: SamplingConfig = SamplingConfig {
    temperature: DEFAULT_TEMPERATURE,
    top_p: DEFAULT_TOP_P,
};

const DEFAULT_RESPONSES_MODEL_CONFIG: ResponsesModelConfig = ResponsesModelConfig {
    sampling: DEFAULT_SAMPLING_CONFIG,
    include_reasoning: false,
    enable_thinking: false,
};

impl ModelConfig {
    const fn new(context_window: usize) -> Self {
        Self {
            context_window,
            responses: DEFAULT_RESPONSES_MODEL_CONFIG,
        }
    }

    const fn with_responses(context_window: usize, responses: ResponsesModelConfig) -> Self {
        Self {
            context_window,
            responses,
        }
    }
}

impl ModelConfigEntry {
    #[allow(clippy::too_many_arguments)]
    const fn new(
        id: &'static str,
        display_name: &'static str,
        short_name: &'static str,
        description: &'static str,
        access: ModelAccessTier,
        capabilities: ModelCapabilities,
        badges: &'static [&'static str],
        listed: bool,
        enabled: bool,
        deprecated: bool,
        sort_order: u16,
        context_window: usize,
    ) -> Self {
        Self {
            id,
            provider_id: id,
            catalog_provider: "tinfoil",
            catalog_provider_id: id,
            display_name,
            short_name,
            description,
            access,
            capabilities,
            badges,
            listed,
            api_listed: listed,
            enabled,
            deprecated,
            sort_order,
            config: ModelConfig::new(context_window),
            catalog_metadata: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    const fn with_responses(
        id: &'static str,
        display_name: &'static str,
        short_name: &'static str,
        description: &'static str,
        access: ModelAccessTier,
        capabilities: ModelCapabilities,
        badges: &'static [&'static str],
        listed: bool,
        enabled: bool,
        deprecated: bool,
        sort_order: u16,
        context_window: usize,
        responses: ResponsesModelConfig,
    ) -> Self {
        Self {
            id,
            provider_id: id,
            catalog_provider: "tinfoil",
            catalog_provider_id: id,
            display_name,
            short_name,
            description,
            access,
            capabilities,
            badges,
            listed,
            api_listed: listed,
            enabled,
            deprecated,
            sort_order,
            config: ModelConfig::with_responses(context_window, responses),
            catalog_metadata: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    const fn api_only(
        id: &'static str,
        display_name: &'static str,
        short_name: &'static str,
        description: &'static str,
        access: ModelAccessTier,
        capabilities: ModelCapabilities,
        badges: &'static [&'static str],
        enabled: bool,
        deprecated: bool,
        sort_order: u16,
        context_window: usize,
    ) -> Self {
        Self {
            id,
            provider_id: id,
            catalog_provider: "tinfoil",
            catalog_provider_id: id,
            display_name,
            short_name,
            description,
            access,
            capabilities,
            badges,
            listed: false,
            api_listed: true,
            enabled,
            deprecated,
            sort_order,
            config: ModelConfig::new(context_window),
            catalog_metadata: None,
        }
    }

    const fn with_catalog_provider(
        mut self,
        provider: &'static str,
        provider_id: &'static str,
    ) -> Self {
        self.catalog_provider = provider;
        self.catalog_provider_id = provider_id;
        self
    }

    const fn with_catalog_metadata(mut self, metadata: ModelCatalogMetadata) -> Self {
        self.catalog_metadata = Some(metadata);
        self
    }

    fn catalog_json(self) -> Value {
        let mut value = json!({
            "id": self.id,
            "object": "model",
            "created": 0,
            "owned_by": "opensecret",
            "provider": self.catalog_provider,
            "provider_id": self.catalog_provider_id,
            "display_name": self.display_name,
            "short_name": self.short_name,
            "description": self.description,
            "context_window": self.config.context_window,
            "max_context_tokens": self.config.context_window,
            "access": self.access.as_str(),
            "capabilities": self.capabilities.json(),
            "tasks": self.tasks(),
            "badges": self.badges,
            "enabled": self.enabled,
            "deprecated": self.deprecated,
            "sort_order": self.sort_order,
        });

        if let Some(metadata) = self.catalog_metadata {
            value["input_modalities"] = json!(metadata.input_modalities);
            value["output_modalities"] = json!(metadata.output_modalities);
            if let Some(parameter_size) = metadata.parameter_size {
                value["parameter_size"] = json!(parameter_size);
            }
            if let Some(active_parameter_size) = metadata.active_parameter_size {
                value["active_parameter_size"] = json!(active_parameter_size);
            }
        }

        value
    }

    fn openai_model_json(self) -> Value {
        json!({
            "id": self.id,
            "object": "model",
            "created": 0,
            "owned_by": "opensecret",
            "tasks": self.tasks(),
            "display_name": self.display_name,
            "short_name": self.short_name,
            "context_window": self.config.context_window,
            "max_context_tokens": self.config.context_window,
            "access": self.access.as_str(),
            "capabilities": self.capabilities.json(),
            "badges": self.badges,
        })
    }

    fn tasks(self) -> Vec<&'static str> {
        let mut tasks = vec!["generate"];
        if self.capabilities.vision {
            tasks.push("vision");
        }
        tasks
    }
}

impl ModelCatalogMetadata {
    const fn new(
        input_modalities: &'static [&'static str],
        output_modalities: &'static [&'static str],
        parameter_size: Option<&'static str>,
        active_parameter_size: Option<&'static str>,
    ) -> Self {
        Self {
            input_modalities,
            output_modalities,
            parameter_size,
            active_parameter_size,
        }
    }
}

impl ModelAccessTier {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Free => "free",
            Self::Starter => "starter",
            Self::Pro => "pro",
        }
    }

    const fn is_available_to(self, plan: ModelPlan) -> bool {
        matches!(self, Self::Free) || matches!(plan, ModelPlan::Paid)
    }
}

impl ModelPlan {
    pub(crate) const fn from_is_paid(is_paid: bool) -> Self {
        if is_paid {
            Self::Paid
        } else {
            Self::Free
        }
    }

    pub(crate) const fn is_paid(self) -> bool {
        matches!(self, Self::Paid)
    }

    pub(crate) fn allows_model(self, model: &str) -> bool {
        let canonical = alias_target(model).unwrap_or(model);
        model_entry(canonical).is_none_or(|entry| entry.access.is_available_to(self))
    }
}

impl ModelAliasTargets {
    pub(crate) const fn for_plan(plan: ModelPlan) -> Self {
        match plan {
            ModelPlan::Free => FREE_MODEL_ALIAS_TARGETS,
            ModelPlan::Paid => PAID_MODEL_ALIAS_TARGETS,
        }
    }

    /// Router v2 uses fixed alias policy rather than legacy model-selector flags.
    /// Resolving an alias does not grant access to its target model.
    pub(crate) const fn for_router_v2(plan: ModelPlan) -> Self {
        Self {
            powerful: GLM_5_3_MODEL_ID,
            ..Self::for_plan(plan)
        }
    }

    pub(crate) const fn for_plan_with_overrides(
        plan: ModelPlan,
        overrides: PaidModelAliasOverrides,
    ) -> Self {
        match plan {
            ModelPlan::Free => FREE_MODEL_ALIAS_TARGETS,
            ModelPlan::Paid => Self {
                quick: PAID_MODEL_ALIAS_TARGETS.quick,
                powerful: if overrides.powerful_glm_5_3 {
                    GLM_5_3_MODEL_ID
                } else {
                    PAID_MODEL_ALIAS_TARGETS.powerful
                },
            },
        }
    }

    pub(crate) fn resolve(self, model: &str) -> &str {
        match model {
            AUTO_QUICK_MODEL_ID => self.quick,
            AUTO_POWERFUL_MODEL_ID => self.powerful,
            _ => model,
        }
    }

    fn target_for(self, alias: &str) -> Option<&'static str> {
        match alias {
            AUTO_QUICK_MODEL_ID => Some(self.quick),
            AUTO_POWERFUL_MODEL_ID => Some(self.powerful),
            _ => None,
        }
    }
}

impl PaidModelAliasOverrides {
    pub(crate) fn from_flag_values(flags: &HashMap<String, bool>) -> Self {
        Self {
            powerful_glm_5_3: flags
                .get(PAID_POWERFUL_GLM_5_3_ALIAS_FLAG_KEY)
                .copied()
                .unwrap_or(false),
        }
    }
}

impl Default for ModelAliasTargets {
    fn default() -> Self {
        Self::for_plan(ModelPlan::Free)
    }
}

impl ModelCapabilities {
    const fn chat(reasoning: bool, vision: bool) -> Self {
        Self {
            chat: true,
            vision,
            reasoning,
            tool_use: true,
        }
    }

    const fn chat_with_tool_use(reasoning: bool, vision: bool, tool_use: bool) -> Self {
        Self {
            chat: true,
            vision,
            reasoning,
            tool_use,
        }
    }

    fn json(self) -> Value {
        json!({
            "chat": self.chat,
            "vision": self.vision,
            "reasoning": self.reasoning,
            "tool_use": self.tool_use,
        })
    }
}

impl ModelAliasEntry {
    fn catalog_json(self, alias_targets: ModelAliasTargets) -> Value {
        let target_model = alias_targets
            .target_for(self.id)
            .expect("catalog alias must have a configured target");
        let target = model_entry(target_model);
        json!({
            "id": self.id,
            "label": self.label,
            "short_name": self.short_name,
            "description": self.description,
            "target_model": target_model,
            "access": target.map(|entry| entry.access.as_str()).unwrap_or("free"),
            "capabilities": target.map(|entry| entry.capabilities.json()).unwrap_or_else(|| ModelCapabilities::chat(false, false).json()),
        })
    }
}

const DEFAULT_MODEL_CONFIG: ModelConfig = ModelConfig::new(DEFAULT_CONTEXT_WINDOW);

const GEMMA4_RESPONSES_MODEL_CONFIG: ResponsesModelConfig = ResponsesModelConfig {
    sampling: DEFAULT_SAMPLING_CONFIG,
    include_reasoning: true,
    enable_thinking: true,
};

const MODEL_CONFIGS: &[ModelConfigEntry] = &[
    ModelConfigEntry::new(
        "gpt-oss-120b",
        "OpenAI GPT-OSS 120B",
        "GPT-OSS",
        "Fast, everyday reasoning model.",
        ModelAccessTier::Free,
        ModelCapabilities::chat(true, false),
        &[],
        true,
        true,
        false,
        10,
        131_072,
    ),
    ModelConfigEntry::with_responses(
        "gemma4-31b",
        "Gemma 4 31B",
        "Gemma 4",
        "Starter-friendly reasoning and vision model.",
        ModelAccessTier::Starter,
        ModelCapabilities::chat(true, true),
        &[],
        true,
        true,
        false,
        20,
        262_144,
        GEMMA4_RESPONSES_MODEL_CONFIG,
    ),
    ModelConfigEntry::new(
        GLM_5_3_FLASH_MODEL_ID,
        "GLM-5.3 Flash",
        "GLM-5.3 Flash",
        "Fast multimodal reasoning and tool-use model with a 1M-token context window.",
        ModelAccessTier::Pro,
        ModelCapabilities::chat(true, true),
        &["New"],
        true,
        true,
        false,
        25,
        1_048_576,
    )
    .with_catalog_metadata(ModelCatalogMetadata::new(
        &["text", "image"],
        &["text"],
        None,
        None,
    )),
    ModelConfigEntry::new(
        KIMI_K3_MODEL_ID,
        "Kimi K3",
        "Kimi K3",
        "Multimodal reasoning and tool-use model.",
        ModelAccessTier::Pro,
        ModelCapabilities::chat(true, true),
        &[],
        true,
        true,
        false,
        30,
        262_144,
    )
    .with_catalog_metadata(ModelCatalogMetadata::new(
        &["text", "image"],
        &["text"],
        Some("2.8T"),
        Some("104B"),
    )),
    ModelConfigEntry::new(
        "kimi-k2-6",
        "Kimi K2.6",
        "Kimi K2.6",
        "Powerful model for deeper thinking and analysis.",
        ModelAccessTier::Pro,
        ModelCapabilities::chat(true, true),
        &[],
        true,
        true,
        false,
        40,
        262_144,
    )
    .with_catalog_provider("continuum", "kimi-k2.6"),
    ModelConfigEntry::new(
        GLM_5_3_MODEL_ID,
        "GLM 5.3",
        "GLM 5.3",
        "Flagship reasoning model for complex engineering and long-horizon agent work.",
        ModelAccessTier::Pro,
        ModelCapabilities::chat(true, false),
        &["New"],
        true,
        true,
        false,
        50,
        // Shared window: Continuum supports 262,144; Tinfoil supports 1,048,576.
        262_144,
    )
    .with_catalog_provider("continuum", "glm-5.3")
    .with_catalog_metadata(ModelCatalogMetadata::new(&["text"], &["text"], None, None)),
    ModelConfigEntry::new(
        DEEPSEEK_V4_1_FLASH_MODEL_ID,
        "DeepSeek V4.1 Flash",
        "DeepSeek V4.1 Flash",
        "Long-context multimodal reasoning and tool-use model.",
        ModelAccessTier::Pro,
        ModelCapabilities::chat(true, true),
        &["New"],
        true,
        true,
        false,
        65,
        1_048_576,
    )
    .with_catalog_metadata(ModelCatalogMetadata::new(
        &["text", "image"],
        &["text"],
        Some("552B"),
        Some("16B"),
    )),
    ModelConfigEntry::new(
        "llama3-3-70b",
        "Llama 3.3 70B",
        "Llama 3.3",
        "General-purpose model.",
        ModelAccessTier::Free,
        ModelCapabilities::chat(false, false),
        &[],
        true,
        true,
        false,
        80,
        131_072,
    ),
    ModelConfigEntry::api_only(
        "gpt-oss-safeguard-120b",
        "OpenAI GPT-OSS Safeguard 120B",
        "GPT-OSS Safeguard",
        "Safety model available through the API.",
        ModelAccessTier::Free,
        ModelCapabilities::chat_with_tool_use(true, false, false),
        &["Safety"],
        true,
        false,
        900,
        131_072,
    ),
];

const MODEL_ALIAS_ENTRIES: &[ModelAliasEntry] = &[
    ModelAliasEntry {
        id: AUTO_QUICK_MODEL_ID,
        label: "Quick",
        short_name: "Quick",
        description: "Fast, everyday responses",
    },
    ModelAliasEntry {
        id: AUTO_POWERFUL_MODEL_ID,
        label: "Powerful",
        short_name: "Powerful",
        description: "Deeper thinking & analysis",
    },
];

fn alias_target(model: &str) -> Option<&'static str> {
    ModelAliasTargets::default().target_for(model)
}

pub(crate) fn model_alias_requires_flag_lookup(model: &str) -> bool {
    model == AUTO_POWERFUL_MODEL_ID
}

fn model_entry(model: &str) -> Option<ModelConfigEntry> {
    MODEL_CONFIGS
        .iter()
        .find(|entry| entry.id == model)
        .copied()
}

#[cfg(test)]
pub(crate) fn enabled_api_completion_model_ids() -> impl Iterator<Item = &'static str> {
    MODEL_CONFIGS
        .iter()
        .filter(|entry| entry.api_listed && entry.enabled && entry.capabilities.chat)
        .map(|entry| entry.id)
}

pub fn resolve_completion_model_id(model: &str) -> Option<&'static str> {
    let canonical = alias_target(model).unwrap_or(model);
    MODEL_CONFIGS
        .iter()
        .find(|entry| entry.id == canonical && entry.api_listed && entry.enabled)
        .map(|entry| entry.provider_id)
}

pub fn resolve_public_model_id(model: &str) -> Option<&'static str> {
    let canonical = alias_target(model).unwrap_or(model);
    MODEL_CONFIGS
        .iter()
        .find(|entry| entry.id == canonical && entry.api_listed && entry.enabled)
        .map(|entry| entry.id)
}

pub fn model_config(model: &str) -> ModelConfig {
    let canonical = alias_target(model).unwrap_or(model);

    MODEL_CONFIGS
        .iter()
        .find(|entry| entry.id == canonical)
        .or_else(|| {
            MODEL_CONFIGS
                .iter()
                .find(|entry| canonical.starts_with(entry.id))
        })
        .map(|entry| entry.config)
        .unwrap_or(DEFAULT_MODEL_CONFIG)
}

pub fn model_context_window(model: &str) -> usize {
    model_config(model).context_window
}

pub fn model_reasoning_history_strategy(model: &str) -> Option<ReasoningHistoryStrategy> {
    let canonical = alias_target(model).unwrap_or(model);
    let normalized = canonical.to_ascii_lowercase();
    let normalized = normalized
        .strip_prefix("openai/")
        .or_else(|| normalized.strip_prefix("tinfoil/"))
        .unwrap_or(&normalized);

    match normalized {
        "kimi-k2-6" | "kimi-k2.6" | "kimi-k3" => {
            Some(ReasoningHistoryStrategy::KimiPreserveThinking)
        }
        "glm-5-2" | "glm-5.2" | "glm-5-3" | "glm-5.3" | "glm-5-3-flash" | "glm-5.3-flash" => {
            Some(ReasoningHistoryStrategy::GlmClearThinking)
        }
        _ => None,
    }
}

pub fn model_supports_reasoning_history(model: &str) -> bool {
    model_reasoning_history_strategy(model).is_some()
}

pub(crate) fn model_catalog_response(alias_targets: ModelAliasTargets) -> Value {
    let data = MODEL_CONFIGS
        .iter()
        .filter(|entry| entry.listed)
        .map(|entry| entry.catalog_json())
        .collect::<Vec<_>>();
    let aliases = MODEL_ALIAS_ENTRIES
        .iter()
        .map(|entry| entry.catalog_json(alias_targets))
        .collect::<Vec<_>>();

    json!({
        "object": "list",
        "data": data,
        "aliases": aliases,
        "defaults": {
            "quick": AUTO_QUICK_MODEL_ID,
            "powerful": AUTO_POWERFUL_MODEL_ID
        },
        "audio": {
            "transcription": {
                "available": true,
                "model": "whisper-large-v3",
                "display_name": "Whisper Large v3"
            },
            "speech": {
                "available": true,
                "model": "voxtral-tts",
                "display_name": "Voxtral TTS"
            }
        }
    })
}

pub fn openai_models_response() -> Value {
    let mut data = MODEL_CONFIGS
        .iter()
        .filter(|entry| entry.api_listed && entry.enabled)
        .map(|entry| entry.openai_model_json())
        .collect::<Vec<_>>();

    data.extend([
        json!({
            "id": "whisper-large-v3",
            "object": "model",
            "created": 0,
            "owned_by": "opensecret",
            "tasks": ["transcribe"],
            "display_name": "Whisper Large v3",
            "short_name": "Whisper",
        }),
        json!({
            "id": "voxtral-small-24b",
            "object": "model",
            "created": 0,
            "owned_by": "opensecret",
            "tasks": ["transcribe"],
            "display_name": "Voxtral Small 24B",
            "short_name": "Voxtral Small",
        }),
        json!({
            "id": "nomic-embed-text",
            "object": "model",
            "created": 0,
            "owned_by": "opensecret",
            "tasks": ["embed"],
            "display_name": "Nomic Embed Text",
            "short_name": "Nomic Embed",
        }),
        json!({
            "id": "voxtral-tts",
            "object": "model",
            "created": 0,
            "owned_by": "opensecret",
            "tasks": ["speech"],
            "display_name": "Voxtral TTS",
            "short_name": "Voxtral TTS",
        }),
    ]);

    json!({
        "object": "list",
        "data": data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has_model(response: &Value, model_id: &str) -> bool {
        response["data"]
            .as_array()
            .expect("model list data")
            .iter()
            .any(|model| model["id"] == model_id)
    }

    fn catalog_model<'a>(response: &'a Value, model_id: &str) -> &'a Value {
        response["data"]
            .as_array()
            .expect("catalog data")
            .iter()
            .find(|model| model["id"] == model_id)
            .expect("model in catalog")
    }

    fn model_ids_with_badge(response: &Value, badge: &str) -> Vec<String> {
        let mut model_ids = response["data"]
            .as_array()
            .expect("model list data")
            .iter()
            .filter(|model| {
                model["badges"]
                    .as_array()
                    .is_some_and(|badges| badges.iter().any(|value| value == badge))
            })
            .filter_map(|model| model["id"].as_str().map(str::to_owned))
            .collect::<Vec<_>>();
        model_ids.sort_unstable();
        model_ids
    }

    #[test]
    fn test_model_context_window_known_models() {
        assert_eq!(model_context_window("llama3-3-70b"), 131_072);
        assert_eq!(model_context_window("gpt-oss-120b"), 131_072);
        assert_eq!(model_context_window("gpt-oss-safeguard-120b"), 131_072);
        assert_eq!(model_context_window("kimi-k2-6"), 262_144);
        assert_eq!(model_context_window("gemma4-31b"), 262_144);
        assert_eq!(model_context_window("glm-5-3"), 262_144);
        assert_eq!(model_context_window("glm-5-3-flash"), 1_048_576);
        assert_eq!(model_context_window("kimi-k3"), 262_144);
        assert_eq!(model_context_window("deepseek-v4-1-flash"), 1_048_576);
        assert_eq!(model_context_window(AUTO_QUICK_MODEL_ID), 131_072);
        assert_eq!(model_context_window(AUTO_POWERFUL_MODEL_ID), 262_144);
    }

    #[test]
    fn test_existing_models_use_default_sampling_config() {
        for model in [
            "llama3-3-70b",
            "gpt-oss-120b",
            "gpt-oss-safeguard-120b",
            "kimi-k2-6",
            "gemma4-31b",
            "glm-5-3",
            "glm-5-3-flash",
            "kimi-k3",
            "deepseek-v4-1-flash",
        ] {
            let config = model_config(model);

            assert_eq!(config.responses.sampling.temperature, DEFAULT_TEMPERATURE);
            assert_eq!(config.responses.sampling.top_p, DEFAULT_TOP_P);
        }
    }

    #[test]
    fn test_gemma4_responses_config_enables_thinking() {
        let responses_config = model_config("gemma4-31b").responses;

        assert!(responses_config.include_reasoning);
        assert!(responses_config.enable_thinking);
    }

    #[test]
    fn test_model_supports_reasoning_history_only_for_validated_models() {
        assert!(model_supports_reasoning_history("kimi-k2-6"));
        assert!(model_supports_reasoning_history("kimi-k2.6"));
        assert!(model_supports_reasoning_history("kimi-k3"));
        assert!(model_supports_reasoning_history("glm-5-2"));
        assert!(model_supports_reasoning_history("glm-5.2"));
        assert!(model_supports_reasoning_history("glm-5-3"));
        assert!(model_supports_reasoning_history("glm-5.3"));
        assert!(model_supports_reasoning_history("glm-5-3-flash"));
        assert!(model_supports_reasoning_history("glm-5.3-flash"));
        assert!(model_supports_reasoning_history(AUTO_POWERFUL_MODEL_ID));

        assert!(!model_supports_reasoning_history("gpt-oss-120b"));
        assert!(!model_supports_reasoning_history("openai/gpt-oss-120b"));
        assert!(!model_supports_reasoning_history(AUTO_QUICK_MODEL_ID));
        assert!(!model_supports_reasoning_history("gpt-oss-safeguard-120b"));
        assert!(!model_supports_reasoning_history("gemma4-31b"));
        assert!(!model_supports_reasoning_history("deepseek-v4-pro"));
        assert!(!model_supports_reasoning_history("llama3-3-70b"));
        assert!(!model_supports_reasoning_history("unknown-model"));
    }

    #[test]
    fn test_model_reasoning_history_strategy_by_model_family() {
        assert_eq!(
            model_reasoning_history_strategy("kimi-k2-6"),
            Some(ReasoningHistoryStrategy::KimiPreserveThinking)
        );
        assert_eq!(
            model_reasoning_history_strategy("kimi-k2.6"),
            Some(ReasoningHistoryStrategy::KimiPreserveThinking)
        );
        assert_eq!(
            model_reasoning_history_strategy("kimi-k3"),
            Some(ReasoningHistoryStrategy::KimiPreserveThinking)
        );
        assert_eq!(
            model_reasoning_history_strategy("glm-5-2"),
            Some(ReasoningHistoryStrategy::GlmClearThinking)
        );
        assert_eq!(
            model_reasoning_history_strategy("glm-5.3"),
            Some(ReasoningHistoryStrategy::GlmClearThinking)
        );
        assert_eq!(
            model_reasoning_history_strategy("glm-5-3-flash"),
            Some(ReasoningHistoryStrategy::GlmClearThinking)
        );
        assert_eq!(
            model_reasoning_history_strategy(AUTO_POWERFUL_MODEL_ID),
            Some(ReasoningHistoryStrategy::GlmClearThinking)
        );
        assert_eq!(model_reasoning_history_strategy("gemma4-31b"), None);
    }

    #[test]
    fn test_sampling_config_applies_overrides() {
        let sampling = DEFAULT_SAMPLING_CONFIG.with_overrides(Some(0.5), None);

        assert_eq!(sampling.temperature, 0.5);
        assert_eq!(sampling.top_p, DEFAULT_TOP_P);
    }

    #[test]
    fn test_model_context_window_unknown_models() {
        assert_eq!(model_context_window("gpt-4"), DEFAULT_CONTEXT_WINDOW);
        assert_eq!(model_context_window("claude-3"), DEFAULT_CONTEXT_WINDOW);
        assert_eq!(
            model_context_window("unknown-model-xyz"),
            DEFAULT_CONTEXT_WINDOW
        );
        assert_eq!(model_context_window(""), DEFAULT_CONTEXT_WINDOW);
    }

    #[test]
    fn test_model_config_prefix_matching() {
        assert_eq!(model_context_window("llama3-3-70b-instruct"), 131_072);
        assert_eq!(
            model_context_window("unknown-r1-70b-instruct"),
            DEFAULT_CONTEXT_WINDOW
        );
    }

    #[test]
    fn test_resolve_completion_model_aliases() {
        assert_eq!(
            resolve_completion_model_id(AUTO_QUICK_MODEL_ID),
            Some(QUICK_MODEL_ID)
        );
        assert_eq!(
            resolve_completion_model_id(AUTO_POWERFUL_MODEL_ID),
            Some(POWERFUL_MODEL_ID)
        );
        assert_eq!(
            resolve_completion_model_id("gpt-oss-safeguard-120b"),
            Some("gpt-oss-safeguard-120b")
        );
        assert_eq!(resolve_completion_model_id("voxtral-small-24b"), None);
        assert_eq!(resolve_completion_model_id("quick"), None);
        assert_eq!(resolve_completion_model_id("kimi-k2-5"), None);
        assert_eq!(resolve_completion_model_id("kimi-k3"), Some("kimi-k3"));
        assert_eq!(resolve_completion_model_id("glm-5-3"), Some("glm-5-3"));
        assert_eq!(
            resolve_completion_model_id("glm-5-3-flash"),
            Some("glm-5-3-flash")
        );
        assert_eq!(resolve_completion_model_id("deepseek-v4-flash"), None);
        assert_eq!(
            resolve_completion_model_id("deepseek-v4-1-flash"),
            Some("deepseek-v4-1-flash")
        );
        assert_eq!(resolve_completion_model_id("unknown-model"), None);
    }

    #[test]
    fn test_resolve_public_model_aliases() {
        assert_eq!(
            resolve_public_model_id(AUTO_QUICK_MODEL_ID),
            Some(QUICK_MODEL_ID)
        );
        assert_eq!(
            resolve_public_model_id(AUTO_POWERFUL_MODEL_ID),
            Some(POWERFUL_MODEL_ID)
        );
        assert_eq!(
            resolve_public_model_id("gpt-oss-safeguard-120b"),
            Some("gpt-oss-safeguard-120b")
        );
        assert_eq!(resolve_public_model_id("kimi-k2-5"), None);
        assert_eq!(resolve_public_model_id("kimi-k3"), Some("kimi-k3"));
        assert_eq!(resolve_public_model_id("glm-5-3"), Some("glm-5-3"));
        assert_eq!(
            resolve_public_model_id("glm-5-3-flash"),
            Some("glm-5-3-flash")
        );
        assert_eq!(resolve_public_model_id("deepseek-v4-flash"), None);
        assert_eq!(
            resolve_public_model_id("deepseek-v4-1-flash"),
            Some("deepseek-v4-1-flash")
        );
        assert_eq!(resolve_public_model_id("unknown-model"), None);
    }

    #[test]
    fn test_model_alias_targets_are_selected_by_plan() {
        assert_eq!(POWERFUL_MODEL_ID, GLM_5_3_MODEL_ID);

        let free = ModelAliasTargets::for_plan(ModelPlan::Free);
        assert_eq!(free.resolve(AUTO_QUICK_MODEL_ID), QUICK_MODEL_ID);
        assert_eq!(free.resolve(AUTO_POWERFUL_MODEL_ID), GLM_5_3_MODEL_ID);

        let paid = ModelAliasTargets::for_plan(ModelPlan::Paid);
        assert_eq!(
            paid.resolve(AUTO_QUICK_MODEL_ID),
            DEEPSEEK_V4_1_FLASH_MODEL_ID
        );
        assert_eq!(paid.resolve(AUTO_POWERFUL_MODEL_ID), GLM_5_3_MODEL_ID);
        assert_eq!(paid.resolve(KIMI_K3_MODEL_ID), KIMI_K3_MODEL_ID);
    }

    #[test]
    fn test_router_v2_auto_targets_use_glm_5_3_and_preserve_plan_specific_quick() {
        for (plan, expected_quick) in [
            (ModelPlan::Free, QUICK_MODEL_ID),
            (ModelPlan::Paid, DEEPSEEK_V4_1_FLASH_MODEL_ID),
        ] {
            let targets = ModelAliasTargets::for_router_v2(plan);
            assert_eq!(targets.resolve(AUTO_QUICK_MODEL_ID), expected_quick);
            assert_eq!(targets.resolve(AUTO_POWERFUL_MODEL_ID), GLM_5_3_MODEL_ID);
            assert_eq!(
                resolve_public_model_id(targets.resolve(AUTO_POWERFUL_MODEL_ID)),
                Some(GLM_5_3_MODEL_ID)
            );
            assert_eq!(
                model_context_window(targets.resolve(AUTO_POWERFUL_MODEL_ID)),
                model_context_window(GLM_5_3_MODEL_ID)
            );
        }
    }

    #[test]
    fn test_router_v2_alias_policy_preserves_explicit_model_identities() {
        for plan in [ModelPlan::Free, ModelPlan::Paid] {
            let targets = ModelAliasTargets::for_router_v2(plan);
            for model in enabled_api_completion_model_ids() {
                assert_eq!(targets.resolve(model), model, "plan={plan:?}");
                assert_eq!(resolve_public_model_id(targets.resolve(model)), Some(model));
            }
            assert_eq!(targets.resolve(GLM_5_3_MODEL_ID), GLM_5_3_MODEL_ID);
            assert_eq!(
                resolve_completion_model_id(targets.resolve(GLM_5_3_MODEL_ID)),
                Some(GLM_5_3_MODEL_ID)
            );
            assert_eq!(targets.resolve("unknown-model"), "unknown-model");
            assert_eq!(
                resolve_public_model_id(targets.resolve("unknown-model")),
                None
            );
        }
    }

    #[test]
    fn test_router_v2_catalog_alias_metadata_matches_resolved_models() {
        for plan in [ModelPlan::Free, ModelPlan::Paid] {
            let targets = ModelAliasTargets::for_router_v2(plan);
            let catalog = model_catalog_response(targets);
            for alias in catalog["aliases"].as_array().expect("aliases") {
                let selector = alias["id"].as_str().expect("alias ID");
                let target = targets.resolve(selector);
                let model = catalog_model(&catalog, target);
                assert_eq!(alias["target_model"], target);
                assert_eq!(alias["access"], model["access"]);
                assert_eq!(alias["capabilities"], model["capabilities"]);
            }
            assert_eq!(catalog["defaults"]["quick"], AUTO_QUICK_MODEL_ID);
            assert_eq!(catalog["defaults"]["powerful"], AUTO_POWERFUL_MODEL_ID);
            assert!(!has_model(&catalog, "glm-5-2"));
            assert!(!has_model(&catalog, DEEPSEEK_V4_FLASH_MODEL_ID));
            assert!(has_model(&catalog, GLM_5_3_MODEL_ID));
        }
    }

    #[test]
    fn test_router_v2_auto_targets_preserve_model_entitlements() {
        for plan in [ModelPlan::Free, ModelPlan::Paid] {
            let targets = ModelAliasTargets::for_router_v2(plan);
            assert!(plan.allows_model(targets.resolve(AUTO_QUICK_MODEL_ID)));
            assert_eq!(
                plan.allows_model(targets.resolve(AUTO_POWERFUL_MODEL_ID)),
                plan.is_paid()
            );
            for model in [GLM_5_3_MODEL_ID, GLM_5_3_FLASH_MODEL_ID] {
                assert_eq!(plan.allows_model(targets.resolve(model)), plan.is_paid());
            }
        }
    }

    #[test]
    fn test_paid_powerful_glm_5_3_alias_override_is_plan_gated_and_default_off() {
        for powerful_enabled in [false, true] {
            let flags = HashMap::from([(
                PAID_POWERFUL_GLM_5_3_ALIAS_FLAG_KEY.to_string(),
                powerful_enabled,
            )]);
            let overrides = PaidModelAliasOverrides::from_flag_values(&flags);

            let free = ModelAliasTargets::for_plan_with_overrides(ModelPlan::Free, overrides);
            assert_eq!(free.resolve(AUTO_QUICK_MODEL_ID), QUICK_MODEL_ID);
            assert_eq!(free.resolve(AUTO_POWERFUL_MODEL_ID), GLM_5_3_MODEL_ID);

            let paid = ModelAliasTargets::for_plan_with_overrides(ModelPlan::Paid, overrides);
            assert_eq!(
                paid.resolve(AUTO_QUICK_MODEL_ID),
                DEEPSEEK_V4_1_FLASH_MODEL_ID
            );
            assert_eq!(paid.resolve(AUTO_POWERFUL_MODEL_ID), GLM_5_3_MODEL_ID);
        }

        assert_eq!(
            PaidModelAliasOverrides::from_flag_values(&HashMap::new()),
            PaidModelAliasOverrides::default()
        );
        assert_eq!(
            PAID_POWERFUL_GLM_5_3_ALIAS_FLAG_KEY,
            "model-alias.paid.powerful.glm-5-3"
        );
        assert_eq!(
            crate::os_flags::PAID_MODEL_ALIAS_FLAG_KEYS,
            &[PAID_POWERFUL_GLM_5_3_ALIAS_FLAG_KEY]
        );
        assert!(model_alias_requires_flag_lookup(AUTO_POWERFUL_MODEL_ID));
        assert!(!model_alias_requires_flag_lookup(AUTO_QUICK_MODEL_ID));
        assert!(!model_alias_requires_flag_lookup(GLM_5_3_MODEL_ID));
    }

    #[test]
    fn test_catalog_alias_metadata_tracks_paid_glm_5_3_override() {
        let flags = HashMap::from([(PAID_POWERFUL_GLM_5_3_ALIAS_FLAG_KEY.to_string(), true)]);
        let targets = ModelAliasTargets::for_plan_with_overrides(
            ModelPlan::Paid,
            PaidModelAliasOverrides::from_flag_values(&flags),
        );
        let catalog = model_catalog_response(targets);
        let powerful = catalog["aliases"]
            .as_array()
            .expect("aliases")
            .iter()
            .find(|alias| alias["id"] == AUTO_POWERFUL_MODEL_ID)
            .expect("powerful alias");

        assert_eq!(powerful["target_model"], GLM_5_3_MODEL_ID);
        assert_eq!(powerful["access"], "pro");
        assert_eq!(powerful["capabilities"]["reasoning"], true);
        assert_eq!(powerful["capabilities"]["vision"], false);
    }

    #[test]
    fn test_golden_auto_alias_resolution_matrix_by_plan() {
        for (plan, selector, expected_target) in [
            (ModelPlan::Free, AUTO_QUICK_MODEL_ID, QUICK_MODEL_ID),
            (
                ModelPlan::Paid,
                AUTO_QUICK_MODEL_ID,
                DEEPSEEK_V4_1_FLASH_MODEL_ID,
            ),
            (ModelPlan::Free, AUTO_POWERFUL_MODEL_ID, GLM_5_3_MODEL_ID),
            (ModelPlan::Paid, AUTO_POWERFUL_MODEL_ID, GLM_5_3_MODEL_ID),
        ] {
            let targets = ModelAliasTargets::for_plan(plan);
            assert_eq!(targets.resolve(selector), expected_target, "plan={plan:?}");
        }
    }

    #[test]
    fn test_golden_explicit_model_resolution_is_plan_invariant() {
        for plan in [ModelPlan::Free, ModelPlan::Paid] {
            let targets = ModelAliasTargets::for_plan(plan);
            for model in [
                QUICK_MODEL_ID,
                "kimi-k2-6",
                KIMI_K3_MODEL_ID,
                GLM_5_3_MODEL_ID,
                GLM_5_3_FLASH_MODEL_ID,
                DEEPSEEK_V4_1_FLASH_MODEL_ID,
            ] {
                assert_eq!(
                    targets.resolve(model),
                    model,
                    "explicit model changed for plan={plan:?}"
                );
            }
        }
    }

    #[test]
    fn test_catalog_alias_metadata_tracks_paid_targets() {
        let targets = ModelAliasTargets::for_plan(ModelPlan::Paid);
        let catalog = model_catalog_response(targets);
        let aliases = catalog["aliases"].as_array().expect("aliases");
        let quick = aliases
            .iter()
            .find(|alias| alias["id"] == AUTO_QUICK_MODEL_ID)
            .expect("quick alias");
        let powerful = aliases
            .iter()
            .find(|alias| alias["id"] == AUTO_POWERFUL_MODEL_ID)
            .expect("powerful alias");

        assert_eq!(quick["target_model"], DEEPSEEK_V4_1_FLASH_MODEL_ID);
        assert_eq!(quick["access"], "pro");
        assert_eq!(quick["capabilities"]["vision"], true);
        assert_eq!(powerful["target_model"], GLM_5_3_MODEL_ID);
        assert_eq!(powerful["access"], "pro");
        assert_eq!(powerful["capabilities"]["vision"], false);
    }

    #[test]
    fn test_model_plan_enforces_catalog_access_tiers_and_alias_targets() {
        for model in [
            "gpt-oss-120b",
            "gpt-oss-safeguard-120b",
            "llama3-3-70b",
            AUTO_QUICK_MODEL_ID,
            "unknown-model",
        ] {
            assert!(ModelPlan::Free.allows_model(model), "free model: {model}");
            assert!(ModelPlan::Paid.allows_model(model), "paid model: {model}");
        }

        for model in [
            "gemma4-31b",
            "kimi-k3",
            "kimi-k2-6",
            "glm-5-3",
            "glm-5-3-flash",
            "deepseek-v4-1-flash",
            AUTO_POWERFUL_MODEL_ID,
        ] {
            assert!(
                !ModelPlan::Free.allows_model(model),
                "free plan should deny: {model}"
            );
            assert!(ModelPlan::Paid.allows_model(model), "paid model: {model}");
        }
    }

    #[test]
    fn test_catalog_hides_api_only_models_and_includes_aliases() {
        let response = model_catalog_response(ModelAliasTargets::default());
        let data = response["data"].as_array().expect("catalog data");
        assert!(data.iter().any(|model| model["id"] == QUICK_MODEL_ID));
        assert!(!data
            .iter()
            .any(|model| model["id"] == "gpt-oss-safeguard-120b"));
        assert!(!data.iter().any(|model| model["id"] == "voxtral-small-24b"));

        let aliases = response["aliases"].as_array().expect("aliases");
        assert!(aliases
            .iter()
            .any(|alias| alias["id"] == AUTO_QUICK_MODEL_ID));
        assert!(aliases
            .iter()
            .any(|alias| alias["id"] == AUTO_POWERFUL_MODEL_ID));

        for (plan, expected_quick, expected_quick_access) in [
            (ModelPlan::Free, QUICK_MODEL_ID, "free"),
            (ModelPlan::Paid, DEEPSEEK_V4_1_FLASH_MODEL_ID, "pro"),
        ] {
            let catalog = model_catalog_response(ModelAliasTargets::for_plan(plan));
            let aliases = catalog["aliases"].as_array().expect("aliases");
            let quick = aliases
                .iter()
                .find(|alias| alias["id"] == AUTO_QUICK_MODEL_ID)
                .expect("quick alias");
            let powerful = aliases
                .iter()
                .find(|alias| alias["id"] == AUTO_POWERFUL_MODEL_ID)
                .expect("powerful alias");

            assert_eq!(quick["target_model"], expected_quick);
            assert_eq!(quick["access"], expected_quick_access);
            assert_eq!(powerful["target_model"], POWERFUL_MODEL_ID);
            assert_eq!(powerful["access"], "pro");
        }
    }

    #[test]
    fn test_openai_models_includes_api_only_models() {
        let response = openai_models_response();
        let data = response["data"].as_array().expect("models data");

        assert!(data
            .iter()
            .any(|model| model["id"] == "gpt-oss-safeguard-120b"));
        assert!(data.iter().any(|model| model["id"] == "voxtral-small-24b"));
        assert!(!data.iter().any(|model| model["id"] == "qwen3-tts"));
        assert!(data.iter().any(|model| model["id"] == "voxtral-tts"));
    }

    #[test]
    fn test_catalog_advertises_voxtral_as_default_speech_model() {
        let response = model_catalog_response(ModelAliasTargets::default());

        assert_eq!(response["audio"]["speech"]["available"], true);
        assert_eq!(response["audio"]["speech"]["model"], "voxtral-tts");
        assert_eq!(response["audio"]["speech"]["display_name"], "Voxtral TTS");
    }

    #[test]
    fn test_catalog_keeps_kimi_k2_6_selectable_through_continuum() {
        let catalog = model_catalog_response(ModelAliasTargets::default());
        let kimi = catalog_model(&catalog, "kimi-k2-6");

        assert_eq!(kimi["provider"], "continuum");
        assert_eq!(kimi["provider_id"], "kimi-k2.6");
        assert_eq!(resolve_completion_model_id("kimi-k2-6"), Some("kimi-k2-6"));
    }

    #[test]
    fn test_catalog_adds_glm_5_3_directly_through_continuum_without_changing_aliases() {
        let catalog = model_catalog_response(ModelAliasTargets::for_plan(ModelPlan::Paid));
        let glm = catalog_model(&catalog, GLM_5_3_MODEL_ID);

        assert_eq!(glm["provider"], "continuum");
        assert_eq!(glm["provider_id"], "glm-5.3");
        assert_eq!(glm["access"], "pro");
        assert_eq!(glm["context_window"], 262_144);
        assert_eq!(glm["input_modalities"], json!(["text"]));
        assert_eq!(glm["output_modalities"], json!(["text"]));
        assert_eq!(glm["capabilities"]["vision"], false);
        assert_eq!(glm["capabilities"]["reasoning"], true);
        assert_eq!(glm["capabilities"]["tool_use"], true);
        assert_eq!(
            resolve_completion_model_id(GLM_5_3_MODEL_ID),
            Some(GLM_5_3_MODEL_ID)
        );

        let powerful = catalog["aliases"]
            .as_array()
            .expect("aliases")
            .iter()
            .find(|alias| alias["id"] == AUTO_POWERFUL_MODEL_ID)
            .expect("powerful alias");
        assert_eq!(powerful["target_model"], GLM_5_3_MODEL_ID);
    }

    #[test]
    fn test_catalog_adds_glm_5_3_flash_through_tinfoil_with_image_and_1m_context() {
        let catalog = model_catalog_response(ModelAliasTargets::for_plan(ModelPlan::Paid));
        let glm = catalog_model(&catalog, GLM_5_3_FLASH_MODEL_ID);

        assert_eq!(glm["provider"], "tinfoil");
        assert_eq!(glm["provider_id"], GLM_5_3_FLASH_MODEL_ID);
        assert_eq!(glm["display_name"], "GLM-5.3 Flash");
        assert_eq!(glm["access"], "pro");
        assert_eq!(glm["context_window"], 1_048_576);
        assert_eq!(glm["input_modalities"], json!(["text", "image"]));
        assert_eq!(glm["output_modalities"], json!(["text"]));
        assert_eq!(glm["capabilities"]["vision"], true);
        assert_eq!(glm["capabilities"]["reasoning"], true);
        assert_eq!(glm["capabilities"]["tool_use"], true);
        assert_eq!(glm["tasks"], json!(["generate", "vision"]));
        assert_eq!(
            resolve_completion_model_id(GLM_5_3_FLASH_MODEL_ID),
            Some(GLM_5_3_FLASH_MODEL_ID)
        );
    }

    #[test]
    fn test_catalog_adds_deepseek_v4_1_flash_through_tinfoil_with_image_and_1m_context() {
        let catalog = model_catalog_response(ModelAliasTargets::for_plan(ModelPlan::Paid));
        let deepseek = catalog_model(&catalog, DEEPSEEK_V4_1_FLASH_MODEL_ID);

        assert_eq!(deepseek["provider"], "tinfoil");
        assert_eq!(deepseek["provider_id"], DEEPSEEK_V4_1_FLASH_MODEL_ID);
        assert_eq!(deepseek["display_name"], "DeepSeek V4.1 Flash");
        assert_eq!(deepseek["access"], "pro");
        assert_eq!(deepseek["context_window"], 1_048_576);
        assert_eq!(deepseek["input_modalities"], json!(["text", "image"]));
        assert_eq!(deepseek["output_modalities"], json!(["text"]));
        assert_eq!(deepseek["capabilities"]["vision"], true);
        assert_eq!(deepseek["capabilities"]["reasoning"], true);
        assert_eq!(deepseek["capabilities"]["tool_use"], true);
        assert_eq!(deepseek["tasks"], json!(["generate", "vision"]));
        assert_eq!(
            resolve_completion_model_id(DEEPSEEK_V4_1_FLASH_MODEL_ID),
            Some(DEEPSEEK_V4_1_FLASH_MODEL_ID)
        );
        assert!(!has_model(&catalog, DEEPSEEK_V4_FLASH_MODEL_ID));
    }

    #[test]
    fn test_new_tinfoil_models_are_generally_listed() {
        let catalog = model_catalog_response(ModelAliasTargets::default());
        let openai_models = openai_models_response();

        for model in [
            "kimi-k3",
            "deepseek-v4-1-flash",
            GLM_5_3_FLASH_MODEL_ID,
            QUICK_MODEL_ID,
        ] {
            assert!(has_model(&catalog, model));
            assert!(has_model(&openai_models, model));
        }
    }

    #[test]
    fn test_new_badges_match_the_currently_launched_models() {
        let expected = vec![
            DEEPSEEK_V4_1_FLASH_MODEL_ID.to_string(),
            GLM_5_3_MODEL_ID.to_string(),
            GLM_5_3_FLASH_MODEL_ID.to_string(),
        ];

        assert_eq!(
            model_ids_with_badge(&model_catalog_response(ModelAliasTargets::default()), "New"),
            expected
        );
        assert_eq!(
            model_ids_with_badge(&openai_models_response(), "New"),
            expected
        );
        assert!(model_ids_with_badge(
            &model_catalog_response(ModelAliasTargets::default()),
            "Reasoning"
        )
        .is_empty());
        assert!(model_ids_with_badge(&openai_models_response(), "Reasoning").is_empty());
    }

    #[test]
    fn test_enriched_catalog_has_verified_new_model_metadata() {
        let catalog = model_catalog_response(ModelAliasTargets::default());
        let kimi = catalog_model(&catalog, "kimi-k3");
        assert_eq!(kimi["access"], "pro");
        assert_eq!(kimi["provider_id"], "kimi-k3");
        assert_eq!(kimi["context_window"], 262_144);
        assert_eq!(kimi["input_modalities"], json!(["text", "image"]));
        assert_eq!(kimi["output_modalities"], json!(["text"]));
        assert_eq!(kimi["parameter_size"], "2.8T");
        assert_eq!(kimi["active_parameter_size"], "104B");
        assert_eq!(kimi["capabilities"]["vision"], true);
        assert_eq!(kimi["capabilities"]["reasoning"], true);
        assert_eq!(kimi["capabilities"]["tool_use"], true);
        assert_eq!(kimi["tasks"], json!(["generate", "vision"]));

        assert!(!has_model(&catalog, "deepseek-v4-flash"));

        let deepseek_v41 = catalog_model(&catalog, DEEPSEEK_V4_1_FLASH_MODEL_ID);
        assert_eq!(deepseek_v41["access"], "pro");
        assert_eq!(deepseek_v41["provider_id"], DEEPSEEK_V4_1_FLASH_MODEL_ID);
        assert_eq!(deepseek_v41["display_name"], "DeepSeek V4.1 Flash");
        assert_eq!(deepseek_v41["context_window"], 1_048_576);
        assert_eq!(deepseek_v41["input_modalities"], json!(["text", "image"]));
        assert_eq!(deepseek_v41["output_modalities"], json!(["text"]));
        assert_eq!(deepseek_v41["parameter_size"], "552B");
        assert_eq!(deepseek_v41["active_parameter_size"], "16B");
        assert_eq!(deepseek_v41["capabilities"]["vision"], true);
        assert_eq!(deepseek_v41["capabilities"]["reasoning"], true);
        assert_eq!(deepseek_v41["capabilities"]["tool_use"], true);
        assert_eq!(deepseek_v41["tasks"], json!(["generate", "vision"]));

        let minimal = openai_models_response();
        let minimal_kimi = catalog_model(&minimal, "kimi-k3");
        assert!(minimal_kimi.get("input_modalities").is_none());
        assert!(minimal_kimi.get("parameter_size").is_none());
    }
}
