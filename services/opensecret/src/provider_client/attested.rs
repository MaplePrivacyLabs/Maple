//! The attested Tinfoil clients behind the provider transport.
//!
//! Everything that names an SDK type lives here, so the transport above deals
//! only in verified clients, finite failure labels and send outcomes.

use crate::maple_inference::{MaplePolicy, ReleaseNotApproved};
use std::error::Error as StdError;
use std::time::SystemTime;
use tinfoil::verifier::{Error as VerifierError, Layer};
use tinfoil::{EnclaveHttpClient, ErrorKind, RouterHttpClient, Trust};

/// Why attestation failed, as finite labels that are safe to log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct AttestationFailureDiagnostic {
    pub(super) category: &'static str,
    pub(super) reason: &'static str,
}

impl AttestationFailureDiagnostic {
    pub(super) fn is_attestation(&self) -> bool {
        self.category == "attestation"
    }

    pub(super) fn is_network(&self) -> bool {
        self.category == "network"
    }
}

fn find_in_chain<'a, T: StdError + 'static>(error: &'a (dyn StdError + 'static)) -> Option<&'a T> {
    let mut next = Some(error);
    while let Some(error) = next {
        if let Some(found) = error.downcast_ref::<T>() {
            return Some(found);
        }
        next = error.source();
    }
    None
}

pub(super) fn attestation_failure_diagnostic(
    error: &tinfoil::Error,
) -> AttestationFailureDiagnostic {
    // SDK errors can contain arbitrary response text, URLs, and document
    // contents. Log only finite labels derived from typed causes, never their
    // payloads.
    let category = match error.kind() {
        ErrorKind::Configuration => "configuration",
        ErrorKind::Network => "network",
        ErrorKind::Attestation => "attestation",
        _ => "other",
    };
    let reason = if error.is_timeout() {
        "timeout"
    } else if find_in_chain::<ReleaseNotApproved>(error).is_some() {
        "release_not_approved"
    } else if let Some(verifier) = find_in_chain::<VerifierError>(error) {
        match verifier {
            VerifierError::Verification(rejected) => match rejected.layer() {
                Layer::Envelope => "envelope",
                Layer::Provenance => "provenance",
                Layer::Quote => "quote",
                Layer::Policy => "policy",
                _ => "verification",
            },
            VerifierError::Usage(_) => "verifier_usage",
            _ => "verifier",
        }
    } else if let Some(http) = find_in_chain::<reqwest::Error>(error) {
        if http.is_connect() {
            "http_connect"
        } else if http.is_timeout() {
            "http_timeout"
        } else if http.is_status() {
            "http_status"
        } else {
            "http"
        }
    } else {
        category
    };
    AttestationFailureDiagnostic { category, reason }
}

/// What a failed send proves about the request.
#[derive(Debug)]
pub(super) enum ExecuteFailure {
    /// The client refused the request before any I/O. Retrying cannot help.
    Refused(&'static str),
    /// No request bytes reached the enclave: the connection was never
    /// established, or the peer did not hold the attested key. In the TLS
    /// transport mode an attestation error always precedes the request.
    Unsent(String),
    /// The request may have been accepted. It must not be replayed.
    Ambiguous(String),
}

fn classify_execute_error(error: &tinfoil::Error) -> ExecuteFailure {
    match error.kind() {
        ErrorKind::Configuration => ExecuteFailure::Refused("request refused by attested client"),
        ErrorKind::Attestation => ExecuteFailure::Unsent(format!(
            "attestation failed before the request was sent: {}",
            attestation_failure_diagnostic(error).reason
        )),
        // The SDK strips the URL from the transport error it wraps.
        _ if find_in_chain::<reqwest::Error>(error).is_some_and(reqwest::Error::is_connect) => {
            ExecuteFailure::Unsent(error.to_string())
        }
        _ => ExecuteFailure::Ambiguous(error.to_string()),
    }
}

/// A client that exists has been verified. It pins every connection to the
/// attested TLS key and refuses any other origin.
#[derive(Clone)]
pub(super) enum AttestedClient {
    /// Tinfoil's shared router, chosen by discovery.
    Router(RouterHttpClient),
    /// One dedicated guest that the caller names.
    Enclave(EnclaveHttpClient),
}

impl AttestedClient {
    /// Discover and verify Tinfoil's shared router.
    pub(super) async fn connect_router() -> Result<Self, AttestationFailureDiagnostic> {
        RouterHttpClient::connect()
            .await
            .map(Self::Router)
            .map_err(|error| attestation_failure_diagnostic(&error))
    }

