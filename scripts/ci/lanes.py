"""The lane table: which ci.yml checks a change selects, and how each one runs.

Read by scripts/ci/plan.py (the ci.yml plan job, `just lane`, and lint). Globs
are repository-relative: `**` spans directories, `*` stays within one path
segment, and a leading `!` excludes paths from the globs it is listed with.

OpenSecret lanes are selected by opensecret_change_detection.py, imported
verbatim, from every changed path, inert ones included (`opensecret` names its
output). Their only globs are the entry scripts of os-sdk-e2e and os-helpers.
deps-opensecret is the exception: like every deps lane, only its manifest,
lockfile and deny.toml select it.

`hook` names the .githooks/pre-commit component that runs a lane's checks
locally. The hook selects it from the lane's own paths only, without the CI
fan-out (`also`, CI_INFRA); an OpenSecret lane's own paths are its selector's
routes under services/opensecret/.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import FrozenSet, Optional, Tuple


@dataclass(frozen=True)
class Runner:
    label: str
    variable: Optional[str] = None  # repository variable that overrides the label


def cache_store(label: str) -> str:
    """Whose Actions cache a runner uses: Blacksmith runners have their own store, every other runner GitHub's."""
    return "blacksmith" if label.startswith("blacksmith-") else "github"


# The override variables are unset; set one to fail over during an outage.
LINUX = Runner("blacksmith-8vcpu-ubuntu-2404", "CI_LINUX_RUNNER")
LINUX_HEAVY = Runner("blacksmith-32vcpu-ubuntu-2404", "CI_LINUX_HEAVY_RUNNER")
MACOS = Runner("blacksmith-6vcpu-macos-27", "CI_MACOS_RUNNER")
WINDOWS = Runner("blacksmith-8vcpu-windows-2025", "CI_WINDOWS_RUNNER")
GITHUB = Runner("ubuntu-latest")  # free GitHub runner for lanes that compile nothing heavy
# GitHub's Xcode 27 image ships Xcode's Metal Toolchain, which Blacksmith's macOS image
# lacks and cannot download (its MobileAsset catalog fetch fails).
GITHUB_XCODE = Runner("xcode-27")
# The Agent updater's native Windows tests and the installer smoke test need GitHub's
# runner: Blacksmith's workspace inherits a foreign write ACE the updater rejects, and
# master's Windows install smoke (agent-desktop-build.yml verify-windows) runs there too.
# PR lanes use the org's 16-core GitHub-hosted Windows 2025 runner (the same image family,
# GitHub's cache store); CI_WINDOWS_GITHUB_RUNNER=windows-2025 falls back to the standard one.
GITHUB_WINDOWS = Runner("maple-windows-2025-16core", "CI_WINDOWS_GITHUB_RUNNER")
GITHUB_WINDOWS_FREE = Runner("windows-2025")  # the free standard runner, for the native Windows test suites
# .github/actions/setup-agent-windows pins this toolchain (Windows has no Nix);
# agent-lint checks that the Agent flake's rustc still reports it.
AGENT_WINDOWS_RUST = "1.98.0"

PR, QUEUE, PUSH, NIGHTLY, DISPATCH = (
    "pull_request", "merge_group", "push", "schedule", "workflow_dispatch")
GATE: FrozenSet[str] = frozenset({PR, QUEUE, PUSH, NIGHTLY, DISPATCH})
PR_EVENTS: FrozenSet[str] = frozenset({PR, QUEUE})  # events that use `pr_paths`
NOT_NIGHTLY: FrozenSet[str] = GATE - {NIGHTLY}  # nightly.yml runs these every night instead
PR_SIDE: FrozenSet[str] = frozenset({PR, QUEUE, DISPATCH})  # master and nightly run these elsewhere
PR_AND_NIGHTLY: FrozenSet[str] = PR_SIDE | {NIGHTLY}  # not on master pushes; the nightly run saves the cache
# ci.yml's generic matrix jobs; any other `job` is the id of a special ci.yml job.
MATRIX_JOBS = ("nix", "windows")

