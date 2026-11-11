# Enclave release statements

The Dev producer adds a reviewer-approved Sigstore statement alongside the
existing PCR0 signature. Client acceptance, production signing and deployment
behavior retain their existing contracts. No new app UI is introduced.

## Selecting a candidate

Run `OpenSecret EIF release` on `master`, select `dev`, and leave `dry_run`
checked. Wait for success, then dispatch `OpenSecret Dev release statement`
on `master` with that build run's numeric ID. Review the selected build's source
and measurements before approving the `pcr-signing-dev` environment. There is
only one signing job in that file: a Sigstore identity names a workflow/ref,
so an ungated job in the same file would bypass this approval boundary.

The producer accepts only a completed successful manual run of
`.github/workflows/opensecret-eif-release.yml` in the canonical public
`MaplePrivacyLabs/Maple` repository on `master`. Its full source SHA must be
reachable from the signing checkout. The artifact must be the unique retained
Dev candidate for that run/source. Archives cannot add paths or symlinks.

`image.eif`, `pcr.json` and `handoff.json` must each have valid build provenance
for that exact source, builder workflow/ref and run attempt. The handoff's mode
must be `dev`; source SHA, EIF SHA-256 and PCR0/1/2 must agree with the selected
files and checksums. Earlier candidates with unattested handoffs are rejected.
The producer uses the selected artifact, without rebuilding it or substituting
the current signing checkout's source SHA.

## Statement contract, version 1

`actions/attest` signs an in-toto Statement v1 with the EIF's SHA-256 as its
subject digest and predicate type
`https://maple.ai/attestations/enclave-release/v1`. Its predicate has this shape:

```json
{
  "environment": "development",
  "source": {
    "repository": "https://github.com/MaplePrivacyLabs/Maple",
    "commit": "<full 40-character lowercase commit SHA>"
  },
  "measurements": {
    "PCR0": "<96 lowercase hex characters>",
    "PCR1": "<96 lowercase hex characters>",
    "PCR2": "<96 lowercase hex characters>"
  },
  "build": {
    "runId": "<decimal build run ID>",
    "runAttempt": 1
  }
}
```

Dev's expected certificate identity is exactly
`https://github.com/MaplePrivacyLabs/Maple/.github/workflows/opensecret-release-statement-dev.yml@refs/heads/master`,
with OIDC issuer `https://token.actions.githubusercontent.com`. The build
workflow's signature establishes provenance, not reviewer release approval.
The signing checkout's SHA identifies the workflow implementation; the signed
predicate's source SHA identifies the EIF build. Those may differ.

Public GitHub repository attestations use Sigstore's public production service,
including for Maple Dev. The workflow returns a standard Sigstore bundle,
containing the statement, signature, certificate and log evidence. It publishes
that file directly, without wrapping it in a separate Maple signature format.

## Configuration boundaries

Configure `pcr-signing-dev` with required review, master-only deployment policy
and no administrator bypass before dispatching. The workflow file is a permanent
client identity: review changes to it and the shared producer helpers accordingly.

The environment supplies these existing legacy-key settings:

| Setting | Purpose |
| --- | --- |
| Secret `OPENSECRET_PCR_SIGNING_BWS_ACCESS_TOKEN` | Read-only BWS machine access to the existing PCR key |
| Variable `OPENSECRET_PCR_SIGNING_KEY_ID` | Existing key's BWS item ID |

The same gated job supplies these static-publisher settings:

| Setting | Purpose |
| --- | --- |
| Secret `ATTESTATIONS_PUBLISH_BWS_ACCESS_TOKEN` | Dev-only read-only BWS machine access to its publisher pair |
| Variable `ATTESTATIONS_R2_ACCESS_KEY_ID_SECRET_ID` | Dev R2 access-key BWS item ID |
| Variable `ATTESTATIONS_R2_SECRET_ACCESS_KEY_SECRET_ID` | Dev R2 secret-key BWS item ID |
| Variable `ATTESTATIONS_R2_ACCOUNT_ID` | Cloudflare account ID, not a secret |
| Variable `ATTESTATIONS_R2_BUCKET` | Must be `maple-attestations-dev` |

The R2 credential must have Object Read & Write authority only for the Dev
bucket. BWS grants must also prevent the Dev publisher machine credential from
retrieving Production's publisher pair. Different item IDs behind a machine
credential with access to both environments do not provide that isolation.
BWS outputs are scoped to consuming steps with `set_env: false`.

Attach `attestations-dev.maple.ai` directly to that bucket and configure public
GET/HEAD CORS with `AllowedOrigins: ["*"]`, exposing `ETag`. No Worker or website
deploy is needed. Keep historical objects without lifecycle expiry; permit
reviewed replacements of the same PCR lookup key. Production will have a separate
bucket, hostname, gated workflow identity and publisher access. This producer
cannot choose its environment, bucket or public hostname from workflow inputs.
Cloudflare provisioning is a separate configuration task; empty configured
buckets are sufficient for that task.

## Publication and compatibility

One reviewer gate authorizes both signatures for the same verified candidate.
The legacy path uses the existing key/format and history append recipe, which
skips already approved measurements. Approval history still requires an ordinary
PR, merge and manual legacy mirror publication. The Dev path in the original
build workflow no longer signs; its Production signing job remains unchanged
apart from its explicit Production-only condition.

When the signing checkout is newer than the selected build, the approval branch
also includes the signing checkout's short SHA. This keeps unrelated source
changes out of retries against the build's original approval branch. Its commit
message still records the selected artifact's full source SHA.

The producer verifies the new bundle with `gh attestation verify`, enforcing
the exact signer, issuer, predicate and signing source SHA, then compares its
authenticated subject and full predicate with the selected candidate. It retains
the EIF, handoff, predicate, original bundle and verifier output in an immutable
`opensecret-approved-dev-<build-run>-<signing-run>` Actions artifact (90-day
retention). Preserve that original artifact in the deployment artifact archive
before its Actions retention expires. This workflow does not provision a durable
EIF archive or change deployment consumers.

Only `https://attestations-dev.maple.ai/<PCR0>.json` is uploaded. The object has
`Content-Type: application/json` and `Cache-Control: public,max-age=300`.
The uploader performs one S3 `PutObject`, never syncs or deletes other keys.
Identical reruns and new approved builds sharing the PCR tuple can replace
that lookup file. Short caching allows replacements; it is not an immutable URL.
Two EIFs can share PCRs, so retain the original bundle for exact-digest rollback.

After uploading, the producer fetches the public URL with a browser Origin,
checks GET/HEAD, CORS and content/cache headers, compares downloaded bytes, and
cryptographically verifies the downloaded bundle again. It allows a complete
cache lifetime for an older same-PCR object to expire. Failed uploads, stale
responses or verification failures fail publication rather than report success.
The job summary records the public URL, bundle digest, EIF digest and source SHA.
An upload can have succeeded before a readback or later legacy-branch push fails;
inspect the run and retry the same reviewed artifact instead of deleting history.

No live signing/publication is exercised by the PR checks. Before SDK integration,
use an authorized Dev run to establish the real public bytes/CORS, log evidence
and compatibility with the selected browser and Rust verification libraries.
Also verify the actual R2/BWS grant isolation; local unit tests cannot prove it.
There is no release expiry or withdrawal policy in this version.
