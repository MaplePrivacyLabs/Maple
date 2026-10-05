# App signing environments

App signing and store credentials belong in GitHub **environment secrets**,
not repository or organization secrets available to arbitrary branch workflows.
Branch protection on `master` alone does not protect repository-level secrets.

| Environment | Credentials | Jobs |
| --- | --- | --- |
| `desktop-signing` | `TAURI_SIGNING_PRIVATE_KEY`, `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`, `APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_ID`, `APPLE_ID_PASSWORD`, `APPLE_TEAM_ID`, `KEYCHAIN_PASSWORD` | macOS/Linux master and release builds |
| `apple-signing` | `APPLE_API_ISSUER`, `APPLE_API_KEY`, `APPLE_API_PRIVATE_KEY`, `APPLE_TEAM_ID` | Shared production iOS and Maple Dev builds and TestFlight submission |
| `android-signing` | `ANDROID_KEYSTORE_BASE64`, `ANDROID_KEY_ALIAS`, `ANDROID_KEY_PASSWORD` | Android master and release builds |
| `windows-signing` | `AZURE_*` signing configuration and both `TAURI_SIGNING_PRIVATE_KEY*` secrets | Windows master and release builds |
| `zapstore-publishing` | `ZAPSTORE_SIGN_WITH` | Existing best-effort publisher after a successful release |

Configure each environment with **selected branches and tags**: branch `master`
and tags `v*`. The existing branch and release-tag rulesets must continue to
protect those refs. Do not add a branch wildcard or allow PR merge refs.
Zapstore runs its trusted workflow from `master` after the release completes.

`desktop-signing`, `android-signing`, and `windows-signing` use the selected-ref
restrictions above without required reviewers. Reviewed code admitted to
protected `master` is trusted to use their production signing credentials, so
ordinary signed CI builds run automatically. These master jobs produce signed
Actions artifacts; they do not publish GitHub Releases or update production
Pages/updater deployments. The same environments also serve owner-controlled
`v*` release tags, so their signing jobs likewise have no separate approval prompt.
Creating a release and controlling its distribution remain separate boundaries.

`Maple Agent Desktop Builds` also uses the existing `desktop-signing` environment
for its trusted `master` macOS Dev/Prod jobs. The same Developer ID publisher
signs distinct Agent bundle identities; it does not replace Research's identity.
Agent contributor packaging, Linux compilation, and macOS compilation receive
no Apple or updater signing secrets. A fresh macOS packaging job downloads the
binary from the same workflow run and validates its profile and embedded source
revision against the exact checkout by reading a dedicated Mach-O metadata
section. It never executes artifact code, including
before the Apple credential step, and does not restore compilation caches or run
Cargo. App launches and Swift runtime checks run only on verifiers without Apple
or updater signing secrets. Agent builds upload separately named Actions artifacts.

The macOS signing job has `contents: read`, `id-token: write`, and
`attestations: write` to mint GitHub build provenance after packaging. Linux
compilation keeps a read-only token; a separate trusted `master` job has those
same provenance permissions and downloads the final Linux artifacts to attest
them. That job does not check out or execute repository code or launch artifacts.
Fresh verifiers have `contents: read` and `attestations: read` and receive no
Apple or updater signing secrets. For master packages, they require provenance
from this workflow, `master`, and the exact source commit. See
[Agent desktop builds](../apps/maple-agent/docs/desktop-builds.md).

Require release-owner approval for `zapstore-publishing`, `pages-production`,
and `auth-pages-production`, with self-review allowed and administrator bypass
disabled. Preserve the three Pages/updater publishers' `master`-only branch
policy and the existing SDK and PCR signing reviewer gates.
Unsigned PR builds and Pages previews keep their automatic behavior.

`updates-production` publishes the original updater metadata and Research
installer redirects automatically after a successful stable `Release` run.
Its manual recovery path requires the same successful repository, tag and commit
proof; it cannot promote an incomplete release. Once that hardened publisher is
merged, remove this environment's required reviewer while retaining its exact
`master` branch restriction and environment secrets. This is a one-time rollout
setting, not a new approval for each release. See the
[updater rollout checks](../services/updates/README.md).

`apple-signing` remains automatic for now because Maple Dev and production iOS
share its credentials. Gating this environment would also interrupt the
automatic Maple Dev TestFlight lane, potentially at both its build and upload
jobs. Production iOS therefore remains an explicit exception until the
credentials are separated. A second GitHub environment containing a copy of
the same production-capable key would not close that boundary: the automatic
development lane must lose access to production-capable credentials first.
Do not switch a workflow to a new environment before its credentials and
policies are ready.

Keep the `windows-signing` name unchanged because Azure's OIDC trust uses that
environment identity. Before approving any reviewer-gated job, inspect its exact
source revision and intended signing or publishing action; a prior source
approval does not authorize every later run.

When migrating credentials, configure the environment restrictions and populate
all destination secrets first, update every consuming job, then remove the
repository-level copies. Preserve existing values and secret names. A failed
or incomplete migration must leave the working source credentials intact.
Rerunning an old workflow revision that predates these environment declarations
will no longer have signing access; use a current reviewed revision instead.

`nix flake check --no-update-lock-file` checks workflow syntax and the signing
job boundaries. The manual `Check app signing credentials` workflow checks
credential availability without signing, building, or publishing anything. Only
jobs using an environment with required reviewers, such as `zapstore-publishing`,
pause for approval. Run it on `master`; a run from a feature branch should be
rejected by the environment policies.

Server-side environment policies and secret placement must also
be verified through GitHub; a source test cannot enforce repository settings.

This boundary prevents a new, unreviewed branch workflow from reading signing
credentials. It does not make code already admitted to a signing job harmless,
prevent an authorized person from publishing a release, or replace the separate
PCR-signing approval policy. Environment approvals guard the jobs that declare
them; they do not remove native GitHub Release or package permissions from
repository/package writers. Those permission paths require separate controls.

The shared iOS release script removes App Store Connect signing inputs from
child-process environments before dependency installation, the frontend build,
and the unsigned archive. It decodes an encoded private key only immediately
before the signed archive/export, into an owned temporary directory with mode
700 and a key file with mode 600. Success, failure, and handled signals remove
that directory; the key is already removed before artifact verification. A
caller-supplied `APPLE_API_KEY_PATH` remains caller-owned and is never modified
or deleted by the script.

This limits accidental inheritance and key-file lifetime within the script. It
does not isolate the signer from code running under the same runner account:
the invoking Nix environment and signed Tauri/native build are still trusted,
and an earlier child could remain running. A caller-supplied key already on disk
also remains accessible to that account. Separate signing infrastructure would
be required to establish that stronger boundary. Hermetic release-script tests
exercise canary environments and cleanup; they do not prove real Apple signing
or TestFlight upload.
