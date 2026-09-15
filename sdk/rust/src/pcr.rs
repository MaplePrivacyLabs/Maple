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
    // Approved 2026-09-14 (services/opensecret signed history)
    "a0ef363874f70bd82af98d8e628bf7f128dfca1f9918e83af494cd17f5d4ff1a0c1b94b235941a6d20138d1152cbe5aa",
    // Approved 2026-09-12 (services/opensecret signed history)
    "3d2f90b4dac7316e5485e03f4cdf3f2f8ae79fde77442546475eac6c3037916ac54b8bdb26c85a73e9b778a9a5077638",
    // Approved 2026-09-05 (services/opensecret signed history)
    "a1c09f74133cbcc0e5034b9a8a5ed1a5a567d4922140d69534827b4473c7f385b989ce594e0b11f6b8ec971decb95608",
    "eeddbb58f57c38894d6d5af5e575fbe791c5bf3bbcfb5df8da8cfcf0c2e1da1913108e6a762112444740b88c163d7f4b",
    "74ed417f88cb0ca76c4a3d10f278bd010f1d3f95eafb254d4732511bb50e404507a4049b779c5230137e4091a5582271",
    "9043fcab93b972d3c14ad2dc8fa78ca7ad374fc937c02435681772a003f7a72876bc4d578089b5c4cf3fe9b480f1aabb",
    "52c3595b151d93d8b159c257301bfd5aa6f49210de0c55a6cd6df5ebeee44e4206cab950500f5d188f7fa14e6d900b75",
    "91cb67311e910cce68cd5b7d0de77aa40610d87c6681439b44c46c3ff786ae643956ab2c812478a1da8745b259f07a45",
    "859065ac81b81d3735130ba08b8af72a7256b603fefb74faabae25ed28cca6edcaa7c10ea32b5948d675c18a9b0f2b1d",
    "acd82a7d3943e23e95a9dc3ce0b0107ea358d6287f9e3afa245622f7c7e3e0a66142a928b6efcc02f594a95366d3a99d",
];

const OFFICIAL_DEVELOPMENT_PCR0S: &[&str] = &[
    // Approved 2026-09-14 (services/opensecret signed history)
    "61d92c66684de563fbc4ce3ece9680966672a065a65dedfd1dcdab149b2d3d3d725ab853dae6dc251dbb3326ff7d1214",
    // Approved 2026-09-12 (services/opensecret signed history)
    "c5470ea9d2d8499a65fbb9bc936bd7a62a5ef90b54da1504535cb4384f303fb673369d64f1a43f17c8ed8d2a89f09949",
    // Approved 2026-09-05 (services/opensecret signed history)
    "831eb975dad0b3f3741612acc92f020c0467bd3ac967ddf925fa60eff9e02cf9239ec50c699715fbf9a35e3448e2a117",
    "62c0407056217a4c10764ed9045694c29fa93255d3cc04c2f989cdd9a1f8050c8b169714c71f1118ebce2fcc9951d1a9",
    "cb95519905443f9f66f05f63c548b61ad1561a27fd5717b69285861aaea3c3063fe12a2571773b67fea3c6c11b4d8ec6",
    "deb5895831b5e4286f5a2dcf5e9c27383821446f8df2b465f141d10743599be20ba3bb381ce063bf7139cc89f7f61d4c",
    "70ba26c6af1ec3b57ce80e1adcc0ee96d70224d4c7a078f427895cdf68e1c30f09b5ac4c456588d872f3f21ff77c036b",
    "669404ea71435b8f498b48db7816a5c2ab1d258b1a77685b11d84d15a73189504d79c4dee13a658de9f4a0cbfc39cfe8",
    "a791bf92c25ffdfd372660e460a0e238c6778c090672df6509ae4bc065cf8668b6baac6b6a11d554af53ee0ff0172ad5",
    "c4285443b87b9b12a6cea3bef1064ec060f652b235a297095975af8f134e5ed65f92d70d4616fdec80af9dff48bb9f35",
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
