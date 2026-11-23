use crate::pcr::Pcr0Environment;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde::de::{Deserialize, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};
use sigstore_trust_root::TrustedRoot;
use sigstore_types::{
    Artifact, Bundle, DerPublicKey, MediaType, Sha256Hash, SignatureContent,
    VerificationMaterialContent,
};
use sigstore_verify::{
    Error as SigstoreError, PublicKeyVerificationPolicy, TimestampError, TransparencyLogError,
    Verifier,
};
use std::cell::Cell;
use std::collections::HashSet;

const EXPECTED_MEDIA_TYPE: &str = "application/vnd.dev.sigstore.bundle.v0.3+json";
pub(super) const EXPECTED_PAYLOAD_TYPE: &str = "application/vnd.in-toto+json";
const EXPECTED_STATEMENT_TYPE: &str = "https://in-toto.io/Statement/v1";
const EXPECTED_PREDICATE_TYPE: &str = "https://maple.ai/attestations/enclave-release/v1";
const SUBJECT_SHA256_HEX_LEN: usize = 64;
const SOURCE_COMMIT_HEX_LEN: usize = 40;
const MEASUREMENT_HEX_LEN: usize = 96;
const MAX_SAFE_RUN_ATTEMPT: u64 = 9_007_199_254_740_991;
const VERIFIED_TIMESTAMP_COUNT: usize = 1;
const TRANSPARENCY_LOG_ENTRY_COUNT: usize = 1;
const DSSE_SIGNATURE_COUNT: usize = 1;
const MEDIA_TYPE_MEMBER: &str = "mediaType";
pub(super) const MESSAGE_SIGNATURE_MEMBER: &str = "messageSignature";
pub(super) const DSSE_ENVELOPE_MEMBER: &str = "dsseEnvelope";
const SIGNATURES_MEMBER: &str = "signatures";
pub(super) const DSSE_PAYLOAD_TYPE_MEMBER: &str = "payloadType";
pub(super) const VERIFICATION_MATERIAL_MEMBER: &str = "verificationMaterial";
const PUBLIC_KEY_MEMBER: &str = "publicKey";
#[cfg(test)]
pub(super) const RAW_BYTES_MEMBER: &str = "rawBytes";
pub(super) const CERTIFICATE_MEMBER: &str = "certificate";
#[cfg(test)]
pub(super) const SIGNATURE_MEMBER: &str = "signature";
const X509_CERTIFICATE_CHAIN_MEMBER: &str = "x509CertificateChain";
pub(super) const TLOG_ENTRIES_MEMBER: &str = "tlogEntries";
pub(super) const LOG_INDEX_MEMBER: &str = "logIndex";
pub(super) const INCLUSION_PROOF_MEMBER: &str = "inclusionProof";
const KIND_VERSION_MEMBER: &str = "kindVersion";
const KIND_MEMBER: &str = "kind";
const VERSION_MEMBER: &str = "version";
const REKOR_V2_KIND_VERSION: (&str, &str) = ("hashedrekord", "0.0.2");
const INTEGRATED_TIME_MEMBER: &str = "integratedTime";
const INCLUSION_PROMISE_MEMBER: &str = "inclusionPromise";
pub(super) const TIMESTAMP_VERIFICATION_DATA_MEMBER: &str = "timestampVerificationData";
pub(super) const RFC3161_TIMESTAMPS_MEMBER: &str = "rfc3161Timestamps";
pub(super) const PAYLOAD_MEMBER: &str = "payload";
pub(super) const STATEMENT_TYPE_MEMBER: &str = "_type";
pub(super) const PREDICATE_TYPE_MEMBER: &str = "predicateType";

