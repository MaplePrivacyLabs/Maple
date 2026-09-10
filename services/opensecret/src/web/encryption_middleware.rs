use axum::{
    body::{Body, Bytes, HttpBody},
    extract::{FromRequestParts, State},
    http::{request::Parts, HeaderMap, Method, Request},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use base64::Engine;
use http_body::{Frame, SizeHint};
use serde::de::{DeserializeOwned, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use uuid::Uuid;

use crate::{transport_v2::crypto::SessionId, ApiError, AppState};

const MAX_ENCRYPTED_BODY_BYTES: usize = 50 * 1024 * 1024; // 50MB

// Containers and scalar values each count as one node; object member names do
// not count separately. String bytes are already covered by the body-size
// limit and do not increase this count.
const MAX_V2_JSON_NODES: usize = 1_048_576;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum JsonComplexityError {
    Invalid,
    TooManyNodes,
}

struct JsonNodeBudget {
    remaining: usize,
    exceeded: bool,
}

impl JsonNodeBudget {
    fn claim<E: serde::de::Error>(&mut self) -> Result<(), E> {
        match self.remaining.checked_sub(1) {
            Some(remaining) => {
                self.remaining = remaining;
                Ok(())
            }
            None => {
                self.exceeded = true;
                Err(E::custom("JSON node limit exceeded"))
            }
        }
    }
}

struct JsonNode<'a> {
    budget: &'a mut JsonNodeBudget,
}

impl<'de> DeserializeSeed<'de> for JsonNode<'_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        self.budget.claim::<D::Error>()?;
        deserializer.deserialize_any(JsonNodeVisitor {
            budget: self.budget,
        })
    }
}

struct JsonNodeVisitor<'a> {
    budget: &'a mut JsonNodeBudget,
}

impl<'de> Visitor<'de> for JsonNodeVisitor<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while sequence
            .next_element_seed(JsonNode {
                budget: &mut *self.budget,
            })?
            .is_some()
        {}
        Ok(())
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        while map.next_key::<IgnoredAny>()?.is_some() {
            map.next_value_seed(JsonNode {
                budget: &mut *self.budget,
            })?;
        }
        Ok(())
    }
}

fn validate_json_complexity(input: &[u8], max_nodes: usize) -> Result<(), JsonComplexityError> {
    let mut budget = JsonNodeBudget {
        remaining: max_nodes,
        exceeded: false,
    };
    let mut deserializer = serde_json::Deserializer::from_slice(input);
    let parsed = JsonNode {
        budget: &mut budget,
    }
    .deserialize(&mut deserializer);

    if budget.exceeded {
        return Err(JsonComplexityError::TooManyNodes);
    }
    parsed.map_err(|_| JsonComplexityError::Invalid)?;
    deserializer.end().map_err(|_| JsonComplexityError::Invalid)
}

fn validate_v2_json_body(input: &[u8], max_nodes: usize) -> Result<(), ApiError> {
    match validate_json_complexity(input, max_nodes) {
        Ok(()) => Ok(()),
        Err(JsonComplexityError::Invalid) => Err(ApiError::BadRequest),
        Err(JsonComplexityError::TooManyNodes) => Err(ApiError::PayloadTooLarge),
    }
}

/// A decrypted request body transferred exactly once from middleware to its
/// handler.
///
/// Axum's [`axum::Extension`] extractor clones the stored value. That is fine
/// for small request metadata, but can duplicate a large deserialized request
/// body. This extractor removes the value from the request extensions instead,
/// preserving ordinary handler ownership without a second allocation.
pub struct Decrypted<T>(pub T);

#[axum::async_trait]
impl<S, T> FromRequestParts<S> for Decrypted<T>
where
    S: Send + Sync,
    T: Clone + Send + Sync + 'static,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .remove::<T>()
            .map(Self)
            .ok_or(ApiError::InternalServerError)
    }
}

