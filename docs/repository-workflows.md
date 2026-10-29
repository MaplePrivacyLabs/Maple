# Repository checks and publication boundaries

Read this reference when changing hooks, workflows, caches, runners or
publication configuration. [CI](ci.md) explains the pull-request checks: the
single required check `ci-ok`, the tiers, labels, the lane table in
`scripts/ci/lanes.py`, lane runners, running a lane locally and adding one.
Agents load [`$change-maple-ci`](../.agents/skills/change-maple-ci/SKILL.md).
Commands and paths are relative to the monorepo root.

## Pre-commit hook

`./setup-hooks.sh` installs `.githooks/pre-commit`. It asks
`scripts/ci/plan.py hook --staged` which components to check: a lane's own
paths select the component in its `hook` field, without CI's fan-out to
consumers, and an unclassified path selects every component. Each selected
component's `.githooks/pre-commit` runs inside its Nix flake: the root `.#ci`
shell for repository checks, Research, Auth, `services/updates/` and
`scripts/ci/agent-updates.sh`; the component flakes for `apps/maple-agent/`,
`sdk/`, `proxy/` and `services/opensecret?submodules=1`. Without Nix the same
commands run from `PATH` with a warning.

The hook runs the fast part of the selected lanes: format, lint, type checks
and unit tests. The Agent runs its `agent-lint` and `agent-test` recipes;
Research Rust runs formatting and tests, without Clippy; repository changes
run actionlint on staged workflows and the `scripts/ci` tests. Integration
suites, cargo-deny, `nix flake check` and packaging stay in CI.
`MAPLE_HOOK_FULL=1` runs slower variants where they exist, `MAPLE_HOOK_ONLY=a,b`
restricts the components, `MAPLE_HOOK_NO_NIX=1` runs from `PATH` even with
Nix installed, and `MAPLE_HOOK_SKIP=1` or `git commit --no-verify` skips the
hook. `scripts/ci/test_plan.py` keeps the hook's components and its
branches in step with the lane table.

## Caches

