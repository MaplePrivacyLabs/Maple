#!/usr/bin/env python3
"""Bound and unpack static Pages artifacts without executing their contents.

The caller must authorize the GitHub repository, workflow, event and run before
using this module. A manifest and its digest establish consistency, not trust:
an artifact producer can choose both. Errors intentionally omit artifact data.
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import struct
import sys
import tarfile
import tempfile
from typing import BinaryIO
import unicodedata
import zipfile
import zlib


ARCHIVE_NAME = "maple-web-dist.tar.gz"
AUTH_ARCHIVE_NAME = "maple-auth-dist.tar.gz"
MANIFEST_NAME = "pages-artifact.json"
MAX_MANIFEST_BYTES = 16 * 1024
MAX_ARCHIVE_BYTES = 200 * 1024 * 1024
MAX_EXPANDED_BYTES = 200 * 1024 * 1024
MAX_FILE_BYTES = 25 * 1024 * 1024
MAX_ENTRIES = 20_000
CHUNK_BYTES = 64 * 1024
MANIFEST_FIELDS = frozenset(
    {"schema_version", "profile", "source_sha", "run_id", "run_attempt", "archive_sha256"}
)
RESERVED_COMPONENTS = frozenset(
    {
        "functions",
        "_worker.js",
        "wrangler.toml",
        "wrangler.json",
        "wrangler.jsonc",
        "package.json",
        "bunfig.toml",
        "tsconfig.json",
        "_routes.json",
        "_headers",
        "_redirects",
        ".assetsignore",
    }
)


class ArtifactError(ValueError):
    """A safe, fixed-category error; never include producer-controlled text."""


def _hex(value: object, length: int) -> bool:
    return isinstance(value, str) and re.fullmatch(rf"[0-9a-f]{{{length}}}", value) is not None


def _positive_integer(value: object) -> bool:
    return type(value) is int and value > 0


def validate_manifest(value: object) -> dict:
    """Validate the complete manifest, including strict types and unknown keys."""
    if not isinstance(value, dict) or set(value) != MANIFEST_FIELDS:
        raise ArtifactError("Invalid artifact manifest fields")
    if type(value["schema_version"]) is not int or value["schema_version"] != 1:
        raise ArtifactError("Unsupported artifact manifest version")
    if value["profile"] not in ("pr", "release", "auth-release"):
        raise ArtifactError("Invalid artifact build profile")
    if not _hex(value["source_sha"], 40) or not _hex(value["archive_sha256"], 64):
        raise ArtifactError("Invalid artifact digest or source identity")
    if not _positive_integer(value["run_id"]) or not _positive_integer(value["run_attempt"]):
        raise ArtifactError("Invalid artifact run identity")
    return dict(value)


def _unique_object(pairs: list[tuple[str, object]]) -> dict:
    result = {}
    for key, value in pairs:
        if key in result:
            raise ArtifactError("Duplicate artifact manifest field")
        result[key] = value
    return result


def _load_manifest(data: bytes) -> dict:
    if len(data) > MAX_MANIFEST_BYTES:
        raise ArtifactError("Artifact manifest exceeds size limit")
    try:
        value = json.loads(data, object_pairs_hook=_unique_object)
    except (UnicodeError, json.JSONDecodeError, RecursionError, ValueError):
        raise ArtifactError("Invalid artifact manifest encoding") from None
    return validate_manifest(value)


def _copy_bounded(source: BinaryIO, target: BinaryIO | None, limit: int) -> str:
    digest = hashlib.sha256()
    count = 0
    while chunk := source.read(min(CHUNK_BYTES, limit - count + 1)):
        count += len(chunk)
        if count > limit:
            raise ArtifactError("Artifact exceeds size limit")
        digest.update(chunk)
        if target is not None:
            target.write(chunk)
    return digest.hexdigest()


def _open_regular(path: Path) -> BinaryIO:
    # The isolated deployment job owns paths; refuse symlinks and special files
    # even when a caller accidentally points this at an unexpected local path.
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    if not stat.S_ISREG(os.fstat(descriptor).st_mode):
        os.close(descriptor)
        raise ArtifactError("Artifact input is not a regular file")
    return os.fdopen(descriptor, "rb")


def pack_manifest(archive: Path, profile: str, sha: str, run_id: int, run_attempt: int) -> dict:
    manifest = validate_manifest(
        {
            "schema_version": 1,
            "profile": profile,
            "source_sha": sha,
            "run_id": run_id,
            "run_attempt": run_attempt,
            "archive_sha256": "0" * 64,
        }
    )
    try:
        with _open_regular(archive) as source:
            manifest["archive_sha256"] = _copy_bounded(source, None, MAX_ARCHIVE_BYTES)
    except OSError:
        raise ArtifactError("Cannot read artifact archive") from None
    return manifest


def read_preview_zip(
    path: Path,
    expected_sha: str,
    expected_run_id: int,
    expected_run_attempt: int,
    output_archive: Path,
    *,
    archive_name: str = ARCHIVE_NAME,
    expected_profile: str = "pr",
) -> dict:
    """Unwrap precisely the two expected GitHub artifact files, without extraction."""
    if (expected_profile, archive_name) not in {
        ("pr", ARCHIVE_NAME), ("auth-release", AUTH_ARCHIVE_NAME),
    }:
        raise ArtifactError("Unsupported artifact archive/profile pair")
    if not _hex(expected_sha, 40) or not all(
        _positive_integer(value) for value in (expected_run_id, expected_run_attempt)
    ):
        raise ArtifactError("Invalid expected artifact identity")
    staged: Path | None = None
    try:
        with _open_regular(path) as source:
            # Bound the central directory before ZipFile reads it into memory.
            if os.fstat(source.fileno()).st_size > MAX_ARCHIVE_BYTES + MAX_MANIFEST_BYTES + 4096:
                raise ArtifactError("Artifact ZIP exceeds size limit")
            _check_zip_directory(source)
            with zipfile.ZipFile(source) as bundle:
                entries = bundle.infolist()
                if len(entries) != 2 or {entry.filename for entry in entries} != {
                    archive_name,
                    MANIFEST_NAME,
                }:
                    raise ArtifactError("Unexpected artifact ZIP entries")
                for entry in entries:
                    unix_type = stat.S_IFMT(entry.external_attr >> 16)
                    if (
                        entry.is_dir()
                        or entry.orig_filename != entry.filename
                        or entry.external_attr & 0x10
                        or unix_type not in (0, stat.S_IFREG)
                        or entry.flag_bits & 1
                        or entry.compress_type not in (zipfile.ZIP_STORED, zipfile.ZIP_DEFLATED)
                    ):
                        raise ArtifactError("Unsupported artifact ZIP entry")
                    limit = MAX_MANIFEST_BYTES if entry.filename == MANIFEST_NAME else MAX_ARCHIVE_BYTES
                    if entry.file_size > limit:
                        raise ArtifactError("Artifact ZIP entry exceeds size limit")
                manifest = _load_manifest(bundle.read(MANIFEST_NAME))
                if (
                    manifest["profile"] != expected_profile
                    or manifest["source_sha"] != expected_sha
                    or manifest["run_id"] != expected_run_id
                    or manifest["run_attempt"] != expected_run_attempt
                ):
                    raise ArtifactError("Artifact identity does not match authorized preview")
                if output_archive.exists() or output_archive.is_symlink():
                    raise ArtifactError("Artifact output already exists")
                descriptor, name = tempfile.mkstemp(prefix=".pages-archive-", dir=output_archive.parent)
                staged = Path(name)
                with os.fdopen(descriptor, "wb") as target, bundle.open(archive_name) as archive:
                    actual_digest = _copy_bounded(archive, target, MAX_ARCHIVE_BYTES)
                if actual_digest != manifest["archive_sha256"]:
                    raise ArtifactError("Artifact archive digest mismatch")
                # A hard link publishes without overwriting an existing path.
                os.link(staged, output_archive)
                staged.unlink()
                staged = None
                return manifest
    except (OSError, EOFError, RuntimeError, zipfile.BadZipFile, NotImplementedError, zlib.error):
        raise ArtifactError("Cannot read artifact ZIP") from None
    finally:
        if staged is not None:
            staged.unlink(missing_ok=True)


def _check_zip_directory(source: BinaryIO) -> None:
    # The artifact has exactly two small central-directory entries. Inspect the
    # fixed-size end record before ZipFile allocates its list of entries. ZIP64
    # and multipart archives are unnecessary below the 200 MiB artifact limit.
    size = os.fstat(source.fileno()).st_size
    source.seek(max(0, size - 65_557))
    tail = source.read(65_557)
    offset = tail.rfind(b"PK\x05\x06")
    if offset < 0 or len(tail) - offset < 22:
        raise ArtifactError("Invalid artifact ZIP directory")
    _, disk, directory_disk, disk_entries, entries, directory_size, directory_offset, comment_size = struct.unpack(
        "<4s4H2LH", tail[offset : offset + 22]
    )
    end_offset = max(0, size - 65_557) + offset
    if (
        disk != 0
        or directory_disk != 0
        or disk_entries != 2
        or entries != 2
        or directory_size > 4096
        or directory_offset + directory_size != end_offset
        or offset + 22 + comment_size != len(tail)
    ):
        raise ArtifactError("Unexpected artifact ZIP directory")
    source.seek(0)


class _ExpandedReader:
    """Bound all decompressed tar bytes, including extension headers and padding."""

    def __init__(self, source: BinaryIO) -> None:
        self.source = source
        self.count = 0

    def read(self, size: int = -1) -> bytes:
        remaining = MAX_EXPANDED_BYTES - self.count
        data = self.source.read(min(size if size >= 0 else remaining + 1, remaining + 1))
        self.count += len(data)
        if self.count > MAX_EXPANDED_BYTES:
            raise ArtifactError("Expanded artifact exceeds size limit")
        return data


def _static_path(name: str, *, is_directory: bool = False) -> str:
    if name.startswith("./"):
        name = name[2:]
    # Trailing slash is the conventional tar directory spelling. Every other
    # path component must be literal, unambiguous and safe on Linux and macOS.
    name = name.removesuffix("/")
    if (
        not name
        or name.startswith("/")
        or "\\" in name
        or ":" in name
        or any(unicodedata.category(character).startswith("C") for character in name)
        or unicodedata.normalize("NFC", name) != name
        or len(name.encode("utf-8")) > 4096
    ):
        raise ArtifactError("Unsafe static artifact path")
    parts = name.split("/")
    if any(
        not part
        # Mobile association documents live in this standard public directory.
        # The exception applies only to its exact root spelling, never another
        # dot-prefixed component or a regular file named .well-known.
        or (
            part.startswith(".")
            and not (
                index == 0
                and part == ".well-known"
                and (len(parts) > 1 or is_directory)
            )
        )
        or len(part.encode("utf-8")) > 255
        or part.casefold() in RESERVED_COMPONENTS
        for index, part in enumerate(parts)
    ):
        raise ArtifactError("Forbidden static artifact path")
    return name


def extract_static(archive: Path, destination: Path, expected_digest: str) -> dict[str, str]:
    """Verify and manually extract static-only files into a new or empty directory.

    Returns relative file paths mapped to SHA-256 values. Never execute or
    interpret JavaScript, HTML, configuration, archive modes or ownership.
    Failed validation leaves the requested destination untouched.
    """
    if not _hex(expected_digest, 64):
        raise ArtifactError("Invalid expected artifact digest")
    staged: Path | None = None
    try:
        if destination.is_symlink() or (
            destination.exists() and (not destination.is_dir() or any(destination.iterdir()))
        ):
            raise ArtifactError("Static destination must be new or empty")
        with _open_regular(archive) as source:
            if _copy_bounded(source, None, MAX_ARCHIVE_BYTES) != expected_digest:
                raise ArtifactError("Artifact archive digest mismatch")
            source.seek(0)
            staged = Path(tempfile.mkdtemp(prefix=".pages-static-", dir=destination.parent))
            hashes: dict[str, str] = {}
            seen: set[str] = set()
            spellings: dict[str, str] = {}
            entries = 0
            with gzip.GzipFile(fileobj=source) as compressed:
                expanded = _ExpandedReader(compressed)
                with tarfile.open(fileobj=expanded, mode="r|") as bundle:
                    for member in bundle:
                        entries += 1
                        if entries > MAX_ENTRIES:
                            raise ArtifactError("Static artifact has too many entries")
                        if (
                            not (member.isdir() or member.isreg())
                            or member.issparse()
                            or any(key.startswith("GNU.sparse") for key in member.pax_headers)
                            or member.size < 0
                            or member.size > MAX_FILE_BYTES
                            or (member.isdir() and member.size != 0)
                            or (not member.isdir() and member.name.endswith("/"))
                        ):
                            raise ArtifactError("Unsupported static artifact entry")
                        name = _static_path(member.name, is_directory=member.isdir())
                        if name in seen:
                            raise ArtifactError("Duplicate static artifact path")
                        seen.add(name)
                        parts = name.split("/")
                        for index in range(1, len(parts) + 1):
                            prefix = "/".join(parts[:index])
                            folded = prefix.casefold()
                            if folded in spellings and spellings[folded] != prefix:
                                raise ArtifactError("Conflicting static artifact path spelling")
                            spellings[folded] = prefix
                        target = staged.joinpath(*parts)
                        target.parent.mkdir(mode=0o755, parents=True, exist_ok=True)
                        if member.isdir():
                            target.mkdir(mode=0o755, exist_ok=True)
                        else:
                            payload = bundle.extractfile(member)
                            if payload is None:
                                raise ArtifactError("Missing static artifact file data")
                            descriptor = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o644)
                            with os.fdopen(descriptor, "wb") as output, payload:
                                hashes[name] = _copy_bounded(payload, output, MAX_FILE_BYTES)
                            if target.stat().st_size != member.size:
                                raise ArtifactError("Truncated static artifact file")
                            target.chmod(0o644)
                # tar ends before gzip; check the trailer and bound any remaining
                # decompressed data so padding cannot hide a compression bomb.
                while expanded.read(CHUNK_BYTES):
                    pass
            if "index.html" not in hashes:
                raise ArtifactError("Static artifact is missing root index.html")
            for root, directories, _ in os.walk(staged):
                Path(root).chmod(0o755)
                for directory in directories:
                    (Path(root) / directory).chmod(0o755)
            os.rename(staged, destination)
            staged = None
            return hashes
    except (OSError, EOFError, tarfile.TarError, RecursionError, UnicodeError, zlib.error):
        raise ArtifactError("Cannot unpack static artifact") from None
    finally:
        if staged is not None:
            shutil.rmtree(staged)


def main() -> int:
    class SafeArgumentParser(argparse.ArgumentParser):
        def error(self, message: str) -> None:
            self.exit(2, "Invalid Pages artifact command arguments\n")

    parser = SafeArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    manifest = commands.add_parser("manifest", help="Write metadata for a built web archive")
    manifest.add_argument("--archive", required=True, type=Path)
    manifest.add_argument("--profile", required=True, choices=("pr", "release", "auth-release"))
    manifest.add_argument("--sha", required=True)
    manifest.add_argument("--run-id", required=True, type=int)
    manifest.add_argument("--run-attempt", required=True, type=int)
    manifest.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    try:
        value = pack_manifest(args.archive, args.profile, args.sha, args.run_id, args.run_attempt)
        args.output.write_text(json.dumps(value, sort_keys=True) + "\n", encoding="utf-8")
    except (ArtifactError, OSError):
        print("Cannot create Pages artifact manifest", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
