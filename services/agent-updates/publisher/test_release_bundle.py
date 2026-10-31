"""Real offline bundle signatures and fixed, in-memory private-state reads."""

from copy import deepcopy
import os
from pathlib import Path
import shutil
import subprocess
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from release_helpers import bundle as bundle_module
from release_helpers.bundle import BundleVerifier, NAMES, PHASES
from release_helpers.context import OperatorError, TARGETS, PLATFORMS, sha
import test_publisher as publisher_fixtures
from test_publisher import FakeStore, publisher


class BundleContext:
    def __init__(self, root):
        self.root = root
        self.core = publisher
        self.bindings_sha256 = "b" * 64
        self.tooling_sha256 = "c" * 64
        self.policy_sha256 = "d" * 64
        self.core_sha256 = sha(Path(publisher.__file__).read_bytes())
        self.emitted = []

    def reference(self, name, data):
        path = self.root / name
        path.write_bytes(data)
        return {"path": str(path), "sha256": sha(data)}

    def record(self, name, value):
        return self.reference(name, publisher.encode_json(value))

    def pinned(self, reference):
        data = Path(reference["path"]).read_bytes()
        if sha(data) != reference["sha256"]:
            raise OperatorError("pinned test input changed")
        return data

    def tool(self, name):
        if name != "minisign":
            raise AssertionError("unexpected tool role")
        self.pinned(self.minisign)
        return self.minisign["path"]

    def emit(self, path, value):
        with Path(path).open("xb") as output:
            output.write(publisher.encode_json(value))
        self.emitted.append((Path(path), deepcopy(value)))


class MemoryR2:
    """Replace only store construction and wire transport; real get() stays active."""

    def __init__(self, objects):
        self.objects, self.calls = objects, []
        self.failure_at = None

    def open(self, request, timeout):
        self.calls.append((request.method, request.key, request.maximum, timeout))
        if self.failure_at == len(self.calls):
            raise publisher.PublishError("offline injected transport failure")
        if request.method != "GET" or not 0 < timeout <= 20:
            raise AssertionError("unexpected private-state transport operation")
        if request.key not in self.objects:
            raise publisher.StoreError(404)
        data, etag = self.objects[request.key]
        publisher.need(len(data) <= request.maximum, "oversized offline metadata")
        return data, {"ETag": etag}

    def install(self, test):
        transport = self

        def initialize(store, endpoint, bucket, access_key=None, secret_key=None):
            if access_key is not None or secret_key is not None:
                raise AssertionError("test must not supply credentials")
            store.endpoint, store.bucket, store.opener = endpoint, bucket, transport

        def request(store, method, key, data=None, content_hash=None, size=None,
                    condition=None, content_type=None, maximum=publisher.MAX_JSON, consumer=None):
            if any(value is not None for value in (data, content_hash, size, condition, content_type, consumer)):
                raise AssertionError("unexpected private-state request payload")
            return store.opener.open(SimpleNamespace(method=method, key=key, maximum=maximum), timeout=20)

        test.enterContext(patch.object(publisher.R2Store, "__init__", initialize))
        test.enterContext(patch.object(publisher.R2Store, "request", request))


