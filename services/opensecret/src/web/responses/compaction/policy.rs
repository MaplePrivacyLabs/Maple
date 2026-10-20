//! Pure compaction budgets. These budgets never change a caller's output cap.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionPolicy {
    /// Hard main input budget, after only the caller's requested output.
    pub input_budget: usize,
    /// Advisory trigger; provider rejection remains an independent signal.
    pub trigger_tokens: usize,
    /// Target verbatim suffix. Atomic exchanges may make it larger.
    pub keep_recent_tokens: usize,
    /// Independent helper input/output budgets, not the main response allowance.
    pub summary_input_budget: usize,
    pub summary_output_tokens: usize,
}

impl CompactionPolicy {
    pub fn from_context_window(
        context_window: usize,
        requested_output_tokens: usize,
        turn_growth_tokens: usize,
    ) -> Self {
        let input_budget = context_window.saturating_sub(requested_output_tokens);
        let trigger_budget = input_budget.saturating_sub(turn_growth_tokens);
        let summary_output_tokens = 4_096.min(context_window / 8);
        Self {
            input_budget,
            trigger_tokens: trigger_budget.saturating_mul(4) / 5,
            keep_recent_tokens: 20_000.min(input_budget / 3),
            summary_input_budget: context_window
                .saturating_sub(summary_output_tokens)
                .saturating_sub(2_048.min(context_window / 8)),
            summary_output_tokens,
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
        assert_eq!(policy.summary_output_tokens, 4_096);
    }

    #[test]
    fn small_windows_reduce_recent_target_and_saturate_safely() {
        let policy = CompactionPolicy::from_context_window(20_000, 4_000, 2_000);
        assert_eq!(policy.input_budget, 16_000);
        assert_eq!(policy.keep_recent_tokens, 5_333);
        assert_eq!(policy.trigger_tokens, 11_200);
        assert!(policy.summary_input_budget < 20_000);
        let impossible = CompactionPolicy::from_context_window(20_000, 30_000, 2_000);
        assert_eq!(impossible.input_budget, 0);
        assert_eq!(impossible.trigger_tokens, 0);
    }
}
