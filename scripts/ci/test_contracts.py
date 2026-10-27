"""Contracts between CI and the release, publication and signing machinery.

Each test imports the code on the other side of its contract (the publishers,
the release preflight) instead of copying it. A failure means a release, update
feed or signed build would break: fix the workflow, not the contract.
"""

from __future__ import annotations

import importlib.util
import itertools
import json
from pathlib import Path
import re
import unittest
from unittest import mock

import plan
from test_workflow_policy import (ENVIRONMENTS, GITHUB_HOSTED, LISTENERS, ROOT, cache_writer, environment, jobs,
                                  load, permissions, runner_labels, triggers, workflows)


def module(name: str, path: str):
    spec = importlib.util.spec_from_file_location(name, ROOT / path)
    loaded = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(loaded)
    return loaded


publisher = module("agent_publisher", "services/agent-updates/publisher/publisher.py")
ci_evidence = module("ci_evidence", ".agents/skills/release-maple/scripts/ci_evidence.py")

# Master workflows whose caches release.yml restores by prefix.
RELEASE_FEEDING = frozenset({"release.yml", "desktop-build.yml", "android-build.yml", "mobile-build.yml",
                             "ios-dev-testflight.yml"})
RESERVED_PREFIXES = (
    "Linux-sccache-", "macOS-sccache-", "Windows-sccache-", "${{ runner.os }}-sccache-", "xcode-cache-",
    "maple-dev-xcode", "release-onnxruntime-ios-", "${{ runner.os }}-bun-", "${{ runner.os }}-gradle-",
)
ONNX_CACHE = "apps/maple-research/frontend/src-tauri/onnxruntime-ios"
# The unsigned Agent builders that write the exact-key release cache; ci.yml's packaging lanes read it.
AGENT_RELEASE_WRITERS = frozenset(("agent-desktop-build.yml", job) for job in ("macos-build", "windows-build", "linux"))
AGENT_RELEASE_READERS = frozenset(("ci.yml", f"agent-package-{os}") for os in ("linux", "macos", "windows"))
# Jobs that may mint an OIDC token: attestation, signing identity, FlakeHub, registries.
OIDC_JOBS = frozenset({
    ("agent-desktop-build.yml", "macos"), ("agent-desktop-build.yml", "windows"),
    ("agent-desktop-build.yml", "attest-linux"), ("android-build.yml", "build-android"),
    ("desktop-build.yml", "build-macos"), ("desktop-build.yml", "build-linux"), ("desktop-build.yml", "build-windows"),
    ("dev-credential-requester-image.yml", "build"), ("ios-dev-testflight.yml", "build-ios-dev"),
    ("mobile-build.yml", "build-ios"), ("opensecret-eif-release.yml", "build"),
    ("opensecret-eif.yml", "eif-candidate"), ("opensecret-eif.yml", "eif-trusted"),
    *(("release.yml", job) for job in ("build-tauri", "build-windows", "build-android", "build-ios", "build-web",
                                       "publish-proxy-release-artifacts", "update-latest-json",
                                       "publish-release-verification-guide")),
    ("sdk-publish-npm.yml", "publish"), ("sdk-publish-rust.yml", "publish"), ("web-build.yml", "build-web-master"),
})
# The EIF builders that warm FlakeHub for opensecret-eif-release.yml.
FLAKEHUB_JOBS = frozenset({("opensecret-eif.yml", "eif-candidate"), ("opensecret-eif.yml", "eif-trusted"),
                           ("opensecret-eif-release.yml", "build")})
