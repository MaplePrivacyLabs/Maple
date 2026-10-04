# Maple updates and Research downloads

This Worker serves the existing Tauri updater metadata and stable Research
installer links from one versioned static asset bundle:

```text
https://updates.trymaple.ai/latest.json
https://updates.trymaple.ai/download/research/stable/macos
https://updates.trymaple.ai/download/research/stable/windows
https://updates.trymaple.ai/download/research/stable/linux-appimage
https://updates.trymaple.ai/download/research/stable/linux-deb
https://updates.trymaple.ai/download/research/stable/linux-rpm
https://updates.trymaple.ai/download/research/stable/android
```

Both route families accept `GET` and `HEAD`; other methods return `405`.
Unknown paths, including the private `/installers.json` bundle file, return `404`.
The installer routes issue temporary `302` redirects to exact, versioned GitHub
release assets with browser and CDN `no-store` headers. They require no browser
JavaScript, API lookup, CORS configuration, R2, database, or visitor credentials.
GitHub still hosts the installer bytes; these redirects do not remove that
availability dependency.

## Installed-client compatibility

`/latest.json` retains its existing schema, response bytes, cache policy, and
GitHub fallback behavior. A missing file returns `404`; invalid or unavailable
metadata returns `503`. Valid metadata must contain the stable version,
timestamp, required platform entries, signatures, and canonical GitHub release
URLs. The Worker returns its original validated bytes, without reconstructing
or rewriting the JSON. Installer catalog failures do not affect requests to the
currently deployed `/latest.json`.

Publication intentionally advances updater metadata and installer links
together. If installer validation or availability checks fail before deployment,
neither advances: the previous verified bundle remains available. A healthy
new Tauri updater asset can therefore wait for an installer-only publication
failure to be resolved. This keeps one release state and recovery path; it does
not provide independent updater and installer promotion.

The installer catalog is separate because a macOS DMG and Android APK are not
Tauri updater artifacts. It explicitly identifies Research stable and all six
installer kinds. It accepts only the canonical repository, same stable tag,
expected Research filenames, positive asset identities/sizes, and SHA-256
digests. Agent Dev/Prod builds and SDK releases do not populate these routes.

## Automatic publication

`Publish updater metadata` runs after a successful stable Maple `Release` run.
No marketing rebuild or additional release button is needed. The workflow:

1. Checks out trusted `master`, resolves the current stable tag to its commit,
   and proves a completed successful `release.yml` run for that exact
   repository, release event, tag and commit. A completed older release cannot
   overwrite the current latest release. Manual dispatch from `master` applies
   the same successful-release gate.
2. Downloads `latest.json` and validates its original bytes against GitHub's
   digest, size and updater schema. Selects exactly one uploaded installer for
   each kind from the actual release inventory, with digest and size metadata.
   Windows/Linux installer URLs must match the updater entries. All six
   installers receive a HEAD availability check; only transient network,
   throttling and server failures receive bounded retries.
3. Rechecks the resolved tag commit, successful release run, release identity,
   and every selected asset's identity, size, digest and URL immediately before
   deployment. Failed preflight leaves the previous deployed bundle intact.
4. Publishes `public/latest.json` and `public/installers.json` together through
   the existing `ASSETS` binding and Worker deployment. Cloudflare credentials
   are available only to the deployment step. Production publications are
   serialized.
5. Verifies the public `/latest.json` is byte-for-byte identical and all six
   public routes return the expected uncached redirects for both GET and HEAD.
   Verification retains up to 60 attempts with 10-second pauses, subject to the
   job's overall timeout. A failure after deployment
   does not mean deployment was rolled back; inspect the live bundle before
   retrying or rolling back.

Generated metadata is ignored by git. Never commit it, change signed updater
bytes, or use a local Wrangler login to publish production. Installer digest
metadata and HEAD checks are not a replacement for the core Release workflow's
build/signature/artifact verification. The publisher does not download the large
installers again.

## Initial cutover and recovery

Merge this Worker and publisher before switching marketing links. Inspect and
cancel obsolete queued or waiting publisher runs before changing approval rules
or dispatching the new publisher: an old run can still execute its old workflow.
An authorized operator must then configure `updates-production` for automatic publishing:
retain its master-only branch policy and environment secrets, and remove its
required-reviewer gate if one is still configured. The workflow's successful
release check is the publication gate; an environment reviewer would otherwise
add an approval to each release.

Dispatch `Publish updater metadata` from master once to publish the existing
current stable release, after verifying its successful Release run. This does
not rebuild installers or create a release. Confirm its public verification,
browser downloads, and ordinary curl GET/HEAD redirects for all six routes
before merging marketing's stable links. CI uses an explicit verification User
Agent, so its success alone does not establish parity with other clients. If
probes are blocked by Cloudflare edge policy, inspect the existing updater
exception and extend it only to these exact GET/HEAD paths when necessary,
preserving the existing `/latest.json` rule. Routine future releases are
automatic.

To repair a failed publisher, dispatch this sibling workflow for the current
successful release; do not rerun core Release or create another release merely
to repair downloads. For an intentional rollback, restore a previously verified
Worker version and its bundled assets together. Existing versioned GitHub assets
must remain available. Stop conflicting pending publishers before rollback so
they cannot immediately replace the restored bundle. Rolling back discovery does not downgrade already
installed clients. Legacy GitHub `/releases/latest` consumers remain supported;
future Agent publishing must preserve Research's release identity separately.

## Development

From the repository root, use Maple's pinned Nix shell and this service's lockfile:

```sh
nix develop --no-update-lock-file .#ci -c ./scripts/ci/updates.sh
```

For local iteration:

```sh
cd services/updates
bun install --frozen-lockfile --ignore-scripts
bun run test
bun run dev
```

`bun run check` formats, typechecks, tests, and performs a credential-free
Wrangler dry-run bundle. The tests also start the configured entry point in
real local workerd with isolated fixture assets and an ephemeral port, checking
the updater bytes and all download redirects without requesting GitHub assets. `bun run prepare:installers` provides the publisher's
prepare, recheck and verify-live commands. Its tests cover incomplete releases,
asset replacement, mismatched CI provenance, invalid catalog identities,
availability failures, and incorrect public responses.

`bun run deploy` is a production action; do not run it without explicit authority
and the intended Cloudflare account, route, and release bundle.
