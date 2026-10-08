//! Provider-error classification and the bounded assistant retry loop.
//!
//! Ported from `packages/ai/src/utils/retry.ts` at Pi v1.0.4.

use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use super::js_value::JsString;
use crate::env::{CancellationToken, PiEnv};
use crate::types::{AssistantMessage, StopReason};

const NON_RETRYABLE_PROVIDER_LIMIT_ERROR_PATTERNS: &[&str] = &[
    "GoUsageLimitError",
    "FreeUsageLimitError",
    "Monthly usage limit reached",
    "available balance",
    "insufficient_quota",
    "out of budget",
    "quota exceeded",
    "billing",
    "subscription_sharing_usage_limit_exceeded",
];

const RETRYABLE_PROVIDER_ERROR_PATTERNS: &[&str] = &[
    "overloaded",
    "currently experiencing high demand",
    "model is at capacity",
    "rate.?limit",
    "too many requests",
    "429",
    "500",
    "502",
    "503",
    "504",
    "520",
    "524",
    "service.?unavailable",
    "server.?error",
    "internal.?error",
    "provider.?returned.?error",
    "exceeded request buffer limit while retrying upstream",
    "network.?error",
    "connection.?error",
    "connection.?refused",
    "connection.?lost",
    "other side closed",
    "fetch failed",
    "getaddrinfo",
    "ENOTFOUND",
    "EAI_AGAIN",
    "upstream.?connect",
    "reset before headers",
    "socket hang up",
    "socket connection was closed",
    "timed? out",
    "timeout",
    "terminated",
    "websocket.?closed",
    "websocket.?error",
    "ended without",
    "stream ended before message_stop",
    "stream ended before a terminal response event",
    "http2 request did not get a response",
    "pending stream has been canceled",
    "retry delay",
    "you can retry your request",
    "try your request again",
    "please retry your request",
    "ResourceExhausted",
    "subscription_sharing_usage_unavailable",
    "subscription_sharing_user_unavailable",
];

fn build_provider_error_pattern(patterns: &[&str]) -> Regex {
    // Pi uses JavaScript's /i without /u. Its patterns contain only ASCII;
    // lowercasing ASCII keeps Unicode characters from acquiring extra matches
    // through Rust's Unicode case folding. A dot consumes one UTF-16 code unit
    // and excludes JavaScript's four line terminators, rather than consuming an
    // arbitrary Unicode scalar as Rust's regex dot would.
    let source = patterns
        .join("|")
        .to_ascii_lowercase()
        .replace('.', r"[^\n\r\u{2028}\u{2029}\u{10000}-\u{10FFFF}]");
    Regex::new(&source).expect("the upstream provider-error patterns are valid")
}

/// Bounded retries with exponential backoff. The initial call does not count
/// toward `max_retries`. Numeric fields retain JavaScript number semantics.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetryPolicy {
    pub enabled: bool,
    pub max_retries: f64,
    pub base_delay_ms: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_agent_delay_ms: Option<f64>,
}

pub const DEFAULT_MAX_AGENT_RETRY_DELAY_MS: f64 = 60_000.0;
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

pub fn retry_delay_ms(policy: &RetryPolicy, attempt: f64) -> f64 {
    let exponent = if attempt.is_nan() {
        f64::NAN
    } else {
        (attempt - 1.0).max(0.0)
    };
    let delay = policy.base_delay_ms * 2.0_f64.powf(exponent);
    let safe_delay = if delay.is_finite() && delay.fract() == 0.0 && delay.abs() <= MAX_SAFE_INTEGER
    {
        delay
    } else {
        MAX_SAFE_INTEGER
    };
    let cap = policy
        .max_agent_delay_ms
        .unwrap_or(DEFAULT_MAX_AGENT_RETRY_DELAY_MS);
    // f64::min discards NaN and does not promise Math.min's signed-zero result.
    if cap.is_nan() {
        f64::NAN
    } else if safe_delay == 0.0 && cap == 0.0 {
        if safe_delay.is_sign_negative() || cap.is_sign_negative() {
            -0.0
        } else {
            0.0
        }
    } else {
        safe_delay.min(cap)
    }
}

/// Callback failures represent rejected JavaScript callback promises. They are
/// returned unchanged, just like producer failures, rather than turned into an
/// assistant error message.
pub type RetryCallbackFuture<'a, E> = Pin<Box<dyn Future<Output = Result<(), E>> + Send + 'a>>;