# Every job that holds a protected environment.
CONSUMERS = {
    ("agent-desktop-build.yml", "macos"): "desktop-signing",
    ("agent-desktop-build.yml", "windows"): "windows-signing",
    **{("agent-updates-publish.yml", job): "updates-production" for job in ("reserve", "sign", "publish")},
    ("android-build.yml", "build-android"): "android-signing",
    ("auth-pages-dev-publish.yml", "development"): "auth-pages-development",
    ("auth-pages-dev-publish.yml", "preview"): "auth-pages-development",
    ("auth-pages-publish.yml", "production"): "auth-pages-production",
    ("desktop-build.yml", "build-macos"): "desktop-signing",
    ("desktop-build.yml", "build-linux"): "desktop-signing",
    ("desktop-build.yml", "build-windows"): "windows-signing",
    ("ios-dev-testflight.yml", "build-ios-dev"): "apple-signing",
    ("ios-dev-testflight.yml", "submit-ios-dev-testflight"): "apple-signing",
    ("mobile-build.yml", "build-ios"): "apple-signing",
    ("mobile-build.yml", "submit-ios-testflight"): "apple-signing",
    ("opensecret-eif-release.yml", "sign"): "pcr-signing",
    ("pages-publish.yml", "preview"): "pages-preview",
    ("pages-publish.yml", "production"): "pages-production",
    ("release.yml", "build-tauri"): "desktop-signing",
    ("release.yml", "build-windows"): "windows-signing",
    ("release.yml", "build-android"): "android-signing",
    ("release.yml", "build-ios"): "apple-signing",
    ("sdk-publish-npm.yml", "publish"): "sdk-npm",
    ("sdk-publish-rust.yml", "publish"): "sdk-crates",
    **{("signing-credentials-check.yml", name): name for name in (
        "desktop-signing", "apple-signing", "android-signing", "windows-signing", "zapstore-publishing")},
    ("updates-publish.yml", "publish"): "updates-production",
    ("zapstore-publish.yml", "publish"): "zapstore-publishing",
}
DOCS = ("docs/**", "*/docs/**", "*/*/docs/**", ".agents/**", "**/README.md", "**/AGENTS.md", "**/CLAUDE.md")

TEMPORARY = {
    # Signed-lane hygiene: CodeQL runs on every master push, never cancelled.
    "evidence": frozenset({"codeql.yml"}),
    # The client lanes delete the PR builds and drop the evidence twins' caches.
    "reserved": frozenset({
        ("android-pr-build.yml", "build-android"), ("desktop-pr-build.yml", "build-macos"),
        ("desktop-pr-build.yml", "build-research-dev-macos"), ("desktop-pr-build.yml", "build-linux"),
        ("mobile-pr-build.yml", "build-ios"), ("frontend-tests.yml", "test-frontend"),
        ("rust-tests.yml", "rust-tests"), ("web-build.yml", "build-web"), ("web-build.yml", "build-web-master"),
    }),
    # Signed-lane hygiene moves id-token from the workflow level to the attesting jobs.
    "oidc": frozenset({("android-build.yml", "changes"), ("desktop-build.yml", "changes"),
                       ("desktop-build.yml", "verify-desktop-artifacts")}),
}


def condition(job: dict) -> str:
    return " ".join(str(job.get("if", "")).split())


def variants(job: dict) -> list:
    matrix = job.get("strategy", {}).get("matrix", {}).get("variant")
    if matrix is None or isinstance(matrix, list):
        return matrix or [None]
    return sorted({value for array in re.findall(r"\[[^\]]*\]", matrix) for value in json.loads(array)})


def cache_steps():
    for name, job_id, job in jobs():
        for step in job.get("steps", []):
            if step.get("uses", "").startswith(("actions/cache", "irgaly/xcode-cache@")):
                yield name, job_id, step


def cancels(concurrency) -> bool:
    if not isinstance(concurrency, dict):
        return False
    value = concurrency.get("cancel-in-progress", False)
    return value is True or (isinstance(value, str) and value != "${{ github.event_name == 'pull_request' }}")


