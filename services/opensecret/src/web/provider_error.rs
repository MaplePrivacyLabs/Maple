//! Client-facing completion errors. Provider text and identifiers never enter
//! this type: even the lossy diagnostic is mapped to a finite local vocabulary.

use crate::inference::{AttemptFailure, AttemptFailureKind};
use axum::http::StatusCode;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Detail {
    InvalidRequest,
    InvalidParameter,
    UnsupportedParameter,
    InvalidMessages,
    ContextLimit,
    PayloadTooLarge,
    ProviderError,
    Timeout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicProviderError {
    status: StatusCode,
    detail: Detail,
}

impl PublicProviderError {
    pub(crate) fn from_failure(failure: &AttemptFailure) -> Option<Self> {
        let (status, detail) = match failure.kind {
            AttemptFailureKind::HttpStatus => match failure.status {
                Some(status @ (400 | 422)) => {
                    let detail = match failure
                        .upstream_diagnostic
                        .as_deref()
                        .and_then(|diagnostic| diagnostic.summary)
                    {
                        Some("upstream rejected a parameter value") => Detail::InvalidParameter,
                        Some("upstream does not support a request parameter") => {
                            Detail::UnsupportedParameter
                        }
                        Some("upstream rejected the message structure") => Detail::InvalidMessages,
                        Some("upstream context or token limit exceeded") => Detail::ContextLimit,
                        _ => Detail::InvalidRequest,
                    };
                    let status = if status == 400 {
                        StatusCode::BAD_REQUEST
                    } else {
                        StatusCode::UNPROCESSABLE_ENTITY
                    };
                    (status, detail)
                }
                Some(413) => (StatusCode::PAYLOAD_TOO_LARGE, Detail::PayloadTooLarge),
                Some(408 | 504) => (StatusCode::GATEWAY_TIMEOUT, Detail::Timeout),
                // In particular, an upstream 401/403 is not a Maple login or
                // entitlement failure, and a 404 is not a missing user resource.
                // The HTTP status wins over contradictory diagnostic labels.
                _ => (StatusCode::BAD_GATEWAY, Detail::ProviderError),
            },
            AttemptFailureKind::ResponseStartTimeout | AttemptFailureKind::StreamTimeout => {
                (StatusCode::GATEWAY_TIMEOUT, Detail::Timeout)
            }
            AttemptFailureKind::Connect
            | AttemptFailureKind::Transport
            | AttemptFailureKind::ResponseBody
            | AttemptFailureKind::InvalidResponse
            | AttemptFailureKind::UpstreamResponseError
            | AttemptFailureKind::UpstreamStreamError
            | AttemptFailureKind::UnexpectedEof => (StatusCode::BAD_GATEWAY, Detail::ProviderError),
            // These retain their existing, separate contracts. In particular,
            // a caller losing interest does not prove an upstream failure.
            AttemptFailureKind::CapacityRejected
            | AttemptFailureKind::ProviderUnavailable
            | AttemptFailureKind::RequestBuild
            | AttemptFailureKind::ConsumerDropped => return None,
        };
        Some(Self { status, detail })
    }

    pub(crate) fn status(self) -> StatusCode {
        self.status
    }

    pub(crate) fn code(self) -> &'static str {
        match self.detail {
            Detail::InvalidRequest => "upstream_invalid_request",
            Detail::InvalidParameter => "upstream_invalid_parameter",
            Detail::UnsupportedParameter => "upstream_unsupported_parameter",
            Detail::InvalidMessages => "upstream_invalid_messages",
            Detail::ContextLimit => "upstream_context_limit",
            Detail::PayloadTooLarge => "upstream_payload_too_large",
            Detail::ProviderError => "upstream_provider_error",
            Detail::Timeout => "upstream_timeout",
        }
    }

    pub(crate) fn message(self) -> &'static str {
        match self.detail {
            Detail::InvalidRequest => "Upstream provider rejected the request. Check that the message format and parameters are supported by this model.",
            Detail::InvalidParameter => "Upstream provider rejected a parameter value. Check the parameters supported by this model.",
            Detail::UnsupportedParameter => "Upstream provider does not support a request parameter. Check the parameters supported by this model.",
            Detail::InvalidMessages => "Upstream provider rejected the message structure. Check the message roles and ordering supported by this model.",
            Detail::ContextLimit => "Upstream provider rejected the context or token limit. Reduce the input or requested output tokens.",
            Detail::PayloadTooLarge => "Upstream provider rejected the request size. Reduce the request payload.",
            Detail::ProviderError => "Upstream provider could not complete the request.",
            Detail::Timeout => "Upstream provider timed out while processing the request.",
        }
    }
}

impl fmt::Display for PublicProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference::{AttemptStage, ReplaySafety};

    #[test]
    fn routing_build_and_consumer_outcomes_keep_their_own_contracts() {
        for kind in [
            AttemptFailureKind::CapacityRejected,
            AttemptFailureKind::ProviderUnavailable,
            AttemptFailureKind::RequestBuild,
            AttemptFailureKind::ConsumerDropped,
        ] {
            let failure = AttemptFailure::new(
                kind,
                AttemptStage::BeforeSend,
                ReplaySafety::ProvenPreAcceptance,
            );
            assert!(PublicProviderError::from_failure(&failure).is_none());
        }
    }

    #[test]
    fn raw_payload_codes_and_identifiers_cannot_become_public_details() {
        let failure = AttemptFailure::new(
            AttemptFailureKind::UpstreamStreamError,
            AttemptStage::Stream,
            ReplaySafety::NotProvenPreAcceptance,
        )
        .with_upstream_code(Some("private-prompt-canary".into()))
        .with_upstream_response(200, None, Some("private-request-canary".into()));
        let error = PublicProviderError::from_failure(&failure).unwrap();
        assert_eq!(error.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(error.code(), "upstream_provider_error");
        assert_eq!(
            error.message(),
            "Upstream provider could not complete the request."
        );
        assert!(!format!("{error:?} {error}").contains("private-"));
    }
}
