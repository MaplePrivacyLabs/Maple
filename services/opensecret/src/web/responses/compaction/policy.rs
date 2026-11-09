//! Pure compaction budgets. These budgets never change a caller's output cap.

/// Room a helper needs to generate its checkpoint, matching Pi's reserve. The
/// summary has no output cap of its own, so this is what keeps a completed
/// answer possible once reasoning has also spent from the same window.
const MIN_SUMMARY_GENERATION_TOKENS: usize = 16_384;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionPolicy {
    /// Hard main input budget, after only the caller's requested output.
    pub input_budget: usize,
    /// Advisory trigger; provider rejection remains an independent signal.
    pub trigger_tokens: usize,
    /// Target verbatim suffix. Atomic exchanges may make it larger.
    pub keep_recent_tokens: usize,
    /// Bound one helper request, instructions and previous summary included, to
    /// what the model can read while still generating. This is independent of
    /// the main response allowance: summary output keeps its native allowance.
    pub summary_input_budget: usize,
}

impl CompactionPolicy {
    pub fn from_context_window(
        context_window: usize,
        requested_output_tokens: usize,
        turn_growth_tokens: usize,
    ) -> Self {
        let input_budget = context_window.saturating_sub(requested_output_tokens);
        let trigger_budget = input_budget.saturating_sub(turn_growth_tokens);
        Self {
            input_budget,
            trigger_tokens: trigger_budget.saturating_mul(4) / 5,
            keep_recent_tokens: 20_000.min(input_budget / 3),
            // A proportional share scales with the output a large window allows.
            // This is an estimate, not an exact model-tokenizer fit guarantee.
            summary_input_budget: context_window.saturating_sub(
                MIN_SUMMARY_GENERATION_TOKENS
                    .max(context_window / 8)
                    .min(context_window / 2),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_reserves_requested_output_without_capping_it() {
        let policy = CompactionPolicy::from_context_window(100_000, 17_123, 8_000);
        assert_eq!(policy.input_budget, 82_877);
        assert_eq!(policy.trigger_tokens, 59_901);
        assert_eq!(policy.keep_recent_tokens, 20_000);
        assert_eq!(policy.summary_input_budget, 83_616);
    }

    #[test]
    fn small_windows_reduce_recent_target_and_saturate_safely() {
        let policy = CompactionPolicy::from_context_window(20_000, 4_000, 2_000);
        assert_eq!(policy.input_budget, 16_000);
        assert_eq!(policy.keep_recent_tokens, 5_333);
        assert_eq!(policy.trigger_tokens, 11_200);
        assert_eq!(policy.summary_input_budget, 10_000);
        let impossible = CompactionPolicy::from_context_window(20_000, 30_000, 2_000);
        assert_eq!(impossible.input_budget, 0);
        assert_eq!(impossible.trigger_tokens, 0);
    }

    #[test]
    fn helper_input_scales_with_the_model_window_and_leaves_room_to_generate() {
        for (window, input) in [(131_072, 114_688), (262_144, 229_376), (1_048_576, 917_504)] {
            let policy = CompactionPolicy::from_context_window(window, 0, 2_048);
            assert_eq!(policy.summary_input_budget, input);
            assert_eq!(policy.input_budget, window);
        }
    }

    #[test]
    fn helper_input_is_independent_of_the_caller_output_reservation() {
        for output in [0, 8_192, 64_000] {
            let policy = CompactionPolicy::from_context_window(262_144, output, 2_048);
            assert_eq!(policy.summary_input_budget, 229_376);
        }
    }
}
