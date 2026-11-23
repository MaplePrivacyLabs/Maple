use super::bundle::{
    decode_release_statement, parse_without_duplicate_keys, verify_release_bundle,
    MeasurementTuple, ReleasePolicy, VerificationFailure, BUILDER_MEMBER, BUILD_MEMBER,
    CERTIFICATE_MEMBER, COMMIT_MEMBER, DSSE_ENVELOPE_MEMBER, ENVIRONMENT_MEMBER,
    INCLUSION_PROOF_MEMBER, LOG_INDEX_MEMBER, MESSAGE_SIGNATURE_MEMBER, PAYLOAD_MEMBER,
    PREDICATE_MEMBER, RAW_BYTES_MEMBER, REPOSITORY_MEMBER, RFC3161_TIMESTAMPS_MEMBER,
    RUN_ATTEMPT_MEMBER, RUN_ID_MEMBER, SIGNATURE_MEMBER, SOURCE_MEMBER, STATEMENT_TYPE_MEMBER,
    TIMESTAMP_VERIFICATION_DATA_MEMBER, TLOG_ENTRIES_MEMBER, VERIFICATION_MATERIAL_MEMBER,
    WORKFLOW_MEMBER,
};
use super::trust::{approval_key, embedded_trusted_root, tuf_http_client, RootRefresher};
use super::{
    ObservationTimeouts, ReleaseBundleSource, ReleaseVerificationOutcome,
    ReleaseVerificationResult, ReleaseVerifier, MAX_BUNDLE_BYTES,
};
use crate::attestation::AttestationDocument;
use crate::pcr::Pcr0Environment;
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use reqwest::Url;
use serde_json::{Map, Value};
use sigstore_trust_root::{SigstoreInstance, TrustedRoot, TufBootstrap, TufConfig};
use sigstore_types::DerPublicKey;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wiremock::matchers::{any, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const DEVELOPMENT_BUNDLE: &[u8] = include_bytes!(
    "../../assets/release/development/b422d5fcb25929a1b76f0eb8aa10c26a78c635a86a58039b48f22eb0dc4dc6f8ae6e646aa2fbbc57db756e6f06cad3dc.json"
);
const DEVELOPMENT_PCR0: &str =
    "b422d5fcb25929a1b76f0eb8aa10c26a78c635a86a58039b48f22eb0dc4dc6f8ae6e646aa2fbbc57db756e6f06cad3dc";
const DEVELOPMENT_PCR1: &str =
    "e83dc536d4cf82daf0e0821e7fdbddf25cd76090602cb67bef7c9d5ab2914417ee6a94bd0c9c1e7ed15a3c3c3bfcc0a3";
const DEVELOPMENT_PCR2: &str =
    "57b7e86919f571beff7539cb8643baafe81ec9f2ffb3434d683268978675a664e0d043dead3367aba14a5d2814a4be97";
const DEVELOPMENT_REPOSITORY: &str = "https://github.com/MaplePrivacyLabs/Maple";
const DEVELOPMENT_COMMIT: &str = "bcf6e84476be2cf08b575c5303f5a83d98adbf40";
const DEVELOPMENT_BUILD_RUN_ID: &str = "44636648269";
const DEVELOPMENT_BUILD_RUN_ATTEMPT: u64 = 1;
const DEVELOPMENT_LOG_INDEX: u64 = 207746380;
const UNSUPPORTED_STATEMENT_TYPE_ALIAS_MEMBER: &str = "payloadType";

struct StubRootRefresher {
    result: Mutex<Result<sigstore_trust_root::TrustedRoot, ()>>,
    calls: Arc<AtomicUsize>,
    delay: Duration,
}

impl StubRootRefresher {
    fn with_result(
        result: Result<sigstore_trust_root::TrustedRoot, ()>,
    ) -> (Box<dyn RootRefresher>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Box::new(Self {
                result: Mutex::new(result),
                calls: Arc::clone(&calls),
                delay: Duration::ZERO,
            }),
            calls,
        )
    }

    fn good_root() -> (Box<dyn RootRefresher>, Arc<AtomicUsize>) {
        Self::with_result(Ok(embedded_trusted_root().unwrap()))
    }
}

#[async_trait]
impl RootRefresher for StubRootRefresher {
    async fn refresh(&self) -> Result<sigstore_trust_root::TrustedRoot, ()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        self.result.lock().map_err(|_| ())?.clone()
    }
}

fn measurements() -> MeasurementTuple {
    MeasurementTuple {
        pcr0: hex::decode(DEVELOPMENT_PCR0).unwrap().try_into().unwrap(),
        pcr1: hex::decode(DEVELOPMENT_PCR1).unwrap().try_into().unwrap(),
        pcr2: hex::decode(DEVELOPMENT_PCR2).unwrap().try_into().unwrap(),
    }
}

