//! Router v2 Auto model policy.
//!
//! An Auto selector names a product tier, not a model. Router v2 resolves the
//! tier's preferred model exactly as before, but when every provider route of
//! that model is unavailable it may choose another approved model from the same
//! tier before any model-dependent preparation happens. Explicit model requests
//! never enter this policy, and Router v1 never calls it.
//!
//! The stage is pure: it consumes one coherent availability snapshot supplied by
//! the caller, never contacts a provider, and never claims a health gate. The
//! chosen model is still subject to the same-model provider planner, the
//! first-send probe claim, and plan access checks downstream.

use super::ModelSelectionMode;
use crate::inference::health::MIN_CAPACITY_COOLDOWN;
use crate::inference_planning::ConfiguredProviders;
use crate::model_config::{
    model_capabilities, model_context_window, ModelAliasTargets, ModelPlan, GLM_5_3_FLASH_MODEL_ID,
    KIMI_K3_MODEL_ID,
};
use crate::web::responses::context_builder::model_uses_kimi_tool_call_ids;
use std::cell::OnceCell;
use std::time::Duration;

pub(crate) const AUTO_MODEL_POLICY_VERSION: &str = "auto-model-v1";

// Alternates are product policy and are tried in order only after every earlier
// candidate is unavailable or incompatible. The tier's preferred model is the
// Router v2 alias target from `ModelAliasTargets::for_router_v2`, so healthy
// requests keep today's model. Free tiers keep their single target; free
// Powerful still resolves to a paid model and is denied by the plan check.
const PAID_QUICK_ALTERNATES: &[&str] = &[GLM_5_3_FLASH_MODEL_ID];
const PAID_POWERFUL_ALTERNATES: &[&str] = &[KIMI_K3_MODEL_ID];
const FREE_ALTERNATES: &[&str] = &[];

/// Ordered candidate models for an Auto tier and plan: the alias target first,
/// then the tier's approved alternates. `None` for explicit selections.
pub(crate) fn auto_model_candidates(
    mode: ModelSelectionMode,
    plan: ModelPlan,
) -> Option<Vec<&'static str>> {
    let selector = mode.alias()?;
    let preferred = ModelAliasTargets::for_router_v2(plan).resolve(selector);
    let alternates = match (mode, plan) {
        (ModelSelectionMode::AutoQuick, ModelPlan::Paid) => PAID_QUICK_ALTERNATES,
        (ModelSelectionMode::AutoPowerful, ModelPlan::Paid) => PAID_POWERFUL_ALTERNATES,
        (_, ModelPlan::Free) => FREE_ALTERNATES,
        (ModelSelectionMode::Explicit, _) => return None,
    };
    let mut candidates = Vec::with_capacity(1 + alternates.len());
    candidates.push(preferred);
    candidates.extend(alternates.iter().copied().filter(|c| *c != preferred));
    Some(candidates)
}

/// Allowlisted reason for the chosen Auto model. Safe to log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutoModelReason {
    /// The tier's preferred model was eligible.
    Primary,
    /// The account's remembered alternate remained healthy and eligible.
    RetainedHealthyChoice,
    /// Every earlier candidate was unavailable or incompatible.
    HealthFallback,
}

impl AutoModelReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::RetainedHealthyChoice => "retained_healthy_choice",
            Self::HealthFallback => "health_fallback",
        }
    }
}

/// Allowlisted reason a candidate was skipped. Safe to log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutoCandidateRejection {
    PlanDenied,
    /// No Router v2 route or no catalog entry exists for the candidate.
    NotConfigured,
    Unavailable {
        retry_after: Duration,
    },
    IncompatibleVision,
    IncompatibleContext {
        required: usize,
        available: usize,
    },
    /// An earlier attempt of this same request could not build its context
    /// on the candidate, so it is excluded from the bounded second decision.
    ContextOverflow,
    IncompatibleToolHistory,
}

/// A candidate an earlier attempt of the same request already chose and must
/// not return to. The cause is carried so the bounded second decision reports
/// the right rejection and, for a lost route, the typed capacity result's own
/// recovery hint rather than a configuration error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExcludedAutoCandidate {
    pub(crate) model_id: String,
    pub(crate) rejection: AutoCandidateRejection,
}

impl ExcludedAutoCandidate {
    /// The candidate's context could not hold this request.
    pub(crate) fn context_overflow(model_id: impl Into<String>) -> Self {
        Self {
            model_id: model_id.into(),
            rejection: AutoCandidateRejection::ContextOverflow,
        }
    }

    /// Every route of the candidate became unavailable after the first
    /// decision's snapshot, at route preparation or at the first-send claim.
    /// `retry_after` is the capacity result's hint; an absent hint falls back
    /// to the minimum capacity cooldown.
    pub(crate) fn unavailable(model_id: impl Into<String>, retry_after: Option<Duration>) -> Self {
        Self {
            model_id: model_id.into(),
            rejection: AutoCandidateRejection::Unavailable {
                retry_after: retry_after.unwrap_or(MIN_CAPACITY_COOLDOWN),
            },
        }
    }
}

