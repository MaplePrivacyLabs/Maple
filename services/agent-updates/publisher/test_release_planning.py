"""Offline planning and immutable lifecycle tests; no real credentials or releases."""
from copy import deepcopy
from pathlib import Path
from types import SimpleNamespace
import base64
import json
import os
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import publisher
import test_publisher as fixtures
import test_trust as trust_fixtures
import test_windows_publisher as windows_fixtures
import test_windows_arm64_publisher as arm_fixtures
from release_helpers import planning
from release_helpers.context import (OperatorError, SCHEMA_BUNDLE, SCHEMA_FREEZE, decode, read_regular, sha,
                                     ALL_PLATFORMS, manifest_names, envelope_names, pointer_names,
                                     platform_counts, predecessor_platforms, r2_request_count)
from release_helpers.public import PublicVerifier


class LocalContext:
    """Only transport and trusted-context acquisition are faked, not package validation."""
    def __init__(self, root, bindings):
        self.root, self.bindings, self.core = root, bindings, publisher
        self.identity = bindings["producer_identity"]
        self.source, self.version, self.build = (self.identity[key] for key in ("source_sha", "version", "build"))
        self.producer_run, self.producer_attempt = self.identity["run_id"], self.identity["run_attempt"]
        self.tree = bindings["payload_tree"]
        self.tooling_sha256, self.policy_sha256 = bindings["tooling_sha256"], bindings["policy_sha256"]
        self.core_sha256 = bindings["code_pins"]["services/agent-updates/publisher/publisher.py"]
        self.primary_sha256 = sha((publisher.public_key_text(bindings["public_key"]) + "\n").encode())
        self.route_sha256 = sha(read_regular(root / "route.toml"))
        self.bindings_path = root / "context-bindings.json"
        self.snapshot()

    def snapshot(self):
        raw = publisher.encode_json(self.bindings)
        self.bindings_path.write_bytes(raw)
        self.bindings_sha256 = sha(raw)

    def pinned(self, ref):
        raw = read_regular(Path(ref["path"]))
        if sha(raw) != ref["sha256"]:
            raise OperatorError("pinned local input changed")
        return raw

    @property
    def publication(self):
        return self.bindings["publication_identity"]

    def tool(self, name):
        return str(self.root / "tools" / name)


class ReleasePlanningTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="release-planning-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.root.chmod(0o700)
        tools = self.root / "tools"
        tools.mkdir(mode=0o700)
        for name in ("gh", "minisign"):
            (tools / name).write_bytes(b"offline executable placeholder; never run\n")
            (tools / name).chmod(0o700)
        self.policy = {"schema": "agent-release-policy-v1", "environment": {
            "id": 17, "name": "updates-production", "reviewer_id": 29}, "r2": {
            "endpoint": "https://" + "a" * 32 + ".r2.cloudflarestorage.com", "bucket": "offline-fixture",
            "route": self.record("route.toml", b"references only\n")},
            "tools": {name: {"path": str(tools / name), "sha256": sha((tools / name).read_bytes())}
                      for name in ("gh", "minisign")}}
        self.policy_ref = self.record("policy.json", publisher.encode_json(self.policy))
        self.notes = self.record("notes-input.txt", b"Offline release notes\n")
        profiles = decode((publisher.ROOT / "apps/maple-agent/release-profiles.json").read_bytes())
        self.key = self.record("key-input.pub", (profiles["prod"]["update_public_keys"][0] + "\n").encode())
        self.old = {"version": "1.2.2", "build": "41", "source_sha": "b" * 40,
                    "publisher_run": 88, "publisher_attempt": 2}
        old_ref = self.record("previous-receipt.json", b"{}\n")
        self.previous = {"schema": planning.PREVIOUS_SCHEMA, "previous": self.old,
                         **{name: old_ref for name in planning.PREVIOUS_REFS},
                         "previous_signed_envelopes": {f"stable/releases/{self.old['version']}/{name}": old_ref
                                                       for name in planning.NAMES}}
        self.previous_ref = self.record("previous.json", publisher.encode_json(self.previous))
        trust = trust_fixtures.TrustTests()
        trust.setUp()
        win = windows_fixtures.WindowsTrustTests()
        win.setUp()
        self.run = {**trust.run, "head_sha": fixtures.SOURCE_SHA, "run_number": 42,
                    "created_at": "2026-10-21T18:00:00Z"}
        self.jobs = deepcopy(trust.jobs)
        self.jobs["jobs"] += win.jobs(win.windows | {"Build Windows ARM64 binary (prod)",
            "Signed Windows ARM64 (prod)", "Verify downloaded Windows ARM64 (prod)"})["jobs"]
        self.jobs["total_count"] = len(self.jobs["jobs"])
        self.artifacts = {"total_count": 4, "artifacts": [
            {"id": index + 100, "name": f"maple-agent-prod-{target}-{self.run['id']}",
             "size_in_bytes": 1234 + index, "digest": "sha256:" + str(index + 1) * 64, "expired": False,
             "workflow_run": {"id": self.run["id"], "head_sha": self.run["head_sha"], "head_branch": "master"}}
            for index, target in enumerate(planning.TARGETS.values())]}
        self.tree = {"sha": self.run["head_sha"], "tree": {"sha": "c" * 40}}
        self.master = {"object": {"type": "commit", "sha": "d" * 40}}
        self.requests = []

    def record(self, name, data):
        path = self.root / name
        path.write_bytes(data)
        path.chmod(0o600)
        return {"path": str(path), "sha256": sha(data)}

    def args(self, **changes):
        values = dict(network=True, state_dir=self.root, policy=Path(self.policy_ref["path"]),
                      policy_sha256=self.policy_ref["sha256"], producer_run=self.run["id"], version="1.2.3",
                      notes_file=Path(self.notes["path"]), public_key=Path(self.key["path"]),
                      previous_manifest=Path(self.previous_ref["path"]), previous_manifest_sha256=self.previous_ref["sha256"],
                      output=self.root / "bindings-plan.json")
        values.update(changes)
        return SimpleNamespace(**values)

    def transport(self, argv, **kwargs):
        self.requests.append(argv)
        self.assertEqual(argv[1:6], ["api", "--hostname", "github.com", "--method", "GET"])
        self.assertEqual(kwargs["stdin"], subprocess.DEVNULL)
        path = argv[-1].removeprefix(f"repos/{publisher.REPOSITORY}/")
        expected = {f"actions/runs/{self.run['id']}": self.run,
                    f"actions/runs/{self.run['id']}/jobs?filter=latest&per_page=100": self.jobs,
                    f"actions/runs/{self.run['id']}/artifacts?per_page=100": self.artifacts,
                    f"git/commits/{self.run['head_sha']}": self.tree, "git/ref/heads/master": self.master}
        self.assertIn(path, expected)
        return SimpleNamespace(returncode=0, stdout=publisher.encode_json(expected[path]))

    def make_plan(self, **changes):
        with patch.object(planning.subprocess, "run", side_effect=self.transport), \
             patch.object(planning, "tooling_digest", return_value="e" * 64):
            result = planning.plan(self.args(**changes))
        raw = Path(result["bindings"]).read_bytes()
        self.assertEqual(sha(raw), result["bindings_sha256"])
        return result, decode(raw)

    def test_plan_records_canonical_prod_jobs_exact_artifacts_and_private_input_copies(self):
        _, bindings = self.make_plan()
        self.assertEqual(bindings["platforms"], list(ALL_PLATFORMS))
        self.assertEqual(set(bindings["artifacts"]), set(ALL_PLATFORMS))
        self.assertEqual(bindings["producer_identity"]["run_attempt"], self.run["run_attempt"])
        self.assertEqual(bindings["producer_identity"]["build"], str(self.run["run_number"]))
        self.assertEqual(bindings["publisher_source"], self.master["object"]["sha"])
        self.assertNotEqual(bindings["publisher_source"], bindings["producer_identity"]["source_sha"])
        self.assertIsNone(bindings["freeze"])
        self.assertIsNone(bindings["publication_identity"])
        proof = decode(read_regular(Path(bindings["qualification"]["path"])))
        self.assertEqual(proof["jobs"], self.jobs)
        self.assertEqual(proof["selected_final_artifact_metadata"], self.artifacts["artifacts"])
        self.assertTrue(proof["producer_terminal"] and proof["required_included_jobs_all_success"])
        self.assertIs(proof["scope"]["include_windows_arm64"], True)
        for path in (bindings["public_key"], bindings["notes"]["path"], bindings["qualification"]["path"],
                     str(self.root / "bindings-plan.json")):
            self.assertEqual(Path(path).stat().st_mode & 0o777, 0o600)
        self.assertEqual(Path(bindings["public_key"]).read_bytes(), Path(self.key["path"]).read_bytes())

    def test_plan_requires_the_arm64_package_and_concrete_signed_consumer_chain(self):
        original = deepcopy((self.jobs, self.artifacts))
        for kind in ("missing", "x64-alias", "failed-signing", "skipped-consumer"):
            self.jobs, self.artifacts = deepcopy(original)
            arm = self.artifacts["artifacts"][-1]
            if kind == "missing":
                self.artifacts["artifacts"].pop()
                self.artifacts["total_count"] -= 1
            elif kind == "x64-alias":
                arm["name"] = self.artifacts["artifacts"][-2]["name"]
            else:
                name = "Signed Windows ARM64 (prod)" if kind == "failed-signing" else "Verify downloaded Windows ARM64 (prod)"
                next(job for job in self.jobs["jobs"] if job["name"] == name)["conclusion"] = (
                    "failure" if kind == "failed-signing" else "skipped")
            with self.subTest(kind=kind), self.assertRaises((OperatorError, publisher.PublishError)):
                self.make_plan()
            self.assertFalse((self.root / "qualification.json").exists())

    def test_another_release_changes_only_data_without_literal_version_or_run_dependencies(self):
        self.run.update(id=973, run_attempt=4, run_number=905, head_sha="f" * 40)
        self.tree.update(sha=self.run["head_sha"])
        for platform, item in zip(ALL_PLATFORMS, self.artifacts["artifacts"]):
            item["name"] = f"maple-agent-prod-{planning.TARGETS[platform]}-{self.run['id']}"
            item["workflow_run"].update(id=self.run["id"], head_sha=self.run["head_sha"])
        _, bindings = self.make_plan(version="7.8.9")
        self.assertEqual(bindings["producer_identity"]["version"], "7.8.9")
        self.assertEqual(bindings["producer_identity"]["build"], "905")
        self.assertEqual(bindings["producer_identity"]["run_attempt"], 4)
        self.assertTrue(all(item["name"].endswith("-973") for item in bindings["artifacts"].values()))

    def test_wrong_source_artifact_and_failed_concrete_job_reject_without_outputs(self):
        cases = [lambda: self.artifacts["artifacts"][0]["workflow_run"].update(head_sha="0" * 40),
                 lambda: self.artifacts["artifacts"][0].update(expired=True),
                 lambda: self.artifacts["artifacts"].append(deepcopy(self.artifacts["artifacts"][0])),
                 lambda: self.jobs["jobs"][0].update(conclusion="skipped"),
                 lambda: self.tree.update(sha="0" * 40)]
        for mutate in cases:
            original = deepcopy((self.artifacts, self.jobs, self.tree))
            mutate()
            with self.subTest(mutate=mutate), self.assertRaises((OperatorError, publisher.PublishError)):
                self.make_plan()
            self.assertFalse((self.root / "qualification.json").exists())
            self.artifacts, self.jobs, self.tree = original

    def test_no_network_permission_changed_baseline_and_existing_snapshot_fail_closed(self):
        with patch.object(planning.subprocess, "run") as transport:
            with self.assertRaises(OperatorError):
                planning.plan(self.args(network=False))
            Path(self.previous_ref["path"]).write_bytes(b"{}")
            with self.assertRaises(OperatorError):
                planning.plan(self.args())
            transport.assert_not_called()
        Path(self.previous_ref["path"]).write_bytes(publisher.encode_json(self.previous))
        _, before = self.make_plan()
        with self.assertRaises(OperatorError):
            self.make_plan()
        self.assertEqual(before, decode((self.root / "bindings-plan.json").read_bytes()))

    def frozen_context(self, platforms=ALL_PLATFORMS):
        _, bindings = self.make_plan()
        bindings["platforms"] = list(platforms)
        bindings["artifacts"] = {name: bindings["artifacts"][name] for name in platforms}
        roots = [*manifest_names(platforms), "metadata-inventory.json"]
        names = [*roots, *(f"payloads/file-{index}" for index in range(platform_counts(platforms)["prepared"] - len(roots)))]
        bindings["prepared_files"] = {name: {"size": 1, "sha256": sha(b"x")} for name in names}
        bindings["freeze"] = self.record("freeze-input.json", publisher.encode_json({
            "schema": SCHEMA_FREEZE, "canonical_prepare_completed": True,
            "source": bindings["producer_identity"]["source_sha"], "tree": bindings["payload_tree"],
            "platforms": bindings["platforms"], "notes": bindings["notes"],
            "producer": {**bindings["producer_identity"], "artifacts": {
                platform: item["name"] for platform, item in bindings["artifacts"].items()}},
            "files": bindings["prepared_files"]}))
        return LocalContext(self.root, bindings)

    def selection(self, ctx, **changes):
        value = {"run_id": 654, "run_attempt": 3, "source_sha": ctx.bindings["publisher_source"],
                 "workflow": planning.WORKFLOW, "payload_source": ctx.source,
                 "producer_run": ctx.producer_run, "producer_attempt": ctx.producer_attempt,
                 "version": ctx.version, "platforms": ctx.bindings["platforms"],
                 "inputs": {"build_run_id": str(ctx.producer_run), "version": ctx.version,
                            "notes": ctx.pinned(ctx.bindings["notes"]).decode(),
                            "include_linux": True, "include_windows": True,
                            "include_windows_arm64": "windows-arm64" in ctx.bindings["platforms"]}}
        value.update(changes)
        return value

    def test_bind_publication_rejects_drift_and_preserves_original_snapshot(self):
        ctx = self.frozen_context()
        initial = deepcopy(ctx.bindings)
        for change in ({"payload_source": "0" * 40}, {"source_sha": ctx.source}, {"producer_attempt": True},
                       {"platforms": ["macos", "linux"]}, {"inputs": {"include_linux": True}}):
            ref = self.record("selection.json", publisher.encode_json(self.selection(ctx, **change)))
            with self.subTest(change=change), self.assertRaises(OperatorError):
                planning.bind_publication(ctx, SimpleNamespace(selection=Path(ref["path"]),
                    selection_sha256=ref["sha256"], output=self.root / "selected.json"))
            self.assertFalse((self.root / "selected.json").exists())
        ref = self.record("selection.json", publisher.encode_json(self.selection(ctx)))
        result = planning.bind_publication(ctx, SimpleNamespace(selection=Path(ref["path"]),
            selection_sha256=ref["sha256"], output=self.root / "selected.json"))
        selected = decode(Path(result["bindings"]).read_bytes())
        self.assertEqual(selected["producer_identity"], initial["producer_identity"])
        self.assertEqual(selected["freeze"], initial["freeze"])
        self.assertEqual(selected["publication_identity"]["run_id"], 654)
        self.assertEqual(ctx.bindings, initial)

    def run_freeze(self, platforms=ALL_PLATFORMS):
        _, bindings = self.make_plan()
        original_plan = deepcopy(bindings)
        bindings["platforms"] = list(platforms)
        bindings["artifacts"] = {name: bindings["artifacts"][name] for name in platforms}
        ctx = LocalContext(self.root, bindings)
        mac = fixtures.PackageBoundaryTests()
        linux = fixtures.LinuxPackageBoundaryTests()
        win = windows_fixtures.WindowsPackageBoundaryTests()
        for case in (mac, linux, win):
            case.setUp()
            self.addCleanup(case.doCleanups)
        packages = {"macos": mac.directory, "linux": linux.directory, "windows": win.directory}
        if "windows-arm64" in platforms:
            arm = arm_fixtures.WindowsArm64PackageBoundaryTests()
            arm.setUp()
            self.addCleanup(arm.doCleanups)
            packages["windows-arm64"] = arm.fixture.directory
        for platform in platforms:
            shutil.copytree(packages[platform], self.root / f"packages-{platform}-{bindings['artifacts'][platform]['id']}")
        trust = {key: ctx.identity[key] for key in ("run_id", "run_attempt", "source_sha", "build", "pub_date")}
        trust["artifacts"] = {platform: item["name"] for platform, item in bindings["artifacts"].items()}
        profiles = decode((publisher.ROOT / "apps/maple-agent/release-profiles.json").read_bytes())
        source_profile = {"encoding": "base64", "content": base64.b64encode(publisher.encode_json(profiles)).decode()}
        # Only transport responses are synthetic. prepare and its default attestor run unchanged.
        responses = [self.run, self.jobs, self.artifacts, source_profile]
        attested = []
        def transport(argv, **kwargs):
            if argv[1:3] == ["attestation", "verify"]:
                attested.append(Path(argv[3]))
                self.assertIn(ctx.source, argv)
                self.assertIn("--deny-self-hosted-runners", argv)
                return SimpleNamespace(returncode=0, stdout=b"")
            self.assertEqual(argv[:2], ["gh", "api"])
            return SimpleNamespace(returncode=0, stdout=publisher.encode_json(responses.pop(0)))
        before = deepcopy(bindings)
        old_path = os.environ.get("PATH")
        with patch.object(publisher.subprocess, "run", side_effect=transport):
            result = planning.freeze(ctx, SimpleNamespace(network=True, output=self.root / "frozen.json"))
        self.assertEqual(os.environ.get("PATH"), old_path)
        self.assertEqual(len(attested), len({str(path) for path in attested}))
        self.assertEqual(len(attested), 16 + 5 * ("windows-arm64" in platforms))
        frozen = decode(Path(result["freeze"]["path"]).read_bytes())
        self.assertTrue(frozen["canonical_prepare_completed"])
        self.assertEqual(frozen["producer"], {**trust, "version": ctx.version})
        self.assertEqual(len(frozen["files"]), platform_counts(platforms)["prepared"])
        self.assertEqual(frozen["platforms"], list(platforms))
        snapshot = decode(Path(result["bindings"]).read_bytes())
        self.assertEqual(snapshot["public_key"], str(Path(snapshot["prepared_bundle"]) / "public-key.pub"))
        self.assertEqual(ctx.bindings, before)
        self.assertEqual(decode((self.root / "bindings-plan.json").read_bytes()), original_plan)

    def test_freeze_runs_unchanged_canonical_prepare_and_twenty_one_default_attestations(self):
        self.run_freeze()

    def test_freeze_continues_legacy_three_platform_snapshot_with_sixteen_attestations(self):
        self.run_freeze(planning.PLATFORMS)

    def test_freeze_requires_explicit_network_before_preparer_or_output(self):
        _, bindings = self.make_plan()
        with patch.object(publisher, "prepare") as prepare:
            with self.assertRaises(OperatorError):
                planning.freeze(LocalContext(self.root, bindings), SimpleNamespace(network=False, output=self.root / "no.json"))
            prepare.assert_not_called()
        self.assertFalse((self.root / "no.json").exists())

    def test_bind_public_requires_successful_exact_run_and_complete_signed_file_proof(self):
        ctx = self.frozen_context()
        ctx.bindings["publication_identity"] = {key: self.selection(ctx)[key]
                                                for key in ("run_id", "run_attempt", "source_sha", "workflow")}
        bundle = self.root / "signed"
        bundle.mkdir(mode=0o700)
        prepared_files = ctx.bindings["prepared_files"]
        files = {**prepared_files, **{name: {"size": 1, "sha256": sha(b"x")}
                                     for name in ("reservation.json", *envelope_names(ctx.bindings["platforms"]))}}
        for name in files:
            path = bundle / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"x")
        proof = {"schema": SCHEMA_BUNDLE, "stage": "signed", "producer": ctx.identity,
                 "publication": ctx.publication, "tooling_sha256": ctx.tooling_sha256,
                 "policy_sha256": ctx.policy_sha256, "core_sha256": ctx.core_sha256,
                 "bindings_sha256": ctx.bindings_sha256, "primary_key_sha256": ctx.primary_sha256,
                 "verified_primary_signatures": sorted(envelope_names(ctx.bindings["platforms"])), "files": files}
        success = {"id": ctx.publication["run_id"], "run_attempt": ctx.publication["run_attempt"],
                   "head_sha": ctx.publication["source_sha"], "path": planning.WORKFLOW, "head_branch": "master",
                   "event": "workflow_dispatch", "status": "completed", "conclusion": "success",
                   "repository": {"full_name": publisher.REPOSITORY}, "head_repository": {"full_name": publisher.REPOSITORY}}
        def bind_args():
            a = self.record("success.json", publisher.encode_json(success))
            b = self.record("signed-proof.json", publisher.encode_json(proof))
            return SimpleNamespace(successful_run=Path(a["path"]), successful_run_sha256=a["sha256"],
                signed_stage_proof=Path(b["path"]), signed_stage_proof_sha256=b["sha256"],
                signed_bundle=bundle, output=self.root / "public.json")
        for change in ("run", "signature", "bytes"):
            with self.subTest(change=change):
                if change == "run": success["conclusion"] = "failure"
                if change == "signature": proof["verified_primary_signatures"].pop()
                if change == "bytes": (bundle / envelope_names(ctx.bindings["platforms"])[0]).write_bytes(b"z")
                with self.assertRaises(OperatorError): planning.bind_public(ctx, bind_args())
                self.assertFalse((self.root / "public.json").exists())
                success["conclusion"] = "success"
                proof["verified_primary_signatures"] = sorted(envelope_names(ctx.bindings["platforms"]))
                (bundle / envelope_names(ctx.bindings["platforms"])[0]).write_bytes(b"x")
        result = planning.bind_public(ctx, bind_args())
        bound = decode(Path(result["bindings"]).read_bytes())
        self.assertEqual(bound["envelope_sha256"], {platform: sha(b"x") for platform in ctx.bindings["platforms"]})
        self.assertEqual(bound["operation_bindings"], {"path": str(ctx.bindings_path), "sha256": ctx.bindings_sha256})
        self.assertNotIn("successful_publication_run", ctx.bindings)

    def acceptance_context(self, platforms=ALL_PLATFORMS):
        """Pinned receipt fixtures model completed checks; they make no signature claim."""
        ctx = self.frozen_context(platforms)
        b = ctx.bindings
        inventory = PublicVerifier.inventory(b)
        research_version = "2.0.0"
        research_latest = {"kind": "research-latest", "url": "https://updates.trymaple.ai/latest.json",
                           "method": "GET", "status": 200, "check_passed": True,
                           "body_sha256": sha(publisher.encode_json({"version": research_version})),
                           "research_version": research_version, "headers": {"etag": '"research-baseline"'}}
        research_records = [research_latest] + [
            {"kind": "research-alias", "url": "https://updates.trymaple.ai/download/research/stable/" + key,
             "key": key, "method": method, "status": 302, "check_passed": True,
             "headers": {"location": f"https://github.com/MaplePrivacyLabs/Maple/releases/download/v{research_version}/{key}"}}
            for key in PublicVerifier.RESEARCH_KEYS for method in ("GET", "HEAD")]
        b["research_preservation"] = self.record("old-research.json", publisher.encode_json({
            "passed": True, "request_count": 7, "writes": 0,
            "requests": [item for item in research_records if item["method"] == "GET"]}))
        b["publication_identity"] = {key: self.selection(ctx)[key]
                                     for key in ("run_id", "run_attempt", "source_sha", "workflow")}
        old_objects = {key: {"size": len(ctx.pinned(ref)), "sha256": ref["sha256"], "etag": '"old-etag"'}
                       for key, ref in b["previous_signed_envelopes"].items()}
        b["previous_private_r2"] = self.record("previous-private.json", publisher.encode_json({"objects": old_objects}))
        b["prepared_bundle"] = str(self.root / "prepared")
        operation = self.record("operation-bindings.json", publisher.encode_json(b))
        b["operation_bindings"] = operation
        bundle = self.root / "signed"
        bundle.mkdir(mode=0o700)
        b["signed_bundle"] = str(bundle)
        files = {**b["prepared_files"], **{name: {"size": 1, "sha256": sha(b"x")}
                                         for name in ("reservation.json", *envelope_names(ctx.bindings["platforms"]))}}
        for name in files:
            path = bundle / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"x")
        proof = {"schema": SCHEMA_BUNDLE, "stage": "signed", "producer": ctx.identity,
                 "publication": ctx.publication, "tooling_sha256": ctx.tooling_sha256,
                 "policy_sha256": ctx.policy_sha256, "core_sha256": ctx.core_sha256,
                 "bindings_sha256": operation["sha256"], "primary_key_sha256": ctx.primary_sha256,
                 "verified_primary_signatures": sorted(envelope_names(ctx.bindings["platforms"])), "files": files}
        b["signed_stage_proof"] = self.record("signed-proof.json", publisher.encode_json(proof))
        b["envelope_sha256"] = {platform: sha(b"x") for platform in ctx.bindings["platforms"]}
        ctx.snapshot()
        records = [{"kind": "payload-full", "url": f"https://{host}/payload-{index}", "check_passed": True}
                   for host in PublicVerifier.HOSTS for index in range(inventory["full_streams"] // len(PublicVerifier.HOSTS))]
        records += research_records
        records += [{"kind": "metadata", "url": "https://" + PublicVerifier.HOSTS[0] + f"/metadata-{index}",
                     "check_passed": True} for index in range(inventory["fixed_requests"] - len(records))]
        public = {"schema": planning.SCHEMA_PUBLIC, "all_checks_passed": True,
                  "agent_checks_passed": True, "research_checks_passed": True,
                  "source": ctx.source, "version": ctx.version, "build": ctx.build,
                  "producer_run": ctx.producer_run, "producer_attempt": ctx.producer_attempt,
                  "publication": ctx.publication, "bindings_sha256": ctx.bindings_sha256,
                  "signed_stage_proof": b["signed_stage_proof"], "freeze_sha256": b["freeze"]["sha256"],
                  "envelope_sha256": b["envelope_sha256"], "previous": b["previous"],
                  "previous_public_sha256": b["previous_public"]["sha256"],
                  "policy_sha256": ctx.policy_sha256, "tooling_sha256": ctx.tooling_sha256,
                  "core_sha256": ctx.core_sha256, "full_payload_streams": inventory["full_streams"],
                  "successful_full_payload_streams": inventory["full_streams"],
                  "fixed_request_count": inventory["fixed_requests"], "total_request_count": inventory["fixed_requests"],
                  "included_platforms": b["platforms"],
                  "preserved_previous_immutables": list(predecessor_platforms(b)), "duplicate_full_payload_files_saved": 0,
                  "credentials_used": False, "redirects_followed": False, "producer_attestations_verified_here": False,
                  "analytics_probe_header": {"X-Maple-Download-Probe": "1"}, "research": {
                      "state": "unchanged", "version": research_version, "metadata_sha256": research_latest["body_sha256"]},
                  "requests": records}
        prefix = f"stable/releases/{ctx.version}/"
        envelope_record = {"size": 1, "sha256": sha(b"x"), "etag": '"new-etag"'}
        objects = {**old_objects, **{prefix + name: deepcopy(envelope_record) for name in manifest_names(platforms)},
                   **{pointer: deepcopy(envelope_record) for pointer in
                      pointer_names(platforms)},
                   **{prefix + "publication-" + name: deepcopy(envelope_record) for name in manifest_names(platforms)},
                   prefix + "publication-metadata.json": deepcopy(envelope_record)}
        private = {"schema": planning.SCHEMA_R2, "phase": "after-publish", "passed": True, "request_count": r2_request_count(b),
                   "bindings_sha256": operation["sha256"], "tooling_sha256": ctx.tooling_sha256,
                   "policy_sha256": ctx.policy_sha256, "route_sha256": ctx.route_sha256,
                   "previous_proof_sha256": "f" * 64, "started_at": "2026-10-21T18:00:00+00:00",
                   "completed_at": "2026-10-21T18:00:01+00:00", "objects": objects}
        return ctx, public, private

    def acceptance_args(self, public, private):
        a = self.record("public-receipt.json", publisher.encode_json(public))
        b = self.record("private-receipt.json", publisher.encode_json(private))
        return SimpleNamespace(public_receipt=Path(a["path"]), public_receipt_sha256=a["sha256"],
            private_receipt=Path(b["path"]), private_receipt_sha256=b["sha256"], output=self.root / "next-baseline.json")

    def test_accept_emits_next_pinned_baseline_without_network_crypto_or_binding_mutation(self):
        ctx, public, private = self.acceptance_context()
        original = deepcopy(ctx.bindings)
        with patch.object(planning.subprocess, "run") as transport, patch.object(publisher, "minisign_verify") as crypto:
            result = planning.accept(ctx, self.acceptance_args(public, private))
            transport.assert_not_called()
            crypto.assert_not_called()
        baseline = decode(Path(result["baseline_manifest"]).read_bytes())
        acceptance = decode(Path(result["root_acceptance"]["path"]).read_bytes())
        self.assertEqual(acceptance["schema"], "agent-release-root-acceptance-v1")
        self.assertTrue(acceptance["release_published"])
        self.assertEqual(acceptance["operation_bindings_sha256"], private["bindings_sha256"])
        self.assertNotEqual(private["bindings_sha256"], ctx.bindings_sha256)
        self.assertEqual(private["request_count"], 16)
        self.assertEqual(public["fixed_request_count"], 125)
        self.assertEqual(public["full_payload_streams"], 14)
        self.assertEqual(len(baseline["previous_signed_envelopes"]), 4)
        self.assertEqual(baseline["previous"]["version"], ctx.version)
        self.assertEqual(baseline["previous_root_acceptance"], result["root_acceptance"])
        self.assertNotEqual(baseline["research_preservation"], original["research_preservation"])
        carried = decode(ctx.pinned(baseline["research_preservation"]))
        self.assertEqual(carried["research"], public["research"])
        self.assertEqual(carried["request_count"], 7)
        self.assertTrue(all(item["method"] == "GET" for item in carried["requests"]))
        for ref in baseline["previous_signed_envelopes"].values():
            self.assertEqual(sha(Path(ref["path"]).read_bytes()), ref["sha256"])
        self.assertEqual(ctx.bindings, original)
        # The generated descriptor is directly consumable by the next plan.
        self.assertEqual(planning._previous(result["baseline_manifest"], result["baseline_manifest_sha256"], "7.8.9")
                         ["previous"], baseline["previous"])
        with self.assertRaises(OperatorError):
            planning.accept(ctx, self.acceptance_args(public, private))

    def test_legacy_three_platform_snapshot_acceptance_stays_exact(self):
        ctx, public, private = self.acceptance_context(planning.PLATFORMS)
        self.assertEqual(len(ctx.bindings["prepared_files"]), 23)
        self.assertEqual(private["request_count"], 13)
        self.assertEqual(public["fixed_request_count"], 97)
        self.assertEqual(public["full_payload_streams"], 10)
        result = planning.accept(ctx, self.acceptance_args(public, private))
        baseline = decode(Path(result["baseline_manifest"]).read_bytes())
        self.assertEqual(len(baseline["previous_signed_envelopes"]), 3)

    def test_subsequent_arm_release_preserves_four_previous_platforms(self):
        refs = self.previous["previous_signed_envelopes"]
        refs[f"stable/releases/{self.old['version']}/windows-arm64-manifest.json"] = deepcopy(next(iter(refs.values())))
        self.previous_ref = self.record("previous.json", publisher.encode_json(self.previous))
        ctx, public, private = self.acceptance_context()
        self.assertEqual(private["request_count"], 17)
        self.assertEqual(public["fixed_request_count"], 127)
        self.assertEqual(public["preserved_previous_immutables"], list(ALL_PLATFORMS))
        bad = deepcopy(private)
        old_arm = f"stable/releases/{self.old['version']}/windows-arm64-manifest.json"
        bad["objects"][old_arm]["etag"] = '"changed-arm-etag"'
        with self.assertRaises(OperatorError):
            planning.accept(ctx, self.acceptance_args(public, bad))
        result = planning.accept(ctx, self.acceptance_args(public, private))
        baseline = decode(Path(result["baseline_manifest"]).read_bytes())
        self.assertEqual(len(baseline["previous_signed_envelopes"]), 4)

    def test_four_platform_selection_cannot_disable_arm_or_omit_its_explicit_input(self):
        ctx = self.frozen_context()
        for change in ("absent", "false", "integer"):
            selected = self.selection(ctx)
            if change == "absent":
                selected["inputs"].pop("include_windows_arm64")
            else:
                selected["inputs"]["include_windows_arm64"] = False if change == "false" else 1
            ref = self.record("selection.json", publisher.encode_json(selected))
            with self.subTest(change=change), self.assertRaises(OperatorError):
                planning.bind_publication(ctx, SimpleNamespace(selection=Path(ref["path"]),
                    selection_sha256=ref["sha256"], output=self.root / "selected.json"))
            self.assertFalse((self.root / "selected.json").exists())

    def test_legacy_publication_selection_keeps_historical_absent_arm_input(self):
        ctx = self.frozen_context(planning.PLATFORMS)
        selected = self.selection(ctx)
        selected["inputs"].pop("include_windows_arm64")
        ref = self.record("selection.json", publisher.encode_json(selected))
        result = planning.bind_publication(ctx, SimpleNamespace(selection=Path(ref["path"]),
            selection_sha256=ref["sha256"], output=self.root / "selected.json"))
        self.assertEqual(decode(Path(result["bindings"]).read_bytes())["selected_publication"], ref)

    def test_accept_rejects_partial_public_wrong_operator_and_changed_old_immutable_evidence(self):
        ctx, public, private = self.acceptance_context()
        changes = [lambda a, b: a.update(successful_full_payload_streams=9),
                   lambda a, b: a.update(producer_attempt=ctx.producer_attempt + 1),
                   lambda a, b: b.update(bindings_sha256=ctx.bindings_sha256),
                   lambda a, b: b.update(phase="before-publish"),
                   lambda a, b: b["objects"][next(iter(ctx.bindings["previous_signed_envelopes"]))].update(etag='"changed"'),
                   lambda a, b: b["objects"]["stable/windows/latest.json"].update(sha256="0" * 64),
                   lambda a, b: a["requests"][0].update(url="https://" + PublicVerifier.HOSTS[1] + "/different"),
                   lambda a, b: a["requests"][0].update(check_passed=False)]
        for change in changes:
            a, b = deepcopy(public), deepcopy(private)
            change(a, b)
            with self.subTest(change=change), self.assertRaises(OperatorError):
                planning.accept(ctx, self.acceptance_args(a, b))
            self.assertFalse((self.root / "root-acceptance.json").exists())
            self.assertFalse((self.root / "next-baseline.json").exists())

    def test_accept_carries_independently_advanced_research_into_next_unchanged_qualification(self):
        ctx, public, private = self.acceptance_context()
        old_preservation = decode(ctx.pinned(ctx.bindings["research_preservation"]))
        version = "2.0.1"
        metadata_sha = sha(publisher.encode_json({"version": version}))
        public["research"] = {"state": "independent_release_verified", "version": version,
                              "metadata_sha256": metadata_sha, "release_id": 81, "tag": "v" + version,
                              "source_sha": "f" * 40, "successful_release_run_ids": [82], "api_requests": []}
        for item in public["requests"]:
            if item.get("kind") == "research-latest":
                item.update(body_sha256=metadata_sha, research_version=version, headers={"etag": '"advanced-research"'})
            elif item.get("kind") == "research-alias":
                item["headers"]["location"] = f"https://github.com/MaplePrivacyLabs/Maple/releases/download/v{version}/{item['key']}"
        with patch.object(planning.subprocess, "run") as transport:
            result = planning.accept(ctx, self.acceptance_args(public, private))
            transport.assert_not_called()
        descriptor = decode(Path(result["baseline_manifest"]).read_bytes())
        verifier = PublicVerifier(ctx)
        verifier.baseline = decode(ctx.pinned(descriptor["previous_public"]))
        records = [deepcopy(item) for item in public["requests"] if item.get("kind", "").startswith("research-")]
        # This is the cross-release failure that forwarding the old ref caused.
        verifier.recent_research = old_preservation
        stale = verifier.qualify_research(records)
        self.assertEqual(stale["state"], "requires_review")
        self.assertIn("accepted Research baselines disagree", stale["error"])
        verifier.recent_research = decode(ctx.pinned(descriptor["research_preservation"]))
        with patch.object(verifier, "research_changed_evidence") as lookup, patch.object(verifier, "fetch") as fetch:
            qualified = verifier.qualify_research(records)
            lookup.assert_not_called()
            fetch.assert_not_called()
        self.assertEqual(qualified, {"state": "unchanged", "version": version, "metadata_sha256": metadata_sha})
        self.assertEqual(verifier.recent_research["public_receipt"], descriptor["previous_public"])


if __name__ == "__main__":
    unittest.main()
