#!/usr/bin/env python3
"""Validate public release identity and create package metadata, never secrets."""

import argparse
import base64
import hashlib
import json
from pathlib import Path, PurePosixPath
import plistlib
import posixpath
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
    if platform not in (None, "macos-aarch64", "linux-x86_64", "windows-x86_64"):
        raise ValueError("unsupported artifact platform")
    if not isinstance(info, dict) or set(info) != BUILD_FIELDS:
        raise ValueError("build-info must contain only the documented public release fields")
    if info["profile"] != profile:
        raise ValueError("binary release profile does not match the requested package")
    for field in PUBLIC_FIELDS:
        if field == "update_feed_url" and platform == "windows-x86_64":
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


def windows_package_name(info):
    return f"maple-agent-{info['profile']}-{info['version']}-{info['git_revision']}-windows-x86_64-setup.exe"


def windows_archive_name(info):
    return f"maple-agent-{info['profile']}-{info['version']}-{info['git_revision']}-windows-x86_64.tar.gz"


def public_files(directory):
    files = list(directory.iterdir())
    if any(path.is_symlink() or not path.is_file() for path in files):
        raise ValueError("artifact directory contains a symlink or unsupported file")
    return {path.name for path in files}


def write_manifest(info, directory, platform, unsigned, build_number=None):
    directory = Path(directory)
    if platform not in ("macos-aarch64", "linux-x86_64", "windows-x86_64"):
        raise ValueError("unsupported artifact platform")
    files = public_files(directory)
    suffix = {"macos-aarch64": ".dmg", "linux-x86_64": ".tar.gz", "windows-x86_64": "-setup.exe"}[platform]
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
    if platform == "windows-x86_64":
        if not isinstance(build_number, str) or not re.fullmatch(r"[1-9][0-9]{0,17}", build_number):
            raise ValueError("Windows package build number must be a positive decimal string")
        expected = {"build-info.json", windows_package_name(info), windows_archive_name(info)}
        if packages[0] != windows_package_name(info) or not expected <= files or files - expected - {"manifest.json", "SHA256SUMS"}:
            raise ValueError("Windows artifact directory must contain only the expected installer, updater bundle and public metadata")
        manifest["macos_signing"] = None
        manifest["windows_signing"] = "unsigned" if unsigned else "authenticode"
        manifest["build"] = build_number
        manifest["updater"] = windows_archive_name(info)
    (directory / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8", newline="\n")
    files = sorted(p for p in directory.iterdir() if p.is_file() and p.name != "SHA256SUMS")
    (directory / "SHA256SUMS").write_text("".join(
        f"{hashlib.sha256(p.read_bytes()).hexdigest()}  {p.name}\n" for p in files
    ), encoding="utf-8", newline="\n")
    for path in directory.iterdir():
        if path.is_file():
            path.chmod(0o644)
    directory.chmod(0o755)


def verify_artifacts(profile, directory, unsigned, source_sha=None, platform=None):
    directory = Path(directory).resolve()
    files = public_files(directory)
    manifest = json.loads((directory / "manifest.json").read_text())
    manifest_platform = manifest.get("platform")
    if manifest_platform not in ("macos-aarch64", "linux-x86_64", "windows-x86_64"):
        raise ValueError("unsupported artifact platform")
    if platform is not None and manifest_platform != platform:
        raise ValueError("manifest platform does not match the requested platform")
    platform = manifest_platform
    info = read_info(profile, directory / "build-info.json", source_sha, platform)
    if manifest.get("schema_version") != 1 or any(
        manifest.get(key) != info[key] for key in ("profile", "source_sha", "version")
    ):
        raise ValueError("manifest does not match the binary build information")
    if manifest.get("distribution") != ("unsigned-pr" if unsigned else "master"):
        raise ValueError("unsigned PR artifact cannot satisfy signed master verification")
    if platform == "macos-aarch64" and manifest.get("macos_signing") != (
        "ad-hoc" if unsigned else "developer-id-notarized"
    ):
        raise ValueError("unexpected macOS signing policy")
    if platform == "linux-x86_64":
        if manifest.get("macos_signing", "missing") is not None or manifest.get("linux_integrity") != "sha256":
            raise ValueError("unexpected Linux integrity policy")
        if manifest.get("package") != linux_package_name(info) or files != {
            "build-info.json", "manifest.json", "SHA256SUMS", linux_package_name(info),
        }:
            raise ValueError("Linux artifact directory must contain exactly the expected bundle and public metadata")
    if platform == "windows-x86_64":
        if manifest.get("macos_signing", "missing") is not None or manifest.get("windows_signing") != ("unsigned" if unsigned else "authenticode"):
            raise ValueError("unexpected Windows signing policy")
        if manifest.get("package") != windows_package_name(info) or files != {
            "build-info.json", "manifest.json", "SHA256SUMS", windows_package_name(info), windows_archive_name(info),
        }:
            raise ValueError("Windows artifact directory must contain exactly the expected installer, updater bundle and public metadata")
        if (manifest.get("updater") != windows_archive_name(info)
                or not isinstance(manifest.get("build"), str)
                or not re.fullmatch(r"[1-9][0-9]{0,17}", manifest["build"])):
            raise ValueError("Windows manifest must bind its exact updater bundle and positive build number")
        if set(manifest) != {"schema_version", "profile", "platform", "source_sha", "version", "package", "distribution", "macos_signing", "windows_signing", "build", "updater"}:
            raise ValueError("Windows manifest has unexpected metadata fields")
    lines = (directory / "SHA256SUMS").read_text().splitlines()
    checked = set()
    for line in lines:
        match = re.fullmatch(r"([0-9a-f]{64})  ([A-Za-z0-9._-]+)", line)
        if not match:
            raise ValueError("malformed checksum entry")
        digest, name = match.groups()
        if name in checked or name == "SHA256SUMS":
            raise ValueError("duplicate or self-referential checksum entry")
        path = directory / name
        if path.is_symlink() or not path.is_file() or hashlib.sha256(path.read_bytes()).hexdigest() != digest:
            raise ValueError(f"artifact checksum failed: {name}")
        checked.add(name)
    if checked != files - {"SHA256SUMS"}:
        raise ValueError("artifact directory contains unchecked files or directories")
    package = manifest.get("package", "")
    suffix = {"macos-aarch64": ".dmg", "linux-x86_64": ".tar.gz", "windows-x86_64": "-setup.exe"}[platform]
    if package not in checked or not package.endswith(suffix):
        raise ValueError("manifest package is missing or has the wrong format")
    return manifest


def extract_app_archive(archive, output, app_name):
    """Extract one app without allowing tar links or names to escape its root."""
    root_name = f"{app_name}.app"
    output = Path(output)
    output.mkdir(parents=True, exist_ok=True)
    with tarfile.open(archive, "r:gz") as source:
        members = source.getmembers()
        paths = set()
        for member in members:
            path = PurePosixPath(member.name)
            if path.is_absolute() or ".." in path.parts or not path.parts or path.parts[0] != root_name:
                raise ValueError("app archive contains an absolute, traversal, or unexpected root path")
            normalized = str(path)
            if normalized in paths:
                raise ValueError("app archive contains duplicate paths")
            paths.add(normalized)
            if not (member.isfile() or member.isdir() or member.issym()):
                raise ValueError("app archive contains a hard link or unsupported special file")
            if member.mode & 0o6000 or not member.mode & 0o004 or (member.isdir() and not member.mode & 0o001):
                raise ValueError("app archive contains unsafe or private public file permissions")
            if member.issym():
                link = PurePosixPath(member.linkname)
                resolved = PurePosixPath(posixpath.normpath(str(path.parent / link)))
                if link.is_absolute() or not resolved.parts or resolved.parts[0] != root_name:
                    raise ValueError("app archive symlink escapes the expected app")
        if root_name not in paths or not any(member.name == root_name and member.isdir() for member in members):
            raise ValueError("app archive does not contain exactly one expected app root")
        # Directories and regular files first, then internal symlinks. Refuse
        # descendants of a symlink, rather than trusting archive order/filtering.
        symlinks = {str(PurePosixPath(member.name)) for member in members if member.issym()}
        for member in members:
            path = PurePosixPath(member.name)
            if any(str(parent) in symlinks for parent in path.parents):
                raise ValueError("app archive writes through a symlink")
        for member in sorted(members, key=lambda member: (len(PurePosixPath(member.name).parts), member.name)):
            destination = output / member.name
            if destination.exists() or destination.is_symlink():
                raise ValueError("app archive destination already contains a member")
            destination.parent.mkdir(parents=True, exist_ok=True, mode=0o755)
            if member.isdir():
                destination.mkdir(mode=member.mode & 0o777)
                destination.chmod(member.mode & 0o777)
            elif member.isfile():
                with source.extractfile(member) as data, destination.open("xb") as file:
                    while chunk := data.read(1024 * 1024):
                        file.write(chunk)
                destination.chmod(member.mode & 0o777)
        for member in members:
            if member.issym():
                (output / member.name).symlink_to(member.linkname)
    return output / root_name


def compare_app_payloads(first, second):
    def payload(directory):
        directory = Path(directory)
        result = {}
        for path in sorted(directory.rglob("*")):
            relative = str(path.relative_to(directory))
            if path.is_symlink():
                result[relative] = ("symlink", path.readlink().as_posix())
            elif path.is_file():
                result[relative] = ("file", hashlib.sha256(path.read_bytes()).hexdigest())
            elif path.is_dir():
                result[relative] = ("directory",)
            else:
                raise ValueError("app payload contains an unsupported special file")
        return result
    if payload(first) != payload(second):
        raise ValueError("app archive payload differs from the verified disk-image app")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("validate", "field", "bundle-name", "plist", "manifest", "verify", "extract-app", "compare-apps"))
    parser.add_argument("profile", choices=("dev", "prod"))
    parser.add_argument("path")
    parser.add_argument("--source-sha")
    parser.add_argument("--field")
    parser.add_argument("--build-number")
    parser.add_argument("--output")
    parser.add_argument("--platform", choices=("macos-aarch64", "linux-x86_64", "windows-x86_64"))
    parser.add_argument("--unsigned", action="store_true")
    args = parser.parse_args()
    if args.operation == "verify":
        print(json.dumps(verify_artifacts(args.profile, args.path, args.unsigned, args.source_sha, args.platform)))
        return
    if args.operation == "extract-app":
        extract_app_archive(args.path, args.output, macos_bundle_name(args.profile))
        return
    if args.operation == "compare-apps":
        compare_app_payloads(args.path, args.output)
        return
    info = read_info(args.profile, args.path, args.source_sha, args.platform)
    if args.operation == "bundle-name":
        print(macos_bundle_name(args.profile))
    elif args.operation == "field":
        if args.field not in BUILD_FIELDS:
            raise ValueError("field is not public release metadata")
        print(info[args.field])
    elif args.operation == "plist":
        write_plist(info, args.output, args.build_number or "1")
    elif args.operation == "manifest":
        write_manifest(info, args.output, args.platform, args.unsigned, args.build_number)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, KeyError, OSError, json.JSONDecodeError, tarfile.TarError) as error:
        print(f"release metadata validation failed: {error}", file=sys.stderr)
        sys.exit(1)
