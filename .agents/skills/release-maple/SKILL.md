---
name: release-maple
description: Prepare, publish, monitor, and verify a Maple release from current master. Use when asked to bump a release version, cut or create a GitHub release, verify signed artifacts and updater metadata, monitor downstream publication, or prepare an explicitly authorized App Store, TestFlight, Google Play, or billing-API handoff.
---

# Release Maple

Treat every release step as a production action. Do not use this workflow for
routine validation. Before a write, state the exact repository, version, tag,
commit, external effect, and authority provided by the user.

## Know the triggers

- A push to `master` starts production-shaped desktop, Android, iOS, web,
  frontend, and Rust workflows. The iOS master workflow uploads its verified
  IPA to TestFlight automatically.
- `Mobile App CI` serializes production iOS build/export, verification, and
  upload so concurrent master pushes cannot reuse an export-assigned build
  number. Its master-only manual dispatch forces a fresh build even without
  app changes. After a duplicate-build rejection, dispatch this full workflow
  from master; retrying only submission reuses the rejected IPA's build number.
- The independent `Maple Dev TestFlight` workflow builds master pushes that
  change Research web or iOS inputs, and every master dispatch, and uploads the
  distinct `cloud.opensecret.maple.dev` application for
  internal testing against the existing development services. Manual dispatch
  accepts master only. Its app identity, credentials, artifact verification,
  internal group setup, and retry contract are documented in
  [Maple Dev on TestFlight](../../../docs/ios-dev-testflight.md).
- Creating a GitHub Release starts the cross-platform release workflow. A
  successful release workflow starts separate updater-metadata, Pages, and
  best-effort Zapstore workflows. These siblings never gate or change the core
  Maple release. With repository variable `MAPLE_PAGES_PRODUCTION_ENABLED=true`,
  `Publish Pages` uploads the verified existing release web artifact. A missing
  or false flag pauses production publication; Cloudflare native Git builds
  remain disabled. Read
  [the Pages publisher architecture](../../../docs/pages-deployments.md) for
  its source contract. Publisher activation, environment administration, and
  Cloudflare build controls belong to a separately authorized operator procedure.
- The same Maple GitHub Release receives four native `maple-proxy` archives and
  their checksum manifest. Never create a separate proxy Release or proxy tag;
  `/releases/latest` must continue to identify the Maple application release.
- Maple GitHub Releases do not publish `maple-sdk` or `maple-proxy` to
  crates.io. A successful stable release starts a non-gating GHCR sibling that
  builds only for a new or missing non-baseline proxy version. It treats exact
  version tags as immutable, checks release proofs and container inputs,
  and repairs minor, major, and `latest` aliases without rebuilding an existing
  exact image. Version `0.3.3` is the explicit unbackfilled migration baseline.
  The container remains separately versioned at
  `ghcr.io/mapleprivacylabs/maple-proxy`.
- GitHub Release creation does not itself submit the release IPA or AAB to
  Apple App Store review or Google Play.

Never push or merge `master`, create a release, retry a workflow, upload to a
store, submit for review, or alter a rollout merely to see whether it works.

## Canonical repository and package initialization

Before creating a new release, verify that `repo.meta.json` identifies
`MaplePrivacyLabs/Maple` as the canonical repository. Normal releases generate
canonical asset URLs; preserve the legacy updater fallback compiled into
installed clients. Do not republish old-owner metadata to repair a current
release: follow the updater publisher's exact-tag verification and recovery path.

Only when initializing a package that does not yet exist, use a manual
`Publish proxy container` run to initialize `ghcr.io/mapleprivacylabs/maple-proxy`
from `master` with `bootstrap_only=true`. This validates the current release
and uploads only content-addressed images, without version or alias tags. It
does not treat failed package enumeration as an empty inventory. GitHub creates
new packages privately; make that package public in its settings, retain Maple
Actions write access, then rerun only the proxy publisher with both manual
inputs disabled. For read-only API diagnosis, use `diagnostics_only=true`;
its success reports completed observations, not publication readiness. Do
not create another release, change proxy versions, or overwrite exact tags to
repair package visibility. An existing public package uses the normal publisher
to verify or recover the eligible version and reconcile aliases; bootstrap is
not a routine release step. Existing old-namespace images receive no updates.

