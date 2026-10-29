"""Offline publication recovery and untrusted artifact boundary tests.

The fake store models conditional writes, interrupted uploads and concurrent
pointer changes. Its synthetic signatures exercise envelope plumbing only;
cryptographic verification remains an explicitly injected boundary here.
"""

import base64
from email.message import Message
import gzip
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import plistlib
import tarfile
import tempfile
import unittest
from unittest.mock import patch
from urllib.parse import urlsplit
from urllib.response import addinfourl


SPEC = importlib.util.spec_from_file_location("agent_publisher", Path(__file__).with_name("publisher.py"))
publisher = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(publisher)
SOURCE_SHA = "a" * 40
PUBLIC_KEY = base64.b64encode(b"Ed" + bytes(40)).decode()
MACOS_METADATA = (
    "macos-SHA256SUMS", "macos-build-info.json", "macos-dmg-notarization-log.json",
    "macos-manifest.json", "macos-notarization-log.json", "provenance.json",
)
LINUX_METADATA = ("linux-SHA256SUMS", "linux-build-info.json", "linux-manifest.json")
WINDOWS_METADATA = ("windows-SHA256SUMS", "windows-build-info.json", "windows-manifest.json")


def synthetic_envelope(manifest):
    signature = "\n".join((
        "untrusted comment: offline fixture, not a real signature",
        base64.b64encode(b"ED" + bytes(72)).decode(),
        "trusted comment: " + publisher.trusted_comment(manifest["version"]),
        base64.b64encode(bytes(64)).decode(),
        "",
    ))
    return publisher.encode_json({
        "manifest": publisher.encode_json(manifest).decode(),
        "signature": signature,
    })


def fixture_verifier(data, public_key):
    """A structural verifier: intentionally no cryptographic claims."""
    if publisher.public_key_text(public_key) != PUBLIC_KEY:
        raise publisher.PublishError("unexpected verification key")
    return publisher.parse_envelope(data)[1]


class FakeStore:
    def __init__(self):
        self.objects = {}
        self.revisions = {}
        self.calls = []
        self.fail_once = None
        self.corrupt_once = None
        self.before_pointer = None
        self.before_linux_pointer = None

    def set(self, key, data):
        self.objects[key] = data
        self.revisions[key] = self.revisions.get(key, 0) + 1

    def etag(self, key):
        return '"' + str(self.revisions[key]) + '"'

    def get(self, key, maximum=publisher.MAX_JSON):
        self.calls.append(("get", key))
        if key not in self.objects:
            return None
        publisher.need(len(self.objects[key]) <= maximum, "oversized fake object")
        return self.objects[key], self.etag(key)

    def same(self, key, digest, size):
        self.calls.append(("same", key))
        if key not in self.objects:
            return False
        data = self.objects[key]
        return len(data) == size and hashlib.sha256(data).hexdigest() == digest

    def put(self, key, data, *, condition, content_type="application/octet-stream"):
        self.calls.append(("put", key, condition, content_type))
        if key == "stable/latest.json" and self.before_pointer:
            callback, self.before_pointer = self.before_pointer, None
            callback()
        if key == "stable/linux/latest.json" and self.before_linux_pointer:
            callback, self.before_linux_pointer = self.before_linux_pointer, None
            callback()
        if key == self.fail_once:
            self.fail_once = None
            raise publisher.StoreError(503)
        header, value = condition
        if header == "If-None-Match":
            if value != "*" or key in self.objects:
                raise publisher.StoreError(412)
        elif header == "If-Match":
            if key not in self.objects or value != self.etag(key):
                raise publisher.StoreError(412)
        else:
            raise AssertionError("unconditional write")
        value = data.read_bytes() if isinstance(data, Path) else data
        if key == self.corrupt_once:
            self.corrupt_once = None
            value = b"acknowledged but corrupted object"
        self.set(key, value)


class FakeResponse(io.BytesIO):
    def __init__(self, data, status=200, headers=None):
        super().__init__(data)
        self.status = status
        self.headers = headers or {}


class FixtureOrigin:
    """The exact public route contract, with no network or executable input."""

    def __init__(self, bundle, manifest, envelope):
        self.bundle, self.manifest, self.envelope = bundle, manifest, envelope
        self.calls = []
        self.redirect = None
        self.range_header = None

    def open(self, request, timeout):
        self.assert_origin(request.full_url)
        path = urlsplit(request.full_url).path
        method = request.get_method()
        range_header = request.get_header("Range")
        self.calls.append((method, path, range_header))
        immutable = publisher.public_path(self.manifest["version"], "manifest.json")
        if path in {"/agent/stable/latest.json", immutable} and method == "GET":
            return FakeResponse(self.envelope)
        records = [*self.manifest["artifacts"].values(), *self.manifest["downloads"].values()]
        linux_path = self.bundle / "linux-manifest.json"
        if linux_path.exists():
            linux = publisher.validate_manifest(linux_path.read_bytes())
            if path in {"/agent/stable/linux/latest.json", publisher.public_path(linux["version"], "linux-manifest.json")} and method == "GET":
                return FakeResponse((self.bundle / "linux-envelope.json").read_bytes())
            if path == "/download/agent/stable/linux" and method == "GET":
                return FakeResponse(b"", status=302, headers={"Location": linux["downloads"]["linux-bundle"]["path"]})
            records += list(linux["artifacts"].values())
        windows_path = self.bundle / "windows-manifest.json"
        if windows_path.exists():
            windows = publisher.validate_manifest(windows_path.read_bytes())
            if path in {"/agent/stable/windows/latest.json", publisher.public_path(windows["version"], "windows-manifest.json")} and method == "GET":
                return FakeResponse((self.bundle / "windows-envelope.json").read_bytes())
            if path == "/download/agent/stable/windows" and method == "GET":
                return FakeResponse(b"", status=302, headers={"Location": windows["downloads"]["windows"]["path"]})
            records += [*windows["artifacts"].values(), *windows["downloads"].values()]
        record = next((item for item in records if item["path"] == path), None)
        if record:
            data = (self.bundle / "payloads" / path.rsplit("/", 1)[1]).read_bytes()
            if method == "HEAD":
                return FakeResponse(b"", headers={"Content-Length": str(len(data))})
            if method == "GET" and range_header == "bytes=0-0":
                return FakeResponse(data[:1], status=206, headers={
                    "Content-Range": self.range_header or f"bytes 0-0/{len(data)}",
                })
            if method == "GET" and range_header is None:
                return FakeResponse(data)
        if path == "/download/agent/stable/macos" and method == "GET":
            return FakeResponse(b"", status=302, headers={
                "Location": self.redirect or self.manifest["downloads"]["macos"]["path"],
            })
        return FakeResponse(b"404\n", status=404)

    def assert_origin(self, url):
        if not url.startswith("https://updates.example/"):
            raise AssertionError("unexpected live verification origin")


class PublicationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.store = FakeStore()
        self.bundle, self.manifest, self.envelope = self.make_bundle("1.2.3", "first")

    def make_bundle(self, version, name, notes="Release fixture", linux=False, windows=False):
        bundle = self.root / name
        payloads = bundle / "payloads"
        payloads.mkdir(parents=True)
        artifacts = {}
        binaries = [("agent.app.tar.gz", b"archive fixture"), ("agent.dmg", b"disk image fixture")]
        if linux:
            binaries.append(("agent-linux.tar.gz", b"Linux tar fixture"))
        if windows:
            binaries += [("agent-windows.tar.gz", b"Windows tar fixture"), ("agent-windows.exe", b"Windows installer fixture")]
        for filename, data in binaries:
            (payloads / filename).write_bytes(data)
            artifacts[filename] = {
                "path": publisher.public_path(version, filename),
                "size": len(data), "sha256": hashlib.sha256(data).hexdigest(),
            }
        manifest = {
            "format": "maple-agent-update-v1", "product": "maple-agent",
            "channel": "stable", "version": version, "build": "42",
            "source_sha": SOURCE_SHA, "pub_date": "2026-10-21T18:00:00Z",
            "notes": notes, "artifacts": {"darwin-aarch64": artifacts["agent.app.tar.gz"]},
            "downloads": {"macos": artifacts["agent.dmg"]},
        }
        if linux:
            linux_manifest = {**manifest, "format": "maple-agent-update-v2",
                              "artifacts": {"linux-x86_64": artifacts["agent-linux.tar.gz"]},
                              "downloads": {"linux-bundle": artifacts["agent-linux.tar.gz"]}}
            (bundle / "linux-manifest.json").write_bytes(publisher.encode_json(linux_manifest))
            (bundle / "linux-envelope.json").write_bytes(synthetic_envelope(linux_manifest))
        if windows:
            windows_manifest = {**manifest, "format": "maple-agent-update-v3",
                                "artifacts": {"windows-x86_64": artifacts["agent-windows.tar.gz"]},
                                "downloads": {"windows": artifacts["agent-windows.exe"]}}
            (bundle / "windows-manifest.json").write_bytes(publisher.encode_json(windows_manifest))
            (bundle / "windows-envelope.json").write_bytes(synthetic_envelope(windows_manifest))
        envelope = synthetic_envelope(manifest)
        (bundle / "manifest.json").write_bytes(publisher.encode_json(manifest))
        (bundle / "envelope.json").write_bytes(envelope)
        (bundle / "public-key.pub").write_text(PUBLIC_KEY + "\n")
        metadata = bundle / "metadata"
        metadata.mkdir()
        for filename in (*MACOS_METADATA, *(LINUX_METADATA if linux else ()), *(WINDOWS_METADATA if windows else ())):
            data = b"offline checksum fixture\n" if filename.endswith("SHA256SUMS") else \
                publisher.encode_json({"fixture": filename, "version": version})
            (metadata / filename).write_bytes(data)
        self.index_metadata(bundle)
        return bundle, manifest, envelope

    def reserve(self):
        return publisher.reserve(self.bundle, self.store)

    def publish(self, bundle=None):
        selected = bundle or self.bundle
        return publisher.publish(selected, self.store, selected / "public-key.pub",
                                 fixture_verifier, previous_verifier=fixture_verifier)

    def release_key(self, filename):
        return publisher.release_key("1.2.3", filename)

    def metadata_key(self, filename="macos-build-info.json"):
        return self.release_key("metadata") + "/" + filename

    def add_metadata(self, bundle=None, data=b'{"profile":"prod","version":"1.2.3"}\n'):
        bundle = bundle or self.bundle
        directory = bundle / "metadata"
        directory.mkdir(exist_ok=True)
        (directory / "macos-build-info.json").write_bytes(data)
        return self.index_metadata(bundle)

    def index_metadata(self, bundle):
        inventory = {path.name: {"sha256": hashlib.sha256(path.read_bytes()).hexdigest(), "size": path.stat().st_size}
                     for path in sorted((bundle / "metadata").iterdir())}
        (bundle / "metadata-inventory.json").write_bytes(publisher.encode_json(inventory))
        return inventory

    def test_reservation_precedes_artifacts_and_complete_manifest_precedes_pointer(self):
        self.reserve()
        self.assertEqual(self.publish(), "published")
        writes = [call[1] for call in self.store.calls if call[0] == "put"]
        self.assertEqual(writes, [
            self.release_key("publication-manifest.json"),
            self.release_key("publication-metadata.json"),
            self.release_key("agent.app.tar.gz"), self.release_key("agent.dmg"),
            *[self.metadata_key(name) for name in MACOS_METADATA],
            self.release_key("manifest.json"), "stable/latest.json",
        ])
        self.assertEqual(self.store.objects["stable/latest.json"], self.envelope)
        self.assertTrue(all(call[2] == ("If-None-Match", "*") for call in self.store.calls if call[0] == "put"))

    def test_each_payload_and_complete_manifest_are_read_back_before_promotion(self):
        self.reserve()
        self.publish()
        complete = next(index for index, call in enumerate(self.store.calls)
                        if call[:2] == ("put", self.release_key("manifest.json")))
        pointer = next(index for index, call in enumerate(self.store.calls)
                       if call[:2] == ("put", "stable/latest.json"))
        for name in ("agent.app.tar.gz", "agent.dmg"):
            verification = next(index for index, call in enumerate(self.store.calls)
                                if call == ("same", self.release_key(name)))
            self.assertLess(verification, complete)
        marker_readback = [index for index, call in enumerate(self.store.calls)
                           if call == ("get", self.release_key("manifest.json")) and index > complete]
        self.assertTrue(marker_readback)
        self.assertLess(marker_readback[0], pointer)

    def test_acknowledged_corrupt_payload_cannot_be_marked_complete_or_promoted(self):
        self.reserve()
        self.store.corrupt_once = self.release_key("agent.app.tar.gz")
        with self.assertRaises(publisher.PublishError):
            self.publish()
        self.assertNotIn(self.release_key("manifest.json"), self.store.objects)
        self.assertNotIn("stable/latest.json", self.store.objects)

    def test_acknowledged_corrupt_complete_manifest_cannot_be_promoted(self):
        self.reserve()
        self.store.corrupt_once = self.release_key("manifest.json")
        with self.assertRaises(publisher.PublishError):
            self.publish()
        self.assertNotIn("stable/latest.json", self.store.objects)

    def test_acknowledged_corrupt_reservation_cannot_receive_signing_receipt(self):
        self.store.corrupt_once = self.release_key("publication-manifest.json")
        with self.assertRaises(publisher.PublishError):
            self.reserve()
        self.assertFalse((self.bundle / "reservation.json").exists())

    def test_private_metadata_inventory_is_reserved_and_files_verified_before_complete(self):
        inventory = self.add_metadata()
        self.reserve()
        self.assertEqual(self.publish(), "published")
        index = publisher.encode_json(inventory)
        self.assertEqual(self.store.objects[self.release_key("publication-metadata.json")], index)
        self.assertEqual(self.store.objects[self.metadata_key()], (self.bundle / "metadata/macos-build-info.json").read_bytes())
        receipt = json.loads((self.bundle / "reservation.json").read_text())
        self.assertEqual(receipt["metadata_inventory_sha256"], hashlib.sha256(index).hexdigest())
        complete = next(position for position, call in enumerate(self.store.calls)
                        if call[:2] == ("put", self.release_key("manifest.json")))
        for name in MACOS_METADATA:
            verification = next(position for position, call in enumerate(self.store.calls)
                                if call == ("same", self.metadata_key(name)))
            self.assertLess(verification, complete)
        # Audit sidecars remain outside the signed public artifact inventory.
        self.assertFalse(any("/metadata/" in record["path"] for record in
                             [*self.manifest["artifacts"].values(), *self.manifest["downloads"].values()]))

    def test_empty_metadata_inventory_cannot_reserve_a_production_release(self):
        for path in (self.bundle / "metadata").iterdir():
            path.unlink()
        (self.bundle / "metadata-inventory.json").write_bytes(publisher.encode_json({}))
        with self.assertRaisesRegex(publisher.PublishError, "exact sidecars"):
            self.reserve()
        self.assertFalse(self.store.objects)

    def test_missing_or_subset_metadata_inventory_cannot_bypass_required_audit_data(self):
        inventory = json.loads((self.bundle / "metadata-inventory.json").read_text())
        (self.bundle / "metadata-inventory.json").unlink()
        with self.assertRaisesRegex(publisher.PublishError, "metadata has no inventory"):
            self.reserve()
        (self.bundle / "metadata").rename(self.bundle / "saved-metadata")
        with self.assertRaisesRegex(publisher.PublishError, "metadata has no inventory"):
            self.reserve()
        (self.bundle / "saved-metadata").rename(self.bundle / "metadata")
        for name in MACOS_METADATA:
            with self.subTest(missing=name):
                subset = {key: record for key, record in inventory.items() if key != name}
                (self.bundle / "metadata-inventory.json").write_bytes(publisher.encode_json(subset))
                with self.assertRaisesRegex(publisher.PublishError, "exact sidecars"):
                    self.reserve()
                self.assertFalse(self.store.objects)

    def test_metadata_inventory_exactly_tracks_linux_presence_in_signed_manifest(self):
        for variant in ("missing-linux", "partial-linux", "unexpected-linux"):
            with self.subTest(variant=variant):
                bundle, _, _ = self.make_bundle("1.2.3", variant, linux=variant != "unexpected-linux")
                if variant == "unexpected-linux":
                    for name in LINUX_METADATA:
                        (bundle / "metadata" / name).write_text("offline Linux metadata\n")
                else:
                    for name in (LINUX_METADATA if variant == "missing-linux" else ("linux-manifest.json",)):
                        (bundle / "metadata" / name).unlink()
                self.index_metadata(bundle)
                with self.assertRaisesRegex(publisher.PublishError, "exact sidecars"):
                    publisher.reserve(bundle, self.store)
                self.assertFalse(self.store.objects)

    def test_linux_bundle_includes_and_publishes_all_required_private_sidecars(self):
        bundle, _, _ = self.make_bundle("1.2.3", "complete-linux", linux=True)
        publisher.reserve(bundle, self.store)
        self.assertEqual(self.publish(bundle), "published")
        for name in (*MACOS_METADATA, *LINUX_METADATA):
            self.assertEqual(self.store.objects[self.metadata_key(name)], (bundle / "metadata" / name).read_bytes())

    def test_linux_v2_has_separate_reserved_bytes_envelope_and_pointer(self):
        bundle, mac, mac_envelope = self.make_bundle("1.2.3", "dual-feed", linux=True)
        linux_data = (bundle / "linux-manifest.json").read_bytes()
        linux = publisher.validate_manifest(linux_data)
        self.assertEqual(set(mac["artifacts"]), {"darwin-aarch64"})
        self.assertEqual(set(mac["downloads"]), {"macos"})
        self.assertEqual(linux["format"], "maple-agent-update-v2")
        self.assertEqual(linux["artifacts"]["linux-x86_64"], linux["downloads"]["linux-bundle"])
        self.assertTrue(linux["downloads"]["linux-bundle"]["path"].endswith(".tar.gz"))
        publisher.reserve(bundle, self.store)
        receipt = json.loads((bundle / "reservation.json").read_bytes())
        self.assertEqual(receipt["linux_manifest_sha256"], hashlib.sha256(linux_data).hexdigest())
        self.assertEqual(self.store.objects[self.release_key("publication-linux-manifest.json")], linux_data)
        self.assertEqual(self.publish(bundle), "published")
        self.assertEqual(self.store.objects["stable/latest.json"], mac_envelope)
        self.assertEqual(self.store.objects["stable/linux/latest.json"], (bundle / "linux-envelope.json").read_bytes())
        self.assertEqual(self.store.objects[self.release_key("linux-manifest.json")], (bundle / "linux-envelope.json").read_bytes())
        self.assertNotEqual(self.store.objects["stable/latest.json"], self.store.objects["stable/linux/latest.json"])
        self.store.calls.clear()
        self.assertEqual(self.publish(bundle), "already-published")
        self.assertFalse(any(call[0] == "put" for call in self.store.calls))

    def test_mac_only_publication_never_reads_or_writes_linux_pointer(self):
        self.store.set("stable/linux/latest.json", b"unrelated Linux pointer")
        self.reserve()
        self.assertEqual(self.publish(), "published")
        self.assertEqual(self.store.objects["stable/linux/latest.json"], b"unrelated Linux pointer")
        self.assertFalse(any(call[1] == "stable/linux/latest.json" for call in self.store.calls))

    def test_linux_v2_rejects_mixed_targets_wrong_suffix_and_different_download(self):
        bundle, mac, _ = self.make_bundle("1.2.3", "linux-schema", linux=True)
        valid = json.loads((bundle / "linux-manifest.json").read_bytes())
        for variant in ("mac-target", "mac-download", "appimage", "different-download", "unknown-field"):
            with self.subTest(variant=variant):
                value = json.loads(json.dumps(valid))
                if variant == "mac-target":
                    value["artifacts"]["darwin-aarch64"] = mac["artifacts"]["darwin-aarch64"]
                elif variant == "mac-download":
                    value["downloads"]["macos"] = mac["downloads"]["macos"]
                elif variant == "appimage":
                    value["artifacts"]["linux-x86_64"]["path"] += ".AppImage"
                    value["downloads"]["linux-bundle"]["path"] += ".AppImage"
                elif variant == "different-download":
                    value["downloads"]["linux-bundle"]["sha256"] = "b" * 64
                else:
                    value["unknown"] = True
                with self.assertRaises(publisher.PublishError):
                    publisher.validate_manifest(publisher.encode_json(value))

    def test_dual_feed_identity_and_payloads_fail_before_any_reservation(self):
        for variant in ("source_sha", "version", "build", "notes", "pub_date", "missing-payload"):
            with self.subTest(variant=variant):
                bundle, _, _ = self.make_bundle("1.2.3", "identity-" + variant, linux=True)
                if variant == "missing-payload":
                    (bundle / "payloads/agent-linux.tar.gz").unlink()
                else:
                    value = json.loads((bundle / "linux-manifest.json").read_bytes())
                    value[variant] = {"source_sha": "b" * 40, "version": "1.2.4", "build": "43",
                                      "notes": "other notes", "pub_date": "2026-10-22T18:00:00Z"}[variant]
                    (bundle / "linux-manifest.json").write_bytes(publisher.encode_json(value))
                store = FakeStore()
                with self.assertRaises(publisher.PublishError):
                    publisher.reserve(bundle, store)
                self.assertFalse(store.calls)

    def test_linux_reservation_conflict_cannot_authorize_signing(self):
        bundle, _, _ = self.make_bundle("1.2.3", "linux-conflict", linux=True)
        self.store.set(self.release_key("publication-linux-manifest.json"), b"different bytes")
        with self.assertRaisesRegex(publisher.PublishError, "immutable object differs"):
            publisher.reserve(bundle, self.store)
        self.assertFalse((bundle / "reservation.json").exists())
        self.assertNotIn("stable/latest.json", self.store.objects)
        self.assertNotIn("stable/linux/latest.json", self.store.objects)

    def test_linux_partial_promotion_retry_repairs_only_unpromoted_pointer(self):
        bundle, _, mac_envelope = self.make_bundle("1.2.3", "linux-interruption", linux=True)
        publisher.reserve(bundle, self.store)
        self.store.fail_once = "stable/linux/latest.json"
        with self.assertRaises(publisher.StoreError):
            self.publish(bundle)
        self.assertEqual(self.store.objects["stable/latest.json"], mac_envelope)
        self.assertNotIn("stable/linux/latest.json", self.store.objects)
        self.store.calls.clear()
        self.assertEqual(self.publish(bundle), "published")
        writes = [call[1] for call in self.store.calls if call[0] == "put"]
        self.assertEqual(writes, ["stable/linux/latest.json"])

    def test_linux_pointer_race_preserves_competitor_and_retry_cannot_roll_it_back(self):
        bundle, _, _ = self.make_bundle("1.2.3", "linux-race", linux=True)
        competitor, _, _ = self.make_bundle("1.2.4", "linux-competitor", linux=True)
        competing_envelope = (competitor / "linux-envelope.json").read_bytes()
        publisher.reserve(bundle, self.store)
        self.store.before_linux_pointer = lambda: self.store.set("stable/linux/latest.json", competing_envelope)
        with self.assertRaisesRegex(publisher.PublishError, "changed concurrently"):
            self.publish(bundle)
        self.assertEqual(self.store.objects["stable/linux/latest.json"], competing_envelope)
        self.store.calls.clear()
        with self.assertRaisesRegex(publisher.PublishError, "must advance"):
            self.publish(bundle)
        self.assertFalse(any(call[0] == "put" for call in self.store.calls))

    def test_dual_retry_repairs_missing_linux_objects_without_replacing_pointers(self):
        bundle, _, _ = self.make_bundle("1.2.3", "linux-repair", linux=True)
        publisher.reserve(bundle, self.store)
        self.publish(bundle)
        linux = json.loads((bundle / "linux-manifest.json").read_bytes())
        missing = linux["artifacts"]["linux-x86_64"]["path"][len("/agent/"):]
        del self.store.objects[missing]
        del self.store.objects[self.release_key("linux-manifest.json")]
        before = {key: self.store.get(key) for key in ("stable/latest.json", "stable/linux/latest.json")}
        self.store.calls.clear()
        self.assertEqual(self.publish(bundle), "already-published")
        self.assertEqual({call[1] for call in self.store.calls if call[0] == "put"},
                         {missing, self.release_key("linux-manifest.json")})
        self.assertEqual(before, {key: self.store.get(key) for key in before})

    def test_linux_reservation_recovers_existing_envelope_exactly(self):
        bundle, _, _ = self.make_bundle("1.2.3", "linux-existing", linux=True)
        publisher.reserve(bundle, self.store)
        self.publish(bundle)
        envelope = (bundle / "linux-envelope.json").read_bytes()
        (bundle / "linux-envelope.json").unlink()
        publisher.reserve(bundle, self.store)
        self.assertEqual((bundle / "linux-envelope.json").read_bytes(), envelope)

    def test_metadata_missing_corrupt_unchecked_and_symlink_files_fail_before_reservation(self):
        for variant in ("missing", "corrupt", "unchecked", "symlink"):
            with self.subTest(variant=variant):
                bundle, _, _ = self.make_bundle("1.2.3", "metadata-" + variant)
                self.add_metadata(bundle)
                metadata = bundle / "metadata/macos-build-info.json"
                if variant == "missing":
                    metadata.unlink()
                elif variant == "corrupt":
                    metadata.write_bytes(b"different bytes")
                elif variant == "unchecked":
                    (bundle / "metadata/linux-build-info.json").write_text("{}")
                else:
                    metadata.unlink()
                    metadata.symlink_to(bundle / "manifest.json")
                with self.assertRaises((publisher.PublishError, OSError)):
                    publisher.reserve(bundle, self.store)
                self.assertFalse(self.store.objects)

    def test_private_metadata_directory_requires_index_and_cannot_be_a_symlink(self):
        self.add_metadata()
        index = (self.bundle / "metadata-inventory.json").read_bytes()
        (self.bundle / "metadata-inventory.json").unlink()
        with self.assertRaisesRegex(publisher.PublishError, "metadata has no inventory"):
            self.reserve()
        (self.bundle / "metadata-inventory.json").write_bytes(index)
        directory = self.bundle / "metadata"
        directory.rename(self.bundle / "other-metadata")
        directory.symlink_to(self.bundle / "other-metadata", target_is_directory=True)
        with self.assertRaises(publisher.PublishError):
            self.reserve()
        self.assertFalse(self.store.objects)

    def test_acknowledged_corrupt_metadata_index_cannot_receive_signing_receipt(self):
        self.add_metadata()
        self.store.corrupt_once = self.release_key("publication-metadata.json")
        with self.assertRaisesRegex(publisher.PublishError, "reserved metadata read-back differs"):
            self.reserve()
        self.assertFalse((self.bundle / "reservation.json").exists())
        self.assertNotIn(self.release_key("manifest.json"), self.store.objects)
        self.assertNotIn("stable/latest.json", self.store.objects)

    def test_publication_requires_reserved_metadata_index_before_writing_payloads(self):
        self.add_metadata()
        self.reserve()
        del self.store.objects[self.release_key("publication-metadata.json")]
        self.store.calls.clear()
        with self.assertRaisesRegex(publisher.PublishError, "metadata must be reserved"):
            self.publish()
        self.assertFalse(any(call[0] == "put" for call in self.store.calls))

    def test_unallowlisted_metadata_or_invalid_hash_index_is_rejected_before_reservation(self):
        valid = self.add_metadata()
        invalid = (
            {**valid, "unexpected.json": valid["macos-build-info.json"]},
            {**valid, "../macos-build-info.json": valid["macos-build-info.json"]},
            {**valid, "macos-build-info.json": {**valid["macos-build-info.json"], "sha256": "not-a-digest"}},
            {**valid, "macos-build-info.json": {**valid["macos-build-info.json"], "size": True}},
            {**valid, "macos-build-info.json": {**valid["macos-build-info.json"], "extra": "unexpected"}},
        )
        for inventory in invalid:
            with self.subTest(inventory=inventory):
                (self.bundle / "metadata-inventory.json").write_bytes(publisher.encode_json(inventory))
                with self.assertRaises(publisher.PublishError):
                    self.reserve()
                self.assertFalse(self.store.objects)

    def test_metadata_index_cannot_change_after_reservation_even_with_same_signed_manifest(self):
        self.add_metadata()
        self.reserve()
        self.add_metadata(data=b'{"profile":"prod","version":"1.2.3","changed":true}\n')
        self.store.calls.clear()
        with self.assertRaises(publisher.PublishError):
            self.publish()
        self.assertFalse(any(call[0] == "put" for call in self.store.calls))
        self.assertNotIn("stable/latest.json", self.store.objects)

    def test_metadata_upload_failure_leaves_no_complete_release_and_resumes_identically(self):
        self.add_metadata()
        self.reserve()
        self.store.fail_once = self.metadata_key()
        with self.assertRaises(publisher.StoreError):
            self.publish()
        self.assertNotIn(self.release_key("manifest.json"), self.store.objects)
        self.assertNotIn("stable/latest.json", self.store.objects)
        self.assertEqual(self.publish(), "published")
        self.assertEqual(self.store.revisions[self.release_key("agent.app.tar.gz")], 1)
        self.assertEqual(self.store.objects[self.metadata_key()], (self.bundle / "metadata/macos-build-info.json").read_bytes())

    def test_acknowledged_corrupt_private_metadata_cannot_be_marked_complete(self):
        self.add_metadata()
        self.reserve()
        self.store.corrupt_once = self.metadata_key()
        with self.assertRaises(publisher.PublishError):
            self.publish()
        self.assertNotIn(self.release_key("manifest.json"), self.store.objects)
        self.assertNotIn("stable/latest.json", self.store.objects)

    def test_published_retry_repairs_missing_private_metadata_without_pointer_rewrite(self):
        self.add_metadata()
        self.reserve()
        self.publish()
        del self.store.objects[self.metadata_key()]
        self.store.calls.clear()
        self.assertEqual(self.publish(), "already-published")
        self.assertEqual(self.store.objects[self.metadata_key()], (self.bundle / "metadata/macos-build-info.json").read_bytes())
        self.assertEqual(self.store.revisions["stable/latest.json"], 1)
        self.assertFalse(any(call[:2] == ("put", "stable/latest.json") for call in self.store.calls))

    def test_published_retry_rejects_existing_corrupt_private_metadata(self):
        self.add_metadata()
        self.reserve()
        self.publish()
        self.store.set(self.metadata_key(), b"corrupt audit metadata")
        self.store.calls.clear()
        with self.assertRaisesRegex(publisher.PublishError, "immutable object differs"):
            self.publish()
        self.assertEqual(self.store.objects[self.metadata_key()], b"corrupt audit metadata")
        self.assertEqual(self.store.revisions["stable/latest.json"], 1)
        self.assertFalse(any(call[:2] == ("put", "stable/latest.json") for call in self.store.calls))

    def test_retry_resumes_partial_upload_without_discoverable_incomplete_release(self):
        self.reserve()
        self.store.fail_once = self.release_key("agent.dmg")
        with self.assertRaises(publisher.StoreError):
            self.publish()
        first = self.store.objects[self.release_key("agent.app.tar.gz")]
        self.assertNotIn(self.release_key("manifest.json"), self.store.objects)
        self.assertNotIn("stable/latest.json", self.store.objects)
        self.assertEqual(self.publish(), "published")
        self.assertEqual(self.store.objects[self.release_key("agent.app.tar.gz")], first)
        self.assertEqual(self.store.revisions[self.release_key("agent.app.tar.gz")], 1)
        self.assertIn(("same", self.release_key("agent.app.tar.gz")), self.store.calls)

    def test_retry_after_complete_manifest_before_promotion_reuses_exact_envelope(self):
        self.reserve()
        self.store.fail_once = "stable/latest.json"
        with self.assertRaises(publisher.StoreError):
            self.publish()
        self.assertEqual(self.store.objects[self.release_key("manifest.json")], self.envelope)
        self.assertNotIn("stable/latest.json", self.store.objects)
        self.assertEqual(self.publish(), "published")
        self.assertEqual(self.store.revisions[self.release_key("manifest.json")], 1)

    def test_existing_immutable_different_bytes_fail_before_pointer(self):
        self.reserve()
        self.store.set(self.release_key("agent.app.tar.gz"), b"different archive")
        with self.assertRaisesRegex(publisher.PublishError, "immutable object differs"):
            self.publish()
        self.assertNotIn("stable/latest.json", self.store.objects)
        self.assertEqual(self.store.objects[self.release_key("agent.app.tar.gz")], b"different archive")

    def test_existing_complete_manifest_different_bytes_are_not_overwritten(self):
        self.reserve()
        self.store.set(self.release_key("manifest.json"), b"other envelope")
        with self.assertRaisesRegex(publisher.PublishError, "immutable object differs"):
            self.publish()
        self.assertNotIn("stable/latest.json", self.store.objects)

    def test_pointer_race_preserves_competing_promotion(self):
        _, old_manifest, old_envelope = self.make_bundle("1.2.2", "old")
        _, newer_manifest, newer_envelope = self.make_bundle("1.2.4", "newer")
        self.assertLess(publisher.version_tuple(old_manifest["version"]), publisher.version_tuple(newer_manifest["version"]))
        self.store.set("stable/latest.json", old_envelope)
        self.reserve()
        self.store.before_pointer = lambda: self.store.set("stable/latest.json", newer_envelope)
        with self.assertRaisesRegex(publisher.PublishError, "pointer changed concurrently"):
            self.publish()
        self.assertEqual(self.store.objects["stable/latest.json"], newer_envelope)
        pointer_write = [call for call in self.store.calls if call[:2] == ("put", "stable/latest.json")][-1]
        self.assertEqual(pointer_write[2], ("If-Match", '"1"'))

    def test_creation_race_preserves_operator_pointer(self):
        _, _, newer_envelope = self.make_bundle("1.2.4", "newer")
        self.reserve()
        self.store.before_pointer = lambda: self.store.set("stable/latest.json", newer_envelope)
        with self.assertRaisesRegex(publisher.PublishError, "pointer changed concurrently"):
            self.publish()
        self.assertEqual(self.store.objects["stable/latest.json"], newer_envelope)

    def test_identical_published_release_is_verified_no_op(self):
        self.reserve()
        self.publish()
        revisions = self.store.revisions.copy()
        self.store.calls.clear()
        self.assertEqual(self.publish(), "already-published")
        self.assertFalse(any(call[:2] == ("put", "stable/latest.json") for call in self.store.calls))
        self.assertEqual(self.store.revisions, revisions)
        self.assertTrue({
            self.release_key("agent.app.tar.gz"), self.release_key("agent.dmg"),
        } <= {call[1] for call in self.store.calls if call[0] == "same"})

    def test_identical_locked_release_retry_never_attempts_an_overwrite(self):
        self.reserve()
        self.publish()
        self.store.calls.clear()
        original_put = self.store.put

        def locked_put(key, data, **kwargs):
            if key in self.store.objects:
                raise publisher.StoreError(403)
            return original_put(key, data, **kwargs)

        with patch.object(self.store, "put", side_effect=locked_put):
            publisher.reserve(self.bundle, self.store)
            self.assertEqual(self.publish(), "already-published")
            self.assertFalse(any(call[0] == "put" for call in self.store.calls))
            missing = self.release_key("agent.app.tar.gz")
            del self.store.objects[missing]
            self.assertEqual(self.publish(), "already-published")
            self.assertEqual([call[1] for call in self.store.calls if call[0] == "put"], [missing])
        self.assertEqual(self.store.revisions["stable/latest.json"], 1)

    def test_immutable_create_race_reuses_only_identical_bytes(self):
        key, data = self.release_key("race.bin"), b"release fixture"
        original_put = self.store.put
        for winning in (data, b"different content"):
            with self.subTest(winning=winning):
                self.store.objects.pop(key, None)
                def racing_put(name, value, **kwargs):
                    self.store.set(name, winning)
                    return original_put(name, value, **kwargs)
                with patch.object(self.store, "put", side_effect=racing_put):
                    if winning == data:
                        publisher.immutable_put(self.store, key, data)
                    else:
                        with self.assertRaisesRegex(publisher.PublishError, "immutable object differs"):
                            publisher.immutable_put(self.store, key, data)
                self.assertEqual(self.store.objects[key], winning)

    def test_immutable_transport_or_authorization_failure_never_becomes_success(self):
        for status in (403, 409, 500):
            with self.subTest(status=status), patch.object(self.store, "put", side_effect=publisher.StoreError(status)):
                with self.assertRaises(publisher.StoreError) as error:
                    publisher.immutable_put(self.store, self.release_key("missing.bin"), b"fixture")
                self.assertEqual(error.exception.status, status)
        with patch.object(self.store, "same", side_effect=publisher.StoreError(403)), \
                patch.object(self.store, "put") as write:
            with self.assertRaises(publisher.StoreError):
                publisher.immutable_put(self.store, self.release_key("missing.bin"), b"fixture")
            write.assert_not_called()

    def test_identical_pointer_repairs_missing_artifact_without_rewriting_pointer(self):
        self.reserve()
        self.publish()
        del self.store.objects[self.release_key("agent.app.tar.gz")]
        self.store.calls.clear()
        self.assertEqual(self.publish(), "already-published")
        self.assertEqual(self.store.objects[self.release_key("agent.app.tar.gz")],
                         (self.bundle / "payloads/agent.app.tar.gz").read_bytes())
        self.assertEqual(self.store.objects["stable/latest.json"], self.envelope)
        self.assertEqual(self.store.revisions["stable/latest.json"], 1)
        self.assertFalse(any(call[:2] == ("put", "stable/latest.json") for call in self.store.calls))

    def test_identical_pointer_does_not_overwrite_corrupt_existing_artifact(self):
        self.reserve()
        self.publish()
        self.store.set(self.release_key("agent.app.tar.gz"), b"corrupt archive")
        self.store.calls.clear()
        with self.assertRaisesRegex(publisher.PublishError, "immutable object differs"):
            self.publish()
        self.assertEqual(self.store.objects[self.release_key("agent.app.tar.gz")], b"corrupt archive")
        self.assertEqual(self.store.objects["stable/latest.json"], self.envelope)
        self.assertEqual(self.store.revisions["stable/latest.json"], 1)
        self.assertFalse(any(call[:2] == ("put", "stable/latest.json") for call in self.store.calls))

    def test_identical_pointer_repairs_missing_complete_manifest_without_pointer_rewrite(self):
        self.reserve()
        self.publish()
        del self.store.objects[self.release_key("manifest.json")]
        self.store.calls.clear()
        self.assertEqual(self.publish(), "already-published")
        self.assertEqual(self.store.objects[self.release_key("manifest.json")], self.envelope)
        self.assertEqual(self.store.objects["stable/latest.json"], self.envelope)
        self.assertEqual(self.store.revisions["stable/latest.json"], 1)
        self.assertFalse(any(call[:2] == ("put", "stable/latest.json") for call in self.store.calls))

    def test_identical_pointer_does_not_overwrite_corrupt_complete_manifest(self):
        self.reserve()
        self.publish()
        self.store.set(self.release_key("manifest.json"), b"corrupt complete manifest")
        self.store.calls.clear()
        with self.assertRaisesRegex(publisher.PublishError, "immutable object differs"):
            self.publish()
        self.assertEqual(self.store.objects[self.release_key("manifest.json")], b"corrupt complete manifest")
        self.assertEqual(self.store.revisions["stable/latest.json"], 1)
        self.assertFalse(any(call[:2] == ("put", "stable/latest.json") for call in self.store.calls))

    def test_equal_or_lower_version_with_different_bytes_is_rejected(self):
        self.reserve()
        self.publish()
        for version in ("1.2.3", "1.2.2"):
            with self.subTest(version=version):
                bundle, _, _ = self.make_bundle(version, "other-" + version, "changed notes")
                if version == "1.2.2":
                    publisher.reserve(bundle, self.store)
                self.store.calls.clear()
                with self.assertRaisesRegex(publisher.PublishError, "reserved before|must advance"):
                    self.publish(bundle)
                self.assertFalse(any(call[0] == "put" for call in self.store.calls))

    def test_equal_version_different_envelope_is_not_treated_as_published_no_op(self):
        self.reserve()
        self.publish()
        envelope = json.loads(self.envelope)
        envelope["signature"] = envelope["signature"].replace("offline fixture, not a real signature", "different fixture comment")
        (self.bundle / "envelope.json").write_bytes(publisher.encode_json(envelope))
        self.store.calls.clear()
        with self.assertRaisesRegex(publisher.PublishError, "release version must advance"):
            self.publish()
        self.assertFalse(any(call[0] == "put" for call in self.store.calls))

    def test_reservation_rejects_changed_notes_or_timestamp(self):
        self.reserve()
        other, _, _ = self.make_bundle("1.2.3", "changed", "changed notes")
        with self.assertRaisesRegex(publisher.PublishError, "immutable object differs"):
            publisher.reserve(other, self.store)
        self.assertFalse((other / "reservation.json").exists())

    def test_reserved_complete_envelope_is_reused_on_retry(self):
        self.reserve()
        self.publish()
        retry, _, _ = self.make_bundle("1.2.3", "retry")
        (retry / "envelope.json").unlink()
        publisher.reserve(retry, self.store)
        self.assertEqual((retry / "envelope.json").read_bytes(), self.envelope)
        self.assertEqual(self.publish(retry), "already-published")

    def test_publish_requires_reservation_and_verification_before_mutations(self):
        with self.assertRaisesRegex(publisher.PublishError, "must be reserved"):
            self.publish()
        self.assertFalse(self.store.objects)
        self.reserve()
        self.store.calls.clear()
        wrong_key = self.root / "other-key.pub"
        wrong_key.write_text(base64.b64encode(b"Ed" + bytes([1]) * 40).decode() + "\n")
        with self.assertRaisesRegex(publisher.PublishError, "key differs"):
            publisher.publish(self.bundle, self.store, wrong_key, fixture_verifier)
        self.assertFalse(any(call[0] == "put" for call in self.store.calls))

    def test_manifest_byte_identity_is_checked_not_only_json_equivalence(self):
        self.reserve()
        (self.bundle / "manifest.json").write_text(json.dumps(self.manifest, indent=2))
        with self.assertRaisesRegex(publisher.PublishError, "manifest bytes differ"):
            self.publish()
        self.assertNotIn("stable/latest.json", self.store.objects)

    def test_invalid_prepared_inventory_is_rejected_before_reservation(self):
        (self.bundle / "payloads/unexpected.txt").write_text("extra")
        with self.assertRaisesRegex(publisher.PublishError, "inventory differs"):
            self.reserve()
        self.assertFalse(self.store.objects)

    def test_corrupt_or_symlink_payload_is_rejected_before_reservation(self):
        payload = self.bundle / "payloads/agent.app.tar.gz"
        payload.write_bytes(b"corrupt")
        with self.assertRaisesRegex(publisher.PublishError, "hash differs"):
            self.reserve()
        payload.unlink()
        payload.symlink_to(self.bundle / "manifest.json")
        with self.assertRaises(OSError):
            self.reserve()
        self.assertFalse(self.store.objects)

    def test_live_verification_covers_public_feed_immutable_head_range_and_download_routes(self):
        origin = FixtureOrigin(self.bundle, self.manifest, self.envelope)
        with patch.object(publisher, "minisign_verify", fixture_verifier), \
                patch.object(publisher.urllib.request, "build_opener", return_value=origin):
            result = publisher.verify_live(self.bundle, self.bundle / "public-key.pub", "https://updates.example")
        self.assertEqual(result, self.manifest)
        self.assertIn(("GET", "/agent/stable/latest.json", None), origin.calls)
        self.assertIn(("GET", publisher.public_path("1.2.3", "manifest.json"), None), origin.calls)
        self.assertIn(("GET", "/download/agent/stable/macos", None), origin.calls)
        for record in [*self.manifest["artifacts"].values(), *self.manifest["downloads"].values()]:
            for method, range_header in (("HEAD", None), ("GET", "bytes=0-0"), ("GET", None)):
                self.assertIn((method, record["path"], range_header), origin.calls)

    def test_live_dual_feed_verification_checks_linux_envelope_payload_and_alias(self):
        bundle, manifest, envelope = self.make_bundle("1.2.3", "dual-live", linux=True)
        origin = FixtureOrigin(bundle, manifest, envelope)
        with patch.object(publisher, "minisign_verify", fixture_verifier), \
                patch.object(publisher.urllib.request, "build_opener", return_value=origin):
            publisher.verify_live(bundle, bundle / "public-key.pub", "https://updates.example")
        for path in ("/agent/stable/linux/latest.json", publisher.public_path("1.2.3", "linux-manifest.json"),
                     "/download/agent/stable/linux"):
            self.assertIn(("GET", path, None), origin.calls)
        record = json.loads((bundle / "linux-manifest.json").read_bytes())["artifacts"]["linux-x86_64"]
        for method, range_header in (("HEAD", None), ("GET", "bytes=0-0"), ("GET", None)):
            self.assertIn((method, record["path"], range_header), origin.calls)

    def test_linux_bytes_cannot_change_after_reservation_to_obtain_a_signature(self):
        bundle, _, _ = self.make_bundle("1.2.3", "changed-linux-sign", linux=True)
        publisher.reserve(bundle, self.store)
        changed = json.loads((bundle / "linux-manifest.json").read_bytes())
        changed["artifacts"]["linux-x86_64"]["path"] = publisher.public_path("1.2.3", "other-linux.tar.gz")
        changed["downloads"]["linux-bundle"] = changed["artifacts"]["linux-x86_64"]
        (bundle / "payloads/agent-linux.tar.gz").rename(bundle / "payloads/other-linux.tar.gz")
        (bundle / "linux-manifest.json").write_bytes(publisher.encode_json(changed))
        with patch.object(publisher.subprocess, "run") as signer, \
                self.assertRaisesRegex(publisher.PublishError, "not been reserved"):
            publisher.sign(bundle, self.root / "unused-secret", bundle / "public-key.pub")
        signer.assert_not_called()

    def test_live_verification_sends_native_headers_through_urllib_for_every_request(self):
        origin = FixtureOrigin(self.bundle, self.manifest, self.envelope)
        requests = []

        def offline_https(_handler, request):
            headers = (request.get_header("User-agent"), request.get_header("Accept-encoding"),
                       request.get_header("X-maple-download-probe"))
            requests.append((request.get_method(), urlsplit(request.full_url).path,
                             request.get_header("Range"), headers))
            # Model the public edge's client policy through urllib's real
            # request, response and redirect handlers, without network access.
            response = origin.open(request, request.timeout) if headers == ("Maple-Agent-Updater/1", "identity", "1") \
                else FakeResponse(b"client headers required", status=403)
            response_headers = Message()
            for name, value in response.headers.items():
                response_headers[name] = value
            result = addinfourl(response, response_headers, request.full_url, response.status)
            result.msg = "Offline fixture"
            return result

        with patch.object(publisher, "minisign_verify", fixture_verifier), \
                patch.object(publisher.urllib.request, "getproxies", return_value={}), \
                patch.object(publisher.urllib.request.HTTPSHandler, "https_open", offline_https):
            result = publisher.verify_live(self.bundle, self.bundle / "public-key.pub", "https://updates.example")

        self.assertEqual(result, self.manifest)
        # Two metadata GETs, HEAD/range/full GET for each of two payloads, and
        # one installer redirect GET. The redirect must never be followed.
        self.assertEqual(len(requests), 9)
        self.assertEqual({request[3] for request in requests}, {("Maple-Agent-Updater/1", "identity", "1")})
        self.assertIn(("GET", "/agent/stable/latest.json", None), origin.calls)
        self.assertIn(("GET", publisher.public_path("1.2.3", "manifest.json"), None), origin.calls)
        self.assertIn(("GET", "/download/agent/stable/macos", None), origin.calls)
        for record in [*self.manifest["artifacts"].values(), *self.manifest["downloads"].values()]:
            for method, range_header in (("HEAD", None), ("GET", "bytes=0-0"), ("GET", None)):
                self.assertIn((method, record["path"], range_header), origin.calls)

    def test_live_metadata_failure_identifies_fixed_path_and_status_without_response_content(self):
        for path in ("/agent/stable/latest.json", publisher.public_path("1.2.3", "manifest.json")):
            for status in (403, 200):
                with self.subTest(path=path, status=status):
                    origin = FixtureOrigin(self.bundle, self.manifest, self.envelope)
                    original_open = origin.open

                    def wrong_metadata(request, timeout):
                        if urlsplit(request.full_url).path == path:
                            return FakeResponse(b"private upstream response", status=status,
                                                headers={"Private-Header": "private upstream header"})
                        return original_open(request, timeout)

                    with patch.object(publisher, "minisign_verify", fixture_verifier), \
                            patch.object(publisher.urllib.request, "build_opener", return_value=origin), \
                            patch.object(origin, "open", side_effect=wrong_metadata), \
                            self.assertRaises(publisher.PublishError) as raised:
                        publisher.verify_live(self.bundle, self.bundle / "public-key.pub", "https://updates.example")
                    self.assertEqual(str(raised.exception), f"live signed metadata differs at {path} (HTTP {status})")

    def test_live_verification_rejects_foreign_or_wrong_download_redirect(self):
        for redirect in ("https://foreign.example/app.dmg", "/agent/stable/releases/1.2.4/app.dmg"):
            with self.subTest(redirect=redirect):
                origin = FixtureOrigin(self.bundle, self.manifest, self.envelope)
                origin.redirect = redirect
                with patch.object(publisher, "minisign_verify", fixture_verifier), \
                        patch.object(publisher.urllib.request, "build_opener", return_value=origin), \
                        self.assertRaisesRegex(publisher.PublishError, "download redirect differs"):
                    publisher.verify_live(self.bundle, self.bundle / "public-key.pub", "https://updates.example")

    def test_live_verification_rejects_wrong_range_response(self):
        origin = FixtureOrigin(self.bundle, self.manifest, self.envelope)
        origin.range_header = "bytes 0-0/999999"
        with patch.object(publisher, "minisign_verify", fixture_verifier), \
                patch.object(publisher.urllib.request, "build_opener", return_value=origin), \
                self.assertRaisesRegex(publisher.PublishError, "range request failed"):
            publisher.verify_live(self.bundle, self.bundle / "public-key.pub", "https://updates.example")

    def test_release_filenames_match_worker_ambiguity_and_length_limits(self):
        for filename in ("agent..app.tar.gz", "a" * 201 + ".dmg", "../app.dmg", "app%2fdmg"):
            with self.subTest(filename=filename), self.assertRaises(publisher.PublishError):
                publisher.release_key("1.2.3", filename)


class PackageBoundaryTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        profiles = json.loads((publisher.ROOT / "apps/maple-agent/release-profiles.json").read_text())
        metadata_spec = importlib.util.spec_from_file_location(
            "fixture_release_info", publisher.ROOT / "apps/maple-agent/scripts/release-info.py")
        release_info = importlib.util.module_from_spec(metadata_spec)
        metadata_spec.loader.exec_module(release_info)
        self.info = {field: profiles["prod"][field] for field in release_info.PUBLIC_FIELDS}
        self.info.update({"profile": "prod", "version": "1.2.3",
                          "source_sha": SOURCE_SHA, "git_revision": SOURCE_SHA[:9]})
        self.stem = "maple-agent-prod-1.2.3-" + SOURCE_SHA[:9] + "-macos-aarch64"
        self.metadata = {"schema_version": 1, "profile": "prod", "platform": "macos-aarch64",
                         "source_sha": SOURCE_SHA, "version": "1.2.3", "package": self.stem + ".dmg",
                         "distribution": "master", "macos_signing": "developer-id-notarized"}
        (self.directory / "build-info.json").write_bytes(publisher.encode_json(self.info))
        (self.directory / "manifest.json").write_bytes(publisher.encode_json(self.metadata))
        (self.directory / (self.stem + ".dmg")).write_bytes(b"fake disk image; never execute")
        self.write_archive()
        for name in ("notarization-log.json", "dmg-notarization-log.json"):
            (self.directory / name).write_bytes(publisher.encode_json({"status": "Accepted", "issues": None}))
        self.checksums()
        self.attested = []

    def write_archive(self, plist=None, entries=None):
        if plist is None:
            plist = {"CFBundleIdentifier": "cloud.opensecret.maple.agent",
                     "CFBundleShortVersionString": "1.2.3", "CFBundleVersion": "42"}
        if entries is None:
            entries = [("Maple Agent.app/Contents/Info.plist", tarfile.REGTYPE, plistlib.dumps(plist))]
        with tarfile.open(self.directory / (self.stem + ".app.tar.gz"), "w:gz") as archive:
            for name, kind, data in entries:
                member = tarfile.TarInfo(name)
                member.type = kind
                member.mode = 0o644
                if kind == tarfile.REGTYPE:
                    member.size = len(data)
                    archive.addfile(member, io.BytesIO(data))
                else:
                    member.linkname = "Other.plist"
                    archive.addfile(member)

    def checksums(self):
        (self.directory / "SHA256SUMS").write_text("".join(
            hashlib.sha256(path.read_bytes()).hexdigest() + "  " + path.name + "\n"
            for path in sorted(self.directory.iterdir()) if path.name != "SHA256SUMS" and path.is_file()
        ))

    def validate(self):
        return publisher.validate_package(self.directory, "macos-aarch64", SOURCE_SHA, "1.2.3",
                                          attestor=lambda path, source: self.attested.append((path.name, source)),
                                          expected_build="42")

    def test_exact_final_inventory_is_hashed_and_each_file_attested_without_execution(self):
        result = self.validate()
        self.assertEqual([path.name for path, _ in result], [self.stem + ".dmg", self.stem + ".app.tar.gz"])
        self.assertEqual({name for name, _ in self.attested}, {path.name for path in self.directory.iterdir()})
        self.assertTrue(all(source == SOURCE_SHA for _, source in self.attested))

    def test_extra_files_missing_files_and_symlinks_are_rejected(self):
        extra = self.directory / "unexpected.json"
        extra.write_text("{}")
        with self.assertRaisesRegex(publisher.PublishError, "unexpected or missing"):
            self.validate()
        extra.unlink()
        target = self.directory / "notarization-log.json"
        target.unlink()
        with self.assertRaisesRegex(publisher.PublishError, "unexpected or missing"):
            self.validate()
        target.symlink_to(self.directory / "dmg-notarization-log.json")
        with self.assertRaises(OSError):
            self.validate()
        self.assertFalse(self.attested)

    def test_corrupt_checksum_duplicate_and_traversal_records_are_rejected(self):
        checksums = self.directory / "SHA256SUMS"
        original = checksums.read_text()
        for value in (original.replace(original[:64], "b" * 64, 1),
                      original + original.splitlines()[0] + "\n",
                      "a" * 64 + "  ../outside\n"):
            with self.subTest(value=value):
                checksums.write_text(value)
                with self.assertRaises(publisher.PublishError):
                    self.validate()
        self.assertFalse(self.attested)

    def test_unsigned_or_dev_identity_is_rejected_even_with_valid_checksums(self):
        self.metadata["distribution"] = "unsigned-pr"
        (self.directory / "manifest.json").write_bytes(publisher.encode_json(self.metadata))
        self.checksums()
        with self.assertRaisesRegex(publisher.PublishError, "invalid final package metadata"):
            self.validate()
        self.info["profile"] = "dev"
        (self.directory / "build-info.json").write_bytes(publisher.encode_json(self.info))
        self.checksums()
        with self.assertRaisesRegex(publisher.PublishError, "Prod build identity"):
            self.validate()
        self.assertFalse(self.attested)

    def test_rejected_notarization_and_duplicate_metadata_keys_are_rejected(self):
        (self.directory / "notarization-log.json").write_bytes(publisher.encode_json({"status": "Rejected", "issues": []}))
        self.checksums()
        with self.assertRaisesRegex(publisher.PublishError, "notarization was not accepted"):
            self.validate()
        (self.directory / "build-info.json").write_bytes(b'{"profile":"prod","profile":"prod"}')
        with self.assertRaisesRegex(publisher.PublishError, "duplicate JSON field"):
            self.validate()
        self.assertFalse(self.attested)

    def test_archive_build_version_and_bundle_identity_match_trusted_publication(self):
        for field, value in (("CFBundleVersion", "43"), ("CFBundleShortVersionString", "1.2.4"),
                             ("CFBundleIdentifier", "cloud.opensecret.maple")):
            with self.subTest(field=field):
                plist = {"CFBundleIdentifier": "cloud.opensecret.maple.agent",
                         "CFBundleShortVersionString": "1.2.3", "CFBundleVersion": "42", field: value}
                self.write_archive(plist)
                self.checksums()
                with self.assertRaises(publisher.PublishError):
                    self.validate()
                self.assertFalse(self.attested)

    def test_archive_duplicate_or_missing_info_plist_is_rejected_without_extraction(self):
        entry = ("Maple Agent.app/Contents/Info.plist", tarfile.REGTYPE,
                 plistlib.dumps({"CFBundleIdentifier": "cloud.opensecret.maple.agent",
                                "CFBundleShortVersionString": "1.2.3", "CFBundleVersion": "42"}))
        for entries in ([entry, entry], [("Other.app/Contents/Info.plist", *entry[1:])], []):
            with self.subTest(entries=len(entries)):
                self.write_archive(entries=entries)
                self.checksums()
                with self.assertRaises(publisher.PublishError):
                    self.validate()
                self.assertFalse(self.attested)

    def test_archive_nonregular_or_oversized_info_plist_is_rejected(self):
        for kind, data in ((tarfile.SYMTYPE, b""), (tarfile.LNKTYPE, b""),
                           (tarfile.FIFOTYPE, b""), (tarfile.REGTYPE, bytes(64 * 1024 + 1))):
            with self.subTest(kind=kind, size=len(data)):
                self.write_archive(entries=[("Maple Agent.app/Contents/Info.plist", kind, data)])
                self.checksums()
                with self.assertRaises(publisher.PublishError):
                    self.validate()
                self.assertFalse(self.attested)

    def test_archive_invalid_plist_or_nondictionary_plist_is_rejected(self):
        for data in (b"invalid plist", b'<?xml version="1.0"?><plist><dict><key>broken</key>',
                     plistlib.dumps(["unexpected", "plist", "shape"])):
            with self.subTest(data=data[:20]):
                self.write_archive(entries=[("Maple Agent.app/Contents/Info.plist", tarfile.REGTYPE, data)])
                self.checksums()
                with self.assertRaises(publisher.PublishError):
                    self.validate()
                self.assertFalse(self.attested)

    def test_archive_invalid_container_is_rejected_with_bounded_error(self):
        (self.directory / (self.stem + ".app.tar.gz")).write_bytes(b"not a gzip or tar archive")
        self.checksums()
        with self.assertRaises(publisher.PublishError):
            self.validate()
        self.assertFalse(self.attested)

    def test_archive_oversized_member_is_rejected_before_reading_its_body(self):
        member = tarfile.TarInfo("Maple Agent.app/Contents/oversized")
        member.size = publisher.MAX_ARTIFACT + 1
        # Only the header exists. The size gate must reject it before a stream
        # parser tries to skip/decompress the declared multi-gigabyte body.
        data = gzip.compress(member.tobuf() + bytes(1024))
        (self.directory / (self.stem + ".app.tar.gz")).write_bytes(data)
        self.checksums()
        with self.assertRaisesRegex(publisher.PublishError, "bounded inventory|oversized member"):
            self.validate()
        self.assertFalse(self.attested)

    def test_archive_extension_headers_are_bounded_before_tarfile_parses_them(self):
        for kind in (tarfile.XHDTYPE, tarfile.XGLTYPE, tarfile.SOLARIS_XHDTYPE,
                     tarfile.GNUTYPE_LONGNAME, tarfile.GNUTYPE_LONGLINK):
            with self.subTest(kind=kind):
                member = tarfile.TarInfo("extension")
                member.type, member.size = kind, 64 * 1024 + 1
                data = gzip.compress(member.tobuf() + bytes(1024))
                (self.directory / (self.stem + ".app.tar.gz")).write_bytes(data)
                self.checksums()
                with self.assertRaisesRegex(publisher.PublishError, "extension metadata exceeds bounds"):
                    self.validate()
                self.assertFalse(self.attested)

    def test_archive_sparse_member_is_not_interpreted_as_an_app_payload(self):
        member = tarfile.TarInfo("sparse")
        member.type = tarfile.GNUTYPE_SPARSE
        data = gzip.compress(member.tobuf() + bytes(1024))
        (self.directory / (self.stem + ".app.tar.gz")).write_bytes(data)
        self.checksums()
        with self.assertRaisesRegex(publisher.PublishError, "sparse app archive members"):
            self.validate()
        self.assertFalse(self.attested)

    def test_pax_sparse_variants_are_rejected_before_stdlib_sparse_map_parsing(self):
        cases = (
            ({"GNU.sparse.major": "1", "GNU.sparse.minor": "0", "GNU.sparse.realsize": "999999999"},
             "_proc_gnusparse_10", b"999999999\n"),
            ({"GNU.sparse.map": "0,999999999", "GNU.sparse.size": "999999999"},
             "_proc_gnusparse_01", b""),
            ({"GNU.sparse.size": "999999999", "GNU.sparse.numblocks": "999999999"},
             "_proc_gnusparse_00", b""),
        )
        for headers, parser, body in cases:
            with self.subTest(parser=parser):
                member = tarfile.TarInfo("Maple Agent.app/Contents/sparse")
                member.pax_headers, member.size = headers, len(body)
                with tarfile.open(self.directory / (self.stem + ".app.tar.gz"), "w:gz") as archive:
                    archive.addfile(member, io.BytesIO(body))
                self.checksums()
                # Keep even a regression safe: reaching the standard library's
                # sparse parser fails immediately before attacker map counts
                # can drive its list allocation or block-reading loops.
                with patch.object(tarfile.TarInfo, parser, side_effect=AssertionError("stdlib sparse parser reached")), \
                        self.assertRaisesRegex(publisher.PublishError, "sparse app archive members are unsupported"):
                    self.validate()
                self.assertFalse(self.attested)


class LinuxPackageBoundaryTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        profiles = json.loads((publisher.ROOT / "apps/maple-agent/release-profiles.json").read_text())
        spec = importlib.util.spec_from_file_location("linux_fixture_info", publisher.ROOT / "apps/maple-agent/scripts/release-info.py")
        release_info = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(release_info)
        self.info = {field: profiles["prod"][field] for field in release_info.PUBLIC_FIELDS}
        self.info.update({"profile": "prod", "version": "1.2.3", "source_sha": SOURCE_SHA,
                          "git_revision": SOURCE_SHA[:9], "update_feed_url": publisher.LINUX_FEED_URL})
        self.stem = "maple-agent-prod-1.2.3-" + SOURCE_SHA[:9] + "-linux-x86_64"
        self.descriptor = {"format": "maple-agent-linux-bundle-v1", "product": "maple-agent",
                           "profile": "prod", "version": "1.2.3", "build": "42",
                           "source_sha": SOURCE_SHA, "bundle_id": "cloud.opensecret.maple.agent",
                           "entrypoint": "AppRun", "executable": "usr/bin/maple-agent"}
        metadata = {"schema_version": 1, "profile": "prod", "platform": "linux-x86_64",
                    "source_sha": SOURCE_SHA, "version": "1.2.3", "package": self.stem + ".tar.gz",
                    "distribution": "master", "macos_signing": None, "linux_integrity": "sha256"}
        (self.directory / "build-info.json").write_bytes(publisher.encode_json(self.info))
        (self.directory / "manifest.json").write_bytes(publisher.encode_json(metadata))
        self.write_archive()
        self.checksums()
        self.attested = []

    def write_archive(self, entries=None):
        if entries is None:
            entries = [("maple-agent.app/bundle.json", tarfile.REGTYPE, publisher.encode_json(self.descriptor))]
        with tarfile.open(self.directory / (self.stem + ".tar.gz"), "w:gz") as archive:
            for name, kind, data in entries:
                member = tarfile.TarInfo(name)
                member.type, member.mode = kind, 0o644
                if kind == tarfile.REGTYPE:
                    member.size = len(data)
                    archive.addfile(member, io.BytesIO(data))
                else:
                    member.linkname = "other.json"
                    archive.addfile(member)

    def checksums(self):
        (self.directory / "SHA256SUMS").write_text("".join(
            hashlib.sha256(path.read_bytes()).hexdigest() + "  " + path.name + "\n"
            for path in sorted(self.directory.iterdir()) if path.name != "SHA256SUMS"
        ))

    def validate(self):
        return publisher.validate_package(self.directory, "linux-x86_64", SOURCE_SHA, "1.2.3",
                                          attestor=lambda path, source: self.attested.append((path.name, source)),
                                          expected_build="42")

    def test_actual_tar_descriptor_binds_build_source_identity_and_attests_all_four_files(self):
        records = self.validate()
        self.assertEqual(len(records), 1)
        self.assertEqual(records[0][0].name, self.stem + ".tar.gz")
        self.assertEqual(len(self.attested), 4)
        self.assertTrue(all(source == SOURCE_SHA for _, source in self.attested))

    def test_wrong_or_extra_descriptor_identity_fields_fail_before_attestation(self):
        original = dict(self.descriptor)
        for field, value in (("build", "43"), ("source_sha", "b" * 40), ("version", "1.2.4"),
                             ("profile", "dev"), ("entrypoint", "other"), ("executable", "other"),
                             ("bundle_id", "other"), ("extra", True)):
            with self.subTest(field=field):
                self.descriptor = {**original, field: value}
                self.write_archive()
                self.checksums()
                with self.assertRaisesRegex(publisher.PublishError, "differs from selected run"):
                    self.validate()
                self.assertFalse(self.attested)

    def test_missing_duplicate_nonregular_or_oversized_linux_descriptor_fails(self):
        entry = ("maple-agent.app/bundle.json", tarfile.REGTYPE, publisher.encode_json(self.descriptor))
        for entries in ([], [entry, entry], [(entry[0], tarfile.SYMTYPE, b"")],
                        [(entry[0], tarfile.REGTYPE, b"null"), entry],
                        [(entry[0], tarfile.REGTYPE, bytes(publisher.MAX_MANIFEST + 1))]):
            with self.subTest(entries=len(entries)):
                self.write_archive(entries)
                self.checksums()
                with self.assertRaises(publisher.PublishError):
                    self.validate()
                self.assertFalse(self.attested)

    def test_actual_preparation_keeps_mac_wire_compatible_and_reserves_linux_separately(self):
        mac = PackageBoundaryTests()
        mac.setUp()
        self.addCleanup(mac.doCleanups)
        notes = self.directory.parent / (self.directory.name + "-notes")
        notes.write_text("release notes")
        self.addCleanup(notes.unlink)
        output = self.directory / "prepared"
        key = self.directory.parent / (self.directory.name + "-key")
        key.write_text(PUBLIC_KEY + "\n")
        self.addCleanup(key.unlink)
        proof = {"run_id": 123, "run_attempt": 1, "source_sha": SOURCE_SHA, "build": "42",
                 "pub_date": "2026-10-21T18:00:00Z", "artifacts": {"macos": "mac", "linux": "linux"}}
        validator = publisher.validate_package
        def offline_validation(*args, **kwargs):
            return validator(*args, **kwargs, attestor=lambda path, source: None)
        with patch.object(publisher, "trust_run", return_value=proof), \
                patch.object(publisher, "check_source_pins", return_value=PUBLIC_KEY) as pins, \
                patch.object(publisher, "validate_package", side_effect=offline_validation):
            prepared = publisher.prepare(123, "1.2.3", "42", mac.directory, self.directory, notes, output, key)
        pins.assert_called_once_with(key, SOURCE_SHA, True)
        self.assertEqual(set(prepared["artifacts"]), {"darwin-aarch64"})
        self.assertEqual(set(prepared["downloads"]), {"macos"})
        linux = publisher.validate_manifest((output / "linux-manifest.json").read_bytes())
        self.assertEqual(linux["format"], "maple-agent-update-v2")
        self.assertEqual(linux["build"], prepared["build"])
        self.assertEqual(linux["source_sha"], prepared["source_sha"])
        self.assertEqual(len(list((output / "payloads").iterdir())), 3)
        store = FakeStore()
        publisher.reserve(output, store)
        self.assertEqual(store.objects[publisher.release_key("1.2.3", "publication-manifest.json")], (output / "manifest.json").read_bytes())
        self.assertEqual(store.objects[publisher.release_key("1.2.3", "publication-linux-manifest.json")], (output / "linux-manifest.json").read_bytes())


