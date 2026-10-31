"""Native ARM64 publication stays separate from shipped Windows x64 feeds."""

from copy import deepcopy
import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
from urllib.parse import urlsplit

import test_publisher as fixtures
import test_windows_publisher as windows_fixtures
from test_publisher import (
    FakeStore, PUBLIC_KEY, SOURCE_SHA, fixture_verifier, publisher,
    synthetic_envelope,
)


ARM_FEED = "https://updates.maple.ai/agent/stable/windows-arm64/latest.json"
ARM_POINTER = "stable/windows-arm64/latest.json"
ARM_SIDECARS = {
    "windows-arm64-build-info.json", "windows-arm64-manifest.json",
    "windows-arm64-SHA256SUMS",
}


class ArmFixtureOrigin(fixtures.FixtureOrigin):
    """Add the ARM64 public routes without changing the legacy origin fixture."""

    def open(self, request, timeout):
        self.assert_origin(request.full_url)
        path, method, byte_range = urlsplit(request.full_url).path, request.get_method(), request.get_header("Range")
        arm = publisher.validate_manifest((self.bundle / "windows-arm64-manifest.json").read_bytes())
        if method == "GET" and path in {
            "/agent/" + ARM_POINTER,
            publisher.public_path(arm["version"], "windows-arm64-manifest.json"),
        }:
            self.calls.append((method, path, byte_range))
            return fixtures.FakeResponse((self.bundle / "windows-arm64-envelope.json").read_bytes())
        if method == "GET" and path == "/download/agent/stable/windows-arm64":
            self.calls.append((method, path, byte_range))
            return fixtures.FakeResponse(b"", status=302, headers={"Location": arm["downloads"]["windows-arm64"]["path"]})
        record = next((item for item in [*arm["artifacts"].values(), *arm["downloads"].values()] if item["path"] == path), None)
        if record is not None:
            self.calls.append((method, path, byte_range))
            data = (self.bundle / "payloads" / path.rsplit("/", 1)[1]).read_bytes()
            if method == "HEAD":
                return fixtures.FakeResponse(b"", headers={"Content-Length": str(len(data))})
            if method == "GET" and byte_range == "bytes=0-0":
                return fixtures.FakeResponse(data[:1], status=206, headers={"Content-Range": f"bytes 0-0/{len(data)}"})
            if method == "GET" and byte_range is None:
                return fixtures.FakeResponse(data)
        return super().open(request, timeout)


