#!/usr/bin/env python3
"""Fixed local Research Dev packaging inputs and macOS artifact checks."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import plistlib
import shlex

ROOT = Path(__file__).resolve().parents[2]
TAURI = ROOT / "apps/maple-research/frontend/src-tauri"
PROFILE = TAURI / "desktop-dev-profile.json"


def validate_config(profile, overlay):
    if overlay["identifier"] != profile["identifier"] or overlay["productName"] != profile["productName"]:
        raise ValueError("Dev package identity does not match its profile")
    if overlay["plugins"]["deep-link"]["desktop"]["schemes"] != [profile["scheme"]]:
        raise ValueError("Dev package must own only its Dev scheme")
    if overlay["plugins"]["updater"]["endpoints"] != [] or overlay["bundle"]["createUpdaterArtifacts"] is not False:
        raise ValueError("Dev package must not consume or produce updater artifacts")
    permissions = overlay["app"]["security"]["capabilities"]
    if any(not isinstance(cap, dict) for cap in permissions) or "$HOME" in json.dumps(permissions):
        raise ValueError("Dev capabilities must not inherit legacy production storage access")


def verify_bundle(bundle, profile):
    with (bundle / "Contents/Info.plist").open("rb") as stream:
        info = plistlib.load(stream)
    if info["CFBundleIdentifier"] != profile["identifier"]:
        raise ValueError("Packaged bundle has the wrong identity")
    if info["CFBundleName"] != profile["productName"]:
        raise ValueError("Packaged bundle has the wrong name")
    schemes = [scheme for entry in info.get("CFBundleURLTypes", [])
               for scheme in entry.get("CFBundleURLSchemes", [])]
    if schemes != [profile["scheme"]]:
        raise ValueError("Packaged bundle has the wrong callback schemes")
    binary = bundle / "Contents/MacOS" / info["CFBundleExecutable"]
    return {"bundle_identifier": info["CFBundleIdentifier"], "scheme": schemes[0],
            "executable_sha256": hashlib.sha256(binary.read_bytes()).hexdigest()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["environment", "verify"])
    parser.add_argument("--bundle", type=Path)
    args = parser.parse_args()
    profile = json.loads(PROFILE.read_text())
    validate_config(profile, json.loads((TAURI / "tauri.desktop-dev.conf.json").read_text()))
    if args.command == "environment":
        for key, value in profile["environment"].items():
            print(f"export {key}={shlex.quote(value)}")
    else:
        if args.bundle is None:
            parser.error("verify requires --bundle")
        if any(os.environ.get(key) != value for key, value in profile["environment"].items()):
            raise ValueError("Build environment drifted from the fixed Dev profile")
        evidence = verify_bundle(args.bundle, profile)
        print(json.dumps({"profile": profile, "artifact": evidence,
                          "frontend_sha256": os.environ["MAPLE_FRONTEND_DIST_TREE_SHA256"]}, indent=2))


if __name__ == "__main__":
    main()
