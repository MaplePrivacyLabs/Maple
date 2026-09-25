//! Deterministic, credential-free completion route planning.
//!
//! The plan describes route identity and ordered same-model candidates without
//! naming or invoking an executable endpoint. Routing applies the configured
//! weights after filtering providers through one health snapshot, preferring
//! the account's remembered provider for the same model when it is still
//! eligible.

use crate::inference::{InferenceIntent, RouteIdentity};
use crate::provider_registry::{
    ProviderId, ProviderRegistry, RouteSelectionSource, SHADOW_ROUTING_POLICY_VERSION,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ConfiguredProviders {
    tinfoil: bool,
    continuum: bool,
}

impl ConfiguredProviders {
    pub(crate) const fn none() -> Self {
        Self {
            tinfoil: false,
            continuum: false,
        }
    }

    #[cfg(test)]
    pub(crate) const fn all() -> Self {
        Self {
            tinfoil: true,
            continuum: true,
        }
    }

    pub(crate) fn with_provider(mut self, provider: ProviderId) -> Self {
        match provider {
            ProviderId::Tinfoil => self.tinfoil = true,
            ProviderId::Continuum => self.continuum = true,
        }
        self
    }

    pub(crate) const fn contains(self, provider: ProviderId) -> bool {
        match provider {
            ProviderId::Tinfoil => self.tinfoil,
            ProviderId::Continuum => self.continuum,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RoutePlanningInput<'a> {
    pub(crate) intent: &'a InferenceIntent,
    pub(crate) configured_providers: ConfiguredProviders,
    /// The provider this account last executed the same public model on.
    /// Preferred over the weighted bucket only among eligible routes.
    pub(crate) remembered_provider: Option<ProviderId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RouteCandidate {
    pub(crate) provider: ProviderId,
    pub(crate) public_model_id: String,
    pub(crate) provider_model_id: String,
    pub(crate) response_model_id: String,
    pub(crate) effective_weight: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlanDecision {
    FixedRoute,
    StaticBucket,
    /// The account's remembered provider was eligible and differed from its
    /// weighted bucket; the warmed route wins.
    RememberedRoute,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CandidateScope {
    SamePublicModelOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RoutePlan {
    pub(crate) selected: RouteIdentity,
    pub(crate) eligible_routes: Vec<RouteCandidate>,
    pub(crate) decision: PlanDecision,
    pub(crate) candidate_scope: CandidateScope,
    pub(crate) policy_version: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RoutePlanningError {
    UnsupportedModel(String),
    NoEligibleRoute(String),
}

#[derive(Debug, Clone)]
struct EligibleRoute {
    candidate: RouteCandidate,
}

pub(crate) fn plan_completion_route(
    registry: &ProviderRegistry,
    input: RoutePlanningInput<'_>,
) -> Result<RoutePlan, RoutePlanningError> {
    let public_model_id = input.intent.public_model_id.as_str();
    let model = registry
        .completion_model(public_model_id)
        .ok_or_else(|| RoutePlanningError::UnsupportedModel(public_model_id.to_string()))?;

    let eligible_routes = model
        .routes
        .iter()
        .filter_map(|route| {
            if !route.enabled || route.weight == 0 {
                return None;
            }
            let provider = registry.provider(route.provider)?;
            if !provider.enabled
                || provider.weight == 0
                || !input.configured_providers.contains(route.provider)
            {
                return None;
            }

            Some(EligibleRoute {
                candidate: RouteCandidate {
                    provider: route.provider,
                    public_model_id: model.public_model_id.to_string(),
                    provider_model_id: route.provider_model_id.to_string(),
                    response_model_id: route.response_model_id.to_string(),
                    effective_weight: u32::from(provider.weight) * u32::from(route.weight),
                },
            })
        })
        .collect::<Vec<_>>();

    if eligible_routes.is_empty() {
        return Err(RoutePlanningError::NoEligibleRoute(
            model.public_model_id.to_string(),
        ));
    }

    // Report fallback when a configured route is unavailable.
    let enabled_route_count = model
        .routes
        .iter()
        .filter(|route| {
            route.enabled
                && route.weight > 0
                && registry
                    .provider(route.provider)
                    .is_some_and(|provider| provider.enabled && provider.weight > 0)
        })
        .count();
    let selection_source = if eligible_routes.len() < enabled_route_count {
        RouteSelectionSource::Fallback
    } else {
        RouteSelectionSource::StaticSplit
    };

    if eligible_routes.len() == 1 {
        return Ok(build_plan(
            &eligible_routes[0],
            &eligible_routes,
            selection_source,
            None,
            PlanDecision::FixedRoute,
        ));
    }

    let bucket = stable_account_bucket(input.intent.account_uuid);
    let selected = select_weighted_route(bucket, &eligible_routes)
        .ok_or_else(|| RoutePlanningError::NoEligibleRoute(model.public_model_id.to_string()))?;

    // Provider prompt caches make the route the account last used materially
    // cheaper than an otherwise equivalent one. When it differs from the
    // weighted choice and is still eligible, keep it.
    if let Some(remembered) = input
        .remembered_provider
        .filter(|provider| *provider != selected.candidate.provider)
        .and_then(|provider| {
            eligible_routes
                .iter()
                .find(|route| route.candidate.provider == provider)
        })
    {
        return Ok(build_plan(
            remembered,
            &eligible_routes,
            RouteSelectionSource::Sticky,
            None,
            PlanDecision::RememberedRoute,
        ));
    }

    Ok(build_plan(
        selected,
        &eligible_routes,
        selection_source,
        Some(bucket),
        PlanDecision::StaticBucket,
    ))
}

fn build_plan(
    selected: &EligibleRoute,
    eligible_routes: &[EligibleRoute],
    selection_source: RouteSelectionSource,
    bucket: Option<u8>,
    decision: PlanDecision,
) -> RoutePlan {
    let selected = &selected.candidate;
    RoutePlan {
        selected: RouteIdentity::new(
            selected.provider,
            selected.public_model_id.clone(),
            selected.provider_model_id.clone(),
            selected.response_model_id.clone(),
            selection_source,
            bucket,
        ),
        eligible_routes: eligible_routes
            .iter()
            .map(|route| route.candidate.clone())
            .collect(),
        decision,
        candidate_scope: CandidateScope::SamePublicModelOnly,
        policy_version: SHADOW_ROUTING_POLICY_VERSION,
    }
}

fn select_weighted_route(bucket: u8, routes: &[EligibleRoute]) -> Option<&EligibleRoute> {
    let total_weight = routes
        .iter()
        .map(|route| route.candidate.effective_weight)
        .sum::<u32>();
    if total_weight == 0 {
        return None;
    }

    let mut cumulative = 0u32;
    for (index, route) in routes.iter().enumerate() {
        let bucket_span = if index == routes.len() - 1 {
            100u32.saturating_sub(cumulative)
        } else {
            (route.candidate.effective_weight * 100) / total_weight
        };
        cumulative = cumulative.saturating_add(bucket_span);

        if u32::from(bucket) < cumulative || index == routes.len() - 1 {
            return Some(route);
        }
    }
    None
}

fn stable_account_bucket(account_uuid: uuid::Uuid) -> u8 {
    (u128::from_be_bytes(*account_uuid.as_bytes()) % 100) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_config::{
        ModelPlan, AUTO_POWERFUL_MODEL_ID, GLM_5_3_FLASH_MODEL_ID, GLM_5_3_MODEL_ID,
    };
    use crate::provider_registry::{ProviderId, PROVIDER_REGISTRY};
    use uuid::Uuid;

    fn intent(requested_model: &str, public_model: &str) -> InferenceIntent {
        InferenceIntent::new(
            Uuid::from_u128(73),
            requested_model,
            public_model,
            ModelPlan::Paid,
            crate::inference::InferenceSurface::Responses,
            crate::inference::WorkloadClass::Interactive,
        )
    }

    #[test]
    fn planner_uses_resolved_public_model_without_resolving_auto_again() {
        let intent = intent(AUTO_POWERFUL_MODEL_ID, GLM_5_3_FLASH_MODEL_ID);
        let plan = plan_completion_route(
            &PROVIDER_REGISTRY,
            RoutePlanningInput {
                intent: &intent,
                configured_providers: ConfiguredProviders::all(),
                remembered_provider: None,
            },
        )
        .expect("shadow route");

        assert_eq!(intent.requested_model_id, AUTO_POWERFUL_MODEL_ID);
        assert_eq!(plan.selected.public_model_id, GLM_5_3_FLASH_MODEL_ID);
        assert_eq!(plan.selected.provider_model_id, "glm-5.3-flash");
        assert_eq!(plan.selected.provider, ProviderId::Continuum);
        assert_eq!(plan.selected.bucket, Some(73));
        assert_eq!(plan.decision, PlanDecision::StaticBucket);
        assert_eq!(plan.eligible_routes.len(), 2);
    }

    #[test]
    fn alias_resolved_glm_5_3_uses_weighted_provider_selection() {
        let intent = intent(AUTO_POWERFUL_MODEL_ID, GLM_5_3_MODEL_ID);
        let plan = plan_completion_route(
            &PROVIDER_REGISTRY,
            RoutePlanningInput {
                intent: &intent,
                configured_providers: ConfiguredProviders::all(),
                remembered_provider: None,
            },
        )
        .expect("GLM 5.3 Continuum plan");

        assert_eq!(intent.requested_model_id, AUTO_POWERFUL_MODEL_ID);
        assert_eq!(intent.public_model_id, GLM_5_3_MODEL_ID);
        assert_eq!(plan.selected.provider, ProviderId::Continuum);
        assert_eq!(plan.selected.provider_model_id, "glm-5.3");
        assert_eq!(plan.selected.bucket, Some(73));
        assert_eq!(
            plan.selected.selection_source,
            RouteSelectionSource::StaticSplit
        );
        assert_eq!(plan.decision, PlanDecision::StaticBucket);
        assert_eq!(plan.eligible_routes.len(), 2);
        assert!(plan
            .eligible_routes
            .iter()
            .all(|route| route.public_model_id == GLM_5_3_MODEL_ID));
    }

    #[test]
    fn planner_is_deterministic_and_candidates_never_cross_public_models() {
        let intent = intent(GLM_5_3_MODEL_ID, GLM_5_3_MODEL_ID);
        let input = RoutePlanningInput {
            intent: &intent,
            configured_providers: ConfiguredProviders::all(),
            remembered_provider: None,
        };

        let first = plan_completion_route(&PROVIDER_REGISTRY, input).expect("first plan");
        let second = plan_completion_route(&PROVIDER_REGISTRY, input).expect("second plan");
        assert_eq!(first, second);
        assert_eq!(first.candidate_scope, CandidateScope::SamePublicModelOnly);
        assert!(first
            .eligible_routes
            .iter()
            .all(|route| route.public_model_id == intent.public_model_id));
        assert_eq!(
            first
                .eligible_routes
                .iter()
                .map(|route| (route.provider, route.provider_model_id.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (ProviderId::Continuum, "glm-5.3"),
                (ProviderId::Tinfoil, GLM_5_3_MODEL_ID),
            ]
        );
    }

    #[test]
    fn unavailable_provider_falls_back_without_authorizing_another_model() {
        let intent = intent(GLM_5_3_MODEL_ID, GLM_5_3_MODEL_ID);
        let configured = ConfiguredProviders::none().with_provider(ProviderId::Tinfoil);
        let plan = plan_completion_route(
            &PROVIDER_REGISTRY,
            RoutePlanningInput {
                intent: &intent,
                configured_providers: configured,
                remembered_provider: None,
            },
        )
        .expect("Tinfoil fallback plan");

        assert_eq!(plan.selected.provider, ProviderId::Tinfoil);
        assert_eq!(plan.selected.public_model_id, GLM_5_3_MODEL_ID);
        assert_eq!(
            plan.selected.selection_source,
            RouteSelectionSource::Fallback
        );
        assert_eq!(plan.decision, PlanDecision::FixedRoute);
        assert_eq!(plan.eligible_routes.len(), 1);
    }

    #[test]
    fn flash_is_a_distinct_dual_provider_model_not_a_glm_5_3_fallback() {
        let intent = intent(GLM_5_3_FLASH_MODEL_ID, GLM_5_3_FLASH_MODEL_ID);
        let plan = plan_completion_route(
            &PROVIDER_REGISTRY,
            RoutePlanningInput {
                intent: &intent,
                configured_providers: ConfiguredProviders::all(),
                remembered_provider: None,
            },
        )
        .expect("Flash route");

        assert_eq!(plan.selected.public_model_id, GLM_5_3_FLASH_MODEL_ID);
        assert_eq!(plan.eligible_routes.len(), 2);
        assert_eq!(
            plan.eligible_routes
                .iter()
                .map(|route| (route.provider, route.provider_model_id.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (ProviderId::Continuum, "glm-5.3-flash"),
                (ProviderId::Tinfoil, GLM_5_3_FLASH_MODEL_ID),
            ]
        );
        assert!(plan
            .eligible_routes
            .iter()
            .all(|route| route.public_model_id == GLM_5_3_FLASH_MODEL_ID));
    }

    #[test]
    fn glm_and_flash_send_seventy_five_percent_of_accounts_to_continuum_on_every_surface() {
        use crate::inference::{InferenceSurface, WorkloadClass};
        for model in [GLM_5_3_MODEL_ID, GLM_5_3_FLASH_MODEL_ID] {
            for (surface, workload) in [
                (InferenceSurface::Responses, WorkloadClass::Interactive),
                (
                    InferenceSurface::ChatCompletions,
                    WorkloadClass::Interactive,
                ),
                (InferenceSurface::Internal, WorkloadClass::Interactive),
                (InferenceSurface::Internal, WorkloadClass::Background),
            ] {
                let mut continuum_count = 0;
                for bucket in 0..100u8 {
                    let intent = InferenceIntent::new(
                        Uuid::from_u128(u128::from(bucket)),
                        model,
                        model,
                        ModelPlan::Paid,
                        surface,
                        workload,
                    );
                    let plan = plan_completion_route(
                        &PROVIDER_REGISTRY,
                        RoutePlanningInput {
                            intent: &intent,
                            configured_providers: ConfiguredProviders::all(),
                            remembered_provider: None,
                        },
                    )
                    .expect("weighted GLM or Flash plan");
                    let expected = if bucket < 75 {
                        continuum_count += 1;
                        ProviderId::Continuum
                    } else {
                        ProviderId::Tinfoil
                    };
                    assert_eq!(
                        plan.selected.provider, expected,
                        "{model}, bucket {bucket}, {surface:?}"
                    );
                    assert_eq!(plan.selected.bucket, Some(bucket));
                    assert_eq!(plan.decision, PlanDecision::StaticBucket);
                    assert_eq!(plan.selected.public_model_id, model);
                }
                assert_eq!(continuum_count, 75);
            }
        }
    }

    #[test]
    fn planner_fails_closed_for_unknown_and_unconfigured_models() {
        let unknown = intent("unknown-model", "unknown-model");
        assert_eq!(
            plan_completion_route(
                &PROVIDER_REGISTRY,
                RoutePlanningInput {
                    intent: &unknown,
                    configured_providers: ConfiguredProviders::all(),
                    remembered_provider: None,
                },
            ),
            Err(RoutePlanningError::UnsupportedModel(
                "unknown-model".to_string()
            ))
        );

        let glm_flash = intent(GLM_5_3_FLASH_MODEL_ID, GLM_5_3_FLASH_MODEL_ID);
        let none = ConfiguredProviders::none();
        assert_eq!(
            plan_completion_route(
                &PROVIDER_REGISTRY,
                RoutePlanningInput {
                    intent: &glm_flash,
                    configured_providers: none,
                    remembered_provider: None,
                },
            ),
            Err(RoutePlanningError::NoEligibleRoute(
                GLM_5_3_FLASH_MODEL_ID.to_string()
            ))
        );
    }

    #[test]
    fn remembered_provider_beats_the_bucket_only_when_eligible_and_different() {
        let mut intent = intent(GLM_5_3_MODEL_ID, GLM_5_3_MODEL_ID);
        intent.account_uuid = Uuid::from_u128(75);
        // Bucket 75 lands on Tinfoil by weight.
        let weighted = plan_completion_route(
            &PROVIDER_REGISTRY,
            RoutePlanningInput {
                intent: &intent,
                configured_providers: ConfiguredProviders::all(),
                remembered_provider: None,
            },
        )
        .expect("weighted plan");
        assert_eq!(weighted.selected.provider, ProviderId::Tinfoil);
        assert_eq!(weighted.decision, PlanDecision::StaticBucket);

        let remembered = plan_completion_route(
            &PROVIDER_REGISTRY,
            RoutePlanningInput {
                intent: &intent,
                configured_providers: ConfiguredProviders::all(),
                remembered_provider: Some(ProviderId::Continuum),
            },
        )
        .expect("remembered plan");
        assert_eq!(remembered.selected.provider, ProviderId::Continuum);
        assert_eq!(remembered.selected.provider_model_id, "glm-5.3");
        assert_eq!(remembered.selected.public_model_id, GLM_5_3_MODEL_ID);
        assert_eq!(
            remembered.selected.selection_source,
            RouteSelectionSource::Sticky
        );
        assert_eq!(remembered.selected.bucket, None);
        assert_eq!(remembered.decision, PlanDecision::RememberedRoute);
        assert_eq!(remembered.eligible_routes, weighted.eligible_routes);

        // Equal to the bucket: the weighted identity is kept.
        let same = plan_completion_route(
            &PROVIDER_REGISTRY,
            RoutePlanningInput {
                intent: &intent,
                configured_providers: ConfiguredProviders::all(),
                remembered_provider: Some(ProviderId::Tinfoil),
            },
        )
        .expect("same plan");
        assert_eq!(same, weighted);

        // Not eligible (health or configuration excluded it): ignored.
        let excluded = plan_completion_route(
            &PROVIDER_REGISTRY,
            RoutePlanningInput {
                intent: &intent,
                configured_providers: ConfiguredProviders::none()
                    .with_provider(ProviderId::Tinfoil),
                remembered_provider: Some(ProviderId::Continuum),
            },
        )
        .expect("excluded plan");
        assert_eq!(excluded.selected.provider, ProviderId::Tinfoil);
        assert_eq!(excluded.decision, PlanDecision::FixedRoute);
        assert_eq!(
            excluded.selected.selection_source,
            RouteSelectionSource::Fallback
        );
    }
}
