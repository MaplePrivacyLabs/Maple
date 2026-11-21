#!/usr/bin/env python3
"""Validate SDK publication inputs and registry results without credentials.

This helper never publishes, executes package contents, or extracts an archive.
The workflow owns the separate unprivileged build and protected upload jobs.
"""

from __future__ import annotations

import argparse
import base64
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import time
import tomllib
import zlib
from urllib.error import HTTPError, URLError
from urllib.request import HTTPRedirectHandler, Request, build_opener


ROOT = Path(__file__).resolve().parents[2]
REPOSITORY = "MaplePrivacyLabs/Maple"
REPOSITORY_ID = "923138240"
OWNER_ID = "322649754"
REPOSITORY_URL = f"https://github.com/{REPOSITORY}"
PACKAGES = {"npm": "@mapleai/sdk", "rust": "maple-sdk", "proxy": "maple-proxy"}
ARCHIVES = {"npm": "package.tgz", "rust": "package.crate", "proxy": "package.crate"}
# crates.io trusts a workflow file and environment per crate. The proxy crate
# publishes through the Rust SDK's workflow so both crates share that identity.
WORKFLOWS = {"npm": "sdk-publish-npm.yml", "rust": "sdk-publish-rust.yml", "proxy": "sdk-publish-rust.yml"}
# Each package's directory in the repository; Cargo records it as path_in_vcs.
SOURCES = {"npm": "sdk", "rust": "sdk/rust", "proxy": "proxy"}
REGISTRIES = {
    "npm": "https://registry.npmjs.org/%40mapleai%2Fsdk",
    "rust": "https://crates.io/api/v1/crates/maple-sdk",
    "proxy": "https://crates.io/api/v1/crates/maple-proxy",
}
MAX_ARCHIVE = 10 * 1024 * 1024
MAX_EXPANDED = 50 * 1024 * 1024
MAX_MEMBERS = 1024
MAX_METADATA = 1024 * 1024
MAX_REGISTRY = 8 * 1024 * 1024
# npm can hold newly uploaded versions during publish-time malware scanning.
# These are read-only confirmation limits, never publication retry policies.
CONFIRM_POLICY = {"npm": (41, 30), "rust": (25, 5), "proxy": (25, 5)}
MANIFEST_FIELDS = {
    "schema_version", "kind", "name", "version", "mode", "source_sha",
    "run_id", "run_attempt", "archive_sha256", "archive_size",
}
VERSION = re.compile(
    r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
    r"(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?"
    r"(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?"
)


class Rejected(ValueError):
    """A category-only failure; never include registry or archive input."""


class NotVisible(Rejected):
    """Publication is not yet visible in read-only registry metadata."""


def require(condition, message):
    if not condition:
        raise Rejected(message)


def _version(value):
    require(isinstance(value, str) and len(value) <= 128, "Invalid package version")
    match = VERSION.fullmatch(value)
    require(match is not None, "Invalid package version")
    prerelease = match[4]
    if prerelease:
        require(all(not part.isdigit() or part == "0" or not part.startswith("0")
                    for part in prerelease.split(".")), "Invalid prerelease version")
    return tuple(int(match[i]) for i in (1, 2, 3)), prerelease, match[5]


def stable_version(value):
    core, prerelease, build = _version(value)
    require(prerelease is None and build is None, "Only stable X.Y.Z publication is supported")
    return core


def _selection(kind, version, mode):
    require(kind in PACKAGES and mode in {"trusted", "bootstrap"}, "Invalid publication selection")
    stable_version(version)


def _unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, "Duplicate JSON field")
        result[key] = value
    return result


def _json(data):
    try:
        def nonfinite(_):
            raise Rejected("Invalid JSON number")

        return json.loads(data, object_pairs_hook=_unique_object, parse_constant=nonfinite)
    except (UnicodeError, json.JSONDecodeError):
        raise Rejected("Invalid JSON metadata") from None


