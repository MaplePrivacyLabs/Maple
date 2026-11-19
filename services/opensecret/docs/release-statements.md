# Enclave release statements

An enclave release statement is Maple's signed approval of one OpenSecret EIF
for one environment. It binds the EIF's SHA-256 digest to the environment it
was approved for, its source commit and its PCR0, PCR1 and PCR2. It is signed
with that environment's own AWS KMS key through Cosign, recorded in Sigstore's
public Rekor v2 transparency log, timestamped by Sigstore's public timestamp
authority, and published as a standard Sigstore bundle at a fixed public URL
keyed by PCR0.

Only Dev statements exist so far. They are produced by
[`opensecret-release-statement-dev.yml`](../../../.github/workflows/opensecret-release-statement-dev.yml)
(`OpenSecret Dev release statement`). Production approvals still use the
legacy signing job in `opensecret-eif-release.yml`; see
[EIF release workflow](nitro-deploy.md#eif-release-workflow).

## Statement

The statement is an in-toto v1 statement with one subject, `image.eif` with the
EIF's SHA-256 digest, and predicate type
`https://maple.ai/attestations/enclave-release/v1`. Its predicate:

```json
{
  "environment": "development",
  "source": {
    "repository": "https://github.com/MaplePrivacyLabs/Maple",
    "commit": "<40-character source commit>"
  },
  "measurements": {
    "PCR0": "<96 hex characters>",
    "PCR1": "<96 hex characters>",
    "PCR2": "<96 hex characters>"
  },
  "build": {
    "runId": "<release workflow run ID, as a string>",
    "runAttempt": 1
  }
}
```

Build mode `dev` signs `development` and `prod` signs `production`; the
mapping is fixed in the producer. A client must choose the public key and host
from its own configured environment, then check `environment` and the three
PCRs; it must never take the environment from a downloaded bundle. A Dev-key
statement then cannot approve Production even if it claims `production`.
`source` and `build` are signed audit data. Every value is derived from the
verified build artifact and its provenance, never from operator input.

The build's `handoff.json` is internal to CI. It appears in no statement or
bundle.

## Running the Dev workflow

Dispatch `opensecret-release-statement-dev.yml` on `master` with one input,
the run ID of a successful `OpenSecret EIF release` run built for `dev`. Its
single job waits for approval of the protected `pcr-signing-dev` environment
(required reviewers, `master` only). That one approval covers both signatures
for the same artifact. The job then:

1. **Refuses to start** if the fixed signing identity in the workflow is
   incomplete: the KMS key ARN or signer role ARN is a placeholder or not the
   expected shape, or the reviewed public key file is not a file the
   checked-out commit tracks, differs from that commit's content, is reached
   through a symlink, or is not a P-384 public key.
2. **Verifies the build.** The run must be `opensecret-eif-release.yml`,
   dispatched on `master` in this public repository, completed successfully,
   and its commit must be in the checked-out `master` history. The artifact
   `opensecret-eif-dev-<commit>` must belong to that run and contain exactly
   `image.eif`, `pcr.json`, `handoff.json` and `SHA256SUMS`, all matching.
   GitHub's build attestation from that workflow, commit and run must cover
   all three files, and `handoff.json` must say `dev` and agree with the EIF
   digest and measurements. Any earlier successful master build qualifies,
   but its artifact is never substituted or rebuilt. Builds from before
   `handoff.json` was listed in `SHA256SUMS` have no attested build mode and
   are refused.
3. **Signs the legacy PCR0 approval** with the existing key, exactly as the
   release workflow's signing job does, so installed clients keep working.
4. **Signs the release statement** with `cosign attest-blob` and the Dev KMS
   key, using the pinned Cosign release and signing configuration below. AWS
   access comes from the job's GitHub OIDC token, exchanged for short-lived
   credentials of the Dev signer role; it reaches only the Cosign process.
   Before signing, the job compares the KMS public key with the reviewed key
   and stops on a mismatch.
5. **Verifies the bundle** before anything is published: Cosign verifies it
   against the reviewed public key, the pinned trusted root and the exact EIF,
   and the producer requires the evidence profile below.
6. **Retains** the EIF, its build files and the original bundle as the
   workflow artifact `opensecret-approved-dev-<commit>-<run>-<attempt>`.
   GitHub keeps it for 90 days at most, so it is not durable rollback
   retention; copy the EIF and its bundle to deployment storage for that.
7. **Publishes** the bundle to the `maple-attestations-dev` bucket as
   `<PCR0>.json` with `Content-Type: application/json` and
   `Cache-Control: public, max-age=300`, reads it back from
   `https://attestations-dev.maple.ai/<PCR0>.json` with a browser `Origin`,
   checks the bytes, content type, cache policy and CORS header, and verifies
   the downloaded copy again.
8. **Pushes the legacy approval branch** for an ordinary review pull request,
   as described in [the EIF release workflow](nitro-deploy.md#eif-release-workflow).
   Signing an earlier build from newer `master` code uses the branch
   `opensecret/pcr-approval-<build commit>-<checkout commit>`.

Any failure stops the job; later steps never run.

### Evidence profile

The producer publishes only a bundle that has all of:

- media type `application/vnd.dev.sigstore.bundle.v0.3+json`, a managed public
  key (no certificate) and one DSSE envelope whose payload is byte-for-byte the
  prepared statement, with one signature;
- exactly one transparency log entry, a Rekor v2 `hashedrekord` 0.0.2 entry from
  the configured log whose digest is SHA-384 of the DSSE pre-authentication
  encoding and whose key is the reviewed P-384 key;
- a Merkle inclusion proof and a checkpoint signed by that log (witness
  cosignature lines may follow and are not required);
- no integrated time and no inclusion promise, so no Rekor v1 entry;
- exactly one RFC 3161 timestamp over this signature.

### Publication and reruns

Uploads write one object, `<PCR0>.json`, and never delete others. A rerun for
the same build overwrites it with its newly signed bundle. A new build whose
measurements are already approved is accepted: the legacy step finds nothing
to sign, and its statement replaces the object for those PCRs. The replaced
bundle stays with its EIF in the earlier run's retained artifact, so
deployment can still check the exact EIF digest while that artifact lasts.
A failed upload or a readback that never returns the uploaded bytes fails the
job and is not reported as published.

Every signing attempt that reaches Rekor makes a permanent public entry, even
if the job fails afterwards. The entry holds only a digest of the statement, the
signature and the public key. Rerunning signs again and creates a new entry.

## Pinned tools and trust material

| Input | Value |
| --- | --- |
| Cosign | v3.1.4, source commit `0e48502cf5ef34e2e60bbd3617aa19945712f705`. Earlier releases cannot verify the P-384 signatures they produce. |
| `cosign-linux-amd64` SHA-256 | `424e2c97d0b45fd7a6b608a87f090f35633f4c23ece5f092d48bba11aaf4bbfa` |
| `cosign-linux-arm64` SHA-256 | `ade83b7aac62e131cacfe96d958948b2150cc9efec46a570a2649b8e27dedad2` |
| Signing configuration | [`signing-config.v0.2.json`](../../../scripts/ci/opensecret-release/signing-config.v0.2.json): the Rekor v2 log `https://log2025-1.rekor.sigstore.dev` and the timestamp authority `https://timestamp.sigstore.dev/api/v1/timestamp`, each `EXACT` 1. No Rekor v1, Fulcio or OIDC entry. |
| Trusted root | [`trusted_root.json`](../../../scripts/ci/opensecret-release/trusted_root.json), Sigstore's production trusted root from its TUF repository, fetched November 17, 2026 |

The signing configuration is passed explicitly because Cosign's default logs
to Rekor v1. It names a dated log shard, so review both files when Sigstore
adds or retires a Rekor v2 shard or rotates its timestamp authority: the
producer then fails closed until they are updated. The producer in
[`opensecret_release_statement.py`](../../../scripts/ci/opensecret_release_statement.py)
holds the Cosign digests, environment mapping, bucket and host.

## Configuration before the first Dev run

The workflow fails closed until all of the following exist.

Committed to this repository once the Dev signing resources exist:

| Value | Where |
| --- | --- |
| Dev KMS key's full ARN, `arn:aws:kms:us-east-1:<account>:key/<key-id>` (never an alias) | `SIGNING_KEY_ARN` in the workflow's job `env` |
| Dev signer role ARN, `arn:aws:iam::<same account>:role/maple-pcr-signer-dev` | `SIGNER_ROLE_ARN` in the workflow's job `env` |
| Reviewed Dev public key, SPKI PEM (P-384) | `scripts/ci/opensecret-release/dev-signing-key.pem` |

The protected GitHub environment `pcr-signing-dev`: required reviewers,
deployment branches limited to `master`, administrator bypass disabled. It
holds exactly this one secret and these five variables, all set on the
environment, not the repository:

| Name | Kind | Value |
| --- | --- | --- |
| `OPENSECRET_PCR_SIGNING_BWS_ACCESS_TOKEN` | Secret | A Bitwarden Secrets Manager machine token for Dev only. It can read the existing legacy PCR signing key item and the Dev bucket's two publisher items, and nothing for Production. Same name as the secret in `pcr-signing`, but its own value and grants. |
| `OPENSECRET_PCR_SIGNING_KEY_ID` | Variable | BWS item ID of the existing legacy PCR signing key |
| `ATTESTATIONS_R2_ACCOUNT_ID` | Variable | Cloudflare account ID that owns the bucket (32 hex characters) |
| `ATTESTATIONS_R2_BUCKET` | Variable | `maple-attestations-dev` |
| `ATTESTATIONS_R2_ACCESS_KEY_ID_SECRET_ID` | Variable | BWS item ID of the Dev bucket publisher's access key ID |
| `ATTESTATIONS_R2_SECRET_ACCESS_KEY_SECRET_ID` | Variable | BWS item ID of the Dev bucket publisher's secret access key |

Outside this repository: the signer role can be assumed only by a job whose
OIDC token matches this repository's `pcr-signing-dev` environment subject, the
`master` ref, and this workflow file at `master`, and it may sign only with the
Dev key. Renaming this workflow file therefore stops signing until the role is
updated;
the Dev publisher credential can write only `maple-attestations-dev`; and the
bucket's `attestations-dev.maple.ai` custom domain serves public `GET`/`HEAD`
with CORS. No zone cache setting may replace the object's
`Cache-Control: public, max-age=300`, or the readback check fails.

## Verifying a published bundle

With the reviewed Dev public key and the EIF:

```sh
curl -fsS -o bundle.json "https://attestations-dev.maple.ai/<PCR0>.json"
cosign verify-blob-attestation --bundle bundle.json \
  --key scripts/ci/opensecret-release/dev-signing-key.pem \
  --type https://maple.ai/attestations/enclave-release/v1 \
  --use-signed-timestamps \
  --trusted-root scripts/ci/opensecret-release/trusted_root.json \
  image.eif
```

Then check the payload's `environment` and PCRs against what you expect. Use
Cosign v3.1.4 or later.
