"""The lane table: which ci.yml checks a change selects, and how each one runs.

Read by scripts/ci/plan.py (the ci.yml plan job, `just lane`, and lint). Globs
are repository-relative: `**` spans directories, `*` stays within one path
segment, and a leading `!` excludes paths from the globs it is listed with.

OpenSecret lanes are selected by opensecret_change_detection.py, imported
verbatim, from every changed path, inert ones included (`opensecret` names its
output). Their only globs are the entry scripts of os-sdk-e2e and os-helpers.

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


# The override variables are unset; set one to fail over during an outage.
LINUX = Runner("blacksmith-8vcpu-ubuntu-2404", "CI_LINUX_RUNNER")
LINUX_HEAVY = Runner("blacksmith-32vcpu-ubuntu-2404", "CI_LINUX_HEAVY_RUNNER")
MACOS = Runner("blacksmith-6vcpu-macos-26", "CI_MACOS_RUNNER")
WINDOWS = Runner("blacksmith-8vcpu-windows-2025", "CI_WINDOWS_RUNNER")
GITHUB = Runner("ubuntu-latest")  # free GitHub runner for lanes that compile nothing heavy
# .github/actions/setup-agent-windows pins this toolchain (Windows has no Nix);
# agent-lint checks that the Agent flake's rustc still reports it.
AGENT_WINDOWS_RUST = "1.98.0"

PR, QUEUE, PUSH, NIGHTLY, DISPATCH = (
    "pull_request", "merge_group", "push", "schedule", "workflow_dispatch")
GATE: FrozenSet[str] = frozenset({PR, QUEUE, PUSH, NIGHTLY, DISPATCH})
PR_EVENTS: FrozenSet[str] = frozenset({PR, QUEUE})  # events that use `pr_paths`
NOT_NIGHTLY: FrozenSet[str] = GATE - {NIGHTLY}  # nightly.yml runs these every night instead
PR_SIDE: FrozenSet[str] = frozenset({PR, QUEUE, DISPATCH})  # master and nightly run these elsewhere
# ci.yml's generic matrix jobs; any other `job` is the id of a special ci.yml job.
MATRIX_JOBS = ("nix", "windows")

# Never selects anything: notes, docs and hooks at the root or a component root
# (one or two levels down). Deeper files can be build inputs: the Agent compiles
# its bundled SKILL.md files and copies app/assets/dict/LICENSE into its notices.
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
STORE = "apps/maple-agent/crates/maple-agent/src/store/"
AGENT_WINDOWS = (
    "apps/maple-agent/**/*windows*", "apps/maple-agent/app/build.rs",
    STORE + "paths.rs", STORE + "keys.rs", STORE + "keys/**", STORE + "lifecycle.rs", STORE + "lifecycle/**",
    "scripts/ci/agent_windows.py",
)
# The packaging scripts and what they call, and the toolchain that builds the bundles.
AGENT_PACKAGING = tuple("apps/maple-agent/" + glob for glob in (
    "scripts/build-*", "scripts/package-*", "scripts/verify-*", "scripts/release-info.py",
    "scripts/macos-build-info.py", "scripts/macos-release-*", "scripts/linux-*", "scripts/smoke-*",
    "scripts/*third*party*notices*", "release-profiles.json", "release_profile.rs", "about.toml", "app/build.rs",
    "app/packaging/**", "flake.nix", "flake.lock",
)) + (".github/workflows/agent-desktop-build.yml",)
APPLE_TOOLCHAIN = ("scripts/ci/apple-toolchain.json", "scripts/ci/select-xcode.py")
# What agent-desktop-build.yml builds and signs on master (`plan.py only agent-desktop`).
AGENT_DESKTOP = AGENT + RUST_SDK + PROXY_RUNTIME + (
    ".github/workflows/agent-desktop-build.yml", ".github/actions/setup-agent-windows/**",
    "scripts/ci/verify-agent-rust-deps.py", *APPLE_TOOLCHAIN, "scripts/ci/agent_windows.py",
    "scripts/ci/install-windows-artifact-signing.ps1", "scripts/ci/windows-artifact-sign.ps1",
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
    cache: str = ""                             # rust-cache shared key under the ci-v1 prefix
    workspaces: str = ""
    cache_files: Tuple[str, ...] = ()           # hashed into the cache key
    timeout: int = 15
    gate: bool = True                           # selected by CI_INFRA edits
    opensecret: Optional[str] = None            # the opensecret_change_detection.py output that selects it
    submodules: bool = False                    # check out submodules, with the full history Nix needs
    hook: Optional[str] = None                  # the pre-commit component that runs these checks


AGENT_DIR = dict(
    workdir="apps/maple-agent", workspaces="apps/maple-agent -> target",
    cache_files=("apps/maple-agent/flake.lock", "apps/maple-agent/Cargo.lock"),
    env=(("RUSTFLAGS", "-D warnings"),),
)
AGENT_FLAKE = dict(flake="./apps/maple-agent", **AGENT_DIR)
AGENT_WIN = dict(job="windows", runner=WINDOWS, **AGENT_DIR)  # pinned rustup, no flake
AGENT_DEPENDENCIES = tuple("apps/maple-agent/" + glob for glob in (
    "Cargo.lock", "**/Cargo.toml", "deny.toml", "about.toml", "flake.nix", "flake.lock", "dependencies/**",
    "scripts/*third*party*notices*", "scripts/audit-sqlcipher-dependencies.py", "scripts/verify-sqlcipher-build.py",
    "app/assets/dict/LICENSE", "crates/maple-agent/src/store/keys/SOURCES.md",
    "crates/maple-agent/src/store/keys/eff-large-wordlist.txt",
))
WINDOWS_PACKAGE = (
    "./scripts/build-release.sh dev && python3 scripts/windows-release.py prepare dev"
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
WINDOWS_DESKTOP = (TAURI + "tauri.windows.conf.json", TAURI + "resources/windows/**", TAURI + "scripts/*windows*",
                   "scripts/ci/desktop-windows-*.sh", "scripts/ci/canonical-windows-*.py")
WINDOWS_LABELS = ("ci:windows", "ci:package", "ci:full")

# The paths only OpenSecret lanes consume; plan.py counts them as classified.
OPENSECRET_HOME = "services/opensecret/**"
OPENSECRET = (OPENSECRET_HOME, "sdk/test/**", ".gitmodules",
              ".agents/skills/validate-opensecret/scripts/disposable_db_tests.sh")
OPENSECRET_SHELL = "./services/opensecret?submodules=1#ci"
OPENSECRET_ENV = (("OPENSECRET_DEV_POSTGRES", "0"), ("OPENSECRET_DEV_ENV", "0"), ("OPENSECRET_DEV_CONTAINERS", "0"),
                  ("RUSTFLAGS", "-D warnings"))
# Every EIF build runs on opensecret-eif-release.yml's runner class (test_contracts C5).
EIF_BUILDER = Runner("blacksmith-8vcpu-ubuntu-2404-arm")

LANES: Tuple[Lane, ...] = (
    Lane("agent-lint", AGENT, also=RUST_SDK + PROXY_RUNTIME + ("scripts/ci/verify-agent-rust-deps.py",),
         run="just ci-lint && python3 ../../scripts/ci/plan.py check-rust",
         cache="agent-lint", timeout=15, hook="agent", **AGENT_FLAKE),
    Lane("agent-test", AGENT, also=RUST_SDK + PROXY_RUNTIME, runner=LINUX_HEAVY,
         run="just ci-test", cache="agent-test", timeout=20, hook="agent", **AGENT_FLAKE),
    Lane("agent-macos", AGENT, also=RUST_SDK + PROXY_RUNTIME + APPLE_TOOLCHAIN,
         runner=MACOS, run="cargo clippy --workspace --all-targets --locked -- -D warnings",
         cache="agent-macos", timeout=25, **AGENT_FLAKE),
    # PRs get the native Windows tests and Clippy only for Windows-specific paths or the ci:windows label.
    Lane("agent-windows", AGENT, also=RUST_SDK + PROXY_RUNTIME, pr_paths=AGENT_WINDOWS,
         labels=("ci:windows", "ci:full"),
         run="python3 scripts/test_windows_build_info.py && python3 scripts/test_windows_host_spawn.py"
             " && cargo clippy --workspace --all-targets --locked -- -D warnings",
         cache="agent-windows", timeout=45, **AGENT_WIN),
    # The rest of `just ci`, and the native macOS and Windows suites: master, nightly and ci:full.
    Lane("agent-full-linux", AGENT, also=RUST_SDK + PROXY_RUNTIME, pr_paths=(), run="just ci-extra",
         cache="agent-full-linux", timeout=30, gate=False, **AGENT_FLAKE),
    Lane("agent-full-macos", AGENT, also=RUST_SDK + PROXY_RUNTIME, pr_paths=(), runner=MACOS,
         run="python3 scripts/test_git_metadata.py && cargo build --workspace --all-targets --locked"
             " && ./scripts/macos-debug-app.sh && cargo test --workspace --locked",
         cache="agent-full-macos", timeout=45, gate=False, **AGENT_FLAKE),
    Lane("agent-full-windows", AGENT, also=RUST_SDK + PROXY_RUNTIME, pr_paths=(),
         run="python3 scripts/test_git_metadata.py && cargo build --workspace --all-targets --locked"
             " && cargo test --workspace --locked",
         cache="agent-full-windows", timeout=60, gate=False, **AGENT_WIN),
    # Pinned SQLCipher sources, per-target advisories and bans, and notices.
    Lane("agent-supply-chain", AGENT_DEPENDENCIES, run="just supply-chain", **AGENT_FLAKE),
    # Released rusqlite's own tests against the static SQLCipher provider (nightly.yml: all three OSes).
    Lane("agent-provider", tuple("apps/maple-agent/" + glob for glob in (
        "Cargo.lock", "dependencies/sqlcipher/**", "scripts/test-rusqlite-provider.py",
        "scripts/test_rusqlite_provider.py",
    )), events=NOT_NIGHTLY, run="python3 scripts/test_rusqlite_provider.py && mkdir -p target"
                              " && cargo metadata --locked --format-version 1 > target/provider-metadata.json"
                              " && python3 scripts/test-rusqlite-provider.py target/provider-metadata.json",
         timeout=20, **AGENT_FLAKE),
    Lane("agent-perf", (), labels=("ci:perf", "ci:full"), events=PR_SIDE, run="just store-bench-ci",
         timeout=30, gate=False, **AGENT_FLAKE),
    # Unsigned Dev packages built the way agent-desktop-build.yml builds master's.
    Lane("agent-package-linux", AGENT_PACKAGING, labels=("ci:package", "ci:full"), events=PR_SIDE,
         job="agent-package-linux", run="just package-rehearsal", timeout=60, gate=False, **AGENT_FLAKE),
    Lane("agent-package-macos", AGENT_PACKAGING + APPLE_TOOLCHAIN, labels=("ci:package", "ci:full"),
         events=PR_SIDE, job="agent-package-macos", runner=MACOS, run="just package-rehearsal", timeout=60,
         gate=False, **AGENT_FLAKE),
    Lane("agent-package-windows", (), labels=("ci:package", "ci:full"), events=PR_SIDE,
         job="agent-package-windows", runner=WINDOWS, workdir="apps/maple-agent", run=WINDOWS_PACKAGE,
         env=(("RUSTFLAGS", "-D warnings -C target-feature=+crt-static"),), timeout=60, gate=False),
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
         job="research-rust", flake=".#ci", timeout=20, hook="research_rust",
         run="(cd apps/maple-research/frontend/src-tauri && cargo fmt --check)"
             " && ./scripts/ci/verify-local-rust-deps.sh && ./scripts/ci/rust.sh"),
    # Unsigned platform builds for PRs that touch native inputs, or by label;
    # desktop-build.yml, mobile-build.yml and android-build.yml build master.
    Lane("research-ios", (TAURI + "gen/apple/**", TAURI + "icons/ios/**", TAURI + "capabilities/mobile-ios.json",
                          TAURI + "tauri.ios-dev.conf.json", TAURI + "Info.plist", TAURI + "Entitlements.plist",
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
         labels=("ci:android", "ci:full"), flake=".#android", run="./scripts/ci/android-pr.sh", timeout=30,
         **PLATFORM),
    Lane("research-desktop-macos", DESKTOP + (TAURI + "tauri.macos.conf.json", TAURI + "Info.plist",
                                              TAURI + "Entitlements.plist", TAURI + "dmg/**", TAURI + "scripts/*macos*",
                                              "scripts/ci/dmg-settings.py", "scripts/ci/test-canonical-macho.py",
                                              "scripts/ci/apple-toolchain.json", "scripts/ci/select-xcode.py"),
         labels=("ci:package", "ci:full"), job="research-desktop-macos", runner=MACOS, flake=".#ci",
         run="./scripts/ci/desktop-pr.sh", timeout=45, **PLATFORM),
    Lane("research-desktop-linux", DESKTOP + (TAURI + "scripts/*linux*",), labels=("ci:package", "ci:full"),
         job="research-desktop-linux", flake=".#desktop-linux", run="./scripts/ci/desktop-pr.sh",
         env=(("MAPLE_TAURI_FAKE_UPDATER_SIGNING", "1"),), timeout=45, **PLATFORM),
    # Windows runs on PRs only for Windows paths or a label; its proofs are verified on Linux.
    Lane("research-desktop-windows", WINDOWS_DESKTOP, labels=WINDOWS_LABELS, job="research-desktop-windows",
         runner=WINDOWS, run="./scripts/ci/desktop-windows-pr.sh", timeout=60, **PLATFORM),
    Lane("research-desktop-windows-verify", WINDOWS_DESKTOP, labels=WINDOWS_LABELS,
         job="research-desktop-windows-verify", runner=GITHUB, flake=".#ci",
         run="./scripts/ci/verify-release-artifacts.sh artifacts windows", **PLATFORM),
    # The unsigned Research Dev app for OAuth rehearsal (apps/maple-research/docs/research-dev-desktop.md).
    Lane("research-dev-macos", (TAURI + "desktop-dev-profile.json", TAURI + "tauri.desktop-dev.conf.json",
                                "scripts/ci/research-dev-desktop.sh", "scripts/ci/research-dev-profile.py"),
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
             "services/opensecret/" + name for name in ("Cargo.lock", "rust-toolchain.toml", "flake.lock")),
         submodules=True, timeout=30, env=OPENSECRET_ENV, hook="opensecret"),
    Lane("os-sdk-e2e", ("scripts/ci/opensecret-sdk-e2e.sh",), opensecret="integration", job="os-sdk-e2e",
         run="./scripts/ci/opensecret-sdk-e2e.sh", timeout=45),
    # The component flake's checks and the x86_64 application package.
    Lane("os-flake-check", (), opensecret="nix", runner=GITHUB, workdir="services/opensecret",
         run="nix flake check --no-update-lock-file --print-build-logs '.?submodules=1'"
             " && nix build --no-link --no-update-lock-file '.?submodules=1#default'", submodules=True,
         timeout=60, env=OPENSECRET_ENV, hook="opensecret"),  # the hook runs its entrypoint checks
    Lane("os-helpers", ("scripts/ci/opensecret-helpers.sh",), opensecret="helpers", job="os-helpers",
         runner=Runner("ubuntu-24.04-arm"),
         run="./scripts/ci/opensecret-helpers.sh", timeout=30),
    Lane("os-pcr", (), opensecret="pcr", runner=GITHUB, timeout=10,
         run='nix build --no-update-lock-file --no-link'
             ' ".#checks.$(nix eval --impure --raw --expr builtins.currentSystem).pcr-compatibility"'),
    Lane("deps-opensecret", (), opensecret="audit", flake=OPENSECRET_SHELL, submodules=True, env=OPENSECRET_ENV,
         run=deny("services/opensecret/Cargo.toml", "services/opensecret"), **DEPS),
    # ci.yml's os-eif job runs these two scripts with the FlakeHub cache; `just lane os-eif` runs them locally.
    Lane("os-eif", (), opensecret="eif", labels=("ci:eif",), events=frozenset({PR}), job="os-eif", runner=EIF_BUILDER,
         run='mode="${EIF_MODE:?dev or prod}" && out="$(mktemp -d)/eif-candidate-$mode"'
             ' && bash scripts/ci/build_opensecret_eif.sh "$mode" "$out"'
             ' && python3 scripts/ci/verify_opensecret_eif_rootfs.py "$mode" "$out" "$(git rev-parse HEAD)"',
         env=OPENSECRET_ENV, timeout=180, gate=False),
)
