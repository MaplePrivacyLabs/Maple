"""Download exact qualified packages or publication stages into private state.

Native gh owns authentication and redirects. Downloads do not verify package
attestations or execute packages: canonical prepare and bundle verification own
those separate boundaries.
"""

from __future__ import annotations

from datetime import datetime, timezone
import hashlib
import os
from pathlib import Path, PurePosixPath
import re
import resource
import stat
import struct
import subprocess
import zipfile

from .bundle import BundleVerifier
from .context import (OperatorError, need, sha, decode, TARGETS, selected_platforms,
                      manifest_names, envelope_names, platform_counts)


SCHEMA_BINDINGS = "agent-release-bindings-v1"
SCHEMA_FREEZE = "agent-release-prepared-freeze-v1"
SCHEMA_DOWNLOAD = "agent-release-stage-download-v1"
PLATFORMS = set(TARGETS)
COUNTS = {"macos": 7, "linux": 4, "windows": 5, "windows-arm64": 5}
SUFFIXES = {"macos": {".app.tar.gz", ".dmg"}, "linux": {".tar.gz"},
            "windows": {".tar.gz", "-setup.exe"}, "windows-arm64": {".tar.gz", "-setup.exe"}}
STAGES = ("prepared", "reserved", "signed")
STAGE_COUNTS = {"prepared": 23, "reserved": 24, "signed": 27}
MAX_METADATA = 4 * 1024**2
MAX_ZIP = MAX_FILE = 512 * 1024**2
MAX_TOTAL = 1024**3
CHUNK = 1024**2
MIN_TRANSFER_TIMEOUT_SECONDS = 900
MAX_TRANSFER_TIMEOUT_SECONDS = 3600
TRANSFER_BYTES_PER_SECOND = 128 * 1024
TRANSFER_STARTUP_SECONDS = 120
HEX = re.compile(r"[0-9a-f]{64}\Z")


def transfer_timeout(size):
    need(type(size) is int and 0 < size <= MAX_ZIP, "invalid ZIP transfer size")
    seconds = TRANSFER_STARTUP_SECONDS + (size + TRANSFER_BYTES_PER_SECOND - 1) // TRANSFER_BYTES_PER_SECOND
    return min(MAX_TRANSFER_TIMEOUT_SECONDS, max(MIN_TRANSFER_TIMEOUT_SECONDS, seconds))


def all_platforms(value):
    try:
        selected_platforms(value)
        return True
    except OperatorError:
        return False


def positive(value):
    need(isinstance(value, str) and re.fullmatch(r"[1-9][0-9]{0,19}", value)
         and int(value) < 2**64, "invalid positive integer")
    return int(value)


def digest(value):
    need(isinstance(value, str) and HEX.fullmatch(value), "digest must be lowercase SHA-256")
    return value


def cap(maximum):
    def apply():
        resource.setrlimit(resource.RLIMIT_FSIZE, (maximum, maximum))
    return apply


def read_record(path, maximum=MAX_METADATA, capture=False):
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(descriptor, "rb") as handle:
        before = os.fstat(handle.fileno())
        need(stat.S_ISREG(before.st_mode) and before.st_nlink == 1
             and before.st_uid == os.getuid() and 0 <= before.st_size <= maximum,
             "unsafe or oversized local file")
        hashed, data, total = hashlib.sha256(), bytearray(), 0
        while block := handle.read(min(CHUNK, maximum - total + 1)):
            total += len(block)
            need(total <= maximum, "file grew beyond bound")
            hashed.update(block)
            if capture:
                data.extend(block)
        after = os.fstat(handle.fileno())
        fields = ("st_dev", "st_ino", "st_size", "st_mtime_ns", "st_ctime_ns")
        need(total == before.st_size and all(getattr(before, field) == getattr(after, field) for field in fields),
             "file changed during validation")
    return {"size": total, "sha256": hashed.hexdigest()}, bytes(data)


