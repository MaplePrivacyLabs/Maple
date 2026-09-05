use crate::{
    error::{Error, Result},
    pcr::{Pcr0Environment, Pcr0TrustPolicy},
    session::{CredentialFence, SessionManager},
    transport_v2::{
        client::{ResponseEventStream, TransportV2Client, TransportV2Session},
        envelope::{CacheNamespaceRoot, Credential, CredentialKind, LogicalHeader, LogicalRequest},
        framing::ResponseEvent,
        TransportV2Error, ROUTING_KEY_HEADER,
    },
    types::*,
};
use base64::{
    engine::general_purpose::{STANDARD as BASE64, URL_SAFE_NO_PAD},
    Engine,
};
use bytes::{Bytes, BytesMut};
use futures::{Stream, StreamExt};
use http::{header, HeaderMap as HttpHeaderMap, Request as HttpRequest, Response as HttpResponse};
use p256::elliptic_curve::rand_core::{OsRng, RngCore};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    fmt,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, RwLock,
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;
use uuid::Uuid;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

/// A decrypted response body returned by [`OpenSecretClient::send_inference_request`].
///
/// Ordinary responses contain one or more raw chunks. Server-sent event
/// responses remain incremental and are forwarded without interpreting their
/// payloads as JSON.
pub type OpenSecretResponseBody = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send + 'static>>;

/// A caller-owned request to an allowed OpenSecret inference endpoint.
pub type InferenceRequest = HttpRequest<Bytes>;

/// A decrypted HTTP response from an OpenSecret inference endpoint.
pub type InferenceResponse = HttpResponse<OpenSecretResponseBody>;

/// A cloneable, request-local ceiling on inference HTTP sends.
///
/// Attestation, token refresh, and other control-plane requests do not consume
/// this budget. A permit is reserved only immediately before an inference HTTP
/// request is sent.
#[derive(Clone, Debug)]
pub struct InferenceSendBudget {
    remaining: Arc<AtomicUsize>,
}

impl InferenceSendBudget {
    /// Creates a budget that permits at most `max_sends` inference HTTP sends.
    pub fn new(max_sends: usize) -> Result<Self> {
        if max_sends == 0 {
            return Err(Error::Configuration(
                "Inference send budget must allow at least one send".to_string(),
            ));
        }
        Ok(Self {
            remaining: Arc::new(AtomicUsize::new(max_sends)),
        })
    }

    /// Returns the number of inference HTTP sends still available.
    pub fn remaining(&self) -> usize {
        self.remaining.load(Ordering::Acquire)
    }

    /// Reserves one inference HTTP send, returning false when exhausted.
    pub fn try_reserve_send(&self) -> bool {
        self.remaining
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
    }
}

struct LogicalRequestOptions<'a> {
    include_cache_namespace: bool,
    inference_send_budget: Option<&'a InferenceSendBudget>,
}

type TransportSessionSlot = Arc<RwLock<Option<Arc<TransportV2Session>>>>;
type SessionRetirement = (TransportSessionSlot, Arc<TransportV2Session>);

const MAX_NATIVE_OAUTH_HANDOFF_GRANT_BYTES: usize = 4 * 1024;

/// One exact Transport V2 request reserved for native OAuth redemption.
///
/// This handle is intentionally opaque and non-Clone. Its advertised IDs are
/// public routing values; session keys and the prepared request capability
/// never leave the SDK.
pub struct PreparedNativeOAuthHandoff {
    session: Arc<TransportV2Session>,
    prepared_request: crate::transport_v2::client::PreparedRequestId,
    session_id: String,
    request_id: String,
    credential_generation: u64,
}

impl PreparedNativeOAuthHandoff {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    /// Compares the unverified routing hints carried by a handoff grant with
    /// this prepared request.
    ///
    /// This is only a local denial-of-service filter. The enclave remains the
    /// authority for the grant signature, expiry, principal, and target
    /// binding before it authenticates the session.
    pub fn matches_untrusted_grant_target(&self, grant: &NativeOAuthHandoffGrant) -> bool {
        grant.untrusted_target().is_some_and(|target| {
            target.session_id == self.session_id && target.request_id == self.request_id
        })
    }
}

impl fmt::Debug for PreparedNativeOAuthHandoff {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedNativeOAuthHandoff")
            .field("session_id", &self.session_id)
            .field("request_id", &self.request_id)
            .finish_non_exhaustive()
    }
}

/// A short-lived signed grant minted by the authenticated hosted browser.
pub struct NativeOAuthHandoffGrant(Zeroizing<String>);

#[derive(Deserialize)]
struct UntrustedNativeOAuthHandoffTarget {
    #[serde(rename = "sid")]
    session_id: String,
    #[serde(rename = "rid")]
    request_id: String,
}

impl NativeOAuthHandoffGrant {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty() || value.len() > MAX_NATIVE_OAUTH_HANDOFF_GRANT_BYTES {
            return Err(Error::Authentication(
                "Native OAuth handoff grant is invalid".to_string(),
            ));
        }
        let segments = value.split('.').collect::<Vec<_>>();
        if segments.len() != 3
            || segments.iter().any(|segment| {
                segment.is_empty()
                    || URL_SAFE_NO_PAD
                        .decode(segment)
                        .ok()
                        .is_none_or(|decoded| URL_SAFE_NO_PAD.encode(decoded) != *segment)
            })
        {
            return Err(Error::Authentication(
                "Native OAuth handoff grant is invalid".to_string(),
            ));
        }
        Ok(Self(Zeroizing::new(value)))
    }

    fn as_str(&self) -> &str {
        self.0.as_str()
    }

    fn untrusted_target(&self) -> Option<UntrustedNativeOAuthHandoffTarget> {
        let payload = self.0.split('.').nth(1)?;
        let decoded = Zeroizing::new(URL_SAFE_NO_PAD.decode(payload).ok()?);
        let target: UntrustedNativeOAuthHandoffTarget =
            serde_json::from_slice(decoded.as_slice()).ok()?;
        if !is_canonical_transport_id(&target.session_id)
            || !is_canonical_transport_id(&target.request_id)
        {
            return None;
        }
        Some(target)
    }
}

fn is_canonical_transport_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

impl fmt::Debug for NativeOAuthHandoffGrant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NativeOAuthHandoffGrant([REDACTED])")
    }
}

#[derive(Serialize)]
struct NativeOAuthHandoffRedeemRequest<'a> {
    grant: &'a str,
}

/// Caller-persistable provider-cache namespace root for Transport V2.
///
/// Applications that need cache hits across restarts should generate this
/// once, persist it as a secret, and restore it when constructing the client.
#[derive(Clone, Eq, PartialEq, Zeroize, ZeroizeOnDrop)]
pub struct TransportV2CacheNamespaceRoot([u8; 32]);

impl TransportV2CacheNamespaceRoot {
    pub fn generate() -> Result<Self> {
        let mut bytes = [0_u8; 32];
        OsRng
            .try_fill_bytes(&mut bytes)
            .map_err(|_| Error::Encryption("Secure randomness was unavailable".to_string()))?;
        Ok(Self(bytes))
    }

    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn from_base64(encoded: &str) -> Result<Self> {
        let decoded = Zeroizing::new(BASE64.decode(encoded)?);
        if decoded.len() != 32 || BASE64.encode(decoded.as_slice()) != encoded {
            return Err(Error::Configuration(
                "Transport V2 cache namespace root must be canonical padded base64 for exactly 32 bytes"
                    .to_string(),
            ));
        }
        let mut bytes = [0_u8; 32];
        bytes.copy_from_slice(decoded.as_slice());
        Ok(Self(bytes))
    }

    pub fn to_base64(&self) -> String {
        BASE64.encode(self.0)
    }

    fn expose_bytes(&self) -> [u8; 32] {
        self.0
    }
}

impl std::fmt::Debug for TransportV2CacheNamespaceRoot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("TransportV2CacheNamespaceRoot([REDACTED])")
    }
}