fn document() -> AttestationDocument {
    let measurements = measurements();
    let mut pcrs = std::collections::HashMap::new();
    pcrs.insert(0, measurements.pcr0.to_vec());
    pcrs.insert(1, measurements.pcr1.to_vec());
    pcrs.insert(2, measurements.pcr2.to_vec());
    AttestationDocument {
        module_id: "local-test".to_owned(),
        timestamp: 0,
        digest: "SHA384".to_owned(),
        pcrs,
        certificate: Vec::new(),
        cabundle: Vec::new(),
        public_key: None,
        user_data: None,
        nonce: None,
    }
}

fn development_key() -> DerPublicKey {
    approval_key(Pcr0Environment::Development).unwrap().unwrap()
}

fn unused_host() -> Url {
    Url::parse("http://127.0.0.1:9/").unwrap()
}

fn embedded_verifier() -> ReleaseVerifier {
    let (root_refresher, _) = StubRootRefresher::good_root();
    ReleaseVerifier::for_test(
        Pcr0Environment::Development,
        Some(development_key()),
        unused_host(),
        embedded_trusted_root().unwrap(),
        root_refresher,
    )
}

fn remote_verifier(
    environment: Pcr0Environment,
    host: Url,
    root_refresher: Box<dyn RootRefresher>,
) -> ReleaseVerifier {
    ReleaseVerifier::for_test(
        environment,
        Some(development_key()),
        host,
        embedded_trusted_root().unwrap(),
        root_refresher,
    )
    .without_embedded_bundle()
}

#[tokio::test]
async fn bundle_and_tuf_redirects_are_not_followed() {
    let redirected = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_bytes(DEVELOPMENT_BUNDLE))
        .expect(0)
        .mount(&redirected)
        .await;

    let bundle_server = MockServer::start().await;
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(302).insert_header("location", redirected.uri().as_str()),
        )
        .mount(&bundle_server)
        .await;
    let (root_refresher, _) = StubRootRefresher::good_root();
    let verifier = remote_verifier(
        Pcr0Environment::Development,
        Url::parse(&bundle_server.uri()).unwrap(),
        root_refresher,
    );
    let result = verifier.observe(&document()).await;
    assert_eq!(result.outcome, ReleaseVerificationOutcome::Rejected);
    assert_eq!(result.category, "bundle-fetch-failed");

    let tuf_server = MockServer::start().await;
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(302).insert_header("location", redirected.uri().as_str()),
        )
        .mount(&tuf_server)
        .await;
    let config = TufConfig::custom(
        tuf_server.uri(),
        TufBootstrap::trusted(SigstoreInstance::PublicGood.tuf_root()),
    )
    .without_cache()
    .with_http_client(tuf_http_client().unwrap());
    assert!(TrustedRoot::from_tuf(config).await.is_err());
    tokio::time::sleep(Duration::from_millis(50)).await;
    redirected.verify().await;
}

fn decoded_payload(bundle: &[u8]) -> Value {
    let bundle: Value = serde_json::from_slice(bundle).unwrap();
    BASE64
        .decode(
            bundle[DSSE_ENVELOPE_MEMBER][PAYLOAD_MEMBER]
                .as_str()
                .unwrap(),
        )
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap()
}

fn unsigned_statement_carrier(encoded_payload: String) -> String {
    // This unsigned carrier exercises statement decoding and policy only.
    let mut envelope = Map::new();
    envelope.insert(PAYLOAD_MEMBER.to_owned(), Value::String(encoded_payload));
    let mut bundle = Map::new();
    bundle.insert(DSSE_ENVELOPE_MEMBER.to_owned(), Value::Object(envelope));
    Value::Object(bundle).to_string()
}

fn real_statement_mutation_error(mutate: impl FnOnce(&mut Value)) -> VerificationFailure {
    // A changed signed payload cannot pass DSSE verification. Exercise the
    // statement policy on this decoded real statement, as the other policy
    // mutation tests do, rather than constructing a look-alike statement.
    let mut payload = decoded_payload(DEVELOPMENT_BUNDLE);
    mutate(&mut payload);
    let encoded = BASE64.encode(serde_json::to_vec(&payload).unwrap());
    let carrier = unsigned_statement_carrier(encoded);
    decode_release_statement(carrier.as_bytes())
        .and_then(|(_, statement)| {
            statement.validate(ReleasePolicy::new(
                Pcr0Environment::Development,
                measurements(),
            ))
        })
        .expect_err("mutated real statement must be rejected")
}

