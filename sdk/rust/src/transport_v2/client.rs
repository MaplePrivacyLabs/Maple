use std::{
    collections::HashMap,
    net::IpAddr,
    pin::Pin,
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use bytes::{Bytes, BytesMut};
use futures::{Stream, StreamExt};
use p256::elliptic_curve::rand_core::{OsRng, RngCore};
use reqwest::{header, redirect::Policy, Client, Url};
use serde::{Deserialize, Serialize};
use x25519_dalek::{EphemeralSecret, PublicKey};
use zeroize::Zeroizing;

use crate::{
    attestation::{AttestationDocument, AttestationVerifier},
    cbor::{self, Value as CborValue},
    pcr::Pcr0TrustPolicy,
};

use super::{
    crypto::{
        attestation_user_data, derive_client_session, HandshakeTranscript, SessionId,
        SessionSecrets, HANDSHAKE_CHALLENGE_BYTES, MIN_REQUEST_RECORD_BYTES,
        X25519_PUBLIC_KEY_BYTES,
    },
    envelope::{LogicalRequest, RequestId},
    framing::{ResponseDecoder, ResponseEvent},
    Result, TransportV2Error, ROUTING_KEY_HEADER,
};

const VERSION: u8 = 2;
const MAX_SESSION_RESPONSE_BYTES: usize = 64 * 1024;
const EXPECTED_SESSION_LIFETIME_SECONDS: u64 = 60 * 60;
const SESSION_EXPIRY_SKEW: Duration = Duration::from_secs(30);
const SESSION_CONTENT_TYPE: &str = "application/json";
const REQUEST_CONTENT_TYPE: &str = "application/octet-stream";

pub(crate) type ResponseEventStream =
    Pin<Box<dyn Stream<Item = Result<ResponseEvent>> + Send + 'static>>;

pub(crate) struct TransportV2Client {
    client: Client,
    base_url: String,
    use_mock_attestation: bool,
    pcr0_trust_policy: Pcr0TrustPolicy,
}

pub(crate) struct TransportV2Session {
    secrets: Arc<SessionSecrets>,
    routing_key: [u8; HANDSHAKE_CHALLENGE_BYTES],
    expires_at: Instant,
}

/// One request identifier reserved by its owning session for a future exact
/// request, such as native-handoff redemption. It is deliberately non-Clone
/// and is consumed by `send_with_id`; no client-side replay registry exists.
pub(crate) struct PreparedRequestId {
    session_id: SessionId,
    request_id: RequestId,
}

/// A single request after its one-time ID has been allocated and its complete
/// logical contents have been sealed. This value is deliberately non-Clone and
/// is consumed by `send_sealed`, preserving the transport's no-resend rule.
pub(crate) struct SealedRequest {
    session_id: SessionId,
    routing_key: [u8; HANDSHAKE_CHALLENGE_BYTES],
    encrypted: Vec<u8>,
    response_opener: super::crypto::ResponseOpener,
}

impl std::fmt::Debug for TransportV2Session {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TransportV2Session")
            .field("session_id", &self.secrets.session_id())
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl TransportV2Session {
    pub(crate) fn encoded_id(&self) -> String {
        self.secrets.session_id().to_string()
    }

    pub(crate) fn id_bytes(&self) -> [u8; 16] {
        *self.secrets.session_id().as_bytes()
    }

    pub(crate) fn is_expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }

    pub(crate) fn prepare_request_id(&self) -> Result<PreparedRequestId> {
        if self.is_expired() {
            return Err(TransportV2Error::SessionExpired);
        }
        Ok(PreparedRequestId {
            session_id: self.secrets.session_id(),
            request_id: RequestId::random()?,
        })
    }
}

impl PreparedRequestId {
    pub(crate) fn encoded(&self) -> String {
        self.request_id.to_string()
    }
}

#[derive(Serialize)]
struct CreateSessionRequest {
    version: u8,
    challenge: String,
    client_public_key: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateSessionResponse {
    version: u8,
    session_id: String,
    attestation_document: String,
    expires_in_seconds: u64,
}

impl TransportV2Client {
    pub(crate) fn new(base_url: String, pcr0_trust_policy: Pcr0TrustPolicy) -> Result<Self> {
        let (base_url, use_mock_attestation) = canonical_base_url(base_url)?;
        let client = Client::builder()
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .map_err(|_| TransportV2Error::InvalidConfiguration)?;
        Ok(Self {
            client,
            base_url,
            use_mock_attestation,
            pcr0_trust_policy,
        })
    }

    /// Establish one session from a single attested X25519 exchange.
    ///
    /// A prepared client secret is never retried. Production documents pass
    /// Nitro verification and PCR policy before any derived key is trusted.
    pub(crate) async fn establish_session(&self) -> Result<TransportV2Session> {
        let started_at = Instant::now();
        let challenge = fresh_challenge()?;
        let encoded_routing_key = STANDARD.encode(challenge);
        let client_secret = EphemeralSecret::random_from_rng(OsRng);
        let client_public_key = PublicKey::from(&client_secret).to_bytes();
        let request = CreateSessionRequest {
            version: VERSION,
            challenge: encoded_routing_key.clone(),
            client_public_key: STANDARD.encode(client_public_key),
        };

        let response = self
            .client
            .post(format!("{}/v2/session", self.base_url))
            .header(header::CONTENT_TYPE, SESSION_CONTENT_TYPE)
            .header(ROUTING_KEY_HEADER, encoded_routing_key)
            .json(&request)
            .send()
            .await
            .map_err(TransportV2Error::Http)?;
        if response.status() != reqwest::StatusCode::OK
            || exact_content_type(response.headers()) != Some(SESSION_CONTENT_TYPE)
        {
            return Err(TransportV2Error::UntrustedOuterResponse);
        }
        let body = read_bounded_response(response, MAX_SESSION_RESPONSE_BYTES).await?;
        let response: CreateSessionResponse =
            serde_json::from_slice(&body).map_err(|_| TransportV2Error::InvalidSessionResponse)?;
        if response.version != VERSION
            || response.expires_in_seconds != EXPECTED_SESSION_LIFETIME_SECONDS
        {
            return Err(TransportV2Error::InvalidSessionResponse);
        }

        let document = if self.use_mock_attestation {
            parse_mock_attestation(&response.attestation_document)?
        } else {
            let document = AttestationVerifier::new()
                .verify_attestation_document_bytes(&response.attestation_document, &challenge)
                .map_err(|_| TransportV2Error::AttestationRejected)?;
            verify_attested_pcr0(&self.pcr0_trust_policy, &document).await?;
            document
        };

        let server_public_key =
            validate_attested_transcript(&document, &challenge, &client_public_key)?;
        let transcript = HandshakeTranscript::new(challenge, client_public_key, server_public_key);
        let secrets = derive_client_session(client_secret, &transcript)?;
        let stated_session_id = SessionId::from_str(&response.session_id)?;
        if secrets.session_id() != stated_session_id {
            return Err(TransportV2Error::InvalidSessionResponse);
        }
        Ok(TransportV2Session {
            secrets: Arc::new(secrets),
            routing_key: challenge,
            expires_at: started_at
                + Duration::from_secs(EXPECTED_SESSION_LIFETIME_SECONDS)
                    .saturating_sub(SESSION_EXPIRY_SKEW),
        })
    }

    /// Send exactly one encrypted request. This method never retries or falls
    /// back to transport v1.
    #[cfg(test)]
    pub(crate) async fn send(
        &self,
        session: &TransportV2Session,
        request: LogicalRequest,
    ) -> Result<ResponseEventStream> {
        let sealed = self.seal(session, request)?;
        self.send_sealed(sealed).await
    }

    #[cfg(test)]
    pub(crate) async fn send_with_id(
        &self,
        session: &TransportV2Session,
        prepared: PreparedRequestId,
        request: LogicalRequest,
    ) -> Result<ResponseEventStream> {
        let sealed = self.seal_with_id(session, prepared, request)?;
        self.send_sealed(sealed).await
    }

    /// Allocate a one-time request ID and synchronously seal one logical
    /// request. No network work or other await point occurs here.
    #[cfg(test)]
    pub(crate) fn seal(
        &self,
        session: &TransportV2Session,
        request: LogicalRequest,
    ) -> Result<SealedRequest> {
        let prepared = session.prepare_request_id()?;
        self.seal_with_id(session, prepared, request)
    }

    pub(crate) fn seal_with_id(
        &self,
        session: &TransportV2Session,
        prepared: PreparedRequestId,
        request: LogicalRequest,
    ) -> Result<SealedRequest> {
        let plaintext = request.encode()?;
        drop(request);
        self.seal_encoded_with_id(session, prepared, &plaintext)
    }

    /// Seal captured logical bytes under a fresh one-use request ID. Keeping
    /// the encoded request permits bounded recovery without re-reading input
    /// bodies, credentials, headers, or the cache namespace root.
    pub(crate) fn seal_encoded(
        &self,
        session: &TransportV2Session,
        plaintext: &[u8],
    ) -> Result<SealedRequest> {
        let prepared = session.prepare_request_id()?;
        self.seal_encoded_with_id(session, prepared, plaintext)
    }

    fn seal_encoded_with_id(
        &self,
        session: &TransportV2Session,
        prepared: PreparedRequestId,
        plaintext: &[u8],
    ) -> Result<SealedRequest> {
        if session.is_expired() {
            return Err(TransportV2Error::SessionExpired);
        }
        if prepared.session_id != session.secrets.session_id() {
            return Err(TransportV2Error::InvalidRequest);
        }
        let request_id = prepared.request_id;
        let encrypted = session.secrets.encrypt_request(request_id, plaintext)?;
        let response_opener = session.secrets.response_opener(request_id)?;
        debug_assert!(encrypted.len() >= MIN_REQUEST_RECORD_BYTES);

        Ok(SealedRequest {
            session_id: session.secrets.session_id(),
            routing_key: session.routing_key,
            encrypted,
            response_opener,
        })
    }

    /// Transmit one already-sealed request exactly once. Callers must not
    /// retain or reconstruct `sealed` after this function begins.
    pub(crate) async fn send_sealed(&self, sealed: SealedRequest) -> Result<ResponseEventStream> {
        let response = self
            .client
            .post(format!("{}/v2/request", self.base_url))
            .header(header::CONTENT_TYPE, REQUEST_CONTENT_TYPE)
            .header("x-session-id", sealed.session_id.to_string())
            .header(ROUTING_KEY_HEADER, STANDARD.encode(sealed.routing_key))
            .body(sealed.encrypted)
            .send()
            .await
            .map_err(TransportV2Error::Http)?;
        if let Some(hint) = session_recovery_hint(response.status(), response.headers()) {
            return Err(TransportV2Error::SessionRecoveryHint(hint));
        }
        if response.status() != reqwest::StatusCode::OK
            || exact_content_type(response.headers()) != Some(REQUEST_CONTENT_TYPE)
        {
            return Err(TransportV2Error::UntrustedOuterResponse);
        }

        let mut source = response.bytes_stream();
        let mut decoder = ResponseDecoder::new(sealed.response_opener);
        let stream = async_stream::try_stream! {
            while let Some(chunk) = source.next().await {
                let chunk = chunk.map_err(TransportV2Error::Http)?;
                for event in decoder.push(&chunk)? {
                    yield event;
                }
            }
            yield decoder.finish()?;
        };
        Ok(Box::pin(stream))
    }
}

async fn verify_attested_pcr0(
    policy: &Pcr0TrustPolicy,
    document: &AttestationDocument,
) -> Result<()> {
    let pcr0 = document
        .pcrs
        .get(&0)
        .ok_or(TransportV2Error::AttestationRejected)?;
    policy
        .verify_pcr0(pcr0)
        .await
        .map_err(|_| TransportV2Error::AttestationRejected)
}

fn fresh_challenge() -> Result<[u8; HANDSHAKE_CHALLENGE_BYTES]> {
    let mut randomness = Zeroizing::new([0_u8; HANDSHAKE_CHALLENGE_BYTES]);
    OsRng
        .try_fill_bytes(&mut *randomness)
        .map_err(|_| TransportV2Error::RandomnessUnavailable)?;
    Ok(*randomness)
}

fn validate_attested_transcript(
    document: &AttestationDocument,
    challenge: &[u8; HANDSHAKE_CHALLENGE_BYTES],
    client_public_key: &[u8; X25519_PUBLIC_KEY_BYTES],
) -> Result<[u8; X25519_PUBLIC_KEY_BYTES]> {
    if document.nonce.as_deref() != Some(challenge.as_slice())
        || document.user_data.as_deref()
            != Some(attestation_user_data(client_public_key).as_slice())
    {
        return Err(TransportV2Error::AttestationRejected);
    }
    document
        .public_key
        .as_deref()
        .ok_or(TransportV2Error::AttestationRejected)?
        .try_into()
        .map_err(|_| TransportV2Error::AttestationRejected)
}

const ERROR_CONTRACT_HEADER: &str = "x-opensecret-error-contract";
const ERROR_CODE_HEADER: &str = "x-opensecret-error-code";

fn session_recovery_hint(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
) -> Option<super::SessionRecoveryHint> {
    use super::SessionRecoveryHint;

    if status != reqwest::StatusCode::BAD_REQUEST
        || exact_header(headers, ERROR_CONTRACT_HEADER) != Some("1")
    {
        return None;
    }
    match exact_header(headers, ERROR_CODE_HEADER)? {
        "session_not_found" => Some(SessionRecoveryHint::SessionNotFound),
        "request_decryption_failed" => Some(SessionRecoveryHint::RequestDecryptionFailed),
        _ => None,
    }
}

fn exact_header<'a>(headers: &'a reqwest::header::HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    Some(value)
}

fn exact_content_type(headers: &reqwest::header::HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(header::CONTENT_TYPE).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    Some(value)
}

async fn read_bounded_response(response: reqwest::Response, limit: usize) -> Result<Bytes> {
    let mut source = response.bytes_stream();
    let mut body = BytesMut::new();
    while let Some(chunk) = source.next().await {
        let chunk = chunk.map_err(TransportV2Error::Http)?;
        let next = body
            .len()
            .checked_add(chunk.len())
            .ok_or(TransportV2Error::InvalidSessionResponse)?;
        if next > limit {
            return Err(TransportV2Error::InvalidSessionResponse);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body.freeze())
}

fn canonical_base_url(base_url: String) -> Result<(String, bool)> {
    let mut parsed = Url::parse(&base_url).map_err(|_| TransportV2Error::InvalidConfiguration)?;
    if !matches!(parsed.scheme(), "http" | "https")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(TransportV2Error::InvalidConfiguration);
    }
    let host = parsed
        .host_str()
        .ok_or(TransportV2Error::InvalidConfiguration)?
        .trim_end_matches('.');
    let local_host = if host.eq_ignore_ascii_case("localhost") {
        true
    } else {
        let address_host = host
            .strip_prefix('[')
            .and_then(|value| value.strip_suffix(']'))
            .unwrap_or(host);
        address_host.parse::<IpAddr>().is_ok_and(|address| {
            address.is_loopback()
                || (cfg!(target_os = "android") && address == IpAddr::from([10, 0, 2, 2]))
        })
    };
    let use_mock_attestation = parsed.scheme() == "http" && local_host;
    if parsed.scheme() != "https" && !use_mock_attestation {
        return Err(TransportV2Error::InvalidConfiguration);
    }

    let path = parsed.path().trim_end_matches('/').to_string();
    parsed.set_path(if path.is_empty() { "/" } else { &path });
    Ok((
        parsed.as_str().trim_end_matches('/').to_string(),
        use_mock_attestation,
    ))
}

fn parse_mock_attestation(document_b64: &str) -> Result<AttestationDocument> {
    let document_bytes = STANDARD
        .decode(document_b64)
        .map_err(|_| TransportV2Error::AttestationRejected)?;
    let cose: CborValue =
        cbor::from_slice(&document_bytes).map_err(|_| TransportV2Error::AttestationRejected)?;
    let payload = match cose {
        CborValue::Array(values) if values.len() == 4 => match &values[2] {
            CborValue::Bytes(bytes) => bytes.clone(),
            _ => return Err(TransportV2Error::AttestationRejected),
        },
        _ => return Err(TransportV2Error::AttestationRejected),
    };
    let document: CborValue =
        cbor::from_slice(&payload).map_err(|_| TransportV2Error::AttestationRejected)?;
    let map = match document {
        CborValue::Map(map) => map,
        _ => return Err(TransportV2Error::AttestationRejected),
    };

    let mut public_key = None;
    let mut user_data = None;
    let mut nonce = None;
    for (key, value) in map {
        let CborValue::Text(key) = key else {
            continue;
        };
        match (key.as_str(), value) {
            ("public_key", CborValue::Bytes(bytes)) => public_key = Some(bytes),
            ("user_data", CborValue::Bytes(bytes)) => user_data = Some(bytes),
            ("nonce", CborValue::Bytes(bytes)) => nonce = Some(bytes),
            _ => {}
        }
    }
    Ok(AttestationDocument {
        module_id: "mock-module".to_string(),
        timestamp: 1,
        digest: "SHA384".to_string(),
        pcrs: HashMap::new(),
        certificate: Vec::new(),
        cabundle: Vec::new(),
        public_key,
        user_data,
        nonce,
    })
}

#[cfg(test)]
impl TransportV2Session {
    fn from_secrets(secrets: SessionSecrets) -> Self {
        Self::from_secrets_with_expiry(secrets, Instant::now() + Duration::from_secs(60))
    }

    fn from_secrets_with_expiry(secrets: SessionSecrets, expires_at: Instant) -> Self {
        Self {
            secrets: Arc::new(secrets),
            routing_key: [0x11; HANDSHAKE_CHALLENGE_BYTES],
            expires_at,
        }
    }
}

#[cfg(test)]
pub(crate) use tests::{SessionResponder, TestV2ServerState};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport_v2::{
        crypto::{open_request_for_test, session_from_shared_for_test, HandshakeTranscript},
        envelope::{Credential, CredentialKind, LogicalHeader, LogicalRequest},
        framing::frame_response_for_test,
    };
    use http::Method;
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex as StdMutex},
    };
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, Request, Respond, ResponseTemplate,
    };
    use x25519_dalek::StaticSecret;

    #[derive(Clone)]
    pub(crate) struct TestV2ServerState {
        secrets: Arc<StdMutex<Option<Arc<SessionSecrets>>>>,
        responses: Arc<StdMutex<VecDeque<TestLogicalResponse>>>,
        requests: Arc<StdMutex<Vec<serde_json::Value>>>,
        request_ids: Arc<StdMutex<Vec<String>>>,
        request_bodies: Arc<StdMutex<Vec<Bytes>>>,
        request_plaintexts: Arc<StdMutex<Vec<Bytes>>>,
    }

    struct TestLogicalResponse {
        status: u16,
        headers: Vec<LogicalHeader>,
        body: Bytes,
        delay: Option<Duration>,
    }

    impl TestV2ServerState {
        pub(crate) fn new() -> Self {
            Self {
                secrets: Arc::new(StdMutex::new(None)),
                responses: Arc::new(StdMutex::new(VecDeque::new())),
                requests: Arc::new(StdMutex::new(Vec::new())),
                request_ids: Arc::new(StdMutex::new(Vec::new())),
                request_bodies: Arc::new(StdMutex::new(Vec::new())),
                request_plaintexts: Arc::new(StdMutex::new(Vec::new())),
            }
        }

        pub(crate) fn queue_json_response(&self, status: u16, body: serde_json::Value) {
            self.responses
                .lock()
                .unwrap()
                .push_back(TestLogicalResponse {
                    status,
                    headers: vec![LogicalHeader::new(
                        "content-type".to_string(),
                        "application/json".to_string(),
                    )
                    .unwrap()],
                    body: Bytes::from(serde_json::to_vec(&body).unwrap()),
                    delay: None,
                });
        }

        pub(crate) fn queue_delayed_json_response(
            &self,
            status: u16,
            body: serde_json::Value,
            delay: Duration,
        ) {
            self.responses
                .lock()
                .unwrap()
                .push_back(TestLogicalResponse {
                    status,
                    headers: vec![LogicalHeader::new(
                        "content-type".to_string(),
                        "application/json".to_string(),
                    )
                    .unwrap()],
                    body: Bytes::from(serde_json::to_vec(&body).unwrap()),
                    delay: Some(delay),
                });
        }

        pub(crate) fn captured_requests(&self) -> Vec<serde_json::Value> {
            self.requests.lock().unwrap().clone()
        }

        pub(crate) fn captured_request_ids(&self) -> Vec<String> {
            self.request_ids.lock().unwrap().clone()
        }

        pub(crate) fn captured_request_bodies(&self) -> Vec<Bytes> {
            self.request_bodies.lock().unwrap().clone()
        }

        pub(crate) fn captured_request_plaintexts(&self) -> Vec<Bytes> {
            self.request_plaintexts.lock().unwrap().clone()
        }

        pub(crate) fn request_responder(&self) -> RequestResponder {
            RequestResponder {
                state: self.clone(),
                transform: None,
            }
        }

        pub(crate) fn request_responder_with_wire_transform(
            &self,
            transform: fn(Vec<u8>) -> Vec<u8>,
        ) -> RequestResponder {
            RequestResponder {
                state: self.clone(),
                transform: Some(transform),
            }
        }
    }

    pub(crate) struct SessionResponder {
        pub(crate) server_secret: [u8; 32],
        pub(crate) state: Option<TestV2ServerState>,
        pub(crate) delay: Option<Duration>,
    }

    pub(crate) struct RequestResponder {
        state: TestV2ServerState,
        transform: Option<fn(Vec<u8>) -> Vec<u8>>,
    }

    impl Respond for SessionResponder {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let request: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let challenge: [u8; 32] = STANDARD
                .decode(request["challenge"].as_str().unwrap())
                .unwrap()
                .try_into()
                .unwrap();
            let client_public_key: [u8; 32] = STANDARD
                .decode(request["client_public_key"].as_str().unwrap())
                .unwrap()
                .try_into()
                .unwrap();
            let server_secret = StaticSecret::from(self.server_secret);
            let server_public_key = PublicKey::from(&server_secret).to_bytes();
            let shared_secret = server_secret.diffie_hellman(&PublicKey::from(client_public_key));
            let transcript =
                HandshakeTranscript::new(challenge, client_public_key, server_public_key);
            let secrets = session_from_shared_for_test(*shared_secret.as_bytes(), transcript);
            let session_id = secrets.session_id();
            if let Some(state) = &self.state {
                *state.secrets.lock().unwrap() = Some(Arc::new(secrets));
            }

            let payload = CborValue::Map(vec![
                (
                    CborValue::Text("public_key".into()),
                    CborValue::Bytes(server_public_key.to_vec()),
                ),
                (
                    CborValue::Text("user_data".into()),
                    CborValue::Bytes(attestation_user_data(&client_public_key)),
                ),
                (
                    CborValue::Text("nonce".into()),
                    CborValue::Bytes(challenge.to_vec()),
                ),
            ]);
            let cose = CborValue::Array(vec![
                CborValue::Bytes(Vec::new()),
                CborValue::Map(Vec::new()),
                CborValue::Bytes(cbor::to_vec(&payload).unwrap()),
                CborValue::Bytes(Vec::new()),
            ]);
            let response = ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "version": VERSION,
                "session_id": session_id.to_string(),
                "attestation_document": STANDARD.encode(cbor::to_vec(&cose).unwrap()),
                "expires_in_seconds": EXPECTED_SESSION_LIFETIME_SECONDS,
            }));
            match self.delay {
                Some(delay) => response.set_delay(delay),
                None => response,
            }
        }
    }

    impl Respond for RequestResponder {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let secrets = self
                .state
                .secrets
                .lock()
                .unwrap()
                .clone()
                .expect("test session was established before its request");
            let (request_id, plaintext) = open_request_for_test(&secrets, &request.body);
            self.state
                .request_plaintexts
                .lock()
                .unwrap()
                .push(Bytes::copy_from_slice(&plaintext));
            let metadata_length = u32::from_be_bytes(
                plaintext[..4]
                    .try_into()
                    .expect("test request metadata length is present"),
            ) as usize;
            let metadata: serde_json::Value =
                serde_json::from_slice(&plaintext[4..4 + metadata_length]).unwrap();
            self.state.requests.lock().unwrap().push(metadata);
            self.state
                .request_ids
                .lock()
                .unwrap()
                .push(request_id.to_string());
            self.state
                .request_bodies
                .lock()
                .unwrap()
                .push(Bytes::copy_from_slice(&plaintext[4 + metadata_length..]));

            let response = self
                .state
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("test response was queued");
            let mut start = vec![1];
            start.extend(
                serde_json::to_vec(&serde_json::json!({
                    "status": response.status,
                    "headers": response.headers,
                }))
                .unwrap(),
            );
            let mut wire = frame_response_for_test(&secrets, request_id, 0, &start);
            let mut sequence = 1;
            if !response.body.is_empty() {
                let chunk = [&[2][..], response.body.as_ref()].concat();
                wire.extend(frame_response_for_test(
                    &secrets, request_id, sequence, &chunk,
                ));
                sequence += 1;
            }
            wire.extend(frame_response_for_test(
                &secrets,
                request_id,
                sequence,
                &[3],
            ));
            if let Some(transform) = self.transform {
                wire = transform(wire);
            }
            let response_template = ResponseTemplate::new(200)
                .insert_header("content-type", REQUEST_CONTENT_TYPE)
                .set_body_bytes(wire);
            match response.delay {
                Some(delay) => response_template.set_delay(delay),
                None => response_template,
            }
        }
    }

    #[test]
    fn recovery_hint_requires_exact_single_contract_and_code_headers() {
        use super::super::SessionRecoveryHint;
        let mut headers = header::HeaderMap::new();
        for (code, expected) in [
            ("session_not_found", SessionRecoveryHint::SessionNotFound),
            (
                "request_decryption_failed",
                SessionRecoveryHint::RequestDecryptionFailed,
            ),
        ] {
            headers.insert(ERROR_CONTRACT_HEADER, "1".parse().unwrap());
            headers.insert(ERROR_CODE_HEADER, code.parse().unwrap());
            assert_eq!(
                session_recovery_hint(reqwest::StatusCode::BAD_REQUEST, &headers),
                Some(expected)
            );
            for status in [200, 401, 403, 404, 409, 429, 500, 503] {
                assert_eq!(
                    session_recovery_hint(reqwest::StatusCode::from_u16(status).unwrap(), &headers),
                    None
                );
            }
            for name in [ERROR_CONTRACT_HEADER, ERROR_CODE_HEADER] {
                let valid = headers[name].clone();
                for invalid in [
                    "",
                    " ",
                    "1, 1",
                    "session_not_found,session_not_found",
                    "SESSION_NOT_FOUND",
                    "request_authentication_failed",
                ] {
                    headers.insert(name, invalid.parse().unwrap());
                    assert_eq!(
                        session_recovery_hint(reqwest::StatusCode::BAD_REQUEST, &headers),
                        None
                    );
                }
                headers.insert(name, header::HeaderValue::from_bytes(&[0xff]).unwrap());
                assert_eq!(
                    session_recovery_hint(reqwest::StatusCode::BAD_REQUEST, &headers),
                    None
                );
                headers.remove(name);
                assert_eq!(
                    session_recovery_hint(reqwest::StatusCode::BAD_REQUEST, &headers),
                    None
                );
                headers.insert(name, valid.clone());
                headers.append(name, valid.clone());
                assert_eq!(
                    session_recovery_hint(reqwest::StatusCode::BAD_REQUEST, &headers),
                    None
                );
                headers.insert(name, valid);
            }
        }
    }

    fn test_session() -> TransportV2Session {
        TransportV2Session::from_secrets(session_from_shared_for_test(
            [0x44; 32],
            HandshakeTranscript::new([0x11; 32], [0x22; 32], [0x33; 32]),
        ))
    }

    fn test_attestation_document(pcr0: Option<Vec<u8>>) -> AttestationDocument {
        let mut pcrs = HashMap::new();
        if let Some(pcr0) = pcr0 {
            pcrs.insert(0, pcr0);
        }
        AttestationDocument {
            module_id: "test-module".to_string(),
            timestamp: 1,
            digest: "SHA384".to_string(),
            pcrs,
            certificate: Vec::new(),
            cabundle: Vec::new(),
            public_key: Some(vec![0x33; X25519_PUBLIC_KEY_BYTES]),
            user_data: Some(attestation_user_data(&[0x22; X25519_PUBLIC_KEY_BYTES])),
            nonce: Some(vec![0x11; HANDSHAKE_CHALLENGE_BYTES]),
        }
    }

    #[test]
    fn only_exact_http_loopback_uses_mock_attestation() {
        for url in [
            "http://localhost:3000",
            "http://localhost.:3000",
            "http://127.0.0.1:3000",
            "http://[::1]:3000",
        ] {
            assert!(canonical_base_url(url.into()).unwrap().1, "{url}");
        }
        assert!(
            !canonical_base_url("https://localhost:3000".into())
                .unwrap()
                .1
        );
        for url in [
            "http://example.com",
            "http://localhost.example.com",
            "http://127.0.0.1.example.com",
            "http://0.0.0.0:3000",
            "https://user@example.com",
            "https://example.com?redirect=http://localhost",
            "https://example.com#localhost",
        ] {
            assert!(canonical_base_url(url.into()).is_err(), "{url}");
        }
        assert_eq!(
            canonical_base_url("http://10.0.2.2:3000".into()).is_ok(),
            cfg!(target_os = "android")
        );
    }

    #[tokio::test]
    async fn one_round_mock_handshake_binds_both_keys_and_binary_challenge() {
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
        let client = TransportV2Client::new(server.uri(), Pcr0TrustPolicy::default()).unwrap();
        let session = client.establish_session().await.unwrap();
        assert_eq!(session.encoded_id().len(), 32);
        assert!(session.prepare_request_id().is_ok());
    }

    #[tokio::test]
    async fn attestation_challenge_routing_key_is_reused_for_the_session_request() {
        let server = MockServer::start().await;
        let state = TestV2ServerState::new();
        state.queue_json_response(200, serde_json::json!({"ok": true}));
        Mock::given(method("POST"))
            .and(path("/v2/session"))
            .respond_with(SessionResponder {
                server_secret: [0x77; 32],
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

        let client = TransportV2Client::new(server.uri(), Pcr0TrustPolicy::default()).unwrap();
        let session = client.establish_session().await.unwrap();
        let request =
            LogicalRequest::new(None, None, Method::GET, "/v1/models".into(), vec![], None)
                .unwrap();
        let mut response = client.send(&session, request).await.unwrap();
        while let Some(event) = response.next().await {
            event.unwrap();
        }

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        let session_routing_key = requests[0]
            .headers
            .get(ROUTING_KEY_HEADER)
            .unwrap()
            .to_str()
            .unwrap();
        let request_routing_key = requests[1]
            .headers
            .get(ROUTING_KEY_HEADER)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(request_routing_key, session_routing_key);
        let session_body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(session_body["challenge"], session_routing_key);
        let decoded = STANDARD.decode(session_routing_key).unwrap();
        assert_eq!(decoded.len(), HANDSHAKE_CHALLENGE_BYTES);
        assert_eq!(STANDARD.encode(decoded), session_routing_key);
        assert!(session_routing_key.ends_with('='));
    }

    #[test]
    fn transcript_rejects_mismatched_nonce_or_client_key_binding() {
        let challenge = [0x11; HANDSHAKE_CHALLENGE_BYTES];
        let client_public_key = [0x22; X25519_PUBLIC_KEY_BYTES];
        let valid = test_attestation_document(None);
        assert_eq!(
            validate_attested_transcript(&valid, &challenge, &client_public_key).unwrap(),
            [0x33; X25519_PUBLIC_KEY_BYTES]
        );

        let mut wrong_nonce = valid.clone();
        wrong_nonce.nonce = Some(vec![0x12; HANDSHAKE_CHALLENGE_BYTES]);
        assert!(matches!(
            validate_attested_transcript(&wrong_nonce, &challenge, &client_public_key),
            Err(TransportV2Error::AttestationRejected)
        ));

        let mut wrong_client_key = valid;
        wrong_client_key.user_data = Some(attestation_user_data(&[0x23; X25519_PUBLIC_KEY_BYTES]));
        assert!(matches!(
            validate_attested_transcript(&wrong_client_key, &challenge, &client_public_key),
            Err(TransportV2Error::AttestationRejected)
        ));
    }

    #[tokio::test]
    async fn verified_document_must_pass_the_configured_pcr0_policy() {
        let approved = vec![0x42; 48];
        let policy = Pcr0TrustPolicy::from_static_allowlist([hex::encode(&approved)]).unwrap();

        verify_attested_pcr0(&policy, &test_attestation_document(Some(approved)))
            .await
            .unwrap();
        assert!(matches!(
            verify_attested_pcr0(&policy, &test_attestation_document(Some(vec![0x43; 48]))).await,
            Err(TransportV2Error::AttestationRejected)
        ));
        assert!(matches!(
            verify_attested_pcr0(&policy, &test_attestation_document(None)).await,
            Err(TransportV2Error::AttestationRejected)
        ));
    }

    #[tokio::test]
    async fn unavailable_v2_endpoint_never_falls_back_to_v1() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/session"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;

        let client = TransportV2Client::new(server.uri(), Pcr0TrustPolicy::default()).unwrap();
        assert!(matches!(
            client.establish_session().await,
            Err(TransportV2Error::UntrustedOuterResponse)
        ));
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.path(), "/v2/session");
    }

    #[tokio::test]
    async fn redirects_are_not_followed_during_session_setup() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/session"))
            .respond_with(ResponseTemplate::new(307).insert_header("location", "/must-not-follow"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/must-not-follow"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let client = TransportV2Client::new(server.uri(), Pcr0TrustPolicy::default()).unwrap();
        assert!(matches!(
            client.establish_session().await,
            Err(TransportV2Error::UntrustedOuterResponse)
        ));
    }

    #[tokio::test]
    async fn request_outer_carries_only_transport_metadata_and_is_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/request"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;
        let client = TransportV2Client::new(server.uri(), Pcr0TrustPolicy::default()).unwrap();
        let request = LogicalRequest::new(
            Some(Credential::new(CredentialKind::ApiKey, "inside-only-secret".into()).unwrap()),
            None,
            Method::GET,
            "/v1/models".into(),
            vec![],
            None,
        )
        .unwrap();
        assert!(matches!(
            client.send(&test_session(), request).await,
            Err(TransportV2Error::UntrustedOuterResponse)
        ));

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(
            request.headers.get("content-type").unwrap(),
            REQUEST_CONTENT_TYPE
        );
        assert_eq!(
            request.headers.get("x-session-id").unwrap(),
            "f7258fb103137c612baab47ced4a5a02"
        );
        assert_eq!(
            request
                .headers
                .get(ROUTING_KEY_HEADER)
                .unwrap()
                .to_str()
                .unwrap(),
            STANDARD.encode([0x11; HANDSHAKE_CHALLENGE_BYTES])
        );
        for forbidden in ["authorization", "proxy-authorization", "cookie"] {
            assert!(!request.headers.contains_key(forbidden));
        }
        assert!(request.body.len() >= MIN_REQUEST_RECORD_BYTES);
        assert!(!request
            .body
            .windows(b"inside-only-secret".len())
            .any(|window| window == b"inside-only-secret"));
    }

    #[tokio::test]
    async fn prepared_request_id_is_consumed_and_bound_to_its_session() {
        let server = MockServer::start().await;
        let client = TransportV2Client::new(server.uri(), Pcr0TrustPolicy::default()).unwrap();
        let first = test_session();
        let second = TransportV2Session::from_secrets(session_from_shared_for_test(
            [0x45; 32],
            HandshakeTranscript::new([0x11; 32], [0x22; 32], [0x33; 32]),
        ));
        let prepared = first.prepare_request_id().unwrap();
        assert_eq!(prepared.encoded().len(), 32);
        let request =
            LogicalRequest::new(None, None, Method::GET, "/v1/models".into(), vec![], None)
                .unwrap();
        assert!(matches!(
            client.send_with_id(&second, prepared, request).await,
            Err(TransportV2Error::InvalidRequest)
        ));
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn expired_sessions_fail_before_request_id_or_network_use() {
        let server = MockServer::start().await;
        let client = TransportV2Client::new(server.uri(), Pcr0TrustPolicy::default()).unwrap();
        let expired = TransportV2Session::from_secrets_with_expiry(
            session_from_shared_for_test(
                [0x44; 32],
                HandshakeTranscript::new([0x11; 32], [0x22; 32], [0x33; 32]),
            ),
            Instant::now(),
        );
        assert!(matches!(
            expired.prepare_request_id(),
            Err(TransportV2Error::SessionExpired)
        ));
        let request =
            LogicalRequest::new(None, None, Method::GET, "/v1/models".into(), vec![], None)
                .unwrap();
        assert!(matches!(
            client.send(&expired, request).await,
            Err(TransportV2Error::SessionExpired)
        ));
        assert!(server.received_requests().await.unwrap().is_empty());
    }
}