pub(super) const SUBJECT_MEMBER: &str = "subject";
const SUBJECT_NAME_MEMBER: &str = "name";
pub(super) const DIGEST_MEMBER: &str = "digest";
pub(super) const SHA256_MEMBER: &str = "sha256";
pub(super) const PREDICATE_MEMBER: &str = "predicate";
pub(super) const ENVIRONMENT_MEMBER: &str = "environment";
pub(super) const SOURCE_MEMBER: &str = "source";
pub(super) const REPOSITORY_MEMBER: &str = "repository";
pub(super) const COMMIT_MEMBER: &str = "commit";
#[cfg(test)]
pub(super) const WORKFLOW_MEMBER: &str = "workflow";
pub(super) const BUILD_MEMBER: &str = "build";
pub(super) const RUN_ID_MEMBER: &str = "runId";
pub(super) const RUN_ATTEMPT_MEMBER: &str = "runAttempt";
const MEASUREMENTS_MEMBER: &str = "measurements";
const PCR0_MEMBER: &str = "PCR0";
const PCR1_MEMBER: &str = "PCR1";
const PCR2_MEMBER: &str = "PCR2";
#[cfg(test)]
pub(super) const BUILDER_MEMBER: &str = "builder";
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct VerificationFailure {
    code: &'static str,
    refreshable: bool,
}