def _regular_bytes(path, limit):
    try:
        descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(descriptor, "rb") as stream:
            info = os.fstat(stream.fileno())
            require(stat.S_ISREG(info.st_mode) and 0 < info.st_size <= limit,
                    "Invalid file type or size")
            data = stream.read(limit + 1)
        require(len(data) == info.st_size, "File changed while reading")
        return data
    except OSError:
        raise Rejected("Cannot read publication input") from None


def _positive(value):
    require(isinstance(value, str) and re.fullmatch(r"[1-9][0-9]{0,19}", value),
            "Invalid workflow run identity")
    return int(value)


def _metadata(kind, value, version):
    require(isinstance(value, dict), "Invalid package metadata")
    package = value if kind == "npm" else value.get("package")
    require(isinstance(package, dict) and package.get("name") == PACKAGES[kind]
            and package.get("version") == version, "Package identity does not match selection")
    repository = package.get("repository")
    if kind == "npm":
        require(package.get("private", False) is False
                and not any(key in package for key in ("workspaces", "bundledDependencies", "bundleDependencies")),
                "Unsupported npm package publication boundary")
        require(isinstance(repository, dict) and repository.get("type") == "git"
                and repository.get("url") == f"git+{REPOSITORY_URL}.git"
                and repository.get("directory") == "sdk", "Invalid npm repository metadata")
        publish = package.get("publishConfig", {})
        require(isinstance(publish, dict) and set(publish) <= {"access", "registry", "tag", "provenance"},
                "Unsupported npm publication configuration")
        require(isinstance(publish.get("registry", "https://registry.npmjs.org/"), str)
                and publish.get("registry", "https://registry.npmjs.org/").rstrip("/")
                == "https://registry.npmjs.org"
                and publish.get("access", "public") == "public"
                and publish.get("tag", "latest") == "latest"
                and publish.get("provenance", True) is True, "Invalid npm publication configuration")
    else:
        require(repository == REPOSITORY_URL, "Invalid Rust repository metadata")


def validate_source(kind, version, mode, root=ROOT, environ=None):
    """Bind a clean checkout and committed version to this trusted dispatch."""
    _selection(kind, version, mode)
    env = os.environ if environ is None else environ
    require(env.get("GITHUB_EVENT_NAME") == "workflow_dispatch"
            and env.get("GITHUB_REF") == "refs/heads/master", "Publication requires a master dispatch")
    require(env.get("GITHUB_REPOSITORY") == REPOSITORY
            and env.get("GITHUB_REPOSITORY_ID") == REPOSITORY_ID
            and env.get("GITHUB_REPOSITORY_OWNER_ID") == OWNER_ID, "Unexpected repository identity")
    sha = env.get("GITHUB_SHA", "")
    require(re.fullmatch(r"[0-9a-f]{40}", sha), "Invalid workflow source SHA")
    expected_workflow = f"{REPOSITORY}/.github/workflows/{WORKFLOWS[kind]}@refs/heads/master"
    require(env.get("GITHUB_WORKFLOW_REF") == expected_workflow, "Unexpected publisher workflow")
    workflow_shas = [env[key] for key in ("GITHUB_WORKFLOW_SHA", "SDK_PUBLISH_WORKFLOW_SHA") if key in env]
    require(workflow_shas and all(value == sha for value in workflow_shas), "Workflow and source SHA differ")
    try:
        head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=root, check=True,
                              capture_output=True, text=True).stdout.strip()
        dirty = subprocess.run(["git", "status", "--porcelain", "--untracked-files=no"],
                               cwd=root, check=True, capture_output=True, text=True).stdout
    except (OSError, subprocess.SubprocessError):
        raise Rejected("Cannot verify publication checkout") from None
    require(head == sha and not dirty, "Publication checkout must match clean trusted source")
    path = Path(root) / SOURCES[kind] / ("package.json" if kind == "npm" else "Cargo.toml")
    data = _regular_bytes(path, MAX_METADATA)
    try:
        metadata = _json(data) if kind == "npm" else tomllib.loads(data.decode("utf-8"))
    except (UnicodeError, tomllib.TOMLDecodeError):
        raise Rejected("Invalid source manifest") from None
    _metadata(kind, metadata, version)
    return {"kind": kind, "name": PACKAGES[kind], "version": version, "mode": mode,
            "source_sha": sha, "run_id": _positive(env.get("GITHUB_RUN_ID")),
            "run_attempt": _positive(env.get("GITHUB_RUN_ATTEMPT"))}


