# Signed PCR files

OpenSecret keeps its existing files directly under `services/opensecret/`:

| File | Purpose |
| --- | --- |
| `pcrDev.json` | Current development build measurements |
| `pcrProd.json` | Current production build measurements |
| `pcrDevHistory.json` | Development PCR0 approvals with existing signatures |
| `pcrProdHistory.json` | Production PCR0 approvals with existing signatures |

The monorepo import preserves their bytes and the existing signing key. It does
not build or deploy an EIF, change a measurement, sign a new approval, or publish
an SDK. Continue the separately authorized Linux/ARM64 build, review, signing,
and deployment process in [the Nitro runbook](nitro-deploy.md).

The existing format uses ECDSA P-384 with SHA-384 and a 96-byte P1363 signature
over the lowercase **PCR0 text only**. PCR1, PCR2, timestamp, environment, and
history order are not authenticated by that signature. The operator must review
those fields and preserve the existing development/production separation. The
helper below validates their shape and consistency; it does not change the
signed message or make additional cryptographic claims.

## Where clients read them

Both SDKs fetch the signed history for their environment from this repository:

- [Development history](https://raw.githubusercontent.com/MaplePrivacyLabs/Maple/master/services/opensecret/pcrDevHistory.json)
- [Production history](https://raw.githubusercontent.com/MaplePrivacyLabs/Maple/master/services/opensecret/pcrProdHistory.json)

These must return HTTP 200 directly and match the reviewed files. The SDK
history fetches reject redirects, so a moved or renamed repository is not a
substitute.

## The legacy copy is retired

Until September 2026 clients read the same four files from the root of
`OpenSecretCloud/opensecret`, and every approval was published there by hand as
well. Research 3.4.0 and both SDKs at 4.0 moved to the URLs above in the same
releases that moved them to Transport V2. Everything older speaks only
Transport V1, which the backend no longer serves, so nothing that can still
reach a current backend reads the old copy.

New approvals are therefore published here only. Leave the old repository and
its files as they are: do not publish to it, prune it, or archive it as part of
a release.

One exception: a build from a commit that still serves Transport V1 can still
be reached by old clients. If such a build is ever deployed, publish its
approval to the old copy too. From `services/opensecret/`, in the same shell as
the check below:

```sh
python3 scripts/pcr_compatibility.py prepare \
  --source-repo /absolute/path/to/Maple --source-ref FULL_REVIEWED_MAPLE_COMMIT_SHA \
  --legacy-repo /absolute/path/to/a/clean/legacy/worktree \
  --legacy-ref FULL_FETCHED_LEGACY_MASTER_SHA
```

It reads the four files from the committed Maple revision, verifies both sides,
and reports what would change. Repeated with `--apply`, it copies the files
into the legacy worktree and leaves them unstaged for review. Publish that with
an ordinary pull request to the old repository.

## Validate an authorized measurement update

Signing setup and credential isolation are documented in
[`secretspec/README.md`](../secretspec/README.md). Use the dedicated
`opensecret_pcr_signing` alias and `just --no-dotenv` operator commands.
Build first without credentials, review measurements, then resolve only the
existing signing key around the signer. Deployment consumes the resulting
reviewed immutable artifact; it must not rebuild or sign implicitly.

After an approval branch from the `OpenSecret EIF release` or
`OpenSecret Dev release statement` workflow has merged, or after running the
operator `update-pcr-*` recipes locally, run the following
from `services/opensecret/`:

```sh
OPENSECRET_DEV_POSTGRES=0 OPENSECRET_DEV_ENV=0 OPENSECRET_DEV_CONTAINERS=0 \
  nix develop --no-update-lock-file -c \
  python3 scripts/pcr_compatibility.py check . \
  --baseline-dir /absolute/path/to/services/opensecret/at/the/previous/master
```

The `update-pcr-dev` and `update-pcr-prod` recipes copy measurements and
append a verified signature. This additional check verifies all four files, every PCR0 signature
against the SDK's existing public key, snapshot membership in the appropriate
history, and preservation of every prior entry in the baseline. Snapshot membership may
refer to an earlier entry to permit a deliberate rollback.

The helper rejects malformed or empty histories, duplicate JSON fields,
duplicate PCR0s, all-zero measurements, malformed signatures, changed or
reordered history entries, and inputs above the SDK limits of 1 MiB per history
and 2,048 entries. It applies the same byte bound to measurement files. It checks
the existing five history fields and four measurement fields; it does not
introduce a new format. Never prune history or relax limits just to make an
update pass: investigate and coordinate a separate compatibility change.

The offline baseline check cannot establish that a checkout is current on
GitHub. Fetch `master` immediately before taking the baseline. If another
operator has written to either history, reconcile the exact entries first; do
not choose a winner by overwriting files. Keep a single operator responsible
for each update.

## Verify publication before deployment

After the approval is merged, fetch each of the four raw files and compare the
downloaded bytes with the reviewed source commit. For each URL, use a temporary
output path and a bounded request that does not follow redirects, for example:

```sh
curl --fail --silent --show-error --proto '=https' --max-time 15 \
  --max-filesize 1048576 --output /absolute/path/to/check/pcrProdHistory.json \
  --write-out '%{http_code}\n' \
  https://raw.githubusercontent.com/MaplePrivacyLabs/Maple/master/services/opensecret/pcrProdHistory.json
```

Require the printed status to be **200**; a 3xx is a failed check even if curl
itself exits successfully. Never add `--location`. Check the immutable commit
URL as well as the `master` URL, compare all four files byte-for-byte, and run
the offline helper's `check` against the downloaded four-file directory. A
cached old `master` response is not successful publication; wait for matching
bytes. Record the commit SHA and hashes. The current deployment keeps serving
during this repository work; these checks do not prove its live EIF or KMS
policy.

Before deploying an authorized new measurement, its approval must be published
and verified so clients can approve it. Keep the existing release process and
its deployment gates. There is still no GitHub-managed EIF deployment,
signing-key migration, or Sigstore change in this path.

## Regression checks

From `services/opensecret/`:

```sh
OPENSECRET_DEV_POSTGRES=0 OPENSECRET_DEV_ENV=0 OPENSECRET_DEV_CONTAINERS=0 \
  nix develop --no-update-lock-file -c python3 scripts/test_pcr_compatibility.py
```

Tests use existing public signed entries and temporary local Git repositories.
They exercise malformed/signature/size failures, prefix divergence, rollback
membership, wrong origins, stale refs, dirty worktrees, symlinks, exact unstaged
copies, and restoration after a simulated write failure. They do not create
signatures, publish files, or contact GitHub.
