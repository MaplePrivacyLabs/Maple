#!/usr/bin/env python3
"""Build-time iOS profile selection and verification of the resulting app/IPA.

The public profile is packaged as an iOS resource. Verification reads the actual
bundle identity and signed resource, rather than trusting an artifact filename.
It complements (and does not replace) the release signature/hash checks.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import plistlib
import re
import shutil
import sys
from urllib.parse import urlsplit
import zipfile


REPO_ROOT = Path(__file__).resolve().parents[2]
TAURI_ROOT = REPO_ROOT / "apps/maple-research/frontend/src-tauri"
RESOURCE_PATH = "assets/maple-build-profile.json"
STATE_PATHS = (
    "gen/apple/maple_iOS/Info.plist",
    "gen/apple/maple_iOS/maple_iOS.entitlements",
    "gen/apple/maple.xcodeproj/project.pbxproj",
    "gen/apple/ExportOptions.plist",
    f"gen/apple/{RESOURCE_PATH}",
    "gen/apple/Assets.xcassets/AppIcon.appiconset",
    ".cargo/config.toml",
)
PROFILES = {
    "production": {
        "bundle_identifier": "cloud.opensecret.maple",
        "display_name": "Maple",
        "environment": {
            "VITE_OPEN_SECRET_API_URL": "https://enclave.trymaple.ai",
            "VITE_OPEN_SECRET_PCR_ENVIRONMENT": "production",
            "VITE_OS_FLAGS_BASE_URL": "https://flags.opensecret.cloud",
            "VITE_MAPLE_BILLING_API_URL": "https://billing.opensecret.cloud",
            "VITE_CLIENT_ID": "ba5a14b5-d915-47b1-b7b1-afda52bc5fc6",
            "VITE_MAPLE_APP_VARIANT": "production",
        },
    },
    "dev": {
        "bundle_identifier": "cloud.opensecret.maple.dev",
        "display_name": "Maple Dev",
        "environment": {
            "VITE_OPEN_SECRET_API_URL": "https://enclave.secretgpt.ai",
            "VITE_OPEN_SECRET_PCR_ENVIRONMENT": "development",
            "VITE_OS_FLAGS_BASE_URL": "https://flags-dev.opensecret.cloud",
            "VITE_MAPLE_BILLING_API_URL": "https://billing-dev.opensecret.cloud",
            "VITE_CLIENT_ID": "ba5a14b5-d915-47b1-b7b1-afda52bc5fc6",
            "VITE_MAPLE_APP_VARIANT": "dev",
        },
    },
}


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def profile_for(variant):
    if variant not in PROFILES:
        raise ValueError("iOS variant must be production or dev")
    return PROFILES[variant]


def validate_build_number(value):
    # Apple's CFBundleVersion: one to three numeric components, 4/2/2 digits.
    if not re.fullmatch(r"[1-9][0-9]{0,3}(?:\.[0-9]{1,2}){0,2}", value):
        raise ValueError("iOS build number must use Apple's 4/2/2 digit format")
    return value


def validate_auth_origin(value):
    if not value:
        raise ValueError("MAPLE_IOS_DEV_AUTH_ORIGIN is required for Maple Dev native sign-in")
    parsed = urlsplit(value)
    if (parsed.scheme != "https" or not parsed.hostname or parsed.username is not None
            or parsed.password is not None or parsed.path not in ("", "/")
            or parsed.query or parsed.fragment or parsed.port not in (None, 443)
            or parsed.hostname != parsed.hostname.lower()
            or not re.fullmatch(r"[a-z0-9](?:[a-z0-9.-]*[a-z0-9])?", parsed.hostname)):
        raise ValueError("MAPLE_IOS_DEV_AUTH_ORIGIN must be a canonical HTTPS origin without credentials, port, path, query or fragment")
    canonical = f"https://{parsed.hostname}"
    if value != canonical:
        raise ValueError("MAPLE_IOS_DEV_AUTH_ORIGIN must use its canonical HTTPS origin spelling")
    return canonical


def make_profile(variant, source_sha, build_number, frontend_hash, environment):
    expected = dict(profile_for(variant))
    expected["environment"] = dict(expected["environment"])
    if variant == "dev":
        expected["environment"]["VITE_MAPLE_DEV_AUTH_ORIGIN"] = validate_auth_origin(
            environment.get("VITE_MAPLE_DEV_AUTH_ORIGIN", ""))
    if not re.fullmatch(r"[0-9a-f]{40}", source_sha):
        raise ValueError("source commit must be a full Git SHA")
    if not re.fullmatch(r"[0-9a-f]{64}", frontend_hash):
        raise ValueError("frontend tree hash must be SHA-256")
    validate_build_number(build_number)
    for key, value in expected["environment"].items():
        if environment.get(key) != value:
            raise ValueError(f"iOS {variant} environment does not match {key}")
    return {
        "schema_version": 1,
        "variant": variant,
        **expected,
        "url_scheme": expected["bundle_identifier"],
        "source_sha": source_sha,
        "build_number": build_number,
        "frontend_tree_sha256": frontend_hash,
    }


def remove_path(path):
    if path.is_symlink() or path.is_file():
        path.unlink()
    elif path.exists():
        shutil.rmtree(path)


def copy_path(source, destination):
    destination.parent.mkdir(parents=True, exist_ok=True)
    if source.is_dir() and not source.is_symlink():
        shutil.copytree(source, destination, symlinks=True)
    else:
        shutil.copy2(source, destination, follow_symlinks=False)


def snapshot(root, state):
    present = []
    for relative in STATE_PATHS:
        source = root / relative
        if source.exists() or source.is_symlink():
            copy_path(source, state / "original" / relative)
            present.append(relative)
    # No project mutations happen until the complete snapshot exists.
    write_json(state / "original-paths.json", present)


def restore(root, state):
    manifest = state / "original-paths.json"
    if not manifest.exists():
        return
    present = json.loads(manifest.read_text())
    for relative in STATE_PATHS:
        target = root / relative
        remove_path(target)
        if relative in present:
            copy_path(state / "original" / relative, target)


def prepare_dev(root):
    info_path = root / "gen/apple/maple_iOS/Info.plist"
    with info_path.open("rb") as handle:
        info = plistlib.load(handle)
    info["CFBundleDisplayName"] = "Maple Dev"
    info["CFBundleURLTypes"] = [{
        "CFBundleURLName": "cloud.opensecret.maple.dev",
        "CFBundleURLSchemes": ["cloud.opensecret.maple.dev"],
    }]
    with info_path.open("wb") as handle:
        plistlib.dump(info, handle)
    entitlements_path = root / "gen/apple/maple_iOS/maple_iOS.entitlements"
    with entitlements_path.open("rb") as handle:
        entitlements = plistlib.load(handle)
    entitlements.pop("com.apple.developer.associated-domains", None)
    with entitlements_path.open("wb") as handle:
        plistlib.dump(entitlements, handle)
    export_path = root / "gen/apple/ExportOptions.plist"
    with export_path.open("rb") as handle:
        options = plistlib.load(handle)
    options.update({
        "method": "app-store-connect",
        "testFlightInternalTestingOnly": True,
        "manageAppVersionAndBuildNumber": False,
    })
    with export_path.open("wb") as handle:
        plistlib.dump(options, handle)
    icon = root / "icons/dev/ios/AppIcon-512@2x.png"
    if not icon.is_file():
        raise ValueError("missing generated Maple Dev iOS icons")
    destination = root / "gen/apple/Assets.xcassets/AppIcon.appiconset"
    remove_path(destination)
    destination.mkdir(parents=True)
    shutil.copy2(icon, destination / "MapleDev.png")
    # Match the existing Xcode catalog's universal iOS 1024-point format.
    write_json(destination / "Contents.json", {
        "images": [{"filename": "MapleDev.png", "idiom": "universal",
                    "platform": "ios", "size": "1024x1024"}],
        "info": {"author": "xcode", "version": 1},
    })


def build_config(root, state, profile):
    config = {}
    if profile["variant"] == "dev":
        config = json.loads((root / "tauri.ios-dev.conf.json").read_text())
    config["build"] = {"beforeBuildCommand": None}
    config.setdefault("bundle", {}).update({
        "resources": {str(state / "maple-build-profile.json"): "maple-build-profile.json"},
        "iOS": {"bundleVersion": profile["build_number"]},
    })
    write_json(state / "tauri-build-config.json", config)


def verify_bundle(info, embedded, variant, source_sha, build_number, *, auth_origin=None,
                  allow_exported_build_number=False):
    expected = profile_for(variant)
    environment = dict(expected["environment"])
    if variant == "dev":
        environment["VITE_MAPLE_DEV_AUTH_ORIGIN"] = validate_auth_origin(auth_origin)
    expected_profile = make_profile(
        variant, source_sha, build_number,
        embedded.get("frontend_tree_sha256", ""), environment,
    )
    if embedded != expected_profile:
        raise ValueError("packaged iOS build profile does not match the requested build")
    if info.get("CFBundleIdentifier") != expected["bundle_identifier"]:
        raise ValueError("packaged iOS bundle identifier does not match variant")
    if info.get("CFBundleDisplayName", info.get("CFBundleName")) != expected["display_name"]:
        raise ValueError("packaged iOS display name does not match variant")
    if info.get("CFBundleVersion") != build_number and not allow_exported_build_number:
        raise ValueError("packaged iOS build number does not match requested build")
    validate_build_number(info.get("CFBundleVersion", ""))
    schemes = [scheme for entry in info.get("CFBundleURLTypes", [])
               for scheme in entry.get("CFBundleURLSchemes", [])]
    if expected["bundle_identifier"] not in schemes:
        raise ValueError("packaged iOS URL scheme is missing")
    other = PROFILES["production" if variant == "dev" else "dev"]["bundle_identifier"]
    if other in schemes:
        raise ValueError("packaged iOS app claims the other variant's URL scheme")
    return {
        "profile": embedded,
        "bundle_identifier": info["CFBundleIdentifier"],
        "display_name": info.get("CFBundleDisplayName", info.get("CFBundleName")),
        "build_number": info["CFBundleVersion"],
        "version": info["CFBundleShortVersionString"],
        "url_schemes": schemes,
    }


def verify_app(app, variant, source_sha, build_number, *, auth_origin=None):
    info = plistlib.loads((app / "Info.plist").read_bytes())
    embedded = json.loads((app / RESOURCE_PATH).read_bytes())
    return verify_bundle(info, embedded, variant, source_sha, build_number, auth_origin=auth_origin)


def verify_ipa(ipa, variant, source_sha, build_number, *, auth_origin=None):
    with zipfile.ZipFile(ipa) as archive:
        names = archive.namelist()
        infos = [name for name in names if re.fullmatch(r"Payload/[^/]+\.app/Info\.plist", name)]
        if len(infos) != 1:
            raise ValueError("IPA must contain exactly one application Info.plist")
        info_path = infos[0]
        resource = info_path.removesuffix("Info.plist") + RESOURCE_PATH
        if names.count(resource) != 1:
            raise ValueError("IPA must contain exactly one public build profile")
        info = plistlib.loads(archive.read(info_path))
        report = verify_bundle(
            info, json.loads(archive.read(resource)),
            variant, source_sha, build_number,
            auth_origin=auth_origin,
            # Preserve production's existing export-managed build numbering.
            # Dev ExportOptions explicitly disables that Xcode behavior.
            allow_exported_build_number=variant == "production",
        )
        if variant == "dev" and info.get("TFInternalTestingOnly") is not True:
            raise ValueError("Maple Dev IPA must be restricted to internal TestFlight testing")
    with ipa.open("rb") as handle:
        report["ipa_sha256"] = hashlib.file_digest(handle, "sha256").hexdigest()
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    validate = commands.add_parser("validate-auth-origin")
    validate.add_argument("origin")
    prepare = commands.add_parser("prepare")
    prepare.add_argument("--state-dir", required=True, type=Path)
    prepare.add_argument("--frontend-hash", required=True)
    for name in ("verify-app", "verify-ipa"):
        subparser = commands.add_parser(name)
        subparser.add_argument("artifact", type=Path)
        subparser.add_argument("--report", type=Path)
        subparser.add_argument("--auth-origin", default=os.environ.get("MAPLE_IOS_DEV_AUTH_ORIGIN"))
    restore_parser = commands.add_parser("restore")
    restore_parser.add_argument("--state-dir", required=True, type=Path)
    for name in ("prepare", "verify-app", "verify-ipa"):
        subparser = commands.choices[name]
        subparser.add_argument("--variant", required=True, choices=PROFILES)
        subparser.add_argument("--source-sha", required=True)
        subparser.add_argument("--build-number", required=True)
    args = parser.parse_args()
    if args.command == "validate-auth-origin":
        print(validate_auth_origin(args.origin))
    elif args.command == "restore":
        restore(TAURI_ROOT, args.state_dir)
    elif args.command == "prepare":
        profile = make_profile(args.variant, args.source_sha, args.build_number,
                               args.frontend_hash, os.environ)
        snapshot(TAURI_ROOT, args.state_dir)
        write_json(args.state_dir / "maple-build-profile.json", profile)
        if args.variant == "dev":
            prepare_dev(TAURI_ROOT)
        build_config(TAURI_ROOT, args.state_dir, profile)
    else:
        verifier = verify_app if args.command == "verify-app" else verify_ipa
        report = verifier(args.artifact, args.variant, args.source_sha, args.build_number,
                          auth_origin=args.auth_origin)
        if args.report:
            write_json(args.report, report)
        print(f"verified-ios-profile  {args.variant}  {report['bundle_identifier']}  {args.build_number}")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, KeyError, zipfile.BadZipFile) as error:
        print(f"iOS build profile check failed: {error}", file=sys.stderr)
        sys.exit(1)
