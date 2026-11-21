#!/usr/bin/env python3
"""Upload a validated Maple crate archive without running package code.

The protected workflow must verify its source, bundle and registry precondition
before invoking this transport. This module only reads data from package.crate;
it never invokes Cargo, a credential provider, build.rs, or a package dependency.
The same encoder is exercised without credentials by --validate-only.

Wire format: https://doc.rust-lang.org/cargo/reference/registry-web-api.html#publish
Metadata follows Cargo 1.89's prepare_transmit, restricted to crates.io-only
dependencies; unsupported manifest forms fail before credentials are read.
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import re
import ssl
import stat
import struct
import sys
import tarfile
import tomllib
from urllib.error import HTTPError
from urllib.request import HTTPRedirectHandler, HTTPSHandler, ProxyHandler, Request, build_opener

# The crates this transport may upload. The Rust SDK is the default; the proxy
# crate shares its workflow, protected environment and trusted publisher.
PACKAGES = ("maple-sdk", "maple-proxy")
PACKAGE = PACKAGES[0]
REPOSITORY = "https://github.com/MaplePrivacyLabs/Maple"
ENDPOINT = "https://crates.io/api/v1/crates/new"
MAX_ARCHIVE = 10 * 1024 * 1024
MAX_UNPACKED = 50 * 1024 * 1024
MAX_METADATA = 1024 * 1024
MAX_RESPONSE = 64 * 1024
MAX_MEMBERS = 1024
VERSION = re.compile(r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\Z")
NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9_-]{0,63}\Z")


class Rejected(ValueError):
    """Fixed messages only; never include archive data or credential-bearing errors."""


class UploadUnconfirmed(Rejected):
    """An upload may have succeeded; callers must confirm rather than retry it."""


def require(condition, message):
    if not condition:
        raise Rejected(message)


def text(value, *, optional=False, limit=4096):
    if value is None and optional:
        return None
    require(isinstance(value, str) and len(value.encode("utf-8")) <= limit,
            "Invalid manifest string")
    require("\x00" not in value, "Invalid manifest string")
    return value


def strings(value, *, limit=512):
    require(isinstance(value, list) and len(value) <= limit, "Invalid manifest string list")
    return [text(item) for item in value]


def boolean(value):
    require(type(value) is bool, "Invalid dependency boolean")
    return value


def relative_path(value):
    value = text(value, limit=1024)
    require(value and "\\" not in value and not any(ord(c) < 32 or ord(c) == 127 for c in value),
            "Invalid archive path")
    require(all(part not in {"", ".", ".."} for part in value.split("/")),
            "Invalid archive path")
    return value


def read_archive(directory, version, crate_name=PACKAGE):
    require(crate_name in PACKAGES, "Unsupported crate")
    require(isinstance(version, str) and len(version) <= 64 and VERSION.fullmatch(version),
            "Only stable numeric SDK versions are supported")
    path = Path(directory) / "package.crate"
    try:
        # O_NOFOLLOW and fstat bind this read to a regular archive, not a link or device.
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(fd, "rb") as archive_file:
            info = os.fstat(archive_file.fileno())
            require(stat.S_ISREG(info.st_mode) and 0 < info.st_size <= MAX_ARCHIVE,
                    "Invalid crate archive size or type")
            archive = archive_file.read(MAX_ARCHIVE + 1)
        require(len(archive) == info.st_size, "Crate archive changed while reading")
        with gzip.GzipFile(fileobj=io.BytesIO(archive), mode="rb") as compressed:
            unpacked = compressed.read(MAX_UNPACKED + 1)
        require(len(unpacked) <= MAX_UNPACKED, "Crate archive exceeds expanded limit")
        files = {}
        prefix = f"{crate_name}-{version}/"
        with tarfile.open(fileobj=io.BytesIO(unpacked), mode="r:") as tar:
            for member in tar:
                require(len(files) < MAX_MEMBERS, "Crate archive has too many files")
                name = relative_path(member.name)
                require(name.startswith(prefix) and member.isfile() and not member.issparse(),
                        "Invalid crate archive member")
                name = name.removeprefix(prefix)
                relative_path(name)
                require(name not in files, "Duplicate crate archive member")
                require(0 <= member.size <= MAX_UNPACKED, "Invalid crate member size")
                source = tar.extractfile(member)
                require(source is not None, "Missing crate member contents")
                with source:
                    content = source.read(member.size + 1)
                require(len(content) == member.size, "Truncated crate archive member")
                files[name] = content
        require("Cargo.toml" in files, "Crate manifest is missing")
        require(len(files["Cargo.toml"]) <= MAX_METADATA, "Crate manifest exceeds limit")
        manifest = tomllib.loads(files["Cargo.toml"].decode("utf-8"))
    except Rejected:
        raise
    except Exception:
        raise Rejected("Unable to read a valid crate archive") from None
    return archive, files, manifest


def dependencies(manifest):
    result = []

    def collect(table, target=None):
        require(isinstance(table, dict), "Invalid dependency table")
        for key, kind in (("dependencies", "normal"), ("dev-dependencies", "dev"),
                          ("build-dependencies", "build")):
            deps = table.get(key, {})
            require(isinstance(deps, dict), "Invalid dependency table")
            for alias, dep in sorted(deps.items()):
                require(NAME.fullmatch(alias), "Invalid dependency name")
                require(isinstance(dep, dict), "Expected normalized dependency table")
                require(set(dep) <= {"version", "package", "features", "optional", "default-features"},
                        "Unsupported dependency manifest fields")
                version_req = text(dep.get("version"), limit=512)
                require(version_req.strip() and version_req.strip() != "*",
                        "Dependency must have a version requirement")
                name = dep.get("package", alias)
                require(isinstance(name, str) and NAME.fullmatch(name), "Invalid dependency name")
                # Cargo's registry accepts the manifest requirement syntax directly.
                # Preserve it instead of implementing a second SemVer parser.
                result.append({
                    "name": name,
                    "version_req": version_req,
                    "features": strings(dep.get("features", [])),
                    "optional": boolean(dep.get("optional", False)),
                    "default_features": boolean(dep.get("default-features", True)),
                    "target": target,
                    "kind": kind,
                    "registry": None,
                    "explicit_name_in_toml": alias if "package" in dep else None,
                })
                require(len(result) <= 512, "Too many crate dependencies")

    collect(manifest)
    targets = manifest.get("target", {})
    require(isinstance(targets, dict), "Invalid target dependency table")
    for target, table in sorted(targets.items()):
        target = text(target, limit=1024)
        require(target and isinstance(table, dict)
                and set(table) <= {"dependencies", "dev-dependencies", "build-dependencies"},
                "Unsupported target dependency table")
        collect(table, target)
    return result


def metadata_from_manifest(manifest, files, version, crate_name=PACKAGE):
    require(crate_name in PACKAGES, "Unsupported crate")
    require(isinstance(manifest, dict) and isinstance(manifest.get("package"), dict),
            "Invalid crate manifest")
    require(not ({"workspace", "patch", "replace", "cargo-features"} & set(manifest)),
            "Unsupported crate manifest form")
    package = manifest["package"]
    require(package.get("name") == crate_name and package.get("version") == version,
            "Crate identity does not match requested release")
    require(package.get("repository") == REPOSITORY, "Crate repository does not match")
    require("publish" not in package or package["publish"] == ["crates-io"],
            "Crate manifest does not allow crates.io publication")
    require(package.get("build") is False, "Expected a packaged crate without a build script")
    require(isinstance(package.get("description"), str) and package["description"].strip(),
            "Crate description is missing")

    readme_file = package.get("readme")
    if readme_file is False:
        readme_file = None
    readme = None
    if readme_file is not None:
        readme_file = relative_path(readme_file)
        require(readme_file in files, "Crate README is missing")
        require(len(files[readme_file]) <= MAX_METADATA, "Crate README exceeds limit")
        try:
            readme = files[readme_file].decode("utf-8")
        except UnicodeError:
            raise Rejected("Crate README must be UTF-8") from None

    license_file = package.get("license-file")
    if license_file is not None:
        license_file = relative_path(license_file)
        require(license_file in files, "Crate license file is missing")
    license_value = text(package.get("license"), optional=True)
    require(bool(license_value) or license_file is not None, "Crate license is missing")

    features = manifest.get("features", {})
    require(isinstance(features, dict) and len(features) <= 512, "Invalid crate features")
    features = {text(name): strings(values) for name, values in features.items()}
    badges = manifest.get("badges", {})
    require(isinstance(badges, dict) and len(badges) <= 64, "Invalid crate badges")
    for name, badge in badges.items():
        text(name)
        require(isinstance(badge, dict) and len(badge) <= 64, "Invalid crate badge")
        for key, value in badge.items():
            text(key)
            text(value)

    result = {
        "name": crate_name, "vers": version, "deps": dependencies(manifest),
        "features": features, "authors": strings(package.get("authors", [])),
        "keywords": strings(package.get("keywords", [])),
        "categories": strings(package.get("categories", [])),
        "readme": readme, "readme_file": readme_file,
        "license": license_value, "license_file": license_file, "badges": badges,
    }
    for key in ("description", "homepage", "documentation", "repository", "links"):
        result[key] = text(package.get(key), optional=True)
    result["rust_version"] = text(package.get("rust-version"), optional=True)
    return result


def prepare_upload(directory, version, crate_name=PACKAGE):
    archive, files, manifest = read_archive(directory, version, crate_name)
    metadata = metadata_from_manifest(manifest, files, version, crate_name)
    encoded = json.dumps(metadata, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode("utf-8")
    require(len(encoded) <= MAX_METADATA, "Crate upload metadata exceeds limit")
    payload = struct.pack("<I", len(encoded)) + encoded + struct.pack("<I", len(archive)) + archive
    return metadata, payload, hashlib.sha256(archive).hexdigest()


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def upload(payload, token):
    require(isinstance(token, str) and 0 < len(token) <= 4096
            and all(33 <= ord(c) <= 126 for c in token), "Publishing credential is missing or invalid")
    require(isinstance(payload, bytes) and len(payload) <= MAX_ARCHIVE + MAX_METADATA + 8,
            "Invalid crate upload payload")
    # Disable ambient proxies and redirects. Authorization goes only to the
    # fixed crates.io HTTPS endpoint; no response body/header is ever logged.
    opener = build_opener(ProxyHandler({}), NoRedirect(), HTTPSHandler(context=ssl.create_default_context()))
    request = Request(ENDPOINT, data=payload, method="PUT", headers={
        "Authorization": token, "Content-Type": "application/octet-stream",
        "Accept": "application/json", "User-Agent": "Maple-SDK-publisher/1.0 (https://github.com/MaplePrivacyLabs/Maple)",
    })
    try:
        with opener.open(request, timeout=60) as response:
            if not 200 <= response.status < 300:
                raise UploadUnconfirmed("Registry upload outcome is unconfirmed; verify the exact version before retrying")
            body = response.read(MAX_RESPONSE + 1)
        if len(body) > MAX_RESPONSE:
            raise ValueError
        result = json.loads(body)
        if not isinstance(result, dict) or "errors" in result:
            raise ValueError
        warnings = result.get("warnings", {})
        if not isinstance(warnings, dict) or not all(isinstance(v, list) for v in warnings.values()):
            raise ValueError
        return sum(len(values) for values in warnings.values())
    except HTTPError as error:
        error.close()
        raise UploadUnconfirmed("Registry upload did not confirm success; verify the exact version before retrying") from None
    except Exception:
        raise UploadUnconfirmed("Registry upload outcome is unconfirmed; verify the exact version before retrying") from None


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--directory", type=Path, required=True)
    parser.add_argument("--package", choices=PACKAGES, default=PACKAGE)
    parser.add_argument("--version", required=True)
    parser.add_argument("--validate-only", action="store_true")
    args = parser.parse_args(argv)
    try:
        metadata, payload, digest = prepare_upload(args.directory, args.version, args.package)
        result = {"name": args.package, "version": metadata["vers"], "archive_sha256": digest,
                  "upload_bytes": len(payload)}
        if args.validate_only:
            result["status"] = "validated"
        else:
            result["warnings_count"] = upload(payload, os.environ.get("CARGO_REGISTRY_TOKEN", ""))
            result["status"] = "upload-accepted-awaiting-registry-confirmation"
        print(json.dumps(result, sort_keys=True))
        return 0
    except Rejected as error:
        print(f"SDK crate publication rejected: {error}", file=sys.stderr)
        return 1
    except Exception:
        print("SDK crate publication rejected: unexpected validation failure", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
