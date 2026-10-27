"""Verify the shared public wire fixture with the pinned real minisign CLI."""

import base64
import json
import hashlib
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import publisher


FIXTURES = Path(__file__).with_name("fixtures")


@unittest.skipUnless(shutil.which("minisign"), "run in the pinned root CI shell for cryptographic tests")
class MinisignTests(unittest.TestCase):
    def setUp(self):
        self.data = (FIXTURES / "signed-envelope.json").read_bytes()
        self.key = FIXTURES / "public-test-key.pub"

    def test_shared_client_fixture_verifies_prehashed_with_exact_comment(self):
        manifest = publisher.minisign_verify(self.data, self.key)
        self.assertEqual(manifest["version"], "0.0.2")
        envelope = json.loads(self.data)
        self.assertEqual(envelope["manifest"].encode(), (FIXTURES / "manifest.json").read_bytes())
        self.assertEqual(envelope["signature"].encode(), (FIXTURES / "manifest.minisig").read_bytes())

    def test_raw_public_key_file_verifies_with_the_real_cli(self):
        with tempfile.TemporaryDirectory(prefix="maple-agent-public-key-test-") as temporary:
            key = Path(temporary) / "one-line.pub"
            key.write_text(publisher.public_key_text(self.key) + "\n")
            self.assertEqual(publisher.minisign_verify(self.data, key)["version"], "0.0.2")

    def test_windows_native_client_shared_fixture_verifies_exact_v3_bytes(self):
        shared = publisher.ROOT / "apps/maple-agent/crates/maple-agent-updater/tests/fixtures"
        envelope = (shared / "signed-windows-v3-envelope.json").read_bytes()
        manifest = publisher.minisign_verify(envelope, self.key)
        self.assertEqual(manifest["format"], "maple-agent-update-v3")
        self.assertEqual(json.loads(envelope)["manifest"].encode(), (shared / "windows-v3-manifest.json").read_bytes())

    def test_existing_signed_pointer_rotation_retry_and_missing_object_repair(self):
        class Store:
            def __init__(self):
                self.objects = {}
                self.writes = []

            def get(self, key, maximum=publisher.MAX_JSON):
                if key not in self.objects:
                    return None
                data = self.objects[key]
                return data, '"' + hashlib.sha256(data).hexdigest() + '"'

            def same(self, key, digest, size):
                value = self.objects.get(key)
                return value is not None and len(value) == size and hashlib.sha256(value).hexdigest() == digest

            def put(self, key, data, *, condition, content_type):
                current = self.get(key)
                if condition == ("If-None-Match", "*"):
                    if current is not None:
                        raise publisher.StoreError(412)
                elif current is None or condition != ("If-Match", current[1]):
                    raise publisher.StoreError(412)
                self.writes.append(key)
                self.objects[key] = data.read_bytes() if isinstance(data, Path) else data

        with tempfile.TemporaryDirectory(prefix="maple-agent-pointer-test-") as temporary:
            root = Path(temporary)
            bundle = root / "bundle"
            bundle.mkdir()
            (bundle / "manifest.json").write_bytes((FIXTURES / "manifest.json").read_bytes())
            (bundle / "envelope.json").write_bytes(self.data)
            # Match the workflow variable's raw one-line public-key input.
            (bundle / "public-key.pub").write_text(publisher.public_key_text(self.key) + "\n")
            payloads = bundle / "payloads"
            payloads.mkdir()
            for name in ("Maple-Agent.app.tar.gz", "Maple-Agent.dmg"):
                (payloads / name).write_bytes(b"signed payload fixture bytes")
            metadata = bundle / "metadata"
            metadata.mkdir()
            inventory = {}
            for name in sorted({"provenance.json"} | publisher.MACOS_METADATA_NAMES):
                data = ("offline public signing fixture: " + name + "\n").encode()
                (metadata / name).write_bytes(data)
                inventory[name] = {"sha256": hashlib.sha256(data).hexdigest(), "size": len(data)}
            (bundle / "metadata-inventory.json").write_bytes(publisher.encode_json(inventory))
            profile = root / "apps/maple-agent/release-profiles.json"
            profile.parent.mkdir(parents=True)
            # The first valid public key does not sign this envelope, exercising
            # the pinned-key fallback used during rotation with the real CLI.
            other_key = base64.b64encode(b"Ed" + bytes(40)).decode()
            profile.write_bytes(publisher.encode_json({"prod": {
                "update_feed_url": publisher.FEED_URL,
                "update_public_keys": [other_key, publisher.public_key_text(self.key)],
            }}))
            store = Store()
            with patch.object(publisher, "ROOT", root):
                publisher.reserve(bundle, store, self.key)
                self.assertEqual(publisher.publish(bundle, store, self.key), "published")
                store.writes.clear()
                self.assertEqual(publisher.publish(bundle, store, self.key), "already-published")
                self.assertEqual(store.writes, [])
                missing = "stable/releases/0.0.2/Maple-Agent.app.tar.gz"
                del store.objects[missing]
                self.assertEqual(publisher.publish(bundle, store, self.key), "already-published")
                self.assertEqual(store.writes, [missing])
                self.assertEqual(store.objects["stable/latest.json"], self.data)
                tampered = json.loads(self.data)
                tampered["manifest"] = tampered["manifest"].replace("A test update.", "A fake update.")
                with self.assertRaisesRegex(publisher.PublishError, "not trusted"):
                    publisher.verify_current(publisher.encode_json(tampered), self.key)

    def test_tampered_signed_bytes_and_comment_are_rejected(self):
        envelope = json.loads(self.data)
        envelope["manifest"] = envelope["manifest"].replace("A test update.", "A fake update.")
        with self.assertRaisesRegex(publisher.PublishError, "verification failed"):
            publisher.minisign_verify(publisher.encode_json(envelope), self.key)
        envelope = json.loads(self.data)
        envelope["signature"] = envelope["signature"].replace("channel=stable", "channel=preview")
        with self.assertRaisesRegex(publisher.PublishError, "comment"):
            publisher.minisign_verify(publisher.encode_json(envelope), self.key)

    def test_dual_feed_real_signatures_share_key_and_retry_exactly(self):
        self.check_multiplatform_signatures(windows=False)

    def test_all_three_feed_real_signatures_share_key_and_retry_exactly(self):
        self.check_multiplatform_signatures(windows=True)

    def check_multiplatform_signatures(self, windows):
        from test_publisher import FakeStore, PublicationTests
        fixtures = PublicationTests()
        fixtures.setUp()
        self.addCleanup(fixtures.doCleanups)
        bundle, _, _ = fixtures.make_bundle("1.2.3", "real-multiplatform", linux=True, windows=windows)
        key, secret = bundle / "public-key.pub", fixtures.root / "offline-dual-secret.key"
        key.unlink()
        result = subprocess.run(["minisign", "-G", "-W", "-p", str(key), "-s", str(secret)],
                                stdin=subprocess.DEVNULL, capture_output=True, timeout=30)
        self.assertEqual(result.returncode, 0, "offline temporary test key generation failed")
        names = ["envelope.json", "linux-envelope.json"] + (["windows-envelope.json"] if windows else [])
        for name in names:
            (bundle / name).unlink()
        store = FakeStore()
        publisher.reserve(bundle, store, key)
        publisher.sign(bundle, secret, key)
        envelopes = {name: (bundle / name).read_bytes() for name in names}
        self.assertEqual(publisher.minisign_verify(envelopes["envelope.json"], key)["format"], "maple-agent-update-v1")
        self.assertEqual(publisher.minisign_verify(envelopes["linux-envelope.json"], key)["format"], "maple-agent-update-v2")
        if windows:
            self.assertEqual(publisher.minisign_verify(envelopes["windows-envelope.json"], key)["format"], "maple-agent-update-v3")
        publisher.sign(bundle, secret, key)
        self.assertEqual(envelopes, {name: (bundle / name).read_bytes() for name in envelopes})
        self.assertEqual(publisher.publish(bundle, store, key, previous_verifier=publisher.minisign_verify), "published")
        publisher.reserve(bundle, store, key)
        self.assertEqual(publisher.publish(bundle, store, key, previous_verifier=publisher.minisign_verify), "already-published")
        self.assertEqual(envelopes["envelope.json"], store.objects["stable/latest.json"])
        self.assertEqual(envelopes["linux-envelope.json"], store.objects["stable/linux/latest.json"])
        if windows:
            self.assertEqual(envelopes["windows-envelope.json"], store.objects["stable/windows/latest.json"])

    def test_real_temporary_key_reservation_signing_verification_and_retry(self):
        class ReservationStore:
            def __init__(self):
                self.objects = {}

            def put(self, key, data, *, condition, content_type):
                self.objects[key] = data

            def get(self, key, maximum=publisher.MAX_JSON):
                return (self.objects[key], '"fixture-etag"') if key in self.objects else None

            def same(self, key, digest, size):
                data = self.objects.get(key)
                return data is not None and len(data) == size and hashlib.sha256(data).hexdigest() == digest

        # These disposable, offline test keys never leave the private tempdir.
        # Production key generation is an owner operation outside this tool.
        with tempfile.TemporaryDirectory(prefix="maple-agent-minisign-test-") as temporary:
            bundle = Path(temporary)
            key = bundle / "public-key.pub"
            secret = bundle / "test-only-secret.key"
            result = subprocess.run(["minisign", "-G", "-W", "-p", str(key), "-s", str(secret)], stdin=subprocess.DEVNULL, capture_output=True, timeout=30)
            self.assertEqual(result.returncode, 0, "offline temporary test key generation failed")
            (bundle / "manifest.json").write_bytes((FIXTURES / "manifest.json").read_bytes())
            payloads = bundle / "payloads"
            payloads.mkdir()
            for name in ("Maple-Agent.app.tar.gz", "Maple-Agent.dmg"):
                (payloads / name).write_bytes(b"signed payload fixture bytes")
            metadata = bundle / "metadata"
            metadata.mkdir()
            inventory = {}
            for name in sorted({"provenance.json"} | publisher.MACOS_METADATA_NAMES):
                data = ("offline signing test sidecar: " + name + "\n").encode()
                (metadata / name).write_bytes(data)
                inventory[name] = {"sha256": hashlib.sha256(data).hexdigest(), "size": len(data)}
            (bundle / "metadata-inventory.json").write_bytes(publisher.encode_json(inventory))
            store = ReservationStore()
            publisher.reserve(bundle, store)
            self.assertIn("stable/releases/0.0.2/publication-manifest.json", store.objects)
            publisher.sign(bundle, secret, key)
            original = (bundle / "envelope.json").read_bytes()
            self.assertEqual(publisher.minisign_verify(original, key)["version"], "0.0.2")
            publisher.sign(bundle, secret, key)
            self.assertEqual((bundle / "envelope.json").read_bytes(), original)
            # A repeated signing attempt is deterministic: the exact trusted
            # comment and untrusted comment contain no implicit timestamp.
            (bundle / "envelope.json").unlink()
            publisher.sign(bundle, secret, key)
            self.assertEqual((bundle / "envelope.json").read_bytes(), original)
            envelope = json.loads(original)
            envelope["manifest"] = envelope["manifest"].replace("A test update.", "A fake update.")
            with self.assertRaisesRegex(publisher.PublishError, "verification failed"):
                publisher.minisign_verify(publisher.encode_json(envelope), key)


if __name__ == "__main__":
    unittest.main()