impl VerificationFailure {
    pub(super) fn code(self) -> &'static str {
        self.code
    }

    pub(super) fn refreshable(self) -> bool {
        self.refreshable
    }

    pub(super) fn policy(code: &'static str) -> Self {
        Self {
            code,
            refreshable: false,
        }
    }

    fn verifier(code: &'static str) -> Self {
        Self {
            code,
            refreshable: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct MeasurementTuple {
    pub(super) pcr0: [u8; 48],
    pub(super) pcr1: [u8; 48],
    pub(super) pcr2: [u8; 48],
}

impl MeasurementTuple {
    pub(super) fn pcr0_hex(&self) -> String {
        hex::encode(self.pcr0)
    }
}

#[derive(Debug)]
pub(super) struct VerificationEvidence {
    pub(super) tlog_verified: bool,
    log_index: u64,
}

#[derive(Debug)]
pub(super) struct ReleaseVerificationEvidence {
    pub(super) tlog_verified: bool,
    pub(super) repository: String,
    pub(super) commit: String,
    pub(super) build_run_id: String,
    pub(super) build_run_attempt: u64,
    pub(super) log_index: u64,
}

#[derive(Clone, Copy)]
pub(super) struct ReleasePolicy {
    environment: Pcr0Environment,
    measurements: MeasurementTuple,
}

impl ReleasePolicy {
    pub(super) fn new(environment: Pcr0Environment, measurements: MeasurementTuple) -> Self {
        Self {
            environment,
            measurements,
        }
    }
}

#[cfg(test)]
pub(super) fn verify_managed_key_bundle(
    bundle_bytes: &[u8],
    approval_key: &DerPublicKey,
    trusted_root: &TrustedRoot,
    artifact: Artifact<'_>,
) -> Result<VerificationEvidence, VerificationFailure> {
    verify(
        bundle_bytes,
        approval_key,
        trusted_root,
        artifact,
        VerificationMode::ManagedKeyCorpus,
    )
}

pub(super) fn verify_release_bundle(
    bundle_bytes: &[u8],
    approval_key: &DerPublicKey,
    trusted_root: &TrustedRoot,
    release_policy: ReleasePolicy,
) -> Result<ReleaseVerificationEvidence, VerificationFailure> {
    // Maple has no EIF bytes here. The only artifact is the signed statement's
    // typed SHA-256 subject digest; the separate SHA-384 PCR tuple is checked
    // against the authenticated Nitro document after signature verification.
    let (_, statement) = decode_release_statement(bundle_bytes)?;
    let artifact = Artifact::from_digest(statement.subject_digest());
    let evidence = verify(
        bundle_bytes,
        approval_key,
        trusted_root,
        artifact,
        VerificationMode::EnclaveRelease,
    )?;
    let source = statement.validate(release_policy)?;
    Ok(ReleaseVerificationEvidence {
        tlog_verified: evidence.tlog_verified,
        repository: source.repository,
        commit: source.commit,
        build_run_id: source.run_id,
        build_run_attempt: source.run_attempt,
        log_index: evidence.log_index,
    })
}

#[derive(Clone, Copy)]
enum VerificationMode {
    // The corpus selects this mode; production release sessions do not.
    #[cfg_attr(not(test), allow(dead_code))]
    ManagedKeyCorpus,
    EnclaveRelease,
}

fn verify(
    bundle_bytes: &[u8],
    approval_key: &DerPublicKey,
    trusted_root: &TrustedRoot,
    artifact: Artifact<'_>,
    mode: VerificationMode,
) -> Result<VerificationEvidence, VerificationFailure> {
    let bundle_text = std::str::from_utf8(bundle_bytes)
        .map_err(|_| VerificationFailure::policy("bundle-malformed"))?;
    parse_without_duplicate_keys(bundle_text)?;
    let raw_bundle = parse_complete_json(bundle_text)
        .map_err(|_| VerificationFailure::policy("bundle-malformed"))?;
    enforce_matching_tlog_indexes(&raw_bundle)?;
    enforce_managed_key_profile(&raw_bundle, mode)?;

    let bundle = Bundle::from_json(bundle_text)
        .map_err(|_| VerificationFailure::policy("bundle-malformed"))?;
    let has_supported_media_type = bundle.media_type == MediaType::Bundle0_3;
    if !has_supported_media_type {
        return Err(VerificationFailure::policy("media-type-unsupported"));
    }
    let has_public_key = matches!(
        bundle.verification_material.content,
        VerificationMaterialContent::PublicKey(_)
    );
    if !has_public_key {
        return Err(VerificationFailure::policy(
            "verification-material-unsupported",
        ));
    }
    if let VerificationMode::EnclaveRelease = mode {
        enforce_release_content(&bundle)?;
    }

    let policy =
        PublicKeyVerificationPolicy::new(approval_key.clone()).require_checkpoint_log_binding();
    let verifier = Verifier::new(trusted_root)
        .map_err(|error| VerificationFailure::verifier(reason_code(&error)))?;
    let result = verifier
        .verify(artifact, &bundle, &policy)
        .map_err(|error| VerificationFailure::verifier(reason_code(&error)))?;
    let has_verified_tlog = result.tlog_verified();
    if !has_verified_tlog {
        return Err(VerificationFailure::policy("verification-incomplete"));
    }
    let has_one_verified_timestamp = result.verified_timestamps().len() == VERIFIED_TIMESTAMP_COUNT;
    if !has_one_verified_timestamp {
        return Err(VerificationFailure::policy("verification-incomplete"));
    }
    // Rekor v2 uses this entry index as the leaf position proved against the
    // signed checkpoint. The earlier equality check makes it the proof's index.
    let log_index = bundle
        .verification_material
        .tlog_entries
        .first()
        .map(|entry| entry.log_index.get())
        .ok_or_else(|| VerificationFailure::policy("verification-incomplete"))?;
    Ok(VerificationEvidence {
        tlog_verified: result.tlog_verified(),
        log_index,
    })
}

fn enforce_matching_tlog_indexes(raw_bundle: &Value) -> Result<(), VerificationFailure> {
    // Verifiers differ in which duplicate index they prove. Reject conflicting
    // values before verification so clients agree on both outcome and audit
    // data. Missing proofs remain the verifier's existing rejection path.
    let entries = raw_bundle
        .get(VERIFICATION_MATERIAL_MEMBER)
        .and_then(|material| material.get(TLOG_ENTRIES_MEMBER))
        .and_then(Value::as_array);
    let Some(entries) = entries else {
        return Ok(());
    };
    for entry in entries {
        let Some(entry) = entry.as_object() else {
            continue;
        };
        let proof = entry.get(INCLUSION_PROOF_MEMBER).and_then(Value::as_object);
        let Some(proof) = proof else {
            continue;
        };
        let entry_index = entry.get(LOG_INDEX_MEMBER).and_then(Value::as_str);
        let proof_index = proof.get(LOG_INDEX_MEMBER).and_then(Value::as_str);
        let entry_index_is_decimal = entry_index.is_some_and(is_decimal);
        let indexes_agree = entry_index_is_decimal && entry_index == proof_index;
        if !indexes_agree {
            return Err(VerificationFailure::policy("tlog-index-mismatch"));
        }
    }
    Ok(())
}

fn enforce_managed_key_profile(
    raw_bundle: &Value,
    mode: VerificationMode,
) -> Result<(), VerificationFailure> {
    let media_type = raw_bundle.get(MEDIA_TYPE_MEMBER).and_then(Value::as_str);
    let has_supported_media_type = media_type == Some(EXPECTED_MEDIA_TYPE);
    if !has_supported_media_type {
        return Err(VerificationFailure::policy("media-type-unsupported"));
    }

    let raw_object = raw_bundle
        .as_object()
        .ok_or_else(|| VerificationFailure::policy("bundle-malformed"))?;
    let content_variants = count_present(
        raw_object,
        &[MESSAGE_SIGNATURE_MEMBER, DSSE_ENVELOPE_MEMBER],
    );
    let has_one_content_variant = content_variants == 1;
    if !has_one_content_variant {
        return Err(VerificationFailure::policy("conflicting-oneof"));
    }
    let material = raw_bundle
        .get(VERIFICATION_MATERIAL_MEMBER)
        .and_then(Value::as_object)
        .ok_or_else(|| VerificationFailure::policy("bundle-malformed"))?;
    let material_variants = count_present(
        material,
        &[
            PUBLIC_KEY_MEMBER,
            CERTIFICATE_MEMBER,
            X509_CERTIFICATE_CHAIN_MEMBER,
        ],
    );
    let has_only_public_key = material_variants == 1 && material.contains_key(PUBLIC_KEY_MEMBER);
    if !has_only_public_key {
        return Err(VerificationFailure::policy(
            "verification-material-unsupported",
        ));
    }

    let envelope = raw_object
        .get(DSSE_ENVELOPE_MEMBER)
        .and_then(Value::as_object);
    let dsse_signature_count = envelope
        .and_then(|envelope| envelope.get(SIGNATURES_MEMBER))
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or_default();
    let has_one_dsse_signature = dsse_signature_count == DSSE_SIGNATURE_COUNT;
    match mode {
        VerificationMode::EnclaveRelease => {
            let has_message_signature = raw_object.contains_key(MESSAGE_SIGNATURE_MEMBER);
            let has_unsupported_content = has_message_signature || !has_one_dsse_signature;
            if has_unsupported_content {
                return Err(VerificationFailure::policy("content-unsupported"));
            }
            let payload_type = envelope
                .and_then(|envelope| envelope.get(DSSE_PAYLOAD_TYPE_MEMBER))
                .and_then(Value::as_str);
            let has_supported_payload_type = payload_type == Some(EXPECTED_PAYLOAD_TYPE);
            if !has_supported_payload_type {
                return Err(VerificationFailure::policy("statement-unsupported"));
            }
        }
        VerificationMode::ManagedKeyCorpus => {
            let has_message_signature = raw_object
                .get(MESSAGE_SIGNATURE_MEMBER)
                .is_some_and(Value::is_object);
            let has_supported_content = has_one_dsse_signature || has_message_signature;
            if !has_supported_content {
                return Err(VerificationFailure::policy("content-unsupported"));
            }
        }
    }

    let entries = material
        .get(TLOG_ENTRIES_MEMBER)
        .and_then(Value::as_array)
        .ok_or_else(|| VerificationFailure::policy("tlog-entry-count"))?;
    let has_one_tlog_entry = entries.len() == TRANSPARENCY_LOG_ENTRY_COUNT;
    if !has_one_tlog_entry {
        return Err(VerificationFailure::policy("tlog-entry-count"));
    }
    let entry = entries[0]
        .as_object()
        .ok_or_else(|| VerificationFailure::policy("tlog-entry-kind"))?;
    let (expected_kind, expected_version) = REKOR_V2_KIND_VERSION;
    let expected_kind_version = serde_json::json!({
        KIND_MEMBER: expected_kind,
        VERSION_MEMBER: expected_version,
    });
    let has_rekor_v2_shape = entry.get(KIND_VERSION_MEMBER) == Some(&expected_kind_version);
    if !has_rekor_v2_shape {
        return Err(VerificationFailure::policy("tlog-entry-kind"));
    }
    let has_integrated_time = entry.contains_key(INTEGRATED_TIME_MEMBER);
    if has_integrated_time {
        return Err(VerificationFailure::policy("integrated-time-present"));
    }
    let has_inclusion_promise = entry.contains_key(INCLUSION_PROMISE_MEMBER);
    if has_inclusion_promise {
        return Err(VerificationFailure::policy("inclusion-promise-present"));
    }
    let timestamp_count = material
        .get(TIMESTAMP_VERIFICATION_DATA_MEMBER)
        .and_then(|data| data.get(RFC3161_TIMESTAMPS_MEMBER))
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or_default();
    let has_one_timestamp = timestamp_count == VERIFIED_TIMESTAMP_COUNT;
    if !has_one_timestamp {
        return Err(VerificationFailure::policy("timestamp-count"));
    }
    Ok(())
}

fn enforce_release_content(bundle: &Bundle) -> Result<(), VerificationFailure> {
    let SignatureContent::DsseEnvelope(envelope) = &bundle.content else {
        return Err(VerificationFailure::policy("content-unsupported"));
    };
    let has_supported_payload_type = envelope.payload_type == EXPECTED_PAYLOAD_TYPE;
    if !has_supported_payload_type {
        return Err(VerificationFailure::policy("content-unsupported"));
    }
    let (expected_kind, expected_version) = REKOR_V2_KIND_VERSION;
    let has_only_rekor_v2 = bundle
        .verification_material
        .tlog_entries
        .first()
        .is_some_and(|entry| {
            entry.kind_version.kind() == expected_kind
                && entry.kind_version.version() == expected_version
                && entry.integrated_time.is_none()
                && entry.inclusion_promise.is_none()
        });
    if !has_only_rekor_v2 {
        return Err(VerificationFailure::policy("tlog-entry-kind"));
    }
    let has_one_timestamp = bundle
        .verification_material
        .timestamp_verification_data
        .rfc3161_timestamps
        .len()
        == VERIFIED_TIMESTAMP_COUNT;
    if !has_one_timestamp {
        return Err(VerificationFailure::policy("timestamp-count"));
    }
    Ok(())
}

fn count_present(object: &Map<String, Value>, keys: &[&str]) -> usize {
    keys.iter().filter(|key| object.contains_key(**key)).count()
}

fn parse_complete_json(input: &str) -> Result<Value, serde_json::Error> {
    let mut deserializer = serde_json::Deserializer::from_str(input);
    let value = Value::deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(value)
}

pub(super) struct ReleaseStatement {
    subject_digest: Sha256Hash,
    predicate: ReleasePredicate,
}

struct ReleasePredicate {
    environment: Value,
    measurements: Value,
    source: Value,
    build: Value,
}

#[derive(Debug)]
pub(super) struct ReleaseSource {
    repository: String,
    commit: String,
    run_id: String,
    run_attempt: u64,
}

impl ReleaseStatement {
    fn from_value(payload: &Value) -> Result<Self, VerificationFailure> {
        let object = payload
            .as_object()
            .ok_or_else(|| VerificationFailure::policy("statement-malformed"))?;
        let statement_type = object.get(STATEMENT_TYPE_MEMBER).and_then(Value::as_str);
        let has_supported_statement_type = statement_type == Some(EXPECTED_STATEMENT_TYPE);
        if !has_supported_statement_type {
            return Err(VerificationFailure::policy("statement-unsupported"));
        }
        let subject = object
            .get(SUBJECT_MEMBER)
            .and_then(Value::as_array)
            .ok_or_else(|| VerificationFailure::policy("statement-malformed"))?;
        let has_subjects = !subject.is_empty();
        if !has_subjects {
            return Err(VerificationFailure::policy("statement-malformed"));
        }
        let subject_digests = subject
            .iter()
            .map(subject_sha256)
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| VerificationFailure::policy("statement-malformed"))?;
        let predicate_type = object.get(PREDICATE_TYPE_MEMBER).and_then(Value::as_str);
        let has_supported_predicate_type = predicate_type == Some(EXPECTED_PREDICATE_TYPE);
        if !has_supported_predicate_type {
            return Err(VerificationFailure::policy("statement-unsupported"));
        }
        let predicate = object
            .get(PREDICATE_MEMBER)
            .and_then(Value::as_object)
            .ok_or_else(|| VerificationFailure::policy("statement-malformed"))?;
        let subject_digest = Sha256Hash::from_hex(subject_digests[0])
            .map_err(|_| VerificationFailure::policy("statement-malformed"))?;
        Ok(Self {
            subject_digest,
            predicate: ReleasePredicate {
                environment: predicate
                    .get(ENVIRONMENT_MEMBER)
                    .cloned()
                    .unwrap_or(Value::Null),
                measurements: predicate
                    .get(MEASUREMENTS_MEMBER)
                    .cloned()
                    .unwrap_or(Value::Null),
                source: predicate.get(SOURCE_MEMBER).cloned().unwrap_or(Value::Null),
                build: predicate.get(BUILD_MEMBER).cloned().unwrap_or(Value::Null),
            },
        })
    }

    fn subject_digest(&self) -> Sha256Hash {
        self.subject_digest
    }

    pub(super) fn validate(
        &self,
        policy: ReleasePolicy,
    ) -> Result<ReleaseSource, VerificationFailure> {
        let environment_matches =
            self.predicate.environment.as_str() == Some(policy.environment.release_name());
        if !environment_matches {
            return Err(VerificationFailure::policy(
                "statement-environment-mismatch",
            ));
        }
        let measurements = well_formed_measurements(&self.predicate.measurements)
            .ok_or_else(|| VerificationFailure::policy("statement-malformed"))?;
        let expected_pcr0 = hex::encode(policy.measurements.pcr0);
        let expected_pcr1 = hex::encode(policy.measurements.pcr1);
        let expected_pcr2 = hex::encode(policy.measurements.pcr2);
        let measurements_match = measurements[0] == expected_pcr0.as_str()
            && measurements[1] == expected_pcr1.as_str()
            && measurements[2] == expected_pcr2.as_str();
        if !measurements_match {
            return Err(VerificationFailure::policy(
                "statement-measurement-mismatch",
            ));
        }
        read_release_source(&self.predicate.source, &self.predicate.build)
    }
}

fn subject_sha256(subject: &Value) -> Option<&str> {
    let subject = subject.as_object()?;
    let name = subject.get(SUBJECT_NAME_MEMBER).and_then(Value::as_str);
    let has_name = name.is_some_and(|name| !name.is_empty());
    if !has_name {
        return None;
    }
    let digest = subject.get(DIGEST_MEMBER).and_then(Value::as_object)?;
    let digest = digest.get(SHA256_MEMBER).and_then(Value::as_str)?;
    let is_sha256 = is_lower_hex(digest, SUBJECT_SHA256_HEX_LEN);
    is_sha256.then_some(digest)
}

fn well_formed_measurements(value: &Value) -> Option<[&str; 3]> {
    let measurements = value.as_object()?;
    let values = [
        measurements.get(PCR0_MEMBER).and_then(Value::as_str),
        measurements.get(PCR1_MEMBER).and_then(Value::as_str),
        measurements.get(PCR2_MEMBER).and_then(Value::as_str),
    ];
    let all_well_formed = values
        .into_iter()
        .all(|value| value.is_some_and(|value| is_lower_hex(value, MEASUREMENT_HEX_LEN)));
    if !all_well_formed {
        return None;
    }
    Some(values.map(|value| value.expect("every measurement was validated")))
}

fn read_release_source(
    source: &Value,
    build: &Value,
) -> Result<ReleaseSource, VerificationFailure> {
    // Source and build values are signed audit data, not policy inputs.
    // Their shapes make the resulting diagnostics useful without pinning
    // a repository, workflow run or other authorization identity.
    let source = source
        .as_object()
        .ok_or_else(|| VerificationFailure::policy("statement-malformed"))?;
    let build = build
        .as_object()
        .ok_or_else(|| VerificationFailure::policy("statement-malformed"))?;
    let repository = source.get(REPOSITORY_MEMBER).and_then(Value::as_str);
    let commit = source.get(COMMIT_MEMBER).and_then(Value::as_str);
    let run_id = build.get(RUN_ID_MEMBER).and_then(Value::as_str);
    let run_attempt = whole_safe_run_attempt(build.get(RUN_ATTEMPT_MEMBER));
    let repository_present = repository.is_some_and(|value| !value.is_empty());
    let commit_is_hash = commit.is_some_and(is_full_lower_hex_commit);
    let run_id_is_decimal = run_id.is_some_and(is_decimal);
    let attempt_is_count = run_attempt.is_some();
    let audit_data_is_well_formed =
        repository_present && commit_is_hash && run_id_is_decimal && attempt_is_count;
    if !audit_data_is_well_formed {
        return Err(VerificationFailure::policy("statement-malformed"));
    }
    Ok(ReleaseSource {
        repository: repository.expect("audit data was validated").to_owned(),
        commit: commit.expect("audit data was validated").to_owned(),
        run_id: run_id.expect("audit data was validated").to_owned(),
        run_attempt: run_attempt.expect("audit data was validated"),
    })
}

fn whole_safe_run_attempt(value: Option<&Value>) -> Option<u64> {
    // JSON notation does not change a number's value. Keep attempts within
    // the range in which every SDK can represent the integer exactly.
    let number = value?.as_number()?;
    if let Some(integer) = number.as_u64() {
        let is_count = (1..=MAX_SAFE_RUN_ATTEMPT).contains(&integer);
        return is_count.then_some(integer);
    }
    let number = number.as_f64()?;
    let is_whole = number.fract() == 0.0;
    let is_in_range = (1.0..=MAX_SAFE_RUN_ATTEMPT as f64).contains(&number);
    let is_count = is_whole && is_in_range;
    if !is_count {
        return None;
    }
    Some(number as u64)
}

fn is_lower_hex(value: &str, expected_len: usize) -> bool {
    let has_expected_length = value.len() == expected_len;
    let has_only_lowercase_hex = value
        .bytes()
        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
    has_expected_length && has_only_lowercase_hex
}

fn is_full_lower_hex_commit(commit: &str) -> bool {
    is_lower_hex(commit, SOURCE_COMMIT_HEX_LEN)
}

fn is_decimal(value: &str) -> bool {
    let is_not_empty = !value.is_empty();
    let has_only_digits = value.bytes().all(|byte| byte.is_ascii_digit());
    is_not_empty && has_only_digits
}

pub(super) fn decode_release_statement(
    bundle_bytes: &[u8],
) -> Result<(Value, ReleaseStatement), VerificationFailure> {
    let bundle_text = std::str::from_utf8(bundle_bytes)
        .map_err(|_| VerificationFailure::policy("bundle-malformed"))?;
    parse_without_duplicate_keys(bundle_text)?;
    let raw_bundle = parse_complete_json(bundle_text)
        .map_err(|_| VerificationFailure::policy("bundle-malformed"))?;
    let envelope = raw_bundle
        .get(DSSE_ENVELOPE_MEMBER)
        .and_then(Value::as_object)
        .ok_or_else(|| VerificationFailure::policy("content-unsupported"))?;
    let encoded_payload = envelope
        .get(PAYLOAD_MEMBER)
        .and_then(Value::as_str)
        .ok_or_else(|| VerificationFailure::policy("statement-malformed"))?;
    let payload = BASE64
        .decode(encoded_payload)
        .map_err(|_| VerificationFailure::policy("statement-malformed"))?;
    let payload_text = std::str::from_utf8(&payload)
        .map_err(|_| VerificationFailure::policy("statement-malformed"))?;
    parse_without_duplicate_keys(payload_text)?;
    let payload = parse_complete_json(payload_text)
        .map_err(|_| VerificationFailure::policy("json-syntax-error"))?;
    let statement = ReleaseStatement::from_value(&payload)?;
    Ok((payload, statement))
}

fn reason_code(error: &SigstoreError) -> &'static str {
    // Until every failure has a typed upstream variant, this compatibility
    // boundary depends on English message fragments. The complete 56-vector
    // corpus guards the mapping; unrecognized messages stay verification
    // failures rather than widening acceptance.
    let message = error.to_string();
    if let Some(code) = reason_for_message(&message) {
        return code;
    }
    match error {
        SigstoreError::SignatureInvalid(_) => "message-signature-invalid",
        SigstoreError::ArtifactMismatch(_) => "artifact-mismatch",
        SigstoreError::TransparencyLog(TransparencyLogError::InclusionProof(_)) => {
            "inclusion-proof-invalid"
        }
        SigstoreError::TransparencyLog(TransparencyLogError::InclusionPromise(_)) => {
            "inclusion-promise-invalid"
        }
        SigstoreError::TransparencyLog(TransparencyLogError::Checkpoint(_)) => {
            "checkpoint-signature-invalid"
        }
        SigstoreError::Timestamp(TimestampError::Invalid(_)) => "timestamp-signature-invalid",
        SigstoreError::Timestamp(TimestampError::NoVerifiedTimestamp(_)) => "timestamp-count",
        _ => "verifier-failed",
    }
}

