#!/usr/bin/env python3
"""Select Agent CI without rebuilding either application for the other's changes."""

from __future__ import annotations

import argparse
from collections.abc import Iterable
import sys

from change_detection import DESKTOP_PLATFORMS, classify_path as research_routes


AGENT_PREFIX = "apps/maple-agent/"
AGENT_INERT_FILES = frozenset(
    {
        "AGENTS.md",
        "CLAUDE.md",
        "README.md",
        "LICENSE",
        ".gitignore",
        "deny.toml",  # Validated by the independent supply-chain lane.
    }
)
AGENT_INERT_PREFIXES = ("docs/",)
SHARED_INPUTS = frozenset(
    {
        "flake.nix",
        "flake.lock",
        ".github/workflows/agent-ci.yml",
        "scripts/ci/agent_change_detection.py",
        "scripts/ci/change_detection.py",
        "scripts/ci/verify-agent-rust-deps.py",
    }
)
KNOWN_INDEPENDENT_PREFIXES = (
    "apps/maple-research/",
    ".agents/",
    ".github/",
    ".githooks/",
    "docs/",
    "scripts/",
    "services/",
)
KNOWN_INDEPENDENT_FILES = frozenset(
    {"AGENTS.md", "README.md", "LICENSE", ".gitignore", ".gitmodules", ".dockerignore",
     ".repo_ignore", "repo.meta.json", "justfile", "setup-hooks.sh"}
)


def affects_agent(path: str) -> bool:
    if not path or path.startswith("/") or ".." in path.split("/"):
        return True
    if path in SHARED_INPUTS:
        return True
    if path.startswith(AGENT_PREFIX):
        relative = path.removeprefix(AGENT_PREFIX)
        return relative not in AGENT_INERT_FILES and not relative.startswith(AGENT_INERT_PREFIXES)
    if path.startswith(("sdk/rust/", "proxy/")):
        # Both desktop apps consume these same local crates. Reuse the existing
        # distinction between runtime/build inputs and standalone docs/tests/locks.
        return bool(research_routes(path) & DESKTOP_PLATFORMS)
    if path.startswith("sdk/"):
        return False
    if path in KNOWN_INDEPENDENT_FILES or path.startswith(KNOWN_INDEPENDENT_PREFIXES):
        return False
    # A newly introduced root may be a build input; don't silently skip it.
    return True


def classify_paths(paths: Iterable[str]) -> bool:
    return any(affects_agent(path) for path in paths)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--all", action="store_true", help="Select Agent when a diff is unavailable")
    args = parser.parse_args()
    paths = (
        path.decode("utf-8", errors="surrogateescape")
        for path in sys.stdin.buffer.read().split(b"\0") if path
    ) if not args.all else ()
    print(f"agent={'true' if args.all or classify_paths(paths) else 'false'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
