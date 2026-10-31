"""Offline fourth-platform approvals keep ARM absence, signatures and ETags exact."""
from copy import deepcopy
import hashlib
import json
import unittest
from unittest.mock import patch

from release_helpers import approval
from release_helpers.context import OperatorError
import test_release_approval as existing


ARM_POINTER = "stable/windows-arm64/latest.json"
ARM_MANIFEST = "windows-arm64-manifest.json"
ARM_ENVELOPE = "windows-arm64-envelope.json"


class ArmApprovalTests(unittest.TestCase):
    fixture = existing.ApprovalTests.fixture
    update = existing.ApprovalTests.update

    def setUp(self):
        self.enterContext(patch.object(approval, "BundleVerifier", existing.FakeVerifier))

    def arm_fixture(self, stage="reserve", *, previous_arm=False):
        ctx, args = self.fixture(stage)
        ctx.bindings["platforms"].append("windows-arm64")
        ctx.bindings["artifacts"]["windows-arm64"] = {}
        publication = {"name": ARM_MANIFEST, "envelope_name": ARM_ENVELOPE,
                       "reservation": "publication-" + ARM_MANIFEST,
                       "data": (ARM_MANIFEST + ctx.version).encode()}
        ctx.publications.append(publication)
        expected_count = {"reserve": 29, "sign": 30, "publish": 34}[stage]
        files = ctx.actual_proof["files"]
        for index in range(len(files), expected_count):
            files[f"file{index}"] = {"sha256": "7" * 64, "size": 1}
        if stage == "publish":
            ctx.actual_proof["verified_primary_signatures"] = sorted(
                item["envelope_name"] for item in ctx.publications)
        self.update(ctx, args, "proof", lambda value: value.update(deepcopy(ctx.actual_proof)))
        self.update(ctx, args, "download", lambda value: value.update(files=deepcopy(files)))

        old_prefix = f"stable/releases/{ctx.bindings['previous']['version']}/"
        baseline = json.loads(ctx.pinned(ctx.bindings["previous_private_r2"]))["objects"]
        if previous_arm:
            baseline[ARM_POINTER] = {"sha256": "a" * 64, "size": 101, "etag": '"old-arm-pointer"'}
            baseline[old_prefix + ARM_MANIFEST] = {
                "sha256": "a" * 64, "size": 101, "etag": '"old-arm-immutable"'}
            ctx.bindings["previous_signed_envelopes"][old_prefix + ARM_MANIFEST] = {}
        ctx.bindings["previous_private_r2"] = ctx.write("baseline.json", {"objects": baseline})
        prefix = f"stable/releases/{ctx.version}/"
        reservation = prefix + publication["reservation"]
        record = {"sha256": hashlib.sha256(publication["data"]).hexdigest(),
                  "size": len(publication["data"]), "etag": '"new-arm-reservation"'}
        r2 = json.loads(args.r2_proof.read_bytes())
        r2["objects"][ARM_POINTER] = deepcopy(baseline[ARM_POINTER]) if previous_arm else None
        if previous_arm:
            r2["objects"][old_prefix + ARM_MANIFEST] = deepcopy(baseline[old_prefix + ARM_MANIFEST])
        r2["objects"][prefix + ARM_MANIFEST] = None
        r2["objects"][reservation] = deepcopy(record) if stage != "reserve" else None
        r2["request_count"] = 17 if previous_arm else 16
        if stage != "reserve":
            prior = json.loads(args.prior_r2.read_bytes())
            for key in (ARM_POINTER, prefix + ARM_MANIFEST):
                prior["objects"][key] = deepcopy(r2["objects"][key])
            if previous_arm:
                prior["objects"][old_prefix + ARM_MANIFEST] = deepcopy(baseline[old_prefix + ARM_MANIFEST])
            prior["objects"][reservation] = deepcopy(record) if stage == "publish" else None
            prior["request_count"] = r2["request_count"]
            reference = ctx.write("prior-r2.json", prior)
            args.prior_r2_sha256 = reference["sha256"]
            r2["previous_proof_sha256"] = reference["sha256"]
        self.update(ctx, args, "r2_proof", lambda value: value.update(r2))
        return ctx, args

    def rejected(self, ctx, args):
        with self.assertRaises((ValueError, OperatorError)):
            approval.Approval(ctx).run(args)
        self.assertEqual(ctx.posts, [])

    def test_each_gate_requires_four_platforms_with_absent_first_arm_pointer(self):
        for previous_arm in (False, True):
            for stage in ("reserve", "sign", "publish"):
                with self.subTest(previous_arm=previous_arm, stage=stage):
                    ctx, args = self.arm_fixture(stage, previous_arm=previous_arm)
                    result = approval.Approval(ctx).run(args)
                    self.assertTrue(result["approval_confirmed"])
                    self.assertEqual(len(ctx.posts), 1)
                    self.assertEqual(ctx.waiting_checks, 3)
                    self.assertIn("Windows ARM64", ctx.posts[0][1]["comment"])

    def test_first_arm_pointer_must_be_absent_before_every_gate(self):
        for stage in ("reserve", "sign", "publish"):
            with self.subTest(stage=stage):
                ctx, args = self.arm_fixture(stage)
                self.update(ctx, args, "r2_proof", lambda value: value["objects"].update({
                    ARM_POINTER: deepcopy(value["objects"]["stable/windows/latest.json"])}))
                self.rejected(ctx, args)
                self.assertEqual(ctx.gets, [])

    def test_arm_reservation_inventory_and_original_etag_are_required(self):
        for case in ("missing", "extra", "count", "etag"):
            with self.subTest(case=case):
                ctx, args = self.arm_fixture("publish")
                key = f"stable/releases/{ctx.version}/publication-{ARM_MANIFEST}"
                def change(value):
                    if case == "missing":
                        value["objects"].pop(key)
                    elif case == "extra":
                        value["objects"]["stable/unexpected"] = None
                    elif case == "count":
                        value["request_count"] = 13
                    else:
                        value["objects"][key]["etag"] = '"borrowed-x64-etag"'
                self.update(ctx, args, "r2_proof", change)
                self.rejected(ctx, args)

    def test_existing_arm_predecessor_keeps_its_own_pointer_and_immutable_etags(self):
        for name in (ARM_POINTER, "immutable"):
            with self.subTest(name=name):
                ctx, args = self.arm_fixture("publish", previous_arm=True)
                key = name if name == ARM_POINTER else f"stable/releases/{ctx.bindings['previous']['version']}/{ARM_MANIFEST}"
                self.update(ctx, args, "r2_proof", lambda value: value["objects"][key].update(etag='"foreign"'))
                self.rejected(ctx, args)

    def test_three_signatures_cannot_approve_a_four_platform_publish(self):
        ctx, args = self.arm_fixture("publish")
        ctx.actual_proof["verified_primary_signatures"].remove(ARM_ENVELOPE)
        self.update(ctx, args, "proof", lambda value: value.update(deepcopy(ctx.actual_proof)))
        self.rejected(ctx, args)


if __name__ == "__main__":
    unittest.main()