pub trait RetryCallbacks<E>: Send {
    fn on_retry_scheduled(
        &mut self,
        _attempt: f64,
        _max_attempts: f64,
        _delay_ms: f64,
        _error_message: &JsString,
    ) -> RetryCallbackFuture<'_, E> {
        Box::pin(async { Ok(()) })
    }

    fn on_retry_attempt_start(&mut self) -> RetryCallbackFuture<'_, E> {
        Box::pin(async { Ok(()) })
    }

    fn on_retry_finished(
        &mut self,
        _success: bool,
        _attempt: f64,
        _final_error: Option<&JsString>,
    ) -> RetryCallbackFuture<'_, E> {
        Box::pin(async { Ok(()) })
    }
}

/// Run one assistant-producing call, retrying transient provider errors within
/// the policy budget. Every callback completes before the next step starts.
pub async fn retry_assistant_call<P, F, E>(
    mut produce: P,
    policy: Option<&RetryPolicy>,
    signal: Option<&CancellationToken>,
    mut callbacks: Option<&mut dyn RetryCallbacks<E>>,
    env: &dyn PiEnv,
) -> Result<AssistantMessage, E>
where
    P: FnMut() -> F,
    F: Future<Output = Result<AssistantMessage, E>>,
{
    let max_attempts = policy
        .filter(|policy| policy.enabled)
        .map_or(0.0, |policy| policy.max_retries);
    let mut attempt = 0.0;
    let mut last_retry: Option<f64> = None;
    loop {
        let response = produce().await?;

        if response.stop_reason == StopReason::Aborted {
            if let (Some(last_retry), Some(callbacks)) = (last_retry, callbacks.as_deref_mut()) {
                callbacks.on_retry_finished(false, last_retry, None).await?;
            }
            return Ok(response);
        }

        if response.stop_reason != StopReason::Error {
            if let (Some(last_retry), Some(callbacks)) = (last_retry, callbacks.as_deref_mut()) {
                callbacks.on_retry_finished(true, last_retry, None).await?;
            }
            return Ok(response);
        }

        if attempt >= max_attempts || !is_retryable_assistant_error(&response) {
            if let (Some(last_retry), Some(callbacks)) = (last_retry, callbacks.as_deref_mut()) {
                callbacks
                    .on_retry_finished(false, last_retry, response.error_message.as_ref())
                    .await?;
            }
            return Ok(response);
        }

        attempt += 1.0;
        last_retry = Some(attempt);
        let unknown_error = JsString::from("Unknown error");
        let error_message = response
            .error_message
            .as_ref()
            .filter(|error| !error.is_empty())
            .unwrap_or(&unknown_error);
        let delay_ms = retry_delay_ms(policy.expect("a retry requires an enabled policy"), attempt);
        if let Some(callbacks) = callbacks.as_deref_mut() {
            callbacks
                .on_retry_scheduled(attempt, max_attempts, delay_ms, error_message)
                .await?;
        }

        if env.sleep(delay_ms, signal).await.is_err() {
            if let Some(callbacks) = callbacks.as_deref_mut() {
                callbacks
                    .on_retry_finished(false, attempt, Some(error_message))
                    .await?;
            }
            return Ok(AssistantMessage {
                stop_reason: StopReason::Aborted,
                error_message: None,
                ..response
            });
        }
        if let Some(callbacks) = callbacks.as_deref_mut() {
            callbacks.on_retry_attempt_start().await?;
        }
    }
}

pub fn is_retryable_assistant_error(message: &AssistantMessage) -> bool {
    if message.stop_reason != StopReason::Error {
        return false;
    }
    let Some(error_message) = message
        .error_message
        .as_ref()
        .filter(|error| !error.is_empty())
    else {
        return false;
    };
    let error_message = error_message.ascii_pattern_text().to_ascii_lowercase();
    static NON_RETRYABLE: OnceLock<Regex> = OnceLock::new();
    static RETRYABLE: OnceLock<Regex> = OnceLock::new();
    if NON_RETRYABLE
        .get_or_init(|| build_provider_error_pattern(NON_RETRYABLE_PROVIDER_LIMIT_ERROR_PATTERNS))
        .is_match(&error_message)
    {
        return false;
    }
    RETRYABLE
        .get_or_init(|| build_provider_error_pattern(RETRYABLE_PROVIDER_ERROR_PATTERNS))
        .is_match(&error_message)
}
