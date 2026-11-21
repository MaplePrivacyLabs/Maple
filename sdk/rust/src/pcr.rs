use crate::{error::Error, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use reqwest::{redirect::Policy, Client, Url};
use ring::signature;
use serde::Deserialize;
use std::collections::HashSet;
use std::time::Duration;

const PCR0_HEX_LEN: usize = 96;
const PCR0_BYTES_LEN: usize = 48;
const MAX_REMOTE_HISTORY_BYTES: usize = 1024 * 1024;
const MAX_REMOTE_HISTORY_ENTRIES: usize = 2048;
const MAX_REMOTE_HISTORY_URLS: usize = 4;
const MAX_REMOTE_HISTORY_URL_BYTES: usize = 2048;
const REMOTE_HISTORY_TIMEOUT: Duration = Duration::from_secs(5);

/// OpenSecret's P-384 PCR-history verification key in SPKI DER form.
///
/// This key is the trust root for remote history entries. Replacing a history
/// URL cannot expand trust without a signature made by the corresponding
/// private key.
const PCR_HISTORY_VERIFICATION_KEY_B64: &str =
    "MHYwEAYHKoZIzj0CAQYFK4EEACIDYgAEHiUY9kFWK1GqBGzczohhwEwElXzgWLDZa9R6wBx3JOBocgSt9+UIzZlJbPDjYeGBfDUXh7Z62BG2vVsh2NgclLB5S7A2ucBBtb1wd8vSQHP8jpdPhZX1slauPgbnROIP";

pub const OFFICIAL_PRODUCTION_PCR_HISTORY_URL: &str =
    "https://raw.githubusercontent.com/MaplePrivacyLabs/Maple/master/services/opensecret/pcrProdHistory.json";
pub const OFFICIAL_DEVELOPMENT_PCR_HISTORY_URL: &str =
    "https://raw.githubusercontent.com/MaplePrivacyLabs/Maple/master/services/opensecret/pcrDevHistory.json";

const OFFICIAL_PRODUCTION_PCR0S: &[&str] = &[
    // Approved 2026-10-11 (services/opensecret signed history)
    "21d27bd19cdd02632485a6e8874753e42cf7f098f1335bb62a9c23e7f64da9100c02dba582da17ac68052fa6be16b7a9",
    // Approved 2026-10-09 (services/opensecret signed history; rollback)
    "643b3c293a4eac11ec14ab405d2405e46cb714263a12da6c62f485a0fa395cb3c3d5ef07b18d39a1f9715f25e7943b66",
];

const OFFICIAL_DEVELOPMENT_PCR0S: &[&str] = &[
    // Approved 2026-10-11 (services/opensecret signed history)
    "a95956c3fb9457e509be6d614ffefcddf98fe594f77bb6a56891fc1f5bdec41da2add13db22e0908363c8d0929c04def",
    // Approved 2026-10-09 (services/opensecret signed history; rollback)
    "e573a3ddc8a27f387e24cfb02a8fa594fe782674de6ee1fa9385126b9f8ec38f4a236e91c371bdd2b9444623c1c1cc34",
];

/// OpenSecret deployment environment whose PCR0 trust roots should be used.
///
/// Production is the fail-closed default. Development must be selected
/// explicitly by clients that connect to an OpenSecret development enclave.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Pcr0Environment {
    #[default]
    Production,
    Development,
}

impl Pcr0Environment {
    pub(crate) const fn release_name(self) -> &'static str {
        match self {
            Self::Production => "production",
            Self::Development => "development",
        }
    }
}

/// PCR0 deployment-identity policy enforced after Nitro document validation.
///
/// The default policy trusts OpenSecret's pinned production PCR0 values and
/// falls back to the signed official production history. Use
/// [`Self::official_for`] to explicitly select development trust roots, or
/// [`Self::from_static_allowlist`] for a custom deployment that must not use
/// remote history.
#[derive(Debug, Clone)]
pub struct Pcr0TrustPolicy {
    trusted_pcr0s: HashSet<String>,
    remote_history_urls: Vec<Url>,
    release_environment: Pcr0Environment,
}

impl Pcr0TrustPolicy {
    /// Return the official production policy used by default.
    pub fn official() -> Self {
        Self::official_for(Pcr0Environment::Production)
    }