fn reason_for_message(message: &str) -> Option<&'static str> {
    // Prefer the longest matching fragment so general suffixes cannot mask a
    // more specific failure from the corpus profile.
    const REASONS: &[(&str, &str)] = &[
        (
            "DSSE signature verification failed",
            "dsse-signature-invalid",
        ),
        (
            "inclusion proof verification failed",
            "inclusion-proof-invalid",
        ),
        (
            "bundle must have an inclusion proof",
            "inclusion-proof-missing",
        ),
        (
            "No valid Rekor signature found on checkpoint",
            "checkpoint-signature-invalid",
        ),
        (
            "No matching Rekor key found for checkpoint signature",
            "checkpoint-signature-invalid",
        ),
        (
            "checkpoint has no valid signature from the entry's log",
            "checkpoint-signature-invalid",
        ),
        (
            "does not match the entry's log",
            "checkpoint-origin-mismatch",
        ),
        (
            "Failed to verify timestamp signature",
            "timestamp-signature-invalid",
        ),
        (
            "CMS signer does not match any trusted TSA certificate",
            "timestamp-untrusted-authority",
        ),
        (
            "Timestamp message hash mismatch",
            "timestamp-message-mismatch",
        ),
        (
            "invalid type: null, expected a protobuf int64",
            "bundle-malformed",
        ),
        ("SET verification failed", "inclusion-promise-invalid"),
        (
            "bundles must have an inclusion promise",
            "inclusion-promise-missing",
        ),
        ("do not match its", "tlog-key-details-mismatch"),
        (
            "does not match key details",
            "tlog-digest-algorithm-mismatch",
        ),
        ("digest is 32 bytes, expected 48", "tlog-digest-malformed"),
        (
            "artifact hash mismatch for hashedrekord entry",
            "tlog-digest-mismatch",
        ),
        (
            "public key in Rekor entry does not match managed key",
            "tlog-key-mismatch",
        ),
        (
            "message digest in bundle does not match artifact hash",
            "artifact-mismatch",
        ),
        ("signature verification failed", "message-signature-invalid"),
        (
            "DSSE signature in bundle does not match signature in Rekor entry",
            "tlog-signature-mismatch",
        ),
    ];
    REASONS
        .iter()
        .filter(|(fragment, _)| message.contains(fragment))
        .max_by_key(|(fragment, _)| fragment.len())
        .map(|(_, code)| *code)
}

