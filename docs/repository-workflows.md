# Repository checks and publication boundaries

Read this reference when changing hooks, workflow selection, shared CI, caching,
or publication configuration. Commands and paths are relative to the monorepo
root. For everyday setup, use the owning component guide and skill.

## Pre-commit hook

`./setup-hooks.sh` installs `.githooks/pre-commit`. It classifies staged paths
with `scripts/ci/hook_change_detection.py` and runs each affected component's
own `.githooks/pre-commit` inside that component's Nix flake, so the tools match
CI: the root `.#ci` shell for Research, Auth, `services/updates/`, and repository
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
The separate `opensecret-eif.yml` builds dev/prod EIF candidates on
same-repository PRs changing measured inputs without editing PCR approvals;
these diagnostic builds do not publish an artifact. It compares measurements
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

## macOS runners and Apple toolchain

macOS jobs use explicit ARM64 `macos-26` or `macos-26-xlarge` runners. Research
desktop and iOS builds select stable Xcode 26.5, build `17F42`, from
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

## Windows runner pilot

The Research unsigned Windows PR package uses
`blacksmith-8vcpu-windows-2025` (Windows Server 2025, public beta). The job
records its actual capacity and Visual Studio Build Tools installation before
running the existing pinned Bun/Rust, ONNX Runtime, VC runtime DLL staging,
and NSIS build. Its cache remains restore-only, and downloaded package proofs
are checked independently on a standard GitHub runner. Signed Windows master
and release builds remain on GitHub's `windows-2025`; this pilot does not
validate Azure signing, updater signing, or installed application behavior.

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
