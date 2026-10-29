"""Offline release-download contracts; no GitHub, credentials or package execution."""

from copy import deepcopy
import json
from pathlib import Path
import stat
import struct
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch
import warnings
import zipfile

import publisher as p
from release_helpers import downloads as d
from release_helpers.context import OperatorError


class DownloadTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.root.chmod(0o700)
        self.references = {}
        self.ctx = SimpleNamespace(
            root=self.root, repo=p.ROOT, core=p, source="a" * 40, tree="c" * 40,
            version="2.7.19", build="42", producer_run=501, producer_attempt=2,
            tooling_sha256="d" * 64, bindings_sha256="e" * 64,
            publication={"run_id": 901, "run_attempt": 3, "source_sha": "b" * 40,
                         "workflow": ".github/workflows/agent-updates-publish.yml"})
        self.ctx.identity = {"run_id": 501, "run_attempt": 2, "source_sha": self.ctx.source,
                             "version": self.ctx.version, "build": "42", "pub_date": "2026-10-29T01:00:00Z"}
        self.ctx.bindings = {
            "schema": d.SCHEMA_BINDINGS, "platforms": ["macos", "linux", "windows"],
            "payload_tree": self.ctx.tree,
            "artifacts": {platform: {"id": number, "name": f"maple-agent-prod-{target}-501",
                                      "size": 1, "zip_sha256": "f" * 64}
                          for number, (platform, target) in enumerate(d.TARGETS.items(), 11)}}
        self.ctx.pinned = self.pinned
        self.ctx.emit = self.emit
        self.ctx.tool = Mock(return_value=Path("/synthetic/gh"))
        self.qualification = {"required_included_jobs_all_success": True, "producer_terminal": True,
                              "run_id": 501, "attempt": 2, "source": self.ctx.source,
                              "version": self.ctx.version, "build": 42,
                              "scope": {"include_linux": True, "include_windows": True},
                              "selected_final_artifact_metadata": []}
        self.ctx.bindings["qualification"] = self.reference("qualification", self.qualification)
        self.ctx.bindings["notes"] = self.reference("notes", b"Release notes\n")
        self.files = self.prepared_files()
        self.freeze = {"schema": d.SCHEMA_FREEZE, "canonical_prepare_completed": True,
                       "source": self.ctx.source, "tree": self.ctx.tree,
                       "platforms": ["macos", "linux", "windows"],
                       "producer": {**self.ctx.identity,
                                    "artifacts": {key: value["name"] for key, value in self.ctx.bindings["artifacts"].items()}},
                       "files": self.records(self.files), "notes": self.ctx.bindings["notes"]}
        self.ctx.bindings["prepared_files"] = self.freeze["files"]
        self.ctx.bindings["freeze"] = self.reference("freeze", self.freeze)
        self.selection = {**self.ctx.publication, "producer_run": 501, "producer_attempt": 2,
                          "payload_source": self.ctx.source, "version": self.ctx.version,
                          "platforms": ["macos", "linux", "windows"],
                          "inputs": {"include_linux": True, "include_windows": True,
                                     "version": self.ctx.version, "build_run_id": "501"}}
        self.ctx.bindings["selected_publication"] = self.reference("selected", self.selection)

    def reference(self, name, value):
        data = value if isinstance(value, bytes) else p.encode_json(value)
        self.references[name] = data
        return {"path": name, "sha256": d.sha(data)}

    def pinned(self, reference):
        data = self.references[reference["path"]]
        p.need(d.sha(data) == reference["sha256"], "test reference changed")
        return data

    def emit(self, path, value):
        data = p.encode_json(value)
        with path.open("xb") as output:
            output.write(data)
        return d.sha(data)

    @staticmethod
    def records(files):
        return {name: {"size": len(data), "sha256": d.sha(data)} for name, data in files.items()}

    def prepared_files(self):
        files = {name: b"{}\n" for name in ("manifest.json", "linux-manifest.json", "windows-manifest.json",
                                           "metadata-inventory.json", "public-key.pub", "trust.json")}
        files.update({"metadata/" + name: b"metadata\n" for name in
                      {"provenance.json"} | p.MACOS_METADATA_NAMES | p.LINUX_METADATA_NAMES | p.WINDOWS_METADATA_NAMES})
        for platform, target in d.TARGETS.items():
            for suffix in d.SUFFIXES[platform]:
                files[f"payloads/maple-agent-prod-{self.ctx.version}-{self.ctx.source[:7]}-{target}{suffix}"] = b"payload\n"
        return files

    def stage_files(self, stage):
        files = dict(self.files)
        reservation = p.encode_json({"manifest_sha256": self.freeze["files"]["manifest.json"]["sha256"],
                                     "linux_manifest_sha256": self.freeze["files"]["linux-manifest.json"]["sha256"],
                                     "windows_manifest_sha256": self.freeze["files"]["windows-manifest.json"]["sha256"],
                                     "metadata_inventory_sha256": self.freeze["files"]["metadata-inventory.json"]["sha256"],
                                     "version": self.ctx.version})
        if stage != "prepared":
            files["reservation.json"] = reservation
        if stage == "signed":
            files.update({name: b"signed\n" for name in ("envelope.json", "linux-envelope.json", "windows-envelope.json")})
        return files, reservation

    def archive(self, files, filename="test.zip", directories=False):
        archive = self.root / filename
        with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as zipped:
            if directories:
                zipped.writestr("metadata/", b"")
                zipped.writestr("payloads/", b"")
            for name, data in files.items():
                zipped.writestr(name, data)
        return archive

    def args(self, stage="prepared"):
        return SimpleNamespace(network=True, stage=stage, producer_job_id=1001, artifact_id=701,
                               artifact_name=f"agent-update-{stage}-901", zip_sha256="f" * 64,
                               zip_size=1234, output_dir=self.root / stage)

    def remote(self, args):
        run = {"id": 901, "repository": {"full_name": p.REPOSITORY},
               "head_repository": {"full_name": p.REPOSITORY},
               "path": self.ctx.publication["workflow"], "head_branch": "master",
               "head_sha": self.ctx.publication["source_sha"], "event": "workflow_dispatch", "run_attempt": 3,
               "display_title": f"Publish Agent {self.ctx.version} from verified build 501"}
        artifact = {"id": 701, "name": args.artifact_name, "expired": False,
                    "digest": "sha256:" + args.zip_sha256, "size_in_bytes": args.zip_size,
                    "workflow_run": {"id": 901, "head_sha": self.ctx.publication["source_sha"]},
                    "created_at": "2026-10-29T02:01:00Z"}
        job = {"id": 1001, "name": {"prepared": "verify", "reserved": "reserve", "signed": "sign"}[args.stage],
               "status": "completed", "conclusion": "success", "run_id": 901,
               "started_at": "2026-10-29T02:00:00Z", "completed_at": "2026-10-29T02:02:00Z"}
        expected = {"publisher_source": self.ctx.publication["source_sha"],
                    "artifacts": {stage: f"agent-update-{stage}-901" for stage in d.STAGES}}
        return [run, {"total_count": 1, "artifacts": [artifact]}, {"total_count": 1, "jobs": [job]}], expected

    def test_frozen_versions_are_data_and_payload_source_is_separate_from_publisher(self):
        for version in ("2.7.19", "0.0.25"):
            with self.subTest(version=version):
                self.ctx.version = self.ctx.identity["version"] = version
                self.selection["version"] = self.selection["inputs"]["version"] = version
                self.ctx.bindings["selected_publication"] = self.reference("selected", self.selection)
                self.files = self.prepared_files()
                self.freeze["files"] = self.ctx.bindings["prepared_files"] = self.records(self.files)
                self.ctx.bindings["freeze"] = self.reference("freeze", self.freeze)
                with patch.object(d, "BundleVerifier") as verifier:
                    verifier.return_value.setup.return_value = (self.ctx.bindings, p)
                    expected, files = d.StageDownloader(self.ctx).frozen_inputs()
                self.assertEqual(expected["publisher_source"], "b" * 40)
                self.assertEqual(files, self.freeze["files"])

    def test_frozen_payload_wrong_version_or_source_is_rejected(self):
        original = deepcopy(self.freeze["files"])
        payload = next(name for name in original if name.startswith("payloads/"))
        for replacement in (payload.replace("2.7.19", "2.7.18"), payload.replace("aaaaaaa", "bbbbbbb")):
            with self.subTest(replacement=replacement):
                files = dict(original)
                files[replacement] = files.pop(payload)
                self.freeze["files"] = self.ctx.bindings["prepared_files"] = files
                self.ctx.bindings["freeze"] = self.reference("freeze", self.freeze)
                with self.assertRaisesRegex(OperatorError, "payload name/source"):
                    d.StageDownloader(self.ctx).frozen_inputs()

    def test_all_stage_inventories_and_optional_directories_extract(self):
        for stage, count in (("prepared", 23), ("reserved", 24), ("signed", 27)):
            with self.subTest(stage=stage):
                files, reservation = self.stage_files(stage)
                archive = self.archive(files, stage + ".zip", directories=True)
                expected = d.StageDownloader.zip_inventory(archive, stage, self.freeze["files"])
                result = d.StageDownloader.extract(archive, self.root / stage, expected, self.freeze["files"], reservation)
                self.assertEqual(len(result), count)
                self.assertEqual(result, self.records(files))

    def test_stage_zip_rejects_unsafe_names_extra_members_and_symlinks(self):
        for name in ("../escape", "/absolute", "payloads\\escape", "./extra", "extra", "metadata/../escape"):
            with self.subTest(name=name):
                archive = self.archive({**self.files, name: b"x"}, "unsafe.zip")
                with self.assertRaises(OperatorError):
                    d.StageDownloader.zip_inventory(archive, "prepared", self.freeze["files"])
        archive = self.archive(self.files, "symlink.zip")
        with zipfile.ZipFile(archive, "a") as zipped:
            link = zipfile.ZipInfo("link")
            link.create_system = 3
            link.external_attr = (stat.S_IFLNK | 0o777) << 16
            zipped.writestr(link, b"target")
        with self.assertRaises(OperatorError):
            d.StageDownloader.zip_inventory(archive, "prepared", self.freeze["files"])

    def test_stage_zip_rejects_duplicate_entries_and_trailing_content(self):
        archive = self.archive(self.files)
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", UserWarning)
            with zipfile.ZipFile(archive, "a") as zipped:
                zipped.writestr("manifest.json", b"{}\n")
        with self.assertRaisesRegex(OperatorError, "duplicate"):
            d.StageDownloader.zip_inventory(archive, "prepared", self.freeze["files"])
        archive = self.archive(self.files, "trailing.zip")
        with archive.open("ab") as stream:
            stream.write(b"trailing")
        with self.assertRaisesRegex(OperatorError, "malformed ZIP"):
            d.StageDownloader.central_directory(archive)

    def test_stage_zip_rejects_split_and_zip64_end_records_before_zipfile(self):
        original = self.archive(self.files).read_bytes()
        for field, value in ((4, 1), (10, 65535), (12, 65536)):
            with self.subTest(field=field):
                data = bytearray(original)
                position = data.rfind(b"PK\x05\x06")
                struct.pack_into("<H" if field < 12 else "<L", data, position + field, value)
                archive = self.root / "malformed.zip"
                archive.write_bytes(data)
                with patch.object(d.zipfile, "ZipFile", side_effect=AssertionError("must not allocate entries")):
                    with self.assertRaisesRegex(OperatorError, "malformed ZIP"):
                        d.StageDownloader.zip_inventory(archive, "prepared", self.freeze["files"])

    def test_member_hash_and_reserved_bytes_remain_independent_of_zip_hash(self):
        files, reservation = self.stage_files("reserved")
        for member, changed in (("manifest.json", b"BAD"), ("reservation.json", b"wrong")):
            with self.subTest(member=member):
                archive = self.archive({**files, member: changed}, "member.zip")
                with self.assertRaisesRegex(OperatorError, "hash differs|reservation bytes"):
                    d.StageDownloader.extract(archive, self.root / member.replace(".", "-"),
                                              set(files), self.freeze["files"], reservation)

    def test_remote_fresh_artifact_matches_exact_job_and_separate_publisher_source(self):
        args = self.args()
        responses, expected = self.remote(args)
        self.ctx.gh_get = Mock(side_effect=responses)
        artifact, job = d.StageDownloader(self.ctx).remote_binding(args, expected)
        self.assertEqual((artifact["id"], job), (701, 1001))
        self.assertEqual(self.ctx.gh_get.call_args_list[2].args[0], "actions/runs/901/attempts/3/jobs?per_page=100")

    def test_remote_rejects_wrong_identity_incomplete_inventory_and_stale_artifacts(self):
        args = self.args()
        base, expected = self.remote(args)
        cases = []
        for key, value in (("head_sha", self.ctx.source), ("run_attempt", 4), ("id", True)):
            rows = deepcopy(base)
            rows[0][key] = value
            cases.append(rows)
        for field in ("artifacts", "jobs"):
            rows = deepcopy(base)
            rows[1 if field == "artifacts" else 2]["total_count"] = 2
            cases.append(rows)
        for stamp in ("2026-10-29T01:59:59Z", "2026-10-29T02:02:01Z", "2026-10-29T02:01:00"):
            rows = deepcopy(base)
            rows[1]["artifacts"][0]["created_at"] = stamp
            cases.append(rows)
        rows = deepcopy(base)
        rows[2]["jobs"][0]["id"] = 1002
        cases.append(rows)
        rows = deepcopy(base)
        rows[1]["artifacts"] *= 2
        rows[1]["total_count"] = 2
        cases.append(rows)
        for number, rows in enumerate(cases):
            with self.subTest(case=number), self.assertRaises(OperatorError):
                self.ctx.gh_get = Mock(side_effect=rows)
                d.StageDownloader(self.ctx).remote_binding(args, expected)

    def test_downloads_need_explicit_network_before_io(self):
        self.ctx.gh_get = Mock(side_effect=AssertionError("network forbidden"))
        for downloader, args in ((d.ProducerDownloader(self.ctx), SimpleNamespace(network=False, platform="linux")),
                                 (d.StageDownloader(self.ctx), self.args())):
            args.network = False
            with self.assertRaisesRegex(OperatorError, "explicit --network"):
                downloader.run(args)
        self.ctx.gh_get.assert_not_called()
        self.assertEqual(list(self.root.iterdir()), [])

    def test_producer_payload_filenames_follow_selected_version(self):
        for version in ("2.7.19", "0.0.25"):
            self.ctx.version = version
            for platform, target in d.TARGETS.items():
                with self.subTest(version=version, platform=platform):
                    files = {name: b"metadata" for name in ("manifest.json", "build-info.json", "SHA256SUMS")}
                    if platform == "macos":
                        files.update({"notarization-log.json": b"log", "dmg-notarization-log.json": b"log"})
                    files.update({f"maple-agent-prod-{version}-aaaaaaa-{target}{suffix}": b"payload"
                                  for suffix in d.SUFFIXES[platform]})
                    archive = self.archive(files, "producer.zip")
                    directory = self.root / f"{version}-{platform}"
                    self.assertEqual(d.ProducerDownloader(self.ctx).extract(archive, directory, platform), self.records(files))

    def producer_remote(self):
        files = {name: b"metadata" for name in ("manifest.json", "build-info.json", "SHA256SUMS")}
        files[f"maple-agent-prod-{self.ctx.version}-aaaaaaa-linux-x86_64.tar.gz"] = b"payload"
        zip_bytes = self.archive(files, "original.zip").read_bytes()
        selected = self.ctx.bindings["artifacts"]["linux"]
        selected["size"], selected["zip_sha256"] = len(zip_bytes), d.sha(zip_bytes)
        artifact = {"id": selected["id"], "name": selected["name"], "expired": False,
                    "size_in_bytes": len(zip_bytes), "digest": "sha256:" + d.sha(zip_bytes),
                    "created_at": "2026-10-29T01:01:00Z",
                    "workflow_run": {"id": 501, "head_sha": self.ctx.source, "head_branch": "master"}}
        self.qualification["selected_final_artifact_metadata"] = [deepcopy(artifact)]
        self.ctx.bindings["qualification"] = self.reference("qualification", self.qualification)
        run = {"repository": {"full_name": p.REPOSITORY}, "head_repository": {"full_name": p.REPOSITORY},
               "id": 501, "run_attempt": 2, "head_sha": self.ctx.source, "head_branch": "master",
               "path": ".github/workflows/agent-desktop-build.yml", "event": "push", "status": "completed",
               "run_number": 42, "created_at": "2026-10-29T01:00:00Z", "conclusion": "success"}
        return zip_bytes, files, [run, artifact, deepcopy(run)]

    def test_producer_download_binds_exact_artifact_zip_and_tooling(self):
        zip_bytes, files, responses = self.producer_remote()
        self.ctx.gh_get = Mock(side_effect=responses)
        downloader = d.ProducerDownloader(self.ctx)
        with patch.object(downloader, "download") as transfer:
            transfer.side_effect = lambda artifact, stream, maximum, timeout: stream.write(zip_bytes)
            receipt = downloader.run(SimpleNamespace(network=True, platform="linux"))
        self.assertEqual(receipt["files"], self.records(files))
        self.assertEqual(receipt["actual_zip_sha256"], d.sha(zip_bytes))
        self.assertEqual(receipt["tooling_sha256"], self.ctx.tooling_sha256)
        self.assertEqual(receipt["bindings_sha256"], self.ctx.bindings_sha256)
        self.assertTrue((self.root / "producer-linux-download.json").is_file())

    def test_producer_attempt_change_during_download_fails_without_receipt(self):
        zip_bytes, _, responses = self.producer_remote()
        responses[-1]["run_attempt"] = 3
        self.ctx.gh_get = Mock(side_effect=responses)
        downloader = d.ProducerDownloader(self.ctx)
        with patch.object(downloader, "download") as transfer:
            transfer.side_effect = lambda artifact, stream, maximum, timeout: stream.write(zip_bytes)
            with self.assertRaisesRegex(OperatorError, "producer differs"):
                downloader.run(SimpleNamespace(network=True, platform="linux"))
        self.assertFalse((self.root / "producer-linux-download.json").exists())

    def test_qualification_rejects_boolean_identity_and_wrong_version(self):
        original = deepcopy(self.qualification)
        for key, value in (("run_id", True), ("attempt", True), ("build", True), ("version", "2.7.18")):
            with self.subTest(key=key):
                changed = {**original, key: value}
                self.ctx.bindings["qualification"] = self.reference("qualification", changed)
                with self.assertRaisesRegex(OperatorError, "qualified producer"):
                    d.ProducerDownloader(self.ctx).qualification()

    def test_stage_output_existing_or_symlink_is_rejected_before_network(self):
        self.ctx.gh_get = Mock(side_effect=AssertionError("network forbidden"))
        args = self.args()
        args.output_dir.mkdir()
        with self.assertRaisesRegex(OperatorError, "output must be absent"):
            d.StageDownloader(self.ctx).run(args)
        args.output_dir.rmdir()
        args.output_dir.symlink_to(self.root / "missing")
        with self.assertRaisesRegex(OperatorError, "output must be absent"):
            d.StageDownloader(self.ctx).run(args)
        self.ctx.gh_get.assert_not_called()

    def test_stage_run_binds_receipt_and_exact_zip_without_live_io(self):
        args = self.args("signed")
        files, _ = self.stage_files("signed")
        zip_bytes = self.archive(files).read_bytes()
        args.zip_size, args.zip_sha256 = len(zip_bytes), d.sha(zip_bytes)
        responses, expected = self.remote(args)
        self.ctx.gh_get = Mock(side_effect=responses)
        downloader = d.StageDownloader(self.ctx)
        with patch.object(d, "BundleVerifier") as verifier, patch.object(downloader, "download") as transfer:
            verifier.return_value.setup.return_value = (self.ctx.bindings, p)
            transfer.side_effect = lambda artifact, stream, maximum, timeout: stream.write(zip_bytes)
            receipt = downloader.run(args)
        self.assertEqual(receipt["actual_zip_sha256"], d.sha(zip_bytes))
        self.assertEqual(receipt["actual_zip_size"], len(zip_bytes))
        self.assertEqual(receipt["files"], self.records(files))
        self.assertEqual(receipt["tooling_sha256"], self.ctx.tooling_sha256)
        self.assertEqual(receipt["freeze_sha256"], self.ctx.bindings["freeze"]["sha256"])
        self.assertEqual(receipt["producer_job_id"], 1001)
        self.assertEqual(json.loads((args.output_dir / "download.json").read_text()), receipt)


if __name__ == "__main__":
    unittest.main()