class ContractTests(unittest.TestCase):
    def test_c1_release_is_unique_and_listeners_prove_their_producer(self):
        configs = workflows()
        names = [config.get("name") for config in configs.values()]
        self.assertEqual([name for name, config in configs.items() if config.get("name") == "Release"],
                         ["release.yml"])
        self.assertEqual([name for name, config in configs.items() if "release" in triggers(config)],
                         ["release.yml"])
        for name in sorted(LISTENERS):
            watched = configs[name]["on"]["workflow_run"]["workflows"]
            self.assertTrue(all(names.count(producer) == 1 for producer in watched), name)
            for job_id, job in configs[name]["jobs"].items():
                gate = condition(job)
                with self.subTest(workflow=name, job=job_id):
                    if job.get("needs"):
                        continue
                    if "workflow_run" not in gate:
                        self.assertIn("github.event_name == 'workflow_dispatch'", gate)
                        continue
                    for control in ("github.event.workflow_run.conclusion == 'success'",
                                    "github.event.workflow_run.head_repository.full_name == github.repository",
                                    "github.event.workflow_run.event =="):
                        self.assertIn(control, gate)
                    paths = re.findall(r"workflow_run\.path == '\.github/workflows/([^']+)'", gate)
                    self.assertTrue(all(configs[path]["name"] in watched for path in paths))
                    if not paths:  # only release.yml runs on `release`
                        self.assertIn("github.event.workflow_run.event == 'release'", gate)

    def test_c1_research_releases_gate_every_release_job_and_publisher(self):
        release = workflows()["release.yml"]
        self.assertEqual(release["jobs"]["classify-app-release"]["if"],
                         "startsWith(github.event.release.tag_name, 'v')")
        for job_id, job in release["jobs"].items():
            if job_id != "classify-app-release":
                needs = job["needs"] if isinstance(job["needs"], list) else [job["needs"]]
                self.assertIn("classify-app-release", needs, job_id)
                self.assertNotIn("if", job, job_id)
        for name, job_id in (("pages-publish.yml", "production"), ("updates-publish.yml", "publish"),
                             ("proxy-publish.yml", "prepare"), ("zapstore-publish.yml", "publish")):
            gate = condition(workflows()[name]["jobs"][job_id])
            for control in ("startsWith(github.event.workflow_run.head_branch, 'v') &&",
                            "github.event.workflow_run.conclusion == 'success'",
                            "github.event.workflow_run.event == 'release'"):
                self.assertIn(control, gate, name)

    def test_c2_agent_publisher_accepts_this_workflows_jobs_and_artifacts(self):
        config = load(ROOT / publisher.WORKFLOW)
        self.assertIn("master", config["on"]["push"]["branches"])
        self.assertIn("workflow_dispatch", config["on"])
        run_id = 4242

        def expand(text, variant):
            return text.replace("${{ matrix.variant }}", str(variant)).replace("${{ github.run_id }}", str(run_id))

        inventory, artifacts = [], []
        for job_id, job in config["jobs"].items():
            for variant in variants(job):
                inventory.append({"name": expand(job.get("name", job_id), variant),
                                  "status": "completed", "conclusion": "success"})
                artifacts += [{"name": expand(step["with"]["name"], variant), "expired": False}
                              for step in job.get("steps", [])
                              if step.get("uses", "").startswith("actions/upload-artifact@")]
            attests = any(step.get("uses", "").startswith("actions/attest@") for step in job.get("steps", []))
            if attests:  # the publisher verifies with --deny-self-hosted-runners
                self.assertTrue(GITHUB_HOSTED.fullmatch(job["runs-on"]), job_id)
            if attests or environment(job) or job.get("name", "").startswith("Verify"):
                self.assertFalse([step for step in job.get("steps", []) if step.get("uses", "").startswith(
                    ("actions/cache", "Swatinem/rust-cache@"))], f"{job_id} must restore no cache")
        run = {"id": run_id, "run_attempt": 1, "run_number": 77, "path": publisher.WORKFLOW,
               "head_branch": "master", "event": "push", "status": "completed", "head_sha": "a" * 40,
               "created_at": "2026-01-01T00:00:00Z", "repository": {"full_name": publisher.REPOSITORY},
               "head_repository": {"full_name": publisher.REPOSITORY}}
        responses = {"": run, "jobs": {"total_count": len(inventory), "jobs": inventory},
                     "artifacts": {"total_count": len(artifacts), "artifacts": artifacts}}
        with mock.patch.object(publisher, "gh_json",
                               lambda path: responses[re.sub(r".*/runs/\d+/?([a-z]*).*", r"\1", path)]):
            proof = publisher.trust_run(run_id, linux=True)
        self.assertEqual(proof["build"], "77")  # run_number is the Agent build number

    def test_c3_release_preflight_finds_its_evidence_jobs_on_master_pushes(self):
        broken = set()
        for name, required in ci_evidence.WORKFLOWS.items():
            config = workflows().get(name)
            self.assertIsNotNone(config, name)
            push = config["on"].get("push") if isinstance(config["on"], dict) else None
            names = {job.get("name", job_id) for job_id, job in config["jobs"].items()}
            found = set(required) <= names if required else any(n.startswith("Analyze (") for n in names)
            self.assertTrue(found, name)
            if (not isinstance(push, dict) or "master" not in push.get("branches", ["master"])
                    or cancels(config.get("concurrency"))
                    or any(cancels(job.get("concurrency")) for job in config["jobs"].values())):
                broken.add(name)
        self.assertEqual(broken, TEMPORARY["evidence"])

    def test_c4_signed_lanes_own_the_release_cache_keys(self):
        steps = list(cache_steps())
        writers = {(name, job_id) for name, job_id, step in steps if cache_writer(step)
                   and name not in RELEASE_FEEDING and step["with"].get("key", "").startswith(RESERVED_PREFIXES)}
        self.assertEqual(writers, TEMPORARY["reserved"])
        feeders = [step["with"]["key"] for name, _, step in steps
                   if name in RELEASE_FEEDING - {"release.yml"} and cache_writer(step)]
        for job_id, job in workflows()["release.yml"]["jobs"].items():
            for step in job.get("steps", []):
                if step.get("uses", "").startswith(("actions/cache", "irgaly/xcode-cache@")):
                    prefixes = [step["with"]["key"]] + [
                        line.strip() for line in step["with"].get("restore-keys", "").splitlines() if line.strip()]
                    with self.subTest(job=job_id, key=step["with"]["key"]):
                        self.assertTrue(any(key.startswith(tuple(prefixes)) for key in feeders))

    def test_c4_onnx_cache_text_matches_release_and_only_signed_lanes_write_it(self):
        onnx = [(name, step) for name, _, step in cache_steps()
                if step["with"].get("path", "").strip() == ONNX_CACHE]
        self.assertLessEqual({"release.yml", "mobile-build.yml", "ios-dev-testflight.yml"}, {n for n, _ in onnx})
        release = next(step["with"] for name, step in onnx if name == "release.yml")
        for name, step in onnx:
            with self.subTest(workflow=name):
                self.assertEqual((step["with"]["key"], step["with"]["restore-keys"]),
                                 (release["key"], release["restore-keys"]))
                if cache_writer(step):
                    self.assertIn(name, RELEASE_FEEDING)

    def test_c4_agent_release_cache_is_exact_and_written_only_by_master_builders(self):
        restores = [(name, job_id, step["with"]) for name, job_id, step in cache_steps()
                    if step["with"].get("key", "").startswith("ci-v1-agent-release-")]
        self.assertEqual({(name, job_id) for name, job_id, _ in restores},
                         AGENT_RELEASE_WRITERS | AGENT_RELEASE_READERS)
        self.assertEqual(len({(cache["key"], cache["path"]) for _, _, cache in restores}), 1)
        self.assertFalse([cache for _, _, cache in restores if "restore-keys" in cache])  # exact keys only
        writers = [(name, job_id, step) for name, job_id, step in cache_steps() if cache_writer(step)
                   and (name, job_id) in AGENT_RELEASE_WRITERS | AGENT_RELEASE_READERS]
        self.assertEqual({(name, job_id) for name, job_id, _ in writers}, AGENT_RELEASE_WRITERS)
        for _, job_id, step in writers:
            with self.subTest(job=job_id):
                self.assertTrue(step["uses"].startswith("actions/cache/save@"))
                self.assertEqual(step["with"]["path"], restores[0][2]["path"])
                self.assertIn("github.event_name == 'push' && github.ref == 'refs/heads/master'", step["if"])

    def test_c4_dev_testflight_build_number_is_the_run_number(self):
        job = workflows()["ios-dev-testflight.yml"]["jobs"]["build-ios-dev"]
        identity = next(step for step in job["steps"] if step.get("id") == "identity")
        self.assertEqual(identity["env"]["RUN_NUMBER"], "${{ github.run_number }}")

    def test_c5_eif_event_gate_matches_the_approval_policy(self):
        # Evaluate the real GitHub expressions with equivalent Python semantics.
        expressions = {}
        for job_id in ("eif-candidate", "eif", "eif-trusted"):
            text = workflows()["opensecret-eif.yml"]["jobs"][job_id]["if"].strip()
            text = text.removeprefix("${{").removesuffix("}}").replace("&&", " and ").replace("||", " or ")
            expressions[job_id] = " ".join(text.replace("!cancelled()", "NOT_CANCELLED")
                                           .replace("always()", "True").split())
        cases = (
            ("pull_request", "refs/pull/1/merge", "success", "true", "false", False),
            ("pull_request", "refs/pull/1/merge", "success", "true", "true", True),
            ("pull_request", "refs/pull/1/merge", "success", "false", "true", True),
            ("pull_request", "refs/pull/1/merge", "failure", "", "", False),
            ("pull_request", "refs/pull/1/merge", "failure", "true", "true", False),
            ("pull_request", "refs/pull/1/merge", "skipped", "", "", False),
            ("pull_request", "refs/pull/1/merge", "cancelled", "", "", False),
            ("pull_request", "refs/pull/1/merge", "success", "", "", False),
            ("pull_request", "refs/heads/master", "success", "true", "true", True),
            ("push", "refs/heads/master", "success", "true", "false", True),
            ("push", "refs/heads/master", "success", "false", "false", False),
            ("push", "refs/heads/master", "failure", "", "", True),
            ("push", "refs/heads/master", "skipped", "", "", True),
            ("push", "refs/heads/master", "success", "", "", True),
            ("push", "refs/heads/feature", "success", "true", "true", False),
            ("push", "refs/tags/master", "success", "true", "true", False),
            ("workflow_dispatch", "refs/heads/master", "success", "true", "false", True),
            ("workflow_dispatch", "refs/heads/master", "failure", "", "", True),
            ("workflow_dispatch", "refs/heads/feature", "success", "true", "false", True),
            ("workflow_dispatch", "refs/tags/review", "success", "true", "false", True),
            ("workflow_dispatch", "refs/tags/master", "failure", "", "", True),
            ("schedule", "refs/heads/master", "success", "true", "true", False),
            ("pull_request_target", "refs/heads/master", "success", "true", "true", False),
            ("workflow_run", "refs/heads/master", "success", "true", "true", False),
        )
        heads = (("MaplePrivacyLabs/Maple", True), ("contributor/Maple", False),
                 ("MaplePrivacyLabs/Maple-fork", False), ("", False), (None, False))
        for case, (head, same_repo), cancelled in itertools.product(cases, heads, (False, True)):
            event, ref, result, eif, approvals, expected = case
            context = {
                "github.event_name": event, "github.ref": ref,
                "github.event.pull_request.head.repo.full_name": head,
                "github.repository": "MaplePrivacyLabs/Maple", "needs.changes.result": result,
                "needs.changes.outputs.eif": eif, "needs.changes.outputs.pcr_approvals": approvals,
                "NOT_CANCELLED": not cancelled,
            }
            selected = []
            for job_id, expression in expressions.items():
                for key, value in context.items():
                    expression = expression.replace(key, repr(value))
                if eval(expression, {"__builtins__": {}}, {}):  # noqa: S307 - fixed workflow text
                    selected.append(job_id)
            trusted = ((event == "pull_request" and same_repo)
                       or (event in ("push", "workflow_dispatch") and ref == "refs/heads/master"))
            wanted = []
            if (not cancelled and event == "pull_request" and same_repo and result == "success"
                    and eif == "true" and approvals == "false"):
                wanted.append("eif-candidate")
            if expected and not cancelled:
                wanted.append("eif-trusted" if trusted else "eif")
            self.assertEqual(selected, wanted, (case, head, cancelled))

    def test_c5_oidc_and_flakehub_stay_in_their_jobs_on_one_builder_class(self):
        minted = {(name, job_id) for name, job_id, job in jobs()
                  if permissions(workflows()[name], job).get("id-token") == "write"}
        self.assertEqual(minted - OIDC_JOBS, TEMPORARY["oidc"])
        builder = workflows()["opensecret-eif-release.yml"]["jobs"]["build"]["runs-on"]
        flakehub = {(name, job_id) for name, job_id, job in jobs() for step in job.get("steps", [])
                    if "flakehub-cache-action@" in step.get("uses", "")}
        self.assertEqual(flakehub, FLAKEHUB_JOBS)
        for name, job_id in flakehub:
            self.assertEqual(workflows()[name]["jobs"][job_id]["runs-on"], builder, job_id)

    def test_c5_eif_release_is_manual_reviewed_and_never_tags(self):
        config = workflows()["opensecret-eif-release.yml"]
        self.assertEqual(set(config["on"]), {"workflow_dispatch"})
        self.assertIs(config["on"]["workflow_dispatch"]["inputs"]["dry_run"]["default"], True)
        self.assertNotRegex(json.dumps(config), r"gh release|git tag|refs/tags|releases/|latest\.json")
        build, sign = config["jobs"]["build"], config["jobs"]["sign"]
        self.assertEqual(build["if"], "github.ref == 'refs/heads/master'")
        self.assertEqual(build["permissions"], {"contents": "read", "id-token": "write", "attestations": "write"})
        self.assertNotIn("environment", build)
        self.assertFalse(re.search(r"\bsecrets\b|github\.token|\bGH_TOKEN\b", json.dumps(build)))
        self.assertEqual(sign["needs"], "build")
        self.assertEqual(sign["if"], "inputs.dry_run == false && needs.build.outputs.approval_needed == 'true'")
        self.assertEqual(sign["permissions"], {"contents": "write"})
        self.assertNotRegex(json.dumps(sign), r"id-token|flakehub-cache")
        key = [step for step in sign["steps"] if "outputs.SIGNING_PRIVATE_KEY" in json.dumps(step)]
        self.assertEqual(len(key), 1)  # the signing key reaches exactly one step
        self.assertEqual(key[0]["env"], {"SIGNING_PRIVATE_KEY": "${{ steps.signing-key.outputs.SIGNING_PRIVATE_KEY }}"})

    def test_c6_protected_environments_have_fixed_consumers(self):
        self.assertEqual({(name, job_id): environment(job) for name, job_id, job in jobs() if environment(job)},
                         CONSUMERS)
        self.assertLessEqual(set(CONSUMERS.values()), set(ENVIRONMENTS))
        for (name, job_id), granted in CONSUMERS.items():
            if granted == "windows-signing":  # the Azure OIDC subject is a GitHub-hosted runner
                labels = runner_labels(name, job_id, workflows()[name]["jobs"][job_id])
                self.assertTrue(labels and all(GITHUB_HOSTED.fullmatch(label) for label in labels), job_id)

    def test_c7_docs_name_only_existing_workflows_and_files(self):
        files = plan.tracked_files()
        names = {Path(path).name for path in files}
        dangling = []
        for path in files:
            if not (path.endswith(".md") and plan.matches(path, DOCS)):
                continue
            text = (ROOT / path).read_text(encoding="utf-8", errors="replace")
            for match in re.finditer(r"(?<![\w./-])((?:\.github/workflows/)?[\w.-]+\.ya?ml)\b", text):
                reference = match.group(1)
                exists = ((ROOT / reference).is_file() if reference.startswith(".github/")
                          else reference in names)
                if not exists:
                    dangling.append(f"{path}: {reference}")
        self.assertEqual(dangling, [])


if __name__ == "__main__":
    unittest.main()
