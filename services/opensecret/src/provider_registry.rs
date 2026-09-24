//! Credential-free provider topology for completion routing.
//!
//! This registry is intentionally independent from executable credentials.
//! Router v2 consumes its route eligibility and weights for every completion
//! model. Legacy feature flags and default-provider preferences stay in the
//! separate Router v1 configuration.

use crate::model_config::{
    DEEPSEEK_V4_1_FLASH_MODEL_ID, GLM_5_3_FLASH_MODEL_ID, GLM_5_3_MODEL_ID, KIMI_K3_MODEL_ID,
    QUICK_MODEL_ID,
};

pub(crate) const SHADOW_ROUTING_POLICY_VERSION: &str = "routing-v2-weighted-v5";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum ProviderId {
    Tinfoil,
    Continuum,
}

impl ProviderId {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Tinfoil => "tinfoil",
            Self::Continuum => "continuum",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteSelectionSource {
    StaticSplit,
    FeatureFlag,
    DefaultProvider,
    Fallback,
    /// Router v2 kept the account's remembered same-model provider instead of
    /// its weighted bucket while that provider stays eligible.
    Sticky,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProviderSpec {
    pub(crate) id: ProviderId,
    pub(crate) weight: u16,
    pub(crate) enabled: bool,
}

/// Scope of an upstream 429 for a configured completion route.
///
/// Tinfoil meters the shared OpenSecret credential per model, while
/// Continuum/Edgeless documents organization-level limits. Keeping this in the
/// credential-free registry prevents runtime error text or arbitrary model
/// strings from defining health-state keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RateLimitScope {
    ProviderModel,
    ProviderAccount,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ModelRouteSpec {
    pub(crate) provider: ProviderId,
    pub(crate) provider_model_id: &'static str,
    pub(crate) response_model_id: &'static str,
    pub(crate) rate_limit_scope: RateLimitScope,
    pub(crate) weight: u16,
    pub(crate) enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CompletionModelSpec {
    pub(crate) public_model_id: &'static str,
    pub(crate) routes: &'static [ModelRouteSpec],
}

const fn completion_model(
    public_model_id: &'static str,
    routes: &'static [ModelRouteSpec],
) -> CompletionModelSpec {
    CompletionModelSpec {
        public_model_id,
        routes,
    }
}

#[derive(Debug)]
pub(crate) struct ProviderRegistry {
    providers: &'static [ProviderSpec],
    completion_models: &'static [CompletionModelSpec],
}

impl ProviderRegistry {
    pub(crate) fn provider(&self, id: ProviderId) -> Option<&ProviderSpec> {
        self.providers.iter().find(|provider| provider.id == id)
    }

    pub(crate) fn providers(&self) -> &'static [ProviderSpec] {
        self.providers
    }

    pub(crate) fn completion_model(&self, public_model_id: &str) -> Option<&CompletionModelSpec> {
        self.completion_models
            .iter()
            .find(|model| model.public_model_id == public_model_id)
    }

    pub(crate) fn completion_models(&self) -> &'static [CompletionModelSpec] {
        self.completion_models
    }
}

const PROVIDERS: &[ProviderSpec] = &[
    ProviderSpec {
        id: ProviderId::Tinfoil,
        weight: 70,
        enabled: true,
    },
    ProviderSpec {
        id: ProviderId::Continuum,
        weight: 30,
        enabled: true,
    },
];

const GPT_OSS_120B_ROUTES: &[ModelRouteSpec] = &[ModelRouteSpec {
    provider: ProviderId::Tinfoil,
    provider_model_id: QUICK_MODEL_ID,
    response_model_id: QUICK_MODEL_ID,
    rate_limit_scope: RateLimitScope::ProviderModel,
    weight: 100,
    enabled: true,
}];

const GEMMA4_31B_ROUTES: &[ModelRouteSpec] = &[ModelRouteSpec {
    provider: ProviderId::Tinfoil,
    provider_model_id: "gemma4-31b",
    response_model_id: "gemma4-31b",
    rate_limit_scope: RateLimitScope::ProviderModel,
    weight: 100,
    enabled: true,
}];

const KIMI_K3_ROUTES: &[ModelRouteSpec] = &[ModelRouteSpec {
    provider: ProviderId::Tinfoil,
    provider_model_id: KIMI_K3_MODEL_ID,
    response_model_id: KIMI_K3_MODEL_ID,
    rate_limit_scope: RateLimitScope::ProviderModel,
    weight: 100,
    enabled: true,
}];

// Combined with the provider weights, 30 * 700 : 70 * 100 allocates healthy
// GLM 5.3 account buckets 75% to Continuum and 25% to Tinfoil.
const GLM_5_3_ROUTES: &[ModelRouteSpec] = &[
    ModelRouteSpec {
        provider: ProviderId::Continuum,
        provider_model_id: "glm-5.3",
        response_model_id: GLM_5_3_MODEL_ID,
        rate_limit_scope: RateLimitScope::ProviderAccount,
        weight: 700,
        enabled: true,
    },
    ModelRouteSpec {
        provider: ProviderId::Tinfoil,
        provider_model_id: GLM_5_3_MODEL_ID,
        response_model_id: GLM_5_3_MODEL_ID,
        rate_limit_scope: RateLimitScope::ProviderModel,
        weight: 100,
        enabled: true,
    },
];

// Use the same healthy 75% Continuum / 25% Tinfoil allocation as GLM 5.3.
const GLM_5_3_FLASH_ROUTES: &[ModelRouteSpec] = &[
    ModelRouteSpec {
        provider: ProviderId::Continuum,
        provider_model_id: "glm-5.3-flash",
        response_model_id: GLM_5_3_FLASH_MODEL_ID,
        rate_limit_scope: RateLimitScope::ProviderAccount,
        weight: 700,
        enabled: true,
    },
    ModelRouteSpec {
        provider: ProviderId::Tinfoil,
        provider_model_id: GLM_5_3_FLASH_MODEL_ID,
        response_model_id: GLM_5_3_FLASH_MODEL_ID,
        rate_limit_scope: RateLimitScope::ProviderModel,
        weight: 100,
        enabled: true,
    },
];

const DEEPSEEK_V4_1_FLASH_ROUTES: &[ModelRouteSpec] = &[ModelRouteSpec {
    provider: ProviderId::Tinfoil,
    provider_model_id: DEEPSEEK_V4_1_FLASH_MODEL_ID,
    response_model_id: DEEPSEEK_V4_1_FLASH_MODEL_ID,
    rate_limit_scope: RateLimitScope::ProviderModel,
    weight: 100,
    enabled: true,
}];

const LLAMA3_3_70B_ROUTES: &[ModelRouteSpec] = &[ModelRouteSpec {
    provider: ProviderId::Tinfoil,
    provider_model_id: "llama3-3-70b",
    response_model_id: "llama3-3-70b",
    rate_limit_scope: RateLimitScope::ProviderModel,
    weight: 100,
    enabled: true,
}];

const GPT_OSS_SAFEGUARD_120B_ROUTES: &[ModelRouteSpec] = &[ModelRouteSpec {
    provider: ProviderId::Tinfoil,
    provider_model_id: "gpt-oss-safeguard-120b",
    response_model_id: "gpt-oss-safeguard-120b",
    rate_limit_scope: RateLimitScope::ProviderModel,
    weight: 100,
    enabled: true,
}];

const COMPLETION_MODELS: &[CompletionModelSpec] = &[
    completion_model(QUICK_MODEL_ID, GPT_OSS_120B_ROUTES),
    completion_model("gemma4-31b", GEMMA4_31B_ROUTES),
    completion_model(KIMI_K3_MODEL_ID, KIMI_K3_ROUTES),
    completion_model(GLM_5_3_MODEL_ID, GLM_5_3_ROUTES),
    completion_model(GLM_5_3_FLASH_MODEL_ID, GLM_5_3_FLASH_ROUTES),
    completion_model(DEEPSEEK_V4_1_FLASH_MODEL_ID, DEEPSEEK_V4_1_FLASH_ROUTES),
    completion_model("llama3-3-70b", LLAMA3_3_70B_ROUTES),
    completion_model("gpt-oss-safeguard-120b", GPT_OSS_SAFEGUARD_120B_ROUTES),
];

pub(crate) static PROVIDER_REGISTRY: ProviderRegistry = ProviderRegistry {
    providers: PROVIDERS,
    completion_models: COMPLETION_MODELS,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_config::{
        enabled_api_completion_model_ids, AUTO_POWERFUL_MODEL_ID, AUTO_QUICK_MODEL_ID,
    };
    use std::collections::BTreeSet;

    #[test]
    fn registry_is_structurally_valid_and_deterministic() {
        let registry = &PROVIDER_REGISTRY;
        let provider_ids = registry
            .providers()
            .iter()
            .map(|provider| provider.id)
            .collect::<BTreeSet<_>>();
        assert_eq!(provider_ids.len(), registry.providers().len());

        let model_ids = registry
            .completion_models()
            .iter()
            .map(|model| model.public_model_id)
            .collect::<BTreeSet<_>>();
        assert_eq!(model_ids.len(), registry.completion_models().len());

        for model in registry.completion_models() {
            assert!(!model.public_model_id.trim().is_empty());
            assert!(!model.routes.is_empty(), "{}", model.public_model_id);
            assert_ne!(model.public_model_id, AUTO_QUICK_MODEL_ID);
            assert_ne!(model.public_model_id, AUTO_POWERFUL_MODEL_ID);

            let mut route_providers = BTreeSet::new();
            for route in model.routes {
                assert!(registry.provider(route.provider).is_some());
                assert!(route_providers.insert(route.provider));
                assert!(!route.provider_model_id.trim().is_empty());
                assert!(!route.response_model_id.trim().is_empty());
                assert_eq!(route.response_model_id, model.public_model_id);
            }
        }

        let ordered_model_ids = registry
            .completion_models()
            .iter()
            .map(|model| model.public_model_id)
            .collect::<Vec<_>>();
        assert_eq!(
            ordered_model_ids,
            vec![
                QUICK_MODEL_ID,
                "gemma4-31b",
                KIMI_K3_MODEL_ID,
                GLM_5_3_MODEL_ID,
                GLM_5_3_FLASH_MODEL_ID,
                DEEPSEEK_V4_1_FLASH_MODEL_ID,
                "llama3-3-70b",
                "gpt-oss-safeguard-120b",
            ]
        );
    }

    #[test]
    fn registry_covers_exactly_enabled_api_completion_models() {
        let expected = enabled_api_completion_model_ids().collect::<BTreeSet<_>>();
        let registered = PROVIDER_REGISTRY
            .completion_models()
            .iter()
            .map(|model| model.public_model_id)
            .collect::<BTreeSet<_>>();

        assert_eq!(registered, expected);
    }

    #[test]
    fn registry_pins_every_provider_model_translation() {
        let routes = PROVIDER_REGISTRY
            .completion_models()
            .iter()
            .flat_map(|model| {
                model.routes.iter().map(move |route| {
                    (
                        model.public_model_id,
                        route.provider,
                        route.provider_model_id,
                        route.response_model_id,
                    )
                })
            })
            .collect::<Vec<_>>();

        assert_eq!(
            routes,
            vec![
                (
                    QUICK_MODEL_ID,
                    ProviderId::Tinfoil,
                    QUICK_MODEL_ID,
                    QUICK_MODEL_ID
                ),
                (
                    "gemma4-31b",
                    ProviderId::Tinfoil,
                    "gemma4-31b",
                    "gemma4-31b"
                ),
                (
                    KIMI_K3_MODEL_ID,
                    ProviderId::Tinfoil,
                    KIMI_K3_MODEL_ID,
                    KIMI_K3_MODEL_ID
                ),
                (
                    GLM_5_3_MODEL_ID,
                    ProviderId::Continuum,
                    "glm-5.3",
                    GLM_5_3_MODEL_ID
                ),
                (
                    GLM_5_3_MODEL_ID,
                    ProviderId::Tinfoil,
                    GLM_5_3_MODEL_ID,
                    GLM_5_3_MODEL_ID
                ),
                (
                    GLM_5_3_FLASH_MODEL_ID,
                    ProviderId::Continuum,
                    "glm-5.3-flash",
                    GLM_5_3_FLASH_MODEL_ID
                ),
                (
                    GLM_5_3_FLASH_MODEL_ID,
                    ProviderId::Tinfoil,
                    GLM_5_3_FLASH_MODEL_ID,
                    GLM_5_3_FLASH_MODEL_ID
                ),
                (
                    DEEPSEEK_V4_1_FLASH_MODEL_ID,
                    ProviderId::Tinfoil,
                    DEEPSEEK_V4_1_FLASH_MODEL_ID,
                    DEEPSEEK_V4_1_FLASH_MODEL_ID
                ),
                (
                    "llama3-3-70b",
                    ProviderId::Tinfoil,
                    "llama3-3-70b",
                    "llama3-3-70b"
                ),
                (
                    "gpt-oss-safeguard-120b",
                    ProviderId::Tinfoil,
                    "gpt-oss-safeguard-120b",
                    "gpt-oss-safeguard-120b"
                ),
            ]
        );
    }

    #[test]
    fn registry_pins_provider_specific_rate_limit_scopes() {
        let scopes = PROVIDER_REGISTRY
            .completion_models()
            .iter()
            .flat_map(|model| {
                model.routes.iter().map(move |route| {
                    (
                        route.provider,
                        route.provider_model_id,
                        route.rate_limit_scope,
                    )
                })
            })
            .collect::<Vec<_>>();

        assert!(scopes.iter().all(|(provider, _, scope)| match provider {
            ProviderId::Tinfoil => *scope == RateLimitScope::ProviderModel,
            ProviderId::Continuum => *scope == RateLimitScope::ProviderAccount,
        }));
        assert!(scopes
            .iter()
            .any(|(provider, model, scope)| *provider == ProviderId::Tinfoil
                && *model == KIMI_K3_MODEL_ID
                && *scope == RateLimitScope::ProviderModel));
        assert!(scopes.iter().any(|(provider, model, scope)| {
            *provider == ProviderId::Continuum
                && *model == "glm-5.3"
                && *scope == RateLimitScope::ProviderAccount
        }));
        assert!(scopes.iter().any(|(provider, model, scope)| {
            *provider == ProviderId::Tinfoil
                && *model == GLM_5_3_MODEL_ID
                && *scope == RateLimitScope::ProviderModel
        }));
        assert!(scopes.iter().any(|(provider, model, scope)| {
            *provider == ProviderId::Continuum
                && *model == "glm-5.3-flash"
                && *scope == RateLimitScope::ProviderAccount
        }));
        assert!(scopes.iter().any(|(provider, model, scope)| {
            *provider == ProviderId::Tinfoil
                && *model == GLM_5_3_FLASH_MODEL_ID
                && *scope == RateLimitScope::ProviderModel
        }));
        assert!(!scopes.iter().any(|(_, model, _)| {
            *model == "glm-5-2" || *model == "glm-5.2" || *model == "deepseek-v4-flash"
        }));
    }

    #[test]
    fn glm_flash_uses_a_seventy_five_percent_continuum_split() {
        let flash = PROVIDER_REGISTRY
            .completion_model(GLM_5_3_FLASH_MODEL_ID)
            .expect("Flash model");
        assert_eq!(
            flash
                .routes
                .iter()
                .map(|route| (route.provider, route.weight))
                .collect::<Vec<_>>(),
            vec![(ProviderId::Continuum, 700), (ProviderId::Tinfoil, 100)]
        );

        let effective_weight = |provider| {
            let route = flash
                .routes
                .iter()
                .find(|route| route.provider == provider)
                .unwrap();
            u32::from(PROVIDER_REGISTRY.provider(provider).unwrap().weight)
                * u32::from(route.weight)
        };
        let tinfoil = effective_weight(ProviderId::Tinfoil);
        let continuum = effective_weight(ProviderId::Continuum);
        assert_eq!(continuum * 100 / (tinfoil + continuum), 75);
    }
}
