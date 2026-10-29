#!/usr/bin/env python3
"""Prepare, sign and conditionally publish independently delivered Agent updates.

Only Python's standard library is used. GitHub provenance is checked with gh;
cryptography uses the repository's pinned minisign, never downloaded executables.
"""

from __future__ import annotations

import argparse
import base64
from contextlib import contextmanager
from datetime import datetime, timezone
import gzip
import hashlib
import hmac
import importlib.util
import json
import os
from pathlib import Path
import plistlib
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import tarfile
import urllib.error
import urllib.parse
import urllib.request
from xml.parsers.expat import ExpatError
import zlib


REPOSITORY = "MaplePrivacyLabs/Maple"
WORKFLOW = ".github/workflows/agent-desktop-build.yml"
MAX_ARTIFACT = 4 * 1024**3
MAX_MANIFEST = 64 * 1024
MAX_ENVELOPE = 128 * 1024
MAX_SIGNATURE = 4 * 1024
MAX_NOTES = 16 * 1024
MAX_JSON = 4 * 1024 * 1024
CHUNK = 1024 * 1024
NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]*\Z")
SEMVER = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\Z")
SHA = re.compile(r"[0-9a-f]{40}\Z")
DIGEST = re.compile(r"[0-9a-f]{64}\Z")
ROOT = Path(__file__).resolve().parents[3]
FEED_URL = "https://updates.maple.ai/agent/stable/latest.json"
LINUX_FEED_URL = "https://updates.maple.ai/agent/stable/linux/latest.json"
WINDOWS_FEED_URL = "https://updates.maple.ai/agent/stable/windows/latest.json"
MACOS_METADATA_NAMES = {"macos-" + name for name in ("build-info.json", "manifest.json", "SHA256SUMS", "notarization-log.json", "dmg-notarization-log.json")}
LINUX_METADATA_NAMES = {"linux-" + name for name in ("build-info.json", "manifest.json", "SHA256SUMS")}
WINDOWS_METADATA_NAMES = {"windows-" + name for name in ("build-info.json", "manifest.json", "SHA256SUMS")}
METADATA_NAMES = {"provenance.json"} | MACOS_METADATA_NAMES | LINUX_METADATA_NAMES | WINDOWS_METADATA_NAMES


class PublishError(ValueError):
    """A bounded, credential-free diagnostic safe to print in CI."""


class StoreError(PublishError):
    def __init__(self, status):
        super().__init__(f"R2 request failed (HTTP {status})")
        self.status = status


def need(condition, message):
    if not condition:
        raise PublishError(message)


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        need(key not in result, "duplicate JSON field")
        result[key] = value
    return result


def decode_json(data, maximum=MAX_JSON):
    need(len(data) <= maximum, "oversized JSON")
    try:
        return json.loads(data.decode("utf-8"), object_pairs_hook=unique_object,
                          parse_constant=lambda _: (_ for _ in ()).throw(PublishError("non-finite JSON number")))
    except (UnicodeDecodeError, json.JSONDecodeError, RecursionError) as error:
        raise PublishError("invalid JSON") from error


