//! Port of `packages/ai/src/utils/overflow.ts`.

use crate::types::{AssistantMessage, StopReason};
use regex::Regex;
use std::sync::OnceLock;

const OVERFLOW_PATTERNS: &[&str] = &[
    r"prompt (?:is )?too long",
    r"prompt exceeds max length",
    r"request_too_large",
    r"input is too long for requested model",
    r"exceeds the context window",
    r"exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\d,]+ tokens?|\s*\([\d,]+\))",
    r"input token count.*exceeds the maximum",
    r"maximum prompt length is \d+",
    r"reduce the length of the messages",
    r"maximum context length is \d+ tokens",
    r"exceeds (?:the )?maximum allowed input length of [\d,]+ tokens?",
    r"input \(\d+ tokens\) is longer than the model'?s context length \(\d+ tokens\)",
    r"exceeds the limit of \d+",
    r"exceeds the available context size",
    r"greater than the context length",
    r"context window exceeds limit",
    r"exceeded model token limit",
    r"too large for model with \d+ maximum context length",
    r"prompt has [\d,]+ tokens?, but the configured context size is [\d,]+ tokens?",
    r"model_context_window_exceeded",
    r"prompt too long; exceeded (?:max )?context length",
    r"range of input length should be",
    r"context[_ ]length[_ ]exceeded",
    r"too many tokens",
    r"token limit exceeded",
];

const NON_OVERFLOW_PATTERNS: &[&str] = &[
    r"^(Throttling error|Service unavailable):",
    r"rate limit",
    r"too many requests",
];

const CEREBRAS_BODYLESS_OVERFLOW_PATTERN: &str = r"^4(?:00|13)\s*(?:status code)?\s*\(no body\)";

fn compile_pattern(pattern: &str) -> Regex {
    // These ASCII patterns use JavaScript /i without /u. Rust's Unicode case
    // folding adds matches such as long-s; scoped ASCII folding avoids those.
    // JS \d is ASCII, and JS \s includes BOM but excludes the NEL code point.
    let mut source = String::new();
    let mut chars = pattern.chars();
    let mut in_class = false;
    while let Some(character) = chars.next() {
        match character {
            '\\' => match chars.next().expect("upstream regex escape has a character") {
                'd' => source.push_str(if in_class { "0-9" } else { "[0-9]" }),
                's' => source.push_str(r"(?u:[\x09-\x0d\x20\u{00a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}])"),
                escaped => { source.push('\\'); source.push(escaped); }
            },
            '[' => { in_class = true; source.push(character); }
            ']' => { in_class = false; source.push(character); }
            '.' if !in_class => source.push_str(r"(?u:[^\n\r\u{2028}\u{2029}])"),
            _ => source.push(character),
        }
    }
    Regex::new(&format!("(?i-u:{source})")).expect("the upstream overflow pattern is valid")
}

/// Return independent regex handles, mirroring Pi's copied pattern array.
pub fn get_overflow_patterns() -> Vec<Regex> {
    overflow_patterns().clone()
}

fn overflow_patterns() -> &'static Vec<Regex> {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        OVERFLOW_PATTERNS
            .iter()
            .map(|pattern| compile_pattern(pattern))
            .collect()
    })
}

pub fn is_context_overflow(message: &AssistantMessage, context_window: Option<f64>) -> bool {
    if message.stop_reason == StopReason::Error
        && let Some(error_message) = message
            .error_message
            .as_ref()
            .filter(|message| !message.is_empty())
    {
        let error_message = error_message.ascii_pattern_text();
        static NON_OVERFLOW: OnceLock<Vec<Regex>> = OnceLock::new();
        let non_overflow = NON_OVERFLOW.get_or_init(|| {
            NON_OVERFLOW_PATTERNS
                .iter()
                .map(|pattern| compile_pattern(pattern))
                .collect()
        });
        if !non_overflow
            .iter()
            .any(|pattern| pattern.is_match(&error_message))
        {
            if overflow_patterns()
                .iter()
                .any(|pattern| pattern.is_match(&error_message))
            {
                return true;
            }
            static CEREBRAS: OnceLock<Regex> = OnceLock::new();
            if message.provider == "cerebras"
                && CEREBRAS
                    .get_or_init(|| compile_pattern(CEREBRAS_BODYLESS_OVERFLOW_PATTERN))
                    .is_match(&error_message)
            {
                return true;
            }
        }
    }

    // JavaScript numeric truthiness excludes zero and NaN, but not negative
    // values or infinity. Preserve it even for unusual caller metadata.
    if let Some(context_window) = context_window.filter(|value| *value != 0.0 && !value.is_nan()) {
        let input_tokens = message.usage.input + message.usage.cache_read;
        if message.stop_reason == StopReason::Stop && input_tokens > context_window {
            return true;
        }
        if message.stop_reason == StopReason::Length
            && message.usage.output == 0.0
            && input_tokens >= context_window * 0.99
        {
            return true;
        }
    }
    false
}

pub fn is_recoverable_length(message: &AssistantMessage, desired_max_output: f64) -> bool {
    message.stop_reason == StopReason::Length
        && desired_max_output > 0.0
        && message.usage.output < desired_max_output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regex_shorthands_and_case_folding_follow_javascript() {
        let patterns = get_overflow_patterns();
        let matches = |text: &str| patterns.iter().any(|pattern| pattern.is_match(text));
        assert!(matches("PROMPT TOO LONG"));
        assert!(!matches("prompt exceedſ max length"));
        assert!(matches("maximum prompt length is 123"));
        assert!(!matches("maximum prompt length is ١٢٣"));
        assert!(matches("exceeds maximum context length\u{feff}(123)"));
        assert!(!matches("exceeds maximum context length\u{0085}(123)"));
        assert!(matches("input token count 🙈 exceeds the maximum"));
        for terminator in ['\n', '\r', '\u{2028}', '\u{2029}'] {
            assert!(!matches(&format!(
                "input token count{terminator}exceeds the maximum"
            )));
        }
    }
}
