use crate::config::{Config, OpenAIError};
use axum::{
    body::{Body, Bytes},
    extract::{OriginalUri, State},
    http::{header, HeaderMap, HeaderName, Method, Request, StatusCode, Uri},
    response::{IntoResponse, Response},
    Json,
};
use futures::{future::BoxFuture, Stream, StreamExt};
use maple_sdk::{
    client::OpenSecretResponseBody, OpenSecretClient, Result as OpenSecretResult,
    TransportV2CacheNamespaceRoot,
};
use std::{collections::HashSet, io, pin::Pin, sync::Arc, time::Duration};
use tokio::{sync::OnceCell, time::Instant};
use tracing::{debug, error, warn};

type ProxyError = (StatusCode, Json<OpenAIError>);

trait InferenceTransport: Send + Sync {
    fn send_inference_request_with_api_key(
        &self,
        request: Request<Bytes>,
        api_key: String,
    ) -> BoxFuture<'_, OpenSecretResult<http::Response<OpenSecretResponseBody>>>;
}

impl InferenceTransport for OpenSecretClient {
    fn send_inference_request_with_api_key(
        &self,
        request: Request<Bytes>,
        api_key: String,
    ) -> BoxFuture<'_, OpenSecretResult<http::Response<OpenSecretResponseBody>>> {
        Box::pin(OpenSecretClient::send_inference_request_with_api_key(
            self, request, api_key,
        ))
    }
}

pub(crate) struct ProxyState {
    config: Config,
    client: OnceCell<Arc<OpenSecretClient>>,
    transport_override: Option<Arc<dyn InferenceTransport>>,
}

impl ProxyState {
    pub(crate) fn new(config: Config) -> Self {
        Self {
            config,
            client: OnceCell::new(),
            transport_override: None,
        }
    }

    #[cfg(test)]
    fn with_transport(config: Config, transport: Arc<dyn InferenceTransport>) -> Self {
        Self {
            config,
            client: OnceCell::new(),
            transport_override: Some(transport),
        }
    }

    async fn client(&self) -> Result<Arc<OpenSecretClient>, ProxyError> {
        let backend_url = self.config.backend_url.clone();
        let pcr0_environment = self.config.pcr0_environment;
        let request_timeout = self.config.request_timeout();
        let configured_cache_namespace_root = self.config.cache_namespace_root.clone();

        let client = self
            .client
            .get_or_try_init(|| async move {
                create_shared_client(
                    &backend_url,
                    pcr0_environment,
                    request_timeout,
                    configured_cache_namespace_root,
                )
                .await
                .map(Arc::new)
            })
            .await;

        match client {
            Ok(client) => Ok(Arc::clone(client)),
            Err(error) => Err(error),
        }
    }

    async fn transport(&self) -> Result<Arc<dyn InferenceTransport>, ProxyError> {
        if let Some(transport) = &self.transport_override {
            return Ok(Arc::clone(transport));
        }

        let client = self.client().await?;
        Ok(client)
    }
}

pub(crate) async fn health_check() -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "ok",
        "service": "maple-proxy",
        "version": env!("CARGO_PKG_VERSION")
    }))
}

fn extract_api_key(headers: &HeaderMap, default_key: Option<&str>) -> Result<String, OpenAIError> {
    // Try to get API key from Authorization header first
    if let Some(auth_header) = headers.get("authorization") {
        let auth_str = auth_header.to_str().map_err(|_| {
            OpenAIError::authentication_error("Invalid Authorization header format")
        })?;

        if let Some(key) = auth_str.strip_prefix("Bearer ") {
            return Ok(key.to_string());
        }
    }

    // Fall back to default API key from config
    default_key
        .map(str::to_owned)
        .ok_or_else(|| OpenAIError::authentication_error("No API key provided. Set MAPLE_API_KEY environment variable or provide Authorization header"))
}

async fn create_shared_client(
    backend_url: &str,
    pcr0_environment: maple_sdk::Pcr0Environment,
    request_timeout: Duration,
    configured_cache_namespace_root: Option<TransportV2CacheNamespaceRoot>,
) -> Result<OpenSecretClient, ProxyError> {
    let client = OpenSecretClient::new_with_pcr0_environment(backend_url, pcr0_environment)
        .map_err(|e| transport_error_response("OpenSecret client creation", &e))?;
    let client = if let Some(cache_namespace_root) = configured_cache_namespace_root {
        client.with_cache_namespace_root(cache_namespace_root)
    } else {
        warn!(
            "MAPLE_CACHE_NAMESPACE_ROOT is not configured; provider cache continuity will reset when this proxy process restarts"
        );
        client
    };

    // Perform attestation handshake
    tokio::time::timeout(request_timeout, client.perform_attestation_handshake())
        .await
        .map_err(|_| timeout_response("Attestation handshake", request_timeout))?
        .map_err(|e| transport_error_response("OpenSecret attestation handshake", &e))?;

    Ok(client)
}

