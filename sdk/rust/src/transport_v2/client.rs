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
    Result, TransportV2Error,
};

const VERSION: u8 = 2;
const MAX_SESSION_RESPONSE_BYTES: usize = 64 * 1024;
const EXPECTED_SESSION_LIFETIME_SECONDS: u64 = 65 * 60;
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
    expires_at: Instant,
}

/// One request identifier reserved by its owning session for a future exact
/// request, such as native-handoff redemption. It is deliberately non-Clone
/// and is consumed by `send_with_id`; no client-side replay registry exists.
pub(crate) struct PreparedRequestId {
    session_id: SessionId,
    request_id: RequestId,
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

    pub(crate) fn prepare_request_id(&self) -> Result<PreparedRequestId> {
        if Instant::now() >= self.expires_at {
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
        let client_secret = EphemeralSecret::random_from_rng(OsRng);
        let client_public_key = PublicKey::from(&client_secret).to_bytes();
        let request = CreateSessionRequest {
            version: VERSION,
            challenge: STANDARD.encode(challenge),
            client_public_key: STANDARD.encode(client_public_key),
        };

        let response = self
            .client
            .post(format!("{}/v2/session", self.base_url))
            .header(header::CONTENT_TYPE, SESSION_CONTENT_TYPE)
            .json(&request)
            .send()
            .await
            .map_err(|_| TransportV2Error::Http)?;
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
            let pcr0 = document
                .pcrs
                .get(&0)
                .ok_or(TransportV2Error::AttestationRejected)?;
            self.pcr0_trust_policy
                .verify_pcr0(pcr0)
                .await
                .map_err(|_| TransportV2Error::AttestationRejected)?;
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
            expires_at: started_at
                + Duration::from_secs(EXPECTED_SESSION_LIFETIME_SECONDS)
                    .saturating_sub(SESSION_EXPIRY_SKEW),
        })
    }

    /// Send exactly one encrypted request. This method never retries or falls
    /// back to transport v1.
    pub(crate) async fn send(
        &self,
        session: &TransportV2Session,
        request: LogicalRequest,
    ) -> Result<ResponseEventStream> {
        let prepared = session.prepare_request_id()?;
        self.send_with_id(session, prepared, request).await
    }

    pub(crate) async fn send_with_id(
        &self,
        session: &TransportV2Session,
        prepared: PreparedRequestId,
        request: LogicalRequest,
    ) -> Result<ResponseEventStream> {
        if Instant::now() >= session.expires_at {
            return Err(TransportV2Error::SessionExpired);
        }
        if prepared.session_id != session.secrets.session_id() {
            return Err(TransportV2Error::InvalidRequest);
        }
        let request_id = prepared.request_id;
        let plaintext = request.encode()?;
        drop(request);
        let encrypted = session.secrets.encrypt_request(request_id, &plaintext)?;
        drop(plaintext);
        let response_opener = session.secrets.response_opener(request_id)?;
        debug_assert!(encrypted.len() >= MIN_REQUEST_RECORD_BYTES);

        let response = self
            .client
            .post(format!("{}/v2/request", self.base_url))
            .header(header::CONTENT_TYPE, REQUEST_CONTENT_TYPE)
            .header("x-session-id", session.secrets.session_id().to_string())
            .body(encrypted)
            .send()
            .await
            .map_err(|_| TransportV2Error::Http)?;
        if response.status() != reqwest::StatusCode::OK
            || exact_content_type(response.headers()) != Some(REQUEST_CONTENT_TYPE)
        {
            return Err(TransportV2Error::UntrustedOuterResponse);
        }

        let mut source = response.bytes_stream();
        let mut decoder = ResponseDecoder::new(response_opener);
        let stream = async_stream::try_stream! {
            while let Some(chunk) = source.next().await {
                let chunk = chunk.map_err(|_| TransportV2Error::Http)?;
                for event in decoder.push(&chunk)? {
                    yield event;
                }
            }
            yield decoder.finish()?;
        };
        Ok(Box::pin(stream))
    }
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
        let chunk = chunk.map_err(|_| TransportV2Error::Http)?;
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
            expires_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport_v2::{
        crypto::{session_from_shared_for_test, HandshakeTranscript},
        envelope::LogicalRequest,
    };
    use http::Method;
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, Request, Respond, ResponseTemplate,
    };
    use x25519_dalek::StaticSecret;

    struct SessionResponder {
        server_secret: [u8; 32],
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
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "version": VERSION,
                "session_id": secrets.session_id().to_string(),
                "attestation_document": STANDARD.encode(cbor::to_vec(&cose).unwrap()),
                "expires_in_seconds": EXPECTED_SESSION_LIFETIME_SECONDS,
            }))
        }
    }

    fn test_session() -> TransportV2Session {
        TransportV2Session::from_secrets(session_from_shared_for_test(
            [0x44; 32],
            HandshakeTranscript::new([0x11; 32], [0x22; 32], [0x33; 32]),
        ))
    }

    #[test]
    fn only_exact_http_loopback_uses_mock_attestation() {
        assert!(
            canonical_base_url("http://127.0.0.1:3000".into())
                .unwrap()
                .1
        );
        assert!(
            !canonical_base_url("https://localhost:3000".into())
                .unwrap()
                .1
        );
        assert!(canonical_base_url("http://example.com".into()).is_err());
        assert!(canonical_base_url("https://user@example.com".into()).is_err());
    }

    #[tokio::test]
    async fn one_round_mock_handshake_binds_both_keys_and_binary_challenge() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/session"))
            .respond_with(SessionResponder {
                server_secret: [0x77; 32],
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
        assert_eq!(
            client.establish_session().await.err(),
            Some(TransportV2Error::UntrustedOuterResponse)
        );
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
        let request =
            LogicalRequest::new(None, None, Method::GET, "/v1/models".into(), vec![], None)
                .unwrap();
        assert_eq!(
            client.send(&test_session(), request).await.err(),
            Some(TransportV2Error::UntrustedOuterResponse)
        );

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
        for forbidden in ["authorization", "proxy-authorization", "cookie"] {
            assert!(!request.headers.contains_key(forbidden));
        }
        assert!(request.body.len() >= MIN_REQUEST_RECORD_BYTES);
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
        assert_eq!(
            client.send_with_id(&second, prepared, request).await.err(),
            Some(TransportV2Error::InvalidRequest)
        );
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
        assert_eq!(
            expired.prepare_request_id().err(),
            Some(TransportV2Error::SessionExpired)
        );
        let request =
            LogicalRequest::new(None, None, Method::GET, "/v1/models".into(), vec![], None)
                .unwrap();
        assert_eq!(
            client.send(&expired, request).await.err(),
            Some(TransportV2Error::SessionExpired)
        );
        assert!(server.received_requests().await.unwrap().is_empty());
    }
}
