# Research Dev desktop package

These manual macOS and Linux packages use hosted Dev and rehearse Research's
native OAuth handoff through the standalone Auth Dev site. They install beside
production Maple. They are separate from the ordinary `just desktop-dev` Local
workflow and from the existing iOS Dev TestFlight build.

## Fixed profile

| Setting | Research Dev desktop |
| --- | --- |
| Display name | Maple Research Dev |
| Bundle ID and callback scheme | `cloud.opensecret.maple.dev` |
| OpenSecret API | `https://enclave.secretgpt.ai` |
| PCR environment | `development` |
| Client ID | `ba5a14b5-d915-47b1-b7b1-afda52bc5fc6` |
| Browser sign-in origin | `https://auth-dev.maple.ai` |
| Flags / billing | `https://flags-dev.opensecret.cloud` / `https://billing-dev.opensecret.cloud` |
| Updater | Disabled; install a new Dev package manually |

The source of these inputs is
[`desktop-dev-profile.json`](../frontend/src-tauri/desktop-dev-profile.json).
The build script overwrites environment selection without editing ignored env
files. The Rust build rejects a mismatched renderer environment, bundle identity,
callback scheme, or updater configuration. The TypeScript SDK pin and production
profile are unchanged.

## Build locally

Run this from the repository root on Linux or macOS (with pinned Xcode on macOS):

```bash
nix run --no-update-lock-file .#research-dev-desktop
```

Nix supplies the pinned build tools; the script selects the fixed hosted Dev
profile. Stop running frontend development servers first: this script reinstalls
frontend dependencies and rebuilds `dist`. It does not change `.env.local` or
publish, upload, sign, or install the result. A Dev account is required; a
production account is not automatically available on hosted Dev.

### Fedora / Linux RPM

The Linux build produces an unsigned host-architecture RPM:

```text
apps/maple-research/frontend/src-tauri/target/research-dev/maple-research-dev-<version>-1.<arch>.rpm
apps/maple-research/frontend/src-tauri/target/research-dev/build-profile.json
```

Install the exact RPM printed at the end of the build, for example on x86_64:

```bash
sudo dnf install ./apps/maple-research/frontend/src-tauri/target/research-dev/maple-research-dev-<version>-1.x86_64.rpm
```

Replace `<version>` with the built version. DNF installs WebKitGTK automatically.
The Dev profile declares its own `libwebkit2gtk-4.1.so.0()(64bit)` RPM capability;
it does not inherit or require a change to the production RPM dependency policy.
This capability is provided by Fedora's `webkit2gtk4.1` and openSUSE's
`libwebkit2gtk-4_1-0` packages. Package metadata compatibility is not a substitute
for install-and-launch validation on each target distribution.
Launch **Maple Research Dev** from KDE's application launcher or run
`maple-research-dev`. Production keeps the `maple` RPM and command; Dev uses
`maple-research-dev` for the package, command, desktop entry, icon, and private
ONNX Runtime directory. Config/data and single-instance routing use the separate
`cloud.opensecret.maple.dev` app ID. The script verifies the final RPM's package
name, dependencies, ELF metadata, callback registration, and isolated file paths.

The intermediate DEB is not a supported install artifact. Linux build files live
under `target/research-dev/cargo` rather than replacing ordinary desktop outputs.
Only this explicit local command builds the Dev RPM; existing PR/release package
jobs and production identities are unchanged.

Older `ci-desktop-pr` RPMs use Dev services but retain the production `maple`
package identity. They do **not** install beside production. Restore the website
RPM to that slot separately; for equal versions use `sudo dnf reinstall ./<rpm>`,
or `sudo dnf downgrade ./<rpm>` if the website version is older. Sign in again if
needed; do not copy credentials between the two apps. No automatic data migration
or deletion is performed by the Dev build.

### macOS

From the repository root on macOS, with the pinned Xcode installed:

```bash
nix develop --no-update-lock-file .#ci -c ./scripts/ci/research-dev-desktop.sh
```

This builds a debug, host-architecture `.app`, without signing, notarization,
upload, a new release lane, or updater artifacts. It verifies the actual bundle's
identifier, display name, and registered scheme and records the executable hash
and frontend tree hash. It uses Research's existing macOS SDK linker setup and
rejects executable dependencies or runtime search paths pointing into the build
host's filesystem, including the Nix store:

```text
apps/maple-research/frontend/src-tauri/target/debug/bundle/macos/Maple Research Dev.app
apps/maple-research/frontend/src-tauri/target/research-dev/build-profile.json
apps/maple-research/frontend/src-tauri/target/research-dev/maple-research-dev-macos.tar.gz
```

CI's unprivileged `research-dev-macos` lane runs this same script on a macOS
runner and uploads `maple-research-dev-macos-pr` for review. That artifact is a
local-install rehearsal package, not a signed release or deployment. The lane
checks out the exact PR head; tie the downloaded package and its profile hashes
to that source run and checkout log when recording test evidence.

The lane runs when the Dev profile or this script changes, or when a PR carries
`ci:research-dev-macos` (or `ci:package`). Labels are read when the run starts:
add the label, then re-run the `CI` workflow. Other native changes use the
`research-desktop-*` lanes.

The different bundle ID separates the WebView's stored credentials and Tauri's
app config/data directories from the production app. Dev does not run the old
production-directory cleanup or inherit access to `$HOME/.config/maple`.
Automatic and manual updater commands are disabled in native code, including
installation/restart commands; changing a renderer preference cannot enable them.

## OAuth contract and rehearsal

The package reuses the existing Dev variant's native handoff and emits:

```text
https://auth-dev.maple.ai/desktop-auth?provider=<provider>&transport=v2&native_session_id=<id>&native_request_id=<id>&native_app_variant=dev
```

The return contains only the one-use grant and uses
`cloud.opensecret.maple.dev://auth`. A Dev build ignores production callback
schemes, and production ignores the Dev scheme. The Dev handler reads a buffered
launch URL after subscribing to subsequent callbacks; both paths use the same
pending-attempt and account-confirmation checks. A process that exited has lost
its in-memory native attempt, so an unsolicited cold launch cannot sign it in:
start a new sign-in from the app. Duplicate delivery cannot install twice.

Before real provider tests, Auth Dev must be published with the matching Dev
backend/client, additive callback settings and provider registrations. Building
this package does not configure or deploy those services. The current iOS Dev
`MAPLE_IOS_DEV_AUTH_ORIGIN` and Research's built-in web auth flow remain unchanged;
this package does not require moving the existing App Dev hostname.

Validate the installed Dev package alongside production: check the correct app
opens for each scheme, exercise an already-running return and startup delivery,
verify automatic and manual opening, reject an unsolicited/other-app return,
and confirm production state and updater remain separate. Then test all intended
providers and same-account/different-account confirmation on the real Auth Dev
host. Source tests, package identity verification, OS dispatch, and successful
provider sign-in are separate evidence; none substitutes for the others.
