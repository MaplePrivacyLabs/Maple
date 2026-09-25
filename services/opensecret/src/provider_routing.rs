use crate::inference::auto_model::ModelAvailability;
use crate::inference::health::{
    ProbeClaimResult, ProbeLease, ShadowDisposition, ShadowHealthState, ShadowObservationMode,
    ShadowObservationReport, ShadowRouteSnapshot, MIN_CAPACITY_COOLDOWN,
};
use crate::inference::sticky_routes::{StickyRoute, StickyRouteMemory};
use crate::inference::{
    AttemptTerminal, InferenceIntent, InferenceSurface, RouteIdentity, RouteKey,
};
use crate::inference_planning::{
    plan_completion_route, ConfiguredProviders, RoutePlan, RoutePlanningError, RoutePlanningInput,
};
use crate::provider_registry::{
    CompletionModelSpec, ProviderId, ProviderRegistry, RouteSelectionSource, PROVIDER_REGISTRY,
};
use crate::proxy_config::{ProxyConfig, ProxyRouter};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use uuid::Uuid;

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
    registry: &'static ProviderRegistry,
    shadow_health: ShadowHealthState,
    /// Per-account route memory for accepted inference requests.
    sticky_routes: StickyRouteMemory,
}

impl Default for ProviderRouter {
    fn default() -> Self {
        Self {
            registry: &PROVIDER_REGISTRY,
            shadow_health: ShadowHealthState::new(&PROVIDER_REGISTRY),
            sticky_routes: StickyRouteMemory::default(),
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
                remembered_provider: self.remembered_provider_for(intent),
            },
        )
    }

    /// Returns the account's remembered Router v2 route for a surface and
    /// selector while the entry is live.
    pub(crate) fn sticky_route(
        &self,
        account_uuid: Uuid,
        surface: InferenceSurface,
        selector: &str,
    ) -> Option<StickyRoute> {
        self.sticky_routes.lookup(account_uuid, surface, selector)
    }

    /// Remembers the route a provider just accepted for a Router v2 logical
    /// request so the account keeps its warmed provider cache afterwards.
    pub(crate) fn remember_route(&self, intent: &InferenceIntent, route: &SelectedProviderRoute) {
        self.sticky_routes.record(
            intent.account_uuid,
            intent.surface,
            &intent.requested_model_id,
            StickyRoute {
                public_model_id: route.public_model_id.clone(),
                provider: route.provider,
                provider_model_id: route.provider_model_id.clone(),
            },
        );
    }

    /// The remembered provider for exactly this intent's public model, which the
    /// planner may prefer over the weighted bucket among eligible routes.
    fn remembered_provider_for(&self, intent: &InferenceIntent) -> Option<ProviderId> {
        self.sticky_routes
            .lookup(
                intent.account_uuid,
                intent.surface,
                &intent.requested_model_id,
            )
            .filter(|route| route.public_model_id == intent.public_model_id)
            .map(|route| route.provider)
    }

    #[cfg(test)]
    pub(crate) fn sticky_routes(&self) -> &StickyRouteMemory {
        &self.sticky_routes
    }

    /// Evaluates every candidate model's configured routes under one health
    /// snapshot, applying the shared route and capacity gates. This is the planning
    /// input for Auto model selection; it claims nothing and permits nothing.
    pub(crate) fn model_availability(
        &self,
        proxy_router: &ProxyRouter,
        public_model_ids: &[&str],
    ) -> HashMap<String, ModelAvailability> {
        let no_exclusions = HashSet::new();
        let candidates = public_model_ids
            .iter()
            .map(|public_model_id| {
                let configured = self
                    .registry
                    .completion_model(public_model_id)
                    .map(|model| self.configured_route_keys(proxy_router, model, &no_exclusions))
                    .filter(|routes| !routes.is_empty());
                (public_model_id.to_string(), configured)
            })
            .collect::<Vec<_>>();
        let route_groups = candidates
            .iter()
            .map(|(_, configured)| {
                configured
                    .as_ref()
                    .map(|routes| routes.iter().map(|(_, key)| key.clone()).collect())
                    .unwrap_or_default()
            })
            .collect::<Vec<Vec<RouteKey>>>();
        let snapshots = self.shadow_health.snapshot_route_groups(&route_groups);

        candidates
            .into_iter()
            .zip(snapshots)
            .map(|((public_model_id, configured), snapshots)| {
                let availability = match (configured, snapshots) {
                    (None, _) => ModelAvailability::NotConfigured,
                    (Some(_), None) => ModelAvailability::Unavailable {
                        retry_after: MIN_CAPACITY_COOLDOWN,
                    },
                    (Some(routes), Some(snapshots)) => {
                        match available_providers_for_model(&routes, &snapshots) {
                            Ok(providers) => ModelAvailability::Available(providers),
                            Err(retry_after) => ModelAvailability::Unavailable { retry_after },
                        }
                    }
                };
                (public_model_id, availability)
            })
            .collect()
    }

    fn configured_route_keys(
        &self,
        proxy_router: &ProxyRouter,
        model: &CompletionModelSpec,
        excluded_routes: &HashSet<RouteKey>,
    ) -> Vec<(ProviderId, RouteKey)> {
        model
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
            .collect()
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

        let configured_routes = self.configured_route_keys(proxy_router, model, excluded_routes);
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
        let available_providers = available_providers_for_model(&configured_routes, &snapshots)
            .map_err(|retry_after| ProviderRoutingError::CapacityUnavailable {
                model: intent.public_model_id.clone(),
                retry_after,
            })?;

        let plan = plan_completion_route(
            self.registry,
            RoutePlanningInput {
                intent,
                configured_providers: available_providers,
                remembered_provider: self.remembered_provider_for(intent),
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

/// Providers of one model whose routes may take a new request under the shared
/// route and capacity gates. `Err` carries the earliest bounded recovery
/// hint when every configured route is open or probing.
fn available_providers_for_model(
    configured_routes: &[(ProviderId, RouteKey)],
    snapshots: &[ShadowRouteSnapshot],
) -> Result<ConfiguredProviders, Duration> {
    debug_assert_eq!(configured_routes.len(), snapshots.len());
    let mut available_providers = ConfiguredProviders::none();
    let mut earliest_recovery = None;
    for ((provider, _), snapshot) in configured_routes.iter().zip(snapshots) {
        match snapshot.effective {
            ShadowDisposition::WouldOpen { remaining } => {
                let remaining = ceil_retry_after(remaining);
                earliest_recovery = Some(
                    earliest_recovery.map_or(remaining, |current: Duration| current.min(remaining)),
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
        return Err(earliest_recovery.unwrap_or(MIN_CAPACITY_COOLDOWN));
    }
    Ok(available_providers)
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
        AttemptFailure, AttemptFailureKind, AttemptStage, AttemptTerminal, ReplaySafety,
        WorkloadClass,
    };
    use crate::model_config::{
        ModelAliasTargets, ModelPlan, AUTO_POWERFUL_MODEL_ID, AUTO_QUICK_MODEL_ID,
        DEEPSEEK_V4_1_FLASH_MODEL_ID, GLM_5_3_FLASH_MODEL_ID, GLM_5_3_MODEL_ID, KIMI_K3_MODEL_ID,
        QUICK_MODEL_ID,
    };
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
            RouteSelectionSource::StaticSplit,
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
        route_failure_terminal_with_kind(
            provider,
            public_model,
            provider_model,
            AttemptFailureKind::StreamTimeout,
        )
    }

    fn route_failure_terminal_with_kind(
        provider: ProviderId,
        public_model: &str,
        provider_model: &str,
        kind: AttemptFailureKind,
    ) -> AttemptTerminal {
        let failure = AttemptFailure::new(
            kind,
            AttemptStage::Stream,
            ReplaySafety::NotProvenPreAcceptance,
        );
        let route = RouteIdentity::new(
            provider,
            public_model,
            provider_model,
            public_model,
            RouteSelectionSource::StaticSplit,
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
    fn glm_weights_preserve_provider_mapping_and_public_identity() {
        let router = ProviderRouter::default();
        let proxy_router = proxy_router_with_both_providers();
        let mut tinfoil_count = 0;
        let mut continuum_count = 0;
        for bucket in 0..100 {
            let mut intent = intent(GLM_5_3_MODEL_ID, GLM_5_3_MODEL_ID);
            intent.account_uuid = uuid_for_bucket(bucket);
            let selected = router
                .select_active_completion_route(&proxy_router, &intent)
                .expect("weighted GLM route");

            let (expected_provider, upstream_model) = if bucket < 75 {
                continuum_count += 1;
                (ProviderId::Continuum, "glm-5.3")
            } else {
                tinfoil_count += 1;
                (ProviderId::Tinfoil, GLM_5_3_MODEL_ID)
            };
            assert_eq!(selected.provider, expected_provider, "bucket {bucket}");
            assert_eq!(selected.provider_model_id, upstream_model);
            assert_eq!(selected.public_model_id, GLM_5_3_MODEL_ID);
            assert_eq!(selected.response_model_id, GLM_5_3_MODEL_ID);
            assert_eq!(selected.bucket, Some(bucket));
            assert_eq!(selected.selection_source, RouteSelectionSource::StaticSplit);
        }
        assert_eq!((tinfoil_count, continuum_count), (25, 75));
    }

    #[test]
    fn glm_routing_uses_the_healthy_same_model_provider() {
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

        let selected = router
            .select_active_completion_route(&proxy_router, &intent)
            .expect("healthy GLM route");
        assert_eq!(selected.provider, ProviderId::Tinfoil);
        assert_eq!(selected.public_model_id, GLM_5_3_MODEL_ID);
    }

    #[test]
    fn single_provider_routing_returns_capacity_without_substituting_a_model() {
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

        let error = router
            .select_active_completion_route(&proxy_router, &intent)
            .expect_err("an explicit model cannot substitute another public model");
        assert!(matches!(
            error,
            ProviderRoutingError::CapacityUnavailable { model, .. }
                if model == KIMI_K3_MODEL_ID
        ));
    }

    #[test]
    fn test_golden_completion_route_matrix_by_selector_plan_and_provider_configuration() {
        #[derive(Clone, Copy)]
        struct Case {
            name: &'static str,
            selector: &'static str,
            plan: ModelPlan,
            bucket: u8,
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
                bucket: 73,
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
                bucket: 73,
                continuum_available: true,
                expected_access: false,
                expected_public_model: GLM_5_3_MODEL_ID,
                expected_provider: "continuum",
                expected_provider_model: "glm-5.3",
                expected_source: RouteSelectionSource::StaticSplit,
            },
            Case {
                name: "paid auto quick",
                selector: AUTO_QUICK_MODEL_ID,
                plan: ModelPlan::Paid,
                bucket: 73,
                continuum_available: true,
                expected_access: true,
                expected_public_model: DEEPSEEK_V4_1_FLASH_MODEL_ID,
                expected_provider: "tinfoil",
                expected_provider_model: DEEPSEEK_V4_1_FLASH_MODEL_ID,
                expected_source: RouteSelectionSource::StaticSplit,
            },
            Case {
                name: "paid auto powerful uses GLM weighted route",
                selector: AUTO_POWERFUL_MODEL_ID,
                plan: ModelPlan::Paid,
                bucket: 73,
                continuum_available: true,
                expected_access: true,
                expected_public_model: GLM_5_3_MODEL_ID,
                expected_provider: "continuum",
                expected_provider_model: "glm-5.3",
                expected_source: RouteSelectionSource::StaticSplit,
            },
            Case {
                name: "explicit K3 is independent of the auto target",
                selector: KIMI_K3_MODEL_ID,
                plan: ModelPlan::Paid,
                bucket: 73,
                continuum_available: true,
                expected_access: true,
                expected_public_model: KIMI_K3_MODEL_ID,
                expected_provider: "tinfoil",
                expected_provider_model: KIMI_K3_MODEL_ID,
                expected_source: RouteSelectionSource::StaticSplit,
            },
            Case {
                name: "explicit GLM 5.3 Tinfoil bucket",
                selector: GLM_5_3_MODEL_ID,
                plan: ModelPlan::Paid,
                bucket: 75,
                continuum_available: true,
                expected_access: true,
                expected_public_model: GLM_5_3_MODEL_ID,
                expected_provider: "tinfoil",
                expected_provider_model: GLM_5_3_MODEL_ID,
                expected_source: RouteSelectionSource::StaticSplit,
            },
            Case {
                name: "explicit GLM 5.3 Continuum bucket",
                selector: GLM_5_3_MODEL_ID,
                plan: ModelPlan::Paid,
                bucket: 73,
                continuum_available: true,
                expected_access: true,
                expected_public_model: GLM_5_3_MODEL_ID,
                expected_provider: "continuum",
                expected_provider_model: "glm-5.3",
                expected_source: RouteSelectionSource::StaticSplit,
            },
            Case {
                name: "explicit GLM 5.3 Tinfoil bucket without a Continuum proxy",
                selector: GLM_5_3_MODEL_ID,
                plan: ModelPlan::Paid,
                bucket: 75,
                continuum_available: false,
                expected_access: true,
                expected_public_model: GLM_5_3_MODEL_ID,
                expected_provider: "tinfoil",
                expected_provider_model: GLM_5_3_MODEL_ID,
                expected_source: RouteSelectionSource::Fallback,
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
            let intent = InferenceIntent::new(
                uuid_for_bucket(case.bucket),
                case.selector,
                resolved_model,
                case.plan,
                InferenceSurface::Responses,
                WorkloadClass::Interactive,
            );
            let selected = router
                .select_active_completion_route(&proxy_router, &intent)
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
            let expected_bucket =
                if case.expected_public_model == GLM_5_3_MODEL_ID && case.continuum_available {
                    Some(case.bucket)
                } else {
                    None
                };
            assert_eq!(selected.bucket, expected_bucket, "{}", case.name);
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
                for bucket in [0, 29, 30, 69, 70, 74, 75, 99] {
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
    fn shadow_planner_matches_active_error_classes_for_unknown_models() {
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
            let active = router.select_active_completion_route(&proxy_router, &intent);
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
    fn baseline_planner_remains_health_independent_of_active_selection() {
        let router = ProviderRouter::default();
        let proxy_router = proxy_router_with_both_providers();
        let mut intent = intent(GLM_5_3_MODEL_ID, GLM_5_3_MODEL_ID);
        intent.account_uuid = uuid_for_bucket(75);
        let active_before = router
            .select_active_completion_route(&proxy_router, &intent)
            .expect("active route before health observation");
        let shadow_before = router
            .shadow_completion_plan(&proxy_router, &intent)
            .expect("baseline route before health observation");
        assert_eq!(active_before.provider, ProviderId::Tinfoil);

        let terminal = capacity_terminal(
            ProviderId::Tinfoil,
            GLM_5_3_MODEL_ID,
            GLM_5_3_MODEL_ID,
            429,
            Duration::from_secs(60),
        );
        let report = router.observe_attempt_terminal(&terminal, ShadowObservationMode::Update);
        assert!(matches!(
            report.snapshot.expect("known route").effective,
            ShadowDisposition::WouldOpen { .. }
        ));

        let active_after = router
            .select_active_completion_route(&proxy_router, &intent)
            .expect("healthy active route");
        let shadow_after = router
            .shadow_completion_plan(&proxy_router, &intent)
            .expect("unchanged baseline route");
        assert_eq!(active_after.provider, ProviderId::Continuum);
        assert_eq!(shadow_after, shadow_before);
        assert_eq!(
            active_after.public_model_id,
            shadow_after.selected.public_model_id
        );
        assert!(matches!(
            compare_shadow_route(&Ok(active_after), &Ok(shadow_after)),
            ShadowRouteComparison::Mismatch { .. }
        ));
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
                75,
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
                    uuid_for_bucket(75),
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
    fn flash_standard_failure_threshold_selects_the_alternate_in_both_directions() {
        let proxy_router = proxy_router_with_both_providers();
        for (bucket, provider, provider_model, alternate) in [
            (
                0,
                ProviderId::Continuum,
                "glm-5.3-flash",
                ProviderId::Tinfoil,
            ),
            (
                75,
                ProviderId::Tinfoil,
                GLM_5_3_FLASH_MODEL_ID,
                ProviderId::Continuum,
            ),
        ] {
            for kind in [
                AttemptFailureKind::ResponseStartTimeout,
                AttemptFailureKind::Transport,
                AttemptFailureKind::UpstreamStreamError,
                AttemptFailureKind::StreamTimeout,
            ] {
                for surface in [
                    InferenceSurface::ChatCompletions,
                    InferenceSurface::Responses,
                ] {
                    for selector in [GLM_5_3_FLASH_MODEL_ID, AUTO_QUICK_MODEL_ID] {
                        let router = ProviderRouter::default();
                        let intent = InferenceIntent::new(
                            uuid_for_bucket(bucket),
                            selector,
                            GLM_5_3_FLASH_MODEL_ID,
                            ModelPlan::Paid,
                            surface,
                            WorkloadClass::Interactive,
                        );
                        let pinned = router
                            .select_active_completion_route(&proxy_router, &intent)
                            .expect("healthy Flash route");
                        assert_eq!(pinned.provider, provider);
                        for observed_failures in 1..=3 {
                            router.observe_attempt_terminal(
                                &route_failure_terminal_with_kind(
                                    provider,
                                    GLM_5_3_FLASH_MODEL_ID,
                                    provider_model,
                                    kind,
                                ),
                                ShadowObservationMode::Update,
                            );
                            let selected = router
                                .select_active_completion_route(&proxy_router, &intent)
                                .expect("a healthy Flash route remains");
                            let expected = if observed_failures < 3 {
                                provider
                            } else {
                                alternate
                            };
                            assert_eq!(selected.provider, expected,
                                "{provider:?} {kind:?} {surface:?} {selector} failure {observed_failures}");
                            assert_eq!(selected.public_model_id, GLM_5_3_FLASH_MODEL_ID);
                            assert_eq!(selected.response_model_id, GLM_5_3_FLASH_MODEL_ID);
                            if observed_failures == 3 {
                                assert_eq!(
                                    selected.selection_source,
                                    RouteSelectionSource::Fallback
                                );
                            }
                        }
                        // A route prepared before the failure keeps its identity; its
                        // send-time claim rejects it instead of sending on the open route.
                        assert_eq!(pinned.provider, provider);
                        assert!(matches!(
                            router.try_claim_probe(&pinned.identity().route_key()),
                            ProbeClaimResult::Rejected { .. }
                        ));
                    }
                }
            }
        }
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
    fn glm_flash_weights_preserve_provider_mapping_and_public_identity() {
        let router = ProviderRouter::default();
        let proxies = proxy_router_with_both_providers();
        for bucket in 0..100 {
            let mut request = intent(GLM_5_3_FLASH_MODEL_ID, GLM_5_3_FLASH_MODEL_ID);
            request.account_uuid = uuid_for_bucket(bucket);
            let expected = if bucket < 75 {
                ProviderId::Continuum
            } else {
                ProviderId::Tinfoil
            };
            let selected = router
                .select_active_completion_route(&proxies, &request)
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
            assert_eq!(selected.bucket, Some(bucket));
            assert_eq!(selected.selection_source, RouteSelectionSource::StaticSplit);
        }
    }

    #[test]
    fn glm_flash_falls_back_when_continuum_is_not_configured() {
        let router = ProviderRouter::default();
        let proxies = ProxyRouter::new(
            "https://api.openai.com".to_string(),
            Some("synthetic-openai-key".to_string()),
            "http://tinfoil.example.com".to_string(),
        );
        let selected = router
            .select_active_completion_route(
                &proxies,
                &intent(GLM_5_3_FLASH_MODEL_ID, GLM_5_3_FLASH_MODEL_ID),
            )
            .expect("configured Tinfoil fallback");
        assert_eq!(selected.provider, ProviderId::Tinfoil);
        assert_eq!(selected.selection_source, RouteSelectionSource::Fallback);
        assert_eq!(selected.proxy.api_key, None);
    }

    #[test]
    fn test_removed_kimi_k2_6_is_unsupported() {
        let router = ProviderRouter::default();
        let proxy_router = proxy_router_with_both_providers();

        let error = router
            .select_active_completion_route(&proxy_router, &intent("kimi-k2-6", "kimi-k2-6"))
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
    fn single_provider_model_preserves_tinfoil_completion_route() {
        let router = ProviderRouter::default();
        let proxy_router = proxy_router_with_both_providers();

        let selected = router
            .select_active_completion_route(&proxy_router, &intent("gpt-oss-120b", "gpt-oss-120b"))
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
            let mut request = intent(model_id, model_id);
            request.account_uuid = uuid_for_bucket(75);
            let selected = router
                .select_active_completion_route(&proxy_router, &request)
                .expect("canonical Tinfoil model should route");

            assert_eq!(selected.proxy.provider_name, "tinfoil");
            assert_eq!(selected.public_model_id, model_id);
            assert_eq!(selected.provider_model_id, model_id);
            assert_eq!(selected.response_model_id, model_id);
            assert_eq!(
                selected.bucket,
                if model_id == GLM_5_3_FLASH_MODEL_ID {
                    Some(75)
                } else {
                    None
                }
            );
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
                .select_active_completion_route(&proxy_router, &intent(model_id, model_id))
                .expect_err("non-canonical model spelling should be rejected");

            assert_eq!(
                error,
                ProviderRoutingError::UnsupportedModel(model_id.to_string())
            );
        }
    }

    #[test]
    fn resolved_free_auto_quick_uses_the_canonical_tinfoil_route() {
        let router = ProviderRouter::default();
        let proxy_router = ProxyRouter::new(
            "http://continuum.example.com".to_string(),
            None,
            "http://tinfoil.example.com".to_string(),
        );

        let targets = ModelAliasTargets::for_plan(ModelPlan::Free);
        let request = InferenceIntent::new(
            uuid_for_bucket(50),
            AUTO_QUICK_MODEL_ID,
            targets.resolve(AUTO_QUICK_MODEL_ID),
            ModelPlan::Free,
            InferenceSurface::ChatCompletions,
            WorkloadClass::Interactive,
        );
        let selected = router
            .select_active_completion_route(&proxy_router, &request)
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
    fn active_routing_rejects_unknown_model_passthrough() {
        let router = ProviderRouter::default();
        let proxy_router = ProxyRouter::new(
            "http://continuum.example.com".to_string(),
            None,
            "http://tinfoil.example.com".to_string(),
        );

        let error = router
            .select_active_completion_route(
                &proxy_router,
                &intent("provider-native-model", "provider-native-model"),
            )
            .expect_err("unsupported model");

        assert_eq!(
            error,
            ProviderRoutingError::UnsupportedModel("provider-native-model".to_string())
        );
    }

    #[test]
    fn active_routing_rejects_unknown_models() {
        let router = ProviderRouter::default();
        let proxy_router = proxy_router_with_both_providers();

        let error = router
            .select_active_completion_route(
                &proxy_router,
                &intent("unknown-model", "unknown-model"),
            )
            .expect_err("unsupported model");

        assert_eq!(
            error,
            ProviderRoutingError::UnsupportedModel("unknown-model".to_string())
        );
    }

    #[test]
    fn glm_falls_back_to_tinfoil_when_continuum_is_missing() {
        let router = ProviderRouter::default();
        let proxy_router = ProxyRouter::new(
            "https://api.openai.com".to_string(),
            None,
            "http://tinfoil.example.com".to_string(),
        );

        let selected = router
            .select_active_completion_route(
                &proxy_router,
                &intent(GLM_5_3_MODEL_ID, GLM_5_3_MODEL_ID),
            )
            .expect("configured Tinfoil fallback");
        assert_eq!(selected.provider, ProviderId::Tinfoil);
        assert_eq!(selected.public_model_id, GLM_5_3_MODEL_ID);
        assert_eq!(selected.provider_model_id, GLM_5_3_MODEL_ID);
        assert_eq!(selected.response_model_id, GLM_5_3_MODEL_ID);
        assert_eq!(selected.selection_source, RouteSelectionSource::Fallback);
        assert_eq!(selected.bucket, None);
    }

    fn open_route_with_capacity_failure(
        router: &ProviderRouter,
        provider: ProviderId,
        public_model: &str,
        provider_model: &str,
        status: u16,
        retry_after_seconds: u64,
    ) {
        router.observe_attempt_terminal(
            &capacity_terminal(
                provider,
                public_model,
                provider_model,
                status,
                Duration::from_secs(retry_after_seconds),
            ),
            ShadowObservationMode::Update,
        );
    }

    fn open_route_with_transport_failures(
        router: &ProviderRouter,
        provider: ProviderId,
        public_model: &str,
        provider_model: &str,
    ) {
        for _ in 0..3 {
            router.observe_attempt_terminal(
                &route_failure_terminal(provider, public_model, provider_model),
                ShadowObservationMode::Update,
            );
        }
    }

    #[test]
    fn model_availability_applies_all_gates_from_one_snapshot() {
        use crate::inference::auto_model::ModelAvailability;
        let router = ProviderRouter::default();
        let proxy_router = proxy_router_with_both_providers();
        let candidates = [
            DEEPSEEK_V4_1_FLASH_MODEL_ID,
            GLM_5_3_FLASH_MODEL_ID,
            GLM_5_3_MODEL_ID,
            KIMI_K3_MODEL_ID,
            "unknown-model",
        ];

        let healthy = router.model_availability(&proxy_router, &candidates);
        assert_eq!(
            healthy[DEEPSEEK_V4_1_FLASH_MODEL_ID],
            ModelAvailability::Available(
                ConfiguredProviders::none().with_provider(ProviderId::Tinfoil)
            )
        );
        assert_eq!(
            healthy[GLM_5_3_FLASH_MODEL_ID],
            ModelAvailability::Available(ConfiguredProviders::all())
        );
        assert_eq!(
            healthy[GLM_5_3_MODEL_ID],
            ModelAvailability::Available(ConfiguredProviders::all())
        );
        assert_eq!(healthy["unknown-model"], ModelAvailability::NotConfigured);

        // Every model observes the same route-health gate. DeepSeek and Flash
        // both become unavailable after all of their routes reach the threshold.
        open_route_with_transport_failures(
            &router,
            ProviderId::Tinfoil,
            DEEPSEEK_V4_1_FLASH_MODEL_ID,
            DEEPSEEK_V4_1_FLASH_MODEL_ID,
        );
        open_route_with_transport_failures(
            &router,
            ProviderId::Tinfoil,
            GLM_5_3_FLASH_MODEL_ID,
            GLM_5_3_FLASH_MODEL_ID,
        );
        open_route_with_transport_failures(
            &router,
            ProviderId::Continuum,
            GLM_5_3_FLASH_MODEL_ID,
            "glm-5.3-flash",
        );
        let after_transport = router.model_availability(&proxy_router, &candidates);
        assert!(matches!(
            after_transport[DEEPSEEK_V4_1_FLASH_MODEL_ID],
            ModelAvailability::Unavailable { retry_after } if retry_after == Duration::from_secs(30)
        ));
        assert!(matches!(
            after_transport[GLM_5_3_FLASH_MODEL_ID],
            ModelAvailability::Unavailable { retry_after } if retry_after == Duration::from_secs(30)
        ));
        // The same snapshot drives the single-model selector.
        let flash_intent = intent(GLM_5_3_FLASH_MODEL_ID, GLM_5_3_FLASH_MODEL_ID);
        assert!(matches!(
            router.select_active_completion_route(&proxy_router, &flash_intent),
            Err(ProviderRoutingError::CapacityUnavailable { model, retry_after })
                if model == GLM_5_3_FLASH_MODEL_ID && retry_after == Duration::from_secs(30)
        ));
        let deepseek_intent = intent(DEEPSEEK_V4_1_FLASH_MODEL_ID, DEEPSEEK_V4_1_FLASH_MODEL_ID);
        assert!(matches!(
            router.select_active_completion_route(&proxy_router, &deepseek_intent),
            Err(ProviderRoutingError::CapacityUnavailable { .. })
        ));

        // On fresh routes, a Continuum 429 opens the provider-account pool:
        // GLM and GLM Flash both keep only their Tinfoil routes.
        let router = ProviderRouter::default();
        open_route_with_capacity_failure(
            &router,
            ProviderId::Continuum,
            GLM_5_3_MODEL_ID,
            "glm-5.3",
            429,
            60,
        );
        let after_429 = router.model_availability(&proxy_router, &candidates);
        assert_eq!(
            after_429[GLM_5_3_MODEL_ID],
            ModelAvailability::Available(
                ConfiguredProviders::none().with_provider(ProviderId::Tinfoil)
            )
        );
        assert_eq!(
            after_429[GLM_5_3_FLASH_MODEL_ID],
            ModelAvailability::Available(
                ConfiguredProviders::none().with_provider(ProviderId::Tinfoil)
            )
        );

        // A 503 on the remaining Tinfoil GLM route leaves GLM fully unavailable
        // with the earliest bounded recovery.
        open_route_with_capacity_failure(
            &router,
            ProviderId::Tinfoil,
            GLM_5_3_MODEL_ID,
            GLM_5_3_MODEL_ID,
            503,
            45,
        );
        let after_503 = router.model_availability(&proxy_router, &candidates);
        assert!(matches!(
            after_503[GLM_5_3_MODEL_ID],
            ModelAvailability::Unavailable { retry_after } if retry_after == Duration::from_secs(45)
        ));
        assert_eq!(
            after_503[KIMI_K3_MODEL_ID],
            ModelAvailability::Available(
                ConfiguredProviders::none().with_provider(ProviderId::Tinfoil)
            )
        );

        // Without a Continuum proxy, Continuum routes are not configured at all.
        let tinfoil_only = ProxyRouter::new(
            "https://api.openai.com".to_string(),
            None,
            "http://tinfoil.example.com".to_string(),
        );
        let without_continuum = ProviderRouter::default()
            .model_availability(&tinfoil_only, &[GLM_5_3_FLASH_MODEL_ID, GLM_5_3_MODEL_ID]);
        assert_eq!(
            without_continuum[GLM_5_3_FLASH_MODEL_ID],
            ModelAvailability::Available(
                ConfiguredProviders::none().with_provider(ProviderId::Tinfoil)
            )
        );
    }

    #[test]
    fn auto_quick_uses_remaining_flash_route_then_rejects_both_open_routes() {
        use crate::inference::auto_model::{
            select_auto_model, AutoModelError, AutoModelRequirements, AutoModelSelectionInput,
            PromptTokenEstimate,
        };
        use crate::inference::ModelSelectionMode;

        let proxy_router = proxy_router_with_both_providers();
        for (failed_provider, failed_model, remaining_provider, remaining_model) in [
            (
                ProviderId::Tinfoil,
                GLM_5_3_FLASH_MODEL_ID,
                ProviderId::Continuum,
                "glm-5.3-flash",
            ),
            (
                ProviderId::Continuum,
                "glm-5.3-flash",
                ProviderId::Tinfoil,
                GLM_5_3_FLASH_MODEL_ID,
            ),
        ] {
            let router = ProviderRouter::default();
            open_route_with_transport_failures(
                &router,
                ProviderId::Tinfoil,
                DEEPSEEK_V4_1_FLASH_MODEL_ID,
                DEEPSEEK_V4_1_FLASH_MODEL_ID,
            );
            open_route_with_transport_failures(
                &router,
                failed_provider,
                GLM_5_3_FLASH_MODEL_ID,
                failed_model,
            );
            let choose = |sticky_model_id| {
                let snapshot = router.model_availability(
                    &proxy_router,
                    &[DEEPSEEK_V4_1_FLASH_MODEL_ID, GLM_5_3_FLASH_MODEL_ID],
                );
                select_auto_model(AutoModelSelectionInput {
                    mode: ModelSelectionMode::AutoQuick,
                    plan: ModelPlan::Paid,
                    sticky_model_id,
                    excluded: None,
                    requirements: AutoModelRequirements {
                        vision: false,
                        kimi_tool_history_compatible: true,
                        prompt_tokens: PromptTokenEstimate::Known(0),
                    },
                    availability: &|model| snapshot[model],
                })
            };
            // Both first selection and a remembered Flash model retain only
            // the provider whose route-health gate is still eligible.
            for sticky in [None, Some(GLM_5_3_FLASH_MODEL_ID)] {
                let decision = choose(sticky).expect("Flash alternate remains eligible");
                assert_eq!(decision.chosen_model_id, GLM_5_3_FLASH_MODEL_ID);
                let selected = router
                    .select_active_completion_route(
                        &proxy_router,
                        &intent(AUTO_QUICK_MODEL_ID, decision.chosen_model_id),
                    )
                    .expect("remaining Flash provider");
                assert_eq!(selected.provider, remaining_provider);
            }
            open_route_with_transport_failures(
                &router,
                remaining_provider,
                GLM_5_3_FLASH_MODEL_ID,
                remaining_model,
            );
            for sticky in [None, Some(GLM_5_3_FLASH_MODEL_ID)] {
                assert!(matches!(choose(sticky),
                    Err(AutoModelError::NoEligibleCandidate { retry_after: Some(retry_after), .. })
                        if retry_after == Duration::from_secs(30)));
            }
        }
    }

    #[test]
    fn remembered_same_model_provider_is_preferred_only_while_eligible() {
        use crate::inference::sticky_routes::StickyRoute;
        let router = ProviderRouter::default();
        let proxy_router = proxy_router_with_both_providers();
        // Bucket 0 lands on Continuum for GLM 5.3 by weight.
        let glm = intent(GLM_5_3_MODEL_ID, GLM_5_3_MODEL_ID);
        let baseline = router
            .select_active_completion_route(&proxy_router, &glm)
            .expect("baseline GLM route");
        assert_eq!(baseline.provider, ProviderId::Continuum);
        assert_eq!(baseline.selection_source, RouteSelectionSource::StaticSplit);

        // The account previously executed GLM on Tinfoil (for example after a
        // Continuum capacity failure that has since healed).
        router.remember_route(
            &glm,
            &SelectedProviderRoute {
                provider: ProviderId::Tinfoil,
                proxy: proxy_router.get_tinfoil_proxy(),
                public_model_id: GLM_5_3_MODEL_ID.to_string(),
                provider_model_id: GLM_5_3_MODEL_ID.to_string(),
                response_model_id: GLM_5_3_MODEL_ID.to_string(),
                bucket: None,
                selection_source: RouteSelectionSource::Fallback,
            },
        );
        let sticky = router
            .select_active_completion_route(&proxy_router, &glm)
            .expect("sticky GLM route");
        assert_eq!(sticky.provider, ProviderId::Tinfoil);
        assert_eq!(sticky.provider_model_id, GLM_5_3_MODEL_ID);
        assert_eq!(sticky.public_model_id, GLM_5_3_MODEL_ID);
        assert_eq!(sticky.selection_source, RouteSelectionSource::Sticky);
        assert_eq!(sticky.bucket, None);

        // A different account with the same bucket keeps its weighted route.
        let mut other = intent(GLM_5_3_MODEL_ID, GLM_5_3_MODEL_ID);
        other.account_uuid = uuid_for_bucket(100);
        assert_eq!(
            router
                .select_active_completion_route(&proxy_router, &other)
                .expect("other account")
                .provider,
            ProviderId::Continuum
        );

        // The memory is keyed by surface as well: the same account's Chat
        // Completions requests for the same selector keep their weighted route.
        let mut chat = intent(GLM_5_3_MODEL_ID, GLM_5_3_MODEL_ID);
        chat.surface = InferenceSurface::ChatCompletions;
        assert_eq!(
            router
                .select_active_completion_route(&proxy_router, &chat)
                .expect("chat surface")
                .selection_source,
            RouteSelectionSource::StaticSplit
        );

        // The memory is keyed by selector: the same account's Auto Powerful
        // requests (which resolve to GLM today) are not affected by an
        // explicit-GLM memory, and vice versa.
        let auto_powerful = intent(AUTO_POWERFUL_MODEL_ID, GLM_5_3_MODEL_ID);
        assert_eq!(
            router
                .select_active_completion_route(&proxy_router, &auto_powerful)
                .expect("auto powerful")
                .selection_source,
            RouteSelectionSource::StaticSplit
        );

        // A remembered route for another public model is ignored.
        router.sticky_routes().record(
            glm.account_uuid,
            glm.surface,
            GLM_5_3_MODEL_ID,
            StickyRoute {
                public_model_id: GLM_5_3_FLASH_MODEL_ID.to_string(),
                provider: ProviderId::Tinfoil,
                provider_model_id: GLM_5_3_FLASH_MODEL_ID.to_string(),
            },
        );
        assert_eq!(
            router
                .select_active_completion_route(&proxy_router, &glm)
                .expect("foreign sticky model ignored")
                .selection_source,
            RouteSelectionSource::StaticSplit
        );

        // Excluded routes (a lost first-send claim) beat the memory.
        router.remember_route(&glm, &sticky);
        let excluded = HashSet::from([sticky.identity().route_key()]);
        let replanned = router
            .select_active_completion_route_excluding(&proxy_router, &glm, &excluded)
            .expect("replan without the sticky route");
        assert_eq!(replanned.provider, ProviderId::Continuum);

        // An opened sticky route falls back to weighted selection among the
        // remaining eligible providers.
        open_route_with_capacity_failure(
            &router,
            ProviderId::Tinfoil,
            GLM_5_3_MODEL_ID,
            GLM_5_3_MODEL_ID,
            429,
            60,
        );
        let after_open = router
            .select_active_completion_route(&proxy_router, &glm)
            .expect("Continuum remains eligible");
        assert_eq!(after_open.provider, ProviderId::Continuum);
        // The planner reports the existing same-model fallback identity, not
        // a sticky choice, once the remembered provider is excluded by health.
        assert_eq!(after_open.selection_source, RouteSelectionSource::Fallback);
    }

    #[test]
    fn remembered_route_equal_to_the_weighted_bucket_keeps_its_static_identity() {
        let router = ProviderRouter::default();
        let proxy_router = proxy_router_with_both_providers();
        let glm = intent(GLM_5_3_MODEL_ID, GLM_5_3_MODEL_ID);
        let baseline = router
            .select_active_completion_route(&proxy_router, &glm)
            .expect("baseline");
        router.remember_route(&glm, &baseline);

        let again = router
            .select_active_completion_route(&proxy_router, &glm)
            .expect("same route");
        assert_eq!(again.identity(), baseline.identity());
        assert_eq!(again.selection_source, RouteSelectionSource::StaticSplit);
        assert_eq!(again.bucket, Some(0));
    }

    #[test]
    fn remembered_routes_expire_after_the_idle_window() {
        use crate::inference::sticky_routes::{StickyRoute, STICKY_ROUTE_IDLE_TTL};
        let router = ProviderRouter::default();
        let proxy_router = proxy_router_with_both_providers();
        let glm = intent(GLM_5_3_MODEL_ID, GLM_5_3_MODEL_ID);
        let recorded_at = std::time::Instant::now() - STICKY_ROUTE_IDLE_TTL;
        router.sticky_routes().record_at(
            glm.account_uuid,
            glm.surface,
            GLM_5_3_MODEL_ID,
            StickyRoute {
                public_model_id: GLM_5_3_MODEL_ID.to_string(),
                provider: ProviderId::Tinfoil,
                provider_model_id: GLM_5_3_MODEL_ID.to_string(),
            },
            recorded_at,
        );

        // Idle for the full window: weighted policy resumes.
        let selected = router
            .select_active_completion_route(&proxy_router, &glm)
            .expect("weighted route");
        assert_eq!(selected.provider, ProviderId::Continuum);
        assert_eq!(selected.selection_source, RouteSelectionSource::StaticSplit);

        // A fresh accepted route resumes sticky selection.
        router.sticky_routes().record(
            glm.account_uuid,
            glm.surface,
            GLM_5_3_MODEL_ID,
            StickyRoute {
                public_model_id: GLM_5_3_MODEL_ID.to_string(),
                provider: ProviderId::Tinfoil,
                provider_model_id: GLM_5_3_MODEL_ID.to_string(),
            },
        );
        let sticky = router
            .select_active_completion_route(&proxy_router, &glm)
            .expect("sticky route");
        assert_eq!(sticky.provider, ProviderId::Tinfoil);
        assert_eq!(sticky.selection_source, RouteSelectionSource::Sticky);
    }
}
