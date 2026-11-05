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
        r"(?i)exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\d,]+ tokens?|\s*\([\d,]+\))",
        r"(?i)input token count.*exceeds the maximum",
        r"(?i)maximum prompt length is \d+",
        r"(?i)reduce the length of the messages",
        r"(?i)maximum context length is \d+ tokens",
        r"(?i)exceeds (?:the )?maximum allowed input length of [\d,]+ tokens?",
        r"(?i)input \(\d+ tokens\) is longer than the model'?s context length \(\d+ tokens\)",
        r"(?i)exceeds the limit of \d+",
        r"(?i)exceeds the available context size",
        r"(?i)greater than the context length",
        r"(?i)context window exceeds limit",
        r"(?i)exceeded model token limit",
        r"(?i)too large for model with \d+ maximum context length",
        r"(?i)prompt has [\d,]+ tokens?, but the configured context size is [\d,]+ tokens?",
        r"(?i)model_context_window_exceeded",
        r"(?i)prompt too long; exceeded (?:max )?context length",
        r"(?i)range of input length should be",
        r"(?i)context[_ ]length[_ ]exceeded",
        r"(?i)too many tokens",
        r"(?i)token limit exceeded",
        // An HTTP 413 is a request too large for the provider, whatever its body says.
        // Errors start with the status, or with a provider's own prefix.
        r"(?i)^(?:[\w -]+ API error \(413\):|413(?:\s|:|$))",
    ])
    .expect("overflow patterns compile")
});

/// Cerebras answers an overflow with a 400 or 413 that has no body.
static CEREBRAS_BODYLESS_OVERFLOW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^4(?:00|13)\s*(?:status code)?\s*\(no body\)").expect("pattern compiles")
});

/// Rate limits can mention tokens without being overflow: Bedrock's throttling and
/// availability errors, rate limits, and any 429. Errors start with the HTTP status when
/// there is one.
static NOT_OVERFLOW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(?:Throttling error|Service unavailable):|rate limit|too many requests|^429\b",
    )
    .expect("pattern compiles")
});

/// Whether a response failed, or silently degraded, because the context was too large.
///
/// Error responses are matched against the known provider texts, and from Cerebras a
/// 400 or 413 without a body counts too. With `context_window`, a successful response
/// whose input exceeded the window counts, and so does a length stop without output whose
/// input filled it, for providers that truncate instead of failing.
pub fn is_context_overflow(message: &AssistantMessage, context_window: Option<u64>) -> bool {
    if message.stop_reason == StopReason::Error
        && let Some(error) = &message.error_message
        && !NOT_OVERFLOW.is_match(error)
        && (OVERFLOW.is_match(error)
            || (message.provider == "cerebras" && CEREBRAS_BODYLESS_OVERFLOW.is_match(error)))
    {
        return true;
    }
    let Some(window) = context_window.filter(|window| *window > 0) else {
        return false;
    };
    let input = message.usage.input + message.usage.cache_read;
    match message.stop_reason {
        StopReason::Stop => input > window,
        // Some servers truncate the input to fill the window and then stop at once. What
        // they count as full can fall a little short of it.
        StopReason::Length => message.usage.output == 0 && input as f64 >= window as f64 * 0.99,
        _ => false,
    }
}

/// Whether a length stop ended before the output limit the caller wanted. Context
/// pressure or a provider's own cut can end a reply early, so the caller can compact and
/// retry once. `desired_max_output` is the limit before it was fitted to the context,
/// such as the model's `max_tokens`.
pub fn is_recoverable_length(message: &AssistantMessage, desired_max_output: u64) -> bool {
    message.stop_reason == StopReason::Length
        && desired_max_output > 0
        && message.usage.output < desired_max_output
}

/// Account limits: an exhausted quota, budget or balance, or a usage limit that resets
/// after hours. Retrying cannot help, even when the provider answers with a 429.
static ACCOUNT_LIMIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)GoUsageLimitError|FreeUsageLimitError|Monthly usage limit reached|available balance",
        r"|insufficient_quota|out of budget|quota exceeded|billing",
        r"|subscription_sharing_usage_limit_exceeded",
        r"|subscription_sharing_workplace_individual_usage_limit_exceeded",
    ))
    .expect("pattern compiles")
});

static RETRYABLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)overloaded|server_busy|servers are currently busy|high demand|at capacity",
        r"|rate.?limit|too many requests",
        // A status code, but not digits inside a longer word or number, such as a
        // request id or a byte count.
        r"|(?:^|[^a-z0-9])(?:429|500|502|503|504|520|524)(?:[^a-z0-9]|$)",
        r"|service.?(?:is.?)?(?:temporarily.?)?unavailable|server.?error|internal.?error",
        r"|provider.?returned.?(?:an.?)?error|exceeded request buffer limit while retrying upstream",
        r"|unable to forward upstream|upstream.?request.?failed|upstream_stream_read_error",
        r"|stream was interrupted|http/?2 stream failed",
        r"|network.?error|connection.?(?:error|refused|lost|reset)",
        r"|other side closed|fetch failed|request to \S+ failed, reason:",
        r"|getaddrinfo|ENOTFOUND|EAI_AGAIN|upstream.?connect",
        r"|reset before headers|socket hang up|socket connection was closed|timed? ?out",
        r"|terminated|websocket.?(?:closed|error)",
        r"|ended without|stream ended|http2 request did not get a response",
        r"|stream disconnected before completion",
        r"|pending stream has been canceled|retry delay",
        r"|retry your request|try your request again|ResourceExhausted",
        r"|subscription_sharing_(?:usage|user)_unavailable",
        r"|error sending request|error decoding response body|broken pipe",
    ))
    .expect("pattern compiles")
});

