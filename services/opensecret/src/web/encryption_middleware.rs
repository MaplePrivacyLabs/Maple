use axum::{
    body::Body,
    extract::FromRequestParts,
    http::{request::Parts, Request},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::de::{DeserializeOwned, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::Serialize;
use std::fmt;

use crate::{transport_v2::crypto::SessionId, ApiError, AppState};

const MAX_LOGICAL_BODY_BYTES: usize = 50 * 1024 * 1024; // 50MB

// Containers and scalar values each count as one node; object member names do
// not count separately. String bytes are already covered by the body-size
// limit and do not increase this count.
const MAX_JSON_NODES: usize = 1_048_576;

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

fn validate_json_body(input: &[u8], max_nodes: usize) -> Result<(), ApiError> {
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

/// The attested Transport V2 session a request arrived on.
///
/// Only the Transport V2 gateway can attach this marker: it is an in-process
/// request extension, so an HTTP caller cannot fabricate one. Its presence is
/// the application's proof that the gateway authenticated and decrypted the
/// request, and that the gateway will encrypt the handler's response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransportSession {
    session_id: SessionId,
}

impl TransportSession {
    pub(crate) fn new(session_id: SessionId) -> Self {
        Self { session_id }
    }

    pub(crate) fn session_id(&self) -> SessionId {
        self.session_id
    }
}

/// Parses the logical request body the Transport V2 gateway already decrypted.
///
/// Fails closed for a request that did not come through the gateway: without
/// its `TransportSession` marker no transport security was established, so no
/// handler may run.
pub async fn decrypt_request<T>(
    mut request: Request<Body>,
    next: Next,
) -> Result<Response, ApiError>
where
    T: DeserializeOwned + Send + Sync + Clone + 'static,
{
    if request.extensions().get::<TransportSession>().is_none() {
        return Err(ApiError::BadRequest);
    }

    if std::any::TypeId::of::<T>() == std::any::TypeId::of::<()>() {
        request.extensions_mut().insert(());
    } else {
        let body = std::mem::replace(request.body_mut(), Body::empty());
        let body_bytes = axum::body::to_bytes(body, MAX_LOGICAL_BODY_BYTES)
            .await
            .map_err(|_| ApiError::PayloadTooLarge)?;
        validate_json_body(&body_bytes, MAX_JSON_NODES)?;
        let decoded = serde_json::from_slice::<T>(&body_bytes).map_err(|_| ApiError::BadRequest)?;
        store_decrypted(&mut request, decoded);
    }

    Ok(next.run(request).await)
}

/// Serializes a handler's logical JSON response. The Transport V2 gateway
/// encrypts the whole response carrier, so the body leaves the handler as
/// ordinary JSON.
pub async fn encrypt_response<T: Serialize>(
    _state: &AppState,
    _transport_session: &TransportSession,
    response: &T,
) -> Result<Response, ApiError> {
    let response_json = serde_json::to_vec(response).map_err(|_| ApiError::InternalServerError)?;
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        response_json,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        http::{Method, StatusCode},
        middleware::from_fn,
        routing::post,
        Extension, Router,
    };
    use serde::Deserialize;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tower::ServiceExt;

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
    fn json_preflight_distinguishes_invalid_json_from_excess_structure() {
        assert!(matches!(
            validate_json_body(br#"[1,]"#, 16),
            Err(ApiError::BadRequest)
        ));
        assert!(matches!(
            validate_json_body(br#"[1,2]"#, 2),
            Err(ApiError::PayloadTooLarge)
        ));
        assert!(validate_json_body(br#"[1,2]"#, 3).is_ok());
    }

    #[derive(Clone, Deserialize)]
    struct Echo {
        value: String,
    }

    fn counting_router(calls: Arc<AtomicUsize>) -> Router<()> {
        Router::new()
            .route(
                "/echo",
                post({
                    let calls = Arc::clone(&calls);
                    move |Decrypted(body): Decrypted<Echo>,
                          Extension(_session): Extension<TransportSession>| {
                        let calls = Arc::clone(&calls);
                        async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            body.value
                        }
                    }
                })
                .layer(from_fn(decrypt_request::<Echo>)),
            )
            .route(
                "/bodyless",
                post({
                    let calls = Arc::clone(&calls);
                    move |Decrypted(()): Decrypted<()>| {
                        let calls = Arc::clone(&calls);
                        async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            StatusCode::NO_CONTENT
                        }
                    }
                })
                .layer(from_fn(decrypt_request::<()>)),
            )
    }

    fn request(
        target: &str,
        body: &'static str,
        session: Option<TransportSession>,
    ) -> Request<Body> {
        let mut request = Request::builder()
            .method(Method::POST)
            .uri(target)
            .body(Body::from(body))
            .unwrap();
        if let Some(session) = session {
            request.extensions_mut().insert(session);
        }
        request
    }

    fn gateway_session() -> TransportSession {
        TransportSession::new(SessionId::from_bytes([7; 16]))
    }

    #[tokio::test]
    async fn request_without_a_gateway_session_never_reaches_a_handler() {
        let calls = Arc::new(AtomicUsize::new(0));
        let router = counting_router(Arc::clone(&calls));

        for (target, body) in [("/echo", r#"{"value":"hello"}"#), ("/bodyless", "")] {
            let response = router
                .clone()
                .oneshot(request(target, body, None))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{target}");
            assert!(response.headers().get(crate::ERROR_CODE_HEADER).is_none());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn gateway_request_body_is_parsed_once_for_its_handler() {
        let calls = Arc::new(AtomicUsize::new(0));
        let router = counting_router(Arc::clone(&calls));

        let response = router
            .clone()
            .oneshot(request(
                "/echo",
                r#"{"value":"hello"}"#,
                Some(gateway_session()),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(&body[..], b"hello");

        let response = router
            .clone()
            .oneshot(request("/bodyless", "", Some(gateway_session())))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        let response = router
            .oneshot(request("/echo", "not json", Some(gateway_session())))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
