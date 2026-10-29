"""Offline approval orchestration tests; every transport is an in-memory mock."""

from copy import deepcopy
from datetime import datetime, timedelta, timezone
import hashlib
import json
import os
from pathlib import Path
from types import SimpleNamespace
import tempfile
import unittest
from unittest.mock import patch

from release_helpers import approval


class FakeContext:
    repository = "MaplePrivacyLabs/Maple"
    workflow = ".github/workflows/agent-updates-publish.yml"
    envname = "updates-production"

    def __init__(self, root, identity_number, stage):
        self.root, self.stage = root, stage
        self.version, previous_version = (("2.4.6", "2.4.5") if identity_number == 1 else ("3.8.12", "3.8.11"))
        self.build, self.source = str(700 + identity_number), str(identity_number) * 40
        self.producer_run, self.producer_attempt = 1000 + identity_number, identity_number
        self.environment_id, self.reviewer_id = 900 + identity_number, 800 + identity_number
        self.tooling_sha256, self.bindings_sha256, self.policy_sha256 = "a" * 64, "b" * 64, "c" * 64
        self.core = SimpleNamespace(decode_json=json.loads)
        self.policy = {"r2": {"route": {"sha256": "d" * 64}}}
        self.identity = {"version": self.version, "build": self.build, "source_sha": self.source,
                         "run_id": self.producer_run, "run_attempt": self.producer_attempt,
                         "pub_date": "2026-10-01T01:02:03Z"}
        self.publication = {"run_id": 2000 + identity_number, "run_attempt": identity_number,
                            "source_sha": str(identity_number + 2) * 40, "workflow": self.workflow}
        self.bindings = {"schema": "agent-release-bindings-v1", "producer_identity": self.identity,
                         "publication_identity": self.publication, "previous": {"version": previous_version},
                         "freeze": {"sha256": "e" * 64}, "selected_publication": {"sha256": "f" * 64},
                         "artifacts": {platform: {} for platform in ("macos", "linux", "windows")}}
        self.gets, self.posts, self.emitted = [], [], []
        self.waiting_checks = 0
        self.race_on_check = None
        self.post_error = None
        self.post_response = [{"id": 51, "environment": self.envname, "sha": self.publication["source_sha"], "ref": "master"}]
        self.run_record = {"id": self.publication["run_id"], "head_sha": self.publication["source_sha"],
                           "head_branch": "master", "run_attempt": self.publication["run_attempt"],
                           "event": "workflow_dispatch", "status": "waiting", "path": self.workflow,
                           "repository": {"full_name": self.repository}, "head_repository": {"full_name": self.repository},
                           "display_title": f"Publish Agent {self.version} from verified build {self.producer_run}"}
        stage_index = ("reserve", "sign", "publish").index(stage)
        self.jobs = [{"id": 30 + index, "name": name, "run_id": self.publication["run_id"],
                      "status": "completed" if index <= stage_index else "waiting",
                      "conclusion": "success" if index <= stage_index else None,
                      "started_at": "2026-10-01T02:00:00Z", "completed_at": "2026-10-01T02:10:00Z"}
                     for index, name in enumerate(("verify", "reserve", "sign", "publish")[:stage_index + 2])]
        self.pending = [{"environment": {"id": self.environment_id, "name": self.envname},
                         "current_user_can_approve": True, "wait_timer": 0}]
        self.environment = {"id": self.environment_id, "name": self.envname, "can_admins_bypass": False,
                            "deployment_branch_policy": {"protected_branches": False, "custom_branch_policies": True},
                            "protection_rules": [{"type": "required_reviewers", "prevent_self_review": False,
                                                  "reviewers": [{"type": "User", "reviewer": {"id": self.reviewer_id}}]},
                                                 {"type": "branch_policy"}]}
        self.branches = {"total_count": 1, "branch_policies": [{"name": "master", "type": "branch"}]}
        self.artifact_record = {"id": 50, "name": f"agent-update-{approval.Approval.STAGES[stage]}-{self.publication['run_id']}",
                                "size_in_bytes": 1234, "digest": "sha256:" + "9" * 64,
                                "expired": False, "created_at": "2026-10-01T02:05:00Z",
                                "workflow_run": {"id": self.publication["run_id"], "head_sha": self.publication["source_sha"]}}
        self.artifacts = {"total_count": 1, "artifacts": [self.artifact_record]}

    def write(self, name, value):
        path = self.root / name
        data = json.dumps(value, sort_keys=True).encode()
        path.write_bytes(data)
        return {"path": str(path), "sha256": hashlib.sha256(data).hexdigest()}

    def pinned(self, reference):
        data = Path(reference["path"]).read_bytes()
        if hashlib.sha256(data).hexdigest() != reference["sha256"]:
            raise ValueError("pinned digest differs")
        return data

    def emit(self, path, value):
        descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        with os.fdopen(descriptor, "wb") as output:
            output.write(json.dumps(value).encode())
            output.flush()
            os.fsync(output.fileno())
        self.emitted.append((path, deepcopy(value)))

    def gh_get(self, path):
        self.gets.append(path)
        run_path = f"actions/runs/{self.publication['run_id']}"
        if path == run_path:
            self.waiting_checks += 1
            return deepcopy(self.run_record)
        if path == run_path + f"/attempts/{self.publication['run_attempt']}/jobs?per_page=100":
            jobs = deepcopy(self.jobs)
            if self.race_on_check == self.waiting_checks:
                jobs[-1]["id"] += 100
            return {"total_count": len(jobs), "jobs": jobs}
        if path == run_path + "/pending_deployments":
            return deepcopy(self.pending)
        if path == run_path + "/artifacts?per_page=100":
            return deepcopy(self.artifacts)
        if path == "environments/" + self.envname:
            return deepcopy(self.environment)
        if path == "environments/" + self.envname + "/deployment-branch-policies":
            return deepcopy(self.branches)
        raise AssertionError("unexpected GET: " + path)

    def gh_post(self, path, body):
        self.posts.append((path, deepcopy(body)))
        if len(self.emitted) != 1 or not self.emitted[0][0].name.endswith("-intent.json"):
            raise AssertionError("POST preceded durable intent")
        if self.post_error:
            raise self.post_error
        return deepcopy(self.post_response)


