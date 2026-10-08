//! Translated from Pi v1.0.4 `packages/ai/test/retry.test.ts`.

use std::convert::Infallible;
use std::future::{Future, ready};
use std::sync::{Arc, Mutex};

use pi_ai::env::CancellationToken;
use pi_ai::types::{AssistantMessage, StopReason};
use pi_ai::utils::js_value::JsString;
use pi_ai::utils::retry::{
    RetryCallbackFuture, RetryCallbacks, RetryPolicy, is_retryable_assistant_error,
    retry_assistant_call, retry_delay_ms,
};
use pi_testkit::env::VirtualEnv;
use serde_json::json;

fn message(text: &str, stop_reason: &str, error: Option<&str>) -> AssistantMessage {
    let mut value = json!({
        "role": "assistant",
        "content": [{ "type": "text", "text": text }],
        "api": "faux", "provider": "faux", "model": "faux-1",
        "usage": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0,
            "totalTokens": 0, "cost": { "input": 0, "output": 0, "cacheRead": 0,
                "cacheWrite": 0, "total": 0 } },
        "stopReason": stop_reason, "timestamp": 0,
    });
    if let Some(error) = error {
        value["errorMessage"] = json!(error);
    }
    serde_json::from_value(value).unwrap()
}

fn error_message(error: &str) -> AssistantMessage {
    message("", "error", Some(error))
}

fn policy(enabled: bool) -> RetryPolicy {
    RetryPolicy {
        enabled,
        max_retries: 3.0,
        base_delay_ms: 0.0,
        max_agent_delay_ms: None,
    }
}

#[derive(Default)]
struct Callbacks {
    scheduled: Vec<(f64, f64, f64, JsString)>,
    started: usize,
    finished: Vec<(bool, f64, Option<JsString>)>,
    events: Option<Arc<Mutex<Vec<String>>>>,
}

impl RetryCallbacks<Infallible> for Callbacks {
    fn on_retry_scheduled(
        &mut self,
        attempt: f64,
        max_attempts: f64,
        delay: f64,
        error: &JsString,
    ) -> RetryCallbackFuture<'_, Infallible> {
        self.scheduled
            .push((attempt, max_attempts, delay, error.to_owned()));
        if let Some(events) = &self.events {
            events.lock().unwrap().push(format!("retry:{attempt}"));
        }
        Box::pin(async { Ok(()) })
    }

    fn on_retry_attempt_start(&mut self) -> RetryCallbackFuture<'_, Infallible> {
        self.started += 1;
        if let Some(events) = &self.events {
            events.lock().unwrap().push("attempt-start".to_owned());
        }
        Box::pin(async { Ok(()) })
    }

    fn on_retry_finished(
        &mut self,
        success: bool,
        attempt: f64,
        error: Option<&JsString>,
    ) -> RetryCallbackFuture<'_, Infallible> {
        self.finished.push((success, attempt, error.cloned()));
        Box::pin(async { Ok(()) })
    }
}

async fn drive<F: Future<Output = Result<AssistantMessage, Infallible>>>(
    env: &VirtualEnv,
    call: F,
) -> AssistantMessage {
    tokio::pin!(call);
    // Advance only this explicit virtual clock, on the same runtime thread as
    // the call. No wall-clock sleep or Tokio auto-advance is used.
    for _ in 0..32 {
        tokio::select! {
            biased;
            result = &mut call => return result.unwrap(),
            () = env.advance(60_000) => {},
        }
    }
    panic!("retry call did not settle within its test budget");
}

// The double underscore separates an expanded `it.each` case from its title.
#[allow(non_snake_case)]
mod provider_retry_classification {
    use super::*;

    #[test]
    fn matches_explicit_provider_retry_guidance() {
        let open_ai = "An error occurred while processing your request. You can retry your request, or contact us through our help center at help.openai.com if the error persists. Please include the request ID req_******** in your message.";
        let bedrock = r#"{"message":"The system encountered an unexpected error during processing. Try your request again."}"#;
        let nvidia = "ResourceExhausted: Worker local total request limit reached (288/48)";
        assert!(is_retryable_assistant_error(&error_message(open_ai)));
        assert!(is_retryable_assistant_error(&error_message(bedrock)));
        assert!(is_retryable_assistant_error(&error_message(nvidia)));
    }

