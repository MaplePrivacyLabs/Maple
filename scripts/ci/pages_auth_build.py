#!/usr/bin/env python3
"""Offline auth artifact validation and the production SDK dependency gate."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import re
import sys
import tempfile

sys.path.insert(0, str(Path(__file__).resolve().parent))
from pages_artifact import ArtifactError, extract_static, pack_manifest


def check_sdk_pin(frontend: Path, *, installed: bool = False) -> str:
    manifest = json.loads((frontend / "package.json").read_text())
    version = manifest["dependencies"]["@mapleai/sdk"]
    if not isinstance(version, str) or not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", version):
        raise ValueError("Production auth requires an exact published SDK version; local links are development-only")
    if tuple(map(int, version.split("."))) < (4, 1, 0):
        raise ValueError("Production auth requires SDK callback-selection support")
    for field in ("overrides", "resolutions"):
        if any("@mapleai/sdk" in name for name in manifest.get(field, {})):
            raise ValueError("Production auth cannot override the pinned SDK source")
    if installed:
        modules = (frontend / "node_modules").resolve()
        sdk = frontend / "node_modules/@mapleai/sdk"
        if not sdk.resolve().is_relative_to(modules):
            raise ValueError("Production auth SDK resolves outside the frozen dependency installation")
        package = json.loads((sdk / "package.json").read_text())
        if package.get("name") != "@mapleai/sdk" or package.get("version") != version:
            raise ValueError("Installed SDK does not match the production pin")
    return version


def check_archive(archive: Path) -> None:
    # Reuse the exact extraction boundary used by the credential-bearing publisher.
    digest = pack_manifest(archive, "auth-release", "0" * 40, 1, 1)["archive_sha256"]
    with tempfile.TemporaryDirectory(prefix="maple-auth-static-check-") as directory:
        files = extract_static(archive, Path(directory) / "assets", digest)
    print(f"Auth artifact contains {len(files)} validated static files.")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    sdk = commands.add_parser("sdk-pin")
    sdk.add_argument("--frontend", type=Path, required=True)
    sdk.add_argument("--installed", action="store_true")
    artifact = commands.add_parser("artifact")
    artifact.add_argument("--archive", type=Path, required=True)
    args = parser.parse_args()
    try:
        if args.command == "sdk-pin":
            check_sdk_pin(args.frontend, installed=args.installed)
        else:
            check_archive(args.archive)
    except (ArtifactError, ValueError, KeyError, TypeError, OSError):
        print("Auth build validation failed: require static assets and an exact published SDK pin for production.",
              file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