/// Route availability for one public model under the shared route and capacity gates,
/// taken from the same health snapshot as every other candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelAvailability {
    Available(ConfiguredProviders),
    Unavailable { retry_after: Duration },
    NotConfigured,
}

/// Tokens the request needs from a model. Only consulted when a candidate has
/// a smaller context window than the preferred model.
pub(crate) enum PromptTokenEstimate<'a> {
    Known(usize),
    /// `upper_bound` is a cheap bound that is never below the exact count (for
    /// byte-level BPE, text bytes bound tokens). `exact` runs at most once and
    /// only when the bound alone cannot admit the candidate.
    Bounded {
        upper_bound: usize,
        exact: &'a dyn Fn() -> usize,
    },
}

/// Request properties that an alternate must satisfy. The preferred model is
/// never rejected by these checks: it keeps today's behavior and error paths.
pub(crate) struct AutoModelRequirements<'a> {
    /// The main model receives raw images (Chat Completions). Responses
    /// describes images with a separate helper and sets this to false.
    pub(crate) vision: bool,
    /// False when the request carries assistant tool-call history whose ids
    /// are not in the form Kimi models require.
    pub(crate) kimi_tool_history_compatible: bool,
    pub(crate) prompt_tokens: PromptTokenEstimate<'a>,
}

pub(crate) struct AutoModelSelectionInput<'a> {
    pub(crate) mode: ModelSelectionMode,
    pub(crate) plan: ModelPlan,
    /// The account's remembered model for this selector, if any.
    pub(crate) sticky_model_id: Option<&'a str>,
    /// A candidate an earlier attempt of this request already chose and lost.
    pub(crate) excluded: Option<&'a ExcludedAutoCandidate>,
    pub(crate) requirements: AutoModelRequirements<'a>,
    pub(crate) availability: &'a dyn Fn(&str) -> ModelAvailability,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AutoModelDecision {
    pub(crate) selector: &'static str,
    pub(crate) preferred_model_id: &'static str,
    pub(crate) chosen_model_id: &'static str,
    pub(crate) reason: AutoModelReason,
    pub(crate) rejected: Vec<(&'static str, AutoCandidateRejection)>,
    pub(crate) policy_version: &'static str,
}

