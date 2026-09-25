#!/usr/bin/env python3
"""Classify changed repository paths for Maple's expensive app build jobs."""

from __future__ import annotations

import argparse
import sys
from collections.abc import Iterable


OUTPUTS = ("frontend", "macos", "linux", "windows", "ios", "android", "ios_onnx")
NATIVE_PLATFORMS = frozenset({"macos", "linux", "windows", "ios", "android"})
DESKTOP_PLATFORMS = frozenset({"macos", "linux", "windows"})
ALL_OUTPUTS = frozenset(OUTPUTS)

INERT_FILES = frozenset(
    {
        ".gitignore",
        ".gitmodules",
        ".dockerignore",
        ".repo_ignore",
        "AGENTS.md",
        "apps/maple-research/AGENTS.md",
        "apps/maple-research/README.md",
        "apps/maple-research/deny.toml",
        "justfile",
        "LICENSE",
        "README.md",
        "repo.meta.json",
        "setup-hooks.sh",
        "apps/maple-research/zapstore.yaml",
    }
)
INERT_PREFIXES = (".agents/", ".githooks/", "docs/", "apps/maple-research/docs/", "apps/maple-research/.githooks/", "services/updates/", "services/opensecret/", "apps/maple-agent/", "apps/maple-auth/")
PURE_FRONTEND_PREFIXES = ("apps/maple-research/frontend/public/", "apps/maple-research/frontend/src/")
PURE_FRONTEND_FILES = frozenset({"apps/maple-research/frontend/icon.svg", "apps/maple-research/frontend/index.html"})
PROXY_RUNTIME_PREFIXES = ("proxy/src/",)
PROXY_INERT_PREFIXES = ("proxy/tests/", "proxy/examples/", "proxy/.githooks/")
PROXY_INERT_FILES = frozenset(
    {
        "proxy/.env.example",
        "proxy/.gitignore",
        "proxy/Cargo.lock",
        "proxy/Dockerfile",
        "proxy/LICENSE",
        "proxy/README.md",
        "proxy/clippy.toml",
        "proxy/deny.toml",
        "proxy/docker-compose.yml",
        "proxy/flake.lock",
        "proxy/flake.nix",
        "proxy/justfile",
        "proxy/rust-toolchain.toml",
        "proxy/rustfmt.toml",
    }
)

IOS_ONNX_INPUTS = frozenset(
    {
        "flake.lock",
        "flake.nix",
        "scripts/ci/_common.sh",
        "scripts/ci/ios-onnxruntime.sh",
        "apps/maple-research/frontend/src-tauri/scripts/build-ios-onnxruntime-all.sh",
        "apps/maple-research/frontend/src-tauri/scripts/canonicalize-static-archive.py",
        "apps/maple-research/frontend/src-tauri/scripts/onnxruntime-pins.sh",
    }
)

WORKFLOW_ROUTES = {
    ".github/workflows/android-build.yml": frozenset({"android"}),
    ".github/workflows/android-pr-build.yml": frozenset({"android"}),
    ".github/workflows/app-change-detection.yml": NATIVE_PLATFORMS,
    ".github/workflows/desktop-build.yml": DESKTOP_PLATFORMS,
    ".github/workflows/desktop-pr-build.yml": DESKTOP_PLATFORMS,
    ".github/workflows/mobile-build.yml": frozenset({"ios", "ios_onnx"}),
    ".github/workflows/mobile-pr-build.yml": frozenset({"ios"}),
}

CI_SCRIPT_ROUTES = {
    "scripts/ci/_common.sh": frozenset({"frontend", *NATIVE_PLATFORMS, "ios_onnx"}),
    "scripts/ci/android-pr.sh": frozenset({"android"}),
    "scripts/ci/android-release.sh": frozenset({"android"}),
    "scripts/ci/attestation-manifest.sh": NATIVE_PLATFORMS,
    "scripts/ci/canonical-ios-app-hash.py": frozenset({"ios"}),
    "scripts/ci/canonical-windows-nsis-payload-hash.py": frozenset({"windows"}),
    "scripts/ci/desktop-pr.sh": DESKTOP_PLATFORMS,
    "scripts/ci/desktop-release.sh": DESKTOP_PLATFORMS,
    "scripts/ci/desktop-windows-pr.sh": frozenset({"windows"}),
    "scripts/ci/desktop-windows-release.sh": frozenset({"windows"}),
    "scripts/ci/frontend.sh": frozenset({"frontend"}),
    "scripts/ci/install-windows-artifact-signing.ps1": frozenset({"windows"}),
    "scripts/ci/install-windows-minisign.ps1": frozenset({"windows"}),
    "scripts/ci/ios-onnxruntime.sh": frozenset({"ios", "ios_onnx"}),
    "scripts/ci/ios-build-profile.py": frozenset({"ios"}),
    "scripts/ci/ios-pr.sh": frozenset({"ios"}),
    "scripts/ci/ios-release.sh": frozenset({"ios"}),
    "scripts/ci/ios-variant.sh": frozenset({"ios"}),
    "scripts/ci/verify-release-artifacts.sh": NATIVE_PLATFORMS,
    "scripts/ci/web.sh": frozenset({"frontend"}),
    "scripts/ci/windows-artifact-sign.ps1": frozenset({"windows"}),
}


