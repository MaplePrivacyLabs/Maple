# Maple Dev on TestFlight

Maple Dev is a separate iOS application, `cloud.opensecret.maple.dev`, built
from the same Research source as production Maple. It has its own App Store
Connect record, subscription catalog, TestFlight groups, installed application,
and update stream. Installing or automatically updating Maple Dev does not
replace production Maple. Its App Store Connect listing is named
**Maple Research Dev**; the installed home-screen name is **Maple Dev**.

The development profile uses the existing development services:

| Setting | Maple Dev |
| --- | --- |
| Home-screen name | Maple Dev |
| App variant | `dev` |
| OpenSecret API and authentication | `https://enclave.secretgpt.ai` |
| Enclave approval environment | `development` |
| Billing | `https://billing-dev.opensecret.cloud` |
| Feature flags | `https://flags-dev.opensecret.cloud` |
| Hosted native authentication | `https://auth-dev.maple.ai` |

The profile is fixed at build time. It is not a user-selectable backend switch.
Development and production accounts remain in their respective existing
services; this app does not require another authentication or billing deployment.
All `VITE_*` values are public configuration and must never contain secrets.

The stable Research development website is `https://app-dev.maple.ai`.
Hosted native sign-in uses the separate Auth application at
`https://auth-dev.maple.ai`. Both sites are protected by Cloudflare Access;
browser validation requires an authorized Access session.

Set the public repository variable `MAPLE_IOS_DEV_AUTH_ORIGIN` to
`https://auth-dev.maple.ai`, exactly as pinned in the Dev profile in
`scripts/ci/ios-build-profile.py`. The native build requires this variable and
compiles it as `VITE_MAPLE_DEV_AUTH_ORIGIN`; a missing or different value fails
before dependency installation and signing setup. Artifact verification also
checks against the committed profile, even if the supplied variable and embedded
profile agree on the same wrong host. There is no production fallback.
Changing this destination requires a reviewed profile update and a new Dev
TestFlight build; neither a variable change nor a source change alters installed
apps. The development Pages build also receives this value; production builds
remove it. An unset variable preserves ordinary web previews but blocks a Maple
Dev native build.

For Google and GitHub, Maple Dev opens Auth's `/desktop-auth` entry with
`native_app_variant=dev`. The standalone `apps/maple-auth` application supports
that target and returns through `cloud.opensecret.maple.dev://auth`. Auth selects
its own `<origin>/auth/<provider>/callback` explicitly. Register those exact
callbacks with the providers and in the development project's backend callback
allowlists, then validate the hosted flow before distributing the native build.
Publishing Auth or setting the build variable does not configure these callbacks.
Native iOS Sign in with Apple uses the development project's allowed
`cloud.opensecret.maple.dev` audience and does not use this browser entry.

Research's built-in web authentication remains separate. Its provider
initiation omits `redirect_url`, so it continues to use the development project's
existing default callbacks. Moving native entry to standalone Auth does not
require changing those defaults or upgrading Research's frontend SDK. Preserve
any defaults still needed by web clients; add Auth's callbacks through
`additional_redirect_urls`. The SDK continuation and native handoff depend on
the initiating browser tab's state, so callbacks must return to the origin that
started that flow. See the [backend callback contract](../services/opensecret/docs/oauth-callbacks.md).

Use the stable Auth origin for native OAuth testing and the stable Research
website for web OAuth testing. Arbitrary PR preview origins are not implicitly
registered OAuth callback sites. Keep production callback settings separate,
and retain hosted-flow-before-native-build ordering for future handoff changes.

## Build and upload workflow

`.github/workflows/ios-dev-testflight.yml` builds and uploads on pushes to
`master` that change Research web or iOS inputs, chosen by the same
`app-change-detection.yml` selector as production. A manual dispatch from
`master` always builds. Other branches, forks, pull requests, tags, and GitHub
Releases cannot enter this workflow's signing or upload jobs. The independent production
`mobile-build.yml` keeps its change selection,
artifact names, and TestFlight destination. Its own serial queue also supports
a master-only manual dispatch to force a fresh production build.

The development workflow calls the shared `scripts/ci/ios-release.sh` with
`MAPLE_IOS_VARIANT=dev`. Omitting that variable continues to build production
Maple. It uses the repository-pinned Nix Apple shell, separate development Xcode
app caches, the shared unsigned ONNX dependency cache, and a separate artifact
and proof directory:

```text
apps/maple-research/frontend/src-tauri/target/ios-dev/Maple-Dev.ipa
apps/maple-research/frontend/src-tauri/target/ios-dev/ios-build-profile.json
apps/maple-research/frontend/src-tauri/target/reproducibility/ios-dev/
```

