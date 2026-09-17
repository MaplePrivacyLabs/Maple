use crate::inference::health::{
    ProbeClaimResult, ProbeLease, ShadowDisposition, ShadowHealthState, ShadowObservationMode,
    ShadowObservationReport, ShadowRouteSnapshot, MIN_CAPACITY_COOLDOWN,
};
use crate::inference::{AttemptTerminal, InferenceIntent, RouteIdentity, RouteKey};
use crate::inference_planning::{
    plan_completion_route, ConfiguredProviders, ProviderPreference, RoutePlan, RoutePlanningError,
    RoutePlanningInput,
};
use crate::model_config::{
    resolve_completion_model_id, resolve_public_model_id, GLM_5_3_FLASH_MODEL_ID, GLM_5_3_MODEL_ID,
};
use crate::os_flags::{GLM_5_3_FLASH_CONTINUUM_FLAG_KEY, GLM_5_3_TINFOIL_FLAG_KEY};
use crate::provider_registry::{
    ProviderId, ProviderRegistry, RouteSelectionSource, PROVIDER_REGISTRY,
};
use crate::proxy_config::{canonicalize_tinfoil_model, ProxyConfig, ProxyRouter};
use std::collections::HashSet;
use std::time::Duration;
use uuid::Uuid;

/// Selects the completion-routing implementation once at an authenticated
/// inference entrypoint. The choice is carried through the complete logical
/// request so feature-flag changes cannot switch routers between provider
/// turns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum InferenceRoutingMode {
    #[default]
    Legacy,
    V2,
}

impl InferenceRoutingMode {
    pub(crate) const fn from_router_v2_flag(value: Option<bool>) -> Self {
        match value {
            Some(true) => Self::V2,
            Some(false) | None => Self::Legacy,
        }
    }
}
#[derive(Debug, Clone, Copy)]
struct ProviderConfig {
    provider: ProviderId,
    weight: u16,
    enabled: bool,
}

#[derive(Debug, Clone, Copy)]
struct ModelProviderRoute {
    provider: ProviderId,
    provider_model_id: &'static str,
    weight: u16,
    enabled: bool,
    requires_explicit_preference: bool,
}

#[derive(Debug, Clone, Copy)]
struct ModelRoutingConfig {
    public_model_id: &'static str,
    routes: &'static [ModelProviderRoute],
    provider_flag: Option<ProviderRoutingFlag>,
    default_provider: Option<ProviderId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProviderRoutingFlag {
    key: &'static str,
    enabled_provider: ProviderId,
    disabled_provider: ProviderId,
}

impl ProviderRoutingFlag {
    pub(crate) const fn key(self) -> &'static str {
        self.key
    }

    pub(crate) const fn preference_for(self, enabled: bool) -> ProviderPreference {
        ProviderPreference::feature_flag(if enabled {
            self.enabled_provider
        } else {
            self.disabled_provider
        })
    }
}

#[derive(Debug)]
struct ProviderRoutingConfig {
    providers: &'static [ProviderConfig],
    models: &'static [ModelRoutingConfig],
}

#[derive(Debug, Clone)]
pub(crate) struct SelectedProviderRoute {
    pub(crate) provider: ProviderId,
    pub(crate) proxy: ProxyConfig,
    pub(crate) public_model_id: String,
    pub(crate) provider_model_id: String,
    pub(crate) response_model_id: String,
    pub(crate) bucket: Option<u8>,
    pub(crate) selection_source: RouteSelectionSource,
}

