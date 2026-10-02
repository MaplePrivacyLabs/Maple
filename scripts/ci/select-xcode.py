#!/usr/bin/env python3
"""Select the exact release Xcode used by Maple's Apple builds and caches."""

import json
import os
from pathlib import Path
import re
import subprocess
import sys


PIN = Path(__file__).with_name("apple-toolchain.json")


class ToolchainError(ValueError):
    pass


def load_pin(path=PIN):
    pin = json.loads(path.read_text())
    if (not isinstance(pin, dict) or set(pin) != {"version", "build"}
            or not isinstance(pin["version"], str)
            or not re.fullmatch(r"\d+\.\d+(?:\.\d+)?", pin["version"])
            or not isinstance(pin["build"], str)
            or not re.fullmatch(r"[A-Za-z0-9]+", pin["build"])):
        raise ToolchainError("apple-toolchain.json requires an Xcode version and release build")
    return pin


def select_xcode(pin, applications=Path("/Applications")):
    version = pin["version"]
    for name in (f"Xcode_{version}.app", f"Xcode_{version}.0.app", "Xcode.app"):
        developer = applications / name / "Contents/Developer"
        if not (developer / "usr/bin/xcodebuild").is_file():
            continue
        # A versioned symlink must not quietly reintroduce a beta fallback.
        developer = developer.resolve()
        if "beta" in str(developer).lower():
            continue
        result = subprocess.run(
            ["/usr/bin/xcodebuild", "-version"],
            env={**os.environ, "DEVELOPER_DIR": str(developer)},
            capture_output=True, text=True, check=False,
        )
        actual_version = re.search(r"^Xcode (\S+)$", result.stdout, re.MULTILINE)
        actual_build = re.search(r"^Build version (\S+)$", result.stdout, re.MULTILINE)
        if (result.returncode == 0 and actual_version and actual_build
                and actual_version[1] in (version, f"{version}.0")
                and actual_build[1] == pin["build"]):
            return developer
    raise ToolchainError(f"Release Xcode {version} build {pin['build']} is not installed")


def publish_selection(developer, pin, environment):
    # Use a job-scoped developer directory instead of changing the runner's
    # global xcode-select state. Nix and the native build helpers inherit it.
    with Path(environment["GITHUB_ENV"]).open("a") as stream:
        stream.write(f"DEVELOPER_DIR={developer}\n")
    with Path(environment["GITHUB_OUTPUT"]).open("a") as stream:
        stream.write(f"version={pin['version']}\nbuild={pin['build']}\n")


def main():
    try:
        pin = load_pin()
        developer = select_xcode(pin)
        publish_selection(developer, pin, os.environ)
    except (ToolchainError, OSError, KeyError, json.JSONDecodeError) as error:
        print(f"Xcode selection failed: {error}", file=sys.stderr)
        return 1
    print(f"Using Xcode {pin['version']} build {pin['build']} from {developer}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
