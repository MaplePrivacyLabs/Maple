#!/usr/bin/env python3
"""Select the component pre-commit checks that staged paths require.

The hook mirrors the lint/test lanes of the GitHub workflows, not the packaging
lanes. Each component is selected only when its own files are staged; shared
crates do not fan out to their consumers here (CI still covers consumers).
"""

from __future__ import annotations

import argparse
import sys
from collections.abc import Iterable


OUTPUTS = (
    "research_frontend",
    "research_rust",
    "agent",
    "sdk_rust",
    "sdk_ts",
    "proxy",
    "opensecret",
    "updates",
    "repo",
)
ALL_OUTPUTS = frozenset(OUTPUTS)

# Documentation and hook/skill metadata never select a check.
INERT_SUFFIXES = (".md", ".txt", "LICENSE", "AGENTS.md", "CLAUDE.md", ".gitignore")
INERT_ROOT_FILES = frozenset(
    {
        ".dockerignore",
        ".gitmodules",
        ".repo_ignore",
        "justfile",
        "repo.meta.json",
        "setup-hooks.sh",
    }
)
INERT_ROOT_PREFIXES = (".agents/", ".githooks/", "docs/")

FRONTEND_PREFIX = "apps/maple-research/frontend/"
TAURI_PREFIX = "apps/maple-research/frontend/src-tauri/"
RESEARCH_INERT_PREFIXES = ("apps/maple-research/docs/", "apps/maple-research/.githooks/")
RESEARCH_INERT_FILES = frozenset({"apps/maple-research/deny.toml", "apps/maple-research/zapstore.yaml"})

AGENT_PREFIX = "apps/maple-agent/"
AGENT_INERT_PREFIXES = ("docs/", ".githooks/")

SDK_TS_PREFIXES = ("sdk/src/",)
SDK_TS_FILES = frozenset(
    {
        "sdk/.npmrc",
        "sdk/.prettierrc.json",
        "sdk/bun.lock",
        "sdk/bunfig.toml",
        "sdk/eslint.config.js",
        "sdk/package.json",
        "sdk/tsconfig.build.json",
        "sdk/tsconfig.json",
        "sdk/vite.config.ts",
    }
)
SDK_RUST_PREFIX = "sdk/rust/"
SDK_SHARED_FILES = frozenset({"sdk/flake.nix", "sdk/flake.lock", "sdk/rust-toolchain.toml"})
SDK_INERT_PREFIXES = ("sdk/docs/", "sdk/test/", "sdk/.githooks/")
SDK_INERT_FILES = frozenset({"sdk/deny.toml", "sdk/justfile"})

PROXY_PREFIX = "proxy/"
PROXY_INERT_PREFIXES = ("proxy/.githooks/",)
PROXY_INERT_FILES = frozenset({"proxy/deny.toml", "proxy/Dockerfile", "proxy/docker-compose.yml", "proxy/justfile"})

BACKEND_PREFIX = "services/opensecret/"
BACKEND_INERT_PREFIXES = ("docs/", ".agents/", ".github/", ".githooks/", "secretspec/", "nix/", "nitro-toolkit/", "privatemode-public/")
BACKEND_INERT_FILES = frozenset(
    {
        "deny.toml",
        "justfile",
        "secretspec.toml",
        "entrypoint.sh",
        "continuum-proxy",
        "continuum-proxy-x86_64",
        "pcrDev.json",
        "pcrDevHistory.json",
        "pcrProd.json",
        "pcrProdHistory.json",
        "pcrPreview.json",
        "pcrPreviewHistory.json",
        "pcr_sign.js",
        "pcr_verify.js",
    }
)

UPDATES_PREFIX = "services/updates/"

REPO_PREFIXES = (".github/", "scripts/ci/")
REPO_FILES = frozenset({"flake.nix", "flake.lock"})


def _inert_name(path: str) -> bool:
    name = path.rsplit("/", 1)[-1]
    return name.endswith(INERT_SUFFIXES) or name in {"README", "LICENSE"}


def classify_path(path: str) -> frozenset[str]:
    """Return the hook components selected by one repository-relative path."""

    if not path or path.startswith("/") or ".." in path.split("/"):
        return ALL_OUTPUTS
    if path in INERT_ROOT_FILES or path.startswith(INERT_ROOT_PREFIXES):
        return frozenset()
    if _inert_name(path):
        return frozenset()

    if path in RESEARCH_INERT_FILES or path.startswith(RESEARCH_INERT_PREFIXES):
        return frozenset()
    if path.startswith(TAURI_PREFIX):
        return frozenset({"research_rust"})
    if path.startswith(FRONTEND_PREFIX):
        return frozenset({"research_frontend"})
    if path.startswith("apps/maple-research/"):
        return frozenset({"research_frontend", "research_rust"})

    if path.startswith(AGENT_PREFIX):
        relative = path.removeprefix(AGENT_PREFIX)
        if relative.startswith(AGENT_INERT_PREFIXES):
            return frozenset()
        return frozenset({"agent"})

    if path in SDK_SHARED_FILES:
        return frozenset({"sdk_rust", "sdk_ts"})
    if path in SDK_INERT_FILES or path.startswith(SDK_INERT_PREFIXES):
        return frozenset()
    if path.startswith(SDK_RUST_PREFIX):
        return frozenset({"sdk_rust"})
    if path in SDK_TS_FILES or path.startswith(SDK_TS_PREFIXES):
        return frozenset({"sdk_ts"})
    if path.startswith("sdk/"):
        return frozenset({"sdk_rust", "sdk_ts"})

    if path in PROXY_INERT_FILES or path.startswith(PROXY_INERT_PREFIXES):
        return frozenset()
    if path.startswith(PROXY_PREFIX):
        return frozenset({"proxy"})

    if path.startswith(BACKEND_PREFIX):
        relative = path.removeprefix(BACKEND_PREFIX)
        if relative in BACKEND_INERT_FILES or relative.startswith(BACKEND_INERT_PREFIXES):
            return frozenset()
        return frozenset({"opensecret"})

    if path.startswith(UPDATES_PREFIX + ".githooks/"):
        return frozenset()
    if path.startswith(UPDATES_PREFIX):
        return frozenset({"updates"})

    if path in REPO_FILES or path.startswith(REPO_PREFIXES):
        return frozenset({"repo"})
    if path.startswith("scripts/"):
        return frozenset()

    # A new, unclassified root gets every check until it is routed explicitly.
    return ALL_OUTPUTS


def classify_paths(paths: Iterable[str]) -> dict[str, bool]:
    selected: set[str] = set()
    for path in paths:
        selected.update(classify_path(path))
    return {name: name in selected for name in OUTPUTS}


def _read_null_delimited_paths() -> list[str]:
    raw_paths = sys.stdin.buffer.read().split(b"\0")
    return [path.decode("utf-8", errors="surrogateescape") for path in raw_paths if path]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--all", action="store_true", help="Select every component without reading paths")
    parser.add_argument("--selected", action="store_true", help="Print only the selected component names, one per line")
    args = parser.parse_args()

    result = {name: True for name in OUTPUTS} if args.all else classify_paths(_read_null_delimited_paths())
    for name in OUTPUTS:
        if args.selected:
            if result[name]:
                print(name)
        else:
            print(f"{name}={'true' if result[name] else 'false'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