class WindowsArm64PublicationTests(unittest.TestCase):
    def setUp(self):
        self.fixture = fixtures.PublicationTests()
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)

    def bundle(self, name="arm64", *, linux=True, windows=True):
        bundle, primary, envelope = self.fixture.make_bundle(
            "1.2.3", name, linux=linux, windows=windows,
        )
        records = {}
        for name, data in (
            ("agent-windows-arm64.tar.gz", b"ARM64 generation fixture"),
            ("agent-windows-arm64.exe", b"ARM64 setup fixture"),
        ):
            (bundle / "payloads" / name).write_bytes(data)
            records[name] = {
                "path": publisher.public_path(primary["version"], name),
                "sha256": hashlib.sha256(data).hexdigest(), "size": len(data),
            }
        manifest = {
            **primary, "format": "maple-agent-update-v4",
            "artifacts": {"windows-aarch64": records["agent-windows-arm64.tar.gz"]},
            "downloads": {"windows-arm64": records["agent-windows-arm64.exe"]},
        }
        (bundle / "windows-arm64-manifest.json").write_bytes(publisher.encode_json(manifest))
        (bundle / "windows-arm64-envelope.json").write_bytes(synthetic_envelope(manifest))
        for name in ARM_SIDECARS:
            (bundle / "metadata" / name).write_bytes(b"ARM64 audit " + name.encode())
        self.fixture.index_metadata(bundle)
        return bundle, primary, envelope

    def publish(self, bundle, store):
        return publisher.publish(
            bundle, store, bundle / "public-key.pub", fixture_verifier,
            previous_verifier=fixture_verifier,
        )

    def test_four_platform_documents_reserve_and_publish_independent_signed_bytes(self):
        bundle, _, mac_envelope = self.bundle()
        store = FakeStore()
        documents = publisher.bundle_manifests(bundle)
        self.assertEqual(
            [entry["manifest"]["format"] for entry in documents],
            ["maple-agent-update-v1", "maple-agent-update-v2", "maple-agent-update-v3", "maple-agent-update-v4"],
        )
        publisher.reserve(bundle, store)
        receipt = json.loads((bundle / "reservation.json").read_bytes())
        self.assertEqual(
            receipt["windows_arm64_manifest_sha256"],
            hashlib.sha256((bundle / "windows-arm64-manifest.json").read_bytes()).hexdigest(),
        )
        self.assertEqual(
            store.objects[publisher.release_key("1.2.3", "publication-windows-arm64-manifest.json")],
            (bundle / "windows-arm64-manifest.json").read_bytes(),
        )
        self.assertEqual(self.publish(bundle, store), "published")
        self.assertEqual(store.objects["stable/latest.json"], mac_envelope)
        self.assertEqual(store.objects["stable/windows/latest.json"], (bundle / "windows-envelope.json").read_bytes())
        self.assertEqual(store.objects[ARM_POINTER], (bundle / "windows-arm64-envelope.json").read_bytes())
        self.assertEqual(
            store.objects[publisher.release_key("1.2.3", "windows-arm64-manifest.json")],
            store.objects[ARM_POINTER],
        )
        for name in ARM_SIDECARS:
            self.assertIn(publisher.metadata_key("1.2.3", name), store.objects)
        self.assertEqual(self.publish(bundle, store), "already-published")

    def test_arm64_does_not_require_or_read_an_unselected_x64_feed(self):
        bundle, _, _ = self.bundle(linux=False, windows=False)
        store = FakeStore()
        publisher.reserve(bundle, store)
        self.publish(bundle, store)
        self.assertIn(ARM_POINTER, store.objects)
        self.assertFalse(any(call[1] in {"stable/windows/latest.json", "stable/linux/latest.json"} for call in store.calls))
        receipt = json.loads((bundle / "reservation.json").read_bytes())
        self.assertIn("windows_arm64_manifest_sha256", receipt)
        self.assertNotIn("windows_manifest_sha256", receipt)

    def test_arm64_target_and_download_are_rejected_by_every_legacy_schema(self):
        bundle, _, _ = self.bundle()
        arm = json.loads((bundle / "windows-arm64-manifest.json").read_bytes())
        publisher.validate_manifest(publisher.encode_json(arm))
        for version in (1, 2, 3):
            with self.subTest(version=version), self.assertRaises(publisher.PublishError):
                publisher.validate_manifest(publisher.encode_json({**arm, "format": f"maple-agent-update-v{version}"}))
        for change in (
            lambda wire: wire["artifacts"].update({"windows-x86_64": None}),
            lambda wire: wire["downloads"].update({"windows": None}),
            lambda wire: wire["artifacts"].update({"darwin-aarch64": next(iter(wire["artifacts"].values()))}),
            lambda wire: wire["downloads"]["windows-arm64"].update(path=wire["artifacts"]["windows-aarch64"]["path"]),
            lambda wire: wire["artifacts"]["windows-aarch64"].update(path=wire["downloads"]["windows-arm64"]["path"]),
        ):
            wire = deepcopy(arm)
            change(wire)
            with self.assertRaises(publisher.PublishError):
                publisher.validate_manifest(publisher.encode_json(wire))

    def test_arm64_audit_sidecars_must_be_complete_and_cannot_bleed_into_x64(self):
        for mode in ("missing", "partial", "orphan"):
            with self.subTest(mode=mode):
                bundle, _, _ = self.bundle(mode)
                if mode == "missing":
                    for name in ARM_SIDECARS:
                        (bundle / "metadata" / name).unlink()
                elif mode == "partial":
                    (bundle / "metadata/windows-arm64-build-info.json").unlink()
                else:
                    (bundle / "windows-arm64-manifest.json").unlink()
                    (bundle / "windows-arm64-envelope.json").unlink()
                    (bundle / "payloads/agent-windows-arm64.tar.gz").unlink()
                    (bundle / "payloads/agent-windows-arm64.exe").unlink()
                self.fixture.index_metadata(bundle)
                store = FakeStore()
                with self.assertRaisesRegex(publisher.PublishError, "exact sidecars"):
                    publisher.reserve(bundle, store)
                self.assertFalse(store.calls)

    def test_partial_arm64_promotion_repairs_only_arm64_and_keeps_x64_pointer(self):
        bundle, _, _ = self.bundle()
        store = FakeStore()
        publisher.reserve(bundle, store)
        store.fail_once = ARM_POINTER
        with self.assertRaises(publisher.StoreError):
            self.publish(bundle, store)
        x64 = store.objects["stable/windows/latest.json"]
        self.assertNotIn(ARM_POINTER, store.objects)
        store.calls.clear()
        self.assertEqual(self.publish(bundle, store), "published")
        self.assertEqual(store.objects["stable/windows/latest.json"], x64)
        self.assertEqual([call[1] for call in store.calls if call[0] == "put"], [ARM_POINTER])

    def test_arm64_pointer_race_preserves_competing_bytes(self):
        bundle, _, _ = self.bundle()
        store = FakeStore()
        publisher.reserve(bundle, store)
        original = store.put
        competitor = b"a concurrently published ARM64 pointer"

        def put(key, data, **kwargs):
            if key == ARM_POINTER:
                store.set(key, competitor)
            return original(key, data, **kwargs)

        with patch.object(store, "put", side_effect=put), self.assertRaisesRegex(publisher.PublishError, "changed concurrently"):
            self.publish(bundle, store)
        self.assertEqual(store.objects[ARM_POINTER], competitor)
        x64 = store.objects["stable/windows/latest.json"]
        store.calls.clear()
        with self.assertRaises(publisher.PublishError):
            self.publish(bundle, store)
        self.assertFalse(any(call[0] == "put" for call in store.calls))
        self.assertEqual(store.objects[ARM_POINTER], competitor)
        self.assertEqual(store.objects["stable/windows/latest.json"], x64)

    def test_live_verifier_checks_arm64_feed_immutable_both_payloads_and_alias(self):
        bundle, primary, envelope = self.bundle()
        origin = ArmFixtureOrigin(bundle, primary, envelope)
        with patch.object(publisher, "minisign_verify", fixture_verifier), \
                patch.object(publisher.urllib.request, "build_opener", return_value=origin):
            publisher.verify_live(bundle, bundle / "public-key.pub", "https://updates.example")
        paths = {path for _, path, _ in origin.calls}
        self.assertIn("/agent/" + ARM_POINTER, paths)
        self.assertIn("/agent/stable/releases/1.2.3/windows-arm64-manifest.json", paths)
        self.assertIn("/download/agent/stable/windows-arm64", paths)
        for name in ("agent-windows-arm64.tar.gz", "agent-windows-arm64.exe"):
            path = publisher.public_path("1.2.3", name)
            self.assertIn(("HEAD", path, None), origin.calls)
            self.assertIn(("GET", path, "bytes=0-0"), origin.calls)
            self.assertIn(("GET", path, None), origin.calls)