class DownloadBase:
    def __init__(self, ctx):
        self.ctx = ctx

    def get(self, path):
        return self.ctx.gh_get(path, maximum=2 * 1024**2)

    def download(self, artifact_id, output, maximum, timeout):
        need(type(artifact_id) is int and 0 < artifact_id < 2**64, "invalid artifact ID")
        try:
            result = subprocess.run(
                [str(self.ctx.tool("gh")), "api", "--hostname", "github.com", "--method", "GET",
                 f"repos/{self.ctx.core.REPOSITORY}/actions/artifacts/{artifact_id}/zip"],
                stdin=subprocess.DEVNULL, stdout=output, stderr=subprocess.DEVNULL,
                timeout=timeout, check=False, preexec_fn=cap(maximum))
        except subprocess.TimeoutExpired:
            raise OperatorError(
                f"GitHub GET exceeded the {timeout}-second transfer timeout; partial archive retained"
            ) from None
        need(result.returncode == 0, "GitHub GET failed or exceeded its bound")


class ProducerDownloader(DownloadBase):
    def checked_producer(self):
        ctx = self.ctx
        run = self.get(f"actions/runs/{ctx.producer_run}")
        need(run.get("repository", {}).get("full_name") == ctx.core.REPOSITORY
             and run.get("head_repository", {}).get("full_name") == ctx.core.REPOSITORY,
             "producer repository differs")
        need(type(run.get("id")) is int and run["id"] == ctx.producer_run
             and type(run.get("run_attempt")) is int and run["run_attempt"] == ctx.producer_attempt
             and run.get("head_sha") == ctx.source and run.get("head_branch") == "master"
             and run.get("path") == ".github/workflows/agent-desktop-build.yml"
             and run.get("event") in ("push", "workflow_dispatch") and run.get("status") == "completed"
             and type(run.get("run_number")) is int and run["run_number"] == int(ctx.build),
             "selected producer differs")
        return run

    def qualification(self):
        ctx = self.ctx
        need(type(ctx.producer_run) is int and 0 < ctx.producer_run < 2**64
             and type(ctx.producer_attempt) is int and 0 < ctx.producer_attempt < 2**64
             and isinstance(ctx.source, str) and re.fullmatch(r"[0-9a-f]{40}", ctx.source)
             and isinstance(ctx.build, str) and re.fullmatch(r"[1-9][0-9]{0,17}", ctx.build),
             "invalid producer identity")
        platforms = selected_platforms(ctx.bindings["platforms"])
        need(set(ctx.bindings["artifacts"]) == set(platforms), "all selected platform artifacts required")
        for platform, item in ctx.bindings["artifacts"].items():
            need(type(item.get("id")) is int and 0 < item["id"] < 2**64
                 and type(item.get("size")) is int and 0 < item["size"] <= MAX_ZIP
                 and isinstance(item.get("zip_sha256"), str) and HEX.fullmatch(item["zip_sha256"])
                 and item.get("name") == f"maple-agent-prod-{TARGETS[platform]}-{ctx.producer_run}",
                 "invalid final artifact binding")
        root = ctx.root.lstat()
        need(stat.S_ISDIR(root.st_mode) and stat.S_IMODE(root.st_mode) == 0o700
             and root.st_uid == os.getuid() and ctx.root.resolve(strict=True) == ctx.root,
             "owned release state must be private0700")
        proof = decode(ctx.pinned(ctx.bindings["qualification"]))
        need(proof.get("required_included_jobs_all_success") is True and proof.get("producer_terminal") is True
             and type(proof.get("run_id")) is int and type(proof.get("attempt")) is int
             and (proof["run_id"], proof["attempt"], proof.get("source"), proof.get("version"))
             == (ctx.producer_run, ctx.producer_attempt, ctx.source, ctx.version)
             and type(proof.get("build")) is int and proof["build"] == int(ctx.build)
             and proof.get("scope", {}).get("include_linux") is True
             and proof.get("scope", {}).get("include_windows") is True
             and proof.get("scope", {}).get("include_windows_arm64", False) is ("windows-arm64" in platforms),
             "qualified producer binding differs")
        return proof

    def extract(self, archive, directory, platform):
        ctx = self.ctx
        directory.mkdir(mode=0o700)
        records = {}
        with zipfile.ZipFile(archive) as bundle:
            entries = bundle.infolist()
            need(len(entries) == COUNTS[platform] and len({item.filename for item in entries}) == len(entries)
                 and sum(item.file_size for item in entries) <= 256 * 1024**2,
                 "producer ZIP inventory or expanded bound differs")
            for entry in entries:
                name = entry.filename
                need(name == entry.orig_filename and re.fullmatch(r"[A-Za-z0-9_.-]{1,200}", name)
                     and name not in (".", "..") and not entry.is_dir()
                     and stat.S_IFMT(entry.external_attr >> 16) in (0, stat.S_IFREG)
                     and not entry.flag_bits & 1
                     and entry.compress_type in (zipfile.ZIP_STORED, zipfile.ZIP_DEFLATED),
                     "unsafe producer ZIP member")
                maximum = 128 * 1024**2 if name.endswith((".tar.gz", ".dmg", "-setup.exe")) else MAX_METADATA
                need(0 < entry.file_size <= maximum, "producer ZIP member exceeds bound")
                fd = os.open(directory / name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
                total, hashed = 0, hashlib.sha256()
                with os.fdopen(fd, "wb") as output, bundle.open(entry) as incoming:
                    while block := incoming.read(min(CHUNK, entry.file_size - total + 1)):
                        total += len(block)
                        need(total <= entry.file_size, "producer ZIP member grew beyond bound")
                        hashed.update(block)
                        output.write(block)
                    output.flush()
                    os.fsync(output.fileno())
                need(total == entry.file_size, "truncated producer ZIP member")
                records[name] = {"size": total, "sha256": hashed.hexdigest()}
        required = {"manifest.json", "build-info.json", "SHA256SUMS"}
        if platform == "macos":
            required |= {"notarization-log.json", "dmg-notarization-log.json"}
        payloads = set(records) - required
        suffixes, revisions = set(), set()
        for name in payloads:
            match = re.fullmatch(r"maple-agent-prod-" + re.escape(ctx.version)
                                 + r"-([0-9a-f]{7,40})-" + re.escape(TARGETS[platform]) + r"(.+)", name)
            need(match is not None and ctx.source.startswith(match[1]) and match[2] in SUFFIXES[platform],
                 "producer payload version/source/target differs")
            revisions.add(match[1])
            suffixes.add(match[2])
        need(required <= set(records) and len(payloads) == len(SUFFIXES[platform])
             and suffixes == SUFFIXES[platform] and len(revisions) == 1, "producer payload inventory differs")
        return records

    def run(self, args):
        need(args.network is True, "native GitHub GETs require explicit --network")
        need(args.platform in selected_platforms(self.ctx.bindings["platforms"]), "producer platform was not selected")
        os.umask(0o077)
        ctx = self.ctx
        proof = self.qualification()
        before = self.checked_producer()
        selected = ctx.bindings["artifacts"][args.platform]
        artifact_id, size, expected = selected["id"], selected["size"], selected["zip_sha256"]
        artifact = self.get(f"actions/artifacts/{artifact_id}")
        need(type(artifact.get("id")) is int and artifact["id"] == artifact_id
             and artifact.get("name") == selected["name"] and artifact.get("expired") is False
             and type(artifact.get("size_in_bytes")) is int and artifact["size_in_bytes"] == size
             and artifact.get("digest") == "sha256:" + expected
             and type(artifact.get("workflow_run", {}).get("id")) is int
             and artifact["workflow_run"]["id"] == ctx.producer_run
             and artifact["workflow_run"].get("head_sha") == ctx.source
             and artifact["workflow_run"].get("head_branch") == "master", "final artifact differs")
        qualified = [item for item in proof["selected_final_artifact_metadata"] if item.get("id") == artifact_id]
        need(len(qualified) == 1 and all(artifact.get(key) == qualified[0].get(key)
             for key in ("id", "name", "digest", "size_in_bytes", "workflow_run")),
             "current artifact differs from qualification")
        archive = ctx.root / f"producer-{args.platform}-{artifact_id}.zip"
        directory = ctx.root / f"packages-{args.platform}-{artifact_id}"
        need(not archive.exists() and not archive.is_symlink()
             and not directory.exists() and not directory.is_symlink(), "producer destination already exists")
        fd = os.open(archive, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd, "wb") as output:
            self.download(artifact_id, output, size + 1, transfer_timeout(size))
            output.flush()
            os.fsync(output.fileno())
        actual, _ = read_record(archive, MAX_ZIP)
        need(actual == {"size": size, "sha256": expected}, "producer ZIP size/digest differs")
        records = self.extract(archive, directory, args.platform)
        after = self.checked_producer()
        need(all(before[key] == after[key] for key in ("id", "run_attempt", "head_sha", "run_number", "created_at")),
             "producer changed during download")
        receipt = {"at": datetime.now(timezone.utc).isoformat(), "producer": ctx.producer_run,
                   "source": ctx.source, "producer_attempt": ctx.producer_attempt, "build": ctx.build,
                   "version": ctx.version, "bindings_sha256": ctx.bindings_sha256,
                   "tooling_sha256": ctx.tooling_sha256,
                   "current_producer": {key: after[key] for key in ("id", "run_attempt", "head_sha", "run_number", "created_at", "status", "conclusion")},
                   "platform": args.platform, "artifact": {key: artifact[key] for key in
                   ("id", "name", "digest", "size_in_bytes", "created_at", "expired", "workflow_run")},
                   "archive": str(archive), "actual_zip_sha256": expected, "directory": str(directory),
                   "files": records,
                   "scope": "Authenticated artifact download and bounded extraction only; attestations and package execution are separate."}
        ctx.emit(ctx.root / f"producer-{args.platform}-download.json", receipt)
        return receipt


class StageDownloader(DownloadBase):
    def frozen_inputs(self):
        ctx, bindings, core = self.ctx, self.ctx.bindings, self.ctx.core
        platforms = selected_platforms(bindings.get("platforms"))
        arm64 = "windows-arm64" in platforms
        need(bindings.get("schema") == SCHEMA_BINDINGS
             and all_platforms(bindings.get("platforms")), "wrong binding schema/platforms")
        publication = ctx.publication
        need(set(publication) == {"run_id", "run_attempt", "source_sha", "workflow"}
             and type(publication["run_id"]) is int and 0 < publication["run_id"] < 2**64
             and type(publication["run_attempt"]) is int and 0 < publication["run_attempt"] < 2**64
             and publication["workflow"] == ".github/workflows/agent-updates-publish.yml"
             and re.fullmatch(r"[0-9a-f]{40}", publication["source_sha"]), "invalid publication identity")
        selected = decode(ctx.pinned(bindings["selected_publication"]))
        expected_inputs = {"include_linux": True, "include_windows": True,
                           "include_windows_arm64": arm64, "version": ctx.version,
                           "build_run_id": str(ctx.producer_run),
                           "notes": ctx.pinned(bindings["notes"]).decode("utf-8")}
        legacy_inputs = {key: value for key, value in expected_inputs.items() if key != "include_windows_arm64"}
        input_match = selected.get("inputs") == expected_inputs or (not arm64 and selected.get("inputs") == legacy_inputs)
        need(type(selected.get("run_id")) is int and type(selected.get("run_attempt")) is int
             and type(selected.get("producer_run")) is int and type(selected.get("producer_attempt")) is int
             and all(selected.get(key) == value for key, value in publication.items())
             and selected.get("payload_source") == ctx.source and selected.get("producer_run") == ctx.producer_run
             and selected.get("producer_attempt") == ctx.producer_attempt and selected.get("version") == ctx.version
             and selected.get("platforms") == list(platforms) and input_match
             and selected.get("inputs", {}).get("include_linux") is True
             and selected.get("inputs", {}).get("include_windows") is True
             and selected.get("inputs", {}).get("include_windows_arm64", False) is arm64,
             "selected publication/producer differs")
        frozen = decode(ctx.pinned(bindings["freeze"]))
        identity, producer = ctx.identity, frozen["producer"]
        need(frozen.get("schema") == SCHEMA_FREEZE and frozen.get("canonical_prepare_completed") is True
             and frozen.get("source") == ctx.source and frozen.get("tree") == ctx.tree
             and frozen.get("platforms") == list(platforms) and identity.get("version") == ctx.version
             and identity.get("build") == ctx.build and identity.get("source_sha") == ctx.source
             and type(identity.get("run_id")) is int and identity["run_id"] == ctx.producer_run
             and type(identity.get("run_attempt")) is int and identity["run_attempt"] == ctx.producer_attempt
             and type(producer.get("run_id")) is int and type(producer.get("run_attempt")) is int
             and all(producer.get(key) == identity[key] for key in ("run_id", "run_attempt", "source_sha", "build", "pub_date"))
             and bindings.get("payload_tree") == ctx.tree, "frozen producer/release binding differs")
        files = frozen.get("files")
        roots = set(manifest_names(platforms)) | {"metadata-inventory.json", "public-key.pub", "trust.json"}
        metadata = {"metadata/" + name for name in {"provenance.json"} | core.MACOS_METADATA_NAMES
                    | core.LINUX_METADATA_NAMES | core.WINDOWS_METADATA_NAMES
                    | (core.WINDOWS_ARM64_METADATA_NAMES if arm64 else set())}
        need(len(metadata) == 12 + 3 * arm64 and isinstance(files, dict)
             and len(files) == platform_counts(platforms)["prepared"]
             and roots | metadata <= set(files) and bindings["prepared_files"] == files,
             "prepared freeze inventory differs")
        payloads = set(files) - roots - metadata
        suffixes = set()
        expected_suffixes = {TARGETS[platform] + suffix for platform in platforms for suffix in SUFFIXES[platform]}
        pattern = (r"payloads/maple-agent-prod-" + re.escape(ctx.version)
                   + r"-([0-9a-f]{7,40})-(" + "|".join(re.escape(suffix) for suffix in sorted(expected_suffixes)) + ")")
        for name in payloads:
            match = re.fullmatch(pattern, name)
            need(match is not None and ctx.source.startswith(match[1]), "frozen payload name/source differs")
            suffixes.add(match[2])
        need(len(payloads) == len(expected_suffixes) and suffixes == expected_suffixes,
             "frozen platform payload inventory differs")
        for name, record in files.items():
            maximum = MAX_FILE if name.startswith("payloads/") else MAX_METADATA
            need(isinstance(record, dict) and set(record) == {"size", "sha256"}
                 and type(record["size"]) is int and 0 < record["size"] <= maximum
                 and isinstance(record["sha256"], str) and HEX.fullmatch(record["sha256"]), "invalid frozen file record")
        need(set(bindings["artifacts"]) == set(platforms)
             and producer["artifacts"] == {key: item["name"] for key, item in bindings["artifacts"].items()}
             and frozen["notes"]["sha256"] == bindings["notes"]["sha256"], "frozen notes/artifact projection differs")
        for key in ("qualification", "notes"):
            ctx.pinned(bindings[key])
        verifier = BundleVerifier(ctx)
        checked, checked_core = verifier.setup()
        need(checked == bindings and checked_core is core, "bundle verifier inputs changed")
        names = {stage: f"agent-update-{stage}-{publication['run_id']}" for stage in STAGES}
        return {"publisher_source": publication["source_sha"], "attempt": publication["run_attempt"], "artifacts": names}, files

    def remote_binding(self, args, expected):
        ctx = self.ctx
        run_id, attempt = ctx.publication["run_id"], ctx.publication["run_attempt"]
        run = self.get(f"actions/runs/{run_id}")
        need(type(run.get("id")) is int and run["id"] == run_id
             and run.get("repository", {}).get("full_name") == ctx.core.REPOSITORY
             and run.get("head_repository", {}).get("full_name") == ctx.core.REPOSITORY
             and run.get("path") == ".github/workflows/agent-updates-publish.yml"
             and run.get("head_branch") == "master" and run.get("head_sha") == expected["publisher_source"]
             and run.get("event") == "workflow_dispatch" and type(run.get("run_attempt")) is int
             and run["run_attempt"] == attempt
             and run.get("display_title") == f"Publish Agent {ctx.version} from verified build {ctx.producer_run}",
             "remote publication run differs")
        result = self.get(f"actions/runs/{run_id}/artifacts?per_page=100")
        need(type(result.get("total_count")) is int and result["total_count"] == len(result.get("artifacts", [])),
             "incomplete artifact inventory")
        matches = [item for item in result["artifacts"] if item.get("name") == expected["artifacts"][args.stage]]
        need(len(matches) == 1, "stage artifact missing or duplicated")
        artifact = matches[0]
        need(artifact.get("name") == args.artifact_name == expected["artifacts"][args.stage]
             and type(artifact.get("id")) is int and artifact["id"] == args.artifact_id
             and artifact.get("expired") is False and artifact.get("digest") == "sha256:" + args.zip_sha256
             and type(artifact.get("size_in_bytes")) is int and artifact["size_in_bytes"] == args.zip_size
             and type(artifact.get("workflow_run", {}).get("id")) is int
             and artifact["workflow_run"]["id"] == run_id
             and artifact["workflow_run"].get("head_sha") == expected["publisher_source"],
             "stage artifact identity/ZIP digest differs")
        result = self.get(f"actions/runs/{run_id}/attempts/{attempt}/jobs?per_page=100")
        need(type(result.get("total_count")) is int and result["total_count"] == len(result.get("jobs", [])),
             "incomplete job inventory")
        producer_name = {"prepared": "verify", "reserved": "reserve", "signed": "sign"}[args.stage]
        producers = [job for job in result["jobs"] if job.get("name") == producer_name]
        need(len(producers) == 1 and producers[0].get("status") == "completed"
             and producers[0].get("conclusion") == "success" and type(producers[0].get("id")) is int
             and producers[0]["id"] == args.producer_job_id
             and type(producers[0].get("run_id")) is int and producers[0]["run_id"] == run_id,
             "exact stage producer has not succeeded in the selected attempt")
        created, started, completed = (datetime.fromisoformat(value.replace("Z", "+00:00")) for value in
                                      (artifact["created_at"], producers[0]["started_at"], producers[0]["completed_at"]))
        need(all(value.tzinfo is not None for value in (created, started, completed)) and started <= created <= completed,
             "artifact is not fresh from the selected attempt's producer job")
        return artifact, producers[0]["id"]

    @staticmethod
    def central_directory(archive, maximum_entries=29):
        size = archive.stat().st_size
        with archive.open("rb") as source:
            source.seek(max(0, size - 65557))
            tail = source.read(65557)
        position = tail.rfind(b"PK\x05\x06")
        need(position >= 0 and len(tail) - position >= 22, "missing ZIP end record")
        _, disk, central_disk, on_disk, count, central_size, offset, comment = struct.unpack_from("<4s4H2LH", tail, position)
        end_offset = size - len(tail) + position
        need(disk == central_disk == 0 and on_disk == count and 0 < count <= maximum_entries
             and 0 < central_size <= 64 * 1024 and offset + central_size == end_offset
             and position + 22 + comment == len(tail), "split, Zip64, oversized or malformed ZIP directory")
        return count

    @classmethod
    def zip_inventory(cls, archive, stage, frozen_files, platforms=("macos", "linux", "windows")):
        platforms = selected_platforms(list(platforms))
        envelopes = set(envelope_names(platforms))
        count = cls.central_directory(archive, platform_counts(platforms)["signed"] + 2)
        with zipfile.ZipFile(archive) as zipped:
            entries = zipped.infolist()
            need(len(entries) == count, "ZIP entry count differs")
            leaves, names, total = {}, set(), 0
            for member in entries:
                raw, name = member.filename, PurePosixPath(member.filename)
                need(raw == member.orig_filename and raw and len(raw.encode()) <= 1024 and "\\" not in raw
                     and not any(ord(char) < 32 or ord(char) > 126 for char in raw)
                     and not name.is_absolute() and ".." not in name.parts and raw not in names
                     and not member.flag_bits & 1, "unsafe, duplicate or encrypted ZIP entry")
                names.add(raw)
                kind = stat.S_IFMT(member.external_attr >> 16)
                need(member.compress_type in {zipfile.ZIP_STORED, zipfile.ZIP_DEFLATED}, "unsupported ZIP compression")
                if member.is_dir():
                    need(raw in {"metadata/", "payloads/"} and kind in {0, stat.S_IFDIR} and member.file_size == 0,
                         "unexpected ZIP directory")
                    continue
                need(str(name) == raw and kind in {0, stat.S_IFREG}, "noncanonical, symlink or special ZIP member")
                maximum = MAX_FILE if raw.startswith("payloads/") else MAX_METADATA
                need(0 < member.file_size <= maximum and 0 <= member.compress_size <= MAX_ZIP, "ZIP member exceeds bounds")
                total += member.file_size
                need(total <= MAX_TOTAL and (member.file_size <= CHUNK or member.file_size <= max(1, member.compress_size) * 200),
                     "ZIP expansion or compression ratio exceeds bounds")
                leaves[raw] = member
            expected = set(frozen_files)
            if stage != "prepared":
                expected.add("reservation.json")
            if stage == "signed":
                expected |= envelopes
            need(set(leaves) == expected and len(leaves) == platform_counts(platforms)[stage], "ZIP leaf inventory differs from selected stage")
            for name, record in frozen_files.items():
                need(leaves[name].file_size == record["size"], "frozen ZIP member size differs")
            if "reservation.json" in leaves:
                need(leaves["reservation.json"].file_size <= 64 * 1024, "oversized reservation")
            for name in envelopes & set(leaves):
                need(leaves[name].file_size <= 128 * 1024, "oversized envelope")
        return expected

    @staticmethod
    def extract(archive, destination, expected, frozen_files, reservation):
        destination.mkdir(mode=0o700)
        files = {}
        with zipfile.ZipFile(archive) as zipped:
            for name in sorted(expected):
                member = zipped.getinfo(name)
                target = destination / name
                target.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
                fd = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
                total, hashed = 0, hashlib.sha256()
                with os.fdopen(fd, "wb") as output, zipped.open(member) as incoming:
                    while block := incoming.read(min(CHUNK, member.file_size - total + 1)):
                        total += len(block)
                        need(total <= member.file_size, "ZIP member expanded past its declared bound")
                        hashed.update(block)
                        output.write(block)
                    output.flush()
                    os.fsync(output.fileno())
                need(total == member.file_size, "ZIP member is truncated")
                files[name] = {"size": total, "sha256": hashed.hexdigest()}
                if name in frozen_files:
                    need(files[name] == frozen_files[name], "extracted frozen file hash differs")
                if name == "reservation.json":
                    need(files[name] == {"size": len(reservation), "sha256": sha(reservation)}, "reservation bytes differ")
        return files

    def run(self, args):
        need(args.network is True, "native GitHub GETs require explicit --network")
        need(args.stage in STAGES, "unknown publication stage")
        need(type(args.artifact_id) is int and 0 < args.artifact_id < 2**64
             and type(args.producer_job_id) is int and 0 < args.producer_job_id < 2**64
             and type(args.zip_size) is int and 0 < args.zip_size <= MAX_ZIP, "invalid stage artifact bounds")
        digest(args.zip_sha256)
        os.umask(0o077)
        ctx = self.ctx
        output = args.output_dir.absolute()
        parent = output.parent
        need(output.is_absolute() and parent.resolve(strict=True) == parent
             and ctx.root.resolve(strict=True) in (parent, *parent.parents)
             and stat.S_ISDIR(parent.lstat().st_mode) and parent.stat().st_uid == os.getuid()
             and not output.exists() and not output.is_symlink(), "output must be absent under owned release state")
        expected_run, frozen_files = self.frozen_inputs()
        artifact, producer_job = self.remote_binding(args, expected_run)
        output.mkdir(mode=0o700)
        archive = output / "artifact.zip"
        fd = os.open(archive, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd, "wb") as stream:
            self.download(artifact["id"], stream, args.zip_size + 1, transfer_timeout(args.zip_size))
            stream.flush()
            os.fsync(stream.fileno())
        actual, _ = read_record(archive, MAX_ZIP)
        need(actual == {"size": args.zip_size, "sha256": args.zip_sha256}, "downloaded ZIP size/digest differs")
        platforms = selected_platforms(ctx.bindings["platforms"])
        expected = self.zip_inventory(archive, args.stage, frozen_files, platforms)
        destination = output / "bundle"
        reservation_fields = {"manifest_sha256": frozen_files["manifest.json"]["sha256"],
                                           "linux_manifest_sha256": frozen_files["linux-manifest.json"]["sha256"],
                                           "windows_manifest_sha256": frozen_files["windows-manifest.json"]["sha256"],
                                           "metadata_inventory_sha256": frozen_files["metadata-inventory.json"]["sha256"],
                                           "version": ctx.version}
        if "windows-arm64" in platforms:
            reservation_fields["windows_arm64_manifest_sha256"] = frozen_files["windows-arm64-manifest.json"]["sha256"]
        reservation = ctx.core.encode_json(reservation_fields)
        files = self.extract(archive, destination, expected, frozen_files, reservation)
        record = {"schema": SCHEMA_DOWNLOAD, "at": datetime.now(timezone.utc).isoformat(),
                  "platforms": list(platforms), "stage": args.stage,
                  "publication_run": ctx.publication["run_id"], "publisher_source": expected_run["publisher_source"],
                  "attempt": ctx.publication["run_attempt"], "producer": ctx.producer_run,
                  "source": ctx.source, "build": ctx.build, "version": ctx.version,
                  "producer_attempt": ctx.producer_attempt,
                  "artifact": {name: artifact[name] for name in ("id", "name", "digest", "size_in_bytes")},
                  "actual_zip_sha256": actual["sha256"], "actual_zip_size": actual["size"],
                  "bindings_sha256": ctx.bindings_sha256, "tooling_sha256": ctx.tooling_sha256,
                  "selected_publication_sha256": ctx.bindings["selected_publication"]["sha256"],
                  "freeze_sha256": ctx.bindings["freeze"]["sha256"], "files": files,
                  "destination": str(destination), "producer_job_id": producer_job, "recovered_envelopes": [],
                  "scope": "Native GitHub GETs and private bounded ZIP extraction only; signatures and approval remain separate."}
        ctx.emit(output / "download.json", record)
        return record


def add_arguments(parser, operation):
    parser.add_argument("--network", action="store_true")
    if operation == "download-producer":
        parser.add_argument("platform", choices=tuple(TARGETS))
    elif operation == "download-stage":
        parser.add_argument("stage", choices=STAGES)
        parser.add_argument("--producer-job-id", required=True, type=positive)
        parser.add_argument("--artifact-id", required=True, type=positive)
        parser.add_argument("--artifact-name", required=True)
        parser.add_argument("--zip-sha256", required=True, type=digest)
        parser.add_argument("--zip-size", required=True, type=positive)
        parser.add_argument("--output-dir", required=True, type=Path)
    else:
        raise ValueError("unknown download operation")
