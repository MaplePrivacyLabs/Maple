//! Credential-free provider topology for completion routing.
//!
//! This registry is intentionally independent from executable credentials.
//! Completion routing consumes its route eligibility and weights for every
//! completion model.

use crate::model_config::{
    DECISION_LARGE_MODEL_ID, DECISION_MEDIUM_MODEL_ID, DECISION_SMALL_MODEL_ID,
    DEEPSEEK_V4_1_FLASH_MODEL_ID, DEEPSEEK_V4_1_FLASH_UNCENSORED_MODEL_ID, GLM_5_3_FLASH_MODEL_ID,
    GLM_5_3_MODEL_ID, KIMI_K3_MODEL_ID, QUICK_MODEL_ID,
};

pub(crate) const SHADOW_ROUTING_POLICY_VERSION: &str = "routing-v2-weighted-v6";
pub(crate) const MAPLE_ROUTING_POLICY_VERSION: &str = "routing-v2-maple-instances-v6";

/// A backend-owned dedicated inference deployment. Its identity is internal to
/// routing; customer pricing and accounting continue to use the Maple family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum MapleInstanceId {
    I01,
    I02,
}

impl MapleInstanceId {
    pub(crate) const ALL: [Self; 2] = [Self::I01, Self::I02];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum ProviderId {
    Tinfoil,
    Continuum,
    Maple(MapleInstanceId),
}

impl ProviderId {
    /// Internal route identity, including the Maple deployment when present.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Tinfoil => "tinfoil",
            Self::Continuum => "continuum",
            Self::Maple(MapleInstanceId::I01) => "maple-i01",
            Self::Maple(MapleInstanceId::I02) => "maple-i02",
        }
    }

    /// Stable logical provider for billing, request policy and cache namespaces.
    pub(crate) const fn family_name(self) -> &'static str {
        match self {
            Self::Maple(_) => "maple",
            _ => self.as_str(),
        }
    }

    pub(crate) const fn is_maple(self) -> bool {
        matches!(self, Self::Maple(_))
    }

    /// Parses only compiled internal identities, never an arbitrary endpoint.
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        match name {
            "tinfoil" => Some(Self::Tinfoil),
            "continuum" => Some(Self::Continuum),
            "maple-i01" => Some(Self::Maple(MapleInstanceId::I01)),
            "maple-i02" => Some(Self::Maple(MapleInstanceId::I02)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteSelectionSource {
    StaticSplit,
    Fallback,
    /// Routing kept the account's remembered same-model provider instead of
    /// its weighted bucket while that provider stays eligible.
    Sticky,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProviderSpec {
    pub(crate) id: ProviderId,
    pub(crate) weight: u16,
    pub(crate) enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ModelRouteSpec {
    pub(crate) provider: ProviderId,
    pub(crate) provider_model_id: &'static str,
    pub(crate) response_model_id: &'static str,
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

/// The routes of a typed-decision model. These serve System One only and never take part in
/// completion routing; they are here so route health covers them like every other route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DecisionModelSpec {
    pub(crate) public_model_id: &'static str,
    pub(crate) routes: &'static [ModelRouteSpec],
}

#[derive(Debug)]
pub(crate) struct ProviderRegistry {
    providers: &'static [ProviderSpec],
    completion_models: &'static [CompletionModelSpec],
    decision_models: &'static [DecisionModelSpec],
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

    pub(crate) fn decision_model(&self, public_model_id: &str) -> Option<&DecisionModelSpec> {
        self.decision_models
            .iter()
            .find(|model| model.public_model_id == public_model_id)
    }

    pub(crate) fn decision_models(&self) -> &'static [DecisionModelSpec] {
        self.decision_models
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
    ProviderSpec {
        id: ProviderId::Maple(MapleInstanceId::I01),
        weight: 100,
        enabled: true,
    },
    ProviderSpec {
        id: ProviderId::Maple(MapleInstanceId::I02),
        weight: 100,
        enabled: true,
    },
];

const GPT_OSS_120B_ROUTES: &[ModelRouteSpec] = &[ModelRouteSpec {
    provider: ProviderId::Tinfoil,
    provider_model_id: QUICK_MODEL_ID,
    response_model_id: QUICK_MODEL_ID,
    weight: 100,
    enabled: true,
}];

const GEMMA4_31B_ROUTES: &[ModelRouteSpec] = &[ModelRouteSpec {
    provider: ProviderId::Tinfoil,
    provider_model_id: "gemma4-31b",
    response_model_id: "gemma4-31b",
    weight: 100,
    enabled: true,
}];

const KIMI_K3_ROUTES: &[ModelRouteSpec] = &[ModelRouteSpec {
    provider: ProviderId::Tinfoil,
    provider_model_id: KIMI_K3_MODEL_ID,
    response_model_id: KIMI_K3_MODEL_ID,
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
        weight: 700,
        enabled: true,
    },
    ModelRouteSpec {
        provider: ProviderId::Tinfoil,
        provider_model_id: GLM_5_3_MODEL_ID,
        response_model_id: GLM_5_3_MODEL_ID,
        weight: 100,
        enabled: true,
    },
];

// GLM 5.3 Flash is served exclusively by Continuum (PrivateMode).
const GLM_5_3_FLASH_ROUTES: &[ModelRouteSpec] = &[ModelRouteSpec {
    provider: ProviderId::Continuum,
    provider_model_id: "glm-5.3-flash",
    response_model_id: GLM_5_3_FLASH_MODEL_ID,
    weight: 700,
    enabled: true,
}];

// Provider multipliers allocate 5% of fresh stock DeepSeek account buckets to
// shared Tinfoil and 95% to dedicated i01: 70 * 20 : 100 * 266. i02 remains
// reserved for its distinct model. Eligible sticky routes and health gates
// retain their normal precedence over this initial allocation.
const DEEPSEEK_V4_1_FLASH_ROUTES: &[ModelRouteSpec] = &[
    ModelRouteSpec {
        provider: ProviderId::Tinfoil,
        provider_model_id: DEEPSEEK_V4_1_FLASH_MODEL_ID,
        response_model_id: DEEPSEEK_V4_1_FLASH_MODEL_ID,
        weight: 20,
        enabled: true,
    },
    ModelRouteSpec {
        provider: ProviderId::Maple(MapleInstanceId::I01),
        provider_model_id: DEEPSEEK_V4_1_FLASH_MODEL_ID,
        response_model_id: DEEPSEEK_V4_1_FLASH_MODEL_ID,
        weight: 266,
        enabled: true,
    },
];

// A distinct checkpoint has a distinct identity at every boundary. Stock
// DeepSeek providers are not fallbacks for the uncensored model.
const DEEPSEEK_V4_1_FLASH_UNCENSORED_ROUTES: &[ModelRouteSpec] = &[ModelRouteSpec {
    provider: ProviderId::Maple(MapleInstanceId::I02),
    provider_model_id: DEEPSEEK_V4_1_FLASH_UNCENSORED_MODEL_ID,
    response_model_id: DEEPSEEK_V4_1_FLASH_UNCENSORED_MODEL_ID,
    weight: 100,
    enabled: true,
}];

// A test-only route table proves that Router V2 still supports two independent
// Maple replicas of the same public model after i02 leaves the production route.
#[cfg(test)]
const DUAL_INSTANCE_TEST_ROUTES: &[ModelRouteSpec] = &[
    ModelRouteSpec {
        provider: ProviderId::Tinfoil,
        provider_model_id: DEEPSEEK_V4_1_FLASH_MODEL_ID,
        response_model_id: DEEPSEEK_V4_1_FLASH_MODEL_ID,
        weight: 20,
        enabled: true,
    },
    ModelRouteSpec {
        provider: ProviderId::Maple(MapleInstanceId::I01),
        provider_model_id: DEEPSEEK_V4_1_FLASH_MODEL_ID,
        response_model_id: DEEPSEEK_V4_1_FLASH_MODEL_ID,
        weight: 133,
        enabled: true,
    },
    ModelRouteSpec {
        provider: ProviderId::Maple(MapleInstanceId::I02),
        provider_model_id: DEEPSEEK_V4_1_FLASH_MODEL_ID,
        response_model_id: DEEPSEEK_V4_1_FLASH_MODEL_ID,
        weight: 133,
        enabled: true,
    },
];

#[cfg(test)]
const DUAL_INSTANCE_TEST_MODELS: &[CompletionModelSpec] = &[completion_model(
    DEEPSEEK_V4_1_FLASH_MODEL_ID,
    DUAL_INSTANCE_TEST_ROUTES,
)];

#[cfg(test)]
pub(crate) static DUAL_INSTANCE_TEST_REGISTRY: ProviderRegistry = ProviderRegistry {
    providers: PROVIDERS,
    completion_models: DUAL_INSTANCE_TEST_MODELS,
    decision_models: &[],
};

const LLAMA3_3_70B_ROUTES: &[ModelRouteSpec] = &[ModelRouteSpec {
    provider: ProviderId::Tinfoil,
    provider_model_id: "llama3-3-70b",
    response_model_id: "llama3-3-70b",
    weight: 100,
    enabled: true,
}];

const GPT_OSS_SAFEGUARD_120B_ROUTES: &[ModelRouteSpec] = &[ModelRouteSpec {
    provider: ProviderId::Tinfoil,
    provider_model_id: "gpt-oss-safeguard-120b",
    response_model_id: "gpt-oss-safeguard-120b",
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
    completion_model(
        DEEPSEEK_V4_1_FLASH_UNCENSORED_MODEL_ID,
        DEEPSEEK_V4_1_FLASH_UNCENSORED_ROUTES,
    ),
    completion_model("llama3-3-70b", LLAMA3_3_70B_ROUTES),
    completion_model("gpt-oss-safeguard-120b", GPT_OSS_SAFEGUARD_120B_ROUTES),
];

// Tinfoil serves the decision models under the same names as our public IDs. They have no
// other provider.
const fn tinfoil_decision_route(model: &'static str) -> ModelRouteSpec {
    ModelRouteSpec {
        provider: ProviderId::Tinfoil,
        provider_model_id: model,
        response_model_id: model,
        weight: 100,
        enabled: true,
    }
}

const DECISION_SMALL_ROUTES: &[ModelRouteSpec] = &[tinfoil_decision_route(DECISION_SMALL_MODEL_ID)];
const DECISION_MEDIUM_ROUTES: &[ModelRouteSpec] =
    &[tinfoil_decision_route(DECISION_MEDIUM_MODEL_ID)];
const DECISION_LARGE_ROUTES: &[ModelRouteSpec] = &[tinfoil_decision_route(DECISION_LARGE_MODEL_ID)];

const DECISION_MODELS: &[DecisionModelSpec] = &[
    DecisionModelSpec {
        public_model_id: DECISION_SMALL_MODEL_ID,
        routes: DECISION_SMALL_ROUTES,
    },
    DecisionModelSpec {
        public_model_id: DECISION_MEDIUM_MODEL_ID,
        routes: DECISION_MEDIUM_ROUTES,
    },
    DecisionModelSpec {
        public_model_id: DECISION_LARGE_MODEL_ID,
        routes: DECISION_LARGE_ROUTES,
    },
];

pub(crate) static PROVIDER_REGISTRY: ProviderRegistry = ProviderRegistry {
    providers: PROVIDERS,
    completion_models: COMPLETION_MODELS,
    decision_models: DECISION_MODELS,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_config::{
        enabled_api_completion_model_ids, AUTO_POWERFUL_MODEL_ID, AUTO_QUICK_MODEL_ID,
    };
    use std::collections::BTreeSet;

    #[test]
    fn uncensored_model_is_distinct_and_has_only_i02_without_stock_fallback() {
        let derivative = PROVIDER_REGISTRY
            .completion_model(DEEPSEEK_V4_1_FLASH_UNCENSORED_MODEL_ID)
            .unwrap();
        assert_eq!(derivative.routes.len(), 1);
        let route = derivative.routes[0];
        assert_eq!(route.provider, ProviderId::Maple(MapleInstanceId::I02));
        assert_eq!(
            route.provider_model_id,
            DEEPSEEK_V4_1_FLASH_UNCENSORED_MODEL_ID
        );
        assert_eq!(
            route.response_model_id,
            DEEPSEEK_V4_1_FLASH_UNCENSORED_MODEL_ID
        );
        for route in PROVIDER_REGISTRY
            .completion_model(DEEPSEEK_V4_1_FLASH_MODEL_ID)
            .unwrap()
            .routes
        {
            assert_ne!(route.provider, ProviderId::Maple(MapleInstanceId::I02));
            assert_eq!(route.provider_model_id, DEEPSEEK_V4_1_FLASH_MODEL_ID);
        }
    }

    #[test]
    fn internal_instance_identity_is_distinct_from_logical_provider_family() {
        for instance in MapleInstanceId::ALL {
            let provider = ProviderId::Maple(instance);
            assert!(provider.is_maple());
            assert_eq!(provider.family_name(), "maple");
            assert_eq!(ProviderId::from_name(provider.as_str()), Some(provider));
            assert_ne!(provider.as_str(), provider.family_name());
        }
        assert_ne!(
            ProviderId::Maple(MapleInstanceId::I01),
            ProviderId::Maple(MapleInstanceId::I02)
        );
        for provider in [ProviderId::Tinfoil, ProviderId::Continuum] {
            assert!(!provider.is_maple());
            assert_eq!(provider.family_name(), provider.as_str());
            assert_eq!(ProviderId::from_name(provider.as_str()), Some(provider));
        }
        for name in [
            "maple",
            "maple-i03",
            "maple-i01.evil",
            "MAPLE-I01",
            "https://i01.tfinf.maple.ai",
        ] {
            assert_eq!(ProviderId::from_name(name), None);
        }
    }

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
                DEEPSEEK_V4_1_FLASH_UNCENSORED_MODEL_ID,
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
                    DEEPSEEK_V4_1_FLASH_MODEL_ID,
                    ProviderId::Tinfoil,
                    DEEPSEEK_V4_1_FLASH_MODEL_ID,
                    DEEPSEEK_V4_1_FLASH_MODEL_ID
                ),
                (
                    DEEPSEEK_V4_1_FLASH_MODEL_ID,
                    ProviderId::Maple(MapleInstanceId::I01),
                    DEEPSEEK_V4_1_FLASH_MODEL_ID,
                    DEEPSEEK_V4_1_FLASH_MODEL_ID
                ),
                (
                    DEEPSEEK_V4_1_FLASH_UNCENSORED_MODEL_ID,
                    ProviderId::Maple(MapleInstanceId::I02),
                    DEEPSEEK_V4_1_FLASH_UNCENSORED_MODEL_ID,
                    DEEPSEEK_V4_1_FLASH_UNCENSORED_MODEL_ID
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
    fn glm_flash_has_only_the_continuum_route() {
        let flash = PROVIDER_REGISTRY
            .completion_model(GLM_5_3_FLASH_MODEL_ID)
            .expect("Flash model");
        assert_eq!(flash.routes.len(), 1);
        assert_eq!(flash.routes[0].provider, ProviderId::Continuum);
        assert_eq!(flash.routes[0].provider_model_id, "glm-5.3-flash");
        assert!(flash.routes[0].enabled);
    }

    #[test]
    fn stock_deepseek_has_shared_and_i01_routes_under_one_public_identity() {
        let model = PROVIDER_REGISTRY
            .completion_model(DEEPSEEK_V4_1_FLASH_MODEL_ID)
            .expect("canonical DeepSeek model");
        assert_eq!(model.routes.len(), 2);
        assert_eq!(
            model
                .routes
                .iter()
                .map(|route| route.provider)
                .collect::<Vec<_>>(),
            [ProviderId::Tinfoil, ProviderId::Maple(MapleInstanceId::I01)]
        );
        assert_eq!(
            model
                .routes
                .iter()
                .map(|route| route.weight)
                .collect::<Vec<_>>(),
            [20, 266]
        );
        for route in model.routes {
            assert_eq!(route.provider_model_id, DEEPSEEK_V4_1_FLASH_MODEL_ID);
            assert_eq!(route.response_model_id, DEEPSEEK_V4_1_FLASH_MODEL_ID);
            assert!(route.enabled);
        }
        assert!(PROVIDER_REGISTRY
            .completion_model("deepseek-v4-1-flash-1017")
            .is_none());
    }

    #[test]
    fn deepseek_unification_preserves_auto_defaults_and_candidates() {
        use crate::inference::auto_model::auto_model_candidates;
        use crate::inference::ModelSelectionMode;
        use crate::model_config::{ModelAliasTargets, ModelPlan};
        let paid = ModelAliasTargets::for_plan(ModelPlan::PAID);
        assert_eq!(paid.resolve("auto:quick"), GLM_5_3_FLASH_MODEL_ID);
        assert_eq!(paid.resolve("auto:powerful"), GLM_5_3_MODEL_ID);
        assert_eq!(
            auto_model_candidates(ModelSelectionMode::AutoQuick, ModelPlan::PAID).unwrap(),
            [GLM_5_3_FLASH_MODEL_ID, DEEPSEEK_V4_1_FLASH_MODEL_ID]
        );
        assert_eq!(
            auto_model_candidates(ModelSelectionMode::AutoPowerful, ModelPlan::PAID).unwrap(),
            [GLM_5_3_MODEL_ID, KIMI_K3_MODEL_ID]
        );
    }

    #[test]
    fn decision_models_have_one_tinfoil_route_each_and_are_not_completion_models() {
        use crate::model_config::decision_models;
        let registered: Vec<&str> = PROVIDER_REGISTRY
            .decision_models()
            .iter()
            .map(|model| model.public_model_id)
            .collect();
        let configured: Vec<&str> = decision_models().iter().map(|model| model.id).collect();
        assert_eq!(registered, configured);
        for id in configured {
            let model = PROVIDER_REGISTRY.decision_model(id).expect("registered");
            assert_eq!(model.routes.len(), 1, "{id}");
            let route = model.routes[0];
            assert_eq!(route.provider, ProviderId::Tinfoil);
            assert_eq!(route.provider_model_id, id);
            assert_eq!(route.response_model_id, id);
            assert!(route.enabled);
            assert!(PROVIDER_REGISTRY.completion_model(id).is_none(), "{id}");
        }
        assert!(PROVIDER_REGISTRY
            .decision_model(GLM_5_3_FLASH_MODEL_ID)
            .is_none());
        assert!(DUAL_INSTANCE_TEST_REGISTRY.decision_models().is_empty());
    }
}