fn store_decrypted<T>(request: &mut Request<Body>, decrypted: T)
where
    T: Clone + Send + Sync + 'static,
{
    request.extensions_mut().insert(decrypted);
}

/// Identifies the transport context that must encrypt a handler response.
///
/// Keeping this distinct from the application's authenticated principal makes
/// the response transport explicit at the handler boundary. V1 contains only
/// the legacy session identifier; future protocol versions can carry their
/// exact response-encryption context without changing every handler again.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransportSession {
    kind: TransportSessionKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum TransportSessionKind {
    V1 {
        session_id: Uuid,
    },
    #[allow(dead_code)] // Constructed by the stacked V2 gateway.
    V2 {
        session_id: SessionId,
    },
}

impl TransportSession {
    // Widened `pub(crate)` only so sibling test modules can fabricate v1
    // markers for the recovery transport-gate tests. It remains an in-process
    // capability: HTTP callers cannot create request extensions, and every
    // recovery route rejects v1 markers at route-layer and handler level.
    pub(crate) fn v1(session_id: Uuid) -> Self {
        Self {
            kind: TransportSessionKind::V1 { session_id },
        }
    }

    /// Marks a request that has already been authenticated and decrypted by
    /// the private Transport V2 gateway. This is an in-process capability: an
    /// HTTP caller cannot create request extensions.
    #[allow(dead_code)] // Constructed by the stacked V2 gateway.
    pub(crate) fn v2(session_id: SessionId) -> Self {
        Self {
            kind: TransportSessionKind::V2 { session_id },
        }
    }

    pub(crate) fn is_v2(&self) -> bool {
        matches!(&self.kind, TransportSessionKind::V2 { .. })
    }

    #[allow(dead_code)] // Used by OAuth and native handoff in the gateway layer.
    pub(crate) fn v2_session_id(&self) -> Option<SessionId> {
        match &self.kind {
            TransportSessionKind::V1 { .. } => None,
            TransportSessionKind::V2 { session_id } => Some(*session_id),
        }
    }

    pub(crate) async fn encrypt_response_bytes(
        &self,
        state: &AppState,
        plaintext: &[u8],
    ) -> Result<Vec<u8>, ApiError> {
        match &self.kind {
            TransportSessionKind::V1 { session_id } => {
                state.encrypt_session_data(session_id, plaintext).await
            }
            TransportSessionKind::V2 { .. } => Ok(plaintext.to_vec()),
        }
    }

    /// Returns the exact `data:` payload expected by the logical SSE stream.
    /// V1 keeps its per-event encryption and base64 wrapper; V2 leaves the
    /// inner stream as ordinary UTF-8 because the gateway encrypts the entire
    /// response carrier record-by-record.
    pub(crate) async fn encode_sse_data(
        &self,
        state: &AppState,
        plaintext: &str,
    ) -> Result<String, ApiError> {
        match &self.kind {
            TransportSessionKind::V1 { .. } => {
                let encrypted = self
                    .encrypt_response_bytes(state, plaintext.as_bytes())
                    .await?;
                Ok(base64::engine::general_purpose::STANDARD.encode(encrypted))
            }
            TransportSessionKind::V2 { .. } => Ok(plaintext.to_string()),
        }
    }
}

#[derive(Deserialize)]
pub struct EncryptedRequest {
    pub encrypted: String,
}

#[derive(Serialize)]
pub struct EncryptedResponse<T: Serialize> {
    pub encrypted: String,
    #[serde(skip)]
    _phantom: std::marker::PhantomData<T>,
}

impl<T: Serialize> EncryptedResponse<T> {
    pub fn new(encrypted: String) -> Self {
        Self {
            encrypted,
            _phantom: std::marker::PhantomData,
        }
    }
}

fn skips_encrypted_body<T: 'static>(method: &Method) -> bool {
    method == Method::GET
        || method == Method::DELETE
        || std::any::TypeId::of::<T>() == std::any::TypeId::of::<()>()
}

