"""Real offline four-signature bundles and architecture-isolated private reads."""
from copy import deepcopy
import os
import shutil
import unittest
from unittest.mock import patch

import test_release_bundle as existing
from release_helpers.context import ALL_PLATFORMS, OperatorError, TARGETS, sha
from test_publisher import FakeStore, publisher


ARM_MANIFEST = "windows-arm64-manifest.json"
ARM_ENVELOPE = "windows-arm64-envelope.json"
ARM_POINTER = "stable/windows-arm64/latest.json"


@unittest.skipUnless(shutil.which("minisign"), "run in the pinned root CI shell for cryptographic tests")
class ArmBundleTests(unittest.TestCase):
    advance = existing.BundleVerifierTests.advance
    r2 = existing.BundleVerifierTests.r2
    reservations = existing.BundleVerifierTests.reservations

    def setUp(self):
        self.enterContext(patch.dict(os.environ))

    def add_arm(self, ctx, bundle):
        primary = publisher.decode_json((bundle / "manifest.json").read_bytes())
        records = {}
        for name, data in (("agent-windows-arm64.tar.gz", b"ARM64 generation fixture"),
                           ("agent-windows-arm64.exe", b"ARM64 installer fixture")):
            (bundle / "payloads" / name).write_bytes(data)
            records[name] = {"path": publisher.public_path(primary["version"], name),
                             "size": len(data), "sha256": sha(data)}
        document = {**primary, "format": "maple-agent-update-v4",
                    "artifacts": {"windows-aarch64": records["agent-windows-arm64.tar.gz"]},
                    "downloads": {"windows-arm64": records["agent-windows-arm64.exe"]}}
        (bundle / ARM_MANIFEST).write_bytes(publisher.encode_json(document))
        for name in publisher.WINDOWS_ARM64_METADATA_NAMES:
            (bundle / "metadata" / name).write_bytes(b"ARM64 audit fixture " + name.encode())
        ctx.fixtures.index_metadata(bundle)

    def fixture(self, *, private_state=False, previous_arm=False):
        ctx = existing.BundleVerifierTests.fixture(self)
        self.add_arm(ctx, ctx.bundle)
        ctx.bindings["platforms"] = list(ALL_PLATFORMS)
        ctx.bindings["artifacts"]["windows-arm64"] = {
            "id": 4, "name": f"maple-agent-prod-{TARGETS['windows-arm64']}-{ctx.producer_run}",
            "size": 20, "zip_sha256": "8" * 64}
        trust = publisher.decode_json((ctx.bundle / "trust.json").read_bytes())
        trust["artifacts"]["windows-arm64"] = ctx.bindings["artifacts"]["windows-arm64"]["name"]
        (ctx.bundle / "trust.json").write_bytes(publisher.encode_json(trust))
        (ctx.bundle / "metadata/provenance.json").write_bytes(publisher.durable_provenance(trust))
        ctx.fixtures.index_metadata(ctx.bundle)
        ctx.bindings["prepared_files"] = {
            str(path.relative_to(ctx.bundle)): {"sha256": sha(path.read_bytes()), "size": path.stat().st_size}
            for path in ctx.bundle.rglob("*") if path.is_file()}
        self.assertEqual(len(ctx.bindings["prepared_files"]), 29)
        if private_state:
            self.private_state(ctx, previous_arm)
        return ctx

    def private_state(self, ctx, previous_arm):
        old, _, _ = ctx.fixtures.make_bundle(ctx.bindings["previous"]["version"], "previous-release", linux=True, windows=True)
        if previous_arm:
            self.add_arm(ctx, old)
        shutil.copyfile(ctx.bundle / "public-key.pub", old / "public-key.pub")
        for item in publisher.bundle_manifests(old):
            (old / item["envelope_name"]).unlink(missing_ok=True)
        publisher.reserve(old, FakeStore(), old / "public-key.pub")
        publisher.sign(old, ctx.secret, old / "public-key.pub")
        objects, baseline, refs = {}, {}, {}
        for index, item in enumerate(publisher.bundle_manifests(old)):
            raw = (old / item["envelope_name"]).read_bytes()
            immutable = publisher.release_key(ctx.bindings["previous"]["version"], item["name"])
            refs[immutable] = ctx.reference(f"old-envelope-{index}.json", raw)
            for kind, key in (("pointer", item["pointer"]), ("immutable", immutable)):
                etag = f'"opaque-{index}-{kind}-multipart"'
                objects[key] = (raw, etag)
                baseline[key] = {"sha256": sha(raw), "size": len(raw), "etag": etag}
        ctx.bindings.update(previous_private_r2=ctx.record("accepted-private.json", {"objects": baseline}),
                            previous_root_acceptance=ctx.record("accepted-root.json", {"accepted": True}),
                            previous_signed_envelopes=refs)
        ctx.route = ctx.root / "references-only-route.json"
        ctx.route.write_text('{"references_only":true}\n')
        ctx.route.chmod(0o600)
        ctx.route_sha256 = sha(ctx.route.read_bytes())
        ctx.endpoint, ctx.bucket = "https://offline.invalid", "offline-fixture"
        ctx.transport = existing.MemoryR2(objects)
        ctx.transport.install(self)

    def test_four_real_primary_signatures_follow_29_30_34_inventory_chain(self):
        ctx = self.fixture()
        publications, metadata, prepared, _ = self.advance(ctx, "prepared")
        previous = ctx.record("prepared-chain.json", prepared)
        publisher.reserve(ctx.bundle, ctx.store, ctx.bundle / "public-key.pub")
        _, _, reserved = ctx.verifier.verify_bundle(ctx.bindings, publisher, ctx.bundle, "reserved", previous)
        previous = ctx.record("reserved-chain.json", reserved)
        publisher.sign(ctx.bundle, ctx.secret, ctx.bundle / "public-key.pub")
        original_path = os.environ.get("PATH")
        with patch.object(publisher, "minisign_verify", wraps=publisher.minisign_verify) as verify:
            _, actual_metadata, signed = ctx.verifier.verify_bundle(ctx.bindings, publisher, ctx.bundle, "signed", previous)
        self.assertEqual(verify.call_count, 4)
        self.assertEqual(os.environ.get("PATH"), original_path)
        self.assertEqual([len(value["files"]) for value in (prepared, reserved, signed)], [29, 30, 34])
        self.assertEqual(set(signed["files"]) - set(reserved["files"]), {item["envelope_name"] for item in publications})
        self.assertEqual(signed["verified_primary_signatures"], sorted(item["envelope_name"] for item in publications))
        self.assertEqual(actual_metadata, metadata)

    def test_arm_signature_tampering_is_rejected_and_verifier_path_is_restored(self):
        ctx = self.fixture()
        _, _, _, previous = self.advance(ctx, "signed")
        envelope = publisher.decode_json((ctx.bundle / ARM_ENVELOPE).read_bytes())
        document = publisher.decode_json(envelope["manifest"].encode())
        document["notes"] += " changed after signing"
        envelope["manifest"] = publisher.encode_json(document).decode()
        (ctx.bundle / ARM_ENVELOPE).write_bytes(publisher.encode_json(envelope))
        original_path = os.environ.get("PATH")
        with self.assertRaisesRegex(publisher.PublishError, "verification failed"):
            ctx.verifier.verify_bundle(ctx.bindings, publisher, ctx.bundle, "signed", previous)
        self.assertEqual(os.environ.get("PATH"), original_path)

    def test_private_read_all_phases_has_exact_16_or_17_ordered_gets_and_own_etags(self):
        for previous_arm in (False, True):
            with self.subTest(previous_arm=previous_arm):
                ctx = self.fixture(private_state=True, previous_arm=previous_arm)
                publications, _, _, _ = self.advance(ctx, "prepared")
                prefix = f"stable/releases/{ctx.version}/"
                expected = ([item["pointer"] for item in publications]
                            + list(ctx.bindings["previous_signed_envelopes"])
                            + [prefix + item["reservation"] for item in publications]
                            + [prefix + "publication-metadata.json"] + [prefix + item["name"] for item in publications])
                count = 17 if previous_arm else 16
                previous = None
                for phase in existing.PHASES:
                    if phase == "before-sign":
                        publisher.reserve(ctx.bundle, ctx.store, ctx.bundle / "public-key.pub")
                        self.reservations(ctx)
                    elif phase == "before-publish":
                        publisher.sign(ctx.bundle, ctx.secret, ctx.bundle / "public-key.pub")
                    elif phase == "after-publish":
                        for item in publications:
                            raw = (ctx.bundle / item["envelope_name"]).read_bytes()
                            ctx.transport.objects[prefix + item["name"]] = (raw, '"immutable-' + item["name"] + '"')
                            ctx.transport.objects[item["pointer"]] = (raw, '"pointer-' + item["name"] + '"')
                    ctx.transport.calls.clear()
                    previous = self.r2(ctx, phase, previous)
                    receipt = publisher.decode_json(ctx.pinned(previous))
                    self.assertTrue(receipt["passed"])
                    self.assertEqual(receipt["request_count"], count)
                    self.assertEqual([item[0] for item in ctx.transport.calls], ["GET"] * count)
                    self.assertEqual([item[1] for item in ctx.transport.calls], expected)
                    self.assertEqual(set(receipt["objects"]), set(expected))
                    if not previous_arm and phase != "after-publish":
                        self.assertIsNone(receipt["objects"][ARM_POINTER])
                    for key, record in receipt["objects"].items():
                        self.assertEqual(record["etag"] if record else None,
                                         ctx.transport.objects[key][1] if key in ctx.transport.objects else None)

    def test_first_arm_pointer_conflicts_fail_at_every_prepublication_gate(self):
        for stop_at in ("before-reserve", "before-sign", "before-publish"):
            with self.subTest(stop_at=stop_at):
                ctx = self.fixture(private_state=True)
                self.advance(ctx, "prepared")
                previous = None
                for phase in existing.PHASES[:3]:
                    if phase == "before-sign":
                        publisher.reserve(ctx.bundle, ctx.store, ctx.bundle / "public-key.pub")
                        self.reservations(ctx)
                    if phase == stop_at:
                        ctx.transport.objects[ARM_POINTER] = deepcopy(ctx.transport.objects["stable/windows/latest.json"])
                        ctx.transport.calls.clear()
                        with self.assertRaisesRegex(OperatorError, "R2 state differs"):
                            self.r2(ctx, phase, previous)
                        self.assertFalse(ctx.emitted[-1][1]["passed"])
                        self.assertEqual(ctx.emitted[-1][1]["request_count"], 16)
                        self.assertTrue(all(item[0] == "GET" for item in ctx.transport.calls))
                        break
                    previous = self.r2(ctx, phase, previous)

    def test_arm_predecessor_and_reservation_each_keep_their_own_opaque_etag(self):
        for kind in ("pointer", "immutable", "reservation"):
            with self.subTest(kind=kind):
                ctx = self.fixture(private_state=True, previous_arm=True)
                self.advance(ctx, "prepared")
                before = self.r2(ctx, "before-reserve")
                publisher.reserve(ctx.bundle, ctx.store, ctx.bundle / "public-key.pub")
                self.reservations(ctx)
                prior = self.r2(ctx, "before-sign", before)
                key = {"pointer": ARM_POINTER,
                       "immutable": publisher.release_key(ctx.bindings["previous"]["version"], ARM_MANIFEST),
                       "reservation": publisher.release_key(ctx.version, "publication-" + ARM_MANIFEST)}[kind]
                raw, _ = ctx.transport.objects[key]
                ctx.transport.objects[key] = (raw, ctx.transport.objects["stable/windows/latest.json"][1])
                with self.assertRaisesRegex(OperatorError, "R2 state differs"):
                    self.r2(ctx, "before-publish", prior)
                self.assertFalse(ctx.emitted[-1][1]["passed"])


if __name__ == "__main__":
    unittest.main()