fn assert_development_identity(result: &ReleaseVerificationResult) {
    let identity = result
        .identity
        .as_ref()
        .expect("verified result exposes signed audit data");
    assert_eq!(identity.repository, DEVELOPMENT_REPOSITORY);
    assert_eq!(identity.commit, DEVELOPMENT_COMMIT);
    assert_eq!(identity.build_run_id, DEVELOPMENT_BUILD_RUN_ID);
    assert_eq!(identity.build_run_attempt, DEVELOPMENT_BUILD_RUN_ATTEMPT);
    assert_eq!(identity.log_index, DEVELOPMENT_LOG_INDEX);
}

#[test]
fn real_development_bundle_verifies_offline_with_verified_tlog() {
    let evidence = verify_release_bundle(
        DEVELOPMENT_BUNDLE,
        &development_key(),
        &embedded_trusted_root().unwrap(),
        ReleasePolicy::new(Pcr0Environment::Development, measurements()),
    )
    .unwrap();
    assert_eq!(DEVELOPMENT_BUNDLE.len(), 5517);
    assert!(evidence.tlog_verified);
    assert_eq!(evidence.repository, DEVELOPMENT_REPOSITORY);
    assert_eq!(evidence.commit, DEVELOPMENT_COMMIT);
    assert_eq!(evidence.build_run_id, DEVELOPMENT_BUILD_RUN_ID);
    assert_eq!(evidence.build_run_attempt, DEVELOPMENT_BUILD_RUN_ATTEMPT);
    assert_eq!(evidence.log_index, DEVELOPMENT_LOG_INDEX);
}

#[test]
fn development_bundle_cannot_be_relabeled_production_or_match_other_measurements() {
    let error = verify_release_bundle(
        DEVELOPMENT_BUNDLE,
        &development_key(),
        &embedded_trusted_root().unwrap(),
        ReleasePolicy::new(Pcr0Environment::Production, measurements()),
    )
    .unwrap_err();
    assert_eq!(error.code(), "statement-environment-mismatch");

    let mut other_measurements = measurements();
    other_measurements.pcr1[0] ^= 1;
    let error = verify_release_bundle(
        DEVELOPMENT_BUNDLE,
        &development_key(),
        &embedded_trusted_root().unwrap(),
        ReleasePolicy::new(Pcr0Environment::Development, other_measurements),
    )
    .unwrap_err();
    assert_eq!(error.code(), "statement-measurement-mismatch");
}

#[test]
fn entry_log_index_must_match_the_inclusion_proof() {
    let mut bundle: Value = serde_json::from_slice(DEVELOPMENT_BUNDLE).unwrap();
    bundle[VERIFICATION_MATERIAL_MEMBER][TLOG_ENTRIES_MEMBER][0][LOG_INDEX_MEMBER] =
        Value::String("207716380".to_owned());
    let error = verify_release_bundle(
        bundle.to_string().as_bytes(),
        &development_key(),
        &embedded_trusted_root().unwrap(),
        ReleasePolicy::new(Pcr0Environment::Development, measurements()),
    )
    .expect_err("a changed entry index must not reach cryptographic verification");
    assert_eq!(error.code(), "tlog-index-mismatch");
}

#[test]
fn inclusion_proof_log_index_must_match_the_entry() {
    let mut bundle: Value = serde_json::from_slice(DEVELOPMENT_BUNDLE).unwrap();
    bundle[VERIFICATION_MATERIAL_MEMBER][TLOG_ENTRIES_MEMBER][0][INCLUSION_PROOF_MEMBER]
        [LOG_INDEX_MEMBER] = Value::String("207716380".to_owned());
    let error = verify_release_bundle(
        bundle.to_string().as_bytes(),
        &development_key(),
        &embedded_trusted_root().unwrap(),
        ReleasePolicy::new(Pcr0Environment::Development, measurements()),
    )
    .expect_err("a changed proof index must not reach cryptographic verification");
    assert_eq!(error.code(), "tlog-index-mismatch");
}

#[test]
fn tlog_index_mismatch_precedes_an_unreadable_payload() {
    let mut bundle: Value = serde_json::from_slice(DEVELOPMENT_BUNDLE).unwrap();
    bundle[VERIFICATION_MATERIAL_MEMBER][TLOG_ENTRIES_MEMBER][0][LOG_INDEX_MEMBER] =
        Value::String("1".to_owned());
    bundle[DSSE_ENVELOPE_MEMBER][PAYLOAD_MEMBER] = Value::String(BASE64.encode("{"));
    let error = verify_release_bundle(
        bundle.to_string().as_bytes(),
        &development_key(),
        &embedded_trusted_root().unwrap(),
        ReleasePolicy::new(Pcr0Environment::Development, measurements()),
    )
    .expect_err("the bundle's conflicting indexes must be rejected before its payload");
    assert_eq!(error.code(), "tlog-index-mismatch");
}

