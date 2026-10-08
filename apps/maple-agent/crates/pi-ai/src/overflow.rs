use std::sync::LazyLock;

use regex::{Regex, RegexSet};

use crate::types::{AssistantMessage, StopReason};

/// Error texts providers return when the input exceeds the context window.
static OVERFLOW: LazyLock<RegexSet> = LazyLock::new(|| {
    RegexSet::new([
        r"(?i)prompt (?:is )?too long",
        r"(?i)prompt exceeds max length",
        r"(?i)request_too_large",
        r"(?i)input is too long for requested model",
        r"(?i)exceeds the context window",
        r"(?i)exceeds (?:the )?(?:model'?s )?maximum context length",
        r"(?i)input token count.*exceeds the maximum",
        r"(?i)maximum prompt length is \d+",
        r"(?i)reduce the length of the messages",
        r"(?i)maximum context length is \d+ tokens",
        r"(?i)exceeds (?:the )?maximum allowed input length",
        r"(?i)is longer than the model'?s context length",
        r"(?i)exceeds the limit of \d+",
        r"(?i)exceeds the available context size",
        r"(?i)greater than the context length",
        r"(?i)context window exceeds limit",
        r"(?i)exceeded model token limit",
        r"(?i)too large for model with \d+ maximum context length",
        r"(?i)but the configured context size is",
        r"(?i)model_context_window_exceeded",
        r"(?i)range of input length should be",
        r"(?i)context[_ ]length[_ ]exceeded",
        r"(?i)too many tokens",
        r"(?i)token limit exceeded",
    ])
    .expect("overflow patterns compile")
});

/// Rate limits can mention tokens without being overflow. Errors start with the HTTP
/// status when there is one.
static NOT_OVERFLOW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)rate.?limit|too many requests|throttl|service.?unavailable|^429\b")
        .expect("pattern compiles")
});

/// Whether a response failed, or silently degraded, because the context was too large.
///
/// Error responses are matched against the known provider texts. With `context_window`,
/// a successful response whose input already filled the window counts too, for providers
/// that truncate instead of failing.
pub fn is_context_overflow(message: &AssistantMessage, context_window: Option<u64>) -> bool {
    if message.stop_reason == StopReason::Error
        && let Some(error) = &message.error_message
        && !NOT_OVERFLOW.is_match(error)
        && OVERFLOW.is_match(error)
    {
        return true;
    }
    let Some(window) = context_window.filter(|window| *window > 0) else {
        return false;
    };
    let input = message.usage.input + message.usage.cache_read;
    match message.stop_reason {
        StopReason::Stop => input > window,
        // Some servers truncate the input to fill the window and then stop at once.
        StopReason::Length => message.usage.output == 0 && input >= window,
        _ => false,
    }
}

static RETRYABLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)overloaded|high demand|at capacity|rate.?limit|too many requests",
        r"|\b(?:429|500|502|503|504|520|524)\b|service.?unavailable|server.?error|internal.?error",
        r"|provider.?returned.?error|network.?error|connection.?(?:error|refused|lost|reset)",
        r"|other side closed|fetch failed|getaddrinfo|ENOTFOUND|EAI_AGAIN|upstream.?connect",
        r"|reset before headers|socket hang up|socket connection was closed|timed? ?out",
        r"|terminated|ended without|stream ended|retry your request|try your request again",
        r"|error sending request|error decoding response body|broken pipe",
    ))
    .expect("pattern compiles")
});

/// Whether a failed response looks like a transient provider or transport error that
/// retrying the same request may fix. Handle context overflow before asking this.
pub fn is_retryable_error(message: &AssistantMessage) -> bool {
    message.stop_reason == StopReason::Error
        && message
            .error_message
            .as_deref()
            .is_some_and(|error| RETRYABLE.is_match(error))
}

/// Exponential backoff: `base_ms * 2^(attempt - 1)`, capped at `max_ms`.
pub fn retry_delay_ms(base_ms: u64, max_ms: u64, attempt: u32) -> u64 {
    let factor = 1u64
        .checked_shl(attempt.saturating_sub(1))
        .unwrap_or(u64::MAX);
    base_ms.saturating_mul(factor).min(max_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Usage;

    fn failed(error: &str) -> AssistantMessage {
        AssistantMessage {
            content: Vec::new(),
            api: "a".into(),
            provider: "p".into(),
            model: "m".into(),
            response_id: None,
            thinking_level: None,
            usage: Usage::default(),
            stop_reason: StopReason::Error,
            error_message: Some(error.into()),
            timestamp: 0,
        }
    }

    #[test]
    fn overflow_errors_are_recognized_across_providers() {
        for error in [
            "prompt is too long: 213462 tokens > 200000 maximum",
            "Your input exceeds the context window of this model",
            "This model's maximum context length is 131072 tokens. However, you requested 140000",
            "Input length (265330) exceeds model's maximum context length (262144).",
            "the request exceeds the available context size, try increasing it",
        ] {
            assert!(is_context_overflow(&failed(error), None), "{error}");
        }
        assert!(is_context_overflow(
            &failed("400 Too many tokens: 140000 > 128000"),
            None
        ));
        for throttled in [
            "ThrottlingException: Too many tokens, rate limit",
            "429 Too many tokens, please wait before trying again.",
        ] {
            assert!(
                !is_context_overflow(&failed(throttled), None),
                "{throttled}"
            );
        }
        assert!(!is_context_overflow(&failed("invalid api key"), None));
    }

    #[test]
    fn silent_overflow_needs_the_context_window() {
        let mut message = failed("");
        message.stop_reason = StopReason::Stop;
        message.error_message = None;
        message.usage.input = 1_100;
        assert!(!is_context_overflow(&message, None));
        assert!(is_context_overflow(&message, Some(1_000)));
        message.usage.input = 900;
        assert!(!is_context_overflow(&message, Some(1_000)));
    }

    #[test]
    fn transient_errors_are_retryable_and_others_are_not() {
        for error in [
            "429 Too Many Requests",
            "503 Service Unavailable",
            "Provider overloaded",
            "error sending request for url",
            "connection reset by peer",
            "Request timed out",
        ] {
            assert!(is_retryable_error(&failed(error)), "{error}");
        }
        for error in ["401 invalid api key", "400 bad request: unknown field"] {
            assert!(!is_retryable_error(&failed(error)), "{error}");
        }
    }

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(retry_delay_ms(2_000, 60_000, 1), 2_000);
        assert_eq!(retry_delay_ms(2_000, 60_000, 3), 8_000);
        assert_eq!(retry_delay_ms(2_000, 60_000, 10), 60_000);
        assert_eq!(retry_delay_ms(2_000, 60_000, 200), 60_000);
    }
}