    #[test]
    fn matches_bun_fetch_socket_drop_wording() {
        assert!(is_retryable_assistant_error(&error_message(
            "The socket connection was closed unexpectedly. For more information, pass `verbose: true` in the second argument to fetch()"
        )));
    }

    #[test]
    fn matches_upstream_request_buffer_exhaustion_wording() {
        assert!(is_retryable_assistant_error(&error_message(
            "Error: exceeded request buffer limit while retrying upstream"
        )));
    }

    #[test]
    fn matches_dns_transport_failure_wording__the_pending_stream_has_been_canceled_caused_by_getaddrinfo_enotfound_bedrock_runtime_us_east_1_amazonaws_com()
     {
        assert!(is_retryable_assistant_error(&error_message(
            "The pending stream has been canceled (caused by: getaddrinfo ENOTFOUND bedrock-runtime.us-east-1.amazonaws.com)"
        )));
    }

    #[test]
    fn matches_dns_transport_failure_wording__connect_enotfound_api_example_com() {
        assert!(is_retryable_assistant_error(&error_message(
            "connect ENOTFOUND api.example.com"
        )));
    }

    #[test]
    fn matches_dns_transport_failure_wording__eai_again_api_example_com() {
        assert!(is_retryable_assistant_error(&error_message(
            "EAI_AGAIN api.example.com"
        )));
    }

    #[test]
    fn matches_dns_transport_failure_wording__getaddrinfo_failed_for_api_example_com() {
        assert!(is_retryable_assistant_error(&error_message(
            "getaddrinfo failed for api.example.com"
        )));
    }

    #[test]
    fn matches_http_2_pending_stream_cancellation__the_pending_stream_has_been_canceled() {
        assert!(is_retryable_assistant_error(&error_message(
            "The pending stream has been canceled"
        )));
    }

    #[test]
    fn matches_http_2_pending_stream_cancellation__the_pending_stream_has_been_canceled_caused_by_socket_closed()
     {
        assert!(is_retryable_assistant_error(&error_message(
            "The pending stream has been canceled (caused by: socket closed)"
        )));
    }

    #[test]
    fn matches_openai_responses_streams_that_end_before_terminal_events() {
        assert!(is_retryable_assistant_error(&error_message(
            "OpenAI Responses stream ended before a terminal response event"
        )));
    }

    #[test]
    fn matches_azure_peak_load_capacity_errors() {
        assert!(is_retryable_assistant_error(&error_message(
            "The system is currently experiencing high demand and cannot process your request. Your request exceeds the maximum usage size allowed during peak load. For improved capacity reliability, consider switching to Provisioned Throughput."
        )));
    }

    #[test]
    fn keeps_provider_limit_errors_non_retryable() {
        assert!(!is_retryable_assistant_error(&error_message(
            "429 quota exceeded"
        )));
    }

    #[test]
    fn keeps_the_chatgpt_subscription_usage_limit_non_retryable() {
        assert!(!is_retryable_assistant_error(&error_message(
            r#"OpenAI API error (429): {"code":"subscription_sharing_usage_limit_exceeded","message":"Usage limit reached."}"#
        )));
    }

    #[test]
    fn retries_temporary_chatgpt_subscription_errors__subscription_sharing_usage_unavailable_usage_cannot_be_checked()
     {
        assert!(is_retryable_assistant_error(&error_message(
            "subscription_sharing_usage_unavailable: Usage cannot be checked."
        )));
    }

    #[test]
    fn retries_temporary_chatgpt_subscription_errors__subscription_sharing_user_unavailable_user_cannot_be_loaded()
     {
        assert!(is_retryable_assistant_error(&error_message(
            "subscription_sharing_user_unavailable: User cannot be loaded."
        )));
    }

    #[test]
    fn classifies_assistant_error_messages() {
        assert!(is_retryable_assistant_error(&error_message(
            "overloaded_error"
        )));
        assert!(is_retryable_assistant_error(&error_message(
            "520 status code (no body)"
        )));
        assert!(is_retryable_assistant_error(&error_message(
            "524 status code (no body)"
        )));
        assert!(!is_retryable_assistant_error(&message(
            "not an error",
            "stop",
            None
        )));
    }
}

