use super::*;
use pi_ai::{
    AssistantMessage, StopReason, SystemMessage, ThinkingContent, Tool, ToolResultMessage,
    UserMessage, is_context_overflow, is_retryable_error,
};
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// One scripted reply of the fake transport.
enum Reply {
    Response {
        status: u16,
        headers: Vec<(&'static str, &'static str)>,
        chunks: Vec<Result<&'static str, maple_sdk::Error>>,
        /// Hold the body open after the chunks instead of ending it.
        stall_after: bool,
    },
    Fail(maple_sdk::Error),
    /// Never answer; settle only once the request is cancelled.
    Stall,
}

fn sse(status: u16, chunks: Vec<&'static str>) -> Reply {
    Reply::Response {
        status,
        headers: Vec::new(),
        chunks: chunks.into_iter().map(Ok).collect(),
        stall_after: false,
    }
}

#[derive(Debug)]
struct Captured {
    method: String,
    uri: String,
    accept: Option<String>,
    body: Value,
}

/// An inference transport that replays scripted replies and records what it
/// was sent and whether each send settled.
#[derive(Default)]
struct FakeTransport {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<Captured>>,
    saw_cancel: AtomicBool,
    settled: AtomicUsize,
}

impl FakeTransport {
    fn new(replies: Vec<Reply>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into()),
            ..Self::default()
        })
    }
}

#[async_trait]
impl MapleInferenceTransport for FakeTransport {
    async fn send_inference_request(
        self: Arc<Self>,
        request: InferenceRequest,
        cancel_token: CancellationToken,
    ) -> maple_sdk::Result<InferenceResponse> {
        self.requests.lock().unwrap().push(Captured {
            method: request.method().to_string(),
            uri: request.uri().to_string(),
            accept: request
                .headers()
                .get(http::header::ACCEPT)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string),
            body: serde_json::from_slice(request.body()).unwrap_or(Value::Null),
        });
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("a scripted reply");
        let result = match reply {
            Reply::Stall => {
                cancel_token.cancelled().await;
                self.saw_cancel.store(true, Ordering::SeqCst);
                // Like the real session, finish reconciling before returning.
                tokio::task::yield_now().await;
                Err(maple_sdk::Error::Other("cancelled".into()))
            }
            Reply::Fail(error) => Err(error),
            Reply::Response {
                status,
                headers,
                chunks,
                stall_after,
            } => {
                let chunks: Vec<maple_sdk::Result<Bytes>> = chunks
                    .into_iter()
                    .map(|chunk| chunk.map(|text| Bytes::from_static(text.as_bytes())))
                    .collect();
                let body = futures_util::stream::iter(chunks);
                let body: OpenSecretResponseBody = if stall_after {
                    Box::pin(body.chain(futures_util::stream::pending()))
                } else {
                    Box::pin(body)
                };
                let mut response = http::Response::new(body);
                *response.status_mut() = http::StatusCode::from_u16(status).unwrap();
                for (name, value) in headers {
                    response
                        .headers_mut()
                        .append(name, http::HeaderValue::from_static(value));
                }
                Ok(response)
            }
        };
        self.settled.fetch_add(1, Ordering::SeqCst);
        result
    }
}

fn model(id: &str) -> Model {
    maple_model(id, None, None)
}

fn context(text: &str) -> Context {
    Context::new(
        "You are Maple.",
        Vec::new(),
        vec![Message::User(UserMessage {
            content: vec![Content::text(text)],
            timestamp: 1,
        })],
    )
}

async fn respond(transport: Arc<FakeTransport>, model: &Model) -> AssistantMessage {
    MapleStreamFn::new(transport)
        .stream(model, context("hi"), StreamOptions::default())
        .result()
        .await
}

async fn failure(reply: Reply) -> AssistantMessage {
    let message = respond(FakeTransport::new(vec![reply]), &model("glm-5-3")).await;
    assert_eq!(message.stop_reason, StopReason::Error, "{message:?}");
    message
}

const TEXT_STREAM: [&str; 3] = [
    "data: {\"id\":\"r1\",\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2}}\n\n",
    "data: [DONE]\n\n",
];

#[tokio::test]
async fn requests_go_to_chat_completions_and_stream_back() {
    let transport = FakeTransport::new(vec![sse(200, TEXT_STREAM.to_vec())]);
    let message = respond(transport.clone(), &model("glm-5-3")).await;
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.text(), "Hello");
    assert_eq!(message.usage.input, 10);
    assert_eq!(message.model, "glm-5-3");

    let requests = transport.requests.lock().unwrap();
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.uri, CHAT_COMPLETIONS_PATH);
    assert_eq!(request.accept.as_deref(), Some("text/event-stream"));
    assert_eq!(request.body["model"], "glm-5-3");
    assert_eq!(request.body["stream"], true);
    // The first version sends no thinking control and no output cap.
    assert!(request.body.get("reasoning_effort").is_none());
    assert!(request.body.get("max_tokens").is_none());
    assert!(request.body.get("max_completion_tokens").is_none());
}