    /// Return the official policy for one explicit OpenSecret environment.
    ///
    /// The selected policy contains only that environment's pinned PCR0 roots
    /// and signed history URL. It never falls back to the other environment.
    pub fn official_for(environment: Pcr0Environment) -> Self {
        let (pcr0s, remote_history_url) = match environment {
            Pcr0Environment::Production => (
                OFFICIAL_PRODUCTION_PCR0S,
                OFFICIAL_PRODUCTION_PCR_HISTORY_URL,
            ),
            Pcr0Environment::Development => (
                OFFICIAL_DEVELOPMENT_PCR0S,
                OFFICIAL_DEVELOPMENT_PCR_HISTORY_URL,
            ),
        };
        let trusted_pcr0s = pcr0s.iter().map(|pcr0| (*pcr0).to_string()).collect();
        let remote_history_urls =
            vec![Url::parse(remote_history_url).expect("official PCR history URL must be valid")];

        Self {
            trusted_pcr0s,
            remote_history_urls,
            release_environment: environment,
        }
    }

    /// Build a remote-disabled policy containing only caller-supplied PCR0s.
    pub fn from_static_allowlist<I, S>(pcr0s: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut policy = Self {
            trusted_pcr0s: HashSet::new(),
            remote_history_urls: Vec::new(),
            release_environment: Pcr0Environment::Production,
        };
        policy.add_pcr0s(pcr0s)?;
        if policy.trusted_pcr0s.is_empty() {
            return Err(Error::Configuration(
                "PCR0 static allowlist must not be empty".to_string(),
            ));
        }
        Ok(policy)
    }

    /// Add caller-supplied PCR0 values to this policy.
    pub fn with_additional_pcr0s<I, S>(mut self, pcr0s: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.add_pcr0s(pcr0s)?;
        Ok(self)
    }

    /// Disable signed remote history and retain only this policy's static set.
    pub fn without_remote_history(mut self) -> Self {
        self.remote_history_urls.clear();
        self
    }

    /// Replace the default remote history locations.
    ///
    /// Every accepted entry must still verify against OpenSecret's hardcoded
    /// signing key. HTTPS is required except for an exact loopback host, which
    /// is allowed to support deterministic local testing and signed mirrors.
    pub fn with_remote_history_urls<I, S>(mut self, urls: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let parsed = urls
            .into_iter()
            .map(|url| parse_remote_history_url(url.as_ref()))
            .collect::<Result<Vec<_>>>()?;
        if parsed.is_empty() || parsed.len() > MAX_REMOTE_HISTORY_URLS {
            return Err(Error::Configuration(format!(
                "PCR history requires between 1 and {MAX_REMOTE_HISTORY_URLS} URLs"
            )));
        }
        self.remote_history_urls = parsed;
        Ok(self)
    }

    fn add_pcr0s<I, S>(&mut self, pcr0s: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for pcr0 in pcr0s {
            let pcr0 = pcr0.as_ref();
            validate_pcr0_hex(pcr0)?;
            self.trusted_pcr0s.insert(pcr0.to_string());
        }
        Ok(())
    }

    pub(crate) fn release_environment(&self) -> Pcr0Environment {
        self.release_environment
    }

    pub(crate) async fn verify_pcr0(&self, pcr0: &[u8]) -> Result<()> {
        if pcr0.len() != PCR0_BYTES_LEN {
            return Err(Error::AttestationVerificationFailed(format!(
                "PCR0 must be {PCR0_BYTES_LEN} bytes"
            )));
        }
        if pcr0.iter().all(|byte| *byte == 0) {
            return Err(Error::AttestationVerificationFailed(
                "PCR0 must not be all zero".to_string(),
            ));
        }
        let pcr0_hex = hex::encode(pcr0);
        if self.trusted_pcr0s.contains(&pcr0_hex) {
            return Ok(());
        }

        // URL validation applies to the complete network request. Do not let a
        // valid HTTPS history location redirect to HTTP, loopback, or another
        // destination that has not passed `parse_remote_history_url`.
        let history_client = Client::builder().redirect(Policy::none()).build()?;
        for url in &self.remote_history_urls {
            let history = match fetch_remote_history(&history_client, url).await {
                Ok(history) => history,
                Err(_) => continue,
            };
            if history
                .iter()
                .any(|entry| entry.pcr0 == pcr0_hex && entry.has_valid_signature())
            {
                return Ok(());
            }
        }

        Err(Error::AttestationVerificationFailed(
            "PCR0 is not approved by the configured trust policy".to_string(),
        ))
    }
}

impl Default for Pcr0TrustPolicy {
    fn default() -> Self {
        Self::official()
    }
}

