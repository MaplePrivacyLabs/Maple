"""The lane table: which ci.yml checks a change selects, and how each one runs.

Read by scripts/ci/plan.py (the ci.yml plan job, `just lane`, and lint). Globs
are repository-relative: `**` spans directories, `*` stays within one path
segment, and a leading `!` excludes paths from the globs it is listed with.

A lane with `legacy` set is still run by that workflow. Its paths are
classified here, so they never count as unknown, but ci.yml selects nothing for
it until its migration clears `legacy` and fills in how it runs.
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
LINUX_HEAVY = Runner("blacksmith-8vcpu-ubuntu-2404", "CI_LINUX_HEAVY_RUNNER")
MACOS = Runner("blacksmith-6vcpu-macos-26", "CI_MACOS_RUNNER")
WINDOWS = Runner("blacksmith-8vcpu-windows-2025", "CI_WINDOWS_RUNNER")  # read by ci.yml itself
GITHUB = Runner("ubuntu-latest")  # free GitHub runner for lanes that compile nothing heavy
# ci.yml pins this toolchain for agent-windows; agent-lint checks that the Agent
# flake's rustc still reports it.
AGENT_WINDOWS_RUST = "1.98.0"

PR, QUEUE, PUSH, NIGHTLY, DISPATCH = (
    "pull_request", "merge_group", "push", "schedule", "workflow_dispatch")
GATE: FrozenSet[str] = frozenset({PR, QUEUE, PUSH, NIGHTLY, DISPATCH})
PR_EVENTS: FrozenSet[str] = frozenset({PR, QUEUE})  # events that use `pr_paths`

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
AGENT_PACKAGING = tuple("apps/maple-agent/" + glob for glob in (
    "scripts/build-*", "scripts/package-*", "scripts/verify-*", "release-profiles.json",
    "release_profile.rs", "about.toml", "app/build.rs", "app/packaging/**", "flake.lock",
)) + (".github/workflows/agent-desktop-build.yml",)
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
    job: str = "nix"                            # the generic matrix, or a special ci.yml job id
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
    legacy: Optional[str] = None                # the workflow that still runs this lane


AGENT_FLAKE = dict(
    flake="./apps/maple-agent", workdir="apps/maple-agent", workspaces="apps/maple-agent -> target",
    cache_files=("apps/maple-agent/flake.lock", "apps/maple-agent/Cargo.lock"),
    env=(("RUSTFLAGS", "-D warnings"),),
)

LANES: Tuple[Lane, ...] = (
    Lane("agent-lint", AGENT, also=RUST_SDK + PROXY_RUNTIME + ("scripts/ci/verify-agent-rust-deps.py",),
         run="just ci-lint && python3 ../../scripts/ci/plan.py check-rust",
         cache="agent-lint", timeout=15, **AGENT_FLAKE),
    Lane("agent-test", AGENT, also=RUST_SDK + PROXY_RUNTIME, runner=LINUX_HEAVY,
         run="just ci-test", cache="agent-test", timeout=20, **AGENT_FLAKE),
    Lane("agent-macos", AGENT,
         also=RUST_SDK + PROXY_RUNTIME + ("scripts/ci/select-xcode.py", "scripts/ci/apple-toolchain.json"),
         runner=MACOS, run="cargo clippy --workspace --all-targets --locked -- -D warnings",
         cache="agent-macos", timeout=25, **AGENT_FLAKE),
    # PRs get Windows clippy only for Windows-specific paths or the ci:windows label.
    Lane("agent-windows", AGENT, also=RUST_SDK + PROXY_RUNTIME, pr_paths=AGENT_WINDOWS,
         labels=("ci:windows", "ci:full"), job="agent-windows", runner=WINDOWS, timeout=45),
    # Every root flake check: actionlint, workflow policy and contracts, CI script
    # tests, the release gates, and release metadata (read from the version files).
    Lane("repo", (
        ".github/**", "scripts/ci/**", ".githooks/**", "flake.nix", "flake.lock", "justfile", "setup-hooks.sh",
        ".agents/skills/release-maple/scripts/**", "services/agent-updates/publisher/**",
        "scripts/update-version.py", "apps/maple-research/zapstore.yaml", "proxy/Cargo.toml", "proxy/justfile",
        RESEARCH + "package.json", TAURI + "Cargo.toml", TAURI + "tauri.conf.json",
    ), runner=GITHUB, run="nix flake check --no-update-lock-file --print-build-logs", timeout=15),

    # Legacy lanes. Lanes that differ only by OS (agent-package, research-desktop)
    # are split when they move, and OpenSecret will route through
    # opensecret_change_detection.py.
    Lane("agent-full", AGENT, legacy="agent-ci.yml"),
    Lane("agent-supply-chain", tuple("apps/maple-agent/" + glob for glob in (
        "Cargo.lock", "**/Cargo.toml", "deny.toml", "about.toml", "flake.lock", "dependencies/**",
        "scripts/generate-third-party-notices.py", "scripts/audit-sqlcipher-dependencies.py",
    )), legacy="agent-sqlcipher-supply-chain.yml"),
    Lane("agent-provider-linux", tuple("apps/maple-agent/" + glob for glob in (
        "Cargo.lock", "dependencies/sqlcipher/**", "scripts/test-rusqlite-provider.py",
        "scripts/test_rusqlite_provider.py",
    )), legacy="agent-ci.yml"),
    Lane("agent-package", AGENT_PACKAGING, labels=("ci:package", "ci:full"), legacy="agent-desktop-build.yml"),
    Lane("agent-perf", (), labels=("ci:perf",), legacy="agent-store-performance.yml"),
    Lane("agent-updates", ("services/agent-updates/**", "services/download-tracking/**", "scripts/ci/agent-updates.sh",
                           "apps/maple-agent/release-profiles.json", "apps/maple-agent/scripts/release-info.py"),
         legacy="agent-updates-tests.yml"),
    Lane("sdk-ts", SDK_TS + ("sdk/.prettierrc.json", "sdk/eslint.config.js", "sdk/justfile", "sdk/.env.example",
                             "sdk/LICENSE.md", "scripts/ci/build-sdk-publish.sh",
                             "scripts/ci/check-sdk-package-consumers.py"),
         legacy="sdk-typescript.yml, sdk-publish-tests.yml"),
    Lane("sdk-rust", ("sdk/rust/**", "sdk/rust-toolchain.toml", "sdk/flake.*", "scripts/ci/build-sdk-publish.sh",
                      "scripts/ci/sdk_crates_upload.py"),
         legacy="sdk-rust.yml, sdk-publish-tests.yml"),
    Lane("proxy", ("proxy/**", "!proxy/Dockerfile"), also=RUST_SDK, legacy="proxy-rust.yml"),
    Lane("proxy-container", ("proxy/Dockerfile", ".dockerignore"), legacy="proxy-container.yml"),
    Lane("deps", ("apps/maple-research/deny.toml", TAURI + "Cargo.*", "sdk/deny.toml", "sdk/rust/Cargo.*",
                  "proxy/deny.toml", "proxy/Cargo.*", "services/opensecret/deny.toml",
                  "services/opensecret/Cargo.*"),
         legacy="supply-chain.yml, sdk-supply-chain.yml, proxy-supply-chain.yml, opensecret-ci.yml"),
    Lane("research-web", (RESEARCH + "**", "!" + TAURI + "**", "scripts/prepare-*.sh", "scripts/ci/frontend.sh",
                          "scripts/ci/web.sh", "scripts/ci/_common.sh", "flake.nix", "flake.lock"),
         legacy="frontend-tests.yml, web-build.yml"),
    Lane("research-rust", (TAURI + "**", "!" + TAURI + "gen/**", "scripts/ci/rust.sh",
                           "scripts/ci/verify-local-rust-deps.sh", "scripts/ci/_common.sh", "flake.nix", "flake.lock"),
         also=RUST_SDK + PROXY_RUNTIME + ("scripts/ci/verify-agent-rust-deps.py",), legacy="rust-tests.yml"),
    Lane("research-ios", (TAURI + "gen/apple/**", TAURI + "icons/ios/**", TAURI + "capabilities/mobile-ios.json",
                          TAURI + "tauri.ios-dev.conf.json", TAURI + "Info.plist", TAURI + "Entitlements.plist",
                          TAURI + "scripts/*ios*", TAURI + "scripts/canonicalize-static-archive.py",
                          TAURI + "scripts/onnxruntime-pins.sh", "scripts/ci/ios-*.sh",
                          "scripts/ci/apple-toolchain.json", "scripts/ci/select-xcode.py"),
         labels=("ci:ios", "ci:full"), legacy="mobile-pr-build.yml"),
    Lane("research-android", (TAURI + "gen/android/**", TAURI + "icons/android/**",
                              TAURI + "capabilities/mobile-android.json", TAURI + "scripts/*android*",
                              "scripts/ci/android-*.sh"),
         labels=("ci:android", "ci:full"), legacy="android-pr-build.yml"),
    Lane("research-desktop", (TAURI + "tauri*.conf.json", TAURI + "Info.plist", TAURI + "Entitlements.plist",
                              TAURI + "build.rs", TAURI + "resources/windows/**", TAURI + "dmg/**",
                              TAURI + "scripts/provide-*-onnxruntime.sh", "scripts/ci/desktop-*.sh",
                              "scripts/ci/_common.sh", "scripts/ci/tauri_bundle_type.py", "scripts/ci/canonical-*.py",
                              "scripts/ci/dmg-settings.py", "scripts/ci/verify-release-artifacts.sh",
                              "scripts/ci/attestation-manifest.sh", "scripts/ci/*.ps1"),
         labels=("ci:package", "ci:full"), legacy="desktop-pr-build.yml"),
    Lane("auth", ("apps/maple-auth/**", "scripts/ci/auth-*.sh", "scripts/ci/pages_auth_build.py",
                  "scripts/ci/pages_artifact.py", "flake.lock"),
         legacy="auth-pages-ci.yml"),
    Lane("updates", ("services/updates/**", "services/download-tracking/**", "scripts/ci/updates.sh"),
         legacy="updates-tests.yml"),
    Lane("os-rust", ("services/opensecret/**", ".gitmodules"), legacy="opensecret-ci.yml"),
    Lane("os-sdk-e2e", ("sdk/test/**", ".agents/skills/validate-opensecret/scripts/disposable_db_tests.sh"),
         also=RUST_SDK + SDK_TS, legacy="sdk-integration.yml"),
    Lane("os-eif", ("scripts/ci/build_opensecret_eif.sh", "scripts/ci/check_opensecret_eif.sh",
                    "scripts/ci/verify_opensecret_eif_rootfs.py", "scripts/ci/fixtures/eif/**"),
         legacy="opensecret-eif.yml"),
)