#[tokio::test]
async fn maple_preserves_explicit_output_while_pi_keeps_its_context_clamp() {
    for field in [
        MaxTokensField::MaxTokens,
        MaxTokensField::MaxCompletionTokens,
    ] {
        let mut model = model("glm-5-3");
        model.context_window = 4_096;
        model.compat.max_tokens_field = field;
        let context = Context::new(
            "You are Maple.",
            vec![Tool::new("read", "Read a file", json!({"type": "object"}))],
            context(&"x".repeat(12_000)).messages[1..].to_vec(),
        );
        let options = StreamOptions {
            max_tokens: Some(2_000),
            ..StreamOptions::default()
        };
        let mut expected = pi_ai::openai::build_request_body(&model, &context, &options).unwrap();
        let (field, alternate) = match field {
            MaxTokensField::MaxTokens => ("max_tokens", "max_completion_tokens"),
            MaxTokensField::MaxCompletionTokens => ("max_completion_tokens", "max_tokens"),
        };
        let clamped = expected[field].as_u64().unwrap();
        assert!(
            clamped > 0 && clamped < 2_000,
            "Pi retains its original clamp"
        );
        expected[field] = json!(2_000);

        let transport = FakeTransport::new(vec![sse(200, TEXT_STREAM.to_vec())]);
        let message = MapleStreamFn::new(transport.clone())
            .stream(&model, context, options)
            .result()
            .await;
        assert_eq!(message.stop_reason, StopReason::Stop);
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests[0].body, expected, "only the requested cap changes");
        assert!(requests[0].body.get(alternate).is_none());
        assert!(requests[0].body.get("tools").is_some());
    }
}

#[tokio::test]
async fn explicit_output_preservation_runs_after_the_existing_payload_hook() {
    for field in [
        MaxTokensField::MaxTokens,
        MaxTokensField::MaxCompletionTokens,
    ] {
        let mut model = model("glm-5-3");
        model.context_window = 4_096;
        model.compat.max_tokens_field = field;
        let hook_calls = Arc::new(AtomicUsize::new(0));
        let calls = hook_calls.clone();
        let options = StreamOptions {
            max_tokens: Some(2_000),
            on_payload: Some(Arc::new(move |mut payload| {
                calls.fetch_add(1, Ordering::SeqCst);
                payload["max_tokens"] = json!(1);
                payload["max_completion_tokens"] = json!(2);
                payload["temperature"] = json!(0.25);
                Box::pin(async move { payload })
            })),
            ..StreamOptions::default()
        };
        let transport = FakeTransport::new(vec![sse(200, TEXT_STREAM.to_vec())]);
        let message = MapleStreamFn::new(transport.clone())
            .stream(&model, context(&"x".repeat(12_000)), options)
            .result()
            .await;
        assert_eq!(message.stop_reason, StopReason::Stop);
        assert_eq!(hook_calls.load(Ordering::SeqCst), 1);
        let requests = transport.requests.lock().unwrap();
        let body = &requests[0].body;
        assert_eq!(body["temperature"], 0.25, "existing hook changes survive");
        let (field, alternate) = match field {
            MaxTokensField::MaxTokens => ("max_tokens", "max_completion_tokens"),
            MaxTokensField::MaxCompletionTokens => ("max_completion_tokens", "max_tokens"),
        };
        assert_eq!(body[field], 2_000);
        assert!(
            body.get(alternate).is_none(),
            "never send competing cap fields"
        );
    }
}

#[tokio::test]
async fn default_and_zero_output_limits_keep_pis_payload_and_existing_hook() {
    for default in [0, 2_000] {
        for requested in [None, Some(0)] {
            let mut model = model("glm-5-3");
            model.context_window = 4_096;
            model.max_tokens = default;
            let context = context(&"x".repeat(12_000));
            let hook_calls = Arc::new(AtomicUsize::new(0));
            let calls = hook_calls.clone();
            let options = StreamOptions {
                max_tokens: requested,
                on_payload: Some(Arc::new(move |mut payload| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    payload["temperature"] = json!(0.25);
                    Box::pin(async move { payload })
                })),
                ..StreamOptions::default()
            };
            let mut expected =
                pi_ai::openai::build_request_body(&model, &context, &options).unwrap();
            expected["temperature"] = json!(0.25);
            let transport = FakeTransport::new(vec![sse(200, TEXT_STREAM.to_vec())]);
            let message = MapleStreamFn::new(transport.clone())
                .stream(&model, context, options)
                .result()
                .await;
            assert_eq!(message.stop_reason, StopReason::Stop);
            assert_eq!(hook_calls.load(Ordering::SeqCst), 1);
            assert_eq!(transport.requests.lock().unwrap()[0].body, expected);
        }
    }
}

