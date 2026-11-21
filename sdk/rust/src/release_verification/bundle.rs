use crate::pcr::Pcr0Environment;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde::de::{Deserialize, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};
use sigstore_trust_root::TrustedRoot;
use sigstore_types::{
    Artifact, Bundle, DerPublicKey, MediaType, Sha256Hash, Sha384Hash, SignatureContent,
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
const TLOG_ENTRIES_MEMBER: &str = "tlogEntries";
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

#[cfg(test)]
pub(super) const SUBJECT_MEMBER: &str = "subject";
#[cfg(test)]
pub(super) const DIGEST_MEMBER: &str = "digest";
#[cfg(test)]
pub(super) const SHA256_MEMBER: &str = "sha256";
#[cfg(test)]
pub(super) const PREDICATE_MEMBER: &str = "predicate";
#[cfg(test)]
pub(super) const ENVIRONMENT_MEMBER: &str = "environment";
#[cfg(test)]
pub(super) const SOURCE_MEMBER: &str = "source";
#[cfg(test)]
pub(super) const REPOSITORY_MEMBER: &str = "repository";
#[cfg(test)]
pub(super) const WORKFLOW_MEMBER: &str = "workflow";
#[cfg(test)]
pub(super) const BUILD_MEMBER: &str = "build";
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
) -> Result<VerificationEvidence, VerificationFailure> {
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
    statement.validate(release_policy)?;
    Ok(evidence)
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
    Ok(VerificationEvidence {
        tlog_verified: result.tlog_verified(),
    })
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

#[derive(serde::Deserialize)]
pub(super) struct ReleaseStatement {
    #[serde(rename = "_type")]
    statement_type: String,
    subject: Vec<ReleaseSubject>,
    #[serde(rename = "predicateType")]
    predicate_type: String,
    predicate: ReleasePredicate,
}

#[derive(serde::Deserialize)]
struct ReleaseSubject {
    #[serde(rename = "name")]
    _name: String,
    digest: SubjectDigest,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SubjectDigest {
    sha256: String,
}

#[derive(serde::Deserialize)]
struct ReleasePredicate {
    environment: String,
    measurements: StatementMeasurements,
}

#[derive(serde::Deserialize)]
struct StatementMeasurements {
    #[serde(rename = "PCR0")]
    pcr0: String,
    #[serde(rename = "PCR1")]
    pcr1: String,
    #[serde(rename = "PCR2")]
    pcr2: String,
}

impl ReleaseStatement {
    fn subject_digest(&self) -> Sha256Hash {
        Sha256Hash::from_hex(&self.subject[0].digest.sha256)
            .expect("subject SHA-256 was validated while parsing")
    }

    pub(super) fn validate(&self, policy: ReleasePolicy) -> Result<(), VerificationFailure> {
        let has_supported_types = self.statement_type == EXPECTED_STATEMENT_TYPE
            && self.predicate_type == EXPECTED_PREDICATE_TYPE;
        if !has_supported_types {
            return Err(VerificationFailure::policy("statement-unsupported"));
        }
        let environment_matches = self.predicate.environment == policy.environment.release_name();
        if !environment_matches {
            return Err(VerificationFailure::policy(
                "statement-environment-mismatch",
            ));
        }
        let pcr0 = Sha384Hash::from_hex(&self.predicate.measurements.pcr0)
            .map_err(|_| VerificationFailure::policy("statement-measurement-mismatch"))?;
        let pcr1 = Sha384Hash::from_hex(&self.predicate.measurements.pcr1)
            .map_err(|_| VerificationFailure::policy("statement-measurement-mismatch"))?;
        let pcr2 = Sha384Hash::from_hex(&self.predicate.measurements.pcr2)
            .map_err(|_| VerificationFailure::policy("statement-measurement-mismatch"))?;
        let measurements_match = pcr0.as_bytes() == &policy.measurements.pcr0
            && pcr1.as_bytes() == &policy.measurements.pcr1
            && pcr2.as_bytes() == &policy.measurements.pcr2;
        if !measurements_match {
            return Err(VerificationFailure::policy(
                "statement-measurement-mismatch",
            ));
        }
        Ok(())
    }
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
    parse_without_duplicate_keys_bytes(&payload)?;
    let payload = serde_json::from_slice::<Value>(&payload)
        .map_err(|_| VerificationFailure::policy("statement-malformed"))?;
    let object = payload
        .as_object()
        .ok_or_else(|| VerificationFailure::policy("statement-malformed"))?;
    let statement_type = object.get(STATEMENT_TYPE_MEMBER).and_then(Value::as_str);
    let predicate_type = object.get(PREDICATE_TYPE_MEMBER).and_then(Value::as_str);
    let has_supported_types = statement_type == Some(EXPECTED_STATEMENT_TYPE)
        && predicate_type == Some(EXPECTED_PREDICATE_TYPE);
    if !has_supported_types {
        return Err(VerificationFailure::policy("statement-unsupported"));
    }
    let statement: ReleaseStatement = serde_json::from_value(payload.clone())
        .map_err(|_| VerificationFailure::policy("statement-malformed"))?;
    let has_one_subject = statement.subject.len() == 1;
    if !has_one_subject {
        return Err(VerificationFailure::policy("statement-malformed"));
    }
    Sha256Hash::from_hex(&statement.subject[0].digest.sha256)
        .map_err(|_| VerificationFailure::policy("statement-malformed"))?;
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
