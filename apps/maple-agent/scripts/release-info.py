#!/usr/bin/env python3
"""Validate public release identity and create package metadata, never secrets."""

import argparse
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
    "display_name", "bundle_id", "data_namespace", "api_url", "billing_api_url", "web_url",
    "client_id", "pcr_environment", "update_tag_prefix", "prerelease",
)
BUILD_FIELDS = set(PUBLIC_FIELDS) | {"profile", "version", "git_revision", "source_sha"}


def read_info(profile, path, source_sha=None):
    return validate_info(profile, json.loads(Path(path).read_text()), source_sha)


def validate_info(profile, info, source_sha=None):
    profiles = json.loads((COMPONENT / "release-profiles.json").read_text())
    if profile not in ("dev", "prod"):
        raise ValueError("release profile must be dev or prod")
    if not isinstance(info, dict) or set(info) != BUILD_FIELDS:
        raise ValueError("build-info must contain only the documented public release fields")
    if info["profile"] != profile:
        raise ValueError("binary release profile does not match the requested package")
    for field in PUBLIC_FIELDS:
        if info[field] != profiles[profile][field]:
            raise ValueError(f"binary {field} does not match the checked-in release profile")
    if not isinstance(info["prerelease"], bool):
        raise ValueError("binary prerelease marker must be a boolean")
    if any(not isinstance(info[field], str) for field in BUILD_FIELDS - {"prerelease"}):
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
        "CFBundleDisplayName": info["display_name"],
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


def write_manifest(info, directory, platform, unsigned):
    directory = Path(directory)
    suffix = ".dmg" if platform == "macos-aarch64" else ".AppImage"
    packages = sorted(p.name for p in directory.iterdir() if p.is_file() and p.name.endswith(suffix))
    if len(packages) != 1:
        raise ValueError("artifact directory must contain exactly one platform package")
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
    (directory / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    files = sorted(p for p in directory.iterdir() if p.is_file() and p.name != "SHA256SUMS")
    (directory / "SHA256SUMS").write_text("".join(
        f"{hashlib.sha256(p.read_bytes()).hexdigest()}  {p.name}\n" for p in files
    ))
    for path in directory.iterdir():
        if path.is_file():
            path.chmod(0o755 if path.name.endswith(".AppImage") else 0o644)
    directory.chmod(0o755)


def verify_artifacts(profile, directory, unsigned, source_sha=None):
    directory = Path(directory).resolve()
    info = read_info(profile, directory / "build-info.json", source_sha)
    manifest = json.loads((directory / "manifest.json").read_text())
    platform = manifest.get("platform")
    if platform not in ("macos-aarch64", "linux-x86_64"):
        raise ValueError("unsupported artifact platform")
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
    files = {p.name for p in directory.iterdir() if p.is_file() and p.name != "SHA256SUMS"}
    if checked != files or any(not p.is_file() or p.is_symlink() for p in directory.iterdir()):
        raise ValueError("artifact directory contains unchecked files or directories")
    package = manifest.get("package", "")
    suffix = ".dmg" if platform == "macos-aarch64" else ".AppImage"
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
    parser.add_argument("operation", choices=("validate", "field", "plist", "manifest", "verify", "extract-app", "compare-apps"))
    parser.add_argument("profile", choices=("dev", "prod"))
    parser.add_argument("path")
    parser.add_argument("--source-sha")
    parser.add_argument("--field")
    parser.add_argument("--build-number", default="1")
    parser.add_argument("--output")
    parser.add_argument("--platform", choices=("macos-aarch64", "linux-x86_64"))
    parser.add_argument("--unsigned", action="store_true")
    args = parser.parse_args()
    if args.operation == "verify":
        print(json.dumps(verify_artifacts(args.profile, args.path, args.unsigned, args.source_sha)))
        return
    if args.operation == "extract-app":
        app_name = json.loads((COMPONENT / "release-profiles.json").read_text())[args.profile]["display_name"]
        extract_app_archive(args.path, args.output, app_name)
        return
    if args.operation == "compare-apps":
        compare_app_payloads(args.path, args.output)
        return
    info = read_info(args.profile, args.path, args.source_sha)
    if args.operation == "field":
        if args.field not in BUILD_FIELDS:
            raise ValueError("field is not public release metadata")
        print(info[args.field])
    elif args.operation == "plist":
        write_plist(info, args.output, args.build_number)
    elif args.operation == "manifest":
        write_manifest(info, args.output, args.platform, args.unsigned)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, KeyError, OSError, json.JSONDecodeError, tarfile.TarError) as error:
        print(f"release metadata validation failed: {error}", file=sys.stderr)
        sys.exit(1)