# Never selects anything: notes, docs and hooks at the root or a component root
# (one or two levels down). Deeper files can be build inputs: the Agent compiles
# its bundled SKILL.md files.
INERT = (
    "README.md", "AGENTS.md", "LICENSE", ".gitignore", ".repo_ignore", "repo.meta.json", "docs/**",
    "*/README.md", "*/LICENSE", "*/.gitignore", "*/docs/**", "*/.githooks/**",
    "*/*/README.md", "*/*/AGENTS.md", "*/*/CLAUDE.md", "*/*/LICENSE", "*/*/.gitignore",
    "*/*/docs/**", "*/*/.githooks/**",
    ".agents/**", "!.agents/skills/release-maple/scripts/**",
    "!.agents/skills/validate-opensecret/scripts/disposable_db_tests.sh",
    # Local tools that no check consumes.
    "proxy/docker-compose.yml", "scripts/testing/**",
)
# Editing the planner or ci.yml selects every active gate lane.
CI_INFRA = (".github/workflows/ci.yml", ".github/actions/**", "scripts/ci/plan.py", "scripts/ci/lanes.py")

# The Agent and the Research shell build the Rust SDK and proxy from the tree.
RUST_SDK = ("sdk/rust/Cargo.toml", "sdk/rust/build.rs", "sdk/rust/src/**", "sdk/rust/assets/**")
PROXY_RUNTIME = ("proxy/Cargo.toml", "proxy/build.rs", "proxy/src/**")
# Build scripts Cargo would pick up if they were added; lint allows them to be absent.
NOT_YET_TRACKED = frozenset({"sdk/rust/build.rs", "proxy/build.rs"})
SDK_TS = ("sdk/src/**", "sdk/package.json", "sdk/bun.lock", "sdk/bunfig.toml",
          "sdk/tsconfig*.json", "sdk/vite.config.ts", "sdk/.npmrc", "sdk/flake.*")

AGENT = ("apps/maple-agent/**",)
WINDOWS_TOOLCHAIN = (".github/actions/setup-agent-windows/**", "scripts/ci/agent_windows.py")
# What the native Windows updater tests compile: the updater crate (its library and
# the bootstrap integration test, which includes the app's Windows bootstrap), its locked
# dependencies, the workspace's dependency features and profiles, and the toolchain.
AGENT_WINDOWS_UPDATER = ("apps/maple-agent/crates/maple-agent-updater/**",
                         "apps/maple-agent/app/src/windows_bootstrap.rs",
                         "apps/maple-agent/Cargo.lock", "apps/maple-agent/Cargo.toml") + WINDOWS_TOOLCHAIN
# The packaging scripts and what they call, and the toolchain that builds the bundles.
AGENT_PACKAGING = tuple("apps/maple-agent/" + glob for glob in (
    "scripts/build-*", "scripts/package-*", "scripts/verify-*", "scripts/release-info.py",
    "scripts/macos-build-info.py", "scripts/macos-release-*", "scripts/linux-*", "scripts/smoke-*",
    "release-profiles.json", "release_profile.rs", "app/build.rs",
    "app/packaging/**", "flake.nix", "flake.lock",
)) + (".github/workflows/agent-desktop-build.yml",)
# The Windows packaging lane's Windows-specific inputs: the Windows packaging scripts,
# the build script's Windows branch, the release identity the installer and the
# Windows feed read, and the Windows toolchain.
AGENT_WINDOWS_PACKAGING = tuple("apps/maple-agent/" + glob for glob in (
    "scripts/windows-*", "scripts/build-release.sh", "scripts/release-info.py", "release-profiles.json",
    "release_profile.rs",
)) + WINDOWS_TOOLCHAIN
APPLE_TOOLCHAIN = ("scripts/ci/apple-toolchain.json", "scripts/ci/select-xcode.py")
# The composite actions that carry Xcode's Metal Toolchain from GitHub's xcode-27 image
# to Blacksmith macOS jobs (agent-desktop-build.yml macos-build, agent-package-macos).
METAL_TOOLCHAIN = (".github/actions/export-metal-toolchain/**", ".github/actions/import-metal-toolchain/**")
# What agent-desktop-build.yml builds and signs on master (`plan.py only agent-desktop`).
AGENT_DESKTOP = AGENT + RUST_SDK + PROXY_RUNTIME + (
    ".github/workflows/agent-desktop-build.yml", ".github/actions/setup-agent-windows/**",
    "scripts/ci/verify-agent-rust-deps.py", *APPLE_TOOLCHAIN, *METAL_TOOLCHAIN, "scripts/ci/agent_windows.py",
    "scripts/ci/cargo_graph_key.py", "scripts/ci/install-windows-artifact-signing.ps1",
    "scripts/ci/windows-artifact-sign.ps1",
)
# Selectors for workflows outside ci.yml, read by `plan.py only NAME`.
SELECTORS = {"agent-desktop": AGENT_DESKTOP}
RESEARCH = "apps/maple-research/frontend/"
TAURI = RESEARCH + "src-tauri/"