def _member_path(member, prefix):
    name = member.name.removesuffix("/") if member.isdir() else member.name
    require(0 < len(name) <= 256 and not any(ord(char) < 32 or ord(char) == 127 for char in name)
            and "\\" not in name and ":" not in name, "Unsafe archive path")
    parts = name.split("/")
    require(parts[0] == prefix and all(part not in {"", ".", ".."} for part in parts),
            "Unsafe archive path")
    require(not any(part.casefold() in {".git", ".github", ".cargo", ".npmrc", ".netrc"}
                    or (part.casefold().startswith(".env") and part != ".env.example")
                    for part in parts[1:]), "Forbidden archive configuration")
    return name


def inspect_archive(path, kind, version, source_sha=None, source_metadata=None):
    """Inspect a bounded gzip tar as data; return metadata and exact-byte digest."""
    require(kind in PACKAGES, "Invalid package kind")
    stable_version(version)
    compressed = _regular_bytes(path, MAX_ARCHIVE)
    prefix = "package" if kind == "npm" else f"{PACKAGES[kind]}-{version}"
    wanted = {f"{prefix}/package.json"} if kind == "npm" else {
        f"{prefix}/Cargo.toml", f"{prefix}/.cargo_vcs_info.json",
    }
    metadata_bytes = {}
    paths = {}
    try:
        # Decompress before parsing tar headers. This bounds oversized PAX/long-name
        # metadata too, which tarfile would otherwise allocate before yielding it.
        with tempfile.TemporaryFile() as expanded:
            with gzip.GzipFile(fileobj=io.BytesIO(compressed)) as archive:
                total = 0
                while chunk := archive.read(64 * 1024):
                    total += len(chunk)
                    require(total <= MAX_EXPANDED, "Expanded package exceeds size limit")
                    expanded.write(chunk)
            expanded.seek(0)
            with tarfile.open(fileobj=expanded, mode="r:") as archive:
                for member in archive:
                    require(len(paths) < MAX_MEMBERS, "Package has too many entries")
                    require(member.isfile() or member.isdir(), "Unsupported archive entry")
                    require(not member.mode & 0o7000 and member.sparse is None, "Unsupported archive file mode")
                    path_name = _member_path(member, prefix)
                    key = path_name.casefold()
                    require(key not in paths, "Duplicate archive path")
                    require(all(paths.get(parent) != "file"
                                for parent in ["/".join(key.split("/")[:i])
                                               for i in range(1, len(key.split("/")))]),
                            "Conflicting archive paths")
                    require(not member.isfile() or not any(item.startswith(key + "/") for item in paths),
                            "Conflicting archive paths")
                    paths[key] = "file" if member.isfile() else "directory"
                    require(0 <= member.size <= MAX_EXPANDED, "Invalid archive member size")
                    if path_name in wanted:
                        require(member.isfile() and 0 < member.size <= MAX_METADATA, "Invalid package metadata size")
                        source = archive.extractfile(member)
                        require(source is not None, "Missing package metadata")
                        with source:
                            data = source.read(MAX_METADATA + 1)
                        require(len(data) == member.size, "Truncated package metadata")
                        metadata_bytes[path_name] = data
                    elif member.isfile():
                        # Read every declared file so truncated last entries cannot
                        # pass merely because their bytes are not metadata.
                        source = archive.extractfile(member)
                        require(source is not None, "Missing package file")
                        with source:
                            copied = 0
                            while chunk := source.read(64 * 1024):
                                copied += len(chunk)
                        require(copied == member.size, "Truncated package file")
                # Reject concatenated/hidden tar contents after the first EOF.
                # Different registry/client tar parsers must see the same files.
                expanded.seek(archive.offset)
                padding = expanded.read()
                require(len(padding) >= 1024 and not padding.strip(b"\0"),
                        "Unexpected data after package archive")
        require(wanted <= set(metadata_bytes), "Missing package identity metadata")
        if kind == "npm":
            required = {f"package/dist/{name}" for name in (
                "maple-sdk.es.js", "maple-sdk.umd.cjs", "index.d.ts", "index.d.cts")}
            require(all(paths.get(name) == "file" for name in required), "Missing npm package entrypoint")
            metadata = _json(metadata_bytes["package/package.json"])
        else:
            require(paths.get(f"{prefix}/src/lib.rs") == "file", "Missing Rust package entrypoint")
            metadata = tomllib.loads(metadata_bytes[f"{prefix}/Cargo.toml"].decode("utf-8"))
            vcs = _json(metadata_bytes[f"{prefix}/.cargo_vcs_info.json"])
            require(isinstance(vcs, dict) and isinstance(vcs.get("git"), dict)
                    and vcs.get("path_in_vcs") == SOURCES[kind]
                    and vcs["git"].get("dirty", False) is False
                    and isinstance(vcs["git"].get("sha1"), str)
                    and re.fullmatch(r"[0-9a-f]{40}", vcs["git"]["sha1"]), "Invalid Rust source metadata")
            if source_sha is not None:
                require(vcs["git"]["sha1"] == source_sha, "Rust package source SHA differs")
    except (OSError, EOFError, tarfile.TarError, UnicodeError, tomllib.TOMLDecodeError, zlib.error):
        raise Rejected("Cannot inspect package archive") from None
    _metadata(kind, metadata, version)
    if source_metadata is not None and kind == "npm":
        require(metadata == source_metadata, "Packed npm manifest differs from committed source")
    return {"archive_sha256": hashlib.sha256(compressed).hexdigest(),
            "archive_size": len(compressed), "metadata": metadata}


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def fetch_inventory(kind):
    """Read the complete package inventory; only package HTTP 404 means absent."""
    require(kind in REGISTRIES, "Invalid package registry")
    request = Request(REGISTRIES[kind], headers={
        "Accept": "application/json", "User-Agent": f"Maple-SDK-publisher ({REPOSITORY_URL})",
        "Cache-Control": "no-cache",
    })
    try:
        with build_opener(NoRedirect).open(request, timeout=8) as response:
            require(response.status == 200, "Registry returned unexpected status")
            data = response.read(MAX_REGISTRY + 1)
        require(len(data) <= MAX_REGISTRY, "Registry metadata exceeds size limit")
        return _json(data)
    except HTTPError as error:
        status = error.code
        error.close()
        if status == 404:
            return None
        raise Rejected("Cannot read package registry") from None
    except (OSError, URLError):
        raise Rejected("Cannot read package registry") from None


