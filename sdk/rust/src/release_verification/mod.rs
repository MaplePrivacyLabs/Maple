mod bundle;
mod trust;

use crate::pcr::Pcr0Environment;
use bundle::{
    verify_release_bundle, MeasurementTuple, ReleasePolicy, VerificationEvidence,
    VerificationFailure,
};
use sigstore_trust_root::TrustedRoot;
use sigstore_types::DerPublicKey;

/// Pure, synchronous release-bundle verification against locally selected trust.
///
/// This primitive has no network, cache, or session behavior. Its caller owns
/// scheduling and any observation lifecycle.
pub(super) struct ReleaseVerifier {
    environment: Pcr0Environment,
    approval_key: Option<DerPublicKey>,
    trusted_root: TrustedRoot,
}

impl ReleaseVerifier {
    pub(super) fn new(environment: Pcr0Environment) -> Result<Self, String> {
        Ok(Self {
            environment,
            approval_key: trust::approval_key(environment)?,
            trusted_root: trust::embedded_trusted_root()?,
        })
    }

    pub(in crate::release_verification) fn verify_bundle(
        &self,
        bytes: &[u8],
        measurements: MeasurementTuple,
    ) -> Result<VerificationEvidence, VerificationFailure> {
        let approval_key = self
            .approval_key
            .as_ref()
            .ok_or_else(|| VerificationFailure::policy("approval-key-not-configured"))?;
        verify_release_bundle(
            bytes,
            approval_key,
            &self.trusted_root,
            ReleasePolicy::new(self.environment, measurements),
        )
    }
}

#[cfg(test)]
mod corpus_tests;
#[cfg(test)]
mod tests;
