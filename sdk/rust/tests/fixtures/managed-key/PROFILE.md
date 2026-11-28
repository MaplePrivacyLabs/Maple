# Managed-key profile (MK)

Some vectors target a managed-key verifier profile beyond what this library
checks. Their library outcomes are recorded separately in the manifest. This
profile is a test contract, not library behaviour.
`crates/sigstore-verify/tests/managed_key_corpus.rs` contains a test-only
reference implementation that reports the first violated clause. Clause IDs
are stable; the table is ordered by the checks.

The profile verifies a bundle against one approved public key that the verifier
already holds. Policy about the signed statement itself (predicate type, subject,
predicate fields) belongs to the application and is not part of this profile.

| Clause | Requirement |
| --- | --- |
| MK-4 | **Unambiguous encoding.** The bundle JSON has no duplicate member names at any level, and no two variants of the same protobuf `oneof` (for example both `dsseEnvelope` and `messageSignature`). An ordinary parser silently keeps one of them, so the ambiguity must be rejected before parsing. (Checked first.) |
| MK-1 | **Media type.** `mediaType` is exactly `application/vnd.dev.sigstore.bundle.v0.3+json`. A missing or older media type is rejected, so relabelling a bundle cannot change which evidence is required. |
| MK-2 | **Verification material.** Exactly one variant, `publicKey`. Its hint is only a lookup hint; the key used is the verifier's approved key, never one taken from the bundle. Certificates and chains are rejected. |
| MK-3 | **Content.** A DSSE envelope with exactly one signature, or a `messageSignature` over the artifact. |
| MK-5 | **One Rekor v2 entry.** Exactly one transparency log entry, of kind `hashedrekord` version `0.0.2`. Rekor v1 entries are rejected, and so is any additional entry, with or without its own inclusion proof. |
| MK-6 | **No Rekor v1 metadata.** The entry has no `integratedTime` member and no `inclusionPromise` member, whatever their value: a present zero, `"0"` or `null` is rejected too. Rekor v2 issues neither. |
| MK-7 | **One timestamp.** Exactly one RFC 3161 timestamp, which verifies against a timestamp authority in the trusted root and covers this bundle's signature. A missing, duplicated, altered, untrusted or transplanted timestamp is rejected. No time-based policy is applied to the verified time. |
| MK-8 | **Library verification.** The bundle verifies with `PublicKeyVerificationPolicy::new(approved_key)` with transparency log verification on and `require_checkpoint_log_binding()`. That covers the DSSE signature, the binding of the log entry's key, signature and digest (including key details, digest algorithm and length), the inclusion proof, and a checkpoint signed by the entry's own log. Witness cosignatures on the checkpoint are neither verified nor required. |

A library failure caused by the timestamp is reported as MK-7; any other library
failure as MK-8. Each profile rejection in the manifest also carries a
language-neutral reason code (README.md, "Reason codes"): the clause's own code
for MK-1 to MK-6 and the presence rule of MK-7, otherwise the code of the
library error.