pub struct OpenSecretClient {
    session_manager: SessionManager,
    refresh_lock: Mutex<()>,
    session_gate: Mutex<()>,
    transport_session: TransportSessionSlot,
    transport_v2: TransportV2Client,
    cache_namespace_root: TransportV2CacheNamespaceRoot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedAuth {
    token: Option<String>,
    source: CredentialSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CredentialSource {
    Anonymous,
    StoredAccessToken { generation: u64 },
    StoredApiKey { generation: u64 },
    StoredRefreshToken { generation: u64 },
    ExplicitApiKey,
}

impl Drop for ResolvedAuth {
    fn drop(&mut self) {
        if let Some(token) = &mut self.token {
            token.zeroize();
        }
    }
}

impl ResolvedAuth {
    fn credential_kind(&self) -> Option<CredentialKind> {
        match self.source {
            CredentialSource::Anonymous => None,
            CredentialSource::StoredAccessToken { .. } => Some(CredentialKind::Bearer),
            CredentialSource::StoredApiKey { .. } | CredentialSource::ExplicitApiKey => {
                Some(CredentialKind::ApiKey)
            }
            CredentialSource::StoredRefreshToken { .. } => Some(CredentialKind::Resumption),
        }
    }

    fn fence(&self) -> Option<CredentialFence<'_>> {
        let value = self.token.as_deref()?;
        match self.source {
            CredentialSource::Anonymous | CredentialSource::ExplicitApiKey => None,
            CredentialSource::StoredAccessToken { generation } => {
                Some(CredentialFence::AccessToken { generation, value })
            }
            CredentialSource::StoredApiKey { generation } => {
                Some(CredentialFence::ApiKey { generation, value })
            }
            CredentialSource::StoredRefreshToken { generation } => {
                Some(CredentialFence::RefreshToken { generation, value })
            }
        }
    }
}

fn append_query_param(query: &mut Vec<String>, key: &str, value: impl ToString) {
    let encoded = utf8_percent_encode(&value.to_string(), NON_ALPHANUMERIC).to_string();
    query.push(format!("{}={}", key, encoded));
}

fn build_agent_items_endpoint(base: &str, params: Option<&AgentItemsListParams>) -> String {
    let mut endpoint = base.to_string();
    let mut query = Vec::new();

    if let Some(params) = params {
        if let Some(limit) = params.limit {
            append_query_param(&mut query, "limit", limit);
        }
        if let Some(after) = params.after {
            append_query_param(&mut query, "after", after);
        }
        if let Some(order) = &params.order {
            append_query_param(&mut query, "order", order);
        }
        if let Some(include) = &params.include {
            for include_value in include {
                append_query_param(&mut query, "include", include_value);
            }
        }
    }

    if !query.is_empty() {
        endpoint.push('?');
        endpoint.push_str(&query.join("&"));
    }

    endpoint
}

fn build_subagents_endpoint(params: Option<&ListSubagentsParams>) -> String {
    let mut endpoint = "/v1/agent/subagents".to_string();
    let mut query = Vec::new();

    if let Some(params) = params {
        if let Some(limit) = params.limit {
            append_query_param(&mut query, "limit", limit);
        }
        if let Some(after) = params.after {
            append_query_param(&mut query, "after", after);
        }
        if let Some(order) = &params.order {
            append_query_param(&mut query, "order", order);
        }
        if let Some(created_by) = &params.created_by {
            append_query_param(&mut query, "created_by", created_by);
        }
    }

    if !query.is_empty() {
        endpoint.push('?');
        endpoint.push_str(&query.join("&"));
    }

    endpoint
}

fn build_conversations_endpoint(params: Option<&ConversationsListParams>) -> String {
    let mut endpoint = "/v1/conversations".to_string();
    let mut query = Vec::new();

    if let Some(params) = params {
        if let Some(limit) = params.limit {
            append_query_param(&mut query, "limit", limit);
        }
        if let Some(after) = params.after {
            append_query_param(&mut query, "after", after);
        }
        if let Some(order) = &params.order {
            append_query_param(&mut query, "order", order);
        }
        if let Some(project_id) = params.project_id {
            append_query_param(&mut query, "project_id", project_id);
        }
        if let Some(unassigned_project) = params.unassigned_project {
            append_query_param(&mut query, "unassigned_project", unassigned_project);
        }
        if let Some(pinned) = params.pinned {
            append_query_param(&mut query, "pinned", pinned);
        }
    }

    if !query.is_empty() {
        endpoint.push('?');
        endpoint.push_str(&query.join("&"));
    }

    endpoint
}

fn build_conversation_projects_endpoint(params: Option<&ConversationProjectListParams>) -> String {
    let mut endpoint = "/v1/conversation-projects".to_string();
    let mut query = Vec::new();

    if let Some(params) = params {
        if let Some(limit) = params.limit {
            append_query_param(&mut query, "limit", limit);
        }
        if let Some(after) = params.after {
            append_query_param(&mut query, "after", after);
        }
        if let Some(order) = &params.order {
            append_query_param(&mut query, "order", order);
        }
    }

    if !query.is_empty() {
        endpoint.push('?');
        endpoint.push_str(&query.join("&"));
    }

    endpoint
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AuthHeaderMode {
    None,
    Jwt,
    ApiKeyOrJwt,
}

fn is_allowed_inference_endpoint(method: &http::Method, path: &str) -> bool {
    matches!(
        (method.as_str(), path),
        ("GET", "/v1/models")
            | ("GET", "/v1/models/catalog")
            | ("POST", "/v1/chat/completions")
            | ("POST", "/v1/embeddings")
            | ("POST", "/v1/audio/speech")
            | ("POST", "/v1/audio/transcriptions")
    )
}

fn is_hop_by_hop_header(name: &http::HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn sanitize_inference_request_headers(headers: &HttpHeaderMap) -> HttpHeaderMap {
    let connection_headers = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .filter_map(|value| http::HeaderName::from_bytes(value.as_bytes()).ok())
        .collect::<Vec<_>>();

    let mut sanitized = HttpHeaderMap::new();
    for (name, value) in headers {
        if !is_hop_by_hop_header(name)
            && !connection_headers.contains(name)
            && *name != header::HOST
            && *name != header::AUTHORIZATION
            && *name != header::COOKIE
            && *name != header::SET_COOKIE
            && name.as_str() != "x-session-id"
            && name.as_str() != ROUTING_KEY_HEADER
            && name.as_str() != "forwarded"
            && name.as_str() != "via"
            && name.as_str() != "x-forwarded-for"
            && name.as_str() != "x-forwarded-host"
            && name.as_str() != "x-forwarded-proto"
            && *name != header::CONTENT_LENGTH
            && *name != header::CONTENT_ENCODING
            && *name != header::ACCEPT_ENCODING
            && name.as_str() != "content-md5"
            && name.as_str() != "digest"
        {
            sanitized.append(name.clone(), value.clone());
        }
    }
    sanitized
}

fn sanitize_inference_response_headers(headers: &HttpHeaderMap) -> HttpHeaderMap {
    let connection_headers = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .filter_map(|value| http::HeaderName::from_bytes(value.as_bytes()).ok())
        .collect::<Vec<_>>();

    let mut sanitized = HttpHeaderMap::new();
    for (name, value) in headers {
        if !is_hop_by_hop_header(name)
            && !connection_headers.contains(name)
            && *name != header::CONTENT_LENGTH
            && *name != header::CONTENT_ENCODING
            && name.as_str() != "content-md5"
            && name.as_str() != "digest"
            && *name != header::ETAG
        {
            sanitized.append(name.clone(), value.clone());
        }
    }
    sanitized
}

fn permits_session_recovery(target: &str) -> bool {
    !matches!(
        target.split('?').next().unwrap_or(target),
        "/auth/github/callback"
            | "/auth/google/callback"
            | "/auth/apple/callback"
            | "/auth/native-handoff/redeem"
    )
}

async fn collect_response_body(mut body: OpenSecretResponseBody) -> Result<Bytes> {
    let mut collected = BytesMut::new();
    while let Some(chunk) = body.next().await {
        collected.extend_from_slice(&chunk?);
    }
    Ok(collected.freeze())
}

fn transport_error(error: TransportV2Error) -> Error {
    match error {
        TransportV2Error::InvalidConfiguration => {
            Error::Configuration("Transport V2 configuration is invalid".to_string())
        }
        TransportV2Error::AttestationRejected => Error::AttestationVerificationFailed(
            "Transport V2 attestation or PCR policy was rejected".to_string(),
        ),
        TransportV2Error::SessionExpired => {
            Error::Session("Transport V2 session expired".to_string())
        }
        TransportV2Error::Http(error) => Error::Http(error),
        TransportV2Error::UntrustedOuterResponse | TransportV2Error::SessionRecoveryHint(_) => {
            Error::InvalidResponse(
                "Transport V2 returned an unauthenticated outer response".to_string(),
            )
        }
        TransportV2Error::Authentication
        | TransportV2Error::InvalidFrame
        | TransportV2Error::InvalidRecord
        | TransportV2Error::InvalidSequence
        | TransportV2Error::TruncatedResponse
        | TransportV2Error::PostTerminalData => Error::InvalidResponse(
            "Transport V2 response authentication or framing failed".to_string(),
        ),
        _ => Error::Encryption(error.to_string()),
    }
}

fn should_retire_transport_session(error: &TransportV2Error) -> bool {
    matches!(
        error,
        TransportV2Error::SessionExpired
            | TransportV2Error::Authentication
            | TransportV2Error::UntrustedOuterResponse
            | TransportV2Error::SessionRecoveryHint(_)
            | TransportV2Error::InvalidFrame
            | TransportV2Error::InvalidRecord
            | TransportV2Error::InvalidSequence
            | TransportV2Error::TruncatedResponse
            | TransportV2Error::PostTerminalData
    )
}

fn retire_transport_session_after_stream_failure(retirement: &Option<SessionRetirement>) {
    let Some((slot, expected)) = retirement else {
        return;
    };
    let Ok(mut current) = slot.write() else {
        return;
    };
    if current
        .as_ref()
        .is_some_and(|session| Arc::ptr_eq(session, expected))
    {
        *current = None;
    }
}

fn response_body_from_events(
    mut events: ResponseEventStream,
    retirement: Option<SessionRetirement>,
) -> OpenSecretResponseBody {
    Box::pin(async_stream::try_stream! {
        while let Some(event) = events.next().await {
            let event = match event {
                Ok(event) => event,
                Err(error) => {
                    if should_retire_transport_session(&error) {
                        retire_transport_session_after_stream_failure(&retirement);
                    }
                    Err(transport_error(error))?
                }
            };
            match event {
                ResponseEvent::Chunk(bytes) => yield bytes,
                ResponseEvent::End => return,
                ResponseEvent::Error { code } => {
                    Err(Error::InvalidResponse(format!(
                        "Transport V2 response stream failed ({code})"
                    )))?;
                }
                ResponseEvent::Start { .. } => {
                    retire_transport_session_after_stream_failure(&retirement);
                    Err(Error::InvalidResponse(
                        "Transport V2 response contained more than one start record".to_string(),
                    ))?;
                }
            }
        }
        retire_transport_session_after_stream_failure(&retirement);
        Err(Error::InvalidResponse(
            "Transport V2 response ended without authenticated finality".to_string(),
        ))?;
    })
}

async fn response_from_events(
    mut events: ResponseEventStream,
    retirement: Option<SessionRetirement>,
) -> Result<InferenceResponse> {
    let Some(start) = events.next().await else {
        retire_transport_session_after_stream_failure(&retirement);
        return Err(Error::InvalidResponse(
            "Transport V2 response ended before its start record".to_string(),
        ));
    };
    let start = match start {
        Ok(start) => start,
        Err(error) => {
            if should_retire_transport_session(&error) {
                retire_transport_session_after_stream_failure(&retirement);
            }
            return Err(transport_error(error));
        }
    };
    let ResponseEvent::Start { status, headers } = start else {
        retire_transport_session_after_stream_failure(&retirement);
        return Err(Error::InvalidResponse(
            "Transport V2 response did not begin with a start record".to_string(),
        ));
    };
    let status = http::StatusCode::from_u16(status)
        .map_err(|_| Error::InvalidResponse("Transport V2 status was invalid".to_string()))?;
    let mut response_headers = HttpHeaderMap::new();
    for logical in headers {
        let name = http::HeaderName::from_bytes(logical.name().as_bytes()).map_err(|_| {
            Error::InvalidResponse("Transport V2 header name was invalid".to_string())
        })?;
        let value = http::HeaderValue::from_str(logical.value()).map_err(|_| {
            Error::InvalidResponse("Transport V2 header value was invalid".to_string())
        })?;
        response_headers.append(name, value);
    }
    let mut response = HttpResponse::new(response_body_from_events(events, retirement));
    *response.status_mut() = status;
    *response.headers_mut() = response_headers;
    Ok(response)
}

#[derive(Deserialize)]
struct JwtTimingClaims {
    exp: u64,
}

fn access_token_needs_refresh(token: &str, now: u64) -> bool {
    let mut segments = token.split('.');
    let (Some(_header), Some(payload), Some(_signature), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return false;
    };
    URL_SAFE_NO_PAD
        .decode(payload)
        .ok()
        .and_then(|payload| serde_json::from_slice::<JwtTimingClaims>(&payload).ok())
        .is_some_and(|claims| claims.exp <= now.saturating_add(30))
}

fn build_chat_completion_stream_request(
    mut request: ChatCompletionRequest,
) -> Result<InferenceRequest> {
    request.stream = Some(true);
    request.stream_options = Some(StreamOptions {
        include_usage: true,
    });
    HttpRequest::builder()
        .method(http::Method::POST)
        .uri("/v1/chat/completions")
        .header(header::ACCEPT, "text/event-stream")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Bytes::from(serde_json::to_vec(&request)?))
        .map_err(|error| {
            Error::Configuration(format!("Failed to build inference request: {error}"))
        })
}

fn parse_agent_sse_event(event_type: &str, data: &str) -> Option<Result<AgentSseEvent>> {
    let parsed = match event_type {
        "agent.message" => serde_json::from_str(data).map(AgentSseEvent::Message),
        "agent.reaction" => serde_json::from_str(data).map(AgentSseEvent::Reaction),
        "agent.typing" => serde_json::from_str(data).map(AgentSseEvent::Typing),
        "agent.done" => serde_json::from_str(data).map(AgentSseEvent::Done),
        "agent.error" => serde_json::from_str(data).map(AgentSseEvent::Error),
        _ => return None,
    };
    Some(parsed.map_err(|error| Error::Api {
        status: 0,
        message: format!("Failed to parse {event_type}: {error}"),
    }))
}

impl OpenSecretClient {
    /// Construct a client using the official production PCR0 trust roots.
    pub fn new(base_url: impl Into<String>) -> Result<Self> {
        Self::new_with_pcr0_environment(base_url, Pcr0Environment::default())
    }

    /// Construct a client using one explicit official PCR0 environment.
    pub fn new_with_pcr0_environment(
        base_url: impl Into<String>,
        pcr0_environment: Pcr0Environment,
    ) -> Result<Self> {
        Self::new_with_pcr0_trust_policy(base_url, Pcr0TrustPolicy::official_for(pcr0_environment))
    }

    /// Construct a client with an explicit PCR0 trust policy.
    pub fn new_with_pcr0_trust_policy(
        base_url: impl Into<String>,
        pcr0_trust_policy: Pcr0TrustPolicy,
    ) -> Result<Self> {
        let transport_v2 =
            TransportV2Client::new(base_url.into(), pcr0_trust_policy).map_err(transport_error)?;
        Ok(Self {
            session_manager: SessionManager::new(),
            refresh_lock: Mutex::new(()),
            session_gate: Mutex::new(()),
            transport_session: Arc::new(RwLock::new(None)),
            transport_v2,
            cache_namespace_root: TransportV2CacheNamespaceRoot::generate()?,
        })
    }

    /// Construct an API-key client using the official production PCR0 trust roots.
    pub fn new_with_api_key(base_url: impl Into<String>, api_key: String) -> Result<Self> {
        Self::new_with_api_key_and_pcr0_environment(base_url, api_key, Pcr0Environment::default())
    }

    /// Construct an API-key client using one explicit official PCR0 environment.
    pub fn new_with_api_key_and_pcr0_environment(
        base_url: impl Into<String>,
        api_key: String,
        pcr0_environment: Pcr0Environment,
    ) -> Result<Self> {
        Self::new_with_api_key_and_pcr0_trust_policy(
            base_url,
            api_key,
            Pcr0TrustPolicy::official_for(pcr0_environment),
        )
    }

    /// Construct an API-key client with an explicit PCR0 trust policy.
    pub fn new_with_api_key_and_pcr0_trust_policy(
        base_url: impl Into<String>,
        api_key: String,
        pcr0_trust_policy: Pcr0TrustPolicy,
    ) -> Result<Self> {
        let transport_v2 =
            TransportV2Client::new(base_url.into(), pcr0_trust_policy).map_err(transport_error)?;
        Ok(Self {
            session_manager: SessionManager::new_with_api_key(api_key),
            refresh_lock: Mutex::new(()),
            session_gate: Mutex::new(()),
            transport_session: Arc::new(RwLock::new(None)),
            transport_v2,
            cache_namespace_root: TransportV2CacheNamespaceRoot::generate()?,
        })
    }

    /// Replace the random per-client provider-cache namespace root.
    ///
    /// The root must be generated independently from account identifiers and
    /// credentials. Persist it when cache continuity across restarts matters.
    #[must_use]
    pub fn with_cache_namespace_root(mut self, root: TransportV2CacheNamespaceRoot) -> Self {
        self.cache_namespace_root = root;
        self
    }

    pub fn set_api_key(&self, api_key: String) -> Result<()> {
        self.session_manager.set_api_key(api_key)
    }

    pub fn clear_api_key(&self) -> Result<()> {
        self.session_manager.clear_api_key()
    }

    pub async fn perform_attestation_handshake(&self) -> Result<()> {
        let _gate = self.session_gate.lock().await;
        let session = Arc::new(
            self.transport_v2
                .establish_session()
                .await
                .map_err(transport_error)?,
        );
        let mut current = self
            .transport_session
            .write()
            .map_err(|_| Error::Session("Transport V2 session state is unavailable".to_string()))?;
        *current = Some(session);
        Ok(())
    }

    /// Reserve one exact anonymous request for a hosted native OAuth handoff.
    ///
    /// The returned IDs may be sent to the hosted browser so the enclave can
    /// bind its signed grant to this request. The handle itself must remain in
    /// the native process and is consumed exactly once by redemption.
    pub async fn prepare_native_oauth_handoff(&self) -> Result<PreparedNativeOAuthHandoff> {
        let credential_generation = self.session_manager.anonymous_generation()?;
        let session = self.ensure_transport_session().await?;
        let prepared_request = {
            let current = self.transport_session.read().map_err(|_| {
                Error::Session("Transport V2 session state is unavailable".to_string())
            })?;
            if !current
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &session))
            {
                return Err(Error::Session(
                    "Native OAuth transport session is no longer current".to_string(),
                ));
            }
            self.session_manager
                .admit_if_anonymous_generation_is_current(credential_generation, || {
                    session.prepare_request_id().map_err(transport_error)
                })?
        };
        let session_id = session.encoded_id();
        let request_id = prepared_request.encoded();
        Ok(PreparedNativeOAuthHandoff {
            session,
            prepared_request,
            session_id,
            request_id,
            credential_generation,
        })
    }