impl SelectedProviderRoute {
    pub(crate) fn identity(&self) -> RouteIdentity {
        RouteIdentity::new(
            self.provider,
            self.public_model_id.clone(),
            self.provider_model_id.clone(),
            self.response_model_id.clone(),
            self.selection_source,
            self.bucket,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProviderRoutingError {
    UnsupportedModel(String),
    NoEligibleRoute(String),
    CapacityUnavailable {
        model: String,
        retry_after: Duration,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CredentialFreeRouteOutcome {
    Selected(RouteIdentity),
    UnsupportedModel,
    NoEligibleRoute,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ShadowRouteComparison {
    Match {
        outcome: CredentialFreeRouteOutcome,
        decision: Option<crate::inference_planning::PlanDecision>,
        candidate_count: usize,
    },
    Mismatch {
        active: CredentialFreeRouteOutcome,
        shadow: CredentialFreeRouteOutcome,
        decision: Option<crate::inference_planning::PlanDecision>,
        candidate_count: usize,
    },
}

#[derive(Debug)]
pub(crate) struct ProviderRouter {
    config: &'static ProviderRoutingConfig,
    registry: &'static ProviderRegistry,
    shadow_health: ShadowHealthState,
}

#[derive(Debug, Clone)]
struct EligibleRoute {
    provider: ProviderId,
    proxy: ProxyConfig,
    provider_model_id: &'static str,
    effective_weight: u32,
}

const PROVIDERS: &[ProviderConfig] = &[
    ProviderConfig {
        provider: ProviderId::Tinfoil,
        weight: 70,
        enabled: true,
    },
    ProviderConfig {
        provider: ProviderId::Continuum,
        weight: 30,
        enabled: true,
    },
];

const GLM_5_3_ROUTES: &[ModelProviderRoute] = &[
    ModelProviderRoute {
        provider: ProviderId::Continuum,
        provider_model_id: "glm-5.3",
        weight: 100,
        enabled: true,
        requires_explicit_preference: false,
    },
    ModelProviderRoute {
        // Preserve the current Router v1 rollout fence. Router v2's separate
        // registry treats the now-GA Tinfoil route as a normal same-model
        // candidate without changing behavior for the feature-flag-off cohort.
        provider: ProviderId::Tinfoil,
        provider_model_id: GLM_5_3_MODEL_ID,
        weight: 100,
        enabled: true,
        requires_explicit_preference: true,
    },
];

const GLM_5_3_FLASH_ROUTES: &[ModelProviderRoute] = &[
    ModelProviderRoute {
        // V1 opts accounts into Continuum explicitly; absent/failed flag lookups
        // keep Flash on Tinfoil. V2 uses its separate weighted registry.
        provider: ProviderId::Continuum,
        provider_model_id: "glm-5.3-flash",
        weight: 100,
        enabled: true,
        requires_explicit_preference: true,
    },
    ModelProviderRoute {
        provider: ProviderId::Tinfoil,
        provider_model_id: GLM_5_3_FLASH_MODEL_ID,
        weight: 100,
        enabled: true,
        requires_explicit_preference: false,
    },
];

const MODEL_ROUTES: &[ModelRoutingConfig] = &[
    ModelRoutingConfig {
        public_model_id: GLM_5_3_MODEL_ID,
        routes: GLM_5_3_ROUTES,
        provider_flag: Some(ProviderRoutingFlag {
            key: GLM_5_3_TINFOIL_FLAG_KEY,
            enabled_provider: ProviderId::Tinfoil,
            disabled_provider: ProviderId::Continuum,
        }),
        default_provider: Some(ProviderId::Continuum),
    },
    ModelRoutingConfig {
        public_model_id: GLM_5_3_FLASH_MODEL_ID,
        routes: GLM_5_3_FLASH_ROUTES,
        provider_flag: Some(ProviderRoutingFlag {
            key: GLM_5_3_FLASH_CONTINUUM_FLAG_KEY,
            enabled_provider: ProviderId::Continuum,
            disabled_provider: ProviderId::Tinfoil,
        }),
        default_provider: Some(ProviderId::Tinfoil),
    },
];

static DEFAULT_PROVIDER_ROUTING_CONFIG: ProviderRoutingConfig = ProviderRoutingConfig {
    providers: PROVIDERS,
    models: MODEL_ROUTES,
};

impl Default for ProviderRouter {
    fn default() -> Self {
        Self {
            config: &DEFAULT_PROVIDER_ROUTING_CONFIG,
            registry: &PROVIDER_REGISTRY,
            shadow_health: ShadowHealthState::new(&PROVIDER_REGISTRY),
        }
    }
}

impl ProviderRouter {
    pub(crate) fn observe_attempt_terminal(
        &self,
        terminal: &AttemptTerminal,
        mode: ShadowObservationMode,
    ) -> ShadowObservationReport {
        self.shadow_health.observe_terminal(terminal, mode)
    }

    pub(crate) fn observe_attempt_terminal_with_probe(
        &self,
        terminal: &AttemptTerminal,
        mode: ShadowObservationMode,
        probe: Option<ProbeLease>,
    ) -> ShadowObservationReport {
        self.shadow_health
            .observe_terminal_with_probe(terminal, mode, probe)
    }

    pub(crate) fn try_claim_probe(&self, route: &RouteKey) -> ProbeClaimResult {
        self.shadow_health.try_claim_probe(route)
    }

    #[cfg(test)]
    pub(crate) fn try_claim_probe_at(
        &self,
        route: &RouteKey,
        now: std::time::Instant,
    ) -> ProbeClaimResult {
        self.shadow_health.try_claim_probe_at(route, now)
    }

    pub(crate) fn shadow_health_snapshot(&self, route: &RouteKey) -> Option<ShadowRouteSnapshot> {
        self.shadow_health.snapshot(route)
    }

    #[cfg(test)]
    pub(crate) fn shadow_observation_count(&self) -> usize {
        self.shadow_health.observation_count()
    }

    #[cfg(test)]
    pub(crate) fn select_completion_route(
        &self,
        proxy_router: &ProxyRouter,
        account_uuid: Uuid,
        requested_model: &str,
    ) -> Result<SelectedProviderRoute, ProviderRoutingError> {
        self.select_completion_route_with_preference(
            proxy_router,
            account_uuid,
            requested_model,
            None,
        )
    }

    pub(crate) fn select_completion_route_with_preference(
        &self,
        proxy_router: &ProxyRouter,
        account_uuid: Uuid,
        requested_model: &str,
        provider_preference: Option<ProviderPreference>,
    ) -> Result<SelectedProviderRoute, ProviderRoutingError> {
        if let Some(public_model_id) = resolve_public_model_id(requested_model) {
            if let Some(model_config) = self.model_config(public_model_id) {
                return self.select_configured_route(
                    proxy_router,
                    account_uuid,
                    model_config,
                    provider_preference,
                );
            }
        }

        self.fallback_completion_route(proxy_router, requested_model)
    }

    /// Dispatch completion routing through the request-scoped implementation
    /// selected at the public inference entrypoint. The legacy implementation
    /// stays intact while Router v2 ignores legacy provider preferences.
    pub(crate) fn select_completion_route_for_mode(
        &self,
        proxy_router: &ProxyRouter,
        intent: &InferenceIntent,
        provider_preference: Option<ProviderPreference>,
        routing_mode: InferenceRoutingMode,
    ) -> Result<SelectedProviderRoute, ProviderRoutingError> {
        match routing_mode {
            InferenceRoutingMode::Legacy => self.select_completion_route_with_preference(
                proxy_router,
                intent.account_uuid,
                &intent.public_model_id,
                provider_preference,
            ),
            InferenceRoutingMode::V2 => self.select_active_completion_route(proxy_router, intent),
        }
    }

    /// Selects the route used by a newly prepared logical request.
    ///
    /// Health filters every configured route for the already-resolved public
    /// model. It may select another provider for that same model, but it never
    /// substitutes a different public model.
    pub(crate) fn select_active_completion_route(
        &self,
        proxy_router: &ProxyRouter,
        intent: &InferenceIntent,
    ) -> Result<SelectedProviderRoute, ProviderRoutingError> {
        self.select_health_aware_route(proxy_router, intent, &HashSet::new())
    }

    /// Replans a not-yet-sent logical request while excluding route resources
    /// that lost eligibility or half-open ownership races. This never retries an
    /// upstream attempt; callers may use it only before the first send.
    pub(crate) fn select_active_completion_route_excluding(
        &self,
        proxy_router: &ProxyRouter,
        intent: &InferenceIntent,
        excluded_routes: &HashSet<RouteKey>,
    ) -> Result<SelectedProviderRoute, ProviderRoutingError> {
        self.select_health_aware_route(proxy_router, intent, excluded_routes)
    }

    pub(crate) fn shadow_completion_plan(
        &self,
        proxy_router: &ProxyRouter,
        intent: &InferenceIntent,
    ) -> Result<RoutePlan, RoutePlanningError> {
        let configured_providers = self.registry.providers().iter().fold(
            ConfiguredProviders::none(),
            |configured, provider| {
                if proxy_for_provider(proxy_router, provider.id).is_some() {
                    configured.with_provider(provider.id)
                } else {
                    configured
                }
            },
        );

        plan_completion_route(
            self.registry,
            RoutePlanningInput {
                intent,
                configured_providers,
            },
        )
    }

    fn select_health_aware_route(
        &self,
        proxy_router: &ProxyRouter,
        intent: &InferenceIntent,
        excluded_routes: &HashSet<RouteKey>,
    ) -> Result<SelectedProviderRoute, ProviderRoutingError> {
        let model = self
            .registry
            .completion_model(&intent.public_model_id)
            .ok_or_else(|| {
                ProviderRoutingError::UnsupportedModel(intent.public_model_id.clone())
            })?;

        let configured_routes = model
            .routes
            .iter()
            .filter_map(|route| {
                let provider = self.registry.provider(route.provider)?;
                if !route.enabled
                    || route.weight == 0
                    || !provider.enabled
                    || provider.weight == 0
                    || proxy_for_provider(proxy_router, route.provider).is_none()
                {
                    return None;
                }
                let route_key = RouteKey {
                    provider: route.provider,
                    provider_model_id: route.provider_model_id.to_string(),
                };
                if excluded_routes.contains(&route_key) {
                    return None;
                }
                Some((route.provider, route_key))
            })
            .collect::<Vec<_>>();

        if configured_routes.is_empty() {
            return Err(ProviderRoutingError::NoEligibleRoute(
                intent.public_model_id.clone(),
            ));
        }

        let route_keys = configured_routes
            .iter()
            .map(|(_, route)| route.clone())
            .collect::<Vec<_>>();
        let snapshots = self
            .shadow_health
            .snapshot_routes(&route_keys)
            .ok_or_else(|| ProviderRoutingError::CapacityUnavailable {
                model: intent.public_model_id.clone(),
                retry_after: MIN_CAPACITY_COOLDOWN,
            })?;

        let mut available_providers = ConfiguredProviders::none();
        let mut earliest_recovery = None;
        for ((provider, _), snapshot) in configured_routes.iter().zip(snapshots) {
            match snapshot.for_failover(model.failover) {
                ShadowDisposition::WouldOpen { remaining } => {
                    let remaining = ceil_retry_after(remaining);
                    earliest_recovery = Some(
                        earliest_recovery
                            .map_or(remaining, |current: Duration| current.min(remaining)),
                    );
                }
                ShadowDisposition::ProbeInFlight { retry_after } => {
                    let retry_after = ceil_retry_after(retry_after);
                    earliest_recovery = Some(
                        earliest_recovery
                            .map_or(retry_after, |current: Duration| current.min(retry_after)),
                    );
                }
                disposition if route_is_available_for_new_request(disposition) => {
                    available_providers = available_providers.with_provider(*provider);
                }
                _ => unreachable!("WouldOpen is handled above"),
            }
        }

        if available_providers == ConfiguredProviders::none() {
            return Err(ProviderRoutingError::CapacityUnavailable {
                model: intent.public_model_id.clone(),
                retry_after: earliest_recovery.unwrap_or(MIN_CAPACITY_COOLDOWN),
            });
        }

        let plan = plan_completion_route(
            self.registry,
            RoutePlanningInput {
                intent,
                configured_providers: available_providers,
            },
        )
        .map_err(provider_routing_error_from_plan)?;

        let selected = plan.selected;
        let proxy = proxy_for_provider(proxy_router, selected.provider)
            .ok_or_else(|| ProviderRoutingError::NoEligibleRoute(intent.public_model_id.clone()))?;
        Ok(SelectedProviderRoute {
            provider: selected.provider,
            proxy,
            public_model_id: selected.public_model_id,
            provider_model_id: selected.provider_model_id,
            response_model_id: selected.response_model_id,
            bucket: selected.bucket,
            selection_source: selected.selection_source,
        })
    }

    pub(crate) fn provider_routing_flag_for_completion_model(
        &self,
        requested_model: &str,
    ) -> Option<ProviderRoutingFlag> {
        let public_model_id = resolve_public_model_id(requested_model)?;
        self.model_config(public_model_id)?.provider_flag
    }

    fn select_configured_route(
        &self,
        proxy_router: &ProxyRouter,
        account_uuid: Uuid,
        model_config: &ModelRoutingConfig,
        provider_preference: Option<ProviderPreference>,
    ) -> Result<SelectedProviderRoute, ProviderRoutingError> {
        let mut eligible_routes = Vec::new();

        for route in model_config.routes {
            if !route.enabled || route.weight == 0 {
                continue;
            }
            if route.requires_explicit_preference
                && provider_preference
                    .is_none_or(|preference| preference.provider() != route.provider)
            {
                continue;
            }

            let Some(provider_config) = self.provider_config(route.provider) else {
                continue;
            };
            if !provider_config.enabled || provider_config.weight == 0 {
                continue;
            }

            let Some(proxy) = proxy_for_provider(proxy_router, route.provider) else {
                continue;
            };

            eligible_routes.push(EligibleRoute {
                provider: route.provider,
                proxy,
                provider_model_id: route.provider_model_id,
                effective_weight: u32::from(provider_config.weight) * u32::from(route.weight),
            });
        }

        if eligible_routes.is_empty() {
            return Err(ProviderRoutingError::NoEligibleRoute(
                model_config.public_model_id.into(),
            ));
        }

        let default_preference = model_config
            .default_provider
            .map(ProviderPreference::default_provider);

        let provider_preference_route = provider_preference.and_then(|preference| {
            eligible_routes
                .iter()
                .find(|route| route.provider == preference.provider())
                .map(|route| (route, preference.source()))
        });
        let default_preference_route = default_preference.and_then(|preference| {
            eligible_routes
                .iter()
                .find(|route| route.provider == preference.provider())
                .map(|route| {
                    let source = if provider_preference.is_some() {
                        RouteSelectionSource::Fallback
                    } else {
                        preference.source()
                    };
                    (route, source)
                })
        });

        let preferred_route = provider_preference_route.or(default_preference_route);

        let (selected, bucket, selection_source) = if let Some((route, source)) = preferred_route {
            (route, None, source)
        } else {
            let selected =
                select_weighted_route(account_uuid, &eligible_routes).ok_or_else(|| {
                    ProviderRoutingError::NoEligibleRoute(model_config.public_model_id.into())
                })?;
            (
                selected.route,
                Some(selected.bucket),
                if provider_preference.is_some() || default_preference.is_some() {
                    RouteSelectionSource::Fallback
                } else {
                    RouteSelectionSource::StaticSplit
                },
            )
        };

        Ok(SelectedProviderRoute {
            provider: selected.provider,
            proxy: selected.proxy.clone(),
            public_model_id: model_config.public_model_id.to_string(),
            provider_model_id: selected.provider_model_id.to_string(),
            response_model_id: model_config.public_model_id.to_string(),
            bucket,
            selection_source,
        })
    }

    fn fallback_completion_route(
        &self,
        proxy_router: &ProxyRouter,
        requested_model: &str,
    ) -> Result<SelectedProviderRoute, ProviderRoutingError> {
        let proxy = proxy_router.get_completion_proxy();
        let resolved_public_model_id =
            resolve_public_model_id(requested_model).map(ToOwned::to_owned);
        let provider_model_id = if proxy.provider_name == ProviderId::Tinfoil.as_str() {
            resolve_completion_model_id(requested_model)
                .ok_or_else(|| ProviderRoutingError::UnsupportedModel(requested_model.into()))?
                .to_string()
        } else {
            resolved_public_model_id
                .clone()
                .unwrap_or_else(|| requested_model.to_string())
        };

        let public_model_id = resolved_public_model_id.unwrap_or_else(|| provider_model_id.clone());

        let response_model_id = if proxy.provider_name == ProviderId::Tinfoil.as_str() {
            canonicalize_tinfoil_model(&provider_model_id)
        } else {
            public_model_id.clone()
        };

        Ok(SelectedProviderRoute {
            provider: ProviderId::Tinfoil,
            proxy,
            public_model_id,
            provider_model_id,
            response_model_id,
            bucket: None,
            selection_source: RouteSelectionSource::StaticSplit,
        })
    }

    fn provider_config(&self, provider: ProviderId) -> Option<&ProviderConfig> {
        self.config
            .providers
            .iter()
            .find(|config| config.provider == provider)
    }

    fn model_config(&self, public_model_id: &str) -> Option<&ModelRoutingConfig> {
        self.config
            .models
            .iter()
            .find(|config| config.public_model_id == public_model_id)
    }
}

pub(crate) fn compare_shadow_route(
    active: &Result<SelectedProviderRoute, ProviderRoutingError>,
    shadow: &Result<RoutePlan, RoutePlanningError>,
) -> ShadowRouteComparison {
    let active_outcome = match active {
        Ok(route) => CredentialFreeRouteOutcome::Selected(route.identity()),
        Err(ProviderRoutingError::UnsupportedModel(_)) => {
            CredentialFreeRouteOutcome::UnsupportedModel
        }
        Err(ProviderRoutingError::NoEligibleRoute(_)) => {
            CredentialFreeRouteOutcome::NoEligibleRoute
        }
        Err(ProviderRoutingError::CapacityUnavailable { .. }) => {
            CredentialFreeRouteOutcome::NoEligibleRoute
        }
    };
    let (shadow_outcome, decision, candidate_count) = match shadow {
        Ok(plan) => (
            CredentialFreeRouteOutcome::Selected(plan.selected.clone()),
            Some(plan.decision),
            plan.eligible_routes.len(),
        ),
        Err(RoutePlanningError::UnsupportedModel(_)) => {
            (CredentialFreeRouteOutcome::UnsupportedModel, None, 0)
        }
        Err(RoutePlanningError::NoEligibleRoute(_)) => {
            (CredentialFreeRouteOutcome::NoEligibleRoute, None, 0)
        }
    };

    if active_outcome == shadow_outcome {
        ShadowRouteComparison::Match {
            outcome: active_outcome,
            decision,
            candidate_count,
        }
    } else {
        ShadowRouteComparison::Mismatch {
            active: active_outcome,
            shadow: shadow_outcome,
            decision,
            candidate_count,
        }
    }
}

#[derive(Debug, Clone)]
struct WeightedSelection<'a> {
    route: &'a EligibleRoute,
    bucket: u8,
}

fn select_weighted_route<'a>(
    account_uuid: Uuid,
    routes: &'a [EligibleRoute],
) -> Option<WeightedSelection<'a>> {
    if routes.is_empty() {
        return None;
    }

    let total_weight = routes
        .iter()
        .map(|route| route.effective_weight)
        .sum::<u32>();
    if total_weight == 0 {
        return None;
    }

    let bucket = stable_account_bucket(account_uuid);
    let mut cumulative = 0u32;

    for (index, route) in routes.iter().enumerate() {
        let bucket_span = if index == routes.len() - 1 {
            100u32.saturating_sub(cumulative)
        } else {
            (route.effective_weight * 100) / total_weight
        };
        cumulative = cumulative.saturating_add(bucket_span);

        if u32::from(bucket) < cumulative || index == routes.len() - 1 {
            return Some(WeightedSelection { route, bucket });
        }
    }

    None
}

fn stable_account_bucket(account_uuid: Uuid) -> u8 {
    (u128::from_be_bytes(*account_uuid.as_bytes()) % 100) as u8
}

fn provider_routing_error_from_plan(error: RoutePlanningError) -> ProviderRoutingError {
    match error {
        RoutePlanningError::UnsupportedModel(model) => {
            ProviderRoutingError::UnsupportedModel(model)
        }
        RoutePlanningError::NoEligibleRoute(model) => ProviderRoutingError::NoEligibleRoute(model),
    }
}

fn ceil_retry_after(duration: Duration) -> Duration {
    let seconds = duration
        .as_secs()
        .saturating_add(u64::from(duration.subsec_nanos() > 0))
        .max(1);
    Duration::from_secs(seconds)
}

fn route_is_available_for_new_request(disposition: ShadowDisposition) -> bool {
    !matches!(
        disposition,
        ShadowDisposition::WouldOpen { .. } | ShadowDisposition::ProbeInFlight { .. }
    )
}

fn proxy_for_provider(proxy_router: &ProxyRouter, provider: ProviderId) -> Option<ProxyConfig> {
    match provider {
        ProviderId::Tinfoil => Some(proxy_router.get_tinfoil_proxy()),
        ProviderId::Continuum => {
            let proxy = proxy_router.get_default_proxy();
            (proxy.provider_name == ProviderId::Continuum.as_str()).then_some(proxy)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference::health::{ShadowDisposition, ShadowObservationMode};
    use crate::inference::{
        AttemptFailure, AttemptFailureKind, AttemptStage, AttemptTerminal, InferenceSurface,
        ReplaySafety, WorkloadClass,
    };
    use crate::model_config::{
        ModelAliasTargets, ModelPlan, PaidModelAliasOverrides, AUTO_POWERFUL_MODEL_ID,
        AUTO_QUICK_MODEL_ID, DEEPSEEK_V4_1_FLASH_MODEL_ID, GLM_5_3_FLASH_MODEL_ID,
        GLM_5_3_MODEL_ID, KIMI_K3_MODEL_ID, QUICK_MODEL_ID,
    };
    use crate::os_flags::PAID_POWERFUL_GLM_5_3_ALIAS_FLAG_KEY;
    use std::collections::HashMap;
    use std::time::Duration;

    fn proxy_router_with_both_providers() -> ProxyRouter {
        ProxyRouter::new(
            "http://continuum.example.com".to_string(),
            None,
            "http://tinfoil.example.com".to_string(),
        )
    }

    fn uuid_for_bucket(bucket: u8) -> Uuid {
        Uuid::from_u128(u128::from(bucket))
    }

    fn intent(requested_model: &str, public_model: &str) -> InferenceIntent {
        InferenceIntent::new(
            uuid_for_bucket(0),
            requested_model,
            public_model,
            ModelPlan::Paid,
            InferenceSurface::Responses,
            WorkloadClass::Interactive,
        )
    }

    fn capacity_terminal(
        provider: ProviderId,
        public_model: &str,
        provider_model: &str,
        status: u16,
        retry_after: Duration,
    ) -> AttemptTerminal {
        let mut failure = AttemptFailure::new(
            AttemptFailureKind::CapacityRejected,
            AttemptStage::AwaitingResponse,
            ReplaySafety::ProvenPreAcceptance,
        );
        failure.status = Some(status);
        failure.retry_after = Some(retry_after);
        let route = RouteIdentity::new(
            provider,
            public_model,
            provider_model,
            public_model,
            RouteSelectionSource::DefaultProvider,
            None,
        );
        AttemptTerminal::Failed {
            attempt: intent(public_model, public_model)
                .begin_execution()
                .begin_attempt(route),
            failure,
        }
    }

    fn route_failure_terminal(
        provider: ProviderId,
        public_model: &str,
        provider_model: &str,
    ) -> AttemptTerminal {
        let failure = AttemptFailure::new(
            AttemptFailureKind::StreamTimeout,
            AttemptStage::Stream,
            ReplaySafety::NotProvenPreAcceptance,
        );
        let route = RouteIdentity::new(
            provider,
            public_model,
            provider_model,
            public_model,
            RouteSelectionSource::DefaultProvider,
            None,
        );
        AttemptTerminal::Failed {
            attempt: intent(public_model, public_model)
                .begin_execution()
                .begin_attempt(route),
            failure,
        }
    }

    #[test]
    fn test_stable_account_bucket_uses_uuid_mod_100() {
        assert_eq!(stable_account_bucket(uuid_for_bucket(0)), 0);
        assert_eq!(stable_account_bucket(uuid_for_bucket(49)), 49);
        assert_eq!(stable_account_bucket(uuid_for_bucket(50)), 50);
        assert_eq!(stable_account_bucket(uuid_for_bucket(99)), 99);
    }

    #[test]
    fn router_v2_flag_is_strictly_opt_in() {
        assert_eq!(
            InferenceRoutingMode::from_router_v2_flag(Some(true)),
            InferenceRoutingMode::V2
        );
        assert_eq!(
            InferenceRoutingMode::from_router_v2_flag(Some(false)),
            InferenceRoutingMode::Legacy
        );
        assert_eq!(
            InferenceRoutingMode::from_router_v2_flag(None),
            InferenceRoutingMode::Legacy
        );
    }

    #[test]
    fn router_v2_glm_weights_are_independent_of_legacy_provider_preferences() {
        let router = ProviderRouter::default();
        let proxy_router = proxy_router_with_both_providers();
        let preferences = [
            None,
            Some(ProviderPreference::feature_flag(ProviderId::Continuum)),
            Some(ProviderPreference::feature_flag(ProviderId::Tinfoil)),
        ];

        for preference in preferences {
            let mut tinfoil_count = 0;
            let mut continuum_count = 0;
            for bucket in 0..100 {
                let mut intent = intent(GLM_5_3_MODEL_ID, GLM_5_3_MODEL_ID);
                intent.account_uuid = uuid_for_bucket(bucket);
                let legacy = router
                    .select_completion_route_for_mode(
                        &proxy_router,
                        &intent,
                        preference,
                        InferenceRoutingMode::Legacy,
                    )
                    .expect("legacy GLM route");
                let v2 = router
                    .select_completion_route_for_mode(
                        &proxy_router,
                        &intent,
                        preference,
                        InferenceRoutingMode::V2,
                    )
                    .expect("weighted Router v2 GLM route");

                // V1 retains its flag and missing-flag behavior for every account.
                let expected_legacy = preference
                    .map(ProviderPreference::provider)
                    .unwrap_or(ProviderId::Continuum);
                assert_eq!(legacy.provider, expected_legacy);
                assert_eq!(legacy.bucket, None);
                assert_eq!(
                    legacy.selection_source,
                    if preference.is_some() {
                        RouteSelectionSource::FeatureFlag
                    } else {
                        RouteSelectionSource::DefaultProvider
                    }
                );

                let (expected_v2, upstream_model) = if bucket < 30 {
                    continuum_count += 1;
                    (ProviderId::Continuum, "glm-5.3")
                } else {
                    tinfoil_count += 1;
                    (ProviderId::Tinfoil, GLM_5_3_MODEL_ID)
                };
                assert_eq!(v2.provider, expected_v2, "bucket {bucket}");
                assert_eq!(v2.provider_model_id, upstream_model);
                assert_eq!(v2.public_model_id, GLM_5_3_MODEL_ID);
                assert_eq!(v2.response_model_id, GLM_5_3_MODEL_ID);
                assert_eq!(v2.bucket, Some(bucket));
                assert_eq!(v2.selection_source, RouteSelectionSource::StaticSplit);
            }
            assert_eq!((tinfoil_count, continuum_count), (70, 30));
        }
    }

    #[test]
    fn router_v2_gate_is_the_only_glm_health_activation_boundary() {
        let router = ProviderRouter::default();
        let proxy_router = proxy_router_with_both_providers();
        let intent = intent(GLM_5_3_MODEL_ID, GLM_5_3_MODEL_ID);

        router.observe_attempt_terminal(
            &capacity_terminal(
                ProviderId::Continuum,
                GLM_5_3_MODEL_ID,
                "glm-5.3",
                429,
                Duration::from_secs(60),
            ),
            ShadowObservationMode::Update,
        );

        let legacy = router
            .select_completion_route_for_mode(
                &proxy_router,
                &intent,
                None,
                InferenceRoutingMode::Legacy,
            )
            .expect("legacy GLM route ignores Router v2 health");
        let v2 = router
            .select_completion_route_for_mode(
                &proxy_router,
                &intent,
                None,
                InferenceRoutingMode::V2,
            )
            .expect("Router v2 selects the healthy GLM route");

        assert_eq!(legacy.provider, ProviderId::Continuum);
        assert_eq!(v2.provider, ProviderId::Tinfoil);
        assert_eq!(legacy.public_model_id, v2.public_model_id);
    }

    #[test]
    fn router_v2_gate_is_the_only_all_model_health_activation_boundary() {
        let router = ProviderRouter::default();
        let proxy_router = proxy_router_with_both_providers();
        let intent = intent(KIMI_K3_MODEL_ID, KIMI_K3_MODEL_ID);

        router.observe_attempt_terminal(
            &capacity_terminal(
                ProviderId::Tinfoil,
                KIMI_K3_MODEL_ID,
                KIMI_K3_MODEL_ID,
                429,
                Duration::from_secs(60),
            ),
            ShadowObservationMode::Update,
        );

        let legacy = router
            .select_completion_route_for_mode(
                &proxy_router,
                &intent,
                None,
                InferenceRoutingMode::Legacy,
            )
            .expect("legacy Kimi K3 route ignores Router v2 health");
        let v2 = router
            .select_completion_route_for_mode(
                &proxy_router,
                &intent,
                None,
                InferenceRoutingMode::V2,
            )
            .expect_err("Router v2 refuses to substitute another public model");

        assert_eq!(legacy.provider, ProviderId::Tinfoil);
        assert_eq!(legacy.public_model_id, KIMI_K3_MODEL_ID);
        assert!(matches!(
            v2,
            ProviderRoutingError::CapacityUnavailable { model, .. }
                if model == KIMI_K3_MODEL_ID
        ));
    }

    #[test]
    fn test_golden_completion_route_matrix_by_selector_plan_and_provider_preference() {
        #[derive(Clone, Copy)]
        struct Case {
            name: &'static str,
            selector: &'static str,
            plan: ModelPlan,
            provider_preference: Option<ProviderPreference>,
            continuum_available: bool,
            expected_access: bool,
            expected_public_model: &'static str,
            expected_provider: &'static str,
            expected_provider_model: &'static str,
            expected_source: RouteSelectionSource,
        }

        let cases = [
            Case {
                name: "free auto quick",
                selector: AUTO_QUICK_MODEL_ID,
                plan: ModelPlan::Free,
                provider_preference: None,
                continuum_available: true,
                expected_access: true,
                expected_public_model: QUICK_MODEL_ID,
                expected_provider: "tinfoil",
                expected_provider_model: QUICK_MODEL_ID,
                expected_source: RouteSelectionSource::StaticSplit,
            },
            Case {
                name: "free auto powerful remains unavailable",
                selector: AUTO_POWERFUL_MODEL_ID,
                plan: ModelPlan::Free,
                provider_preference: None,
                continuum_available: true,
                expected_access: false,
                expected_public_model: GLM_5_3_MODEL_ID,
                expected_provider: "continuum",
                expected_provider_model: "glm-5.3",
                expected_source: RouteSelectionSource::DefaultProvider,
            },
            Case {
                name: "paid auto quick",
                selector: AUTO_QUICK_MODEL_ID,
                plan: ModelPlan::Paid,
                provider_preference: None,
                continuum_available: true,
                expected_access: true,
                expected_public_model: DEEPSEEK_V4_1_FLASH_MODEL_ID,
                expected_provider: "tinfoil",
                expected_provider_model: DEEPSEEK_V4_1_FLASH_MODEL_ID,
                expected_source: RouteSelectionSource::StaticSplit,
            },
            Case {
                name: "paid auto powerful uses GLM default",
                selector: AUTO_POWERFUL_MODEL_ID,
                plan: ModelPlan::Paid,
                provider_preference: None,
                continuum_available: true,
                expected_access: true,
                expected_public_model: GLM_5_3_MODEL_ID,
                expected_provider: "continuum",
                expected_provider_model: "glm-5.3",
                expected_source: RouteSelectionSource::DefaultProvider,
            },
            Case {
                name: "explicit K3 is independent of the auto target",
                selector: KIMI_K3_MODEL_ID,
                plan: ModelPlan::Paid,
                provider_preference: None,
                continuum_available: true,
                expected_access: true,
                expected_public_model: KIMI_K3_MODEL_ID,
                expected_provider: "tinfoil",
                expected_provider_model: KIMI_K3_MODEL_ID,
                expected_source: RouteSelectionSource::StaticSplit,
            },
            Case {
                name: "explicit GLM 5.3 Tinfoil preference",
                selector: GLM_5_3_MODEL_ID,
                plan: ModelPlan::Paid,
                provider_preference: Some(ProviderPreference::feature_flag(ProviderId::Tinfoil)),
                continuum_available: true,
                expected_access: true,
                expected_public_model: GLM_5_3_MODEL_ID,
                expected_provider: "tinfoil",
                expected_provider_model: GLM_5_3_MODEL_ID,
                expected_source: RouteSelectionSource::FeatureFlag,
            },
            Case {
                name: "explicit GLM 5.3 Continuum preference",
                selector: GLM_5_3_MODEL_ID,
                plan: ModelPlan::Paid,
                provider_preference: Some(ProviderPreference::feature_flag(ProviderId::Continuum)),
                continuum_available: true,
                expected_access: true,
                expected_public_model: GLM_5_3_MODEL_ID,
                expected_provider: "continuum",
                expected_provider_model: "glm-5.3",
                expected_source: RouteSelectionSource::FeatureFlag,
            },
            Case {
                name: "explicit GLM 5.3 Tinfoil preference without a Continuum proxy",
                selector: GLM_5_3_MODEL_ID,
                plan: ModelPlan::Paid,
                provider_preference: Some(ProviderPreference::feature_flag(ProviderId::Tinfoil)),
                continuum_available: false,
                expected_access: true,
                expected_public_model: GLM_5_3_MODEL_ID,
                expected_provider: "tinfoil",
                expected_provider_model: GLM_5_3_MODEL_ID,
                expected_source: RouteSelectionSource::FeatureFlag,
            },
        ];

        let router = ProviderRouter::default();
        for case in cases {
            let alias_targets = ModelAliasTargets::for_plan(case.plan);
            let resolved_model = alias_targets.resolve(case.selector);
            let access = case.plan.allows_model(resolved_model);

            assert_eq!(access, case.expected_access, "{}", case.name);
            if !case.expected_access {
                continue;
            }

            let proxy_router = if case.continuum_available {
                proxy_router_with_both_providers()
            } else {
                ProxyRouter::new(
                    "https://api.openai.com".to_string(),
                    None,
                    "http://tinfoil.example.com".to_string(),
                )
            };
            let selected = router
                .select_completion_route_with_preference(
                    &proxy_router,
                    uuid_for_bucket(73),
                    resolved_model,
                    case.provider_preference,
                )
                .unwrap_or_else(|error| panic!("{}: {error:?}", case.name));

            assert_eq!(
                selected.public_model_id, case.expected_public_model,
                "{}",
                case.name
            );
            assert_eq!(
                selected.provider_model_id, case.expected_provider_model,
                "{}",
                case.name
            );
            assert_eq!(
                selected.response_model_id, case.expected_public_model,
                "{}",
                case.name
            );
            assert_eq!(
                selected.proxy.provider_name, case.expected_provider,
                "{}",
                case.name
            );
            assert_eq!(
                selected.selection_source, case.expected_source,
                "{}",
                case.name
            );
            assert_eq!(selected.bucket, None, "{}", case.name);
        }
    }

    #[test]
    fn shadow_planner_matches_healthy_router_v2_for_every_model_and_provider_configuration() {
        let router = ProviderRouter::default();
        let both = proxy_router_with_both_providers();
        let tinfoil_only = ProxyRouter::new(
            "https://api.openai.com".to_string(),
            None,
            "http://tinfoil.example.com".to_string(),
        );

        for proxy_router in [&both, &tinfoil_only] {
            for model in PROVIDER_REGISTRY.completion_models() {
                for bucket in [0, 29, 30, 69, 70, 99] {
                    let mut intent = intent(model.public_model_id, model.public_model_id);
                    intent.account_uuid = uuid_for_bucket(bucket);
                    let active = router.select_active_completion_route(proxy_router, &intent);
                    let shadow = router.shadow_completion_plan(proxy_router, &intent);
                    assert!(
                        matches!(
                            compare_shadow_route(&active, &shadow),
                            ShadowRouteComparison::Match { .. }
                        ),
                        "model={}, bucket={bucket}, active={active:?}, shadow={shadow:?}",
                        model.public_model_id
                    );
                }
            }
        }
    }

    #[test]
    fn shadow_planner_matches_legacy_error_classes_for_unknown_models() {
        let router = ProviderRouter::default();
        let proxy_router = proxy_router_with_both_providers();

        for model in [
            "unknown-model",
            "kimi-k-3",
            "kimi-k3-latest",
            "deepseek-v4-flash",
            "deepseek-v4-flash-0731",
            "deepseek-v4.1-flash",
            "deepseek-v4-1-flash-latest",
        ] {
            let account_uuid = uuid_for_bucket(50);
            let intent = InferenceIntent::new(
                account_uuid,
                model,
                model,
                ModelPlan::Paid,
                InferenceSurface::ChatCompletions,
                WorkloadClass::Interactive,
            );
            let active = router.select_completion_route_with_preference(
                &proxy_router,
                account_uuid,
                model,
                None,
            );
            let shadow = router.shadow_completion_plan(&proxy_router, &intent);

            assert!(matches!(
                compare_shadow_route(&active, &shadow),
                ShadowRouteComparison::Match {
                    outcome: CredentialFreeRouteOutcome::UnsupportedModel,
                    ..
                }
            ));
        }
    }

    #[test]
    fn legacy_selector_and_baseline_planner_remain_health_independent() {
        let router = ProviderRouter::default();
        let proxy_router = proxy_router_with_both_providers();
        let account_uuid = uuid_for_bucket(73);
        let provider_preference = Some(ProviderPreference::feature_flag(ProviderId::Tinfoil));
        let intent = InferenceIntent::new(
            account_uuid,
            GLM_5_3_MODEL_ID,
            GLM_5_3_MODEL_ID,
            ModelPlan::Paid,
            InferenceSurface::Responses,
            WorkloadClass::Interactive,
        );

        let active_before = router
            .select_completion_route_with_preference(
                &proxy_router,
                account_uuid,
                GLM_5_3_MODEL_ID,
                provider_preference,
            )
            .expect("active route before shadow health");
        let shadow_before = router
            .shadow_completion_plan(&proxy_router, &intent)
            .expect("shadow route before shadow health");

        let mut failure = AttemptFailure::new(
            AttemptFailureKind::CapacityRejected,
            AttemptStage::AwaitingResponse,
            ReplaySafety::ProvenPreAcceptance,
        );
        failure.status = Some(429);
        failure.retry_after = Some(Duration::from_secs(60));
        let terminal = AttemptTerminal::Failed {
            attempt: intent
                .begin_execution()
                .begin_attempt(active_before.identity()),
            failure,
        };
        let report = router.observe_attempt_terminal(&terminal, ShadowObservationMode::Update);
        assert!(matches!(
            report.snapshot.expect("known route").effective,
            ShadowDisposition::WouldOpen { .. }
        ));

        let active_after = router
            .select_completion_route_with_preference(
                &proxy_router,
                account_uuid,
                GLM_5_3_MODEL_ID,
                provider_preference,
            )
            .expect("active route after shadow health");
        let shadow_after = router
            .shadow_completion_plan(&proxy_router, &intent)
            .expect("shadow route after shadow health");

        assert_eq!(active_after.identity(), active_before.identity());
        assert_eq!(shadow_after, shadow_before);
        assert_eq!(
            active_after.public_model_id,
            shadow_after.selected.public_model_id
        );
        assert_eq!(active_after.provider, shadow_after.selected.provider);
    }

    #[test]
    fn active_glm_health_fallback_is_symmetric_across_weighted_providers() {
        let proxy_router = proxy_router_with_both_providers();
        for (bucket, open_provider, open_model, alternate, alternate_model) in [
            (
                0,
                ProviderId::Continuum,
                "glm-5.3",
                ProviderId::Tinfoil,
                GLM_5_3_MODEL_ID,
            ),
            (
                73,
                ProviderId::Tinfoil,
                GLM_5_3_MODEL_ID,
                ProviderId::Continuum,
                "glm-5.3",
            ),
        ] {
            let router = ProviderRouter::default();
            let mut intent = intent(GLM_5_3_MODEL_ID, GLM_5_3_MODEL_ID);
            intent.account_uuid = uuid_for_bucket(bucket);
            assert_eq!(
                router
                    .select_active_completion_route(&proxy_router, &intent)
                    .expect("healthy route")
                    .provider,
                open_provider
            );
            router.observe_attempt_terminal(
                &capacity_terminal(
                    open_provider,
                    GLM_5_3_MODEL_ID,
                    open_model,
                    429,
                    Duration::from_secs(60),
                ),
                ShadowObservationMode::Update,
            );
            let selected = router
                .select_active_completion_route(&proxy_router, &intent)
                .expect("same-model alternate");
            assert_eq!(selected.provider, alternate);
            assert_eq!(selected.provider_model_id, alternate_model);
            assert_eq!(selected.public_model_id, GLM_5_3_MODEL_ID);
            assert_eq!(selected.response_model_id, GLM_5_3_MODEL_ID);
            assert_eq!(selected.selection_source, RouteSelectionSource::Fallback);
        }
    }

    #[test]
    fn active_glm_canary_switches_only_new_requests_to_same_model_alternate() {
        let router = ProviderRouter::default();
        let proxy_router = proxy_router_with_both_providers();
        let intent = intent(GLM_5_3_MODEL_ID, GLM_5_3_MODEL_ID);

        let pinned_before_failure = router
            .select_active_completion_route(&proxy_router, &intent)
            .expect("initial Continuum route");
        assert_eq!(pinned_before_failure.provider, ProviderId::Continuum);

        router.observe_attempt_terminal(
            &capacity_terminal(
                ProviderId::Continuum,
                GLM_5_3_MODEL_ID,
                "glm-5.3",
                429,
                Duration::from_secs(60),
            ),
            ShadowObservationMode::Update,
        );

        // The previously returned pin is immutable; only a fresh preparation
        // observes the newly opened circuit.
        assert_eq!(pinned_before_failure.provider, ProviderId::Continuum);
        assert_eq!(pinned_before_failure.provider_model_id, "glm-5.3");

        let selected_after_failure = router
            .select_active_completion_route(&proxy_router, &intent)
            .expect("Tinfoil GLM fallback");
        assert_eq!(selected_after_failure.provider, ProviderId::Tinfoil);
        assert_eq!(selected_after_failure.provider_model_id, GLM_5_3_MODEL_ID);
        assert_eq!(selected_after_failure.public_model_id, GLM_5_3_MODEL_ID);
        assert_eq!(
            selected_after_failure.selection_source,
            RouteSelectionSource::Fallback
        );
    }

    #[test]
    fn active_glm_canary_bypasses_an_open_provider() {
        let router = ProviderRouter::default();
        let proxy_router = proxy_router_with_both_providers();
        let intent = intent(GLM_5_3_MODEL_ID, GLM_5_3_MODEL_ID);

        router.observe_attempt_terminal(
            &capacity_terminal(
                ProviderId::Tinfoil,
                GLM_5_3_MODEL_ID,
                GLM_5_3_MODEL_ID,
                503,
                Duration::from_secs(60),
            ),
            ShadowObservationMode::Update,
        );

        let selected = router
            .select_active_completion_route(&proxy_router, &intent)
            .expect("Continuum GLM fallback");
        assert_eq!(selected.provider, ProviderId::Continuum);
        assert_eq!(selected.provider_model_id, "glm-5.3");
        assert_eq!(selected.selection_source, RouteSelectionSource::Fallback);
    }

    #[test]
    fn active_glm_canary_returns_typed_capacity_when_every_configured_route_is_open() {
        let router = ProviderRouter::default();
        let proxy_router = proxy_router_with_both_providers();
        let intent = intent(GLM_5_3_MODEL_ID, GLM_5_3_MODEL_ID);

        for terminal in [
            capacity_terminal(
                ProviderId::Tinfoil,
                GLM_5_3_MODEL_ID,
                GLM_5_3_MODEL_ID,
                429,
                Duration::from_secs(40),
            ),
            capacity_terminal(
                ProviderId::Continuum,
                GLM_5_3_MODEL_ID,
                "glm-5.3",
                529,
                Duration::from_secs(10),
            ),
        ] {
            router.observe_attempt_terminal(&terminal, ShadowObservationMode::Update);
        }

        let error = router
            .select_active_completion_route(&proxy_router, &intent)
            .expect_err("both GLM routes are open");
        match error {
            ProviderRoutingError::CapacityUnavailable { model, retry_after } => {
                assert_eq!(model, GLM_5_3_MODEL_ID);
                assert_eq!(retry_after, Duration::from_secs(30));
            }
            other => panic!("unexpected route error: {other:?}"),
        }
    }

    #[test]
    fn active_glm_canary_uses_route_failure_threshold_but_not_watch_state() {
        let router = ProviderRouter::default();
        let proxy_router = proxy_router_with_both_providers();
        let intent = intent(GLM_5_3_MODEL_ID, GLM_5_3_MODEL_ID);
        let failure = || route_failure_terminal(ProviderId::Continuum, GLM_5_3_MODEL_ID, "glm-5.3");

        for observed_failures in 1..=3 {
            router.observe_attempt_terminal(&failure(), ShadowObservationMode::Update);
            let selected = router
                .select_active_completion_route(&proxy_router, &intent)
                .expect("GLM route");
            let expected = if observed_failures < 3 {
                ProviderId::Continuum
            } else {
                ProviderId::Tinfoil
            };
            assert_eq!(selected.provider, expected, "failure {observed_failures}");
        }
    }

    #[test]
    fn continuum_account_429_blocks_continuum_glm_flash_and_keeps_glm_on_tinfoil() {
        let router = ProviderRouter::default();
        let proxy_router = proxy_router_with_both_providers();
        router.observe_attempt_terminal(
            &capacity_terminal(
                ProviderId::Continuum,
                GLM_5_3_FLASH_MODEL_ID,
                "glm-5.3-flash",
                429,
                Duration::from_secs(60),
            ),
            ShadowObservationMode::Update,
        );

        let glm = router
            .select_active_completion_route(
                &proxy_router,
                &intent(GLM_5_3_MODEL_ID, GLM_5_3_MODEL_ID),
            )
            .expect("Tinfoil GLM after Continuum account limit");
        assert_eq!(glm.provider, ProviderId::Tinfoil);

        let flash = router
            .select_active_completion_route(
                &proxy_router,
                &intent(GLM_5_3_FLASH_MODEL_ID, GLM_5_3_FLASH_MODEL_ID),
            )
            .expect("Flash still has a healthy Tinfoil route");
        assert_eq!(flash.provider, ProviderId::Tinfoil);
        assert_eq!(flash.public_model_id, GLM_5_3_FLASH_MODEL_ID);
    }

    #[test]
    fn flash_tinfoil_429_fails_over_to_continuum() {
        let router = ProviderRouter::default();
        let proxy_router = proxy_router_with_both_providers();
        router.observe_attempt_terminal(
            &capacity_terminal(
                ProviderId::Tinfoil,
                GLM_5_3_FLASH_MODEL_ID,
                GLM_5_3_FLASH_MODEL_ID,
                429,
                Duration::from_secs(60),
            ),
            ShadowObservationMode::Update,
        );

        let selected = router
            .select_active_completion_route(
                &proxy_router,
                &InferenceIntent::new(
                    uuid_for_bucket(50),
                    GLM_5_3_FLASH_MODEL_ID,
                    GLM_5_3_FLASH_MODEL_ID,
                    ModelPlan::Paid,
                    InferenceSurface::Responses,
                    WorkloadClass::Interactive,
                ),
            )
            .expect("Flash capacity failover");
        assert_eq!(selected.provider, ProviderId::Continuum);
        assert_eq!(selected.provider_model_id, "glm-5.3-flash");
        assert_eq!(selected.selection_source, RouteSelectionSource::Fallback);
    }

    #[test]
    fn flash_tinfoil_timeouts_do_not_fail_over_to_continuum() {
        let router = ProviderRouter::default();
        let proxy_router = proxy_router_with_both_providers();
        let failure = || {
            route_failure_terminal(
                ProviderId::Tinfoil,
                GLM_5_3_FLASH_MODEL_ID,
                GLM_5_3_FLASH_MODEL_ID,
            )
        };
        for _ in 0..3 {
            router.observe_attempt_terminal(&failure(), ShadowObservationMode::Update);
        }

        let selected = router
            .select_active_completion_route(
                &proxy_router,
                &InferenceIntent::new(
                    uuid_for_bucket(50),
                    GLM_5_3_FLASH_MODEL_ID,
                    GLM_5_3_FLASH_MODEL_ID,
                    ModelPlan::Paid,
                    InferenceSurface::Responses,
                    WorkloadClass::Interactive,
                ),
            )
            .expect("Flash stays on Tinfoil after transport failures");
        assert_eq!(selected.provider, ProviderId::Tinfoil);
        assert_eq!(selected.provider_model_id, GLM_5_3_FLASH_MODEL_ID);
    }

    #[test]
    fn active_health_filter_applies_to_auto_without_crossing_public_models() {
        let router = ProviderRouter::default();
        let proxy_router = proxy_router_with_both_providers();
        router.observe_attempt_terminal(
            &capacity_terminal(
                ProviderId::Continuum,
                GLM_5_3_MODEL_ID,
                "glm-5.3",
                429,
                Duration::from_secs(60),
            ),
            ShadowObservationMode::Update,
        );

        let synthetic_auto_glm = intent(AUTO_POWERFUL_MODEL_ID, GLM_5_3_MODEL_ID);
        let auto_route = router
            .select_active_completion_route(&proxy_router, &synthetic_auto_glm)
            .expect("Auto Powerful uses the healthy GLM provider");
        assert_eq!(auto_route.provider, ProviderId::Tinfoil);
        assert_eq!(auto_route.public_model_id, GLM_5_3_MODEL_ID);
        assert_eq!(auto_route.provider_model_id, GLM_5_3_MODEL_ID);
        assert_eq!(auto_route.response_model_id, GLM_5_3_MODEL_ID);
        assert_eq!(auto_route.selection_source, RouteSelectionSource::Fallback);
    }

    #[test]
    fn active_health_filter_never_substitutes_for_single_route_explicit_models() {
        let proxy_router = proxy_router_with_both_providers();

        for (provider, public_model, provider_model) in [
            (ProviderId::Tinfoil, KIMI_K3_MODEL_ID, KIMI_K3_MODEL_ID),
            (ProviderId::Tinfoil, QUICK_MODEL_ID, QUICK_MODEL_ID),
        ] {
            let router = ProviderRouter::default();
            router.observe_attempt_terminal(
                &capacity_terminal(
                    provider,
                    public_model,
                    provider_model,
                    429,
                    Duration::from_secs(60),
                ),
                ShadowObservationMode::Update,
            );

            let error = router
                .select_active_completion_route(&proxy_router, &intent(public_model, public_model))
                .expect_err("an explicit model cannot fall through to another public model");
            assert!(matches!(
                error,
                ProviderRoutingError::CapacityUnavailable { model, .. }
                    if model == public_model
            ));
        }
    }

    #[test]
    fn open_or_probe_in_flight_blocks_a_new_request() {
        assert!(route_is_available_for_new_request(
            ShadowDisposition::Healthy
        ));
        assert!(route_is_available_for_new_request(
            ShadowDisposition::Watch {
                consecutive_failures: 2
            }
        ));
        assert!(route_is_available_for_new_request(
            ShadowDisposition::WouldProbe
        ));
        assert!(!route_is_available_for_new_request(
            ShadowDisposition::WouldOpen {
                remaining: Duration::from_secs(1)
            }
        ));
        assert!(!route_is_available_for_new_request(
            ShadowDisposition::ProbeInFlight {
                retry_after: Duration::from_secs(1)
            }
        ));
    }

    #[test]
    fn retry_after_rounds_up_and_never_returns_zero() {
        assert_eq!(ceil_retry_after(Duration::ZERO), Duration::from_secs(1));
        assert_eq!(
            ceil_retry_after(Duration::from_nanos(1)),
            Duration::from_secs(1)
        );
        assert_eq!(
            ceil_retry_after(Duration::from_millis(1_001)),
            Duration::from_secs(2)
        );
    }

    #[test]
    fn test_single_provider_models_have_no_provider_routing_flag() {
        let router = ProviderRouter::default();

        assert_eq!(
            router.provider_routing_flag_for_completion_model("gpt-oss-120b"),
            None
        );
        assert_eq!(
            router.provider_routing_flag_for_completion_model("deepseek-v4-1-flash"),
            None
        );
    }

    #[test]
    fn glm_flash_v1_flag_is_opt_in_and_v2_ignores_it() {
        let router = ProviderRouter::default();
        let proxies = proxy_router_with_both_providers();
        let flag = router
            .provider_routing_flag_for_completion_model(GLM_5_3_FLASH_MODEL_ID)
            .expect("Flash legacy provider flag");
        assert_eq!(flag.key(), GLM_5_3_FLASH_CONTINUUM_FLAG_KEY);

        // Missing/unavailable/timed-out lookups all yield None at the shared
        // flag call site. False must preserve Tinfoil just like None.
        for enabled in [None, Some(false), Some(true)] {
            let preference = enabled.map(|value| flag.preference_for(value));
            let expected_legacy = if enabled == Some(true) {
                ProviderId::Continuum
            } else {
                ProviderId::Tinfoil
            };
            for bucket in 0..100 {
                let mut request = intent(GLM_5_3_FLASH_MODEL_ID, GLM_5_3_FLASH_MODEL_ID);
                request.account_uuid = uuid_for_bucket(bucket);
                for (mode, expected) in [
                    (InferenceRoutingMode::Legacy, expected_legacy),
                    (
                        InferenceRoutingMode::V2,
                        if bucket < 30 {
                            ProviderId::Continuum
                        } else {
                            ProviderId::Tinfoil
                        },
                    ),
                ] {
                    let selected = router
                        .select_completion_route_for_mode(&proxies, &request, preference, mode)
                        .expect("Flash route");
                    assert_eq!(selected.provider, expected);
                    assert_eq!(selected.public_model_id, GLM_5_3_FLASH_MODEL_ID);
                    assert_eq!(selected.response_model_id, GLM_5_3_FLASH_MODEL_ID);
                    assert_eq!(
                        selected.provider_model_id,
                        match expected {
                            ProviderId::Continuum => "glm-5.3-flash",
                            ProviderId::Tinfoil => GLM_5_3_FLASH_MODEL_ID,
                        }
                    );
                    assert_eq!(
                        selected.bucket,
                        match mode {
                            InferenceRoutingMode::Legacy => None,
                            InferenceRoutingMode::V2 => Some(bucket),
                        }
                    );
                }
            }
        }
    }

    #[test]
    fn glm_flash_v1_enabled_flag_falls_back_when_continuum_is_not_configured() {
        let router = ProviderRouter::default();
        let proxies = ProxyRouter::new(
            "https://api.openai.com".to_string(),
            Some("synthetic-openai-key".to_string()),
            "http://tinfoil.example.com".to_string(),
        );
        let flag = router
            .provider_routing_flag_for_completion_model(GLM_5_3_FLASH_MODEL_ID)
            .unwrap();
        let selected = router
            .select_completion_route_with_preference(
                &proxies,
                uuid_for_bucket(0),
                GLM_5_3_FLASH_MODEL_ID,
                Some(flag.preference_for(true)),
            )
            .expect("configured Tinfoil fallback");
        assert_eq!(selected.provider, ProviderId::Tinfoil);
        assert_eq!(selected.selection_source, RouteSelectionSource::Fallback);
        assert_eq!(selected.proxy.api_key, None);
    }

    #[test]
    fn test_glm_5_3_tinfoil_flag_maps_true_and_false_to_separate_providers() {
        let router = ProviderRouter::default();
        let flag = router
            .provider_routing_flag_for_completion_model(GLM_5_3_MODEL_ID)
            .expect("GLM 5.3 provider flag");

        assert_eq!(flag.key(), GLM_5_3_TINFOIL_FLAG_KEY);
        assert_eq!(flag.preference_for(true).provider(), ProviderId::Tinfoil);
        assert_eq!(flag.preference_for(false).provider(), ProviderId::Continuum);
        assert_eq!(
            flag.preference_for(true).source(),
            RouteSelectionSource::FeatureFlag
        );
    }

    #[test]
    fn test_glm_5_3_always_uses_continuum_and_canonicalizes_the_response() {
        let router = ProviderRouter::default();
        let proxy_router = proxy_router_with_both_providers();

        let selected = router
            .select_completion_route(&proxy_router, uuid_for_bucket(50), GLM_5_3_MODEL_ID)
            .expect("route");

        assert_eq!(selected.proxy.provider_name, "continuum");
        assert_eq!(selected.public_model_id, GLM_5_3_MODEL_ID);
        assert_eq!(selected.provider_model_id, "glm-5.3");
        assert_eq!(selected.response_model_id, GLM_5_3_MODEL_ID);
        assert_eq!(selected.bucket, None);
        assert_eq!(
            selected.selection_source,
            RouteSelectionSource::DefaultProvider
        );
    }

    #[test]
    fn test_glm_5_3_tinfoil_route_requires_explicit_enabled_preference() {
        let router = ProviderRouter::default();
        let proxy_router = proxy_router_with_both_providers();
        let flag = router
            .provider_routing_flag_for_completion_model(GLM_5_3_MODEL_ID)
            .expect("GLM 5.3 provider flag");

        let selected = router
            .select_completion_route_with_preference(
                &proxy_router,
                uuid_for_bucket(50),
                GLM_5_3_MODEL_ID,
                Some(flag.preference_for(true)),
            )
            .expect("Tinfoil route");

        assert_eq!(selected.proxy.provider_name, "tinfoil");
        assert_eq!(selected.public_model_id, GLM_5_3_MODEL_ID);
        assert_eq!(selected.provider_model_id, GLM_5_3_MODEL_ID);
        assert_eq!(selected.response_model_id, GLM_5_3_MODEL_ID);
        assert_eq!(selected.bucket, None);
        assert_eq!(selected.selection_source, RouteSelectionSource::FeatureFlag);

        let selected = router
            .select_completion_route_with_preference(
                &proxy_router,
                uuid_for_bucket(50),
                GLM_5_3_MODEL_ID,
                Some(flag.preference_for(false)),
            )
            .expect("PrivateMode route");

        assert_eq!(selected.proxy.provider_name, "continuum");
        assert_eq!(selected.provider_model_id, "glm-5.3");
        assert_eq!(selected.selection_source, RouteSelectionSource::FeatureFlag);
    }

    #[test]
    fn test_auto_powerful_uses_glm_route_table() {
        let router = ProviderRouter::default();
        let proxy_router = proxy_router_with_both_providers();

        let selected = router
            .select_completion_route(
                &proxy_router,
                uuid_for_bucket(70),
                crate::model_config::AUTO_POWERFUL_MODEL_ID,
            )
            .expect("route");

        assert_eq!(selected.proxy.provider_name, "continuum");
        assert_eq!(selected.public_model_id, GLM_5_3_MODEL_ID);
        assert_eq!(selected.provider_model_id, "glm-5.3");
        assert_eq!(selected.response_model_id, GLM_5_3_MODEL_ID);
        assert_eq!(selected.bucket, None);
        assert_eq!(
            selected.selection_source,
            RouteSelectionSource::DefaultProvider
        );
    }

    #[test]
    fn test_paid_powerful_glm_5_3_override_uses_continuum_route() {
        let router = ProviderRouter::default();
        let proxy_router = proxy_router_with_both_providers();
        let flags = HashMap::from([(PAID_POWERFUL_GLM_5_3_ALIAS_FLAG_KEY.to_string(), true)]);
        let targets = ModelAliasTargets::for_plan_with_overrides(
            ModelPlan::Paid,
            PaidModelAliasOverrides::from_flag_values(&flags),
        );

        let selected = router
            .select_completion_route(
                &proxy_router,
                uuid_for_bucket(70),
                targets.resolve(crate::model_config::AUTO_POWERFUL_MODEL_ID),
            )
            .expect("route");

        assert_eq!(selected.proxy.provider_name, "continuum");
        assert_eq!(selected.public_model_id, GLM_5_3_MODEL_ID);
        assert_eq!(selected.provider_model_id, "glm-5.3");
        assert_eq!(selected.response_model_id, GLM_5_3_MODEL_ID);
        assert_eq!(selected.bucket, None);
        assert_eq!(
            selected.selection_source,
            RouteSelectionSource::DefaultProvider
        );
    }

    #[test]
    fn test_removed_kimi_k2_6_is_unsupported() {
        let router = ProviderRouter::default();
        let proxy_router = proxy_router_with_both_providers();

        let error = router
            .select_completion_route(&proxy_router, uuid_for_bucket(1), "kimi-k2-6")
            .expect_err("deprecated Kimi K2.6 is no longer a public model");

        assert_eq!(
            error,
            ProviderRoutingError::UnsupportedModel("kimi-k2-6".to_string())
        );
    }

    #[test]
    fn test_router_v2_glm_flash_uses_weighted_continuum_and_tinfoil_routes() {
        let router = ProviderRouter::default();
        let proxy_router = proxy_router_with_both_providers();

        let selected = router
            .select_active_completion_route(
                &proxy_router,
                &intent(GLM_5_3_FLASH_MODEL_ID, GLM_5_3_FLASH_MODEL_ID),
            )
            .expect("Flash v2 route");
        assert_eq!(selected.provider, ProviderId::Continuum);
        assert_eq!(selected.public_model_id, GLM_5_3_FLASH_MODEL_ID);
        assert_eq!(selected.provider_model_id, "glm-5.3-flash");
        assert_eq!(selected.response_model_id, GLM_5_3_FLASH_MODEL_ID);
    }

    #[test]
    fn test_non_configured_model_preserves_existing_tinfoil_completion_route() {
        let router = ProviderRouter::default();
        let proxy_router = proxy_router_with_both_providers();

        let selected = router
            .select_completion_route(&proxy_router, uuid_for_bucket(50), "gpt-oss-120b")
            .expect("route");

        assert_eq!(selected.proxy.provider_name, "tinfoil");
        assert_eq!(selected.public_model_id, "gpt-oss-120b");
        assert_eq!(selected.provider_model_id, "gpt-oss-120b");
        assert_eq!(selected.response_model_id, "gpt-oss-120b");
        assert_eq!(selected.bucket, None);
    }

    #[test]
    fn test_new_tinfoil_models_preserve_canonical_ids() {
        let router = ProviderRouter::default();
        let proxy_router = proxy_router_with_both_providers();

        for model_id in ["kimi-k3", "deepseek-v4-1-flash", "glm-5-3-flash"] {
            let selected = router
                .select_completion_route(&proxy_router, uuid_for_bucket(50), model_id)
                .expect("canonical Tinfoil model should route");

            assert_eq!(selected.proxy.provider_name, "tinfoil");
            assert_eq!(selected.public_model_id, model_id);
            assert_eq!(selected.provider_model_id, model_id);
            assert_eq!(selected.response_model_id, model_id);
            assert_eq!(selected.bucket, None);
        }
    }

    #[test]
    fn test_new_tinfoil_models_reject_near_spellings() {
        let router = ProviderRouter::default();
        let proxy_router = proxy_router_with_both_providers();

        for model_id in [
            "kimi-k-3",
            "kimi-k3-latest",
            "deepseek-v4-flash",
            "deepseek-v4-flash-0731",
            "deepseek-v4flash",
            "deepseek-v4.1-flash",
            "deepseek-v4-1-flash-latest",
            "deepseek-v41-flash",
            "glm-5.3-flash",
            "glm-5-3-flash-latest",
        ] {
            let error = router
                .select_completion_route(&proxy_router, uuid_for_bucket(50), model_id)
                .expect_err("non-canonical model spelling should be rejected");

            assert_eq!(
                error,
                ProviderRoutingError::UnsupportedModel(model_id.to_string())
            );
        }
    }

    #[test]
    fn test_tinfoil_fallback_resolves_known_alias_before_provider_request() {
        let router = ProviderRouter::default();
        let proxy_router = ProxyRouter::new(
            "http://continuum.example.com".to_string(),
            None,
            "http://tinfoil.example.com".to_string(),
        );

        let selected = router
            .select_completion_route(
                &proxy_router,
                uuid_for_bucket(50),
                crate::model_config::AUTO_QUICK_MODEL_ID,
            )
            .expect("route");

        assert_eq!(selected.proxy.provider_name, "tinfoil");
        assert_eq!(
            selected.public_model_id,
            crate::model_config::QUICK_MODEL_ID
        );
        assert_eq!(
            selected.provider_model_id,
            crate::model_config::QUICK_MODEL_ID
        );
        assert_eq!(
            selected.response_model_id,
            crate::model_config::QUICK_MODEL_ID
        );
        assert_eq!(selected.bucket, None);
    }

    #[test]
    fn test_tinfoil_fallback_rejects_unknown_model_passthrough() {
        let router = ProviderRouter::default();
        let proxy_router = ProxyRouter::new(
            "http://continuum.example.com".to_string(),
            None,
            "http://tinfoil.example.com".to_string(),
        );

        let error = router
            .select_completion_route(&proxy_router, uuid_for_bucket(50), "provider-native-model")
            .expect_err("unsupported model");

        assert_eq!(
            error,
            ProviderRoutingError::UnsupportedModel("provider-native-model".to_string())
        );
    }

    #[test]
    fn test_tinfoil_fallback_rejects_unknown_models() {
        let router = ProviderRouter::default();
        let proxy_router = proxy_router_with_both_providers();

        let error = router
            .select_completion_route(&proxy_router, uuid_for_bucket(50), "unknown-model")
            .expect_err("unsupported model");

        assert_eq!(
            error,
            ProviderRoutingError::UnsupportedModel("unknown-model".to_string())
        );
    }

    #[test]
    fn test_configured_model_errors_when_continuum_is_missing() {
        let router = ProviderRouter::default();
        let proxy_router = ProxyRouter::new(
            "https://api.openai.com".to_string(),
            None,
            "http://tinfoil.example.com".to_string(),
        );

        let error = router
            .select_completion_route(&proxy_router, uuid_for_bucket(50), GLM_5_3_MODEL_ID)
            .expect_err("no eligible GLM 5.3 route");

        assert_eq!(
            error,
            ProviderRoutingError::NoEligibleRoute(GLM_5_3_MODEL_ID.to_string())
        );
    }
}