/// Rejects a v1 transport session before the handler can run.
///
/// Applied as a route layer on sub-routers whose routes are reachable only
/// through the v2 gateway. A missing `TransportSession` also fails: a request
/// that bypassed both transports never established transport security.
pub async fn require_transport_v2(
    request: Request<Body>,
    next: Next,
) -> Result<Response, ApiError> {
    if !request
        .extensions()
        .get::<TransportSession>()
        .is_some_and(TransportSession::is_v2)
    {
        return Err(ApiError::BadRequest);
    }
    Ok(next.run(request).await)
}

/// Handler-level defense-in-depth: the middleware gate rejects v1 sessions
/// before the handler, and this guard rejects them again inside the handler.
pub(crate) fn require_v2_transport_session(session: &TransportSession) -> Result<(), ApiError> {
    if !session.is_v2() {
        return Err(ApiError::BadRequest);
    }
    Ok(())
}

fn parse_session_id(headers: &HeaderMap) -> Result<Uuid, ApiError> {
    headers
        .get("x-session-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| Uuid::parse_str(value).ok())
        .ok_or(ApiError::BadRequest)
}

async fn forward_bodyless_request<T, Resource, SessionCheck, RunNext, RunNextFuture>(
    session_check: SessionCheck,
    session_id: Uuid,
    mut request: Request<Body>,
    run_next: RunNext,
) -> Result<Response, ApiError>
where
    T: 'static,
    Resource: Send + Unpin + 'static,
    SessionCheck: Future<Output = Result<Resource, ApiError>>,
    RunNext: FnOnce(Request<Body>) -> RunNextFuture,
    RunNextFuture: Future<Output = Response>,
{
    let resource = session_check.await?;

    if std::any::TypeId::of::<T>() == std::any::TypeId::of::<()>() {
        request.extensions_mut().insert(());
    }
    request
        .extensions_mut()
        .insert(TransportSession::v1(session_id));
    let response = run_next(request).await;
    Ok(hold_resource_through_response_body(response, resource))
}

pub async fn decrypt_request<T>(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    mut request: Request<Body>,
    next: Next,
) -> Result<Response, ApiError>
where
    T: DeserializeOwned + Send + Sync + Clone + 'static,
{
    if request
        .extensions()
        .get::<TransportSession>()
        .is_some_and(TransportSession::is_v2)
    {
        if std::any::TypeId::of::<T>() == std::any::TypeId::of::<()>() {
            request.extensions_mut().insert(());
        } else {
            let body = std::mem::replace(request.body_mut(), Body::empty());
            let body_bytes = axum::body::to_bytes(body, MAX_ENCRYPTED_BODY_BYTES)
                .await
                .map_err(|_| ApiError::PayloadTooLarge)?;
            validate_v2_json_body(&body_bytes, MAX_V2_JSON_NODES)?;
            let decoded =
                serde_json::from_slice::<T>(&body_bytes).map_err(|_| ApiError::BadRequest)?;
            store_decrypted(&mut request, decoded);
        }

        return Ok(next.run(request).await);
    }

    let session_id = parse_session_id(&headers)?;

    // Skip body processing for GET, DELETE, or when T is ().
    if skips_encrypted_body::<T>(request.method()) {
        // Bodyless requests cannot prove possession by decrypting a payload.
        // Reject an unknown session before a handler can perform side effects.
        return forward_bodyless_request::<T, _, _, _, _>(
            async {
                let session_lease = state.acquire_request_session(&session_id).await?;
                state.touch_session(&session_id).await?;
                Ok(session_lease)
            },
            session_id,
            request,
            move |request| next.run(request),
        )
        .await;
    }

    let body = std::mem::replace(request.body_mut(), Body::empty());
    let body_bytes = axum::body::to_bytes(body, MAX_ENCRYPTED_BODY_BYTES)
        .await
        .map_err(|_| ApiError::PayloadTooLarge)?;

    let encrypted_request: EncryptedRequest =
        serde_json::from_slice(&body_bytes).map_err(|_| ApiError::BadRequest)?;

    // Pin only after the request body has arrived. A slow or abandoned
    // upload therefore cannot retain a session indefinitely.
    let session_lease = state.acquire_request_session(&session_id).await?;
    let decrypted_data = state
        .decrypt_session_data(&session_id, &session_lease, &encrypted_request.encrypted)
        .await
        .map_err(|_| ApiError::BadRequest)?;

    let decrypted: T = serde_json::from_slice(&decrypted_data).map_err(|e| {
        tracing::error!("Failed to deserialize decrypted data: {:?}", e);
        ApiError::BadRequest
    })?;

    store_decrypted(&mut request, decrypted);
    request
        .extensions_mut()
        .insert(TransportSession::v1(session_id));
    let response = next.run(request).await;
    Ok(hold_resource_through_response_body(response, session_lease))
}

fn hold_resource_through_response_body<T>(mut response: Response, resource: T) -> Response
where
    T: Send + Unpin + 'static,
{
    let body = std::mem::replace(response.body_mut(), Body::empty());
    *response.body_mut() = Body::new(ResourceBody {
        inner: body,
        _resource: resource,
    });
    response
}

struct ResourceBody<T> {
    inner: Body,
    _resource: T,
}

impl<T> HttpBody for ResourceBody<T>
where
    T: Send + Unpin + 'static,
{
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        Pin::new(&mut self.get_mut().inner).poll_frame(context)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

pub async fn encrypt_response<T: Serialize>(
    state: &AppState,
    transport_session: &TransportSession,
    response: &T,
) -> Result<Response, ApiError> {
    let response_json = serde_json::to_vec(response).map_err(|_| ApiError::InternalServerError)?;

    if transport_session.is_v2() {
        return Ok((
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            response_json,
        )
            .into_response());
    }

    let encrypted_response = transport_session
        .encrypt_response_bytes(state, &response_json)
        .await?;
    Ok(Json(EncryptedResponse::<T>::new(
        base64::engine::general_purpose::STANDARD.encode(encrypted_response),
    ))
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease_aware_cache::{InsertError, LeaseAwareTtlCache};
    use axum::{
        http::{HeaderMap, HeaderValue},
        response::IntoResponse,
    };
    use std::convert::Infallible;
    use std::future::{poll_fn, ready};
    use std::io;
    use std::num::NonZeroUsize;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[derive(Debug)]
    struct CloneSpy {
        clone_count: Arc<AtomicUsize>,
        value: String,
    }

    impl Clone for CloneSpy {
        fn clone(&self) -> Self {
            self.clone_count.fetch_add(1, Ordering::SeqCst);
            Self {
                clone_count: self.clone_count.clone(),
                value: self.value.clone(),
            }
        }
    }

    #[tokio::test]
    async fn decrypted_body_is_moved_to_the_handler_without_cloning() {
        let clone_count = Arc::new(AtomicUsize::new(0));
        let mut request = Request::new(Body::empty());
        store_decrypted(
            &mut request,
            CloneSpy {
                clone_count: clone_count.clone(),
                value: "request body".to_string(),
            },
        );
        let (mut parts, _) = request.into_parts();

        let Decrypted(body) = Decrypted::<CloneSpy>::from_request_parts(&mut parts, &())
            .await
            .unwrap();

        assert_eq!(body.value, "request body");
        assert_eq!(clone_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn decrypted_body_can_only_be_extracted_once() {
        let mut request = Request::new(Body::empty());
        store_decrypted(&mut request, "request body".to_string());
        let (mut parts, _) = request.into_parts();

        let first = Decrypted::<String>::from_request_parts(&mut parts, &()).await;
        let second = Decrypted::<String>::from_request_parts(&mut parts, &()).await;

        assert!(matches!(first, Ok(Decrypted(body)) if body == "request body"));
        assert!(matches!(second, Err(ApiError::InternalServerError)));
    }

    #[test]
    fn json_complexity_counts_structure_without_counting_string_bytes() {
        let large_string = format!(r#""{}""#, "x".repeat(128 * 1024));
        assert_eq!(validate_json_complexity(large_string.as_bytes(), 1), Ok(()));

        let structured = br#"[true,{"payload":"aGVsbG8="},[-1,1.5,null]]"#;
        assert_eq!(validate_json_complexity(structured, 8), Ok(()));
        assert_eq!(
            validate_json_complexity(structured, 7),
            Err(JsonComplexityError::TooManyNodes)
        );
    }

    #[test]
    fn v2_json_preflight_distinguishes_invalid_json_from_excess_structure() {
        assert!(matches!(
            validate_v2_json_body(br#"[1,]"#, 16),
            Err(ApiError::BadRequest)
        ));
        assert!(matches!(
            validate_v2_json_body(br#"[1,2]"#, 2),
            Err(ApiError::PayloadTooLarge)
        ));
        assert!(validate_v2_json_body(br#"[1,2]"#, 3).is_ok());
    }

    #[test]
    fn classifies_every_bodyless_disjunct() {
        assert!(skips_encrypted_body::<serde_json::Value>(&Method::GET));
        assert!(skips_encrypted_body::<serde_json::Value>(&Method::DELETE));
        assert!(skips_encrypted_body::<()>(&Method::POST));
        assert!(!skips_encrypted_body::<serde_json::Value>(&Method::POST));
    }

    #[test]
    fn missing_and_malformed_session_headers_are_generic_bad_requests() {
        for headers in [HeaderMap::new(), {
            let mut headers = HeaderMap::new();
            headers.insert("x-session-id", HeaderValue::from_static("not-a-uuid"));
            headers
        }] {
            let error = parse_session_id(&headers).unwrap_err();
            let response = error.into_response();
            assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
            assert!(response.headers().get(crate::ERROR_CODE_HEADER).is_none());
        }
    }

    #[tokio::test]
    async fn failed_session_check_never_calls_next() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed_calls = calls.clone();

        let result = forward_bodyless_request::<(), _, _, _, _>(
            ready(Err::<(), ApiError>(ApiError::SessionNotFound)),
            Uuid::new_v4(),
            Request::builder()
                .method(Method::POST)
                .body(Body::empty())
                .unwrap(),
            move |_| {
                observed_calls.fetch_add(1, Ordering::SeqCst);
                async { Response::new(Body::empty()) }
            },
        )
        .await;

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("unknown session reached next"),
        };
        let response = error.into_response();
        assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(
            response.headers().get(crate::ERROR_CODE_HEADER).unwrap(),
            "session_not_found"
        );
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), br#"{"status":400,"message":"Bad Request"}"#);
    }

    #[tokio::test]
    async fn successful_session_check_calls_next_once_with_extensions() {
        let session_id = Uuid::new_v4();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed_calls = calls.clone();

        forward_bodyless_request::<(), _, _, _, _>(
            ready(Ok::<(), ApiError>(())),
            session_id,
            Request::builder()
                .method(Method::POST)
                .body(Body::empty())
                .unwrap(),
            move |request| {
                assert_eq!(
                    request.extensions().get::<TransportSession>(),
                    Some(&TransportSession::v1(session_id))
                );
                assert!(request.extensions().get::<()>().is_some());
                observed_calls.fetch_add(1, Ordering::SeqCst);
                async { Response::new(Body::empty()) }
            },
        )
        .await
        .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    struct DropSpy(Arc<AtomicUsize>);

    impl Drop for DropSpy {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn unpolled_response_holds_resource_until_body_drop() {
        let drops = Arc::new(AtomicUsize::new(0));
        let response = Response::new(Body::from("encrypted response"));
        let response = hold_resource_through_response_body(response, DropSpy(drops.clone()));

        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(response);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn streaming_response_lease_blocks_eviction_until_body_drop() {
        let mut cache =
            LeaseAwareTtlCache::new(NonZeroUsize::new(1).unwrap(), Duration::from_secs(60));
        cache.insert_evicting(1, "active stream").unwrap();
        let lease = cache.acquire(&1).unwrap();

        let stream = futures::stream::pending::<Result<Bytes, io::Error>>();
        let response = Response::new(Body::from_stream(stream));
        let response = hold_resource_through_response_body(response, lease);

        assert_eq!(
            cache.insert_evicting(2, "new session"),
            Err(InsertError::AllEntriesLeased)
        );
        drop(response);
        assert!(cache.insert_evicting(2, "new session").is_ok());
    }

    #[tokio::test]
    async fn wrapped_body_preserves_payload_and_exact_size_hint() {
        let drops = Arc::new(AtomicUsize::new(0));
        let response = Response::new(Body::from("encrypted response"));
        let response = hold_resource_through_response_body(response, DropSpy(drops.clone()));

        assert_eq!(response.body().size_hint().exact(), Some(18));
        let bytes = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"encrypted response");
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    struct DataThenTrailersBody {
        next_frame: u8,
    }

    impl HttpBody for DataThenTrailersBody {
        type Data = Bytes;
        type Error = Infallible;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
            let frame = match self.next_frame {
                0 => Some(Frame::data(Bytes::from_static(b"x"))),
                1 => {
                    let mut trailers = HeaderMap::new();
                    trailers.insert("x-test-trailer", HeaderValue::from_static("preserved"));
                    Some(Frame::trailers(trailers))
                }
                _ => None,
            };
            self.next_frame += 1;
            Poll::Ready(frame.map(Ok))
        }

        fn is_end_stream(&self) -> bool {
            self.next_frame > 1
        }

        fn size_hint(&self) -> SizeHint {
            SizeHint::with_exact(1)
        }
    }

    #[tokio::test]
    async fn wrapped_body_preserves_trailers_and_lease_through_end_of_stream() {
        let drops = Arc::new(AtomicUsize::new(0));
        let response = Response::new(Body::new(DataThenTrailersBody { next_frame: 0 }));
        let response = hold_resource_through_response_body(response, DropSpy(drops.clone()));
        let mut body = Box::pin(response.into_body());

        let data = poll_fn(|context| body.as_mut().poll_frame(context))
            .await
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap();
        assert_eq!(&data[..], b"x");

        let trailers = poll_fn(|context| body.as_mut().poll_frame(context))
            .await
            .unwrap()
            .unwrap()
            .into_trailers()
            .unwrap();
        assert_eq!(trailers["x-test-trailer"], "preserved");
        assert!(poll_fn(|context| body.as_mut().poll_frame(context))
            .await
            .is_none());
        assert!(body.is_end_stream());
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        drop(body);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    struct DataThenErrorBody {
        next_frame: u8,
    }

    impl HttpBody for DataThenErrorBody {
        type Data = Bytes;
        type Error = io::Error;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
            let frame = match self.next_frame {
                0 => Some(Ok(Frame::data(Bytes::from_static(b"x")))),
                1 => Some(Err(io::Error::other("test body failure"))),
                _ => None,
            };
            self.next_frame += 1;
            Poll::Ready(frame)
        }
    }

    #[tokio::test]
    async fn wrapped_body_preserves_errors_and_resource_until_drop() {
        let drops = Arc::new(AtomicUsize::new(0));
        let response = Response::new(Body::new(DataThenErrorBody { next_frame: 0 }));
        let response = hold_resource_through_response_body(response, DropSpy(drops.clone()));
        let mut body = Box::pin(response.into_body());

        let data = poll_fn(|context| body.as_mut().poll_frame(context))
            .await
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap();
        assert_eq!(&data[..], b"x");

        let error = poll_fn(|context| body.as_mut().poll_frame(context))
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("test body failure"));
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        drop(body);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}