def registry_versions(kind, inventory):
    """Validate inventory completeness before interpreting registry versions."""
    require(isinstance(inventory, dict), "Invalid registry inventory")
    if kind == "npm":
        require(inventory.get("name") == PACKAGES[kind], "Registry package identity differs")
        versions = inventory.get("versions")
        require(isinstance(versions, dict) and 0 < len(versions) <= 10000, "Invalid npm version inventory")
        for version, metadata in versions.items():
            _version(version)
            require(isinstance(metadata, dict) and metadata.get("name") == PACKAGES[kind]
                    and metadata.get("version") == version, "Invalid npm version metadata")
        tags = inventory.get("dist-tags")
        require(isinstance(tags, dict), "Invalid npm dist tags")
        if "latest" in tags:
            stable_version(tags["latest"])
            require(tags["latest"] in versions, "Latest npm tag is missing from version inventory")
        return versions
    crate, entries = inventory.get("crate"), inventory.get("versions")
    require(isinstance(crate, dict) and crate.get("id") == PACKAGES[kind]
            and isinstance(entries, list) and 0 < len(entries) <= 10000, "Invalid Rust version inventory")
    require(type(crate.get("num_versions")) is int and crate["num_versions"] == len(entries)
            and isinstance(crate.get("versions"), list)
            and len(crate["versions"]) == len(entries), "Incomplete Rust version inventory")
    versions, ids = {}, set()
    for entry in entries:
        require(isinstance(entry, dict) and entry.get("crate") == PACKAGES[kind]
                and type(entry.get("id")) is int and entry["id"] > 0
                and type(entry.get("yanked")) is bool, "Invalid Rust version metadata")
        version = entry.get("num")
        _version(version)
        require(version not in versions and entry["id"] not in ids, "Duplicate Rust registry version")
        versions[version] = entry
        ids.add(entry["id"])
    require(all(type(item) is int for item in crate["versions"])
            and set(crate["versions"]) == ids, "Incomplete Rust version inventory")
    return versions


