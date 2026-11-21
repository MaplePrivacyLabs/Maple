use crate::pcr::Pcr0Environment;
use async_trait::async_trait;
use reqwest::Url;
use sha2::{Digest, Sha256};
use sigstore_trust_root::{TrustedRoot, TufConfig};
use sigstore_types::DerPublicKey;

const DEVELOPMENT_APPROVAL_KEY_FINGERPRINT: &str =
    "d5bd5386930a63f926424148b83220a5f44141b7e05571d7468360c531c86c43";
const DEVELOPMENT_BUNDLE_HOST: &str = "https://attestations-dev.maple.ai";
const PRODUCTION_BUNDLE_HOST: &str = "https://attestations.maple.ai";
const EMBEDDED_TRUSTED_ROOT_JSON: &str = include_str!("../../assets/release/trusted_root.json");
const DEVELOPMENT_APPROVAL_KEY_PEM: &str =
    include_str!("../../assets/release/development/approval-key.pem");

#[async_trait]
pub(super) trait RootRefresher: Send + Sync {
    async fn refresh(&self) -> Result<TrustedRoot, ()>;
}

pub(super) struct SigstoreTufRefresher;

#[async_trait]
impl RootRefresher for SigstoreTufRefresher {
    async fn refresh(&self) -> Result<TrustedRoot, ()> {
        // Use an explicitly pinned Sigstore TUF root and no ambient disk cache.
        // The in-memory root is the only root reused by this verifier.
        let config = production_tuf_config().map_err(|_| ())?;
        TrustedRoot::from_tuf(config).await.map_err(|_| ())
    }
}

pub(super) fn tuf_http_client() -> Result<sigstore_trust_root::reqwest::Client, String> {
    sigstore_trust_root::reqwest::Client::builder()
        .redirect(sigstore_trust_root::reqwest::redirect::Policy::none())
        .timeout(super::DEFAULT_ROOT_REFRESH_TIMEOUT)
        .build()
        .map_err(|_| "cannot construct TUF refresh HTTP client".to_owned())
}

fn production_tuf_config() -> Result<TufConfig, String> {
    Ok(TufConfig::production()
        .without_cache()
        .with_http_client(tuf_http_client()?))
}

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

pub(super) fn bundle_host(environment: Pcr0Environment) -> Url {
    let is_development = environment == Pcr0Environment::Development;
    let host = if is_development {
        DEVELOPMENT_BUNDLE_HOST
    } else {
        PRODUCTION_BUNDLE_HOST
    };
    Url::parse(host).expect("release bundle hosts are valid HTTPS URLs")
}

#[cfg(test)]
mod tests {
    use super::{DEVELOPMENT_APPROVAL_KEY_PEM, EMBEDDED_TRUSTED_ROOT_JSON};

    const REVIEWED_DEVELOPMENT_APPROVAL_KEY: &[u8] =
        include_bytes!("../../../../scripts/ci/opensecret-release/dev-signing-key.pem");
    const REVIEWED_TRUSTED_ROOT: &[u8] =
        include_bytes!("../../../../scripts/ci/opensecret-release/trusted_root.json");

    #[test]
    fn embedded_trust_inputs_match_release_pipeline_bytes() {
        assert_eq!(
            DEVELOPMENT_APPROVAL_KEY_PEM.as_bytes(),
            REVIEWED_DEVELOPMENT_APPROVAL_KEY
        );
        assert_eq!(EMBEDDED_TRUSTED_ROOT_JSON.as_bytes(), REVIEWED_TRUSTED_ROOT);
    }
}
