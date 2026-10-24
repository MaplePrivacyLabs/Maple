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
path dependencies; the proxy workflows' path filters list the same inputs.
TypeScript SDK changes never select app builds: each frontend builds the SDK
source its own manifest selects, so changing that manifest (a pin bump or a
local link) is what selects its lanes. Tests, docs, container-only inputs, and
standalone component lockfiles retain their independent lanes. Update the classifier and
its table-driven tests when the dependency graph or component layout changes.
The backend has its own root `opensecret-ci.yml` workflow and change selector;
`sdk-integration.yml` tests both SDKs against `services/opensecret/` from the
same checkout. Backend changes do not imply Research or Agent packaging.
Agent desktop packages compare master against the last successful ancestor run
with all four Dev/Prod macOS/Linux download verifiers completed; selector-only
and failed runs do not consume outstanding changes. Unrelated master pushes skip
Agent packages. Manual master dispatch always builds both profiles. PR previews
build and package only Dev on macOS and Linux, including the matching Linux
downloaded-package verifier; Prod packages remain covered by master builds.
PRs keep their merge-base selection and signing stays confined to trusted master.
Research desktop, Android and iOS PR workflows group runs by workflow and PR.
A new run cancels earlier runs for that same unsigned preview; unrelated PRs
and signed, master and manual queues keep their existing behavior.
The separate `opensecret-eif.yml` builds dev/prod EIF candidates on
same-repository PRs changing measured inputs without editing PCR approvals.
Each diagnostic build explicitly realizes the user ramdisk selected by its EIF
derivation, even when Nix substitutes a cached final EIF. The entrypoint's CA
bundle must be present in that archive and omit the affected emSign roots. If
the source CA store path is also present, its bytes must match the archive.
The check records whether retained Comodo AAA is excluded from ordinary TLS
PEMs and explicitly rejected by OpenSSL for server trust. It records the
archived bundle and EIF hashes with the PCRs in the job summary; it does not upload
an artifact. The workflow compares measurements
on PRs editing approved PCR JSON, relevant master changes, and manual runs.
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

## Nix runtime

GitHub Actions jobs use Determinate Nix 3.23.3 through an immutable
`DeterminateSystems/determinate-nix-action` commit and an explicit
`source-tag: v3.23.3`. Keep the action revision and source tag synchronized when
upgrading. `scripts/ci/test_nix_workflows.py` rejects the floating installer,
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
no signing credentials and retain their existing branch-scoped caches. No
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

## Agent compilation and cache measurements

Agent CI runs the three-platform workspace matrix, three-platform upstream
SQLCipher provider suite, and plain Linux optimized binary as independent jobs.
The final `Agent CI required jobs` check rejects failed or unexpectedly skipped
selected jobs. All retain fail-closed change selection. The plain Linux binary
and Desktop Dev/Prod packages have separate profiles and caches.

Before restoring targets, `scripts/ci/agent_cache.py` fingerprints Rust inside
the component Nix shell, or the explicitly selected Windows MSVC environment,
plus native ABI/toolchain, flags, features, profile and provider configuration.
The component shell also pins Rustup proxies to its Nix toolchain so Cargo's
external Clippy/rustfmt commands cannot silently select a host compiler.
Windows setup also pins Cargo's linker to the selected MSVC `link.exe` and
compiles/runs an offline fixture with a Cargo build script before restoring
targets. This catches Git Bash's unrelated `link.exe` shadowing Microsoft tools.
The workspace and Desktop caches use this compatibility prefix with
`rust-cache`'s automatic dependency suffix. PRs only read; successful master
pushes write. Application version changes can reuse compatible dependencies;
Cargo still checks freshness. Cache probes use `agent_cache_command.py` to
identify only the actual selected compiler; incidental installed Rustup
toolchains do not describe these builds and are excluded from the inventory.
After a complete target-cache miss, a read-only lookup can restore an older
archive within the same actual compiler/native ABI, job, OS and architecture
prefix. A restored Cargo target marker prevents a second archive download
after an ordinary dependency-prefix hit. The primary action retains its exact
dependency hash and remains the only writer. The provider cache additionally requires the
verified, resolved upstream test-only lockfile and transformed manifest, with
no fallback across provider dependency graphs. Its temporary source and target
remain outside shipping workspace outputs.

Unsigned builders upload seven-day `maple-agent-*-metrics-*` artifacts with
actual environment identity, phase durations, logical target sizes, and bounded
current Cargo timing HTML. Action logs report exact/restore keys and transferred
cache bytes; logical target size is not compressed cache size. Windows build
and test are separate phases. See [the Agent performance audit](agent-ci-performance.md)
for the baseline, tradeoffs and cold/warm validation procedure.

## macOS runners and Apple toolchain

Research desktop/iOS builds, Agent release-binary jobs
and native proxy macOS archives use ARM64 `blacksmith-6vcpu-macos-27` runners.
Artifact verifiers, TestFlight uploads, and Agent preview packaging/protected
signing use GitHub's `xcode-27` image runners. Research signed builders retain
their existing `desktop-signing` or `apple-signing` environment, so their
provider receives that environment's secret context. Existing master pushes
can build and upload independent Dev/Prod TestFlight artifacts after verification.
The Agent's final signed-package provenance identifies its GitHub
packaging/signing job; Blacksmith is trusted for prebuilt binary integrity.
Research desktop and iOS builds and the Agent's macOS jobs select stable Xcode 27.1,
build `27A9275`, from
[`scripts/ci/apple-toolchain.json`](../scripts/ci/apple-toolchain.json) through
the shared selector, which aligns the runner's active `xcode-select` directory
and `DEVELOPER_DIR`; the root Nix shell exports the same version and build
for its Apple recipes. Tauri mobile tooling discovers the active Xcode directly.
Do not rely on the runner's default Xcode or beta app
names; hosted images can keep a release under a suffixed name such as
`Xcode_27.1_Release_Candidate.app`, which the selector accepts only when its
build matches the pin. Xcode 27 builds iOS code for deployment targets 15.0 and
later, so the iOS ONNX Runtime build targets 16.0 like the app. The independent Agent shell selects a full installed Xcode, and the
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

The Research unsigned Windows PR package uses
`blacksmith-8vcpu-windows-2025` (Windows Server 2025, public beta). The job
records its actual capacity and Visual Studio Build Tools installation before
running the existing pinned Bun/Rust, ONNX Runtime, VC runtime DLL staging,
and NSIS build. Its cache remains restore-only, and downloaded package proofs
are checked independently on a standard GitHub runner. Signed Windows master
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