@dataclass(frozen=True)
class Lane:
    id: str
    paths: Tuple[str, ...]                      # own inputs
    also: Tuple[str, ...] = ()                  # CI-only fan-out to consumers
    pr_paths: Optional[Tuple[str, ...]] = None  # narrower selector for PRs and the merge queue
    labels: Tuple[str, ...] = ("ci:full",)      # PR labels that add the lane
    events: FrozenSet[str] = GATE
    job: str = "nix"                            # a MATRIX_JOBS job, or a special ci.yml job id
    runner: Runner = LINUX
    flake: Optional[str] = None                 # `nix develop` flake, from the repository root
    workdir: str = "."
    run: str = ""
    env: Tuple[Tuple[str, str], ...] = ()
    cache: str = ""                             # rust-cache shared key under ci.yml's ci-v2 prefix
    workspaces: str = ""
    cache_files: Tuple[str, ...] = ()           # toolchain pins, hashed into the shared key
    cache_targets: bool = True                  # False caches only the Cargo home: registry and git sources
    timeout: int = 15
    gate: bool = True                           # selected by CI_INFRA edits
    opensecret: Optional[str] = None            # the opensecret_change_detection.py output that selects it
    submodules: bool = False                    # check out submodules, with the full (blobless) history Nix needs
    hook: Optional[str] = None                  # the pre-commit component that runs these checks
    restore: str = ""                           # read-only restore of a cache another workflow writes
    requires: Tuple[str, ...] = ()              # lanes whose jobs this lane's job needs; selected with it


AGENT_DIR = dict(
    workdir="apps/maple-agent", workspaces="apps/maple-agent -> target",
    cache_files=("apps/maple-agent/flake.lock",),
    env=(("RUSTFLAGS", "-D warnings"),),
)
AGENT_FLAKE = dict(flake="./apps/maple-agent", **AGENT_DIR)
# The macOS lanes also key their caches on the Xcode pin, so an Xcode upgrade never
# restores objects built against the previous SDK.
AGENT_MACOS = dict(AGENT_FLAKE, runner=MACOS,
                   cache_files=AGENT_DIR["cache_files"] + ("scripts/ci/apple-toolchain.json",))
# gpui compiles its Metal shaders at run time instead of build time, so these lanes need
# no Metal Toolchain (Blacksmith's macOS image has none). Compiled shaders are proven by
# master's signed Prod build on every Agent push, the nightly Dev build and ci:package.
RUNTIME_SHADERS = "-F gpui-platform/runtime_shaders"
# Lanes that only resolve the Agent's graph share its Cargo home, without a target
# directory, so they stop cloning the zed and cua git dependencies on every run.
AGENT_CARGO_HOME = dict(AGENT_FLAKE, cache="agent-cargo-home", cache_targets=False)
AGENT_WIN = dict(job="windows", runner=WINDOWS, **AGENT_DIR)  # pinned rustup, no flake
FAST_RELEASE = (("CARGO_PROFILE_RELEASE_LTO", "thin"), ("CARGO_PROFILE_RELEASE_CODEGEN_UNITS", "16"))
AGENT_PACKAGE_FLAKE = dict(AGENT_FLAKE, env=AGENT_DIR["env"] + FAST_RELEASE)
AGENT_DEPENDENCIES = tuple("apps/maple-agent/" + glob for glob in (
    "Cargo.lock", "**/Cargo.toml", "deny.toml", "flake.nix", "flake.lock", "dependencies/**",
    "scripts/audit-sqlcipher-dependencies.py", "scripts/verify-sqlcipher-build.py",
    "crates/maple-agent/src/store/keys/eff-large-wordlist.txt",
))
WINDOWS_PACKAGE = (
    # windows-release.py requires a build number; like package-release.sh, use the
    # Actions run number, or 1 outside Actions.
    'export MAPLE_PACKAGE_BUILD_NUMBER="${GITHUB_RUN_NUMBER:-1}"'
    " && ./scripts/build-release.sh dev && python3 scripts/windows-release.py prepare dev"
    " && python3 scripts/windows-release.py package dev --unsigned"
    # The stock image exports Azure CLI paths that the fail-closed signing guard rejects.
    " && env -u AZURE_CONFIG_DIR -u AZURE_DEVOPS_CACHE_DIR -u AZURE_EXTENSION_DIR"
    " python3 scripts/windows-release.py verify dev dist/dev --unsigned --smoke"
)

