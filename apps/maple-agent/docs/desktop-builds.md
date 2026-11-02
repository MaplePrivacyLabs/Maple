# Agent desktop builds

`Maple Agent Desktop Builds` (`.github/workflows/agent-desktop-build.yml`)
builds the desktop packages:

- **Master push** that changes Agent inputs: signed Prod for macOS, Linux and
  Windows x64. Its `changes` job runs `scripts/ci/plan.py only agent-desktop`;
  an unusable diff builds.
- **Nightly** (cron 10:37 UTC; GitHub starts scheduled runs hours late): Dev
  for every platform.
- **Dispatch**: Dev and Prod for every platform, or, with `release=true`, Prod
  for every platform plus publication ([releasing](updates.md#releasing)).

Windows ARM64 builds natively on GitHub's slow Windows ARM image, so it builds
only nightly, on dispatch and for releases. Master runs are never cancelled. A
later push without Agent changes doesn't retry a failed run: re-run its failed
jobs or dispatch.

The unsigned Dev packaging rehearsals in `ci.yml` (`agent-package-*`) run
nightly and on pull requests with the `ci:package` label
([CI labels](../../../docs/ci.md#labels)). Add it when a PR changes packaging
scripts, the Agent flake or the toolchains.

| Profile | Application | macOS bundle ID | Data namespace | Updates |
| --- | --- | --- | --- | --- |
| Dev | Maple Work Dev | `cloud.opensecret.maple.agent.dev` | `maple-agent-dev` | None; Actions artifacts |
| Prod | Maple Work | `cloud.opensecret.maple.agent` | `maple-agent-prod` | Signed Agent feed |

The binary embeds its profile, hosted API settings and PCR environment;
`--build-info` prints that identity as JSON.

## How packages are built

Each platform compiles in a job without signing credentials and hands the binary
to a signing job:

- **macOS:** compiles on Blacksmith's macOS 27 image. That image has no Metal
  Toolchain, so a GitHub `xcode-27` job exports Xcode's as a one-day artifact
  first; a retry more than a day later must re-run that export job too. The
  `desktop-signing` job signs, notarizes and staples the app and the DMG.
- **Linux:** builds with `cargo zigbuild` against glibc 2.31 and packages a tar
  bundle in the same job. No signing.
- **Windows:** compiles with a static CRT. The `windows-signing` job
  Authenticode-signs the executable, the NSIS installer and its generated
  uninstaller through Azure Artifact Signing, then writes the updater archive.

Signed jobs attest their final files with GitHub build provenance; a release
attests the Linux files from its publish job. To check a downloaded file:

```sh
gh attestation verify FILE -R MaplePrivacyLabs/Maple \
  --signer-workflow MaplePrivacyLabs/Maple/.github/workflows/agent-desktop-build.yml
```

## Download and install

Artifacts are named `maple-agent-PROFILE-PLATFORM-RUN`, where `PLATFORM` is
`macos-aarch64`, `linux-x86_64`, `windows-x86_64` or `windows-aarch64`. Final
packages are kept for 30 days. The intermediate unsigned binaries are
`prebuilt-maple-agent-PROFILE-PLATFORM-RUN`, kept for 7 days.

**macOS:** open the DMG and drag the app into Applications. Both profiles can
coexist with Maple Research.

**Windows:** run the `-setup.exe` installer as your desktop user. x64 needs
Windows 10 version 1703 or later; ARM64 needs Windows 11. Dev and Prod install
separately, per user. Uninstalling keeps account, settings and task data.

**Linux:** extract the whole archive, then run the installer from a terminal:

```sh
tar -xzpf maple-agent-prod-VERSION-SHA-linux-x86_64.tar.gz
./maple-agent.app/install.sh
```

`-p` keeps the archive's file modes, which the installer requires. The Dev
archive uses `maple-agent-dev.app`. The installer creates
`~/.local/maple-agent.app` (Dev: `~/.local/maple-agent-dev.app`), a command in
`~/.local/bin` and a desktop entry; search for **Maple Work** in Activities.
Python 3 is needed for the first install only; later versions arrive through the
updater. To run without installing, start `./maple-agent.app/AppRun` from a
terminal. The bundle carries its keyboard, X11, Wayland and audio client
libraries; the host supplies glibc, graphics drivers, CA certificates and the
desktop session. On NixOS, use the flake package or nix-ld.

## Packaging commands

From `apps/maple-agent`, in the pinned shell (`just package-rehearsal` runs the
first two):

```sh
nix develop --no-update-lock-file . -c ./scripts/build-release.sh dev
nix develop --no-update-lock-file . -c ./scripts/package-release.sh dev --unsigned
```

On Windows, in Git Bash with the toolchain from
`.github/actions/setup-agent-windows` and
`RUSTFLAGS='-D warnings -C target-feature=+crt-static'`:

```sh
./scripts/build-release.sh dev
MAPLE_PACKAGE_BUILD_NUMBER=1 python3 scripts/windows-release.py package dev --unsigned
```

For ARM64, set `MAPLE_WINDOWS_ARCH=aarch64` for the build and pass
`--arch aarch64` to packaging. Signed packaging runs only in the master signing
jobs, without `--unsigned`.