#[tokio::test]
async fn an_explicit_output_limit_keeps_existing_inflight_cancellation() {
    let transport = FakeTransport::new(vec![Reply::Stall]);
    let cancel = CancellationToken::new();
    let stream = MapleStreamFn::new(transport.clone()).stream(
        &model("glm-5-3"),
        context("hi"),
        StreamOptions {
            max_tokens: Some(2_000),
            cancel: cancel.clone(),
            ..StreamOptions::default()
        },
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        while transport.requests.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    cancel.cancel();
    let message = stream.result().await;
    assert_eq!(message.stop_reason, StopReason::Aborted);
    tokio::time::timeout(Duration::from_secs(1), async {
        while transport.settled.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(transport.saw_cancel.load(Ordering::SeqCst));
    assert_eq!(
        transport.requests.lock().unwrap()[0].body["max_tokens"],
        2_000
    );
}

#[tokio::test]
async fn a_server_overflow_with_an_explicit_output_limit_keeps_pis_classification() {
    let transport = FakeTransport::new(vec![json_reply(
        413,
        Vec::new(),
        "{\"error\":{\"message\":\"context is too large\"}}",
    )]);
    let message = MapleStreamFn::new(transport.clone())
        .stream(
            &model("glm-5-3"),
            context("hi"),
            StreamOptions {
                max_tokens: Some(2_000),
                ..StreamOptions::default()
            },
        )
        .result()
        .await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert!(is_context_overflow(&message, None));
    assert!(!is_retryable_error(&message));
    assert_eq!(
        transport.requests.lock().unwrap()[0].body["max_tokens"],
        2_000
    );
}

fn json_reply(
    status: u16,
    headers: Vec<(&'static str, &'static str)>,
    body: &'static str,
) -> Reply {
    Reply::Response {
        status,
        headers,
        chunks: vec![Ok(body)],
        stall_after: false,
    }
}

#[tokio::test]
async fn a_response_carries_its_headers_to_pi() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let transport = FakeTransport::new(vec![Reply::Response {
        status: 200,
        headers: vec![("X-Request-Id", "r1")],
        chunks: TEXT_STREAM.iter().map(|chunk| Ok(*chunk)).collect(),
        stall_after: false,
    }]);
    let options = StreamOptions {
        on_response: Some(Arc::new(move |response| {
            sink.lock().unwrap().push(response);
            Box::pin(async {})
        })),
        ..StreamOptions::default()
    };
    let message = MapleStreamFn::new(transport)
        .stream(&model("glm-5-3"), context("hi"), options)
        .result()
        .await;
    assert_eq!(message.stop_reason, StopReason::Stop);
    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].status, 200);
    assert_eq!(
        seen[0].headers.get("x-request-id").map(String::as_str),
        Some("r1")
    );
}

const SYSTEM_ONE_REPLY: &str = r#"{"id":"so_1","model":"glm-5-3-flash","answers":{"urgent":{"type":"noul","noul":0.9,"temperature":1.0,"option_mass":0.98}},"usage":{"input_tokens":120,"output_tokens":1,"cached_tokens":20,"requests":1}}"#;

fn urgent_ticket() -> pi_ai::ClassifierContext {
    pi_ai::ClassifierContext {
        state: json!({ "ticket": "Charged twice" }),
        images: Vec::new(),
        questions: [(
            "urgent".to_string(),
            pi_ai::ClassifierQuestion::bool("Is it urgent?"),
        )]
        .into_iter()
        .collect(),
    }
}

#[tokio::test]
async fn system_one_classifies_over_the_accounts_transport() {
    let transport = FakeTransport::new(vec![
        // A rate limit that names its wait is sent again after it.
        json_reply(
            429,
            vec![("retry-after-ms", "1")],
            r#"{"error":{"message":"slow down"}}"#,
        ),
        json_reply(200, Vec::new(), SYSTEM_ONE_REPLY),
    ]);
    let registry = maple_model_registry(transport.clone(), []);
    let model = registry
        .find_classifier(MAPLE_PROVIDER, SYSTEM_ONE_MODEL_ID)
        .expect("System One is registered");
    assert!(model.supports_images());
    let result = registry
        .classify(&model, urgent_ticket(), pi_ai::ClassifierOptions::default())
        .await;
    assert!(!result.is_failure(), "{result:?}");
    assert!(matches!(
        result.answers["urgent"],
        pi_ai::ClassifierAnswer::Bool { probability, .. } if probability == 0.9
    ));
    assert_eq!(result.response_id.as_deref(), Some("so_1"));
    let usage = result.usage.expect("usage");
    assert_eq!((usage.input, usage.cache_read, usage.output), (100, 20, 1));

    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        assert_eq!(request.method, "POST");
        assert_eq!(request.uri, SYSTEM_ONE_PATH);
        assert_eq!(request.accept.as_deref(), Some("application/json"));
    }
    assert_eq!(requests[1].body["model"], SYSTEM_ONE_MODEL_ID);
    assert_eq!(requests[1].body["questions"]["urgent"]["type"], "noul");
}

