"""Public delivery contracts for the first and subsequent native ARM releases."""

import base64
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch
from urllib.parse import urlsplit

from release_helpers.public import PublicVerifier
from test_release_public import PublicFixture, RecordedTransport, publisher


class ArmTransport(RecordedTransport):
    def open(self, request, timeout):
        if urlsplit(request.full_url).path == "/download/agent/stable/windows-arm64":
            self.calls.append({"url": request.full_url, "method": request.get_method(),
                               "headers": {k.lower(): v for k, v in request.header_items()}, "timeout": timeout})
            location = self.fixture.current["windows-arm64"]["manifest"]["downloads"]["windows-arm64"]["path"]
            return self.alias(request.full_url, location)
        return super().open(request, timeout)


class ArmPublicFixture(PublicFixture):
    def __init__(self, root, *, previous_arm=False):
        super().__init__(root)
        self.add_arm("current", self.current, self.ctx.version, self.ctx.build, self.ctx.source)
        if previous_arm:
            self.add_arm("previous", self.old, "7.8.9", "102", "a" * 40)
        platforms = [*PublicVerifier.PLATFORMS, "windows-arm64"]
        self.ctx.bindings["platforms"] = platforms
        prepared = root / "current-prepared"
        self.freeze["platforms"] = platforms
        self.freeze["files"] = {str(p.relative_to(prepared)): {"sha256": self.ctx.digest(p.read_bytes()), "size": p.stat().st_size}
                                for p in prepared.rglob("*") if p.is_file()}
        self.repin("freeze", lambda _: self.freeze)
        self.repin("signed_stage_proof", lambda proof: {**proof,
                   "verified_primary_signatures": sorted(r["envelope_name"] for r in self.current.values()),
                   "files": {r["envelope_name"]: {"sha256": self.ctx.digest(r["envelope"])} for r in self.current.values()}})
        self.ctx.bindings["envelope_sha256"] = {p: self.ctx.digest(r["envelope"]) for p, r in self.current.items()}
        self.ctx.bindings["previous_signed_envelopes"] = {
            f"stable/releases/7.8.9/{r['name']}": self.ctx.pin_bytes("previous-bound-" + r["envelope_name"], r["envelope"])
            for r in self.old.values()}
        hashes = {p: self.ctx.digest(r["envelope"]) for p, r in self.old.items()}
        def baseline_update(value):
            value["envelope_sha256"] = hashes
            if previous_arm:
                old = self.old["windows-arm64"]
                for host in PublicVerifier.HOSTS:
                    url = f"https://{host}/agent/stable/releases/7.8.9/{old['name']}"
                    value["requests"].append({"url": url, "method": "GET", "check_passed": True,
                                               "body_sha256": self.ctx.digest(old["envelope"]),
                                               "headers": {"etag": self.etag(url)}})
            return value
        self.repin("previous_public", baseline_update)
        self.repin("previous_root_acceptance", lambda value: {**value, "envelopes": hashes,
                   "public_receipt_sha256": self.ctx.bindings["previous_public"]["sha256"]})
        self.transport = ArmTransport(self)

    def repin(self, name, transform):
        reference = self.ctx.bindings[name]
        path = Path(reference["path"])
        path.write_bytes(publisher.encode_json(transform(json.loads(path.read_bytes()))))
        reference["sha256"] = self.ctx.digest(path.read_bytes())

    def add_arm(self, label, records, version, build, source):
        prepared, signed = self.ctx.root / (label + "-prepared"), self.ctx.root / (label + "-signed")
        payloads, artifacts = {}, []
        for i, suffix in enumerate((".tar.gz", "-setup.exe")):
            name = f"maple-agent-prod-{version}-{source[:7]}-windows-aarch64{suffix}"
            data = (b"native-arm64" + bytes([i])) * 350
            path = publisher.public_path(version, name)
            (prepared / "payloads" / name).write_bytes(data)
            payloads[path] = data
            artifacts.append({"path": path, "size": len(data), "sha256": self.ctx.digest(data)})
        manifest = {"format": "maple-agent-update-v4", "product": "maple-agent", "channel": "stable",
                    "version": version, "build": build, "source_sha": source, "pub_date": "2026-01-02T03:04:05Z",
                    "notes": records["macos"]["manifest"]["notes"], "artifacts": {"windows-aarch64": artifacts[0]},
                    "downloads": {"windows-arm64": artifacts[1]}}
        inner = publisher.encode_json(manifest)
        signature = "\n".join(("untrusted comment: structural fixture", base64.b64encode(b"ED" + bytes(72)).decode(),
                              "trusted comment: " + publisher.trusted_comment(version), base64.b64encode(bytes(64)).decode(), ""))
        envelope = publisher.encode_json({"manifest": inner.decode(), "signature": signature})
        name, envelope_name = "windows-arm64-manifest.json", "windows-arm64-envelope.json"
        (prepared / name).write_bytes(inner)
        (signed / envelope_name).write_bytes(envelope)
        records["windows-arm64"] = {"manifest": manifest, "envelope": envelope, "name": name,
                                      "envelope_name": envelope_name, "payloads": payloads}
        self.envelope_urls[f"/agent/stable/releases/{version}/{name}"] = envelope
        if label == "current":
            self.envelope_urls["/agent/stable/windows-arm64/latest.json"] = envelope
            self.payloads.update(payloads)


class ArmPublicVerifierTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()

    def setup_fixture(self, *, previous_arm=False):
        self.fixture = ArmPublicFixture(self.root, previous_arm=previous_arm)
        self.verifier = PublicVerifier(self.fixture.ctx)
        self.verifier.opener = self.fixture.transport

    def live(self):
        with patch("release_helpers.public.subprocess.run", return_value=SimpleNamespace(returncode=0)):
            return self.verifier.run(SimpleNamespace(execute_live=True, output=self.root / "public"))

    def receipt(self):
        return json.loads((self.root / "public" / "receipt.json").read_bytes())

    def test_first_arm_release_preserves_all_existing_platforms_and_checks_fourth(self):
        self.setup_fixture()
        preview = self.verifier.run(SimpleNamespace(execute_live=False, output=None))
        self.assertEqual((preview["fixed_requests"], preview["maximum_requests"], preview["full_payload_streams"]), (125, 132, 14))
        self.assertEqual(self.fixture.transport.calls, [])
        self.assertTrue(self.live()["all_checks_passed"])
        receipt = self.receipt()
        self.assertEqual((receipt["fixed_request_count"], receipt["full_payload_streams"]), (125, 14))
        self.assertEqual(receipt["preserved_previous_immutables"], list(PublicVerifier.PLATFORMS))
        self.assertEqual(receipt["included_platforms"], [*PublicVerifier.PLATFORMS, "windows-arm64"])
        arm = [r for r in self.fixture.transport.calls if "windows-arm64" in r["url"] or "windows-aarch64" in r["url"]]
        self.assertEqual(len(arm), 28)
        self.assertTrue(all(r["headers"]["x-maple-download-probe"] == "1" for r in arm))
        self.assertEqual(receipt["duplicate_full_payload_files_saved"], 0)

    def test_subsequent_arm_release_preserves_previous_arm_bytes_and_own_etag(self):
        self.setup_fixture(previous_arm=True)
        url = "https://updates.maple.ai/agent/stable/releases/7.8.9/windows-arm64-manifest.json"
        self.fixture.transport.envelope_etag_override[url] = self.fixture.etag(url.replace("updates.maple.ai", "updates.trymaple.ai"))
        self.assertFalse(self.live()["all_checks_passed"])
        receipt = self.receipt()
        self.assertEqual(receipt["fixed_request_count"], 127)
        self.assertEqual(receipt["full_payload_streams"], 14)
        failure = next(r for r in receipt["requests"] if r["url"] == url)
        self.assertIn("own ETag", failure["check_error"])

    def test_subsequent_arm_release_preserves_all_four_previous_envelopes(self):
        self.setup_fixture(previous_arm=True)
        self.assertTrue(self.live()["all_checks_passed"])
        receipt = self.receipt()
        self.assertEqual(receipt["fixed_request_count"], 127)
        self.assertEqual(receipt["preserved_previous_immutables"], [*PublicVerifier.PLATFORMS, "windows-arm64"])

    def test_arm_signature_cannot_be_omitted(self):
        self.setup_fixture()
        self.fixture.repin("signed_stage_proof", lambda proof: {**proof,
                           "verified_primary_signatures": [name for name in proof["verified_primary_signatures"]
                                                           if name != "windows-arm64-envelope.json"]})
        with self.assertRaisesRegex(ValueError, "signed-stage proof"):
            self.live()
        self.assertEqual(self.fixture.transport.calls, [])

    def test_signature_proof_requires_canonical_sorted_inventory(self):
        self.setup_fixture()
        self.fixture.repin("signed_stage_proof", lambda proof: {**proof,
                           "verified_primary_signatures": [self.verifier.MANIFEST_NAMES[p].replace("manifest", "envelope")
                                                           for p in self.verifier.platforms]})
        with self.assertRaisesRegex(ValueError, "signed-stage proof"):
            self.live()
        self.assertEqual(self.fixture.transport.calls, [])

    def test_missing_selected_platform_inventory_has_no_legacy_fallback(self):
        self.setup_fixture()
        del self.fixture.ctx.bindings["platforms"]
        with self.assertRaisesRegex(ValueError, "complete selected platform"):
            PublicVerifier(self.fixture.ctx)

    def test_previous_arm_cannot_be_omitted_from_signed_reference_inventory(self):
        self.setup_fixture(previous_arm=True)
        del self.fixture.ctx.bindings["previous_signed_envelopes"]["stable/releases/7.8.9/windows-arm64-manifest.json"]
        self.verifier = PublicVerifier(self.fixture.ctx)
        self.verifier.opener = self.fixture.transport
        with self.assertRaisesRegex(ValueError, "previous platform inventory"):
            self.live()
        self.assertEqual(self.fixture.transport.calls, [])

    def test_unsupported_or_dropped_platform_is_rejected(self):
        self.setup_fixture(previous_arm=True)
        self.fixture.ctx.bindings["platforms"] = list(PublicVerifier.PLATFORMS)
        with self.assertRaisesRegex(ValueError, "may not be dropped"):
            PublicVerifier(self.fixture.ctx)
        self.fixture.ctx.bindings["platforms"] = [*PublicVerifier.PLATFORMS, "other"]
        with self.assertRaisesRegex(ValueError, "complete selected platform"):
            PublicVerifier(self.fixture.ctx)


if __name__ == "__main__":
    unittest.main()