#[test]
fn message_signature_without_a_dsse_envelope_is_content_unsupported() {
    let mut bundle: Value = serde_json::from_slice(DEVELOPMENT_BUNDLE).unwrap();
    let object = bundle.as_object_mut().unwrap();
    let content = object.remove(DSSE_ENVELOPE_MEMBER).unwrap();
    object.insert(MESSAGE_SIGNATURE_MEMBER.to_owned(), content);
    let error = verify_release_bundle(
        bundle.to_string().as_bytes(),
        &development_key(),
        &embedded_trusted_root().unwrap(),
        ReleasePolicy::new(Pcr0Environment::Development, measurements()),
    )
    .expect_err("message signatures are not release statements");
    assert_eq!(error.code(), "content-unsupported");
}

#[test]
fn signed_statement_tampering_is_rejected_before_policy_can_authorize_it() {
    let mut bundle: Value = serde_json::from_slice(DEVELOPMENT_BUNDLE).unwrap();
    let mut payload = decoded_payload(DEVELOPMENT_BUNDLE);
    payload[PREDICATE_MEMBER][ENVIRONMENT_MEMBER] = Value::String("production".to_owned());
    bundle[DSSE_ENVELOPE_MEMBER][PAYLOAD_MEMBER] =
        Value::String(BASE64.encode(serde_json::to_vec(&payload).unwrap()));
    let error = verify_release_bundle(
        serde_json::to_string(&bundle).unwrap().as_bytes(),
        &development_key(),
        &embedded_trusted_root().unwrap(),
        ReleasePolicy::new(Pcr0Environment::Production, measurements()),
    )
    .unwrap_err();
    assert_eq!(error.code(), "dsse-signature-invalid");
}

#[test]
fn bad_signature_precedes_unsupported_statement_content() {
    let mut bundle: Value = serde_json::from_slice(DEVELOPMENT_BUNDLE).unwrap();
    let mut payload = decoded_payload(DEVELOPMENT_BUNDLE);
    payload[STATEMENT_TYPE_MEMBER] =
        Value::String("https://example.invalid/unsupported".to_owned());
    bundle[DSSE_ENVELOPE_MEMBER][PAYLOAD_MEMBER] =
        Value::String(BASE64.encode(serde_json::to_vec(&payload).unwrap()));
    let error = verify_release_bundle(
        serde_json::to_string(&bundle).unwrap().as_bytes(),
        &development_key(),
        &embedded_trusted_root().unwrap(),
        ReleasePolicy::new(Pcr0Environment::Development, measurements()),
    )
    .unwrap_err();
    assert_eq!(error.code(), "dsse-signature-invalid");
}

#[test]
fn source_build_and_workflow_are_not_authorization_identity() {
    let mut payload = decoded_payload(DEVELOPMENT_BUNDLE);
    payload[PREDICATE_MEMBER][SOURCE_MEMBER][REPOSITORY_MEMBER] =
        Value::String("example/attacker-controlled-fork".to_owned());
    payload[PREDICATE_MEMBER][SOURCE_MEMBER][WORKFLOW_MEMBER] =
        Value::String(".github/workflows/untrusted.yml".to_owned());
    payload[PREDICATE_MEMBER][BUILD_MEMBER][BUILDER_MEMBER] =
        Value::String("other-builder".to_owned());
    payload[PREDICATE_MEMBER][BUILD_MEMBER][RUN_ID_MEMBER] = Value::String("1".to_owned());
    let encoded = BASE64.encode(serde_json::to_vec(&payload).unwrap());
    let carrier = unsigned_statement_carrier(encoded);
    let (_, statement) = decode_release_statement(carrier.as_bytes()).unwrap();
    statement
        .validate(ReleasePolicy::new(
            Pcr0Environment::Development,
            measurements(),
        ))
        .unwrap();
}

#[test]
fn release_statement_rejects_an_empty_repository() {
    let error = real_statement_mutation_error(|statement| {
        statement[PREDICATE_MEMBER][SOURCE_MEMBER][REPOSITORY_MEMBER] =
            Value::String(String::new());
    });
    assert_eq!(error.code(), "statement-malformed");
}

#[test]
fn release_statement_rejects_a_short_commit() {
    let error = real_statement_mutation_error(|statement| {
        statement[PREDICATE_MEMBER][SOURCE_MEMBER][COMMIT_MEMBER] =
            Value::String("bcf6e844".to_owned());
    });
    assert_eq!(error.code(), "statement-malformed");
}