def _native_path_routes(path: str) -> frozenset[str]:
    android_prefixes = (
        "apps/maple-research/frontend/src-tauri/gen/android/",
        "apps/maple-research/frontend/src-tauri/icons/android/",
        "apps/maple-research/frontend/src-tauri/capabilities/mobile-android.json",
    )
    ios_prefixes = (
        "apps/maple-research/frontend/src-tauri/gen/apple/",
        "apps/maple-research/frontend/src-tauri/icons/ios/",
        "apps/maple-research/frontend/src-tauri/onnxruntime-ios/",
        "apps/maple-research/frontend/src-tauri/capabilities/mobile-ios.json",
    )
    windows_prefixes = ("apps/maple-research/frontend/src-tauri/resources/windows/",)

    if path.startswith(android_prefixes):
        return frozenset({"android"})
    if path.startswith(ios_prefixes):
        return frozenset({"ios"})
    if path.startswith(windows_prefixes):
        return frozenset({"windows"})

    if path in {
        "apps/maple-research/frontend/src-tauri/Entitlements.plist",
        "apps/maple-research/frontend/src-tauri/Info.plist",
        "apps/maple-research/frontend/src-tauri/apple-sign-in-info.md",
    }:
        return frozenset({"macos", "ios"})
    if path == "apps/maple-research/frontend/src-tauri/tauri.macos.conf.json":
        return frozenset({"macos"})
    if path == "apps/maple-research/frontend/src-tauri/tauri.windows.conf.json":
        return frozenset({"windows"})
    if path == "apps/maple-research/frontend/src-tauri/tauri.ios-dev.conf.json":
        return frozenset({"ios"})

    if path.startswith("apps/maple-research/frontend/src-tauri/scripts/"):
        filename = path.rsplit("/", 1)[-1]
        if "android" in filename:
            return frozenset({"android"})
        if "ios" in filename or "apple" in filename or filename == "canonicalize-static-archive.py":
            routes = {"ios"}
            if path in IOS_ONNX_INPUTS:
                routes.add("ios_onnx")
            return frozenset(routes)
        if "windows" in filename:
            return frozenset({"windows"})
        if "linux" in filename:
            return frozenset({"linux"})
        if "macos" in filename:
            return frozenset({"macos"})
        if filename == "run-with-desktop-onnxruntime.sh":
            return DESKTOP_PLATFORMS
        if filename == "onnxruntime-pins.sh":
            return frozenset({*NATIVE_PLATFORMS, "ios_onnx"})

    # Unknown files inside the native application remain fail-safe for every target.
    return NATIVE_PLATFORMS


def classify_path(path: str) -> frozenset[str]:
    """Return the app build lanes affected by one repository-relative path."""

    if not path or path.startswith("/") or ".." in path.split("/"):
        return ALL_OUTPUTS
    if path in INERT_FILES or path.startswith(INERT_PREFIXES):
        return frozenset()
    if path.startswith("sdk/"):
        # Apps build whatever SDK source their own manifests select. Changing
        # that manifest (a pin bump or a local link) selects the app's lanes.
        return frozenset()
    if path == "proxy/Cargo.toml" or path.startswith(PROXY_RUNTIME_PREFIXES):
        return DESKTOP_PLATFORMS
    if path in PROXY_INERT_FILES or path.startswith(PROXY_INERT_PREFIXES):
        return frozenset()
    if path.startswith("proxy/"):
        # Unknown files in the consumed proxy crate may be build inputs.
        return DESKTOP_PLATFORMS
    if path in PURE_FRONTEND_FILES or path.startswith(PURE_FRONTEND_PREFIXES):
        return frozenset({"frontend"})
    if path.startswith("apps/maple-research/frontend/src-tauri/"):
        return _native_path_routes(path)
    if path.startswith("apps/maple-research/frontend/"):
        # Dependency and build-configuration changes can affect every packaged app.
        return frozenset({"frontend", *NATIVE_PLATFORMS})
    if path in {"flake.lock", "flake.nix"}:
        return ALL_OUTPUTS
    if path in WORKFLOW_ROUTES:
        return WORKFLOW_ROUTES[path]
    if path.startswith(".github/workflows/"):
        return frozenset()
    if path in CI_SCRIPT_ROUTES:
        return CI_SCRIPT_ROUTES[path]
    if path.startswith("scripts/ci/"):
        return frozenset()

    # A new, unclassified root is potentially an application build input.
    return ALL_OUTPUTS


def classify_paths(paths: Iterable[str]) -> dict[str, bool]:
    routes: set[str] = set()
    for path in paths:
        routes.update(classify_path(path))
    return {name: name in routes for name in OUTPUTS}


def _read_null_delimited_paths() -> list[str]:
    raw_paths = sys.stdin.buffer.read().split(b"\0")
    return [path.decode("utf-8", errors="surrogateescape") for path in raw_paths if path]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--all",
        action="store_true",
        help="Conservatively enable every output without reading paths",
    )
    args = parser.parse_args()

    result = {name: True for name in OUTPUTS} if args.all else classify_paths(_read_null_delimited_paths())
    for name in OUTPUTS:
        print(f"{name}={'true' if result[name] else 'false'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