#[derive(Debug, Deserialize)]
struct PcrHistoryEntry {
    #[serde(rename = "PCR0")]
    pcr0: String,
    #[serde(rename = "PCR1")]
    pcr1: String,
    #[serde(rename = "PCR2")]
    pcr2: String,
    timestamp: u64,
    signature: String,
}

impl PcrHistoryEntry {
    fn validate(&self) -> Result<()> {
        validate_pcr0_hex(&self.pcr0)?;
        validate_pcr0_hex(&self.pcr1)?;
        validate_pcr0_hex(&self.pcr2)?;
        if self.timestamp == 0 {
            return Err(Error::AttestationVerificationFailed(
                "PCR history timestamp must be nonzero".to_string(),
            ));
        }
        let signature = BASE64.decode(&self.signature)?;
        if signature.len() != 96 {
            return Err(Error::AttestationVerificationFailed(
                "PCR history signature must be 96 bytes".to_string(),
            ));
        }
        Ok(())
    }

    fn has_valid_signature(&self) -> bool {
        let Ok(signature_bytes) = BASE64.decode(&self.signature) else {
            return false;
        };
        let Ok(spki) = BASE64.decode(PCR_HISTORY_VERIFICATION_KEY_B64) else {
            return false;
        };
        let Some(public_key) = spki.get(spki.len().saturating_sub(97)..) else {
            return false;
        };
        if public_key.first() != Some(&0x04) {
            return false;
        }

        signature::UnparsedPublicKey::new(&signature::ECDSA_P384_SHA384_FIXED, public_key)
            .verify(self.pcr0.as_bytes(), &signature_bytes)
            .is_ok()
    }
}

async fn fetch_remote_history(client: &Client, url: &Url) -> Result<Vec<PcrHistoryEntry>> {
    tokio::time::timeout(
        REMOTE_HISTORY_TIMEOUT,
        fetch_remote_history_inner(client, url),
    )
    .await
    .map_err(|_| {
        Error::AttestationVerificationFailed("PCR history request timed out".to_string())
    })?
}

async fn fetch_remote_history_inner(client: &Client, url: &Url) -> Result<Vec<PcrHistoryEntry>> {
    let mut response = client.get(url.clone()).send().await?;
    if !response.status().is_success() {
        return Err(Error::AttestationVerificationFailed(
            "PCR history request failed".to_string(),
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_REMOTE_HISTORY_BYTES as u64)
    {
        return Err(Error::AttestationVerificationFailed(
            "PCR history response is too large".to_string(),
        ));
    }

    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > MAX_REMOTE_HISTORY_BYTES {
            return Err(Error::AttestationVerificationFailed(
                "PCR history response is too large".to_string(),
            ));
        }
        body.extend_from_slice(&chunk);
    }

    let entries: Vec<PcrHistoryEntry> = serde_json::from_slice(&body)?;
    if entries.is_empty() || entries.len() > MAX_REMOTE_HISTORY_ENTRIES {
        return Err(Error::AttestationVerificationFailed(format!(
            "PCR history requires between 1 and {MAX_REMOTE_HISTORY_ENTRIES} entries"
        )));
    }
    for entry in &entries {
        entry.validate()?;
    }
    Ok(entries)
}

fn validate_pcr0_hex(pcr0: &str) -> Result<()> {
    if pcr0.len() != PCR0_HEX_LEN
        || !pcr0
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::Configuration(
            "PCR0 values must be 96 lowercase hexadecimal characters".to_string(),
        ));
    }
    if pcr0.bytes().all(|byte| byte == b'0') {
        return Err(Error::Configuration(
            "PCR0 values must not be all zero".to_string(),
        ));
    }
    Ok(())
}