class TrustedRunTests(unittest.TestCase):
    def setUp(self):
        self.run = {
            "id": 123, "run_attempt": 2, "run_number": 42,
            "repository": {"full_name": publisher.REPOSITORY},
            "head_repository": {"full_name": publisher.REPOSITORY},
            "path": publisher.WORKFLOW, "head_branch": "master", "event": "push",
            "status": "completed", "conclusion": "success", "head_sha": SOURCE_SHA,
        }
        self.names = {"Signed macOS (prod)", "Verify downloaded macOS (prod)"}

    def jobs(self, names=None):
        jobs = [{"name": name, "status": "completed", "conclusion": "success"} for name in sorted(names or self.names)]
        return {"total_count": len(jobs), "jobs": jobs}

    def test_macos_requires_concrete_completed_prod_jobs(self):
        self.assertEqual(publisher.validate_run(self.run, self.jobs()), self.run)
        for conclusion in ("skipped", "failure", "cancelled", None):
            jobs = self.jobs()
            jobs["jobs"][0]["conclusion"] = conclusion
            with self.subTest(conclusion=conclusion), self.assertRaises(publisher.PublishError):
                publisher.validate_run(self.run, jobs)
        with self.assertRaisesRegex(publisher.PublishError, "required Prod package job"):
            publisher.validate_run(self.run, self.jobs({"Select Agent packaging"}))

    def test_linux_is_optional_but_each_linux_gate_is_required_when_selected(self):
        with self.assertRaises(publisher.PublishError):
            publisher.validate_run(self.run, self.jobs(), linux=True)
        names = self.names | {"Linux bundle (prod)", "Attest Linux packages (prod)", "Verify downloaded Linux (prod)"}
        publisher.validate_run(self.run, self.jobs(names), linux=True)
        for name in names - self.names:
            with self.subTest(name=name), self.assertRaises(publisher.PublishError):
                publisher.validate_run(self.run, self.jobs(names - {name}), linux=True)

    def test_incomplete_duplicate_or_untrusted_job_inventory_fails_closed(self):
        jobs = self.jobs()
        jobs["total_count"] += 1
        with self.assertRaisesRegex(publisher.PublishError, "incomplete"):
            publisher.validate_run(self.run, jobs)
        jobs = self.jobs()
        jobs["jobs"].append(jobs["jobs"][0].copy())
        jobs["total_count"] += 1
        with self.assertRaises(publisher.PublishError):
            publisher.validate_run(self.run, jobs)
        for field, value in (("head_branch", "feature"), ("event", "pull_request"),
                             ("path", ".github/workflows/release.yml"), ("head_sha", "a" * 7)):
            with self.subTest(field=field), self.assertRaises(publisher.PublishError):
                publisher.validate_run({**self.run, field: value}, self.jobs())
        with self.assertRaisesRegex(publisher.PublishError, "untrusted build repository"):
            publisher.validate_run({**self.run, "head_repository": {"full_name": "other/Maple"}}, self.jobs())


if __name__ == "__main__":
    unittest.main()