mod retry_delay_ms {
    use super::*;

    #[test]
    fn caps_agent_retry_delay() {
        let mut policy = policy(true);
        policy.base_delay_ms = 2_000.0;
        assert_eq!(retry_delay_ms(&policy, 6.0), 60_000.0);
        policy.max_agent_delay_ms = Some(5_000.0);
        assert_eq!(retry_delay_ms(&policy, 5.0), 5_000.0);
        policy.max_agent_delay_ms = Some(0.0);
        assert_eq!(retry_delay_ms(&policy, 5.0), 0.0);
    }
}

mod retry_assistant_call {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn returns_a_successful_response_immediately_without_retrying() {
        let env = VirtualEnv::default();
        let mut calls = 0;
        let produce = || {
            calls += 1;
            ready(Ok(message("ok", "stop", None)))
        };
        let res = drive(
            &env,
            retry_assistant_call(produce, Some(&policy(true)), None, None, &env),
        )
        .await;
        assert_eq!(
            serde_json::to_value(res.content).unwrap(),
            json!([{ "type": "text", "text": "ok" }])
        );
        assert_eq!(calls, 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn does_not_retry_an_aborted_message() {
        let env = VirtualEnv::default();
        let mut calls = 0;
        let produce = || {
            calls += 1;
            ready(Ok(message("", "aborted", None)))
        };
        let mut callbacks = Callbacks::default();
        let res = drive(
            &env,
            retry_assistant_call(
                produce,
                Some(&policy(true)),
                None,
                Some(&mut callbacks),
                &env,
            ),
        )
        .await;
        assert_eq!(res.stop_reason, StopReason::Aborted);
        assert_eq!(calls, 1);
        assert!(callbacks.scheduled.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn does_not_retry_a_non_retryable_error_quota_billing() {
        let env = VirtualEnv::default();
        let mut calls = 0;
        let produce = || {
            calls += 1;
            ready(Ok(error_message("insufficient_quota")))
        };
        let mut callbacks = Callbacks::default();
        let res = drive(
            &env,
            retry_assistant_call(
                produce,
                Some(&policy(true)),
                None,
                Some(&mut callbacks),
                &env,
            ),
        )
        .await;
        assert_eq!(res.stop_reason, StopReason::Error);
        assert_eq!(calls, 1);
        assert!(callbacks.scheduled.is_empty());
        assert!(callbacks.finished.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn retries_a_transient_error_up_to_maxretries_then_returns_the_final_error() {
        let env = VirtualEnv::default();
        let mut calls = 0;
        let produce = || {
            calls += 1;
            ready(Ok(error_message("terminated")))
        };
        let mut callbacks = Callbacks::default();
        let res = drive(
            &env,
            retry_assistant_call(
                produce,
                Some(&policy(true)),
                None,
                Some(&mut callbacks),
                &env,
            ),
        )
        .await;
        assert_eq!(res.stop_reason, StopReason::Error);
        assert_eq!(calls, 4);
        assert_eq!(callbacks.scheduled.len(), 3);
        assert_eq!(
            callbacks.finished,
            [(false, 3.0, Some("terminated".into()))]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reports_capped_retry_delays() {
        let env = VirtualEnv::default();
        let mut calls = 0;
        let policy = RetryPolicy {
            enabled: true,
            max_retries: 4.0,
            base_delay_ms: 10.0,
            max_agent_delay_ms: Some(15.0),
        };
        let produce = || {
            calls += 1;
            ready(Ok(if calls < 5 {
                error_message("terminated")
            } else {
                message("recovered", "stop", None)
            }))
        };
        let mut callbacks = Callbacks::default();
        drive(
            &env,
            retry_assistant_call(produce, Some(&policy), None, Some(&mut callbacks), &env),
        )
        .await;
        assert_eq!(
            callbacks
                .scheduled
                .iter()
                .map(|call| call.2)
                .collect::<Vec<_>>(),
            [10.0, 15.0, 15.0, 15.0]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stops_retrying_once_a_call_succeeds() {
        let env = VirtualEnv::default();
        let mut calls = 0;
        let produce = || {
            calls += 1;
            ready(Ok(if calls < 3 {
                error_message("terminated")
            } else {
                message("recovered", "stop", None)
            }))
        };
        let mut callbacks = Callbacks::default();
        let res = drive(
            &env,
            retry_assistant_call(
                produce,
                Some(&policy(true)),
                None,
                Some(&mut callbacks),
                &env,
            ),
        )
        .await;
        assert_eq!(
            serde_json::to_value(res.content).unwrap(),
            json!([{ "type": "text", "text": "recovered" }])
        );
        assert_eq!(calls, 3);
        assert_eq!(callbacks.finished, [(true, 2.0, None)]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reports_an_aborted_retried_call_as_unsuccessful() {
        let env = VirtualEnv::default();
        let mut calls = 0;
        let produce = || {
            calls += 1;
            ready(Ok(if calls == 1 {
                error_message("terminated")
            } else {
                message("", "aborted", None)
            }))
        };
        let mut callbacks = Callbacks::default();
        let res = drive(
            &env,
            retry_assistant_call(
                produce,
                Some(&policy(true)),
                None,
                Some(&mut callbacks),
                &env,
            ),
        )
        .await;
        assert_eq!(res.stop_reason, StopReason::Aborted);
        assert_eq!(calls, 2);
        assert_eq!(callbacks.finished, [(false, 1.0, None)]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn does_not_retry_when_policy_is_disabled() {
        let env = VirtualEnv::default();
        let mut calls = 0;
        let produce = || {
            calls += 1;
            ready(Ok(error_message("terminated")))
        };
        let mut callbacks = Callbacks::default();
        let res = drive(
            &env,
            retry_assistant_call(
                produce,
                Some(&policy(false)),
                None,
                Some(&mut callbacks),
                &env,
            ),
        )
        .await;
        assert_eq!(res.stop_reason, StopReason::Error);
        assert_eq!(calls, 1);
        assert!(callbacks.scheduled.is_empty());
        assert!(callbacks.finished.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn emits_onretryattemptstart_after_backoff_before_each_retried_call() {
        let env = VirtualEnv::default();
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut calls = 0;
        let produce = || {
            events.lock().unwrap().push(format!("produce:{calls}"));
            calls += 1;
            ready(Ok(if calls < 3 {
                error_message("terminated")
            } else {
                message("recovered", "stop", None)
            }))
        };
        let mut callbacks = Callbacks {
            events: Some(Arc::clone(&events)),
            ..Callbacks::default()
        };
        let res = drive(
            &env,
            retry_assistant_call(
                produce,
                Some(&policy(true)),
                None,
                Some(&mut callbacks),
                &env,
            ),
        )
        .await;
        assert_eq!(
            serde_json::to_value(res.content).unwrap(),
            json!([{ "type": "text", "text": "recovered" }])
        );
        assert_eq!(callbacks.scheduled.len(), 2);
        assert_eq!(callbacks.started, 2);
        assert_eq!(
            *events.lock().unwrap(),
            [
                "produce:0",
                "retry:1",
                "attempt-start",
                "produce:1",
                "retry:2",
                "attempt-start",
                "produce:2"
            ]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn aborts_backoff_sleep_via_signal_returns_an_aborted_message_and_emits_onretryfinished_false()
     {
        let env = VirtualEnv::default();
        let controller = CancellationToken::new();
        let mut calls = 0;
        let produce = || {
            calls += 1;
            ready(Ok::<_, Infallible>(error_message("terminated")))
        };
        let policy = RetryPolicy {
            enabled: true,
            max_retries: 5.0,
            base_delay_ms: 10_000.0,
            max_agent_delay_ms: None,
        };
        let mut callbacks = Callbacks::default();
        let abort = async {
            while env.pending_timers() == 0 {
                tokio::task::yield_now().await;
            }
            controller.cancel();
        };
        let (res, ()) = tokio::join!(
            retry_assistant_call(
                produce,
                Some(&policy),
                Some(&controller),
                Some(&mut callbacks),
                &env
            ),
            abort
        );
        let res = res.unwrap();
        assert_eq!(res.stop_reason, StopReason::Aborted);
        assert_eq!(res.error_message, None);
        assert_eq!(calls, 1);
        assert_eq!(
            callbacks.finished,
            [(false, 1.0, Some("terminated".into()))]
        );
    }
}

// These focused checks protect the Rust boundary adaptations in addition to the
// upstream assertions above: JavaScript numbers, regex semantics, and rejected
// asynchronous callbacks are observable by callers.
mod language_contracts {
    use super::*;

    #[test]
    fn backoff_preserves_javascript_safe_integer_and_min_semantics() {
        let mut policy = policy(true);
        policy.base_delay_ms = 0.5;
        assert_eq!(retry_delay_ms(&policy, 1.0), 60_000.0);
        assert_eq!(retry_delay_ms(&policy, 2.0), 1.0);
        policy.base_delay_ms = 2_000.0;
        assert_eq!(retry_delay_ms(&policy, 1_024.0), 60_000.0);
        assert_eq!(retry_delay_ms(&policy, f64::NAN), 60_000.0);
        policy.max_agent_delay_ms = Some(f64::NAN);
        assert!(retry_delay_ms(&policy, 1.0).is_nan());
        policy.base_delay_ms = 0.0;
        policy.max_agent_delay_ms = Some(-0.0);
        assert!(retry_delay_ms(&policy, 1.0).is_sign_negative());
        policy.base_delay_ms = -1.0;
        policy.max_agent_delay_ms = None;
        assert_eq!(retry_delay_ms(&policy, 2.0), -2.0);
    }

    #[test]
    fn classification_preserves_javascript_non_unicode_regex_behavior() {
        assert!(is_retryable_assistant_error(&error_message("RaTe-LiMiT")));
        assert!(is_retryable_assistant_error(&error_message("rateélimit")));
        assert!(!is_retryable_assistant_error(&error_message("rate😀limit")));
        assert!(!is_retryable_assistant_error(&error_message("rate\rlimit")));
        assert!(!is_retryable_assistant_error(&error_message("rate\nlimit")));
        assert!(!is_retryable_assistant_error(&error_message(
            "rate\u{2028}limit"
        )));
        assert!(!is_retryable_assistant_error(&error_message(
            "rate\u{2029}limit"
        )));
        assert!(!is_retryable_assistant_error(&error_message(
            "ſervice unavailable"
        )));
        assert!(!is_retryable_assistant_error(&error_message(
            "socKet hang up"
        )));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn producer_rejection_is_propagated_without_retry() {
        let env = VirtualEnv::default();
        let mut calls = 0;
        let produce = || {
            calls += 1;
            ready(Err::<AssistantMessage, _>("producer rejected"))
        };
        let result = retry_assistant_call(produce, Some(&policy(true)), None, None, &env).await;
        assert_eq!(result.unwrap_err(), "producer rejected");
        assert_eq!(calls, 1);
        assert_eq!(env.pending_timers(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn callbacks_are_awaited_and_rejections_stop_the_loop() {
        struct RejectingCallbacks(Arc<Mutex<Vec<&'static str>>>);
        impl RetryCallbacks<&'static str> for RejectingCallbacks {
            fn on_retry_scheduled(
                &mut self,
                _: f64,
                _: f64,
                _: f64,
                _: &JsString,
            ) -> RetryCallbackFuture<'_, &'static str> {
                Box::pin(async {
                    self.0.lock().unwrap().push("scheduled:started");
                    tokio::task::yield_now().await;
                    self.0.lock().unwrap().push("scheduled:rejected");
                    Err("callback rejected")
                })
            }
            fn on_retry_attempt_start(&mut self) -> RetryCallbackFuture<'_, &'static str> {
                self.0.lock().unwrap().push("attempt:started");
                Box::pin(async { Ok(()) })
            }
        }
        let env = VirtualEnv::default();
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut callbacks = RejectingCallbacks(Arc::clone(&events));
        let produce = || {
            events.lock().unwrap().push("produce");
            ready(Ok(error_message("terminated")))
        };
        let result = retry_assistant_call(
            produce,
            Some(&policy(true)),
            None,
            Some(&mut callbacks),
            &env,
        )
        .await;
        assert_eq!(result.unwrap_err(), "callback rejected");
        assert_eq!(
            *events.lock().unwrap(),
            ["produce", "scheduled:started", "scheduled:rejected"]
        );
        assert_eq!(env.pending_timers(), 0);
    }
}
