use super::bundle::{parse_without_duplicate_keys, MeasurementTuple};
use super::trust::{approval_key, embedded_trusted_root};
use super::{Pcr0Environment, ReleaseVerifier};

fn measurements() -> MeasurementTuple {
    MeasurementTuple {
        pcr0: [0x01; 48],
        pcr1: [0x02; 48],
        pcr2: [0x03; 48],
    }
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
fn production_without_an_approval_key_fails_only_release_verification() {
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
fn nested_json_syntax_errors_do_not_look_like_duplicate_members() {
    let nested = parse_without_duplicate_keys(r#"{"outer":{"inner":[1,]}}"#).unwrap_err();
    assert_eq!(nested.code(), "json-syntax-error");
    let duplicate = parse_without_duplicate_keys(r#"{"outer":{"inner":1,"inner":2}}"#).unwrap_err();
    assert_eq!(duplicate.code(), "duplicate-json-key");
}
