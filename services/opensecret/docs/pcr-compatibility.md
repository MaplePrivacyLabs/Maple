# Signed PCR files and the legacy compatibility mirror

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

## Keep existing clients working

Retain `OpenSecretCloud/opensecret`, its `master` branch, and these exact root
paths, including the two signed histories:

- [Development history](https://raw.githubusercontent.com/OpenSecretCloud/opensecret/master/pcrDevHistory.json)
- [Production history](https://raw.githubusercontent.com/OpenSecretCloud/opensecret/master/pcrProdHistory.json)

Keep that repository public, unarchived, and writable for the operator. A
repository move or redirect is not a substitute: current TypeScript and Rust
SDK history fetches reject redirects. Existing clients continue to use the old
URLs; they do not discover the monorepo layout automatically.

The in-tree TypeScript and Rust SDK defaults use these canonical histories:

- [Canonical development history](https://raw.githubusercontent.com/MaplePrivacyLabs/Maple/master/services/opensecret/pcrDevHistory.json)
- [Canonical production history](https://raw.githubusercontent.com/MaplePrivacyLabs/Maple/master/services/opensecret/pcrProdHistory.json)

Both locations must return HTTP 200 directly and match the reviewed files;
use the verification procedure below for every authorized measurement update.
SDK URL changes are independent of registry renaming or publishing. Older
published SDKs and installed clients retain their existing URLs until upgraded.
Do not remove the legacy history or set a calendar cutoff as a consequence of
the source cutover. An eventual cutoff needs an explicit client compatibility
decision.

## Validate an authorized measurement update

Signing setup and credential isolation are documented in
[`secretspec/README.md`](../secretspec/README.md). Use the dedicated
`opensecret_pcr_signing` alias and `just --no-dotenv` operator commands.
Build first without credentials, review measurements, then resolve only the
existing signing key around the signer. Deployment consumes the resulting
reviewed immutable artifact; it must not rebuild or sign implicitly.

After reviewing an authorized EIF and running the existing operator PCR update
and signing steps, run the following from `services/opensecret/`:

```sh
OPENSECRET_DEV_POSTGRES=0 OPENSECRET_DEV_ENV=0 OPENSECRET_DEV_CONTAINERS=0 \
  nix develop --no-update-lock-file -c \
  python3 scripts/pcr_compatibility.py check . \
  --baseline-dir /absolute/path/to/a/fresh/legacy/worktree
```

The `update-pcr-dev` and `update-pcr-prod` recipes copy measurements and
append a verified signature. This additional check verifies all four files, every PCR0 signature
against the SDK's existing public key, snapshot membership in the appropriate
history, and preservation of every prior legacy entry. Snapshot membership may
refer to an earlier entry to permit a deliberate rollback.

The helper rejects malformed or empty histories, duplicate JSON fields,
duplicate PCR0s, all-zero measurements, malformed signatures, changed or
reordered history entries, and inputs above the SDK limits of 1 MiB per history
and 2,048 entries. It applies the same byte bound to measurement files. It checks
the existing five history fields and four measurement fields; it does not
introduce a new format. Never prune history or relax limits just to make an
update pass: investigate and coordinate a separate compatibility change.

The offline baseline check cannot establish that a checkout is current on
GitHub. Fetch the legacy repository immediately before preparing the copy. If
another operator has written to either history, reconcile the exact entries
into the canonical history first; do not choose a winner by overwriting files.
Keep a single operator responsible for each update across both repositories.

## Prepare a manual legacy publication

Use a dedicated, clean legacy worktree on a fresh branch from fetched
`origin/master`. The example variables below must identify your exact local
checkouts and reviewed **full commit SHAs**. The helper reads source files from
the specified committed monorepo revision, not uncommitted working files.

```sh
maple_repo=/absolute/path/to/Maple
legacy_repo=/absolute/path/to/opensecret-pcr-worktree
maple_pcr_ref=FULL_REVIEWED_40_CHARACTER_MAPLE_COMMIT_SHA

git -C "$legacy_repo" fetch origin refs/heads/master:refs/remotes/origin/master
legacy_pcr_ref="$(git -C "$legacy_repo" rev-parse refs/remotes/origin/master)"
# In a clean dedicated worktree, create this operator-chosen branch at that ref.
git -C "$legacy_repo" switch -c pcr-compatibility-update "$legacy_pcr_ref"

cd "$maple_repo/services/opensecret"
OPENSECRET_DEV_POSTGRES=0 OPENSECRET_DEV_ENV=0 OPENSECRET_DEV_CONTAINERS=0 \
  nix develop --no-update-lock-file -c python3 scripts/pcr_compatibility.py prepare \
  --source-repo "$maple_repo" --source-ref "$maple_pcr_ref" \
  --legacy-repo "$legacy_repo" --legacy-ref "$legacy_pcr_ref"
```

The default command only reports entry counts, SHA-256 file hashes, sizes, and
which files would change. Inspect that result. It requires the expected GitHub
origins, tracked regular files, the exact legacy HEAD and fetched
`origin/master`, a clean legacy worktree/index, valid signatures in both
revisions, and an unchanged legacy history prefix. A stale or divergent local
baseline fails before any copy. The helper performs no network requests and
cannot detect an unfetched remote update.

Repeat the same command with `--apply` to copy the exact verified bytes into
only the four legacy files. The helper rechecks local state before copying. It
does not stage, commit, push, sign, load a private key, or require a CI token.
An import with identical files produces no changes and needs no mirror commit.

For a real update, inspect and commit the result through the normal operator
review process:

```sh
git -C "$legacy_repo" diff --check
git -C "$legacy_repo" diff -- pcrDev.json pcrProd.json pcrDevHistory.json pcrProdHistory.json
git -C "$legacy_repo" add -- pcrDev.json pcrProd.json pcrDevHistory.json pcrProdHistory.json
git -C "$legacy_repo" commit -m 'Mirror reviewed OpenSecret PCR approvals from Maple'
git -C "$legacy_repo" fetch origin refs/heads/master:refs/remotes/origin/master
test "$(git -C "$legacy_repo" rev-parse refs/remotes/origin/master)" = "$legacy_pcr_ref"
```

If the last check fails, stop and reconcile against the newer legacy master in
a fresh worktree before publishing. Publish the reviewed branch with an ordinary
non-force push and PR. Include the immutable Maple source SHA and helper output
in the PR. Recheck the latest legacy master immediately before merge and verify
that every existing history entry remains unchanged. Do not resolve a concurrent
history edit by replacing the other operator's file wholesale.

## Verify publication before deployment or SDK URL cutover

After the approved changes are merged in both repositories, fetch each of the
four raw files from each repository and compare the downloaded bytes with the
reviewed source commit. For each URL, use a temporary output path and a bounded
request that does not follow redirects, for example:

```sh
curl --fail --silent --show-error --proto '=https' --max-time 15 \
  --max-filesize 1048576 --output /absolute/path/to/check/pcrProdHistory.json \
  --write-out '%{http_code}\n' \
  https://raw.githubusercontent.com/OpenSecretCloud/opensecret/master/pcrProdHistory.json
```

Require the printed status to be **200**; a 3xx is a failed check even if curl
itself exits successfully. Never add `--location`. Check immutable commit URLs
as well as `master` URLs, compare all four files byte-for-byte, and run the
offline helper's `check` against each downloaded four-file directory. A cached
old `master` response is not successful publication; wait for matching bytes.
Record both commit SHAs and hashes. The current deployment keeps serving during
this repository work; these checks do not prove its live EIF or KMS policy.

Before deploying an authorized new measurement, both histories must be
published and verified so clients using either location can approve it. Keep
the existing release process and its deployment gates. No GitHub-managed EIF
deployment, automatic legacy backpublisher, signing-key migration, or Sigstore
change is part of this compatibility path.

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