## Prepare the version

1. Prepare the bump on a clean focused branch based on current `origin/master`;
   do not switch another worktree to `master` or force its owning worktree off
   that branch. Compare the checked-in version with the latest release:

   ```bash
   git fetch origin master
   git merge-base --is-ancestor origin/master HEAD
   current_version="$(nix develop --no-update-lock-file .#ci -c just get-version | tail -n 1)"
   released_version="$(gh api repos/MaplePrivacyLabs/Maple/releases/latest --jq '.tag_name | ltrimstr("v")')"
   printf 'current=%s released=%s\n' "$current_version" "$released_version"
   ```

2. If `current_version` is newer, retain that pending version unless the user
   explicitly chooses a different target. For example, a requested `3.4.0`
   replaces a pending `3.3.11`; a generic request to release does not trigger
   another bump. The chosen target must still be newer than the published release.
3. If versions are equal, establish the intended next version. Proceed when
   the user names an exact version or patch/minor/major level. If the user
   delegates the choice, use patch; do not infer minor or major from commits.
4. On a focused branch, run exactly one repository helper:

   ```bash
   nix develop --no-update-lock-file .#ci -c just update-version X.Y.Z
   nix develop --no-update-lock-file .#ci -c just bump-patch
   nix develop --no-update-lock-file .#ci -c just bump-minor
   nix develop --no-update-lock-file .#ci -c just bump-major
   ```

5. Review all manifest, Apple project, Android version-code, and
   `apps/maple-research/frontend/src-tauri/Cargo.lock` changes. Run the applicable Maple validation
   gates and submit the isolated bump through normal review when authorized.
   Do not use `just release`; it creates a local tag before the reviewed GitHub
   flow.
6. After the bump merges, use or create a clean worktree on `master`. If another
   worktree already owns that branch, use its checkout instead of forcing or
   stealing it. Pull with `--ff-only` and wait for the required CI evidence.
   Release only current master; preflight verifies it again.

## Run preflight

Run the bundled fail-closed preflight from the repository root:

```bash
preflight="$(.agents/skills/release-maple/scripts/preflight.sh)"
printf '%s\n' "$preflight" | jq .
tag="$(printf '%s' "$preflight" | jq -r .tag)"
previous_tag="$(printf '%s' "$preflight" | jq -r .previous_tag)"
head_sha="$(printf '%s' "$preflight" | jq -r .head_sha)"
```

The script requires a clean current `master`, exact manifest version parity, a
newer version and unused tag, and successful executed master CI jobs.
CodeQL must not be red: the newest finished CodeQL run (master push, nightly or
manual) on the release commit or an ancestor must have succeeded. A scan still
running does not block the release. If the newest finished scan failed, fix it
or re-run CodeQL from master before preflight. Other workflows still require
master-push evidence. Research test/build evidence may
come from an ancestor only when the complete Git tree diff contains exclusively
independent Agent, documentation, agent guidance, or release-gate test-harness
changes. Unknown paths and changes to Research, SDK, proxy, shared scripts,
workflows, or Nix require fresh evidence. Skipped builds never count as build
proof; the helper searches a bounded workflow history and rejects newer failures,
pending runs, missing jobs, changed inputs, and non-ancestors. The returned
`ci_evidence` records each actual run, tested SHA, and whether it was reused.
Preflight also rechecks master after collecting evidence. Stop on any failure; correct it through the normal reviewed process.
Never overwrite or move a release tag.

