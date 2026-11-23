# Release statement vectors

`vectors.json` holds release statements and the answer every Maple SDK must
give for each one. The TypeScript and Rust SDKs each keep a byte-identical
copy and pin its SHA-256 in their tests, so the two cannot drift apart.

A vector's `payload` is the base64 of a DSSE payload whose signature is taken
as already verified. The statement check is asked whether it approves the
environment and measurements in `expected_environment` and
`expected_measurements`. `expected.result` is `accept` or `reject`, and a
rejection names its `category`.

Every vector but four is an edit of the statement signed for the published
Development release. `generate.py` makes them; rerun it after changing the
list:

    python3 -I generate.py ../../../src/lib/releaseEvidence/published/dev-b422d5fc.sigstore.json > vectors.json

## The order of the checks

A statement can be wrong in several ways at once. Both SDKs report the first
of these that fails:

1. The payload is UTF-8. Otherwise `statement-malformed`.
2. It is JSON (`json-syntax-error`) and names no member twice in any object
   (`duplicate-json-key`).
3. It is a JSON object. Otherwise `statement-malformed`.
4. `_type` is `https://in-toto.io/Statement/v1`. Otherwise
   `statement-unsupported`.
5. `subject` is a non-empty array whose entries each have a non-empty `name`
   and a `digest.sha256` of 64 lowercase hex characters. Otherwise
   `statement-malformed`.
6. `predicateType` is `https://maple.ai/attestations/enclave-release/v1`.
   Otherwise `statement-unsupported`.
7. `predicate` is an object. Otherwise `statement-malformed`.
8. `predicate.environment` equals the expected environment. Otherwise
   `statement-environment-mismatch`.
9. `predicate.measurements` is an object whose `PCR0`, `PCR1` and `PCR2` are
   each 96 lowercase hex characters. Otherwise `statement-malformed`.
10. Those three equal the expected measurements. Otherwise
    `statement-measurement-mismatch`.
11. `predicate.source` and `predicate.build` are objects; `source.repository`
    is a non-empty string; `source.commit` is 40 lowercase hex characters;
    `build.runId` is a string of decimal digits; `build.runAttempt` is a
    number. Otherwise `statement-malformed`.

Members the list does not name are ignored.

## Numbers

`build.runAttempt` is judged by its value, not by how it is written: `1`,
`1.0` and `1e0` are the same number. It must be a whole number from 1 to
9007199254740991, the largest every JSON reader holds exactly.