#[tokio::test]
async fn decision_models_classify_text_through_the_same_api() {
    let transport = FakeTransport::new(vec![json_reply(200, Vec::new(), SYSTEM_ONE_REPLY)]);
    let registry = maple_model_registry(transport.clone(), []);
    for id in ["decision-small", "decision-medium", "decision-large"] {
        let model = registry
            .find_classifier(MAPLE_PROVIDER, id)
            .expect("decision model is registered");
        assert!(!model.supports_images(), "{id}");
        assert_eq!(model.context_window, 32_768, "{id}");
    }
    let model = registry
        .find_classifier(MAPLE_PROVIDER, "decision-medium")
        .unwrap();
    let result = registry
        .classify(&model, urgent_ticket(), pi_ai::ClassifierOptions::default())
        .await;
    assert!(!result.is_failure(), "{result:?}");
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].uri, SYSTEM_ONE_PATH);
    assert_eq!(requests[0].body["model"], "decision-medium");
}

#[tokio::test]
async fn a_failed_system_one_request_keeps_its_body_out_of_the_error() {
    let transport = FakeTransport::new(vec![json_reply(
        422,
        Vec::new(),
        r#"{"error":{"message":"the state mentions a secret","code":"system_one_too_many_questions"}}"#,
    )]);
    let registry = maple_model_registry(transport, []);
    let model = registry
        .find_classifier(MAPLE_PROVIDER, SYSTEM_ONE_MODEL_ID)
        .unwrap();
    let result = registry
        .classify(&model, urgent_ticket(), pi_ai::ClassifierOptions::default())
        .await;
    assert!(result.is_failure());
    let error = result.error_message.unwrap();
    assert!(error.starts_with("System One API error (422)"), "{error}");
    assert!(
        error.contains("Maple rejected the inference request (status 422)"),
        "{error}"
    );
    assert!(!error.contains("secret"), "{error}");
}

#[tokio::test]
async fn http_errors_become_fixed_messages_pi_classifies() {
    struct Case {
        status: u16,
        body: &'static str,
        text: &'static str,
        retryable: bool,
        overflow: bool,
    }
    let cases = [
        Case {
            status: 401,
            body: "{\"error\":{\"message\":\"token expired for user@example.com\"}}",
            text: AUTHENTICATION_ERROR_MESSAGE,
            retryable: false,
            overflow: false,
        },
        Case {
            status: 402,
            body: "",
            text: CREDITS_EXHAUSTED_MESSAGE,
            retryable: false,
            overflow: false,
        },
        Case {
            status: 413,
            body: "",
            text: CONTEXT_OVERFLOW_MESSAGE,
            retryable: false,
            overflow: true,
        },
        Case {
            status: 400,
            body: "{\"error\":{\"message\":\"This model's maximum context length is 1000 tokens\"}}",
            text: CONTEXT_OVERFLOW_MESSAGE,
            retryable: false,
            overflow: true,
        },
        Case {
            status: 400,
            body: "{\"error\":{\"message\":\"bad tool schema\"}}",
            text: "Maple rejected the inference request (status 400)",
            retryable: false,
            overflow: false,
        },
        Case {
            status: 429,
            body: "{\"error\":{\"message\":\"slow down: too many tokens per minute\"}}",
            text: RATE_LIMIT_MESSAGE,
            retryable: true,
            overflow: false,
        },
        Case {
            status: 503,
            body: "upstream connect error",
            text: "Maple server error (status 503)",
            retryable: true,
            overflow: false,
        },
        Case {
            status: 507,
            body: "",
            text: "Maple server error (status 507)",
            retryable: true,
            overflow: false,
        },
        Case {
            status: 404,
            body: "",
            text: ENDPOINT_NOT_FOUND_MESSAGE,
            retryable: false,
            overflow: false,
        },
    ];
    for case in cases {
        let message = failure(Reply::Response {
            status: case.status,
            headers: Vec::new(),
            chunks: vec![Ok(case.body)],
            stall_after: false,
        })
        .await;
        let error = message.error_message.clone().unwrap();
        assert_eq!(
            error,
            format!("{}: {}", case.status, json!({"message": case.text}))
        );
        // The provider's own text never reaches the message.
        assert!(!error.contains("example.com") && !error.contains("schema"));
        assert_eq!(is_retryable_error(&message), case.retryable, "{error}");
        assert_eq!(
            is_context_overflow(&message, None),
            case.overflow,
            "{error}"
        );
    }
}

