#!/usr/bin/env python3
"""Validate public release identity and create package metadata, never secrets."""

import argparse
import base64
import hashlib
import json
from pathlib import Path
import plistlib
import re
import sys
import tarfile


COMPONENT = Path(__file__).resolve().parent.parent
PUBLIC_FIELDS = (
    "display_name", "bundle_id", "data_namespace", "api_url", "billing_api_url", "web_url", "auth_origin",
    "client_id", "pcr_environment", "update_tag_prefix", "prerelease",
    "update_feed_url", "update_public_keys",
)
BUILD_FIELDS = set(PUBLIC_FIELDS) | {"profile", "version", "git_revision", "source_sha"}
WINDOWS_PLATFORMS = ("windows-x86_64", "windows-aarch64")
PLATFORMS = ("macos-aarch64", "linux-x86_64", *WINDOWS_PLATFORMS)


def read_info(profile, path, source_sha=None, platform=None):
    return validate_info(profile, json.loads(Path(path).read_text(encoding="utf-8")), source_sha, platform)


def validate_update_keys(keys):
    if not isinstance(keys, list) or len(keys) not in (0, 2):
        raise ValueError("update trust must contain zero or two distinct public keys")
    public_points = set()
    for key in keys:
        if not isinstance(key, str) or len(key) > 1024:
            raise ValueError("invalid public update key")
        # Comments and key IDs do not make a duplicate public key a backup.
        lines = key.strip().splitlines()
        packet = lines[-1] if lines else ""
        try:
            decoded = base64.b64decode(packet, validate=True)
        except ValueError as error:
            raise ValueError("invalid public update key encoding") from error
        if len(decoded) != 42 or decoded[:2] != b"Ed":
            raise ValueError("invalid public update key packet")
        if decoded[10:] in public_points:
            raise ValueError("update trust requires distinct public keys")
        public_points.add(decoded[10:])


def validate_info(profile, info, source_sha=None, platform=None):
    profiles = json.loads((COMPONENT / "release-profiles.json").read_text())
    if profile not in ("dev", "prod"):
        raise ValueError("release profile must be dev or prod")
    if platform not in (None, *PLATFORMS):
        raise ValueError("unsupported artifact platform")
    if not isinstance(info, dict) or set(info) != BUILD_FIELDS:
        raise ValueError("build-info must contain only the documented public release fields")
    if info["profile"] != profile:
        raise ValueError("binary release profile does not match the requested package")
    for field in PUBLIC_FIELDS:
        if field == "update_feed_url" and platform == "windows-aarch64":
            expected = profiles[profile]["windows_arm64_update_feed_url"]
        elif field == "update_feed_url" and platform == "windows-x86_64":
            expected = profiles[profile]["windows_update_feed_url"]
        elif field == "update_feed_url" and platform == "linux-x86_64":
            expected = profiles[profile]["linux_update_feed_url"]
        else:
            expected = profiles[profile][field]
        if info[field] != expected:
            raise ValueError(f"binary {field} does not match the checked-in release profile")
    if not isinstance(info["prerelease"], bool):
        raise ValueError("binary prerelease marker must be a boolean")
    if info["auth_origin"] is not None and not isinstance(info["auth_origin"], str):
        raise ValueError("public auth origin must be a string or null")
    if info["update_feed_url"] is not None and not isinstance(info["update_feed_url"], str):
        raise ValueError("public update feed must be a string or null")
    keys = info["update_public_keys"]
    validate_update_keys(keys)
    if profile == "dev" and (info["update_feed_url"] is not None or keys):
        raise ValueError("Dev builds cannot enable the production updater")
    if any(not isinstance(info[field], str) for field in BUILD_FIELDS - {
        "prerelease", "auth_origin", "update_feed_url", "update_public_keys",
    }):
        raise ValueError("public build identity fields must be strings")
    if not re.fullmatch(r"[0-9a-f]{40}", info["source_sha"]):
        raise ValueError("release binary must record a full source commit")
    if source_sha and info["source_sha"] != source_sha:
        raise ValueError("binary source commit does not match this checkout")
    revision = info["git_revision"]
    if not re.fullmatch(r"[0-9a-f]{7,40}", revision) or not info["source_sha"].startswith(revision):
        raise ValueError("release binary revision must be a clean abbreviation of its source commit")
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?", info["version"]):
        raise ValueError("invalid release version")
    return info


def write_plist(info, path, build_number):
    if not re.fullmatch(r"[1-9][0-9]*", build_number):
        raise ValueError("package build number must be a positive integer")
    plist = plistlib.loads((COMPONENT / "app/macos/Info.plist").read_bytes())
    plist.update({
        # Finder localizes only when this base name matches the bundle path.
        "CFBundleDisplayName": macos_bundle_name(info["profile"]),
        "CFBundleName": info["display_name"],
        "CFBundleIdentifier": info["bundle_id"],
        "CFBundleShortVersionString": info["version"].split("-", 1)[0],
        "CFBundleVersion": build_number,
        "CFBundleIconFile": "MapleAgent.icns",
        # The native CUA recorder uses SCRecordingOutput (macOS 15+). Do not
        # promise older OS support based only on the prototype's plist.
        "LSMinimumSystemVersion": "15.0",
        "NSMicrophoneUsageDescription": "Maple records your microphone when you choose to dictate a message.",
    })
    plist.pop("LSEnvironment", None)
    Path(path).write_bytes(plistlib.dumps(plist))
    localized = Path(path).parent / "Resources/en.lproj/InfoPlist.strings"
    localized.parent.mkdir(parents=True, exist_ok=True)
    localized.write_bytes(plistlib.dumps({
        "CFBundleDisplayName": info["display_name"],
        "CFBundleName": info["display_name"],
    }))