Review the [SDK consumer version policy](../../../docs/sdk-publishing.md#consumer-version-policy)
for the clients being released. Record the frontend's selected SDK from its
manifest/lockfile and each Rust consumer's resolved version/source using the
policy's `cargo metadata --locked` command. Local links are allowed. If those
clients will ship unpublished SDK changes, recommend publishing the SDK and
pinning those consumers first; an intentional local-source release can proceed
with the exact monorepo commit recorded. Unrelated SDK source changes do not
require a pinned client to upgrade, and this preference adds no release gate.
Publishing changed SDK source requires a new registry version; neither an app
release nor an in-tree edit replaces an already published package.

Bundled PCR roots are a local fast path: on a miss, the SDK verifies the signed
history for the selected environment. Newly signed approvals can therefore
support a new EIF without a client bundle refresh. Refreshing bundles can avoid
that fetch and improve compatibility when fetching is unavailable; it is not
another signing or enclave authorization step. Keep the canonical and legacy
signed histories intact under the
[PCR compatibility procedure](../../../services/opensecret/docs/pcr-compatibility.md).
Pruning an app's roots does not remove roots from its pinned published SDK or
revoke approvals in signed history.

When preparing an enclave trust or PCR rotation change, review both browser
consumers' embedded fallbacks against the approved development and production
histories: `apps/maple-research/frontend/src/config/openSecretClientConfig.ts`
and `apps/maple-auth/src/config/openSecretClientConfig.ts`. Assess each affected
app's combined app-provided and pinned-SDK coverage when signed-history fetching
is unavailable, preserving development/production separation. Record whether
the intended measurements are covered locally or rely on signed-history fetching;
refresh only for the intended offline compatibility target. Record affected
artifacts and any pending rollout
in the handoff. Auth owns a separate SDK pin and publisher: refreshing Research
does not update Auth, and an Auth publication remains a separately authorized
operation under [the Pages guide](../../../docs/pages-deployments.md#independent-auth-site).
This is a compatibility review of each consumer, not a requirement to keep their
lists byte-identical or release them together.

Record the proxy version and inspect its own runtime inputs since `previous_tag`:

```bash
proxy_version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' proxy/Cargo.toml | head -n 1)"
git diff --name-only "$previous_tag".."$head_sha" -- proxy
printf 'proxy_version=%s\n' "$proxy_version"
```

Inspect the proxy's manifest and lockfile at both revisions. Include changes
under `sdk/rust` in its runtime comparison when either revision uses that local
SDK. With registry dependencies at both revisions, compare the selected SDK
versions/checksums in `proxy/Cargo.lock`; unrelated `sdk/rust` edits are not
proxy runtime changes. The embedding app's SDK selection is separate from the
standalone proxy's lockfile.

If consumed runtime inputs changed without a proxy version change, stop and
make the version decision explicit before publishing. A normal Maple Release
always builds the checked-in proxy version, but that does not implicitly
authorize a crates.io or GHCR publish.

Preview GitHub's generated notes:

```bash
gh api --method POST repos/MaplePrivacyLabs/Maple/releases/generate-notes \
  -f tag_name="$tag" \
  -f target_commitish="$head_sha" \
  -f previous_tag_name="$previous_tag" | jq -r '.name, .body'
```

Confirm the notes span the intended changes and recheck that `head_sha` is
still `origin/master`. GitHub's generated body is changelog input, not a
complete public release description. Draft a concise user-facing summary and
highlights from the exact release diff, place them above the generated notes,
and review the complete Markdown in a temporary `notes_file`. Do not publish a
PR-list-only description when the release has meaningful product changes.
Present the tag, commit, previous tag, and final notes to the user before
creating the release unless the current request already gives unambiguous
authority for that exact release.

## Publish once

Create the GitHub Release exactly once. This creates the tag in the same flow:

```bash
gh release create "$tag" \
  --repo MaplePrivacyLabs/Maple \
  --target "$head_sha" \
  --title "$tag" \
  --notes-file "$notes_file"
```

Do not create or push a local tag first. Record the release URL and confirm the
release and workflow resolve to `head_sha`.

## Monitor release CI

Find and watch the new `Release` run:

```bash
gh run list --repo MaplePrivacyLabs/Maple --workflow Release --event release \
  --commit "$head_sha" --limit 10 \
  --json databaseId,displayTitle,headSha,status,conclusion,url

gh run watch RELEASE_RUN_ID \
  --repo MaplePrivacyLabs/Maple --exit-status --compact
```

Stay with every platform build, signature/canonical proof, artifact upload,
the four native proxy builds and their published-asset verification, updater
`latest.json`, aggregate verification, and verification-guide step.
Packaging success alone is not runtime smoke; inspect the workflow's actual
verification and attestation results. Record attestation generation separately
from independent verification of downloaded assets; an overall successful run
does not prove every attestation was generated or cryptographically verified.

After `Release` succeeds, inspect the two required publication handoffs. Do
not rerun the core Release to repair either sibling:

```bash
gh run list --repo MaplePrivacyLabs/Maple --workflow 'Publish updater metadata' \
  --limit 10 \
  --json databaseId,status,conclusion,headSha,createdAt,url

gh run list --repo MaplePrivacyLabs/Maple --workflow 'Publish Pages' \
  --limit 10 \
  --json databaseId,status,conclusion,headSha,createdAt,url
```

Both publishers execute trusted master, which can be newer than the release.
Identify the updater run by its validated release tag and source, and the Pages
run by its production job's validated source SHA. Do not filter either by the
workflow checkout SHA; preview publication uses the same Pages workflow name.

Also inspect the independent proxy-container publisher. It should either prove
the expected immutable proxy version, public AMD64/ARM64 manifest, image labels,
build digests, linked provenance descriptors, and aliases; publish a missing
eligible version; or explicitly skip the unbackfilled `0.3.3` baseline.
Retry it with manual dispatch; never create a
proxy tag or Release and never rerun the core Release to repair it:

```bash
gh run list --repo MaplePrivacyLabs/Maple --workflow 'Publish proxy container' \
  --limit 10 \
  --json databaseId,status,conclusion,headSha,createdAt,url
```

The updater workflow must publish the original verified `latest.json` and its
matching Research installer catalog before reporting distribution current.
Its public check proves byte-identical Tauri metadata and all six GET/HEAD
installer redirects; marketing follows the stable URLs without a redeploy.
Treat incomplete assets, provenance failures and blocked download routes as
publication failures, not reasons to alter updater metadata or weaken checks.
Use the [updates service guide](../../../services/updates/README.md) for its
one-time rollout and recovery boundaries. Require the `production` job in
`Publish Pages` to succeed: it validates the release asset, uploads without a
rebuild, checks CF stage/commit/active canonical deployment, and then advances
the ref without force. This does not prove browser login/chat or configuration.
Report and repair sibling failures without altering completed release artifacts.

Confirm the production ref after that release's publication:

```bash
pages_sha="$(gh api repos/MaplePrivacyLabs/Maple/git/ref/heads/pages-production --jq .object.sha)"
[[ "$pages_sha" == "$head_sha" ]]
```

Inspect the `Publish Pages` production job summary and GitHub deployment. The
summary names the deployment ID and SHA. A failed post-upload freshness/ref/status check can
leave a new CF deployment already active: inspect actual canonical state before
retrying, and follow the deployment guide's hold/rollback procedure.

A raw `curl` from an automated VM may be denied by edge policy. An allowed-browser
smoke is separate application evidence; do not turn an edge-policy 403 into a
release failure. When authorized, manually dispatch `Publish Pages` from master
to retry an eligible source under the
[Pages source contract](../../../docs/pages-deployments.md#build-and-destination-contract).
An older stable release cannot overwrite a newer published web commit. Recovery
uses a recorded Cloudflare deployment rollback or a reviewed forward fix;
do not rewind `pages-production` or enable native Git builds. Never recreate a
Release or rerun the core release merely to repair Pages.

On failure, read the failed logs before acting:

```bash
gh run view RELEASE_RUN_ID --repo MaplePrivacyLabs/Maple --log-failed
```

Retry only a terminal failure proven to be transient infrastructure trouble:

```bash
gh run rerun RELEASE_RUN_ID --repo MaplePrivacyLabs/Maple --failed
```

Do not classify version/proof mismatches, deterministic builds, signing
failures, missing credentials, or integrity checks as transient. Do not delete
or recreate a published release without separate explicit direction.

## Verify the release and optionally inspect Zapstore

Verify the published release and its assets:

```bash
gh release view "$tag" --repo MaplePrivacyLabs/Maple \
  --json tagName,name,isDraft,isPrerelease,publishedAt,targetCommitish,url,assets

mkdir -p artifacts
gh release download "$tag" --repo MaplePrivacyLabs/Maple --dir artifacts
nix develop --no-update-lock-file .#ci -c \
  ./scripts/ci/verify-release-artifacts.sh artifacts proxy
```

Confirm the release contains all four stable proxy archives and
`maple-proxy-release-final.sha256`, and that their attestations and the
published-asset verification job succeeded. Report the embedded proxy version
separately from the Maple application version. Do not report crates.io as
updated unless its independent manual publisher was explicitly authorized and
verified. Report GHCR as published only after its sibling workflow and anonymous
manifest verification succeed; otherwise report the unchanged-version skip or
failure separately.

Verify that the hosted updater serves the same metadata as the GitHub Release:

```bash
updater_dir="$(mktemp -d)"
version="${tag#v}"

gh release download "$tag" --repo MaplePrivacyLabs/Maple \
  --pattern latest.json --dir "$updater_dir/release"
curl --fail --silent --show-error --location --max-time 20 \
  https://updates.trymaple.ai/latest.json >"$updater_dir/hosted.json"

jq -e --arg version "$version" '.version == $version' \
  "$updater_dir/hosted.json" "$updater_dir/release/latest.json"
cmp "$updater_dir/hosted.json" "$updater_dir/release/latest.json"
```

Do not report updater publication complete from workflow status alone: require
the public endpoint to return the intended version and content.

Zapstore starts only after `Release` succeeds and is strictly best effort. Its
queued, running, skipped, or failed state must not delay release completion,
trigger a release retry, or be reported as a Maple release failure. Inspect it
only when Zapstore status is specifically useful:

```bash
gh run list --repo MaplePrivacyLabs/Maple --workflow 'Publish to Zapstore' \
  --commit "$head_sha" --limit 10 \
  --json databaseId,status,conclusion,headSha,createdAt,url
```

Do not retry or repair Zapstore as part of the Maple release flow. A separate
explicit request may authorize investigating or retrying Zapstore itself.

Do not call the core repository release complete while its required `Release`
workflow is queued or running. Report updater publication and Pages production
as separate downstream states. Zapstore is not a required release workflow.

## Store and API handoff

Apple and Google actions remain manual production operations. Do not open a
store console, choose a track, add testers, upload a build, answer compliance
questions, submit for review, release an approved version, or change a rollout
without explicit authorization for that exact action.
Use the authority already provided for the requested handoff; ask only for
missing scope or a new external effect. Follow the
[store handoff procedure](references/store-handoff.md) for build selection,
Apple review submission, Google track retirement, and publishing state checks.

For an authorized handoff:

1. Identify the exact source commit and artifact. For a GitHub release asset,
   also record its tag; for an existing production TestFlight build, match its
   verified Mobile App CI export and upload receipt.
2. Verify its digest, platform signature, application/bundle ID, visible
   version, build/version code, and proof from that artifact's producing workflow.
3. Record the destination application, tester group or release track,
   countries/audience, rollout choice, and any review/compliance state before
   submission.
4. After the store reports a result, distinguish upload, processing, testing,
   review, approval, rollout, and public availability. Do not infer one state
   from another.
5. Trace the selected platform's actual billing path before applying a version
   gate. A legacy products API result does not establish StoreKit availability.
   If that client path uses a configured version gate, verify the exact version.
   Any service-side gate change or deployment requires its own reviewed workflow
   and authority.

Keep time-specific build numbers, review outcomes, blockers, and rollout facts
in the release handoff or issue that owns them, not in this evergreen skill.

## Report

Report the Maple version, proxy version, tag, exact commit, release URL, main
workflow URL and attempt count, application and four-proxy-archive verification
results, any required-workflow retry and supporting evidence, authorized
store/API actions, and every boundary that remains unverified. State crates.io
and GHCR status separately. If Zapstore was inspected, report its status as
non-gating. Separate repository release completion from store distribution and
live application availability.
