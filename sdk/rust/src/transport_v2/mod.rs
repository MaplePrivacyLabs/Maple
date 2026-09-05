//! Private, dormant client engine for the second attested transport.
//!
//! This module owns only the wire protocol. The existing public SDK remains on
//! transport v1 until a later stack layer deliberately adapts it to this API.

pub(crate) mod client;
mod crypto;
pub(crate) mod envelope;
pub(crate) mod framing;

use thiserror::Error;

pub(crate) const ROUTING_KEY_HEADER: &str = "x-opensecret-routing-key";

/// An unauthenticated outer hint that permits one managed-client recovery.
/// It is not proof that the original request was never dispatched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionRecoveryHint {
    SessionNotFound,
    RequestDecryptionFailed,
}

/// Stable, redacted failures from the private transport engine.
#[derive(Debug, Error)]
pub(crate) enum TransportV2Error {
    #[error("transport-v2 configuration is invalid")]
    InvalidConfiguration,
    #[error("secure randomness is unavailable")]
    RandomnessUnavailable,
    #[error("transport-v2 attestation was rejected")]
    AttestationRejected,
    #[error("transport-v2 session response is invalid")]
    InvalidSessionResponse,
    #[error("transport-v2 session has expired")]
    SessionExpired,
    #[error("transport-v2 key derivation failed")]
    KeyDerivation,
    #[error("transport-v2 key exchange was non-contributory")]
    NonContributoryKey,
    #[error("transport-v2 request is invalid")]
    InvalidRequest,
    #[error("transport-v2 request encryption failed")]
    Encryption,
    #[error("transport-v2 response authentication failed")]
    Authentication,
    #[error("transport-v2 outer response is untrusted")]
    UntrustedOuterResponse,
    #[error("transport-v2 outer response requested session recovery: {0:?}")]
    SessionRecoveryHint(SessionRecoveryHint),
    #[error("transport-v2 HTTP exchange failed: {0}")]
    Http(#[source] reqwest::Error),
    #[error("transport-v2 response frame is invalid")]
    InvalidFrame,
    #[error("transport-v2 response record is invalid")]
    InvalidRecord,
    #[error("transport-v2 response record was transplanted or reordered")]
    InvalidSequence,
    #[error("transport-v2 response ended without authenticated finality")]
    TruncatedResponse,
    #[error("transport-v2 response contained bytes after its terminal record")]
    PostTerminalData,
}

pub(crate) type Result<T> = std::result::Result<T, TransportV2Error>;
