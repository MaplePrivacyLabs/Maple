use super::bundle::{
    verify_managed_key_bundle, DIGEST_MEMBER, DSSE_ENVELOPE_MEMBER, DSSE_PAYLOAD_TYPE_MEMBER,
    EXPECTED_PAYLOAD_TYPE, PAYLOAD_MEMBER, SHA256_MEMBER, SUBJECT_MEMBER,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde::Deserialize;
use serde_json::Value;
use sigstore_trust_root::TrustedRoot;
use sigstore_types::{Artifact, DerPublicKey, Sha256Hash};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
struct Manifest {
    vectors: Vec<VectorSpec>,
}

#[derive(Deserialize)]
struct VectorSpec {
    id: String,
    bundle: PathBuf,
    key: PathBuf,
    artifact: PathBuf,
    trusted_root: PathBuf,
    parent: Option<String>,
    expected: Expectations,
}

#[derive(Deserialize)]
struct Expectations {
    profile: ExpectedProfile,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "result", rename_all = "lowercase")]
enum ExpectedProfile {
    Accept,
    Reject { code: String },
}

struct LoadedVector {
    bundle_json: String,
    key: DerPublicKey,
    artifact: Vec<u8>,
    root: TrustedRoot,
}

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/managed-key")
}

fn manifest() -> Manifest {
    serde_json::from_str(&fs::read_to_string(corpus_dir().join("manifest.json")).unwrap()).unwrap()
}

fn load(spec: &VectorSpec) -> LoadedVector {
    let directory = corpus_dir();
    LoadedVector {
        bundle_json: fs::read_to_string(directory.join(&spec.bundle)).unwrap(),
        key: DerPublicKey::from_pem(&fs::read_to_string(directory.join(&spec.key)).unwrap())
            .unwrap(),
        artifact: fs::read(directory.join(&spec.artifact)).unwrap(),
        root: TrustedRoot::from_file(directory.join(&spec.trusted_root)).unwrap(),
    }
}

fn artifact<'a>(bundle_json: &str, artifact: &'a [u8]) -> Artifact<'a> {
    let bundle: Value = serde_json::from_str(bundle_json).unwrap();
    let envelope = &bundle[DSSE_ENVELOPE_MEMBER];
    let has_supported_payload_type = envelope[DSSE_PAYLOAD_TYPE_MEMBER] == EXPECTED_PAYLOAD_TYPE;
    if !has_supported_payload_type {
        return Artifact::from(artifact);
    }
    let Some(payload) = envelope[PAYLOAD_MEMBER]
        .as_str()
        .and_then(|payload| BASE64.decode(payload).ok())
        .and_then(|payload| serde_json::from_slice::<Value>(&payload).ok())
    else {
        return Artifact::from(artifact);
    };
    let Some(hash) = payload[SUBJECT_MEMBER][0][DIGEST_MEMBER][SHA256_MEMBER]
        .as_str()
        .and_then(|hash| Sha256Hash::from_hex(hash).ok())
    else {
        return Artifact::from(artifact);
    };
    Artifact::from_digest(hash)
}

fn run(spec: &VectorSpec) -> Result<bool, &'static str> {
    let vector = load(spec);
    verify_managed_key_bundle(
        vector.bundle_json.as_bytes(),
        &vector.key,
        &vector.root,
        artifact(&vector.bundle_json, &vector.artifact),
    )
    .map(|evidence| evidence.tlog_verified)
    .map_err(|error| error.code())
}

#[test]
fn managed_key_corpus_all_56_vectors_with_profile_reason_codes() {
    let manifest = manifest();
    assert_eq!(manifest.vectors.len(), 56);
    let by_id: std::collections::HashMap<_, _> = manifest
        .vectors
        .iter()
        .map(|spec| (spec.id.as_str(), spec))
        .collect();
    let mut failures = Vec::new();
    let mut accepted_count = 0;
    let mut rejected_count = 0;
    for spec in &manifest.vectors {
        if let Some(parent) = spec.parent.as_deref() {
            let parent_spec = by_id[parent];
            let has_accepted_parent =
                matches!(parent_spec.expected.profile, ExpectedProfile::Accept);
            if !has_accepted_parent {
                failures.push(format!("{} has unaccepted parent {parent}", spec.id));
            }
        }
        let actual = run(spec);
        match (&spec.expected.profile, actual) {
            (ExpectedProfile::Accept, Ok(tlog_verified)) => {
                accepted_count += 1;
                if !tlog_verified {
                    failures.push(format!("{} accepted without verified tlog", spec.id));
                }
            }
            (ExpectedProfile::Reject { code }, Err(actual_code)) => {
                rejected_count += 1;
                let has_expected_rejection_code = *code == actual_code;
                if !has_expected_rejection_code {
                    failures.push(format!(
                        "{} expected {code}, received {actual_code}",
                        spec.id
                    ));
                }
            }
            (expected, actual) => failures.push(format!(
                "{} expected {expected:?}, received {actual:?}",
                spec.id
            )),
        }
    }
    assert_eq!(accepted_count, 9);
    assert_eq!(rejected_count, 47);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
