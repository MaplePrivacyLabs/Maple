use std::{fmt, str::FromStr};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use bytes::Bytes;
use http::{HeaderName, HeaderValue, Method, Uri};
use p256::elliptic_curve::rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize, Serializer};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use super::{Result, TransportV2Error};

const VERSION: u8 = 2;
const REQUEST_ID_BYTES: usize = 16;
const METADATA_LENGTH_BYTES: usize = 4;
const MAX_METADATA_BYTES: usize = 128 * 1024;
const MAX_BODY_BYTES: usize = 50 * 1024 * 1024;
const MAX_CREDENTIAL_BYTES: usize = 16 * 1024;
const MAX_METHOD_BYTES: usize = 32;
const MAX_TARGET_BYTES: usize = 16 * 1024;
const MAX_HEADER_COUNT: usize = 64;

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(super) struct RequestId([u8; REQUEST_ID_BYTES]);

impl RequestId {
    pub(super) fn random() -> Result<Self> {
        let mut bytes = [0; REQUEST_ID_BYTES];
        OsRng
            .try_fill_bytes(&mut bytes)
            .map_err(|_| TransportV2Error::RandomnessUnavailable)?;
        Ok(Self(bytes))
    }

    #[cfg(test)]
    pub(super) const fn from_bytes(bytes: [u8; REQUEST_ID_BYTES]) -> Self {
        Self(bytes)
    }

    pub(super) const fn as_bytes(&self) -> &[u8; REQUEST_ID_BYTES] {
        &self.0
    }
}

impl fmt::Debug for RequestId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RequestId(")?;
        formatter.write_str(&hex::encode(self.0))?;
        formatter.write_str(")")
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&hex::encode(self.0))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CredentialKind {
    Bearer,
    ApiKey,
    Resumption,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Credential {
    kind: CredentialKind,
    value: String,
}

impl Credential {
    pub(crate) fn new(kind: CredentialKind, value: String) -> Result<Self> {
        if value.is_empty()
            || value.len() > MAX_CREDENTIAL_BYTES
            || !value.bytes().all(|byte| matches!(byte, 0x21..=0x7e))
        {
            return Err(TransportV2Error::InvalidRequest);
        }
        Ok(Self { kind, value })
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Credential")
            .field("kind", &self.kind)
            .field("value_bytes", &self.value.len())
            .finish()
    }
}

impl Drop for Credential {
    fn drop(&mut self) {
        self.value.zeroize();
    }
}

#[derive(Clone, Eq, PartialEq, Zeroize, ZeroizeOnDrop)]
pub(crate) struct CacheNamespaceRoot([u8; 32]);

impl CacheNamespaceRoot {
    pub(crate) const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl fmt::Debug for CacheNamespaceRoot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CacheNamespaceRoot([REDACTED])")
    }
}

impl Serialize for CacheNamespaceRoot {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let encoded = Zeroizing::new(STANDARD.encode(self.0));
        serializer.serialize_str(encoded.as_str())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LogicalHeader {
    name: String,
    value: String,
}

impl LogicalHeader {
    pub(crate) fn new(name: String, value: String) -> Result<Self> {
        let header = Self { name, value };
        validate_header(&header)?;
        Ok(header)
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn value(&self) -> &str {
        &self.value
    }
}

pub(crate) struct LogicalRequest {
    credential: Option<Credential>,
    cache_namespace_root: Option<CacheNamespaceRoot>,
    method: Method,
    target: String,
    headers: Vec<LogicalHeader>,
    body: Option<Bytes>,
}

impl LogicalRequest {
    pub(crate) fn new(
        credential: Option<Credential>,
        cache_namespace_root: Option<CacheNamespaceRoot>,
        method: Method,
        target: String,
        headers: Vec<LogicalHeader>,
        body: Option<Bytes>,
    ) -> Result<Self> {
        let request = Self {
            credential,
            cache_namespace_root,
            method,
            target,
            headers,
            body,
        };
        request.validate()?;
        Ok(request)
    }

    fn validate(&self) -> Result<()> {
        if self.method.as_str().len() > MAX_METHOD_BYTES {
            return Err(TransportV2Error::InvalidRequest);
        }
        validate_target(&self.target)?;
        validate_headers(&self.headers)?;
        if self.body.as_ref().map_or(0, Bytes::len) > MAX_BODY_BYTES {
            return Err(TransportV2Error::InvalidRequest);
        }
        Ok(())
    }

