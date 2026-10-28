# Agent desktop builds

`Maple Agent Desktop Builds` builds, signs and verifies Prod packages when a
protected `master` push changes Agent inputs, Dev packages every night from the
`master` tip (10:37 UTC), and both profiles on a manual dispatch from `master`.
Its `changes` job runs `scripts/ci/plan.py only agent-desktop` on the push's own
diff; an unusable diff or a selector error builds. No master run is
cancelled, so every Agent push gets its own packages. A later push without Agent
changes does not retry a failed run: re-run its failed jobs, dispatch, or fix
forward. The unsigned Dev packaging rehearsals in `ci.yml`
(`agent-package-linux`, `agent-package-macos`, `agent-package-windows`) run
nightly and on pull requests with the `ci:package` label; add it when a PR
changes packaging or release scripts, the Agent flake or the toolchains.

Unrelated master commits have no new Agent artifacts. Existing downloads keep
their original source SHA and provenance; they are never relabeled as a newer
commit. A manual dispatch from `master` forces fresh packages, including when
the previous run's 30-day artifacts have expired. macOS ARM64 builds are
Developer ID signed, notarized, and stapled. Linux x86_64 builds are tar bundles
with a bundled runtime and a normal application directory. CI checks each
downloaded bundle's command-line startup and detached host lifecycle in an
Ubuntu 24.04 container with no Nix installation and no network
access. Each master run completes independently. Windows x64 packages are
per-user NSIS installers requiring Windows 10 version 1703 (build 15063) or later. This baseline supplies the OS `icuuc.dll`
imported by GPUI; see Microsoft's [Windows ICU version history](https://learn.microsoft.com/en-us/windows/win32/intl/international-components-for-unicode--icu-).
Trusted master
packages use the existing Azure publisher to Authenticode-sign the application,
embedded uninstaller and final installer. Pull requests that change those
Windows packaging inputs, or carry the `ci:package` label, build, install and
smoke-test an unsigned Dev installer.

| Profile | Application | macOS bundle ID | Configuration/data namespace | Update delivery |
| --- | --- | --- | --- | --- |
| Dev | Maple Work Dev | `cloud.opensecret.maple.agent.dev` | `maple-agent-dev` | Disabled; master Actions artifacts |
| Prod | Maple Work | `cloud.opensecret.maple.agent` | `maple-agent-prod` | Platform-specific signed Agent feed; publication separately gated |

The binary embeds its profile, hosted API/client/billing settings, and PCR
environment. `--build-info` prints the public build identity as JSON without
opening the application or reading a saved account. Package verification checks
that identity against the requested profile and the package manifest. Packaged
Dev and Prod use their own state directories and do not adopt legacy Agent or
Research state. The development launcher retains its existing workspace
configuration and uses a separate debug app identity.

macOS compiles each profile in a job without signing credentials and uploads
its prebuilt binary. Packaging runs on a fresh runner that downloads that
binary and checks its embedded profile and source commit against the run's
checkout before the signing step receives Apple credentials. It reads a dedicated
Mach-O JSON section without executing artifact code, including before signing.
No app binary runs anywhere on the signing runner. Packaging does not restore a
Cargo cache or compile the application. For Prod, the packaging runner first
compiles a small updater-only scanner from its trusted checkout, before the step
receiving Apple credentials. After packaging, that tool reads the actual
`.app.tar.gz` through the installer's archive scanner, without extracting or
executing the app, and checks its SHA-256 against the package checksums before
final artifact upload or attestation. Dev uses a different bundle root and has
no production updater. PR rehearsals build Dev only, on runners without signing
credentials, and run the same prebuilt verification. Runtime metadata, duplicate
Swift runtime, and real GPUI window/first-render checks run only in credential-free
verification. GUI startup uses empty temporary state and does not sign in.

