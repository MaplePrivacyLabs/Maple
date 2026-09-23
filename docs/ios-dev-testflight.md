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

The profile is fixed at build time. It is not a user-selectable backend switch.
Development and production accounts remain in their respective existing
services; this app does not require another authentication or billing deployment.
All `VITE_*` values are public configuration and must never contain secrets.

Set the public repository variable `MAPLE_IOS_DEV_AUTH_ORIGIN` to the canonical
HTTPS origin of the existing development website, with no path, query, fragment,
or credentials. The intended stable origin is `https://app-dev.trymaple.ai`,
serving Research's master development build. Provision and verify that site
before enabling this channel; naming it here does not establish a deployment.
The native build requires the variable and has no fallback to the
production website. The development Pages build receives the same value as
`VITE_MAPLE_DEV_AUTH_ORIGIN`; production builds remove that development setting.
An unset variable preserves ordinary web previews, but blocks a Maple Dev native
build. Setting the variable does not deploy the callback site.

Existing services still need to recognize the separate client identity before
all authentication paths work. Native Sign in with Apple requires deployment of
the optional per-project native audience support and configuration of the
development project's allowed `cloud.opensecret.maple.dev` audience. The existing
web Apple client ID remains unchanged. Browser-based OAuth also requires the
development callback site to deploy support for the separate app callback
scheme. Those backend and callback deployments are separate rollout gates;
building this app does not apply them automatically.

The current Dev entry targets Research's built-in `/desktop-auth` flow. The
standalone `apps/maple-auth` application currently accepts only the original
native target and returns the production callback scheme; it needs explicit Dev
target support before its origin can be selected here. Publishing that separate
auth site alone does not complete Maple Dev's sign-in integration.

Research currently pins `@mapleai/sdk` 4.0.1. Its provider initiation does not
select a `redirect_url`, so each enabled provider's existing development-project
default callback must return to this exact origin. Adding an
`additional_redirect_urls` entry alone does not select it. The SDK continuation
and native handoff use the initiating tab's `sessionStorage`; returning to
another origin cannot complete that flow. Prefer the development site's verified
current callback origin, and assess other development clients before changing
any shared default. Verify the same callback in the provider console, including
the existing Apple web Services ID's `<origin>/auth/apple/callback`. Selecting a
non-default callback would require a separate SDK and caller upgrade. See the
[backend callback contract](../services/opensecret/docs/oauth-callbacks.md).

Once these shared development defaults move, new OAuth attempts from arbitrary
PR preview origins cannot retain their cross-origin continuation state. Use the
stable development site for OAuth testing; previews can still serve other branch
validation. Production callback settings remain separate. Publish the compatible
hosted development flow before enabling native browser sign-in, and retain that
web-before-native ordering for future handoff protocol changes.

## Build and upload workflow

`.github/workflows/ios-dev-testflight.yml` starts for every push to `master`,
including documentation-only pushes. A manual dispatch is also available from
`master`. Other branches, forks, pull requests, tags, and GitHub Releases cannot
enter this workflow's signing or upload jobs. The existing production
`mobile-build.yml` keeps its current triggers, change selection, artifact names,
and TestFlight destination.

The development workflow calls the shared `scripts/ci/ios-release.sh` with
`MAPLE_IOS_VARIANT=dev`. Omitting that variable continues to build production
Maple. It uses the repository-pinned Nix Apple shell, separate development Xcode
and ONNX cache keys, and a separate artifact and proof directory:

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