impl AutoModelDecision {
    pub(crate) fn changed_model(&self) -> bool {
        self.chosen_model_id != self.preferred_model_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AutoModelError {
    /// The selector is not an Auto alias; callers must not reach this stage.
    NotAuto,
    /// The plan does not include the tier's preferred model. Reported with
    /// the existing plan error so an alias never becomes an access grant.
    PreferredModelDenied,
    /// The tier's preferred model has no Router v2 route at all. This is a
    /// configuration error that alternates must not mask.
    PreferredModelNotConfigured,
    /// No candidate is eligible. `retry_after` is present only when at least
    /// one candidate was rejected for health; otherwise the tier is
    /// misconfigured or incompatible and waiting will not help.
    NoEligibleCandidate {
        selector: &'static str,
        preferred_model_id: &'static str,
        retry_after: Option<Duration>,
        rejected: Vec<(&'static str, AutoCandidateRejection)>,
    },
}

pub(crate) fn select_auto_model(
    input: AutoModelSelectionInput<'_>,
) -> Result<AutoModelDecision, AutoModelError> {
    let (Some(selector), Some(candidates)) = (
        input.mode.alias(),
        auto_model_candidates(input.mode, input.plan),
    ) else {
        return Err(AutoModelError::NotAuto);
    };
    let preferred = candidates[0];
    if !input.plan.allows_model(preferred) {
        return Err(AutoModelError::PreferredModelDenied);
    }
    if (input.availability)(preferred) == ModelAvailability::NotConfigured {
        return Err(AutoModelError::PreferredModelNotConfigured);
    }

    // Order encodes the reason: a remembered alternate keeps its warmed route
    // while healthy, then the preferred model, then the remaining alternates.
    let excluded_model = input.excluded.map(|excluded| excluded.model_id.as_str());
    let sticky = input
        .sticky_model_id
        .and_then(|sticky| candidates.iter().copied().find(|c| *c == sticky))
        .filter(|sticky| *sticky != preferred && Some(*sticky) != excluded_model);
    let ordered = sticky
        .map(|sticky| (sticky, AutoModelReason::RetainedHealthyChoice))
        .into_iter()
        .chain(std::iter::once((preferred, AutoModelReason::Primary)))
        .chain(
            candidates
                .iter()
                .copied()
                .filter(|c| *c != preferred && Some(*c) != sticky)
                .map(|c| (c, AutoModelReason::HealthFallback)),
        );

    let preferred_window = model_context_window(preferred);
    let exact_tokens = OnceCell::new();
    let mut rejected = Vec::new();
    let mut earliest_recovery: Option<Duration> = None;
    // A candidate excluded by an earlier attempt of this request is recorded
    // up front with its own cause, so the second decision explains why it
    // differs and a lost route keeps its capacity hint when nothing else
    // remains.
    if let Some(excluded) = input.excluded {
        if let Some(candidate) = candidates.iter().copied().find(|c| *c == excluded.model_id) {
            if let AutoCandidateRejection::Unavailable { retry_after } = excluded.rejection {
                earliest_recovery =
                    Some(earliest_recovery.map_or(retry_after, |current| current.min(retry_after)));
            }
            rejected.push((candidate, excluded.rejection));
        }
    }

    for (candidate, reason) in ordered {
        if Some(candidate) == excluded_model {
            continue;
        }
        match (input.availability)(candidate) {
            ModelAvailability::Available(_) => {}
            ModelAvailability::Unavailable { retry_after } => {
                earliest_recovery =
                    Some(earliest_recovery.map_or(retry_after, |current| current.min(retry_after)));
                rejected.push((
                    candidate,
                    AutoCandidateRejection::Unavailable { retry_after },
                ));
                continue;
            }
            ModelAvailability::NotConfigured => {
                rejected.push((candidate, AutoCandidateRejection::NotConfigured));
                continue;
            }
        }
        if candidate != preferred {
            if !input.plan.allows_model(candidate) {
                rejected.push((candidate, AutoCandidateRejection::PlanDenied));
                continue;
            }
            if let Some(rejection) = incompatible_alternate(
                candidate,
                preferred_window,
                &input.requirements,
                &exact_tokens,
            ) {
                rejected.push((candidate, rejection));
                continue;
            }
        }
        return Ok(AutoModelDecision {
            selector,
            preferred_model_id: preferred,
            chosen_model_id: candidate,
            reason,
            rejected,
            policy_version: AUTO_MODEL_POLICY_VERSION,
        });
    }

    Err(AutoModelError::NoEligibleCandidate {
        selector,
        preferred_model_id: preferred,
        retry_after: earliest_recovery,
        rejected,
    })
}

fn incompatible_alternate(
    candidate: &'static str,
    preferred_window: usize,
    requirements: &AutoModelRequirements<'_>,
    exact_tokens: &OnceCell<usize>,
) -> Option<AutoCandidateRejection> {
    // A candidate without a catalog entry has no verified capabilities.
    let Some(capabilities) = model_capabilities(candidate) else {
        return Some(AutoCandidateRejection::NotConfigured);
    };
    if requirements.vision && !capabilities.vision {
        return Some(AutoCandidateRejection::IncompatibleVision);
    }
    if model_uses_kimi_tool_call_ids(candidate) && !requirements.kimi_tool_history_compatible {
        return Some(AutoCandidateRejection::IncompatibleToolHistory);
    }

    // A window at least as large as the preferred model's fits whatever the
    // preferred model would accept; the existing context checks still apply.
    let candidate_window = model_context_window(candidate);
    if candidate_window < preferred_window {
        let required = match requirements.prompt_tokens {
            PromptTokenEstimate::Known(tokens) => tokens,
            PromptTokenEstimate::Bounded { upper_bound, exact } => {
                if upper_bound < candidate_window {
                    return None;
                }
                *exact_tokens.get_or_init(exact)
            }
        };
        if required >= candidate_window {
            return Some(AutoCandidateRejection::IncompatibleContext {
                required,
                available: candidate_window,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_config::{
        enabled_api_completion_model_ids, AUTO_POWERFUL_MODEL_ID, AUTO_QUICK_MODEL_ID,
        DEEPSEEK_V4_1_FLASH_MODEL_ID, GLM_5_3_MODEL_ID, QUICK_MODEL_ID,
    };
    use crate::provider_registry::{ProviderId, PROVIDER_REGISTRY};
    use std::collections::HashMap;

    fn requirements() -> AutoModelRequirements<'static> {
        AutoModelRequirements {
            vision: false,
            kimi_tool_history_compatible: true,
            prompt_tokens: PromptTokenEstimate::Known(1_000),
        }
    }

    fn available() -> ModelAvailability {
        ModelAvailability::Available(ConfiguredProviders::none().with_provider(ProviderId::Tinfoil))
    }

    fn unavailable(seconds: u64) -> ModelAvailability {
        ModelAvailability::Unavailable {
            retry_after: Duration::from_secs(seconds),
        }
    }

    fn availability(
        entries: &[(&'static str, ModelAvailability)],
    ) -> HashMap<&'static str, ModelAvailability> {
        entries.iter().copied().collect()
    }

    fn select(
        mode: ModelSelectionMode,
        plan: ModelPlan,
        sticky: Option<&str>,
        requirements: AutoModelRequirements<'_>,
        table: &HashMap<&'static str, ModelAvailability>,
    ) -> Result<AutoModelDecision, AutoModelError> {
        select_excluding(mode, plan, sticky, None, requirements, table)
    }

    fn select_excluding(
        mode: ModelSelectionMode,
        plan: ModelPlan,
        sticky: Option<&str>,
        excluded: Option<&ExcludedAutoCandidate>,
        requirements: AutoModelRequirements<'_>,
        table: &HashMap<&'static str, ModelAvailability>,
    ) -> Result<AutoModelDecision, AutoModelError> {
        let lookup = |model: &str| {
            table
                .get(model)
                .copied()
                .unwrap_or(ModelAvailability::NotConfigured)
        };
        select_auto_model(AutoModelSelectionInput {
            mode,
            plan,
            sticky_model_id: sticky,
            excluded,
            requirements,
            availability: &lookup,
        })
    }

    #[test]
    fn candidate_tables_start_with_the_router_v2_alias_target_and_stay_in_plan() {
        for plan in [ModelPlan::Free, ModelPlan::Paid] {
            let targets = ModelAliasTargets::for_router_v2(plan);
            for (mode, selector) in [
                (ModelSelectionMode::AutoQuick, AUTO_QUICK_MODEL_ID),
                (ModelSelectionMode::AutoPowerful, AUTO_POWERFUL_MODEL_ID),
            ] {
                let candidates = auto_model_candidates(mode, plan).expect("auto tier");
                assert_eq!(
                    candidates[0],
                    targets.resolve(selector),
                    "{plan:?} {selector}"
                );
                let unique = candidates.iter().collect::<std::collections::BTreeSet<_>>();
                assert_eq!(unique.len(), candidates.len(), "duplicate candidate");
                for candidate in &candidates {
                    assert!(
                        enabled_api_completion_model_ids().any(|id| id == *candidate),
                        "{candidate} is not an enabled API completion model"
                    );
                    assert!(
                        PROVIDER_REGISTRY.completion_model(candidate).is_some(),
                        "{candidate} has no Router v2 routes"
                    );
                    assert!(
                        model_capabilities(candidate)
                            .expect("catalog entry")
                            .tool_use,
                        "{candidate} must support tool use"
                    );
                    // Alternates never widen access beyond the alias target.
                    assert_eq!(
                        plan.allows_model(candidate),
                        plan.allows_model(candidates[0]),
                        "{plan:?} {candidate}"
                    );
                }
            }
            assert_eq!(
                auto_model_candidates(ModelSelectionMode::Explicit, plan),
                None
            );
        }
        assert_eq!(
            auto_model_candidates(ModelSelectionMode::AutoQuick, ModelPlan::Paid),
            Some(vec![DEEPSEEK_V4_1_FLASH_MODEL_ID, GLM_5_3_FLASH_MODEL_ID])
        );
        assert_eq!(
            auto_model_candidates(ModelSelectionMode::AutoPowerful, ModelPlan::Paid),
            Some(vec![GLM_5_3_MODEL_ID, KIMI_K3_MODEL_ID])
        );
        assert_eq!(
            auto_model_candidates(ModelSelectionMode::AutoQuick, ModelPlan::Free),
            Some(vec![QUICK_MODEL_ID])
        );
        assert_eq!(
            auto_model_candidates(ModelSelectionMode::AutoPowerful, ModelPlan::Free),
            Some(vec![GLM_5_3_MODEL_ID])
        );
        assert_eq!(AUTO_MODEL_POLICY_VERSION, "auto-model-v1");
    }

    #[test]
    fn healthy_preferred_model_wins_without_consulting_alternates() {
        let table = availability(&[
            (DEEPSEEK_V4_1_FLASH_MODEL_ID, available()),
            (GLM_5_3_FLASH_MODEL_ID, unavailable(60)),
        ]);
        let decision = select(
            ModelSelectionMode::AutoQuick,
            ModelPlan::Paid,
            None,
            requirements(),
            &table,
        )
        .expect("primary");

        assert_eq!(decision.selector, AUTO_QUICK_MODEL_ID);
        assert_eq!(decision.preferred_model_id, DEEPSEEK_V4_1_FLASH_MODEL_ID);
        assert_eq!(decision.chosen_model_id, DEEPSEEK_V4_1_FLASH_MODEL_ID);
        assert_eq!(decision.reason, AutoModelReason::Primary);
        assert!(!decision.changed_model());
        assert!(decision.rejected.is_empty());
        assert_eq!(decision.policy_version, AUTO_MODEL_POLICY_VERSION);
    }

    #[test]
    fn unavailable_preferred_model_falls_back_to_the_approved_alternate() {
        let table = availability(&[
            (DEEPSEEK_V4_1_FLASH_MODEL_ID, unavailable(45)),
            (GLM_5_3_FLASH_MODEL_ID, available()),
        ]);
        let decision = select(
            ModelSelectionMode::AutoQuick,
            ModelPlan::Paid,
            None,
            requirements(),
            &table,
        )
        .expect("fallback");

        assert_eq!(decision.chosen_model_id, GLM_5_3_FLASH_MODEL_ID);
        assert_eq!(decision.reason, AutoModelReason::HealthFallback);
        assert!(decision.changed_model());
        assert_eq!(
            decision.rejected,
            vec![(
                DEEPSEEK_V4_1_FLASH_MODEL_ID,
                AutoCandidateRejection::Unavailable {
                    retry_after: Duration::from_secs(45)
                }
            )]
        );

        let powerful = availability(&[
            (GLM_5_3_MODEL_ID, unavailable(30)),
            (KIMI_K3_MODEL_ID, available()),
        ]);
        let decision = select(
            ModelSelectionMode::AutoPowerful,
            ModelPlan::Paid,
            None,
            requirements(),
            &powerful,
        )
        .expect("powerful fallback");
        assert_eq!(decision.chosen_model_id, KIMI_K3_MODEL_ID);
        assert_eq!(decision.reason, AutoModelReason::HealthFallback);
    }

    #[test]
    fn remembered_alternate_is_retained_while_healthy_and_ignored_otherwise() {
        let both_available = availability(&[
            (DEEPSEEK_V4_1_FLASH_MODEL_ID, available()),
            (GLM_5_3_FLASH_MODEL_ID, available()),
        ]);
        let decision = select(
            ModelSelectionMode::AutoQuick,
            ModelPlan::Paid,
            Some(GLM_5_3_FLASH_MODEL_ID),
            requirements(),
            &both_available,
        )
        .expect("retained");
        assert_eq!(decision.chosen_model_id, GLM_5_3_FLASH_MODEL_ID);
        assert_eq!(decision.reason, AutoModelReason::RetainedHealthyChoice);
        assert!(decision.rejected.is_empty());

        let alternate_open = availability(&[
            (DEEPSEEK_V4_1_FLASH_MODEL_ID, available()),
            (GLM_5_3_FLASH_MODEL_ID, unavailable(20)),
        ]);
        let decision = select(
            ModelSelectionMode::AutoQuick,
            ModelPlan::Paid,
            Some(GLM_5_3_FLASH_MODEL_ID),
            requirements(),
            &alternate_open,
        )
        .expect("primary after sticky opened");
        assert_eq!(decision.chosen_model_id, DEEPSEEK_V4_1_FLASH_MODEL_ID);
        assert_eq!(decision.reason, AutoModelReason::Primary);
        assert_eq!(decision.rejected.len(), 1);

        // A remembered model outside the tier or equal to the preferred model
        // changes nothing.
        for sticky in [
            Some(KIMI_K3_MODEL_ID),
            Some(DEEPSEEK_V4_1_FLASH_MODEL_ID),
            Some("unknown"),
        ] {
            let decision = select(
                ModelSelectionMode::AutoQuick,
                ModelPlan::Paid,
                sticky,
                requirements(),
                &both_available,
            )
            .expect("primary");
            assert_eq!(decision.chosen_model_id, DEEPSEEK_V4_1_FLASH_MODEL_ID);
            assert_eq!(decision.reason, AutoModelReason::Primary);
        }
    }

    #[test]
    fn alternates_are_skipped_when_the_request_needs_more_context_than_they_offer() {
        let table = availability(&[
            (DEEPSEEK_V4_1_FLASH_MODEL_ID, unavailable(30)),
            (GLM_5_3_FLASH_MODEL_ID, available()),
        ]);
        let flash_window = model_context_window(GLM_5_3_FLASH_MODEL_ID);
        assert!(flash_window < model_context_window(DEEPSEEK_V4_1_FLASH_MODEL_ID));

        let fits = select(
            ModelSelectionMode::AutoQuick,
            ModelPlan::Paid,
            None,
            AutoModelRequirements {
                prompt_tokens: PromptTokenEstimate::Known(flash_window - 1),
                ..requirements()
            },
            &table,
        )
        .expect("fits the smaller window");
        assert_eq!(fits.chosen_model_id, GLM_5_3_FLASH_MODEL_ID);

        let error = select(
            ModelSelectionMode::AutoQuick,
            ModelPlan::Paid,
            None,
            AutoModelRequirements {
                prompt_tokens: PromptTokenEstimate::Known(flash_window),
                ..requirements()
            },
            &table,
        )
        .expect_err("too large for the alternate");
        assert_eq!(
            error,
            AutoModelError::NoEligibleCandidate {
                selector: AUTO_QUICK_MODEL_ID,
                preferred_model_id: DEEPSEEK_V4_1_FLASH_MODEL_ID,
                retry_after: Some(Duration::from_secs(30)),
                rejected: vec![
                    (
                        DEEPSEEK_V4_1_FLASH_MODEL_ID,
                        AutoCandidateRejection::Unavailable {
                            retry_after: Duration::from_secs(30)
                        }
                    ),
                    (
                        GLM_5_3_FLASH_MODEL_ID,
                        AutoCandidateRejection::IncompatibleContext {
                            required: flash_window,
                            available: flash_window,
                        }
                    ),
                ],
            }
        );
    }

    #[test]
    fn bounded_estimates_tokenize_only_when_the_cheap_bound_cannot_admit_a_candidate() {
        let flash_window = model_context_window(GLM_5_3_FLASH_MODEL_ID);
        let calls = std::cell::Cell::new(0usize);
        let exact = || {
            calls.set(calls.get() + 1);
            flash_window - 1
        };
        let quick_table = availability(&[
            (DEEPSEEK_V4_1_FLASH_MODEL_ID, unavailable(30)),
            (GLM_5_3_FLASH_MODEL_ID, available()),
        ]);

        // A byte bound below the window admits the alternate without counting.
        let decision = select(
            ModelSelectionMode::AutoQuick,
            ModelPlan::Paid,
            None,
            AutoModelRequirements {
                vision: false,
                kimi_tool_history_compatible: true,
                prompt_tokens: PromptTokenEstimate::Bounded {
                    upper_bound: flash_window - 1,
                    exact: &exact,
                },
            },
            &quick_table,
        )
        .expect("admitted by bound");
        assert_eq!(decision.chosen_model_id, GLM_5_3_FLASH_MODEL_ID);
        assert_eq!(calls.get(), 0);

        // A bound at or above the window requires the exact count, once.
        let decision = select(
            ModelSelectionMode::AutoQuick,
            ModelPlan::Paid,
            Some(GLM_5_3_FLASH_MODEL_ID),
            AutoModelRequirements {
                vision: false,
                kimi_tool_history_compatible: true,
                prompt_tokens: PromptTokenEstimate::Bounded {
                    upper_bound: flash_window * 3,
                    exact: &exact,
                },
            },
            &quick_table,
        )
        .expect("admitted by exact count");
        assert_eq!(decision.chosen_model_id, GLM_5_3_FLASH_MODEL_ID);
        assert_eq!(calls.get(), 1);

        // Kimi's window equals GLM 5.3's, so Powerful never counts tokens, and
        // an unavailable candidate is rejected before any compatibility work.
        calls.set(0);
        let lookup_available = |_: &str| available();
        select_auto_model(AutoModelSelectionInput {
            mode: ModelSelectionMode::AutoPowerful,
            plan: ModelPlan::Paid,
            sticky_model_id: Some(KIMI_K3_MODEL_ID),
            excluded: None,
            requirements: AutoModelRequirements {
                vision: false,
                kimi_tool_history_compatible: true,
                prompt_tokens: PromptTokenEstimate::Bounded {
                    upper_bound: usize::MAX,
                    exact: &exact,
                },
            },
            availability: &lookup_available,
        })
        .expect("powerful");
        let flash_open = availability(&[
            (DEEPSEEK_V4_1_FLASH_MODEL_ID, available()),
            (GLM_5_3_FLASH_MODEL_ID, unavailable(10)),
        ]);
        select(
            ModelSelectionMode::AutoQuick,
            ModelPlan::Paid,
            Some(GLM_5_3_FLASH_MODEL_ID),
            AutoModelRequirements {
                vision: false,
                kimi_tool_history_compatible: true,
                prompt_tokens: PromptTokenEstimate::Bounded {
                    upper_bound: usize::MAX,
                    exact: &exact,
                },
            },
            &flash_open,
        )
        .expect("primary");
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn kimi_is_skipped_for_incompatible_tool_history_but_vision_is_never_lost() {
        let table = availability(&[
            (GLM_5_3_MODEL_ID, unavailable(30)),
            (KIMI_K3_MODEL_ID, available()),
        ]);
        let error = select(
            ModelSelectionMode::AutoPowerful,
            ModelPlan::Paid,
            None,
            AutoModelRequirements {
                kimi_tool_history_compatible: false,
                ..requirements()
            },
            &table,
        )
        .expect_err("kimi cannot take foreign tool-call ids");
        assert!(matches!(
            error,
            AutoModelError::NoEligibleCandidate { ref rejected, retry_after: Some(_), .. }
                if rejected.contains(&(KIMI_K3_MODEL_ID, AutoCandidateRejection::IncompatibleToolHistory))
        ));

        // GLM 5.3 has no vision; Kimi does, so images never make the
        // alternate less capable than the preferred model.
        let decision = select(
            ModelSelectionMode::AutoPowerful,
            ModelPlan::Paid,
            None,
            AutoModelRequirements {
                vision: true,
                ..requirements()
            },
            &table,
        )
        .expect("kimi accepts images");
        assert_eq!(decision.chosen_model_id, KIMI_K3_MODEL_ID);
        assert!(model_capabilities(KIMI_K3_MODEL_ID).expect("kimi").vision);
        assert!(!model_capabilities(GLM_5_3_MODEL_ID).expect("glm").vision);
        assert!(model_uses_kimi_tool_call_ids(KIMI_K3_MODEL_ID));
        assert!(!model_uses_kimi_tool_call_ids(GLM_5_3_FLASH_MODEL_ID));
    }

    #[test]
    fn free_callers_keep_their_single_target_and_paid_gates() {
        let table = availability(&[
            (QUICK_MODEL_ID, unavailable(30)),
            (DEEPSEEK_V4_1_FLASH_MODEL_ID, available()),
            (GLM_5_3_FLASH_MODEL_ID, available()),
            (GLM_5_3_MODEL_ID, available()),
        ]);
        let error = select(
            ModelSelectionMode::AutoQuick,
            ModelPlan::Free,
            Some(GLM_5_3_FLASH_MODEL_ID),
            requirements(),
            &table,
        )
        .expect_err("free quick has no paid alternate");
        assert!(matches!(
            error,
            AutoModelError::NoEligibleCandidate { preferred_model_id, ref rejected, .. }
                if preferred_model_id == QUICK_MODEL_ID && rejected.len() == 1
        ));

        assert_eq!(
            select(
                ModelSelectionMode::AutoPowerful,
                ModelPlan::Free,
                None,
                requirements(),
                &table,
            ),
            Err(AutoModelError::PreferredModelDenied)
        );
    }

    #[test]
    fn no_eligible_candidate_reports_recovery_only_for_health_rejections() {
        let table = availability(&[
            (DEEPSEEK_V4_1_FLASH_MODEL_ID, unavailable(90)),
            (GLM_5_3_FLASH_MODEL_ID, unavailable(40)),
        ]);
        let error = select(
            ModelSelectionMode::AutoQuick,
            ModelPlan::Paid,
            None,
            requirements(),
            &table,
        )
        .expect_err("all open");
        assert!(matches!(
            error,
            AutoModelError::NoEligibleCandidate { retry_after: Some(retry_after), .. }
                if retry_after == Duration::from_secs(40)
        ));

        // An unconfigured preferred model is a configuration error that no
        // alternate may mask; an unconfigured alternate is simply skipped.
        let unconfigured = availability(&[]);
        assert_eq!(
            select(
                ModelSelectionMode::AutoQuick,
                ModelPlan::Paid,
                None,
                requirements(),
                &unconfigured,
            ),
            Err(AutoModelError::PreferredModelNotConfigured)
        );
        let alternate_missing = availability(&[(DEEPSEEK_V4_1_FLASH_MODEL_ID, unavailable(30))]);
        let error = select(
            ModelSelectionMode::AutoQuick,
            ModelPlan::Paid,
            None,
            requirements(),
            &alternate_missing,
        )
        .expect_err("alternate not configured");
        assert!(matches!(
            error,
            AutoModelError::NoEligibleCandidate { retry_after: Some(_), ref rejected, .. }
                if rejected.contains(&(GLM_5_3_FLASH_MODEL_ID, AutoCandidateRejection::NotConfigured))
        ));
    }

    #[test]
    fn an_alternate_that_overflowed_is_excluded_from_the_bounded_second_decision() {
        let both_available = availability(&[
            (DEEPSEEK_V4_1_FLASH_MODEL_ID, available()),
            (GLM_5_3_FLASH_MODEL_ID, available()),
        ]);
        // The remembered alternate wins the first decision...
        let first = select(
            ModelSelectionMode::AutoQuick,
            ModelPlan::Paid,
            Some(GLM_5_3_FLASH_MODEL_ID),
            requirements(),
            &both_available,
        )
        .expect("retained");
        assert_eq!(first.chosen_model_id, GLM_5_3_FLASH_MODEL_ID);

        // ...and the healthy preferred model wins once it is excluded, even
        // though the memory still names it.
        let overflowed = ExcludedAutoCandidate::context_overflow(GLM_5_3_FLASH_MODEL_ID);
        let second = select_excluding(
            ModelSelectionMode::AutoQuick,
            ModelPlan::Paid,
            Some(GLM_5_3_FLASH_MODEL_ID),
            Some(&overflowed),
            requirements(),
            &both_available,
        )
        .expect("preferred after overflow");
        assert_eq!(second.chosen_model_id, DEEPSEEK_V4_1_FLASH_MODEL_ID);
        assert_eq!(second.reason, AutoModelReason::Primary);
        assert_eq!(
            second.rejected,
            vec![(
                GLM_5_3_FLASH_MODEL_ID,
                AutoCandidateRejection::ContextOverflow
            )]
        );

        // With the preferred model down, the exclusion leaves a health
        // condition with the preferred model's recovery hint.
        let preferred_open = availability(&[
            (DEEPSEEK_V4_1_FLASH_MODEL_ID, unavailable(25)),
            (GLM_5_3_FLASH_MODEL_ID, available()),
        ]);
        let error = select_excluding(
            ModelSelectionMode::AutoQuick,
            ModelPlan::Paid,
            None,
            Some(&overflowed),
            requirements(),
            &preferred_open,
        )
        .expect_err("nothing else eligible");
        assert!(matches!(
            error,
            AutoModelError::NoEligibleCandidate { retry_after: Some(retry_after), ref rejected, .. }
                if retry_after == Duration::from_secs(25)
                    && rejected.contains(&(GLM_5_3_FLASH_MODEL_ID, AutoCandidateRejection::ContextOverflow))
        ));
    }

    #[test]
    fn a_candidate_that_lost_its_routes_keeps_the_capacity_result_and_its_hint() {
        // Free Quick has a single candidate. The first decision chose it and
        // its route then closed before the first send; the snapshot may not
        // have caught up yet. The bounded second decision must report the
        // capacity condition with the hint the request actually received,
        // not a configuration error.
        let free = availability(&[(QUICK_MODEL_ID, available())]);
        let lost =
            ExcludedAutoCandidate::unavailable(QUICK_MODEL_ID, Some(Duration::from_secs(20)));
        assert_eq!(
            select_excluding(
                ModelSelectionMode::AutoQuick,
                ModelPlan::Free,
                None,
                Some(&lost),
                requirements(),
                &free,
            ),
            Err(AutoModelError::NoEligibleCandidate {
                selector: AUTO_QUICK_MODEL_ID,
                preferred_model_id: QUICK_MODEL_ID,
                retry_after: Some(Duration::from_secs(20)),
                rejected: vec![(
                    QUICK_MODEL_ID,
                    AutoCandidateRejection::Unavailable {
                        retry_after: Duration::from_secs(20),
                    },
                )],
            })
        );

        // An absent hint falls back to the minimum cooldown.
        let lost_without_hint = ExcludedAutoCandidate::unavailable(QUICK_MODEL_ID, None);
        assert!(matches!(
            select_excluding(
                ModelSelectionMode::AutoQuick,
                ModelPlan::Free,
                None,
                Some(&lost_without_hint),
                requirements(),
                &free,
            ),
            Err(AutoModelError::NoEligibleCandidate { retry_after: Some(retry_after), .. })
                if retry_after == MIN_CAPACITY_COOLDOWN
        ));

        // Paid Quick whose alternate cannot hold the request: the preferred
        // model's capacity result survives with both causes recorded.
        let paid = availability(&[
            (DEEPSEEK_V4_1_FLASH_MODEL_ID, available()),
            (GLM_5_3_FLASH_MODEL_ID, available()),
        ]);
        let flash_window = model_context_window(GLM_5_3_FLASH_MODEL_ID);
        let lost = ExcludedAutoCandidate::unavailable(
            DEEPSEEK_V4_1_FLASH_MODEL_ID,
            Some(Duration::from_secs(35)),
        );
        assert_eq!(
            select_excluding(
                ModelSelectionMode::AutoQuick,
                ModelPlan::Paid,
                None,
                Some(&lost),
                AutoModelRequirements {
                    prompt_tokens: PromptTokenEstimate::Known(flash_window),
                    ..requirements()
                },
                &paid,
            ),
            Err(AutoModelError::NoEligibleCandidate {
                selector: AUTO_QUICK_MODEL_ID,
                preferred_model_id: DEEPSEEK_V4_1_FLASH_MODEL_ID,
                retry_after: Some(Duration::from_secs(35)),
                rejected: vec![
                    (
                        DEEPSEEK_V4_1_FLASH_MODEL_ID,
                        AutoCandidateRejection::Unavailable {
                            retry_after: Duration::from_secs(35),
                        },
                    ),
                    (
                        GLM_5_3_FLASH_MODEL_ID,
                        AutoCandidateRejection::IncompatibleContext {
                            required: flash_window,
                            available: flash_window,
                        },
                    ),
                ],
            })
        );

        // With a compatible alternate the second decision simply moves on.
        let moved = select_excluding(
            ModelSelectionMode::AutoQuick,
            ModelPlan::Paid,
            None,
            Some(&lost),
            requirements(),
            &paid,
        )
        .expect("alternate takes the request");
        assert_eq!(moved.chosen_model_id, GLM_5_3_FLASH_MODEL_ID);
        assert_eq!(moved.reason, AutoModelReason::HealthFallback);
        assert_eq!(
            moved.rejected,
            vec![(
                DEEPSEEK_V4_1_FLASH_MODEL_ID,
                AutoCandidateRejection::Unavailable {
                    retry_after: Duration::from_secs(35),
                },
            )]
        );
    }

    #[test]
    fn explicit_selectors_never_enter_the_policy() {
        let lookup = |_: &str| available();
        assert_eq!(
            select_auto_model(AutoModelSelectionInput {
                mode: ModelSelectionMode::Explicit,
                plan: ModelPlan::Paid,
                sticky_model_id: None,
                excluded: None,
                requirements: requirements(),
                availability: &lookup,
            }),
            Err(AutoModelError::NotAuto)
        );
    }
}