class WindowsArm64TrustTests(unittest.TestCase):
    def setUp(self):
        fixture = windows_fixtures.WindowsTrustTests()
        fixture.setUp()
        self.run, self.base, self.jobs = fixture.run, fixture.base, fixture.jobs
        self.arm = {"Build Windows ARM64 binary (prod)", "Signed Windows ARM64 (prod)", "Verify downloaded Windows ARM64 (prod)"}

    def test_arm64_concrete_prod_chain_is_required_independently_of_x64(self):
        publisher.validate_run(self.run, self.jobs(self.base | self.arm), windows_arm64=True)
        for missing in self.arm:
            with self.subTest(missing=missing), self.assertRaises(publisher.PublishError):
                publisher.validate_run(self.run, self.jobs((self.base | self.arm) - {missing}), windows_arm64=True)
        for conclusion in ("skipped", "failure", "cancelled", None):
            jobs = self.jobs(self.base | self.arm)
            next(job for job in jobs["jobs"] if job["name"] == "Signed Windows ARM64 (prod)")["conclusion"] = conclusion
            with self.subTest(conclusion=conclusion), self.assertRaises(publisher.PublishError):
                publisher.validate_run(self.run, jobs, windows_arm64=True)
        publisher.validate_run(self.run, self.jobs(self.base), windows_arm64=False)

    def test_trust_selects_exact_arm64_artifact_and_refuses_expired_duplicate_or_x64(self):
        names = ["maple-agent-prod-macos-aarch64-123", "maple-agent-prod-windows-aarch64-123"]
        records = [{"name": name, "expired": False} for name in names]
        artifacts = {"total_count": 2, "artifacts": records}
        with patch.object(publisher, "gh_json", side_effect=[self.run, self.jobs(self.base | self.arm), artifacts]):
            proof = publisher.trust_run(123, windows_arm64=True)
        self.assertEqual(proof["artifacts"]["windows-arm64"], names[1])
        for changed in (
            records[:1], [*records, records[1]],
            [records[0], {"name": names[1], "expired": True}],
            [records[0], {"name": names[1].replace("aarch64", "x86_64"), "expired": False}],
        ):
            with self.subTest(records=changed), patch.object(publisher, "gh_json", side_effect=[
                self.run, self.jobs(self.base | self.arm), {"total_count": len(changed), "artifacts": changed},
            ]), self.assertRaises(publisher.PublishError):
                publisher.trust_run(123, windows_arm64=True)

    def test_canonical_arm64_feed_must_be_pinned_by_source_and_publisher(self):
        profile = {"prod": {"update_feed_url": publisher.FEED_URL,
                            "windows_arm64_update_feed_url": ARM_FEED,
                            "update_public_keys": [PUBLIC_KEY]}}
        publisher.validate_pins(PUBLIC_KEY, profile, deepcopy(profile), windows_arm64=True)
        for value in (None, publisher.WINDOWS_FEED_URL, "https://elsewhere.example/arm64/latest.json"):
            bad = deepcopy(profile)
            bad["prod"]["windows_arm64_update_feed_url"] = value
            for local, source in ((bad, profile), (profile, bad)):
                with self.subTest(value=value), self.assertRaises(publisher.PublishError):
                    publisher.validate_pins(PUBLIC_KEY, local, source, windows_arm64=True)


