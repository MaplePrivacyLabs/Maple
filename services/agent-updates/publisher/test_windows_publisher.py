"""Windows-specific provenance, immutable publication and archive boundaries."""

from copy import deepcopy
import gzip
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import test_publisher as fixtures
from test_publisher import (
    FakeStore, FixtureOrigin,
    PUBLIC_KEY, SOURCE_SHA, fixture_verifier, publisher, synthetic_envelope,
)


class WindowsPublicationTests(unittest.TestCase):
    def setUp(self):
        self.fixture = fixtures.PublicationTests()
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)

    def bundle(self, name="windows", linux=True):
        return self.fixture.make_bundle("1.2.3", name, linux=linux, windows=True)

    def publish(self, bundle, store):
        return publisher.publish(bundle, store, bundle / "public-key.pub",
                                 fixture_verifier, previous_verifier=fixture_verifier)

    def test_windows_has_its_own_exact_reserved_signed_document_and_pointer(self):
        bundle, mac, mac_envelope = self.bundle()
        store = FakeStore()
        publications = publisher.bundle_manifests(bundle)
        self.assertEqual([x["manifest"]["format"] for x in publications],
                         ["maple-agent-update-v1", "maple-agent-update-v2", "maple-agent-update-v3"])
        publisher.reserve(bundle, store)
        receipt = json.loads((bundle / "reservation.json").read_bytes())
        self.assertEqual(receipt["windows_manifest_sha256"], hashlib.sha256((bundle / "windows-manifest.json").read_bytes()).hexdigest())
        self.assertEqual(receipt["linux_manifest_sha256"], hashlib.sha256((bundle / "linux-manifest.json").read_bytes()).hexdigest())
        self.assertEqual(store.objects[publisher.release_key("1.2.3", "publication-windows-manifest.json")],
                         (bundle / "windows-manifest.json").read_bytes())
        self.assertEqual(self.publish(bundle, store), "published")
        self.assertEqual(store.objects["stable/latest.json"], mac_envelope)
        self.assertEqual(store.objects["stable/windows/latest.json"], (bundle / "windows-envelope.json").read_bytes())
        self.assertEqual(store.objects[publisher.release_key("1.2.3", "windows-manifest.json")],
                         (bundle / "windows-envelope.json").read_bytes())
        self.assertEqual(set(mac["artifacts"]), {"darwin-aarch64"})
        for name in publisher.WINDOWS_METADATA_NAMES:
            self.assertIn(publisher.metadata_key("1.2.3", name), store.objects)
        self.assertEqual(self.publish(bundle, store), "already-published")

    def test_mac_and_windows_without_linux_do_not_misclassify_the_second_manifest(self):
        bundle, _, _ = self.bundle(linux=False)
        store = FakeStore()
        publisher.reserve(bundle, store)
        receipt = json.loads((bundle / "reservation.json").read_bytes())
        self.assertIn("windows_manifest_sha256", receipt)
        self.assertNotIn("linux_manifest_sha256", receipt)
        self.publish(bundle, store)
        self.assertIn("stable/windows/latest.json", store.objects)
        self.assertFalse(any(call[1] == "stable/linux/latest.json" for call in store.calls))

    def test_windows_metadata_is_exact_and_cannot_be_omitted_or_attached_to_other_formats(self):
        for mode in ("missing", "partial", "extra"):
            with self.subTest(mode=mode):
                bundle, _, _ = self.bundle(mode)
                directory = bundle / "metadata"
                if mode == "missing":
                    for name in publisher.WINDOWS_METADATA_NAMES:
                        (directory / name).unlink()
                elif mode == "partial":
                    (directory / "windows-manifest.json").unlink()
                else:
                    (bundle / "windows-manifest.json").unlink()
                    (bundle / "windows-envelope.json").unlink()
                    (bundle / "payloads/agent-windows.tar.gz").unlink()
                    (bundle / "payloads/agent-windows.exe").unlink()
                self.fixture.index_metadata(bundle)
                store = FakeStore()
                with self.assertRaisesRegex(publisher.PublishError, "exact sidecars"):
                    publisher.reserve(bundle, store)
                self.assertFalse(store.calls)

    def test_windows_identity_mismatch_and_unsupported_fields_fail_before_reservation(self):
        for field, value in (("build", "43"), ("source_sha", "b" * 40),
                             ("notes", "other notes"), ("format", "maple-agent-update-v2")):
            with self.subTest(field=field):
                bundle, _, _ = self.bundle(field)
                wire = json.loads((bundle / "windows-manifest.json").read_bytes())
                wire[field] = value
                (bundle / "windows-manifest.json").write_bytes(publisher.encode_json(wire))
                with self.assertRaises(publisher.PublishError):
                    publisher.reserve(bundle, FakeStore())
        bundle, _, _ = self.bundle("schema")
        wire = json.loads((bundle / "windows-manifest.json").read_bytes())
        for change in (lambda x: x["artifacts"].update({"darwin-aarch64": next(iter(x["artifacts"].values()))}),
                       lambda x: x["downloads"].update({"macos": next(iter(x["downloads"].values()))}),
                       lambda x: x["artifacts"]["windows-x86_64"].update(path=x["downloads"]["windows"]["path"]),
                       lambda x: x["downloads"]["windows"].update(path=x["artifacts"]["windows-x86_64"]["path"])):
            bad = deepcopy(wire)
            change(bad)
            with self.assertRaises(publisher.PublishError):
                publisher.validate_manifest(publisher.encode_json(bad))

    def test_windows_partial_promotion_retry_only_repairs_the_missing_pointer(self):
        bundle, _, _ = self.bundle()
        store = FakeStore()
        publisher.reserve(bundle, store)
        store.fail_once = "stable/windows/latest.json"
        with self.assertRaises(publisher.StoreError):
            self.publish(bundle, store)
        self.assertIn("stable/latest.json", store.objects)
        self.assertIn("stable/linux/latest.json", store.objects)
        self.assertNotIn("stable/windows/latest.json", store.objects)
        store.calls.clear()
        self.assertEqual(self.publish(bundle, store), "published")
        writes = [call[1] for call in store.calls if call[0] == "put"]
        self.assertEqual(writes, ["stable/windows/latest.json"])

    def test_windows_pointer_race_preserves_competing_bytes_and_refuses_retry(self):
        bundle, _, _ = self.bundle()
        competitor = b"a competing release pointer"
        store = FakeStore()
        publisher.reserve(bundle, store)
        original_put = store.put

        def put(key, data, **kwargs):
            if key == "stable/windows/latest.json":
                store.set(key, competitor)
            return original_put(key, data, **kwargs)

        with patch.object(store, "put", side_effect=put), \
                self.assertRaisesRegex(publisher.PublishError, "changed concurrently"):
            self.publish(bundle, store)
        self.assertEqual(store.objects["stable/windows/latest.json"], competitor)
        store.calls.clear()
        with self.assertRaises(publisher.PublishError):
            self.publish(bundle, store)
        self.assertFalse(any(call[0] == "put" for call in store.calls))
        self.assertEqual(store.objects["stable/windows/latest.json"], competitor)

    def test_windows_conflicting_reservation_and_changed_signed_bytes_fail_closed(self):
        bundle, _, _ = self.bundle()
        store = FakeStore()
        store.set(publisher.release_key("1.2.3", "publication-windows-manifest.json"), b"different")
        with self.assertRaises(publisher.PublishError):
            publisher.reserve(bundle, store)
        self.assertNotIn("stable/windows/latest.json", store.objects)
        store = FakeStore()
        publisher.reserve(bundle, store)
        wire = json.loads((bundle / "windows-manifest.json").read_bytes())
        wire["notes"] = "changed"
        (bundle / "windows-envelope.json").write_bytes(synthetic_envelope(wire))
        with self.assertRaises(publisher.PublishError):
            self.publish(bundle, store)
        self.assertNotIn("stable/windows/latest.json", store.objects)

    def test_live_verifier_covers_windows_feed_immutable_both_payloads_and_alias(self):
        bundle, mac, envelope = self.bundle()
        origin = FixtureOrigin(bundle, mac, envelope)
        with patch.object(publisher, "minisign_verify", fixture_verifier), \
                patch.object(publisher.urllib.request, "build_opener", return_value=origin):
            publisher.verify_live(bundle, bundle / "public-key.pub", "https://updates.example")
        paths = {path for _, path, _ in origin.calls}
        for path in ("/agent/stable/windows/latest.json", "/agent/stable/releases/1.2.3/windows-manifest.json",
                     "/download/agent/stable/windows", "/agent/stable/releases/1.2.3/agent-windows.tar.gz",
                     "/agent/stable/releases/1.2.3/agent-windows.exe"):
            self.assertIn(path, paths)
        for suffix in ("agent-windows.tar.gz", "agent-windows.exe"):
            path = "/agent/stable/releases/1.2.3/" + suffix
            self.assertIn(("HEAD", path, None), origin.calls)
            self.assertIn(("GET", path, "bytes=0-0"), origin.calls)
            self.assertIn(("GET", path, None), origin.calls)