    /// Redeem a hosted handoff grant through the exact request prepared above.
    ///
    /// No credential or cache namespace root is attached. This method sends at
    /// most once and never allocates a replacement request ID.
    pub async fn redeem_native_oauth_handoff(
        &self,
        prepared: PreparedNativeOAuthHandoff,
        grant: NativeOAuthHandoffGrant,
    ) -> Result<LoginResponse> {
        // Hold the current-session read lock through sealing, and the
        // credential read lock inside it. A concurrent session replacement or
        // authentication change must linearize wholly before or after this
        // exact request becomes eligible for transmission.
        let sealed = {
            let current = self.transport_session.read().map_err(|_| {
                Error::Session("Transport V2 session state is unavailable".to_string())
            })?;
            let current = current.as_ref().ok_or_else(|| {
                Error::Session("Native OAuth transport session is no longer current".to_string())
            })?;
            if current.is_expired() || !Arc::ptr_eq(current, &prepared.session) {
                return Err(Error::Session(
                    "Native OAuth transport session is no longer current".to_string(),
                ));
            }
            self.session_manager
                .admit_if_anonymous_generation_is_current(prepared.credential_generation, || {
                    let body = Bytes::from(serde_json::to_vec(&NativeOAuthHandoffRedeemRequest {
                        grant: grant.as_str(),
                    })?);
                    let request = LogicalRequest::new(
                        None,
                        None,
                        http::Method::POST,
                        "/auth/native-handoff/redeem".to_string(),
                        vec![LogicalHeader::new(
                            header::CONTENT_TYPE.as_str().to_string(),
                            "application/json".to_string(),
                        )
                        .map_err(transport_error)?],
                        Some(body),
                    )
                    .map_err(transport_error)?;
                    self.transport_v2
                        .seal_with_id(&prepared.session, prepared.prepared_request, request)
                        .map_err(transport_error)
                })?
        };
        drop(grant);

        let events = match self.transport_v2.send_sealed(sealed).await {
            Ok(events) => events,
            Err(error) => {
                // Delivery may have happened even when no authenticated
                // response arrived. The enclave may therefore have bound the
                // session while this client still appears anonymous.
                self.clear_transport_session_if(&prepared.session)?;
                return Err(transport_error(error));
            }
        };
        let response = match response_from_events(
            events,
            Some((
                Arc::clone(&self.transport_session),
                Arc::clone(&prepared.session),
            )),
        )
        .await
        {
            Ok(response) => response,
            Err(error) => {
                self.clear_transport_session_if(&prepared.session)?;
                return Err(error);
            }
        };
        let status = response.status();
        let body = match collect_response_body(response.into_body()).await {
            Ok(body) => body,
            Err(error) => {
                self.clear_transport_session_if(&prepared.session)?;
                return Err(error);
            }
        };
        if !status.is_success() {
            self.clear_transport_session_if(&prepared.session)?;
            return Err(Error::Api {
                status: status.as_u16(),
                message: String::from_utf8_lossy(&body).into_owned(),
            });
        }
        let mut response: LoginResponse = match serde_json::from_slice(&body) {
            Ok(response) => response,
            Err(error) => {
                self.clear_transport_session_if(&prepared.session)?;
                return Err(error.into());
            }
        };
        let installed = {
            // The response belongs to the retained session. Do not install it
            // into a client whose session was replaced while redemption was
            // in flight, even if its credential generation is still anonymous.
            let current = self.transport_session.read().map_err(|_| {
                Error::Session("Transport V2 session state is unavailable".to_string())
            })?;
            current
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &prepared.session))
                && self.session_manager.set_tokens_if_anonymous_generation(
                    prepared.credential_generation,
                    response.access_token.clone(),
                    response.refresh_token.clone(),
                )?
        };
        if !installed {
            response.access_token.zeroize();
            response.refresh_token.zeroize();
            self.clear_transport_session_if(&prepared.session)?;
            return Err(Error::Authentication(
                "Authentication changed during native OAuth handoff".to_string(),
            ));
        }
        Ok(response)
    }

    async fn ensure_transport_session(&self) -> Result<Arc<TransportV2Session>> {
        if let Some(session) = self.current_transport_session()? {
            if !session.is_expired() {
                return Ok(session);
            }
            self.clear_transport_session_if(&session)?;
        }

        let _gate = self.session_gate.lock().await;
        if let Some(session) = self.current_transport_session()? {
            if !session.is_expired() {
                return Ok(session);
            }
            self.clear_transport_session_if(&session)?;
        }

        let session = Arc::new(
            self.transport_v2
                .establish_session()
                .await
                .map_err(transport_error)?,
        );
        let mut current = self
            .transport_session
            .write()
            .map_err(|_| Error::Session("Transport V2 session state is unavailable".to_string()))?;
        *current = Some(Arc::clone(&session));
        Ok(session)
    }

    fn current_transport_session(&self) -> Result<Option<Arc<TransportV2Session>>> {
        self.transport_session
            .read()
            .map(|session| session.clone())
            .map_err(|_| Error::Session("Transport V2 session state is unavailable".to_string()))
    }

    fn clear_transport_session_if(&self, expected: &Arc<TransportV2Session>) -> Result<()> {
        let mut current = self
            .transport_session
            .write()
            .map_err(|_| Error::Session("Transport V2 session state is unavailable".to_string()))?;
        if current
            .as_ref()
            .is_some_and(|session| Arc::ptr_eq(session, expected))
        {
            *current = None;
        }
        Ok(())
    }

    pub fn get_session_id(&self) -> Result<Option<Uuid>> {
        let Some(session) = self.current_transport_session()? else {
            return Ok(None);
        };
        if session.is_expired() {
            self.clear_transport_session_if(&session)?;
            return Ok(None);
        }
        Ok(Some(Uuid::from_bytes(session.id_bytes())))
    }

    pub async fn test_connection(&self) -> Result<String> {
        let response = self
            .send_logical_request(
                http::Method::GET,
                "/health-check".to_string(),
                Vec::new(),
                None,
                ResolvedAuth {
                    token: None,
                    source: CredentialSource::Anonymous,
                },
                false,
            )
            .await?;
        let status = response.status();
        let body = collect_response_body(response.into_body()).await?;
        if !status.is_success() {
            return Err(Error::Api {
                status: status.as_u16(),
                message: String::from_utf8_lossy(&body).into_owned(),
            });
        }
        String::from_utf8(body.to_vec()).map_err(Into::into)
    }

    async fn encrypted_api_call<T: Serialize, U: DeserializeOwned>(
        &self,
        endpoint: &str,
        method: &str,
        data: Option<T>,
    ) -> Result<U> {
        let auth = self.resolve_auth(AuthHeaderMode::None)?;
        self.json_request(endpoint, method, data, auth).await
    }

    async fn authenticated_api_call<T: Serialize, U: DeserializeOwned>(
        &self,
        endpoint: &str,
        method: &str,
        data: Option<T>,
    ) -> Result<U> {
        let auth = self
            .resolve_auth_after_preflight(AuthHeaderMode::Jwt)
            .await?;
        self.json_request(endpoint, method, data, auth).await
    }

    async fn json_request<T: Serialize, U: DeserializeOwned>(
        &self,
        endpoint: &str,
        method: &str,
        data: Option<T>,
        auth: ResolvedAuth,
    ) -> Result<U> {
        let method = http::Method::from_bytes(method.as_bytes()).map_err(|error| {
            Error::Configuration(format!("Invalid logical HTTP method: {error}"))
        })?;
        let body = data
            .map(|data| serde_json::to_vec(&data).map(Bytes::from))
            .transpose()?;
        let headers = if body.is_some() {
            vec![LogicalHeader::new(
                header::CONTENT_TYPE.as_str().to_string(),
                "application/json".to_string(),
            )
            .map_err(transport_error)?]
        } else {
            Vec::new()
        };
        let response = self
            .send_logical_request(method, endpoint.to_string(), headers, body, auth, false)
            .await?;
        let status = response.status();
        let body = collect_response_body(response.into_body()).await?;
        if !status.is_success() {
            return Err(Error::Api {
                status: status.as_u16(),
                message: String::from_utf8_lossy(&body).into_owned(),
            });
        }
        Ok(serde_json::from_slice(&body)?)
    }

    async fn send_logical_request(
        &self,
        method: http::Method,
        target: String,
        headers: Vec<LogicalHeader>,
        body: Option<Bytes>,
        auth: ResolvedAuth,
        include_cache_namespace: bool,
    ) -> Result<InferenceResponse> {
        self.send_logical_request_with_options(
            method,
            target,
            headers,
            body,
            auth,
            LogicalRequestOptions {
                include_cache_namespace,
                inference_send_budget: None,
            },
        )
        .await
    }

    async fn send_logical_request_with_options(
        &self,
        method: http::Method,
        target: String,
        headers: Vec<LogicalHeader>,
        body: Option<Bytes>,
        auth: ResolvedAuth,
        options: LogicalRequestOptions<'_>,
    ) -> Result<InferenceResponse> {
        let permits_recovery = permits_session_recovery(&target);
        let anonymous_generation = matches!(auth.source, CredentialSource::Anonymous)
            .then(|| self.session_manager.credential_generation())
            .transpose()?;
        let credential = auth
            .credential_kind()
            .zip(auth.token.as_ref())
            .map(|(kind, token)| Credential::new(kind, token.clone()))
            .transpose()
            .map_err(transport_error)?;
        let cache_namespace_root = (options.include_cache_namespace && credential.is_some())
            .then(|| CacheNamespaceRoot::from_bytes(self.cache_namespace_root.expose_bytes()));
        let request = LogicalRequest::new(
            credential,
            cache_namespace_root,
            method,
            target,
            headers,
            body,
        )
        .map_err(transport_error)?;

        // Capture once: public inference bodies are owned Bytes, and typed
        // request bodies have already been serialized. A recovery must not
        // select a new credential or reconstruct any part of the request.
        let plaintext = request.encode().map_err(transport_error)?;
        drop(request);
        let mut recovery_used = false;
        loop {
            let session = self.ensure_transport_session().await?;
            // Fence each admission after session/attestation awaits, then seal
            // under the same credential read lock. Explicit per-request API
            // keys are immutable and independent of managed defaults.
            let sealed = self.session_manager.admit_if_credential_is_current(
                auth.fence().or_else(|| {
                    anonymous_generation
                        .map(|generation| CredentialFence::Generation { generation })
                }),
                || {
                    self.transport_v2
                        .seal_encoded(&session, &plaintext)
                        .map_err(transport_error)
                },
            )?;
            // Reserve only an actual inference HTTP send. Session recovery
            // must share this ceiling with the caller's outer retry layer.
            if let Some(send_budget) = options.inference_send_budget {
                if !send_budget.try_reserve_send() {
                    return Err(Error::Other(
                        "Inference request send budget exhausted".to_string(),
                    ));
                }
            }
            match self.transport_v2.send_sealed(sealed).await {
                Ok(events) => {
                    drop(auth);
                    drop(plaintext);
                    // Once encrypted response processing begins, neither
                    // authentication/framing errors nor partial bodies replay.
                    return response_from_events(
                        events,
                        Some((Arc::clone(&self.transport_session), session)),
                    )
                    .await;
                }
                Err(error) => {
                    if should_retire_transport_session(&error) {
                        self.clear_transport_session_if(&session)?;
                    }
                    if permits_recovery
                        && !recovery_used
                        && options
                            .inference_send_budget
                            .is_none_or(|send_budget| send_budget.remaining() > 0)
                        && matches!(error, TransportV2Error::SessionRecoveryHint(_))
                    {
                        recovery_used = true;
                        continue;
                    }
                    return Err(transport_error(error));
                }
            }
        }
    }

    async fn send_resumption_request(
        &self,
        endpoint: &str,
        auth: ResolvedAuth,
        body: Option<Bytes>,
    ) -> Result<InferenceResponse> {
        let headers = if body.is_some() {
            vec![LogicalHeader::new(
                header::CONTENT_TYPE.as_str().to_string(),
                "application/json".to_string(),
            )
            .map_err(transport_error)?]
        } else {
            Vec::new()
        };
        self.send_logical_request(
            http::Method::POST,
            endpoint.to_string(),
            headers,
            body,
            auth,
            true,
        )
        .await
    }

    async fn resolve_auth_after_preflight(
        &self,
        auth_mode: AuthHeaderMode,
    ) -> Result<ResolvedAuth> {
        let initial = self.resolve_auth(auth_mode)?;
        if matches!(
            initial.source,
            CredentialSource::StoredApiKey { .. } | CredentialSource::ExplicitApiKey
        ) || !self.auth_needs_preflight_refresh(&initial)?
        {
            return Ok(initial);
        }

        let _refresh_guard = self.refresh_lock.lock().await;
        let current = self.resolve_auth(auth_mode)?;
        if current == initial && self.auth_needs_preflight_refresh(&current)? {
            self.refresh_token_inner().await?;
        }
        self.resolve_auth(auth_mode)
    }

    fn auth_needs_preflight_refresh(&self, auth: &ResolvedAuth) -> Result<bool> {
        let Some(token) = auth.token.as_deref() else {
            return Ok(false);
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| Error::Authentication("System clock is before Unix epoch".to_string()))?
            .as_secs();
        Ok(access_token_needs_refresh(token, now))
    }

    /// Sends a lossless request to an allowed OpenSecret inference endpoint.
    ///
    /// The complete logical request, including its selected credential, is
    /// authenticated inside one Transport V2 envelope. No outer credential is
    /// emitted. One marked outer session-recovery error may re-establish the
    /// attested session and resend the captured request with a fresh request
    /// ID. No other failure triggers replay, and there is no V1 downgrade.
    pub async fn send_inference_request(
        &self,
        request: InferenceRequest,
    ) -> Result<InferenceResponse> {
        self.send_inference_request_with_budget(request, InferenceSendBudget::new(2)?)
            .await
    }

    /// Sends an inference request while sharing a caller-owned send budget.
    ///
    /// Only inference HTTP sends consume this request-local budget. Attestation
    /// and proactive token refresh do not consume it, and callers can share it
    /// across their own retry layers.
    pub async fn send_inference_request_with_budget(
        &self,
        request: InferenceRequest,
        send_budget: InferenceSendBudget,
    ) -> Result<InferenceResponse> {
        let auth = self
            .resolve_auth_after_preflight(AuthHeaderMode::ApiKeyOrJwt)
            .await?;
        self.send_inference_request_with_auth(request, auth, send_budget)
            .await
    }

    /// Sends one inference request with an explicit per-request API key.
    ///
    /// This is intended for multiplexing callers such as maple-proxy. The key
    /// is placed only inside the encrypted request and is not retained by the
    /// client or installed as its default credential.
    pub async fn send_inference_request_with_api_key(
        &self,
        request: InferenceRequest,
        api_key: String,
    ) -> Result<InferenceResponse> {
        let credential = ResolvedAuth {
            token: Some(api_key),
            source: CredentialSource::ExplicitApiKey,
        };
        self.send_inference_request_with_auth(request, credential, InferenceSendBudget::new(2)?)
            .await
    }

    async fn send_inference_request_with_auth(
        &self,
        request: InferenceRequest,
        auth: ResolvedAuth,
        send_budget: InferenceSendBudget,
    ) -> Result<InferenceResponse> {
        let (parts, body) = request.into_parts();
        if parts.uri.scheme().is_some() || parts.uri.authority().is_some() {
            return Err(Error::Configuration(
                "Inference request URI must be relative".to_string(),
            ));
        }
        if !is_allowed_inference_endpoint(&parts.method, parts.uri.path()) {
            return Err(Error::Configuration(format!(
                "Inference endpoint is not allowed: {} {}",
                parts.method,
                parts.uri.path()
            )));
        }
        let target = parts
            .uri
            .path_and_query()
            .ok_or_else(|| Error::Configuration("Inference request URI has no path".to_string()))?
            .as_str()
            .to_string();
        let headers = sanitize_inference_request_headers(&parts.headers)
            .iter()
            .map(|(name, value)| {
                let value = value.to_str().map_err(|_| {
                    Error::Configuration("Inference header value was not textual".to_string())
                })?;
                LogicalHeader::new(name.as_str().to_string(), value.to_string())
                    .map_err(transport_error)
            })
            .collect::<Result<Vec<_>>>()?;
        let body = if parts.method == http::Method::GET && body.is_empty() {
            None
        } else {
            Some(body)
        };
        let response = self
            .send_logical_request_with_options(
                parts.method,
                target,
                headers,
                body,
                auth,
                LogicalRequestOptions {
                    include_cache_namespace: true,
                    inference_send_budget: Some(&send_budget),
                },
            )
            .await?;
        let (mut parts, body) = response.into_parts();
        parts.headers = sanitize_inference_response_headers(&parts.headers);
        Ok(HttpResponse::from_parts(parts, body))
    }

    /// Typed compatibility wrapper over the lossless inference transport.
    async fn encrypted_openai_call<T: Serialize, U: DeserializeOwned>(
        &self,
        endpoint: &str,
        method: &str,
        data: Option<T>,
    ) -> Result<U> {
        let method = http::Method::from_bytes(method.as_bytes()).map_err(|error| {
            Error::Configuration(format!("Invalid inference HTTP method: {error}"))
        })?;
        let body = match data {
            Some(data) => Bytes::from(serde_json::to_vec(&data)?),
            None => Bytes::new(),
        };
        let mut builder = HttpRequest::builder().method(method).uri(endpoint);
        if !body.is_empty() {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
        }
        let request = builder.body(body).map_err(|error| {
            Error::Configuration(format!("Failed to build inference request: {error}"))
        })?;
        let response = self.send_inference_request(request).await?;
        let status = response.status();
        let body = collect_response_body(response.into_body()).await?;
        if !status.is_success() {
            return Err(Error::Api {
                status: status.as_u16(),
                message: String::from_utf8_lossy(&body).into_owned(),
            });
        }
        Ok(serde_json::from_slice(&body)?)
    }

    async fn encrypted_stream_call<T: Serialize>(
        &self,
        endpoint: &str,
        method: &str,
        data: Option<T>,
        auth_mode: AuthHeaderMode,
    ) -> Result<InferenceResponse> {
        let method = http::Method::from_bytes(method.as_bytes()).map_err(|error| {
            Error::Configuration(format!("Invalid streaming HTTP method: {error}"))
        })?;
        let body = data
            .map(|data| serde_json::to_vec(&data).map(Bytes::from))
            .transpose()?;
        let mut headers = vec![LogicalHeader::new(
            header::ACCEPT.as_str().to_string(),
            "text/event-stream".to_string(),
        )
        .map_err(transport_error)?];
        if body.is_some() {
            headers.push(
                LogicalHeader::new(
                    header::CONTENT_TYPE.as_str().to_string(),
                    "application/json".to_string(),
                )
                .map_err(transport_error)?,
            );
        }
        let auth = self.resolve_auth_after_preflight(auth_mode).await?;
        self.send_logical_request(method, endpoint.to_string(), headers, body, auth, false)
            .await
    }

    fn resolve_auth(&self, auth_mode: AuthHeaderMode) -> Result<ResolvedAuth> {
        let credentials = self.session_manager.get_credential_snapshot()?;
        match auth_mode {
            AuthHeaderMode::None => Ok(ResolvedAuth {
                token: None,
                source: CredentialSource::Anonymous,
            }),
            AuthHeaderMode::Jwt => Ok(ResolvedAuth {
                token: credentials
                    .tokens
                    .as_ref()
                    .map(|tokens| tokens.access_token.clone()),
                source: CredentialSource::StoredAccessToken {
                    generation: credentials.token_generation,
                },
            }),
            AuthHeaderMode::ApiKeyOrJwt => {
                if let Some(api_key) = credentials.api_key {
                    Ok(ResolvedAuth {
                        token: Some(api_key),
                        source: CredentialSource::StoredApiKey {
                            generation: credentials.api_key_generation,
                        },
                    })
                } else {
                    Ok(ResolvedAuth {
                        token: credentials.tokens.map(|tokens| tokens.access_token),
                        source: CredentialSource::StoredAccessToken {
                            generation: credentials.token_generation,
                        },
                    })
                }
            }
        }
    }

    // Auth Methods
    pub async fn login(
        &self,
        email: String,
        password: String,
        client_id: Uuid,
    ) -> Result<LoginResponse> {
        let credentials = LoginCredentials {
            email: Some(email),
            id: None,
            password,
            client_id,
        };

        let response: LoginResponse = self
            .encrypted_api_call("/login", "POST", Some(credentials))
            .await?;

        // Store the tokens
        self.session_manager.set_tokens(
            response.access_token.clone(),
            Some(response.refresh_token.clone()),
        )?;

        Ok(response)
    }

    pub async fn login_with_id(
        &self,
        id: Uuid,
        password: String,
        client_id: Uuid,
    ) -> Result<LoginResponse> {
        let credentials = LoginCredentials {
            email: None,
            id: Some(id),
            password,
            client_id,
        };

        let response: LoginResponse = self
            .encrypted_api_call("/login", "POST", Some(credentials))
            .await?;

        // Store the tokens
        self.session_manager.set_tokens(
            response.access_token.clone(),
            Some(response.refresh_token.clone()),
        )?;

        Ok(response)
    }

    pub async fn register(
        &self,
        email: String,
        password: String,
        client_id: Uuid,
        name: Option<String>,
    ) -> Result<LoginResponse> {
        let credentials = RegisterCredentials {
            email: Some(email),
            name,
            password,
            client_id,
        };

        let response: LoginResponse = self
            .encrypted_api_call("/register", "POST", Some(credentials))
            .await?;

        // Store the tokens
        self.session_manager.set_tokens(
            response.access_token.clone(),
            Some(response.refresh_token.clone()),
        )?;

        Ok(response)
    }

    pub async fn register_guest(&self, password: String, client_id: Uuid) -> Result<LoginResponse> {
        let credentials = RegisterCredentials {
            email: None,
            name: None,
            password,
            client_id,
        };

        let response: LoginResponse = self
            .encrypted_api_call("/register", "POST", Some(credentials))
            .await?;

        // Store the tokens
        self.session_manager.set_tokens(
            response.access_token.clone(),
            Some(response.refresh_token.clone()),
        )?;

        Ok(response)
    }

    // OAuth Methods

    pub async fn initiate_github_auth(
        &self,
        client_id: Uuid,
        invite_code: Option<String>,
    ) -> Result<GithubAuthResponse> {
        let request = OAuthInitRequest {
            client_id,
            invite_code,
        };
        self.encrypted_api_call("/auth/github", "POST", Some(request))
            .await
    }

    pub async fn handle_github_callback(
        &self,
        code: String,
        state: String,
        invite_code: String,
    ) -> Result<LoginResponse> {
        let request = OAuthCallbackRequest {
            code,
            state,
            invite_code,
        };

        let response: LoginResponse = self
            .encrypted_api_call("/auth/github/callback", "POST", Some(request))
            .await?;

        self.session_manager.set_tokens(
            response.access_token.clone(),
            Some(response.refresh_token.clone()),
        )?;

        Ok(response)
    }

    pub async fn initiate_google_auth(
        &self,
        client_id: Uuid,
        invite_code: Option<String>,
    ) -> Result<GoogleAuthResponse> {
        let request = OAuthInitRequest {
            client_id,
            invite_code,
        };
        self.encrypted_api_call("/auth/google", "POST", Some(request))
            .await
    }

    pub async fn handle_google_callback(
        &self,
        code: String,
        state: String,
        invite_code: String,
    ) -> Result<LoginResponse> {
        let request = OAuthCallbackRequest {
            code,
            state,
            invite_code,
        };

        let response: LoginResponse = self
            .encrypted_api_call("/auth/google/callback", "POST", Some(request))
            .await?;

        self.session_manager.set_tokens(
            response.access_token.clone(),
            Some(response.refresh_token.clone()),
        )?;

        Ok(response)
    }

    pub async fn initiate_apple_auth(
        &self,
        client_id: Uuid,
        invite_code: Option<String>,
    ) -> Result<AppleAuthResponse> {
        let request = OAuthInitRequest {
            client_id,
            invite_code,
        };
        self.encrypted_api_call("/auth/apple", "POST", Some(request))
            .await
    }

    pub async fn handle_apple_callback(
        &self,
        code: String,
        state: String,
        invite_code: String,
    ) -> Result<LoginResponse> {
        let request = OAuthCallbackRequest {
            code,
            state,
            invite_code,
        };

        let response: LoginResponse = self
            .encrypted_api_call("/auth/apple/callback", "POST", Some(request))
            .await?;

        self.session_manager.set_tokens(
            response.access_token.clone(),
            Some(response.refresh_token.clone()),
        )?;

        Ok(response)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn handle_apple_native_sign_in(
        &self,
        user_identifier: String,
        identity_token: String,
        client_id: Uuid,
        email: Option<String>,
        given_name: Option<String>,
        family_name: Option<String>,
        nonce: Option<String>,
        invite_code: Option<String>,
    ) -> Result<LoginResponse> {
        let request = AppleNativeSignInRequest {
            user_identifier,
            identity_token,
            client_id,
            email,
            given_name,
            family_name,
            nonce,
            invite_code,
        };

        let response: LoginResponse = self
            .encrypted_api_call("/auth/apple/native", "POST", Some(request))
            .await?;

        self.session_manager.set_tokens(
            response.access_token.clone(),
            Some(response.refresh_token.clone()),
        )?;

        Ok(response)
    }

    async fn refresh_token_inner(&self) -> Result<()> {
        let credentials = self.session_manager.get_credential_snapshot()?;
        let refresh_token = credentials
            .tokens
            .as_ref()
            .and_then(|tokens| tokens.refresh_token.clone())
            .ok_or_else(|| Error::Authentication("No refresh token available".to_string()))?;

        // Refresh is authorized by the resumption credential inside the
        // encrypted envelope. Its logical body is deliberately absent.
        let token_generation = credentials.token_generation;
        let auth = ResolvedAuth {
            token: Some(refresh_token),
            source: CredentialSource::StoredRefreshToken {
                generation: token_generation,
            },
        };
        let response = self.send_resumption_request("/refresh", auth, None).await?;
        let status = response.status();
        let body = collect_response_body(response.into_body()).await?;
        self.apply_authenticated_refresh_response(token_generation, status, &body)
    }

    fn apply_authenticated_refresh_response(
        &self,
        expected_token_generation: u64,
        status: http::StatusCode,
        body: &[u8],
    ) -> Result<()> {
        if !status.is_success() {
            if matches!(status.as_u16(), 401 | 403) {
                self.session_manager
                    .clear_tokens_if_generation(expected_token_generation)?;
            }
            return Err(Error::Api {
                status: status.as_u16(),
                message: String::from_utf8_lossy(body).into_owned(),
            });
        }
        let response: RefreshResponse = serde_json::from_slice(body)?;

        // A synchronous set_tokens or a logout/clear may have replaced these
        // credentials while the HTTP refresh was in flight. Drop this stale
        // response instead of reinstalling credentials the caller superseded.
        self.session_manager.set_tokens_if_generation(
            expected_token_generation,
            response.access_token,
            Some(response.refresh_token),
        )?;

        Ok(())
    }

    pub async fn refresh_token(&self) -> Result<()> {
        let _refresh_guard = self.refresh_lock.lock().await;
        self.refresh_token_inner().await
    }

    async fn logout_inner(&self, push_device_id: Option<Uuid>) -> Result<()> {
        // Serialize logout with refresh so an internal token rotation cannot
        // race the clear. Application-supplied credentials remain lock-free
        // and win through the generation check below.
        let _refresh_guard = self.refresh_lock.lock().await;
        let credentials = self.session_manager.get_credential_snapshot()?;
        let refresh_token = credentials
            .tokens
            .as_ref()
            .and_then(|tokens| tokens.refresh_token.clone())
            .ok_or_else(|| Error::Authentication("No refresh token available".to_string()))?;

        let request = LogoutRequest {
            refresh_token: refresh_token.clone(),
            push_device_id,
        };
        let body = Bytes::from(serde_json::to_vec(&request)?);
        let auth = ResolvedAuth {
            token: Some(refresh_token),
            source: CredentialSource::StoredRefreshToken {
                generation: credentials.token_generation,
            },
        };
        let response = self
            .send_resumption_request("/logout", auth, Some(body))
            .await?;
        let status = response.status();
        let body = collect_response_body(response.into_body()).await?;
        if !status.is_success() {
            return Err(Error::Api {
                status: status.as_u16(),
                message: String::from_utf8_lossy(&body).into_owned(),
            });
        }

        // Do not clear credentials installed by the application while the
        // logout request was in flight (for example, a rapid account switch).
        self.session_manager
            .clear_all_if_generation(credentials.generation)?;

        Ok(())
    }

    pub async fn logout(&self) -> Result<()> {
        self.logout_inner(None).await
    }

    pub async fn logout_with_push_device_id(&self, push_device_id: Uuid) -> Result<()> {
        self.logout_inner(Some(push_device_id)).await
    }

    pub fn get_access_token(&self) -> Result<Option<String>> {
        self.session_manager.get_access_token()
    }

    /// Return one coherent snapshot of the current JWT token pair.
    ///
    /// Prefer this over separate access- and refresh-token reads when the pair
    /// will be persisted or copied into another client: an automatic refresh
    /// can replace both values between two independent lock acquisitions.
    pub fn get_tokens(&self) -> Result<Option<TokenPair>> {
        self.session_manager.get_tokens()
    }

    pub fn get_refresh_token(&self) -> Result<Option<String>> {
        self.session_manager.get_refresh_token()
    }

    pub fn set_tokens(&self, access_token: String, refresh_token: Option<String>) -> Result<()> {
        self.session_manager.set_tokens(access_token, refresh_token)
    }

    // User Profile API
    pub async fn get_user(&self) -> Result<UserResponse> {
        self.authenticated_api_call("/protected/user", "GET", None::<()>)
            .await
    }

    pub async fn register_push_device(
        &self,
        request: RegisterPushDeviceRequest,
    ) -> Result<PushDevice> {
        self.authenticated_api_call("/v1/push/devices", "POST", Some(request))
            .await
    }

    pub async fn list_push_devices(&self) -> Result<PushDeviceListResponse> {
        self.authenticated_api_call("/v1/push/devices", "GET", None::<()>)
            .await
    }

    pub async fn revoke_push_device(&self, id: Uuid) -> Result<DeletedPushDeviceResponse> {
        self.authenticated_api_call(&format!("/v1/push/devices/{}", id), "DELETE", None::<()>)
            .await
    }

    // API Key Management
    pub async fn create_api_key(&self, name: String) -> Result<ApiKeyCreateResponse> {
        let request = ApiKeyCreateRequest { name };
        self.authenticated_api_call("/protected/api-keys", "POST", Some(request))
            .await
    }

    pub async fn list_api_keys(&self) -> Result<Vec<ApiKey>> {
        let response: ApiKeyListResponse = self
            .authenticated_api_call("/protected/api-keys", "GET", None::<()>)
            .await?;

        // Sort by created_at descending (newest first)
        let mut keys = response.keys;
        keys.sort_by_key(|key| std::cmp::Reverse(key.created_at));

        Ok(keys)
    }

    pub async fn delete_api_key(&self, name: &str) -> Result<()> {
        // URL-encode the name to handle special characters
        let encoded_name = utf8_percent_encode(name, NON_ALPHANUMERIC).to_string();
        let url = format!("/protected/api-keys/{}", encoded_name);
        let _: serde_json::Value = self
            .authenticated_api_call(&url, "DELETE", None::<()>)
            .await?;
        Ok(())
    }

    // Key-Value Storage APIs
    pub async fn kv_get(&self, key: &str) -> Result<String> {
        let encoded_key = utf8_percent_encode(key, NON_ALPHANUMERIC).to_string();
        let url = format!("/protected/kv/{}", encoded_key);
        self.authenticated_api_call(&url, "GET", None::<()>).await
    }

    pub async fn kv_put(&self, key: &str, value: String) -> Result<String> {
        let encoded_key = utf8_percent_encode(key, NON_ALPHANUMERIC).to_string();
        let url = format!("/protected/kv/{}", encoded_key);
        self.authenticated_api_call(&url, "PUT", Some(value)).await
    }

    pub async fn kv_delete(&self, key: &str) -> Result<()> {
        let encoded_key = utf8_percent_encode(key, NON_ALPHANUMERIC).to_string();
        let url = format!("/protected/kv/{}", encoded_key);
        let _: serde_json::Value = self
            .authenticated_api_call(&url, "DELETE", None::<()>)
            .await?;
        Ok(())
    }

    pub async fn kv_delete_all(&self) -> Result<()> {
        let _: serde_json::Value = self
            .authenticated_api_call("/protected/kv", "DELETE", None::<()>)
            .await?;
        Ok(())
    }

    pub async fn kv_list(&self) -> Result<Vec<KVListItem>> {
        self.authenticated_api_call("/protected/kv", "GET", None::<()>)
            .await
    }

    // Private Key APIs
    pub async fn get_private_key(&self, options: Option<KeyOptions>) -> Result<PrivateKeyResponse> {
        let mut url = "/protected/private_key".to_string();
        if let Some(opts) = &options {
            let mut params = Vec::new();
            if let Some(path) = &opts.seed_phrase_derivation_path {
                let encoded = utf8_percent_encode(path, NON_ALPHANUMERIC).to_string();
                params.push(format!("seed_phrase_derivation_path={}", encoded));
            }
            if let Some(path) = &opts.private_key_derivation_path {
                let encoded = utf8_percent_encode(path, NON_ALPHANUMERIC).to_string();
                params.push(format!("private_key_derivation_path={}", encoded));
            }
            if !params.is_empty() {
                url.push('?');
                url.push_str(&params.join("&"));
            }
        }
        self.authenticated_api_call(&url, "GET", None::<()>).await
    }

    pub async fn get_private_key_bytes(
        &self,
        options: Option<KeyOptions>,
    ) -> Result<PrivateKeyBytesResponse> {
        let mut url = "/protected/private_key_bytes".to_string();
        if let Some(opts) = &options {
            let mut params = Vec::new();
            if let Some(path) = &opts.seed_phrase_derivation_path {
                let encoded = utf8_percent_encode(path, NON_ALPHANUMERIC).to_string();
                params.push(format!("seed_phrase_derivation_path={}", encoded));
            }
            if let Some(path) = &opts.private_key_derivation_path {
                let encoded = utf8_percent_encode(path, NON_ALPHANUMERIC).to_string();
                params.push(format!("private_key_derivation_path={}", encoded));
            }
            if !params.is_empty() {
                url.push('?');
                url.push_str(&params.join("&"));
            }
        }
        self.authenticated_api_call(&url, "GET", None::<()>).await
    }

    // Message Signing API
    pub async fn sign_message(
        &self,
        message_bytes: &[u8],
        algorithm: SigningAlgorithm,
        key_options: Option<KeyOptions>,
    ) -> Result<SignMessageResponse> {
        let message_base64 = BASE64.encode(message_bytes);
        let request = SignMessageRequest {
            message_base64,
            algorithm,
            key_options: key_options.map(|opts| SigningKeyOptions {
                private_key_derivation_path: opts.private_key_derivation_path,
                seed_phrase_derivation_path: opts.seed_phrase_derivation_path,
            }),
        };
        self.authenticated_api_call("/protected/sign_message", "POST", Some(request))
            .await
    }

    // Public Key API
    pub async fn get_public_key(
        &self,
        algorithm: SigningAlgorithm,
        key_options: Option<KeyOptions>,
    ) -> Result<PublicKeyResponse> {
        let mut url = format!(
            "/protected/public_key?algorithm={}",
            match algorithm {
                SigningAlgorithm::Schnorr => "schnorr",
                SigningAlgorithm::Ecdsa => "ecdsa",
            }
        );
        if let Some(opts) = key_options {
            if let Some(path) = &opts.private_key_derivation_path {
                let encoded = utf8_percent_encode(path, NON_ALPHANUMERIC).to_string();
                url.push_str(&format!("&private_key_derivation_path={}", encoded));
            }
            if let Some(path) = &opts.seed_phrase_derivation_path {
                let encoded = utf8_percent_encode(path, NON_ALPHANUMERIC).to_string();
                url.push_str(&format!("&seed_phrase_derivation_path={}", encoded));
            }
        }
        self.authenticated_api_call(&url, "GET", None::<()>).await
    }

    // Third Party Token API
    pub async fn generate_third_party_token(
        &self,
        audience: Option<String>,
    ) -> Result<ThirdPartyTokenResponse> {
        let request = ThirdPartyTokenRequest { audience };
        self.authenticated_api_call("/protected/third_party_token", "POST", Some(request))
            .await
    }

    // Encryption/Decryption APIs
    pub async fn encrypt_data(
        &self,
        data: String,
        key_options: Option<KeyOptions>,
    ) -> Result<EncryptDataResponse> {
        let request = EncryptDataRequest {
            data,
            key_options: key_options.map(|opts| EncryptionKeyOptions {
                private_key_derivation_path: opts.private_key_derivation_path,
                seed_phrase_derivation_path: opts.seed_phrase_derivation_path,
            }),
        };
        self.authenticated_api_call("/protected/encrypt", "POST", Some(request))
            .await
    }

    pub async fn decrypt_data(
        &self,
        encrypted_data: String,
        key_options: Option<KeyOptions>,
    ) -> Result<String> {
        let request = DecryptDataRequest {
            encrypted_data,
            key_options: key_options.map(|opts| EncryptionKeyOptions {
                private_key_derivation_path: opts.private_key_derivation_path,
                seed_phrase_derivation_path: opts.seed_phrase_derivation_path,
            }),
        };
        self.authenticated_api_call("/protected/decrypt", "POST", Some(request))
            .await
    }

    // Account Management APIs

    /// Changes the password for the currently authenticated user
    pub async fn change_password(
        &self,
        current_password: String,
        new_password: String,
    ) -> Result<()> {
        let request = ChangePasswordRequest {
            current_password,
            new_password,
        };
        let response: CredentialUpdateResponse = self
            .authenticated_api_call("/protected/change_password", "POST", Some(request))
            .await?;
        if let Some(access_token) = response.access_token {
            let refresh_token = match response.refresh_token {
                Some(refresh_token) => Some(refresh_token),
                None => self.session_manager.get_refresh_token()?,
            };
            self.session_manager
                .set_tokens(access_token, refresh_token)?;
        }
        Ok(())
    }

    /// Requests a password reset for the given email
    /// Note: This does not require authentication but still uses encryption
    pub async fn request_password_reset(
        &self,
        email: String,
        hashed_secret: String,
        client_id: Uuid,
    ) -> Result<()> {
        let request = PasswordResetRequest {
            email,
            hashed_secret,
            client_id,
        };
        let _: serde_json::Value = self
            .encrypted_api_call("/password-reset/request", "POST", Some(request))
            .await?;
        Ok(())
    }

    /// Confirms a password reset with the code from email
    /// Note: This does not require authentication but still uses encryption
    pub async fn confirm_password_reset(
        &self,
        email: String,
        alphanumeric_code: String,
        plaintext_secret: String,
        new_password: String,
        client_id: Uuid,
    ) -> Result<()> {
        let request = PasswordResetConfirmRequest {
            email,
            alphanumeric_code,
            plaintext_secret,
            new_password,
            client_id,
        };
        let _: serde_json::Value = self
            .encrypted_api_call("/password-reset/confirm", "POST", Some(request))
            .await?;
        Ok(())
    }

    /// Verifies an email address with the code from the verification email
    /// Note: This does not require authentication but still uses encryption
    pub async fn verify_email(&self, code: String) -> Result<()> {
        let _: serde_json::Value = self
            .encrypted_api_call(&format!("/verify-email/{}", code), "GET", None::<()>)
            .await?;
        Ok(())
    }

    /// Requests a new email verification code
    pub async fn request_new_verification_code(&self) -> Result<()> {
        let _: serde_json::Value = self
            .authenticated_api_call("/protected/request_verification", "POST", None::<()>)
            .await?;
        Ok(())
    }

    /// Initiates the account deletion process
    pub async fn request_account_deletion(&self, hashed_secret: String) -> Result<()> {
        let request = InitiateAccountDeletionRequest { hashed_secret };
        let _: serde_json::Value = self
            .authenticated_api_call("/protected/delete-account/request", "POST", Some(request))
            .await?;
        Ok(())
    }

    /// Confirms account deletion with the code from email
    pub async fn confirm_account_deletion(
        &self,
        confirmation_code: String,
        plaintext_secret: String,
    ) -> Result<()> {
        let request = ConfirmAccountDeletionRequest {
            confirmation_code,
            plaintext_secret,
        };
        let _: serde_json::Value = self
            .authenticated_api_call("/protected/delete-account/confirm", "POST", Some(request))
            .await?;
        Ok(())
    }

    // AI/OpenAI API Methods

    /// Creates a new conversation.
    pub async fn create_conversation(
        &self,
        request: ConversationCreateRequest,
    ) -> Result<Conversation> {
        self.authenticated_api_call("/v1/conversations", "POST", Some(request))
            .await
    }

    /// Lists conversations with optional filters and pagination.
    pub async fn list_conversations(
        &self,
        params: Option<ConversationsListParams>,
    ) -> Result<ConversationsListResponse> {
        let endpoint = build_conversations_endpoint(params.as_ref());
        self.authenticated_api_call(&endpoint, "GET", None::<()>)
            .await
    }

    /// Fetches a single conversation by UUID.
    pub async fn get_conversation(&self, conversation_id: Uuid) -> Result<Conversation> {
        self.authenticated_api_call(
            &format!("/v1/conversations/{}", conversation_id),
            "GET",
            None::<()>,
        )
        .await
    }

    /// Partially updates a conversation.
    pub async fn update_conversation(
        &self,
        conversation_id: Uuid,
        request: ConversationUpdateRequest,
    ) -> Result<Conversation> {
        if request.is_empty() {
            return Err(Error::Configuration(
                "Conversation update request must include at least one field".to_string(),
            ));
        }

        self.authenticated_api_call(
            &format!("/v1/conversations/{}", conversation_id),
            "POST",
            Some(request),
        )
        .await
    }

    /// Deletes a single conversation by UUID.
    pub async fn delete_conversation(
        &self,
        conversation_id: Uuid,
    ) -> Result<DeletedObjectResponse> {
        self.authenticated_api_call(
            &format!("/v1/conversations/{}", conversation_id),
            "DELETE",
            None::<()>,
        )
        .await
    }

    /// Lists items in a conversation.
    pub async fn list_conversation_items(
        &self,
        conversation_id: Uuid,
        params: Option<AgentItemsListParams>,
    ) -> Result<ConversationItemsResponse> {
        let endpoint = build_agent_items_endpoint(
            &format!("/v1/conversations/{}/items", conversation_id),
            params.as_ref(),
        );
        self.authenticated_api_call(&endpoint, "GET", None::<()>)
            .await
    }

    /// Fetches a single item from a conversation.
    pub async fn get_conversation_item(
        &self,
        conversation_id: Uuid,
        item_id: Uuid,
    ) -> Result<ConversationItem> {
        self.authenticated_api_call(
            &format!("/v1/conversations/{}/items/{}", conversation_id, item_id),
            "GET",
            None::<()>,
        )
        .await
    }

    /// Deletes all conversations
    pub async fn delete_conversations(&self) -> Result<ConversationsDeleteResponse> {
        self.authenticated_api_call("/v1/conversations", "DELETE", None::<()>)
            .await
    }

    /// Batch deletes multiple conversations by their IDs
    pub async fn batch_delete_conversations(
        &self,
        ids: Vec<String>,
    ) -> Result<BatchDeleteConversationsResponse> {
        let request = BatchDeleteConversationsRequest { ids };
        self.authenticated_api_call("/v1/conversations/batch-delete", "POST", Some(request))
            .await
    }

    /// Batch updates conversation project assignments. Use `None` to clear the project.
    pub async fn batch_update_conversation_project(
        &self,
        ids: Vec<Uuid>,
        project_id: Option<Uuid>,
    ) -> Result<BatchUpdateConversationProjectResponse> {
        let request = BatchUpdateConversationProjectRequest { ids, project_id };
        self.authenticated_api_call(
            "/v1/conversations/batch-update-project",
            "POST",
            Some(request),
        )
        .await
    }

    /// Creates a new conversation project.
    pub async fn create_conversation_project(
        &self,
        request: ConversationProjectCreateRequest,
    ) -> Result<ConversationProject> {
        self.authenticated_api_call("/v1/conversation-projects", "POST", Some(request))
            .await
    }

    /// Lists conversation projects with pagination.
    pub async fn list_conversation_projects(
        &self,
        params: Option<ConversationProjectListParams>,
    ) -> Result<ConversationProjectsListResponse> {
        let endpoint = build_conversation_projects_endpoint(params.as_ref());
        self.authenticated_api_call(&endpoint, "GET", None::<()>)
            .await
    }

    /// Fetches a single conversation project by UUID.
    pub async fn get_conversation_project(&self, project_id: Uuid) -> Result<ConversationProject> {
        self.authenticated_api_call(
            &format!("/v1/conversation-projects/{}", project_id),
            "GET",
            None::<()>,
        )
        .await
    }

    /// Updates a conversation project and/or its project instructions.
    pub async fn update_conversation_project(
        &self,
        project_id: Uuid,
        request: ConversationProjectUpdateRequest,
    ) -> Result<ConversationProject> {
        if request.is_empty() {
            return Err(Error::Configuration(
                "Conversation project update request must include at least one field".to_string(),
            ));
        }

        self.authenticated_api_call(
            &format!("/v1/conversation-projects/{}", project_id),
            "POST",
            Some(request),
        )
        .await
    }

    /// Deletes a conversation project by UUID.
    pub async fn delete_conversation_project(
        &self,
        project_id: Uuid,
    ) -> Result<DeletedObjectResponse> {
        self.authenticated_api_call(
            &format!("/v1/conversation-projects/{}", project_id),
            "DELETE",
            None::<()>,
        )
        .await
    }

    /// Fetches available AI models
    pub async fn get_models(&self) -> Result<ModelsResponse> {
        self.encrypted_openai_call("/v1/models", "GET", None::<()>)
            .await
    }

    /// Fetches the full model catalog with context windows and capabilities.
    pub async fn get_model_catalog(&self) -> Result<ModelCatalogResponse> {
        self.encrypted_openai_call("/v1/models/catalog", "GET", None::<()>)
            .await
    }

    /// Creates embeddings for the given input text(s)
    ///
    /// # Example
    /// ```ignore
    /// let request = EmbeddingRequest {
    ///     input: "Hello, world!".into(),
    ///     model: "nomic-embed-text".to_string(),
    ///     encoding_format: None,
    ///     dimensions: None,
    ///     user: None,
    /// };
    /// let response = client.create_embeddings(request).await?;
    /// ```
    pub async fn create_embeddings(&self, request: EmbeddingRequest) -> Result<EmbeddingResponse> {
        self.encrypted_openai_call("/v1/embeddings", "POST", Some(request))
            .await
    }

    /// Creates a chat completion (non-streaming)
    pub async fn create_chat_completion(
        &self,
        mut request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse> {
        request.stream = Some(false);
        self.encrypted_openai_call("/v1/chat/completions", "POST", Some(request))
            .await
    }

    /// Creates a streaming chat completion
    pub async fn create_chat_completion_stream(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = Result<ChatCompletionChunk>> + Send>>>
    {
        use eventsource_stream::Eventsource;
        use futures::StreamExt;

        let request = build_chat_completion_stream_request(request)?;
        let response = self.send_inference_request(request).await?;
        let status = response.status();
        if !status.is_success() {
            let body = collect_response_body(response.into_body()).await?;
            return Err(Error::Api {
                status: status.as_u16(),
                message: String::from_utf8_lossy(&body).into_owned(),
            });
        }

        let stream = response
            .into_body()
            .map(|result| result.map_err(std::io::Error::other));

        let event_stream = stream.eventsource().filter_map(move |event| {
            async move {
                match event {
                    Ok(event) => {
                        // Check if this is the [DONE] event
                        if event.data == "[DONE]" {
                            return None;
                        }

                        match serde_json::from_str::<ChatCompletionChunk>(&event.data) {
                            Ok(chunk) => Some(Ok(chunk)),
                            Err(error) => Some(Err(Error::Api {
                                status: 0,
                                message: format!("Failed to parse chunk: {error}"),
                            })),
                        }
                    }
                    Err(error) => Some(Err(Error::Api {
                        status: 0,
                        message: format!("SSE error: {error}"),
                    })),
                }
            }
        });

        Ok(Box::pin(event_stream))
    }

    // Web API Methods

    /// Searches the public web through OpenSecret's configured search provider.
    pub async fn web_search(&self, request: WebSearchRequest) -> Result<WebSearchResponse> {
        self.authenticated_api_call("/v1/web/search", "POST", Some(request))
            .await
    }

    /// Extracts sanitized Markdown from public URLs through OpenSecret's configured provider.
    pub async fn web_extract(&self, request: WebExtractRequest) -> Result<WebExtractResponse> {
        self.authenticated_api_call("/v1/web/extract", "POST", Some(request))
            .await
    }

    async fn agent_chat_stream(
        &self,
        endpoint: String,
        input: &str,
    ) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = Result<AgentSseEvent>> + Send>>> {
        use eventsource_stream::Eventsource;
        use futures::StreamExt;

        let request = AgentChatRequest {
            input: input.to_string(),
        };

        let response = self
            .encrypted_stream_call(&endpoint, "POST", Some(request), AuthHeaderMode::Jwt)
            .await?;
        let status = response.status();
        if !status.is_success() {
            let body = collect_response_body(response.into_body()).await?;
            return Err(Error::Api {
                status: status.as_u16(),
                message: String::from_utf8_lossy(&body).into_owned(),
            });
        }

        let stream = response
            .into_body()
            .map(|result| result.map_err(std::io::Error::other));

        let event_stream = stream.eventsource().filter_map(|event| async move {
            let event = match event {
                Ok(event) => event,
                Err(error) => {
                    return Some(Err(Error::Api {
                        status: 0,
                        message: format!("SSE error: {error}"),
                    }));
                }
            };
            if event.data == "[DONE]" {
                return None;
            }

            parse_agent_sse_event(&event.event, &event.data)
        });

        Ok(Box::pin(event_stream))
    }

    // Agent API Methods

    /// Fetches the current user's main agent.
    pub async fn get_main_agent(&self) -> Result<MainAgentResponse> {
        self.authenticated_api_call("/v1/agent", "GET", None::<()>)
            .await
    }

    /// Explicitly initializes the current user's main agent.
    pub async fn init_main_agent(
        &self,
        request: InitMainAgentRequest,
    ) -> Result<InitMainAgentResponse> {
        self.authenticated_api_call("/v1/agent/init", "POST", Some(request))
            .await
    }

    /// Deletes the current user's main agent and resets shared agent state.
    pub async fn delete_main_agent(&self) -> Result<DeletedObjectResponse> {
        self.authenticated_api_call("/v1/agent", "DELETE", None::<()>)
            .await
    }

    /// Lists items in the main agent conversation.
    pub async fn list_main_agent_items(
        &self,
        params: Option<AgentItemsListParams>,
    ) -> Result<AgentItemsListResponse> {
        let endpoint = build_agent_items_endpoint("/v1/agent/items", params.as_ref());
        self.authenticated_api_call(&endpoint, "GET", None::<()>)
            .await
    }

    /// Fetches a single item from the main agent conversation.
    pub async fn get_main_agent_item(&self, item_id: Uuid) -> Result<ConversationItem> {
        self.authenticated_api_call(&format!("/v1/agent/items/{}", item_id), "GET", None::<()>)
            .await
    }

    /// Sets or replaces the current user's reaction on a main-agent assistant message item.
    pub async fn set_main_agent_item_reaction(
        &self,
        item_id: Uuid,
        emoji: impl Into<String>,
    ) -> Result<ConversationItem> {
        self.authenticated_api_call(
            &format!("/v1/agent/items/{}/reaction", item_id),
            "POST",
            Some(SetMessageReactionRequest {
                emoji: emoji.into(),
            }),
        )
        .await
    }

    /// Clears the current user's reaction on a main-agent assistant message item.
    pub async fn clear_main_agent_item_reaction(&self, item_id: Uuid) -> Result<ConversationItem> {
        self.authenticated_api_call(
            &format!("/v1/agent/items/{}/reaction", item_id),
            "DELETE",
            None::<()>,
        )
        .await
    }

    /// Sends a message to the main agent and returns a stream of SSE events.
    pub async fn agent_chat(
        &self,
        input: &str,
    ) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = Result<AgentSseEvent>> + Send>>> {
        self.agent_chat_stream("/v1/agent/chat".to_string(), input)
            .await
    }

    /// Creates a new subagent for the current user.
    pub async fn create_subagent(
        &self,
        request: CreateSubagentRequest,
    ) -> Result<SubagentResponse> {
        self.authenticated_api_call("/v1/agent/subagents", "POST", Some(request))
            .await
    }

    /// Lists subagents for the current user with pagination and filtering.
    pub async fn list_subagents(
        &self,
        params: Option<ListSubagentsParams>,
    ) -> Result<SubagentListResponse> {
        let endpoint = build_subagents_endpoint(params.as_ref());
        self.authenticated_api_call(&endpoint, "GET", None::<()>)
            .await
    }

    /// Fetches a single subagent by UUID.
    pub async fn get_subagent(&self, id: Uuid) -> Result<SubagentResponse> {
        self.authenticated_api_call(&format!("/v1/agent/subagents/{}", id), "GET", None::<()>)
            .await
    }

    /// Sends a message to a specific subagent and returns a stream of SSE events.
    pub async fn subagent_chat(
        &self,
        id: Uuid,
        input: &str,
    ) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = Result<AgentSseEvent>> + Send>>> {
        self.agent_chat_stream(format!("/v1/agent/subagents/{}/chat", id), input)
            .await
    }

    /// Lists items in a subagent conversation.
    pub async fn list_subagent_items(
        &self,
        id: Uuid,
        params: Option<AgentItemsListParams>,
    ) -> Result<AgentItemsListResponse> {
        let endpoint = build_agent_items_endpoint(
            &format!("/v1/agent/subagents/{}/items", id),
            params.as_ref(),
        );
        self.authenticated_api_call(&endpoint, "GET", None::<()>)
            .await
    }

    /// Fetches a single item from a subagent conversation.
    pub async fn get_subagent_item(&self, id: Uuid, item_id: Uuid) -> Result<ConversationItem> {
        self.authenticated_api_call(
            &format!("/v1/agent/subagents/{}/items/{}", id, item_id),
            "GET",
            None::<()>,
        )
        .await
    }

    /// Sets or replaces the current user's reaction on a subagent assistant message item.
    pub async fn set_subagent_item_reaction(
        &self,
        id: Uuid,
        item_id: Uuid,
        emoji: impl Into<String>,
    ) -> Result<ConversationItem> {
        self.authenticated_api_call(
            &format!("/v1/agent/subagents/{}/items/{}/reaction", id, item_id),
            "POST",
            Some(SetMessageReactionRequest {
                emoji: emoji.into(),
            }),
        )
        .await
    }

    /// Clears the current user's reaction on a subagent assistant message item.
    pub async fn clear_subagent_item_reaction(
        &self,
        id: Uuid,
        item_id: Uuid,
    ) -> Result<ConversationItem> {
        self.authenticated_api_call(
            &format!("/v1/agent/subagents/{}/items/{}/reaction", id, item_id),
            "DELETE",
            None::<()>,
        )
        .await
    }

    /// Deletes a subagent by UUID.
    pub async fn delete_subagent(&self, id: Uuid) -> Result<DeletedObjectResponse> {
        self.authenticated_api_call(&format!("/v1/agent/subagents/{}", id), "DELETE", None::<()>)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport_v2::client::{SessionResponder, TestV2ServerState};
    use futures::stream;
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    fn encoded_timing_token(exp: u64) -> String {
        let payload = URL_SAFE_NO_PAD.encode(serde_json::json!({ "exp": exp }).to_string());
        format!("header.{payload}.signature")
    }

    fn untrusted_handoff_grant(session_id: &str, request_id: &str) -> NativeOAuthHandoffGrant {
        let payload = URL_SAFE_NO_PAD
            .encode(serde_json::json!({ "sid": session_id, "rid": request_id }).to_string());
        NativeOAuthHandoffGrant::new(format!("e30.{payload}.c2ln")).unwrap()
    }

    fn response_events(
        events: Vec<std::result::Result<ResponseEvent, TransportV2Error>>,
    ) -> ResponseEventStream {
        Box::pin(stream::iter(events))
    }

    async fn mount_delayed_session_with_no_request(server: &MockServer, server_secret: [u8; 32]) {
        Mock::given(method("POST"))
            .and(path("/v2/session"))
            .respond_with(SessionResponder {
                server_secret,
                state: None,
                delay: Some(std::time::Duration::from_millis(500)),
            })
            .expect(1)
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/request"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(server)
            .await;
    }

    async fn wait_until_session_establishment_is_in_flight(server: &MockServer) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if server.received_requests().await.is_some_and(|requests| {
                    requests
                        .iter()
                        .any(|request| request.url.path() == "/v2/session")
                }) {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("session establishment request was not observed");
    }

    #[test]
    fn cache_namespace_root_round_trips_without_debug_disclosure() {
        let root = TransportV2CacheNamespaceRoot::from_bytes([42; 32]);
        let encoded = root.to_base64();

        assert_eq!(
            TransportV2CacheNamespaceRoot::from_base64(&encoded).unwrap(),
            root
        );
        assert_eq!(
            format!("{root:?}"),
            "TransportV2CacheNamespaceRoot([REDACTED])"
        );
        assert!(!format!("{root:?}").contains(&encoded));
    }

    #[test]
    fn cache_namespace_root_rejects_noncanonical_or_wrong_length_base64() {
        let canonical = BASE64.encode([42; 32]);

        assert!(
            TransportV2CacheNamespaceRoot::from_base64(canonical.trim_end_matches('=')).is_err()
        );
        assert!(TransportV2CacheNamespaceRoot::from_base64(&BASE64.encode([42; 31])).is_err());
    }

    #[test]
    fn native_handoff_grant_is_canonical_bounded_and_redacted() {
        let grant = NativeOAuthHandoffGrant::new("e30.e30.c2ln").unwrap();
        assert_eq!(format!("{grant:?}"), "NativeOAuthHandoffGrant([REDACTED])");
        for invalid in [
            "",
            "one.two",
            "one.two.three.four",
            "e30=.e30.c2ln",
            "e30.e30.bad+alphabet",
        ] {
            assert!(NativeOAuthHandoffGrant::new(invalid).is_err(), "{invalid}");
        }
        assert!(NativeOAuthHandoffGrant::new(format!(
            "{}.e30.c2ln",
            "a".repeat(MAX_NATIVE_OAUTH_HANDOFF_GRANT_BYTES)
        ))
        .is_err());
    }

    #[tokio::test]
    async fn prepared_handoff_filters_mismatched_untrusted_grant_targets_locally() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/session"))
            .respond_with(SessionResponder {
                server_secret: [0x89; 32],
                state: None,
                delay: None,
            })
            .expect(1)
            .mount(&server)
            .await;

        let client = OpenSecretClient::new(server.uri()).unwrap();
        let first = client.prepare_native_oauth_handoff().await.unwrap();
        let second = client.prepare_native_oauth_handoff().await.unwrap();
        let stale_first_grant = untrusted_handoff_grant(first.session_id(), first.request_id());

        assert!(first.matches_untrusted_grant_target(&stale_first_grant));
        assert!(
            !second.matches_untrusted_grant_target(&stale_first_grant),
            "an A callback must not consume a newer B request"
        );
        assert!(
            second.matches_untrusted_grant_target(&untrusted_handoff_grant(
                second.session_id(),
                second.request_id()
            ))
        );
        assert!(!second.matches_untrusted_grant_target(
            &NativeOAuthHandoffGrant::new("e30.e30.c2ln").unwrap()
        ));
    }

    #[tokio::test]
    async fn native_handoff_redeems_the_advertised_request_once_without_outer_secrets() {
        let server = MockServer::start().await;
        let state = TestV2ServerState::new();
        let user_id = Uuid::new_v4();
        state.queue_json_response(
            200,
            serde_json::json!({
                "id": user_id,
                "email": "native@example.com",
                "access_token": "native-access",
                "refresh_token": "native-refresh",
            }),
        );
        Mock::given(method("POST"))
            .and(path("/v2/session"))
            .respond_with(SessionResponder {
                server_secret: [0x8a; 32],
                state: Some(state.clone()),
                delay: None,
            })
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/request"))
            .respond_with(state.request_responder())
            .expect(1)
            .mount(&server)
            .await;

        let client = OpenSecretClient::new(server.uri()).unwrap();
        let prepared = client.prepare_native_oauth_handoff().await.unwrap();
        let advertised_session_id = prepared.session_id().to_string();
        let advertised_request_id = prepared.request_id().to_string();
        for id in [&advertised_session_id, &advertised_request_id] {
            assert_eq!(id.len(), 32);
            assert!(id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')));
        }

        let response = client
            .redeem_native_oauth_handoff(
                prepared,
                NativeOAuthHandoffGrant::new("e30.e30.c2ln").unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.id, user_id);
        let tokens = client.get_tokens().unwrap().unwrap();
        assert_eq!(tokens.access_token, "native-access");
        assert_eq!(tokens.refresh_token.as_deref(), Some("native-refresh"));

        let logical = state.captured_requests();
        assert_eq!(logical.len(), 1);
        assert_eq!(logical[0]["version"], 2);
        assert_eq!(logical[0]["credential"], serde_json::Value::Null);
        assert_eq!(logical[0]["cache_namespace_root"], serde_json::Value::Null);
        assert_eq!(logical[0]["method"], "POST");
        assert_eq!(logical[0]["target"], "/auth/native-handoff/redeem");
        assert_eq!(logical[0]["body_present"], true);
        assert_eq!(logical[0]["headers"].as_array().unwrap().len(), 1);
        assert_eq!(logical[0]["headers"][0]["name"], "content-type");
        assert_eq!(logical[0]["headers"][0]["value"], "application/json");
        assert_eq!(state.captured_request_ids(), vec![advertised_request_id]);
        let bodies = state.captured_request_bodies();
        assert_eq!(bodies.len(), 1);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bodies[0]).unwrap(),
            serde_json::json!({ "grant": "e30.e30.c2ln" })
        );

        let outer = server.received_requests().await.unwrap();
        assert_eq!(outer.len(), 2);
        assert_eq!(outer[1].url.path(), "/v2/request");
        assert_eq!(
            outer[1].headers.get("x-session-id").unwrap(),
            advertised_session_id.as_str()
        );
        assert!(!outer[1].headers.contains_key(header::AUTHORIZATION));
        assert!(!outer[1].headers.contains_key(header::COOKIE));
        for secret in ["e30.e30.c2ln", "native-access", "native-refresh"] {
            assert!(!outer[1]
                .body
                .windows(secret.len())
                .any(|window| window == secret.as_bytes()));
        }
    }

    #[tokio::test]
    async fn native_handoff_rejects_stale_anonymous_generation_before_network_use() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/session"))
            .respond_with(SessionResponder {
                server_secret: [0x8b; 32],
                state: None,
                delay: None,
            })
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/request"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        let client = OpenSecretClient::new(server.uri()).unwrap();
        let prepared = client.prepare_native_oauth_handoff().await.unwrap();
        client
            .set_tokens("new-access".to_string(), Some("new-refresh".to_string()))
            .unwrap();
        assert!(matches!(
            client
                .redeem_native_oauth_handoff(
                    prepared,
                    NativeOAuthHandoffGrant::new("e30.e30.c2ln").unwrap(),
                )
                .await,
            Err(Error::Authentication(message))
                if message == "Authentication changed during native OAuth handoff"
        ));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn native_handoff_rejects_a_replaced_transport_session_before_network_use() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/session"))
            .respond_with(SessionResponder {
                server_secret: [0x8c; 32],
                state: None,
                delay: None,
            })
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/request"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        let client = OpenSecretClient::new(server.uri()).unwrap();
        let prepared = client.prepare_native_oauth_handoff().await.unwrap();
        client.perform_attestation_handshake().await.unwrap();
        assert!(matches!(
            client
                .redeem_native_oauth_handoff(
                    prepared,
                    NativeOAuthHandoffGrant::new("e30.e30.c2ln").unwrap(),
                )
                .await,
            Err(Error::Session(message))
                if message == "Native OAuth transport session is no longer current"
        ));
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn native_handoff_response_cannot_overwrite_credentials_changed_in_flight() {
        let server = MockServer::start().await;
        let state = TestV2ServerState::new();
        state.queue_delayed_json_response(
            200,
            serde_json::json!({
                "id": Uuid::new_v4(),
                "email": "stale@example.com",
                "access_token": "stale-access",
                "refresh_token": "stale-refresh",
            }),
            std::time::Duration::from_millis(500),
        );
        Mock::given(method("POST"))
            .and(path("/v2/session"))
            .respond_with(SessionResponder {
                server_secret: [0x8d; 32],
                state: Some(state.clone()),
                delay: None,
            })
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/request"))
            .respond_with(state.request_responder())
            .expect(1)
            .mount(&server)
            .await;

        let client = Arc::new(OpenSecretClient::new(server.uri()).unwrap());
        let prepared = client.prepare_native_oauth_handoff().await.unwrap();
        let pending = {
            let client = Arc::clone(&client);
            tokio::spawn(async move {
                client
                    .redeem_native_oauth_handoff(
                        prepared,
                        NativeOAuthHandoffGrant::new("e30.e30.c2ln").unwrap(),
                    )
                    .await
            })
        };
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if state.captured_requests().len() == 1 {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("native handoff request was not observed");
        client
            .set_tokens("new-access".to_string(), Some("new-refresh".to_string()))
            .unwrap();

        assert!(matches!(
            pending.await.unwrap(),
            Err(Error::Authentication(message))
                if message == "Authentication changed during native OAuth handoff"
        ));
        let tokens = client.get_tokens().unwrap().unwrap();
        assert_eq!(tokens.access_token, "new-access");
        assert_eq!(tokens.refresh_token.as_deref(), Some("new-refresh"));
        assert!(client.current_transport_session().unwrap().is_none());
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn failed_native_handoff_is_not_retried_and_retires_ambiguous_session() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/session"))
            .respond_with(SessionResponder {
                server_secret: [0x8e; 32],
                state: None,
                delay: None,
            })
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/request"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;

        let client = OpenSecretClient::new(server.uri()).unwrap();
        let prepared = client.prepare_native_oauth_handoff().await.unwrap();
        assert!(client
            .redeem_native_oauth_handoff(
                prepared,
                NativeOAuthHandoffGrant::new("e30.e30.c2ln").unwrap(),
            )
            .await
            .is_err());
        assert!(client.current_transport_session().unwrap().is_none());
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].url.path(), "/v2/session");
        assert_eq!(requests[1].url.path(), "/v2/request");
    }

    #[tokio::test]
    async fn native_handoff_requires_a_dedicated_anonymous_client() {
        let client =
            OpenSecretClient::new_with_api_key("http://127.0.0.1:1", "api-key".to_string())
                .unwrap();
        assert!(matches!(
            client.prepare_native_oauth_handoff().await,
            Err(Error::Authentication(message))
                if message == "Native OAuth handoff requires an anonymous client"
        ));
    }

    #[derive(Serialize)]
    struct NativeHandoffGrantIntegrationRequest<'a> {
        native_session_id: &'a str,
        native_request_id: &'a str,
    }

    #[derive(Deserialize)]
    struct NativeHandoffGrantIntegrationResponse {
        grant: String,
    }

    #[tokio::test]
    #[ignore = "Requires the disposable OpenSecret SDK integration backend"]
    async fn native_handoff_full_stack_round_trip() -> Result<()> {
        let base_url = std::env::var("VITE_OPEN_SECRET_API_URL")
            .expect("VITE_OPEN_SECRET_API_URL must identify the disposable backend");
        let client_id = std::env::var("VITE_TEST_CLIENT_ID")
            .ok()
            .and_then(|value| Uuid::parse_str(&value).ok())
            .expect("VITE_TEST_CLIENT_ID must be a UUID");

        let browser = OpenSecretClient::new(base_url.clone())?;
        let registered = browser
            .register_guest(
                format!("native_handoff_full_stack_{}", Uuid::new_v4()),
                client_id,
            )
            .await?;

        let native_a = OpenSecretClient::new(base_url.clone())?;
        let native_b = OpenSecretClient::new(base_url)?;
        let prepared_a = native_a.prepare_native_oauth_handoff().await?;
        let prepared_b = native_b.prepare_native_oauth_handoff().await?;

        let minted: NativeHandoffGrantIntegrationResponse = browser
            .authenticated_api_call(
                "/auth/native-handoff/grant",
                "POST",
                Some(NativeHandoffGrantIntegrationRequest {
                    native_session_id: prepared_a.session_id(),
                    native_request_id: prepared_a.request_id(),
                }),
            )
            .await?;

        let transplanted = native_b
            .redeem_native_oauth_handoff(
                prepared_b,
                NativeOAuthHandoffGrant::new(minted.grant.clone())?,
            )
            .await;
        assert!(
            matches!(transplanted, Err(Error::Api { status: 401, .. })),
            "a grant must not authenticate a different native session/request: {transplanted:?}"
        );
        assert!(native_b.get_tokens()?.is_none());

        let redeemed = native_a
            .redeem_native_oauth_handoff(prepared_a, NativeOAuthHandoffGrant::new(minted.grant)?)
            .await?;
        assert_eq!(redeemed.id, registered.id);
        let native_user = native_a.get_user().await?;
        assert_eq!(native_user.user.id, registered.id);
        assert!(native_a.get_tokens()?.is_some());

        Ok(())
    }

    #[test]
    fn access_token_expiry_is_only_a_conservative_timing_hint() {
        assert!(access_token_needs_refresh(&encoded_timing_token(130), 100));
        assert!(access_token_needs_refresh(&encoded_timing_token(129), 100));
        assert!(!access_token_needs_refresh(&encoded_timing_token(131), 100));
        assert!(!access_token_needs_refresh("not-a-jwt", 100));
        assert!(!access_token_needs_refresh("a.invalid-json.c", 100));
    }

    #[test]
    fn typed_stream_request_keeps_sse_and_json_headers_and_v2_stream_flags() {
        let request = ChatCompletionRequest {
            model: "test-model".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: serde_json::json!("hello"),
                tool_calls: None,
                reasoning_content: None,
            }],
            temperature: None,
            max_tokens: None,
            stream: Some(false),
            stream_options: None,
            tools: None,
            tool_choice: None,
        };
        let encoded = build_chat_completion_stream_request(request).unwrap();
        assert_eq!(encoded.method(), http::Method::POST);
        assert_eq!(encoded.uri(), "/v1/chat/completions");
        assert_eq!(encoded.headers()[header::ACCEPT], "text/event-stream");
        assert_eq!(encoded.headers()[header::CONTENT_TYPE], "application/json");
        let body: serde_json::Value = serde_json::from_slice(encoded.body()).unwrap();
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[tokio::test]
    async fn transport_network_failures_preserve_the_public_http_error_class() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let client = OpenSecretClient::new(format!("http://{address}")).unwrap();

        assert!(matches!(
            client.perform_attestation_handshake().await,
            Err(Error::Http(_))
        ));
    }

    #[test]
    fn transient_or_malformed_refresh_response_preserves_credentials() {
        let client = OpenSecretClient::new("http://127.0.0.1:1").unwrap();
        client
            .set_tokens("access".to_string(), Some("refresh".to_string()))
            .unwrap();
        let generation = client
            .session_manager
            .get_credential_snapshot()
            .unwrap()
            .token_generation;

        assert!(client
            .apply_authenticated_refresh_response(
                generation,
                http::StatusCode::SERVICE_UNAVAILABLE,
                b"temporarily unavailable",
            )
            .is_err());
        assert_eq!(
            client.get_access_token().unwrap().as_deref(),
            Some("access")
        );
        assert!(client
            .apply_authenticated_refresh_response(generation, http::StatusCode::OK, b"not-json",)
            .is_err());
        assert_eq!(
            client.get_access_token().unwrap().as_deref(),
            Some("access")
        );
    }

    #[test]
    fn authenticated_refresh_rejection_clears_only_the_rejected_generation() {
        let client = OpenSecretClient::new("http://127.0.0.1:1").unwrap();
        client
            .set_tokens("old-access".to_string(), Some("old-refresh".to_string()))
            .unwrap();
        let old_generation = client
            .session_manager
            .get_credential_snapshot()
            .unwrap()
            .token_generation;
        client
            .set_tokens("new-access".to_string(), Some("new-refresh".to_string()))
            .unwrap();

        assert!(client
            .apply_authenticated_refresh_response(
                old_generation,
                http::StatusCode::UNAUTHORIZED,
                b"rejected",
            )
            .is_err());
        assert_eq!(
            client.get_access_token().unwrap().as_deref(),
            Some("new-access")
        );

        let current_generation = client
            .session_manager
            .get_credential_snapshot()
            .unwrap()
            .token_generation;
        assert!(client
            .apply_authenticated_refresh_response(
                current_generation,
                http::StatusCode::FORBIDDEN,
                b"rejected",
            )
            .is_err());
        assert!(client.get_tokens().unwrap().is_none());
    }

    #[test]
    fn successful_refresh_updates_only_the_generation_that_was_sent() {
        let client = OpenSecretClient::new("http://127.0.0.1:1").unwrap();
        client
            .set_tokens("old-access".to_string(), Some("old-refresh".to_string()))
            .unwrap();
        let generation = client
            .session_manager
            .get_credential_snapshot()
            .unwrap()
            .token_generation;
        let body = br#"{"access_token":"rotated-access","refresh_token":"rotated-refresh"}"#;

        client
            .apply_authenticated_refresh_response(generation, http::StatusCode::OK, body)
            .unwrap();
        let tokens = client.get_tokens().unwrap().unwrap();
        assert_eq!(tokens.access_token, "rotated-access");
        assert_eq!(tokens.refresh_token.as_deref(), Some("rotated-refresh"));
    }

    #[test]
    fn inference_allowlist_is_method_and_path_exact() {
        assert!(is_allowed_inference_endpoint(
            &http::Method::POST,
            "/v1/chat/completions"
        ));
        assert!(is_allowed_inference_endpoint(
            &http::Method::GET,
            "/v1/models"
        ));
        assert!(!is_allowed_inference_endpoint(
            &http::Method::POST,
            "/v1/models"
        ));
        assert!(!is_allowed_inference_endpoint(
            &http::Method::GET,
            "/protected/private_key"
        ));
        assert!(!is_allowed_inference_endpoint(
            &http::Method::POST,
            "/v1/chat/completions/extra"
        ));
    }

    #[tokio::test]
    async fn inference_budget_excludes_preflight_and_blocks_a_later_send() {
        let server = MockServer::start().await;
        let state = TestV2ServerState::new();
        state.queue_json_response(
            200,
            serde_json::json!({
                "access_token": "fresh-access",
                "refresh_token": "fresh-refresh",
            }),
        );
        state.queue_json_response(200, serde_json::json!({ "object": "list", "data": [] }));
        Mock::given(method("POST"))
            .and(path("/v2/session"))
            .respond_with(SessionResponder {
                server_secret: [0x75; 32],
                state: Some(state.clone()),
                delay: None,
            })
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/request"))
            .respond_with(state.request_responder())
            .expect(2)
            .mount(&server)
            .await;
        let client = OpenSecretClient::new(server.uri()).unwrap();
        client
            .set_tokens(encoded_timing_token(0), Some("refresh-secret".to_string()))
            .unwrap();
        let send_budget = InferenceSendBudget::new(1).unwrap();
        let request = HttpRequest::get("/v1/models").body(Bytes::new()).unwrap();
        let response = client
            .send_inference_request_with_budget(request, send_budget.clone())
            .await
            .unwrap();
        assert_eq!(response.status(), http::StatusCode::OK);
        collect_response_body(response.into_body()).await.unwrap();
        assert_eq!(send_budget.remaining(), 0);

        let request = HttpRequest::get("/v1/models").body(Bytes::new()).unwrap();
        assert!(matches!(
            client
                .send_inference_request_with_budget(request, send_budget)
                .await,
            Err(Error::Other(message)) if message == "Inference request send budget exhausted"
        ));
        let logical = state.captured_requests();
        assert_eq!(logical.len(), 2);
        assert_eq!(logical[0]["target"], "/refresh");
        assert_eq!(logical[1]["target"], "/v1/models");
        assert_eq!(logical[1]["credential"]["value"], "fresh-access");
        server.verify().await;
    }

    #[tokio::test]
    async fn inference_transport_does_not_follow_redirects_outside_the_send_budget() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/session"))
            .respond_with(SessionResponder {
                server_secret: [0x76; 32],
                state: None,
                delay: None,
            })
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/request"))
            .respond_with(ResponseTemplate::new(307).insert_header("location", "/redirected"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/redirected"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let client = OpenSecretClient::new_with_api_key(server.uri(), "api-key".to_string()).unwrap();
        let send_budget = InferenceSendBudget::new(2).unwrap();
        let request = HttpRequest::get("/v1/models").body(Bytes::new()).unwrap();
        assert!(matches!(
            client
                .send_inference_request_with_budget(request, send_budget.clone())
                .await,
            Err(Error::InvalidResponse(_))
        ));
        assert_eq!(send_budget.remaining(), 1);
        server.verify().await;
    }

    #[tokio::test]
    async fn explicit_inference_api_key_is_never_installed_as_client_state() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/session"))
            .respond_with(SessionResponder {
                server_secret: [0x77; 32],
                state: None,
                delay: None,
            })
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/request"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;
        let client = OpenSecretClient::new(server.uri()).unwrap();
        let request = HttpRequest::get("/v1/models").body(Bytes::new()).unwrap();

        assert!(client
            .send_inference_request_with_api_key(request, "per-request-secret".to_string())
            .await
            .is_err());
        assert!(client.session_manager.get_api_key().unwrap().is_none());
        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 2);
        assert_eq!(received[0].url.path(), "/v2/session");
        assert_eq!(received[1].url.path(), "/v2/request");
        assert!(!received[1].headers.contains_key(header::AUTHORIZATION));
        assert!(!received[1].headers.contains_key(header::COOKIE));
        assert!(received[1].headers.contains_key("x-session-id"));
        assert!(!received[1]
            .body
            .windows(b"per-request-secret".len())
            .any(|window| window == b"per-request-secret"));
    }

    #[tokio::test]
    async fn superseded_bearer_during_session_establishment_is_never_sent() {
        let server = MockServer::start().await;
        mount_delayed_session_with_no_request(&server, [0x81; 32]).await;
        let client = Arc::new(OpenSecretClient::new(server.uri()).unwrap());
        client
            .set_tokens("old-access".to_string(), Some("old-refresh".to_string()))
            .unwrap();

        let pending = {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.get_user().await })
        };
        wait_until_session_establishment_is_in_flight(&server).await;
        client
            .set_tokens("new-access".to_string(), Some("new-refresh".to_string()))
            .unwrap();

        assert!(matches!(
            pending.await.unwrap(),
            Err(Error::Authentication(message))
                if message == "Credential changed before Transport V2 request admission"
        ));
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.path(), "/v2/session");
    }

    #[tokio::test]
    async fn superseded_stored_api_key_during_session_establishment_is_never_sent() {
        let server = MockServer::start().await;
        mount_delayed_session_with_no_request(&server, [0x82; 32]).await;
        let client = Arc::new(
            OpenSecretClient::new_with_api_key(server.uri(), "old-api-key".to_string()).unwrap(),
        );

        let pending = {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.get_models().await })
        };
        wait_until_session_establishment_is_in_flight(&server).await;
        client.set_api_key("new-api-key".to_string()).unwrap();

        assert!(matches!(
            pending.await.unwrap(),
            Err(Error::Authentication(message))
                if message == "Credential changed before Transport V2 request admission"
        ));
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.path(), "/v2/session");
    }

    #[tokio::test]
    async fn cleared_resumption_during_session_establishment_is_never_sent() {
        let server = MockServer::start().await;
        mount_delayed_session_with_no_request(&server, [0x83; 32]).await;
        let client = Arc::new(OpenSecretClient::new(server.uri()).unwrap());
        client
            .set_tokens("old-access".to_string(), Some("old-refresh".to_string()))
            .unwrap();

        let pending = {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.refresh_token().await })
        };
        wait_until_session_establishment_is_in_flight(&server).await;
        client.session_manager.clear_tokens().unwrap();

        assert!(matches!(
            pending.await.unwrap(),
            Err(Error::Authentication(message))
                if message == "Credential changed before Transport V2 request admission"
        ));
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.path(), "/v2/session");
    }

    #[tokio::test]
    async fn concurrent_preflight_refreshes_once_before_sending_each_original_request_once() {
        let server = MockServer::start().await;
        let state = TestV2ServerState::new();
        state.queue_json_response(
            200,
            serde_json::json!({
                "access_token": "fresh-access",
                "refresh_token": "fresh-refresh",
            }),
        );
        for _ in 0..2 {
            state.queue_json_response(
                200,
                serde_json::json!({
                    "object": "list",
                    "data": [],
                }),
            );
        }
        Mock::given(method("POST"))
            .and(path("/v2/session"))
            .respond_with(SessionResponder {
                server_secret: [0x78; 32],
                state: Some(state.clone()),
                delay: None,
            })
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/request"))
            .respond_with(state.request_responder())
            .expect(3)
            .mount(&server)
            .await;

        let client = OpenSecretClient::new(server.uri()).unwrap();
        client
            .set_tokens(encoded_timing_token(0), Some("refresh-secret".to_string()))
            .unwrap();
        let (first, second) = tokio::join!(client.get_models(), client.get_models());
        assert!(first.is_ok());
        assert!(second.is_ok());

        let logical = state.captured_requests();
        assert_eq!(logical.len(), 3);
        assert_eq!(logical[0]["target"], "/refresh");
        assert_eq!(logical[0]["credential"]["kind"], "resumption");
        assert_eq!(logical[0]["credential"]["value"], "refresh-secret");
        for request in &logical[1..] {
            assert_eq!(request["target"], "/v1/models");
            assert_eq!(request["credential"]["kind"], "bearer");
            assert_eq!(request["credential"]["value"], "fresh-access");
        }
        let outer = server.received_requests().await.unwrap();
        assert_eq!(outer.len(), 4);
        assert!(outer
            .iter()
            .all(|request| !request.headers.contains_key(header::AUTHORIZATION)));
        assert_eq!(
            client.get_access_token().unwrap().as_deref(),
            Some("fresh-access")
        );
    }

    #[tokio::test]
    async fn authenticated_post_send_failure_is_not_retried_or_repaired() {
        let server = MockServer::start().await;
        let state = TestV2ServerState::new();
        state.queue_json_response(401, serde_json::json!({ "error": "rejected" }));
        Mock::given(method("POST"))
            .and(path("/v2/session"))
            .respond_with(SessionResponder {
                server_secret: [0x79; 32],
                state: Some(state.clone()),
                delay: None,
            })
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/request"))
            .respond_with(state.request_responder())
            .expect(1)
            .mount(&server)
            .await;

        let client =
            OpenSecretClient::new_with_api_key(server.uri(), "api-key".to_string()).unwrap();
        assert!(matches!(
            client.get_models().await,
            Err(Error::Api { status: 401, .. })
        ));
        assert_eq!(state.captured_requests().len(), 1);
        let outer = server.received_requests().await.unwrap();
        assert_eq!(outer.len(), 2);
        assert_eq!(outer[0].url.path(), "/v2/session");
        assert_eq!(outer[1].url.path(), "/v2/request");
    }

    #[test]
    fn inference_request_headers_preserve_logical_content_type_but_strip_outer_authority() {
        let mut headers = HttpHeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            "Bearer outer-secret".parse().unwrap(),
        );
        headers.insert(
            header::CONTENT_TYPE,
            "multipart/form-data; boundary=test".parse().unwrap(),
        );
        headers.insert(header::CONTENT_LENGTH, "123".parse().unwrap());
        headers.insert(header::CONTENT_ENCODING, "gzip".parse().unwrap());
        headers.insert(header::COOKIE, "session=outer-secret".parse().unwrap());
        headers.insert("forwarded", "for=192.0.2.1".parse().unwrap());
        headers.insert("via", "1.1 outer-proxy".parse().unwrap());
        headers.insert("x-forwarded-for", "192.0.2.1".parse().unwrap());
        headers.insert("x-forwarded-host", "outer.example".parse().unwrap());
        headers.insert("x-forwarded-proto", "https".parse().unwrap());
        headers.insert("x-session-id", "outer-session".parse().unwrap());
        headers.insert(ROUTING_KEY_HEADER, "outer-routing-key".parse().unwrap());
        headers.insert("x-client-metadata", "kept".parse().unwrap());
        headers.append("x-client-metadata", "also-kept".parse().unwrap());

        let sanitized = sanitize_inference_request_headers(&headers);

        assert_eq!(
            sanitized.get(header::CONTENT_TYPE).unwrap(),
            "multipart/form-data; boundary=test"
        );
        assert_eq!(
            sanitized
                .get_all("x-client-metadata")
                .iter()
                .map(|value| value.to_str().unwrap())
                .collect::<Vec<_>>(),
            ["kept", "also-kept"]
        );
        assert!(!sanitized.contains_key(header::AUTHORIZATION));
        assert!(!sanitized.contains_key(header::CONTENT_LENGTH));
        assert!(!sanitized.contains_key(header::CONTENT_ENCODING));
        assert!(!sanitized.contains_key(header::COOKIE));
        assert!(!sanitized.contains_key("forwarded"));
        assert!(!sanitized.contains_key("via"));
        assert!(!sanitized.contains_key("x-forwarded-for"));
        assert!(!sanitized.contains_key("x-forwarded-host"));
        assert!(!sanitized.contains_key("x-forwarded-proto"));
        assert!(!sanitized.contains_key("x-session-id"));
        assert!(!sanitized.contains_key(ROUTING_KEY_HEADER));
    }

    #[tokio::test]
    async fn response_events_reconstruct_status_headers_and_incremental_body() {
        let events = response_events(vec![
            Ok(ResponseEvent::Start {
                status: 206,
                headers: vec![LogicalHeader::new(
                    "content-type".to_string(),
                    "audio/mpeg".to_string(),
                )
                .unwrap()],
            }),
            Ok(ResponseEvent::Chunk(Bytes::from_static(b"abc"))),
            Ok(ResponseEvent::Chunk(Bytes::from_static(b"def"))),
            Ok(ResponseEvent::End),
        ]);

        let response = response_from_events(events, None).await.unwrap();
        assert_eq!(response.status(), http::StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "audio/mpeg"
        );
        assert_eq!(
            collect_response_body(response.into_body()).await.unwrap(),
            Bytes::from_static(b"abcdef")
        );
    }

    #[tokio::test]
    async fn response_error_record_is_not_mistaken_for_clean_eof() {
        let events = response_events(vec![
            Ok(ResponseEvent::Start {
                status: 200,
                headers: Vec::new(),
            }),
            Ok(ResponseEvent::Chunk(Bytes::from_static(b"partial"))),
            Ok(ResponseEvent::Error {
                code: "provider_failed".to_string(),
            }),
        ]);

        let response = response_from_events(events, None).await.unwrap();
        let error = collect_response_body(response.into_body())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("provider_failed"));
    }

    #[tokio::test]
    async fn response_without_authenticated_terminal_is_rejected() {
        let events = response_events(vec![
            Ok(ResponseEvent::Start {
                status: 200,
                headers: Vec::new(),
            }),
            Ok(ResponseEvent::Chunk(Bytes::from_static(b"partial"))),
        ]);

        let response = response_from_events(events, None).await.unwrap();
        let error = collect_response_body(response.into_body())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("authenticated finality"));
    }

    #[test]
    fn endpoint_builders_encode_query_values() {
        let params = ConversationsListParams {
            limit: Some(25),
            after: Some(Uuid::nil()),
            order: Some("desc".to_string()),
            project_id: None,
            unassigned_project: Some(true),
            pinned: Some(false),
        };
        let endpoint = build_conversations_endpoint(Some(&params));

        assert!(endpoint.starts_with("/v1/conversations?"));
        assert!(endpoint.contains("limit=25"));
        assert!(endpoint.contains("after=00000000%2D0000%2D0000%2D0000%2D000000000000"));
        assert!(endpoint.contains("order=desc"));
        assert!(endpoint.contains("unassigned_project=true"));
        assert!(endpoint.contains("pinned=false"));
    }

    #[tokio::test]
    async fn empty_conversation_updates_still_fail_before_network_use() {
        let client = OpenSecretClient::new("http://127.0.0.1:1").unwrap();

        assert!(matches!(
            client
                .update_conversation(Uuid::nil(), ConversationUpdateRequest::default())
                .await,
            Err(Error::Configuration(message))
                if message.contains("at least one field")
        ));
        assert!(matches!(
            client
                .update_conversation_project(
                    Uuid::nil(),
                    ConversationProjectUpdateRequest::default(),
                )
                .await,
            Err(Error::Configuration(message))
                if message.contains("at least one field")
        ));
    }

    #[test]
    fn agent_sse_parser_preserves_typed_message_and_reaction_ids() {
        let message_id = Uuid::new_v4();
        let message = format!(r#"{{"message_id":"{message_id}","message":"hello"}}"#);
        match parse_agent_sse_event("agent.message", &message)
            .unwrap()
            .unwrap()
        {
            AgentSseEvent::Message(event) => {
                assert_eq!(event.message_id, message_id);
                assert_eq!(event.message, "hello");
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let item_id = Uuid::new_v4();
        let reaction = format!(r#"{{"item_id":"{item_id}","emoji":"👍"}}"#);
        match parse_agent_sse_event("agent.reaction", &reaction)
            .unwrap()
            .unwrap()
        {
            AgentSseEvent::Reaction(event) => {
                assert_eq!(event.item_id, item_id);
                assert_eq!(event.emoji, "👍");
            }
            other => panic!("unexpected event: {other:?}"),
        }

        assert!(parse_agent_sse_event("unknown", "{}").is_none());
    }
}

#[cfg(test)]
mod recovery_tests;
