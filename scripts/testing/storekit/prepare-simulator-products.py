#!/usr/bin/env python3
"""Prepare both already-built Maple simulator bundles for the loopback fixture."""

import argparse
import json
import os
from pathlib import Path
import plistlib
import stat
import subprocess
import tempfile


ROOT = Path(__file__).resolve().parents[3]
TAURI = ROOT / "apps/maple-research/frontend/src-tauri"
PROJECT = TAURI / "gen/apple/maple.xcodeproj"


def simulator_products():
    output = subprocess.check_output([
        "xcodebuild", "-showBuildSettings", "-json", "-project", str(PROJECT),
        "-scheme", "maple_iOS", "-configuration", "debug", "-sdk", "iphonesimulator",
    ], text=True)
    targets = [entry["buildSettings"] for entry in json.loads(output) if entry.get("target") == "maple_iOS"]
    if len(targets) != 1:
        raise ValueError("Expected exactly one maple_iOS build-settings entry")
    settings = targets[0]
    if (Path(settings["PROJECT_FILE_PATH"]).resolve() != PROJECT.resolve()
            or settings["CONFIGURATION"] != "debug"
            or settings["PLATFORM_NAME"] != "iphonesimulator"
            or settings["FULL_PRODUCT_NAME"] != "Maple.app"):
        raise ValueError("Xcode did not resolve this project's debug iOS simulator product")
    paths = [Path(settings["TARGET_BUILD_DIR"]) / settings["FULL_PRODUCT_NAME"],
             TAURI / "gen/apple/build/arm64-sim/Maple.app"]
    return list(dict.fromkeys(path.resolve(strict=True) for path in paths))


def validate(app):
    if not app.is_dir() or app.suffix != ".app":
        raise ValueError(f"Expected a built application bundle: {app}")
    path = app / "Info.plist"
    raw = path.read_bytes()
    info = plistlib.loads(raw)
    if info.get("DTPlatformName") != "iphonesimulator" or info.get("CFBundleIdentifier") != "cloud.opensecret.maple":
        raise ValueError(f"Refusing to patch a non-Maple or non-simulator bundle: {app}")
    executable = info.get("CFBundleExecutable")
    if not isinstance(executable, str) or Path(executable).name != executable or not (app / executable).is_file():
        raise ValueError(f"Missing built simulator executable: {app}")
    if path.is_symlink():
        raise ValueError(f"Refusing to replace a symlinked bundle Info.plist: {path}")
    transport = info.setdefault("NSAppTransportSecurity", {})
    if not isinstance(transport, dict):
        raise ValueError(f"Unexpected NSAppTransportSecurity format: {path}")
    transport["NSAllowsLocalNetworking"] = True
    return path, raw, info


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check-only", action="store_true", help="Resolve and validate existing products without changing them")
    args = parser.parse_args()
    # Validate every product before changing either one.
    products = [(app, validate(app)) for app in simulator_products()]
    for app, (path, raw, info) in products:
        if not args.check_only:
            data = plistlib.dumps(info, fmt=plistlib.FMT_BINARY if raw.startswith(b"bplist") else plistlib.FMT_XML, sort_keys=False)
            with tempfile.NamedTemporaryFile(dir=app, prefix=".storekit-info-", delete=False) as temporary:
                temporary.write(data)
                temporary_path = Path(temporary.name)
            try:
                temporary_path.chmod(stat.S_IMODE(path.stat().st_mode))
                os.replace(temporary_path, path)
            finally:
                temporary_path.unlink(missing_ok=True)
            subprocess.run(["codesign", "--force", "--sign", "-", "--timestamp=none",
                            "--preserve-metadata=identifier,entitlements,flags,runtime", str(app)], check=True)
            subprocess.run(["codesign", "--verify", "--strict", str(app)], check=True)
        print(f"{'Validated' if args.check_only else 'Prepared'} StoreKit simulator product: {app}")


if __name__ == "__main__":
    main()
