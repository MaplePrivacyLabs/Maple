# SDK publishing

Publish each SDK independently through a manual GitHub Actions workflow:

| Package | Version source | Workflow | Protected environment |
| --- | --- | --- | --- |
| `@mapleai/sdk` | `sdk/package.json` | [`sdk-publish-npm.yml`](../.github/workflows/sdk-publish-npm.yml) | `sdk-npm` |
| `maple-sdk` | `sdk/rust/Cargo.toml` | [`sdk-publish-rust.yml`](../.github/workflows/sdk-publish-rust.yml) | `sdk-crates` |

One run handles one SDK. Publishing both means starting both workflows. These
workflows never create GitHub Releases or tags, so they do not affect the
GitHub `/releases/latest` endpoint used by older desktop clients. npm's `latest`
dist-tag belongs to `@mapleai/sdk` in the npm registry and is independent of
GitHub Releases and Maple app updates.

The initial workflow supports stable `X.Y.Z` versions only. It publishes the
version already committed on protected `master`; it does not bump versions,
commit files, or release the Maple application. Prepare subsequent SDK version
changes in a normal PR, including the SDK's lockfile. Consumer upgrades are
separate choices; publishing does not update application dependencies.

## Consumer version policy

Prefer published SDK versions for client applications, with each consumer
choosing when to upgrade. Pin application manifests exactly and commit their
lockfiles. Read each consumer's current manifest and lockfile to determine its
selected version and source; the initial registry versions and command examples
in this guide are not current dependency declarations. The reusable proxy
library uses a compatible SDK requirement, so its embedding application can
choose the version. The standalone proxy has its own lockfile.
An SDK publication should not automatically upgrade every consumer, and an
older published pin is not a reason to block an unrelated client release.

Local SDK dependencies are supported during active development, including on
`master`. Edit the existing manifest and regenerate its lockfile with the
owning component's pinned tools; no special development mode is required:

- TypeScript can use `file:../../../sdk` from the Research frontend and return
  to an exact registry version when ready. A local package version does not
  freeze its source; frontend preparation builds the selected local SDK.
- Rust can use a Cargo `path` dependency or a root `[patch.crates-io]` override
  for `maple-sdk`. Keep the host app and embedded proxy on the same SDK source
  and version because they exchange SDK types. A patch belongs in the
  consuming Cargo workspace root, not only the SDK or proxy manifest. Update
  the selected dependency requirements and affected lockfiles together; a
  `version` alongside `path` checks compatibility, but still builds local
  source. Remove local overrides when returning to a published pin.

Research's `bunfig.toml` defaults to `install.frozenLockfile = true`. To switch
its SDK dependency, temporarily set only that setting to `false`, enter the
root's pinned Nix shell, and run **one** command from
`apps/maple-research/frontend/`:

```sh
# Published version:
bun --no-env-file add --exact @mapleai/sdk@3.5.2 --ignore-scripts
# Or local source:
bun --no-env-file add --exact @mapleai/sdk@file:../../../sdk --ignore-scripts
```

Restore `frozenLockfile = true` immediately, including if the command fails;
leave dependency-age and script-execution protections unchanged. Review the
manifest/lockfile delta, then run `just install` from the monorepo root to
validate the selected dependency through the normal frozen install path.

Before releasing a client, proxy, or CLI, inspect what that consumer will
actually ship. For Research's frontend, check the `@mapleai/sdk` entry in both
`apps/maple-research/frontend/package.json` and its adjacent `bun.lock`. For a Rust consumer,
run this in its pinned environment, substituting its manifest path:

```sh
cargo metadata --locked --format-version 1 --manifest-path PATH/TO/Cargo.toml \
  | jq '.packages[] | select(.name == "maple-sdk") | {version, source, manifest_path}'
```

A registry source and locked version identify the published crate; a null
source and the in-repository manifest identify local source. Inspect overrides
as well as direct dependencies. A local SDK can have unpublished changes even
when its version matches the registry. Compare the relevant published source
commit/provenance or package contents when deciding whether SDK runtime changes
will ship.

