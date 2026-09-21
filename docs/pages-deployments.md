# Pages publisher architecture

The repository builds and publishes static web assets through separate build
and credential-bearing publisher workflows. This document describes their
source contract and contributor validation. Live activation, environment
administration, cutover, and recovery are operator procedures outside this
public guide.

`MAPLE_PAGES_PREVIEW_ENABLED` and `MAPLE_PAGES_PRODUCTION_ENABLED` must equal
the literal string `true` for their respective jobs. A missing/false production
variable also enables the legacy branch-promoter job; it does not itself
configure Cloudflare's native builds. Merging source does not establish live
publisher settings or deployed state.

## Build and destination contract

| Source | Configuration profile | Destination |
| --- | --- | --- |
| Open internal PR targeting `master` | `pr` | `pr-N` Pages preview |
| Current `master` push | `pr` | `master` Pages preview |
| Latest stable Maple release with successful `Release` run | `release` | `pages-production` / `trymaple.ai` |

`Pages preview build` calls `scripts/ci/web.sh` with
`MAPLE_WEB_ENVIRONMENT=pr` for both PRs and master. It builds the PR head SHA,
not a production-configured master artifact. A separate unprivileged checkout
at the workflow/merge SHA supplies manifest tooling for older PR heads.
Path filters cover the existing web build inputs and the Pages CI files.
Fork PRs retain ordinary CI but receive no hosted preview. Pushes to other
branches without a qualifying PR do not get automatic previews from this path.

The fixed build settings come from `scripts/ci/_common.sh`:

| Setting | Preview | Production |
| --- | --- | --- |
| OpenSecret API | `https://enclave.secretgpt.ai` | `https://enclave.trymaple.ai` |
| PCR environment | `development` | `production` |
| Flags | `https://flags-dev.opensecret.cloud` | `https://flags.opensecret.cloud` |
| Billing | `https://billing-dev.opensecret.cloud` | `https://billing.opensecret.cloud` |

Both profiles retain the same public client ID. Vite embeds these public values
at build time; deployment does not substitute Cloudflare dashboard variables.
Production downloads the existing `maple-web-dist.tar.gz` and `web-final.sha256`
from the successful stable GitHub release, checks GitHub digests and checksums,
and uploads those bytes without rebuilding. A master push alone cannot publish
production. The existing master verification artifact is unchanged.

## Credential and artifact boundaries