fn timeout_response(operation: &str, timeout: Duration) -> ProxyError {
    error!(
        "{} timed out after {} seconds",
        operation,
        timeout.as_secs()
    );
    (
        StatusCode::GATEWAY_TIMEOUT,
        Json(OpenAIError::server_error(format!(
            "{} timed out after {} seconds",
            operation,
            timeout.as_secs()
        ))),
    )
}

/// Transparently forwards an OpenAI-compatible inference request through the
/// encrypted OpenSecret transport without interpreting either body.
pub(crate) async fn proxy_openai_request(
    State(state): State<Arc<ProxyState>>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ProxyError> {
    // Browser-reachable CORS mode and a server-held fallback credential are
    // deliberately mutually exclusive. An explicit request credential still
    // works, while a configured default is ignored in this mode.
    let default_api_key = (!state.config.enable_cors)
        .then(|| state.config.default_api_key.as_deref())
        .flatten();
    let api_key = extract_api_key(&headers, default_api_key)
        .map_err(|e| (StatusCode::UNAUTHORIZED, Json(e)))?;

    debug!("Proxying {} {}", method, uri.path());

    let transport = state.transport().await?;
    let request = build_upstream_request(method, uri, &headers, body);
    let request_timeout = state.config.request_timeout();
    let request_deadline = tokio::time::sleep(request_timeout).deadline();
    let response = tokio::time::timeout_at(
        request_deadline,
        transport.send_inference_request_with_api_key(request, api_key),
    )
    .await
    .map_err(|_| timeout_response("OpenAI-compatible request", request_timeout))?
    .map_err(|error| transport_error_response("OpenSecret inference request", &error))?;

    Ok(build_downstream_response(
        response,
        state.config.stream_idle_timeout(),
        request_deadline,
    ))
}

fn build_upstream_request(
    method: Method,
    uri: Uri,
    headers: &HeaderMap,
    body: Bytes,
) -> Request<Bytes> {
    let mut request = Request::new(body);
    *request.method_mut() = method;
    *request.uri_mut() = uri;
    copy_safe_request_headers(headers, request.headers_mut());
    request
}

fn copy_safe_request_headers(source: &HeaderMap, destination: &mut HeaderMap) {
    let connection_headers = connection_header_names(source);

    for name in source.keys() {
        if is_unsafe_request_header(name) || connection_headers.contains(name) {
            continue;
        }
        for value in source.get_all(name) {
            destination.append(name.clone(), value.clone());
        }
    }
}

fn connection_header_names(headers: &HeaderMap) -> HashSet<HeaderName> {
    headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect()
}

fn is_unsafe_request_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "authorization"
            | "cookie"
            | "set-cookie"
            | "accept-encoding"
            | "proxy-authorization"
            | "host"
            | "content-length"
            | "transfer-encoding"
            | "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "te"
            | "trailer"
            | "upgrade"
            | "forwarded"
            | "via"
            | "x-forwarded-for"
            | "x-forwarded-host"
            | "x-forwarded-proto"
            | "x-session-id"
    )
}

fn transport_error_response(operation: &str, error: &impl std::fmt::Display) -> ProxyError {
    error!("{} failed: {}", operation, error);
    (
        StatusCode::BAD_GATEWAY,
        Json(OpenAIError::server_error(
            "Failed to communicate securely with the Maple backend",
        )),
    )
}

fn build_downstream_response(
    response: http::Response<OpenSecretResponseBody>,
    stream_idle_timeout: Duration,
    request_deadline: Instant,
) -> Response {
    let (parts, body) = response.into_parts();
    // V2 returns at authenticated Start, before the body completes. Ordinary
    // responses retain the original request deadline; SSE uses only idle time.
    let request_deadline = (!is_event_stream(&parts.headers)).then_some(request_deadline);
    let mut response = Response::new(Body::from_stream(stream_with_timeouts(
        body,
        stream_idle_timeout,
        request_deadline,
    )));
    *response.status_mut() = parts.status;
    copy_safe_response_headers(&parts.headers, response.headers_mut());
    response
}

