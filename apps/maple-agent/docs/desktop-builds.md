# Agent desktop builds

`Maple Agent Desktop Builds` builds both hosted profiles on every protected
`master` push and on a manual dispatch from `master`. macOS ARM64 builds are
Developer ID signed, notarized, and stapled. Linux x86_64 builds are AppImages
with a bundled runtime; CI also launches each downloaded AppImage in an Ubuntu
24.04 container with no Nix installation and no network access.

| Profile | Application | macOS bundle ID | Configuration/data namespace | Update tags |
| --- | --- | --- | --- | --- |
| Dev | Maple Agent Dev | `cloud.opensecret.maple.agent.dev` | `maple-agent-dev` | `maple-agent-dev-vX.Y.Z`, prerelease |
| Prod | Maple Agent | `cloud.opensecret.maple.agent` | `maple-agent-prod` | `maple-agent-vX.Y.Z`, stable |

The binary embeds its profile, hosted API/client/billing settings, and PCR
environment. `--build-info` prints the public build identity as JSON without
opening the application or reading a saved account. Package verification checks
that identity against the requested profile and the package manifest. Packaged
Dev and Prod use their own state directories and do not adopt legacy Agent or
Research state. The development launcher retains its existing workspace
configuration and uses a separate debug app identity.

## Download and install

Open the latest successful `Maple Agent Desktop Builds` run on GitHub Actions.
Its artifacts include the profile, platform, run ID, and attempt in their names:

- `maple-agent-dev-macos-aarch64-RUN-ATTEMPT`
- `maple-agent-prod-macos-aarch64-RUN-ATTEMPT`
- `maple-agent-dev-linux-x86_64-RUN-ATTEMPT`
- `maple-agent-prod-linux-x86_64-RUN-ATTEMPT`

Artifacts are retained for 30 days. macOS packages contain a DMG and application
archive; drag the chosen Agent app into Applications. Both Agent profiles can
coexist with Maple Research. Linux packages contain an AppImage; make it
executable and launch it. Each artifact also contains public build information,
a package manifest, and checksums.

Pull requests produce unsigned macOS previews and Linux packaging evidence on
fresh hosted runners. Their macOS artifacts include `unsigned` in the name and
cannot satisfy the signed-artifact verifier.

## Packaging commands

From `apps/maple-agent`, using the pinned component shell:

```sh
nix develop --no-update-lock-file . -c ./scripts/build-release.sh dev
nix develop --no-update-lock-file . -c ./scripts/package-release.sh dev --unsigned
nix develop --no-update-lock-file . -c ./scripts/verify-release.sh dev dist/dev --unsigned
```

Use `prod` for the production service profile. Official macOS signing is run
only by the trusted CI job using the existing `desktop-signing` GitHub
environment. Cargo and native compilation run before that job step receives
Apple credentials. Linux packaging needs no Apple credentials.

## Distribution and update boundaries

These builds are Actions downloads. They do not create GitHub Releases or
advance an updater feed. The application discovers only releases in its own
profile's tag namespace and opens their release page for a manual download;
it does not install an update. Dev and Prod reject each other's releases, and
Research's bare `vX.Y.Z` releases are excluded.

A future Agent publisher must use namespaced tags and `make_latest: false`,
preserving Research's repository-wide latest release. Release publication and
any Research-to-Agent migration require their own explicit authorization. No
Research install is replaced, overwritten, or migrated by this workflow.