PROXY_CONTAINER = ("proxy/Dockerfile", ".dockerignore", "proxy/Cargo.lock") + PROXY_RUNTIME + RUST_SDK
SDK_CRATE = (
    'crate="$(mktemp -d)" && ./scripts/ci/build-sdk-publish.sh rust "$crate"'
    ' && python3 -I scripts/ci/sdk_crates_upload.py --directory "$crate" --validate-only --version'
    ' "$(python3 -I -c \'import pathlib, tomllib; print(tomllib.loads('
    'pathlib.Path("sdk/rust/Cargo.toml").read_text())["package"]["version"])\')"'
)


def deny(manifest: str, component: str) -> str:
    """cargo-deny with the component's own toolchain, from the Agent flake's nixpkgs
    (the cargo-deny that agent-supply-chain runs)."""
    return ("nix shell --no-update-lock-file --inputs-from ./apps/maple-agent nixpkgs#cargo-deny -c cargo deny"
            f" --manifest-path {manifest} --config {component}/deny.toml --all-features --locked"
            " check advisories bans")


DEPS = dict(runner=GITHUB, gate=False)
# Research platform builds: PR side only, never on CI edits (ci:full runs them).
PLATFORM = dict(events=PR_SIDE, gate=False)
DESKTOP = tuple(TAURI + glob for glob in ("tauri.conf.json", "build.rs", "scripts/onnxruntime-pins.sh",
                                          "scripts/run-with-desktop-onnxruntime.sh")) + tuple(
    "scripts/ci/" + name for name in ("desktop-pr.sh", "desktop-release.sh", "_common.sh", "tauri_bundle_type.py",
                                      "verify-release-artifacts.sh", "attestation-manifest.sh"))
# The Windows PR lane is the only one that runs the scripts it shares with DESKTOP in Git Bash.
WINDOWS_DESKTOP = (TAURI + "tauri.windows.conf.json", TAURI + "resources/windows/**", TAURI + "scripts/*windows*",
                   TAURI + "scripts/onnxruntime-pins.sh", "scripts/ci/desktop-windows-*.sh", "scripts/ci/canonical-windows-*.py", "scripts/ci/_common.sh",
                   "scripts/ci/verify-release-artifacts.sh")

# The paths only OpenSecret lanes consume; plan.py counts them as classified.
OPENSECRET_HOME = "services/opensecret/**"
OPENSECRET = (OPENSECRET_HOME, "sdk/test/**", ".gitmodules",
              ".agents/skills/validate-opensecret/scripts/disposable_db_tests.sh")
OPENSECRET_SHELL = "./services/opensecret?submodules=1#ci"
OPENSECRET_ENV = (("OPENSECRET_DEV_POSTGRES", "0"), ("OPENSECRET_DEV_ENV", "0"), ("OPENSECRET_DEV_CONTAINERS", "0"),
                  ("RUSTFLAGS", "-D warnings"))
# Every FlakeHub EIF builder runs on opensecret-eif-release.yml's runner class (test_contracts C5).
EIF_BUILDER = Runner("blacksmith-16vcpu-ubuntu-2404-arm")

