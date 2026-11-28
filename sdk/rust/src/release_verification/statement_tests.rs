use super::bundle::{
    decode_release_statement, MeasurementTuple, ReleasePolicy, VerificationFailure,
    DSSE_ENVELOPE_MEMBER, PAYLOAD_MEMBER,
};
use crate::pcr::Pcr0Environment;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde::Deserialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

const STATEMENT_VECTORS: &[u8] =
    include_bytes!("../../tests/fixtures/release-statement/vectors.json");
const STATEMENT_VECTORS_SHA256: &str =
    "cc1e934761241660e9eb3675f8fd43ef786cdcee7c8d9e20e8b17abc55dbce68";
const STATEMENT_VECTOR_COUNT: usize = 50;

#[derive(Deserialize)]
struct VectorFile {
    expected_environment: String,
    expected_measurements: ExpectedMeasurements,
    vectors: Vec<StatementVector>,
}

#[derive(Deserialize)]
struct ExpectedMeasurements {
    #[serde(rename = "PCR0")]
    pcr0: String,
    #[serde(rename = "PCR1")]
    pcr1: String,
    #[serde(rename = "PCR2")]
    pcr2: String,
}

#[derive(Deserialize)]
struct StatementVector {
    id: String,
    shows: String,
    payload: String,
    expected: ExpectedOutcome,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "result", rename_all = "lowercase")]
enum ExpectedOutcome {
    Accept,
    Reject { category: String },
}

fn unsigned_statement_carrier(payload: &[u8]) -> Vec<u8> {
    // Statement vectors exercise the policy after signature verification, so
    // they do not need or reproduce the release bundle's cryptographic proof.
    let mut envelope = Map::new();
    envelope.insert(
        PAYLOAD_MEMBER.to_owned(),
        Value::String(BASE64.encode(payload)),
    );
    let mut bundle = Map::new();
    bundle.insert(DSSE_ENVELOPE_MEMBER.to_owned(), Value::Object(envelope));
    serde_json::to_vec(&Value::Object(bundle)).unwrap()
}

fn policy(vectors: &VectorFile) -> ReleasePolicy {
    let environment = match vectors.expected_environment.as_str() {
        "development" => Pcr0Environment::Development,
        "production" => Pcr0Environment::Production,
        other => panic!("unsupported vector environment {other}"),
    };
    let expected = &vectors.expected_measurements;
    let measurements = MeasurementTuple {
        pcr0: hex::decode(&expected.pcr0).unwrap().try_into().unwrap(),
        pcr1: hex::decode(&expected.pcr1).unwrap().try_into().unwrap(),
        pcr2: hex::decode(&expected.pcr2).unwrap().try_into().unwrap(),
    };
    ReleasePolicy::new(environment, measurements)
}

fn run(payload: &[u8], policy: ReleasePolicy) -> Result<(), VerificationFailure> {
    let carrier = unsigned_statement_carrier(payload);
    decode_release_statement(&carrier)
        .and_then(|(_, statement)| statement.validate(policy))
        .map(|_| ())
}

#[test]
fn release_statement_all_50_vectors_with_ordered_categories() {
    let digest = hex::encode(Sha256::digest(STATEMENT_VECTORS));
    assert_eq!(digest, STATEMENT_VECTORS_SHA256);
    let vectors: VectorFile = serde_json::from_slice(STATEMENT_VECTORS).unwrap();
    assert_eq!(vectors.vectors.len(), STATEMENT_VECTOR_COUNT);
    let policy = policy(&vectors);
    let mut passed = 0;
    let mut failures = Vec::new();
    for vector in &vectors.vectors {
        let payload = BASE64.decode(&vector.payload).unwrap();
        let actual = run(&payload, policy);
        let actual_description = format!("{actual:?}");
        let matches = match (&vector.expected, &actual) {
            (ExpectedOutcome::Accept, Ok(())) => true,
            (ExpectedOutcome::Reject { category }, Err(error)) => category == error.code(),
            _ => false,
        };
        if matches {
            passed += 1;
        } else {
            failures.push(format!(
                "{} ({}): expected {:?}, received {actual_description}",
                vector.id, vector.shows, vector.expected
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{passed}/{STATEMENT_VECTOR_COUNT} vectors matched; mismatches: {}",
        failures.join(", ")
    );
}