#[tokio::test]
async fn transport_failures_become_fixed_messages_pi_classifies() {
    let cases: Vec<(maple_sdk::Error, &str, bool)> = vec![
        (
            maple_sdk::Error::Authentication("expired".into()),
            AUTHENTICATION_ERROR_MESSAGE,
            false,
        ),
        (
            maple_sdk::Error::Api {
                status: 402,
                message: "no credits".into(),
            },
            CREDITS_EXHAUSTED_MESSAGE,
            false,
        ),
        (
            maple_sdk::Error::Api {
                status: 429,
                message: String::new(),
            },
            RATE_LIMIT_MESSAGE,
            true,
        ),
        (
            maple_sdk::Error::Api {
                status: 502,
                message: String::new(),
            },
            "Maple server error (status 502)",
            true,
        ),
        (
            maple_sdk::Error::AttestationVerificationFailed(
                "This device's clock is about 2 hours behind the secure enclave".into(),
            ),
            DEVICE_CLOCK_ERROR_MESSAGE,
            false,
        ),
        (
            maple_sdk::Error::AttestationVerificationFailed("bad document".into()),
            ATTESTATION_VERIFICATION_ERROR_MESSAGE,
            false,
        ),
        (
            maple_sdk::Error::Decryption("tag mismatch".into()),
            SECURE_CONNECTION_ERROR_MESSAGE,
            false,
        ),
        (
            maple_sdk::Error::Configuration("no url".into()),
            PREPARE_ERROR_MESSAGE,
            false,
        ),
    ];
    for (error, text, retryable) in cases {
        let message = failure(Reply::Fail(error)).await;
        assert_eq!(message.error_message.as_deref(), Some(text));
        assert_eq!(is_retryable_error(&message), retryable, "{text}");
        assert!(!is_context_overflow(&message, None), "{text}");
    }
}

#[tokio::test]
async fn a_lost_secure_session_is_not_retried() {
    let message = failure(Reply::Response {
        status: 400,
        headers: vec![
            ("x-opensecret-error-contract", "1"),
            ("x-opensecret-error-code", "session_not_found"),
        ],
        chunks: vec![Ok("{}")],
        stall_after: false,
    })
    .await;
    assert_eq!(
        message.error_message.as_deref(),
        Some(SECURE_CONNECTION_ERROR_MESSAGE)
    );
    assert!(!is_retryable_error(&message));
}

#[tokio::test]
async fn an_error_frame_in_a_successful_response_fails_the_reply() {
    let message = failure(sse(
        200,
        vec![
            "data: {\"choices\":[{\"delta\":{\"content\":\"Par\"}}]}\n\n",
            "data: {\"error\":{\"message\":\"upstream overloaded\"}}\n\n",
        ],
    ))
    .await;
    assert_eq!(
        message.error_message.as_deref(),
        Some("upstream overloaded")
    );
    assert!(is_retryable_error(&message));
}

#[tokio::test]
async fn a_truncated_stream_is_a_retryable_failure() {
    let message = failure(sse(
        200,
        vec!["data: {\"choices\":[{\"delta\":{\"content\":\"Par\"}}]}\n\n"],
    ))
    .await;
    assert!(is_retryable_error(&message), "{message:?}");
}

#[tokio::test]
async fn a_broken_encrypted_stream_is_terminal_and_other_stream_errors_are_not() {
    let message = failure(Reply::Response {
        status: 200,
        headers: Vec::new(),
        chunks: vec![
            Ok("data: {\"choices\":[{\"delta\":{\"content\":\"Par\"}}]}\n\n"),
            Err(maple_sdk::Error::Decryption("tag mismatch".into())),
        ],
        stall_after: false,
    })
    .await;
    assert_eq!(
        message.error_message.as_deref(),
        Some(SECURE_CONNECTION_ERROR_MESSAGE)
    );
    assert!(!is_retryable_error(&message));

    let message = failure(Reply::Response {
        status: 200,
        headers: Vec::new(),
        chunks: vec![Err(maple_sdk::Error::Other("reset".into()))],
        stall_after: false,
    })
    .await;
    assert_eq!(message.error_message.as_deref(), Some(STREAM_ERROR_MESSAGE));
    assert!(is_retryable_error(&message));
}

#[tokio::test]
async fn a_server_that_never_answers_or_goes_quiet_times_out() {
    let transport = FakeTransport::new(vec![Reply::Stall]);
    let message = respond(transport.clone(), &model("glm-5-3")).await;
    assert_eq!(
        message.error_message.as_deref(),
        Some(REQUEST_TIMEOUT_MESSAGE)
    );
    assert!(is_retryable_error(&message));
    // The transport was told to stop and settled before the error came back.
    assert!(transport.saw_cancel.load(Ordering::SeqCst));
    assert_eq!(transport.settled.load(Ordering::SeqCst), 1);

    let message = failure(Reply::Response {
        status: 200,
        headers: Vec::new(),
        chunks: vec![Ok(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Par\"}}]}\n\n",
        )],
        stall_after: true,
    })
    .await;
    assert_eq!(
        message.error_message.as_deref(),
        Some(STREAM_TIMEOUT_MESSAGE)
    );
    assert!(is_retryable_error(&message));
}

