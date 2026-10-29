//! A deliberately lossy boundary for untrusted upstream error bodies.
//!
//! Providers can echo prompts, tool arguments, credentials, or identifiers in
//! any field. Only fixed vocabulary and locally generated summaries may leave
//! this module; neither a JSON field nor a transport error is a safe log value.

use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::Value;
use std::time::Duration;
use tokio::time::{timeout_at, Instant};

const MAX_DIAGNOSTIC_BODY_BYTES: usize = 8 * 1024;
const DIAGNOSTIC_BODY_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticBodyState {
    Complete,
    Empty,
    Truncated,
    InvalidEncoding,
    InvalidJson,
    ReadError,
    DeadlineExceeded,
}

impl DiagnosticBodyState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Empty => "empty",
            Self::Truncated => "truncated",
            Self::InvalidEncoding => "invalid_encoding",
            Self::InvalidJson => "invalid_json",
            Self::ReadError => "read_error",
            Self::DeadlineExceeded => "deadline_exceeded",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamDiagnostic {
    pub body_state: DiagnosticBodyState,
    pub code: Option<&'static str>,
    pub error_type: Option<&'static str>,
    /// A locally generated category summary, never an upstream text snippet.
    pub summary: Option<&'static str>,
    // Exact input-validation rejection; broader context/output-limit summaries
    // never authorize recovery. Upstream text and counts are not retained.
    input_context_overflow: bool,
}

impl UpstreamDiagnostic {
    fn without_details(body_state: DiagnosticBodyState) -> Self {
        Self {
            body_state,
            code: None,
            error_type: None,
            summary: None,
            input_context_overflow: false,
        }
    }

    pub(crate) fn is_input_context_overflow(&self) -> bool {
        self.body_state == DiagnosticBodyState::Complete && self.input_context_overflow
    }
}

/// Inspect a small, complete error body without draining an unbounded response.
///
/// Reaching the byte bound is conservatively marked truncated, even if the
/// next poll would have been EOF. The deadline covers the whole read, not each
/// chunk. Dropping this future drops the owned stream; no task outlives it.
pub(super) async fn read_error_diagnostic<S>(mut stream: S) -> UpstreamDiagnostic
where
    S: Stream<Item = Result<Bytes, String>> + Unpin,
{
    let deadline = Instant::now() + DIAGNOSTIC_BODY_TIMEOUT;
    let mut body = Vec::with_capacity(MAX_DIAGNOSTIC_BODY_BYTES);
    let mut polls = 0_u8;
    loop {
        // An immediately ready stream of empty chunks must not defeat the
        // total deadline or monopolize the executor indefinitely.
        if Instant::now() >= deadline {
            return UpstreamDiagnostic::without_details(DiagnosticBodyState::DeadlineExceeded);
        }
        if polls == 16 {
            tokio::task::yield_now().await;
            polls = 0;
            continue;
        }
        polls += 1;
        match timeout_at(deadline, stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                if chunk.len() >= MAX_DIAGNOSTIC_BODY_BYTES - body.len() {
                    return UpstreamDiagnostic::without_details(DiagnosticBodyState::Truncated);
                }
                body.extend_from_slice(&chunk);
            }
            Ok(Some(Err(_))) => {
                return UpstreamDiagnostic::without_details(DiagnosticBodyState::ReadError);
            }
            Ok(None) => return parse_complete_body(&body),
            Err(_) => {
                return UpstreamDiagnostic::without_details(DiagnosticBodyState::DeadlineExceeded);
            }
        }
    }
}

fn parse_complete_body(body: &[u8]) -> UpstreamDiagnostic {
    let Ok(text) = std::str::from_utf8(body) else {
        return UpstreamDiagnostic::without_details(DiagnosticBodyState::InvalidEncoding);
    };
    if text.trim().is_empty() {
        return UpstreamDiagnostic::without_details(DiagnosticBodyState::Empty);
    }
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return UpstreamDiagnostic::without_details(DiagnosticBodyState::InvalidJson);
    };
    let error = value
        .get("error")
        .filter(|v| v.is_object())
        .unwrap_or(&value);
    let code = allowlisted_label(error.get("code"));
    let error_type = allowlisted_label(error.get("type"));
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| value.get("error").and_then(Value::as_str))
        .or_else(|| value.get("detail").and_then(Value::as_str));
    let summary = code
        .and_then(summary_for_label)
        .or_else(|| message.and_then(summary_for_message))
        .or_else(|| error_type.and_then(summary_for_label));
    UpstreamDiagnostic {
        body_state: DiagnosticBodyState::Complete,
        code,
        error_type,
        summary,
        input_context_overflow: input_validation_labels(error)
            && message.is_some_and(known_input_context_overflow),
    }
}

