"""Architecture-bound offline downloads and dispatch; no live I/O or execution."""
from copy import deepcopy
import json
import stat
import subprocess
import unittest
from unittest.mock import Mock, patch

import publisher as p
import test_download_helpers as fixtures
from release_helpers import dispatch, downloads as d
from release_helpers.context import OperatorError


class ArmDownloadTests(unittest.TestCase):
    def setUp(self):
        self.fixture = fixtures.DownloadTests()
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        self.ctx = self.fixture.ctx
        self.platforms = ["macos", "linux", "windows", "windows-arm64"]
        self.ctx.bindings["platforms"] = self.platforms[:]
        self.ctx.bindings["artifacts"]["windows-arm64"] = {
            "id": 14, "name": "maple-agent-prod-windows-aarch64-501", "size": 1, "zip_sha256": "f" * 64}
        self.fixture.qualification["scope"]["include_windows_arm64"] = True
        self.ctx.bindings["qualification"] = self.fixture.reference("qualification", self.fixture.qualification)
        self.fixture.selection["platforms"] = self.platforms[:]
        self.fixture.selection["inputs"]["include_windows_arm64"] = True
        self.ctx.bindings["selected_publication"] = self.fixture.reference("selected", self.fixture.selection)
        self.files = self.fixture.prepared_files()
        self.files["windows-arm64-manifest.json"] = b"{}\n"
        self.files.update({"metadata/" + name: b"arm metadata\n" for name in p.WINDOWS_ARM64_METADATA_NAMES})
        self.fixture.files = self.files
        self.fixture.freeze["platforms"] = self.platforms[:]
        self.fixture.freeze["producer"]["artifacts"] = {
            key: item["name"] for key, item in self.ctx.bindings["artifacts"].items()}
        self.rebind()

    def rebind(self):
        self.fixture.freeze["files"] = self.ctx.bindings["prepared_files"] = self.fixture.records(self.files)
        self.ctx.bindings["freeze"] = self.fixture.reference("freeze", self.fixture.freeze)

    def frozen(self):
        with patch.object(d, "BundleVerifier") as verifier:
            verifier.return_value.setup.return_value = self.ctx.bindings, p
            return d.StageDownloader(self.ctx).frozen_inputs()

    def test_four_platform_freeze_preserves_separate_payload_identity(self):
        _, files = self.frozen()
        self.assertEqual(len(files), 29)
        payloads = [name for name in files if name.startswith("payloads/")]
        self.assertEqual(len(payloads), 7)
        self.assertEqual(len([name for name in payloads if "windows-aarch64" in name]), 2)
        self.assertEqual(len([name for name in payloads if "windows-x86_64" in name]), 2)

    def test_selected_publication_and_freeze_cannot_drop_arm(self):
        for target in (self.fixture.selection, self.fixture.freeze):
            original = deepcopy(target)
            target["platforms"] = self.platforms[:-1]
            key = "selected_publication" if target is self.fixture.selection else "freeze"
            self.ctx.bindings[key] = self.fixture.reference(key, target)
            with self.assertRaisesRegex(OperatorError, "binding differs|producer differs"):
                self.frozen()
            target.clear()
            target.update(original)
            self.ctx.bindings[key] = self.fixture.reference(key, target)

    def test_arm_publication_inputs_are_exact_and_include_notes(self):
        for field, value in (("include_windows_arm64", None), ("include_windows_arm64", 1), ("include_linux", 1),
                             ("notes", "changed"), ("extra", "unbound")):
            changed = deepcopy(self.fixture.selection)
            changed["inputs"][field] = value
            self.ctx.bindings["selected_publication"] = self.fixture.reference("selected", changed)
            with self.assertRaisesRegex(OperatorError, "selected publication/producer"):
                self.frozen()

    def test_arm_qualification_requires_actual_arm_success_scope(self):
        self.assertEqual(d.ProducerDownloader(self.ctx).qualification()["scope"]["include_windows_arm64"], True)
        for flag in (False, None, "true", 1):
            changed = deepcopy(self.fixture.qualification)
            changed["scope"]["include_windows_arm64"] = flag
            self.ctx.bindings["qualification"] = self.fixture.reference("qualification", changed)
            with self.assertRaisesRegex(OperatorError, "qualified producer"):
                d.ProducerDownloader(self.ctx).qualification()

    def test_arm_producer_zip_rejects_x64_payload_name(self):
        files = {name: b"metadata\n" for name in ("manifest.json", "build-info.json", "SHA256SUMS")}
        for suffix in d.SUFFIXES["windows-arm64"]:
            files[f"maple-agent-prod-{self.ctx.version}-aaaaaaa-windows-aarch64{suffix}"] = b"payload\n"
        archive = self.fixture.archive(files, "arm-producer.zip")
        actual = d.ProducerDownloader(self.ctx).extract(archive, self.ctx.root / "arm-producer", "windows-arm64")
        self.assertEqual(actual, self.fixture.records(files))
        wrong = {name.replace("windows-aarch64", "windows-x86_64"): data for name, data in files.items()}
        archive = self.fixture.archive(wrong, "x64-producer.zip")
        with self.assertRaisesRegex(OperatorError, "target differs"):
            d.ProducerDownloader(self.ctx).extract(archive, self.ctx.root / "wrong-producer", "windows-arm64")

    def test_four_platform_stage_zip_inventory_and_arm_signature_are_exact(self):
        for stage, count in (("prepared", 29), ("reserved", 30), ("signed", 34)):
            with self.subTest(stage=stage):
                files, _ = self.fixture.stage_files(stage)
                if stage == "signed":
                    files["windows-arm64-envelope.json"] = b"arm signed\n"
                archive = self.fixture.archive(files, stage + ".zip", directories=True)
                expected = d.StageDownloader.zip_inventory(archive, stage, self.ctx.bindings["prepared_files"], self.platforms)
                self.assertEqual(len(expected), count)
                if stage == "signed":
                    files.pop("windows-arm64-envelope.json")
                    missing = self.fixture.archive(files, "missing-arm-signature.zip")
                    with self.assertRaisesRegex(OperatorError, "ZIP leaf inventory"):
                        d.StageDownloader.zip_inventory(missing, stage, self.ctx.bindings["prepared_files"], self.platforms)

    def test_legacy_three_platform_stage_rejects_unselected_arm(self):
        archive = self.fixture.archive(self.files, "unselected.zip")
        with self.assertRaisesRegex(OperatorError, "ZIP leaf inventory"):
            d.StageDownloader.zip_inventory(archive, "prepared", self.ctx.bindings["prepared_files"])

    def test_signed_stage_download_binds_arm_reservation_and_receipt(self):
        args = self.fixture.args("signed")
        files, raw = self.fixture.stage_files("signed")
        reservation = json.loads(raw)
        reservation["windows_arm64_manifest_sha256"] = self.ctx.bindings["prepared_files"]["windows-arm64-manifest.json"]["sha256"]
        files["reservation.json"] = p.encode_json(reservation)
        files["windows-arm64-envelope.json"] = b"arm signed\n"
        zip_bytes = self.fixture.archive(files, "arm-stage.zip").read_bytes()
        args.zip_size, args.zip_sha256 = len(zip_bytes), d.sha(zip_bytes)
        responses, _ = self.fixture.remote(args)
        self.ctx.gh_get = Mock(side_effect=responses)
        downloader = d.StageDownloader(self.ctx)
        with patch.object(d, "BundleVerifier") as verifier, patch.object(downloader, "download") as transfer:
            verifier.return_value.setup.return_value = self.ctx.bindings, p
            transfer.side_effect = lambda artifact, output, maximum, timeout: output.write(zip_bytes)
            receipt = downloader.run(args)
        self.assertEqual(receipt["platforms"], self.platforms)
        self.assertEqual(len(receipt["files"]), 34)
        self.assertEqual(receipt["files"], self.fixture.records(files))
        self.assertEqual(receipt["actual_zip_sha256"], d.sha(zip_bytes))
        self.assertEqual(json.loads((args.output_dir / "bundle/reservation.json").read_text()), reservation)

    def test_large_four_platform_stage_transfer_can_take_more_than_fifteen_minutes(self):
        args = self.fixture.args()
        zip_bytes = self.fixture.archive(self.files, "large-stage.zip").read_bytes()
        args.zip_size, args.zip_sha256 = 248_926_519, d.sha(zip_bytes)
        responses, _ = self.fixture.remote(args)
        self.ctx.gh_get = Mock(side_effect=responses)

        def transfer(command, **options):
            if 1000 > options["timeout"]:
                raise subprocess.TimeoutExpired(command, options["timeout"])
            options["stdout"].write(zip_bytes)
            return subprocess.CompletedProcess(command, 0)

        # Simulate the large transport's byte count without a 249 MB fixture;
        # real ZIP inventory, extraction and frozen-file hashes still run.
        with patch.object(d, "BundleVerifier") as verifier, \
             patch.object(d.subprocess, "run", side_effect=transfer) as request, \
             patch.object(d, "cap", wraps=d.cap) as byte_bound, \
             patch.object(d, "read_record", return_value=(
                 {"size": args.zip_size, "sha256": args.zip_sha256}, b"")) as record:
            verifier.return_value.setup.return_value = self.ctx.bindings, p
            receipt = d.StageDownloader(self.ctx).run(args)
        request.assert_called_once()
        self.assertEqual(request.call_args.kwargs["timeout"], 2020)
        byte_bound.assert_called_once_with(args.zip_size + 1)
        record.assert_called_once_with(args.output_dir / "artifact.zip", d.MAX_ZIP)
        self.assertEqual(receipt["actual_zip_size"], args.zip_size)
        self.assertEqual(receipt["actual_zip_sha256"], args.zip_sha256)
        self.assertEqual(receipt["files"], self.fixture.records(self.files))
        self.assertEqual(len(receipt["files"]), 29)

    def test_large_stage_timeout_retains_partial_without_retry_or_success_receipt(self):
        args = self.fixture.args()
        args.zip_size = 248_926_519
        responses, _ = self.fixture.remote(args)
        self.ctx.gh_get = Mock(side_effect=responses)
        partial = b"incomplete four-platform stage"
        downloader = d.StageDownloader(self.ctx)

        def transfer(command, **options):
            options["stdout"].write(partial)
            raise subprocess.TimeoutExpired(command, options["timeout"])

        with patch.object(d, "BundleVerifier") as verifier, \
             patch.object(d.subprocess, "run", side_effect=transfer) as request, \
             patch.object(downloader, "extract") as extract, \
             patch.object(d, "read_record") as record:
            verifier.return_value.setup.return_value = self.ctx.bindings, p
            with self.assertRaisesRegex(OperatorError, "2020-second transfer timeout; partial archive retained"):
                downloader.run(args)
            archive = args.output_dir / "artifact.zip"
            self.assertEqual(archive.read_bytes(), partial)
            self.assertEqual(stat.S_IMODE(archive.stat().st_mode), 0o600)
            self.assertFalse((args.output_dir / "bundle").exists())
            self.assertFalse((args.output_dir / "download.json").exists())
            self.ctx.gh_get = Mock(side_effect=AssertionError("second request forbidden"))
            with self.assertRaisesRegex(OperatorError, "output must be absent"):
                downloader.run(args)
            self.ctx.gh_get.assert_not_called()
            request.assert_called_once()
            self.assertEqual(request.call_args.kwargs["timeout"], 2020)
            extract.assert_not_called()
            record.assert_not_called()

    def test_new_legacy_dispatch_explicitly_disables_default_arm_input(self):
        fixture = fixtures.DownloadTests()
        fixture.setUp()
        self.addCleanup(fixture.doCleanups)
        fixture.ctx.bindings["prepared_bundle"] = str(fixture.ctx.root / "bundle")
        with patch.object(dispatch, "BundleVerifier"):
            _, body = dispatch.Dispatcher(fixture.ctx).inputs()
        self.assertIs(body["inputs"]["include_windows_arm64"], False)

    def test_dispatch_selects_arm_explicitly_and_refuses_false_qualification(self):
        self.ctx.bindings["prepared_bundle"] = str(self.ctx.root / "bundle")
        with patch.object(dispatch, "BundleVerifier"):
            _, body = dispatch.Dispatcher(self.ctx).inputs()
        self.assertIs(body["inputs"]["include_windows_arm64"], True)
        self.assertEqual(body["inputs"]["build_run_id"], "501")
        self.assertEqual(body["inputs"]["version"], self.ctx.version)
        wrong = deepcopy(self.fixture.qualification)
        wrong["scope"]["include_windows_arm64"] = False
        self.ctx.bindings["qualification"] = self.fixture.reference("qualification", wrong)
        with patch.object(dispatch, "BundleVerifier"), self.assertRaisesRegex(OperatorError, "qualified producer"):
            dispatch.Dispatcher(self.ctx).inputs()


if __name__ == "__main__":
    unittest.main()