    pub(super) fn encode(&self) -> Result<Zeroizing<Vec<u8>>> {
        self.validate()?;
        let metadata = RequestMetadata {
            version: VERSION,
            credential: self.credential.clone(),
            cache_namespace_root: self.cache_namespace_root.clone(),
            method: self.method.as_str().to_string(),
            target: self.target.clone(),
            headers: self.headers.clone(),
            body_present: self.body.is_some(),
        };
        let encoded_metadata = Zeroizing::new(
            serde_json::to_vec(&metadata).map_err(|_| TransportV2Error::InvalidRequest)?,
        );
        if encoded_metadata.len() > MAX_METADATA_BYTES {
            return Err(TransportV2Error::InvalidRequest);
        }

        let body = self.body.as_deref().unwrap_or_default();
        let mut encoded = Zeroizing::new(Vec::with_capacity(
            METADATA_LENGTH_BYTES + encoded_metadata.len() + body.len(),
        ));
        encoded.extend_from_slice(&(encoded_metadata.len() as u32).to_be_bytes());
        encoded.extend_from_slice(&encoded_metadata);
        encoded.extend_from_slice(body);
        Ok(encoded)
    }
}

#[derive(Serialize)]
struct RequestMetadata {
    version: u8,
    credential: Option<Credential>,
    cache_namespace_root: Option<CacheNamespaceRoot>,
    method: String,
    target: String,
    headers: Vec<LogicalHeader>,
    body_present: bool,
}

fn validate_target(target: &str) -> Result<()> {
    if target.is_empty()
        || target.len() > MAX_TARGET_BYTES
        || !target.starts_with('/')
        || target.starts_with("//")
        || target.contains(['#', '\\'])
    {
        return Err(TransportV2Error::InvalidRequest);
    }
    let uri = Uri::from_str(target).map_err(|_| TransportV2Error::InvalidRequest)?;
    if uri.scheme().is_some()
        || uri.authority().is_some()
        || uri
            .path_and_query()
            .is_none_or(|path_and_query| path_and_query.as_str() != target)
    {
        return Err(TransportV2Error::InvalidRequest);
    }
    Ok(())
}

fn validate_headers(headers: &[LogicalHeader]) -> Result<()> {
    if headers.len() > MAX_HEADER_COUNT {
        return Err(TransportV2Error::InvalidRequest);
    }
    for header in headers {
        validate_header(header)?;
    }
    Ok(())
}

fn validate_header(header: &LogicalHeader) -> Result<()> {
    let parsed_name = HeaderName::from_bytes(header.name.as_bytes())
        .map_err(|_| TransportV2Error::InvalidRequest)?;
    if parsed_name.as_str() != header.name || is_gateway_controlled_header(parsed_name.as_str()) {
        return Err(TransportV2Error::InvalidRequest);
    }
    HeaderValue::from_str(&header.value).map_err(|_| TransportV2Error::InvalidRequest)?;
    Ok(())
}

fn is_gateway_controlled_header(name: &str) -> bool {
    matches!(
        name,
        "authorization"
            | "proxy-authorization"
            | "cookie"
            | "set-cookie"
            | "host"
            | "content-length"
            | "transfer-encoding"
            | "connection"
            | "keep-alive"
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_uses_bounded_metadata_and_a_raw_body_tail() {
        let body = Bytes::from_static(&[0, 1, 2, 0xfe, 0xff]);
        let request = LogicalRequest::new(
            Some(Credential::new(CredentialKind::Bearer, "signed.token".into()).unwrap()),
            Some(CacheNamespaceRoot::from_bytes([0x22; 32])),
            Method::POST,
            "/v1/chat/completions?trace=1".into(),
            vec![LogicalHeader::new("content-type".into(), "application/json".into()).unwrap()],
            Some(body.clone()),
        )
        .unwrap();
        let encoded = request.encode().unwrap();
        assert!(encoded.ends_with(&body));
        let metadata_length = u32::from_be_bytes(encoded[..4].try_into().unwrap()) as usize;
        let metadata: serde_json::Value =
            serde_json::from_slice(&encoded[4..4 + metadata_length]).unwrap();
        assert_eq!(metadata["version"], 2);
        assert_eq!(metadata["credential"]["kind"], "bearer");
        assert_eq!(metadata["credential"]["value"], "signed.token");
        assert_eq!(
            metadata["cache_namespace_root"],
            STANDARD.encode([0x22; 32])
        );
        assert_eq!(metadata["body_present"], true);
    }

    #[test]
    fn absent_and_present_empty_bodies_are_distinct() {
        let make = |body| {
            LogicalRequest::new(None, None, Method::POST, "/v1/test".into(), vec![], body)
                .unwrap()
                .encode()
                .unwrap()
        };
        let absent = make(None);
        let empty = make(Some(Bytes::new()));
        assert_ne!(absent, empty);
        for (encoded, expected) in [(&absent, false), (&empty, true)] {
            let length = u32::from_be_bytes(encoded[..4].try_into().unwrap()) as usize;
            let metadata: serde_json::Value =
                serde_json::from_slice(&encoded[4..4 + length]).unwrap();
            assert_eq!(metadata["body_present"], expected);
        }
    }

    #[test]
    fn outer_authority_fields_cannot_enter_logical_headers_or_targets() {
        for name in ["authorization", "cookie", "host", "x-session-id"] {
            assert!(LogicalHeader::new(name.into(), "value".into()).is_err());
        }
        for target in ["https://example.com", "//example.com", "/ok#fragment"] {
            assert!(
                LogicalRequest::new(None, None, Method::GET, target.into(), vec![], None,).is_err()
            );
        }
    }

    #[test]
    fn repeated_end_to_end_headers_remain_ordered() {
        let encoded = LogicalRequest::new(
            None,
            None,
            Method::GET,
            "/v1/models".into(),
            vec![
                LogicalHeader::new("x-provider-option".into(), "first".into()).unwrap(),
                LogicalHeader::new("x-provider-option".into(), "second".into()).unwrap(),
            ],
            None,
        )
        .unwrap()
        .encode()
        .unwrap();
        let length = u32::from_be_bytes(encoded[..4].try_into().unwrap()) as usize;
        let metadata: serde_json::Value = serde_json::from_slice(&encoded[4..4 + length]).unwrap();
        assert_eq!(metadata["headers"][0]["value"], "first");
        assert_eq!(metadata["headers"][1]["value"], "second");
    }

    #[test]
    fn request_ids_are_random_full_width_values_without_a_registry() {
        let first = RequestId::random().unwrap();
        let second = RequestId::random().unwrap();
        assert_eq!(first.as_bytes().len(), 16);
        assert_ne!(first, second);
    }
}