@unittest.skipUnless(shutil.which("minisign"), "run in the pinned root CI shell for cryptographic tests")
class BundleVerifierTests(unittest.TestCase):
    def setUp(self):
        # A failing assertion must still leave the test process environment intact.
        self.enterContext(patch.dict(os.environ))

    def fixture(self, identity_number=1, private_state=False):
        fixtures = publisher_fixtures.PublicationTests()
        fixtures.setUp()
        self.addCleanup(fixtures.doCleanups)
        fixtures.root = fixtures.root.resolve()
        ctx = BundleContext(fixtures.root)
        ctx.version, old_version = (("2.4.6", "2.4.5") if identity_number == 1 else ("3.8.12", "3.8.11"))
        ctx.source, ctx.build = str(identity_number) * 40, str(700 + identity_number)
        ctx.producer_run, ctx.producer_attempt = 1000 + identity_number, identity_number
        ctx.publication = {"run_id": 2000 + identity_number, "run_attempt": identity_number,
                           "source_sha": str(identity_number + 2) * 40,
                           "workflow": ".github/workflows/agent-updates-publish.yml"}
        bundle, _, _ = fixtures.make_bundle(ctx.version, "three-platform", linux=True, windows=True)
        for name in NAMES:
            document = publisher.decode_json((bundle / name).read_bytes())
            document.update(source_sha=ctx.source, build=ctx.build)
            (bundle / name).write_bytes(publisher.encode_json(document))
        publications = publisher.bundle_manifests(bundle)
        for publication in publications:
            (bundle / publication["envelope_name"]).unlink()
        key, secret = bundle / "public-key.pub", ctx.root / "disposable-test-secret.key"
        key.unlink()
        result = subprocess.run(["minisign", "-G", "-W", "-p", str(key), "-s", str(secret)],
                                stdin=subprocess.DEVNULL, capture_output=True, timeout=30)
        self.assertEqual(result.returncode, 0, "disposable offline key generation failed")
        ctx.primary_sha256 = sha((publisher.public_key_text(key) + "\n").encode())
        executable = Path(shutil.which("minisign")).resolve()
        ctx.minisign = {"path": str(executable), "sha256": sha(executable.read_bytes())}
        manifest = publications[0]["manifest"]
        identity = {name: manifest[name] for name in ("version", "build", "source_sha", "pub_date")}
        identity.update(run_id=ctx.producer_run, run_attempt=ctx.producer_attempt)
        artifact_names = {platform: f"maple-agent-prod-{TARGETS[platform]}-{ctx.producer_run}" for platform in PLATFORMS}
        trust = {name: identity[name] for name in ("run_id", "run_attempt", "source_sha", "build", "pub_date")}
        trust["artifacts"] = artifact_names
        (bundle / "trust.json").write_bytes(publisher.encode_json(trust))
        (bundle / "metadata/provenance.json").write_bytes(publisher.durable_provenance(trust))
        fixtures.index_metadata(bundle)
        selected = dict(ctx.publication, payload_source=ctx.source, producer_run=ctx.producer_run,
                        producer_attempt=ctx.producer_attempt, version=ctx.version)
        ctx.bindings = {"schema": "agent-release-bindings-v1", "producer_identity": identity, "platforms": list(PLATFORMS),
                        "publication_identity": ctx.publication, "public_key": str(key),
                        "previous": {"version": old_version},
                        "artifacts": {platform: {"id": index + 1, "name": name, "size": index + 10,
                                                 "zip_sha256": str(index + 4) * 64}
                                      for index, (platform, name) in enumerate(artifact_names.items())},
                        "notes": ctx.reference("notes.txt", manifest["notes"].encode()),
                        "qualification": ctx.record("qualified.json", {"passed": True}),
                        "freeze": ctx.record("freeze.json", {"schema": "agent-release-prepared-freeze-v1"}),
                        "selected_publication": ctx.record("selected.json", selected),
                        "prepared_files": {str(path.relative_to(bundle)): {"sha256": sha(path.read_bytes()), "size": path.stat().st_size}
                                           for path in bundle.rglob("*") if path.is_file()}}
        self.assertEqual(len(ctx.bindings["prepared_files"]), 23)
        ctx.bundle, ctx.secret, ctx.fixtures, ctx.store = bundle, secret, fixtures, FakeStore()
        ctx.verifier = BundleVerifier(ctx)
        if private_state:
            self.private_state(ctx)
        return ctx

    def advance(self, ctx, stage):
        previous = None
        proof = None
        for current in ("prepared", "reserved", "signed"):
            if current == "reserved":
                publisher.reserve(ctx.bundle, ctx.store, ctx.bundle / "public-key.pub")
            elif current == "signed":
                publisher.sign(ctx.bundle, ctx.secret, ctx.bundle / "public-key.pub")
            publications, metadata, proof = ctx.verifier.verify_bundle(ctx.bindings, publisher, ctx.bundle, current, previous)
            if current == stage:
                return publications, metadata, proof, previous
            previous = ctx.record(current + "-proof.json", proof)
        raise AssertionError("invalid fixture stage")

    def private_state(self, ctx):
        old, _, _ = ctx.fixtures.make_bundle(ctx.bindings["previous"]["version"], "previous-release", linux=True, windows=True)
        shutil.copyfile(ctx.bundle / "public-key.pub", old / "public-key.pub")
        for publication in publisher.bundle_manifests(old):
            (old / publication["envelope_name"]).unlink()
        publisher.reserve(old, FakeStore(), old / "public-key.pub")
        publisher.sign(old, ctx.secret, old / "public-key.pub")
        objects, baseline, references = {}, {}, {}
        for index, publication in enumerate(publisher.bundle_manifests(old)):
            data = (old / publication["envelope_name"]).read_bytes()
            immutable = publisher.release_key(ctx.bindings["previous"]["version"], publication["name"])
            references[immutable] = ctx.reference(f"old-envelope-{index}.json", data)
            for suffix, key in (("pointer", publication["pointer"]), ("immutable", immutable)):
                etag = f'"opaque-{index}-{suffix}-multipart"'
                objects[key] = (data, etag)
                baseline[key] = {"sha256": sha(data), "size": len(data), "etag": etag}
        ctx.bindings.update(previous_private_r2=ctx.record("accepted-private.json", {"objects": baseline}),
                            previous_root_acceptance=ctx.record("accepted-root.json", {"accepted": True}),
                            previous_signed_envelopes=references)
        ctx.route = ctx.root / "references-only-route.json"
        ctx.route.write_text('{"references_only":true}\n')
        ctx.route.chmod(0o600)
        ctx.route_sha256 = sha(ctx.route.read_bytes())
        ctx.endpoint, ctx.bucket = "https://offline.invalid", "offline-fixture"
        ctx.transport = MemoryR2(objects)
        ctx.transport.install(self)

    def r2(self, ctx, phase, previous=None):
        publications = publisher.bundle_manifests(ctx.bundle)
        metadata = publisher.bundle_metadata(ctx.bundle, publications[0]["manifest"])[0]
        output = ctx.root / (phase + f"-{len(ctx.emitted)}.json")
        ctx.verifier.r2_read(ctx.bindings, publisher, ctx.bundle, publications, metadata, phase, previous, output)
        return {"path": str(output), "sha256": sha(output.read_bytes())}

    def reservations(self, ctx):
        for key, data in ctx.store.objects.items():
            ctx.transport.objects[key] = (data, '"reservation-' + key.rsplit("/", 1)[-1] + '"')

    def test_two_release_identities_progress_from_23_to_24_to_27_files_and_verify_all_three_signatures(self):
        for identity_number in (1, 2):
            with self.subTest(identity=identity_number):
                ctx = self.fixture(identity_number)
                self.assertEqual(ctx.verifier.setup(), (ctx.bindings, publisher))
                publications, metadata, prepared, _ = self.advance(ctx, "prepared")
                prepared_reference = ctx.record("prepared-chain.json", prepared)
                publisher.reserve(ctx.bundle, ctx.store, ctx.bundle / "public-key.pub")
                _, _, reserved = ctx.verifier.verify_bundle(ctx.bindings, publisher, ctx.bundle, "reserved", prepared_reference)
                reserved_reference = ctx.record("reserved-chain.json", reserved)
                publisher.sign(ctx.bundle, ctx.secret, ctx.bundle / "public-key.pub")
                original_path = os.environ.get("PATH")
                with patch.object(publisher, "minisign_verify", wraps=publisher.minisign_verify) as verify:
                    _, actual_metadata, signed = ctx.verifier.verify_bundle(ctx.bindings, publisher, ctx.bundle, "signed", reserved_reference)
                self.assertEqual(os.environ.get("PATH"), original_path)
                self.assertEqual(verify.call_count, 3)
                self.assertTrue(all(call.args[1] == str(ctx.bundle / "public-key.pub") for call in verify.call_args_list))
                self.assertEqual([len(value["files"]) for value in (prepared, reserved, signed)], [23, 24, 27])
                self.assertEqual(set(reserved["files"]) - set(prepared["files"]), {"reservation.json"})
                self.assertEqual(set(signed["files"]) - set(reserved["files"]), {item["envelope_name"] for item in publications})
                self.assertEqual(signed["verified_primary_signatures"], sorted(item["envelope_name"] for item in publications))
                self.assertEqual(signed["producer"], ctx.bindings["producer_identity"])
                self.assertEqual(signed["publication"], ctx.publication)
                self.assertEqual(actual_metadata, metadata)

    def test_each_stage_rejects_each_platform_payload_tampering_extra_files_and_symlinks(self):
        for stage in ("prepared", "reserved", "signed"):
            ctx = self.fixture()
            publications, _, _, previous = self.advance(ctx, stage)
            cases = [(platform, next(iter(publication["manifest"]["artifacts"].values()))["path"].rsplit("/", 1)[-1])
                     for platform, publication in zip(ctx.bindings["artifacts"], publications)]
            for case, payload in cases + [("extra", None), ("symlink", None)]:
                with self.subTest(stage=stage, corruption=case):
                    clone = ctx.root / f"{stage}-{case}"
                    shutil.copytree(ctx.bundle, clone)
                    if payload:
                        (clone / "payloads" / payload).write_bytes(b"tampered platform payload")
                    elif case == "extra":
                        (clone / "unexpected.txt").write_text("unreviewed")
                    else:
                        (clone / "unreviewed-link").symlink_to(ctx.bundle / "manifest.json")
                    with self.assertRaises((OperatorError, publisher.PublishError)):
                        ctx.verifier.verify_bundle(ctx.bindings, publisher, clone, stage, previous)

    def test_wrong_notes_and_provenance_are_rejected_even_when_metadata_inventory_is_consistent(self):
        for corruption in ("notes", "provenance"):
            with self.subTest(corruption=corruption):
                ctx = self.fixture()
                if corruption == "notes":
                    ctx.bindings["notes"] = ctx.reference("different-notes.txt", b"different approved notes")
                else:
                    provenance = publisher.decode_json((ctx.bundle / "metadata/provenance.json").read_bytes())
                    provenance["source_sha"] = "0" * 40
                    (ctx.bundle / "metadata/provenance.json").write_bytes(publisher.encode_json(provenance))
                    ctx.fixtures.index_metadata(ctx.bundle)
                with self.assertRaisesRegex(OperatorError, "notes differ|provenance differs"):
                    ctx.verifier.verify_bundle(ctx.bindings, publisher, ctx.bundle, "prepared", None)

    def test_each_primary_signature_is_cryptographically_required_and_path_is_restored_after_failure(self):
        ctx = self.fixture()
        publications, _, _, previous = self.advance(ctx, "signed")
        for publication in publications:
            with self.subTest(envelope=publication["envelope_name"]):
                clone = ctx.root / ("tampered-" + publication["envelope_name"])
                shutil.copytree(ctx.bundle, clone)
                envelope_path = clone / publication["envelope_name"]
                envelope = publisher.decode_json(envelope_path.read_bytes())
                manifest = publisher.decode_json(envelope["manifest"].encode())
                manifest["notes"] += " changed after signing"
                envelope["manifest"] = publisher.encode_json(manifest).decode()
                envelope_path.write_bytes(publisher.encode_json(envelope))
                original_path = os.environ.get("PATH")
                with self.assertRaisesRegex(publisher.PublishError, "verification failed"):
                    ctx.verifier.verify_bundle(ctx.bindings, publisher, clone, "signed", previous)
                self.assertEqual(os.environ.get("PATH"), original_path)

    def test_predecessor_schema_identity_phase_and_exact_inventory_delta_are_required(self):
        ctx = self.fixture()
        _, _, _, previous = self.advance(ctx, "reserved")
        for case in ("missing", "schema", "bindings_sha256", "tooling_sha256", "policy_sha256", "stage", "delta", "changed"):
            with self.subTest(case=case):
                selected = previous
                if case == "missing":
                    selected = None
                else:
                    prior = publisher.decode_json(ctx.pinned(previous))
                    if case == "delta":
                        prior["files"].pop("manifest.json")
                    elif case == "changed":
                        prior["files"]["manifest.json"]["sha256"] = "0" * 64
                    else:
                        prior[case] = "wrong"
                    selected = ctx.record("bad-predecessor-" + case + ".json", prior)
                with self.assertRaises(OperatorError):
                    ctx.verifier.verify_bundle(ctx.bindings, publisher, ctx.bundle, "reserved", selected)

    def test_r2_all_phases_use_exactly_13_ordered_gets_and_preserve_each_objects_opaque_etag(self):
        for identity_number in (1, 2):
            with self.subTest(identity=identity_number):
                ctx = self.fixture(identity_number, private_state=True)
                publications, metadata, _, _ = self.advance(ctx, "prepared")
                prefix = f"stable/releases/{ctx.version}/"
                expected_keys = ([item["pointer"] for item in publications]
                                 + [publisher.release_key(ctx.bindings["previous"]["version"], name) for name in NAMES]
                                 + [prefix + item["reservation"] for item in publications]
                                 + [prefix + "publication-metadata.json"] + [prefix + item["name"] for item in publications])
                previous = None
                for phase in PHASES:
                    if phase == "before-sign":
                        publisher.reserve(ctx.bundle, ctx.store, ctx.bundle / "public-key.pub")
                        self.reservations(ctx)
                    elif phase == "before-publish":
                        publisher.sign(ctx.bundle, ctx.secret, ctx.bundle / "public-key.pub")
                    elif phase == "after-publish":
                        for item in publications:
                            data = (ctx.bundle / item["envelope_name"]).read_bytes()
                            ctx.transport.objects[prefix + item["name"]] = (data, '"new-immutable-' + item["name"] + '"')
                            ctx.transport.objects[item["pointer"]] = (data, '"new-pointer-' + item["name"] + '"')
                    ctx.transport.calls.clear()
                    previous = self.r2(ctx, phase, previous)
                    receipt = publisher.decode_json(ctx.pinned(previous))
                    self.assertTrue(receipt["passed"])
                    self.assertEqual(receipt["schema"], "agent-release-r2-proof-v1")
                    self.assertEqual(receipt["request_count"], 13)
                    self.assertEqual(list(receipt["objects"]), sorted(expected_keys))
                    self.assertEqual([row[0] for row in ctx.transport.calls], ["GET"] * 13)
                    self.assertEqual([row[1] for row in ctx.transport.calls], expected_keys)
                    for key, record in receipt["objects"].items():
                        self.assertEqual(record["etag"] if record else None,
                                         ctx.transport.objects[key][1] if key in ctx.transport.objects else None)

    def test_r2_baseline_bytes_and_each_opaque_etag_are_checked(self):
        for case in ("old-bytes", "old-own-etag", "pointer-own-etag", "reservation-own-etag",
                     "reservation-bytes", "unexpected-complete", "invalid-etag"):
            with self.subTest(case=case):
                ctx = self.fixture(private_state=True)
                self.advance(ctx, "prepared")
                before = self.r2(ctx, "before-reserve")
                publisher.reserve(ctx.bundle, ctx.store, ctx.bundle / "public-key.pub")
                self.reservations(ctx)
                prior = self.r2(ctx, "before-sign", before)
                publications = publisher.bundle_manifests(ctx.bundle)
                old = publisher.release_key(ctx.bindings["previous"]["version"], publications[0]["name"])
                pointer = publications[0]["pointer"]
                reservation = publisher.release_key(ctx.version, publications[0]["reservation"])
                key = reservation if case.startswith("reservation-") else pointer if case == "pointer-own-etag" else old
                data, etag = ctx.transport.objects[key]
                if case in {"old-bytes", "reservation-bytes"}:
                    data += b"tamper"
                elif case == "unexpected-complete":
                    key = publisher.release_key(ctx.version, publications[0]["name"])
                    data, etag = b"unexpected complete document before publication", '"unexpected"'
                elif case == "invalid-etag":
                    etag = '"invalid\nopaque"'
                else:
                    other = old if key == pointer else pointer
                    etag = ctx.transport.objects[other][1]
                ctx.transport.objects[key] = (data, etag)
                ctx.transport.calls.clear()
                with self.assertRaises(OperatorError):
                    self.r2(ctx, "before-publish", prior)
                receipt = ctx.emitted[-1][1]
                self.assertFalse(receipt["passed"])
                self.assertLessEqual(receipt["request_count"], 13)
                self.assertTrue(all(call[0] == "GET" for call in ctx.transport.calls))

    def test_r2_phase_chain_start_and_predecessor_pins_fail_before_any_get(self):
        ctx = self.fixture(private_state=True)
        self.advance(ctx, "prepared")
        before = self.r2(ctx, "before-reserve")
        for case in ("unexpected-start", "missing", "phase", "schema", "bindings_sha256", "tooling_sha256", "policy_sha256", "hash"):
            with self.subTest(case=case):
                selected, phase = before, "before-sign"
                if case == "unexpected-start":
                    phase = "before-reserve"
                elif case == "missing":
                    selected = None
                elif case == "hash":
                    selected = dict(before, sha256="0" * 64)
                else:
                    receipt = publisher.decode_json(ctx.pinned(before))
                    receipt[case] = "wrong"
                    selected = ctx.record("wrong-r2-" + case + ".json", receipt)
                ctx.transport.calls.clear()
                with self.assertRaises(OperatorError):
                    self.r2(ctx, phase, selected)
                self.assertEqual(ctx.transport.calls, [])

    def test_private_route_and_accepted_predecessor_bytes_are_pinned_before_gets(self):
        for case in ("route-bytes", "route-mode", "acceptance", "old-envelope"):
            with self.subTest(case=case):
                ctx = self.fixture(private_state=True)
                self.advance(ctx, "prepared")
                if case == "route-bytes":
                    ctx.route.write_text('{"different":true}\n')
                elif case == "route-mode":
                    ctx.route.chmod(0o644)
                elif case == "acceptance":
                    Path(ctx.bindings["previous_root_acceptance"]["path"]).write_text("changed")
                else:
                    reference = next(iter(ctx.bindings["previous_signed_envelopes"].values()))
                    Path(reference["path"]).write_text("changed")
                with self.assertRaises(OperatorError):
                    self.r2(ctx, "before-reserve")
                self.assertEqual(ctx.transport.calls, [])

    def test_private_read_rejects_methods_order_payloads_and_bounds_before_transport(self):
        ctx = self.fixture(private_state=True)
        self.advance(ctx, "prepared")
        publications = publisher.bundle_manifests(ctx.bundle)
        for case in ("POST", "wrong-key", "wrong-order", "payload", "consumer", "maximum"):
            with self.subTest(case=case):
                def hostile_get(store, key, maximum):
                    method, values = "GET", {"maximum": maximum}
                    if case == "POST":
                        method = "POST"
                    elif case == "wrong-key":
                        key += "-unreviewed"
                    elif case == "wrong-order":
                        key = publications[1]["pointer"]
                    elif case == "payload":
                        values["data"] = b"unreviewed body"
                    elif case == "consumer":
                        values["consumer"] = lambda response: response
                    else:
                        values["maximum"] = max(publisher.MAX_ENVELOPE, publisher.MAX_MANIFEST) + 1
                    return store.request(method, key, **values)

                ctx.transport.calls.clear()
                with patch.object(publisher.R2Store, "get", hostile_get), self.assertRaises(OperatorError):
                    self.r2(ctx, "before-reserve")
                self.assertEqual(ctx.transport.calls, [])
                self.assertEqual(ctx.emitted[-1][1]["request_count"], 0)
                self.assertFalse(ctx.emitted[-1][1]["passed"])

    def test_private_read_restores_timer_and_handler_after_success_or_failure_with_elapsed_time(self):
        for failure in (False, True):
            with self.subTest(failure=failure):
                ctx = self.fixture(private_state=True)
                self.advance(ctx, "prepared")
                if failure:
                    ctx.transport.failure_at = 4
                previous_handler, state = object(), {"restoring": False}

                def handler_changed(signum, selected):
                    if selected is previous_handler:
                        state["restoring"] = True
                    return previous_handler

                with patch.object(bundle_module.time, "monotonic", side_effect=lambda: 105.0 if state["restoring"] else 100.0), \
                        patch.object(bundle_module.signal, "signal", side_effect=handler_changed) as handler, \
                        patch.object(bundle_module.signal, "setitimer", side_effect=[(30.0, 5.0), (0.0, 0.0), (0.0, 0.0)]) as timer:
                    if failure:
                        with self.assertRaisesRegex(publisher.PublishError, "offline injected"):
                            self.r2(ctx, "before-reserve")
                    else:
                        self.r2(ctx, "before-reserve")
                self.assertEqual(timer.call_args_list[-1].args, (bundle_module.signal.ITIMER_REAL, 25.0, 5.0))
                self.assertEqual(handler.call_args_list[-1].args, (bundle_module.signal.SIGALRM, previous_handler))

    def test_r2_transport_failure_retains_partial_failed_receipt(self):
        ctx = self.fixture(private_state=True)
        self.advance(ctx, "prepared")
        ctx.transport.failure_at = 4
        with self.assertRaisesRegex(publisher.PublishError, "offline injected"):
            self.r2(ctx, "before-reserve")
        receipt = ctx.emitted[-1][1]
        self.assertFalse(receipt["passed"])
        self.assertEqual(receipt["request_count"], 4)
        self.assertEqual(len(receipt["objects"]), 3)
        self.assertEqual(len(ctx.transport.calls), 4)
        self.assertTrue(ctx.emitted[-1][0].exists())


if __name__ == "__main__":
    unittest.main()