Before uploading, a fresh job downloads the same run's named artifact, verifies
its reproducibility proofs, and checks the IPA's bundle identity, embedded public
build profile, source commit, and expected build number. This profile records
the selected environment and frontend input hash; it does not by itself prove
the running application's network behavior. A failed verification stops
the upload. Credentials are introduced only in the signing and final upload
steps, within the existing protected `apple-signing` environment.

Before device testing, inspect `environment.VITE_MAPLE_DEV_AUTH_ORIGIN` in the
artifact's `ios-build-profile.json` and the IPA's embedded
`Payload/<app>.app/assets/maple-build-profile.json`. The committed expected Auth
origin is `https://auth-dev.maple.ai`; passing the profile checks proves this
configuration matches, not that provider callbacks or device handoff work.
Record the exact source commit and installed TestFlight build number, then
confirm Google/GitHub actually visit Auth and return to Maple Dev. Native Apple
success does not validate this hosted Google/GitHub path.

The development export sets `testFlightInternalTestingOnly=true`, which prevents
the IPA from being used for external TestFlight or App Store distribution. IPA
verification requires Xcode's exported `TFInternalTestingOnly` marker to be the
boolean `true`; an absent, false, or malformed marker blocks upload. The
reproducibility comparison normalizes only this exact marker on iPhoneOS, since
Xcode adds it during export. Other metadata remains part of the comparison, and
the final IPA checksum still covers the marker. Pre-export archives and simulator
apps do not require the export marker. Apple
processing and availability to the configured internal group are separate from
a successful upload; the workflow does not submit for App Review or manage
testers. See Apple's [internal tester documentation](https://developer.apple.com/help/app-store-connect/test-a-beta-version/add-internal-testers/).

### Build numbers and retries

The marketing version follows the normal source version. The development build
number is `<workflow run number>.<build attempt>`, distinct from the production
build numbering. The build job passes its exact number and artifact name to the
upload job, so retrying only a failed upload does not expect a new IPA.

If a build has already been accepted by Apple, do not retry its upload. For a
fresh build of current master, dispatch this workflow again, giving it a new
run number. Prefer a new dispatch over rerunning an older workflow after newer
builds have been uploaded: Apple requires increasing build numbers within a
version. The profile helper rejects numbers outside Apple's supported format.

The workflow serializes development runs with `queue: max` and does not cancel
in-progress work. GitHub can retain up to 100 pending runs. Ordering follows when
runs enter the queue, not an absolute commit-order guarantee; excessive queueing
or a failed run still requires operator attention. See [GitHub's concurrency
documentation](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/control-workflow-concurrency).

## Apple and credential setup

The separate app must exist in the same Apple development team, with the explicit
bundle ID, required capabilities, and its own App Store Connect record. Create
an internal TestFlight group for that record and enable automatic distribution
if each successfully processed upload should reach the group. Group membership
and automatic distribution are App Store Connect settings, not build-time
secrets. Internal testers must be eligible App Store Connect users with access
to this app; adding external testers would require a different distribution
policy and export.

The workflow reuses the existing `APPLE_API_ISSUER`, `APPLE_API_KEY`,
`APPLE_API_PRIVATE_KEY`, and `APPLE_TEAM_ID` secrets from `apple-signing`.
Its App Store Connect API key must be authorized for the new app and signing
resources. A second SecretSpec file or secrets project is not required just
because the app has a different bundle ID. If the existing key has restricted
application access, grant the required access or configure a separate authorized
key through the existing secret-management process. Never place Apple private
keys in the app or its public build profile.

Apple subscriptions belong to an app's catalog. Maple Dev needs its own Pro and
Max products; the production app's catalog is not shared. Its billing mapping
and verifier must recognize the development bundle and the corresponding
catalog. TestFlight uses Apple's sandbox even though the app is distribution
signed. Selecting the development Maple backend and selecting Apple's sandbox
are separate concerns.

## Validation boundaries

Workflow and profile tests can verify configuration, rejection of mixed
identities, and artifact checks without signing or uploading. A signed build
proves packaging and signing only. The artifact check reads the real bundle
identity and the packaged public profile of selected build inputs; it does not
independently decode Tauri's compiled frontend to prove endpoint usage. The build
checks that its frontend tree stays unchanged during packaging; runtime checks
must confirm the app actually uses the selected services. After the first
authorized upload, confirm
Apple processing, internal group availability, the distinct installed app and
visible development identity, development login, and purchase/restore through
the actual sandbox catalog. Confirm server-side receipt verification and
notifications separately. Production Maple still needs its own final sandbox
test through its own TestFlight listing before an IAP release.