#[test]
fn release_statement_rejects_a_missing_build() {
    let error = real_statement_mutation_error(|statement| {
        statement[PREDICATE_MEMBER]
            .as_object_mut()
            .unwrap()
            .remove(BUILD_MEMBER);
    });
    assert_eq!(error.code(), "statement-malformed");
}

#[test]
fn release_statement_rejects_a_non_numeric_run_id() {
    let error = real_statement_mutation_error(|statement| {
        statement[PREDICATE_MEMBER][BUILD_MEMBER][RUN_ID_MEMBER] =
            Value::String("not-a-run-id".to_owned());
    });
    assert_eq!(error.code(), "statement-malformed");
}

#[test]
fn release_statement_rejects_a_zero_run_attempt() {
    let error = real_statement_mutation_error(|statement| {
        statement[PREDICATE_MEMBER][BUILD_MEMBER][RUN_ATTEMPT_MEMBER] = Value::from(0);
    });
    assert_eq!(error.code(), "statement-malformed");
}

#[test]
fn duplicate_statement_members_are_rejected() {
    let payload = decoded_payload(DEVELOPMENT_BUNDLE);
    let original_predicate = format!("{PREDICATE_MEMBER:?}:{{");
    let duplicate_predicate =
        format!("{PREDICATE_MEMBER:?}:{{{ENVIRONMENT_MEMBER:?}:\"production\",");
    let duplicate = serde_json::to_string(&payload).unwrap().replacen(
        &original_predicate,
        &duplicate_predicate,
        1,
    );
    let carrier = unsigned_statement_carrier(BASE64.encode(duplicate));
    let error = decode_release_statement(carrier.as_bytes()).err().unwrap();
    assert_eq!(error.code(), "duplicate-json-key");
}

#[test]
fn payload_type_does_not_replace_a_missing_statement_type() {
    let mut statement = decoded_payload(DEVELOPMENT_BUNDLE);
    statement
        .as_object_mut()
        .unwrap()
        .remove(STATEMENT_TYPE_MEMBER);
    statement[UNSUPPORTED_STATEMENT_TYPE_ALIAS_MEMBER] =
        Value::String("https://in-toto.io/Statement/v1".to_owned());
    let encoded = BASE64.encode(serde_json::to_vec(&statement).unwrap());
    let carrier = unsigned_statement_carrier(encoded);
    let error = decode_release_statement(carrier.as_bytes()).err().unwrap();
    assert_eq!(error.code(), "statement-unsupported");
}

#[test]
fn real_bundle_without_a_timestamp_is_rejected() {
    let mut bundle: Value = serde_json::from_slice(DEVELOPMENT_BUNDLE).unwrap();
    bundle[VERIFICATION_MATERIAL_MEMBER][TIMESTAMP_VERIFICATION_DATA_MEMBER]
        [RFC3161_TIMESTAMPS_MEMBER] = serde_json::json!([]);
    let error = verify_release_bundle(
        bundle.to_string().as_bytes(),
        &development_key(),
        &embedded_trusted_root().unwrap(),
        ReleasePolicy::new(Pcr0Environment::Development, measurements()),
    )
    .unwrap_err();
    assert_eq!(error.code(), "timestamp-count");
}

#[test]
fn certificate_material_beside_the_managed_key_is_rejected() {
    let mut bundle: Value = serde_json::from_slice(DEVELOPMENT_BUNDLE).unwrap();
    bundle[VERIFICATION_MATERIAL_MEMBER][CERTIFICATE_MEMBER] =
        serde_json::json!({ (RAW_BYTES_MEMBER): "AA==" });
    let error = verify_release_bundle(
        bundle.to_string().as_bytes(),
        &development_key(),
        &embedded_trusted_root().unwrap(),
        ReleasePolicy::new(Pcr0Environment::Development, measurements()),
    )
    .unwrap_err();
    assert_eq!(error.code(), "verification-material-unsupported");
}

#[test]
fn message_signature_beside_the_dsse_envelope_is_rejected() {
    let mut bundle: Value = serde_json::from_slice(DEVELOPMENT_BUNDLE).unwrap();
    bundle[MESSAGE_SIGNATURE_MEMBER] = serde_json::json!({ (SIGNATURE_MEMBER): "AA==" });
    let error = verify_release_bundle(
        bundle.to_string().as_bytes(),
        &development_key(),
        &embedded_trusted_root().unwrap(),
        ReleasePolicy::new(Pcr0Environment::Development, measurements()),
    )
    .unwrap_err();
    assert_eq!(error.code(), "conflicting-oneof");
}

