use pi_agent_core::types::ThinkingLevel;
pub const DEFAULT_THINKING_LEVEL: ThinkingLevel = ThinkingLevel::Medium;
pub const THINKING_LEVEL_OPTIONS: &[ThinkingLevel] = &[
    ThinkingLevel::Off,
    ThinkingLevel::Minimal,
    ThinkingLevel::Low,
    ThinkingLevel::Medium,
    ThinkingLevel::High,
    ThinkingLevel::Xhigh,
    ThinkingLevel::Max,
];