/// The message grammar must not override conflicting structured error metadata.
/// Missing labels are tolerated for provider wrappers which omit them; present
/// labels must agree with this specific request-validation rejection.
fn input_validation_labels(error: &Value) -> bool {
    let code_matches = match error.get("code").filter(|value| !value.is_null()) {
        None => true,
        Some(Value::Number(value)) => value.as_u64() == Some(400),
        Some(Value::String(value)) => matches!(
            value.as_str(),
            "context_length_exceeded"
                | "invalid_request_error"
                | "invalid_request"
                | "invalid_argument"
        ),
        Some(_) => false,
    };
    let type_matches = error
        .get("type")
        .filter(|value| !value.is_null())
        .is_none_or(|value| {
            matches!(
                value.as_str(),
                Some(
                    "BadRequestError"
                        | "invalid_request_error"
                        | "invalid_request"
                        | "invalid_argument"
                )
            )
        });
    let parameter_matches = error
        .get("param")
        .filter(|value| !value.is_null())
        .is_none_or(|value| value.as_str() == Some("input_tokens"));
    code_matches && type_matches && parameter_matches
}

/// Recognize the pinned vLLM input-token validator's complete, fixed grammar.
/// Source: vLLM db9527a46873454610df6dbedf79a36d6bf1a7f6,
/// `vllm/renderers/params.py::TokenizeParams._token_len_check`.
/// The API uses `str(VLLMValidationError)`, including its parameter/value suffix:
/// https://github.com/vllm-project/vllm/blob/db9527a46873454610df6dbedf79a36d6bf1a7f6/vllm/exceptions.py
/// https://github.com/vllm-project/vllm/blob/db9527a46873454610df6dbedf79a36d6bf1a7f6/vllm/entrypoints/serve/exception_handling/error_response.py
/// Its separate output-limit validator is not eligible: trimming history cannot
/// repair an output allowance which by itself fills the model's whole window.
/// Values are used only to validate the relation, never retained or logged.
fn known_input_context_overflow(message: &str) -> bool {
    fn number_before(text: &mut &str, separator: &str) -> Option<u64> {
        let (number, rest) = text.split_once(separator)?;
        if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        *text = rest;
        number.parse().ok()
    }

    let Some(mut rest) = message
        .trim()
        .strip_prefix("This model's maximum context length is ")
    else {
        return false;
    };
    let Some(window) = number_before(&mut rest, " tokens. However, you requested ") else {
        return false;
    };
    let Some(output) = number_before(&mut rest, " output tokens and your prompt contains ") else {
        return false;
    };
    let lower_bound = rest.starts_with("at least ");
    if lower_bound {
        rest = &rest["at least ".len()..];
    }
    let Some(input) = number_before(&mut rest, " input tokens, for a total of ") else {
        return false;
    };
    if lower_bound {
        let Some(remainder) = rest.strip_prefix("at least ") else {
            return false;
        };
        rest = remainder;
    }
    let Some(total) = number_before(
        &mut rest,
        " tokens. Please reduce the length of the input prompt or the number of requested output tokens.",
    ) else {
        return false;
    };
    // VLLMValidationError.__str__ appends this exact parameter/value suffix
    // before the API serializes the message. Accept it only when it agrees.
    let valid_suffix = rest.is_empty()
        || rest
            .strip_prefix(" (parameter=input_tokens, value=")
            .and_then(|suffix| suffix.strip_suffix(')'))
            .filter(|value| !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|value| value.parse::<u64>().ok())
            == Some(input);
    valid_suffix
        && window > 0
        && input > 0
        && output < window
        && input.checked_add(output) == Some(total)
        && total > window
}