def validate_registry(kind, version, mode, inventory):
    _selection(kind, version, mode)
    if inventory is None:
        require(mode == "bootstrap", "Trusted publication requires an existing registry package")
        return
    versions = registry_versions(kind, inventory)
    require(mode == "trusted", "Bootstrap requires the package to be absent")
    require(version not in versions, "Package version already exists")
    target = stable_version(version)
    stable = [_version(value)[0] for value in versions if _version(value)[1] is None]
    require(not stable or target > max(stable), "Package version does not advance registry history")
    if kind == "npm" and "latest" in inventory["dist-tags"]:
        latest, prerelease, _ = _version(inventory["dist-tags"]["latest"])
        require(target > latest or (target == latest and prerelease is not None),
                "Package version would move npm latest backward")


def _bundle_directory(directory):
    directory = Path(directory)
    require(directory.is_dir() and not directory.is_symlink(), "Invalid bundle directory")
    return directory


def bundle_archive(directory, archive, context, source_metadata=None):
    """Copy exactly the inspected bytes into a fresh artifact directory."""
    directory = Path(directory)
    require(not directory.exists(), "Bundle output directory already exists")
    kind, version = context["kind"], context["version"]
    inspected = inspect_archive(archive, kind, version, context["source_sha"], source_metadata)
    directory.mkdir(parents=True)
    try:
        destination = directory / ARCHIVES[kind]
        with destination.open("xb") as target:
            target.write(_regular_bytes(archive, MAX_ARCHIVE))
        copied = inspect_archive(destination, kind, version, context["source_sha"], source_metadata)
        require(copied["archive_sha256"] == inspected["archive_sha256"], "Archive changed while bundling")
        manifest = {"schema_version": 1, **context,
                    "archive_sha256": copied["archive_sha256"], "archive_size": copied["archive_size"]}
        (directory / "manifest.json").write_text(json.dumps(manifest, sort_keys=True) + "\n")
        return manifest
    except Exception:
        shutil.rmtree(directory)
        raise


def verify_bundle(directory, kind, version, mode, context, source_metadata=None):
    directory = _bundle_directory(directory)
    _selection(kind, version, mode)
    require({entry.name for entry in directory.iterdir()} == {ARCHIVES[kind], "manifest.json"},
            "Unexpected publication bundle files")
    manifest = _json(_regular_bytes(directory / "manifest.json", MAX_METADATA))
    require(isinstance(manifest, dict) and set(manifest) == MANIFEST_FIELDS, "Invalid publication manifest fields")
    require(type(manifest["schema_version"]) is int and manifest["schema_version"] == 1,
            "Unsupported publication manifest schema")
    require(all(manifest.get(key) == value and type(manifest.get(key)) is type(value)
                for key, value in context.items()), "Publication bundle belongs to another source or run")
    require(manifest["kind"] == kind and manifest["name"] == PACKAGES[kind]
            and manifest["version"] == version and manifest["mode"] == mode,
            "Publication bundle selection differs")
    require(type(manifest["archive_size"]) is int and 0 < manifest["archive_size"] <= MAX_ARCHIVE
            and isinstance(manifest["archive_sha256"], str)
            and re.fullmatch(r"[0-9a-f]{64}", manifest["archive_sha256"]), "Invalid publication archive digest")
    inspected = inspect_archive(directory / ARCHIVES[kind], kind, version, context["source_sha"], source_metadata)
    require(inspected["archive_sha256"] == manifest["archive_sha256"]
            and inspected["archive_size"] == manifest["archive_size"], "Publication archive digest differs")
    return manifest