If a release includes unpublished SDK changes, recommend publishing that SDK
and pinning the affected consumer before releasing the client. This is a
release-preparation preference, not a mandatory gate or a new approval step.
An intentional release with local SDK source is allowed; record that source
and the exact monorepo commit in the release handoff. Registry publishing keeps
its existing protected workflow and authorization requirements below. Ordinary
client work does not authorize an SDK publication.

Validate the selected dependency mode and lockfile. SDK source and backend
integration checks continue to exercise the in-tree SDK; passing a client build
that consumes a registry version does not validate unpublished SDK source.

## Run a release

After the one-time setup below:

1. Open [Maple Actions](https://github.com/MaplePrivacyLabs/Maple/actions) and
   select the npm or Rust SDK publishing workflow. Choose **Run workflow** on
   `master`.
2. Enter the exact version committed for that SDK, keep `mode=trusted`, and
   set `dry_run=false`. This single run builds and validates the package before
   requesting publishing approval.
3. The CTO reviews that run's package and source commit, then approves the
   pending `sdk-npm` or `sdk-crates` environment. Check the completed run's
   registry verification and package URL.

A separate dry run is optional. Leaving the default `dry_run=true` builds and
validates without publishing or requesting environment approval. If you use
one, review the actual publish run's commit and artifact too: `master` may
have changed between runs.

For the CLI, these commands from `sdk/` dispatch dry runs:

```sh
just publish-npm 3.5.2
just publish-cargo 3.6.2
```

The recipes accept `VERSION MODE DRY_RUN`. To publish an approved version, use
`just publish-npm VERSION trusted false` or
`just publish-cargo VERSION trusted false`. They only call GitHub; no registry
credentials or package publication run on the local machine.

The equivalent GitHub CLI dispatch is:

```sh
gh workflow run sdk-publish-npm.yml --repo MaplePrivacyLabs/Maple --ref master \
  --raw-field version=3.5.2 --raw-field mode=trusted --raw-field dry_run=true

gh workflow run sdk-publish-rust.yml --repo MaplePrivacyLabs/Maple --ref master \
  --raw-field version=3.6.2 --raw-field mode=trusted --raw-field dry_run=true
```

GitHub also exposes the same workflow inputs through its workflow-dispatch API.
See [manually running a workflow](https://docs.github.com/en/actions/how-tos/manage-workflow-runs/manually-run-a-workflow).

## GitHub environment setup

Create the repository environments `sdk-npm` and `sdk-crates` with these
settings before any real publish:

- Allow deployments from the branch `master` only; do not allow tags.
- Require reviewer `AnthonyRonning` (GitHub user ID `101225832`, the CTO).
- Allow that reviewer to approve a run they started, so the CTO can release
  without depending on another team member.
- Disable administrator bypass of the environment's protection rules.

The registry trust must name the exact environment from the table above.
Keep the repository's protected `master` review and CTO bypass policy. Normal
trusted publishing needs no stored npm or crates.io publishing token.

## First publication, entirely in Actions

A new package needs one bootstrap publication before configuring its registry
trusted publisher. Do this separately for npm and crates.io. The workflow's
`bootstrap` mode is restricted to a package that does not yet exist in its
registry; it cannot publish subsequent versions.

First merge the workflows, configure the protected environments, and run a
`mode=bootstrap`, `dry_run=true` validation for the chosen package and version.
Do this before creating the temporary bootstrap token.
The intended first versions are `@mapleai/sdk@3.5.2` and `maple-sdk@3.6.2`.
Merging these workflows or running a dry run does not publish either package.

### npm bootstrap

1. Use the verified npm account that owns the `mapleai` organization, with 2FA
   enabled. Create a granular access token with read/write access to the
   `@mapleai` package scope, no organization administration permissions,
   **Bypass 2FA** enabled, and a one-day expiry. Scope access is needed because
   the new `@mapleai/sdk` package does not yet exist to select individually.
   See [npm access-token creation](https://docs.npmjs.com/creating-and-viewing-access-tokens/).
2. Store the token as the **environment secret** `NPM_BOOTSTRAP_TOKEN` in
   `sdk-npm` through GitHub Settings → Environments → sdk-npm. The CLI
   alternative prompts for the value without including it in command history:

   ```sh
   gh secret set NPM_BOOTSTRAP_TOKEN --repo MaplePrivacyLabs/Maple --env sdk-npm
   ```

3. When the exact first version and commit are approved for publication, run
   `sdk-publish-npm.yml` on `master` with `mode=bootstrap` and `dry_run=false`.
   Review and approve its environment. Confirm the package appears at the URL
   reported by the run.
4. In the new npm package's settings, add the GitHub trusted publisher with
   organization `MaplePrivacyLabs`, repository `Maple`, workflow filename
   `sdk-publish-npm.yml`, and environment `sdk-npm`. Under **Allowed actions**,
   explicitly permit direct `npm publish`; new configurations may otherwise
   allow only staged publication.
5. Revoke the bootstrap token in npm and delete `NPM_BOOTSTRAP_TOKEN` from
   `sdk-npm`. Set package publishing access to **Require two-factor
   authentication and disallow tokens**. Trusted publishing continues to work
   under that setting. Subsequent runs use `mode=trusted`.

The workflow's npm trusted publication includes provenance. See
[npm trusted publishing](https://docs.npmjs.com/trusted-publishers/) for the
registry settings and provider requirements.

After bootstrap and trust configuration, the first successful trusted run
establishes that npm accepts this repository's OIDC identity. Preserve the
existing repository-wide OIDC subject configuration; Windows signing shares it.

### crates.io bootstrap

1. Use the GitHub-linked crates.io account and verify its work email. Create
   a token restricted to the exact crate `maple-sdk`, with **publish-new**
   permission only and the shortest practical expiry. No owner-management or
   existing-version publication permission is needed for bootstrap.
2. Store it as the **environment secret** `CRATES_BOOTSTRAP_TOKEN` in
   `sdk-crates`, using the GitHub UI or the interactive CLI prompt:

   ```sh
   gh secret set CRATES_BOOTSTRAP_TOKEN --repo MaplePrivacyLabs/Maple --env sdk-crates
   ```

3. When the exact first version and commit are approved for publication, run
   `sdk-publish-rust.yml` on `master` with `mode=bootstrap` and `dry_run=false`.
   Review and approve its environment, then confirm the published crate and
   checksum reported by the run.
4. In the crate's trusted-publishing settings, add organization
   `MaplePrivacyLabs`, repository `Maple`, workflow filename
   `sdk-publish-rust.yml`, and environment `sdk-crates`. Enable the crate's
   trusted-publishing-only setting.
5. Revoke the bootstrap token in crates.io and delete `CRATES_BOOTSTRAP_TOKEN`
   from `sdk-crates`. Subsequent runs use `mode=trusted`.

See [crates.io trusted publishing](https://crates.io/docs/trusted-publishing).
Never paste bootstrap tokens into chat, commits, logs, workflow inputs, or
repository variables. They are temporary protected environment secrets.

## Publishing trust boundary

Pull requests cannot publish. Manual releases require the canonical repository,
protected `master`, an exact committed package version, and the SDK environment
approval. Both workflows reject prereleases and previously published versions;
stable releases must advance the registry's existing stable version.

Build and package validation run without publishing secrets or OIDC permission.
The publisher uses a fresh runner, validates the same run's artifact against
its expected identity, version, source commit, and digest, and publishes those
exact bytes. It does not run package lifecycle scripts or Rust build scripts
with registry credentials. Rust uses the registry upload API for the prepared
`.crate`; npm uploads the prepared `.tgz` with lifecycle scripts disabled.

A dry run provides build and package evidence; it does not prove the registry
trust or a real upload works. npm's
[publish-time scanning](https://github.blog/changelog/2026-07-28-npm-publish-time-malware-scanning-and-dual-use-metadata/)
usually delays availability by about 5–15 minutes and can take longer. After
upload, Actions checks the registry read-only for up to 20 minutes for npm or
2 minutes for crates.io; it never retries the upload. A longer delay can leave
the run failed with publication unconfirmed even if the upload was accepted.

Check the registry before any retry. If a fresh attempt is needed, start a new
dispatch or choose **Re-run all jobs**. **Re-run failed jobs** cannot reuse a
package artifact from an earlier run attempt. Published versions are immutable;
the workflow will not overwrite one or move npm's `latest` backwards.