def encode_json(value):
    return (json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")) + "\n").encode("utf-8")


@contextmanager
def regular_file(path):
    # NOFOLLOW and NONBLOCK reject symlinks/FIFOs before a privileged job reads.
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(descriptor, "rb") as source:
        need(stat.S_ISREG(os.fstat(source.fileno()).st_mode), "expected a regular file")
        yield source


def read_bytes(path, maximum):
    with regular_file(path) as source:
        need(os.fstat(source.fileno()).st_size <= maximum, "oversized input file")
        data = source.read(maximum + 1)
    need(len(data) <= maximum, "oversized input file")
    return data


def hash_stream(source, maximum=MAX_ARTIFACT):
    digest, size = hashlib.sha256(), 0
    while block := source.read(min(CHUNK, maximum - size + 1)):
        size += len(block)
        need(size <= maximum, "oversized artifact")
        digest.update(block)
    return digest.hexdigest(), size


def hash_file(path, maximum=MAX_ARTIFACT):
    with regular_file(path) as source:
        need(0 < os.fstat(source.fileno()).st_size <= maximum, "invalid artifact size")
        return hash_stream(source, maximum)


def write_once(path, data):
    path = Path(path)
    if path.exists() or path.is_symlink():
        need(read_bytes(path, max(MAX_JSON, len(data))) == data, "existing prepared bytes differ; use the original bundle")
        return
    with path.open("xb") as target:
        target.write(data)


def version_tuple(value):
    need(isinstance(value, str) and len(value) <= 64 and SEMVER.fullmatch(value), "version must be canonical stable SemVer")
    numbers = tuple(int(component) for component in value.split("."))
    need(all(number <= 2**64 - 1 for number in numbers), "SemVer component exceeds u64")
    return numbers


def trusted_comment(version):
    return f"product=maple-agent channel=stable version={version}"


def release_key(version, filename):
    version_tuple(version)
    need(isinstance(filename, str) and len(filename) <= 200 and ".." not in filename and NAME.fullmatch(filename), "invalid release filename")
    return f"stable/releases/{version}/{filename}"


def public_path(version, filename):
    return "/agent/" + release_key(version, filename)


def validate_manifest(data):
    manifest = decode_json(data, MAX_MANIFEST)
    fields = {"format", "product", "channel", "version", "build", "source_sha", "pub_date", "notes", "artifacts", "downloads"}
    need(isinstance(manifest, dict) and set(manifest) == fields, "invalid manifest fields")
    need(manifest["format"] in {"maple-agent-update-v1", "maple-agent-update-v2", "maple-agent-update-v3"} and manifest["product"] == "maple-agent" and manifest["channel"] == "stable", "wrong update identity")
    version_tuple(manifest["version"])
    build = manifest["build"]
    need(isinstance(build, str) and re.fullmatch(r"[1-9][0-9]{0,17}", build), "invalid build number")
    need(isinstance(manifest["source_sha"], str) and SHA.fullmatch(manifest["source_sha"]), "invalid source SHA")
    date = manifest["pub_date"]
    need(isinstance(date, str) and re.fullmatch(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z", date), "invalid publication date")
    try:
        datetime.strptime(date, "%Y-%m-%dT%H:%M:%SZ")
    except ValueError as error:
        raise PublishError("invalid publication date") from error
    notes = manifest["notes"]
    need(isinstance(notes, str) and len(notes.encode("utf-8")) <= MAX_NOTES, "oversized notes")
    need(all(ord(char) >= 32 or char in "\n\t" for char in notes) and "\x7f" not in notes, "notes contain ASCII controls")
    artifacts, downloads = manifest["artifacts"], manifest["downloads"]
    linux_v2 = manifest["format"] == "maple-agent-update-v2"
    windows_v3 = manifest["format"] == "maple-agent-update-v3"
    if windows_v3:
        need(isinstance(artifacts, dict) and set(artifacts) == {"windows-x86_64"}, "invalid Windows v3 update targets")
        need(isinstance(downloads, dict) and set(downloads) == {"windows"}, "invalid Windows v3 downloads")
    elif linux_v2:
        need(isinstance(artifacts, dict) and set(artifacts) == {"linux-x86_64"}, "invalid Linux v2 update targets")
        need(isinstance(downloads, dict) and set(downloads) == {"linux-bundle"}, "invalid Linux v2 downloads")
    else:
        # Retain the legacy v1 contract when interpreting historical envelopes.
        need(isinstance(artifacts, dict) and {"darwin-aarch64"} <= set(artifacts) <= {"darwin-aarch64", "linux-x86_64"}, "invalid update targets")
        need(isinstance(downloads, dict) and set(downloads) == ({"macos", "linux-appimage"} if "linux-x86_64" in artifacts else {"macos"}), "downloads do not match update targets")
    for kind, entries in (("artifacts", artifacts), ("downloads", downloads)):
        for target, entry in entries.items():
            need(isinstance(entry, dict) and set(entry) == {"path", "size", "sha256"}, "invalid artifact record")
            path = entry["path"]
            need(isinstance(path, str), "invalid artifact path")
            prefix = f"/agent/stable/releases/{manifest['version']}/"
            need(path.startswith(prefix), "artifact path has wrong release root")
            filename = path[len(prefix):]
            need(public_path(manifest["version"], filename) == path, "invalid artifact path")
            suffix = (".tar.gz" if kind == "artifacts" else ".exe") if windows_v3 else ".tar.gz" if linux_v2 else ".app.tar.gz" if target == "darwin-aarch64" else ".dmg" if target == "macos" else ".AppImage"
            need(filename.endswith(suffix), "wrong artifact suffix")
            need(type(entry["size"]) is int and 0 < entry["size"] <= MAX_ARTIFACT, "invalid artifact size")
            need(isinstance(entry["sha256"], str) and DIGEST.fullmatch(entry["sha256"]), "invalid artifact digest")
    if "linux-x86_64" in artifacts:
        need(artifacts["linux-x86_64"] == downloads["linux-bundle" if linux_v2 else "linux-appimage"], "Linux download and update differ")
    return manifest


def signature_text(signature, version):
    need(isinstance(signature, str) and len(signature.encode("utf-8")) <= MAX_SIGNATURE, "invalid or oversized signature")
    lines = signature.splitlines()
    need(len(lines) == 4 and lines[0].startswith("untrusted comment: ") and lines[2] == "trusted comment: " + trusted_comment(version), "wrong minisign comment or signature structure")
    try:
        packet = base64.b64decode(lines[1], validate=True)
        global_signature = base64.b64decode(lines[3], validate=True)
    except ValueError as error:
        raise PublishError("invalid minisign encoding") from error
    need(len(packet) == 74 and packet[:2] == b"ED" and len(global_signature) == 64, "signature must use prehashed minisign")


def parse_envelope(data):
    envelope = decode_json(data, MAX_ENVELOPE)
    need(isinstance(envelope, dict) and set(envelope) == {"manifest", "signature"} and isinstance(envelope["manifest"], str), "invalid signed envelope")
    manifest = validate_manifest(envelope["manifest"].encode("utf-8"))
    signature_text(envelope["signature"], manifest["version"])
    return envelope, manifest


def minisign_verify(data, public_key):
    envelope, manifest = parse_envelope(data)
    # Never print tool stderr: it may contain untrusted data or a secret path.
    with tempfile.TemporaryDirectory(prefix="maple-agent-verify-") as temporary:
        message = Path(temporary) / "manifest.json"
        signature = Path(temporary) / "signature.minisig"
        message.write_bytes(envelope["manifest"].encode("utf-8"))
        signature.write_text(envelope["signature"], encoding="utf-8")
        # Configured/public pinned keys may be raw base64 or a commented file.
        # minisign -p only accepts the latter, while -P accepts the normalized
        # public key packet in either case. No secret key is passed in argv.
        result = subprocess.run(["minisign", "-V", "-H", "-q", "-m", str(message), "-x", str(signature), "-P", public_key_text(public_key)], stdin=subprocess.DEVNULL, capture_output=True, timeout=30)
        need(result.returncode == 0, "minisign verification failed")
    return manifest


def gh_json(path):
    result = subprocess.run(["gh", "api", "--method", "GET", path], capture_output=True, timeout=60)
    need(result.returncode == 0, "GitHub API request failed")
    return decode_json(result.stdout)


def validate_run(run, jobs, linux=False, windows=False):
    need(isinstance(run, dict) and run.get("repository", {}).get("full_name") == REPOSITORY and run.get("head_repository", {}).get("full_name") == REPOSITORY, "untrusted build repository")
    need(run.get("path") == WORKFLOW and run.get("head_branch") == "master" and run.get("event") in {"push", "workflow_dispatch"}, "untrusted build workflow or ref")
    need(run.get("status") == "completed", "build run has not completed")
    need(isinstance(run.get("head_sha"), str) and SHA.fullmatch(run["head_sha"]), "invalid build commit")
    need(type(run.get("id")) is int and run["id"] > 0 and type(run.get("run_attempt")) is int and run["run_attempt"] > 0, "invalid build run identity")
    need(type(run.get("run_number")) is int and 0 < run["run_number"] < 10**18, "invalid build run number")
    need(isinstance(jobs, dict) and jobs.get("total_count") == len(jobs.get("jobs", [])), "incomplete build job inventory")
    required = {"Signed macOS (prod)", "Verify downloaded macOS (prod)"}
    if linux:
        required |= {"Linux bundle (prod)", "Attest Linux packages (prod)", "Verify downloaded Linux (prod)"}
    if windows:
        required |= {"Build Windows binary (prod)", "Signed Windows (prod)", "Verify downloaded Windows (prod)"}
    for name in required:
        matches = [job for job in jobs["jobs"] if job.get("name") == name]
        need(len(matches) == 1 and matches[0].get("status") == "completed" and matches[0].get("conclusion") == "success", "required Prod package job did not succeed: " + name)
    # An unrelated Dev or platform failure is not an Agent Prod release gate.
    # Overall run success is intentionally not treated as package proof.
    return run


def trust_run(run_id, linux=False, windows=False):
    need(str(run_id).isdecimal() and int(run_id) > 0, "invalid run ID")
    run = gh_json(f"repos/{REPOSITORY}/actions/runs/{int(run_id)}")
    attempt = run.get("run_attempt")
    need(type(attempt) is int and attempt > 0, "invalid build attempt")
    # GitHub's latest job inventory preserves successful producer jobs when
    # only failed consumers were re-run. An attempt-only inventory would omit
    # those producers even though their exact artifacts remain authoritative.
    jobs = gh_json(f"repos/{REPOSITORY}/actions/runs/{int(run_id)}/jobs?filter=latest&per_page=100")
    validate_run(run, jobs, linux, windows)
    need(run["id"] == int(run_id), "build run ID differs")
    artifacts = gh_json(f"repos/{REPOSITORY}/actions/runs/{int(run_id)}/artifacts?per_page=100")
    need(artifacts.get("total_count") == len(artifacts.get("artifacts", [])), "incomplete artifact inventory")
    names = {"macos": f"maple-agent-prod-macos-aarch64-{int(run_id)}"}
    if linux:
        names["linux"] = f"maple-agent-prod-linux-x86_64-{int(run_id)}"
    if windows:
        names["windows"] = f"maple-agent-prod-windows-x86_64-{int(run_id)}"
    for name in names.values():
        matches = [item for item in artifacts["artifacts"] if item.get("name") == name]
        need(len(matches) == 1 and matches[0].get("expired") is False, "required package artifact missing, duplicated or expired")
    return {"run_id": run["id"], "run_attempt": attempt, "source_sha": run["head_sha"], "build": str(run["run_number"]), "pub_date": run["created_at"], "artifacts": names}


def attest(path, source_sha):
    result = subprocess.run(["gh", "attestation", "verify", str(path), "--repo", REPOSITORY, "--signer-workflow", f"{REPOSITORY}/{WORKFLOW}", "--source-ref", "refs/heads/master", "--source-digest", source_sha, "--deny-self-hosted-runners"], stdin=subprocess.DEVNULL, capture_output=True, timeout=120)
    need(result.returncode == 0, "final package attestation verification failed")


def public_key_text(path):
    lines = read_bytes(path, MAX_SIGNATURE).decode("ascii").splitlines()
    if len(lines) == 2 and lines[0].startswith("untrusted comment: "):
        lines = lines[1:]
    need(len(lines) == 1, "invalid minisign public key file")
    try:
        packet = base64.b64decode(lines[0], validate=True)
    except ValueError as error:
        raise PublishError("invalid minisign public key encoding") from error
    need(len(packet) == 42 and packet[:2] == b"Ed", "invalid minisign public key")
    return lines[0]


def validate_pins(key, local_profile, source_profile, linux=False, windows=False):
    for profile in (local_profile, source_profile):
        need(isinstance(profile, dict) and isinstance(profile.get("prod"), dict), "missing Prod release profile")
        prod = profile["prod"]
        need(prod.get("update_feed_url") == FEED_URL, "Prod update feed does not match the production feed")
        if linux:
            need(prod.get("linux_update_feed_url") == LINUX_FEED_URL, "Prod Linux update feed does not match the production feed")
        if windows:
            need(prod.get("windows_update_feed_url") == WINDOWS_FEED_URL, "Prod Windows update feed does not match the production feed")
        keys = prod.get("update_public_keys")
        need(isinstance(keys, list) and all(isinstance(item, str) for item in keys) and key in keys, "signing key is not pinned by the selected source and trusted publisher")


def check_source_pins(public_key, source_sha, linux=False, windows=False):
    key = public_key_text(public_key)
    local = decode_json(read_bytes(ROOT / "apps/maple-agent/release-profiles.json", MAX_MANIFEST))
    response = gh_json(f"repos/{REPOSITORY}/contents/apps/maple-agent/release-profiles.json?ref={source_sha}")
    need(isinstance(response, dict) and response.get("encoding") == "base64" and isinstance(response.get("content"), str), "selected source profile is unavailable")
    try:
        source_bytes = base64.b64decode(response["content"].replace("\n", ""), validate=True)
    except ValueError as error:
        raise PublishError("selected source profile encoding is invalid") from error
    validate_pins(key, local, decode_json(source_bytes, MAX_MANIFEST), linux, windows)
    return key


def durable_provenance(proof):
    # A verifier-only rerun advances the diagnostic attempt while retaining
    # the exact source, packages, hashes and release identity.
    provenance = {name: value for name, value in proof.items() if name != "run_attempt"}
    provenance["attestation_policy"] = {"repository": REPOSITORY, "signer_workflow": WORKFLOW, "source_ref": "refs/heads/master", "source_sha": proof["source_sha"], "deny_self_hosted_runners": True}
    return encode_json(provenance)


def verify_current(data, public_key):
    """Accept an existing feed signed by either currently trusted pinned key.

    The new release still has to use the specifically selected key. This
    fallback only verifies the pointer being replaced during normal rotation.
    Removing the old key deliberately stops accepting its feed as authority.
    """
    selected = public_key_text(public_key)
    profile = decode_json(read_bytes(ROOT / "apps/maple-agent/release-profiles.json", MAX_MANIFEST))
    validate_pins(selected, profile, profile)
    for key in profile["prod"]["update_public_keys"]:
        with tempfile.TemporaryDirectory(prefix="maple-agent-current-key-") as temporary:
            path = Path(temporary) / "public-key.pub"
            path.write_text(key + "\n", encoding="ascii")
            public_key_text(path)
            try:
                return minisign_verify(data, path)
            except PublishError:
                continue
    raise PublishError("current feed signature is not trusted by the pinned key set")


class BoundedTarInfo(tarfile.TarInfo):
    def _proc_member(self, archive):
        # tarfile processes PAX/GNU extension records before yielding a member.
        # Cap them here, before its parser can allocate a header-sized buffer.
        archive.maple_headers = getattr(archive, "maple_headers", 0) + 1
        need(archive.maple_headers <= 100_000, "app archive exceeds bounded header inventory")
        need(0 <= self.size <= MAX_ARTIFACT, "app archive contains an oversized member")
        if self.type in {tarfile.XHDTYPE, tarfile.XGLTYPE, tarfile.SOLARIS_XHDTYPE, tarfile.GNUTYPE_LONGNAME, tarfile.GNUTYPE_LONGLINK}:
            need(self.size <= 64 * 1024, "app archive extension metadata exceeds bounds")
            archive.maple_metadata_bytes = getattr(archive, "maple_metadata_bytes", 0) + self.size
            need(archive.maple_metadata_bytes <= MAX_JSON, "app archive cumulative extension metadata exceeds bounds")
        need(self.type != tarfile.GNUTYPE_SPARSE, "sparse app archive members are unsupported")
        return super()._proc_member(archive)

    def _proc_gnusparse_00(self, *arguments):
        raise PublishError("sparse app archive members are unsupported")

    def _proc_gnusparse_01(self, *arguments):
        raise PublishError("sparse app archive members are unsupported")

    def _proc_gnusparse_10(self, *arguments):
        raise PublishError("sparse app archive members are unsupported")


def _archive_plist(archive):
    expected = "Maple Agent.app/Contents/Info.plist"
    found, count, expanded = None, 0, 0
    with regular_file(archive) as compressed:
        with tarfile.open(fileobj=compressed, mode="r|gz", tarinfo=BoundedTarInfo) as source:
            for member in source:
                count += 1
                expanded += member.size
                need(count <= 100_000 and 0 <= member.size <= MAX_ARTIFACT and expanded <= 8 * 1024**3, "app archive exceeds bounded inventory")
                if member.name != expected:
                    continue
                need(found is None and member.isfile() and 0 < member.size <= 64 * 1024, "invalid or duplicate app Info.plist member")
                with source.extractfile(member) as payload:
                    data = payload.read(64 * 1024 + 1)
                need(len(data) == member.size, "truncated app Info.plist")
                try:
                    found = plistlib.loads(data)
                except (plistlib.InvalidFileException, ValueError, TypeError, ExpatError, RecursionError) as error:
                    raise PublishError("invalid app Info.plist") from error
    return found


def validate_archive_build(archive, version, build, bundle_id="cloud.opensecret.maple.agent"):
    """Read the small plist member without extracting or executing the app."""
    try:
        found = _archive_plist(archive)
    except (tarfile.TarError, EOFError, OSError, RecursionError) as error:
        raise PublishError("invalid app archive") from error
    need(isinstance(found, dict), "app archive has no regular Info.plist")
    need(found.get("CFBundleVersion") == build and found.get("CFBundleShortVersionString") == version and found.get("CFBundleIdentifier") == bundle_id, "app Info.plist build, version or identity differs from selected run")


class LinuxTarInfo(BoundedTarInfo):
    def _proc_member(self, archive):
        need(getattr(archive, "maple_headers", 0) < 20_000, "Linux archive exceeds bounded header inventory")
        if self.type in {tarfile.XHDTYPE, tarfile.XGLTYPE, tarfile.SOLARIS_XHDTYPE, tarfile.GNUTYPE_LONGNAME, tarfile.GNUTYPE_LONGLINK}:
            need(self.size <= 16 * 1024, "Linux archive extension metadata exceeds bounds")
        return super()._proc_member(archive)


def validate_linux_archive_build(archive, version, build, source_sha, bundle_id):
    """Bind the small descriptor without extracting or executing the Linux app.

    The producer and downloaded consumer additionally scan the complete layout.
    This publisher check independently binds their payload to the selected run.
    """
    expected = {"format": "maple-agent-linux-bundle-v1", "product": "maple-agent",
                "profile": "prod", "version": version, "build": build,
                "source_sha": source_sha, "bundle_id": bundle_id,
                "entrypoint": "AppRun", "executable": "usr/bin/maple-agent"}
    need(isinstance(build, str) and re.fullmatch(r"[1-9][0-9]{0,17}", build), "Linux package build must match the selected run")
    found, count, expanded = None, 0, 0
    try:
        with regular_file(archive) as compressed:
            with tarfile.open(fileobj=compressed, mode="r|gz", tarinfo=LinuxTarInfo) as source:
                for member in source:
                    count += 1
                    expanded += member.size
                    need(count <= 20_000 and len(member.name.encode("utf-8")) <= 1024
                         and 0 <= member.size <= 2 * 1024**3 and expanded <= 4 * 1024**3,
                         "Linux archive exceeds bounded inventory")
                    if member.name != "maple-agent.app/bundle.json":
                        continue
                    need(found is None and member.isfile() and 0 < member.size <= MAX_MANIFEST,
                         "invalid or duplicate Linux bundle descriptor")
                    with source.extractfile(member) as payload:
                        data = payload.read(MAX_MANIFEST + 1)
                    need(len(data) == member.size, "truncated Linux bundle descriptor")
                    found = decode_json(data, MAX_MANIFEST)
                    need(isinstance(found, dict), "invalid Linux bundle descriptor")
    except (tarfile.TarError, EOFError, OSError, RecursionError) as error:
        raise PublishError("invalid Linux bundle archive") from error
    need(found == expected, "Linux bundle build, version, source or identity differs from selected run")


class WindowsTarInfo(BoundedTarInfo):
    def _proc_member(self, archive):
        need(getattr(archive, "maple_headers", 0) < 32, "Windows archive exceeds bounded header inventory")
        # The Windows bundle has a fixed ASCII layout and needs no extension,
        # sparse, link, device or executable archive metadata interpretation.
        need(self.type in {tarfile.REGTYPE, tarfile.AREGTYPE, tarfile.DIRTYPE},
             "Windows archive contains unsupported member metadata")
        return super()._proc_member(archive)


def preflight_windows_archive(archive):
    """Bound the raw tar stream, including bytes hidden after its end markers."""
    expanded, count = 0, 0
    with regular_file(archive) as compressed:
        with gzip.GzipFile(fileobj=compressed, mode="rb") as stream:
            while True:
                header = stream.read(512)
                need(len(header) == 512, "truncated Windows archive header")
                if header == bytes(512):
                    need(stream.read(512) == bytes(512), "invalid Windows archive end marker")
                    trailing = stream.read(10241)
                    need(len(trailing) <= 10240 and not any(trailing),
                         "Windows archive has trailing content or excessive padding")
                    return
                count += 1
                member = tarfile.TarInfo.frombuf(header, "utf-8", "strict")
                need(count <= 32 and member.type in {tarfile.REGTYPE, tarfile.AREGTYPE, tarfile.DIRTYPE}
                     and not member.linkname, "Windows archive contains unsupported member metadata")
                expanded += member.size
                need(0 <= member.size <= 2 * 1024**3 and expanded <= 4 * 1024**3
                     and (not member.isdir() or member.size == 0),
                     "Windows archive exceeds bounded inventory")
                remaining = (member.size + 511) // 512 * 512
                while remaining:
                    block = stream.read(min(remaining, CHUNK))
                    need(bool(block), "truncated Windows archive payload")
                    remaining -= len(block)


def validate_windows_archive_build(archive, version, build, source_sha, bundle_id, build_info):
    """Validate the complete fixed Windows layout without extraction/execution.

    Native signed-package consumers verify Authenticode on the generation executable. This
    independent publisher check binds their attested archive to the selected
    release and rejects every auxiliary or ambiguous archive member.
    """
    root = "maple-agent-windows"
    files = {"bundle-info.json", "maple-agent.exe", "LICENSE.txt", "build-info.json"}
    expected_files = {root + "/" + name for name in files}
    expected_dirs = {root}
    expected = {"format": "maple-agent-windows-bundle-v2", "product": "maple-agent", "profile": "prod",
                "version": version, "build": build, "source_sha": source_sha,
                "bundle_id": bundle_id, "entrypoint": "maple-agent.exe"}
    need(isinstance(build, str) and re.fullmatch(r"[1-9][0-9]{0,17}", build),
         "Windows package build must match the selected run")
    seen, descriptor, embedded_info, expanded = set(), None, None, 0
    try:
        preflight_windows_archive(archive)
        with regular_file(archive) as compressed:
            with tarfile.open(fileobj=compressed, mode="r|gz", tarinfo=WindowsTarInfo) as source:
                for member in source:
                    name = member.name
                    need(name not in seen and len(name.encode("utf-8")) <= 200,
                         "duplicate or oversized Windows archive path")
                    seen.add(name)
                    expanded += member.size
                    need(0 <= member.size <= 2 * 1024**3 and expanded <= 4 * 1024**3,
                         "Windows archive exceeds bounded inventory")
                    if member.isdir():
                        need(name in expected_dirs and member.size == 0 and member.mode == 0o755,
                             "invalid Windows archive directory")
                        continue
                    need(member.isfile() and name in expected_files and member.mode == 0o644 and member.size > 0,
                         "invalid Windows archive file inventory")
                    if name in {root + "/bundle-info.json", root + "/build-info.json"}:
                        need(member.size <= MAX_MANIFEST, "oversized Windows bundle identity")
                        with source.extractfile(member) as payload:
                            data = payload.read(MAX_MANIFEST + 1)
                        need(len(data) == member.size, "truncated Windows bundle identity")
                        if name.endswith("/bundle-info.json"):
                            descriptor = decode_json(data, MAX_MANIFEST)
                        else:
                            embedded_info = data
    except (tarfile.TarError, EOFError, OSError, UnicodeError, RecursionError, zlib.error) as error:
        raise PublishError("invalid Windows bundle archive") from error
    need(seen == expected_files | expected_dirs, "Windows archive file inventory differs")
    need(descriptor == expected, "Windows bundle build, version, source or identity differs from selected run")
    need(embedded_info == build_info, "Windows archive build-info bytes differ from the attested sidecar")


def validate_package(directory, platform, source_sha, version, attestor=attest, expected_build=None, metadata_records=None):
    directory = Path(directory)
    need(directory.is_dir() and not directory.is_symlink(), "package input must be a directory")
    info = decode_json(read_bytes(directory / "build-info.json", 16 * 1024))
    # Reuse the checked-in public profile contract, never package-supplied code.
    script = ROOT / "apps/maple-agent/scripts/release-info.py"
    spec = importlib.util.spec_from_file_location("agent_release_info", script)
    release_info = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(release_info)
    try:
        release_info.validate_info("prod", info, source_sha, platform)
    except (ValueError, KeyError, TypeError) as error:
        raise PublishError("package does not match the Prod build identity") from error
    need(info["version"] == version, "package version differs from publication version")
    stem = f"maple-agent-prod-{version}-{info['git_revision']}-{platform}"
    package = stem + (".dmg" if platform == "macos-aarch64" else "-setup.exe" if platform == "windows-x86_64" else ".tar.gz")
    binaries = [package, stem + ".app.tar.gz"] if platform == "macos-aarch64" else [package, stem + ".tar.gz"] if platform == "windows-x86_64" else [package]
    expected = {"build-info.json", "manifest.json", "SHA256SUMS", *binaries}
    if platform == "macos-aarch64":
        expected |= {"notarization-log.json", "dmg-notarization-log.json"}
    need(set(path.name for path in directory.iterdir()) == expected, "unexpected or missing final package files")
    records = {}
    for name in sorted(expected):
        path = directory / name
        digest, size = hash_file(path, MAX_ARTIFACT if name in binaries else MAX_JSON)
        records[name] = {"sha256": digest, "size": size}
    metadata = decode_json(read_bytes(directory / "manifest.json", MAX_MANIFEST))
    expected_metadata = {"schema_version": 1, "profile": "prod", "platform": platform, "source_sha": source_sha, "version": version, "package": package, "distribution": "master", "macos_signing": "developer-id-notarized" if platform == "macos-aarch64" else None}
    if platform == "linux-x86_64":
        expected_metadata["linux_integrity"] = "sha256"
    if platform == "windows-x86_64":
        need(isinstance(expected_build, str) and re.fullmatch(r"[1-9][0-9]{0,17}", expected_build), "Windows package build must match the selected run")
        expected_metadata.update(windows_signing="authenticode", updater=stem + ".tar.gz", build=expected_build)
    need(isinstance(metadata, dict) and type(metadata.get("schema_version")) is int and metadata == expected_metadata, "invalid final package metadata")
    checked = set()
    for line in read_bytes(directory / "SHA256SUMS", 16 * 1024).decode("ascii").splitlines():
        match = re.fullmatch(r"([0-9a-f]{64})  ([A-Za-z0-9._-]+)", line)
        need(match is not None, "invalid package checksum record")
        digest, name = match.groups()
        need(name not in checked and name != "SHA256SUMS" and name in records and records[name]["sha256"] == digest, "package checksum mismatch or duplicate")
        checked.add(name)
    need(checked == expected - {"SHA256SUMS"}, "package checksum inventory differs")
    if platform == "macos-aarch64":
        for name in ("notarization-log.json", "dmg-notarization-log.json"):
            notary = decode_json(read_bytes(directory / name, MAX_JSON))
            need(isinstance(notary, dict) and notary.get("status") == "Accepted" and notary.get("issues") in (None, []), "package notarization was not accepted")
        if expected_build is not None:
            validate_archive_build(directory / binaries[1], version, expected_build, info["bundle_id"])
    if platform == "linux-x86_64":
        validate_linux_archive_build(directory / binaries[0], version, expected_build, source_sha, info["bundle_id"])
    if platform == "windows-x86_64":
        validate_windows_archive_build(directory / binaries[1], version, expected_build, source_sha, info["bundle_id"], read_bytes(directory / "build-info.json", 16 * 1024))
    for name in sorted(expected):
        attestor(directory / name, source_sha)
    if metadata_records is not None:
        metadata_records.update({name: record for name, record in records.items() if name not in binaries})
    return [(directory / name, {"path": public_path(version, name), **records[name]}) for name in binaries]


def prepare(run_id, version, build, macos, linux, notes_file, output, public_key, pub_date=None, windows=None):
    version_tuple(version)
    options = {"windows": True} if windows else {}
    proof = trust_run(run_id, bool(linux), **options)
    need(build == proof["build"], "build number must match the package run number")
    key = check_source_pins(public_key, proof["source_sha"], bool(linux), **options)
    notes = read_bytes(notes_file, MAX_NOTES).decode("utf-8")
    mac_metadata, linux_metadata, windows_metadata = {}, {}, {}
    mac = validate_package(macos, "macos-aarch64", proof["source_sha"], version, expected_build=proof["build"], metadata_records=mac_metadata)
    lin = validate_package(linux, "linux-x86_64", proof["source_sha"], version, expected_build=proof["build"], metadata_records=linux_metadata) if linux else []
    win = validate_package(windows, "windows-x86_64", proof["source_sha"], version, expected_build=proof["build"], metadata_records=windows_metadata) if windows else []
    manifest = {"format": "maple-agent-update-v1", "product": "maple-agent", "channel": "stable", "version": version, "build": build, "source_sha": proof["source_sha"], "pub_date": pub_date or proof["pub_date"], "notes": notes, "artifacts": {"darwin-aarch64": mac[1][1]}, "downloads": {"macos": mac[0][1]}}
    data = encode_json(manifest)
    validate_manifest(data)
    output = Path(output)
    output.mkdir(parents=True, exist_ok=True)
    need(not output.is_symlink(), "bundle directory must not be a symlink")
    write_once(output / "manifest.json", data)
    if lin:
        linux_manifest = {**manifest, "format": "maple-agent-update-v2",
                          "artifacts": {"linux-x86_64": lin[0][1]},
                          "downloads": {"linux-bundle": lin[0][1]}}
        linux_data = encode_json(linux_manifest)
        validate_manifest(linux_data)
        write_once(output / "linux-manifest.json", linux_data)
    if win:
        windows_manifest = {**manifest, "format": "maple-agent-update-v3",
                            "artifacts": {"windows-x86_64": win[1][1]},
                            "downloads": {"windows": win[0][1]}}
        windows_data = encode_json(windows_manifest)
        validate_manifest(windows_data)
        write_once(output / "windows-manifest.json", windows_data)
    write_once(output / "trust.json", encode_json(proof))
    write_once(output / "public-key.pub", ("untrusted comment: Maple Agent update key\n" + key + "\n").encode("ascii"))
    metadata = output / "metadata"
    metadata.mkdir(exist_ok=True)
    need(not metadata.is_symlink(), "metadata directory must not be a symlink")
    inventory = {}
    for prefix, directory, source_records in (("macos", macos, mac_metadata), ("linux", linux, linux_metadata), ("windows", windows, windows_metadata)):
        for name, record in sorted(source_records.items()):
            data = read_bytes(Path(directory) / name, MAX_JSON)
            need((hashlib.sha256(data).hexdigest(), len(data)) == (record["sha256"], record["size"]), "package metadata changed after provenance verification")
            destination = prefix + "-" + name
            write_once(metadata / destination, data)
            inventory[destination] = record
    # A validator-only build retry advances run_attempt without changing any
    # release bytes. Retain the stable build identity in durable audit data;
    # the current attempt remains available in the CI trust.json diagnostic.
    provenance_data = durable_provenance(proof)
    write_once(metadata / "provenance.json", provenance_data)
    inventory["provenance.json"] = {"sha256": hashlib.sha256(provenance_data).hexdigest(), "size": len(provenance_data)}
    write_once(output / "metadata-inventory.json", encode_json(inventory))
    payloads = output / "payloads"
    payloads.mkdir(exist_ok=True)
    need(not payloads.is_symlink(), "payload directory must not be a symlink")
    for source, record in mac + lin + win:
        destination = payloads / source.name
        if destination.exists() or destination.is_symlink():
            need(hash_file(destination) == (record["sha256"], record["size"]), "existing prepared artifact differs")
        else:
            with regular_file(source) as input_file, destination.open("xb") as target:
                shutil.copyfileobj(input_file, target, CHUNK)
            need(hash_file(destination) == (record["sha256"], record["size"]), "artifact changed during preparation")
    return manifest


def bundle_manifests(bundle):
    """Every exact unsigned document is prepared before reservation/signing.

    The first document remains compatible with shipped v1 Mac clients. Linux v2
    and Windows v3 each have their own pointer, envelope and reservation.
    """
    bundle = Path(bundle)
    publications = []
    for name, envelope, reservation, pointer in (
        ("manifest.json", "envelope.json", "publication-manifest.json", "stable/latest.json"),
        ("linux-manifest.json", "linux-envelope.json", "publication-linux-manifest.json", "stable/linux/latest.json"),
        ("windows-manifest.json", "windows-envelope.json", "publication-windows-manifest.json", "stable/windows/latest.json"),
    ):
        path = bundle / name
        if name != "manifest.json" and not (path.exists() or path.is_symlink()):
            need(not ((bundle / envelope).exists() or (bundle / envelope).is_symlink()), "platform envelope has no prepared manifest")
            continue
        data = read_bytes(path, MAX_MANIFEST)
        manifest = validate_manifest(data)
        if not publications:
            need(manifest["format"] == "maple-agent-update-v1" and set(manifest["artifacts"]) == {"darwin-aarch64"}, "new Mac publication must remain mac-only v1")
        else:
            expected_format = "maple-agent-update-v2" if name == "linux-manifest.json" else "maple-agent-update-v3"
            need(manifest["format"] == expected_format, "platform publication has wrong format")
            primary = publications[0]["manifest"]
            need(all(manifest[field] == primary[field] for field in
                     ("product", "channel", "version", "build", "source_sha", "pub_date", "notes")),
                 "platform release identity differs")
        publications.append({"name": name, "envelope_name": envelope, "reservation": reservation,
                             "pointer": pointer, "data": data, "manifest": manifest})
    return publications


def reservation_receipt(publications, metadata_inventory):
    primary = publications[0]
    receipt = {"manifest_sha256": hashlib.sha256(primary["data"]).hexdigest(),
               "metadata_inventory_sha256": hashlib.sha256(metadata_inventory).hexdigest(),
               "version": primary["manifest"]["version"]}
    for publication in publications[1:]:
        prefix = "linux" if publication["name"] == "linux-manifest.json" else "windows"
        receipt[prefix + "_manifest_sha256"] = hashlib.sha256(publication["data"]).hexdigest()
    return receipt


def bundle_records(bundle, manifest):
    payloads = Path(bundle) / "payloads"
    need(payloads.is_dir() and not payloads.is_symlink(), "invalid prepared payload directory")
    publications = bundle_manifests(bundle)
    need(publications[0]["manifest"] == manifest, "primary prepared manifest differs")
    records = {}
    for publication in publications:
        wire = publication["manifest"]
        for record in [*wire["artifacts"].values(), *wire["downloads"].values()]:
            name = record["path"].rsplit("/", 1)[1]
            need(name not in records or records[name] == record, "conflicting prepared artifact records")
            records[name] = record
    need(set(path.name for path in payloads.iterdir()) == set(records), "prepared payload inventory differs")
    for name, record in records.items():
        need(hash_file(payloads / name, record["size"]) == (record["sha256"], record["size"]), "prepared artifact hash differs")
    return [(payloads / name, record) for name, record in sorted(records.items())]


def bundle_metadata(bundle, manifest):
    bundle = Path(bundle)
    index = bundle / "metadata-inventory.json"
    need(index.exists() or index.is_symlink(), "metadata has no inventory")
    data = read_bytes(index, MAX_MANIFEST)
    inventory = decode_json(data, MAX_MANIFEST)
    expected = {"provenance.json"} | MACOS_METADATA_NAMES
    formats = {publication["manifest"]["format"] for publication in bundle_manifests(bundle)}
    if "maple-agent-update-v2" in formats:
        expected |= LINUX_METADATA_NAMES
    if "maple-agent-update-v3" in formats:
        expected |= WINDOWS_METADATA_NAMES
    need(isinstance(inventory, dict) and set(inventory) == expected, "private metadata inventory must contain the exact sidecars for every included target")
    directory = bundle / "metadata"
    need(directory.is_dir() and not directory.is_symlink() and {path.name for path in directory.iterdir()} == set(inventory), "private metadata inventory differs")
    result = []
    for name, record in sorted(inventory.items()):
        need(isinstance(record, dict) and set(record) == {"sha256", "size"} and isinstance(record["sha256"], str) and DIGEST.fullmatch(record["sha256"]), "invalid private metadata record")
        need(type(record["size"]) is int and 0 < record["size"] <= MAX_JSON, "invalid private metadata size")
        path = directory / name
        need(hash_file(path, record["size"]) == (record["sha256"], record["size"]), "private metadata hash differs")
        result.append((path, record))
    return data, result


def metadata_key(version, filename):
    need(filename in METADATA_NAMES, "invalid private metadata name")
    return release_key(version, "metadata") + "/" + filename


def publish_metadata(store, version, records):
    for path, record in records:
        key = metadata_key(version, path.name)
        immutable_put(store, key, path, "application/json" if path.suffix == ".json" else "text/plain", maximum=record["size"])
        need(store.same(key, record["sha256"], record["size"]), "private metadata read-back differs")


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, file, code, message, headers, new_url):
        return None


def https_origin(value):
    parsed = urllib.parse.urlsplit(value)
    need(parsed.scheme == "https" and parsed.hostname and not parsed.username and not parsed.password and not parsed.query and not parsed.fragment and parsed.path in {"", "/"} and parsed.port in {None, 443}, "expected a plain HTTPS origin")
    return f"https://{parsed.netloc}"


class R2Store:
    """Path-style S3 requests with signed conditional headers; no redirects."""

    def __init__(self, endpoint, bucket, access_key=None, secret_key=None):
        self.endpoint = https_origin(endpoint)
        need(re.fullmatch(r"[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]", bucket), "invalid bucket name")
        self.bucket = bucket
        self.access_key = access_key or os.environ.get("AWS_ACCESS_KEY_ID", "")
        self.secret_key = secret_key or os.environ.get("AWS_SECRET_ACCESS_KEY", "")
        need(self.access_key and self.secret_key, "R2 credentials are not configured")
        self.opener = urllib.request.build_opener(NoRedirect)

    def request(self, method, key, data=None, content_hash=None, size=None, condition=None, content_type=None, maximum=MAX_JSON, consumer=None):
        need(not key.startswith("/") and all(NAME.fullmatch(part) for part in key.split("/")), "invalid R2 object key")
        path = "/" + self.bucket + "/" + urllib.parse.quote(key, safe="/-_.~")
        now = datetime.now(timezone.utc)
        timestamp, day = now.strftime("%Y%m%dT%H%M%SZ"), now.strftime("%Y%m%d")
        digest = content_hash or hashlib.sha256(data or b"").hexdigest()
        headers = {"host": urllib.parse.urlsplit(self.endpoint).netloc, "x-amz-content-sha256": digest, "x-amz-date": timestamp}
        if condition:
            headers[condition[0].lower()] = condition[1]
        if content_type:
            headers["content-type"] = content_type
        if size is not None:
            headers["content-length"] = str(size)
        names = ";".join(sorted(headers))
        canonical_headers = "".join(f"{name}:{headers[name].strip()}\n" for name in sorted(headers))
        canonical = f"{method}\n{path}\n\n{canonical_headers}\n{names}\n{digest}"
        scope = f"{day}/auto/s3/aws4_request"
        to_sign = f"AWS4-HMAC-SHA256\n{timestamp}\n{scope}\n{hashlib.sha256(canonical.encode()).hexdigest()}"
        signing = ("AWS4" + self.secret_key).encode()
        for part in (day, "auto", "s3", "aws4_request"):
            signing = hmac.new(signing, part.encode(), hashlib.sha256).digest()
        signature = hmac.new(signing, to_sign.encode(), hashlib.sha256).hexdigest()
        headers["authorization"] = f"AWS4-HMAC-SHA256 Credential={self.access_key}/{scope}, SignedHeaders={names}, Signature={signature}"
        request = urllib.request.Request(self.endpoint + path, method=method, data=data, headers=headers)
        try:
            with self.opener.open(request, timeout=120) as response:
                response_headers = dict(response.headers.items())
                if consumer is not None:
                    return consumer(response), response_headers
                if method == "HEAD":
                    return b"", response_headers
                body = response.read(maximum + 1)
                need(len(body) <= maximum, "oversized R2 response")
                return body, response_headers
        except urllib.error.HTTPError as error:
            raise StoreError(error.code) from None
        except (OSError, urllib.error.URLError) as error:
            raise PublishError("R2 transport failed") from None

    def get(self, key, maximum=MAX_JSON):
        try:
            data, headers = self.request("GET", key, maximum=maximum)
        except StoreError as error:
            if error.status == 404:
                return None
            raise
        etag = next((value for name, value in headers.items() if name.lower() == "etag"), None)
        need(etag is not None and len(etag) < 256, "R2 object has no valid ETag")
        return data, etag

    def same(self, key, digest, size):
        # Read existing content, rather than trusting mutable custom metadata
        # or multipart ETags as SHA-256. This is only needed on retries/races.
        try:
            return self.stream_hash(key, maximum=size) == (digest, size)
        except StoreError as error:
            if error.status == 404:
                return False
            raise

    def stream_hash(self, key, maximum=MAX_ARTIFACT):
        # A GET can be large: hash a bounded stream, rather than buffering it.
        return self.request("GET", key, consumer=lambda response: hash_stream(response, maximum))[0]

    def put(self, key, data, *, condition, content_type="application/octet-stream"):
        if isinstance(data, Path):
            digest, size = hash_file(data, MAX_JSON if "/metadata/" in key else MAX_ARTIFACT)
            with regular_file(data) as source:
                self.request("PUT", key, source, digest, size, condition, content_type)
        else:
            self.request("PUT", key, data, hashlib.sha256(data).hexdigest(), len(data), condition, content_type)


def immutable_put(store, key, data, content_type="application/octet-stream", maximum=MAX_ARTIFACT):
    if isinstance(data, Path):
        digest, size = hash_file(data, maximum)
    else:
        digest, size = hashlib.sha256(data).hexdigest(), len(data)
    # A locked release object may reject an attempted write before evaluating
    # its conditional header. Reuse matching bytes without any write attempt.
    # Missing/different objects still require a create-only PUT; the second
    # comparison handles a writer winning the race after this first read.
    if store.same(key, digest, size):
        return
    try:
        store.put(key, data, condition=("If-None-Match", "*"), content_type=content_type)
    except StoreError as error:
        if error.status != 412:
            raise
        need(store.same(key, digest, size), "immutable object differs; choose a new release version")


def reserve(bundle, store, public_key=None):
    bundle = Path(bundle)
    publications = bundle_manifests(bundle)
    manifest = publications[0]["manifest"]
    bundle_records(bundle, manifest)
    metadata_inventory, _ = bundle_metadata(bundle, manifest)
    # Bind every exact unsigned document and the complete sidecar inventory
    # before the signing job can retrieve a key or use this release version.
    for publication in publications:
        key = release_key(manifest["version"], publication["reservation"])
        immutable_put(store, key, publication["data"], "application/json")
        reserved = store.get(key, MAX_MANIFEST)
        need(reserved is not None and reserved[0] == publication["data"], "reserved manifest read-back differs")
    immutable_put(store, release_key(manifest["version"], "publication-metadata.json"), metadata_inventory, "application/json")
    reserved_metadata = store.get(release_key(manifest["version"], "publication-metadata.json"), MAX_MANIFEST)
    need(reserved_metadata is not None and reserved_metadata[0] == metadata_inventory, "reserved metadata read-back differs")
    for publication in publications:
        existing = store.get(release_key(manifest["version"], publication["name"]), MAX_ENVELOPE)
        if existing:
            envelope, _ = parse_envelope(existing[0])
            need(envelope["manifest"].encode("utf-8") == publication["data"], "reserved envelope has different manifest bytes")
            if public_key:
                minisign_verify(existing[0], public_key)
            write_once(bundle / publication["envelope_name"], existing[0])
    write_once(bundle / "reservation.json", encode_json(reservation_receipt(publications, metadata_inventory)))
    return manifest


def sign(bundle, secret_key, public_key):
    bundle = Path(bundle)
    need(public_key_text(public_key) == public_key_text(bundle / "public-key.pub"), "signing key differs from the prepared pinned key")
    publications = bundle_manifests(bundle)
    manifest = publications[0]["manifest"]
    bundle_records(bundle, manifest)
    metadata_inventory, _ = bundle_metadata(bundle, manifest)
    receipt = decode_json(read_bytes(bundle / "reservation.json", MAX_MANIFEST))
    need(receipt == reservation_receipt(publications, metadata_inventory), "manifest has not been reserved before signing")
    for publication in publications:
        target = bundle / publication["envelope_name"]
        data = publication["data"]
        if target.exists() or target.is_symlink():
            existing = read_bytes(target, MAX_ENVELOPE)
            need(parse_envelope(existing)[0]["manifest"].encode("utf-8") == data, "existing envelope differs")
            minisign_verify(existing, public_key)
            continue
        with tempfile.TemporaryDirectory(prefix="maple-agent-sign-") as temporary:
            message = Path(temporary) / "manifest.json"
            signature = Path(temporary) / "signature.minisig"
            message.write_bytes(data)
            result = subprocess.run(["minisign", "-S", "-W", "-s", str(secret_key), "-m", str(message), "-x", str(signature), "-c", "Maple Agent update", "-t", trusted_comment(manifest["version"])], stdin=subprocess.DEVNULL, capture_output=True, timeout=30)
            need(result.returncode == 0, "minisign signing failed")
            signature_data = read_bytes(signature, MAX_SIGNATURE).decode("utf-8")
        envelope = encode_json({"manifest": data.decode("utf-8"), "signature": signature_data})
        minisign_verify(envelope, public_key)
        write_once(target, envelope)
    return manifest


def publish(bundle, store, public_key, verifier=minisign_verify, previous_verifier=verify_current):
    bundle = Path(bundle)
    need(public_key_text(public_key) == public_key_text(bundle / "public-key.pub"), "publication key differs from the prepared pinned key")
    publications = bundle_manifests(bundle)
    manifest = publications[0]["manifest"]
    records = bundle_records(bundle, manifest)
    metadata_inventory, metadata_records = bundle_metadata(bundle, manifest)
    for publication in publications:
        envelope = read_bytes(bundle / publication["envelope_name"], MAX_ENVELOPE)
        verified = verifier(envelope, public_key)
        need(verified == publication["manifest"] and parse_envelope(envelope)[0]["manifest"].encode("utf-8") == publication["data"], "signed and reserved manifest bytes differ")
        publication["envelope"] = envelope
        reserved = store.get(release_key(manifest["version"], publication["reservation"]), MAX_MANIFEST)
        need(reserved is not None and reserved[0] == publication["data"], "version must be reserved before signing and publication")
    reserved_metadata = store.get(release_key(manifest["version"], "publication-metadata.json"), MAX_MANIFEST)
    need(reserved_metadata is not None and reserved_metadata[0] == metadata_inventory, "metadata must be reserved before signing and publication")
    # Snapshot ALL selected feeds before uploading or promoting any. A later
    # operator change wins its CAS. Partial promotion resumes by exact retry;
    # there is no unsafe multi-pointer rollback or unconditional replacement.
    for publication in publications:
        pointer = store.get(publication["pointer"], MAX_ENVELOPE)
        publication["previous"] = pointer
        if pointer:
            previous = previous_verifier(pointer[0], public_key)
            need(previous["format"] == publication["manifest"]["format"], "published feed format differs")
            if pointer[0] != publication["envelope"]:
                need(version_tuple(manifest["version"]) > version_tuple(previous["version"]), "release version must advance the published version")
    for path, record in records:
        key = record["path"][len("/agent/"):]
        immutable_put(store, key, path, maximum=record["size"])
        need(store.same(key, record["sha256"], record["size"]), "uploaded artifact read-back differs")
    publish_metadata(store, manifest["version"], metadata_records)
    for publication in publications:
        key = release_key(manifest["version"], publication["name"])
        immutable_put(store, key, publication["envelope"], "application/json")
        complete = store.get(key, MAX_ENVELOPE)
        need(complete is not None and complete[0] == publication["envelope"], "complete manifest read-back differs")
    changed = False
    for publication in publications:
        pointer = publication["previous"]
        if pointer and pointer[0] == publication["envelope"]:
            continue
        condition = ("If-Match", pointer[1]) if pointer else ("If-None-Match", "*")
        try:
            store.put(publication["pointer"], publication["envelope"], condition=condition, content_type="application/json")
        except StoreError as error:
            if error.status == 412:
                raise PublishError("publication pointer changed concurrently; inspect it before retrying") from None
            raise
        changed = True
    return "published" if changed else "already-published"


def live_request(opener, url, *, method="GET", headers=None, maximum=MAX_ENVELOPE):
    request = urllib.request.Request(url, method=method, headers=headers or {})
    try:
        response = opener.open(request, timeout=120)
    except urllib.error.HTTPError as error:
        response = error
    except (OSError, urllib.error.URLError):
        raise PublishError("live endpoint transport failed") from None
    with response:
        data = response.read(maximum + 1)
        need(len(data) <= maximum, "oversized live response")
        return response.status, response.headers, data


def verify_live(bundle, public_key, base_url):
    bundle = Path(bundle)
    origin = https_origin(base_url)
    publications = bundle_manifests(bundle)
    manifest = publications[0]["manifest"]
    bundle_records(bundle, manifest)
    opener = urllib.request.build_opener(NoRedirect)
    # Match native delivery while excluding release verification from analytics.
    opener.addheaders = [("User-Agent", "Maple-Agent-Updater/1"), ("Accept-Encoding", "identity"),
                         ("X-Maple-Download-Probe", "1")]
    for publication in publications:
        envelope = read_bytes(bundle / publication["envelope_name"], MAX_ENVELOPE)
        verified = minisign_verify(envelope, public_key)
        need(verified == publication["manifest"] and parse_envelope(envelope)[0]["manifest"].encode("utf-8") == publication["data"], "live verification envelope differs from preparation")
        for path in ("/agent/" + publication["pointer"], public_path(manifest["version"], publication["name"])):
            status, _, body = live_request(opener, origin + path)
            need(status == 200 and body == envelope, f"live signed metadata differs at {path} (HTTP {status})")
        wire = publication["manifest"]
        for record in {item["path"]: item for item in [*wire["artifacts"].values(), *wire["downloads"].values()]}.values():
            status, headers, _ = live_request(opener, origin + record["path"], method="HEAD", maximum=0)
            need(status == 200 and headers.get("Content-Length") == str(record["size"]), "live artifact HEAD failed")
            status, headers, part = live_request(opener, origin + record["path"], headers={"Range": "bytes=0-0"}, maximum=1)
            need(status == 206 and len(part) == 1 and headers.get("Content-Range") == f"bytes 0-0/{record['size']}", "live artifact range request failed")
            request = urllib.request.Request(origin + record["path"])
            try:
                with opener.open(request, timeout=120) as response:
                    need(response.status == 200 and hash_stream(response, record["size"]) == (record["sha256"], record["size"]), "live artifact digest differs")
            except (urllib.error.URLError, OSError):
                raise PublishError("live artifact download failed") from None
        for target, record in wire["downloads"].items():
            alias = "linux" if target == "linux-bundle" else target
            status, headers, _ = live_request(opener, origin + f"/download/agent/stable/{alias}", maximum=1024)
            need(status in {302, 307} and headers.get("Location") in {record["path"], origin + record["path"]}, "live download redirect differs")
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    trust = commands.add_parser("trust")
    trust.add_argument("--run-id", required=True)
    trust.add_argument("--linux", action="store_true")
    trust.add_argument("--windows", action="store_true")
    trust.add_argument("--output", required=True)
    prep = commands.add_parser("prepare")
    prep.add_argument("--run-id", required=True)
    prep.add_argument("--version", required=True)
    prep.add_argument("--build", required=True)
    prep.add_argument("--macos", required=True)
    prep.add_argument("--linux")
    prep.add_argument("--windows")
    prep.add_argument("--notes-file", required=True)
    prep.add_argument("--pub-date")
    prep.add_argument("--output", required=True)
    prep.add_argument("--public-key", required=True)
    for name in ("reserve", "sign", "publish", "verify-live"):
        command = commands.add_parser(name)
        command.add_argument("--bundle", required=True)
        command.add_argument("--public-key", required=True)
        if name in {"reserve", "publish"}:
            command.add_argument("--endpoint", required=True)
            command.add_argument("--bucket", required=True)
        if name == "sign":
            command.add_argument("--secret-key", required=True)
        if name in {"publish", "verify-live"}:
            command.add_argument("--base-url", required=True)
    args = parser.parse_args()
    if args.command == "trust":
        proof = trust_run(args.run_id, args.linux, args.windows)
        write_once(args.output, encode_json(proof))
        print(json.dumps(proof, sort_keys=True))
    elif args.command == "prepare":
        manifest = prepare(args.run_id, args.version, args.build, args.macos, args.linux, args.notes_file, args.output, args.public_key, args.pub_date, args.windows)
        print(f"prepared Agent {manifest['version']} from {manifest['source_sha']}")
    elif args.command == "reserve":
        manifest = reserve(args.bundle, R2Store(args.endpoint, args.bucket), args.public_key)
        print(f"reserved Agent {manifest['version']} manifest")
    elif args.command == "sign":
        manifest = sign(args.bundle, args.secret_key, args.public_key)
        print(f"signed Agent {manifest['version']}")
    elif args.command == "publish":
        result = publish(args.bundle, R2Store(args.endpoint, args.bucket), args.public_key)
        verify_live(args.bundle, args.public_key, args.base_url)
        print(result + "; live metadata, signature, artifacts, ranges and redirects verified")
    elif args.command == "verify-live":
        manifest = verify_live(args.bundle, args.public_key, args.base_url)
        print(f"verified live Agent {manifest['version']}")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, KeyError, TypeError, UnicodeError, tarfile.TarError, RecursionError, subprocess.SubprocessError):
        # Do not expose subprocess stderr, request URLs/headers, or secret paths.
        # PublishError messages are authored here and contain no credentials.
        error = sys.exc_info()[1]
        print(str(error) if isinstance(error, PublishError) else "publisher operation failed; inspect trusted inputs and credentials", file=sys.stderr)
        raise SystemExit(1)