fn parse_remote_history_url(value: &str) -> Result<Url> {
    if value.len() > MAX_REMOTE_HISTORY_URL_BYTES {
        return Err(Error::Configuration(
            "PCR history URL is too long".to_string(),
        ));
    }
    let url = Url::parse(value)
        .map_err(|error| Error::Configuration(format!("Invalid PCR history URL: {error}")))?;
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(Error::Configuration(
            "PCR history URL must not contain credentials or a fragment".to_string(),
        ));
    }
    let is_loopback = url.host_str().is_some_and(|host| {
        let host = host.trim_end_matches('.');
        let address_host = host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(host);
        host.eq_ignore_ascii_case("localhost")
            || address_host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    });
    if url.scheme() != "https" && !(url.scheme() == "http" && is_loopback) {
        return Err(Error::Configuration(
            "PCR history URL must use HTTPS (HTTP is allowed only for loopback)".to_string(),
        ));
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::{matchers::path, Mock, MockServer, ResponseTemplate};

    const SIGNED_PCR0: &str =
        "3637534c33a8bafc5034d5763e441a481f161bbbe888e375ce14b016c7497dc4e550afe866bd8e65969b409d54766481";
    const SIGNED_PCR0_SIGNATURE: &str =
        "GZTXC0Xt0+yAaAatmMUd37pUJpF0nRAOj3Df9qxDOvDvRkiTF8UbGlzlL4kIOi/nd7dXAaEqYnY7OlpyngHBED2CSTpRRwV0xGo109epfqUKWWudrFaXpMsJ+GRKJLFO";
    const UNKNOWN_PCR0: &str = concat!(
        "1111111111111111",
        "1111111111111111",
        "1111111111111111",
        "1111111111111111",
        "1111111111111111",
        "1111111111111111",
    );
    const ALL_ZERO_PCR0: &str = concat!(
        "0000000000000000",
        "0000000000000000",
        "0000000000000000",
        "0000000000000000",
        "0000000000000000",
        "0000000000000000",
    );

    fn pcr_bytes(value: &str) -> Vec<u8> {
        hex::decode(value).unwrap()
    }

    fn history(signature: &str) -> serde_json::Value {
        json!([{
            "PCR0": SIGNED_PCR0,
            "PCR1": "e45de6f4e9809176f6adc68df999f87f32a602361247d5819d1edf11ac5a403cfbb609943705844251af85713a17c83a",
            "PCR2": "fe0a6f7c29c7c4999571869f880b6d5086b377deaaf359e19ae824edacd6a9d90247b793a5f2d73c0e74e2f9630aeb4a",
            "timestamp": 1743710235_u64,
            "signature": signature,
            "futureMetadata": { "release": "ignored" },
        }])
    }

    #[tokio::test]
    async fn official_policy_defaults_to_production_only() {
        let policy = Pcr0TrustPolicy::official();
        let default_policy = Pcr0TrustPolicy::default();
        let production_history =
            Url::parse(OFFICIAL_PRODUCTION_PCR_HISTORY_URL).expect("valid production URL");

        for candidate in [&policy, &default_policy] {
            assert_eq!(
                candidate.remote_history_urls,
                vec![production_history.clone()]
            );
            assert_eq!(
                candidate.trusted_pcr0s.len(),
                OFFICIAL_PRODUCTION_PCR0S.len()
            );
            assert!(OFFICIAL_PRODUCTION_PCR0S
                .iter()
                .all(|pcr0| candidate.trusted_pcr0s.contains(*pcr0)));
            assert!(OFFICIAL_DEVELOPMENT_PCR0S
                .iter()
                .all(|pcr0| !candidate.trusted_pcr0s.contains(*pcr0)));
        }

        let static_policy = policy.without_remote_history();
        static_policy
            .verify_pcr0(&pcr_bytes(OFFICIAL_PRODUCTION_PCR0S[0]))
            .await
            .unwrap();
        assert!(static_policy
            .verify_pcr0(&pcr_bytes(OFFICIAL_DEVELOPMENT_PCR0S[0]))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn development_policy_excludes_production_trust() {
        let policy = Pcr0TrustPolicy::official_for(Pcr0Environment::Development);
        assert_eq!(
            policy.remote_history_urls,
            vec![Url::parse(OFFICIAL_DEVELOPMENT_PCR_HISTORY_URL).expect("valid development URL")]
        );
        assert_eq!(policy.trusted_pcr0s.len(), OFFICIAL_DEVELOPMENT_PCR0S.len());
        assert!(OFFICIAL_DEVELOPMENT_PCR0S
            .iter()
            .all(|pcr0| policy.trusted_pcr0s.contains(*pcr0)));
        assert!(OFFICIAL_PRODUCTION_PCR0S
            .iter()
            .all(|pcr0| !policy.trusted_pcr0s.contains(*pcr0)));

        let static_policy = policy.without_remote_history();
        static_policy
            .verify_pcr0(&pcr_bytes(OFFICIAL_DEVELOPMENT_PCR0S[0]))
            .await
            .unwrap();
        assert!(static_policy
            .verify_pcr0(&pcr_bytes(OFFICIAL_PRODUCTION_PCR0S[0]))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn older_approvals_require_history_rather_than_bundled_trust() {
        let production = Pcr0TrustPolicy::official().without_remote_history();
        let development =
            Pcr0TrustPolicy::official_for(Pcr0Environment::Development).without_remote_history();
        assert!(production
            .verify_pcr0(&pcr_bytes("eeddbb58f57c38894d6d5af5e575fbe791c5bf3bbcfb5df8da8cfcf0c2e1da1913108e6a762112444740b88c163d7f4b"))
            .await
            .is_err());
        assert!(development
            .verify_pcr0(&pcr_bytes("62c0407056217a4c10764ed9045694c29fa93255d3cc04c2f989cdd9a1f8050c8b169714c71f1118ebce2fcc9951d1a9"))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn all_zero_pcr0_is_never_trusted() {
        assert!(Pcr0TrustPolicy::from_static_allowlist([ALL_ZERO_PCR0]).is_err());

        let policy = Pcr0TrustPolicy::official().without_remote_history();
        let error = policy.verify_pcr0(&[0; PCR0_BYTES_LEN]).await.unwrap_err();
        assert!(matches!(error, Error::AttestationVerificationFailed(_)));
    }

    #[tokio::test]
    async fn static_allowlist_approves_only_exact_pcr0() {
        let policy = Pcr0TrustPolicy::from_static_allowlist([SIGNED_PCR0]).unwrap();
        policy.verify_pcr0(&pcr_bytes(SIGNED_PCR0)).await.unwrap();

        let error = policy
            .verify_pcr0(&[0x42; PCR0_BYTES_LEN])
            .await
            .unwrap_err();
        assert!(matches!(error, Error::AttestationVerificationFailed(_)));
    }

    #[tokio::test]
    async fn signed_remote_history_approves_matching_pcr0() {
        let server = MockServer::start().await;
        Mock::given(path("/history.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(history(SIGNED_PCR0_SIGNATURE)))
            .expect(1)
            .mount(&server)
            .await;
        let policy = Pcr0TrustPolicy::from_static_allowlist([UNKNOWN_PCR0])
            .unwrap()
            .with_remote_history_urls([format!("{}/history.json", server.uri())])
            .unwrap();

        policy.verify_pcr0(&pcr_bytes(SIGNED_PCR0)).await.unwrap();
    }

    #[tokio::test]
    async fn invalid_remote_signature_fails_closed() {
        let server = MockServer::start().await;
        let invalid_signature = BASE64.encode([0u8; 96]);
        Mock::given(path("/history.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(history(&invalid_signature)))
            .expect(1)
            .mount(&server)
            .await;
        let policy = Pcr0TrustPolicy::from_static_allowlist([UNKNOWN_PCR0])
            .unwrap()
            .with_remote_history_urls([format!("{}/history.json", server.uri())])
            .unwrap();

        let error = policy
            .verify_pcr0(&pcr_bytes(SIGNED_PCR0))
            .await
            .unwrap_err();
        assert!(matches!(error, Error::AttestationVerificationFailed(_)));
    }

    #[tokio::test]
    async fn malformed_remote_history_fails_closed() {
        let server = MockServer::start().await;
        Mock::given(path("/history.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
                "PCR0": "too-short",
                "PCR1": "too-short",
                "PCR2": "too-short",
                "timestamp": 1,
                "signature": SIGNED_PCR0_SIGNATURE,
            }])))
            .expect(1)
            .mount(&server)
            .await;
        let policy = Pcr0TrustPolicy::from_static_allowlist([UNKNOWN_PCR0])
            .unwrap()
            .with_remote_history_urls([format!("{}/history.json", server.uri())])
            .unwrap();

        assert!(policy.verify_pcr0(&pcr_bytes(SIGNED_PCR0)).await.is_err());
    }

    #[tokio::test]
    async fn remote_history_redirects_are_not_followed() {
        let server = MockServer::start().await;
        let redirected_url = format!("{}/redirected.json", server.uri());
        Mock::given(path("/history.json"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", redirected_url))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/redirected.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(history(SIGNED_PCR0_SIGNATURE)))
            .expect(0)
            .mount(&server)
            .await;
        let policy = Pcr0TrustPolicy::from_static_allowlist([UNKNOWN_PCR0])
            .unwrap()
            .with_remote_history_urls([format!("{}/history.json", server.uri())])
            .unwrap();

        assert!(policy.verify_pcr0(&pcr_bytes(SIGNED_PCR0)).await.is_err());
    }

    #[test]
    fn rejects_unsafe_remote_history_urls() {
        assert!(Pcr0TrustPolicy::official()
            .with_remote_history_urls(["http://example.com/history.json"])
            .is_err());
        assert!(Pcr0TrustPolicy::official()
            .with_remote_history_urls(["https://user@example.com/history.json"])
            .is_err());
    }
}