#[tokio::test]
async fn embedded_bundle_verifies_and_then_uses_verified_cache() {
    let verifier = embedded_verifier();
    let first = verifier.observe(&document()).await;
    assert_eq!(first.outcome, ReleaseVerificationOutcome::Verified);
    assert_eq!(first.source, ReleaseBundleSource::Embedded);
    assert!(first.tlog_verified());
    assert_development_identity(&first);
    assert_eq!(verifier.cache_len(), 1);

    let second = verifier.observe(&document()).await;
    assert_eq!(second.outcome, ReleaseVerificationOutcome::Verified);
    assert_eq!(second.source, ReleaseBundleSource::Cache);
    assert_development_identity(&second);
    assert_eq!(verifier.cache_len(), 1);
}

#[tokio::test]
async fn unembedded_bundle_is_fetched_once_then_served_from_cache() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/{DEVELOPMENT_PCR0}.json")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(DEVELOPMENT_BUNDLE))
        .expect(1)
        .mount(&server)
        .await;
    let (root_refresher, refresh_calls) = StubRootRefresher::good_root();
    let verifier = remote_verifier(
        Pcr0Environment::Development,
        Url::parse(&server.uri()).unwrap(),
        root_refresher,
    );

    let first = verifier.observe(&document()).await;
    assert_eq!(first.outcome, ReleaseVerificationOutcome::Verified);
    assert_eq!(first.source, ReleaseBundleSource::Remote);
    assert!(first.tlog_verified());
    let second = verifier.observe(&document()).await;
    assert_eq!(second.source, ReleaseBundleSource::Cache);
    assert_eq!(second.outcome, ReleaseVerificationOutcome::Verified);
    assert_eq!(refresh_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn production_cannot_accept_development_bundle_or_use_another_policy_cache() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(DEVELOPMENT_BUNDLE))
        .expect(1)
        .mount(&server)
        .await;
    let (root_refresher, _) = StubRootRefresher::good_root();
    let production = remote_verifier(
        Pcr0Environment::Production,
        Url::parse(&server.uri()).unwrap(),
        root_refresher,
    );
    let development = embedded_verifier();
    assert_eq!(
        development.observe(&document()).await.outcome,
        ReleaseVerificationOutcome::Verified
    );
    assert_eq!(development.cache_len(), 1);
    assert_eq!(production.cache_len(), 0);
    let result = production.observe(&document()).await;
    assert_eq!(result.outcome, ReleaseVerificationOutcome::Rejected);
    assert_eq!(result.category, "statement-environment-mismatch");
    assert_eq!(production.cache_len(), 0);
}

#[tokio::test]
async fn missing_production_approval_key_never_fetches_or_falls_back() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let (root_refresher, refresh_calls) = StubRootRefresher::good_root();
    let verifier = ReleaseVerifier::for_test(
        Pcr0Environment::Production,
        None,
        Url::parse(&server.uri()).unwrap(),
        embedded_trusted_root().unwrap(),
        root_refresher,
    );
    let result = verifier.observe(&document()).await;
    assert_eq!(result.category, "approval-key-not-configured");
    assert_eq!(result.outcome, ReleaseVerificationOutcome::Rejected);
    assert_eq!(refresh_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn measurement_mismatch_is_not_cached_as_verified() {
    let verifier = embedded_verifier();
    let mut document = document();
    document.pcrs.get_mut(&1).unwrap()[0] ^= 1;
    let first = verifier.observe(&document).await;
    assert_eq!(first.category, "statement-measurement-mismatch");
    assert_eq!(verifier.cache_len(), 0);
    let second = verifier.observe(&document).await;
    assert_eq!(second.source, ReleaseBundleSource::Embedded);
    assert_eq!(second.outcome, ReleaseVerificationOutcome::Rejected);
    assert_eq!(verifier.cache_len(), 0);
}

#[tokio::test]
async fn missing_authenticated_pcr_tuple_is_a_policy_mismatch() {
    let verifier = embedded_verifier();
    let mut document = document();
    document.pcrs.remove(&2);
    let result = verifier.observe(&document).await;
    assert_eq!(result.category, "statement-measurement-mismatch");
    assert_eq!(result.source, ReleaseBundleSource::Unavailable);
    assert_eq!(verifier.cache_len(), 0);
}

#[tokio::test]
async fn bundle_not_found_is_observable_without_caching() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    let (root_refresher, _) = StubRootRefresher::good_root();
    let verifier = remote_verifier(
        Pcr0Environment::Development,
        Url::parse(&server.uri()).unwrap(),
        root_refresher,
    );
    assert_eq!(
        verifier.observe(&document()).await.category,
        "bundle-not-found"
    );
    assert_eq!(verifier.cache_len(), 0);
}