    /// Verify the guest at `host` against the approved releases. The verifier
    /// proves a signed release of one repository at a time; the policy check
    /// then runs on this and on every later verification of the same client,
    /// so a key rotation can never move it to an unapproved release.
    pub(super) async fn connect_maple(
        host: &str,
        policy: &MaplePolicy,
    ) -> Result<Self, AttestationFailureDiagnostic> {
        let mut failure = AttestationFailureDiagnostic {
            category: "configuration",
            reason: "no_approved_release",
        };
        for repo in policy.repos() {
            let approved = policy.clone();
            let connected = EnclaveHttpClient::builder(host, Trust::repo(repo))
                .verification_check(move |verified| Ok(approved.check(verified)?))
                .connect()
                .await;
            match connected {
                Ok(client) => return Ok(Self::Enclave(client)),
                Err(error) => failure = attestation_failure_diagnostic(&error),
            }
        }
        Err(failure)
    }

    /// The host of the verified enclave.
    pub(super) fn enclave(&self) -> &str {
        match self {
            Self::Router(client) => client.enclave(),
            Self::Enclave(client) => client.enclave(),
        }
    }

    /// When the verification that requests are bound to stops authorizing
    /// them. `None` once it has expired or the SDK has discarded it, which it
    /// does when the enclave rejects the attested key or a later verification
    /// fails attestation.
    pub(super) fn freshness_expires_at(&self) -> Option<SystemTime> {
        match self {
            Self::Router(client) => client.current_verification(),
            Self::Enclave(client) => client.current_verification(),
        }
        .and_then(|report| report.verified_facts()?.freshness_expires_at())
    }

    /// Whether the current verification still authorizes requests. Evidence
    /// without a deadline never does.
    pub(super) fn is_fresh(&self) -> bool {
        self.freshness_expires_at()
            .is_some_and(|deadline| SystemTime::now() < deadline)
    }

    /// Verify again without sending a request. A network failure keeps the
    /// current verification until its own deadline; an attestation failure
    /// discards it, and the client is then no longer fresh.
    pub(super) async fn reverify(&self) -> Result<(), AttestationFailureDiagnostic> {
        match self {
            Self::Router(client) => client.reverify().await,
            Self::Enclave(client) => client.reverify().await,
        }
        .map(|_| ())
        .map_err(|error| attestation_failure_diagnostic(&error))
    }

    pub(super) async fn execute(
        &self,
        request: reqwest::Request,
    ) -> Result<reqwest::Response, ExecuteFailure> {
        match self {
            Self::Router(client) => client.execute(request).await,
            Self::Enclave(client) => client.execute(request).await,
        }
        .map_err(|error| classify_execute_error(&error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORBIDDEN: [&str; 4] = [
        "Bearer",
        "diagnostic-secret",
        "private.invalid",
        "forged_log",
    ];

    fn assert_no_payload(rendered: &str) {
        for forbidden in FORBIDDEN {
            assert!(!rendered.contains(forbidden), "{rendered}");
        }
    }

    // The SDK's errors cannot be constructed from outside it, so each case
    // provokes a real one. None of them leaves the process.
    #[tokio::test]
    async fn configuration_failures_are_labelled_without_payloads() {
        let sensitive = "Bearer diagnostic-secret https://private.invalid/?token=diagnostic-secret\nforged_log=true";
        let error = EnclaveHttpClient::builder(sensitive, Trust::repo("owner/name"))
            .connect()
            .await
            .expect_err("a URL with credentials is not a host");
        let diagnostic = attestation_failure_diagnostic(&error);
        assert_eq!(
            diagnostic,
            AttestationFailureDiagnostic {
                category: "configuration",
                reason: "configuration",
            }
        );
        assert!(!diagnostic.is_attestation() && !diagnostic.is_network());
        assert_no_payload(&format!("{diagnostic:?}"));
        match classify_execute_error(&error) {
            ExecuteFailure::Refused(label) => assert_no_payload(label),
            other => panic!("a configuration error must not be retried: {other:?}"),
        }
    }

    #[tokio::test]
    async fn unreachable_guest_is_a_network_failure_that_proves_nothing_was_sent() {
        crate::http_client::install_ring_crypto_provider().unwrap();
        // Bind and drop to get a loopback port that refuses connections.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let policy =
            crate::maple_inference::endpoint(crate::provider_registry::MapleInstanceId::I01)
                .direct_policy()
                .unwrap()
                .unwrap();
        let diagnostic =
            match AttestedClient::connect_maple(&format!("127.0.0.1:{port}"), &policy).await {
                Ok(_) => panic!("nothing listens on the port"),
                Err(diagnostic) => diagnostic,
            };
        assert_eq!(diagnostic.category, "network");
        assert_eq!(diagnostic.reason, "http_connect");
        assert!(diagnostic.is_network() && !diagnostic.is_attestation());
    }

    #[tokio::test]
    async fn a_policy_without_releases_never_reaches_the_network() {
        // `direct_policy` returns no policy for an unconfigured guest; this
        // guards the constructor itself.
        let empty = crate::maple_inference::empty_policy_for_test();
        assert_eq!(
            AttestedClient::connect_maple("i01.tfinf.maple.ai", &empty)
                .await
                .err(),
            Some(AttestationFailureDiagnostic {
                category: "configuration",
                reason: "no_approved_release",
            })
        );
    }
}
