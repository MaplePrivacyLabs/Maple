#!/usr/bin/env python3
"""Recover trusted Cargo dependency caches through the Actions artifact service.

This helper never uploads, executes restored files, or changes Cargo's freshness
checks. Native Cargo caches remain an independent read source. Only successful
push/master platform jobs can supply an artifact accepted by restore.
"""

from __future__ import annotations

import argparse
from datetime import datetime, timedelta, timezone
import gzip
import hashlib
import io
import json
import math
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import sys
import tarfile
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import zipfile


SCHEMA = 1
WORKFLOWS = {"agent-ci.yml", "agent-desktop-build.yml"}
# A clean hosted target is ~4.4 GB before dependency pruning, plus ~4.2 GB of
# public sources. Its prior zstd archive was already ~2.95 GB; allow gzip-1
# headroom while retaining bounded downloads and three-day workflow retention.
MAX_ARCHIVE = 6 * 1024**3
MAX_UNPACKED = 16 * 1024**3
MAX_FILE = 3 * 1024**3
MAX_FILES = 300_000
MAX_MANIFEST = 512 * 1024
MAX_CANDIDATES = 10
MAX_AGE_DAYS = 7
CHUNK = 1024 * 1024
HASH = re.compile(r"[0-9a-f]{64}\Z")
SHA = re.compile(r"[0-9a-f]{40}\Z")
REPOSITORY = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+\Z")
PACKAGE = re.compile(r"[A-Za-z0-9_-]+\Z")
RESERVED = {"CON", "PRN", "AUX", "NUL", *(f"COM{i}" for i in range(1, 10)), *(f"LPT{i}" for i in range(1, 10))}
LIB_EXTENSIONS = {".rlib", ".rmeta", ".d", ".so", ".dylib", ".dll", ".lib", ".a", ".pdb", ".exp",
                  ".dll.lib", ".dll.exp", ".dll.pdb"}
# These source repositories were independently reviewed as public. A GitHub URL
# and pinned commit alone do not establish that cached repository data is public.
REVIEWED_GIT_REPOSITORIES = {
    "1password/cpace", "mapleprivacylabs/rusqlite", "proptest-rs/proptest", "trycua/cua",
    "zed-industries/font-kit", "zed-industries/scap", "zed-industries/wasm_thread",
    "zed-industries/xim-rs", "zed-industries/zed",
}


class Rejected(ValueError):
    """An untrusted, incompatible, unsafe, or oversized cache is not usable."""


def sha256(path: Path) -> str:
    result = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(CHUNK), b""):
            result.update(chunk)
    return result.hexdigest()


def json_read(path: Path, limit: int = MAX_MANIFEST):
    if path.stat().st_size > limit:
        raise Rejected("JSON document exceeds the cache limit")
    return json.loads(path.read_text(encoding="utf-8"))