fn is_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"))
}

fn copy_safe_response_headers(source: &HeaderMap, destination: &mut HeaderMap) {
    let connection_headers = connection_header_names(source);

    for name in source.keys() {
        if is_unsafe_response_header(name) || connection_headers.contains(name) {
            continue;
        }
        for value in source.get_all(name) {
            destination.append(name.clone(), value.clone());
        }
    }
}

fn is_unsafe_response_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "set-cookie"
            | "content-length"
            | "transfer-encoding"
            | "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "upgrade"
    )
}

fn stream_with_timeouts(
    mut stream: OpenSecretResponseBody,
    stream_idle_timeout: Duration,
    request_deadline: Option<Instant>,
) -> Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>> {
    Box::pin(async_stream::stream! {
        loop {
            let idle_deadline = tokio::time::sleep(stream_idle_timeout).deadline();
            let (deadline, message) = match request_deadline {
                Some(deadline) if deadline <= idle_deadline =>
                    (deadline, "Maple backend request timed out"),
                _ => (idle_deadline, "Maple backend response stream timed out"),
            };
            let chunk_result = tokio::select! {
                // An expired total deadline wins even if buffered body data is
                // ready, including when the downstream consumer resumes late.
                biased;
                _ = tokio::time::sleep_until(deadline) => {
                    error!("{message}");
                    drop(stream);
                    yield Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        message,
                    ));
                    break;
                },
                chunk = stream.next() => match chunk {
                    Some(chunk_result) => chunk_result,
                    None => break,
                },
            };

            match chunk_result {
                Ok(bytes) => yield Ok(bytes),
                Err(error) => {
                    error!("OpenSecret response stream failed: {}", error);
                    yield Err(io::Error::other("Maple backend response stream failed"));
                    break;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::to_bytes,
        http::{HeaderValue, Request as AxumRequest},
    };
    use std::{
        collections::VecDeque,
        sync::{
            atomic::{AtomicBool, Ordering},
            Mutex,
        },
    };
    use tower::ServiceExt;

    fn test_config() -> Config {
        Config {
            host: "127.0.0.1".to_string(),
            port: 0,
            backend_url: "http://localhost:3000".to_string(),
            pcr0_environment: maple_sdk::Pcr0Environment::Production,
            default_api_key: None,
            cache_namespace_root: None,
            debug: false,
            enable_cors: false,
            request_timeout_secs: 300,
            stream_idle_timeout_secs: 300,
        }
    }

    struct MockTransport {
        requests: Mutex<Vec<(String, Request<Bytes>)>>,
        responses: Mutex<VecDeque<OpenSecretResult<http::Response<OpenSecretResponseBody>>>>,
        start_delay: Duration,
    }

    impl MockTransport {
        fn new(responses: Vec<OpenSecretResult<http::Response<OpenSecretResponseBody>>>) -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
                responses: Mutex::new(responses.into()),
                start_delay: Duration::ZERO,
            }
        }

        fn take_requests(&self) -> Vec<(String, Request<Bytes>)> {
            std::mem::take(&mut *self.requests.lock().unwrap())
        }
    }

    impl InferenceTransport for MockTransport {
        fn send_inference_request_with_api_key(
            &self,
            request: Request<Bytes>,
            api_key: String,
        ) -> BoxFuture<'_, OpenSecretResult<http::Response<OpenSecretResponseBody>>> {
            self.requests.lock().unwrap().push((api_key, request));
            let response = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("a mock response for every request");
            Box::pin(async move {
                if !self.start_delay.is_zero() {
                    tokio::time::sleep(self.start_delay).await;
                }
                response
            })
        }
    }

    struct PendingTransport;

    impl InferenceTransport for PendingTransport {
        fn send_inference_request_with_api_key(
            &self,
            _request: Request<Bytes>,
            _api_key: String,
        ) -> BoxFuture<'_, OpenSecretResult<http::Response<OpenSecretResponseBody>>> {
            Box::pin(std::future::pending())
        }
    }

    fn raw_response(
        status: StatusCode,
        headers: &[(&str, &str)],
        chunks: Vec<Bytes>,
    ) -> http::Response<OpenSecretResponseBody> {
        let body: OpenSecretResponseBody = Box::pin(futures::stream::iter(
            chunks.into_iter().map(Ok::<_, maple_sdk::Error>),
        ));
        let mut response = http::Response::new(body);
        *response.status_mut() = status;
        for (name, value) in headers {
            response.headers_mut().append(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        response
    }

    fn mock_app(transport: Arc<MockTransport>) -> axum::Router {
        let mut config = test_config();
        config.default_api_key = Some("default-key".to_string());
        let state = Arc::new(ProxyState::with_transport(config.clone(), transport));
        crate::create_app_with_state(config, state)
    }

    #[tokio::test]
    async fn all_explicit_inference_routes_forward_method_uri_headers_and_exact_body() {
        let responses = (0..3)
            .map(|_| {
                Ok(raw_response(
                    StatusCode::OK,
                    &[],
                    vec![Bytes::from_static(b"ok")],
                ))
            })
            .collect();
        let transport = Arc::new(MockTransport::new(responses));
        let app = mock_app(Arc::clone(&transport));
        let chat_body = Bytes::from_static(
            br#"{"model":"gemma4-31b","messages":[],"include_reasoning":false,"chat_template_kwargs":{"enable_thinking":false,"future":{"keep":true}}}"#,
        );
        let embedding_body = Bytes::from_static(b"\0\xffraw-provider-body");

        for request in [
            AxumRequest::builder()
                .method(Method::GET)
                .uri("/v1/models?provider=tinfoil")
                .header(header::AUTHORIZATION, "Bearer models-key")
                .header("x-provider-beta", "models-v2")
                .body(Body::empty())
                .unwrap(),
            AxumRequest::builder()
                .method(Method::POST)
                .uri("/v1/chat/completions?preview=1")
                .header(header::AUTHORIZATION, "Bearer chat-key")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-provider-beta", "thinking-controls")
                .body(Body::from(chat_body.clone()))
                .unwrap(),
            AxumRequest::builder()
                .method(Method::POST)
                .uri("/v1/embeddings")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(embedding_body.clone()))
                .unwrap(),
        ] {
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(to_bytes(response.into_body(), 16).await.unwrap(), "ok");
        }

        let requests = transport.take_requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].0, "models-key");
        assert_eq!(requests[1].0, "chat-key");
        assert_eq!(requests[2].0, "default-key");
        assert_eq!(requests[0].1.method(), Method::GET);
        assert_eq!(requests[0].1.uri(), "/v1/models?provider=tinfoil");
        assert!(requests[0].1.body().is_empty());
        assert_eq!(requests[0].1.headers()["x-provider-beta"], "models-v2");
        assert_eq!(requests[1].1.method(), Method::POST);
        assert_eq!(requests[1].1.uri(), "/v1/chat/completions?preview=1");
        assert_eq!(requests[1].1.body(), &chat_body);
        assert_eq!(
            requests[1].1.headers()["x-provider-beta"],
            "thinking-controls"
        );
        assert_eq!(requests[2].1.uri(), "/v1/embeddings");
        assert_eq!(requests[2].1.body(), &embedding_body);
        assert!(requests
            .iter()
            .all(|(_, request)| request.headers().get(header::AUTHORIZATION).is_none()));
    }

    #[tokio::test]
    async fn sse_response_is_forwarded_byte_for_byte_with_one_done_marker() {
        let first = Bytes::from_static(b"data: {\"id\":\"one\",\"future\":true}\n\n");
        let done = Bytes::from_static(b"data: [DONE]\n\n");
        let transport = Arc::new(MockTransport::new(vec![Ok(raw_response(
            StatusCode::OK,
            &[
                ("content-type", "text/event-stream"),
                ("x-request-id", "req-sse"),
            ],
            vec![first.clone(), done.clone()],
        ))]));
        let response = mock_app(transport)
            .oneshot(
                AxumRequest::builder()
                    .method(Method::POST)
                    .uri("/v1/chat/completions")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"stream":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/event-stream"
        );
        assert_eq!(response.headers()["x-request-id"], "req-sse");
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let mut expected = first.to_vec();
        expected.extend_from_slice(&done);
        assert_eq!(body.as_ref(), expected);
        assert_eq!(
            body.windows(b"[DONE]".len())
                .filter(|w| *w == b"[DONE]")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn upstream_error_status_safe_headers_and_body_are_preserved() {
        let error_body = Bytes::from_static(
            br#"{"error":{"message":"rate limited","type":"rate_limit_error"}}"#,
        );
        let transport = Arc::new(MockTransport::new(vec![Ok(raw_response(
            StatusCode::TOO_MANY_REQUESTS,
            &[
                ("content-type", "application/json"),
                ("retry-after", "7"),
                ("x-request-id", "req-429"),
                ("content-length", "999"),
                ("connection", "x-remove"),
                ("x-remove", "not-forwarded"),
            ],
            vec![error_body.clone()],
        ))]));
        let response = mock_app(transport)
            .oneshot(
                AxumRequest::builder()
                    .method(Method::POST)
                    .uri("/v1/embeddings")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        assert_eq!(response.headers()[header::RETRY_AFTER], "7");
        assert_eq!(response.headers()["x-request-id"], "req-429");
        assert!(response.headers().get(header::CONTENT_LENGTH).is_none());
        assert!(response.headers().get("x-remove").is_none());
        assert_eq!(
            to_bytes(response.into_body(), 1024).await.unwrap(),
            error_body
        );
    }

    #[tokio::test]
    async fn upstream_server_error_status_and_body_are_preserved() {
        let error_body = Bytes::from_static(b"provider unavailable");
        let transport = Arc::new(MockTransport::new(vec![Ok(raw_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &[("content-type", "text/plain"), ("retry-after", "2")],
            vec![error_body.clone()],
        ))]));
        let response = mock_app(transport)
            .oneshot(
                AxumRequest::builder()
                    .method(Method::GET)
                    .uri("/v1/models")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::RETRY_AFTER], "2");
        assert_eq!(
            to_bytes(response.into_body(), 1024).await.unwrap(),
            error_body
        );
    }

    #[tokio::test(start_paused = true)]
    async fn response_start_timeout_is_gateway_timeout() {
        let mut config = test_config();
        config.default_api_key = Some("default-key".to_string());
        config.request_timeout_secs = 1;
        let state = Arc::new(ProxyState::with_transport(
            config.clone(),
            Arc::new(PendingTransport),
        ));
        let response = crate::create_app_with_state(config, state)
            .oneshot(
                AxumRequest::builder()
                    .method(Method::GET)
                    .uri("/v1/models")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    }

    struct DropMarker(Arc<AtomicBool>);

    impl Drop for DropMarker {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn very_large_configured_timeouts_preserve_ready_responses() {
        let transport = Arc::new(MockTransport::new(vec![Ok(raw_response(
            StatusCode::OK,
            &[("content-type", "application/json")],
            vec![Bytes::from_static(b"{}")],
        ))]));
        let config = test_config()
            .with_api_key("fixture-key".into())
            .with_request_timeout_secs(u64::MAX)
            .with_stream_idle_timeout_secs(u64::MAX);
        let state = Arc::new(ProxyState::with_transport(config.clone(), transport));
        let response = crate::create_app_with_state(config, state)
            .oneshot(
                AxumRequest::builder()
                    .uri("/v1/models")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(to_bytes(response.into_body(), 1024).await.unwrap(), "{}");
    }

    #[tokio::test(start_paused = true)]
    async fn non_streaming_body_uses_original_request_deadline_and_drops_without_retry() {
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = DropMarker(Arc::clone(&dropped));
        let body: OpenSecretResponseBody = Box::pin(async_stream::stream! {
            let _guard = guard;
            tokio::time::sleep(Duration::from_millis(600)).await;
            yield Ok(Bytes::from_static(b"{}"));
        });
        let mut upstream = http::Response::new(body);
        upstream
            .headers_mut()
            .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        let mut transport = MockTransport::new(vec![Ok(upstream)]);
        transport.start_delay = Duration::from_millis(600);
        let transport = Arc::new(transport);
        // Short test-only durations on a paused clock; production defaults stay 300s.
        let config = test_config()
            .with_api_key("fixture-key".into())
            .with_request_timeout_secs(1)
            .with_stream_idle_timeout_secs(5);
        let state = Arc::new(ProxyState::with_transport(
            config.clone(),
            transport.clone(),
        ));
        let started = Instant::now();
        let response = crate::create_app_with_state(config, state)
            .oneshot(
                AxumRequest::builder()
                    .uri("/v1/models")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(started.elapsed() >= Duration::from_millis(600));
        assert!(to_bytes(response.into_body(), 1024).await.is_err());
        assert!(started.elapsed() >= Duration::from_secs(1));
        assert!(started.elapsed() < Duration::from_millis(1200));
        assert!(dropped.load(Ordering::SeqCst));
        assert_eq!(transport.take_requests().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn timely_non_streaming_chunks_do_not_extend_the_total_deadline() {
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = DropMarker(Arc::clone(&dropped));
        let body: OpenSecretResponseBody = Box::pin(async_stream::stream! {
            let _guard = guard;
            loop {
                tokio::time::sleep(Duration::from_millis(300)).await;
                yield Ok(Bytes::from_static(b"chunk"));
            }
        });
        let mut stream = stream_with_timeouts(
            body,
            Duration::from_secs(5),
            Some(Instant::now() + Duration::from_secs(1)),
        );
        for _ in 0..3 {
            assert_eq!(stream.next().await.unwrap().unwrap(), "chunk");
        }
        let error = stream.next().await.unwrap().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        // Upstream is released before yielding the error, even if the caller stops polling.
        assert!(dropped.load(Ordering::SeqCst));
        assert!(stream.next().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn expired_deadline_rejects_ready_body_when_consumer_resumes() {
        let body: OpenSecretResponseBody =
            Box::pin(futures::stream::iter([Ok(Bytes::from_static(b"ready"))]));
        let mut stream = stream_with_timeouts(
            body,
            Duration::from_secs(5),
            Some(Instant::now() + Duration::from_secs(1)),
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(
            stream.next().await.unwrap().unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[tokio::test(start_paused = true)]
    async fn sse_remains_incremental_beyond_request_deadline() {
        let body: OpenSecretResponseBody = Box::pin(async_stream::stream! {
            for chunk in [b"data: first\n\n".as_slice(), b"data: second\n\n", b"data: [DONE]\n\n"] {
                tokio::time::sleep(Duration::from_millis(600)).await;
                yield Ok(Bytes::from_static(chunk));
            }
        });
        let mut upstream = http::Response::new(body);
        upstream.headers_mut().insert(
            header::CONTENT_TYPE,
            "Text/Event-Stream ; charset=utf-8".parse().unwrap(),
        );
        let started = Instant::now();
        let response = build_downstream_response(
            upstream,
            Duration::from_secs(1),
            started + Duration::from_secs(1),
        );
        let mut stream = response.into_body().into_data_stream();
        assert_eq!(stream.next().await.unwrap().unwrap(), "data: first\n\n");
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(stream.next().await.unwrap().unwrap(), "data: second\n\n");
        assert_eq!(stream.next().await.unwrap().unwrap(), "data: [DONE]\n\n");
        assert!(started.elapsed() > Duration::from_secs(1));
        assert!(stream.next().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_downstream_body_releases_pending_upstream() {
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = DropMarker(Arc::clone(&dropped));
        let body: OpenSecretResponseBody = Box::pin(async_stream::stream! {
            let _guard = guard;
            yield Ok(Bytes::from_static(b"first"));
            std::future::pending::<()>().await;
        });
        let response = build_downstream_response(
            http::Response::new(body),
            Duration::from_secs(300),
            Instant::now() + Duration::from_secs(300),
        );
        let mut body = response.into_body().into_data_stream();
        assert_eq!(body.next().await.unwrap().unwrap(), "first");
        drop(body);
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn routes_outside_the_explicit_proxy_surface_are_not_forwarded() {
        let transport = Arc::new(MockTransport::new(Vec::new()));
        let response = mock_app(Arc::clone(&transport))
            .oneshot(
                AxumRequest::builder()
                    .method(Method::POST)
                    .uri("/v1/audio/speech")
                    .body(Body::from("opaque"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(transport.take_requests().is_empty());
    }

    #[tokio::test]
    async fn transport_errors_are_safe_bad_gateway_responses() {
        let transport = Arc::new(MockTransport::new(vec![Err(maple_sdk::Error::Other(
            "sensitive transport detail".to_string(),
        ))]));
        let response = mock_app(transport)
            .oneshot(
                AxumRequest::builder()
                    .method(Method::GET)
                    .uri("/v1/models")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("Failed to communicate securely"));
        assert!(!body.contains("sensitive transport detail"));
    }

    #[test]
    fn request_headers_strip_credentials_framing_and_connection_options() {
        let mut source = HeaderMap::new();
        source.insert(header::AUTHORIZATION, "Bearer secret".parse().unwrap());
        source.insert(header::COOKIE, "session=local-secret".parse().unwrap());
        source.insert(header::ACCEPT_ENCODING, "gzip, br".parse().unwrap());
        source.insert(header::HOST, "localhost".parse().unwrap());
        source.insert(header::CONTENT_LENGTH, "10".parse().unwrap());
        source.insert(header::CONNECTION, "x-remove".parse().unwrap());
        source.insert("x-session-id", "forged".parse().unwrap());
        source.insert("forwarded", "for=192.0.2.1".parse().unwrap());
        source.insert("via", "1.1 outer-proxy".parse().unwrap());
        source.insert("x-forwarded-for", "192.0.2.1".parse().unwrap());
        source.insert("x-forwarded-host", "outer.example".parse().unwrap());
        source.insert("x-forwarded-proto", "https".parse().unwrap());
        source.insert("x-remove", "also-forbidden".parse().unwrap());
        source.insert("x-provider-beta", "keep-me".parse().unwrap());

        let mut destination = HeaderMap::new();
        copy_safe_request_headers(&source, &mut destination);

        assert_eq!(destination.get("x-provider-beta").unwrap(), "keep-me");
        assert!(destination.get(header::AUTHORIZATION).is_none());
        assert!(destination.get(header::COOKIE).is_none());
        assert!(destination.get(header::ACCEPT_ENCODING).is_none());
        assert!(destination.get(header::CONTENT_LENGTH).is_none());
        assert!(destination.get("x-session-id").is_none());
        assert!(destination.get("forwarded").is_none());
        assert!(destination.get("via").is_none());
        assert!(destination.get("x-forwarded-for").is_none());
        assert!(destination.get("x-forwarded-host").is_none());
        assert!(destination.get("x-forwarded-proto").is_none());
        assert!(destination.get("x-remove").is_none());
    }

    #[test]
    fn response_headers_strip_cookie_credentials() {
        let mut source = HeaderMap::new();
        source.insert(
            header::SET_COOKIE,
            "session=backend-secret".parse().unwrap(),
        );
        source.insert("x-request-id", "req-1".parse().unwrap());

        let mut destination = HeaderMap::new();
        copy_safe_response_headers(&source, &mut destination);

        assert!(destination.get(header::SET_COOKIE).is_none());
        assert_eq!(destination.get("x-request-id").unwrap(), "req-1");
    }

    #[tokio::test]
    async fn response_stream_reports_idle_timeout() {
        let backend_stream: OpenSecretResponseBody =
            Box::pin(futures::stream::pending::<OpenSecretResult<Bytes>>());
        let mut stream = stream_with_timeouts(backend_stream, Duration::from_millis(1), None);

        let error = tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn auth_header_overrides_default_and_default_remains_supported() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer request-key".parse().unwrap());
        assert_eq!(
            extract_api_key(&headers, Some("default-key")).unwrap(),
            "request-key"
        );
        assert_eq!(
            extract_api_key(&HeaderMap::new(), Some("default-key")).unwrap(),
            "default-key"
        );
        assert!(extract_api_key(&HeaderMap::new(), None).is_err());
    }

    #[tokio::test]
    async fn cors_mode_never_spends_the_configured_default_key() {
        let transport = Arc::new(MockTransport::new(vec![Ok(raw_response(
            StatusCode::OK,
            &[],
            vec![Bytes::from_static(b"ok")],
        ))]));
        let mut config = test_config();
        config.enable_cors = true;
        config.default_api_key = Some("must-not-be-spent".to_string());
        let state = Arc::new(ProxyState::with_transport(
            config.clone(),
            Arc::clone(&transport) as Arc<dyn InferenceTransport>,
        ));
        let response = crate::create_app_with_state(config.clone(), state)
            .oneshot(
                AxumRequest::builder()
                    .method(Method::GET)
                    .uri("/v1/models")
                    .header(header::ORIGIN, "https://untrusted.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(transport.take_requests().is_empty());

        let response = crate::create_app_with_state(
            config.clone(),
            Arc::new(ProxyState::with_transport(
                config,
                Arc::clone(&transport) as Arc<dyn InferenceTransport>,
            )),
        )
        .oneshot(
            AxumRequest::builder()
                .method(Method::GET)
                .uri("/v1/models")
                .header(header::ORIGIN, "https://browser.example")
                .header(header::AUTHORIZATION, "Bearer per-request-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let requests = transport.take_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0, "per-request-key");
    }
}
