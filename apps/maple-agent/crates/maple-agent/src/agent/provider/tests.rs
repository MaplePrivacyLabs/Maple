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
        assert_eq!(error, format!("{} {}", case.status, case.text));
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
        })
    );
    assert_eq!(
        catalog_entry(&catalog, "auto:powerful"),
        Some(CatalogEntry {
            context_window: Some(256_000),
            vision: Some(true),
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