def confirm_registry(kind, version, inventory, archive):
    if inventory is None:
        raise NotVisible("Published package is not visible yet")
    versions = registry_versions(kind, inventory)
    if version not in versions:
        raise NotVisible("Published version is not visible yet")
    metadata = versions[version]
    data = _regular_bytes(archive, MAX_ARCHIVE)
    if kind == "npm":
        dist = metadata.get("dist")
        require(isinstance(dist, dict) and isinstance(dist.get("integrity"), str),
                "Published npm integrity is missing")
        expected = "sha512-" + base64.b64encode(hashlib.sha512(data).digest()).decode("ascii")
        require(expected in dist["integrity"].split(), "Published npm archive checksum differs")
        if inventory["dist-tags"].get("latest") != version:
            raise NotVisible("Published npm latest tag is not visible yet")
    else:
        require(metadata.get("checksum") == hashlib.sha256(data).hexdigest()
                and metadata.get("yanked") is False, "Published Rust archive checksum or state differs")


def _report(message):
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as output:
            output.write(message)
    print(message, flush=True)


def confirm_publication(directory, kind, version, mode, context, source_metadata=None):
    manifest = verify_bundle(directory, kind, version, mode, context, source_metadata)
    attempts, interval = CONFIRM_POLICY[kind]
    deadline = time.monotonic() + (attempts - 1) * interval
    pending = "Publication is not visible; inspect registry state before another dispatch"
    for attempt in range(attempts):
        require(not attempt or time.monotonic() <= deadline, pending)
        try:
            confirm_registry(kind, version, fetch_inventory(kind), Path(directory) / ARCHIVES[kind])
            break
        except NotVisible:
            if attempt == 0:
                _report("Waiting for registry visibility; checks are read-only and no upload retry will occur.\n")
            if attempt == attempts - 1:
                raise Rejected(pending) from None
            remaining = deadline - time.monotonic()
            require(remaining > 0, pending)
            time.sleep(min(interval, remaining))
    url = (f"https://www.npmjs.com/package/@mapleai/sdk/v/{version}" if kind == "npm"
           else f"https://crates.io/crates/{PACKAGES[kind]}/{version}")
    message = (f"Published [{PACKAGES[kind]} {version}]({url}) from "
               f"[`{context['source_sha']}`]({REPOSITORY_URL}/commit/{context['source_sha']}).\n\n"
               f"Verified archive SHA-256: `{manifest['archive_sha256']}`.\n")
    _report(message)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["source", "bundle", "verify", "confirm"])
    parser.add_argument("--kind", choices=list(PACKAGES), required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--mode", choices=["trusted", "bootstrap"], required=True)
    parser.add_argument("--archive", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--directory", type=Path)
    args = parser.parse_args()
    context = validate_source(args.kind, args.version, args.mode)
    source_metadata = (_json(_regular_bytes(ROOT / "sdk/package.json", MAX_METADATA))
                       if args.kind == "npm" else None)
    if args.command == "source":
        require(args.archive is args.output is args.directory is None, "Unexpected source arguments")
        validate_registry(args.kind, args.version, args.mode, fetch_inventory(args.kind))
        print(json.dumps(context, sort_keys=True))
    elif args.command == "bundle":
        require(args.archive is not None and args.output is not None and args.directory is None,
                "Bundle archive and output are required")
        bundle_archive(args.output, args.archive, context, source_metadata)
    else:
        require(args.directory is not None and args.archive is args.output is None, "Bundle directory is required")
        if args.command == "verify":
            verify_bundle(args.directory, args.kind, args.version, args.mode, context, source_metadata)
            validate_registry(args.kind, args.version, args.mode, fetch_inventory(args.kind))
            print("Publication source, archive, and registry preconditions verified.")
        else:
            confirm_publication(args.directory, args.kind, args.version, args.mode, context, source_metadata)


if __name__ == "__main__":
    try:
        main()
    except (Rejected, OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        message = str(error) if isinstance(error, Rejected) else "Invalid publication input"
        print(f"SDK publication validation rejected: {message}.", file=sys.stderr)
        sys.exit(1)