pub(super) fn parse_without_duplicate_keys(input: &str) -> Result<(), VerificationFailure> {
    parse_without_duplicate_keys_bytes(input.as_bytes())
}

fn parse_without_duplicate_keys_bytes(input: &[u8]) -> Result<(), VerificationFailure> {
    let duplicate_key_detected = Cell::new(false);
    let mut deserializer = serde_json::Deserializer::from_slice(input);
    let parsed = NoDuplicateKeys {
        duplicate_key_detected: &duplicate_key_detected,
    }
    .deserialize(&mut deserializer);
    if parsed.is_err() {
        let category = if duplicate_key_detected.get() {
            "duplicate-json-key"
        } else {
            "json-syntax-error"
        };
        return Err(VerificationFailure::policy(category));
    }
    deserializer
        .end()
        .map_err(|_| VerificationFailure::policy("json-syntax-error"))
}

#[derive(Clone, Copy)]
struct NoDuplicateKeys<'a> {
    duplicate_key_detected: &'a Cell<bool>,
}

impl<'de> DeserializeSeed<'de> for NoDuplicateKeys<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for NoDuplicateKeys<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("JSON without duplicate member names")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut seen = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            let is_new_key = seen.insert(key);
            if !is_new_key {
                self.duplicate_key_detected.set(true);
                return Err(serde::de::Error::custom("duplicate member"));
            }
            map.next_value_seed(self)?;
        }
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element_seed(self)?.is_some() {}
        Ok(())
    }

    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(())
    }
}
