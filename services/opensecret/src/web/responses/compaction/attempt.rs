//! Response-local attempt identities and source-race decisions.

use crate::model_config::ReasoningReplay;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct AttemptKey {
    head: Option<Uuid>,
    coverage_fingerprint: [u8; 32],
    model: String,
    replay: u8,
}

impl AttemptKey {
    pub(super) fn new(
        head: Option<Uuid>,
        coverage_fingerprint: [u8; 32],
        model: &str,
        replay: ReasoningReplay,
    ) -> Self {
        Self {
            head,
            coverage_fingerprint,
            model: model.to_owned(),
            replay: replay_tag(replay),
        }
    }
}

fn replay_tag(replay: ReasoningReplay) -> u8 {
    match replay {
        ReasoningReplay::None => 0,
        ReasoningReplay::CurrentTurn => 1,
        ReasoningReplay::All => 2,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SourceRaceAction {
    Replan,
    FreshFallback,
    Fail,
}

/// A source mismatch never authorizes replaying the old snapshot. A soft-band
/// request gets one fresh, complete validation attempt after bounded replans;
/// mandatory compaction and a second inconsistent read still fail closed.
pub(super) fn source_race_action(
    races: &mut usize,
    max_retries: usize,
    forced: bool,
    fallback_only: bool,
) -> SourceRaceAction {
    *races = races.saturating_add(1);
    if *races <= max_retries {
        SourceRaceAction::Replan
    } else if !forced && !fallback_only {
        SourceRaceAction::FreshFallback
    } else {
        SourceRaceAction::Fail
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attempt_identity_changes_with_source_head_model_and_replay_policy() {
        let key = AttemptKey::new(None, [1; 32], "model", ReasoningReplay::None);
        assert_eq!(
            key,
            AttemptKey::new(None, [1; 32], "model", ReasoningReplay::None)
        );
        for other in [
            AttemptKey::new(
                Some(Uuid::from_u128(1)),
                [1; 32],
                "model",
                ReasoningReplay::None,
            ),
            AttemptKey::new(None, [2; 32], "model", ReasoningReplay::None),
            AttemptKey::new(None, [1; 32], "other", ReasoningReplay::None),
            AttemptKey::new(None, [1; 32], "model", ReasoningReplay::All),
        ] {
            assert_ne!(key, other);
        }
    }

    #[test]
    fn source_races_allow_only_one_fresh_soft_band_fallback() {
        let mut races = 0;
        assert_eq!(
            source_race_action(&mut races, 2, false, false),
            SourceRaceAction::Replan
        );
        assert_eq!(
            source_race_action(&mut races, 2, false, false),
            SourceRaceAction::Replan
        );
        assert_eq!(
            source_race_action(&mut races, 2, false, false),
            SourceRaceAction::FreshFallback
        );
        assert_eq!(
            source_race_action(&mut races, 2, false, true),
            SourceRaceAction::Fail
        );
        let mut forced_races = 2;
        assert_eq!(
            source_race_action(&mut forced_races, 2, true, false),
            SourceRaceAction::Fail
        );
    }
}