fn allowlisted_label(value: Option<&Value>) -> Option<&'static str> {
    // Return literals, rather than returning even a bounded slice of input.
    match value.and_then(Value::as_str)? {
        "invalid_request_error" => Some("invalid_request_error"),
        "invalid_request" => Some("invalid_request"),
        "invalid_argument" => Some("invalid_argument"),
        "invalid_parameter" => Some("invalid_parameter"),
        "invalid_value" => Some("invalid_value"),
        "unsupported_parameter" => Some("unsupported_parameter"),
        "context_length_exceeded" => Some("context_length_exceeded"),
        "max_tokens_limit_exceeded" => Some("max_tokens_limit_exceeded"),
        "rate_limit_exceeded" => Some("rate_limit_exceeded"),
        "too_many_requests" => Some("too_many_requests"),
        "overloaded_error" => Some("overloaded_error"),
        "server_overloaded" => Some("server_overloaded"),
        "temporarily_unavailable" => Some("temporarily_unavailable"),
        "service_unavailable" => Some("service_unavailable"),
        "insufficient_quota" => Some("insufficient_quota"),
        "authentication_error" => Some("authentication_error"),
        "invalid_api_key" => Some("invalid_api_key"),
        "permission_denied" => Some("permission_denied"),
        "model_not_found" => Some("model_not_found"),
        "not_found_error" => Some("not_found_error"),
        "server_error" => Some("server_error"),
        "internal_server_error" => Some("internal_server_error"),
        "BadRequestError" => Some("BadRequestError"),
        "NotFoundError" => Some("NotFoundError"),
        "AuthenticationError" => Some("AuthenticationError"),
        "PermissionDeniedError" => Some("PermissionDeniedError"),
        "RateLimitError" => Some("RateLimitError"),
        "InternalServerError" => Some("InternalServerError"),
        "ServiceUnavailableError" => Some("ServiceUnavailableError"),
        _ => None,
    }
}

fn summary_for_label(label: &str) -> Option<&'static str> {
    match label {
        "invalid_request_error" | "invalid_request" | "invalid_argument" | "BadRequestError" => {
            Some("upstream rejected the request format")
        }
        "invalid_parameter" | "invalid_value" => Some("upstream rejected a parameter value"),
        "unsupported_parameter" => Some("upstream does not support a request parameter"),
        "context_length_exceeded" | "max_tokens_limit_exceeded" => {
            Some("upstream context or token limit exceeded")
        }
        "rate_limit_exceeded" | "too_many_requests" | "RateLimitError" => {
            Some("upstream rate limit reached")
        }
        "overloaded_error" | "server_overloaded" => Some("upstream reported overload"),
        "temporarily_unavailable" | "service_unavailable" | "ServiceUnavailableError" => {
            Some("upstream temporarily unavailable")
        }
        "insufficient_quota" => Some("upstream quota exhausted"),
        "authentication_error" | "invalid_api_key" | "AuthenticationError" => {
            Some("upstream authentication rejected")
        }
        "permission_denied" | "PermissionDeniedError" => Some("upstream permission denied"),
        "model_not_found" => Some("upstream model unavailable"),
        "not_found_error" | "NotFoundError" => Some("upstream resource not found"),
        "server_error" | "internal_server_error" | "InternalServerError" => {
            Some("upstream internal error")
        }
        _ => None,
    }
}

