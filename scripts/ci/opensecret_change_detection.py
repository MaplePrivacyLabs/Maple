#!/usr/bin/env python3
"""Select backend checks and SDK compatibility without rebuilding desktop apps."""

from __future__ import annotations

import argparse
from collections.abc import Iterable
import sys


CHECK_OUTPUTS = ("rust", "nix", "integration", "audit", "eif")
OUTPUTS = (*CHECK_OUTPUTS, "pcr_approvals")
ALL_CHECKS = frozenset(CHECK_OUTPUTS)
BACKEND_PREFIX = "services/opensecret/"
BACKEND_INERT_FILES = frozenset({
    "AGENTS.md", "README.md", "LICENSE", ".gitignore", ".gitmodules",
})
APPROVED_PCR_FILES = frozenset({
    "pcrDev.json", "pcrDevHistory.json", "pcrProd.json", "pcrProdHistory.json",
})
PCR_INPUTS = frozenset({
    "pcrPreview.json", "pcrPreviewHistory.json", "pcr_verify.js", "pcr_sign.js",
    "scripts/pcr_compatibility.py", "scripts/test_pcr_compatibility.py",
})
EIF_CI_INPUTS = frozenset({
    ".github/workflows/opensecret-eif.yml",
    "scripts/ci/check_opensecret_eif.sh",
})
# Shell test inputs consumed directly by the component flake, not Cargo.
NIX_TEST_INPUTS = frozenset({"tests/entrypoint_entropy_preflight.sh"})
BACKEND_INERT_PREFIXES = ("docs/", ".agents/", ".github/")
SHARED_INPUTS = frozenset({
    ".gitmodules",
    ".github/workflows/opensecret-change-detection.yml",
    "scripts/ci/opensecret_change_detection.py",
})
SDK_INTEGRATION_FILES = frozenset({
    "sdk/.npmrc", "sdk/bun.lock", "sdk/bunfig.toml", "sdk/package.json",
    "sdk/rust-toolchain.toml", "sdk/flake.nix", "sdk/flake.lock",
    ".github/workflows/sdk-integration.yml",
})
KNOWN_INDEPENDENT_PREFIXES = (
    "apps/", "proxy/", "services/updates/", ".agents/", ".github/",
    ".githooks/", "docs/", "scripts/",
)
KNOWN_INDEPENDENT_FILES = frozenset({
    "AGENTS.md", "README.md", "LICENSE", ".gitignore", ".dockerignore",
    ".repo_ignore", "repo.meta.json", "justfile", "setup-hooks.sh",
    # The backend and SDK have their own pinned flakes.
    "flake.nix", "flake.lock",
})


def classify_path(path: str) -> frozenset[str]:
    if not path or path.startswith("/") or ".." in path.split("/"):
        return ALL_CHECKS
    if path in SHARED_INPUTS:
        return ALL_CHECKS
    if path in EIF_CI_INPUTS:
        return frozenset({"eif"})
    if path == ".github/workflows/opensecret-ci.yml":
        return frozenset({"rust", "nix", "audit", "eif"})
    if path in SDK_INTEGRATION_FILES or path.startswith(("sdk/src/", "sdk/rust/", "sdk/test/")):
        return frozenset({"integration"})
    if path.startswith(BACKEND_PREFIX):
        relative = path.removeprefix(BACKEND_PREFIX)
        if relative in BACKEND_INERT_FILES or relative.startswith(BACKEND_INERT_PREFIXES):
            return frozenset()
        if relative in APPROVED_PCR_FILES:
            return frozenset({"eif", "pcr_approvals"})
        if relative in PCR_INPUTS:
            # Offline PCR tooling does not require backend builds or a dev shell.
            return frozenset()
        if relative == "deny.toml":
            return frozenset({"audit"})
        if relative in {"Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "flake.nix", "flake.lock"}:
            return ALL_CHECKS
        if relative.startswith(("src/", ".cargo/")) or relative == "build.rs":
            return frozenset({"rust", "nix", "integration", "eif"})
        if relative in NIX_TEST_INPUTS:
            return frozenset({"nix"})
        if relative == "secretspec.toml" or relative.startswith("secretspec/"):
            # Provider manifests are read only by the operator-boundary fixtures
            # in the component flake check; they are not build or runtime inputs.
            return frozenset({"nix"})
        if relative.startswith(("tests/", "migrations/")):
            return frozenset({"rust", "integration"})
        if relative == ".env.sample":
            return frozenset({"integration"})
        if relative.startswith(("nix/", "nitro-toolkit/", "privatemode-public/")) or relative in {
            "entrypoint.sh", "continuum-proxy", "nitro-toolkit", "privatemode-public",
        }:
            return frozenset({"nix", "eif"})
        # Unknown backend files could be build or runtime inputs.
        return ALL_CHECKS
    if path.startswith("sdk/") or path in KNOWN_INDEPENDENT_FILES or path.startswith(KNOWN_INDEPENDENT_PREFIXES):
        return frozenset()
    # New roots must be classified explicitly before checks can be skipped.
    return ALL_CHECKS


def classify_paths(paths: Iterable[str]) -> dict[str, bool]:
    selected: set[str] = set()
    for path in paths:
        selected.update(classify_path(path))
    return {name: name in selected for name in OUTPUTS}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--all", action="store_true", help="Select all checks without claiming approvals changed")
    args = parser.parse_args()
    if args.all:
        result = {name: name in ALL_CHECKS for name in OUTPUTS}
    else:
        paths = (path.decode("utf-8", errors="surrogateescape")
                 for path in sys.stdin.buffer.read().split(b"\0") if path)
        result = classify_paths(paths)
    for name in OUTPUTS:
        print(f"{name}={'true' if result[name] else 'false'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