LANES: Tuple[Lane, ...] = (
    Lane("agent-lint", AGENT, also=RUST_SDK + PROXY_RUNTIME + ("scripts/ci/verify-agent-rust-deps.py",),
         run="just ci-lint && python3 ../../scripts/ci/plan.py check-rust", runner=LINUX_HEAVY,
         cache="agent-lint", timeout=40, hook="agent", **AGENT_FLAKE),
    Lane("agent-test", AGENT, also=RUST_SDK + PROXY_RUNTIME, runner=LINUX_HEAVY,
         run="just ci-test", cache="agent-test", timeout=40, hook="agent", **AGENT_FLAKE),
    Lane("agent-macos", AGENT, also=RUST_SDK + PROXY_RUNTIME + APPLE_TOOLCHAIN,
         run=f"cargo clippy --workspace --all-targets --locked {RUNTIME_SHADERS} -- -D warnings",
         cache="agent-macos", timeout=25, **AGENT_MACOS),
    # Windows Clippy and the native PE-metadata and host-spawn fixtures, on every Agent change.
    Lane("agent-windows", AGENT, also=RUST_SDK + PROXY_RUNTIME + ("scripts/ci/agent_windows.py",),
         labels=("ci:windows", "ci:full"),
         run="python3 scripts/test_windows_build_info.py && python3 scripts/test_windows_host_spawn.py"
             " && cargo clippy --workspace --all-targets --locked -- -D warnings",
         cache="agent-windows", timeout=45, **AGENT_WIN),
    # The updater's native Windows filesystem, Authenticode, transaction and bootstrap
    # tests, on GitHub's free image (16 cores only add queueing to a 2 s suite); master
    # pushes run them too and save their cache. Serial and no-fail-fast, as the suite's
    # fixtures were qualified.
    Lane("agent-windows-updater", AGENT_WINDOWS_UPDATER, labels=("ci:windows", "ci:full"),
         run="cargo test -p maple-agent-updater --locked --no-fail-fast -- --test-threads=1",
         cache="agent-windows-updater", timeout=30, **dict(AGENT_WIN, runner=GITHUB_WINDOWS_FREE)),
    # The rest of `just ci` and the native macOS suite (with runtime shaders, see
    # RUNTIME_SHADERS): master, nightly and ci:full.
    Lane("agent-full-linux", AGENT, also=RUST_SDK + PROXY_RUNTIME, pr_paths=(), run="just ci-extra",
         cache="agent-full-linux", timeout=30, gate=False, **AGENT_FLAKE),
    Lane("agent-full-macos", AGENT, also=RUST_SDK + PROXY_RUNTIME + APPLE_TOOLCHAIN, pr_paths=(),
         run="python3 scripts/test_git_metadata.py"
             f" && cargo build --workspace --all-targets --locked {RUNTIME_SHADERS}"
             f" && ./scripts/macos-debug-app.sh && cargo test --workspace --locked {RUNTIME_SHADERS}",
         cache="agent-full-macos", timeout=45, gate=False, **AGENT_MACOS),
    # The native Windows suite: nightly (which saves its cache) and by label, on the free runner.
    Lane("agent-full-windows", AGENT, also=RUST_SDK + PROXY_RUNTIME, pr_paths=(), labels=("ci:windows", "ci:full"),
         events=PR_AND_NIGHTLY,
         run="python3 scripts/test_git_metadata.py && cargo build --workspace --all-targets --locked"
             " && cargo test --workspace --locked",
         cache="agent-full-windows", timeout=90, gate=False, **dict(AGENT_WIN, runner=GITHUB_WINDOWS_FREE)),
    # Pinned SQLCipher sources, per-target advisories and bans.
    Lane("agent-supply-chain", AGENT_DEPENDENCIES, run="just supply-chain", timeout=30, **AGENT_CARGO_HOME),
    # Released rusqlite's own tests against the static SQLCipher provider (nightly.yml: all three OSes).
    Lane("agent-provider", tuple("apps/maple-agent/" + glob for glob in (
        "Cargo.lock", "dependencies/sqlcipher/**", "scripts/test-rusqlite-provider.py",
        "scripts/test_rusqlite_provider.py",
    )), events=NOT_NIGHTLY, run="python3 scripts/test_rusqlite_provider.py && mkdir -p target"
                              " && cargo metadata --locked --format-version 1 > target/provider-metadata.json"
                              " && python3 scripts/test-rusqlite-provider.py target/provider-metadata.json",
         timeout=20, **AGENT_CARGO_HOME),
    # Unsigned Dev packages through the scripts agent-desktop-build.yml uses on master.
    # The rehearsal proves packaging, not the optimizer: thin LTO with parallel codegen
    # instead of master's fat LTO, from its own cache that the nightly run refreshes.
    # Packaging builds take 15-40 minutes, so no PR waits on them: ci:package, nightly,
    # and master's real builds in agent-desktop-build.yml.
    Lane("agent-package-linux", AGENT_PACKAGING, pr_paths=(), labels=("ci:package", "ci:full"),
         events=PR_AND_NIGHTLY, job="agent-package-linux", run="just package-rehearsal", timeout=60, gate=False, **AGENT_PACKAGE_FLAKE),
    Lane("agent-package-macos", AGENT_PACKAGING + APPLE_TOOLCHAIN + METAL_TOOLCHAIN, pr_paths=(), labels=("ci:package", "ci:full"),
         events=PR_AND_NIGHTLY, job="agent-package-macos", runner=MACOS, run="just package-rehearsal", timeout=60,
         gate=False, requires=("metal-toolchain",), **AGENT_PACKAGE_FLAKE),
    # Exports Xcode's Metal Toolchain as a one-day artifact for the macOS packaging rehearsal,
    # which compiles gpui's shaders like the signed builds. Nothing selects it by path or
    # label: the planner adds it to every plan with a lane that requires it.
    Lane("metal-toolchain", (), pr_paths=(), labels=(), gate=False, job="metal-toolchain",
         runner=GITHUB_XCODE, timeout=15),
    Lane("agent-package-windows", AGENT_WINDOWS_PACKAGING, pr_paths=(), labels=("ci:package", "ci:full"),
         events=PR_AND_NIGHTLY, job="agent-package-windows", runner=GITHUB_WINDOWS, workdir="apps/maple-agent", run=WINDOWS_PACKAGE,
         env=(("RUSTFLAGS", "-D warnings -C target-feature=+crt-static"),) + FAST_RELEASE, timeout=90, gate=False),
    # Every root flake check: actionlint, workflow policy and contracts, CI script
    # tests, the release gates, and release metadata (read from the version files).
    Lane("repo", (
        ".github/**", "scripts/ci/**", ".githooks/**", "flake.nix", "flake.lock", "justfile", "setup-hooks.sh",
        ".agents/skills/release-maple/scripts/**", "services/agent-updates/publisher/**",
        "scripts/update-version.py", "apps/maple-research/zapstore.yaml", "proxy/Cargo.toml", "proxy/justfile",
        RESEARCH + "package.json", TAURI + "Cargo.toml", TAURI + "tauri.conf.json",
    ), runner=GITHUB, run="nix flake check --no-update-lock-file --print-build-logs", timeout=15, hook="repo"),

    # The SDKs, proxy and container. build-sdk-publish.sh is the publishers' own builder.
    Lane("sdk-ts", SDK_TS + ("sdk/.prettierrc.json", "sdk/eslint.config.js", "sdk/justfile", "sdk/.env.example",
                             "sdk/LICENSE.md", "scripts/ci/build-sdk-publish.sh",
                             "scripts/ci/check-sdk-package-consumers.py"),
         runner=GITHUB, run='./scripts/ci/build-sdk-publish.sh npm "$(mktemp -d)"', hook="sdk_ts"),
    Lane("sdk-rust", ("sdk/rust/**", "sdk/rust-toolchain.toml", "sdk/flake.*", "scripts/ci/build-sdk-publish.sh",
                      "scripts/ci/sdk_crates_upload.py"),
         runner=GITHUB, flake=".#pages", run=SDK_CRATE, hook="sdk_rust"),  # .#pages: a Python with tomllib
    Lane("proxy", ("proxy/**", "!proxy/Dockerfile"), also=RUST_SDK, runner=GITHUB, flake="./proxy",
         workdir="proxy", run='just check && RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --all-features'
                             " && cargo machete",
         env=(("RUSTFLAGS", "-D warnings"),), hook="proxy"),
    # PRs build the image only when its recipe changes; master builds both architectures.
    Lane("proxy-container", PROXY_CONTAINER, pr_paths=("proxy/Dockerfile", ".dockerignore"), runner=GITHUB,
         run="docker build --platform linux/amd64 -f proxy/Dockerfile .", timeout=20),
    Lane("proxy-container-arm64", PROXY_CONTAINER, pr_paths=(), runner=Runner("ubuntu-24.04-arm"),
         run="docker build --platform linux/arm64 -f proxy/Dockerfile .", timeout=20, gate=False),
    # Advisories and bans when a dependency graph changes. A new advisory is drift,
    # not a CI change, so CI edits skip these; ci.yml's nightly run re-checks them.
    Lane("deps-research", ("apps/maple-research/deny.toml", TAURI + "Cargo.toml", TAURI + "Cargo.lock",
                           "proxy/Cargo.toml", "sdk/rust/Cargo.toml"),
         run=deny(TAURI + "Cargo.toml", "apps/maple-research"), flake=".#ci", **DEPS),
    Lane("deps-sdk", ("sdk/deny.toml", "sdk/rust/Cargo.toml", "sdk/rust/Cargo.lock"),
         run=deny("sdk/rust/Cargo.toml", "sdk"), flake="./sdk", **DEPS),
    Lane("deps-proxy", ("proxy/deny.toml", "proxy/Cargo.toml", "proxy/Cargo.lock", "sdk/rust/Cargo.toml"),
         run=deny("proxy/Cargo.toml", "proxy"), flake="./proxy", **DEPS),

    # Research web and Rust on PRs; frontend-tests.yml, web-build.yml and
    # rust-tests.yml verify master (rust-tests.yml also writes research-rust's cache).
    Lane("research-web", (RESEARCH + "**", "!" + TAURI + "**", "scripts/prepare-*.sh", "scripts/ci/frontend.sh",
                          "scripts/ci/web.sh", "scripts/ci/_common.sh", "flake.nix", "flake.lock"),
         events=PR_SIDE, runner=GITHUB, flake=".#ci",
         run="./scripts/ci/frontend.sh && MAPLE_WEB_ENVIRONMENT=pr ./scripts/ci/web.sh", hook="research_frontend"),
    Lane("research-rust", (TAURI + "**", "!" + TAURI + "gen/**", "scripts/ci/rust.sh",
                           "scripts/ci/verify-local-rust-deps.sh", "scripts/ci/_common.sh", "flake.nix", "flake.lock"),
         also=RUST_SDK + PROXY_RUNTIME + ("scripts/ci/verify-agent-rust-deps.py",), events=PR_SIDE,
         job="research-rust", flake=".#ci", timeout=45, hook="research_rust",
         run="(cd apps/maple-research/frontend/src-tauri && cargo fmt --check)"
             " && ./scripts/ci/verify-local-rust-deps.sh && ./scripts/ci/rust.sh"),
    # Unsigned platform builds: iOS and Android for PRs that touch their native inputs;
    # the desktop bundles (15-45 minutes) only by label, so no PR waits on them.
    # desktop-build.yml, mobile-build.yml and android-build.yml build master.
    Lane("research-ios", (TAURI + "gen/apple/**", TAURI + "icons/ios/**", TAURI + "capabilities/mobile-ios.json",
                          TAURI + "tauri.ios-dev.conf.json", TAURI + "Info.plist", TAURI + "Entitlements.plist",
                          # iOS-only sources: the scene manifest tauri-cli merges into the app's
                          # plist, plugin Swift code, and the vendored tao's iOS platform layer.
                          TAURI + "Info.ios.plist", TAURI + "plugins/*/ios/**", TAURI + "patches/*/ios/**",
                          TAURI + "patches/tao-*/src/platform_impl/ios/**",
                          TAURI + "scripts/*ios*", TAURI + "scripts/canonicalize-static-archive.py",
                          TAURI + "scripts/onnxruntime-pins.sh", "scripts/ci/ios-*.sh", "scripts/ci/ios-build-profile.py",
                          "scripts/ci/canonical-ios-app-hash.py", "scripts/ci/_common.sh",
                          "scripts/ci/apple-toolchain.json", "scripts/ci/select-xcode.py"),
         labels=("ci:ios", "ci:full"), job="research-ios", runner=MACOS, flake=".#apple",
         run="./scripts/ci/ios-onnxruntime.sh && ./scripts/ci/ios-pr.sh",
         env=(("MAPLE_IOS_DOWNLOAD_SIMULATOR_RUNTIME", "1"),), timeout=120, **PLATFORM),
    Lane("research-android", (TAURI + "gen/android/**", TAURI + "icons/android/**",
                              TAURI + "capabilities/mobile-android.json", TAURI + "scripts/*android*",
                              "scripts/ci/android-*.sh", "scripts/ci/_common.sh"),
         labels=("ci:android", "ci:full"), flake=".#android", run="./scripts/ci/android-pr.sh", timeout=45,
         restore="research-rust", **PLATFORM),
    Lane("research-desktop-macos", DESKTOP + (TAURI + "tauri.macos.conf.json", TAURI + "Info.plist",
                                              TAURI + "Entitlements.plist", TAURI + "dmg/**", TAURI + "scripts/*macos*",
                                              "scripts/ci/dmg-settings.py", "scripts/ci/test-canonical-macho.py",
                                              "scripts/ci/apple-toolchain.json", "scripts/ci/select-xcode.py"), pr_paths=(),
         labels=("ci:package", "ci:full"), job="research-desktop-macos", runner=MACOS, flake=".#ci",
         run="./scripts/ci/desktop-pr.sh", timeout=45, **PLATFORM),
    Lane("research-desktop-linux", DESKTOP + (TAURI + "scripts/*linux*",), pr_paths=(), labels=("ci:package", "ci:full"),
         job="research-desktop-linux", flake=".#desktop-linux", run="./scripts/ci/desktop-pr.sh",
         env=(("MAPLE_TAURI_FAKE_UPDATER_SIGNING", "1"),), timeout=45, **PLATFORM),
    # Windows verifies its own proofs after the build.
    Lane("research-desktop-windows", WINDOWS_DESKTOP, pr_paths=(), labels=("ci:package", "ci:full"),
         job="research-desktop-windows", runner=GITHUB_WINDOWS, timeout=60, **PLATFORM,
         run="./scripts/ci/desktop-windows-pr.sh"
             " && ./scripts/ci/verify-release-artifacts.sh ./apps/maple-research/frontend/src-tauri/target windows"),
    # The unsigned Research Dev app for OAuth rehearsal (apps/maple-research/docs/research-dev-desktop.md).
    Lane("research-dev-macos", (TAURI + "desktop-dev-profile.json", TAURI + "tauri.desktop-dev.conf.json",
                                "scripts/ci/research-dev-desktop.sh", "scripts/ci/research-dev-profile.py"), pr_paths=(),
         labels=("ci:research-dev-macos", "ci:package", "ci:full"), job="research-dev-macos", runner=MACOS,
         flake=".#ci", run="./scripts/ci/research-dev-desktop.sh", timeout=45, **PLATFORM),

    # Services and the hosted Auth app. auth-pages-ci.yml also builds Auth, for the
    # Dev deployment on master and a preview artifact on PRs.
    Lane("auth", ("apps/maple-auth/**", "scripts/ci/auth-*.sh", "scripts/ci/pages_auth_build.py",
                  "scripts/ci/pages_artifact.py", "flake.nix", "flake.lock"),
         events=PR_SIDE, runner=GITHUB, flake=".#ci", run="./scripts/ci/auth-ci.sh", hook="auth"),
    Lane("updates", ("services/updates/**", "services/download-tracking/**", "scripts/ci/updates.sh",
                     "scripts/ci/_common.sh", "flake.nix", "flake.lock"),
         runner=GITHUB, flake=".#ci", run="./scripts/ci/updates.sh", hook="updates"),
    Lane("agent-updates", ("services/agent-updates/**", "services/download-tracking/**", "scripts/ci/agent-updates.sh",
                           "scripts/ci/test_agent_updates_workflows.py", "apps/maple-agent/release-profiles.json",
                           "apps/maple-agent/scripts/release-info.py", "scripts/ci/_common.sh", "flake.nix",
                           "flake.lock"),
         runner=GITHUB, flake=".#ci", run="./scripts/ci/agent-updates.sh", hook="agent_updates"),

    # OpenSecret, selected by its own selector (see the module docstring). The PR
    # EIF candidate builds only for same-repository PRs to master that edit no
    # approval, on the release builder's runner class; opensecret-eif.yml compares
    # approvals.
    Lane("os-rust", (), opensecret="rust", flake=OPENSECRET_SHELL, workdir="services/opensecret",
         run="cargo fmt --all -- --check && cargo clippy --locked --all-targets --all-features -- -D warnings"
             " && cargo test --locked --all-features",
         cache="os-rust", workspaces="services/opensecret -> target", cache_files=tuple(
             "services/opensecret/" + name for name in ("rust-toolchain.toml", "flake.lock")),
         submodules=True, timeout=30, env=OPENSECRET_ENV, hook="opensecret"),
    Lane("os-sdk-e2e", ("scripts/ci/opensecret-sdk-e2e.sh",), opensecret="integration", job="os-sdk-e2e",
         run="./scripts/ci/opensecret-sdk-e2e.sh", timeout=45),
    # The component flake's checks.
    Lane("os-flake-check", (), opensecret="nix", runner=GITHUB, workdir="services/opensecret",
         run="nix flake check --no-update-lock-file --print-build-logs '.?submodules=1'", submodules=True,
         timeout=30, env=OPENSECRET_ENV, hook="opensecret"),  # the hook runs its entrypoint checks
    Lane("os-helpers", ("scripts/ci/opensecret-helpers.sh",), opensecret="helpers", job="os-helpers",
         runner=Runner("ubuntu-24.04-arm"),
         run="./scripts/ci/opensecret-helpers.sh", timeout=30),
    Lane("os-pcr", (), opensecret="pcr", runner=GITHUB, timeout=10,
         run='nix build --no-update-lock-file --no-link'
             ' ".#checks.$(nix eval --impure --raw --expr builtins.currentSystem).pcr-compatibility"'),
    # Not the selector's `audit` output, which CI files also trigger: like the other
    # deps lanes, only the dependency graph's own inputs select it.
    Lane("deps-opensecret", tuple("services/opensecret/" + name for name in ("Cargo.toml", "Cargo.lock", "deny.toml")),
         flake=OPENSECRET_SHELL, submodules=True, env=OPENSECRET_ENV,
         run=deny("services/opensecret/Cargo.toml", "services/opensecret"), **DEPS),
    # ci.yml's os-eif job runs these two scripts with the FlakeHub cache; `just lane os-eif` runs them locally.
    Lane("os-eif", (), opensecret="eif", labels=("ci:eif",), events=frozenset({PR}), job="os-eif", runner=EIF_BUILDER,
         run='mode="${EIF_MODE:?dev or prod}" && out="$(mktemp -d)/eif-candidate-$mode"'
             ' && bash scripts/ci/build_opensecret_eif.sh "$mode" "$out"'
             ' && python3 scripts/ci/verify_opensecret_eif_rootfs.py "$mode" "$out" "$(git rev-parse HEAD)"',
         env=OPENSECRET_ENV, timeout=180, gate=False),
)