class WindowsTrustTests(unittest.TestCase):
    def setUp(self):
        self.run = {"id": 123, "run_attempt": 1, "run_number": 42,
                    "repository": {"full_name": publisher.REPOSITORY},
                    "head_repository": {"full_name": publisher.REPOSITORY},
                    "path": publisher.WORKFLOW, "head_branch": "master", "event": "push",
                    "status": "completed", "head_sha": SOURCE_SHA, "created_at": "2026-10-21T18:00:00Z"}
        self.base = {"Signed macOS (prod)", "Verify downloaded macOS (prod)"}
        self.windows = {"Build Windows binary (prod)", "Signed Windows (prod)", "Verify downloaded Windows (prod)"}

    def jobs(self, names):
        jobs = [{"name": name, "status": "completed", "conclusion": "success"} for name in names]
        return {"total_count": len(jobs), "jobs": jobs}

    def test_each_windows_prod_job_is_required_and_aggregate_or_skip_cannot_substitute(self):
        publisher.validate_run(self.run, self.jobs(self.base | self.windows), windows=True)
        for missing in self.windows:
            with self.subTest(missing=missing), self.assertRaises(publisher.PublishError):
                publisher.validate_run(self.run, self.jobs((self.base | self.windows) - {missing}), windows=True)
        for conclusion in ("skipped", "failure", "cancelled", None):
            jobs = self.jobs(self.base | self.windows)
            next(x for x in jobs["jobs"] if x["name"] == "Signed Windows (prod)")["conclusion"] = conclusion
            with self.subTest(conclusion=conclusion), self.assertRaises(publisher.PublishError):
                publisher.validate_run(self.run, jobs, windows=True)
        publisher.validate_run(self.run, self.jobs(self.base), windows=False)

    def test_trust_selects_exact_windows_artifact_and_rejects_missing_expired_duplicate(self):
        names = ["maple-agent-prod-macos-aarch64-123", "maple-agent-prod-windows-x86_64-123"]
        artifacts = {"total_count": 2, "artifacts": [{"name": name, "expired": False} for name in names]}
        with patch.object(publisher, "gh_json", side_effect=[self.run, self.jobs(self.base | self.windows), artifacts]):
            proof = publisher.trust_run(123, windows=True)
        self.assertEqual(proof["artifacts"]["windows"], names[1])
        for entries in (artifacts["artifacts"][:1], [*artifacts["artifacts"], artifacts["artifacts"][1]],
                        [artifacts["artifacts"][0], {"name": names[1], "expired": True}]):
            bad = {"total_count": len(entries), "artifacts": entries}
            with patch.object(publisher, "gh_json", side_effect=[self.run, self.jobs(self.base | self.windows), bad]), \
                    self.assertRaises(publisher.PublishError):
                publisher.trust_run(123, windows=True)

    def test_windows_source_and_publisher_need_same_canonical_feed_and_pinned_key(self):
        profile = {"prod": {"update_feed_url": publisher.FEED_URL, "windows_update_feed_url": publisher.WINDOWS_FEED_URL,
                            "update_public_keys": [PUBLIC_KEY]}}
        publisher.validate_pins(PUBLIC_KEY, profile, deepcopy(profile), windows=True)
        for bad in (None, "https://foreign.example/windows/latest.json", publisher.FEED_URL):
            changed = deepcopy(profile)
            changed["prod"]["windows_update_feed_url"] = bad
            for first, second in ((changed, profile), (profile, changed)):
                with self.subTest(bad=bad), self.assertRaises(publisher.PublishError):
                    publisher.validate_pins(PUBLIC_KEY, first, second, windows=True)


class WindowsPackageBoundaryTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        spec = importlib.util.spec_from_file_location("windows_fixture_info", publisher.ROOT / "apps/maple-agent/scripts/release-info.py")
        release_info = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(release_info)
        profiles = json.loads((publisher.ROOT / "apps/maple-agent/release-profiles.json").read_text())
        self.info = {field: profiles["prod"][field] for field in release_info.PUBLIC_FIELDS}
        self.info.update(profile="prod", version="1.2.3", source_sha=SOURCE_SHA,
                         git_revision=SOURCE_SHA[:9], update_feed_url=publisher.WINDOWS_FEED_URL)
        self.info_bytes = publisher.encode_json(self.info)
        self.stem = "maple-agent-prod-1.2.3-" + SOURCE_SHA[:9] + "-windows-x86_64"
        self.metadata = {"schema_version": 1, "profile": "prod", "platform": "windows-x86_64",
                         "source_sha": SOURCE_SHA, "version": "1.2.3", "package": self.stem + "-setup.exe",
                         "distribution": "master", "macos_signing": None, "windows_signing": "authenticode",
                         "updater": self.stem + ".tar.gz", "build": "42"}
        self.descriptor = {"format": "maple-agent-windows-bundle-v1", "product": "maple-agent",
                           "profile": "prod", "version": "1.2.3", "build": "42", "source_sha": SOURCE_SHA,
                           "bundle_id": self.info["bundle_id"], "entrypoint": "maple-agent.exe"}
        (self.directory / "build-info.json").write_bytes(self.info_bytes)
        (self.directory / "manifest.json").write_bytes(publisher.encode_json(self.metadata))
        (self.directory / (self.stem + "-setup.exe")).write_bytes(b"offline signed installer fixture; never execute")
        self.write_archive()
        self.checksums()
        self.attested = []

    def entries(self):
        root = "maple-agent-windows"
        items = [(name, tarfile.DIRTYPE, b"") for name in (root, root + "/notices", root + "/notices/wordlists")]
        files = {"bundle-info.json": publisher.encode_json(self.descriptor), "build-info.json": self.info_bytes,
                 "maple-agent.exe": b"offline signed PE fixture; never execute", "LICENSE.txt": b"license",
                 "notices/THIRD_PARTY_NOTICES.txt": b"notices", "notices/THIRD_PARTY_NOTICES.html": b"html",
                 "notices/manifest.json": b"{}", "notices/wordlists/eff-large-wordlist.txt": b"word",
                 "notices/wordlists/SOURCES.md": b"source"}
        return items + [(root + "/" + name, tarfile.REGTYPE, data) for name, data in files.items()]

    def write_archive(self, entries=None):
        with tarfile.open(self.directory / (self.stem + ".tar.gz"), "w:gz", format=tarfile.USTAR_FORMAT) as archive:
            for name, kind, data in self.entries() if entries is None else entries:
                member = tarfile.TarInfo(name)
                member.type, member.mode = kind, 0o755 if kind == tarfile.DIRTYPE else 0o644
                if kind == tarfile.REGTYPE:
                    member.size = len(data)
                    archive.addfile(member, io.BytesIO(data))
                else:
                    member.linkname = "outside" if kind == tarfile.SYMTYPE else ""
                    archive.addfile(member)

    def checksums(self):
        (self.directory / "SHA256SUMS").write_text("".join(
            hashlib.sha256(path.read_bytes()).hexdigest() + "  " + path.name + "\n"
            for path in sorted(self.directory.iterdir()) if path.name != "SHA256SUMS"
        ))

    def validate(self):
        return publisher.validate_package(self.directory, "windows-x86_64", SOURCE_SHA, "1.2.3",
                                          expected_build="42", attestor=lambda path, source: self.attested.append((path.name, source)))

    def test_exact_installer_and_bundle_are_bound_to_source_build_inventory_and_all_attestations(self):
        result = self.validate()
        self.assertEqual([path.name for path, _ in result], [self.stem + "-setup.exe", self.stem + ".tar.gz"])
        self.assertEqual({name for name, _ in self.attested}, {p.name for p in self.directory.iterdir()})
        self.assertEqual(len(self.attested), 5)

    def test_archive_identity_mismatch_missing_extra_duplicate_links_and_sidecar_drift_fail_before_attestation(self):
        original = deepcopy(self.descriptor)
        for field, value in (("build", "43"), ("source_sha", "b" * 40), ("profile", "dev"), ("extra", True)):
            with self.subTest(field=field):
                self.descriptor = {**original, field: value}
                self.write_archive()
                self.checksums()
                with self.assertRaises(publisher.PublishError):
                    self.validate()
                self.assertFalse(self.attested)
        self.descriptor = original
        entries = self.entries()
        for changed in (entries[:-1], entries + [entries[-1]], entries + [("maple-agent-windows/extra", tarfile.REGTYPE, b"extra")],
                        entries + [("maple-agent-windows/uninstall.exe", tarfile.REGTYPE, b"forbidden generation uninstaller")],
                        [(name, tarfile.SYMTYPE if name.endswith("/maple-agent.exe") else kind, data) for name, kind, data in entries],
                        [(name, kind, b"other identity" if name.endswith("/build-info.json") else data) for name, kind, data in entries]):
            with self.subTest(entries=len(changed)):
                self.write_archive(changed)
                self.checksums()
                with self.assertRaises(publisher.PublishError):
                    self.validate()
                self.assertFalse(self.attested)

    def test_unsigned_wrong_manifest_build_and_extra_final_file_cannot_qualify(self):
        for field, value in (("windows_signing", "unsigned-pr"), ("distribution", "unsigned-pr"), ("build", "43")):
            with self.subTest(field=field):
                changed = {**self.metadata, field: value}
                (self.directory / "manifest.json").write_bytes(publisher.encode_json(changed))
                self.checksums()
                with self.assertRaises(publisher.PublishError):
                    self.validate()
                self.assertFalse(self.attested)
        (self.directory / "manifest.json").write_bytes(publisher.encode_json(self.metadata))
        (self.directory / "extra.exe").write_bytes(b"undeclared payload")
        with self.assertRaises(publisher.PublishError):
            self.validate()
        self.assertFalse(self.attested)

    def test_trailing_content_invalid_end_marker_excessive_padding_and_truncated_gzip_fail(self):
        archive = self.directory / (self.stem + ".tar.gz")
        canonical = gzip.decompress(archive.read_bytes())
        # Find the actual tar end, skipping member bodies rather than searching
        # for zero bytes that may legitimately occur inside a PE file.
        offset = 0
        while canonical[offset:offset + 512] != bytes(512):
            member = tarfile.TarInfo.frombuf(canonical[offset:offset + 512], "utf-8", "strict")
            offset += 512 + (member.size + 511) // 512 * 512
        variants = [gzip.compress(canonical + b"undeclared content"),
                    gzip.compress(canonical[:offset + 512] + b"bad second end marker".ljust(512, b"\0")),
                    gzip.compress(canonical + bytes(10241)),
                    gzip.compress(canonical)[:-4]]
        for corrupt in variants:
            with self.subTest(size=len(corrupt)):
                archive.write_bytes(corrupt)
                self.checksums()
                with self.assertRaises(publisher.PublishError):
                    self.validate()
                self.assertFalse(self.attested)

    def test_preparation_keeps_mac_bytes_independent_and_binds_windows_sidecars(self):
        mac = fixtures.PackageBoundaryTests()
        mac.setUp()
        self.addCleanup(mac.doCleanups)
        notes = self.directory.parent / (self.directory.name + "-notes")
        notes.write_text("release notes")
        self.addCleanup(notes.unlink)
        key = self.directory.parent / (self.directory.name + "-key")
        key.write_text(PUBLIC_KEY + "\n")
        self.addCleanup(key.unlink)
        output = self.directory.parent / (self.directory.name + "-prepared")
        self.addCleanup(lambda: __import__("shutil").rmtree(output, ignore_errors=True))
        proof = {"run_id": 123, "run_attempt": 1, "source_sha": SOURCE_SHA, "build": "42",
                 "pub_date": "2026-10-21T18:00:00Z", "artifacts": {"macos": "mac", "windows": "windows"}}
        validator = publisher.validate_package
        def offline_validation(*args, **kwargs):
            return validator(*args, **kwargs, attestor=lambda path, source: None)
        with patch.object(publisher, "trust_run", return_value=proof) as trust, \
                patch.object(publisher, "check_source_pins", return_value=PUBLIC_KEY) as pins, \
                patch.object(publisher, "validate_package", side_effect=offline_validation):
            prepared = publisher.prepare(123, "1.2.3", "42", mac.directory, None, notes, output, key, windows=self.directory)
        trust.assert_called_once_with(123, False, windows=True)
        pins.assert_called_once_with(key, SOURCE_SHA, False, windows=True)
        self.assertEqual(set(prepared["artifacts"]), {"darwin-aarch64"})
        windows = publisher.validate_manifest((output / "windows-manifest.json").read_bytes())
        self.assertEqual(windows["artifacts"]["windows-x86_64"]["path"].rsplit("/", 1)[1], self.stem + ".tar.gz")
        self.assertEqual(windows["downloads"]["windows"]["path"].rsplit("/", 1)[1], self.stem + "-setup.exe")
        self.assertEqual(len(list((output / "payloads").iterdir())), 4)
        self.assertEqual(sum(path.is_file() for path in output.rglob("*")), 18)
        publisher.reserve(output, FakeStore())
        self.assertEqual(sum(path.is_file() for path in output.rglob("*")), 19)


if __name__ == "__main__":
    unittest.main()