#[tokio::test]
async fn bundle_fetch_timeout_is_observable_without_caching() {
    let slow_server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(200)))
        .mount(&slow_server)
        .await;
    let (root_refresher, _) = StubRootRefresher::good_root();
    let verifier = remote_verifier(
        Pcr0Environment::Development,
        Url::parse(&slow_server.uri()).unwrap(),
        root_refresher,
    )
    .with_timeouts(ObservationTimeouts {
        bundle_fetch: Duration::from_millis(25),
        root_refresh: Duration::from_millis(25),
        overall: Duration::from_secs(1),
    });
    let started = Instant::now();
    assert_eq!(
        verifier.observe(&document()).await.category,
        "bundle-timeout"
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(verifier.cache_len(), 0);
}

#[tokio::test]
async fn overall_observation_deadline_cancels_slow_network_work() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(2)))
        .mount(&server)
        .await;
    let (root_refresher, _) = StubRootRefresher::good_root();
    let verifier = remote_verifier(
        Pcr0Environment::Development,
        Url::parse(&server.uri()).unwrap(),
        root_refresher,
    )
    .with_timeouts(ObservationTimeouts {
        bundle_fetch: Duration::from_secs(5),
        root_refresh: Duration::from_secs(5),
        overall: Duration::from_millis(25),
    });
    let started = Instant::now();
    assert_eq!(
        verifier.observe(&document()).await.category,
        "observation-timeout"
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(verifier.cache_len(), 0);
}

#[tokio::test]
async fn chunked_bundle_body_is_capped_before_json_parsing() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 4096];
        let _ = socket.read(&mut request).await;
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        let chunk = vec![b'x'; 64 * 1024];
        for _ in 0..17 {
            let chunk_size_write_failed = socket
                .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                .await
                .is_err();
            if chunk_size_write_failed {
                return;
            }
            let chunk_write_failed = socket.write_all(&chunk).await.is_err();
            if chunk_write_failed {
                return;
            }
            let terminator_write_failed = socket.write_all(b"\r\n").await.is_err();
            if terminator_write_failed {
                return;
            }
        }
        let _ = socket.write_all(b"0\r\n\r\n").await;
    });
    let (root_refresher, _) = StubRootRefresher::good_root();
    let verifier = remote_verifier(
        Pcr0Environment::Development,
        Url::parse(&format!("http://{address}")).unwrap(),
        root_refresher,
    );
    let result = verifier.observe(&document()).await;
    assert_eq!(result.category, "bundle-too-large");
    assert_eq!(verifier.cache_len(), 0);
    tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn declared_oversized_bundle_is_rejected_before_body_reading() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; MAX_BUNDLE_BYTES + 1]))
        .mount(&server)
        .await;
    let (root_refresher, _) = StubRootRefresher::good_root();
    let verifier = remote_verifier(
        Pcr0Environment::Development,
        Url::parse(&server.uri()).unwrap(),
        root_refresher,
    );
    assert_eq!(
        verifier.observe(&document()).await.category,
        "bundle-too-large"
    );
    assert_eq!(verifier.cache_len(), 0);
}

#[tokio::test]
async fn failed_root_refresh_preserves_failure_and_is_observable() {
    let staging_tsa = sigstore_trust_root::TrustedRoot::from_json(include_str!(
        "../../tests/fixtures/managed-key/roots/production-staging-tsa.json"
    ))
    .unwrap();
    let verifier = ReleaseVerifier::for_test(
        Pcr0Environment::Development,
        Some(development_key()),
        unused_host(),
        staging_tsa,
        StubRootRefresher::with_result(Err(())).0,
    );
    let result = verifier.observe(&document()).await;
    assert_eq!(result.outcome, ReleaseVerificationOutcome::Rejected);
    assert_eq!(result.category, "timestamp-untrusted-authority");
    assert_eq!(result.refresh_error, Some("trust-refresh-failed"));
    assert_eq!(verifier.cache_len(), 0);
}