fn summary_for_message(message: &str) -> Option<&'static str> {
    // Matching is advisory only. Never copy any part of the message, including
    // a suffix that could echo a prompt, key, account, or tool argument. Only
    // anchored, known phrases are recognized; arbitrary text is suppressed.
    let normalized = message.trim().to_ascii_lowercase();
    let summaries = [
        (
            "this model's maximum context length is",
            "upstream context or token limit exceeded",
        ),
        (
            "maximum context length",
            "upstream context or token limit exceeded",
        ),
        (
            "prompt is too long",
            "upstream context or token limit exceeded",
        ),
        (
            "messages must not be empty",
            "upstream rejected the message structure",
        ),
        (
            "conversation roles must alternate",
            "upstream rejected the message structure",
        ),
        (
            "after the optional system message, conversation roles must alternate",
            "upstream rejected the message structure",
        ),
        (
            "roles must alternate",
            "upstream rejected the message structure",
        ),
        ("rate limit reached", "upstream rate limit reached"),
        ("too many requests", "upstream rate limit reached"),
        ("model is overloaded", "upstream reported overload"),
        ("server overloaded", "upstream reported overload"),
        ("service unavailable", "upstream temporarily unavailable"),
        (
            "incorrect api key provided",
            "upstream authentication rejected",
        ),
        ("invalid api key", "upstream authentication rejected"),
        ("authentication failed", "upstream authentication rejected"),
    ];
    summaries
        .into_iter()
        .find_map(|(prefix, summary)| normalized.starts_with(prefix).then_some(summary))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use serde_json::json;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::task::{Context, Poll};

    async fn from_json(value: Value) -> UpstreamDiagnostic {
        read_error_diagnostic(stream::iter([Ok(Bytes::from(value.to_string()))])).await
    }

    fn input_overflow_message(window: u64, output: u64, input: u64, lower_bound: bool) -> String {
        let qualifier = if lower_bound { "at least " } else { "" };
        format!(
            "This model's maximum context length is {window} tokens. However, you requested {output} output tokens and your prompt contains {qualifier}{input} input tokens, for a total of {qualifier}{} tokens. Please reduce the length of the input prompt or the number of requested output tokens.",
            input + output
        )
    }

    #[tokio::test]
    async fn only_complete_known_input_validation_can_authorize_context_recovery() {
        for lower_bound in [false, true] {
            for suffix in ["", " (parameter=input_tokens, value=1048576)"] {
                let mut diagnostic = from_json(json!({"error": {
                "type": "BadRequestError", "code": 400, "param": "input_tokens",
                "message": format!("{}{suffix}", input_overflow_message(1_048_576, 1, 1_048_576, lower_bound))
            }}))
            .await;
                assert!(diagnostic.is_input_context_overflow());
                for state in [
                    DiagnosticBodyState::Truncated,
                    DiagnosticBodyState::DeadlineExceeded,
                    DiagnosticBodyState::ReadError,
                ] {
                    diagnostic.body_state = state;
                    assert!(!diagnostic.is_input_context_overflow());
                }
            }
        }
    }

    #[tokio::test]
    async fn output_only_unknown_and_inconsistent_limit_errors_cannot_authorize_recovery() {
        let positive = input_overflow_message(1_048_576, 1, 1_048_576, false);
        for error in [
            json!({"code": "max_tokens_limit_exceeded", "message": positive}),
            json!({"code": "rate_limit_exceeded", "message": positive}),
            json!({"code": "authentication_error", "message": positive}),
            json!({"code": 429, "message": positive}),
            json!({"code": "400", "message": positive}),
            json!({"type": "RateLimitError", "message": positive}),
            json!({"type": "unknown-private-type", "message": positive}),
            json!({"param": {"input_tokens": true}, "message": positive}),
            json!({"code": "context_length_exceeded", "message": "private-prompt-canary"}),
            json!({"message": "max_tokens must be at most 32768"}),
            json!({"message": "This model's maximum context length is 100 tokens."}),
            json!({"message": input_overflow_message(100, 100, 1, false)}),
            json!({"message": input_overflow_message(100, 101, 1, false)}),
            json!({"message": input_overflow_message(100, 1, 99, false)}),
            json!({"message": input_overflow_message(100, 1, 100, true).replacen("for a total of at least ", "for a total of ", 1)}),
            json!({"message": input_overflow_message(100, 1, 100, false).replace("total of 101", "total of 102")}),
            json!({"message": format!("{positive} private-prompt-canary")}),
            json!({"message": format!("{positive} (parameter=input_tokens, value=7)")}),
            json!({"message": format!("{positive} (parameter=max_tokens, value=1048576)")}),
            json!({"param": "max_tokens", "message": positive}),
        ] {
            let diagnostic = from_json(json!({"error": error})).await;
            assert!(!diagnostic.is_input_context_overflow());
            assert!(!format!("{diagnostic:?}").contains("private-"));
        }
    }

    #[tokio::test]
    async fn recognized_input_validation_keeps_only_the_boolean_signal() {
        let diagnostic = from_json(json!({"error": {
            "type": "BadRequestError", "code": 400, "param": "input_tokens",
            "message": input_overflow_message(1_048_576, 1, 1_048_576, false),
            "prompt": "private-prompt-canary",
            "arguments": "private-tool-canary"
        }}))
        .await;
        assert!(diagnostic.is_input_context_overflow());
        let formatted = format!("{diagnostic:?}");
        assert!(!formatted.contains("1048576"));
        assert!(!formatted.contains("private-"));
    }

    #[tokio::test]
    async fn structured_codes_survive_but_echoed_private_fields_do_not() {
        let diagnostic = from_json(json!({
            "error": {
                "code": "context_length_exceeded",
                "type": "invalid_request_error",
                "message": "private-prompt-canary",
                "param": "private-tool-argument-canary"
            },
            "request_id": "private-account-canary",
            "headers": {"authorization": "private-credential-canary"}
        }))
        .await;
        assert_eq!(diagnostic.code, Some("context_length_exceeded"));
        assert_eq!(diagnostic.error_type, Some("invalid_request_error"));
        assert_eq!(
            diagnostic.summary,
            Some("upstream context or token limit exceeded")
        );
        assert!(!format!("{diagnostic:?}").contains("private-"));
    }

    #[tokio::test]
    async fn unknown_and_control_bearing_values_never_leave_the_boundary() {
        for value in [
            json!({"error": {"code": "secret-code", "type": "secret-type", "message": "secret-prompt"}}),
            json!({"error": {"code": "rate_limit_exceeded\nsecret", "type": "invalid_request_error\u{1b}", "message": "secret\r\n\u{1b}prompt"}}),
            json!({"error": {"code": {"message": "secret"}, "type": ["secret"], "message": {"text": "secret"}}}),
            json!({"message": "user prompt says: rate limit reached secret-prompt"}),
        ] {
            assert_eq!(
                from_json(value).await,
                UpstreamDiagnostic::without_details(DiagnosticBodyState::Complete)
            );
        }
    }

    #[tokio::test]
    async fn known_message_prefix_produces_only_a_static_summary() {
        let diagnostic = from_json(json!({
            "error": {
                "message": "Incorrect API key provided: secret-key\n\u{1b}[31mprivate-prompt",
                "type": "unknown-type-secret"
            }
        }))
        .await;
        assert_eq!(diagnostic.summary, Some("upstream authentication rejected"));
        assert_eq!(diagnostic.code, None);
        assert_eq!(diagnostic.error_type, None);
        let formatted = format!("{diagnostic:?}");
        assert!(!formatted.contains("secret"));
        assert!(!formatted.chars().any(char::is_control));
    }

    #[tokio::test]
    async fn complete_multichunk_json_is_parsed_without_content_type() {
        let diagnostic = read_error_diagnostic(stream::iter([
            Ok(Bytes::from_static(
                br#"{"error":{"type":"invalid_request_error","message":"Messages "#,
            )),
            Ok(Bytes::from_static(
                br#"must not be empty: private-prompt"}}"#,
            )),
        ]))
        .await;
        assert_eq!(diagnostic.body_state, DiagnosticBodyState::Complete);
        assert_eq!(
            diagnostic.summary,
            Some("upstream rejected the message structure")
        );
    }

    #[tokio::test]
    async fn empty_invalid_encoding_and_non_json_are_distinct() {
        for (body, expected) in [
            (Bytes::new(), DiagnosticBodyState::Empty),
            (Bytes::from_static(b" \r\n\t"), DiagnosticBodyState::Empty),
            (
                Bytes::from_static(&[0xff]),
                DiagnosticBodyState::InvalidEncoding,
            ),
            (
                Bytes::from_static(b"<html>secret-error</html>"),
                DiagnosticBodyState::InvalidJson,
            ),
            (
                Bytes::from_static(br#"{"error":"unfinished"#),
                DiagnosticBodyState::InvalidJson,
            ),
        ] {
            let diagnostic = read_error_diagnostic(stream::iter([Ok(body)])).await;
            assert_eq!(diagnostic, UpstreamDiagnostic::without_details(expected));
        }
    }

    #[tokio::test]
    async fn oversized_first_chunk_is_not_drained_or_parsed() {
        let polls = Arc::new(AtomicUsize::new(0));
        let count = polls.clone();
        let stream = stream::iter([
            Ok(Bytes::from(vec![b'x'; MAX_DIAGNOSTIC_BODY_BYTES * 8])),
            Err("private-transport-error".to_owned()),
        ])
        .inspect(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(
            read_error_diagnostic(stream).await,
            UpstreamDiagnostic::without_details(DiagnosticBodyState::Truncated)
        );
        assert_eq!(polls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn multichunk_bound_is_total_and_never_parses_partial_prefix() {
        let polls = Arc::new(AtomicUsize::new(0));
        let count = polls.clone();
        let stream = stream::iter([
            Ok(Bytes::from_static(
                br#"{"error":{"code":"rate_limit_exceeded"}}"#,
            )),
            Ok(Bytes::from(vec![b' '; MAX_DIAGNOSTIC_BODY_BYTES])),
            Err("private-transport-error".to_owned()),
        ])
        .inspect(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(
            read_error_diagnostic(stream).await,
            UpstreamDiagnostic::without_details(DiagnosticBodyState::Truncated)
        );
        assert_eq!(polls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn exact_byte_bound_is_conservatively_truncated() {
        let body = Bytes::from(vec![b' '; MAX_DIAGNOSTIC_BODY_BYTES]);
        assert_eq!(
            read_error_diagnostic(stream::iter([Ok(body)]))
                .await
                .body_state,
            DiagnosticBodyState::Truncated
        );
    }

    #[tokio::test]
    async fn interrupted_body_does_not_trust_an_otherwise_valid_prefix() {
        let diagnostic = read_error_diagnostic(stream::iter([
            Ok(Bytes::from_static(
                br#"{"error":{"code":"rate_limit_exceeded"}}"#,
            )),
            Err("private-transport-error".to_owned()),
        ]))
        .await;
        assert_eq!(
            diagnostic,
            UpstreamDiagnostic::without_details(DiagnosticBodyState::ReadError)
        );
    }

    #[tokio::test]
    async fn stalled_stream_has_a_bounded_deadline() {
        let diagnostic = tokio::time::timeout(
            Duration::from_secs(1),
            read_error_diagnostic(stream::pending()),
        )
        .await
        .expect("diagnostic read must not retain the upstream indefinitely");
        assert_eq!(
            diagnostic,
            UpstreamDiagnostic::without_details(DiagnosticBodyState::DeadlineExceeded)
        );
    }

    #[tokio::test]
    async fn progress_does_not_restart_the_total_deadline() {
        let stream = Box::pin(stream::unfold((), |()| async {
            tokio::time::sleep(Duration::from_millis(60)).await;
            Some((Ok(Bytes::from_static(b" ")), ()))
        }));
        let diagnostic =
            tokio::time::timeout(Duration::from_millis(500), read_error_diagnostic(stream))
                .await
                .expect("progress must not extend the total diagnostic deadline");
        assert_eq!(diagnostic.body_state, DiagnosticBodyState::DeadlineExceeded);
    }

    struct PendingUntilDropped(Arc<AtomicBool>);

    impl Stream for PendingUntilDropped {
        type Item = Result<Bytes, String>;

        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Pending
        }
    }

    impl Drop for PendingUntilDropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn caller_cancellation_drops_the_owned_stream() {
        let dropped = Arc::new(AtomicBool::new(false));
        let mut reader = Box::pin(read_error_diagnostic(PendingUntilDropped(dropped.clone())));
        assert!(futures::poll!(&mut reader).is_pending());
        assert!(!dropped.load(Ordering::SeqCst));
        drop(reader);
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn immediately_ready_empty_chunks_cannot_starve_cancellation() {
        let mut reader = Box::pin(read_error_diagnostic(stream::repeat(Ok(Bytes::new()))));
        assert!(
            futures::poll!(&mut reader).is_pending(),
            "a continuously ready stream must yield so the caller can cancel"
        );
        drop(reader);
    }
}
