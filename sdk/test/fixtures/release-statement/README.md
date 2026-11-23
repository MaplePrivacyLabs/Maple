# Release statement vectors

`vectors.json` holds release statements and the answer every Maple SDK must
give for each one. The TypeScript and Rust SDKs each keep a byte-identical
copy and pin its SHA-256 in their tests, so the two cannot drift apart.

A vector's `payload` is the base64 of a DSSE payload whose signature is taken
as already verified. The statement check is asked whether it approves the
environment and measurements in `expected_environment` and
`expected_measurements`. `expected.result` is `accept` or `reject`, and a
rejection names its `category`.

Every vector but four is built from the statement signed for the published
Development release. `generate.py` makes them; rerun it after changing the
list:

    python3 -I generate.py ../../../src/lib/releaseEvidence/published/dev-b422d5fc.sigstore.json > vectors.json

## The order of the checks

A statement can be wrong in several ways at once. Both SDKs report the first
of these that fails:

1. The payload is UTF-8. Otherwise `statement-malformed`.
2. It is JSON (`json-syntax-error`). The whole text is checked first, so
   text that is not JSON gets this answer even if it also repeats a member.
   A leading byte order mark is not JSON.
3. It names no member twice in any object (`duplicate-json-key`).
4. It is a JSON object. Otherwise `statement-malformed`.
5. `_type` is `https://in-toto.io/Statement/v1`. Otherwise
   `statement-unsupported`.
6. `subject` is a non-empty array whose entries each have a non-empty `name`
   and a `digest.sha256` of 64 lowercase hex characters. Otherwise
   `statement-malformed`.
7. `predicateType` is `https://maple.ai/attestations/enclave-release/v1`.
   Otherwise `statement-unsupported`.
8. `predicate` is an object. Otherwise `statement-malformed`.
9. `predicate.environment` equals the expected environment. Otherwise
   `statement-environment-mismatch`.
10. `predicate.measurements` is an object whose `PCR0`, `PCR1` and `PCR2` are
    each 96 lowercase hex characters. Otherwise `statement-malformed`.
11. Those three equal the expected measurements. Otherwise
    `statement-measurement-mismatch`.
12. `predicate.source` and `predicate.build` are objects; `source.repository`
    is a non-empty string; `source.commit` is 40 lowercase hex characters;
    `build.runId` is a string of decimal digits; `build.runAttempt` is a
    number. Otherwise `statement-malformed`.

Members the list does not name are ignored.

## Numbers

`build.runAttempt` is judged by its value, not by how it is written: `1`,
`1.0` and `1e0` are the same number. It must be a whole number from 1 to
9007199254740991, the largest every JSON reader holds exactly.

## Known differences

The two SDKs must agree on what they accept. They need not give the same
failure category for input that both reject.

### Rejected by both, with different categories

The two verifier libraries read a bundle in a different order. The Rust
verifier parses the payload as a statement with a subject digest before it
checks the signature; the TypeScript verifier checks the signature first. So
when a bundle's signature does not verify, or the bundle is corrupt, the
TypeScript SDK answers `dsse-signature-invalid` where the Rust SDK may name
whatever is wrong with the payload instead: `json-syntax-error`,
`statement-malformed`, `statement-unsupported` or `bundle-malformed`.

For a statement whose signature verifies, the categories are the same and
the vectors pin them.

### Accepted by one SDK only

Three forms are read by the TypeScript SDK and rejected by the Rust SDK as
`json-syntax-error`. Each needs a statement that Maple's own key signed in a
form the release pipeline never writes:

- a number too large for a double, such as `1e400`, anywhere in the text
  (as `build.runAttempt` the TypeScript SDK also rejects it, as
  `statement-malformed`);
- a lone surrogate escape, such as `"\ud800"`, in a member the checks ignore;
- objects or arrays nested more than 128 deep.