def macos_bundle_name(profile):
    # Installed paths and updater archive roots predate the display branding.
    return {"dev": "Maple Agent Dev", "prod": "Maple Agent"}[profile]


def linux_package_name(info):
    return f"maple-agent-{info['profile']}-{info['version']}-{info['git_revision']}-linux-x86_64.tar.gz"


def windows_package_name(info, platform="windows-x86_64"):
    if platform not in WINDOWS_PLATFORMS:
        raise ValueError("unsupported Windows package architecture")
    return f"maple-agent-{info['profile']}-{info['version']}-{info['git_revision']}-{platform}-setup.exe"


def windows_archive_name(info, platform="windows-x86_64"):
    if platform not in WINDOWS_PLATFORMS:
        raise ValueError("unsupported Windows package architecture")
    return f"maple-agent-{info['profile']}-{info['version']}-{info['git_revision']}-{platform}.tar.gz"


def public_files(directory):
    files = list(directory.iterdir())
    if any(path.is_symlink() or not path.is_file() for path in files):
        raise ValueError("artifact directory contains a symlink or unsupported file")
    return {path.name for path in files}


def write_manifest(info, directory, platform, unsigned, build_number=None):
    directory = Path(directory)
    if platform not in PLATFORMS:
        raise ValueError("unsupported artifact platform")
    files = public_files(directory)
    suffix = {"macos-aarch64": ".dmg", "linux-x86_64": ".tar.gz", "windows-x86_64": "-setup.exe", "windows-aarch64": "-setup.exe"}[platform]
    packages = sorted(name for name in files if name.endswith(suffix))
    if len(packages) != 1:
        raise ValueError("artifact directory must contain exactly one platform package")
    if platform == "linux-x86_64" and (
        packages[0] != linux_package_name(info)
        or not {"build-info.json", packages[0]} <= files
        or files - {"build-info.json", "manifest.json", "SHA256SUMS", packages[0]}
    ):
        raise ValueError("Linux artifact directory must contain only the expected bundle and public metadata")
    manifest = {
        "schema_version": 1,
        "profile": info["profile"],
        "platform": platform,
        "source_sha": info["source_sha"],
        "version": info["version"],
        "package": packages[0],
        "distribution": "unsigned-pr" if unsigned else "master",
        "macos_signing": "ad-hoc" if unsigned else "developer-id-notarized",
    }
    if platform == "linux-x86_64":
        # Linux has no OS code-signing service. This workflow emits checksums;
        # do not describe a SHA-256 file as a detached cryptographic signature.
        manifest["macos_signing"] = None
        manifest["linux_integrity"] = "sha256"
    if platform in WINDOWS_PLATFORMS:
        if not isinstance(build_number, str) or not re.fullmatch(r"[1-9][0-9]{0,17}", build_number):
            raise ValueError("Windows package build number must be a positive decimal string")
        expected = {"build-info.json", windows_package_name(info, platform), windows_archive_name(info, platform)}
        if packages[0] != windows_package_name(info, platform) or not expected <= files or files - expected - {"manifest.json", "SHA256SUMS"}:
            raise ValueError("Windows artifact directory must contain only the expected installer, updater bundle and public metadata")
        manifest["macos_signing"] = None
        manifest["windows_signing"] = "unsigned" if unsigned else "authenticode"
        manifest["build"] = build_number
        manifest["updater"] = windows_archive_name(info, platform)
    (directory / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8", newline="\n")
    files = sorted(p for p in directory.iterdir() if p.is_file() and p.name != "SHA256SUMS")
    (directory / "SHA256SUMS").write_text("".join(
        f"{hashlib.sha256(p.read_bytes()).hexdigest()}  {p.name}\n" for p in files
    ), encoding="utf-8", newline="\n")
    for path in directory.iterdir():
        if path.is_file():
            path.chmod(0o644)
    directory.chmod(0o755)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("field", "bundle-name", "plist", "manifest"))
    parser.add_argument("profile", choices=("dev", "prod"))
    parser.add_argument("path")
    parser.add_argument("--source-sha")
    parser.add_argument("--field")
    parser.add_argument("--build-number")
    parser.add_argument("--output")
    parser.add_argument("--platform", choices=PLATFORMS)
    parser.add_argument("--unsigned", action="store_true")
    args = parser.parse_args()
    info = read_info(args.profile, args.path, args.source_sha, args.platform)
    if args.operation == "bundle-name":
        print(macos_bundle_name(args.profile))
    elif args.operation == "field":
        if args.field not in BUILD_FIELDS:
            raise ValueError("field is not public release metadata")
        print(info[args.field])
    elif args.operation == "plist":
        write_plist(info, args.output, "1" if args.build_number is None else args.build_number)
    elif args.operation == "manifest":
        write_manifest(info, args.output, args.platform, args.unsigned, args.build_number)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, KeyError, OSError, json.JSONDecodeError, tarfile.TarError) as error:
        print(f"release metadata validation failed: {error}", file=sys.stderr)
        sys.exit(1)
