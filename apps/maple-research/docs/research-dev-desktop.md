# Research Dev desktop package

This manual macOS package rehearses Research's native OAuth handoff through the
standalone Auth Dev site. It is separate from the ordinary `just desktop-dev`
Local workflow and from the existing iOS Dev TestFlight build.

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

From the repository root on macOS, with the pinned Xcode installed:

```bash
nix develop --no-update-lock-file .#ci -c ./scripts/ci/research-dev-desktop.sh
```

This builds a debug, host-architecture `.app`, without signing, notarization,
upload, a new release lane, or updater artifacts. It verifies the actual bundle's
identifier, display name, and registered scheme and records the executable hash
and frontend tree hash:

```text
apps/maple-research/frontend/src-tauri/target/debug/bundle/macos/Maple Research Dev.app
apps/maple-research/frontend/src-tauri/target/research-dev/build-profile.json
apps/maple-research/frontend/src-tauri/target/research-dev/maple-research-dev-macos.tar.gz
```

The unprivileged Desktop App PR Build workflow runs this same script as a
separate macOS job and uploads `maple-research-dev-macos-pr` for review. That
artifact is a local-install rehearsal package, not a signed release or deployment.

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