- Pull requests restore Actions caches, and no pull-request write reaches a
  cache `master` reads. The exceptions are the EIF jobs.
  `opensecret-eif.yml`'s `eif` job (fork PRs that edit PCR approvals, and
  manual runs outside `master`) has no FlakeHub credentials, so its Nix store
  goes to the run's branch-scoped Actions cache, which `master` and other
  pull requests cannot read. Same-repository PR EIF builds (`ci.yml`'s
  `os-eif` and `opensecret-eif.yml`'s `eif-trusted`) push to the FlakeHub Nix
  cache by design, and fall back to the branch-scoped Actions cache when
  FlakeHub does not initialize
  ([cache policy](../services/opensecret/docs/nitro-deploy.md#binary-caches-and-cold-run-validation)).
- Lane caches (`Swatinem/rust-cache`, prefix `ci-v2`) are keyed by lane and
  toolchain pins (the workspace's `flake.lock`, and OpenSecret's
  `rust-toolchain.toml`), which must match exactly, then by rust-cache's
  lockfile hash, which may not: a version bump restores its exact cache, and
  any other lockfile change the newest cache of the same lane and toolchain.
  The restore key also hashes the runner's rustc versions (the Windows lanes'
  rustup pin among them) and `CARGO*`/`RUST*` variables, so a runner image
  with a new preinstalled Rust starts cold once, until master saves again.
  rust-cache's save keeps every build of a crate still in the graph, so a
  master run that will save a new cache first deletes the lane's target
  directory: each saved cache holds one dependency graph.
  `agent-supply-chain` and `agent-provider`, which only resolve the Agent's
  dependency graph, share `agent-cargo-home`: the Cargo registry and git
  sources (zed and cua among them) without a target directory.
  Master pushes and the nightly run save them, into the store of the lane's
  own runner vendor; a runner from the other vendor (a failover) only
  restores. Heavy Rust caches therefore live in Blacksmith's store. GitHub's
  holds what GitHub-hosted jobs need: the lane caches of `agent-full-windows`
  (saved only by the nightly run) and `agent-windows-updater`, the
  `agent-package-windows` packaging cache (below), the signed Research Windows
  build's Bun binary, the release, the Nix installer and that EIF check.
  `test_workflow_policy.py` lists these writers.
- The signed master builds write the keys `release.yml` restores by prefix
  (`<os>-sccache-*` for macOS and Linux, `xcode-cache-*`,
  `release-onnxruntime-ios-*`, `<os>-bun-*`, `<os>-gradle-*`). Research Windows
  builds use no compiler cache. No pull-request job writes these prefixes;
  the Research platform lanes only restore them. The iOS ONNX key text is
  identical in every workflow that uses it. A release job reads only the store
  of its own runner vendor, so `release.yml`'s `build-web` (GitHub-hosted)
  starts with a cold Bun cache; Bun installs in seconds.
- `agent-desktop-build.yml`'s unsigned builders save the exact key
  `ci-v1-agent-release-<os>-<graph hash>` on master pushes, with no fallback
  keys. `scripts/ci/cargo_graph_key.py` hashes the Agent's `flake.lock` and
  its `Cargo.lock` without the versions of workspace and path crates, which
  Cargo rebuilds anyway: a release version bump keeps the key, and a change
  to any third-party package or dependency edge, or to the toolchain, makes
  the next build cold. Pushes build only Prod; the nightly Dev and dispatched
  builds restore the same key without saving (the profile changes only Agent
  workspace crates, which Cargo rebuilds). No `ci.yml` lane reads it. The
  Agent packaging lanes build thin LTO into their own
  `ci-v1-agent-package-<os>-*` caches, which only the nightly master run
  saves. The Agent's signing, attestation and verification jobs restore
  nothing.

## Concurrency

| Workflow | Group | Cancelled |
| --- | --- | --- |
| `ci.yml` | per PR, or per merge-queue head or SHA | superseded PR runs only |
| `frontend-tests`, `rust-tests`, `web-build`, `desktop-build`, `android-build`, `proxy-rust` | none | never |
| `codeql.yml` | per SHA | never |
| `agent-desktop-build.yml` | per run | never |
| `mobile-build.yml`, `ios-dev-testflight.yml` | one queue each (`queue: max`) | never |
| `opensecret-eif.yml`, `pages-preview-build.yml`, `auth-pages-ci.yml` | per PR or SHA | PR runs only |
| `nightly.yml` | one group | never |
| Publishers | their own serial groups | never |

## Contracts

`scripts/ci/test_contracts.py` runs in the `repo` lane and imports the code on
the other side of each contract. A failure means the workflow change is wrong:

1. `release.yml` and the publishers (`pages-publish`, `proxy-publish`,
   `updates-publish`, `zapstore-publish`, `sdk-publish-npm`, `sdk-publish-rust`,
   `agent-updates-publish`, `auth-pages-publish`, `opensecret-eif-release`)
   change only with owner review. Exactly one workflow is named `Release`, and
   every `workflow_run` listener names its producer and checks the head
   repository, conclusion and event.
2. `services/agent-updates/publisher/publisher.py` trusts
   `agent-desktop-build.yml` by path, job names, artifact names and signer, and
   its run number is the Agent build number: never rename that file. The
   Windows packaging and verification jobs set `MAPLE_PACKAGE_BUILD_NUMBER` to
   that run number. `scripts/ci/windows_signature_reference.py` (the manual
   signature probe) trusts the same file's Windows Prod jobs and their runners.
3. The release preflight (`.agents/skills/release-maple/scripts/ci_evidence.py`)
   needs the evidence jobs of `frontend-tests`, `rust-tests`, `web-build`,
   `desktop-build`, `android-build` and `mobile-build` on master pushes, and a
   CodeQL that is not red: the newest finished scan of the release commit or an
   ancestor succeeded (a scan still running does not block). These run on every
   relevant master push and are never cancelled.
4. The signed master builds write the release cache keys above (the Agent's
   is `cargo_graph_key.py`'s hash of its lockfiles, with no fallback), and
   `ios-dev-testflight.yml`'s run number is the Maple Dev build number: never
   rename that file.
5. Every OpenSecret EIF builder that uses FlakeHub (`ci.yml`'s `os-eif`,
   `opensecret-eif.yml`'s `eif-trusted` and the release build) runs on the
   release builder's runner class, and only the listed jobs receive
   `id-token: write` or use FlakeHub. The unprivileged fork job stays on
   `ubuntu-24.04-arm`. An EIF approval
   mismatch fails only where that environment's approval files are edited
   (or a manual run outside `master`); on master it is a notice.
6. Each protected environment has a fixed set of consumer jobs;
   `windows-signing` runs only on GitHub-hosted runners.
7. Docs, guides and READMEs name only workflows that exist. Docs select no
   lane, so this runs with the `repo` lane and nightly; run `just lane repo`
   after editing docs.

The Pages and Auth publishers also trust their producers
(`pages-preview-build.yml`, `pages-production-build.yml`,
`auth-pages-ci.yml`, `auth-pages-build.yml`) by file, artifact name and
gates; `test_pages_workflows.py` and `test_pages_auth_workflows.py` pin them.

`scripts/ci/test_workflow_policy.py` holds the rules every workflow keeps: no
`pull_request_target`, and a fixed set of workflows that run on pull requests;
actions pinned to commits; checkouts without persisted credentials; read-only
workflow permissions, with write scopes granted to the jobs that need them;
secrets only inside their environment's jobs; a pinned, verified Nix install; a
timeout on every job; `ci-ok` needing every other `ci.yml` job; and declared
runner labels.

## Signed builds and publication

Master pushes start signed Research builds (`desktop-build`, `android-build`,
`mobile-build`, `ios-dev-testflight`) when `app-change-detection.yml` and
`scripts/ci/change_detection.py` find Research inputs for that platform; the
scripts only `ci.yml`'s unsigned lanes run select none. Only `mobile-build` and
`ios-dev-testflight` can also be dispatched, which always builds; desktop and
Android release evidence comes only from master pushes. The selector routes
proxy runtime and Rust SDK build inputs to the native builds, because the
Research shell builds both from the tree. TypeScript SDK changes never select
app builds: each frontend builds the SDK its own manifest selects. Agent Prod
desktop packages build on master pushes that change Agent inputs (`plan.py only
agent-desktop`), Agent Dev packages nightly, and both on every manual dispatch.
Signing jobs keep their protected environments; approving one trusts its
runner with that environment's secrets.
To rehearse the signed Research release scripts outside CI, run
`scripts/ci/signed-release-rehearsal.sh <desktop|ios|android|all>` in the root
Nix shell that builds the platform (`.#apple`, `.#android`, and
`.#desktop-linux` on Linux or `.#ci` on macOS); its usage text lists the
signing variables, and nothing is uploaded.

Routine development never authorizes releases, signing, store submission,
deployment, or live-service changes. Never push to `master` as a validation
step: app inputs start production-shaped signed builds and can upload iOS
artifacts to TestFlight. Creating a GitHub Release starts release builds and
downstream publication. Use `$release-maple` only for explicitly requested
release work, and report the tag and commit before publishing.

For Pages, read [the deployment guide](pages-deployments.md). Credential-bearing
publication runs from trusted master; publishing flags, protected environments
and Cloudflare build controls are operator configuration, not consequences of
merging source.

## Agent Windows updater checks

The `agent-windows-updater` lane (see `scripts/ci/lanes.py` for what selects
it) runs the updater's native filesystem, Authenticode-rejection, transaction
and recovery tests and the bootstrap integration test on GitHub's Windows
image. Like every `windows` matrix lane it has no signing credential or
protected environment. A pass qualifies no official signature, signed
installed upgrade, visible GUI or paired remote upgrade.

The manual `agent-windows-signature-probe.yml` takes an exact historical
producer run, attempt and source and the artifact's ZIP and PE digests. It
checks that the trusted-master Windows Prod build, signing and download
verification jobs passed and that the ZIP and package identity match, then runs
the production Authenticode verifier on the PE without launching it. It has no
default reference and nothing depends on it; a pass proves historical signature
interoperability only.

## OpenSecret checks

`ci.yml` runs `os-rust`, `os-flake-check` (the component flake's checks),
`os-helpers` (the parent helper images), `deps-opensecret` (when its
manifest, lockfile or `deny.toml` changes), `os-pcr` (the root
`pcr-compatibility` check for PCR tooling and approval edits) and
`os-sdk-e2e`, which tests both SDKs against `services/opensecret/` from the
same checkout. The `os-eif` lane builds dev and
prod EIF candidates on same-repository PRs to `master` that change measured
inputs (or carry the `ci:eif` label) without editing PCR approvals, on the
release builder's
`blacksmith-16vcpu-ubuntu-2404-arm` class, and reports their PCRs.
`opensecret-eif.yml` compares measurements on PRs that edit approved PCR JSON,
on relevant master changes and on manual runs; that approval status is not
part of `ci-ok`. A mismatch fails a manual run outside `master`, and a PR only
in an environment whose approval files it edits. On master it is pending
approval: the job passes with a notice and the built PCRs in its summary. Build
and comparison errors always fail. These checks never change approvals, sign,
release, or authorize deployment. Master and same-repository PR EIF builds receive OIDC for
FlakeHub caching; fork PRs and non-master manual runs never do. Verify cache
changes on fresh hosted runners, not a warm local Nix store. Details:
[CI approval checks](../services/opensecret/docs/nitro-deploy.md#ci-approval-checks)
and the [cache policy](../services/opensecret/docs/nitro-deploy.md#binary-caches-and-cold-run-validation).

## Nix runtime

Jobs install Determinate Nix 3.23.3 through an immutable
`DeterminateSystems/determinate-nix-action` commit with `source-tag: v3.23.3`
(`.github/actions/setup-nix`, or inline in the sacred workflows), then run
`scripts/ci/verify-nix-runtime.sh`. Keep the revision and tag in step when
upgrading. No GitHub token reaches the installer; FlakeHub authentication is
limited to jobs with `id-token: write`.

The pinned `actionlint` carries
`scripts/ci/patches/actionlint-stream-stdin.patch`, so analyzer input streams
after the child starts; remove it when upstream includes this. The macOS shell
caps Bash heredoc pipes at 512 bytes, and larger bodies use a temporary file.

## Runners and toolchains

[CI runners](ci.md#runners) assigns the `ci.yml` lanes. The signed and
release workflows use:

| Work | Runner |
| --- | --- |
| Research macOS, Linux, Android and iOS builds, which compile and sign (master and release); `rust-tests.yml`; the Agent's unsigned master builds | Blacksmith (`blacksmith-6vcpu-macos-26`, `blacksmith-8vcpu-ubuntu-2404`, `blacksmith-8vcpu-windows-2025`) |
| Release `build-windows` | `maple-windows-2025-16core` |
| Signed Research Windows master build; Agent Windows signing and verification | `windows-2025` |
| Artifact verifiers, TestFlight submission, the Agent's signing and attestation, CodeQL, web and frontend evidence | Other GitHub-hosted runners |
| Proxy release builds and their PR rehearsal (`proxy-rust.yml`) | GitHub-hosted Linux and Windows, Blacksmith macOS |
| EIF builders that use FlakeHub | `blacksmith-16vcpu-ubuntu-2404-arm` |

The Agent's signing and attestation, `windows-signing` jobs and every artifact
verifier stay GitHub-hosted: the Agent publisher rejects self-hosted signers,
Blacksmith counts as self-hosted, and verifiers run fresh. The Agent's
unsigned master Windows build compiles on Blacksmith because its fat-LTO link
runs on one thread, so 16 cores are no faster. Blacksmith's Windows workspace
inherits a foreign write ACE that the updater's storage validation rejects,
so the updater tests and install smokes stay on GitHub's image. Jobs that
compile and sign install the pinned Artifact Signing module before compiling,
so a failed module download fails early; the module's own NuGet packages and
the Azure login still come after the compile. The installer retries the
PowerShell Gallery and its CDN and accepts only the pinned SHA-256. Rust,
MSVC, Perl and the linker are pinned by `.github/actions/setup-agent-windows`
for the Agent.

Research desktop and iOS builds select Xcode 26.5 (`17F42`) from
[`scripts/ci/apple-toolchain.json`](../scripts/ci/apple-toolchain.json) through
`scripts/ci/select-xcode.py`; the root Nix shell exports the same version for
its Apple recipes. Do not rely on a runner's default Xcode. The
[iOS ONNX pins](../apps/maple-research/frontend/src-tauri/scripts/onnxruntime-pins.sh)
verify artifacts against that exact build, so an Xcode upgrade means rebuilding
and reviewing those hashes and validating desktop and iOS packaging. A newer
CI image does not raise the shipped deployment targets: Research supports
macOS 13.4 and iOS 16.0.

Research Linux packaging builds AppImage and DEB with Tauri, then the RPM from
the normalized DEB payload. `scripts/ci/sccache-stats.sh` reports compiler-cache
statistics with the build's own cache configuration.
The RPM builder preserves explicit `bundle.linux.rpm.depends` from the Tauri
configuration. WebKitGTK (`webkit2gtk4.1`) is required on Fedora, including KDE;
package verification rejects RPMs missing that requirement. Install a downloaded
RPM through Discover or `sudo dnf install ./Maple-<version>-1.x86_64.rpm` so the
package manager resolves dependencies, rather than using `rpm --nodeps`.

For a side-by-side hosted Dev installation, use
`nix run --no-update-lock-file .#research-dev-desktop`, not an ordinary Research
PR package. The Linux Dev command stages its own DEB payload and normalizes it
into the separately named `maple-research-dev` RPM under `target/research-dev`.
It uses the fixed Dev native identity and callback scheme, disables updates,
and verifies the final package contains only Dev-owned executable, launcher,
icon, and runtime paths. See the
[Research Dev package guide](../apps/maple-research/docs/research-dev-desktop.md).

## SDK consumption

Follow the [consumer version policy](sdk-publishing.md#consumer-version-policy).
Consumers select published SDK pins independently; local links are for
development. SDK publication and upgrading a consumer are separate decisions.

## Maintaining guidance

Re-check prescriptive language against source, tests, and executed workflows.
Correct material workflow, ownership, or validation changes alongside the
implementation. Keep unrelated drift separately scoped and avoid duplicated
policy. Public guides describe implementation, supported setup, tests, and CI;
credential administration and private operational procedures belong with their
owners outside this repository.
