"""Approve exactly one currently waiting publication gate from pinned evidence.

This helper never dispatches a workflow, changes environment policy, retries a
POST, or advances another gate. An exclusive durable intent precedes the final
race checks, and remains in place even when the approval result is ambiguous.
"""

from datetime import datetime, timezone
import hashlib
from pathlib import Path
import re

from .bundle import BundleVerifier


class Approval:
    @staticmethod
    def add_arguments(parser):
        parser.add_argument("--stage", choices=("reserve", "sign", "publish"), required=True)
        parser.add_argument("--approve-one", action="store_true")
        for name in ("proof", "download", "r2-proof"):
            parser.add_argument("--" + name, type=Path, required=True)
            parser.add_argument("--" + name + "-sha256", required=True)
        for name, digest_name in (("previous-proof", "previous-sha256"), ("prior-r2", "prior-r2-sha256")):
            parser.add_argument("--" + name, type=Path)
            parser.add_argument("--" + digest_name)

    STAGES = {"reserve": "prepared", "sign": "reserved", "publish": "signed"}
    PHASES = {"reserve": "before-reserve", "sign": "before-sign", "publish": "before-publish"}
    MANIFESTS = ("manifest.json", "linux-manifest.json", "windows-manifest.json")

    def __init__(self, ctx):
        self.ctx = ctx
        self.verifier = BundleVerifier(ctx)

    def need(self, condition, message):
        if not condition:
            raise ValueError(message)

    def sha(self, data):
        return hashlib.sha256(data).hexdigest()

    def bound_json(self, path, expected):
        self.need(isinstance(expected, str) and re.fullmatch(r"[0-9a-f]{64}", expected),
                  "expected SHA256")
        raw = self.ctx.pinned({"path": str(path), "sha256": expected})
        return self.ctx.core.decode_json(raw), self.sha(raw)

    def time(self, value):
        self.need(isinstance(value, str), "missing evidence timestamp")
        result = datetime.fromisoformat(value.replace("Z", "+00:00"))
        self.need(result.tzinfo is not None, "evidence timestamp must have a timezone")
        return result

    def waiting(self, stage, expected_producer_job):
        publication = self.ctx.publication
        run_id, attempt = publication["run_id"], publication["run_attempt"]
        run = self.ctx.gh_get(f"actions/runs/{run_id}")
        self.need(run.get("id") == run_id and run.get("head_sha") == publication["source_sha"]
                  and run.get("head_branch") == "master" and run.get("run_attempt") == attempt
                  and run.get("event") == "workflow_dispatch" and run.get("status") != "completed"
                  and run.get("path") == self.ctx.workflow
                  and run.get("repository", {}).get("full_name") == self.ctx.repository
                  and run.get("head_repository", {}).get("full_name") == self.ctx.repository
                  and run.get("display_title") == f"Publish Agent {self.ctx.version} from verified build {self.ctx.producer_run}",
                  "current publisher run differs")
        result = self.ctx.gh_get(f"actions/runs/{run_id}/attempts/{attempt}/jobs?per_page=100")
        jobs = result.get("jobs", [])
        self.need(type(result.get("total_count")) is int and result["total_count"] == len(jobs)
                  and {job.get("name") for job in jobs} <= {"verify", "reserve", "sign", "publish"},
                  "incomplete or unexpected job inventory")
        current = [job for job in jobs if job.get("name") == stage]
        self.need(len(current) == 1 and current[0].get("status") == "waiting"
                  and current[0].get("conclusion") is None and type(current[0].get("id")) is int
                  and current[0]["id"] > 0, "exact selected gate is not waiting")
        self.need([job.get("name") for job in jobs if job.get("status") == "waiting"] == [stage],
                  "another gate is waiting")
        producer_name = {"reserve": "verify", "sign": "reserve", "publish": "sign"}[stage]
        producer = None
        for name in {"reserve": ("verify",), "sign": ("verify", "reserve"),
                     "publish": ("verify", "reserve", "sign")}[stage]:
            prior = [job for job in jobs if job.get("name") == name]
            self.need(len(prior) == 1 and prior[0].get("status") == "completed"
                      and prior[0].get("conclusion") == "success", "prior actual job did not succeed")
            if name == producer_name:
                self.need(type(expected_producer_job) is int and prior[0].get("id") == expected_producer_job
                          and prior[0].get("run_id") == run_id,
                          "artifact-producing job differs from reviewed download")
                producer = prior[0]
        pending = self.ctx.gh_get(f"actions/runs/{run_id}/pending_deployments")
        self.need(isinstance(pending, list) and len(pending) == 1
                  and pending[0].get("environment", {}).get("id") == self.ctx.environment_id
                  and pending[0].get("environment", {}).get("name") == self.ctx.envname
                  and pending[0].get("current_user_can_approve") is True
                  and pending[0].get("wait_timer", 0) == 0, "exact environment approval is unavailable")
        return current[0]["id"], producer

    def policy(self):
        environment = self.ctx.gh_get("environments/" + self.ctx.envname)
        self.need(environment.get("id") == self.ctx.environment_id
                  and environment.get("name") == self.ctx.envname
                  and environment.get("can_admins_bypass") is False
                  and environment.get("deployment_branch_policy") == {
                      "protected_branches": False, "custom_branch_policies": True},
                  "environment identity/admin/branch policy differs")
        rules = environment.get("protection_rules", [])
        self.need(all(rule.get("type") in {"required_reviewers", "branch_policy"} for rule in rules)
                  and sum(rule.get("type") == "branch_policy" for rule in rules) <= 1,
                  "unexpected protection rule")
        required = [rule for rule in rules if rule.get("type") == "required_reviewers"]
        self.need(len(required) == 1 and required[0].get("prevent_self_review") is False,
                  "self-review policy differs")
        reviewers = required[0].get("reviewers", [])
        self.need(len(reviewers) == 1 and reviewers[0].get("type") == "User"
                  and reviewers[0].get("reviewer", {}).get("id") == self.ctx.reviewer_id,
                  "required reviewer differs")
        branches = self.ctx.gh_get("environments/" + self.ctx.envname + "/deployment-branch-policies")
        self.need(type(branches.get("total_count")) is int and branches["total_count"] == 1
                  and len(branches.get("branch_policies", [])) == 1
                  and [(row.get("name"), row.get("type")) for row in branches["branch_policies"]]
                  == [("master", "branch")], "master-only branch policy differs")
        return {"id": self.ctx.environment_id, "prevent_self_review": False,
                "reviewer": self.ctx.reviewer_id, "can_admins_bypass": False,
                "branches": [{"name": "master", "type": "branch"}]}

    def r2_proof(self, args, bindings, publications, metadata):
        proof, proof_sha = self.bound_json(args.r2_proof, args.r2_proof_sha256)
        self.need(proof.get("schema") == "agent-release-r2-proof-v1"
                  and proof.get("bindings_sha256") == self.ctx.bindings_sha256
                  and proof.get("tooling_sha256") == self.ctx.tooling_sha256
                  and proof.get("policy_sha256") == self.ctx.policy_sha256
                  and proof.get("phase") == self.PHASES[args.stage] and proof.get("passed") is True
                  and type(proof.get("request_count")) is int and proof["request_count"] == 13
                  and proof.get("route_sha256") == self.ctx.policy["r2"]["route"]["sha256"],
                  "wrong current private R2 proof")
        start, end = [self.time(proof.get(name)) for name in ("started_at", "completed_at")]
        now = datetime.now(timezone.utc)
        self.need(0 <= (end - start).total_seconds() <= 181
                  and 0 <= (now - end).total_seconds() <= 300,
                  "private R2 evidence is stale or has an invalid time bound")
        prior = None
        if args.stage == "reserve":
            self.need(args.prior_r2 is None and args.prior_r2_sha256 is None
                      and proof.get("previous_proof_sha256") is None, "reserve R2 proof must start the chain")
        else:
            self.need(args.prior_r2 is not None and args.prior_r2_sha256 is not None,
                      "missing predecessor R2 proof")
            prior, prior_sha = self.bound_json(args.prior_r2, args.prior_r2_sha256)
            previous_phase = "before-reserve" if args.stage == "sign" else "before-sign"
            self.need(prior.get("schema") == proof["schema"]
                      and prior.get("bindings_sha256") == self.ctx.bindings_sha256
                      and prior.get("tooling_sha256") == self.ctx.tooling_sha256
                      and prior.get("policy_sha256") == self.ctx.policy_sha256
                      and prior.get("passed") is True and prior.get("phase") == previous_phase
                      and proof.get("previous_proof_sha256") == prior_sha, "private R2 predecessor differs")
        baseline = self.ctx.core.decode_json(self.ctx.pinned(bindings["previous_private_r2"]))["objects"]
        old_prefix = f"stable/releases/{bindings['previous']['version']}/"
        expected = {key: baseline[key] for key in (
            "stable/latest.json", "stable/linux/latest.json", "stable/windows/latest.json",
            *(old_prefix + name for name in self.MANIFESTS))}
        prefix = f"stable/releases/{self.ctx.version}/"
        for name in ("publication-manifest.json", "publication-linux-manifest.json",
                     "publication-windows-manifest.json", "publication-metadata.json", *self.MANIFESTS):
            expected[prefix + name] = None
        if args.stage != "reserve":
            for key, data in [(prefix + item["reservation"], item["data"]) for item in publications] + [
                    (prefix + "publication-metadata.json", metadata)]:
                expected[key] = {"sha256": self.sha(data), "size": len(data)}
                if args.stage == "publish":
                    expected[key]["etag"] = prior["objects"][key]["etag"]
        objects = proof.get("objects", {})
        self.need(set(objects) == set(expected) and len(objects) == 13, "private R2 object inventory differs")
        for key, record in expected.items():
            actual = objects[key]
            self.need(actual is None if record is None else isinstance(actual, dict)
                      and all(actual.get(field) == value for field, value in record.items())
                      and isinstance(actual.get("etag"), str) and actual["etag"]
                      and all(ord(char) >= 32 and ord(char) != 127 for char in actual["etag"]),
                      "private R2 state differs from the exact current bundle/predecessor")
        return proof_sha

    def local_proofs(self, args, bindings):
        stage = self.STAGES[args.stage]
        publication = self.ctx.publication
        run, attempt, publisher_source = publication["run_id"], publication["run_attempt"], publication["source_sha"]
        proof, proof_sha = self.bound_json(args.proof, args.proof_sha256)
        download, download_sha = self.bound_json(args.download, args.download_sha256)
        self.need(download.get("schema") == "agent-release-stage-download-v1", "download receipt schema differs")
        self.need(download.get("stage") == stage and download.get("bindings_sha256") == self.ctx.bindings_sha256
                  and download.get("publication_run") == run and download.get("attempt") == attempt
                  and download.get("publisher_source") == publisher_source and download.get("producer") == self.ctx.producer_run
                  and download.get("producer_attempt") == self.ctx.producer_attempt and download.get("source") == self.ctx.source
                  and download.get("build") == self.ctx.build and download.get("version") == self.ctx.version,
                  "download identity differs")
        self.need(download.get("tooling_sha256") == self.ctx.tooling_sha256
                  and download.get("freeze_sha256") == bindings["freeze"]["sha256"]
                  and download.get("selected_publication_sha256") == bindings["selected_publication"]["sha256"]
                  and download.get("recovered_envelopes") == [],
                  "download code/freeze/selection pins differ or unexpected recovery")
        bundle = Path(download["destination"])
        self.need(bundle.is_absolute() and bundle.resolve(strict=True) == bundle
                  and bundle.is_relative_to(self.ctx.root), "bundle is outside private owned state")
        previous = None
        if stage == "prepared":
            self.need(args.previous_proof is None and args.previous_sha256 is None,
                      "prepared proof must start the chain")
        else:
            self.need(args.previous_proof is not None and args.previous_sha256 is not None,
                      "missing predecessor bundle proof")
            previous = {"path": str(args.previous_proof), "sha256": args.previous_sha256}
        publications, metadata, actual = self.verifier.verify_bundle(bindings, self.ctx.core, bundle, stage, previous)
        self.need(tuple(item["name"] for item in publications) == self.MANIFESTS,
                  "all three exact publication documents required")
        self.need(proof.get("schema") == "agent-release-bundle-proof-v1"
                  and proof.get("tooling_sha256") == self.ctx.tooling_sha256
                  and actual == proof and actual["files"] == download.get("files")
                  and len(actual["files"]) == {"prepared": 23, "reserved": 24, "signed": 27}[stage],
                  "current exact inventory, downloaded map or authenticated proof differs")
        signatures = sorted(item["envelope_name"] for item in publications) if stage == "signed" else []
        self.need(actual.get("verified_primary_signatures") == signatures
                  and len(signatures) == (3 if stage == "signed" else 0),
                  "all three primary signatures required before publish")
        artifact = download.get("artifact", {})
        self.need(type(artifact.get("id")) is int and artifact["id"] > 0
                  and artifact.get("name") == f"agent-update-{stage}-{run}"
                  and type(artifact.get("size_in_bytes")) is int and 0 < artifact["size_in_bytes"] <= 512 * 1024**2
                  and artifact.get("digest") == "sha256:" + download.get("actual_zip_sha256", "")
                  and re.fullmatch(r"[0-9a-f]{64}", download.get("actual_zip_sha256", ""))
                  and download.get("actual_zip_size") == artifact["size_in_bytes"], "stage artifact ZIP pin differs")
        self.need(type(download.get("producer_job_id")) is int and download["producer_job_id"] > 0,
                  "stage artifact producer job is missing")
        r2_sha = self.r2_proof(args, bindings, publications, metadata)
        return download, {"proof": proof_sha, "download": download_sha, "r2": r2_sha,
                          "bindings": self.ctx.bindings_sha256, "tooling": self.ctx.tooling_sha256,
                          "policy": self.ctx.policy_sha256}, publications, metadata

    def artifact(self, download, producer):
        run = self.ctx.publication["run_id"]
        result = self.ctx.gh_get(f"actions/runs/{run}/artifacts?per_page=100")
        rows = result.get("artifacts", [])
        self.need(type(result.get("total_count")) is int and result["total_count"] == len(rows),
                  "incomplete fresh artifact inventory")
        matches = [row for row in rows if row.get("id") == download["artifact"]["id"]]
        self.need(len(matches) == 1 and sum(row.get("name") == download["artifact"]["name"] for row in rows) == 1,
                  "artifact identity/name is not unique")
        artifact = matches[0]
        self.need(all(artifact.get(key) == download["artifact"].get(key)
                      for key in ("id", "name", "digest", "size_in_bytes"))
                  and artifact.get("expired") is False and artifact.get("workflow_run", {}).get("id") == run
                  and artifact.get("workflow_run", {}).get("head_sha") == self.ctx.publication["source_sha"],
                  "fresh artifact metadata differs")
        created, started, completed = [self.time(value) for value in (
            artifact.get("created_at"), producer.get("started_at"), producer.get("completed_at"))]
        self.need(started <= created <= completed, "artifact is not fresh from the selected attempt's producer job")

    def run(self, args):
        self.need(args.stage in self.STAGES and args.approve_one is True,
                  "one explicit actual publication approval required")
        self.need(self.ctx.root.resolve(strict=True) == self.ctx.root
                  and self.ctx.root.stat().st_mode & 0o7777 == 0o700, "owned root must be private0700")
        self.need(self.ctx.envname == "updates-production" and type(self.ctx.environment_id) is int
                  and self.ctx.environment_id > 0 and type(self.ctx.reviewer_id) is int and self.ctx.reviewer_id > 0,
                  "canonical approval environment/reviewer differs")
        bindings, core = self.verifier.setup()
        self.need(bindings == self.ctx.bindings and core is self.ctx.core, "reviewed release context differs")
        run, attempt = self.ctx.publication["run_id"], self.ctx.publication["run_attempt"]
        prefix = f"{run}-attempt{attempt}-{args.stage}-approval"
        intent_path = self.ctx.root / (prefix + "-intent.json")
        self.need(not intent_path.exists() and not intent_path.is_symlink(),
                  "an approval intent already exists; no automatic retry")
        download, hashes, publications, metadata = self.local_proofs(args, bindings)
        producer_job = download["producer_job_id"]
        job, producer = self.waiting(args.stage, producer_job)
        self.artifact(download, producer)
        guarded_policy = self.policy()
        self.need(self.waiting(args.stage, producer_job)[0] == job, "current waiting gate changed during review")
        self.r2_proof(args, bindings, publications, metadata)
        body = {"environment_ids": [self.ctx.environment_id], "state": "approved", "comment": (
            f"{args.stage} only: reviewed exact Mac+Linux+Windows Agent{self.ctx.version}/build{self.ctx.build} "
            f"from producer{self.ctx.producer_run}/source{self.ctx.source[:7]}; exact stage ZIP, freeze, proof chain, "
            "three primary signatures where applicable and fresh private R2 state verified for this current gate.")}
        intent = {"at": datetime.now(timezone.utc).isoformat(), "stage": args.stage,
                  "publication_run": run, "attempt": attempt, "job_id": job, "producer_job_id": producer_job,
                  "publisher_source": self.ctx.publication["source_sha"], "producer": self.ctx.producer_run,
                  "source": self.ctx.source, "producer_attempt": self.ctx.producer_attempt,
                  "version": self.ctx.version, "build": self.ctx.build, "hashes": hashes,
                  "artifact": download["artifact"], "policy": guarded_policy, "body": body,
                  "scope": "exactly this current waiting gate; no future approval, retry, dispatch or policy mutation"}
        self.ctx.emit(intent_path, intent)
        outcome = {"at": datetime.now(timezone.utc).isoformat(), "stage": args.stage,
                   "publication_run": run, "attempt": attempt, "job_id": job, "approval_confirmed": False}
        try:
            self.need(self.waiting(args.stage, producer_job)[0] == job, "gate changed after intent; no POST made")
            self.r2_proof(args, bindings, publications, metadata)
            returned = self.ctx.gh_post(f"actions/runs/{run}/pending_deployments", body)
            self.need(isinstance(returned, list) and len(returned) == 1
                      and returned[0].get("environment") == self.ctx.envname
                      and returned[0].get("sha") == self.ctx.publication["source_sha"],
                      "approval response differs; inspect before any retry")
            outcome["approval_confirmed"] = True
            outcome["deployment"] = {key: returned[0].get(key) for key in ("id", "environment", "ref", "sha")}
        except Exception as error:
            outcome["error_type"] = type(error).__name__
            self.ctx.emit(self.ctx.root / (prefix + "-result.json"), outcome)
            raise
        self.ctx.emit(self.ctx.root / (prefix + "-result.json"), outcome)
        return outcome
