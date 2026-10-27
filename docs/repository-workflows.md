# Repository checks and publication boundaries

Read this reference when changing hooks, workflow selection, shared CI, caching,
or publication configuration. Commands and paths are relative to the monorepo
root. For everyday setup, use the owning component guide and skill.

## Pre-commit hook

`./setup-hooks.sh` installs `.githooks/pre-commit`. It classifies staged paths
with `scripts/ci/hook_change_detection.py` and runs each affected component's
own `.githooks/pre-commit` inside that component's Nix flake, so the tools match
CI: the root `.#ci` shell for Research, Auth, `services/updates/`, the independent
Agent delivery checks in `scripts/ci/agent-updates.sh`, and repository
checks; the component flakes for `apps/maple-agent/`, `sdk/`, `proxy/`, and
`services/opensecret?submodules=1`. Without Nix it runs the same commands from
`PATH` and warns that results may differ. It runs each selected component's format, lint, type-check, and unit-test gates;
Research Rust runs formatting and tests, without Clippy. Other Rust components
include their documented Clippy checks; shared
crates do not fan out to their consumers. It never runs integration suites,
cargo-deny, `nix flake check`, or packaging. `MAPLE_HOOK_FULL=1` runs the
slower CI-complete variants (for example the Agent's full `just ci`);
`MAPLE_HOOK_SKIP=1` or `git commit --no-verify` bypasses it. The hook is a fast
local gate before the GitHub Actions cycle, not full CI parity. Keep the
classifier, its table-driven tests, and the component scripts in step with the
workflows when a lane changes.

`scripts/ci/change_detection.py` routes expensive app packaging. It selects
desktop builds for proxy runtime inputs and native builds for Rust SDK build
inputs (`sdk/rust/Cargo.toml`, `src/`, `build.rs`, `assets/`), because the
proxy, the Agent and the Research native shell build both from the tree as
path dependencies; the proxy and Research lanes in `scripts/ci/lanes.py` list the same inputs.
TypeScript SDK changes never select app builds: each frontend builds the SDK
source its own manifest selects, so changing that manifest (a pin bump or a
local link) is what selects its lanes. Tests, docs, container-only inputs, and
standalone component lockfiles retain their independent lanes. Update the classifier and
its table-driven tests when the dependency graph or component layout changes.
OpenSecret's `ci.yml` lanes are selected by its own classifier,
`scripts/ci/opensecret_change_detection.py`, which `plan.py` imports unchanged:
`os-rust`, `os-flake-check`, `os-helpers` (the parent helper images),
`deps-opensecret`, `os-pcr` (the root `pcr-compatibility` check, for PCR tooling
and approval edits) and `os-sdk-e2e`, which tests both SDKs against
`services/opensecret/` from the same checkout (`scripts/ci/opensecret-sdk-e2e.sh`).
`nightly.yml` builds the x86_64 package. Backend changes do not imply Research
or Agent packaging.
Agent desktop packages build on master pushes whose own diff changes Agent
inputs (`plan.py only agent-desktop`), and on every manual master dispatch.
Packaging-input PRs rehearse unsigned Dev packages on Linux and macOS in
`ci.yml` (Windows with the `ci:package` label); signing stays confined to
trusted master.
The `os-eif` lane builds dev/prod EIF candidates on same-repository PRs
changing measured inputs without editing PCR approvals, on the release builder's
`blacksmith-8vcpu-ubuntu-2404-arm` class.
Each diagnostic build explicitly realizes the user ramdisk selected by its EIF
derivation, even when Nix substitutes a cached final EIF. The entrypoint's CA
bundle must be present in that archive and omit the affected emSign roots. If
the source CA store path is also present, its bytes must match the archive.
The check records whether retained Comodo AAA is excluded from ordinary TLS
PEMs and explicitly rejected by OpenSSL for server trust. It records the
archived bundle and EIF hashes with the PCRs in the job summary; it does not upload
an artifact. `opensecret-eif.yml` compares measurements
on PRs editing approved PCR JSON, relevant master changes, and manual runs; that
approval status is not part of `ci-ok`.
Preserve ordinary backend PRs without fresh approvals and meaningful master
mismatches; these checks never change approvals, sign, release, or authorize
deployment. Master and same-repository PR EIF builds receive OIDC for FlakeHub caching.
Fork PRs and non-master manual runs use GitHub's branch-scoped cache without
OIDC. Preserve that head-repository boundary and verify cache changes on fresh
hosted runners, not just a warm local Nix store. See the [cache policy](../services/opensecret/docs/nitro-deploy.md#binary-caches-and-cold-run-validation).

For Pages, read [the deployment guide](pages-deployments.md). Preserve
unprivileged preview builds and separate development/production profiles.
Credential-bearing publication executes trusted master. Publishing flags,
protected environments, and native Cloudflare build controls are operator
configuration, not consequences of merging source.

Routine development never authorizes releases, signing, store submission,
deployment, or live-service changes. Never push to `master` as a validation
step: app inputs start production-shaped signed builds and can upload iOS
artifacts to TestFlight. Creating a GitHub Release starts release builds and
downstream publication. Use `$release-maple` only for explicitly requested
release work, and report the tag and commit before publishing.

## Workflow checks

The `repo` lane of `ci.yml` runs `nix flake check` when workflows, `scripts/ci/`,
hooks, the root flake, the release-preflight scripts, the Agent update publisher,
or the Research version files change. Besides actionlint and the release gates,
it runs `scripts/ci/test_workflow_policy.py` (invariants every workflow keeps)
and `scripts/ci/test_contracts.py` (what the release, the publishers, signing and
OpenSecret rely on). A contract failure means fix the workflow, not the contract.

## Nix runtime

GitHub Actions jobs use Determinate Nix 3.23.3 through an immutable
`DeterminateSystems/determinate-nix-action` commit and an explicit
`source-tag: v3.23.3`. Keep the action revision and source tag synchronized when
upgrading. `scripts/ci/test_workflow_policy.py` rejects the floating installer,
an unexpected action revision or tag, installation before checkout, or a missing
runtime verification step. Each job checks the installed Determinate and
upstream Nix versions before doing build work. The workflows pass no GitHub
token to the installer; FlakeHub authentication remains limited to jobs that
separately receive `id-token: write`.

The pinned `actionlint` has a small source patch in
`scripts/ci/patches/actionlint-stream-stdin.patch`: analyzer input streams after
the child starts, so workflow lint cannot fill a pipe before its reader runs.
The Nix package runs a large-input regression for stdout and combined output.
Remove the patch when the pinned upstream includes this behavior.
The macOS validation shell also caps Bash heredoc pipes at the POSIX 512-byte
minimum; larger bodies use its normal temporary-file path. Package regressions
exercise both heredocs and here-strings against the built Bash. Apple’s shell
bootstraps this macOS tool build so the cached Bash cannot block configure.

## Linux runners

Research desktop/Android compilation and Agent Linux CI/AppImage builds use
`blacksmith-8vcpu-ubuntu-2404`. Lightweight selectors, web builds, artifact
verification and the separate Agent attestation job remain on GitHub's standard
runners. Signing jobs retain their protected environments. The desktop release
build command only receives Apple environment variables on macOS, but that
condition does not restrict the runner's job secret context: approving a signing
job trusts its runner with the selected environment's secrets. PR builds have
no signing credentials and only restore caches that master writes. No
persistent vendor disks or Docker builders are configured by these workflows.

Research Linux packaging builds AppImage and DEB through Tauri, then creates
the final RPM once from the normalized DEB payload. Its staging executable is
marked as RPM so Tauri selects the RPM updater entry and installer. Build and
download verifiers check the executable identity in all three package formats;
updater metadata, signing and publication keep their existing flow.

Research's Unix compiler-cache statistics use `scripts/ci/sccache-stats.sh`,
which shares the build's cache configuration. In Actions the sccache socket
lives under `RUNNER_TEMP`, so separate Nix shells reach the same server despite
their different `TMPDIR` values. Cache archive restoration and compiler hits
remain separate measurements.

## Agent lanes and caches

Agent checks are `ci.yml` lanes in `scripts/ci/lanes.py`, reported by `ci-ok`.
PRs run `agent-lint`, `agent-test` and `agent-macos`; `agent-windows` on Windows
paths or the `ci:windows` label; `agent-supply-chain` and `agent-provider` when
dependency inputs change; and `agent-package-linux`/`agent-package-macos` when
packaging inputs change (`ci:package` adds Windows). Master pushes also run
`agent-windows` and `agent-full-{linux,macos,windows}`, never cancelled;
`ci:full` runs every lane on a PR and `ci:perf` the store benchmark.
`nightly.yml` runs the advisories, SQLCipher sources, the provider suite on all
three OSes and the store benchmark, and keeps one `ci-nightly` issue.

Lane caches use `Swatinem/rust-cache` with exact keys (`ci-v1-<lane>-` plus a
hash of the Agent `flake.lock` and `Cargo.lock`), written only by master pushes
and the nightly `ci.yml` run, on Blacksmith only. Windows has no Nix:
`.github/actions/setup-agent-windows` pins Rust (`agent-lint` checks it equals
the flake's), MSVC, native Perl and the linker. The unsigned
`agent-desktop-build.yml` builders restore and, on master pushes, save the
exact key `ci-v1-agent-release-<os>-<hash of both lockfiles>` with no fallback;
the PR packaging lanes only restore it, and signing, attestation and verifier
jobs restore nothing.

`agent-windows-updater-tests.yml` separately runs native updater, filesystem,
Authenticode rejection and application bootstrap component tests on
`windows-2025` for the affected Agent paths. It uses the existing pinned Windows
Rust and linker setup without signing credentials, release environments or
dependency-artifact restoration. Required native cases must execute, including
reparse fixtures; missing fixture privileges fail the job. Its source/run/attempt
receipt and Cargo logs are retained even on failure. The lightweight bootstrap
target does not replace the real Windows application build, official Maple
signature verification, signed installed A-to-B upgrade, visible GUI or paired
remote upgrade qualification.

The same workflow independently typechecks `maple-agent-app --all-targets` on
Windows with `cargo check --locked`, including Windows-only desktop fixtures.
It runs in parallel with native updater tests and retains its own source/run
receipt and complete compiler log. This catches app/test type errors without
release code generation or waiting for release LTO. A successful typecheck does
not claim test execution, packaging or installed-upgrade qualification.

The separate manual-only `agent-windows-signature-probe.yml` accepts an exact
historical producer run/attempt/source and artifact ZIP/PE digests. It validates
successful trusted-master Windows Prod build, signing and downloaded-consumer
jobs, bounded ZIP bytes and package identity before reading the selected PE with
the real admission/startup Authenticode verifier. It never launches that PE.
There is no default historical reference or dependency from ordinary PR tests;
expired references must be selected again explicitly. Its result proves only
historical signature interoperability, not the new updater's installed upgrade
or final-source release qualification.

## Client and service lanes

SDK, proxy, Research and service checks are `ci.yml` lanes too. `sdk-ts` and
`sdk-rust` run the publishers' own builder (`scripts/ci/build-sdk-publish.sh`);
`proxy` runs `just check`, rustdoc and `cargo machete`; `deps-research`,
`deps-sdk` and `deps-proxy` run cargo-deny advisories and bans when that
dependency graph changes; `proxy-container` builds the image when its recipe
changes, and master builds both architectures. `research-web`, `research-rust`
and `auth` run on PRs only: `frontend-tests.yml`, `web-build.yml`,
`rust-tests.yml` and `auth-pages-ci.yml` cover master, and `rust-tests.yml`
writes the `ci-v1-research-rust-` cache that `research-rust` restores. `updates`
and `agent-updates` run on every event. The unsigned Research platform builds
(`research-desktop-{macos,linux,windows}`, `research-ios`, `research-android`,
`research-dev-macos`) run on PRs that touch their native inputs or carry
`ci:package`, `ci:ios`, `ci:android`, `ci:windows` or `ci:research-dev-macos`.
They restore the caches the signed master builds save and never save one.
`proxy-rust.yml` rehearses the release's native proxy builds.

## macOS runners and Apple toolchain

Research desktop/iOS builds, Agent release-binary jobs
and native proxy macOS archives use ARM64 `blacksmith-6vcpu-macos-26` runners.
Artifact verifiers, TestFlight uploads and Agent protected signing use standard
GitHub `macos-26` runners; Agent PR packaging rehearsals use Blacksmith.
Research signed builders retain their existing `desktop-signing` or
`apple-signing` environment, so their provider receives that environment's
secret context. Existing master pushes
can build and upload independent Dev/Prod TestFlight artifacts after verification.
The Agent's final signed-package provenance identifies its GitHub
packaging/signing job; Blacksmith is trusted for prebuilt binary integrity.
Research desktop and iOS builds select stable Xcode 26.5, build `17F42`, from
[`scripts/ci/apple-toolchain.json`](../scripts/ci/apple-toolchain.json) through
the shared selector, which aligns the runner's active `xcode-select` directory
and `DEVELOPER_DIR`; the root Nix shell exports the same version and build
for its Apple recipes. Tauri mobile tooling discovers the active Xcode directly.
Do not rely on the runner's default Xcode or beta app
names. The independent Agent shell selects a full installed Xcode, and the
SDK/proxy flakes own their Nix compiler and SDK inputs.

The [iOS ONNX pins](../apps/maple-research/frontend/src-tauri/scripts/onnxruntime-pins.sh)
verify artifacts against the exact Xcode build. An Xcode upgrade requires
rebuilding and reviewing those hashes, keeping cache keys tied to the selected
version/build and artifact inputs, and validating desktop and iOS packaging.
Retiring a CI host image does not raise the shipped client's deployment
targets: Research still supports macOS 13.4 and iOS 16.0.

iOS builds share the unsigned ONNX dependency cache written by trusted master
and release jobs. PRs only restore it; a miss builds locally and never saves a
PR cache. Every restored artifact is checked against the pinned Xcode and ONNX
hashes before use. Dev and Prod app build caches, signing and publication remain
independent; there is no separate ONNX cache warmer.

## Windows runner pilot

Agent Windows x64 release binaries build on `blacksmith-8vcpu-windows-2025` with
pinned Rust/MSVC selection, static CRT linkage and the selected SDK's shader
compiler. Credential-free builders upload the executable and third-party notices. A fresh
master job inspects embedded PE profile/source metadata without executing the
artifact, then reuses `windows-signing` and the existing Azure Artifact Signing
helpers for the app, NSIS uninstaller and installer. Separate native verifiers
check downloaded provenance, payload identity, signatures and isolated
install/uninstall; signing and verification stay on GitHub's `windows-2025`.
The `ci:package` PR label builds an unsigned Dev installer without credentials.
These packages are Actions downloads; Windows automatic updates and built-in
computer-use remain unsupported.

The Research unsigned Windows PR package (`research-desktop-windows`, on
Windows paths or the `ci:windows`/`ci:package` labels) uses
`blacksmith-8vcpu-windows-2025` (Windows Server 2025, public beta) with the
pinned Bun/Rust, ONNX Runtime, VC runtime DLL staging and NSIS build. It has no
cache: the signed master build writes its cache to GitHub's store, which
Blacksmith runners cannot read. Downloaded package proofs are checked
independently on a standard GitHub runner. Signed Windows master
and release builds remain on GitHub's `windows-2025`; this pilot does not
validate Azure signing, updater signing, or installed application behavior.
Windows master/release artifact verifiers also use standard `ubuntu-latest`
runners rather than larger runners.

## SDK consumption

Follow the [consumer version policy](sdk-publishing.md#consumer-version-policy).
Consumers independently select published SDK pins; local links are supported
for development. Inspect each consumer's manifest/lockfile and actual SDK
source. SDK publication and upgrading a consumer are separate decisions.

## Maintaining guidance

Re-check prescriptive language against source, tests, and executed workflows.
Correct material workflow, ownership, or validation changes alongside the
implementation. Keep unrelated drift separately scoped and avoid generic or
duplicated policy. Public guides describe implementation, supported setup,
tests, and CI; company credential administration and private operational
procedures belong with their owners outside this repository.