Windows also compiles without signing credentials, linking the CRT statically.
A fresh packaging runner reads the executable's dedicated read-only `.maple`
PE metadata section and checks profile/source without executing artifact code.
Master packaging reuses `windows-signing` and the existing Azure signing helpers;
it does not need another Azure app registration or certificate profile. Native
verifiers check installed payload identity, system-only DLL imports, Authenticode
signatures for master packages, CLI startup and isolated install/uninstall. These
checks do not establish interactive rendering, browser sign-in or tray behavior.
The host detaches from the client's console and process group. If an enclosing
Windows job forbids breakaway, the host retains that job's limits and lifetime;
terminating the enclosing job also terminates its host.
Built-in Windows computer-use remains tracked separately in
[issue #1295](https://github.com/MaplePrivacyLabs/Maple/issues/1295).

## Download and install

Open the latest successful `Maple Agent Desktop Builds` run with completed
package verifiers on GitHub Actions: a push run for Prod, the nightly run for
Dev, or a dispatched run for both. A selector-only push run has no packages, and
the Agent publisher accepts only push and dispatched runs.
Its artifacts include the profile, platform, and run ID in their names:

- `maple-agent-dev-macos-aarch64-RUN`
- `maple-agent-prod-macos-aarch64-RUN`
- `maple-agent-dev-linux-x86_64-RUN`
- `maple-agent-prod-linux-x86_64-RUN`
- `maple-agent-dev-windows-x86_64-RUN`
- `maple-agent-prod-windows-x86_64-RUN`

Artifact names stay stable when failed jobs are retried, so verification can
reuse a successful profile's package from an earlier attempt. A rerun producer
replaces only its own profile/platform artifact after packaging verification;
the replacement has a new artifact ID and download link. Every package must
still match the run's source commit and pass the same native verification.
Internal binary artifacts use `macos-aarch64-prebuilt` or
`windows-x86_64-prebuilt` in their names.
They retain the same run/profile identity across retries, so a packaging-only
retry can reuse a successful compilation while that internal artifact is retained.

Final packages are retained for 30 days; internal prebuilt binaries for 7 days.
Windows packages contain an `-windows-x86_64-setup.exe` installer, an updater
`.tar.gz`, public build-info, package manifest and checksums. Run the installer as your desktop
user. Dev and Prod use distinct per-user installation directories, shortcuts and
uninstall entries, separate from Research. Uninstall removes installed program
files and integration while preserving account, settings and task data. Packaged
Prod uses a separate signed Windows v3 feed. The per-user installation keeps a
stable signed launcher and uninstaller; updates stage a new
`versions/<version>-<build>/` generation and atomically select it with
`current.json`, preserving the running generation. Dev has no production feed.
Closing a successfully completed interactive installer opens Maple. Silent
`/S` installs do not launch it. The executable, installer and uninstaller use
the profile's Maple icon. Normal GUI startup also refreshes the icon for this
profile's existing Start menu shortcuts and installed-app entry, including
upgrades from 0.0.12, while preserving the stable launcher and shortcut targets.
macOS packages contain a DMG and application
archive; drag the chosen Agent app into Applications. Both Agent profiles can
coexist with Maple Research. Linux packages contain
`maple-agent-PROFILE-VERSION-SHA-linux-x86_64.tar.gz`. Extract the archive and run
the contained `install.sh` as your desktop user:

```sh
tar -xzpf maple-agent-prod-VERSION-SHA-linux-x86_64.tar.gz
./maple-agent.app/install.sh
```

The `-p` option preserves the archive's reviewed file modes, including when your
shell uses a restrictive umask. The installer requires those modes unchanged.

The Dev archive uses `maple-agent-dev.app`. The installer creates a per-user
application directory at `~/.local/maple-agent.app` (Dev:
`~/.local/maple-agent-dev.app`), a command in `~/.local/bin`, and a desktop entry.
Python 3 is required for initial installation only. First install
refuses existing installation or integration paths; subsequent upgrades use the
application's updater. Its `AppRun` entrypoint starts the
bundled executable and runtime directly, without an AppImage mount or FUSE.
The host must provide its normal CA certificate trust store for TLS validation;
existing user certificate-path overrides remain supported. The isolated CI
container mounts the runner's public `/etc/ssl/certs` read-only and keeps TLS
validation enabled.
Compose and dead-key input use the host's `/usr/share/X11/locale` data when
available; an explicit `XLOCALEDIR` override remains unchanged.
Initial installation leaves profile, SQL, authentication, and settings data
untouched. Managed uninstall is not yet provided.
The final Linux artifact contains exactly that tar archive, `build-info.json`,
`manifest.json`, and `SHA256SUMS`. The archive has one application root,
including `bundle.json`, the entrypoint, executable, runtime libraries,
and installer helpers. Package verification extracts to temporary storage and
does not install into your application directory.

Native BSDtar creation explicitly disables xattrs, file flags, ACLs and macOS
metadata, while preserving signed file bytes, public modes and symlink targets.
`COPYFILE_DISABLE` alone does not suppress all PAX host metadata. Normal native
format support remains enabled for required long paths; unsupported metadata is
rejected by the real installer scanner. Existing extracted-archive codesign,
notarization, Gatekeeper and DMG comparison checks remain required.

Every final master package file, including its public metadata and checksums,
receives GitHub build provenance. The trusted macOS signing job mints provenance
after packaging. Linux compilation uses a read-only token and builds each
profile once. A separate trusted job checks out the run's exact master source,
compiles the updater-only archive scanner, and reads each downloaded tar through
the installer's Linux parser before attesting its final files. That job never
launches artifact code. Fresh verifiers repeat the actual archive scan and check the
downloaded files against this workflow, `master`, and the exact source commit.
For a downloaded file, verify the provenance with the GitHub CLI:

```sh
gh attestation verify FILE -R MaplePrivacyLabs/Maple \
  --signer-workflow MaplePrivacyLabs/Maple/.github/workflows/agent-desktop-build.yml \
  --source-ref refs/heads/master --source-digest SOURCE_SHA \
  --deny-self-hosted-runners
```

The source SHA is recorded in `build-info.json`. Provenance authenticates the
build origin; Linux checksums alone do not provide that authentication.

PR rehearsals upload unsigned Dev packages for seven days. Their artifacts
include `unsigned` in the name and cannot satisfy the signed-artifact verifier.
The preview app is ad-hoc signed without hardened runtime so macOS can load its
bundled libraries without a Developer ID Team ID. Signed master apps retain
hardened runtime. Preview verification rejects incompatible library-validation
flags; downloaded previews may still require macOS's **Open Anyway** approval.
PR previews are not attested.

The macOS GUI smoke establishes window creation, entry into the first render,
and process survival; it does not capture a presented frame or verify login, chat,
microphone or capture permissions. The Linux host smoke uses empty private state
and a sanitized environment. Two signed-out ACP clients must reuse one host and
receive a fixed sign-in or local store setup guard; stopping that isolated
host must release its owner lock and socket. These command-line checks do not exercise
X11/Wayland or host GPU drivers; those remain native runtime checks. Dev checkout
and portal requests use the Dev billing API, and pricing/payment return links
use `app-dev.maple.ai`, which is protected by Cloudflare Access. Prod uses
`app.maple.ai`. Profile changes require checkout success/cancellation and portal
return validation; package metadata alone does not prove those browser flows.

## Packaging commands

From `apps/maple-agent`, using the pinned component shell:

```sh
nix develop --no-update-lock-file . -c ./scripts/build-release.sh dev
nix develop --no-update-lock-file . -c ./scripts/package-release.sh dev --unsigned
nix develop --no-update-lock-file . -c ./scripts/verify-release.sh dev dist/dev --unsigned
```

On Linux these commands produce and verify the installed bundle tar. The
standalone AppImage builder remains a separate packaging option and is not the
official updater artifact. Linux metadata validation explicitly selects
`linux-x86_64`, including its platform-specific update feed; macOS metadata
continues to use the macOS feed. Dev has no update feed on either platform.
Both platforms retain one release version and source commit; separate feed
paths preserve compatibility with existing macOS clients.

Windows builds run on `blacksmith-8vcpu-windows-2025` with the pinned Rust/MSVC
environment from `.github/actions/setup-agent-windows` and
`RUSTFLAGS='-D warnings -C target-feature=+crt-static'`; signing stays on
GitHub's `windows-2025`. `build-release.sh` selects the explicit x64 MSVC target
so host build tools keep their normal linkage, then stages the exact resulting
executable.
From the component directory in Git Bash on Windows:

```sh
./scripts/build-release.sh dev
export MAPLE_PACKAGE_BUILD_NUMBER=1 # Actions supplies its actual github.run_number
python3 scripts/windows-release.py package dev --unsigned
python3 scripts/windows-release.py verify dev dist/dev --unsigned --smoke
```

The package script verifies its pinned NSIS compiler download. Signed packaging
uses the same command without `--unsigned`, only in the trusted master job after
Azure login and the existing pinned Artifact Signing module setup. The signing
runner never executes downloaded app code. Install/uninstall smoke runs only on
a separate credential-free verifier.
The exact five final files, build number, source/profile and fixed tar inventory
are checked before attestation and again after download. The tar is generated
from the same extracted installer generation, so its signed executable and
all public notice/descriptor bytes match. Native preflight checks destination
and code trust before installed files or registration change; a durable manual
install intent permits safe interrupted-install recovery. Unsigned Prod must
refuse before modification; unsigned Dev remains a positive install/GUI/uninstall
preview with no production feed or keys.
The native smoke also repeats the silent install without opening another
process, then completes the interactive installer and observes its actual
auto-launched window before the existing host-stop and uninstall checks.
Both GUI probes begin with legacy shortcut icon metadata and require the
canonical persistent artwork, unchanged shortcut targets and other properties,
and the profile's updated installed-app icon.

The downloaded verifier captures the signed checker's read-only preflight
output before invoking NSIS. On failure it records the install phase and only
the presence/type of fixed files and registration in its isolated fixture;
cleanup errors do not replace the original failure. NSIS exit codes distinguish
architecture/version probes (10–13), checker extraction (20), preflight launch
or child refusal (21–22), preparation (23–24), payload/registration (30–31),
activation (40–41), and uninstall (50–53). A failed installer is not retried by
the verifier. These diagnostics do not establish installed upgrade qualification.

For a produced Prod updater archive, compile the trusted data-only scanner and
validate the exact package directory (add `--unsigned` for unsigned packages):

```sh
nix develop --no-update-lock-file . -c cargo build --locked -p maple-agent-updater --example validate-release-archive
nix develop --no-update-lock-file . -c ./scripts/verify-updater-archive.sh dist/prod --unsigned
```

For Linux, pass `--platform linux-x86_64`; the scanner accepts both hosted
profiles (`--profile dev` for a Dev archive). The scanner checks the actual tar
bytes against `SHA256SUMS` and validates archive data without extracting or
executing the application.

Use `prod` for the production service profile. Official macOS signing is run
only by the trusted master CI job, reusing the existing `desktop-signing` GitHub
environment and its five Apple secrets. Application compilation runs on a
separate runner; the trusted updater scanner compiles before the job step receiving Apple credentials. Linux
packaging needs no Apple credentials.

## Distribution and update boundaries

These builds remain Actions downloads and do not advance an updater feed.
The independent Agent publisher promotes only verified Prod artifacts into the
Agent R2 bucket. It creates no GitHub Release/tag and does not redeploy Research.
The client accepts only its signed Agent product/channel/target manifest, with
the primary and backup keys embedded in the Prod profile. Packaged Prod builds
on macOS/Linux/Windows check their platform's feed. The Windows v3 feed binds
its generation `.tar.gz` separately from the manual `.exe` download and does
not change the installed macOS v1 or Linux v2 contracts. Publication remains separately gated, and
invitations require a signed native update rehearsal on every required launch
platform. An empty-key profile
still disables updater traffic. See
[updater activation](updates.md).

No new app-signing environment or credential migration is needed. Keep the
existing `desktop-signing` and `windows-signing` credentials and policies, Research updater key,
publication workflow and compatibility URLs intact. No Research install is
replaced, overwritten or migrated by Agent packaging or publication.

Windows updater archives use `maple-agent-windows-bundle-v2` and contain only
`bundle-info.json`, `maple-agent.exe`, `LICENSE.txt` and `build-info.json` under
`maple-agent-windows/`. The updater and publisher require that exact inventory.
The earlier, unpublished Windows bundle format is rejected. macOS and Linux
retain their existing formats and signed-manifest compatibility.
