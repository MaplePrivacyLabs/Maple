use crate::pcr::Pcr0Environment;
use sha2::{Digest, Sha256};
use sigstore_trust_root::TrustedRoot;
use sigstore_types::DerPublicKey;

const DEVELOPMENT_APPROVAL_KEY_FINGERPRINT: &str =
    "d5bd5386930a63f926424148b83220a5f44141b7e05571d7468360c531c86c43";
const EMBEDDED_TRUSTED_ROOT_JSON: &str = include_str!("../../assets/release/trusted_root.json");
const DEVELOPMENT_APPROVAL_KEY_PEM: &str =
    include_str!("../../assets/release/development/approval-key.pem");

pub(super) fn embedded_trusted_root() -> Result<TrustedRoot, String> {
    TrustedRoot::from_json(EMBEDDED_TRUSTED_ROOT_JSON)
        .map_err(|_| "invalid embedded Sigstore trusted root".to_owned())
}

pub(super) fn approval_key(environment: Pcr0Environment) -> Result<Option<DerPublicKey>, String> {
    let is_development = environment == Pcr0Environment::Development;
    if !is_development {
        // Production reports unavailable release verification; it never falls
        // back to the Development approval key or to relaxed verification.
        return Ok(None);
    }

    let key = DerPublicKey::from_pem(DEVELOPMENT_APPROVAL_KEY_PEM)
        .map_err(|_| "invalid embedded development approval key".to_owned())?;
    let actual_fingerprint = hex::encode(Sha256::digest(key.as_bytes()));
    let has_expected_fingerprint = actual_fingerprint == DEVELOPMENT_APPROVAL_KEY_FINGERPRINT;
    if !has_expected_fingerprint {
        return Err("development approval key fingerprint mismatch".to_owned());
    }
    Ok(Some(key))
}