class FakeVerifier:
    def __init__(self, ctx):
        self.ctx = ctx

    def setup(self):
        return self.ctx.bindings, self.ctx.core

    def verify_bundle(self, bindings, core, bundle, stage, previous):
        if previous != self.ctx.expected_previous:
            raise ValueError("wrong predecessor")
        return deepcopy(self.ctx.publications), self.ctx.metadata, deepcopy(self.ctx.actual_proof)


class ApprovalTests(unittest.TestCase):
    def setUp(self):
        self.enterContext(patch.object(approval, "BundleVerifier", FakeVerifier))

    def fixture(self, stage="reserve", identity_number=1):
        temporary = self.enterContext(tempfile.TemporaryDirectory())
        root = Path(temporary).resolve()
        root.chmod(0o700)
        ctx = FakeContext(root, identity_number, stage)
        bundle = root / "bundle"
        bundle.mkdir()
        ctx.publications = [{"name": name, "envelope_name": envelope, "reservation": "publication-" + name,
                             "data": (name + ctx.version).encode()}
                            for name, envelope in zip(approval.Approval.MANIFESTS,
                                                      ("envelope.json", "linux-envelope.json", "windows-envelope.json"))]
        ctx.metadata = b"exact private metadata"
        files = {f"file{index}": {"sha256": "8" * 64, "size": 1}
                 for index in range({"reserve": 23, "sign": 24, "publish": 27}[stage])}
        ctx.actual_proof = {"schema": "agent-release-bundle-proof-v1", "tooling_sha256": ctx.tooling_sha256,
                            "bindings_sha256": ctx.bindings_sha256, "stage": approval.Approval.STAGES[stage],
                            "files": files, "verified_primary_signatures": sorted(item["envelope_name"] for item in ctx.publications)
                            if stage == "publish" else []}
        download = {"schema": "agent-release-stage-download-v1", "stage": approval.Approval.STAGES[stage],
                    "bindings_sha256": ctx.bindings_sha256, "tooling_sha256": ctx.tooling_sha256,
                    "publication_run": ctx.publication["run_id"], "attempt": ctx.publication["run_attempt"],
                    "publisher_source": ctx.publication["source_sha"], "producer": ctx.producer_run,
                    "producer_attempt": ctx.producer_attempt, "source": ctx.source, "build": ctx.build, "version": ctx.version,
                    "freeze_sha256": ctx.bindings["freeze"]["sha256"],
                    "selected_publication_sha256": ctx.bindings["selected_publication"]["sha256"],
                    "recovered_envelopes": [], "destination": str(bundle), "files": files,
                    "artifact": deepcopy(ctx.artifact_record), "actual_zip_sha256": "9" * 64, "actual_zip_size": 1234,
                    "producer_job_id": ctx.jobs[-2]["id"]}
        old_prefix = f"stable/releases/{ctx.bindings['previous']['version']}/"
        baseline = {key: {"sha256": str(index) * 64, "size": index + 1, "etag": f'"old-{index}"'}
                    for index, key in enumerate(("stable/latest.json", "stable/linux/latest.json", "stable/windows/latest.json",
                                                 *(old_prefix + name for name in approval.Approval.MANIFESTS)))}
        ctx.bindings["previous_private_r2"] = ctx.write("baseline.json", {"objects": baseline})
        prefix = f"stable/releases/{ctx.version}/"
        absent = {prefix + name: None for name in ("publication-manifest.json", "publication-linux-manifest.json",
                                                  "publication-windows-manifest.json", "publication-metadata.json",
                                                  *approval.Approval.MANIFESTS)}
        objects = deepcopy(baseline) | absent
        previous_objects = deepcopy(objects)
        for index, (key, data) in enumerate([(prefix + item["reservation"], item["data"]) for item in ctx.publications]
                                          + [(prefix + "publication-metadata.json", ctx.metadata)]):
            record = {"sha256": hashlib.sha256(data).hexdigest(), "size": len(data), "etag": f'"new-{index}"'}
            if stage != "reserve":
                objects[key] = record
            if stage == "publish":
                previous_objects[key] = deepcopy(record)
        end = datetime.now(timezone.utc) - timedelta(seconds=1)
        r2 = {"schema": "agent-release-r2-proof-v1", "tooling_sha256": ctx.tooling_sha256,
              "policy_sha256": ctx.policy_sha256,
              "bindings_sha256": ctx.bindings_sha256, "phase": approval.Approval.PHASES[stage], "passed": True,
              "request_count": 13, "route_sha256": ctx.policy["r2"]["route"]["sha256"],
              "started_at": (end - timedelta(seconds=2)).isoformat(), "completed_at": end.isoformat(),
              "objects": objects, "previous_proof_sha256": None}
        prior_r2 = None
        previous = None
        if stage != "reserve":
            prior = deepcopy(r2)
            prior["phase"] = "before-reserve" if stage == "sign" else "before-sign"
            prior["objects"] = previous_objects
            prior_r2 = ctx.write("prior-r2.json", prior)
            r2["previous_proof_sha256"] = prior_r2["sha256"]
            previous = ctx.write("previous.json", {"schema": "agent-release-bundle-proof-v1"})
        ctx.expected_previous = previous
        args = SimpleNamespace(stage=stage, approve_one=True, previous_proof=Path(previous["path"]) if previous else None,
                               previous_sha256=previous["sha256"] if previous else None,
                               prior_r2=Path(prior_r2["path"]) if prior_r2 else None,
                               prior_r2_sha256=prior_r2["sha256"] if prior_r2 else None)
        for name, value in (("proof", ctx.actual_proof), ("download", download), ("r2_proof", r2)):
            reference = ctx.write(name + ".json", value)
            setattr(args, name, Path(reference["path"]))
            setattr(args, name + "_sha256", reference["sha256"])
        return ctx, args

    def update(self, ctx, args, name, change):
        value = json.loads(getattr(args, name).read_bytes())
        change(value)
        reference = ctx.write(name + ".json", value)
        setattr(args, name + "_sha256", reference["sha256"])

    def rejected(self, ctx, args):
        with self.assertRaises((ValueError, KeyError, FileNotFoundError)):
            approval.Approval(ctx).run(args)
        self.assertEqual(ctx.posts, [])

    def test_two_release_identities_each_approve_only_the_selected_gate(self):
        for identity in (1, 2):
            for stage in ("reserve", "sign", "publish"):
                with self.subTest(identity=identity, stage=stage):
                    ctx, args = self.fixture(stage, identity)
                    result = approval.Approval(ctx).run(args)
                    self.assertTrue(result["approval_confirmed"])
                    self.assertEqual(ctx.waiting_checks, 3)
                    self.assertEqual(len(ctx.posts), 1)
                    path, body = ctx.posts[0]
                    self.assertEqual(path, f"actions/runs/{ctx.publication['run_id']}/pending_deployments")
                    self.assertEqual(body["environment_ids"], [ctx.environment_id])
                    self.assertEqual(body["state"], "approved")
                    self.assertIn(f"Agent{ctx.version}/build{ctx.build}", body["comment"])
                    self.assertEqual([path.name.rsplit("-", 1)[-1] for path, _ in ctx.emitted], ["intent.json", "result.json"])

    def test_missing_authorization_existing_intent_and_wrong_environment_never_post(self):
        for case in ("approve_one", "intent", "environment", "root"):
            with self.subTest(case=case):
                ctx, args = self.fixture()
                if case == "approve_one":
                    args.approve_one = False
                elif case == "intent":
                    (ctx.root / f"{ctx.publication['run_id']}-attempt{ctx.publication['run_attempt']}-reserve-approval-intent.json").write_text("{}")
                elif case == "environment":
                    ctx.envname = "other-environment"
                else:
                    ctx.root.chmod(0o755)
                self.rejected(ctx, args)
                self.assertEqual(ctx.gets, [])

    def test_invalid_local_download_receipts_never_post(self):
        changes = {"schema": "legacy-download-v1", "stage": "signed", "bindings_sha256": "0" * 64,
                   "tooling_sha256": "0" * 64, "publication_run": 123, "attempt": 3,
                   "publisher_source": "0" * 40, "producer": 123, "producer_attempt": 3,
                   "source": "0" * 40, "build": "999", "version": "9.9.9", "freeze_sha256": "0" * 64,
                   "selected_publication_sha256": "0" * 64, "recovered_envelopes": ["envelope.json"],
                   "actual_zip_sha256": "0" * 64, "actual_zip_size": 1, "producer_job_id": True,
                   "files": {}}
        for field, value in changes.items():
            with self.subTest(field=field):
                ctx, args = self.fixture()
                self.update(ctx, args, "download", lambda receipt: receipt.update({field: value}))
                self.rejected(ctx, args)
                self.assertEqual(ctx.gets, [])

    def test_pinned_proof_and_bundle_signature_evidence_are_required(self):
        for case in ("hash", "schema", "tooling", "inventory", "signatures", "predecessor"):
            with self.subTest(case=case):
                ctx, args = self.fixture("publish")
                if case == "hash":
                    args.proof_sha256 = "0" * 64
                elif case == "predecessor":
                    args.previous_sha256 = None
                else:
                    field, value = {"schema": ("schema", "legacy-bundle-v1"), "tooling": ("tooling_sha256", "0" * 64),
                                    "inventory": ("files", {}), "signatures": ("verified_primary_signatures", ["envelope.json"])}[case]
                    ctx.actual_proof[field] = value
                    self.update(ctx, args, "proof", lambda receipt: receipt.update({field: value}))
                self.rejected(ctx, args)

    def test_r2_schema_identity_freshness_inventory_and_own_etags(self):
        cases = ("schema", "tooling_sha256", "bindings_sha256", "policy_sha256", "route_sha256", "phase", "passed", "request_count",
                 "stale", "future", "naive", "duration", "missing", "extra", "old-etags", "reservation-etags", "chain")
        for case in cases:
            with self.subTest(case=case):
                ctx, args = self.fixture("publish")
                def corrupt(receipt):
                    if case in ("schema", "phase"):
                        receipt[case] = "wrong"
                    elif case in ("tooling_sha256", "bindings_sha256", "policy_sha256", "route_sha256"):
                        receipt[case] = "0" * 64
                    elif case == "passed":
                        receipt[case] = False
                    elif case == "request_count":
                        receipt[case] = 12
                    elif case in ("stale", "future", "naive", "duration"):
                        end = datetime.now(timezone.utc) + timedelta(seconds={"stale": -301, "future": 30}.get(case, -1))
                        receipt["completed_at"] = end.isoformat()
                        receipt["started_at"] = (end - timedelta(seconds=182 if case == "duration" else 2)).isoformat()
                        if case == "naive":
                            receipt["completed_at"] = end.replace(tzinfo=None).isoformat()
                    elif case == "missing":
                        receipt["objects"].pop("stable/latest.json")
                    elif case == "extra":
                        receipt["objects"]["unexpected"] = None
                    elif case == "old-etags":
                        left, right = receipt["objects"]["stable/latest.json"], receipt["objects"]["stable/linux/latest.json"]
                        left["etag"], right["etag"] = right["etag"], left["etag"]
                    elif case == "reservation-etags":
                        prefix = f"stable/releases/{ctx.version}/"
                        left, right = receipt["objects"][prefix + "publication-manifest.json"], receipt["objects"][prefix + "publication-linux-manifest.json"]
                        left["etag"], right["etag"] = right["etag"], left["etag"]
                    else:
                        receipt["previous_proof_sha256"] = "0" * 64
                self.update(ctx, args, "r2_proof", corrupt)
                self.rejected(ctx, args)
                self.assertEqual(ctx.gets, [])

    def test_reserve_starts_both_proof_chains(self):
        for field in ("previous_proof", "prior_r2", "r2_previous"):
            with self.subTest(field=field):
                ctx, args = self.fixture()
                if field == "r2_previous":
                    self.update(ctx, args, "r2_proof", lambda receipt: receipt.update(previous_proof_sha256="0" * 64))
                else:
                    setattr(args, field, ctx.root / "unwanted.json")
                self.rejected(ctx, args)

    def test_predecessor_r2_proof_is_pinned_and_has_the_immediate_phase(self):
        for case in ("absent", "hash", "phase", "schema", "bindings", "tooling", "policy", "passed"):
            with self.subTest(case=case):
                ctx, args = self.fixture("publish")
                if case == "absent":
                    args.prior_r2 = None
                elif case == "hash":
                    args.prior_r2_sha256 = "0" * 64
                else:
                    previous = json.loads(args.prior_r2.read_bytes())
                    field, value = {"phase": ("phase", "before-reserve"), "schema": ("schema", "legacy-r2-v1"),
                                    "bindings": ("bindings_sha256", "0" * 64), "tooling": ("tooling_sha256", "0" * 64),
                                    "policy": ("policy_sha256", "0" * 64),
                                    "passed": ("passed", False)}[case]
                    previous[field] = value
                    reference = ctx.write("prior-r2.json", previous)
                    args.prior_r2_sha256 = reference["sha256"]
                    self.update(ctx, args, "r2_proof", lambda receipt: receipt.update(previous_proof_sha256=reference["sha256"]))
                self.rejected(ctx, args)
                self.assertEqual(ctx.gets, [])

    def test_download_bundle_must_remain_inside_the_private_root(self):
        ctx, args = self.fixture()
        outside = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.update(ctx, args, "download", lambda receipt: receipt.update(destination=str(outside)))
        self.rejected(ctx, args)
        self.assertEqual(ctx.gets, [])

    def test_remote_run_jobs_pending_environment_policy_and_artifact_fail_closed(self):
        for case in ("source", "attempt", "branch", "event", "title", "workflow", "repository", "completed",
                     "producer-failed", "producer-skipped", "producer-id", "second-waiting", "no-waiting",
                     "pending-id", "pending-name", "cannot-approve", "wait-timer", "admin-bypass", "reviewer",
                     "self-review", "extra-rule", "branch-policy", "artifact-digest", "artifact-expired",
                     "artifact-source", "artifact-run", "artifact-duplicate", "artifact-time"):
            with self.subTest(case=case):
                ctx, args = self.fixture("publish")
                run_fields = {"source": ("head_sha", "0" * 40), "attempt": ("run_attempt", 3), "branch": ("head_branch", "topic"),
                              "event": ("event", "push"), "title": ("display_title", "another publication"),
                              "workflow": ("path", "different.yml"), "repository": ("repository", {"full_name": "other/repo"}),
                              "completed": ("status", "completed")}
                if case in run_fields:
                    field, value = run_fields[case]
                    ctx.run_record[field] = value
                elif case.startswith("producer-"):
                    ctx.jobs[-2]["conclusion" if case != "producer-id" else "id"] = {
                        "producer-failed": "failure", "producer-skipped": "skipped", "producer-id": 999}[case]
                elif case == "second-waiting":
                    ctx.jobs[0]["status"] = "waiting"
                elif case == "no-waiting":
                    ctx.jobs[-1]["status"] = "queued"
                elif case == "pending-id":
                    ctx.pending[0]["environment"]["id"] += 1
                elif case == "pending-name":
                    ctx.pending[0]["environment"]["name"] = "other"
                elif case == "cannot-approve":
                    ctx.pending[0]["current_user_can_approve"] = False
                elif case == "wait-timer":
                    ctx.pending[0]["wait_timer"] = 10
                elif case == "admin-bypass":
                    ctx.environment["can_admins_bypass"] = True
                elif case == "reviewer":
                    ctx.environment["protection_rules"][0]["reviewers"][0]["reviewer"]["id"] += 1
                elif case == "self-review":
                    ctx.environment["protection_rules"][0]["prevent_self_review"] = True
                elif case == "extra-rule":
                    ctx.environment["protection_rules"].append({"type": "unexpected"})
                elif case == "branch-policy":
                    ctx.branches["branch_policies"][0]["name"] = "*"
                elif case == "artifact-digest":
                    ctx.artifact_record["digest"] = "sha256:" + "0" * 64
                elif case == "artifact-expired":
                    ctx.artifact_record["expired"] = True
                elif case == "artifact-source":
                    ctx.artifact_record["workflow_run"]["head_sha"] = "0" * 40
                elif case == "artifact-run":
                    ctx.artifact_record["workflow_run"]["id"] += 1
                elif case == "artifact-duplicate":
                    ctx.artifacts["artifacts"].append(deepcopy(ctx.artifact_record))
                    ctx.artifacts["total_count"] += 1
                elif case == "artifact-time":
                    ctx.jobs[-2]["started_at"] = "2026-10-01T02:06:00Z"
                self.rejected(ctx, args)

    def test_final_gate_race_preserves_intent_and_failed_result_without_post(self):
        ctx, args = self.fixture()
        ctx.race_on_check = 3
        self.rejected(ctx, args)
        self.assertEqual(len(ctx.emitted), 2)
        self.assertTrue(ctx.emitted[0][0].name.endswith("-intent.json"))
        self.assertFalse(ctx.emitted[1][1]["approval_confirmed"])
        self.assertEqual(ctx.emitted[1][1]["error_type"], "ValueError")
        self.rejected(ctx, args)
        self.assertEqual(len(ctx.emitted), 2)

    def test_r2_expiration_after_intent_preserves_failed_result_without_post(self):
        ctx, args = self.fixture()
        helper = approval.Approval(ctx)
        original = helper.r2_proof
        calls = []

        def expires(*values):
            calls.append(True)
            if len(calls) == 3:
                raise ValueError("evidence expired after intent")
            return original(*values)

        with patch.object(helper, "r2_proof", side_effect=expires):
            with self.assertRaisesRegex(ValueError, "expired after intent"):
                helper.run(args)
        self.assertEqual(ctx.posts, [])
        self.assertEqual(len(ctx.emitted), 2)
        self.assertFalse(ctx.emitted[-1][1]["approval_confirmed"])
        self.assertTrue(ctx.emitted[0][0].name.endswith("-intent.json"))
        self.assertFalse(ctx.emitted[1][1]["approval_confirmed"])
        self.assertEqual(ctx.emitted[1][1]["error_type"], "ValueError")
        self.rejected(ctx, args)
        self.assertEqual(len(ctx.emitted), 2)

    def test_post_failure_or_unexpected_response_is_never_retried(self):
        for case in ("transport", "response"):
            with self.subTest(case=case):
                ctx, args = self.fixture("sign")
                if case == "transport":
                    ctx.post_error = RuntimeError("ambiguous transport result")
                else:
                    ctx.post_response[0]["sha"] = "0" * 40
                with self.assertRaises((RuntimeError, ValueError)):
                    approval.Approval(ctx).run(args)
                self.assertEqual(len(ctx.posts), 1)
                self.assertFalse(ctx.emitted[-1][1]["approval_confirmed"])
                with self.assertRaisesRegex(ValueError, "no automatic retry"):
                    approval.Approval(ctx).run(args)
                self.assertEqual(len(ctx.posts), 1)


if __name__ == "__main__":
    unittest.main()
