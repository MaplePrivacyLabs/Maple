use model_tokenizer::{count_segment, encode_segment, provenance, SpecialTokenPolicy, TokenizerId};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Deserialize)]
struct Corpus {
    fixtures: Vec<Fixture>,
}

#[derive(Deserialize)]
struct Fixture {
    name: String,
    text: String,
    expected: BTreeMap<String, Expected>,
}

#[derive(Deserialize)]
struct Expected {
    raw: Vec<u32>,
    ordinary: Vec<u32>,
}

#[test]
fn all_catalog_text_ids_match_independent_python_engines() {
    let corpus: Corpus = serde_json::from_str(include_str!("fixtures/text_goldens.json")).unwrap();
    assert_eq!(corpus.fixtures.len(), 12);
    for fixture in corpus.fixtures {
        for id in TokenizerId::ALL {
            let expected = &fixture.expected[provenance(id).asset_id];
            assert_eq!(
                encode_segment(id, &fixture.text, SpecialTokenPolicy::RawPipeline).unwrap(),
                expected.raw,
                "raw model={} fixture={}",
                id.model(),
                fixture.name,
            );
            assert_eq!(
                encode_segment(id, &fixture.text, SpecialTokenPolicy::Ordinary).unwrap(),
                expected.ordinary,
                "ordinary model={} fixture={}",
                id.model(),
                fixture.name,
            );
            assert_eq!(
                count_segment(id, &fixture.text, SpecialTokenPolicy::RawPipeline).unwrap(),
                expected.raw.len(),
                "raw count model={} fixture={}",
                id.model(),
                fixture.name,
            );
            assert_eq!(
                count_segment(id, &fixture.text, SpecialTokenPolicy::Ordinary).unwrap(),
                expected.ordinary.len(),
                "ordinary count model={} fixture={}",
                id.model(),
                fixture.name,
            );
        }
    }
}

#[derive(Deserialize)]
struct BoundaryCorpus {
    fixtures: Vec<BoundaryFixture>,
}

#[derive(Deserialize)]
struct BoundaryFixture {
    name: String,
    unit: String,
    repeats: usize,
    suffix: String,
    raw: ExpectedHash,
    ordinary: ExpectedHash,
}

#[derive(Deserialize)]
struct ExpectedHash {
    count: usize,
    ids_le_sha256: String,
}

#[test]
fn kimi_wrapper_boundaries_match_pinned_python_reference() {
    let corpus: BoundaryCorpus =
        serde_json::from_str(include_str!("fixtures/kimi_boundaries.json")).unwrap();
    for fixture in corpus.fixtures {
        let text = fixture.unit.repeat(fixture.repeats) + &fixture.suffix;
        for (policy, expected) in [
            (SpecialTokenPolicy::RawPipeline, &fixture.raw),
            (SpecialTokenPolicy::Ordinary, &fixture.ordinary),
        ] {
            let ids = encode_segment(TokenizerId::KimiK3, &text, policy).unwrap();
            assert_eq!(
                ids.len(),
                expected.count,
                "count {} {policy:?}",
                fixture.name
            );
            let mut digest = Sha256::new();
            for id in ids {
                digest.update(id.to_le_bytes());
            }
            assert_eq!(
                format!("{:x}", digest.finalize()),
                expected.ids_le_sha256,
                "IDs {} {policy:?}",
                fixture.name
            );
        }
    }
}