def environment(path: Path):
    value = json_read(path)
    compatibility = value.get("compatibility", "")
    payload = {key: item for key, item in value.items() if key not in {"compatibility", "legacy_compatibility", "legacy_identity"}}
    calculated = hashlib.sha256(json.dumps(payload, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
    if not HASH.fullmatch(compatibility) or compatibility != calculated:
        raise Rejected("actual compiler/native compatibility document is invalid")
    if value.get("profile") not in ("ci", "provider", "release"):
        raise Rejected("unsupported Cargo cache profile")
    for key in ("platform", "architecture"):
        if not re.fullmatch(r"[A-Za-z0-9_-]+", value.get(key, "")):
            raise Rejected("invalid cache platform identity")
    return value


def artifact_name(identity, dependency=""):
    if identity["profile"] == "provider" and not HASH.fullmatch(dependency):
        raise Rejected("provider archives require the verified test-only dependency fingerprint")
    if dependency and not HASH.fullmatch(dependency):
        raise Rejected("invalid dependency fingerprint")
    suffix = f"-{dependency}" if dependency else ""
    return f"maple-agent-deps-v{SCHEMA}-{identity['profile']}-{identity['platform']}-{identity['architecture']}-{identity['compatibility']}{suffix}"


def path_parts(name: str):
    path = PurePosixPath(name)
    if not name or not path.parts or path.is_absolute() or path.as_posix() != name:
        raise Rejected("noncanonical archive path")
    for part in path.parts:
        if part in (".", "..") or any(ord(c) < 32 for c in part) or any(c in part for c in "\\:"):
            raise Rejected("unsafe archive path")
        if part.endswith((".", " ")) or part.split(".")[0].upper() in RESERVED:
            raise Rejected("archive path aliases a Windows special name")
    return path.parts


def trusted_producer(job_name, workflow):
    repository = os.environ.get("GITHUB_REPOSITORY", "")
    if not REPOSITORY.fullmatch(repository):
        raise Rejected("a repository-scoped Actions context is required")
    if os.environ.get("GITHUB_EVENT_NAME") != "push" or os.environ.get("GITHUB_REF") != "refs/heads/master":
        raise Rejected("only push/master jobs may prepare dependency archives")
    head = os.environ.get("GITHUB_SHA", "")
    if not SHA.fullmatch(head):
        raise Rejected("producer commit identity is missing")
    run_id, attempt = int(os.environ["GITHUB_RUN_ID"]), int(os.environ["GITHUB_RUN_ATTEMPT"])
    if run_id <= 0 or attempt <= 0 or not job_name or workflow not in WORKFLOWS:
        raise Rejected("producer run, job or workflow identity is invalid")
    return {"repository": repository, "run_id": run_id, "run_attempt": attempt,
            "head_sha": head, "job_name": job_name, "workflow": workflow}


def packages(metadata):
    """Use Cargo source identity, never workspace-directory location, as authority."""
    public, local = [], set()
    for package in metadata["packages"]:
        source = package.get("source")
        names = {package["name"], *(target["name"] for target in package.get("targets", [])
                  if set(target["kind"]) & {"lib", "rlib", "proc-macro", "dylib", "staticlib", "cdylib"})}
        if not all(PACKAGE.fullmatch(name) for name in names):
            raise Rejected("invalid Cargo package/target name")
        if source is None:
            local.update(name.replace("-", "_") for name in names)
            local.add(package["name"])
            continue
        if source.startswith("registry+"):
            allowed = source == "registry+https://github.com/rust-lang/crates.io-index"
        elif source.startswith("git+"):
            url = urllib.parse.urlsplit(source[4:])
            repository = url.path.strip("/").casefold().removesuffix(".git")
            allowed = (url.scheme == "https" and url.hostname == "github.com" and not url.username
                       and not url.password and set(urllib.parse.parse_qs(url.query)) <= {"rev", "tag", "branch"}
                       and SHA.fullmatch(url.fragment) is not None and repository in REVIEWED_GIT_REPOSITORIES)
        else:
            allowed = False
        if allowed:
            public.append(package)
        else:
            # Unreviewed/private sources must not leak through target artifacts
            # or through name collisions with an otherwise reviewed dependency.
            local.update(name.replace("-", "_") for name in names)
            local.add(package["name"])
    package_names = {package["name"] for package in public} - local
    targets = {target["name"].replace("-", "_") for package in public
               for target in package.get("targets", [])
               if set(target["kind"]) & {"lib", "rlib", "proc-macro", "dylib", "staticlib", "cdylib"}} - local
    return public, package_names, targets


def dependency_graph(metadata, public):
    features = {node["id"]: sorted(node.get("features", [])) for node in metadata.get("resolve", {}).get("nodes", [])}
    selected = sorted((package["name"], package["version"], package["source"], features.get(package["id"], [])) for package in public)
    return hashlib.sha256(json.dumps(selected, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def receipt_reader(job_name):
    return {"run_id": os.environ.get("GITHUB_RUN_ID"), "run_attempt": os.environ.get("GITHUB_RUN_ATTEMPT"), "job_name": job_name}


def receipt_location(receipt, target, cargo_home):
    if receipt and (receipt.is_symlink() or receipt.resolve().is_relative_to(target.resolve()) or receipt.resolve().is_relative_to(cargo_home.resolve())):
        raise Rejected("artifact receipts must be fresh job-local files outside restored Cargo caches")


def source_roots(public, cargo_home):
    roots = set()
    for package in public:
        try:
            relative = Path(package["manifest_path"]).resolve().relative_to(cargo_home)
        except ValueError:
            continue
        parts = relative.parts
        if len(parts) >= 5 and parts[:2] == ("registry", "src"):
            roots.add(PurePosixPath(*parts[:4]).as_posix())
        elif len(parts) >= 5 and parts[:2] == ("git", "checkouts"):
            roots.add(PurePosixPath(*parts[:4]).as_posix())
            roots.add(PurePosixPath("git", "db", parts[2]).as_posix())
    return roots


def tree_files(root: Path, skip=None):
    """Dereference internal links; never put link entries in the portable archive."""
    if not root.exists():
        return
    boundary = root.resolve()

    def walk(path, logical, ancestors):
        if skip and skip(logical):
            return
        selected = path.resolve()
        if not selected.is_relative_to(boundary):
            raise Rejected("dependency tree contains a link outside its source/output root")
        if selected.is_dir():
            if selected in ancestors or len(ancestors) >= 64:
                raise Rejected("dependency tree contains a directory-link cycle")
            for child in sorted(selected.iterdir()):
                yield from walk(child, logical / child.name, ancestors | {selected})
        elif selected.is_file():
            yield selected, logical
        else:
            raise Rejected("dependency tree contains a nonregular file")
    yield from walk(root, PurePosixPath(), set())


def sanitized_git(relative, source, cargo_home):
    """Return None to omit hooks, or bytes to replace local Git configuration."""
    parts = relative.parts
    metadata = parts[:2] == ("git", "db") or ".git" in parts
    if not metadata:
        return False
    if parts[0] == "git" and parts[-1] == ".git":
        raise Rejected("regular .git pointers cannot enter a portable Cargo source cache")
    if "hooks" in parts or source.name.endswith(".lock") or source.name == "config.worktree":
        return None
    if source.name == "config":
        bare = parts[:2] == ("git", "db")
        return f"[core]\nrepositoryformatversion = 0\nbare = {'true' if bare else 'false'}\nfilemode = false\n".encode()
    if parts[-3:] == ("objects", "info", "alternates"):
        lines = source.read_text(encoding="utf-8").splitlines()
        replacements = []
        for line in lines:
            destination = (source.parent / line).resolve()
            if not destination.is_relative_to(cargo_home / "git/db") or not destination.is_dir():
                raise Rejected("Git object alternates escape the cached public repositories")
            replacements.append(os.path.relpath(destination, source.parent).replace(os.sep, "/"))
        return ("\n".join(replacements) + "\n").encode()
    return False


def cargo_records(public, cargo_home):
    roots = source_roots(public, cargo_home)
    for root in sorted(roots):
        if not (cargo_home / root).resolve().is_relative_to(cargo_home):
            raise Rejected("Cargo source storage escapes its declared home")
        def skip(relative):
            return "hooks" in relative.parts and (root.startswith("git/db/") or ".git" in relative.parts)
        for source, relative in tree_files(cargo_home / root, skip):
            relative = PurePosixPath(root) / relative
            replacement = sanitized_git(relative, source, cargo_home)
            if replacement is not None:
                yield source, "cargo/" + relative.as_posix(), replacement
    # Registry archives and sparse index records are selected by Cargo metadata,
    # never by copying the Cargo home (which contains credentials and binaries).
    for package in public:
        if not package["source"].startswith("registry+"):
            continue
        try:
            parts = Path(package["manifest_path"]).resolve().relative_to(cargo_home).parts
        except ValueError:
            continue
        if parts[:2] != ("registry", "src") or len(parts) < 5:
            continue
        registry = parts[2]
        name = package["name"].lower()
        index = f"1/{name}" if len(name) == 1 else f"2/{name}" if len(name) == 2 else f"3/{name[0]}/{name}" if len(name) == 3 else f"{name[:2]}/{name[2:4]}/{name}"
        for relative in (f"registry/cache/{registry}/{package['name']}-{package['version']}.crate",
                         f"registry/index/{registry}/.cache/{index}"):
            source = cargo_home / relative
            if source.is_file() and not source.is_symlink():
                yield source, "cargo/" + relative, False
    for registry in {PurePosixPath(root).parts[2] for root in roots if root.startswith("registry/src/")}:
        source = cargo_home / "registry/index" / registry / "config.json"
        if source.is_file() and not source.is_symlink():
            config = json_read(source)
            # Do not persist mirror URLs, auth configuration, or private fields.
            url = urllib.parse.urlsplit(config.get("dl", ""))
            if url.scheme == "https" and url.hostname in {"static.crates.io", "crates.io"} and not url.username and not url.password and not url.query:
                cleaned = json.dumps({"dl": config["dl"], "api": "https://crates.io"}).encode()
                yield source, f"cargo/registry/index/{registry}/config.json", cleaned


def profile_part(parts):
    for index, part in enumerate(parts):
        if part in ("debug", "release") and index <= 2:
            prefix = parts[:index]
            if all(item == "build" or re.fullmatch(r"[A-Za-z0-9_]+(?:-[A-Za-z0-9_]+){2,}", item) for item in prefix):
                return parts[index + 1:]
    return ()


def target_allowed(parts, names, targets):
    tail = profile_part(parts)
    if len(tail) < 2:
        return False
    if tail[0] in {"build", ".fingerprint"}:
        package, separator, fingerprint = tail[1].rpartition("-")
        return bool(separator and package in names and re.fullmatch(r"[0-9a-f]{8,64}", fingerprint)
                    and "incremental" not in tail)
    if tail[0] == "deps" and len(tail) == 2:
        name = tail[1]
        extension = next((item for item in sorted(LIB_EXTENSIONS, key=len, reverse=True) if name.endswith(item)), "")
        stem = name[:-len(extension)] if extension else name
        stem, separator, fingerprint = stem.rpartition("-")
        return bool(extension in LIB_EXTENSIONS and separator and re.fullmatch(r"[0-9a-f]{8,64}", fingerprint)
                    and (stem in targets or stem.removeprefix("lib") in targets))
    return False


def target_records(target, names, targets):
    # Do not traverse app bundles, notices, incremental data or other unrelated
    # target outputs merely to filter them later (macOS bundles contain links).
    bases = [target, target / "build"]
    for base in tuple(bases):
        if base.is_dir():
            bases.extend(child for child in base.iterdir() if child.is_dir() and re.fullmatch(r"[A-Za-z0-9_]+(?:-[A-Za-z0-9_]+){2,}", child.name))
    for base in bases:
        if base.exists() and (base.is_symlink() or not base.resolve().is_relative_to(target)):
            raise Rejected("Cargo build layout escapes its target directory")
        for profile in ("debug", "release"):
            if (base / profile).is_symlink():
                raise Rejected("Cargo profile directory is a symlink")
            for kind in ("deps", ".fingerprint", "build"):
                directory = base / profile / kind
                if not directory.is_dir() or directory.is_symlink():
                    continue
                for child in sorted(directory.iterdir()):
                    relative = PurePosixPath(*child.relative_to(target).parts)
                    if not target_allowed(relative.parts, names, targets):
                        continue
                    if kind == "deps":
                        if not child.is_file() or child.is_symlink():
                            raise Rejected("dependency library is not an ordinary file")
                        yield child, "target/" + relative.as_posix(), False
                    else:
                        if not child.is_dir() or child.is_symlink() or not child.resolve().is_relative_to(target):
                            raise Rejected("dependency build output escapes its target directory")
                        for source, inner in tree_files(child):
                            yield source, "target/" + (relative / inner).as_posix(), False


def pack(identity, metadata, target, cargo_home, output, job_name, workflow, dependency="", receipt=None):
    started = time.monotonic()
    producer = trusted_producer(job_name, workflow)
    name = artifact_name(identity, dependency)
    public, names, targets = packages(metadata)
    graph = dependency_graph(metadata, public)
    receipt_location(receipt, target, cargo_home)
    if receipt and receipt.is_file():
        previous = json_read(receipt)
        if (previous.get("compatibility") == identity["compatibility"] and previous.get("dependency_fingerprint") == dependency
                and previous.get("reader") == receipt_reader(job_name)
                and previous.get("dependency_graph") == graph
                and datetime.now(timezone.utc) - timedelta(days=1) < instant(previous["created_at"]) <= datetime.now(timezone.utc)):
            return {"needs_upload": False, "artifact_name": name, "dependency_graph": graph,
                    "source_artifact_id": previous["artifact_id"], "seed_created_at": previous["created_at"]}
    if target.is_symlink() or cargo_home.is_symlink():
        raise Rejected("Cargo cache roots must not be symlinks")
    target, cargo_home = target.resolve(), cargo_home.resolve()
    if target == cargo_home or target.is_relative_to(cargo_home) or cargo_home.is_relative_to(target):
        raise Rejected("Cargo home and target roots must be separate")
    if output.resolve().is_relative_to(target) or output.resolve().is_relative_to(cargo_home):
        raise Rejected("dependency bundle must be outside its input cache roots")
    if output.exists() and any(output.iterdir()):
        raise Rejected("dependency bundle output must be new or empty")
    output.mkdir(parents=True, exist_ok=True)
    archive = output / "dependencies.tar.gz"
    entries, total, seen = 0, 0, {}
    records = cargo_records(public, cargo_home)

    def all_records():
        yield from records
        yield from target_records(target, names, targets)

    with archive.open("wb") as raw, gzip.GzipFile(filename="", fileobj=raw, mode="wb", compresslevel=1, mtime=0) as compressed, tarfile.open(fileobj=compressed, mode="w|", format=tarfile.PAX_FORMAT) as stream:
        for source, relative, replacement in all_records():
            path_parts(relative)
            if relative.casefold() in seen:
                if seen[relative.casefold()] != relative:
                    raise Rejected("dependency inputs contain case-alias paths")
                continue
            seen[relative.casefold()] = relative
            attributes = source.stat()
            size = len(replacement) if replacement is not False else attributes.st_size
            entries, total = entries + 1, total + size
            if entries > MAX_FILES or total > MAX_UNPACKED or size > MAX_FILE or raw.tell() > MAX_ARCHIVE:
                raise Rejected("dependency archive exceeds bounded size/file limits")
            member = tarfile.TarInfo(relative)
            member.size, member.mtime = size, attributes.st_mtime
            member.mode = 0o755 if attributes.st_mode & 0o111 else 0o644
            if replacement is False:
                with source.open("rb") as content:
                    stream.addfile(member, content)
            else:
                stream.addfile(member, io.BytesIO(replacement))
    if not entries or archive.stat().st_size > MAX_ARCHIVE:
        raise Rejected("dependency archive is empty or exceeds its compressed-size limit")
    manifest = {"schema": SCHEMA, "needs_upload": True, "artifact_name": name, "compatibility": identity["compatibility"],
                "profile": identity["profile"], "platform": identity["platform"], "architecture": identity["architecture"],
                "dependency_fingerprint": dependency, "dependency_graph": graph, "producer": producer,
                "package_names": sorted(names), "target_names": sorted(targets),
                "archive_sha256": sha256(archive), "archive_bytes": archive.stat().st_size,
                "unpacked_bytes": total, "files": entries, "pack_seconds": round(time.monotonic() - started, 3)}
    (output / "manifest.json").write_text(json.dumps(manifest, sort_keys=True, indent=2) + "\n", encoding="utf-8")
    return manifest


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, fp, code, message, headers, newurl):
        return None


class GitHub:
    def __init__(self, repository, token):
        if not REPOSITORY.fullmatch(repository) or not token:
            raise Rejected("repository-scoped Actions read access is required")
        self.repository, self.token = repository, token

    def request(self, endpoint):
        return urllib.request.Request(f"https://api.github.com/repos/{self.repository}/{endpoint}",
                                      headers={"Authorization": f"Bearer {self.token}", "Accept": "application/vnd.github+json",
                                               "X-GitHub-Api-Version": "2022-11-28", "User-Agent": "Maple-dependency-cache"})

    def get(self, endpoint):
        with urllib.request.build_opener(NoRedirect()).open(self.request(endpoint), timeout=30) as response:
            content = response.read(4 * 1024 * 1024 + 1)
        if len(content) > 4 * 1024 * 1024:
            raise Rejected("Actions metadata exceeds its bounded response size")
        return json.loads(content)

    def download(self, artifact_id, destination, digest):
        try:
            response = urllib.request.build_opener(NoRedirect()).open(self.request(f"actions/artifacts/{artifact_id}/zip"), timeout=30)
        except urllib.error.HTTPError as error:
            if error.code not in (301, 302, 303, 307, 308):
                raise
            location = error.headers.get("Location", "")
            if error.fp is not None:
                error.close()
            url = urllib.parse.urlsplit(location)
            allowed = (".blob.core.windows.net", ".actions.githubusercontent.com", ".githubusercontent.com")
            if url.scheme != "https" or not url.hostname or not url.hostname.endswith(allowed) or url.username or url.password:
                raise Rejected("Actions download redirect has an unexpected HTTPS origin")
            # Never carry the GitHub Authorization header to the signed blob URL,
            # including a second redirect to a different origin.
            response = urllib.request.build_opener(NoRedirect()).open(urllib.request.Request(location), timeout=60)
        observed, count = hashlib.sha256(), 0
        with response, destination.open("wb") as output:
            length = response.headers.get("Content-Length")
            if length and int(length) > MAX_ARCHIVE + MAX_MANIFEST + 1024**2:
                raise Rejected("artifact download exceeds its size limit")
            while chunk := response.read(CHUNK):
                count += len(chunk)
                if count > MAX_ARCHIVE + MAX_MANIFEST + 1024**2:
                    raise Rejected("artifact download exceeds its size limit")
                observed.update(chunk)
                output.write(chunk)
        if observed.hexdigest() != digest:
            raise Rejected("Actions artifact ZIP SHA256 differs from API digest")
        return count


def instant(value):
    return datetime.fromisoformat(value.replace("Z", "+00:00"))


def source_run(api, artifact, name, workflow, now):
    """Establish immutable run/repository identity before downloading any ZIP."""
    if (artifact.get("name") != name or artifact.get("expired") or not isinstance(artifact.get("id"), int)
            or artifact.get("size_in_bytes", MAX_ARCHIVE + 1) > MAX_ARCHIVE + MAX_MANIFEST + 1024**2
            or not re.fullmatch(r"sha256:[0-9a-f]{64}", artifact.get("digest", ""))):
        raise Rejected("artifact identity, digest, expiry or size is invalid")
    created = instant(artifact["created_at"])
    if not now - timedelta(days=MAX_AGE_DAYS) <= created <= now + timedelta(minutes=5):
        raise Rejected("artifact is outside the bounded retention window")
    origin = artifact["workflow_run"]
    run = api.get(f"actions/runs/{int(origin['id'])}")
    if (run.get("id") != origin["id"] or run.get("repository", {}).get("full_name") != api.repository
            or run.get("head_repository", {}).get("full_name") != api.repository
            or run.get("event") != "push" or run.get("head_branch") != "master"
            or run.get("path") != f".github/workflows/{workflow}"
            or not SHA.fullmatch(run.get("head_sha", ""))
            or origin.get("head_sha") != run["head_sha"] or origin.get("head_branch") != "master"
            or not isinstance(run.get("repository", {}).get("id"), int)
            or not isinstance(run.get("head_repository", {}).get("id"), int)
            or origin.get("repository_id") != run.get("repository", {}).get("id")
            or origin.get("head_repository_id") != run.get("head_repository", {}).get("id")):
        raise Rejected("artifact was not produced by this repository's push/master workflow")
    if type(run.get("run_attempt")) is not int or run["run_attempt"] < 1:
        raise Rejected("producer run has an invalid attempt bound")
    return run


def source_proof(api, artifact, name, job_name, workflow, now, attempt=None, run=None):
    # The latest run attempt may rerun only another platform's failed job.
    # After the ZIP digest has bound its manifest, prove the declared original
    # producer attempt rather than invalidating an earlier successful seed.
    run = run or source_run(api, artifact, name, workflow, now)
    if attempt is None:
        attempt = run["run_attempt"]
    if type(attempt) is not int or not 1 <= attempt <= run["run_attempt"]:
        raise Rejected("manifest producer attempt is outside the trusted run bound")
    created = instant(artifact["created_at"])
    jobs = api.get(f"actions/runs/{run['id']}/attempts/{attempt}/jobs?per_page=100")
    if jobs.get("total_count", 101) > 100:
        raise Rejected("platform job proof exceeds the bounded jobs scan")
    selected = [job for job in jobs["jobs"] if job.get("name") == job_name]
    if len(selected) != 1 or selected[0].get("status") != "completed" or selected[0].get("conclusion") != "success":
        raise Rejected("the exact producer platform job did not complete successfully")
    job = selected[0]
    if not job.get("steps") or any(step.get("conclusion") not in ("success", "skipped") for step in job["steps"]):
        raise Rejected("producer job has incomplete or failed checks")
    if not instant(job["started_at"]) <= created <= instant(job["completed_at"]) + timedelta(seconds=30):
        raise Rejected("artifact did not originate inside the proven successful job attempt")
    return {"repository": api.repository, "run_id": int(run["id"]), "run_attempt": attempt,
            "head_sha": run["head_sha"], "job_name": job_name, "workflow": workflow}


def unzip(zip_path, directory):
    with zipfile.ZipFile(zip_path) as archive:
        members = archive.infolist()
        if len(members) != 2 or {item.filename for item in members} != {"manifest.json", "dependencies.tar.gz"}:
            raise Rejected("artifact ZIP must contain exactly its manifest and dependency archive")
        for member in members:
            mode = member.external_attr >> 16
            limit = MAX_MANIFEST if member.filename == "manifest.json" else MAX_ARCHIVE
            if (member.is_dir() or member.flag_bits & 1 or stat.S_IFMT(mode) not in (0, stat.S_IFREG)
                    or member.file_size < 0 or member.file_size > limit):
                raise Rejected("unsafe artifact ZIP entry")
            with archive.open(member) as source, (directory / member.filename).open("wb") as output:
                copied = 0
                while chunk := source.read(CHUNK):
                    copied += len(chunk)
                    if copied > member.file_size or copied > limit:
                        raise Rejected("artifact ZIP entry exceeds its declared size")
                    output.write(chunk)
            if copied != member.file_size:
                raise Rejected("artifact ZIP entry is truncated")


def member_destination(name, manifest, cargo_home, target):
    parts = path_parts(name)
    if parts[0] == "target" and target_allowed(parts[1:], manifest["package_names"], manifest["target_names"]):
        return target.joinpath(*parts[1:])
    if parts[0] == "cargo" and len(parts) >= 4:
        tail = parts[1:]
        registry = tail[:2] in (("registry", "src"), ("registry", "cache"), ("registry", "index"))
        git = tail[:2] in (("git", "db"), ("git", "checkouts"))
        if registry or git:
            if git and tail[-1] == ".git":
                raise Rejected("regular .git pointers cannot enter a portable Cargo source cache")
            if (".git" in tail or tail[:2] == ("git", "db")) and "hooks" in tail:
                raise Rejected("Git hooks cannot enter the dependency cache")
            return cargo_home.joinpath(*tail)
    raise Rejected("archive entry is outside the dependency-only namespaces")


def safe_destination(path, root):
    if any(parent.is_symlink() for parent in root.parents):
        raise Rejected("cache destination has a symlink ancestor")
    if root.is_symlink() or root.exists() and not root.is_dir():
        raise Rejected("cache destination root is not an ordinary directory")
    current = root
    for part in path.relative_to(root).parts:
        current /= part
        if current.is_symlink():
            raise Rejected("cache destination contains a symlink")
        if current != path and current.exists() and not current.is_dir():
            raise Rejected("cache destination parent is not a directory")
    if path.exists() and not path.is_file():
        raise Rejected("cache destination file has an unexpected type")


def tar_envelope(archive):
    """Bound physical TAR headers before tarfile can allocate PAX payloads."""
    headers, physical = 0, 0
    with gzip.open(archive, "rb") as stream:
        while True:
            header = stream.read(512)
            if not header or header == bytes(512):
                break
            if len(header) != 512:
                raise Rejected("truncated dependency TAR header")
            headers += 1
            if headers > MAX_FILES * 2:
                raise Rejected("dependency TAR has too many physical headers")
            encoded = header[124:136]
            try:
                size = int.from_bytes(encoded[1:], "big") if encoded[0] == 0x80 else int(encoded.rstrip(b"\0 ").lstrip(b" ") or b"0", 8)
            except ValueError as error:
                raise Rejected("invalid dependency TAR size") from error
            kind = header[156:157]
            if size < 0 or size > MAX_FILE or kind not in (b"0", b"\0", tarfile.XHDTYPE):
                raise Rejected("unsupported dependency TAR entry type/size")
            if kind == tarfile.XHDTYPE:
                if size > 64 * 1024:
                    raise Rejected("oversized dependency TAR extended metadata")
                payload = stream.read(size)
                remaining = payload
                while remaining:
                    try:
                        length = int(remaining.split(b" ", 1)[0])
                        record = remaining[:length]
                        key = record.split(b" ", 1)[1].split(b"=", 1)[0]
                    except (ValueError, IndexError) as error:
                        raise Rejected("invalid dependency TAR extended metadata") from error
                    if length <= 0 or length > len(remaining) or not record.endswith(b"\n") or key not in (b"path", b"mtime"):
                        raise Rejected("unsupported dependency TAR extended metadata")
                    remaining = remaining[length:]
                skipped = (512 - size % 512) % 512
            else:
                skipped = ((size + 511) // 512) * 512
            physical += 512 + ((size + 511) // 512) * 512
            if physical > MAX_UNPACKED + MAX_FILES * 2048:
                raise Rejected("dependency TAR exceeds its physical decompression budget")
            while skipped:
                chunk = stream.read(min(CHUNK, skipped))
                if not chunk:
                    raise Rejected("truncated dependency TAR contents")
                skipped -= len(chunk)


def unpack(directory, identity, producer, cargo_home, target, dependency=""):
    manifest = json_read(directory / "manifest.json")
    expected = artifact_name(identity, dependency)
    if (manifest.get("schema") != SCHEMA or manifest.get("artifact_name") != expected
            or manifest.get("compatibility") != identity["compatibility"]
            or manifest.get("dependency_fingerprint") != dependency or manifest.get("producer") != producer
            or any(manifest.get(key) != identity[key] for key in ("profile", "platform", "architecture"))):
        raise Rejected("dependency manifest does not match the actual environment and proven producer")
    if (type(manifest.get("files")) is not int or not 0 < manifest["files"] <= MAX_FILES
            or type(manifest.get("unpacked_bytes")) is not int or not 0 <= manifest["unpacked_bytes"] <= MAX_UNPACKED
            or not isinstance(manifest.get("package_names"), list) or not isinstance(manifest.get("target_names"), list)
            or not all(isinstance(name, str) and PACKAGE.fullmatch(name) for name in manifest["package_names"] + manifest["target_names"])
            or not HASH.fullmatch(manifest.get("dependency_graph", ""))):
        raise Rejected("dependency manifest has invalid size or package bounds")
    archive = directory / "dependencies.tar.gz"
    if (archive.stat().st_size > MAX_ARCHIVE or archive.stat().st_size != manifest["archive_bytes"]
            or sha256(archive) != manifest["archive_sha256"]):
        raise Rejected("dependency tar size or SHA256 differs from the trusted manifest")
    tar_envelope(archive)
    cargo_home, target = cargo_home.absolute(), target.absolute()
    scope = {"package_names": set(manifest["package_names"]), "target_names": set(manifest["target_names"])}
    seen, destinations, count, size = set(), [], 0, 0
    # Preflight every entry and every existing destination before writing any
    # cache file. extractall is deliberately never used for either archive.
    with tarfile.open(archive, mode="r|gz") as stream:
        for member in stream:
            if (not member.isreg() or member.issparse() or not 0 <= member.size <= MAX_FILE
                    or not math.isfinite(member.mtime) or not 0 <= member.mtime <= 4_102_444_800):
                raise Rejected("dependency tar may contain only ordinary nonsparse files")
            folded = member.name.casefold()
            if folded in seen:
                raise Rejected("dependency tar contains duplicate/case-alias paths")
            seen.add(folded)
            count, size = count + 1, size + member.size
            if count > MAX_FILES or size > MAX_UNPACKED:
                raise Rejected("dependency tar exceeds extraction bounds")
            destination = member_destination(member.name, scope, cargo_home, target)
            parts = PurePosixPath(member.name).parts
            metadata = parts[:3] == ("cargo", "git", "db") or ".git" in parts
            if metadata and parts[-1] in ("config", "config.worktree", "alternates") and member.size > 64 * 1024:
                raise Rejected("Git metadata exceeds its bounded size")
            root = target if member.name.startswith("target/") else cargo_home
            safe_destination(destination, root)
            destinations.append((member.name, destination))
    if count != manifest["files"] or size != manifest["unpacked_bytes"]:
        raise Rejected("dependency tar totals differ from the trusted manifest")
    staging = directory / "staged"
    staging.mkdir()
    with tarfile.open(archive, mode="r|gz") as stream:
        extracted = 0
        for member in stream:
            if extracted >= len(destinations):
                raise Rejected("dependency archive gained entries between verification passes")
            name, _ = destinations[extracted]
            extracted += 1
            if member.name != name:
                raise Rejected("dependency archive changed between verification passes")
            selected = staging.joinpath(*PurePosixPath(name).parts)
            selected.parent.mkdir(parents=True, exist_ok=True)
            with stream.extractfile(member) as source, selected.open("wb") as output:
                shutil.copyfileobj(source, output, CHUNK)
            if selected.stat().st_size != member.size:
                raise Rejected("dependency tar entry is truncated")
            parts = PurePosixPath(name).parts
            metadata = parts[:3] == ("cargo", "git", "db") or ".git" in parts
            if metadata and parts[-1] == "config":
                bare = parts[:3] == ("cargo", "git", "db")
                expected_config = f"[core]\nrepositoryformatversion = 0\nbare = {'true' if bare else 'false'}\nfilemode = false\n".encode()
                if selected.read_bytes() != expected_config:
                    raise Rejected("Git configuration was not sanitized by the dependency packer")
            if metadata and parts[-1] == "config.worktree":
                raise Rejected("Git worktree configuration cannot enter the dependency cache")
            os.chmod(selected, 0o755 if member.mode & 0o111 else 0o644)
            os.utime(selected, (member.mtime, member.mtime))
    if extracted != len(destinations):
        raise Rejected("dependency archive lost entries between verification passes")
    for name, _ in destinations:
        parts = PurePosixPath(name).parts
        selected = staging.joinpath(*parts)
        if parts[:3] in (("cargo", "git", "db"), ("cargo", "git", "checkouts")) and parts[-3:] == ("objects", "info", "alternates"):
            for line in selected.read_text(encoding="utf-8").splitlines():
                if not line or PurePosixPath(line).is_absolute() or "\\" in line or ":" in line or any(ord(c) < 32 for c in line):
                    raise Rejected("Git object alternates must be bounded relative paths")
                referenced = (selected.parent / line).resolve()
                if not referenced.is_relative_to((staging / "cargo/git/db").resolve()) or not referenced.is_dir():
                    raise Rejected("Git object alternates escape the restored public object storage")
        if parts[:3] == ("cargo", "registry", "index") and parts[-1] == "config.json":
            config = json_read(selected, 64 * 1024)
            url = urllib.parse.urlsplit(config.get("dl", ""))
            if (set(config) != {"dl", "api"} or config["api"] != "https://crates.io" or url.scheme != "https"
                    or url.hostname not in {"static.crates.io", "crates.io"} or url.username or url.password or url.query):
                raise Rejected("registry index configuration is not sanitized public configuration")
    for name, destination in destinations:
        root = target if name.startswith("target/") else cargo_home
        safe_destination(destination, root)
        destination.parent.mkdir(parents=True, exist_ok=True)
        staged = staging.joinpath(*PurePosixPath(name).parts)
        if destination.exists() and not os.access(destination, os.W_OK):
            # Cargo Git object storage can use read-only hardlinks. Never chmod
            # a restored destination through a potentially shared inode.
            if sha256(destination) == sha256(staged):
                continue
            raise Rejected("a different read-only dependency file already exists")
        os.replace(staged, destination)
    return manifest


def outputs(values):
    if os.environ.get("GITHUB_OUTPUT"):
        with Path(os.environ["GITHUB_OUTPUT"]).open("a", encoding="utf-8") as output:
            for key, value in values.items():
                output.write(f"{key}={value}\n")


def restore(api, identity, target, cargo_home, job_name, workflow, dependency="", receipt=None):
    started = time.monotonic()
    name = artifact_name(identity, dependency)
    receipt_location(receipt, target, cargo_home)
    candidates = api.get("actions/artifacts?" + urllib.parse.urlencode({"name": name, "per_page": MAX_CANDIDATES}))
    for listed in candidates.get("artifacts", [])[:MAX_CANDIDATES]:
        try:
            artifact = api.get(f"actions/artifacts/{int(listed['id'])}")
            now = datetime.now(timezone.utc)
            run = source_run(api, artifact, name, workflow, now)
            with tempfile.TemporaryDirectory(prefix="maple-dependency-restore-") as temporary:
                work = Path(temporary)
                transferred = api.download(artifact["id"], work / "artifact.zip", artifact["digest"].removeprefix("sha256:"))
                unzip(work / "artifact.zip", work)
                declared = json_read(work / "manifest.json").get("producer", {})
                attempt = declared.get("run_attempt")
                if type(attempt) is not int:
                    raise Rejected("manifest producer attempt is missing")
                proof = source_proof(api, artifact, name, job_name, workflow, now, attempt=attempt, run=run)
                manifest = unpack(work, identity, proof, cargo_home, target, dependency)
            if receipt:
                receipt.parent.mkdir(parents=True, exist_ok=True)
                receipt.write_text(json.dumps({"compatibility": identity["compatibility"], "dependency_fingerprint": dependency,
                                              "dependency_graph": manifest["dependency_graph"], "created_at": artifact["created_at"],
                                              "artifact_id": artifact["id"], "artifact_digest": artifact["digest"], "producer": proof,
                                              "reader": receipt_reader(job_name)}, sort_keys=True) + "\n", encoding="utf-8")
            result = {"restored": "true", "artifact-id": artifact["id"], "source-run-id": proof["run_id"],
                      "archive-bytes": manifest["archive_bytes"], "download-bytes": transferred,
                      "restore-seconds": round(time.monotonic() - started, 3), "seed-created-at": artifact["created_at"]}
            outputs(result)
            print(json.dumps(result, sort_keys=True))
            return True
        except (Rejected, OSError, EOFError, OverflowError, KeyError, TypeError, ValueError, tarfile.TarError, zipfile.BadZipFile) as error:
            print(f"Dependency artifact {listed.get('id', 'unknown')} rejected: {type(error).__name__}", file=sys.stderr)
    outputs({"restored": "false"})
    print("No compatible, proven successful master dependency artifact; Cargo will build cold")
    return False


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("pack", "restore"))
    parser.add_argument("--environment", type=Path, required=True)
    parser.add_argument("--target", type=Path, required=True)
    parser.add_argument("--cargo-home", type=Path, default=Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo"))))
    parser.add_argument("--job-name", required=True)
    parser.add_argument("--workflow", choices=sorted(WORKFLOWS), default="agent-ci.yml")
    parser.add_argument("--dependency-fingerprint", default="")
    parser.add_argument("--metadata", type=Path)
    parser.add_argument("--output-dir", type=Path)
    parser.add_argument("--receipt", type=Path, help="write restored artifact proof, or reuse that proof during pack")
    arguments = parser.parse_args()
    identity = environment(arguments.environment)
    if arguments.command == "pack":
        if not arguments.metadata or not arguments.output_dir:
            parser.error("pack requires --metadata and --output-dir")
        manifest = pack(identity, json_read(arguments.metadata, 64 * 1024**2), arguments.target,
                        arguments.cargo_home, arguments.output_dir, arguments.job_name,
                        arguments.workflow, arguments.dependency_fingerprint, arguments.receipt)
        outputs({"needs-upload": str(manifest["needs_upload"]).lower(), "packed": str(manifest["needs_upload"]).lower(),
                 "artifact-name": manifest["artifact_name"], "archive-bytes": manifest.get("archive_bytes", 0)})
        print(json.dumps({key: manifest[key] for key in ("needs_upload", "artifact_name", "archive_bytes", "unpacked_bytes", "files", "pack_seconds", "source_artifact_id", "seed_created_at") if key in manifest}, sort_keys=True))
    else:
        try:
            api = GitHub(os.environ.get("GITHUB_REPOSITORY", ""), os.environ.get("GH_TOKEN", os.environ.get("GITHUB_TOKEN", "")))
            restore(api, identity, arguments.target, arguments.cargo_home, arguments.job_name,
                    arguments.workflow, arguments.dependency_fingerprint, arguments.receipt)
        except (Rejected, OSError, ValueError, KeyError, TypeError) as error:
            outputs({"restored": "false"})
            print(f"Dependency artifact recovery unavailable ({type(error).__name__}); Cargo will build cold")


if __name__ == "__main__":
    try:
        main()
    except (Rejected, OSError, ValueError, KeyError, TypeError) as error:
        print(f"Dependency archive refused: {type(error).__name__}: {error}", file=sys.stderr)
        sys.exit(1)