#[tokio::test]
async fn stopping_a_run_cancels_the_send_and_lets_it_settle() {
    let transport = FakeTransport::new(vec![Reply::Stall]);
    let cancel = CancellationToken::new();
    let options = StreamOptions {
        cancel: cancel.clone(),
        ..StreamOptions::default()
    };
    let stream =
        MapleStreamFn::new(transport.clone()).stream(&model("glm-5-3"), context("hi"), options);
    while transport.requests.lock().unwrap().is_empty() {
        tokio::task::yield_now().await;
    }
    cancel.cancel();
    let message = stream.result().await;
    assert_eq!(message.stop_reason, StopReason::Aborted);
    for _ in 0..100 {
        if transport.settled.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(transport.saw_cancel.load(Ordering::SeqCst));
    assert_eq!(transport.settled.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_dropped_stream_still_lets_the_send_settle() {
    let transport = FakeTransport::new(vec![Reply::Stall]);
    let cancel = CancellationToken::new();
    let options = StreamOptions {
        cancel: cancel.clone(),
        ..StreamOptions::default()
    };
    let stream =
        MapleStreamFn::new(transport.clone()).stream(&model("glm-5-3"), context("hi"), options);
    while transport.requests.lock().unwrap().is_empty() {
        tokio::task::yield_now().await;
    }
    drop(stream);
    // A run that ends cancels its token; the send then settles on its own.
    cancel.cancel();
    for _ in 0..100 {
        if transport.settled.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(transport.settled.load(Ordering::SeqCst), 1);
}

fn screenshot_result(id: &str, data: &str) -> Message {
    Message::ToolResult(ToolResultMessage {
        tool_call_id: id.into(),
        tool_name: "computer".into(),
        content: vec![
            Content::text("screen"),
            Content::Image(pi_ai::ImageContent {
                data: data.into(),
                mime_type: "image/png".into(),
            }),
        ],
        details: None,
        usage: None,
        is_error: false,
        timestamp: 0,
    })
}

#[test]
fn only_the_newest_tool_result_images_stay_in_a_request() {
    let user_image = Message::User(UserMessage {
        content: vec![Content::Image(pi_ai::ImageContent {
            data: "user".into(),
            mime_type: "image/png".into(),
        })],
        timestamp: 0,
    });
    let mut messages = vec![user_image.clone()];
    for index in 0..5 {
        messages.push(screenshot_result(
            &format!("c{index}"),
            &format!("shot{index}"),
        ));
    }
    bound_tool_result_images(&mut messages);
    assert_eq!(messages[0], user_image);
    let images: Vec<bool> = messages[1..]
        .iter()
        .map(|message| match message {
            Message::ToolResult(result) => matches!(result.content[1], Content::Image(_)),
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(images, vec![false, false, true, true, true]);
    let Message::ToolResult(oldest) = &messages[1] else {
        unreachable!()
    };
    assert_eq!(oldest.content[1], Content::text(SUPERSEDED_IMAGE_MARKER));
}

const KIMI_TOOL_STREAM: [&str; 4] = [
    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"shell:0\",\"function\":{\"name\":\"shell\",\"arguments\":\"{}\"}}]}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"chatcmpl-tool-abc\",\"function\":{\"name\":\"read\",\"arguments\":\"{}\"}}]}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
    "data: [DONE]\n\n",
];

#[tokio::test]
async fn kimi_k3_legacy_tool_ids_get_unique_ids() {
    let transport = FakeTransport::new(vec![sse(200, KIMI_TOOL_STREAM.to_vec())]);
    let mut stream = MapleStreamFn::new(transport).stream(
        &model("kimi-k3"),
        context("hi"),
        StreamOptions::default(),
    );
    let mut started = Vec::new();
    let mut final_ids = Vec::new();
    while let Some(event) = stream.next().await {
        match event {
            AssistantMessageEvent::ToolCallStart { id, .. } => started.push(id),
            AssistantMessageEvent::Done { message } => {
                final_ids = message.tool_calls().map(|call| call.id.clone()).collect();
            }
            _ => {}
        }
    }
    assert_eq!(final_ids.len(), 2);
    assert!(final_ids[0].starts_with("chatcmpl-tool-") && final_ids[0] != "shell:0");
    assert_eq!(final_ids[1], "chatcmpl-tool-abc");
    assert_eq!(started, final_ids);

    // Other models keep the ids they were sent.
    let transport = FakeTransport::new(vec![sse(200, KIMI_TOOL_STREAM.to_vec())]);
    let message = respond(transport, &model("glm-5-3")).await;
    assert_eq!(message.tool_calls().next().unwrap().id, "shell:0");
}

#[test]
fn legacy_kimi_ids_name_their_tool() {
    assert!(is_legacy_kimi_tool_id("shell", "shell"));
    assert!(is_legacy_kimi_tool_id("shell:12", "shell"));
    assert!(!is_legacy_kimi_tool_id("shell:", "shell"));
    assert!(!is_legacy_kimi_tool_id("chatcmpl-tool-1", "shell"));
    assert!(!is_legacy_kimi_tool_id("call_abc", "shell"));
}

fn catalog() -> maple_sdk::ModelCatalogResponse {
    serde_json::from_value(json!({
        "object": "list",
        "data": [
            {"id": "glm-5-3", "context_window": 200000, "max_context_tokens": 200000,
             "capabilities": {"chat": true, "vision": false}},
            {"id": "kimi-k3", "context_window": 256000, "capabilities": {"vision": true}},
            {"id": "odd", "context_window": 1000, "max_context_tokens": 2000},
        ],
        "aliases": [
            {"id": "auto:powerful", "target_model": "kimi-k3"},
            {"id": "auto:text", "target_model": "kimi-k3", "capabilities": {"vision": false}},
        ],
    }))
    .unwrap()
}

#[test]
fn catalog_entries_resolve_aliases_and_reconcile_limits() {
    let catalog = catalog();
    assert_eq!(
        catalog_entry(&catalog, "glm-5-3"),
        Some(CatalogEntry {
            context_window: Some(200_000),
            vision: Some(false),
            ..CatalogEntry::default()
        })
    );
    assert_eq!(
        catalog_entry(&catalog, "auto:powerful"),
        Some(CatalogEntry {
            context_window: Some(256_000),
            vision: Some(true),
            ..CatalogEntry::default()
        })
    );
    // An explicit alias capability, false included, overrides the target's.
    assert_eq!(
        catalog_entry(&catalog, "auto:text").unwrap().vision,
        Some(false)
    );
    // Disagreeing limits are unknown.
    assert_eq!(catalog_entry(&catalog, "odd").unwrap().context_window, None);
    assert_eq!(catalog_entry(&catalog, "missing"), None);
}

#[test]
fn maple_models_carry_the_catalog_and_send_no_thinking_control() {
    let catalog = catalog();
    let entry = catalog_entry(&catalog, "kimi-k3");
    let model = maple_model("kimi-k3", entry.as_ref(), None);
    assert_eq!(model.provider, MAPLE_PROVIDER);
    assert_eq!(model.api, MAPLE_API);
    assert_eq!(model.context_window, 256_000);
    assert!(model.supports_images());
    assert!(!model.reasoning);
    assert_eq!(model.thinking_levels(), vec![pi_ai::ThinkingLevel::Off]);
    assert_eq!(model.max_tokens, 0);

    let unknown = maple_model("new-model", None, None);
    assert_eq!(unknown.context_window, DEFAULT_CONTEXT_WINDOW);
    assert!(!unknown.supports_images());
    // MAPLE_CONTEXT_LIMIT wins over the catalog.
    assert_eq!(
        maple_model("kimi-k3", entry.as_ref(), Some(64_000)).context_window,
        64_000
    );
}

#[tokio::test]
async fn an_alias_gets_its_own_reasoning_back_even_when_another_model_answered() {
    // The alias answers from another model family; the reply is recorded
    // under the requested id, so its reasoning goes back on the next turn.
    let transport = FakeTransport::new(vec![
        sse(
            200,
            vec![
                "data: {\"model\":\"kimi-k3-0905\",\"choices\":[{\"delta\":{\"reasoning_content\":\"plan\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n",
            ],
        ),
        sse(200, TEXT_STREAM.to_vec()),
    ]);
    let alias = model("auto:powerful");
    let stream_fn = MapleStreamFn::new(transport.clone());
    let first = stream_fn
        .stream(&alias, context("hi"), StreamOptions::default())
        .result()
        .await;
    assert_eq!(first.model, "auto:powerful");
    assert!(matches!(
        first.content.first(),
        Some(AssistantContent::Thinking(ThinkingContent { .. }))
    ));

    let mut next = context("hi");
    next.messages.push(Message::Assistant(first));
    next.messages.push(Message::User(UserMessage {
        content: vec![Content::text("more")],
        timestamp: 2,
    }));
    stream_fn
        .stream(&alias, next, StreamOptions::default())
        .result()
        .await;
    let requests = transport.requests.lock().unwrap();
    let assistant = requests[1].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "assistant")
        .cloned()
        .unwrap();
    assert_eq!(assistant["reasoning_content"], "plan");
    assert_eq!(assistant["content"], "answer");
}

#[test]
fn the_tool_declarations_ride_the_system_message() {
    // Maple's tools reach the request through Pi's transcript, unchanged.
    let tool = Tool::new("read", "Read a file", json!({"type": "object"}));
    let context = Context::new("You are Maple.", vec![tool], Vec::new());
    let Message::System(SystemMessage { tools_added, .. }) = &context.messages[0] else {
        panic!("a system message");
    };
    assert_eq!(tools_added[0].name, "read");
}

#[test]
fn context_length_texts_are_recognized() {
    for text in [
        "This model's maximum context length is 131072 tokens",
        "Input is too long for requested model",
        "request payload is too large: 20000000 bytes exceeds the limit",
        "prompt tokens exceed the model limit",
    ] {
        assert!(is_context_length_exceeded_message(text), "{text}");
    }
    for text in ["bad request", "rate limit exceeded", "too many requests"] {
        assert!(!is_context_length_exceeded_message(text), "{text}");
    }
}

#[test]
fn reasoning_catalog_shapes_define_levels_and_clamp_the_default() {
    use ThinkingLevel::{High, Low, Max, Medium, Minimal, Off, Xhigh};
    let cases = [
        (
            "glm-5-3",
            json!({"mandatory":true,"supported_efforts":["max","high","low"]}),
            vec![Low, High, Max],
            High,
        ),
        (
            "kimi-k3",
            json!({"mandatory":false,"supported_efforts":["max","high","low"]}),
            vec![Low, High, Max],
            High,
        ),
        (
            "deepseek-v4-1-flash",
            json!({"supported_efforts":["max","xhigh","high","low"]}),
            vec![Off, Low, High, Xhigh, Max],
            High,
        ),
        (
            "gpt-oss-120b",
            json!({"mandatory":true,"supported_efforts":["high","medium","low"]}),
            vec![Low, Medium, High],
            Medium,
        ),
        (
            "gemma4-31b",
            json!({"mandatory":false}),
            vec![Off, Minimal, Low, Medium, High],
            Medium,
        ),
        (
            "untiered-required",
            json!({"mandatory":true}),
            vec![Minimal, Low, Medium, High],
            Medium,
        ),
        ("llama3-3-70b", Value::Null, vec![Off], Off),
    ];
    for (id, reasoning, levels, default) in cases {
        let entry = CatalogEntry {
            reasoning: serde_json::from_value(reasoning).unwrap(),
            ..Default::default()
        };
        let model = maple_model(id, Some(&entry), None);
        assert_eq!(model.thinking_levels(), levels, "{id}");
        assert_eq!(model.clamp_thinking_level(Medium), default, "{id}");
        assert_eq!(model.reasoning, entry.reasoning.is_some());
        assert_eq!(model.compat.supports_reasoning_effort, model.reasoning);
        for level in levels {
            let expected = model
                .reasoning
                .then(|| if level == Off { "none" } else { level.as_str() }.to_string());
            assert_eq!(
                model.provider_thinking_value(level),
                expected,
                "{id}/{level:?}"
            );
        }
    }
}

#[test]
fn aliases_keep_their_reasoning_overrides_and_the_target_restrictions() {
    let catalog = serde_json::from_value(json!({
        "object":"list", "data":[
            {"id":"target", "reasoning":{"mandatory":true,"supported_efforts":["high","low"]}},
            {"id":"kimi-k3", "reasoning":{"supported_efforts":["max","high","low"]}}
        ], "aliases":[
            {"id":"auto:inherit","target_model":"target"},
            {"id":"auto:override","target_model":"target","reasoning":{"supported_efforts":["medium"]}},
            {"id":"auto:kimi","target_model":"kimi-k3"}
        ]
    })).unwrap();
    for (id, levels) in [
        (
            "auto:inherit",
            vec![ThinkingLevel::Low, ThinkingLevel::High],
        ),
        (
            "auto:override",
            vec![ThinkingLevel::Off, ThinkingLevel::Medium],
        ),
        (
            "auto:kimi",
            vec![ThinkingLevel::Low, ThinkingLevel::High, ThinkingLevel::Max],
        ),
    ] {
        let entry = catalog_entry(&catalog, id).unwrap();
        assert_eq!(
            maple_model(id, Some(&entry), None).thinking_levels(),
            levels
        );
    }
}

#[tokio::test]
async fn thinking_choices_reach_the_wire_and_side_calls_omit_them() {
    let entry = CatalogEntry {
        reasoning: Some(maple_sdk::ModelReasoning {
            supported_efforts: Some(vec!["low".into(), "high".into(), "max".into()]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let model = maple_model("reasoning-model", Some(&entry), None);
    for level in model.thinking_levels() {
        let transport = FakeTransport::new(vec![sse(200, TEXT_STREAM.to_vec())]);
        let mut stream = MapleStreamFn::new(transport.clone()).stream(
            &model,
            context("Hello"),
            StreamOptions {
                reasoning: Some(level),
                ..Default::default()
            },
        );
        while stream.next().await.is_some() {}
        assert_eq!(
            transport.requests.lock().unwrap()[0].body["reasoning_effort"],
            model.provider_thinking_value(level).unwrap()
        );
    }
    let transport = FakeTransport::new(vec![sse(200, TEXT_STREAM.to_vec())]);
    respond(
        transport.clone(),
        &maple_model("reasoning-model", None, None),
    )
    .await;
    assert!(
        transport.requests.lock().unwrap()[0]
            .body
            .get("reasoning_effort")
            .is_none()
    );
}