/// Whether a failed response looks like a transient provider or transport error that
/// retrying the same request may fix. An exhausted quota, budget or balance never is.
/// Handle context overflow before asking this.
pub fn is_retryable_error(message: &AssistantMessage) -> bool {
    message.stop_reason == StopReason::Error
        && message
            .error_message
            .as_deref()
            .is_some_and(|error| !ACCOUNT_LIMIT.is_match(error) && RETRYABLE.is_match(error))
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
            "400 `prompt too long; exceeded max context length by 100918 tokens`",
            "400 {\"code\":\"1261\",\"message\":\"Prompt too long\"}",
            "400 {\"code\":\"1261\",\"message\":\"Prompt exceeds max length\"}",
            "400 The input (516368 tokens) is longer than the model's context length (262144 tokens).",
            "Error: 503 litellm.ServiceUnavailableError: litellm.MidStreamFallbackError: litellm.APIConnectionError: APIConnectionError: OpenAIException - Requested token count exceeds the model's maximum context length of 131072 tokens.",
            "Provider returned error: Input length 131393 exceeds the maximum allowed input length of 131040 tokens.",
            "400 Prompt has 256468 tokens, but the configured context size is 256000 tokens",
            "Prompt has 5,958,968 tokens, but the configured context size is 256,000 tokens",
        ] {
            assert!(is_context_overflow(&failed(error), None), "{error}");
        }
        // The phrases count only with the sizes Pi's patterns ask for.
        for error in [
            "500 `model runner crashed unexpectedly`",
            "max_tokens exceeds the model's maximum context length",
            "the request is longer than the model's context length",
        ] {
            assert!(!is_context_overflow(&failed(error), None), "{error}");
        }
        assert!(is_context_overflow(
            &failed("400 Too many tokens: 140000 > 128000"),
            None
        ));
        for throttled in [
            "ThrottlingException: Too many tokens, rate limit",
            "429 Too many tokens, please wait before trying again.",
            "429: {\"message\":\"Too many tokens per minute\"}",
            "Throttling error: Too many tokens, please wait before trying again.",
            "Service unavailable: The service is temporarily unavailable.",
            "Rate limit exceeded, please retry after 30 seconds.",
            "Too many requests. Please slow down.",
        ] {
            assert!(
                !is_context_overflow(&failed(throttled), None),
                "{throttled}"
            );
        }
        assert!(!is_context_overflow(&failed("invalid api key"), None));
    }

    #[test]
    fn a_bodyless_400_is_overflow_only_from_cerebras() {
        let mut message = failed("400 status code (no body)");
        message.provider = "cerebras".into();
        assert!(is_context_overflow(&message, Some(131_072)));
        message.provider = "opencode-go".into();
        assert!(!is_context_overflow(&message, Some(1_000_000)));
        // A 413 is overflow from every provider.
        message.error_message = Some("413 status code (no body)".into());
        assert!(is_context_overflow(&message, Some(1_000_000)));
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

    fn length_stop(input: u64, cache_read: u64, output: u64) -> AssistantMessage {
        let mut message = failed("");
        message.stop_reason = StopReason::Length;
        message.error_message = None;
        message.usage = Usage {
            input,
            cache_read,
            output,
            total_tokens: input + cache_read + output,
            ..Usage::default()
        };
        message
    }

    #[test]
    fn a_length_stop_without_output_that_fills_the_window_is_overflow() {
        // Xiaomi's cut leaves the input a little short of the window.
        assert!(is_context_overflow(
            &length_stop(58, 1_048_512, 0),
            Some(1_048_576)
        ));
        assert!(!is_context_overflow(
            &length_stop(1_000, 0, 4_096),
            Some(200_000)
        ));
        assert!(!is_context_overflow(&length_stop(100, 0, 0), Some(200_000)));
    }

    #[test]
    fn a_length_stop_below_the_wanted_output_is_recoverable() {
        assert!(is_recoverable_length(&length_stop(3, 253_584, 16), 128_000));
        assert!(!is_recoverable_length(&length_stop(4_062, 0, 1_024), 1_024));
        assert!(is_recoverable_length(&length_stop(100, 0, 0), 128_000));
        assert!(!is_recoverable_length(&length_stop(100, 0, 0), 0));
        let mut stopped = length_stop(100, 0, 0);
        stopped.stop_reason = StopReason::Stop;
        assert!(!is_recoverable_length(&stopped, 128_000));
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
            "{\"code\":\"server_busy\",\"message\":\"Busy\"}",
            "Our servers are currently busy, please come back later",
            "The service is temporarily unavailable",
            "The upstream provider returned an error",
            "Unable to forward upstream stream",
            "upstream_error: Upstream request failed",
            "upstream_stream_read_error",
            "The stream was interrupted",
            "Upstream HTTP/2 stream failed",
            "request to https://oauth2.googleapis.com/token failed, reason: ",
            "stream disconnected before completion: stream closed before response.completed",
            "status_503",
        ] {
            assert!(is_retryable_error(&failed(error)), "{error}");
        }
        for error in [
            "401 invalid api key",
            "400 bad request: unknown field",
            // Digits inside a request id or a byte count are no status code.
            "400 invalid request req_011CU503xyz",
            "400 the body exceeds 5242880 bytes",
        ] {
            assert!(!is_retryable_error(&failed(error)), "{error}");
        }
    }

    #[test]
    fn a_413_is_overflow_whatever_its_body_says() {
        for error in [
            "413 {\"message\":\"failed to parse request\",\"code\":\"\"}",
            "413",
            "413: Payload Too Large",
            "github-copilot API error (413): {\"message\":\"failed to parse request\"}",
        ] {
            assert!(is_context_overflow(&failed(error), None), "{error}");
        }
        // Only the status, not a number in the body or a longer one.
        for error in [
            "github-copilot API error (400): {\"message\":\"failed to parse request: invalid value 413\"}",
            "custom-provider API error (400): upstream mentioned OpenAI API error (413): invalid input",
            "custom-provider API error (4130): invalid input",
            "4130 failed to parse request",
        ] {
            assert!(!is_context_overflow(&failed(error), None), "{error}");
        }
        // Only for a failed response.
        let mut stopped = failed("413 Payload Too Large");
        stopped.stop_reason = StopReason::Aborted;
        assert!(!is_context_overflow(&stopped, None));
    }

    #[test]
    fn exhausted_quotas_are_not_retried_even_as_429s() {
        for error in [
            "429 You exceeded your current quota (insufficient_quota)",
            "429 Monthly usage limit reached",
            "Rate limit: quota exceeded for this key",
            "402 billing hard limit reached",
            "subscription_sharing_usage_limit_exceeded",
            "429 subscription_sharing_workplace_individual_usage_limit_exceeded",
        ] {
            assert!(!is_retryable_error(&failed(error)), "{error}");
        }
        for error in [
            "websocket closed",
            "ResourceExhausted: try again",
            "subscription_sharing_usage_unavailable",
        ] {
            assert!(is_retryable_error(&failed(error)), "{error}");
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