class WindowsArm64PackageBoundaryTests(unittest.TestCase):
    def setUp(self):
        self.fixture = windows_fixtures.WindowsPackageBoundaryTests()
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        self.fixture.info["update_feed_url"] = ARM_FEED
        self.fixture.info_bytes = publisher.encode_json(self.fixture.info)
        stem = self.fixture.stem.replace("windows-x86_64", "windows-aarch64")
        for suffix in ("-setup.exe", ".tar.gz"):
            (self.fixture.directory / (self.fixture.stem + suffix)).rename(self.fixture.directory / (stem + suffix))
        self.fixture.stem = stem
        self.fixture.metadata.update(platform="windows-aarch64", package=stem + "-setup.exe", updater=stem + ".tar.gz")
        self.write()

    def write(self):
        fixture = self.fixture
        (fixture.directory / "build-info.json").write_bytes(fixture.info_bytes)
        (fixture.directory / "manifest.json").write_bytes(publisher.encode_json(fixture.metadata))
        fixture.write_archive()
        fixture.checksums()

    def validate(self):
        return publisher.validate_package(
            self.fixture.directory, "windows-aarch64", SOURCE_SHA, "1.2.3",
            expected_build="42", attestor=lambda path, source: self.fixture.attested.append((path.name, source)),
        )

    def test_arm64_package_binds_exact_native_target_and_all_five_attestations(self):
        result = self.validate()
        self.assertEqual([path.name for path, _ in result], [self.fixture.stem + "-setup.exe", self.fixture.stem + ".tar.gz"])
        self.assertEqual(len(self.fixture.attested), 5)
        self.assertEqual({name for name, _ in self.fixture.attested}, {path.name for path in self.fixture.directory.iterdir()})
        self.assertTrue(all(source == SOURCE_SHA for _, source in self.fixture.attested))

    def test_x64_package_identity_or_feed_cannot_qualify_as_arm64(self):
        original = deepcopy(self.fixture.metadata)
        for field, value in (("platform", "windows-x86_64"), ("windows_signing", "unsigned-pr"), ("build", "43")):
            self.fixture.metadata = {**original, field: value}
            self.write()
            with self.subTest(field=field), self.assertRaises(publisher.PublishError):
                self.validate()
            self.assertFalse(self.fixture.attested)
        self.fixture.metadata = original
        self.fixture.info["update_feed_url"] = publisher.WINDOWS_FEED_URL
        self.fixture.info_bytes = publisher.encode_json(self.fixture.info)
        self.write()
        with self.assertRaises(publisher.PublishError):
            self.validate()
        self.assertFalse(self.fixture.attested)

    def test_prepare_four_platforms_attests_every_input_and_keeps_architecture_sidecars_separate(self):
        mac = fixtures.PackageBoundaryTests()
        linux = fixtures.LinuxPackageBoundaryTests()
        windows = windows_fixtures.WindowsPackageBoundaryTests()
        for fixture in (mac, linux, windows):
            fixture.setUp()
            self.addCleanup(fixture.doCleanups)
        proof = {
            "run_id": 123, "run_attempt": 1, "source_sha": SOURCE_SHA, "build": "42",
            "pub_date": "2026-10-21T18:00:00Z",
            "artifacts": {"macos": "mac", "linux": "linux", "windows": "windows", "windows-arm64": "windows-arm64"},
        }
        attested = []
        validator = publisher.validate_package

        def validate(*args, **kwargs):
            return validator(*args, **kwargs, attestor=lambda path, source: attested.append((path, source)))

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            notes, key, output = root / "notes", root / "key.pub", root / "prepared"
            notes.write_text("Release fixture")
            key.write_text(PUBLIC_KEY + "\n")
            with patch.object(publisher, "trust_run", return_value=proof) as trust, \
                    patch.object(publisher, "check_source_pins", return_value=PUBLIC_KEY) as pins, \
                    patch.object(publisher, "validate_package", side_effect=validate):
                primary = publisher.prepare(
                    123, "1.2.3", "42", mac.directory, linux.directory, notes,
                    output, key, windows=windows.directory,
                    windows_arm64=self.fixture.directory,
                )
            trust.assert_called_once_with(123, True, windows=True, windows_arm64=True)
            pins.assert_called_once_with(key, SOURCE_SHA, True, windows=True, windows_arm64=True)
            self.assertEqual(len(attested), 21)
            self.assertTrue(all(source == SOURCE_SHA for _, source in attested))
            self.assertEqual(set(primary["artifacts"]), {"darwin-aarch64"})
            arm = publisher.validate_manifest((output / "windows-arm64-manifest.json").read_bytes())
            self.assertEqual(set(arm["artifacts"]), {"windows-aarch64"})
            self.assertEqual(set(arm["downloads"]), {"windows-arm64"})
            self.assertEqual(len(list((output / "payloads").iterdir())), 7)
            self.assertEqual(sum(path.is_file() for path in output.rglob("*")), 29)
            self.assertEqual(
                (output / "metadata/windows-arm64-build-info.json").read_bytes(),
                self.fixture.info_bytes,
            )
            self.assertNotEqual(
                (output / "metadata/windows-arm64-build-info.json").read_bytes(),
                (output / "metadata/windows-build-info.json").read_bytes(),
            )
            publisher.reserve(output, FakeStore())
            self.assertEqual(sum(path.is_file() for path in output.rglob("*")), 30)


if __name__ == "__main__":
    unittest.main()