#[tokio::test]
async fn failed_verification_refreshes_tuf_root_once_then_retries() {
    let staging_tsa = sigstore_trust_root::TrustedRoot::from_json(include_str!(
        "../../tests/fixtures/managed-key/roots/production-staging-tsa.json"
    ))
    .unwrap();
    let (root_refresher, calls) = StubRootRefresher::good_root();
    let verifier = ReleaseVerifier::for_test(
        Pcr0Environment::Development,
        Some(development_key()),
        unused_host(),
        staging_tsa,
        root_refresher,
    );
    let result = verifier.observe(&document()).await;
    assert_eq!(result.outcome, ReleaseVerificationOutcome::Verified);
    assert!(result.tlog_verified());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let cached = verifier.observe(&document()).await;
    assert_eq!(cached.source, ReleaseBundleSource::Cache);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn concurrent_verifiers_coalesce_root_refresh_and_share_only_successes() {
    let staging_tsa = sigstore_trust_root::TrustedRoot::from_json(include_str!(
        "../../tests/fixtures/managed-key/roots/production-staging-tsa.json"
    ))
    .unwrap();
    let (root_refresher, calls) = StubRootRefresher::good_root();
    let verifier = Arc::new(ReleaseVerifier::for_test(
        Pcr0Environment::Development,
        Some(development_key()),
        unused_host(),
        staging_tsa,
        root_refresher,
    ));
    let mut tasks = Vec::new();
    for _ in 0..4 {
        let verifier = Arc::clone(&verifier);
        tasks.push(tokio::spawn(
            async move { verifier.observe(&document()).await },
        ));
    }
    for task in tasks {
        assert_eq!(
            task.await.unwrap().outcome,
            ReleaseVerificationOutcome::Verified
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(verifier.cache_len(), 1);
}

#[test]
fn cancelled_observation_never_starts_queued_verification() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        ready_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("the sole blocking worker is occupied");

        let worker_starts = Arc::new(AtomicUsize::new(0));
        let verifier = embedded_verifier().with_worker_start_counter(Arc::clone(&worker_starts));
        let document = document();
        let mut observation = Box::pin(verifier.observe(&document));
        let observation_cancelled =
            tokio::time::timeout(Duration::from_millis(25), &mut observation)
                .await
                .is_err();
        assert!(observation_cancelled);
        drop(observation);
        release_tx.send(()).unwrap();
        blocker.await.unwrap();
        tokio::time::sleep(Duration::from_millis(25)).await;
        let starts_after_cancellation = worker_starts.load(Ordering::SeqCst);
        assert_eq!(starts_after_cancellation, 0);
        println!("verification starts after cancellation: {starts_after_cancellation}");

        let completed = verifier.observe(&document).await;
        assert_eq!(completed.outcome, ReleaseVerificationOutcome::Verified);
        assert_eq!(worker_starts.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn embedded_trust_inputs_construct_the_selected_verifier() {
    assert!(approval_key(Pcr0Environment::Development)
        .unwrap()
        .is_some());
    embedded_trusted_root().unwrap();
    ReleaseVerifier::new(Pcr0Environment::Development).unwrap();
}

#[test]
fn production_verifier_rejects_verification_without_an_approval_key() {
    let verifier = ReleaseVerifier::new(Pcr0Environment::Production).unwrap();
    let error = verifier
        .verify_bundle(b"not evaluated without a key", measurements())
        .unwrap_err();
    assert_eq!(error.code(), "approval-key-not-configured");
}

#[test]
fn json_syntax_errors_are_distinct_from_duplicate_members() {
    let syntax = parse_without_duplicate_keys(r#"{"mediaType":true,}"#).unwrap_err();
    assert_eq!(syntax.code(), "json-syntax-error");
    let incomplete = parse_without_duplicate_keys("{").unwrap_err();
    assert_eq!(incomplete.code(), "json-syntax-error");

    let duplicate =
        parse_without_duplicate_keys(r#"{"mediaType":"one","mediaType":"two"}"#).unwrap_err();
    assert_eq!(duplicate.code(), "duplicate-json-key");
}

#[test]
fn json_syntax_precedes_duplicates_in_bundle_and_statement_text() {
    let bundle =
        parse_without_duplicate_keys(r#"{"mediaType":"one","mediaType":"two",}"#).unwrap_err();
    assert_eq!(bundle.code(), "json-syntax-error");

    let payload = serde_json::to_string(&decoded_payload(DEVELOPMENT_BUNDLE)).unwrap();
    let original = format!("{PREDICATE_MEMBER:?}:{{");
    let duplicate = format!("{PREDICATE_MEMBER:?}:{{{ENVIRONMENT_MEMBER:?}:\"production\",");
    let mut payload = payload.replacen(&original, &duplicate, 1);
    payload.pop();
    let carrier = unsigned_statement_carrier(BASE64.encode(payload));
    let statement = decode_release_statement(carrier.as_bytes()).err().unwrap();
    assert_eq!(statement.code(), "json-syntax-error");
}

#[test]
fn nested_json_syntax_errors_do_not_look_like_duplicate_members() {
    let nested = parse_without_duplicate_keys(r#"{"outer":{"inner":[1,]}}"#).unwrap_err();
    assert_eq!(nested.code(), "json-syntax-error");
    let duplicate = parse_without_duplicate_keys(r#"{"outer":{"inner":1,"inner":2}}"#).unwrap_err();
    assert_eq!(duplicate.code(), "duplicate-json-key");
}