The preview build has only `contents: read`, no protected environment, no CF
secrets, and no restored caches. It uploads exactly the archive and its manifest
under a name containing the GitHub run ID and attempt. `Publish Pages` executes
only its trusted default-branch checkout; it never executes PR scripts, installs
PR dependencies, or loads PR Wrangler configuration. Checkout credentials are
not persisted and external actions are pinned to commit SHAs. This separation
addresses the [GitHub Security Lab privileged PR execution pattern](https://securitylab.github.com/resources/github-actions-preventing-pwn-requests/).

The publisher rechecks repository IDs, workflow ID/path, event, successful run,
run attempt, source SHA, and the current open internal PR head or master head.
Production additionally checks the exact stable tag, successful release build,
reachability from master, latest-release identity, and forward-only production
ref movement. Superseded sources fail closed, including rerun attempts.

Artifacts remain untrusted data. ZIP/tar parsing bounds compressed and expanded
bytes, individual file sizes, entries, and paths. It rejects links, special
files, traversal, duplicates, ambiguous spelling, hidden paths (except the root
`.well-known` directory used for mobile app links), Functions,
`_worker.js`, Wrangler/package configuration, `_routes.json`, `_headers`, and
`_redirects`. Only static files are extracted, including a required `index.html`.
The publisher rechecks their hashes immediately before upload.

Pinned Wrangler dependencies are installed from trusted `services/updates/bun.lock`
with lifecycle scripts disabled, before CF credentials enter the final step.
Wrangler runs outside the checkout and artifact tree, with no artifact bundling,
an isolated home, and an allowlisted child environment. It receives CF credentials
but no GitHub/BWS token, runner command files, inherited Node options, or proxy
configuration. Its raw output is suppressed and structured results are checked.

A producer can fabricate a manifest alongside an artifact. Hashes and provenance
prove consistency and authorized origin, not that PR JavaScript is honest or
uses the declared endpoints. Internal PR review, Access, development accounts,
and browser smoke remain necessary. Do not enter production credentials into
an unreviewed preview. The publisher's token boundary is separate from browser
trust in the application being previewed.

## Verification semantics

CI verifies the Cloudflare deployment's successful stage, environment, project,
branch, URL, and commit; production also verifies the active canonical deployment.
It then advances the production ref without force and reports GitHub status.
That is deployment-state evidence, not an authenticated application smoke test.
Access-protected previews require a permitted browser; an automated HTTP 403
alone does not establish a broken application.

Both publisher jobs keep their protected environments but set
`environment.deployment: false`. This prevents GitHub's automatic job-completion
record for the publisher's master checkout from superseding the deployment
reported for the actual artifact SHA. The explicit deployment status owns the
production/preview URL. Branch policies, reviewers, wait timers, and secrets still
apply; custom deployment-protection GitHub Apps are incompatible with this mode
and make the job fail. See [GitHub's environment-without-deployment rules](https://docs.github.com/en/actions/how-tos/deploy/configure-and-manage-deployments/control-deployments#using-environments-without-deployments).

GitHub and Cloudflare do not provide an atomic transaction. An upload can
be active even when later source/ref/status checks fail. A workflow failure
therefore does not by itself establish that the previous deployment is still
serving. Deployment-state checks and authenticated application smoke are
separate evidence.

Offline checks from the repository root (use the host system for a local check):

```bash
nix build --no-update-lock-file --no-link --print-build-logs .#checks.x86_64-linux.pages
nix flake check --no-update-lock-file
MAPLE_WEB_ENVIRONMENT=pr nix develop --no-update-lock-file .#ci -c ./scripts/ci/web.sh
```

## Independent auth site

The standalone [Auth application](../apps/maple-auth/README.md) owns its
package, registry SDK pin, lockfile, assets, tests, configuration, build, and
publication path under `apps/maple-auth`. It imports no Research source or
configuration and does not use Research's dependency installation. The shared
protocol comes from the published SDK; the small hosted UI/helper copies are
maintained and tested within Auth. Research retains its built-in web auth,
existing SDK pin, and native entry URLs. Neither an Auth change nor an Auth
publication requires a Research release.

| Lane | Source and configuration | Result |
| --- | --- | --- |
| `Auth Pages CI` | PRs targeting any base, including forks and stacked branches; relevant master pushes; `pr` profile | Offline publisher checks, standalone Auth checks and Auth build; no Research build or publication |
| `Auth Pages build` | Manual dispatch on protected `master`; `release` profile | `maple-auth-production-RUN-ATTEMPT` artifact containing `maple-auth-dist.tar.gz` and `pages-artifact.json` |
| `Publish Auth Pages` | Separate manual dispatch on protected `master`, selecting the exact successful build run and attempt | Fixed `maple-auth` Pages project, `maple-auth.pages.dev`, `auth-pages-production` Git ref and protected environment, reported URL `https://auth.trymaple.ai` |

`MAPLE_AUTH_PAGES_PRODUCTION_ENABLED` must be the literal string `true` in both
the workflow and publisher process. It is off when absent, empty or false.
Merging these files starts neither production auth publication nor native-client
entry changes. No auth preview is automatically hosted. The native auth-entry
origin remains the existing apex until a separately authorized rollout changes it.

`scripts/ci/auth-ci.sh` installs and checks only Auth. `scripts/ci/auth-web.sh`
builds its `index.html` entry to `apps/maple-auth/dist`, then archives it under
`apps/maple-auth/target/reproducibility/maple-auth-dist.tar.gz`. Both scripts
use Auth's own helper, not Research/Tauri build tooling. Auth-only source edits
select the Auth lane without Research or Agent component checks/packaging.
Shared CI or release infrastructure edits can still select other affected lanes.

The fixed `pr` profile uses `https://enclave.secretgpt.ai` and development PCRs;
`release` uses `https://enclave.trymaple.ai` and production PCRs. Both use the
existing public Maple client ID. Build/run commands use Bun's `--no-env-file`,
and Auth's Vite configuration disables dotenv loading for these fixed builds.
Dependency installation uses Auth's frozen lockfile with lifecycle scripts
disabled. The pinned Bun 1.3.5 installer can still read local dotenv files
despite that flag; its child cannot alter the shell's fixed build profile.
Scripts never rename or move managed dotenv files. Fresh production checkouts
contain no managed workspace dotenv files. The pinned CI shell provides Node
(required by TypeScript/Vite CLI shebangs), Bun, and Python.

Auth pins published `@mapleai/sdk` 4.1.1; Research retains its own 4.0.1 pin.
Both fixed Auth profiles reject local SDK links and source overrides, require
an exact stable SDK version of at least 4.1.0, and check the installed package
name/version and resolution inside Auth's own `node_modules`. Future Auth
upgrades publish the SDK first, then update only Auth's manifest and lockfile.
The offline gate neither publishes the SDK nor queries the registry.
The bundle boundary rejects sibling application code, source SDK imports,
and the legacy SDK.

Before publishing an Auth change related to enclave trust or PCR rotation,
review `apps/maple-auth/src/config/openSecretClientConfig.ts` against the
approved development/production histories. Verify the combined app-provided and
pinned-SDK roots support the intended approved enclave when signed-history fetching
is unavailable, preserving environment separation. Include Research's
separate fallback in the [SDK consumer rollout review](sdk-publishing.md#rolling-an-sdk-fix-out-to-clients).
Record the Auth artifact/publication separately; a Research release does not
refresh the hosted Auth copy, and the two lists need not be byte-identical.

The auth publisher uses trusted master tooling and the same static archive,
download, Wrangler and credential boundaries described above. It accepts only
the auth build workflow's successful manual master run, current run attempt and
exact current master SHA in the expected repository. The archive name and
`auth-release` manifest profile cannot substitute for an app artifact. It
rechecks the selection before upload and after deployment. The auth production
ref must already exist, and advances without force; a stale build or non-forward
selection fails closed. Operators must create the project, ref, protected
environment, scoped credentials, custom-domain configuration and activation
variable separately. The project must have the fixed identity above and either
no Git source (a Direct Upload project) or an explicit
`source.config.production_deployments_enabled: false`. A present but malformed
Git-source configuration is rejected. The existing app project retains its
requirement for explicitly disabled native Git production builds.

Only the final auth deploy step receives CF credentials. Its protected
`auth-pages-production` environment uses `deployment: false`, with the same
protection and explicit artifact-SHA status semantics as the app publisher.
The auth path does not write `pages-production` or report the app's public URL.

Auth response headers come from the trusted publisher's fixed `AUTH_HEADERS`
constant, applied to `/*`: `Cache-Control: no-store, max-age=0`,
`X-Robots-Tag: noindex, nofollow`, `Referrer-Policy: no-referrer`,
`X-Frame-Options: DENY` and `Content-Security-Policy: frame-ancestors 'none'`.
The publisher creates `_headers` only after re-extracting and checking all
producer asset hashes. Producer `_headers`, redirects and worker configuration
remain forbidden; the app publisher adds no headers. These source rules do not
establish the live Cloudflare cache policy: auth cache bypass, actual response
headers, custom domains, TLS/Access and browser/native handoff must be verified
during the separate dark-publication rehearsal before redirect activation.

For unprivileged local Auth checks and build (no services or publication):

```bash
nix develop --no-update-lock-file .#ci -c ./scripts/ci/auth-ci.sh
MAPLE_AUTH_ENVIRONMENT=pr nix develop --no-update-lock-file .#ci -c ./scripts/ci/auth-web.sh
```

The Pages offline test target also covers auth provenance, SDK pinning, static
artifact rejection, fixed response headers and destination isolation. A passing
build or publisher test is source evidence; retained-session login, callback
allowlists and deployed handoff behavior require the later rehearsal.
